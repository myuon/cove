# ADR 0071: A checked conversion answers a value and whether there is one

- Status: Accepted
- Date: 2026-09-29
- Decides: that `Float.toInt` is `std.float.toInt`, a Cove body over one
  primitive, `core.floatTruncate`; that the instruction beneath it,
  `Inst::FloatTruncate`, is a checked conversion with **two logical outputs** —
  an `Int` and a `Bool` — and does not write Cove's `Option<Int>`; that no
  x86 indefinite value is part of the IR's or the VM's contract; that the
  refusal of a finite value outside `Int`'s range quotes the value as Cove
  renders a `Float`; and that a program's own `toInt` call stays an unexpanded
  Cove call. Recorded for [issue #432](https://github.com/myuon/cove/issues/432)
- Supersedes: [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s
  Decision 8 **"the primary diagnostic and its blame are unchanged"**, for one
  sentence only: `Float.toInt`'s refusal of a finite value outside `Int`'s
  range, whose quoted value changes notation (Decision 4 below). Its words
  around the value, its blame, the other two refusals, every other
  diagnostic, and every other gate of Decision 8 are unchanged. ADR 0064's
  header gains its `Superseded in part by` pointer
- Refers to, without superseding:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2,
  whose vocabulary names "a checked typed conversion", and Decision 6, which
  deleted `Convert::FloatToInt` as "a second, wrong answer in the IR";
  [ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md), whose
  `core.refuse` this migration does not use — `Float.toInt` answers an `Err`,
  it does not stop the run, so there is no trap and no blame to move;
  [ADR 0070](0070-a-call-whose-continuation-is-doomed-may-be-expanded.md),
  whose rule does not admit `std.float.toInt` for expansion, which is why a
  program's own call stays a call (Decision 5)

## Context

`Float.toInt` was the second of the three intrinsics left (#536), and the only
one of them a representative program runs: `f64::trunc` and three refusals in
Rust behind `Intrinsic::FloatToInt`, answering `Result<Int, Error>`. On cq it
was 40,006 calls on `revenue-summary`, 86,345 on `confirmed-bookings` and
260,000 on `rate-card`, and **140,000 of rate-card's came from inside
`std.float`**, whose `format` and `renderInto` each ask it for a significand
or a whole number they have already held inside the range.

The operation is the case ADR 0064's Decision 2 names in so many words — a
checked typed conversion — and it could not be written in Cove at a price
worth paying: a bit-by-bit truncation is about 350 IR instructions a call,
+19% on rate-card's VM. So it needed an instruction. It could not be a member
of `Convert`: a checked conversion has two answers, the integer and whether
there is one, and `Convert`'s shape is one word in and one out. Decision 6 had
already deleted that family's `FloatToInt` for being Rust's saturating `as` —
`0` for a NaN, clamped at each end — which is a second answer for exactly the
inputs `Float.toInt` refuses.

The plan (issue #432, "Plan: migrate `Intrinsic::FloatToInt`") set out three
primitives and three notations; the decisions below are the ones taken on it.

## Decision

### 1. The primitive is a checked conversion with two logical outputs

`Inst::FloatTruncate { dst, ok, a }`:

- `ok` is a `Bool`, true exactly when `-2^63 <= a < 2^63`. A NaN compares
  false with both ends and is not `ok`; neither is either infinity.
- When `ok`, `dst` is `a` truncated toward zero — exact, because every double
  in the range truncates to an integer an `Int` holds. `-0.0` and every
  subnormal are `0`, `-2^63` is `Int.MIN`, and the largest double that is `ok`
  is `2^63 - 1024`.
- **The contract is total.** Every conversion that is not valid answers the
  one canonical pair **`(0, false)`**, on every tier. The zero is part of that
  pair and carries no converted integer; a consumer that decides whether the
  conversion succeeded inspects `ok`, never `dst`. Defining the pair — rather
  than leaving `dst` unspecified — is what lets the tiers write the same bits
  and the tables beside them compare them.

Which of the three reasons made a value not `ok` is not an output. The
standard library tells them apart in Cove on the refusal path, where the
comparisons cost nothing a value that converts has to pay.

The verifier holds it to `dst: Int`, `ok: Bool`, `a: Float`, and two different
slots for the two answers — and to nothing else. It encodes as one opcode, the
two hundredth. The encoded VM's arm and the tree-walking interpreter's
`core.floatTruncate` share one specification, `cove-runtime`'s
`float::truncate`; the native tier lowers it (fourteen instructions, no
branch) and admits it, so it refuses no function.

### 2. No instruction constructs `Option`

`core.floatTruncate(x) -> Option<Int>` is the interface `std.float` sees, and
the `Option` is built by the lowering from the two outputs — a branch on `ok`,
the `Some` tag over a payload the instruction wrote in place, or the `None`
tag and the payload cleared — with the instructions any enum construction is
made of. So no instruction knows how an `Option` is laid out, and the
instruction's contract is a pair of scalars that any later user can consume
without going through Cove's enum representation.

The alternative in the plan, "(A)" as first written, was an instruction that
wrote the `Option<Int>` itself. It would have saved a branch and a tag at each
site and put a layout — an enum's discriminant and payload — into an
instruction whose question is arithmetic.

### 3. B-sat and B-ind are rejected

- **B-sat** — Rust's saturating `as` behind guards in Cove — is the shape
  ADR 0064's Decision 6 deleted in #459. An instruction whose answer for a NaN
  is `0` and for `1e30` is `Int.MAX` is a second answer for the inputs the
  language refuses, whatever guards stand in front of it.
- **B-ind** — x86-64's "integer indefinite", `Int.MIN` for every input it
  cannot convert, behind guards in Cove — is total and one word, and it is
  still a second answer: `Int.MIN` is also the right answer for `-2^63`, so
  the sentinel is ambiguous by construction, and the IR and the VM would have
  to reproduce one machine's convention exactly. The native tier **uses**
  `cvttsd2si` and reads its indefinite value to compute `ok` — it compares the
  operand's bits against `-2^63`'s to tell the two apart — and then writes the
  documented `0`. The sentinel stays inside that one template.

### 4. The range refusal quotes the value as Cove renders it

`Float.toInt`'s three sentences are unchanged in their words, in their order
(NaN, then an infinity, then out of range) and in being `Err` values rather
than traps. The finite one quotes the value with Cove's own `{value}`:

| value | before (Rust's `{}`) | after (`{value}`) |
| --- | --- | --- |
| `2^63` | `9223372036854776000` | `9223372036854775808.0` |
| `-2^63 - 2048` | `-9223372036854778000` | `-9223372036854777856.0` |
| `1e30` | `1000000000000000000000000000000` | `1000000000000000019884624838656.0` |

**This is an intentional diagnostic change**, and it is the part of ADR 0064's
Decision 8 this ADR supersedes. Rust's notation — shortest round-trip digits,
positional, no `.0` — is one nothing else in Cove writes, and every `Float` it
was used for here is integral, so it never agreed with the value's own
interpolation: `tests/e2e/values_float_to_int`'s `quote.*` rows printed the two
side by side for that reason. Keeping it would have meant a renderer for that
notation in Cove, 120 to 180 lines used by this one sentence. `NaN`, `inf` and
`-inf` render identically either way.

### 5. A program's own call is an unexpanded Cove call, and that cost is accepted

`std.float.toInt` returns what `toIntRefused` answers, a call whose
continuation returns rather than traps, so ADR 0070's rule does not admit it
and a program's own `x.toInt()` is a `Call`. That is accepted as it stands.
There is **no `Float.toInt`-specific short circuit in the lowering**; an
improvement, if one is wanted, has to be a general call or inlining
optimisation. `std.float`'s own three sites are not calls: they ask
`core.floatTruncate` directly, because each holds a value inside the range
already and has no use for the `Err` `toInt` would build.

## Measurement

`before` is `cce2aa1`; **commit 3** is the migration (`std.float.toInt`, the
intrinsic deleted) and **commit 4** is `std.float`'s three sites on
`core.floatTruncate`. Each was built `--profile checked --features template`
in its own target directory and every run was over one pristine tree at
`cce2aa1`, from `examples/`, with cq's inputs outside it:
`bookings-20k.jsonl` (#509's 20,000 records) for `revenue-summary` and
`confirmed-bookings`, and ADR 0046's 20,000-times `rates.csv`, 120,000 rows,
for `rate-card`. cq's output is byte-identical across the three binaries and
both tiers, per workload.

### Counters

| cq | before | commit 3 | commit 4 |
| --- | ---: | ---: | ---: |
| emitted IR / functions | 8,355 / 91 | 8,471 / 93 | 8,485 / 93 |
| `IntrinsicCall` sites | 8 | 3 | 3 |
| `Float.toInt` mediated calls | 40,006 / 86,345 / 260,000 | 0 | 0 |
| unexpanded std `Call` sites | 72 | 81 | 78 |
| compiled / refused (native) | 81 / 10 | 83 / 10 | 83 / 10 |
| machine code | 749,362 B | 764,970 B | 764,965 B |
| allocations / words, `rate-card` | 9,080,175 / 44,360,712 | +4 / +24 | +4 / +24 |

Executed VM instructions, by workload:

| workload | before | commit 3 | commit 4 |
| --- | ---: | ---: | ---: |
| `revenue-summary` | 257,229,581 | +360,054 (+0.140%) | +360,012 (+0.140%) |
| `confirmed-bookings` | 291,760,531 | +777,105 (+0.266%) | +646,270 (+0.222%) |
| `rate-card` | 469,002,075 | +2,340,000 (+0.499%) | +1,380,000 (+0.294%) |

**Every instruction of it is attributed**, by `--profile --profile-rows all`
on each binary:

- `std.float.toInt` executes **9 instructions a call** on its success path —
  the conversion, the branch, the `Some` tag and the jump that build the
  `Option`, the `switch` on it, the `Ok` built from the payload (two copies and
  a tag), and the return. The `call` that reaches it replaces the
  `intrinsic-call` one for one in its caller. Commit 3 is exactly that times
  the calls: 40,006, 86,345 and 260,000.
- Commit 4 takes `std.float`'s sites off the call. Each is now the
  conversion, the branch, the tag and the jump where it was one
  `intrinsic-call`: **+3 in `renderInto`** (20,000 on rate-card, 6,190 on
  `confirmed-bookings`) and **+2 in `format`** (120,000 and 13,385, and 6 on
  `revenue-summary`), where the `Result` the old site matched on held a
  reference and had to be cleared after the match and an `Option<Int>` does
  not.
- What is left against `before` at commit 4 is the programs' own calls — 9
  each, 1,080,000 on rate-card's 120,000 in `cq.json.renderNumber` (+0.230%) —
  and the in-range windows, 300,000 (+0.064%).

**The accepted cost of Decision 5 is +0.230% on rate-card, against the plan's
estimate of +0.15%.** The estimate assumed about six instructions a call; the
body is nine, three of them the `Option`-to-`Result` hand-over that Decision 2
puts in Cove rather than in an instruction.

On the native tier every call of `std.float.toInt` is a native-to-native direct
call, so each is an `open` and a `close` helper call where there was one
`intrinsic` helper call: rate-card's helper calls are −260,000 `intrinsic` and
+120,000 each `open` and `close` at commit 4 (+260,000 each at commit 3),
`confirmed-bookings`' −86,345 and +66,770, `revenue-summary`'s −40,006 and
+40,000. No function is newly refused: `std.float.toInt` and `toIntRefused`
compile, and the refusal path — the interpolation, `renderInto`, `format` —
runs as machine code with no crossing back to the VM. `run_copy`'s byte copies
move by +2, +76 and −10 on the three workloads, identically at commits 3 and 4,
which is the 24-word literal heap moving where a byte window's store crosses a
chunk rather than anything either commit executes.

**The allocations and words are unchanged at run time.** The +4 and +24 are the
literal heap, placed once before the run: `toIntRefused`'s whole `NaN` sentence
(57 bytes, nine words) and the three pieces the other two are interpolated
between (30, 26 and 31 bytes, five words each) — the text the Rust arm built at
each refusal, which no workload reaches.

`crates/cove-cli/tests/copies.rs`' forwardable copies rise 7,095 → 7,358 at
commit 3, one per program: `toIntRefused`'s `NaN` sentence is a `str` moved
into its `Error`, a copy straight after its producer; a per-function listing of
the standard library's counted copies names that function and no other.
Commit 4 moves nothing there. `crates/cove-runtime/tests/encoded.rs`' literal
heap rises 1,762 → 1,786 words, the same four literals.

covefmt has no `Float.toInt` site, and its IR, executed instructions, machine
code, calls and allocations are identical on all three binaries, so it is the
control.

### Wall time

Process wall time, one cold round discarded and then fifteen interleaved
rounds, the binary order rotated each round, nothing else running; seconds,
`median (min–max)`, and the paired delta against `before` with its wins:

| row | before | commit 3 | commit 4 |
| --- | ---: | ---: | ---: |
| `revenue-summary` VM | 2.309 (2.285–2.479) | 2.183, −5.45%, 15/15 | 2.186, −5.59%, 15/15 |
| `revenue-summary` native | 1.456 (1.435–1.494) | 1.481, +2.63%, 4/15 | 1.492, +1.91%, 0/15 |
| `confirmed-bookings` VM | 2.887 (2.807–3.032) | 2.715, −5.58%, 15/15 | 2.701, −6.49%, 15/15 |
| `confirmed-bookings` native | 1.839 (1.806–1.863) | 1.863, +1.00%, 4/15 | 1.868, +1.67%, 1/15 |
| `rate-card` VM | 5.685 (5.638–5.749) | 5.620, −1.18%, 12/15 | 5.622, −1.09%, 14/15 |
| `rate-card` native | 3.835 (3.726–3.942) | 3.855, +0.17%, 7/15 | 3.820, −0.18%, 9/15 |

**Every native row is inside cq's ±2.9% floor. The VM rows of
`revenue-summary` and `confirmed-bookings` are outside it, faster, and that is
the binaries and not the change.** covefmt, whose instructions, calls,
allocations and machine code are identical on all three binaries, moved the
same way in a smoke run of three rounds: −4.56% and −4.54% on the VM, +0.05%
and +0.94% on the native tier. An A/A control — `before`'s source built again
under another path — moved −0.48% and −0.43% on the two VM rows and +0.09%
and −0.85% on the two native ones. What the change itself costs is the
counters above: at most +0.29% of the VM's instructions, on `rate-card`.

`benches/rendering` interpolates a `Float` in its timed loop, which is
`renderInto`'s third site, so it had the same fifteen rounds on its own
per-rendering rows. Its `float` row went from 1,248 to 1,181 and then 1,130 ns
on the VM (−4.78%, 12/15, then −9.00%, 15/15), and from 544 to 512 and then
475 ns on the native tier (−3.78%, 10/15, then −12.10%, 15/15): a mediated
intrinsic call was dearer than the nine-instruction Cove call that replaced it
at commit 3, and dearer again than the four instructions commit 4 left. Its
nine other rows, whose counts did not move, stayed between −4.1% and +0.1%.
`benches/floatabs`, `floatminmax`, `floatround`, `floatsqrt`, `admission` and
`ordering` changed only outside their timed loops — up to 28 instructions in
the lines that print their witnesses, and their unused `std.float` sites — and
their per-operation rows are noise at fifteen rounds, their ranges wider than
any median move. The other 49 benches have identical IR and machine code, and
the seven among them whose executed count differs by a few instructions
differ inside `std.int.renderInto`, in the digits of the durations they
print (read off `--profile` for `contains`).

## Consequences

- `Intrinsic::FloatToInt` is deleted with its schema entry, its lowering, its
  oracle arm and its machine arm, and `Class::Float` and `Carried::Int` with
  it, which nothing else produced. `Float.parse` is the one intrinsic left.
- `Inst::FloatTruncate` is the two hundredth opcode, lowered on every tier and
  held to one table of 79 operands, generated from their bits by a standalone
  program, on the encoded VM and the template arm alike.
- `tests/e2e/values_float_to_int`'s golden changes in its nine finite
  out-of-range rows and nothing else; `values_float` and `values_float_abs`
  change in one row each, and `values_float_to_int_native`, which holds the
  native tier to the VM, in five.
- Follow-ups: whether a general optimisation — a match on a constructed
  `Option` folded into a branch on what built it, or a call whose continuation
  returns expanded like ADR 0070's — should take back some of Decision 5's
  nine instructions a call; neither is proposed here.
