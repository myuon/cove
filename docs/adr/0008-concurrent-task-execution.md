# ADR 0008: Concurrent task execution and `Shared`

- Status: Accepted
- Date: 2026-08-25

## Context

ADR 0003 phase 1 built task scopes, handles, budgets, safepoints,
cancellation, and traces against **sequential** execution: an `async fn` body
runs at its call site and `spawn` returns an already-settled task. Phase 2 was
to replace execution with a thread per task.

The Language Card meanwhile names `Shared` as the sanctioned way to hold
mutable state across tasks, and the runtime's own task-safety diagnostic tells
the programmer to reach for it. `Shared` does not exist.

The two are one decision. `Shared` without concurrency is a wrapper that
guards nothing; concurrency without `Shared` leaves the diagnostic pointing at
a type that is not there.

## Decision

Run each spawned task on its own thread, and add `Shared<T>` as the
synchronized handle that may cross a task boundary.

### The task-safety rule already decides what may cross

The card says immutable task-safe values may cross a task boundary, a
`Vector` may not even through a `let`, closures cross only when every capture
does, and host resources declare it in their schema. `crates/cove-runtime/src/task.rs`
already enforces exactly this, and the schema field exists.

That rule is the whole design. A value that may cross is one that can be
copied at the boundary, which is the same condition a thread requires. So
crossing a task boundary copies, and the copy is what the new thread owns.

### `Shared<T>`

```cove
let metrics = Shared(Metrics(requests: 0, failures: 0))

metrics.lock(fn(var value) {
  value.record(failed)
})
```

`Shared(value)` wraps a task-safe value. `lock` takes a closure receiving a
`var` alias to the wrapped value, runs it with the lock held, and returns the
closure's value. There is no `get` and no `set`: every access is scoped, so a
read-modify-write cannot be written as two operations that race.

`Shared<T>` is itself task-safe and crosses a boundary by sharing, not
copying. That is the one exception to the copy rule, and it is the reason the
type exists.

The wrapped value must be task-safe. A `Shared<Vector<T>>` would let a vector
be reached from two tasks, which the card forbids in the sentence that names
`Shared`.

### Cancellation and budgets stay where they are

Budgets, safepoints, and cancellation were deliberately built first, against
sequential execution, so that phase 2 changes where work runs and not what
controls it. A task's fuel is drawn from the run's budget; cancelling a scope
cancels its threads through the existing `Cancellation` flag, which a
safepoint already observes.

### What this does not add

No async I/O, no work-stealing scheduler, no task priorities, no `select`. A
task blocks its thread when it waits, which is the simplest thing that is
correct and is enough to make `spawn` mean what the card says it means.

## Consequences

Values crossing a boundary must be `Send` in Rust terms. The card's
task-safety rule and Rust's requirement are the same rule, which is what makes
this a replacement rather than a rewrite — but the runtime's `Rc`-based value
representation is not `Send`, so the values that cross must be converted at
the boundary. That conversion is exactly the copy the rule already demands.

Tracing gains real concurrency to describe: CPU time and I/O wait become
separately attributable, which ADR 0001 lists as a success criterion and phase
1 explicitly could not validate.

A thread per task is not the end state. It is the smallest thing that makes
the card true, and a scheduler can replace it without changing the language.
