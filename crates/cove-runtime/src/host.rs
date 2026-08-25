//! The Host API boundary.
//!
//! Cove code has no ambient authority. Files, network, clocks, processes, and
//! databases are explicit capabilities with replaceable real, fake, filtered,
//! or denied implementations. The runtime rejects Host API calls that were not
//! granted.
//!
//! This module holds the boundary itself — [`HostApi`], [`Grants`], and
//! [`HostRegistry`] — together with the three small modules that have nothing
//! else to say: [`Console`], [`Env`], and [`Documents`]. A host with rules of
//! its own gets a module of its own: [`crate::clock`], [`crate::files`],
//! [`crate::process`], and [`crate::database`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cove_sema::Capability;

use crate::budget::Budget;
use crate::error::RuntimeError;
use crate::schema::{Effect, HostType, OperationSchema};
use crate::trace::{HostOutcome, NullSink, RecordedValue, TraceEvent, TraceSink};
use crate::value::Value;

/// One host-provided module, such as `console` or `env`.
///
/// A host is shared by every task of a run, so an operation is invoked
/// through a shared reference and a host is `Send + Sync`. A host that needs
/// mutable state of its own says so with a lock it owns, which is also what
/// decides how much of it two tasks may do at once: `console` serializes its
/// writes so a line is never torn, while `clock.sleep` holds nothing, so two
/// tasks can wait at the same time instead of queueing behind each other.
pub trait HostApi: Send + Sync {
    /// The name Cove source uses, such as `console`.
    fn name(&self) -> &str;

    /// The capability a host must grant for this module.
    fn capability(&self) -> Capability;

    /// The operations this module exposes, and everything the runtime needs
    /// to know about each of them.
    ///
    /// The schema is the module's declaration of itself: a host cannot expose
    /// an operation without saying what it takes, what it produces, what it
    /// costs the outside world, and whether its result may cross a task
    /// boundary.
    fn schema(&self) -> &[OperationSchema];

    /// Invokes one operation.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError>;
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
///
/// `HostRegistry::call` is the single choke point through which Cove code
/// reaches external authority, so it is also the right place to observe that
/// authority being exercised: an optional [`Budget`] charges every call
/// against the run's host-call limit before dispatch, and an optional
/// [`TraceSink`] records every call, granted or denied, with how long it
/// took.
pub struct HostRegistry {
    modules: Vec<Box<dyn HostApi>>,
    grants: Grants,
    trace: Arc<dyn TraceSink>,
    /// The run's budget, shared by every task: ADR 0008 draws a task's fuel
    /// from the run's budget rather than giving each task one of its own, so
    /// there is still exactly one authoritative count of what the run spent.
    budget: Mutex<Option<Budget>>,
    irreversible_writes: AtomicU64,
}

impl HostRegistry {
    pub fn new(grants: Grants) -> Self {
        HostRegistry {
            modules: Vec::new(),
            grants,
            trace: Arc::new(NullSink),
            budget: Mutex::new(None),
            irreversible_writes: AtomicU64::new(0),
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

    /// Installs where trace events go. Replaces any sink installed earlier;
    /// the default is [`NullSink`], which discards everything.
    pub fn set_trace(&mut self, sink: Arc<dyn TraceSink>) {
        self.trace = sink;
    }

    /// Installs the budget every call is charged against. Replaces any
    /// budget installed earlier; the default is no budget, which imposes no
    /// host-call limit here (the interpreter's own safepoints still apply
    /// its other limits).
    pub fn set_budget(&mut self, budget: Budget) {
        *self
            .budget
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(budget);
    }

    /// Runs `f` against the run's budget, if the host installed one.
    ///
    /// This is how the interpreter charges its own safepoints — loop back
    /// edges, calls, and `await` — and how a caller reads the counters after
    /// a run. Every task thread reaches the one budget through here, so the
    /// lock is held for the charge and nothing else.
    pub fn with_budget<R>(&self, f: impl FnOnce(&mut Budget) -> R) -> Option<R> {
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        budget.as_mut().map(f)
    }

    /// How many calls this run dispatched whose schema declares them
    /// [`Effect::IrreversibleWrite`].
    ///
    /// This is what reads the `effect` an operation declares. Cove makes
    /// irreversible operations require visible intent, so a run is able to
    /// say how many of the things it did cannot be taken back; `cove run
    /// --stats` prints the count. Whether each call actually reached the
    /// outside world is the host's business rather than the registry's, so a
    /// call the host answered with `Err` is still counted: the registry knows
    /// only that a program asked for something irreversible.
    pub fn irreversible_writes(&self) -> u64 {
        self.irreversible_writes.load(Ordering::Relaxed)
    }

    /// Looks up which host module exposes `op`, for unqualified `use` imports.
    pub fn module_for_operation(&self, op: &str) -> Option<&str> {
        self.modules
            .iter()
            .find(|m| m.schema().iter().any(|entry| entry.name == op))
            .map(|m| m.name())
    }

    /// The schema of one operation, if the module and the operation both
    /// exist.
    pub fn schema_for(&self, module: &str, op: &str) -> Option<&OperationSchema> {
        self.modules
            .iter()
            .find(|m| m.name() == module)?
            .schema()
            .iter()
            .find(|entry| entry.name == op)
    }

    /// Whether the value `module.op` produces may cross a task boundary, or
    /// `None` when no such operation exists.
    ///
    /// The Language Card puts this decision in the schema rather than in the
    /// value: "Host resources declare task-safety in their Host API schema."
    pub fn result_is_task_safe(&self, module: &str, op: &str) -> Option<bool> {
        Some(self.schema_for(module, op)?.result_is_task_safe)
    }

    /// Describes `args` for a trace, or hands back nothing when no sink will
    /// read them.
    ///
    /// A [`RecordedValue`] is a copy, and one of a value no boundary may
    /// carry is also a rendering of it, so an untraced run makes neither: an
    /// event nothing keeps has nothing worth describing.
    fn record_args(&self, args: &[Value]) -> Vec<RecordedValue> {
        if !self.trace.is_recording() {
            return Vec::new();
        }
        args.iter().map(RecordedValue::of).collect()
    }

    /// Dispatches a Host API call after checking the grant, the schema, and
    /// the budget, tracing the outcome either way.
    pub fn call(&self, module: &str, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let Some(entry) = self.modules.iter().find(|m| m.name() == module) else {
            return Err(RuntimeError::new(format!("unknown host module `{module}`")));
        };
        let declared = entry
            .schema()
            .iter()
            .find(|entry| entry.name == op)
            .copied();
        // An operation declares the capability it needs; the module's own
        // capability stands in for an operation that does not exist, so a
        // call into an ungranted module is still reported as ungranted rather
        // than as a misspelling.
        let capability = match &declared {
            Some(schema) => Capability::new(schema.capability),
            None => entry.capability(),
        };
        if !self.grants.allows(&capability) {
            self.trace.record(TraceEvent::HostCall {
                module: module.to_string(),
                op: op.to_string(),
                capability: capability.to_string(),
                wait: std::time::Duration::ZERO,
                granted: false,
                args: self.record_args(&args),
                outcome: None,
            });
            return Err(RuntimeError::new(format!(
                "`{module}.{op}` requires the `{capability}` capability, which this run was not granted"
            ))
            .with_rule("Cove code has no ambient authority; the host grants capabilities at the execution boundary.")
            .with_help(format!(
                "add `{capability}` to `allow` in the run's `cove.toml` table"
            )));
        }
        let Some(schema) = declared else {
            let known = entry
                .schema()
                .iter()
                .map(|entry| format!("`{}`", entry.name))
                .collect::<Vec<_>>();
            return Err(RuntimeError::new(format!(
                "host module `{module}` has no operation `{op}`"
            ))
            .with_help(format!(
                "`{module}` exposes {}",
                if known.is_empty() {
                    "no operations".to_string()
                } else {
                    known.join(", ")
                }
            )));
        };
        if !schema.accepts(args.len()) {
            return Err(RuntimeError::new(format!(
                "`{module}.{op}` takes {}, but {} were given",
                schema.expected_arity(),
                args.len()
            ))
            .with_help(format!(
                "the Host API schema declares `{module}.{}`",
                schema.signature()
            )));
        }

        if let Some(Err(error)) = self.with_budget(|budget| {
            budget
                .charge_host_call()
                .map_err(|stopped| budget.to_runtime_error(stopped))
        }) {
            self.trace.record(TraceEvent::HostCall {
                module: module.to_string(),
                op: op.to_string(),
                capability: capability.to_string(),
                wait: std::time::Duration::ZERO,
                granted: false,
                args: self.record_args(&args),
                outcome: None,
            });
            return Err(error);
        }

        if schema.effect == Effect::IrreversibleWrite {
            self.irreversible_writes.fetch_add(1, Ordering::Relaxed);
        }

        // A trace has to carry the arguments to be replayable, and the host
        // takes ownership of them, so they are recorded before dispatch
        // rather than reconstructed afterwards.
        let recorded_args = self.record_args(&args);
        let started = Instant::now();
        let result = entry.call(op, args);
        let wait = started.elapsed();
        // The schema decides whether the result is written down. An operation
        // that is not recordable has its call recorded and its result left
        // out: replaying `process.exit` by handing back a value would keep
        // running a program that had ended.
        if self.trace.is_recording() {
            let outcome = if schema.recordable {
                match &result {
                    Ok(value) => HostOutcome::Value(RecordedValue::of(value)),
                    Err(error) => HostOutcome::Error(error.message.clone()),
                }
            } else {
                HostOutcome::NotRecordable
            };
            self.trace.record(TraceEvent::HostCall {
                module: module.to_string(),
                op: op.to_string(),
                capability: capability.to_string(),
                wait,
                granted: true,
                args: recorded_args,
                outcome: Some(outcome),
            });
        }
        result
    }
}

/// The name, capability, and operations of one host module, without the
/// module itself.
///
/// [`HostApi::schema`] borrows from a live module, and a tool that reads a
/// recorded trace has no live module to borrow from. The entries are
/// [`Copy`], so this is the same table, detached.
#[derive(Clone, Debug)]
pub struct ModuleSchema {
    /// The name Cove source uses, such as `console`.
    pub name: String,
    /// The capability a host must grant for this module.
    pub capability: Capability,
    /// Every operation the module exposes.
    pub operations: Vec<OperationSchema>,
}

/// The schema of every host module the toolchain ships.
///
/// `cove trace` and `cove replay` read a trace without a host to ask, and
/// both need what the schema says: which calls the trace recorded are
/// irreversible, which capability each one needs, and whether a result was
/// recordable. Building the modules to ask them keeps that answer the same
/// one `cove run` enforces, rather than a second copy that can drift.
pub fn shipped_schema() -> Vec<ModuleSchema> {
    let modules: Vec<Box<dyn HostApi>> = vec![
        Box::new(Console::new(std::io::sink())),
        Box::new(Env::new(BTreeMap::new())),
        Box::new(Documents::in_memory(BTreeMap::new())),
        Box::new(crate::clock::Clock::real()),
        Box::new(crate::files::Files::in_memory(BTreeMap::new())),
        Box::new(crate::process::Process::real(Vec::new(), Vec::new())),
        Box::new(crate::database::Database::denied()),
    ];
    modules
        .iter()
        .map(|module| ModuleSchema {
            name: module.name().to_string(),
            capability: module.capability(),
            operations: module.schema().to_vec(),
        })
        .collect()
}

/// The operations `console` exposes.
///
/// Both take a variadic `String`, which is why `console.println("a", "b")`
/// prints one line of two space-separated parts. Bytes already handed to the
/// terminal cannot be taken back, so both are irreversible writes.
static CONSOLE_SCHEMA: &[OperationSchema] = &[
    OperationSchema {
        name: "println",
        params: &[HostType::String],
        variadic: true,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "console",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    },
    OperationSchema {
        name: "print",
        params: &[HostType::String],
        variadic: true,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "console",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    },
];

/// `console`: line-oriented output.
pub struct Console<W: Write + Send> {
    /// Held under a lock so that one task's line is written whole: two tasks
    /// printing at once must interleave lines, never halves of a line.
    out: Mutex<W>,
}

impl<W: Write + Send> Console<W> {
    pub fn new(out: W) -> Self {
        Console {
            out: Mutex::new(out),
        }
    }
}

impl<W: Write + Send> HostApi for Console<W> {
    fn name(&self) -> &str {
        "console"
    }

    fn capability(&self) -> Capability {
        Capability::new("console")
    }

    fn schema(&self) -> &[OperationSchema] {
        CONSOLE_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let text = args
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = match op {
            "println" => writeln!(out, "{text}"),
            "print" => write!(out, "{text}"),
            _ => unreachable!("checked by HostRegistry::call"),
        };
        match result.and_then(|_| out.flush()) {
            Ok(()) => Ok(Value::ok(Value::Unit)),
            Err(e) => Ok(Value::err(Value::error(format!("console: {e}")))),
        }
    }
}

/// The operations `env` exposes.
static ENV_SCHEMA: &[OperationSchema] = &[OperationSchema {
    name: "get",
    params: &[HostType::String],
    variadic: false,
    result: HostType::Option(&HostType::String),
    capability: "env",
    effect: Effect::Read,
    cancellable: false,
    recordable: true,
    result_is_task_safe: true,
}];

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

    fn schema(&self) -> &[OperationSchema] {
        ENV_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
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

/// The operations `documents` exposes.
static DOCUMENTS_SCHEMA: &[OperationSchema] = &[OperationSchema {
    name: "read",
    params: &[HostType::String],
    variadic: false,
    result: HostType::Result(&HostType::String, &HostType::Error),
    capability: "documents",
    effect: Effect::Read,
    cancellable: false,
    recordable: true,
    result_is_task_safe: true,
}];

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

    fn schema(&self) -> &[OperationSchema] {
        DOCUMENTS_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
        let documents = Documents::in_memory(BTreeMap::from([(
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
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("input".into())])
            .expect("no runtime error");
        assert_eq!(ok_str(read), "five little words here");
    }

    #[test]
    fn rooted_rejects_a_missing_document() {
        let dir = TempDir::new("rooted-missing");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("absent".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `absent`");
    }

    #[test]
    fn rooted_rejects_path_traversal() {
        let dir = TempDir::new("rooted-traversal");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("..".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `..`");
    }

    #[test]
    fn rooted_rejects_a_nested_path() {
        let dir = TempDir::new("rooted-nested");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("a/b".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `a/b`");
    }

    #[test]
    fn rooted_rejects_an_empty_name() {
        let dir = TempDir::new("rooted-empty");
        let documents = Documents::rooted(dir.path().to_path_buf());

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

    /// Collects every event recorded into it, for assertions.
    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<TraceEvent>>>);

    impl RecordingSink {
        fn events(&self) -> Vec<TraceEvent> {
            self.0
                .lock()
                .expect("no test panics while recording")
                .clone()
        }
    }

    impl TraceSink for RecordingSink {
        fn record(&self, event: TraceEvent) {
            self.0
                .lock()
                .expect("no test panics while recording")
                .push(event);
        }
    }

    /// What a recorded value shows as, which is the value it carried on the
    /// far side of the boundary the event crossed.
    fn shown(recorded: &RecordedValue) -> String {
        recorded_value(recorded).to_string()
    }

    /// The value a recorded value carried.
    fn recorded_value(recorded: &RecordedValue) -> Value {
        match recorded {
            RecordedValue::Carried(transfer) => transfer.clone().into_value(),
            RecordedValue::Opaque { shown, .. } => Value::Str(shown.as_str().into()),
        }
    }

    fn registry_with_documents() -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(["documents"]));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]))));
        hosts
    }

    #[test]
    fn budget_stops_a_call_before_it_dispatches() {
        use crate::budget::{Budget, Limits};

        let mut hosts = registry_with_documents();
        hosts.set_budget(Budget::new(Limits {
            max_host_calls: Some(0),
            ..Limits::default()
        }));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        let error = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be stopped by the budget");
        assert!(error.rule.is_some(), "{error:?}");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall { granted, .. } => assert!(!granted),
            other => panic!("expected a HostCall event, found {other:?}"),
        }
        assert_eq!(hosts.with_budget(|budget| budget.host_calls()), Some(1));
    }

    #[test]
    fn a_granted_call_produces_one_event_with_a_plausible_wait() {
        let mut hosts = registry_with_documents();
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                module,
                op,
                capability,
                wait,
                granted,
                args,
                outcome,
            } => {
                assert_eq!(module, "documents");
                assert_eq!(op, "read");
                assert_eq!(capability, "documents");
                assert!(*granted);
                assert!(*wait < std::time::Duration::from_secs(1), "{wait:?}");
                // A trace that says only that a call happened cannot replay
                // it, so the event carries the call's arguments and its
                // result too.
                assert_eq!(args.len(), 1);
                assert_eq!(shown(&args[0]), "input");
                match outcome {
                    Some(HostOutcome::Value(value)) => {
                        assert_eq!(ok_str(recorded_value(value)), "hello world")
                    }
                    other => panic!("expected a recorded value, found {other:?}"),
                }
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    /// The schema's `recordable` flag decides whether a result is written
    /// down, and `process.exit` is the one shipped operation it decides
    /// against: replaying it by handing back a value would keep running a
    /// program that had ended.
    #[test]
    fn a_result_is_recorded_only_when_the_schema_says_it_may_be() {
        let mut hosts = HostRegistry::new(Grants::new(["process"]));
        hosts.register(Box::new(crate::process::Process::recorded(
            vec!["one".to_string()],
            BTreeMap::new(),
            crate::process::ProcessLog::new(),
        )));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts.call("process", "args", Vec::new()).expect("granted");
        hosts
            .call("process", "exit", vec![Value::Int(2)])
            .expect("granted");

        let events = sink.events();
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            // `process.args` is recordable, so its result is recorded.
            TraceEvent::HostCall {
                outcome: Some(HostOutcome::Value(value)),
                ..
            } => assert_eq!(shown(value), "[one]"),
            other => panic!("expected a recorded value, found {other:?}"),
        }
        match &events[1] {
            TraceEvent::HostCall {
                op,
                args,
                outcome: Some(HostOutcome::NotRecordable),
                ..
            } => {
                assert_eq!(op, "exit");
                // The call itself is still recorded, arguments and all: what
                // the program asked for is exactly the part worth knowing.
                assert_eq!(args.len(), 1);
                assert_eq!(shown(&args[0]), "2");
            }
            other => panic!("expected `not recordable`, found {other:?}"),
        }
    }

    /// A sink that reads nothing is asked for nothing: describing a call's
    /// values costs a copy of each, and an untraced run — whose sink is
    /// [`NullSink`] — should not pay for a description nobody keeps.
    #[test]
    fn a_sink_that_is_not_recording_is_given_no_events() {
        /// Records every event it is given, while saying it will not read
        /// them, exactly as `NullSink` does.
        #[derive(Clone, Default)]
        struct Deaf(RecordingSink);

        impl TraceSink for Deaf {
            fn record(&self, event: TraceEvent) {
                self.0.record(event);
            }

            fn is_recording(&self) -> bool {
                false
            }
        }

        let mut hosts = registry_with_documents();
        let sink = Deaf::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");

        assert!(sink.0.events().is_empty(), "{:?}", sink.0.events());
    }

    /// A call the run was not granted never reaches a host, so there is no
    /// result to record — but there is still a request worth recording.
    #[test]
    fn a_refused_call_records_its_arguments_and_no_result() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected");

        let events = sink.events();
        match &events[0] {
            TraceEvent::HostCall {
                granted,
                args,
                outcome,
                ..
            } => {
                assert!(!granted);
                assert_eq!(args.len(), 1);
                assert!(outcome.is_none(), "{outcome:?}");
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    #[test]
    fn an_unknown_operation_lists_the_operations_that_exist() {
        let hosts = registry_with_documents();

        let error = hosts
            .call("documents", "write", Vec::new())
            .expect_err("`documents` has no `write`");
        assert_eq!(
            error.message,
            "host module `documents` has no operation `write`"
        );
        assert_eq!(error.help.as_deref(), Some("`documents` exposes `read`"));
    }

    #[test]
    fn too_many_arguments_are_rejected_before_the_host_sees_them() {
        let hosts = registry_with_documents();

        let error = hosts
            .call(
                "documents",
                "read",
                vec![Value::Str("input".into()), Value::Str("extra".into())],
            )
            .expect_err("`documents.read` takes one argument");
        assert_eq!(
            error.message,
            "`documents.read` takes 1 argument, but 2 were given"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("the Host API schema declares `documents.read(String) -> Result<String, Error>`")
        );
    }

    #[test]
    fn too_few_arguments_are_rejected_too() {
        let hosts = registry_with_documents();

        let error = hosts
            .call("documents", "read", Vec::new())
            .expect_err("`documents.read` takes one argument");
        assert_eq!(
            error.message,
            "`documents.read` takes 1 argument, but 0 were given"
        );
    }

    /// `console.println("a", "b")` is one line of two parts, so the arity
    /// check must not reject it.
    #[test]
    fn a_variadic_operation_accepts_any_number_of_arguments() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Console::new(Vec::new())));

        for arity in 0..3 {
            hosts
                .call("console", "println", vec![Value::Str("part".into()); arity])
                .unwrap_or_else(|e| panic!("arity {arity} should be accepted: {}", e.message));
        }
    }

    #[test]
    fn task_safety_of_a_host_result_comes_from_the_schema() {
        let hosts = registry_with_documents();

        assert_eq!(hosts.result_is_task_safe("documents", "read"), Some(true));
        assert_eq!(hosts.result_is_task_safe("documents", "write"), None);
        assert_eq!(hosts.result_is_task_safe("network", "read"), None);
    }

    /// The registry gates on the module's capability, and each operation
    /// declares the capability it needs. Nothing today mixes capabilities
    /// inside one module, and a module whose operations disagreed with it
    /// would make the grant check and the schema tell different stories.
    #[test]
    fn every_operation_declares_its_module_capability() {
        for module in shipped_schema() {
            for entry in &module.operations {
                assert_eq!(
                    entry.capability,
                    module.capability.as_str(),
                    "`{}.{}`",
                    module.name,
                    entry.name
                );
            }
        }
    }

    /// What `cove trace` and `cove replay` read instead of a live host.
    #[test]
    fn the_shipped_schema_names_every_module_a_run_registers() {
        let names: Vec<String> = shipped_schema().into_iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            [
                "console",
                "env",
                "documents",
                "clock",
                "files",
                "process",
                "database"
            ]
        );
    }

    /// The counter behind `cove run --stats`, and the one thing that reads
    /// an operation's declared `effect`.
    #[test]
    fn only_the_calls_the_schema_calls_irreversible_are_counted() {
        let mut hosts = HostRegistry::new(Grants::new(["console", "files"]));
        hosts.register(Box::new(Console::new(Vec::new())));
        hosts.register(Box::new(crate::files::Files::in_memory(BTreeMap::new())));
        assert_eq!(hosts.irreversible_writes(), 0);

        hosts
            .call("files", "exists", vec![Value::Str("a.txt".into())])
            .expect("the call should be allowed");
        assert_eq!(hosts.irreversible_writes(), 0);

        hosts
            .call(
                "files",
                "write",
                vec![Value::Str("a.txt".into()), Value::Str("x".into())],
            )
            .expect("the call should be allowed");
        hosts
            .call("console", "println", vec![Value::Str("a".into())])
            .expect("the call should be allowed");
        assert_eq!(hosts.irreversible_writes(), 2);
    }

    /// A call the run was not granted never reaches the host, so it never
    /// changed anything to count.
    #[test]
    fn an_ungranted_irreversible_call_is_not_counted() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(crate::files::Files::in_memory(BTreeMap::new())));

        hosts
            .call(
                "files",
                "write",
                vec![Value::Str("a.txt".into()), Value::Str("x".into())],
            )
            .expect_err("the call should be rejected");
        assert_eq!(hosts.irreversible_writes(), 0);
    }

    #[test]
    fn a_denied_call_produces_an_event_with_granted_false() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected for the missing grant");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                capability,
                granted,
                ..
            } => {
                assert_eq!(capability, "documents");
                assert!(!granted);
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }
}
