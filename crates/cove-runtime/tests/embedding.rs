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
//! checker was never handed -- one of the cases below is one -- and because a
//! host is held to its schema whether or not any compiler read it.
//!
//! `company` is still a table this file wrote. The last section registers a
//! module nothing wrote: its name, the capability it is gated on, and the
//! operations it answers arrive as data once the process is running.
//! `ModuleSchema` is `Copy` with `'static` contents, so a host like that
//! assembles its table once and leaks it -- and what the case there holds it
//! to is the *once*, which is what makes the cost a bounded one instead of a
//! leak per call.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cove_diag::{render, Diagnostic, SourceMap};
use cove_runtime::interp::Interpreter;
use cove_runtime::value::MapKey;
use cove_runtime::{
    Budget, Effect, FieldSchema, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, Runtime, RuntimeError, TypeSchema, Value, ValueView,
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
        let [name] = args.as_slice() else {
            unreachable!("checked by HostRegistry::call")
        };
        let Some(name) = name.as_str() else {
            unreachable!("checked by HostRegistry::call")
        };
        self.reads.lock().unwrap().push(name.to_string());
        Ok(match self.notes.get(name) {
            Some(text) => Value::ok(Value::string(*text)),
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

    match value.ok_payload() {
        Some([payload]) => {
            assert_eq!(payload.to_string(), "hello from a host-owned capability");
        }
        _ => panic!("expected `Ok(String)`, found {value}"),
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
        .call("documents", "read", vec![Value::int(3)])
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
    operations: &[
        OperationSchema {
            name: "employee",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Named("company.Employee"), &HostType::Error),
            capability: "directory",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // A table the application holds and the program reads, and a set the
        // program builds and the application is asked about. Neither could be
        // declared until `HostType` gained `Map` and `Set` (issue #153); before
        // that a host handed the first over as two arrays or as an array of
        // pairs, and the Cove side rebuilt what the host already had.
        OperationSchema {
            name: "rates",
            params: &[],
            variadic: false,
            result: HostType::Result(
                &HostType::Map(&HostType::String, &HostType::Int),
                &HostType::Error,
            ),
            capability: "directory",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "staffed",
            params: &[HostType::Set(&HostType::String)],
            variadic: false,
            result: HostType::Result(&HostType::Int, &HostType::Error),
            capability: "directory",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
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
        match (op, args.as_slice()) {
            ("employee", [id]) => {
                let Some(id) = id.as_str() else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.lookups.lock().unwrap().push(id.to_string());
                Ok(match self.people.get(id) {
                    // Through the constructor, which is the whole of what a
                    // host says about a struct: the type name the schema
                    // declares and the fields in its order. The `Rc`, the
                    // field vector and the `opaque` flag are the runtime's,
                    // and since ADR 0028 they are not merely hidden by
                    // convention -- there is no variant to write instead.
                    Some(seniority) => Value::ok(Value::structure(
                        "company.Employee",
                        [
                            ("name", Value::string(id)),
                            ("seniority", Value::int(*seniority)),
                        ],
                    )),
                    None => Value::err(Value::error(format!("no employee named `{id}`"))),
                })
            }
            // The whole table, handed over as the one value it is. A host
            // builds a `Map` out of `MapKey`s because a `Map` key is a
            // `MapKey`; nothing here converts, and nothing on the Cove side
            // rebuilds.
            ("rates", []) => {
                Ok(Value::ok(Value::map(self.people.iter().map(
                    |(name, rate)| (MapKey::Str((*name).to_string()), Value::int(*rate)),
                ))))
            }
            // And the other direction: the set the program built arrives
            // whole, and the host reads it as the set it is.
            ("staffed", [names]) => {
                let Some(names) = names.elements() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(Value::ok(Value::int(
                    names
                        .filter(|name| match name {
                            MapKey::Str(text) => self.people.contains_key(text.as_str()),
                            _ => unreachable!("checked by HostRegistry::call"),
                        })
                        .count() as i64,
                )))
            }
            (op, _) => unreachable!("`company` declares no operation `{op}` of this shape"),
        }
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

/// A `Map` and a `Set` cross whole, in both directions.
///
/// `HostType` had `Array`, `Option` and `Result` and nothing else compound
/// until issue #153, so a host holding a table of rates handed it over as
/// something else and the Cove side rebuilt what the host already had. Both
/// are ordinary `Value` variants and both are types the checker knows, so
/// what was missing was only the vocabulary to declare them in.
///
/// The checker reads the declaration — `rates.get("ada")` is an
/// `Option<Int>` here and nothing else would type — and the boundary enforces
/// the same one, which is the pairing the rest of this file is about.
#[test]
fn a_host_may_declare_a_map_and_a_set_and_hand_one_over_whole() {
    let hosts = directory(Arc::new(Mutex::new(Vec::new())));
    let (sources, checked) = compiled(
        &hosts,
        "\
use company

/// Reads a table the application holds, and asks it about a set.
export fn main() -> Result<Int, Error> {
  let rates = company.rates()?
  let known = company.staffed(Set.of(\"ada\", \"grace\"))?
  Ok(rates.get(\"ada\").unwrapOr(0) + known)
}
",
    );
    let program = checked.unwrap_or_else(|items| {
        panic!(
            "{}",
            items
                .iter()
                .map(|item| render(&sources, item))
                .collect::<String>()
        )
    });
    assert!(program.notices.is_empty(), "{:?}", program.notices);

    let value = Interpreter::new(&Runtime::new(
        Arc::new(program),
        Arc::new(sources),
        Arc::new(hosts),
    ))
    .run_entry("app", "main", Vec::new())
    .expect("the checked program runs");

    // Seven from the table plus the one name of the two that the directory
    // knows.
    assert_eq!(value.to_string(), "Ok(8)");
}

/// The element type of a declared `Set` is checked, so a program handing one
/// of the wrong element type over is refused at its call site.
#[test]
fn a_set_of_the_wrong_element_type_is_a_static_error() {
    let hosts = directory(Arc::new(Mutex::new(Vec::new())));
    let (sources, checked) = compiled(
        &hosts,
        "\
use company

/// Hands a set of `Int` where the schema declares `Set<String>`.
export fn main() -> Result<Int, Error> {
  Ok(company.staffed(Set.of(1, 2))?)
}
",
    );
    let items = checked.expect_err("a `Set<Int>` is not the declared `Set<String>`");
    let rendered: String = items.iter().map(|item| render(&sources, item)).collect();
    assert!(
        rendered.contains("expected `Set<String>`, found `Set<Int>`"),
        "{rendered}"
    );
}

/// A schema declaring a key no value can be is refused where the schema is
/// read, which for an embedder is a test it writes once over its own table.
///
/// This is the one thing adding `Map` and `Set` made possible to write and
/// impossible to satisfy: a `Set` element is a `MapKey`, and a name says
/// nothing about whether the values behind it are. The boundary would have
/// refused every value of it, on whichever call happened to carry one first;
/// `ModuleSchema::validate` says so before anything runs.
#[test]
fn a_schema_declaring_a_key_no_value_can_be_is_refused_where_it_is_read() {
    const KEYED_BY_A_DECLARED_TYPE: ModuleSchema = ModuleSchema {
        name: "company",
        capability: "directory",
        operations: &[OperationSchema {
            name: "roster",
            params: &[],
            variadic: false,
            result: HostType::Result(
                &HostType::Set(&HostType::Named("company.Employee")),
                &HostType::Error,
            ),
            capability: "directory",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        }],
        types: COMPANY.types,
        resources: &[],
    };

    assert!(COMPANY.validate().is_ok());
    assert_eq!(
        KEYED_BY_A_DECLARED_TYPE
            .validate()
            .expect_err("a set of a named type is not one anything can be")
            .to_string(),
        "the result of `company.roster` is declared `Result<Set<company.Employee>, Error>`, \
         and `company.Employee` cannot be a `Map` key or a `Set` element"
    );
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

// ---------------------------------------------------------------------------
// A module whose description is not in the binary at all.
//
// `company` above is the embedder's own module and is still a `const`: this
// file knows what it declares. A plugin does not. Its module name, the
// capability it is gated on, and the operations it answers arrive as data
// once the process is running, and `ModuleSchema` is `Copy` with `'static`
// contents, so there is nothing for such a host to borrow them from. What it
// does instead is assemble its table once and leak it.
//
// The *once* is the part worth a test. A handful of allocations per module
// for the life of a process is the cost `ModuleSchema`'s documentation
// accepts, and argues against every alternative; the same host assembling a
// table inside `module_schema` would leak on every call the registry
// dispatches, which nothing bounds. Nothing in the type system tells those
// two apart, so this does.

/// What the embedder learns only at run time: a plugin manifest read from
/// disk, a service description fetched at connect time, a module named in
/// configuration. None of it is `'static` and none of it is written here.
struct Manifest {
    /// The name Cove source will write, such as `settings`.
    module: String,
    /// The capability the module is gated on, which is not its name.
    capability: String,
    /// The operations it answers, each taking one `String` and producing a
    /// `Result<String, Error>`.
    operations: Vec<String>,
}

/// How often the host was asked to describe itself, and how often it
/// actually assembled a table. The gap between the two is what the
/// `OnceLock` below is for, and what a test can hold it to.
#[derive(Default)]
struct Asked {
    asks: AtomicUsize,
    assemblies: AtomicUsize,
}

/// A host that describes itself out of a [`Manifest`] rather than out of a
/// table this file wrote.
struct Plugin {
    manifest: Manifest,
    /// What the module actually serves, which is data like the manifest is.
    values: BTreeMap<String, String>,
    /// The assembled description. Filled on the first ask and copied out
    /// afterwards, which is what holds the leak to one per module.
    schema: OnceLock<ModuleSchema>,
    asked: Arc<Asked>,
    /// Every call that reached the implementation.
    calls: Arc<Mutex<Vec<String>>>,
}

impl Plugin {
    /// Turns the manifest into the one description this module has.
    ///
    /// Every owned piece is leaked: the module name, the capability, each
    /// operation's name, and the vector of operations itself. None of it is
    /// freed again, which is the whole reason this runs once.
    fn assemble(&self) -> ModuleSchema {
        self.asked.assemblies.fetch_add(1, Ordering::Relaxed);
        let capability: &'static str = String::leak(self.manifest.capability.clone());
        let operations: Vec<OperationSchema> = self
            .manifest
            .operations
            .iter()
            .map(|name| OperationSchema {
                name: String::leak(name.clone()),
                params: &[HostType::String],
                variadic: false,
                result: HostType::Result(&HostType::String, &HostType::Error),
                capability,
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            })
            .collect();
        ModuleSchema {
            name: String::leak(self.manifest.module.clone()),
            capability,
            operations: Vec::leak(operations),
            types: &[],
            resources: &[],
        }
    }
}

impl HostApi for Plugin {
    fn module_schema(&self) -> ModuleSchema {
        self.asked.asks.fetch_add(1, Ordering::Relaxed);
        *self.schema.get_or_init(|| self.assemble())
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // The boundary checked the call against the assembled table before
        // dispatching it, exactly as it checks one against a `const` table:
        // the operation is one the manifest named, and its one argument is a
        // `String`.
        let [key] = args.as_slice() else {
            unreachable!("checked by HostRegistry::call")
        };
        let Some(key) = key.as_str() else {
            unreachable!("checked by HostRegistry::call")
        };
        self.calls.lock().unwrap().push(format!("{op}({key})"));
        Ok(match self.values.get(key) {
            Some(value) => Value::ok(Value::string(value.as_str())),
            None => Value::err(Value::error(format!("no setting named `{key}`"))),
        })
    }
}

/// The embedding: a manifest discovered at run time, a host that describes
/// itself from it, and a registry granting exactly the capability it names.
fn plugin(asked: Arc<Asked>, calls: Arc<Mutex<Vec<String>>>) -> HostRegistry {
    let manifest = Manifest {
        module: "settings".to_string(),
        capability: "configuration".to_string(),
        operations: vec!["lookup".to_string()],
    };
    let mut hosts = HostRegistry::new(Grants::new(vec![manifest.capability.clone()]));
    hosts.register(Box::new(Plugin {
        manifest,
        values: BTreeMap::from([("theme".to_string(), "dark".to_string())]),
        schema: OnceLock::new(),
        asked,
        calls,
    }));
    hosts
}

/// The whole pattern in one case: a module nothing in the binary describes is
/// checked by the compiler, dispatched by the registry, and assembled exactly
/// once however many times either of them asks it to describe itself.
#[test]
fn a_schema_assembled_at_run_time_is_checked_dispatched_and_built_once() {
    let asked = Arc::new(Asked::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hosts = plugin(Arc::clone(&asked), Arc::clone(&calls));

    let (sources, checked) = compiled(
        &hosts,
        "\
use settings

/// Reads one setting through a module a manifest described.
export fn main() -> Result<String, Error> {
  settings.lookup(\"theme\")
}
",
    );
    let program = checked.expect("a program written against an assembled schema checks");
    assert!(
        program.notices.is_empty(),
        "a module the checker was handed warns about nothing, however its table was built"
    );
    // The capability is the manifest's, and neither the module's name nor
    // anything this file wrote.
    assert_eq!(
        program.modules["app"].functions["main"].required_capabilities,
        [Capability::new("configuration")].into_iter().collect()
    );

    let value = Interpreter::new(&Runtime::new(
        Arc::new(program),
        Arc::new(sources),
        Arc::new(hosts),
    ))
    .run_entry("app", "main", Vec::new())
    .expect("the checked program runs");
    assert_eq!(value.to_string(), "Ok(dark)");
    assert_eq!(*calls.lock().unwrap(), vec!["lookup(theme)".to_string()]);

    // The point of the `OnceLock`. `module_schemas`, every lookup the
    // boundary made on the way to the call, and the dispatch itself each ask
    // this module to describe itself, and all of them read one table: a host
    // that built a new one per ask would have leaked per ask.
    assert!(
        asked.asks.load(Ordering::Relaxed) > 1,
        "checking and dispatching ask a module to describe itself more than once"
    );
    assert_eq!(
        asked.assemblies.load(Ordering::Relaxed),
        1,
        "the table is assembled and leaked once per module, however often it is asked for"
    );
}

/// And the checker really is reading the assembled table rather than letting
/// an unknown module through: an operation the manifest never named is an
/// error at its call site, before anything runs.
#[test]
fn a_call_the_manifest_does_not_describe_is_a_static_error() {
    let asked = Arc::new(Asked::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hosts = plugin(Arc::clone(&asked), Arc::clone(&calls));

    let (sources, checked) = compiled(
        &hosts,
        "\
use settings

/// Calls an operation the manifest does not list.
export fn main() -> Result<String, Error> {
  settings.locale(\"theme\")
}
",
    );
    let items = checked.expect_err("an operation the manifest never named is an error");
    let rendered: String = items.iter().map(|item| render(&sources, item)).collect();
    assert!(
        rendered.contains("host module `settings` has no operation `locale`"),
        "{rendered}"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a program the checker refused never reaches the host"
    );
}

/// A host reads a whole answer without ever naming how the runtime holds it.
///
/// This is what ADR 0028 decision 6 makes compulsory rather than advisable.
/// Every line below is a *host* line — it compiles outside `cove-runtime`,
/// where `Value` is an abstract type — and there is no longer any other way
/// to write them: the variants are sealed, so `let Value::Struct(s) = value`
/// does not compile here at all.
///
/// The `Vector` is the case worth the trouble. Its elements sit behind a
/// cell, so nothing can hand out a `&[Value]` of them; issue #196 recorded
/// that as the one place the borrow-based reader design could not reach, and
/// `Value::vector_elements` reaches it with a guard that reads as a slice. A
/// host cannot *build* one, and that is deliberate too: a vector's identity
/// is observable, so a materialization that copied one would be wrong. That
/// is why the value comes back from a program.
#[test]
fn a_host_reads_every_part_of_an_answer_through_the_sealed_api() {
    let (sources, program) = program_of(
        "\
/// A draft the host reads apart, one part of each kind.
export struct Draft {
  title: String
  guests: Vector<String>
  seats: Array<Int>
  rates: Map<String, Int>
  tags: Set<String>
  note: Option<String>
}

export fn main() -> Draft {
  Draft(
    title: \"summit\",
    guests: Vector.of(\"ada\"),
    seats: [1, 2],
    rates: Map.of(MapEntry(key: \"ada\", value: 7)),
    tags: Set.of(\"vip\"),
    note: Some(\"soon\"),
  )
}
",
    );
    let hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
    let value = Interpreter::new(&Runtime::new(program, sources, Arc::new(hosts)))
        .run_entry("app", "main", Vec::new())
        .expect("the program runs");

    // The classification, matched with no `_` arm. That is the property
    // `ValueView` exists for: a representation change leaves this compiling
    // and a new kind of Cove value does not.
    let ValueView::Struct(draft) = value.view() else {
        panic!("expected a struct, found {value}");
    };
    assert_eq!(draft.type_name(), "app.Draft");
    assert!(!draft.is_opaque());
    assert_eq!(draft.len(), 6);
    assert_eq!(
        draft.fields().map(|(name, _)| name).collect::<Vec<_>>(),
        ["title", "guests", "seats", "rates", "tags", "note"]
    );

    let part = |name: &str| draft.field(name).expect("a declared field");
    assert_eq!(part("title").as_str(), Some("summit"));

    let guests = part("guests")
        .vector_elements()
        .expect("a vector answers a guard");
    assert_eq!(guests.len(), 1);
    assert_eq!(guests[0].as_str(), Some("ada"));
    assert!(
        part("guests").items().is_none(),
        "a vector is not an array, and the reader says so"
    );

    assert_eq!(
        part("seats").items().map(<[Value]>::len),
        Some(2),
        "an array is contiguous, which is what its reader promises"
    );

    let ValueView::Map(rates) = part("rates").view() else {
        panic!("expected a map");
    };
    assert_eq!(
        rates
            .get(&MapKey::Str("ada".to_string()))
            .and_then(Value::as_int),
        Some(7)
    );

    let ValueView::Set(tags) = part("tags").view() else {
        panic!("expected a set");
    };
    assert!(tags.contains(&MapKey::Str("vip".to_string())));

    assert_eq!(part("note").case(), Some("Some"));
    assert_eq!(
        part("note")
            .some_payload()
            .and_then(<[Value]>::first)
            .and_then(Value::as_str),
        Some("soon")
    );
}
