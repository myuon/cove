# ADR 0059: A keyed collection is searched by order, not hashed

- Status: Accepted
- Date: 2026-09-15
- Decides: which layout-directed intrinsics `Map` and `Set` keep in the
  runtime when their public algorithms move to Cove source, and what moves
- Supersedes:
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  **Phase 4 intrinsic** — "`Map` and `Set` retain layout-directed
  `value-hash` and `value-equal` intrinsics", the "probing, growth policy"
  that sentence moves over them, and the division-table row "probing and
  table policy | layout-directed value hash/equality". Nothing else in
  ADR 0058 changes: its run operations, typed intrinsic boundary, blame and
  migration order stand
- Changes no source-language API and no representation

## Context

ADR 0058's Phase 4 was written as if `Map` and `Set` were hash tables. They
are not, and never have been.

- The layouts are sorted packed runs. `Shape::Members` is "the header's `len`
  members, ascending and distinct" and `Shape::Entries` is "the header's `len`
  entries — key then value — ascending by key"
  (`crates/cove-ir/src/layout.rs`).
- Every lookup is a binary search. `seek` in
  `crates/cove-runtime/src/vm/builtins/keyed.rs` answers `Ok(at)` or
  `Err(at)` as `slice::binary_search` does, over a fallible comparison.
- The comparison is `key::order` in
  `crates/cove-runtime/src/vm/builtins/key.rs`: a runtime rule over the
  value's structure — family rank first (`Unit` < `Bool` < `Int` <
  `Duration` < `Str` < enum < struct < `Array` < `Set` < `Map` < `Range`),
  then contents, bounded by `MAX_DEPTH`. `key::admits` is the matching rule
  that refuses a `Vector`, anything holding one, and a `Float`.
- The oracle is the same shape: `Repr::Map(Rc<BTreeMap<MapKey, Value>>)` and
  `Repr::Set(Rc<BTreeSet<MapKey>>)` in `crates/cove-runtime/src/value.rs`,
  ordered by `MapKey`'s derived `Ord`.
- `inserted` and `removed` never write through the receiver. Each searches,
  allocates a new run of `n + 1`, `n - 1` or `n` elements, and copies around
  the insertion or removal point.
- Nothing hashes a Cove value. The `HashMap`s and `HashSet`s in the tree are
  keyed by compiler tables and heap bookkeeping, not by program values.

The order is not an implementation detail. `LANGUAGE_REFERENCE.md` says a
`for` iterates "ascending key order for a `Map`, sorted order for a `Set`";
`LINEAR_VM.md` says both "iterate in ascending order and render that way";
and `LANGUAGE_CARD.md` says their ascending order "is storage rather than an
order a caller chose".

So the intrinsic ADR 0058 told Phase 4 to retain does not exist, and the one
Phase 4 needs is not named. The Phase 4 plan on issue #378 found this before
any keyed code moved.

[ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md)'s
reason for keeping `Map` and `Set` primitive — "Cove has no way to hash an
arbitrary `K`" — carries over unchanged with the word replaced: Cove has no way
to *order* an arbitrary `K`, because `key::order` reads raw heap words by
layout. That is why order stays an intrinsic below, and why nothing here
exposes a generic ordering to user programs.

## Decision

### The runtime keeps order, equality and admission

`Map` and `Set` retain three layout-directed atomic intrinsics:

| Intrinsic | Meaning |
| --- | --- |
| `value-order` | three-way comparison of two values of one layout under `key::order`'s total order |
| `value-equal` | equality of two values of one layout — what `==` and `Any.equals` already are |
| key admission | refusal of a value that cannot be a key, with the refusal naming the method and the offending part |

Each is reached as ADR 0058 prescribes: a static intrinsic identity, a fixed
operand shape, declared effects and the caller's blame. A duplicate key in a
literal is *found* in Cove, as a `value-order` answer of equal; the refusal
it raises keeps today's wording and is raised through a typed intrinsic, as
admission's is.

### The algorithm moves to Cove

Standard-library Cove implements, over `value-order`, element loads and a
keyed run finish:

- binary search, and the found/not-found decision it answers;
- `inserted` (including replacing an existing key's value) and `removed`,
  built as a new sorted run;
- duplicate detection when a literal is built;
- `contains`, `get`, `toArray`, `keys` and `values`.

The two ADR 0058 table cells this replaces now read:

| Standard-library Cove | Core intrinsic/runtime |
| --- | --- |
| search and keyed construction policy | layout-directed value order/equality and key admission |

### The representation is kept

`Map` and `Set` stay sorted packed runs with ascending iteration and
rendering. A run is sorted because it was built in order, as it is today; a
keyed finish does not sort.

## Alternatives considered

### Make `Map` and `Set` hash tables

This is what ADR 0058's wording assumed. It would lose the order the language
promises, so every `for`, rendering, `keys()`, `values()`, `toArray()` and
boundary materialisation would need either a sort — O(n log n) where it is
O(n) now — or a second ordered index maintained beside the table. That is a
change of performance class for iteration, or a change of language semantics
if the sort is dropped instead. Rejected; changing it needs an ADR that
supersedes the iteration order as well.

### A persistent tree or a keyed builder

Building a map by repeated `inserted` copies `0 + 1 + … + (n - 1)` entries
and allocates `n` runs — O(n²) for n keys. cq's JSON parser builds every
object this way (`examples/cq/json/json.cove`, `fields.inserted(name,
value)` per field), and #378's Phase 4 plan estimates about 900k `inserted`
calls on its JSON Lines workload. A persistent balanced tree would make each
insert O(log n); a builder that accepts unordered entries and sorts once at
finish would make construction O(n log n). Either changes the representation
or adds surface, so neither is decided here. The quadratic construction is a
known cost of the kept representation, and a proposal to remove it gets its
own ADR with its own workload.

### Keep `value-hash` in the wording and add `value-order` beside it

An unused intrinsic in an accepted decision is a standing instruction to
implement it. Nothing would call it.

## Consequences

- Phase 4 of ADR 0058 proceeds over `value-order`, `value-equal` and key
  admission; `value-hash` is never added.
- The cost of a keyed operation moving to Cove is one `value-order` per
  binary-search step, O(log n) of them, instead of one Rust call for the whole
  search. The gates of ADR 0058 and the Phase 4 plan apply to that trade.
- The O(n²) cost of building a map by repeated `inserted` remains, on the VM,
  in native code and in the oracle, whose `inserted` clones its `BTreeMap`.
- ADR 0042's expressibility argument for keyed collections is unaffected in
  substance: order, like hashing would have been, stays below the language.
- ADR 0058's closing bullet that runtime work remains in Rust "where it needs
  … hashing" is descriptive of the superseded row; for `Map` and `Set` the
  work that remains is ordering.
