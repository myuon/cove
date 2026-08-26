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

Taken on a release build (`cargo build --release -p cove-cli`) over a generated
100,000-record file of 17 MB, on:

```text
cpu    Intel(R) Core(TM) i7-10700K CPU @ 3.80GHz
os     macOS 26.5.2 (x86_64)
rustc  1.93.1 (01f6ddf75 2026-02-11)
```

Wall time, fuel, and heap are what `cove run --stats` reports; resident memory
is `/usr/bin/time -l`'s.

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

| run | records | wall | records/s | peak Cove heap | allocations | collections | GC pause | host calls | irreversible writes | RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| generate 100,000 records | 100,000 | 3.48 s | 28,700 | 106 B | 100,000 | 1,563 | 4.65 ms | 100,004 | 100,002 | — |
| `revenue-summary` → CSV | 100,000 | 90.8 s | 1,100 | **0 B** | 8 | 1 | 2.75 µs | 100,011 | 6 | 10.5 MB |
| `confirmed-bookings` → 66,825 JSON Lines | 100,000 | 96.7 s | 1,030 | 6,661 B | 66,825 | 1,045 | 12.9 ms | 166,832 | 66,827 | 10.5 MB |

Wall time is the median of three runs, which vary by well under a second once
the file is in the page cache; everything else is identical from run to run,
because the interpreter's work is. `scripts/perf-cq.sh` is the script that
takes this table, so anybody can take it again.

Recording a trace costs 2.6%: the same 20,000-record run is 17.8 s untraced and
18.2 s with `--trace`.

**What "peak Cove heap" is, and is not.** It is `--stats`'s `peak_bytes`, which
is the mark-and-sweep collector's own heap — what a `Vector`, a `Map`, a
closure, or a task's state occupies. It is not the process's resident memory. A
`String` is a reference-counted allocation outside the collector, and so are
the reader's buffer and whatever the host holds, and none of the three appears
in this number. Read it as what the collector was asked to manage, which is the
only memory Cove's own numbers can speak for.

The `RSS` column is the whole-process figure, from `/usr/bin/time -l`, and it
is what the collector's zero does not say: the process holds about 10.5 MB —
the binary, the program's sources, the reader's buffer, the allocator's arenas
— steadily, whether the input is 17 MB or the 4 KB one. Both numbers are true
and they answer different questions.

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

(Those four are from before the sprint below; the shape did not change, only
the scale.)

Host I/O is not the cost: reading 17 MB a line at a time takes 0.59 s, which is
29 MB/s through 100,000 grant-checked, schema-checked, budget-charged Host API
calls. The cost is the interpreted per-character loop, and it is 99% of the run.

Checking JSON's number grammar rather than delegating it to `Float.parse` is
part of that 99% and costs about 2.5% of the run's fuel — which is what
refusing `01` and `1e999` is worth paying, and cheap next to what the scanner
was already spending.

## The performance sprint

[Issue #104](https://github.com/myuon/cove/issues/104) ran a bounded sprint
against these numbers before deciding anything about a new backend. What
follows is what it measured, what it changed, and what it concluded. Every
number here is reproducible with `scripts/perf-cq.sh` and
`./target/release/cove-bench --iterations 20`.

### Where the time went

Profiling the real 20,000-record run and attributing each sample to the
nearest interpreter frame said this, and it is not what wall time alone would
have suggested:

**About half of the run was allocation and deallocation.** `malloc` and `free`
were 44% of self time, and 50% with Rust's allocation shims and the drop glue
counted in. The other half was the tree walk itself.

**The allocation was spread, not concentrated.** No single site was more than
about 5%: `eval` 5.5%, `eval_block` 3.5%, `find_method` 3.4%, `invoke` 2.8%,
`plain_values` 2.5%, `Env::declare` 1.8%, `Value::some` 1.8%. That shape is the
finding — it says the interpreter allocates a little for almost everything it
does, rather than doing one expensive thing often.

### What was changed

Five changes, none of them visible from Cove source, and none of them changing
what any program means:

- `is_mutating_method` walked all eighteen builtin types' method lists,
  comparing strings, on every method call. It is memoized.
- `Value::type_name` returned an owned `String`, and `==` called it twice to
  check that two values were the same type. Type identity is now asked as a
  question rather than answered as a name, and receiver dispatch asks for the
  declared name only when there is one.
- `chars()` allocated a fresh one-character string per character. ASCII
  characters now come from a per-thread table.
- A struct value was a `Box`, so every non-mutating method call copied the
  box, the field vector, and every field. It is an `Rc` copied on write, and
  the one place a field is written takes a private copy first.
- `Option` and `Result` allocated two strings for their type and case names on
  every construction, and `find_method` allocated two more for its lookup key.
  Both are cached or reused.

### What that bought

| | before | after | |
| --- | ---: | ---: | ---: |
| `revenue-summary`, 100,000 records | 111.8 s | 90.8 s | **1.23×** |
| `confirmed-bookings`, 100,000 records | 120.0 s | 96.7 s | **1.24×** |
| trace overhead, 20,000 records | +10.2% | +2.6% | |
| resident memory | 10.6 MB | 10.5 MB | unchanged |
| managed heap, allocations, collections | | | unchanged |

And on the mechanism benchmarks — each one the same 2,000,000-iteration loop
with a single thing added, so a difference between two rows is what that thing
costs (`benches/`, run by `cove-bench`):

| benchmark | what it adds | before | after | |
| --- | --- | ---: | ---: | ---: |
| `arith` | nothing: the loop alone | 600 ms | 438 ms | 1.37× |
| `arrayget` | an indexed read and its `Option` | 1,914 ms | 1,425 ms | 1.34× |
| `field` | a struct field | 1,655 ms | 879 ms | **1.88×** |
| `method` | a call around that field | 4,684 ms | 3,099 ms | 1.51× |
| `call` | a call with no receiver | 1,895 ms | 1,749 ms | 1.08× |
| `chars` | the per-character scan | 2,789 ms | 1,842 ms | 1.51× |

### What that corrected

[Issue #99](https://github.com/myuon/cove/issues/99) measured that reaching a
character through a struct's *method* cost about twice what reaching it through
a local did, and attributed the difference to the receiver being passed by
value: `self` is a copy, and a copy was an allocation.

That attribution was wrong, and the fix is what showed it. Making a struct
copy-on-write removed the copy, and `field` duly got 1.88× faster — the largest
win of the five. But the *ratio* barely moved:

| | before | after |
| --- | ---: | ---: |
| local | 2.73 s | 1.86 s |
| through a struct's field | 3.71 s | 2.48 s |
| through a struct's method | 5.38 s | 3.63 s |
| method over field | +45% | +46% |

So the receiver copy was real and worth removing, and it was not what made a
method expensive. The call is. `call` says the same thing from the other side:
a call to a function with no receiver at all still costs about 1.3 seconds over
2,000,000 iterations, which is roughly 650 ns a call, and `pure` — naive
`fib(20)`, which is almost nothing but calls — agrees at about 790 ns.

### What is left, and why it is not more shaving

A call builds an environment, allocates a vector for its arguments, declares
each parameter into a scope, and tears all of it down. Every name inside that
body is then found by scanning the scopes in reverse and comparing strings:

```rust
fn lookup(&self, name: &str) -> Option<&Place> {
    self.scopes.iter().rev()
        .find_map(|scope| scope.bindings.iter().rev().find(|(n, _)| &**n == name))
        .map(|(_, place)| place)
}
```

That is the cost, and it is structural rather than local. The run evaluates
about 700 million AST nodes in 91 seconds, which is 130 ns a node. Removing
*every* remaining allocation would leave the tree walk, which is the other
half, so the ceiling for this kind of work is around 2× — and the sprint's
target was 10×.

The next real gain needs names resolved to slots before the program runs and a
value representation that does not allocate per operation. That is a redesign,
and [issue #105](https://github.com/myuon/cove/issues/105) scopes it with these
measurements as the expected benefit. The interpreter is not the bottleneck
because it is a tree walker; it is the bottleneck because it is a tree walker
that looks everything up by name.

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
