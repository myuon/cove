# ADR 0119: A held closure keeps only the resource names it reaches

- Status: Accepted
- Date: 2026-10-25
- Decides: that while an object of a linear-memory machine's heap is pinned
  for a value an embedder holds, the machine's resource table is trimmed at a
  run's edge to the names the pinned objects reach — found by walking them
  for their `Repr::Host` words — rather than kept whole; that a kept name
  stays at the word that already names it and a dropped one leaves a vacant
  slot the next name takes; and that the walk is paced by the names it can
  drop. Recorded for
  [issue #694](https://github.com/myuon/cove/issues/694)
- Supersedes: [ADR 0116](0116-a-machine-s-resource-table-is-emptied-between-runs.md)'s
  decision 2, that the table is kept whole while anything is pinned
- Refers to, without superseding:
  [ADR 0013](0013-host-resource-handles.md) (a handle is equal to another
  when they name the same resource) and
  [ADR 0031](0031-a-host-handle-is-not-a-vm-handle.md) (why the table is not
  the heap)

## Context

ADR 0116 made the resource table per-run state, emptied at both edges of
every entry, with one exception: while any heap object is pinned for a value
an embedder holds — a closure the embedder kept — the table is kept whole,
because a `Repr::Host` word in that closure's environment indexes it. It
recorded the residue: a machine whose embedder keeps one closure for its
whole life keeps the old growth for that life, one name per new resource per
run. Measured on `crates/cove-runtime/tests/lifecycle.rs`'s fixture, a machine
holding a closure with one ticket in it and run 20,000 times on a run that
mints and redeems a ticket held **20,001** names at the end.

ADR 0116 named the refinement and declined it — "a heap walk on every edge
where something is pinned, and no embedder here keeps a handle-capturing
closure across runs". Issue 694 asks for it: a resident machine whose
embedder keeps a handler closure for the life of the process
(`http.Server.handle` is written that way) is the ordinary shape, and the
table's growth is then unbounded in exactly the deployment ADR 0116 was for.
What ADR 0116 objected to was the cost class, not the idea: a walk at every
edge would make a run's fixed cost proportional to what the embedder holds.
This ADR is the walk without that.

What a walk needs, the heap already has. Every object's header names its
layout, and a layout says, word by word, which `Repr` each payload word is —
the static per-word map the collector traces by. A `Repr::Host` word is
therefore findable without tagging anything: the same shapes the collector
walks for `Repr::Ref`, read for `Repr::Host` as well.

## Decision

1. **The roots are the pinned set, and only it.** ADR 0116's argument is
   kept: at a run's edge no frame exists, an unreachable object is never read
   (a new one's payload is zeroed), and a handle that left a run as a value is
   the name itself, not a word. So the only words that can index the table
   after an edge are in objects reachable from what an embedder holds.
2. **With nothing pinned, the table is emptied without a walk**, exactly as
   ADR 0116 decides. That is every run whose embedder keeps no closure, and it
   costs what it did.
3. **With something pinned, the table keeps exactly the names the pinned
   objects reach.** `Space::pinned_host_words` walks from the pinned addresses
   through every `Repr::Ref`, over the same shapes the collector traces —
   matched exhaustively, so a new shape cannot be added to one and forgotten
   in the other — and reports every non-zero `Repr::Host` word.
   `Resources::trim` vacates every slot nothing reported. A walk of its own,
   not a mode of the collector's: the collector's leaf test
   (`Layout::may_hold_refs`) is the wrong one here — a struct of two handles
   holds no reference and is what this has to read — and the collector's walk
   is the hot one. It marks into a set of its own rather than the collector's
   bitmap, so its cost is what is held and not the heap's size.
4. **A kept name keeps its word.** The words naming it are in the embedder's
   objects and are not rewritten, so a trim does not compact: a dropped name
   leaves a vacant slot, and the next name written down takes the lowest
   vacant one before the table grows. Rewriting the words instead would be a
   second pass over the same objects, writing into what an embedder holds,
   and anything kept in the order of those words would have to be put back
   in order after it. Within
   a run nothing changes: one resource is one word (ADR 0013's equality), and
   a run handed a resource the table kept is handed its kept word.
5. **The walk is paced by what it can drop.** Only a name written down since
   the last trim makes the table larger than that trim left it, so an edge
   where the table has not grown does no walk. Otherwise the walk is taken
   once the new names number at least 32 (`TRIM_FLOOR`) and at least a
   sixteenth of what the last walk read (`TRIM_WORDS_PER_NAME`). The walk
   costs what the embedder holds; paced by the names it can drop, it costs a
   constant number of words a name, which is the class interning the name is
   already in. The floor spreads the walk's fixed cost — its locks and its
   vectors — over many runs.

## Consequences

- A resident machine whose embedder holds a closure keeps the names that
  closure reaches and at most `max(32, held words / 16)` more, plus one run's
  own. On the measurement above the table ends at **2** names (the closure's
  and the slack the floor allows) instead of 20,001; holding a closure over a
  vector of 10,000 tickets it ends at 10,525 instead of 30,000.
- What a run costs is unchanged in class and, measured, in size. 20,000 runs
  of the mint-and-redeem entry, median of ten 2,000-run blocks, encoded and
  native, `--profile checked`, an x86-64 Mac, three rounds each:

  | held by the embedder              | ADR 0116 (µs a run) | this ADR (µs a run) |
  | --------------------------------- | ------------------- | ------------------- |
  | nothing                           | 3.9 – 4.1           | 3.8 – 3.9           |
  | a closure over one ticket         | 3.8 – 4.2           | 3.8 – 4.0           |
  | a closure over 10,000 tickets     | 3.9 – 4.2           | 4.0 – 4.1           |

  A first version that rebuilt the id index on every trim and collected the
  reached words into a hash set measured 5.5 µs on the last row: the walk's
  per-word cost, not its pacing, is what decides that row, which is why
  `trim` takes a vector of flags and touches the id index only for the names
  it drops. A version without the floor measured about 0.7 µs a run more on
  the middle row, a walk at every edge (on a loaded machine, where a run
  took 8 µs).
- Nothing changes per instruction, and the native tier needs nothing: its code
  never reads the table, only the words, and the walk reads the heap the
  native tier writes.
- A name kept by a trim and then left unreachable — the embedder dropped one
  of two closures, or a closure's cell now holds a different handle — goes at
  the next trim rather than the next edge. That is slack, not growth: the
  table is no larger than the last trim left it.
- `Vm::resources_held` still says how many names the table holds;
  `lifecycle.rs`'s `a_held_closure_keeps_only_the_names_it_reaches_*` and
  `a_name_is_kept_while_any_held_value_reaches_it_*` read it, the latter
  through a closure in a closure, a closure in a struct, and a cell a closure
  rewrites, and `mem.rs`'s
  `the_resource_words_a_pinned_object_reaches_are_found_and_no_others` holds
  the walk to a struct of a handle in an array, a box and a closure.

## Alternatives considered

- **Piggy-back on the collector**: have a collection record the live
  `Repr::Host` words and prune the table when it runs. A collection runs
  inside a run, where frames hold words too, and a frame's roots are
  published as addresses, not as typed slots, so the collector cannot see a
  frame's `Repr::Host` words; pruning there would drop a name a frame still
  holds. And a run that allocates little never collects, so the table would
  grow between collections exactly as before. What the collector does share
  is the knowledge of shapes, and the walk shares that.
- **Reference-count each name.** A word is untagged and copied freely by
  every instruction; counting its copies would put work into the dispatch
  loop and the native tier's generated code. ADR 0116 rejected it for the
  same reason, and nothing here changes it.
- **Walk at every edge where something is pinned.** Exact at every edge, and
  it is the cost class ADR 0116 declined: a run that does nothing would pay a
  traversal of everything the embedder holds.
- **Compact the table and rewrite the kept words.** A dense table, at the
  price of writing into the embedder's objects at an edge and of re-sorting
  anything they keep in the order of the words. A vacant slot is reused, so
  the table does not grow from not compacting.
