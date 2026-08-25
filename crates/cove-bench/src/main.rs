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
//! # Output
//!
//! One JSON object per line on stdout, in the order the benchmarks ran:
//!
//! ```text
//! {"benchmark":"pure","kind":"interpreter","iterations":<u32>,"wall_ns":{"min":<u64>,"mean":<u64>,"max":<u64>},"fuel_spent":<u64>,"fuel_per_sec":<f64>,"heap_peak_bytes":{"min":<u64>,"mean":<u64>,"max":<u64>},"host_calls":<u64>,"irreversible_writes":<u64>,"ok":<bool>}
//! {"benchmark":"pure","kind":"trace_overhead","untraced_wall_ns":<u64>,"traced_wall_ns":<u64>,"overhead_ratio":<f64>}
//! {"benchmark":"hostheavy","kind":"interpreter", ...same shape as "pure" above...}
//! {"benchmark":"hostheavy","kind":"trace_overhead", ...same shape as "pure" above...}
//! {"benchmark":"startup","kind":"process","iterations":<u32>,"wall_ns":{"min":<u64>,"mean":<u64>,"max":<u64>},"ok":<bool>}
//! ```
//!
//! `ok` is `false` when a benchmark's entry returned `Err`, the interpreter
//! itself failed, or (for `startup`) the spawned process exited non-zero. A
//! caller comparing a future backend against a recorded baseline should
//! refuse numbers from a run that is not `ok`.
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

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
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

    for name in ["pure", "hostheavy"] {
        match bench_interpreter(&package, &program, &sources, name, iterations) {
            Ok(report) => {
                ok &= report.ok;
                println!("{}", report.to_json());
            }
            Err(message) => {
                eprintln!("cove-bench: benchmark `{name}`: {message}");
                ok = false;
            }
        }

        match bench_trace_overhead(&package, &program, &sources, name, iterations) {
            Ok(report) => println!("{}", report.to_json()),
            Err(message) => {
                eprintln!("cove-bench: benchmark `{name}` (trace overhead): {message}");
                ok = false;
            }
        }
    }

    match bench_startup(iterations) {
        Ok(report) => {
            ok &= report.ok;
            println!("{}", report.to_json());
        }
        Err(message) => {
            eprintln!("cove-bench: startup: {message}");
            ok = false;
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
    let program = cove_sema::resolve::resolve(&package).map_err(|items| {
        format!(
            "`{}` does not resolve:\n{}",
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

/// Builds a fresh registry, budget, and interpreter -- exactly what `cove
/// run` builds for one run -- and calls `module.entry` once under `trace`.
fn run_once(
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    module: &str,
    entry: &str,
    allow: &[String],
    trace: Arc<dyn TraceSink>,
) -> RunMeasurement {
    let mut hosts = fake_hosts(allow.to_vec());
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(trace.clone());

    let runtime =
        Runtime::new(Arc::clone(program), Arc::clone(sources), Arc::new(hosts)).with_trace(trace);
    let mut interpreter = Interpreter::new(&runtime);

    let started = Instant::now();
    let outcome = interpreter.run_entry(module, entry, Vec::<Rc<str>>::new());
    let wall = started.elapsed();
    let heap = interpreter.heap_stats();

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

/// One benchmark's interpreter-level report: wall time, fuel spent (the
/// interpreter's own normalized work counter, unaffected by machine noise),
/// and the heap's peak live bytes.
struct InterpreterReport {
    benchmark: &'static str,
    iterations: u32,
    wall_ns: Stats,
    fuel_spent: u64,
    fuel_per_sec: f64,
    heap_peak_bytes: Stats,
    host_calls: u64,
    irreversible_writes: u64,
    ok: bool,
}

impl InterpreterReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"interpreter\",\"iterations\":{},\"wall_ns\":{},\"fuel_spent\":{},\"fuel_per_sec\":{:.1},\"heap_peak_bytes\":{},\"host_calls\":{},\"irreversible_writes\":{},\"ok\":{}}}",
            self.benchmark,
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

fn bench_interpreter(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    name: &'static str,
    iterations: u32,
) -> Result<InterpreterReport, String> {
    let (module, entry, allow) = entry_for(package, program, name)?;

    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut heap_peak = Vec::with_capacity(iterations as usize);
    let mut fuel_spent = 0;
    let mut host_calls = 0;
    let mut irreversible_writes = 0;
    let mut ok = true;

    for _ in 0..iterations {
        let measurement = run_once(program, sources, module, entry, &allow, Arc::new(NullSink));
        wall_ns.push(measurement.wall.as_nanos() as u64);
        heap_peak.push(measurement.heap.peak_bytes);
        fuel_spent = measurement.fuel_spent;
        host_calls = measurement.host_calls;
        irreversible_writes = measurement.irreversible_writes;
        if let Some(message) = measurement.failure {
            eprintln!("cove-bench: benchmark `{name}` did not pass: {message}");
            ok = false;
        }
    }

    let wall = Stats::of(&wall_ns);
    let fuel_per_sec = if wall.mean > 0 {
        fuel_spent as f64 / (wall.mean as f64 / 1e9)
    } else {
        0.0
    };

    Ok(InterpreterReport {
        benchmark: name,
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
    untraced_wall_ns: u64,
    traced_wall_ns: u64,
    overhead_ratio: f64,
}

impl TraceOverheadReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"{}\",\"kind\":\"trace_overhead\",\"untraced_wall_ns\":{},\"traced_wall_ns\":{},\"overhead_ratio\":{:.3}}}",
            self.benchmark, self.untraced_wall_ns, self.traced_wall_ns, self.overhead_ratio
        )
    }
}

fn bench_trace_overhead(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    name: &'static str,
    iterations: u32,
) -> Result<TraceOverheadReport, String> {
    let (module, entry, allow) = entry_for(package, program, name)?;

    let mut untraced = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let m = run_once(program, sources, module, entry, &allow, Arc::new(NullSink));
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
        let m = run_once(program, sources, module, entry, &allow, sink);
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
        untraced_wall_ns: untraced_mean,
        traced_wall_ns: traced_mean,
        overhead_ratio,
    })
}

/// Process-level startup: spawns the real `cove` binary and times the whole
/// exec-to-exit span, which is what an in-process measurement cannot see.
struct StartupReport {
    iterations: u32,
    wall_ns: Stats,
    ok: bool,
}

impl StartupReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"benchmark\":\"startup\",\"kind\":\"process\",\"iterations\":{},\"wall_ns\":{},\"ok\":{}}}",
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

fn bench_startup(iterations: u32) -> Result<StartupReport, String> {
    let cove = cove_binary()?;
    let root = benches_root();

    let mut wall_ns = Vec::with_capacity(iterations as usize);
    let mut ok = true;

    for _ in 0..iterations {
        let started = Instant::now();
        let status = Command::new(&cove)
            .arg("run")
            .arg("startup")
            .current_dir(&root)
            .status()
            .map_err(|e| format!("cannot run `{}`: {e}", cove.display()))?;
        wall_ns.push(started.elapsed().as_nanos() as u64);
        ok &= status.success();
    }

    Ok(StartupReport {
        iterations,
        wall_ns: Stats::of(&wall_ns),
        ok,
    })
}
