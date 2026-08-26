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

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::error::RuntimeError;
use crate::host::HostApi;
use crate::schema::ModuleSchema;
use crate::value::Value;

/// `files`: reading, writing, listing, and removing files under one root.
pub struct Files {
    source: FileSource,
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
        Files {
            source: FileSource::Rooted(root),
        }
    }

    /// A fake filesystem that lives only in this process, for tests.
    ///
    /// Each key is a `/`-separated relative path, exactly as a program would
    /// write it, and its value is the file's contents. A path with no key of
    /// its own but with keys below it is a directory.
    pub fn in_memory(files: BTreeMap<String, String>) -> Self {
        Files {
            source: FileSource::InMemory(Mutex::new(files)),
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
            _ => unreachable!("checked by HostRegistry::call"),
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
    use crate::host::{Grants, HostRegistry};
    use crate::schema::Effect;
    use std::path::Path;

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
        let rendered: Vec<String> = files.schema().iter().map(|op| op.signature()).collect();
        assert_eq!(
            rendered,
            [
                "read(String) -> Result<String, Error>",
                "write(String, String) -> Result<Unit, Error>",
                "exists(String) -> Bool",
                "list(String) -> Result<Array<String>, Error>",
                "delete(String) -> Result<Unit, Error>",
            ]
        );
    }

    /// The effect distinction is the point of this host, so it is asserted
    /// rather than left to a reader of the table.
    #[test]
    fn reads_and_writes_declare_different_effects() {
        let files = Files::in_memory(BTreeMap::new());
        for op in files.schema() {
            let expected = match op.name {
                "read" | "exists" | "list" => Effect::Read,
                "write" | "delete" => Effect::IrreversibleWrite,
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
}
