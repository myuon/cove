# ADR 0074: An Int also carries a fixed-width bit pattern

- Status: Accepted
- Date: 2026-09-30
- Decides: seven public bit operations on `Int`, interpreted as a 64-bit
  two's-complement word; checked shift counts; typed scalar IR operations
  available on the interpreter, encoded VM and native tier
- Supersedes: nothing
- Refers to: [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s
  distinction between a machine operation and a library algorithm, and
  [ADR 0072](0072-float-parse-is-cove.md)'s decimal conversion implementation

## Context

Bit masks, packed fields, binary encodings and multiword arithmetic need to
inspect and move bits. These are basic operations on a fixed-width integer,
useful to ordinary Cove programs as well as the standard library.

There is a concrete example: ADR 0072's Eisel–Lemire middle tier implements
wide multiplication in thirty-bit limbs using existing arithmetic. It has
already landed without new instructions. Its emulation is evidence of
friction, not a reason to make a float parser an instruction. Bit operations
provide a general vocabulary for such implementations without moving their
algorithms or policy into Rust.

The language already gives Int checked arithmetic. That contract continues:
arithmetic computes a signed mathematical result and traps on overflow.
Bit operations instead manipulate the fixed-width representation. Their
different contract must be explicit.

## Decision

### 1. Public operations use methods, with no new syntax

All operands and results below are `Int`.

| Method | Meaning |
| --- | --- |
| `x.bitAnd(y)` | Bitwise AND |
| `x.bitOr(y)` | Bitwise OR |
| `x.bitXor(y)` | Bitwise XOR |
| `x.bitNot()` | Complement all 64 bits |
| `x.shiftLeft(n)` | Left shift, discarding bits above bit 63 |
| `x.shiftRight(n)` | Arithmetic right shift, extending the sign bit |
| `x.shiftRightLogical(n)` | Logical right shift, filling with zeros |

These are public APIs, not a stdlib-only escape hatch. Method spelling fits
the existing Int API and avoids adding tokens, precedence rules or assignment
operators. Calls evaluate their receiver and arguments once in the ordinary
order. Bool's short-circuit operations retain their existing meaning.

This ADR introduces neither `UInt` nor implicit signed/unsigned conversions.
An Int is still signed for comparisons, arithmetic, rendering and parsing.
Its bits are observable through these explicit operations.

### 2. The bit contract is independent of the host

Let `u(x)` be the unsigned integer in `0..2^64-1` with the same 64 bits as
`x`; let `s(w)` interpret a 64-bit word as a signed two's-complement Int.

For AND, OR, XOR and NOT, apply the operation to `u(x)` and return `s(w)`.
For valid shift count `n`:

- `shiftLeft`: `s((u(x) * 2^n) mod 2^64)`.
- `shiftRight`: `floor(x / 2^n)`, including for negative x.
- `shiftRightLogical`: `s(floor(u(x) / 2^n))`.

Left shift never traps for bits discarded or for a change of sign. This
does not give wrapping semantics to `+`, `-` or `*`, and a compiler may
not replace checked multiplication by a left shift unless their behavior is
proved equivalent for the operands concerned.

Examples pin the distinction:

| Expression | Result |
| --- | ---: |
| `5.bitAnd(3)` | 1 |
| `5.bitOr(3)` | 7 |
| `5.bitXor(3)` | 6 |
| `0.bitNot()` | -1 |
| `(-1).bitNot()` | 0 |
| `1.shiftLeft(63)` | -9223372036854775808 |
| `(-1).shiftLeft(1)` | -2 |
| `(-3).shiftRight(1)` | -2 |
| `(-1).shiftRightLogical(1)` | 9223372036854775807 |
| `(-1).shiftRightLogical(63)` | 1 |

A count of zero returns the receiver for every shift.

### 3. Shift counts are checked, not masked

A shift count must be in `0..63`. Every other Int, including -1, 64,
Int.MIN and Int.MAX, traps before the operation. There is no modulo-64
interpretation, no clamp and no implementation-dependent result.

The primary diagnostic is:

```text
a shift count must be between 0 and 63, got {count}
```

It names the source call as the primary location, using the existing
diagnostic and blame conventions. Constant evaluation has the same contract
and must not execute an invalid host-language shift. Any compile-time report
must identify the same invalid count.

The restriction is a scalar operation's domain check, like division's
nonzero divisor, not a text or collection policy. The checked shift IR owns
this check, so the VM and native tier cannot accidentally inherit a CPU's
masked shift count. A proven-valid count can eliminate the check.

### 4. These lower to typed scalar operations

The semantic IR distinguishes AND, OR, XOR, NOT, left shift, arithmetic right
shift and logical right shift. Existing instruction-family conventions should
determine their enum and opcode organization; no generic operation-name
dispatcher or Int/Float cross-product is required.

The verifier checks Int inputs and destination. Shift counts are Int values
and are validated at execution unless proven valid. Constant folding,
printing, encoding, decoding, data flow, tracing and fuel accounting preserve
the same operation identities and semantics.

AND/OR/XOR/NOT are pure, bounded and non-allocating. Shifts are the same on a
valid count and may raise on an invalid one. None allocates, collects, accesses
the heap, calls a Host API or requires an authority grant.

The public methods may resolve directly to these machine operations, as
other scalar operations do. No IntrinsicCall, public-method dispatcher or
mandatory wrapper-inlining exception is introduced. If a thin Cove binding
is used for API consistency, its underlying operation has the same typed
scalar contract.

### 5. All execution tiers implement the same bits

- The interpreter and encoded VM use explicitly fixed-width operations.
  Logical right shift views the operand as an unsigned word and interprets
  the resulting bits as Int; it is not a numeric unsigned conversion.
- Native code uses inline machine operations and a checked count. The current
  template backend must admit every new operation. It must validate a dynamic
  count before a machine shift; x86's implicit masking is not the contract.
- There is no native-to-VM crossing or runtime algorithm helper on the valid
  path. Operand/destination aliasing must be correct, including register
  constraints for dynamic shift counts.
- Trace/replay and resource accounting include the operations under the
  existing scalar instruction rules.

### 6. Scope stays at the seven basic operations

This decision does not add leading/trailing-zero counts, population count,
rotates, wide multiply, wrapping arithmetic, unsigned comparison, float
bit-casts or new literal syntax.

Some may be justified later. In particular, `mulhi` needs a separate signed
versus unsigned contract and measurements; basic bit operations alone do not
promise an optimal Eisel–Lemire implementation. This ADR neither revises
ADR 0072's accepted algorithm nor authorizes a parser rewrite.

## Implementation and validation

Implement semantics, schema/API resolution, IR plumbing and all three tiers
in one coherent change. Update the generated reference and document the
difference between arithmetic overflow and discarded shift bits.

Required checks:

- Boundary tables for 0, -1, Int.MIN, Int.MAX, alternating masks and every
  valid shift count; invalid counts including the two Int extremes.
- Deterministic randomized results against an independent 64-bit reference;
  AST, encoded VM and native must agree.
- Negative arithmetic-right-shift cases distinguish floor division from
  truncation toward zero.
- Source diagnostics and blame agree across tiers.
- Tests actually enter compiled native functions; include dynamic counts,
  aliasing and constant operands.
- Structural checks show typed operations, no intrinsic boundary, no
  allocation and no native refusal introduced by these operations.
- Run the repository's implementation gates, including the native and dogfood
  steps, as documented in CLAUDE.md.

Measure a fixed bit-manipulation workload against its existing arithmetic
implementation where available. Report instructions, fuel, allocations,
native helper calls, code size and wall time. Use unchanged covefmt/cq
workloads as controls for build or layout effects. Report measurements as
measurements; this design makes no numerical speedup claim.

## Alternatives

- **Continue arithmetic emulation:** possible, as ADR 0072 shows, but obscures
  masks and signed bit patterns and costs work unrelated to the algorithm.
- **Add UInt first:** introduces another numeric type and conversion rules;
  the seven operations are completely defined without it.
- **Add operators now:** familiar, but adds grammar and precedence choices
  unnecessary to provide the capability.
- **Mask shift counts:** convenient on some CPUs, but silently turns a likely
  erroneous count of 64 into a shift by zero.
- **Check left-shift overflow like multiplication:** prevents ordinary bit
  manipulation and conflates a representation operation with arithmetic.

## Consequences

Cove gains a small, public set of machine-representation operations.
Algorithms over those operations stay visible as ordinary Cove control flow.
Signed Int arithmetic remains checked, shift behavior is portable, and the
native tier can compile the basic operations directly.

The cost is seven operation semantics and their compiler/runtime plumbing.
Additional numeric capabilities and parser optimizations remain separate,
measured decisions.
