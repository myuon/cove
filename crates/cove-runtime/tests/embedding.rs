//! The minimal embedding acceptance test ADR 0001 asks for.
//!
//! Embedded execution is only `MVP required` if a host outside this crate
//! can actually load Cove source, supply its own [`HostApi`] implementation
//! for a capability, impose its own [`Limits`], and see the difference
//! between a granted call and a refused one. This file proves exactly that,
//! using only the public surface `cove-runtime` exports: [`HostApi`],
//! [`HostRegistry`], [`Grants`], [`Budget`], [`Limits`], and [`Runtime`],
//! plus [`cove_runtime::interp::Interpreter`].
//!
//! A host module's *name* and the shape of its operations are the compiler's:
//! `cove_sema` resolves `documents` as a host module and checks
//! `read("welcome")` against `cove_schema`'s description of it. Its
//! *implementation* is not. `cove run` and `cove build` always wire up
//! [`cove_runtime::Documents`]; this test wires up a host-owned
//! implementation instead, to show that the boundary an embedding host
//! programs against is the trait, not any one shipped struct. It touches no
//! network and no real filesystem: the package and the documents it serves
//! are held in memory.
//!
//! A host that takes a shipped module's name takes its description with it,
//! which is what lets the compiler check a program written against it. A host
//! that registers a module of its own is one the compiler has never heard of
//! and checks nothing about -- and is exactly why the boundary checks a
//! call's arguments as well: the schema this file declares below is enforced
//! against every call made through it, whether or not any compiler read it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::{
    Budget, Effect, Grants, HostApi, HostRegistry, HostType, Limits, OperationSchema, Runtime,
    RuntimeError, Value,
};
use cove_sema::resolve::Program;
use cove_sema::{Capability, Config, Module, Package, Unit};

/// A `documents` implementation a host defines for itself: a fixed set of
/// named notes and a log of every read the host observed. Nothing here comes
/// from `cove_runtime::host` -- an embedding host does not get to reuse the
/// shipped `Documents`, it writes this.
struct HostOwnedDocuments {
    notes: BTreeMap<&'static str, &'static str>,
    reads: Arc<Mutex<Vec<String>>>,
}

/// The one operation this test host exposes, described the same way a
/// shipped host describes itself.
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

impl HostApi for HostOwnedDocuments {
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
        assert_eq!(op, "read", "the only operation this test host registers");
        // The registry checked the call against the schema above before
        // dispatching it, so an embedding host restates none of it: the
        // operation exists, there is one argument, and it is a `String`.
        let [Value::Str(name)] = args.as_slice() else {
            unreachable!("checked by HostRegistry::call")
        };
        self.reads.lock().unwrap().push(name.to_string());
        Ok(match self.notes.get(&**name) {
            Some(text) => Value::ok(Value::Str((*text).into())),
            None => Value::err(Value::error(format!("no document named `{name}`"))),
        })
    }
}

/// Parses and resolves the one-module package every case below runs,
/// entirely in memory: embedding Cove needs no directory on disk, so proving
/// it does not either.
fn program() -> (Arc<SourceMap>, Arc<Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let text = "\
use documents.read

/// Reads a note through the embedding host's own `documents`
/// implementation.
export fn main() -> Result<String, Error> {
  read(\"welcome\")
}
";
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
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    let program = cove_sema::resolve::resolve(&package).expect("the fixture resolves");
    (Arc::new(sources), Arc::new(program))
}

/// Runs `app.main` through a host-owned `documents` implementation, granting
/// `grants` and imposing `limits` -- exactly what a Rust host embedding Cove
/// would write.
fn run(
    grants: &[&str],
    limits: Limits,
    reads: Arc<Mutex<Vec<String>>>,
) -> Result<Value, RuntimeError> {
    let (sources, program) = program();
    let mut hosts = HostRegistry::new(Grants::new(grants.to_vec()));
    hosts.register(Box::new(HostOwnedDocuments {
        notes: BTreeMap::from([("welcome", "hello from a host-owned capability")]),
        reads,
    }));
    hosts.set_budget(Budget::new(limits));
    let runtime = Runtime::new(program, sources, Arc::new(hosts));
    Interpreter::new(&runtime).run_entry("app", "main", Vec::new())
}

/// The success half of the acceptance test: granted and within limits, the
/// host's own implementation runs and its result reaches back out to Rust.
#[test]
fn a_host_owned_capability_runs_when_granted_and_within_limits() {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let value = run(
        &["documents"],
        Limits {
            max_host_calls: Some(10),
            ..Limits::default()
        },
        reads.clone(),
    )
    .expect("a granted call within limits must succeed");

    match value {
        Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Ok" => {
            assert_eq!(
                result.payload[0].to_string(),
                "hello from a host-owned capability"
            );
        }
        other => panic!("expected `Ok(String)`, found {other}"),
    }
    assert_eq!(*reads.lock().unwrap(), vec!["welcome".to_string()]);
}

/// The denial half: no ambient authority means an ungranted capability is
/// refused before the host-owned implementation is ever called.
#[test]
fn a_host_owned_capability_is_refused_when_not_granted() {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let error =
        run(&[], Limits::default(), reads.clone()).expect_err("an ungranted call must be refused");

    assert!(
        error
            .message
            .contains("requires the `documents` capability, which this run was not granted"),
        "{}",
        error.message
    );
    assert!(
        reads.lock().unwrap().is_empty(),
        "a refused call must never reach the host's own implementation"
    );
}

/// A second denial path: the host's own limits stop a granted call too, so
/// granting a capability is not the same as trusting a run to call it freely.
#[test]
fn a_host_s_own_limit_stops_a_granted_call() {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let error = run(
        &["documents"],
        Limits {
            max_host_calls: Some(0),
            ..Limits::default()
        },
        reads,
    )
    .expect_err("a call over the host's own limit must be refused");

    assert!(
        error.message.contains("host-call limit of 0 exceeded"),
        "{}",
        error.message
    );
}

/// The boundary holds a call to the schema the *host* declared, not to one
/// the toolchain knows: this host's `documents` is its own, and a call
/// carrying an `Int` where its schema says `String` is refused before the
/// implementation above is reached.
///
/// This is the half of the argument check `cove check` cannot make. The
/// compiler reads the description of the modules the toolchain ships, and an
/// embedding may register anything; the boundary reads the description the
/// module registered with.
#[test]
fn an_argument_the_host_s_own_schema_does_not_admit_is_refused() {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let mut hosts = HostRegistry::new(Grants::new(vec!["documents"]));
    hosts.register(Box::new(HostOwnedDocuments {
        notes: BTreeMap::new(),
        reads: Arc::clone(&reads),
    }));

    let error = hosts
        .call("documents", "read", vec![Value::Int(3)])
        .expect_err("an `Int` where the host declared a `String` is refused");

    assert_eq!(
        error.message,
        "`documents.read` was given `Int` as argument 1, but its schema declares `String` there"
    );
    assert!(
        reads.lock().unwrap().is_empty(),
        "a call the boundary refused must never reach the host's own implementation"
    );
}
