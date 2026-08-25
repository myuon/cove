//! Runtime resource control.
//!
//! ADR 0001 makes termination, CPU usage, and memory usage runtime concerns
//! rather than properties the type system proves: "Totality, determinism, and
//! absence of loops are explicitly not MVP guarantees." This module is where
//! that decision becomes code. A [`Budget`] tracks one run against the
//! [`Limits`] a host chose, and the interpreter consults it at safepoints —
//! loop back edges, calls, and `await` — rather than at arbitrary points, so
//! the cost of enforcement is bounded and predictable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::RuntimeError;

/// The rule this module implements, quoted for every error it raises.
const RULE: &str =
    "ADR 0001: CPU, memory, time, and host-call limits are runtime controls, not termination proofs.";

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
const DEADLINE_CHECK_INTERVAL: u64 = 64;

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
        if self.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }

        self.fuel_spent = self.fuel_spent.saturating_add(fuel);
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

    /// Charges one host call against the budget, failing before the call is
    /// dispatched if it would exceed `max_host_calls`.
    pub fn charge_host_call(&mut self) -> Result<(), Stopped> {
        if self.cancellation.is_cancelled() {
            return Err(Stopped::Cancelled);
        }
        self.host_calls += 1;
        if let Some(limit) = self.limits.max_host_calls {
            if self.host_calls > limit {
                return Err(Stopped::HostCalls);
            }
        }
        Ok(())
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
        };
        RuntimeError::new(message).with_rule(RULE)
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
        };
        RuntimeError::new(message).with_rule(RULE)
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

    #[test]
    fn cancellation_also_stops_host_call_charging() {
        let cancellation = Cancellation::new();
        let mut budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
        cancellation.cancel();
        assert_eq!(budget.charge_host_call(), Err(Stopped::Cancelled));
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
