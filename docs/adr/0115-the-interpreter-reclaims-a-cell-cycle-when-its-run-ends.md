# ADR 0115: The interpreter reclaims a cycle of `Shared` cells when its run ends, and the fuzz runs check for leaks

- Status: Accepted
- Date: 2026-10-24
- Decides: that a cycle among `Shared` cells the tree-walking interpreter
  built is freed when the run's entry returns, or when the run's `Runtime`
  goes, unless something outside the cells still reaches it; and that the
  fuzz targets run with LeakSanitizer on where it exists. Recorded for
  [issue #688](https://github.com/myuon/cove/issues/688)
- Supersedes: [ADR 0011](0011-garbage-collection.md)'s "What this leaves
  uncollected", for the interpreter — the decision that a cycle through two
  or more cells is an accepted leak
- Refers to, without superseding:
  [ADR 0037](0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md) (a cycle
  through a cell is an ordinary cycle on the linear-memory backend),
  [ADR 0112](0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md)
  and [ADR 0113](0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md)
  (the fuzz targets), and
  [ADR 0114](0114-what-a-failed-run-leaves-behind.md) (the cells a run holds
  are per-run state)

## Context

LeakSanitizer stopped `differential_source`'s pull-request smoke on Linux
with 1,692 bytes in 36 allocations, all of them *indirect* leaks. Run under
LeakSanitizer one input at a time, the 411-input seed corpus has exactly two
inputs that leak, `tests/e2e/fail_key_known_shared` and
`tests/e2e/shared_cycle`, and every stack ends in the tree-walking
interpreter:

```text
Indirect leak of 768 byte(s) in 4 object(s) allocated from:
  #0 malloc
  #1 <cove_runtime::task::Transfer>::convert
  #2 <cove_runtime::shared::SharedCell>::lock::<..., Interpreter::call_shared_method::{closure#1}>
  #3 <cove_runtime::interp::Interpreter>::call_shared_method
  ...
  #23 <cove_runtime::interp::Interpreter>::run_entry
  #24 cove_fuzz::differential::ast_run
Indirect leak of ... allocated from:
  #0 malloc
  #1 <cove_runtime::shared::SharedCell>::wrap
  #2 cove_runtime::builtins::call_constructor
  ...
```

Both programs build two cells that hold each other. On the interpreter a cell
is an `Arc<SharedCell>`, and the per-task heap never looks inside one, so a
ring of them is an `Arc` cycle that nothing frees. ADR 0011 knew this and
accepted it: collecting cycles among cells "needs a collector that stops
every thread", which it ruled out. ADR 0037 removed the leak on the
linear-memory backend, where a cell is an ordinary heap object, and left the
interpreter's behaviour alone.

ADR 0114 now says the cells a run holds are per-run state that must not
survive the run. A ring the interpreter built and dropped survives it, for
the life of the process.

ADR 0011's objection was to stopping threads that are running. There is a
moment in every interpreter run when no thread of it is running: when the
entry has returned. Leaving a task scope waits for or cancels its children,
so by then every task has been joined and every task heap retired. A
collection at that moment does not have to stop anyone.

## Decision

### 1. The run's cells are collected when the entry returns

The `Runtime` keeps a table of `Weak` handles to every cell the interpreter
creates (`Shared(value)` is the one place). When the entry returns, after the
entry's own heap has been retired, `Cells::reclaim` collects them as one
graph, by the argument `crate::heap` already makes for vectors:

- for each cell, count the handles to it that sit inside cells, and compare
  that with `Arc::strong_count`. A shortfall is a handle held outside — by
  the answer, by a value the embedder kept, by a host — so that cell is a
  root;
- mark everything the roots' contents reach;
- empty what is left. Emptying a cell drops the handles it held, so `Arc`
  frees the ring.

The table is reclaimed again when the last clone of the `Runtime` is
dropped, which catches a ring the answer held past the entry and was then
dropped.

Every cell is locked with `try_lock` for the whole of the count. If any is
held — a thread of another run sharing the `Runtime`, say — the reclamation
is skipped rather than waited for. With every lock held no cell's contents
can change, and a handle outside the cells can only be copied from another
such handle, so a cell found unreachable stays unreachable.

The table prunes the handles of cells `Arc` has already freed whenever it
doubles, so it is the size of what is live, not of everything ever made.

### 2. The fuzz targets run with LeakSanitizer on

`scripts/fuzz.sh` no longer turns LeakSanitizer off, so on Linux a leak
fails the smoke: either per input, where libFuzzer names the input, or at
exit. `docs/FUZZING.md` said leaks were "not what this is for". That was
right about memory safety and wrong about cost: a cycle a run leaves
behind is a reference left behind, which ADR 0114 counts as a fault, and
the one tool that sees it was switched off. It was a note in the manual, not
an ADR decision, so nothing is superseded for it.

## Consequences

- An interpreter run that builds a ring of cells and drops it returns that
  memory. `crates/cove-runtime/tests/shared_cycles.rs` holds that on every
  host, macOS included, by asking the run's table how many cells are alive:
  zero after a run that dropped its ring, built on the entry's thread or a
  task's; two while the answer holds one, and zero once it is dropped and
  the table reclaimed.
- Creating a `Shared` on the interpreter takes one more lock and pushes one
  `Weak`; a run's end walks its live cells once. Neither changes a
  performance class: `Shared(value)` already walks `value` to convert it and
  allocates the cell.
- A ring an embedder keeps past the entry and drops while the `Runtime` lives
  is freed at the next run's end or at the `Runtime`'s drop, not at once.
- On Linux the fuzz smoke now fails on a leak. A leak a deliberate
  process-lifetime cache makes would have to go in a suppression file; none
  was needed.

## What this does not decide

- whether the interpreter's `lock` stops refusing a cell that would hold a
  handle to itself, as the linear-memory backend did under ADR 0037. That
  refusal is now unnecessary on either backend, but removing it changes what
  the interpreter answers, and that is a decision of its own;
- anything about a cycle that does not pass through a cell, which the
  per-task heap already collects;
- when the linear-memory backend collects, which is ADR 0037's and unchanged.
