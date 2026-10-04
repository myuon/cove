//! What a parked run costs to hold and to resume, when an embedder's own pool
//! of worker threads drives many of them:
//! [ADR 0080](../../../../../docs/adr/0080-a-host-call-may-answer-pending.md).
//!
//! A measurement, not a test, for `isolates.rs`'s reason: it installs a
//! counting global allocator, so that what it reports is the bytes a parked run
//! *retains*.
//!
//! ```text
//! cargo run --profile checked -p cove-rules --bin cove-rules-parked [n] [workers]
//! ```
//!
//! The program is a request handler that makes three host calls with a little
//! work between them, and the host, `fetch`, answers every call pending. So the
//! scheduler here — a queue and `workers` threads, which is the embedder's and
//! not the runtime's — parks each of `n` runs three times and resumes it three
//! times, on whichever worker takes it off the queue. The host's work is free:
//! the answer is computed on the worker as it resumes, so what is timed is the
//! runtime's half and nothing else.
//!
//! Reported: the bytes one parked run retains, by the allocator and by RSS; how
//! long one resume takes (the answer written, the run to its next park); how
//! long a parked run waited in the queue; and that every run answered exactly
//! what the same run answers when every host call blocks, in the same number of
//! instructions.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_diag::SourceMap;
use cove_runtime::{
    Effect, Grants, HostAnswer, HostApi, HostRegistry, HostType, ModuleSchema, OperationSchema,
    OwnedVm, ParkedVm, PreparedProgram, Runtime, RuntimeError, Step, Transfer, Value,
};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config, HostSchemas};

// --------------------------------------------------------------- the counter

static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting bytes allocated and bytes freed.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Bytes live now, by the allocator's count.
fn live() -> i64 {
    ALLOCATED.load(Ordering::Relaxed) as i64 - FREED.load(Ordering::Relaxed) as i64
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

// ------------------------------------------------------------------ the host

const FETCH: ModuleSchema = ModuleSchema {
    name: "fetch",
    capability: "fetch",
    operations: &[OperationSchema {
        name: "get",
        params: &[HostType::Int],
        variadic: false,
        result: HostType::Int,
        capability: "fetch",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

/// The answer to `get(n)`, wherever it is computed.
fn reply(n: i64) -> i64 {
    n * 31 + 7
}

/// Answers every call pending where the run can park, and at once where it
/// cannot — which in this program is nowhere.
struct Fetch;

impl HostApi for Fetch {
    fn module_schema(&self) -> ModuleSchema {
        FETCH
    }

    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        Ok(Value::int(reply(args[0].as_int().expect("Int"))))
    }

    fn call_parkable(
        &self,
        _op: &str,
        args: Vec<Value>,
        _back: &mut dyn cove_runtime::Reentry,
    ) -> HostAnswer {
        HostAnswer::Pending(Box::new(args[0].as_int().expect("Int")))
    }
}

const SOURCE: &str = "\
use fetch

/// A request handler: three host calls, with a little work after each.
export fn handle() -> Int {
  var total = 0
  for call in 0..<3 {
    let got = fetch.get(call)
    for i in 0..<200 {
      total += (got + i) % 7
    }
  }
  total
}
";

fn build() -> (Arc<Runtime>, Arc<HostRegistry>, PreparedProgram) {
    let mut hosts = HostRegistry::new(Grants::new(["fetch"]));
    hosts.register(Box::new(Fetch));
    let schemas = HostSchemas::only(hosts.module_schemas());
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("parses");
    let mut modules = BTreeMap::from([(
        "app".to_string(),
        Module {
            name: "app".to_string(),
            dir: PathBuf::from("app"),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    cove_sema::stdlib::install(&mut sources, &mut modules).expect("the stdlib installs");
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    let checked = Compiler::new()
        .with_schemas(schemas.clone())
        .compile(&package)
        .unwrap_or_else(|_| panic!("checks"));
    let lowered = cove_ir::lower_entry(&checked, &sources, &schemas, "app", "handle")
        .unwrap_or_else(|_| panic!("lowers"));
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(Runtime::new(
        Arc::new(checked),
        Arc::new(sources),
        Arc::clone(&hosts),
    ));
    (runtime, hosts, PreparedProgram::new(Arc::new(lowered)))
}

// --------------------------------------------------------------- the numbers

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn summary(times: &mut [Duration]) -> String {
    times.sort();
    let mean = times.iter().sum::<Duration>().as_nanos() as f64 / times.len().max(1) as f64;
    format!(
        "mean {:>8.2} us  p50 {:>8.2} us  p99 {:>8.2} us  max {:>8.2} us",
        mean / 1e3,
        percentile(times, 0.5).as_nanos() as f64 / 1e3,
        percentile(times, 0.99).as_nanos() as f64 / 1e3,
        times.last().copied().unwrap_or_default().as_nanos() as f64 / 1e3,
    )
}

/// One parked run on the scheduler's queue, and when it was put there.
struct Queued {
    parked: ParkedVm,
    since: Instant,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).map_or(1000, |n| n.parse().expect("n"));
    let workers: usize = args.get(2).map_or(4, |w| w.parse().expect("workers"));
    let (runtime, hosts, prepared) = build();
    let vm = || OwnedVm::new(Arc::clone(&runtime), Arc::clone(&hosts), prepared.clone());

    // The reference: the same run with every call blocking.
    let mut reference = vm();
    let expected = reference
        .invoke("app", "handle", Vec::new())
        .expect("runs")
        .to_string();
    let instructions = reference.instructions();
    let mut blocking: Vec<Duration> = (0..n)
        .map(|_| {
            let mut fresh = vm();
            let started = Instant::now();
            fresh.invoke("app", "handle", Vec::new()).expect("runs");
            started.elapsed()
        })
        .collect();
    println!("## {n} runs, {workers} workers: `handle` answers {expected} in {instructions} instructions");
    println!("  blocking run, whole:     {}", summary(&mut blocking));

    // Every run started and parked at its first call, on this thread.
    let rss_before = rss_kib();
    let before = live();
    let mut queue = VecDeque::with_capacity(n);
    let mut first: Vec<Duration> = Vec::with_capacity(n);
    for _ in 0..n {
        let fresh = vm();
        let started = Instant::now();
        let step = fresh.invoke_parkable("app", "handle", Vec::new());
        first.push(started.elapsed());
        match step {
            Step::Parked(parked) => queue.push_back(Queued {
                parked,
                since: Instant::now(),
            }),
            Step::Answered(..) | Step::Yielded(_) => panic!("the first call parks"),
        }
    }
    let held = live() - before;
    let rss_after = rss_kib();
    println!(
        "  parked:  {:>10.0} B retained/run by the allocator, {:.0} B/run by RSS ({} KiB -> {} KiB)",
        held as f64 / n as f64,
        (rss_after as f64 - rss_before as f64) * 1024.0 / n as f64,
        rss_before,
        rss_after
    );
    println!("  start to first park:     {}", summary(&mut first));

    // The embedder's scheduler: a queue and a pool. Nothing in it is the
    // runtime's — which is the point.
    let queue = Arc::new(Mutex::new(queue));
    let finished = Arc::new(AtomicUsize::new(0));
    let resumes = Arc::new(Mutex::new(Vec::with_capacity(3 * n)));
    let waits = Arc::new(Mutex::new(Vec::with_capacity(3 * n)));
    let wrong = Arc::new(AtomicUsize::new(0));
    let wall = Instant::now();
    let pool: Vec<_> = (0..workers)
        .map(|_| {
            let (queue, finished, resumes, waits, wrong, expected) = (
                Arc::clone(&queue),
                Arc::clone(&finished),
                Arc::clone(&resumes),
                Arc::clone(&waits),
                Arc::clone(&wrong),
                expected.clone(),
            );
            std::thread::spawn(move || {
                let (mut mine, mut waited) = (Vec::new(), Vec::new());
                while finished.load(Ordering::Relaxed) < n {
                    let Some(Queued { mut parked, since }) = queue.lock().unwrap().pop_front()
                    else {
                        std::thread::yield_now();
                        continue;
                    };
                    let started = Instant::now();
                    waited.push(started - since);
                    let request = parked.take_request().expect("a request");
                    let asked = *request.downcast::<i64>().expect("the host's own type");
                    let step = parked.resume(Ok(Transfer::Int(reply(asked))));
                    mine.push(started.elapsed());
                    match step {
                        Step::Parked(parked) => queue.lock().unwrap().push_back(Queued {
                            parked,
                            since: Instant::now(),
                        }),
                        Step::Answered(vm, answer) => {
                            let answer = answer.map(|v| v.to_string()).ok();
                            if answer.as_deref() != Some(expected.as_str())
                                || vm.instructions() != instructions
                            {
                                wrong.fetch_add(1, Ordering::Relaxed);
                            }
                            finished.fetch_add(1, Ordering::Relaxed);
                        }
                        Step::Yielded(_) => unreachable!("nothing here asks a run to yield"),
                    }
                }
                resumes.lock().unwrap().extend(mine);
                waits.lock().unwrap().extend(waited);
            })
        })
        .collect();
    for worker in pool {
        worker.join().expect("a worker");
    }
    let wall = wall.elapsed();
    let mut resumes = std::mem::take(&mut *resumes.lock().unwrap());
    let mut waits = std::mem::take(&mut *waits.lock().unwrap());
    println!(
        "  resume (answer to next park or end), {} resumes: {}",
        resumes.len(),
        summary(&mut resumes)
    );
    println!("  queue wait:              {}", summary(&mut waits));
    println!(
        "  wall {:.1} ms, {:.0} resumes/s; {} of {n} runs answered other than the blocking run",
        wall.as_secs_f64() * 1e3,
        resumes.len() as f64 / wall.as_secs_f64(),
        wrong.load(Ordering::Relaxed)
    );
    assert_eq!(wrong.load(Ordering::Relaxed), 0);
}
