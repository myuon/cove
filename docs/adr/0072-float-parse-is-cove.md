# ADR 0072: `Float.parse` is Cove

- Status: Proposed
- Date: 2026-09-30
- Decides: that `Float.parse` is `std.float.parse`, a Cove body with **no
  instruction of its own** — a grammar scan, Clinger's fast path, an
  Eisel–Lemire middle tier in thirty-bit limbs over a table of powers of five
  held as one string literal, and simple decimal conversion over a buffer of
  at most 768 digits for what the middle tier cannot decide; that its
  grammar, its answers, its refusal and its signed zeros are exactly what
  Rust's `str::parse::<f64>` gave; and that a program's own call stays an
  unexpanded Cove call. Recorded for
  [issue #432](https://github.com/myuon/cove/issues/432). **The cost below is
  measured and not yet accepted**: myuon reviews it before this ADR is
  accepted. Two parts of it are already decided by myuon, and are recorded as
  such: the middle tier uses existing instructions only, and its table's
  +1,222 words of literal heap in every program that reaches `Float.parse` are
  accepted
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

This ADR was first written for #544, which shipped (B) with no middle tier.
It is still `Proposed`, and it is revised here rather than superseded: a
prototype evaluation of middle tiers over existing instructions found an
Eisel–Lemire product in thirty-bit limbs correct, allocation-free and an
order of magnitude cheaper than the slow path on every input past Clinger's
box but the longest, and a reshaped scan about a fifth cheaper on the inputs
inside it. myuon decided to adopt both with **existing instructions only**,
to accept the table's literal heap, and to record them here with fresh
measurements. The history of the numbers — before #544, #544, the scan (F),
the middle tier (M1) — is kept in the measurements.

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
That holds for the middle tier too, and the last part of §2 says why a
wider multiply was not added for it.

### 2. The algorithm

**The grammar scan** reads one optional sign; then, if the next byte is above
`9`, one of the three words in any case (`std.float.parseWord`); otherwise
digits with at most one `.` among them and at least one digit in all, then
optionally `e` or `E`, an optional sign and at least one digit, then the end.
In the same pass it accumulates the digits into an `Int` `mantissa` that
stops taking them at `10^17` (a digit after that is counted in `dropped`),
and the exponent into an `Int` that stops growing at `10^17`. Every refusal of
a number is decided into one flag and made at one call site, so that
`cove_ir::lower::inline`, which expands a small leaf where its caller is hot,
expands the refusal's interpolation once and not five times.

The scan's shape is the one that is cheapest per digit, and that is all that
distinguishes it from the one #544 shipped (the answers, the grammar and every
refusal are the same, which the goldens and `float_parse.rs` hold): the first
byte is read once and serves the sign and the word test; **the first eighteen
digit bytes are taken with no test on `mantissa`** — eighteen digits are below
`10^18` whatever they are, and before each of them the `10^17` rule would
have taken it anyway, because `k` digits are below `10^k` — and only a longer
run goes on to a loop that asks; the whole part and the fraction are loops of
their own, so neither the point nor where it was is a question asked of every
digit; and the box's three tests are nested so each is one fused
compare-and-branch. A digit costs nine VM instructions where it cost thirteen.

**The zero** is answered first: a mantissa of nothing but zeros is `0.0` with
the sign written, at any exponent (`0e999999999999999999999` is `0.0`).

**The fast path** is taken when `mantissa < 2^53` and the decimal exponent
`scale` (the exponent less the fraction digits) is within `±22` — a mantissa
that stopped taking digits is at least `10^17`, past `2^53`, so the first test
says that it did not. Then `mantissa.toFloat()` is exact, `10^|scale|` is one of the twenty-three exact
literals `1.0` through `1.0e22` — found by a comparison tree, three comparisons
for the one- and two-place scales cq's inputs have — and one IEEE multiply or
divide is correctly rounded, so the answer is. The sign is applied to the
operand (`mantissa.toFloat() * sign`, with `sign` `±1.0`), which is exact and
lets the answer be written straight into the `Ok`. The path makes no call and
allocates nothing; `crates/cove-ir/src/lower/tests/methods.rs`'
`std_float_parse_has_a_fast_path_that_calls_and_allocates_nothing` pins that
every instruction of `std.float.parse` is a scalar, a comparison, a branch, a
byte read or a length, or a call whose answer is returned straight away.

**Everything else is `std.float.parseMiddle`**, the middle tier, which `parse`
calls in tail position with what the scan knows — where the digits start and
how many there are, the sign, `mantissa`, `dropped`, `q = scale + dropped` and
the saturated exponent — so the grammar is never read twice:

1. `q < -342` is `±0.0` and `q > 308` is `±inf`, before anything else.
2. `std.float.eiselLemire(mantissa, q)` answers `mantissa × 10^q` correctly
   rounded, or `NaN` when it cannot tell. It is Eisel and Lemire's algorithm
   (Rust's `dec2flt/lemire.rs`) with Cove's arithmetic: the mantissa shifted
   to `w` in `[2^59, 2^60)` by a ladder of six compare-and-multiplies in place
   of a count of leading zeros; `F`, the truncated `5^q × 2^-g` in
   `[2^89, 2^90)`, read from the table; `L = w × F` exactly, in five
   thirty-bit limbs from six thirty-by-thirty products, where Rust takes the
   top of a 64×128 product; the top fifty-four bits of `L` as a significand
   and its round bit; Lemire's test for whether the product's error could
   carry into them; the subnormal rounding; an exact-division case for short
   fractions; and the significand times `2^e` by a ladder of exact
   multiplications (`std.float.timesPowerOfTwo`). §3.2 is the argument.
3. **A dropped digit that is not nought** — found by reading the dropped
   digits backwards from the last, which are the last `dropped` digits before
   the exponent, and stopping at the first that is not `0` — asks the same of
   `mantissa + 1`, and the answer stands only when both are the same `Float`.
4. Anything left is `std.float.parseSlow`'s, called in tail position with the
   digits' start, the sign and the exponent: it reads the digits again into
   its buffer, which it has to, and nothing else.

The table is one **string literal** of 9,765 bytes: for each `q` from −342 to
308, `F`'s three thirty-bit limbs as fifteen bytes of six bits above `'0'`
(`'0'` to `'o'`, so no byte is a quote or an interpolation brace), read with
`byteAt`. `g = floor(q × log2 5) − 89` is not stored: it is
`(217706 q + 78643200) / 65536 − 1200 − q − 89`, whose numerator is positive
over the range so that `/` is a floor, checked against the exact value for
all 651 `q`. The literal is placed once before the run, like every literal
(ADR 0045): **1,222 words and one allocation of literal heap in every
program that reaches `Float.parse`**, which myuon accepted. A whole-package
lowering places every function's literals, so there it is every program:
`encoded.rs`' fixture rises 1,793 → 3,015 words, and
`vm::differential`'s collector-stress cases add the 1,222 words to the
4,096-word heap their floors on collections were set against.

The middle tier allocates nothing, and every function of it compiles
natively: `crates/cove-ir/src/lower/tests/methods.rs`'
`std_float_parse_has_a_middle_tier_that_allocates_nothing` pins that each of
its instructions is a scalar, a comparison, a branch, a byte read, a length, a
literal, a call within the tier, or the one tail call of the slow path; and
`float_parse.rs` and `native_tier.rs` assert that `parseMiddle` and
`eiselLemire` are compiled.

**What the middle tier cannot decide is `std.float.parseSlow`**, Nigel Tao's
simple decimal conversion as Rust's `dec2flt/decimal.rs` and `slow.rs`
implement it:

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

**The middle tier has no instruction of its own, and that is a decision.**
When this ADR was first written (#544) it omitted Eisel–Lemire, on the
estimate that thirty-bit limb arithmetic would cost hundreds of instructions
a product and that a table would cost about 1,300 words of literal heap in
every program; the slow path then cost 3,190 VM instructions and a 772-word
buffer on a 17-digit round-trip. Prototyped and measured (#432's middle-tier
evaluation), the limb product is 23 VM instructions and the whole tier about
160, and the table is 1,222 words — so it was added, at the price myuon
accepted, with **existing instructions only**. What a wider multiply would buy
was estimated from the tier's listing rather than built:

| part of the tier | VM instr. now | with `mulhi`, `mullo`, shifts, `clz` | and a word table and a bit-level assembly |
| --- | ---: | ---: | ---: |
| normalise (six-step ladder) | ~13 | 2 | 2 |
| table read (15 `byteAt` and the decode) | **59** | ~35 | 2 |
| product (6 multiplies, carries) | 23 | 4 | 4 |
| extract, `g`, `e`, rounding, ambiguity | ~40 | ~15 | ~15 |
| `significand × 2^e` (ladder) | ~25 | ~25 | ~3 |
| **the tier** | **~160** | **~80** | **~30** |

Two things decide it. **Storage dominates**: the table's decode is 59 of the
tier's ~160 instructions and the product only 23, so a multiply instruction
alone buys about 2×, and the other 2–3× needs the table to be words rather
than text and a way to build a `Float` from bits — neither of which is a
multiply. And **Cove's `Int` is signed and trapping**: Lemire's product is an
unsigned, wrapping 64×64 multiply of a mantissa normalised to `w >= 2^63`,
and the 19-digit mantissa Rust keeps is not an `Int` at all, so the
operations would have to be defined on bit patterns — a new kind of `Int`
operation, which ADR 0064's Decision 2 would have to admit for a 2–5× on
inputs that are already an order of magnitude cheaper than they were. The
tier keeps eighteen digits (`mantissa < 10^18`, so `mantissa + 1` fits the
normalisation) where Rust keeps nineteen, and pays for it only on inputs whose
nineteenth digit decides a boundary, which then fall back.

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

#### 3.2 The middle tier

Write `m` for `mantissa` and `q` for `scale + dropped`. The scan guarantees
`1 <= m < 10^18`, and `T = m · 10^q` when every dropped digit is nought,
`m · 10^q < T < (m + 1) · 10^q` when one is not. Write `X = m · 10^q` for the
value the tier rounds (and, in step 7, `(m + 1) · 10^q`); `m <= 10^18`
throughout.

1. **Range.** `q < -342` gives `T < (m + 1) · 10^q <= 10^18 · 10^-343 =
   10^-325`, below `2^-1075` (half the least subnormal, about
   `2.47 · 10^-324`), so the answer is `±0.0`. `q > 308` gives
   `T >= 10^309 > 2^1024`, past `MAX + ½ ulp`, so it is `±inf`. Inside, the
   table has an entry for every `q`.
2. **The table.** `g = floor(q · log2 5) − 89` puts `5^q · 2^-g` in
   `[2^89, 2^90)`, and `F = floor(5^q · 2^-g)` is in the same interval. `F` is
   exact exactly when `5^q · 2^-g` is an integer: for `q >= 0` that is
   `g <= 0`, which is `q <= 38` (`5^38 < 2^89 < 5^39`); for `q < 0` never,
   since `5^|q|` does not divide a power of two. So
   `F = 5^q · 2^-g` for `0 <= q <= 38`, and `F < 5^q · 2^-g < F + 1`
   otherwise. All 651 entries, the formula for `g` (a positive numerator, so
   truncating division is the floor) and the exact range were checked against
   exact rationals when the table was generated and again, independently,
   against the literal as it stands in `std/float.cove`.
3. **The product is exact and cannot trap.** `w = m · 2^s` is in
   `[2^59, 2^60)` (the ladder's six steps each leave `w` below `2^60` and
   raise its floor to `2^28`, `2^44`, `2^52`, `2^56`, `2^58`, `2^59`). `w`'s
   two limbs and `F`'s three are below `2^30`, every partial product is below
   `2^60`, every column sum below `2^61 + 2^31`: no `Int` operation traps, and
   `L = w · F` in `[2^148, 2^150)` exactly.
4. **The enclosure — the product's error bound.** `X = Y · 2^(g + q − s)`
   with `Y = w · 5^q · 2^-g`. When `F` is exact, `Y = L`. Otherwise
   `L = w · F < Y < w · (F + 1) = L + w < L + 2^60`: **the truncated table
   costs less than `2^60` in `L`'s units**, which is `2^-35` of the unit of
   the significand's round bit.
5. **Rounding, and why the ambiguity test is conservative.** Let `b` be 96
   when `L >= 2^149` and 95 otherwise, so `N = floor(L / 2^b)` is in
   `[2^53, 2^54)`; the answer is `Y / 2^(b+1)` rounded to an integer, in
   units of `2^e` with `e = b + 1 + g + q − s`, and `N` is that quantity's
   integer part doubled plus its round bit.
   - **`F` exact** (`0 <= q <= 38`): `Y = L`, and the rounding is read off
     `L`: `N` even rounds down; `N` odd rounds up unless every bit of `L`
     below `b` is nought, which is an exact tie, and then to the even
     neighbour. That is round-half-to-even of the exact value.
   - **Otherwise**, if the bits of `L` from 60 to `b − 1` are not all ones,
     then `L mod 2^b <= 2^b − 2^60 − 1`, so `N · 2^b <= L < Y <
     L + 2^60 <= (N + 1) · 2^b`: `Y / 2^b` is strictly between `N` and
     `N + 1`. It is then never a tie and never a boundary, and the answer is
     `floor(N / 2) + (N mod 2)`. When those bits *are* all ones the error
     might carry into `N`, and the tier answers `NaN`. That test does not
     look at the error, only at whether an error below `2^60` could reach the
     round bit — so it declines some inputs it could have decided, and
     never answers one it could not.
   - The significand is in `[2^52, 2^53]`; `2^53` is the next binade's
     `2^52`, and the assembly below represents it exactly.
6. **Exact ties.** Every midpoint between two binary64 values is
   `k · 2^p` with `k` odd below `2^54`. For `q > 38`, `X`'s odd part is a
   multiple of `5^q >= 5^39 > 2^90`, so `X` is never a midpoint (nor a
   binary64 value). For `q <= −26`, `X = m · 2^q / 5^|q|` with
   `m < 10^18 < 5^26`, so it is not a dyadic rational at all. So the ties the
   tier can meet are at `0 <= q <= 38`, where `F` is exact and the tie rule
   above decides them — `1e23` is one — and at `−25 <= q <= −1`, where a tie
   is `Y / 2^b` an integer, which the second case above can never hold, so it
   is always in the ambiguous branch. There, **when `5^|q|` divides `m`**,
   `X = (m / 5^|q|) · 2^q` exactly, and the tier answers
   `(m / 5^|q|).toFloat()` — IEEE's conversion, round-half-to-even — times
   `2^q`, exact because the result is at least `2^-26`, a normal. That is
   the 40-digit halfway `9007199254740993.000…` (eighteen digits kept, the
   rest zeros, `q = −2`) and every exact value or tie with a short fraction.
   Otherwise the tier declines.
7. **Eighteen digits, and a dropped digit.** When a dropped digit is not
   nought, `m · 10^q < T < (m + 1) · 10^q`, and rounding to nearest is
   monotone, so if `m · 10^q` and `(m + 1) · 10^q` round to the same `Float`
   so does `T`. The tier answers only then; when they differ, a boundary lies
   between them and `parseSlow` decides. Rust keeps nineteen digits; keeping
   eighteen, so that `m + 1 <= 10^18 < 2^60` fits the normalisation, only
   moves which inputs have a boundary in `(m, m + 1)`. A dropped digit that is
   nought changes nothing: then `T = m · 10^q` exactly.
8. **Subnormals and the least normal value.** A normal answer has
   `e >= −1074` (its least significand `2^52` times `2^-1074` is `2^-1022`).
   When `e < −1074` the answer is rounded once at `2^-1074`: with
   `d = −1074 − e`, `N' = floor(N / 2^d)` is the value in units of
   `2^-1075`, and the answer is `N'` rounded as above with `e = −1074`. It is
   strictly inside `(N', N' + 1)` unless the bits of `L` from 60 to `b − 1`
   are all ones *and* the `d` low bits of `N` are all ones: if the first
   fails, `Y / 2^b` is in `(N, N + 1)` and dividing by `2^d` keeps it in
   `(N', N' + 1)`; if the second fails, `Y / 2^b < N + 2` and `N`'s low bits
   `r <= 2^d − 2` give `Y / 2^(b+d) < N' + (r + 2) / 2^d <= N' + 1`. So the
   subnormal test is Lemire's with the dropped bits added to it. `d > 54`
   means `X < 2^55 · 2^(e−1) < 2^-1075`, which is `0.0`. A subnormal whose
   rounding reaches `2^52` is `2^-1022` exactly, because the least normal
   binade has the subnormals' unit — **so the least normal boundary needs no
   case of its own**, and `2.2250738585072011e-308`, just below it, is `d = 1`.
   The exact case never reaches here (`q >= 0`), nor does step 6 (`q >= −26`).
9. **Assembly and overflow.** `significand × 2^e` is a ladder of
   multiplications or divisions by `2^512`, `2^256`, … `2`, each exact: every
   intermediate is `significand × 2^j` with `j` between nought and `e`, and
   `e >= −1074`, so it is a multiple of `2^-1074` with at most 53 significant
   bits, representable unless it is at least `2^1024`. Going up, the
   intermediates are below the result, so only the last step can overflow,
   and it does exactly when `significand × 2^e >= 2^1024` — which is when the
   correctly rounded value with an unbounded exponent is past `MAX`, and IEEE
   answers `inf`. Going down, the result is at least `2^-1074`.
10. **The sign** is applied last, as a multiplication by `±1.0` of a value
    that is already the answer's magnitude: exact, and it keeps `−0.0` and
    `−inf`.

The tier's answers are therefore exactly the fast path's and the slow path's
would have been, and it declines — to the slow path — exactly on the
ambiguous branch without an exact division, and on a dropped digit whose two
ends round apart.

#### 3.3 Why 768 digits are enough

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

#### 3.4 Extreme exponents

The exponent stops growing once it reaches `10^17`, so it is held at most
`10^18 + 9`, and `point`'s own part is at most the text's length in magnitude.
If the written exponent is at least `10^17` in magnitude, the value with a
nonzero digit has `|point| >= 10^17 - length`, which for any text shorter than
`10^17 - 400` bytes is past both step 2 bounds on the same side as the true
`point` — and the true `point` is further out still — so both are `±inf` or
both `±0.0`. The middle tier meets such an exponent first, and decides it the
same way for the same reason: `q = scale + dropped` is then at least
`10^17 − length` in magnitude, far outside `[−342, 308]`, on the true
exponent's side. A text of `10^17` bytes, a hundred petabytes, is not one a
heap holds; that is the one assumption. A mantissa of zeros is zero whatever
the exponent, and is answered before the exponent is used.

Every `Int` in the body is bounded so that nothing can trap: `mantissa`
`< 10^18`; the exponent `<= 10^18 + 9`; `point` and `scale` within the text's
length of that, below `2^63`; the long division's remainder below
`10 × divisor <= 5.8 × 10^18`; the significand below `10^17`; and `exp2`
within about `±1100`. None of these is a deviation from the algorithm that
changes an answer: the 59- and 25-bit steps only change how many passes there
are, and the argument above holds for any sequence of steps.

#### 3.5 Subnormals and the least normal value

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

#### 3.6 How the tests reach each edge

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
- **The middle tier** (`middle_tier` and `round_trips`): mantissas of one to
  eighteen digits at `q = −345` to `−340`, `−326` to `−323`, `−310` to
  `−307`, `−28` to `−25`, `37` to `40` and `290` to `310` — the table's first
  and last entries and one past each, where `F` stops being exact and where
  the exact division stops — each also with a nonzero nineteenth digit and
  with dropped zeros; the exact midpoints of 300 floats between `2^40` and
  `2^64`, whose expansions are short enough for the tier, cut to 17, 18 and
  19 digits and one unit either side; the midpoints of 300 floats across the
  whole range cut to 18 and 19 digits and one unit either side; 300 random
  subnormals; `1e23`, the 40-digit halfway and its neighbours at the 40th
  digit, `2.2250738585072011e-308` at 17 to 23 digits and its neighbours, and
  `MAX` at 17 to 19 digits; and seeded round-trips, random bit patterns spelled
  shortest, with 17 digits and with 18, and random 15- to 19-digit strings
  with an exponent anywhere from −350 to 310.

The default run is 1,105 adversarial, 3,500 random, about 7,000 middle-tier
and 6,000 round-trip inputs on each tier; the four ignored wide sweeps are
100,000 more on the VM, and two ignored middle-tier sweeps (`round_trips`
over ten more seeds) another 100,000, which `cargo ratchet` runs in about
eighty seconds.

Outside the repository, the prototype's own differential harness (a Rust
generator and a bit-for-bit checker against `str::parse::<f64>`) was run
against the **shipped** body on the built binary: 7,471,537 inputs in ten
classes on the native tier and the same 7,471,537 on the VM, **0 wrong on
either** — the classes and the fallback rates are in the measurements.

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
- **The middle tier allocates nothing**, and it answers every 17-digit and
  every shortest round-trip, subnormals and exact ties included (0 fallbacks
  in 2,000,000 of each, measured below).
- **The slow path allocates one digit buffer**: `core.vectorWithCapacity(768)`,
  a vector's header and its 768-word store, two objects and 772 words, once a
  call. Nothing else; the state is `Int`s and a `Bool`, and `divided` writes
  through `var` parameters. It is reached now only by what the middle tier
  declines.
- **A refusal allocates its message**, as the Rust arm did.
- **The literal heap** gains the three literals the body holds that no program
  already had — `` ` is not a Float ``, `nan` and `infinity` — seven words,
  and the middle tier's table, 9,765 bytes and **1,222 words**, one
  allocation, placed once before the run in every program that reaches
  `Float.parse` (every program, under a whole-package lowering). myuon
  accepted that cost.

ADR 0064's Decision 8 asks that allocations not regress. On every workload in
this repository they do not: execution allocations and words are identical to
the instruction on all three cq workloads, and the +4 allocations and +1,229
words are the literals. **Per input they do** — an 800-digit text, or one
whose nineteenth digit decides a boundary, allocates the buffer where the Rust
arm allocated nothing — and that is
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

Four binaries, and the history reads left to right: **pre-#544** is main at
`c709a9e`, with `Float.parse` Rust's behind `Intrinsic::FloatParse`;
**#544** is main at `38c9cc8`, `std.float.parse` with a fast path and the slow
path; **F** is #544 with the scan reshaped (§2's grammar scan); **M1** is F
with the middle tier. #544, F and M1 were each built
`--profile checked --features template` in a target directory of its own from
a clean tree; pre-#544 is the binary #544's own measurement below used. Every
run was over one pristine tree of `38c9cc8`, from `examples/`, with nothing
else running: `bookings-20k.jsonl` (#509's 20,000 records) for
`revenue-summary` and `confirmed-bookings`, and ADR 0046's `rates.csv` 20,000
times over, 120,000 rows, for `rate-card`. cq's output is byte-identical
across #544, F and M1 and both tiers, per workload.

### Counters

| cq | pre-#544 | #544 | F | M1 |
| --- | ---: | ---: | ---: | ---: |
| emitted IR / functions | 8,485 / 93 | 9,440 / 97 | 9,479 / 97 | 9,853 / 100 |
| `IntrinsicCall` sites | 3 | 0 | 0 | 0 |
| compiled / refused (native) | 83 / 10 | 87 / 10 | 87 / 10 | 90 / 10 |
| machine code | 764,965 B | 843,614 B | 847,201 B | 875,538 B |

(#544's own record below says 9,443 and 843,717 B, measured one revision
before it merged; the #544 column here is the merged `38c9cc8`.) M1's three
new functions are `parseMiddle`, `eiselLemire` and `timesPowerOfTwo`, and all
three compile; nothing is newly refused.

Executed VM instructions, and `std.float.parse`'s share a call, by
`--profile --profile-rows all`:

| workload | calls | pre-#544 | #544 | F | M1 | a call: #544 → F → M1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `revenue-summary` | 60,000 | 257,589,593 | 262,322,846 | −740,000 (−0.28%) | −680,000 (−0.26%) | 78.9 → 66.6 → 67.6 |
| `confirmed-bookings` | 73,385 | 292,406,801 | 298,928,722 | −1,077,229 (−0.36%) | −1,003,844 (−0.34%) | 88.9 → 74.2 → 75.2 |
| `rate-card` | 240,000 | 470,382,075 | 501,782,075 | **−5,840,000 (−1.16%)** | −5,600,000 (−1.12%) | 130.8 → 106.5 → 107.5 |

(F and M1 are against #544.) **Every instruction of each difference is
`std.float.parse`'s**: no other function's count moves, and on cq the
middle tier and the slow path never run, because every number cq reads is
inside Clinger's box. F saves about a fifth of the parse — `rate-card`'s six-
and five-character rates 24.3 instructions a call, `revenue-summary`'s one- to
five-character numbers 12.3 — and M1 costs one instruction a call back, the
`dropped = 0` its scan starts from. Of #544's +31.4 M on `rate-card`, F and
M1 take back 5.6 M. Dispatches fall with the instructions (`rate-card`
425,141,939 → 418,841,939 → 418,981,939).

On the native tier the helper calls do not move: `open` and `close` are the
same on all three binaries on every workload (cq's parse is one
native-to-native call a site, as in #544), and `intrinsic` is 0.

**Allocations.** What a run allocates is identical on #544, F and M1, to the
object, on every workload. `--stats`' totals are +1 allocation and **+1,222
words** on M1 on every workload, and that is the literal heap: the table,
placed once before the run. `crates/cove-runtime/tests/encoded.rs`' literal
heap rises 1,793 → 3,015 words, the same one literal.

**`crates/cove-cli/tests/copies.rs`** is 7,399 on #544, F and M1, over the same
264 programs — no program was added. Both commits were +264 at first draft,
one copy in each program, and both were removed rather than ratcheted: F's
was `let from = at` straight after `at += 1`, a copy after a producer, now
`let from = at + 1`; M1's was a `powerOfTwo(d)` helper whose answer the
inliner copied into `let p`, now computed in place.

### Wall time

Process wall time, one cold round discarded and then fifteen interleaved
rounds, the order rotated each round; seconds, `median (min–max)`, and the
median of the fifteen paired deltas with the rounds the right-hand binary won:

| row | #544 | F | M1 | #544 → F | #544 → M1 |
| --- | ---: | ---: | ---: | ---: | ---: |
| `revenue-summary` VM | 2.246 (2.236–2.305) | 2.280 (2.263–2.304) | 2.285 (2.271–2.377) | +1.17%, 2/15 | +1.70%, 2/15 |
| `revenue-summary` native | 1.480 (1.446–1.507) | 1.477 (1.455–1.549) | 1.480 (1.459–1.523) | +0.48%, 7/15 | +0.35%, 7/15 |
| `confirmed-bookings` VM | 2.799 (2.783–2.860) | 2.824 (2.804–2.874) | 2.827 (2.807–2.878) | +0.46%, 5/15 | +0.96%, 3/15 |
| `confirmed-bookings` native | 1.864 (1.837–1.939) | 1.869 (1.827–1.923) | 1.858 (1.820–1.879) | −0.01%, 9/15 | −0.51%, 10/15 |
| `rate-card` VM | 5.876 (5.784–5.995) | 5.908 (5.823–6.177) | 5.868 (5.794–5.973) | +0.49%, 5/15 | −0.14%, 9/15 |
| `rate-card` native | 3.798 (3.726–3.922) | 3.877 (3.713–4.013) | 3.766 (3.693–3.849) | +1.24%, 6/15 | −1.13%, 10/15 |

And the whole migration on the VM, pre-#544 in the same rounds:

| row | pre-#544 | #544 | M1 | pre-#544 → #544 | pre-#544 → M1 |
| --- | ---: | ---: | ---: | ---: | ---: |
| `revenue-summary` VM | 2.190 (2.166–2.212) | 2.246 | 2.285 | +2.95%, 0/15 | +4.31%, 0/15 |
| `confirmed-bookings` VM | 2.698 (2.672–2.741) | 2.799 | 2.827 | +3.66%, 0/15 | +4.90%, 0/15 |
| `rate-card` VM | 5.604 (5.567–5.680) | 5.876 | 5.868 | +5.20%, 0/15 | +4.43%, 0/15 |

**Every #544 → F and #544 → M1 row is inside cq's ±2.9% floor, and the
direction of the VM rows is not the change's.** F executes 1.2% fewer
instructions and 1.5% fewer dispatches on `rate-card` and is measured +0.49%;
M1, one instruction a call more than F, −0.14%. A first run of the same
protocol, on a build of M1 whose table decode was one expression rather than
a statement a step (the same instructions in other frame slots — `cove fmt`
and covefmt disagreed on the expression's layout), put the same rows at
+0.35% to +1.59% on the VM and −1.45% to +1.90% natively, every one inside the
floor too. The control says why the VM rows lean slower: covefmt makes no
`Float.parse` call, and its output, `--stats` and `--boundary` are identical
on #544, F and M1 on both tiers — but for native's count of unreached
declarations, 897 → 900, the three new functions — and five interleaved pairs
of it measured **+2.60% (0/5) for F and +3.06% (0/5) for M1 on the VM**
(+1.98% and +3.93% in the first run), and +0.08% and +0.34% on the native
tier. The binaries differ in VM speed by as much as the cq rows do, on a
program whose work did not change by one instruction, so the VM rows measure
the build and not the parse. VM and native print the same covefmt bytes on
M1.

The pre-#544 rows are the migration's whole cost on the VM so far: #544's
+3.0 to +5.2% here (it recorded +3.5 to +4.0% on its own day), and +4.3 to
+4.9% with M1 — not more than #544's own, row for row within the noise the
control shows.

### Per input class

The scratch package of #544's table: `Float.parse` on one text in a callee's
loop, and the same loop without it, and the difference is the parse (the
loop's own few instructions are in the count, as they were). Instructions,
allocations and words are per call on the VM, from `--stats`; nanoseconds are
the median of five rounds on the VM and on the native tier.

| class | text | VM instr. #544 / F / **M1** | allocs / words #544 → **M1** | VM ns #544 / F / **M1** | native ns #544 / F / **M1** |
| --- | --- | ---: | ---: | ---: | ---: |
| cq-shaped | `109.00` | 138 / 113 / **114** | 0 / 0 → **0 / 0** | 516 / 502 / **547** | 96 / 89 / **92** |
| one place | `0.1` | 100 / 87 / **88** | 0 / 0 → **0 / 0** | 361 / 363 / **380** | 71 / 71 / **73** |
| 16 digits | `3.141592653589793` | 285 / 216 / **217** | 0 / 0 → **0 / 0** | 1,049 / 912 / **911** | 189 / 173 / **175** |
| 17-digit round-trip | `1.2345678901234567` | 3,191 / 3,118 / **387** | 2 / 772 → **0 / 0** | 20,740 / 21,086 / **2,158** | 4,519 / 4,468 / **514** |
| 19 digits | `1234567890123456789` | 6,025 / 5,951 / **561** | 2 / 772 → **0 / 0** | 41,008 / 43,730 / **3,177** | 7,988 / 7,894 / **820** |
| 20 digits | `12345678901234567891` | 6,308 / 6,233 / **582** | 2 / 772 → **0 / 0** | 43,143 / 44,185 / **3,253** | 8,197 / 8,223 / **841** |
| e = 23, a tie | `1e23` | 4,729 / 4,718 / **280** | 2 / 772 → **0 / 0** | 32,392 / 33,648 / **1,670** | 6,481 / 6,517 / **441** |
| e = −23 | `1e-23` | 4,187 / 4,176 / **275** | 2 / 772 → **0 / 0** | 26,380 / 27,188 / **1,635** | 5,603 / 5,524 / **429** |
| e = 300 | `1e300` | 129,123 / 129,112 / **298** | 2 / 772 → **0 / 0** | 1,046,801 / 1,090,388 / **1,761** | 169,673 / 169,533 / **439** |
| e = −300 | `1e-300` | 122,622 / 122,611 / **300** | 2 / 772 → **0 / 0** | 1,023,997 / 1,062,279 / **1,780** | 163,479 / 163,545 / **447** |
| subnormal | `4.9406564584124654e-320` | 155,591 / 155,515 / **480** | 2 / 772 → **0 / 0** | 1,324,199 / 1,368,542 / **2,771** | 209,848 / 209,809 / **746** |
| halfway, 40 digits | `9007199254740993.000…` | 6,103 / 6,008 / **806** | 2 / 772 → **0 / 0** | 40,170 / 41,341 / **3,848** | 7,680 / 7,693 / **881** |
| 768 digits | `wide.mid` padded | 31,136 / 30,313 / **36,433** | 2 / 772 → 2 / 772 | 121,804 / 121,483 / **148,682** | 25,771 / 26,226 / **31,388** |
| 800 digits | the same, `…01` | 36,996 / 36,141 / **37,295** | 2 / 772 → 2 / 772 | 159,770 / 160,591 / **167,597** | 31,717 / 30,746 / **32,239** |
| huge exponent | `1e99999999999999999999` | 376 / 366 / **313** | 2 / 772 → **0 / 0** | 2,187 / 2,216 / **1,368** | 981 / 976 / **269** |
| refusal | `1.0x` | 128 / 113 / **114** | 2 / 9 → 2 / 9 | 747 / 742 / **741** | 366 / 373 / **377** |

- **Inside the box, F is the change**: `109.00` 138 → 113, `3.141592653589793`
  285 → 216; M1 adds the one instruction back.
- **Past it, M1 is**: every class but the two longest is decided by the middle
  tier and allocates nothing. Against #544's slow path it is 10 to 19 times
  faster on the VM from 17 to 40 digits and at `1e±23`, and 470 to 600 times
  at `1e±300` and the subnormal; natively 8.5 to 15 times and 280 to 390
  times. `1e23` and the 40-digit halfway
  are exact ties, decided (§3.2, step 6).
- **The 768- and 800-digit rows fall back, and pay for the attempt**: the
  middle tier reads the 750 dropped digits backwards to find the first that
  is not nought, asks `m` and `m + 1`, finds a boundary between them, and
  hands over — +5,297 VM instructions (+17%) on the 768-digit tie, where 714
  of those digits are zeros, and +299 (+0.8%) on the 800-digit input, whose
  last digit is the nonzero one. That is the one regression, on inputs no
  workload has.
- The huge exponent is the range test now, before any digit is buffered.
- Inside the box the VM nanoseconds move by less than the build does: M1's
  `109.00` is 547 ns against F's 502 on one more instruction, and covefmt,
  whose work is identical on the three binaries, moves as much (the control
  under the wall times).

#### How often the slow path is reached

By `--stats` over the prototype harness's input classes (allocations of the
parse, less those of the same loop without it, over two, on the native tier;
no class below has a refusal, so every allocation is the buffer's):

| class | inputs | slow path |
| --- | ---: | ---: |
| shortest round-trips (`{:e}`) of random bit patterns, subnormals included | 1,000,000 | **0** |
| 17 significant digits (`{:.16e}`) of random bit patterns | 1,000,000 | **0** |
| shortest positional (`{}`), `x · 10^e`, `|e| <= 20` | 250,000 | **0** |
| random 15- to 19-digit strings, exponent in `[−350, 310]` | 1,000,000 | 2,711 (0.27%) |
| random 20- to 40-digit strings | 250,000 | 3,895 (1.56%) |
| `1eN`, `5eN`, `9eN`, `1.5eN`, … for `N` in `[−400, 400]` | 8,010 | **0** |
| midpoints: exact, and rounded and cut to 17, 18 and 19 digits; exact midpoints up to 40 digits; short midpoints ±1 in the 19th digit | 1,853,574 | 655,666 (35.4%) |
| ±1 ulp neighbours, shortest and 17 digits | 1,000,000 | **0** |
| 17 digits and zeros, `0…01`, exact midpoints padded with zeros or `…1` | 69,875 | 3,258 (4.66%) |

The midpoint class is built to be the tier's worst case — nineteen-digit
near-midpoints whose `[m, m + 1)` straddles a boundary, and exact midpoints
too long for it — and those are the inputs it is meant to decline.

#### The differential harness, outside the repository

The same classes, plus 40,078 grammar edges and random strings over
`0-9.eE+-`, run through the **shipped** `std.float.parse` on M1's binary, each
answer compared with `str::parse::<f64>` bit for bit: **7,471,537 inputs, 0
wrong on the native tier, and the same 7,471,537, 0 wrong, on the VM.**

### Estimated against measured

The prototype evaluation estimated its M1 with F's scan at 383 VM
instructions, 2,076 ns and 502 ns natively on the 17-digit round-trip, and
said a backward scan of the dropped digits would bring the 19-, 20- and
40-digit rows to or below its 630, 653 and 874. Measured in the shipped body:
**387, 2,158 and 514 ns** on the round-trip, and **561, 582 and 806** on the
three rows; +374 emitted IR and +28.3 KB of machine code on cq against the
estimated +352 and +26.2 KB (the tier's entry is a function of its own, so
that `parse` keeps every call a way out). Its estimate that a real fallback
would cost about +5,000 VM instructions over #544 on 768 digits was right:
+5,297. And the estimate that F would save about 6.0 M instructions on
`rate-card` measured 5.84 M.

**This cost is measured and is not accepted by this ADR**, with the two
exceptions myuon has decided — no new instruction, and the table's 1,222
words. What is asked of review is: on cq, −1.1% VM instructions and every
wall row inside the floor (the VM rows' direction being the build's, by the
control); per input, the classes above, and the 768-digit row's +17%.

### #544's measurement, as it was recorded

What follows is #544's own record, against `c709a9e`, kept as written for the
history. Where the tables above measure the same thing — the per-class table,
the wall rows, the counters — they supersede it.

`before` is main at `c709a9e`; `after` is this change's second commit
(`std.float.parse`, the intrinsic route and the interpreter's arm deleted).
Each was built `--profile checked --features template` in its own target
directory, and every run was over one pristine worktree of `c709a9e`, from
`examples/`, with nothing else running: `bookings-20k.jsonl` (#509's 20,000
records) for `revenue-summary` and `confirmed-bookings`, and ADR 0046's
`rates.csv` 20,000 times over, 120,000 rows, for `rate-card`. cq's output is
byte-identical across the two binaries and both tiers, per workload.

#### Counters

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

#### Wall time

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

#### The fast path and the slow path, per input class

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

#### Estimated against measured

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
- A text past Clinger's box costs a few hundred VM instructions and nothing
  allocated where #544 made it thousands to hundreds of thousands and a
  772-word buffer; the middle tier's table costs every program that reaches
  `Float.parse` 1,222 words of literal heap. What still reaches the slow
  path is an input whose nineteenth or later digit decides a boundary, a long
  exact tie, and text of hundreds of digits.
- A wider multiply, a word table and a bit-level `Float` assembly would take
  the middle tier from about 160 VM instructions to about 30 (estimated); that
  is a question for ADR 0064's Decision 2, and on today's numbers not one
  worth its new kind of `Int` operation.
