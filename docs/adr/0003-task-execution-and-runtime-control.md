# ADR 0003: Task execution and runtime control

- Status: Accepted, superseded in part by
  [ADR 0008](0008-concurrent-task-execution.md)
- Date: 2026-08-25
- Superseded by: [ADR 0008](0008-concurrent-task-execution.md), which took over
  phase 2 of the Decision below and, with it, the whole of "What Phase 1 does
  not validate"
- Implemented by: PR #6 (phase 1); PR #23 replaced phase 1's execution model
- Implementation status: complete — phase 1 shipped whole, and phase 2 became an
  ADR of its own rather than a later commit against this one

## Context

ADR 0001 makes the runtime as important as the compiler: grants, cancellation,
deadlines, CPU and memory budgets, and trace events that separate CPU work from
I/O wait are listed as the second deliverable of the MVP. The Language Card
adds the language-visible half of that contract:

> Concurrent work belongs to a task scope. Leaving the scope waits for or
> cancels its child tasks. Immutable task-safe values such as arrays may cross
> task boundaries. A vector cannot cross, even through `let`.

None of it exists. `async fn`, `await`, `scope`, and `spawn` parse and resolve,
and the interpreter stops on them with "not implemented yet". Three of the seven
representative programs are blocked behind that message.

The interpreter is a recursive tree walker holding `Rc` values. Nothing about it
can suspend a computation, which is what real interleaving requires.

## Considered options

**A single-threaded cooperative scheduler** would give real interleaving,
cancellation, deadlines, and I/O attribution while keeping `Rc`. It requires
rewriting the interpreter in continuation-passing style or as an explicit stack
machine, because a recursive Rust tree walker cannot pause mid-expression.

**A thread per task** would also give real interleaving, and needs *less*
interpreter surgery: the tree walker keeps working, one instance per thread.
The cost lands on the value representation instead. Rust demands `Send` for
anything crossing a thread, and `Rc` is not `Send` — but Cove already forbids
sharing mutable structure across tasks, so the only values that may legally
cross are exactly the ones that can be copied at the boundary. The language
rule and the implementation constraint are the same rule.

**Deterministic sequential execution** runs a spawned task to completion where
it is spawned. It gives no interleaving at all, so timeouts, cancellation, and
CPU-versus-wait attribution are shapes without substance.

## Decision

Do the third option first, deliberately and temporarily, and build the runtime
controls around it.

**Phase 1 — semantics and plumbing.** Implement `async fn`, `await`, `scope`,
and `spawn` with sequential execution: `spawn` defers a task, `await` runs it,
and leaving a scope runs or cancels whatever the body did not await. Implement
the runtime controls for real, not as placeholders: a fuel budget, a wall-clock
deadline, a cancellation flag, a call-depth limit, and host-call accounting,
all checked at defined safepoints — loop back edges, calls, and `await`. Emit
trace events for task spawn, completion, and cancellation, and for every host
call.

**Phase 2 — real concurrency.** Replace sequential execution with a thread per
task, and make the task boundary a typed conversion between a `Send` transfer
value and the interpreter's `Rc` values. That conversion is where the Language
Card's task-safety rule stops being prose: an `Array` of task-safe values
converts, a `Vector` does not, and a closure converts only when every capture
does. `Shared<T>` becomes the synchronized handle the card already names.

Phase 1 is chosen for sequencing, not because sequential execution is
defensible as an end state. The budgets, safepoints, trace events, and host
accounting are the parts that every later model needs, and they can be built
and tested without a scheduler. Doing them first means Phase 2 replaces one
component instead of growing three.

## What Phase 1 does not validate

Stating this plainly is part of the decision.

- There is no interleaving, so a deadline can only interrupt a task at a
  safepoint inside its own execution, never while another task runs.
- `clock.timeout` can fail a task that overruns, but cannot overlap the work it
  is timing with anything else.
- Trace events distinguish CPU work from host-call wait, but with one task
  running at a time the distinction is not yet a concurrency measurement.
- ADR 0001's success criterion that "CPU time and I/O wait are accurately
  attributable in traces" is therefore not met until Phase 2.

The MVP is a hypothesis test. Recording an unmet criterion is more useful than
producing a number that looks like it was met.

## Consequences

`examples/tasks/` runs at the end of Phase 1. `examples/server/` and
`examples/callbacks/` additionally need `http`, `database`, and `Shared<T>`,
and follow separately.

Sequential execution must not leak into the language's meaning. `spawn` returns
a handle whose value is only observable through `await`, and a scope's exit
behaviour is defined by the Language Card rather than by when the interpreter
happens to run the body. Any test that would pass only because tasks run
sequentially is a test of the implementation, not of Cove, and should be
written so Phase 2 does not have to change it.

## Superseded in part (2026-08-25): phase 2 became ADR 0008

Phase 1 shipped as written, and almost all of it is still what the runtime
does. Fuel, a wall-clock deadline, a cancellation flag, a call-depth limit,
and host-call accounting are checked at exactly the safepoints named above —
loop back edges, calls, and `await` — and a task spawn, completion, or
cancellation, and every host call, is still an event. Those parts were built
first precisely so that they would survive the change of execution model, and
they did.

What did not survive is sequential execution itself.
[ADR 0008](0008-concurrent-task-execution.md) took over phase 2 rather than
leaving it a later commit against this ADR, so `spawn` starts a thread and
`Shared<T>` is defined there rather than here. The consequence for a reader is
that the Decision's phase 1 paragraph and the whole of "What Phase 1 does not
validate" describe what was true between PR #6 and PR #23, and describe
nothing about the runtime today: tasks interleave, a deadline stops a task
while another runs, and `clock.timeout` overlaps the work it bounds. ADR
0001's success criterion about attributing CPU time and I/O wait is the one
line of that section still worth checking rather than assuming — a trace now
carries each task's own CPU and each host call's wait, but not which task made
the call, which `cove trace` says of itself under "not carried by these
events".
