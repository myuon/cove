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
//! {"benchmark":"benches","kind":"lowering","backend":"vm","iterations":<u32>,"wall_ns":{"min":<u64>,"mean":<u64>,"max":<u64>},"functions":<usize>,"ok":<bool>}
//! {"benchmark":"pure","kind":"interpreter","backend":"ast","iterations":<u32>,"wall_ns":{"min":<u64>,"mean":<u64>,"max":<u64>},"fuel_spent":<u64>,"fuel_per_sec":<f64>,"heap_peak_bytes":{"min":<u64>,"mean":<u64>,"max":<u64>},"host_calls":<u64>,"irreversible_writes":<u64>,"ok":<bool>}
//! {"benchmark":"pure","kind":"vm","backend":"vm", ...the same fields...}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"ast","untraced_wall_ns":<u64>,"traced_wall_ns":<u64>,"overhead_ratio":<f64>}
//! {"benchmark":"pure","kind":"trace_overhead","backend":"vm", ...the same fields...}
//! {"benchmark":"hostheavy", ...the same four lines...}
//! ... and the same four for each of `arith`, `arrayget`, `field`, `method`,
//! `call`, and `chars`
//! {"benchmark":"startup","kind":"process","backend":"ast","iterations":<u32>,"wall_ns":{"min":<u64>,"mean":<u64>,"max":<u64>},"ok":<bool>}
//! {"benchmark":"startup","kind":"process","backend":"vm", ...the same fields...}
//! ```
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
//! recorded baseline, should refuse numbers from a run that is not `ok`.
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
//! cargo build --workspace
//! ./target/debug/cove-bench                      # a handful of iterations: what CI runs, for correctness
//! ./target/debug/cove-bench --iterations 200      # a real local measurement
//! ```
//!
//! Reading one backend against the other is what the output is arranged for:
//! the two `wall_ns` means of one benchmark are the comparison, and the
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
    HostRegistry, JsonlSink, Limits, NullSink, Process, ProcessLog, Runtime, TraceHeader,
    TraceSink, Value, ValueCapture, VirtualTime,
};
use cove_sema::package::Package;
use cove_sema::resolve::Program;

/// How many times each benchmark runs when `--iterations` is not given.
///
/// Small enough that the whole harness finishes in well under a second, so
/// CI can run it for correctness on every push without becoming the slow
/// step in the pipeline; see ADR 0012.
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

    let (sources, package, program) = match load_benches() {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("cove-bench: {message}");
            return ExitCode::FAILURE;
        }
    };
    let sources = Arc::new(sources);
    let program = Arc::new(program);

    let mut ok = true;

    // One lowering of the whole package, timed on its own, because that is
    // both what `cove run --backend vm` does and the only honest place to put
    // a cost that is paid once per program rather than once per run.
    let lowered = bench_lowering(&program, iterations);
    match &lowered {
        Ok(report) => println!("{}", report.to_json()),
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
            }
            Err(message) => {
                eprintln!("cove-bench: startup on {backend}: {message}");
                ok = false;
            }
        }
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
    ir: cove_ir::Program,
}

impl LoweringReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"benches\",\"kind\":\"lowering\",\"backend\":\"vm\",\"iterations\":{},\"wall_ns\":{},\"functions\":{},\"ok\":true}}",
            self.iterations,
            self.wall_ns.to_json(),
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
        ir,
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
    hosts.register(Box::new(Console::new(std::io::sink())));
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
    ir: Option<&cove_ir::Program>,
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
    let (outcome, heap) = match ir {
        Some(ir) => {
            let mut vm = Vm::new(&runtime, &hosts, ir);
            let outcome = vm.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, vm.heap_stats())
        }
        None => {
            let mut interpreter = Interpreter::new(&runtime);
            let outcome = interpreter.run_entry(module, entry, Vec::<Rc<str>>::new());
            (outcome, interpreter.heap_stats())
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
        failure,
    }
}

/// `Some(message)` when `value` is the `Err` an entry returned; `None` for
/// `Ok` or an entry that returns bare `()`.
fn entry_err_message(value: &Value) -> Option<String> {
    let Value::Enum(result) = value else {
        return None;
    };
    if &*result.type_name != "Result" || &*result.case != "Err" {
        return None;
    }
    Some(
        result
            .payload
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// The minimum, mean, and maximum of a series of samples -- enough to see
/// whether a run was stable without carrying every sample into the report.
struct Stats {
    min: u64,
    mean: u64,
    max: u64,
}

impl Stats {
    fn of(samples: &[u64]) -> Stats {
        let min = *samples.iter().min().expect("at least one sample");
        let max = *samples.iter().max().expect("at least one sample");
        let sum: u128 = samples.iter().map(|&n| u128::from(n)).sum();
        let mean = (sum / samples.len() as u128) as u64;
        Stats { min, mean, max }
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"min\":{},\"mean\":{},\"max\":{}}}",
            self.min, self.mean, self.max
        )
    }
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
            self.wall_ns.to_json(),
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
    ir: Option<&cove_ir::Program>,
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
    let fuel_per_sec = if wall.mean > 0 {
        fuel_spent as f64 / (wall.mean as f64 / 1e9)
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
    ir: Option<&cove_ir::Program>,
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
            values: ValueCapture::Redacted,
            entry: format!("{module}.{entry}"),
            args: Vec::new(),
        };
        let sink: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(std::io::sink(), header));
        let m = run_once(program, sources, module, entry, &allow, sink, ir);
        traced.push(m.wall.as_nanos() as u64);
    }

    let untraced_mean = Stats::of(&untraced).mean;
    let traced_mean = Stats::of(&traced).mean;
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
            self.wall_ns.to_json(),
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
