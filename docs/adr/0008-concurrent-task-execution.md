# ADR 0008: Concurrent task execution and `Shared`

- Status: Accepted
- Date: 2026-08-25
- Supersedes: phase 2 of
  [ADR 0003](0003-task-execution-and-runtime-control.md)'s Decision, and with it
  its "What Phase 1 does not validate" section
- Amends: [ADR 0001](0001-mvp-language-design.md), which names `Shared<T>`,
  `Mutex<T>`, `RwLock<T>`, `Atomic<T>`, and `Channel<T>` as the synchronized
  handles; the MVP has the first and none of the rest
- Amended by: this ADR's own "Amendment (2026-08-25): what a `spawn` orders"
  below, which extends the absence of a scheduling policy to a task's start:
  what a spawned task has done by the time a cancellation reaches it is
  decided by neither the runtime nor the program, and is not asserted by a
  test; and [ADR 0003](0003-task-execution-and-runtime-control.md)'s
  "Amendment (2026-08-25): the runtime sizes the stack it recurses on", which
  decides how large the thread a task gets is, because that size is a
  property of the interpreter's frames rather than of the thread-per-task
  choice and any scheduler replacing this one would carry it unchanged
- Implemented by: PR #23; the amendment by PR #47; the stack a task's thread
  gets by the change that closed issue #67
- Implementation status: complete — the amendment adds no code, which is the
  whole of what it decides

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

## Amendment (2026-08-25): what a `spawn` orders

The Decision above says the state machine holds no scheduling policy — only
`crate::interp` decides when a task is joined.
[Issue #39](https://github.com/myuon/cove/issues/39) asked whether that should
stop short of a task's *start*.

`examples/callbacks/main.cove` spawns a repeating report timer into a scope
and cancels it once the server's listener runs dry:

```cove
scope services {
  let reportTimer = services.spawn {
    clock.every(60s, async fn() { ... })
  }

  while server.handle(router.routes)? { }

  reportTimer.cancel()
}
```

How many times that timer fires is decided by neither the program, the clock,
nor the Host API. `clock.every` reads its task's cancellation flag before it
does anything else, so a `cancel` that lands before the timer's first check
means no round at all, and which of the two lands first is the operating
system's answer. Measured on one machine against the fake hosts, the same
program fired the timer in 200 of 200 runs with two requests to serve first —
150 of them with every core saturated — and in 0 of 100 runs with none. The
only thing that changed was how much work `main` did before it cancelled. CI,
being slower and more contended, found the zero on a run where that machine
never did, which is how this surfaced (PR #35).

The program is not wrong. Under a real clock a sixty-second timer cancelled
after two requests fires zero times, which is the honest answer. What is
missing is any way for a test to *pin* the count, and this amendment decides
that nothing is added to provide one. It records the three ways it could have
been provided, and why each was refused, so that the next program with a
background task does not reopen the question from the beginning.

**What is decided: a `spawn` starts a task and orders nothing else.** It
returns once the thread exists. Whether that thread has run a single
instruction by the time the parent's next statement runs is not something this
runtime answers, and a program that needs an answer writes one: `await`,
leaving a scope, and `Shared::lock` are the orderings a program can rely on
because they are the ones it asks for. `cancel` is not among them — it asks,
and whether it stopped work or arrived after the work was done is known only
at the join, which is exactly why `TaskCancelled` is traced there and not at
the `cancel`.

**What follows for a test: assert what the program decides.** For `callbacks`
that is everything the request-serving task prints, in order, because one task
decides its own order; the responses the server served; and, of the report
timer, that the fake clock offers one round at most, so at most one report
line may appear, and that a line which does appear is one of the three the
program could print — the timer sees zero, one, or two requests recorded, and
neither route fails, so `failures=0` always. It is not asserted that the line
appears. `clock.every`'s own behaviour — one round on a virtual clock, an
`Err` handed back rather than retried, nothing run at all when the task is
already cancelled — is pinned exactly by the unit tests in
`crates/cove-runtime/src/clock.rs`, which drive it with a stub `Reentry` and
have no second thread to race. What is uncovered is the mechanism in a program
that also does something else, and that is what this amendment accepts.

**Rejected: a rendezvous at `spawn`.** A `spawn` that did not return until the
child's body had started would cost a synchronisation on every task for a
guarantee only a test wants, and it is a scheduling policy, which is what this
ADR does not have. It also would not work. The point a `spawn` can name is
"the body has begun"; the check that decides the count is several steps later
— evaluate the call, dispatch through `HostRegistry`, enter `Clock::every`,
read the flag — and all of those still race the parent's `cancel`. To pin the
count, `spawn` would have to block until the child reached a point that only
the child's body knows about and that lives inside a host operation. And this
same `spawn` already refused to wait once: ADR 0003's concurrency-limit
amendment stops a run at its limit rather than blocking until a sibling
finishes, on the grounds that "waiting is a scheduling policy, and ADR 0008
deliberately has none". A rendezvous would go a dozen lines below that
refusal, in the same function, and leave it arguing with itself.

**Rejected: a clock a test steps.** A virtual clock whose `sleep` waits until
the host advances time past the wake point, plus a way to advance it one round
at a time, would change `sleep`'s documented behaviour — it advances the clock
and returns at once, which is what makes a program that waits finish
immediately — and would turn a forgotten advance into a hang rather than a
pass. It also relocates the race instead of removing it: the thread that
advances the clock is no more ordered against the program's `cancel` than the
scheduler is, unless it also blocks the program somewhere, which is the
rendezvous again under another name.

**Rejected: a `clock.every` that reports its rounds.** Making the timer answer
with how many rounds it completed changes a Host API's declared result from
`Result<Unit, Error>` to `Result<Int, Error>` for every program that uses it,
so that a test may learn a number. The number is still the scheduler's: it
reports the race rather than deciding it. And a count only reaches a test if
the program prints it, which is the console line the test already counts, so
the strongest assertion it buys — that between zero and one round happened —
is the assertion the test already makes.

**When this is worth reopening.** If a second representative program grows a
background task whose effects a test wants to pin, the cost of a stepped clock
is being paid for twice and it becomes the cheapest of the three; a rendezvous
at `spawn` does not become correct with a second caller. Until then the
consequence lives in two places that say so: `examples/README.md`, and the
test in `crates/cove-cli/tests/examples.rs` that stops where the program stops
deciding.
