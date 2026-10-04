//! A run parked at a host call and resumed on another thread:
//! [ADR 0080](../../../docs/adr/0080-a-host-call-may-answer-pending.md).
//!
//! What is held here is the contract, from the embedder's side of it. A run
//! that parks and is resumed answers what the same run answers when every host
//! call blocks, in the same number of instructions, with the same trace; a host
//! call made where the run cannot park — inside a callback, beside a running
//! task, inside a `lock` — blocks instead, and the program cannot tell; and the
//! types an embedder moves between threads are `Send`, which the compiler
//! checks below rather than a comment claiming it.
//!
//! The host here, `fetch`, answers every call the same way whether it pends or
//! not: a pending `get(n)` is answered by the test with [`reply`], and a
//! blocking one computes the same number itself. So any difference between the
//! two runs is the runtime's and not the host's.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use cove_diag::SourceMap;
use cove_runtime::trace::{TraceEvent, TraceSink};
use cove_runtime::{
    Effect, Grants, HostAnswer, HostApi, HostRegistry, HostType, ModuleSchema, OperationSchema,
    OwnedVm, ParkedVm, PreparedProgram, Reentry, ResourceHandle, ResourceSchema, Runtime,
    RuntimeError, Step, Transfer, Value, Vm,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

// ------------------------------------------------------------------ the host

const fn op(name: &'static str, params: &'static [HostType], result: HostType) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: "fetch",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const CONN: ResourceSchema = ResourceSchema {
    name: "Conn",
    task_safe: true,
    operations: &[op("read", &[HostType::Int], HostType::Int)],
};

const FETCH: ModuleSchema = ModuleSchema {
    name: "fetch",
    capability: "fetch",
    operations: &[
        op("get", &[HostType::Int], HostType::Int),
        op("run", &[HostType::Any], HostType::Int),
        op("open", &[], HostType::Named("fetch.Conn")),
    ],
    types: &[],
    resources: &[CONN],
};

/// What `fetch` asks its embedder for when it pends: the host's own type,
/// which the runtime moves and never reads.
#[derive(Debug, PartialEq)]
enum Request {
    Get(i64),
    Read(i64),
}

/// The answer to a request — and the answer a blocking call computes itself.
fn reply(request: &Request) -> i64 {
    match request {
        Request::Get(n) => n * n + 1,
        Request::Read(n) => 1000 + n,
    }
}

/// Counts which way each call was made.
#[derive(Default)]
struct Fetch {
    pended: AtomicUsize,
    blocked: AtomicUsize,
}

fn int(args: &[Value]) -> i64 {
    args[0].as_int().expect("the schema says Int")
}

impl HostApi for Fetch {
    fn module_schema(&self) -> ModuleSchema {
        FETCH
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "get" => {
                self.blocked.fetch_add(1, Ordering::Relaxed);
                Ok(Value::int(reply(&Request::Get(int(&args)))))
            }
            "open" => Ok(Value::from_resource(ResourceHandle::new("fetch", &CONN, 1))),
            other => unreachable!("`{other}` is answered by `call_with`"),
        }
    }

    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            "run" => back.call(&args[0], Vec::new()),
            _ => self.call(op, args),
        }
    }

    fn call_resource(
        &self,
        _handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        _back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        assert_eq!(op, "read");
        self.blocked.fetch_add(1, Ordering::Relaxed);
        Ok(Value::int(reply(&Request::Read(int(&args)))))
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        match op {
            "get" => {
                self.pended.fetch_add(1, Ordering::Relaxed);
                HostAnswer::Pending(Box::new(Request::Get(int(&args))))
            }
            _ => HostAnswer::Ready(self.call_with(op, args, back)),
        }
    }

    fn call_resource_parkable(
        &self,
        _handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        _back: &mut dyn Reentry,
    ) -> HostAnswer {
        assert_eq!(op, "read");
        self.pended.fetch_add(1, Ordering::Relaxed);
        HostAnswer::Pending(Box::new(Request::Read(int(&args))))
    }
}

// --------------------------------------------------------------- the program

const SOURCE: &str = "\
use fetch

/// Five gets and a read, with work between them.
export fn main() -> Int {
  var total = 0
  for i in 0..<5 {
    total += fetch.get(i) * (i + 1)
  }
  let conn = fetch.open()
  total + conn.read(total)
}

/// A get inside a callback the host runs, and one after it.
export fn inCallback() -> Int {
  let inside = fetch.run(fn() {
    fetch.get(7)
  })
  inside + fetch.get(8)
}

/// A get beside a running task, and one after the scope — and a task spawned
/// after that, which lands beside the first in the run's task table.
export fn withChild() -> Int {
  var total = 0
  scope work {
    let child = work.spawn {
      40
    }
    total = fetch.get(2)
    total += await child
  }
  total += fetch.get(3)
  scope more {
    let second = more.spawn {
      2
    }
    total += await second
  }
  total
}

/// A get inside a `lock`, and one after it.
export fn inLock() -> Int {
  let cell = Shared(0)
  cell.lock(fn(var value) {
    value = fetch.get(4)
  })
  let held = cell.lock(fn(value) {
    value
  })
  held + fetch.get(5)
}
";

/// Everything a run is built over, built once.
struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    fetch: Arc<Fetch>,
    prepared: PreparedProgram,
    events: Arc<Events>,
}

/// A host module registered by `Arc`, so the test can read its counters.
struct Counted(Arc<Fetch>);

impl HostApi for Counted {
    fn module_schema(&self) -> ModuleSchema {
        self.0.module_schema()
    }
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.0.call(op, args)
    }
    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        self.0.call_with(op, args, back)
    }
    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        self.0.call_resource(handle, op, args, back)
    }
    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        self.0.call_parkable(op, args, back)
    }
    fn call_resource_parkable(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> HostAnswer {
        self.0.call_resource_parkable(handle, op, args, back)
    }
}

/// Every event a run writes, in order.
#[derive(Default)]
struct Events(Mutex<Vec<TraceEvent>>);

impl TraceSink for Events {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    /// The events, less the durations two runs of one program cannot share.
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
            .into_iter()
            .map(|event| match event {
                TraceEvent::HostCall {
                    task,
                    module,
                    op,
                    capability,
                    granted,
                    args,
                    outcome,
                    ..
                } => {
                    format!("host {task} {module}.{op} {capability} {granted} {args:?} {outcome:?}")
                }
                TraceEvent::EntryExit {
                    module, function, ..
                } => format!("exit {module}.{function}"),
                other => format!("{other:?}"),
            })
            .collect()
    }
}

fn world_with(module: Box<dyn HostApi>, fetch: Arc<Fetch>) -> World {
    let events = Arc::new(Events::default());
    let mut hosts = HostRegistry::new(Grants::new(["fetch"]));
    hosts.register(module);
    hosts.set_trace(Arc::clone(&events) as Arc<dyn TraceSink>);
    let schemas = HostSchemas::only(hosts.module_schemas());
    let (sources, package) = packaged(SOURCE);
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
    let lowered = cove_ir::lower(&checked, &sources, &schemas)
        .unwrap_or_else(|items| panic!("the fixture lowers:\n{}", render(items)));
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(
        Runtime::new(Arc::new(checked), Arc::new(sources), Arc::clone(&hosts))
            .with_trace(Arc::clone(&events) as Arc<dyn TraceSink>),
    );
    World {
        runtime,
        hosts,
        fetch,
        prepared: PreparedProgram::new(Arc::new(lowered)),
        events,
    }
}

fn world() -> World {
    let fetch = Arc::new(Fetch::default());
    world_with(Box::new(Counted(Arc::clone(&fetch))), fetch)
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

impl World {
    fn vm(&self) -> OwnedVm {
        OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.prepared.clone(),
        )
    }

    /// `app.name` with every host call blocking: the reference answer.
    fn blocking(&self, name: &str) -> (String, u64) {
        let mut vm = self.vm();
        let answer = vm.invoke("app", name, Vec::new());
        (shown(answer), vm.instructions())
    }
}

fn shown(answer: Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => value.to_string(),
        Err(error) => format!("error: {}", error.message),
    }
}

/// A run that answered, as something that may leave the thread it ran on.
type Finished = (String, u64);

/// Resumes `parked` on a thread of its own with the answer to its request,
/// and comes back with the run answered or parked again.
fn resume_elsewhere(mut parked: ParkedVm) -> (Result<Finished, ParkedVm>, ThreadId) {
    let request = parked
        .take_request()
        .expect("a parked run carries its request")
        .downcast::<Request>()
        .expect("the request is the host's own type");
    let answer = Transfer::Int(reply(&request));
    std::thread::spawn(move || {
        let outcome = match parked.resume(Ok(answer)) {
            Step::Answered(vm, answer) => Ok((shown(answer), vm.instructions())),
            Step::Parked(parked) => Err(parked),
        };
        (outcome, std::thread::current().id())
    })
    .join()
    .unwrap()
}

/// Runs `app.name` parkably, answering every request on a new thread, and
/// says what it answered, in how many instructions, and how many times it
/// parked on how many threads.
fn parked_run(world: &World, name: &str) -> (Finished, usize, usize) {
    let mut threads = std::collections::HashSet::new();
    let mut parks = 0;
    let mut next = match world.vm().invoke_parkable("app", name, Vec::new()) {
        Step::Answered(vm, answer) => return ((shown(answer), vm.instructions()), 0, 0),
        Step::Parked(parked) => parked,
    };
    loop {
        parks += 1;
        let (outcome, thread) = resume_elsewhere(next);
        threads.insert(thread);
        match outcome {
            Ok(finished) => return (finished, parks, threads.len()),
            Err(parked) => next = parked,
        }
    }
}

// --------------------------------------------------------------------- cases

/// The types an embedder moves between threads are `Send`, and the check is
/// the compiler's.
#[test]
fn what_an_embedder_moves_between_threads_is_send() {
    fn sends<T: Send>() {}
    sends::<OwnedVm>();
    sends::<ParkedVm>();
    sends::<Vm<'static>>();
    sends::<Transfer>();
    sends::<RuntimeError>();
}

/// The whole of the contract, on the program that parks at every call: six
/// parks, each resumed on a thread of its own, and the same answer in the same
/// number of instructions as the run where every call blocked.
#[test]
fn a_run_parked_and_resumed_on_other_threads_answers_as_the_blocking_run_does() {
    let world = world();
    let blocking = world.blocking("main");
    assert_eq!(world.fetch.blocked.load(Ordering::Relaxed), 6);
    assert_eq!(world.fetch.pended.load(Ordering::Relaxed), 0);
    let expected: i64 = (0..5).map(|i| (i * i + 1) * (i + 1)).sum::<i64>();
    assert_eq!(blocking.0, (expected + 1000 + expected).to_string());

    let (parked, parks, threads) = parked_run(&world, "main");
    assert_eq!(parked, blocking, "the same answer in the same instructions");
    assert_eq!(parks, 6, "five gets and a resource read");
    assert_eq!(threads, 6, "every resume ran on a thread of its own");
    assert_eq!(world.fetch.pended.load(Ordering::Relaxed), 6);
    assert_eq!(world.fetch.blocked.load(Ordering::Relaxed), 6, "none more");
}

/// The trace of a parked run is the trace of the blocking one: every host
/// call recorded once, in order, with its arguments and the answer it was
/// resumed with — which is what a replay reads back.
#[test]
fn a_parked_call_is_traced_as_the_call_that_blocked() {
    let world = world();
    world.blocking("main");
    let blocking = world.events.taken();
    parked_run(&world, "main");
    let parked = world.events.taken();
    assert_eq!(
        blocking.iter().filter(|e| e.starts_with("host ")).count(),
        7,
        "five gets, an open and a read: {blocking:#?}"
    );
    assert_eq!(parked, blocking);
}

/// And what was recorded replays: a host that answers each call from the
/// parked run's tape, in order and without computing anything, drives the
/// program to the same answer.
#[test]
fn a_parked_run_s_tape_replays_to_the_same_answer() {
    let world = world();
    let (answer, _, _) = parked_run(&world, "main");
    let tape: VecDeque<String> = world
        .events
        .taken()
        .into_iter()
        .filter(|event| event.starts_with("host "))
        .collect();
    assert_eq!(tape.len(), 7);

    /// Answers `get` and `read` with the `Int` the tape recorded.
    struct Replay(Mutex<VecDeque<String>>);
    impl Replay {
        fn next(&self) -> Value {
            let event = self.0.lock().unwrap().pop_front().expect("the tape has it");
            let at = event
                .rfind("Int(")
                .expect("an Int outcome, last on the line")
                + 4;
            let digits: String = event[at..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            Value::int(digits.parse().unwrap())
        }
    }
    impl HostApi for Replay {
        fn module_schema(&self) -> ModuleSchema {
            FETCH
        }
        fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            match op {
                "open" => {
                    self.0.lock().unwrap().pop_front();
                    Ok(Value::from_resource(ResourceHandle::new("fetch", &CONN, 1)))
                }
                _ => Ok(self.next()),
            }
        }
        fn call_resource(
            &self,
            _: &ResourceHandle,
            _: &str,
            _: Vec<Value>,
            _: &mut dyn Reentry,
        ) -> Result<Value, RuntimeError> {
            Ok(self.next())
        }
    }
    let replay = world_with(
        Box::new(Replay(Mutex::new(tape))),
        Arc::new(Fetch::default()),
    );
    assert_eq!(replay.blocking("main"), answer);
}

/// A call inside a callback the host is running blocks, because the host's
/// Rust frame is below it; the call after the callback returns parks.
#[test]
fn a_call_inside_a_callback_blocks_and_the_one_after_it_parks() {
    let world = world();
    let blocking = world.blocking("inCallback");
    assert_eq!(blocking.0, (50 + 65).to_string());
    world.fetch.blocked.store(0, Ordering::Relaxed);

    let (parked, parks, _) = parked_run(&world, "inCallback");
    assert_eq!(parked, blocking);
    assert_eq!(parks, 1, "only `get(8)` could park");
    assert_eq!(
        world.fetch.blocked.load(Ordering::Relaxed),
        1,
        "`get(7)` blocked"
    );
}

/// A call beside a task that has not been joined blocks, because the task is a
/// thread inside this run's thread scope; the call after the scope parks, and a
/// task spawned after the resume is placed and joined correctly.
#[test]
fn a_call_beside_a_running_task_blocks_and_the_one_after_the_scope_parks() {
    let world = world();
    let blocking = world.blocking("withChild");
    assert_eq!(blocking.0, (5 + 40 + 10 + 2).to_string());
    world.fetch.blocked.store(0, Ordering::Relaxed);

    let (parked, parks, _) = parked_run(&world, "withChild");
    assert_eq!(parked, blocking);
    assert_eq!(parks, 1, "only `get(3)` could park");
    assert_eq!(
        world.fetch.blocked.load(Ordering::Relaxed),
        1,
        "`get(2)` blocked"
    );
}

/// A machine whose last run spawned tasks spawns again in its next one.
///
/// The case a resumed run depends on, without any parking: the run's earlier
/// tasks are still in its task table when a new thread scope opens, so the
/// scope's handle list has to start with a place for each of them, or the next
/// task's handle lands at an index the table does not give it.
#[test]
fn a_machine_that_spawned_spawns_again_in_its_next_run() {
    let world = world();
    let mut vm = world.vm();
    for _ in 0..3 {
        assert_eq!(shown(vm.invoke("app", "withChild", Vec::new())), "57");
    }
}

/// A call inside a `lock` blocks, because the cell is held until the region
/// ends; the call after it parks, and the cell is free on the next thread.
#[test]
fn a_call_inside_a_lock_blocks_and_the_one_after_it_parks() {
    let world = world();
    let blocking = world.blocking("inLock");
    assert_eq!(blocking.0, (17 + 26).to_string());
    world.fetch.blocked.store(0, Ordering::Relaxed);

    let (parked, parks, _) = parked_run(&world, "inLock");
    assert_eq!(parked, blocking);
    assert_eq!(parks, 1, "only `get(5)` could park");
    assert_eq!(
        world.fetch.blocked.load(Ordering::Relaxed),
        1,
        "`get(4)` blocked"
    );
}

/// An answer the operation's declaration does not admit is refused when it
/// arrives, at the call, as a host that answered it at once would have been.
#[test]
fn an_answer_of_the_wrong_type_is_refused_at_the_call() {
    let world = world();
    let Step::Parked(parked) = world.vm().invoke_parkable("app", "main", Vec::new()) else {
        panic!("the first get parks");
    };
    let refused = std::thread::spawn(
        move || match parked.resume(Ok(Transfer::Str("no".into()))) {
            Step::Answered(_, answer) => answer
                .map(|v| v.to_string())
                .map_err(|e| (e.message, e.rule.map(String::from))),
            Step::Parked(_) => panic!("a refused answer ends the run"),
        },
    )
    .join()
    .unwrap();
    let (message, rule) = refused.expect_err("the answer is refused");
    assert!(message.contains("fetch.get"), "{message}");
    assert!(rule.is_some_and(|rule| rule.contains("schema")));
}

/// A host that failed fails the run at the call, with the host's error.
#[test]
fn a_failed_answer_fails_the_run() {
    let world = world();
    let Step::Parked(parked) = world.vm().invoke_parkable("app", "main", Vec::new()) else {
        panic!("the first get parks");
    };
    match parked.resume(Err(RuntimeError::new("the peer went away"))) {
        Step::Answered(_, answer) => {
            let error = answer.expect_err("the run fails");
            assert_eq!(error.message, "the peer went away");
            assert!(error.span.is_some(), "blamed on the call");
        }
        Step::Parked(_) => panic!("a failed answer ends the run"),
    }
}

/// A machine handed back by an answered run runs again, blocking or parking,
/// and a parked run that is dropped is simply gone.
#[test]
fn an_answered_machine_runs_again_and_a_dropped_one_is_gone() {
    let world = world();
    let Step::Parked(parked) = world.vm().invoke_parkable("app", "main", Vec::new()) else {
        panic!("the first get parks");
    };
    assert_eq!(
        parked.request().and_then(|r| r.downcast_ref::<Request>()),
        Some(&Request::Get(0))
    );
    drop(parked);

    let mut vm = world.vm();
    let first = vm.invoke("app", "inLock", Vec::new());
    assert_eq!(shown(first), "43");
    let Step::Parked(parked) = vm.invoke_parkable("app", "inLock", Vec::new()) else {
        panic!("`get(5)` parks");
    };
    let (outcome, _) = resume_elsewhere(parked);
    assert_eq!(
        outcome.ok().map(|(answer, _)| answer).as_deref(),
        Some("43")
    );
}

// ------------------------------------------------- a budget is the run's (#577)

/// A budget bounding at most `host_calls` host calls and `fuel` fuel.
fn bounded(host_calls: u64, fuel: u64) -> cove_runtime::Budget {
    cove_runtime::Budget::new(cove_runtime::Limits {
        fuel: Some(fuel),
        max_host_calls: Some(host_calls),
        ..cove_runtime::Limits::default()
    })
}

/// What `main` spends, blocking, alone: the figures every concurrent run of it
/// has to be charged exactly, whatever else is running over the registry.
fn spent_alone(world: &World) -> (u64, u64) {
    let mut vm = world.vm();
    let answer = vm.invoke_within(bounded(7, u64::MAX), "app", "main", Vec::new());
    assert!(answer.is_ok(), "{}", shown(answer));
    (vm.meter().host_calls(), vm.meter().fuel_spent())
}

/// Issue #577: many runs over one registry at once, on threads of their own,
/// each with a budget of its own — and each charged its own calls and fuel
/// and nothing of anybody else's.
///
/// `main` makes seven host calls. The runs given a limit of seven must all
/// answer, which they did not when a budget was installed in the registry:
/// every run's calls went to whichever budget was installed last. The runs
/// given six must all stop at the seventh call, with the error a lone run
/// stops with — a budget that was somebody else's would have let some of
/// them through.
#[test]
fn concurrent_runs_over_one_registry_are_charged_to_their_own_budgets() {
    let world = Arc::new(world());
    let (calls, fuel) = spent_alone(&world);
    assert_eq!(calls, 7);
    let workers: Vec<_> = (0..8)
        .map(|worker| {
            let world = Arc::clone(&world);
            std::thread::spawn(move || {
                let limit = if worker % 2 == 0 { 7 } else { 6 };
                let mut vm = world.vm();
                for _ in 0..200 {
                    let answer =
                        vm.invoke_within(bounded(limit, fuel * 2), "app", "main", Vec::new());
                    if limit == 7 {
                        assert!(answer.is_ok(), "a run within its limit: {}", shown(answer));
                        assert_eq!(vm.meter().fuel_spent(), fuel, "its own fuel, exactly");
                    } else {
                        assert_eq!(
                            shown(answer),
                            "error: execution stopped: host-call limit of 6 exceeded"
                        );
                    }
                    assert_eq!(vm.meter().host_calls(), 7, "its own calls, exactly");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("every run was charged its own calls");
    }
    assert!(
        world.hosts.with_budget(|_| ()).is_none(),
        "the registry was given no run's budget"
    );
}

/// The same, for parked runs: eight runs parked at once over one registry,
/// resumed in turn, each on a thread of its own. Round-robin is the order that
/// a budget installed in the registry got most wrong — every resume charged
/// the budget of the run that began last — and here every run is charged its
/// own seven calls and its own limit stops it.
#[test]
fn parked_runs_resumed_in_turn_are_charged_to_their_own_budgets() {
    let world = world();
    let (_, fuel) = spent_alone(&world);
    let limit = |run: usize| if run == 3 { 6 } else { 7 };
    let mut parked: Vec<Option<ParkedVm>> = (0..8)
        .map(|run| {
            match world.vm().invoke_within_parkable(
                bounded(limit(run), fuel * 2),
                "app",
                "main",
                Vec::new(),
            ) {
                Step::Parked(parked) => Some(parked),
                Step::Answered(_, answer) => panic!("the first get parks: {}", shown(answer)),
            }
        })
        .collect();
    let mut answers: Vec<Option<(String, u64, u64)>> = (0..8).map(|_| None).collect();
    while parked.iter().any(Option::is_some) {
        for run in 0..parked.len() {
            let Some(mut next) = parked[run].take() else {
                continue;
            };
            let request = next
                .take_request()
                .expect("a parked run carries its request")
                .downcast::<Request>()
                .expect("the request is the host's own type");
            let answer = Transfer::Int(reply(&request));
            let step = std::thread::spawn(move || match next.resume(Ok(answer)) {
                Step::Answered(vm, answer) => Ok((
                    shown(answer),
                    vm.meter().host_calls(),
                    vm.meter().fuel_spent(),
                )),
                Step::Parked(parked) => Err(Box::new(parked)),
            })
            .join()
            .unwrap();
            match step {
                Ok(finished) => answers[run] = Some(finished),
                Err(again) => parked[run] = Some(*again),
            }
        }
    }
    let (expected, _) = world.blocking("main");
    for (run, answer) in answers.into_iter().enumerate() {
        let (answer, calls, spent) = answer.expect("every run answered");
        assert_eq!(calls, 7, "run {run}: its own calls, exactly");
        if limit(run) == 7 {
            assert_eq!(answer, expected, "run {run}");
            assert_eq!(spent, fuel, "run {run}: its own fuel, exactly");
        } else {
            assert_eq!(
                answer, "error: execution stopped: host-call limit of 6 exceeded",
                "run {run}"
            );
        }
    }
}

/// A spawned task is charged to the budget of the run that spawned it — its
/// place under the concurrency limit, and its fuel — which is the run's own
/// and not the registry's.
#[test]
fn a_spawned_task_is_charged_to_its_run_s_budget() {
    let world = world();
    let tasks = |max_tasks| {
        cove_runtime::Budget::new(cove_runtime::Limits {
            max_tasks: Some(max_tasks),
            ..cove_runtime::Limits::default()
        })
    };
    let mut vm = world.vm();
    let answer = vm.invoke_within(tasks(1), "app", "withChild", Vec::new());
    assert_eq!(shown(answer), "57");
    assert_eq!(
        vm.meter().live_tasks(),
        0,
        "each place went back at its join"
    );
    assert_eq!(vm.meter().host_calls(), 2);
    let with_tasks = vm.meter().fuel_spent();

    let answer = vm.invoke_within(tasks(0), "app", "withChild", Vec::new());
    assert!(
        shown(answer).starts_with("error: execution stopped: concurrency limit of 0 task(s)"),
        "the run's own limit refused the spawn"
    );
    let refused = vm.meter().fuel_spent();
    assert!(
        refused < with_tasks,
        "the children's fuel was the run's: {refused} refused against {with_tasks} run"
    );
    assert!(world.hosts.with_budget(|_| ()).is_none());
}

/// The tree-walking interpreter's budget is its run's as well: interpreters
/// over one `Runtime` on threads of their own, each stopped by its own limit
/// and charged its own calls.
#[test]
fn concurrent_interpreters_over_one_registry_are_charged_to_their_own_budgets() {
    let world = Arc::new(world());
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let world = Arc::clone(&world);
            std::thread::Builder::new()
                .stack_size(cove_runtime::STACK_SIZE)
                .spawn(move || {
                    let limit = if worker % 2 == 0 { 7 } else { 6 };
                    let mut interpreter = cove_runtime::interp::Interpreter::new(&world.runtime);
                    for _ in 0..100 {
                        let answer = interpreter.invoke_within(
                            bounded(limit, u64::MAX),
                            "app",
                            "main",
                            Vec::new(),
                        );
                        if limit == 7 {
                            assert!(answer.is_ok(), "a run within its limit: {}", shown(answer));
                        } else {
                            assert_eq!(
                                shown(answer),
                                "error: execution stopped: host-call limit of 6 exceeded"
                            );
                        }
                        let meter = interpreter.meter().expect("the run was given a budget");
                        assert_eq!(meter.host_calls(), 7, "its own calls, exactly");
                    }
                })
                .unwrap()
        })
        .collect();
    for worker in workers {
        worker.join().expect("every run was charged its own calls");
    }
}
