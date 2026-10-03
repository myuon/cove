//! What one isolate — one `cove_runtime::Vm` and what it needs beside it —
//! costs, in memory and in time, and how much of that is shareable.
//!
//! A measurement, not a test, for `main.rs`'s reason: it installs a counting
//! global allocator. This one also counts frees, so that what it reports is
//! *retained* bytes (live after the step) and not only pressure.
//!
//! ```text
//! cargo run --release -p cove-rules --bin cove-rules-isolates
//! ```
//!
//! With no arguments it is a driver: it runs itself once per configuration, in
//! a fresh process each, so that one configuration's leaked instances and
//! allocator state cannot colour the next, and so that the process RSS it
//! reports belongs to that configuration alone. One configuration is
//! `cove-rules-isolates <mode> <entry> <n>`:
//!
//! - `shared`: one image decoded once, one `Runtime`, one `HostRegistry`, and
//!   `n` VMs all borrowing them, each built with `Vm::new` — so each encodes
//!   and verifies the program again. Kept as it was, for the numbers before
//!   `PreparedProgram` to stay reproducible.
//! - `prepared`: `shared`, but the program is prepared once
//!   (`PreparedProgram::new`) and every VM built with `Vm::with_prepared`, so
//!   the encoding, the verification and the layout tables are shared too.
//! - `image`: per instance, decode the image, build a `Runtime` and a `Vm`.
//! - `lower`: per instance, lower the entry from the checked program (which is
//!   shared), build a `Runtime` and a `Vm`.
//! - `heap8k`: `shared`, but every VM built with `Vm::with_heap_words(.., 8192)`,
//!   as a control on what the heap budget's spine costs.
//!
//! `cove-rules-isolates resident` is a second driver, for
//! [issue #572](https://github.com/myuon/cove/issues/572): what a *resident*
//! isolate costs once it has been used, rather than once it has been built.
//! One configuration is `cove-rules-isolates resident <entry> <n> <k>`: `n`
//! `prepared` VMs, all kept alive, each invoked `k` times round-robin; it
//! reports the bytes retained and the RSS per VM, the heap words each has
//! committed, and how many collections they ran. `resident <n> <k,k,..>`
//! runs both entries at every `k` in a fresh process each.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::{Grants, HostRegistry, PreparedProgram, Runtime, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config, HostSchemas};

// --------------------------------------------------------------- the counter

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting allocations, bytes allocated and bytes
/// freed. A reallocation is a free of the old size and an allocation of the
/// new. `alloc_zeroed` is forwarded rather than left to the default, which
/// would write the zeroes and so commit pages `calloc` would not have.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        let ptr = System.alloc(layout);
        attribute::born(ptr, layout.size());
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        let ptr = System.alloc_zeroed(layout);
        attribute::born(ptr, layout.size());
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        attribute::died(ptr);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        attribute::died(ptr);
        let new = System.realloc(ptr, layout, new_size);
        attribute::born(new, new_size);
        new
    }
}

/// Which code retains what, for one stretch of work: every allocation made
/// while tracking is on is keyed by the innermost frames of its backtrace that
/// are in a `cove_` crate, and dropped again when it is freed. Slow, and only
/// for the `attribute` mode.
mod attribute {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    pub static ON: AtomicBool = AtomicBool::new(false);
    pub static LIVE: Mutex<Option<HashMap<usize, (usize, String)>>> = Mutex::new(None);

    thread_local! {
        static INSIDE: Cell<bool> = const { Cell::new(false) };
    }

    fn guarded(body: impl FnOnce()) {
        if !ON.load(Ordering::Relaxed) {
            return;
        }
        let entered = INSIDE.with(|inside| !inside.replace(true));
        if entered {
            body();
            INSIDE.with(|inside| inside.set(false));
        }
    }

    pub fn born(ptr: *mut u8, size: usize) {
        guarded(|| {
            let trace = format!("{}", std::backtrace::Backtrace::force_capture());
            // The innermost source locations in this workspace's crates,
            // inlined frames included.
            let key = trace
                .lines()
                .filter_map(|line| line.trim().strip_prefix("at "))
                .filter(|at| at.contains("/crates/cove-"))
                .map(|at| at.rsplit_once("/crates/").map_or(at, |(_, tail)| tail))
                .take(3)
                .collect::<Vec<_>>()
                .join("  <-  ");
            LIVE.lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(ptr as usize, (size, key));
        });
    }

    pub fn died(ptr: *mut u8) {
        guarded(|| {
            if let Some(live) = LIVE.lock().unwrap().as_mut() {
                live.remove(&(ptr as usize));
            }
        });
    }

    /// The live bytes by key, largest first.
    pub fn report() {
        ON.store(false, Ordering::Relaxed);
        let live = LIVE.lock().unwrap().take().unwrap_or_default();
        let mut by: HashMap<String, (usize, usize)> = HashMap::new();
        for (_, (size, key)) in live {
            let at = by.entry(key).or_default();
            at.0 += size;
            at.1 += 1;
        }
        let mut rows: Vec<_> = by.into_iter().collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1 .0));
        for (key, (bytes, count)) in rows.iter().take(25) {
            println!("{bytes:>9} B {count:>6} allocs  {key}");
        }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[derive(Clone, Copy)]
struct Mark {
    allocations: u64,
    allocated: u64,
    freed: u64,
}

fn mark() -> Mark {
    Mark {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated: ALLOCATED.load(Ordering::Relaxed),
        freed: FREED.load(Ordering::Relaxed),
    }
}

impl Mark {
    /// Allocations since `self`, and bytes retained since `self`.
    fn since(self) -> (u64, i64) {
        let now = mark();
        (
            now.allocations - self.allocations,
            (now.allocated - self.allocated) as i64 - (now.freed - self.freed) as i64,
        )
    }
}

fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

// --------------------------------------------------------------- the package

/// `RulePackage::load`, again, because that type keeps the checked program
/// and the source map private and an isolate needs both by value.
fn load() -> (cove_sema::resolve::Program, SourceMap) {
    let root = cove_rules::package_root();
    let mut files: Vec<(String, PathBuf, String)> = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    let mut sources = SourceMap::new();
    let mut modules: BTreeMap<String, Module> = BTreeMap::new();
    for (name, path, text) in files {
        let file = sources.add(path.clone(), &text);
        let ast = cove_syntax::parse_file(&sources, file).expect("the rule package parses");
        modules
            .entry(name.clone())
            .or_insert_with(|| Module {
                name: name.clone(),
                dir: path.parent().unwrap_or(&root).to_path_buf(),
                units: Vec::new(),
            })
            .units
            .push(Unit { file, path, ast });
    }
    cove_sema::stdlib::install(&mut sources, &mut modules).expect("the stdlib installs");
    let package = Package {
        root: root.clone(),
        config: Config::default(),
        modules,
    };
    let program = Compiler::new()
        .with_host_schema(cove_rules::REVIEWS)
        .compile(&package)
        .unwrap_or_else(|_| panic!("the rule package checks"));
    (program, sources)
}

fn collect(root: &Path, dir: &Path, into: &mut Vec<(String, PathBuf, String)>) {
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir).expect("readable") {
        let path = entry.expect("readable").path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            let text = std::fs::read_to_string(&path).expect("readable");
            let mut parts = vec!["rules".to_string()];
            if let Ok(rest) = dir.strip_prefix(root) {
                parts.extend(
                    rest.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                );
            }
            into.push((parts.join("."), path, text));
        }
    }
    subdirs.sort();
    for subdir in subdirs {
        collect(root, &subdir, into);
    }
}

fn lower(
    checked: &cove_sema::resolve::Program,
    sources: &SourceMap,
    module: &str,
    entry: &str,
) -> cove_ir::Program {
    let schemas = HostSchemas::new().with(cove_rules::REVIEWS);
    cove_ir::lower_entry(checked, sources, &schemas, module, entry)
        .unwrap_or_else(|_| panic!("{module}.{entry} lowers"))
}

// ------------------------------------------------------------ one config

/// What the entry is called with: the process argument `main.rs` uses.
fn argument(entry: &str) -> &'static str {
    match entry {
        "floor" => "0",
        _ => "1",
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[at]
}

fn summary(times: &mut [Duration]) -> String {
    times.sort();
    let mean = times.iter().sum::<Duration>().as_nanos() as f64 / times.len() as f64;
    format!(
        "mean {:>9.1} us  p50 {:>9.1} us  p99 {:>9.1} us",
        mean / 1e3,
        percentile(times, 0.5).as_nanos() as f64 / 1e3,
        percentile(times, 0.99).as_nanos() as f64 / 1e3
    )
}

/// One isolate's own state when nothing is shared.
struct Owned {
    program: cove_ir::Program,
    runtime: Runtime,
}

fn one(mode: &str, entry: &str, n: usize) {
    let module = "rules";
    let (checked, sources) = load();
    let checked = Arc::new(checked);
    let lowered = lower(&checked, &sources, module, entry);
    let image = cove_ir::serial::encode(&lowered, &sources, module, entry)
        .unwrap_or_else(|_| panic!("the image encodes"));
    println!(
        "## {mode} {module}.{entry} n={n}: {} fns ({} with bodies), {} IR insts, {} strings, image {} B",
        lowered.functions.len(),
        lowered.functions.iter().filter(|f| !f.stub).count(),
        lowered
            .functions
            .iter()
            .map(|f| f.code.len())
            .sum::<usize>(),
        lowered.strings.len(),
        image.len()
    );

    // The parts every mode may share. Built before the baseline so that what
    // is reported is per instance.
    let shared_image = cove_ir::serial::decode(&image).expect("the image reads back");
    let shared_sources = Arc::new(shared_image.sources);
    let shared_hosts = Arc::new(HostRegistry::new(Grants::default()));
    let shared_runtime = Runtime::new(
        Arc::default(),
        Arc::clone(&shared_sources),
        Arc::clone(&shared_hosts),
    );
    let shared_program: &'static cove_ir::Program = Box::leak(Box::new(shared_image.program));
    let shared_runtime: &'static Runtime = Box::leak(Box::new(shared_runtime));
    let shared_prepared: &'static PreparedProgram = Box::leak(Box::new(PreparedProgram::new(
        Arc::new(shared_program.clone()),
    )));

    // One isolate's construction, measured once outside the loop as a
    // warm-up, and its components broken down.
    {
        let at = mark();
        let decoded = cove_ir::serial::decode(&image).expect("reads");
        let (a, b) = at.since();
        println!("  part: decode image (Program+SourceMap)       {a:>7} allocs {b:>10} B retained");
        let at = mark();
        let hosts = Arc::new(HostRegistry::new(Grants::default()));
        let (a, b) = at.since();
        println!(
            "  part: HostRegistry::new                       {a:>7} allocs {b:>10} B retained"
        );
        let at = mark();
        let runtime = Runtime::new(Arc::default(), Arc::new(decoded.sources), hosts);
        let (a, b) = at.since();
        println!(
            "  part: Runtime::new (empty checked program)    {a:>7} allocs {b:>10} B retained"
        );
        let at = mark();
        let vm = Vm::new(&runtime, runtime.hosts(), &decoded.program);
        let (a, b) = at.since();
        println!(
            "  part: Vm::new                                 {a:>7} allocs {b:>10} B retained"
        );
        drop(vm);
        let at = mark();
        let vm = Vm::with_heap_words(&runtime, runtime.hosts(), &decoded.program, 1 << 13);
        let (a, b) = at.since();
        println!(
            "  part: Vm::with_heap_words(8192)               {a:>7} allocs {b:>10} B retained"
        );
        drop(vm);
        // The one-time cost `prepared` moves out of every `Vm`: what one
        // preparation allocates and retains, and how long it takes, as the
        // median of several so one cold run does not stand for it.
        let program = Arc::new(decoded.program.clone());
        let at = mark();
        let prepared = PreparedProgram::new(Arc::clone(&program));
        let (a, b) = at.since();
        println!(
            "  part: PreparedProgram::new (once per program) {a:>7} allocs {b:>10} B retained"
        );
        drop(prepared);
        let mut times: Vec<Duration> = (0..50)
            .map(|_| {
                let started = Instant::now();
                let prepared = PreparedProgram::new(Arc::clone(&program));
                let took = started.elapsed();
                drop(prepared);
                took
            })
            .collect();
        println!(
            "  part: PreparedProgram::new time               {}",
            summary(&mut times)
        );
        let at = mark();
        let vm = Vm::with_prepared(&runtime, runtime.hosts(), shared_prepared);
        let (a, b) = at.since();
        println!(
            "  part: Vm::with_prepared                       {a:>7} allocs {b:>10} B retained"
        );
        drop(vm);
        println!(
            "  size_of::<Vm>() = {} B, size_of::<Runtime>() = {} B",
            std::mem::size_of::<Vm<'_>>(),
            std::mem::size_of::<Runtime>()
        );
    }

    let rss_before = rss_kib();
    let base = mark();
    let mut vms: Vec<Box<Vm<'static>>> = Vec::with_capacity(n);
    let mut created = Vec::with_capacity(n);
    let start_all = Instant::now();
    for _ in 0..n {
        let started = Instant::now();
        let vm = match mode {
            "shared" => Vm::new(shared_runtime, shared_runtime.hosts(), shared_program),
            "prepared" => {
                Vm::with_prepared(shared_runtime, shared_runtime.hosts(), shared_prepared)
            }
            "heap8k" => Vm::with_heap_words(
                shared_runtime,
                shared_runtime.hosts(),
                shared_program,
                1 << 13,
            ),
            "image" | "lower" => {
                let (program, sources) = if mode == "image" {
                    let decoded = cove_ir::serial::decode(&image).expect("reads");
                    (decoded.program, Arc::new(decoded.sources))
                } else {
                    (
                        lower(&checked, &sources, module, entry),
                        Arc::clone(&shared_sources),
                    )
                };
                let hosts = Arc::new(HostRegistry::new(Grants::default()));
                let runtime = Runtime::new(Arc::default(), sources, hosts);
                let owned: &'static Owned = Box::leak(Box::new(Owned { program, runtime }));
                Vm::new(&owned.runtime, owned.runtime.hosts(), &owned.program)
            }
            other => panic!("unknown mode {other}"),
        };
        vms.push(Box::new(vm));
        created.push(started.elapsed());
    }
    let wall = start_all.elapsed();
    let (allocs, retained) = base.since();
    let rss_after = rss_kib();
    println!(
        "  create:  {:>8.1} allocs/vm  {:>10.0} B retained/vm  {}  (wall {:.1} ms)",
        allocs as f64 / n as f64,
        retained as f64 / n as f64,
        summary(&mut created),
        wall.as_secs_f64() * 1e3
    );
    println!(
        "  rss:     {} KiB -> {} KiB, {:.0} B/vm by RSS",
        rss_before,
        rss_after,
        (rss_after as f64 - rss_before as f64) * 1024.0 / n as f64
    );

    let args = || vec![Rc::<str>::from(argument(entry))];
    for turn in ["first", "second", "third"] {
        let at = mark();
        let mut times = Vec::with_capacity(n);
        for vm in vms.iter_mut() {
            let started = Instant::now();
            vm.run_entry(module, entry, args())
                .expect("the invocation succeeds");
            times.push(started.elapsed());
        }
        let (allocs, retained) = at.since();
        println!(
            "  {turn:<6} invoke: {:>8.1} allocs/vm  {:>10.0} B retained/vm  {}",
            allocs as f64 / n as f64,
            retained as f64 / n as f64,
            summary(&mut times)
        );
    }
    println!("  rss after 3 invokes: {} KiB", rss_kib());
    std::mem::forget(vms);
}

// ------------------------------------------------------------ resident VMs

/// `n` prepared VMs, all resident, each invoked `k` times: what a resident
/// isolate costs once it has been *used*. See issue #572.
fn resident(entry: &str, n: usize, k: usize) {
    let module = "rules";
    let (checked, sources) = load();
    let lowered = lower(&checked, &sources, module, entry);
    let image = cove_ir::serial::encode(&lowered, &sources, module, entry)
        .unwrap_or_else(|_| panic!("the image encodes"));
    let decoded = cove_ir::serial::decode(&image).expect("the image reads back");
    let hosts = Arc::new(HostRegistry::new(Grants::default()));
    let runtime: &'static Runtime = Box::leak(Box::new(Runtime::new(
        Arc::default(),
        Arc::new(decoded.sources),
        hosts,
    )));
    let prepared: &'static PreparedProgram =
        Box::leak(Box::new(PreparedProgram::new(Arc::new(decoded.program))));
    // A warm-up, so that process-wide one-time state is not charged.
    {
        let mut vm = Vm::with_prepared(runtime, runtime.hosts(), prepared);
        vm.run_entry(module, entry, vec![Rc::from(argument(entry))])
            .expect("the invocation succeeds");
    }

    // Before the baseline, and written, so that what is reported is the
    // VMs' alone: a `Duration` per invocation is sixteen bytes, which at fifty
    // thousand invocations would read as a leak, by the allocator's count and
    // by RSS alike.
    let mut times = vec![Duration::ZERO; n * k];
    let rss_before = rss_kib();
    let base = mark();
    let mut vms: Vec<Vm<'static>> = (0..n)
        .map(|_| Vm::with_prepared(runtime, runtime.hosts(), prepared))
        .collect();
    let started = Instant::now();
    for turn in 0..k {
        for (at, vm) in vms.iter_mut().enumerate() {
            let began = Instant::now();
            vm.run_entry(module, entry, vec![Rc::from(argument(entry))])
                .expect("the invocation succeeds");
            times[turn * n + at] = began.elapsed();
        }
    }
    let wall = started.elapsed();
    let (_, retained) = base.since();
    let rss_after = rss_kib();
    let heap: Vec<u64> = vms.iter().map(|vm| vm.heap_words()).collect();
    let collections: u64 = vms.iter().map(|vm| vm.collections()).sum();
    let live: Vec<u64> = vms.iter().filter_map(|vm| vm.live_words()).collect();
    println!(
        "resident rules.{entry:<12} n={n:<5} k={k:<5} {:>9.0} B retained/vm {:>9.0} B rss/vm  \
         heap words/vm mean {:>8.0} max {:>8}  collections {:>6} ({:.2}/vm)  \
         live words/vm {}  invoke {}  (wall {:.1} s)",
        retained as f64 / n as f64,
        (rss_after as f64 - rss_before as f64) * 1024.0 / n as f64,
        heap.iter().sum::<u64>() as f64 / n as f64,
        heap.iter().max().copied().unwrap_or(0),
        collections,
        collections as f64 / n as f64,
        if live.is_empty() {
            "-".to_string()
        } else {
            format!("{:.0}", live.iter().sum::<u64>() as f64 / live.len() as f64)
        },
        summary(&mut times),
        wall.as_secs_f64()
    );
    std::mem::forget(vms);
}

// --------------------------------------------------------------- the driver

/// Which code the retained bytes of one `Vm::new`, and of its first
/// invocation, belong to.
fn attribute_vm(entry: &str) {
    let module = "rules";
    let (checked, sources) = load();
    let lowered = lower(&checked, &sources, module, entry);
    let hosts = Arc::new(HostRegistry::new(Grants::default()));
    let runtime = Runtime::new(Arc::default(), Arc::new(sources), hosts);
    // A warm-up, so that process-wide one-time state is not charged.
    drop(Vm::new(&runtime, runtime.hosts(), &lowered));
    attribute::ON.store(true, Ordering::Relaxed);
    let mut vm = Vm::new(&runtime, runtime.hosts(), &lowered);
    println!(
        "# Vm::new, rules.{entry}: live bytes by allocating frames ({} heap words)",
        vm.heap_words()
    );
    attribute::report();
    attribute::ON.store(true, Ordering::Relaxed);
    vm.run_entry(module, entry, vec![Rc::from(argument(entry))])
        .expect("runs");
    println!(
        "# first invocation, rules.{entry}: live bytes by allocating frames ({} heap words)",
        vm.heap_words()
    );
    attribute::report();
    std::mem::forget(vm);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 5 && args[1] == "resident" {
        let (entry, n, k) = (
            args[2].clone(),
            args[3].parse().expect("n"),
            args[4].parse().expect("k"),
        );
        cove_runtime::on_cove_stack(move || resident(&entry, n, k)).expect("a thread");
        return;
    }
    if args.get(1).map(String::as_str) == Some("resident") {
        let me = std::env::current_exe().expect("this binary");
        let n = args.get(2).map_or("1000".to_string(), Clone::clone);
        let ks = args.get(3).map_or("1,10,100,1000", String::as_str);
        for entry in ["floor", "decideSample"] {
            for k in ks.split(',') {
                let status = std::process::Command::new(&me)
                    .args(["resident", entry, &n, k])
                    .status()
                    .expect("runs");
                assert!(status.success(), "resident {entry} {n} {k} failed");
            }
        }
        return;
    }
    if args.len() == 3 && args[1] == "attribute" {
        let entry = args[2].clone();
        cove_runtime::on_cove_stack(move || attribute_vm(&entry)).expect("a thread");
        return;
    }
    if args.len() == 4 {
        let (mode, entry, n) = (
            args[1].clone(),
            args[2].clone(),
            args[3].parse().expect("n"),
        );
        cove_runtime::on_cove_stack(move || one(&mode, &entry, n)).expect("a thread");
        return;
    }
    let me = std::env::current_exe().expect("this binary");
    let ns: Vec<usize> = match args.get(1) {
        Some(list) => list.split(',').map(|n| n.parse().expect("n")).collect(),
        None => vec![1, 100, 1000, 10000],
    };
    for entry in ["floor", "decideSample"] {
        for mode in ["shared", "prepared", "heap8k", "image", "lower"] {
            for &n in &ns {
                // About 0.56 MB an unshared decision isolate: ten thousand of
                // them is 5.6 GB, which is the explosion and not a measurement.
                if n > 1000 && entry == "decideSample" && (mode == "image" || mode == "lower") {
                    println!("## {mode} rules.{entry} n={n}: skipped, ~5.6 GB projected");
                    continue;
                }
                let status = std::process::Command::new(&me)
                    .args([mode, entry, &n.to_string()])
                    .status()
                    .expect("runs");
                assert!(status.success(), "{mode} {entry} {n} failed");
            }
        }
    }
}
