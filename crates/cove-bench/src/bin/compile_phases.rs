//! What it costs to *start* a Cove program: the front end, the lowering, and
//! everything else `cove run` does before the entry's first instruction.
//!
//! [Issue #556](https://github.com/myuon/cove/issues/556) wants `cove fmt` to
//! run `examples/covefmt`, and a covefmt start was about 180 ms of which the
//! program's own work is a few. `cove run --stats` reports one figure for the
//! lowering and nothing for the front end, so this is the table that says where
//! the rest goes.
//!
//! # Timed by difference, the way `fmt_phases.rs` is
//!
//! A phase timed with a clock around its own call is a phase table, and a
//! phase table measures the table. So each iteration runs **whole passes**,
//! each one from nothing and each one a prefix of the next, and a phase is the
//! difference between two of them:
//!
//! 1. read: walk the package and read every `.cove` file, as `package::load`
//!    does, and nothing else;
//! 2. load: `cove_sema::package::load`, which reads, parses, and parses the
//!    standard library it attaches;
//! 3. resolve: 2, then `Compiler::resolve`;
//! 4. check: 2, then `Compiler::compile`, which resolves and type-checks;
//! 5. lower: 4, then `cove_ir::lower_entry` for the entry named.
//!
//! The passes are interleaved within an iteration, so a drift in the machine
//! moves every pass and not one phase. What each pass built is dropped
//! **after** its clock stops; the drop is reported once, apart, because
//! `cove run` pays it too, on its way out.
//!
//! What comes after the lowering is a few milliseconds, which is inside the
//! noise of a difference between two 150 ms passes. So it is timed as passes of
//! its own over the IR pass 5 left, which is what an embedded-IR start would
//! begin from:
//!
//! - vm: a `Runtime`, a registry and `Vm::new`, which encodes and verifies the
//!   bytecode and places the literals;
//! - native: `cove_runtime::compile_native` and `Vm::with_native`, where the
//!   host has the tier.
//!
//! The same iteration also times every step of pass 5 with a clock of its
//! own, and both columns are printed. Where they disagree, believe the
//! difference. A few parts are timed alone over what a pass left, and say so:
//! the standard library's parse (`stdlib::attach`), the IR verifier
//! (`cove_ir::verify`, which `lower_entry` runs last) and the bytecode encoder
//! (`encode_program`, which `Vm::new` runs).
//!
//! # Usage
//!
//! ```console
//! $ cargo build --profile checked -p cove-bench
//! $ ./target/checked/cove-compile-phases examples covefmt.main [iterations]
//! $ ./target/checked/cove-compile-phases examples covefmt.main 200 --only lower
//! ```
//!
//! `--only <pass>` runs that one prefix pass and nothing else, for as many
//! iterations as asked, which is what a sampling profiler wants to be pointed
//! at.
//!
//! It prints what the passes did — the files and lines read, the IR functions
//! the lowering emitted and how many of them are stubs, and the functions the
//! native tier compiled — because a timing over a pass that stopped early is
//! not a timing of the pass.
//!
//! # What it does not see
//!
//! Every pass after the first runs on a heap the passes before it warmed, and
//! `cove run` starts on a cold one. A one-pass process (`--only lower` with
//! one iteration) costs about 11 ms more than the steady pass on `examples`
//! (measured 2026-10-02), which is first-touch memory and cold caches spread
//! over every phase. Add that, process start, and the drop to compare this
//! table with a `cove run` wall clock.

use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::{Grants, HostRegistry, Runtime, Vm};
use cove_sema::{Compiler, HostSchemas};

/// The prefix passes, in order.
const PASSES: [&str; 5] = ["read", "load", "resolve", "check", "lower"];

/// Everything a pass built, dropped by the caller once the clock has stopped.
type Held = Box<dyn Any>;

/// One more allocation.
///
/// A load and a store rather than `fetch_add`, which on x86-64 is a locked
/// instruction and measured +3% on the lowering when it was one. Every pass
/// runs on one thread, so nothing is lost; a count from another thread could
/// be, and the count is a measurement rather than something to rely on.
fn count() {
    ALLOCATIONS.store(ALLOCATIONS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
}

/// The system allocator, counting the allocations it hands out.
///
/// Allocation is the largest single item in a profile of the front end and
/// the lowering, and macOS's allocator is quick enough that its cost hides
/// inside every caller's own row. So each phase's allocations are counted, by
/// the same difference between passes as its time.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// Safety: every method forwards to `System`, which upholds `GlobalAlloc`'s
// contract; the counter changes nothing about the memory handed out.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cove-compile-phases: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Median, minimum and maximum.
fn quantile(mut held: Vec<f64>) -> (f64, f64, f64) {
    held.sort_by(f64::total_cmp);
    (held[held.len() / 2], held[0], held[held.len() - 1])
}

fn say(label: &str, (median, min, max): (f64, f64, f64), note: &str) {
    println!("{label:<13} {median:7.2} ms  [{min:6.2}..{max:6.2}]  {note}");
}

fn run() -> Result<(), String> {
    let usage =
        "usage: cove-compile-phases <package-dir> <module.entry> [iterations] [--only <pass>]";
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let only = match args.iter().position(|arg| arg == "--only") {
        Some(at) => {
            let pass = args.get(at + 1).cloned().ok_or(usage)?;
            let Some(index) = PASSES.iter().position(|p| *p == pass) else {
                return Err(format!("`--only` takes one of {PASSES:?}"));
            };
            args.drain(at..at + 2);
            Some(index)
        }
        None => None,
    };
    let root = PathBuf::from(args.first().ok_or(usage)?);
    let entry = args.get(1).ok_or(usage)?.clone();
    let (module, name) = entry.rsplit_once('.').ok_or(usage)?;
    let iterations: usize = match args.get(2) {
        Some(n) => n.parse().map_err(|_| usage.to_string())?,
        None => 15,
    };
    let root = root
        .canonicalize()
        .map_err(|e| format!("cannot open `{}`: {e}", root.display()))?;

    if let Some(upto) = only {
        let started = Instant::now();
        for _ in 0..iterations {
            let held = pass(&root, module, name, upto, &mut [Duration::ZERO; 5])?;
            drop(held);
        }
        println!(
            "{iterations} x {}: {:.1} ms each, drops included",
            PASSES[upto],
            ms(started.elapsed()) / iterations as f64
        );
        return Ok(());
    }

    // What the work was, from one pass of each kind, so the timings below can
    // be read against it.
    let texts = pass(&root, module, name, 0, &mut [Duration::ZERO; 5])?;
    let texts = texts
        .downcast::<Vec<String>>()
        .expect("pass 1 holds the texts");
    let files = texts.len();
    let lines: usize = texts.iter().map(|text| text.lines().count()).sum();
    let mut sources = SourceMap::new();
    let package =
        cove_sema::package::load(&root, &mut sources).map_err(|_| "the package does not load")?;
    let program = Arc::new(
        Compiler::new()
            .compile(&package)
            .map_err(|_| "the package does not check")?,
    );
    let ir = cove_ir::lower_entry(&program, &sources, &HostSchemas::new(), module, name)
        .map_err(|_| format!("`{entry}` does not lower"))?;
    let std_lines: usize = sources
        .files()
        .filter(|file| sources.is_library(file.id))
        .map(|file| file.text.lines().count())
        .sum();
    let sources = Arc::new(sources);
    let native = cove_runtime::compile_native(&ir).ok();

    // Allocations, counted once per prefix pass: they do not vary from one
    // iteration to the next.
    let mut allocations = [0u64; 5];
    for (upto, cell) in allocations.iter_mut().enumerate() {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let held = pass(&root, module, name, upto, &mut [Duration::ZERO; 5])?;
        *cell = ALLOCATIONS.load(Ordering::Relaxed) - before;
        drop(held);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<String>::new())));
        let runtime = Runtime::new(
            Arc::clone(&program),
            Arc::clone(&sources),
            Arc::clone(&hosts),
        );
        drop(Vm::new(&runtime, &hosts, &ir));
    }
    let vm_allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;

    // `rows[i][p]` is prefix pass `p`'s wall time in iteration `i`, and
    // `steps[i][p]` step `p` timed on its own inside pass 5.
    let mut rows: Vec<[f64; 5]> = Vec::new();
    let mut steps: Vec<[f64; 5]> = Vec::new();
    let mut drops = Vec::new();
    let mut vm = Vec::new();
    let mut compiled = Vec::new();
    let mut stdlib = Vec::new();
    let mut verify = Vec::new();
    let mut encode = Vec::new();
    let mut cold: Option<[f64; 5]> = None;
    for iteration in 0..=iterations {
        let mut row = [0.0; 5];
        for (upto, cell) in row.iter_mut().enumerate() {
            let started = Instant::now();
            let held = pass(&root, module, name, upto, &mut [Duration::ZERO; 5])?;
            *cell = ms(started.elapsed());
            let started = Instant::now();
            drop(held);
            if upto == 4 {
                drops.push(ms(started.elapsed()));
            }
        }
        let mut each = [Duration::ZERO; 5];
        drop(pass(&root, module, name, 4, &mut each)?);
        if iteration == 0 {
            cold = Some(row);
            continue;
        }
        rows.push(row);
        steps.push(each.map(ms));

        // After the lowering, from the IR it left.
        let started = Instant::now();
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<String>::new())));
        let runtime = Runtime::new(
            Arc::clone(&program),
            Arc::clone(&sources),
            Arc::clone(&hosts),
        );
        let machine = Vm::new(&runtime, &hosts, &ir);
        vm.push(ms(started.elapsed()));
        drop(machine);
        if native.is_some() {
            let started = Instant::now();
            let tier = cove_runtime::compile_native(&ir).map_err(|e| e.to_string())?;
            let machine = Vm::with_native(&runtime, &hosts, &ir, &tier);
            compiled.push(ms(started.elapsed()));
            drop(machine);
        }

        let mut fresh = SourceMap::new();
        let started = Instant::now();
        let attached = cove_sema::stdlib::attach(&mut fresh);
        stdlib.push(ms(started.elapsed()));
        drop(attached);
        let started = Instant::now();
        let verified = cove_ir::verify(&ir);
        verify.push(ms(started.elapsed()));
        if verified.is_err() {
            return Err("the lowered program does not verify".into());
        }
        let started = Instant::now();
        let encoded = cove_ir::bytecode::encode_program(&ir);
        encode.push(ms(started.elapsed()));
        drop(encoded);
    }

    let column = |table: &[[f64; 5]], pick: &dyn Fn(&[f64; 5]) -> f64| {
        quantile(table.iter().map(pick).collect())
    };
    println!(
        "package `{}`, entry `{entry}`\n  {files} file(s), {lines} line(s) of package source; \
         {std_lines} line(s) of standard library",
        root.display()
    );
    println!(
        "  lowered {} function(s), {} of them stubs; {}",
        ir.functions.len(),
        ir.functions.iter().filter(|f| f.is_stub()).count(),
        match &native {
            Some(tier) => format!(
                "native compiles {} of {} reachable",
                tier.compiled(),
                tier.reachable()
            ),
            None => "no native tier on this host".to_string(),
        }
    );
    // The shape of what the passes after `emit` walk. Several of them keep a
    // table per instruction per frame word, so the sum of those products is a
    // better predictor of their cost than the instruction count is.
    let bodies: Vec<_> = ir.functions.iter().filter(|f| !f.is_stub()).collect();
    let instructions: usize = bodies.iter().map(|f| f.code.len()).sum();
    let product: usize = bodies.iter().map(|f| f.code.len() * f.reprs.len()).sum();
    if let Some(largest) = bodies.iter().max_by_key(|f| f.code.len() * f.reprs.len()) {
        println!(
            "  {instructions} instruction(s) in the bodies; sum of instructions x frame words \
             {product}, of which the largest, `{}.{}`, is {} x {}",
            largest.module,
            largest.name,
            largest.code.len(),
            largest.reprs.len()
        );
    }
    println!("  {iterations} iteration(s) after one cold one; median [min..max]");
    println!();
    println!("by difference between whole passes:");
    let notes = [
        ("read", "walk and read the package's .cove files"),
        ("parse", "load - read: the package and the standard library"),
        ("resolve", "resolve - load"),
        ("typeck", "check - resolve"),
        ("lower", "lower - check: cove_ir::lower_entry"),
    ];
    for (upto, (label, note)) in notes.iter().enumerate() {
        let difference = match upto {
            0 => column(&rows, &|r| r[0]),
            p => column(&rows, &|r| r[p] - r[p - 1]),
        };
        let allocated = match upto {
            0 => allocations[0],
            p => allocations[p].saturating_sub(allocations[p - 1]),
        };
        say(label, difference, &format!("{allocated:>9} alloc  {note}"));
    }
    say(
        "to lowered",
        column(&rows, &|r| r[4]),
        &format!(
            "{:>9} alloc  pass 5, the whole front end and the lowering",
            allocations[4]
        ),
    );
    if let Some(cold) = cold {
        println!(
            "{:<13} {:7.2} ms  (lower {:.2} ms)  the first iteration",
            "  cold",
            cold[4],
            cold[4] - cold[3]
        );
    }
    say(
        "drop",
        quantile(drops),
        "what pass 5 built, which `cove run` frees on its way out",
    );
    println!();
    println!("from the IR pass 5 left, as passes of their own:");
    say(
        "vm setup",
        quantile(vm),
        &format!(
            "{vm_allocations:>9} alloc  Runtime, HostRegistry, Vm::new (encode, verify, \
             place literals)"
        ),
    );
    if !compiled.is_empty() {
        say(
            "native",
            quantile(compiled),
            "compile_native and Vm::with_native",
        );
    }
    println!();
    println!("each step of pass 5 timed alone (a phase table, for comparison):");
    say("load", column(&steps, &|r| r[1]), "read and parse");
    say("check", column(&steps, &|r| r[3]), "resolve and typeck");
    say("lower", column(&steps, &|r| r[4]), "");
    println!();
    println!("timed alone, over what a pass left:");
    say(
        "std parse",
        quantile(stdlib),
        "cove_sema::stdlib::attach, part of `parse`",
    );
    say(
        "ir verify",
        quantile(verify),
        "cove_ir::verify, part of `lower`",
    );
    say(
        "encode",
        quantile(encode),
        "encode_program, part of `vm setup`",
    );
    Ok(())
}

/// Runs prefix passes 1 to `upto + 1`, putting step `p`'s own time in
/// `each[p]`, and hands back what it built so the caller can drop it outside
/// any clock.
fn pass(
    root: &Path,
    module: &str,
    name: &str,
    upto: usize,
    each: &mut [Duration; 5],
) -> Result<Held, String> {
    if upto == 0 {
        let mut texts = Vec::new();
        walk(root, &mut texts);
        return Ok(Box::new(texts));
    }

    let started = Instant::now();
    let mut sources = SourceMap::new();
    let package =
        cove_sema::package::load(root, &mut sources).map_err(|_| "the package does not load")?;
    each[1] = started.elapsed();
    if upto == 1 {
        return Ok(Box::new((sources, package)));
    }
    let compiler = Compiler::new();
    let started = Instant::now();
    if upto == 2 {
        let program = compiler
            .resolve(&package)
            .map_err(|_| "the package does not resolve")?;
        each[2] = started.elapsed();
        return Ok(Box::new((sources, package, program)));
    }
    let program = compiler
        .compile(&package)
        .map_err(|_| "the package does not check")?;
    each[3] = started.elapsed();
    if upto == 3 {
        return Ok(Box::new((sources, package, program)));
    }

    let started = Instant::now();
    let ir = cove_ir::lower_entry(&program, &sources, &HostSchemas::new(), module, name)
        .map_err(|_| format!("`{module}.{name}` does not lower"))?;
    each[4] = started.elapsed();
    Ok(Box::new((sources, package, program, ir)))
}

/// Every `.cove` file `package::load` would read, read: a directory whose name
/// starts with `.`, `target`, and a nested package are skipped, as it skips
/// them.
fn walk(dir: &Path, texts: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() {
            if name.starts_with('.') || name == "target" || path.join("cove.toml").is_file() {
                continue;
            }
            walk(&path, texts);
        } else if name.ends_with(".cove") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                texts.push(text);
            }
        }
    }
}
