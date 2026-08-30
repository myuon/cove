# covecheck — a concurrent HTTP health and link checker

`covecheck` reads a manifest of endpoints, fetches them a bounded number at a
time, judges each answer against the conditions its check wrote down, and
reports every verdict in the manifest's order — whatever order the endpoints
answered in.

It is the first program in this repository that drives the whole concurrency
cluster together against one host: a task scope per window, a `spawn` per
check, an `await` per task, a `Shared` counter every task writes, and a
`clock.timeout` around each check and around the run. Each of those had a
corpus case of its own before this; none of them had a program.

The question it exists to answer is narrower than `cq`'s, and it is not "is
concurrency comfortable". It is: **can a concurrent Cove program have output
you can put in a golden file?** [Determinism](#determinism-is-the-whole-design)
is the answer, and it is yes, at a price the program pays in its shape.

## Running it

Everything below runs from the `examples/` directory. The manifest lives in
`files/`, which is where the `files` capability is rooted by default.

```console
$ cd examples
$ cove run covecheck -- checks.json
checks.json: 5 check(s)
  unchecked   health                  http: cannot connect to 127.0.0.1:8080: Connection refused (os error 61)
  unchecked   bookings                http: cannot connect to 127.0.0.1:8080: Connection refused (os error 61)
  unchecked   prices                  http: cannot connect to 127.0.0.1:8080: Connection refused (os error 61)
  unchecked   docs                    http: cannot connect to 127.0.0.1:8080: Connection refused (os error 61)
  unchecked   metrics                 http: cannot connect to 127.0.0.1:8080: Connection refused (os error 61)
0 passed, 5 failed, 0 skipped
error: covecheck: 5 check(s) failed and 0 were not tried
```

`files/checks.json` points at `127.0.0.1:8080`, which is where
[`server`](../server/main.cove) listens, so there is something to check by
running that first in another terminal:

```console
$ cove run server &
$ cove run covecheck -- checks.json --concurrency 2
checks.json: 5 check(s)
  ok          health                  15 character(s)
  failed      bookings                the status is 404, and the check expects a success
  failed      prices                  the status is 404, and the check expects a success
  failed      docs                    the status is 404, and the check expects a success
  failed      metrics                 the status is 404, and the check expects a success
1 passed, 4 failed, 0 skipped
```

`server` routes only `/health`, so four of the five are 404s. The two examples
were not written to compose, and this is what that looks like rather than
something to fix.

Read the two transcripts together, because the pair is the whole of what
[issue #145](https://github.com/myuon/cove/issues/145) was about. Nothing is
listening in the first and everything answers `404` in the second, and the run
now says which happened: `unchecked` for a service this run learned nothing
about, `failed` for one that answered and answered wrongly. Both used to read
`unchecked`, and the difference survived only in prose the host had
written.

`--format json` writes the same run for another program to read, on one line
(broken here, and only here, to fit the page):

```console
$ cove run covecheck -- checks.json --format json
{"checks":[
{"detail":"15 character(s)","name":"health","outcome":"ok","url":"http://127.0.0.1:8080/health"},
{"detail":"the status is 404, and the check expects a success","name":"bookings","outcome":"failed","url":"http://127.0.0.1:8080/bookings"},
...
],"failed":4,"manifest":"checks.json","passed":1,"skipped":0}
```

`cove run covecheck -- --help` lists the options.

| option | what it does |
| --- | --- |
| `--concurrency <n>` | how many checks may be in flight at once, default 4 |
| `--stop-after <n>` | start no more checks once this many have failed |
| `--timeout <n>` | stop the whole run after this many seconds, default 30 |
| `--format text\|json` | which report to write, default `text` |

`--timeout` is the whole-run deadline and not a per-check one; a check bounds
its own wait in the manifest instead. Both are refused at zero — a run
or a check given no time at all can only run out of it — and both are refused
negative, so the smallest run anybody can ask for is one second and the
smallest wait a check can ask for is one millisecond.

The exit status is the run's verdict: a run every check passed answers `Ok`,
and one with a failure or a check it never tried answers `Err` and says how
many of each.

## The manifest

```json
{
  "checks": [
    {
      "name": "health",
      "url": "http://127.0.0.1:8080/health",
      "expectStatus": 200,
      "expect": "\"status\":\"ok\"",
      "maxLength": 256,
      "timeoutMs": 500
    }
  ]
}
```

`name` and `url` are required; the four conditions and the wait are not, and a
check that writes none of them still says something — that the endpoint
answers, and answers successfully.

| field | what it asks |
| --- | --- |
| `expectStatus` | the status must be exactly this |
| `expect` | the body must contain this text |
| `reject` | the body must not contain this text |
| `maxLength` | the body must be no longer than this many characters |
| `timeoutMs` | the answer must arrive within this many milliseconds |

The four conditions are tried in the order the table lists them and the first
one that breaks is the whole answer, so two runs over one response report the
same reason. Reporting every broken condition would be more information and
less of an answer.

`expectStatus` is first in the table because it is first in the judgement, and
that order is an argument rather than a convention. A `404` sends an error
page, so a check that looked at the text before the status would report `the
body does not contain "harbour-loft"` — true of the error page, and silent
about the fact that the route is gone.

It is also the one condition every check has whether or not it writes one down.
A check that names no status expects a success, meaning any status from 200 to
299; a check that names one expects that number and nothing else, which is how
an endpoint that is *supposed* to refuse gets checked at all. There is no way
to write "any status", because a checker that accepted any status would not be
checking the thing an HTTP endpoint most often gets wrong. The default is also
what keeps a manifest written before this field existed meaning what it meant:
such a check was only ever reached by a `2xx`, since a `fetch` that answered
anything else used to fail.

The range is the whole of what its reader adds on top of being a count.
`expectStatus: 20` and `expectStatus: 4004` are refused, because they are
typing mistakes for `200` and `404` and a checker that took them would wait for
an answer that cannot arrive and then report the endpoint as broken.
`expectStatus: 418` is accepted, and that is the rule rather than an oversight:
the bound is on the notation and not on a list of statuses this program knows,
because what an endpoint answers is the endpoint's business.

`timeoutMs` is the odd one out of the five and is listed last for that reason.
The other four are conditions on an answer that arrived; this one bounds how
long the run waits for one to arrive at all, and a check that overruns it is
`unchecked` rather than `failed` — the same news as an endpoint that refused
the connection, because both are runs that learned nothing about the service.
A check that leaves it out is bounded by the program's own 5 seconds, which is
why the field is optional rather than a number that means "no bound": there is
no way to write a check that waits forever, and there should not be.

A manifest that will not load stops the run before a request is made, and the
message names the file, the check, and the field:

```console
$ cove run covecheck -- broken.json
error: broken.json: check 2: `expect` must be a string, and is a boolean
```

There is no line number in it. The manifest is one JSON document rather than a
record per line, so what is wrong is a path through that document and not a
place in the file — which is a smaller diagnostic than [`cq`](../cq/README.md)
gives, for a smaller reason to give one.

The parser is `cq.json`, imported rather than written again. A second JSON
parser in this package would test nothing the first one does not, and
`cq.json` already refuses rather than guesses about the four number forms JSON
does not write. This is the second cross-example import in `examples/`, after
`restricted/` importing `text/`.

## What a check could not say, and what closed it

Two things a health checker obviously wants were missing when this program was
written, and neither was missing because the program declined to do it. Both
are closed now, and both are still described here — as what the gap was and
what closed it — because in each case the thing that closed it was a change to
the language or to its hosts that this example is the argument for.

**An expected status, which was the first gap and is now closed.** `http.fetch`
used to be declared `fetch(String) -> Result<String, Error>`, so the whole of
what a client learned was a body or a message. The real host *had* the status
and threw it away: anything outside 200-299 became
`Err("http: {url} answered {status}")`, which meant a program could not tell
"the connection was refused" from "the server answered 404" except by matching
on prose the host had written, and prose is not an interface. This program had
one `unchecked` outcome covering both, and no check could name a status.
**[Issue #145](https://github.com/myuon/cove/issues/145)** was that gap, and it
was filed from here.

What closed it is that `fetch` answers `Result<http.Response, Error>` — the
same `http.Response` a route handler returns and `http.json` builds, so one
type is what a client receives and what a server sends. **A status the server
sent is an `Ok`, whatever the number is**, and an `Err` now means only that no
response arrived: a URL this host will not send, a connection it could not
make, a read that ran out of time, or a response larger than the host will
hold. That is the distinction this program needed, and it is a distinction in
the *shape* of the answer rather than in its wording.

The shape was a decision, and the alternative was a resource handle. `http`
already issues them — `http.Server` has `port` and `close` — so "a response
is a handle you ask questions of" was available, and it is what a streaming
body would need. It was rejected on
[ADR 0013](../../docs/adr/0013-host-resource-handles.md)'s own line: a handle
is for something the host owns and can run out of, and something a program
opens, holds, and closes. A response that has already arrived in full is none
of those, and making it one would have added a `close` nothing should call and
a lifetime nothing has — which is the argument
[ADR 0020](../../docs/adr/0020-a-diagnostic-stream-for-console.md) made against
a `console.Stream` for the same reason. A body read once is a value.

What it bought this program is small and is the point. `judge` takes an
`http.Response` instead of a `String` and asks the status first, and
`runner.cove`'s `match` on the fetch stopped having to guess from a message
which of two very different things had happened. `Unchecked` still exists and
still means what its name says; what changed is that it no longer means two
things at once.

**A per-check timeout, which was the second gap and is now closed.** Every
check has always been bounded — `runCheck` wraps its fetch in
`clock.timeout(checkBound(check))` and `main` wraps the whole run in
`clock.timeout(options.deadline)` — but when this program was written both of
those bounds were constants written in `runner.cove`, and neither the manifest
nor the command line could carry one. `Duration` had no associated function,
and the reference gives it `+` and `-` and not `*`, so there was no expression
that turned a number arriving from outside the program into the `Duration` a
host call takes. Writing the timeout as a sum of `1ms`, `2ms`, `4ms`, ... would
have worked and would have been a workaround rather than a program.
**[Issue #146](https://github.com/myuon/cove/issues/146)** was that gap, and it
was filed from here.

It named two shapes and the language took the first: **an associated function
per literal suffix**, `Duration.nanos`, `micros`, `millis`, `seconds`,
`minutes` and `hours`, with the same six names reading a `Duration` back as an
`Int`. The other shape was scalar multiplication, `Duration * Int`, which is a
smaller change and would have been enough to *set* a bound. It is not enough
for this program, because there is no expression made of `*` that answers how
many milliseconds a `Duration` is: `options_test.cove` could then assert the
deadline it had just read only by running out of it, and a message that named
a check's own timeout would have had to carry the number alongside the
`Duration` and trust the two to stay in step. A bound that can be configured
is a bound that has to be reportable, and the direction that reads is the one
that decided the shape.

What closing it bought is two bounds that come from outside the source.
`timeoutMs` in a check becomes `Duration.millis` of itself, and `--timeout n`
becomes `Duration.seconds` of what was typed. Neither replaced a literal with
a computation: `defaultCheckBound()` is still `5s` and `runBound()` is still
`30s`, and they are still functions with names. That is the point rather than
an oversight — `clock.timeout` cannot tell a literal from a built `Duration`,
so the bound a person configured and the bound the program decided on their
behalf are one value of one type, and `checkBound` is one `match` over an
`Option` because it has nothing else to reconcile.

The numbers that are not bounds are refused where they are read rather than
where they are used. A `timeoutMs` that is negative or a fraction of a
millisecond is refused by the same reader that refuses those for `maxLength`,
because being a count is the same question for both; a `timeoutMs` of `0` gets
past that reader and is refused by the field's own rule, since a check allowed
no time at all would come back `unchecked` however healthy its endpoint, which
is what `--concurrency 0` and `--timeout 0` are refused for as well. A count
so large that its nanoseconds do not fit an `Int` is *not* refused: it stops
the run inside `Duration.millis`, which is where every duration that overruns
an `Int` stops, and a copy of the language's limit written into `manifest.cove`
would be a second one to keep true.

`maxLength` is a third thing that is smaller than it sounds. The bound is the
program's, not the host's: `fetch` answers a whole body and there is no way to
ask it for less, so what `maxLength` bounds is what the checker will *accept*
and not what the run will *hold*. A response that is too large has already been
held by the time this program measures it. It is a count of characters rather
than bytes for the same kind of reason — `String.length()` counts characters
and nothing offers a byte count.

The host has a bound of its own now, and it is not this one. A response over a
mebibyte is refused by `http` before a Cove value is built, which is the same
number and the same argument as the bounds its listener already held itself to
and as `files.Reader.readLine`'s: a host reads what it decided to read rather
than what the input asked it to
([ADR 0018](../../docs/adr/0018-streaming-file-io.md)). It is deliberately a
constant rather than a field of a check, for the reason that ADR gives — a
caller cannot say how large an answer it has not seen yet should be, so a
per-request bound would make every manifest answer a question about a response
that has not arrived. `maxLength` is the question a manifest *can* answer,
which is how large an answer it is willing to call healthy.

## Determinism is the whole design

A concurrent program whose output goes in a golden file is a test that fails
at random unless the program was arranged not to be one. Two corpus cases in
this repository already flip run to run — `tests/e2e:fail_max_tasks` on the
interpreter and `examples:callbacks` on the VM, each against *itself* — and
`crates/cove-cli/tests/differential.rs` has a whole rule about comparing a
cancelled task by nothing but its spawn. `covecheck` is a `[run.<name>]` under
`examples/`, so its console output is compared line for line across two
backends on every run of the corpus. Four decisions are what make that safe.

**No task writes anything.** Every task's whole product is a `Verdict` value it
answers. The report is built once, at the end, by the task that read the
manifest. A run's tasks share one console and the order two threads reach it
in is the scheduler's, so a program that printed as it went would be
comparing an interleaving.

**Each window is awaited in the order it was spawned.** The verdicts come back
in the manifest's order because the array is built by awaiting task 1, then
task 2, then task 3 — not because anything finished in that order. Nothing is
lost by waiting in order: a window takes as long as its slowest member either
way.

**The stop decision is taken between windows and nowhere else.** `--stop-after`
is read after every task of a window has settled, so which checks a run gives
up on is decided by the manifest and the concurrency and not by which thread
was quickest. A budget consulted *inside* a task would make each verdict
depend on which sibling failed first, and a verdict would stop being a fact
about the endpoint.

**The one piece of shared state is a counter, and `lock` makes each update one
operation.** `Shared<Tally>` is written by every check task — `started` when it
begins, `passed` or `failed` when it settles. Which order those increments
land in belongs to the scheduler; the totals do not, because a
read-modify-write inside `lock` cannot be split into two that race. The counts
could have been folded out of the finished array instead, and `started` is the
one of the three that could not: it is what says how many checks were never
tried, which the verdicts do not carry.

What is left over is the thing that could not be made deterministic, and it is
named rather than hidden. See [Stopping](#stopping-is-a-bound).

## Bounded concurrency is a window, not a queue

`--concurrency n` runs the checks in windows of `n`: spawn `n`, await all `n`,
then the next `n`. A window is not what a checker would ideally do — a work
queue that starts a new check the moment any one finishes keeps `n` in flight
continuously, where a window runs at the pace of its slowest member and then
starts over.

The window is what the language has. Bounded concurrency the other way needs a
way to wait for *whichever* task settles first, and
[ADR 0008](../../docs/adr/0008-concurrent-task-execution.md) decided against
one: "no async I/O, no work-stealing scheduler, no task priorities, no
`select`." That is a decision rather than a gap, so there is no issue filed
for it; this is what a program built on the decision looks like.

The upper bound is real either way — no more than `n` tasks exist at once —
and `covecheck` at `--concurrency 1` and at `--concurrency 40` report the same
run, which both `cove test` and `crates/cove-cli/tests/examples.rs` assert.

## Stopping is a bound

There are two ways this program stops early, and only one of them has a golden
file.

`--stop-after n` stops *starting* checks once `n` have failed. The checks it
never started are reported as `skipped` rather than left out, because a report
that omitted them would be a report of a different manifest. It is
deterministic for the reason above: the decision is taken at a point where
every task in flight has already settled.

The whole-run deadline is the other, and it is the one that cancels. `main`
wraps the run in `clock.timeout(options.deadline)` — `--timeout n` seconds, or
the `30s` `runBound()` answers when nobody said; a run that overruns leaves the
window's scope early, and leaving a scope early cancels its children. What
this program then reports is that the bound was exceeded — and nothing about
how far each cancelled check got, because
[ADR 0024](../../docs/adr/0024-a-stop-is-a-bound-not-a-point.md) makes a stop
a bound and not a point. A cancelled loop stops within a fixed amount of
further work after the cancellation becomes true, not at the next instruction,
and whether the stop reached a check before its fetch, during it, or after it
had already finished is the scheduler's answer.

So the cancellation is demonstrated by a `test fn` that asserts the bound and
says what it gives up:

```cove
test fn aCancelledLoopStopsWithinItsOwnBound() -> Result<Unit, Error> {
  let whole = Shared(Turns(taken: 0))
  counting(whole, false)
  assertEqual(whole.lock(fn(it) { it.taken }), rounds())?

  let stopped = Shared(Turns(taken: 0))
  counting(stopped, true)
  assert(stopped.lock(fn(it) { it.taken }) <= rounds())?
  Ok(())
}
```

The second assertion is `<=` and not `<`, and that is the test rather than a
weakness in it. A cancelled loop may have taken none of its turns, all of
them, or anything between; asserting a count would be asserting the
scheduler's answer.

## Shape

Every file below is one module, `covecheck`, because a directory is a module
and these are the files in it.

| file | what it is |
| --- | --- |
| `main.cove` | the entry: read, run under the deadline, report, decide the exit |
| `options.cove` | the command line, which touches no host |
| `manifest.cove` | `Check`, `Manifest`, and the validation from parsed JSON |
| `verdict.cove` | `Outcome`, `Verdict`, and `judge`, which is the whole judgement and is pure |
| `runner.cove` | the concurrent engine: windows, tasks, the shared tally, the bounds |
| `report.cove` | the two reports, both built as text rather than printed |

Splitting the judgement out of the runner is what lets `judge` be tested
without a host and the runner be tested without an endpoint. The report is
built and returned rather than printed for the same reason twice over: it is
what makes the thing a test asserts on the thing a person reads, and it is
what keeps the console out of the tasks.

## Where its behaviour is pinned

Three places, and they check different things.

`cove test` runs the 39 `test fn` declarations in this directory, on whichever
backend it was asked for. Most are about the parts that touch nothing — the
manifest, the judgement, the command line, the two reports — and five are
about the runner, over the fake `http` that has no recorded answer for any
URL. That fake is what makes those five tests about *the runner*: every
verdict carries the URL the host was asked for, so a window that paired its
results with the wrong checks would say so. A sixth is about the runner and
reaches nothing at all, because which `Duration` a check is bounded by is
decided before a request is made: it asserts the bound each check is given and
not what happens when one expires, which is the scheduler's and the clock's
answer rather than this program's.

`crates/cove-cli/tests/examples.rs` runs the whole program against a fake
`http` seeded with responses, which is the only way to reach a passing check, a
text mismatch, a body over its bound, and a named non-2xx status in one run.
Its manifest carries a check named `admin` that expects `403` and gets it, next
to one for an endpoint nothing answers for, so the two verdicts issue #145 was
about sit side by side in one golden report; a test of its own runs that
manifest twice over `/admin`, once answered `200` and once not answered at all,
and pins that the first is `failed` and the second is `unchecked`.

That manifest also carries a `timeoutMs`, and one of its runs passes `--timeout`, so the path both numbers
take from outside the program to a `clock.timeout` is walked there, and the
assertion is that a run inside its bounds reports what a run with no bounds
written down reports. It is also where the two things the fakes *cannot*
produce are written down: a timeout firing, which needs something to move the
virtual clock and a fetch does not, and a cancellation, which is a race by ADR
0024's own account.

`crates/cove-cli/tests/differential.rs` runs it on both backends and compares
the console, the answer, the filesystem and the trace. Its `http` fake has no
recorded answers at all, so every check comes back `unchecked` with the URL it
asked for — which is a perfectly good program to compare, and is the one that
would catch a run whose report order depended on its scheduler.

## What is not here

The issue this was built for asks for p50/p95 check overhead and trace volume
measured apart from network latency. There are no numbers in this README, and
that is a gap rather than a decision: measuring them wants a machine that is
not running anything else, and this branch did not have one. `cq`'s README is
the shape such a section should take.
