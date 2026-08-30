# ADR 0029: A benchmark number is evidence within one build, and a row is its own error bar

- Status: Accepted
- Date: 2026-08-31
- Supersedes: [ADR 0012](0012-performance-gate-and-native-backend.md)'s
  four statements about how the harness is run and what its numbers are
  evidence of — that **CI runs `cove-bench --iterations 3`**, that **wall time
  and process startup are reported as `{min, mean, max}`**, that
  `./target/debug/cove-bench` **is "a repeatable local measurement"**, and that
  comparing against a recorded baseline is **"a same-machine,
  same-build-profile exercise"**. Nothing else in ADR 0012 is touched: not the
  five gates, not their status, not the ranking of the specification above the
  oracle above a backend, not the decision that CI asserts no threshold, and
  not the two hermeticity arguments those four statements sit next to.
  "What of ADR 0012 survives" below goes through them
- Records, rather than decides. Every number here was already measured and
  already written down, in `docs/VM_ARCHITECTURE.md` and in the code and CI
  config the four statements describe. This ADR exists because ADR 0012 is
  accepted and immutable and four of its sentences are no longer true, and a
  reader arriving at it needs to be told so from ADR 0012's own header. **It
  decides nothing about performance.** See "What this ADR does not decide"
- Implementation status: everything below is built and merged.
  [PR #188](https://github.com/myuon/cove/pull/188) added the statistics,
  `perf(ci): run the benchmark harness optimized, and once` (381305a) changed
  CI, [issue #204](https://github.com/myuon/cove/issues/204) added
  `[profile.bench-stable]` and the negative result it holds, and
  [PR #207](https://github.com/myuon/cove/pull/207) made `round-robin` the
  default sample order and closed
  [issue #205](https://github.com/myuon/cove/issues/205)

## Context

ADR 0012 added `crates/cove-bench`, decided that CI runs it for correctness
and asserts no threshold, and set five gates that a compiled backend would
have to cross before building one is worth it. All of that stands. What has
not stood is the surrounding description of *how the harness runs* and *what
one of its numbers is evidence of*. Four statements have drifted away from the
tree, none of them by a single change, and none of them by anyone deciding to
change what ADR 0012 decided:

- CI's benchmark step was rewritten for speed, and the iteration count went
  with it.
- The harness was given real statistics, so the three-number summary ADR 0012
  documents is now seven numbers plus the samples, and there is a comparison
  mode that did not exist.
- Two rounds of measuring the measurement — first a wide layout band, then a
  direct null study of the machine — established what a benchmark number is
  and is not evidence of, and the answer is narrower and more specific than
  "repeatable".
- The same two rounds showed that holding the machine and the build profile
  fixed is necessary and is not sufficient, because the largest term is the
  build's code layout, which changes between two builds of the same profile on
  the same machine.

ADR 0012 is accepted, so it does not get edited to say any of this. Its header
gets a pointer to here and nothing else. This ADR is that pointer's
destination.

The reason it is worth an ADR rather than a paragraph in
`docs/VM_ARCHITECTURE.md` — where all of the evidence already lives — is that
ADR 0012's four statements are the ones a future reader will act on. Somebody
deciding whether gate 1 has been crossed will reach for ADR 0012, read that a
same-machine comparison is the exercise, run one, and get a number they cannot
interpret. The gates are fine. The instructions beside them are not.

## What is true now

### 1. CI runs `cove-bench --iterations 1`, optimized

ADR 0012: "So CI runs `cove-bench --iterations 3` for correctness only and
asserts no threshold."

`.github/workflows/ci.yml` now builds `cargo build --release -p cove-cli -p
cove-bench` and runs `./target/release/cove-bench --iterations 1`. The step's
purpose is unchanged and the CI comment says so in ADR 0012's own terms: every
benchmark executes, every metric comes back well-formed, the process exits 0,
and no threshold is asserted.

What changed is cost. Unoptimized, three iterations each, with the traced and
untraced runs `trace_overhead` needs on top, came to nine executions of every
benchmark — 422 seconds of a 514-second pipeline, 82% of a run spent on the
one step that asserts nothing. The pipeline is now 137 seconds. The
benchmarks are sized for an optimized build (`benches/arith` turns a loop two
million times), and neither the profile nor the count changes any of the three
things the step checks.

Two details are worth carrying here because they are easy to get wrong later.
The CLI is built in the same profile, because `startup` spawns the `cove`
binary beside the harness and an optimized harness must not measure a debug
CLI. And `--release` rather than `bench-stable`: see 4 below.

### 2. A wall-time series reports seven statistics and its own samples

ADR 0012 describes wall time as "`{min, mean, max}` nanoseconds over
`--iterations` runs", `heap_peak_bytes` the same, and process startup as
"`{min, mean, max}` over `--iterations` samples".

[PR #188](https://github.com/myuon/cove/pull/188) added `crates/cove-bench/src/stats.rs`.
`wall_ns` — for every row, including `startup` and the lowering row — is now:

```json
{"min":…,"mean":…,"max":…,"p25":…,"median":…,"p75":…,"iqr":…,"samples":[…]}
```

`heap_peak_bytes` is the same object without `samples`. `min`, `mean` and
`max` are first and keep their names deliberately, so that a reader written
against ADR 0012's description still works; the file says so where it emits
them.

The median rather than the mean, because a wall-time series has a floor and no
ceiling and the mean is the statistic that moves furthest when one late sample
arrives. The interquartile range rather than `max − min`, because the range
grows with the sample count and the middle half does not. `samples` is in the
order the run took them, not sorted, so a reader can see *when* in a series a
slow sample arrived — the difference between a machine that drifted and a
benchmark that is noisy.

The same PR added a mode ADR 0012 has no account of at all. `--baseline
<path>` reads a previous run's output and emits one `"kind":"comparison"` line
per row, carrying `delta_pct` — the shift between the two medians — and a 95%
interval around it (`ci_low_pct`, `ci_high_pct`, `confidence`), built as a
percentile bootstrap of ten thousand resamples from a fixed seed, so the same
two series always produce the same interval. The verdict is read off the
interval: `regression`, `improvement`, `inside the noise`, or `underpowered`
when either side has fewer than six samples, six being the point at which a
distribution-free 95% statement about one median first exists.

This is why the baseline format is the harness's own output: a summary cannot
be compared against a summary, because the interval is built by resampling the
samples themselves.

### 3. "Repeatable" is true of a ratio and an exact count, and false of a cross-build absolute

ADR 0012's Consequences: "`cargo build -p cove-bench &&
./target/debug/cove-bench` is a repeatable local measurement anyone can run
before and after a change that might affect performance."

The command is also out of date — CI and every table in
`docs/VM_ARCHITECTURE.md` are optimized builds now, and an unoptimized run of
the current benchmarks takes minutes — but that is the small half. The large
half is "repeatable", which needs qualifying in a specific direction, and the
direction is the opposite of the one that reads naturally.

**The machine is not the problem.** Twelve `cove-bench --iterations 15` suites
of one unmodified release binary, in one four-hour sitting, give 66 pairs per
row; the shift between two runs' medians, pooled over all 21 rows, has
**median 0.78% and 90th percentile 2.58%**. The quietest row in the suite,
`arith` on the VM, **never moved more than 0.84%** in those 66 comparisons.
Excluding the lowering row and the two `startup` rows the figures are 0.71%
and 2.18%.

That null does not grow with anything anyone expected it to. Two suites nine
minutes apart disagree exactly as much as two two hours apart (median 0.74%
under 15 minutes, 0.79% at 15–45, 0.78% over 45), and the six suites that had
a real 16-second incremental rebuild in front of them are not distinguishable
from the six that did not. The CPU's effective clock, measured directly by a
dependent `addq` chain five times before and five times after every suite —
220 probes — spans **0.7% through its middle half**, and `pmset -g therm`
reported `CPU_Speed_Limit` of 100 at all 44 snapshots. Thermal throttling and
scheduler limits were ruled out *by proxy*, not measured: `powermetrics` needs
root and was not run, so there is **no package-temperature and no per-core
residency figure**, and this ADR does not have one to give.

**A suite-wide maximum is a different statistic from a row's error bar.**
[Issue #205](https://github.com/myuon/cove/issues/205) was filed on the
observation that one binary disagreed with itself "by up to 7.4%", read as a
per-row error bar. It is not one. Compute, for each of the same 66 null pairs,
the largest shift over the suite's rows, and that maximum has median 3.99% and
reaches 7.4% or more in **14% of pairs** — 7.4% is an ordinary draw from the
null. Over the 18 execution rows alone the maximum's median is 2.89% and
nothing reaches 7.4% at all.
[PR #207](https://github.com/myuon/cove/pull/207) corrected the reading and
closed the issue. **Quote the row; never quote the suite maximum.**

**The large term is layout, and it belongs to the build.** The workspace has
no `[profile.release]`, so release builds with `codegen-units = 16` and no
LTO, and rustc partitions codegen units by module — which makes *where* code
lives a performance variable independent of what it does.
[Issue #179](https://github.com/myuon/cove/issues/179)'s control is the clean
demonstration: an `Inst` variant that is never emitted, never executed, and
reachable from no program. It measured **+23.5% on `arith`/VM in one build**
and **−1.00% on the same row in another**, both against a row whose own null
is under 1%. Neither is noise and neither cancels the other: they are two real
observations of two different builds. A user running the +23.5% binary is
23.5% slower on that program. What the number cannot do is attribute anything
to a design, because the arrangement of the code is not something this
workspace controls or measures.

**Two rows are not evidence at ordinary sizes.** The lowering row times a
**0.13 ms** operation, so fifteen samples of it are two milliseconds of
measurement; `startup` spawns a process and its 99th-percentile sample is
**eighty times its median** — the page-cache effect ADR 0012 itself warned
about, still there. Between them, the lowering row and the two `startup` rows
are the largest shift in **two thirds** of null pairs. Neither is evidence of
anything under about 10%.

So the honest replacement for "repeatable local measurement":

- **A ratio within one build is repeatable.** Two rows of one `--matrix` run
  share whatever layout that binary has.
- **An exact count is repeatable.** `--stats` counts what ran, and no rebuild
  can touch it. A count is not *sufficient* — issue #126 found three changes
  with identical counts summing to 19% slower — but a count that moved when it
  should not have is the cheapest way to catch a mistake.
- **A wall-clock absolute compared across two builds is not.** Not for a
  regression and not for an improvement.
- **The discipline is to bracket**: base, variant, base again, and quote the
  two base runs' disagreement as the error bar. Averaging the two base runs is
  worth about 15% off the error and nothing at all off the worst case; the
  reason to bracket is the third run, because the two base runs' disagreement
  is the only error bar measured in the same conditions as the result.

One harness change follows from the same round and is recorded here because
ADR 0012's account of the harness is what this ADR is repairing.
**`--sample-order round-robin` is the default** (PR #207). The harness used to
take every sample of one row before starting the next, so a row's whole series
came from one instant of a nine-and-a-half-minute suite — for the fastest rows,
twenty milliseconds of it — and nothing in the row's own spread could say so.
Round-robin takes one sample of every row per pass. The suite takes the same
564 seconds and the same rows run the same number of times; only *when* each
sample is taken changes. Over the 18 rows the order governs, the null improved
from 0.61% to 0.45% at the median and 13 of 18 rows improved — a quarter of
the noise, for nothing, which is the honest size of it and not a fix.
`--sample-order blocked` reproduces the old order. **At `--iterations 1` the
two orders are the same sequence, so CI is unaffected.**

### 4. Same-machine, same-build-profile is necessary and is not sufficient

ADR 0012: "Comparing a future backend against a recorded baseline is a
same-machine, same-build-profile exercise, done when the question actually
comes up."

This one is not untrue. It is incomplete, and it is incomplete in the way that
matters most, because a reader who satisfies both conditions will believe they
have controlled for everything. They have controlled for the small term. Both
builds in the +23.5% measurement above were release builds on this machine in
one sitting, and they differed by an enum variant no program can reach.

Fixing the profile does not fix layout, and the obvious remedy was tried and
does not work either. [Issue #204](https://github.com/myuon/cove/issues/204)
added `[profile.bench-stable]` — `inherits = "release"`, `codegen-units = 1`,
nothing else, no LTO — because #179's reasoning implies that one codegen unit
per crate should stop module boundaries from being a performance variable. Run
against the same dead-variant control, **it made the spurious shift larger**:
largest absolute shift 6.78% under `bench-stable` against 3.87% under plain
release in the same session, a band 9.53 points wide against 6.42. The build
costs 44% more from scratch and 96% more incrementally. The profile is
checked in, holding that recorded negative result, and **nothing selects it**:
CI uses `--release` and so does every table in `docs/VM_ARCHITECTURE.md`. The
claim is not "`codegen-units = 1` is worse" — one perturbation does not size a
band — it is "it is not better, and it costs".

So the complete statement is: same machine and same build profile are
necessary; what makes two numbers comparable is that they came from *the same
build*, or that the difference between two builds was bracketed and quoted
against a null measured in the same session.

## What of ADR 0012 survives

A reader of a superseding ADR needs to know what was not replaced as much as
what was. All of the following stand exactly as ADR 0012 wrote them, and this
ADR does not reopen any of them:

- **The ranking: the specification, then the oracle, then a backend.** The
  Language Card and the executable semantic tests are the specification; the
  reference interpreter is the oracle; a compiled backend sits below both and
  is checked differentially against the interpreter. Nothing measured here has
  any bearing on it. It remains load-bearing, and ADR 0019 already left it
  standing when it replaced ADR 0012's "the interpreter is the only backend".
- **CI does not gate on wall time.** The benchmark step asserts correctness
  and no threshold. The evidence above is the strongest argument yet *for*
  that decision, not against it: a shared runner cannot do better than a quiet
  machine whose own quietest row still moves by up to 0.84%, and a threshold
  set anywhere useful would fire on layout.
- **All five gates, and each one's status.** Throughput within roughly a 10x
  band of a native reference; warm startup not past roughly 50 ms; trace
  overhead within roughly 5x; peak live heap within roughly 2x of a baseline;
  compile time undefined until a compile stage exists. Two met, none failed,
  three not evaluable — gate 1 for want of a reference native program, gate 4
  because both interpreter benchmarks baseline at zero live heap bytes, gate 5
  because there is nothing to compile. Nothing here changes a gate's threshold
  or its status.
- **Everything the harness measures besides wall time.** `fuel_spent` and
  `fuel_per_sec`, `heap_peak_bytes`, `host_calls`, `irreversible_writes`, and
  the trace-overhead ratio. Fuel and instruction counts are, if anything,
  *more* load-bearing after this round, since they are the figures a rebuild
  cannot move.
- **Why this is hermetic, not a fixed baseline**, and the refusal to check in
  a golden number. The first sentence of that section — no network, no real
  filesystem, every host in-memory or virtual, so correctness is stable enough
  to assert on every push — is what makes the CI step defensible at one
  iteration as much as at three.
- **Everything a compiled backend must still make observable**, the typed-IR
  path, the differential test, and what a "fixed" Host operation would
  license. None of it is a measurement claim.

ADR 0012's absolute numbers — 95.0 ms and 17.5 ms on `pure`, 2.2–2.8x traced,
3.9–5.3 ms of startup — stay as written and stay readable as what they always
were: a snapshot from one real run on one machine, which that ADR was explicit
about not promising to reproduce. This ADR does not restate them and does not
correct them. It says what such a snapshot is evidence of, which is the thing
ADR 0012 did not have a way to say.

## What this ADR does not decide

Named, so that a later reader does not mistake a record for a decision:

- **It does not decide the performance gate afresh.** No gate's threshold
  moves, no gate's status changes, and no new gate is added. A gate is crossed
  by a measurement, and this ADR contains no measurement of Cove against
  anything.
- **It does not decide a build configuration.** `[profile.release]` stays
  absent and `[profile.bench-stable]` stays unselected. Whether the workspace
  should eventually pin a layout, and how, is open; the evidence says only
  that `codegen-units = 1` alone is not the answer.
- **It does not decide whether the layout band can be reduced.** The
  experiment that could settle it is described in `docs/VM_ARCHITECTURE.md` —
  at least five distinct never-executed perturbations per profile, each
  bracketed, compared on the spread of the perturbations rather than on any
  one of them, roughly six hours per profile — and it has not been run.
- **It does not decide a CI threshold**, nor propose one. See above.
- **It does not decide what the lowering row and `startup` should become.**
  Both are known not to be evidence at the few-percent level. Whether to
  resize them, drop them from a maximum, or leave them is not settled here.

## Consequences

ADR 0012 gains a `Superseded in part by` line naming the four statements, and
nothing else in that file changes — including the four sentences themselves,
which stay wrong on the page, because a reader needs to be able to see what
this project believed when it wrote them.

Anyone comparing two Cove builds now has the rules stated where the gates are,
rather than only in a performance document they may not reach: quote the row
and not the suite maximum, bracket and quote the two base runs' disagreement,
prefer a within-build ratio and an exact count to a cross-build absolute, and
distrust the lowering row and `startup` below about 10%. Those rules were
already true before this ADR and are already written down; what this adds is
that ADR 0012 now points at them.

No code changes and no CI changes. Every fact recorded here was already the
state of the tree when this ADR was written.
