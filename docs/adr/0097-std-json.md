# ADR 0097: JSON is `std.json`, and a name written twice is an error

- Status: Accepted
- Date: 2026-10-14
- Supersedes nothing. No ADR decided where JSON lives; it lived in
  `examples/cq/json` because that is where it was first written, and in two
  copies of that file in minicloud because packages cannot share a module
- Refers to: [issue #647](https://github.com/myuon/cove/issues/647), the
  decision to add the module; [issue #602](https://github.com/myuon/cove/issues/602),
  the evidence and the decision that packages do not share modules;
  [ADR 0069](0069-structural-reflection-stays-inside-the-standard-library.md),
  which keeps structural reflection inside the standard library;
  [PHILOSOPHY](../PHILOSOPHY.md)'s "Earn complexity through use", "Strong
  defaults" and "Preserve the performance class"
- Decides: that the standard library has a module `std.json` whose interface
  is the one below; that `parse` refuses an object naming a field twice; that
  a whole-package lowering lowers `std.json` only as far as the package
  reaches it; that `examples/cq` and `examples/covecheck` use it and the copy
  they shared is deleted

## Context

Three programs held the same JSON module. `examples/cq/json/json.cove` was
the first, written for `cq`'s JSON Lines records and imported from there by
`examples/covecheck`. minicloud's webhook lab copied it whole and grew it —
`\u` escapes read and written, surrogate pairs joined, every control character
escaped on the way out, an indented renderer for its detail page — and its
ledger copied the webhook lab's and grew it once more, refusing an object that
names a field twice, because "a posted result whose `p99` appears twice has no
one value to keep". minicloud's apps cannot import from each other (issue #602
decided they will not), so each copy will go on drifting unless the module is
somewhere every package already has.

Diffing the three, they share all of their shape and disagree on nothing they
both do:

- `enum Json { Null, Bool(Bool), Number(Float), Text(String), Items(Array<Json>), Fields(Map<String, Json>) }`;
- `parse(String) -> Result<Json, Detail>`, with `Detail { column, message }`
  in a module of the program's own (`cq.diag`) or of the copy's;
- `render(Json) -> String`, one line, fields in `Map` order;
- `field(Json, String) -> Option<Json>`;
- the same 64-level nesting bound, the same hand-walked number grammar, the
  same refusal of `1e999` as an infinity no input meant.

What they disagree on is only what one copy grew and another did not. Every
one of those is a refusal in `cq.json` that a later copy turned into the
right answer, plus two helpers the minicloud apps used for their own records
(`text`, `int`, `bool` with fallbacks, `textMap`, `fromTextMap`).

That is the "recurring friction" PHILOSOPHY asks for, and an unusually clean
case of it: three programs, one interface, and every extension already
written once and used.

## Decision

### The interface

```cove
export struct Detail { line: Int, column: Int, message: String }

export enum Json {
  Null
  Bool(Bool)
  Number(Float)
  Text(String)
  Items(Array<Json>)
  Fields(Map<String, Json>)
}

export fn parse(text: String) -> Result<Json, Detail>
export fn render(value: Json) -> String
export fn renderPretty(value: Json) -> String
export fn field(value: Json, name: String) -> Option<Json>

export fn asText(value: Json) -> Option<String>
export fn asNumber(value: Json) -> Option<Float>
export fn asWhole(value: Json) -> Option<Int>
export fn asBool(value: Json) -> Option<Bool>
export fn asItems(value: Json) -> Option<Array<Json>>
export fn asFields(value: Json) -> Option<Map<String, Json>>
```

A program writes `use std.json` and `json.parse(line)`, or imports the names
it uses. Nothing in `cove_schema` binds any of it and nothing in the lowering
names it: it is a library module like `std.stringbuilder`, lowered from Cove
like the program that calls it.

**The shared shape is kept as it was.** `Json`'s six cases, `parse`, `render`
and `field` are the three copies' common interface, unchanged, so a copy moves
onto this by changing its `use` lines.

**`Number` stays `Float`.** All three copies hold the number and not its
spelling, and none keeps an `Int` beside it. They agree, so there was no
choice between representations to make; what was checked is that the agreed
one round-trips. Every number up to 2^53 is exact, `render` writes a whole
number without a decimal point, and `-0` is written `-0` (the copies wrote
`0`; the sign is part of the `Float` and costs one comparison to keep). An
integer past 2^53 is rounded to the nearest `Float` — `9007199254740993` reads
as `9007199254740992` — and the module's documentation says that an identifier
that wide should travel as a string. Keeping the source text, or an `Int` case
beside `Float`, would round-trip more digits and would put a second number
type into a format that has one; nothing in the three programs needed it. A
`Float` JSON cannot write, an infinity or `NaN`, is rendered `null` rather
than as text no parser reads.

**`Detail` gains `line`.** The copies parsed one line at a time and reported a
column; `renderPretty` writes text over many lines and `parse` has to read it
back, so a column counted from the start of the text would point nowhere an
editor can go. `line` and `column` both count from 1 and `column` counts
characters within its line. A JSON Lines record is always line 1, so `cq`'s
own `Detail` (column only, located by `cq.diag.locate`) takes the column and
drops the line, and every diagnostic `cq` writes is byte-for-byte what it was.
The messages are the copies' messages, including the ones that say "line".

**The union of the extensions is taken.** From the minicloud copies:

- `\u` escapes, four hex digits of either case;
- a surrogate pair read as the one character it names, and either half alone
  **refused** — a Cove `String` holds only scalar values, and replacing a
  lone half with U+FFFD would hand a program text its input did not hold;
- `\b` and `\f`, which `cq.json` refused only because Cove literals could not
  write those characters before [ADR 0094](0094-a-string-names-a-scalar-by-number-with-u.md);
- every control character escaped by `render`, with the two-character form
  where JSON has one and `\u00XX` otherwise, so any `String` round-trips;
- `renderPretty`: two spaces a level, `": "` after a name, empty containers
  kept as `[]` and `{}`.

**The accessors answer `Option`, and take a value, not a name.** The
minicloud helpers took an object and a name and answered a fallback —
`text(value, "id")` was `""` for a missing field, a number, and a string
`""` alike. A fallback is the caller's decision, and a standard library that
makes it for them is the silent default PHILOSOPHY's "Strong defaults" warns
against. So each accessor answers its own case's payload or `None`, and a
caller composes `field` with one and chooses its own fallback with
`unwrapOr`. `asWhole` is the one with a judgement in it: it answers a number
that is exactly an `Int` and `None` for `3.5` or `1e300`, which is the rule
`cq.records` already applied to a count. `textMap` and `fromTextMap` were one
app's record shape and are not taken.

### A name written twice is an error

RFC 8259 says the names within an object "SHOULD be unique" and leaves a
parser that meets a duplicate free to keep the last, keep the first, or
refuse. `Map.inserted` keeps the last, so `cq.json` kept the last value
without saying so.

`std.json` refuses: `{"p99": 1, "p99": 2}` is
`the field \`p99\` appears twice in one object`, located at the second name.
Two spellings of one name — `"a"` and `"a"` — are one name, because
names are compared after escapes are read. The rule is per object; the same
name in two objects is two fields.

The reason is that every other choice is a guess about which value the writer
meant, and the guess is invisible. Two parsers that guess differently read
one document as two different values — the shape of a request-smuggling bug,
and the ledger's reason. A program that has a real use for duplicates is
reading something that is not quite JSON and can say so with a parser of its
own; a program that did not expect them is told at the line that has one. This
is the same line the module draws at `01`, `1e999` and a lone surrogate:
input the format does not mean one thing by is refused rather than read as one
of the things it might mean.

The check costs no search. `inserted` already looks the name up, so a
duplicate is the map not growing: `fields.length()` before and after, one
machine instruction each. The consequence is the order two errors are found
in — a duplicate name whose value is also malformed reports the value, which
is read before it is inserted — and a test pins it.

### A program has `std.json` only by importing it

Every other standard-library module is part of the language: a builtin
method is bound to it, or the lowering names it (an interpolation's appends
are `std.stringbuilder`'s, `==` on erased values is `std.dynamic`'s). `std.json`
is neither, and it is the first such module. A sliced lowering — `cove run`,
`cove test`, `cove build`, every embedding entry — already lowered only what
its roots reach, so a program that never imports `std.json` never paid for it.
The whole-package lowering, `cove_ir::lower`, lowered *every* declaration,
library included; it is what listings, the differential harness and the
corpus surveys use. Attaching `std.json` to it made every one of those
programs carry a JSON parser it never imports: `copies.rs`'s ratchet rose
from 7,570 to 16,050, about 31 copies in each of 271 programs, and a dozen
lowering tests whose premise was "the library instantiates nothing generic"
failed on `Map<String, Json>`.

So `cove_sema::stdlib::is_imported_library` names the modules a program has
only by importing them — `std.json` alone — and `cove_ir::lower` lowers their
declarations only as far as the rest of the package reaches: the call graph's
reach first, then the same rounds a sliced lowering runs for a function
reached as a value. Every other library module is lowered whole as before.

### `cq` and `covecheck` move onto it

`examples/cq/json` is deleted. `cq.records`, `cq.rows` and the two programs
import `std.json`, and `cq.diag.fromJson` turns a `std.json.Detail` into
`cq`'s own. `examples/covecheck` imported `cq.json` and now imports
`std.json`. The 25 tests of `cq/json/json_test.cove` move to
`tests/e2e/std_json`, a package of its own run by `cove test` on both
backends, where 45 tests now cover the module: every kind, every escape,
surrogate pairs and lone halves, numbers at their edges, whitespace, both
nesting bounds, every error with its line, column and message, duplicates,
and both renderers' round trips.

## Consequences

**Every program checks 799 more lines of library.** `std.json` is parsed and
checked with the rest of the standard library once per process, whether a
program uses it or not (ADR 0083), and lowered only where a program calls it.
Measured as `cove run hello` over the whole `examples/` package, ten
interleaved runs of each binary: a median of 192.6 ms before and 196.2 ms
after, **+3.6 ms a compile**. That is the standard library growing by about
nine per cent, and it is paid once per process.

**`copies.rs`'s ratchet falls by 48** — 7,570 to 7,522 when it was measured,
and 7,618 to 7,570 over issue #649's `Vector.filled`, which landed between.
The 84 of `cq.json`'s tests
left the survey with the tests; the 36 `std.json` adds over `cq.json` are in
the three rows that use it and nowhere else.

**`cq` runs the same parser, with two additions, and they were measured.**
On a 20,000-record file of `cq.sample`'s, on the encoded VM: `revenue-summary`
executes 261,642,960 instructions before and 262,742,960 after (**+0.42%**,
exactly 55 a record — the duplicate check's two `length` reads and compare
per field), and `confirmed-bookings` 297,938,373 and 303,499,397 (**+1.87%**).
The difference between the two is the renderer: `confirmed-bookings` writes
13,385 rows through `render`, and `cq.json` asked whether a string needed
escaping with five `contains` calls for its five escapes, where `std.json`
walks the bytes once looking for every control character too. That is the
price of `render` being correct for any `String`, and it is within the class.
Wall time did not move beyond run-to-run noise: medians of five interleaved
runs were 2.047 s and 2.109 s (`revenue-summary`) and 2.651 s and 2.675 s
(`confirmed-bookings`) on the VM, on a machine other builds were loading. On
the native tier the counts dispatched by the encoded tier are identical
(760,159 and 1,001,024), and the medians were 0.683 s and 0.679 s, 1.037 s and
1.016 s. Every
output file and every line `cq` writes is byte-for-byte identical on both
tiers.

**A `Json` is not a bridge to a program's types, and this does not build
one.** A program reads a record into a struct field by field, as `cq.records`
does. A derived `Json` ⇄ struct conversion would need what ADR 0069 keeps
inside the standard library — a structural view of an erased value — or a
derive mechanism the language does not have, and `http.json`, which encodes
an `Any` as JSON in the host, is a separate path this does not touch. If
programs show that writing the conversion by hand is the recurring friction,
that is a later ADR, and it would start from ADR 0069's reasons.

**minicloud's copies are not changed here.** They can drop their `json/`
directories by changing `use` lines, and their `text`/`int`/`bool` helpers
become `field` composed with an accessor and the fallback they choose.
