# ADR 0108: Civil dates and RFC 3339 are `std.datetime`, at a fixed offset

- Status: Accepted
- Date: 2026-10-21
- Decides: that reading an instant as a calendar date and a clock, and RFC
  3339 text both ways, are a standard-library module, **`std.datetime`** — not
  `std.time` — that a program has only by importing it; that its interface is
  one struct, `DateTime`, of eight public fields, with `fromUnixMillis`,
  `toUnixMillis`, `formatIso`, `toIso`, `toIsoDate`, `toIsoTime`, `weekday`, `parseIso`,
  `daysInMonth` and `isLeapYear`; that the calendar is the proleptic
  Gregorian one and an offset is a fixed count of minutes, with no time-zone
  database; that `parseIso` answers milliseconds and an `Err` naming the
  byte, requires an offset and truncates a fraction; that fields out of range
  are an `Err` from `toUnixMillis` and an offset out of range given to
  `fromUnixMillis` stops the run; that `Int` gains `floorDiv` and `floorMod`;
  and that all of it is Cove, with no instruction of its own. Recorded for
  [issue #667](https://github.com/myuon/cove/issues/667)
- Supersedes: nothing
- Refers to, without superseding:
  [issue #646](https://github.com/myuon/cove/issues/646), item 5, the survey
  that found this code in four minicloud apps, and
  [issue #602](https://github.com/myuon/cove/issues/602), which left ISO 8601
  "pending: no date/time type in std yet";
  [ADR 0107](0107-std-url.md) and [ADR 0097](0097-std-json.md), the precedent
  for a library module a program imports;
  [ADR 0106](0106-utf-8-over-bytes-and-int-to-text-in-a-radix-are-cove.md),
  the precedent for an `Int` method that is Cove in `std.int`;
  [ADR 0005](0005-module-to-module-imports.md), the order a `use` resolves in;
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2
  and [ADR 0073](0073-a-primitive-below-the-library-is-an-instruction.md)'s
  Decision 2, which say where a new primitive would go; and
  [ADR 0062](0062-an-append-is-ensure-store-commit.md), the append the
  writers write through

## Context

Issue #646 counted what five minicloud apps copy between them. Item 5 is this
ADR.

**`isoTime` and `floorDiv` are in four of them.** `apps/admin/text/text.cove`
(minicloud `601dad0`) and the same in ledger, watch and webhooks, 17 call
sites in all: Howard Hinnant's `civil_from_days` transcribed into Cove, with
a `floorDiv` beside it so that an instant before 1970 lands in the right day,
and a `padded` that zero-fills each field by building strings. Ledger also
has a 110-line `parseIso`, with `daysIn` and `daysFromCivil`, for the
`measuredAt` a client posts.

**What the callers want is often the fields, not the string.** Admin's
`atTime` formats a stamp and slices bytes 11 to 23 back out of it for the
clock. Admin shows every time in Japan Standard Time by formatting
`millis + 9h` as if it were UTC — a stamp ending in a `Z` that is a lie — and
slicing the date, the clock to the second and the clock to the minute out of
that (`jstStamp`, `jst`, `jstClock`, `jstDate`).

What ledger's `parseIso` accepts, read from the code and its tests: a date, a
`T` (or `t`, or a space), `HH:MM:SS`, an optional fraction of any length
truncated to milliseconds, and a required `Z`, `z` or `±HH:MM`. A time with
no zone is refused, "which names no one instant", and so is anything with a
bare date. It answers `Option<Int>`, so the client's error says only that the
text was not a time.

@myuon decided on 2026-10-21 to add a date/time module to std, as sketched in
issue #667: a civil value, `fromUnixMillis` and its inverse, ISO 8601 both
ways, `daysInMonth`, and `Int.floorDiv` and `floorMod` beneath them.

## Decision

### 1. The interface

```cove
use std.datetime

export struct DateTime {
  year: Int, month: Int, day: Int,                 // month 1-12, day 1-31
  hour: Int, minute: Int, second: Int, millisecond: Int,
  offsetMinutes: Int,                              // east of UTC, -1439..1439
}

datetime.fromUnixMillis(millis: Int, offsetMinutes: Int) -> DateTime
datetime.formatIso(millis: Int)         -> String   // fromUnixMillis(millis, 0).toIso()
at.toUnixMillis()                       -> Result<Int, Error>
at.toIso()                              -> String   // 2026-10-05T12:29:49.566+09:00
at.toIsoDate()                          -> String   // 2026-10-05
at.toIsoTime()                          -> String   // 12:29:49.566
at.weekday()                            -> Int      // 1 Monday .. 7 Sunday
datetime.parseIso(text: String)         -> Result<Int, Error>
datetime.daysInMonth(year: Int, month: Int) -> Int
datetime.isLeapYear(year: Int)          -> Bool

value.floorDiv(by: Int) -> Int          // std.int, every program
value.floorMod(by: Int) -> Int
```

A module and not methods on `Int`, for ADR 0107's reason: a calendar is
something some programs read, not something every program carries. It is on
`cove_sema::stdlib`'s imported list, so a program that does not
`use std.datetime` lowers none of it.

`DateTime` is a struct with public fields because the fields are what the
callers want: `at.hour`, `at.day`, `at.weekday()` instead of a slice of a
stamp. It is not `opaque`, so a program may build one from fields it read;
`toUnixMillis` is then the check (§4). The offset is a field, so the value
says which clock it was read at and `toIso` writes the offset that is true —
admin's JST becomes `fromUnixMillis(millis, 540)`, with `+09:00` on it.

`formatIso(millis)` is `fromUnixMillis(millis, 0).toIso()`, byte for byte.
It is a shorthand and earns its place by count: a UTC stamp — a JSON field, a
stored time, a table cell — is what 17 of the copies' call sites write, as
`text.isoTime(ms)`, and each would otherwise name an offset of `0` and a
value it never reads. It takes no offset, because a stamp at another offset
is read off a `DateTime` the caller already wants the fields of.

`toIsoDate` and `toIsoTime` are the two halves `toIso` writes, because the
callers' slices were of exactly those: every byte is ASCII at a fixed width,
so `toIsoTime().sliceBytes(0, 5)` is the clock to the minute. A general
`String.padStart` is not added: every number written here has a fixed width,
and the module writes the digits into its buffer directly.

### 2. The name is `std.datetime`, because a host may have a `time`

minicloud's host module is called `time` (`time.nowMillis()`,
`time.nowMicros()`), and nothing reserves the name: a host names its modules.
A standard-library module `std.time` would be bound by `use std.time` under
its last segment, `time`, which is the name `use time` gives the host module.
What happens then was tried, with a package module in the library's place:

- The two `use`s are **not** refused. A one-segment host `use` binds no name
  the conflict check sees, so `cove::resolve::import_conflict` is not
  raised.
- A qualified call `time.f(...)` is checked against the **host** first
  (`call_qualified` asks `host_uses` before `module_imports`), so
  `time.fromUnixMillis(...)` is "host module `time` has no operation
  `fromUnixMillis`". The library's functions are unreachable in exactly the
  files that read the clock, which are the files that want to format it.
- A qualified *type*, `time.DateTime`, goes the other way and finds the
  module.

`module_shadows_host` does not apply, because the module's name is
`std.time`, not `time`. So the name would work in every file that does not
also read the host's clock, and fail confusingly in the ones that do. A
standard library should not take a name an embedder may already use, and
`datetime` is the name the module's value has. `std.date` was the other
candidate; it reads as if it held no clock.

### 3. The calendar, and where the arithmetic is safe

The proleptic Gregorian calendar, run back past 1582 and through year 1
without a gap: year `0` is 1 BC and `-1` is 2 BC, as ISO 8601 numbers them.
A day is exactly 86,400,000 ms, because Unix time has no leap second, so
neither does this — `parseIso` refuses `:60` with that reason.

`fromUnixMillis` is Hinnant's `civil_from_days` over a day number from
`floorDiv`. It is **total**: every `Int` and every offset in range has an
answer, the least and the greatest `Int` included (years -292,275,055 and
292,278,994). The offset is added to the time of day, not to the instant,
and carried into the day count, so no sum can pass an end of `Int`.
`toUnixMillis` is its inverse for all of those: the day is multiplied one
nearer zero when it is negative, because the least `Int` is partway through
its day and the start of that day is not an `Int`. Valid fields naming an
instant past either end of `Int` trap, as `Duration`'s builders do.

### 4. Errors: `Err` for data, a stop for the call

- **`parseIso` answers `Err`**, quoting the text and naming the byte where
  the field that is wrong starts: `` `2026-02-30T00:00:00Z` is not an RFC 3339
  date-time: at byte 8, day 30 is past the end of a month of 28 days``. The
  text is data a program received, and ledger's client is owed a reason.
- **`toUnixMillis` answers `Err`** naming the first field out of range,
  because a struct with public fields may have been filled from input.
- **`fromUnixMillis` stops the run** for an offset outside -1439 to 1439,
  and **`daysInMonth`** for a month outside 1 to 12, worded through
  `core.refuse` as `Int.toHex` refuses a negative width. Both are the call's
  own argument, almost always a constant, and there is no answer to give.
- **The writers check nothing.** `toIso` on a `DateTime` built with month 13
  writes month 13. Checking there would make a formatter fallible for a value
  `toUnixMillis` already exists to check.

### 5. The text

`toIso` writes `YYYY-MM-DDTHH:MM:SS.sssZ` at offset 0 and `±HH:MM` in place
of the `Z` otherwise — always the milliseconds, always three digits, so the
stamps at one offset are one length and sort as text in instant order, which
is what the copies wrote. A year 0 to 9999 is four digits, all that RFC 3339
allows. Any other year is ISO 8601's expanded form as JavaScript's
`toISOString` writes it: a sign and at least six digits (`-000001`,
`+010000`). `parseIso` reads that back while it is six digits; a year of
seven or more digits is written and not read, because no RFC 3339 reader
reads it and no caller has one.

`parseIso` reads RFC 3339's `date-time` and what ledger's copy read:

- `YYYY-MM-DD`, `T`, `HH:MM:SS`, then optionally `.` and **1 to 9 digits**,
  **truncated** to the millisecond — the millisecond the instant falls in,
  as ledger's copy did — then `Z` or `±HH:MM`;
- `t` and `z` lowercase, and a space for the `T`, which RFC 3339 §5.6
  permits and ledger accepted;
- `-00:00` as `Z`; the six-digit signed year of §5, except `-000000`.

It refuses a time with no offset and a bare date, as ledger's copy did, for
its reason: neither names one instant. It refuses month `00` and `13`,
day `00`, a day past its month (February 29th of a common year included),
hour 24, minute or second 60, an offset of 24 hours or 60 minutes, `+0900`
without its colon, a tenth fractional digit, and anything after the offset.

It answers milliseconds rather than a `DateTime`, because the one caller
wants the instant, and `fromUnixMillis` on the answer gives the fields at any
offset. It does not keep the offset the text was written at.

### 6. `Int.floorDiv` and `floorMod`

Every copy carried `floorDiv`, and the module needs both, so they are `Int`
methods in `std.int` beside `abs`, as ADR 0106 added `toHex`: `floorDiv`
rounds toward negative infinity, and `floorMod` is the remainder that goes
with it, with the sign of the divisor, as Python's `//` and `%` and Java's
`Math.floorDiv` and `floorMod` are. They trap exactly where `/` and `%` trap:
on a zero divisor, and on the least `Int` over `-1` — for `floorMod`, too,
whose mathematical answer is `0`, because `%` traps there and the two should
not disagree about which inputs are wrong.

Each is written as `/` or `%` and a correction, answering the expression
itself rather than a `let`: `std.int` is lowered into every program, and a
first draft that returned a bound quotient put one copy into the answer of
each, in each of the corpus's 283 programs.

### 7. All of it is Cove

ADR 0073 gives a new primitive two homes, an instruction or Cove. This is
arithmetic on `Int`, bytes read with `byteAt`, and digits written through
ADR 0062's ensure, store and commit into one buffer per answer. No
instruction is added, and the native tier compiles every function of the
module.

A rough measure, to say which class this is in: a million rounds of
`fromUnixMillis(…, 540).toIso()` and `parseIso` of the answer take **6.6 s
on the VM and 2.0 s on the native tier** on the machine this was written on,
about 6.6 and 2.0 µs a round trip.

## Consequences

- The four minicloud copies of `isoTime` and `floorDiv`, and ledger's
  `parseIso`, `daysIn` and `daysFromCivil`, can be replaced:
  `text.isoTime(ms)` is `datetime.formatIso(ms)`, with the same bytes. Ledger's `parseIso(text)` becomes
  `datetime.parseIso(text)`, whose `Err` the schema check can now pass on to
  the client where it answered "not a time". Admin's JST helpers
  read `fromUnixMillis(ms, 540)` and its fields or halves, and `atTime`'s
  slice of a stamp becomes `toIsoTime()`.
- `parseIso` is stricter than ledger's copy in one place: a fraction of ten
  or more digits is an `Err`.
- `tests/e2e/values_std_datetime` pins every answer on the interpreter, the
  encoded VM and the native tier; `values_int_floor_div` pins the two `Int`
  methods; `fail_datetime_offset` pins the stop.
- `crates/cove-cli/tests/copies.rs` rises by 57, all of it the three new
  programs; `formatIso` adds none.
- **An `Instant`.** The host's `time.nowMillis()` and `nowMicros()` answer
  `Int`, which is why no app uses `Duration` and the apps hand-format
  intervals (admin's `queueDuration`). An `Instant` type — a point on the
  Unix timeline, with `Instant - Instant` a `Duration` and
  `fromUnixMillis`'s argument one of them — would cover both. It is not
  decided here: it changes what a host returns, which is an embedder's
  interface, and `DateTime` reads an `Int` today without closing that door.
- **Not decided here**: a time-zone database and the offset that changes
  with the date; a `DateTime` from `parseIso` that keeps the written offset;
  a date with no time; durations in ISO 8601 (`PT5M`). None has a caller.
