# ADR 0072: `Float.parse` is Cove

- Status: Proposed
- Date: 2026-09-30
- Decides: that `Float.parse` is `std.float.parse`, a Cove body with **no
  instruction of its own** — a grammar scan, Clinger's fast path, and simple
  decimal conversion over a buffer of at most 768 digits for everything
  outside the fast path's box; that its grammar, its answers, its refusal and
  its signed zeros are exactly what Rust's `str::parse::<f64>` gave; and that
  a program's own call stays an unexpanded Cove call. Recorded for
  [issue #432](https://github.com/myuon/cove/issues/432). **The cost below is
  measured and not yet accepted**: myuon reviews it before this ADR is accepted
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md), whose
  Decision 2 this fulfils for the last variant (a parser is a policy over a
  representation, not a machine), whose Decision 6 asked for the corpus before
  the body, and whose Decision 8's allocation clause is read per program below;
  [ADR 0071](0071-a-checked-conversion-answers-a-value-and-whether-there-is-one.md),
  the migration before this one and the shape of its measurement;
  [ADR 0070](0070-a-call-whose-continuation-is-doomed-may-be-expanded.md),
  whose rule does not admit `std.float.parse` for expansion, because its
  refusal *returns*; and #471, `crates/cove-runtime/tests/clinger_box.rs`,
  which established that no bit-cast is needed

## Context

`Float.parse` was the last `Intrinsic` a program emitted: Rust's
`str::parse::<f64>` behind one `Inst::IntrinsicCall`, answering
`Result<Float, Error>`. On cq it is 60,000 calls on `revenue-summary`, 73,385
on `confirmed-bookings` and 240,000 on `rate-card`, from three sites —
`cq.json.parseNumber`, `cq.rows.asJson` and `cq.programs.rateOf` — and every
one of those inputs is 1 to 7 characters, at most two fraction digits, no
exponent: inside Clinger's box. It cost 141 ns a call on both tiers, of which
`dec2flt` was 12.7; the rest was the crossing and the `Result` plumbing.

#471 answered whether a correctly rounded parser needs an instruction at all,
and the answer was no: `m.toFloat() * 10^e` is exact over the box, and a
significand already rounded to its target precision times `2^e` is an exact
`Float` multiplication for every binary64, subnormals included — so no
bit-cast is needed either. What was left was to write it.

The plan (issue #432, "Plan: migrate `Intrinsic::FloatParse`") set out three
routes: (A) a checked parse instruction, (B) pure Cove, (C) pure Cove over a
narrower primitive (`mulhi`, or a correctly rounded `m × 10^e`). myuon chose
(B), with Rust's own fallback — the bounded `Decimal` — as its slow path, and
asked for the decimal-rounding argument to be made explicitly here.

## Decision

### 1. There is no parse instruction

On the native tier `dec2flt` cannot be a template: a parse instruction would
be a runtime helper call there, and the crossing ADR 0064 set out to remove
would survive under a new name — the 141 ns a call that was mostly the
crossing. And the whole-grammar variant puts policy in the instruction: the
spellings `inf`, `infinity` and `nan`, their case, a leading `+`, `.5` and
`1.` are decisions about text, which Decision 2's test ("policy is a branch on
its argument", and "would the instruction have to be renamed if the method
were") refuses. The digits-only variant, `DecimalToFloat`, is closer to a
checked conversion, but it is still a helper call natively, and everything past
19 digits would still need a Cove fallback beside it.

So `std.float.parse` is written in the instructions Cove already has: byte
reads, checked `Int` arithmetic, `Int.toFloat`, and IEEE `+ - * /` on `Float`.

### 2. The algorithm

**The grammar scan** reads one optional sign; then, if the next byte is above
`9`, one of the three words in any case (`std.float.parseWord`); otherwise
digits with at most one `.` among them and at least one digit in all, then
optionally `e` or `E`, an optional sign and at least one digit, then the end.
In the same pass it accumulates the digits into an `Int` `mantissa` that
stops taking them at `10^17` (a digit after that sets `many`), and the
exponent into an `Int` that stops growing at `10^17`. Every refusal of a
number is decided into one flag and made at one call site, so that
`cove_ir::lower::inline`, which expands a small leaf where its caller is hot,
expands the refusal's interpolation once and not five times.

**The zero** is answered first: a mantissa of nothing but zeros is `0.0` with
the sign written, at any exponent (`0e999999999999999999999` is `0.0`).

**The fast path** is taken when `!many`, `mantissa < 2^53` and the decimal
exponent `scale` (the exponent less the fraction digits) is within `±22`. Then
`mantissa.toFloat()` is exact, `10^|scale|` is one of the twenty-three exact
literals `1.0` through `1.0e22` — found by a comparison tree, three comparisons
for the one- and two-place scales cq's inputs have — and one IEEE multiply or
divide is correctly rounded, so the answer is. The sign is applied to the
operand (`mantissa.toFloat() * sign`, with `sign` `±1.0`), which is exact and
lets the answer be written straight into the `Ok`. The path makes no call and
allocates nothing; `crates/cove-ir/src/lower/tests/methods.rs`'
`std_float_parse_has_a_fast_path_that_calls_and_allocates_nothing` pins that
every instruction of `std.float.parse` is a scalar, a comparison, a branch, a
byte read or a length, or a call whose answer is returned straight away.

**Everything else is `std.float.parseSlow`**, Nigel Tao's simple decimal
conversion as Rust's `dec2flt/decimal.rs` and `slow.rs` implement it:

1. The significant digits are read into a buffer — a `Vector<Int>` of
   capacity 768, one decimal digit a word — with a decimal point `point` so
   that the value is `0.d1 d2 … dn × 10^point`, `d1` not nought, trailing zeros
   trimmed. A digit past the 768th is dropped, and `truncated` is set when one
   that was dropped is not nought. `point` then takes the exponent.
2. `point < -324` is `±0.0` and `point >= 310` is `±inf`, before a digit moves.
3. While the value is 1 or more it is divided by `2^s`, `s` at most 59; while
   it is below one half it is multiplied by `2^s`, `s` at most 25; `exp2`
   counts. **A multiplication by `2^s` is a division by `5^s` followed by a
   move of the point by `s`** (`x·2^s = (x / 5^s)·10^s`), so both directions
   are one routine, `std.float.divided`: one pass of long division from the
   most significant digit, in place. That is the one departure from Rust,
   which multiplies from the least significant digit and needs a 1,308-byte
   table of powers of five to know how many digits the product has; the
   division needs no table. Its bounds are Cove's, not `u64`'s — `2^59` and
   `5^25` are the largest divisors `d` for which `10·d + 9` stays below
   `2^63`, where Rust's is `2^60`.
4. The value is then `2x × 2^(exp2-1)` with `2x` in `[1, 2)`. A binary
   exponent below −1022 is a subnormal, and the digits are divided by two
   until it is −1022; `exp2 >= 1024` is `±inf`.
5. Fifty-three bits up (`5^25`, `5^25`, `5^3` and the point), and
   `std.float.rounded` takes the whole part and rounds it: up past a half, and
   on a digit `5` that is the last digit held and has nothing dropped behind it
   (`truncated` false), to even. A significand that rounded up to `2^53` is
   halved **from the digits** and rounded again, and `exp2` is checked for
   overflow again.
6. `significand × 2^(exp2-52)`, by a ladder of exact multiplications or
   divisions by `2^32`, `2^16`, `2^8`, `2^4`, `2^2` and `2`, times `sign`.

**Eisel–Lemire, Rust's middle tier, is omitted.** It needs a 64×64→128-bit
multiply, shifts and a count of leading zeros, none of which Cove has — each
product would be thirty-bit limb arithmetic, hundreds of instructions — and a
table of about 650 128-bit powers of five, some 1,300 words of literal heap in
every program. It exists to be fast on 17- to 19-digit inputs, and no workload
in this repository has one. What a 17-digit input costs without it is in the
slow-path table below.

### 3. Why this rounds correctly: the argument

Write `T` for the exact value of the decimal (positive; the sign is applied
last, and rounding to nearest is symmetric). The claim is that
`std.float.parse` answers the binary64 nearest `T`, ties to even, for every
text its grammar accepts.

#### 3.1 The fast path

For `mantissa < 2^53` the `Int` converts to a `Float` exactly, and for
`|scale| <= 22` the power `10^|scale|` is exact (`5^22 < 2^53`). IEEE 754
correctly rounds each of `*` and `/`, so `m · 10^s` and `m / 10^s` are the
nearest `Float` to `T`, ties to even. `clinger_box.rs` swept all 3,157,200
pairs of that box against Rust and showed the box is tight: one step past any
edge, a quarter of mantissas are one ulp off, which is why the fast path stops
exactly there. Multiplying by `sign` first is exact.

#### 3.2 Why 768 digits are enough

A binary64 value is `m · 2^e` with `m < 2^53` and `e >= -1074`. The midpoint
between two neighbours is `k · 2^q` with `k = 2m + 1` odd, `k < 2^54`, and
`q >= -1075`. If `q >= 0` it is an integer below `2^1024 < 10^309`: at most
309 digits. If `q < 0` it is `k · 5^(-q) / 10^(-q)`, whose significant digits
are those of the integer `k · 5^(-q)` — which has no trailing zero, because
it is odd. And `k · 5^(-q) < 2^54 · 5^1075`, whose base-10 logarithm is
`54·0.30103 + 1075·0.69897 = 16.256 + 751.393 = 767.649`. So **every midpoint
has at most 768 significant digits**, and 768 is attained: `k = 2^53 + 1` at
`q = -1075`, the midpoint just above the least normal value `2^-1022`, is at
least `10^767.347` and has exactly 768; so does every subnormal midpoint with
`k >= 4,048,045,066,146,213`. (A binary64 value itself has at most 767:
`m · 5^1074 < 2^53 · 5^1074 < 10^766.65`. That is the "767 digits" Rust's
comment on `MAX_DIGITS` states, and it is why Rust stores 768 — "the max
digits + 1". The plan's sentence that a midpoint has at most 767 was the
value's bound, not the midpoint's; the buffer is exactly the midpoint's.)

What the buffer does with that is a monotonicity argument. Let `G` be the set
of numbers with at most 768 significant digits, and `⌊y⌋_G` the largest member
of `G` not above `y` — which is exactly what keeping the first 768 digits of
`y` and dropping the rest computes. Three properties are all it needs:

- `⌊y⌋_G <= y`, with equality exactly when `y` is in `G`;
- it is monotone;
- **for `g` in `G`, `y >= g` exactly when `⌊y⌋_G >= g`.**

The slow path is a chain of such truncations between exact scalings:
`x_0 = ⌊T⌋_G`, then `x_i = ⌊c_i · x_{i-1}⌋_G` with each `c_i` a power of two.
Take any threshold `r` and its images `r_i = c_i ⋯ c_1 · r`. If every `r_i` is
in `G`, induction on the third property gives `x_n >= r_n` exactly when
`T >= r`. And `truncated` is exactly "`x_n < T · c_n ⋯ c_1`": it is set at the
first step that drops a nonzero digit, from which point the held value is
strictly below the true one and stays so under exact scaling, and a step that
drops only zeros changes nothing. So the pair `(x_n, truncated)` answers
**`T < r`, `T = r` or `T > r`** for every such threshold: below when
`x_n < r_n`; equal when `x_n = r_n` and nothing was dropped; above otherwise.

The thresholds the answer depends on are the binary64 values and the midpoints
(the final significand `M` is `⌊T · 2^J⌋`, decided against `M` and `M + 1`,
which are binary64 values or `2^53` of the next binade or `2^1024`; the
rounding is decided against `M + ½`, a midpoint), and the powers of two the
normalisation compares against (`½` and `1` in its frame). Each has at most
768 digits in the frame of the text, by the count above (`2^1024` has 309, and
the powers of two near a value that is not already zero or infinity by
step 2 — `2^k` with `k >= -1080` — have at most 756). In every other frame
the slow path passes through, a threshold near the held value `x` is `k · 2^p` with `k` odd below `2^54`: an integer below
`10^310` when `p >= 0`, and otherwise the digits of `k · 5^(-p)`, at most
`log10(r) - p + 1` with `-p <= 54 - log2(r)` — so at most
`55 + 0.7 · max(0, -log2 r)`, and the frames' values are
bounded below: step 3's divisions stop at a tenth and its multiplications
start at `T` and only grow, step 4 goes down by at most 59 bits from a half
(`T >= 10^-325` puts `exp2 >= -1080`), and step 5 goes up. So every threshold
is in `G` in every frame, and the answer is the right one:

- `M` is right: `x_n < M + 1 = r_n` says `T · 2^J < M + 1`, and
  `x_n >= M` says `T · 2^J >= M`.
- the rounding is right: a digit after the point above `5`, or a `5` with
  another digit after it (the last digit held is never nought, because every
  step trims trailing zeros), is `x_n > M + ½`, so `T > M + ½`; a `5` that is
  the last digit is `x_n = M + ½`, which is `T > M + ½` when `truncated` and a
  tie otherwise; anything else is `x_n < M + ½`, so `T < M + ½`.

**The `truncated` flag and halfway cases**, then, in one sentence: a dropped
nonzero digit means the true value is strictly above the kept prefix, so a
kept prefix that is exactly a midpoint must round up and never to even; and a
kept prefix that is not a midpoint rounds the same way with or without the
dropped tail, because every midpoint is a 768-digit number and the prefix is
on the same side of it as the value.

#### 3.3 Extreme exponents

The exponent stops growing once it reaches `10^17`, so it is held at most
`10^18 + 9`, and `point`'s own part is at most the text's length in magnitude.
If the written exponent is at least `10^17` in magnitude, the value with a
nonzero digit has `|point| >= 10^17 - length`, which for any text shorter than
`10^17 - 400` bytes is past both step 2 bounds on the same side as the true
`point` — and the true `point` is further out still — so both are `±inf` or
both `±0.0`. A text of `10^17` bytes, a hundred petabytes, is not one a heap
holds; that is the one assumption. A mantissa of zeros is zero whatever the
exponent, and is answered before the exponent is used.

Every `Int` in the body is bounded so that nothing can trap: `mantissa`
`< 10^18`; the exponent `<= 10^18 + 9`; `point` and `scale` within the text's
length of that, below `2^63`; the long division's remainder below
`10 × divisor <= 5.8 × 10^18`; the significand below `10^17`; and `exp2`
within about `±1100`. None of these is a deviation from the algorithm that
changes an answer: the 59- and 25-bit steps only change how many passes there
are, and the argument above holds for any sequence of steps.

#### 3.4 Subnormals and the least normal value

Rounding happens once, at the precision of the answer's own exponent: step 4
moves a subnormal to the frame whose unit is `2^-1074` before step 5 rounds,
so a subnormal is never rounded at 53 bits and then again. The least normal
value `2^-1022` and the largest subnormal share that unit — the normal binade
at the bottom has the same spacing as the subnormals — so a subnormal whose
significand rounds up to `2^52` is exactly `2^-1022`, the least normal value,
and nothing more is needed. The assembly `significand × 2^(exp2-52)` is exact
because the significand is already rounded to the answer's precision: every
intermediate of the ladder is `significand × 2^j` with `j >= -1074`, which is
a multiple of `2^-1074` with at most 53 significant bits and below `2^1024`,
so representable (`clinger_box.rs`'
`a_power_of_two_reaches_every_binary64_without_a_bit_surface`).

#### 3.5 How the tests reach each edge

`crates/cove-runtime/tests/float_parse.rs` holds the answer to Rust's
`str::parse::<f64>` bit for bit — a `NaN` as "is `NaN`", a refusal by its
message — on the VM, and under `template` on the native tier. Since this
change it is an oracle the subject does not ship.

- **The 768-digit bound and `truncated`**: the midpoints of nineteen `Float`s
  (normal, subnormal, both sides of `2^-1022`, next to `MAX`) written with 17,
  19, 20, 40, 767, 768 and 769 significant digits and one unit either side;
  the midpoint followed by `0…01` at digits 769, 770, 800 and 1000; the same
  padded with zeros past 768; and 1 to 800 random digits.
- **Halfway cases**: those midpoints exactly, which are ties, and
  `values_float_parse`'s `tie.*` and `wide.*` rows.
- **Extreme exponents**: exponents at `2^31`, `2^63`, `10^17`, `10^20` and
  `2^64`, both signs; mantissas of 300 to 5,000 leading or trailing zeros that
  an exponent cancels; `0e999999999999999999999`.
- **Subnormals**: the least subnormal and its half, as their shortest
  spellings, their neighbours and their exact 751- and 752-digit expansions;
  the largest subnormal and least normal value, their neighbours and midpoints;
  2.2250738585072011e-308 and its neighbours.
- **Round-trips**: random bit patterns spelled `{:e}`, shortest-positional and
  with 17 digits.

The default run is 1,105 adversarial and 3,500 random inputs on each tier;
the four ignored cases are 100,000 more on the VM.

### 4. The refusal is one sentence, returned

`` `{text}` is not a Float ``, the input quoted byte for byte, as an `Err`
value — the Rust arm's sentence exactly, which `values_float_parse`'s `msg.*`
rows pin as bytes. It is `std.float.parseRefused`, a function of its own for
`std.int.refuseInt`'s reason, and reached from one site in `parse` and one in
`parseWord`. It is not ADR 0067's `core.refuse`: which text arrived was the
data's business, not the call's.

### 5. What it allocates

- **The fast path allocates nothing**, and every input in this repository's
  workloads takes it.
- **The slow path allocates one digit buffer**: `core.vectorWithCapacity(768)`,
  a vector's header and its 768-word store, two objects and 772 words, once a
  call. Nothing else; the state is `Int`s and a `Bool`, and `divided` writes
  through `var` parameters.
- **A refusal allocates its message**, as the Rust arm did.
- **The literal heap** gains the three literals the body holds that no program
  already had — `` ` is not a Float ``, `nan` and `infinity` — seven words,
  placed once before the run.

ADR 0064's Decision 8 asks that allocations not regress. On every workload in
this repository they do not: execution allocations and words are identical to
the instruction on all three cq workloads, and the +3 allocations and +7 words
are the literals. **Per input they do** — a 17-digit or 800-digit text
allocates the buffer where the Rust arm allocated nothing — and that is
reported below rather than gated, as issue #536's D4 proposed and the decision
record asked: judged per program on the workloads, reported per input class.

### 6. A program's own call is an unexpanded Cove call

`Float.parse(text)` lowers to `call std.float.parse` where it was one
`intrinsic-call`, one for one. The body is far past the inliner's limits, and
its refusal returns rather than traps, so ADR 0070's rule does not admit it;
the cost of the call is in the measurements.

### 7. What this change does not delete

`Intrinsic`, its `FloatParse` variant, `Inst::IntrinsicCall`, the native
helper protocol and `--boundary`'s mediated-intrinsic tables are the
mechanism, and deleting it is a separate change with its own ADR (ADR 0064's
Decision 1 and its header). No program emits an intrinsic call any more.
**The VM's `FloatParse` arm stays with the mechanism**, although the plan
listed it for deletion here: the cases that hold the mechanism's reporting to
account — `vm::report`'s and `native_tier.rs`'s — need an intrinsic that
runs, answering `Ok` without allocating and `Err` with one allocation, and
they now put the call back into lowered IR by hand. The interpreter's arm
and the `ASSOCIATED` route are deleted.

## Measurement

`before` is main at `c709a9e`; `after` is this change's second commit
(`std.float.parse`, the intrinsic route and the interpreter's arm deleted).
Each was built `--profile checked --features template` in its own target
directory, and every run was over one pristine worktree of `c709a9e`, from
`examples/`, with nothing else running: `bookings-20k.jsonl` (#509's 20,000
records) for `revenue-summary` and `confirmed-bookings`, and ADR 0046's
`rates.csv` 20,000 times over, 120,000 rows, for `rate-card`. cq's output is
byte-identical across the two binaries and both tiers, per workload.

### Counters

| cq | before | after |
| --- | ---: | ---: |
| emitted IR / functions | 8,485 / 93 | 9,443 / 97 |
| `IntrinsicCall` sites | 3 | **0** |
| unexpanded std `Call` sites | 78 | 87 |
| compiled / refused (native) | 83 / 10 | 87 / 10 |
| machine code | 764,965 B | 843,717 B |

Executed VM instructions, by workload, and what executed them:

| workload | calls | before | after | `std.float.parse` a call |
| --- | ---: | ---: | ---: | ---: |
| `revenue-summary` | 60,000 | 257,589,593 | +4,733,253 (+1.84%) | 78.9 |
| `confirmed-bookings` | 73,385 | 292,406,801 | +6,521,921 (+2.23%) | 88.9 |
| `rate-card` | 240,000 | 470,382,075 | **+31,400,000 (+6.68%)** | 130.8 |

**Every instruction of the difference is `std.float.parse`'s**, by
`--profile --profile-rows all` on both binaries: the `call` at each site
replaces the `intrinsic-call` one for one, the callers' own counts do not
move, and `parseSlow`, `parseWord` and `parseRefused` never run. By site:
`cq.json.parseNumber` (60,000 calls on each bookings workload, one- to
five-character numbers) is 78.9 instructions a call; `cq.rows.asJson`
(13,385 on `confirmed-bookings`, 120,000 on `rate-card`) and
`cq.programs.rateOf` (120,000 on `rate-card`), which parse the same six- and
five-character rates, are 130.8, and `asJson` on `confirmed-bookings`' 21
values 133.6. The digit loop is about eleven instructions a byte; the rest — the sign,
the refusal flag, the zero test, the box test, the tree, the operation and
the `Ok` — is fixed.

On the native tier every call is a native-to-native direct call, so each is an
`open` and a `close` helper call where there was one `intrinsic` helper call:
−240,000 `intrinsic` and +240,000 each of `open` and `close` on `rate-card`,
−73,385 and +73,385 on `confirmed-bookings`, −60,000 and +60,000 on
`revenue-summary`. **No function is newly refused**: `parse`, `parseSlow`,
`parseWord` and `parseRefused` all compile (83 → 87 compiled, 10 refused
before and after), and no call crosses back to the VM.

**Allocations and words at run time are unchanged, to the object.** By
`--profile`, which counts what the run allocates: 1,413,455 / 9,170,074 on
`revenue-summary`, 2,425,746 / 14,928,501 on `confirmed-bookings` and
9,080,021 / 44,360,076 on `rate-card`, on both binaries. `--stats`' totals
are +3 allocations and +7 words on each workload, and those are the literal
heap, placed once before the run: `` ` is not a Float `` (16 bytes, three
words), `nan` and `infinity` (two each); the refusal's opening backtick is
already `std.int.refuseInt`'s and `inf` already `std.float.format`'s.
`crates/cove-runtime/tests/encoded.rs`' literal heap rises 1,786 → 1,793 words,
the same three.

`crates/cove-cli/tests/copies.rs`' forwardable copies rise 7,358 → 7,392 with
the first commit, all of it `values_float_parse_native` existing (no compiler
changed), and 7,392 → 7,399 with the second, over the same 264 programs:
+2 each in `examples:cq`, `examples:cqSample` and `examples:covecheck` — the
one refusal site in `parse` and the one in `parseWord`, whose
`parseRefused` `cove_ir::lower::inline` expands because the parse runs in a
loop there (`std.int.parse` has six of the same in the same programs) — and +1
in `values_float_parse_native`, whose helper returns `Float.parse(text)`
straight away and gets a call's answer copied where the intrinsic call wrote
it. `parse` funnels its refusals to one site for this reason: written as a
`return` at each of five, it was +6 in each of the three programs.

covefmt makes no `Float.parse` call and is the control: its output, `--stats`
and `--boundary` are identical on the two binaries on both tiers but for
native's count of unreached declarations, 889 → 897, the eight new functions of
`std.float`; and VM and native print the same bytes.

### Wall time

Process wall time, one cold round discarded and then fifteen interleaved
pairs, the order alternated each round; seconds, `median (min–max)`, and the
median of the fifteen paired deltas with the rounds `after` won:

| row | before | after | paired delta |
| --- | ---: | ---: | ---: |
| `revenue-summary` VM | 2.187 (2.166–2.226) | 2.268 (2.250–2.368) | +4.01%, 0/15 |
| `revenue-summary` native | 1.493 (1.452–1.534) | 1.480 (1.464–1.554) | +0.02%, 7/15 |
| `confirmed-bookings` VM | 2.717 (2.688–2.742) | 2.805 (2.793–2.865) | +3.53%, 0/15 |
| `confirmed-bookings` native | 1.872 (1.820–1.978) | 1.848 (1.811–1.875) | −0.88%, 9/15 |
| `rate-card` VM | 5.624 (5.563–5.859) | 5.827 (5.786–5.936) | **+3.82%, 0/15** |
| `rate-card` native | 3.821 (3.686–3.915) | 3.800 (3.752–3.857) | −0.32%, 9/15 |

**The three VM rows are outside cq's ±2.9% floor and slower, and that is the
change**: covefmt, the control, moved −0.28% on the VM (3/5) and +0.84% on the
native tier (1/5) in a smoke run of five pairs, so the binaries do not account
for it. The native rows are inside the floor.

### The fast path and the slow path, per input class

A scratch package calls `Float.parse` on one text in a callee's loop and on
the same text in a bare loop, and the difference is the parse. Instructions,
allocations and words are per call on the VM; nanoseconds are the median of
five rounds, on the VM and on the native tier, with `before`'s in
parentheses. `before` was one `intrinsic-call` and the helper whatever the
text: four or five VM instructions a call in this loop, where `after` counts
the loop's same few too.

| class | text | VM instr. | allocs / words | VM ns | native ns |
| --- | --- | ---: | ---: | ---: | ---: |
| cq-shaped | `109.00` | 138 | 0 / 0 | 514 (128) | **95** (129) |
| 17-digit round-trip | `1.2345678901234567` | 3,190 | 2 / 772 | 20,712 (152) | 4,511 (147) |
| 19 digits | `1234567890123456789` | 6,024 | 2 / 772 | 41,256 (156) | 7,951 (145) |
| 20 digits | `12345678901234567891` | 6,307 | 2 / 772 | 42,806 (179) | 8,307 (181) |
| e = 23, outside the box | `1e23` | 4,729 | 2 / 772 | 32,321 (125) | 6,491 (124) |
| e = −23 | `1e-23` | 4,187 | 2 / 772 | 26,778 (145) | 5,504 (132) |
| subnormal | `4.9406564584124654e-320` | **155,590** | 2 / 772 | **1,333,297** (164) | 211,661 (156) |
| halfway, 40 digits | `9007199254740993.000…` | 6,102 | 2 / 772 | 40,180 (590) | 7,735 (585) |
| 768 digits | `wide.mid` padded | 31,135 | 2 / 772 | 123,094 (1,754) | 26,451 (1,732) |
| 800 digits | the same, `…01` | 36,995 | 2 / 772 | 159,952 (2,321) | 32,041 (2,320) |
| huge exponent | `1e99999999999999999999` | 375 | 2 / 772 | 2,228 (161) | 999 (164) |
| refusal | `1.0x` | 127 | 2 / 9 | 735 (266) | 374 (263) |

`before` allocated nothing for any number and one object of four words for a
refusal. The slow path's two objects are the one buffer, a vector's header and
its 768-word store; the refusal's two are the interpolation's builder and the
message it finishes. **No workload in this repository reaches any row but the
first.** (The instruction column is the final body's. The nanoseconds were
taken one revision earlier, before `parseSlow`'s upward loop was restated so
that covefmt and `cove fmt` agree on it — covefmt prints an `else if` whose
condition compares against a negative literal differently — which moved
`1e-23` by −6 instructions and the subnormal by −45 and nothing else; the
workload counters above are the same on both.)

The slow path's cost is the digits times the distance: each pass of
`divided` is a few instructions a digit, a value near `10^-320` needs about
forty-three of them to climb a thousand bits at twenty-five a pass, and its
digits grow as it climbs. That is simple decimal conversion's own shape — Rust
pays it too, on the inputs its Eisel–Lemire tier cannot decide — at Cove's
prices, and it is three to four orders of magnitude over the helper call on
the VM. It is reported here, not gated.

### Estimated against measured

The plan estimated, for `rate-card`, **+5 to +8% VM instructions**, **+1.6 to
+2.7% VM wall** and **−0.6% native wall**, from 100 to 150 instructions a
call. Measured: **+6.68%** instructions at 130.8 a call, inside the estimate;
**+3.82%** VM wall, over it and outside cq's floor, at about 846 ns a call
more — the plan's 5.1 ns an instruction was `std.int.parse`'s mix, and this
body's is dearer (`revenue-summary`'s 79 instructions a call cost about
1,350 ns); and **−0.32%** native, inside the floor. The cq-shaped parse is
95 ns natively against the plan's 25 to 40 and the helper's 129: faster than
the crossing it replaces, by a quarter rather than by three quarters.

**This cost is measured and is not accepted by this ADR.** What is asked of
review is the VM's +3.5 to +4.0% on the three cq workloads, the native tier's
parity, and the slow path's per-input price in the table above.

## Consequences

- `Float.parse` is correctly rounded by an argument written down here rather
  than by trust in a library, and held to Rust bit for bit by a test that no
  longer runs Rust on the subject's path.
- No program emits `Inst::IntrinsicCall`. The mechanism has no producer, which
  is what its deletion waits on.
- A text past Clinger's box costs thousands to hundreds of thousands of VM
  instructions and one 772-word buffer where it cost one helper call; no
  workload in this repository has one. If one does, the lever is a middle tier
  over a wider multiply, which ADR 0064's Decision 2 would have to admit first.
