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
  unchecked   bookings                http: http://127.0.0.1:8080/bookings answered 404
  unchecked   prices                  http: http://127.0.0.1:8080/prices answered 404
  unchecked   docs                    http: http://127.0.0.1:8080/docs answered 404
  unchecked   metrics                 http: http://127.0.0.1:8080/metrics answered 404
1 passed, 4 failed, 0 skipped
```

`server` routes only `/health`, so four of the five are 404s. The two examples
were not written to compose, and this is what that looks like rather than
something to fix.

`--format json` writes the same run for another program to read, on one line
(broken here, and only here, to fit the page):

```console
$ cove run covecheck -- checks.json --format json
{"checks":[
{"detail":"15 character(s)","name":"health","outcome":"ok","url":"http://127.0.0.1:8080/health"},
{"detail":"http: http://127.0.0.1:8080/bookings answered 404","name":"bookings","outcome":"unchecked","url":"http://127.0.0.1:8080/bookings"},
...
],"failed":4,"manifest":"checks.json","passed":1,"skipped":0}
```

`cove run covecheck -- --help` lists the options.

| option | what it does |
| --- | --- |
| `--concurrency <n>` | how many checks may be in flight at once, default 4 |
| `--stop-after <n>` | start no more checks once this many have failed |
| `--format text\|json` | which report to write, default `text` |

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
      "expect": "\"status\":\"ok\"",
      "maxLength": 256
    }
  ]
}
```

`name` and `url` are required; the three conditions are not, and a check that
writes none of them still says something — that the endpoint answers at all.

| field | what it asks |
| --- | --- |
| `expect` | the body must contain this text |
| `reject` | the body must not contain this text |
| `maxLength` | the body must be no longer than this many characters |

The conditions are tried in that order and the first one that breaks is the
whole answer, so two runs over one body report the same reason. Reporting
every broken condition would be more information and less of an answer.

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

## What a check cannot say, and why

Two things a health checker obviously wants are missing, and neither is
missing because the program declined to do them.

**An expected status.** `http.fetch` is declared
`fetch(String) -> Result<String, Error>`: the whole of what a client learns is
the body, or a message. The real host *has* the status and throws it away — a
status outside 200-299 becomes `Err("http: {url} answered {status}")` — so a
program cannot tell "the connection was refused" from "the server answered
404" except by reading prose the host wrote. That is why this program has one
`unchecked` outcome covering both, and why no check names a status.
**[Issue #145](https://github.com/myuon/cove/issues/145)** is that gap.

**A per-check timeout.** Every check *is* bounded — `runCheck` wraps its fetch
in `clock.timeout(checkBound())` and `main` wraps the whole run in
`clock.timeout(runBound())` — but the bounds are constants the program writes
down, and the manifest cannot carry one. `Duration` has no constructor and no
multiplication, so there is no expression that turns a number a manifest holds
into the `Duration` a host call takes. Writing the timeout as a sum of
`1ms`, `2ms`, `4ms`, ... would have worked and would have been a workaround
rather than a program. **[Issue #146](https://github.com/myuon/cove/issues/146)**
is that gap.

`maxLength` is a third thing that is smaller than it sounds. The bound is the
program's, not the host's: `fetch` answers a whole `String` and there is no
way to ask it for less, so what `maxLength` bounds is what the checker will
*accept* and not what the run will *hold*. A response that is too large has
already been held by the time this program measures it. It is a count of
characters rather than bytes for the same kind of reason — `String.length()`
counts characters and nothing offers a byte count.

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
wraps the run in `clock.timeout(runBound())`; a run that overruns leaves the
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

`cove test` runs the 31 `test fn` declarations in this directory, on whichever
backend it was asked for. Most are about the parts that touch nothing — the
manifest, the judgement, the command line, the two reports — and five are
about the runner, over the fake `http` that has no recorded answer for any
URL. That fake is what makes those five tests about *the runner*: every
verdict carries the URL the host was asked for, so a window that paired its
results with the wrong checks would say so.

`crates/cove-cli/tests/examples.rs` runs the whole program against a fake
`http` seeded with bodies, which is the only way to reach a passing check, a
text mismatch, and a body over its bound in one run. It is also where the two
things the fakes *cannot* produce are written down: a timeout, which needs
something to move the virtual clock and a fetch does not, and a cancellation,
which is a race by ADR 0024's own account.

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
