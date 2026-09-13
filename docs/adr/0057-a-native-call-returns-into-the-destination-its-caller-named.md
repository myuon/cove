# ADR 0057: A native call returns into the destination its caller named

- Status: Accepted
- Date: 2026-09-13
- Decides: how a compiled function hands its answer back, replacing the owned
  vector and the two-stage copy the bootstrap used
- Supersedes: [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  return path only — "the callee reports a return slot" and the caller copies
  it out — and nothing else. Its slot ABI, its function-at-a-time compilation
  and its safepoint contract stand
- Extends: [ADR 0056](0056-the-first-code-generator-is-the-one-that-was-cheaper-everywhere.md)'s
  template compiler, which is the generator this changes

## Context

The spike of #363 measured the same optimized IR through the VM, Cranelift and
a hand-written template compiler:

| | vm | cranelift | template |
|---|---:|---:|---:|
| no Cove calls | 4.57 ms | 1.87 ms | 1.93 ms |
| 6.4 nested Cove calls per invocation | 16.99 ms | 15.70 ms | 16.02 ms |

Native is 2.4x the VM without Cove calls and **1.08x** with them. The
generators differ by a few per cent; the call does not.

The bootstrap call path is ten steps, and the last four are the ones this ADR
is about:

```text
1  caller values are canonical in its slot frame
2  generated code enters the Rust call helper
3  the helper synchronises accounting and the safepoint
4  open_frame grows and zeroes a callee frame
5  arguments are copied from caller slots to packed callee slots
6  the runtime selects and enters the callee's tier
7  the callee reports a return slot
8  enter copies the result into a new Vec<u64>
9  the helper copies that vector to the caller's destination
10 the frame is popped and pointers are republished
```

Steps 8 and 9 are an owned allocation and a second copy for a value whose
destination was known before the call was made. Every `Inst::Call` carries it:
the lowering settled `dst` when it settled everything else.

A stack-top return was considered and is not selected. Cove's IR is
slot-based, so a value left on a stack top still has to be moved into the
caller's settled destination — the copy would move, not disappear.

## Decision

**A native call is given the destination to write, and writes it.**

The logical entry ABI gains a hidden destination, as *stable word indices*
rather than raw pointers:

```rust
unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    callee_base: u64,
    return_base: u64,
    return_slot: u32,
) -> Outcome
```

The Rust spelling may differ. These properties are normative:

- `return_base + return_slot` names a caller-owned destination run.
- Indices survive a reallocation of the Cove stack's `Vec`. This is the same
  reason `base` is an index and not a pointer, and it is not negotiable: the
  stack `resize`s, and a destination pointer held across `push_frame` is a
  dangling one.
- The return width stays static in `Function::returns`.
- A successful return writes the destination **before the callee frame is
  removed**.
- An internal Cove call constructs no `Vec<u64>`.
- A zero-width return writes nothing.
- A raised or stopped call publishes no result. "Semantically" is the whole of
  it: a partially written destination that nothing may read is permitted, a
  destination that a later reader can mistake for an answer is not.
- A reference is a one-word heap address; returning one copies the word and
  never the object.
- A native/VM boundary may materialise slot frames. Native-to-native may take
  a lighter path.
- Writing straight into the destination must keep GC validity at every
  safepoint. **Initially the result is published only on the return path,
  where no safepoint intervenes.**
- The shape must stay compatible with later scalar register returns, direct
  native calls, and register promotion.

### Two phases, measured apart

Phase 1 removes the owned vector; Phase 2 gives the callee the destination.
They land separately because they are two different claims about where the
time goes, and one number covering both would let a change that did nothing
hide behind a change that did.

## What this costs

**A wider entry signature**, and two more things every code generator must get
right. The template compiler is the only generator this touches today, which
is a reason to do it now rather than after a second one exists.

**A destination that outlives its writer's frame.** The callee writes into the
caller's frame while its own is still live. That is sound because the indices
are stable and the caller cannot run concurrently, but it is a wider aliasing
contract than "the callee touches only its own frame", and it is the thing a
reader of this code needs to be told.

**A publication rule with a temporal condition.** "Only on the return path,
where no safepoint intervenes" is a restriction that a later optimisation will
want to relax, and relaxing it needs the GC argument made properly rather than
noticed to be missing.

## Alternatives considered

### Keep the owned vector

It is one allocation and one copy per call on a path measured at 1.08x the VM.
Whether *this* is the dominant term is what Phase 1 measures; that it is
unnecessary is not in question.

### Return on a stack top

Slot-based IR means the value still has to reach a settled destination, so the
copy moves rather than disappears, and a second discipline arrives for
nothing.

### Return scalars in registers

The better answer for scalars, and it does not conflict — it is the reason
this ADR requires the shape to stay compatible with it. It is not first
because it does not cover wide returns and would leave both paths to keep
correct while the simpler one is unmeasured.

### Do this only for native-to-native

Half the calls in a mixed run cross a tier, and the destination is as known
there as anywhere. A rule with an exception is two rules.

## Consequences

- An internal Cove call allocates nothing to return a value.
- The result is written once, into the place the lowering already chose.
- `Inst::Call`'s `dst` becomes part of the native ABI rather than something
  the runtime applies afterwards.
- The callee writes into its caller's frame, under stable indices.
- Publication is confined to the no-safepoint return path until a GC argument
  extends it.
- Phases 1 and 2 are measured separately, on #363's harness and its real
  inputs, against the VM every time.
- Whether this is *the* term in the 1.08x is not assumed. #365 decomposes the
  call path; this ADR removes the part of it that is indefensible whatever the
  decomposition says.
