# ADR 0046: A byte offset is a value a `String` hands out, not an index into it

- Status: Accepted
- Date: 2026-09-08
- Decides: the smallest set of `String` primitives a UTF-8 scanner can be
  written against without allocating per character, whether a code point is a
  type or an `Int`, and which of the methods that were waiting on the answer
  it actually frees — for
  [issue #292](https://github.com/myuon/cove/issues/292)
- Supersedes nothing.
  [ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md) named
  this an open question and declined to answer it; this answers it and
  contradicts nothing it decided

## Context

`chars()` is the only cheap way into a `String`, and what it answers is an
`Array<String>` — one heap string per character, because a character in Cove
is a `String` of length 1 and there is no other type for it to be.

Everything a Cove program does per character goes through that. The two
hand-written parsers in the repository, `examples/cq/csv` and
`examples/cq/json`, both open with `text.chars()` and walk the array with a
cursor. `examples/cq/README.md` measured what that costs — **1.35 µs a
character**, about a thousand times a builtin doing the same work — and found
the reason to be structural rather than incidental: every access allocates an
`Option` and every character is a heap `Rc<str>`.

It also blocked things. ADR 0042 sorted every builtin into primitive, library
or capability and left five `String` methods and three `parse` methods
unsorted behind one sentence: each is expressible over a smaller string
primitive, and **which** primitive is an open question. Nothing in the corpus
converts a digit to its numeric value, because over `chars()` there is no way
to — a digit is a `String`, so its value is a lookup rather than arithmetic,
and `cq.json`'s number parser walks the grammar only to validate it and then
hands the matched text to `Float.parse`.

Three facts about the surface shaped the answer more than any of that.

**`String` already has an index space.** `slice`, `indexOf` and `length` all
count characters, deliberately and in writing — `crates/cove-runtime/src/vm/builtins/text.rs`
says so at the top of the file, and the schema's own doc comment said an API
that mixed characters and bytes "never has the chance to become a trap".

**The bytes are already there.** The VM object is UTF-8 packed eight bytes to
a word behind a header whose length field is the **byte** count, so a byte
length is one word read that the language could not see.

**Every `String` builtin in the VM copies the whole string first.**
`text.rs`'s receiver goes through `string_of`, which is
`String::from_utf8(machine.string_bytes(addr))` — a `Vec<u8>`, a byte-at-a-time
unpack, and a full UTF-8 validation, on every call. `s.length()` on a
one-mebibyte string copies a mebibyte.

## Decision

**Three operations count bytes, and every one of them says so in its name.**

```
String.byteLength() -> Int
String.codePointAtByte(offset: Int) -> Option<Int>
String.sliceBytes(from: Int, to: Int) -> Result<String, Error>
```

Everything else is unchanged. `length`, `slice` and `indexOf` still count
characters and none of them moves; there is no breaking change here, and a
program that never writes `Byte` never meets a byte offset.

### A byte offset is not an index

Adding a byte-offset primitive puts a second index space on a type that has
one, where `slice(0, 3)` and a byte walk disagree on every string with a
multi-byte character in it — silently, and only sometimes.

Three ways out were open: make everything byte-indexed and break three shipped
methods; make the new primitives character-indexed and pay O(n) an access; or
keep both and stop them mixing. **This takes the third, and what stops them
mixing is the naming rule.** A byte offset is a value `byteLength` and
`codePointAtByte` hand out and `codePointAtByte` and `sliceBytes` take back.
No method without `Byte` in its name accepts one, and none is documented as a
general way to index a `String`.

That is a convention and not a type, and it is worth being plain about it:
nothing in the checker refuses `slice(0, byteLength())`. What the rule buys is
that the mistake is **visible in the source** rather than only in the answer.

### A code point is an `Int`

There is no `Char` type and this does not add one.

A `Char` buys type safety and costs a literal syntax, an ordering, an
equality, a `Map` key, and an arm everywhere the checker enumerates builtin
types. That cost is not a guess. A new builtin aggregate is `MapEntry`-scale,
and `MapEntry` needs its own `BuiltinType` variant, its own `Ty` variant with
about twenty-five exhaustive-match sites in `crates/cove-sema/src/typeck.rs`
alone, a hardcoded construction branch in the interpreter, layout handling in
the VM, arms in four lowering files, and an arm at the embedding boundary.

What the observed use wants is a scalar it can do arithmetic on. The corpus's
one character classifier is `character >= "0" && character <= "9"`; over a
scalar that is `codePoint >= 48 && codePoint <= 57`, and a digit's value is
`codePoint - 48`. **That difference is the whole of what ADR 0042's Library
test means by *natural*.**

`String.fromCodePoint(Int) -> Result<String, Error>` already said a code point
is an `Int` in the other direction. This is its inverse and the two now agree.

### There is no `nextByteOffset`, because the width is a function of the value

The obvious shape for the reader is a pair — the scalar, and where the next
one starts. It is not needed. **A `String` is valid UTF-8 in its shortest
form**, so a scalar below 0x80 occupies one byte, below 0x800 two, below
0x10000 three, and four otherwise, which Cove computes in three comparisons.

So `codePointAtByte` answers a bare `Option<Int>`. Two other shapes were
considered and rejected: a `CodePointStep { codePoint, nextByteOffset }`
struct, which is the `MapEntry`-scale wiring above for a two-field aggregate;
and packing both into one `Int` as `codePoint | nextOffset << 21`, unpacked
inside the standard library, which is the same information in a form that has
to be explained. **Neither buys anything the three comparisons do not.**

### `None` conflates three things, and `sliceBytes` refuses rather than clamps

`codePointAtByte` answers `None` at the end of the string, before its start,
and inside a character. A scanner that starts at 0 and advances by the width
of what it read reaches none of the last two, so telling them apart would put
a `Result` on the one path this exists for. It is **total**: nothing written
over it inherits a trap.

`sliceBytes` is the opposite, because its arguments do come out of a program's
arithmetic. It refuses an offset that is out of range, out of order, or inside
a character — which is the one place it parts from `slice`. `slice` clamps
because a character position out of range is a caller's arithmetic about a
sequence it can count; a byte offset that is out of range or interior is one
this type never handed out, and quietly moving it to the nearest legal one
would answer a question nobody asked. **The invariant that a `String` is valid
UTF-8 is kept here, at the primitive, and not anywhere downstream.**

### The machine answers where, what, and which bytes — and nothing else

`contains`, `startsWith`, `endsWith`, `split` and `parse` are not instructions
and do not become any. `byteLength` is the object header's length field.
`codePointAtByte` reads the lead byte out of a payload word and at most three
more. `sliceBytes` checks two bytes and copies the range. None of the three
goes through `string_of`, and they are the first `String` operations in this
backend that do not.

## What it measured

One build — `27109f3` plus this change, `--release`, one machine, nothing else
running — following
[ADR 0029](0029-a-benchmark-number-is-evidence-within-one-build.md).
`examples/cq/README.md`'s 90.8 s for the same program is an older tree and is
**not** the comparison; the baseline was taken again. `fuel_spent` and
`allocated_words` repeat to the digit; wall time repeated within 0.7%.

### The scanner, which is what the primitives are for

`benches/chars` against `benches/bytescan`: the same line, the same 32,000
rounds, the same answer, and the only difference is how a character is
reached.

| | wall | fuel | allocated words |
| --- | ---: | ---: | ---: |
| `chars()` and an `Array<String>` | 698 ms | 58,240,025 | **5,984,027** |
| a byte cursor | **418 ms** | 26,432,025 | **24** |
| a byte cursor, `width` behind a call | 504 ms | 36,352,025 | 24 |

**The allocation goes to nothing.** Not less — 24 words for the whole run,
which is the fixture rather than the loop. The 1.67× and the 2.2× less fuel
are the smaller half of that result.

The third row is what writing `width` as a function instead of inline costs:
**20%**. It is recorded because a program that reaches for the helper pays it,
and because it is the same finding `examples/cq/README.md` already had for a
method on a struct receiver.

### The parsers that exist

`examples/cq` over a generated 17 MB, 100,000-record JSON Lines file, and over
120,000 CSV records, each program run twice.

| | wall | fuel | allocated words |
| --- | ---: | ---: | ---: |
| `revenue-summary`, `chars()` | 23.79 s | 1,233,348,640 | 104,300,680 |
| `revenue-summary`, byte cursor | **17.90 s** | 1,169,365,142 | **36,512,897** |
| `confirmed-bookings`, `chars()` | 27.51 s | 1,290,688,627 | 120,312,065 |
| `confirmed-bookings`, byte cursor | **21.66 s** | 1,226,705,129 | **52,524,282** |
| `rate-card` (CSV), `chars()` | 10.75 s | 319,062,153 | 54,200,798 |
| `rate-card` (CSV), byte cursor | **7.91 s** | 260,621,815 | **27,140,652** |

1.33×, 1.27× and 1.36×, and **2.9×, 2.3× and 2.0× less allocation.**

The interesting column is `fuel_spent`, which barely moved — 5% on the JSON
rows — while wall time fell a quarter. **What was bought is allocation and
collection, not instructions**, which is the same thing
`examples/cq/README.md` meant when it said `fuel_spent` is not tracking what
is expensive.

Both parsers keep their diagnostics to the character and the column, so each
grew a byte-offset-to-column conversion that is O(n) — on the error path only,
taken once for a record that is wrong and never for one that is right. That is
what made a byte cursor affordable at all.

### `contains` and `Int.parse`, written in Cove

`benches/stringlib`, 20,000 iterations over the same inputs, against the
builtins they would replace.

| | wall | fuel | against the builtin |
| --- | ---: | ---: | ---: |
| `contains`, builtin | 7.8 ms | 340,030 | — |
| `contains`, Cove, scalar by scalar | 793 ms | 39,960,030 | **101×** |
| `contains`, Cove, slice and compare | 764 ms | 33,480,030 | 98× |
| `Int.parse`, builtin | 7.1 ms | 400,031 | — |
| `Int.parse`, Cove | 44.8 ms | 3,500,031 | **6.3×** |

**These are not the same answer, and that is the finding.**

`contains` is a change of performance class. The builtin is `str::find`; the
Cove body is a loop at Cove's per-character cost. **`String.contains`,
`startsWith` and `endsWith` stay primitive**, and #292's success condition was
written so that this counts as an answer rather than a failure. One
qualification, because the ratio invites over-reading: the haystack is 61
bytes and the needle is near its end, so 101× is per-operation overhead rather
than asymptotics, and a longer haystack makes it worse.

`Int.parse` is 6.3×, which is a constant, and its body is
`value * 10 + (digit - 48)`. Whether that constant is worth the two Rust
implementations it deletes is [#254](https://github.com/myuon/cove/issues/254)'s
decision — but it is now a decision with a number in front of it.

One expressibility finding came out of writing `contains` and is worth
keeping: **it cannot be written byte-wise over these primitives.**
`codePointAtByte` answers `None` inside a character, so two different
characters' continuation bytes compare equal and a byte-wise loop finds
matches that are not there. Comparing scalars from a boundary is correct
because UTF-8 is self-synchronising, and it is the only correct shape here.

## Consequences

**`String` has two index spaces and a naming rule**, and the rule is not
enforced by the checker. If it is broken in practice — a program passing a
byte offset to `slice` and getting a wrong answer rather than an error — that
is the evidence for revisiting it, and the answer then is an opaque offset
type rather than a `Char`.

**`length()` is O(n) and `byteLength()` is O(1)** on the same string, and they
answer different numbers. That was already true in the implementation and is
now visible in the surface.

**Nothing has moved to the standard library.** This adds three primitives and
migrates no method. What it changes for #254 is that the blocked rows now
divide: `contains`, `startsWith` and `endsWith` are answered and stay;
`Int.parse`, `Int.parseRadix` and `Float.parse` are unblocked and waiting on
whether 6.3× is worth paying. `words` is now expressible over these and is
unmeasured. `join` was never this issue's and is
[#293](https://github.com/myuon/cove/issues/293).

**A string builder is still missing.** `sliceBytes` removed the quadratic
append from both parsers, because a field is now one slice rather than a
character at a time — but it removed it by not appending, not by making
appending linear. A program that genuinely builds a string from parts still
writes `"{a}{b}"` and still copies. That is #293 and this does not close it.

**There is no readable way to write a code point.** Cove has no top-level
`let` or `const`, so a scalar constant is a magic number with a comment or a
zero-argument function, and the function costs 20% in a loop. What the
rewritten parsers say is `if head == 123 {` with `// \`{\`` after it, and a
reader of the corpus will dislike it. It was not designed away, and it is recorded here as the price
rather than hidden. If it becomes the recurring friction PHILOSOPHY.md's
"Earn complexity through use" asks for, the answer is a code-point literal
that is an `Int` — new syntax, which would have to earn its place on that
evidence rather than on this sentence.

## What shipped with it

- `crates/cove-schema/src/builtins.rs` — three rows, and a `String` doc
  comment that now states the naming rule as well as the character-index one.
- `crates/cove-runtime/src/builtins.rs` — the oracle's three, over `Rc<str>`.
- `crates/cove-runtime/src/vm/builtins/text.rs` — the machine's three, plus
  `receiver_addr`, `byte_at` and `decode`, which are why they are worth
  having.
- `crates/cove-ir/src/lower/methods.rs` — three rows in `MACHINE_METHODS`. No
  new instruction, no new `Ty`, no golden listing changed.
- `examples/cq/json/json.cove`, `examples/cq/csv/csv.cove` — both parsers.
- `benches/bytescan`, `benches/stringlib` — the rows above.
- `tests/e2e/values_string` — the three on `"aあいbう"`, every refusal
  included, run on both backends against one `expected.out`.

`KNOWN_DISAGREEMENTS` in `crates/cove-cli/tests/vm_coverage.rs` was empty
before this and is empty after it.

The CSV measurement's input has no generator; it is `rates.csv`'s six rows
repeated, made from `examples/cq/data` with

```console
$ { head -1 rates.csv; for i in $(seq 1 20000); do tail -n +2 rates.csv; done; } > rates-large.csv
```
