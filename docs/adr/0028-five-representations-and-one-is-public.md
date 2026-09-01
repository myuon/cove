# ADR 0028: Five representations, and one of them is public

- Status: Superseded by [ADR 0034](0034-one-physical-word-stack.md). Its
  measurements and alternatives remain historical evidence; its five-part
  runtime representation taxonomy is not binding.
- Date: 2026-08-30
- Supersedes: nothing. It **decides what
  [ADR 0027](0027-a-place-and-a-capture-name-a-slot.md) left open** under
  "What is not decided here", and continues
  [ADR 0019](0019-executable-ir-and-vm.md)'s "Slots, not names" rather than
  replacing any part of it. "What this supersedes, and what it does not",
  below, goes through ADR 0019, ADR 0027, ADR 0013 and ADR 0011 one at a time
  and says why each survives intact
- Decides: [issue #197](https://github.com/myuon/cove/issues/197)'s ADR phase,
  and [issue #196](https://github.com/myuon/cove/issues/196), which asked
  whether `Value`'s variants should be sealed
- Records the relation to
  [issue #109](https://github.com/myuon/cove/issues/109), which #197's
  acceptance criteria ask for: **#109 stays closed and is not
  reopened.** Its measurement is what gives this ADR its shape rather than
  something this ADR overturns; "What #109 settled, and why it is the reason
  for 8 and not 1" says how
- Implementation status: **nothing is built.** Not the slot, not the heap
  object, not the dynamic value, not the sealing, not `ValueView`. This ADR is
  prose and type sketches, and the sketches are illustrations of decisions
  rather than signatures anybody has compiled. #197's prototype phase is what
  builds a narrow vertical slice, and its measurement gate is what decides
  whether the slice becomes a migration

## Context

### One Rust type is doing five jobs

`cove_runtime::Value` is a twenty-two-variant enum in
`crates/cove-runtime/src/value.rs`. It is, simultaneously and without any
seam between the roles:

1. **what a Cove value is** — the tree-walking oracle evaluates into it, so it
   is the executable statement of the language's value semantics;
2. **a VM frame and operand slot** — `Vm::stack` is a `Vec<Value>`, and a
   frame's value window is a range of it;
3. **the heap object graph** — a `Struct` is an `Rc<StructValue>`, a `Vector`
   an `Rc<VectorStorage>`, an `Enum` a `Box<EnumValue>`; there is no VM-owned
   layout under any of them;
4. **the dynamic representation** — every variant carries its own tag, so
   every value is self-describing whether or not anything needs it to be;
5. **the public embedding type** — `HostApi::call` takes `Vec<Value>` and
   answers a `Value`, and every variant of it is `pub`.

Nothing about that arrangement was decided. It is what a tree-walking
interpreter needs — one self-describing type, because a tree walk knows
nothing statically — and it survived into the VM because the VM was grown
beside the interpreter rather than against a representation of its own.

ADR 0027 already took a bite out of the second job: a slot the checker settled
as `Int` or `Bool` lives in `Vm::scalars`, a `Vec<i64>`, and a `Value` is
never built for it. What that ADR decided is the *identity* rule — "nothing
about a slot's role decides which stack it lives in; only the checker's answer
about its type does" — and it explicitly left the rest open:

> **A single physical frame.** ... this ADR unifies the *identity* of a slot
> without unifying its storage: there are still three stacks, three bases per
> frame, and three counts on a call. Design B of that issue — one compact
> word-wide slot stack with a GC bitmap — is not built, not measured, and not
> refused here.

This ADR is that open item, plus the four other representations #197 names,
plus the public boundary, decided together — because they cannot honestly be
decided apart. A slot cannot stop being a `Value` while `Value` is also what a
host matches on, unless somebody first says which of the two `Value` is.

### What #109 settled, and why it is the reason for 8 and not 1

Issue #109 was scoped on the hypothesis that the *width and shape* of `Value`
is where a VM's remaining cost is. It closed under its own acceptance
criterion 1 — close without the broad implementation — and the two findings
that closed it are the two this ADR is built on.

**Width is worth about one percent per eight bytes.** Measured directly, not
guessed: a padding variant that nothing constructs was added to `Value` to
widen it, and the suite run at 24, 32 and 40 bytes, with the 40 column
reproducing the real pre-`c8450e7` `Value` to within a percent as its control.
`docs/VM_ARCHITECTURE.md`'s "The value representation, audited" is the table.
The one large win was structural rather than dimensional — `HostFn` inlined
two fat pointers, so the variant naming `console.println` set the width of
every `Int` both backends moved, and boxing it took 40 to 24. PR #187 added a
`const _` assertion so that number cannot be undone quietly.

**The eight bytes below 24 are not available at all, and the reason is
semantic.** `docs/LANGUAGE_REFERENCE.md` says `Int` is a 64-bit signed
integer and `docs/LANGUAGE_CARD.md` says integer overflow is a broken
invariant rather than a wrapped result. An 8-byte *universal* value has no
room for a full 64-bit integer and a tag. NaN boxing gives about 51 bits of
payload; an aligned tagged pointer gives 61 or 62. Both would box integers
outside their range, which would make arithmetic near `i64::MAX` allocate —
a change to what `Int` *costs*, in a language whose Card promises what `Int`
*is*. #109 asked the question about `Float` and the binding constraint turned
out to be `Int`. **The floor for a universal tagged value in this language is
16 bytes**, and it is a floor no benchmark can move.

Those two together are exactly why #197 proposes **8-byte typed slots** and
not **an 8-byte universal tagged value**, and this ADR takes the distinction
as its premise rather than re-deriving it. Eight bytes is available to a slot
whose type is *already known*, because such a slot needs no tag. It is not
available to a value that must describe itself. The whole design below is the
consequence of that one sentence.

### The record on the public half, and what it argues

Four changes to what a value *is* reached embedders. #195 went back over three
of them and asked whether a host written against its new readers would have
survived each. The answer was yes three times, and the honest version is
narrower:

- **#104**, `Box<StructValue>` → `Rc<StructValue>` — survived **by accident**.
  Both deref to `StructValue`, so a body that only reads fields needed no
  change.
- **the 24-byte reduction** (#109) — survived because **nothing outside the
  runtime read a `HostFn`**, not because reading one was safe.
- **#192**, `EnumValue::payload` → `Payload` — survived **by shim**. `Payload`
  carries a hand-written `Deref<Target = [Value]>` and `From<Vec<Value>>`
  written for exactly that purpose.

So the past record is not evidence that `pub` variants are survivable. It is
evidence that each change was individually rescued, twice by luck and once by
someone paying for a compatibility shim. #196 states it that way and asks the
question this ADR answers.

### What #195 promised, and what that forecloses

#195 gave a host a way to read a value without naming its representation:
`declared_type`, `field`, `fields`, `case`, `payload`, `items`, `elements`,
`entries`, `as_bool`, `as_int`, `as_float`, `as_duration_nanos`, `as_str`,
`is_unit`, `range`, `resource`, `host_op`, `arity`. A reader borrows, because
the readers that already existed — `ok_payload`, `StructValue::get` — do.

Its own report states the limit, and it is the thing this ADR has to resolve:

> every part a reader answers with must be *stored* as the thing it answers
> with. Fields can move behind a different pointer, a shared shape table, or
> an inline arity the way `Payload` did — invisible. They cannot become
> *computed* values (unpacked from a tagged word, decoded lazily, held under a
> lock) without these signatures changing. `payload() -> Option<&[Value]>` is
> specifically a promise that a payload stays contiguous.

**A value unpacked from an 8-byte slot is a computed value.** So #197's
direction and #195's signatures are in direct tension, and "The tension #195
left, and how it is resolved" below is where this ADR pays that bill rather
than noting it.

`Vector` is the precedent worth studying, because it is where the borrow-based
design already failed to reach a type. `VectorStorage` is
`{ elements: RefCell<Vec<Value>>, frozen: RefCell<bool> }`, because an alias
may write the elements. Nothing can hand out a plain `&[Value]` of them, so
`Value::items()` answers `None` for a `Vector` — and there is no
`Value::vector` constructor either. #196 records this as "the one place the
borrow-based reader design cannot reach" and leaves it. This ADR does not
leave it.

### The collector, and the one failure it cannot survive

`crates/cove-runtime/src/heap.rs` is where root discovery is decided today.
`Roots::walk(&self, visit: &mut dyn FnMut(&Value))` is the trait; the VM's
implementor walks the whole live value stack and the open task scopes.
`Scan::count` counts the references the collector can *see* for each shared
allocation and compares them against `Rc::strong_count`; a shortfall means a
reference is held somewhere the walk cannot read — a Rust local — and that
makes the allocation a root. `Marker::visit` then marks and bills what is
reachable, with a per-container `walked` set so a shared `Rc` is not counted
twice.

That arrangement has one failure mode and it is not merely "an object was
marked twice". It is **a root storage location yielded twice during the
reference-counting walk**, because a reference counted twice conceals exactly
the shortfall the rule depends on. That is distinct from two real graph edges
pointing at one shared object — both edges must be counted — and from marking,
where one object must be expanded only once. #192 kept `Vm::arg_vectors` out
of the root set for that reason; ADR 0027 kept a place out of it on the same
ground.

A VM-owned heap handle is not automatically an `Rc`. If it is an index or
offset, moving it from the VM stack into a Rust local creates no
`Rc::strong_count` shortfall, so the current collector cannot infer that
temporary root. Decision 8 therefore separates the invariant that survives
from the mechanism that may not.

## Decision

### 0. There are five representations, they are named, and only one is public

| # | name | what it is | visibility |
| --- | --- | --- | --- |
| 1 | *Cove value semantics* | what the Language Card and Reference say a value is | not a Rust type at all |
| 2 | `Slot` | a VM frame or operand slot: **8 bytes, untagged** | private to `cove-runtime` |
| 3 | `HeapObject` | a VM-owned object: **one-word handle plus a header** | private to `cove-runtime` |
| 4 | `Dynamic` | a genuinely type-erased value: **16 bytes, `(witness, payload)`** | private to `cove-runtime` |
| 5 | `Value` | what a host is handed, **materialized at the boundary** | `pub`, and the only one |

The rule that makes #197's thesis true by construction rather than by
discipline is the visibility column, and it is one sentence:

> **No public signature in this workspace mentions a `Slot`, a `HeapObject`,
> a `Dynamic`, a layout id, a witness, a handle, a frame base, or a tag.**

That is checkable — it is a `grep` over `pub fn` — and it is the difference
between "the representations are allowed to differ" and "the representations
cannot fail to differ".

Layer 1 is listed because leaving it out is how the other four get confused
for each other. It is not implemented by anything; it is what all four of the
others are answerable to, and ADR 0012's ranking — specification above oracle
above backend — is what says so.

### 1. A slot is eight bytes, untagged, and its kind comes from metadata

A frame is one contiguous region of 8-byte slots. A slot holds:

| static slot kind | the eight bytes hold |
| --- | --- |
| `Int` | the full signed 64-bit value |
| `Float` | the full IEEE-754 64-bit bit pattern, every pattern including every NaN |
| `Bool` | canonical 0 or 1 |
| `Unit` | no slot where the layout can omit it; otherwise a canonical zero word |
| a heap-backed value — struct, string, array, vector, map, set, closure, enum | a VM heap handle |
| a host resource | a stable host handle |
| a genuinely erased value | a two-slot `Dynamic`; see decision 3 |

**The bits are not self-describing and never become so.** What a slot means
comes from `cove_ir::Function`'s per-slot layout and from the instruction that
touches it, both of which are the checker's answers written down at lowering
time. This is the direct continuation of ADR 0019's "Slots, not names" — the
index is in the instruction, so using it costs nothing, and there is nothing
to confirm because the layout *is* the thing the index refers to — and of
ADR 0027's "only the checker's answer about its type" decides a slot's
representation.

`Float` moves onto this scheme, which it is not on today: `SlotKind` is
`Value | Scalar(Scalar) | Place` and a `Float` is a `Value`. A full IEEE-754
double is exactly eight bytes with nothing left over, which is the whole
reason a *typed* slot can hold one and a *tagged* value cannot.

**One logical frame, one slot numbering, one base.** Parameters, locals,
temporaries and captures occupy one index space from one frame base. A
*slot* is always one eight-byte word; a *value location* is the first slot
plus a layout whose width is one or more consecutive slots. Most values have
width one. A `Dynamic` has width two, and an aggregate may also have width
greater than one if its lowered layout chooses an inline representation.
Instructions name the value location's first slot and the function's layout
metadata supplies its width and the kind of every constituent slot.

This distinction is required rather than optional. Without it, decision 3's
two-word `Dynamic` would violate the same one-slot-one-index rule that an
inline enum was said to violate. Slot identity and value identity are not the
same thing: **every word has one slot index, while one logical value may occupy
several adjacent slot indices.** This is what #162's title asks for and what
ADR 0027 explicitly did not decide.

**The physical arrangement was left to measurement here and is now decided by
[ADR 0034](0034-one-physical-word-stack.md).**
At the time of this decision, one word-wide array was the obvious realization
but was not mandated, because #179 says why a cross-build absolute on this
workspace is not evidence. ADR 0034 now mandates that realization for the
production VM and disallows a physically split production frame; three independently numbered stacks and three independent frame
bases are not one logical frame. What *is* decided is the invariant any
physical arrangement must satisfy: **a slot the layout calls scalar must never
be reachable by a walk that treats it as a reference, and a slot must not be
widened past eight bytes to make some other part convenient.**

**A place is unchanged.** ADR 0027 decided a place is a root and a path where
the root names a slot; under one numbering there is one kind of root rather
than two, which is a simplification of that decision and not a change to it. A
place is still not a GC root.

### 2. A heap object is a handle and a header

A slot referring to heap-backed data holds one word. What it names carries, in
VM-owned metadata or an object header:

- a **layout / type id**;
- the object's **size**;
- its **reference map** — which of its words are handles, so a collector
  scans those and not the scalars beside them;
- its **payload layout**, including a variable-length tail where it has one;
- the heap or arena's **movement guarantee**. Under ADR 0011 and the Language
  Card, collection is **non-moving**, so the guarantee is that a handle stays
  valid and an object stays put. This is an allocator/arena invariant, not a
  mandatory word in every object header. A future mixed heap may encode it per
  layout or object if it needs to. A moving collector would owe what
  `docs/VM_ARCHITECTURE.md`'s "Collection is non-moving" already writes down,
  and this ADR does not propose one.

**An object reference does not carry its type when the slot kind or the header
already provides it.** That is #197's rule and it is taken as written.

**Enum and `Option` layout is selected per lowered type and is not fixed by
this ADR.** The initial prototype may use a heap object with the case in its
header and one handle slot, because that is the smallest implementation. It
must not turn that prototype choice into the permanent cost of every
`Option<Int>` or fieldless enum without measurement.

A lowered enum layout may later be:

- an immediate discriminant in one scalar slot;
- a discriminant followed by one or more typed payload slots;
- one handle slot naming a heap object;
- a niche layout where semantic analysis and the GC map can describe it
  precisely.

Decision 1 permits the second: a logical value may occupy several adjacent
eight-byte slots while every slot still has one index. A niche layout is more
complex because the reference map may have to interpret the word according to
the enum layout, so it is neither selected nor forbidden here. The required
invariant is that the lowered layout completely determines how to find every
reference; runtime code must not guess.

#192 already put the arities an ordinary program builds inline in
`EnumValue` rather than in a vector beside it, which is where the
`Some(x)`-costs-two-allocations finding went. The prototype records whether a
heap representation gives that win back before it becomes the default.

### 3. A dynamic value is sixteen bytes, and `dyn` and `Any` are not one thing

Only a genuinely erased value carries a tag:

```text
Dynamic {
    witness:  one word    // what this is, or how to call it
    payload:  one word    // immediate bits, or a heap handle
}
```

Sixteen bytes, because #109's finding says that is the floor for anything
self-describing in this language and no measurement can move it. A small
scalar payload stays inline, which preserves its full domain — an erased `Int`
is still a full 64 bits, which is the point of not squeezing it into eight.
Anything large or variable-sized is a handle.

**`dyn Trait` and a future `Any` share the shape and not the representation.**
#197 asks this directly and the answer is that they are distinct:

- **`dyn Trait` is `(witness, data)`,** where the witness is the
  implementation table for the trait the value was used at. It answers *how to
  call*. ADR 0006 already decided this is the one place a Cove value's runtime
  representation depends on its static type, and that the wrapper carries the
  trait so a diagnostic can name it.
- **`Any` is `(TypeId, payload)`,** where the type id names a declaration. It
  answers *what it is*.

Conflating them would make every `dyn` carry a type id it never reads and
every `Any` carry a table for a trait there is no bound naming. They may share
one 16-byte Rust type in the VM if that measures better; they are two witness
kinds in it, and the witness kind is part of the layout the VM owns.

**Nothing here adds `Any` to the language.** #197 says it need not, and this
ADR does not. What is decided is what the representation *would be*, so that a
slot design is not chosen today that forecloses it tomorrow.

### 4. Reflection reads metadata, never bits

Ordinary typed slots must not carry a tag so that reflection can find one.
That is a requirement rather than a preference, because a tag that exists only
for reflection is a tag on every `Int` in every loop in every program that
never reflects.

| what is being reflected on | where the type comes from |
| --- | --- |
| a statically known concrete type | the lowering emits the resolved type id as a constant; nothing reads the value |
| a generic parameter | the lowering's choice — a constant where it specializes, an explicit witness in the calling convention where it does not. Either way the type comes from the lowering, and never from the value |
| a `dyn Trait` value | the value's own witness word, which is what `dyn` *is*. One word, not the payload's bits |
| a future `Any` | its `TypeId` word. Same: the witness, not the payload |

`cove_ir::lower` already specializes on the checker's answers rather than
guessing — ADR 0027 records that "a declaration reached both directly and
through a value is lowered twice, once with an `Int` parameter as a scalar
slot and once with every argument on the value stack" — so the machinery that
would carry a resolved type id is the machinery that already carries a
`SlotKind`.

**Type identity is nominal, and a type id names a declaration.** ADR 0006
makes traits nominal and conformance explicit; ADR 0014 makes an exported
struct opaque or not. So two structurally identical declarations have two type
ids, and — should Cove gain type aliases — an alias is not a type, and every
alias of one declaration answers that declaration's id. A reflection API that
answered anything else would be reporting a different language than the
checker checks.

### 5. `Value` is materialized at the boundary, and the boundary list is closed

A `Value` is **built** when something crosses out of the VM, and **consumed**
when something crosses in. It is not a window onto a slot, a heap object or a
dynamic value; it is a separate object with a representation of its own, whose
parts are stored as the things the readers answer with.

Conversion happens at exactly these places and nowhere else:

- entry arguments and the entry's result;
- Host calls — arguments out, result in;
- Host reentry — the arguments a host passes to a Cove closure, and its answer;
- trace and value capture;
- the embedding APIs (`Vm::invoke` and its neighbours).

And the negative, which is the operative half: **the VM must not materialize a
`Value` to execute an instruction.** Adding two integers, reading a field,
calling a function, taking a branch — none of them builds one.

The interpreter is the exception, deliberately and permanently. The oracle
evaluates into `Value` and keeps doing so, because being readable is most of
what makes it useful as an oracle (ADR 0019) and because a second
representation in it would be a second thing to be wrong. So after this ADR
the oracle and the public boundary share one representation, and the VM shares
neither.

**This trades boundary cost for hot-loop cost, knowingly.** #90's measurement,
recorded on #109, is that Host conversion is already **25% of an embedding
invocation's time and 24% of its allocations**, and that the cost of a
crossing is dominated by *what* crosses rather than by the crossing — one
operation carrying a ten-field struct with two arrays costs 3,160 ns and 37
allocations, while a Host callback carrying one `Int`, including the reentry
that runs a Cove closure, costs 1,276.6 ns. Separating the representations can
only make a crossing more expensive, not less. This ADR accepts that and names
the measurement the prototype owes for it in "What is not decided here".

### 6. Every variant of `Value` is sealed

`Value` becomes an abstract type at the `cove-runtime` crate boundary. All
twenty-two variants, not a chosen subset.

```rust
pub struct Value(Repr);   // Repr is private; nothing outside the crate names it
```

**All of them, including the scalars.** #196 recommends option 3 — keep
`Int`, `Bool`, `Float`, `Str`, `Unit` matchable because they are stable by
construction, and seal only the variants with a pointer in them, which are
exactly the ones that have moved. That recommendation is not taken, and the
reason is decision 0. A partial seal leaves the public API a *mixture* of an
abstraction and a representation, and the visibility rule that makes #197's
thesis checkable — no public signature names an internal representation —
stops being checkable the moment some of them do. "Which half of `Value` may I
match on" is a question no embedder should have to hold in their head, and a
line drawn by today's implementation is a line that has to be re-argued every
time the implementation moves. The internal representation and the public API
are completely separated, or they are not separated.

The scalars are also less stable than they look. Decision 1 puts a `Float` in
a slot and decision 3 puts an erased `Int` in a `Dynamic`; whether a
materialized `Value` ends up storing a `Duration` as an `i64` or as something
else is precisely the kind of question this ADR is trying to stop being an
embedder's business.

**Sealing is about the crate boundary, not about the runtime's own code.** The
variants become crate-visible. `cove-runtime` — the interpreter, `builtins`,
`heap`, `trace`, `vm` — matches them exactly as it does now. What changes is
that `cove-ir`, `cove-cli`, `examples/rules`, and every embedder outside this
workspace go through constructors and readers.

**The constructor half has to be finished first, and it is not.** Today there
is `Value::structure`, `enumeration`, `array`, `set`, `map`, `ok`, `err`,
`some`, `none`, `error` — and **no constructor for a scalar at all**. A host
writes `Value::Int(3)` because there is nothing else to write. This workspace
alone names a `Value` variant about sixty-six times outside `cove-runtime`,
across fifteen distinct variants, and the four commonest are `Str`, `Int`,
`Enum` and `Struct`. So sealing requires, as work that must land before it:
`Value::int`, `float`, `bool`, `unit`, `duration`, `string`, `range`,
`resource`, `host_fn`, `host_module`, `type_name` — the mirrors of the readers
#195 added, on the way in.

### 7. `ValueView` gives exhaustive matching back, and is exhaustive on purpose

Sealing without this would take away a real safety property, which is #196's
objection and it is correct. So the exhaustive match comes back as a stable
public view:

```rust
impl Value {
    /// Classify this value. O(1), allocates nothing, borrows from `self`.
    pub fn view(&self) -> ValueView<'_>;
}

pub enum ValueView<'a> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(i64),
    Str(&'a str),
    Array(&'a [Value]),
    Vector(Elements<'a>),
    Map(Entries<'a>),
    Set(Members<'a>),
    Struct(StructView<'a>),
    Enum(EnumView<'a>),
    Closure(ClosureView<'a>),
    HostModule(&'a str),
    HostFn { module: &'a str, op: &'a str },
    Resource(&'a ResourceHandle),
    Type(&'a str),
    Range(RangeBounds),
    Task(TaskView<'a>),
    TaskScope(TaskScopeView<'a>),
    Shared(SharedView<'a>),
}
```

Four decisions are in that sketch.

**It is not `#[non_exhaustive]`, and that is the whole point.** A
`#[non_exhaustive]` view would give back the syntax of an exhaustive match and
none of its value. `ValueView` is the stable public *classification of Cove's
value kinds*, and it changes when the **language** gains a kind of value — not
when the runtime changes how one is stored.

That is the property sealing actually buys, and it is worth stating plainly
because it is easy to read this ADR as being about hiding things. Today one
enum carries two unrelated kinds of change, and an embedder cannot tell them
apart: `Box<StructValue>` → `Rc<StructValue>` and "Cove now has a new kind of
value" arrive at a host as the same compile error. After this, a
representation change is invisible and a language change is a compile error at
every `match` — which is exactly the right way round, and it is why #196's
"that compile error is a real safety property, not only a nuisance" is
answered rather than traded away.

**Its payloads borrow or copy, and never allocate.** `view()` must be O(1) or
it is a trap. That is what constrains the private `Repr`: each part must still
be *stored* as the thing the view answers with. The promise is smaller than
today's — it is about a materialized boundary type and not about a slot, a
heap object or a dynamic value — and "The tension #195 left" below is where
that is spelled out.

**It looks through `dyn Trait`, so there is no `Dyn` variant.** Every reader
#195 added looks through the wrapper, via `Value::erased`, for the reason that
PR gives: "the wrapper is a representation, not something the program put
there". None of the constructors can build one. A view that reintroduced it
would contradict every reader beside it. A host that wants the trait name asks
for it — `Value::dyn_trait() -> Option<&str>` — and that is a reader, not a
variant.

**`Vector` gets a guard, which is how the borrow design reaches the type it
could not.** `Elements<'a>` is an opaque public guard over the `RefCell` — it
derefs to `[Value]`, so a host reads a vector the way it reads an array — and
`Value::items` gains a companion `Value::vector_elements` answering one.
(`Value::elements` is already taken: it is a `Set`'s reader.) The same
shape covers `Map`, `Set`, `Shared` and anything else whose contents sit
behind an interior-mutability cell. This closes #196's "the one place the
borrow-based reader design cannot reach": the answer is that a reader whose
storage is behind a cell answers a guard rather than a slice, and the guard is
public and opaque.

`Vector` also keeps having no *copying* constructor, and that is not an
oversight either: a `Vector`'s identity is observable (`is` is defined only
for it), so a materialization that copied one would be wrong. The values whose
identity is observable — `Vector`, `Shared`, `Task`, `TaskScope`, `Resource` —
are materialized as handles rather than as copies, which is what they already
are.

**The readers stay.** `field`, `payload`, `as_int` and the rest are what a
host converting into its own types wants, and they are not replaced. `view()`
is for the host that wants to be told when the language changes.

### 8. What the collector is owed

Every representation above has to be walkable precisely, but the present
`Rc::strong_count` shortfall mechanism is not assumed to survive a
VM-owned handle representation unchanged.

Three different multiplicities must not be conflated:

1. **Root storage locations are yielded once.** Aliased environments or a
   place and the slot it names must not cause the same stored reference to be
   reported twice.
2. **Real graph edges are counted once each.** If two fields point at the same
   object, both references exist and both count for any reference-count
   comparison.
3. **Objects are expanded once during marking.** Shared or cyclic objects may
   be reached through many edges, but their interiors are traversed once.

Under the representations above:

- a **scalar slot** contains no reference by construction;
- a **handle slot** is a root according to the frame reference map;
- a **place** is not an additional root: it names storage whose slot is
  already described by the frame layout;
- a **heap object's** interior is walked according to its layout's reference
  map;
- a **`Dynamic` value** has a two-slot layout whose witness says whether the
  payload slot is immediate bits or a handle.

The last field is required for precise scanning, but it does not by itself
solve temporary rooting. The prototype must choose and test one of these
coherent mechanisms:

- handles participate in reference counting strongly enough for the existing
  shortfall rule to detect a Rust-local copy;
- every Rust-local handle that can survive to a safepoint is registered in an
  explicit temporary/shadow-root stack;
- the dispatch discipline guarantees that a collection can occur only when
  every live handle has been returned to a mapped VM slot;
- another mechanism with the same precise invariant is specified.

An index or offset copied into a Rust local does not change
`Rc::strong_count`, so **the ADR does not claim that the current shortfall
collector survives such a handle untouched**. The vertical slice must include
a safepoint with a heap handle temporarily outside the VM stack and prove that
the object remains live. Only after handle semantics and temporary-root
discipline are chosen can the collector migration be called specified.

## The tension #195 left, and how it is resolved

#195's readers promise that every part a reader answers with is **stored** as
the thing it answers with. #197 proposes 8-byte typed slots, and a value
unpacked from an 8-byte slot is a **computed** value. Both cannot be true of
one type.

**The resolution is that they are not one type.** `Value` keeps a materialized
representation of its own, and #195's signatures remain honest about *it* —
unchanged, not weakened, not deprecated. `payload() -> Option<&[Value]>` still
promises a contiguous payload, and `Value` can keep that promise for the same
reason it keeps it today: a `Value` is built by the boundary, and the boundary
can build whatever shape the readers require.

What changes is what that promise is *about*. Before this ADR, `Value` is also
the VM's slot and the VM's heap object, so `payload() -> &[Value]` promises
that the **VM's** enum payload stays contiguous. After it, the promise binds
only the boundary type, and the VM's `HeapObject` may be non-contiguous,
tagged, decoded lazily, held behind a lock, or split across arenas — none of
it visible, because none of it is a `Value`.

So the honest statement of what sealing buys is not "the representation is now
free to change". It is:

> The promise got smaller and it got named. What `Value` promises is that each
> part a reader answers with is stored as the thing it answers with — a
> promise about a materialization, not about a slot, a heap object, or a
> dynamic value, which are the things that will actually change.

**The signatures do not change.** That is the decision, stated as the
alternative it was chosen over: the other coherent answer is to change the
readers to return owned or guard types — `payload() -> Option<Payload<'_>>` —
so that `Value` could be a lazy window onto VM storage. It is refused, for
three reasons.

1. It would break every embedder a second time, for a benefit measured in
   avoiding a copy at a boundary that #90 already shows is dominated by *what*
   crosses rather than by how it is shaped.
2. A lazy window keeps a `Value` alive against VM storage, which means a host
   holding one constrains when a collection may run. ADR 0011's per-task heap
   and `docs/VM_ARCHITECTURE.md`'s safepoint list both assume nothing outside
   the runtime is holding a view into the heap at a safepoint. Materialization
   is what keeps that assumption true, and it is worth more than a copy.
3. Materialization is what makes the oracle and the boundary share one
   representation, which is what keeps the differential harness comparing
   values rather than comparing two conversions.

The cost is honest and is stated in "Costs": the boundary copies, and the
boundary is already a quarter of an embedding invocation.

`Vector` is where this was already visible before anybody proposed a slot, and
decision 7's guard is the general form of its answer: a part that cannot be
handed out as a plain borrow is handed out as an opaque guard, and the guard
is public API. That is the mechanism the design has for any future part whose
storage will not sit still — and having it means the next such part does not
need a `Deref` shim written for it the way `Payload` did.

## Alternatives

Six, as #197 requires, against the nine criteria it names.

**No cell in this table is a measured number, and that is deliberate.** Every
one of them is a structural consequence — what a representation *makes* true,
which is knowable without running anything. This ADR makes no performance
claim it has not measured, for the reason
[#179](https://github.com/myuon/cove/issues/179) records: a control build
whose only change is one `Inst` variant that is never emitted and never
executed moved `benches/arith` by +23.5%, so a cross-build absolute on this
workspace cannot separate a design from its code layout. Two facts in this
repository *are* measured and they are named under the table, which is also
where the numbers this design still owes are named.

| | full `Int64`/`Float64` | frame memory | scalar hot loop | GC roots | dynamic / reflection | heap pressure | portability | JIT | complexity |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **A universal 16-byte tagged value** | yes | 2× a typed slot | tag read + branch per op | one uniform walk, easy | free — every value self-describes | unchanged | clean | uniform ABI, wide slots | low |
| **A universal 8-byte NaN-boxed value** | **no** — ~51 payload bits | 1× | boxing near `i64::MAX` | uniform, but must decode | free | **allocates on large `Int`** | float bit-pattern assumptions | uniform | high |
| **Tagged pointers + boxed full-width scalars** | **no** — 61–62 bits | 1× | boxing near `i64::MAX` | uniform | free | **allocates on large `Int`** | alignment assumptions | uniform | high |
| **8-byte typed slots + 16-byte dynamic** ← chosen | **yes** | 1× typed, 2× erased | no tag, no branch | per-frame reference map + witness | metadata; a `dyn` reads its witness | unchanged | clean | frame layout is what a JIT needs | **high** |
| **8-byte typed slots, all erased values boxed** | yes | 1× everywhere | same as chosen | uniform: every erased slot is a handle | **an erased `Int` allocates** | worse at erasure boundaries | clean | same | medium |
| **The status quo — a 24-byte `Value` in every slot** | yes | 3× | clone, drop, match per op | whole window is the root set | free | unchanged | clean | 24-byte slots to describe | **lowest — it exists** |

The two measured facts, and they are the only two: **a 24-byte `Value` against
a 40-byte one is about 1% per 8 bytes** across the suite, from #109's
padding-variant experiment; and **two scalar/`Value` boundary crossings are
16.0 ns a turn and allocate nothing at all**, from #123's matrix, where seven
of its nine rows allocate not once in two million turns. What no number in
this repository can currently say is what an 8-byte typed slot is worth
against ADR 0027's arrangement, or what materialization costs a boundary
carrying a compound value. Those are the prototype's, and "What is not decided
here" lists them.

**Rows 2 and 3 are refused on semantics, not on speed.** #109 settled this and
`docs/VM_ARCHITECTURE.md` states it: "neither is rejected here for being slow
or unportable — they are rejected because Cove's `Int` does not fit in them".
An 8-byte universal value cannot hold a full 64-bit integer and a tag, so both
would box integers outside their range, and arithmetic near `i64::MAX` would
allocate. That is a change to what `Int` *costs* in a language whose Card
promises what `Int` *is*.

**Row 1 is refused because it pays twice for a tag almost nothing reads.** It
is the honest fallback and it is genuinely simple — one uniform slot, one
uniform walk. But the checker already knows the type of nearly every slot in a
program, so a universal tag is a per-slot cost to store a fact that was
already proved, and #197's whole premise is that a proof is cheaper than a
tag. It also does not reach eight bytes, so the frame memory it wants is
double what the chosen row wants for the same program.

**Row 5 is refused for the same reason as rows 2 and 3, one level in.** Boxing
every erased value is simpler — every erased slot is a handle, and the GC
question disappears entirely, which is not nothing given decision 8's witness
obligation. But it makes an erased `Int` allocate, and an erased `Int` is a
full `Int`; the language should not have a value whose *width* determines
whether using it costs a heap object. The 8 bytes saved per erased slot are
not worth an allocation, and #109's measurement says what 8 bytes is worth.

**Row 6 is the status quo and it is refused by #197 rather than here**, but it
deserves the honest entry it gets: it is the only row that exists, it is
correct, and every number in this repository was measured on it. What is wrong
with it is not a benchmark. It is that one type is doing five jobs, so a change
to any one of them is a change to all five, and the record in Context is what
that has cost three times.

**Row 4 is chosen, and its own cell says its cost: high complexity.** It is
the only row with two representations to debug and a witness obligation the
collector did not have. That is the trade, stated as a trade.

### `Option` and enum layout, which #197 asks separately

Not fixed before the prototype. Decision 2 permits an immediate discriminant,
multiple typed slots, a heap handle, or a precisely described niche layout.
The initial slice may use the heap form for implementation economy, but it
must measure allocation and pointer-chasing on `Option<Int>` and
`Result<Int, Error>` before selecting a default.

## What this supersedes, and what it does not

**It supersedes nothing.** Going through the ADRs it touches, one at a time,
because the interesting ones are the near misses.

**ADR 0019, "Slots, not names".** Untouched and extended. "A function's frame
is a contiguous region of slots whose size is known when the function is
lowered" is exactly what decision 1 says; what this ADR adds is how wide a slot
is and where its interpretation comes from, which ADR 0019 never decided.
ADR 0019's "fuel is charged for VM work" and "`fuel_spent` becomes
backend-specific" absorb the instruction-count changes a new slot design will
cause, which is why those changes need no supersession either.

**ADR 0027.** Untouched, and this is the one that most looks like a
supersession and is not. ADR 0027 decided that a place names a slot and a
capture takes the slot its own kind names, and listed a single physical frame
under "What is not decided here" — "not built, not measured, and **not refused
here**". Deciding something an accepted ADR explicitly declined to decide is
not replacing its decision. Its actual decision — that only the checker's
answer about a slot's type decides its representation — is the premise
decision 1 is built on, not something decision 1 contradicts.

**[ADR 0013](0013-host-resource-handles.md).** Untouched, and this is the near
miss worth naming precisely.
ADR 0013 decided that "a resource handle is a name" and that
"`Value::Resource(Arc<ResourceHandle>)` is the whole of the representation,
and every field of it is part of the name". Sealing means an embedder can no
longer *write* `Value::Resource(..)` — but the decision was about what a
handle **is**, not about which syntax reaches it. `ResourceHandle` stays `pub`
with `pub` fields, `Value::resource() -> Option<&ResourceHandle>` already
exists, and a `Value::resource(..)` constructor is on decision 6's list. What
ADR 0013 *describes* rather than decides — the sixteen hand-written
`let [Value::Str(name)] = args.as_slice() else { ... }` arms across the shipped
hosts, which #195 deliberately left alone — is code this ADR requires to
change. Describing code is not deciding it, so there is nothing to supersede,
and the migration is named in Costs instead.

**[ADR 0011](0011-garbage-collection.md) and the collector.** Untouched.
Precise, non-moving, per task, with
the reference-count shortfall rule rooting what a Rust local holds. Decision 8
adds an obligation to the *representations* rather than changing the
collector's decision.

**[ADR 0006](0006-traits-and-dispatch.md).** Untouched, and relied on.
"Dynamic dispatch is the first place
where a Cove value's runtime representation depends on its static type" is the
sentence decision 3 turns into a witness word.

**[ADR 0012](0012-performance-gate-and-native-backend.md).** Untouched. The
specification ranks above the oracle ranks above a
backend, and decision 5's "the interpreter keeps evaluating into `Value`" is
what keeps the oracle readable, which is most of what makes it useful.

**Issue #109.** Stays closed. It closed under its own acceptance criterion 1 —
close without the broad implementation, with evidence that other bottlenecks
come first — and nothing here disturbs that verdict. This ADR is not the broad
`Value` redesign #109 refused; it is the *separation* of five things that were
one thing, and #109's own measurements are what determine its shape. Two of
#109's "work, if the gate is met" items that it left undone are what this ADR
finally decides: "define migration away from external pattern matching on
internal `Value` variants", and "internal representation should become less
exposed to embedders".

## Costs

Every one of these is real and none of them is hypothetical.

**A breaking change for every embedder, once, deliberately.** Every host that
matches a `Value` variant or constructs one by naming it stops compiling. In
this workspace alone that is about sixty-six mentions across fifteen variants
in `cove-ir`, `cove-cli` and `examples/rules`, plus the sixteen
`let [Value::Str(name)] = args.as_slice()` arms in the shipped hosts that
ADR 0013 named and #195 left. Outside it, it is every embedding that exists.
The migration is mechanical — a match becomes `view()` or a reader, a
construction becomes a constructor — and it is not small, and the constructor
half has to be written before any of it can start.

**A `ValueView` to maintain in step with the language.** It is exhaustive on
purpose, so it is a second place every new kind of Cove value must be added,
and forgetting is a compile error inside the crate rather than a silent gap —
which is the good case, and it is still a cost. Every `ValueView` variant also
constrains what the private `Repr` may store, which is the promise decision 7
narrowed rather than removed.

**The boundary can only get more expensive.** #90 measured Host conversion at
25% of an embedding invocation's time and 24% of its allocations, on the
current arrangement where the boundary hands over a `Value` the VM was already
holding. Materializing from slots and heap objects adds work to exactly that
path. This ADR takes that trade because the hot loop runs far more often than
the boundary does, and it does not pretend the trade is free.

**Two representations to debug.** A VM bug can now be "the slot was right and
the materialization was wrong", which is a class of bug that cannot exist
today. Worse, the oracle and the VM stop sharing a value representation, so a
differential disagreement can now be a *conversion* bug rather than an
execution bug — and the differential harness is the project's main safety net.
Whoever builds the prototype owes a conversion round-trip test per value
family, which is #109's partially-done item and is now load-bearing.

**A witness gains a required field it would not otherwise need.** Decision 8
makes "is my payload a handle" part of every witness, because a per-frame
reference map cannot answer it for a dynamic value. That is a real cost of
choosing row 4 over row 5. The prototype additionally owes an explicit
temporary-root discipline if VM heap handles are indices or offsets rather
than reference-counted handles.

**`Value`'s 24-byte assertion stops being a compatibility statement.** PR
#187's `const _` stays — it is a control against silent drift and it earns its
place — but after sealing it is an internal fact, and nothing outside the
crate may depend on it.

## What is not decided here

**The physical arrangement of the frame was left open here and is closed by
ADR 0034.** Decision 1 decides one logical frame, one numbering, one base,
8-byte slots, and value layouts that may span adjacent slots. ADR 0034 selects
one contiguous word stack with layout-derived reference metadata for the
production VM and requires the independently based stacks to be removed.

**Every number.** This ADR makes no performance claim it has not measured, and
the two it cites are #109's width table and #123's crossing cost. The
measurements it *owes*, which are the prototype's gate:

- `benches/arith` and `benches/call`, dynamic instruction counts alongside
  wall time, within-build ratios rather than cross-build absolutes;
- one heap-backed field case and one dynamic/type-erased case;
- **the boundary, against a compound value** — the ten-field struct with two
  arrays, not an `Int`, because #109's closing note says a per-call figure
  understates the boundary for anything carrying a compound value and that is
  what an embedding's operations carry;
- bytes per live frame, and allocations per turn.

**Whether Cove gets `Any`, or reflection at all.** Decision 3 and decision 4
say what the representation would be so that today's slot design does not
foreclose tomorrow's feature. Neither adds anything to the language, and #197
explicitly does not ask them to.

**Whether the materialized `Value` gets narrower or wider once sealed.** It
becomes a private fact, and this ADR deliberately does not fix it. What it
fixes is that nobody outside the crate may care.

**The migration order.** Constructors first, then `view()`, then the seal,
then the internal representations behind it — that is the obvious order and it
is a plan rather than a decision, and #197's prototype phase is where a plan
belongs.

**Whether a moving collector ever happens.** Non-moving, per ADR 0011 and the
Card. `docs/VM_ARCHITECTURE.md` already writes down what a moving collector
would owe. Decision 2 records movement as a heap/arena invariant without
requiring a word in every object header, and nothing here asks the question.

## Consequences

- The sentence #197 calls its thesis — "changing the VM's internal
  representation must not require exposing that representation to embedders" —
  becomes checkable with a `grep` over `pub fn` rather than maintained by
  discipline. That is the whole of what this ADR is for.
- A representation change and a language change stop arriving at an embedder
  as the same compile error. The first becomes invisible; the second becomes a
  compile error at every `match`, which is the right way round and is what
  makes sealing acceptable rather than merely convenient.
- The next shape change costs no shim. #192 paid for a `Deref<Target =
  [Value]>` and a `From<Vec<Value>>` so that a payload change would not reach
  embedders; #104 survived because two smart pointers happened to deref to the
  same thing. Neither mechanism is needed again.
- `cove-runtime` gains a second internal vocabulary — slot, handle, header,
  witness, layout id — and the oracle keeps the first. Every crate above
  `cove-runtime` loses both and gets `Value`, its constructors, its readers,
  and `ValueView`.
- VM fuel changes for programs whose slots change representation, because VM
  fuel is its instruction count. ADR 0019 makes fuel backend-specific and says
  why; ADR 0024's four bounds are untouched, since nothing here changes what
  may be gathered between two checks.
- `cove trace` and `cove replay` keep working on both backends, because trace
  events stay source-level (ADR 0019) and a captured value is materialized at
  a boundary decision 5 lists. What a trace records is a `Value`, and a
  `Value` is what it has always been.
- Nothing in this ADR may be built before it is accepted. That is #197's own
  requirement and it is the reason this ADR's status is `Proposed`.
