# ADR 0068: A dynamic value is inspected in Cove, not walked in Rust

- Status: Proposed. Phases 0–4 are built (#491–#511). Phase 5's final
  counts and performance report are in
  [docs/measurements/adr-0068.md](../measurements/adr-0068.md), and Phase 5
  is blocked on [#512](https://github.com/myuon/cove/issues/512),
  [#513](https://github.com/myuon/cove/issues/513),
  [#514](https://github.com/myuon/cove/issues/514),
  [#515](https://github.com/myuon/cove/issues/515) and
  [#516](https://github.com/myuon/cove/issues/516)
- Date: 2026-09-23
- Decides: that the remaining boxed fallback is a missing structural-reflection
  capability rather than four irreducible standard-library operations; that a
  type-erased value carries a run-time type identity and may be inspected
  through a read-only semantic view; that equality, ordering, key admission and
  rendering of such a value are ordinary Cove algorithms over that view; that
  physical layout, addresses and GC metadata remain below the boundary; and
  that static layout specialization remains the fast path.
- Proposed for
  [issue #432](https://github.com/myuon/cove/issues/432), after issue #454's
  layout-directed migration found the last four dynamic arms.
- Supersedes, when accepted:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 4,
  which makes `Shape::Boxed` a permanent fallback through
  `Inst::IntrinsicCall`. Its observation remains correct — the concrete
  layout is unknown until the box is opened — but its conclusion does not.
  Opening the box need not hand the whole operation to Rust. It may hand a
  semantic structural view to Cove.
- Refers to, without superseding:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 3,
  which synthesizes ordinary private IR functions where the layout is known;
  [ADR 0040](0040-bounded-work-is-a-language-guarantee.md)'s bounded work,
  depth and cancellation requirements;
  [ADR 0034](0034-one-physical-word-stack.md)'s representation and evaluator
  agreement;
  and [ADR 0045](0045-a-literal-is-there-before-the-program-runs.md)'s
  program-owned immutable metadata.

## Context

### Four intrinsics remain for one missing ability

ADR 0064 moved the statically known population of four operations into
layout-directed functions synthesized by lowering:

- `AnyEquals`;
- `ValueOrder`;
- `ValueAdmitKey`;
- `ValueRenderInto`.

The result is 485 synthesized functions over 305 distinct
`(operation, layout)` pairs in the completion census on #432. Those functions
are ordinary control-flow IR. The printer, verifier, optimizer, VM and native
tier see no operation name and no backend recognizes one.

The four `Intrinsic` variants nevertheless remain. They are reached only
when the operand has `Shape::Boxed`: a `dyn Trait` or a Host schema's
`Any` whose first payload word is a `LayoutId` known only at run time. The
Rust runtime opens that box and recursively walks the value.

That boundary has been described as four fallbacks. It is more accurately one
missing language capability: **Cove cannot inspect a value after its static type
has been erased.**

Keeping the four operations below the boundary would make their names part of
the machine indefinitely. Replacing them with four new IR instructions would
delete `IntrinsicCall` while preserving the same policy boundary under a new
spelling. A method renamed in the standard library would still require an IR
change, which fails ADR 0064's discriminator.

### A type tag is necessary and not sufficient

A boxed value already carries a run-time layout identity. Knowing the identity
alone does not let Cove implement an operation over the value. Cove also needs
a descriptor answering semantic questions:

- is this an `Int`, `String`, struct, enum, sequence or keyed collection;
- which case of an enum is present;
- how many fields, payloads or elements exist;
- what is the semantic child at an index;
- which scalar value does a scalar view hold.

It does **not** need, and must not receive, a word offset, alignment, tag bit,
GC bitmap or raw address. Those are facts about one backend representation.
Making the standard library depend on them would prevent the representation
from being replaced.

### Returning a child is a rooting question

A naïve API of `field(value, index) -> Dynamic` hides a lifetime problem. A
child may be inline in its parent, may be a reference to another heap object,
or may be an immediate scalar. An allocation or collection between taking the
child and reading it must not leave an interior pointer behind, and boxing
every child into a new heap object would make a recursive walk allocate once
per node.

The reflection value therefore cannot be specified as a naked
`(LayoutId, pointer)`. It is a **rooted view**. Its representation is private,
but its invariant is public to the verifier and collector: for as long as a
view is live, every object needed to read the value it denotes remains rooted,
and no raw interior address survives a safepoint.

## Decision

### 1. Type-erased values expose a read-only structural view

The compiler/runtime gains an internal opaque Cove type, called
`DynamicView` in this ADR. The name is not a public language commitment.

Conceptually it denotes:

```text
DynamicView {
  type identity,
  rooted owner,
  location within that owner
}
```

This is a semantic description, not an ABI. An implementation may use a root
plus offset, a handle into a per-run view table, or another representation that
the collector and native tier can verify. It may not expose a raw address to
Cove, and it may not require allocating a heap box for every projected child.

A `Shape::Boxed` value can be opened as one `DynamicView`. A projection of a
field, enum payload, sequence element or keyed entry answers another rooted
view. A view is immutable and cannot escape into a public Cove value in this
phase.

### 2. Reflection describes language structure, not memory layout

The internal core surface provides the following logical capabilities. Exact
function and instruction names are chosen during implementation, but every
entry must map to one primitive structural observation and may not implement a
standard-library operation.

```text
dynamicType(view) -> TypeId
typeKind(type) -> TypeKind

dynamicBool(view) -> Bool
dynamicInt(view) -> Int
dynamicFloat(view) -> Float
dynamicString(view) -> String
dynamicDuration(view) -> Duration

dynamicCase(view) -> Int
dynamicChildCount(view) -> Int
dynamicChild(view, index) -> DynamicView
```

The initial `TypeKind` distinguishes at least:

- unit and scalar values;
- strings;
- structs;
- enums;
- option and result;
- arrays and vectors;
- sets and maps;
- opaque/non-structural values.

A map child is observed in its canonical entry order, with key then value. A set
is observed in its canonical element order. Struct fields and enum payloads are
observed in declaration order. These orders are already part of the existing
walks and must not be silently redefined by the reflection API.

The implementation may split `dynamicChild` into field, payload, element and
entry primitives if that makes verification materially stronger. It may not
expose byte/word offsets or make the standard library branch on a physical
`Layout` variant.

### 3. Type identity is opaque, per program and not serializable

`TypeId` is an opaque identity valid for one compiled `Program`. Equality is
its only required primitive operation.

A numeric value of a `TypeId`:

- is not stable between compilations;
- is not written to trace, replay or persistent data as a language identity;
- is not an address;
- does not become a public hashing or ordering key;
- cannot be fabricated by Cove code.

Program-owned type descriptors are immutable and placed before execution. A
lookup allocates nothing. The descriptor table is walked by the collector only
if its private representation contains managed references; a public program
never owns or mutates it.

### 4. The four dynamic algorithms live in Cove

The standard library implements the dynamic arms as ordinary Cove functions
over `DynamicView`:

```text
std.dynamic.equals
std.dynamic.order
std.dynamic.admitKey
std.dynamic.renderInto
```

The names above describe ownership, not necessarily a public module.

Their observable semantics remain those pinned by ADR 0064's corpora and the
current Rust walks. In particular:

- equality first compares semantic type identity and then structure;
- ordering follows the same canonical type, case, field and collection order;
- key admission refuses exactly the kinds and depths it refuses today;
- rendering uses the same punctuation, field/case names and nesting rule.

The functions are ordinary IR. No backend recognizes their names. The only
operations below the boundary are the structural observations in Decision 2,
allocation/runtime capabilities they already use, and stopping through ADR
0067's `core.refuse`.

When this migration is complete, the Rust implementations of the four boxed
walks and their four `Intrinsic` variants are deleted.

### 5. Static specialization remains and must not route through reflection

Decision 3 of ADR 0064 remains the fast path. If lowering knows the layout, it
calls or inlines the existing synthesized function directly.

```text
known layout   -> synthesized layout function
Shape::Boxed   -> open DynamicView -> Cove reflection walk
```

A structural test rejects a statically known operand routed through
`DynamicView`. Reflection is the semantic fallback for erased values, not a
convenience API for lowering.

A future optimizer or native tier may guard on a run-time `TypeId` and dispatch
to an existing synthesized function. Such specialization is optional and may
not change the Cove fallback's meaning.

### 6. The first surface is private to the standard library

This ADR does not add public reflection syntax, a public `Dynamic` type, or a
stable metadata API. The capability is initially reachable only through
`core.*` from trusted standard-library modules and synthesized support code.

Public reflection raises additional questions this migration does not need to
answer:

- whether a program may enumerate private field names;
- whether attributes and documentation are reflected;
- how type identities cross package and process boundaries;
- whether mutation through reflection exists;
- how reflection interacts with capability security.

After the four fallback migrations, a separate ADR may expose a restricted
public API. No private representation chosen here is promised to that API.

### 7. Opaque kinds remain opaque deliberately

Functions, Host handles, resources, tasks, scopes and other capability-bearing
values do not become structurally readable merely because they are boxed.
Their descriptor answers an opaque kind, and the Cove algorithms reproduce the
existing operation-specific behaviour: equality/order/admission may refuse or
use the already specified identity rule, and rendering may use the existing
opaque description.

Reflection must not turn a capability into bytes, reveal a Host address, or
make two resource handles comparable when the language does not.

### 8. Work, recursion and cycles obey the existing bounds

Every child projection and scalar observation is charged. Collection walks
poll and charge proportional work under ADR 0040. The Cove algorithms preserve
the current maximum nesting behaviour until a separate decision changes it.

The AST oracle and the linear-memory backend must refuse at the same semantic
depth. Issue #480's existing disagreement is resolved before this ADR's
migration is declared complete; the reflection walk may not pin one evaluator's
private constant as the language answer accidentally.

If Cove values can form a cycle in a kind admitted to these walks, the
operation uses the existing cycle rule. This ADR does not introduce pointer
identity as a new equality rule.

### 9. The view is verified as a capability

`DynamicView` is not an integer tuple that ordinary Cove can assemble. The
verifier tracks its representation/class explicitly.

The verifier and runtime enforce:

- only opening a verified boxed value creates a root view;
- only a descriptor-compatible projection creates a child view;
- scalar projection agrees with `TypeKind`;
- an index is bounds checked below the boundary;
- a live view keeps its owner reachable across allocation and safepoints;
- a view cannot be stored in ordinary collections, returned through a public
  function, captured by a task, passed to Host code or serialized;
- no operation writes through a view.

The native tier either lowers these observations directly or calls narrow
runtime helpers. It does not call an operation-level equality/order/render
helper.

## Rejected alternatives

### Keep four permanent `IntrinsicCall` variants

This is the least implementation work and the architecture ADR 0064 currently
describes. It is rejected because the four names are standard-library policy,
the generic intrinsic mechanism remains for them alone, and a method-level
boundary cannot be optimized or replaced by Cove.

### Rename them to four dynamic IR instructions

`DynamicEquals`, `DynamicOrder`, `DynamicAdmitKey` and
`DynamicRender` would delete the enum and preserve its substance. They fail
ADR 0064's renaming test and are rejected.

### Dispatch `TypeId` through an operation-specific function table

A table from `TypeId` to synthesized equality/order/render functions is a
valid optimization and may be added later. It is not the semantic substrate:
the compiler would still own the closed set of operations, and user- or
library-defined structural algorithms could not be written without another
table family.

The reflection walk is the fallback definition. A function table may optimize
that definition without replacing it.

### Put a vtable on every boxed value

A vtable makes dynamic dispatch cheap, but freezes an operation set and an ABI
on every box. It is premature for four cold fallbacks and would couple boxed
representation to equality, ordering, rendering and key policy. Rejected for
this phase.

### Expose raw layout metadata

Raw offsets and pointers make the implementation small and every later
representation change expensive. They also create unrooted interior-pointer and
capability-leak hazards. Rejected.

### Allocate a new box for every child

This avoids a borrowed/rooted view but turns a walk into one allocation per
node, changes collection pressure and makes the fallback path pathologically
expensive. Rejected. A view must retain its root without allocating per child.

## Adoption

### Phase 0 — pin and census

- Record every remaining boxed call site and dynamic count.
- Extend the existing corpora so each semantic `TypeKind`, nested combination,
  opaque kind, limit and refusal is pinned on AST, VM and native.
- Resolve or explicitly decide #480's evaluator depth disagreement.
- Record allocations, allocated words, instructions, dispatches, fuel, helper
  crossings, machine-code bytes and wall time for the current Rust fallback.

### Phase 1 — descriptor and view substrate

- Add opaque `TypeId`, `TypeKind` and `DynamicView` classes/layouts.
- Implement opening, kind inspection, scalar projection and child projection.
- Add exhaustive verifier, GC rooting, liveness, encoding, printer, VM and
  native tests.
- Prove by allocation counters that walking children allocates no box per
  child.
- Add structural tests forbidding public escape and raw-layout exposure.

No `Intrinsic` leaves in this phase.

### Phase 2 — one end-to-end operation

Move `AnyEquals` first. It returns one word, writes no buffer and has the
smallest interaction surface.

- Implement `std.dynamic.equals`.
- Compare it against the pinned Rust walk on every kind and nesting edge.
- Delete the boxed arm's `AnyEquals` producer and variant.
- Report the static-specialized and reflected populations separately.
- Use the result to decide whether any observation primitive is too coarse or
  too expensive before the other three depend on it.

### Phase 3 — order and key admission

- Move `ValueOrder`, preserving canonical collection order.
- Move `ValueAdmitKey`, preserving the refusal and ADR 0067 sentences.
- Keep decision and refusal in the same user-visible frame where the existing
  corpus requires it.
- Resolve depth/cancellation behaviour identically across evaluators.

### Phase 4 — rendering

- Move `ValueRenderInto` using the existing growable byte substrate.
- Preserve exact rendering bytes, including nested values and opaque kinds.
- Measure #478's repeated-ensure behaviour separately; it may justify a
  capacity optimization but does not justify restoring a rendering intrinsic.

### Phase 5 — deletion and possible publication

- Delete the Rust operation-level boxed walkers once their last non-oracle
  caller is gone.
- Delete the four variants and, when every other migration in #432 is complete,
  `Intrinsic`, `IntrinsicCall`, its VM dispatcher, native ABI and reporting
  machinery.
- Publish the final static-versus-reflected counts and performance report.
- Only then decide in a separate ADR whether any reflection surface becomes
  public Cove API.

## Gates

Every phase keeps:

- AST, VM and native observable results identical;
- exact diagnostic message, rule, help and primary blame;
- no raw address or physical offset visible to Cove;
- no child allocation required merely to traverse a value;
- no unrooted interior reference across a safepoint;
- bounded fuel, cancellation and nesting;
- zero statically known layouts routed through reflection;
- no operation-named replacement instruction or runtime helper;
- measurements on fixed-input covefmt, cq and a boxed-value mechanism bench.

A slowdown of the genuinely dynamic path is reported rather than hidden: it is
allowed to pay for type inspection. A regression in code that has a static
layout, an increase in its fallback count, or an allocation per reflected child
is a failed gate.
