# ADR 0110: `Int.toDecimal(width)`, the decimal mirror of `toHex`

- Status: Accepted
- Date: 2026-10-21
- Decides: that `Int` has `toDecimal(width: Int) -> String`, the decimal
  digits of the receiver with zeros in front to make at least `width` of
  them; that the sign goes before the zeros and the width counts digits, not
  the sign, so `(-5).toDecimal(3)` is `"-005"`; that a width smaller than the
  digit count pads nothing; that a negative width stops the run with `toHex`'s
  rule; and that it is Cove in `std.int`, `toHex`'s body at radix 10, with no
  instruction of its own. Recorded for
  [issue #674](https://github.com/myuon/cove/issues/674)
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0106](0106-utf-8-over-bytes-and-int-to-text-in-a-radix-are-cove.md),
  whose `toHex(width)` this mirrors and whose reasons it takes whole

## Context

ADR 0106 added `toHex(width)` as the one padded writer, and said why it was
the hex one: every fixed-width use issue #646 found was radix 16. After the
minicloud apps moved onto the standard library (minicloud #155), the one
helper still copied into three of them is `padded`, a zero-padded decimal
that prepends a `"0"` a turn and so is quadratic in the padding. Its call
sites are a `DateTime`'s month and day (`padded(at.month, 2)`), the fraction
of a fixed-point figure (`padded(hundredths % 100, 2)`) and a key that sorts
as text in time order (`padded(micros, 16)`). `std.datetime` (ADR 0108)
writes its own fields through a private `appendDigits` that no program can
name.

Issue #674 weighed this method against a general `String.padStart(width,
fill)`. @myuon decided on 2026-10-21 for the decimal mirror of `toHex`.

## Decision

```cove
value.toDecimal(width) -> String
```

It is `toHex(width)` with ten where the sixteen was, and every rule ADR 0106
gave `toHex` holds of it for the same reason:

- **`width` is a minimum, never a bound**: a value with more digits is written
  whole, and `0` and `1` both mean no padding, so `value.toDecimal(0)` is
  `"{value}"`.
- **Sign and magnitude, the sign first**: `(-5).toDecimal(3)` is `"-005"`,
  the notation `Int.parse` reads, so `Int.parse(value.toDecimal(width))` is
  `Ok(value)` for every `Int`, the least included.
- **A negative width stops the run** through `core.refuse`, with `toHex`'s
  rule sentence and a message that names `Int.toDecimal`.

It is a method of its own and not a `padStart` on `String`: padding a
number is a statement about its digits — the zeros go after the sign — that
a string padder cannot make, and `padStart` would pad by characters and
allocate the digits first. A general padder for aligning text is not decided
here.

`std.int.digitsIn` is already the body of `toStringRadix` and `toHex`, and is
`toDecimal`'s too. Nothing is added below the library.

`std.datetime`'s field writers are **not** rewritten over it. They append into
one buffer allocated at the stamp's exact length; `toDecimal` answers a
`String` of its own, so a field written through it would be an allocation and
a copy where there is none now, for the same bytes.

## Consequences

- Every program carries one more small body, and it adds no forwardable copy
  to any program that was there before: `copies.rs` rises by 39, all of it the
  two new end-to-end cases. Its refusal adds one five-word literal every
  program places, which the `encoded.rs` heap fixture accounts for.
- `tests/e2e/values_int_to_decimal` pins the answers on the interpreter, the
  encoded VM and the native tier, both ends of `Int` included;
  `fail_int_to_decimal_width` pins the refusal word for word.
