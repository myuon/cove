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
//! is where that is decided; `docs/VM_ARCHITECTURE.md` states each bound in
//! prose.
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
use cove_runtime::vm::{Vm, BACK_EDGE_FUEL, SAFEPOINT_INTERVAL};
use cove_runtime::{
    Budget, Cancellation, Effect, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, Reentry, Runtime, RuntimeError, Value,
};
use cove_sema::resolve::Program as Checked;
use cove_sema::{Compiler, Config, Module, Package, Unit};

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
        Ok(Value::ok(Value::Unit))
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
            return Ok(Value::ok(Value::Unit));
        }
        if op != "bounded" {
            return self.call(op, args);
        }
        // `clock.timeout` in miniature, and deliberately the same shape: a
        // flag raised while the body runs, the body stopped at its next
        // safepoint, and the host turning that stop into the answer it
        // promised rather than letting it end the run.
        let stop = Cancellation::new();
        self.bounds.lock().unwrap().push(stop.clone());
        let outcome = back.call_until(&args[0], Vec::new(), &stop);
        self.bounds.lock().unwrap().pop();
        match outcome {
            Ok(_) => Ok(Value::ok(Value::Unit)),
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
    /// What the VM charged for, which is `None` on the tree walk because a
    /// tree walk has no instructions.
    instructions: Option<u64>,
    /// Irreversible Host effects, counted by the boundary rather than by the
    /// host.
    irreversible_writes: u64,
    ticks: u64,
    /// Host effects performed after a stop was armed.
    ticks_after_arming: u64,
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
            let program = match cove_ir::lower::lower(&checked) {
                Ok(program) => Arc::new(program),
                Err(why) => panic!("the program lowers, but stopped at {why}"),
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
/// `{arm}` called once before it. No call inside the body, so the only
/// safepoint a turn reaches is its back edge — which is the case
/// `BACK_EDGE_FUEL` gates and therefore the case a bound has to be stated
/// for.
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
/// The deadline and `max_host_calls` are read at every Host call as well as
/// at a safepoint, so a program with a `probe` call in it would be stopped
/// there rather than on the schedule under test. This is the loop the two
/// deadline cases and the fuel case measure, for that reason.
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
/// first one — before its first instruction, on the VM.
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
        // the one it took, and the VM has charged nothing at all, because
        // entering the entry is a safepoint and it is taken before the first
        // block is charged.
        let bound = match backend {
            Backend::Ast => SAFEPOINT_FUEL,
            Backend::Vm => 0,
        };
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
/// plus one turn on both backends — with the difference that the VM's
/// gathering is fuel it charged in advance and the tree walk's is a single
/// safepoint's fixed charge.
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
/// On the VM that is [`BACK_EDGE_FUEL`], read at a back edge. On the tree
/// walk every back edge is a safepoint, so nothing gathers and the figure is
/// the one safepoint's own charge.
fn gathering(backend: Backend) -> u64 {
    match backend {
        Backend::Ast => SAFEPOINT_FUEL,
        Backend::Vm => BACK_EDGE_FUEL,
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

/// **A long straight line with no back edge.** The VM charges a whole extent
/// on arriving at its head, so a block whose extent does not fit in what is
/// left of the budget is refused entire — none of the prefix that would have
/// fitted runs. The tree walk charges nothing for straight-line work at all,
/// so the same program finishes on it.
///
/// This is the sharpest difference between the two backends' budgets, and it
/// is a difference in *outcome* rather than only in `fuel_spent`: one stops
/// and one answers. ADR 0024 is where that is decided rather than discovered.
#[test]
fn a_straight_line_is_refused_whole_by_the_vm_and_charged_for_by_neither_half() {
    let mut source =
        String::from("use probe\n\nexport fn main() -> Result<Int, Error> {\n  var t = 0\n");
    for _ in 0..400 {
        source.push_str("  t = t + 1\n");
    }
    source.push_str("  Ok(t)\n}\n");

    let whole = go(&source, Limits::default(), Backend::Vm);
    let extent = whole.fuel_spent;
    assert!(
        extent > SAFEPOINT_INTERVAL,
        "the straight line has to be longer than one forced safepoint to make the point"
    );

    // Fuel for a good half of the line, and none of it runs: the extent is
    // charged at its head and the safepoint that follows the charge refuses.
    let half = go(
        &source,
        Limits {
            fuel: Some(extent / 2),
            ..Limits::default()
        },
        Backend::Vm,
    );
    assert!(half.stopped, "the VM refuses a block it cannot afford");
    assert_eq!(half.outcome, RunOutcome::Fuel);
    assert_eq!(
        half.fuel_spent, extent,
        "the whole extent is charged, including the part that never ran"
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
    assert_eq!(walked.answer, "Ok(400)");
}

/// **A Host call at the end of a block whose budget will not cover it.**
/// Nothing between the head of a block and its end asks about fuel, so every
/// Host call in a straight line is made before the budget that the line
/// exhausts is measured. That is the bound, and it is `SAFEPOINT_INTERVAL`
/// plus one block rather than zero — which is why `max_host_calls` and not
/// fuel is the limit that bounds *effects*.
#[test]
fn host_calls_in_one_straight_line_all_happen_before_the_budget_is_measured() {
    let mut source =
        String::from("use probe\n\nexport fn main() -> Result<Int, Error> {\n  var t = 0\n");
    for _ in 0..40 {
        source.push_str("  probe.tick()?\n  t = t + 1\n");
    }
    source.push_str("  Ok(t)\n}\n");

    // The VM: a limit of one is exhausted by the first block's charge, and
    // every Host call in that block is made before that charge is measured
    // at the return.
    let vm = go(
        &source,
        Limits {
            fuel: Some(1),
            ..Limits::default()
        },
        Backend::Vm,
    );
    assert!(vm.stopped, "the VM runs out: {}", vm.answer);
    assert_eq!(vm.outcome, RunOutcome::Fuel);
    assert_eq!(
        vm.ticks, 40,
        "fuel does not bound the effects inside one straight line"
    );

    // The tree walk reaches no safepoint between entering the entry and
    // returning from it, so once a limit lets it in at all it cannot be
    // stopped inside the line — the forty happen and it answers.
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
        // Four turns were completed before the flag went up, and both
        // backends stop at the very next `lock` — a call is an unconditional
        // safepoint, so a loop whose body calls anything is asked every turn
        // and the gathered back-edge schedule never comes into it. Four is
        // what both measure; the range is what is asserted, because a
        // maximum is the shape that survives a constant being changed.
        assert!(
            (4..=6).contains(&turns),
            "{backend:?}: {turns} turns survived the stop"
        );
    }
}

// --------------------------------------------- pending fuel is never lost

/// **Pending fuel is never lost.** The VM charges a block at a time and
/// spends what it has charged at a safepoint, so every way a run can end
/// without reaching another safepoint is a way its last charge could go
/// missing — and `fuel_spent` would report less work than the run did.
///
/// The invariant that catches it is that a VM run's `fuel_spent` is never
/// below the instructions it charged for. It fails on every stopping path
/// without `Vm::spend_pending_fuel`: a run that raised reported 0 fuel for 56
/// instructions before it existed.
#[test]
fn a_vm_run_never_reports_less_fuel_than_the_instructions_it_charged() {
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
        let charged = run.instructions.expect("the VM counts instructions");
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
/// VM's implementation asked two of the three: a `clock.every` timer in a
/// cancelled task ended cleanly on the tree walk and did not on the VM, which
/// made it the one question the two backends gave a host different answers
/// to. This is the case that failed.
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
