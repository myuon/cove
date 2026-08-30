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
//! # Both backends
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) says every
//! number this harness reports must say which backend produced it, and
//! [issue #111](https://github.com/myuon/cove/issues/111) gates the VM's
//! adoption on the comparison. So every measurement below carries a
//! `backend`, and every benchmark is measured on both.
//!
//! The VM cannot run every construct yet. Where it cannot, the benchmark says
//! so and names what stopped it rather than going missing: a benchmark absent
//! from a report reads as one nobody ran, and the two are not the same fact.
//!
//! `cove_ir::lower` lowers a whole package and refuses all of it for one
//! construct anywhere in it, which is exactly what `cove run --backend vm`
//! does, so that is what is measured here: one lowering of `benches/`, timed
//! apart from every execution, because lowering happens once per program and
//! execution happens for as long as the program does. That separation is the
//! compile/lower breakdown #111 asks for.
//!
//! # Output
//!
//! One JSON object per line on stdout, in the order the benchmarks ran:
//!
//! ```text
//! {"benchmark":"benches","kind":"lowering","backend":"vm","iterations":<u32>,"wall_ns":<series>,"functions":<usize>,"ok":<bool>}
//! {"benchmark":"pure","kind":"interpreter","backend":"ast","iterations":<u32>,"wall_ns":<series>,"fuel_spent":<u64>,"fuel_per_sec":<f64>,"heap_peak_bytes":<summary>,"host_calls":<u64>,"irreversible_writes":<u64>,"ok":<bool>}
//! {"benchmark":"pure","kind":"vm","backend":"vm", ...the same fields...}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"ast","untraced_wall_ns":<u64>,"traced_wall_ns":<u64>,"overhead_ratio":<f64>}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"vm", ...the same fields...}
//! {"benchmark":"hostheavy", ...the same four lines...}
//! ... and the same four for each of `arith`, `arrayget`, `field`, `method`,
//! `call`, `chars`, and `callback`
//! {"benchmark":"startup","kind":"process","backend":"ast","iterations":<u32>,"wall_ns":<series>,"ok":<bool>}
//! {"benchmark":"startup","kind":"process","backend":"vm", ...the same fields...}
//! ```
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
//! `samples` is every timing the run took, on the wall-time series alone. It
//! is what turns a recorded run into a baseline: a summary can only be
//! compared against another summary by arithmetic that invents the spread it
//! needs, and the samples do not have to be invented.
//!
//! A benchmark the lowering refuses reports that instead of its `vm` lines:
//!
//! ```text
//! {"benchmark":"pure","kind":"unsupported","backend":"vm","what":"<the construct>","ok":false}
//! ```
//!
//! `kind` keeps the value it has always had for the interpreter's rows, so a
//! reader of the older format still finds exactly the rows it was reading and
//! does not silently start counting the VM's as well. `backend` is what now
//! says which of the two produced a number.
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
//! mean of the two base runs. A single base-then-variant pair cannot tell the
//! change from the time that passed between them: on the machine
//! `docs/VM_ARCHITECTURE.md`'s tables were taken on, one unmodified binary
//! disagreed with itself by 7.4% over forty minutes with nothing changed at
//! all. The two base runs' disagreement with each other is the measurement's
//! own error bar, it costs one extra run, and it should be quoted beside the
//! result -- where it is as large as the effect, that is the result.
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
//! Reading one backend against the other is what the output is arranged for:
//! the two `wall_ns` medians of one benchmark are the comparison, and the
//! `fuel_spent` beside them is not, because ADR 0019 makes fuel
//! backend-specific and says so.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::vm::Vm;
use cove_runtime::{
    Budget, Cancellation, Clock, Console, Database, Documents, Env, Files, Grants, HeapStats,
    HostRegistry, JsonlSink, Limits, NullSink, Process, ProcessLog, RecordingBackend, Runtime,
    TraceHeader, TraceSink, Value, ValueCapture, VirtualTime,
};
use cove_sema::package::Package;
use cove_sema::resolve::Program;

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
        return matrix(&package, &program, &sources, iterations);
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

    for name in [
        "pure",
        "hostheavy",
        "arith",
        "arrayget",
        "field",
        "method",
        "call",
        "chars",
        "callback",
    ] {
        for backend in [Backend::Ast, Backend::Vm] {
            // A benchmark the lowering refused is reported as refused rather
            // than skipped: a missing row reads as a benchmark nobody ran,
            // and that is a different fact from one the VM cannot run.
            let ir = match (backend, &lowered) {
                (Backend::Ast, _) => None,
                (Backend::Vm, Ok(report)) => Some(&report.ir),
                (Backend::Vm, Err(why)) => {
                    println!("{}", Unsupported::new(name, why).to_json());
                    ok = false;
                    continue;
                }
            };

            match bench_execution(&package, &program, &sources, name, iterations, backend, ir) {
                Ok(report) => {
                    ok &= report.ok;
                    println!("{}", report.to_json());
                    // A run that did not pass is not a measurement of
                    // anything, so it is not compared: the module docs say a
                    // caller should refuse numbers from a run that is not
                    // `ok`, and this is that caller.
                    if report.ok {
                        compare(
                            baseline.as_ref(),
                            &mut compared,
                            name,
                            backend.kind(),
                            &backend.to_string(),
                            &report.wall_ns,
                        );
                    }
                }
                Err(message) => {
                    eprintln!("cove-bench: benchmark `{name}` on {backend}: {message}");
                    ok = false;
                }
            }

            match bench_trace_overhead(&package, &program, &sources, name, iterations, backend, ir)
            {
                Ok(report) => println!("{}", report.to_json()),
                Err(message) => {
                    eprintln!(
                        "cove-bench: benchmark `{name}` on {backend} (trace overhead): {message}"
                    );
                    ok = false;
                }
            }
        }
    }

    for backend in [Backend::Ast, Backend::Vm] {
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
}

impl Backend {
    /// The value of the `kind` field, which keeps the string the interpreter's
    /// rows have always carried so that a reader of the older format finds
    /// exactly the rows it was reading and no more.
    fn kind(self) -> &'static str {
        match self {
            Backend::Ast => "interpreter",
            Backend::Vm => "vm",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Ast => "ast",
            Backend::Vm => "vm",
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
/// is a diagnostic somebody reads rather than a gate something compares. It
/// runs on the VM alone for the same reason: what it measures is a calling
/// convention, and the interpreter does not have one -- it has an
/// environment chain, which is a different thing and not the question.
///
/// This does not run under `cove-bench` with no arguments, and that is
/// deliberate: eight two-million-turn loops on top of the suite would double
/// what every push waits for, to answer a question nobody asked on that push.
fn matrix(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    iterations: u32,
) -> ExitCode {
    let lowered = match cove_ir::lower::lower(program) {
        Ok(lowered) => Arc::new(lowered),
        Err(why) => {
            eprintln!("cove-bench: the VM cannot run `benches/`: {}", why.what);
            return ExitCode::FAILURE;
        }
    };

    println!(
        "the calling-convention matrix, VM backend, {iterations} iteration(s) \
of {MATRIX_TURNS} turns each"
    );
    println!(
        "row                 median       min       max  spread vs base   \
instructions per turn   ns/turn  what"
    );

    // A row name after `--matrix` runs that row alone, which is what a
    // profiler wants: `samply record -- cove-bench --matrix conv_host`
    // records one row rather than eight.
    let only: Option<String> = std::env::args()
        .skip_while(|argument| argument != "--matrix")
        .nth(1)
        .filter(|argument| !argument.starts_with("--"));

    let mut ok = true;
    let mut baseline = 0.0f64;
    for (index, (name, what)) in MATRIX.iter().enumerate() {
        if only.as_deref().is_some_and(|wanted| wanted != *name) {
            continue;
        }
        let (module, entry, allow) = match entry_for(package, program, name) {
            Ok(found) => found,
            Err(message) => {
                eprintln!("cove-bench: {message}");
                return ExitCode::FAILURE;
            }
        };

        let mut samples = Vec::with_capacity(iterations as usize);
        let mut instructions = 0;
        for _ in 0..iterations {
            let measurement = run_once(
                program,
                sources,
                module,
                entry,
                &allow,
                Arc::new(NullSink),
                Some(&lowered),
            );
            samples.push(measurement.wall.as_nanos() as u64);
            instructions = measurement.instructions.unwrap_or(0);
            if let Some(message) = measurement.failure {
                eprintln!("cove-bench: matrix row `{name}` did not pass: {message}");
                ok = false;
            }
        }
        samples.sort_unstable();

        let median = stats::quantile(&samples, 0.5) / 1e6;
        let min = samples[0] as f64 / 1e6;
        let max = samples[samples.len() - 1] as f64 / 1e6;
        if index == 0 {
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
    /// How many instructions the run executed, on the VM. `None` on the
    /// interpreter, which has none -- the same distinction `cove run
    /// --stats` makes, and for the same reason.
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
    ir: Option<&Arc<cove_ir::Program>>,
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
    let (outcome, heap, instructions) = match ir {
        Some(ir) => {
            let mut vm = Vm::new(&runtime, &hosts, ir);
            let outcome = vm.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, vm.heap_stats(), Some(vm.instructions()))
        }
        None => {
            let mut interpreter = Interpreter::new(&runtime);
            let outcome = interpreter.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, interpreter.heap_stats(), None)
        }
    };
    let wall = started.elapsed();

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
    heap_peak_bytes: Stats,
    host_calls: u64,
    irreversible_writes: u64,
    ok: bool,
}

impl ExecutionReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"{}\",\"backend\":\"{}\",\"iterations\":{},\"wall_ns\":{},\"fuel_spent\":{},\"fuel_per_sec\":{:.1},\"heap_peak_bytes\":{},\"host_calls\":{},\"irreversible_writes\":{},\"ok\":{}}}",
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
            self.ok,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn bench_execution(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    name: &'static str,
    iterations: u32,
    backend: Backend,
    ir: Option<&Arc<cove_ir::Program>>,
) -> Result<ExecutionReport, String> {
    let (module, entry, allow) = entry_for(package, program, name)?;

    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut heap_peak = Vec::with_capacity(iterations as usize);
    let mut fuel_spent = 0;
    let mut host_calls = 0;
    let mut irreversible_writes = 0;
    let mut ok = true;

    for _ in 0..iterations {
        let measurement = run_once(
            program,
            sources,
            module,
            entry,
            &allow,
            Arc::new(NullSink),
            ir,
        );
        wall_ns.push(measurement.wall.as_nanos() as u64);
        heap_peak.push(measurement.heap.peak_bytes);
        fuel_spent = measurement.fuel_spent;
        host_calls = measurement.host_calls;
        irreversible_writes = measurement.irreversible_writes;
        if let Some(message) = measurement.failure {
            eprintln!("cove-bench: benchmark `{name}` on {backend} did not pass: {message}");
            ok = false;
        }
    }

    let wall = Stats::of(&wall_ns);
    let fuel_per_sec = if wall.mean() > 0 {
        fuel_spent as f64 / (wall.mean() as f64 / 1e9)
    } else {
        0.0
    };

    Ok(ExecutionReport {
        benchmark: name,
        backend,
        iterations,
        wall_ns: wall,
        fuel_spent,
        fuel_per_sec,
        heap_peak_bytes: Stats::of(&heap_peak),
        host_calls,
        irreversible_writes,
        ok,
    })
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

#[allow(clippy::too_many_arguments)]
fn bench_trace_overhead(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    name: &'static str,
    iterations: u32,
    backend: Backend,
    ir: Option<&Arc<cove_ir::Program>>,
) -> Result<TraceOverheadReport, String> {
    let (module, entry, allow) = entry_for(package, program, name)?;

    let mut untraced = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let m = run_once(
            program,
            sources,
            module,
            entry,
            &allow,
            Arc::new(NullSink),
            ir,
        );
        untraced.push(m.wall.as_nanos() as u64);
    }

    let mut traced = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let header = TraceHeader {
            // The backend this measurement is of, which is the one the
            // recording would have been made on had it been kept.
            backend: match backend {
                Backend::Ast => RecordingBackend::Ast,
                Backend::Vm => RecordingBackend::Vm,
            },
            values: ValueCapture::Redacted,
            entry: format!("{module}.{entry}"),
            args: Vec::new(),
        };
        let sink: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(std::io::sink(), header));
        let m = run_once(program, sources, module, entry, &allow, sink, ir);
        traced.push(m.wall.as_nanos() as u64);
    }

    let untraced_mean = Stats::of(&untraced).mean();
    let traced_mean = Stats::of(&traced).mean();
    let overhead_ratio = if untraced_mean > 0 {
        traced_mean as f64 / untraced_mean as f64
    } else {
        1.0
    };

    Ok(TraceOverheadReport {
        benchmark: name,
        backend,
        untraced_wall_ns: untraced_mean,
        traced_wall_ns: traced_mean,
        overhead_ratio,
    })
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
