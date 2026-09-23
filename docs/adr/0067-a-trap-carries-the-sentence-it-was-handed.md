# ADR 0067: A trap carries the sentence it was handed

- Status: Accepted
- Date: 2026-09-23
- Decides: that `Inst::Trap` carries three sentences built at run time rather
  than one string chosen at lowering time; that the standard library may stop a
  run through one primitive, `core.refuse`, and what that primitive is
  forbidden to know; that an empty sentence is an absent one; and which of the
  questions the six blocked migrations raise this ADR answers and which it
  deliberately leaves to them. Recorded for
  [issue #461](https://github.com/myuon/cove/issues/461)
- Supersedes:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2
  **acceptable vocabulary** — the five-entry list of what a primitive may be.
  It gains a sixth entry: *stopping a run with a sentence the caller built*.
  Decision 2's **test** is not touched and is not weakened: "a primitive's only
  failures are ones the program did not write" is a rule about an operation
  that computes and may fail while computing, and `core.refuse` computes
  nothing and cannot fail. The list, read as the exhaustive enumeration it was
  written as, admitted no way for anything above the boundary to stop a run at
  all — which is why six of the variants ADR 0064 exists to retire could not
  move. What changes is the list; the test decides the same six the same way it
  did before, and now they can obey it.
  ADR 0064's header gains its `Superseded in part by` pointer
- Refers to, without superseding:
  [ADR 0062](0062-an-append-is-ensure-store-commit.md)'s **"each refusal calls
  `core.refuseByteRange`, an `IntrinsicCall` that always raises — an intrinsic
  rather than a Cove call, so the body remains an inlinable leaf"**. That
  decision stands, and this ADR is careful not to overturn it by implication: a
  mechanism for raising from Cove is not a finding about what raising from Cove
  costs `appendRange`, whose leafness ADR 0062 chose deliberately and measured.
  The intrinsic stays until a migration measures its replacement.
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  blame rule — the caller's line is the diagnostic's primary location and the
  library's is context — which this ADR keeps exactly and relies on;
  [ADR 0045](0045-a-literal-is-there-before-the-program-runs.md), which is what
  makes a constant sentence in a slot cost no allocation;
  [ADR 0034](0034-one-physical-word-stack.md)'s evaluator
  agreement, which is what a refusal's three sentences are now held to on both
  backends

## Context

### A Cove body can answer, and could not stop

ADR 0064's Decision 2 says that every refusal quoting a method name, a
parameter name, a value or a range is the standard library's, and is
constructed in Cove. Six of the variants it wants to retire cannot obey it,
for one reason: **a Cove body had nothing to raise with.**

Established by looking rather than by assuming, in #460, #466 and #476:

- `ExprKind` has no `raise`, `throw` or `panic` form, and no keyword for one;
- `assert` and `assertEqual` build an ordinary `Err` the enclosing function
  must return;
- `Result` and `Option` expose no panicking accessor;
- `?` is refused at check time outside a `Result`-returning function;
- `Inst::Trap` carried a `StrId`, so it could stop a run with a constant and
  could not name which offset was wrong.

`crates/cove-ir/src/lower/core.rs` has said so all along — `appendRange`
*"decides a byte range in Cove and has nothing to raise with"* — which is why
ADR 0062 gave it an intrinsic.

The six, one reason each, with the step of
[issue #454](https://github.com/myuon/cove/issues/454) each belongs to:

| variant | step | what it raises on |
| --- | --- | --- |
| `StringSplit` | 3 | an empty separator |
| `StringReplace` | 3 | an empty needle |
| `StringRefuseByteRange` | 3 | one of five things about a byte range |
| `IntParseRadix` | 4 | a radix outside `2..=36` |
| `FloatFormat` | 6 | a digit count outside `0..=17` |
| `ValueRefuseDuplicate` | 7 | a duplicate key |

`crates/cove-schema/src/builtins.rs` argues the line they all draw
deliberately: *between an argument the program got wrong and text the data got
wrong*. The data's failure is an `Err` a Cove body builds happily. The
program's failure raises.

### The constraint was one layer under "it cannot choose the words"

This is #476's finding, and it is the part that decides the shape of the fix.
**A `RuntimeError` is three printed sentences** — `message`, `rule` and `help`
— and a trap carried one. So `Intrinsic::ValueAdmitKey`'s refusal was
unwritable *even in the arm where every interpolation is a constant*, because
a trap is one string and the refusal is three. An operation can pass the "does
it quote a value computed at run time" test and still be unmovable.

A `core.refuse(message: String)` of the shape #461 first sketched would
therefore have migrated **none** of the six without dropping a `rule:` and a
`help:` line that ADR 0064's Decision 8 gates on — and the two `help` lines of
`String.split` and `String.replace` are not the same sentence, so they cannot
even share one.

### The words were already above the line, twice, and below it, twice

`String.sliceBytes`' five questions are worded in `std/string.cove`'s
`refuseRange`, in `vm::intrinsics::text::refuse_byte_range`, and in
`builtins`' `wrong_byte_range` — three copies of five sentences, in two
languages, kept in agreement "by nothing but care", as ADR 0064 put it. The
reason the Rust copies exist is not that anyone wanted them: it is that the
path which raises could not be written where the path which answers was.

### Cove could already raise; it could not choose

The wall was never "there is no way to stop a run from above the boundary".
`core.refuseByteRange` is a `core.*` builtin, reachable from
`std/stringbuilder.cove`, that never answers. So this is a question about a
primitive's **operands**, not about a new kind of control flow — and `Never` is
a non-issue, because that builtin's declared result is `Unit` and the code
after the call is simply unreachable.

## Decision

### 1. `Inst::Trap` carries three sentences, in slots

```
Trap { message: Slot, rule: Slot, help: Slot }
```

Each slot holds the address of a heap `String` — `Repr::Ref`, which the
verifier now requires of all three, exactly as it already required of
`Inst::AssertFailed`'s one. `AssertFailed` is the precedent in every pass, and
its own doc already argued the case for a slot: `assertEqual` renders the two
values it compared, and that string is built at run time.

**Three and not one**, because the alternative migrates nothing: the
constraint above is not that the words are computed, it is that a refusal is
three lines. **Slots and not a wider payload**, because two of the three
sentences a migration needs quote a value the run computed, and an instruction
payload is a compile-time constant by construction.

A trap whose sentence *is* a constant — an exhausted `match`, an
undispatchable trait call, and the three refusals synthesis writes — now
materialises its string into a slot with `Inst::Str` first. That costs no
allocation and no branch: ADR 0045 puts every program literal in the heap
before the run's first instruction, so `Inst::Str` is a load of a precomputed
address. It costs **two instructions per trap site** — the message and one
empty string shared by `rule` and `help` — on a path that, by construction,
ends the run. What that is worth in a program, and why a second failure
instruction carrying the slots is *not* the answer, is the Measurement section
below: +3.1% of emitted IR on covefmt, +0.017% of the instructions it executes,
and 6,380 *fewer* bytes of machine code.

### 2. `core.refuse` is admitted, and it is forbidden to know anything

```
core.refuse(message: String, rule: String, help: String) -> Unit
```

One `Inst::Trap` over the three arguments. It is the whole of the new surface.

It passes Decision 2's own discriminator — *"whether the operation would have
to be renamed if the standard library renamed a method"* — as cleanly as
anything in the vocabulary: it quotes nothing, names no method, no parameter,
no value and no range. The standard library builds all three sentences and
hands them over, so **the policy stays in Cove and only the stopping is
below.** That is the entire argument for admitting it, and it is why the entry
the vocabulary gains is *stopping*, not *refusing*: a primitive that decided
anything about whether to stop would be back on the wrong side of Decision 2's
test.

It never answers. Its declared result is `Unit` rather than a bottom type, for
`core.refuseByteRange`'s reason: a diverging expression would be a new shape
for the checker and the verifier, and nothing here needs one.

### 3. An empty sentence is an absent sentence

A `rule` or a `help` that is the empty string prints no such line. Not a blank
`rule:`, and not an `Option<Slot>` in the instruction.

This is a choice and it is worth stating as one. The instruction is 16 bytes
with three free word fields, so three slots are free and a sentinel would not
be; and on the Cove side a refusal with two sentences reads as
`core.refuse(message, rule, "")`, which says what it means without a second
primitive or a second arity. `Float.format`'s refusal has two lines and
`Map.of`'s has three; both are one call.

### 4. The blame rule does not move, and a migration owns its diagnostic

ADR 0058's rule governs, unchanged: the caller's line is the diagnostic's
primary location, and every library line on the way is context labelled *in
the standard library*. `Int.abs` already traps from `std/int.cove` this way
(#258).

Two consequences a migration has to own rather than discover:

- a refusal raised one Cove call deeper than the intrinsic it replaces gains
  **one context line**. It is an observable change to a pinned `expected.err`,
  and it is the migration's to justify, not this ADR's to permit in advance;
- #476 measured that a raise inside a *synthesized* walk adds a second
  `in the standard library` label **on the same line**. So for synthesis the
  right shape remains the one #476 found — the walk decides, `lower::core`
  refuses at the same site in the same frame — and this ADR does not change
  that recommendation. A trap that can carry words is not a licence to raise
  from anywhere.

### 5. What this does not decide

- **Which of the six migrate, or when, or in what order.** Each is measured
  when it lands, against ADR 0064's Decision 8 gates. In particular ADR 0062's
  leaf argument for `appendRange` is untouched: the intrinsic whose *only*
  purpose is a sentence is, of the six, the one whose replacement has the most
  to prove, because the body it raises from is one the lowering expands.
- **`assert`'s lowering.** A trap can now carry the string `assertEqual`
  renders, which is a better lowering than the one assertions have; nothing
  here proposes it.
- **Whether `Intrinsic` reaches zero.** Four variants remain behind Decision
  4's boxed fallback by design, and #454's final step is unreachable for that
  reason rather than for this one.

## Measurement

The question a reader will have is whether making every trap pay for a slot was
worth avoiding a second failure instruction, and it is a fair one: the cheap
design is `Trap { message: StrId }` kept as it was, with the slots on an
instruction of their own, and it would cost the existing traps nothing. So the
cost was measured rather than argued. `--boundary`, both representative
programs, against the same corpus of 318 files in the same directory order:

| | covefmt | cq |
| --- | --- | --- |
| emitted IR, instructions | 11,219 → **11,570** (+351, +3.1%) | 6,876 → **7,031** (+155, +2.3%) |
| emitted IR, functions | 111 → 113 | 79 → 81 |
| encoded VM, instructions executed | 1,432,218,178 → **1,432,458,276** (+240,098, **+0.017%**) | 1,741 → **1,741**, to the instruction |
| encoded VM, dispatches | 1,347,614,263 → 1,347,856,078 (+0.018%) | 1,062 → 1,062 |
| machine code | 803,382 → **797,002** bytes (−6,380, **−0.79%**) | — |
| compiled functions | 105 of 111 → 107 of 113 | — |

`cove-bench`'s nine rows are **identical in fuel and in instruction count**,
every one — but that is a control that cannot see what it controls, since none
of them reaches a trap, and it is recorded as the fence it is rather than as
evidence.

**The static rise is the whole of the cost and it is not where it appears to
be.** +351 instructions is two `Inst::Str` per trap site, and the traps in
those two programs are what the lowering writes: an uncovered `match`, an
undispatchable call, and the three sentences synthesis raises. None of them
executes in a run that answers.

**The +2 functions on each program are one effect, and it is the reason the
machine code fell.** A function whose trap grew by two instructions crossed
`lower::inline`'s leaf budget and stopped being expanded, so it is emitted once
and *called* rather than copied into each of its callers: hence two more
compiled functions, 6,380 fewer bytes of machine code, and 234,995 more
`native -> native direct` calls. That last number looks large beside 17.7
million calls and is 0.017% beside 1.43 billion instructions, which is the
denominator that matters — and it is a twentieth of covefmt's ±1% rebuild noise
floor, so no wall-clock claim is made in either direction.

One further movement, recorded because it would otherwise look like a fault:
the buffer windows' *declines by safepoint* moved by about 0.2% (`push.words`
63,588 → 63,704). Two more instructions per trap site move where a poll lands,
and a window declined for a safepoint is ADR 0062's protocol working. The
fused-versus-slow split is unchanged to within the same 0.2%, and no window
changed shape.

So the second instruction is not bought. A design that split them would trade a
permanent second failure instruction — in the verifier, the inliner, the
liveness pass, both backends and the native subset — for 0.017%.

## Consequences

The linear-memory backend reads three addresses where it read a `StrId`, on a
path that was already leaving. The tree-walking interpreter gains one
`call_core` arm and needed nothing structural: it has always built messages
out of live values, because it is Rust over `Value`s and never sees an `Inst`.

The native tier carries three addresses across a raise instead of one
`StrId`. `NativeCtx::raise_detail` — whose only consumer was `Raise::Trapped`
— is gone, and three address fields take its place; the code generator loads
each slot and stores it, which is the shape `Raise::IndexOutOfRange` already
used for the two numbers it names. Compiled code still leaves through
`Outcome::Raised`, with no new helper call and no new outcome to test.

What the repository gets for it is one place to word a refusal instead of two
languages' worth, for six operations that between them own the diagnostics of
`split`, `replace`, `parseRadix`, `format`, a byte range and a duplicate key.
What it pays is two instructions per constant trap site, and one entry in a
vocabulary that was meant to be closed.

## Adoption

Four things are asserted, one per layer, and the first is the one that did not
exist before:

- **A Cove body raises a refusal it worded, and both evaluators word it the
  same** — `vm::differential`'s
  `a_refusal_a_cove_body_words_arrives_whole_on_both_evaluators`, over a probe
  in a library module, which is ADR 0034's rule applied to a refusal rather
  than to an answer. It compares all three sentences rather than the message,
  because `differential`'s own `said` keeps the message alone and a dropped
  `rule:` would have read as agreement. It pins the lowering too: one
  `Inst::Trap` and no `IntrinsicCall`, since a refusal primitive that was an
  intrinsic would be one more variant rather than the end of six.
- **The three slots are verified as `Repr::Ref`**, in
  `crates/cove-ir/src/verify.rs`, and the whole corpus exercises the widened
  instruction through the traps the lowering already emitted.
- **A trap runs as machine code and names its sentences by address** —
  `cove-native`'s `a_trap_names_its_sentences_by_address`, which asserts the
  three loads read the slots the instruction named.
- **A trap is entered from a real run on the native tier** —
  `crates/cove-runtime/tests/native_tier.rs`'
  `a_trap_in_machine_code_is_the_vm_s_refusal`, and that file had **no** trap
  case before this ADR, for a reason worth recording: a trap carried an
  immediate, so there was nothing about one a *run* could get wrong. Now there
  is. Its fixture reaches a trap the way `conformance.rs` does — two enums
  declaring a `Red`, so resolution abstains about exhaustiveness and the
  default arm is reachable — and it runs in the step of
  `.github/workflows/ci.yml` that drives `cove-runtime`'s suite under
  `--features template`, because `crates/cove-native`'s own suite compiles the
  code generator and does not enter it from a VM.
