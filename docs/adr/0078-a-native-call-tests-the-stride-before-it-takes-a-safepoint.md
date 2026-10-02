# ADR 0078: A native call tests the stride before it takes a safepoint

- Status: Accepted
- Date: 2026-10-02
- Supersedes:
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  **"Safepoints occur at least: … around allocation or runtime calls which may
  collect"**, as it has been read for a *call*: that every `Inst::Call` made
  from compiled code — through the mediated `call` helper or the direct
  `open` helper — takes ADR 0040's safepoint unconditionally, on the argument
  that "a call may allocate and an allocation may collect" (`cove_native::abi`'s
  `CallFn` and `OpenFn`, and `vm::exec::native`'s `call` and `open`). A call is
  now a **poll**: the helper charges the caller's unpaid work, tests the stride,
  and takes the safepoint when it has been reached. *Allocation* is untouched:
  the allocating helpers still take a safepoint every time. So is the rest of
  that list, and so is "No compiled interval may exceed the backend's stated
  `T`"
- Preserves:
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s table of bounds, in its
  own units and unchanged — the work between two polls is at most `S + T`, as
  [ADR 0060](0060-a-backedge-tests-the-stride-before-it-calls.md) states it for
  a backedge — and
  [ADR 0057](0057-a-native-call-returns-into-the-destination-its-caller-named.md)'s
  return path, which was and remains free of safepoints
- Decides: what a compiled call asks of the runtime's accounting, and when

## Context

ADR 0060 found that the native tier was paying a helper call at every loop
backedge to be told that nothing was due, and made the backedge a two-instruction
compare against a threshold the runtime publishes. It left calls alone, and said
why only implicitly: a call already enters the runtime — `open` has a frame to
push and an account to keep — so the poll was not a separate C-ABI crossing to
remove.

It is still a separate *piece of work*. `open` and `call` charge the unpaid
work and then run `Machine::safepoint` whole, every time:

1. `interp::stopped_here` — the task's cancellation flag and every bounded
   call's flag, with the instruction's span computed in front of it;
2. `Meter::safepoint` — an atomic `fetch_add` on the run's shared fuel total,
   the run's cancellation flag, the fuel limit, and with a deadline beside a
   fuel limit a *second* shared atomic `fetch_add` for
   `DEADLINE_CHECK_INTERVAL`;
3. `Memory::poll` — a relaxed load of the collector's pending flag.

With `cove fmt` compiled whole (ADR 0076, ADR 0077) the native tier makes
**15.7 million** direct calls in one `covefmtBench` run and `examples/cq`'s
`revenue-summary` over 100,000 records makes **89.4 million**. A sampled
profile of each (`/usr/bin/sample`, the method of `scripts/covefmt-profile.sh`)
put the safepoint taken under `open` at:

| program, native tier | `Machine::safepoint` and below | of which under `open` |
| --- | ---: | ---: |
| covefmtBench | 5.7% | 3.7% |
| cq `revenue-summary` | 16.8% | 14.9% |

The stride is 1,024 units of work. covefmtBench charges 2.23 billion over its
15.7 million calls, about 140 a call, and cq 1.33 billion over 89.4 million,
about 15 — so six calls in seven on covefmt, and sixty-seven in sixty-eight on
cq, took the three steps and were told nothing was due. It is the shape ADR 0060 removed from backedges, in the one other place
compiled code reaches a safepoint for no reason but that it happens to be
there.

The encoded tier has never done this. Its `CALL` arm opens a frame and
dispatches the callee; the safepoint is the dispatch loop's own stride test.

### Why "around runtime calls which may collect" is not a reason for a call

The sentence in ADR 0055 exists so that a collection never happens at a point
where a reference is held only in a register. Every collection the runtime can
run is reached through one of two doors:

- **an allocation**, `Machine::allocate`, which collects and retries when the
  heap is full. Every compiled allocation reaches it through a helper — `alloc`,
  `growable`, `run_copy`, `dynamic` — that syncs the frame's program counter
  and takes a safepoint itself. A callee's allocations are the callee's helper
  calls, and they are exactly as safe whether or not the call that reached
  the callee was a safepoint;
- **the collector rendezvous**, `Memory::poll`, step 3 of a safepoint.

`open` does neither unless it takes the safepoint. What it does besides is push a
`Frame` and grow the word stack, and neither of those reads or frees the heap.
So the only thing an unconditional safepoint at a call bought was a rendezvous
and a fuel charge *more often than the stride*, which is promptness ADR 0040
does not ask for and ADR 0060 already declined to buy at a backedge.

## Decision

### A call is a poll

Both call helpers — `open`, the direct path, and `call`, the mediated one —
do what they did up to the safepoint: move `NativeCtx::pending_work` into the
machine's `bulk_work`, and sync the caller's program counter. Then they ask
`Machine::safepoint_due`, which is

```rust
SAFEPOINT_STRIDE.saturating_sub(work() - charged_work) == 0
```

— the remainder `Machine::poll_budget` publishes as `NativeCtx::poll_at`
(ADR 0060), reaching nought. When it is true they take `Machine::safepoint`,
whole and in ADR 0040's order; when it is false they go straight on to
`admit_frame` and `push_frame`. Nothing else in either helper moves, and
`republish` still runs, so the threshold the caller resumes with is the
remainder after the charge.

No emitted code changes. The compare is in Rust because the helper is already
running and already holds the two numbers it compares.

### What the bound is, stated

A compiled function polls at every backedge (ADR 0060) and now, as before, at
every call — what changed is what a poll at a call costs when nothing is due.
The work between two polls in a descent is therefore one level of the call,
and the overspend past any stop condition is at most `S + T`: the row ADR 0040
states for every stop mode, and the row ADR 0060 states for a backedge.

The two new `native_tier.rs` cases assert it on real machine code, over a
recursion with no loop in it — `counts`, whose descent has no backedge at all,
so every poll in it is a call's:

- under `fuel: Some(5_000)`, the run stops with a fuel error, inside the
  compiled descent, having spent at least the limit and at most
  `SAFEPOINT_STRIDE + 64` past it;
- with the run cancelled before it begins, it stops with a cancellation error
  having spent at most `SAFEPOINT_STRIDE + 64`.

A call that did not poll at all would run the whole descent — a million units
of work — before anything noticed, and would answer rather than stop.

### What is not changed, and is worth saying

**The return path.** `close` charges the callee's last unpaid block and is not a
safepoint, which is ADR 0057's "published only on the return path, where no
safepoint intervenes" and ADR 0060's third publisher. It was not a poll before
and is not one now, so the *unwinding* of a deep loop-free recursion still
polls nowhere until its caller next calls or loops. That gap is ADR 0055's
"at bounded intervals inside long straight-line code", it is older than this
ADR, and this ADR neither widens it nor closes it: the longest interval between
two polls is the same before and after, because the unwind is where it was.

**Allocation.** `alloc`, `growable`, `run_copy` and the allocating `dynamic`
observations still take a safepoint every time. Making the first two polls too
was measured in the same session and bought **−0.7%** on covefmtBench's native
wall time (inside its ±1% floor) and **−1.4%** on cq: not enough to change a
sentence ADR 0055 states in as many words. It is a later ADR's if a program
shows it.

**What a safepoint is.** `Machine::safepoint` is unchanged, and so is every
caller that is not a call helper.

## Consequences

Measured on x86-64 macOS, the binary of this ADR's commit against its parent,
interleaved, fifteen runs an arm in one session, median [min..max]:

| row | before | after | delta |
| --- | ---: | ---: | ---: |
| covefmtBench, native, wall | 4,025 [4,005..4,046] ms | 3,859 [3,838..3,884] ms | −4.1% |
| covefmtBench, native, `whole` | 359 [356..370] ms | 338 [334..342] ms | −5.8% |
| cq `revenue-summary` 100k, native, wall | 6,240 [6,194..6,283] ms | 5,428 [5,386..5,520] ms | −13.0% |

The VM executes none of this change. cq on the VM measured +1.6% at this
commit (nine runs an arm, ranges overlapping), inside that program's ±2.9%
rebuild floor and the same layout sensitivity ADR 0060 recorded;
covefmtBench on the VM did not move.

In the sampled profile of cq, the `safepoints` bucket fell from **16.8%** of the
native run to **2.4%**.

- Fuel is charged exactly as before: the same work reaches `bulk_work` at the
  same call, and `fuel_spent` at the end of a run that finishes is the same
  number. What moves is *when* it is handed to the `Meter`: in batches of a
  stride rather than at every call, which is how the encoded tier and a
  compiled backedge already hand it over.
- A stop is observed later than it was — within `S + T`, where it used to be
  within one level of a call. That is the same plain statement ADR 0060 made
  for backedges: compiled code was tighter than its stated bound, and now uses
  the bound it was given. `responsiveness.rs` has no native row yet (ADR
  0055's adoption condition, not this ADR's), so the bound is asserted by the
  `native_tier.rs` cases above.
- With a deadline beside a fuel limit, `DEADLINE_CHECK_INTERVAL` counts
  safepoints, and a compiled call no longer is one unless it is due. The clock
  is therefore read about once per `64 × S` of work, which is ADR 0040's row
  `64 × (S + T)`; before this it was read far more often than that row asked.
- A collection requested by another task waits at most `S + T` of this task's
  work for the rendezvous, as it already did at a backedge.

## Alternatives considered

**Emit the compare in front of `open`, and call a separate safepoint helper
when it is due.** It would save nothing: `open` has to be called anyway, for
the frame. The compare costs the same on either side of the call, and on this
side it needs no code generator change and no second helper.

**Inline `open` and `close` into generated code for the common case.** That is
where the rest of the call path's cost is — after this change, and after the
cheaper-`open` change made beside it, `open` and `close` are still
about a quarter of cq's native run — but the
frame stack and the word stack are Rust `Vec`s that only Rust may resize, and
moving them under emitted code is a change to the runtime's data structures
and to ADR 0057's protocol, not to its accounting. It is not decided here.

**Make allocation a poll too.** Measured, and declined above.

**Leave it.** Issue #365 warned that a direct call which dropped the poll would
be a speedup that was really a missing check. This keeps the check — every
call still compares, and the bound is asserted on machine code — and drops
only the three steps that a compare says are not due.
