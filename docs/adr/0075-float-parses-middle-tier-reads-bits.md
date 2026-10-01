# ADR 0075: `Float.parse`'s middle tier reads bits

- Status: Proposed
- Date: 2026-10-01
- Decides: that `std.float.parse`'s Eisel–Lemire middle tier is written over
  [ADR 0074](0074-an-int-also-carries-a-fixed-width-bit-pattern.md)'s `Int`
  bit operations, and computes its product in two stages, so that most
  inputs read only ten of a table entry's fifteen bytes. It adds no
  instruction, and it changes no answer, no fallback and no byte of the
  table. Recorded for [issue #432](https://github.com/myuon/cove/issues/432)'s
  follow-up
- Supersedes: [ADR 0072](0072-float-parse-is-cove.md)'s "the middle tier
  uses existing instructions only", for the bit operations ADR 0074 has since
  added. Everything else in ADR 0072 stands: no instruction of its own, the
  algorithm, its correctness argument as amended in Decision 3 below, the
  table and its accepted cost, and the Decimal fallback
- Refers to, without superseding:
  [ADR 0074](0074-an-int-also-carries-a-fixed-width-bit-pattern.md), whose
  §6 says that it "neither revises ADR 0072's accepted algorithm nor
  authorizes a parser rewrite". This ADR is the separate, measured decision
  that sentence asks for

## Context

ADR 0072 put an Eisel–Lemire middle tier between Clinger's fast path and
simple decimal conversion. It emulates the 64×128-bit product in thirty-bit
limbs with checked signed arithmetic, and decodes a 651-entry table of powers
of five from one string literal. myuon accepted it with "existing
instructions only", because at the time Cove had no shift and no mask. A
17-digit round-trip cost 387 VM instructions, and 59 of `eiselLemire`'s 138
went to reading the table.

ADR 0074 then gave `Int` seven bit operations, as typed instructions on
every tier. Its §6 kept rewriting the parser out of its scope. myuon asked
for the rewrite to be tried and measured. This ADR records the result and the
decision to adopt it.

## Decision

### 1. The product is taken in two stages

- **Stage 1** reads only `F`'s top two limbs, ten of the entry's fifteen
  bytes. From four 30×30-bit products it computes
  `L' = w · (f2 · 2^60 + f1 · 2^30)`, and lets `top = L' >> 90`.
  - If `top`'s low five bits are neither all ones nor all zeros, `top`
    decides the answer, for every `q`, including those where `F` is exact and
    those whose result is subnormal.
- **Stage 2** runs otherwise, which is about 2 inputs in 32 plus every exact
  tie.
  - It reads `f0`, adds `w · f0`, and runs ADR 0072's Lemire test unchanged on
    the same exact `L`.
  - It skips reading `f0` for `0 <= q <= 25`, where `f0` is zero. That was
    checked against the literal.

### 2. Shifts and masks replace division and comparison where they are cheaper

- **Carries** are `shiftRight(30)` and `bitAnd(2^30 - 1)`. On the native tier
  this removes a real `idiv` each time.
- **The binary exponent** is `e = (217706 · q + 458752).shiftRight(16) + h − s`.
  The floor comes from the arithmetic shift's floor definition. This was
  checked against the exact value for all 651 `q`.
- **Rounding and the subnormal `2^d`** are shifts.
- **`timesPowerOfTwo`** scales by 2^248, 2^124 and 2^62, then once by
  `1.shiftLeft(k).toFloat()` with `k <= 62`.
- **Kept as they were:**
  - **the normalisation ladder**, where a shift is no cheaper than the
    comparison it would replace;
  - **the thirty-bit limbs.** A 32×32-bit partial product would overflow
    checked `*`.

### 3. The correctness argument carries over, with one new step

ADR 0072's argument is amended here, not replaced:

- **Its step 3's bounds are now per stage.**
  - Stage 1: column sums are below `2^61 + 2^30` and `top` is below `2^60`.
  - Stage 2: `d1` is below `2^60 + 2^31`, and `L` is exactly the previous
    `L`.
  - Every shift count is in `1..62`, so ADR 0074's count check can never
    trap.
  - Where step 3 said "a positive numerator, so `/` is a floor", the floor
    now comes from `shiftRight`'s definition.
- **A new stage-1 step.**
  - `L' <= Y < L' + 2^90`, and `L' < Y` unless `F` is exact.
  - If `top mod 32` is neither 0 nor 31, then `Y` lies strictly between
    `N · 2^b` and `(N + 1) · 2^b`. So it is never a rounding boundary and
    never a tie.
  - Excluding the all-zeros case is what covers an exact `F`.
  - Subnormals follow as in step 8.
- **Steps 4 to 8** apply unchanged to stage 2.
- **Step 9's power-of-two ladder** is re-chunked. Its exactness argument is
  the same.

### 4. The table stays byte for byte

A denser encoding was considered and rejected:

- String literals have no `\x` or `\u` escape, so a byte carries at most
  about 6.6 bits.
- That would save at most one of stage 1's ten bytes, and it would halve
  stage 1's error margin.

The literal heap stays at 1,222 words.

## Measurement

`before` is main `c940bfe` and `after` is this change. Both were built
`--profile checked --features template` and run over one pristine tree.
Times are medians of 10 rounds, with the loop in a callee.

**Correctness:**
- `float_parse.rs` passes unchanged on the VM and the native tier, including
  its ignored sweeps.
- A differential run of **6,471,537 inputs** against Rust's
  `str::parse::<f64>` gave **0 wrong answers on native and 0 on the VM**. It
  includes 1 M random 17-digit round-trips over the whole range, subnormals,
  and 1.85 M midpoints.
- **The fallback rate to the Decimal path is identical in every class:**
  0.2711% on 15–19 digits, 1.558% on 20–40, 35.373% on midpoints, and 4.663%
  on tails.

| input | VM instr. | VM ns | native ns |
| --- | ---: | ---: | ---: |
| `109.00`, `0.1`, `3.141592653589793` | unchanged | about −1% | about −1% |
| `1.2345678901234567` | 387 → 312 | 2,058 → 1,535 (−25%) | 524 → 386 (−26%) |
| 19 digits | 561 → 427 | 3,150 → 2,188 (−31%) | 816 → 554 (−32%) |
| 20 digits | 582 → 438 | 3,228 → 2,218 (−31%) | 838 → 572 (−32%) |
| `1e23` | 280 → 242 | 1,670 → 1,251 (−25%) | 441 → 347 (−21%) |
| `1e-23` | 275 → 217 | 1,640 → 1,166 (−29%) | 429 → 298 (−31%) |
| `1e300` | 298 → 244 | 1,736 → 1,240 (−29%) | 440 → 325 (−26%) |
| `1e-300` | 300 → 246 | 1,738 → 1,216 (−30%) | 446 → 326 (−27%) |
| subnormal | 480 → 401 | 2,794 → 2,032 (−27%) | 748 → 568 (−24%) |
| 40-digit halfway | 806 → 784 | 3,849 → 3,592 (−7%) | 878 → 803 (−9%) |
| 768 and 800 digits | −151 | within ±1% | within ±1% |

**Allocations are unchanged on every row.**

`eiselLemire`'s 17-digit instructions, by part, from `--profile`:

| part | before | after |
| --- | ---: | ---: |
| table read | 59 | 40 |
| product and carries | 23 | 9 |
| normalise | 16 | 13 |
| `e` and `g` | 11 | 9 |
| limb split of `w` | 3 | 4 |
| tests and rounding | ~24 | 8 |
| call and return | 2 | 2 |
| **total** | **138** | **85** |

`timesPowerOfTwo` goes from 32 to 10 instructions. The scan in `parse`, at
about 202, now dominates a 17-digit call.

**Programs:**
- cq executes exactly the same instructions on all three workloads, because
  it stays on the fast path. Its output, allocations and heap are identical on
  both tiers.
- cq's wall smoke, 10 pairs, is within its ±2.9% floor on every row.
- covefmtBench is identical in output and in `--stats`/`--boundary`.

**Static size:**
- `std.float`'s emitted IR goes from 2,918 to 2,876.
- cq's machine code goes from 875,538 to 872,286 bytes.
- Native still compiles 90 and refuses 10.
- `copies.rs` is unchanged.

## Consequences

- A text past Clinger's box parses about a quarter to a third faster on both
  tiers. Nothing that ran before changes its answer, its fallback or its
  allocation.
- **No workload in the repository reaches the middle tier**, so none of
  them gets faster. The gain is for programs that read full-precision floats,
  which ADR 0072's review called ordinary input.
- Making the table read (still 40 instructions) much cheaper would need a
  word table, a bit-cast or `mulhi`. Each of those is a separate, measured
  decision under ADR 0064 Decision 2 and ADR 0074 §6.
