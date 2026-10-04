//! A run that yields **inside compiled code** and is resumed elsewhere:
//! [ADR 0085](../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md).
//!
//! `yielding.rs` is ADR 0084's contract on the encoded tier. This is the same
//! contract with the native tier installed, from the embedder's side: a
//! preparation compiled once with [`PreparedProgram::with_native`], an
//! [`OwnedVm`] per run, and a run that gives its thread up at a safepoint
//! *inside* a compiled loop — at a backedge, at an allocation, at a call — with
//! two compiled frames standing, and is run on from a thread of its own.
//!
//! What has to hold is what holds for the encoded tier: the same answer, the
//! same instruction count, the same fuel, the same allocation and the same
//! collections as the run nothing interrupted **on the same tier**. (Not the
//! encoded tier's counts: the two tiers count work differently, and ADR 0040's
//! cross-backend bound is the contract between them. The answers agree.)
//!
//! Every case asserts the yields were taken *inside compiled code*
//! ([`YieldedVm::compiled_frames`]), because a yield the dispatch loop took
//! would pass every other assertion here and test nothing new.
//!
//! The gate is the native tier's: see `native_tier.rs` beside this file.

#![cfg(all(feature = "template", target_arch = "x86_64", unix))]

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::SourceMap;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{
    Budget, Cancellation, Effect, Grants, HostAnswer, HostApi, HostRegistry, HostType, Limits,
    ModuleSchema, OperationSchema, OwnedVm, PreparedProgram, Reentry, Runtime, RuntimeError, Step,
    Transfer, Value, YieldRequest, YieldedVm,
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
        op("wait", &[HostType::Int], HostType::Int),
    ],
    types: &[],
    resources: &[],
};

/// `nudge` raises the yield request of the run the test installed; `wait`
/// answers its argument plus one, pending where the run can park.
#[derive(Default)]
struct Sched {
    signal: Mutex<Option<YieldRequest>>,
}

impl HostApi for Sched {
    fn module_schema(&self) -> ModuleSchema {
        SCHED
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "nudge" => {
                if let Some(signal) = &*self.signal.lock().unwrap() {
                    signal.request();
                }
                Ok(Value::unit())
            }
            "wait" => Ok(Value::int(args[0].as_int().expect("an Int") + 1)),
            _ => unreachable!("{op}"),
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        match op {
            "wait" => HostAnswer::Pending(Box::new(args[0].as_int().expect("an Int"))),
            _ => HostAnswer::Ready(self.call_with(op, args, back)),
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
    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        self.0.call_parkable(op, args, back)
    }
}

// --------------------------------------------------------------- the program

/// Compiled functions under encoded entries.
///
/// Every `export fn` makes a host call, so it is refused and runs encoded: the
/// entry is always encoded anyway, and its `call` into `spin`, `churn` or `fib`
/// is the VM-to-native hop the chain of compiled frames starts at. `counts(0)`
/// keeps the inliner off `mix` and `churn` (it is recursive, so nothing that
/// calls it is expanded), so a yield inside `mix` leaves two compiled frames
/// standing — `spin` waiting on its call, and `mix` in its loop.
const SOURCE: &str = "\
use sched

/// Recursive, so nothing that calls it is inlined away. `counts(0)` is zero.
fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// A compiled leaf with a loop of its own.
fn mix(total: Int, i: Int) -> Int {
  var t = total
  var k = 0
  while k < 3 {
    t = (t * 31 + i + k) % 1000003
    k += 1
  }
  t + counts(0)
}

/// A compiled loop over a compiled call.
fn spin(n: Int) -> Int {
  var total = 0
  var i = 0
  while i < n {
    total = mix(total, i)
    i += 1
  }
  total
}

/// Allocates every turn, so its backedge never finds a stride due: it polls
/// at its allocations. One array stays live across the whole loop.
fn churn(n: Int) -> Int {
  let kept = [7, 11, 13]
  var total = 0
  var i = 0
  while i < n {
    let fresh = [i, i + 1, i + 2, i + 3]
    total = (total + fresh.get(i % 4).unwrapOr(0) + kept.get(i % 3).unwrapOr(0)) % 1000003
    i += 1
  }
  total + kept.get(0).unwrapOr(0) + counts(0)
}

/// A recursion with no loop in it, which polls only at its calls.
fn fib(n: Int) -> Int {
  if n < 2 {
    n
  } else {
    fib(n - 1) + fib(n - 2)
  }
}

/// Twenty rounds, each asking to yield and then running a compiled loop.
export fn main() -> Int {
  var total = 0
  for round in 0..<20 {
    sched.nudge()
    total = (total + spin(round * 50 + 400)) % 1000003
  }
  total
}

/// Long enough for a monitor thread to interrupt many times.
export fn long() -> Int {
  sched.nudge()
  spin(300000)
}

export fn allocating() -> Int {
  var total = 0
  for round in 0..<10 {
    sched.nudge()
    total = (total + churn(20000 + round)) % 1000003
  }
  total
}

export fn recursing() -> Int {
  var total = 0
  for round in 0..<10 {
    sched.nudge()
    total += fib(18 + round % 2)
  }
  total
}

/// Encoded, because of its host call, and called from compiled code: a
/// request it raises is below a `drive_from`, where nothing can yield.
fn nudged(n: Int) -> Int {
  sched.nudge()
  spin(n)
}

/// Compiled, calling an encoded function that calls a compiled one.
fn outer(n: Int) -> Int {
  var total = 0
  var i = 0
  while i < 3 {
    total = (total + nudged(n)) % 1000003
    i += 1
  }
  (total + spin(n)) % 1000003
}

export fn below() -> Int {
  sched.nudge()
  outer(3000)
}

/// Parks at `wait` and yields in `spin`, six times each.
export fn mixed() -> Int {
  var total = 0
  for round in 0..<6 {
    total += sched.wait(round)
    sched.nudge()
    total = (total + spin(2000)) % 1000003
  }
  total
}
";

// ----------------------------------------------------------------- the world

struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    sched: Arc<Sched>,
    /// Compiled once, for every run here.
    native: PreparedProgram,
    /// The same program without the tier, for the answers.
    encoded: PreparedProgram,
}

fn world() -> World {
    let sched = Arc::new(Sched::default());
    let mut hosts = HostRegistry::new(Grants::new(["sched"]));
    hosts.register(Box::new(Registered(Arc::clone(&sched))));
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
    let runtime = Arc::new(Runtime::new(
        Arc::new(checked),
        Arc::new(sources),
        Arc::clone(&hosts),
    ));
    let encoded = PreparedProgram::new(Arc::new(lowered));
    let native = encoded
        .clone()
        .with_native()
        .expect("this host compiles native code");
    World {
        runtime,
        hosts,
        sched,
        native,
        encoded,
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

/// What a run came to, in the terms two runs of it on one tier must agree on.
#[derive(Debug, PartialEq)]
struct Finished {
    answer: String,
    instructions: u64,
    fuel: u64,
    allocated: u64,
    collections: u64,
}

fn finished(vm: &OwnedVm, answer: &Result<Value, RuntimeError>) -> Finished {
    Finished {
        answer: shown(answer),
        instructions: vm.instructions(),
        fuel: vm.meter().fuel_spent(),
        allocated: vm.allocated_words(),
        collections: vm.collections(),
    }
}

impl World {
    /// A native machine whose yield request `nudge` raises.
    fn vm(&self) -> OwnedVm {
        let vm = OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.native.clone(),
        );
        *self.sched.signal.lock().unwrap() = Some(vm.yield_request());
        vm
    }

    /// A native machine nothing asks to yield.
    fn quiet(&self) -> OwnedVm {
        let vm = OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.native.clone(),
        );
        *self.sched.signal.lock().unwrap() = None;
        vm
    }

    /// `app.name` parkably on the native tier, never asked to yield; a park
    /// is answered on this thread.
    fn uninterrupted(&self, name: &str, budget: Budget) -> Finished {
        let run = drive(
            self.quiet()
                .invoke_within_parkable(budget, "app", name, Vec::new()),
        );
        assert_eq!(run.yields, 0, "nothing asked it to leave");
        run.finished
    }

    /// `app.name` on the encoded tier, for its answer.
    fn encoded_answer(&self, name: &str) -> String {
        let vm = OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.encoded.clone(),
        );
        *self.sched.signal.lock().unwrap() = None;
        let run = drive(vm.invoke_within_parkable(unlimited(), "app", name, Vec::new()));
        assert_eq!(run.native_yields, 0);
        run.finished.answer
    }
}

/// How a run went: what it came to, how many times it yielded and how many of
/// those were inside compiled code, how many times it parked, on how many
/// threads it ran, and how many safepoints declined to yield.
#[derive(Debug)]
struct Driven {
    finished: Finished,
    yields: usize,
    native_yields: usize,
    parks: usize,
    threads: usize,
    declined: u64,
    vm_to_native: u64,
}

/// A step taken apart on the thread that took it. (A [`Step`] is not `Send`;
/// its answer is a `Value`.)
enum Taken {
    Answered(Finished, u64, u64),
    Yielded(Box<YieldedVm>),
    Parked(Box<cove_runtime::ParkedVm>),
}

fn taken(step: Step) -> Taken {
    match step {
        Step::Answered(vm, answer) => Taken::Answered(
            finished(&vm, &answer),
            vm.yields_declined(),
            vm.tiers().vm_to_native,
        ),
        Step::Parked(parked) => Taken::Parked(Box::new(parked)),
        Step::Yielded(yielded) => Taken::Yielded(Box::new(yielded)),
    }
}

/// Drives `step` to an answer, resuming every yield and every park on a new
/// thread. A park is answered with its argument plus one, as `wait` blocking
/// would have answered.
fn drive(step: Step) -> Driven {
    let mut yields = 0;
    let mut native_yields = 0;
    let mut parks = 0;
    let mut threads = HashSet::new();
    let mut next = taken(step);
    loop {
        let (step, thread) = match next {
            Taken::Answered(finished, declined, vm_to_native) => {
                return Driven {
                    finished,
                    yields,
                    native_yields,
                    parks,
                    threads: threads.len(),
                    declined,
                    vm_to_native,
                };
            }
            Taken::Yielded(yielded) => {
                yields += 1;
                if yielded.compiled_frames() > 0 {
                    native_yields += 1;
                }
                std::thread::spawn(move || (taken(yielded.resume()), std::thread::current().id()))
                    .join()
                    .unwrap()
            }
            Taken::Parked(mut parked) => {
                parks += 1;
                let asked = *parked
                    .take_request()
                    .expect("wait's request")
                    .downcast::<i64>()
                    .expect("an Int");
                std::thread::spawn(move || {
                    let step = parked.resume(Ok(Transfer::Int(asked + 1)));
                    (taken(step), std::thread::current().id())
                })
                .join()
                .unwrap()
            }
        };
        threads.insert(thread);
        next = step;
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
        Step::Parked(_) => panic!("it yields before it parks"),
    }
}

/// Raises `signal` every `every` until the answer is in.
struct Monitor {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Monitor {
    fn on(signal: YieldRequest, every: Duration) -> Monitor {
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(every);
                    signal.request();
                }
            })
        };
        Monitor {
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

// --------------------------------------------------------------------- cases

/// The fixture is what it says: the functions the yields are meant to land in
/// are compiled, and the entries are not.
#[test]
fn the_loops_are_compiled_and_the_entries_are_not() {
    let world = world();
    let native = world.native.native().expect("compiled");
    let refused: Vec<&str> = native
        .refusals()
        .iter()
        .map(|refused| refused.name.as_str())
        .collect();
    for compiled in ["counts", "mix", "spin", "churn", "fib", "outer"] {
        assert!(
            !refused
                .iter()
                .any(|name| name.ends_with(&format!(".{compiled}"))),
            "{compiled} is refused: {refused:?}"
        );
    }
    for entry in ["main", "nudged"] {
        assert!(
            refused
                .iter()
                .any(|name| name.ends_with(&format!(".{entry}"))),
            "{entry} is compiled: {refused:?}"
        );
    }
}

/// One compilation, shared: a clone of a preparation holds the same code.
#[test]
fn native_code_is_compiled_once_per_preparation() {
    let world = world();
    let one = world.native.clone();
    let two = world.native.clone();
    assert!(std::ptr::eq(
        one.native().expect("compiled"),
        two.native().expect("compiled")
    ));
    assert!(world.encoded.native().is_none());
}

/// The whole of the contract: twenty yields inside a compiled loop, each run on
/// from a thread of its own, and the run nothing interrupted's answer, count,
/// fuel, allocation and collections.
#[test]
fn a_run_yielded_inside_compiled_code_answers_as_the_uninterrupted_run_does() {
    let world = world();
    let expected = world.uninterrupted("main", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "main", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert_eq!(run.yields, 20, "one yield for every nudge: {run:?}");
    assert_eq!(run.native_yields, 20, "every one inside compiled code");
    assert_eq!(run.threads, 20, "every resume on a thread of its own");
    assert_eq!(run.declined, 0, "nothing stood in the way");
    assert!(run.vm_to_native > 0, "the tier was used");
    assert_eq!(expected.answer, world.encoded_answer("main"));
}

/// A yield in `mix`'s loop leaves `spin` waiting on its call below it, and
/// both are re-entered: the innermost at its loop head, the other after the
/// call.
#[test]
fn a_yield_leaves_every_compiled_frame_of_the_chain_standing() {
    let world = world();
    let yielded = first_yield(&world, "main", unlimited());
    let frames = yielded.compiled_frames();
    assert!(
        (1..=2).contains(&frames),
        "spin, and mix if it was in it: {frames}"
    );
    let mut deepest = frames;
    let mut next = taken(yielded.resume());
    while let Taken::Yielded(yielded) = next {
        deepest = deepest.max(yielded.compiled_frames());
        next = taken(yielded.resume());
    }
    assert_eq!(deepest, 2, "some yield landed inside mix, below spin");
}

/// A monitor raising the request on a clock, as a scheduler's would: however
/// many times it lands in the compiled loop, the answer and the counts are the
/// uninterrupted run's.
#[test]
fn a_monitor_slices_a_compiled_loop_many_times() {
    let world = world();
    let expected = world.uninterrupted("long", unlimited());
    let vm = world.quiet();
    let monitor = Monitor::on(vm.yield_request(), Duration::from_micros(100));
    let run = drive(vm.invoke_within_parkable(unlimited(), "app", "long", Vec::new()));
    drop(monitor);
    assert_eq!(run.finished, expected);
    assert!(run.native_yields > 3, "sliced: {run:?}");
}

/// A loop that allocates every turn takes a safepoint at each allocation, so
/// its backedge is never due: it yields at its allocations, the allocation
/// runs again on resuming, and the array it keeps live across the loop
/// survives every collection in between.
#[test]
fn a_loop_that_allocates_yields_at_its_allocations_and_keeps_what_it_holds() {
    let world = world();
    let expected = world.uninterrupted("allocating", unlimited());
    assert!(expected.collections >= 3, "it collects: {expected:?}");
    let run = drive(world.vm().invoke_within_parkable(
        unlimited(),
        "app",
        "allocating",
        Vec::new(),
    ));
    assert_eq!(run.finished, expected);
    assert_eq!(run.native_yields, 10, "{run:?}");
    assert_eq!(expected.answer, world.encoded_answer("allocating"));
}

/// A recursion with no loop polls only at its calls, and yields there.
#[test]
fn a_recursion_yields_at_its_calls() {
    let world = world();
    let expected = world.uninterrupted("recursing", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "recursing", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert_eq!(run.native_yields, 10, "{run:?}");
    assert_eq!(expected.answer, world.encoded_answer("recursing"));
}

/// Below an encoded callee of compiled code — a `drive_from`, whose Rust
/// caller is the compiled frame — a request is declined at every safepoint,
/// compiled or encoded, and honoured at the first due safepoint in compiled
/// code once the callee has returned.
#[test]
fn a_request_below_an_encoded_callee_waits_for_it_to_return() {
    let world = world();
    let expected = world.uninterrupted("below", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "below", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert!(run.declined > 0, "the callee crossed safepoints: {run:?}");
    // One flag, raised four times: every safepoint due between the requests is
    // inside a callee, so they are one request, honoured in `outer`'s last
    // `spin` once the third callee has returned.
    assert_eq!(run.yields, 1, "{run:?}");
    assert_eq!(run.native_yields, 1, "{run:?}");
}

/// A fuel limit stops a run yielded inside compiled code where it stops the
/// uninterrupted one, at the same charge.
#[test]
fn a_fuel_limit_stops_a_yielded_compiled_run_where_it_stops_the_uninterrupted_one() {
    let world = world();
    let fuel = || {
        Budget::new(Limits {
            fuel: Some(150_000),
            ..Limits::default()
        })
    };
    let expected = world.uninterrupted("main", fuel());
    assert!(expected.answer.contains("fuel"), "{expected:?}");
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(fuel(), "app", "main", Vec::new()),
    );
    assert!(run.native_yields > 0, "{run:?}");
    assert_eq!(run.finished, expected);
}

/// Cancelled while yielded: the budget's error, and the machine back for its
/// next run, which answers.
#[test]
fn a_run_yielded_inside_compiled_code_can_be_cancelled() {
    let world = world();
    let yielded = first_yield(&world, "main", unlimited());
    assert!(yielded.compiled_frames() > 0);
    let (vm, error) = yielded.cancel();
    assert_eq!(error.message, "execution stopped: the run was cancelled");
    assert_eq!(error.outcome, RunOutcome::Cancelled);
    assert!(error.span.is_some(), "blamed on where it stood");
    *world.sched.signal.lock().unwrap() = Some(vm.yield_request());
    let again = drive(vm.invoke_within_parkable(unlimited(), "app", "main", Vec::new()));
    assert_eq!(
        again.finished.answer,
        world.uninterrupted("main", unlimited()).answer
    );
}

/// A cancellation raised while it waited ends it when it is resumed.
#[test]
fn a_run_yielded_inside_compiled_code_whose_flag_was_raised_is_cancelled_when_resumed() {
    let world = world();
    let cancellation = Cancellation::new();
    let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
    let yielded = first_yield(&world, "main", budget);
    assert!(yielded.compiled_frames() > 0);
    cancellation.cancel();
    let Step::Answered(_, Err(error)) = yielded.resume() else {
        panic!("a cancelled run does not run on");
    };
    assert_eq!(error.outcome, RunOutcome::Cancelled);
}

/// The deadline kept running while it was yielded.
#[test]
fn a_run_yielded_inside_compiled_code_keeps_its_deadline() {
    let world = world();
    let budget = Budget::new(Limits {
        deadline: Some(Duration::from_millis(5)),
        ..Limits::default()
    });
    let yielded = first_yield(&world, "main", budget);
    assert!(yielded.compiled_frames() > 0);
    std::thread::sleep(Duration::from_millis(20));
    let Step::Answered(_, Err(error)) = yielded.resume() else {
        panic!("a run past its deadline does not run on");
    };
    assert_eq!(error.outcome, RunOutcome::Deadline);
}

/// Dropping a run yielded inside compiled code abandons it and nothing else:
/// the frames it left are data, and the next run over the same code answers.
#[test]
fn dropping_a_run_yielded_inside_compiled_code_is_safe() {
    let world = world();
    for _ in 0..4 {
        let yielded = first_yield(&world, "main", unlimited());
        assert!(yielded.compiled_frames() > 0);
        drop(yielded);
    }
    assert_eq!(
        world.uninterrupted("main", unlimited()).answer,
        world.encoded_answer("main")
    );
}

/// A run that parks at a host call and yields inside compiled code, one after
/// the other, six times each, every resume on a thread of its own.
#[test]
fn a_run_parks_and_yields_inside_compiled_code_in_turn() {
    let world = world();
    let expected = world.uninterrupted("mixed", unlimited());
    let run = drive(
        world
            .vm()
            .invoke_within_parkable(unlimited(), "app", "mixed", Vec::new()),
    );
    assert_eq!(run.finished, expected);
    assert_eq!(run.parks, 6, "{run:?}");
    assert_eq!(run.native_yields, 6, "{run:?}");
    assert_eq!(run.threads, 12);
    assert_eq!(expected.answer, world.encoded_answer("mixed"));
}

/// Many isolates over one compiled program at once, each sliced by its own
/// monitor and some under a fuel limit: the code is shared and nothing else
/// is, so each answers what it answers alone.
#[test]
fn isolates_share_compiled_code_and_keep_their_own_state_and_budgets() {
    let world = Arc::new(world());
    let limited = || {
        Budget::new(Limits {
            fuel: Some(1_000_000),
            ..Limits::default()
        })
    };
    let alone = world.uninterrupted("long", unlimited());
    let alone_limited = world.uninterrupted("long", limited());
    assert!(alone_limited.answer.contains("fuel"));
    let runs: Vec<_> = (0..8)
        .map(|n| {
            let world = Arc::clone(&world);
            std::thread::spawn(move || {
                let vm = OwnedVm::new(
                    Arc::clone(&world.runtime),
                    Arc::clone(&world.hosts),
                    world.native.clone(),
                );
                let budget = match n % 2 {
                    0 => unlimited(),
                    _ => limited(),
                };
                let monitor = Monitor::on(vm.yield_request(), Duration::from_micros(50 + n * 10));
                let run = drive(vm.invoke_within_parkable(budget, "app", "long", Vec::new()));
                drop(monitor);
                (n, run)
            })
        })
        .collect();
    let mut sliced = 0;
    for run in runs {
        let (n, run) = run.join().unwrap();
        sliced += run.native_yields;
        match n % 2 {
            0 => assert_eq!(run.finished, alone, "isolate {n}"),
            _ => assert_eq!(run.finished, alone_limited, "isolate {n}"),
        }
    }
    assert!(sliced > 8, "the monitors landed: {sliced}");
}
