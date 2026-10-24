//! The permission boundary, reached by every route a program has to it:
//! issue [#681](https://github.com/myuon/cove/issues/681), "権限境界の検査".
//!
//! `docs/ISOLATION_TESTING.md` states the specification, and
//! [ADR 0114](../../../docs/adr/0114-what-a-failed-run-leaves-behind.md)
//! records it. The short form: **a run's authority is its registry's grant
//! set, the same set for every frame of the run.** A closure, a function
//! value, a callback a host runs, a standard-library higher-order function, a
//! spawned task and a host operation bound to a name all reach the host
//! through [`HostRegistry`]'s one check, against the grants of the registry
//! the *running* machine was built over. Nothing carries a capability with it
//! — not a closure, not a handle — and nothing a program can do widens the
//! set.
//!
//! Every case here proves a denial by its absence of effect, not by its
//! error alone: each shipped host is wrapped in a [`Spy`] that logs every
//! call that reaches the implementation, the fakes keep their state where
//! the test can read it, and a denied capability must leave the spy's log
//! empty and the fake's state as it was. Each route is first run with the
//! capability *granted*, and must reach the host there — a route that never
//! reached the host would pass every denial below without testing anything.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::files::Tree;
use cove_runtime::http::RecordedResponse;
use cove_runtime::interp::Interpreter;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{
    Budget, Clock, Console, Database, Documents, Env, Files, GrantSource, Grants, HostAnswer,
    HostApi, HostRegistry, Http, Limits, ModuleSchema, OwnedVm, PreparedProgram, Process,
    ProcessLog, Reentry, ResourceHandle, Runtime, RuntimeError, Value, VirtualTime,
};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

// ------------------------------------------------------------------ the spy

/// A host that logs every call reaching it, then hands the call on.
struct Spy {
    inner: Arc<dyn HostApi>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Spy {
    fn note(&self, op: &str) {
        let module = self.inner.module_schema().name;
        self.log.lock().unwrap().push(format!("{module}.{op}"));
    }
}

impl HostApi for Spy {
    fn module_schema(&self) -> ModuleSchema {
        self.inner.module_schema()
    }
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.note(op);
        self.inner.call(op, args)
    }
    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        self.note(op);
        self.inner.call_with(op, args, back)
    }
    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        self.note(&format!("{}.{op}", handle.type_name));
        self.inner.call_resource(handle, op, args, back)
    }
    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        self.note(op);
        self.inner.call_parkable(op, args, back)
    }
    fn call_resource_parkable(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> HostAnswer {
        self.note(&format!("{}.{op}", handle.type_name));
        self.inner.call_resource_parkable(handle, op, args, back)
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every shipped capability, and the one call a program makes to exercise it.
struct Capability {
    name: &'static str,
    /// The operation as a value: `files.write`.
    op: &'static str,
    /// Its arguments, as Cove source.
    args: &'static str,
}

const CAPABILITIES: &[Capability] = &[
    Capability {
        name: "console",
        op: "console.println",
        args: "\"pwned\"",
    },
    Capability {
        name: "env",
        op: "env.get",
        args: "\"SECRET\"",
    },
    Capability {
        name: "documents",
        op: "documents.read",
        args: "\"secret\"",
    },
    Capability {
        name: "clock",
        op: "clock.now",
        args: "",
    },
    Capability {
        name: "files",
        op: "files.write",
        args: "\"planted.txt\", \"pwned\"",
    },
    Capability {
        name: "process",
        op: "process.run",
        args: "\"/bin/echo\", [\"pwned\"]",
    },
    Capability {
        name: "database",
        op: "database.query",
        args: "\"delete from bookings\"",
    },
    Capability {
        name: "http",
        op: "http.fetch",
        args: "\"http://127.0.0.1:9/pwned\"",
    },
];

/// Every route from a program to a host call. `{E}` is the call, `{OP}` the
/// operation as a value, `{A}` its arguments and `{C}` the capability's name,
/// which makes each function's name its own.
const ROUTES: &[(&str, &str)] = &[
    (
        "Direct",
        "export fn {C}Direct() -> Int {
  let r = {E}
  0
}",
    ),
    (
        "Closure",
        "fn {C}Make() -> fn() -> Int {
  fn() {
    let r = {E}
    1
  }
}

export fn {C}Closure() -> Int {
  let later = {C}Make()
  later()
}",
    ),
    (
        "Helper",
        "fn {C}Act() -> Int {
  let r = {E}
  1
}

export fn {C}Helper() -> Int {
  twice({C}Act)
}",
    ),
    (
        "Map",
        "export fn {C}Map() -> Int {
  [1, 2].map(fn(x) {
    let r = {E}
    x
  }).length()
}",
    ),
    (
        "Filter",
        "export fn {C}Filter() -> Int {
  Vector.of(1, 2).toArray().filter(fn(x) {
    let r = {E}
    true
  }).length()
}",
    ),
    (
        "Task",
        "export fn {C}Task() -> Int {
  var total = 0
  scope work {
    let child = work.spawn {
      {C}Act()
    }
    total = await child
  }
  total
}",
    ),
    (
        "HostValue",
        "export fn {C}HostValue() -> Int {
  let operation = {OP}
  let r = operation({A})
  0
}",
    ),
    (
        "Callback",
        "export fn {C}Callback() -> Int {
  let r = clock.timeout(1s, fn() {
    {C}Act()
  })
  0
}",
    ),
];

fn source() -> String {
    let mut text = String::from(
        "use clock
use console
use database
use documents
use env
use files
use http
use process

/// Calls `f` twice.
fn twice(f: fn() -> Int) -> Int {
  f() + f()
}

/// Calls whatever it is handed: capability-open, so the static analysis
/// cannot say what it requires (ADR 0015).
export fn apply(f: fn() -> Int) -> Int {
  f()
}

/// A closure that writes a file when called and does nothing when made.
export fn writer() -> fn() -> Int {
  fn() {
    let r = files.write(\"planted.txt\", \"pwned\")
    1
  }
}

/// A reader over a file, for a host to keep.
export fn openReader() -> Result<files.Reader, Error> {
  files.open(\"notes.txt\")
}

/// Reads a line through a reader it is handed.
export fn readWith(reader: files.Reader) -> Result<Option<String>, Error> {
  reader.readLine()
}
",
    );
    for capability in CAPABILITIES {
        let call = format!("{}({})", capability.op, capability.args);
        for (route, body) in ROUTES {
            if *route == "Callback" && capability.name == "clock" {
                continue;
            }
            text.push('\n');
            text.push_str(
                &body
                    .replace("{E}", &call)
                    .replace("{OP}", capability.op)
                    .replace("{A}", capability.args)
                    .replace("{C}", capability.name),
            );
            text.push('\n');
        }
    }
    text
}

// ----------------------------------------------------------------- the world

/// One registry, with its spies and fakes.
struct Hosts {
    registry: Arc<HostRegistry>,
    log: Arc<Mutex<Vec<String>>>,
    files: Tree,
    processes: ProcessLog,
    console: Sink,
}

impl Hosts {
    /// Every shipped host, faked and spied on, granting `grants`.
    fn new(grants: &[&str], source: GrantSource) -> Hosts {
        let log = Arc::new(Mutex::new(Vec::new()));
        let console = Sink::default();
        let files = Arc::new(Files::in_memory(BTreeMap::from([(
            "notes.txt".to_string(),
            "the first line\nthe second\n".to_string(),
        )])));
        let processes = ProcessLog::new();
        let tree = files.tree();
        let modules: Vec<Arc<dyn HostApi>> = vec![
            Arc::new(Console::new(console.clone(), console.clone())),
            Arc::new(Env::new(BTreeMap::from([(
                "SECRET".to_string(),
                "hunter2".to_string(),
            )]))),
            Arc::new(Documents::in_memory(BTreeMap::from([(
                "secret".to_string(),
                "classified".to_string(),
            )]))),
            Arc::new(Clock::virtual_clock(VirtualTime::new())),
            files,
            Arc::new(Process::recorded(
                Vec::new(),
                BTreeMap::new(),
                processes.clone(),
            )),
            Arc::new(Database::recorded(BTreeMap::new())),
            Arc::new(Http::recorded(
                BTreeMap::from([(
                    "http://127.0.0.1:9/pwned".to_string(),
                    RecordedResponse::ok("fetched"),
                )]),
                Vec::new(),
            )),
        ];
        let mut registry = HostRegistry::new(Grants::new(grants.iter().copied()));
        registry.set_grant_source(source);
        for inner in modules {
            registry.register(Box::new(Spy {
                inner,
                log: Arc::clone(&log),
            }));
        }
        Hosts {
            registry: Arc::new(registry),
            log,
            files: tree,
            processes,
            console,
        }
    }

    /// Every shipped capability but `denied`.
    fn all_but(denied: &str) -> Hosts {
        let grants: Vec<&str> = CAPABILITIES
            .iter()
            .map(|c| c.name)
            .filter(|name| *name != denied)
            .collect();
        Hosts::new(&grants, GrantSource::Config)
    }

    fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn calls_to(&self, module: &str) -> Vec<String> {
        let prefix = format!("{module}.");
        self.calls()
            .into_iter()
            .filter(|call| call.starts_with(&prefix))
            .collect()
    }

    /// The fakes' state, as a denial must leave it.
    fn untouched(&self) -> bool {
        self.files
            .files()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            == ["notes.txt"]
            && self.processes.runs().is_empty()
            && self.console.0.lock().unwrap().is_empty()
    }
}

/// The program, checked and lowered once.
struct Compiled {
    sources: Arc<SourceMap>,
    program: Arc<Program>,
    encoded: PreparedProgram,
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    native: PreparedProgram,
}

fn compiled() -> &'static Compiled {
    static COMPILED: std::sync::OnceLock<Compiled> = std::sync::OnceLock::new();
    COMPILED.get_or_init(|| {
        let hosts = Hosts::new(&[], GrantSource::Config);
        let schemas = HostSchemas::only(hosts.registry.module_schemas());
        let (sources, package) = packaged(&source());
        let render = |items: Vec<cove_diag::Diagnostic>| {
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<String>()
        };
        let checked = Compiler::new()
            .with_schemas(schemas.clone())
            .compile(&package)
            .unwrap_or_else(|items| panic!("the fixture checks:\n{}", render(items)));
        // Every exported function but the host-operation values, which the
        // linear-memory lowering does not take yet ("not yet lowered: a name
        // reached through a module"): that route runs on the interpreter.
        let names: Vec<String> = CAPABILITIES
            .iter()
            .flat_map(routes)
            .filter(|name| !name.ends_with("HostValue"))
            .chain(["apply", "writer", "openReader", "readWith"].map(String::from))
            .collect();
        let roots: Vec<(&str, &str)> = names.iter().map(|name| ("app", name.as_str())).collect();
        let lowered = cove_ir::lower_roots(&checked, &sources, &schemas, &roots)
            .unwrap_or_else(|items| panic!("the fixture lowers:\n{}", render(items)));
        let encoded = PreparedProgram::new(Arc::new(lowered));
        Compiled {
            sources: Arc::new(sources),
            program: Arc::new(checked),
            #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
            native: encoded
                .clone()
                .with_native()
                .expect("this host compiles native code"),
            encoded,
        }
    })
}

fn packaged(text: &str) -> (SourceMap, Package) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let file = sources.add(path.clone(), text);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::new();
    modules.insert(
        "app".to_string(),
        Module {
            name: "app".to_string(),
            dir: PathBuf::from("app"),
            units: vec![Unit { file, path, ast }],
        },
    );
    cove_sema::stdlib::install(&mut sources, &mut modules).expect("the standard library parses");
    (
        sources,
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        },
    )
}

/// The three ways to run the program.
#[derive(Clone, Copy, Debug)]
enum Backend {
    Encoded,
    #[cfg_attr(
        not(all(feature = "template", target_arch = "x86_64", unix)),
        allow(dead_code)
    )]
    Native,
    Ast,
}

fn runtime(hosts: &Hosts) -> Arc<Runtime> {
    let compiled = compiled();
    Arc::new(Runtime::new(
        Arc::clone(&compiled.program),
        Arc::clone(&compiled.sources),
        Arc::clone(&hosts.registry),
    ))
}

fn machine(hosts: &Hosts, backend: Backend) -> OwnedVm {
    let compiled = compiled();
    let prepared = match backend {
        Backend::Encoded => compiled.encoded.clone(),
        #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
        Backend::Native => compiled.native.clone(),
        #[cfg(not(all(feature = "template", target_arch = "x86_64", unix)))]
        Backend::Native => unreachable!("gated on the tier"),
        Backend::Ast => unreachable!("the interpreter is not a machine"),
    };
    OwnedVm::new(runtime(hosts), Arc::clone(&hosts.registry), prepared)
}

/// `app.name()` on `backend` over `hosts`.
fn run(
    hosts: &Hosts,
    backend: Backend,
    name: &str,
    args: Vec<Value>,
) -> Result<Value, RuntimeError> {
    let budget = || Budget::new(Limits::default());
    match backend {
        Backend::Ast => {
            let runtime = runtime(hosts);
            let mut interpreter = Interpreter::new(&runtime);
            interpreter.invoke_within(budget(), "app", name, args)
        }
        _ => machine(hosts, backend).invoke_within(budget(), "app", name, args),
    }
}

fn on_stack(body: impl FnOnce() + Send) {
    cove_runtime::on_cove_stack(body).expect("a thread to run Cove on");
}

/// The functions that reach `capability`, one per route: every route but a
/// callback through `clock` when `clock` is the capability, since the
/// callback's host is the one denied.
fn routes(capability: &Capability) -> impl Iterator<Item = String> + '_ {
    ROUTES
        .iter()
        .filter(move |(route, _)| !(*route == "Callback" && capability.name == "clock"))
        .map(move |(route, _)| format!("{}{route}", capability.name))
}

/// [`routes`], less what `backend` cannot run: the linear-memory lowering
/// does not take a host operation as a value yet.
fn routes_on(capability: &Capability, backend: Backend) -> Vec<String> {
    routes(capability)
        .filter(|name| matches!(backend, Backend::Ast) || !name.ends_with("HostValue"))
        .collect()
}

fn described(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("Ok({value})"),
        Err(error) => format!(
            "Err({:?}, denied {:?}): {}",
            error.outcome, error.denied_capability, error.message
        ),
    }
}

// ------------------------------------------------------------------- denial

fn every_route_to_an_ungranted_capability_is_refused_before_the_host(backend: Backend) {
    on_stack(|| {
        let mut failures = Vec::new();
        let mut checked = 0;
        for capability in CAPABILITIES {
            for function in routes_on(capability, backend) {
                // The control: granted, the route reaches the host.
                let granted = Hosts::new(
                    &CAPABILITIES.iter().map(|c| c.name).collect::<Vec<_>>(),
                    GrantSource::Config,
                );
                let answer = run(&granted, backend, &function, Vec::new());
                if answer.is_err() || granted.calls_to(capability.name).is_empty() {
                    failures.push(format!(
                        "{backend:?} {function} granted: {} reached {:?}",
                        described(&answer),
                        granted.calls()
                    ));
                    continue;
                }

                // Denied: refused at the boundary, named, and with no effect.
                let denied = Hosts::all_but(capability.name);
                let answer = run(&denied, backend, &function, Vec::new());
                let refused = matches!(
                    &answer,
                    Err(error)
                        if error.outcome == RunOutcome::HostBoundary
                            && error.denied_capability.as_deref() == Some(capability.name)
                );
                let reached = denied.calls_to(capability.name);
                if !refused || !reached.is_empty() || !denied.untouched() {
                    failures.push(format!(
                        "{backend:?} {function} denied: {}; the host saw {reached:?}; fakes untouched: {}",
                        described(&answer),
                        denied.untouched()
                    ));
                }
                checked += 1;
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        // Eight capabilities by eight routes, less `clock`'s own callback and,
        // off the interpreter, the eight host-operation values.
        let expected = match backend {
            Backend::Ast => 63,
            _ => 55,
        };
        assert_eq!(checked, expected, "{backend:?}: every route was run");
    });
}

#[test]
fn every_route_to_an_ungranted_capability_is_refused_before_the_host_encoded() {
    every_route_to_an_ungranted_capability_is_refused_before_the_host(Backend::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn every_route_to_an_ungranted_capability_is_refused_before_the_host_native() {
    every_route_to_an_ungranted_capability_is_refused_before_the_host(Backend::Native);
}

#[test]
fn every_route_to_an_ungranted_capability_is_refused_before_the_host_ast() {
    every_route_to_an_ungranted_capability_is_refused_before_the_host(Backend::Ast);
}

// --------------------------------------------- values that cross between runs

/// A closure made by a run that may not write files, and called by a later
/// run of the same machine, is refused at the write: making a closure asks
/// for nothing, and calling it asks the registry the running machine was
/// built over. The entry that calls it is capability-open, so no static
/// check could have refused it first (ADR 0015) — the runtime's grant check
/// is the only thing that decides.
#[test]
fn a_closure_kept_from_an_earlier_run_is_checked_when_it_is_called() {
    on_stack(|| {
        let hosts = Hosts::all_but("files");
        let mut vm = machine(&hosts, Backend::Encoded);
        let budget = || Budget::new(Limits::default());
        let closure = vm
            .invoke_within(budget(), "app", "writer", Vec::new())
            .expect("making the closure asks for nothing");
        assert!(hosts.calls().is_empty());
        let answer = vm.invoke_within(budget(), "app", "apply", vec![closure]);
        let error = answer.expect_err("the write is refused");
        assert_eq!(error.denied_capability.as_deref(), Some("files"));
        assert!(hosts.calls().is_empty(), "{:?}", hosts.calls());
        assert!(hosts.untouched());

        let entry = compiled().program.lookup_fn("app", "apply").unwrap();
        assert!(
            entry.is_capability_open(),
            "no static check could have seen it"
        );
    });
}

/// A closure made by a machine whose registry grants `files` is refused by
/// a machine whose registry does not, before it is called: its environment
/// is the other machine's heap. It does not carry its maker's authority
/// across, and neither registry's host sees a call.
#[test]
fn a_closure_does_not_carry_its_maker_s_grants_to_another_machine() {
    on_stack(|| {
        let granted = Hosts::new(&["files"], GrantSource::Config);
        let denied = Hosts::new(&[], GrantSource::Config);
        let closure = machine(&granted, Backend::Encoded)
            .invoke_within(Budget::new(Limits::default()), "app", "writer", Vec::new())
            .expect("a closure crosses out");
        let answer = machine(&denied, Backend::Encoded).invoke_within(
            Budget::new(Limits::default()),
            "app",
            "apply",
            vec![closure],
        );
        let error = answer.expect_err("another machine refuses it");
        assert!(error.message.contains("another run"), "{}", error.message);
        assert!(granted.calls().is_empty() && denied.calls().is_empty());
        assert!(granted.untouched() && denied.untouched());
    });
}

/// A handle is a name, not a capability. A `files.Reader` opened by a run
/// that may read files, handed by its embedder to a run over a registry that
/// does not grant `files`, is refused at its first operation — on both
/// tiers that can take one — and the second registry's host never sees it.
#[test]
fn a_handle_handed_to_a_run_without_the_grant_is_refused_at_its_first_use() {
    on_stack(|| {
        for backend in [Backend::Encoded, Backend::Ast] {
            let granted = Hosts::new(&["files"], GrantSource::Config);
            let opened =
                run(&granted, backend, "openReader", Vec::new()).expect("the reader opens");
            let reader = opened.ok_payload().expect("an `Ok`")[0].clone();
            assert!(reader.resource().is_some(), "{reader}");
            let denied = Hosts::new(&[], GrantSource::Config);
            let answer = run(&denied, backend, "readWith", vec![reader.clone()]);
            let error = answer.expect_err("the read is refused");
            assert_eq!(
                error.denied_capability.as_deref(),
                Some("files"),
                "{backend:?}"
            );
            assert!(
                denied.calls().is_empty(),
                "{backend:?}: {:?}",
                denied.calls()
            );

            // In its own registry it still works: the refusal was the
            // other registry's grants, not damage to the handle.
            let answer = run(&granted, backend, "readWith", vec![reader]);
            assert_eq!(
                described(&answer),
                "Ok(Ok(Some(the first line)))",
                "{backend:?}"
            );
        }
    });
}

// ------------------------------------------------------- nothing widens them

/// Where the grants came from changes the advice and nothing else: a sealed
/// binary's registry refuses exactly what a `cove.toml` one refuses, and the
/// help it gives names rebuilding rather than editing a file it never reads.
#[test]
fn a_sealed_grant_set_refuses_exactly_what_a_configured_one_does() {
    on_stack(|| {
        for capability in CAPABILITIES {
            let function = format!("{}Direct", capability.name);
            let mut answers = Vec::new();
            for source in [GrantSource::Config, GrantSource::Sealed] {
                let grants: Vec<&str> = CAPABILITIES
                    .iter()
                    .map(|c| c.name)
                    .filter(|name| *name != capability.name)
                    .collect();
                let hosts = Hosts::new(&grants, source);
                let error =
                    run(&hosts, Backend::Encoded, &function, Vec::new()).expect_err("refused");
                assert!(hosts.calls_to(capability.name).is_empty());
                answers.push(error);
            }
            let [config, sealed] = &answers[..] else {
                unreachable!()
            };
            assert_eq!(config.message, sealed.message);
            assert_eq!(config.denied_capability, sealed.denied_capability);
            assert_eq!(config.outcome, sealed.outcome);
            assert!(sealed.help.as_deref().unwrap().contains("build it again"));
            assert!(!config.help.as_deref().unwrap().contains("build it again"));
        }
    });
}

/// A registry's grants are what it was built with, after any number of runs
/// over it — refused, failed, or answered — and a refusal in one run is not
/// remembered in the next: the same registry grants the next run exactly
/// what it granted the first, no more and no less.
#[test]
fn a_registry_s_grants_are_the_same_for_every_run_over_it() {
    on_stack(|| {
        let hosts = Hosts::all_but("files");
        let before: Vec<String> = hosts
            .registry
            .grants()
            .iter()
            .map(|c| c.to_string())
            .collect();
        let mut vm = machine(&hosts, Backend::Encoded);
        for _ in 0..3 {
            for function in ["filesDirect", "filesTask", "envDirect", "filesCallback"] {
                let answer =
                    vm.invoke_within(Budget::new(Limits::default()), "app", function, Vec::new());
                match function {
                    "envDirect" => assert!(answer.is_ok(), "{}", described(&answer)),
                    _ => assert_eq!(
                        answer.expect_err(function).denied_capability.as_deref(),
                        Some("files")
                    ),
                }
            }
        }
        let after: Vec<String> = hosts
            .registry
            .grants()
            .iter()
            .map(|c| c.to_string())
            .collect();
        assert_eq!(before, after);
        assert!(hosts.calls_to("files").is_empty());
        assert_eq!(hosts.calls_to("env").len(), 3);
    });
}
