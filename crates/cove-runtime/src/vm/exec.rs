//! The dispatch loop.
//!
//! One `Machine` runs one task's frames over one [`Memory`]. It is a
//! register machine: every instruction names its operands and its
//! destination by slot, and a slot is `memory[frame_base + slot]`.
//!
//! # There is no Rust recursion here
//!
//! A Cove call pushes a frame onto [`Machine::frames`] and continues the same
//! loop. Nothing about a call grows the native stack, so how deep a Cove
//! program may recurse is decided by [`STACK_WORDS`] alone rather than by how
//! large a Rust stack frame the dispatch loop happens to compile to — which
//! is a number that changes when an unrelated instruction is added.
//!
//! # There is no `Value` here
//!
//! Ordinary Cove-to-Cove execution moves words and heap objects. The public
//! `Value` is built at the boundary — a Host call, an entry's answer, a trace
//! capture — and nowhere else. There is no operand `Vec<Value>`, no argument
//! buffer, no spill area and no fallback path, which is what ADR 0034 asks
//! for and what the predecessor could not say.

use std::sync::{Arc, Mutex};
use std::thread::{Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use cove_diag::Span;
use cove_ir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Convert, FunctionId, HostOpId, Inst, LayoutId, Len,
    Num, Program, Repr, Shape, Slot, StrId,
};

use crate::budget::{Cancellation, Meter, Stopped};
use crate::error::RuntimeError;
use crate::host::{HostRegistry, Reentry, ResourceHandle};
use crate::interp::stopped_here;
use crate::runtime::{Runtime, ENTRY_TASK};
use crate::task;
use crate::trace::TraceEvent;
use crate::vm::builtins::operand::Operand;
use crate::vm::mem::{Collected, Memory, NoSegment, Overflow, Parked, Rooted, Roots};
use crate::vm::{boundary, builtins, cell};
// The one import of the public `Value` outside `boundary`, and the one thing
// ADR 0034 allows it for: a host call's arguments and its answer exist as
// `Value`s for the length of the call and nowhere else. Nothing here stores
// one, and the three places it is named are all in transit — the vector
// handed to the boundary, the callee a way back is offered, and a callback's
// arguments and answer, which are the same boundary crossed the other way
// round and are converted by the same file.
use crate::value::Value;

/// How many instructions run between two budget checks.
///
/// A budget check reads an atomic and a clock, and doing that per instruction
/// would cost more than most instructions do. Doing it per *call* would let a
/// tight arithmetic loop run unbounded. A fixed stride is the arrangement that
/// bounds both: the run notices a cancellation within a known number of
/// instructions, whatever it is doing.
///
/// It is public because it is the arithmetic of a contract rather than a
/// tuning knob.
/// [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)
/// states every bound a stop mode promises in terms of it, and
/// `crates/cove-runtime/tests/responsiveness.rs` measures each of them, so
/// moving this number moves a stated maximum and costs both.
pub const SAFEPOINT_STRIDE: u64 = 1024;

/// One live call.
///
/// The top of [`Machine::frames`] is the frame currently executing, not the
/// caller of it. That costs a write of `pc` before anything that can collect
/// or fail, and it buys a collector and an error reporter that need no
/// special case for "and also the one in the local variables".
struct Frame {
    function: FunctionId,
    /// The linear address of slot 0.
    base: u64,
    /// Where this frame resumes: the instruction after the call it is
    /// suspended at, or the one about to run.
    pc: u32,
    /// The slot of the *caller's* frame this call's answer is written to.
    dst: Slot,
}

/// What one task is holding, read where the collector asks rather than
/// gathered in advance.
///
/// A safepoint runs every [`SAFEPOINT_STRIDE`] instructions and a collection
/// is rare, so the walk has to cost nothing when the answer is that nothing
/// is pending — and [`Memory::poll`] answers that with one relaxed load,
/// *before* it asks for roots. Gathering into a `Vec` first would have paid
/// for a pass over every reference slot of every live frame on the common
/// path in order to save nothing on the rare one.
///
/// It borrows the machine immutably and reads its own memory, which is why
/// every reader of it takes `&self`: a collection and a park are both things
/// a task asks for about itself, and neither changes a frame.
struct Live<'m, 'a>(&'m Machine<'a>);

impl Roots for Live<'_, '_> {
    fn each_root(&self, f: &mut dyn FnMut(u64)) {
        let machine = self.0;
        let program = machine.program;
        for frame in &machine.frames {
            let function = program.function(frame.function);
            for slot in function.refs.iter() {
                let word = machine.mem.slot(frame.base, slot);
                if word != 0 {
                    f(word);
                }
            }
        }
        for &addr in &machine.interned {
            if addr != 0 {
                f(addr);
            }
        }
        for &addr in &machine.temps {
            if addr != 0 {
                f(addr);
            }
        }
        // A cell this task is inside. It is already named by a `Repr::Ref`
        // slot of the frame the lock region belongs to — the lowering holds
        // the receiver for the whole region — so this adds nothing to what is
        // reachable. What it adds is that the claim does not have to be made:
        // [`Machine::give_cells_back`] *writes* the lock word of every one of
        // these, on a path taken after the loop has stopped, and a word
        // written into a run the sweep reclaimed would be a word of whatever
        // is allocated there next.
        for &addr in &machine.held {
            f(addr);
        }
        // The scheduler table is a *root provider*, which is the whole of
        // what keeps it from being the second value store ADR 0034 forbids:
        // it holds the address of the object a task's answer goes into, and
        // the object and its words are in the run's one heap like anything
        // else. Nothing that wanted to dodge a heap representation could be
        // put here, because an address is all there is room for.
        for child in &machine.children {
            if child.answer != 0 {
                f(child.answer);
            }
            if child.closure != 0 {
                f(child.closure);
            }
        }
    }
}

/// What a task thread hands back: nothing, or why it stopped.
///
/// Nothing, because the value is not carried out — the parent allocated the
/// object it goes into before the thread existed, and the child wrote its
/// words there. Handing the words back through the join would have left them
/// in a Rust `Vec`, which nothing the collector walks names, for as long as
/// the join took.
type Outcome = Result<(), RuntimeError>;

/// What a spawned task has done so far.
///
/// [`crate::task::TaskState`] is the oracle's, and the four are the same
/// four: a task ends by finishing, by failing, by being cancelled, or by
/// breaking an invariant in its own thread — and the last is reported as the
/// third or the second, exactly as [`crate::task::Task::join`] reports it.
enum ChildState {
    /// The body is running on its own thread, which has not been joined.
    Running,
    /// The body produced a value, and it is in the answer object.
    Settled,
    /// The body raised. Awaiting again raises the same error.
    Failed(RuntimeError),
    /// The task's own flag was raised and it stopped at a safepoint rather
    /// than finishing. Awaiting a cancelled task is an error.
    Cancelled,
}

/// One task this machine spawned, and everything about it that is not the
/// thread.
///
/// The thread is not here, and cannot be: a scoped join handle borrows the
/// scope it was started in, and that is a lifetime one turn of the dispatch
/// loop owns rather than a field the machine can hold. So the handles live
/// beside this list, at the same indices, for the length of one
/// [`Machine::drive`].
struct Child {
    /// Trace identity, unique across the run.
    id: u64,
    /// Where in its scope's spawn order it is, counting from one.
    position: usize,
    /// The name of the scope that owns it.
    ///
    /// Both are here for one reason: a diagnostic says *task 2 of scope
    /// `requests`*, and [`crate::task::describe`] is where both backends read
    /// that sentence from.
    scope: Arc<str>,
    /// The flag the body reads at its own safepoints.
    cancellation: Cancellation,
    /// The closure environment the body runs, until it has been joined.
    ///
    /// Held as a root, and it has to be. The lowering ends the closure
    /// temporary's live range at the `spawn` — correctly: the value is a
    /// temporary and the instruction consumed it — so the moment the
    /// `Inst::Clear` after the `Inst::Spawn` runs, nothing in the parent's
    /// frame names the environment. The child has not necessarily read it
    /// yet: ADR 0008's amendment says a `spawn` orders nothing, so whether
    /// the thread has run an instruction by then is the operating system's
    /// answer, and one allocation in the parent could otherwise free the
    /// object the child is about to enter.
    ///
    /// The oracle has no such window because the closure crosses as a
    /// `Transfer` — a copy the receiving thread owns outright. Here what
    /// crosses is an address into a heap both tasks share, so the *table*
    /// holds it, and this is the second thing that makes the scheduler table
    /// a root provider.
    ///
    /// It is dropped at the join rather than at the child's first
    /// instruction, because that is the earliest moment the parent can know
    /// the child is done with it — and it retains nothing the child was not
    /// already retaining, since the captures were copied into the child's
    /// own frame.
    closure: u64,
    /// The object this task's answer is written into.
    ///
    /// Allocated by **this** task, before the thread existed, and a root of
    /// this task from that moment. A child that allocated its own would have
    /// left it named by nothing the collector walks between its own last
    /// safepoint and the parent's next one.
    answer: u64,
    /// What the words inside that object are.
    layout: LayoutId,
    state: ChildState,
}

impl Child {
    /// How a diagnostic names this task.
    fn describe(&self) -> String {
        task::describe(self.position, &self.scope)
    }
}

/// One task scope this machine has entered.
///
/// The scope owns every task spawned into it, which is what lets leaving it
/// wait for or cancel them. It is never removed: a `Repr::Scope` word is an
/// index, and an index that could be reused would name two scopes over one
/// run.
struct ScopeEntry {
    name: Arc<str>,
    /// Indices into [`Machine::children`], in spawn order.
    tasks: Vec<usize>,
    /// Set once the scope has been left. A handle that outlived its scope can
    /// no longer spawn into it.
    closed: bool,
}

/// One task's execution over one linear memory.
pub(crate) struct Machine<'a> {
    program: &'a Program,
    /// What every thread of this run shares: the trace, and the counter task
    /// ids are drawn from.
    ///
    /// `None` is a machine with no run around it — what this module's own
    /// tests drive — and it costs exactly two things, both of which are
    /// reporting rather than execution: nothing is traced, and task ids are
    /// drawn from a counter of this machine's own. A program still spawns,
    /// awaits and cancels.
    runtime: Option<&'a Runtime>,
    /// The boundary a [`Inst::CallHost`] calls through, if this run has one.
    ///
    /// `None` is a machine with no host behind it — what a test that runs
    /// arithmetic drives, and the same state [`crate::host::NoReentry`]
    /// exists for on the other side of the boundary. A program that reaches
    /// a host call from one is told what is missing rather than being given a
    /// registry that answers nothing.
    hosts: Option<&'a HostRegistry>,
    mem: Memory,
    frames: Vec<Frame>,
    /// The string object for each [`StrId`], allocated on first use.
    ///
    /// A literal in a loop allocates once for the run rather than once per
    /// turn. The table is a root for as long as the machine lives, which is
    /// the price: a string mentioned once and never reached again is retained.
    /// That is the right trade for a *literal*, which the program named
    /// statically and can name again.
    interned: Vec<u64>,
    /// The host resources this run has been handed, in the table a
    /// [`Repr::Host`] word indexes.
    ///
    /// [`Repr::Host`]'s own documentation is what fixes the shape — *an index
    /// into the run's host resource table* — and this is that table. It is
    /// here and not in the heap because
    /// [ADR 0031](../../../../docs/adr/0031-a-host-handle-is-not-a-vm-handle.md)
    /// draws exactly this line: a host resource handle is a name the *host*
    /// minted for something the host owns, and a heap object is a reference
    /// into storage this run allocated. Only the second is the VM's. Making a
    /// resource an object in the traced heap would put a collection in charge
    /// of a lifetime [ADR 0013](../../../../docs/adr/0013-host-resource-handles.md)
    /// gives to the host, and would mean sweeping something whose `close` the
    /// program had not written.
    ///
    /// It is not a second value store either, which is the other thing
    /// ADR 0034 and ADR 0031 forbid. Nothing a Cove program can write down
    /// may be put in it: an entry is an [`Arc`] of a [`ResourceHandle`] — a
    /// module, a type name, a number and a flag — and the only two operations
    /// over it are [`Machine::resource`] and [`Machine::resource_word`],
    /// neither of which can be handed a Cove value. A value that wanted to
    /// avoid having a heap representation could not hide here.
    ///
    /// **The word is one past the index, so zero is no resource.** Frames are
    /// zeroed on entry, so a `Host` slot that has not been written yet reads
    /// zero exactly as a `Ref` slot reads null; a table indexed straight by
    /// the word would answer an unwritten slot with whichever resource
    /// happened to be first.
    ///
    /// Nothing is ever removed. ADR 0013 says a closed resource's handle
    /// survives as a name for something that is gone, and that a host never
    /// reuses an identity — so an entry that outlived its resource is still
    /// the right answer to give, because the refusal a later call earns is
    /// the *host's* and can only be reached by handing the host the name.
    /// What that costs is one name per distinct resource this run was handed,
    /// which is the size of the table the host is keeping anyway.
    ///
    /// It is a field of the machine, which today is the run: a run has one
    /// machine as it has one [`Memory`]. When a run has task threads this
    /// moves where the object heap moves, and for the reason ADR 0013 gives
    /// rather than by analogy — a resource is owned by the *run*, not by the
    /// task or the scope that opened it, so a handle one task was given is a
    /// name every task of the run may hold.
    ///
    /// Shared by every task of the run, behind a lock, and that is not an
    /// economy: a task-safe resource **crosses** a task boundary, and what
    /// crosses is the word. A table of this task's own would make that word
    /// an index into a list the receiving task does not have — so a handle a
    /// parent opened would name whatever the child happened to open first,
    /// or nothing. ADR 0013 says a resource is the *run's*, and this is what
    /// that sentence costs.
    resources: Arc<Mutex<Vec<Arc<ResourceHandle>>>>,
    /// Objects a boundary conversion is holding and no frame names yet.
    ///
    /// A frame is a root because a static map says which of its slots are
    /// references. A half-built object is not: it is reachable only from a
    /// Rust local, which nothing walks, and the next allocation the
    /// conversion makes could collect it out from under itself. So the
    /// conversion says so, explicitly, for exactly as long as that is true —
    /// [`Machine::push_temp`] to take a root, [`Machine::release_temps`] to
    /// give every root back that was taken since a mark.
    ///
    /// It is a stack rather than a set because the discipline is lexical: a
    /// conversion that recurses takes a mark on the way in and releases to it
    /// on the way out, so nothing has to remember which root was whose.
    temps: Vec<u64>,
    /// The task scopes this machine has entered, in the order it entered
    /// them. A `Repr::Scope` word is one past an index into this.
    ///
    /// This machine's, not the run's, and that is what the task-safety rule
    /// buys: neither a `TaskScope` nor a `Task` may cross a task boundary, so
    /// a word formed here is only ever read here. Two tasks cannot form one
    /// another's handles, which is the same disjointness by construction that
    /// keeps two stack segments apart.
    scopes: Vec<ScopeEntry>,
    /// The tasks this machine spawned, in spawn order across every scope. A
    /// `Repr::Task` word is one past an index into this.
    children: Vec<Child>,
    /// This task's own cancellation flag, or `None` for the entry, which has
    /// none.
    ///
    /// Separate from the run's, which lives in the [`Meter`] every task
    /// shares: cancelling one task stops that task, and cancelling the run
    /// stops everything. [`crate::interp::stopped_here`] is where the two a
    /// *thread* owns are read, and it is the oracle's own function so that
    /// neither backend can drift from the other's answer.
    cancellation: Option<Cancellation>,
    /// The flags of the bounded host calls this thread is inside, innermost
    /// last.
    ///
    /// [`crate::host::Reentry::call_until`] pushes one: `clock.timeout` bounds
    /// its body, and *"`stop` bounds this call and everything inside it,
    /// including a further host call the body makes and any callback that host
    /// runs in turn"*. So a safepoint reads these beside this task's own flag,
    /// which is what makes a timeout a timeout rather than a measurement taken
    /// afterwards. [`crate::interp::stopped_here`] is the oracle's own
    /// function and asks both, so neither backend can drift from the other's
    /// answer about which of the two stopped the work.
    stops: Vec<Cancellation>,
    /// How many host calls running a Cove callback are stacked on this
    /// thread.
    ///
    /// A Cove call adds no native frame here — that is what the dispatch loop
    /// is for — but a *reentry* does: the host is a Rust frame, and running
    /// its callback puts another turn of [`Machine::dispatch`] below it. So
    /// this is bounded exactly as the oracle bounds it, by
    /// [`crate::interp::MAX_REENTRY_DEPTH`], and for the same reason.
    reentry_depth: usize,
    /// Which task this machine is running, for a trace and for the way back
    /// a host is offered.
    task: u64,
    /// Where the next task id comes from when there is no [`Runtime`] to ask.
    ///
    /// Only a test reaches it. A run draws ids from one counter for the whole
    /// run, because a task id is a trace identity and two tasks spawned at
    /// the same time on two threads must not share one.
    next_task: u64,
    instructions: u64,
    /// How many of [`Machine::instructions`] have been handed to the run's
    /// [`Meter`].
    ///
    /// The two counters differ by the work this machine has done and not yet
    /// paid for, and every place that pays hands over exactly that difference
    /// and sets this to the count it paid up to. There is no second
    /// accumulator to keep in step with the instruction count, which is what
    /// makes "the run is charged for every instruction it dispatched" a
    /// subtraction rather than a claim about the paths somebody remembered.
    ///
    /// Three places move it, and between them they cover every way work can
    /// be done. The periodic safepoint in [`Machine::dispatch`] is the
    /// ordinary one. [`Machine::charge_at_host_boundary`] is
    /// [ADR 0030](../../../../docs/adr/0030-a-host-call-asks-the-fuel-limit.md)'s:
    /// the fuel a run has been charged has to be current before a Host call
    /// asks whether it may begin. [`Machine::spend_pending_fuel`] is the last
    /// one, at the end of a run or of a spawned task's thread, because a run
    /// that raised, ran out of budget, was cancelled or was abandoned by the
    /// host that bounded it leaves through Rust's `?` rather than through an
    /// instruction and reaches no further safepoint —
    /// [ADR 0024](../../../../docs/adr/0024-a-stop-is-a-bound-not-a-point.md)
    /// says pending fuel is never lost, and this is the counter that makes
    /// that checkable.
    charged: u64,
    /// How long this machine has spent inside host calls.
    ///
    /// The oracle charges the same measurement against every open timing
    /// context so that a run can separate its own work from what it spent
    /// waiting; this machine has one context, which is the run.
    host_wait: Duration,
    collected: Collected,
    /// The `Shared` cells this task is inside, innermost last.
    ///
    /// `lock` is two instructions with a call between them, and the release is
    /// an obligation on every exit path — which the lowering discharges on the
    /// one it can write. This is the other one: a runtime error is not a jump
    /// the lowering emits, so a cell a failing task never gave back would be a
    /// cell no task could ever take again. It is the same division
    /// [`Machine::stop_all`] is under for a task scope, and it costs a push
    /// and a pop per `lock`.
    ///
    /// A `Vec` rather than a set because the discipline is lexical: two cells
    /// nest, the refusal is per cell rather than per task, and the lowering
    /// leaves the regions in the order it entered them.
    held: Vec<u64>,
    /// The layout of the value the host call now running was handed back by a
    /// callback, if it ran one.
    ///
    /// This exists for one question the family search in
    /// [`crate::vm::boundary`] cannot answer. A host answer that crosses in
    /// at a `Shape::Boxed` position has to be tagged with the family it
    /// holds, and the tag is what [`Inst::Unbox`] compares against the layout
    /// the checker settled at the use. The search reads the value's own
    /// description, and a description does not always name one family:
    /// `Err(Error("no"))` fits `Result<Int, Error>` and
    /// `Result<http.Response, Error>` equally well, and the two are different
    /// runs of words. Which one the search returned was then decided by which
    /// the lowering happened to intern first.
    ///
    /// A callback's answer needs no search, because it is a value that just
    /// left this machine. `clock.timeout` declares `Result<Any, Error>` and
    /// wraps whatever its body answered, so what goes in the box is the
    /// callback's return value and the family it belongs to is the callback's
    /// declared return layout — a static fact, recorded here on the way out
    /// so that the way back in does not have to guess at it.
    ///
    /// Cleared when a host call begins ([`Back::parked`]) and written when a
    /// callback returns ([`Machine::call_from_host`]), so it names the
    /// innermost call in progress and never an older one: a host call the
    /// callback itself makes clears it on the way in and the callback's own
    /// return writes it afterwards.
    callback_answer: Option<LayoutId>,
    /// Where the most recent assertion failed, and the message it produced.
    ///
    /// Written only by [`Inst::AssertFailed`], which the failing arm of a
    /// lowered assertion carries. A failed assertion is an ordinary `Err`
    /// from here on — the machine does not stop, and a program that handles
    /// it goes on running — so this is a record of what was seen and not a
    /// state the run is in. A test runner reads it to point at the assertion
    /// rather than at the test, and keeps the message so that it can tell
    /// the `Err` it is holding from a later, unrelated one.
    assertion_failure: Option<(Span, String)>,
}

impl<'a> Machine<'a> {
    /// A machine with no host boundary, for a program that calls none.
    pub(crate) fn new(program: &'a Program, heap_words: usize) -> Machine<'a> {
        Machine::with_hosts(program, heap_words, None)
    }

    /// A machine that calls hosts through `hosts`, with nothing above it.
    pub(crate) fn with_hosts(
        program: &'a Program,
        heap_words: usize,
        hosts: Option<&'a HostRegistry>,
    ) -> Machine<'a> {
        Machine::for_run(program, heap_words, hosts, None)
    }

    /// The entry task of one run.
    pub(crate) fn for_run(
        program: &'a Program,
        heap_words: usize,
        hosts: Option<&'a HostRegistry>,
        runtime: Option<&'a Runtime>,
    ) -> Machine<'a> {
        Machine {
            program,
            runtime,
            hosts,
            mem: Memory::new(heap_words),
            frames: Vec::new(),
            interned: vec![0; program.strings.len()],
            resources: Arc::new(Mutex::new(Vec::new())),
            temps: Vec::new(),
            scopes: Vec::new(),
            children: Vec::new(),
            cancellation: None,
            stops: Vec::new(),
            reentry_depth: 0,
            task: ENTRY_TASK,
            next_task: 1,
            instructions: 0,
            charged: 0,
            host_wait: Duration::ZERO,
            collected: Collected::default(),
            held: Vec::new(),
            callback_answer: None,
            assertion_failure: None,
        }
    }

    /// A machine for a spawned task, over a stack segment of its own and the
    /// run's one heap.
    ///
    /// Everything a run owns is shared and everything a task owns is fresh,
    /// and the split is the whole of ADR 0008 here. Shared: the program, the
    /// hosts, the trace, the heap, the run's budget, the resource table.
    /// Fresh: the stack segment, the frames, the string objects this task
    /// allocates for its own literals, and the scheduler table it spawns its
    /// own children into.
    ///
    /// The interned strings are the one that looks like an economy and is
    /// not. A literal is an *object*, and an object belongs to the heap both
    /// tasks address; interning it twice costs one object per literal per
    /// task and buys a table with no lock on the path a literal in a loop
    /// takes. Sharing it would put a lock between every `Inst::Str` and its
    /// answer.
    #[allow(clippy::too_many_arguments)]
    fn for_task(
        program: &'a Program,
        hosts: Option<&'a HostRegistry>,
        runtime: Option<&'a Runtime>,
        resources: Arc<Mutex<Vec<Arc<ResourceHandle>>>>,
        mem: Memory,
        cancellation: Cancellation,
        task: u64,
    ) -> Machine<'a> {
        Machine {
            program,
            runtime,
            hosts,
            mem,
            frames: Vec::new(),
            interned: vec![0; program.strings.len()],
            resources,
            temps: Vec::new(),
            scopes: Vec::new(),
            children: Vec::new(),
            cancellation: Some(cancellation),
            stops: Vec::new(),
            reentry_depth: 0,
            task,
            next_task: 1,
            instructions: 0,
            charged: 0,
            host_wait: Duration::ZERO,
            collected: Collected::default(),
            held: Vec::new(),
            callback_answer: None,
            assertion_failure: None,
        }
    }

    /// The family of the value a callback answered during the host call in
    /// progress, if one ran. See [`Machine::callback_answer`].
    pub(crate) fn callback_answer(&self) -> Option<LayoutId> {
        self.callback_answer
    }

    /// How many instructions this machine has run.
    pub(crate) fn instructions(&self) -> u64 {
        self.instructions
    }

    /// What every collection so far has done.
    pub(crate) fn collected(&self) -> Collected {
        self.collected
    }

    /// Words the heap region occupies, free blocks included.
    pub(crate) fn heap_words(&self) -> u64 {
        self.mem.heap_words()
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub(crate) fn allocated_words(&self) -> u64 {
        self.mem.allocated_words()
    }

    /// How long this machine has waited on hosts.
    pub(crate) fn host_wait(&self) -> Duration {
        self.host_wait
    }

    /// Where the most recent failed assertion was written, and the message
    /// it produced, or `None` when none has failed.
    pub(crate) fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// Runs `entry` with `args` already in word form, answering the words of
    /// its result.
    ///
    /// The caller converts: this is below the boundary, and nothing here
    /// knows what a public `Value` is. `args` is the parameters' words
    /// flattened in declaration order — a `(Int, Point, Int)` list is four
    /// words — because that is what the frame they are written into is.
    pub(crate) fn run(
        &mut self,
        entry: FunctionId,
        args: &[u64],
        budget: &Meter,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let function = program.function(entry);
        debug_assert_eq!(
            args.len(),
            function.param_words(&program.layouts) as usize,
            "an entry is called with its parameters' words"
        );

        // On an empty stack, and that has to be said rather than assumed. A
        // machine is built once and called many times — `Vm::invoke_within`
        // exists so that one bounded invocation can be stopped without
        // damaging the session that made it — and a call that was stopped
        // where it stood left its frames standing, because a runtime error is
        // not a jump the lowering emits and nothing unwound them. Building
        // this call on top of those would give `Inst::Return` a caller to
        // resume that belongs to a call that is over: the answer would be
        // written into an abandoned frame and the run would continue in it.
        //
        // The cells go back for the same reason and in the same breath. A
        // `lock` region the stopped call was inside was left by no
        // `SharedUnlock`, and a cell nobody gives back is a cell no task can
        // ever take again.
        self.give_cells_back(0);
        self.frames.clear();
        self.mem.reset_stack();
        self.temps.clear();

        let base = self
            .mem
            .push_frame(function.frame_size())
            .map_err(|Overflow| self.too_deep(function.span))?;
        for (slot, word) in args.iter().enumerate() {
            self.mem.set_slot(base, slot as u32, *word);
        }
        self.frames.push(Frame {
            function: entry,
            base,
            pc: 0,
            dst: 0,
        });
        self.drive(budget)
    }

    /// Runs the frame already on the stack, inside the thread scope its
    /// `spawn`s start their children in.
    ///
    /// The scope is here rather than around the whole run because it is what
    /// bounds a task's threads to a task: nothing this body starts can outlive
    /// this call, so the borrow a child holds of the program, the hosts and
    /// the run is a borrow the compiler can check rather than one this module
    /// has to promise. A child that spawns children of its own opens a scope
    /// of its own, nested, and leaves it before it answers.
    ///
    /// Whatever the body did, no thread leaves here with work to do. A
    /// runtime error is not a jump the lowering emits, so there is no
    /// `ScopeCancel` on that path and nothing would otherwise cancel the
    /// children of a scope the error left — and `std::thread::scope` would
    /// then wait for a task that is waiting for nobody. [`Machine::stop_all`]
    /// is that path, and on the ordinary one it has nothing to do because
    /// every scope was already left where it was written.
    fn drive(&mut self, budget: &Meter) -> Result<Vec<u64>, RuntimeError> {
        std::thread::scope(|threads| {
            let mut running: Vec<Option<ScopedJoinHandle<'_, Outcome>>> = Vec::new();
            let answer = self.dispatch(budget, threads, &mut running, 0);
            debug_assert!(
                answer.is_err() || !self.anything_running(),
                "a body that answered left every scope it opened, so nothing is still running"
            );
            debug_assert!(
                answer.is_err() || self.held.is_empty(),
                "a body that answered left every cell it took"
            );
            self.give_cells_back(0);
            self.stop_all(&mut running);
            // Last, after the answer is settled and after the children are
            // joined, so that what is put back is everything this thread
            // dispatched and nothing is added to the run's total once the
            // total has been read.
            self.spend_pending_fuel(budget);
            answer
        })
    }

    /// Hands the run's [`Meter`] whatever this thread has dispatched and not
    /// yet paid for, at the end of a run or of a spawned task's thread.
    ///
    /// The ordinary way out of a body pays on its way: a run long enough to
    /// reach a periodic safepoint has handed over every whole stride of it,
    /// and a Host call has handed over the part of the stride that preceded
    /// it. What pays for nothing is the remainder — the instructions after
    /// the last hand-over — and every way a run can end without dispatching
    /// another instruction is a way that remainder would be dropped with the
    /// stacks: a raised error, an exhausted budget, a cancelled task, a
    /// bounded call the host abandoned. Each of those leaves through Rust's
    /// `?` rather than through an instruction.
    ///
    /// The work was really done, so the run is charged for it.
    /// [ADR 0024](../../../../docs/adr/0024-a-stop-is-a-bound-not-a-point.md)
    /// decides that pending fuel is never lost, and a `fuel_spent` below the
    /// instructions the run dispatched is the observable form of losing it.
    ///
    /// [`Meter::spend`] rather than [`Meter::safepoint`], and the difference
    /// is the whole reason this is its own function: this runs after the
    /// answer is settled, and a stop raised here would replace the reason the
    /// run actually ended. A run that raised would report that it was out of
    /// fuel.
    ///
    /// A spawned task's thread reaches this through its own [`Machine::drive`]
    /// and pays into the same accounting, because ADR 0008 draws a task's
    /// fuel from the run's budget rather than giving each task one of its
    /// own.
    fn spend_pending_fuel(&mut self, budget: &Meter) {
        let pending = self.instructions - self.charged;
        if pending != 0 {
            self.charged = self.instructions;
            budget.spend(pending);
        }
    }

    /// Hands over what this thread has dispatched and not yet paid for, and
    /// asks the run's accounting whether it may continue — at a Host call,
    /// before the call is dispatched.
    ///
    /// # The contract
    ///
    /// **No Host call begins once the fuel a run has been charged has reached
    /// its limit.**
    /// [ADR 0030](../../../../docs/adr/0030-a-host-call-asks-the-fuel-limit.md)
    /// decides that, and it is a statement about the bound rather than about
    /// the count: what the two backends share is the property, not the number
    /// that satisfies it. The oracle satisfies it by holding no pending fuel
    /// at all — `Interpreter::charge_safepoint` hands `SAFEPOINT_FUEL` over in
    /// the same call that charges it, so its charged total cannot move while
    /// a straight line runs. This machine holds pending fuel by construction,
    /// because it charges on a fixed instruction stride, so it satisfies it
    /// the other way ADR 0030 allows: by flushing here.
    ///
    /// Without this, a Host call is just another instruction the stride
    /// counts, and a straight line of them shorter than one
    /// [`SAFEPOINT_STRIDE`] is not stopped at any fuel limit whatever —
    /// forty effects under a limit of one, which is the shape ADR 0030 was
    /// written to refuse.
    ///
    /// # Why this is not a safepoint
    ///
    /// It asks the budget and nothing else. The two flags a thread owns are
    /// read by the caller one line above, so repeating them here would be
    /// asking a question that has just been answered.
    ///
    /// The collector is the interesting half. The predecessor's argument for
    /// leaving it out was that its arguments had already been drained into a
    /// `Vec<Value>` and were therefore rooted by their own references rather
    /// than by the walk. **That argument does not hold here, and the
    /// conclusion still does.** The arguments this boundary converts are
    /// [`Value`]s built by [`crate::vm::boundary::to_value`], which copies
    /// out of the heap rather than naming it, and the words they were read
    /// from are still in the slots of a frame this machine has not left — so
    /// a collection here would be as sound as one anywhere else, and
    /// [`Machine::park`] is about to publish exactly those roots for the
    /// length of the call. The reason not to collect is therefore the second
    /// half of the predecessor's: this machine's collection point is a
    /// rendezvous poll, and putting one in front of every Host call would
    /// make an unpredictable sweep part of the cost of reaching the outside
    /// world, for a reason the budget never asked for.
    fn charge_at_host_boundary(&mut self, budget: &Meter, span: Span) -> Result<(), RuntimeError> {
        let pending = self.instructions - self.charged;
        self.charged = self.instructions;
        if let Err(stopped) = budget.safepoint(pending) {
            return Err(budget.to_runtime_error(stopped).at(span));
        }
        Ok(())
    }

    /// Runs the body of a spawned closure, which is this machine's whole
    /// task.
    ///
    /// The closure takes no parameters — `scope.spawn { ... }` is written
    /// with none and an `async fn`'s handle carries none — so its frame is
    /// its captures and its locals. The captures are copied out of the
    /// environment exactly as [`Inst::CallClosure`] copies them, because it
    /// is the same object read the same way; what differs is only that the
    /// caller is a thread rather than an instruction.
    fn enter_closure(
        &mut self,
        object: u64,
        budget: &Meter,
        span: Span,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let callee = self.callee_of(object)?;
        let target = program.function(callee);
        if !target.params.is_empty() {
            return Err(wrong_arity(target.qualified(), target.params.len(), 0).at(span));
        }
        let base = self
            .mem
            .push_frame(target.frame_size())
            .map_err(|Overflow| self.too_deep(span))?;
        let mut held = 1;
        for capture in &target.captures {
            let width = program.layout(capture.layout).width();
            self.mem.copy_words(
                base + capture.slot as u64,
                self.mem.payload_addr(object, held),
                width,
            );
            held += width;
        }
        self.frames.push(Frame {
            function: callee,
            base,
            pc: 0,
            dst: 0,
        });
        self.drive(budget)
    }

    /// The loop.
    ///
    /// `function`, `base` and `pc` are kept in locals rather than read out of
    /// the top frame on every instruction, and written back at the two points
    /// where something else looks: a collection, and a failure.
    fn dispatch<'s>(
        &mut self,
        budget: &Meter,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
        floor: usize,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let top = self.frames.last().expect("run pushed a frame");
        let mut id = top.function;
        let mut base = top.base;
        let mut pc = top.pc as usize;
        let mut code = &program.function(id).code[..];

        loop {
            self.instructions += 1;
            if self.instructions.is_multiple_of(SAFEPOINT_STRIDE) {
                self.sync(pc);
                // This task's own flag and every bounded call this thread is
                // inside first, then the run's accounting, in the order
                // `Interpreter::charge_safepoint` asks them: a cancelled task
                // is cancelled and not out of fuel, whichever of the two also
                // happens to be true. `stopped_here` is the oracle's own
                // function, so the two backends cannot disagree about which
                // of the two stopped the work.
                stopped_here(self.cancellation.as_ref(), &self.stops, self.span(id, pc))?;
                // What is handed over is the instructions run since the last
                // hand-over, not the stride: a Host call between two
                // safepoints may already have paid for part of this window,
                // and charging the stride flat would charge that part twice.
                // It is a whole stride whenever nothing intervened, which is
                // the case a bound is stated against.
                let gathered = self.instructions - self.charged;
                self.charged = self.instructions;
                if let Err(stopped) = budget.safepoint(gathered) {
                    return Err(budget.to_runtime_error(stopped).at(self.span(id, pc)));
                }
                // And the run's: a collection is stop-the-world, so a task
                // that never published its roots would be a task the
                // collector waits for while it waits for the collector. The
                // question costs one relaxed load when nothing is pending,
                // which is every time but the rare one.
                self.mem.poll(&Live(self));
            }

            let inst = &code[pc];
            pc += 1;

            macro_rules! fail {
                ($error:expr) => {{
                    self.sync(pc - 1);
                    return Err($error.at(self.span(id, pc - 1)));
                }};
            }

            match *inst {
                // ---- constants and moves -------------------------------
                Inst::Unit { dst } => self.mem.set_slot(base, dst, 0),
                Inst::Bool { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Int { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Float { dst, bits } => self.mem.set_slot(base, dst, bits),
                Inst::Str { dst, text } => {
                    self.sync(pc - 1);
                    match self.intern(text) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                // ADR 0001's field-wise shallow copy, and the whole of it.
                // A value's words are where the value is, so copying one is
                // copying its run of words: a `Wrapper { p: Point, v: Vector }`
                // copies three, the `Point` becomes independent and the
                // `Vector` stays shared, and neither answer needed a policy.
                Inst::Copy { dst, src, layout } => {
                    let width = self.width(layout);
                    self.mem
                        .copy_words(base + dst as u64, base + src as u64, width);
                }
                // The one instruction whose whole purpose is what it stops
                // happening: a reference the frame no longer needs is not a
                // root, so the object it named is unreachable now rather
                // than when this frame returns.
                Inst::Clear { slot, layout } => {
                    let width = self.width(layout);
                    self.mem.clear_words(base + slot as u64, width);
                }

                // ---- scalar operations ----------------------------------
                Inst::Neg { num, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match num {
                        Num::Int => match (a as i64).checked_neg() {
                            Some(value) => value as u64,
                            None => fail!(overflowed("negation")),
                        },
                        Num::Float => (-f64::from_bits(a)).to_bits(),
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Arith { num, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let word = match num {
                        Num::Int => {
                            // Which of the two the operands are decides only
                            // what the message calls the operation. The
                            // arithmetic is the same, because a `Duration` is
                            // nanoseconds and nanoseconds add like integers.
                            let duration = self.repr(id, dst) == Some(Repr::Duration);
                            match int_arith(op, x as i64, y as i64, duration) {
                                Ok(value) => value as u64,
                                Err(error) => fail!(error),
                            }
                        }
                        Num::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            float_arith(op, x, y).to_bits()
                        }
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Cmp { on, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let answer = match on {
                        Compare::Int => compare(op, (x as i64).cmp(&(y as i64))),
                        Compare::Bool | Compare::Identity => match op {
                            CmpOp::Eq => x == y,
                            CmpOp::Ne => x != y,
                            // The verifier admits only `Eq` and `Ne` here;
                            // ordering a `Bool` or an identity is not a
                            // question the language asks.
                            _ => fail!(RuntimeError::new(
                                "this comparison is not defined for these operands"
                            )),
                        },
                        Compare::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            match op {
                                CmpOp::Eq => x == y,
                                CmpOp::Ne => x != y,
                                CmpOp::Lt => x < y,
                                CmpOp::Le => x <= y,
                                CmpOp::Gt => x > y,
                                CmpOp::Ge => x >= y,
                            }
                        }
                        Compare::Str => {
                            let ordering = self.compare_strings(x, y);
                            compare(op, ordering)
                        }
                    };
                    self.mem.set_slot(base, dst, answer as u64);
                }
                Inst::Not { dst, a } => {
                    let a = self.mem.slot(base, a);
                    self.mem.set_slot(base, dst, (a == 0) as u64);
                }
                Inst::Convert { to, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match to {
                        Convert::IntToFloat => (a as i64 as f64).to_bits(),
                        Convert::FloatToInt => f64::from_bits(a) as i64 as u64,
                    };
                    self.mem.set_slot(base, dst, word);
                }

                // ---- control flow ----------------------------------------
                Inst::Jump { to } => pc = to as usize,
                Inst::BranchFalse { cond, to } => {
                    if self.mem.slot(base, cond) == 0 {
                        pc = to as usize;
                    }
                }
                Inst::Switch { on, table } => {
                    let index = self.mem.slot(base, on) as usize;
                    let table = program.table(table);
                    pc = *table.targets.get(index).unwrap_or(&table.default) as usize;
                }
                // The words `Function::returns` describes, copied into the
                // caller's destination *location* — which is a base slot and
                // a width, like every other value location. The copy happens
                // before the frame is dropped, because the words are in it.
                Inst::Return { src } => {
                    let width = self.width(program.function(id).returns);
                    // The frame being left is what says where its answer
                    // goes. Keeping the destination with the callee rather
                    // than re-reading the caller's `Call` means a return
                    // touches one instruction, not two.
                    let done = self.frames.pop().expect("a frame is executing");
                    // The floor is where this turn of the loop was entered,
                    // and returning to it is what ends it. It is zero for a
                    // whole task's body and the caller's depth for a host
                    // callback, whose caller is a Rust frame rather than an
                    // instruction — so what is below the floor is left
                    // standing, and the answer's words are handed back in
                    // Rust.
                    match self.frames.last().filter(|_| self.frames.len() > floor) {
                        None => {
                            let answer = self.mem.read_words(base + src as u64, width);
                            self.mem.pop_frame(base);
                            return Ok(answer);
                        }
                        Some(caller) => {
                            id = caller.function;
                            let caller_base = caller.base;
                            pc = caller.pc as usize;
                            code = &program.function(id).code[..];
                            self.mem.copy_words(
                                caller_base + done.dst as u64,
                                base + src as u64,
                                width,
                            );
                            self.mem.pop_frame(base);
                            base = caller_base;
                        }
                    }
                }

                // ---- calls -------------------------------------------------
                Inst::Call { dst, callee, args } => {
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    if list.len() != target.params.len() {
                        fail!(wrong_arity(
                            target.qualified(),
                            target.params.len(),
                            list.len()
                        ));
                    }
                    if let Err(error) = self.admit_frame(budget, self.span(id, pc - 1)) {
                        self.sync(pc - 1);
                        return Err(error);
                    }
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    // Parameters occupy the callee's frame from slot 0 in
                    // declaration order, each taking the words its layout
                    // says. There is no argument buffer and no permutation
                    // into type groups: the callee's frame begins where this
                    // one ends, and the words are copied straight into it.
                    // The width is the *parameter's*, not the argument's,
                    // although an argument now carries a layout of its own
                    // and the verifier holds the two to be the same one. The
                    // frame being written is the callee's, and
                    // `Function::params` is the only fact about the callee
                    // here: a `CallClosure` does not know which function it
                    // is entering until it has read the object, so nothing
                    // static could be authoritative there, and one rule for
                    // both is worth more than the symmetry.
                    let mut at = 0;
                    for (arg, layout) in list.iter().zip(&target.params) {
                        let width = self.width(*layout);
                        self.mem
                            .copy_words(callee_base + at as u64, base + arg.slot as u64, width);
                        at += width;
                    }
                    self.sync(pc);
                    self.frames.push(Frame {
                        function: callee,
                        base: callee_base,
                        pc: 0,
                        dst,
                    });
                    id = callee;
                    base = callee_base;
                    pc = 0;
                    code = &program.function(id).code[..];
                }
                // A closure call is a frame like any other, and that is the
                // whole of it. The callee is not in the instruction — it is a
                // word of the object the slot names — and the captures follow
                // the arguments into the slots `Function::captures` names.
                // Nothing else differs from [`Inst::Call`]: no Rust frame is
                // added, so a `map` over a `map` over a `map` nests in the
                // reserved stack region and nowhere else, which is the
                // property `docs/LINEAR_VM.md` asks a closure-taking sequence
                // method to lower to a loop in order to keep.
                Inst::CallClosure { dst, closure, args } => {
                    let object = self.mem.slot(base, closure);
                    let callee = match self.callee_of(object) {
                        Ok(callee) => callee,
                        Err(error) => fail!(error),
                    };
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    if list.len() != target.params.len() {
                        fail!(wrong_arity(
                            target.qualified(),
                            target.params.len(),
                            list.len()
                        ));
                    }
                    if let Err(error) = self.admit_frame(budget, self.span(id, pc - 1)) {
                        self.sync(pc - 1);
                        return Err(error);
                    }
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    let mut at = 0;
                    for (arg, layout) in list.iter().zip(&target.params) {
                        let width = self.width(*layout);
                        self.mem
                            .copy_words(callee_base + at as u64, base + arg.slot as u64, width);
                        at += width;
                    }
                    // The object has to stay reachable across every one of
                    // these reads, and it does, for a reason rather than by
                    // luck: it is named by slot `closure` of a frame this has
                    // not left, the verifier holds that slot to `Repr::Ref`,
                    // and a `Repr::Ref` slot of a live frame is a root. So no
                    // temporary root is taken here — and nothing between the
                    // read and the last write allocates, so no collection can
                    // happen in the window at all.
                    //
                    // `Capture::slot` is read rather than re-derived from
                    // `arity + at`. The two agree, and the verifier is what
                    // says so: it refuses a capture naming a slot outside the
                    // frame or holding a different `Repr`.
                    //
                    // A capture is stored inline in the environment, each at
                    // its own width, so where one begins is the widths of the
                    // ones before it — the same arrangement the parameters
                    // are under, in a payload instead of a frame.
                    let mut held = 1;
                    for capture in &target.captures {
                        let width = self.width(capture.layout);
                        self.mem.copy_words(
                            callee_base + capture.slot as u64,
                            self.mem.payload_addr(object, held),
                            width,
                        );
                        held += width;
                    }
                    self.sync(pc);
                    self.frames.push(Frame {
                        function: callee,
                        base: callee_base,
                        pc: 0,
                        dst,
                    });
                    id = callee;
                    base = callee_base;
                    pc = 0;
                    code = &program.function(id).code[..];
                }
                // The one instruction that leaves the machine. Everything it
                // needs to read out of the frame is read before the call, so
                // that the frames are consistent for the length of it: a host
                // may collect through the boundary, and a boundary that had
                // been handed a stale program counter would walk this frame
                // to a slot the loop had already moved past.
                Inst::CallHost { dst, op, args } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    match self.call_host(base, op, args, budget, span, threads, running) {
                        Ok(words) => {
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // The same boundary, addressed to a handle rather than to a
                // module. ADR 0013 gives the host the only record of what is
                // open, so which resource answers is the word in `receiver`
                // and nothing static.
                Inst::CallResource {
                    dst,
                    receiver,
                    op,
                    args,
                } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    match self
                        .call_resource(base, receiver, op, args, budget, span, threads, running)
                    {
                        Ok(words) => {
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // Not a boundary. A builtin reads the words and the objects
                // the machine already holds, and answers one word; nothing
                // here is materialised into a `Value` on the way.
                Inst::CallBuiltin { dst, builtin, args } => {
                    self.sync(pc - 1);
                    match self.call_builtin(base, builtin, args) {
                        Ok(words) => {
                            // The answer is a value location like any other:
                            // `Builtin::result` names its layout, and an
                            // `Option<Int>` is two words rather than an
                            // address to two words somewhere else.
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }

                // ---- the heap ----------------------------------------------
                Inst::Alloc { dst, layout, len } => {
                    let len = match len {
                        Len::Fixed => 0,
                        Len::Count(n) => n,
                        Len::Slot(slot) => self.mem.slot(base, slot) as u32,
                    };
                    self.sync(pc - 1);
                    match self.allocate(layout, len) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                // A field of a *heap object* is a run of words at a static
                // offset, and its width is the layout the instruction names.
                // A field of an inline struct is not here at all: it is a
                // slot number the lowering computed, and reaching it costs
                // nothing.
                Inst::LoadField {
                    dst,
                    obj,
                    at,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let width = self.width(layout);
                    match self.checked(addr, at, width) {
                        Ok(()) => self.mem.copy_words(
                            base + dst as u64,
                            self.mem.payload_addr(addr, at),
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                Inst::StoreField {
                    obj,
                    at,
                    src,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let width = self.width(layout);
                    match self.checked(addr, at, width) {
                        Ok(()) => self.mem.copy_words(
                            self.mem.payload_addr(addr, at),
                            base + src as u64,
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                // The stride is the element layout's width, so an
                // `Array<Point>` is a run of two-word elements rather than a
                // run of addresses.
                Inst::LoadElem {
                    dst,
                    obj,
                    index,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => self.mem.copy_words(
                            base + dst as u64,
                            self.mem.payload_addr(addr, at),
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                Inst::StoreElem {
                    obj,
                    index,
                    src,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => self.mem.copy_words(
                            self.mem.payload_addr(addr, at),
                            base + src as u64,
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Len { dst, obj } => {
                    let addr = self.mem.slot(base, obj);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let len = self.mem.object_len(addr) as i64;
                    self.mem.set_slot(base, dst, len as u64);
                }
                // The other half of the header word `Len` reads. What an
                // object *is* is a question the object answers, and this is
                // that answer as an `Int`, so that a dispatch over it is an
                // ordinary `Switch` rather than an instruction of its own.
                Inst::LayoutOf { dst, obj } => {
                    let addr = self.mem.slot(base, obj);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let layout = self.mem.object_layout(addr).0 as i64;
                    self.mem.set_slot(base, dst, layout as u64);
                }

                // ---- places --------------------------------------------------
                Inst::AddrOfSlot { dst, slot } => self.mem.set_slot(base, dst, base + slot as u64),
                Inst::AddrOfField { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at, 1) {
                        Ok(()) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::AddrOfElem {
                    dst,
                    obj,
                    index,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                // A place is the address of the *first word* of a value
                // location, and its width is static — so a load and a store
                // through one move the words the layout says, and a nested
                // write through a `var` parameter updates the destination
                // words in place with nothing in between.
                // The one place instruction whose operand is a place. It
                // is arithmetic and nothing else: what an address names is a
                // value location, and a value location's parts are at static
                // offsets from its first word, so a field of a `var`
                // parameter is an addition rather than a load of the whole
                // value and a store of it back.
                Inst::AddrOfPart { dst, addr, at } => {
                    let word = self.mem.slot(base, addr);
                    self.mem.set_slot(base, dst, word + at as u64);
                }
                Inst::Load { dst, addr, layout } => {
                    let addr = self.mem.slot(base, addr);
                    let width = self.width(layout);
                    self.mem.copy_words(base + dst as u64, addr, width);
                }
                Inst::Store { addr, src, layout } => {
                    let addr = self.mem.slot(base, addr);
                    let width = self.width(layout);
                    self.mem.copy_words(addr, base + src as u64, width);
                }

                // ---- erasure ---------------------------------------------------
                // A box holds the layout of what it carries in payload word
                // 0 and that value's words after it, so a boxed `Point` is a
                // two-word payload rather than a reference to somewhere else
                // again. The header's length carries the width, because a
                // `Boxed` layout cannot know it.
                Inst::Box { dst, src, layout } => {
                    let width = self.width(layout);
                    self.sync(pc - 1);
                    let boxed = match self.allocate(self.boxed_layout(), width) {
                        Ok(addr) => addr,
                        Err(error) => fail!(error),
                    };
                    self.mem.set_payload(boxed, 0, layout.0 as u64);
                    self.mem
                        .copy_words(self.mem.payload_addr(boxed, 1), base + src as u64, width);
                    self.mem.set_slot(base, dst, boxed);
                }
                Inst::Unbox { dst, src, layout } => {
                    let addr = self.mem.slot(base, src);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    if self.mem.payload(addr, 0) != layout.0 as u64 {
                        fail!(RuntimeError::new(
                            "this value is not of the type it is being read as"
                        ));
                    }
                    let width = self.width(layout);
                    self.mem
                        .copy_words(base + dst as u64, self.mem.payload_addr(addr, 1), width);
                }

                // ---- tasks ---------------------------------------------------
                Inst::ScopeEnter { dst, name } => {
                    let named = program.string(name).clone();
                    self.scopes.push(ScopeEntry {
                        name: named,
                        tasks: Vec::new(),
                        closed: false,
                    });
                    // One past the index, so a `Repr::Scope` slot a zeroed
                    // frame has not written names no scope.
                    self.mem.set_slot(base, dst, self.scopes.len() as u64);
                }
                // The body reached its end, so this is the exit that waits.
                // The oracle is `crate::task::wait_for_children`, and what it
                // answers about a failing child is a value here rather than
                // control flow: the lowering knows which `Err` to build and
                // what to return, and this does not.
                Inst::ScopeLeave {
                    scope,
                    failed,
                    error,
                    layout,
                } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    let word = self.mem.slot(base, scope);
                    match self.leave_scope(word, running, span) {
                        Ok(None) => self.mem.set_slot(base, failed, 0),
                        Ok(Some(child)) => {
                            match self.write_child_error(child, base + error as u64, layout) {
                                Ok(()) => self.mem.set_slot(base, failed, 1),
                                Err(error) => fail!(error),
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // The other exit, and the one a jump takes. Nothing is
                // answered: a scope being left early is already leaving with
                // something to say, and a child's failure found on the way
                // out would replace it with an unrelated one.
                Inst::ScopeCancel { scope } => {
                    self.sync(pc - 1);
                    let word = self.mem.slot(base, scope);
                    match self.scope_at(word, self.span(id, pc - 1)) {
                        Ok(at) => self.cancel_scope(at, running),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Spawn {
                    dst,
                    scope,
                    closure,
                    answer,
                } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    let scope_word = self.mem.slot(base, scope);
                    let object = self.mem.slot(base, closure);
                    match self.spawn(scope_word, object, answer, budget, span, threads, running) {
                        Ok(word) => self.mem.set_slot(base, dst, word),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Await { dst, task, answer } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    let word = self.mem.slot(base, task);
                    match self.settle(word, answer, running, span) {
                        Ok(words) => {
                            for (at, held) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *held);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // A call to an `async fn` already ran, here, on this stack.
                // What is left is the handle, and the table is where a
                // `Repr::Task` word's name lives whether a thread ever
                // existed or not.
                Inst::Settled { dst, src, answer } => {
                    self.sync(pc - 1);
                    let words = self.mem.read_words(base + src as u64, self.width(answer));
                    match self.settled(&words, answer, running) {
                        Ok(word) => self.mem.set_slot(base, dst, word),
                        Err(error) => fail!(error),
                    }
                }
                // Asking is all it does. Whether the task stopped or had
                // already finished is known only where something waits for
                // it, which is why `TaskCancelled` is traced at the join.
                Inst::Cancel { task } => {
                    let word = self.mem.slot(base, task);
                    match self.child_at(word, self.span(id, pc - 1)) {
                        Ok(at) => {
                            if matches!(self.children[at].state, ChildState::Running) {
                                self.children[at].cancellation.cancel();
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }

                // ---- cells -------------------------------------------------------
                // Acquire, and then an ordinary `CallClosure` and an
                // `Inst::SharedUnlock` the lowering emitted around it. The
                // roots are published for the length of the wait, because a
                // task waiting for a cell cannot reach a safepoint of its own
                // and a collector that waited for it would be waiting for a
                // task that is waiting for the collector.
                Inst::SharedLock { cell: slot } => {
                    let addr = self.mem.slot(base, slot);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    self.sync(pc - 1);
                    match cell::lock(&self.mem, addr, &Live(self)) {
                        // Recorded so that the machine can give it back on the
                        // one exit path the lowering cannot write. See
                        // [`Machine::held`].
                        Ok(()) => self.held.push(addr),
                        Err(cell::Reentrant) => fail!(reentrant_lock()),
                    }
                }
                Inst::SharedUnlock { cell: slot } => {
                    let addr = self.mem.slot(base, slot);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    cell::unlock(&self.mem, addr);
                    debug_assert_eq!(
                        self.held.last().copied(),
                        Some(addr),
                        "a lock region is left in the order it was entered"
                    );
                    self.held.pop();
                }

                // ---- failure -----------------------------------------------------
                Inst::Trap { message } => {
                    let message = program.string(message).to_string();
                    fail!(RuntimeError::new(message))
                }
                // The only instruction that changes nothing the program can
                // read. `message` is the `String` the failing arm just
                // built, and it is read here rather than carried out in the
                // `Err`, because the `Err` is a value like any other and
                // this is the last moment anything knows which assertion it
                // came from.
                //
                // The bytes are copied. A run goes on after a failed
                // assertion — `?` propagates it, a test catches it — and the
                // object holding them is unreachable as soon as the arm
                // clears its slot, so a reference kept here would be a root
                // nothing walks.
                //
                // Lossily, which is the one place this crate reads a string
                // that way. Every string in the heap was written from valid
                // UTF-8, so bytes that are not are a bug in this machine;
                // the boundary answers that with an error because it is
                // handing the value to a program, and this is a report about
                // a failure that already happened. Stopping a run over the
                // rendering of its own diagnostic would lose the diagnostic.
                Inst::AssertFailed { message } => {
                    let addr = self.mem.slot(base, message);
                    let text = String::from_utf8_lossy(&self.string_bytes(addr)).into_owned();
                    self.assertion_failure = Some((self.span(id, pc - 1), text));
                }
            }
        }
    }

    /// Writes the local program counter back into the top frame.
    ///
    /// Called before anything that reads the frames: a collection, which
    /// walks them for roots, and a failure, which reads a span out of one.
    fn sync(&mut self, pc: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc as u32;
        }
    }

    fn span(&self, id: FunctionId, pc: usize) -> Span {
        self.program.function(id).span_at(pc)
    }

    fn repr(&self, id: FunctionId, slot: Slot) -> Option<Repr> {
        self.program.function(id).repr(slot)
    }

    fn too_deep(&self, span: Span) -> RuntimeError {
        self.too_deep_error().at(span)
    }

    fn too_deep_error(&self) -> RuntimeError {
        RuntimeError::new("this call nests too deeply")
            .with_rule("A recursion that does not terminate is stopped rather than left to run.")
    }

    /// Refuses a frame that would take this task past the embedder's
    /// [`Limits::max_call_depth`].
    ///
    /// This is a *budget*, and the difference from
    /// [`Machine::too_deep_error`] is the whole reason it is a second check.
    /// A frame that would leave this task's stack segment is a stack
    /// overflow: a fact about the memory the run was built with, reported as
    /// a runtime error and classified as one. `max_call_depth` is a number an
    /// embedder chose, reported as a stop, and classified as
    /// [`RunOutcome::CallDepth`] — so a tool that reads a trace can tell a
    /// program that recursed too far from a limit the embedder set, which is
    /// what makes either bound worth acting on. Both stay, and they are asked
    /// in the order the oracle asks them: the unconditional limit first, then
    /// the configured one.
    ///
    /// Counted against *this* task's frames rather than against a total the
    /// run shares, for the reason [`crate::budget::Budget`] gives for not
    /// enforcing this limit itself: ADR 0008 gives each task a stack of its
    /// own, and a shared count would stop a shallow task because a sibling
    /// was deep.
    ///
    /// The limit is read off the meter at the call rather than cached in a
    /// field, and that is a decision about where the answer lives. A
    /// `Machine` holds no [`Meter`] — the run's accounting is passed into
    /// [`Machine::dispatch`], which is what lets one machine serve a session
    /// of invocations each bounded by a budget of its own. A field would have
    /// to be re-bound every time [`crate::Vm::invoke_within`] installed one,
    /// and a stale one would enforce the previous request's limit on this
    /// request. Reading it here cannot go stale, because it comes from the
    /// very meter the safepoints of this run are charging. It costs one
    /// pointer chase through an `Arc` to a field that does not change while a
    /// run lasts, on a path that is already writing a frame.
    ///
    /// [`Limits::max_call_depth`]: crate::budget::Limits::max_call_depth
    /// [`RunOutcome::CallDepth`]: crate::trace::RunOutcome::CallDepth
    fn admit_frame(&self, budget: &Meter, span: Span) -> Result<(), RuntimeError> {
        if let Some(limit) = budget.limits().max_call_depth {
            if self.frames.len() + 1 > limit {
                // The error names the value the limit was configured with, so
                // it is built where that value is rather than here.
                return Err(budget.to_runtime_error(Stopped::CallDepth).at(span));
            }
        }
        Ok(())
    }

    /// How many words a value of `layout` occupies.
    ///
    /// The one question every move in the machine asks now: a value location
    /// is a base slot and a width, and this is the width. It is a table read
    /// rather than a walk, because [`cove_ir::Layout`] caches the flattened
    /// words for exactly the readers that are on this path.
    #[inline]
    fn width(&self, layout: LayoutId) -> u32 {
        self.program.layout(layout).width()
    }

    /// Allocates, collecting once if the first attempt does not fit.
    fn allocate(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        let words = self
            .program
            .layout(layout)
            .payload_words(len, &self.program.layouts);
        if let Some(addr) = self.mem.alloc(layout, len, words) {
            return Ok(addr);
        }
        self.collect();
        self.mem
            .alloc(layout, len, words)
            .ok_or_else(|| RuntimeError::new("this run has no memory left"))
    }

    /// Stops the world and reclaims what nothing this run's tasks hold
    /// reaches.
    ///
    /// This task's own roots are [`Live`]; every other task's are what it
    /// published at the safepoint it parked at. The host resource table is
    /// among neither and could not be: it holds names rather than addresses,
    /// so there is nothing in it for a mark to follow — and a `Repr::Host`
    /// word in a frame is not gathered either, because `Function::refs` is
    /// `RefMap::of` the `Repr`s and `Repr::Host::is_ref` is false. A
    /// `Repr::Task` and a `Repr::Scope` are outside it for the same reason,
    /// and what a task's table *does* name is reached through [`Live`].
    pub(crate) fn collect(&mut self) {
        let done = self.mem.collect(&self.program.layouts, &Live(self));
        self.collected = done;
    }

    /// How many temporary roots are held, for a caller about to take more.
    ///
    /// The mark to hand back to [`Machine::release_temps`]. Taking it and
    /// releasing to it is the whole discipline: a conversion that recurses
    /// nests marks, and a conversion that fails releases on the way out
    /// because the caller that took the mark is the one that releases it.
    pub(crate) fn temps(&self) -> usize {
        self.temps.len()
    }

    /// Holds `addr` as a root until the mark it was taken after is released.
    ///
    /// What this is for is the window in which an object exists and nothing
    /// the collector walks names it: between the allocation of a struct and
    /// the write of its last field, the object is reachable only from a Rust
    /// local, and building one of those fields can allocate. Without a root
    /// here the collector would be right to free it, and the write that
    /// followed would land in a free block.
    pub(crate) fn push_temp(&mut self, addr: u64) {
        self.temps.push(addr);
    }

    /// Releases every temporary root taken since `mark`.
    ///
    /// The object is not freed by this; it stops being a root, which is what
    /// a root has to do the moment something else names it. Releasing rather
    /// than leaving them is what keeps this from becoming the retention the
    /// static reference map was careful not to be.
    pub(crate) fn release_temps(&mut self, mark: usize) {
        self.temps.truncate(mark);
    }

    /// The resource a [`Repr::Host`] word names, or `None` for a word that
    /// names none.
    ///
    /// `None` is two things, and the caller is what tells them apart: the
    /// zero a frame's own zeroing leaves in a slot nothing has written, and a
    /// word from somewhere that is not this table. Both are questions about a
    /// value crossing rather than about the machine, so both are reported by
    /// [`crate::vm::boundary`] and neither is decided here.
    pub(crate) fn resource(&self, word: u64) -> Option<Arc<ResourceHandle>> {
        self.held_resources()
            .get(word.checked_sub(1)? as usize)
            .cloned()
    }

    /// The run's resource table, recovering from a lock a panicking task
    /// left poisoned.
    ///
    /// A table of names is not a state anything recovers *from*: what it held
    /// before the panic is exactly what it holds after, because nothing here
    /// is ever removed. Refusing every later resource operation of the run
    /// because one task panicked would turn a task's failure into the run's,
    /// which is the opposite of what a task boundary is for — the same
    /// reasoning `Space::allocator` gives about the heap's own lock.
    fn held_resources(&self) -> std::sync::MutexGuard<'_, Vec<Arc<ResourceHandle>>> {
        self.resources
            .lock()
            .unwrap_or_else(|held| held.into_inner())
    }

    /// The word naming `handle`, writing it into the table the first time
    /// this run is handed it.
    ///
    /// Interned rather than appended, so that one resource is one word for
    /// the length of a run. ADR 0013 says two handles are equal when they
    /// name the same resource, and a table that gave one resource two words
    /// would be a table on which comparing the words was not comparing the
    /// resources — which is the one thing an untagged word naming a resource
    /// has to get right. `task_safe` is not part of the comparison for the
    /// same reason it is not part of [`ResourceHandle::names_same`]: it is a
    /// fact about the kind, copied onto every handle of it, so two handles
    /// naming one resource cannot disagree about it.
    ///
    /// The scan is linear over the resources this run has been handed. That
    /// is the table the host is keeping too, at the size the host keeps it.
    pub(crate) fn resource_word(&mut self, handle: &ResourceHandle) -> u64 {
        let mut held = self.held_resources();
        if let Some(at) = held.iter().position(|kept| kept.names_same(handle)) {
            return at as u64 + 1;
        }
        held.push(Arc::new(handle.clone()));
        // One past the index, because a zeroed slot has to mean no resource.
        held.len() as u64
    }

    /// Materialises the arguments, calls the host, and writes its answer
    /// back as a word.
    ///
    /// This follows [`crate::interp::Interpreter::call_host`] rather than
    /// inventing an order of its own, because what a host call does is a fact
    /// about the language and not about a backend. The registry is what
    /// charges [`crate::Budget::charge_host_call`], refuses an ungranted
    /// capability, holds the arguments and the answer to the operation's
    /// schema, and writes the `HostCall` trace event; a backend that repeated
    /// any of that would be a second opinion about a question that already has
    /// one. What is left for the machine is the three things only it can do:
    /// read the words out as the `Repr`s of the slots they came from, wait,
    /// and write the answer back.
    ///
    /// The run's own cancellation is not checked here. The oracle checks a
    /// *task's* flag and the flag of every bounded call its thread is inside,
    /// neither of which this machine has yet; the run's flag is read inside
    /// the boundary by `charge_host_call`, which is where it is read on every
    /// backend.
    #[allow(clippy::too_many_arguments)]
    fn call_host<'s>(
        &mut self,
        base: u64,
        op: HostOpId,
        args: ArgsId,
        budget: &Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let op = program.host_op(op);
        let list = program.arg_list(args);

        // An argument names a value location, so each one materialises as
        // the whole of what is at it. That is why a struct or an enum
        // reaches a host as itself: it used to be boxed on the way in — a
        // slot said where an operand began and never how wide it was — and
        // the host was handed an erased value where the schema declared a
        // concrete one.
        let mut values = Vec::with_capacity(list.len());
        for arg in list {
            let words = self
                .mem
                .read_words(base + arg.slot as u64, self.width(arg.layout));
            values.push(
                boundary::to_value(self, arg.layout, &words).map_err(|error| error.at(span))?,
            );
        }

        let hosts = self.hosts.ok_or_else(|| {
            RuntimeError::new(format!(
                "`{}.{}` cannot be called, because this run has no host boundary",
                op.module, op.operation
            ))
            .at(span)
        })?;
        stopped_here(self.cancellation.as_ref(), &self.stops, span)?;
        self.charge_at_host_boundary(budget, span)?;
        let started = Instant::now();
        let answer = {
            // A task inside a host call is not running Cove and its frames do
            // not change, so the snapshot it leaves stays true for the whole
            // call — and a collection that waited for it instead would be
            // waiting for something outside the run altogether. A callback is
            // the exception, and [`Back::call`] is where it says so: the
            // moment Cove runs again the snapshot stops being true, so the
            // park is dropped for exactly as long as the callback runs.
            let mut back = Back::parked(self, budget, span, threads, running);
            hosts.call_with(&op.module, &op.operation, values, &mut back)
        };
        self.host_wait += started.elapsed();
        let answer = answer.map_err(|error| error.at(span))?;
        let result = op.result;
        boundary::from_value(self, result, &answer).map_err(|error| error.at(span))
    }

    /// The same, addressed to the resource the [`Repr::Host`] word in
    /// `receiver` names.
    ///
    /// Everything [`Machine::call_host`] does, through the one seam that
    /// differs: `HostRegistry::call_resource` rather than
    /// `HostRegistry::call_with`. The grant, the schema on both sides, the
    /// budget and the trace are the registry's on this path too — a resource
    /// operation is a Host API call and is charged and recorded as one — and
    /// this follows `crate::interp::Interpreter::call_host_resource`, which
    /// is the same three lines around the same call.
    ///
    /// The handle is looked up rather than materialised. ADR 0013 makes it a
    /// name the host minted, `Machine::resource` is the table that word
    /// indexes, and the registry takes it as the thing being addressed — so
    /// it is never one of `args`, and the arguments are what the host is
    /// handed.
    ///
    /// A zero word is refused rather than read through. `docs/LINEAR_VM.md`
    /// is explicit that the word is one past the index so that a slot nothing
    /// has written names no resource, and that zero *"earns the same refusal
    /// a null reference does"* — which is this one, because a `Host` slot
    /// read before it was given a handle is the same lowering bug reaching
    /// the machine.
    #[allow(clippy::too_many_arguments)]
    fn call_resource<'s>(
        &mut self,
        base: u64,
        receiver: Slot,
        op: HostOpId,
        args: ArgsId,
        budget: &Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let op = program.host_op(op);
        let list = program.arg_list(args);

        let word = self.mem.slot(base, receiver);
        let Some(handle) = self.resource(word) else {
            return Err(null_object().at(span));
        };

        let mut values = Vec::with_capacity(list.len());
        for arg in list {
            let words = self
                .mem
                .read_words(base + arg.slot as u64, self.width(arg.layout));
            values.push(
                boundary::to_value(self, arg.layout, &words).map_err(|error| error.at(span))?,
            );
        }

        let hosts = self.hosts.ok_or_else(|| {
            RuntimeError::new(format!(
                "`{}` cannot be called, because this run has no host boundary",
                op.qualified()
            ))
            .at(span)
        })?;
        stopped_here(self.cancellation.as_ref(), &self.stops, span)?;
        // A resource operation is a Host API call and is bounded as one, so
        // ADR 0030's boundary is here for the reason it is in
        // [`Machine::call_host`] and in the same order.
        self.charge_at_host_boundary(budget, span)?;
        let started = Instant::now();
        let answer = {
            let mut back = Back::parked(self, budget, span, threads, running);
            hosts.call_resource(&handle, &op.operation, values, &mut back)
        };
        self.host_wait += started.elapsed();
        let answer = answer.map_err(|error| error.at(span))?;
        let result = op.result;
        boundary::from_value(self, result, &answer).map_err(|error| error.at(span))
    }

    /// Reads the operand words out of the frame and hands them to the
    /// builtin.
    ///
    /// An operand is a value location: the layout the argument names and the
    /// words at its slot. Both halves are read here rather than in
    /// [`crate::vm::builtins`] for the reason the boundary takes them here
    /// too — a word is untagged and where it came from is a fact about this
    /// frame, which a builtin has no business knowing about.
    ///
    /// The words are copied into one buffer and the operands point into it,
    /// so a builtin reads a whole `Point` without holding a frame and
    /// without the argument list having to promise that consecutive operands
    /// are adjacent, which it never could: the lowering places each argument
    /// where a run of the right shape was free.
    fn call_builtin(
        &mut self,
        base: u64,
        builtin: BuiltinId,
        args: ArgsId,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let list = program.arg_list(args);
        let mut words: Vec<u64> = Vec::with_capacity(list.len());
        let mut runs = Vec::with_capacity(list.len());
        for arg in list {
            let from = words.len();
            let width = self.width(arg.layout);
            for at in 0..width {
                words.push(self.mem.slot(base, arg.slot + at));
            }
            runs.push((arg.layout, from, words.len()));
        }
        let operands: Vec<Operand> = runs
            .iter()
            .map(|(layout, from, to)| Operand {
                layout: *layout,
                words: &words[*from..*to],
            })
            .collect();
        builtins::call(self, program.builtin(builtin), &operands)
    }

    /// The string object for `text`, allocated the first time it is asked for.
    fn intern(&mut self, text: StrId) -> Result<u64, RuntimeError> {
        if self.interned[text.index()] != 0 {
            return Ok(self.interned[text.index()]);
        }
        let bytes = self.program.string(text).clone();
        let addr = self.allocate(self.program.str_layout, bytes.len() as u32)?;
        self.write_bytes(addr, bytes.as_bytes());
        self.interned[text.index()] = addr;
        Ok(addr)
    }

    /// The layout a [`Inst::Box`] allocates its object as.
    ///
    /// The program says, rather than this searching for a `Shape::Boxed`.
    /// A search has to answer something when it fails, and the answer it
    /// used to give — `LayoutId::FREE` — sized the object by the wrong
    /// shape, so a box of a two-word value was allocated one word short and
    /// the copy into it ran off the end of the heap.
    fn boxed_layout(&self) -> LayoutId {
        self.program.boxed_layout
    }

    /// Checks that `addr` is an object with a payload word `at`.
    ///
    /// A reference slot carries no layout, so the object is the only thing
    /// that can say how wide it is. The lowering computed `at` from the type
    /// the checker settled, so this should never refuse — and it is here
    /// because "should never" is not "cannot", and reading past an object
    /// into whatever follows it would be a silent wrong answer rather than a
    /// loud one.
    fn checked(&self, addr: u64, at: u32, width: u32) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let layout = self.program.layout(self.mem.object_layout(addr));
        let words = layout.payload_words(self.mem.object_len(addr), &self.program.layouts);
        if at + width > words {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {words}",
                layout.name
            )));
        }
        Ok(())
    }

    /// The function the closure object at `addr` calls.
    ///
    /// Three things have to hold before a frame is pushed, and none of them
    /// is something a program can get wrong: the object has to be a closure's,
    /// the callee it names has to be one this program has, and the captures it
    /// holds have to be the ones that callee reads. The checker resolved the
    /// callee's type and the verifier holds the slot to `Repr::Ref`, so each
    /// of the three is a lowering bug — reported for the reason
    /// [`Machine::checked`] is, because the alternative is a frame whose
    /// capture slots hold whatever followed the object in the heap.
    ///
    /// The id comes from the object rather than from the layout, which carries
    /// one too. They agree — a layout is one per lowered lambda — and the
    /// object's word is the one [`Inst::CallClosure`] is defined in terms of.
    pub(crate) fn callee_of(&self, addr: u64) -> Result<FunctionId, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let program = self.program;
        let layout = program.layout(self.mem.object_layout(addr));
        let Shape::Closure { captures, .. } = &layout.shape else {
            // The oracle's words for a call of something that is not a
            // function, with the name the layout carries — which is the name
            // the declaration wrote, and so the one a `Value` of this object
            // would answer.
            return Err(RuntimeError::new(format!(
                "`{}` is not callable",
                layout.name
            )));
        };
        let word = self.mem.payload(addr, 0);
        let callee = u32::try_from(word)
            .ok()
            .map(FunctionId)
            .filter(|id| id.index() < program.functions.len())
            .ok_or_else(|| {
                RuntimeError::new(format!(
                    "this closure names function {word}, which this program has not"
                ))
            })?;
        let target = program.function(callee);
        if target.captures.len() != captures.len() {
            return Err(RuntimeError::new(format!(
                "this closure and `{}` disagree about its captures: {} held, {} read",
                target.qualified(),
                captures.len(),
                target.captures.len()
            )));
        }
        Ok(callee)
    }

    /// Turns a language-level index into a payload offset, at a stride of
    /// `width`.
    ///
    /// The header's length counts *elements*, not words, so an index is
    /// checked against it and then multiplied — which is what makes an
    /// `Array<Point>` a run of two-word elements and an out-of-range index on
    /// one say the same thing it says on an `Array<Int>`.
    fn element(&self, addr: u64, at: i64, width: u32) -> Result<u32, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let len = self.mem.object_len(addr) as i64;
        if at < 0 || at >= len {
            return Err(
                RuntimeError::new(format!("index {at} is outside a collection of {len}"))
                    .with_rule("An index outside a collection is a broken invariant."),
            );
        }
        Ok(at as u32 * width)
    }

    /// Orders two string objects by their bytes.
    fn compare_strings(&self, a: u64, b: u64) -> std::cmp::Ordering {
        self.string_bytes(a).cmp(&self.string_bytes(b))
    }

    /// The bytes of the string object at `addr`.
    ///
    /// A null address answers the empty string rather than failing: the one
    /// caller that can see one is the comparison, and two strings one of
    /// which does not exist is a lowering bug the verifier will catch
    /// elsewhere, not something to unwind a comparison for.
    pub(crate) fn string_bytes(&self, addr: u64) -> Vec<u8> {
        if addr == 0 {
            return Vec::new();
        }
        let len = self.mem.object_len(addr) as usize;
        let mut out = Vec::with_capacity(len);
        for at in 0..len.div_ceil(8) {
            let word = self.mem.payload(addr, at as u32);
            for byte in 0..8 {
                if out.len() == len {
                    break;
                }
                out.push((word >> (byte * 8)) as u8);
            }
        }
        out
    }

    /// A new string object holding `text`.
    ///
    /// Unlike [`Machine::intern`] this allocates every time. Interning is for
    /// a literal, which the program named statically and can name again; a
    /// string that arrived from outside has no such name and retaining every
    /// one a host ever answered would be a leak with a table in front of it.
    pub(crate) fn new_string(&mut self, text: &str) -> Result<u64, RuntimeError> {
        let addr = self.allocate(self.program.str_layout, text.len() as u32)?;
        self.write_bytes(addr, text.as_bytes());
        Ok(addr)
    }

    fn write_bytes(&mut self, addr: u64, bytes: &[u8]) {
        for (at, chunk) in bytes.chunks(8).enumerate() {
            let mut word = 0u64;
            for (byte, value) in chunk.iter().enumerate() {
                word |= (*value as u64) << (byte * 8);
            }
            self.mem.set_payload(addr, at as u32, word);
        }
    }

    /// The program this machine runs.
    pub(crate) fn program(&self) -> &'a Program {
        self.program
    }

    /// What the object at `addr` is, for a boundary that has to name it.
    pub(crate) fn object_layout(&self, addr: u64) -> LayoutId {
        self.mem.object_layout(addr)
    }

    /// The length field of the object at `addr`: elements, or a string's
    /// bytes.
    pub(crate) fn object_len(&self, addr: u64) -> u32 {
        self.mem.object_len(addr)
    }

    /// Payload word `at` of the object at `addr`.
    pub(crate) fn payload(&self, addr: u64, at: u32) -> u64 {
        self.mem.payload(addr, at)
    }

    /// Writes payload word `at` of the object at `addr`.
    pub(crate) fn set_payload(&mut self, addr: u64, at: u32, word: u64) {
        self.mem.set_payload(addr, at, word);
    }

    /// Re-labels the object at `addr`, releasing the `spare` words it gives
    /// up. See [`Memory::relabel`].
    pub(crate) fn relabel(&mut self, addr: u64, layout: LayoutId, len: u32, spare: u32) {
        let payload = self.payload_words(layout, len);
        self.mem.relabel(addr, layout, len, payload, spare);
    }

    /// The `words` payload words of the object at `addr`, from `at`.
    ///
    /// What a boundary reads when a value is inline in a payload: an array
    /// element, a capture, a struct field, the value inside a box. Nothing in
    /// ordinary execution calls it — a move inside the machine never leaves
    /// the memory.
    pub(crate) fn payload_run(&self, addr: u64, at: u32, words: u32) -> Vec<u64> {
        self.mem.read_words(self.mem.payload_addr(addr, at), words)
    }

    /// Writes `words` into the payload of the object at `addr`, from `at`.
    pub(crate) fn set_payload_run(&mut self, addr: u64, at: u32, words: &[u64]) {
        for (offset, word) in words.iter().enumerate() {
            self.mem.set_payload(addr, at + offset as u32, *word);
        }
    }

    /// How many words a value of `layout` occupies, for a caller outside the
    /// dispatch loop.
    pub(crate) fn words_of(&self, layout: LayoutId) -> u32 {
        self.width(layout)
    }

    /// How many payload words an object of `layout` with header length `len`
    /// occupies.
    pub(crate) fn payload_words(&self, layout: LayoutId, len: u32) -> u32 {
        self.program
            .layout(layout)
            .payload_words(len, &self.program.layouts)
    }

    // ---- the scheduler -----------------------------------------------------

    /// The scope a `Repr::Scope` word names.
    fn scope_at(&self, word: u64, span: Span) -> Result<usize, RuntimeError> {
        word.checked_sub(1)
            .map(|at| at as usize)
            .filter(|at| *at < self.scopes.len())
            .ok_or_else(|| no_such_handle("task scope").at(span))
    }

    /// The task a `Repr::Task` word names.
    fn child_at(&self, word: u64, span: Span) -> Result<usize, RuntimeError> {
        word.checked_sub(1)
            .map(|at| at as usize)
            .filter(|at| *at < self.children.len())
            .ok_or_else(|| no_such_handle("task").at(span))
    }

    /// `scope.spawn { ... }`: a thread for the closure, and the handle the
    /// scope now owns.
    ///
    /// This follows `crate::task::spawn_into` step for step, because what a
    /// `spawn` decides is a fact about the language rather than about a
    /// backend: the scope has to still be open, the run's concurrency limit
    /// is charged **before** the task is given an id, an event or a thread,
    /// the trace records the spawn before the thread starts so that a task is
    /// never seen completing before it was seen spawning, and a place charged
    /// for a task that never got a thread goes back.
    ///
    /// What differs is the two things only this backend can do. The answer's
    /// object is allocated here, before the thread exists, so that it is a
    /// root of this task from the moment it can hold anything; and the child
    /// is handed a [`Memory`] over a stack segment of its own and the run's
    /// one heap, which is the whole of issue #240's Q1.
    ///
    /// It returns once the thread exists and orders nothing else. ADR 0008's
    /// amendment refuses a rendezvous here, and so does this.
    #[allow(clippy::too_many_arguments)]
    fn spawn<'s>(
        &mut self,
        scope_word: u64,
        object: u64,
        answer: LayoutId,
        budget: &Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Result<u64, RuntimeError> {
        let at = self.scope_at(scope_word, span)?;
        if self.scopes[at].closed {
            return Err(task::scope_already_left(&self.scopes[at].name, span));
        }
        if object == 0 {
            return Err(null_object().at(span));
        }
        // Charged before this task is given an id, an event or a thread: a
        // thread that has started is a resource already taken, which no later
        // safepoint could refuse.
        if let Some(hosts) = self.hosts {
            if let Some(Err(error)) = hosts.with_budget(|held| {
                held.charge_task()
                    .map_err(|stopped| held.to_runtime_error(stopped))
            }) {
                return Err(error.at(span));
            }
        }

        match self.launch(at, object, answer, budget, span, threads, running) {
            Ok(word) => Ok(word),
            Err(error) => {
                // A task the machine refused is not a task the run holds, so
                // the place charged for it above goes back.
                if let Some(hosts) = self.hosts {
                    hosts.with_budget(|held| held.release_task());
                }
                Err(error)
            }
        }
    }

    /// Everything after the concurrency limit has been charged.
    ///
    /// Split out so that every way of failing after the charge gives the
    /// place back, in one place rather than at each way out.
    #[allow(clippy::too_many_arguments)]
    fn launch<'s>(
        &mut self,
        at: usize,
        object: u64,
        answer: LayoutId,
        budget: &Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Result<u64, RuntimeError> {
        // The segment first, because taking one can wait: a task joining a
        // run whose collection has already begun waits it out, and a task
        // that waited without publishing its roots would be a task the
        // collector waits for while it waits for the collector. So this task
        // parks for the length of the wait, exactly as it does around a host
        // call and around a join.
        let segment = {
            let parked = self.mem.blocking(&Live(self));
            let taken = self.mem.for_task();
            drop(parked);
            taken
        };
        let segment = segment
            .map_err(|NoSegment| no_segment_left().at(span).with_rule(crate::budget::RULE))?;

        // Then the answer's home, allocated before the thread exists so that
        // it is a root of this task from the moment it can hold anything. The
        // closure is in a `Repr::Ref` slot of a live frame, so a collection
        // here finds it — which is the one thing this allocation could
        // otherwise have taken away.
        let width = self.width(answer);
        let home = self.allocate(self.boxed_layout(), width)?;
        self.mem.set_payload(home, 0, answer.0 as u64);

        let id = match self.runtime {
            Some(runtime) => runtime.next_task_id(),
            None => {
                self.next_task += 1;
                self.next_task - 1
            }
        };
        let scope = self.scopes[at].name.clone();
        let position = self.scopes[at].tasks.len() + 1;
        // Traced before the thread starts, so a task is never seen completing
        // before it was seen spawning.
        if let Some(runtime) = self.runtime {
            runtime.trace(TraceEvent::TaskSpawned {
                id,
                parent: (self.task != ENTRY_TASK).then_some(self.task),
                scope: scope.to_string(),
            });
        }

        let cancellation = Cancellation::new();
        let program = self.program;
        let hosts = self.hosts;
        let runtime = self.runtime;
        let resources = Arc::clone(&self.resources);
        let meter = budget.clone();
        let flag = cancellation.clone();
        let handle = threads.spawn(move || {
            run_task(
                program, hosts, runtime, resources, segment, meter, flag, id, object, home, span,
            )
        });

        let index = self.children.len();
        self.children.push(Child {
            id,
            position,
            scope,
            cancellation,
            closure: object,
            answer: home,
            layout: answer,
            state: ChildState::Running,
        });
        running.push(Some(handle));
        self.scopes[at].tasks.push(index);
        // One past the index, because a zeroed slot has to mean no task.
        Ok(index as u64 + 1)
    }

    /// The handle a call to an `async fn` answers, around words the call has
    /// already produced.
    ///
    /// [`crate::task::Task::settled`] is the oracle and this is the same
    /// thing in a table: a task with no thread, whose value is known before
    /// the handle exists. Everything downstream then works without being
    /// told which kind it has — [`Machine::join`] returns at once because the
    /// state is not `Running`, [`Machine::settle`] reads the answer object
    /// the way it reads a spawned task's, and an `Inst::Cancel` does nothing
    /// to a task that is not running, exactly as `Task::cancel` does nothing.
    ///
    /// Three things it deliberately does not do, each because the oracle does
    /// not do it either. It takes **no place under the concurrency limit**:
    /// nothing was started, and a limit on how many tasks run at once is not
    /// a limit on how many `async fn` calls a program makes. It is **not put
    /// in any scope**, so leaving a scope neither waits for it nor cancels
    /// it — there is nothing left to wait for. And it is **not traced**: it
    /// is `id` zero, the identity `crate::task::Task` gives a handle that
    /// "never appears in a trace because it never ran as a task".
    ///
    /// What it costs is one table entry and one object, kept for the rest of
    /// the run. That is the price of a `Repr::Task` word being a name rather
    /// than an address: the collector reads a static per-slot map and never
    /// inspects a word, so the answer object has to be reachable from
    /// somewhere the collector walks, and the table is that somewhere. The
    /// oracle's `Rc<Task>` is freed when the last handle goes; a handle here
    /// can be inside a `Vector<Task<T>>` or a struct field, and no static map
    /// can say when the last one died.
    fn settled(
        &mut self,
        words: &[u64],
        answer: LayoutId,
        running: &mut Vec<Option<ScopedJoinHandle<'_, Outcome>>>,
    ) -> Result<u64, RuntimeError> {
        // The same object a spawned task's answer goes into, so that
        // `Machine::settle` reads one shape rather than two.
        let home = self.allocate(self.boxed_layout(), words.len() as u32)?;
        self.mem.set_payload(home, 0, answer.0 as u64);
        for (at, word) in words.iter().enumerate() {
            self.mem.set_payload(home, 1 + at as u32, *word);
        }
        let index = self.children.len();
        self.children.push(Child {
            // Position zero and this name are what `crate::task::describe`
            // renders as *this task*: a handle with no place in a spawn
            // order, because there was no spawn.
            id: 0,
            position: 0,
            scope: Arc::from("this call"),
            cancellation: Cancellation::new(),
            closure: 0,
            answer: home,
            layout: answer,
            state: ChildState::Settled,
        });
        // The two lists are one list at two indices, so a task with no thread
        // still takes its place in both.
        running.push(None);
        Ok(index as u64 + 1)
    }

    /// `await task`: waits for the thread and answers the words its body
    /// produced.
    ///
    /// A body runs at most once and is waited for at most once, so awaiting
    /// the same handle twice answers the same value and repeats no effect —
    /// which falls out of the state rather than being arranged, exactly as it
    /// does in `crate::task::settle`.
    fn settle(
        &mut self,
        word: u64,
        answer: LayoutId,
        running: &mut [Option<ScopedJoinHandle<'_, Outcome>>],
        span: Span,
    ) -> Result<Vec<u64>, RuntimeError> {
        let at = self.child_at(word, span)?;
        // A task blocked on an `await` is standing where a safepoint would
        // be, so it is owed the answer one gives: a cancelled task does not
        // wait for a sibling it will never read.
        stopped_here(self.cancellation.as_ref(), &[], span)?;
        self.join(at, running);
        match &self.children[at].state {
            ChildState::Settled => {
                let width = self.width(answer);
                let home = self.children[at].answer;
                Ok(self.mem.read_words(self.mem.payload_addr(home, 1), width))
            }
            ChildState::Failed(error) => Err(error.clone()),
            ChildState::Cancelled => Err(task::awaiting_a_cancelled(
                &self.children[at].describe(),
                span,
            )),
            ChildState::Running => {
                unreachable!("joining a task leaves it settled, failed, or cancelled")
            }
        }
    }

    /// Waits for one task's thread and records what it produced.
    ///
    /// This is `crate::task::join` and `crate::task::Task::join` together,
    /// and the three things they decide are decided here in the same order: a
    /// task that stopped after its own cancellation was requested is
    /// *cancelled* rather than failed, because that is the stop the program
    /// asked for; the place it held under the concurrency limit goes back at
    /// the join rather than on the task's own thread, so that what a `spawn`
    /// is refused for does not depend on how quickly a sibling finished; and
    /// `TaskCancelled` is traced here, because this is the only place that
    /// knows a cancellation stopped work rather than arriving after it.
    fn join(&mut self, at: usize, running: &mut [Option<ScopedJoinHandle<'_, Outcome>>]) {
        if !matches!(self.children[at].state, ChildState::Running) {
            return;
        }
        let Some(handle) = running[at].take() else {
            return;
        };
        let outcome = {
            // Published for the whole wait. A task waiting for a sibling
            // cannot reach a safepoint of its own, and a collector that
            // waited for it would be waiting for a task that is waiting for a
            // task that is waiting for the collector.
            let parked = self.mem.blocking(&Live(self));
            let outcome = handle.join();
            drop(parked);
            outcome
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            // A panic is a broken invariant in the task's own thread. The
            // message has already reached stderr; what this task needs is an
            // error rather than a value that never arrived.
            Err(_) => Err(task::broken_invariant(&self.children[at].describe())),
        };
        let cancelled = self.children[at].cancellation.is_cancelled();
        self.children[at].state = match outcome {
            Ok(()) => ChildState::Settled,
            Err(_) if cancelled => ChildState::Cancelled,
            Err(error) => ChildState::Failed(error),
        };
        // The body is over, so the environment it was entered through is no
        // longer anything's to keep: the captures it held were copied into
        // the child's frame before its first instruction.
        self.children[at].closure = 0;
        if let Some(hosts) = self.hosts {
            hosts.with_budget(|held| held.release_task());
        }
        if matches!(self.children[at].state, ChildState::Cancelled) {
            if let Some(runtime) = self.runtime {
                runtime.trace(TraceEvent::TaskCancelled {
                    id: self.children[at].id,
                });
            }
        }
    }

    /// Waits for every child of a scope the body reached the end of **that
    /// the body did not await**, and answers the first that failed in a way
    /// the enclosing function has to pass on.
    ///
    /// `crate::task::wait_for_children` is the oracle and this is its
    /// translation. Waiting is in spawn order, which is an order of
    /// *observation* only — the tasks ran at the same time on threads of
    /// their own — and a task the program itself cancelled is neither a
    /// failure nor a success, because the program asked for that stop.
    ///
    /// # Why an awaited child is skipped
    ///
    /// `if !task.is_running() { continue }` is the oracle's first line and it
    /// is a decision rather than an optimisation: a child the body awaited
    /// has already handed its value to the program, and the program has
    /// already done whatever it does with one. Reporting it again here would
    /// overwrite the answer the body computed *from* that failure with the
    /// failure itself, so
    ///
    /// ```cove
    /// let answer = task.await()
    /// match answer { Ok(n) => n, Err(_) => fallback() }
    /// ```
    ///
    /// could not recover from a failed child at all — leaving the scope would
    /// throw the recovery away. What is left to wait for is what nothing has
    /// read, which is the case the rule exists for: a failure sitting unread
    /// in a handle nobody awaited reaches the caller rather than vanishing.
    ///
    /// A child is "awaited" here for the same reason it is there: joining is
    /// what settles a child's state, and [`Machine::settle`] joins. So a
    /// state that is no longer [`ChildState::Running`] is exactly a child
    /// something has already waited for.
    ///
    /// `Ok(Some(child))` is a child whose value was `Err(...)`; `Err` is a
    /// child that raised, which propagates as itself. Either way the tasks
    /// still running are cancelled and waited for before this answers.
    fn leave_scope(
        &mut self,
        word: u64,
        running: &mut [Option<ScopedJoinHandle<'_, Outcome>>],
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        let at = self.scope_at(word, span)?;
        let mut index = 0;
        let mut failure = None;
        let mut raised = None;
        // Read by index rather than from a snapshot, so a scope that grew
        // while it was being left is still waited for to the end.
        while let Some(&child) = self.scopes[at].tasks.get(index) {
            index += 1;
            if !matches!(self.children[child].state, ChildState::Running) {
                continue;
            }
            self.join(child, running);
            match &self.children[child].state {
                ChildState::Settled => {
                    if self.child_failed(child) {
                        failure = Some(child);
                        break;
                    }
                }
                ChildState::Failed(error) => {
                    raised = Some(error.clone());
                    break;
                }
                ChildState::Cancelled | ChildState::Running => {}
            }
        }
        if failure.is_some() || raised.is_some() {
            self.cancel_scope(at, running);
        }
        self.scopes[at].closed = true;
        match raised {
            Some(error) => Err(error),
            None => Ok(failure),
        }
    }

    /// Cancels every running child of a scope and waits for it to stop.
    ///
    /// Every child is asked first and waited for afterwards, so they stop at
    /// the same time rather than one after another —
    /// `crate::task::cancel_children`, in the same two passes.
    fn cancel_scope(&mut self, at: usize, running: &mut [Option<ScopedJoinHandle<'_, Outcome>>]) {
        for &child in &self.scopes[at].tasks {
            if matches!(self.children[child].state, ChildState::Running) {
                self.children[child].cancellation.cancel();
            }
        }
        let mut index = 0;
        while let Some(&child) = self.scopes[at].tasks.get(index) {
            index += 1;
            self.join(child, running);
        }
        self.scopes[at].closed = true;
    }

    /// Whether any task this machine spawned has not been joined.
    ///
    /// Asked only by a debug assertion, and what it is asserting is the
    /// reason the answer's words are safe to carry out of the frame that
    /// produced them: [`Machine::stop_all`] can block, and a task that blocks
    /// publishes its roots — which no longer name a popped frame. On the path
    /// that answers words there is nothing to block for, because every scope
    /// was left where it was written and leaving one joins its children.
    fn anything_running(&self) -> bool {
        self.children
            .iter()
            .any(|child| matches!(child.state, ChildState::Running))
    }

    /// Cancels and joins every task still running, whatever scope it is in.
    ///
    /// The unwind path, and the one exit a scope has that the lowering cannot
    /// write: a runtime error is not a jump, so no `ScopeCancel` stands
    /// between it and the end of the run. Without this the thread scope would
    /// wait for a task nothing had asked to stop.
    ///
    /// On the ordinary path it has nothing to do, because every scope was
    /// left where it was written.
    fn stop_all(&mut self, running: &mut [Option<ScopedJoinHandle<'_, Outcome>>]) {
        for child in &self.children {
            if matches!(child.state, ChildState::Running) {
                child.cancellation.cancel();
            }
        }
        for at in 0..self.children.len() {
            self.join(at, running);
        }
    }

    /// Gives back every cell this task took above `mark`, innermost first.
    ///
    /// The unwind path for a `lock`, and the exact analogue of
    /// [`Machine::stop_all`]: a runtime error is not a jump, so no
    /// `Inst::SharedUnlock` stands between it and the end of the run, and a
    /// cell nobody gave back is a cell no task can ever take. On the ordinary
    /// path it has nothing to do, because every lock region was left where it
    /// was written.
    ///
    /// Innermost first, because that is the order the regions would have
    /// ended in.
    fn give_cells_back(&mut self, mark: usize) {
        while self.held.len() > mark {
            let addr = self.held.pop().expect("the length is above the mark");
            cell::unlock(&self.mem, addr);
        }
    }

    /// Whether a settled child's value was `Err(...)`.
    ///
    /// The answer object holds the layout of what it carries in its first
    /// payload word and the value's words after it, so the discriminant is
    /// the second. A child whose answer is not an enum with an `Err` case did
    /// not fail this way and cannot: `crate::task::failure_of` asks the same
    /// question of a materialised value and answers `None` for the same
    /// values.
    fn child_failed(&self, at: usize) -> bool {
        self.err_part(at)
            .is_some_and(|(index, _, _)| self.mem.payload(self.children[at].answer, 1) == index)
    }

    /// Where a child's `Err` payload is in its answer object: the case index,
    /// the payload word it begins at, and its layout.
    fn err_part(&self, at: usize) -> Option<(u64, u32, LayoutId)> {
        let layout = self.program.layout(self.children[at].layout);
        let Shape::Enum { cases, .. } = &layout.shape else {
            return None;
        };
        let index = cases.iter().position(|case| &*case.name == "Err")?;
        let part = cases[index].parts.first()?;
        // Word 0 of the object is the held layout and word 1 is the value's
        // discriminant, so the payload region begins at word 2.
        Some((index as u64, 2 + part.at, part.layout))
    }

    /// Copies a failing child's `Err` payload into the location the enclosing
    /// function will wrap and return.
    ///
    /// The two layouts are held to being one. They are not the same fact —
    /// one is what the child answered and the other is what the function the
    /// scope was written in fails with — and the checker never had to unify
    /// them, because the oracle wraps whatever it finds in a `Value::err` and
    /// asks nothing. Here a run of words copied at the wrong width is the one
    /// fault this backend must not have quietly, so a disagreement is
    /// reported rather than truncated.
    fn write_child_error(
        &mut self,
        at: usize,
        into: u64,
        layout: LayoutId,
    ) -> Result<(), RuntimeError> {
        let Some((_, word, held)) = self.err_part(at) else {
            return Err(RuntimeError::new(
                "this task failed with a value that is not an error the enclosing function can \
                 answer with",
            ));
        };
        if held != layout {
            let found = self.program.layout(held).name.clone();
            let wanted = self.program.layout(layout).name.clone();
            return Err(RuntimeError::new(format!(
                "this task failed with a `{found}`, and the function its scope was written in \
                 answers a `{wanted}`"
            )));
        }
        let width = self.width(layout);
        let from = self.mem.payload_addr(self.children[at].answer, word);
        self.mem.copy_words(into, from, width);
        Ok(())
    }

    /// Publishes this task's roots and stands at a safepoint until the
    /// answer is dropped.
    ///
    /// [`crate::vm::mem::Parked`] rather than the borrowed guard, because
    /// the one caller that needs it is [`Back`], which holds this machine
    /// mutably and so cannot also hold a guard borrowing its memory.
    fn park(&self) -> Parked {
        self.mem.park(&Live(self))
    }

    /// Runs a Cove callable from outside the dispatch loop, which is what a
    /// host callback needs and the only thing that does.
    ///
    /// # The convention
    ///
    /// **The call opens its frame at the top of the stack region as it
    /// stands, and leaves it exactly as it found it.** The callee's frame
    /// begins where the deepest live one ends, which is what
    /// [`Memory::push_frame`] answers and what an [`Inst::Call`] would have
    /// got; the arguments are written into it as the parameters' words, the
    /// captures follow them out of the environment object, and the frame is
    /// popped by the return that answers. The frame stack grows above the
    /// frame the interrupted instruction belongs to and comes back down to
    /// it, which is what `floor` means in [`Machine::dispatch`].
    ///
    /// That the outer frames are *left* rather than unwound is the whole
    /// reason this is another turn of the loop rather than a jump: the
    /// instruction that made the host call has not finished, and its frame's
    /// slots are live — including, in every case that matters, the slot
    /// holding the closure that is being called.
    ///
    /// # What a failure leaves
    ///
    /// Nothing. A host may catch what a callback failed with and carry on —
    /// `clock.timeout` is written to — so the frames, the stack region, the
    /// task scopes the callback opened and the tasks it spawned are all put
    /// back the way they were found. The outer run has no unwinding because
    /// an abandoned frame's slots stay on the stack until the run ends, which
    /// is sound only because the run is ending, and that reasoning does not
    /// reach here.
    ///
    /// # What is still accounted
    ///
    /// Everything the loop accounts, because it is the loop. Fuel is charged
    /// every [`SAFEPOINT_STRIDE`] instructions, a frame that would leave this
    /// task's stack segment is a stack overflow, and every safepoint the
    /// callee reaches asks what a safepoint asks — including
    /// [`Machine::stops`], which [`Reentry::call_until`] pushes onto.
    ///
    /// [`Reentry::call_until`]: crate::host::Reentry::call_until
    fn call_from_host<'s>(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        budget: &Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Result<Value, RuntimeError> {
        // Raised for as long as the callback runs and dropped when it
        // returns, so a host that runs its callback twice pays for one level
        // twice over rather than for two levels at once. What is bounded is
        // how many are stacked on this thread, because that is what is
        // spending the native stack.
        if self.reentry_depth >= crate::interp::MAX_REENTRY_DEPTH {
            return Err(crate::interp::reentry_too_deep(span));
        }
        let (target_id, object) =
            boundary::callback_target(self, callee).map_err(|error| error.at(span))?;
        let program = self.program;
        let target = program.function(target_id);
        if args.len() != target.params.len() {
            return Err(wrong_arity(target.qualified(), target.params.len(), args.len()).at(span));
        }

        // A callback's own frame is an ordinary Cove frame and counts as
        // one, which is the answer the oracle gives: the one frame a reentry
        // adds is enough to cross a limit the same recursion fits under when
        // it is called directly.
        self.admit_frame(budget, span)?;

        let floor = self.frames.len();
        let children = self.children.len();
        let scopes = self.scopes.len();
        let cells = self.held.len();
        let base = self
            .mem
            .push_frame(target.frame_size())
            .map_err(|Overflow| self.too_deep(span))?;
        // The frame is on the stack *before* an argument is converted, and
        // that is what roots the ones already converted: a `Repr::Ref` slot
        // of a live frame is a root, `boundary::from_value` allocates, and a
        // value built into a Rust vector first would have been named by
        // nothing the collector walks while the next one was built.
        self.frames.push(Frame {
            function: target_id,
            base,
            pc: 0,
            dst: 0,
        });
        let answer = self.enter_callback(
            object, target_id, args, base, floor, budget, threads, running, span,
        );
        match answer {
            Ok(words) => {
                // Nothing between here and the conversion allocates, so the
                // objects these words name are still the ones they named when
                // the frame that produced them was popped — the same reason
                // `Machine::run` may carry an answer out of a frame.
                let returns = self.program.function(target_id).returns;
                let answer =
                    boundary::to_value(self, returns, &words).map_err(|error| error.at(span))?;
                // The value has left, and its family goes with it: a host
                // that wraps this answer in a result it declared `Any` hands
                // it straight back, and the box that is built for it there is
                // tagged with what it was here. See
                // [`Machine::callback_answer`].
                self.callback_answer = Some(returns);
                Ok(answer)
            }
            Err(error) => {
                self.unwind_to(floor, children, scopes, cells, base, running);
                Err(error)
            }
        }
    }

    /// The callback's frame, filled and run. See [`Machine::call_from_host`].
    #[allow(clippy::too_many_arguments)]
    fn enter_callback<'s>(
        &mut self,
        object: u64,
        target_id: FunctionId,
        args: Vec<Value>,
        base: u64,
        floor: usize,
        budget: &Meter,
        threads: &'s Scope<'s, 'a>,
        running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
        span: Span,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let mut at = 0;
        for (value, layout) in args.iter().zip(&program.function(target_id).params) {
            let layout = *layout;
            let words = boundary::from_value(self, layout, value).map_err(|e| e.at(span))?;
            self.mem.write_words(base + at as u64, &words);
            at += self.width(layout);
        }
        // The captures, out of the environment object and into the slots
        // `Function::captures` names — the same read `Inst::CallClosure`
        // makes, of the same object, at the same widths.
        let mut held = 1;
        for capture in &program.function(target_id).captures {
            let width = self.width(capture.layout);
            self.mem.copy_words(
                base + capture.slot as u64,
                self.mem.payload_addr(object, held),
                width,
            );
            held += width;
        }
        self.reentry_depth += 1;
        let answer = self.dispatch(budget, threads, running, floor);
        self.reentry_depth -= 1;
        answer
    }

    /// Puts the frames, the stack region, the scopes and the tasks back the
    /// way a failed callback found them.
    ///
    /// The children first and while their frames still stand: cancelling and
    /// joining blocks, a task that blocks publishes its roots, and roots that
    /// named frames this had already truncated would be addresses of words
    /// nothing owns.
    fn unwind_to(
        &mut self,
        frames: usize,
        children: usize,
        scopes: usize,
        cells: usize,
        base: u64,
        running: &mut [Option<ScopedJoinHandle<'_, Outcome>>],
    ) {
        // Before anything is joined or truncated: a host that catches what a
        // callback failed with carries on, and a cell the callback took and
        // this did not give back would be held for the rest of the run by a
        // frame that no longer exists.
        self.give_cells_back(cells);
        for child in &self.children[children..] {
            if matches!(child.state, ChildState::Running) {
                child.cancellation.cancel();
            }
        }
        for at in children..self.children.len() {
            self.join(at, running);
        }
        // A scope the callback opened and did not leave is closed here rather
        // than at the end of the run, for the reason its children were joined
        // here: its threads would otherwise outlive every frame that could
        // name them.
        for scope in &mut self.scopes[scopes..] {
            scope.closed = true;
        }
        self.frames.truncate(frames);
        self.mem.pop_frame(base);
    }

    /// Whether `rooted` names an object of this run's memory.
    pub(crate) fn holds(&self, rooted: &Rooted) -> bool {
        self.mem.is_mine(rooted)
    }

    /// Makes the object at `addr` a root for as long as the answer lives.
    ///
    /// The one thing a `Value` crossing out of here can need that a frame
    /// cannot give it. See [`crate::vm::mem::Rooted`].
    pub(crate) fn pin(&self, addr: u64) -> Rooted {
        self.mem.pin(addr)
    }

    /// A new object of `layout` with header length `len`, collecting once if
    /// the first attempt does not fit.
    /// A new object of `layout` with header length `len`, collecting once if
    /// the first attempt does not fit.
    ///
    /// The payload is zeroed, so a reference field reads as null until it is
    /// written — which is what makes a half-built object safe to collect
    /// *through* once [`Machine::push_temp`] has made it safe to collect
    /// *around*.
    pub(crate) fn new_object(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        self.allocate(layout, len)
    }
}

/// The way back a host is offered while the linear-memory backend runs.
///
/// A host that was handed a Cove callback calls it through this. The callback
/// is an ordinary frame of this machine — [`Machine::call_from_host`] pushes
/// it exactly as [`Inst::CallClosure`] would — and the difference from a call
/// the loop made is only in who is waiting for it: a Rust frame rather than
/// an instruction. So it holds the machine mutably, which is what makes the
/// rest of [`Reentry`]'s contract true rather than merely stated. There can
/// be one of these per host call and it cannot be moved to another thread, so
/// a host cannot use its way back concurrently; it borrows the machine, so a
/// host cannot keep it; and every level of nesting is another one further
/// down the same native stack, which is what
/// [`crate::interp::MAX_REENTRY_DEPTH`] counts.
///
/// # What re-entry costs here, and why the bound is the oracle's
///
/// `docs/LINEAR_VM.md` says **a builtin never calls back into Cove**, and the
/// reason it gives is the property the loop exists to have: how deep a Cove
/// program may nest is decided by the reserved stack region and not by how
/// large a Rust frame the interpreter compiled to. A builtin that ran a
/// closure itself would put a Rust frame under every Cove frame the closure
/// made, so a `map` over a `map` over a `map` would be three Rust frames deep
/// before the program did anything — and a builtin has an alternative, which
/// is to be lowered to a loop in the IR.
///
/// A host callback is the other case, and it differs in both halves. The host
/// is *already* a Rust frame: it was reached through
/// `HostRegistry::dispatch`, which the machine called, and nothing about
/// lowering anything would remove it. And the reentry is the language's own —
/// ADR 0013 gives the host the resource and the `Reentry` contract gives it
/// the callback — so there is no loop to lower it to. `clock.timeout(500ms)
/// { .. }` cannot become a `CallClosure` in the caller's body, because what
/// decides whether the body runs at all, and what stops it, is on the host's
/// side.
///
/// So the rule holds where it was aimed and does not reach this. What it
/// leaves is that one thing is no longer bounded by the stack region: between
/// the callback's frame and the frame that called the host sit
/// `HostRegistry::dispatch`, however much native stack the host itself uses,
/// and one more turn of [`Machine::dispatch`]. That is exactly the situation
/// [`crate::interp::MAX_REENTRY_DEPTH`] was calibrated for, in the oracle,
/// where its documentation says the depth limit's promise *"holds for Cove
/// calling Cove and stops holding exactly where a third party controls the
/// multiplier"*. The sentence is true of this backend word for word — it is
/// true of it *more* narrowly, because Cove calling Cove costs no native
/// stack here at all — so the bound is the same bound and the refusal is the
/// oracle's own, from [`crate::interp::reentry_too_deep`].
///
/// [`Reentry`]: crate::host::Reentry
struct Back<'m, 's, 'a> {
    machine: &'m mut Machine<'a>,
    budget: &'m Meter,
    /// Where the host call that is running this was written, so a failure
    /// inside it points at the call rather than at nothing.
    span: Span,
    /// The thread scope a `spawn` inside a callback starts its children in.
    ///
    /// The *caller's*, not one of this call's own. A callback is a frame of
    /// this machine and its tasks are this machine's tasks: they go into
    /// [`Machine::children`] at indices `running` is parallel to, and a scope
    /// of this call's own would have made those two disagree — a task the
    /// outer level spawned would be at an index a nested `running` had no
    /// handle at. One scope per task, which is what [`Machine::drive`] opens,
    /// is also what bounds every thread of the task to the task.
    threads: &'s Scope<'s, 'a>,
    running: &'m mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    /// The safepoint the calling task stands at while the host runs.
    ///
    /// Taken here and dropped for exactly as long as a callback runs. A task
    /// inside a host call is not running Cove, so the roots it published stay
    /// true and a collection need not wait for it; a task running a callback
    /// *is* running Cove, its frames change between two instructions, and a
    /// snapshot left standing would be telling the collector to trace a frame
    /// that has moved.
    parked: Option<Parked>,
}

impl<'m, 's, 'a> Back<'m, 's, 'a> {
    /// The way back for one host call, with the calling task parked.
    fn parked(
        machine: &'m mut Machine<'a>,
        budget: &'m Meter,
        span: Span,
        threads: &'s Scope<'s, 'a>,
        running: &'m mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    ) -> Back<'m, 's, 'a> {
        let parked = machine.park();
        // This call has run no callback yet, so nothing it answers may be
        // tagged with the family an earlier one left behind.
        machine.callback_answer = None;
        Back {
            machine,
            budget,
            span,
            threads,
            running,
            parked: Some(parked),
        }
    }

    /// Runs `callee`, off the safepoint and back onto it.
    fn run(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
        drop(self.parked.take());
        let answer = self.machine.call_from_host(
            callee,
            args,
            self.budget,
            self.span,
            self.threads,
            self.running,
        );
        // Whatever the callback did, this task is inside a host call again
        // and the host may go on to wait, to call again, or to answer.
        self.parked = Some(self.machine.park());
        answer
    }
}

impl Reentry for Back<'_, '_, '_> {
    fn call(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.run(callee, args)
    }

    /// The same call, with `stop` added to what its safepoints stop on.
    ///
    /// The flag bounds this call *and everything inside it*, which is why it
    /// stands on the machine rather than being handed to the frame: a further
    /// host call the body makes, and any callback that host runs in turn, are
    /// reached through the same [`Machine::stops`].
    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        stop: &Cancellation,
    ) -> Result<Value, RuntimeError> {
        self.machine.stops.push(stop.clone());
        let result = self.run(callee, args);
        self.machine.stops.pop();
        result
    }

    /// Everything a safepoint would stop on, asked from outside the loop.
    ///
    /// A host that is waiting is standing where a safepoint would be, so it
    /// is owed the same answer one gets: the calling task's own flag, the
    /// flags of the bounded calls this thread is inside, and the run's
    /// cancellation. The middle one is what tells a host blocked inside a
    /// `clock.timeout` body that something is wrong, and it could not be
    /// answered until a callback could run here at all.
    fn is_cancelled(&self) -> bool {
        self.machine
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
            || self.machine.stops.iter().any(Cancellation::is_cancelled)
            || self.budget.is_cancelled()
    }

    fn time_left(&self) -> Option<Duration> {
        self.budget
            .limits()
            .deadline
            .map(|deadline| deadline.saturating_sub(self.budget.elapsed()))
    }

    /// The task whose stack this call is standing on, which is the task the
    /// boundary records the call against.
    fn task(&self) -> u64 {
        self.machine.task
    }
}

/// One spawned task's thread, from the closure in to the answer written.
///
/// `crate::interp::run_task` is the oracle's, and the shape is the same: an
/// evaluator of the receiving task's own, the body, and then the trace event
/// a finished task writes. What is not here is the conversion. ADR 0008 says
/// *"the runtime's `Rc`-based value representation is not `Send`, so the
/// values that cross must be converted at the boundary"* — and in this model
/// there is nothing to convert, because a crossing value is a run of words in
/// a heap both tasks already address. The closure crossed as its address, and
/// the answer goes back into an object the parent allocated.
///
/// A task stopped by its own cancellation did not run to completion, so it is
/// **not** traced as completed here; it is traced as cancelled by whoever
/// waits for it, which is the only place that knows it stopped rather than
/// finished. That is `crate::task::finished`'s rule, kept.
#[allow(clippy::too_many_arguments)]
fn run_task(
    program: &Program,
    hosts: Option<&HostRegistry>,
    runtime: Option<&Runtime>,
    resources: Arc<Mutex<Vec<Arc<ResourceHandle>>>>,
    segment: Memory,
    budget: Meter,
    cancellation: Cancellation,
    id: u64,
    closure: u64,
    answer: u64,
    span: Span,
) -> Outcome {
    let mut machine = Machine::for_task(
        program,
        hosts,
        runtime,
        resources,
        segment,
        cancellation.clone(),
        id,
    );
    let started = Instant::now();
    let result = machine.enter_closure(closure, &budget, span);
    if !(result.is_err() && cancellation.is_cancelled()) {
        if let Some(runtime) = runtime {
            runtime.trace(TraceEvent::TaskCompleted {
                id,
                // What the body spent rather than what the clock did: a task
                // that waited on a host was not working while it waited, and
                // a trace that could not tell the two apart is what ADR 0008
                // lists as the thing phase 1 could not validate.
                cpu: started.elapsed().saturating_sub(machine.host_wait()),
            });
        }
    }
    let words = result?;
    // Into the object the parent allocated, whose address it has held as a
    // root since before this thread existed. Nothing between here and the
    // last frame's `Return` allocates or reaches a safepoint, so no
    // collection can run in the window where these words are only in a Rust
    // `Vec`.
    for (at, word) in words.iter().enumerate() {
        machine.mem.set_payload(answer, 1 + at as u32, *word);
    }
    Ok(())
}

fn compare(op: CmpOp, ordering: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => ordering == Equal,
        CmpOp::Ne => ordering != Equal,
        CmpOp::Lt => ordering == Less,
        CmpOp::Le => ordering != Greater,
        CmpOp::Gt => ordering == Greater,
        CmpOp::Ge => ordering != Less,
    }
}

/// `Int` arithmetic, with the language's messages.
///
/// The messages are the interpreter's, word for word, because overflow and
/// division by zero are rules of the language rather than of a backend. The
/// differential corpus compares them.
fn int_arith(op: ArithOp, a: i64, b: i64, duration: bool) -> Result<i64, RuntimeError> {
    let named = |what: &'static str| -> &'static str {
        if duration {
            "duration arithmetic"
        } else {
            what
        }
    };
    match op {
        ArithOp::Add => a
            .checked_add(b)
            .ok_or_else(|| overflowed(named("addition"))),
        ArithOp::Sub => a
            .checked_sub(b)
            .ok_or_else(|| overflowed(named("subtraction"))),
        ArithOp::Mul => a
            .checked_mul(b)
            .ok_or_else(|| overflowed(named("multiplication"))),
        ArithOp::Div => {
            if b == 0 {
                Err(divided_by_zero("division"))
            } else {
                a.checked_div(b).ok_or_else(|| overflowed("division"))
            }
        }
        ArithOp::Rem => {
            if b == 0 {
                Err(divided_by_zero("remainder"))
            } else {
                a.checked_rem(b).ok_or_else(|| overflowed("remainder"))
            }
        }
    }
}

fn float_arith(op: ArithOp, a: f64, b: f64) -> f64 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

fn overflowed(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} overflowed"))
        .with_rule("Integer overflow is a broken invariant, not a wrapped result.")
}

fn divided_by_zero(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} by zero"))
        .with_rule("Division and remainder by zero are broken invariants.")
}

/// A call passed a number of arguments the callee does not declare.
///
/// The verifier checks it, so this is a lowering bug that got past it. It is
/// reported rather than assumed because the alternative is a callee whose
/// remaining parameters hold whatever the frame was zeroed with — which is a
/// silent wrong answer instead of a loud one.
fn wrong_arity(callee: String, declared: usize, given: usize) -> RuntimeError {
    RuntimeError::new(format!(
        "this call passes {given} argument(s) to `{callee}`, which declares {declared}"
    ))
}

/// A reference slot held null where an object was needed.
///
/// This is not a language-level `nil`: Cove has none. It is a lowering bug
/// reaching the machine, reported rather than read through.
fn null_object() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

/// A `lock` taken by a task that already holds the same cell.
///
/// The oracle's, word for word: `crate::shared::reentrant_lock` is where these
/// three sentences are written, and a program refused by one backend and not
/// the other in different words would be two languages. What
/// [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
/// removed is the *other* refusal `lock` used to make; this one it kept, and
/// gave the reason: locking the same cell twice from one task is a live lock
/// state, and no collector can answer one.
fn reentrant_lock() -> RuntimeError {
    RuntimeError::new("this task already holds this `Shared`, so `lock` would wait for itself")
        .with_rule(
            "`lock` holds the value for the whole of the closure it is given, so a `lock` on the same `Shared` inside it can never be granted.",
        )
        .with_help("do the whole read-modify-write in one `lock`")
}

/// A `Repr::Task` or `Repr::Scope` word that names no entry of this task's
/// scheduler table.
///
/// Zero is what a zeroed frame leaves in a slot nothing has written, which is
/// the same lowering bug a null reference is and earns the same refusal.
fn no_such_handle(what: &str) -> RuntimeError {
    RuntimeError::new(format!("this {what} was read before it was given one"))
}

/// A `spawn` this run has no stack segment left for.
///
/// The reserved stack region divides into a fixed number of segments and a
/// task owns one, so a run with more tasks executing at once than there are
/// segments has nowhere to put the next one's frames. It is reported as what
/// it is — a limit on how many tasks may run at once — rather than as a
/// second, differently worded ceiling standing beside the one
/// `[run.*] max_tasks` configures, because a program that hits either has hit
/// the same wall.
///
/// The tree-walking oracle has no equivalent: it puts a task's frames on a
/// thread's own stack and is bounded by what the operating system will give
/// it. Every corpus program stays far below both.
fn no_segment_left() -> RuntimeError {
    RuntimeError::new(
        "execution stopped: this run has no stack segment left for another task, so no more          tasks may run at once",
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cove_ir::{Arg, ArgsId, Capture, Function, Layout, RefMap, Table, TableId};
    use std::sync::Arc;

    /// Builds a program by hand.
    ///
    /// The lowering is a separate piece and a separate test suite. What is
    /// under test here is the machine, so its programs are written in the IR
    /// directly: a failure is then unambiguously the loop's, and a change to
    /// the lowering cannot quietly stop exercising an instruction.
    ///
    /// `pub(crate)` so that the boundary's and the builtins' tests write
    /// their fixtures the same way. A hand-written program is the only kind
    /// any of them uses, and having one builder is what keeps a fixture from
    /// being the thing under test.
    #[derive(Default)]
    pub(crate) struct Build {
        pub(crate) program: Program,
    }

    impl Build {
        pub(crate) fn strings(mut self, texts: &[&str]) -> Build {
            self.program.strings = texts.iter().map(|text| Arc::from(*text)).collect();
            self
        }

        /// A family that lives in the heap, so a value of it is one address.
        pub(crate) fn layout(&mut self, name: &str, shape: Shape) -> LayoutId {
            self.push(Layout::object(name, shape))
        }

        /// A one-word family: the width-one case of the whole model.
        pub(crate) fn word(&mut self, name: &str, repr: Repr) -> LayoutId {
            self.push(Layout::word(name, repr))
        }

        /// A struct, laid out inline from its fields' layouts.
        ///
        /// The offsets are computed by `cove_ir::struct_layout` rather than
        /// written out, because they are not a choice a fixture gets to make:
        /// a fixture free to say where a field is could agree with a machine
        /// that had it wrong.
        pub(crate) fn structure(&mut self, name: &str, fields: &[(&str, LayoutId)]) -> LayoutId {
            let named: Vec<(Arc<str>, LayoutId)> = fields
                .iter()
                .map(|(name, id)| (Arc::from(*name), *id))
                .collect();
            let (fields, words) = cove_ir::struct_layout(&named, &self.program.layouts);
            self.push(Layout::inline(
                name,
                Shape::Struct {
                    fields,
                    opaque: false,
                },
                words,
            ))
        }

        /// An `export opaque struct`, which renders as its bare name.
        pub(crate) fn opaque(&mut self, name: &str, fields: &[(&str, LayoutId)]) -> LayoutId {
            let id = self.structure(name, fields);
            if let Shape::Struct { opaque, .. } = &mut self.program.layouts[id.index()].shape {
                *opaque = true;
            }
            id
        }

        /// An enum, laid out under the payload-agreement rule.
        pub(crate) fn enumeration(
            &mut self,
            name: &str,
            cases: &[(&str, Vec<LayoutId>)],
        ) -> LayoutId {
            let named: Vec<(Arc<str>, Vec<LayoutId>)> = cases
                .iter()
                .map(|(name, parts)| (Arc::from(*name), parts.clone()))
                .collect();
            let (cases, payload) = cove_ir::enum_layout(&named, &self.program.layouts);
            let mut words = vec![Repr::Int];
            words.extend_from_slice(&payload);
            self.push(Layout::inline(name, Shape::Enum { cases, payload }, words))
        }

        /// The layout a `Box` allocates, reserved the way the lowering
        /// reserves it: a fixture that had to remember to declare one would
        /// be a fixture that could forget, and forgetting sizes the object
        /// by the wrong shape.
        pub(crate) fn boxed(&mut self) -> LayoutId {
            self.seed();
            self.program.boxed_layout
        }

        /// `LayoutId(0)` is the sweeper's free block and `LayoutId(1)` is the
        /// box, exactly as `cove_ir::lower` reserves them.
        fn seed(&mut self) {
            if self.program.layouts.is_empty() {
                self.program.layouts.push(Layout::free());
                self.program
                    .layouts
                    .push(Layout::object("Any", Shape::Boxed));
                self.program.boxed_layout = LayoutId(1);
            }
        }

        fn push(&mut self, layout: Layout) -> LayoutId {
            self.seed();
            self.program.layouts.push(layout);
            LayoutId(self.program.layouts.len() as u32 - 1)
        }

        /// An argument list: where each value is, and the layout that says
        /// how wide it is.
        pub(crate) fn args(&mut self, args: &[(Slot, LayoutId)]) -> ArgsId {
            self.program.args.push(
                args.iter()
                    .map(|(slot, layout)| Arg {
                        slot: *slot,
                        layout: *layout,
                    })
                    .collect(),
            );
            ArgsId(self.program.args.len() as u32 - 1)
        }

        pub(crate) fn table(&mut self, targets: &[u32], default: u32) -> TableId {
            self.program.tables.push(Table {
                targets: targets.to_vec(),
                default,
            });
            TableId(self.program.tables.len() as u32 - 1)
        }

        pub(crate) fn function(
            &mut self,
            name: &str,
            params: &[LayoutId],
            reprs: &[Repr],
            returns: LayoutId,
            code: Vec<Inst>,
        ) -> FunctionId {
            let nowhere = Span::new(cove_diag::FileId(0), 0, 0);
            let spans = vec![nowhere; code.len()];
            self.program.functions.push(Function {
                module: Arc::from("t"),
                name: Arc::from(name),
                params: params.to_vec(),
                reprs: reprs.to_vec(),
                refs: RefMap::of(reprs),
                returns,
                captures: Vec::<Capture>::new(),
                code,
                spans,
                span: nowhere,
                is_async: false,
            });
            let id = FunctionId(self.program.functions.len() as u32 - 1);
            self.program
                .by_name
                .insert((Arc::from("t"), Arc::from(name)), id);
            id
        }

        /// A function that reads captures: what a lowered lambda is.
        ///
        /// The slot each capture lands in is filled in here rather than
        /// written out per fixture, because it is not a choice a fixture gets
        /// to make: captures follow the parameters, so the first one begins
        /// where the parameters' words end and each one after it follows at
        /// its own width, and a fixture free to say otherwise could agree
        /// with a machine that had the rule wrong.
        pub(crate) fn lambda(
            &mut self,
            name: &str,
            params: &[LayoutId],
            reprs: &[Repr],
            returns: LayoutId,
            captures: &[LayoutId],
            code: Vec<Inst>,
        ) -> FunctionId {
            let mut slot: Slot = params
                .iter()
                .map(|id| self.program.layout(*id).width())
                .sum();
            let held: Vec<Capture> = captures
                .iter()
                .enumerate()
                .map(|(at, layout)| {
                    let capture = Capture {
                        name: Arc::from(format!("c{at}")),
                        slot,
                        layout: *layout,
                    };
                    slot += self.program.layout(*layout).width();
                    capture
                })
                .collect();
            let id = self.function(name, params, reprs, returns, code);
            self.program.functions[id.index()].captures = held;
            id
        }

        /// Checks the program the way the lowering must, so a malformed test
        /// fixture fails as a fixture rather than as a machine bug.
        pub(crate) fn done(self) -> Program {
            cove_ir::verify(&self.program).expect("a hand-written test program is well formed");
            self.program
        }

        /// A program of layouts and strings and no functions.
        ///
        /// What a boundary or a builtin test needs: both of them convert or
        /// read values rather than running code, and a function written only
        /// so that a program has one would be a fixture nothing reads.
        pub(crate) fn bare(mut self) -> Program {
            let str_layout = self.layout("String", Shape::Str);
            self.program.str_layout = str_layout;
            self.done()
        }

        /// The one-word layout of `repr`, declared once per fixture.
        pub(crate) fn scalar(&mut self, repr: Repr) -> LayoutId {
            if let Some(at) = self
                .program
                .layouts
                .iter()
                .position(|layout| layout.shape == Shape::Word(repr))
            {
                return LayoutId(at as u32);
            }
            self.word(repr.name(), repr)
        }
    }

    pub(crate) fn budget() -> Meter {
        crate::budget::Budget::new(crate::budget::Limits::default()).meter()
    }

    /// The words a run of `entry` answers.
    ///
    /// A function answers a *value location*, which is a run of words, so
    /// this is the general shape of a result and [`run`] is the common case
    /// of it. A fixture that answers a `Point` reads two words here rather
    /// than one address naming two words somewhere else.
    fn run_words(
        program: &Program,
        entry: FunctionId,
        args: &[u64],
    ) -> Result<Vec<u64>, RuntimeError> {
        Machine::new(program, 1 << 16).run(entry, args, &budget())
    }

    /// The one word a run of `entry` answers.
    ///
    /// Most of what is under test here is one word wide — an `Int`, a `Bool`,
    /// a reference — and writing `[0]` at every one of those call sites would
    /// put the same unchecked index in fifty places. The assertion is what
    /// keeps it honest: a fixture whose answer stopped being one word fails
    /// here rather than quietly reporting its first word.
    fn run(program: &Program, entry: FunctionId, args: &[u64]) -> Result<u64, RuntimeError> {
        let words = run_words(program, entry, args)?;
        assert_eq!(words.len(), 1, "this fixture answers one word");
        Ok(words[0])
    }

    #[test]
    fn a_constant_comes_back() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "answer",
            &[],
            &[Repr::Int],
            int,
            vec![Inst::Int { dst: 0, value: 42 }, Inst::Return { src: 0 }],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 42);
    }

    #[test]
    fn arithmetic_reads_and_writes_slots() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "add",
            &[int, int],
            &[Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[3, 4]).unwrap() as i64, 7);
    }

    /// The messages are the language's, not the backend's; the differential
    /// corpus compares them against the tree-walking oracle word for word.
    #[test]
    fn arithmetic_faults_say_what_the_oracle_says() {
        let cases: Vec<(ArithOp, i64, i64, &str)> = vec![
            (ArithOp::Div, 1, 0, "`Int` division by zero"),
            (ArithOp::Rem, 1, 0, "`Int` remainder by zero"),
            (ArithOp::Add, i64::MAX, 1, "`Int` addition overflowed"),
            (ArithOp::Mul, i64::MAX, 2, "`Int` multiplication overflowed"),
        ];
        for (op, a, b, message) in cases {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let f = build.function(
                "fault",
                &[int, int],
                &[Repr::Int, Repr::Int, Repr::Int],
                int,
                vec![
                    Inst::Arith {
                        num: Num::Int,
                        op,
                        dst: 2,
                        a: 0,
                        b: 1,
                    },
                    Inst::Return { src: 2 },
                ],
            );
            let program = build.done();
            let error = run(&program, f, &[a as u64, b as u64]).unwrap_err();
            assert_eq!(error.message, message);
        }
    }

    /// A `Duration` is nanoseconds and its arithmetic is an integer's, so the
    /// only thing that changes is what an overflow is called.
    #[test]
    fn a_duration_overflow_is_named_a_duration_overflow() {
        let mut build = Build::default();
        let duration = build.scalar(Repr::Duration);
        let f = build.function(
            "late",
            &[duration, duration],
            &[Repr::Duration, Repr::Duration, Repr::Duration],
            duration,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[i64::MAX as u64, 1]).unwrap_err();
        assert_eq!(error.message, "`Int` duration arithmetic overflowed");
    }

    #[test]
    fn a_branch_takes_one_side() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // fn abs(n) { if n < 0 { -n } else { n } }
        let f = build.function(
            "abs",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool],
            int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Neg {
                    num: Num::Int,
                    dst: 0,
                    a: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[(-5i64) as u64]).unwrap() as i64, 5);
        assert_eq!(run(&program, f, &[5]).unwrap() as i64, 5);
    }

    #[test]
    fn a_loop_runs_to_its_bound() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // fn sum(n) { var t = 0; var i = 0; while i < n { t = t + i; i = i + 1 }; t }
        let f = build.function(
            "sum",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 2,
                    b: 0,
                },
                Inst::BranchFalse { cond: 3, to: 8 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 2,
                    b: 4,
                },
                Inst::Jump { to: 2 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 45);
    }

    /// A call writes its arguments straight into the callee's slots and its
    /// answer straight into the caller's destination. There is no buffer
    /// between the two frames, and this is what says so.
    #[test]
    fn recursion_nests_frames_and_unwinds_them() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(3, int)]);
        // fn fact(n) { if n <= 1 { 1 } else { n * fact(n - 1) } }
        let f = build.function(
            "fact",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 1, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Le,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Return { src: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Sub,
                    dst: 3,
                    a: 0,
                    b: 1,
                },
                Inst::Call {
                    dst: 4,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 4,
                    a: 0,
                    b: 4,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 3_628_800);
    }

    /// Depth is bounded by the reserved stack region, not by the Rust stack:
    /// a call does not recurse in the dispatch loop, so this returns an error
    /// rather than ending the process.
    #[test]
    fn an_unbounded_recursion_is_stopped() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(0, int)]);
        let f = build.function(
            "forever",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Call {
                    dst: 1,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[0]).unwrap_err();
        assert_eq!(error.message, "this call nests too deeply");
    }

    // ---- values that are more than one word ------------------------------

    /// `docs/LINEAR_VM.md` §1, in the IR it writes out.
    ///
    /// ~~~cove
    /// struct Point { x: Int, y: Int }
    /// var a = Point(x: 1, y: 2)
    /// var b = a
    /// b.x = 7
    /// ~~~
    ///
    /// `a` is at slots 0–1 and `b` at 2–3, and `b = a` is one `Copy` of two
    /// words. `a.x` is slot 0 and nothing touched it — not because a bit said
    /// the copy was unshared, but because the copy put `b`'s words where `b`
    /// is. There is no sharing bit, no copy-on-write and no write path to
    /// unshare; `b.x = 7` writes slot 2 and that is all of it.
    ///
    /// The answer is the four slots read as one `Pair`, which is the same
    /// claim from the other side: a value location is a base slot and a
    /// layout, so two adjacent `Point`s *are* a four-word value.
    #[test]
    fn a_copy_is_the_words_of_the_value() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let pair = build.structure("Pair", &[("a", point), ("b", point)]);
        let f = build.function(
            "copy",
            &[],
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
            pair,
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 2 },
                Inst::Copy {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Int { dst: 4, value: 7 },
                // `b.x` is slot 2 + 0: a field of an inline struct is
                // arithmetic the lowering did, not an instruction.
                Inst::Copy {
                    dst: 2,
                    src: 4,
                    layout: int,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(program.layout(point).words, vec![Repr::Int, Repr::Int]);
        assert_eq!(run_words(&program, f, &[]).unwrap(), vec![1, 2, 7, 2]);
    }

    /// `docs/LINEAR_VM.md` §3: `struct Wrapper { p: Point, v: Vector<Int> }`
    /// is `[p.x: Int, p.y: Int, v: Ref]`, and a copy copies all three words.
    ///
    /// Two answers fall out of that one copy and neither needed a policy. The
    /// `Point` words become independent, so writing `b.p.x` leaves `a.p.x`
    /// alone. The `Vector` address is duplicated, so both wrappers name one
    /// vector — which is ADR 0001 verbatim, because a `Vector`'s storage is
    /// shared and mutable by the language's own rule rather than by anything
    /// the representation decided.
    #[test]
    fn a_copied_wrapper_separates_its_point_and_shares_its_vector() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let vector = build.layout("Vector", Shape::Vector { elem: int });
        let wrapper = build.structure("Wrapper", &[("p", point), ("v", vector)]);
        let both = build.structure("Both", &[("a", wrapper), ("b", wrapper)]);
        let f = build.function(
            "wrap",
            &[],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
            ],
            both,
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 2 },
                Inst::Alloc {
                    dst: 2,
                    layout: vector,
                    len: Len::Fixed,
                },
                Inst::Copy {
                    dst: 3,
                    src: 0,
                    layout: wrapper,
                },
                Inst::Int { dst: 6, value: 7 },
                Inst::Copy {
                    dst: 3,
                    src: 6,
                    layout: int,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(
            program.layout(wrapper).words,
            vec![Repr::Int, Repr::Int, Repr::Ref]
        );
        let words = run_words(&program, f, &[]).unwrap();
        assert_eq!(words.len(), 6);
        assert_eq!(&words[..2], &[1, 2], "`a`'s point is where `a` is");
        assert_eq!(&words[3..5], &[7, 2], "`b`'s point is where `b` is");
        assert_ne!(words[2], 0, "the vector was allocated");
        assert_eq!(words[2], words[5], "and both wrappers name that one vector");
    }

    /// `docs/LINEAR_VM.md` §5: a parameter takes the words its layout says,
    /// from slot 0 onward in declaration order, so a `(Int, Point, Int)` list
    /// occupies slots 0, 1–2 and 3. Nothing is permuted into type groups,
    /// because there are no type groups.
    ///
    /// The answer is a `Point` too, and `Return` copies the two words its
    /// `Function::returns` describes into the caller's destination location.
    /// Neither direction allocates: a struct crosses a call as its words.
    #[test]
    fn a_struct_is_passed_and_returned_as_its_words() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        // fn shift(n: Int, p: Point, m: Int) -> Point
        let shift = build.function(
            "shift",
            &[int, point, int],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            point,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 4,
                    a: 1,
                    b: 0,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 5,
                    a: 2,
                    b: 3,
                },
                Inst::Return { src: 4 },
            ],
        );
        let args = build.args(&[(0, int), (1, point), (3, int)]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            point,
            vec![
                Inst::Int { dst: 0, value: 10 },
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 2, value: 2 },
                Inst::Int { dst: 3, value: 20 },
                Inst::Call {
                    dst: 4,
                    callee: shift,
                    args,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        let target = program.function(shift);
        assert_eq!(target.param_slot(0, &program.layouts), 0);
        assert_eq!(target.param_slot(1, &program.layouts), 1);
        assert_eq!(target.param_slot(2, &program.layouts), 3);
        assert_eq!(target.param_words(&program.layouts), 4);
        assert_eq!(run_words(&program, main, &[]).unwrap(), vec![11, 22]);
    }

    /// An `Array<Point>` is a run of two-word elements rather than a run of
    /// addresses, and the stride an element instruction uses is the element
    /// layout's width.
    ///
    /// The header's `len` counts *elements*, so an index is checked against
    /// three and then multiplied — which is why writing element 1 through an
    /// `AddrOfElem` leaves element 2 alone rather than smearing across it,
    /// and why index 3 is refused although the object holds six words.
    #[test]
    fn an_array_of_points_is_walked_at_a_two_word_stride() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let two = build.structure("Two", &[("a", point), ("b", point)]);
        let reprs = &[
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Addr,
        ];
        let walk = build.function(
            "walk",
            &[],
            reprs,
            two,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: points,
                    len: Len::Count(3),
                },
                // xs[0] = Point(1, 2)
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 1 },
                Inst::Int { dst: 3, value: 2 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // xs[1] = Point(3, 4)
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 2, value: 3 },
                Inst::Int { dst: 3, value: 4 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // xs[2] = Point(5, 6)
                Inst::Int { dst: 1, value: 2 },
                Inst::Int { dst: 2, value: 5 },
                Inst::Int { dst: 3, value: 6 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // A place naming element 1, written through: two words at
                // one address, with nothing between the address and them.
                Inst::Int { dst: 1, value: 1 },
                Inst::AddrOfElem {
                    dst: 8,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Int { dst: 2, value: 30 },
                Inst::Int { dst: 3, value: 40 },
                Inst::Store {
                    addr: 8,
                    src: 2,
                    layout: point,
                },
                Inst::LoadElem {
                    dst: 4,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Int { dst: 1, value: 2 },
                Inst::LoadElem {
                    dst: 6,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Return { src: 4 },
            ],
        );
        let past = build.function(
            "past",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: points,
                    len: Len::Count(3),
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::LoadElem {
                    dst: 2,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(
            run_words(&program, walk, &[]).unwrap(),
            vec![30, 40, 5, 6],
            "the write through element 1 left element 2 where it was"
        );
        let error = run(&program, past, &[]).unwrap_err();
        assert_eq!(error.message, "index 3 is outside a collection of 3");
    }

    /// An enum is a discriminant word and a payload region wide enough for
    /// every case, and the offsets are assigned so that **every case using a
    /// payload word agrees on its `Repr`**.
    ///
    /// `enum Msg { Text(Cell), Count(Int) }` therefore lays out as
    /// `[disc: Int, Ref, Int]`: `Count`'s `Int` cannot share `Text`'s
    /// reference word, so it takes a third. Two things follow, and this is
    /// both of them. Constructing a case zeroes the region it does not fill,
    /// so `Count`'s reference word reads null rather than whatever `Text`
    /// left there. And the collector never reads the discriminant to decide
    /// what to trace — the region's map is static, which is one fewer thing
    /// that can be wrong.
    ///
    /// The heap holds one cell and not two, so the second allocation is the
    /// question: it succeeds when the value was rebuilt as `Count`, because
    /// the word naming the first cell was zeroed and nothing reaches it, and
    /// it fails when the value is still `Text`, because that same word is
    /// traced and the cell is live.
    #[test]
    fn an_enums_payload_is_retained_by_its_static_map() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let msg = build.enumeration("Msg", &[("Text", vec![cell]), ("Count", vec![int])]);
        let boolean = build.scalar(Repr::Bool);
        let f = build.function(
            "held",
            &[boolean],
            &[
                Repr::Bool,
                // The `Msg` is at slots 1–3: a discriminant, `Text`'s
                // reference and `Count`'s integer.
                Repr::Int,
                Repr::Ref,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
            ],
            int,
            vec![
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(1200),
                },
                Inst::Int { dst: 1, value: 0 },
                Inst::Copy {
                    dst: 2,
                    src: 4,
                    layout: cell,
                },
                // The enum's payload word is now the only name for the cell.
                Inst::Clear {
                    slot: 4,
                    layout: cell,
                },
                Inst::BranchFalse { cond: 0, to: 8 },
                // Constructing `Count` zeroes the region it does not fill,
                // which is what leaves `Text`'s reference word null.
                Inst::Clear {
                    slot: 1,
                    layout: msg,
                },
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 3, value: 5 },
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(1200),
                },
                Inst::Int { dst: 5, value: 7 },
                Inst::Return { src: 5 },
            ],
        );
        let program = build.done();
        assert_eq!(
            program.layout(msg).words,
            vec![Repr::Int, Repr::Ref, Repr::Int]
        );

        let mut kept = Machine::new(&program, 2048);
        let error = kept.run(f, &[0], &budget()).unwrap_err();
        assert_eq!(error.message, "this run has no memory left");

        let mut dropped = Machine::new(&program, 2048);
        assert_eq!(dropped.run(f, &[1], &budget()).unwrap(), vec![7]);
        assert!(
            dropped.collected().collections > 0,
            "the second cell only fits after the first is reclaimed"
        );
    }

    /// A frame's map is a function of its `Repr`s, and a multiword value
    /// contributes its flattened per-word ones.
    ///
    /// `docs/LINEAR_VM.md` §6: a `Wrapper { p: Point, v: Vector }` at slot 5
    /// contributes `Int, Int, Ref`, so slot 7 is a root and 5 and 6 are not.
    /// Nothing about the value's *width* reaches the collector — it reads one
    /// bit per slot, as it did when every value was one word, and a wide
    /// value is simply several slots' worth of bits.
    ///
    /// The other half is that a slot the map does not name cannot hold a
    /// reference at all: the verifier holds every instruction to the `Repr`
    /// of the slot it names, so a program that put an address in slot 6 is
    /// not a program. That is what makes one static bit per slot sound, and
    /// it is why the dynamic half of this test reaches for `Clear` instead —
    /// a reference slot the map *does* name, emptied at its last use.
    #[test]
    fn a_frames_map_covers_a_multiword_value_word_by_word() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let wrapper = build.structure("Wrapper", &[("p", point), ("v", cell)]);
        assert_eq!(
            build.program.layout(wrapper).words,
            vec![Repr::Int, Repr::Int, Repr::Ref],
            "three words: the `Point` inline, and the cell's address"
        );

        // A `Wrapper` at slots 5-7, a scratch reference at slot 8, and the
        // answer at slot 9.
        let reprs = vec![
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Ref,
            Repr::Int,
        ];
        let f = build.function(
            "wrapper",
            &[],
            &reprs,
            int,
            vec![
                // The wrapper's `v`, which its own slot keeps alive.
                Inst::Alloc {
                    dst: 7,
                    layout: cell,
                    len: Len::Count(600),
                },
                // A second cell, named by a reference slot that is then
                // cleared — so the map still reads slot 8, and reads null.
                Inst::Alloc {
                    dst: 8,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::Clear {
                    slot: 8,
                    layout: cell,
                },
                Inst::Int { dst: 5, value: 1 },
                Inst::Int { dst: 6, value: 2 },
                // A third cell fits only if the second was reclaimed, and the
                // first must survive to be written through afterwards.
                Inst::Alloc {
                    dst: 8,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::Int { dst: 9, value: 0 },
                Inst::Int {
                    dst: 4,
                    value: 4242,
                },
                Inst::StoreElem {
                    obj: 7,
                    index: 9,
                    src: 4,
                    layout: int,
                },
                Inst::LoadElem {
                    dst: 9,
                    obj: 7,
                    index: 9,
                    layout: int,
                },
                Inst::Return { src: 9 },
            ],
        );
        let program = build.done();

        // The static half, which is the claim `docs/LINEAR_VM.md` makes.
        let refs = &program.function(f).refs;
        assert!(!refs.is_ref(5), "the `Point`'s x is not a root");
        assert!(!refs.is_ref(6), "the `Point`'s y is not a root");
        assert!(refs.is_ref(7), "the vector's address is");
        assert_eq!(refs.iter().collect::<Vec<_>>(), vec![7, 8]);

        // The dynamic half: two cells fit at a time and three do not, so the
        // run only finishes because the cleared slot stopped being a root —
        // and it finishes with the wrapper's own cell still there to write.
        let mut machine = Machine::new(&program, 1600);
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![4242]);
        assert!(
            machine.collected().collections > 0,
            "the third cell only fits after the cleared one is reclaimed"
        );
    }

    // ---- closures ------------------------------------------------------

    /// The layout of a lambda that reads `captures`.
    fn closure_layout(build: &mut Build, function: FunctionId, captures: &[LayoutId]) -> LayoutId {
        build.layout(
            "closure",
            Shape::Closure {
                function,
                captures: captures.to_vec(),
            },
        )
    }

    /// A closure's frame is a callee's frame with two writes rather than one:
    /// the arguments into the words the parameters occupy, and then the
    /// captures into the slots `Function::captures` names, which are the ones
    /// straight after.
    #[test]
    fn a_closure_call_copies_the_arguments_then_the_captures() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // { it -> it + captured }
        let add = build.lambda(
            "lambda",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Int],
            int,
            &[int],
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let layout = closure_layout(&mut build, add, &[int]);
        let args = build.args(&[(3, int)]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: add.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int { dst: 2, value: 10 },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 2,
                    layout: int,
                },
                Inst::Int { dst: 3, value: 5 },
                Inst::CallClosure {
                    dst: 4,
                    closure: 0,
                    args,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 15);
    }

    /// A capture is copied into a `Repr::Ref` slot of the callee's frame, so
    /// it is a root of that frame like any other — which is what makes a
    /// closure need no second story for the collector.
    ///
    /// The captured object is reachable from nowhere else by the time the call
    /// happens: the caller cleared its own slot, and it is not a string, so the
    /// interned table is not quietly holding it either. The body then allocates
    /// until the heap has to be swept several times over before reading the
    /// capture back.
    #[test]
    fn a_capture_survives_a_collection_in_the_callee() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let body = build.lambda(
            "lambda",
            &[],
            &[
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Bool,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            int,
            &[cell],
            vec![
                Inst::Int { dst: 2, value: 300 },
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 1,
                    b: 2,
                },
                Inst::BranchFalse { cond: 3, to: 9 },
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear {
                    slot: 4,
                    layout: cell,
                },
                Inst::Int { dst: 5, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 5,
                },
                Inst::Jump { to: 2 },
                Inst::Int { dst: 6, value: 0 },
                Inst::LoadElem {
                    dst: 7,
                    obj: 0,
                    index: 6,
                    layout: int,
                },
                Inst::Return { src: 7 },
            ],
        );
        let layout = closure_layout(&mut build, body, &[cell]);
        let none = build.args(&[]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            int,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: cell,
                    len: Len::Count(1),
                },
                Inst::Int { dst: 3, value: 0 },
                Inst::Int {
                    dst: 4,
                    value: 4242,
                },
                Inst::StoreElem {
                    obj: 1,
                    index: 3,
                    src: 4,
                    layout: int,
                },
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 2,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 2,
                    layout: int,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 1,
                    layout: cell,
                },
                Inst::Clear {
                    slot: 1,
                    layout: cell,
                },
                Inst::CallClosure {
                    dst: 5,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 5 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 4096);
        assert_eq!(machine.run(main, &[], &budget()).unwrap(), vec![4242]);
        assert!(
            machine.collected().collections > 0,
            "the body is meant to allocate more than the heap holds"
        );
    }

    /// A closure that calls itself through its own capture nests until the
    /// reserved stack region is full, and stops there — with the message any
    /// other unbounded recursion gets, because it is the same event. No Rust
    /// frame is added per turn, so how deep this goes is `STACK_WORDS` and
    /// nothing else.
    #[test]
    fn a_closure_chain_is_bounded_by_the_stack_region() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // What the closure captures is the closure, so the capture's layout
        // is one reference word rather than the callee's own `Int`.
        let held = build.word("captured", Repr::Ref);
        let none = build.args(&[]);
        // What it answers is never reached, because it never returns.
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Ref, Repr::Int],
            int,
            &[held],
            vec![
                Inst::CallClosure {
                    dst: 1,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 1 },
            ],
        );
        let layout = closure_layout(&mut build, body, &[held]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                // The closure captures itself, which is the shortest way to
                // write a call chain with no bound on it.
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 0,
                    layout: held,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(error.message, "this call nests too deeply");
    }

    /// `fn main() { spin() }`, where `spin` is a closure whose body never
    /// leaves its loop.
    ///
    /// The caller is four instructions and the fifth enters the closure, so
    /// every safepoint after the first handful is one the closure's own frame
    /// is executing at.
    fn spinning_closure(build: &mut Build) -> FunctionId {
        let int = build.scalar(Repr::Int);
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Int],
            int,
            &[],
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let layout = closure_layout(build, body, &[]);
        let none = build.args(&[]);
        build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        )
    }

    /// The safepoint is a fact about the loop, not about which frame the loop
    /// is in: a run spinning inside a closure is cancelled within one stride
    /// exactly as one spinning in its entry is.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint_inside_a_closure() {
        let mut build = Build::default();
        let main = spinning_closure(&mut build);
        let program = build.done();
        let cancellation = Cancellation::new();
        let budget = crate::budget::Budget::with_cancellation(
            crate::budget::Limits::default(),
            cancellation.clone(),
        );
        cancellation.cancel();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget.meter()).unwrap_err();
        assert_eq!(error.message, "execution stopped: the run was cancelled");
        assert!(machine.instructions() <= SAFEPOINT_STRIDE + 1);
    }

    /// And fuel is charged at the same points, so a closure cannot spend a
    /// run's budget without the run noticing.
    #[test]
    fn fuel_runs_out_at_a_safepoint_inside_a_closure() {
        let mut build = Build::default();
        let main = spinning_closure(&mut build);
        let program = build.done();
        let budget = crate::budget::Budget::new(crate::budget::Limits {
            fuel: Some(2 * SAFEPOINT_STRIDE),
            ..Default::default()
        });
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget.meter()).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "execution stopped: fuel budget of {} exhausted",
                2 * SAFEPOINT_STRIDE
            )
        );
        assert_eq!(machine.instructions(), 2 * SAFEPOINT_STRIDE);
    }

    /// The callee comes out of a heap object, so the machine checks that the
    /// object is one a call can be made through rather than reading its first
    /// word as a function id. Nothing a program can write reaches this; a
    /// lowering that did would otherwise push a frame for whichever function
    /// the object's first word happened to name.
    #[test]
    fn a_call_through_something_that_is_not_a_closure_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int)]);
        let none = build.args(&[]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::CallClosure {
                    dst: 1,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(error.message, "`Point` is not callable");
    }

    /// A closure object whose captures are not the ones its callee reads is a
    /// lowering bug, and copying what it holds would fill the callee's capture
    /// slots from whatever follows the object in the heap.
    #[test]
    fn a_closure_whose_captures_do_not_match_its_callee_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Int, Repr::Int],
            int,
            &[int, int],
            vec![Inst::Return { src: 0 }],
        );
        // One capture, against a callee that reads two.
        let layout = closure_layout(&mut build, body, &[int]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(
            error.message,
            "this closure and `t.lambda` disagree about its captures: 1 held, 2 read"
        );
    }

    /// `bump(var total)` adds to the caller's own binding rather than to a
    /// copy of it. A place is one word holding the address of that binding,
    /// and this is the whole of the mechanism.
    #[test]
    fn a_place_writes_the_callers_own_slot() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let unit = build.scalar(Repr::Unit);
        let place = build.scalar(Repr::Addr);
        let args = build.args(&[(1, place)]);
        let bump = build.function(
            "bump",
            &[place],
            &[Repr::Addr, Repr::Int, Repr::Int, Repr::Unit],
            unit,
            vec![
                Inst::Load {
                    dst: 1,
                    addr: 0,
                    layout: int,
                },
                Inst::Int { dst: 2, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Store {
                    addr: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Unit { dst: 3 },
                Inst::Return { src: 3 },
            ],
        );
        let caller = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Addr, Repr::Unit],
            int,
            vec![
                Inst::Int { dst: 0, value: 10 },
                Inst::AddrOfSlot { dst: 1, slot: 0 },
                Inst::Call {
                    dst: 2,
                    callee: bump,
                    args,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, caller, &[]).unwrap() as i64, 11);
    }

    /// A field of a *heap object* is a load and a store; a field of an inline
    /// struct is not an instruction at all. This is the first kind, which is
    /// what a struct reaches by being the payload of an object.
    #[test]
    fn an_object_round_trips_through_its_fields() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let f = build.function(
            "make",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int { dst: 1, value: 4 },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 1,
                    layout: int,
                },
                Inst::LoadField {
                    dst: 1,
                    obj: 0,
                    at: 0,
                    layout: int,
                },
                Inst::LoadField {
                    dst: 2,
                    obj: 0,
                    at: 1,
                    layout: int,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 12);
    }

    /// Reading past an object is a lowering bug, and the machine reports it
    /// rather than reading whatever follows the object in the heap.
    ///
    /// The object reaches the slot it is read out of through a `Copy`, which
    /// is what leaves the machine to answer: `cove_ir::verify` refuses this
    /// statically wherever it can prove which layout a reference slot holds,
    /// and a slot written by a copy holds whatever the source held. Both
    /// checks are wanted — the static one catches the bug at lowering time,
    /// and this one catches it where the layout is not a static fact.
    #[test]
    fn a_field_past_the_object_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let one = build.structure("One", &[("x", int)]);
        let held = build.layout("Held", Shape::Word(Repr::Ref));
        let f = build.function(
            "past",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Ref],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: one,
                    len: Len::Fixed,
                },
                Inst::Copy {
                    dst: 2,
                    src: 0,
                    layout: held,
                },
                Inst::LoadField {
                    dst: 1,
                    obj: 2,
                    at: 3,
                    layout: int,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[]).unwrap_err();
        assert!(
            error.message.contains("word 3 of a `One`"),
            "{}",
            error.message
        );
    }

    /// The loop allocates in a loop, clearing the slot each turn. Without
    /// `Clear` the frame would hold every object it ever made; with it the
    /// heap stays flat, and this is the test that says so.
    #[test]
    fn clearing_a_slot_lets_the_collector_reclaim() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let f = build.function(
            "churn",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 1,
                    b: 0,
                },
                Inst::BranchFalse { cond: 2, to: 8 },
                Inst::Alloc {
                    dst: 3,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear {
                    slot: 3,
                    layout: cell,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 4,
                },
                Inst::Jump { to: 1 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        // A heap far smaller than 4000 objects of 65 words: the run only
        // finishes because each turn's object is unreachable by the next.
        let mut machine = Machine::new(&program, 4096);
        let answer = machine.run(f, &[4000], &budget()).unwrap();
        assert_eq!(answer, vec![4000]);
        assert!(
            machine.collected().collections > 0,
            "the run should have had to collect"
        );
    }

    #[test]
    fn a_string_literal_is_allocated_once() {
        let mut build = Build::default().strings(&["hello"]);
        let bool_layout = build.scalar(Repr::Bool);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "twice",
            &[],
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            bool_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(0),
                },
                Inst::Cmp {
                    on: Compare::Identity,
                    op: CmpOp::Eq,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    #[test]
    fn strings_compare_by_their_bytes() {
        let mut build = Build::default().strings(&["apple", "banana"]);
        let bool_layout = build.scalar(Repr::Bool);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "order",
            &[],
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            bool_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(1),
                },
                Inst::Cmp {
                    on: Compare::Str,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    /// A box carries the *layout* of what it holds in payload word 0, not a
    /// per-word `Repr`: erasure is where a value stops having a static width,
    /// so what the box has to record is the thing that says the width.
    #[test]
    fn a_box_answers_the_layout_it_holds_and_refuses_another() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let boolean = build.scalar(Repr::Bool);
        build.boxed();
        let good = build.function(
            "round-trip",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    layout: int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    layout: int,
                },
                Inst::Return { src: 2 },
            ],
        );
        let wrong = build.function(
            "wrong-type",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Bool],
            boolean,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    layout: int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    layout: boolean,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, good, &[7]).unwrap() as i64, 7);
        assert_eq!(
            run(&program, wrong, &[7]).unwrap_err().message,
            "this value is not of the type it is being read as"
        );
    }

    /// A boxed `Point` is a two-word payload rather than a reference to
    /// somewhere else again: the object holds the `LayoutId` in payload word
    /// 0 and the value's words after it, and the header's `len` is that
    /// value's width, because a `Boxed` layout cannot know it.
    ///
    /// So an `Unbox` at the wrong layout is refused for the same reason it is
    /// on a scalar — the word the box carries is a layout and the layouts do
    /// not match — and nothing about the width had to be guessed.
    #[test]
    fn a_box_holds_a_multiword_value_inline() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        build.boxed();
        let round_trip = build.function(
            "round-trip",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            point,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Unbox {
                    dst: 3,
                    src: 2,
                    layout: point,
                },
                Inst::Return { src: 3 },
            ],
        );
        let width = build.function(
            "width",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Len { dst: 3, obj: 2 },
                Inst::Return { src: 3 },
            ],
        );
        let wrong = build.function(
            "wrong-layout",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Unbox {
                    dst: 3,
                    src: 2,
                    layout: int,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();
        assert_eq!(run_words(&program, round_trip, &[]).unwrap(), vec![3, 4]);
        assert_eq!(run(&program, width, &[]).unwrap(), 2);
        assert_eq!(
            run(&program, wrong, &[]).unwrap_err().message,
            "this value is not of the type it is being read as"
        );
    }

    #[test]
    fn a_switch_picks_a_case_and_falls_to_its_default() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let table = build.table(&[3, 5], 7);
        let f = build.function(
            "pick",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Switch { on: 0, table },
                Inst::Int { dst: 1, value: 0 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 10 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 20 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 30 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[0]).unwrap() as i64, 10);
        assert_eq!(run(&program, f, &[1]).unwrap() as i64, 20);
        assert_eq!(run(&program, f, &[9]).unwrap() as i64, 30);
    }

    // ---- the host boundary -------------------------------------------

    /// A host with one operation of each kind of argument the boundary has
    /// to move: a scalar in and out, and a string in and out.
    ///
    /// Written here rather than reused from a shipped module because what is
    /// under test is the *instruction*: `console.println` would drag in a
    /// grant table, an output stream and a schema written for a different
    /// purpose, and a failure would take a paragraph to attribute.
    struct Probe;

    static PROBE_OPS: &[cove_schema::OperationSchema] = &[
        cove_schema::OperationSchema {
            name: "double",
            params: &[cove_schema::HostType::Int],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "probe",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "shout",
            params: &[cove_schema::HostType::String],
            variadic: false,
            result: cove_schema::HostType::String,
            capability: "probe",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ];

    impl crate::host::HostApi for Probe {
        fn module_schema(&self) -> cove_schema::ModuleSchema {
            cove_schema::ModuleSchema {
                name: "probe",
                capability: "probe",
                operations: PROBE_OPS,
                types: &[],
                resources: &[],
            }
        }

        fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
            match op {
                "double" => Ok(Value::int(
                    args[0].as_int().expect("the schema holds it") * 2,
                )),
                "shout" => Ok(Value::string(format!(
                    "{}!",
                    args[0].as_str().expect("the schema holds it")
                ))),
                other => Err(RuntimeError::new(format!("no `{other}` here"))),
            }
        }
    }

    fn probing(granted: bool) -> crate::host::HostRegistry {
        let grants = if granted {
            crate::host::Grants::new(["probe"])
        } else {
            crate::host::Grants::new(Vec::<String>::new())
        };
        let mut hosts = crate::host::HostRegistry::new(grants);
        hosts.register(Box::new(Probe));
        hosts
    }

    /// `fn f(n) { probe.double(n) }`, in the IR.
    fn calls_double(build: &mut Build) -> FunctionId {
        let int = build.scalar(Repr::Int);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: int,
        });
        let op = cove_ir::HostOpId(build.program.host_ops.len() as u32 - 1);
        let args = build.args(&[(0, int)]);
        build.function(
            "f",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
            vec![Inst::CallHost { dst: 1, op, args }, Inst::Return { src: 1 }],
        )
    }

    #[test]
    fn a_host_call_moves_a_word_out_and_the_answer_back() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let hosts = probing(true);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        assert_eq!(machine.run(f, &[21], &budget()).unwrap(), vec![42]);
    }

    /// A string argument and a string answer, which is the case that
    /// allocates on both sides of the boundary.
    #[test]
    fn a_host_call_carries_strings_in_and_out() {
        let mut build = Build::default().strings(&["hey"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("shout"),
            result: str_layout,
        });
        let op = cove_ir::HostOpId(0);
        let args = build.args(&[(0, str_layout)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref, Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::CallHost { dst: 1, op, args },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let hosts = probing(true);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let words = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(words[0])).unwrap(),
            "hey!"
        );
    }

    /// The boundary refuses an ungranted capability, and it is the boundary
    /// that does it: the machine passes the call on and reports what came
    /// back, classification included.
    #[test]
    fn an_ungranted_call_is_refused_at_the_boundary() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let hosts = probing(false);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let error = machine.run(f, &[1], &budget()).unwrap_err();
        assert!(
            error.message.contains("probe"),
            "the refusal names the capability: {}",
            error.message
        );
        assert_eq!(error.denied_capability.as_deref(), Some("probe"));
        assert_eq!(error.outcome, crate::trace::RunOutcome::HostBoundary);
    }

    /// The host-call limit is charged inside the boundary, which is where the
    /// oracle charges it too — `Budget::charge_host_call`, once per call,
    /// before the host is reached.
    #[test]
    fn a_host_call_is_charged_the_way_the_oracle_charges_it() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: int,
        });
        let op = cove_ir::HostOpId(0);
        let args = build.args(&[(0, int)]);
        let f = build.function(
            "f",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
            vec![
                Inst::CallHost { dst: 1, op, args },
                Inst::CallHost { dst: 1, op, args },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let mut hosts = probing(true);
        let limits = crate::budget::Limits {
            max_host_calls: Some(1),
            ..Default::default()
        };
        let budget = crate::budget::Budget::new(limits);
        let meter = budget.meter();
        hosts.set_budget(budget);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let error = machine.run(f, &[2], &meter).unwrap_err();
        assert_eq!(
            error.message,
            "execution stopped: host-call limit of 1 exceeded"
        );
        // Two, not one: the boundary counts the call it is about to make
        // and then refuses it for being past the limit. That is the shared
        // counter doing what it does for every backend, which is the point —
        // nothing here keeps a count of its own.
        assert_eq!(hosts.with_budget(|budget| budget.host_calls()), Some(2));
    }

    /// A machine with no host behind it says what is missing rather than
    /// answering as if the call had happened.
    #[test]
    fn a_host_call_with_no_boundary_says_what_is_missing() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(f, &[1], &budget()).unwrap_err();
        assert_eq!(
            error.message,
            "`probe.double` cannot be called, because this run has no host boundary"
        );
    }

    // ---- host resources ------------------------------------------------

    /// A host that issues a resource and takes one back.
    ///
    /// The two directions a `Repr::Host` word has to move, and nothing else:
    /// `open` answers a handle the way `files.open(path)` answers a
    /// `files.Reader`, and `read` is handed one back the way
    /// `files.read(reader)` is. It counts what it has opened, so two readers
    /// are two resources and `read` answering the id says which one arrived.
    #[derive(Default)]
    struct Vault {
        opened: std::sync::atomic::AtomicU64,
    }

    static VAULT_RESOURCES: &[cove_schema::ResourceSchema] = &[cove_schema::ResourceSchema {
        name: "Reader",
        task_safe: true,
        operations: &[],
    }];

    static VAULT_OPS: &[cove_schema::OperationSchema] = &[
        cove_schema::OperationSchema {
            name: "open",
            params: &[],
            variadic: false,
            result: cove_schema::HostType::Named("vault.Reader"),
            capability: "vault",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "read",
            params: &[cove_schema::HostType::Named("vault.Reader")],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "vault",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ];

    impl crate::host::HostApi for Vault {
        fn module_schema(&self) -> cove_schema::ModuleSchema {
            cove_schema::ModuleSchema {
                name: "vault",
                capability: "vault",
                operations: VAULT_OPS,
                types: &[],
                resources: VAULT_RESOURCES,
            }
        }

        fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
            match op {
                // Counting upward and never reusing, which is the rule
                // ADR 0013 puts on an identity.
                "open" => {
                    let id = self
                        .opened
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    Ok(Value::from_resource(ResourceHandle::new(
                        "vault",
                        &VAULT_RESOURCES[0],
                        id,
                    )))
                }
                // The host recognises its own resource, which is the whole
                // point of the name crossing back: nothing about the word
                // reached here.
                "read" => Ok(Value::int(
                    args[0].resource().expect("the schema holds it").id as i64,
                )),
                other => Err(RuntimeError::new(format!("no `{other}` here"))),
            }
        }
    }

    fn vault() -> crate::host::HostRegistry {
        let mut hosts = crate::host::HostRegistry::new(crate::host::Grants::new(["vault"]));
        hosts.register(Box::new(Vault::default()));
        hosts
    }

    /// The two host operations, and the one-word family a handle occupies.
    fn vault_ops(build: &mut Build) -> (LayoutId, cove_ir::HostOpId, cove_ir::HostOpId) {
        let int = build.scalar(Repr::Int);
        let reader = build.word("vault.Reader", Repr::Host);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("vault"),
            operation: Arc::from("open"),
            result: reader,
        });
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("vault"),
            operation: Arc::from("read"),
            result: int,
        });
        let ops = build.program.host_ops.len() as u32;
        (
            reader,
            cove_ir::HostOpId(ops - 2),
            cove_ir::HostOpId(ops - 1),
        )
    }

    /// A host operation whose result is a resource writes the word, not a
    /// boxed value: the answer is a value location of one `Repr::Host` slot,
    /// and `Inst::CallHost` copies its words into the frame exactly as it
    /// does for an `Int`. Nothing about the instruction knows a resource is
    /// different.
    #[test]
    fn a_host_call_answering_a_resource_writes_the_word() {
        let mut build = Build::default();
        let (reader, open, _) = vault_ops(&mut build);
        let args = build.args(&[]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host],
            reader,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));

        let answer = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            answer.len(),
            1,
            "a handle is a name, and a name is one word"
        );
        assert_eq!(
            machine
                .resource(answer[0])
                .map(|handle| handle.to_string())
                .as_deref(),
            Some("vault.Reader#1"),
            "the word indexes the run's table, and the table holds the name"
        );
        assert_eq!(
            machine.allocated_words(),
            0,
            "a resource is not an object, so nothing was allocated to hold one"
        );
    }

    /// A resource goes back to the host that issued it, by the name it was
    /// issued under. The host is what recognises it; the word never left.
    #[test]
    fn a_resource_goes_back_to_the_host_that_issued_it() {
        let mut build = Build::default();
        let (reader, open, read) = vault_ops(&mut build);
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let one = build.args(&[(0, reader)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host, Repr::Host, Repr::Int],
            int,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args: none,
                },
                // A second resource, so that an answer of `1` is the first
                // reader rather than whatever the table happened to hold.
                Inst::CallHost {
                    dst: 1,
                    op: open,
                    args: none,
                },
                Inst::CallHost {
                    dst: 2,
                    op: read,
                    args: one,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![1]);
    }

    /// A frame holding a resource across a collection keeps it, and the
    /// collector never sees it.
    ///
    /// Both halves are the claim. The static one is that `Function::refs` —
    /// which is `RefMap::of` the frame's `Repr`s — does not name the `Host`
    /// slot, so the one pass the collector makes over a frame does not read
    /// it. The dynamic one is that the run still gets the right resource back
    /// afterwards: the word is untouched, the table is not swept, and the
    /// handle it indexes is the one the host issued.
    #[test]
    fn a_resource_in_a_frame_survives_a_collection_and_is_not_a_root() {
        let mut build = Build::default();
        let (reader, open, read) = vault_ops(&mut build);
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let none = build.args(&[]);
        let one = build.args(&[(0, reader)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host, Repr::Ref, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args: none,
                },
                Inst::Alloc {
                    dst: 1,
                    layout: cell,
                    len: Len::Count(600),
                },
                // The cell's last use. A second one fits only if this one is
                // reclaimed, which is what makes the collection happen with
                // the resource live in slot 0.
                Inst::Clear {
                    slot: 1,
                    layout: cell,
                },
                Inst::Alloc {
                    dst: 2,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::CallHost {
                    dst: 3,
                    op: read,
                    args: one,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();

        // The static half: the collector's one question about a slot, asked
        // of the map it actually reads.
        let refs = &program.function(f).refs;
        assert!(!refs.is_ref(0), "a host word is not a root");
        assert_eq!(refs.iter().collect::<Vec<_>>(), vec![1, 2]);

        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1000, Some(&hosts));
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![1]);
        assert!(
            machine.collected().collections > 0,
            "the second cell only fits after the first is reclaimed"
        );
    }

    /// A run that will not stop on its own is stopped by its budget, and the
    /// stride is what bounds how long that takes.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "spin",
            &[],
            &[Repr::Int],
            int,
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let program = build.done();
        let cancellation = Cancellation::new();
        let budget = crate::budget::Budget::with_cancellation(
            crate::budget::Limits::default(),
            cancellation.clone(),
        );
        cancellation.cancel();
        let mut machine = Machine::new(&program, 1 << 12);
        assert!(machine.run(f, &[], &budget.meter()).is_err());
        assert!(machine.instructions() <= SAFEPOINT_STRIDE + 1);
    }
    // ---- tasks ---------------------------------------------------------------

    /// A `Repr::Scope` slot, a `Repr::Task` slot, and the three scratch words
    /// every fixture below wants.
    ///
    /// Written once because what is under test is the scheduler and not the
    /// arithmetic around it: every one of these programs opens a scope,
    /// builds a closure environment, spawns it, and leaves the scope, and the
    /// only thing that differs is what the body does.
    fn counter(build: &mut Build) -> LayoutId {
        let int = build.scalar(Repr::Int);
        build.structure("Counter", &[("n", int)])
    }

    /// The instructions that fill a closure environment naming `body` and
    /// capturing the object in `held`, into slot `dst`, using `scratch`.
    fn close_over(
        build: &mut Build,
        layout: LayoutId,
        body: FunctionId,
        dst: Slot,
        scratch: Slot,
        held: Option<Slot>,
    ) -> Vec<Inst> {
        let int = build.scalar(Repr::Int);
        let word = build.scalar(Repr::Ref);
        let mut code = vec![
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            Inst::Int {
                dst: scratch,
                value: body.0 as i64,
            },
            Inst::StoreField {
                obj: dst,
                at: 0,
                src: scratch,
                layout: int,
            },
        ];
        if let Some(src) = held {
            code.push(Inst::StoreField {
                obj: dst,
                at: 1,
                src,
                layout: word,
            });
        }
        code
    }

    /// Leaving a scope waits for a task the body never awaited.
    ///
    /// The Language Card's sentence, measured the only way a machine can
    /// measure it: the child spends fifty thousand turns before it writes,
    /// and the parent reads the write. A `ScopeLeave` that did not join would
    /// read the zero the allocation left.
    #[test]
    fn leaving_a_scope_waits_for_a_task_the_body_never_awaited() {
        let mut build = Build::default().strings(&["tasks"]);
        let int = build.scalar(Repr::Int);
        let word = build.scalar(Repr::Ref);
        let held = counter(&mut build);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
            &[word],
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Int {
                    dst: 2,
                    value: 50_000,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 1,
                    b: 2,
                },
                Inst::BranchFalse { cond: 3, to: 7 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 4,
                },
                Inst::Jump { to: 3 },
                Inst::Int { dst: 1, value: 7 },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Return { src: 1 },
            ],
        );
        let environment = closure_layout(&mut build, body, &[word]);
        let mut code = vec![
            Inst::Alloc {
                dst: 0,
                layout: held,
                len: Len::Fixed,
            },
            Inst::Int { dst: 6, value: 0 },
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 6,
                layout: int,
            },
            Inst::ScopeEnter {
                dst: 1,
                name: StrId(0),
            },
        ];
        code.extend(close_over(&mut build, environment, body, 2, 6, Some(0)));
        code.extend([
            Inst::Spawn {
                dst: 3,
                scope: 1,
                closure: 2,
                answer: int,
            },
            Inst::ScopeLeave {
                scope: 1,
                failed: 4,
                error: 5,
                layout: int,
            },
            Inst::LoadField {
                dst: 6,
                obj: 0,
                at: 0,
                layout: int,
            },
            Inst::Return { src: 6 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 7);
    }

    /// A body runs at most once and is waited for at most once, so awaiting
    /// the same handle twice answers the same value and repeats no effect.
    ///
    /// The counter is what says "no effect twice": it is incremented by the
    /// body and read after both awaits, and the answer is the product, so a
    /// second run would double it.
    #[test]
    fn awaiting_the_same_handle_twice_runs_the_body_once() {
        let mut build = Build::default().strings(&["tasks"]);
        let int = build.scalar(Repr::Int);
        let word = build.scalar(Repr::Ref);
        let held = counter(&mut build);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            &[word],
            vec![
                Inst::LoadField {
                    dst: 1,
                    obj: 0,
                    at: 0,
                    layout: int,
                },
                Inst::Int { dst: 2, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ],
        );
        let environment = closure_layout(&mut build, body, &[word]);
        let mut code = vec![
            Inst::Alloc {
                dst: 0,
                layout: held,
                len: Len::Fixed,
            },
            Inst::Int { dst: 8, value: 0 },
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 8,
                layout: int,
            },
            Inst::ScopeEnter {
                dst: 1,
                name: StrId(0),
            },
        ];
        code.extend(close_over(&mut build, environment, body, 2, 8, Some(0)));
        code.extend([
            Inst::Spawn {
                dst: 3,
                scope: 1,
                closure: 2,
                answer: int,
            },
            Inst::Await {
                dst: 4,
                task: 3,
                answer: int,
            },
            Inst::Await {
                dst: 5,
                task: 3,
                answer: int,
            },
            Inst::ScopeLeave {
                scope: 1,
                failed: 6,
                error: 7,
                layout: int,
            },
            Inst::LoadField {
                dst: 8,
                obj: 0,
                at: 0,
                layout: int,
            },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 4,
                a: 4,
                b: 5,
            },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Mul,
                dst: 8,
                a: 8,
                b: 4,
            },
            Inst::Return { src: 8 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Int,
                Repr::Int,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        // One run of the body, and 7 from each of the two awaits.
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 14);
    }

    /// A task the program cancelled has no value to await, in the words the
    /// oracle uses.
    ///
    /// The body cannot end any other way — it is an unbounded loop — so what
    /// is being measured is that the flag reached a safepoint and that the
    /// join told a stop from a finish.
    #[test]
    fn awaiting_a_cancelled_task_is_refused() {
        let mut build = Build::default().strings(&["tasks"]);
        let int = build.scalar(Repr::Int);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Int],
            int,
            &[],
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let environment = closure_layout(&mut build, body, &[]);
        let mut code = vec![Inst::ScopeEnter {
            dst: 0,
            name: StrId(0),
        }];
        code.extend(close_over(&mut build, environment, body, 1, 6, None));
        code.extend([
            Inst::Spawn {
                dst: 2,
                scope: 0,
                closure: 1,
                answer: int,
            },
            Inst::Cancel { task: 2 },
            Inst::Await {
                dst: 3,
                task: 2,
                answer: int,
            },
            Inst::ScopeLeave {
                scope: 0,
                failed: 4,
                error: 5,
                layout: int,
            },
            Inst::Return { src: 3 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Int,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(
            error.message,
            "task 1 of scope `tasks` was cancelled, so it has no value to await"
        );
    }

    /// A call to an `async fn` is a call and a handle, and the handle can be
    /// awaited twice.
    ///
    /// No `ScopeEnter`, no `Spawn` and no thread: an `async fn` runs at its
    /// call site and `Inst::Settled` is the handle around what it produced.
    /// Awaiting twice answers the same words, which falls out of the state
    /// rather than being arranged — the same way it does for a spawned task,
    /// and the same way `crate::task::settle` gets it.
    #[test]
    fn a_settled_task_answers_the_call_s_words_however_often_it_is_awaited() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let body = build.function(
            "body",
            &[],
            &[Repr::Int],
            int,
            vec![Inst::Int { dst: 0, value: 7 }, Inst::Return { src: 0 }],
        );
        let main = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Task, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: body,
                    args: none,
                },
                Inst::Settled {
                    dst: 1,
                    src: 0,
                    answer: int,
                },
                Inst::Await {
                    dst: 2,
                    task: 1,
                    answer: int,
                },
                Inst::Await {
                    dst: 3,
                    task: 1,
                    answer: int,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 2,
                    b: 3,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 14);
    }

    /// Cancelling a settled task does nothing, so awaiting it still answers.
    ///
    /// `crate::task::Task::cancel` asks a task that is *running* to stop, and
    /// a task whose body already ran is not one: cancellation stops work that
    /// has not happened, it does not undo work that has. An `async fn`'s
    /// handle is the extreme case of that, because its work was over before
    /// the handle existed.
    #[test]
    fn cancelling_a_settled_task_does_not_take_its_value_away() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let body = build.function(
            "body",
            &[],
            &[Repr::Int],
            int,
            vec![Inst::Int { dst: 0, value: 7 }, Inst::Return { src: 0 }],
        );
        let main = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Task, Repr::Int],
            int,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: body,
                    args: none,
                },
                Inst::Settled {
                    dst: 1,
                    src: 0,
                    answer: int,
                },
                Inst::Cancel { task: 1 },
                Inst::Await {
                    dst: 2,
                    task: 1,
                    answer: int,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 7);
    }

    /// A settled task's answer is a root of the task that made it.
    ///
    /// The words go into a heap object the scheduler table names, and the
    /// slot the call left them in is cleared at once — which is what the
    /// lowering emits, because the `Inst::Settled` consumed the temporary.
    /// So from that instruction onwards the table is the only thing that
    /// names the object, and the loop below allocates far more than the heap
    /// holds to say so: without the table among the roots the sweep would
    /// take the answer and the `await` would read a reclaimed word.
    #[test]
    fn a_settled_task_s_answer_survives_a_collection() {
        let mut build = Build::default().strings(&["kept"]);
        let int = build.scalar(Repr::Int);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let none = build.args(&[]);
        let body = build.function(
            "body",
            &[],
            &[Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        // s0 the call's answer, s1 the handle, s2 the counter, s3 the bound,
        // s4 the test, s5 the churn, s6 the step, s7 what the await answers,
        // s8 the length that is returned.
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Task,
                Repr::Int,
                Repr::Int,
                Repr::Bool,
                Repr::Ref,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
            ],
            int,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: body,
                    args: none,
                },
                Inst::Settled {
                    dst: 1,
                    src: 0,
                    answer: str_layout,
                },
                Inst::Clear {
                    slot: 0,
                    layout: str_layout,
                },
                Inst::Int { dst: 2, value: 0 },
                Inst::Int {
                    dst: 3,
                    value: 4000,
                },
                Inst::Int { dst: 6, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 4,
                    a: 2,
                    b: 3,
                },
                Inst::BranchFalse { cond: 4, to: 12 },
                Inst::Alloc {
                    dst: 5,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear {
                    slot: 5,
                    layout: cell,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 2,
                    b: 6,
                },
                Inst::Jump { to: 6 },
                Inst::Await {
                    dst: 7,
                    task: 1,
                    answer: str_layout,
                },
                Inst::Len { dst: 8, obj: 7 },
                Inst::Return { src: 8 },
            ],
        );
        let program = build.done();
        // A heap far smaller than 4000 objects of 65 words, so the run only
        // reaches the `await` by collecting several times on the way.
        let mut machine = Machine::new(&program, 4096);
        // `kept` is four bytes, and it is still four bytes.
        assert_eq!(machine.run(main, &[], &budget()).unwrap(), vec![4]);
        assert!(
            machine.collected().collections > 0,
            "the run should have had to collect"
        );
    }

    /// A child that raised propagates as itself out of the scope it was in.
    ///
    /// Not as a value: a runtime error is not something a Cove expression can
    /// hold, so `ScopeLeave` fails with it rather than answering it. That is
    /// the difference `crate::task::ChildFailure` draws, kept.
    #[test]
    fn a_child_that_raises_leaves_the_scope_with_its_own_error() {
        let mut build = Build::default().strings(&["tasks", "the child said so"]);
        let int = build.scalar(Repr::Int);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Int],
            int,
            &[],
            vec![Inst::Trap { message: StrId(1) }],
        );
        let environment = closure_layout(&mut build, body, &[]);
        let mut code = vec![Inst::ScopeEnter {
            dst: 0,
            name: StrId(0),
        }];
        code.extend(close_over(&mut build, environment, body, 1, 5, None));
        code.extend([
            Inst::Spawn {
                dst: 2,
                scope: 0,
                closure: 1,
                answer: int,
            },
            Inst::ScopeLeave {
                scope: 0,
                failed: 3,
                error: 4,
                layout: int,
            },
            Inst::Return { src: 4 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(error.message, "the child said so");
    }

    /// Two tasks allocating at once over one heap, and an object only the
    /// parent's frame names.
    ///
    /// This is the whole of issue #240's Q1 as a test. The two children churn
    /// far past the heap's budget, so the collections are theirs; the parent
    /// is parked in a join for all of them, holding one object no child can
    /// reach. A collection that read a stale snapshot of the parent's frame,
    /// or that did not wait for a task at all, frees it.
    #[test]
    fn a_collection_a_sibling_ran_keeps_what_the_parent_holds() {
        let mut build = Build::default().strings(&["tasks"]);
        let int = build.scalar(Repr::Int);
        let held = counter(&mut build);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
            &[],
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Int {
                    dst: 2,
                    value: 20_000,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 1,
                    b: 2,
                },
                Inst::BranchFalse { cond: 3, to: 8 },
                Inst::Alloc {
                    dst: 0,
                    layout: held,
                    len: Len::Fixed,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 4,
                },
                Inst::Jump { to: 3 },
                Inst::Return { src: 1 },
            ],
        );
        let environment = closure_layout(&mut build, body, &[]);
        let mut code = vec![
            Inst::Alloc {
                dst: 0,
                layout: held,
                len: Len::Fixed,
            },
            Inst::Int { dst: 9, value: 42 },
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 9,
                layout: int,
            },
            Inst::ScopeEnter {
                dst: 1,
                name: StrId(0),
            },
        ];
        code.extend(close_over(&mut build, environment, body, 2, 9, None));
        code.extend([
            Inst::Spawn {
                dst: 3,
                scope: 1,
                closure: 2,
                answer: int,
            },
            Inst::Spawn {
                dst: 4,
                scope: 1,
                closure: 2,
                answer: int,
            },
            Inst::Await {
                dst: 5,
                task: 3,
                answer: int,
            },
            Inst::Await {
                dst: 6,
                task: 4,
                answer: int,
            },
            Inst::ScopeLeave {
                scope: 1,
                failed: 7,
                error: 8,
                layout: int,
            },
            // The two children's turns, plus the word the parent held from
            // before the first collection to after the last.
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 5,
                a: 5,
                b: 6,
            },
            Inst::LoadField {
                dst: 9,
                obj: 0,
                at: 0,
                layout: int,
            },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 9,
                a: 9,
                b: 5,
            },
            Inst::Return { src: 9 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Task,
                Repr::Int,
                Repr::Int,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 42 + 40_000);
    }
    /// A child whose *value* was `Err(...)` leaves that error where the
    /// enclosing function can return it.
    ///
    /// The other half of what a failing child can be, and the half the corpus
    /// does not reach: `scope s { s.spawn { f()? } }` means the failure
    /// reaches the caller rather than sitting unread in a handle nobody
    /// awaited, and `crate::task::ChildFailure::Returned` is where the oracle
    /// says so. Here the answer is a run of words in the object the parent
    /// allocated, and what `ScopeLeave` copies out of it is the `Err` case's
    /// payload — at the layout the instruction names, which is the
    /// *enclosing* function's failure and not the child's answer.
    #[test]
    fn a_child_whose_value_failed_leaves_the_scope_with_its_payload() {
        let mut build = Build::default().strings(&["tasks"]);
        let int = build.scalar(Repr::Int);
        let answer = build.enumeration("Result", &[("Ok", vec![int]), ("Err", vec![int])]);
        let body = build.lambda(
            "body",
            &[],
            &[Repr::Int, Repr::Int],
            answer,
            &[],
            vec![
                // `Err(9)`: the case index, then the payload word the case
                // was placed at.
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 9 },
                Inst::Return { src: 0 },
            ],
        );
        let environment = closure_layout(&mut build, body, &[]);
        let mut code = vec![Inst::ScopeEnter {
            dst: 0,
            name: StrId(0),
        }];
        code.extend(close_over(&mut build, environment, body, 1, 5, None));
        code.extend([
            Inst::Spawn {
                dst: 2,
                scope: 0,
                closure: 1,
                answer,
            },
            Inst::ScopeLeave {
                scope: 0,
                failed: 3,
                error: 4,
                layout: int,
            },
            Inst::Return { src: 4 },
        ]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Scope,
                Repr::Ref,
                Repr::Task,
                Repr::Bool,
                Repr::Int,
                Repr::Int,
            ],
            int,
            code,
        );
        let program = build.done();
        // Zero would be the location as the frame was zeroed, which is what
        // a `ScopeLeave` that had not noticed would leave there.
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 9);
    }

    // ---- a host runs a Cove callback ------------------------------------

    /// A host that runs the callback it was handed.
    ///
    /// Three shapes, which are the three the shipped hosts have: `apply`
    /// calls once, as `http.Server.handle` does; `twice` calls more than
    /// once, as `clock.every` does; and `bounded` bounds the body with a flag
    /// it raises first, as `clock.timeout` does, and turns the stop into its
    /// own answer rather than passing the error on.
    struct Runner;

    static RUNNER_OPS: &[cove_schema::OperationSchema] = &[
        cove_schema::OperationSchema {
            name: "apply",
            params: &[cove_schema::HostType::Any],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "runner",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: false,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "twice",
            params: &[cove_schema::HostType::Any],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "runner",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: false,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "bounded",
            params: &[cove_schema::HostType::Any],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "runner",
            effect: cove_schema::Effect::Read,
            cancellable: true,
            recordable: false,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "caught",
            params: &[cove_schema::HostType::Any],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "runner",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: false,
            result_is_task_safe: true,
        },
    ];

    impl crate::host::HostApi for Runner {
        fn module_schema(&self) -> cove_schema::ModuleSchema {
            cove_schema::ModuleSchema {
                name: "runner",
                capability: "runner",
                operations: RUNNER_OPS,
                types: &[],
                resources: &[],
            }
        }

        fn call_with(
            &self,
            op: &str,
            args: Vec<Value>,
            back: &mut dyn Reentry,
        ) -> Result<Value, RuntimeError> {
            match op {
                "apply" => back.call(&args[0], Vec::new()),
                "twice" => {
                    let one = back.call(&args[0], Vec::new())?.as_int().unwrap_or(0);
                    let other = back.call(&args[0], Vec::new())?.as_int().unwrap_or(0);
                    Ok(Value::int(one + other))
                }
                // The `clock.timeout` shape: the flag is raised before the
                // body runs, so the body stops at its first safepoint and the
                // host answers its own bound rather than passing the error on.
                "bounded" => {
                    let stop = Cancellation::new();
                    stop.cancel();
                    match back.call_until(&args[0], Vec::new(), &stop) {
                        Ok(_) => Ok(Value::int(0)),
                        Err(_) if stop.is_cancelled() => Ok(Value::int(-1)),
                        Err(error) => Err(error),
                    }
                }
                // A host that catches what a callback failed with and carries
                // on, which is what makes restoring the frames this call
                // grew a requirement rather than a tidiness.
                "caught" => match back.call(&args[0], Vec::new()) {
                    Ok(value) => Ok(value),
                    Err(_) => Ok(Value::int(-2)),
                },
                other => Err(RuntimeError::new(format!("no `{other}` here"))),
            }
        }

        fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            Err(RuntimeError::new(format!("`{op}` needs a way back")))
        }
    }

    fn running() -> crate::host::HostRegistry {
        let mut hosts = crate::host::HostRegistry::new(crate::host::Grants::new(["runner"]));
        hosts.register(Box::new(Runner));
        hosts
    }

    /// A program whose entry hands `runner.<op>` a closure over `depth`.
    ///
    /// The lambda is the recursion the reentry bound is about:
    ///
    /// ~~~text
    /// fn step() -> Int {          // captures d
    ///   if d == 0 { return 0 }
    ///   runner.apply(fn() { ... d - 1 ... }) + 1
    /// }
    /// ~~~
    ///
    /// So `step` at `d` makes one host call, which runs `step` at `d - 1`,
    /// and the answer counts the levels — which is what makes a wrong bound
    /// visible as a wrong number rather than only as a missing error.
    fn a_reentering_program(op: &str, depth: i64) -> Program {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let reference = build.word("Fn", Repr::Ref);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("runner"),
            operation: Arc::from(op),
            result: int,
        });
        let call = cove_ir::HostOpId(build.program.host_ops.len() as u32 - 1);
        let args = build.args(&[(0, reference)]);
        let inner = build.args(&[(3, reference)]);

        // The lambda is at the index it will be pushed to, which is what lets
        // its own body name it: a closure over `step` is what `step` builds.
        let step_id = FunctionId(build.program.functions.len() as u32);
        let closure = build.layout(
            "closure",
            Shape::Closure {
                function: step_id,
                captures: vec![int],
            },
        );
        let step = build.lambda(
            "step",
            &[],
            &[
                Repr::Int,  // 0: the capture, `d`
                Repr::Int,  // 1: zero
                Repr::Bool, // 2: d != 0
                Repr::Ref,  // 3: the closure over d - 1
                Repr::Int,  // 4: the callee's id
                Repr::Int,  // 5: d - 1
                Repr::Int,  // 6: the answer
                Repr::Int,  // 7: one
            ],
            int,
            &[int],
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Ne,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 11 },
                Inst::Alloc {
                    dst: 3,
                    layout: closure,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 4,
                    value: step_id.0 as i64,
                },
                Inst::StoreField {
                    obj: 3,
                    at: 0,
                    src: 4,
                    layout: int,
                },
                Inst::Int { dst: 7, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Sub,
                    dst: 5,
                    a: 0,
                    b: 7,
                },
                Inst::StoreField {
                    obj: 3,
                    at: 1,
                    src: 5,
                    layout: int,
                },
                Inst::CallHost {
                    dst: 6,
                    op: call,
                    args: inner,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 6,
                    a: 6,
                    b: 7,
                },
                // The frame is zeroed, so the `d == 0` arm answers the zero
                // that is already standing there.
                Inst::Return { src: 6 },
            ],
        );
        assert_eq!(step, step_id, "the lambda is where its own body says");

        build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: closure,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: step_id.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int {
                    dst: 2,
                    value: depth,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 2,
                    layout: int,
                },
                Inst::CallHost {
                    dst: 3,
                    op: call,
                    args,
                },
                Inst::Return { src: 3 },
            ],
        );
        build.done()
    }

    /// The entry of [`a_reentering_program`], run.
    fn reentering(op: &str, depth: i64) -> Result<i64, RuntimeError> {
        let program = a_reentering_program(op, depth);
        let entry = program.functions.len() as u32 - 1;
        let hosts = running();
        let mut machine = Machine::with_hosts(&program, 1 << 14, Some(&hosts));
        machine
            .run(FunctionId(entry), &[], &budget())
            .map(|words| words[0] as i64)
    }

    /// A closure reaches a host, and the host runs it.
    ///
    /// The whole of what was missing. The boundary refused to materialise one
    /// and `Back::call` refused on the other side, so a program that wrote
    /// `clock.timeout(500ms) { .. }` did not lower at all.
    #[test]
    fn a_host_runs_the_callback_it_was_handed() {
        assert_eq!(reentering("apply", 1).unwrap(), 1);
    }

    /// A host may call the callback as many times as its operation means, and
    /// each one is a call the run pays for in full.
    #[test]
    fn a_host_may_call_its_callback_more_than_once() {
        // Each round answers 1, so a host that ran it twice answers 2 — and
        // one that reused a frame instead of opening a second would not.
        assert_eq!(reentering("twice", 1).unwrap(), 2);
    }

    /// A callback that fails leaves the machine exactly as it found it.
    ///
    /// `clock.timeout` catches what the body failed with and answers its own
    /// bound, so the frames the callback grew, the words its frames occupied
    /// and the scopes it opened must all be back where they were — the outer
    /// run has no unwinding, and the reasoning that makes that sound (the run
    /// is ending) does not reach a host that carries on.
    #[test]
    fn a_failed_callback_leaves_the_frames_it_grew() {
        // The body divides by zero at the first level and the host catches
        // it; the entry then returns through frames that have to be intact.
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let reference = build.word("Fn", Repr::Ref);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("runner"),
            operation: Arc::from("caught"),
            result: int,
        });
        let op = cove_ir::HostOpId(0);
        let args = build.args(&[(0, reference)]);
        let step_id = FunctionId(build.program.functions.len() as u32);
        let closure = build.layout(
            "closure",
            Shape::Closure {
                function: step_id,
                captures: vec![],
            },
        );
        let step = build.lambda(
            "step",
            &[],
            &[Repr::Int, Repr::Int, Repr::Int],
            int,
            &[],
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 0 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Div,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        assert_eq!(step, step_id);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: closure,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: step_id.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallHost { dst: 2, op, args },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let hosts = running();
        let mut machine = Machine::with_hosts(&program, 1 << 13, Some(&hosts));
        let before = machine.mem.stack_words();
        assert_eq!(
            machine.run(main, &[], &budget()).unwrap(),
            vec![-2i64 as u64]
        );
        assert_eq!(
            machine.mem.stack_words(),
            before,
            "the callback's frame went back where it came from"
        );
        assert!(machine.frames.is_empty());
    }

    /// A bounded call's flag stops the body at its next safepoint.
    ///
    /// `Reentry::call_until` says `stop` *"bounds this call and everything
    /// inside it"*, and until a callback could run here at all there was
    /// nothing for it to bound: `Machine::stops` was `&[]` at every safepoint
    /// and at the boundary. The body here is a loop long enough to reach a
    /// safepoint, and what stops it is the oracle's own `stopped_here`.
    #[test]
    fn a_bounded_callback_stops_at_a_safepoint() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let reference = build.word("Fn", Repr::Ref);
        build.program.host_ops.push(cove_ir::HostOp {
            resource: None,
            module: Arc::from("runner"),
            operation: Arc::from("bounded"),
            result: int,
        });
        let op = cove_ir::HostOpId(0);
        let args = build.args(&[(0, reference)]);
        let step_id = FunctionId(build.program.functions.len() as u32);
        let closure = build.layout(
            "closure",
            Shape::Closure {
                function: step_id,
                captures: vec![],
            },
        );
        // `var i = 0; while i < 100000 { i += 1 }; i`
        let step = build.lambda(
            "step",
            &[],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
            &[],
            vec![
                Inst::Int { dst: 0, value: 0 },
                Inst::Int {
                    dst: 1,
                    value: 100_000,
                },
                Inst::Int { dst: 3, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 6 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 0,
                    a: 0,
                    b: 3,
                },
                Inst::Jump { to: 3 },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(step, step_id);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: closure,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: step_id.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallHost { dst: 2, op, args },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let hosts = running();
        let mut machine = Machine::with_hosts(&program, 1 << 13, Some(&hosts));
        // The host turned the stop into its own answer, which it could only
        // do because the body stopped.
        assert_eq!(
            machine.run(main, &[], &budget()).unwrap(),
            vec![-1i64 as u64]
        );
        // And the flag went with the call that raised it.
        assert!(machine.stops.is_empty());
    }

    // ---- how deep a reentry may nest ------------------------------------

    /// Host → Cove → Host → Cove, as deep as the bound allows.
    ///
    /// This is the case that decides what `docs/LINEAR_VM.md`'s *"a builtin
    /// never calls back into Cove"* means here. Cove calling Cove adds no
    /// native frame in this backend, so the reserved stack region is the
    /// whole of the depth question — but a *host* callback is not Cove
    /// calling Cove: the host is already a Rust frame, and running its
    /// callback puts `HostRegistry::dispatch`, the host's own frames, and
    /// another turn of `Machine::dispatch` under every Cove frame the
    /// callback makes. So the bound is the oracle's `MAX_REENTRY_DEPTH`,
    /// which exists for exactly that.
    #[test]
    fn a_reentry_may_nest_up_to_the_bound() {
        // One host call per level, `MAX_REENTRY_DEPTH` of them stacked at the
        // deepest point, and the answer counts them.
        let depth = crate::interp::MAX_REENTRY_DEPTH as i64 - 1;
        assert_eq!(reentering("apply", depth).unwrap(), depth);
    }

    /// And no deeper: the run stops, rather than the process.
    #[test]
    fn a_reentry_past_the_bound_stops_the_run_rather_than_the_process() {
        let depth = crate::interp::MAX_REENTRY_DEPTH as i64;
        let error = reentering("apply", depth).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "reentry depth limit of {} reached while a host ran a Cove callback",
                crate::interp::MAX_REENTRY_DEPTH
            ),
            "the refusal is the oracle's own, word for word"
        );
    }

    /// A host that runs its callback twice pays for one level twice over
    /// rather than for two levels at once.
    #[test]
    fn calling_a_callback_twice_is_one_level_twice() {
        // Deep enough that two levels at once would be past the bound, and
        // `twice` at every level, so a count that did not come back down
        // would refuse long before this answers.
        let depth = crate::interp::MAX_REENTRY_DEPTH as i64 - 1;
        assert!(reentering("twice", depth).is_ok());
    }

    // ---- cells ---------------------------------------------------------

    /// A cell taken and given back leaves nothing held.
    ///
    /// The pair the lowering emits, on its own, so that a failure here is the
    /// machine's arms and not a lowering that emitted the wrong pair.
    #[test]
    fn a_cell_is_taken_and_given_back_by_the_pair_of_instructions() {
        let mut build = Build::default();
        let int = build.word("Int", Repr::Int);
        let held = build.layout("Shared", Shape::Shared { value: int });
        let main = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Ref],
            int,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: held,
                    len: Len::Fixed,
                },
                Inst::SharedLock { cell: 1 },
                Inst::Int { dst: 0, value: 7 },
                Inst::StoreField {
                    obj: 1,
                    at: cove_ir::SHARED_VALUE,
                    src: 0,
                    layout: int,
                },
                Inst::SharedUnlock { cell: 1 },
                Inst::LoadField {
                    dst: 0,
                    obj: 1,
                    at: cove_ir::SHARED_VALUE,
                    layout: int,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        assert_eq!(machine.run(main, &[], &budget()).unwrap(), vec![7]);
        assert!(machine.held.is_empty());
    }

    /// A run that fails inside a `lock` gives back every cell it was holding.
    ///
    /// The release is an obligation on every exit path, and a runtime error is
    /// the one path the lowering cannot write: it is not a jump, so no
    /// `Inst::SharedUnlock` stands between it and the end of the run. Without
    /// this a cell a failing task never gave back would be a cell no task
    /// could ever take — and *no task* is the point, because the heap and the
    /// cells in it belong to the run rather than to the task that failed.
    ///
    /// Two cells, because they nest and the refusal is per cell: giving back
    /// only the innermost would leave the other held.
    #[test]
    fn a_failing_run_gives_back_every_cell_it_held() {
        let mut build = Build::default().strings(&["stop"]);
        let int = build.word("Int", Repr::Int);
        let held = build.layout("Shared", Shape::Shared { value: int });
        let main = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Ref, Repr::Ref],
            int,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: held,
                    len: Len::Fixed,
                },
                Inst::Alloc {
                    dst: 2,
                    layout: held,
                    len: Len::Fixed,
                },
                Inst::SharedLock { cell: 1 },
                Inst::SharedLock { cell: 2 },
                Inst::Trap {
                    message: cove_ir::StrId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget()).unwrap_err();
        assert_eq!(error.message, "stop");

        assert!(machine.held.is_empty());
        // The frames are left standing by a failed run, so the two cells are
        // still where the fixture put them and can be asked.
        let base = machine.frames[0].base;
        for slot in [1, 2] {
            let addr = machine.mem.slot(base, slot);
            assert_eq!(
                cell::holder(&machine.mem, addr),
                0,
                "a cell a failing task took is free again"
            );
        }
    }

    /// A task that already holds a cell is refused rather than made to wait,
    /// and the refusal does not take the cell.
    #[test]
    fn a_reentrant_lock_is_refused_and_leaves_the_cell_held_once() {
        let mut build = Build::default();
        let int = build.word("Int", Repr::Int);
        let held = build.layout("Shared", Shape::Shared { value: int });
        let main = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Ref],
            int,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: held,
                    len: Len::Fixed,
                },
                Inst::SharedLock { cell: 1 },
                Inst::SharedLock { cell: 1 },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget()).unwrap_err();
        assert_eq!(
            error.message,
            "this task already holds this `Shared`, so `lock` would wait for itself"
        );
        // The refusal did not take the cell a second time, so the unwind gives
        // it back exactly once.
        assert!(machine.held.is_empty());
        let addr = machine.mem.slot(machine.frames[0].base, 1);
        assert_eq!(cell::holder(&machine.mem, addr), 0);
    }
}
