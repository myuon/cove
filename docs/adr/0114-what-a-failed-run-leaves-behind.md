# ADR 0114: What a failed run leaves behind, and whose authority a run has

- Status: Accepted
- Superseded in part by
  [ADR 0116](0116-a-machine-s-resource-table-is-emptied-between-runs.md),
  which makes the resource table per-run state rather than the machine's
- Date: 2026-10-24
- Decides: what the runtime promises the run after a failed one — on the same
  machine, the same preparation, the same registry and the same worker — and
  what counts as state that must not survive; that a run's authority is its
  registry's grant set, the same for every frame of it, with nothing that
  carries, delegates or widens a capability; that the runtime does not
  correlate a parked run's answer with its request beyond the operation's
  declared type; and which out-of-memory events are recoverable. Recorded for
  [issue #681](https://github.com/myuon/cove/issues/681), priority 3 and
  "権限境界の検査"
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0008](0008-concurrent-task-execution.md) (tasks share the run's budget
  and registry),
  [ADR 0011](0011-garbage-collection.md)'s amendment and
  [ADR 0088](0088-an-embedder-sizes-the-heap-and-is-told-of-a-cancellation.md)
  (the heap's capacity is the machine's, not a limit),
  [ADR 0015](0015-capability-analysis-for-higher-order-calls.md) (the
  runtime's grant check is the only thing that decides),
  [ADR 0031](0031-a-host-handle-is-not-a-vm-handle.md) (a handle is a name the
  host minted),
  [ADR 0080](0080-a-host-call-may-answer-pending.md),
  [ADR 0082](0082-a-parked-run-keeps-its-deadline.md),
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md) and
  [ADR 0091](0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md) (how a
  run is parked, yielded and stopped), and
  [ADR 0112](0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md)
  (what a finding is)

## Context

Issue #681's priority 3 asks that every stage of starting and running an
isolate be made to fail, and that the clean-up and the next run be checked:
no double free, no use after free, no reference left behind, nothing of the
previous run's state or authority visible to the next. Its permission items
ask for denial tests through closures, messages and handles, and for the
expected specification of capability inheritance and delegation to be
written down.

None of this had been written down as one promise. Pieces were: ADR 0080
gives `OwnedVm`, `ParkedVm` and `Step`, and says an answered `Step` hands the
machine back "for its next run"; ADR 0088 makes a resident `OwnedVm`'s heap
"one arena across" many requests; the edge example's `Isolates::Pooled`
reuses machines on the strength of "Cove has no global state". What "for its
next run" promises — and what "state" means when a machine's heap, counters
and resource table outlive a run by design — was left to whoever reads the
code. So was the permission model: the code has one gate,
`HostRegistry::dispatch`, but nothing said whether a closure, a task or a
handle could carry authority past it.

`docs/ISOLATION_TESTING.md` surveys what exists. Two facts shape the
decisions below. First, there is no in-process messaging between isolates:
a `Value` is `!Send`, a `Shared` cell is refused at the boundary, a closure
names one machine's heap, and the only things that cross between runs are
host-owned (a host's own store, a resource handle). Second, the machine is the
unit an embedder reuses, and it is built to be reused: what it keeps across
runs is either immutable, the embedder's, or measured per run.

## Decision

### 1. A run after a failure cannot tell the failure happened

After a run on an `OwnedVm` fails at any point — refused before its first
instruction, trapped, out of heap, stopped by its deadline, cancellation,
call depth, host-call limit or task limit, parked and then cancelled,
dropped, or resumed with a failure or an answer of the wrong type — the
machine handed back runs the next run exactly as a fresh machine built from
the same `PreparedProgram` with the same heap capacity runs it. "Exactly" is
observable and is what the tests compare: the answer, every byte reaching the
console, every call reaching a host, and the instructions, allocated words and
host calls the run is charged.

A preparation, a registry and a process are never damaged by a run's failure,
and a program the front end refuses does not damage the process's caches.

### 2. What is state, and what is not

The promise is about **per-run state**, and per-run state is: the frames and
stack, the tasks and their threads, the cells a run holds, the host request a
parked run carries, the budget's accounting and its cancellation flag, the
yield request, and every word of the heap the run did not explicitly hand
out. None of it may survive the run.

These survive by design and are not leaks:

- **The heap arena and its counters.** The arena is the machine's (ADR 0088);
  what a finished run allocated is garbage, reclaimed by the next collection,
  and is never readable — a new object's payload is zeroed. `instructions`,
  `allocated_words`, `collections`, `tiers` and `yields_declined` are
  cumulative over the machine's life; an embedder reads differences.
- **What the embedder kept.** A value a run handed out keeps what it needs: a
  closure pins its environment in its machine's heap for as long as the value
  lives, and may be handed back to *that* machine in a later run. It is refused
  by any other machine.
- **What a host keeps.** A host's own state — the edge example's `kv` store,
  a fake's tree of files — is the host's, outlives runs on purpose, and is per
  registry: two registries do not share it unless the embedder shares the
  host.
- **What is immutable.** The preparation's encoding and native code, the
  registry's grants, the standard library checked once per process, and the
  per-thread interned names.

### 3. A run's authority is its registry's grants, for every frame of it

Every host call a run makes, from any frame — its entry, a closure however it
was reached, a standard-library higher-order function's callback, a callback
a host runs, a spawned task, a host operation bound to a name — is checked
against the grants of the registry the **running machine** was built over,
at the moment of the call. So:

- **Nothing inherits more than the run has, and nothing has less.** A callee
  has its caller's authority because both have the run's. A task has its
  parent's for the same reason (ADR 0008).
- **Nothing carries a capability.** A closure made by a run that may write
  files grants nothing when it is called by a run that may not; it is refused
  at the write if it reaches one on its own machine, and refused as another
  machine's closure if it reaches another. A resource handle is a name (ADR
  0031): handed to a run whose registry does not grant its capability, it is
  refused at its first operation.
- **Nothing delegates or attenuates.** There is no API by which a run, a
  closure or a task obtains a narrower or a wider grant set than the run's.
- **Nothing widens the set.** Grants are fixed when the registry is built and
  the registry is shared by `Arc`; no host operation and no language construct
  changes them. `GrantSource` says where they came from and changes only the
  advice a refusal gives: a sealed binary refuses exactly what a configured
  run refuses.
- **The static analysis is advice.** `required_capabilities` and
  capability-openness (ADR 0015) let a host refuse a program early, as the edge
  server does at deploy. They decide nothing at run time.

A refusal is a `RuntimeError` with `outcome == HostBoundary` and
`denied_capability` naming the capability, raised before the host is reached:
a denied operation has no effect.

### 4. A late answer is held to its type; correlating it is the embedder's

A parked run's only handle is its `ParkedVm`, and every way of ending one —
`resume`, `cancel`, drop — consumes it, so an answer cannot be delivered to a
run that has ended. A machine reused after a cancelled park parks again with a
new `ParkedVm`, and an answer handed to it is held to the operation's declared
result type and no further: the runtime cannot tell a stale answer from the
right one when they have the same type, and does not try. An embedder that
holds answers that may arrive late correlates them itself, as
`examples/edge`'s parking lot does by id. A `Cancellation` is the run's: one
raised after its run has ended reaches no later run of the machine.

### 5. Two kinds of out-of-memory

- **The heap's capacity is recoverable.** An allocation that does not fit the
  machine's heap, after a collection, fails with "this run has no memory left"
  as an ordinary `RuntimeError`. The run ends; §1 holds for the machine.
- **The process's memory is not.** An allocation the runtime makes outside the
  heap — the Rust side of a host call, a value crossing the boundary, the
  front end — that the process cannot satisfy aborts the process, as Rust's
  allocator does. There is no recovery, and no test of one. A guest that can
  cause one by its own choice of sizes is a finding (ADR 0112 §3), not an
  outcome. So is a stack overflow, which a run with enough compiled-to-encoded
  crossings and no `max_call_depth` can cause on a thread without the runtime's
  stack.

## Consequences

- `crates/cove-runtime/tests/lifecycle.rs`, `authority.rs` and
  `owned_lifetimes.rs`, `examples/edge/host/tests/isolation.rs` and
  `examples/rules/host/tests/session_reuse.rs` hold the runtime to §1 to §5 on
  both tiers and, for authority, on the interpreter too. The randomised storm
  is fixed-seed and runs in `cargo t`; a long variant is `#[ignore]`d.
- The first finding is a departure from §1: `invoke` and `run_entry` — the
  ways in with no budget of their own — run under the meter the last
  `invoke_within` left on the machine, so they inherit its cancellation, its
  passed deadline and its spent host-call count, on both backends. The cases
  that reproduce it are `#[ignore]`d and gated on `COVE_KNOWN_FINDINGS`, so
  that neither `cargo t` nor `cargo ratchet` is red until it is fixed.
- The resource table a machine keeps is never trimmed, so a resident machine
  that is handed a new handle on every run grows by one entry a run. §2
  counts that as the machine's, like the arena; it is an embedder's memory
  cost, not a leak of authority.
- An embedder that passes a handle between registries makes it name whatever
  the other registry's host has under that id. §3 makes that the embedder's
  responsibility, as ADR 0031 makes a handle's meaning the host's.

## Alternatives considered

- **Tag every resource handle with the registry that minted it** and refuse
  it elsewhere. That would make §3's handle rule hold even against an
  embedder's mistake, at the cost of a registry identity in every handle and a
  check on every resource call. No embedder in the repository passes handles
  between registries, so by "earn complexity through use" it waits for one.
- **Correlate answers with requests in the runtime**, with an id in every
  `ParkedVm` and `resume` taking it back. The only embedder that holds late
  answers already correlates them, and the `ParkedVm` is itself a linear
  capability for the call; an id would be a second one.
- **Promise only that the next run *succeeds*** rather than that it is
  indistinguishable. That is what the existing tests checked, and it would not
  have caught a carried-over budget, a buffered line, or a heap read through.
