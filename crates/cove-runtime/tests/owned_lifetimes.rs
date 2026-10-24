//! `OwnedVm`'s one piece of `unsafe`, small enough for Miri: issue
//! [#681](https://github.com/myuon/cove/issues/681), "Miri は unsafe を含む小さな
//! 部品に適用する".
//!
//! `src/vm/parked.rs` builds a `Vm<'static>` that borrows three `Arc`s held
//! beside it in the same struct, and relies on field order to drop the `Vm`
//! first. That is sound only if nothing the `Vm` points into moves or is freed
//! while it lives — through every way an embedder can move, park, resume,
//! cancel and drop one. This drives each of those once over the smallest
//! program that parks, so that `cargo +nightly miri test -p cove-runtime --test
//! owned_lifetimes` (see `docs/ISOLATION_TESTING.md`, which says why it has not
//! finished yet: the front end runs under Miri too) checks the borrows under
//! Stacked Borrows and the frees with Miri's allocator, and an ordinary `cargo
//! t` runs the same cases in a few milliseconds.
//!
//! The program is lowered once, here, and the cases share it: under Miri the
//! front end is most of the cost.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use cove_diag::SourceMap;
use cove_runtime::{
    Budget, Effect, Grants, HostAnswer, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, OwnedVm, PreparedProgram, Reentry, Runtime, RuntimeError, Step, Transfer,
    Value,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

const WAIT: OperationSchema = OperationSchema {
    name: "wait",
    params: &[HostType::Int],
    variadic: false,
    result: HostType::Int,
    capability: "pend",
    effect: Effect::Read,
    cancellable: false,
    recordable: true,
    result_is_task_safe: true,
};

const PEND: ModuleSchema = ModuleSchema {
    name: "pend",
    capability: "pend",
    operations: &[WAIT],
    types: &[],
    resources: &[],
};

/// Pends every call, with its argument as the request.
struct Pend;

impl HostApi for Pend {
    fn module_schema(&self) -> ModuleSchema {
        PEND
    }
    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        Ok(args[0].clone())
    }
    fn call_parkable(&self, _op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        HostAnswer::Pending(Box::new(args[0].as_int().unwrap()))
    }
}

const SOURCE: &str = "\
use pend

export fn twice(n: Int) -> Int {
  pend.wait(n) + pend.wait(n + 1)
}
";

struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    prepared: PreparedProgram,
}

// `PreparedProgram` and the two `Arc`s are `Send + Sync`; the world is built
// once and every case clones out of it.
fn world() -> &'static World {
    static WORLD: OnceLock<World> = OnceLock::new();
    WORLD.get_or_init(|| {
        let mut hosts = HostRegistry::new(Grants::new(["pend"]));
        hosts.register(Box::new(Pend));
        let schemas = HostSchemas::only(hosts.module_schemas());
        let mut sources = SourceMap::new();
        let path = PathBuf::from("app/main.cove");
        let file = sources.add(path.clone(), SOURCE);
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
        cove_sema::stdlib::install(&mut sources, &mut modules).expect("the std parses");
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        };
        let checked = Compiler::new()
            .with_schemas(schemas.clone())
            .compile(&package)
            .unwrap_or_else(|_| panic!("the fixture checks"));
        let lowered = cove_ir::lower_entry(&checked, &sources, &schemas, "app", "twice")
            .unwrap_or_else(|_| panic!("the fixture lowers"));
        let hosts = Arc::new(hosts);
        World {
            runtime: Arc::new(Runtime::new(
                Arc::new(checked),
                Arc::new(sources),
                Arc::clone(&hosts),
            )),
            hosts,
            prepared: PreparedProgram::new(Arc::new(lowered)),
        }
    })
}

/// A machine whose `Arc`s are the only ones left once `world`'s clones are
/// gone — built from clones, so that dropping it is what frees what it
/// borrowed when the clones are the last holders.
fn machine() -> OwnedVm {
    let world = world();
    OwnedVm::with_heap_words(
        Arc::clone(&world.runtime),
        Arc::clone(&world.hosts),
        world.prepared.clone(),
        1 << 12,
    )
}

fn parked(vm: OwnedVm) -> cove_runtime::ParkedVm {
    match vm.invoke_within_parkable(
        Budget::new(Limits::default()),
        "app",
        "twice",
        vec![Value::int(20)],
    ) {
        Step::Parked(parked) => parked,
        _ => panic!("`twice` parks at its first call"),
    }
}

fn on_stack(body: impl FnOnce() + Send) {
    cove_runtime::on_cove_stack(body).expect("a thread to run Cove on");
}

#[test]
fn a_machine_parked_moved_resumed_and_reused_on_other_threads_keeps_its_borrows() {
    on_stack(|| {
        let mut parked = parked(machine());
        let request = *parked.take_request().unwrap().downcast::<i64>().unwrap();
        assert_eq!(request, 20);
        // Moved to another thread, resumed there, parked again, moved back.
        let again = std::thread::Builder::new()
            .stack_size(cove_runtime::STACK_SIZE)
            .spawn(move || match parked.resume(Ok(Transfer::Int(100))) {
                Step::Parked(again) => again,
                _ => panic!("the second call parks"),
            })
            .unwrap()
            .join()
            .unwrap();
        let (mut vm, answer) = match again.resume(Ok(Transfer::Int(1))) {
            Step::Answered(vm, answer) => (vm, answer),
            _ => panic!("two calls and it answers"),
        };
        assert_eq!(answer.unwrap().as_int(), Some(101));
        // Reused for a blocking run after it has crossed threads twice.
        let blocking = vm.invoke_within(
            Budget::new(Limits::default()),
            "app",
            "twice",
            vec![Value::int(1)],
        );
        assert_eq!(blocking.unwrap().as_int(), Some(3));
    });
}

#[test]
fn a_machine_dropped_parked_cancelled_or_answered_frees_what_it_borrowed_once() {
    on_stack(|| {
        // Dropped while parked, with its request still in it.
        drop(parked(machine()));
        // Cancelled, then dropped.
        let (vm, error) = parked(machine()).cancel();
        assert!(error.message.contains("cancelled"), "{}", error.message);
        drop(vm);
        // Dropped on another thread than the one it parked on.
        let parked = parked(machine());
        std::thread::spawn(move || drop(parked)).join().unwrap();
        // Never run.
        drop(machine());
    });
}
