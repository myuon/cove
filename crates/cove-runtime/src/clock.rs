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

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use cove_sema::Capability;

use crate::error::RuntimeError;
use crate::host::HostApi;
use crate::schema::{Effect, HostType, OperationSchema};
use crate::value::Value;

/// The current time of a virtual clock, shared between the host and whoever
/// moves it.
///
/// Cloning shares the same counter: advancing one handle advances every clone,
/// including the one already given to a [`Clock`].
#[derive(Clone, Debug, Default)]
pub struct VirtualTime(Rc<Cell<i64>>);

impl VirtualTime {
    /// A clock sitting at its origin.
    pub fn new() -> Self {
        VirtualTime::default()
    }

    /// Nanoseconds since the origin.
    pub fn nanos(&self) -> i64 {
        self.0.get()
    }

    /// Moves time forward by `nanos`.
    ///
    /// A monotonic clock never runs backwards, so a negative `nanos` moves
    /// nothing. Time saturates at [`i64::MAX`] rather than overflowing, since
    /// a clock that wrapped would report a time before one it already
    /// reported.
    pub fn advance(&self, nanos: i64) {
        if nanos > 0 {
            self.0.set(self.0.get().saturating_add(nanos));
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

/// The operations `clock` exposes.
///
/// `every` is deliberately absent. `examples/callbacks/main.cove` calls it,
/// but a repeating timer needs a scheduler that keeps running beside the
/// program, and ADR 0003 phase 1 settles tasks sequentially at the call site.
/// A host that answered `every` today would either run the handler once or
/// block forever, and neither is what the program asked for.
///
/// `timeout` is absent for a different reason: it takes the work to bound as
/// a trailing closure, and [`HostApi::call`] receives values but has no way to
/// call back into the interpreter to run one.
static CLOCK_SCHEMA: &[OperationSchema] = &[
    OperationSchema {
        name: "now",
        params: &[],
        variadic: false,
        result: HostType::Duration,
        capability: "clock",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    },
    OperationSchema {
        name: "sleep",
        params: &[HostType::Duration],
        variadic: false,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "clock",
        // Waiting leaves nothing outside the run different, so it reads the
        // clock rather than writing anything.
        effect: Effect::Read,
        // Nothing has happened yet while a wait is in flight, so abandoning
        // one is safe. ADR 0003 phase 2 is what will actually do it.
        cancellable: true,
        recordable: true,
        result_is_task_safe: true,
    },
];

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
        CLOCK_SCHEMA
    }

    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "now" => Ok(Value::Duration(self.now_nanos())),
            "sleep" => {
                let [Value::Duration(nanos)] = args.as_slice() else {
                    return Err(RuntimeError::new(
                        "`clock.sleep` takes one `Duration` argument",
                    ));
                };
                Ok(self.sleep(*nanos))
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Grants, HostRegistry};

    fn nanos(value: Value) -> i64 {
        match value {
            Value::Duration(nanos) => nanos,
            other => panic!("expected a `Duration`, found {other}"),
        }
    }

    fn is_ok(value: &Value) -> bool {
        matches!(value, Value::Enum(result)
            if &*result.type_name == "Result" && &*result.case == "Ok")
    }

    fn err_message(value: Value) -> String {
        match value {
            Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Err" => {
                result
                    .payload
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            }
            other => panic!("expected `Err(...)`, found {other}"),
        }
    }

    #[test]
    fn a_virtual_clock_starts_at_its_origin_and_stands_still() {
        let time = VirtualTime::new();
        let mut clock = Clock::virtual_clock(time.clone());

        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 0);
        assert_eq!(nanos(clock.call("now", Vec::new()).unwrap()), 0);
        assert_eq!(time.nanos(), 0);
    }

    #[test]
    fn a_virtual_clock_moves_only_when_the_host_moves_it() {
        let time = VirtualTime::new();
        let mut clock = Clock::virtual_clock(time.clone());

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
        let mut clock = Clock::virtual_clock(time.clone());

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
        for mut clock in [Clock::real(), Clock::virtual_clock(VirtualTime::new())] {
            let slept = clock.call("sleep", vec![Value::Duration(-1)]).unwrap();
            assert_eq!(
                err_message(slept),
                "clock: a sleep duration must not be negative"
            );
        }
    }

    #[test]
    fn a_real_clock_never_reports_an_earlier_time_than_it_already_did() {
        let mut clock = Clock::real();

        let first = nanos(clock.call("now", Vec::new()).unwrap());
        let second = nanos(clock.call("now", Vec::new()).unwrap());
        assert!(first >= 0, "{first}");
        assert!(second >= first, "{second} < {first}");
    }

    #[test]
    fn a_real_clock_observes_its_own_sleep() {
        let mut clock = Clock::real();

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
}
