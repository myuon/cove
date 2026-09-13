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

#[cfg(any(feature = "cranelift", feature = "template"))]
fn helpers() -> cove_native::NativeHelpers {
    cove_native::NativeHelpers { safepoint }
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
        #[allow(unused_mut)]
        let mut words = vec![0u64; self.frame as usize + base as usize];
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

/// Enters compiled code over `words` and reads the answer out of the frame.
#[cfg(any(feature = "cranelift", feature = "template"))]
fn enter(entry: cove_native::Entry, words: &mut [u64], base: u64) -> (u64, i64) {
    let mut ctx = cove_native::NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr());
    let started = Instant::now();
    // Safety: `ctx.words` is `words`, `base` indexes into it, and the frame the
    // lowering asked for fits inside what was allocated above.
    let outcome = unsafe { entry(&mut ctx, base) };
    let elapsed = started.elapsed();
    assert_eq!(
        outcome,
        cove_native::Outcome::Returned,
        "the raced unit returns; it raises nothing and its safepoint never stops"
    );
    let answer = words[base as usize + ctx.return_slot as usize] as i64;
    (elapsed.as_nanos() as u64, answer)
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
