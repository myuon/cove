# ADR 0069: Structural reflection stays inside the standard library

- Status: Accepted
- Date: 2026-09-28
- Decides: that the structural-reflection capability ADR 0068 built —
  `DynamicView`, the `core.dynamic*` observations, `RenderPath` and
  `IdentitySet` — stays an internal capability of the standard library and
  `core`, and is not published as Cove API; why publishing the current surface
  is rejected, named against the code that would be exposed; that a limited
  derive/protocol mechanism is the preferred direction if a public need is ever
  shown, and is neither designed nor implemented here; and what a proposal must
  bring to reopen the question. Recorded for
  [ADR 0068](0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
  Phase 5b, under
  [issue #432](https://github.com/myuon/cove/issues/432)
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0068](0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
  Decision 6, **"The first surface is private to the standard library"**,
  which ends "After the four fallback migrations, a separate ADR may expose a
  restricted public API. No private representation chosen here is promised to
  that API." This is that separate ADR, and its answer is *not yet*. Decision 6
  is not changed: the surface was private, and it stays private for the reasons
  below rather than for Decision 6's reason, which was that the migration did
  not need to answer the question. ADR 0068's Decisions 3 (type identity is
  opaque, per program and not serializable), 7 (opaque kinds remain opaque) and
  9 (the view is verified as a capability) are the facts this ADR reasons
  from, and none of them is weakened;
  [ADR 0014](0014-opaque-exported-types.md), whose `opaque` struct is one of
  the things a public view would have to answer for;
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 3,
  the synthesized layout-directed functions that are the nearest existing
  thing to a derive

## Context

### The question ADR 0068 deferred is now askable

ADR 0068 moved `==`, key order, key admission and rendering over an erased
value — a `dyn Trait`, or a Host `Any` — out of four Rust walks and into
`std.dynamic`, written in Cove over a private, read-only view of the box. Its
Phase 5 put one item after everything else: "decide in a separate ADR whether
any reflection surface becomes public Cove API". Every other item of Phase 5
is closed ([docs/measurements/adr-0068.md](../measurements/adr-0068.md)), so
the question is now askable against a finished implementation rather than a
plan.

The surface that exists is:

- **`DynamicView`**, a three-word inline value (`crates/cove-ir/src/dynamic.rs`:
  `VIEW_LAYOUT`, the viewed value's `LayoutId`; `VIEW_OWNER`, a `Ref` to the
  object that roots it; `VIEW_AT`, the payload word it begins at), made only by
  `core.dynamicOpen(value: Any)`.
- **Twenty-one `core.*` observations** beside the open, in
  `crates/cove-schema/src/builtins.rs`:
  `dynamicKind`, `dynamicSameType`, `dynamicSameObject`, `dynamicNameOrder`,
  the five scalar reads, `dynamicCase`, `dynamicChildCount`, `dynamicChild`,
  `dynamicTypeName`, `dynamicFieldName`, `dynamicCaseName`, `dynamicOpaque`,
  `dynamicHandleText`, `dynamicOnPath`, and `identitySet` / `identityEnter` /
  `identityLeave`.
- **`RenderPath`**, two words — an `Addr` of the innermost frame entry of a
  rendering composed for a known layout, and a depth — asked one question,
  `core.dynamicOnPath`.
- **`IdentitySet`**, two words in the frame of the function that made it, over
  an untraced table of vector addresses (`Shape::IdentityTable`).

None of it is reachable from a program. `DynamicView`, `RenderPath`,
`IdentitySet` and `Any` are resolved only in a module
`cove_sema::stdlib::is_library_module` answers for; in a program the names
denote nothing and a package may declare its own. The four `std.dynamic`
functions are not exported: "Nothing in a program calls this. The lowering
does."

### Nothing yet asks for it

PHILOSOPHY's "Earn complexity through use" admits a feature when
"representative programs show recurring friction that simpler language or
library designs cannot solve". No program in this repository — covefmt, cq,
`examples/`, `benches/`, the e2e corpus — has asked to inspect a value
structurally. Every structural operation a program performs on an erased value
today is one of the four the language already defines, and those are served.
The candidate uses that come to mind — serialization, a debugger or pretty
printer, generic programming over "any struct" — are imaginable, and
imaginable is the standard PHILOSOPHY names as not enough.

So the question is not only whether Cove wants public reflection. It is also
whether the surface that exists is the one to publish, if it ever does. The
next section says why it is not.

## Decision

### 1. Structural reflection stays an internal std/core capability

`DynamicView`, the `core.dynamic*` observations, `RenderPath` and
`IdentitySet` remain reachable only from standard-library modules and from
code the lowering synthesizes. Nothing is added to `BUILTINS`, no name
becomes resolvable in a program, no `std.dynamic` function is exported, and
the checker's `cove::type::dynamic_view_escape` rules stay exactly as they are.

This is a decision about **now**, taken on evidence that can change. It is
not a finding that Cove must never have reflection.
[What would reopen this](#what-would-reopen-this) says what would change it.

### 2. What the internal capability continues to serve

The four operations ADR 0068 built, and nothing else:

- `Any.equals`, the `==`, `!=` and `assertEqual` of two boxed operands
  (`std.dynamic.equals`);
- the canonical key order (`std.dynamic.order`);
- key admission and its refusals, with ADR 0067's sentences
  (`std.dynamic.refusesKey`, `refuseKey`);
- rendering, including the `[…]` cycle marker and opaque text
  (`std.dynamic.renderInto`).

A new standard-library use may be written over the same capability without a
new ADR, provided it holds to Decision 3 below and is not itself a public
reflection API under another name: a `std` function that is exported and
hands a program a view, a kind code, a field name enumeration or a type
identity is publication, and needs the ADR that
[What would reopen this](#what-would-reopen-this) asks for.

### 3. What stays guaranteed

However the internal surface grows, Cove code — standard library included —
is never handed:

- **a raw address.** A view's owner is a `Ref` word the collector traces; no
  interior address is formed. `RenderPath`'s `Addr` and `IdentitySet`'s table
  of addresses are below the boundary and answer only `Bool` or `()`
  (`dynamicOnPath`, `dynamicSameObject`, `identityEnter`, `identityLeave`).
- **runtime layout.** No observation answers a `LayoutId`, a word offset, a
  tag bit or a `Shape`. `VIEW_LAYOUT` and `VIEW_AT` are fields of an `opaque`
  layout that no Cove source reads.
- **a handle number as a number.** A Host resource's number appears in Cove
  only inside the text `core.dynamicHandleText` answers — `<http.Server#1>` —
  which is the text the language already printed for that handle on a
  statically known path (`tests/e2e/values_render_opaque`), and which nothing
  parses. No observation answers it as an `Int`.

### 4. Publishing the current surface is rejected, and derive/protocol is the preferred future direction

The three alternatives follow in [Alternatives](#alternatives). (B) is chosen.
(A) is rejected. (C) is recorded as the direction a future proposal should
start from, and is **not designed here**: no syntax, no trait name, no
generated signature and no implementation is decided by this ADR.

## Alternatives

### (A) Publish the current reflection API — rejected

Make `DynamicView` and the `core.dynamic*` observations, or thin exported
`std` wrappers of them, available to programs.

The cost is small in code and large in commitment. Publishing does not expose
one capability; it exposes five things at once, each of which is today a
private choice the standard library and the machine can change together in one
pull request, and none of which ADR 0068 promised: "No private representation
chosen here is promised to that API."

**Rooting and lifetime rules.** A view roots its owner: `VIEW_OWNER` is a
`Ref` word, and the object stays reachable for exactly as long as a slot
holding the view is live. That is sound only because the checker keeps the
view where the walk that opened it can see it end. Today the rule
(`DYNAMIC_VIEW_ESCAPE_RULE`) admits a local, a `Vector` of views, and the
parameters and results of non-exported functions, and refuses an exported
signature, a struct field, an enum payload, a closure capture and a task.
`RenderPath` is narrower — it names *frames*, so it may not be a result at
all. `IdentitySet` is narrower still: `core.identityEnter` rewrites the local
that holds it, so it is held only in a local of the function that made it,
and is refused even as a parameter or a type argument. Those are three
different lifetime disciplines, each enforced by an ad hoc check written for
trusted callers whose code is reviewed with the checker. A public type would
need them stated as language rules — a new kind of non-escaping value in a
language that has none — with diagnostics written for program authors, and
could never relax them afterwards without an unsoundness.

**Private structure.** `core.dynamicChild` reads every field of a struct,
including an `opaque` struct's (ADR 0014). `core.dynamicFieldName`,
`dynamicTypeName` and `dynamicCaseName` answer the names of any module's
declarations. The kind table records why this is acceptable today:
"An `opaque` struct is row 6 and not row 14 ... a view is read only by the
standard library's own walks, which are trusted code, and what a walk shows of
one is the walk's decision — equality compares the fields, ... and a rendering
shows only the name." That reasoning depends on the reader being trusted.
Published, the same observations let any module enumerate and read another
module's private fields, which is exactly the first open question ADR 0068's
Decision 6 listed — "whether a program may enumerate private field names" —
answered *yes* by accident rather than by decision.

**Capability-bearing values.** Functions, Host handles, tasks, task scopes and
`Shared` cells are kinds 13, 14 and 15; they have no children and no scalar
to read (ADR 0068's Decision 7). But a view of one still exists, still roots the handle,
and still answers `dynamicHandleText` and `dynamicSameObject`. A public view
would be a way to carry a capability somewhere its static type was erased and
ask questions of it there — which PHILOSOPHY's "No ambient authority" and
ADR 0068's Decision 6 "how reflection interacts with capability security" both require
to be designed, not inherited. `Shared` shows the cost concretely: it was
moved from row 14 to its own row 15 in Phase 4b-ii only so a *rendering* could
print `<shared>`, a distinction a public API would have to keep meaningful for
every future reader.

**Runtime type identity.** The internal surface deliberately never reifies a
type: Cove asks `dynamicSameType` and `dynamicNameOrder` and gets a `Bool` or
an `Int` sign. Underneath is `VIEW_LAYOUT`, a `LayoutId` numbered per compiled
`Program` — not stable across compilations, not an address, not written to
trace or replay (ADR 0068's Decision 3). And the identity those two answer is shaped by
evaluator agreement, not by what a user would want: `declared_name` leaves
instantiation off, so `Option<Int>` and `Option<String>` are one type, because
the AST oracle's values carry no type arguments. A public reflection API would
have to publish a type identity. Publishing this one would make that
agreement artefact a language rule; publishing a different one would mean a
representation this ADR does not have. Either way it would be asked
immediately to be hashable, orderable and serializable, which ADR 0068's
Decision 3 forbids.

**Representation constraints.** Several facts the surface depends on are
facts about this implementation:

- *Kind codes.* `dynamicKind` answers an `Int` from `DynamicKind`'s sixteen
  rows. That table changed twice during ADR 0068 itself — the `opaque` struct
  moved from 14 to 6 in Phase 2, and `Shared` was appended as 15 in Phase 4b-ii
  — and `std.dynamic.admitted` tests `kind < 13` although the table says
  "nothing may compare two codes as numbers". That is harmless while the table
  and its only reader change in one commit, and a compatibility promise the
  moment a program reads a code.
- *The view's shape.* Three inline words, `Int`/`Ref`/`Int`. `dynamic.rs`
  records the intent that "a later representation — a handle into a per-run
  table — changes this module and the machine, and no Cove"; that is true only
  while "no Cove" means the standard library.
- *Layout-derived facts.* The child order — declaration order for fields and
  payloads, canonical order for a set, "twice a map's entries — key then
  value" — and which values are identity-bearing ("a whole `Vector`, and
  nothing else") are chosen to match the existing walks and the two
  evaluators, not designed as a public model.
- *Cycle and identity machinery.* `RenderPath` carries a frame address Cove
  passes along and cannot read; `IdentitySet` is a hash table of object addresses the
  collector does not trace, sound only because the walk holds a view of each
  vector until it leaves the pair. Both exist so that four walks find a cycle
  without scanning their path; neither is an abstraction a program should build on.

Consequences if chosen: every item above becomes a compatibility surface; the
escape rules become language; the questions ADR 0068's Decision 6 listed are
answered by the current implementation's accidents; and the next
representation change of the view, the kind table or the identity machinery
needs a deprecation rather than a commit. No program in the repository would use it.

### (B) Keep it internal — chosen

Consequences:

- **No program can inspect an erased value structurally** except through the
  four operations the language defines. A program that wants serialization or
  a structural dump writes it per type, statically, as it does today — cq's
  JSON handling is that shape, and no program has yet shown the shape to be
  recurring friction.
- **The representation stays free.** The kind table, the view's three words,
  the child order, the escape rules and the cycle machinery may change in one
  pull request that updates `std.dynamic` alongside, as they did throughout
  ADR 0068.
- **This ADR's Decision 3 holds by construction**, because the only readers
  are trusted and reviewed with the machine they read.
- **Performance class is unaffected.** Nothing is added; static layouts keep
  the synthesized path of ADR 0064's Decision 3, and the reflected path is the
  one ADR 0068 measured.
- **The cost** is that a real public use case, when one appears, starts from
  zero public surface rather than from an API it could have extended.
  [What would reopen this](#what-would-reopen-this) is how that start is kept
  short.

### (C) A limited derive/protocol mechanism — preferred future direction, deferred

Instead of a view of *any* value, a type **opts in** to a structural protocol,
and the compiler generates its implementation from the declaration — the way
ADR 0064's Decision 3 already synthesizes `==`, order and rendering for a
known layout, but user-visible and per protocol.

Why it is the preferred direction:

- **It is static.** It rides the fast path ADR 0068's Decision 5 protects
  rather than the erased fallback, and needs no run-time type identity.
- **Privacy is the declaring module's.** A type that does not opt in exposes
  nothing; an `opaque` type decides what its protocol shows; a capability
  type simply has no implementation.
- **It exposes no representation.** No kind codes, no view, no rooting
  discipline: the generated code is ordinary Cove over ordinary values.
- **It is narrower.** A protocol answers one question — "serialize this",
  "describe this for a debugger" — rather than every question.

Why it is not designed here:

- There is no representative program to design it against, and PHILOSOPHY's
  "Syntax must earn its place" applies squarely: a `derive` is syntax.
- The shape depends on the use case. Serialization needs a data model and a
  choice about field names and versioning; a debugger needs an opaque-aware
  description; generic programming may need neither. Designing one mechanism
  for three unmet needs would be designing for the imagined one.

Consequences if later chosen: a new ADR decides the syntax and its first
protocol; the internal view may be one way its fallback for erased values is
implemented, and nothing of the view's representation is promised to it.

### Other options considered

**Publish a read-only subset** — kinds, scalars and child counts, without
field names or opaque structs. Rejected for now: it still publishes the kind
codes, the view's lifetime discipline and a type identity, which are the
expensive parts, while leaving out the names every real use (serialization,
debugging) would ask for next.

**Publish only a `describe(value: Any) -> String`**, a stable textual dump.
Deferred rather than rejected: rendering already does this for erased values
through interpolation, and a separate stable format is a serialization
decision that belongs to (C)'s first use case.

**Publish reflection as a Host capability**, granted per embedding. Deferred:
it moves the trust question to the Host without answering the representation
questions, and no embedder has asked.

## What would reopen this

A new ADR — not an amendment to this one — may revisit the decision when all
of the following are true:

1. **A concrete public use case** is named: serialization, a debugger or
   inspector, or generic programming over user types, or another of the same
   weight.
2. **Representative programs show the friction**, per "Earn complexity through
   use": real programs in or beside this repository whose code today repeats
   per-type structural boilerplate, or cannot be written, *recurring* across
   more than one program — not a single example written to motivate the
   feature.
3. **The proposal starts from (C)** and says why a derive/protocol does not
   suffice before proposing any public view of arbitrary values.
4. **It answers ADR 0068's Decision 6 questions** for the surface it proposes:
   private field names, attributes and documentation, type identity across
   package and process boundaries, mutation, and capability security.
5. **It keeps this ADR's Decision 3** and names what of the internal
   representation, if anything, it depends on — expecting none of it to be
   promised.

Until then, a request for public reflection is answered by this ADR.
