# ADR 0064: An intrinsic names a machine, not a method

- Status: Proposed
- Superseded in part by
  [ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md), which adds a
  sixth entry — stopping a run with a sentence the caller built — to Decision
  2's acceptable vocabulary, leaving Decision 2's test as it stands
- Date: 2026-09-19
- Decides: that `Inst::IntrinsicCall` is a migration mechanism whose
  population may only shrink; the test a surviving IR primitive has to pass;
  how the five layout-directed operations are specialized at lowering time
  and where the one dynamic fallback is; who owns the Unicode version and the
  IEEE-754, parsing and formatting behaviour once the Rust standard library no
  longer does; what the reporting has to reconcile; and the gates a migration
  is measured against. Phase 0 of
  [issue #432](https://github.com/myuon/cove/issues/432)
- Supersedes:
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  **"Unicode case conversion, parsing and formatting may remain direct
  intrinsics; their public wrappers still live in the standard library where
  there is policy or composition to express."** They may not remain. That
  sentence was written where ADR 0058 was looking — at collections, whose
  migration it was deciding — and it exempted the text and scalar families
  from the rule it had just proved for `Array`, `Vector`, `Map` and `Set`
  rather than deciding anything about them. This ADR withdraws the exemption
  and applies the same rule to all 31 variants. ADR 0058's header gains its
  `Superseded in part by` pointer when this ADR is accepted, not while it is
  proposed — the order ADR 0061 follows
- Refers to, without superseding:
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  dispatch decision — "lowering resolves it to an intrinsic identifier with a
  fixed operand and result shape", and the VM matching on the variant rather
  than on a pair of strings — which stays exactly as it is for as long as any
  variant survives. ADR 0058 said of its predecessor mechanism that it
  "remains during migration, but it is not a target architecture"; this ADR
  says the same of ADR 0058's own, which ADR 0058 never claimed otherwise.
  Deleting the mechanism will contradict ADR 0058, and needs its own
  superseding ADR at the time it actually happens rather than in advance;
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s bounds and cancellation,
  which every migrated loop inherits;
  [ADR 0059](0059-a-keyed-collection-is-searched-by-order-not-hashed.md)'s
  `core.order`, whose existing lowering is the precedent Decision 3
  generalizes; and the proposed
  ADR 0061 (`docs/adr-0061-display`, unmerged), which decides what `"{x}"`
  renders and is the source-language half of `ValueRenderInto`'s migration
- Changes no source-language API

## Context

`Intrinsic` has 31 variants. Every one of them is named after a public method
— `String.split`, `Float.format`, `Value.renderInto` — and every one of them
is a whole algorithm in Rust reached by one IR instruction. The layering is

```text
public API -> IntrinsicCall(public-operation identifier) -> Rust algorithm
```

and the standard library is not in it. `String.length` has no Cove body at
all: `crates/cove-schema/src/builtins.rs` declares the method, lowering
resolves the `(receiver, operation)` pair to a variant, and
`crates/cove-runtime/src/vm/intrinsics/text.rs:42` is `operand::text(...)?
.chars().count()`. Fifteen of the sixteen text variants are that shape.

ADR 0058 proved the alternative on collections. `Vector.push`, `Map.inserted`,
`Set.contains`, `StringBuilder.append` and `String.sliceBytes` are Cove
functions over a small typed substrate — `RunLoad`, `RunSlice`, `RunStore`,
`RunCopy`, `GrowableAlloc/Ensure/Commit`, `RunFinish` over `Storage::PackedBytes`
and `Storage::Words` — and the runtime below them names storage rather than
methods. `std.string.sliceBytes` is the shape the rest of this ADR is about:
five range questions, a refusal worded in Cove, and one `core.stringSlice`
that validates nothing because the Cove above it already did.

What ADR 0058 did not do is finish. It exempted "Unicode case conversion,
parsing and formatting" explicitly, and it left the text predicates, the text
constructors and the five layout-directed walks where they were because
collections were what it was deciding. Three things have changed since, and
together they turn that leftover into the thing worth doing next.

### The intrinsic boundary is a floor the native tier cannot lower

ADR 0055's native tier compiles 208 of covefmt's 214 reachable functions and
89 of cq's 113. It does not compile a single intrinsic: both code generators
cross back into the runtime through one generic helper for every variant
(`crates/cove-native/src/compile.rs:1133`,
`crates/cove-native/src/template.rs:876`), including `Float.sqrt` and
`Float.abs`, which are one machine instruction each.

On covefmt that is **413,750 of 415,811** mediated intrinsic calls made from
compiled code, and `String.length` alone is 405,588 of them. Measured below:
an intrinsic call costs the same whether its caller is interpreted or
compiled — 96.0 ns against 89.1 ns — while Cove code doing the same work gets
**4.8x faster** when compiled. Every operation left on the far side of this
boundary is work the native tier is structurally unable to speed up, and its
share of a compiled program grows with every function the tier learns.

### The refusals are the language's, and they are written in Rust

`String.split` refuses an empty separator; `Int.parseRadix` refuses a radix
outside `2..=36`; `Float.format` refuses a digit count past 17;
`String.refuseByteRange` exists for no other purpose than to word a refusal —
and `std.string.refuseRange` words the *same five questions* in Cove, for the
same API, kept in agreement with `text.rs`'s copy by nothing but care. A
diagnostic is the most source-language-shaped thing a language has, and these
are compiled into the runtime.

### Nothing in the repository says which Unicode this is

There is no `rust-toolchain.toml`. `String.toUpper` is `str::to_uppercase`
and `String.trim` is `str::trim`, whose Unicode tables ship with the Rust
compiler and are revised with it. So `String.toUpper`'s answer on a character
whose case mapping a Unicode revision changed is a fact about which rustc
built the binary, and two builds of Cove from the same commit may disagree
about it — with nothing in the repository able to say which is right, because
it never wrote the answer down. That is not a defect the migration
introduces; it is one the migration is the occasion to fix.

## Decision

### 1. `IntrinsicCall` is a migration mechanism, and its population only falls

ADR 0058's identifier stays. What this ADR fixes is that the set it indexes
is closed and monotonically shrinking: **`Intrinsic` may lose variants and
may never gain one.**

A ratchet test compares the variant set — as a *set*, not as a count, for the
reason `crates/cove-cli/tests/vm_coverage.rs` compares its known
disagreements as a set: a count cannot tell a variant that left from one that
arrived, so a change that migrates `String.split` and adds `Text.replace`
leaves the number falling and the architecture exactly where it was. A new
operation that wants to be an `IntrinsicCall` is a design error, and the test
names it as one.

The mechanism is deleted when no variant is left, which is a later ADR's to
decide, at the time it happens.

### 2. A primitive may not fail for a reason the program wrote down

This is the test a surviving low-level operation has to pass, and it is
sharper than "is it representation-dependent", which every one of the 31
variants can claim.

> **A primitive's only failures are ones the program did not write: an
> exhausted heap, a bound, a cancellation. Every refusal that quotes a method
> name, a parameter name, a value or a range is the standard library's, and is
> constructed in Cove.**

An operation whose *purpose* is a diagnostic — `StringRefuseByteRange`,
`ValueRefuseDuplicate` — fails the test by construction. An operation whose
policy is a branch on its argument — `split`'s empty separator, `parseRadix`'s
`2..=36`, `format`'s 17 digits — fails it at that branch. In
`Intrinsic::effects` terms: `MAY_RAISE` for a *language-level* refusal must be
empty for every surviving primitive. A depth bound or a heap exhaustion is not
one; ADR 0040's bounds are resource facts and stay below.

A primitive must also be nameable without naming an API. The acceptable
vocabulary is:

- a typed scalar operation that maps to a CPU or backend operation
  (`sqrt`, `abs`, `round`, a checked typed conversion);
- a load or store of one unit at a named offset in a run of a named storage;
- a bounded proportional run operation — search, compare, slice, copy — whose
  work is charged and which is cancellable under ADR 0040;
- allocation, ensure, commit and finish of a typed run;
- inspection of a dynamic value's runtime layout, where Decision 4 says static
  specialization is impossible.

`TextReplace` is `StringReplace` renamed and is refused. The discriminator is
not the spelling: it is whether the operation would have to be renamed if the
standard library renamed a method.

### 3. A layout-directed operation is specialized where its layout is known

`core.order` already does this. `Body::core_order`
(`crates/cove-ir/src/lower/core.rs:1075`) emits one `Inst::Cmp` of
`CmpOp::Order` where `Body::ordered_by` finds a comparison instruction that
orders the layout exactly as the walk would, and falls back to
`Intrinsic::ValueOrder` only where none does. cq's 20,000-record run reaches
`std.map.seekMap` 180,000 times and executes `ValueOrder` **zero** times, at
zero sites: its `String` keys take the instruction.

That rule generalizes, and Decision 3 is that it must. For
`AnyEquals`, `ValueOrder`, `ValueAdmitKey`, `ValueRenderInto` and
`ValueRefuseDuplicate`, **lowering synthesizes one private function per
(operation, layout) actually reached, composed structurally**: scalars use
typed compare and render operations; structs take their fields in declaration
order; enums compare case and then payload; runs loop over the element
operation; keyed collections use their declared canonical order; recursion
keeps ADR 0040's depth and work bounds.

The synthesized function is ordinary function and control-flow IR. It is
visible to the printer, the verifier, the optimizer and both code generators
as a function, and **no backend may recognize it by name.** A backend that
matches on a standard-library function name has reinvented the string dispatch
ADR 0058 deleted.

Lowering synthesizes it rather than Cove declaring it, and that is a fact
about the language rather than a preference: Cove has no structural access.
Field access is by static name, `match` names a case, and a `dyn Trait` gives
polymorphism over behaviour and not over structure — so `equal<T>` cannot be
*written* for an arbitrary `T` today. `Body::instantiation`
(`crates/cove-ir/src/lower/dispatch.rs:614`) already produces one function per
`(declaration, type arguments)` pair; what Decision 3 adds is a producer that
walks a `Shape` instead of substituting into a written body. This ADR does not
make a public trait system a prerequisite for that, and does not decide the
syntax of one.

### 4. There is exactly one dynamic-layout boundary, and it is `Shape::Boxed`

`dyn Trait` and a Host schema's `Any` erase to `Shape::Boxed`, whose payload
word 0 is a `LayoutId` read at run time (`crates/cove-ir/src/layout.rs:290`).
That layout is genuinely unknown until the box is opened, so Decision 3 cannot
reach it and a fallback is required rather than tolerated.

It is *one* fallback, reached only from `Shape::Boxed`. A site whose operand
layout is statically known may not use it for convenience. The static and
dynamic counts are reported separately (Decision 7), a structural test asserts
that no statically-known-layout site reaches the fallback, and the intended
count on an ordinary program is zero. covefmt's and cq's are zero today, for
the uninteresting reason that neither executes any of the five.

### 5. Cove owns its Unicode version, and writes it down

One Unicode version is fixed per language release and named in one place in
the repository. Tables are generated **during repository development** by a
checked-in generator, committed as a reproducible asset, and a test asserts
that regenerating produces the same bytes. Nothing is built at program
startup.

Decoding, lookup and encoding are Cove over the run substrate, and one-to-many
mappings are supported wherever the present semantics have them. If a
primitive survives here it exposes a general read-only table capability — a
bounded load from a static run — and never Unicode policy. "Uppercase one
character" is a method, not a machine, and Decision 2 refuses it.

Where the migrated tables and the rustc version in use disagree, **the
committed tables are right**, and the differential corpus records the
disagreement rather than hiding it.

### 6. Parse and format are pinned by a corpus written before they move

`Float.format` and `Float.parse` are Rust's Grisu3-with-Dragon4 and `dec2flt`
today. Their observable behaviour is part of Cove's API whether or not anyone
decided it, so it is pinned before it moves, not after:

- a differential corpus is landed **before** the first line of the
  reimplementation, covering finite boundary values, subnormals, infinities,
  NaN, both zeros, halfway-rounding cases, exponent extremes, invalid syntax,
  every supported precision argument, and round-trip;
- **a formatting change is not accepted because the output parses back to the
  same number.** The bytes are the contract;
- `Float.min`/`max` NaN and signed-zero behaviour, `Float.round`'s
  half-away-from-zero tie rule, and the parsers' sign, whitespace and overflow
  policy are each pinned by a test before the arm that implements them is
  touched.

`Float.sqrt` need not be a Newton iteration in Cove. A typed scalar IR
operation that maps to `sqrtsd` passes Decision 2: it names machine semantics
and would not be renamed if `Float.sqrt` were.

`Inst::Convert::FloatToInt` already exists, verified, encoded, printed and
executed, and no lowering emits it — and its bare `as` cast disagrees with the
intrinsic's checked `Result`. Phase 3 either gives it the checked semantics or
deletes it; leaving a second, wrong answer in the IR is not an option.

### 7. Reporting reconciles exactly, and costs nothing when off

Today's `--boundary` already reports static `IntrinsicCall` sites, dynamic
calls per variant, and the split between the encoded and native tiers, and
`--profile --profile-rows all` already reports per-site counts that **sum
exactly** to the boundary total — verified below on both programs. What is
missing, and what Phase 0's reporting work is:

- **allocations and allocated words per variant.** The `--profile` opcode
  table carries them, but only for opcodes that ran `OPCODE_FLOOR = 1,000`
  times or more (`crates/cove-cli/src/main.rs:2089`). That floor is right for
  a *time* column — an average of three intervals is three readings of a clock
  — and wrong for a count. Allocations and words go on the boundary report's
  mediated-intrinsics table, where the floor does not apply. The floor is not
  weakened.
- **allocations and allocated words per site**, for the same reason.
- **proportional-work charges per variant**, which nothing attributes today:
  eighteen of the 31 carry `BULK_WORK`, and what that work costs is invisible
  inside the fuel total.
- **machine-code bytes attributable to intrinsic calls**, beside the window
  bytes ADR 0063 already reports.

The totals must reconcile exactly with the opcode and site profile, and
reporting must be free when disabled — one `Option` test on a path that is
already a Rust call, as `crates/cove-runtime/src/vm/report.rs` documents.

### 8. What a migration is gated on

A migration is complete only when the variant, its signature, class, category
and effects entries, its runtime arm and its tests are **deleted, in the same
stage that migrates its last producer**. A variant whose dynamic count is zero
but which still exists has not been migrated.

Per operation:

- the public API is unchanged, and AST, VM and both native generators agree;
- the primary diagnostic and its blame are unchanged, with a differential test
  per documented edge case;
- allocations and allocated words do not regress, at all, without a reason
  stated and accepted in the pull request;
- no new native refusal and no new native-to-VM crossing;
- GC roots and aliases stay correct across every allocation, and long scans
  stay bounded and cancellable under ADR 0040.

On wall time, measured on fixed-input covefmt and cq with interleaved builds
of both tiers:

- **neither program regresses past the rebuild noise floor** on either tier,
  judged against a control whose counts the change does not touch. ADR 0063
  measured covefmt VM moving −2.13% between two builds whose every count was
  byte-identical — a rebuild's code layout, larger than the whole count
  reduction it was measuring — so a whole-program wall-time claim needs a
  control or it is not a claim;
- **the migrated operation's own cost may rise, and the pull request records
  the multiple.** A rise past **2x** on the VM arm is not a failure by itself
  — the native arm is where these operations are going — but it is the signal
  to fix the general run substrate rather than to keep a nominal intrinsic
  quietly. Phase 1 is the architecture gate for exactly this, and it re-profiles
  before Phase 2 begins.

A migration that causes a repeatable regression outside noise may be reverted
or redesigned. The enum count is not a reason to keep a bad one.

## Measurement

Baseline commit `2fa8225`, `--profile checked`, x86-64 macOS,
`--features cove-cli/template` for the native arm. covefmt is
`cove run covefmtBench --files-root ..` from `examples/`, 267 files and
807,035 bytes. cq is `cove run cq --files-root cq/data -- bookings-20k.jsonl
--program revenue-summary`, 20,000 records, the input made by
`cove run cqSample --files-root cq/data -- 20000 bookings-20k.jsonl` — which
is written down because the file is not in the repository and a baseline
nobody can reproduce is a number, not a baseline.

|  | covefmt VM | covefmt native | cq VM | cq native |
| --- | ---: | ---: | ---: | ---: |
| emitted IR | 12,586 / 214 fns | same | 6,534 / 113 fns | same |
| `IntrinsicCall` sites | 42 | 42 | 53 | 53 |
| semantic instructions | 775,991,778 | 3,465,857 encoded | 264,228,796 | 9,863,951 encoded |
| dispatches | 735,278,388 | — | 257,322,816 | — |
| `fuel_spent` | 783,683,593 | 783,683,592 | 268,780,626 | 268,780,626 |
| allocations | 3,892,765 | 3,892,766 | 1,433,565 | 1,433,565 |
| allocated words | 48,378,224 | 48,378,228 | 9,614,088 | 9,614,088 |
| compiled / refused | — | 208 / 6 | — | 89 / 24 |
| machine code | — | 922,417 B | — | 375,036 B |
| `execute=` | 4,993.1 ms | 2,093.7 ms | 2,125.4 ms | 1,535.2 ms |
| **mediated intrinsics** | **415,809** | **415,811** | **140,092** | **140,092** |
| of those, from native | 0 | **413,750** | 0 | 91 |

**The per-site profile reconciles exactly.** Summing every `intrinsic-call`
row of `--profile --profile-rows all`'s by-instruction table gives 415,809 on
covefmt and 140,092 on cq — the boundary totals, to the call. (The by-opcode
table gives 140,000 on cq, 92 short, and that is `OPCODE_FLOOR`: four variants
ran fewer than 1,000 times. Decision 7 fixes the attribution, not the floor.)

### The 31 variants

Static sites and dynamic calls are this baseline's. "Destination" is the
proposal this ADR's phases are organised around, not a decision about how each
one is written.

| # | ph | variant | covefmt sites / calls | cq sites / calls | destination |
| --- | --- | --- | ---: | ---: | --- |
| 1 | 1 | `StringStartsWith` | 0 / 0 | 2 / 2 | Cove |
| 2 | 1 | `StringEndsWith` | 1 / 1,163 | 0 / 0 | Cove |
| 3 | 1 | `StringContains` | 1 / 896 | 14 / 80 | Cove |
| 4 | 1 | `StringIndexOf` | 0 / 0 | 0 / 0 | Cove |
| 5 | 1 | `StringLength` | 2 / **405,588** | 8 / 20,000 | Cove loop over `RunLoad` |
| 6 | 1 | `FloatAbs` | 0 / 0 | 0 / 0 | typed scalar op |
| 7 | 1 | `FloatMin` | 0 / 0 | 0 / 0 | Cove over typed compare |
| 8 | 1 | `FloatMax` | 0 / 0 | 0 / 0 | Cove over typed compare |
| 9 | 2 | `StringSlice` | 0 / 0 | 0 / 0 | Cove over `RunSlice` |
| 10 | 2 | `StringTrim` | 0 / 0 | 5 / **20,000** | Cove; whitespace from Decision 5's tables |
| 11 | 2 | `StringSplit` | 0 / 0 | 0 / 0 | Cove |
| 12 | 2 | `StringJoin` | 18 / 8,162 | 6 / 4 | Cove over ensure/store/commit |
| 13 | 2 | `StringReplace` | 0 / 0 | 1 / 0 | Cove |
| 14 | 2 | `StringChars` | 0 / 0 | 2 / 0 | Cove |
| 15 | 2 | `StringWords` | 0 / 0 | 0 / 0 | Cove |
| 16 | 2 | `StringFromCodePoint` | 0 / 0 | 0 / 0 | Cove encode over a growable run |
| 17 | 2 | `StringRefuseByteRange` | 20 / 0 | 0 / 0 | Cove; merges with `std.string.refuseRange` |
| 18 | 2 | `IntParse` | 0 / 0 | 1 / 0 | Cove |
| 19 | 2 | `IntParseRadix` | 0 / 0 | 0 / 0 | Cove |
| 20 | 3 | `FloatToInt` | 0 / 0 | 2 / **40,000** | checked typed conversion + Cove |
| 21 | 3 | `FloatRound` | 0 / 0 | 0 / 0 | typed scalar op |
| 22 | 3 | `FloatSqrt` | 0 / 0 | 0 / 0 | typed scalar op |
| 23 | 3 | `FloatParse` | 0 / 0 | 4 / **60,000** | Cove over packed bytes |
| 24 | 3 | `FloatFormat` | 0 / 0 | 3 / 6 | Cove |
| 25 | 4 | `StringToUpper` | 0 / 0 | 0 / 0 | Cove over Decision 5's tables |
| 26 | 4 | `StringToLower` | 0 / 0 | 0 / 0 | Cove over Decision 5's tables |
| 27 | 5 | `AnyEquals` | 0 / 0 | 0 / 0 | per-layout synthesis |
| 28 | 5 | `ValueOrder` | 0 / 0 | 0 / 0 | per-layout synthesis |
| 29 | 5 | `ValueAdmitKey` | 0 / 0 | 0 / 0 | per-layout synthesis |
| 30 | 5 | `ValueRenderInto` | 0 / 0 | 5 / 0 | per-layout synthesis; see proposed ADR 0061 |
| 31 | 5 | `ValueRefuseDuplicate` | 0 / 0 | 0 / 0 | Cove error construction |
| | | **total** | **42 / 415,809** | **53 / 140,092** | |

Both site columns sum to the emitted-IR figure and both call columns to the
mediated-intrinsics figure, which is what makes this a census rather than a
list. The per-variant detail behind each row — source declaration, lowering
producer, VM arm and the Rust facility it leans on, native treatment, effects,
signature, verbatim diagnostics, and what substrate a Cove implementation
already has — is on [the issue](https://github.com/myuon/cove/issues/432).

**Seventeen of the 31 are reached by neither program**, and that is the most
useful thing the census says about ordering: a migration whose variant runs
zero times on both cannot be gated by them, and needs a program written for it
or a correctness argument instead of a measurement. The four that these two
programs can actually measure are `StringLength`, `FloatParse`, `FloatToInt`
and `StringTrim` — and `ValueOrder` runs zero times on cq at zero sites
despite 180,000 `std.map.seekMap` calls, because Decision 3's rule already
applies to it.

### What a Cove reimplementation actually costs, measured

`String.length` is 97.5% of covefmt's intrinsic traffic and it is the whole of
Phase 1's risk, so it was prototyped before this ADR was written rather than
after. A scratch package times three entries over the same eight ASCII strings
— covefmt's printer pieces, 48 bytes and 48 characters per pass, 100,000
passes, 800,000 calls — with the loop in a callee so the native tier compiles
it, and the outermost frame, which is always encoded, does nothing:

- `byteLength()`, a header load, as the control;
- `length()`, the intrinsic;
- a Cove loop that reads the lead byte with `byteAt` and advances by its width.

The control is exact for `length`: the intrinsic entry and the control entry
execute **6,600,088 instructions each**, because `length()` and `byteLength()`
are one instruction apiece. Medians of five interleaved sets, minus the
control:

| | VM | native |
| --- | ---: | ---: |
| `length()` intrinsic | 76.8 ms — **96.0 ns/call** | 71.3 ms — **89.1 ns/call** |
| Cove loop | 137.0 ms — 171.3 ns/call | 28.4 ms — 35.5 ns/call |
| | **1.78x slower** | **2.51x faster** |

and the same shape for `contains`, 400,000 calls over four haystacks
(its control is looser — the branch is taken at different rates — so read the
ratio and not the nanoseconds):

| | VM | native |
| --- | ---: | ---: |
| `contains()` intrinsic | 185.2 ns/call | 181.2 ns/call |
| Cove loop | 743.5 ns/call | 158.3 ns/call |
| | **4.02x slower** | **1.14x faster** |

**An intrinsic call costs the same whether its caller is interpreted or
compiled** — 96.0 against 89.1, 185.2 against 181.2 — and Cove code doing the
same work is 4.8x and 4.7x faster compiled than interpreted. That is the whole
argument of this ADR in four numbers. The boundary is not a cost the native
tier pays; it is a cost the native tier *cannot* pay down, and it is the
reason the ratio flips sign between the columns.

Projecting the `String.length` migration onto covefmt's 405,588 calls:
**+30.5 ms on the VM (+0.61%)** and **−21.7 ms on native (−1.04%)**, the
latter also removing 405,588 native-to-runtime crossings, 98.0% of covefmt's
intrinsic helper calls. Both are inside Decision 8's bound, and the VM figure
is smaller than the rebuild-layout swing ADR 0063 measured — which is exactly
why Decision 8 asks for a control rather than for a percentage.

The projection's weak point is stated rather than smoothed: the prototype's
strings average six ASCII characters, chosen to resemble printer pieces, and
covefmt's actual length distribution at `covefmt.column+28` and `+32` was not
measured. A distribution with long strings moves the VM column against the
migration linearly, at 7.67 instructions per character. Phase 1 measures it
for real on covefmt before it claims anything.

## Consequences

The standard library grows and the runtime shrinks, which is the point, and
three costs come with it.

**Cove is slower at text than Rust is, on the interpreted tier**, by roughly
2x to 4x per operation, and Phase 1 exists to find out whether that is 2x or
20x before eleven more operations are committed to the same path. The answer
above is the encouraging one, and it was taken on one microbenchmark.

**A byte-run search is the first primitive this ADR is likely to owe.** The
`contains` figure — 4.02x on the VM — is the one that would justify it, and
Decision 2 already says what it may be called: a bounded run search over a
named storage, not `String.indexOf`. It is not added on this ADR's authority.
It is added when Phase 1's whole-program measurement asks for it, and refused
if it does not.

**The Unicode tables become the repository's to carry and to regenerate**, and
their size, build time and binary impact are reported at Phase 4 rather than
assumed. Decision 5 trades an unpinned dependency on the compiler for a pinned
one on a committed asset, and that is a trade rather than a saving.

Against those: a refusal a user reads is written in the language it refuses; a
diagnostic exists once instead of in Rust and Cove and the oracle; the native
tier can compile the standard library instead of calling out of it; and
`Intrinsic` can only get shorter.

## Adoption

Phase 0 is this ADR, the census of all 31 variants on the issue, the baseline
above, and the ratchet of Decision 1. Decision 7's reporting is the next pull
request and is not landed here.

Phases 1 through 6 are [#432](https://github.com/myuon/cove/issues/432)'s, one
small pull request per migration, each measured on its own. Phase 1 is the
architecture gate and re-profiles before Phase 2 begins.

This ADR is not complete until every variant is gone; its completion report is
the issue's, and it names the surviving primitives and why each is
irreducible.
