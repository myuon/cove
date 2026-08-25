//! `clock`: monotonic time, and waiting.
//!
//! The Language Card lists the clock among the operations that are typed Host
//! APIs rather than ambient authority, and says a host "may provide real,
//! fake, filtered, remote, or denied implementations". This module is where
//! that becomes two implementations of one Host API: [`Clock::real`] reads the
//! platform's monotonic clock, and [`Clock::virtual_clock`] reads a counter
//! that moves only when the host moves it. Cove code cannot tell them apart,
//! which is what makes a program that observes time testable.
//!
//! Time is a `Duration` since an origin the host picks, never a wall-clock
//! date. A `Duration` subtracts, so `clock.now() - startedAt` is the elapsed
//! time of a piece of work, and no program can accidentally depend on the
//! origin itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cove_sema::Capability;

use crate::budget::Cancellation;
use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry};
use crate::schema::{ModuleSchema, OperationSchema};
use crate::value::Value;

/// How often a watchdog looks at the work it is bounding.
///
/// A timeout is a bound, not a stopwatch, so the granularity only decides how
/// long past the bound a body may run before its next safepoint sees the
/// flag.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// The current time of a virtual clock, shared between the host and whoever
/// moves it.
///
/// Cloning shares the same counter: advancing one handle advances every clone,
/// including the one already given to a [`Clock`]. The counter is
/// synchronized because a host is reachable from every task of a run, so two
/// tasks may read or advance this clock at the same time.
#[derive(Clone, Debug, Default)]
pub struct VirtualTime(Arc<Mutex<i64>>);

impl VirtualTime {
    /// A clock sitting at its origin.
    pub fn new() -> Self {
        VirtualTime::default()
    }

    /// Nanoseconds since the origin.
    pub fn nanos(&self) -> i64 {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Moves time forward by `nanos`.
    ///
    /// A monotonic clock never runs backwards, so a negative `nanos` moves
    /// nothing. Time saturates at [`i64::MAX`] rather than overflowing, since
    /// a clock that wrapped would report a time before one it already
    /// reported.
    pub fn advance(&self, nanos: i64) {
        if nanos > 0 {
            let mut now = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *now = now.saturating_add(nanos);
        }
    }
}

/// `clock`: how much time has passed, and waiting for more of it to pass.
pub struct Clock {
    source: ClockSource,
}

enum ClockSource {
    /// Real monotonic time, measured from the instant this host was built.
    Real(Instant),
    /// Time that moves only when the host moves it.
    Virtual(VirtualTime),
}

/// What `clock` declares about itself.
///
/// The table is [`cove_schema::hosts::CLOCK`], so the description the
/// compiler checks a call against and the one the boundary dispatches through
/// are the same bytes.
const SCHEMA: ModuleSchema = cove_schema::hosts::CLOCK;

impl Clock {
    /// Real monotonic time, measured from the instant this host is built.
    ///
    /// The origin is the host's own construction rather than the Unix epoch,
    /// so granting `clock` never discloses the wall-clock date.
    pub fn real() -> Self {
        Clock {
            source: ClockSource::Real(Instant::now()),
        }
    }

    /// A clock whose time moves only when `time` is advanced.
    ///
    /// `sleep` on a virtual clock advances `time` by the requested duration
    /// and returns immediately: the host satisfies the wait by moving its own
    /// clock instead of by blocking. A program cannot observe the difference
    /// except that it finishes at once, which is what makes a test that
    /// depends on elapsed time deterministic.
    pub fn virtual_clock(time: VirtualTime) -> Self {
        Clock {
            source: ClockSource::Virtual(time),
        }
    }

    /// Nanoseconds since this clock's origin.
    ///
    /// A real clock saturates at [`i64::MAX`], which no process reaches: it is
    /// roughly 292 years of uptime.
    fn now_nanos(&self) -> i64 {
        match &self.source {
            ClockSource::Real(origin) => {
                i64::try_from(origin.elapsed().as_nanos()).unwrap_or(i64::MAX)
            }
            ClockSource::Virtual(time) => time.nanos(),
        }
    }

    /// Whether this clock's time is the machine's.
    fn is_real(&self) -> bool {
        matches!(&self.source, ClockSource::Real(_))
    }

    /// Runs `body` and answers what it produced, unless it took longer than
    /// `nanos`.
    ///
    /// A real clock bounds the body while it runs: a watchdog thread raises a
    /// flag when the bound is reached, and the body stops at its next
    /// safepoint. That is a timeout rather than a measurement — the work
    /// stops, and the caller is told it did.
    ///
    /// A virtual clock has no thread to raise anything, because it has no
    /// time of its own: it moves only when something moves it, and the only
    /// thing that moves it during the body is the body's own `sleep`. So a
    /// virtual clock judges afterwards, by how far the body pushed it. The
    /// answer is the same one — a body that slept past the bound timed out —
    /// and it is deterministic, which is what a virtual clock is for.
    fn timeout(
        &self,
        nanos: i64,
        body: &Value,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        if nanos < 0 {
            return Ok(Value::err(Value::error(
                "clock: a timeout must not be negative",
            )));
        }
        let expired = |value: Value| {
            let _ = value;
            Value::err(Value::error(format!(
                "clock: timed out after {}",
                Value::Duration(nanos)
            )))
        };
        match &self.source {
            ClockSource::Real(_) => {
                let stop = Cancellation::new();
                let watch = Watchdog::start(nanos, stop.clone());
                let outcome = back.call_until(body, Vec::new(), &stop);
                drop(watch);
                match outcome {
                    Ok(value) if stop.is_cancelled() => Ok(expired(value)),
                    Ok(value) => Ok(Value::ok(value)),
                    // A body stopped by this bound reports the bound, not
                    // whatever the safepoint happened to say.
                    Err(_) if stop.is_cancelled() => Ok(expired(Value::Unit)),
                    Err(error) => Err(error),
                }
            }
            ClockSource::Virtual(time) => {
                let before = time.nanos();
                let value = back.call(body, Vec::new())?;
                if time.nanos().saturating_sub(before) > nanos {
                    return Ok(expired(value));
                }
                Ok(Value::ok(value))
            }
        }
    }

    /// Runs `body` every `nanos` until the task holding the timer is
    /// cancelled, or until `body` fails.
    ///
    /// A real clock repeats: that is what a timer is. A virtual clock fires
    /// once, because it has no time of its own — its `sleep` moves the clock
    /// instead of waiting, so a repeating timer on it would be a loop with
    /// nothing to wait for. One round is what a clock that only moves when
    /// the host moves it can honestly give, and it is what makes a program
    /// with a timer testable without one.
    ///
    /// The flag is read before the first round, so a timer whose task is
    /// cancelled before its thread gets this far runs nothing at all. How many
    /// rounds such a timer completed is therefore decided by neither this
    /// clock nor the program that spawned it, and ADR 0008's amendment records
    /// why nothing orders the two.
    fn every(
        &self,
        nanos: i64,
        body: &Value,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        if nanos < 0 {
            return Ok(Value::err(Value::error(
                "clock: a timer period must not be negative",
            )));
        }
        loop {
            if back.is_cancelled() {
                return Ok(Value::ok(Value::Unit));
            }
            self.sleep(nanos);
            if back.is_cancelled() {
                return Ok(Value::ok(Value::Unit));
            }
            let answered = back.call(body, Vec::new())?;
            // The body reports failure the way every Cove function does, and
            // a timer whose body failed stops rather than failing again every
            // period from now on.
            if answered.is_err() {
                return Ok(answered);
            }
            if !self.is_real() {
                return Ok(Value::ok(Value::Unit));
            }
        }
    }

    /// Waits `nanos`, or reports why it will not.
    fn sleep(&self, nanos: i64) -> Value {
        if nanos < 0 {
            return Value::err(Value::error("clock: a sleep duration must not be negative"));
        }
        match &self.source {
            ClockSource::Real(_) => {
                std::thread::sleep(std::time::Duration::from_nanos(nanos as u64))
            }
            ClockSource::Virtual(time) => time.advance(nanos),
        }
        Value::ok(Value::Unit)
    }
}

impl HostApi for Clock {
    fn name(&self) -> &str {
        "clock"
    }

    fn capability(&self) -> Capability {
        Capability::new("clock")
    }

    fn schema(&self) -> &[OperationSchema] {
        SCHEMA.operations
    }

    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            "timeout" => {
                let [Value::Duration(nanos), body] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.timeout(*nanos, body, back)
            }
            "every" => {
                let [Value::Duration(nanos), body] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.every(*nanos, body, back)
            }
            _ => self.call(op, args),
        }
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "now" => Ok(Value::Duration(self.now_nanos())),
            "sleep" => {
                let [Value::Duration(nanos)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(self.sleep(*nanos))
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

/// A thread that raises a flag once a bound is reached, and stops when the
/// work it was watching is done.
///
/// It polls rather than sleeping the whole bound, so a body that finishes
/// early does not leave a thread asleep for a minute behind it.
struct Watchdog {
    finished: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(nanos: i64, stop: Cancellation) -> Watchdog {
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let deadline = Instant::now() + std::time::Duration::from_nanos(nanos as u64);
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if Instant::now() >= deadline {
                    stop.cancel();
                    return;
                }
                std::thread::sleep(WATCH_INTERVAL);
            }
        });
        Watchdog {
            finished,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Grants, HostRegistry};
    use std::time::Duration;

    fn nanos(value: Value) -> i64 {
        match value {
            Value::Duration(nanos) => nanos,
            other => panic!("expected a `Duration`, found {other}"),
        }
    }

    fn is_ok(value: &Value) -> bool {
        value.is_ok()
    }

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    fn ok_int(value: Value) -> i64 {
        match value.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value::Int(n)) => *n,
                other => panic!("expected `Ok(Int)`, found {other:?}"),
            },
            None => panic!("expected `Ok(...)`, found {value}"),
        }
    }

    /// What a [`StubReentry`] runs in place of a Cove callback.
    type StubBody = Box<dyn FnMut(&Cancellation) -> Result<Value, RuntimeError>>;

    /// A stub [`Reentry`] for tests, standing in for the interpreter: it runs
    /// the boxed closure it was built with instead of dispatching into Cove
    /// code, and hands the closure whichever [`Cancellation`] the call
    /// carried, so a body that wants to observe a bound can.
    struct StubReentry {
        calls: usize,
        /// Whether the task holding this reentry has been asked to stop.
        ///
        /// Shared rather than owned so a body can raise it while the host is
        /// looping, which is how a test ends a repeating timer the way a
        /// cancelled task ends one.
        cancelled: Arc<AtomicBool>,
        body: StubBody,
    }

    impl StubReentry {
        fn new(body: impl FnMut(&Cancellation) -> Result<Value, RuntimeError> + 'static) -> Self {
            StubReentry {
                calls: 0,
                cancelled: Arc::new(AtomicBool::new(false)),
                body: Box::new(body),
            }
        }

        /// Reports the task holding this reentry as already cancelled.
        fn cancelled(self) -> Self {
            self.cancelled.store(true, Ordering::Relaxed);
            self
        }

        /// Reports the task holding this reentry as stopped when `flag` is
        /// raised, so a body can end a repeating timer from inside a round
        /// the way a cancelled task ends one.
        fn stopped_by(mut self, flag: Arc<AtomicBool>) -> Self {
            self.cancelled = flag;
            self
        }
    }

    impl Reentry for StubReentry {
        fn call(&mut self, _callee: &Value, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            self.calls += 1;
            (self.body)(&Cancellation::new())
        }

        fn call_until(
            &mut self,
            _callee: &Value,
            _args: Vec<Value>,
            stop: &Cancellation,
        ) -> Result<Value, RuntimeError> {
            self.calls += 1;
            (self.body)(stop)
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Relaxed)
        }

        /// Neither `timeout` nor `every` reads the run's deadline — a bound
        /// the program wrote is the only clock either of them keeps — so this
        /// stub has none to report.
        fn time_left(&self) -> Option<std::time::Duration> {
            None
        }
    }

    #[test]
    fn a_virtual_clock_starts_at_its_origin_and_stands_still() {
        let time = VirtualTime::new();
        let clock = Clock::virtual_clock(time.clone());

        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 0);
        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 0);
        assert_eq!(time.nanos(), 0);
    }

    #[test]
    fn a_virtual_clock_moves_only_when_the_host_moves_it() {
        let time = VirtualTime::new();
        let clock = Clock::virtual_clock(time.clone());

        time.advance(1_500_000_000);
        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 1_500_000_000);

        time.advance(500_000_000);
        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 2_000_000_000);
    }

    #[test]
    fn a_virtual_clock_never_runs_backwards() {
        let time = VirtualTime::new();
        time.advance(1_000);
        time.advance(-1_000);
        assert_eq!(time.nanos(), 1_000);
    }

    #[test]
    fn sleeping_a_virtual_clock_advances_it_instead_of_waiting() {
        let time = VirtualTime::new();
        let clock = Clock::virtual_clock(time.clone());

        let started = Instant::now();
        let slept = clock
            .call("sleep", vec![Value::Duration(3_600_000_000_000)])
            .unwrap();
        assert!(is_ok(&slept), "{slept}");
        assert_eq!(time.nanos(), 3_600_000_000_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn sleeping_a_negative_duration_is_an_error_on_either_clock() {
        for clock in [Clock::real(), Clock::virtual_clock(VirtualTime::new())] {
            let slept = clock.call("sleep", vec![Value::Duration(-1)]).unwrap();
            assert_eq!(
                err_message(slept),
                "clock: a sleep duration must not be negative"
            );
        }
    }

    #[test]
    fn a_real_clock_never_reports_an_earlier_time_than_it_already_did() {
        let clock = Clock::real();

        let first = nanos(clock.call("now", Vec::new()).unwrap());
        let second = nanos(clock.call("now", Vec::new()).unwrap());
        assert!(first >= 0, "{first}");
        assert!(second >= first, "{second} < {first}");
    }

    #[test]
    fn a_real_clock_observes_its_own_sleep() {
        let clock = Clock::real();

        let before = nanos(clock.call("now", Vec::new()).unwrap());
        let slept = clock
            .call("sleep", vec![Value::Duration(1_000_000)])
            .unwrap();
        assert!(is_ok(&slept), "{slept}");
        let after = nanos(clock.call("now", Vec::new()).unwrap());
        assert!(after - before >= 1_000_000, "{after} - {before}");
    }

    #[test]
    fn a_run_without_the_clock_grant_cannot_read_the_time() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Clock::real()));

        let error = hosts
            .call("clock", "now", Vec::new())
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`clock.now` requires the `clock` capability, which this run was not granted"
        );
    }

    #[test]
    fn a_granted_virtual_clock_is_reachable_through_the_registry() {
        let time = VirtualTime::new();
        let mut hosts = HostRegistry::new(Grants::new(["clock"]));
        hosts.register(Box::new(Clock::virtual_clock(time.clone())));

        hosts
            .call("clock", "sleep", vec![Value::Duration(250_000_000)])
            .expect("the call should be allowed");
        let now = hosts
            .call("clock", "now", Vec::new())
            .expect("the call should be allowed");
        assert_eq!(nanos(now), 250_000_000);
    }

    #[test]
    fn timeout_on_a_virtual_clock_answers_ok_when_the_body_does_not_oversleep() {
        let clock = Clock::virtual_clock(VirtualTime::new());
        let mut back = StubReentry::new(|_stop| Ok(Value::Int(42)));

        let answer = clock
            .call_with(
                "timeout",
                vec![Value::Duration(1_000_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert!(is_ok(&answer), "{answer}");
        assert_eq!(ok_int(answer), 42);
    }

    /// A virtual clock has no time of its own, so `timeout` judges afterwards
    /// by how far the body's own `sleep` pushed the shared clock, rather than
    /// by racing a watchdog thread against it.
    #[test]
    fn timeout_on_a_virtual_clock_times_out_when_the_body_sleeps_past_the_bound() {
        let time = VirtualTime::new();
        let clock = Clock::virtual_clock(time.clone());
        let sleeper = Clock::virtual_clock(time);
        let mut back = StubReentry::new(move |_stop| {
            sleeper.call("sleep", vec![Value::Duration(2_000_000_000)])
        });

        let answer = clock
            .call_with(
                "timeout",
                vec![Value::Duration(1_000_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert_eq!(
            err_message(answer),
            format!("clock: timed out after {}", Value::Duration(1_000_000_000))
        );
    }

    #[test]
    fn timeout_on_a_real_clock_answers_ok_when_the_body_finishes_before_the_bound() {
        let clock = Clock::real();
        let mut back = StubReentry::new(|_stop| Ok(Value::Int(7)));

        let answer = clock
            .call_with(
                "timeout",
                vec![Value::Duration(200_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert!(is_ok(&answer), "{answer}");
        assert_eq!(ok_int(answer), 7);
    }

    /// The bound is kept to a few milliseconds so the test finishes quickly,
    /// and the body's own loop watches `stop` directly so nothing can hang:
    /// the watchdog is what raises the flag, and the body is what has to
    /// notice it.
    #[test]
    fn timeout_on_a_real_clock_times_out_a_body_that_spins_past_the_bound() {
        let clock = Clock::real();
        let mut back = StubReentry::new(|stop| {
            while !stop.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(Value::Unit)
        });

        let answer = clock
            .call_with(
                "timeout",
                vec![Value::Duration(5_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert_eq!(
            err_message(answer),
            format!("clock: timed out after {}", Value::Duration(5_000_000))
        );
    }

    #[test]
    fn a_negative_timeout_bound_is_an_error_on_either_clock() {
        for clock in [Clock::real(), Clock::virtual_clock(VirtualTime::new())] {
            let mut back = StubReentry::new(|_stop| Ok(Value::Unit));
            let answer = clock
                .call_with("timeout", vec![Value::Duration(-1), Value::Unit], &mut back)
                .unwrap();
            assert_eq!(err_message(answer), "clock: a timeout must not be negative");
            assert_eq!(
                back.calls, 0,
                "a bound this obviously bad never runs the body"
            );
        }
    }

    #[test]
    fn a_negative_timer_period_is_an_error_on_either_clock() {
        for clock in [Clock::real(), Clock::virtual_clock(VirtualTime::new())] {
            let mut back = StubReentry::new(|_stop| Ok(Value::Unit));
            let answer = clock
                .call_with("every", vec![Value::Duration(-1), Value::Unit], &mut back)
                .unwrap();
            assert_eq!(
                err_message(answer),
                "clock: a timer period must not be negative"
            );
            assert_eq!(
                back.calls, 0,
                "a period this obviously bad never runs the body"
            );
        }
    }

    /// A virtual clock has no time of its own to repeat a timer with, so
    /// `every` gives the one round it honestly can rather than looping
    /// forever with nothing to wait for.
    #[test]
    fn every_on_a_virtual_clock_fires_exactly_once() {
        let clock = Clock::virtual_clock(VirtualTime::new());
        let mut back = StubReentry::new(|_stop| Ok(Value::ok(Value::Unit)));

        let answer = clock
            .call_with(
                "every",
                vec![Value::Duration(1_000_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert!(is_ok(&answer), "{answer}");
        assert_eq!(back.calls, 1);
    }

    #[test]
    fn every_hands_back_a_failing_bodys_err_instead_of_repeating() {
        let clock = Clock::virtual_clock(VirtualTime::new());
        let mut back = StubReentry::new(|_stop| Ok(Value::err(Value::error("boom"))));

        let answer = clock
            .call_with(
                "every",
                vec![Value::Duration(1_000_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert_eq!(err_message(answer), "boom");
        assert_eq!(back.calls, 1, "a failing round is not retried");
    }

    /// Runs `body` on a thread of its own and fails if it has not finished
    /// within `limit`.
    ///
    /// The rule below is one a host breaks by deadlocking, which is a way of
    /// failing that a test asserting on a result never reaches. This makes a
    /// regression a failure with a message rather than a suite that never
    /// ends.
    fn within<T: Send + 'static>(limit: Duration, body: impl FnOnce() -> T + Send + 'static) -> T {
        let (finished, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = finished.send(body());
        });
        done.recv_timeout(limit)
            .unwrap_or_else(|_| panic!("this did not finish within {limit:?}"))
    }

    /// A host may run the callback it was handed as many times as its
    /// operation means, and a timer means once a period. Nothing counts the
    /// invocations and none of them is cheaper than the first: what ends the
    /// loop is the run being stopped, which the host reads between rounds.
    #[test]
    fn every_on_a_real_clock_runs_the_body_once_a_round_until_the_run_is_stopped() {
        let rounds = within(Duration::from_secs(10), || {
            let clock = Clock::real();
            let stop = Arc::new(AtomicBool::new(false));
            let raise = Arc::clone(&stop);
            let mut rounds = 0;
            let mut back = StubReentry::new(move |_stop| {
                rounds += 1;
                if rounds >= 3 {
                    raise.store(true, Ordering::Relaxed);
                }
                Ok(Value::ok(Value::Unit))
            })
            .stopped_by(stop);

            let answer = clock
                .call_with("every", vec![Value::Duration(0), Value::Unit], &mut back)
                .unwrap();
            assert!(is_ok(&answer), "{answer}");
            back.calls
        });
        assert_eq!(rounds, 3, "the timer ran a round each period until stopped");
    }

    /// A host must hold no lock of its own while it runs a Cove callback,
    /// because the callback is Cove code and Cove code may call the same host
    /// again. `clock`'s only state is the virtual clock's counter, and it is
    /// held for a read and a write and nothing else — so a timer's body may
    /// read and move the very clock that is running it.
    ///
    /// Held across the round, this would deadlock the task on a mutex three
    /// frames up its own stack, which is why it runs under a bound.
    #[test]
    fn a_timer_body_may_read_and_move_the_clock_that_is_running_it() {
        let moved = within(Duration::from_secs(10), || {
            let time = VirtualTime::new();
            let clock = Clock::virtual_clock(time.clone());
            let inside = Clock::virtual_clock(time.clone());
            let mut back = StubReentry::new(move |_stop| {
                inside.call("now", Vec::new())?;
                inside.call("sleep", vec![Value::Duration(5)])?;
                Ok(Value::ok(Value::Unit))
            });

            let answer = clock
                .call_with(
                    "every",
                    vec![Value::Duration(1_000_000_000), Value::Unit],
                    &mut back,
                )
                .unwrap();
            assert!(is_ok(&answer), "{answer}");
            time.nanos()
        });
        assert_eq!(
            moved, 1_000_000_005,
            "the period the timer slept, plus what its body slept from inside the round"
        );
    }

    #[test]
    fn every_answers_ok_without_running_the_body_when_the_task_is_already_cancelled() {
        let clock = Clock::virtual_clock(VirtualTime::new());
        let mut back = StubReentry::new(|_stop| panic!("the body must not run")).cancelled();

        let answer = clock
            .call_with(
                "every",
                vec![Value::Duration(1_000_000_000), Value::Unit],
                &mut back,
            )
            .unwrap();
        assert!(is_ok(&answer), "{answer}");
        assert_eq!(back.calls, 0);
    }
}
