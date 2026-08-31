# ADR 0024: A stop is a bound, not a point

- Status: Accepted
- Superseded in part by
  [ADR 0030](0030-a-host-call-asks-the-fuel-limit.md), which replaces
  "A Host call is a stop point for every flag and for no budget" in the one
  clause that exempts fuel, and the bound that clause states — that Host
  effects under an exhausted fuel budget are bounded by one straight line's
  worth of them. A Host call now asks the fuel limit too, and the bound is
  zero on both backends
- Date: 2026-08-29
- Supersedes: [ADR 0003](0003-task-execution-and-runtime-control.md)'s
  decision that the runtime controls are "all checked at defined safepoints —
  loop back edges, calls, and `await`", in the part that names the operations
  rather than a bound. Everything else in ADR 0003 stands and this ADR leans
  on it: the controls themselves, that they are runtime controls and not
  termination proofs, and its amendment that a blocking Host call must
  cooperate, which is the same argument applied to the one place a backend
  cannot reach
- Supersedes: [ADR 0019](0019-executable-ir-and-vm.md)'s
  "Fuel is charged for VM work", in the part of its consequences that says
  "anything comparing runs across backends must compare outcomes, not fuel".
  Under a fuel limit the outcomes differ too. The decision itself stands
  whole, and this ADR is what its own sentence — "that is accepted and must be
  documented per backend rather than papered over" — asks for
- Implemented by: [PR #143](https://github.com/myuon/cove/pull/143), closing
  [issue #120](https://github.com/myuon/cove/issues/120)
- Implementation status: complete. `docs/VM_ARCHITECTURE.md` states each bound
  in prose and `crates/cove-runtime/tests/responsiveness.rs` measures every one
  of them on both backends

## Context

[ADR 0003](0003-task-execution-and-runtime-control.md) built the runtime
controls and said where they are checked, in the terms the only backend then
had: "loop back edges, calls, and `await`". That was a list of operations, and
it was exact, because the tree walk charges a fixed amount at each of those
three and nowhere else.

[ADR 0019](0019-executable-ir-and-vm.md) built a second backend, and PR #114
made it charge fuel a basic block at a time and check a back edge only once
`BACK_EDGE_FUEL` had gathered. Both are sound optimizations and both were
measured. Neither is describable in ADR 0003's terms: the VM checks at a
subset of back edges, at every block head where a threshold is crossed, and at
places a tree walk has no name for.

`docs/VM_ARCHITECTURE.md` covered the gap with a claim rather than a
statement — that the difference is not one a program can be written to
observe. [Issue #120](https://github.com/myuon/cove/issues/120) said that is
too strong, and it is. A program can observe it, three ways: it can be
cancelled and keep running for a bounded but nonzero while; it can be handed
back a partially mutated `Shared` cell by a call a timeout stopped; and, under
a fuel limit, it can stop on one backend and answer on the other.

The last of those is the one that matters most, because it is not a matter of
degree. The VM charges a whole straight line on arriving at its head and the
safepoint that may refuse comes after the charge, so a block whose extent
exceeds what is left of the budget is refused *entire*, with none of the
prefix that would have fitted executed. The tree walk charges nothing for
straight-line work at all. Four hundred assignments in a row, under a fuel
limit of half what the VM charges for them: the VM stops and the tree walk
answers `Ok(400)`.

So "compare outcomes, not fuel" is not enough. There is no unit in which the
two backends' fuel means the same thing, which ADR 0019 decided and this ADR
does not disturb, and there is therefore no fuel limit at which their outcomes
are guaranteed to agree either.

Writing the contract down found three faults, which is the argument for
writing it down. A host polling `Reentry::is_cancelled` from inside a
cancelled task was told the task was fine, on the VM only. A Host effect could
follow a raised stop flag on both backends, because a Host call was not a
place either of the two thread-owned flags was read. And a run that ended by
raising, or by being stopped, lost the fuel it had gathered since its last
safepoint, so `fuel_spent` under-reported the work by up to a whole safepoint
interval.

## Decision

### A stop is stated as a bound, and the bound is a constant known before the run

Each way a run can be stopped states a maximum: how much work may still
happen after the stop becomes true, in that backend's own fuel, and what may
still be observed afterwards. A schedule may batch its checks, and both
backends do, provided what it gathers between two checks is bounded by a
constant fixed before the run — `BACK_EDGE_FUEL` and `SAFEPOINT_INTERVAL` on
the VM, `SAFEPOINT_FUEL` and `DEADLINE_CHECK_INTERVAL` on both.

That replaces naming the operations. Naming operations was exact for one
backend and has no meaning across two; naming a bound is checkable on any
number of them, including one nobody has written.

### The two backends satisfy the same bound and are not required to stop at the same point

They stop at different source operations for the same program under the same
limits. They spend different fuel for the same work. Under a fuel limit, one
may stop where the other answers.

What they are held to is: a stated maximum in each backend's own units; the
same stop reported as the same `RunOutcome`, in the same words, with the same
trace event; and the effect rules below, identically. Anything that compares
two backends' runs compares those. A differential test that sets a fuel limit
low enough to reach is comparing the backends' schedules, which is not what a
differential test is for; `crates/cove-cli/tests/differential.rs` sets none,
and that is now a decision rather than an accident.

### A Host call is a stop point for every flag and for no budget

Three stops are flags — the run's cancellation, a task's own, and a bounded
call's — and a flag is already true or already false, so reading one costs an
atomic load. All three are read before every Host call, so no Host effect
follows a raised flag. `Budget::charge_host_call` reads the run's, and
`crate::interp::stopped_here` reads the other two, which a `Budget` cannot
because it is shared by every task of a run.

Three stops are budgets. The deadline and `max_host_calls` are measured at
every Host call, because a call that waits is the one thing a Cove-side
safepoint cannot bound and because a Host call already costs more than reading
a clock. **Fuel is not**, and deliberately: measuring fuel means spending what
a backend has charged and not yet handed over, and not doing that per
instruction is the whole of why block charging exists. So the bound on Host
effects under an exhausted fuel budget is one straight line's worth of them,
and **`max_host_calls` is the control that bounds effects exactly**, fuel is
the control that bounds work, and the deadline is the control that bounds
time.

### What a stop may leave behind is stated, not minimized

Host effects already performed stay performed; nothing is rolled back, and the
Host API schema's `Effect::IrreversibleWrite` is the field that says which
ones could not be. No value is ever half written, because a stop is taken
between two instructions. A stopped call's writes to its own locals go with
its frame, so what a surviving caller sees is only writes to storage they
share, and those are whole turns of a loop rather than fractions of one. And a
stop is at an expression rather than at a statement: a call is a safepoint, so
`f(a) + g(b)` can stop with `f`'s effects made and `g`'s not.

### Pending fuel is never lost

A backend that charges in batches spends what it has charged at a safepoint,
and every exit that reaches no further safepoint is an exit its last charge
could go out with. Whatever a run charged is counted, however the run ended.
The invariant is that a VM run's `fuel_spent` is never below the instructions
it charged for.

## Consequences

- `docs/VM_ARCHITECTURE.md` carries the table of bounds, and a change to any
  of the four constants changes a number there. That is intended: the
  constants are the contract's arithmetic and they are public for that reason.
- A third backend is held to the bounds and not to the schedule. What it owes
  is a maximum per stop mode, the same `RunOutcome` for the same stop, and the
  effect rules; what it does not owe is stopping where either of these two
  does.
- A host that wants to bound what a run can do to the outside world sets
  `max_host_calls`. Setting only `fuel` bounds work and bounds effects only to
  within a straight line, and the documentation now says so where an embedder
  reads it.
- A fuel limit is not portable between backends. A program tuned to a fuel
  limit on one may stop on the other, and an embedder that moves a run between
  them re-measures the limit. ADR 0019 already made `fuel_spent`
  backend-specific; this is the part of that which is about the *limit* rather
  than the report.
- The bounds are tested rather than asserted:
  `crates/cove-runtime/tests/responsiveness.rs` measures a control run's cost
  per turn and its prefix, and holds the stopped run to their sum plus one
  gathering, on both backends, for every stop mode. A constant tightened makes
  those tests pass with more margin; a constant loosened past what a comment
  claims makes them fail.
- ADR 0003's and ADR 0019's headers gain a pointer each, and nothing else.
