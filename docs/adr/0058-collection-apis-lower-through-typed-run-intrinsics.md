# ADR 0058: Collection APIs lower through typed run intrinsics, not builtin dispatch

- Status: Accepted. Superseded in part by
  [ADR 0059](0059-a-keyed-collection-is-searched-by-order-not-hashed.md),
  which replaces the Phase 4 `value-hash` intrinsic with `value-order`
- Date: 2026-09-15
- Decides: what belongs in executable IR beneath `String`, `Array`,
  `Vector`, builders, `Map` and `Set`; how the standard library reaches
  representation-dependent operations; and the path by which
  `Inst::CallBuiltin` leaves collection hot paths
- Supersedes:
  [ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md)'s
  **Primitive** test, which kept a whole public method in the runtime when
  only its smallest representation-dependent operation needed to be there;
  and
  [ADR 0043](0043-a-method-moves-if-it-is-total-and-takes-no-closure.md)'s
  **"It must be total"** condition, whose reason — the caller's span
  disappearing into `std/` — this ADR removes at its source by carrying blame
- Elaborates:
  [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  common packed-run substrate and
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  shared semantic optimization
- Changes no source-language API

## Context

Cove currently uses the word *builtin* for two different facts.

The schema says that `Vector<T>` has a method named `set`. That is a
source-language API fact: the checker needs its receiver, arguments, result
and mutability. Executable IR may then contain `CallBuiltin Vector.set`.
That is an execution decision: the VM builds operands, dispatches by the
receiver and operation names, checks values dynamically, runs Rust, builds an
answer in temporary storage and copies it into the destination.

The first fact does not require the second.

The distinction became measurable while bringing the native tier up on
`covefmt`. Small collection operations were individually recognized again
inside both native code generators: `String.byteLength`, `Vector.push`,
`Vector.set` and `Vector.freeze`. That removes encoded dispatch, but makes
the code generators know nominal standard-library APIs and repeats the same
semantic decomposition in Cranelift and the template compiler.

It also prevents the optimizer from seeing across the operation. A
`sliceBytes` followed by an append is opaque at `CallBuiltin`; it cannot
become one source-range copy. Two appends cannot share one capacity check.
Construction followed by freeze cannot lose the growable owner. Bounds known
by a caller cannot discharge checks hidden inside Rust.

The coverage work found a second problem. Native coverage is not monotone in
time when compilation is all-or-nothing by function. Compiling a caller while
one of its callees remains encoded can add enough native-to-VM crossings to
make the program slower. A census ranked only by builtin call count also
picked the wrong work: a frequently called operation may account for little
time and may be one of many blockers in its function.

The response cannot be to add one native `Method` arm per builtin until the
table is empty. Cove needs a boundary at which collection semantics are
visible to common optimization and the runtime is called only for work which
must remain there.

## Decision

### A builtin name is an API classification, not an execution mechanism

`cove-schema` continues to declare source-visible builtin types, methods and
associated functions. A declaration is implemented by exactly one of:

1. **standard library** — ordinary typed Cove source;
2. **core intrinsic** — a typed, compiler-known operation over Cove's
   representation or runtime;
3. **capability** — a Host or resource operation outside the process.

This refines ADR 0042. Reading or writing representation is a reason for the
*smallest representation-dependent operation* to be intrinsic. It is not a
reason for the complete public method and its control flow to remain a
runtime-dispatched builtin.

`Inst::CallBuiltin` remains during migration, but it is not a target
architecture. No new collection API is implemented by adding another
name-dispatched runtime arm unless a later ADR supersedes this decision.

### Collections lower to fixed and growable typed runs

ADR 0052's representation is the common model.

A fixed run has a storage description and a logical length:

```text
FixedRun(storage, length)

storage = PackedBytes | Words(LayoutId)
```

A growable run has a stable owner and a replaceable fixed store:

```text
GrowableRun {
    length,
    store: FixedRun(storage, capacity)
}
```

The source types are nominal wrappers over these representations:

| Source value | Foundation |
| --- | --- |
| `String` | valid UTF-8 `FixedRun(PackedBytes)` |
| `Array<T>` | `FixedRun(Words(layout_of(T)))` |
| string builder | `GrowableRun(PackedBytes)` |
| `Vector<T>` | `GrowableRun(Words(layout_of(T)))` |

A `Map` or `Set` may choose its own table layout, but its contiguous
storage, allocation and copying use the same word-run foundation. Elements
remain packed at their layout stride. A collection does not impose a box per
element.

`PackedBytes` and `Words(LayoutId)` are statically distinguished. Ordinary
access does not inspect a tag at run time, and a byte offset does not become a
general sub-word `Repr::Addr`.

### Executable IR exposes six semantic run operations

The names below specify semantic families. Encoding may split an operation
where byte and word addressing require different machine instructions, and
the VM may fuse an operation for dispatch. Those representation choices do
not change the shared meaning.

#### Allocate a fixed run

```text
run-alloc dst, storage, capacity
```

This allocates physical capacity. Allocation failure, GC and heap growth stay
in the runtime. Exact source constructions may use it directly; a growable
owner uses it for its store.

#### Load and store one unit

```text
run-load  dst, run, index, storage
run-store run, index, src, storage
```

For `PackedBytes`, one unit is a byte. For `Words(layout)`, one unit is the
whole layout-sized element. Bounds are in logical units, not physical words.

A backend may retain specialised byte and element opcodes. The important
property is that they implement this typed operation rather than a nominal
`String.byteAt` or `Vector.set`.

#### Copy a range

```text
run-copy dst, dstOffset, src, srcOffset, count, storage
```

The operation has memmove semantics. It copies bytes for `PackedBytes` and
whole elements for `Words(layout)`. Source and destination may overlap.
Work is charged proportionally and a long copy polls in bounded chunks under
ADR 0040.

This is the common operation beneath string slicing and appending,
`Vector.toArray`, `Array.toVector`, vector growth, removal and bulk
construction. It is not expressed as a Cove loop merely to avoid adding an
intrinsic: doing so would change one bulk operation into per-unit dispatch
and safepoints.

#### Ensure growable capacity

```text
growable-ensure owner, additional, storage
```

If `length + additional <= capacity`, it does nothing. Otherwise it
allocates a larger store, copies the live prefix and replaces the owner's
store reference. Arithmetic is checked before mutation. The fast path is
constant work; allocation and bulk copy are runtime slow paths.

#### Commit initialized units

```text
growable-commit owner, count, storage
```

This advances the logical length after the caller has initialized the
corresponding suffix. Keeping ensure, writes and commit visible as separate
semantic operations permits the optimizer to combine capacity checks and
copies without exposing uninitialized units as collection elements.

Verification requires every path which commits units to initialize them
first. A backend may fuse ensure, copy and commit after that fact is proved.

#### Finish a growable run

```text
run-finish dst, owner, targetLayout, validation
validation = None | Utf8
```

Finish consumes a locally unique owner, relabels its store down from capacity
to logical length, publishes unused tail space to the allocator and returns
the same store address. `Vector.freeze` uses `None`; a byte builder uses
`Utf8`.

Validation may be eliminated when the optimizer proves that every byte came
from valid String ranges at UTF-8 boundaries. Otherwise UTF-8 validation is
a proportionally charged runtime operation.

### The standard library may call typed core intrinsics

ADR 0042 says the standard library is ordinary Cove and has no privileged
language semantics. That remains true: its control flow, types, calls,
debugging and optimization are ordinary Cove.

The core modules it is compiled with may declare typed intrinsics which are
not importable by user programs. This is the same boundary by which ordinary
language runtimes implement allocation or atomics: the library has no secret
syntax, while the compiler recognizes the declarations it owns.

The standard library implements the algorithm around those calls. For
example, `Vector.set` performs its range decision and constructs
`Option<T>` in Cove; the store is a typed run store. A string builder's
`appendSlice` checks the source range, ensures capacity, copies one packed
range and commits it.

The intended division is:

| Standard-library Cove | Core intrinsic/runtime |
| --- | --- |
| loops, branches and arithmetic | allocation and GC |
| Option/Result construction | bounded bulk copy |
| collection range policies | grow slow path |
| probing and table policy | layout-directed value hash/equality |
| append orchestration | UTF-8 validation and Unicode tables |
| public method composition | scheduler, synchronization and Host boundary |

### Runtime calls are statically identified and typed

A core operation which remains in Rust is not dispatched by receiver and
method strings. Lowering resolves it to an intrinsic identifier with a fixed
operand and result shape.

Conceptually:

```text
intrinsic-call dst, IntrinsicId, args, blame
```

Each intrinsic has compiler-visible effects:

```text
may_allocate
may_collect
may_raise
may_block
bulk_work
reads_memory
writes_memory
```

These facts decide whether generated code must publish roots, synchronize the
program counter, take a safepoint and reload stack or heap pointers. A
non-allocating field bound check does not pay the allocation protocol. A
grow operation does.

The VM may use Rust dispatch on the numeric identifier. Native code binds a
direct helper address or a compact helper table entry. Neither reconstructs a
builtin name, allocates an operand vector, or copies a variable result through
an untyped temporary solely to cross the boundary. Results are written
directly to the destination named by the slot ABI.

### Fallibility preserves the source call site's blame

ADR 0043 keeps fallible methods in the runtime because moving their body to
the standard library currently moves the diagnostic into `std/`. That is a
diagnostic limitation, not a permanent execution boundary.

A fallible intrinsic carries a `BlameId` naming the source operation whose
lowering introduced it. Inlining preserves that id. If the library body is
not inlined, the runtime error still reports the user's call site as the
primary span and may include the library location as secondary context.

Consequently, totality is no longer a condition for moving a method to the
standard library once blame preservation exists. Until it exists, the
corresponding fallible method may remain on `CallBuiltin`; migration must not
regress diagnostics to claim architectural completion.

### Standard-library bodies are optimized before backend legalization

The pipeline is:

```text
checked program plus instantiated standard library
  -> typed executable IR
  -> inline library wrappers and hot library bodies
  -> common semantic optimization
       |- eliminate redundant bounds and capacity checks
       |- combine adjacent ensures and commits
       |- fuse source-range slice followed by append
       |- remove temporary run allocations and copies
       |- remove growable owner from construct-then-finish
  -> VM legalization and dispatch fusion
  -> native legalization and machine code
```

Inlining is not merely a call-cost policy here. It exposes the protocol on
which semantic optimization operates. Small public wrappers over run
intrinsics are mandatory inline candidates; larger algorithms retain the
target-specific budgets ADR 0055 specifies.

A native backend lowers run operations and intrinsic calls. It does not match
`("Vector", "set")`, `("String", "byteLength")` or another public API
name. The existing native `Method` arms are migration scaffolding and are
deleted as their source methods move.

## Migration of the current surface

The migration proceeds by dependency, not by dynamic call count.

### Phase 1: establish the boundary

1. Add typed intrinsic identifiers and effect metadata.
2. Add blame preservation for fallible intrinsics.
3. Add the storage descriptor and shared `run-copy` semantics.
4. Report emitted IR, mediated intrinsics, encoded VM instructions,
   native-to-VM crossings and native-to-runtime calls separately.

### Phase 2: converge the existing byte and buffer instructions

The current `ByteAt`, `AllocBytes`, `WriteByte`, `CopyBytes`,
`FinishString`, `AllocBuffer`, `AppendByte`, `AppendBytes` and
`FinishBuffer` are mapped onto the typed run families.

Compatibility opcodes may remain in encoded bytecode during transition, but
common optimization operates on the run meaning. New optimization is not
written twice for the old nominal families.

### Phase 3: move sequential collections

The public algorithms of `Array`, `Vector`, `String` and the string
builder move into Cove source over run intrinsics.

Initial targets are operations already decomposed independently by native
lowering: lengths, indexed access, push, set, freeze, slicing and conversion.
Then `pop`, `remove`, `contains`, `indexOf` and join follow.

Unicode case conversion, parsing and formatting may remain direct intrinsics;
their public wrappers still live in the standard library where there is
policy or composition to express.

### Phase 4: move keyed collections

`Map` and `Set` retain layout-directed `value-hash` and `value-equal`
intrinsics. Probing, growth policy and public collection behavior move to
Cove source over word runs unless measurement shows a performance-class
regression which cannot be removed by optimization.

### Phase 5: remove nominal execution dispatch

Once no reachable standard-library collection method lowers to
`CallBuiltin`:

- delete the corresponding runtime name-dispatch arms;
- delete native `Method::Push`, `Method::Set`,
  `Method::Freeze` and their successors;
- make a new collection `CallBuiltin` a verification failure;
- remove `Inst::CallBuiltin` when the remaining scalar constructors and
  assertions have migrated or have explicit intrinsic identities.

Host and resource calls are unaffected. They are capabilities rather than
builtins under ADR 0042 and retain their own instructions.

## Gates

A migration is accepted only when all of these remain true:

- the tree-walking evaluator and linear-memory VM agree on the full corpus;
- the encoded VM and every native code generator agree on result, error and
  stop outcome;
- GC tests cover references live across allocation, grow and finish;
- long run operations retain ADR 0040's cancellation and fuel bound;
- aliases observe growable mutation, and unique finish refuses surviving
  aliases;
- fallible migrated methods report the original source call site;
- no phase claims performance from coverage alone.

Performance reports include:

- wall time with interleaved builds and a VM-only binary-layout control;
- time attributed by function and operation, not call count alone;
- allocations and allocated words;
- emitted, mediated and encoded instruction counts;
- VM-to-native, native-to-VM, direct-native and runtime-helper crossings;
- counts of each semantic fusion which fired.

The adoption target is not “zero helpers”. It is that hot control flow and
collection protocol are visible to the optimizer, while expensive or
representation-sensitive leaf work crosses one typed boundary without name
dispatch or temporary value reconstruction.

## Alternatives considered

### Keep lowering one builtin at a time in each native backend

This improves individual programs but repeats public API semantics in every
backend, gives common optimization nothing to see and has no natural end.
The work which motivated this ADR showed that higher native coverage may even
increase crossings and wall time.

### Put every collection operation in executable IR

An `Inst::StringReplace` or `Inst::VectorSet` makes dispatch cheaper but
turns the IR into a mirror of the public library. Every new method becomes a
VM opcode, a verifier arm, a bytecode encoding and a native lowering. Fusion
still has to understand nominal combinations. This is the shape being moved
away from.

### Implement allocation and raw memory entirely in Cove

Ordinary Cove cannot safely participate in precise GC, publish roots, relabel
heap objects or preserve bounded cancellation during a bulk copy without a
trusted boundary. Exposing unrestricted raw memory would enlarge the language
and its safety proof far beyond what replacing builtins requires.

### Keep fallible methods primitive forever

That preserves today's diagnostic but lets a missing call stack decide the
optimizer boundary indefinitely. Carrying blame explicitly fixes the reason
at its source and permits the method's ordinary control flow to become
visible.

### Lower standard-library calls without inlining them

This removes duplicate Rust implementations but leaves call frames between
the optimizer and the collection protocol. Larger algorithms may remain
calls; thin wrappers whose purpose is to expose run operations are part of
the lowering contract and are inlined before target legalization.

## Consequences

- Public collection APIs stop defining the VM and native instruction sets.
- `String`, `Array`, `Vector` and builders share allocation, growth,
  copying and finishing without sharing nominal runtime dispatch.
- Common optimization can remove real allocations, traversals, checks and
  copies before the VM and JIT diverge.
- Runtime work remains in Rust where it needs GC, bulk native code, Unicode,
  hashing, scheduling or Host access, but is reached through typed static
  identities.
- The standard library becomes replaceable Cove source over a small trusted
  core rather than a second runtime.
- The IR gains a run protocol and intrinsic effects, but loses pressure to
  gain one instruction per public method.
- The native code generators become smaller as nominal `Method` lowerings
  disappear.
- ADR 0043's totality restriction becomes temporary migration state rather
  than a permanent classification.
- `CallBuiltin` is deprecated by architecture before it is removed from the
  tree, allowing correctness and performance to move together.
