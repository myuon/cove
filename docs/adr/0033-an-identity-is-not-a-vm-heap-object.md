# ADR 0033: An identity is not a VM heap object, and the seam stays one-way

- Status: Superseded by [ADR 0034](0034-one-physical-word-stack.md). Cove-owned
  Vector and Shared values live in the VM heap region rather than an identity
  store. Task control state and Host-owned resources remain externally owned,
  as restated by ADR 0034.
- Date: 2026-08-31
- Supersedes:
  [ADR 0028](0028-five-representations-and-one-is-public.md)'s **answer to
  which store an identity-bearing value's handle names**. That answer is
  spread across two places and neither of them decides it. Decision 7's
  closing sentence — "The values whose identity is observable — `Vector`,
  `Shared`, `Task`, `TaskScope`, `Resource` — are materialized as handles
  rather than as copies, which is what they already are" — names one storage
  class for five kinds and says nothing about which store the handle is a
  handle *into*. Decision 1's slot table then supplies the missing half by
  implication: its row "a heap-backed value — struct, string, array,
  **vector**, map, set, closure, enum" holds "a VM heap handle", which places
  a `Vector` in the VM-owned traced heap — where decision 7's identity rule
  forbids it to arrive, because arriving means being copied. This ADR
  replaces the implication and keeps the rule. Nothing else in ADR 0028 is replaced — "What ADR 0028 keeps",
  below, goes through the parts that stand one at a time, because a reader of
  a superseding ADR needs to know what was *not* replaced as much as what was
- Decides: [issue #218](https://github.com/myuon/cove/issues/218), which
  reported the gap and named the two options; the owner's comment on it is the
  decision this ADR records
- Records the relation to
  [issue #197](https://github.com/myuon/cove/issues/197): this gates **only
  the identity-bearing part** of that migration, not the whole of it, and
  [issue #212](https://github.com/myuon/cove/issues/212) is unaffected and
  continues

## Context

### The gap is one question ADR 0028 asks and does not answer

ADR 0028 decides that a slot is eight bytes and that what it holds for a
heap-backed value is a handle. It also decides that five kinds of value have
observable identity and must never be materialized by copying. Both are right.
Together they ask a third question — *which store does an identity-bearing
value's handle name?* — and ADR 0028 supplies no sentence that answers it.

What it supplies instead is a table row that reads like an answer. `vector`
appears in decision 1's list of heap-backed values whose eight bytes hold "a
VM heap handle", beside `struct`, `string`, `array`, `map`, `set`, `closure`
and `enum`. A reader implementing decision 1 straightforwardly puts a `Vector`
in the VM-owned traced heap. A reader implementing decision 7 cannot, because
getting it there means copying it and decision 7 refuses that. The ADR
therefore *reads* as though there is a single answer while providing none, and
that is a worse failure than an acknowledged gap, because it is invisible
until somebody builds it.

Somebody did. [PR #215](https://github.com/myuon/cove/pull/215), adding a
variable-length tail to the slice, reported this as the one thing ADR 0028
decides that could not be implemented.

### Neither of the two obvious ways out is free, and one of them is unsound

Issue #218 states the choice as two options: either the five kinds stay in
`crate::heap`'s counted world permanently, or the materialisation seam stops
being one-way so that a `Value` can be consumed *into* the handle heap. The
second is what "one uniform heap" would require, and the reason it is not
cheap is a finding that predates the question.

[PR #210](https://github.com/myuon/cove/pull/210) established that a
shadow-root stack over `Value` would be **unsound**. A `Value` sitting in a
Rust local is already accounted for by its own reference count, so a second
root list yields one reference twice and thereby conceals the very shortfall
that roots it — which is [PR #192](https://github.com/myuon/cove/pull/192)'s
`arg_vectors` failure, arrived at from the other direction. The shadow stack
is sound over a *handle* and only over a handle, because **a handle is not a
counted reference**. Nothing in `crates/cove-runtime/src/slot.rs` — no slot,
no handle, no heap object — *holds* a `Value`; the type appears there only as
what materialisation produces on the way out. That is a load-bearing property
rather than tidiness, and it is the property this ADR is protecting.

[PR #211](https://github.com/myuon/cove/pull/211) turned that into the shape
of the seam: materialisation runs one way, out of the VM, and the two
universes are disjoint. #215 then hit the wall the disjointness implies. A
`Vector` in the handle heap can be neither a copy — decision 7 refuses it, and
its identity is the point — nor a `Handle` inside a `Value`, because that is
precisely what joins the two heaps back together.

So making the seam bidirectional is not a matter of writing the inbound
conversion. It requires answering a rooting question in the other direction
that #211 named and did not answer: consuming a `Value` on the way in
allocates in the handle heap with a **half-built object in a Rust local**,
which must be rooted for the collection that can happen while it is being
built.

### The five are not one thing, and that is why the question had no answer

The reason ADR 0028 could name the five and still not place them is that they
do not belong in one place. Decision 7's list groups them by an *observable
language property* — identity — and then decision 1's table asks where the
handle points, which is a question about **ownership**. Those two
classifications do not coincide, and ADR 0028's own decision 1 already knew
it for one of the five: a host resource gets its own row and its own kind of
handle, a "stable host handle", distinct from a VM heap handle. ADR 0031 later
made that distinction a rule.

`Resource` was never the odd one out. It was the only member of the list whose
ownership ADR 0028 happened to have written down.

## Decision

### 1. The seam stays one-way

**A public, materialised `Value` does not contain a VM heap handle, and no
general `Value -> HandleHeap` import path is added in order to finish #197.**

Decision 5 of ADR 0028 stands exactly as written: a `Value` is built when
something crosses out of the VM, the boundary list is closed, and the VM must
not materialize a `Value` to execute an instruction. What this ADR adds is the
negative that #211 established and #218 asked to be made a decision: the
inbound half is not opened to make one uniform heap possible.

### 2. Five identity-bearing kinds, three ownership classes

The five kinds decision 7 names are **not one storage class**:

| kind | ownership class |
| --- | --- |
| `Vector`, `Shared` | **Cove-owned mutable identity cells** |
| `Task`, `TaskScope` | **runtime-control handles with lifecycle semantics** |
| `Resource` | a **Host-owned stable handle** — ADR 0028's decision 1 already distinguishes it from a VM heap reference, and ADR 0031 states the rule |

This classification is the substance of this ADR. Everything below follows
from it.

A `Vector` and a `Shared` are cells the Cove program mutates and compares by
identity; the VM did not mint them for its own convenience and cannot decide
their lifetime by tracing alone. A `Task` and a `TaskScope` are not data at
all — they are control, with a lifecycle (spawn, join, exit, scope teardown)
that ADR 0003 and ADR 0008 define and that no object header describes. A
`Resource` names something the host owns.

### 3. They stay outside the ordinary VM-owned traced object heap

**None of the three classes is an object in the VM-owned traced heap.** Each
lives in the store appropriate to its ownership: an identity store for the
Cove-owned cells, the runtime's control structures for tasks and scopes, and
the host's for a resource.

This does not put them outside the eight-byte slot. **A typed VM slot may hold
a compact handle or reference into the appropriate identity, control or Host
store.** That is not the same as either of the two things it is easy to
confuse it with:

- it does **not** mean retaining a general 24-byte `Value` slot for these
  kinds — decision 1's eight bytes hold for them as for everything else;
- it does **not** mean copying the object — the handle names the same
  identity, which is the whole reason a handle is what a slot holds.

### 4. The eight-byte-slot goal permits several compact handle kinds

Stated explicitly, because decision 1's table reads as though there is one.
A slot's eight bytes may hold a VM heap handle, or a handle into an identity
store, or a runtime-control handle, or a stable host handle. What decision 1
actually requires of all of them is what it says a slot must satisfy: eight
bytes, untagged, its kind supplied by the layout rather than by the bits, and
never reachable by a walk that treats a scalar as a reference. **Several
compact handle kinds satisfy that; one uniform handle kind was never what made
it true.**

### 5. Materialisation produces an owning wrapper for the same identity

At a boundary decision 5 lists, an identity-bearing value is materialised as
an **owning opaque wrapper for the same identity** — not a copy, and not a
window that keeps VM storage alive. And the property that makes it a
materialisation of an *identity* rather than of a value:

> **A round trip through a Host call preserves identity.** A value that goes
> out to a host and comes back is the same `Vector`, the same `Shared`, the
> same `Task`, `TaskScope` or `Resource` — observably so, under `is`.

This is what decision 7 meant by "materialized as handles rather than as
copies", said in terms of what a host can observe rather than in terms of what
the runtime happens to store.

### 6. Plain copyable aggregates remain candidates for the VM-owned heap

Strings, arrays, structs, ordinary enums, and later map and set storage **where
their semantics permit**, are candidates for the VM-owned handle heap and are
what the migration in #197 is for. Nothing in this ADR narrows that.

The hedge on maps and sets is deliberate and is not a decision deferred out of
caution: `Map` and `Set` need a reference-map `Part` that can say "a key", and
today `slot::Part` is `Int | Float | Bool | Nested`. Whether their storage can
be described precisely enough to live in the traced heap is a question about
their layout, to be answered when that `Part` is designed. This ADR does not
answer it in either direction.

### 7. What the implementation owes

Obligations, not suggestions.

- **One explicit handle kind, and one reference-map entry, per external
  identity class.** Not one shared "not ours" kind covering all three.
- **Lifecycle tests** for: storage in a frame; storage in a heap object's
  field; a Host round trip; Host reentry; task exit; and collection.
- **It must not fall back to a generic `Value` slot** as the representation of
  those handles. A `Value` in a slot is the thing #197 exists to remove, and
  reintroducing it for the five kinds that were hardest is reintroducing it
  where it costs most.

## What this deliberately accepts, and what it declines

**A mixed physical object model, for semantically different ownership
classes.** That is a real cost and it is chosen with its eyes open: there will
be more than one store, more than one kind of handle, and a reader of the
runtime will have to know which is which.

One uniform heap is what the mixed model is being traded against, and it is
not valuable enough to justify any of the three things it would require.

1. **A bidirectional seam and its half-built-object rooting protocol.** #211
   named the inbound rooting question and did not answer it; #210 is why it
   cannot be answered by the mechanism that answers the outbound one. A
   protocol for rooting an object that is under construction in a Rust local
   is a new invariant in the most dangerous part of the system, and it would
   be bought with nothing but uniformity.
2. **Making public values constrain the lifetime of a per-task VM heap.** A
   `Value` holding a VM heap handle is a host holding a reference into VM
   storage. ADR 0011's per-task heap and `docs/VM_ARCHITECTURE.md`'s safepoint
   list both assume nothing outside the runtime holds a view into the heap at
   a safepoint, and ADR 0028's own resolution of the tension #195 left already
   refused a lazy window for exactly this reason. Importing a handle *into* a
   `Value` is that refusal undone from the other side.
3. **Conflating VM, runtime-control and Host ownership.** These three decide
   lifetime by three different mechanisms — tracing, a task lifecycle, and the
   host's own bookkeeping. One heap does not make them one mechanism; it makes
   the difference implicit, and ADR 0031 is a recent record of what an
   implicit distinction between two kinds of handle costs.

An ADR that states a decision without stating what it declined is half an ADR,
and these three are what this one declined.

## What ADR 0028 keeps

Everything except the implication named in the header. Stated one at a time,
because this ADR is easy to mistake for a re-opening of ADR 0028 and it is not
one.

- **The one-way materialisation seam, which is preserved rather than
  replaced.** Decision 5 in full: `Value` is built when something crosses out,
  consumed when something crosses in, the boundary list is closed, the VM must
  not materialize a `Value` to execute an instruction, and the interpreter is
  the deliberate and permanent exception. This ADR strengthens that seam; it
  does not touch it.
- **Decision 1**, apart from the reading of one table row. Eight-byte untagged
  slots, one logical frame with one numbering and one base, a value location
  that may span adjacent slots, and the invariant that a slot the layout calls
  scalar is never reachable by a walk that treats it as a reference.
- **Decision 2.** A heap object is a handle plus a header carrying a layout
  id, a size, a reference map, a payload layout and the heap's movement
  guarantee — for the objects that are in that heap.
- **Decisions 3 and 4.** The sixteen-byte `(witness, payload)` dynamic value,
  and reflection that reads metadata and never bits.
- **Decision 6, the seal.** All twenty-two variants of `Value`, including the
  scalars, and the argument that a partial seal is worse than none.
- **Decision 7, `ValueView`.** Exhaustive on purpose and not
  `#[non_exhaustive]`; changing when the language gains a kind of value and
  not when the runtime changes how one is stored; the opaque guard for parts
  behind an interior-mutability cell; and — the part of decision 7 this ADR
  exists to keep true — **`Vector` keeps having no copying constructor, and an
  identity-bearing value is materialized as a handle rather than as a copy.**
  What moves is only which store that handle names.
- **Decision 8, what the collector is owed, and all three of its
  multiplicities.** Root storage locations yielded once; real graph edges
  counted once each; objects expanded once during marking. This ADR adds
  stores that the multiplicities must hold across; it does not alter them, and
  decision 8's requirement that the prototype choose and test one coherent
  temporary-rooting mechanism is unchanged.
- **The resolution of the tension #195 left.** `Value` keeps a materialized
  representation of its own, the readers keep their names and shapes, and the
  promise binds the boundary type rather than a slot or a heap object. Clause
  5 above is that resolution applied to identity rather than a change to it.
- **The Alternatives, the Costs, and "What is not decided here"**, including
  every measurement the prototype still owes.

ADR 0031's restatement of the visibility rule is also untouched, and clause 3
above is stated in its vocabulary: a handle into storage the VM owns is never
public, and the handles this ADR adds are not handles into storage the VM
owns.

## What is not decided here

The owner's decision is a classification and a set of obligations. Several
implementation questions follow from it, and inventing answers to them here
would be recording decisions nobody made.

- **The representation of each compact handle kind.** Width, tagging, whether
  the kinds share one numbering or have one each, and how a slot's layout
  names which kind it holds. Clause 4 requires several kinds and clause 7
  requires them to be explicit; neither says what they are made of.
- **Where each store physically lives**, and whether the Cove-owned identity
  store keeps today's `crate::heap` counted representation, becomes traced, or
  becomes something else. This ADR places `Vector` and `Shared` outside the
  VM-owned traced object heap and does not say what they are inside.
- **How reachability is computed across stores** — an object in the traced
  heap holding a handle to an identity cell that holds `Value`s, and the
  reverse. Clause 7 makes collection one of the lifecycle cases the
  implementation must test; what mechanism passes that test is decision 8's
  open choice, not this ADR's.
- **The `Part` that can say "a key"**, and therefore whether `Map` and `Set`
  storage moves at all. Clause 6 says "where their semantics permit" and stops
  there.
- **The name and shape of the owning opaque wrapper** clause 5 requires, and
  whether one wrapper type serves all three ownership classes or each has its
  own.
- **The migration order** within the identity-bearing part. #197's prototype
  phase is where a plan belongs, and ADR 0028 already says so.

## Consequences

- **#218's option 1 is chosen, with its premise corrected.** The five kinds do
  not become objects in the VM-owned traced heap. But they are not thereby
  left in "the counted world" as a single leftover: they are three ownership
  classes with three stores, and the eight-byte slot reaches all of them.
- **The migration in #197 is partial by construction, and that is the design
  rather than a shortfall.** #218's own analysis is what makes this
  bearable: #210's finding says rooting must be settled per heap, and settling
  it per heap is exactly what having named the stores makes possible.
- **#212 is unaffected and continues.** Its scalar/call/`var` vertical slice
  does not need any of these identity stores, and nothing above changes what a
  scalar slot is.
- **This gates only the identity-bearing part of the broad heap migration.**
  Strings, arrays, structs and ordinary enums are not waiting on anything
  here.
- **`Map` and `Set` stay blocked**, on the `Part` question rather than on this
  one. The difference matters: after this ADR their blocker is a layout
  question with an owner, not an unanswered ADR.
- **The runtime gains more than one handle vocabulary**, and a reader must
  know which store a handle names. Clause 7's "one explicit handle kind per
  external identity class" is what keeps that knowable at the type level
  rather than by comment.
- **Nothing in `cove-runtime` changes because this ADR was written.** It
  records a decision about what the migration may and may not do; the work is
  #197's.
