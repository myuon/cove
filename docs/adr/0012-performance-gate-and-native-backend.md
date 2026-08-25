# ADR 0012: The performance gate and the path to a native backend

- Status: Accepted
- Date: 2026-08-25
- Amends: [ADR 0001](0001-mvp-language-design.md), whose success criterion
  about competitive execution performance becomes a measured gate rather than a
  hypothesis, and with it
  [ADR 0002](0002-implementation-language-and-backend.md) and
  [ADR 0009](0009-cove-build.md) — both of which stand exactly as written; what
  changes is that the criterion they deferred is now measured, and the deferral
  has gates
- Implemented by: PR #34
- Implementation status: complete — `crates/cove-bench`, the `benches/`
  package, and the CI step that runs it all exist, and gates 2 and 3 are met.
  That gate 1 is not askable until a reference native program exists, gate 4
  not until a heap-allocating benchmark does, and gate 5 not until a compile
  stage does, is this ADR's finding rather than a gap in it; so is the absence
  of a CI threshold, argued for under "Why this is hermetic, not a fixed
  baseline".

## Context

ADR 0002 chose a tree-walking interpreter as the only MVP backend on the
grounds that Host API dispatch, grant enforcement, budgets, cancellation, and
tracing must be hand-instrumented under every backend, so a compiled backend
would buy execution speed and nothing else — and execution speed is not what
the MVP tests. ADR 0009 then made `cove build` produce a self-contained
native executable that embeds the interpreter, stating plainly that this is
"not a code generator" and that ADR 0001's success criterion about
competitive execution performance "still cannot be evaluated." ADR 0001 names
that criterion precisely: "approximately Go-class compilation speed and
execution performance." A tree-walking interpreter was never going to be
judged against that bar at the MVP stage — ADR 0002 says so directly — but
three ADRs in, there is still no number to say by how much it misses, or
whether missing it is even a problem any real Cove program has.

Both decisions are correct and both leave the same question open: how would
anyone know when that criterion stops being evaluable in principle and starts
being false in practice? Nothing today measures the interpreter against
anything. There is no recorded number for how fast it runs a representative
program, how much a program's startup costs, how much tracing costs, or how
much memory a run holds live. "Native-first" and "competitive performance" are
hypotheses from ADR 0001 that have gone three ADRs without an experiment.

This ADR is not a decision to build a compiler. It is a decision about what
would have to be true before building one is worth it, and a harness that can
tell whether that is true — today, and again whenever this question is
revisited.

## Decision

Add a benchmark harness, `crates/cove-bench`, and a small package of
representative programs, `benches/`, and record what the interpreter does
today as the baseline every later claim about performance is measured
against. The interpreter remains the only backend and the only executable
answer to what a Cove program means — an answer that stays accountable to the
Language Card, as "The specification, the oracle, and the backends" below sets
out; nothing here proposes writing a second one.

### The benchmark suite

`benches/` is a `cove.toml` package sitting next to `examples/` and
`tests/e2e/`, not inside either: it is not a representative *program* the way
`examples/` is, and not a regression fixture the way `tests/e2e/` is — it
exists only to be timed. It has three entries.

`pure` is naive recursive Fibonacci, `fib(20)`, about 22,000 calls. It grants
no capability at all. This is deliberate: it isolates the interpreter's own
per-call cost — environment-chain lookup, argument binding, arithmetic,
branching — from anything Host dispatch adds, so a regression here can only
be the tree walker itself. Its own correctness check (`assertEqual(fib(20),
6765)`) runs as part of every measured call, so a broken interpreter fails the
benchmark before it reports a number for it.

`hostheavy` calls `clock.now()` and `console.println` 2,000 times each. It is
driven by the same deterministic fakes `cove test` already grants by default
(`crates/cove-cli/src/test.rs`): a `console` that writes into a sink nobody
reads, and a `clock` whose `VirtualTime` never advances on its own. Every call
is therefore reproducible from run to run and machine to machine in what it
*does*, even though how long it takes still is not — this is what makes it
possible to say a Host-heavy benchmark measures dispatch, grant checking, and
budget accounting rather than real I/O latency, without needing a network or
a filesystem to get there.

`startup` is `export fn main() {}` — the smallest entry the type checker
accepts. `cove-bench` runs it by spawning the real `cove` binary built
alongside itself, because process creation and binary loading are exactly
what an in-process measurement cannot see.

### Metrics

`cove-bench` reports, as one JSON object per line on stdout:

- **Wall time**, as `{min, mean, max}` nanoseconds over `--iterations` runs.
- **Throughput**, as `fuel_spent` and `fuel_per_sec`. Fuel is not new: it is
  `Budget`'s existing per-safepoint counter (ADR 0003, ADR 0008), charged at
  every loop back edge, call, and `await` regardless of whether a limit is
  set. It is a better throughput number than wall time alone because it is
  the interpreter's own accounting, insulated from whatever else the machine
  running it is doing — two runs with the same `fuel_spent` did the same
  amount of interpreted work, whatever their wall time says.
- **Memory**, as `heap_peak_bytes`, which is `HeapStats::peak_bytes` — the
  largest live set ADR 0011's collector measured during the run. `pure` and
  `hostheavy` both report zero: neither allocates a collectable object, which
  is itself a fact worth protecting, not an oversight — a future change that
  makes simple arithmetic or a `String` interpolation start allocating on the
  GC heap would show up here first.
- **Trace overhead**, as the ratio of mean wall time with a real `JsonlSink`
  (writing to `io::sink()`, so the destination costs nothing) against the same
  run with `NullSink`. This isolates what recording an event costs from
  whatever a trace's destination costs.
- **Host-call counters**, `host_calls` and `irreversible_writes`, read
  directly from `HostRegistry`, the same counters `cove run --stats` prints.
- **Process startup**, as the wall time of spawning `cove run startup` from
  outside, `{min, mean, max}` over `--iterations` samples.

**Compile time has no baseline in this ADR.** No stage exists yet that turns
Cove source into anything other than the AST the interpreter walks, so there
is nothing to time. `cove build`'s own cost is Rust's linking, already
described and accepted in ADR 0009, and it is not this harness's concern. If
the incremental path below adds a typed-IR or AOT stage, that stage gets a
`compile_ns` field in this same JSON output before it gets a threshold; this
ADR leaves the shape of that number to whoever adds the stage; it does not
guess it.

### What the harness measured, and the thresholds that follow from it

Recorded on the machine this ADR was written on, which is the only claim any
of these absolute numbers make — see "Why this is hermetic, not a fixed
baseline" below for why no golden number is checked in.

```text
pure (fib(20), zero Host calls)
  debug:    95.0 ms mean,  2.30M fuel/sec, ~230K calls/sec, 1.01x traced
  release:  17.5 ms mean, 12.54M fuel/sec, ~1.25M calls/sec, 1.00x traced
  heap_peak_bytes: 0 (min, mean, and max) in both profiles

hostheavy (4,001 Host calls through console and clock)
  debug:    18.1 ms mean,  1.11M fuel/sec, 2.23x traced
  release:   3.4 ms mean,  5.81M fuel/sec, 2.80x traced
  heap_peak_bytes: 0 (min, mean, and max) in both profiles

startup (process spawn of `cove run startup`, warm)
  debug:     5.3 ms mean
  release:   3.9 ms mean
```

("calls/sec" is `fib`'s own call count divided by wall time; fuel is charged
per safepoint, not per call, so the two numbers measure related but different
things and neither substitutes for the other.)

The heap figures are quoted from the same runs as the times beside them, not
recalled separately: `heap_peak_bytes` is printed on the same JSON line as the
wall time and the fuel rate, so every run that produced a figure above
produced a zero next to it, and a `cove-bench --iterations 3` on this machine
still prints `{"min":0,"mean":0,"max":0}` for both interpreter benchmarks.
They are recorded here because gate 4 below turns on them, and a reader should
be able to see that baseline rather than be told about it.

These numbers set five gates. Crossing one is what would make building a
compiled backend a decision worth making, rather than a hypothesis worth
stating a fourth time:

1. **Throughput.** The interpreter remains the only backend as long as a
   representative Cove program's profiled CPU time is not dominated by
   interpretation overhead — as opposed to Host waits or the program's own
   algorithm — relative to a reference native implementation of the same
   program, within roughly a 10x band. Nothing has crossed this today,
   because nothing has been compared: there is no reference native
   implementation of a representative Cove program to compare against. That
   comparison, on a real workload, is the actual trigger this ADR names —
   not a number this ADR can precompute in its absence.
2. **Startup.** A native backend must not regress warm process startup past
   roughly 50 ms for a trivial entry. Today's baseline — 3.9–5.3 ms — is not
   a reason to compile anything: the embedded-interpreter binary ADR 0009
   chose already pays almost nothing beyond `exec` itself. This gate exists
   to protect that property against a future backend that adds a slow
   warm-up phase (bytecode verification, JIT warm-up), not to motivate one.
3. **Trace overhead.** Recording a trace event for every Host call must stay
   within roughly 5x the untraced wall time on a Host-call-heavy workload.
   Today's 2.2–2.8x is comfortably inside that. ADR 0001 requires Host calls,
   grants, budgets, and tracing to be observable "without language-specific
   application hooks" under every backend; a backend that makes full tracing
   prohibitively expensive fails that requirement in practice even if it
   technically emits every event.
4. **Memory.** Peak live heap bytes for the same workload must not exceed
   roughly 2x the interpreter's own recorded baseline for it. `pure` and
   `hostheavy` both baseline at zero, and twice zero is zero: a relative
   threshold measured against a zero baseline is not a threshold, and no
   backend can pass or fail it. This gate is therefore not evaluable today
   rather than met. Making it evaluable means adding a benchmark that
   allocates a collectable object, which neither existing one does — but that
   is not a reason to add one. The memory *budget* these numbers were once
   compared against no longer exists: ADR 0011's "Amendment (2026-08-25): the
   memory budget is removed" retracted `Limits::max_memory` and left the
   collector's measurements as observability rather than an enforced bound, so
   there is no live claim here that a heap-allocating benchmark would rescue.
   What such a benchmark would buy is a performance observation about the
   collector — which is a perfectly good reason to add one when someone
   actually wants to measure the collector, and no reason at all to add one
   merely so that this gate has something to report.
5. **Compile time.** Undefined until a compile stage exists, per above. The
   placeholder bar, to be revisited when it does: compiling a representative
   package should stay within the same order of magnitude as `cove build`'s
   existing link-dominated cost, so that a compile step does not turn `cove
   run`'s edit-run loop into a build tool's.

Of the five, two are met, none is failed, and three are not evaluable. Gates
2 and 3 — startup and trace overhead — are met today, comfortably, on the
evidence recorded above. Nothing is over a threshold this ADR states, so no
gate is failed. Gates 1, 4, and 5 cannot be evaluated at all, each for its own
reason and none of them a fact about the interpreter: gate 1 has no reference
native program to compare a representative workload against, gate 4 has no
heap-allocating benchmark to give it a baseline that a multiple means anything
against, and gate 5 has no compile stage to time. A gate that cannot be asked
is not a gate that has been passed, and this ADR counts it as neither.

That the two gates which can be answered are answered comfortably is
consistent with ADR 0002's original reasoning: nothing about Host dispatch,
budgets, or tracing was ever expected to be the bottleneck, because none of it
changes shape under a different backend. If a bottleneck exists, gate 1 is
where it would show up, and gate 1 is the one this ADR cannot evaluate without
a workload and a native reference program that do not exist yet. Building
those is the next real step in this direction, not a backend.

### The specification, the oracle, and the backends

Three things have a say in what a Cove program means, and they do not have the
same say. Ranked by authority: the Language Card and the tests that execute
its claims; then the reference interpreter; then any compiled backend.

The Language Card, together with the executable semantic tests that check it —
the `tests/e2e/` cases and the `test fn` suites that assert a documented
behaviour by running it — is the specification. Conformance here means a
program conforming to the Card, not a type conforming to a trait; it is the
Card that a disagreement is ultimately a disagreement about.

`crates/cove-runtime/src/interp.rs` is the reference interpreter, and its role
is to be the executable oracle: the thing that turns the Card's prose into a
fact a machine can check. That role is real and load-bearing — for almost any
question about what a program does, running it is the only practical way to
ask — but being the oracle does not make the interpreter the specification.
Where the interpreter disagrees with the Card, or with a semantic test, the
interpreter is what is wrong: the finding is an interpreter bug, and if no
test caught it, a missing test as well. ADR 0002 says as much in the same
breath as it names the interpreter the reference — "Because the interpreter is
the reference for semantics, its behaviour must stay traceable to the Language
Card" — and the sentence after it, "Where the Language Card and ADR 0001
disagree, the Language Card wins," is about the Card against another ADR
rather than the Card against an implementation, but it fixes the direction of
authority all the same. The Card is what the rest is answerable to.

A compiled backend, should one ever exist, sits below the interpreter and is
checked differentially against it, as the section below describes. The default
reading of a mismatch is that the backend is wrong, and that default is worth
committing to, because it is right nearly always: the interpreter is the code
every program in this repository has already run through, and it has no
lowering, no register allocation, and no code generator to get subtly wrong.
But it is a presumption, not a definition. Two implementations can agree with
each other and both be wrong about the Card — a shared misreading of the same
sentence, or a lowering that faithfully preserves an interpreter bug — and no
amount of agreement between them turns that into correctness. Agreeing with
the interpreter makes a backend consistent, which is exactly what a
differential test can establish and all it can establish. Being correct is a
claim about the Language Card, and only the Card and the tests that execute it
can settle it.

### The incremental path: typed IR, then AOT or adaptive compilation

Should gate 1 ever be crossed, the path is two steps, taken in order:

**A typed IR first.** `cove-sema` already produces a fully resolved,
type-checked `Program` — every call is resolved, every type is known. A typed
IR is a lowering of that fact into a shape a compiler can consume, not a new
source of truth: it changes nothing about what a program means, only what a
later stage is handed. This step alone is useful independent of whether a
compiler ever follows it, because it is also what a differential-testing tool
would lower both the interpreter's input and a compiled backend's input from.

**Then either whole-program AOT or adaptive compilation of hot functions —
not both, and not chosen here.** Whole-program AOT extends ADR 0009's `cove
build` in place: the command still names an output, not a strategy, and a
compiled binary replaces an embedded interpreter without the command
changing. Adaptive compilation instead compiles only functions that cross a
call-count or fuel threshold, leaving the rest interpreted, which suits a
workload dominated by a few hot functions rather than broad, flat cost. Which
one is right depends on the shape of the workload that actually crosses gate
1, which is exactly the thing this ADR does not have yet. Choosing between
them now would be deciding blind.

### What compiled code must still make observable

Compiling Cove code is only permitted to change how the computation *between*
Host calls executes. It may never change what a Host call costs to observe or
control. Concretely: every compiled call site must still charge
`Budget::charge_host_call`, check its grant through `HostRegistry`, dispatch
through the same `HostApi::call` shape, and record the same
`TraceEvent::HostCall` that an interpreted call would — a compiled backend
does not get its own dispatch path, only a faster way of arriving at this
one. Loop back edges, calls, and `await` points must still reach
`Budget::safepoint`, so fuel, deadlines, and cancellation keep meaning
something; a compiled backend defines its own fuel charge per compiled
safepoint, and that charge must stay close enough to the interpreter's that
`--stats` and a trace mean comparable things regardless of which backend
produced them. A trace's JSONL shape, documented in `trace.rs`, does not
change with the backend either — `cove replay` and anything else reading a
trace must not need to know which one ran.

### Specializing a Host operation the host declares fixed

The Host API schema (`OperationSchema` in `crates/cove-runtime/src/schema.rs`)
has no "fixed" or "stable" flag today; adding one is future schema work, not
this ADR's to design. But the question of what such a flag would license is
answered here, because it bounds what a compiler is allowed to do with it: a
host declaring an operation fixed licenses specializing its *dispatch* — a
compiled call site resolving the module and operation at compile time instead
of at every call, even devirtualizing the Rust call underneath — and nothing
more. The grant check, the budget charge, and the trace event fire exactly as
they would for an unresolved call, with one exception that is not special to
"fixed" operations at all: when tracing is off, nothing is recorded, fixed or
not, which is the same rule an interpreted run already follows.

Folding a fixed operation's *result* — deciding at compile time what
`clock.now()` will return, say, because a host promises it is deterministic
within a run — is explicitly out of scope here. That changes what is
recordable, not merely what is fast: a folded call would not appear in a
trace the way a dispatched one does, and that is a decision with its own
consequences for observability, not a corollary of this one.

### Differential testing between interpreted and compiled execution

Whoever builds a backend builds this test alongside it, not a bespoke one:
run every program in `benches/`, `examples/`, and `tests/e2e/` through the
interpreter and the new backend under identical fakes — the same ones this
harness and `cove test` already register — and diff two things. First, the
returned `Value`. Second, the sequence of recorded trace events, compared on
everything but wall-clock-derived fields (`wait_ns`, `cpu_ns`, `pause_ns`),
which are expected to differ; call shape — module, operation, capability,
arguments, outcome — must match exactly. A mismatch in either is presumptively
a compilation bug, and the presumption is strong enough to debug from: look at
the backend first. The rarer reading is the one "The specification, the oracle,
and the backends" above leaves room for — the backend matches the Language
Card and the interpreter does not — and a mismatch that turns out that way is
an interpreter bug plus a conformance test nobody had written, both of which
get fixed before the backend's behaviour is called a regression. What the test
never licenses is treating a mismatch as a difference of opinion between two
equally good answers: one of the two programs is wrong about the Card, and the
differential test exists to make sure somebody finds out which.

### Why this is hermetic, not a fixed baseline

`cove-bench` touches no network and no real filesystem, and every host it
grants is in-memory or virtual, so its correctness — every benchmark runs,
every metric comes back well-formed, the process exits 0 — is stable enough
to assert in CI on every push. Its wall-clock *numbers* are not: this very
machine measured a warm process startup of 3.9 ms and, on the same binary
immediately after a fresh build, a first invocation over a full second slower
— a page-cache effect that has nothing to do with Cove. A shared CI runner is
noisier than this machine, not quieter. So CI runs `cove-bench --iterations
3` for correctness only and asserts no threshold; the figures above are a
snapshot from one real run for a human to read, not a number this repository
promises to keep reproducing to the millisecond. Comparing a future backend
against a recorded baseline is a same-machine, same-build-profile exercise,
done when the question actually comes up — a number frozen into this ADR
today would describe a laptop, not Cove.

## Consequences

`cargo build -p cove-bench && ./target/debug/cove-bench` is a repeatable local
measurement anyone can run before and after a change that might affect
performance, and CI gains one fast, non-flaky step confirming the harness
itself keeps working. No runtime behavior changes: this ADR adds
`crates/cove-bench` and `benches/`, and touches nothing under
`crates/cove-runtime` or `crates/cove-cli`.

ADR 0002 and ADR 0009 stand exactly as written. What changes is that their
open question — whether a compiled backend is worth building — now has a
harness that could answer it, and five gates stating what an answer would
have to show. Today it shows two of them comfortably met, startup and trace
overhead; none failed; and three not evaluable — gate 1 because nothing has
been measured against a native reference, gate 4 because both benchmarks
baseline at zero live heap bytes, and gate 5 because there is nothing yet to
compile. That is not this ADR failing to decide; deciding correctly, on the
evidence available, is recording that the interpreter has not been shown to
need help, and saying which of the questions cannot honestly be asked yet
instead of counting them as answers.
