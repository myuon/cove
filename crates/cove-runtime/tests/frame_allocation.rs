//! Issue #212's fourth acceptance criterion, counted rather than asserted:
//! **direct calls and returns allocate nothing after warm capacity.**
//!
//! A global allocator counts every `alloc` and every `realloc` this process
//! makes. It lives in a test binary of its own so that nothing else in the
//! suite pays for it, and it is a count rather than a byte figure because
//! what is being asked is whether a call *touches* the allocator at all.
//!
//! # Why a difference and not one absolute
//!
//! An absolute would be a number about the whole run — the trace's four
//! events each turn a module name into a `String`, `on_cove_stack` spawns a
//! thread, and a `HeapSummary` is built whatever the program did. None of
//! that is a call.
//!
//! So the measurement is a *difference*: the same program run over ten
//! thousand calls and over twenty thousand. Everything that is not a call is
//! identical between the two, so if the two counts are equal then the ten
//! thousand extra calls allocated nothing, and if they are not the difference
//! is exactly what a call costs. This needs no access to anything private and
//! cannot be fooled by a warm-up that happened to hide a cost.
//!
//! There are two such differences here. The first is Phase A's — ten thousand
//! extra **calls** — and the second is Phase B's — ten thousand extra **struct
//! field writes**, which is the workload a rooted frame exists for. Both are
//! taken on the frame and on the `Vm` beside it, in one process, because a
//! zero is only worth reading beside something that is not one.
//!
//! `crates/cove-runtime/src/frame.rs` is the backend and its module docs are
//! the calling convention this is checking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::frame::FrameVm;
use cove_runtime::host::{Grants, HostRegistry};
use cove_runtime::{on_cove_stack, Runtime, Vm};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};

// --------------------------------------------------------- the counter

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

/// The system allocator, with a counter in front of it.
///
/// `realloc` counts because growing a `Vec` past its capacity is exactly the
/// thing this test is looking for: a stack that reallocated under a call is a
/// call that allocated.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `body` with the counter on, and answers how many allocations it made.
fn counted<T>(body: impl FnOnce() -> T) -> (T, u64) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let answer = body();
    COUNTING.store(false, Ordering::Relaxed);
    (answer, ALLOCATIONS.load(Ordering::Relaxed))
}

// ------------------------------------------------------------ the workload

/// One free function called once a turn, and two entries that turn the loop a
/// different number of times.
///
/// Everything but the number of calls is the same between them: the same
/// callee, the same loop, the same epilogue, the same trace events.
const SOURCE: &str = "\
fn identity(value: Int) -> Int {
  value
}

fn work(turns: Int) -> Int {
  var total = 0
  var i = 0
  while i < turns {
    total = total + identity(i)
    i = i + 1
  }
  total
}

export fn main() -> Int {
  work(10000)
}

export fn twice() -> Int {
  work(20000)
}

struct Cell {
  at: Int
  step: Int
}

fn writes(turns: Int) -> Int {
  var cell = Cell(at: 0, step: 1)
  var i = 0
  while i < turns {
    cell.at = cell.at + cell.step
    i = i + 1
  }
  cell.at
}

export fn fields() -> Int {
  writes(10000)
}

export fn fieldsTwice() -> Int {
  writes(20000)
}
";

fn prepared() -> (
    Arc<SourceMap>,
    Arc<cove_sema::resolve::Program>,
    Arc<cove_ir::Program>,
) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the source parses");
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules: BTreeMap::from([(
            "m".to_string(),
            Module {
                name: "m".to_string(),
                dir: PathBuf::from("m"),
                units: vec![Unit { file, path, ast }],
            },
        )]),
    };
    let checked = cove_sema::Compiler::new()
        .compile(&package)
        .expect("the source checks");
    let ir = cove_ir::lower::lower(&checked).expect("it lowers");
    cove_ir::lower::validate(&ir).expect("it holds the invariants");
    (Arc::new(sources), Arc::new(checked), Arc::new(ir))
}

/// **Ten thousand more calls allocate nothing** — on the eight-byte frame,
/// and on the `Vm` beside it.
///
/// The two entries differ by exactly ten thousand calls and ten thousand
/// returns, and by nothing else. The counts are equal, so a call and a return
/// reach the allocator zero times once the stack's capacity is warm — which
/// is what issue #212 asks for and what "there is no argument vector" means
/// in practice.
///
/// One test rather than two, because the counter is a global: two tests would
/// run on two threads of one process and count each other's allocations,
/// which is what the first draft of this file did and what its numbers were.
///
/// The `Vm` half is here because a reader of the frame's number will want to
/// know whether it is a property of the new arrangement or of both. It is of
/// both: the `Vm` also resizes windows of stacks it already owns, and issue
/// #212's expectation of zero was never a prediction that the old
/// arrangement allocated. What differs between them is the *width* of the
/// window and what moving through it costs, and that is what the wall-clock
/// rows measure.
#[test]
fn a_call_reaches_the_allocator_zero_times_on_either_backend() {
    let (sources, checked, ir) = prepared();
    let Counts {
        frame_ten,
        frame_twenty,
        vm_ten,
        vm_twenty,
        frame_fields_ten,
        frame_fields_twenty,
        vm_fields_ten,
        vm_fields_twenty,
    } = on_cove_stack(move || {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(checked, sources, hosts.clone());

        let mut frame = FrameVm::new(&runtime, &hosts, &ir);
        // Both entries first, so that the stack's capacity, the constant pool
        // and every lazily built table are already there at the deepest
        // either of them reaches. What is being measured is the steady state,
        // which is the state a benchmark measures.
        frame
            .run_entry("m", "main", Vec::new())
            .expect("it answers");
        frame
            .run_entry("m", "twice", Vec::new())
            .expect("it answers");
        let (_, frame_ten) = counted(|| frame.run_entry("m", "main", Vec::new()));
        let (_, frame_twenty) = counted(|| frame.run_entry("m", "twice", Vec::new()));

        // The same difference over a loop that writes a struct field a turn,
        // which is Phase B's workload: a field write is a copy, so ten
        // thousand extra turns are ten thousand extra objects in the traced
        // heap and however many collections the pacing decides.
        frame
            .run_entry("m", "fields", Vec::new())
            .expect("it answers");
        frame
            .run_entry("m", "fieldsTwice", Vec::new())
            .expect("it answers");
        let (_, frame_fields_ten) = counted(|| frame.run_entry("m", "fields", Vec::new()));
        let (_, frame_fields_twenty) = counted(|| frame.run_entry("m", "fieldsTwice", Vec::new()));

        let mut vm = Vm::new(&runtime, &hosts, &ir);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");
        vm.run_entry("m", "twice", Vec::new()).expect("it answers");
        let (_, vm_ten) = counted(|| vm.run_entry("m", "main", Vec::new()));
        let (_, vm_twenty) = counted(|| vm.run_entry("m", "twice", Vec::new()));

        let mut vm = Vm::new(&runtime, &hosts, &ir);
        vm.run_entry("m", "fields", Vec::new()).expect("it answers");
        vm.run_entry("m", "fieldsTwice", Vec::new())
            .expect("it answers");
        let (_, vm_fields_ten) = counted(|| vm.run_entry("m", "fields", Vec::new()));
        let (_, vm_fields_twenty) = counted(|| vm.run_entry("m", "fieldsTwice", Vec::new()));

        Counts {
            frame_ten,
            frame_twenty,
            vm_ten,
            vm_twenty,
            frame_fields_ten,
            frame_fields_twenty,
            vm_fields_ten,
            vm_fields_twenty,
        }
    })
    .expect("a thread to run Cove on");

    assert_eq!(
        frame_ten,
        frame_twenty,
        "on the 8-byte frame, twenty thousand calls allocated {frame_twenty} time(s) and ten \
         thousand allocated {frame_ten}, so a call costs {} allocation(s)",
        (frame_twenty as i64 - frame_ten as i64) as f64 / 10_000.0
    );
    assert_eq!(
        vm_ten, vm_twenty,
        "on the VM, twenty thousand calls allocated {vm_twenty} time(s) and ten thousand \
         allocated {vm_ten}"
    );
    // And the fixed cost of a run is small, so the equalities above are a real
    // zero rather than two large numbers that happened to match.
    // Eight on the frame and four on the VM, as this is written: the trace's
    // four events and the answer, plus the `admits` walk this backend runs on
    // the way into every run and the other two have no equivalent of.
    assert!(
        frame_ten < 64 && vm_ten < 64,
        "a run's fixed cost is {frame_ten} allocation(s) on the frame and {vm_ten} on the VM, \
         which is more than the trace's events and the answer can account for"
    );

    // --------------------------------------------- and a struct field write
    //
    // **Ten thousand more field writes allocate nothing on the frame, and
    // twenty thousand times on the VM.** That is the sharpest single number
    // Phase B produced and it is not about the frame at all: it is about where
    // a struct's field *names* live. A `Vm` struct is an `Rc<StructValue>`
    // holding a `Vec<(Rc<str>, Value)>`, so writing a field through
    // `Rc::make_mut` copies both the cell and the vector, twice per turn; a
    // traced-heap object is a layout id and a run of words, so the same write
    // is a copy of two words into an entry the free list handed back.
    //
    // ADR 0028 decision 2 is what makes the difference available -- "what it
    // names carries a layout id, the object's size, its reference map, its
    // payload layout" in *VM-owned metadata* rather than in every object --
    // and this is what that sentence is worth on one loop.
    assert_eq!(
        frame_fields_ten,
        frame_fields_twenty,
        "on the 8-byte frame, twenty thousand field writes allocated \
         {frame_fields_twenty} time(s) and ten thousand allocated \
         {frame_fields_ten}, so a field write costs {} allocation(s)",
        (frame_fields_twenty as i64 - frame_fields_ten as i64) as f64 / 10_000.0
    );
    // The VM's is the control that says the zero above is a property of this
    // arrangement rather than of the workload. It is asserted as a range
    // rather than as a figure because what is being said is "this allocates
    // per turn and the other does not", and the exact multiple is
    // `Rc::make_mut`'s business.
    let per_write = (vm_fields_twenty as i64 - vm_fields_ten as i64) as f64 / 10_000.0;
    assert!(
        per_write >= 1.0,
        "the VM's field write is supposed to allocate, and it cost {per_write} \
         allocation(s) over ten thousand extra turns; if it no longer does, the \
         frame's zero is no longer a comparison"
    );
}

/// What one process's counting produced, named rather than positional because
/// eight `u64`s in a tuple is eight chances to swap two.
struct Counts {
    frame_ten: u64,
    frame_twenty: u64,
    vm_ten: u64,
    vm_twenty: u64,
    frame_fields_ten: u64,
    frame_fields_twenty: u64,
    vm_fields_ten: u64,
    vm_fields_twenty: u64,
}
