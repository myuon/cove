# ADR 0106: UTF-8 over bytes, and an `Int` written in a radix, are Cove

- Status: Accepted
- Date: 2026-10-21
- Decides: that `String` has `toUtf8() -> Array<Int>` and the associated
  functions `fromUtf8(bytes: Array<Int>) -> Result<String, Error>` and
  `fromUtf8Lossy(bytes: Array<Int>) -> String`; that `Int` has
  `toStringRadix(radix: Int) -> String` and `toHex(width: Int) -> String`;
  that all five are Cove in `std.string` and `std.int`, with **no instruction
  of their own**; that well-formed means Unicode §3.9's Table 3-7 exactly, and
  that the lossy reading replaces one maximal subpart with one U+FFFD, as the
  WHATWG Encoding Standard's decoder does; that digits past nine are
  **lowercase**; and that a radix outside `2..=36` and a negative width stop
  the run. Recorded for [issue #665](https://github.com/myuon/cove/issues/665)
- Supersedes: nothing
- Refers to, without superseding:
  [issue #646](https://github.com/myuon/cove/issues/646), the survey of five
  minicloud apps that found these written by hand five times and wrong five
  times; [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s
  Decision 2 and
  [ADR 0073](0073-a-primitive-below-the-library-is-an-instruction.md)'s
  Decision 2, which say where a new primitive would go and why none is needed
  here; [ADR 0062](0062-an-append-is-ensure-store-commit.md), the append all
  three writers stand on; and
  [ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md), the refusal
  `parseRadix` already uses for a radix

## Context

Issue #646 counted what five minicloud apps copy between them. Two rows of it
are this ADR.

**`decodeUtf8` is in all five, and all five accept overlong forms.**
`apps/admin/text/text.cove:105-147` and its four copies read a lead byte,
take the continuation bytes it calls for and hand the code point to
`String.fromCodePoint`. That catches a negative point, a point past U+10FFFF
and a surrogate, because `fromCodePoint` sees the number; it cannot catch an
overlong form, because the number an overlong form spells is a legal one.
`E0 80 80` decodes to U+0000, `F0 8F BF BF` to U+FFFF. Whether a sequence was
the shortest one is knowable only while it is being read. Each copy also
builds one `String` per code point into a `Vector` and joins them at the end.
There is no inverse at all: a program that has a `String` and wants its bytes
writes the `byteAt` loop.

**A byte as two hex digits is in three spellings**, `hexDigit`, `hexByte` and
`hex4`, plus `std.json`'s own `\u00XX`, each indexing into a literal
`"0123456789abcdef"` or its uppercase twin. `Int.parseRadix` reads hex and
nothing writes it.

@myuon decided on 2026-10-21 to add both to the standard library. URL decoding
(#666) is the next consumer.

## Decision

### 1. The interface

```cove
text.toUtf8()                -> Array<Int>
String.fromUtf8(bytes)       -> Result<String, Error>
String.fromUtf8Lossy(bytes)  -> String
value.toStringRadix(radix)   -> String
value.toHex(width)           -> String
```

The names follow what is there. `fromUtf8` sits beside `fromCodePoint`, an
associated function on `String` that builds one out of something that is not
one; `toUtf8` is its inverse and a method, as `chars()` is. `toStringRadix` is
`parseRadix`'s inverse and says so in the name; it is a method on the value
rather than an associated function on `Int`, because the value is the thing
being written, as `Float.format(digits)` is.

`fromUtf8` answers `Result<String, Error>` because that is what every fallible
`String` constructor answers: `fromCodePoint` and `sliceBytes` both do, and
both for this reason — the input is data that can arrive wrong, not a call the
program got wrong. The message names the byte offset where the first
ill-formed sequence begins, and says differently when the element there is not
a byte at all. There is no structured error: no `Error` in the standard
library carries more than a message, and a caller that wants to recover
characters calls the lossy one rather than resuming at an offset.

**`toHex` is a method of its own and not a width parameter on
`toStringRadix`.** A builtin parameter is a name and a type and nothing else —
`parseRadix` is a second function beside `parse` for exactly this reason — so a
width on `toStringRadix` would be an argument every caller writes, and every
caller but the hex ones would write `0`. The fixed-width uses #646 found are
all radix 16 (a percent-escape's two digits, a `\u` escape's four), so the
padded form is the hex one, and `padStart` on a `String` is not added for it.

**Lowercase.** `std.json` already writes `é`, and Rust's `{:x}`,
JavaScript's `toString(16)`, Go's `strconv.FormatInt` and Python's `hex()` all
write lowercase; `parseRadix` reads either, so no round trip depends on the
choice. RFC 3986 asks that percent-encoding *should* use uppercase, so a URL
encoder (#666) uppercases its two digits or keeps its own two-entry table; that
is one consumer's rule and is written there rather than made the default for
every other.

**Negatives are sign and magnitude**, `(-255).toStringRadix(16)` being
`"-ff"`, because that is the notation `parseRadix` reads: so
`Int.parseRadix(v.toStringRadix(r), r)` is `Ok(v)` for every `Int` and every
radix, the least `Int` included, and `Int.parseRadix(v.toHex(w), 16)` is
`Ok(v)` for every width. A two's-complement rendering would need a width to
mean anything and would not read back. `toHex`'s padding goes between the sign
and the digits, `(-10).toHex(4)` being `"-000a"`; the width counts digits and
not the sign, and is a minimum and never a bound, so a value is never cut.

**A radix outside `2..=36` and a negative width stop the run** through
`core.refuse`, for `parseRadix`'s reason and `Vector.filled`'s: neither names
anything to write, which is the call being wrong rather than an answer to give.
The radix refusal shares `parseRadix`'s rule and help sentences.

### 2. The definition of well-formed, and of what one U+FFFD replaces

**Strict** is Unicode 17's Table 3-7 and nothing looser: `00..7F`; `C2..DF`
then one continuation; `E0` then `A0..BF`, `E1..EC` and `EE..EF` then
`80..BF`, `ED` then `80..9F`, each then one more continuation; `F0` then
`90..BF`, `F1..F3` then `80..BF`, `F4` then `80..8F`, each then two more. So
`C0`, `C1` and `F5..FF` lead nothing, an overlong form is refused by its
second byte's lower bound and a surrogate or a value past U+10FFFF by its upper
one. An element outside `0..=255` is not a byte and is ill-formed too.

**Lossy** replaces each *maximal subpart of an ill-formed subsequence* with one
U+FFFD — Unicode's §3.9 "U+FFFD Substitution of Maximal Subparts", which the
WHATWG Encoding Standard's UTF-8 decoder makes normative for the web, and which
Rust's `String::from_utf8_lossy` and Python's `errors="replace"` also follow. A
lead byte and as many continuations as could still have completed it are one
U+FFFD; the byte that showed the sequence could not complete is read again as
the start of what follows. So `E1 80 41` is U+FFFD then `A`, and `E0 80 80` is
three U+FFFDs, because `80` cannot follow `E0` at all. Choosing this over the
older "one U+FFFD per bad byte" or "one per bad run" means a page decoded here
shows its replacement characters exactly where a browser shows them, which is
what a host serving the bytes back to a browser needs.

### 3. All five are Cove, and nothing is added below the library

ADR 0073 gives a new primitive two homes: an IR instruction admitted under ADR
0064's Decision 2 — an operation of the machine, that a method could be renamed
without renaming — or Cove in the standard library. Each of these five is a
policy over a representation that the substrate already reaches:

- **`toUtf8`** is `core.byteLength`, one `byteAt` a byte and ADR 0062's
  ensure, store and commit into `core.vectorWithCapacity(length)`, finished by
  `core.vectorFinish`. It is `std.string.chars` with an `Int` where the slice
  was, and it decodes nothing because a `String` is already its UTF-8.
- **`fromUtf8` and `fromUtf8Lossy`** are one function, `decodeInto`, the
  WHATWG decoder transcribed: `needed`, `seen`, a `lower` and `upper` bound for
  the next byte, and a `for` over the array. A completed sequence is written by
  `std.string.writePoint`, which `toUpper` and `toLower` already write
  through, into a buffer `core.bytesAllocate`d once at `bytes.length()` and
  finished by `core.bytesFinish`. Which byte sequences are text is exactly the
  kind of rule ADR 0064 keeps out of the machine — `fromCodePoint`'s surrogate
  hole is the precedent — and the machine's one UTF-8 rule, that
  `bytesFinish` refuses a run that is not valid, is unchanged and can only
  agree, since a well-formed sequence is the one encoding of its scalar.
- **`toStringRadix` and `toHex`** are `std.int.renderInto`'s loop with the
  radix where the ten was, on the non-positive side of zero for its reason, the
  least `Int`. The digit count is known before the first digit is written, so
  the answer is allocated once at exactly its length.

A Rust decode behind an instruction would be faster per byte on the VM and is
not taken, for ADR 0064's reasons and one more: the native tier compiles these
loops, where it could only call out of compiled code to an instruction's
helper. Nothing in the repository decodes bytes in a hot loop today, and the
consumers in view — form bodies and query strings, a few hundred bytes a
request — are not where a constant factor on a linear pass changes a
performance class.

A rough measure, to say which class this is in and nothing finer: twenty
decodes, ten strict and ten lossy, of a 680,000-byte array of one-, two-,
three- and four-byte text, with the `toUtf8` that made it, run whole in
**1.82 s on the VM and 0.34 s on the native tier** on the machine this was
written on — at most 134 and 25 ns a byte, the setup included. Linear, one pass, one
allocation of the answer.

### 4. `std.json` writes its `\u00XX` with `toHex(4)`

`std.json`'s escape of a control character was a `"0123456789abcdef"` literal
and two `sliceBytes(…).unwrapOr("")`; it is `"\\u{byte.toHex(4)}"` now. The
output is the same bytes — lowercase, four digits — so nothing a program reads
from `Json.render` changes.

## Consequences

- The five minicloud copies of `decodeUtf8` can be replaced by
  `String.fromUtf8Lossy`, and doing so fixes their overlong-form bug. Their
  hex helpers become `toHex(2)`, plus a `toUpper()` where they percent-encode.
- Every program carries the five new bodies, as it carries the rest of
  `std.string` and `std.int`. They add no forwardable copy to any of them:
  `crates/cove-cli/tests/copies.rs` rises by 72, which is 78 for the four new
  end-to-end cases and −6 for `std.json`'s escape. A first draft of
  `fromUtf8`'s refusal was +530, a copy and an `unwrapOr<Int>` in every
  program, and was rewritten to match on the `Option` instead. Their refusal
  sentences are 38 words of literals every program places, which two heap
  fixtures in `cove-runtime` account for.
- `tests/e2e/values_string_utf8` and `tests/e2e/values_int_to_string_radix`
  pin the answers on the interpreter, the encoded VM and the native tier:
  every width boundary, every ill-formed shape named above, all 256 single
  bytes, round trips over a stride of the code space and of `Int` in every
  radix. `fail_int_to_string_radix` and `fail_int_to_hex_width` pin the two
  refusals word for word.
- **Not decided here**: a `Bytes` type. `Array<Int>` is what the host hands
  over and what the copies already use, at a word per byte; a packed byte type
  is a question for when a program measures that cost.
