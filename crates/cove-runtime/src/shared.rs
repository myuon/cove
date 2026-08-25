//! `Shared<T>`: mutable state that more than one task may reach.
//!
//! The Language Card names this type in the sentence that keeps a vector out
//! of a task: "finish it as an array or wrap mutable state in `Shared` or
//! another synchronized type." ADR 0008 makes it the one value that crosses a
//! task boundary by sharing rather than by copying, which is the reason the
//! type exists.
//!
//! ```cove
//! let metrics = Shared(Metrics(requests: 0, failures: 0))
//! metrics.lock(fn(var value) { value.record(failed) })
//! ```
//!
//! There is no `get` and no `set`. Every access is a scoped [`SharedCell::lock`],
//! so a read-modify-write cannot be written as two operations that race: the
//! read, the modification, and the write are one call with the lock held for
//! all three.
//!
//! What the cell holds is a [`Transfer`], not a [`Value`]: the wrapped value
//! must be task-safe, a task-safe value is exactly one a `Transfer` can carry,
//! and a `Transfer` is the only form two threads can both address. Each `lock`
//! therefore converts the cell's contents into the locking task's own
//! [`Value`] and converts back what the closure leaves — the same copy the
//! task-safety rule already demands at every other boundary.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cove_diag::Span;

use crate::error::RuntimeError;
use crate::task::{NotTaskSafe, Transfer, TASK_SAFETY_RULE};
use crate::value::Value;

/// The next tag [`THREAD_TAG`] hands out.
///
/// Zero means "no task holds this cell", so tags start at one.
static NEXT_THREAD_TAG: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// A number identifying this thread among the threads that take locks.
    ///
    /// A [`std::thread::ThreadId`] cannot be turned into an integer on stable
    /// Rust, and the holder has to be readable without taking a lock of its
    /// own, so the runtime hands out its own tags.
    static THREAD_TAG: u64 = NEXT_THREAD_TAG.fetch_add(1, Ordering::Relaxed);
}

/// The storage a `Shared<T>` value addresses.
///
/// Cloning the [`Arc`] is what "crosses by sharing" means: every task that
/// received the `Shared` addresses this one cell, and the [`Mutex`] is what
/// makes their accesses take turns.
pub struct SharedCell {
    value: Mutex<Transfer>,
    /// The tag of the thread currently inside [`SharedCell::lock`], or zero.
    ///
    /// Only that thread can write its own tag here, so reading it back is a
    /// sound test for "this task already holds this cell" — which is a
    /// deadlock the runtime reports instead of waiting for.
    holder: AtomicU64,
}

/// Marks a cell as held for as long as the value is alive, so every path out
/// of a `lock` — including a closure that raised an error — releases it.
struct Held<'a> {
    holder: &'a AtomicU64,
}

impl<'a> Held<'a> {
    fn new(holder: &'a AtomicU64, tag: u64) -> Held<'a> {
        holder.store(tag, Ordering::Release);
        Held { holder }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.holder.store(0, Ordering::Release);
    }
}

impl SharedCell {
    /// Wraps an already converted value.
    pub fn new(value: Transfer) -> Arc<SharedCell> {
        Arc::new(SharedCell {
            value: Mutex::new(value),
            holder: AtomicU64::new(0),
        })
    }

    /// `Shared(value)`: wraps `value`, or reports why it may not be wrapped.
    ///
    /// The payload must be task-safe for the same reason a spawned closure's
    /// captures must be. A `Shared<Vector<T>>` would let a vector be reached
    /// from two tasks, which is exactly what the sentence naming `Shared`
    /// forbids.
    pub fn wrap(value: &Value, span: Span) -> Result<Arc<SharedCell>, RuntimeError> {
        match Transfer::of(value) {
            Ok(transfer) => Ok(SharedCell::new(transfer)),
            Err(found) => Err(cannot_wrap(&found, span)),
        }
    }

    /// Runs `body` with the wrapped value, holding the lock for the whole
    /// call, and stores back whatever `body` leaves in it.
    ///
    /// `body` receives the value and returns its own result together with the
    /// value to store — which is the same value when the closure took it as a
    /// `var` alias and mutated it in place. A `body` that raises an error
    /// stores nothing, so a half-finished modification is never left behind
    /// for another task to find.
    pub fn lock<R>(
        &self,
        span: Span,
        body: impl FnOnce(Value) -> Result<(R, Value), RuntimeError>,
    ) -> Result<R, RuntimeError> {
        let tag = THREAD_TAG.with(|tag| *tag);
        if self.holder.load(Ordering::Acquire) == tag {
            return Err(reentrant_lock(span));
        }
        // A panic inside a `lock` ends the run that raised it, so a poisoned
        // cell is not a state anything recovers from. Taking the value back
        // keeps one task's broken invariant from becoming a second,
        // unrelated failure in another.
        let mut guard = self
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Declared after the guard, so it is dropped before it: the cell stops
        // being held before another task can acquire it.
        let _held = Held::new(&self.holder, tag);
        let (result, updated) = body(guard.clone().into_value())?;
        *guard = Transfer::of(&updated).map_err(|found| cannot_store(&found, span))?;
        Ok(result)
    }
}

/// A cell is a handle, and its contents are reachable only under the lock, so
/// it shows as the handle it is rather than as what it currently holds.
impl fmt::Debug for SharedCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Shared")
    }
}

/// `Shared(value)` where `value` may not cross a task boundary.
fn cannot_wrap(found: &NotTaskSafe, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`Shared` cannot wrap {}, which cannot cross a task boundary",
        found.subject()
    ))
    .at(span)
    .with_rule(TASK_SAFETY_RULE)
    .with_help(found.help("wrapping it"))
}

/// A `lock` whose closure left something behind that may not cross a task
/// boundary.
fn cannot_store(found: &NotTaskSafe, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`lock` cannot store {}, which cannot cross a task boundary",
        found.subject()
    ))
    .at(span)
    .with_rule(TASK_SAFETY_RULE)
    .with_help(found.help("storing it"))
}

/// A `lock` taken by a task that already holds the same cell.
///
/// Waiting would be waiting for itself, so the runtime says so instead of
/// hanging.
fn reentrant_lock(span: Span) -> RuntimeError {
    RuntimeError::new("this task already holds this `Shared`, so `lock` would wait for itself")
        .at(span)
        .with_rule(
            "`lock` holds the value for the whole of the closure it is given, so a `lock` on the same `Shared` inside it can never be granted.",
        )
        .with_help("do the whole read-modify-write in one `lock`")
}
