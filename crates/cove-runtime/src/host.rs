//! The Host API boundary.
//!
//! Cove code has no ambient authority. Files, network, clocks, processes, and
//! databases are explicit capabilities with replaceable real, fake, filtered,
//! or denied implementations. The runtime rejects Host API calls that were not
//! granted.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

use cove_sema::Capability;

use crate::error::RuntimeError;
use crate::value::Value;

/// One host-provided module, such as `console` or `env`.
pub trait HostApi {
    /// The name Cove source uses, such as `console`.
    fn name(&self) -> &str;

    /// The capability a host must grant for this module.
    fn capability(&self) -> Capability;

    /// The operations this module exposes.
    fn operations(&self) -> &[&str];

    /// Invokes one operation.
    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError>;
}

/// The set of capabilities granted at the execution boundary.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    granted: BTreeSet<Capability>,
}

impl Grants {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Grants {
            granted: names.into_iter().map(Capability::new).collect(),
        }
    }

    pub fn allows(&self, capability: &Capability) -> bool {
        self.granted.contains(capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.granted.iter()
    }
}

/// Holds every host module available to a run, and the grants that gate them.
pub struct HostRegistry {
    modules: Vec<Box<dyn HostApi>>,
    grants: Grants,
}

impl HostRegistry {
    pub fn new(grants: Grants) -> Self {
        HostRegistry {
            modules: Vec::new(),
            grants,
        }
    }

    pub fn register(&mut self, module: Box<dyn HostApi>) {
        self.modules.push(module);
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    pub fn contains(&self, name: &str) -> bool {
        self.modules.iter().any(|m| m.name() == name)
    }

    /// Looks up which host module exposes `op`, for unqualified `use` imports.
    pub fn module_for_operation(&self, op: &str) -> Option<&str> {
        self.modules
            .iter()
            .find(|m| m.operations().contains(&op))
            .map(|m| m.name())
    }

    /// Dispatches a Host API call after checking the grant.
    pub fn call(
        &mut self,
        module: &str,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let Some(entry) = self.modules.iter_mut().find(|m| m.name() == module) else {
            return Err(RuntimeError::new(format!("unknown host module `{module}`")));
        };
        let capability = entry.capability();
        if !self.grants.allows(&capability) {
            return Err(RuntimeError::new(format!(
                "`{module}.{op}` requires the `{capability}` capability, which this run was not granted"
            ))
            .with_rule("Cove code has no ambient authority; the host grants capabilities at the execution boundary.")
            .with_help(format!(
                "add `{capability}` to `allow` in the run's `cove.toml` table"
            )));
        }
        if !entry.operations().contains(&op) {
            return Err(RuntimeError::new(format!(
                "host module `{module}` has no operation `{op}`"
            )));
        }
        entry.call(op, args)
    }
}

/// `console`: line-oriented output.
pub struct Console<W: Write> {
    out: W,
}

impl<W: Write> Console<W> {
    pub fn new(out: W) -> Self {
        Console { out }
    }
}

impl<W: Write> HostApi for Console<W> {
    fn name(&self) -> &str {
        "console"
    }

    fn capability(&self) -> Capability {
        Capability::new("console")
    }

    fn operations(&self) -> &[&str] {
        &["println", "print"]
    }

    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let text = args
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let result = match op {
            "println" => writeln!(self.out, "{text}"),
            "print" => write!(self.out, "{text}"),
            _ => unreachable!("checked by HostRegistry::call"),
        };
        match result.and_then(|_| self.out.flush()) {
            Ok(()) => Ok(Value::ok(Value::Unit)),
            Err(e) => Ok(Value::err(Value::error(format!("console: {e}")))),
        }
    }
}

/// `env`: read-only access to the environment the host supplies.
///
/// The map is given to the constructor rather than read from the process, so a
/// host decides exactly which variables a run can observe.
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Builds an environment from the variables the host chooses to expose.
    pub fn new(vars: BTreeMap<String, String>) -> Self {
        Env { vars }
    }

    /// Snapshots the real process environment. Explicit by design: nothing
    /// else in the runtime reads `std::env`.
    pub fn from_process() -> Self {
        Env {
            vars: std::env::vars().collect(),
        }
    }
}

impl HostApi for Env {
    fn name(&self) -> &str {
        "env"
    }

    fn capability(&self) -> Capability {
        Capability::new("env")
    }

    fn operations(&self) -> &[&str] {
        &["get"]
    }

    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "get" => {
                let [Value::Str(name)] = args.as_slice() else {
                    return Err(RuntimeError::new("`env.get` takes one `String` argument"));
                };
                Ok(match self.vars.get(&**name) {
                    Some(value) => Value::some(Value::Str(value.as_str().into())),
                    None => Value::none(),
                })
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

/// `documents`: a filtered, read-only view over a fixed set of named text
/// documents.
///
/// Granting `documents` never grants filesystem access. A host names exactly
/// which documents exist; there is no way to reach a path this module was not
/// built to expose, so a grant of `documents` is narrow authority, never
/// ambient access to a directory.
pub struct Documents {
    source: DocumentsSource,
}

enum DocumentsSource {
    InMemory(BTreeMap<String, String>),
    Rooted(PathBuf),
}

impl Documents {
    /// A fake implementation backed by an in-memory map, for tests.
    pub fn in_memory(documents: BTreeMap<String, String>) -> Self {
        Documents {
            source: DocumentsSource::InMemory(documents),
        }
    }

    /// Reads `<root>/<name>.txt` for a document named `name`.
    ///
    /// `name` must be a single plain path component: empty names, `.`, `..`,
    /// and names containing `/`, `\`, or a NUL byte are all rejected before
    /// the filesystem is touched. This keeps the capability narrow: a grant
    /// of `documents` can only ever reach the fixed set of `.txt` files under
    /// `root`, never an arbitrary path via traversal or an absolute path.
    pub fn rooted(root: PathBuf) -> Self {
        Documents {
            source: DocumentsSource::Rooted(root),
        }
    }

    fn read(&self, name: &str) -> Result<String, String> {
        let missing = || format!("no document named `{name}`");
        match &self.source {
            DocumentsSource::InMemory(documents) => {
                documents.get(name).cloned().ok_or_else(missing)
            }
            DocumentsSource::Rooted(root) => {
                if !is_plain_document_name(name) {
                    return Err(missing());
                }
                std::fs::read_to_string(root.join(format!("{name}.txt"))).map_err(|_| missing())
            }
        }
    }
}

/// Whether `name` is safe to join onto a root: a single component, never a
/// path that could escape it.
fn is_plain_document_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

impl HostApi for Documents {
    fn name(&self) -> &str {
        "documents"
    }

    fn capability(&self) -> Capability {
        Capability::new("documents")
    }

    fn operations(&self) -> &[&str] {
        &["read"]
    }

    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "read" => {
                let [Value::Str(name)] = args.as_slice() else {
                    return Err(RuntimeError::new(
                        "`documents.read` takes one `String` argument",
                    ));
                };
                Ok(match self.read(name) {
                    Ok(text) => Value::ok(Value::Str(text.into())),
                    Err(message) => Value::err(Value::error(message)),
                })
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cove-runtime-test-{name}-{}-{}",
                std::process::id(),
                nanos()
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

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn ok_str(value: Value) -> String {
        match value {
            Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Ok" => {
                match result.payload.first() {
                    Some(Value::Str(text)) => text.to_string(),
                    other => panic!("expected `Ok(String)`, found {other:?}"),
                }
            }
            other => panic!("expected `Ok(String)`, found {other}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value {
            Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Err" => {
                result
                    .payload
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            }
            other => panic!("expected `Err(...)`, found {other}"),
        }
    }

    #[test]
    fn in_memory_read_hits_and_misses() {
        let mut documents = Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]));

        let hit = documents
            .call("read", vec![Value::Str("input".into())])
            .expect("no runtime error");
        assert_eq!(ok_str(hit), "hello world");

        let miss = documents
            .call("read", vec![Value::Str("missing".into())])
            .expect("no runtime error");
        assert_eq!(err_message(miss), "no document named `missing`");
    }

    #[test]
    fn rooted_reads_a_real_file() {
        let dir = TempDir::new("rooted-read");
        std::fs::write(dir.path().join("input.txt"), "five little words here").unwrap();
        let mut documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("input".into())])
            .expect("no runtime error");
        assert_eq!(ok_str(read), "five little words here");
    }

    #[test]
    fn rooted_rejects_a_missing_document() {
        let dir = TempDir::new("rooted-missing");
        let mut documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("absent".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `absent`");
    }

    #[test]
    fn rooted_rejects_path_traversal() {
        let dir = TempDir::new("rooted-traversal");
        let mut documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("..".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `..`");
    }

    #[test]
    fn rooted_rejects_a_nested_path() {
        let dir = TempDir::new("rooted-nested");
        let mut documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("a/b".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `a/b`");
    }

    #[test]
    fn rooted_rejects_an_empty_name() {
        let dir = TempDir::new("rooted-empty");
        let mut documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named ``");
    }

    #[test]
    fn registry_without_the_documents_grant_rejects_the_call() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));

        let error = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected");
        assert!(error.message.contains("documents"), "{}", error.message);
    }

    #[test]
    fn registry_with_the_documents_grant_allows_the_call() {
        let mut hosts = HostRegistry::new(Grants::new(["documents"]));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]))));

        let value = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");
        assert_eq!(ok_str(value), "hello world");
    }
}
