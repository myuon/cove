//! `files`: the real filesystem, confined to a directory the host chose.
//!
//! The Language Card names files first among the operations that are typed
//! Host APIs rather than ambient authority. [`crate::host::Documents`] is a
//! narrower thing: a read-only view over a fixed set of `.txt` documents, so
//! a program that only reads its inputs never has to be handed a filesystem.
//! This module is the filesystem itself — reading, writing, listing, and
//! removing — and it is a separate capability precisely because it is the
//! wider one.
//!
//! Granting `files` must not hand over the machine, so the real
//! implementation is rooted: [`Files::rooted`] takes the one directory a run
//! may reach, and every path is checked against it twice. The lexical check
//! refuses an absolute path, a `..` component, and a backslash, none of which
//! can name a place inside the root. The second check follows symbolic links,
//! because a path made only of ordinary components can still leave the root
//! through one. [`Files::in_memory`] is the fake: the same paths are refused
//! for the same reasons, so a test written against it exercises the rules the
//! real filesystem enforces.
//!
//! Paths are always relative to the root and always `/`-separated. `.` names
//! the root itself, which is how `list(".")` asks what a run can see.
//!
//! `read` and `write` move a whole file. `open` and `create` move one a line
//! at a time instead, through the two resource kinds ADR 0018 added: a
//! `files.Reader` answers lines until there are none left, a `files.Writer`
//! takes them, and each is a position in a file, which is why neither may
//! cross a task boundary. Both are reached through the same `files`
//! capability and both go through the same path checks, so a handle cannot
//! name a place the root does not contain.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry, ResourceHandle};
use crate::schema::ModuleSchema;
use crate::value::Value;

/// The most of one line this host will read.
///
/// One mebibyte, and the bound exists for the reason `http`'s do: a host
/// reads what it decided to read rather than what the input asked it to, and
/// what sits under a granted root is not the run's to trust. The bound is the
/// host's and not the program's because `readLine` takes no argument — giving
/// it one would make every caller answer a question about a file it has not
/// seen. `read` stays unbounded, since it is the operation whose name says it
/// wants the whole thing.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// `files`: reading, writing, listing, and removing files under one root.
pub struct Files {
    source: FileSource,
    /// The readers this host still has open, by the identity it issued.
    readers: Mutex<BTreeMap<u64, ReaderState>>,
    /// The writers this host still has open, by the identity it issued.
    writers: Mutex<BTreeMap<u64, WriterState>>,
    /// The identity the next reader or writer gets.
    ///
    /// One counter serves both kinds, so an identity is unique among
    /// everything this host issued rather than merely among the readers or
    /// merely among the writers. ADR 0013 makes a handle a name and requires
    /// that a name never be reused; a counter per kind would hand out
    /// `files.Reader#1` and `files.Writer#1` from the same host, and then the
    /// number a trace or a diagnostic prints would no longer say on its own
    /// which of this host's resources was meant.
    next_id: AtomicU64,
}

/// One reader this host has open.
struct ReaderState {
    /// The path it was opened on, so a failure names what the program wrote
    /// rather than the handle it was handed back.
    path: String,
    form: ReaderForm,
}

/// Where an open reader reads from, which is whichever form the
/// [`FileSource`] that issued it keeps its files in.
enum ReaderForm {
    /// A buffered handle on the real file, which holds one buffer however
    /// long the file is.
    Rooted(std::io::BufReader<std::fs::File>),
    /// The contents as they stood when the reader was opened, and how far
    /// into them it has read. The fake tree holds a `String` rather than
    /// something to seek in, so a position is a byte offset into that.
    InMemory { contents: String, position: usize },
}

/// One writer this host has open.
struct WriterState {
    /// The path it was created on, for the reason [`ReaderState::path`] is
    /// kept.
    path: String,
    form: WriterForm,
}

/// Where an open writer writes to.
enum WriterForm {
    /// A buffered handle on the real file, which `close` flushes.
    Rooted(std::io::BufWriter<std::fs::File>),
    /// The key the fake tree stores this file under, and the text written so
    /// far.
    InMemory { key: String, text: String },
}

enum FileSource {
    /// The real filesystem, reachable only inside this directory.
    Rooted(PathBuf),
    /// A tree that lives only in this process, keyed by `/`-separated
    /// relative path.
    ///
    /// The real filesystem is the operating system's to synchronize; this
    /// tree is the host's own state, so the host locks it. Two tasks writing
    /// at once therefore take turns here exactly as they do on disk.
    InMemory(Mutex<BTreeMap<String, String>>),
}

/// What `files` declares about itself.
///
/// The table is [`cove_schema::hosts::FILES`], so the description the
/// compiler checks a call against and the one the boundary dispatches through
/// are the same bytes.
const SCHEMA: ModuleSchema = cove_schema::hosts::FILES;

impl Files {
    /// The real filesystem, reachable only inside `root`.
    ///
    /// The root is the host's choice, never the program's: no path a program
    /// writes can name a place outside it, so granting `files` grants exactly
    /// this directory.
    ///
    /// `root` need not exist yet. Until it does, every read answers that the
    /// path is not there; the first `write` creates it, along with any
    /// directories the written path names below it, so a program does not
    /// have to know whether the host prepared the tree.
    pub fn rooted(root: PathBuf) -> Self {
        Files::with_source(FileSource::Rooted(root))
    }

    /// A fake filesystem that lives only in this process, for tests.
    ///
    /// Each key is a `/`-separated relative path, exactly as a program would
    /// write it, and its value is the file's contents. A path with no key of
    /// its own but with keys below it is a directory.
    pub fn in_memory(files: BTreeMap<String, String>) -> Self {
        Files::with_source(FileSource::InMemory(Mutex::new(files)))
    }

    fn with_source(source: FileSource) -> Self {
        Files {
            source,
            readers: Mutex::new(BTreeMap::new()),
            writers: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn read(&self, path: &str) -> Result<String, String> {
        match &self.source {
            FileSource::Rooted(root) => {
                let full = rooted_path(root, path)?;
                std::fs::read_to_string(&full).map_err(|e| read_error(path, &e))
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                stored(files)
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| missing(path))
            }
        }
    }

    fn write(&self, path: &str, contents: &str) -> Result<(), String> {
        match &self.source {
            FileSource::Rooted(root) => {
                // A path this host refuses must not reach the filesystem at
                // all, so the lexical rules are applied before anything is
                // created.
                relative_parts(path)?;
                // The root is the host's own directory, so creating it is
                // never an escape. It has to exist before the containment
                // check below, which resolves symbolic links and therefore
                // needs a real directory to resolve against.
                std::fs::create_dir_all(root)
                    .map_err(|e| format!("files: cannot create the root directory: {e}"))?;
                let full = rooted_path(root, path)?;
                // `rooted_path` refused every component that could climb out,
                // so the directories still missing below the deepest existing
                // one can only be created inside the root.
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("files: cannot write `{path}`: {e}"))?;
                }
                std::fs::write(&full, contents)
                    .map_err(|e| format!("files: cannot write `{path}`: {e}"))
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                if key.is_empty() {
                    return Err(format!("files: `{path}` is a directory"));
                }
                stored(files).insert(key, contents.to_string());
                Ok(())
            }
        }
    }

    /// Whether `path` names something this host can reach.
    ///
    /// A path this host refuses answers `false` rather than reporting the
    /// refusal: outside the root there is nothing for a run to observe, and
    /// an `exists` that distinguished "refused" from "absent" would disclose
    /// what lies outside the capability.
    fn exists(&self, path: &str) -> bool {
        match &self.source {
            FileSource::Rooted(root) => match rooted_path(root, path) {
                Ok(full) => full.exists(),
                Err(_) => false,
            },
            FileSource::InMemory(files) => match relative_key(path) {
                Ok(key) if key.is_empty() => true,
                Ok(key) => {
                    let prefix = format!("{key}/");
                    let files = stored(files);
                    files.contains_key(&key) || files.keys().any(|k| k.starts_with(&prefix))
                }
                Err(_) => false,
            },
        }
    }

    /// The names directly inside the directory `path`, in ascending order.
    ///
    /// The names are the entries themselves, not paths: a program joins them
    /// onto the directory it asked about. Ordering is defined so that a
    /// listing is the same on every run and every platform.
    fn list(&self, path: &str) -> Result<Vec<String>, String> {
        match &self.source {
            FileSource::Rooted(root) => {
                let full = rooted_path(root, path)?;
                let entries = std::fs::read_dir(&full).map_err(|e| read_error(path, &e))?;
                let mut names = BTreeSet::new();
                for entry in entries {
                    let entry = entry.map_err(|e| format!("files: cannot list `{path}`: {e}"))?;
                    names.insert(entry.file_name().to_string_lossy().into_owned());
                }
                Ok(names.into_iter().collect())
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                let depth = if key.is_empty() {
                    0
                } else {
                    key.split('/').count()
                };
                let mut names = BTreeSet::new();
                for path in stored(files).keys() {
                    let parts: Vec<&str> = path.split('/').collect();
                    if parts.len() <= depth {
                        continue;
                    }
                    if !key.is_empty() && parts[..depth].join("/") != key {
                        continue;
                    }
                    names.insert(parts[depth].to_string());
                }
                if names.is_empty() && !key.is_empty() {
                    return Err(missing(path));
                }
                Ok(names.into_iter().collect())
            }
        }
    }

    /// Opens `path` for reading and issues the handle that names the reader.
    ///
    /// The path goes through the checks `read` applies and nothing else, so a
    /// path this host refuses for a whole-file read is refused here for the
    /// same stated reason.
    fn open(&self, path: &str) -> Result<Value, String> {
        let form = match &self.source {
            FileSource::Rooted(root) => {
                let full = rooted_path(root, path)?;
                let file = std::fs::File::open(&full).map_err(|e| read_error(path, &e))?;
                ReaderForm::Rooted(std::io::BufReader::new(file))
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                let contents = stored(files)
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| missing(path))?;
                ReaderForm::InMemory {
                    contents,
                    position: 0,
                }
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.readers().insert(
            id,
            ReaderState {
                path: path.to_string(),
                form,
            },
        );
        Ok(Value::Resource(ResourceHandle::new(
            "files",
            &SCHEMA.resources[0],
            id,
        )))
    }

    /// Creates or truncates `path` and issues the handle that names the
    /// writer.
    fn create(&self, path: &str) -> Result<Value, String> {
        let form = match &self.source {
            FileSource::Rooted(root) => {
                // The order `write` uses, and for its reasons: the lexical
                // rules refuse a path before anything is created, the root is
                // the host's own directory and so always safe to create, and
                // the containment check needs a directory that exists to
                // resolve against.
                relative_parts(path)?;
                std::fs::create_dir_all(root)
                    .map_err(|e| format!("files: cannot create the root directory: {e}"))?;
                let full = rooted_path(root, path)?;
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("files: cannot write `{path}`: {e}"))?;
                }
                let file = std::fs::File::create(&full)
                    .map_err(|e| format!("files: cannot write `{path}`: {e}"))?;
                WriterForm::Rooted(std::io::BufWriter::new(file))
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                if key.is_empty() {
                    return Err(format!("files: `{path}` is a directory"));
                }
                // Creating truncates, and the fake truncates when the writer
                // is issued rather than when it is first written to, so the
                // tree says what a freshly created real file says.
                stored(files).insert(key.clone(), String::new());
                WriterForm::InMemory {
                    key,
                    text: String::new(),
                }
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.writers().insert(
            id,
            WriterState {
                path: path.to_string(),
                form,
            },
        );
        Ok(Value::Resource(ResourceHandle::new(
            "files",
            &SCHEMA.resources[1],
            id,
        )))
    }

    /// Writes `text` through the writer `state` names.
    ///
    /// The in-memory form publishes everything written so far under its key on
    /// every call rather than only on `close`, so a `read` of the same path
    /// answers what has been written, the way a read of a real file that has
    /// been flushed does.
    fn write_through(&self, state: &mut WriterState, text: &str) -> Result<(), String> {
        let WriterState { path, form } = state;
        match (&self.source, form) {
            (_, WriterForm::Rooted(file)) => file
                .write_all(text.as_bytes())
                .map_err(|e| format!("files: cannot write `{path}`: {e}")),
            (FileSource::InMemory(files), WriterForm::InMemory { key, text: written }) => {
                written.push_str(text);
                stored(files).insert(key.clone(), written.clone());
                Ok(())
            }
            (FileSource::Rooted(_), WriterForm::InMemory { .. }) => {
                unreachable!("a writer takes the form of the source that issued it")
            }
        }
    }

    /// The readers this host has open, taken back from a poisoned lock for
    /// the reason [`stored`] gives.
    fn readers(&self) -> MutexGuard<'_, BTreeMap<u64, ReaderState>> {
        self.readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The writers this host has open, taken back from a poisoned lock for
    /// the reason [`stored`] gives.
    fn writers(&self) -> MutexGuard<'_, BTreeMap<u64, WriterState>> {
        self.writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        match &self.source {
            FileSource::Rooted(root) => {
                let full = rooted_path(root, path)?;
                std::fs::remove_file(&full).map_err(|e| read_error(path, &e))
            }
            FileSource::InMemory(files) => {
                let key = relative_key(path)?;
                match stored(files).remove(&key) {
                    Some(_) => Ok(()),
                    None => Err(missing(path)),
                }
            }
        }
    }
}

/// The in-memory tree, taken back from a lock a panicking run may have
/// poisoned: a broken invariant in one task must not turn every later
/// `files` call in another into a second, unrelated failure.
fn stored(files: &Mutex<BTreeMap<String, String>>) -> MutexGuard<'_, BTreeMap<String, String>> {
    files
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The message for a path that names nothing this host can reach.
fn missing(path: &str) -> String {
    format!("files: `{path}` does not exist")
}

/// Reports an error from an operation that only observes the filesystem.
///
/// A missing path is reported the same way whether the filesystem or this
/// host's own bookkeeping noticed it, so the fake and the real
/// implementation answer a missing path identically.
fn read_error(path: &str, error: &std::io::Error) -> String {
    match error.kind() {
        ErrorKind::NotFound => missing(path),
        _ => format!("files: cannot read `{path}`: {error}"),
    }
}

/// The components of `path`, refusing anything that could name a place
/// outside a root.
///
/// The rules are lexical, so they hold before the filesystem is touched and
/// hold identically for the in-memory fake:
///
/// - an empty path names nothing;
/// - a NUL byte cannot appear in a path the operating system will accept;
/// - an absolute path names a place chosen by the program rather than the
///   host;
/// - a `..` component climbs out of the root;
/// - a backslash separates components on some platforms and not on others,
///   so a path containing one does not mean the same thing everywhere.
///
/// A `.` component is dropped, so `.` alone names the root.
fn relative_parts(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Err("files: a path must not be empty".to_string());
    }
    if path.contains('\0') {
        return Err(format!("files: `{path}` contains a NUL byte"));
    }
    if path.contains('\\') {
        return Err(format!(
            "files: `{path}` contains a backslash, and a path is `/`-separated and relative to the root this host grants"
        ));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(format!(
            "files: `{path}` is absolute, and a path is relative to the root this host grants"
        ));
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("files: `{path}` leaves the root this host grants"))
            }
        }
    }
    Ok(parts)
}

/// `path` as the `/`-separated key the in-memory fake stores it under. The
/// root itself is the empty key.
fn relative_key(path: &str) -> Result<String, String> {
    Ok(relative_parts(path)?.join("/"))
}

/// `path` resolved against `root`, or the reason this host refuses it.
///
/// The lexical rules in [`relative_parts`] are not enough on their own: every
/// component of `a/b.txt` is ordinary, and `a` may still be a symbolic link
/// to somewhere else entirely. So the deepest ancestor of the resolved path
/// that exists is canonicalized and checked against the canonical root. The
/// components below it cannot climb back out, because `..` was already
/// refused.
fn rooted_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let parts = relative_parts(path)?;
    let mut full = root.to_path_buf();
    for part in &parts {
        full.push(part);
    }
    // A root that does not exist holds nothing, so there is nothing to refuse
    // and nothing to find. `write` creates the root before it asks.
    let Ok(canonical_root) = root.canonicalize() else {
        return Err(missing(path));
    };
    if !within(&canonical_root, &full) {
        return Err(format!(
            "files: `{path}` resolves outside the root this host grants"
        ));
    }
    Ok(full)
}

/// Whether `candidate` really lives under the canonical `root`, following
/// symbolic links as far as `candidate` exists.
fn within(root: &Path, candidate: &Path) -> bool {
    let mut existing = candidate.to_path_buf();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return real.starts_with(root);
        }
        if !existing.pop() {
            return false;
        }
    }
}

/// `Ok(())` or `Err(Error(message))`, the shape every fallible `files`
/// operation answers with.
fn result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => Value::ok(value),
        Err(message) => Value::err(Value::error(message)),
    }
}

/// The next line of `state`, with its terminator removed, or `None` when
/// there is no next line.
///
/// A final line with no terminator is a line: the terminator ends a line
/// rather than making one. A `\r\n` ending is a `\n` ending whose terminator
/// is two bytes, so the `\r` goes with the terminator and is not part of what
/// is answered, while a `\r` at the very end of a file with no `\n` after it
/// is an ordinary byte of the last line.
fn read_line(state: &mut ReaderState) -> Result<Option<String>, String> {
    let ReaderState { path, form } = state;
    match form {
        ReaderForm::Rooted(reader) => {
            // `take` bounds the read rather than what the read kept, so a
            // line past the bound stops the reader at the bound instead of
            // being gathered whole and then measured.
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut bytes)
                .map_err(|e| format!("files: cannot read `{path}`: {e}"))?;
            if bytes.is_empty() {
                return Ok(None);
            }
            if bytes.len() > MAX_LINE_BYTES && bytes.last() != Some(&b'\n') {
                return Err(too_long(path));
            }
            let line = String::from_utf8(strip_terminator(bytes))
                .map_err(|_| format!("files: `{path}` is not UTF-8"))?;
            Ok(Some(line))
        }
        ReaderForm::InMemory { contents, position } => {
            if *position >= contents.len() {
                return Ok(None);
            }
            // Every position this advances to is just past a `\n`, which is
            // one byte and never part of another character, so the slice is
            // always taken at a character boundary.
            let rest = &contents[*position..];
            let (line, advance) = match rest.find('\n') {
                Some(at) => (&rest[..at], at + 1),
                None => (rest, rest.len()),
            };
            if line.len() > MAX_LINE_BYTES {
                return Err(too_long(path));
            }
            let terminated = advance > line.len();
            *position += advance;
            let line = if terminated {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            };
            Ok(Some(line.to_string()))
        }
    }
}

/// `bytes` without the line terminator it ended with, if it ended with one.
fn strip_terminator(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    bytes
}

/// The message for a line longer than this host reads, which names the bound
/// so a run learns the number rather than only that there was one.
fn too_long(path: &str) -> String {
    format!("files: `{path}` has a line longer than the {MAX_LINE_BYTES} bytes this host reads")
}

/// Flushes whatever `state` is still holding.
///
/// Dropping a [`std::io::BufWriter`] flushes it and throws the error away, so
/// a `close` that did not flush here would report success for bytes that
/// never reached the disk. The in-memory form has nothing to flush, because
/// every write already published.
fn flush(state: &mut WriterState) -> Result<(), String> {
    let WriterState { path, form } = state;
    match form {
        WriterForm::Rooted(file) => file
            .flush()
            .map_err(|e| format!("files: cannot write `{path}`: {e}")),
        WriterForm::InMemory { .. } => Ok(()),
    }
}

/// A call on a handle whose reader or writer this host no longer has.
///
/// This is a [`RuntimeError`] rather than a Cove `Err`, and deliberately: a
/// line read from a reader that was closed is not an expected failure the
/// program should handle, it is the program having kept a name past the thing
/// it named.
fn closed(handle: &ResourceHandle, op: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "`{handle}` is closed, so `{op}` has nothing to act on"
    ))
    .with_rule(
        "A host resource handle names a resource the host owns. Closing the resource ends the handle; the name outlives it and addresses nothing.",
    )
    .with_help("open a new one, or move the `close` after the last use")
}

impl HostApi for Files {
    fn module_schema(&self) -> ModuleSchema {
        SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "read" => {
                let path = one_path(op, &args)?;
                Ok(result(self.read(&path).map(|text| Value::Str(text.into()))))
            }
            "write" => {
                let [Value::Str(path), Value::Str(contents)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                let (path, contents) = (path.to_string(), contents.to_string());
                Ok(result(self.write(&path, &contents).map(|()| Value::Unit)))
            }
            "exists" => {
                let path = one_path(op, &args)?;
                Ok(Value::Bool(self.exists(&path)))
            }
            "list" => {
                let path = one_path(op, &args)?;
                Ok(result(self.list(&path).map(|names| {
                    Value::Array(names.into_iter().map(|n| Value::Str(n.into())).collect())
                })))
            }
            "delete" => {
                let path = one_path(op, &args)?;
                Ok(result(self.delete(&path).map(|()| Value::Unit)))
            }
            "open" => {
                let path = one_path(op, &args)?;
                Ok(result(self.open(&path)))
            }
            "create" => {
                let path = one_path(op, &args)?;
                Ok(result(self.create(&path)))
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }

    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        _back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match handle.type_name.as_str() {
            "Reader" => match op {
                "readLine" => {
                    // The lock is held across the read. Nothing a reader does
                    // reenters the interpreter, so holding it cannot deadlock,
                    // and taking the state out to read outside the lock would
                    // let a `close` elsewhere find the entry missing while its
                    // reader was still reading.
                    let mut readers = self.readers();
                    let Some(state) = readers.get_mut(&handle.id) else {
                        return Err(closed(handle, op));
                    };
                    Ok(result(read_line(state).map(|line| match line {
                        Some(text) => Value::some(Value::Str(text.into())),
                        None => Value::none(),
                    })))
                }
                "close" => match self.readers().remove(&handle.id) {
                    Some(_) => Ok(Value::ok(Value::Unit)),
                    None => Err(closed(handle, op)),
                },
                _ => unreachable!("checked by HostRegistry::call_resource"),
            },
            "Writer" => match op {
                "write" | "writeLine" => {
                    let [Value::Str(text)] = args.as_slice() else {
                        unreachable!("checked by HostRegistry::call_resource")
                    };
                    let mut written = text.to_string();
                    if op == "writeLine" {
                        written.push('\n');
                    }
                    let mut writers = self.writers();
                    let Some(state) = writers.get_mut(&handle.id) else {
                        return Err(closed(handle, op));
                    };
                    Ok(result(
                        self.write_through(state, &written).map(|()| Value::Unit),
                    ))
                }
                // The entry goes whether or not the flush succeeds: the file
                // is closed either way, so a handle that reported a failed
                // flush and stayed open would name a writer nothing can
                // recover.
                "close" => {
                    let Some(mut state) = self.writers().remove(&handle.id) else {
                        return Err(closed(handle, op));
                    };
                    Ok(result(flush(&mut state).map(|()| Value::Unit)))
                }
                _ => unreachable!("checked by HostRegistry::call_resource"),
            },
            _ => unreachable!("checked by HostRegistry::call_resource"),
        }
    }
}

/// The single `String` path argument of `op`.
fn one_path(op: &str, args: &[Value]) -> Result<String, RuntimeError> {
    match args {
        [Value::Str(path)] => Ok(path.to_string()),
        _ => Err(RuntimeError::new(format!(
            "`files.{op}` takes one `String` argument"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Grants, HostRegistry, NoReentry};
    use crate::schema::Effect;
    use std::path::Path;
    use std::sync::Arc;

    /// A temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cove-files-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ok_value(value: Value) -> Value {
        match value.ok_payload() {
            Some(payload) => payload.first().cloned().unwrap_or(Value::Unit),
            None => panic!("expected `Ok(...)`, found {value}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    fn strings(value: Value) -> Vec<String> {
        match value {
            Value::Array(items) => items.iter().map(ToString::to_string).collect(),
            other => panic!("expected an `Array`, found {other}"),
        }
    }

    fn is_true(value: Value) -> bool {
        match value {
            Value::Bool(b) => b,
            other => panic!("expected a `Bool`, found {other}"),
        }
    }

    fn str_arg(text: &str) -> Value {
        Value::Str(text.into())
    }

    /// The handle `open` or `create` answered with.
    fn handle(value: Value) -> Arc<ResourceHandle> {
        match ok_value(value) {
            Value::Resource(handle) => handle,
            other => panic!("expected a resource handle, found {other}"),
        }
    }

    fn opened(files: &Files, path: &str) -> Arc<ResourceHandle> {
        handle(files.call("open", vec![str_arg(path)]).unwrap())
    }

    fn created(files: &Files, path: &str) -> Arc<ResourceHandle> {
        handle(files.call("create", vec![str_arg(path)]).unwrap())
    }

    fn on(files: &Files, handle: &ResourceHandle, op: &str, args: Vec<Value>) -> Value {
        files
            .call_resource(handle, op, args, &mut NoReentry)
            .unwrap_or_else(|error| panic!("`{op}` on `{handle}`: {}", error.message))
    }

    fn next_line(files: &Files, handle: &ResourceHandle) -> Value {
        on(files, handle, "readLine", Vec::new())
    }

    /// The line a `readLine` answered, or `None` at the end of the file.
    fn line(value: Value) -> Option<String> {
        let option = ok_value(value);
        match option.some_payload() {
            Some(payload) => Some(payload.first().map(ToString::to_string).unwrap_or_default()),
            None => {
                assert_eq!(option.to_string(), "None", "expected `Some(..)` or `None`");
                None
            }
        }
    }

    /// The lines `path` holds, read through a reader until it answers `None`.
    fn lines(files: &Files, path: &str) -> Vec<String> {
        let reader = opened(files, path);
        let mut read = Vec::new();
        while let Some(text) = line(next_line(files, &reader)) {
            read.push(text);
        }
        on(files, &reader, "close", Vec::new());
        read
    }

    /// A fake and a real host rooted at a fresh directory, so every rule can
    /// be asserted against both implementations of the same Host API.
    fn both(dir: &TempDir) -> Vec<Files> {
        vec![
            Files::rooted(dir.path().to_path_buf()),
            Files::in_memory(BTreeMap::new()),
        ]
    }

    #[test]
    fn writing_then_reading_answers_what_was_written() {
        let dir = TempDir::new("round-trip");
        for files in both(&dir) {
            let written = files
                .call("write", vec![str_arg("notes.txt"), str_arg("five words")])
                .unwrap();
            assert_eq!(ok_value(written).to_string(), "()");

            let read = files.call("read", vec![str_arg("notes.txt")]).unwrap();
            assert_eq!(ok_value(read).to_string(), "five words");
        }
    }

    #[test]
    fn writing_twice_keeps_only_the_second_contents() {
        let dir = TempDir::new("overwrite");
        for files in both(&dir) {
            files
                .call("write", vec![str_arg("notes.txt"), str_arg("first")])
                .unwrap();
            files
                .call("write", vec![str_arg("notes.txt"), str_arg("second")])
                .unwrap();

            let read = files.call("read", vec![str_arg("notes.txt")]).unwrap();
            assert_eq!(ok_value(read).to_string(), "second");
        }
    }

    #[test]
    fn a_nested_path_is_created_along_with_its_directories() {
        let dir = TempDir::new("nested");
        for files in both(&dir) {
            files
                .call("write", vec![str_arg("a/b/c.txt"), str_arg("deep")])
                .unwrap();

            let read = files.call("read", vec![str_arg("a/b/c.txt")]).unwrap();
            assert_eq!(ok_value(read).to_string(), "deep");
            assert!(is_true(files.call("exists", vec![str_arg("a/b")]).unwrap()));
            assert_eq!(
                strings(ok_value(files.call("list", vec![str_arg("a")]).unwrap())),
                ["b"]
            );
        }
    }

    #[test]
    fn reading_a_path_that_is_not_there_reports_it() {
        let dir = TempDir::new("missing");
        for files in both(&dir) {
            let read = files.call("read", vec![str_arg("absent.txt")]).unwrap();
            assert_eq!(err_message(read), "files: `absent.txt` does not exist");
        }
    }

    #[test]
    fn exists_answers_before_and_after_a_write() {
        let dir = TempDir::new("exists");
        for files in both(&dir) {
            assert!(!is_true(
                files.call("exists", vec![str_arg("notes.txt")]).unwrap()
            ));
            files
                .call("write", vec![str_arg("notes.txt"), str_arg("here")])
                .unwrap();
            assert!(is_true(
                files.call("exists", vec![str_arg("notes.txt")]).unwrap()
            ));
        }
    }

    #[test]
    fn listing_the_root_answers_its_entries_in_order() {
        let dir = TempDir::new("list-root");
        for files in both(&dir) {
            for name in ["b.txt", "a.txt", "c.txt"] {
                files
                    .call("write", vec![str_arg(name), str_arg("x")])
                    .unwrap();
            }

            let listed = files.call("list", vec![str_arg(".")]).unwrap();
            assert_eq!(strings(ok_value(listed)), ["a.txt", "b.txt", "c.txt"]);
        }
    }

    #[test]
    fn listing_a_directory_that_is_not_there_reports_it() {
        let dir = TempDir::new("list-missing");
        for files in both(&dir) {
            let listed = files.call("list", vec![str_arg("nowhere")]).unwrap();
            assert_eq!(err_message(listed), "files: `nowhere` does not exist");
        }
    }

    #[test]
    fn deleting_removes_the_file_and_then_reports_it_gone() {
        let dir = TempDir::new("delete");
        for files in both(&dir) {
            files
                .call("write", vec![str_arg("notes.txt"), str_arg("x")])
                .unwrap();

            let deleted = files.call("delete", vec![str_arg("notes.txt")]).unwrap();
            assert_eq!(ok_value(deleted).to_string(), "()");
            assert!(!is_true(
                files.call("exists", vec![str_arg("notes.txt")]).unwrap()
            ));

            let again = files.call("delete", vec![str_arg("notes.txt")]).unwrap();
            assert_eq!(err_message(again), "files: `notes.txt` does not exist");
        }
    }

    /// Every path that names a place outside the root, refused by both
    /// implementations for the same stated reason, before either one touches
    /// storage.
    #[test]
    fn every_path_that_could_escape_the_root_is_refused() {
        let cases = [
            ("", "files: a path must not be empty"),
            ("..", "files: `..` leaves the root this host grants"),
            (
                "../cove.toml",
                "files: `../cove.toml` leaves the root this host grants",
            ),
            (
                "a/../../b.txt",
                "files: `a/../../b.txt` leaves the root this host grants",
            ),
            (
                "/etc/passwd",
                "files: `/etc/passwd` is absolute, and a path is relative to the root this host grants",
            ),
            (
                "a\\b.txt",
                "files: `a\\b.txt` contains a backslash, and a path is `/`-separated and relative to the root this host grants",
            ),
            ("a\0b", "files: `a\0b` contains a NUL byte"),
        ];

        let dir = TempDir::new("escape");
        for (path, expected) in cases {
            for files in both(&dir) {
                for op in ["read", "list", "delete"] {
                    let refused = files.call(op, vec![str_arg(path)]).unwrap();
                    assert_eq!(err_message(refused), expected, "`{op}` of `{path}`");
                }
                let refused = files
                    .call("write", vec![str_arg(path), str_arg("payload")])
                    .unwrap();
                assert_eq!(err_message(refused), expected, "`write` of `{path}`");
                assert!(
                    !is_true(files.call("exists", vec![str_arg(path)]).unwrap()),
                    "`exists` of `{path}`"
                );
            }
        }
    }

    /// A refused write must not reach the filesystem, not merely report that
    /// it did not.
    #[test]
    fn a_refused_write_leaves_nothing_behind() {
        let dir = TempDir::new("refused-write");
        let outside = dir.path().join("outside.txt");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let files = Files::rooted(root);

        let refused = files
            .call("write", vec![str_arg("../outside.txt"), str_arg("payload")])
            .unwrap();
        assert_eq!(
            err_message(refused),
            "files: `../outside.txt` leaves the root this host grants"
        );
        assert!(!outside.exists());

        // `create` truncates whatever it opens, so a refused one must not
        // reach the filesystem either.
        let refused = files
            .call("create", vec![str_arg("../outside.txt")])
            .unwrap();
        assert_eq!(
            err_message(refused),
            "files: `../outside.txt` leaves the root this host grants"
        );
        assert!(!outside.exists());
    }

    /// A path of ordinary components can still leave the root through a
    /// symbolic link, which no lexical rule can see.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_out_of_the_root_is_refused() {
        let dir = TempDir::new("symlink");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "not yours").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("up")).unwrap();

        let files = Files::rooted(root);
        for path in ["link.txt", "up/secret.txt"] {
            let refused = files.call("read", vec![str_arg(path)]).unwrap();
            assert_eq!(
                err_message(refused),
                format!("files: `{path}` resolves outside the root this host grants")
            );
        }

        let refused = files
            .call("write", vec![str_arg("link.txt"), str_arg("payload")])
            .unwrap();
        assert_eq!(
            err_message(refused),
            "files: `link.txt` resolves outside the root this host grants"
        );
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "not yours");
    }

    /// A symbolic link that stays inside the root is an ordinary path.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_inside_the_root_is_allowed() {
        let dir = TempDir::new("symlink-inside");
        std::fs::write(dir.path().join("real.txt"), "inside").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();

        let files = Files::rooted(dir.path().to_path_buf());
        let read = files.call("read", vec![str_arg("link.txt")]).unwrap();
        assert_eq!(ok_value(read).to_string(), "inside");
    }

    /// The root is the host's to create, so a run against a root that is not
    /// there yet reads nothing and writes normally.
    #[test]
    fn a_root_that_does_not_exist_yet_is_empty_until_the_first_write() {
        let dir = TempDir::new("absent-root");
        let root = dir.path().join("not-created-yet");
        let files = Files::rooted(root.clone());

        let read = files.call("read", vec![str_arg("notes.txt")]).unwrap();
        assert_eq!(err_message(read), "files: `notes.txt` does not exist");
        assert!(!is_true(
            files.call("exists", vec![str_arg("notes.txt")]).unwrap()
        ));
        assert!(!root.exists());

        files
            .call("write", vec![str_arg("notes.txt"), str_arg("now")])
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "now"
        );
    }

    #[test]
    fn a_run_without_the_files_grant_cannot_read() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Files::in_memory(BTreeMap::new())));

        let error = hosts
            .call("files", "read", vec![str_arg("notes.txt")])
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`files.read` requires the `files` capability, which this run was not granted"
        );
    }

    #[test]
    fn a_granted_files_host_is_reachable_through_the_registry() {
        let mut hosts = HostRegistry::new(Grants::new(["files"]));
        hosts.register(Box::new(Files::in_memory(BTreeMap::from([(
            "notes.txt".to_string(),
            "hello".to_string(),
        )]))));

        let read = hosts
            .call("files", "read", vec![str_arg("notes.txt")])
            .expect("the call should be allowed");
        assert_eq!(ok_value(read).to_string(), "hello");
    }

    #[test]
    fn signatures_read_like_source() {
        let files = Files::in_memory(BTreeMap::new());
        let rendered: Vec<String> = files
            .module_schema()
            .operations
            .iter()
            .map(|op| op.signature())
            .collect();
        assert_eq!(
            rendered,
            [
                "read(String) -> Result<String, Error>",
                "write(String, String) -> Result<Unit, Error>",
                "exists(String) -> Bool",
                "list(String) -> Result<Array<String>, Error>",
                "delete(String) -> Result<Unit, Error>",
                "open(String) -> Result<files.Reader, Error>",
                "create(String) -> Result<files.Writer, Error>",
            ]
        );
        let rendered: Vec<String> = SCHEMA.resources[0]
            .operations
            .iter()
            .map(|op| op.signature())
            .collect();
        assert_eq!(
            rendered,
            [
                "readLine() -> Result<Option<String>, Error>",
                "close() -> Result<Unit, Error>",
            ]
        );
        let rendered: Vec<String> = SCHEMA.resources[1]
            .operations
            .iter()
            .map(|op| op.signature())
            .collect();
        assert_eq!(
            rendered,
            [
                "write(String) -> Result<Unit, Error>",
                "writeLine(String) -> Result<Unit, Error>",
                "close() -> Result<Unit, Error>",
            ]
        );
    }

    /// The effect distinction is the point of this host, so it is asserted
    /// rather than left to a reader of the table.
    #[test]
    fn reads_and_writes_declare_different_effects() {
        let files = Files::in_memory(BTreeMap::new());
        for op in files.module_schema().operations {
            let expected = match op.name {
                "read" | "exists" | "list" | "open" => Effect::Read,
                "write" | "delete" | "create" => Effect::IrreversibleWrite,
                other => panic!("unexpected operation `{other}`"),
            };
            assert_eq!(op.effect, expected, "`files.{}`", op.name);
            assert_eq!(
                op.cancellable,
                expected == Effect::Read,
                "`files.{}`",
                op.name
            );
        }
    }

    // ------------------------------------------ streaming: readers and writers

    #[test]
    fn a_writer_and_a_reader_round_trip_the_lines_that_were_written() {
        let dir = TempDir::new("stream-round-trip");
        for files in both(&dir) {
            let writer = created(&files, "log.txt");
            for text in ["first", "second", "third"] {
                let written = on(&files, &writer, "writeLine", vec![str_arg(text)]);
                assert_eq!(ok_value(written).to_string(), "()");
            }
            on(&files, &writer, "close", Vec::new());

            let reader = opened(&files, "log.txt");
            assert_eq!(line(next_line(&files, &reader)).as_deref(), Some("first"));
            assert_eq!(line(next_line(&files, &reader)).as_deref(), Some("second"));
            assert_eq!(line(next_line(&files, &reader)).as_deref(), Some("third"));
            assert_eq!(line(next_line(&files, &reader)), None);
            on(&files, &reader, "close", Vec::new());
        }
    }

    /// `write` puts down exactly what it was given, so a file whose last
    /// piece had no newline ends without one — and that piece is still a
    /// line.
    #[test]
    fn a_last_line_with_no_terminator_is_still_a_line() {
        let dir = TempDir::new("stream-unterminated");
        for files in both(&dir) {
            let writer = created(&files, "log.txt");
            on(&files, &writer, "writeLine", vec![str_arg("first")]);
            on(&files, &writer, "write", vec![str_arg("second")]);
            on(&files, &writer, "close", Vec::new());

            assert_eq!(lines(&files, "log.txt"), ["first", "second"]);
        }
    }

    /// A `\r\n` ending is a `\n` ending whose terminator is two bytes, so
    /// neither byte reaches the program.
    #[test]
    fn a_carriage_return_before_a_newline_is_part_of_the_terminator() {
        let dir = TempDir::new("stream-crlf");
        for files in both(&dir) {
            files
                .call(
                    "write",
                    vec![str_arg("log.txt"), str_arg("first\r\nsecond\r\nthird")],
                )
                .unwrap();

            assert_eq!(lines(&files, "log.txt"), ["first", "second", "third"]);
        }
    }

    #[test]
    fn an_empty_file_answers_no_lines_at_all() {
        let dir = TempDir::new("stream-empty");
        for files in both(&dir) {
            let writer = created(&files, "log.txt");
            on(&files, &writer, "close", Vec::new());

            let reader = opened(&files, "log.txt");
            assert_eq!(line(next_line(&files, &reader)), None);
            on(&files, &reader, "close", Vec::new());
        }
    }

    /// The bound is the host's, so it is the same number on both
    /// implementations and the refusal names it.
    #[test]
    fn a_line_at_the_bound_is_read_and_one_past_it_is_refused() {
        let dir = TempDir::new("stream-bound");
        for files in both(&dir) {
            for (path, length) in [("at.txt", MAX_LINE_BYTES), ("past.txt", MAX_LINE_BYTES + 1)] {
                let contents = "a".repeat(length) + "\n";
                files
                    .call("write", vec![str_arg(path), str_arg(&contents)])
                    .unwrap();
            }

            let reader = opened(&files, "at.txt");
            assert_eq!(
                line(next_line(&files, &reader)).map(|text| text.len()),
                Some(MAX_LINE_BYTES)
            );

            let reader = opened(&files, "past.txt");
            let refused = next_line(&files, &reader);
            assert_eq!(
                err_message(refused),
                format!(
                    "files: `past.txt` has a line longer than the {MAX_LINE_BYTES} bytes this host reads"
                )
            );
        }
    }

    #[test]
    fn opening_a_path_that_is_not_there_reports_it() {
        let dir = TempDir::new("stream-missing");
        for files in both(&dir) {
            let refused = files.call("open", vec![str_arg("absent.txt")]).unwrap();
            assert_eq!(err_message(refused), "files: `absent.txt` does not exist");
        }
    }

    /// A handle is another way to name a path, so it is refused wherever a
    /// path is — with the reason `read` gives, since the checks are the ones
    /// `read` runs.
    #[test]
    fn a_path_that_leaves_the_root_is_refused_for_a_handle_as_it_is_for_a_read() {
        let dir = TempDir::new("stream-escape");
        for path in ["../escape", "/etc/passwd"] {
            for files in both(&dir) {
                let expected = err_message(files.call("read", vec![str_arg(path)]).unwrap());
                for op in ["open", "create"] {
                    let refused = files.call(op, vec![str_arg(path)]).unwrap();
                    assert_eq!(err_message(refused), expected, "`{op}` of `{path}`");
                }
            }
        }
    }

    #[test]
    fn a_reader_that_was_closed_reports_that_its_handle_addresses_nothing() {
        let dir = TempDir::new("stream-closed-reader");
        for files in both(&dir) {
            files
                .call("write", vec![str_arg("log.txt"), str_arg("only\n")])
                .unwrap();
            let reader = opened(&files, "log.txt");
            on(&files, &reader, "close", Vec::new());

            let error = files
                .call_resource(&reader, "readLine", Vec::new(), &mut NoReentry)
                .expect_err("a read from a closed reader is refused");
            assert_eq!(
                error.message,
                "`files.Reader#1` is closed, so `readLine` has nothing to act on"
            );
        }
    }

    #[test]
    fn closing_twice_reports_that_the_handle_addresses_nothing() {
        let dir = TempDir::new("stream-closed-twice");
        for files in both(&dir) {
            let writer = created(&files, "log.txt");
            on(&files, &writer, "close", Vec::new());

            let error = files
                .call_resource(&writer, "close", Vec::new(), &mut NoReentry)
                .expect_err("closing a closed writer is refused");
            assert_eq!(
                error.message,
                "`files.Writer#1` is closed, so `close` has nothing to act on"
            );
        }
    }

    /// One counter serves both kinds, so no two of this host's handles carry
    /// the same number.
    #[test]
    fn a_reader_and_a_writer_of_one_host_never_share_an_identity() {
        let dir = TempDir::new("stream-identity");
        for files in both(&dir) {
            let writer = created(&files, "log.txt");
            let reader = opened(&files, "log.txt");

            assert_eq!(writer.qualified_type(), "files.Writer");
            assert_eq!(reader.qualified_type(), "files.Reader");
            assert_ne!(writer.id, reader.id);
            assert!(!writer.task_safe);
            assert!(!reader.task_safe);
        }
    }

    /// `create` prepares the tree below the root the way `write` does, so a
    /// program does not have to make the directories it is about to write in.
    #[test]
    fn creating_a_nested_path_creates_the_directories_above_it() {
        let dir = TempDir::new("stream-nested");
        for files in both(&dir) {
            let writer = created(&files, "a/b/log.txt");
            on(&files, &writer, "writeLine", vec![str_arg("deep")]);
            on(&files, &writer, "close", Vec::new());

            assert_eq!(lines(&files, "a/b/log.txt"), ["deep"]);
        }
    }

    /// A file the host was pointed at is whatever was in the directory, so a
    /// line that is not text is reported rather than being made into one.
    #[test]
    fn a_line_that_is_not_utf8_is_reported() {
        let dir = TempDir::new("stream-not-utf8");
        std::fs::write(dir.path().join("bytes.bin"), [0xff, 0xfe, b'\n']).unwrap();
        let files = Files::rooted(dir.path().to_path_buf());

        let reader = opened(&files, "bytes.bin");
        assert_eq!(
            err_message(next_line(&files, &reader)),
            "files: `bytes.bin` is not UTF-8"
        );
    }

    /// The fake publishes on every call rather than on `close`, so what a
    /// writer has written is readable while it is still open.
    #[test]
    fn the_fake_publishes_what_a_writer_has_written_before_it_is_closed() {
        let files = Files::in_memory(BTreeMap::new());
        let writer = created(&files, "log.txt");
        on(&files, &writer, "writeLine", vec![str_arg("first")]);

        let read = files.call("read", vec![str_arg("log.txt")]).unwrap();
        assert_eq!(ok_value(read).to_string(), "first\n");
    }

    #[test]
    fn a_run_without_the_files_grant_cannot_open_or_use_a_reader() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Files::in_memory(BTreeMap::new())));

        let error = hosts
            .call("files", "open", vec![str_arg("notes.txt")])
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`files.open` requires the `files` capability, which this run was not granted"
        );

        let handle = ResourceHandle::new("files", &SCHEMA.resources[0], 1);
        let error = hosts
            .call_resource(&handle, "readLine", Vec::new(), &mut NoReentry)
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`files.Reader.readLine` requires the `files` capability, which this run was not granted"
        );
    }

    /// ADR 0018's effects, asserted rather than left to a reader of the
    /// table: reading a line reads, writing one cannot be undone, and closing
    /// gives back what opening took.
    #[test]
    fn a_reader_and_a_writer_declare_the_effects_their_calls_have() {
        for resource in SCHEMA.resources {
            assert!(!resource.task_safe, "`files.{}`", resource.name);
            for op in resource.operations {
                let expected = match op.name {
                    "readLine" => Effect::Read,
                    "write" | "writeLine" => Effect::IrreversibleWrite,
                    "close" => Effect::ReversibleWrite,
                    other => panic!("unexpected operation `{other}`"),
                };
                assert_eq!(op.effect, expected, "`files.{}.{}`", resource.name, op.name);
                assert_eq!(
                    op.cancellable,
                    expected == Effect::Read,
                    "`files.{}.{}`",
                    resource.name,
                    op.name
                );
            }
        }
    }
}
