# ADR 0051: A string is built as a byte run, not a vector of pieces

- Status: Accepted
- Date: 2026-09-12
- Decides: that the executable IR can allocate, fill and finish packed byte
  runs, and that string construction lowers to those operations instead of
  remaining opaque behind collection and string builtins
- Supersedes nothing. It extends
  [ADR 0019](0019-executable-ir-and-vm.md)'s executable IR with the
  variable-sized scalar storage it did not decide, and contradicts none of
  what that ADR settled
- Preserves: [ADR 0034](0034-one-physical-word-stack.md)'s one linear memory.
  A byte run is an object in its heap region, not a side buffer or a third
  value store

## Context

The executable IR can name a string, read one byte and read an object's
length. It cannot describe how a string is made. Construction is hidden behind
builtins such as `String.sliceBytes` and `String.join`, and a Cove program
that builds text commonly spells the missing operation indirectly:

```text
String.sliceBytes
    -> allocate a String
    -> Vector.push its one-word handle
    -> String.join
    -> walk the pieces and allocate the final String
```

That shape is the hot path of `examples/covefmt`. The instruction profile
before the stack-address changes of PR #347 reported:

| operation | calls | measured cost |
|---|---:|---:|
| `Vector.push` | 5,073,508 | 284 ns/call |
| `String.sliceBytes` | 1,764,011 | 394 ns/call |
| `String.join` | 146,394 | 1,814 ns/call |

Together the string-building operations accounted for 26.1% of the profile's
cost above its per-instruction floor. Removing one intermediate allocation did
not move the wall clock: the program still dispatched the calls, maintained
the vector of handles and copied all of the bytes at the join.

The IR already has the physical ingredients. Strings are heap objects whose
header carries a byte length, `ByteAt` reads their packed payload, and the
heap allocates variable-sized objects. What is absent is an IR vocabulary that
lets lowering say that the result is one byte run assembled from other byte
runs.

### Why `Array<Byte>` is not that vocabulary

A general array is a run of elements whose layout is measured in eight-byte
words. Making `Byte` one such element spends eight bytes for one byte.
Packing only `Array<Byte>` changes more: an element address can point inside
a word, `Repr::Addr` no longer names every assignable element, and generic
`LoadElem`, `StoreElem`, layout width and copying acquire a second unit.

Those may be useful language decisions, but none is required to build a
string. The runtime already has a packed-byte representation for strings.
This ADR exposes that capability to lowering without changing the physical
contract of every array.

### Why a byte loop in IR is not enough

Replacing one native builtin call with one interpreted instruction per byte
increases dispatch, which is already the dominant cost. The useful primitive
is therefore a **bulk** byte operation visible to the optimiser and executed
as a native run copy, not a Cove loop over `ByteAt` and `StoreElem`.

## Decision

### The IR has an internal packed byte-run object

The heap gains an internal byte-run layout. It stores a byte length and packed
bytes in the same linear-memory heap as every other Cove object. Its payload
contains no references, so a partially built run is safe for the collector to
walk.

A byte run is an IR/runtime construction value, not a new public Cove type.
This ADR does not add `ByteBuffer`, change `Array<Byte>`, or give source
programs another mutable collection.

A run under construction is not a `String`. Arbitrary bytes need not be
UTF-8, and a partially filled object must not become observable as a valid
string through a safepoint, debugger, Host boundary or call. Finishing is the
operation that establishes the string invariant.

### The IR can allocate, write, copy and finish byte runs

The instruction vocabulary gains operations with these meanings; their final
Rust names and operand packing are implementation details:

```text
alloc-bytes    dst, length
write-byte     bytes, at, value
copy-bytes     dst, dst_at, src, src_at, length
finish-string  dst, bytes
```

- `alloc-bytes` allocates a fixed-length, zero-initialised construction run.
- `write-byte` writes one checked byte within that run. It exists for
  delimiters and encoded scalar values, not as the preferred way to copy text.
- `copy-bytes` copies a range in bulk. Its source may be a String or another
  byte run; its destination is a byte run under construction. Bounds and
  overlap have defined checks rather than relying on the native copy routine.
- `finish-string` validates UTF-8 and turns the completed run into the
  immutable String result without copying its payload. Failure produces the
  same source-level error the corresponding String operation produces today.

The bytecode verifier distinguishes a construction run from an ordinary
`Repr::Ref` object strongly enough to refuse:

- writing into a String;
- using an unfinished run where a Cove value, call argument, return value,
  captured value or Host value is required;
- finishing or writing a value that is not a byte run;
- copying outside either object.

How that distinction is represented — a dedicated `Repr`, verifier abstract
state, or an internal layout known to the verifier — is left to implementation.
It must not add a runtime tag check to every ordinary reference operation.

### String construction lowers to bulk byte operations

A construction whose final length is known uses one exact allocation. In
particular, `String.join(parts)` lowers conceptually to:

1. read and check the part lengths and separators;
2. allocate the exact result length;
3. copy each part and separator into the run;
4. finish the run as a String.

The lowering and optimiser may fuse a slice used only as input to a
construction:

```text
join(..., source.sliceBytes(from, to), ...)
```

becomes a `copy-bytes` directly from `source[from..to]`. It does not
allocate the sliced String or put its handle in an intermediate vector.

This is the principal performance decision. Merely expressing the current
`sliceBytes -> Vector.push -> join` sequence with different names does not
satisfy it.

A later growable string-builder API may lower to the same primitives, but its
source syntax, ownership rules and growth policy are not decided here. The
first implementation is allowed to cover exact-sized joins and fused slices
only.

### Bulk work remains bounded work

One IR instruction remains one dispatch and one instruction for profiling,
but a byte operation is not one unit of computational work merely because it
has one opcode. Consistent with ADR 0019, allocation, validation and copying
are charged proportionally to the bytes they examine or write.

Large copies and UTF-8 validation must cooperate with the stop bounds decided
by [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md). An implementation may
process a run in bounded native chunks and poll between them; it may not make
cancellation latency proportional to an unbounded input while reporting one
cheap instruction.

## What this costs

**An internal object state.** The verifier and debugger must understand a
reference that is live for GC but is not yet a source-language value.

**Several opcodes.** They enlarge the encoded dispatch loop, whose footprint
has measured costs of its own. `write-byte` and `copy-bytes` are distinct
because one scalar store and one bulk copy have different operands and costs;
variants that are not used by a measured lowering are not added pre-emptively.

**UTF-8 validation.** A run containing arbitrary written bytes is validated
once at finish. A later optimisation may prove that a run assembled solely
from whole valid String ranges and ASCII constants is valid and select a
verified fast finish. It may not silently skip validation for an arbitrary
byte run.

**A length pass for exact construction.** `join` reads the pieces once to
calculate the allocation and once to copy them. That is preferable to repeated
growth and does not create an intermediate String. A growable builder remains
available for constructions whose length cannot be known cheaply.

## Alternatives considered

### Keep String operations as builtins

A builtin is appropriate for an indivisible native operation. It is the wrong
boundary for a construction made of allocation and copies when that boundary
prevents lowering from removing intermediate slices and collections. The
profile measures the whole hidden sequence, not only a slow implementation of
one builtin.

### Lower every byte operation to existing scalar instructions

This makes the work visible and multiplies dispatches by the number of bytes.
It moves a native memory operation into the VM's most expensive layer and is
rejected.

### Make String equal to `Array<Byte>`

This either gives every byte one word or introduces packed generic elements
and sub-word addresses. Both are larger language and memory-model decisions
than string construction needs. Nothing here prevents a later ADR from giving
`Array<Byte>` the same packed backing representation, provided it states how
mutation, element addresses and generic layouts work.

### Expose a public StringBuilder first

A builder may be useful, but public API is not required to remove the
formatter's intermediate work. Deciding the physical and IR primitive first
lets a later API be judged by semantics rather than used to smuggle a runtime
side buffer into the language.

### Construct a String in place without a separate unfinished state

A zero-filled payload is collector-safe but not necessarily a valid String
while it is being filled, and arbitrary byte writes can leave invalid UTF-8.
Calling it a String early makes validity a temporal convention instead of a
checked boundary. The construction state is explicit instead.

## Consequences

- String construction is visible to lowering and optimisation while byte
  copying remains a native bulk operation.
- A slice consumed only by a larger construction need not exist as a heap
  object.
- An array of String handles is no longer the required intermediate
  representation of joined output.
- Packed bytes remain in the one VM heap and do not introduce a side store.
- General arrays retain word-sized element addressing; `Array<Byte>` packing
  remains undecided.
- The implementation is not accepted as a performance improvement merely
  because allocation or instruction counts fall. It must record wall time for
  `examples/covefmt`, the opcode profile, allocated objects and allocated
  words before and after.
- The conformance corpus must cover invalid UTF-8, empty runs, zero-length
  copies, overlapping copies, range errors, cancellation during a large copy,
  GC during construction and a finished String crossing the Host boundary.
