//! The monotonic clock the runtime measures a run against.
//!
//! On every platform with a monotonic clock in `std`, this is
//! [`std::time::Instant`] and nothing else: the re-export below is the whole
//! of the module, and no call site pays anything for it existing.
//!
//! `wasm32-unknown-unknown` has no clock. `std::time::Instant::now()` there
//! is `unsupported` and traps — measured, not assumed: a probe that called it
//! under node answered `RuntimeError: unreachable`. That is a problem for
//! this runtime specifically, because a clock is read on the path of *every*
//! run whether or not the program asks the time: [`crate::budget::Meter::new`]
//! starts the deadline clock and [`crate::trace::Timing::start`] starts the
//! run's timing, and both are reached before the first instruction executes.
//!
//! # What a deadline does here
//!
//! It keeps working. The brief allowed either that or refusing a deadline at
//! the boundary, and refusing was the weaker answer: an embedder that could
//! not bound a run by time would be left with fuel as its only bound, and
//! fuel is not portable between backends. So the clock is **an imported host
//! function** — the embedder supplies `cove.cove_now_millis`, returning
//! monotonically non-decreasing milliseconds since an origin it picks, and
//! [`crate::budget::Meter::safepoint`] compares against it exactly as it
//! compares against `Instant` anywhere else. `performance.now()` is that
//! function in a browser and under node.
//!
//! The import is not optional and is not defaulted. A module instantiated
//! without it fails to instantiate, loudly, at load. A default — a counter
//! that never moves, say — would give a run a deadline that silently never
//! fires, which is the one outcome this project will not ship: the caller
//! would be told the run was bounded and it would not be.
//!
//! # What this module does not cover
//!
//! [`crate::http`] still reads `std::time::Instant` directly. It is the one
//! module whose every path needs a socket, and `wasm32-unknown-unknown` has
//! none, so its clock reads are unreachable there and swapping them would be
//! churn against a module that cannot run either way.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::Instant;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::ops::Add;
    use std::time::Duration;

    #[link(wasm_import_module = "cove")]
    extern "C" {
        /// Monotonically non-decreasing milliseconds since an origin the
        /// embedder picks. `performance.now()` satisfies this.
        ///
        /// The origin is never disclosed to a Cove program: `clock.now()`
        /// reports time since the host was built, and a deadline is a
        /// difference. So an embedder is free to hand back
        /// `performance.now()`, `Date.now()`, or a counter of its own.
        fn cove_now_millis() -> f64;
    }

    /// A point on the embedder's monotonic clock, in nanoseconds since its
    /// origin.
    ///
    /// The same surface [`std::time::Instant`] offers at the call sites in
    /// this crate, and no more: taking a reading, the duration since one, and
    /// a deadline built by adding to one. Both differences saturate, as
    /// `std`'s `saturating_duration_since` does, so a clock that an embedder
    /// let run backwards reports no elapsed time rather than panicking or
    /// wrapping.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct Instant(u64);

    impl Instant {
        /// Reads the embedder's clock.
        pub(crate) fn now() -> Instant {
            // A NaN or a negative reading is a broken embedder, and the
            // origin is the safest reading to attribute to it: it makes
            // elapsed time zero, so a deadline is not reached early. It
            // cannot make a deadline unreachable, because a clock that then
            // recovers reports the real elapsed time from the real origin.
            let millis = unsafe { cove_now_millis() };
            let nanos = millis * 1.0e6;
            Instant(if nanos.is_finite() && nanos > 0.0 {
                nanos as u64
            } else {
                0
            })
        }

        /// How long since this reading was taken.
        pub(crate) fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }

        /// How long between `earlier` and this reading, or zero if `earlier`
        /// is the later of the two.
        pub(crate) fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(earlier.0))
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, held: Duration) -> Instant {
            let nanos = u64::try_from(held.as_nanos()).unwrap_or(u64::MAX);
            Instant(self.0.saturating_add(nanos))
        }
    }
}
