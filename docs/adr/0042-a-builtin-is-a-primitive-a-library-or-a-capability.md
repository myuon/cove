# ADR 0042: A builtin is a primitive, a library, or a capability

- Status: Accepted
- Date: 2026-09-06
- Decides: which of the 18 builtin types' 97 methods, 13 associated functions
  and 7 free builtins stay in the runtime, which become Cove source in a
  standard library, and which are capabilities — and the one test that sorts
  them
- Supersedes nothing. [ADR 0001](0001-mvp-language-design.md) chose the
  builtin surface and [ADR 0017](0017-embedder-host-api-schemas.md) drew the
  capability boundary; neither is contradicted here. This ADR sorts what those
  decisions left in one undifferentiated pile
- Implementation status: the mechanism exists and carries exactly one method.
  `Array.isEmpty` is Cove source in `crates/cove-sema/std/array.cove`, bound by
  `cove_schema::builtins::STANDARD_LIBRARY`, resolved generically by lowering
  and by the tree-walking interpreter, and shipped in
  [PR #253](https://github.com/myuon/cove/pull/253). Nothing else has moved.
  The generated reference `docs/BUILTINS.md` carries an "implemented by"
  column, so the state of this migration is a fact about the tree rather than
  a claim in a document

## Context

Cove has one word for three different things.

A builtin is anything the checker reads out of `cove-schema` and either
evaluator answers in Rust. `Array.length()` is a builtin. So is
`Shared(x).lock()`, which takes an inline heap word with Acquire/Release
atomics in the VM and an `Arc` with a `Transfer` in the interpreter. So is
`Option.isSome()`, which is a `match` a Cove program could write in one line
and which exists twice in Rust because nothing ever asked whether it had to.

That undifferentiated pile has a cost that is not aesthetic. **Every builtin
is implemented twice** — once in `crates/cove-runtime/src/builtins.rs` for the
tree-walking oracle and once in `crates/cove-runtime/src/vm/builtins/` for the
linear-memory machine — and the two share no implementation code. What holds
them together is the differential corpus in
`crates/cove-cli/tests/vm_coverage.rs`, which runs every program in the
repository on both and compares. That corpus is load-bearing and it has caught
real divergence. It also cannot catch the case where both evaluators are
independently right, which is the common case for the easy methods, so for
those the second implementation buys nothing and costs a place to be wrong
later.

The question is which ones those are, and until now the project had no answer
that was not a guess. It has evidence now, from two places.

**`myuon/the-cove`.** A real application — 962 lines of Cove deciding creature
behaviour inside a Rust world — has been built and finished. What it used is a
fact rather than a prediction: **seven builtins**, in total, across the whole
program. `Some` (10 calls), `Array.filter` (6), `Array.fold` (3), `Float.max`
(2), `Float.min` (2), `Array.length` (1), `Array.get` (1). Zero `Vector`, zero
`Map`, zero `Set`, zero `Task`, zero `Shared`, zero `Scope`. Zero loops. The
one thing it wanted and did not have was `sqrt`: `instinct.compass()` writes
`0.7071067811865476` out four times, and every normalisation, distance and
sort in the reef is owned by the Rust host because the language could not do
it. That gap is [issue #250](https://github.com/myuon/cove/issues/250) and is
not this ADR's business.

**`cove reference`.** The generated inventory made the surface countable for
the first time, and counting it produced the finding this ADR is mostly about:
**several builtins are already not runtime functions.** `Option` and `Result`
methods, `Range` methods, and `Array.map`/`filter`/`fold` are lowered to
instructions or to walks and never reach a builtin arm at all. `Error` and
`MapEntry` are types with fields and no methods whatsoever. The boundary is
already further toward "library" than the schema's flat shape suggests; what
was missing was a way to say so.

## Decision

A builtin is exactly one of three things, and the test is stated so that a
future addition can be sorted without re-arguing this ADR.

### Primitive

It stays in the runtime, implemented once per evaluator, **if and only if it
must know something the language cannot say**: the machine's representation of
a value, its memory management, or its scheduling.

Concretely, an operation is a primitive when it reads or writes heap words
directly, allocates, changes an object's header, participates in garbage
collection, crosses threads, or converts between two representations. The test
is *expressibility*, not speed.

**Performance alone does not make a primitive.** Following
[PHILOSOPHY.md](../PHILOSOPHY.md)'s "Preserve the performance class", a
measured change of *class* — O(1) to O(n), or an order of magnitude — is a
reason to reconsider a classification; a measured constant-factor slowdown is
not, and an *unmeasured* belief about speed is not a reason for anything. This
matters most for `Map` and `Set`, which are primitive in this ADR for a reason
that is not their speed: **Cove has no way to hash an arbitrary `K`.** Hashing
reads the value's raw heap words through the same machinery as `equal` and
`key`, which the language does not expose and this ADR does not propose
exposing. If it ever does, `Map` and `Set` come back to this table on the
expressibility argument, not on a benchmark.

### Library

It becomes Cove source under `crates/cove-sema/std/`, bound by
`cove_schema::builtins::STANDARD_LIBRARY`, **if Cove can already say it** using
primitives and the ordinary language — arithmetic, comparison, `match`,
closures, calls.

The user-facing syntax does not change. `items.isEmpty()` type-checks from the
same schema entry it always did; the schema now also says where the body
lives, and lowering resolves that generically. A method does not become a free
function, a program does not gain an import, and the reference documents it in
the same place as before with one extra column saying who implements it.

Being expressible is necessary and not sufficient. A method also has to be
*natural* in Cove — a Cove body that reimplements a machine detail in slow
arithmetic is not an improvement, it is the same coupling with worse
performance and a longer diff.

### Capability

It reaches outside the process — I/O, resources, the clock, the network — and
stays at the host boundary, where it already is.

**Nothing changes here, and the evidence says nothing should.** The boundary is
already sharp in the one way that matters: `CallHost` and `CallResource`
materialise a `Value` and cross `HostRegistry`; `CallBuiltin` does not import
`Value` at all. The 8 shipped modules and 25 operations are outside this
migration entirely, and this ADR mentions them only to record that the
three-way split has a third arm that needed no work.

## The classification

Read against `docs/BUILTINS.md` at this ADR's date. **P** primitive, **L**
library, **?** blocked on a question named below the table.

| Type | Primitive | Library | Blocked |
| --- | --- | --- | --- |
| `Array<T>` | `get`, `length`, `toVector` | `isEmpty`, `contains`, `indexOf` | `slice`, `sorted`, `map`, `filter`, `fold` |
| `Vector<T>` | `get`, `length`, `push`, `set`, `pop`, `remove`, `freeze`, `toArray`, `of` | `isEmpty`, `contains`, `indexOf` | `slice`, `sorted`, `map`, `filter`, `fold` |
| `Map<K, V>` | `get`, `length`, `contains`, `keys`, `values`, `inserted`, `removed`, `of` | `isEmpty` | |
| `Set<T>` | `length`, `contains`, `inserted`, `removed`, `toArray`, `of` | `isEmpty` | |
| `String` | `length`, `chars`, `slice`, `indexOf`, `split`, `trim`, `replace`, `toUpper`, `toLower`, `fromCodePoint` | `isEmpty` | `words`, `join`, `contains`, `startsWith`, `endsWith` |
| `Range` | | | `length`, `isEmpty`, `contains` |
| `Option<T>` | | `isSome`, `isNone`, `unwrapOr` | |
| `Result<T, E>` | | `isOk`, `isError`, `unwrapOr`, `mapError` | |
| `Int` | `toFloat`, `parse`, `parseRadix` | `abs`, `min`, `max` | |
| `Float` | `toInt`, `round`, `format`, `parse` | | `abs`, `min`, `max` |
| `Duration` | `nanos` (the reader) | `micros`, `millis`, `seconds`, `minutes`, `hours`, and all six builders | |
| `Task<T>` | `await`, `cancel` | | |
| `Shared<T>` | `lock` | | |
| `Scope` | `spawn` | | |
| `Bool`, `Unit` | | | see below |
| `Error`, `MapEntry` | — fields only, nothing to sort — | | |
| free | `Ok`, `Err`, `Some`, `Error`, `Shared` | `assert` | `assertEqual` |
| host × 25 | — capability, unchanged — | | |

`snapshot()` appears on eleven types and in none of these rows. It is dead
surface: only `Array`, `Vector` and `Int` are ever called, in either corpus.
Deleting the other eight is [issue #249](https://github.com/myuon/cove/issues/249)
and is a separate decision from this one, because deleting a method is a
breaking change and moving one is not.

### What "blocked" means, case by case

**`map`, `filter`, `fold` (Array, Vector).** Expressible — Cove has
first-class closures, and the reef's `instinct.nearest` is a `fold` over a
closure written in Cove today. But they are currently lowered to *walk
instructions*, not to builtin arms, so moving them is not "delete a Rust arm";
it is replacing an instruction with a loop, and whether that crosses a
performance class is exactly the question ADR-level reasoning cannot answer.
**Measure before moving.** These are the most-used builtins in the one real
application, which raises the cost of getting it wrong and is a reason for
care rather than a reason for haste.

**`slice`, `sorted`.** Writable in Cove given a `Vector` to build into, at the
same complexity. Left blocked because the natural Cove body allocates a
`Vector` and freezes it, and the freeze is a primitive whose cost in this
position has not been looked at.

**`Range.length`, `isEmpty`, `contains`.** Pure arithmetic over start, end and
step — and Cove cannot read a `Range`'s start, end or step, because `Range` is
a builtin type with no fields. The block is field access, not arithmetic.

**`Float.abs`, `min`, `max`.** `f64::max` propagates the non-NaN operand; a
Cove body written as `if a > b { a } else { b }` answers differently when one
side is NaN. **That is a language-semantics decision and not a refactor**, so
these do not move until someone decides which answer Cove gives. `Int.min` and
`Int.max` have no such question and are library.

**`String.contains`, `startsWith`, `endsWith`, `words`, `join`.** Each is
expressible over a smaller string primitive, and which primitive is the right
one — `chars`, byte access, a slice comparison — is an open design question
that this ADR does not settle. The rows marked P for `String` are a working
set, not a claim that all ten are irreducible.

**`assertEqual`.** `assert` is `if condition { Ok(()) } else { Err(...) }` and
moves. `assertEqual` needs `==` on a generic `T`, which goes through the same
raw-heap-word `equal` machinery as `Map`'s hashing. It moves if and only if
generic equality is something Cove can say, which is the same open question.

**`Bool.snapshot`, `Unit.snapshot`.** Their only methods are `snapshot`, so
issue #249 decides them entirely and this ADR does not.

### Why `Duration` is the row to read twice

`Duration` has thirteen entries: six associated builders and seven readers.
Given one primitive — the reader `nanos()` — every other one is a
multiplication or a division by a constant, written in Cove in a line each.
**One primitive in, twelve methods out**, no new semantics, no representation
knowledge, and two of the readers (`micros`, `minutes`) are never called
anywhere in either corpus. It is the largest single reduction available and
the least interesting to argue about, which is why it is named here rather
than left to be discovered.

## What this does not decide

- **The order of migration.** That is the issue this ADR is paired with, and
  it is a scheduling question, not a design one.
- **Whether the standard library is precompiled.** `cove_sema::stdlib::attach`
  is the boundary where that will be answered — it parses today and will
  answer a cached artifact later without any caller changing — but nothing
  here decides when, and nothing here should be read as a promise that the
  current parse-every-time cost is acceptable forever. It is acceptable for
  one function.
- **New builtins.** `sqrt` and trigonometry are the one gap a real application
  actually hit, and they are issue #250. Adding them is governed by "Earn
  complexity through use", which they have.
- **Deleting `snapshot`.** Issue #249.
- **Exposing hashing, generic equality, or a `Range`'s fields.** Each would
  change this table. Each is a language change and gets its own ADR.

## Consequences

**A method that moves stops being implemented twice.** That is the whole
benefit and it is worth stating plainly, because the differential corpus makes
it easy to believe two implementations are safe. They are safe against
divergence and they are not safe against a bug both evaluators share, and they
cost a second edit forever.

**A method that moves becomes visible to a Cove reader.** `std/array.cove`
says `items.length() == 0` in as many words. The Rust arms it replaced said
the same thing in two places and neither was somewhere a Cove programmer would
look.

**The standard library is ordinary Cove, with no privileges.** It is checked,
lowered, executed, stepped in the debugger and traced exactly like a program's
own modules — `cove debug` steps into `std.array.isEmpty<Int> at
std/array.cove:9:3` and back out. This ADR forbids a standard-library-only
semantics: if the library needs something the language cannot express, the
answer is that the method stays primitive, not that the library gets a
privilege.

**A migrated method costs a call.** A `CallBuiltin` becomes a `Call` to a
declared generic function, which is a frame. For `isEmpty` that is measured at
nothing worth reporting and it is not nothing in general; the blocked rows
above are blocked partly for this reason. Where it matters it is to be
measured, and where it is measured to change the performance class the method
stays where it is — which is the classification test doing its job rather than
an exception to it.

**Nothing that was never used gets built.** The reef used seven builtins and
that number is the strongest evidence in this document. It is a reason to move
carefully and it is emphatically not a reason to delete the other ninety: a
corpus of one application says what one application needed, and
[PHILOSOPHY.md](../PHILOSOPHY.md)'s "Earn complexity through use" governs
*addition*, not removal.

## Evidence

- The inventory: `docs/BUILTINS.md` and `docs/builtins.json`, generated by
  `cove reference` from `cove-schema` and checked in CI. 18 types, 97 methods,
  13 associated functions, 7 free builtins, 8 host modules, 25 host operations.
- The application: `myuon/the-cove`, 962 lines of Cove, seven builtins used.
- The mechanism: [PR #253](https://github.com/myuon/cove/pull/253), one method
  moved end to end through checker, lowering, both evaluators, native and Wasm,
  the debugger, the generated reference and the conformance ratchet.
- The divergence survey: of 18 builtin types, exactly one — `Task`/`Scope` —
  behaves differently on wasm32, and only as a hard refusal to spawn. Nothing
  else in the builtin surface diverges by target, which is why this
  classification does not need a per-target column.
