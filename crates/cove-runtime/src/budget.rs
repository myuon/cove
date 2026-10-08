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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::error::RuntimeError;
use crate::trace::RunOutcome;
use crate::wallclock::Instant;

/// The rule this module implements, quoted for every error it raises.
///
/// Visible to the crate because the limits are not all in one place: the
/// linear-memory backend's reserved stack region bounds how many tasks may
/// run at once as well, and a second wording of the same rule beside it would
/// be a second answer that could drift.
pub(crate) const RULE: &str =
    "ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.";

/// Limits a host imposes on one run.
///
/// A `None` field imposes nothing: `Limits::default()` never stops a run.
///
/// There is no work allowance among them.
/// [ADR 0091](../../../docs/adr/0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md)
/// removed the fuel budget: a host bounds a run's time with
/// [`Limits::deadline`] or stops it with a [`Cancellation`].
#[derive(Clone, Debug, Default)]
pub struct Limits {
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
    /// a scheduling policy and ADR 0008 has none. Like the host-call limit,
    /// it bounds the *run*: every task alive anywhere in it counts, so
    /// a program cannot stay under the limit by spreading its tasks over more
    /// scopes.
    pub max_tasks: Option<u64>,
}

/// Why execution was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
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
    /// wants to know which one: a cancelled run and a run past its deadline
    /// are not the same report, however alike the two stops look from inside
    /// the budget.
    pub fn outcome(self) -> RunOutcome {
        match self {
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
///
/// # Being told
///
/// A running run notices at its next safepoint, which reads the flag. A run
/// that is not running — parked at a host call, or yielded and waiting in a
/// queue — reads nothing, and the embedder holding it is the one that has to
/// notice and call [`ParkedVm::cancel`](crate::ParkedVm::cancel). So the flag
/// also tells whoever asked: [`Cancellation::on_cancel`] runs a callback when
/// it is raised, which is how an async host wakes the task awaiting the run
/// (store a `Waker`, wake it in the callback), and
/// [`Cancellation::wait_timeout`] blocks a thread until it is raised. Neither
/// costs a safepoint anything: the flag it reads is the same one word
/// ([ADR 0088](../../../docs/adr/0088-an-embedder-sizes-the-heap-and-is-told-of-a-cancellation.md)).
#[derive(Clone, Default)]
pub struct Cancellation(Arc<Flag>);

/// What every clone of one [`Cancellation`] shares.
#[derive(Default)]
struct Flag {
    /// The flag a safepoint reads: one load, with nothing else on the path.
    raised: AtomicBool,
    /// Who is waiting to be told, and whether they have been.
    waiting: Mutex<Waiting>,
    /// Signalled once, when the flag is raised, for
    /// [`Cancellation::wait_timeout`].
    told: Condvar,
}

/// The callbacks registered and not yet run, and whether the flag has been
/// raised under the lock — which is what decides whether a callback
/// registered now runs now or later.
#[derive(Default)]
struct Waiting {
    raised: bool,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cancellation")
            .field(&self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    /// A fresh, not-yet-cancelled flag.
    pub fn new() -> Self {
        Cancellation::default()
    }

    /// Requests cancellation. Idempotent: cancelling twice is the same as
    /// cancelling once.
    ///
    /// The first call runs every [`Cancellation::on_cancel`] callback, on
    /// this thread, after the flag is raised and outside any lock, and wakes
    /// every [`Cancellation::wait_timeout`].
    pub fn cancel(&self) {
        self.0.raised.store(true, Ordering::SeqCst);
        let callbacks = {
            let mut waiting = self.waiting();
            if waiting.raised {
                return;
            }
            waiting.raised = true;
            std::mem::take(&mut waiting.callbacks)
        };
        self.0.told.notify_all();
        for callback in callbacks {
            callback();
        }
    }

    /// Whether [`Cancellation::cancel`] has been called on this flag or any
    /// clone of it.
    pub fn is_cancelled(&self) -> bool {
        self.0.raised.load(Ordering::SeqCst)
    }

    /// Runs `callback` once, when the flag is raised: on the thread that
    /// raises it, or on this one, now, if it already has been.
    ///
    /// A callback is kept until the flag is raised or the last clone is
    /// dropped, and there is no taking one back: register once per waiter,
    /// not once per poll. It should be short — a `Waker::wake`, a send on a
    /// channel — because the thread cancelling runs it before `cancel`
    /// returns.
    pub fn on_cancel(&self, callback: impl FnOnce() + Send + 'static) {
        {
            let mut waiting = self.waiting();
            if !waiting.raised {
                waiting.callbacks.push(Box::new(callback));
                return;
            }
        }
        callback();
    }

    /// Blocks until the flag is raised or `timeout` has passed, and answers
    /// whether it was raised.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (waiting, _) = self
            .0
            .told
            .wait_timeout_while(self.waiting(), timeout, |waiting| !waiting.raised)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        waiting.raised
    }

    fn waiting(&self) -> MutexGuard<'_, Waiting> {
        self.0
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One run's accounting: what it was limited to, when it started, and the
/// host calls and tasks it has taken so far.
///
/// One allocation, reached by every thread of the run at once. ADR 0008 makes
/// a task's limits the run's rather than giving each task some of its own,
/// so there is exactly one of these per run however many tasks it has, and
/// every counter in it is an atomic rather than a field behind a lock — see
/// [`Meter`] for why that is the shape.
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
    host_calls: AtomicU64,
    /// Whether a test has made the deadline pass now, rather than waiting for
    /// the clock: [`Meter::expire_for_test`].
    #[cfg(test)]
    expired: AtomicBool,
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
/// The lock was not protecting anything that needed one. A safepoint reads an
/// atomic flag and a clock that started before the run, and a host call adds
/// to a counter and compares it against a limit fixed before the run.
/// None of that is a multi-field invariant two threads could tear; the
/// counters were plain integers because the struct holding them happened to
/// be reached by `&mut`, not because anything wanted them to be. So they are
/// atomics, this is the `&self` view of them, and the mutex is left to what
/// installs a budget and what reads the counters back.
///
/// # What is still the mutex's
///
/// [`crate::host::HostRegistry::with_budget`] still exists and still locks. It
/// is how a budget installed by `set_budget` is read back — how `cove run
/// --stats` reads the host calls a run made. It used to be how the charges that are
/// not per-instruction were made as well — a host call, a spawn, a task that
/// ended — and they are this type's now, for a reason that is not speed.
///
/// # A meter is the run's, not the registry's
///
/// A registry is shared by every run over it, at once and from many threads
/// (an [`OwnedVm`](crate::OwnedVm)'s registry is an `Arc`). When the budget
/// of an invocation was installed *in* the registry, it was one slot every
/// concurrent run charged its host calls to and replaced on entry, and runs
/// were stopped for each other's calls: issue #577 measured 997 of 10,000
/// concurrent requests refused for a host-call limit none of them had
/// reached. So the meter is carried by the run — the backend holds it, every
/// task thread is handed a clone, and a host call is charged to the meter
/// of the run that made it.
///
/// # Taking one, and restarts
///
/// A `Meter` names the accounting of the run it was taken from rather than
/// "whatever budget the registry holds now". `Budget::restart` gives its
/// budget fresh accounting, so a `Meter` taken before a restart charges the
/// run that ended. Both backends therefore take theirs where a run begins:
/// `Vm::new` and `Interpreter::new` take the one `set_budget` installed, if
/// any, and `invoke_within` and `run_entry_within` take the one of the budget
/// they were handed, after restarting it. A registry's budget cannot be
/// replaced by any other route — `set_budget` needs `&mut HostRegistry` and
/// a backend holds the registry by shared reference for as long as it
/// exists — so those are all the places a stale one could come from.
#[derive(Clone, Debug)]
pub struct Meter {
    state: Arc<Accounting>,
}

/// Tracks one run against its [`Limits`].
///
/// A `Budget` is not `Clone`: it is one run's, and a second one would be a
/// second run. What is shared instead is [`Meter`], the view of the same
/// accounting that a safepoint asks through, and every task thread of the
/// run holds one — ADR 0008 makes a task's limits the run's rather than
/// giving each task some of its own, so there is still exactly one
/// authoritative count of what the run took. Share a [`Cancellation`] when
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
                host_calls: AtomicU64::new(0),
                #[cfg(test)]
                expired: AtomicBool::new(false),
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

    /// The run's [`Cancellation`], shared: what a host holding a parked or
    /// yielded run registers [`Cancellation::on_cancel`] on, without having
    /// kept the one it built the [`Budget`] with.
    pub fn cancellation(&self) -> Cancellation {
        self.state.cancellation.clone()
    }

    /// Checks cancellation and then the deadline. Both backends call this at
    /// their safepoints, and the encoded machine at a Host-call boundary too.
    ///
    /// The clock is read at every call while a deadline is set, because
    /// nothing else bounds how much work a run does
    /// ([ADR 0091](../../../docs/adr/0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md)).
    /// The backends call this once per stride of work rather than per
    /// instruction, which is what keeps that affordable.
    ///
    /// The order is the whole of what a caller can observe about this: a run
    /// that was cancelled and is past its deadline is reported cancelled.
    pub fn safepoint(&self) -> Result<(), Stopped> {
        self.interrupted().map_or(Ok(()), Err)
    }

    /// Charges one host call against the run, failing before the call is
    /// dispatched if the run was cancelled, if its deadline has passed, or if
    /// the call would exceed `max_host_calls`.
    ///
    /// A host call is a control point exactly as a safepoint is. ADR 0003
    /// puts the controls at "loop back edges, calls, and `await`", and a run
    /// whose work is waiting on a host reaches none of the other three: a
    /// deadline checked only in Cove code would not bound a program that
    /// spends its time inside calls. The clock is read on every call, because
    /// a host call already costs far more than reading it does.
    pub fn charge_host_call(&self) -> Result<(), Stopped> {
        let state = &self.state;
        if state.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }
        if self.past_deadline() {
            return Err(Stopped::Deadline);
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
    /// already held. A refusal stops the run the way a deadline does; a
    /// `spawn` that waited for a sibling to finish would be a scheduler, and
    /// ADR 0008 deliberately has no scheduling policy.
    ///
    /// The check and the taking are one step, so two `spawn`s racing for the
    /// last place cannot both be told there is one. That used to be the
    /// registry's mutex; it is this compare-and-swap now, which holds however
    /// this is reached.
    pub fn charge_task(&self) -> Result<(), Stopped> {
        let live = &self.state.live_tasks;
        match self.state.limits.max_tasks {
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
        let _ = self
            .state
            .live_tasks
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                Some(live.saturating_sub(1))
            });
    }

    /// How many spawned tasks are alive right now: what the concurrency
    /// limit bounds, and what a stop reports.
    pub fn live_tasks(&self) -> u64 {
        self.state.live_tasks.load(Ordering::Relaxed)
    }

    /// Total host calls charged so far, including any that were then
    /// rejected for exceeding the limit, for reporting.
    pub fn host_calls(&self) -> u64 {
        self.state.host_calls.load(Ordering::Relaxed)
    }

    /// Wall-clock time elapsed since the run started.
    pub fn elapsed(&self) -> Duration {
        self.state.started_at.elapsed()
    }

    /// What the run's deadline leaves, or `None` for a run with no deadline.
    ///
    /// Zero once the deadline has passed, never a wrapped figure: the
    /// subtraction saturates. The clock started when the run did and has not
    /// stopped since — a run parked at a host call is still running — so this
    /// is what a scheduler holding a parked run sets its timer by
    /// ([ADR 0082](../../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)).
    pub fn time_left(&self) -> Option<Duration> {
        let deadline = self.state.limits.deadline?;
        Some(deadline.saturating_sub(self.state.started_at.elapsed()))
    }

    /// The stop a run that is not executing has already reached, if any:
    /// [`Stopped::Cancelled`] if its flag is raised, [`Stopped::Deadline`] if
    /// its deadline has passed, and `None` otherwise.
    ///
    /// The two questions [`Meter::safepoint`] asks, in the order it asks
    /// them. The host-call limit and the concurrency limit are not among
    /// them: each is charged for work, and a run that is not executing is
    /// doing none. This is what a parked run is asked as it is resumed
    /// ([ADR 0082](../../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)).
    pub fn interrupted(&self) -> Option<Stopped> {
        if self.state.cancellation.is_cancelled() {
            return Some(Stopped::Cancelled);
        }
        self.past_deadline().then_some(Stopped::Deadline)
    }

    /// Whether the run has a deadline and it has passed.
    fn past_deadline(&self) -> bool {
        let Some(deadline) = self.state.limits.deadline else {
            return false;
        };
        #[cfg(test)]
        if self.state.expired.load(Ordering::Relaxed) {
            return true;
        }
        self.state.started_at.elapsed() >= deadline
    }

    /// Makes a run that has a deadline past it from now on, as if the clock
    /// had reached it: the instrument this crate's bound tests raise a
    /// deadline with at a point they choose.
    #[cfg(test)]
    pub(crate) fn expire_for_test(&self) {
        self.state.expired.store(true, Ordering::Relaxed);
    }

    /// Converts why execution stopped into a [`RuntimeError`] naming the
    /// limit and its configured value, quoting ADR 0001's position that these
    /// are runtime controls rather than termination proofs.
    pub fn to_runtime_error(&self, stopped: Stopped) -> RuntimeError {
        let message = match stopped {
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
    /// nowhere else — `invoke_within` and its siblings, on both backends, are
    /// the only callers.
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
    /// truthfully do. Every caller holds the budget alone at that moment —
    /// it was handed over by value — so nothing is charging this one either way; what
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

    /// Checks cancellation and the deadline. The interpreter calls this at
    /// safepoints: loop back edges, calls, and `await`.
    ///
    /// [`Meter::safepoint`] is the whole of it. A backend on a per-instruction
    /// path holds a [`Meter`] and calls that instead of reaching a `Budget`
    /// through the registry's lock; this is here for a caller that has a
    /// `Budget` in hand and asks once.
    pub fn safepoint(&self) -> Result<(), Stopped> {
        self.meter.safepoint()
    }

    /// Charges one host call against the budget. [`Meter::charge_host_call`]
    /// is the whole of it.
    pub fn charge_host_call(&self) -> Result<(), Stopped> {
        self.meter.charge_host_call()
    }

    /// Charges one task against the concurrency limit.
    /// [`Meter::charge_task`] is the whole of it.
    pub fn charge_task(&self) -> Result<(), Stopped> {
        self.meter.charge_task()
    }

    /// Forgets a task whose end has been observed. [`Meter::release_task`]
    /// is the whole of it.
    pub fn release_task(&self) {
        self.meter.release_task();
    }

    /// How many spawned tasks are alive right now.
    pub fn live_tasks(&self) -> u64 {
        self.meter.live_tasks()
    }

    /// Total host calls charged so far, including any that were then
    /// rejected for exceeding the limit, for reporting.
    pub fn host_calls(&self) -> u64 {
        self.meter.host_calls()
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

    /// A callback registered before the flag is raised runs once, on the
    /// raising thread, however many times it is raised; one registered after
    /// runs at once, on the registering thread.
    #[test]
    fn a_cancellation_tells_each_callback_once() {
        let cancellation = Cancellation::new();
        let runs = Arc::new(AtomicU64::new(0));
        for _ in 0..3 {
            let runs = Arc::clone(&runs);
            cancellation.on_cancel(move || {
                runs.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let clone = cancellation.clone();
        thread::spawn(move || {
            clone.cancel();
            clone.cancel();
        })
        .join()
        .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let late = Arc::clone(&runs);
        cancellation.on_cancel(move || {
            late.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(runs.load(Ordering::SeqCst), 4, "ran at once");
        cancellation.cancel();
        assert_eq!(runs.load(Ordering::SeqCst), 4, "and not again");
    }

    /// A callback may itself touch the flag — cancelling, registering — since
    /// it runs outside the lock.
    #[test]
    fn a_cancellation_callback_may_use_the_flag() {
        let cancellation = Cancellation::new();
        let inner = cancellation.clone();
        let (told, heard) = std::sync::mpsc::channel();
        cancellation.on_cancel(move || {
            inner.cancel();
            inner.on_cancel(move || told.send(()).unwrap());
        });
        cancellation.cancel();
        heard
            .try_recv()
            .expect("registered from inside, and run at once");
    }

    /// `wait_timeout` answers `false` when the time passes first and `true`
    /// once another thread raises the flag.
    #[test]
    fn a_thread_waits_for_a_cancellation() {
        let cancellation = Cancellation::new();
        assert!(!cancellation.wait_timeout(Duration::from_millis(5)));
        let clone = cancellation.clone();
        let waiter = thread::spawn(move || clone.wait_timeout(Duration::from_secs(30)));
        cancellation.cancel();
        assert!(waiter.join().unwrap());
        assert!(cancellation.wait_timeout(Duration::ZERO), "already raised");
    }

    #[test]
    fn deadline_fires_when_exceeded() {
        let budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.safepoint(), Err(Stopped::Deadline));
    }

    #[test]
    fn deadline_absent_never_stops() {
        let budget = Budget::new(Limits::default());
        thread::sleep(Duration::from_millis(5));
        assert_eq!(budget.safepoint(), Ok(()));
    }

    #[test]
    fn deadline_alone_is_observed_on_the_first_safepoint() {
        // The clock is consulted at every call, not merely at some of them.
        let budget = Budget::new(Limits {
            deadline: Some(Duration::from_millis(1)),
            ..Limits::default()
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(budget.safepoint(), Err(Stopped::Deadline));
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

        assert_eq!(budget.safepoint(), Err(Stopped::Cancelled));
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

    /// A restart is fresh accounting rather than counters written back to
    /// zero, so a [`Meter`] taken before one keeps charging the run it was
    /// taken from. Both backends take theirs where a run begins for exactly
    /// this reason, and this is the fact they are relying on.
    #[test]
    fn a_meter_taken_before_a_restart_belongs_to_the_run_that_ended() {
        let mut budget = Budget::new(Limits::default());
        let before = budget.meter();
        assert_eq!(before.charge_host_call(), Ok(()));
        assert_eq!(budget.host_calls(), 1);

        budget.restart();
        assert_eq!(budget.host_calls(), 0);

        assert_eq!(before.charge_host_call(), Ok(()));
        assert_eq!(budget.host_calls(), 0, "the new run is charged nothing");
        assert_eq!(before.host_calls(), 2, "the old run kept its own total");

        assert_eq!(budget.meter().charge_host_call(), Ok(()));
        assert_eq!(budget.host_calls(), 1);
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
        assert_eq!(budget.safepoint(), Err(Stopped::Cancelled));
        assert!(budget.cancellation().is_cancelled());
    }

    #[test]
    fn to_runtime_error_names_the_configured_value() {
        let budget = Budget::new(Limits {
            max_host_calls: Some(42),
            ..Limits::default()
        });
        let error = budget.to_runtime_error(Stopped::HostCalls);
        assert!(error.message.contains("42"), "{}", error.message);
        assert!(error.rule.is_some());
    }
}
