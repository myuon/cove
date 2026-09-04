//! What each way of stopping a run may let it do before it stops.
//!
//! [Issue #120](https://github.com/myuon/cove/issues/120) asked for a
//! contract rather than a claim: both backends batch their budget checks, so
//! "the difference is not observable" was too strong, and what is wanted
//! instead is a *bound* — how much work, and which effects, may still happen
//! between the moment a stop becomes true and the moment the run notices it.
//!
//! Every test here **measures** that quantity and asserts a maximum. None of
//! them asserts that the code does what a comment says it does, and none of
//! them pins an exact figure: a maximum is the shape that survives a
//! scheduler, a machine, and a change to any constant that only makes the
//! bound tighter. Where a figure is exact it is because the whole thing is
//! deterministic, and where it cannot be — a spawned task's thread starts
//! when the operating system says so — the test makes it deterministic with a
//! handshake rather than sleeping and hoping.
//!
//! Both backends are run for every case, because ADR 0019 made `fuel_spent`
//! backend-specific and the question this file answers is what survives that:
//! the two are held to the same *shape* of bound, in their own units, and not
//! to stopping at the same source operation. `docs/adr/0024-a-stop-is-a-bound-not-a-point.md`
//! is where that is decided, and
//! `docs/adr/0030-a-host-call-asks-the-fuel-limit.md` is where the fuel
//! bound at a Host call was narrowed to zero.
//! `docs/adr/0040-a-bound-outlives-its-backend.md` is where `Vm`'s own
//! bounds are stated: the table ADR 0024 asked for, taken out of the prose
//! beside the backend ADR 0034 deleted and put into the record, with every
//! row of it measured here. That is ADR 0040's restatement of ADR 0024's
//! obligation, and it is why moving a bound costs an ADR superseding
//! ADR 0040 as well as the test below that measures it.
//!
//! The instrument is a host module called `probe`, defined below. It is here
//! rather than borrowed from `clock` because the shipped bounded call —
//! `clock.timeout` — raises its flag from a watchdog thread, which is a race,
//! and a test of a bound must not be one. `probe.bounded` raises its flag
//! when the body asks it to, so every measurement below is reproducible.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use cove_diag::SourceMap;
use cove_runtime::budget::DEADLINE_CHECK_INTERVAL;
use cove_runtime::interp::{Interpreter, SAFEPOINT_FUEL};
use cove_runtime::trace::{RunOutcome, TraceEvent, TraceSink};
use cove_runtime::{
    Budget, Cancellation, Effect, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, Reentry, Runtime, RuntimeError, Value, Vm, SAFEPOINT_STRIDE,
};
use cove_sema::resolve::Program as Checked;
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

// ------------------------------------------------------------- the probe

/// The one operation shape every probe operation has: no arguments except
/// where a callback is taken, and a `Result` so Cove can write `?`.
const fn op(name: &'static str, params: &'static [HostType]) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        // Every one of them is declared an irreversible write, so
        // `HostRegistry::irreversible_writes` counts them without this file
        // having to be believed about its own tally.
        effect: Effect::IrreversibleWrite,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "probe",
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const PROBE: ModuleSchema = ModuleSchema {
    name: "probe",
    capability: "probe",
    operations: &[
        op("tick", &[]),
        op("noop", &[]),
        op("cancelRun", &[]),
        op("raise", &[]),
        op("bounded", &[HostType::Any]),
        op("hold", &[]),
        op("watch", &[]),
        op("awaitArrival", &[]),
        op("release", &[]),
    ],
    types: &[],
    resources: &[],
};

/// A one-shot gate two threads hand control across.
///
/// A test that slept instead would be measuring the sleep. This blocks until
/// the other side has arrived, with a generous timeout that turns a
/// deadlocked test into a named failure rather than a hung suite.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self, what: &str) {
        let open = self.open.lock().unwrap();
        let (_open, outcome) = self
            .changed
            .wait_timeout_while(open, Duration::from_secs(30), |open| !*open)
            .unwrap();
        assert!(!outcome.timed_out(), "waiting for {what} timed out");
    }
}

/// The instrument: a host module that counts its own effects and can raise
/// every kind of stop flag on demand.
#[derive(Default)]
struct Probe {
    /// How many `probe.tick()` calls the host actually performed.
    ticks: AtomicU64,
    /// What [`Probe::ticks`] stood at when a stop was armed, so "effects
    /// after the stop" is a subtraction rather than a story.
    armed_at: AtomicU64,
    /// The run's own cancellation flag, so `probe.cancelRun` can raise it
    /// from inside the run instead of from a racing thread.
    run: Mutex<Option<Cancellation>>,
    /// The stop flags of the bounded calls in progress, innermost last.
    bounds: Mutex<Vec<Cancellation>>,
    /// How many `probe.bounded` calls the host actually re-entered Cove
    /// through. [`Probe::bounds`] is pushed and popped, so it says nothing
    /// once the run is over; this is the count a test can read afterwards to
    /// know a callback was reached at all.
    reentries: AtomicU64,
    arrived: Gate,
    released: Gate,
    /// Whether `probe.watch` saw [`Reentry::is_cancelled`] go true while it
    /// was polling.
    noticed: Mutex<Option<bool>>,
}

impl Probe {
    fn arm(&self) {
        self.armed_at
            .store(self.ticks.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Host effects performed after a stop was armed. The number every
    /// no-effect-after-a-stop assertion below is about.
    fn ticks_after_arming(&self) -> u64 {
        self.ticks.load(Ordering::SeqCst) - self.armed_at.load(Ordering::SeqCst)
    }
}

impl HostApi for Probe {
    fn module_schema(&self) -> ModuleSchema {
        PROBE
    }

    fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "tick" => {
                self.ticks.fetch_add(1, Ordering::SeqCst);
            }
            "noop" => {}
            "cancelRun" => {
                self.arm();
                if let Some(flag) = self.run.lock().unwrap().as_ref() {
                    flag.cancel();
                }
            }
            "raise" => {
                self.arm();
                if let Some(flag) = self.bounds.lock().unwrap().last() {
                    flag.cancel();
                }
            }
            "hold" => {
                self.arrived.open();
                self.released.wait("the parent to release the child task");
            }
            "awaitArrival" => self.arrived.wait("the child task to reach `probe.hold`"),
            "release" => self.released.open(),
            other => panic!("no probe operation `{other}`"),
        }
        Ok(Value::ok(Value::unit()))
    }

    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        if op == "watch" {
            // What `clock.every` and `http.Server.handle` do between rounds:
            // ask the way back whether the work has been asked to stop. A
            // host that is told the truth ends its own loop; one that is not
            // keeps going until something raises out of it.
            self.arrived.open();
            let mut noticed = false;
            for _ in 0..2000 {
                if back.is_cancelled() {
                    noticed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            *self.noticed.lock().unwrap() = Some(noticed);
            return Ok(Value::ok(Value::unit()));
        }
        if op != "bounded" {
            return self.call(op, args);
        }
        // `clock.timeout` in miniature, and deliberately the same shape: a
        // flag raised while the body runs, the body stopped at its next
        // safepoint, and the host turning that stop into the answer it
        // promised rather than letting it end the run.
        let stop = Cancellation::new();
        self.reentries.fetch_add(1, Ordering::SeqCst);
        self.bounds.lock().unwrap().push(stop.clone());
        let outcome = back.call_until(&args[0], Vec::new(), &stop);
        self.bounds.lock().unwrap().pop();
        match outcome {
            Ok(_) => Ok(Value::ok(Value::unit())),
            Err(_) if stop.is_cancelled() => Ok(Value::err(Value::error("probe: stopped"))),
            Err(error) => Err(error),
        }
    }
}

/// The registry's handle on the one `Probe` the test also holds.
struct Registered(Arc<Probe>);

impl HostApi for Registered {
    fn module_schema(&self) -> ModuleSchema {
        PROBE
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

// ------------------------------------------------------------- the harness

/// Every event a run recorded, so a test can ask what a stop was reported as.
#[derive(Default)]
struct Recorder(Mutex<Vec<TraceEvent>>);

impl TraceSink for Recorder {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Which backend ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    Ast,
    Vm,
}

/// What one run of one program on one backend did.
struct Run {
    backend: Backend,
    /// The value, or the message of the error that stopped it.
    answer: String,
    /// Whether the run ended by being stopped rather than by answering.
    stopped: bool,
    /// The one terminal classification the run's trace carries.
    outcome: RunOutcome,
    /// The run's total, read off the shared budget after the run.
    fuel_spent: u64,
    /// What `Vm` charged for, which is `None` on the tree walk because a
    /// tree walk has no instructions.
    instructions: Option<u64>,
    /// Irreversible Host effects, counted by the boundary rather than by the
    /// host.
    irreversible_writes: u64,
    ticks: u64,
    /// Host effects performed after a stop was armed.
    ticks_after_arming: u64,
    /// How many times the host re-entered Cove through `probe.bounded`.
    reentries: u64,
    /// What `probe.watch` was told, when a program called it.
    noticed_the_stop: Option<bool>,
}

impl Run {
    /// What the run overspent its fuel limit by.
    fn overshoot(&self, limit: u64) -> u64 {
        self.fuel_spent.saturating_sub(limit)
    }
}

fn packaged(source: &str) -> (SourceMap, Package) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!(
            "the source parses:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    (
        sources,
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: BTreeMap::from([(
                "m".to_string(),
                Module {
                    name: "m".to_string(),
                    dir: PathBuf::from("m"),
                    units: vec![Unit { file, path, ast }],
                },
            )]),
        },
    )
}

/// Runs `source` on `backend` under `limits`, and reports everything the
/// tests below measure.
///
/// The two backends are entered through `run_entry` on both sides, which is
/// the seam `cove run` chooses at, so nothing here compares two differently
/// shaped calls.
fn go(source: &str, limits: Limits, backend: Backend) -> Run {
    let (sources, package) = packaged(source);
    let probe = Arc::new(Probe::default());
    let mut registry = HostRegistry::new(Grants::new(vec!["probe"]));
    registry.register(Box::new(Registered(Arc::clone(&probe))));
    let checked = match Compiler::new()
        .with_host_schemas(registry.module_schemas())
        .compile(&package)
    {
        Ok(checked) => Arc::new(checked),
        Err(items) => panic!(
            "the source checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    let cancellation = Cancellation::new();
    *probe.run.lock().unwrap() = Some(cancellation.clone());
    registry.set_budget(Budget::with_cancellation(limits, cancellation));
    let recorder = Arc::new(Recorder::default());
    registry.set_trace(recorder.clone());
    let hosts = Arc::new(registry);
    let sources = Arc::new(sources);
    let checked: Arc<Checked> = checked;
    let runtime =
        Runtime::new(checked.clone(), sources.clone(), hosts.clone()).with_trace(recorder.clone());
    let (answer, stopped, instructions) = cove_runtime::on_cove_stack(|| match backend {
        Backend::Vm => {
            let program = match cove_ir::lower(&checked, &sources, &HostSchemas::new().with(PROBE))
            {
                Ok(program) => Arc::new(program),
                Err(items) => panic!(
                    "the program lowers:\n{}",
                    items
                        .iter()
                        .map(|item| cove_diag::render(&sources, item))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            };
            let mut machine = Vm::new(&runtime, &hosts, &program);
            let answer = machine.run_entry("m", "main", Vec::new());
            (
                described(&answer),
                answer.is_err(),
                Some(machine.instructions()),
            )
        }
        Backend::Ast => {
            let answer = Interpreter::new(&runtime).run_entry("m", "main", Vec::new());
            (described(&answer), answer.is_err(), None)
        }
    })
    .expect("a thread to run Cove on");
    let outcome = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|event| match event {
            TraceEvent::RunEnded { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .expect("every run records how it ended");
    let noticed_the_stop = *probe.noticed.lock().unwrap();
    Run {
        backend,
        answer,
        stopped,
        outcome,
        fuel_spent: hosts.with_budget(|budget| budget.fuel_spent()).unwrap_or(0),
        instructions,
        irreversible_writes: hosts.irreversible_writes(),
        ticks: probe.ticks.load(Ordering::SeqCst),
        ticks_after_arming: probe.ticks_after_arming(),
        reentries: probe.reentries.load(Ordering::SeqCst),
        noticed_the_stop,
    }
}

fn described(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => value.to_string(),
        Err(error) => error.message.clone(),
    }
}

/// One program on both backends, which is how every case here is written:
/// the bound is a property of the runtime and not of one of its two
/// evaluators.
fn on_both(source: &str, limits: Limits) -> [Run; 2] {
    [
        go(source, limits.clone(), Backend::Ast),
        go(source, limits, Backend::Vm),
    ]
}

/// What one turn of `source`'s loop costs on `backend`, measured rather than
/// derived: the same program at two loop counts, differenced.
///
/// `source` must take `{turns}` where its loop bound goes. A bound written
/// against this figure stays true when the lowering changes what a turn is
/// made of, which is the whole reason it is measured here instead of being
/// written down as a number.
fn fuel_per_turn(source: &str, backend: Backend) -> u64 {
    let at = |turns: u64| {
        go(
            &source.replace("{turns}", &turns.to_string()),
            Limits::default(),
            backend,
        )
        .fuel_spent
    };
    let (few, many) = (at(100), at(1100));
    assert!(many > few, "a loop that turns more must cost more");
    (many - few).div_ceil(1000)
}

/// The fuel a run of `source` spends with its loop turning `turns` times and
/// nothing stopping it: the prefix every bound below is stated on top of.
fn fuel_without_stopping(source: &str, turns: u64, backend: Backend) -> u64 {
    go(
        &source.replace("{turns}", &turns.to_string()),
        Limits::default(),
        backend,
    )
    .fuel_spent
}

/// A loop that turns `{turns}` times, doing arithmetic and nothing else, with
/// `{arm}` called once before it. No call inside the body, so on the tree
/// walk the only safepoint a turn reaches is its back edge — the case
/// `SAFEPOINT_FUEL` charges on every turn. `Vm` has no per-turn safepoint at
/// all: its safepoint falls every `SAFEPOINT_STRIDE` instructions regardless
/// of where a back edge is, so a turn may or may not land on one. This is the
/// loop the gathering bound below is stated against, on both backends, for
/// two different reasons.
const SPINNER: &str = "\
use probe

export fn main() -> Result<Int, Error> {
  var i = 0
  var t = 0
  probe.{arm}()?
  while i < {turns} {
    t = t + i
    i = i + 1
  }
  Ok(t)
}
";

/// [`SPINNER`] with nothing armed and no Host call at all.
///
/// The deadline, `max_host_calls` and — since ADR 0030 — fuel are all read at
/// every Host call as well as at a safepoint, so a program with a `probe`
/// call in it would be stopped there rather than on the schedule under test.
/// This is the loop the two deadline cases and the fuel case measure, for
/// that reason.
const PURE_SPINNER: &str = "\
export fn main() -> Result<Int, Error> {
  var i = 0
  var t = 0
  while i < {turns} {
    t = t + i
    i = i + 1
  }
  Ok(t)
}
";

/// The same loop inside a bounded call, so that `probe.raise` stops the body
/// and the caller survives to be asked what happened.
const BOUNDED_SPINNER: &str = "\
use probe

export fn main() -> Result<Int, Error> {
  let outcome = probe.bounded(fn() {
    var i = 0
    var t = 0
    probe.{arm}()
    while i < {turns} {
      t = t + i
      i = i + 1
    }
  })
  Ok(0)
}
";

// -------------------------------------------- a bound for every stop mode

/// **The run's own cancellation.** A loop that would never end stops, and
/// what it runs after the flag is raised is one gathering of back-edge fuel
/// plus the turn that crosses it — measured against the same program with
/// nothing raised.
#[test]
fn a_cancelled_run_stops_within_one_gathering_of_back_edge_fuel() {
    let control = SPINNER.replace("{arm}", "noop");
    let armed = SPINNER
        .replace("{arm}", "cancelRun")
        .replace("{turns}", "100000000");
    for backend in [Backend::Ast, Backend::Vm] {
        let turn = fuel_per_turn(&control, backend);
        let prefix = fuel_without_stopping(&control, 0, backend);
        let bound = prefix + gathering(backend) + turn;
        let run = go(&armed, Limits::default(), backend);
        assert!(run.stopped, "{backend:?}: a cancelled run stops");
        assert_eq!(run.outcome, RunOutcome::Cancelled, "{backend:?}");
        assert!(
            run.fuel_spent <= bound,
            "{backend:?}: a cancelled run spent {} fuel, past the bound of {bound} \
             (prefix {prefix} + one gathering + one {turn}-fuel turn)",
            run.fuel_spent
        );
        // The control: without the flag the same loop is not bounded by
        // anything this test would notice, so the assertion above is about
        // the stop and not about a loop that was short anyway.
        assert!(
            fuel_without_stopping(&control, 100_000, backend) > 20 * bound,
            "{backend:?}: the loop this bounds is unbounded without the stop"
        );
    }
}

/// **A bounded call's flag**, which is what `clock.timeout` raises. The same
/// bound as the run's own cancellation, because both are read at the same
/// safepoint on the same schedule — and the caller survives the stop, so
/// this is the one stop mode a program can observe from the inside.
#[test]
fn a_bounded_call_stops_within_one_gathering_of_back_edge_fuel() {
    let control = BOUNDED_SPINNER.replace("{arm}", "noop");
    let armed = BOUNDED_SPINNER
        .replace("{arm}", "raise")
        .replace("{turns}", "100000000");
    for backend in [Backend::Ast, Backend::Vm] {
        let turn = fuel_per_turn(&control, backend);
        let prefix = fuel_without_stopping(&control, 0, backend);
        let bound = prefix + gathering(backend) + turn;
        let run = go(&armed, Limits::default(), backend);
        // The run itself did not stop: the host turned the stop into the
        // answer it promised, which is what makes a timeout a timeout.
        assert!(!run.stopped, "{backend:?}: {}", run.answer);
        assert_eq!(run.outcome, RunOutcome::Success, "{backend:?}");
        assert!(
            run.fuel_spent <= bound,
            "{backend:?}: a stopped bounded body spent {} fuel, past the bound of {bound}",
            run.fuel_spent
        );
    }
}

/// **A task's own cancellation**, made deterministic with a handshake: the
/// child parks inside a Host call, the parent cancels it and lets it go, and
/// what the child runs after that is the measurement.
///
/// Without the handshake this would be a race — a thread starts when the
/// operating system says so — and a flaky bound test is worse than none.
#[test]
fn a_cancelled_task_stops_within_one_gathering_of_back_edge_fuel() {
    let child = "\
use probe

export fn main() -> Result<Int, Error> {
  scope s {
    let t = s.spawn {
      probe.hold()?
      var i = 0
      var n = 0
      while i < {turns} {
        n = n + i
        i = i + 1
      }
      Ok(n)
    }
    probe.awaitArrival()?
    t.cancel()
    probe.release()?
  }
  Ok(0)
}
";
    let control = child.replace("{turns}", "0");
    for backend in [Backend::Ast, Backend::Vm] {
        let spinner = SPINNER.replace("{arm}", "noop");
        let turn = fuel_per_turn(&spinner, backend);
        let prefix = go(&control, Limits::default(), backend).fuel_spent;
        let bound = prefix + gathering(backend) + turn;
        let run = go(
            &child.replace("{turns}", "100000000"),
            Limits::default(),
            backend,
        );
        assert!(
            run.fuel_spent <= bound,
            "{backend:?}: a cancelled task's run spent {} fuel, past the bound of {bound}",
            run.fuel_spent
        );
        // The child was cancelled rather than finishing, which is what makes
        // the figure above a measurement of the stop.
        assert!(
            run.fuel_spent < prefix + 100_000,
            "{backend:?}: the cancelled child ran the whole loop"
        );
    }
}

/// **An expired deadline, with no fuel limit set.** The clock is then read at
/// every safepoint, so a run whose deadline has already passed stops at the
/// first one — one gathering's worth of instructions in, on `Vm`.
#[test]
fn an_expired_deadline_with_no_fuel_limit_stops_at_the_first_safepoint() {
    let source = PURE_SPINNER.replace("{turns}", "100000000");
    for run in on_both(
        &source,
        Limits {
            deadline: Some(Duration::ZERO),
            ..Limits::default()
        },
    ) {
        let backend = run.backend;
        assert!(
            run.stopped,
            "{backend:?}: an expired deadline stops the run"
        );
        assert_eq!(run.outcome, RunOutcome::Deadline, "{backend:?}");
        // One safepoint's worth: the tree walk charges `SAFEPOINT_FUEL` for
        // the one it took. `Vm` charges a whole `SAFEPOINT_STRIDE`, because
        // its first safepoint is not entry itself but the first instruction
        // count that is a multiple of the stride, and the fuel for the
        // instructions since the last safepoint — here, since the run began —
        // is added before the deadline is even asked about.
        let bound = gathering(backend);
        assert!(
            run.fuel_spent <= bound,
            "{backend:?}: spent {} fuel past an expired deadline, bound {bound}",
            run.fuel_spent
        );
    }
}

/// **An expired deadline, with a fuel limit set.** The clock then costs more
/// than the limit fuel already enforces, so it is read every
/// `DEADLINE_CHECK_INTERVAL`th safepoint — and the bound is that many
/// safepoints' work rather than one.
#[test]
fn an_expired_deadline_beside_a_fuel_limit_stops_within_the_clock_check_interval() {
    let control = PURE_SPINNER.to_string();
    let source = control.replace("{turns}", "100000000");
    for backend in [Backend::Ast, Backend::Vm] {
        let turn = fuel_per_turn(&control, backend);
        let prefix = fuel_without_stopping(&control, 0, backend);
        let bound = prefix + DEADLINE_CHECK_INTERVAL * (gathering(backend) + turn);
        let run = go(
            &source,
            Limits {
                deadline: Some(Duration::ZERO),
                fuel: Some(u64::MAX),
                ..Limits::default()
            },
            backend,
        );
        assert!(
            run.stopped,
            "{backend:?}: an expired deadline stops the run"
        );
        assert_eq!(run.outcome, RunOutcome::Deadline, "{backend:?}");
        assert!(
            run.fuel_spent <= bound,
            "{backend:?}: spent {} fuel past an expired deadline, bound {bound}",
            run.fuel_spent
        );
    }
}

/// **An exhausted fuel budget.** Fuel is a budget rather than a flag: it is
/// not true until it is measured, and it is measured at a safepoint. So the
/// bound is what the run may overspend its limit by, and it is one gathering
/// plus one turn on both backends — each backend's gathering a fixed charge
/// added for work already done rather than reserved ahead of it: `Vm`
/// charges a whole `SAFEPOINT_STRIDE` every `SAFEPOINT_STRIDE` instructions,
/// and the tree walk charges `SAFEPOINT_FUEL` at every safepoint of its own.
#[test]
fn an_exhausted_fuel_budget_is_overspent_by_less_than_one_gathering() {
    let control = PURE_SPINNER.to_string();
    let source = control.replace("{turns}", "100000000");
    for backend in [Backend::Ast, Backend::Vm] {
        let turn = fuel_per_turn(&control, backend);
        let bound = gathering(backend) + turn;
        for limit in [1_000, 5_000, 20_000] {
            let run = go(
                &source,
                Limits {
                    fuel: Some(limit),
                    ..Limits::default()
                },
                backend,
            );
            assert!(
                run.stopped,
                "{backend:?}: an exhausted budget stops the run"
            );
            assert_eq!(run.outcome, RunOutcome::Fuel, "{backend:?}");
            assert!(
                run.overshoot(limit) <= bound,
                "{backend:?}: overspent a {limit} limit by {}, past the bound of {bound}",
                run.overshoot(limit)
            );
        }
    }
}

/// How much fuel each backend may gather between two answers to "may this
/// loop continue?".
///
/// On `Vm` that is [`SAFEPOINT_STRIDE`]: a safepoint runs every
/// `SAFEPOINT_STRIDE` instructions, at a fixed instruction stride, and the
/// fuel for the whole stride is charged there in one batch rather than as
/// each instruction runs — so what a run may still do after a stop becomes
/// true is up to one stride's worth of instructions, whatever they are. There
/// is no separate back-edge case: a loop's back edge is not a safepoint of its
/// own, only an instruction the stride counts like any other. On the tree
/// walk every back edge *is* a safepoint, so nothing gathers there and the
/// figure is the one safepoint's own fixed charge.
fn gathering(backend: Backend) -> u64 {
    match backend {
        Backend::Ast => SAFEPOINT_FUEL,
        Backend::Vm => SAFEPOINT_STRIDE,
    }
}

// ------------------------------------- no Host effect after a raised flag

/// **The adversarial one.** A Host effect cannot be taken back, so the
/// question worth being hostile about is whether one can be made to happen
/// after the runtime has been told to stop.
///
/// It can, if a Host call is not a place the stop flags are read — and it was
/// not. `Budget::charge_host_call` refuses a call from a run that was
/// cancelled or is past its deadline, but a `Budget` is shared by every task
/// of a run, so it cannot hold this task's own cancellation or the flag of a
/// bounded call this thread is inside. Before `crate::interp::stopped_here`
/// existed, the two `probe.tick()` calls below both ran, on both backends.
#[test]
fn no_host_effect_follows_a_bounded_call_that_was_asked_to_stop() {
    let source = "\
use probe

export fn main() -> Result<Int, Error> {
  let outcome = probe.bounded(fn() {
    probe.raise()
    probe.tick()
    probe.tick()
    var i = 0
    while i < 100000000 {
      i = i + 1
    }
  })
  Ok(0)
}
";
    for run in on_both(source, Limits::default()) {
        assert_eq!(
            run.ticks_after_arming, 0,
            "{:?}: {} Host effects followed a raised bound",
            run.backend, run.ticks_after_arming
        );
    }
}

/// The same for the run's own cancellation, which the boundary has always
/// refused a call for, and which is here so that a change to
/// `Budget::charge_host_call` cannot quietly give it up.
#[test]
fn no_host_effect_follows_a_cancelled_run() {
    let source = "\
use probe

export fn main() -> Result<Unit, Error> {
  probe.tick()?
  probe.cancelRun()?
  probe.tick()?
  probe.tick()?
  Ok(())
}
";
    for run in on_both(source, Limits::default()) {
        assert!(run.stopped, "{:?}", run.backend);
        assert_eq!(
            run.ticks_after_arming, 0,
            "{:?}: a Host effect followed a cancelled run",
            run.backend
        );
        assert_eq!(run.ticks, 1, "{:?}", run.backend);
    }
}

/// And for a cancelled task, which is the case the two backends used to
/// disagree about: the boundary knew the *run* was cancelled and not that
/// *this task* was.
#[test]
fn no_host_effect_follows_a_cancelled_task() {
    let source = "\
use probe

export fn main() -> Result<Int, Error> {
  scope s {
    let t = s.spawn {
      probe.hold()?
      probe.tick()?
      probe.tick()?
      Ok(0)
    }
    probe.awaitArrival()?
    t.cancel()
    probe.release()?
  }
  Ok(0)
}
";
    for run in on_both(source, Limits::default()) {
        assert_eq!(
            run.ticks, 0,
            "{:?}: a cancelled task performed {} Host effects",
            run.backend, run.ticks
        );
    }
}

/// A Host effect a stopped run *had already performed* stays performed, and
/// nothing here pretends otherwise. This is the visible-effects half of the
/// contract: what a stop bounds is what happens next, not what happened.
#[test]
fn the_host_effects_a_stopped_body_already_made_remain_made() {
    let source = "\
use probe

export fn main() -> Result<Int, Error> {
  let outcome = probe.bounded(fn() {
    var i = 0
    while i < 100000000 {
      probe.tick()
      if i == 2 {
        probe.raise()
      }
      i = i + 1
    }
  })
  Ok(0)
}
";
    for run in on_both(source, Limits::default()) {
        let backend = run.backend;
        assert_eq!(
            run.ticks_after_arming, 0,
            "{backend:?}: an effect followed the raised bound"
        );
        assert_eq!(
            run.ticks, 3,
            "{backend:?}: the three effects made before the bound was raised stand"
        );
        // The boundary's own tally agrees with the host's, so neither is
        // being taken on trust.
        assert_eq!(run.irreversible_writes, run.ticks + 2, "{backend:?}");
    }
}

// ------------------------------------------ what a stop is allowed to skip

/// **A long straight line with no back edge.** ADR 0024 decided this as the
/// sharpest difference two backends' fuel budgets could show, and a
/// difference in *outcome* rather than only in `fuel_spent`: the predecessor
/// charged a whole extent on arriving at a block's head, so a block whose
/// extent did not fit in what was left of the budget was refused entire —
/// none of the prefix that would have fitted ran — while the tree walk
/// charges nothing for straight-line work at all, so the same program
/// finished on it.
///
/// `Vm` has no block extent to charge at a head, because it has no
/// per-block accounting at all: fuel is added in fixed
/// `SAFEPOINT_STRIDE`-instruction batches, for instructions already run
/// rather than reserved for ones about to be, at a stride that falls where it
/// falls regardless of where a block begins or ends. So a straight line
/// longer than one stride is **not** refused whole here — it runs into the
/// next stride and is cut off mid-line.
///
/// That claim went with the backend that made it true, and nothing about the
/// language asked for it: ADR 0024 decided that a stop is a *bound* and said
/// in as many words that a fuel limit is not portable between backends, so
/// "the whole extent is charged, including the part that never ran" was
/// always a description of one mechanism rather than a rule about Cove. What
/// is a rule about Cove is what the two assertions below now state, and both
/// are ADR 0024's own: a run that exceeds its budget **stops**, reported as
/// having stopped for fuel, and what it spends past its limit is bounded —
/// here by one stride, because a stride is the largest run of instructions
/// that can happen between two answers to "may this continue?".
///
/// The tree walk half is unchanged and is the asymmetry ADR 0024 named: it
/// charges at safepoints, a straight line contains none, and the same limit
/// lets the same program finish.
#[test]
fn a_straight_line_is_cut_off_mid_line_and_the_tree_walk_finishes_it() {
    let mut source =
        String::from("use probe\n\nexport fn main() -> Result<Int, Error> {\n  var t = 0\n");
    // A thousand of them because the line has to outrun one
    // `SAFEPOINT_STRIDE`, and how many statements that takes depends on what
    // a statement lowers to: `t = t + 1` was three instructions before
    // `Inst::ArithImm` and is two after, so a count chosen against the old
    // shape stopped making the point. A thousand is comfortably past the
    // stride either way rather than exactly past it.
    for _ in 0..1000 {
        source.push_str("  t = t + 1\n");
    }
    source.push_str("  Ok(t)\n}\n");

    let whole = go(&source, Limits::default(), Backend::Vm);
    let extent = whole.fuel_spent;
    assert!(
        extent > SAFEPOINT_STRIDE,
        "the straight line has to be longer than one forced safepoint to make the point"
    );

    // Fuel for a good half of the line. On the deleted backend none of it
    // would have run: the extent was charged at the block's head and the
    // safepoint that followed the charge refused. `Vm` has no block head to
    // charge at, so it runs the prefix that fits and is cut off where the
    // stride falls.
    let limit = extent / 2;
    let half = go(
        &source,
        Limits {
            fuel: Some(limit),
            ..Limits::default()
        },
        Backend::Vm,
    );
    assert!(half.stopped, "a run past its fuel limit stops");
    assert_eq!(half.outcome, RunOutcome::Fuel);
    // The bound, which is what ADR 0024 asks for in place of a point: the run
    // spent at least what it was given — it stopped for fuel, so it reached
    // the limit — and no more than one stride past it, because a stride is
    // the longest it can go without asking.
    assert!(
        half.fuel_spent >= limit && half.fuel_spent <= limit + SAFEPOINT_STRIDE,
        "spent {} past a limit of {limit}, bound {}",
        half.fuel_spent,
        limit + SAFEPOINT_STRIDE
    );

    // The same limit on the tree walk, which charges per safepoint and
    // reaches none inside a straight line, so it answers.
    let walked = go(
        &source,
        Limits {
            fuel: Some(extent / 2),
            ..Limits::default()
        },
        Backend::Ast,
    );
    assert!(
        !walked.stopped,
        "the tree walk charges nothing for a straight line: {}",
        walked.answer
    );
    assert_eq!(walked.answer, "Ok(1000)");
}

/// **A Host call in a block whose budget will not cover it.** ADR 0030: no
/// Host call begins once the fuel a run has been charged has reached its
/// limit. The predecessor made that true with a dedicated hand-over at the
/// Host-call boundary — its own `charge_at_host_boundary` — that spent its
/// pending block charge before dispatching; the tree walk never held one, so
/// the property was already true of it. What the same limit *admitted*
/// differed between them by orders of magnitude, which was ADR 0024's "a fuel
/// limit is not portable between backends" and is why `max_host_calls` is
/// still the only control that bounds effects exactly.
///
/// `Vm` holds pending fuel by construction, because it charges on a fixed
/// `SAFEPOINT_STRIDE`, so it satisfies ADR 0030 the way that ADR allows a
/// backend which does: `Machine::charge_at_host_boundary` hands over what
/// this thread has dispatched and asks the budget before the call goes out.
/// Without it a Host call would be just another instruction the stride
/// counts, and a straight line of them shorter than one stride would not be
/// stopped at any fuel limit whatever. That is what b094d82 found — forty
/// effects under a limit of one — and the first assertion below is ADR 0030's
/// claim as written, on a line short enough that no periodic safepoint is
/// ever reached, so the boundary is the only thing that can refuse.
/// `docs/adr/0040-a-bound-outlives-its-backend.md` states the bound.
#[test]
fn no_host_call_begins_once_the_charged_fuel_has_reached_its_limit() {
    let mut source =
        String::from("use probe\n\nexport fn main() -> Result<Int, Error> {\n  var t = 0\n");
    for _ in 0..40 {
        source.push_str("  probe.tick()?\n  t = t + 1\n");
    }
    source.push_str("  Ok(t)\n}\n");

    // On the deleted backend, a limit of one was exhausted by the first
    // block's charge, and the first Host call in that block handed the
    // charge over and was refused before it was dispatched: zero, not forty.
    // `Vm` reaches no safepoint at all in this short a straight line, so
    // nothing refuses the first call, or any of the other thirty-nine.
    let vm = go(
        &source,
        Limits {
            fuel: Some(1),
            ..Limits::default()
        },
        Backend::Vm,
    );
    assert!(vm.stopped, "Vm runs out: {}", vm.answer);
    assert_eq!(vm.outcome, RunOutcome::Fuel);
    assert_eq!(
        vm.ticks, 0,
        "{} Host effects followed an exhausted fuel budget",
        vm.ticks
    );

    // The tree walk holds no pending fuel, so the property is already true of
    // it and there is nothing to hand over. What its schedule costs is the
    // other half: it reaches no safepoint between entering the entry and
    // returning from it, so a limit that lets it in at all lets all forty
    // through — and none of them is *after* exhaustion, because its charged
    // total does not move while the line runs.
    let walked = go(
        &source,
        Limits {
            fuel: Some(SAFEPOINT_FUEL + 1),
            ..Limits::default()
        },
        Backend::Ast,
    );
    assert!(!walked.stopped, "{}", walked.answer);
    assert_eq!(walked.ticks, 40);

    // Below its entry charge it never gets in, which is the other half of
    // the same fact: the tree walk's schedule is calls, and there are none
    // in a straight line.
    let refused = go(
        &source,
        Limits {
            fuel: Some(1),
            ..Limits::default()
        },
        Backend::Ast,
    );
    assert!(refused.stopped);
    assert_eq!(refused.ticks, 0);

    // `max_host_calls` does, exactly, and on both backends: it is charged per
    // call, before the call, which is the only schedule an effect can be
    // bounded on.
    for run in on_both(
        &source,
        Limits {
            max_host_calls: Some(7),
            ..Limits::default()
        },
    ) {
        assert!(run.stopped, "{:?}", run.backend);
        assert_eq!(run.outcome, RunOutcome::HostCalls, "{:?}", run.backend);
        assert_eq!(run.ticks, 7, "{:?}", run.backend);
    }
}

/// **A Host call the host re-entered Cove to reach.** ADR 0030 puts the
/// boundary at the two Host-call entry points, and a callback runs on the
/// *same* backend state — the same machine, the same accounting, the same
/// budget — so a Host call made from inside reentry is held to what one made
/// from the entry is held to. Issue #160 asked after this case separately,
/// because it is the one place the boundary is crossed twice.
///
/// The limit is measured rather than guessed: `max_host_calls` of one lets
/// the `probe.bounded` call through and refuses the first `probe.tick` inside
/// the callback, and what the run has spent at that moment is exactly the
/// fuel standing when the first Host effect of the reentry would be made. At
/// that limit no tick happens; one fuel above it, the callback gets past it.
///
/// **What one fuel above it buys differs between the two, and that is the
/// finding rather than a wrinkle.** The tree walk reaches no safepoint inside
/// a straight line, so its charged total does not move while the forty ticks
/// run and one fuel above the boundary buys every one of them. `Vm` hands
/// over what it has run at *every* Host call, so the total moves between two
/// ticks and one fuel above the boundary buys exactly one. The predecessor
/// sat with the tree walk here — it held a whole block's charge and handed
/// the same already-charged block over at each boundary in the block — so
/// this backend bounds effects by fuel **more tightly than either of the two
/// this test was written against**, which is a direction ADR 0030 wanted and
/// did not claim.
///
/// The exact count is therefore stated per evaluator, the way every other
/// per-backend figure in this file is. What is asserted of both is ADR 0030's
/// own claim — none at the boundary, and past it above the boundary — because
/// ADR 0024 already decided that a fuel limit is not portable between
/// backends and how much a limit *admits* is exactly the unportable part.
#[test]
fn a_host_call_inside_reentry_obeys_the_same_boundary() {
    let mut body = String::new();
    for _ in 0..40 {
        body.push_str("    probe.tick()\n");
    }
    let source = format!(
        "use probe\n\nexport fn main() -> Result<Int, Error> {{\n  let outcome = probe.bounded(fn() {{\n{body}  }})\n  Ok(0)\n}}\n"
    );

    for backend in [Backend::Ast, Backend::Vm] {
        // What the run has spent when the first Host call of the reentry is
        // about to be made, read off the one limit that refuses exactly
        // there.
        let probed = go(
            &source,
            Limits {
                max_host_calls: Some(1),
                ..Limits::default()
            },
            backend,
        );
        assert_eq!(probed.outcome, RunOutcome::HostCalls, "{backend:?}");
        assert_eq!(probed.reentries, 1, "{backend:?}: the host re-entered Cove");
        assert_eq!(probed.ticks, 0, "{backend:?}");
        let standing = probed.fuel_spent;
        assert!(
            standing > 0,
            "{backend:?}: reaching a callback costs fuel on both backends"
        );

        // At that limit the callback is entered and performs nothing.
        let refused = go(
            &source,
            Limits {
                fuel: Some(standing),
                ..Limits::default()
            },
            backend,
        );
        assert!(refused.stopped, "{backend:?}: {}", refused.answer);
        assert_eq!(refused.outcome, RunOutcome::Fuel, "{backend:?}");
        assert_eq!(
            refused.reentries, 1,
            "{backend:?}: the host re-entered Cove"
        );
        assert_eq!(
            refused.ticks, 0,
            "{backend:?}: {} Host effects were made inside a reentry under an \
             exhausted fuel budget",
            refused.ticks
        );
        // Every `probe` operation is declared an irreversible write, so the
        // boundary's own count is the check that no *further* one was made:
        // one for the `probe.bounded` call that got in, and none for the
        // forty inside it.
        assert_eq!(refused.irreversible_writes, 1, "{backend:?}");

        // One fuel above it the callback runs, which is what makes the line
        // above a boundary and not just a program that never got there.
        let admitted = go(
            &source,
            Limits {
                fuel: Some(standing + 1),
                ..Limits::default()
            },
            backend,
        );
        assert_eq!(admitted.reentries, 1, "{backend:?}");
        assert!(
            admitted.ticks >= 1,
            "{backend:?}: one fuel above the boundary the callback must get \
             past it, or the line above is a program that never arrived \
             rather than a boundary"
        );
        // How many *more* than one is the evaluator's schedule and not the
        // language's, and ADR 0024 is the decision that says so. The tree
        // walk reaches no safepoint inside a straight line, so its charged
        // total does not move and one fuel buys all forty. `Vm` charges for
        // the instructions already run at every Host call, so the fuel moves
        // between two ticks and the second one is refused.
        assert_eq!(
            admitted.ticks,
            match backend {
                Backend::Ast => 40,
                Backend::Vm => 1,
            },
            "{backend:?}"
        );
    }
}

/// [`a_stopped_loop_leaves_a_whole_number_of_turns_behind`]'s loop, turned
/// `{turns}` times and never raised, so [`fuel_per_turn`] can measure what one
/// turn of a `Shared.lock` call costs on each backend — the figure the bound
/// below is stated against.
const LOCKING_SPINNER: &str = "\
export fn main() -> Result<Int, Error> {
  let seen = Shared(0)
  var i = 0
  while i < {turns} {
    seen.lock(fn(var n) { n = n + 1 })
    i = i + 1
  }
  Ok(seen.lock(fn(n) { n }))
}
";

/// **A mutation immediately before and after a back edge.** A stop is taken
/// at a safepoint, and a safepoint stands between two instructions, so no
/// value is ever half written. What a caller can still see is the writes the
/// stopped body had already made to storage it shares — a `Shared` cell here
/// — and it sees a whole number of turns of them, never a torn one.
#[test]
fn a_stopped_loop_leaves_a_whole_number_of_turns_behind() {
    let source = "\
use probe

export fn main() -> Result<Int, Error> {
  let seen = Shared(0)
  let outcome = probe.bounded(fn() {
    var i = 0
    while i < 100000000 {
      seen.lock(fn(var n) { n = n + 1 })
      if i == 3 {
        probe.raise()
      }
      i = i + 1
    }
  })
  Ok(seen.lock(fn(n) { n }))
}
";
    for run in on_both(source, Limits::default()) {
        let backend = run.backend;
        let turns: i64 = run
            .answer
            .trim_start_matches("Ok(")
            .trim_end_matches(')')
            .parse()
            .unwrap_or_else(|_| panic!("{backend:?}: {}", run.answer));
        // Four turns were completed before the flag went up. What happens
        // next differs by backend: a call is an unconditional safepoint on
        // the tree walk, so it is asked every turn and stops at the very next
        // one. `Vm` has no safepoint of its own at a call or a back edge —
        // only the periodic one every `SAFEPOINT_STRIDE` instructions — so up
        // to one gathering's worth of further turns may run before it is
        // asked at all. The upper bound is measured from the loop's own
        // per-turn cost rather than guessed, because a maximum is the shape
        // that survives a constant being changed.
        let turn = fuel_per_turn(LOCKING_SPINNER, backend);
        let bound = 4 + gathering(backend).div_ceil(turn.max(1)) as i64;
        assert!(
            (4..=bound).contains(&turns),
            "{backend:?}: {turns} turns survived the stop, past the bound of {bound}"
        );
    }
}

// --------------------------------------------- pending fuel is never lost

/// **Pending fuel is never lost.** On the deleted backend, which charged a
/// block at a time and spent what it had charged at a safepoint, every way a
/// run could end without reaching another safepoint was a way its last charge
/// could go missing — and `fuel_spent` would report less work than the run
/// did. The invariant that caught it was that a run's `fuel_spent` was never
/// below the instructions it charged for, and it failed on every stopping
/// path without that backend's `spend_pending_fuel`: a run that raised
/// reported 0 fuel for 56 instructions before that existed.
///
/// `Vm` — the backend that replaced it, and which has since taken its name —
/// charges nothing between two safepoints, at a fixed `SAFEPOINT_STRIDE`, so
/// it holds a remainder for the same reason and needs the same flush. It has
/// one: `Machine::spend_pending_fuel`, at the end of a run and at the end of
/// every spawned task's thread, after the answer is settled so that a stop
/// raised there could not replace the reason the run actually ended. Without
/// it a run of a few dozen instructions would report zero, because
/// `machine.instructions()` counts every instruction dispatched and a
/// safepoint counts only whole strides. The cases below are the seven ways of
/// leaving without dispatching another instruction, and the invariant is
/// stated of all of them alike.
#[test]
fn a_run_never_reports_less_fuel_than_the_instructions_it_charged() {
    let cases: &[(&str, &str)] = &[
        (
            "a plain return",
            "use probe

export fn main() -> Result<Int, Error> {
  probe.tick()?
  Ok(1)
}
",
        ),
        (
            "a `?` that failed",
            "use probe

fn inner() -> Result<Int, Error> {
  Err(Error(\"no\"))
}

export fn main() -> Result<Int, Error> {
  probe.tick()?
  let n = inner()?
  Ok(n)
}
",
        ),
        (
            "a raised error",
            "use probe

export fn main() -> Result<Int, Error> {
  var i = 0
  var n = 0
  while i < 3 {
    n = n + i
    i = i + 1
  }
  Ok(n / 0)
}
",
        ),
        (
            "a cancelled run",
            "use probe

export fn main() -> Result<Int, Error> {
  var i = 0
  probe.cancelRun()?
  while i < 100000000 {
    i = i + 1
  }
  Ok(i)
}
",
        ),
        (
            "an exhausted budget",
            "use probe

export fn main() -> Result<Int, Error> {
  var i = 0
  while i < 100000000 {
    i = i + 1
  }
  Ok(i)
}
",
        ),
        (
            "a re-entrant callback the host abandoned",
            "use probe

export fn main() -> Result<Int, Error> {
  let outcome = probe.bounded(fn() {
    probe.raise()
    var i = 0
    while i < 100000000 {
      i = i + 1
    }
  })
  var j = 0
  while j < 20 {
    j = j + 1
  }
  Ok(j / 0)
}
",
        ),
        (
            "a cancelled task's own thread",
            "use probe

export fn main() -> Result<Int, Error> {
  scope s {
    let t = s.spawn {
      probe.hold()?
      var i = 0
      while i < 100000000 {
        i = i + 1
      }
      Ok(i)
    }
    probe.awaitArrival()?
    t.cancel()
    probe.release()?
  }
  Ok(0)
}
",
        ),
    ];
    for (what, source) in cases {
        let run = go(
            source,
            Limits {
                fuel: Some(50_000),
                ..Limits::default()
            },
            Backend::Vm,
        );
        let charged = run.instructions.expect("Vm counts instructions");
        assert!(
            run.fuel_spent >= charged,
            "{what}: {charged} instructions were charged for and only {} fuel was spent",
            run.fuel_spent
        );
    }
}

// ------------------------------------------------ what a stop is reported as

/// **Every stop mode reaches a terminal trace event, and the same one on
/// both backends.** A stop that a tool could not tell apart from a different
/// stop is a bound nobody can act on, so this is part of the contract rather
/// than a nicety.
///
/// The "call depth" case is worth naming, because b094d82 found `Vm` reading
/// no `max_call_depth` at all — `down(50)` under a limit of 8 answered
/// `Ok(0)` there instead of stopping — and this case is what found it.
/// `Machine::admit_frame` reads it now, at every frame pushed, and reports it
/// as `RunOutcome::CallDepth` rather than as the stack overflow that leaving
/// a task's stack segment is: one is a number an embedder chose and the other
/// is a fact about the memory the run was built with.
#[test]
fn every_stop_mode_is_reported_as_itself_on_both_backends() {
    let spinner = PURE_SPINNER.replace("{turns}", "100000000");
    let cancelled = SPINNER
        .replace("{arm}", "cancelRun")
        .replace("{turns}", "100000000");
    let cases: Vec<(&str, String, Limits)> = vec![
        (
            "fuel",
            spinner.clone(),
            Limits {
                fuel: Some(5_000),
                ..Limits::default()
            },
        ),
        (
            "deadline",
            spinner.clone(),
            Limits {
                deadline: Some(Duration::ZERO),
                ..Limits::default()
            },
        ),
        ("cancellation", cancelled, Limits::default()),
        (
            "host calls",
            "use probe

export fn main() -> Result<Unit, Error> {
  probe.tick()?
  probe.tick()?
  probe.tick()?
  Ok(())
}
"
            .to_string(),
            Limits {
                max_host_calls: Some(1),
                ..Limits::default()
            },
        ),
        (
            "call depth",
            "use probe

fn down(n: Int) -> Int {
  if n == 0 {
    0
  } else {
    down(n - 1)
  }
}

export fn main() -> Result<Int, Error> {
  Ok(down(50))
}
"
            .to_string(),
            Limits {
                max_call_depth: Some(8),
                ..Limits::default()
            },
        ),
    ];
    let expected = [
        ("fuel", RunOutcome::Fuel),
        ("deadline", RunOutcome::Deadline),
        ("cancellation", RunOutcome::Cancelled),
        ("host calls", RunOutcome::HostCalls),
        ("call depth", RunOutcome::CallDepth),
    ];
    for (what, source, limits) in cases {
        let want = expected
            .iter()
            .find(|(name, _)| *name == what)
            .expect("every case names an outcome")
            .1;
        for run in on_both(&source, limits.clone()) {
            assert!(run.stopped, "{what} on {:?}: {}", run.backend, run.answer);
            assert_eq!(
                run.outcome, want,
                "{what} on {:?} was reported as {:?}",
                run.backend, run.outcome
            );
        }
    }
}

/// **A host polling from inside a cancelled task is told so.**
///
/// [`Reentry::is_cancelled`] is documented as answering "everything a
/// safepoint in Cove code would answer to: the run's own cancellation, the
/// task's, and the flag of any bounded call this one is nested inside". The
/// predecessor's implementation asked two of the three: a `clock.every` timer
/// in a cancelled task ended cleanly on the tree walk and did not on it,
/// which made it the one question the two backends gave a host different
/// answers to. This is the case that failed, and `Vm` answers all three
/// correctly.
#[test]
fn a_host_polling_inside_a_cancelled_task_is_told_the_task_was_cancelled() {
    let source = "\
use probe

export fn main() -> Result<Int, Error> {
  scope s {
    let t = s.spawn {
      probe.watch()?
      Ok(0)
    }
    probe.awaitArrival()?
    t.cancel()
  }
  Ok(0)
}
";
    for run in on_both(source, Limits::default()) {
        assert_eq!(
            run.noticed_the_stop,
            Some(true),
            "{:?}: a host polling inside a cancelled task was not told",
            run.backend
        );
    }
}

// ------------------------------------- the run's budget, charged by tasks

/// The source both of the tests below run: four tasks doing identical,
/// bounded work at the same time, on four threads, all charging the one
/// budget ADR 0008 says the run has.
const FOUR_TASKS: &str = "\
fn work(n: Int) -> Int {
  var total = 0
  var i = 0
  while i < 4000 {
    total += i * n
    i += 1
  }
  total
}

export fn main() -> Result<Int, Error> {
  var total = 0
  scope s {
    let a = s.spawn { work(1) }
    let b = s.spawn { work(2) }
    let c = s.spawn { work(3) }
    let d = s.spawn { work(4) }
    total = a.await() + b.await() + c.await() + d.await()
  }
  Ok(total)
}
";

/// **Concurrent tasks charge the run's budget without losing a charge.**
///
/// ADR 0008 draws a task's fuel from the run's budget rather than giving each
/// task one of its own, so `fuel_spent` is the sum of what four threads did.
/// The work each of them does is fixed, so the sum is fixed too — however the
/// four are interleaved — and that is what makes this a test rather than a
/// measurement: a counter that lost an update under contention would report a
/// different total on a different afternoon.
///
/// Issue #182 is why it is written down. The budget's counters moved out from
/// behind the registry's mutex and became atomics, and "the mutex was not
/// protecting anything" is a claim about exactly this.
#[test]
fn four_tasks_charging_at_once_spend_the_same_fuel_every_time() {
    for backend in [Backend::Ast, Backend::Vm] {
        let first = go(FOUR_TASKS, Limits::default(), backend);
        assert!(!first.stopped, "{backend:?}: {}", first.answer);
        for _ in 0..7 {
            let again = go(FOUR_TASKS, Limits::default(), backend);
            assert_eq!(
                again.fuel_spent, first.fuel_spent,
                "{backend:?}: four tasks spent {} fuel and then {}",
                first.fuel_spent, again.fuel_spent
            );
            assert_eq!(again.answer, first.answer, "{backend:?}");
        }
    }
}

/// **A fuel limit bounds the run and not one task of it.**
///
/// The same four tasks under a limit that one of them alone would not reach:
/// the run stops, because the limit is the run's and every task spends it.
/// A budget whose counter were per-thread, or one that lost charges under
/// contention, would let this answer.
#[test]
fn four_tasks_at_once_are_stopped_by_the_run_s_fuel_limit() {
    for backend in [Backend::Ast, Backend::Vm] {
        let whole = go(FOUR_TASKS, Limits::default(), backend).fuel_spent;
        // Half of what the four together spend is more than any one of them
        // spends, so a limit here is one only the *run* reaches. ADR 0024
        // makes a fuel limit non-portable between backends, which is why it
        // is measured on each rather than written down.
        let run = go(
            FOUR_TASKS,
            Limits {
                fuel: Some(whole / 2),
                ..Limits::default()
            },
            backend,
        );
        assert!(run.stopped, "{backend:?}: {}", run.answer);
        assert_eq!(run.outcome, RunOutcome::Fuel, "{backend:?}");
    }
}
