# ADR 0116: A machine's resource table is emptied between runs

- Status: Accepted
- Superseded in part by
  [ADR 0119](0119-a-held-closure-keeps-only-the-resource-names-it-reaches.md),
  which replaces decision 2 (the table kept whole while anything is pinned)
  with a trim to the names the pinned objects reach
- Date: 2026-10-24
- Decides: that the table of host resource names a linear-memory machine
  keeps (`Machine::resources`, the table a `Repr::Host` word indexes) is
  per-run state, emptied at both edges of every entry — where it begins and
  where it leaves, whether it answered or failed — unless an object of the
  machine's heap is pinned for a value an embedder holds, in which case it is
  kept whole and emptied at the first edge after the last such value is
  dropped; and that a handle is found in the table by a hash lookup on its id
  rather than a scan. Recorded for
  [issue #685](https://github.com/myuon/cove/issues/685)
- Supersedes: [ADR 0114](0114-what-a-failed-run-leaves-behind.md)'s
  consequence that the resource table is never trimmed and counts as the
  machine's, like the arena
- Refers to, without superseding:
  [ADR 0013](0013-host-resource-handles.md) (a handle is a name the host
  minted, equal to another when they name the same resource, and survives its
  resource's `close`) and
  [ADR 0031](0031-a-host-handle-is-not-a-vm-handle.md) (why the table is not
  the heap)

## Context

ADR 0114 §2 lists what is per-run and what survives a run by design. Its
Consequences recorded, from reading the code, that the resource table is
neither: every handle a run is handed is interned into a table that lives as
long as the machine and that nothing removes from, and each resource
operation scans it linearly. ADR 0114 counted that as the machine's, like the
arena — an embedder's memory cost, not a leak of authority.

Measured, it is more than a memory cost. A resident machine reused for every
request — `examples/edge`'s `Isolates::Pooled`, the shape ADR 0088 sized the
heap for — handed one new handle a request grows by one entry a request, and
the scan compares two strings an entry. On `crates/cove-runtime/tests/lifecycle.rs`'s
fixture, a run that mints a ticket and redeems it took 21 µs a run averaged
over its first 2,000 runs on one machine and 420 µs a run by its
20,000th, on the encoded tier, `--profile checked`, an x86-64 Mac: the cost of
a resource operation was linear in the machine's age. The arena does not do
that; a collection reclaims what a finished run left.

What decides whether the table *may* be emptied is which words naming a
resource can outlive a run. A frame cannot: it is gone with its run. An
unreachable heap object cannot be read: a new object's payload is zeroed. A
handle that left the run as a `Value` is not a word at all — the boundary
turned it into the `ResourceHandle` itself, and handed back it is interned
again. That leaves exactly one: a word inside an object the embedder kept, and
by ADR 0114 §2 that is a closure's environment, which the boundary pins in the
heap for as long as the embedder holds the value. Emptying the table under
such a closure was tried and is wrong: the closure's next call answered "this
value names a host resource this run was never handed", and with the table
refilled by a later run it would have named whatever that run was handed
first.

## Decision

1. **The table is per-run state.** `Vm` empties it where an entry begins,
   before its arguments are converted, and where it leaves, after its answer
   has been converted out — on every way in, budgeted or not, blocking or
   parkable, and whether the run answered or failed. A run parked or yielded
   in between has not left and keeps its table.
2. **Unless the heap is pinned for an embedder.** While any object is pinned
   for a value an embedder holds, the table is kept whole at both edges.
   Which of its names the kept objects reach is not known without walking
   them, and walking them at every edge would put a heap traversal into an
   invocation's fixed cost for a case no embedder in the repository has; the
   first edge after the last such value is dropped empties it.
3. **A lookup is by id.** The table keeps, beside its names, a map from a
   resource's id to the entries that carry it, so interning a handle is a hash
   lookup and a comparison with the few entries that share its id — one,
   unless two hosts or two kinds issue the same number.
4. **What a word means within a run does not change.** Interning still gives
   one resource one word for the length of a run (ADR 0013's equality), a name
   still outlives its resource's `close` within the run, and zero is still no
   resource.

## Consequences

- A resident machine's table holds one run's names, and a resource operation
  costs the same on a machine's 20,000th run as on its first: 7.9 to 8.6 µs a
  run across all 20,000 on the measurement above, against 21 to 420 µs before.
- An entry pays two uncontended locks at each edge (the pinned set and, if it
  is empty, the table) and nothing per instruction.
- A machine whose embedder keeps one of its closures for its whole life keeps
  the old growth for that life. That is the residue of decision 2, stated
  rather than hidden; tracing the pinned objects for their `Repr::Host` words
  is the refinement if an embedder ever needs both.
- `Vm::resources_held` and `OwnedVm::resources_held` (`#[doc(hidden)]`) say
  how many names the table holds, which is what
  `lifecycle.rs`'s `a_reused_machine_keeps_no_resource_from_its_earlier_runs_*`
  and `a_kept_handle_still_names_its_resource_*` read.
- The tree-walking interpreter has no such table: its values carry the
  `ResourceHandle` itself.

## Alternatives considered

- **Empty the table unconditionally at a run's end.** Simplest, and wrong: a
  closure the embedder kept with a handle in its environment then names
  nothing, or the wrong resource. The test above is the case.
- **Keep the table and only index it.** Lookup stops being linear, but the
  table still grows by a name a request for the machine's life — ADR 0114's
  per-run list is the better home for it, and the condition in decision 2 is
  one lock.
- **Trace the pinned objects for the words they hold** and keep only those
  names. Exact, but it is a heap walk on every edge where something is
  pinned, and no embedder here keeps a handle-capturing closure across runs.
  "Earn complexity through use" says it waits for one.
- **Refcount the names from the heap.** A word is untagged and copied freely
  by every instruction; counting its copies would put work into the dispatch
  loop and the native tier's generated code for a table that the condition
  above already bounds.
