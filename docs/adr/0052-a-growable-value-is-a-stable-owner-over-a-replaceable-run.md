# ADR 0052: A growable value is a stable owner over a replaceable run

- Status: Accepted
- Date: 2026-09-12
- Decides: `FixedRun<E>` and `Buffer<E>` as the packed, nominally neutral
  storage foundation over which the standard library can implement String,
  Array, Vector and later ordinary collections
- Supersedes: [ADR 0051](0051-a-string-is-built-as-a-byte-run.md)'s
  String-specific fixed byte-run vocabulary and its prohibition on passing
  unfinished construction to a Cove call. Its packed-byte, bulk-copy, UTF-8,
  fuel and cancellation requirements survive in the generic foundation
- Preserves: [ADR 0001](0001-mvp-language-design.md)'s immutable fixed-length
  Array, mutable growable Vector and O(1) unique `Vector.freeze()`
- Extends: [ADR 0034](0034-one-physical-word-stack.md)'s one heap with no new
  value store
- Superseded in part by
  [ADR 0053](0053-a-gate-names-what-can-be-measured.md): its implementation
  gate, which named a rewrite of `examples/covefmt` that this ADR's own
  append-only API forbids, because that formatter reads its output back and
  unwrites it. Nothing else here is disturbed

## Context

ADR 0051 gave the IR a fixed packed byte run:

```text
alloc-bytes -> write/copy -> finish-string
```

That is enough when the final byte length is known before construction. It is
not enough for the formatter it was measured against. All three hot joins in
`examples/covefmt` are filled by data-dependent loops, and the largest is a
`var out` parameter passed through recursive calls. The length is known only
after the writes have happened.

Preallocating a large fixed run does not answer this. A capacity is a bound,
not a length. If it is exposed as an Array length, the unwritten part becomes
observable as values that were never appended. If exceeding it is an error,
capacity becomes program semantics rather than a performance hint. If the
object itself moves when it grows, every alias and `var` address to it goes
stale.

The VM already has the right answer for elements. A `Vector<T>` is a stable
two-word owner:

```text
Vector header: [logical length, store reference]
store header:  [Elements<T>, capacity]
store payload: capacity × width(T) words
```

A push onto a full store allocates a larger store, copies the live prefix and
replaces the reference in the stable owner. `Vector.freeze()` consumes a
locally unique owner, relabels its store downward to the live length, and
returns that store as an immutable Array without copying it.

Packed bytes need the same ownership and growth discipline, not a second
allocator and not a byte-sized imitation of every Vector operation. The
difference between them is the storage unit and the reference map:

- a byte run packs eight bytes per word and contains no references;
- an element run stores values at the element layout's word stride and is
  traced by that layout.

## The endpoint: library collections over VM runs

The profile motivates doing this now; String is not the architectural fact.
The VM must not permanently know nominal `String`, `Array` or `Vector`
operations. It knows two smaller representation foundations:

```text
FixedRun<E>                         Buffer<E>
  immutable                          shared mutable identity
  logical length                     logical length
  packed payload                     capacity
                                     replaceable packed store
```

`FixedRun<E>` is an immutable, exactly-sized contiguous run. `Buffer<E>` is
a stable identity over a growable contiguous run and uniquely finishes to a
`FixedRun<E>` without copying the live prefix.

The target standard library can define, schematically:

```cove
export opaque struct Array<T> {
    run: FixedRun<T>
}

export opaque struct Vector<T> {
    buffer: Buffer<T>
}

export opaque struct String {
    bytes: FixedRun<Byte>
}
```

These are schematic because `FixedRun` and `Buffer` are privileged intrinsic
types: ordinary code cannot forge their layouts. A one-field Array or String
wrapper may be representation-transparent, so each remains one reference
word and pays no wrapper allocation. Vector's Buffer reference preserves its
observable alias identity when the ordinary value wrapper is copied.

Migration is complete only when replacing one of these standard-library
implementations requires no new VM object shape or builtin dispatch arm.
Lowering may target collection protocols and foundation intrinsics; it must
not recognise a collection by comparing its nominal name.

## Packing is the one ordinary collection representation

`Packed` means that elements occupy one contiguous payload at the physical
stride chosen by their settled layout. It does not mean every type occupies
one byte:

- `Byte` has an eight-bit stride, so eight bytes occupy one VM word;
- word scalars and references have a one-word stride;
- an inline struct repeats its flattened word layout with no per-element box;
- an object-valued element is one reference because that is the element's own
  representation, not because the collection added a box.

The first element-layout descriptions are therefore:

```text
PackedByte | Words(LayoutId)
```

There is no per-element boxed fallback. If a type has no valid packed element
layout, a collection of it is refused until that layout exists. The Rust
implementation may specialise byte and word loops; the shared abstraction
does not require a dynamic branch per element.

A packed sub-word value is read and written through run operations. It does
not manufacture a `Repr::Addr` into the middle of a word. This ADR commits
only Byte to a sub-word layout; bit-packing Bool or another scalar requires
its own addressability and mutation decision.

Array, Vector and String use one run. Set and Map also use packed runs of
members or inline key/value entries; a hash table may use several packed runs
for control bytes, keys and values. A separately named rope, linked list or
persistent tree may choose its own representation, but it is not a hidden
alternate representation of an ordinary Array, Vector or String.

## Decision
### One growable-run discipline

A growable value consists of:

1. a stable owner object containing its logical length and a reference to its
   current store;
2. a replaceable store object whose header length is its capacity;
3. a live prefix `[0, length)` and unobservable spare capacity
   `[length, capacity)`.

The runtime implements allocation, capacity checks, growth, live-prefix copy,
vacated-region clearing and finishing once over a storage description:

```text
element layout = PackedByte | Words(LayoutId)
```

This is one algorithm and one set of invariants below builtin dispatch. It
need not be one Rust type if that makes a hot path generic or indirect; byte
and word entry points may be specialised around the shared contract.

Growth uses the existing Vector policy initially: when full, allocate twice
the capacity from a small floor, copy the live prefix and replace the owner's
store reference. A caller-supplied initial capacity replaces the floor for the
first allocation. Capacity is a performance hint: exceeding it grows rather
than changes the program's result.

Allocation and arithmetic reject a capacity or growth that exceeds the run's
representable length or the run's memory budget before mutating the owner.

### Capacity is not an Array length

An Array remains immutable and every element below its length remains
initialised and observable. There is no `Array.withCapacity` whose apparent
length includes uninitialised elements.

Array construction uses the growable element owner when its final length is
not known, then finishes to an Array. Existing `Vector<T>` is the
source-language owner for this path and gains an initial-capacity constructor;
the final spelling is part of the standard API rather than the IR, with this
meaning:

```cove
var values = Vector.withCapacity<Int>(estimated)
values.push(1)
values.push(2)
let array = values.freeze()
```

`length()` answers two throughout, not `estimated`. Existing
`Vector.of`, `Array.toVector`, mutation, alias identity, `freeze` and
`toArray` semantics are unchanged.

The growable-run implementation replaces the private growth machinery beneath
Vector rather than adding another collection beside it. The uniqueness proof
and finish transition belong to Buffer, not to Vector by nominal name, so a
standard-library wrapper can forward them.

### A byte builder is the typed owner of a packed run

String construction gains a mutable owner whose store is
`storage unit = PackedBytes`. It is a source-language value rather than a raw
`Shape::Bytes` object, because the formatter must pass it through ordinary
Cove calls.

Its minimum operations are:

```text
withCapacity(capacity)
length()
append(String)
appendSlice(String, from, to)
appendByte(Int)
finish() -> Result<String, Error>
```

The standard library chooses the public builder name; that name does not
appear in IR or VM dispatch. Its representation is an opaque wrapper over
`Buffer<Byte>`, not an Array and not a String under construction.

- `append` copies a valid String into the live suffix in bulk.
- `appendSlice` checks the same bounds and UTF-8 boundaries as
  `String.sliceBytes`, then copies directly from the source String. It does
  not allocate the sliced String.
- `appendByte` accepts one byte value. Because arbitrary byte sequences may
  not be UTF-8, `finish` remains fallible.
- `finish` consumes a locally unique builder, validates the live prefix,
  relabels its store downward from capacity to length and returns it as a
  String without copying the live bytes.

A builder copy aliases the same stable owner, as a Vector copy does. Mutation
through either alias is visible through both. Finishing requires the same
conservative local uniqueness proof as `Vector.freeze()`; it does not count a
`var` argument against itself, and it does count every route by which another
holder could survive the call. A later explicit copying conversion may be
added if a non-unique builder needs a String; this ADR does not require one.

### The raw run still does not cross a call

`Shape::Bytes` remains an internal store and cannot be a call argument,
return, capture or Host value. The value crossing a Cove call is the typed
builder owner, whose layout contains a traceable reference to that store.

This preserves ADR 0051's reason for the refusal: no arbitrary unfinished byte
object is mistaken for a source-language value. It also permits the operation
the formatter needs:

```cove
fn emit(node: Tree, var out: StringBuilder) {
    out.appendSlice(source, node.from, node.to)?
    emitChildren(node, var out)
}
```

The raw store never leaves the owner. Host crossing and task safety follow the
owner type's declared rules rather than treating `Shape::Bytes` as an
`Any`.

### Finishing reuses the store

A store header's length is capacity because the allocator and collector must
be able to walk the entire physical object. The logical length lives in the
stable owner. Finishing consumes that owner and calls the existing downward
relabel operation with the logical length:

- an element store is relabelled from growable Elements to immutable Elements
  and becomes an Array;
- a byte store is relabelled from Bytes to Str and becomes a String;
- the unused tail becomes allocator-visible spare space under the same rule
  `Vector.freeze()` already uses;
- relabelling never grows an object and never copies its live prefix.

For element storage, every unused or vacated word that the store's layout could
trace is zero. Byte storage contains no references, but its unused tail is
zeroed as well so String word equality and deterministic rendering cannot
observe stale padding after finish.

### The IR exposes bulk owner operations

ADR 0051's fixed-run instructions remain useful for exact constructions and
tests. Growable construction adds operations over the owner rather than
exposing its two payload words to lowering:

```text
alloc-builder
append-byte
append-bytes
finish-builder
```

The concrete instruction family may share encoded forms between packed bytes
and layout-sized elements when the storage description makes the operation
unambiguous. It must not turn one appended byte or one element into a
`call-builtin` merely to reuse the source API.

A bulk append is one dispatch, not a loop of interpreted stores. Growing may
allocate and copy; an append that fits writes directly into the current store.
The optimiser may replace:

```text
sliceBytes(source, from, to) -> append
```

with one checked append from that source range.

The common abstraction belongs below builtin dispatch. Standard-library
String construction, Vector and Array construction call the same foundation
intrinsics rather than each implementing capacity arithmetic, allocation and
live-prefix copying in nominal builtins.

### Bulk work remains proportionally charged

Growth, append, finish validation and live-prefix copying are charged
proportionally to the bytes or words examined. They cooperate with ADR 0024's
stop bounds in bounded chunks.

The current `copy-bytes` implementation and existing String builtins do not
yet meet this requirement. Implementing growable runs includes changing the
safepoint threshold from equality at an instruction multiple to elapsed work
since the previous poll, so adding bulk work cannot step over and miss a
safepoint. Fuel, cancellation and collector polling move together.

## What this costs

**One indirection for a growable owner.** The stable header is why growth does
not invalidate aliases. Vector already pays it. A finished Array or String
does not: it is the consumed store itself.

**Capacity may temporarily retain memory.** A builder or Vector that grows and
then becomes short holds its high-water capacity until finish or collection.
The initial doubling policy matches existing Vector behaviour. Shrinking
during mutation is not introduced here.

**A public construction value.** The byte builder is language surface because
recursive Cove code must name it. Its API is deliberately append-only; it does
not gain indexing, insertion, removal, sorting or general collection
algorithms merely because Vector has them.

**Uniqueness at finish.** An aliased builder cannot become an immutable String
in place while another alias remains mutable. The existing Vector proof is
reused rather than adding reference counts or copy-on-write.

**Layout-specialised packed access.** PackedByte access differs from word-run
access. Verified metadata or specialised opcodes choose it once; no ordinary
reference operation gains a tag check. `Repr::Addr` remains a word address and
no general sub-word place is introduced.

**A privileged substrate.** Collections become replaceable standard-library
values, but FixedRun, Buffer and element layouts remain intrinsic. Native
contiguous allocation and precise GC tracing cannot be implemented safely by
ordinary Cove code over no memory primitive.

## Alternatives considered

### A fixed Array with an initial capacity

Rejected as a description of the value. If capacity is length, uninitialised
elements become observable. If capacity is a hard bound, a tuning estimate
changes semantics. If length is kept separately, the value is the stable owner
and backing run decided here, not an Array.

### A fixed byte run sized generously by the caller

It avoids growth only when the estimate is right, wastes the full estimate
when it is high, and turns an underestimate into either an error or a second
ad-hoc growth mechanism. Capacity remains useful as the first allocation, not
as a limit.

### A linked list or chunked builder

It avoids copying on growth and makes every append and final read indirect.
The formatter ultimately needs one contiguous String, so it pays a flattening
copy at finish. Contiguous doubling matches Vector, gives amortised constant
append and permits no-copy finish.

### Reuse `Vector<Int>` or word-sized `Array<Byte>`

It spends one eight-byte word per byte and cannot bulk-copy directly into the
String representation. Packing generic Array elements would require sub-word
places and is not smuggled into a builder decision.

### Optimise `Vector<String>.join` without a builder

A general Vector can be observed between pushes, aliased, modified by a callee
and joined more than once. Replacing it with a byte builder requires
whole-program proof across recursive calls. The explicit append-only owner
states the intended construction and makes the fast path local.

### Keep String, Array and Vector as VM nominal shapes

This preserves working code and prevents their replacement in the standard
library: every implementation change needs another VM shape or name-dispatch
arm. Existing shapes are migration evidence, not the final ownership of
collection semantics.

### Give bytes and elements independent growth implementations

That duplicates the capacity arithmetic, overflow rules, allocator
interaction, tail clearing and finish transition. Vector has already exercised
the general ownership shape; bytes differ only in storage unit and reference
map.

## Consequences

- All ordinary collection payloads are contiguous packed runs at the element
  layout's stride; there is no collection-imposed per-element box.
- The VM intrinsically knows FixedRun, Buffer and element layouts rather than
  nominal String, Array and Vector operations.
- Array and String may be transparent standard-library wrappers over FixedRun;
  Vector is an opaque wrapper over Buffer.
- String construction and mutable sequence construction use one stable-owner
  and replaceable-store architecture.
- An initial capacity avoids known early reallocations without becoming a
  semantic bound.
- `Vector<T>` remains the growable constructor for Array values and can be
  pre-sized.
- A byte builder can cross recursive Cove calls as a typed owner while its raw
  unfinished store cannot.
- Unique finish produces String or Array without copying the live prefix.
- String append operations copy source ranges directly and do not materialise
  intermediate slices.
- Array elements remain word-addressed and String bytes remain packed; shared
  growth machinery does not conflate their element representations.
- Migration additionally requires Array, Vector and String behaviour to be
  implemented in standard-library source over foundation intrinsics, with no
  VM dispatch by those nominal names on the migrated path.
- The implementation gate is `examples/covefmt` rewritten to the builder,
  with identical output and all formatter ratchets passing. It reports wall
  time, instruction count, allocations and allocated words against the
  pre-builder main.
- Tests cover zero capacity, underestimated capacity, repeated growth,
  overflow, empty finish, invalid UTF-8, unaligned slices, GC before and after
  growth, aliases observing mutation, uniqueness refusal, a builder passed
  through recursive `var` calls, cancellation during a large append, and
  O(1) finishing for both bytes and elements.
