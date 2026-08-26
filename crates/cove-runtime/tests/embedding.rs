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
//! One thing a host supplies that is not a limit is the stack. The runtime
//! sizes every thread it creates for Cove, because its call depth limit is a
//! promise about the native stack and a promise like that needs a known
//! stack underneath it; a thread the host created is the one thread the
//! runtime cannot size. So an embedding runs Cove inside
//! [`cove_runtime::on_cove_stack`], which is one line and is the last case
//! below.
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
//! which is what lets the compiler check a program written against it. A
//! module of the embedder's own used to be one the compiler had never heard
//! of, and the boundary was the first and only thing to look at a call into
//! it. It need not be: `HostApi::module_schema` is one `ModuleSchema`, and
//! `cove_sema::Compiler` takes that same value, so the second half of this
//! file registers a `company` module and checks a program against the very
//! table the registry will hold it to. Neither description is restated, so
//! neither can drift.
//!
//! The boundary still checks, because a host may register a module the
//! checker was never handed -- the last case below is one -- and because a
//! host is held to its schema whether or not any compiler read it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::{render, Diagnostic, SourceMap};
use cove_runtime::interp::Interpreter;
use cove_runtime::value::StructValue;
use cove_runtime::{
    Budget, Effect, FieldSchema, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, Runtime, RuntimeError, TypeSchema, Value,
};
use cove_sema::resolve::Program;
use cove_sema::{Capability, Compiler, Config, HostSchemas, Module, Package, Unit};

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
const DOCUMENTS_SCHEMA: ModuleSchema = ModuleSchema {
    name: "documents",
    capability: "documents",
    operations: &[OperationSchema {
        name: "read",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::String, &HostType::Error),
        capability: "documents",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

impl HostApi for HostOwnedDocuments {
    fn module_schema(&self) -> ModuleSchema {
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
    program_of(
        "\
use documents.read

/// Reads a note through the embedding host's own `documents`
/// implementation.
export fn main() -> Result<String, Error> {
  read(\"welcome\")
}
",
    )
}

/// Parses a one-module package written inline, entirely in memory.
fn package_of(text: &str) -> (SourceMap, Package) {
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
    (
        sources,
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        },
    )
}

/// Parses and resolves a one-module package written inline.
fn program_of(text: &str) -> (Arc<SourceMap>, Arc<Program>) {
    let (sources, package) = package_of(text);
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

/// The stack a run recurses on is the last thing an embedding has to supply,
/// and this is the one line that supplies it.
///
/// The runtime's depth limit is a promise that a recursive program stops with
/// an error rather than exhausting the native stack, and the runtime keeps it
/// by sizing every thread it creates: a spawned task's, and the one every
/// `cove` command runs on. A thread a host created is the one it cannot size.
/// So a host runs Cove inside `on_cove_stack`, as here, or gives a thread of
/// its own `.stack_size(cove_runtime::STACK_SIZE)` and builds the interpreter
/// inside it. On a smaller stack a deep enough program ends the process
/// instead, which is a failure a host cannot catch or report.
///
/// The whole run happens inside the closure because nothing Cove-shaped can
/// leave it: a `Value` is `Rc`-based and is not `Send`. Only the message comes
/// back.
#[test]
fn an_embedding_runs_cove_on_a_stack_the_runtime_sized() {
    let text = "\
fn nest(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    nest(n - 1) + 1
  }
}

/// Recurses far past whatever the runtime's call depth limit is.
export fn main() -> Result<Unit, Error> {
  let answer = nest(1000)
  Ok(())
}
";

    let message = cove_runtime::on_cove_stack(|| {
        let (sources, program) = program_of(text);
        let hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
        let runtime = Runtime::new(program, sources, Arc::new(hosts));
        Interpreter::new(&runtime)
            .run_entry("app", "main", Vec::new())
            .expect_err("recursion past the depth limit stops the run")
            .message
    })
    .expect("a thread to run Cove on");

    assert!(
        message.starts_with("call depth limit of"),
        "the run was stopped by the depth limit rather than by the stack: {message}"
    );
}

// ---------------------------------------------------------------------------
// A module of the embedder's own, checked before it is run.
//
// Everything above registers `documents`, whose name and description the
// toolchain ships. `company` below is nobody's but this file's: no `cove`
// command has heard of it, and until the compiler is handed its table,
// nothing checks a call into it before the boundary does.

/// What `company` declares about itself: one operation, one type of its own,
/// and a capability that is not its own name.
///
/// This is the only description of the module in this file. It is what
/// `Directory` registers with, and it is what the compiler is handed, so
/// there is no second copy to keep in step.
const COMPANY: ModuleSchema = ModuleSchema {
    name: "company",
    capability: "directory",
    operations: &[OperationSchema {
        name: "employee",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::Named("company.Employee"), &HostType::Error),
        capability: "directory",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[TypeSchema {
        name: "Employee",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "name",
                ty: HostType::String,
            },
            FieldSchema {
                name: "seniority",
                ty: HostType::Int,
            },
        ],
    }],
    resources: &[],
};

/// The embedder's own host module: a staff directory it keeps in memory.
struct Directory {
    people: BTreeMap<&'static str, i64>,
    /// Every lookup that reached the implementation, so a test can tell a
    /// call the checker stopped from one that ran.
    lookups: Arc<Mutex<Vec<String>>>,
}

impl HostApi for Directory {
    fn module_schema(&self) -> ModuleSchema {
        COMPANY
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        assert_eq!(op, "employee", "the only operation `company` declares");
        let [Value::Str(id)] = args.as_slice() else {
            unreachable!("checked by HostRegistry::call")
        };
        self.lookups.lock().unwrap().push(id.to_string());
        Ok(match self.people.get(&**id) {
            Some(seniority) => Value::ok(Value::Struct(Rc::new(StructValue {
                type_name: "company.Employee".into(),
                fields: vec![
                    ("name".into(), Value::Str(id.clone())),
                    ("seniority".into(), Value::Int(*seniority)),
                ],
                opaque: false,
            }))),
            None => Value::err(Value::error(format!("no employee named `{id}`"))),
        })
    }
}

/// Registers `company` and grants it, which is the whole of what an
/// embedding sets up.
fn directory(lookups: Arc<Mutex<Vec<String>>>) -> HostRegistry {
    // The capability granted is the one the schema declares, `directory`,
    // and not the module's name.
    let mut hosts = HostRegistry::new(Grants::new(vec!["directory"]));
    hosts.register(Box::new(Directory {
        people: BTreeMap::from([("ada", 7)]),
        lookups,
    }));
    hosts
}

/// A program written against `company`, in every way well typed.
const SENIORITY: &str = "\
use company

/// Reports how senior one employee is.
export fn main() -> Result<Int, Error> {
  let found = company.employee(\"ada\")?
  Ok(found.seniority)
}
";

/// The pairing, in one line: the compiler is handed the tables the registry
/// was registered with, so the program is checked against the descriptions
/// the run is about to enforce.
///
/// Nothing about `company` is written twice. `HostRegistry::module_schemas`
/// hands back what each registered module declared, and
/// `Compiler::with_host_schemas` takes it as it is.
fn compiled(hosts: &HostRegistry, text: &str) -> (SourceMap, Result<Program, Vec<Diagnostic>>) {
    let (sources, package) = package_of(text);
    let checked = Compiler::new()
        .with_host_schemas(hosts.module_schemas())
        .compile(&package);
    (sources, checked)
}

/// The well-typed half: a program calling a module no toolchain ships is
/// checked at its call sites, derives the capability the schema declares,
/// and then runs.
#[test]
fn a_registered_host_s_own_schema_checks_the_program_that_calls_it() {
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let hosts = directory(Arc::clone(&lookups));
    let (sources, checked) = compiled(&hosts, SENIORITY);
    let program = checked.expect("a well-typed program against a supplied schema checks");
    assert!(
        program.notices.is_empty(),
        "a module the checker was handed warns about nothing"
    );

    // The capability derived from the call is the schema's, not the module
    // name: `company` is gated on `directory`.
    assert_eq!(
        program.modules["app"].functions["main"].required_capabilities,
        [Capability::new("directory")].into_iter().collect()
    );

    let value = Interpreter::new(&Runtime::new(
        Arc::new(program),
        Arc::new(sources),
        Arc::new(hosts),
    ))
    .run_entry("app", "main", Vec::new())
    .expect("the checked program runs");

    assert_eq!(value.to_string(), "Ok(7)");
    assert_eq!(*lookups.lock().unwrap(), vec!["ada".to_string()]);
}

/// The other half, and the point of the whole exercise: a mistake in a call
/// into the embedder's own module is an error at its call site, reported
/// before anything runs, exactly as it would be for `http.fetch`.
#[test]
fn a_mistake_in_a_call_into_a_registered_host_is_a_static_error() {
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let hosts = directory(Arc::clone(&lookups));
    let (sources, checked) = compiled(
        &hosts,
        "\
use company

/// Passes an `Int` where the schema declares a `String`, and reads a field
/// `company.Employee` does not carry.
export fn main() -> Result<Int, Error> {
  let found = company.employee(3)?
  Ok(found.tenure)
}
",
    );
    let items = checked.expect_err("a call the schema does not admit is an error");
    let rendered: String = items.iter().map(|item| render(&sources, item)).collect();

    assert!(
        rendered.contains("expected `String`, found `Int`")
            && rendered.contains("argument `#1` is `String`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("`company.Employee` has no field `tenure`")
            && rendered.contains("declares `name`, `seniority`"),
        "{rendered}"
    );
    assert!(
        lookups.lock().unwrap().is_empty(),
        "a program the checker refused never reaches the host"
    );
}

/// A host the checker was not handed keeps the fallback it always had: the
/// call is unchecked until the boundary, the run still works, and the
/// program is told so rather than left to assume it checked.
#[test]
fn a_host_the_checker_was_not_handed_is_left_to_the_boundary_with_a_warning() {
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let hosts = directory(Arc::clone(&lookups));
    let (sources, package) = package_of(SENIORITY);
    let program = Compiler::new()
        .compile(&package)
        .expect("a module the checker cannot see is not an error");

    let rendered: String = program
        .notices
        .iter()
        .map(|item| render(&sources, item))
        .collect();
    assert!(
        rendered.contains("no Host API schema describes the host module `company`"),
        "{rendered}"
    );

    let value = Interpreter::new(&Runtime::new(
        Arc::new(program),
        Arc::new(sources),
        Arc::new(hosts),
    ))
    .run_entry("app", "main", Vec::new())
    .expect("the boundary checks what the checker could not");
    assert_eq!(value.to_string(), "Ok(7)");
}

/// An embedding whose registry is its own says so, and then a shipped module
/// it did not register is as unknown to the checker as any other name.
///
/// This is the other half of pairing a checker with a registry.
/// `with_host_schemas` *adds* to the shipped tables, which is what a run that
/// registers the shipped hosts and some of its own wants. This embedding
/// registers `company` and nothing else, so the shipped tables describe
/// nothing it is going to run: without `HostSchemas::only`, a program writing
/// `files.write` would check cleanly against a `files` module this run has
/// not got, and the first mention of the problem would be the boundary's
/// `unknown host module`, at run time -- the one failure handing schemas to
/// the checker is meant to prevent.
#[test]
fn an_embedding_whose_registry_is_its_own_does_not_check_against_a_module_it_has_not_got() {
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let hosts = directory(Arc::clone(&lookups));
    let text = "\
use company
use files

/// Writes what the directory says, through a host this run has not got.
export fn main() -> Result<Unit, Error> {
  let found = company.employee(\"ada\")?
  files.write(\"seniority.txt\", \"7\")?
  Ok(())
}
";

    // The default set answers for `files` out of the shipped tables, and says
    // nothing about a module the run will refuse.
    let (_, package) = package_of(text);
    let open = Compiler::new()
        .with_host_schemas(hosts.module_schemas())
        .compile(&package)
        .expect("the shipped `files` table makes this look like a checked call");
    assert!(
        !open
            .notices
            .iter()
            .any(|item| item.message.contains("`files`")),
        "the shipped fallback has nothing to warn about"
    );

    // Closing the set leaves `files` described by nothing, which is what it
    // is: the checker warns at the `use`, before anything runs.
    let (sources, package) = package_of(text);
    let closed = Compiler::new()
        .with_schemas(HostSchemas::only(hosts.module_schemas()))
        .compile(&package)
        .expect("an undescribed module is a warning, not an error");
    let rendered: String = closed
        .notices
        .iter()
        .map(|item| render(&sources, item))
        .collect();
    assert!(
        rendered.contains("no Host API schema describes the host module `files`"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("host module `company`"),
        "`company` is registered, so it is described: {rendered}"
    );

    // And the boundary agrees with what the closed set said.
    let error = Interpreter::new(&Runtime::new(
        Arc::new(closed),
        Arc::new(sources),
        Arc::new(hosts),
    ))
    .run_entry("app", "main", Vec::new())
    .expect_err("the run has no `files` module to dispatch to");
    assert!(
        error.message.contains("unknown host module `files`"),
        "{}",
        error.message
    );
}
