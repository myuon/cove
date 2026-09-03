//! Runtime resource control.
//!
//! ADR 0001 makes termination and CPU usage runtime concerns rather than
//! properties the type system proves: "Totality, determinism, and
//! absence of loops are explicitly not MVP guarantees." This module is where
//! that decision becomes code. A [`Budget`] tracks one run against the
//! [`Limits`] a host chose, and the interpreter consults it at safepoints —
//! loop back edges, calls, and `await` — rather than at arbitrary points, so
//! the cost of enforcement is bounded and predictable.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::RuntimeError;
use crate::trace::RunOutcome;

/// The rule this module implements, quoted for every error it raises.
///
/// Visible to the crate because the limits are not all in one place: the
/// linear-memory backend's reserved stack region bounds how many tasks may
/// run at once as well, and a second wording of the same rule beside it would
/// be a second answer that could drift.
pub(crate) const RULE: &str =
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

/// One run's accounting: what it was limited to, when it started, and what
/// it has spent so far.
///
/// One allocation, reached by every thread of the run at once. ADR 0008 draws
/// a task's fuel from the run's budget rather than giving each task one of
/// its own, so there is exactly one of these per run however many tasks it
/// has, and every counter in it is an atomic rather than a field behind a
/// lock — see [`Meter`] for why that is the shape.
///
/// `limits`, `cancellation` and `started_at` do not change while a run lasts.
/// A run that starts over gets a fresh one of these rather than having this
/// one reset, which is what [`Budget::restart`] does and why `started_at` can
/// be a plain [`Instant`] read without synchronization.
#[derive(Debug)]
struct Accounting {
    limits: Limits,
    cancellation: Cancellation,
    started_at: Instant,
    fuel_spent: AtomicU64,
    host_calls: AtomicU64,
    /// How many safepoints have been taken while a deadline was set, which is
    /// what picks every [`DEADLINE_CHECK_INTERVAL`]th one to read the clock
    /// at. It counts up and is never reset: a counter that were reset would
    /// lose the increments of every thread that raced the reset, and how long
    /// a run may go without reading the clock is a bound ADR 0024 states.
    safepoints_under_deadline: AtomicU64,
    /// How many spawned tasks are alive right now: charged before a task is
    /// given a thread and released when the task that spawned it observes
    /// its end.
    live_tasks: AtomicU64,
}

/// One run's budget as a safepoint charges it: a handle every task thread can
/// hold at once, over counters that need no lock.
///
/// # Why this is not a `&mut Budget`
///
/// It used to be. [`crate::host::HostRegistry::with_budget`] locked a mutex,
/// handed the closure a `&mut Budget`, and unlocked — at every call and at
/// every return, because every call and every return is a safepoint. Issue
/// #182 measured what that cost: on `benches/call`, `with_budget` plus
/// `pthread_mutex_lock` plus `pthread_mutex_unlock` were 36% of the run
/// against the predecessor's `execute` at 46%.
///
/// The lock was not protecting anything that needed one. A safepoint adds to
/// `fuel_spent`, reads an atomic flag, compares against a limit fixed before
/// the run, and every so often reads a clock that started before the run.
/// None of that is a multi-field invariant two threads could tear; the
/// counters were plain integers because the struct holding them happened to
/// be reached by `&mut`, not because anything wanted them to be. So they are
/// atomics, this is the `&self` view of them, and the mutex is left to what
/// installs a budget and what reads the counters back.
///
/// # What is still the mutex's
///
/// [`crate::host::HostRegistry::with_budget`] still exists and still locks. It
/// is how a budget is installed, how `cove run --stats` reads what a run
/// spent, and how the charges that are not per-instruction are made — a host
/// call, a spawn, a task that ended. Every one of those is bounded by
/// something far more expensive than a lock, and moving them would have been
/// churn without a number behind it.
///
/// # Taking one, and restarts
///
/// A `Meter` names the accounting of the run it was taken from rather than
/// "whatever budget the registry holds now". `Budget::restart` gives its
/// budget fresh accounting, so a `Meter` taken before a restart charges the
/// run that ended. Both backends therefore take theirs where a run begins:
/// `Lvm::new` and `Interpreter::new` take one, and `invoke_within` and
/// `run_entry_within` take another immediately after installing the budget
/// they were handed. A registry's budget cannot be replaced by any other
/// route — `set_budget` needs `&mut HostRegistry` and a backend holds the
/// registry by shared reference for as long as it exists — so those are all
/// the places a stale one could come from.
#[derive(Clone, Debug)]
pub struct Meter {
    state: Arc<Accounting>,
}

/// Tracks one run against its [`Limits`].
///
/// A `Budget` is not `Clone`: it is one run's, and a second one would be a
/// second run. What is shared instead is [`Meter`], the view of the same
/// accounting that a safepoint charges through, and every task thread of the
/// run holds one — ADR 0008 draws a task's fuel from the run's budget rather
/// than giving each task one of its own, so there is still exactly one
/// authoritative count of what the run spent. Share a [`Cancellation`] when
/// another thread needs to stop the run.
///
/// `max_call_depth` is the one limit a budget does not itself enforce. Call
/// depth is a property of one stack, and with a thread per task there is a
/// stack per task, so the interpreter checks its own depth against
/// [`Limits::max_call_depth`]; counting every task's frames into one number
/// would stop a shallow task because a sibling was deep.
#[derive(Debug)]
pub struct Budget {
    meter: Meter,
}

impl Meter {
    /// Fresh accounting for a run bounded by `limits` and stopped by
    /// `cancellation`, with the deadline clock starting now.
    fn new(limits: Limits, cancellation: Cancellation) -> Self {
        Meter {
            state: Arc::new(Accounting {
                limits,
                cancellation,
                started_at: Instant::now(),
                fuel_spent: AtomicU64::new(0),
                host_calls: AtomicU64::new(0),
                safepoints_under_deadline: AtomicU64::new(0),
                live_tasks: AtomicU64::new(0),
            }),
        }
    }

    /// The limits the run was given, which do not change while it lasts.
    pub fn limits(&self) -> &Limits {
        &self.state.limits
    }

    /// Whether the run has been cancelled from outside.
    ///
    /// The *run's* flag, which every task of it shares. A task's own flag and
    /// a bounded call's belong to one thread, and `crate::interp::stopped_here`
    /// is where those two are read.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancellation.is_cancelled()
    }

    /// Checks cancellation, the deadline, and fuel in one call. Both backends
    /// call this at their safepoints. `fuel` is the cost of the work performed
    /// since the last one.
    ///
    /// The order the three questions are asked in is the whole of what a
    /// caller can observe about this, and it is the order they were asked in
    /// when a mutex was held across all three.
    pub fn safepoint(&self, fuel: u64) -> Result<(), Stopped> {
        // Counted before anything can refuse, because `fuel` is work the run
        // has already done and a stop does not un-do it. Reading the
        // cancellation flag first and returning would have thrown away
        // whatever the caller had gathered since its last safepoint, which
        // on a backend that charges in batches is most of what it did.
        // Nothing about *which* stop is reported moves: the limit is still
        // checked after the flag, so a cancelled run is still cancelled and
        // not out of fuel.
        let spent = self.add_fuel(fuel);
        if self.state.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }

        if let Some(limit) = self.state.limits.fuel {
            if spent >= limit {
                return Err(Stopped::Fuel);
            }
        }

        if let Some(deadline) = self.state.limits.deadline {
            // With no fuel limit nothing else bounds the run, so the clock is
            // read at every safepoint. Otherwise one safepoint in
            // `DEADLINE_CHECK_INTERVAL` reads it, chosen off a counter that
            // only ever counts up rather than one reset at each check: a reset
            // would discard whatever another task added between the check and
            // the reset, and this interval is a bound rather than a heuristic.
            let must_check_clock = self.state.limits.fuel.is_none()
                || self
                    .state
                    .safepoints_under_deadline
                    .fetch_add(1, Ordering::Relaxed)
                    % DEADLINE_CHECK_INTERVAL
                    == DEADLINE_CHECK_INTERVAL - 1;
            if must_check_clock && self.state.started_at.elapsed() >= deadline {
                return Err(Stopped::Deadline);
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
    pub fn spend(&self, fuel: u64) {
        self.add_fuel(fuel);
    }

    /// Adds `fuel` to the run's total and answers what the total is now.
    ///
    /// Saturating rather than wrapping, which is what it was when the total
    /// was a plain field behind a lock: a run that has spent more fuel than a
    /// `u64` can name has passed any limit that could have been set on it, and
    /// wrapping would hand it a fresh budget. The correction is a second store
    /// rather than a compare-and-swap loop because the branch is never taken,
    /// and a safepoint is not a place to pay for a case that cannot arise.
    fn add_fuel(&self, fuel: u64) -> u64 {
        let before = self.state.fuel_spent.fetch_add(fuel, Ordering::Relaxed);
        let after = before.wrapping_add(fuel);
        if after < before {
            self.state.fuel_spent.store(u64::MAX, Ordering::Relaxed);
            return u64::MAX;
        }
        after
    }

    /// Total fuel spent so far, for reporting.
    pub fn fuel_spent(&self) -> u64 {
        self.state.fuel_spent.load(Ordering::Relaxed)
    }

    /// Wall-clock time elapsed since the run started.
    pub fn elapsed(&self) -> Duration {
        self.state.started_at.elapsed()
    }

    /// Converts why execution stopped into a [`RuntimeError`] naming the
    /// limit and its configured value, quoting ADR 0001's position that these
    /// are runtime controls rather than termination proofs.
    pub fn to_runtime_error(&self, stopped: Stopped) -> RuntimeError {
        let message = match stopped {
            Stopped::Fuel => format!(
                "execution stopped: fuel budget of {} exhausted",
                self.state.limits.fuel.unwrap_or_default()
            ),
            Stopped::Deadline => format!(
                "execution stopped: wall-clock deadline of {:?} exceeded",
                self.state.limits.deadline.unwrap_or_default()
            ),
            Stopped::Cancelled => "execution stopped: the run was cancelled".to_string(),
            Stopped::CallDepth => format!(
                "execution stopped: call-depth limit of {} exceeded",
                self.state.limits.max_call_depth.unwrap_or_default()
            ),
            Stopped::HostCalls => format!(
                "execution stopped: host-call limit of {} exceeded",
                self.state.limits.max_host_calls.unwrap_or_default()
            ),
            Stopped::Concurrency => format!(
                "execution stopped: concurrency limit of {} task(s) exceeded, with {} already running",
                self.state.limits.max_tasks.unwrap_or_default(),
                self.state.live_tasks.load(Ordering::Relaxed),
            ),
        };
        RuntimeError::new(message)
            .with_rule(RULE)
            .with_outcome(stopped.outcome())
    }
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
            meter: Meter::new(limits, cancellation),
        }
    }

    /// This run's accounting, in the handle a safepoint charges through.
    ///
    /// A caller that will charge more than once holds on to what this
    /// answers: taking one costs an `Arc` clone, and charging through one
    /// costs no lock at all. [`Meter`] says where each backend takes its own
    /// and why that is where a run begins.
    pub fn meter(&self) -> Meter {
        self.meter.clone()
    }

    /// The cancellation flag for this run. Clone and hand it to whoever may
    /// need to cancel the run from another thread.
    pub fn cancellation(&self) -> Cancellation {
        self.meter.state.cancellation.clone()
    }

    /// Starts this budget over, for the run that is about to begin.
    ///
    /// Every count goes back to zero and the deadline clock starts again from
    /// now. That is the answer to the one question a per-invocation limit
    /// raises that a per-run one does not: a `Budget` starts its clock when it
    /// is built, and a budget built to bound an invocation that has not begun
    /// would spend its deadline waiting for its turn. The deadline runs from
    /// the invocation, so this is called as the invocation is entered and
    /// nowhere else — [`crate::host::HostRegistry::begin_run`] is the only
    /// caller.
    ///
    /// The [`Cancellation`] is *not* reset, and that is not an oversight. A
    /// flag somebody raised stays raised: the handle is shared, whoever
    /// cancelled did so on purpose, and a run that quietly un-cancelled itself
    /// as it started would be a stop this crate promised and did not make.
    /// A caller that wants a fresh flag builds a fresh budget with one.
    ///
    /// It is fresh accounting rather than counters written back to zero,
    /// because zeroing counters a running task might still be charging is a
    /// race with no answer — while a [`Meter`] handed out for the previous run
    /// keeps charging the run it belongs to, which is the only thing it could
    /// truthfully do. `begin_run` is the only caller and it holds the budget
    /// alone at that moment, so nothing is charging this one either way; what
    /// the shape buys is that a mistake about that would be a stale number in
    /// a finished run's report rather than a torn one in a live run's limit.
    pub(crate) fn restart(&mut self) {
        self.meter = Meter::new(
            self.meter.state.limits.clone(),
            self.meter.state.cancellation.clone(),
        );
    }

    /// The limits this budget was constructed with.
    pub fn limits(&self) -> &Limits {
        self.meter.limits()
    }

    /// Checks cancellation, the deadline, and fuel in one call. The
    /// interpreter calls this at safepoints: loop back edges, calls, and
    /// `await`. `fuel` is the cost of the work performed since the last
    /// safepoint.
    ///
    /// [`Meter::safepoint`] is the whole of it. A backend on a per-instruction
    /// path holds a [`Meter`] and calls that instead of reaching a `Budget`
    /// through the registry's lock; this is here for a caller that has a
    /// `Budget` in hand and charges once.
    pub fn safepoint(&self, fuel: u64) -> Result<(), Stopped> {
        self.meter.safepoint(fuel)
    }

    /// Adds `fuel` to the run's total without asking whether the run may
    /// continue. [`Meter::spend`] says when that is what a backend wants.
    pub fn spend(&self, fuel: u64) {
        self.meter.spend(fuel);
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
    pub fn charge_host_call(&self) -> Result<(), Stopped> {
        let state = &self.meter.state;
        if state.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }
        if let Some(deadline) = state.limits.deadline {
            if state.started_at.elapsed() >= deadline {
                return Err(Stopped::Deadline);
            }
        }
        let made = state
            .host_calls
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if let Some(limit) = state.limits.max_host_calls {
            if made > limit {
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
    ///
    /// The check and the taking are one step, so two `spawn`s racing for the
    /// last place cannot both be told there is one. That used to be the
    /// registry's mutex; it is this compare-and-swap now, which holds however
    /// this is reached.
    pub fn charge_task(&self) -> Result<(), Stopped> {
        let live = &self.meter.state.live_tasks;
        match self.meter.state.limits.max_tasks {
            Some(limit) => live
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                    (live < limit).then(|| live + 1)
                })
                .map(|_| ())
                .map_err(|_| Stopped::Concurrency),
            None => {
                live.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// Forgets a task whose end has been observed, so its place is free
    /// again.
    ///
    /// A task ends by finishing, by failing, by being cancelled, or by
    /// breaking an invariant in its own thread, and all four reach the caller
    /// as a join. Releasing anywhere else would make this a limit on how many
    /// tasks a run may spawn in total rather than on how many it may hold at
    /// once.
    pub fn release_task(&self) {
        let _ = self.meter.state.live_tasks.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |live| Some(live.saturating_sub(1)),
        );
    }

    /// How many spawned tasks are alive right now: what the concurrency
    /// limit bounds, and what a stop reports.
    pub fn live_tasks(&self) -> u64 {
        self.meter.state.live_tasks.load(Ordering::Relaxed)
    }

    /// Total fuel spent so far, for reporting.
    pub fn fuel_spent(&self) -> u64 {
        self.meter.fuel_spent()
    }

    /// Total host calls charged so far, including any that were then
    /// rejected for exceeding the limit, for reporting.
    pub fn host_calls(&self) -> u64 {
        self.meter.state.host_calls.load(Ordering::Relaxed)
    }

    /// Wall-clock time elapsed since the budget was created.
    pub fn elapsed(&self) -> Duration {
        self.meter.elapsed()
    }

    /// Converts why execution stopped into a [`RuntimeError`] naming the
    /// limit and its configured value, quoting ADR 0001's position that these
    /// are runtime controls rather than termination proofs.
    pub fn to_runtime_error(&self, stopped: Stopped) -> RuntimeError {
        self.meter.to_runtime_error(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn fuel_limit_fires_when_exhausted() {
        let budget = Budget::new(Limits {
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
        let budget = Budget::new(Limits::default());
        for _ in 0..1_000 {
            assert_eq!(budget.safepoint(u64::MAX / 2000), Ok(()));
        }
    }

    #[test]
    fn deadline_fires_when_exceeded() {
        let budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.safepoint(0), Err(Stopped::Deadline));
    }

    #[test]
    fn deadline_absent_never_stops() {
        let budget = Budget::new(Limits::default());
        thread::sleep(Duration::from_millis(5));
        assert_eq!(budget.safepoint(0), Ok(()));
    }

    #[test]
    fn deadline_alone_is_observed_on_the_first_safepoint() {
        // With no fuel limit, the clock must be consulted every call, not
        // merely every `DEADLINE_CHECK_INTERVAL`th one.
        let budget = Budget::new(Limits {
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
        let budget = Budget::new(Limits {
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
        let budget = Budget::new(Limits::default());
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

        assert_eq!(budget.safepoint(0), Err(Stopped::Cancelled));
    }

    /// The deadline bounds a run whose work is host calls, which reaches no
    /// loop back edge, no Cove call, and no `await` to be stopped at.
    #[test]
    fn the_deadline_also_stops_host_call_charging() {
        let budget = Budget::new(Limits {
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
        let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
        cancellation.cancel();
        assert_eq!(budget.charge_host_call(), Err(Stopped::Cancelled));
    }

    #[test]
    fn the_concurrency_limit_fires_on_the_spawn_that_would_pass_it() {
        let budget = Budget::new(Limits {
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
        let budget = Budget::new(Limits {
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
        let budget = Budget::new(Limits::default());
        budget.release_task();
        assert_eq!(budget.live_tasks(), 0);
    }

    #[test]
    fn concurrency_limit_absent_never_stops() {
        let budget = Budget::new(Limits::default());
        for _ in 0..1_000 {
            assert_eq!(budget.charge_task(), Ok(()));
        }
    }

    /// The concurrency diagnostic has the same shape as the memory one: it
    /// names the limit that was configured, says what the run was holding,
    /// and cites the rule.
    #[test]
    fn the_concurrency_diagnostic_names_the_limit_and_what_is_running() {
        let budget = Budget::new(Limits {
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

    /// Every task of a run charges the one budget, so what it reports is the
    /// sum of what they all did and not the last writer's share of it. A
    /// counter that were read, added to, and written back would lose most of
    /// this; a `fetch_add` loses none of it.
    #[test]
    fn nothing_is_lost_when_every_thread_charges_at_once() {
        const THREADS: u64 = 8;
        const EACH: u64 = 20_000;

        let budget = Arc::new(Budget::new(Limits::default()));
        let meters: Vec<_> = (0..THREADS).map(|_| budget.meter()).collect();
        let handles: Vec<_> = meters
            .into_iter()
            .map(|meter| {
                thread::spawn(move || {
                    for _ in 0..EACH {
                        assert_eq!(meter.safepoint(1), Ok(()));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(budget.fuel_spent(), THREADS * EACH);
    }

    /// The fuel limit bounds the *run*, so it is the total across every task
    /// that reaches it, and every task that asks after it has been reached is
    /// told so. ADR 0008 draws a task's fuel from the run's budget and this is
    /// what that means when the tasks are actually concurrent.
    #[test]
    fn a_fuel_limit_stops_every_thread_that_shares_the_run() {
        const THREADS: u64 = 8;
        const LIMIT: u64 = 10_000;

        let budget = Arc::new(Budget::new(Limits {
            fuel: Some(LIMIT),
            ..Limits::default()
        }));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| budget.meter())
            .map(|meter| {
                thread::spawn(move || {
                    // Every thread runs until the run refuses it, which it
                    // must: the limit is the run's, so one thread spending it
                    // stops the others too.
                    let mut charged = 0u64;
                    loop {
                        charged += 1;
                        if meter.safepoint(1) == Err(Stopped::Fuel) {
                            return charged;
                        }
                        assert!(charged <= LIMIT, "a thread outran the run's whole budget");
                    }
                })
            })
            .collect();
        let charged: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // Nothing is spent twice and nothing is dropped: what the budget
        // reports is exactly what the threads between them charged.
        assert_eq!(budget.fuel_spent(), charged);
        assert!(budget.fuel_spent() >= LIMIT);
    }

    /// A place under the concurrency limit is taken by one `spawn` or the
    /// other and never by both. The mutex the registry holds used to make the
    /// check and the taking one step; this holds without it, which is what
    /// lets a `spawn` be charged from wherever a `spawn` happens.
    #[test]
    fn two_spawns_racing_for_the_last_place_cannot_both_take_it() {
        const THREADS: u64 = 8;
        const LIMIT: u64 = 3;

        for _ in 0..20 {
            let budget = Arc::new(Budget::new(Limits {
                max_tasks: Some(LIMIT),
                ..Limits::default()
            }));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| Arc::clone(&budget))
                .map(|budget| thread::spawn(move || budget.charge_task().is_ok()))
                .collect();
            let taken = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|took| *took)
                .count() as u64;
            assert_eq!(taken, LIMIT);
            assert_eq!(budget.live_tasks(), LIMIT);
        }
    }

    /// `max_host_calls` bounds what a run does to the outside world, which
    /// ADR 0024 makes the control that bounds effects exactly. A call counted
    /// twice or not at all on one thread would make that bound a guess.
    #[test]
    fn every_host_call_is_counted_once_however_many_threads_make_them() {
        const THREADS: u64 = 8;
        const EACH: u64 = 5_000;

        let budget = Arc::new(Budget::new(Limits::default()));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| Arc::clone(&budget))
            .map(|budget| {
                thread::spawn(move || {
                    for _ in 0..EACH {
                        assert_eq!(budget.charge_host_call(), Ok(()));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(budget.host_calls(), THREADS * EACH);
    }

    /// Fuel saturates rather than wrapping, so a run that has spent more than
    /// a `u64` can name cannot come back under a limit it has passed.
    #[test]
    fn fuel_saturates_rather_than_wrapping() {
        let budget = Budget::new(Limits {
            fuel: Some(u64::MAX),
            ..Limits::default()
        });
        budget.spend(u64::MAX - 1);
        assert_eq!(budget.safepoint(1_000), Err(Stopped::Fuel));
        assert_eq!(budget.fuel_spent(), u64::MAX);
    }

    /// A restart is fresh accounting rather than counters written back to
    /// zero, so a [`Meter`] taken before one keeps charging the run it was
    /// taken from. Both backends take theirs where a run begins for exactly
    /// this reason, and this is the fact they are relying on.
    #[test]
    fn a_meter_taken_before_a_restart_belongs_to_the_run_that_ended() {
        let mut budget = Budget::new(Limits::default());
        let before = budget.meter();
        before.spend(100);
        assert_eq!(budget.fuel_spent(), 100);

        budget.restart();
        assert_eq!(budget.fuel_spent(), 0);

        before.spend(7);
        assert_eq!(budget.fuel_spent(), 0, "the new run is charged nothing");
        assert_eq!(before.fuel_spent(), 107, "the old run kept its own total");

        budget.meter().spend(7);
        assert_eq!(budget.fuel_spent(), 7);
    }

    /// A restart keeps the flag for the reason `restart` gives, and it keeps
    /// it through the fresh accounting: a run cancelled before it started is
    /// still cancelled.
    #[test]
    fn a_restart_keeps_the_cancellation_it_was_built_with() {
        let cancellation = Cancellation::new();
        let mut budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
        cancellation.cancel();
        budget.restart();
        assert_eq!(budget.safepoint(0), Err(Stopped::Cancelled));
        assert!(budget.cancellation().is_cancelled());
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
