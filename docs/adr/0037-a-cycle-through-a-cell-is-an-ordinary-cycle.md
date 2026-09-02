# ADR 0037: A cycle through a `Shared` cell is an ordinary cycle

- Status: Accepted
- Date: 2026-09-02
- Supersedes: [ADR 0011](0011-garbage-collection.md)'s
  "Amendment (2026-08-25): a policy for cycles among cells", which made
  `Shared` acyclicity a rule of the language and had `lock` refuse the one
  cycle it could see. The rest of ADR 0011 stands, including the collector
  itself and the second amendment
- Decides: the question
  [issue #240](https://github.com/myuon/cove/issues/240)'s Q11 raised, when
  building `Shared` on the linear-memory backend

## Context

ADR 0011's amendment decided a policy for cycles among cells, and it named
the two facts it rested on:

- *"Committing a `lock` already walks the whole value the closure leaves,
  once, to convert it to a `Transfer`"* — so refusing a direct cycle cost
  nothing beyond a pointer comparison on a walk that was happening anyway;
- *"The per-task heap does not track cells at all — a `Shared` is a leaf to
  it, by design"* — so the collector could not answer the question, and a
  cycle through two or more cells stayed *"an accepted, documented leak"*.

It also named the condition for revisiting: *"The stop-the-world collector
for cells stays exactly where the section above left it: worth building
once `Shared` sees enough real use."*

Both facts are properties of the implementation that was current, and
[ADR 0034](0034-one-physical-word-stack.md) replaced it. In the linear memory
a `Shared` cell is an ordinary object in the run's one traced heap: a lock
word and the wrapped value inline. The values reachable through it are
ordinary objects in the same heap. There is no `Transfer` and no walk to
attach the check to, and the collector the amendment deferred is the
collector that is running.

So the amendment's decision now rests on nothing, and its rule is enforced by
a mechanism that no longer exists in the backend being built. Leaving it
would mean **reconstructing** the walk for the sole purpose of preserving a
refusal the collector makes unnecessary.

## Decision

**A cycle through one or more `Shared` cells is an ordinary object-graph
cycle, and is collected when it becomes unreachable.**

`lock` no longer refuses a closure that leaves the cell holding a handle to
itself. `Shared` ownership does not have to stay acyclic, and the Language
Card no longer says it does.

Two alternatives were considered and rejected, and the reasons are worth
keeping because they are the same reasons in two shapes:

- **Reconstruct the walk in the machine.** It would impose a cost on every
  `lock` for a condition the collector already handles, and — as the
  amendment itself says — it would still detect only the direct case. Paying
  for a partial answer to a question that now has a complete one is the
  wrong trade in both directions at once.
- **Move it to `cove-sema`**, as [ADR 0035](0035-a-value-type-may-not-contain-itself.md)
  did for recursive layouts and as issue #240's Q10 did for `freeze()`'s
  uniqueness. General heap acyclicity is not statically decidable under
  Cove's aliases, calls, branches, collections and Host interactions without
  a substantially stronger ownership or type discipline than the language
  has. The two analyses that *were* moved are both about a **declaration** or
  a **local** fact; this is about the shape of a running heap.

### Reentrant locking is a separate rule and stays

Locking a cell a task already holds is still rejected rather than made to
wait. These are different questions and only one of them changed:

- **leaving a cell containing a reference to itself** is heap topology, and
  the collector owns it;
- **locking the same cell twice from one task** is a live lock-state error,
  and no collector can answer it.

The amendment's own reasoning kept them together only because one walk
happened to be able to see both.

## Consequences

- `tests/e2e/fail_shared_cycle` no longer describes the language. It is
  replaced by coverage that establishes the new semantics, and — where it can
  be written without depending on when a collection runs — a test that an
  unreachable cycle through one or more cells is reclaimed.
- The Language Card loses *"`Shared` ownership must stay acyclic … A cycle
  through two or more cells is not detected and leaks."* The leak it
  documented is gone rather than merely undetected, which is the substantive
  change: the amendment refused the case it could see and leaked the rest,
  and now neither happens.
- The predecessor keeps its behaviour until it is deleted. It is frozen, and
  a program that its runtime refuses and the replacement runs is one more
  reason the two do not coexist past the cutover.
- Nothing else about `Shared` moves. It is still the one synchronized handle
  ([ADR 0008](0008-concurrent-task-execution.md)), still the only way two
  tasks reach one mutable value, and its lock word is still what publishes
  everything a holder wrote.

## What this does not decide

- whether a weak handle is ever added — ADR 0001's scope list still says no,
  and nothing needs one;
- when a collection runs, which is ADR 0011's and unchanged;
- anything about cycles that do not pass through a cell, which were already
  ordinary.
