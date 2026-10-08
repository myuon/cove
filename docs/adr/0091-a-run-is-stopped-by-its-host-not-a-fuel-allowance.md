# ADR 0091: A run is stopped by its host, not a fuel allowance

- Status: Proposed
- Date: 2026-10-08
- Supersedes, on acceptance:
  [ADR 0003](0003-task-execution-and-runtime-control.md)'s fuel-budget requirement;
  [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md) and
  [ADR 0040](0040-a-bound-outlives-its-backend.md), only their fuel-exhaustion
  and pending-fuel accounting contracts;
  [ADR 0030](0030-a-host-call-asks-the-fuel-limit.md)'s requirement that a host
  call ask the fuel limit
- Refers to:
  [ADR 0082](0082-a-parked-run-keeps-its-deadline.md),
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md),
  [ADR 0086](0086-a-yield-request-makes-compiled-code-poll.md),
  [ADR 0088](0088-an-embedder-sizes-the-heap-and-is-told-of-a-cancellation.md),
  [ADR 0089](0089-a-bulk-safepoint-offers-the-yield-at-the-next-instruction.md),
  and [ADR 0090](0090-a-host-call-s-charge-begins-a-stride-for-a-yield-too.md):
  cancellation, deadlines and yielding remain independent of fuel
- Decides: remove the fuel allowance and its public contract; retain the
  mechanisms needed to interrupt and schedule execution
- Implementation status: implemented on branch `feat/remove-fuel` (the pull request that removes fuel)

## Context

Fuel was introduced with the runtime controls in ADR 0003. Subsequent ADRs
made it a real contract: exhaustion bounds further execution, pending work is
never lost, and a host call is refused once the charged total reaches its limit.

The question now is whether that contract earns its place. A caller generally
cannot predict the allowance a useful program needs. Instruction work is not
elapsed time or CPU time, and ADR 0024 already says a fuel limit is not portable
between backends. A caller must learn an implementation-specific number before
it can choose an allowance that permits useful work.

Unpredictability does not make fuel useless. An allowance can still bound work
without predicting completion, and a fixed evaluator may use it for repeatable
cutoffs. Those are specialized requirements, however; no current representative
use requires retaining a general fuel-budget API for them.

The multi-app host needs to stop runaway execution and schedule competing runs.
It already has cancellation, deadlines and yielding as distinct controls.
Needing an interruption mechanism does not imply needing an instruction-count
allowance. PHILOSOPHY's “earn complexity through use” favors removing the latter
until a concrete use requires it.

## Decision

### 1. Remove the fuel allowance, not just its public spelling

Remove fuel configuration from embedding APIs, CLI options, package run
configuration and built executables, together with enforcement, exhaustion
errors and fuel-only terminal outcomes. Do not keep an undocumented allowance
or introduce another instruction-budget API in its place.

This is a compatibility change. Existing fuel configuration must receive an
explicit migration diagnostic rather than silently becoming an unlimited run.
New traces do not report fuel exhaustion. Existing trace/replay fixtures and
any historical-format support must be inventoried during implementation;
this ADR does not require deleting historical records.

### 2. Keep interruption and scheduling separate from that allowance

Cancellation, wall-clock deadlines, host-call limits, call-depth limits,
concurrency controls, heap capacity and yielding are unchanged by this decision.
An embedder chooses the deadline and cancellation policy; the runtime continues
to enforce the controls it already offers. No runtime deadline is removed or
replaced by an external timer-only convention.

Safepoints, poll thresholds, stride counters and host-boundary checks are not
deleted merely because their current names or implementation mention fuel.
They also serve cancellation, deadlines, collection and yielding.

Keep internal work counters where those mechanisms need them. Rename or split
them when their names imply an allowance that no longer exists. Remove
fuel-only accounting, including pending-charge flushing needed solely to
enforce or report the allowance. Instruction counts used for diagnostics or
benchmarks are a separate concern and are not removed by this ADR.

### 3. Preserve the remaining stop and yield contracts

The non-fuel bounds and effect rules in ADRs 0024 and 0040 still stand.
Running code must observe cancellation and deadlines within its stated bounds;
stopping only the caller's wait is insufficient. Host-boundary cancellation,
deadline and host-call checks remain.

A parked or yielded run retains its deadline and cancellation behavior.
Removing fuel must not prevent a host-call-heavy loop or a bulk operation from
offering a yield, nor leave a resumed run permanently unable to yield.
The host-boundary and stride mechanisms described in ADRs 0086, 0089 and 0090
must be preserved by purpose, even when fuel-charging bookkeeping is removed.

A blocking host call still owes the cooperation or explicit limitation described
in ADR 0003. Cancellation is cooperative; this decision does not claim that the
runtime can forcibly interrupt arbitrary host code. Process isolation remains a
host choice where that stronger guarantee is required.

CPU-time metering or enforcement is not added here. A wall-clock deadline
includes waiting and is not a CPU-time budget.

## Consequences

- Callers no longer choose or tune an evaluator-specific fuel allowance.
- The runtime loses deterministic work-budget exhaustion as a supported stop mode.
- Fuel-only accounting and enforcement can be removed. Any speed improvement
  must be measured; cancellation and scheduling checks still have a cost.
- A host that previously relied only on fuel must choose a deadline or a
  cancellation policy appropriate to its use before migrating.
- Fuel is not repurposed as billing, a scheduling quantum or a hidden safety
  limit. Internal scheduling work counts may survive without a public allowance.

## Migration and validation

1. Inventory fuel across all execution backends, embedding APIs, CLI and package
   configuration, sealed builds, traces, replay, examples and tests. Inventory
   dependencies on the same counters separately.
2. Remove the allowance and fuel-only stop/reporting paths. Update callers and
   configuration diagnostics, and document the compatibility change.
3. Replace tests that used fuel as a convenient stopping mechanism with the
   control they actually intend to test. Remove fuel-exhaustion assertions;
   preserve interruption, host-effect, resource-cleanup and yield coverage.
4. Verify cancellation and deadlines for compute loops, straight-line work,
   bulk operations, host-call-heavy loops, callbacks and tasks on each supported
   evaluator. Verify parked/yielded cancellation, expired-deadline resume and
   subsequent runs on the returned machine.
5. Verify yield responsiveness and resumed execution on the dispatch and native
   tiers, including the host-call and bulk-operation cases from ADRs 0089/0090.
   Do not loosen a remaining documented bound as an incidental cleanup.
6. Run the repository's implementation gates and representative benchmarks.
   Report performance with and without host calls; do not infer improvement
   merely from deleting a counter.

On acceptance, add the supersession pointers to the affected older ADR headers
without changing their bodies, as CLAUDE.md requires. Broad ADRs are superseded
only in the fuel clauses named above. This proposed document leaves their
accepted headers untouched until that decision is made.

## Alternatives considered

**Keep fuel public.** Useful for repeatable work cutoffs, but makes every caller
face a unit that current uses do not need. Retaining a capability because a use
is imaginable is insufficient.

**Keep fuel private.** Hides configuration while retaining the enforcement and
accounting burden. There is no concrete internal consumer requiring it.

**Replace fuel with a new runtime time-budget feature.** The runtime already
has wall-clock deadlines and cancellation. This decision needs no new budget
system and does not confuse elapsed time with CPU time.

## Reconsideration

Reconsider a work allowance when a concrete consumer needs repeatable cutoffs
or a computation budget independent of wall time, and can state the required
units and stability guarantees. That future requirement does not justify
retaining fuel today.
