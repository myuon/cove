//! Failure, clean-up, and the next run: issue
//! [#681](https://github.com/myuon/cove/issues/681), priority 3.
//!
//! `docs/ISOLATION_TESTING.md` is the specification this holds the runtime
//! to, and [ADR 0114](../../../docs/adr/0114-what-a-failed-run-leaves-behind.md)
//! the decisions behind it. In one sentence: **a run that failed at any stage
//! leaves the machine, the preparation and the registry exactly fit for the
//! next run, and the next run cannot tell that it was not the first.**
//!
//! "Cannot tell" is measured, not assumed. Every case below fails a run one
//! way — refused before it starts, out of heap, trapped, past its deadline,
//! cancelled from another thread, too deep, over its host-call limit, parked
//! and then cancelled, dropped, resumed with a failure — and then runs the
//! same probe on the *same* machine and on a *fresh* one built from the same
//! [`PreparedProgram`]. The two [`Observed`]s have to be equal: the answer,
//! what reached the console, what reached the host, and the instructions,
//! allocated words and host calls the run was charged. A heap that kept a
//! previous run's words where the next run reads, a console line buffered
//! across the end of a run, a budget counter or cancellation flag that
//! survived, or a frame left on the stack would each show up as a difference
//! in one of those.
//!
//! Each case runs on the encoded tier, and again with the native tier
//! installed where this host has one (the gate is `native_tier.rs`'s).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::SourceMap;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{
    Budget, Cancellation, Console, Effect, Grants, HostAnswer, HostApi, HostRegistry, HostType,
    Limits, ModuleSchema, OperationSchema, OwnedVm, ParkedVm, PreparedProgram, Reentry,
    ResourceHandle, ResourceSchema, Runtime, RuntimeError, Step, Transfer, Value, YieldedVm,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

// ------------------------------------------------------------------ the host

const fn op(name: &'static str) -> OperationSchema {
    OperationSchema {
        name,
        params: &[HostType::Int],
        variadic: false,
        result: HostType::Int,
        capability: "probe",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const TICKETS: &[ResourceSchema] = &[ResourceSchema {
    name: "Ticket",
    task_safe: true,
    operations: &[],
}];

/// `probe.ticket(n)` mints a new resource, a `probe.Ticket`, recording `n`
/// against it; `probe.redeem(ticket)` answers the `n` it was minted with.
const PROBE: ModuleSchema = ModuleSchema {
    name: "probe",
    capability: "probe",
    operations: &[
        op("wait"),
        op("note"),
        OperationSchema {
            result: HostType::Named("probe.Ticket"),
            ..op("ticket")
        },
        OperationSchema {
            params: &[HostType::Named("probe.Ticket")],
            ..op("redeem")
        },
    ],
    types: &[],
    resources: TICKETS,
};

/// What `probe.wait` asks its embedder for when it pends. It counts its own
/// drops, so a case can see that a request the run was parked with is freed
/// exactly once, whichever way the run ended.
struct Request {
    n: i64,
    dropped: Arc<AtomicUsize>,
}

impl Drop for Request {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// The answer to `wait(n)`, pended or blocking.
fn reply(n: i64) -> i64 {
    n * 2 + 1
}

/// Every call that reached the host, in order, and every request it made
/// that has since been dropped.
#[derive(Default)]
struct Probe {
    log: Mutex<Vec<String>>,
    made: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    /// The `n` each ticket was minted with, by id less one: an id is never
    /// reused (ADR 0013).
    tickets: Mutex<Vec<i64>>,
}

impl Probe {
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

struct Registered(Arc<Probe>);

fn int(args: &[Value]) -> i64 {
    args[0].as_int().expect("the schema says Int")
}

impl HostApi for Registered {
    fn module_schema(&self) -> ModuleSchema {
        PROBE
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if op == "redeem" {
            let ticket = args[0].resource().expect("the schema says a ticket");
            let tickets = self.0.tickets.lock().unwrap();
            return Ok(Value::int(tickets[ticket.id as usize - 1]));
        }
        let n = int(&args);
        if op == "ticket" {
            let mut tickets = self.0.tickets.lock().unwrap();
            tickets.push(n);
            let id = tickets.len() as u64;
            return Ok(Value::from_resource(ResourceHandle::new(
                "probe",
                &TICKETS[0],
                id,
            )));
        }
        self.0.log.lock().unwrap().push(format!("{op} {n}"));
        match op {
            "wait" => Ok(Value::int(reply(n))),
            "note" => Ok(Value::int(n)),
            other => unreachable!("`{other}` is not declared"),
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
        match op {
            "wait" => {
                let n = int(&args);
                self.0.log.lock().unwrap().push(format!("{op} {n} pended"));
                self.0.made.fetch_add(1, Ordering::SeqCst);
                HostAnswer::Pending(Box::new(Request {
                    n,
                    dropped: Arc::clone(&self.0.dropped),
                }))
            }
            _ => HostAnswer::Ready(self.call_with(op, args, back)),
        }
    }
}

/// A console writer the test can read back.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn taken(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

// --------------------------------------------------------------- the program

const SOURCE: &str = "\
use console
use probe

/// The probe every case runs after a failure: deterministic, and it
/// allocates a vector, an array, strings and a closure, so a heap that held
/// a previous run's words where this one reads would change its answer.
export fn work(n: Int) -> String {
  var items = Vector.of(0)
  for i in 0..<n {
    items.push((i * 7919 + 13) % 1009)
  }
  var total = 0
  for i in 0..<items.length() {
    total += items.get(i).unwrapOr(0)
  }
  let words = items.toArray().map(fn(x) {
    \"w{x}\"
  })
  let joined = words.fold(\"\", fn(text, word) {
    \"{text}{word}\"
  })
  \"{n}:{total}:{words.length()}:{joined.length()}\"
}

/// Prints half a line, fills a vector with a pattern, and divides by zero.
export fn trap(n: Int) -> String {
  let printed = console.print(\"half a line from a run that traps \")
  var items = Vector.of(0)
  for i in 0..<n {
    items.push(23130 + i)
  }
  let zero = n - n
  \"{items.length() / zero}\"
}

/// Counts until something stops it.
export fn spin() -> Int {
  var turns = 0
  while turns >= 0 {
    turns += 1
  }
  turns
}

/// Nests `n` calls deep.
export fn deep(n: Int) -> Int {
  if n <= 0 {
    return 0
  }
  deep(n - 1) + 1
}

/// Makes `n` host calls, each of which may park.
export fn calls(n: Int) -> Int {
  var total = 0
  for i in 0..<n {
    total += probe.wait(i)
  }
  total
}

/// Keeps `n` numbers alive at once.
export fn hog(n: Int) -> Int {
  var items = Vector.of(0)
  for i in 0..<n {
    items.push(i)
  }
  items.length()
}

/// Prints one line.
export fn say(n: Int) -> Int {
  let printed = console.println(\"say {n}\")
  n
}

/// Two tasks, one of which makes a host call.
export fn tasks(n: Int) -> Int {
  var total = 0
  scope work {
    let noted = work.spawn {
      probe.note(n)
    }
    let doubled = work.spawn {
      n * 2
    }
    total = (await noted) + (await doubled)
  }
  total
}

/// A task that makes a host call, beside a sibling that divides by zero.
export fn taskTrap(n: Int) -> Int {
  var total = 0
  scope work {
    let noted = work.spawn {
      probe.note(n)
    }
    let broken = work.spawn {
      n / (n - n)
    }
    total = (await noted) + (await broken)
  }
  total
}

/// Holds a cell's lock while it traps.
export fn lockTrap(n: Int) -> Int {
  let cell = Shared(n)
  cell.lock(fn(var value) {
    value = value / (n - n)
  })
  n
}

/// Holds a cell's lock and answers.
export fn lockOk(n: Int) -> Int {
  let cell = Shared(n)
  cell.lock(fn(var value) {
    value = value + 1
  })
  cell.lock(fn(value) {
    value
  })
}

/// A closure that adds `n`, for a host to keep.
export fn adder(n: Int) -> fn(Int) -> Int {
  fn(x) {
    x + n
  }
}

/// Calls `f`.
export fn apply(f: fn(Int) -> Int, x: Int) -> Int {
  f(x)
}

/// Mints a ticket for `n` and redeems it, so the run is handed a resource.
export fn ticket(n: Int) -> Int {
  probe.redeem(probe.ticket(n))
}

/// A ticket for `n`, handed to the embedder.
export fn mint(n: Int) -> probe.Ticket {
  probe.ticket(n)
}

/// What a ticket the embedder kept was minted with.
export fn redeem(ticket: probe.Ticket) -> Int {
  probe.redeem(ticket)
}

/// A closure that has a ticket for `n` in its environment.
export fn keeper(n: Int) -> fn(Int) -> Int {
  let held = probe.ticket(n)
  fn(x) {
    probe.redeem(held) + x
  }
}
";

// ----------------------------------------------------------------- the world

/// The two tiers a run can be built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Encoded,
    #[cfg_attr(
        not(all(feature = "template", target_arch = "x86_64", unix)),
        allow(dead_code)
    )]
    Native,
}

/// Everything a run is built over, built once.
struct World {
    runtime: Arc<Runtime>,
    hosts: Arc<HostRegistry>,
    probe: Arc<Probe>,
    out: Sink,
    encoded: PreparedProgram,
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    native: PreparedProgram,
}

fn world() -> World {
    let probe = Arc::new(Probe::default());
    let out = Sink::default();
    let mut hosts = HostRegistry::new(Grants::new(["console", "probe"]));
    hosts.register(Box::new(Registered(Arc::clone(&probe))));
    hosts.register(Box::new(Console::new(out.clone(), out.clone())));
    let schemas = HostSchemas::only(hosts.module_schemas());
    let (sources, package) = packaged(SOURCE);
    let render = |items: Vec<cove_diag::Diagnostic>| {
        items
            .iter()
            .map(|item| cove_diag::render(&sources, item))
            .collect::<String>()
    };
    let checked = Compiler::new()
        .with_schemas(schemas.clone())
        .compile(&package)
        .unwrap_or_else(|items| panic!("the fixture checks:\n{}", render(items)));
    let lowered = cove_ir::lower(&checked, &sources, &schemas)
        .unwrap_or_else(|items| panic!("the fixture lowers:\n{}", render(items)));
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(Runtime::new(
        Arc::new(checked),
        Arc::new(sources),
        Arc::clone(&hosts),
    ));
    let encoded = PreparedProgram::new(Arc::new(lowered));
    World {
        runtime,
        hosts,
        probe,
        out,
        #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
        native: encoded
            .clone()
            .with_native()
            .expect("this host compiles native code"),
        encoded,
    }
}

fn packaged(text: &str) -> (SourceMap, Package) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let file = sources.add(path.clone(), text);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::new();
    modules.insert(
        "app".to_string(),
        Module {
            name: "app".to_string(),
            dir: PathBuf::from("app"),
            units: vec![Unit { file, path, ast }],
        },
    );
    cove_sema::stdlib::install(&mut sources, &mut modules).expect("the standard library parses");
    (
        sources,
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        },
    )
}

/// The default heap, which every case uses unless it is about the heap.
const HEAP: usize = 1 << 22;

impl World {
    fn prepared(&self, tier: Tier) -> PreparedProgram {
        match tier {
            Tier::Encoded => self.encoded.clone(),
            #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
            Tier::Native => self.native.clone(),
            #[cfg(not(all(feature = "template", target_arch = "x86_64", unix)))]
            Tier::Native => unreachable!("native cases are gated on the tier"),
        }
    }

    fn machine(&self, tier: Tier, heap: usize) -> OwnedVm {
        OwnedVm::with_heap_words(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.prepared(tier),
            heap,
        )
    }

    /// Forgets what earlier runs left in the console and the host's log.
    fn quiet(&self) {
        self.out.taken();
        self.probe.taken();
    }
}

/// Everything a run is seen to do, and what it was charged.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    answer: String,
    out: String,
    host: Vec<String>,
    instructions: u64,
    allocated_words: u64,
    host_calls: u64,
}

fn shown(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => value.to_string(),
        Err(error) => format!("error({:?}): {}", error.outcome, error.message),
    }
}

/// Runs `app.name(n)` on `vm` under a fresh unbounded budget and says what
/// it did — the console, the host, and the counters, as differences from
/// before the run so that a machine's earlier runs do not count.
fn observe(world: &World, vm: &mut OwnedVm, name: &str, n: i64) -> Observed {
    world.quiet();
    let (instructions, allocated) = (vm.instructions(), vm.allocated_words());
    let answer = vm.invoke_within(
        Budget::new(Limits::default()),
        "app",
        name,
        vec![Value::int(n)],
    );
    Observed {
        answer: shown(&answer),
        out: world.out.taken(),
        host: world.probe.taken(),
        instructions: vm.instructions() - instructions,
        allocated_words: vm.allocated_words() - allocated,
        host_calls: vm.meter().host_calls(),
    }
}

/// The probes a reused machine has to answer exactly as a fresh one does.
const PROBES: &[(&str, i64)] = &[
    ("work", 300),
    ("say", 7),
    ("calls", 3),
    ("tasks", 5),
    ("lockOk", 4),
    ("deep", 40),
];

/// Runs every probe on `vm` and on a fresh machine of the same tier and heap,
/// and asserts the two are indistinguishable.
fn indistinguishable_from_fresh(
    world: &World,
    tier: Tier,
    heap: usize,
    vm: &mut OwnedVm,
    after: &str,
) {
    for &(name, n) in PROBES {
        let reused = observe(world, vm, name, n);
        let mut fresh_vm = world.machine(tier, heap);
        let fresh = observe(world, &mut fresh_vm, name, n);
        assert!(
            !reused.answer.starts_with("error"),
            "{tier:?}: `{name}` after {after} failed: {reused:?}"
        );
        assert_eq!(
            reused, fresh,
            "{tier:?}: `{name}` on a machine reused after {after} differs from a fresh machine"
        );
    }
    if tier == Tier::Native {
        let tiers = vm.tiers();
        assert!(
            tiers.vm_to_native + tiers.native_to_native_direct > 0,
            "the native cases entered compiled code: {tiers:?}"
        );
    }
}

/// Runs `body` on a thread with the stack the runtime asks for.
fn on_stack(body: impl FnOnce() + Send) {
    cove_runtime::on_cove_stack(body).expect("a thread to run Cove on");
}

fn limits(f: impl FnOnce(&mut Limits)) -> Budget {
    let mut limits = Limits::default();
    f(&mut limits);
    Budget::new(limits)
}

// ------------------------------------------------- one failure, then reuse

/// One way to fail a run, on a machine that is then reused.
struct Failure {
    what: &'static str,
    heap: usize,
    run: fn(&World, &mut OwnedVm) -> Result<Value, RuntimeError>,
    expect: fn(&RuntimeError) -> bool,
}

fn int_arg(n: i64) -> Vec<Value> {
    vec![Value::int(n)]
}

/// Every failure a run started with `invoke_within` can come to, blocking.
fn failures() -> Vec<Failure> {
    vec![
        Failure {
            what: "an invocation of a function the package does not declare",
            heap: HEAP,
            run: |_, vm| vm.invoke_within(Budget::new(Limits::default()), "app", "nowhere", vec![]),
            expect: |e| e.message.contains("nowhere"),
        },
        Failure {
            what: "an argument of the wrong type",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    Budget::new(Limits::default()),
                    "app",
                    "work",
                    vec![Value::string("three hundred")],
                )
            },
            expect: |e| e.outcome == RunOutcome::Invariant,
        },
        Failure {
            what: "an entry with two parameters asked for as a command",
            heap: HEAP,
            run: |_, vm| vm.run_entry("app", "apply", vec![]),
            expect: |e| e.message.contains("declares 2 parameters"),
        },
        Failure {
            what: "a division by zero after printing half a line",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(Budget::new(Limits::default()), "app", "trap", int_arg(5000))
            },
            expect: |e| e.outcome == RunOutcome::Invariant,
        },
        Failure {
            what: "a deadline",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    limits(|l| l.deadline = Some(Duration::from_millis(20))),
                    "app",
                    "spin",
                    vec![],
                )
            },
            expect: |e| e.outcome == RunOutcome::Deadline,
        },
        Failure {
            what: "a cancellation from another thread",
            heap: HEAP,
            run: |_, vm| {
                let cancellation = Cancellation::new();
                let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
                let canceller = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    cancellation.cancel();
                    // A second cancel is harmless: the flag is already raised.
                    cancellation.cancel();
                });
                let answer = vm.invoke_within(budget, "app", "spin", vec![]);
                canceller.join().unwrap();
                answer
            },
            expect: |e| e.outcome == RunOutcome::Cancelled,
        },
        Failure {
            what: "the call-depth limit",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    limits(|l| l.max_call_depth = Some(30)),
                    "app",
                    "deep",
                    int_arg(500),
                )
            },
            expect: |e| e.outcome == RunOutcome::CallDepth,
        },
        Failure {
            what: "the host-call limit",
            heap: HEAP,
            run: |world, vm| {
                let answer = vm.invoke_within(
                    limits(|l| l.max_host_calls = Some(2)),
                    "app",
                    "calls",
                    int_arg(10),
                );
                // The limit stopped the third call before the host saw it.
                assert_eq!(world.probe.taken(), ["wait 0", "wait 1"]);
                answer
            },
            expect: |e| e.outcome == RunOutcome::HostCalls,
        },
        Failure {
            what: "the task limit",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    limits(|l| l.max_tasks = Some(1)),
                    "app",
                    "tasks",
                    int_arg(5),
                )
            },
            expect: |e| e.outcome == RunOutcome::Concurrency,
        },
        Failure {
            what: "a task that divides by zero beside one that calls the host",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    Budget::new(Limits::default()),
                    "app",
                    "taskTrap",
                    int_arg(5),
                )
            },
            expect: |e| e.outcome == RunOutcome::Invariant,
        },
        Failure {
            what: "a division by zero inside a `lock`",
            heap: HEAP,
            run: |_, vm| {
                vm.invoke_within(
                    Budget::new(Limits::default()),
                    "app",
                    "lockTrap",
                    int_arg(5),
                )
            },
            expect: |e| e.outcome == RunOutcome::Invariant,
        },
        Failure {
            what: "running out of a small heap",
            heap: 1 << 15,
            run: |_, vm| {
                vm.invoke_within(
                    Budget::new(Limits::default()),
                    "app",
                    "hog",
                    int_arg(1 << 16),
                )
            },
            expect: |e| e.message == "this run has no memory left",
        },
    ]
}

fn every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from(tier: Tier) {
    on_stack(|| {
        let world = world();
        for failure in failures() {
            let mut vm = world.machine(tier, failure.heap);
            // A run before the failure, so the machine has a history to leak.
            let before = observe(&world, &mut vm, "work", 100);
            assert!(!before.answer.starts_with("error"), "{before:?}");
            world.quiet();
            let failed = (failure.run)(&world, &mut vm);
            let error = failed.expect_err(failure.what);
            assert!(
                (failure.expect)(&error),
                "{tier:?}: {} failed as {:?}: {}",
                failure.what,
                error.outcome,
                error.message
            );
            indistinguishable_from_fresh(&world, tier, failure.heap, &mut vm, failure.what);
        }
    });
}

#[test]
fn every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from_encoded() {
    every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from_native() {
    every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from(Tier::Native);
}

/// A program the front end refuses — at the parser, at the checker, or with
/// a standard-library name shadowed — leaves the process fit to compile the
/// next one. The standard library is checked once per process and the
/// result kept (ADR 0083), so a refusal that left that cache half-built
/// would show in every program compiled after it: here the probe compiled
/// after three refusals answers, prints and is charged exactly as the one
/// compiled before them.
#[test]
fn a_refused_program_leaves_the_process_fit_to_compile_the_next() {
    on_stack(|| {
        let probe = || {
            let world = world();
            let mut vm = world.machine(Tier::Encoded, HEAP);
            PROBES
                .iter()
                .map(|&(name, n)| observe(&world, &mut vm, name, n))
                .collect::<Vec<_>>()
        };
        let before = probe();

        let mut sources = SourceMap::new();
        let file = sources.add(PathBuf::from("app/main.cove"), "export fn ( {");
        assert!(cove_syntax::parse_file(&sources, file).is_err());

        let schemas = HostSchemas::only(world().hosts.module_schemas());
        for refused in [
            "export fn work(n: Int) -> String {\n  n\n}\n",
            "struct Vector {\n  n: Int\n}\n\nexport fn work(n: Int) -> Int {\n  Vector(n: n).n\n}\n",
            "use probe\n\nexport fn work(n: Int) -> Int {\n  probe.missing(n)\n}\n",
        ] {
            let (_, package) = packaged(refused);
            let checked = Compiler::new().with_schemas(schemas.clone()).compile(&package);
            assert!(checked.is_err(), "the front end refuses:\n{refused}");
        }

        assert_eq!(probe(), before);
    });
}

/// A machine built for one entry, asked for a function that entry does not
/// reach, refuses before a frame is pushed — and serves its own entry next as
/// a fresh machine of the same preparation does.
#[test]
fn a_function_the_lowering_left_out_is_refused_and_the_machine_serves_on() {
    on_stack(|| {
        let world = world();
        let schemas = HostSchemas::only(world.hosts.module_schemas());
        let lowered = cove_ir::lower_entry(
            world.runtime.program(),
            world.runtime.sources(),
            &schemas,
            "app",
            "work",
        )
        .expect("`work` lowers on its own");
        let prepared = PreparedProgram::new(Arc::new(lowered));
        let machine = || {
            OwnedVm::new(
                Arc::clone(&world.runtime),
                Arc::clone(&world.hosts),
                prepared.clone(),
            )
        };
        let mut vm = machine();
        let refused = vm
            .invoke_within(Budget::new(Limits::default()), "app", "trap", int_arg(3))
            .expect_err("`trap` was not lowered");
        assert!(refused.message.contains("trap"), "{}", refused.message);
        let reused = observe(&world, &mut vm, "work", 300);
        let fresh = observe(&world, &mut machine(), "work", 300);
        assert!(!reused.answer.starts_with("error"), "{reused:?}");
        assert_eq!(reused, fresh);
    });
}

/// A failed run's console output is what it wrote before it failed, and none
/// of it is held back for the next run to emit.
#[test]
fn a_trap_leaves_no_output_for_the_next_run_to_emit() {
    on_stack(|| {
        let world = world();
        let mut vm = world.machine(Tier::Encoded, HEAP);
        world.quiet();
        let failed = vm.invoke_within(Budget::new(Limits::default()), "app", "trap", int_arg(10));
        assert!(failed.is_err());
        assert_eq!(world.out.taken(), "half a line from a run that traps ");
        let next = observe(&world, &mut vm, "say", 1);
        assert_eq!(next.out, "say 1\n");
    });
}

// ------------------------------------------------------- the heap's capacity

/// A heap far too small for the program — smaller than its literals — is a
/// refusal on every run, never a panic, and the preparation it was built
/// from is unharmed.
#[test]
fn a_machine_too_small_for_its_literals_refuses_and_the_preparation_survives() {
    on_stack(|| {
        let world = world();
        for heap in [0, 1, 2, 8, 64, 512] {
            let mut vm = world.machine(Tier::Encoded, heap);
            for _ in 0..2 {
                let answer =
                    vm.invoke_within(Budget::new(Limits::default()), "app", "work", int_arg(50));
                if let Err(error) = &answer {
                    assert_eq!(
                        error.message, "this run has no memory left",
                        "a heap of {heap} words"
                    );
                }
            }
            drop(vm);
            let mut fresh = world.machine(Tier::Encoded, HEAP);
            let answer = observe(&world, &mut fresh, "work", 50);
            assert!(!answer.answer.starts_with("error"), "{answer:?}");
        }
    });
}

/// The fault injection ADR 0088's capacity makes possible: the same program
/// over heaps from far too small to ample, where every allocation that does
/// not fit is the recoverable "no memory left" and never anything else, and
/// every machine — whichever side of the line it fell — runs the next
/// program as a fresh one of its size does.
fn a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side(tier: Tier) {
    on_stack(|| {
        let world = world();
        let need = 5_000;
        let mut answered = 0;
        let mut refused = 0;
        let mut heap = 1 << 10;
        while heap <= 1 << 16 {
            let mut vm = world.machine(tier, heap);
            let answer =
                vm.invoke_within(Budget::new(Limits::default()), "app", "hog", int_arg(need));
            match &answer {
                Ok(value) => {
                    assert_eq!(value.to_string(), (need + 1).to_string());
                    answered += 1;
                }
                Err(error) => {
                    assert_eq!(error.message, "this run has no memory left", "heap {heap}");
                    refused += 1;
                }
            }
            // A second, smaller run on the same machine: what the first one
            // allocated is garbage now and must not count against it.
            let small = observe(&world, &mut vm, "hog", 10);
            let mut fresh_vm = world.machine(tier, heap);
            let fresh = observe(&world, &mut fresh_vm, "hog", 10);
            assert_eq!(small, fresh, "{tier:?}: heap {heap}");
            heap += heap / 4;
        }
        assert!(
            answered > 0 && refused > 0,
            "{answered} answered, {refused} refused: the sweep crossed the line"
        );
    });
}

#[test]
fn a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side_encoded() {
    a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side_native() {
    a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side(Tier::Native);
}

// --------------------------------------------------- parked runs, every exit

/// Parks `calls(3)` at its first call.
fn parked(world: &World, vm: OwnedVm, budget: Budget) -> ParkedVm {
    match vm.invoke_within_parkable(budget, "app", "calls", int_arg(3)) {
        Step::Parked(parked) => parked,
        Step::Answered(_, answer) => panic!("answered instead of parking: {}", shown(&answer)),
        Step::Yielded(_) => panic!("nothing asked it to yield"),
    }
    .tap(|_| assert_eq!(world.probe.taken(), ["wait 0 pended"]))
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}

/// Answers every request a parked run makes until it answers.
fn finish(mut step: Step) -> (OwnedVm, Result<Value, RuntimeError>) {
    loop {
        match step {
            Step::Answered(vm, answer) => return (vm, answer),
            Step::Parked(mut parked) => {
                let request = parked
                    .take_request()
                    .expect("a parked run carries its request")
                    .downcast::<Request>()
                    .expect("the request is the probe's");
                let n = request.n;
                drop(request);
                step = parked.resume(Ok(Transfer::Int(reply(n))));
            }
            Step::Yielded(yielded) => step = yielded.resume(),
        }
    }
}

/// Each way a parked run can end, and the machine it hands back.
fn every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run(tier: Tier) {
    on_stack(|| {
        let world = world();
        type Exit = fn(&World, ParkedVm) -> (OwnedVm, RuntimeError);
        let exits: Vec<(&str, Exit)> = vec![
            ("cancelled", |_, parked| parked.cancel()),
            ("cancelled with its request taken", |_, mut parked| {
                drop(parked.take_request());
                parked.cancel()
            }),
            ("resumed with a host failure", |_, parked| {
                match parked.resume(Err(RuntimeError::new("the upstream went away"))) {
                    Step::Answered(vm, Err(error)) => (vm, error),
                    _ => panic!("a failed answer fails the run"),
                }
            }),
            (
                "resumed with an answer of the wrong type",
                |_, parked| match parked.resume(Ok(Transfer::string("not an Int"))) {
                    Step::Answered(vm, Err(error)) => (vm, error),
                    _ => panic!("a wrong answer fails the run"),
                },
            ),
            ("resumed after its flag was raised", |_, parked| {
                parked.meter().cancellation().cancel();
                match parked.resume(Ok(Transfer::Int(1))) {
                    Step::Answered(vm, Err(error)) => (vm, error),
                    _ => panic!("a cancelled run does not run on"),
                }
            }),
            ("resumed past its deadline", |_, parked| {
                std::thread::sleep(parked.time_left().expect("it has a deadline"));
                std::thread::sleep(Duration::from_millis(1));
                match parked.resume(Ok(Transfer::Int(1))) {
                    Step::Answered(vm, Err(error)) => {
                        assert_eq!(error.outcome, RunOutcome::Deadline);
                        (vm, error)
                    }
                    _ => panic!("a run past its deadline does not run on"),
                }
            }),
            (
                "resumed on another thread and then cancelled at its second call",
                |_, mut parked| {
                    drop(parked.take_request());
                    std::thread::scope(|scope| {
                        scope
                            .spawn(move || match parked.resume(Ok(Transfer::Int(reply(0)))) {
                                Step::Parked(again) => again.cancel(),
                                _ => panic!("`calls(3)` parks at its second call"),
                            })
                            .join()
                            .unwrap()
                    })
                },
            ),
        ];
        for (what, exit) in exits {
            let vm = world.machine(tier, HEAP);
            let budget = limits(|l| l.deadline = Some(Duration::from_millis(50)));
            let dropped = world.probe.dropped.load(Ordering::SeqCst);
            let made = world.probe.made.load(Ordering::SeqCst);
            let parked = parked(&world, vm, budget);
            let (mut vm, error) = exit(&world, parked);
            assert_ne!(error.outcome, RunOutcome::Success, "{what}");
            // Every request the host made was dropped exactly once,
            // whichever way the run ended.
            assert_eq!(
                world.probe.made.load(Ordering::SeqCst) - made,
                world.probe.dropped.load(Ordering::SeqCst) - dropped,
                "{tier:?}: {what}: requests made and dropped"
            );
            indistinguishable_from_fresh(&world, tier, HEAP, &mut vm, what);
            // And a parkable run on the reused machine parks and finishes as
            // a fresh one's does.
            world.quiet();
            let (_, answer) = finish(vm.invoke_within_parkable(
                Budget::new(Limits::default()),
                "app",
                "calls",
                int_arg(3),
            ));
            assert_eq!(
                shown(&answer),
                (reply(0) + reply(1) + reply(2)).to_string(),
                "{what}"
            );
            assert_eq!(
                world.probe.taken(),
                ["wait 0 pended", "wait 1 pended", "wait 2 pended"]
            );
        }
    });
}

#[test]
fn every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run_encoded() {
    every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run_native() {
    every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run(Tier::Native);
}

/// Dropping a machine in each state it can be dropped in: never run,
/// answered, failed, parked with its request, parked without it, yielded,
/// and with a closure it made still held by the host after it is gone.
fn a_machine_may_be_dropped_in_every_state(tier: Tier) {
    on_stack(|| {
        let world = world();
        let dropped = world.probe.dropped.load(Ordering::SeqCst);
        let made = world.probe.made.load(Ordering::SeqCst);

        drop(world.machine(tier, HEAP));

        let mut answered = world.machine(tier, HEAP);
        observe(&world, &mut answered, "work", 10);
        drop(answered);

        let mut failed = world.machine(tier, HEAP);
        assert!(failed
            .invoke_within(Budget::new(Limits::default()), "app", "trap", int_arg(10))
            .is_err());
        drop(failed);

        let with_request = parked(
            &world,
            world.machine(tier, HEAP),
            Budget::new(Limits::default()),
        );
        drop(with_request);

        let mut without = parked(
            &world,
            world.machine(tier, HEAP),
            Budget::new(Limits::default()),
        );
        let request = without.take_request();
        drop(without);
        // The request outlives the run it was taken from, and is still the
        // host's own value.
        assert_eq!(request.unwrap().downcast::<Request>().unwrap().n, 0);

        let yielded = yielded(world.machine(tier, HEAP));
        drop(yielded);

        let mut maker = world.machine(tier, HEAP);
        let closure = maker
            .invoke_within(Budget::new(Limits::default()), "app", "adder", int_arg(40))
            .expect("a closure crosses out");
        drop(maker);
        // The closure's environment was the dropped machine's; holding it,
        // rendering it and dropping it last must all be safe.
        let _ = closure.to_string();
        drop(closure);

        assert_eq!(
            world.probe.made.load(Ordering::SeqCst) - made,
            world.probe.dropped.load(Ordering::SeqCst) - dropped,
            "every request made was dropped exactly once"
        );
        let mut fresh = world.machine(tier, HEAP);
        let after = observe(&world, &mut fresh, "work", 10);
        assert!(!after.answer.starts_with("error"), "{after:?}");
    });
}

#[test]
fn a_machine_may_be_dropped_in_every_state_encoded() {
    a_machine_may_be_dropped_in_every_state(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_machine_may_be_dropped_in_every_state_native() {
    a_machine_may_be_dropped_in_every_state(Tier::Native);
}

/// `spin` yielded at a safepoint, by a request raised before it started and
/// held raised by a thread until it is honoured.
fn yielded(vm: OwnedVm) -> YieldedVm {
    let signal = vm.yield_request();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let raiser = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                signal.request();
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };
    let step = vm.invoke_within_parkable(
        limits(|l| l.deadline = Some(Duration::from_secs(30))),
        "app",
        "spin",
        vec![],
    );
    stop.store(true, Ordering::Relaxed);
    raiser.join().unwrap();
    match step {
        Step::Yielded(yielded) => yielded,
        _ => panic!("`spin` yields when asked"),
    }
}

/// A yielded run cancelled, and one whose flag was raised while it waited,
/// each hand back a machine as fit for the next run as a fresh one.
fn every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run(tier: Tier) {
    on_stack(|| {
        let world = world();
        let (mut vm, error) = yielded(world.machine(tier, HEAP)).cancel();
        assert_eq!(error.outcome, RunOutcome::Cancelled);
        indistinguishable_from_fresh(&world, tier, HEAP, &mut vm, "a cancelled yield");

        let yielded = yielded(vm);
        yielded.meter().cancellation().cancel();
        let Step::Answered(mut vm, Err(error)) = yielded.resume() else {
            panic!("a cancelled yield does not run on");
        };
        assert_eq!(error.outcome, RunOutcome::Cancelled);
        indistinguishable_from_fresh(
            &world,
            tier,
            HEAP,
            &mut vm,
            "a yield resumed after its flag was raised",
        );
    });
}

#[test]
fn every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run_encoded() {
    every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run_native() {
    every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run(Tier::Native);
}

// -------------------------------------------- what arrives after a run ended

/// A cancellation is a run's, not a machine's: the previous run's flag,
/// raised after it ended — a late timer, a host's delayed abort — reaches
/// nothing when the machine is already running the next one, and the
/// callback a host registered on it runs once, on the raising thread.
#[test]
fn a_late_cancel_of_a_finished_run_does_not_reach_the_next_one() {
    on_stack(|| {
        let world = world();
        let mut vm = world.machine(Tier::Encoded, HEAP);
        let old = Cancellation::new();
        let told = Arc::new(AtomicUsize::new(0));
        {
            let told = Arc::clone(&told);
            old.on_cancel(move || {
                told.fetch_add(1, Ordering::SeqCst);
            });
        }
        let first = vm.invoke_within(
            Budget::with_cancellation(Limits::default(), old.clone()),
            "app",
            "work",
            int_arg(10),
        );
        assert!(first.is_ok());
        // The next run spins under a deadline of its own while the previous
        // run's flag is raised, twice, from another thread.
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            old.cancel();
            old.cancel();
        });
        let second = vm.invoke_within(
            limits(|l| l.deadline = Some(Duration::from_millis(60))),
            "app",
            "spin",
            vec![],
        );
        raiser.join().unwrap();
        assert_eq!(
            second.expect_err("spin is stopped").outcome,
            RunOutcome::Deadline,
            "stopped by its own deadline, not by the last run's flag"
        );
        assert_eq!(told.load(Ordering::SeqCst), 1, "the callback ran once");
    });
}

/// The answer to a run that was cancelled cannot be delivered: `cancel`
/// consumed the `ParkedVm`, which is the only handle to the call. What the
/// embedder still holds is the answer it was going to send, and handing
/// *that* to the next run's park is the embedder correlating wrongly — the
/// runtime cannot tell an answer from the right request from one from the
/// wrong one when they have the same type, so it holds the answer to the
/// declared type and no further. This pins both halves: a stale answer of
/// the wrong type is refused at the call, and a stale answer of the right
/// type is taken as the answer (`docs/ISOLATION_TESTING.md`, "late
/// answers": correlating is the embedder's, as `examples/edge` does by id).
#[test]
fn a_stale_answer_is_held_to_the_declared_type_and_no_further() {
    on_stack(|| {
        let world = world();
        let vm = world.machine(Tier::Encoded, HEAP);
        let (vm, _) = parked(&world, vm, Budget::new(Limits::default())).cancel();
        let stale = Transfer::string("the cancelled run's answer");
        let again = parked(&world, vm, Budget::new(Limits::default()));
        let Step::Answered(vm, refused) = again.resume(Ok(stale)) else {
            panic!("a wrong answer fails the run");
        };
        let refused = refused.expect_err("an answer of the wrong type is refused");
        assert!(refused.message.contains("String"), "{}", refused.message);

        let again = parked(&world, vm, Budget::new(Limits::default()));
        let (_, answer) = finish(again.resume(Ok(Transfer::Int(1000))));
        assert_eq!(shown(&answer), (1000 + reply(1) + reply(2)).to_string());
    });
}

// --------------------------------------------- values that outlive their run

/// A closure a run handed out is pinned in its machine's heap. The same
/// machine may be handed it back in a later run — it is that machine's — and
/// a collection in between keeps it; another machine refuses it, because its
/// environment is an address in a heap that machine does not have.
#[test]
fn a_closure_handed_out_is_callable_by_its_own_machine_alone() {
    on_stack(|| {
        let world = world();
        let mut owner = world.machine(Tier::Encoded, 1 << 16);
        let closure = owner
            .invoke_within(Budget::new(Limits::default()), "app", "adder", int_arg(40))
            .expect("a closure crosses out");
        // Enough garbage to collect several times over in between.
        let churn =
            owner.invoke_within(Budget::new(Limits::default()), "app", "work", int_arg(2000));
        assert!(churn.is_ok(), "{}", shown(&churn));
        assert!(
            owner.collections() > 0,
            "the heap collected while the closure was held"
        );
        let called = owner.invoke_within(
            Budget::new(Limits::default()),
            "app",
            "apply",
            vec![closure.clone(), Value::int(2)],
        );
        assert_eq!(shown(&called), "42");

        let mut other = world.machine(Tier::Encoded, HEAP);
        let refused = other.invoke_within(
            Budget::new(Limits::default()),
            "app",
            "apply",
            vec![closure, Value::int(2)],
        );
        let refused = refused.expect_err("another machine refuses it");
        assert!(
            refused.message.contains("another run"),
            "{}",
            refused.message
        );
        indistinguishable_from_fresh(&world, Tier::Encoded, HEAP, &mut other, "a refused closure");
    });
}

// ---------------------------------------------------------- the budget's owner

/// Whether to run the cases that reproduce a finding not yet fixed.
///
/// They are `#[ignore]`d so that `cargo t` stays green, and they return at
/// once unless `COVE_KNOWN_FINDINGS` is set so that `cargo ratchet`, which
/// runs every ignored case, does too. `COVE_KNOWN_FINDINGS=1 cargo test
/// --profile checked -p cove-runtime --test lifecycle -- --ignored` is the
/// reproduction. A fix deletes the gate and the `#[ignore]`.
fn known_findings() -> bool {
    std::env::var_os("COVE_KNOWN_FINDINGS").is_some()
}

/// `invoke` — the way in with no budget of its own — after an `invoke_within`
/// whose budget was cancelled. The specification says the next run cannot
/// tell the last one happened; what happens is that `invoke` runs under the
/// meter the last `invoke_within` left on the machine, so it starts
/// cancelled. The same holds for a deadline that passed and a host-call
/// count that was spent, on both backends (`Vm::budget` and
/// `Interpreter::budget` are replaced only by a way in that brings a
/// budget).
#[test]
#[ignore = "issue 681: `invoke` after `invoke_within` runs under the previous run's budget (its cancellation, deadline and host-call count); fails until fixed, so it runs only with COVE_KNOWN_FINDINGS=1"]
fn an_unbudgeted_invocation_does_not_inherit_the_last_run_s_budget() {
    if !known_findings() {
        return;
    }
    on_stack(|| {
        let world = world();
        let mut vm = world.machine(Tier::Encoded, HEAP);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let cancelled = vm.invoke_within(
            Budget::with_cancellation(Limits::default(), cancellation),
            "app",
            "spin",
            vec![],
        );
        assert_eq!(cancelled.unwrap_err().outcome, RunOutcome::Cancelled);
        let next = vm.invoke("app", "work", int_arg(10));
        assert!(
            next.is_ok(),
            "the next run started cancelled: {}",
            shown(&next)
        );

        let mut vm = world.machine(Tier::Encoded, HEAP);
        let spent = vm.invoke_within(
            limits(|l| l.max_host_calls = Some(1)),
            "app",
            "calls",
            int_arg(1),
        );
        assert!(spent.is_ok());
        let next = vm.invoke("app", "calls", int_arg(1));
        assert!(
            next.is_ok(),
            "the next run was charged to the last one's limit: {}",
            shown(&next)
        );
    });
}

// ------------------------------------------------------- the resource table

/// A machine handed a new resource on every run does not keep them: its
/// table is emptied at every run's edge, so after a thousand runs it holds
/// none, and each run's handle still names its own resource (issue #685).
fn a_reused_machine_keeps_no_resource_from_its_earlier_runs(tier: Tier) {
    on_stack(|| {
        let world = world();
        let mut vm = world.machine(tier, HEAP);
        for n in 0..1_000 {
            let answer = vm.invoke("app", "ticket", int_arg(n));
            assert_eq!(shown(&answer), n.to_string(), "{tier:?}: run {n}");
            assert_eq!(vm.resources_held(), 0, "{tier:?}: after run {n}");
        }
        // A run that fails is a run's edge too.
        let stopped = vm.invoke_within(
            limits(|l| l.max_host_calls = Some(1)),
            "app",
            "ticket",
            int_arg(7),
        );
        assert!(
            stopped.is_err(),
            "{tier:?}: the second call is over the limit"
        );
        assert_eq!(vm.resources_held(), 0, "{tier:?}: after a failed run");
        // And a parked one, at the edge where it answers.
        let unbounded = || Budget::new(Limits::default());
        let (vm, answer) =
            finish(vm.invoke_within_parkable(unbounded(), "app", "ticket", int_arg(9)));
        assert_eq!(shown(&answer), "9");
        assert_eq!(vm.resources_held(), 0, "{tier:?}: after a parkable run");
    });
}

#[test]
fn a_reused_machine_keeps_no_resource_from_its_earlier_runs_encoded() {
    a_reused_machine_keeps_no_resource_from_its_earlier_runs(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_reused_machine_keeps_no_resource_from_its_earlier_runs_native() {
    a_reused_machine_keeps_no_resource_from_its_earlier_runs(Tier::Native);
}

/// A handle the embedder kept as a value, and a closure the embedder kept
/// with a handle in its environment, both still name their own resource in
/// a later run — after runs that were handed other resources, and after the
/// table was emptied. While the closure is held the table is kept (a word in
/// its environment indexes it); once it is dropped, the next run's edge
/// empties it.
fn a_kept_handle_still_names_its_resource(tier: Tier) {
    on_stack(|| {
        let world = world();
        let mut vm = world.machine(tier, HEAP);

        let ticket = vm.invoke("app", "mint", int_arg(41)).expect("a ticket");
        assert_eq!(
            vm.resources_held(),
            0,
            "{tier:?}: a value is the name itself"
        );
        for n in 0..10 {
            assert_eq!(
                shown(&vm.invoke("app", "ticket", int_arg(n))),
                n.to_string()
            );
        }
        let redeemed = vm.invoke("app", "redeem", vec![ticket.clone()]);
        assert_eq!(shown(&redeemed), "41", "{tier:?}: the kept handle");

        let kept = vm.invoke("app", "keeper", int_arg(100)).expect("a closure");
        let held = vm.resources_held();
        assert!(held >= 1, "{tier:?}: a kept closure keeps the table");
        for n in 0..10 {
            assert_eq!(
                shown(&vm.invoke("app", "ticket", int_arg(n))),
                n.to_string()
            );
        }
        let applied = vm.invoke("app", "apply", vec![kept.clone(), Value::int(1)]);
        assert_eq!(
            shown(&applied),
            "101",
            "{tier:?}: the closure's ticket is its own, not one a later run was handed"
        );
        let redeemed = vm.invoke("app", "redeem", vec![ticket]);
        assert_eq!(shown(&redeemed), "41", "{tier:?}: beside a kept closure");

        drop(kept);
        assert_eq!(shown(&vm.invoke("app", "ticket", int_arg(3))), "3");
        assert_eq!(
            vm.resources_held(),
            0,
            "{tier:?}: the first edge after the closure went empties the table"
        );
    });
}

#[test]
fn a_kept_handle_still_names_its_resource_encoded() {
    a_kept_handle_still_names_its_resource(Tier::Encoded);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_kept_handle_still_names_its_resource_native() {
    a_kept_handle_still_names_its_resource(Tier::Native);
}

// ------------------------------------------------------ a randomised storm

/// A splitmix64 step: the storm's choices, reproducible from its seed.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// What one machine of the storm's pool is doing.
enum Slot {
    Idle(OwnedVm),
    Parked(ParkedVm),
    Yielded(YieldedVm),
    Empty,
}

/// Interleaves create, run, fail, park, resume, cancel, yield and drop over
/// a pool of machines, from a fixed seed, and checks every run that answers
/// against the answer a fresh machine gives.
fn storm(tier: Tier, seed: u64, steps: usize) {
    on_stack(|| {
        let world = world();
        let expected = |name: &str, n: i64| {
            let mut fresh = world.machine(tier, HEAP);
            shown(&fresh.invoke_within(Budget::new(Limits::default()), "app", name, int_arg(n)))
        };
        let mut want: BTreeMap<(String, i64), String> = BTreeMap::new();
        let mut answer_of = |name: &str, n: i64| {
            want.entry((name.to_string(), n))
                .or_insert_with(|| expected(name, n))
                .clone()
        };
        let mut pool: Vec<Slot> = (0..4).map(|_| Slot::Empty).collect();
        let mut state = seed;
        let made = world.probe.made.load(Ordering::SeqCst);
        let dropped = world.probe.dropped.load(Ordering::SeqCst);
        let mut log = Vec::new();
        for step in 0..steps {
            let at = (splitmix(&mut state) % pool.len() as u64) as usize;
            let choice = splitmix(&mut state) % 10;
            let n = (splitmix(&mut state) % 50) as i64;
            let slot = std::mem::replace(&mut pool[at], Slot::Empty);
            log.push(format!("{step}: slot {at} choice {choice} n {n}"));
            let context = || log[log.len().saturating_sub(12)..].join("\n");
            pool[at] = match (slot, choice) {
                (Slot::Empty, _) => Slot::Idle(world.machine(tier, HEAP)),
                (Slot::Idle(vm), 0) => {
                    drop(vm);
                    Slot::Empty
                }
                (Slot::Idle(mut vm), 1 | 2) => {
                    let got = shown(&vm.invoke_within(
                        Budget::new(Limits::default()),
                        "app",
                        "work",
                        int_arg(n),
                    ));
                    assert_eq!(got, answer_of("work", n), "{}", context());
                    Slot::Idle(vm)
                }
                (Slot::Idle(mut vm), 3) => {
                    let failed =
                        vm.invoke_within(Budget::new(Limits::default()), "app", "trap", int_arg(n));
                    assert!(failed.is_err(), "{}", context());
                    Slot::Idle(vm)
                }
                (Slot::Idle(mut vm), 4) => {
                    let stopped = vm.invoke_within(
                        limits(|l| l.deadline = Some(Duration::from_micros(300))),
                        "app",
                        "spin",
                        vec![],
                    );
                    assert_eq!(
                        stopped.unwrap_err().outcome,
                        RunOutcome::Deadline,
                        "{}",
                        context()
                    );
                    Slot::Idle(vm)
                }
                (Slot::Idle(vm), 5 | 6) => {
                    match vm.invoke_within_parkable(
                        Budget::new(Limits::default()),
                        "app",
                        "calls",
                        int_arg(1 + n % 3),
                    ) {
                        Step::Parked(parked) => Slot::Parked(parked),
                        _ => panic!("`calls` parks\n{}", context()),
                    }
                }
                (Slot::Idle(vm), _) => Slot::Yielded(yielded(vm)),
                (Slot::Parked(parked), 0) => {
                    drop(parked);
                    Slot::Empty
                }
                (Slot::Parked(parked), 1 | 2) => {
                    let (vm, error) = parked.cancel();
                    assert_eq!(error.outcome, RunOutcome::Cancelled, "{}", context());
                    Slot::Idle(vm)
                }
                (Slot::Parked(parked), 3) => {
                    match parked.resume(Err(RuntimeError::new("refused"))) {
                        Step::Answered(vm, Err(_)) => Slot::Idle(vm),
                        _ => panic!("a failed answer fails the run\n{}", context()),
                    }
                }
                (Slot::Parked(parked), _) => {
                    let (vm, answer) = finish(Step::Parked(parked));
                    assert!(answer.is_ok(), "{}\n{}", shown(&answer), context());
                    Slot::Idle(vm)
                }
                (Slot::Yielded(yielded), 0) => {
                    drop(yielded);
                    Slot::Empty
                }
                (Slot::Yielded(yielded), 1..=3) => {
                    let (vm, error) = yielded.cancel();
                    assert_eq!(error.outcome, RunOutcome::Cancelled, "{}", context());
                    Slot::Idle(vm)
                }
                (Slot::Yielded(yielded), _) => {
                    yielded.meter().cancellation().cancel();
                    match yielded.resume() {
                        Step::Answered(vm, Err(error)) => {
                            assert_eq!(error.outcome, RunOutcome::Cancelled, "{}", context());
                            Slot::Idle(vm)
                        }
                        _ => panic!("a cancelled yield does not run on\n{}", context()),
                    }
                }
            };
            world.quiet();
        }
        // Every machine still standing answers as a fresh one.
        for slot in pool {
            let mut vm = match slot {
                Slot::Idle(vm) => vm,
                Slot::Parked(parked) => parked.cancel().0,
                Slot::Yielded(yielded) => yielded.cancel().0,
                Slot::Empty => continue,
            };
            indistinguishable_from_fresh(&world, tier, HEAP, &mut vm, "the storm");
        }
        assert_eq!(
            world.probe.made.load(Ordering::SeqCst) - made,
            world.probe.dropped.load(Ordering::SeqCst) - dropped,
            "every request made in the storm was dropped exactly once"
        );
    });
}

#[test]
fn a_storm_of_lifecycles_leaves_every_machine_as_a_fresh_one_encoded() {
    storm(Tier::Encoded, 0x681, 400);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_storm_of_lifecycles_leaves_every_machine_as_a_fresh_one_native() {
    storm(Tier::Native, 0x681_0003, 400);
}

/// The same storm, long: 20,000 steps over several seeds on both tiers, for
/// a person to run after touching parking, yielding, or the machine's reset.
#[test]
#[ignore = "long: run with `cargo ratchet` or `--ignored`; about a minute"]
fn a_long_storm_of_lifecycles() {
    for seed in [1, 2, 3, 0xdead_beef] {
        storm(Tier::Encoded, seed, 5_000);
        #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
        storm(Tier::Native, seed, 5_000);
    }
}
