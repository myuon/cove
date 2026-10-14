# ADR 0098: The elementary functions are Cove

- Status: Accepted
- Date: 2026-10-14
- Decides: that `Float` has `exp`, `ln`, `log10`, `sin`, `cos` and
  `pow(exponent)`; that all six are `std.float`, Cove bodies built from
  `+`, `-`, `*`, `/`, comparison, `Int` arithmetic and `core.floatTruncate`,
  with **no instruction of their own** and no call into a host `libm`; that
  their contract is **within one unit in the last place of the exact value**,
  with IEEE 754's and C99's special values, exact answers wherever the exact
  value is a double that the contract names, and `sin` and `cos` reduced
  exactly at every magnitude; and that the contract is held to a 300-bit
  reference the subject does not ship. Recorded for
  [issue #648](https://github.com/myuon/cove/issues/648)
- Supersedes: nothing
- Refers to, without superseding:
  [issue #250](https://github.com/myuon/cove/issues/250), which admitted
  `Float.sqrt` *because* IEEE 754 makes it exact and declined "a wider set of
  maths functions" because it does not make them exact — this ADR keeps that
  argument and answers it another way;
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2
  and [ADR 0073](0073-a-primitive-below-the-standard-library-is-an-instruction.md)'s
  Decision 2, which say where a new primitive would go and why none is needed
  here; and [ADR 0072](0072-float-parse-is-cove.md), the precedent of a
  numerically exacting `Float` operation written in Cove, whose
  string-literal table encoding `std.float.twoOverPi` reuses

## Context

Issue #602's survey of the cove-tools apps found two of them writing these
functions in Cove for themselves: the algorithm playground `exp`, `ln`, `sin`
and `cos`, the benchmark ledger `log10` and `pow`. That is the recurring
friction the philosophy asks for, and myuon decided on 2026-10-14 to add
them, per function by measurement and correctness, with one requirement above
the rest: **every tier answers the same bits for every input** — the
tree-walking interpreter, the encoded VM and the native tier — and CI diffs
the native tier against the VM.

Issue #250 is why that requirement is the whole question. `sqrt` was admitted
alone because IEEE 754 §5.4.1 makes it correctly rounded: every conforming
machine answers the same bits, and a replay that depends on it is a replay
that holds on somebody else's computer. The standard requires nothing of the
sort for `exp` or `sin`. Two `libm`s answer different last bits — macOS's and
glibc's do, and CI runs on both — so an operation that reached the host's
`libm` would be one whose answer depended on which machine ran the program,
and on the native tier on whichever `libm` the code generator's helper was
linked against.

There were two ways to keep the bits:

- **(A) an IR instruction per function** whose every tier calls one Rust
  implementation that does not come from the platform — a pure-Rust port such
  as the `libm` crate — with the native tier calling it as a runtime helper;
- **(B) Cove in `std.float`**, built from operations IEEE 754 does make exact,
  so that every tier agrees because every tier is running the same
  correctly rounded operations in the same order.

## Decision

### 1. All six are (B)

(A) is the thing ADR 0064 Decision 2 and ADR 0073 Decision 2 were written to
stop. An instruction below the standard library has to name a machine
operation — "a typed scalar operation that maps to a CPU or backend
operation" — and `exp` names a method, implemented by a body of a few hundred
operations that happens to be written in Rust. It would be `IntrinsicCall`
come back under another name, with a native helper and its protocol, and
"Separate policy from mechanism" says the same thing from the other side: an
algorithm a Cove body can express safely belongs in Cove source.

(B) needs nothing of the machine that is not already there, and gets the
requirement for free: the interpreter, the VM and the native code generator
each implement `+`, `-`, `*` and `/` as IEEE 754 binary64 operations rounded
to nearest, and none of them contracts a multiply and an add into a fused
one, so a body made of them is the same computation everywhere. Nothing in
this change touches an evaluator or the code generator, and there is no new
lowering to write once.

The measurement below is what made (B) acceptable rather than merely
principled: it is in the performance class of the Cove code a program would
otherwise write for itself, which is what the two cove-tools apps did, and it
is a correct one.

### 2. The algorithms are fdlibm's, rewritten where they read bits

The six are FreeBSD's `msun` — Sun's fdlibm, the code musl and Rust's `libm`
crate carried — because it is the best-understood implementation of exactly
this contract and its error analysis is published with it. Their notice is
kept in `std.float`, as it asks.

fdlibm reads and writes the bits of a double, and a Cove body has no bit
access to a `Float`. Each use was replaced by something that computes the same
thing, and none of the replacements is a new primitive:

- **a comparison against the double whose high word was the test**:
  `hx > 0x3fd62e42` is `|x| >= 0.3465735912322998`, the decimal that is
  exactly the double `0x3FD62E4300000000`;
- **ladders of exact products by powers of two** where fdlibm read an exponent
  field or wrote one — `exponentOf`, `twoTo`, and `scaled`, which puts `2^k`
  back with one exact product and, for a subnormal answer, one rounding;
- **a Veltkamp split** where fdlibm cleared a low word to make a value short
  enough that a product of two of them is exact. The split rounds where
  clearing truncated, which bounds the part split off by the same amount;
- **the `1.5 · 2^52` addition** for "the nearest whole number", which fdlibm's
  `rem_pio2` already used;
- **a Payne–Hanek reduction of its own** past `2^20 · π/2`, over 24-bit `Int`
  limbs of `2/π` read out of a string literal four bytes a digit (ADR 0072's
  encoding), where fdlibm's `__rem_pio2_large` worked in doubles: the
  significand's three 24-bit digits against ten digits of `2/π` that start
  where the exponent says, eight limbs carried, about 166 bits of fraction.
  The closest a double comes to a multiple of `π/2` is about `2^-61` of a
  quarter turn, at `6381956970095103 · 2^797`, so more than a hundred bits
  survive the worst cancellation there is; that value is a row of the e2e
  golden, and its cosine is `-4.687165924254628e-19`, the published one.

One place does better than fdlibm and had to. `pow` with `|y|` past `2^31`
and `x` within `2^-20` of one takes `log2(x)` from a short series, and
fdlibm's leading product `ivln2_h · t` is not exact — `t` has one bit more
than the comment beside it says — so at `y` near `2^32` the answer was off by
**234 units** in a prototype that copied it. That product is taken exactly
now, by Dekker's method.

### 3. The contract

- **Within one unit in the last place** of the exact value, for every finite
  argument, subnormal arguments and answers included, and not correctly
  rounded by promise: `1.0.exp()` is `2.7182818284590455`, one unit above the
  double nearest `e`.
- **Exact where the exact value is a double and the contract names it**:
  `exp(0) = 1`, `ln(1) = 0`, `log10` of every normal power of ten is that
  integer, and `pow` of two integers is the integer whenever it is below
  `2^53` — `3.0.pow(33.0)` is `5559060566555523.0`. `log10` of a *subnormal*
  power of ten is not an integer, because the double named `1.0e-320` is not
  `10^-320`.
- **The special values are IEEE 754's and C99's**: `ln` and `log10` are
  `-inf` at both zeros and `NaN` below them; `exp(-inf)` is `0`; `sin` and
  `cos` of an infinity are `NaN`; `sin(-0.0)` is `-0.0`. `pow` has all of
  C99's twenty-three cases — `x.pow(0.0)` is `1.0` even for a `NaN` `x`,
  `1.0.pow(y)` is `1.0` even for a `NaN` `y`, `(-1.0).pow(inf)` is `1.0`,
  `(-8.0).pow(1.0 / 3.0)` is `NaN` because `1.0 / 3.0` is not a third, and
  `(-0.0).pow(-3.0)` is `-inf`.
- **`sin` and `cos` are reduced exactly at every magnitude.** `sin(1.0e300)` is
  the sine of the number `1.0e300` names, to within the bound, not the sine of
  some nearby multiple of `π` plus noise. Whether a program means anything by
  the sine of `10^300` is its business; what this guarantees is that it is the
  same number on every machine and correct.
- **Nothing traps.** Like `abs`, `round` and `sqrt`, all six are total over
  `Float`: what has no answer is `NaN`.

### 4. How it is held

- `crates/cove-runtime/tests/float_math.rs` runs 3,637 rows — seeded random
  draws from every regime and the edges, `float_math_reference.py`'s list —
  against values `mpmath` computed at 300 bits, carried as the correctly
  rounded double and the remainder in units of its last place, so the test
  measures error in `f64` with no bignum of its own and subnormal answers
  measure correctly. It asserts every row under one unit, every exact value
  exact, and on a host the native tier runs on, that compiled code — all six
  and every helper, asserted compiled — answers the VM's bits for every row.
- `tests/e2e/values_float_math` is the golden of 138 named rows, each with its
  reciprocal so a zero says which zero it is, run on the interpreter and the
  VM by the harness and on the native tier by its `native` file.
- `benches/floatmath` is the cost, a `manual` matrix.

## Measured

### Accuracy

Worst error in units in the last place, on the encoded VM and identically on
the native tier:

| | committed rows (3,637) | wide sweep (73,237, seed 7, not committed) |
| --- | ---: | ---: |
| `exp` | 0.675 | 0.797 |
| `ln` | 0.724 | 0.730 |
| `log10` | 0.514 | 0.595 |
| `sin` | 0.656 | 0.754 |
| `cos` | 0.614 | 0.695 |
| `pow` | 0.729 | 0.777 |

### Cost

`benches/floatmath`, i7-10700K, `--profile checked`, the median of three runs
on a machine shared with other builds (load 5 to 8), nanoseconds a call by
difference from the bare loop:

| row | VM | native |
| --- | ---: | ---: |
| `exp(0.75)` | 323 | 72 |
| `ln(3.7)` | 393 | 88 |
| `log10(3.7)` | 441 | 96 |
| `sin(0.6)`, no reduction | 125 | 20 |
| `sin(10.0)`, Cody–Waite | 327 | 65 |
| `sin(1.0e300)`, Payne–Hanek | 2,853 | 637 |
| `cos(0.6)` | 136 | 27 |
| `cos(10.0)` | 322 | 67 |
| `cos(1.0e300)` | 2,859 | 603 |
| `pow(1.7, 3.3)` | 995 | 236 |
| `pow(3.0, 20.0)` | 1,051 | 251 |
| `sqrt(2.0)`, for scale | 2 | 2 |

So a call is a few hundred VM instructions' worth, and the native tier is
four to five times faster, the ratio Cove code has on it generally. The far
reduction is the one expensive regime and is reached only past `1.6 · 10^6`
radians; reading each digit of `2/π` once rather than three times took it
from about 6.1 µs to 2.9 µs on the VM.

What every program pays, measured against `origin/main` built the same way
in the same session:

- **The standard library is checked on every run**, and it is 10% longer:
  `cove check` of an empty package went from 23.9 ms to 26.2 ms, and `cove
  run` of `export fn main() { }` from 24.8 ms to 27.3 ms (medians of 40
  alternating runs). The cost is linear in the length of `std`, not in what a
  program calls; a cached, pre-checked standard library would take it back
  for every module at once, and is not this change's to make.
- **Every program lowers the bodies** — `copies.rs` counts 1,470 more IR
  instructions in each of its 273 programs, since nothing removes an
  unreached function — and lowering the empty program measured 0.677 ms
  against 0.690 ms. `copies.rs`'s ratchet rises by 55, all of it the two new
  programs; the bodies themselves add none, after a first draft added 1,638
  (see the constant's note there).

## Considered and not taken

- **(A), a Rust implementation behind an instruction**, for the reasons in
  Decision 1. It would have been faster on the VM — one dispatch rather than
  a few hundred — and it is the obvious thing to reach for if a profile ever
  shows a program spending its time here; that program would be the evidence
  a later ADR needs, and this one does not have it.
- **The host `libm`**, on any tier: it is the one choice that cannot meet the
  requirement.
- **A bit-cast between `Float` and `Int`** — a `Convert` each way, one machine
  instruction — would have let the fdlibm code be transcribed rather than
  rewritten, and would shorten the ladders. It is new surface that no
  correctness argument needs, and the ladders cost a few dozen instructions of
  a few hundred; it is the first thing to measure if these functions'
  cost ever matters.
- **Correct rounding**, as CORE-MATH does it. It costs large tables and a
  multiple-precision fallback for the rare hard cases, and its benefit over a
  one-unit bound is that two *different* correct implementations agree —
  which Cove does not need, because it has one implementation and every tier
  runs it.

## Consequences

- A program can compute an angle, a growth curve or a decibel in Cove and get
  the same bits on every machine and every tier, which is the property
  issue #250 said only `sqrt` could keep.
- The six functions' answers are now part of what a change can break: the
  e2e golden pins 138 of them bit for bit, and `float_math.rs` holds the rest
  to the bound. A change to how `+`, `-`, `*` or `/` is lowered on any tier
  will show up in both.
- `tan`, `atan2`, `asin`, `acos`, `exp2`, `log2`, `log1p` and the hyperbolic
  functions are not here, and not because they are hard: they are the same
  kernels and reductions, and wait for a program that needs them.
