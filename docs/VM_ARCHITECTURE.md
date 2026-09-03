# The VM: what this document measures, now that the backend it described is gone

The backend this document described has been deleted. [ADR 0034](adr/0034-one-physical-word-stack.md)
replaced it — the crate then called `cove-ir`, its `Vm` and `FrameVm`, the
`admits` predicate and the duplicate heap all went with the cutover — and
[docs/LINEAR_VM.md](LINEAR_VM.md) is the design of what runs a Cove program
now. Read that one for how the machine works.

What this document is now: the record of how this project takes a
measurement, which turns out to be the part of it that was never about the
backend. The ±6% code-layout band, what `codegen-units = 1` was measured to
be worth, the control-build discipline of base, variant, base again, how to
read a reported spread, and that a row's own error bar is about a quarter of
the maximum-over-rows figure — none of that is a property of the interpreter
loop this document used to describe. It is a property of the harness and the
machine, and both are still here.

The architecture chapters and the per-change measurements taken on that
architecture were deleted rather than rewritten. Documentation of code that
does not exist is a trap: it reads as current, it is cited as if it were
current, and the gap between what it says and what runs is invisible until
someone acts on it. Those chapters are not lost — they are in git history, in
this file, at commit `6e90085`, for anyone who wants to read what was built
and what was measured about it — but they do not belong in a document that
purports to describe the present.

Every figure kept below was taken on the deleted backend. That does not
weaken it. What "What the measurement itself costs" measures is the harness
and the machine — a code-layout band, a build's disagreement with itself
forty minutes apart, what a sampling profiler costs to attach — and a layout
band and a same-binary null are properties of the build and the machine, not
of which interpreter loop the build happened to contain. The numbers are
dated; the method is not.

## Answering the ADR pointers this deletion would otherwise break

An accepted ADR is immutable, so the ADRs that cite a section deleted here
cannot be edited to point somewhere else. This document repairs the pointer
from its own side instead. Six accepted ADRs cite this document by section
name; four of those sections are gone, and two survive because they are the
section kept below.

- **"The value representation, audited"**, cited by
  [ADR 0028](adr/0028-five-representations-and-one-is-public.md):83, was
  the audit issue #116 asked for: `cove_runtime::Value` was 40 bytes because
  one variant — `HostFn`'s two fat pointers, for `console.println` — set the
  width of the whole enum, one boxing commit brought it to 24, an 8-byte
  padding experiment priced every width from 24 to 40 at roughly 0.7% to
  1.9% per 8 bytes on the VM, and going narrower than 16 bytes was rejected
  on semantics rather than on cost — Cove's `Int` does not fit in a NaN box
  or a tagged pointer's payload. It described the deleted backend's `Value`
  representation and the table of its measured widths; both are gone.
- **"Collection is non-moving"**, cited by ADR 0028:289 and 888, said that
  nothing the deleted backend's collector touched was relocated, walked
  through what a moving collector would have had to rewrite — the value
  stack's absolute indices, held by the place stack and by every place a
  frame handed a callee — and left that bill unpaid rather than paid. It
  described a heap this backend no longer has.
- **The safepoint list**, cited by ADR 0028:647 and [ADR 0033](adr/0033-an-identity-is-not-a-vm-heap-object.md):225,
  enumerated where the deleted backend's `Vm::collect_if_due` could safely
  run a collection — entering the entry, every call, every return, every
  back edge with enough fuel gathered, the per-block charge past
  `SAFEPOINT_INTERVAL`, an `await`, and a `?` that failed — and the
  argument, that nothing outside the runtime holds a view into the heap at
  one of those points, for a `Vm` that has been deleted.
- **The table of bounds**, cited by [ADR 0024](adr/0024-a-stop-is-a-bound-not-a-point.md):28,148
  and [ADR 0030](adr/0030-a-host-call-asks-the-fuel-limit.md):139, gave a
  maximum amount of work each stop mode — the run's cancellation, a task's
  own, a bounded call's flag, the deadline, fuel, `max_host_calls`,
  `max_call_depth`, the concurrency limit — could let run past it on the
  deleted backend, in units of one gathering (`G`) and one turn (`T`) of a
  loop. The bound's *shape* changed with the replacement, and
  [ADR 0040](adr/0040-a-bound-outlives-its-backend.md) is where the table
  lives now: it supersedes ADR 0024's placement of it precisely so that the
  next backend replacement cannot strand it here again.

One deleted section is cited by *code* rather than by an ADR, and the
citation is load-bearing in a way the others are not. **The
calling-convention matrix** — nine rows of the deleted backend's boundaries,
nine samples a row on a quiet machine, reported as `{median, min, max}` — is
the only set of distributions this repository has ever written down, and
`crates/cove-bench/src/stats.rs` cites it to *refuse* a claim: if benchmark
timings were reliably right-skewed then `max - median` would exceed
`median - min` on most rows, and it exceeds on three, falls short on five,
and ties on one. Those three counts are the whole of what the citation
needs, so they are restated here and the table they came from is not. What
justifies the median in `cove-bench` was never the skew.

[ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md) and
[ADR 0029](adr/0029-a-benchmark-number-is-evidence-within-one-build.md) also cite this
document by name, for the control-build methodology rather than for a
deleted section. Those pointers still resolve: "What the measurement itself
costs" and "What `codegen-units = 1` was measured to be worth" are the
section kept below, verbatim.

## What the measurement itself costs

[Issue #123](https://github.com/myuon/cove/issues/123) asks for one workload
under five configurations — production; instruction statistics off with fuel
unchanged; both off, for attribution only; a sampling profiler attached; and
trace off against trace on — and for wall-time distributions rather than
single runs. Two of those five did not exist when it asked, and the reason is
the thing being measured. `Vm::charge` adds a block's length to `self.fuel`
and to `self.instructions` in the same two lines, which is most of why
charging by the block is cheap, so there was no configuration in which one of
them was off and the other was on.

### The mechanism that turns a thing off can cost more than the thing

Two mechanisms could build that configuration, and the choice between them is
not a detail of the measurement — it is the first result.

A compile-time removal costs nothing at run time and changes the binary, which
matters here more than it would elsewhere: this document has established more
than once — including in a section since deleted — that a change altering
nothing a program executes can still move `arith` by
several percent, because the dispatch body's footprint and its branch-target
alignment are costs every program pays. A runtime flag leaves the binary alone
and puts a branch on the path, and the branch is the same shape as the
increment it guards.

Both were built and both were measured. The flag is a `bool` on the `Vm`, read
from the environment once at construction so that one binary gives both
halves, and tested in `Vm::charge` around the increment. Fifteen runs of
`cove run <bench> --backend vm --stats`, medians of `execute=`, interleaved
over three rounds:

| build                          | `arith` | `field` |
| ------------------------------ | ------: | ------: |
| production                     | 84.7 ms | 447.9 ms |
| the flag, counting             | 86.6 ms | 448.4 ms |
| the flag, not counting         | 88.7 ms | 449.3 ms |
| the counter removed at compile time | 84.1 ms | 441.7 ms |

**The flag recovers nothing, and on `arith` it costs 2.4% to have the counter
switched off.** That last figure reproduced on all three rounds and it is
within one binary, so it is not layout; what it is has not been established
here, and it is reported because it was measured rather than because it is
understood. What the table does establish is the shape of the answer: the
branch costs what the increment costs, so a flag would put the whole of the
mechanism on every production run in order to save a figure that is worth
between 0.7% and 1.9% on the benchmark most sensitive to it.

**So no flag is shipped**, and configuration 2 is a build made to be measured
and thrown away. The patch that makes it, and eight others, are
`scripts/ablate/*.patch`, with `scripts/ablate/run.sh` to apply each one,
build it, and put the binary somewhere a measurement can name. They are
patches rather than a cargo feature for the reason above: a feature would be
carried by the production binary, and this is a measurement rather than a
capability. They are expected to rot, and `git apply` refuses loudly when they
do, which is the wanted behaviour — an ablation that applied to code it was
not written for would measure something nobody named.

### What each part of the bookkeeping costs

Every row below was removed *alone* from the shipped build, and the shipped
build was measured on both sides of the study: it read 84.6 ms and 84.8 ms on
`arith`, 448.4 ms and 447.4 ms on `field`, and 232.2 ms and 232.0 ms on
`call`, so the machine did not move under it. Medians of fifteen, through the
`cove` binary. A positive number is what the removal saved.

| removed                                                 | `arith` | `field` | `call` |
| ------------------------------------------------------- | ------: | ------: | -----: |
| the block instruction counter                            |   +0.7% |   +1.4% |  +0.2% |
| fuel accumulation and its interval compare               |   +4.7% |   +2.8% |  −2.1% |
| both of those — configuration 3                          |   +4.6% |   +4.2% |  −2.6% |
| the back edge's whole check                              |  +11.4% |   +7.0% |  +1.4% |
| the safepoint at every call and every return             |   −0.9% |   +0.9% | +38.7% |
| the budget's lock and accounting inside every safepoint  |   +7.1% |   +5.8% | +40.3% |
| the two stop flags a safepoint reads                     |   −9.1% |   −0.5% |  +2.5% |
| the collection a safepoint asks about                    |   −8.5% |   −1.7% |  −6.0% |

Four readings, and the fourth is the one worth the section.

**The instruction counter is nearly free and its three measurements do not
agree.** It is 1.9% on `arith` in issue #126's ablation, 2.9% in a round of
this study taken before the change below landed, and 0.7% here; on `field`,
which a section since deleted established as the benchmark to trust for a
small effect, it is 1.4%. The honest statement is that it is worth something
under two percent and that no single measurement of it is separable from
code layout. It is not what makes `--stats` cost anything.

**Two removals made things slower, and both are pure deletions.** Removing the
stop-flag read from `Vm::safepoint` costs `arith` 9.1%, and removing the
collection question costs it 8.5% and `call` 6.0%. Neither can be a real cost
of the code that was deleted. This is the same layout sensitivity the
dispatch-loop study found and the same size, and it is kept here rather than
tidied away because a reader who sees only the positive rows will believe the
study is more precise than it is. What follows from it is that `stopped_here`
and `Heap::should_collect` are free within anything measurable here, which is
the useful half of a result that looks like nonsense.

**Configuration 3 is not the floor and does not sum.** Removing the charge
entirely also stops the back edge from ever firing, because what a back edge
reads is the fuel the charge accumulates — so the third row should be at least
the fourth and it is less than half of it. `arith` is superadditive in the
other direction too, as a section since deleted recorded. No subset of
these rows adds up to another one, and the table should be read as eight
separate statements rather than as a decomposition.

**What dominates is none of the above.** It is the mutex a safepoint takes to
reach the run's `Budget`, and it does not show on `arith` because `arith`'s
loop calls nothing. On `benches/call`, which calls one function per turn, the
whole of the budget's lock and accounting is 40.3% of the run. The profile
says the same thing in different words — `samply` at 5 kHz, self time, on the
symbol-bearing build:

```text
benches/arith                             benches/call
 74.66%  Vm::execute                       45.99%  Vm::execute
  7.53%  HostRegistry::with_budget         16.02%  HostRegistry::with_budget
  4.79%  pthread_mutex_lock                10.34%  pthread_mutex_lock
  4.11%  pthread_mutex_unlock              10.08%  pthread_mutex_unlock
  2.05%  interp::stopped_here               5.43%  Vm::leave
```

A `Budget` lives behind `Mutex<Option<Budget>>` on the `HostRegistry`, because
every task of a run shares one and a task runs on a thread of its own. A call
is an unconditional safepoint and so is a return, so a loop that calls one
function pays for two acquisitions a turn, and the two of them together cost
more than the dispatch of the whole loop body.

### One acquisition of three was buying nothing, and is gone

`Vm::enter` took the lock twice. Once to read `Limits::max_call_depth`, and
once again inside the safepoint that follows. The first of the two is what
this change removes, and what makes it removable is that the answer cannot
have changed: a `Budget` is installed with `HostRegistry::set_budget`, which
needs `&mut HostRegistry`, and a `Vm` holds the registry by shared reference
for as long as it exists — so no budget can be installed or replaced while a
run is in progress, and the limit a call reads is the limit the previous call
read. `Vm::host_call_depth_limit` asks once, through the lock, and answers
from a field afterwards. It asks on the first call rather than in `Vm::new`
because `Vm` is public and nothing about constructing one promises that a
budget has been installed yet.

Nothing else moves. The order the three checks are made in is the order
`Interpreter::call_target` makes them and is unchanged, the lock is still taken to
build the error when the limit is exceeded, and a `Vm` with no budget behind
its registry still has no limit, which is what `with_budget` answering `None`
has always meant.

| bench    | before   | after    |          |
| -------- | -------: | -------: | -------- |
| `call`   | 279.0 ms | 232.6 ms | **16.6% faster** |
| `method` | 911.8 ms | 822.9 ms | 9.8% faster |
| `pure`   |  2.82 ms |  2.33 ms | 17.2% faster |
| `arith`  |  88.4 ms |  85.2 ms | 3.7% faster |
| `field`  | 442.2 ms | 445.7 ms | 0.8% slower |

Medians of fifteen, with the parent measured on both sides. The last two rows
are the control and should be read as one: neither loop calls a Cove function,
so neither can respond to this, and both are inside the band `arith` is known
to move in for layout alone. An ablation that removed the read entirely,
rather than remembering it, measured 16.7% and 9.4% on the first two — so the
change recovers the whole of what was there to recover, which is what says
nothing was left behind.

The interpreter does the same thing at the same point and was not changed.
Nothing here measures the oracle, and a change to it would have to be measured
before it could be claimed; the site is `Interpreter::call_target`'s own
`max_call_depth` read, for whoever asks the question next.

What is left on the call path is two acquisitions a turn, one at the call and
one at the return, and they are still the largest single cost `benches/call`
has. That was a target rather than a finding when this section was written.
The section below is what became of it.

### The other two acquisitions are gone, and the counters are atomics

[Issue #182](https://github.com/myuon/cove/issues/182) asked what the mutex was
protecting, and the answer was: nothing that needed a mutex. Per safepoint a
`Budget` adds to `fuel_spent`, reads the run's cancellation, compares against a
fuel limit fixed before the run began, and every `DEADLINE_CHECK_INTERVAL`th
time reads a clock that started before the run began. The cancellation was
already an atomic flag. `limits` and `started_at` are immutable for a run.
`fuel_spent` and the deadline tick were plain integers *because the struct
holding them was reached by `&mut`*, and for no other reason.

So they are `AtomicU64`s now, `Budget::safepoint` takes `&self`, and the whole
of the accounting lives in one `Arc<Accounting>` that every thread of the run
shares. `crate::budget::Meter` is the handle onto it: a backend takes one where
a run begins — `Vm::new`, `Interpreter::new`, and again in `invoke_within` and
`run_entry_within` after the budget they were handed is installed — and charges
through it at every safepoint after that with no lock at all.
`HostRegistry::with_budget` still exists and still locks; what is left behind it
is installing a budget, reading the counters back for `--stats`, and the two
charges that are not per-instruction (a host call, a spawn).

**Nothing about the schedule moves.** `SAFEPOINT_INTERVAL`, `BACK_EDGE_FUEL`,
`SAFEPOINT_FUEL` and `DEADLINE_CHECK_INTERVAL` are unchanged, which matters
because ADR 0024 states each stop as a bound in those constants' arithmetic. The
order of the three questions inside a safepoint is unchanged, so which stop is
reported is unchanged. Fuel is still counted before anything can refuse, which
is ADR 0024's "pending fuel is never lost". A Host call is still a stop point
for all three flags and for the deadline and `max_host_calls`, which is the
other half of the same decision — issue #120 found real faults in both of those
and `crates/cove-runtime/tests/responsiveness.rs` still measures them.

**The VM, medians of fifteen against a baseline recorded at `6d53791`, with the
95% percentile-bootstrap interval on the median shift:**

| bench       | at `6d53791` | with this |      shift |     95% interval |
| ----------- | -----------: | --------: | ---------: | ---------------: |
| `call`      |       236 ms |    153 ms | **-35.1%** | -35.5% to -34.2% |
| `method`    |       812 ms |    646 ms | **-20.4%** | -20.9% to -20.2% |
| `pure`      |      2.37 ms |   1.35 ms | **-43.2%** | -44.1% to -41.2% |
| `field`     |       432 ms |    423 ms |  **-2.0%** |   -2.4% to -0.9% |
| `arith`     |      86.0 ms |   77.8 ms |  **-9.6%** |  -10.1% to -9.2% |
| `arrayget`  |       666 ms |    652 ms |  **-2.2%** |   -2.7% to -1.6% |
| `chars`     |       818 ms |    805 ms |  **-1.6%** |   -2.2% to -0.9% |
| `hostheavy` |      3.80 ms |   3.89 ms |  **+2.4%** |     0.7% to 3.8% |

**The interpreter, the same run:**

| bench       | at `6d53791` | with this |      shift |     95% interval |
| ----------- | -----------: | --------: | ---------: | ---------------: |
| `call`      |      1531 ms |   1368 ms | **-10.7%** | -10.9% to -10.3% |
| `method`    |      2780 ms |   2589 ms |  **-6.9%** |   -7.4% to -6.6% |
| `pure`      |      15.5 ms |   14.1 ms |  **-9.4%** |   -9.8% to -8.4% |
| `field`     |       829 ms |    778 ms |  **-6.1%** |   -6.3% to -4.2% |
| `arith`     |       429 ms |    363 ms | **-15.4%** | -16.4% to -15.1% |
| `arrayget`  |      1492 ms |   1431 ms |  **-4.1%** |   -4.5% to -3.4% |
| `chars`     |      1916 ms |   1880 ms |  **-1.9%** |   -2.1% to -0.9% |
| `hostheavy` |      4.94 ms |   4.86 ms |  **-1.7%** |   -2.7% to -0.8% |

Four readings.

**`call` captured 35.1% of the ablation's 40.3% ceiling, and `pure` more than
that.** The ceiling was measured by removing the lock *and the accounting*
together, and the accounting is still here — a `fetch_add`, two compares, and
the branch that picks one safepoint in sixty-four to read a clock at. What the
gap between 35.1% and 40.3% prices is that remainder, which is the honest
reading of a partial capture and is why this section does not claim the whole
of it.

**`field` moved 2.0%, where the ceiling was 5.8%.** `field`'s loop calls
nothing, so the two acquisitions a call and a return cost were never its to
save; what it has is back edges, and a back edge already waited for
`BACK_EDGE_FUEL` to gather. That the interval excludes zero at all is the
useful part, and the size of it is inside the band this document records for
layout alone.

**The interpreter moved as much as the VM did, and it was not the target.**
Nothing about the tree walk changed except which side of the lock its
`charge_safepoint` and its `max_call_depth` read are on, and `arith` on the AST
backend is 15.4% faster for it. That is the same lock, at the same
schedule, on a backend that charges a fixed amount per safepoint rather than in
blocks — so it takes the lock *more* often per unit of work, and it is the row
that shows the acquisition's own cost most plainly.

**`hostheavy` on the VM went the other way, 2.4% slower with an interval of
0.7% to 3.8%.** It is the one benchmark dominated by the path that still locks,
so there is nothing here for it to win, and `host.rs` gained a method — which
[#179](https://github.com/myuon/cove/issues/179) says is enough on its own to
move a benchmark that never executes it. `startup` on the interpreter is the
other row that moved the wrong way, 2.8% with an interval of 1.5% to 6.1%, and
it times a process from `exec` to exit with a few milliseconds of Cove in it.
An interval says a difference is real; it does not say the difference is the
change. Both are recorded rather than explained away, and they are the rows a
reader should be most suspicious of.

The whole run is nineteen rows: fifteen improvements, two inside the noise, the
two above. The widest interval that did not clear zero is `startup` on the VM at
-3.4% to +1.9%, so a regression larger than that anywhere in the suite would
have been seen.

### The profiler, the trace, and `--stats` itself

`CARGO_PROFILE_RELEASE_DEBUG=1` is a configuration change and it is named
here because it is one, but it is not a measurable one: the symbol-bearing
build read 86.1 ms against 85.4 ms on `arith`, 440.0 ms against 444.1 ms on
`field`, and 231.8 ms against 233.1 ms on `call` — under a percent in both
directions and not separable from layout. So a profiled run and a production
run are the same program, which is what makes the profiles above readable
beside the times.

Attaching `samply` is a different matter, and it has to be read in the right
units. The section the profiler is watching gets 6.3% slower at its default
1 kHz and 6.8% at 10 kHz, which is small; the *process* goes from 93.9 ms to
1,359 ms, which is fourteen times longer and is almost entirely samply's own
setup and symbolication rather than anything the program did. A reader who
times the wrapper will conclude the profiler costs 1,265 ms per run and will
be wrong by two orders of magnitude.

| configuration        | `arith`, `execute=` |
| -------------------- | ------------------: |
| no profiler          |             86.0 ms |
| `samply -r 1000`     |             91.4 ms |
| `samply -r 10000`    |             91.8 ms |

The trace is the configuration with the largest spread between two programs,
and the reason is that **the VM has no trace-disabled branch in its dispatch
loop at all**. Instructions are not traced — ADR 0019 does not propose an
instruction-level trace and this backend does not record one — so what a trace
costs is paid per Host call and nowhere else. Process wall time, medians of
fifteen:

| configuration                | `arith` | `hostheavy` |
| ---------------------------- | ------: | ----------: |
| neither `--stats` nor a trace | 93.1 ms |     14.2 ms |
| `--stats`                     | 93.3 ms |     16.5 ms |
| `--stats --trace`             | 93.4 ms |     52.8 ms |

That has a consequence for this whole document, and it is worth stating
plainly. **Every `execute=` figure recorded anywhere above — including in the
backend tables this document used to carry — comes from a run with `--stats`,
and `--stats` is not the production configuration.** A run
that asks for neither a trace nor statistics installs a `NullSink` and the
registry then knows that nothing will read a description of a call's values;
`--stats` installs a composite sink instead, and describing every Host call's
arguments and result for a sink that discards them costs `hostheavy` 16.8%.
On the benchmarks the backend tables this document used to carry were made
of, it costs nothing measurable, because `arith`, `field`, `call`, `method`,
`pure`, `chars` and `arrayget` make no Host call between them. `hostheavy` is
the one benchmark whose
`--stats` time should not be read as its production time, and the figure that
should be is the 14.2 ms above.

### A change to `vm.rs` moved a benchmark that cannot execute it

[Issue #179](https://github.com/myuon/cove/issues/179) says that the workspace
has no `[profile.release]`, so release builds with `codegen-units = 16` and no
LTO, and rustc partitions codegen units by module — which makes where code
lives a performance variable independent of what it does. That was reasoned
from the build configuration. It has since been observed directly, and the
instance is worth recording because it is cleaner than the ones that suggested
it.

Measuring [issue #160](https://github.com/myuon/cove/issues/160) meant building
one variant of `Vm`: a private method added to `vm.rs` and called from
`Vm::call_host` and `Vm::call_resource`, and nowhere else. Two `cove-bench
--iterations 15` runs of the unmodified build bracket one of the variant, all
three from the same session on the same machine:

| bench       | base | variant | base again | variant vs base | Host calls |
| ----------- | ---: | ------: | ---------: | --------------: | ---------: |
| `field`     |  432.40 ms | 457.91 ms | 433.63 ms | **+5.9%** | none |
| `method`    |  821.18 ms | 846.96 ms | 813.43 ms | +3.1% | none |
| `arith`     |   88.35 ms |  86.18 ms |  88.10 ms | −2.5% | none |
| `chars`     |  818.09 ms | 828.10 ms | 808.97 ms | +1.2% | none |
| `call`      |  239.51 ms | 240.35 ms | 238.92 ms | +0.4% | none |
| `pure`      |    2.34 ms |   2.32 ms |   2.30 ms | −0.7% | none |
| `hostloop`  |  663.54 ms | 666.07 ms | 643.22 ms | +0.4% | 1,000,000 |
| `hostheavy` |    3.79 ms |   4.07 ms |   3.82 ms | +7.4% | 4,001 |

**`field` is 5.9% slower on a code path it never reaches.** It makes no Host
call, so it never executes the added method, and it runs the same 47,428,595
instructions in all three builds. The two unmodified runs agree with each other
to 0.3%, so the machine did not move under them. What moved is the layout of a
module `field` spends its whole run inside.

The consequence for reading the two host benchmarks is the point. The change
is *about* Host calls, and the only two rows that make any are inside a band
that rows making none demonstrate is at least ±6% wide. So the honest bound on
what the change costs is "less than what adding a function to `vm.rs` costs
benchmarks that cannot call it", and no number smaller than that is available
from this build configuration. `hostloop`'s 1,000,000 Host calls put the
change's own cost at +2.5 ns a call against the two baselines' own 20 ns of
disagreement; `benches/convention`'s `conv_host`, corrected by its `conv_fresh`
control, puts it at +47 ns against a boundary that costs 887. Both are small
and neither is resolved.

This is the fourth time layout has been the answer — [#114](https://github.com/myuon/cove/issues/114)'s
cold match arms, [#126](https://github.com/myuon/cove/issues/126)'s spills, the
calling convention's unattributed 8 ms on `arith`, and now this — and it is the
first where the moved benchmark provably does not run the changed code at all.

### The layout band is much wider than it was thought to be

The section above bounds the band at "at least ±6%", from a build whose added
method `field` never executes. Issue #162's work needed a bound it could state
a design against, so it built the control the earlier measurement could not:
**the base commit, with one `Inst` variant added that is never emitted, never
executed, and reachable from no program.** The variant is matched in
`Vm::execute`'s dispatch group with an `unreachable!` body and in `validate`
with a bound check; nothing else differs from `2c19429`, and every benchmark
runs the same instructions it ran before.

`cove-bench --matrix --iterations 15` and `cove-bench --iterations 15`, base
binary and control binary, interleaved on one machine in one sitting:

| row / bench | base | control | control vs base | instructions |
| --- | ---: | ---: | ---: | ---: |
| `arith` (VM) | 80.53 ms | 99.46 ms | **+23.5%** | identical |
| `conv_var` | 112.64 ms | 125.73 ms | **+11.6%** | identical |
| `conv_local` | 86.32 ms | 91.87 ms | **+6.4%** | identical |
| `chars` (VM) | 566.31 ms | 578.37 ms | +2.1% | identical |
| `conv_host` | 2336.32 ms | 2410.71 ms | +3.2% | identical |
| `field` (VM) | 425.27 ms | 430.13 ms | +1.1% | identical |
| `method` (VM) | 651.16 ms | 653.84 ms | +0.4% | identical |
| `call` (VM) | 154.79 ms | 152.69 ms | −1.4% | identical |
| every AST row | — | — | −0.2% to +3.7% | identical |

**`arith` on the VM is 23.5% slower for a variant no program can reach.** That
is four times the ±6% the section above records and it is on the benchmark
this document has most often read a few percent off. The machine did not move
under it: the AST rows, which share the binary and none of the code, span
−0.2% to +3.7%, and a third run of the base binary agrees with the first two
to 1.4% on `conv_local`.

Three things follow, and they are the reading rules for anything measured on
this workspace until it has a `[profile.release]`:

- **A cross-build absolute is not evidence.** Not for a regression and not for
  an improvement. Two builds that differ by one enum variant differ by 23.5% on
  a benchmark neither of them changed.
- **A within-build ratio is.** Two rows of one matrix run in one binary share
  whatever layout that binary has. `conv_var ÷ conv_local` is 1.30× on the
  base, 1.37× on the control, and 1.00× after ADR 0027 — and the middle column
  is what says the third number is a change and not a build.
- **An instruction count is.** `--stats` counts what ran. #126 proved a count
  is not *sufficient* — three changes with identical counts summed to 19%
  slower — but a count that moved when it should not have, or did not move
  when it should have, is still the cheapest way to catch a mistake, and it is
  the only figure here that no rebuild can touch.

What this does not say is that the band is noise. It is a real cost paid by a
real build; a user running that binary is 23.5% slower on that program. What
it says is that the cost belongs to the *arrangement of the code*, which this
workspace does not control and does not measure, and so cannot be attributed
to a design being compared against another.

### How to read a measurement, now that the harness reports a spread

Everything above — including the backend tables this document used to
carry — was read by eye. `cove-bench` reported `{min, mean, max}`, a
reader compared three numbers against a band held in memory, and the sentence
a refactor wants to write — "no statistically meaningful regression" — was not
a number this repository could produce. [Issue #179](https://github.com/myuon/cove/issues/179)
names that as its third item and it is now done, so the discipline the rest of
this section describes has a tool rather than only a habit.

**Every wall-time series now reports its quartiles and its own samples.** The
`wall_ns` object gained `p25`, `median`, `p75` and `iqr` beside the three
fields it always had, and a `samples` array holding every timing the run took.
The median rather than the mean because a wall-time series has a floor and no
ceiling: the failure mode is a sample that is too *large*, and the mean is the
statistic that moves furthest when one arrives. The interquartile range rather
than `max - min` because the range grows with the sample count — a longer
series has more chances to catch one bad run — and the middle half does not.
`crates/cove-bench/src/stats.rs` argues both at length, including what was
checked about the shape of these distributions and what that check cannot
settle.

**`--baseline <path>` compares this run against a recorded one.** The baseline
format is the harness's own output, so recording one is `cove-bench > file`.
For each row present in both, it prints the shift between the medians and a
95% percentile-bootstrap interval around it, and reads a verdict off the
interval: an interval excluding zero cleared the noise, one containing zero did
not.

That last case is the one this document has needed and been unable to state.
When the interval contains zero, **the interval's width is the honest bound**,
and it is what a "no regression" sentence should quote — not the shift, and
certainly not a claim that the change had no effect. It is the same move the
section above makes in prose when it says the honest bound on issue #160's cost
is "less than what adding a function to `vm.rs` costs benchmarks that cannot
call it"; the difference is that the harness now computes that bound instead of
a reader estimating it.

**Compare against a fixed commit, not the parent.** Unchanged, and the reason
is still #126: three changes each individually inside the noise summed to 19%.
`--baseline` is what makes it cheap — record the suite once on the commit being
measured against, keep the file, and pass it to every run after.

**The ±6% band is per benchmark and the harness now measures it rather than
recalling it.** This section established the band on `arith` by observing
benchmarks move on code they cannot execute, which is why `field`, `pure` and
`call` are the discriminating cases. That band was a remembered number applied
globally. A comparison's interval is derived from the two series being
compared, so a benchmark that is quiet gets a narrow interval and one that is
noisy gets a wide one, without anybody choosing a threshold. What the recorded
±6% is still needed for is the thing no run-to-run spread can see: layout
sensitivity is *systematic* between two builds, not noise within one, so two
tight series can disagree by 6% with both intervals narrow and neither wrong.
**An interval says the difference is real. It does not say the difference is
the change.** That distinction is what the rest of this section is about.

**Six samples is the floor, fifteen is the practice.** `--iterations` is the
sample count — there is no second flag — and below six samples a side the
harness reports the shift and refuses to call it, because no 95%
distribution-free statement about a median exists on fewer. CI still runs
`--iterations 1` and is unaffected: it asserts correctness, never a number, and
a series of one costs it exactly what it cost before.

### What `codegen-units = 1` was measured to be worth, and why the answer is nothing

The two sections above blame layout, and
[issue #179](https://github.com/myuon/cove/issues/179) names the fix its
reasoning implies: give the workspace a profile with `codegen-units = 1`, so
that a crate is one codegen unit and a module boundary stops being a place
rustc can decide to lay code out differently across. Option 2 of that issue
adds it as a *bench-only* profile — `[profile.bench-stable]`, `inherits =
"release"`, `codegen-units = 1`, nothing else, and no LTO, which was always
meant to be a separate measurement — so that `[profile.release]` stays at
Cargo's defaults and CI keeps the 137-second pipeline it was cut to.

It was built, and the control #179 asks for was run under it. **It does not
work, and the round is recorded here because a measured negative result is
worth more than the hypothesis it replaces.**

#### What was run

The same control as the section above: the current commit, built twice, the
second time with **one `Inst` variant added that no lowering emits, no program
reaches, and `Vm::execute` matches only with an `unreachable!` body**. Both
profiles got that pair, so there are four binaries; all four are byte-identical
across a reboot and across `-j 16`, `-j 4` and `-j 2`, so nothing below is
nondeterministic codegen. `fuel_spent` is identical on every row of all four —
every benchmark ran exactly the instructions it ran before.

Six `cove-bench --iterations 15` suites and six `--matrix --iterations 15`
runs, one machine, one sitting, arranged so each variant run is **bracketed**
by a run of its own base binary before and after it. Every figure below is the
variant against the mean of its two brackets, which is the only way to state
one at all — for the reason the next subsection gives.

#### The result

| row | release (`codegen-units = 16`) | bench-stable (`codegen-units = 1`) |
| --- | ---: | ---: |
| `arith` (VM) | **−1.00%** | **−6.01%** |
| `conv_var` | +2.18% | −6.78% |
| `conv_local` | +2.55% | −6.40% |
| `call` (VM) | +1.12% | −5.19% |
| `field` (VM) | −3.51% | −2.79% |
| `method` (VM) | −3.44% | −2.01% |
| `chars` (VM) | +0.65% (AST) | −2.15% |
| `pure` (VM) | −3.87% | −5.70% |
| **largest \|shift\| over 24–27 rows** | **3.87%** | **6.78%** |
| **band width** | **6.42 pp** | **9.53 pp** |

**The spurious shift is larger under `bench-stable`, not smaller.** Where the
default profile spread its 24 rows over 6.4 percentage points, one codegen unit
per crate spread 27 rows over 9.5, and the row #179 leads with — `arith` on the
VM — moved six times further under the profile that was supposed to hold it
still. The shape differs too, and the mechanism is legible: at 16 codegen units
a dead variant perturbs the unit it lands in and leaves the others alone, so the
shifts are small and mixed in sign; at one unit per crate it relays out the
whole crate at once, so nearly every row moves the same way together.

#### The control did not reproduce, and that is the more important finding

The section above records **+23.5% on `arith`** and **+11.6% on `conv_var`**
from exactly this control under exactly this default profile. This round, under
the same profile, the same kind of never-executed `Inst` variant moved `arith`
by **−1.00%** and `conv_var` by **+2.18%**.

So there was no +23.5% here to shrink. That does not make the earlier
measurement wrong — it was taken, and its instruction counts were checked, the
same way this one was. What it means is that **one dead variant is a single
draw from the layout distribution, not a measurement of it.** Two draws of the
same experiment, at different commits, returned +23.5% and −1.00%. A control
built from one perturbation can therefore say that layout sensitivity exists;
it cannot size it, and it cannot be used to score a profile against another,
which is what this round tried to do with it.

#### And the machine moved as much as the code did

Each arm's base binary was run twice, roughly forty minutes apart, with nothing
else on the machine. The same binary disagreed with itself by:

| | release | bench-stable |
| --- | ---: | ---: |
| `pure` (VM) | **−7.40%** | −6.57% |
| `field` (VM) | +4.93% | −1.19% (AST: −5.31%) |
| `method` (VM) | +3.24% | −1.05% |
| `arrayget` (VM) | +2.58% | +3.22% |
| largest \|shift\| | **7.40%** | 6.57% |

**The null is the size of the signal.** A binary compared against itself moved
up to 7.4%, which is more than either arm's variant moved against its bracket.
So neither arm's number above is separable from drift, and the honest statement
about `codegen-units = 1` is not "it is worse" but "**it is not better, and
this machine cannot currently resolve a difference smaller than about 7% between
two runs of anything, including one binary and itself.**"

That is a prerequisite result. Until the same-binary null is brought under a
percent or so, no layout experiment on this workspace can measure a layout
effect, because the thing being measured is smaller than the ruler.

#### What run-to-run spread says, and where the profile does help

Within a single run the harness's own quartiles are small and the two profiles
are the same: **median per-row IQR 1.09% of the median under `bench-stable`,
1.13% under release.** The profile buys nothing there either.

The one place it does help is the fastest rows, where a fixed cost is a larger
fraction of a short run: the **largest** per-row IQR across the timed rows is
**2.12% under `bench-stable` against 11.83% under release**, and `pure` on the
VM — the row that carries most of that — goes from 6.08% to 1.40%. So one
codegen unit does make a short benchmark's series tighter. It just does not
make two *builds* comparable, which is the entire thing #179 wanted.

This is also the clearest illustration of the rule the harness's own comparison
already states: an interval built from within-run samples can be narrow on both
sides and still be measuring a difference that is not the change. Under
`bench-stable`, `arith`/VM's comparison against its base reads
**−6.49% [−7.43, −5.84], "improvement"** — a confident, narrow interval, on a
benchmark whose only difference from its baseline is an enum variant it cannot
reach.

#### What it costs

Both binaries, `-j 4`, this machine:

| | release | bench-stable | |
| --- | ---: | ---: | ---: |
| from scratch, deps included | 34.2 s | 49.2 s | **+44%** |
| rebuild after a `vm.rs` edit | 17.2 s | 33.7 s | **+96%** |
| total CPU, from scratch | 120.3 s | 94.1 s | −22% |

The CPU column is the interesting one: one codegen unit does *less* total work
and still takes longer, because it cannot spread that work across cores. The
penalty is serialization, so it gets worse on a wider machine, not better.

#### The recommendation, and what CI does

**Do not adopt `bench-stable` as the baseline for implementation comparisons.**
It costs 44% to 96% more build time, it does not narrow the cross-build band,
and on this round's evidence it widens it. The profile stays defined so this
measurement can be reproduced and so the next person does not have to build it
again to find out; nothing in the workspace selects it.

`.github/workflows/ci.yml` is untouched and unaffected. It builds `--release`
and runs `cove-bench --iterations 1`, both deliberately, and a profile no step
names costs it nothing.

#### What this changes about how to read a measurement

The rule the section above states — **a cross-build absolute is not evidence on
this workspace** — is not narrowed by any of this. `bench-stable` does not earn
back cross-build absolutes and nothing here suggests a profile that would.

It is **widened**, in a direction that section did not reach. That rule is about
two builds. This round shows the weaker claim fails too: *a same-build absolute
taken forty minutes later is not evidence either*, because one binary moved 7.4%
against itself with nothing changed at all. So:

- **Bracket, do not pair.** A variant run must have a run of its base binary
  before it *and* after it, and the figure quoted is the variant against the
  mean of the two. A single base-then-variant pair cannot tell a change from the
  half-hour that passed between them. Every number in this section is bracketed;
  the sections above — including the backend sections this document used to
  carry — that quote a base run once should be read as the weaker evidence
  they are.
- **Quote the null beside the signal.** The two brackets' disagreement with each
  other is the measurement's own error bar, it costs one extra run, and where it
  is as large as the effect — as it is here — that is the result.
- **One perturbation does not size a band.** +23.5% and −1.00% are the same
  experiment at two commits. Sizing layout sensitivity needs several distinct
  dead variants per profile, interleaved with base runs, not one.

#### If the LTO question is asked later

Thin LTO was deliberately excluded so that two changes would not land in one
measurement, and it should stay excluded until the design above is fixed —
running it now would produce another single-draw number of the kind this round
has just shown is uninterpretable. What it would take: at least five *distinct*
never-executed perturbations per profile, each bracketed by base runs, with the
same-binary null reported per row, and the profiles compared on the **spread of
the perturbations** rather than on any one of them. That is roughly six hours of
wall time per profile on this machine, and the first thing it should establish
is whether the ±7% same-binary drift can be brought down at all — because if it
cannot, the experiment cannot resolve anything smaller and should not be run.

### The ±7% was a maximum over two dozen rows, and a row's own error bar is a quarter of it

[Issue #205](https://github.com/myuon/cove/issues/205) took the number the
section above ends on — one binary disagreeing with itself by 7.4% — and asked
what it is, what it is correlated with, and whether it can be brought down.
The first answer changes how every table above — including the backend
tables this document used to carry — should be read, and it is not about the
machine at all.

#### What was run

Twenty-two `cove-bench --iterations 15` suites in one four-hour sitting,
nothing under test changing between any two of them:

- **Six back to back**, one release binary, no other work on the machine.
- **Six more of that same binary**, each preceded by a real incremental
  rebuild (`touch crates/cove-runtime/src/vm.rs`, then
  `cargo build --release -j 4 -p cove-cli -p cove-bench`, 16 s every time) —
  because the session that produced the 7.4% had builds in it and this one had
  to find out whether that mattered.
- **Ten of a second binary**, alternating the two sample orders the subsection
  after next is about, in the sequence `b r r b b r r b b r` so that neither
  arm sits at one end of the session.

Every suite is bracketed by a **direct measurement of the CPU's effective
clock**: a dependent `addq` chain, which retires at exactly one add per cycle
on this microarchitecture, so five hundred million adds timed give gigahertz
without needing root. Five before each suite and five after, 220 probes in
all. `sysctl vm.loadavg`, `machdep.xcpm.ratio_changes_total` and
`pmset -g therm` were recorded at the same points.

#### The machine, since this document has never said

An **Intel Core i7-10700K**, eight cores and sixteen threads, macOS 25G83,
32 GiB, and `vm.swapusage` total `0.00M`. `machdep.xcpm.hard_plimit_max_100mhz_ratio`
is 51 against a 3.8 GHz base, so the hardware is free to move between 0.8 and
5.1 GHz and turbo alone could account for far more than 7%. It did not.

| the clock probe, 220 samples over four hours | GHz |
| ------------------------------------------- | ---: |
| 1st percentile                               | 4.6162 |
| 25th                                         | 4.6730 |
| median                                       | 4.6808 |
| 75th                                         | 4.7053 |
| 99th                                         | 4.7450 |
| the single worst probe                       | 4.5069 |

**The middle half of the machine's clock spans 0.7%**, and `pmset -g therm`
reported `CPU_Speed_Limit` of 100 at every one of its 44 snapshots — no
thermal and no scheduler limit, ever. `powermetrics` needs root and was not
run, so there is **no package temperature and no per-core residency figure
here**; that is a gap in this measurement and is stated rather than guessed
at. What the probe does establish is that whatever moves the benchmarks, the
core they run on is not changing speed by anything like the amount the
benchmarks move.

#### What a row's disagreement with itself actually is

Twelve suites of one binary, 66 pairs per row, the shift between two runs'
medians:

| row | median | 90th | worst |
| --- | -----: | ---: | ----: |
| `arith` (VM)      | 0.25% | 0.53% |  0.84% |
| `field` (VM)      | 0.35% | 0.92% |  1.10% |
| `arrayget` (VM)   | 0.48% | 1.07% |  1.41% |
| `call` (VM)       | 0.55% | 1.79% |  2.72% |
| `chars` (VM)      | 0.78% | 1.42% |  2.42% |
| `pure` (VM)       | 0.84% | 2.43% |  3.20% |
| `arith` (AST)     | 1.20% | 2.58% |  3.29% |
| `hostheavy` (VM)  | 1.41% | 2.97% |  4.26% |
| `field` (AST)     | 1.65% | 3.35% |  4.70% |
| `startup` (VM)    | 1.66% | 3.85% |  5.63% |
| `startup` (AST)   | 2.18% | 5.13% |  6.97% |
| `benches` lowering| 2.28% | 9.29% | 12.01% |
| **all 21 rows pooled** | **0.78%** | **2.58%** | 12.01% |
| **without the lowering and the two `startup` rows** | **0.71%** | **2.18%** | 5.78% |

That is the number this repository did not have. **A row's honest error bar on
this machine is about 0.8% in the middle and 2.5% at the 90th percentile**,
and the quietest row in the suite, `arith` on the VM, never moved by more than
0.84% in 66 comparisons of one binary with itself.

#### It is not the gap between the runs, and it is not the builds

The 7.4% was framed as drift "over forty minutes". It is not.

| gap between the two suites | median | 90th | 99th | worst |
| --- | ---: | ---: | ---: | ---: |
| under 15 minutes  | 0.74% | 2.67% | 5.47% |  9.01% |
| 15 to 45 minutes  | 0.79% | 2.47% | 7.05% | 11.40% |
| over 45 minutes   | 0.78% | 2.66% | 6.05% | 12.01% |

**The three distributions are the same one.** Two suites nine minutes apart
disagree exactly as much as two suites two hours apart, so nothing here is
accumulating with time — not heat, not page-cache state, not uptime. The six
suites with a 16-second `cargo build -j 4` in front of them are not
distinguishable from the six without, either. Whatever this is, it is present
between any two runs and does not grow.

#### The 7.4% was `max over rows`, which is a different statistic

Take the same twelve suites of one binary and compute, for each of the 66
pairs, *the largest shift over the suite's rows* — which is what "disagreed
with itself by up to 7.40%" reports:

| statistic | over all 21 rows | over the 18 execution rows |
| --- | ---: | ---: |
| median | 3.99% | 2.89% |
| 90th percentile | 9.29% | 4.23% |
| worst | 12.01% | 5.78% |
| pairs reaching 7.4% or more | **14%** | **none** |

**7.4% is an ordinary draw from this null.** A maximum over two dozen rows is
a maximum of two dozen samples of a heavy-tailed thing, and its median is five
times the median of any one row. Nothing was wrong with the observation; what
was wrong was reading a suite-wide maximum as a per-row error bar. **It is
not, and no row should be compared against it.**

Which rows carry that maximum is the other half of the answer:

| row | how often it is the largest shift in a null pair |
| --- | ---: |
| `benches` lowering        | 36% |
| `startup` (AST)           | 20% |
| `startup` (VM)            | 12% |
| `hostheavy` (both)        | 14% |
| `field` (AST)             |  8% |
| `callback` (AST)          |  5% |
| the remaining fifteen rows |  6% between them |

The two worst are the two that are not really benchmarks of the runtime's
steady state. The lowering row times a **0.13 ms** operation, so fifteen
samples of it are two milliseconds of measurement; `startup` spawns a process
and pays whatever the operating system charges for that, and its 99th
percentile sample is **eighty times its median** — the page-cache effect ADR
0012 warned about, still there. **Neither is evidence of anything at the
few-percent level and neither ever was.**

#### The shape of a series, which `stats.rs` asked for and could not have

`crates/cove-bench/src/stats.rs` argues for the median from the shape of the
failure and then says, honestly, that the skew argument was not supported by
the only data available — three order statistics from nine-sample series. It
asks whoever next takes a run on a quiet machine to look at the real shape.
Ninety samples a row, pooled over six suites, each expressed against its own
row's median:

| row | 1st | 25th | 75th | 99th | worst |
| --- | --: | ---: | ---: | ---: | ----: |
| `field` (VM)    | −0.86% | −0.33% | +0.34% |  +2.77% |    +2.8% |
| `arith` (VM)    | −1.23% | −0.42% | +0.29% | +34.00% |     +34% |
| `pure` (VM)     | −5.21% | −0.97% | +1.73% | +15.89% |     +16% |
| `startup` (AST) | −7.40% | −1.63% | +18.9% |  +8131% |  +8,131% |

**It is a floor with a long right tail, and the tail is much longer than the
body.** The middle half of a good row spans less than a percent while its
worst sample is tens of percent above the median. So the median was the right
choice for the reason `stats.rs` gives — a decision must not move when one
sample arrives late — and the argument from skew, which that file declined to
rely on, turns out to hold after all. The interquartile range was the right
spread for the same reason: on `arith`/VM, `max − min` is 35% of the median
and the IQR is 0.7%.

#### Bracketing helps, and it helps less than it sounds like it should

The rule the section above adopted — base, variant, base again, quote the
variant against the mean of the two — was never measured. Over consecutive
triples of the twelve suites:

| what is quoted | median error | 90th | worst |
| --- | ---: | ---: | ---: |
| the pair, `B` against one `A` | 0.74% | 2.51% | 9.58% |
| the bracket, `B` against the mean of two `A`s | 0.64% | 2.06% | 9.74% |
| the bracket's own null, the two `A`s against each other | 0.71% | 2.43% | 11.40% |

**Averaging two base runs takes about 15% off the error**, which is roughly
what averaging two draws of anything does, and it does nothing at all to the
worst case. The value of the rule is the third row, not the second: the
bracket's real product is *an error bar that was measured in the same session
as the result*, and that is worth the extra run whatever the average does.

#### What was tried: taking the samples in a different order

`cove-bench` took every sample of one row before starting the next. So a row's
whole series was taken at one instant of a nine-and-a-half-minute suite, and
for the fastest rows that instant is very short indeed — fifteen samples of
`pure` on the VM are twenty milliseconds of measurement, and fifteen of the
lowering are two. Whatever the machine was doing then is the whole of the
row's answer, and nothing in the row's own spread can say so.

`--sample-order round-robin`, now the default, takes one sample of every row
per pass instead. The same rows run the same number of times, the suite takes
the same 564 seconds, and only *when* each sample is taken changes. Five
suites of each order, alternating, one binary:

| over the 18 rows the order governs | blocked | round-robin |
| --- | ---: | ---: |
| median disagreement between two suites | 0.61% | **0.45%** |
| 90th percentile | 1.97% | **1.67%** |
| worst | 4.40% | **3.62%** |
| rows that improved | — | **13 of 18** |

**A quarter of the noise, for nothing.** That is the honest size of it: it is
not a fix, and the sign test on thirteen of eighteen is not overwhelming
either. It is the default because it costs no time, no work, and no output
format — and because it removes a structural embarrassment rather than a
number, namely that a row could take its entire answer from one instant it
could not report.

Two things say not to claim more than that.

**The rows the flag cannot touch also "improved", and they cannot have.** The
lowering row and both `startup` rows are measured outside the loop the order
governs, and their null still came out 2× to 4× smaller in the round-robin
arm. That is two outlier suites landing in the other arm by luck — the
lowering read +8.66% and +6.91% in two blocked suites, `startup`/AST read
+15.37% in one. **Five suites an arm cannot resolve a heavy-tailed row**, so
the all-rows figure (0.80% → 0.51%) overstates what the change did, and the
eighteen-row figure is the one to read.

**The two orders report the same numbers, as far as this can tell.** The
median absolute difference between an arm's row medians is 0.73% and the worst
is 2.77% — the same size as the null itself, so there is no evidence that
round-robin measures anything different. A baseline recorded under one order
can be compared against a run under the other; it just has one more source of
disagreement in it than a same-order comparison does.

One detail is worth reading the other way round. `pure` on the VM went from a
within-run IQR of 1.86% to 2.87% while its *between*-run disagreement halved.
The series got wider and the answer got steadier, which is exactly what should
happen: a series spread over the suite starts including the variation a series
taken at one instant was blind to. **The wider interval is the more honest
one.**

#### The rule, narrowed

The section above says a same-build absolute taken forty minutes later is not
evidence. That was too strong, and it was too strong in a specific way: it
generalised a maximum over rows into a bound on every row. What this round
supports:

- **A row's error bar is about 0.8% at the median and 2.5% at the 90th
  percentile on this machine**, per row, per pair of runs. Not 7%. A
  difference of 3% on a single execution row, seen twice, is outside the null;
  the earlier rule would have thrown it away.
- **Never quote the suite's largest shift as an error bar.** Its median on a
  null is 4% and it reaches 15%. Quote the row.
- **The lowering row and both `startup` rows are not evidence** at anything
  under about 10%. They carry two thirds of every null maximum.
- **Bracket anyway.** Not because averaging the two base runs is worth much —
  it is worth 15% — but because the two base runs' disagreement is the only
  error bar measured in the same conditions as the result.
- **Time between runs is not a variable**, and neither is an incremental build
  between them. Both were measured and neither is.
- **This is the floor, and it is close.** The machine's own clock holds to
  0.7% through its middle half, `arith`/VM's null is 0.25%, and the remaining
  rows sit between that and the machine. There is no large remedy left to
  find here; what is left is arithmetic — more samples, more perturbations —
  and the reason to want it is layout, which is a property of the builds and
  not of the machine.

`--sample-order blocked` reproduces every "blocked" figure above, and nothing
selects it otherwise.
