//! Runtime resource control.
//!
//! ADR 0001 makes termination and CPU usage runtime concerns rather than
//! properties the type system proves: "Totality, determinism, and
//! absence of loops are explicitly not MVP guarantees." This module is where
//! that decision becomes code. A [`Budget`] tracks one run against the
//! [`Limits`] a host chose, and the interpreter consults it at safepoints —
//! loop back edges, calls, and `await` — rather than at arbitrary points, so
//! the cost of enforcement is bounded and predictable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::RuntimeError;
use crate::trace::RunOutcome;

/// The rule this module implements, quoted for every error it raises.
const RULE: &str =
    "ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.";

/// How many [`Budget::safepoint`] calls pass between checks of the wall clock
/// when a deadline is set.
///
/// `Instant::now()` reads a monotonic clock, which on most platforms is a
/// system call or vDSO trap — several orders of magnitude slower than
/// decrementing an integer. Checking it at every safepoint would tax
/// fuel-heavy loops for a bound that fuel usually enforces anyway. Every
/// [`DEADLINE_CHECK_INTERVAL`]th call keeps the wasted overrun bounded to a
/// small, fixed number of safepoints without paying the clock's cost on every
/// one. When no fuel limit is set, nothing else bounds the run, so the clock
/// is consulted on every call regardless of this constant.
pub const DEADLINE_CHECK_INTERVAL: u64 = 64;

/// Limits a host imposes on one run.
///
/// A `None` field imposes nothing: `Limits::default()` never stops a run.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// The total fuel a run may spend before it is stopped.
    pub fuel: Option<u64>,
    /// The wall-clock duration a run may take before it is stopped.
    pub deadline: Option<Duration>,
    /// The total number of host calls a run may make before it is stopped.
    pub max_host_calls: Option<u64>,
    /// The deepest a call may nest before it is stopped.
    pub max_call_depth: Option<usize>,
    /// The tasks a run may have alive at once before it is stopped.
    ///
    /// ADR 0001 lists concurrency limits beside CPU and time, and a
    /// thread is the one resource a program can take without asking for it.
    /// So this limit is charged where the taking happens: `spawn` charges it
    /// before a thread exists, and a `spawn` past the limit stops the run
    /// rather than waiting for a sibling to finish, because waiting would be
    /// a scheduling policy and ADR 0008 has none. Like fuel and host calls,
    /// it bounds the *run*: every task alive anywhere in it counts, so
    /// a program cannot stay under the limit by spreading its tasks over more
    /// scopes.
    pub max_tasks: Option<u64>,
}

/// Why execution was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The fuel budget was exhausted.
    Fuel,
    /// The wall-clock deadline was exceeded.
    Deadline,
    /// The run was cancelled from outside.
    Cancelled,
    /// The call-depth limit was exceeded.
    CallDepth,
    /// The host-call limit was exceeded.
    HostCalls,
    /// A `spawn` would have left more tasks alive at once than the
    /// concurrency limit allows.
    Concurrency,
}

impl Stopped {
    /// How a run stopped this way is classified in its terminal trace event.
    ///
    /// One [`RunOutcome`] per [`Stopped`], because each of these is a
    /// different control and a reader deciding what to do about a stopped run
    /// wants to know which one: a run out of fuel and a run past its deadline
    /// are not the same report, however alike the two stops look from inside
    /// the budget.
    pub fn outcome(self) -> RunOutcome {
        match self {
            Stopped::Fuel => RunOutcome::Fuel,
            Stopped::Deadline => RunOutcome::Deadline,
            Stopped::Cancelled => RunOutcome::Cancelled,
            Stopped::CallDepth => RunOutcome::CallDepth,
            Stopped::HostCalls => RunOutcome::HostCalls,
            Stopped::Concurrency => RunOutcome::Concurrency,
        }
    }
}

/// A cancellation flag shared with whoever may cancel the run.
///
/// Cloning shares the same underlying flag: cancelling one handle cancels
/// every clone, including ones already handed to a [`Budget`].
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// A fresh, not-yet-cancelled flag.
    pub fn new() -> Self {
        Cancellation(Arc::new(AtomicBool::new(false)))
    }

    /// Requests cancellation. Idempotent: cancelling twice is the same as
    /// cancelling once.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether [`Cancellation::cancel`] has been called on this flag or any
    /// clone of it.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Tracks one run against its [`Limits`].
///
/// A `Budget` is not `Clone`: it holds the one authoritative count of fuel
/// spent and host calls made for a single run, and ADR 0008 draws a task's
/// fuel from the run's budget rather than giving each task one of its own, so
/// every task thread charges this one through
/// [`crate::host::HostRegistry::with_budget`]. Share a [`Cancellation`]
/// instead when another thread needs to stop the run.
///
/// `max_call_depth` is the one limit a budget does not itself enforce. Call
/// depth is a property of one stack, and with a thread per task there is a
/// stack per task, so the interpreter checks its own depth against
/// [`Limits::max_call_depth`]; counting every task's frames into one number
/// would stop a shallow task because a sibling was deep.
pub struct Budget {
    limits: Limits,
    cancellation: Cancellation,
    started_at: Instant,
    fuel_spent: u64,
    host_calls: u64,
    safepoints_since_deadline_check: u64,
    /// How many spawned tasks are alive right now: charged before a task is
    /// given a thread and released when the task that spawned it observes
    /// its end.
    live_tasks: u64,
}

impl Budget {
    /// Tracks a run against `limits`, starting the deadline clock now.
    pub fn new(limits: Limits) -> Self {
        Budget::with_cancellation(limits, Cancellation::new())
    }

    /// Tracks a run against `limits`, using a [`Cancellation`] the caller
    /// already holds a handle to, so it can be cancelled from elsewhere.
    pub fn with_cancellation(limits: Limits, cancellation: Cancellation) -> Self {
        Budget {
            limits,
            cancellation,
            started_at: Instant::now(),
            fuel_spent: 0,
            host_calls: 0,
            safepoints_since_deadline_check: 0,
            live_tasks: 0,
        }
    }

    /// The cancellation flag for this run. Clone and hand it to whoever may
    /// need to cancel the run from another thread.
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    /// The limits this budget was constructed with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Checks cancellation, the deadline, and fuel in one call. The
    /// interpreter calls this at safepoints: loop back edges, calls, and
    /// `await`. `fuel` is the cost of the work performed since the last
    /// safepoint.
    pub fn safepoint(&mut self, fuel: u64) -> Result<(), Stopped> {
        // Counted before anything can refuse, because `fuel` is work the run
        // has already done and a stop does not un-do it. Reading the
        // cancellation flag first and returning would have thrown away
        // whatever the caller had gathered since its last safepoint, which
        // on a backend that charges in batches is most of what it did.
        // Nothing about *which* stop is reported moves: the limit is still
        // checked after the flag, so a cancelled run is still cancelled and
        // not out of fuel.
        self.fuel_spent = self.fuel_spent.saturating_add(fuel);
        if self.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }

        if let Some(limit) = self.limits.fuel {
            if self.fuel_spent >= limit {
                return Err(Stopped::Fuel);
            }
        }

        if let Some(deadline) = self.limits.deadline {
            self.safepoints_since_deadline_check += 1;
            let must_check_clock = self.limits.fuel.is_none()
                || self.safepoints_since_deadline_check >= DEADLINE_CHECK_INTERVAL;
            if must_check_clock {
                self.safepoints_since_deadline_check = 0;
                if self.started_at.elapsed() >= deadline {
                    return Err(Stopped::Deadline);
                }
            }
        }

        Ok(())
    }

    /// Adds `fuel` to the run's total without asking whether the run may
    /// continue.
    ///
    /// A backend that charges fuel in batches holds some between two
    /// safepoints, and the safepoint is where that holding is spent. A run
    /// that ends anywhere else — by raising, by being stopped, by a task
    /// thread finishing — reaches no further safepoint, so what it had
    /// gathered would simply not be counted, and `fuel_spent` would report
    /// less work than the run did. This is where the last of it is put back,
    /// and it decides nothing: the run is already over, and a second stop
    /// raised here would be answering a question nobody asked.
    pub fn spend(&mut self, fuel: u64) {
        self.fuel_spent = self.fuel_spent.saturating_add(fuel);
    }

    /// Charges one host call against the budget, failing before the call is
    /// dispatched if the run was cancelled, if its deadline has passed, or if
    /// the call would exceed `max_host_calls`.
    ///
    /// A host call is a control point exactly as a safepoint is. ADR 0003
    /// puts the controls at "loop back edges, calls, and `await`", and a run
    /// whose work is waiting on a host reaches none of the other three: a
    /// deadline checked only in Cove code would not bound a program that
    /// spends its time inside calls. The clock is read on every call rather
    /// than every `DEADLINE_CHECK_INTERVAL`th, because a host call already
    /// costs far more than reading it does.
    pub fn charge_host_call(&mut self) -> Result<(), Stopped> {
        if self.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }
        if let Some(deadline) = self.limits.deadline {
            if self.started_at.elapsed() >= deadline {
                return Err(Stopped::Deadline);
            }
        }
        self.host_calls += 1;
        if let Some(limit) = self.limits.max_host_calls {
            if self.host_calls > limit {
                return Err(Stopped::HostCalls);
            }
        }
        Ok(())
    }

    /// Charges one task against the concurrency limit, refusing it before it
    /// is given a thread if the run already holds as many tasks as it may.
    ///
    /// Every other limit stops a run for work it has already done. This one
    /// refuses work that has not started, because a thread is taken rather
    /// than spent: by the time a safepoint could observe it, the resource is
    /// already held. A refusal stops the run the way exhausted fuel does; a
    /// `spawn` that waited for a sibling to finish would be a scheduler, and
    /// ADR 0008 deliberately has no scheduling policy.
    pub fn charge_task(&mut self) -> Result<(), Stopped> {
        if let Some(limit) = self.limits.max_tasks {
            if self.live_tasks >= limit {
                return Err(Stopped::Concurrency);
            }
        }
        self.live_tasks += 1;
        Ok(())
    }

    /// Forgets a task whose end has been observed, so its place is free
    /// again.
    ///
    /// A task ends by finishing, by failing, by being cancelled, or by
    /// breaking an invariant in its own thread, and all four reach the caller
    /// as a join. Releasing anywhere else would make this a limit on how many
    /// tasks a run may spawn in total rather than on how many it may hold at
    /// once.
    pub fn release_task(&mut self) {
        self.live_tasks = self.live_tasks.saturating_sub(1);
    }

    /// How many spawned tasks are alive right now: what the concurrency
    /// limit bounds, and what a stop reports.
    pub fn live_tasks(&self) -> u64 {
        self.live_tasks
    }

    /// Total fuel spent so far, for reporting.
    pub fn fuel_spent(&self) -> u64 {
        self.fuel_spent
    }

    /// Total host calls charged so far, including any that were then
    /// rejected for exceeding the limit, for reporting.
    pub fn host_calls(&self) -> u64 {
        self.host_calls
    }

    /// Wall-clock time elapsed since the budget was created.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Converts why execution stopped into a [`RuntimeError`] naming the
    /// limit and its configured value, quoting ADR 0001's position that these
    /// are runtime controls rather than termination proofs.
    pub fn to_runtime_error(&self, stopped: Stopped) -> RuntimeError {
        let message = match stopped {
            Stopped::Fuel => format!(
                "execution stopped: fuel budget of {} exhausted",
                self.limits.fuel.unwrap_or_default()
            ),
            Stopped::Deadline => format!(
                "execution stopped: wall-clock deadline of {:?} exceeded",
                self.limits.deadline.unwrap_or_default()
            ),
            Stopped::Cancelled => "execution stopped: the run was cancelled".to_string(),
            Stopped::CallDepth => format!(
                "execution stopped: call-depth limit of {} exceeded",
                self.limits.max_call_depth.unwrap_or_default()
            ),
            Stopped::HostCalls => format!(
                "execution stopped: host-call limit of {} exceeded",
                self.limits.max_host_calls.unwrap_or_default()
            ),
            Stopped::Concurrency => format!(
                "execution stopped: concurrency limit of {} task(s) exceeded, with {} already running",
                self.limits.max_tasks.unwrap_or_default(),
                self.live_tasks,
            ),
        };
        RuntimeError::new(message)
            .with_rule(RULE)
            .with_outcome(stopped.outcome())
    }
}

impl From<Stopped> for RuntimeError {
    /// A context-free conversion for callers with no [`Budget`] at hand. It
    /// names the limit but not its configured value; prefer
    /// [`Budget::to_runtime_error`] when a budget is available, since it can
    /// quote the value that was configured.
    fn from(stopped: Stopped) -> Self {
        let message = match stopped {
            Stopped::Fuel => "execution stopped: fuel budget exhausted",
            Stopped::Deadline => "execution stopped: wall-clock deadline exceeded",
            Stopped::Cancelled => "execution stopped: the run was cancelled",
            Stopped::CallDepth => "execution stopped: call-depth limit exceeded",
            Stopped::HostCalls => "execution stopped: host-call limit exceeded",
            Stopped::Concurrency => "execution stopped: concurrency limit exceeded",
        };
        RuntimeError::new(message)
            .with_rule(RULE)
            .with_outcome(stopped.outcome())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn fuel_limit_fires_when_exhausted() {
        let mut budget = Budget::new(Limits {
            fuel: Some(10),
            ..Limits::default()
        });
        assert_eq!(budget.safepoint(5), Ok(()));
        assert_eq!(budget.safepoint(4), Ok(()));
        assert_eq!(budget.safepoint(1), Err(Stopped::Fuel));
        assert_eq!(budget.fuel_spent(), 10);
    }

    #[test]
    fn fuel_limit_absent_never_stops() {
        let mut budget = Budget::new(Limits::default());
        for _ in 0..1_000 {
            assert_eq!(budget.safepoint(u64::MAX / 2000), Ok(()));
        }
    }

    #[test]
    fn deadline_fires_when_exceeded() {
        let mut budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.safepoint(0), Err(Stopped::Deadline));
    }

    #[test]
    fn deadline_absent_never_stops() {
        let mut budget = Budget::new(Limits::default());
        thread::sleep(Duration::from_millis(5));
        assert_eq!(budget.safepoint(0), Ok(()));
    }

    #[test]
    fn deadline_alone_is_observed_on_the_first_safepoint() {
        // With no fuel limit, the clock must be consulted every call, not
        // merely every `DEADLINE_CHECK_INTERVAL`th one.
        let mut budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.safepoint(0), Err(Stopped::Deadline));
    }

    #[test]
    fn the_call_depth_limit_is_reported_but_not_counted_here() {
        // Depth belongs to one stack and a task has a stack of its own, so
        // the interpreter counts frames and the budget only carries the
        // limit and names it in the error.
        let budget = Budget::new(Limits {
            max_call_depth: Some(2),
            ..Limits::default()
        });
        assert_eq!(budget.limits().max_call_depth, Some(2));
        assert_eq!(
            budget.to_runtime_error(Stopped::CallDepth).message,
            "execution stopped: call-depth limit of 2 exceeded"
        );
    }

    #[test]
    fn max_host_calls_fires_when_exceeded() {
        let mut budget = Budget::new(Limits {
            max_host_calls: Some(2),
            ..Limits::default()
        });
        assert_eq!(budget.charge_host_call(), Ok(()));
        assert_eq!(budget.charge_host_call(), Ok(()));
        assert_eq!(budget.charge_host_call(), Err(Stopped::HostCalls));
        assert_eq!(budget.host_calls(), 3);
    }

    #[test]
    fn max_host_calls_absent_never_stops() {
        let mut budget = Budget::new(Limits::default());
        for _ in 0..1_000 {
            assert_eq!(budget.charge_host_call(), Ok(()));
        }
    }

    #[test]
    fn cancellation_from_another_thread_stops_the_run() {
        let budget = Budget::new(Limits::default());
        let cancellation = budget.cancellation();
        let handle = thread::spawn(move || {
            cancellation.cancel();
        });
        handle.join().unwrap();

        let mut budget = budget;
        assert_eq!(budget.safepoint(0), Err(Stopped::Cancelled));
    }

    /// The deadline bounds a run whose work is host calls, which reaches no
    /// loop back edge, no Cove call, and no `await` to be stopped at.
    #[test]
    fn the_deadline_also_stops_host_call_charging() {
        let mut budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        assert_eq!(budget.charge_host_call(), Ok(()));
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.charge_host_call(), Err(Stopped::Deadline));
        // A call refused for the deadline is not one the run made.
        assert_eq!(budget.host_calls(), 1);
    }

    #[test]
    fn cancellation_also_stops_host_call_charging() {
        let cancellation = Cancellation::new();
        let mut budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
        cancellation.cancel();
        assert_eq!(budget.charge_host_call(), Err(Stopped::Cancelled));
    }

    #[test]
    fn the_concurrency_limit_fires_on_the_spawn_that_would_pass_it() {
        let mut budget = Budget::new(Limits {
            max_tasks: Some(2),
            ..Limits::default()
        });
        assert_eq!(budget.charge_task(), Ok(()));
        assert_eq!(budget.charge_task(), Ok(()));
        assert_eq!(budget.charge_task(), Err(Stopped::Concurrency));
        // A refused task is not one the run holds: the limit refuses work
        // before it starts rather than counting work that did.
        assert_eq!(budget.live_tasks(), 2);
    }

    /// The limit bounds the tasks alive at once, not the tasks a run spawns
    /// over its life: a run that ends each task before starting the next may
    /// start as many as it likes.
    #[test]
    fn a_task_that_ended_frees_its_place_for_the_next_one() {
        let mut budget = Budget::new(Limits {
            max_tasks: Some(1),
            ..Limits::default()
        });
        for _ in 0..1_000 {
            assert_eq!(budget.charge_task(), Ok(()));
            budget.release_task();
        }
        assert_eq!(budget.live_tasks(), 0);
    }

    /// Releasing more tasks than were charged cannot lend a run capacity it
    /// never had, whatever a caller does.
    #[test]
    fn releasing_a_task_that_was_never_charged_frees_nothing() {
        let mut budget = Budget::new(Limits::default());
        budget.release_task();
        assert_eq!(budget.live_tasks(), 0);
    }

    #[test]
    fn concurrency_limit_absent_never_stops() {
        let mut budget = Budget::new(Limits::default());
        for _ in 0..1_000 {
            assert_eq!(budget.charge_task(), Ok(()));
        }
    }

    /// The concurrency diagnostic has the same shape as the memory one: it
    /// names the limit that was configured, says what the run was holding,
    /// and cites the rule.
    #[test]
    fn the_concurrency_diagnostic_names_the_limit_and_what_is_running() {
        let mut budget = Budget::new(Limits {
            max_tasks: Some(4),
            ..Limits::default()
        });
        for _ in 0..4 {
            assert_eq!(budget.charge_task(), Ok(()));
        }
        assert_eq!(budget.charge_task(), Err(Stopped::Concurrency));
        let error = budget.to_runtime_error(Stopped::Concurrency);
        assert!(
            error.message.contains("concurrency limit of 4 task(s)"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("4 already running"),
            "{}",
            error.message
        );
        assert!(error.rule.is_some());
    }

    #[test]
    fn to_runtime_error_names_the_configured_value() {
        let budget = Budget::new(Limits {
            fuel: Some(42),
            ..Limits::default()
        });
        let error = budget.to_runtime_error(Stopped::Fuel);
        assert!(error.message.contains('4') && error.message.contains('2'));
        assert!(error.rule.is_some());
    }
}
