# cq — a typed streaming transformation over JSON Lines and CSV

`cq` reads a file one record at a time, parses and validates each record into a
type it declares, folds it, and writes rows out in either JSON Lines or CSV. It
sits in the space between `jq`, `awk`, and a small ETL job.

It is the first substantial practical program written in Cove, and it exists to
answer a question rather than to be shipped: is ordinary data programming in
Cove comfortable? [Findings](#findings) is the answer, and it is a mixed one.

## Running it

Everything below runs from the `examples/` directory. `--files-root cq/data`
is what confines the `files` capability to this example's own data.

```console
$ cd examples
$ cove run cq --files-root cq/data -- bookings.jsonl --program revenue-summary
property,bookings,nights,revenue,averageNightlyRate
harbour-loft,8,12,2208.00,184.00
orchard-barn,6,34,3272.50,96.25
seaside-cottage,8,28,3626.00,129.50
cq: read 24 records, wrote 3 rows to the console
```

Writing to a file instead of the console:

```console
$ cove run cq --files-root cq/data -- bookings.jsonl \
    --program revenue-summary \
    --output summary.csv
cq: read 24 records, wrote 3 rows to summary.csv
```

Reading CSV and writing JSON Lines:

```console
$ cove run cq --files-root cq/data -- rates.csv --program rate-card
{"nightlyRate":109,"notes":"Two bedrooms, sea view","property":"seaside-cottage","season":"low"}
{"nightlyRate":159,"notes":"Minimum stay 3 nights","property":"seaside-cottage","season":"high"}
{"nightlyRate":164,"notes":"","property":"harbour-loft","season":"low"}
{"nightlyRate":219,"notes":"Says \"quiet\" on the listing","property":"harbour-loft","season":"high"}
{"nightlyRate":86.5,"notes":"Dog friendly","property":"orchard-barn","season":"low"}
{"nightlyRate":124,"notes":"Closed for repairs, 12–14 March","property":"orchard-barn","season":"high"}
cq: read 7 records, wrote 6 rows to the console
```

`cove run cq --files-root cq/data -- --help` lists the programs and options.

`cove build cq` makes it a self-contained executable, which is where the
capability model shows through most plainly:

```console
$ cove build cq
built `cq` from 24 file(s) into `target/cq`
  entry:  cq.main
  grants: console, files, process
  limits: (none)
$ cd /tmp/somewhere && mkdir files && cp .../bookings.jsonl files/
$ /path/to/target/cq bookings.jsonl --program revenue-summary
property,bookings,nights,revenue,averageNightlyRate
...
```

The built binary carries the same three grants the `cove.toml` entry declared,
and its `files` root is `files/` beside the working directory. A path it cannot
reach is refused by the same rules `--files-root` enforces here.

### Programs

| `--program` | reads | writes | what it does |
| --- | --- | --- | --- |
| `revenue-summary` | JSON Lines | CSV | groups bookings by property and totals their nights and revenue |
| `confirmed-bookings` | JSON Lines | JSON Lines | keeps the confirmed bookings and reports what each is worth |
| `rate-card` | CSV | JSON Lines | reads a seasonal rate card and normalizes it |

A program is a case of an enum rather than a file the run loads. Cove has no
way to load a module at run time — a package is resolved, checked, and linked
before anything executes — so `--program` names one of the transformations this
package declares, and adding one is a change to `cq.programs.Program` and a
`match` arm the checker will not let anybody forget.

`--output-format jsonl|csv` overrides a program's default, in either direction:
every program's output is a row of `Cell`s, and a cell renders in either
format.

## Bad input

A record that cannot be read stops the run and says where:

```console
$ cove run cq --files-root cq/data -- bookings-malformed.jsonl --program revenue-summary
property,bookings,nights,revenue,averageNightlyRate
error: bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string
$ echo $?
1
```

Stopping is the default because a summary computed from most of the input is a
wrong answer that looks like a right one. `--skip-invalid` reports each bad
record and keeps going:

```console
$ cove run cq --files-root cq/data -- bookings-malformed.jsonl \
    --program revenue-summary --skip-invalid
property,bookings,nights,revenue,averageNightlyRate
bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string
bookings-malformed.jsonl:6:35: expected `,` or `}` after a field
bookings-malformed.jsonl:9:1: this record has no `id` field
harbour-loft,2,3,552.00,184.00
orchard-barn,1,5,481.25,96.25
seaside-cottage,3,10,1295.00,129.50
cq: read 7 records and skipped 3, wrote 3 rows to the console
```

`bookings-malformed.jsonl` holds one of each thing that can be wrong: a field
of the wrong type, JSON that does not parse, a missing required field, and a
blank line, which is skipped silently because a blank line is not a record.

`--limit <count>` bounds the records taken from the input, sound or not. That
is the meaning a limit wants: it bounds the work the run does, and counting
only the good records would make how much of the file was touched depend on how
much of it was wrong.

Both parsers refuse rather than guess, and the distinction they are built
around is between input a format does not cover and input a format spells
differently. `"a"x` is not an unsupported CSV field, it is a field the naive
reading turns into `ax` — a different value — so a quoted field must end at a
`,` or at the end of the record. `01`, `+1`, `.5`, and `1.` are not numbers
JSON writes, and `Float.parse` reads all four, so `cq.json` walks JSON's own
number grammar instead of delegating the judgement; `1e999` is grammatical and
still refused, because the `inf` it parses to is not a number any input meant.
A data tool that quietly turns one record in a hundred thousand into a
different record is worse than one that stops.

The diagnostic reads `file:line:column: message`, so an editor that already
knows how to jump to a compiler error can jump to a bad record. A syntax error
carries a real column; a validation error always says column 1, and that is a
limitation rather than a choice — a parsed `Json` value has forgotten where in
the line it was written, and carrying a span on every value is what a
diagnostic layer would do, which this program does not have.

Diagnostics go to the console even when the output does too, and interleave
with it, because `console` has no second stream to put them on.

A run that stops leaves the output it had already written. Nothing is buffered
to make a failure look clean, because buffering it would mean holding the whole
output, which is the thing this program exists not to do.

## Shape

Each directory is a module.

| module | what it is |
| --- | --- |
| `cq` | the CLI: `main.cove` runs one transformation, `options.cove` reads the command line, `sample.cove` generates measurable input |
| `cq.json` | a recursive `Json` value, a parser that reports a column, and a renderer |
| `cq.csv` | RFC 4180 field splitting and formatting, as a four-state machine |
| `cq.diag` | a `Detail` a parser can report, and the `Diagnostic` it becomes once the line is known |
| `cq.records` | the typed `Booking` and `Status`, and the validation that produces them |
| `cq.rows` | `Cell`, `Format`, and the rendering that makes one row go out as either |
| `cq.pipeline` | the streaming engine: read, fold, write, count |
| `cq.programs` | the three transformations |

The engine is one generic function. A transformation supplies a state type, a
step, and a finish, and gets back a loop that never holds more than one record:

```cove
export fn transform<S>(
  reader: files.Reader,
  sink: Sink,
  ...
  initial: S,
  step: fn(state: S, line: String, number: Int) -> Result<Step<S>, Detail>,
  finish: fn(state: S) -> Array<Array<Cell>>,
  ...
) -> Result<Outcome, Error>
```

`Step<S>` is what lets an aggregation and a filter be the same kind of thing.
An aggregation returns no rows until its input runs out and everything it knows
is in the state; a filter returns a row per record it keeps and carries almost
nothing. Each transformation's state is its own type — `Revenue` holds one
entry per property, `RateCard` holds whether the header has gone by — which is
why `main` dispatches with a `match` rather than a table of function values: a
table would have to agree about the state type, and these three do not.

## Measurements

Taken on a release build (`cargo build --release -p cove-cli`), macOS on Apple
silicon, over a generated 100,000-record file of 17 MB. Wall time and heap are
what `cove run --stats` reports.

Generate the input first — it is not checked in, because seventeen megabytes is
not something to keep in a repository. It comes from a seed, so two people
measure the same file:

```console
$ cove run cqSample --files-root cq/data --stats -- 100000 bookings-large.jsonl
cq: wrote 100000 records to bookings-large.jsonl
```

```console
$ cove run cq --files-root cq/data --stats -- bookings-large.jsonl \
    --program revenue-summary --output summary-large.csv
```

| run | records | wall | records/s | peak Cove heap | allocations | collections | GC pause | host calls | irreversible writes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| generate 100,000 records | 100,000 | 3.48 s | 28,700 | 106 B | 100,000 | 1,563 | 4.65 ms | 100,004 | 100,002 |
| `revenue-summary` → CSV | 100,000 | 111.5 s | 900 | **0 B** | 8 | 1 | 2.72 µs | 100,011 | 6 |
| `confirmed-bookings` → 66,825 JSON Lines | 100,000 | 120.5 s | 830 | 6,661 B | 66,825 | 1,045 | 14.9 ms | 166,832 | 66,827 |

Wall time is the median of three runs, which vary by well under a second once
the file is in the page cache; everything else is identical from run to run,
because the interpreter's work is.

**What "peak Cove heap" is, and is not.** It is `--stats`'s `peak_bytes`, which
is the mark-and-sweep collector's own heap — what a `Vector`, a `Map`, a
closure, or a task's state occupies. It is not the process's resident memory. A
`String` is a reference-counted allocation outside the collector, and so are
the reader's buffer and whatever the host holds, and none of the three appears
in this number. Read it as what the collector was asked to manage, which is the
only memory Cove's own numbers can speak for.

The first column of that table is the point and the second is the problem.

**Nothing the program builds outlives the line it was built from.** Two
separate things say so, and they are worth keeping separate. The measurement:
aggregating 100,000 records over a 17 MB input leaves the collector's heap at
zero bytes, with eight allocations and one collection, and filtering to 66,825
output records peaks at 6.7 KB rather than at the 7.9 MB it wrote. The
structure: the loop holds one line, one step's rows, and the transformation's
state, and each transformation's state is bounded by something other than the
length of the input — one entry per property for the aggregation, a count for
the filter. The first is evidence for the second rather than a proof of it,
since the numbers do not cover `String`. This is what `files.open` was added
for ([ADR 0018](../../docs/adr/0018-streaming-file-io.md)), and it holds.

**Throughput is about 900 records a second**, and that is slow. Where it goes,
measured by running the same loop with each stage added:

| stage | wall | added |
| --- | ---: | ---: |
| read 100,000 lines and take their length | 0.59 s | — |
| ... and call `chars()` on each | 2.09 s | 1.5 s |
| ... and parse each as JSON | 101.9 s | **99.8 s** |
| ... and validate each into a `Booking` | 111.2 s | 9.3 s |

Host I/O is not the cost: reading 17 MB a line at a time takes 0.59 s, which is
29 MB/s through 100,000 grant-checked, schema-checked, budget-charged Host API
calls. The cost is the interpreted per-character loop, and it is 99% of the run.

Checking JSON's number grammar rather than delegating it to `Float.parse` is
part of that 99% and costs about 2.5% of the run's fuel — which is what
refusing `01` and `1e999` is worth paying, and cheap next to what the scanner
was already spending.

## Findings

### Per-character work costs about 1.4 µs

The floor, measured directly — 1.95 million characters through
`chars.get(i).unwrapOr("")` and one comparison, and nothing else:

| how the character is reached | wall for 1.95 M | per character |
| --- | ---: | ---: |
| local `Array<String>`, index in a local | 2.63 s | 1.35 µs |
| the same through a struct's field | 3.64 s | 1.87 µs |
| the same through a struct's method | 5.26 s | 2.70 µs |

Two things follow. Reaching a character costs about 1.4 µs because every access
allocates an `Option` and every character is a heap `Rc<str>` — the plain
arithmetic loop `total += i % 7` runs at 32 M fuel/s and this runs at 7.5 M
fuel/s, so `fuel_spent` is not tracking what is expensive. And **calling a
method on a struct receiver doubles the cost of the loop it is in**, because
`self` is passed by value and a struct copy is an allocation. `cq.json`'s
scanner is written the natural way, with `peek()` and `take()` on a `Scanner`,
and pays for it.

Anything a program does per character in Cove is roughly a thousand times more
expensive than the same work inside a builtin. That is the single most
important thing this example learned.

### Building a string is quadratic, and the linear alternative is slower

`+` on two strings is refused, and its help says to interpolate, so appending
means `text = "{text}{character}"` — which copies everything read so far on
every character. `cq.json.parseText` does exactly that and is therefore
quadratic in the length of a field.

The linear alternative is to push each character onto a `Vector<String>` and
join once. Measured over the same 100,000 records, it was **worse**: 101.6 s
against 95.9 s, and 86 MB moved through 28,000 collections with 441 ms of GC
pause, against zero allocations. On fields of twenty characters the copying was
never the expensive part, and the `Vector` per field was.

So the example keeps the quadratic form deliberately, and the host's
one-mebibyte line bound is what stops its worst case from being unbounded. Cove
has no string builder, and this is the shape of the hole.

### What worked well

Recursive enums are comfortable. `enum Json { ... Items(Array<Json>),
Fields(Map<String, Json>) }` type-checks and runs with no ceremony, and `match`
exhaustiveness meant that adding a case broke every place that had to care.

Generic functions taking closures carried the whole engine. `transform<S>` with
a `step: fn(S, String, Int) -> Result<Step<S>, Detail>` gave three
transformations with three unrelated state types one streaming loop, and the
call sites read well because arguments are labelled.

`Map` is ordered, so grouped output is reproducible without a sort — which
matters, because there is no sort ([#95](https://github.com/myuon/cove/issues/95)).

`Result` and `Detail` made the diagnostics fall out. Every parse step answers a
column, `?` carries it up, and the one place that knows the line number adds
it. Nothing had to be threaded by hand.

A `var` parameter is a genuine inout alias, so the scanner threads through the
recursive parser without copying — it is the `self` receivers, not the
parameters, that cost.

Exhaustive `match` over a small enum is what made the CSV splitter right.
Review found that the first version, which tracked quoting with a `Bool`, read
`"a"x` as `ax`: after the closing quote it was back in the state an unquoted
field begins in, and nothing said that was wrong. Naming the four states —
start of a field, inside an unquoted one, inside a quoted one, and just past a
closing quote — turned an implicit fall-through into an arm the checker made
somebody write. The bug was not that a state was handled badly; it was that a
state had no name.

### Gaps this example is blocked on or lives with

Filed before it could be written, and closed by the pull requests this one is
stacked on:

- [#92](https://github.com/myuon/cove/issues/92) — `String` had no
  text-processing methods at all. Nothing here was expressible.
- [#93](https://github.com/myuon/cove/issues/93) — `Float` had no `parse` and
  there was no `Int`/`Float` conversion or fixed-precision formatting.
- [#94](https://github.com/myuon/cove/issues/94) — `files` read and wrote whole
  files, so "without retaining the full input" was not achievable.

Still open, and found by writing this:

- [#95](https://github.com/myuon/cove/issues/95) — no sort and no higher-order
  collection operations. Every `map` and `filter` here is a `for` and a
  `Vector.push`.
- [#99](https://github.com/myuon/cove/issues/99) — per-character work costs
  about 1.4 µs, and a struct method call doubles it. This is the measurement
  above, filed with its numbers; it is the largest thing this example found.
- [#100](https://github.com/myuon/cove/issues/100) — `Result` has no
  `unwrapOr`, although `Option` does, so every fallible value with a sensible
  default costs a four-line `match`.
- [#101](https://github.com/myuon/cove/issues/101) — nothing builds a character
  from a code point and `Int.parse` has no radix, so a `\u0041` escape cannot
  be implemented. `cq.json` refuses it rather than half-supporting it. `\b` and
  `\f` are refused for a nearer reason: Cove's own string literals cannot write
  those characters.
- [#102](https://github.com/myuon/cove/issues/102) — `console` has one stream,
  so diagnostics land in the output.

Two more are limitations of this program rather than of Cove, and are recorded
here because the language does not make either cheap to fix. A `Json` value
carries no span, so a validation error can name the field and not the place.
And nothing can be loaded at run time, so `--program` selects a transformation
rather than loading one.

## Tests

`cove test` runs the `test fn` declarations inside the package, which cover the
parser, the CSV codec, the record validation, the row rendering, and the
command line — everything that touches no host.

`crates/cove-cli/tests/examples.rs` runs `cq` itself against deterministic fake
hosts: an in-memory filesystem holding the same fixtures that are checked in
here, and a console that is a buffer. That is where the end-to-end output is
pinned.

Neither needs the 100,000-record file. The measurements above are a local
exercise, for the reason [ADR 0012](../../docs/adr/0012-performance-gate-and-native-backend.md)
gives for its own: wall-clock numbers on a shared runner are too noisy for a
threshold to mean anything.
