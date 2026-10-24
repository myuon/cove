# ADR 0118: The interpreter lets a `Shared` cell hold a handle to itself

- Status: Accepted
- Date: 2026-10-25
- Decides: that the tree-walking interpreter's `lock` stores a value that
  names the cell being locked, as the linear-memory backend has since ADR
  0037, so the two evaluators answer alike on such a program; and that the
  cycle it makes is freed by the run-end collection of ADR 0115. Recorded for
  [issue #695](https://github.com/myuon/cove/issues/695)
- Supersedes: [ADR 0037](0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)'s
  consequence that "the predecessor keeps its behaviour until it is deleted",
  for the interpreter's refusal of a cell that would hold itself
- Refers to, without superseding:
  [ADR 0011](0011-garbage-collection.md) (whose amendment made the refusal,
  and which ADR 0037 already superseded for it) and
  [ADR 0115](0115-the-interpreter-reclaims-a-cell-cycle-when-its-run-ends.md)
  (the collection that makes the refusal unnecessary)

## Context

ADR 0011's amendment had `SharedCell::lock` refuse a closure whose result
would leave the cell holding a handle to itself. It rested on two facts: the
check was a pointer comparison on a walk `lock` already made, and nothing
reclaimed an `Arc` cycle among cells, so a refusal was better than a silent
leak. A cycle through two or more cells was not caught, and leaked.

ADR 0037 removed the refusal on the linear-memory backend, where a cell is an
ordinary object in a traced heap and the collector frees a cycle through it.
It left the interpreter refusing, under the consequence that the predecessor
"keeps its behaviour until it is deleted". The interpreter was not deleted:
it is the oracle every differential comparison, `cove test --backend ast`
and the fuzz targets of ADRs 0112 and 0113 hold the machine to. So the same
program — ADR 0011's own example, `n.lock(fn(var value) { value =
Node(cell: Some(n)) })` — ran on one evaluator and raised a runtime error on
the other. `vm/differential.rs` carried it as the one case that was not
`agree`.

ADR 0115 then took away the second fact for the interpreter too: when a run's
entry returns, `Cells::reclaim` frees every cell of the run that nothing
outside the cells reaches. A cell that holds itself is the shortest such
cycle. Its count of handles from inside the cells includes the self-edge,
because `Transfer::each_cell` yields the handle the cell holds to itself like
any other, so a self-holding cell nothing else names is found unreachable and
emptied. ADR 0115 said the refusal was now unnecessary and left removing it
to its own decision, because removing it changes what the interpreter
answers.

## Decision

**The interpreter's `lock` stores what the closure leaves whatever cells it
names, including the cell being locked.** The refusal, its message ("this
`lock` would leave the cell holding a handle to itself, and no collector
reclaims that cycle") and its rule ("`Shared` ownership must stay acyclic")
are removed, together with `Transfer::reaches`, which existed only for it.

Storing a handle to the locked cell does not lock it again. Converting the
closure's result to a `Transfer` copies a `Shared` as its `Arc` and never
looks inside a cell, which is the reason the refusal could be a pointer
comparison in the first place. A `lock` taken *through* the stored handle
while the cell is still held is the reentrancy error ADR 0037 kept, unchanged,
on both evaluators.

Nothing about printing changes: the interpreter renders a `Shared` as the
handle it is, without reading what it holds, so a cell that holds itself
renders in finite time.

## Consequences

- The interpreter and the linear-memory backend answer alike on a program
  that stores a cell into itself. `vm/differential.rs`'s
  `a_cell_may_come_to_hold_itself` is now an `agree`, and
  `tests/e2e/shared_cycle` builds such a cell and is compared on the encoded
  VM, the interpreter and `--backend native` (where the native tier refuses
  `SharedLock`, so the functions that lock stay in the VM).
- `crates/cove-runtime/tests/shared_cycles.rs` holds that the cycle is freed
  when the entry returns, and that one the answer keeps survives until the
  answer goes and the table is reclaimed.
- A program that relied on the refusal to find a mistake now runs, and the
  cycle it builds lives until its run ends, unless something outside the
  cells still reaches it. That is what the linear-memory backend already did,
  modulo when its collector runs.
- `lock` on the interpreter skips one walk over the stored value.

## What this does not decide

- when the linear-memory backend collects, which is ADR 0037's and unchanged;
- anything about a ring the embedder keeps past the entry, which is ADR
  0115's.
