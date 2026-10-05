# ADR 0088: An embedder sizes the heap, and is told of a cancellation

- Status: Proposed
- Date: 2026-10-06
- Issue: [#601](https://github.com/myuon/cove/issues/601)
- Refers to:
  [ADR 0011](0011-garbage-collection.md)'s amendment "the memory budget is
  removed", which retracted `Limits::max_memory` and whose argument this keeps;
  [ADR 0081](0081-a-run-collects-when-it-has-allocated-its-allowance.md), which
  decides when a run collects and leaves the heap bounded by its budget;
  [ADR 0080](0080-a-host-call-may-answer-pending.md) and
  [ADR 0082](0082-a-parked-run-keeps-its-deadline.md), whose `ParkedVm::cancel`
  is what a host told of a cancellation calls;
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md), whose `YieldedVm` is the
  other run a host holds off its thread
- Supersedes: nothing. No accepted ADR decides who sizes a run's heap — the
  code's own documentation of `Vm::with_heap_words` said "not a knob an
  embedder is invited to reach for", and that sentence changes with this — nor
  whether a `Cancellation` tells anyone
- Decides: how an embedder sizes an isolate's heap; how a holder of a run that
  is not running learns it was cancelled; what "this entry can spawn" means as
  a check-time fact

## Context

`cove-tools` is the first embedder written outside this repository, and its
host (myuon/cove-tools#5, #6) runs one `OwnedVm` per app, many apps per
process, each with a `max_heap_words` its operator configured. It found four
things the runtime could not do and two the edge example had already listed as
friction (issue 601). Most of them need no decision — a `heap_words()` that
`OwnedVm` had and `ParkedVm` lacked is a forwarding method — and they are
listed under "What else landed" below. Three do:

- **The heap.** `OwnedVm::new` always built a heap of `DEFAULT_HEAP_WORDS`
  (four mebiwords). The only way to another size was `Vm::with_heap_words`,
  whose documentation said it was for tests forcing a collection and not for
  embedders, citing ADR 0011's amendment: a `Limits::max_memory` that bounds
  only what one collector can see is "that instrument's readout wearing a
  ceiling's name". So a host whose app may use 64 MiB of heap could only read
  `heap_words()` after the run answered, and an app that grew past its share
  took the process's memory with it before it was noticed.
- **Cancellation.** `Cancellation` was an `Arc<AtomicBool>`. A running run
  reads it at a safepoint; a run parked at a host call or yielded in a queue
  reads nothing, and the host holding it has to notice and call `cancel` on
  it. With a bare flag it could only poll, so `cove-tools` paired every flag
  with a token of its own (a `tokio` cancellation token) that it cancelled
  alongside — two objects meaning one thing, which the runtime could not see.
- **Spawning.** A host that refuses concurrency per app compiled the program
  and scanned the lowered IR for a spawn. It wanted the answer where it gets
  `required_capabilities`: on the checked entry, before lowering.

## Decision

### 1. The heap's capacity is a constructor argument, not a limit

`OwnedVm::with_heap_words(runtime, hosts, prepared, heap_words)` builds a
machine whose heap may grow to `heap_words` words; `OwnedVm::new` is that with
`DEFAULT_HEAP_WORDS`. A run that needs more fails the allocation with the
runtime's existing "this run has no memory left" — the same error, at the same
place, as a run that exhausts the default.

It is deliberately *not* a `Limits` field, and ADR 0011's amendment is why. A
`Limits` field is a promise about the run: `fuel`, `deadline` and
`max_host_calls` each bound something the run does, wherever it does it. A heap
capacity bounds one region — Cove-owned objects — and not a host's
allocations, a resource, or a task's stack segment. Named beside the others it
would read as a memory ceiling, which is the exact mistake the amendment
retracted. As a constructor argument it is what it is: the size of the arena
this machine allocates in, chosen when the machine is built, as a JVM's `-Xmx`
is chosen for the process rather than for a call.

That is also why it is the machine's and not the run's: a resident `OwnedVm`
runs many requests, and its heap is one arena across them.

Pacing does not change (ADR 0081). A run collects when it has allocated its
allowance since the last collection, and also when an allocation does not fit
the capacity — which with a small capacity is simply sooner. Nothing about the
allowance's computation reads the capacity.

`heap_words()` — what the heap occupies now — is offered on `ParkedVm` and
`YieldedVm` as well as `OwnedVm`, so a host enforcing a *softer* limit of its
own (say, refuse to resume a run above 80% of its share) can look at every
point a run is in its hands.

### 2. A `Cancellation` tells whoever asked

`Cancellation::on_cancel(callback)` registers a `FnOnce() + Send + 'static`
that runs once when the flag is raised: on the thread that raises it, after the
flag is visible, outside any lock, before `cancel` returns — or at once, on the
registering thread, if it already has been. `Cancellation::wait_timeout(d)`
blocks a thread until the flag is raised or `d` passes. `Meter::cancellation()`
hands out the run's flag, so a host holding a `ParkedVm` or `YieldedVm`
registers on it through `meter()` without having kept the one it built the
budget with.

The safepoint is untouched. The flag a safepoint reads is the same one atomic
word, now first in a struct beside a mutex and a condition variable that only
`cancel`, `on_cancel` and `wait_timeout` touch.

A callback, not a `Future`. The runtime depends on no async runtime and should
not acquire one to hand out a future; a callback is the primitive every
executor adapts in a line (store the `Waker`, wake it in the callback), and a
blocking host adapts it as easily (send on a channel). `wait_timeout` exists
for the host with a timer thread and no executor, which is what `examples/edge`
is.

There is no deregistration. A callback is kept until the flag is raised or the
last clone of it is dropped. A handle that unregistered on drop would make
every registration a guard the host has to keep somewhere, for the case of a
run that finished without being cancelled — which is the case where the flag
itself is about to be dropped. A host registers once per waiter, not once per
poll, and the documentation says so.

### 3. "Can spawn" is "reaches a `scope`"

`FnEntry::direct_spawns` says a function's own body opens a task `scope`;
`FnEntry::can_spawn` says it or something it calls does, propagated over the
call graph in the same fixed point as `required_capabilities`.

It is defined by `scope`, not by `.spawn`, because the resolver that computes
these facts has no types, and because for this question the two agree where it
matters. A task is spawned only into a scope; a scope's handle is a type the
language gives no name to, so no parameter and no field can hold one; so
whatever spawns runs inside a `scope` that some function on the call stack
opened, and every function on that stack reaches it. A scope that spawns
nothing is the one over-approximation. The fact is a lower bound exactly where
`required_capabilities` is — when `is_capability_open()`, a call the graph
cannot follow may reach a scope it cannot see — and a host that refuses
concurrency refuses on `can_spawn || is_capability_open()`.

## What else landed

None of these needed a decision; they are recorded so the issue's list has one
place it is answered.

- `ParkedVm::yields_declined()` and `YieldedVm::yields_declined()`, beside the
  `heap_words()` above; `OwnedVm::assertion_failure()`.
- `Transfer::ok`, `err`, `some`, `none`, `error`, `string`, `structure` and
  `enumeration` — each the `Transfer` that `Transfer::of` makes of the `Value`
  constructor of the same name — so a host answers a parked run without
  building an `Rc` value on a thread that never runs Cove (edge README item 7).
- `cove_sema::package::load_module(root, name, sources)`: the `.cove` files
  directly in `root/<name>` as module `name`, with the standard library (edge
  README item 6).
- `cove_runtime::testing`: `TestRun { program, sources, schemas, backend,
  limits }.run(test, registry)` lowers, runs and reports one `DeclaredTest` by
  `cove test`'s rules, and `testing::report` reports an outcome obtained some
  other way. `cove test` and `cove-edge test` both call it, keeping only their
  own grant policy (edge README item 10).

## Consequences

- An embedder can bound each isolate's heap before it runs, and a runaway app
  fails its own allocation rather than the process's.
- `Cancellation` is no longer one atomic behind its `Arc`: it carries a
  mutex and a condition variable, allocated once per flag. A spawned task makes
  one flag of its own, so a spawn pays for them; a spawn already pays for a
  thread, and nothing here measured the difference.
- A callback runs on the cancelling thread. A host that cancels from a latency-
  sensitive thread and registers a slow callback has made its own problem; the
  documentation says to keep them short.
- `can_spawn` is a fact every function has, computed for every package, at the
  cost of one more set in a fixed point that already runs.

## What this does not decide

- Whether a capacity should also be settable on `Vm::with_prepared` (the
  borrowed machine). Nothing has asked.
- Running a `test fn` parked, through `call_parkable`. `testing::report` is the
  half of it a parked runner would need; the runner is not written.
- A heap capacity in `cove.toml` for `cove run`. A command-line run is one
  process for one program, and the default has not been in anyone's way.

## Alternatives considered

- **`Limits::max_heap_words`.** Rejected for the reason in Decision 1: it
  would sit beside bounds on the run and read as a memory ceiling, which ADR
  0011's amendment already found to be a false promise.
- **A `Future` from `Cancellation`.** Would tie the runtime to `std::task` and
  to a registration scheme per poll, for a need a callback meets.
- **A `can_spawn` computed after type checking, from `Scope.spawn` calls.**
  Exact where this is not — a scope that spawns nothing — at the cost of a
  second propagation in a different pass; the over-approximation it removes is
  not one a program writes on purpose.
