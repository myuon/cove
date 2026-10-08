//! How far a run goes after a stop or a yield request becomes true, measured
//! where the machine's own work can be read.
//!
//! [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)
//! states every stop as a bound: once the run's cancellation is raised, its
//! deadline passes or a yield is asked for, the run does at most `S + T` more
//! work before it notices — `S` the safepoint stride, `T` the most one step
//! between two questions can do (a turn, a compiled block, a bulk operation's
//! chunk). These tests were written against fuel, which stopped a run at a
//! chosen point; [ADR 0091](../../../../docs/adr/0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md)
//! removed fuel, so what chooses the point now is a [`Trip`]: a stop raised
//! once the machine's work reaches a figure the test picked, at many such
//! figures across each run, and recorded with the work at which it was raised.
//! The bound is then a subtraction: the work at the stop, or at the yield,
//! less the work at the raising.
//!
//! They are here and not under `tests/` because the instrument is the
//! machine's: [`Machine::work`](crate::vm::exec::Machine) and the trips are
//! this crate's and no embedder's, and a public accessor only tests wanted is
//! what ADR 0091 asked not to keep.
//!
//! Every case runs on the dispatch loop and, where the host can, on the native
//! tier; every program is one of the shapes a stop has been lost in before —
//! a loop of plain instructions, a loop of host calls (#618), a loop whose
//! every safepoint is inside a bulk copy (#606) or a growth, and a recursion
//! with no loop at all (ADR 0078).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cove_diag::SourceMap;
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

use crate::trace::RunOutcome;
use crate::vm::exec::{Trip, SAFEPOINT_STRIDE};
use crate::{
    Budget, Effect, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema, OperationSchema,
    OwnedVm, PreparedProgram, Runtime, RuntimeError, Step, Value, YieldedVm,
};

const S: u64 = SAFEPOINT_STRIDE;

/// The most one step between two questions does in the loops below that move
/// no bulk: a turn of a loop, a call and its return, or the block compiled
/// code charges on entering it.
const TURN: u64 = 128;

/// The most a bulk operation does between two of its own safepoints: one
/// chunk, which is one stride of words, and the instruction around it.
const CHUNK: u64 = SAFEPOINT_STRIDE + TURN;

// ----------------------------------------------------------------- the host

const SCHED: ModuleSchema = ModuleSchema {
    name: "sched",
    capability: "sched",
    operations: &[OperationSchema {
        name: "tick",
        params: &[HostType::Int],
        variadic: false,
        result: HostType::Int,
        capability: "sched",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

/// `tick` answers its argument plus one, at once.
struct Sched;

impl HostApi for Sched {
    fn module_schema(&self) -> ModuleSchema {
        SCHED
    }

    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        Ok(Value::int(args[0].as_int().expect("an Int") + 1))
    }
}

// -------------------------------------------------------------- the program

const SOURCE: &str = "\
use sched

fn spin(n: Int) -> Int {
  var total = 0
  for i in 0..<n {
    total = (total + i * 7 + 3) % 1000003
  }
  total
}

/// Plain instructions, and nothing else.
export fn spinning() -> Int {
  spin(150000)
}

/// A host call every turn, and turns far shorter than a stride: every charge
/// the loop makes is a host boundary's (#618).
export fn ticking() -> Int {
  var total = 0
  var i = 0
  while i < 12000 {
    total = (total + sched.tick(i)) % 1000003
    i += 1
  }
  total
}

/// A vector snapshotted every turn, each copy longer than a stride, so every
/// safepoint the loop reaches is one a copy takes inside itself (#606).
export fn snapshotting() -> Int {
  var v: Vector<Int> = Vector.of()
  var total = 0
  var i = 0
  while i < 1600 {
    v.push(i * 7 % 1013)
    let copy = v.snapshot()
    total = (total + copy.length() + copy.get(i / 2).unwrapOr(0)) % 1000003
    i += 1
  }
  total
}

/// A run grown by copying itself into a vector and freezing it, every turn.
export fn regrowing() -> Int {
  var cells: Array<Int> = [1, 2, 3]
  var turns = 0
  while cells.length() < 1600 {
    var v = cells.toVector()
    v.push(turns)
    cells = v.freeze()
    turns += 1
  }
  turns
}

/// Recursive, so no call can be inlined away, and with no loop in it: on the
/// native tier a call is the only place its descent polls (ADR 0078).
fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

export fn counting() -> Int {
  counts(30000)
}
";

/// How much of a run of `name` a stop may be raised in and be held to the
/// bound: all of it, but for the recursion, whose ascent is returns and no
/// calls. A return is not a poll on the native tier — ADR 0078 made a *call*
/// one — so the bound is the descent's, which is the first half of the run's
/// work and more; the encoded tier counts a return as it counts any
/// instruction, and is held to the same part so the two are one case.
fn stoppable(name: &str, whole: u64) -> u64 {
    match name {
        "counting" => whole * 45 / 100,
        _ => whole,
    }
}

/// Every program, with the most one of its steps does between two questions.
const PROGRAMS: [(&str, u64); 5] = [
    ("spinning", TURN),
    ("ticking", TURN),
    ("snapshotting", CHUNK),
    ("regrowing", CHUNK),
    ("counting", TURN),
];

// ---------------------------------------------------------------- the world

/// Which tier a run's compiled functions run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Encoded,
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    Native,
}

/// Every tier this host can run.
fn tiers() -> Vec<Tier> {
    #[allow(unused_mut)]
    let mut tiers = vec![Tier::Encoded];
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    tiers.push(Tier::Native);
    tiers
}

struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    encoded: PreparedProgram,
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    native: PreparedProgram,
}

fn world() -> World {
    let mut hosts = HostRegistry::new(Grants::new(["sched"]));
    hosts.register(Box::new(Sched));
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
    World {
        runtime,
        hosts,
        #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
        native: encoded
            .clone()
            .with_native()
            .expect("this host compiles native code"),
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

impl World {
    fn vm(&self, tier: Tier) -> OwnedVm {
        let prepared = match tier {
            Tier::Encoded => self.encoded.clone(),
            #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
            Tier::Native => self.native.clone(),
        };
        OwnedVm::new(Arc::clone(&self.runtime), Arc::clone(&self.hosts), prepared)
    }

    /// `app.name` on `tier` with nothing raised: its answer, instructions and
    /// work, which every interrupted run of it is held to.
    fn uninterrupted(&self, name: &str, tier: Tier) -> Finished {
        self.marked(name, tier, &[])
    }

    /// [`World::uninterrupted`], looking at each point of `at` as a run with
    /// a request raised there would and raising nothing ([`Trip::Mark`]).
    ///
    /// The schedule a yielded run is held to, position by position. Looking
    /// is not free of consequence: the dispatch loop recomputes when it next
    /// asks, and after a bulk operation that took no safepoint of its own the
    /// recomputed question can fall sooner than the one it replaced. So the
    /// run a yielded one is compared with looks where it looked.
    fn marked(&self, name: &str, tier: Tier, at: &[u64]) -> Finished {
        let mut vm = self.vm(tier);
        vm.inner()
            .set_trips(at.iter().map(|at| (*at, Trip::Mark)).collect());
        match vm.invoke_within_parkable(budget(), "app", name, Vec::new()) {
            Step::Answered(mut vm, answer) => Finished::of(&mut vm, &answer),
            Step::Parked(_) | Step::Yielded(_) => panic!("{name}: nothing asked it to leave"),
        }
    }
}

/// A budget with a deadline far away, so that [`Trip::Expire`] has one to
/// make pass.
fn budget() -> Budget {
    Budget::new(Limits {
        deadline: Some(Duration::from_secs(3600)),
        ..Limits::default()
    })
}

/// What a run came to, in the terms an interrupted run of it must agree on:
/// its answer, its instructions, its work, and the work at every safepoint it
/// took — the schedule, position by position.
#[derive(Debug, PartialEq, Eq)]
struct Finished {
    answer: String,
    instructions: u64,
    work: u64,
    safepoints: Vec<u64>,
}

impl Finished {
    fn of(vm: &mut OwnedVm, answer: &Result<Value, RuntimeError>) -> Finished {
        Finished {
            answer: match answer {
                Ok(value) => value.to_string(),
                Err(error) => format!("error: {}", error.message),
            },
            instructions: vm.instructions(),
            work: vm.work(),
            safepoints: vm.inner().safepoints().to_vec(),
        }
    }
}

/// `count` points spread across a run of `total` work, none at its very ends.
fn points(total: u64, count: u64) -> Vec<u64> {
    (1..=count).map(|k| total * k / (count + 1)).collect()
}

// ------------------------------------------------------------ stop bounds

/// **A cancellation or a passed deadline raised anywhere in a run is noticed
/// within one stride and one step**, on every tier and every shape, at a dozen
/// points across each run — not only at the run's first safepoint.
///
/// What is measured is the work the run did between the raising and the stop.
/// A loop of host calls notices at its next call; a bulk operation at its
/// next chunk; compiled code at its next poll, a backedge's or a call's. The
/// trip is also held to the point it was asked for, so that a stop raised late
/// cannot pass the bound by being measured from where it was raised.
#[test]
fn a_stop_raised_anywhere_in_a_run_is_noticed_within_one_stride_and_one_step() {
    let world = world();
    for tier in tiers() {
        for (name, step) in PROGRAMS {
            let whole = world.uninterrupted(name, tier).work;
            assert!(
                whole > 20 * S,
                "{name} on {tier:?}: long enough to stop inside"
            );
            for trip in [Trip::Cancel, Trip::Expire] {
                let want = match trip {
                    Trip::Cancel => RunOutcome::Cancelled,
                    _ => RunOutcome::Deadline,
                };
                for at in points(stoppable(name, whole), 12) {
                    let mut vm = world.vm(tier);
                    vm.inner().set_trips(vec![(at, trip)]);
                    let error = vm
                        .invoke_within(budget(), "app", name, Vec::new())
                        .expect_err("a raised stop stops the run");
                    let what = format!("{name} on {tier:?}, {trip:?} at {at}");
                    assert_eq!(error.outcome, want, "{what}: {}", error.message);
                    let raised = vm.inner().raised().to_vec();
                    assert_eq!(raised.len(), 1, "{what}: {raised:?}");
                    let (_, when) = raised[0];
                    assert!(
                        when >= at && when - at <= S + step,
                        "{what}: raised at {when}, which is not where it was asked for"
                    );
                    let after = vm.work() - when;
                    assert!(
                        after <= S + step,
                        "{what}: {after} work after the stop was raised, past S + T = {}",
                        S + step
                    );
                    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
                    if tier == Tier::Native && name == "counting" {
                        assert!(
                            vm.tiers().native_to_native_direct > 0,
                            "{what}: the descent ran in compiled code: {:?}",
                            vm.tiers()
                        );
                    }
                }
            }
        }
    }
}

// ----------------------------------------------------------- yield bounds

/// One yielded run's progress, driven to its answer.
struct Driven {
    finished: Finished,
    /// For each yield: the work done between the last request and the yield.
    after_request: Vec<u64>,
    /// How many requests were raised over the run.
    requests: usize,
}

/// A step taken apart on the thread that took it: the run answered — what it
/// came to and how many requests were raised — or it yielded. (A [`Step`] is
/// not `Send`; its answer is a `Value`.)
type Taken = Result<(Finished, usize), Box<YieldedVm>>;

fn taken(step: Step) -> Taken {
    match step {
        Step::Answered(mut vm, answer) => {
            let requests = vm.inner().raised().len();
            Ok((Finished::of(&mut vm, &answer), requests))
        }
        Step::Parked(_) => panic!("nothing here pends"),
        Step::Yielded(yielded) => Err(Box::new(yielded)),
    }
}

/// Drives a parkable run of `app.name` to its answer with a yield requested
/// at every point of `at`, resuming every yield — every third on a thread of
/// its own.
fn driven(world: &World, tier: Tier, name: &str, at: &[u64]) -> Driven {
    let mut vm = world.vm(tier);
    vm.inner()
        .set_trips(at.iter().map(|at| (*at, Trip::Yield)).collect());
    let mut next = taken(vm.invoke_within_parkable(budget(), "app", name, Vec::new()));
    let mut after_request = Vec::new();
    loop {
        match next {
            Ok((finished, requests)) => {
                return Driven {
                    finished,
                    after_request,
                    requests,
                };
            }
            Err(yielded) => {
                let raised = yielded.raised();
                let (_, when) = *raised.last().expect("a yield answers a request");
                // Nought when the request was raised at the safepoint that
                // yields: the yield stands before the instruction it counted.
                after_request.push(yielded.work().saturating_sub(when));
                // Each request is answered by its own yield before the next
                // is raised, which is what makes the subtraction above the
                // distance from *this* request.
                assert_eq!(
                    raised.len(),
                    after_request.len(),
                    "{name} on {tier:?}: one yield per request"
                );
                next = if after_request.len() % 3 == 0 {
                    std::thread::spawn(move || taken(yielded.resume()))
                        .join()
                        .unwrap()
                } else {
                    taken(yielded.resume())
                };
            }
        }
    }
}

/// **A resumed run keeps yielding, every time it is asked, within one stride
/// and one step of being asked** — on the dispatch loop and the native tier,
/// in a loop of plain instructions, a loop of host calls (ADR 0090, #618), a
/// loop whose safepoints are inside bulk copies (ADR 0089, #606), a growth,
/// and a loop-free recursion. And it answers what the run nothing
/// interrupted answers, in the same instructions and the same work, having
/// taken the same safepoints at the same points of its work — position by
/// position, so that a resumed run which took the stride's safepoint a second
/// time, after a bulk operation's or a host call's charge it had already made
/// (#606, #618), is a difference here and not only a shifted schedule.
///
/// The requests are three strides apart, so a run that lost its ability to
/// yield after a resume — the shape of both #606 and #618 — would answer a
/// request late or not at all, and the count of yields, which is asserted
/// equal to the count of requests, would fall short.
#[test]
fn a_resumed_run_yields_again_and_again_within_one_stride_and_one_step() {
    let world = world();
    for tier in tiers() {
        for (name, step) in PROGRAMS {
            let whole = world.uninterrupted(name, tier).work;
            let at: Vec<u64> = (1..)
                .map(|k| k * 3 * S)
                .take_while(|at| *at + 3 * S < stoppable(name, whole))
                .collect();
            let expected = world.marked(name, tier, &at);
            assert!(at.len() >= 10, "{name} on {tier:?}: asked many times");
            let run = driven(&world, tier, name, &at);
            let what = format!("{name} on {tier:?}");
            assert_eq!(run.finished, expected, "{what}: answers as uninterrupted");
            assert_eq!(run.requests, at.len(), "{what}: every request was raised");
            assert_eq!(
                run.after_request.len(),
                at.len(),
                "{what}: every request was answered by a yield"
            );
            for (k, after) in run.after_request.iter().enumerate() {
                assert!(
                    *after <= S + step,
                    "{what}: yield {k} came {after} work after its request, past S + T = {}",
                    S + step
                );
            }
        }
    }
}
