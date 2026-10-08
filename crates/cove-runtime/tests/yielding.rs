//! A run asked to give its thread up at a safepoint, and run on elsewhere:
//! [ADR 0084](../../../docs/adr/0084-a-run-may-yield-at-a-safepoint.md).
//!
//! The contract, from the embedder's side. A run that yields and is resumed
//! answers what the same run answers uninterrupted, in the same number of
//! instructions, doing the same work, with the same trace; a run asked to
//! yield where it cannot — inside a host's callback, beside a running task —
//! declines and goes on, and yields at the first safepoint where it can; a
//! yielded run can be cancelled and keeps its deadline; and what an embedder
//! moves between threads is `Send`.
//!
//! The host's `nudge` raises the run's own [`YieldRequest`], which makes every
//! yield here happen at a safepoint the program chose rather than one a timer
//! happened to land on. One case uses a real monitor thread instead.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::SourceMap;
use cove_runtime::trace::{RunOutcome, TraceEvent, TraceSink};
use cove_runtime::{
    Budget, Cancellation, Effect, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, OwnedVm, PreparedProgram, Reentry, Runtime, RuntimeError, Step, Value,
    YieldRequest, YieldedVm,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

// ------------------------------------------------------------------ the host

const fn op(name: &'static str, params: &'static [HostType], result: HostType) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: "sched",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const SCHED: ModuleSchema = ModuleSchema {
    name: "sched",
    capability: "sched",
    operations: &[
        op("nudge", &[], HostType::Unit),
        op("tick", &[HostType::Int], HostType::Int),
        op("run", &[HostType::Any], HostType::Int),
    ],
    types: &[],
    resources: &[],
};

/// `nudge` raises the yield request of the run the test installed; `tick`
/// answers its argument plus one at once; `run` runs a callback, which is
/// where a run cannot yield.
#[derive(Default)]
struct Sched {
    signal: Mutex<Option<YieldRequest>>,
}

impl HostApi for Sched {
    fn module_schema(&self) -> ModuleSchema {
        SCHED
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if op == "tick" {
            return Ok(Value::int(args[0].as_int().expect("an Int") + 1));
        }
        assert_eq!(op, "nudge");
        if let Some(signal) = &*self.signal.lock().unwrap() {
            signal.request();
        }
        Ok(Value::unit())
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
}

struct Registered(Arc<Sched>);

impl HostApi for Registered {
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
}

// --------------------------------------------------------------- the program

const SOURCE: &str = "\
use sched

/// Enough work to cross several safepoints.
fn spin(n: Int) -> Int {
  var total = 0
  for i in 0..<n {
    total = (total + i * 7 + 3) % 1000003
  }
  total
}

/// Twenty rounds, each asking to yield and then crossing a safepoint.
export fn main() -> Int {
  var total = 0
  for round in 0..<20 {
    sched.nudge()
    total = (total + spin(round + 400)) % 1000003
  }
  total
}

/// Long enough for a monitor thread to interrupt many times.
export fn long() -> Int {
  spin(400000)
}

/// Asked inside a callback the host runs, where the run cannot yield.
export fn inCallback() -> Int {
  let inside = sched.run(fn() {
    sched.nudge()
    spin(3000)
  })
  inside + spin(3000)
}

/// Asked beside a running task, which pins the run to its thread.
export fn withChild() -> Int {
  var total = 0
  scope work {
    let child = work.spawn {
      spin(10)
    }
    sched.nudge()
    total = spin(3000)
    total += await child
  }
  total + spin(3000)
}

/// Asked inside a `lock`, which does not pin the run.
export fn inLock() -> Int {
  let cell = Shared(0)
  cell.lock(fn(var value) {
    sched.nudge()
    value = spin(3000)
  })
  let held = cell.lock(fn(value) {
    value
  })
  held + spin(10)
}

/// A vector grown one element at a time and snapshotted every turn: each
/// turn's copy is longer than a stride, so every safepoint the loop reaches
/// is one the copy takes inside itself (#606).
fn snapshots(n: Int) -> Int {
  var v: Vector<Int> = Vector.of()
  var total = 0
  var i = 0
  while i < n {
    v.push(i * 7 % 1013)
    let copy = v.snapshot()
    total = (total + copy.length() + copy.get(i / 2).unwrapOr(0)) % 1000003
    i += 1
  }
  total
}

/// A loop of `toVector()` and a length check, and nothing else (#606).
fn regrow(n: Int) -> Int {
  var cells: Array<Int> = [1, 2, 3]
  var turns = 0
  while cells.length() < n {
    var v = cells.toVector()
    v.push(turns)
    cells = v.freeze()
    turns += 1
  }
  turns
}

/// Twenty rounds, each asking to yield and then snapshotting.
export fn snapshotting() -> Int {
  var total = 0
  for round in 0..<20 {
    sched.nudge()
    total = (total + snapshots(round + 1500)) % 1000003
  }
  total
}

/// A host call every turn, and turns far shorter than a stride: every charge
/// the loop makes is a host boundary's (#618).
fn ticks(n: Int) -> Int {
  var total = 0
  var i = 0
  while i < n {
    total = (total + sched.tick(i)) % 1000003
    i += 1
  }
  total
}

/// Twenty rounds, each asking to yield and then ticking.
export fn ticking() -> Int {
  var total = 0
  for round in 0..<20 {
    sched.nudge()
    total = (total + ticks(round + 3000)) % 1000003
  }
  total
}

/// Asked beside a running task, so the request waits for the task; once it
/// is joined, every safepoint the run reaches is inside a copy of two
/// thousand words, so the yield is offered after one.
export fn copyingAfterChild() -> Int {
  var cells: Array<Int> = [1, 2, 3]
  while cells.length() < 2000 {
    var v = cells.toVector()
    v.push(cells.length())
    cells = v.freeze()
  }
  var total = 0
  scope work {
    let child = work.spawn {
      spin(10)
    }
    sched.nudge()
    total = spin(10)
    total += await child
  }
  while cells.length() < 2100 {
    var v = cells.toVector()
    v.push(total)
    cells = v.freeze()
  }
  cells.length() + total
}

/// Twenty rounds, each asking to yield and then regrowing.
export fn regrowing() -> Int {
  var total = 0
  for round in 0..<20 {
    sched.nudge()
    total = (total + regrow(round + 1500)) % 1000003
  }
  total
}
";

// ----------------------------------------------------------------- the world

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
                    args,
                    outcome,
                    ..
                } => format!("host {task} {module}.{op} {args:?} {outcome:?}"),
                TraceEvent::EntryExit {
                    module, function, ..
                } => format!("exit {module}.{function}"),
                other => format!("{other:?}"),
            })
            .collect()
    }
}

struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    sched: Arc<Sched>,
    prepared: PreparedProgram,
    events: Arc<Events>,
}

fn world() -> World {
    let sched = Arc::new(Sched::default());
    let events = Arc::new(Events::default());
    let mut hosts = HostRegistry::new(Grants::new(["sched"]));
    hosts.register(Box::new(Registered(Arc::clone(&sched))));
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
        sched,
        prepared: PreparedProgram::new(Arc::new(lowered)),
        events,
    }
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

fn shown(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => value.to_string(),
        Err(error) => format!("error: {}", error.message),
    }
}

/// What a run came to, in the terms two runs of it must agree on.
#[derive(Debug, PartialEq)]
struct Finished {
    answer: String,
    instructions: u64,
    work: u64,
}

fn finished(vm: &OwnedVm, answer: &Result<Value, RuntimeError>) -> Finished {
    Finished {
        answer: shown(answer),
        instructions: vm.instructions(),
        work: vm.work(),
    }
}

impl World {
    /// A machine whose yield request `nudge` raises.
    fn vm(&self) -> OwnedVm {
        let vm = OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.prepared.clone(),
        );
        *self.sched.signal.lock().unwrap() = Some(vm.yield_request());
        vm
    }

    /// `app.name` parkably, never asked to yield: `nudge` raises nothing.
    fn uninterrupted(&self, name: &str, budget: Budget) -> Finished {
        let vm = self.vm();
        *self.sched.signal.lock().unwrap() = None;
        match vm.invoke_within_parkable(budget, "app", name, Vec::new()) {
            Step::Answered(vm, answer) => finished(&vm, &answer),
            Step::Parked(_) | Step::Yielded(_) => panic!("nothing asked it to leave"),
        }
    }
}

/// How a run that yielded went: what it came to, how many times it yielded,
/// on how many threads it ran, and how many safepoints declined to yield.
struct Yielding {
    finished: Finished,
    yields: usize,
    threads: usize,
    declined: u64,
}

/// A step taken apart on the thread that took it: the run answered — what
/// it came to and its declined count — or yielded. (A [`Step`] is not `Send`;
/// its answer is a `Value`.)
type Taken = Result<(Finished, u64), Box<YieldedVm>>;

fn taken(step: Step) -> Taken {
    match step {
        Step::Answered(vm, answer) => Ok((finished(&vm, &answer), vm.yields_declined())),
        Step::Parked(_) => panic!("nothing here pends"),
        Step::Yielded(yielded) => Err(Box::new(yielded)),
    }
}

/// Drives `step` to an answer, resuming every yield on a new thread.
fn drive(step: Step) -> Yielding {
    let mut yields = 0;
    let mut threads = HashSet::new();
    let mut next = taken(step);
    loop {
        match next {
            Ok((finished, declined)) => {
                return Yielding {
                    finished,
                    yields,
                    threads: threads.len(),
                    declined,
                };
            }
            Err(yielded) => {
                yields += 1;
                let (step, thread) = std::thread::spawn(move || {
                    (taken(yielded.resume()), std::thread::current().id())
                })
                .join()
                .unwrap();
                threads.insert(thread);
                next = step;
            }
        }
    }
}

fn unlimited() -> Budget {
    Budget::new(Limits::default())
}

fn first_yield(world: &World, name: &str, budget: Budget) -> YieldedVm {
    match world
        .vm()
        .invoke_within_parkable(budget, "app", name, Vec::new())
    {
        Step::Yielded(yielded) => yielded,
        Step::Answered(_, answer) => panic!("it yields first: {}", shown(&answer)),
        Step::Parked(_) => panic!("nothing here pends"),
    }
}

// --------------------------------------------------------------------- cases

#[test]
fn what_an_embedder_moves_between_threads_is_send() {
    fn sends<T: Send>() {}
    fn shares<T: Send + Sync>() {}
    sends::<YieldedVm>();
    shares::<YieldRequest>();
}

/// The whole of the contract: twenty yields, each run on from a thread of its
/// own, and the same answer in the same instructions for the same work as the
/// run nothing interrupted.
#[test]
fn a_yielded_run_resumed_on_other_threads_answers_as_the_uninterrupted_run_does() {
    let world = world();
    let expected = world.uninterrupted("main", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "main", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert_eq!(run.yields, 20, "one yield for every nudge");
    assert_eq!(run.threads, 20, "every resume on a thread of its own");
    assert_eq!(run.declined, 0, "nothing stood in the way");
}

/// A yield writes no event, so the trace of a yielded run is the
/// uninterrupted run's — which is what a replay reads back.
#[test]
fn a_yielded_run_is_traced_as_the_uninterrupted_run() {
    let world = world();
    world.uninterrupted("main", unlimited());
    let expected = world.events.taken();
    drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "main", Vec::new()),
    );
    let yielded = world.events.taken();
    assert_eq!(
        expected.iter().filter(|e| e.starts_with("host ")).count(),
        20
    );
    assert_eq!(yielded, expected);
}

/// A blocking run is never asked: the request is raised and nothing reads it.
#[test]
fn a_blocking_run_does_not_yield() {
    let world = world();
    let mut vm = world.vm();
    let answer = vm.invoke("app", "main", Vec::new());
    assert_eq!(
        finished(&vm, &answer),
        world.uninterrupted("main", unlimited())
    );
    assert_eq!(vm.yields_declined(), 0);
}

/// A request raised before a run begins is not a request for it.
#[test]
fn a_request_raised_before_the_run_begins_is_lowered() {
    let world = world();
    *world.sched.signal.lock().unwrap() = None;
    let vm = world.vm();
    *world.sched.signal.lock().unwrap() = None;
    let signal = vm.yield_request();
    signal.request();
    match vm.invoke_within_parkable(unlimited(), "app", "long", Vec::new()) {
        Step::Answered(..) => {}
        _ => panic!("the stale request was lowered"),
    }
    assert!(!signal.is_requested());
}

/// Asked inside a callback, the run declines at every safepoint the callback
/// crosses, and yields at the first one after the host call returns — once.
#[test]
fn a_run_inside_a_callback_declines_and_yields_after_it() {
    let world = world();
    let expected = world.uninterrupted("inCallback", unlimited());
    let run = drive(world.vm().invoke_within_parkable(
        unlimited(),
        "app",
        "inCallback",
        Vec::new(),
    ));
    assert_eq!(run.finished, expected);
    assert!(run.declined > 0, "the callback crossed safepoints");
    assert_eq!(
        run.yields, 1,
        "and the request waited for the first one after"
    );
}

/// Beside a running task the run declines too, until the task is awaited.
#[test]
fn a_run_beside_a_running_task_declines_until_it_is_joined() {
    let world = world();
    let expected = world.uninterrupted("withChild", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "withChild", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert!(run.declined > 0, "the task was running");
    assert_eq!(run.yields, 1);
}

/// A held `Shared` cell does not pin a run: it yields inside the `lock`, is
/// run on elsewhere still holding it, and leaves the region and takes the cell
/// again as the uninterrupted run does.
#[test]
fn a_run_inside_a_lock_yields() {
    let world = world();
    let expected = world.uninterrupted("inLock", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "inLock", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert_eq!(run.declined, 0);
    assert_eq!(run.yields, 1);
}

/// A monitor thread raising the request on a clock, as a scheduler's would:
/// however many times it lands, the answer and the count are the
/// uninterrupted run's.
#[test]
fn a_run_sliced_by_a_monitor_answers_as_the_uninterrupted_run() {
    let world = world();
    let expected = world.uninterrupted("long", unlimited());
    let vm = world.vm();
    let signal = vm.yield_request();
    let done = Arc::new(AtomicBool::new(false));
    let monitor = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_micros(200));
                signal.request();
            }
        })
    };
    let run = drive(vm.invoke_within_parkable(unlimited(), "app", "long", Vec::new()));
    done.store(true, Ordering::Relaxed);
    monitor.join().unwrap();
    assert_eq!(run.finished, expected);
    assert!(run.yields > 1, "sliced: {} yields", run.yields);
}

/// `cancel` ends a yielded run as cancelled, writes the end of the trace a
/// stopped run writes, and hands the machine back for its next run.
#[test]
fn a_cancelled_yielded_run_ends_with_a_trace_that_says_so() {
    let world = world();
    world.events.taken();
    let yielded = first_yield(&world, "main", unlimited());
    let (vm, error) = yielded.cancel();
    assert_eq!(error.message, "execution stopped: the run was cancelled");
    assert_eq!(error.outcome, RunOutcome::Cancelled);
    assert!(error.span.is_some(), "blamed on where it stood");
    let events = world.events.taken();
    assert!(events.iter().any(|e| e == "exit app.main"), "{events:#?}");
    let last = events.last().expect("the run wrote events");
    assert!(
        last.starts_with("RunEnded") && last.contains("Cancelled"),
        "{events:#?}"
    );
    let again = drive(vm.invoke_within_parkable(unlimited(), "app", "main", Vec::new()));
    assert_eq!(
        again.finished.answer,
        world.uninterrupted("main", unlimited()).answer
    );
}

/// A run whose flag was raised while it waited in a queue is cancelled when it
/// is resumed.
#[test]
fn a_yielded_run_whose_flag_was_raised_is_cancelled_when_resumed() {
    let world = world();
    let cancellation = Cancellation::new();
    let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
    let yielded = first_yield(&world, "main", budget);
    cancellation.cancel();
    let Step::Answered(_, Err(error)) = yielded.resume() else {
        panic!("a cancelled run does not run on");
    };
    assert_eq!(error.outcome, RunOutcome::Cancelled);
}

/// A deadline keeps running while a run is yielded, and resuming it past the
/// deadline stops it with the deadline.
#[test]
fn a_yielded_run_keeps_its_deadline() {
    let world = world();
    let budget = Budget::new(Limits {
        deadline: Some(Duration::from_millis(5)),
        ..Limits::default()
    });
    let yielded = first_yield(&world, "main", budget);
    assert!(yielded.time_left().is_some());
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(yielded.time_left(), Some(Duration::ZERO));
    let Step::Answered(_, Err(error)) = yielded.resume() else {
        panic!("a run past its deadline does not run on");
    };
    assert_eq!(
        error.message,
        "execution stopped: wall-clock deadline of 5ms exceeded"
    );
    assert_eq!(error.outcome, RunOutcome::Deadline);
}

/// Issue 601: a yielded run says what its heap holds and how many safepoints
/// declined, where a host enforcing limits of its own looks between slices —
/// and the declined count is the one the machine reports once it answers.
#[test]
fn a_yielded_run_reports_its_heap_and_its_declined_yields() {
    let world = world();
    let yielded = first_yield(&world, "inCallback", unlimited());
    let declined = yielded.yields_declined();
    assert!(declined > 0, "the callback crossed safepoints first");
    assert!(
        yielded.heap_words() > 0,
        "the callback's closure is on the heap"
    );
    let run = drive(yielded.resume());
    assert_eq!(run.declined, declined, "nothing declined after the yield");
}

/// A run stopped through the `Cancellation` it was given tells whoever
/// registered with the flag — which is how a host learns to cancel a run it
/// is holding yielded, without a token of its own.
#[test]
fn a_host_is_told_when_a_yielded_run_s_flag_is_raised() {
    let world = world();
    let cancellation = cove_runtime::Cancellation::new();
    let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
    let yielded = first_yield(&world, "main", budget);
    let (told, heard) = std::sync::mpsc::channel();
    yielded
        .meter()
        .cancellation()
        .on_cancel(move || told.send(()).unwrap());
    assert!(heard.try_recv().is_err(), "nothing raised yet");
    std::thread::spawn(move || cancellation.cancel());
    heard
        .recv_timeout(Duration::from_secs(10))
        .expect("the callback ran");
    let (_, error) = yielded.cancel();
    assert_eq!(error.outcome, RunOutcome::Cancelled);
}

/// **#606 and #618.** A loop whose every safepoint is taken inside a bulk
/// copy — a `snapshot` or a `toVector` every turn — or whose every charge is a
/// host call's — `tick` every turn — yields once for every request, as a loop
/// of ordinary instructions does, and answers as the uninterrupted run in the
/// same count for the same work. Before the fixes the dispatch loop's own
/// stride test never found a safepoint due, and all three ran to the end
/// without yielding once.
#[test]
fn a_loop_whose_every_charge_is_a_bulk_copy_or_a_host_call_yields_when_asked() {
    let world = world();
    for name in ["snapshotting", "regrowing", "ticking"] {
        let expected = world.uninterrupted(name, unlimited());
        assert!(
            !expected.answer.starts_with("error"),
            "{name}: {expected:?}"
        );
        let run = drive(
            world
                .vm()
                .invoke_within_parkable(unlimited(), "app", name, Vec::new()),
        );
        assert_eq!(run.finished, expected, "{name}");
        assert_eq!(run.yields, 20, "{name}: one yield for every nudge");
        assert_eq!(run.declined, 0, "{name}: nothing stood in the way");
    }
}

// Tests stood in this file that held a yielded run to the uninterrupted run's
// schedule by putting a fuel limit where the two would part: one past the
// charge a run yielded after (#606, #618), half its work, or a fixed figure.
// ADR 0091 removed the instrument with the allowance. The arithmetic they
// guarded — that a resumed run does not take the stride's safepoint again —
// is held by `crate::vm::exec`'s
// `a_yield_after_a_bulk_safepoint_is_offered_at_the_next_instruction` and
// `a_yield_after_a_host_call_waits_for_a_stride_since_the_last_safepoint`, and
// the totals by every case above.
