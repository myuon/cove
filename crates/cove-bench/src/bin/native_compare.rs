//! `cove-native-compare`: which native code generator, measured rather than
//! argued.
//!
//! ADR 0055 names Cranelift for Cove's first native code generator. This is the
//! bounded experiment that checks that choice against the cheapest alternative
//! — a hand-written x86-64 template compiler — on **identical optimized Cove
//! IR**, and it is an experiment and not a product: nothing here is reachable
//! from `cove run`, and neither arm is wired to the runtime.
//!
//! # What it measures, and what it refuses to mix
//!
//! Four numbers, reported separately because they are four different costs and
//! a total would hide which one moved:
//!
//! - **JIT init**: making a code generator, once per process.
//! - **compile**: one function, from IR to executable code, including whatever
//!   the arm's W^X finalization costs.
//! - **code size**: the bytes of machine code that came out.
//! - **execution**: entering the compiled function and running it to its
//!   return, with the VM's own run of the same IR beside it.
//!
//! # The answer is checked every single time
//!
//! Every run of every arm is compared against the VM's answer for that
//! iteration, and a disagreement aborts the process. A performance number from
//! a run that computed the wrong thing is worse than no number, because it
//! looks like a number.
//!
//! # The arms are interleaved
//!
//! One iteration runs the VM, then Cranelift, then the template arm, and then
//! the next iteration does the same. All of one arm followed by all of another
//! measures the machine's drift as if it were the arms' difference; this way a
//! thermal or scheduling change lands on all three.
//!
//! # What is compiled, and why it is not `arith`'s `main`
//!
//! `benches/arith/main.cove` is the loop this comparison is about, and it is
//! lowered here exactly as `cove run --backend vm` lowers it, so its IR
//! instruction count and its VM time are this program's own. But its `main`
//! ends in `assertEqual(total, 285715)?` and `Ok(())`, which are a builtin
//! call, an enum construction and a `Result` frame slot — three things outside
//! the subset both arms compile, and ADR 0055 refuses a function whole rather
//! than splitting it. So `main` compiles on neither arm, and a three-arm race
//! over it is not available.
//!
//! What is raced instead is **arith's loop, taken from arith's own source**:
//! the text between `var total = 0` and the `assertEqual` is read out of
//! `benches/arith/main.cove` at run time and wrapped in a function that returns
//! `total`. Nothing about the loop is retyped here, so the raced code cannot
//! drift from the benchmark; if the file changes shape, the extraction fails
//! loudly rather than racing something else. The VM runs that same lowered IR,
//! and is the oracle for it.

use std::sync::Arc;
use std::time::Instant;

use cove_diag::SourceMap;
use cove_runtime::{
    Budget, Cancellation, Clock, Console, Database, Documents, Env, Files, Grants, HostRegistry,
    Limits, NullSink, Process, ProcessLog, Runtime, VirtualTime, Vm,
};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;
use cove_sema::HostSchemas;

/// How many iterations each arm runs when `--iterations` is not given.
const DEFAULT_ITERATIONS: u32 = 20;

/// The module the extracted loop is compiled as.
const LOOP_MODULE: &str = "m";
/// The function the extracted loop becomes.
const LOOP_ENTRY: &str = "arithLoop";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cove-native-compare: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let iterations = flag_u32("--iterations").unwrap_or(DEFAULT_ITERATIONS);
    let dump = flag("--dump");
    let disasm = flag("--disasm");
    let decompose = flag("--decompose");

    conditions(iterations);

    // `arith` itself: the program this comparison is about, lowered the way the
    // vm backend lowers it.
    let (sources, checked) = benches_package()?;
    let arith = cove_ir::lower_entry(&checked, &sources, &HostSchemas::new(), "arith", "main")
        .map_err(|items| render(&sources, &items))?;
    report_ir("arith.main, as `cove run --backend vm` lowers it", &arith);
    if dump {
        println!(
            "{}",
            cove_ir::print::function(&arith, entry_of(&arith, "arith", "main")?)
        );
    }
    refusals(&arith, entry_of(&arith, "arith", "main")?);

    // What the benchmark itself costs on the VM, for scale: the raced loop is
    // this program's loop, and this is the number it sits inside.
    let arith_sources = Arc::new(sources);
    let arith_checked = Arc::new(checked);
    let arith = Arc::new(arith);
    let whole: Vec<u64> = (0..3)
        .map(|_| time_arith_main(&arith_checked, &arith_sources, &arith))
        .collect::<Result<Vec<u64>, String>>()?;
    println!(
        "  arith.main on the vm, 3 runs: fastest {}",
        Scale::Millis.of(whole.iter().copied().min().unwrap_or(0))
    );
    println!();

    // The raced unit: arith's loop, out of arith's own file.
    let source = loop_source()?;
    let (loop_sources, loop_checked) = check_one_module(&source)?;
    let ir = cove_ir::lower_entry(
        &loop_checked,
        &loop_sources,
        &HostSchemas::new(),
        LOOP_MODULE,
        LOOP_ENTRY,
    )
    .map_err(|items| render(&loop_sources, &items))?;
    let id = entry_of(&ir, LOOP_MODULE, LOOP_ENTRY)?;
    report_ir("the raced unit: arith's loop", &ir);
    if dump {
        println!("{}", cove_ir::print::function(&ir, id));
    }
    let ir = Arc::new(ir);
    let checked = Arc::new(loop_checked);
    let loop_sources = Arc::new(loop_sources);

    // One lowering, three arms. Compilation and initialization first, because
    // the execution loop wants the compiled entry points in hand.
    let arms = Arms::new(&ir, id)?;
    arms.report();
    if disasm {
        arms.hexdump();
    }

    let mut vm = Vec::with_capacity(iterations as usize);
    let mut native: Vec<(&'static str, Vec<u64>)> = arms
        .names()
        .into_iter()
        .map(|name| (name, Vec::with_capacity(iterations as usize)))
        .collect();

    for iteration in 0..iterations {
        let (nanos, answer) = time_vm(&checked, &loop_sources, &ir)?;
        vm.push(nanos);
        for (arm, samples) in &mut native {
            let (nanos, mine) = arms.time(arm, id)?;
            if mine != answer {
                return Err(format!(
                    "{arm} answered {mine} on iteration {iteration} and the VM answered \
                     {answer}; a run whose answer differs is a failure, not a data point"
                ));
            }
            samples.push(nanos);
        }
    }

    println!();
    heading(&format!(
        "execution, {iterations} iterations, interleaved, one entry each"
    ));
    line("vm", &vm, Scale::Millis);
    for (arm, samples) in &native {
        line(arm, samples, Scale::Millis);
    }
    println!();
    println!("every arm's answer was checked against the VM's on every iteration");

    println!();
    covefmt(iterations, dump, decompose)?;
    Ok(())
}

/// One row of a table: the first sample apart from the rest.
///
/// The cold run is reported on its own because it is not a sample of the same
/// thing — it pays the first touch of the code page, the frame and the caches.
/// Every other column is over the warm samples only.
fn line(name: &str, samples: &[u64], unit: Scale) {
    let cold = samples.first().copied().unwrap_or(0);
    let warm = if samples.len() > 1 {
        &samples[1..]
    } else {
        samples
    };
    let mut sorted = warm.to_vec();
    sorted.sort_unstable();
    let min = sorted.first().copied().unwrap_or(0);
    let max = sorted.last().copied().unwrap_or(0);
    let median = if sorted.is_empty() {
        0
    } else {
        sorted[sorted.len() / 2]
    };
    let mean = if sorted.is_empty() {
        0
    } else {
        sorted.iter().sum::<u64>() / sorted.len() as u64
    };
    println!(
        "  {name:<18} {:<11} {:<11} {:<11} {:<11} {:<11}",
        unit.of(cold),
        unit.of(min),
        unit.of(median),
        unit.of(mean),
        unit.of(max)
    );
}

/// What a column of a table is counted in.
///
/// Two scales because the costs are four orders of magnitude apart: a
/// compilation is microseconds and an execution is milliseconds, and printing
/// both in one unit would report one of them as `0.000` or as a wall of digits.
#[derive(Clone, Copy)]
enum Scale {
    /// Only a native arm reports anything this small, so a build with no arm
    /// never constructs it.
    #[cfg_attr(
        not(any(feature = "cranelift", feature = "template")),
        allow(dead_code)
    )]
    Micros,
    Millis,
}

impl Scale {
    fn of(self, nanos: u64) -> String {
        match self {
            Scale::Micros => format!("{:.3}us", nanos as f64 / 1_000.0),
            Scale::Millis => format!("{:.3}ms", nanos as f64 / 1_000_000.0),
        }
    }
}

fn heading(what: &str) {
    println!("{what}");
    println!(
        "  {:<18} {:<11} {:<11} {:<11} {:<11} {:<11}",
        "arm", "cold", "min", "median", "mean", "max"
    );
}

/// What the numbers below are facts about.
fn conditions(iterations: u32) {
    println!("conditions");
    println!("  cpu             {}", cpu());
    println!(
        "  os / arch       {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("  binary          {}", exe());
    println!("  rustc           {}", shell("rustc", &["-V"]));
    println!("  commit          {}", shell("git", &["rev-parse", "HEAD"]));
    println!(
        "  dirty           {}",
        if shell("git", &["status", "--porcelain"]).is_empty() {
            "no"
        } else {
            "yes (uncommitted changes)"
        }
    );
    println!(
        "  load average    {}",
        shell("sysctl", &["-n", "vm.loadavg"])
    );
    println!("  iterations      {iterations}");
    println!();
}

fn cpu() -> String {
    let brand = shell("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if brand.is_empty() {
        shell("uname", &["-m"])
    } else {
        brand
    }
}

fn exe() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

fn shell(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

// --- the IR ------------------------------------------------------------------

/// How many IR instructions a lowering came to.
///
/// Over the functions the entry actually reached: `cove_ir::lower_entry` gives
/// every declaration of the package a table entry and stubs the ones no path
/// leads to, and a stub is not code this or any arm would compile.
fn report_ir(what: &str, program: &cove_ir::Program) {
    let functions: Vec<&cove_ir::Function> = program
        .functions
        .iter()
        .filter(|function| !function.is_stub())
        .collect();
    let instructions: usize = functions.iter().map(|function| function.code.len()).sum();
    println!("{what}");
    println!(
        "  {instructions} IR instructions after lowering, across {} lowered functions \
         ({} table entries, the rest stubs)",
        functions.len(),
        program.functions.len()
    );
}

fn entry_of(
    program: &cove_ir::Program,
    module: &str,
    name: &str,
) -> Result<cove_ir::FunctionId, String> {
    program
        .function_named(module, name)
        .ok_or_else(|| format!("the lowering has no `{module}.{name}`"))
}

/// Which arms refuse `arith.main`, which is both of them.
///
/// Reported rather than assumed, because the whole reason the raced unit is the
/// loop and not `main` is this refusal.
#[allow(unused_variables)]
fn refusals(program: &cove_ir::Program, id: cove_ir::FunctionId) {
    #[allow(unused_mut)]
    let mut said: Vec<(&str, bool)> = Vec::new();
    #[cfg(feature = "cranelift")]
    {
        let mut jit = cove_native::Jit::new(helpers()).expect("this host supports Cranelift");
        said.push(("cranelift", jit.compile(program, id).is_some()));
    }
    #[cfg(feature = "template")]
    {
        let mut jit = cove_native::template::Jit::new(helpers()).expect("this host is x86-64");
        said.push(("template", jit.compile(program, id).is_some()));
    }
    for (arm, compiled) in said {
        println!(
            "  {arm}: {}",
            if compiled {
                "compiles it"
            } else {
                "refuses it, whole, as ADR 0055 requires"
            }
        );
    }
    println!();
}

// --- the program under test --------------------------------------------------

/// `benches/`, checked.
fn benches_package() -> Result<(SourceMap, Checked), String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches");
    let mut sources = SourceMap::new();
    let package =
        cove_sema::package::load(&root, &mut sources).map_err(|items| render(&sources, &items))?;
    let checked = cove_sema::Compiler::new()
        .compile(&package)
        .map_err(|items| render(&sources, &items))?;
    Ok((sources, checked))
}

/// arith's loop, out of arith's own file, wrapped in a function that returns it.
///
/// The extraction is deliberately brittle in the safe direction: every landmark
/// it depends on is asserted, so a benchmark that changed shape stops this
/// program instead of racing something that is no longer arith's loop.
fn loop_source() -> Result<String, String> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches/arith/main.cove");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let body = text
        .split_once("export fn main() -> Result<Unit, Error> {\n")
        .ok_or("benches/arith/main.cove no longer declares `export fn main`")?
        .1;
    let body = body
        .split_once("  assertEqual(")
        .ok_or("benches/arith/main.cove no longer ends its loop at an `assertEqual`")?
        .0;
    if !body.contains("while i < 2000000") || !body.contains("i % 7 == 0") {
        return Err("benches/arith/main.cove's loop is not the loop this raced".to_string());
    }
    Ok(format!(
        "export fn {LOOP_ENTRY}() -> Int {{\n{body}  total\n}}\n"
    ))
}

/// One module of source, parsed, resolved and type-checked the way `cove run`
/// does it.
fn check_one_module(source: &str) -> Result<(SourceMap, Checked), String> {
    let mut sources = SourceMap::new();
    let path = std::path::PathBuf::from(format!("{LOOP_MODULE}/main.cove"));
    let file = sources.add(path.clone(), source.to_string());
    let ast = cove_syntax::parse_file(&sources, file).map_err(|items| render(&sources, &items))?;
    let mut modules = std::collections::BTreeMap::new();
    modules.insert(
        LOOP_MODULE.to_string(),
        Module {
            name: LOOP_MODULE.to_string(),
            dir: std::path::PathBuf::from(LOOP_MODULE),
            units: vec![Unit { file, path, ast }],
        },
    );
    for (name, module) in
        cove_sema::stdlib::attach(&mut sources).map_err(|items| render(&sources, &items))?
    {
        modules.insert(name, module);
    }
    let package = Package {
        root: std::path::PathBuf::from("."),
        config: Config::default(),
        modules,
    };
    let checked = cove_sema::Compiler::new()
        .compile(&package)
        .map_err(|items| render(&sources, &items))?;
    Ok((sources, checked))
}

fn render(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

// --- the VM arm --------------------------------------------------------------

/// One run of `arith.main` itself on the VM, timed.
///
/// The whole benchmark, assertion and `Result` and all, so that the raced loop's
/// number has the program it came from beside it.
fn time_arith_main(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    ir: &Arc<cove_ir::Program>,
) -> Result<u64, String> {
    let mut hosts = fake_hosts();
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(Arc::new(NullSink));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(Arc::clone(checked), Arc::clone(sources), Arc::clone(&hosts));
    let mut vm = Vm::new(&runtime, &hosts, ir);
    let started = Instant::now();
    let outcome = vm.run_entry("arith", "main", Vec::new());
    let elapsed = started.elapsed();
    outcome.map_err(|error| format!("arith.main failed: {}", error.message))?;
    Ok(elapsed.as_nanos() as u64)
}

/// One run of the lowered IR on the linear-memory VM, timed.
///
/// Everything a run needs is built outside the timer, exactly as `cove-bench`
/// builds it: the same deterministic fake hosts, the same default budget. What
/// is timed is the call.
fn time_vm(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    ir: &Arc<cove_ir::Program>,
) -> Result<(u64, i64), String> {
    let mut hosts = fake_hosts();
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(Arc::new(NullSink));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(Arc::clone(checked), Arc::clone(sources), Arc::clone(&hosts));
    let mut vm = Vm::new(&runtime, &hosts, ir);

    let started = Instant::now();
    let outcome = vm.invoke(LOOP_MODULE, LOOP_ENTRY, Vec::new());
    let elapsed = started.elapsed();

    let value =
        outcome.map_err(|error| format!("the VM refused the raced unit: {}", error.message))?;
    let answer = value
        .as_int()
        .ok_or_else(|| "the raced unit did not answer an `Int`".to_string())?;
    Ok((elapsed.as_nanos() as u64, answer))
}

/// The same deterministic fakes `cove-bench` grants, and no capability at all:
/// the raced unit reaches no Host.
fn fake_hosts() -> HostRegistry {
    let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
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

// --- the native arms ---------------------------------------------------------

/// The runtime's half of the boundary, as the experiment's double.
///
/// Both arms call *this* function, at the same places, with the same accumulated
/// work count, so the safepoint is a constant of the comparison rather than a
/// difference between the arms. A real one is where ADR 0040's three-step stop
/// order lives; this one answers "carry on" and is the floor of what a poll can
/// cost.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn safepoint(_ctx: *mut cove_native::NativeCtx, _pc: u32, _work: u64) -> bool {
    true
}

/// The call helper the *arith* scenario binds, which nothing reaches.
///
/// arith's loop holds no call — the whole reason the raced unit is the loop and
/// not `main` is that `main`'s `assertEqual` is one — and this arm is entered with
/// no runtime behind it: `ctx.host` is null, because the frame is a `Vec<u64>`
/// this program owns and there is no `Machine` at all. So a real helper could not
/// be bound here, and this one answers "raised", which the entry below asserts
/// against. A call that was somehow reached fails the run loudly rather than
/// answering something.
///
/// The covefmt scenario binds `cove_runtime::native_helpers()`, which is the real
/// pair, over a real machine.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_call(
    _ctx: *mut cove_native::NativeCtx,
    _base: u64,
    _pc: u32,
    _callee: u32,
    _args: u32,
    _dst: u32,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

/// The open half, for the *arith* scenario, which reaches no call either.
///
/// `no_call`'s reason, one step along: arith's loop holds no call, so a direct
/// one cannot be reached, and this answers "the runtime finished it, and it
/// raised" — which the entry below asserts against.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_open(
    _ctx: *mut cove_native::NativeCtx,
    _base: u64,
    _pc: u32,
    _callee: u32,
    _args: u32,
    _dst: u32,
) -> cove_native::Opened {
    cove_native::Opened {
        entry: None,
        base: u64::from(cove_native::Outcome::Raised.abi()),
    }
}

/// The close half, for a call that cannot happen. See [`no_open`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_close(
    _ctx: *mut cove_native::NativeCtx,
    outcome: u32,
    _callee: u32,
) -> u32 {
    outcome
}

/// The allocation helper, for a scenario that allocates nothing. See [`no_call`].
///
/// arith's loop is `total += i` and holds no allocation, so this cannot be
/// reached; nought is the ABI's "refused, and the runtime is holding the error",
/// which the entry below asserts against the same way it asserts a call.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_alloc(
    _ctx: *mut cove_native::NativeCtx,
    _pc: u32,
    _layout: u32,
    _len: i64,
) -> u64 {
    0
}

/// The builtin helper, for a scenario that calls none. See [`no_call`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_builtin(
    _ctx: *mut cove_native::NativeCtx,
    _base: u64,
    _pc: u32,
    _dst: u32,
    _builtin: u32,
    _args: u32,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

/// The growable-run helper, for a scenario that builds none. See [`no_call`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_growable(
    _ctx: *mut cove_native::NativeCtx,
    _base: u64,
    _pc: u32,
    _op: u32,
    _a: u32,
    _b: u32,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

/// The run-copy helper, for a scenario that copies no run. See [`no_call`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_run_copy(
    _ctx: *mut cove_native::NativeCtx,
    _base: u64,
    _pc: u32,
    _args: u32,
    _words: u32,
    _elem: u32,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

/// The byte copy under an emitted append, for a scenario that appends nothing.
/// See [`no_call`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_copy_bytes(
    _ctx: *mut cove_native::NativeCtx,
    _dst: u64,
    _dst_at: u64,
    _src: u64,
    _src_at: u64,
    _len: u64,
) {
}

/// The field-access helpers, for a scenario that loads and stores no field.
/// See [`no_call`].
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_field_load(
    _ctx: *mut cove_native::NativeCtx,
    _pc: u32,
    _addr: u64,
    _at: u32,
    _width: u32,
    _into: u64,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

/// [`no_field_load`], the other direction.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
#[cfg(any(feature = "cranelift", feature = "template"))]
unsafe extern "C" fn no_field_store(
    _ctx: *mut cove_native::NativeCtx,
    _pc: u32,
    _addr: u64,
    _at: u32,
    _width: u32,
    _from: u64,
) -> u32 {
    cove_native::Outcome::Raised.abi()
}

#[cfg(any(feature = "cranelift", feature = "template"))]
fn helpers() -> cove_native::NativeHelpers {
    cove_native::NativeHelpers {
        safepoint,
        call: no_call,
        open: no_open,
        close: no_close,
        alloc: no_alloc,
        builtin: no_builtin,
        growable: no_growable,
        run_copy: no_run_copy,
        copy_bytes: no_copy_bytes,
        field_load: no_field_load,
        field_store: no_field_store,
    }
}

/// Every arm that was compiled in, with its initialization and compilation
/// already measured.
struct Arms {
    /// How many words the raced function's frame is.
    frame: u32,
    #[cfg(feature = "cranelift")]
    cranelift: (cove_native::Jit, cove_native::Compiled, Measured),
    #[cfg(feature = "template")]
    template: (
        cove_native::template::Jit,
        cove_native::template::Compiled,
        Measured,
    ),
}

/// What making and using a code generator cost, apart from execution.
#[cfg(any(feature = "cranelift", feature = "template"))]
struct Measured {
    init: Vec<u64>,
    compile: Vec<u64>,
    code_bytes: u32,
}

#[cfg(any(feature = "cranelift", feature = "template"))]
impl Measured {
    /// The compile samples alone, for the second scenario: it compiles every
    /// function of a slice once rather than one function ten times, so it has no
    /// initialization samples of its own and no one code size.
    fn of(compile: Vec<u64>) -> Measured {
        Measured {
            init: Vec::new(),
            compile,
            code_bytes: 0,
        }
    }

    fn report(&self, arm: &str) {
        line(&format!("{arm} init"), &self.init, Scale::Micros);
        line(&format!("{arm} compile"), &self.compile, Scale::Micros);
    }
}

/// How many times initialization and compilation are sampled.
///
/// Enough for a minimum to mean something, and not so many that the report is
/// about them: they are paid once per process and once per function, and the
/// thing under test is execution.
#[cfg(any(feature = "cranelift", feature = "template"))]
const SETUP_SAMPLES: usize = 10;

impl Arms {
    #[allow(unused_variables, clippy::let_and_return)]
    fn new(ir: &Arc<cove_ir::Program>, id: cove_ir::FunctionId) -> Result<Arms, String> {
        let frame = ir.function(id).frame_size();
        let arms = Arms {
            frame,
            #[cfg(feature = "cranelift")]
            cranelift: {
                let mut init = Vec::new();
                for _ in 0..SETUP_SAMPLES {
                    let started = Instant::now();
                    let jit = cove_native::Jit::new(helpers())
                        .map_err(|error| format!("cranelift: {error}"))?;
                    init.push(started.elapsed().as_nanos() as u64);
                    drop(jit);
                }
                let mut compile = Vec::new();
                let mut held = None;
                for _ in 0..SETUP_SAMPLES {
                    let mut jit = cove_native::Jit::new(helpers())
                        .map_err(|error| format!("cranelift: {error}"))?;
                    let started = Instant::now();
                    let compiled = jit
                        .compile(ir, id)
                        .ok_or("cranelift refused the raced unit")?;
                    jit.finalize()
                        .map_err(|error| format!("cranelift: {error}"))?;
                    compile.push(started.elapsed().as_nanos() as u64);
                    held = Some((jit, compiled));
                }
                let (jit, compiled) = held.expect("`SETUP_SAMPLES` is not zero");
                let measured = Measured {
                    init,
                    compile,
                    code_bytes: compiled.code_bytes,
                };
                (jit, compiled, measured)
            },
            #[cfg(feature = "template")]
            template: {
                let mut init = Vec::new();
                for _ in 0..SETUP_SAMPLES {
                    let started = Instant::now();
                    let jit = cove_native::template::Jit::new(helpers())
                        .map_err(|error| format!("template: {error}"))?;
                    init.push(started.elapsed().as_nanos() as u64);
                    drop(jit);
                }
                let mut compile = Vec::new();
                let mut held = None;
                for _ in 0..SETUP_SAMPLES {
                    let mut jit = cove_native::template::Jit::new(helpers())
                        .map_err(|error| format!("template: {error}"))?;
                    let started = Instant::now();
                    let compiled = jit
                        .compile(ir, id)
                        .ok_or("template refused the raced unit")?;
                    jit.finalize()
                        .map_err(|error| format!("template: {error}"))?;
                    compile.push(started.elapsed().as_nanos() as u64);
                    held = Some((jit, compiled));
                }
                let (jit, compiled) = held.expect("`SETUP_SAMPLES` is not zero");
                let measured = Measured {
                    init,
                    compile,
                    code_bytes: compiled.code_bytes,
                };
                (jit, compiled, measured)
            },
        };
        Ok(arms)
    }

    /// Which arms this build has, in the order one iteration runs them.
    ///
    /// Written as a `cfg`-selected slice rather than a built `Vec`, so that a
    /// build with one arm, two arms or none is the same three lines.
    fn names(&self) -> Vec<&'static str> {
        [
            #[cfg(feature = "cranelift")]
            "cranelift",
            #[cfg(feature = "template")]
            "template",
        ]
        .to_vec()
    }

    fn report(&self) {
        heading(&format!(
            "initialization and compilation, {SETUP_SAMPLES_OR_ZERO} samples each"
        ));
        #[cfg(feature = "cranelift")]
        self.cranelift.2.report("cranelift");
        #[cfg(feature = "template")]
        self.template.2.report("template");
        if self.names().is_empty() {
            println!("  no native arm was compiled in; build with `--features cranelift,template`");
        }
        #[cfg(feature = "cranelift")]
        println!(
            "  cranelift emitted {} bytes of machine code",
            self.cranelift.2.code_bytes
        );
        #[cfg(feature = "template")]
        println!(
            "  template  emitted {} bytes of machine code",
            self.template.2.code_bytes
        );
    }

    /// Every arm's machine code, as hex, for a disassembler outside this
    /// process.
    ///
    /// Read back out of the executable mapping rather than kept as a
    /// by-product of compilation, so what is printed is what will run.
    fn hexdump(&self) {
        #[cfg(feature = "cranelift")]
        hex(
            "cranelift",
            self.cranelift.0.entry(self.cranelift.1),
            self.cranelift.1.code_bytes,
        );
        #[cfg(feature = "template")]
        hex(
            "template",
            self.template.0.entry(self.template.1),
            self.template.1.code_bytes,
        );
    }

    /// One entry into one arm's compiled code, timed, and what it answered.
    ///
    /// The frame is fresh every time and the base is deliberately not zero: a
    /// slot address formed as if the frame began at word zero would read the
    /// wrong words and answer wrongly, which is a bug a comparison should not
    /// be the first thing to notice.
    #[allow(unused_variables)]
    fn time(&self, arm: &str, id: cove_ir::FunctionId) -> Result<(u64, i64), String> {
        let base: u64 = 8;
        // The frame, and one word past it for the destination: ADR 0057's entry
        // writes its answer where its caller says, and here the caller is this
        // function.
        #[allow(unused_mut)]
        let mut words = vec![0u64; self.frame as usize + base as usize + 1];
        match arm {
            #[cfg(feature = "cranelift")]
            "cranelift" => {
                let entry = self.cranelift.0.entry(self.cranelift.1);
                Ok(enter(entry, &mut words, base))
            }
            #[cfg(feature = "template")]
            "template" => {
                let entry = self.template.0.entry(self.template.1);
                Ok(enter(entry, &mut words, base))
            }
            other => Err(format!("there is no `{other}` arm in this build")),
        }
    }
}

#[cfg(any(feature = "cranelift", feature = "template"))]
const SETUP_SAMPLES_OR_ZERO: usize = SETUP_SAMPLES;
#[cfg(not(any(feature = "cranelift", feature = "template")))]
const SETUP_SAMPLES_OR_ZERO: usize = 0;

/// One compiled function's bytes, as hex on one line.
#[cfg(any(feature = "cranelift", feature = "template"))]
fn hex(arm: &str, entry: cove_native::Entry, bytes: u32) {
    // Safety: `entry` points at `bytes` bytes of readable, executable code that
    // the arm just emitted.
    let code = unsafe { std::slice::from_raw_parts(entry as usize as *const u8, bytes as usize) };
    println!("disasm {arm} {bytes}");
    for byte in code {
        print!("{byte:02x}");
    }
    println!();
}

/// Enters compiled code over `words` and reads the answer out of the destination.
///
/// The destination is the last word of `words`, which is one past the frame: the
/// entry is handed it as `return_base + return_slot` — the two indices ADR 0057
/// widened the ABI with — and has written the answer there by the time it
/// returns. Nothing is copied out of a reported slot, because there is no longer
/// one to report.
#[cfg(any(feature = "cranelift", feature = "template"))]
fn enter(entry: cove_native::Entry, words: &mut [u64], base: u64) -> (u64, i64) {
    let into = (words.len() - 1) as u64;
    let mut ctx = cove_native::NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr(), 0);
    let started = Instant::now();
    // Safety: `ctx.words` is `words`, `base` indexes into it, the frame the
    // lowering asked for fits inside what was allocated above, and `into` is one
    // word of it that the frame does not reach.
    let outcome = unsafe { entry(&mut ctx, base, into, 0) };
    let elapsed = started.elapsed();
    assert_eq!(
        outcome,
        cove_native::Outcome::Returned,
        "the raced unit returns; it raises nothing and its safepoint never stops"
    );
    let answer = words[into as usize] as i64;
    (elapsed.as_nanos() as u64, answer)
}

/// Every function of the slice the template arm admits, compiled against
/// `helpers`.
///
/// The one place that loop is written. It takes the helper table rather than
/// asking [`cove_runtime::native_helpers`] for it, because the decomposition
/// below compiles the same slice a dozen times over a dozen *ablated* tables and
/// the code is otherwise byte for byte the same — the only difference between
/// two arms of it is the address in the `movabs` that loads the call helper,
/// which is what makes the difference between two arms the difference between
/// the helpers and nothing else.
#[cfg(feature = "template")]
fn compile_slice(
    ir: &Arc<cove_ir::Program>,
    helpers: cove_native::NativeHelpers,
    direct: bool,
) -> Result<(cove_native::template::Jit, Tier, Measured), String> {
    let mut jit =
        cove_native::template::Jit::new(helpers).map_err(|error| format!("template: {error}"))?;
    if direct {
        jit = jit.calling_directly();
    }
    let mut tier = Tier {
        entries: vec![None; ir.functions.len()],
        compiled: 0,
        refused: Vec::new(),
        bytes: 0,
        bytes_of: std::collections::BTreeMap::new(),
        counts_helpers: false,
    };
    let mut compile = Vec::new();
    let mut done = Vec::new();
    for id in candidates(ir) {
        let started = Instant::now();
        let compiled = jit.compile(ir, id);
        let elapsed = started.elapsed().as_nanos() as u64;
        match compiled {
            Some(compiled) => {
                compile.push(elapsed);
                tier.compiled += 1;
                tier.bytes += compiled.code_bytes;
                tier.bytes_of.insert(id, compiled.code_bytes);
                done.push((id, compiled));
            }
            None => tier.refused.push(ir.function(id).qualified()),
        }
    }
    jit.finalize()
        .map_err(|error| format!("template: {error}"))?;
    for (id, compiled) in done {
        tier.entries[id.index()] = Some(jit.entry(compiled));
    }
    Ok((jit, tier, Measured::of(compile)))
}

// --- the decomposition of the call path --------------------------------------
//
// Issue #365's Part 1. The question is what a native call *costs*, attributed to
// the parts of the path rather than to a suspect named first, and the method is
// ablation: `cove_runtime::native_ablate` names components, a variant of the call
// helper does one of them **twice**, and the difference between a variant and the
// baseline is one instance of that component.
//
// Three properties are what make the attribution worth believing, and each one is
// a deliberate choice rather than a convenience:
//
// - **the baseline is the production helper.** `native_helpers_ablated::<0>()` is
//   `native_helpers()` — one function body, one instantiation, every ablation
//   branch folded away by a constant. The table prints both anyway, and the
//   difference between those two rows is the *instrumentation's* own cost,
//   measured rather than argued.
// - **every variant computes the same program.** A variant adds work; it never
//   removes it. So every answer is still checked against the VM's on every call,
//   which is the one check a performance number cannot do without.
// - **the arms are interleaved**, VM first and then every variant, one iteration
//   at a time, exactly as the races above are.
//
// What it costs is stated where the numbers are: a doubled component is warm the
// second time, so every figure is a lower bound.

/// One arm of the decomposition: a name, and the helpers it binds.
#[cfg(feature = "template")]
struct Variant {
    what: &'static str,
    helpers: cove_native::NativeHelpers,
}

/// The arms, in the order one iteration runs them.
///
/// A mask is a *const* parameter, so every arm is written out: there is no way to
/// turn a run-time `u64` into an instantiation, and writing them out is also what
/// keeps the name beside the mask it names.
#[cfg(feature = "template")]
fn variants() -> Vec<Variant> {
    use cove_runtime::native_ablate as it;
    use cove_runtime::{native_helpers, native_helpers_ablated as ablated};
    vec![
        Variant {
            what: "production",
            helpers: native_helpers(),
        },
        Variant {
            what: "baseline (mask 0)",
            helpers: ablated::<0>(),
        },
        Variant {
            what: "+ span",
            helpers: ablated::<{ it::AGAIN_SPAN }>(),
        },
        Variant {
            what: "+ admit_frame",
            helpers: ablated::<{ it::AGAIN_ADMIT }>(),
        },
        Variant {
            what: "+ safepoint",
            helpers: ablated::<{ it::AGAIN_SAFEPOINT }>(),
        },
        Variant {
            what: "+ push_frame/pop",
            helpers: ablated::<{ it::AGAIN_PUSH_POP }>(),
        },
        Variant {
            what: "+ zero fill",
            helpers: ablated::<{ it::AGAIN_ZERO }>(),
        },
        Variant {
            what: "+ argument lookups",
            helpers: ablated::<{ it::AGAIN_ARG_LOOKUP }>(),
        },
        Variant {
            what: "+ argument copy",
            helpers: ablated::<{ it::AGAIN_ARG_COPY }>(),
        },
        Variant {
            what: "+ open_frame, whole",
            helpers: ablated::<{ it::AGAIN_OPEN_FRAME }>(),
        },
        Variant {
            what: "+ frames push/pop",
            helpers: ablated::<{ it::AGAIN_FRAMES }>(),
        },
        Variant {
            what: "+ ctx and indices",
            helpers: ablated::<{ it::AGAIN_CTX }>(),
        },
        Variant {
            what: "+ pop_frame",
            helpers: ablated::<{ it::AGAIN_POP }>(),
        },
        Variant {
            what: "+ republish",
            helpers: ablated::<{ it::AGAIN_REPUBLISH }>(),
        },
        Variant {
            what: "+ floor Vec",
            helpers: ablated::<{ it::AGAIN_FLOOR_VEC }>(),
        },
        Variant {
            what: "+ one C-ABI hop",
            helpers: ablated::<{ it::AGAIN_HOP }>(),
        },
        Variant {
            what: "+ mediation, whole",
            helpers: ablated::<{ it::AGAIN_MEDIATION }>(),
        },
    ]
}

/// The decomposition, on the template arm.
///
/// `calls` is the same slice of real `(l, r)` pairs the race above ran, and
/// `session` is the same session, so the frames, the heap and the arguments are
/// the ones already measured.
#[cfg(feature = "template")]
fn decomposed(
    ir: &Arc<cove_ir::Program>,
    session: &mut cove_runtime::NativeSession<'_, '_>,
    iterations: u32,
    calls: &[Vec<u64>],
) -> Result<(), String> {
    heading_line("the call path, decomposed: each arm does one component twice");

    // The census first, and untimed: how wide the frames are, how many parameter
    // words are copied, and how often `push_frame` reallocates rather than
    // fitting. A timer cannot answer the last one, and "almost never" is a term
    // of the decomposition rather than a detail.
    // Mediated, every one of them: what the decomposition attributes is the
    // mediated helper's work, and a direct call does not reach it.
    let (_census_jit, census_tier, _) = compile_slice(
        ir,
        cove_runtime::native_helpers_ablated::<{ cove_runtime::native_ablate::CENSUS }>(),
        false,
    )?;
    cove_runtime::census_reset();
    for args in calls {
        session
            .call(&census_tier, args)
            .map_err(|error| format!("the census pass refused a call: {}", error.message))?;
    }
    let census = cove_runtime::census_taken();
    let nested = census.calls as f64 / calls.len() as f64;
    println!(
        "  the census, over one untimed pass of {} call(s): {} nested call(s) through the helper, \
         {nested:.2} per raced call",
        calls.len(),
        census.calls
    );
    println!(
        "  frames: {:.1} word(s) per callee frame, of which {:.2} are parameter words over \
         {:.2} parameter(s); deepest {} frame(s)",
        census.frame_words as f64 / census.calls as f64,
        census.param_words as f64 / census.calls as f64,
        census.params as f64 / census.calls as f64,
        census.deepest
    );
    println!(
        "  `push_frame` reallocated on {} of {} call(s) ({:.4}%), and stopped {} of them",
        census.reallocations,
        census.calls,
        100.0 * census.reallocations as f64 / census.calls as f64,
        census.stops
    );
    if census.stops > 0 {
        return Err(
            "a call stopped during the census, so the ablation numbers below are not readable: \
             a duplicated safepoint swallows the stop it saw"
                .to_string(),
        );
    }
    println!();

    // ADR 0058's boundary report over the same untimed pass, on the table the
    // tier itself compiles — direct calls on, and the counting helpers in place of
    // the production ones. Taken rather than read, so the timed arms below do not
    // count.
    let (_boundary_jit, mut boundary_tier, _) =
        compile_slice(ir, cove_runtime::native_helpers_counting(), true)?;
    boundary_tier.counts_helpers = true;
    session.count_boundary();
    for args in calls {
        session
            .call(&boundary_tier, args)
            .map_err(|error| format!("the boundary pass refused a call: {}", error.message))?;
    }
    let boundary = session
        .take_boundary()
        .ok_or("the session was asked to count and has no report")?;
    println!(
        "  the boundary, over one untimed pass of {} call(s) on the direct-call table:",
        calls.len()
    );
    print!("{boundary}");
    println!();

    let variants = variants();
    let tiers: Vec<(cove_native::template::Jit, Tier, Measured)> = variants
        .iter()
        .map(|variant| compile_slice(ir, variant.helpers, false))
        .collect::<Result<Vec<_>, String>>()?;
    let mut samples: Vec<Vec<u64>> = vec![Vec::with_capacity(iterations as usize); variants.len()];
    let mut vm_samples = Vec::with_capacity(iterations as usize);
    let mut oracle: Vec<u64> = Vec::with_capacity(calls.len());
    let mut mine: Vec<u64> = Vec::with_capacity(calls.len());

    for iteration in 0..iterations {
        oracle.clear();
        let started = Instant::now();
        for args in calls {
            let answer = session
                .call(&cove_runtime::NothingCompiled, args)
                .map_err(|error| format!("the vm refused the raced slice: {}", error.message))?;
            oracle.push(answer[0]);
        }
        vm_samples.push(started.elapsed().as_nanos() as u64);

        for (at, (_, tier, _)) in tiers.iter().enumerate() {
            mine.clear();
            let started = Instant::now();
            for args in calls {
                let answer = session
                    .call(tier, args)
                    .map_err(|error| format!("{}: {}", variants[at].what, error.message))?;
                mine.push(answer[0]);
            }
            samples[at].push(started.elapsed().as_nanos() as u64);
            for (which, (answered, expected)) in mine.iter().zip(&oracle).enumerate() {
                if answered != expected {
                    return Err(format!(
                        "`{}` answered {answered} for call {which} of iteration {iteration} and \
                         the VM answered {expected}; a run whose answer differs is a failure, not \
                         a data point",
                        variants[at].what
                    ));
                }
            }
        }
    }

    println!(
        "execution: {iterations} iterations of {} call(s) each, interleaved, the vm first",
        calls.len()
    );
    println!(
        "  {:<22} {:<11} {:<11} {:<11} {:<11} {:<11} {:<11}",
        "arm", "cold", "min", "median", "mean", "max", "ns/call"
    );
    line("vm", &vm_samples, Scale::Millis);
    let baseline = median(&samples[1]);
    for (at, variant) in variants.iter().enumerate() {
        let held = &samples[at];
        let cold = held.first().copied().unwrap_or(0);
        let warm = if held.len() > 1 {
            &held[1..]
        } else {
            &held[..]
        };
        let mut sorted = warm.to_vec();
        sorted.sort_unstable();
        let middle = median(held);
        let delta = middle as f64 - baseline as f64;
        println!(
            "  {:<22} {:<11} {:<11} {:<11} {:<11} {:<11} {:+.2}",
            variant.what,
            Scale::Millis.of(cold),
            Scale::Millis.of(sorted.first().copied().unwrap_or(0)),
            Scale::Millis.of(middle),
            Scale::Millis.of(sorted.iter().sum::<u64>() / sorted.len().max(1) as u64),
            Scale::Millis.of(sorted.last().copied().unwrap_or(0)),
            delta / (calls.len() as f64 * nested)
        );
    }
    println!(
        "  the last column is the median difference from `baseline (mask 0)` divided by the \
         {:.0} nested call(s) the pass makes, in nanoseconds: one instance of the component the \
         arm does twice",
        calls.len() as f64 * nested
    );
    println!(
        "  every arm's answer was checked against the VM's on every one of {} call(s)",
        u64::from(iterations) * calls.len() as u64 * (variants.len() + 1) as u64
    );
    println!(
        "  a doubled component is warm the second time, so every figure is a lower bound; \
         `production` against `baseline (mask 0)` is the instrumentation's own cost"
    );
    println!(
        "  every ablation is out of line, so every figure also carries one direct call and \
         return: `+ one C-ABI hop` bounds that at its own figure, and the components whose \
         figures are near it are the ones that cost nothing"
    );
    Ok(())
}

/// The decomposition needs the template arm, which is the tier's code generator.
#[cfg(not(feature = "template"))]
fn decomposed(
    _ir: &Arc<cove_ir::Program>,
    _session: &mut cove_runtime::NativeSession<'_, '_>,
    _iterations: u32,
    _calls: &[Vec<u64>],
) -> Result<(), String> {
    Err("`--decompose` needs `--features template`, which is the tier's code generator".to_string())
}

// --- arguments ---------------------------------------------------------------

fn flag(name: &str) -> bool {
    std::env::args().skip(1).any(|arg| arg == name)
}

fn flag_u32(name: &str) -> Option<u32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1)?.parse().ok().filter(|value| *value > 0)
}

// --- the covefmt slice -------------------------------------------------------
//
// The second raced scenario, and the one that measures the *prediction*. The
// first scenario's verdict — the two arms within 6% on `arith`'s loop — left the
// case for Cranelift resting on richer code: register allocation, many live
// values, branches. Two functions out of `examples/covefmt/print.cove`'s own
// profile are that richer code.
//
// # Why these two
//
// `byteOfPunct` is 5.3% of covefmt's run over 29 call sites, and it is where the
// interesting instructions are: an `Array` element read at a three-word stride,
// an `Option`'s tag and its `switch`, an enum tag comparison, inline-struct field
// loads (which are slot reads, because an inline struct *is* slots),
// `String.byteAt`, a length, and `Repr::Ref` parameters.
//
// `wantsASpaceBetween` is 5.5% over 313,741 calls, it *calls* `byteOfPunct`, and
// it is branchy with many live values. So it exercises native-to-native and
// native-to-VM calls, and it is where register allocation should show if it is
// going to.
//
// # Allocation is deliberately excluded, and that is not an oversight
//
// ADR 0055 keeps allocation, collection, Host calls and string building as
// runtime helpers. An allocation is therefore *one identical helper call in both
// arms*, and it cannot separate two code generators — a scenario built out of
// them would measure the runtime and report it as a code-generator difference.
// The instructions raced here are the ones each arm emits itself.
//
// # What the harness itself costs, and why the arm-against-arm figure survives it
//
// Every call — the VM's and both arms' — goes through `Vm::native_session`, which
// pushes a frame, writes the parameter words, runs the function and hands back the
// answer's words in a fresh `Vec`. That `Vec` is one allocation per call and it is
// inside the timer, and a native-to-VM call pays one more thing besides: the
// nested `std::thread::scope` `Machine::drive_from` opens, because a `spawn`
// starts its children in one.
//
// None of that is separated out, and it does not have to be. **It is the same
// runtime doing the same thing on every row**, so the difference between the two
// arms is the difference between the code they emit and nothing else. What the
// shared cost does do is compress the *ratios*: a row's native-against-VM figure
// is a lower bound on what removing dispatch is worth, which is why `arith`'s
// row — one entry, two million loop turns, no per-call cost at all — is the one to
// read for that question and these rows are the ones to read for the other.

/// The file the real inputs are lexed out of.
///
/// covefmt's own printer, which is the file the two raced functions are declared
/// in and 120 KB of real Cove source. Nothing about the data is synthetic: the
/// `String` is this file and the `Array<Token>` is what covefmt's own lexer makes
/// of it, run on the VM.
const CORPUS: &str = "../../examples/covefmt/print.cove";

/// The module the two raced functions are in.
const COVEFMT: &str = "covefmt";
/// The lexer, which builds the `Array<Token>`.
const LEXER: &str = "tokens";
/// The raced entry, which calls the other one.
const WANTS: &str = "wantsASpaceBetween";
/// The function it calls, which is the one with the heap reads in it.
const BYTE_OF_PUNCT: &str = "byteOfPunct";

/// How many `(l, r)` pairs one iteration calls over.
///
/// Every pair is a real one — `wantsASpace` asks `wantsASpaceBetween(source,
/// held, at - 1, at + 1)` for a run boundary at `at`, and these are those pairs
/// over the corpus's own tokens — and there are as many of them as the corpus has
/// tokens. The bound is here so an iteration is milliseconds rather than seconds;
/// what it costs is that the pairs raced are a prefix of the file rather than all
/// of it, and every arm and the VM race exactly the same prefix.
const PAIRS: usize = 20_000;

fn covefmt(iterations: u32, dump: bool, decompose: bool) -> Result<(), String> {
    heading_line("the second raced slice: covefmt's `wantsASpaceBetween` and `byteOfPunct`");

    let (sources, checked) = examples_package()?;
    let ir = cove_ir::lower_roots(
        &checked,
        &sources,
        &HostSchemas::new(),
        &[(COVEFMT, LEXER), (COVEFMT, WANTS)],
    )
    .map_err(|items| render(&sources, &items))?;
    let wants = entry_of(&ir, COVEFMT, WANTS)?;
    let byte_of_punct = entry_of(&ir, COVEFMT, BYTE_OF_PUNCT)?;
    report_ir("the lowered slice", &ir);
    println!(
        "  the two raced functions are {} IR instructions: {} in `{WANTS}`, {} in `{BYTE_OF_PUNCT}`",
        ir.function(wants).code.len() + ir.function(byte_of_punct).code.len(),
        ir.function(wants).code.len(),
        ir.function(byte_of_punct).code.len()
    );
    if dump {
        println!("{}", cove_ir::print::function(&ir, wants));
        println!("{}", cove_ir::print::function(&ir, byte_of_punct));
    }
    println!(
        "  allocation is deliberately outside the raced subset: ADR 0055 keeps it a runtime \
         helper, so it is one identical call in both arms and cannot separate them"
    );
    println!();

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    let source =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;

    let ir = Arc::new(ir);
    let sources = Arc::new(sources);
    let checked = Arc::new(checked);
    let mut hosts = fake_hosts();
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(Arc::new(NullSink));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let mut vm = Vm::new(&runtime, &hosts, &ir);

    // The real inputs, built by covefmt's own lexer on the VM.
    let started = Instant::now();
    let held = vm
        .invoke(COVEFMT, LEXER, vec![cove_runtime::Value::string(&*source)])
        .map_err(|error| format!("`{COVEFMT}.{LEXER}` failed: {}", error.message))?;
    let lexed = started.elapsed();
    let count = held
        .items()
        .ok_or_else(|| format!("`{COVEFMT}.{LEXER}` did not answer an `Array`"))?
        .len();
    println!("the real inputs");
    println!(
        "  {} bytes of `{}`, lexed on the vm in {} into {count} token(s)",
        source.len(),
        path.file_name().unwrap_or_default().to_string_lossy(),
        Scale::Millis.of(lexed.as_nanos() as u64)
    );
    // `wantsASpace(source, held, at)` asks about `at - 1` and `at + 1`, so these
    // are the pairs the formatter itself asks about, in the order it asks them.
    let pairs: Vec<(u64, u64)> = (1..count.saturating_sub(1))
        .take(PAIRS)
        .map(|at| ((at - 1) as u64, (at + 1) as u64))
        .collect();
    println!(
        "  {} `(l, r)` pair(s) per iteration, which are the pairs `wantsASpace` asks about",
        pairs.len()
    );
    println!();

    let arms = Compiled::all(&ir)?;
    arms.report();
    arms.raced(&ir, wants, byte_of_punct);
    println!();

    collects_and_agrees(&ir, &sources, &checked, &source, &arms)?;
    println!();

    // Two races rather than one, and the reason is the second table below.
    //
    // `byteOfPunct` holds **no call**: an `Array` element read, an `Option`'s tag
    // and switch, a tag comparison, a `String.byteAt`, a length, and branches. So
    // racing it alone is the two code generators' *emitted code* against each
    // other with nothing else in the measurement — which is the question the
    // comparison exists to answer.
    //
    // `wantsASpaceBetween` calls it, and five other functions besides, so racing
    // it is the call protocol and the branchy many-live-values code the prediction
    // was about. It is also, as the numbers show, dominated by call machinery both
    // tiers share — which is itself worth knowing and is not visible from one
    // table.
    {
        let mut inner = vm
            .native_session(
                COVEFMT,
                BYTE_OF_PUNCT,
                vec![
                    cove_runtime::Value::string(&*source),
                    held.clone(),
                    cove_runtime::Value::int(0),
                ],
            )
            .map_err(|error| format!("the native session refused: {}", error.message))?;
        let words = inner.arguments().to_vec();
        let calls: Vec<Vec<u64>> = pairs
            .iter()
            .map(|(l, _)| vec![words[0], words[1], *l])
            .collect();
        race(
            &format!("`{BYTE_OF_PUNCT}` alone, which holds no call"),
            &mut inner,
            &arms,
            iterations,
            &calls,
        )?;
    }
    println!();

    let mut session = vm
        .native_session(
            COVEFMT,
            WANTS,
            vec![
                cove_runtime::Value::string(&*source),
                held,
                cove_runtime::Value::int(0),
                cove_runtime::Value::int(0),
            ],
        )
        .map_err(|error| format!("the native session refused: {}", error.message))?;
    let words = session.arguments().to_vec();
    if words.len() != 4 {
        return Err(format!(
            "`{COVEFMT}.{WANTS}` takes four words and this lowering gives it {}",
            words.len()
        ));
    }
    let calls: Vec<Vec<u64>> = pairs
        .iter()
        .map(|(l, r)| vec![words[0], words[1], *l, *r])
        .collect();
    race(
        &format!("`{WANTS}`, which calls `{BYTE_OF_PUNCT}` and five others"),
        &mut session,
        &arms,
        iterations,
        &calls,
    )?;
    if decompose {
        println!();
        decomposed(&ir, &mut session, iterations, &calls)?;
    }
    Ok(())
}

/// How many words the collecting pass's heap may grow to.
///
/// Small enough that the corpus's own tokens and the strings `isTheWord` builds
/// do not fit in it at once, so a collection is forced — and large enough that
/// what is live does fit, so the run does not fail for want of memory. Tuned by
/// running it: the assertion below is that a collection *happened*, so a number
/// that stopped forcing one would be caught rather than quietly skipped.
const COLLECTING_HEAP_WORDS: usize = 1 << 18;

/// The most calls the collecting pass will make waiting for a second collection.
///
/// It stops as soon as it has one, so this is a *bound* and not a count: a cap is
/// here so that a change which stopped the slice allocating fails the pass
/// instead of looping.
const COLLECTING_CAP: usize = 400_000;

/// The pass that makes ADR 0055's collector claim a *measurement*.
///
/// "Collection uses the VM stack as the first root map" is only exercised by a run
/// that collects, and the timed races below do not: their heap is the default one
/// and nothing they allocate fills it. So the same calls are made again over a
/// heap small enough to collect, and the answers are compared again.
///
/// What it proves is the thing the widened subset put at risk. `wantsASpaceBetween`
/// takes a `String` and an `Array<Token>` as `Repr::Ref` parameters, it calls a
/// function the subset refuses (`holdsSomething`, which reaches
/// `String.sliceBytes` and so allocates), and the allocation can collect while a
/// *native* frame is on the stack holding both references. If either arm kept a
/// reference only in a register across the call helper, the collector would sweep
/// a live object and the answers would stop agreeing — which is exactly what this
/// checks, and it checks it against the VM's own answer for every call.
fn collects_and_agrees(
    ir: &Arc<cove_ir::Program>,
    sources: &Arc<SourceMap>,
    checked: &Arc<Checked>,
    source: &str,
    arms: &Compiled,
) -> Result<(), String> {
    println!("a collecting pass, over a heap of {COLLECTING_HEAP_WORDS} word(s)");
    let mut hosts = fake_hosts();
    hosts.set_budget(Budget::with_cancellation(
        Limits::default(),
        Cancellation::new(),
    ));
    hosts.set_trace(Arc::new(NullSink));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(Arc::clone(checked), Arc::clone(sources), Arc::clone(&hosts));
    let mut vm = Vm::with_heap_words(&runtime, &hosts, ir, COLLECTING_HEAP_WORDS);
    let held = vm
        .invoke(COVEFMT, LEXER, vec![cove_runtime::Value::string(source)])
        .map_err(|error| {
            format!(
                "`{COVEFMT}.{LEXER}` failed on a small heap: {}",
                error.message
            )
        })?;
    let count = held
        .items()
        .ok_or_else(|| format!("`{COVEFMT}.{LEXER}` did not answer an `Array`"))?
        .len();
    let mut session = vm
        .native_session(
            COVEFMT,
            WANTS,
            vec![
                cove_runtime::Value::string(source),
                held,
                cove_runtime::Value::int(0),
                cove_runtime::Value::int(0),
            ],
        )
        .map_err(|error| format!("the native session refused: {}", error.message))?;
    let words = session.arguments().to_vec();
    let pairs: Vec<(u64, u64)> = (1..count.saturating_sub(1))
        .map(|at| ((at - 1) as u64, (at + 1) as u64))
        .collect();

    // Which calls allocate, asked rather than guessed. The native tier allocates
    // nothing itself — allocation is a runtime helper and deliberately outside the
    // raced subset — so a collection can only be triggered by a callee the subset
    // refused, and only some `(l, r)` pairs reach one. `holdsSomething` is that
    // callee: it reaches `String.sliceBytes`, which allocates, and it is reached
    // when the token at `r` is a `[`.
    let mut allocating: Vec<(u64, u64)> = Vec::new();
    for (l, r) in &pairs {
        let before = session.allocations();
        session
            .call(
                &cove_runtime::NothingCompiled,
                &[words[0], words[1], *l, *r],
            )
            .map_err(|error| format!("the vm refused a call: {}", error.message))?;
        if session.allocations() > before {
            allocating.push((*l, *r));
        }
    }
    if allocating.is_empty() {
        return Err(
            "no call in the slice allocates, so no collection can be forced and this pass \
             cannot prove anything; the slice has changed shape"
                .to_string(),
        );
    }

    let before = session.collections();
    let mut disagreed = 0usize;
    let mut made = 0usize;
    // Two rather than one, because the first collection is the one that proves the
    // walk ran and the second is the one that proves the objects it did not free
    // are still there to be read.
    while session.collections() - before < 2 && made < COLLECTING_CAP {
        for (l, r) in &allocating {
            let args = [words[0], words[1], *l, *r];
            let expected = session
                .call(&cove_runtime::NothingCompiled, &args)
                .map_err(|error| format!("the vm refused a collecting call: {}", error.message))?;
            for arm in arms.names() {
                let answered = session.call(arms.entries(arm)?, &args).map_err(|error| {
                    format!("{arm} refused a collecting call: {}", error.message)
                })?;
                if answered != expected {
                    disagreed += 1;
                }
            }
            made += 1;
        }
    }
    let collections = session.collections() - before;
    println!(
        "  {} of {} pair(s) allocate; {made} call(s) per arm forced {collections} collection(s) \
         while a native frame held the `String` and the `Array`",
        allocating.len(),
        pairs.len()
    );
    if disagreed > 0 {
        return Err(format!(
            "{disagreed} answer(s) differed from the VM's under collection, which means a \
             reference was not where the root walk looks for it"
        ));
    }
    if collections == 0 && !arms.names().is_empty() {
        return Err(
            "no collection ran in `COLLECTING_CAP` calls, so this pass proved nothing about the \
             root walk"
                .to_string(),
        );
    }
    println!("  every answer agreed with the VM's");
    Ok(())
}

/// One function, raced over `calls` by the VM and by every arm, interleaved.
///
/// The VM goes first in each iteration and its answers are the oracle; every arm's
/// answer is compared against it for **every call**, because a performance number
/// from a run that computed the wrong thing is worse than no number.
///
/// The comparison is after the timed loop rather than inside it, so the timer
/// measures the call and not the check. It still checks every call: the answers are
/// collected into a `Vec` reserved before the loop.
fn race(
    what: &str,
    session: &mut cove_runtime::NativeSession<'_, '_>,
    arms: &Compiled,
    iterations: u32,
    calls: &[Vec<u64>],
) -> Result<(), String> {
    let mut vm_samples = Vec::with_capacity(iterations as usize);
    let mut oracle: Vec<u64> = Vec::with_capacity(calls.len());
    let mut mine: Vec<u64> = Vec::with_capacity(calls.len());
    let mut native: Vec<(&'static str, Vec<u64>)> = arms
        .names()
        .into_iter()
        .map(|name| (name, Vec::with_capacity(iterations as usize)))
        .collect();
    let mut dispatched = 0u64;
    let before_tiers = session.tiers();

    for iteration in 0..iterations {
        oracle.clear();
        let before = session.instructions();
        let started = Instant::now();
        for args in calls {
            let answer = session
                .call(&cove_runtime::NothingCompiled, args)
                .map_err(|error| format!("the vm refused the raced slice: {}", error.message))?;
            oracle.push(answer[0]);
        }
        vm_samples.push(started.elapsed().as_nanos() as u64);
        dispatched += session.instructions() - before;

        for (arm, samples) in &mut native {
            let entries = arms.entries(arm)?;
            mine.clear();
            let started = Instant::now();
            for args in calls {
                let answer = session
                    .call(entries, args)
                    .map_err(|error| format!("{arm} refused the raced slice: {}", error.message))?;
                mine.push(answer[0]);
            }
            samples.push(started.elapsed().as_nanos() as u64);
            for (at, (answered, expected)) in mine.iter().zip(&oracle).enumerate() {
                if answered != expected {
                    return Err(format!(
                        "{arm} answered {answered} for call {at} of iteration {iteration} and the \
                         VM answered {expected}; a run whose answer differs is a failure, not a \
                         data point"
                    ));
                }
            }
        }
    }

    let tiers = session.tiers();
    println!(
        "execution: {what}, {iterations} iterations of {} call(s), interleaved",
        calls.len()
    );
    println!(
        "  {:<18} {:<11} {:<11} {:<11} {:<11} {:<11}",
        "arm", "cold", "min", "median", "mean", "max"
    );
    line("vm", &vm_samples, Scale::Millis);
    for (arm, samples) in &native {
        line(arm, samples, Scale::Millis);
    }
    let made = u64::from(iterations) * calls.len() as u64;
    let arms_here = arms.names().len().max(1) as u64;
    println!(
        "  per call, at the median: vm {:.0}ns{}",
        median(&vm_samples) as f64 / calls.len() as f64,
        native
            .iter()
            .map(|(arm, samples)| format!(
                ", {arm} {:.0}ns",
                median(samples) as f64 / calls.len() as f64
            ))
            .collect::<String>()
    );
    println!(
        "  the vm's own iterations dispatched {:.0} IR instruction(s) per call",
        dispatched as f64 / made as f64
    );
    // The VM's own iterations are `made` encoded calls of the raced function
    // itself, and they are taken off so the figures are about the *arms*: what a
    // native arm's call divided into.
    let native_calls = tiers.native() - before_tiers.native();
    let encoded_calls = (tiers.encoded() - before_tiers.encoded()).saturating_sub(made);
    println!(
        "  each arm's calls divided by tier: {:.1} native and {:.1} encoded per raced call \
         ({native_calls} and {encoded_calls} in total)",
        native_calls as f64 / (made * arms_here) as f64,
        encoded_calls as f64 / (made * arms_here) as f64
    );
    println!("  every arm's answer was checked against the VM's on every one of those calls");
    // ADR 0055's "Collection uses the VM stack as the first root map" is only
    // exercised by a run that collected. Whether this one did is reported rather
    // than assumed, because a scenario that never collected has not tested it.
    match session.collections() {
        0 => println!(
            "  no collection ran here, which is what a default heap and a slice that barely \
             allocates come to; the collecting pass above is where the root walk is exercised"
        ),
        collections => println!(
            "  {collections} collection(s) ran with native frames on the stack, and every answer \
             still agreed"
        ),
    }
    Ok(())
}

/// The median of `samples`, which is what the per-call figures above divide.
fn median(samples: &[u64]) -> u64 {
    let warm = if samples.len() > 1 {
        &samples[1..]
    } else {
        samples
    };
    let mut sorted = warm.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// `examples/`, checked.
fn examples_package() -> Result<(SourceMap, Checked), String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut sources = SourceMap::new();
    let package =
        cove_sema::package::load(&root, &mut sources).map_err(|items| render(&sources, &items))?;
    let checked = cove_sema::Compiler::new()
        .compile(&package)
        .map_err(|items| render(&sources, &items))?;
    Ok((sources, checked))
}

fn heading_line(what: &str) {
    println!("{what}");
}

/// Every arm, with every function of the slice it could compile already
/// compiled.
///
/// Every function rather than only the two raced ones, and that is what makes the
/// tier counts mean something: whatever `byteOfPunct` and `wantsASpaceBetween`
/// call is compiled too if the shared subset admits it, and run by the VM if it
/// does not. A run that compiled only the two would report `encoded` calls that
/// were an artefact of the harness rather than of the subset.
struct Compiled {
    #[cfg(feature = "cranelift")]
    cranelift: (cove_native::Jit, Tier, Measured),
    #[cfg(feature = "template")]
    template: (cove_native::template::Jit, Tier, Measured),
    /// The same slice, compiled by the same arm, emitting a **direct call**
    /// wherever the callee has compiled code.
    ///
    /// Issue #365's Part 2, and it is a second `Jit` rather than a flag on the
    /// first for a reason the harness cannot do without: the two forms have to be
    /// *interleaved* against each other and against the VM, and a flag would give
    /// one run of one of them. Everything else about it is the same — the same
    /// lowering, the same subset, the same helper table — so the difference
    /// between the two rows is the difference between the two call sequences.
    #[cfg(feature = "template")]
    template_direct: (cove_native::template::Jit, Tier, Measured),
}

/// One arm's `Program + FunctionId -> native entry` table.
///
/// ADR 0055's entry table, as the one question the runtime asks of it. `None`
/// means the callee runs on the encoded VM.
///
/// A build with no code generator constructs none of these, and the type is still
/// compiled: the tier table is the *runtime's* shape and not a code generator's,
/// which is the same reason `cove_native::abi` is compiled without one.
#[cfg_attr(
    not(any(feature = "cranelift", feature = "template")),
    allow(dead_code)
)]
struct Tier {
    entries: Vec<Option<cove_runtime::NativeEntry>>,
    compiled: usize,
    /// What it would not compile, by name.
    ///
    /// Named rather than counted, because a count cannot say whether the slice's
    /// hot callees ran natively: a run in which `byteOfPunct` was refused would
    /// look like full coverage and measure the VM.
    refused: Vec<String>,
    bytes: u32,
    /// The machine code of each function, by id, for the per-function figure the
    /// arith scenario reports and the totals above cannot give.
    bytes_of: std::collections::BTreeMap<cove_ir::FunctionId, u32>,
    /// Whether the table was compiled against `native_helpers_counting`, which is
    /// what lets a boundary report read its helper counts.
    counts_helpers: bool,
}

impl cove_runtime::Tiered for Tier {
    fn entry(&self, id: cove_ir::FunctionId) -> Option<cove_runtime::NativeEntry> {
        self.entries.get(id.index()).copied().flatten()
    }

    fn counts_helpers(&self) -> bool {
        self.counts_helpers
    }
}

#[cfg_attr(
    not(any(feature = "cranelift", feature = "template")),
    allow(dead_code)
)]
impl Tier {
    fn report(&self, arm: &str) {
        println!(
            "  {arm:<10} compiled {} function(s) and refused {}, emitting {} bytes of machine code",
            self.compiled,
            self.refused.len(),
            self.bytes
        );
    }

    fn refusals(&self, arm: &str) {
        println!("  {arm} refused: {}", self.refused.join(", "));
    }

    fn sizes(
        &self,
        arm: &str,
        ir: &cove_ir::Program,
        wants: cove_ir::FunctionId,
        inner: cove_ir::FunctionId,
    ) {
        println!(
            "  {arm:<10} emitted {} bytes for `{}` and {} for `{}`",
            self.bytes_of.get(&wants).copied().unwrap_or(0),
            ir.function(wants).name,
            self.bytes_of.get(&inner).copied().unwrap_or(0),
            ir.function(inner).name
        );
    }
}

/// Which functions of `ir` an arm is asked about: every lowered one.
///
/// Unreachable in a build with no arm, for [`Tier`]'s reason.
///
/// A stub is not a body, so it is not offered — the shared subset refuses one
/// anyway, and counting it as a refusal would inflate the refusals with
/// declarations no path leads to.
#[cfg_attr(
    not(any(feature = "cranelift", feature = "template")),
    allow(dead_code)
)]
fn candidates(ir: &cove_ir::Program) -> Vec<cove_ir::FunctionId> {
    (0..ir.functions.len())
        .map(|at| cove_ir::FunctionId(at as u32))
        .filter(|id| !ir.function(*id).is_stub())
        .collect()
}

impl Compiled {
    #[allow(unused_variables, clippy::let_and_return)]
    fn all(ir: &Arc<cove_ir::Program>) -> Result<Compiled, String> {
        let held = Compiled {
            #[cfg(feature = "cranelift")]
            cranelift: {
                let mut jit = cove_native::Jit::new(cove_runtime::native_helpers())
                    .map_err(|error| format!("cranelift: {error}"))?;
                let mut tier = Tier {
                    entries: vec![None; ir.functions.len()],
                    compiled: 0,
                    refused: Vec::new(),
                    bytes: 0,
                    bytes_of: std::collections::BTreeMap::new(),
                    counts_helpers: false,
                };
                let mut compile = Vec::new();
                let mut done = Vec::new();
                for id in candidates(ir) {
                    let started = Instant::now();
                    let compiled = jit.compile(ir, id);
                    let elapsed = started.elapsed().as_nanos() as u64;
                    match compiled {
                        Some(compiled) => {
                            compile.push(elapsed);
                            tier.compiled += 1;
                            tier.bytes += compiled.code_bytes;
                            tier.bytes_of.insert(id, compiled.code_bytes);
                            done.push((id, compiled));
                        }
                        None => tier.refused.push(ir.function(id).qualified()),
                    }
                }
                jit.finalize()
                    .map_err(|error| format!("cranelift: {error}"))?;
                for (id, compiled) in done {
                    tier.entries[id.index()] = Some(jit.entry(compiled));
                }
                (jit, tier, Measured::of(compile))
            },
            #[cfg(feature = "template")]
            template: compile_slice(ir, cove_runtime::native_helpers(), false)?,
            #[cfg(feature = "template")]
            template_direct: compile_slice(ir, cove_runtime::native_helpers(), true)?,
        };
        Ok(held)
    }

    fn names(&self) -> Vec<&'static str> {
        [
            #[cfg(feature = "cranelift")]
            "cranelift",
            #[cfg(feature = "template")]
            "template",
            #[cfg(feature = "template")]
            "template direct",
        ]
        .to_vec()
    }

    #[allow(unused_variables)]
    fn entries(&self, arm: &str) -> Result<&dyn cove_runtime::Tiered, String> {
        match arm {
            #[cfg(feature = "cranelift")]
            "cranelift" => Ok(&self.cranelift.1),
            #[cfg(feature = "template")]
            "template" => Ok(&self.template.1),
            #[cfg(feature = "template")]
            "template direct" => Ok(&self.template_direct.1),
            other => Err(format!("there is no `{other}` arm in this build")),
        }
    }

    fn report(&self) {
        println!("compilation, one sample per function");
        println!(
            "  {:<18} {:<11} {:<11} {:<11} {:<11} {:<11}",
            "arm", "first", "min", "median", "mean", "max"
        );
        #[cfg(feature = "cranelift")]
        line(
            "cranelift compile",
            &self.cranelift.2.compile,
            Scale::Micros,
        );
        #[cfg(feature = "template")]
        line("template compile", &self.template.2.compile, Scale::Micros);
        #[cfg(feature = "template")]
        line(
            "direct compile",
            &self.template_direct.2.compile,
            Scale::Micros,
        );
        #[cfg(feature = "cranelift")]
        self.cranelift.1.report("cranelift");
        #[cfg(feature = "template")]
        self.template.1.report("template");
        #[cfg(feature = "template")]
        self.template_direct.1.report("template direct");
        // The subset is one shared predicate, so the two sets are the same set —
        // and the refusals are printed once rather than twice to say so. A
        // comparison over two different subsets would not be one.
        #[cfg(all(feature = "cranelift", feature = "template"))]
        assert_eq!(
            self.cranelift.1.refused, self.template.1.refused,
            "the two arms share one subset predicate, so they refuse one set"
        );
        #[cfg(feature = "cranelift")]
        self.cranelift.1.refusals("both arms");
        #[cfg(all(feature = "template", not(feature = "cranelift")))]
        self.template.1.refusals("template");
        if self.names().is_empty() {
            println!("  no native arm was compiled in; build with `--features cranelift,template`");
        }
    }

    /// Whether the two raced functions are compiled, and how many bytes each
    /// came to.
    ///
    /// The first is the one fact a reader has to have before believing the
    /// execution table: if either function is refused, what was raced is the VM.
    /// The second is the per-function code size the arith scenario reports for
    /// its one function — the totals above are over every function of the slice,
    /// and the two raced ones are what the execution tables are about.
    ///
    /// Initialization is not measured again here. It is once per process and per
    /// code generator, so the figure is the arith scenario's above and repeating
    /// it would be reporting the same measurement twice.
    fn raced(&self, ir: &cove_ir::Program, wants: cove_ir::FunctionId, inner: cove_ir::FunctionId) {
        for arm in self.names() {
            let entries = self.entries(arm).expect("the arm is in this build");
            for id in [wants, inner] {
                println!(
                    "  {arm}: `{}` {}",
                    ir.function(id).qualified(),
                    match cove_runtime::Tiered::entry(entries, id) {
                        Some(_) => "is compiled",
                        None => "IS REFUSED, so what this arm raced is the VM",
                    }
                );
            }
        }
        #[cfg(feature = "cranelift")]
        self.cranelift.1.sizes("cranelift", ir, wants, inner);
        #[cfg(feature = "template")]
        self.template.1.sizes("template", ir, wants, inner);
        #[cfg(feature = "template")]
        self.template_direct
            .1
            .sizes("template direct", ir, wants, inner);
    }
}
