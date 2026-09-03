//! `cove-bench`: the interpreter performance-gate harness for ADR 0012.
//!
//! This is not a `cove` subcommand and makes no promise of a stable CLI
//! surface. It loads the package under `benches/` and runs each benchmark's
//! entry directly against `cove-runtime` -- the same crate `cove run` and
//! `cove test` are built on -- so it measures the interpreter itself rather
//! than shelling out and re-paying parse and resolve on every sample.
//!
//! Every Host a benchmark reaches is the same deterministic fake `cove test`
//! already grants by default (see `crates/cove-cli/src/test.rs`): a
//! `console` that writes into a sink nobody reads, a `clock` whose
//! `VirtualTime` never advances on its own, and every other host answering
//! from empty in-memory state. Nothing here touches the network or the real
//! filesystem, which is what keeps it hermetic and non-flaky.
//!
//! The one exception is the `startup` benchmark, which measures a real
//! process: it spawns the `cove` binary built alongside this one and times
//! the whole exec-to-exit span, because process creation and binary loading
//! are exactly what an in-process measurement cannot see.
//!
//! # Four backends
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) says every
//! number this harness reports must say which backend produced it, and
//! [issue #111](https://github.com/myuon/cove/issues/111) gates the VM's
//! adoption on the comparison. So every measurement below carries a
//! `backend`, and every benchmark is measured on all of them.
//!
//! The VM cannot run every construct yet. Where it cannot, the benchmark says
//! so and names what stopped it rather than going missing: a benchmark absent
//! from a report reads as one nobody ran, and the two are not the same fact.
//!
//! The third is `frame`, the experimental eight-byte-word frame
//! [issue #212](https://github.com/myuon/cove/issues/212) asks for, and it is
//! here rather than in a build of its own for the reason that issue insists
//! on: **the comparison has to be made within one benchmark binary.** ADR
//! 0029 is why — "a ratio within one build is repeatable", and a cross-build
//! absolute on this workspace is not, because a dead `Inst` variant that no
//! program can reach moved `arith` by +23.5% in one build and by −1.00% in
//! another. Two rows of one run share whatever layout that binary has, so the
//! `vm` and `frame` medians of one benchmark are a comparison and two builds'
//! absolutes are not.
//!
//! It runs a closed scalar subset — see `cove_runtime::frame` — so most rows
//! have no `frame` line and say so by name instead. **A frame refusal does
//! not fail the suite.** Refusing six of nine rows is the experiment working:
//! it is a measurement vehicle rather than a third permanent evaluator, and
//! `cove run` cannot select it.
//!
//! The fourth is `lvm`, the linear-memory backend
//! [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md) decides, and
//! it is here for the same reason and one more. The reason is ADR 0029's:
//! its completion condition 9 — *"representative performance gates show no
//! order-of-magnitude or repeated multi-fold regression"* — is a ratio
//! against the backend it replaces, and a ratio is evidence only inside one
//! build. The one more is that ADR 0034 makes it the *production* path once
//! the replacement lands, so unlike `frame` it is not an experiment with an
//! admission predicate: `cove_lir` refuses nothing on purpose, and a
//! benchmark it cannot lower is a gap in the replacement rather than a
//! program a subset declines. **An `lvm` refusal fails the suite**, where a
//! `frame` refusal does not.
//!
//! `cove_ir::lower` lowers a whole package and refuses all of it for one
//! construct anywhere in it, which is exactly what `cove run --backend vm`
//! does, so that is what is measured here: one lowering of `benches/`, timed
//! apart from every execution, because lowering happens once per program and
//! execution happens for as long as the program does. That separation is the
//! compile/lower breakdown #111 asks for.
//!
//! `cove_lir::lower_entry` slices by reachability instead, which is what
//! `cove run --backend lvm` does, so the `lvm` lowering is timed once **per
//! benchmark** and reported under that benchmark's name rather than under
//! `benches`. The two lowering figures are a slice against a package and are
//! not each other's baseline.
//!
//! # Output
//!
//! One JSON object per line on stdout, in the order the benchmarks are
//! listed here — which is not necessarily the order they were timed in; see
//! `--sample-order` below:
//!
//! ```text
//! {"benchmark":"benches","kind":"lowering","backend":"vm","iterations":<u32>,"wall_ns":<series>,"functions":<usize>,"ok":<bool>}
//! {"benchmark":"pure","kind":"lowering","backend":"lvm", ...the same fields...}
//! ... and one `lvm` lowering line for each of the other benchmarks
//! {"benchmark":"pure","kind":"interpreter","backend":"ast","iterations":<u32>,"wall_ns":<series>,"fuel_spent":<u64>,"fuel_per_sec":<f64>,"heap_peak_bytes":<summary>,"host_calls":<u64>,"irreversible_writes":<u64>,"instructions":<u64|null>,"ok":<bool>}
//! {"benchmark":"pure","kind":"vm","backend":"vm", ...the same fields...}
//! {"benchmark":"pure","kind":"lvm","backend":"lvm", ...the same fields...}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"ast","untraced_wall_ns":<u64>,"traced_wall_ns":<u64>,"overhead_ratio":<f64>}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"vm", ...the same fields...}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"lvm", ...the same fields...}
//! {"benchmark":"hostheavy", ...the same lines...}
//! ... and the same for each of `arith`, `arrayget`, `field`, `method`,
//! `call`, `chars`, and `callback`
//! {"benchmark":"startup","kind":"process","backend":"ast","iterations":<u32>,"wall_ns":<series>,"ok":<bool>}
//! {"benchmark":"startup","kind":"process","backend":"vm", ...the same fields...}
//! {"benchmark":"startup","kind":"process","backend":"lvm", ...the same fields...}
//! ```
//!
//! `instructions` is how many instructions the run executed, and `null` on
//! the interpreter, which has none. It is beside the wall time because ADR
//! 0029 makes an exact count repeatable where an absolute is not, and ADR
//! 0034 asks for instruction counts by name when a gate is argued about: a
//! backend that is slower per instruction and one that runs more of them are
//! two different findings, and the ratio alone cannot tell them apart.
//!
//! where `<summary>` and `<series>` are
//!
//! ```text
//! <summary> = {"min":<u64>,"mean":<u64>,"max":<u64>,"p25":<f64>,"median":<f64>,"p75":<f64>,"iqr":<f64>}
//! <series>  = {...the same fields...,"samples":[<u64>, ...]}
//! ```
//!
//! `min`, `mean` and `max` are the three ADR 0012 named and they still mean
//! what they meant. The quartiles are what
//! [issue #179](https://github.com/myuon/cove/issues/179) asks for: a spread
//! a regression claim can be stated against instead of a band the reader is
//! expected to remember. `crates/cove-bench/src/stats.rs` says why the median
//! and the interquartile range rather than the mean and a standard deviation.
//!
//! `samples` is every timing the run took, on the wall-time series alone,
//! **in the order the run took them**. It is what turns a recorded run into a
//! baseline: a summary can only be compared against another summary by
//! arithmetic that invents the spread it needs, and the samples do not have to
//! be invented. The order is what says *when* a slow sample arrived, which is
//! the difference between a machine that drifted through a series and a
//! benchmark that is noisy in it; nothing that compares two runs reads it,
//! because every statistic here is an order statistic.
//!
//! A benchmark the lowering refuses reports that instead of its `vm` lines,
//! and a benchmark the eight-byte frame refuses reports the same shape:
//!
//! ```text
//! {"benchmark":"pure","kind":"unsupported","backend":"vm","what":"<the construct>","ok":false}
//! {"benchmark":"field","kind":"unsupported","backend":"frame","what":"<the construct>","ok":false}
//! {"benchmark":"chars","kind":"unsupported","backend":"lvm","what":"<what the lowering said>","ok":false}
//! ```
//!
//! The first fails the suite and the second does not: one means no backend
//! but the interpreter can run the benchmark at all, and the other means an
//! experiment's subset has not reached it yet. The third fails it, and is a
//! third fact again: the replacement has no subset to fall short of, so a
//! benchmark it cannot lower is a gap in the backend ADR 0034 makes the
//! production one.
//!
//! `kind` keeps the value it has always had for the interpreter's rows, so a
//! reader of the older format still finds exactly the rows it was reading and
//! does not silently start counting the others. `backend` is what now says
//! which of the four produced a number.
//!
//! `ok` is `false` when a benchmark's entry returned `Err`, a backend itself
//! failed, the lowering was refused, or (for `startup`) the spawned process
//! exited non-zero. A caller comparing two backends, or either against a
//! recorded baseline, should refuse numbers from a run that is not `ok`; this
//! harness's own `--baseline` does, and compares no row that is not `ok`.
//!
//! # Comparing against a recorded run
//!
//! `--baseline <path>` reads a file of the output above and adds one line per
//! row it recognizes:
//!
//! ```text
//! {"benchmark":"field","kind":"comparison","of":"vm","backend":"vm","baseline_median_ns":<f64>,"median_ns":<f64>,"delta_pct":<f64>,"ci_low_pct":<f64|null>,"ci_high_pct":<f64|null>,"confidence":0.95,"verdict":"<verdict>"}
//! ```
//!
//! `kind` is `comparison` rather than the kind of the row compared, again so
//! that a reader filtering on `kind` keeps finding what it was finding; `of`
//! is the kind this line is about. The verdict is one of `regression`,
//! `improvement`, `inside the noise`, or `underpowered`, and it is read off
//! the interval: an interval that excludes zero cleared the noise and one
//! that contains it did not. A summary of the whole comparison goes to
//! stderr, so stdout stays one JSON object per line.
//!
//! **The baseline is a fixed commit, not the parent.** That is the discipline
//! [issue #126](https://github.com/myuon/cove/issues/126) exists to enforce:
//! three changes each individually inside the noise summed to a 19%
//! regression, and only a comparison against a commit far enough back could
//! have seen it.
//!
//! ```text
//! git worktree add /tmp/base <the fixed commit>
//! cargo build --release -p cove-cli -p cove-bench   # in /tmp/base
//! /tmp/base/target/release/cove-bench --iterations 15 > /tmp/base.jsonl
//! cargo build --release -p cove-cli -p cove-bench   # here
//! ./target/release/cove-bench --iterations 15 --baseline /tmp/base.jsonl
//! ```
//!
//! **Bracket the variant, do not pair it.** Run the base binary, then the
//! variant, then the base binary *again*, and quote the variant against the
//! mean of the two base runs. The two base runs' disagreement with each other
//! is the measurement's own error bar, it costs one extra run, and it should
//! be quoted beside the result -- where it is as large as the effect, that is
//! the result.
//!
//! **Compare one row against itself. Never the largest row of the suite.**
//! Twenty-two suites in which nothing under test changed, measured for
//! [issue #205](https://github.com/myuon/cove/issues/205), put a single row's
//! disagreement with itself at 0.5% to 0.8% in the middle and 2% to 3% at the
//! 90th percentile. The *largest* disagreement over the suite's twenty-one
//! rows is a different statistic with a different distribution: on that same
//! null its median is about 4% and it reaches 15%.
//! `docs/VM_ARCHITECTURE.md`'s earlier "7.4% against itself" was that
//! statistic, so it is not the error bar for any row and no row should be read
//! against it.
//!
//! **Two rows are not evidence at the few-percent level, and never were.**
//! `benches`/`lowering` times a 0.13 ms lowering and `startup` times a
//! spawned process; between them they carry the suite's largest null shift
//! two thirds of the time. Read the execution rows.
//!
//! Nothing about this makes a comparison across two machines, two build
//! profiles, or two busy afternoons meaningful. It compares the samples it is
//! given; whether they were taken on a quiet machine is the reader's to
//! answer, and it is the assumption every table in
//! `docs/VM_ARCHITECTURE.md` rests on.
//!
//! A regression verdict does not fail the process. ADR 0012's argument for
//! gating no wall-clock number in CI is unaffected by this: the exit code
//! still reports correctness alone.
//!
//! # The mechanism benchmarks
//!
//! `pure`, `hostheavy`, and `startup` are ADR 0012's, and measure a program.
//! `arith`, `arrayget`, `field`, `method`, `call`, and `chars` are issue
//! #104's, and measure one mechanism each: every one of them is the same
//! 2,000,000-iteration loop with exactly one thing added, so the difference
//! between two of them is what that thing costs. `arith` is the loop alone;
//! `arrayget` adds an indexed read and the `Option` it answers; `field` adds
//! a struct field; `method` adds a call around that field; `call` adds a call
//! with no receiver; and `chars` is the per-character scan `examples/cq`
//! spends nearly all of its time in.
//!
//! `callback` is issue #193's, and belongs to the same family: 2,000,000
//! invocations again, but of a closure reached through a higher-order
//! builtin — `filter`, over an array, with the callback a helper builds over
//! one capture. It is read beside `call`, which makes the same number of
//! entries into a body through the call instruction instead, so the
//! difference between the two is what re-entering the evaluator from inside
//! a builtin costs. That route had no row before, which is why the per-call
//! argument vector #184 removed from the builtin path survived on the
//! callback path with nothing to price it.
//!
//! They exist because a wall-clock number for a whole program says how slow
//! it is and not what is slow about it. They do not replace the application
//! measurement in `examples/cq/README.md`; they are what makes it readable.
//!
//! This tool asserts no thresholds of its own; see ADR 0012 for why wall-clock
//! numbers are not gated in CI, and for the thresholds a human applies when
//! reading a `--iterations`-heavy local run.
//!
//! # Running it
//!
//! ```text
//! cargo build --release --workspace
//! ./target/release/cove-bench --iterations 1      # what CI runs, for correctness
//! ./target/release/cove-bench --iterations 15     # a real local measurement
//! ./target/release/cove-bench --iterations 15 --sample-order blocked
//! ./target/release/cove-bench --matrix --backend vm,lvm --iterations 9
//! ```
//!
//! **`--release` is the profile to measure under.** The workspace also defines
//! `[profile.bench-stable]`, which is `release` with `codegen-units = 1`, and
//! it is *not* the one to reach for: it was added to test whether one codegen
//! unit per crate would stop module boundaries being a performance variable
//! ([issue #179](https://github.com/myuon/cove/issues/179)), it was measured
//! against a never-executed `Inst` variant, and it did not -- the spurious
//! shift came out larger under it than under plain `release`, for 44% to 96%
//! more build time. `docs/VM_ARCHITECTURE.md`, "What `codegen-units = 1` was
//! measured to be worth", is the round. It stays defined so that result can be
//! reproduced; nothing selects it.
//!
//! Optimized in both cases. The benchmarks are sized to be measurable in an
//! optimized build, so an unoptimized one does not run them uniformly slower
//! in some way that could be divided back out — it runs them for minutes.
//!
//! **`--iterations` is how many samples a benchmark's series has**, and there
//! is deliberately no second flag beside it: the runs the spread is computed
//! over and the runs the harness performs are the same runs. So a run at
//! `--iterations 1` reports a series of one, whose median is its only sample
//! and whose interquartile range is zero — which is exactly what CI wants and
//! costs it nothing, and is why it stays at one. Six is the fewest samples
//! any comparison here will draw a conclusion from, and
//! `docs/VM_ARCHITECTURE.md` takes its tables at fifteen.
//!
//! **`--sample-order` is when those samples are taken**, and the default is
//! `round-robin`: one sample of every row, then a second of every row, so
//! that each row's series is spread over the whole suite rather than taken at
//! one instant of it. `blocked` is the order this harness used before
//! [issue #205](https://github.com/myuon/cove/issues/205) — every sample of a
//! row before the next row starts — and it is kept so the round that changed
//! the default can be reproduced. Neither costs more than the other: the
//! suite takes the same 564 seconds, runs the same runs, and reports the same
//! fields. At `--iterations 1` the two are the same sequence, so CI is
//! unaffected.
//!
//! Reading one backend against another is what the output is arranged for:
//! the `wall_ns` medians of one benchmark are the comparison, and the
//! `fuel_spent` beside them is not, because ADR 0019 makes fuel
//! backend-specific and says so. Nor is `instructions`, across an IR
//! boundary: `vm` and `lvm` count instructions of two different instruction
//! sets, so a `lvm`/`vm` count ratio prices *the same program in two
//! languages* and is a decomposition of the wall-clock ratio rather than a
//! second measurement of it. What it is good for is dividing one into the
//! other: nanoseconds per instruction is a number about a dispatch loop.
//!
//! The `vm` and `frame` rows are the one pair where `fuel_spent` *is*
//! comparable, and that is a fact about them rather than an exception to ADR
//! 0019. Both charge one fuel per instruction of the same lowered code by the
//! same block extents, and neither charges anything proportional in the
//! subset the frame admits — no string comparison, no collection copy — so
//! the two figures are the same dynamic instruction count. They are equal on
//! `arith`, `call` and `pure`, and a run in which they are not is a run in
//! which one of the two did different work.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::frame::FrameVm;
use cove_runtime::interp::Interpreter;
use cove_runtime::vm::Vm;
use cove_runtime::{
    Budget, Cancellation, Clock, Console, Database, Documents, Env, Files, Grants, HeapStats,
    HostRegistry, JsonlSink, Limits, Lvm, NullSink, Process, ProcessLog, RecordingBackend, Runtime,
    TraceHeader, TraceSink, Value, ValueCapture, VirtualTime,
};
use cove_sema::package::Package;
use cove_sema::resolve::Program;
use cove_sema::HostSchemas;

mod stats;

use stats::{Baseline, Comparison, Stats, Verdict};

/// How many times each benchmark runs when `--iterations` is not given.
///
/// This claimed the whole harness finishes in well under a second, and it
/// was not true of any run anyone made: CI asked for three iterations of an
/// unoptimized build and spent 82% of its pipeline waiting. No count fixes
/// that, because the benchmarks are sized for an optimized build and three
/// iterations of one without optimization take minutes. So CI builds the
/// harness optimized and asks for one, and this default is what a local
/// reader gets who wants a first look rather than a measurement. ADR 0012
/// says why no number here is gated.
const DEFAULT_ITERATIONS: u32 = 5;

/// The benchmarks the suite runs, in the order their rows are reported.
const BENCHMARKS: [&str; 9] = [
    "pure",
    "hostheavy",
    "arith",
    "arrayget",
    "field",
    "method",
    "call",
    "chars",
    "callback",
];

/// The order the suite takes its samples in.
///
/// This changes nothing about *what* is measured -- the same rows run the
/// same number of times either way, each row's report is the same shape, and
/// a whole suite takes the same 564 seconds under either -- only *when* each
/// sample is taken. `docs/VM_ARCHITECTURE.md`, "What the measurement itself
/// costs", is the round that measured which one to prefer, and by how little.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SampleOrder {
    /// Every sample of one row, then every sample of the next.
    ///
    /// The order this harness used until [issue
    /// #205](https://github.com/myuon/cove/issues/205), and the reason a short
    /// row was the least reliable thing in the suite: `pure` on the VM runs in
    /// about 1.4 ms, so fifteen samples of it are twenty milliseconds of
    /// measurement taken at one instant of a suite that lasts nine and a half
    /// minutes. Whatever the machine was doing in that instant is the whole of
    /// that row's answer, and nothing in the row's own spread can say so.
    ///
    /// Kept because the round that replaced it was run against it, and a
    /// result nobody can reproduce is a result nobody can check.
    Blocked,
    /// One sample of every row, then a second of every row, and so on.
    ///
    /// The same total work, rearranged so that each row's series is spread
    /// over the whole suite instead of one instant of it. A machine that
    /// drifts over the suite then drifts *through* every row's series rather
    /// than between one row's series and another's, so the median of a series
    /// is a summary of the session rather than of a moment in it, and the
    /// interquartile range beside it starts including the drift instead of
    /// being blind to it.
    RoundRobin,
}

/// The order a run uses when `--sample-order` is not given.
///
/// Round-robin, on this evidence: five suites an arm, interleaved on one
/// machine, one unmodified binary against itself. Over the eighteen rows the
/// order actually governs, the median disagreement between two suites fell
/// from 0.61% to 0.45% and its 90th percentile from 1.97% to 1.67%, thirteen
/// of eighteen rows improved, and the suite took the same time. That is a
/// quarter of the noise and not a fix; it is the default because it costs
/// nothing, not because it settles anything.
///
/// A run at `--iterations 1` -- which is what CI does -- takes exactly the
/// same samples in exactly the same sequence under either order, because one
/// pass over the rows *is* one sample of each.
const DEFAULT_SAMPLE_ORDER: SampleOrder = SampleOrder::RoundRobin;

fn main() -> ExitCode {
    // The benchmarks run Cove entries, so they run on the stack the runtime
    // sizes for that, the same as `cove run` does. Measuring an interpreter
    // on a stack it would not be given is measuring something else.
    match cove_runtime::on_cove_stack(bench) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cove-bench: could not start the thread the benchmarks run on: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs every benchmark and reports each one as a line of JSON.
fn bench() -> ExitCode {
    let iterations = parse_iterations();
    let order = match parse_sample_order() {
        Ok(order) => order,
        Err(message) => {
            eprintln!("cove-bench: {message}");
            return ExitCode::FAILURE;
        }
    };

    // Read before anything is measured, so a baseline that does not exist or
    // that a build too old to record its samples produced is a failure before
    // the machine has spent minutes on a run nobody can read.
    let baseline = match load_baseline() {
        Ok(baseline) => baseline,
        Err(message) => {
            eprintln!("cove-bench: {message}");
            return ExitCode::FAILURE;
        }
    };

    let (sources, package, program) = match load_benches() {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("cove-bench: {message}");
            return ExitCode::FAILURE;
        }
    };
    let sources = Arc::new(sources);
    let program = Arc::new(program);

    if std::env::args().any(|argument| argument == "--matrix") {
        if baseline.is_some() {
            eprintln!(
                "cove-bench: `--baseline` compares the benchmark suite, not `--matrix`; ignoring it"
            );
        }
        return matrix(&package, &program, &sources, iterations, order);
    }

    let mut ok = true;
    let mut compared: Vec<Compared> = Vec::new();

    // One lowering of the whole package, timed on its own, because that is
    // both what `cove run --backend vm` does and the only honest place to put
    // a cost that is paid once per program rather than once per run.
    let lowered = bench_lowering(&program, iterations);
    match &lowered {
        Ok(report) => {
            println!("{}", report.to_json());
            compare(
                baseline.as_ref(),
                &mut compared,
                "benches",
                "lowering",
                "vm",
                &report.wall_ns,
            );
        }
        Err(why) => eprintln!("cove-bench: the VM cannot run `benches/`: {why}"),
    }

    // One linear-memory lowering per benchmark, for the reason there is one
    // `cove_ir` lowering of the package: `cove_lir::lower_entry` lowers what
    // one entry reaches, which is what `cove run --backend lvm` lowers, and
    // lowering is paid once per program rather than once per run. Every one
    // of them happens before any execution is timed.
    let mut linear: Vec<LinearLowering> = Vec::new();
    for name in BENCHMARKS {
        let (module, entry) = match entry_for(&package, &program, name) {
            Ok((module, entry, _)) => (module, entry),
            // Reported once, by the loop below that resolves every row of
            // every backend against the same lookup.
            Err(_) => continue,
        };
        match bench_linear_lowering(&program, &sources, name, module, entry, iterations) {
            Ok(report) => {
                println!("{}", report.to_json());
                compare(
                    baseline.as_ref(),
                    &mut compared,
                    name,
                    "lowering",
                    "lvm",
                    &report.wall_ns,
                );
                linear.push(report);
            }
            Err(diagnostics) => {
                let what = diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| "the lowering failed and said nothing".to_string());
                println!(
                    "{}",
                    NotLowered {
                        benchmark: name,
                        what: &what
                    }
                    .to_json()
                );
                eprintln!("cove-bench: `benches/{name}` does not lower to the linear IR: {what}");
                // A gap in the backend ADR 0034 makes the production one,
                // and not an experiment declining a program: it fails.
                ok = false;
            }
        }
    }

    // Every row this run will time, resolved before any of them is timed.
    // An entry that does not exist, or a benchmark the lowering refused, is
    // a fact about the suite rather than about the machine, and finding it
    // out halfway through would put an error message inside somebody's
    // series.
    let mut rows: Vec<Row> = Vec::new();
    for name in BENCHMARKS {
        for backend in [Backend::Ast, Backend::Vm, Backend::Frame, Backend::Lvm] {
            // The linear-memory program this row runs, and `None` for every
            // other backend. An `lvm` row whose lowering failed was already
            // reported above, so it is skipped here rather than reported
            // twice.
            let lir = match backend {
                Backend::Lvm => {
                    let Some(lowering) = linear.iter().find(|lowering| lowering.benchmark == name)
                    else {
                        continue;
                    };
                    Some(&lowering.program)
                }
                _ => None,
            };
            // A benchmark the lowering refused is reported as refused rather
            // than skipped: a missing row reads as a benchmark nobody ran,
            // and that is a different fact from one the VM cannot run.
            let ir = match (backend, &lowered) {
                // Neither of these reads `cove_ir`: the interpreter runs the
                // checked program and `lvm` runs its own lowering, so a
                // refusal over there is not their row's business.
                (Backend::Ast, _) | (Backend::Lvm, _) => None,
                (_, Ok(report)) => Some(&report.ir),
                (_, Err(why)) => {
                    println!("{}", Unsupported::new(name, why).to_json());
                    // A lowering failure is the VM's row missing and the
                    // frame's row missing for one reason, so it fails the
                    // suite once rather than twice.
                    ok &= backend != Backend::Vm;
                    continue;
                }
            };
            match Row::resolve(&package, &program, name, backend, ir, lir) {
                Ok(row) => rows.push(row),
                Err(message) => {
                    eprintln!("cove-bench: benchmark `{name}` on {backend}: {message}");
                    ok = false;
                }
            }
        }
    }
    // The eight-byte frame runs a closed scalar subset, so most of the suite
    // has no `frame` row. A refusal is reported by name for the reason a
    // lowering refusal is -- a missing row reads as a benchmark nobody ran --
    // and it does **not** fail the suite, because refusing is the experiment
    // working rather than the experiment broken. See `cove_runtime::frame`.
    rows.retain(|row| {
        if row.backend != Backend::Frame {
            return true;
        }
        let ir = row
            .ir
            .expect("a frame row was resolved against the lowering");
        match cove_runtime::frame::admits(ir, row.module, row.entry) {
            Ok(_) => true,
            Err(refused) => {
                println!(
                    "{}",
                    FrameRefusal {
                        benchmark: row.name,
                        what: &refused.what,
                    }
                    .to_json()
                );
                false
            }
        }
    });

    take_samples(&program, &sources, &mut rows, iterations, order);

    for row in &rows {
        let report = row.report(iterations);
        ok &= report.ok;
        println!("{}", report.to_json());
        // A run that did not pass is not a measurement of anything, so it is
        // not compared: the module docs say a caller should refuse numbers
        // from a run that is not `ok`, and this is that caller.
        if report.ok {
            compare(
                baseline.as_ref(),
                &mut compared,
                row.name,
                row.backend.kind(),
                &row.backend.to_string(),
                &report.wall_ns,
            );
        }
        // Not for the frame: ADR 0026 makes a recording name the backend
        // that made it and there is no `RecordingBackend::Frame`, so there is
        // nothing honest for the traced half of the ratio to record itself
        // as. Issue #212 asks for wall time, instructions, allocations and
        // frame bytes, and not for a trace-overhead ratio.
        if row.backend != Backend::Frame {
            println!(
                "{}",
                bench_trace_overhead(&program, &sources, row, iterations).to_json()
            );
        }
    }

    for backend in [Backend::Ast, Backend::Vm, Backend::Lvm] {
        match bench_startup(iterations, backend) {
            Ok(report) => {
                ok &= report.ok;
                println!("{}", report.to_json());
                if report.ok {
                    compare(
                        baseline.as_ref(),
                        &mut compared,
                        "startup",
                        "process",
                        &backend.to_string(),
                        &report.wall_ns,
                    );
                }
            }
            Err(message) => {
                eprintln!("cove-bench: startup on {backend}: {message}");
                ok = false;
            }
        }
    }

    if baseline.is_some() {
        summarize(&compared);
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Which backend produced a number.
///
/// ADR 0019 requires every number this harness reports to say so, because the
/// two are not interchangeable: `fuel_spent` is defined per backend, and a
/// wall-clock figure that did not name its backend would be a comparison
/// missing half of itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    /// The tree-walking interpreter, which is the oracle.
    Ast,
    /// The dedicated VM, over the executable IR.
    Vm,
    /// The experimental eight-byte frame, over the same IR.
    ///
    /// [Issue #212](https://github.com/myuon/cove/issues/212) asks for the
    /// comparison to be made **within one benchmark binary**, and this is
    /// what makes that possible: the same lowered program, the same hosts,
    /// the same budget, the same `run_once`, and one more row. ADR 0029 says
    /// why that shape and not two builds — "a ratio within one build is
    /// repeatable" and a cross-build absolute is not.
    ///
    /// It runs a closed scalar subset and refuses the rest, so most rows have
    /// no `frame` line and say so. See `cove_runtime::frame`.
    Frame,
    /// The linear-memory backend of ADR 0034, over `cove_lir`.
    ///
    /// The one that is meant to survive. ADR 0034 replaces the executable IR,
    /// the lowering and the VM rather than renovating them, and its
    /// completion condition 9 — "representative performance gates show no
    /// order-of-magnitude or repeated multi-fold regression" — is a statement
    /// about *these* rows against the `vm` rows beside them. So it is a row
    /// here for exactly the reason `frame` is: ADR 0029 says a ratio within
    /// one build is repeatable and a cross-build absolute is not, and two
    /// rows of one run share whatever layout that binary has.
    ///
    /// It has no admission predicate. `cove_lir` refuses nothing on purpose —
    /// a construct it has not been taught is a gap in the lowering rather
    /// than a program the backend declines — so an `lvm` row that is missing
    /// is a bug, and this harness reports one as a failure rather than as an
    /// experiment's subset not having reached it yet.
    Lvm,
}

impl Backend {
    /// The value of the `kind` field, which keeps the string the interpreter's
    /// rows have always carried so that a reader of the older format finds
    /// exactly the rows it was reading and no more.
    fn kind(self) -> &'static str {
        match self {
            Backend::Ast => "interpreter",
            Backend::Vm => "vm",
            Backend::Frame => "frame",
            Backend::Lvm => "lvm",
        }
    }

    /// The name `--backend` accepts for this backend, and the one a row is
    /// reported under.
    fn parse(name: &str) -> Option<Backend> {
        match name {
            "ast" => Some(Backend::Ast),
            "vm" => Some(Backend::Vm),
            "frame" => Some(Backend::Frame),
            "lvm" => Some(Backend::Lvm),
            _ => None,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Ast => "ast",
            Backend::Vm => "vm",
            Backend::Frame => "frame",
            Backend::Lvm => "lvm",
        })
    }
}

/// What lowering the package cost, and the IR every VM measurement runs.
struct LoweringReport {
    iterations: u32,
    wall_ns: Stats,
    /// How many functions the lowering emitted, which is what the time is a
    /// time for.
    functions: usize,
    ir: Arc<cove_ir::Program>,
}

impl LoweringReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"benches\",\"kind\":\"lowering\",\"backend\":\"vm\",\"iterations\":{},\"wall_ns\":{},\"functions\":{},\"ok\":true}}",
            self.iterations,
            self.wall_ns.to_json_with_samples(),
            self.functions,
        )
    }
}

/// Lowers the package `iterations` times and validates what it produced.
///
/// The validation is inside the measurement because it is inside the run:
/// `cove run --backend vm` validates before it executes, so leaving it out
/// here would report a cost nobody pays. The last lowering is the one kept,
/// since every one of them is the same program.
fn bench_lowering(
    program: &Arc<Program>,
    iterations: u32,
) -> Result<LoweringReport, cove_ir::Unsupported> {
    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut last = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let ir = cove_ir::lower::lower(program)?;
        cove_ir::lower::validate(&ir)
            .unwrap_or_else(|why| panic!("the lowering holds the VM's invariants: {why}"));
        wall_ns.push(started.elapsed().as_nanos() as u64);
        last = Some(ir);
    }
    let ir = last.expect("`--iterations` is a positive integer, so one lowering happened");
    Ok(LoweringReport {
        iterations,
        wall_ns: Stats::of(&wall_ns),
        functions: ir.functions.len(),
        // Shared rather than owned outright, because a `Vm` takes the handle
        // a spawned task's thread would be given a share of. No benchmark
        // spawns one; the handle is what the type asks for either way.
        ir: Arc::new(ir),
    })
}

/// What lowering one benchmark to `cove_lir` cost, and the program every
/// `lvm` measurement of that benchmark runs.
///
/// One per benchmark rather than one for the package, because the two
/// lowerings do not slice the same way and this harness measures what the
/// command does. `cove_ir::lower` takes a whole package and refuses all of it
/// for one construct anywhere in it, which is what `cove run --backend vm`
/// asks for. `cove_lir::lower_entry` lowers what one entry reaches and stubs
/// the rest, which is what `cove run --backend lvm` asks for. So the `lvm`
/// lowering row is named for the benchmark it lowered and the `vm` one is
/// named `benches`, and a reader comparing the two figures is comparing a
/// slice against a package on purpose.
struct LinearLowering {
    benchmark: &'static str,
    iterations: u32,
    wall_ns: Stats,
    /// How many entries the function table has.
    ///
    /// Not the same figure as the `vm` lowering row's, and it should not be
    /// read as one: a slice still gives every declaration of the package a
    /// table entry, and the ones it did not reach are stubs. What the two
    /// rows share is that each is the count for the thing that was timed.
    functions: usize,
    program: Arc<cove_lir::Program>,
}

impl LinearLowering {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"lowering\",\"backend\":\"lvm\",\"iterations\":{},\"wall_ns\":{},\"functions\":{},\"ok\":true}}",
            self.benchmark,
            self.iterations,
            self.wall_ns.to_json_with_samples(),
            self.functions,
        )
    }
}

/// Lowers one benchmark's entry to `cove_lir` `iterations` times.
///
/// The same shape as [`bench_lowering`] and for the same reason: lowering is
/// paid once per program and execution for as long as the program runs, so
/// the two are timed apart. The schemas are the shipped ones and no others,
/// which is the set `cove_sema::Compiler::new()` checked this package
/// against — `cove run` passes the same, and a lowering that read a different
/// set would be lowering a different program.
fn bench_linear_lowering(
    program: &Program,
    sources: &SourceMap,
    benchmark: &'static str,
    module: &str,
    entry: &str,
    iterations: u32,
) -> Result<LinearLowering, Vec<cove_diag::Diagnostic>> {
    let schemas = HostSchemas::new();
    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut last = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let lowered = cove_lir::lower_entry(program, sources, &schemas, module, entry)?;
        wall_ns.push(started.elapsed().as_nanos() as u64);
        last = Some(lowered);
    }
    let lowered = last.expect("`--iterations` is a positive integer, so one lowering happened");
    Ok(LinearLowering {
        benchmark,
        iterations,
        wall_ns: Stats::of(&wall_ns),
        functions: lowered.functions.len(),
        program: Arc::new(lowered),
    })
}

/// One benchmark the VM cannot run, and what stopped it.
struct Unsupported<'a> {
    benchmark: &'static str,
    why: &'a cove_ir::Unsupported,
}

impl<'a> Unsupported<'a> {
    fn new(benchmark: &'static str, why: &'a cove_ir::Unsupported) -> Unsupported<'a> {
        Unsupported { benchmark, why }
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"unsupported\",\"backend\":\"vm\",\"what\":\"{}\",\"ok\":false}}",
            self.benchmark,
            escape(&self.why.what),
        )
    }
}

/// One benchmark the eight-byte frame cannot run, and what stopped it.
///
/// Separate from [`Unsupported`], which is about a *lowering* that refused,
/// because these are two different facts: a lowering refusal means no backend
/// but the interpreter can run the benchmark at all, and this one means the
/// experiment's subset does not reach it yet. The first fails the suite and
/// the second does not.
struct FrameRefusal<'a> {
    benchmark: &'static str,
    what: &'a str,
}

impl FrameRefusal<'_> {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"unsupported\",\"backend\":\"frame\",\"what\":\"{}\",\"ok\":false}}",
            self.benchmark,
            escape(self.what),
        )
    }
}

/// One benchmark the linear-memory lowering could not lower, and what it said.
///
/// The same shape as the two above and a third fact again. `cove_lir` has no
/// admission predicate and no `Unsupported` type — its module docs say a
/// construct it has not been taught is a bug in the lowering rather than a
/// program it declines — so this row is neither an experiment's subset
/// falling short nor a whole-package lowering refusing one construct. It is a
/// gap in the backend ADR 0034 makes the production one, and it **fails the
/// suite** for that reason.
struct NotLowered<'a> {
    benchmark: &'static str,
    what: &'a str,
}

impl NotLowered<'_> {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"unsupported\",\"backend\":\"lvm\",\"what\":\"{}\",\"ok\":false}}",
            self.benchmark,
            escape(self.what),
        )
    }
}

/// Escapes what a JSON string may not carry literally.
///
/// The construct a refusal names is written for a person and can hold a
/// backtick, a quote, or a backslash; the rest of this file's fields are
/// numbers and fixed identifiers, which is why this is the only place that
/// needs it.
fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c => vec![c],
        })
        .collect()
}

/// Reads `--iterations <n>` from the process arguments, falling back to
/// [`DEFAULT_ITERATIONS`] when it is absent or not a positive integer.
fn parse_iterations() -> u32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--iterations" {
            if let Some(value) = args.get(i + 1).and_then(|v| v.parse::<u32>().ok()) {
                if value > 0 {
                    return value;
                }
            }
            eprintln!("cove-bench: `--iterations` needs a positive integer; using the default");
            return DEFAULT_ITERATIONS;
        }
        i += 1;
    }
    DEFAULT_ITERATIONS
}

/// Reads `--sample-order <blocked|round-robin>` from the process arguments.
///
/// Unlike `--iterations`, a value this does not recognize is an error rather
/// than a fallback: the two orders disagree with each other by more than most
/// changes this repository measures, so a run that silently used the other
/// one would be a measurement of the wrong thing under the right name.
fn parse_sample_order() -> Result<SampleOrder, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--sample-order" {
            return match args.get(i + 1).map(String::as_str) {
                Some("blocked") => Ok(SampleOrder::Blocked),
                Some("round-robin") => Ok(SampleOrder::RoundRobin),
                Some(other) => Err(format!(
                    "`--sample-order` is `blocked` or `round-robin`, not `{other}`"
                )),
                None => Err("`--sample-order` needs `blocked` or `round-robin`".to_string()),
            };
        }
        i += 1;
    }
    Ok(DEFAULT_SAMPLE_ORDER)
}

// ------------------------------------------------------- comparing two runs

/// Reads `--baseline <path>`, if it was given.
///
/// A previous run's own JSON output is the baseline format, which is what
/// makes the fixed-commit discipline
/// [issue #126](https://github.com/myuon/cove/issues/126) argues for a
/// two-command exercise: record the suite once on the commit being measured
/// against, keep the file, and pass it to every run afterwards. Three changes
/// each individually inside the noise summed to 19% there, and no comparison
/// against the parent alone could have seen it.
fn load_baseline() -> Result<Option<Baseline>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--baseline" {
            let path = args
                .get(i + 1)
                .ok_or_else(|| "`--baseline` needs a path to a recorded run".to_string())?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read the baseline `{path}`: {e}"))?;
            let baseline = Baseline::parse(&text)
                .map_err(|why| format!("`{path}` is not a baseline: {why}"))?;
            eprintln!(
                "cove-bench: comparing against `{path}`, which has {} rows",
                baseline.len()
            );
            return Ok(Some(baseline));
        }
        i += 1;
    }
    Ok(None)
}

/// One row that had a baseline to be read against.
struct Compared {
    /// How the row is named in the summary: the benchmark and the backend.
    row: String,
    comparison: Comparison,
}

/// Emits the comparison for one row, when there is a baseline and it has the
/// row.
///
/// A row the baseline does not have produces nothing at all. It is not an
/// error -- benchmarks get added, and the VM learns to run ones it used to
/// refuse -- but it is also not a comparison, and a line saying "no change"
/// for a row that was never measured before would be the worst of both.
fn compare(
    baseline: Option<&Baseline>,
    compared: &mut Vec<Compared>,
    benchmark: &str,
    kind: &str,
    backend: &str,
    current: &Stats,
) {
    let Some(baseline) = baseline else {
        return;
    };
    let Some(recorded) = baseline.samples(benchmark, kind, backend) else {
        return;
    };
    let comparison = Comparison::of(recorded, current.samples());
    println!("{}", comparison.to_json(benchmark, kind, backend));
    compared.push(Compared {
        row: format!("{benchmark}/{backend}"),
        comparison,
    });
}

/// Ends a compared run with the sentence the JSON above is the evidence for.
///
/// On stderr, because stdout is the machine-readable stream and a reader
/// piping it into a file should not find prose in it. This asserts nothing
/// and fails nothing: ADR 0012 argues that wall-clock numbers are not gated,
/// and a verdict computed here is one for a person to act on rather than a
/// threshold this process enforces. The exit code still reflects correctness
/// alone.
fn summarize(compared: &[Compared]) {
    if compared.is_empty() {
        eprintln!("cove-bench: no row of this run had a counterpart in the baseline");
        return;
    }

    let count = |wanted: Verdict| {
        compared
            .iter()
            .filter(|row| row.comparison.verdict == wanted)
            .count()
    };
    eprintln!(
        "cove-bench: {} rows compared: {} regression(s), {} improvement(s), {} inside the noise, {} underpowered",
        compared.len(),
        count(Verdict::Regression),
        count(Verdict::Improvement),
        count(Verdict::InsideTheNoise),
        count(Verdict::Underpowered),
    );

    for row in compared {
        if matches!(
            row.comparison.verdict,
            Verdict::Regression | Verdict::Improvement
        ) {
            eprintln!(
                "cove-bench:   {} {:+.2}% [{:+.2}, {:+.2}] -- {}",
                row.row,
                row.comparison.delta_pct,
                row.comparison.low_pct,
                row.comparison.high_pct,
                row.comparison.verdict.as_str(),
            );
        }
    }

    // The widest interval that did not clear zero is the honest bound on what
    // this run could be hiding, and it is the number a "no meaningful
    // regression" sentence should quote. Without it the sentence claims the
    // change had no effect, which is not what an interval containing zero
    // says.
    let widest = compared
        .iter()
        .filter(|row| row.comparison.verdict == Verdict::InsideTheNoise)
        .max_by(|a, b| {
            let width = |row: &Compared| row.comparison.high_pct - row.comparison.low_pct;
            width(a)
                .partial_cmp(&width(b))
                .expect("an interval that cleared the floor is not NaN")
        });
    if let Some(widest) = widest {
        eprintln!(
            "cove-bench: the widest interval that did not clear zero is {} [{:+.2}, {:+.2}]; \
a regression larger than that would have been seen",
            widest.row, widest.comparison.low_pct, widest.comparison.high_pct,
        );
    }
    if count(Verdict::Underpowered) > 0 {
        eprintln!(
            "cove-bench: an underpowered row has fewer than {} samples on one side; \
`--iterations {}` or more is what makes it a claim",
            stats::MIN_SAMPLES,
            stats::MIN_SAMPLES,
        );
    }
}

// ------------------------------------------------ the calling-convention matrix

/// The rows of the calling-convention matrix, and what each one is.
///
/// [Issue #123](https://github.com/myuon/cove/issues/123) asks what the typed
/// three-stack convention costs at each of its boundaries, so
/// `benches/convention/main.cove` writes `benches/arith`'s loop out again for
/// each of them and changes exactly one thing between two rows: how the
/// turn's `i` reaches the arithmetic that consumes it. The first row is the
/// baseline the rest are read against.
///
/// Eight of the nine are the shapes #123 names. `conv_fresh` is a control
/// rather than one of them: `conv_host`'s callback has to be written at its
/// call site, because it reads the turn's `i` and a capture is a snapshot,
/// so that row builds a closure per turn as well as crossing the Host
/// boundary. This is the row that tells the two apart.
const MATRIX: [(&str, &str); 9] = [
    ("conv_local", "a settled scalar local"),
    ("conv_var", "the same local, rooted for a `var` argument"),
    ("conv_static", "a static declared call"),
    ("conv_fnvalue", "a declared function used as a value"),
    ("conv_closure", "a closure call"),
    ("conv_capture", "a captured scalar"),
    ("conv_generic", "a scalar crossing to generic `Value`"),
    ("conv_fresh", "a closure built per turn, called here"),
    ("conv_host", "a Host callback, and the reentry that runs it"),
];

/// How many turns each row of the matrix takes. Every entry writes the same
/// literal, and the table below divides by it to report a cost per turn.
const MATRIX_TURNS: u64 = 2_000_000;

/// Runs the matrix and prints it as a table.
///
/// A table rather than the JSON the rest of this harness emits, because this
/// is a diagnostic somebody reads rather than a gate something compares.
///
/// It ran on the VM alone until ADR 0034, and the reason it gave still holds
/// for the interpreter: what it measures is a calling convention, and the
/// interpreter does not have one -- it has an environment chain, which is a
/// different thing. What changed is that there is now a second backend that
/// *does* have one, and ADR 0034's completion condition 9 is a question about
/// it. So `--backend` takes a comma-separated list, the default is `vm` and
/// nothing about a default run has moved, and `--backend vm,lvm` measures the
/// two conventions against each other **in one run** -- which is the only
/// place ADR 0029 says such a ratio may be read.
///
/// This does not run under `cove-bench` with no arguments, and that is
/// deliberate: eight two-million-turn loops on top of the suite would double
/// what every push waits for, to answer a question nobody asked on that push.
/// One row of the calling-convention matrix on one backend, and the samples
/// taken of it.
struct MatrixRow<'a> {
    name: &'static str,
    what: &'static str,
    backend: Backend,
    module: &'a str,
    entry: &'a str,
    allow: Vec<String>,
    ir: Option<&'a Arc<cove_ir::Program>>,
    lir: Option<&'a Arc<cove_lir::Program>>,
    samples: Vec<u64>,
    instructions: u64,
    ok: bool,
}

/// Reads `--backend <list>` for the matrix, defaulting to the VM alone.
///
/// A value it does not recognize is an error rather than a fallback, for the
/// reason `--sample-order` gives: a run that silently measured something else
/// is a measurement of the wrong thing under the right name. `frame` is
/// refused by name rather than ignored -- the eight-byte frame has an
/// admission predicate and the matrix is written out of closures, captures
/// and a Host callback, so what it would report is mostly refusals.
fn parse_matrix_backends() -> Result<Vec<Backend>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--backend" {
            let Some(list) = args.get(i + 1) else {
                return Err("`--backend` needs a comma-separated list of backends".to_string());
            };
            let mut backends = Vec::new();
            for name in list.split(',') {
                match Backend::parse(name) {
                    Some(Backend::Frame) => {
                        return Err(
                            "the matrix has no `frame` column: that backend admits a closed \
scalar subset and every row here is a call"
                                .to_string(),
                        )
                    }
                    Some(backend) if backends.contains(&backend) => {
                        return Err(format!("`{backend}` is named twice in `--backend`"))
                    }
                    Some(backend) => backends.push(backend),
                    None => {
                        return Err(format!(
                            "`--backend` takes `ast`, `vm` or `lvm`, not `{name}`"
                        ))
                    }
                }
            }
            if backends.is_empty() {
                return Err("`--backend` needs at least one backend".to_string());
            }
            return Ok(backends);
        }
        i += 1;
    }
    Ok(vec![Backend::Vm])
}

fn matrix(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    iterations: u32,
    order: SampleOrder,
) -> ExitCode {
    let backends = match parse_matrix_backends() {
        Ok(backends) => backends,
        Err(message) => {
            eprintln!("cove-bench: {message}");
            return ExitCode::FAILURE;
        }
    };

    // Only what a named backend needs is lowered, so a `--backend ast` run
    // does not fail on a lowering nothing in it was going to execute.
    let lowered = if backends.contains(&Backend::Vm) {
        match cove_ir::lower::lower(program) {
            Ok(lowered) => Some(Arc::new(lowered)),
            Err(why) => {
                eprintln!("cove-bench: the VM cannot run `benches/`: {}", why.what);
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    // A row name after `--matrix` runs that row alone, which is what a
    // profiler wants: `samply record -- cove-bench --matrix conv_host`
    // records one row rather than eight.
    let only: Option<String> = std::env::args()
        .skip_while(|argument| argument != "--matrix")
        .nth(1)
        .filter(|argument| !argument.starts_with("--"));
    let wanted = |name: &str| only.as_deref().is_none_or(|only| only == name);

    // One linear-memory lowering per row, because that is the unit
    // `cove_lir::lower_entry` lowers. All of them before anything is timed.
    let mut linear: Vec<(&'static str, Arc<cove_lir::Program>)> = Vec::new();
    if backends.contains(&Backend::Lvm) {
        for (name, _) in MATRIX.iter() {
            if !wanted(name) {
                continue;
            }
            let (module, entry, _) = match entry_for(package, program, name) {
                Ok(found) => found,
                Err(message) => {
                    eprintln!("cove-bench: {message}");
                    return ExitCode::FAILURE;
                }
            };
            match cove_lir::lower_entry(program, sources, &HostSchemas::new(), module, entry) {
                Ok(lowered) => linear.push((name, Arc::new(lowered))),
                Err(diagnostics) => {
                    let what = diagnostics
                        .first()
                        .map(|d| d.message.as_str())
                        .unwrap_or("the lowering failed and said nothing");
                    eprintln!("cove-bench: `{name}` does not lower to the linear IR: {what}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    println!(
        "the calling-convention matrix, {} backend(s), {iterations} iteration(s) \
of {MATRIX_TURNS} turns each",
        backends
            .iter()
            .map(Backend::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );

    // The matrix is read as ratios *between* its rows, so the order it takes
    // its samples in matters to it more than it does to the suite: a row
    // measured minutes after the row it is divided by carries whatever the
    // machine did in between into the quotient. So it obeys `--sample-order`
    // too, which is why every row is opened before any of them is timed and
    // nothing is printed until all of them are finished. With more than one
    // backend the same argument covers the ratio between two backends of one
    // row, which is what interleaves them here rather than running a backend
    // at a time.
    let mut rows: Vec<MatrixRow> = Vec::new();
    for (name, what) in MATRIX.iter() {
        if !wanted(name) {
            continue;
        }
        let (module, entry, allow) = match entry_for(package, program, name) {
            Ok(found) => found,
            Err(message) => {
                eprintln!("cove-bench: {message}");
                return ExitCode::FAILURE;
            }
        };
        for &backend in &backends {
            rows.push(MatrixRow {
                name,
                what,
                backend,
                module,
                entry,
                allow: allow.clone(),
                ir: match backend {
                    Backend::Vm => lowered.as_ref(),
                    _ => None,
                },
                lir: match backend {
                    Backend::Lvm => linear
                        .iter()
                        .find(|(row, _)| row == name)
                        .map(|(_, program)| program),
                    _ => None,
                },
                samples: Vec::with_capacity(iterations as usize),
                instructions: 0,
                ok: true,
            });
        }
    }

    let sample = |row: &mut MatrixRow| {
        let measurement = run_once(
            program,
            sources,
            row.module,
            row.entry,
            &row.allow,
            Arc::new(NullSink),
            row.backend,
            row.ir,
            row.lir,
        );
        row.samples.push(measurement.wall.as_nanos() as u64);
        row.instructions = measurement.instructions.unwrap_or(0);
        if let Some(message) = measurement.failure {
            eprintln!(
                "cove-bench: matrix row `{}` on {} did not pass: {message}",
                row.name, row.backend
            );
            row.ok = false;
        }
    };
    match order {
        SampleOrder::Blocked => {
            for row in rows.iter_mut() {
                for _ in 0..iterations {
                    sample(row);
                }
            }
        }
        SampleOrder::RoundRobin => {
            for _ in 0..iterations {
                for row in rows.iter_mut() {
                    sample(row);
                }
            }
        }
    }

    let mut ok = true;
    // One block per backend, so that a default run reads exactly as it always
    // has and a two-backend run is two tables rather than one table whose
    // neighbouring lines are not comparable.
    for &backend in &backends {
        println!("\n{backend}:");
        println!(
            "row                 median       min       max  spread vs base   \
instructions per turn   ns/turn  what"
        );
        let mut baseline = 0.0f64;
        for row in rows.iter_mut().filter(|row| row.backend == backend) {
            let (name, what, instructions) = (row.name, row.what, row.instructions);
            ok &= row.ok;
            let samples = &mut row.samples;
            samples.sort_unstable();

            let median = stats::quantile(samples, 0.5) / 1e6;
            let min = samples[0] as f64 / 1e6;
            let max = samples[samples.len() - 1] as f64 / 1e6;
            if baseline == 0.0 && name == MATRIX[0].0 {
                baseline = median;
            }
            // A single row asked for by name has no baseline beside it, and a
            // ratio against a row that did not run would be a number made up.
            let against = if baseline > 0.0 {
                format!("{:.2}x", median / baseline)
            } else {
                "-".to_string()
            };
            println!(
                "{:<14} {:>8.2}ms {:>8.2}ms {:>8.2}ms {:>6.1}% {:>7} {:>14} {:>8.1} {:>8.1}  {}",
                name,
                median,
                min,
                max,
                100.0 * (max - min) / median,
                against,
                instructions,
                instructions as f64 / MATRIX_TURNS as f64,
                median * 1e6 / MATRIX_TURNS as f64,
                what
            );
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The `benches/` package, rooted next to this crate.
fn benches_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches")
}

fn load_benches() -> Result<(SourceMap, Package, Program), String> {
    let root = benches_root();
    let mut sources = SourceMap::new();
    let package = cove_sema::package::load(&root, &mut sources).map_err(|items| {
        format!(
            "`{}` does not load:\n{}",
            root.display(),
            render_all(&sources, &items)
        )
    })?;
    // Both halves of the check, because the lowering reads what the second
    // one settled and a program that was only resolved carries none of it.
    // A benchmark measured against that program would be measuring a
    // lowering `cove run --backend vm` never produces.
    let program = cove_sema::Compiler::new()
        .compile(&package)
        .map_err(|items| {
            format!(
                "`{}` does not check:\n{}",
                root.display(),
                render_all(&sources, &items)
            )
        })?;
    Ok((sources, package, program))
}

fn render_all(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The module, function name, and granted capabilities for `[run.<name>]`,
/// looked up the way `cove run` looks up a run.
fn entry_for<'a>(
    package: &'a Package,
    program: &Program,
    name: &str,
) -> Result<(&'a str, &'a str, Vec<String>), String> {
    let run = package
        .config
        .runs
        .get(name)
        .ok_or_else(|| format!("`benches/cove.toml` has no `[run.{name}]` table"))?;
    let (module, entry) = run
        .entry_parts()
        .ok_or_else(|| format!("`[run.{name}] entry` must be a qualified function"))?;
    if program.lookup_fn(module, entry).is_none() {
        return Err(format!(
            "`[run.{name}] entry` refers to `{}`, which `benches/` does not declare",
            run.entry
        ));
    }
    Ok((module, entry, run.allow.clone()))
}

/// The same deterministic fakes `cove test` grants by default (see
/// `crates/cove-cli/src/test.rs`), always chosen here: a Host-heavy
/// benchmark measures dispatch, grant checks, and budget accounting through
/// them, never real I/O latency and never the network.
fn fake_hosts(allow: Vec<String>) -> HostRegistry {
    let mut hosts = HostRegistry::new(Grants::new(allow));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.register(Box::new(Env::new(Default::default())));
    hosts.register(Box::new(Documents::in_memory(Default::default())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Files::in_memory(Default::default())));
    hosts.register(Box::new(Process::recorded(
        Vec::new(),
        Default::default(),
        ProcessLog::new(),
    )));
    hosts.register(Box::new(Database::recorded(Default::default())));
    hosts
}

/// What one run of a benchmark's entry measured.
struct RunMeasurement {
    wall: Duration,
    fuel_spent: u64,
    host_calls: u64,
    irreversible_writes: u64,
    heap: HeapStats,
    /// How many instructions the run executed, on a lowered backend. `None`
    /// on the interpreter, which has none -- the same distinction `cove run
    /// --stats` makes, and for the same reason.
    ///
    /// The figure a rebuild cannot move, which is why it is reported beside
    /// the wall time rather than only inside `--matrix`: ADR 0029 makes an
    /// exact count repeatable where an absolute is not, and ADR 0034 asks for
    /// instruction counts by name if a gate fails. A ratio with a count
    /// beside it says whether a backend was slower per instruction or simply
    /// ran more of them.
    instructions: Option<u64>,
    /// `Some(message)` when the entry returned `Err` or the interpreter
    /// itself failed; `None` when it passed.
    failure: Option<String>,
}

/// Builds a fresh registry, budget, and backend -- exactly what `cove run`
/// builds for one run -- and calls `module.entry` once under `trace`.
///
/// `ir` is what selects the backend: the VM when the program was lowered, the
/// interpreter when it was not. Everything either of them is given is built
/// the same way and given to both, so the difference between two measurements
/// is the backend and nothing around it.
#[allow(clippy::too_many_arguments)]
fn run_once(
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    module: &str,
    entry: &str,
    allow: &[String],
    trace: Arc<dyn TraceSink>,
    backend: Backend,
    ir: Option<&Arc<cove_ir::Program>>,
    lir: Option<&Arc<cove_lir::Program>>,
) -> RunMeasurement {
    let mut hosts = fake_hosts(allow.to_vec());
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(trace.clone());

    let hosts = Arc::new(hosts);
    let runtime =
        Runtime::new(Arc::clone(program), Arc::clone(sources), hosts.clone()).with_trace(trace);

    let started = Instant::now();
    // The linear backend is matched first because what selects it is a
    // different program: `ir` is `cove_ir`'s and `lir` is `cove_lir`'s, and a
    // row carries the one its backend runs.
    if backend == Backend::Lvm {
        let lir = lir.expect("an `lvm` row was resolved against the linear lowering");
        let mut lvm = Lvm::new(&runtime, &hosts, lir);
        let outcome = lvm.run_entry(module, entry, Vec::<Rc<str>>::new());
        // `heap_words` is the heap region's size, free blocks included, and
        // not the peak live set the object heaps report. The two answer
        // different questions and this harness does not pretend otherwise:
        // see [`ExecutionReport::heap_peak_bytes`].
        let heap = HeapStats {
            peak_bytes: lvm.heap_words() * 8,
            allocated_bytes: lvm.allocated_words() * 8,
            ..HeapStats::default()
        };
        let instructions = Some(lvm.instructions());
        let wall = started.elapsed();
        return finish(&runtime, wall, heap, instructions, outcome);
    }
    let (outcome, heap, instructions) = match (backend, ir) {
        (Backend::Vm, Some(ir)) => {
            let mut vm = Vm::new(&runtime, &hosts, ir);
            let outcome = vm.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, vm.heap_stats(), Some(vm.instructions()))
        }
        (Backend::Frame, Some(ir)) => {
            let mut frame = FrameVm::new(&runtime, &hosts, ir);
            let outcome = frame.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, frame.heap_stats(), Some(frame.instructions()))
        }
        // The interpreter, and — unreachably — a lowered backend with no IR,
        // which `Row::resolve` cannot produce.
        _ => {
            let mut interpreter = Interpreter::new(&runtime);
            let outcome = interpreter.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, interpreter.heap_stats(), None)
        }
    };
    let wall = started.elapsed();
    finish(&runtime, wall, heap, instructions, outcome)
}

/// Reads the counters a run leaves behind and says whether it passed.
///
/// Shared by every backend, so that what a measurement is made of does not
/// depend on which evaluator produced it.
fn finish(
    runtime: &Runtime,
    wall: Duration,
    heap: HeapStats,
    instructions: Option<u64>,
    outcome: Result<Value, cove_runtime::RuntimeError>,
) -> RunMeasurement {
    let (fuel_spent, host_calls) = runtime
        .hosts()
        .with_budget(|budget| (budget.fuel_spent(), budget.host_calls()))
        .unwrap_or((0, 0));
    let irreversible_writes = runtime.hosts().irreversible_writes();

    let failure = match outcome {
        Ok(value) => entry_err_message(&value),
        Err(error) => Some(error.message),
    };

    RunMeasurement {
        wall,
        fuel_spent,
        host_calls,
        irreversible_writes,
        heap,
        instructions,
        failure,
    }
}

/// `Some(message)` when `value` is the `Err` an entry returned; `None` for
/// `Ok` or an entry that returns bare `()`.
fn entry_err_message(value: &Value) -> Option<String> {
    Some(
        value
            .err_payload()?
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// One benchmark's execution report on one backend: wall time, fuel spent,
/// and the heap's peak live bytes.
///
/// `fuel_spent` is the backend's own normalized work counter, unaffected by
/// machine noise and comparable only against itself: ADR 0019 says an
/// instruction is not an AST node and there is no honest mapping between
/// them, so the two backends' fuel figures are two measurements and not one
/// comparison. `wall_ns` is what compares them.
struct ExecutionReport {
    benchmark: &'static str,
    backend: Backend,
    iterations: u32,
    wall_ns: Stats,
    fuel_spent: u64,
    fuel_per_sec: f64,
    /// The largest live set a collection measured, on the backends that
    /// collect an object heap.
    ///
    /// **On `lvm` this is a different statistic under the same name**, and
    /// there is no honest way to make it the same one: that backend has no
    /// object heap to take a live set of, it has a heap *region*, and what it
    /// can answer is how many words of it the run held. So the `lvm` row
    /// reports the region's size in bytes — free blocks included — and the
    /// `ast`, `vm` and `frame` rows report what they always did. Read the
    /// `lvm` figure against itself and not against the row above it.
    heap_peak_bytes: Stats,
    host_calls: u64,
    irreversible_writes: u64,
    /// How many instructions the run executed, or `None` on the interpreter.
    instructions: Option<u64>,
    ok: bool,
}

impl ExecutionReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"{}\",\"backend\":\"{}\",\"iterations\":{},\"wall_ns\":{},\"fuel_spent\":{},\"fuel_per_sec\":{:.1},\"heap_peak_bytes\":{},\"host_calls\":{},\"irreversible_writes\":{},\"instructions\":{},\"ok\":{}}}",
            self.benchmark,
            self.backend.kind(),
            self.backend,
            self.iterations,
            self.wall_ns.to_json_with_samples(),
            self.fuel_spent,
            self.fuel_per_sec,
            self.heap_peak_bytes.to_json(),
            self.host_calls,
            self.irreversible_writes,
            match self.instructions {
                Some(instructions) => instructions.to_string(),
                None => "null".to_string(),
            },
            self.ok,
        )
    }
}

/// One benchmark on one backend, and the samples taken of it so far.
///
/// The series is a field rather than a local of the loop that fills it
/// because [`SampleOrder::RoundRobin`] leaves and comes back: a row is opened
/// once, sampled at whatever points in the suite the order says, and read
/// only when every row is finished.
struct Row<'a> {
    name: &'static str,
    backend: Backend,
    module: &'a str,
    entry: &'a str,
    allow: Vec<String>,
    ir: Option<&'a Arc<cove_ir::Program>>,
    /// The linear-memory program this row runs, on an `lvm` row and nowhere
    /// else. Lowered per entry, because that is what `cove_lir` lowers.
    lir: Option<&'a Arc<cove_lir::Program>>,
    wall_ns: Vec<u64>,
    heap_peak: Vec<u64>,
    fuel_spent: u64,
    host_calls: u64,
    irreversible_writes: u64,
    instructions: Option<u64>,
    ok: bool,
}

impl<'a> Row<'a> {
    /// Looks the benchmark's entry up, without running anything.
    fn resolve(
        package: &'a Package,
        program: &Program,
        name: &'static str,
        backend: Backend,
        ir: Option<&'a Arc<cove_ir::Program>>,
        lir: Option<&'a Arc<cove_lir::Program>>,
    ) -> Result<Row<'a>, String> {
        let (module, entry, allow) = entry_for(package, program, name)?;
        Ok(Row {
            name,
            backend,
            module,
            entry,
            allow,
            ir,
            lir,
            wall_ns: Vec::new(),
            heap_peak: Vec::new(),
            fuel_spent: 0,
            host_calls: 0,
            irreversible_writes: 0,
            instructions: None,
            ok: true,
        })
    }

    /// Runs the benchmark once more and keeps what that run measured.
    ///
    /// The counters are assignments rather than accumulations because they
    /// are exact and every run produces the same ones: a benchmark that ran a
    /// different number of instructions on its ninth sample than on its first
    /// would be a different benchmark, and that is what `fuel_spent` being
    /// identical across a table is there to prove.
    fn sample(&mut self, program: &Arc<Program>, sources: &Arc<SourceMap>) {
        let measurement = run_once(
            program,
            sources,
            self.module,
            self.entry,
            &self.allow,
            Arc::new(NullSink),
            self.backend,
            self.ir,
            self.lir,
        );
        self.wall_ns.push(measurement.wall.as_nanos() as u64);
        self.heap_peak.push(measurement.heap.peak_bytes);
        self.fuel_spent = measurement.fuel_spent;
        self.host_calls = measurement.host_calls;
        self.irreversible_writes = measurement.irreversible_writes;
        self.instructions = measurement.instructions;
        if let Some(message) = measurement.failure {
            eprintln!(
                "cove-bench: benchmark `{}` on {} did not pass: {message}",
                self.name, self.backend
            );
            self.ok = false;
        }
    }

    /// What the row measured, once every sample of it has been taken.
    fn report(&self, iterations: u32) -> ExecutionReport {
        let wall = Stats::of(&self.wall_ns);
        let fuel_per_sec = if wall.mean() > 0 {
            self.fuel_spent as f64 / (wall.mean() as f64 / 1e9)
        } else {
            0.0
        };
        ExecutionReport {
            benchmark: self.name,
            backend: self.backend,
            iterations,
            wall_ns: wall,
            fuel_spent: self.fuel_spent,
            fuel_per_sec,
            heap_peak_bytes: Stats::of(&self.heap_peak),
            host_calls: self.host_calls,
            irreversible_writes: self.irreversible_writes,
            instructions: self.instructions,
            ok: self.ok,
        }
    }
}

/// Fills every row's series, in the order `order` asks for.
///
/// Both orders run exactly the same runs exactly as many times. What differs
/// is when: [`SampleOrder::Blocked`] finishes a row before it starts the next
/// one, so a row's whole series is taken in one span of the suite, and
/// [`SampleOrder::RoundRobin`] takes one sample of every row per pass, so
/// each row's series is spread across the whole of it.
fn take_samples(
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    rows: &mut [Row<'_>],
    iterations: u32,
    order: SampleOrder,
) {
    match order {
        SampleOrder::Blocked => {
            for row in rows.iter_mut() {
                for _ in 0..iterations {
                    row.sample(program, sources);
                }
            }
        }
        SampleOrder::RoundRobin => {
            for _ in 0..iterations {
                for row in rows.iter_mut() {
                    row.sample(program, sources);
                }
            }
        }
    }
}

/// Compares one benchmark run untraced against the same run under a real
/// [`JsonlSink`] writing nowhere: the difference is tracing's own cost, not
/// the cost of whatever the sink's destination happens to be.
struct TraceOverheadReport {
    benchmark: &'static str,
    backend: Backend,
    untraced_wall_ns: u64,
    traced_wall_ns: u64,
    overhead_ratio: f64,
}

impl TraceOverheadReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"trace_overhead\",\"backend\":\"{}\",\"untraced_wall_ns\":{},\"traced_wall_ns\":{},\"overhead_ratio\":{:.3}}}",
            self.benchmark,
            self.backend,
            self.untraced_wall_ns,
            self.traced_wall_ns,
            self.overhead_ratio
        )
    }
}

/// Times one row untraced and then traced, back to back.
///
/// This one stays blocked whatever `--sample-order` says, and deliberately:
/// what it reports is the *ratio* of two series of the same work, so the two
/// have to be taken as close together as they can be. Spreading them apart
/// would put the machine's drift between the numerator and the denominator,
/// which is the mistake the flag exists to avoid making everywhere else.
fn bench_trace_overhead(
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    row: &Row<'_>,
    iterations: u32,
) -> TraceOverheadReport {
    let (module, entry, allow, ir, lir) = (row.module, row.entry, &row.allow, row.ir, row.lir);

    let mut untraced = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let m = run_once(
            program,
            sources,
            module,
            entry,
            allow,
            Arc::new(NullSink),
            row.backend,
            ir,
            lir,
        );
        untraced.push(m.wall.as_nanos() as u64);
    }

    let mut traced = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let header = TraceHeader {
            // The backend this measurement is of, which is the one the
            // recording would have been made on had it been kept.
            backend: match row.backend {
                Backend::Ast => RecordingBackend::Ast,
                Backend::Lvm => RecordingBackend::Lvm,
                // Unreachable for `Backend::Frame`: `bench` does not ask for
                // a trace-overhead row of one. ADR 0026 makes a recording
                // name the backend that made it, and there is no
                // `RecordingBackend::Frame` — adding one would be a change to
                // the recording format for the sake of an experiment that is
                // written to be deleted, which issue #212 puts out of scope.
                Backend::Vm | Backend::Frame => RecordingBackend::Vm,
            },
            values: ValueCapture::Redacted,
            entry: format!("{module}.{entry}"),
            args: Vec::new(),
        };
        let sink: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(std::io::sink(), header));
        let m = run_once(
            program,
            sources,
            module,
            entry,
            allow,
            sink,
            row.backend,
            ir,
            lir,
        );
        traced.push(m.wall.as_nanos() as u64);
    }

    let untraced_mean = Stats::of(&untraced).mean();
    let traced_mean = Stats::of(&traced).mean();
    let overhead_ratio = if untraced_mean > 0 {
        traced_mean as f64 / untraced_mean as f64
    } else {
        1.0
    };

    TraceOverheadReport {
        benchmark: row.name,
        backend: row.backend,
        untraced_wall_ns: untraced_mean,
        traced_wall_ns: traced_mean,
        overhead_ratio,
    }
}

/// Process-level startup: spawns the real `cove` binary and times the whole
/// exec-to-exit span, which is what an in-process measurement cannot see.
struct StartupReport {
    backend: Backend,
    iterations: u32,
    wall_ns: Stats,
    ok: bool,
}

impl StartupReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"startup\",\"kind\":\"process\",\"backend\":\"{}\",\"iterations\":{},\"wall_ns\":{},\"ok\":{}}}",
            self.backend,
            self.iterations,
            self.wall_ns.to_json_with_samples(),
            self.ok
        )
    }
}

/// The `cove` binary built alongside this one.
///
/// `cove-bench` is not run through `cargo test`, so `CARGO_BIN_EXE_cove` is
/// not set; the two binaries land in the same target directory whether the
/// workspace was built with `cargo build --workspace` or one crate at a
/// time, so this binary's own directory is where to look.
fn cove_binary() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot read this binary's own path: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "this binary has no parent directory".to_string())?;
    let name = if cfg!(windows) { "cove.exe" } else { "cove" };
    let path = dir.join(name);
    if !path.is_file() {
        return Err(format!(
            "`{}` does not exist; run `cargo build -p cove-cli` first",
            path.display()
        ));
    }
    Ok(path)
}

/// The startup a `--backend vm` process pays is the one this measures, which
/// is the point of measuring it here rather than in-process: the lowering is
/// part of what a VM run costs before it does any work, and a process is
/// where every such cost is paid at once.
fn bench_startup(iterations: u32, backend: Backend) -> Result<StartupReport, String> {
    let cove = cove_binary()?;
    let root = benches_root();

    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut ok = true;

    for _ in 0..iterations {
        let started = Instant::now();
        let status = Command::new(&cove)
            .arg("run")
            .arg("startup")
            .arg("--backend")
            .arg(backend.to_string())
            .current_dir(&root)
            .status()
            .map_err(|e| format!("cannot run `{}`: {e}", cove.display()))?;
        wall_ns.push(started.elapsed().as_nanos() as u64);
        ok &= status.success();
    }

    Ok(StartupReport {
        backend,
        iterations,
        wall_ns: Stats::of(&wall_ns),
        ok,
    })
}
