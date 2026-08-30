//! What a collection *does* is not something a program can print, so these
//! read the heap rather than the console. The programs themselves — a
//! cycle built and discarded, a graph one member of which stays rooted, a
//! capture that is the only root left, a place written through across a
//! collection, a body the host runs re-entrantly — are in the differential
//! corpus as `tests/e2e:gc_*`, where both backends run them and their
//! answers are compared. These are the other half: the same shapes, asked
//! what the heap made of them.

use super::*;

/// A sink that keeps every event, so a test can assert on what a run
/// recorded rather than on how it was formatted.
///
/// Task threads record through the same sink as the entry, so this is
/// shared and locked exactly as the real ones are.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<TraceEvent>>>);

impl crate::trace::TraceSink for Recorder {
    fn record(&self, event: TraceEvent) {
        self.0
            .lock()
            .expect("no test panics while tracing")
            .push(event);
    }
}

impl Recorder {
    fn events(&self) -> Vec<TraceEvent> {
        self.0.lock().expect("no test panics while tracing").clone()
    }
}

/// One backend's run, together with what its heaps did.
struct HeapRun {
    answer: Result<String, RuntimeError>,
    output: String,
    events: Vec<TraceEvent>,
}

impl HeapRun {
    /// Every collection the run recorded, as `(task, allocated, freed)`.
    fn collections(&self) -> Vec<(u64, u64, u64)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                TraceEvent::HeapCollected {
                    task,
                    allocated,
                    freed,
                    ..
                } => Some((*task, *allocated, *freed)),
                _ => None,
            })
            .collect()
    }

    /// Objects every collection of the run reclaimed between them.
    fn freed(&self) -> u64 {
        self.collections().iter().map(|(_, _, freed)| freed).sum()
    }

    /// The run's `heap_summary`, which is always its last heap event.
    fn summary(&self) -> HeapStats {
        self.events
            .iter()
            .rev()
            .find_map(|event| match event {
                TraceEvent::HeapSummary {
                    allocated,
                    allocated_bytes,
                    collections,
                    live_bytes,
                    peak_bytes,
                    pause,
                } => Some(HeapStats {
                    allocated_objects: *allocated,
                    allocated_bytes: *allocated_bytes,
                    collections: *collections,
                    freed_objects: 0,
                    live_bytes: *live_bytes,
                    live_objects: 0,
                    peak_bytes: *peak_bytes,
                    pause: *pause,
                }),
                _ => None,
            })
            .expect("a run ends with a heap summary")
    }

    fn value(&self) -> &str {
        match &self.answer {
            Ok(rendered) => rendered,
            Err(error) => panic!("the program ran without a runtime error: {error:?}"),
        }
    }
}

/// Runs `m.main` on the oracle, watching every heap.
fn interpreted_heap(checked: &Arc<Checked>, sources: &Arc<SourceMap>) -> HeapRun {
    let buffer = Buffer::default();
    let recorder = Recorder::default();
    let runtime = Runtime::new(checked.clone(), sources.clone(), hosts(&buffer, None))
        .with_trace(Arc::new(recorder.clone()));
    let answer = Interpreter::new(&runtime).run_entry("m", "main", Vec::new());
    HeapRun {
        answer: described(answer),
        output: buffer.text(),
        events: recorder.events(),
    }
}

/// Lowers the program and runs `m.main` on the VM, watching every heap.
fn lowered_heap(checked: &Arc<Checked>, sources: &Arc<SourceMap>) -> HeapRun {
    let program = match cove_ir::lower::lower(checked) {
        Ok(program) => program,
        Err(why) => panic!("the program lowers, but stopped at {why}"),
    };
    let entry = program
        .function_named("m", "main")
        .expect("`m.main` was lowered");
    let buffer = Buffer::default();
    let recorder = Recorder::default();
    let hosts = hosts(&buffer, None);
    let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone())
        .with_trace(Arc::new(recorder.clone()));
    let answer = Vm::new(&runtime, &hosts, &Arc::new(program)).run(entry, Vec::new());
    HeapRun {
        answer: described(answer),
        output: buffer.text(),
        events: recorder.events(),
    }
}

/// Runs one program on both backends, watching every heap on each.
///
/// Two runs rather than one, because the question these tests ask is
/// whether the two backends' heaps behave the same, and a figure is only
/// evidence of that beside the other backend's.
fn heaps_of(source: &str) -> (HeapRun, HeapRun) {
    let (sources, checked) = checked_module(source);
    crate::on_cove_stack(|| {
        (
            interpreted_heap(&checked, &sources),
            lowered_heap(&checked, &sources),
        )
    })
    .expect("a thread to run Cove on")
}

/// Enough abandoned objects for a heap to have collected several times.
const CHURN: usize = 200;

/// A loop that builds one cycle per turn and abandons it.
fn churn(count: usize) -> String {
    format!(
        "  var i = 0\n  while i < {count} {{\n    var v = Vector.of()\n    v.push(v)\n    i += 1\n  }}\n"
    )
}

/// `m.main`, returning `Result<Unit, Error>`, around `body`.
fn collecting(body: &str) -> String {
    format!(
        "use console.println\n\nexport fn main() -> Result<Unit, Error> {{\n{body}  Ok(())\n}}\n"
    )
}

/// Asserts that two heaps mean the same thing by what they report.
///
/// # What is equal, and what is not allowed to be
///
/// A program allocates the objects it allocates on either backend, and
/// ends holding what it ends holding, so `allocated_objects`,
/// `allocated_bytes` and `live_bytes` are compared exactly. Those are the
/// figures issue #119 is about: a run that reports cumulative allocation
/// where the other reports live memory is the thing that was wrong.
///
/// How many collections it took to get there is *not* compared, and it is
/// worth saying why rather than leaving the omission to be read as
/// laxity. A collection happens at a safepoint where enough has been
/// allocated since the last one, and the two backends put safepoints in
/// different places — the interpreter takes one at every loop turn, the
/// VM at the first back edge with `BACK_EDGE_FUEL` gathered — so the VM
/// asks the question less often and can overshoot the threshold further
/// before it asks. Fewer collections over the same allocation is what
/// that looks like, and it is a schedule and not a semantics. The same
/// goes for how much each collection reclaimed: an acyclic vector that
/// `Rc` frees before any collection sees it is never counted as swept, so
/// what the sweeps add up to depends on when they ran.
///
/// The peak is not compared either, and it differs by more than a
/// margin. It is the largest live set some collection measured, and the
/// two backends stand in different places when one runs: a `var v`
/// declared inside a loop body is out of the interpreter's environment
/// chain by the time the turn's safepoint is reached, and is still the
/// VM frame's slot until the slot is written again. So a churn loop can
/// report a peak of zero on the oracle and of one vector here, and
/// neither is wrong. Callers that care bound it rather than equate it.
///
/// What *is* required of the schedule is that both backends collect where
/// the other does and reclaim where the other does. A backend that
/// collected only once, or that never freed anything, would pass every
/// exact comparison above and be the bug this whole change is about.
fn same_heap(ast: &HeapRun, vm: &HeapRun) {
    let (a, v) = (ast.summary(), vm.summary());
    assert_eq!(
        a.allocated_objects, v.allocated_objects,
        "allocation differs:\n  ast {a:?}\n  vm  {v:?}"
    );
    assert_eq!(
        a.allocated_bytes, v.allocated_bytes,
        "allocated bytes differ:\n  ast {a:?}\n  vm  {v:?}"
    );
    assert_eq!(
        a.live_bytes, v.live_bytes,
        "live bytes differ:\n  ast {a:?}\n  vm  {v:?}"
    );
    assert_eq!(
        a.collections > 0,
        v.collections > 0,
        "one backend collected and the other did not:\n  ast {a:?}\n  vm  {v:?}"
    );
    assert_eq!(
        ast.freed() > 0,
        vm.freed() > 0,
        "one backend reclaimed and the other did not:\n  ast {:?}\n  vm  {:?}",
        ast.collections(),
        vm.collections()
    );
}

/// The whole reason for a collector, on this backend for the first time.
/// `Rc` cannot free a vector that holds itself, so without a mark and a
/// sweep every one of these would still be live when the run ended.
#[test]
fn a_cycle_a_lowered_program_built_is_reclaimed() {
    let (ast, vm) = heaps_of(&collecting(&churn(CHURN)));
    assert_eq!(vm.value(), ast.value());
    assert!(
        vm.freed() > 0,
        "the VM reclaimed nothing: {:?}",
        vm.collections()
    );
    assert_eq!(
        vm.summary().live_bytes,
        0,
        "the run ended holding something: {:?}",
        vm.summary()
    );
    same_heap(&ast, &vm);
}

/// Allocation that is discarded as fast as it is made leaves a live set
/// that does not grow. Cove has no memory limit for a run to be stopped
/// by, so "bounded" is read off the peak the collections measured rather
/// than off a budget: the peak is a handful of objects where the total
/// allocated is hundreds.
#[test]
fn repeated_allocation_and_discard_leaves_a_bounded_live_set() {
    let (ast, vm) = heaps_of(&collecting(
        "  var total = 0\n  var i = 0\n  while i < 400 {\n    var v = Vector.of()\n    v.push(i)\n    total += v.length()\n    i += 1\n  }\n  println(\"{total}\")?\n",
    ));
    assert_eq!(vm.output, "400\n");
    assert_eq!(ast.output, vm.output);
    let summary = vm.summary();
    assert_eq!(summary.allocated_objects, 400);
    assert!(summary.collections > 0, "{summary:?}");
    assert!(
        summary.peak_bytes < summary.allocated_bytes / 10,
        "the live set grew with the loop: {summary:?}"
    );
    same_heap(&ast, &vm);
}

/// A frame's value window is the root set, so a slot the running frame
/// still holds survives however many collections run beside it.
#[test]
fn a_slot_a_standing_frame_holds_survives_every_collection() {
    let (ast, vm) = heaps_of(&collecting(&format!(
        "  var kept = Vector.of(1, 2, 3)\n{}  println(\"kept {{kept.length()}} {{kept}}\")?\n",
        churn(CHURN)
    )));
    assert_eq!(vm.output, "kept 3 [1, 2, 3]\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// A capture is a value slot, copied into the frame's window by the call
/// that entered the body — so a vector reachable only through a returned
/// closure's capture is reachable from the value stack, and survives.
#[test]
fn a_closure_capture_is_the_only_root_and_is_enough() {
    let (ast, vm) = heaps_of(&format!(
        "use console.println\n\nfn hidden() -> fn() -> Int {{\n  var counted = Vector.of(1, 2, 3, 4, 5)\n  fn() {{\n    counted.length()\n  }}\n}}\n\nexport fn main() -> Result<Unit, Error> {{\n  let count = hidden()\n{}  println(\"{{count()}}\")?\n  Ok(())\n}}\n",
        churn(CHURN)
    ));
    assert_eq!(vm.output, "5\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// Frames standing above frames, with a value operand of the outermost
/// still on the stack when the innermost collects.
///
/// `Vector.of` evaluates its first argument, leaves it standing as an
/// operand, and only then makes the call that churns — so the collection
/// happens with a vector that is an operand rather than a slot. A root
/// set that walked frame windows and stopped at `value_frame_size` would
/// miss it, which is why the whole of `stack[..len]` is the root set.
#[test]
fn a_live_operand_above_nested_frames_survives_a_collection() {
    let (ast, vm) = heaps_of(&format!(
        "use console.println\n\nfn made(n: Int) -> Vector<Int> {{\n  var v = Vector.of()\n  var i = 0\n  while i < n {{\n    v.push(i)\n    i += 1\n  }}\n  v\n}}\n\nfn afterChurn(n: Int) -> Vector<Int> {{\n{}  made(n)\n}}\n\nexport fn main() -> Result<Unit, Error> {{\n  let both: Vector<Vector<Int>> = Vector.of(made(3), afterChurn(4))\n  println(\"{{both}}\")?\n  Ok(())\n}}\n",
        churn(CHURN)
    ));
    assert_eq!(vm.output, "[[0, 1, 2], [0, 1, 2, 3]]\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// A place is a slot number rather than an independent root, so what it
/// names has to stay rooted by the stack that slot is in for the whole
/// of the place's life. A callee collecting between two writes through
/// one is where that is either true or not.
#[test]
fn a_var_place_is_written_through_across_a_collection() {
    let (ast, vm) = heaps_of(&format!(
        "use console.println\n\nfn fill(var output: Vector<Int>, upTo: Int) {{\n  var n = 0\n  while n < upTo {{\n    output.push(n)\n{}    n += 1\n  }}\n}}\n\nexport fn main() -> Result<Unit, Error> {{\n  var output = Vector.of()\n  fill(var output, upTo: 4)\n  println(\"{{output}}\")?\n  Ok(())\n}}\n",
        churn(80)
    ));
    assert_eq!(vm.output, "[0, 1, 2, 3]\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// The same question asked of a place rooted at a *scalar* slot, which is
/// what issue #162 added and which the collector must go on ignoring.
///
/// A scalar-rooted place reaches an `i64` and so reaches nothing the
/// collector owns. What has to survive is the other direction: the frames
/// standing under it hold real values, a collection runs between two writes
/// through the place, and the answer and the heap have to be the oracle's
/// either way. `Vm::places` is where the argument is, and this is where it
/// is run.
#[test]
fn a_scalar_var_place_is_written_through_across_a_collection() {
    let (ast, vm) = heaps_of(&format!(
        "use console.println\n\nfn count(var total: Int, upTo: Int) {{\n  var n = 0\n  while n < upTo {{\n    total += n\n{}    n += 1\n  }}\n}}\n\nexport fn main() -> Result<Unit, Error> {{\n  var total = 0\n  var held = Vector.of(1, 2, 3)\n  count(var total, upTo: 4)\n  println(\"{{total}} {{held.length()}}\")?\n  Ok(())\n}}\n",
        churn(80)
    ));
    assert_eq!(vm.output, "6 3\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// A host running a Cove body re-entrantly takes the closure off the
/// stack into a vector of its own, so while the body runs the closure and
/// everything it captured are held only by the host — where no root set
/// can read them. What keeps them is that a reference nothing can read is
/// a reference the collector's counting is short of.
#[test]
fn a_collection_inside_host_reentry_keeps_what_the_host_is_holding() {
    let (ast, vm) = heaps_of(&format!(
        "use clock.timeout\nuse console.println\n\nexport fn main() -> Result<Unit, Error> {{\n  var kept = Vector.of(1, 2, 3)\n  let answered = timeout(60s) {{\n{}    kept.length()\n  }}?\n  println(\"{{answered}} {{kept}}\")?\n  Ok(())\n}}\n",
        churn(CHURN)
    ));
    assert_eq!(vm.output, "3 [1, 2, 3]\n");
    assert_eq!(ast.output, vm.output);
    assert!(vm.freed() > 0, "{:?}", vm.collections());
    same_heap(&ast, &vm);
}

/// Each spawned task has a VM and a heap of its own, so each collects on
/// its own thread, and the event says whose heap it was. A collection
/// that reached across the boundary would empty a vector another task is
/// still holding.
#[test]
fn each_spawned_task_collects_its_own_heap() {
    let (ast, vm) = heaps_of(&format!(
        "use console.println\n\nfn work(mark: Int) -> Int {{\n  var kept = Vector.of(mark, mark, mark)\n{}  kept.length() * 100 + mark\n}}\n\nexport fn main() -> Result<Unit, Error> {{\n  scope tasks {{\n    let one = tasks.spawn {{ work(1) }}\n    let two = tasks.spawn {{ work(2) }}\n    println(\"{{one.await()}} {{two.await()}}\")?\n  }}\n  Ok(())\n}}\n",
        churn(CHURN)
    ));
    assert_eq!(vm.output, "301 302\n");
    assert_eq!(ast.output, vm.output);
    let collected: BTreeSet<u64> = vm
        .collections()
        .into_iter()
        .filter(|(_, _, freed)| *freed > 0)
        .map(|(task, _, _)| task)
        .collect();
    assert!(
        collected.contains(&1) && collected.contains(&2),
        "both tasks should have collected: {collected:?}"
    );
    same_heap(&ast, &vm);
}

/// A heap dies with the thread that owns it, and a table of `Weak`s
/// dropped without a sweep takes nothing with it — so a task that ends
/// while a cycle it built is still reachable would leave that cycle
/// behind. `Vm::retire_heap` sweeps once more, which is what makes a
/// task's memory a task's to give back.
#[test]
fn a_task_that_ends_still_naming_a_cycle_leaves_nothing_behind() {
    let (ast, vm) = heaps_of(
        "use console.println\n\nstruct Node {\n  next: Vector<Node>\n}\n\nfn holds() -> Int {\n  var kept: Vector<Node> = Vector.of()\n  kept.push(Node(next: kept))\n  kept.length()\n}\n\nexport fn main() -> Result<Unit, Error> {\n  scope tasks {\n    let one = tasks.spawn { holds() }\n    println(\"{one.await()}\")?\n  }\n  Ok(())\n}\n",
    );
    assert_eq!(vm.output, "1\n");
    let summary = vm.summary();
    assert_eq!(
        vm.freed(),
        summary.allocated_objects,
        "a cycle outlived the task that built it: {summary:?}"
    );
    same_heap(&ast, &vm);
}

/// ADR 0011 asks allocation, live heap size, collection count, and pause
/// time to be trace events, and #119 asks the two backends to mean one
/// thing by them. This is the run that produces all four on this one.
#[test]
fn the_trace_carries_allocation_the_live_heap_collections_and_pause() {
    let (ast, vm) = heaps_of(&collecting(&format!(
        "  var kept = Vector.of(1)\n{}",
        churn(CHURN)
    )));
    let collections = vm.collections();
    assert!(!collections.is_empty(), "no collection was recorded");
    for (_, allocated, _) in &collections {
        assert!(*allocated > 0, "a collection recorded no allocation");
    }
    let summary = vm.summary();
    assert_eq!(summary.allocated_objects, CHURN as u64 + 1);
    assert!(summary.allocated_bytes > 0);
    assert_eq!(summary.collections, collections.len() as u64);
    // What the run ended holding is nothing: the entry's frame went with
    // it, and retiring its heap swept what the frame had been holding.
    assert_eq!(summary.live_bytes, 0);
    assert!(summary.peak_bytes > 0, "the kept vector was live");
    assert!(
        summary.pause > Duration::ZERO,
        "a collection took no time at all"
    );
    same_heap(&ast, &vm);
}

/// A program that allocates nothing collectable pays for no collection at
/// all, and says so in the same figures the other backend says it in.
#[test]
fn a_program_that_allocates_nothing_is_never_collected() {
    let (ast, vm) = heaps_of(&collecting("  println(\"{1 + 1}\")?\n"));
    assert_eq!(vm.output, "2\n");
    assert_eq!(vm.summary().collections, 0);
    assert_eq!(vm.summary().allocated_objects, 0);
    assert!(vm.collections().is_empty());
    same_heap(&ast, &vm);
}
