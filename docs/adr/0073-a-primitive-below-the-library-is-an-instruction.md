# ADR 0073: A primitive below the standard library is an instruction

- Status: Proposed
- Date: 2026-09-30
- Decides: that `Inst::IntrinsicCall` and everything that exists only for it —
  the `Intrinsic` enum and its signatures, classes and effects, `IntrinsicSite`,
  `Op::IntrinsicCall`, the VM's `vm::intrinsics` dispatcher, the native
  intrinsic helper and its protocol, and the boundary report's mediated-intrinsics
  row — are deleted, now that no variant has a producer; that an operation
  which has to stay below the standard library is an IR instruction of its own,
  admitted under [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s
  Decision 2, and never again a call into Rust by an identifier; and that
  ADR 0064 is accepted, as the program it proposed is complete. Recorded for
  [issue #432](https://github.com/myuon/cove/issues/432)
- Supersedes:
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  **"Runtime calls are statically identified and typed"**: the
  `intrinsic-call dst, IntrinsicId, args, blame` instruction by which "a core
  operation which remains in Rust" is reached, the per-intrinsic effect
  metadata (`may_allocate`, `may_raise` and the rest) that generated code reads
  around such a call, and the VM's dispatch on the numeric identifier and native
  code's binding of "a direct helper address or a compact helper table entry"
  for it. ADR 0058's header gains its `Superseded in part by` pointer
- Refers to, without superseding:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 1,
  which this fulfils — "the mechanism is deleted when no variant is left, which
  is a later ADR's to decide, at the time it happens" — and its header's "Deleting
  the mechanism will contradict ADR 0058, and needs its own superseding ADR",
  which is this; its Decision 2, where a future primitive goes; and its
  Adoption's completion clause, which "ADR 0064's completion report" below
  answers;
  [ADR 0068](0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
  Phase 5, whose "Delete `Intrinsic`, `IntrinsicCall`, its VM dispatcher,
  native ABI and reporting machinery" was transferred to #432
  (`docs/measurements/adr-0068.md`) and is done here;
  [ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md),
  [ADR 0070](0070-a-call-whose-continuation-is-doomed-may-be-expanded.md),
  [ADR 0071](0071-a-checked-conversion-answers-a-value-and-whether-there-is-one.md)
  and [ADR 0072](0072-float-parse-is-cove.md), the last migrations; and ADR
  0058's **"Fallibility preserves the source call site's blame"**, which is
  not superseded: its rule — the user's call site is the primary span — is
  kept by ADR 0067's trap and by inlining, and only the vehicle it named, a
  `BlameId` on a fallible intrinsic, has nothing left to ride on
- Changes no source-language API, and no executed instruction

## Context

[ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md) found 31
`Intrinsic` variants, every one named after a public method, and made the
set a migration mechanism whose population could only fall. It fell to one:
`Float.parse`, whose last producer issue #432 removed in #544 and #545
([ADR 0072](0072-float-parse-is-cove.md)). The variant was kept after that
only as scaffolding — the VM arm, and fixtures that built the `intrinsic-call`
by hand so that the reporting still had a call to count — because ADR 0064
Decision 1 reserves the mechanism's deletion for a later ADR, and ADR 0064's
header says that deletion contradicts ADR 0058 and needs its own. This is it.

### The population, 31 to 0

Every row deletes the variant in the change that migrated its last producer,
as ADR 0064's Decision 8 requires.

| after | variants deleted | PR | the ADR the replacement stands on |
| ---: | --- | --- | --- |
| 30 | `StringLength` | #438 | 0064 |
| 29 | `StringEndsWith` | #439 | 0064 |
| 28 | `StringStartsWith` | #443 | 0064 |
| 27 | `StringContains` | #449 | 0065 (`Inst::RunFind`) |
| 26 | `StringIndexOf` | #450 | 0065 |
| 25 | `FloatAbs` | #451 | 0064 Decision 2 (`Inst::FloatAbs`) |
| 23 | `FloatMin`, `FloatMax` | #453 | 0064 Decision 2 (`Inst::FloatMinMax`) |
| 22 | `FloatRound` | #457 | 0064 Decision 2 (`Inst::FloatRound`) |
| 21 | `FloatSqrt` | #458 | 0064 Decision 2 (`Inst::FloatSqrt`) |
| 20 | `StringSlice` | #460 | 0064 |
| 19 | `StringFromCodePoint` | #462 | 0064 |
| 18 | `StringJoin` | #463 | 0064, 0062 |
| 17 | `StringChars` | #465 | 0064 |
| 16 | `IntParse` | #467 | 0064 |
| 14 | `StringWords`, `StringTrim` | #468 | 0064 Decision 5 |
| 12 | `StringToUpper`, `StringToLower` | #470 | 0064 Decision 5 |
| 11 | `ValueRefuseDuplicate` | #482 | 0067 (`core.refuse`) |
| 10 | `IntParseRadix` | #483 | 0067 |
| 8 | `StringSplit`, `StringReplace` | #485 | 0067, 0065 |
| 7 | `FloatFormat` | #489 | 0064 Decision 6 |
| 6 | `AnyEquals` | #495 | 0068 |
| 5 | `ValueOrder` | #497 | 0068 |
| 4 | `ValueRenderInto` | #504 | 0068 |
| 3 | `ValueAdmitKey` | #507 | 0068 |
| 2 | `StringRefuseByteRange` | #539 | 0070, 0067 |
| 1 | `FloatToInt` | #540 | 0071 (`Inst::FloatTruncate`) |
| 1 | — `Float.parse`'s last producer | #544, #545 | 0072 |
| 0 | `FloatParse`, and the mechanism | this change | 0073 |

Twenty-eight pull requests, and the count only ever fell: ADR 0064's ratchet
compared the variant set as a set, so no row above could have hidden an
arrival behind a departure.

## Decision

### 1. The mechanism is deleted, whole

Everything that existed only for `IntrinsicCall` goes, crate by crate:

- **cove-ir**: `intrinsic.rs` — `Intrinsic`, `ALL`, `COUNT`, `Signature`,
  `Class`, `Carried`, `Category`, `Effects`, and the ratchet and census tests
  that held them; `Inst::IntrinsicCall`; `IntrinsicSite`, `SiteId` and
  `Program::intrinsic_sites`; the verifier's signature check and its cases; the
  printer's `intrinsic-call`; the arms in `flow`, `lower::frees` and
  `lower::inline`; `lower::methods`' `emit_intrinsic_call`, and the dead
  `ASSOCIATED` path behind it (`Duration.nanos` keeps its `Convert`s); and
  `Op::IntrinsicCall` with its encoding, decoding and bytecode verification.
- **cove-runtime**: `vm::intrinsics` — the dispatcher, the `scalar` arm, the
  `operand` frame-and-destination protocol and the `make` builders that only its
  arms called; `Machine::call_intrinsic` and the checked read-before-write and
  effect assertions around it, with the per-thread allocation count that only
  they read; the encoded `INTRINSIC_CALL` arm; the native `intrinsic` helper,
  its counted twin and its table slot; `Blocked::Intrinsic`; and the
  mediated-intrinsics rows of the boundary report.
- **cove-native**: `IntrinsicFn`, `IntrinsicProtocol`, `IntrinsicCode`, the
  template compiler's `intrinsic_call` and its helper-table slot, subset
  admission of the instruction, and the suite's protocol and attribution cases.
- **cove-cli**: `--boundary`'s mediated-intrinsics section, the native report's
  intrinsic machine-code table, and the refusal census's `IntrinsicCall` table.

Two things that lived in `vm::intrinsics` had readers outside it and moved
rather than went. The text of a handle — a Host resource, a scope, a task — is
`vm::exec::dynamic::text_of_handle`, which `Inst::HandleText` and
`core.dynamicHandleText` both write with. The growable-run cases that were
`vm::intrinsics::seq`'s and the keyed-finish case beside them, with the fixture
they build in, are `vm::sequences`: they test `Machine`'s run operations, which
stay.

The opcode table loses one entry, 200 to 199; every opcode after
`CallResource` is numbered one lower, which is a fact about the table rather
than about any program, because every number is computed from the family bases.
The native helper table loses its `intrinsic` field; it is a Rust struct of
function pointers read by name, so no other slot moves.

### 2. A primitive below the standard library is an instruction

What replaces ADR 0058's decision is the shape every migration above already
took. An operation that the standard library cannot write — because it names
a machine operation, is proportional work that must be charged and
cancellable, stops a run, or observes a type-erased value — is **an IR
instruction of its own**, admitted by ADR 0064 Decision 2's test: its only
failures are ones the program did not write, and it would not have to be
renamed if the standard library renamed a method. It is printed, verified,
encoded, and lowered by the native tier as that instruction, and what it may
allocate or raise is a fact the verifier and the code generator know about
*that instruction*, not a flag set read off a table of identifiers.

Where the native tier needs the runtime for one, it gets a helper of its own,
as `AllocFn`, `GrowableFn`, `RunCopyFn` and `DynamicFn` are. What there is not,
again, is one instruction and one helper for "call this Rust function by
number": that is the shape ADR 0064 measured as a floor the native tier cannot
lower, and it is the shape whose population had to be ratcheted down by hand
for twenty-eight pull requests. A new operation that wants to be a runtime
call by identifier is the design error ADR 0064's ratchet used to name, and
there is no longer a mechanism for it to be.

### 3. The boundary report loses a row, not its reconciliation

`--boundary` reported emitted IR with its `IntrinsicCall` sites, mediated
intrinsic calls by variant and tier with their allocations and words (ADR 0064
Decision 7), native-to-runtime helper calls including `intrinsic`, and the
native report a table of the machine code charged to intrinsic call sites.
Every intrinsic column, row and table is deleted; every other quantity stays,
printed as before.

What held the report to account moves with it. The case that reconciled the
boundary report against the instruction profile per intrinsic variant now
reconciles the run: one run watched by the profiler and by boundary counting at
once must agree exactly on the instructions executed, and the profiler's
allocations and words are bounded by the heap's. `native_tier.rs`'s
`the_boundary_report_counts_each_quantity_apart` put a `Float.parse`
`intrinsic-call` back by hand to have a mediated call to count; it now counts
what that call is — a library call the encoded tier makes `n` times, and a
VM-to-native crossing each time, because the parser compiles and its caller does
not. Only cases that tested the mechanism itself are deleted.

### 4. ADR 0064 is accepted as its program completes

ADR 0064 stayed `Proposed` through its whole series, though
[ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md),
[ADR 0068](0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md) and
[ADR 0071](0071-a-checked-conversion-answers-a-value-and-whether-there-is-one.md)
each superseded a part of it. Its program is complete: no variant is left and
the mechanism is gone. Its status becomes `Accepted`, its `Superseded in part
by` pointers stay as they are, and its body is not touched. ADR 0058's header
gains the pointer ADR 0064 said it would gain "when this ADR is accepted",
naming the sentence ADR 0064 withdrew, beside this ADR's own.

## ADR 0064's completion report

ADR 0064's Adoption: "This ADR is not complete until every variant is gone; its
completion report is the issue's, and it names the surviving primitives and
why each is irreducible." Every variant is gone. What stands below the standard
library in their place, and why each cannot be Cove:

| primitive | admitted by | stands under | why it stays below |
| --- | --- | --- | --- |
| `Inst::RunFind` (`core.stringFind`) | ADR 0065, #449 | `String.contains`, `indexOf`, `split`, `replace` | a search proportional to a haystack the caller did not size, charged and cancellable under ADR 0040; a Cove scan pays a dispatch per byte (4.02x on the VM, ADR 0064's measurement) — Decision 2's bounded run operation |
| `Inst::FloatAbs`, `FloatMinMax`, `FloatRound`, `FloatSqrt` | ADR 0064 Decision 2; #451, #453, #457, #458 | `Float.abs`, `min`, `max`, `round`, `sqrt` | typed scalar operations with bit-level contracts pinned per instruction: `abs` must not quiet a signalling NaN, which every arithmetic route does; `min` and `max` fix NaN and signed zero; `round` ties away from zero; `sqrt` is IEEE 754's correctly rounded square root, which an iteration in Cove would not reproduce bit for bit |
| `Inst::FloatTruncate` (`core.floatTruncate`) | ADR 0071, #540 | `Float.toInt` | the one way a `Float` becomes an `Int`: a checked typed conversion, named in Decision 2's list, with two outputs and no sentinel |
| `Inst::Trap`'s three sentences (`core.refuse`) | ADR 0067, #481 | every refusal the migrated methods word in Cove | stopping a run is not a value a Cove body can return; the primitive decides nothing about whether to stop — Decision 2's sixth entry |
| ADR 0068's observations of an erased value — `DynOpen`, `DynKind`, `DynRead`, `DynCase`, `DynCount`, `DynChild`, `DynSameType`, `DynSameObject`, `DynNameOrder`, `DynTypeName`, `DynFieldName`, `DynCaseName`, `DynOpaque`, `DynHandleText`, `DynOnPath`, the identity set's three, and `HandleText` | ADR 0068; #492 onward, #503, #504, #523 | `std.dynamic`'s equality, order, key admission and rendering of a `dyn Trait` or Host `Any` | Cove has no structural access to a value whose type was erased; the view answers structural questions and never an address, an offset or a layout — Decision 2's fifth entry, as ADR 0068 replaced Decision 4 |

Everything else the variants did is Cove over primitives that were already
there: ADR 0058's and ADR 0062's run substrate (`Len`, `RunLoad`, `RunSlice`,
`RunCopy`, the growable ensure, store and commit, `RunFinish`), ADR 0059's
three-way `Cmp` order, the `Convert`s ADR 0058's Phase 5 made of
`Duration.nanos` and `Int.toFloat`, the walks ADR 0064's Decision 3 has the
lowering synthesize, and Unicode tables held as string literals (Decision 5).
None of those was admitted for a variant, so none is on the list.

The typed scalar row is irreducible as a contract, which is a different claim
from irreducible as an algorithm. Four of the five were admitted while the
native tier's subset took no float comparison or arithmetic, so a Cove body
would have sent every caller back to the VM; issue #501 has since admitted
both. Whether `abs`, `min`, `max` or `round` could now be written in Cove to
the same bits is not a question ADR 0064 owes an answer to, and it is not
decided here.

## Measurement

`before` is main at `356fd2f`; `after` is this change. Each is
`--profile checked --features template`, run in turn over one tree, from
`examples/`: covefmtBench with `--files-root ..`; cq over #509's
`bookings-20k.jsonl` for `revenue-summary` and `confirmed-bookings`, and ADR
0046's `rates.csv` 20,000 times over, 120,000 rows, for `rate-card`. Every
program's standard output is byte-identical across the two binaries and both
tiers (covefmt's four timing lines aside).

### Nothing that runs changed

Nothing emitted the instruction, so no executed instruction can move, and none
did:

| workload | tier | instructions | `fuel_spent` | allocations | allocated words | host calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| covefmt | VM | 1,748,427,632 | 1,769,705,095 | 8,442,524 | 107,013,982 | 887 |
| covefmt | native | 193,088 | 1,769,705,038 | 8,442,524 | 107,013,982 | 887 |
| revenue-summary | VM | 261,642,846 | 266,195,360 | 1,413,617 | 9,171,965 | 20,009 |
| revenue-summary | native | 4,040,199 | 266,195,360 | 1,413,617 | 9,171,965 | 20,009 |
| confirmed-bookings | VM | 297,924,878 | 309,654,423 | 2,425,908 | 14,930,392 | 33,390 |
| confirmed-bookings | native | 4,267,683 | 309,654,423 | 2,425,908 | 14,930,392 | 33,390 |
| rate-card | VM | 496,182,075 | 545,382,088 | 9,080,183 | 44,361,965 | 240,006 |
| rate-card | native | 6,600,289 | 545,382,088 | 9,080,183 | 44,361,965 | 240,006 |

Each figure is the same before and after, to the unit. So is every other line
of `--stats --boundary`: emitted IR (11,617 instructions in 110 functions on
covefmt, 9,853 in 100 on cq), dispatches, fused windows and their census,
growths, library calls, every tier crossing, every helper call, compiled and
refused functions, and machine-code bytes (827,668 on covefmt, 875,538 on cq).
The only lines that differ are the ones this change deletes — `0 of them
IntrinsicCall site(s)`, the `intrinsic 0` helper row, and `mediated
intrinsics, 0 call(s)` with its header — and the timings.

### What it saves

| | before | after | |
| --- | ---: | ---: | ---: |
| opcodes | 200 | 199 | −1 |
| `cove` binary (checked, template) | 9,581,392 B | 9,484,552 B | −96,840 B (−1.01%) |
| its `__text` | 6,265,632 B | 6,193,344 B | −72,288 B |
| `encoded::dispatch` | 39,248 B | 39,104 B | −144 B |
| lines under `crates/`, code and tests | | | +683 −5,631 (−4,948), 56 files |

The dispatch loop lost its `INTRINSIC_CALL` arm and nothing else, 144 bytes of
it; the arm's work was already out of line (`call_intrinsic` was
`#[inline(never)]`). covefmtBench on the VM, five interleaved pairs of the two
binaries, ABBA-ordered, `execute=` medians: **10,875 ms before and 10,634 ms
after (−2.2%)**, every after run faster than every before run. That is outside
the ±1% covefmt usually rebuilds within, so it was investigated rather than
kept. The one executed-path change besides the arm is the per-thread allocation
count `call_intrinsic`'s effect assertion read, a thread-local increment per
allocation under `debug_assertions`, which the checked profile keeps; a control
binary — this change with that increment put back — ran with the after binary,
not the before one, in a second interleaved set of five triples (medians 10,908
before, 10,681 after, 10,604 control). So the counter is not it. What remains is
the rebuilt dispatch loop and the code around it, over identical counts:
[ADR 0063](0063-a-buffer-window-is-measured-before-it-is-optimized.md) measured
covefmt's VM moving −2.13% between two builds whose every count was
byte-identical, and this is the same size and shape. **No wall-time effect is
claimed**; what is claimed is that dispatch was not made slower.

## Consequences

- The IR has one fewer instruction and no instruction whose meaning is "run the
  Rust function this number names". Every operation below the standard library
  is an instruction a reader of a listing can look up, the verifier checks and
  the native tier lowers as itself.
- A future primitive costs an instruction, a verifier arm, an encoding and a
  native lowering, which is more than a variant used to cost. That is the
  intended price: ADR 0058's Alternatives already weighed "put every collection
  operation in executable IR" and refused it for *public methods*; what
  Decision 2 admits is machine operations, and over twenty-eight migrations it
  admitted the five rows of the completion report, not thirty-one.
- ADR 0064 Decision 7's per-variant allocation and machine-code attribution has
  nothing left to attribute and is gone with the report rows. What a primitive
  costs is attributed where every other instruction's is: the instruction
  profile, and the boundary report's helper table where a helper is involved.
- ADR 0064's variant ratchet, `the_intrinsic_set_only_shrinks`, is deleted with
  the set it guarded; there is no longer a set that could grow.
- ADR 0064 is accepted. Its Adoption places its completion report on issue
  #432; the section above is that report, and the comment can carry it.
