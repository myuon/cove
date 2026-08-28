//! The VM that runs the executable IR.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) decides why this
//! exists. A tree-walking interpreter re-derives, on every evaluation, facts
//! that were settled before the program ran — where a binding lives, what a
//! call targets, how big a frame is — and a tree has nowhere to put an answer
//! so that using it costs nothing. [`cove_ir`] put those answers into the
//! instructions; this runs them.
//!
//! # The interpreter is the oracle, so nothing here decides anything
//!
//! ADR 0012 ranks the specification above the oracle above a backend, and ADR
//! 0019 keeps that arrangement: [`crate::interp`] is what a Cove program
//! means, and this is a second way of arriving at the same answer. So every
//! value operation here calls the interpreter's own code rather than
//! restating it — `binary`, `unary`, [`crate::builtins::call_method`],
//! [`crate::builtins::call_constructor`], and the one [`HostRegistry`] both
//! backends dispatch through. What is written here is the execution model and
//! nothing else: a stack, a frame, and a dispatch loop.
//!
//! Where an answer is not the interpreter's to give, it is the checker's. A
//! condition that is not a `Bool` reports what `cove-sema` reports, because
//! the IR does not say whether the jump it lowered to came from an `if`, a
//! `while`, or a `&&`, and the checker refuses such a program before any of
//! them is lowered.
//!
//! # A typed instruction does the same thing faster, or it is not typed
//!
//! [`Inst::IntBinary`] and [`Inst::GetFieldAt`] are emitted only where the
//! checker settled the type they assume, so what arrives is what was
//! promised and neither instruction examines it. What that is allowed to
//! change is how the answer is reached and nothing about the answer:
//! overflow, division by zero, and remainder by zero raise the interpreter's
//! own errors through the interpreter's own helpers, so a program that fails
//! fails identically whichever instruction ran. An operand that is somehow
//! not what the lowering promised is a broken invariant of this backend, and
//! is said so the way an empty operand stack is.
//!
//! # One stack, and a frame is a region of it
//!
//! Every frame's slots and every frame's operands live in one contiguous
//! `Vec<Value>`. A call does not allocate: the arguments are already on the
//! stack, in order, and they *become* the callee's first slots. That is the
//! whole point of the exercise — issue #104 measured a minimal call at
//! 650–790 ns, most of it building an environment and tearing it down.
//!
//! # A second stack, for the words that are not values
//!
//! Beside it is a `Vec<i64>`, and a slot the checker proved holds an `Int` or
//! a `Bool` lives there instead — as the integer itself, or as 0 or 1. Every
//! frame is a window into both, at its `base` and at its `scalar_base`, and
//! a call and a return move both the same way.
//!
//! That includes the arguments and the answer. A parameter the checker
//! settled is a scalar slot, so its argument was pushed onto the scalar
//! stack and becomes that slot without moving, exactly as a value argument
//! becomes a value slot; `cove_ir::Function::params` is which of the two
//! each argument arrived on and `cove_ir::Inst::Call` carries the counts.
//! `cove_ir::Function::returns` is the same question about the answer, and
//! [`Inst::ReturnScalar`] is what a function that answers a scalar ends in.
//! Neither is examined at run time: the counts are in the instruction and
//! the two stacks are resized from them.
//!
//! The point is negative. `benches/arith` adds two integers two million
//! times, and a `Value` is 40 bytes with a destructor, so the loop was moving
//! 40 bytes per push and running drop glue per pop to do arithmetic that owns
//! nothing. A typed instruction over a scalar stack does not touch a `Value`
//! at all, and [`Inst::ScalarToValue`] and [`Inst::ValueToScalar`] are the
//! two places where the loop meets something general. A struct field of a
//! settled scalar type is read by [`Inst::GetFieldAtScalar`] instead, and
//! does not go through either: the field is converted straight off the
//! struct, without a `Value` built only to be handed to `ValueToScalar` and
//! thrown away.
//!
//! # A third stack, for the places a `var` parameter names
//!
//! A `var` parameter names the caller's own storage rather than a copy of
//! it, and what names storage here is a *place*: an index into the value
//! stack, and the field positions to walk from what stands there. Those live
//! in a third `Vec` beside the other two, a frame is a window into all
//! three, and `cove_ir::Inst::Call` carries a third argument count.
//!
//! `Place`, below, says why an index is the right thing to hold and what its
//! validity rests on. The short version is that the value stack is one `Vec`
//! that reallocates, and that no place leaves the frame that built it — a
//! closure captures the *value* a place names, which is what
//! `Interpreter::make_closure` captures too.
//!
//! **A scalar slot holds no reference, and neither does a place.** That is
//! what the root set is: the stacks are numbered separately, so a scalar slot
//! is not a number in the value stack's space at all, and a frame's whole
//! value window, `stack[base .. base + value_frame_size]`, is its root set
//! with nothing inside it to skip. A place holds an index into that same
//! window, so whatever it reaches is reachable from what is already scanned.
//! There is no collection in this VM yet — nothing the lowering covers
//! allocates growable storage — so this is a statement about where the roots
//! are rather than code that reads them, said here so that whoever writes
//! that collection does not have to infer it. A *moving* collector would need
//! one thing more, which is that a place is an index into the storage it is
//! moving.
//!
//! # Fuel is charged by the block, not by the instruction
//!
//! A straight line's length is known when it is lowered, and the dispatch
//! loop ran two additions and a compare per instruction to arrive at the same
//! number one addition could. So `cove_ir::Function::block_fuel` says how far
//! the line beginning at each index runs, and `Vm::charge` adds the whole of
//! it where control *arrives* at a head — at the entry, at a taken jump, at
//! the fall-through of one not taken, at a callee's first instruction, at the
//! caller's resumption after a return, and at the fall-through of a `?`.
//!
//! Those are all the arrivals there are, which is what makes the total for a
//! path the total it was before. The counts overlap on purpose: a head that
//! is also fallen into is covered by the extent above it, so the fall costs
//! nothing to notice. `cove_ir::lower::block_fuel` is where that is argued.
//!
//! What changes is how much may happen between two *checks* of the budget:
//! one straight line more than before, bounded by the length of the
//! function's code. `SAFEPOINT_INTERVAL` states that bound.
//!
//! # A closure is a function id and the values beside it
//!
//! `cove_ir::Inst::MakeClosure` builds a `Value::Closure` whose body is a
//! `crate::value::ClosureBody::Lowered` — a `FunctionId` of this run's
//! program — over the captures the lowering settled. `cove_ir::Inst::CallValue`
//! is what enters one: the arguments are already the callee's first value
//! slots, the captures are copied in behind them, and the frame is opened
//! the way any other call opens one.
//!
//! A host that receives such a closure can run it, which is what
//! `Vm::call_from_host` is: the dispatch loop, entered again on the stacks
//! as they stand, with fuel, cancellation, the depth limit and the trace all
//! still the loop's.
//!
//! # A task gets a VM of its own
//!
//! ADR 0008 runs a spawned task on a thread of its own, and gives it an
//! evaluator of its own to run it with. For this backend that is a second
//! `Vm`: [`Vm::for_task`] builds one on the new thread, over the same
//! [`Runtime`] and the same `cove_ir::Program`, with its own three stacks,
//! its own frames, its own heap, and its own constant values. Nothing about
//! the spawning VM crosses, because nothing about it could — every one of
//! those is `Rc`-based or is a `Vec` this thread owns.
//!
//! **What crosses is the program and the body.** The program is shared,
//! which is what makes a `FunctionId` in a lowered closure mean the same
//! function on the far side; see `cove_ir`'s "One program, and every thread
//! of a run reads it". The body crosses as a `crate::task::Transfer`, which
//! *is* the task-safety rule — the Language Card lets a value cross exactly
//! when copying it is the whole of transferring it — and it is the same walk
//! the interpreter's `spawn` makes.
//!
//! Every decision either backend makes about a task is `crate::task`'s, so
//! there is no second statement of what may cross, of what a scope does with
//! a child that failed, or of what a `spawn` charges. What is written here is
//! the stack discipline: six instructions, one dispatch arm, and the one
//! thing a stack machine has to answer that a tree walk does not — what
//! happens to a scope whose frame returned out from under it. [`Vm::leave`]
//! is that answer, because it is the one place a frame is popped.
//!
//! Fuel, the deadline, the run's cancellation, the task's own cancellation,
//! the depth limit, and the trace are accounted inside a task exactly as they
//! are outside one, because they are accounted by the same `Vm`: a task's VM
//! reaches the run's one budget through the same [`HostRegistry`], and
//! [`Vm::safepoint`] asks the task's own flag beside the rest.
//!
//! # What is not here
//!
//! An `async fn`, `Shared`, and everything else `cove_ir::lower` reports as
//! [`cove_ir::Unsupported`]. ADR 0019's no-silent-fallback rule is
//! what makes that the right shape: a program the lowering refuses never
//! reaches this, so there is no construct this can be wrong about.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::{SourceMap, Span};
use cove_ir::{
    BinaryOp as IrBinary, Const, ConstId, DispatchId, Function, FunctionId, Inst, IntOp, Program,
    Scalar, SlotKind, UnaryOp as IrUnary,
};
use cove_schema::builtins::{free_builtin, FreeBuiltinKind, MAP_ENTRY, NONE_CASE, OPTION, RESULT};
use cove_syntax::ast::{BinaryOp, EnumDecl, UnaryOp};

use crate::budget::{Cancellation, Stopped};
use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::heap::{Heap, HeapStats};
use crate::host::{HostRegistry, Reentry, ResourceHandle};
use crate::interp::{
    as_dyn, binary, coerce_inside, divide_by_zero, dyn_receiver, no_field, not_a_struct, overflow,
    returned_error_message, source_text, unary, work_stopped, MAX_CALL_DEPTH,
};
use crate::runtime::{Runtime, ENTRY_TASK};
use crate::task::{self, ChildFailure, TaskOutcome, TaskScope, Transfer};
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::value::{Closure, ClosureBody, StructValue, Value};

/// Fuel charged for executing one instruction.
///
/// ADR 0019 requires fuel to be charged for VM work, and accepts that the two
/// backends will not report the same `fuel_spent` for the same program:
/// instructions are not AST nodes and there is no honest mapping between
/// them. What must hold on both is the property fuel exists for — a run that
/// exceeds its budget stops, deterministically, at a point the program can be
/// told about — and an instruction is the unit of work this backend can
/// promise that about.
///
/// It is charged a basic block at a time rather than an instruction at a
/// time, by [`Vm::charge`], which is the same total over the same path: how
/// far a straight line runs was settled when it was lowered, and a straight
/// line runs to its end or the run ends. What that changes is not what a path
/// costs but how much may happen between two *checks* of the budget, and
/// [`SAFEPOINT_INTERVAL`] is where that bound is stated.
const INSTRUCTION_FUEL: u64 = 1;

/// How much fuel may accumulate between safepoints before one is forced.
///
/// Fuel is charged a basic block at a time and spent against the shared
/// [`crate::budget::Budget`] at a safepoint, so a long stretch of
/// straight-line instructions would otherwise hold its charge until the next
/// call or back edge. A cap keeps "a run that exceeds its budget stops" a
/// statement about the run rather than about its loops.
///
/// The cap is read where the charge is made rather than where the
/// instructions were counted, so what it bounds is the fuel standing at the
/// *start* of a block. A block runs to its end once entered, so the work
/// between two safepoints is this much plus one block — bounded by the
/// length of the function's code, which is finite and known before the run —
/// plus whatever the proportional charges of that block added.
const SAFEPOINT_INTERVAL: u64 = 1024;

/// How much fuel may accumulate across back edges before the shared budget is
/// spent against.
///
/// A back edge is where a loop can be stopped, so one is checked at every one
/// of them — but *checking* takes a lock the tasks share, and `benches/arith`
/// takes two million back edges, which was 13% of its run.
///
/// So a back edge checks only once this much fuel has gathered, and every
/// question a back edge asks waits for it: the run's cancellation, its
/// deadline, its fuel, and the stop flags of every bounded call this thread is
/// inside. What that costs is granularity: a loop notices a stop within this
/// much fuel plus the one turn that crosses it, rather than within one
/// iteration. What it buys is that a tight loop does not lock a mutex per turn
/// and does not walk a list per turn either. The number is small enough that
/// the difference is not one a program can be written to observe, and
/// [`SAFEPOINT_INTERVAL`] still bounds a straight line that has no back edge
/// at all.
const BACK_EDGE_FUEL: u64 = 64;

/// One call in progress.
///
/// The four numbers are what a return needs and nothing more, which is why a
/// call costs a push here rather than an allocation.
///
/// It is 32 bytes, and staying 32 bytes is why `return_pc` is a `u32` rather
/// than the `usize` it was. A frame is copied into a local of
/// [`Vm::execute`]'s hot loop and read by every instruction that addresses a
/// slot, so its width is register pressure in the loop `benches/arith`
/// spends its whole run in; adding the place window without narrowing
/// something would have made it 40. An instruction index is a `u32`
/// everywhere else in the IR — every jump target is one, and
/// `cove_ir::lower::validate` bounds them all by the code's length — so a
/// resumption point being one too is the same fact read from the other side.
#[derive(Clone, Copy)]
struct Frame {
    /// The function whose instructions are running.
    function: FunctionId,
    /// Where the caller resumes: the instruction after its `Call`.
    return_pc: u32,
    /// Where this frame's value slots begin in the value stack. Its slots
    /// are `stack[base .. base + value_frame_size]`, and its operands sit
    /// above them.
    ///
    /// This is also the caller's value-operand top, because the arguments
    /// that travel on the value stack were pushed onto the caller's stack
    /// and then became this frame's first value slots without moving. A
    /// return truncates to `base`, which is that one fact read from the
    /// other side.
    base: usize,
    /// Where this frame's scalar slots begin in the scalar stack. Its slots
    /// are `scalars[scalar_base .. scalar_base + scalar_frame_size]`.
    ///
    /// A separate number from `base` because the two stacks are separate,
    /// and read the same way from the other side: it is the caller's scalar
    /// operand top before it pushed the scalar arguments, so those arguments
    /// become this frame's first scalar slots without moving, exactly as the
    /// value arguments become its first value slots. A return truncates to
    /// it, which discards this frame's scalar slots and its scalar arguments
    /// together and leaves the caller's own operands exactly as they were.
    ///
    /// The two stacks are numbered separately: `cove_ir::Inst::LoadLocal` and
    /// `cove_ir::Inst::StoreLocal` address `base`'s stack, and
    /// `cove_ir::Inst::LoadScalar` and `cove_ir::Inst::StoreScalar` address
    /// this one, so which stack a slot lives in is decided by which
    /// instruction addresses it rather than by anything read at run time.
    scalar_base: usize,
    /// Where this frame's place slots begin in the place stack, read from
    /// the other side exactly as the two above are: it is the caller's place
    /// operand top before it pushed the place arguments, so those arguments
    /// become this frame's first place slots without moving.
    place_base: usize,
}

/// An assignable location: a slot of the value stack, and the struct fields
/// to navigate from what stands in it.
///
/// This is `crate::interp::Place` with the one thing that made it allocate
/// taken out. The interpreter gives every binding an `Rc<RefCell<Value>>` of
/// its own, so a place can be a share of that cell; this VM put every
/// binding into one contiguous `Vec<Value>` precisely so that a call would
/// allocate nothing, and reintroducing a cell per binding to make places
/// possible would give back the whole of what the arrangement bought.
///
/// # Why an index, and what it rests on
///
/// An index into the value stack, rather than a pointer into it, because the
/// stack is a `Vec` that grows: a push can move every element, and a raw
/// pointer taken before that push would name freed memory afterwards, while
/// an index names the same slot before and after. It is also the only form a
/// safe Rust program can hold at all, since a borrow of the stack would stop
/// the VM from touching the stack.
///
/// The index is absolute rather than relative to a frame, because a place
/// travels: `bump(var total)` builds it in the caller's frame and reads and
/// writes it in the callee's, where `frame.base` is a different number.
///
/// **What makes it valid is that nothing a lowered program can build
/// outlives the frame it was built in.** A frame's slots are live from the
/// call that opened the window to the return that truncates it, and a place
/// is built by an instruction of some frame and consumed by an instruction
/// of that frame or of one it called. No call answers a place, and no value
/// contains one.
///
/// A closure is the construct that could have broken that, because it can be
/// returned. It does not, because **a closure captures the value a place
/// names rather than the place**: `cove_ir::lower`'s `Body::lambda` reads a
/// captured `var` parameter with a `cove_ir::Inst::PlaceRead`, which is the
/// read `Env::captures` makes in the interpreter, and the oracle agrees that
/// it is a read — a closure over a `var` binding still answers what the
/// binding held when the closure was written, after the binding has been
/// assigned to. `cove_ir::Inst::PlaceLocal` carries the same note on the
/// other side of the boundary.
///
/// A callback a host runs re-entrantly does not break it either, for a
/// narrower reason: `Vm::call_from_host` opens its frame *above* the
/// frames that are standing and truncates back to where it found them, so a
/// place standing in one of those frames still points where it pointed.
///
/// # The path
///
/// A `Vec<u32>` of field positions, outermost first, which is exactly the
/// shape `crate::interp::Place::steps` has and costs exactly what it costs
/// there: an empty path allocates nothing, and appending a step to a
/// non-empty one copies it.
///
/// A fixed-size inline path was the alternative, and it would make a place
/// `Copy` and a refinement free. It was not taken because the bound could
/// not be enforced where a bound has to be enforced. The depth a place
/// reaches is the sum of the static appends along a chain of calls —
/// `f(var c.inner)` inside a body that was itself handed a place with a path
/// — and the lowering of a callee cannot see what depth its callers will
/// hand it. So the bound would have to be checked at run time, and a program
/// that exceeded it would fail on this backend and answer on the oracle,
/// which is the one difference between the two that is not allowed to exist.
#[derive(Clone, Debug)]
struct Place {
    /// Which slot of the value stack this is rooted at, absolutely.
    slot: usize,
    /// The field positions to walk from there, outermost first. Empty for a
    /// place that names a binding.
    path: Vec<u32>,
}

impl Place {
    /// The place naming `slot` of the value stack, with no path.
    fn rooted_at(slot: usize) -> Place {
        Place {
            slot,
            path: Vec::new(),
        }
    }

    /// This place with one more field step on the end, which is
    /// `crate::interp::Place::field` by position rather than by name.
    fn field(&self, index: u32) -> Place {
        let mut path = self.path.clone();
        path.push(index);
        Place {
            slot: self.slot,
            path,
        }
    }
}

/// What a `MakeStruct` builds, worked out once for the whole run.
///
/// Splitting the field list and asking the checker whether the type is opaque
/// are facts about a declaration rather than about an execution, so they are
/// settled where every other such fact is: before the first instruction runs.
struct StructShape {
    type_name: Rc<str>,
    fields: Vec<Rc<str>>,
    /// Whether the declaration was `export opaque struct`, which is what
    /// makes a value of it render as its name alone (ADR 0014). The IR
    /// carries the type's qualified name and the checker knows the rest, so
    /// this is read from the checker exactly as `Interpreter::init_struct`
    /// reads it.
    opaque: bool,
}

/// What a `MakeEnum` builds a case of, worked out once for the whole run.
///
/// A `MakeEnum` names its type with one constant holding the qualified name,
/// and the declaration behind that name does not change between two of them,
/// so one entry per type constant is a complete table. The declaration is
/// what says which cases exist and how much payload each carries, and it is
/// the checker's answer rather than the IR's — read here exactly as
/// `Interpreter::find_enum` reads it, and read once.
struct EnumShape {
    /// The module that declares the enum, which is the first half of the
    /// qualified name the built value carries.
    module: Rc<str>,
    decl: Arc<EnumDecl>,
}

/// A task scope this VM has entered and not yet left.
///
/// The depth is `frames.len()` at the `enter-scope`, so a frame that is
/// popped can be asked what it had open: everything entered at a depth
/// greater than the frames that remain.
struct OpenScope {
    depth: usize,
    scope: Rc<TaskScope>,
}

/// Runs a lowered program.
///
/// One VM runs one body on one thread: the entry, or the body of a spawned
/// task. Everything shared with the rest of the run — the checked program,
/// the source map, the host boundary, the trace — is reached through the
/// [`Runtime`] it borrows, and the lowered program through the handle beside
/// it, which is what a `spawn` hands the thread it starts. That is the
/// arrangement [`crate::interp::Interpreter`] already has, with one thing
/// more, because this backend has a program of its own to reach.
pub struct Vm<'a> {
    runtime: &'a Runtime,
    hosts: &'a HostRegistry,
    /// The lowered program, held through the handle a task's thread is given
    /// a share of rather than as a bare reference.
    ///
    /// A `&Program` would say everything this VM needs and nothing the VM a
    /// spawned task builds needs: that one runs on a thread of its own, for
    /// as long as the task runs, which is not bounded by the frame that
    /// spawned it. An `Arc` is what outlives that frame, and it is sound to
    /// share because a `cove_ir::Program` is immutable once lowered and
    /// holds `Arc<str>` rather than `Rc<str>` for exactly this reason.
    program: &'a Arc<Program>,
    /// The run's sources, for the one diagnostic that quotes source text: a
    /// failing assertion names its condition in the words the test was
    /// written in. Read off the [`Runtime`] exactly as
    /// [`crate::interp::Interpreter`] reads it.
    sources: &'a SourceMap,
    /// One contiguous value stack shared by every frame: slots below,
    /// operands above.
    stack: Vec<Value>,
    /// The same arrangement for the slots and operands the checker proved
    /// are `Int` or `Bool`, as eight bytes each and with no tag and no
    /// destructor.
    ///
    /// Nothing in here is a GC root: a scalar is a number, and a number
    /// reaches nothing.
    scalars: Vec<i64>,
    /// The same arrangement again for the places a `var` parameter names:
    /// slots below, operands above, one window per frame.
    ///
    /// Nothing in here is a GC root either, and for a related reason: a
    /// place holds an index into the value stack, so whatever it reaches is
    /// already reachable from the value stack's own window.
    places: Vec<Place>,
    frames: Vec<Frame>,
    /// One entry per constant, filled for the constants a `MakeStruct` names
    /// its type with and empty everywhere else.
    shapes: Vec<Option<StructShape>>,
    /// The same table for the enums a `MakeEnum` builds a case of.
    enums: Vec<Option<EnumShape>>,
    /// One entry per constant, as the [`Value`] that constant stands for.
    ///
    /// The pool holds a name as an `Arc<str>`, so that one lowered program
    /// can be read by every thread of a run, and a `Value::Str` holds an
    /// `Rc<str>`, because a value belongs to the task that built it. Turning
    /// one into the other is an allocation, and a constant is loaded as
    /// often as its instruction runs — so it is done once per VM, here,
    /// and every load after that is the `Rc` clone it always was.
    constants: Vec<Value>,
    /// This task's heap.
    ///
    /// ADR 0011: a value belongs to one task or is immutable and shared, so a
    /// task's objects are its own, and ADR 0008 gives each task a thread — so
    /// this heap is reached only from the thread that owns it, exactly as
    /// `Interpreter::heap` is. A spawned task's VM has one of its own, and
    /// [`Vm::retire_heap`] folds what it did into the run's totals when the
    /// thread ends.
    ///
    /// There is still no collection here. This backend has no collector, so
    /// what a heap it owns can report is what it allocated; a cycle a lowered
    /// program builds is not reclaimed, which is a gap in this backend rather
    /// than a property of the subset it covers.
    heap: Heap,
    /// Fuel charged since the last safepoint, spent at the next one.
    fuel: u64,
    /// How many instructions this VM has executed.
    ///
    /// Fuel cannot answer that question. An operation whose cost is not
    /// constant is charged proportionally — a copy by its size, a call by its
    /// arguments — so `fuel` counts work and not instructions, and the two
    /// diverge by exactly the amount that makes fuel a budget. This is the
    /// other number: how much of the program had to run. Wall time moves for
    /// many reasons and this moves for one, which is what makes it the figure
    /// a change to the lowering is judged by.
    instructions: u64,
    /// This task's own cancellation flag, when this VM is running a spawned
    /// task's body rather than the entry.
    ///
    /// Cancelling the *run* is the budget's flag, which every safepoint
    /// already observes through the shared budget. This is the second flag a
    /// safepoint checks: it stops one task without stopping the run, which is
    /// what leaving a scope early asks for.
    cancellation: Option<Cancellation>,
    /// The task this VM is running: the spawned task's id, or
    /// [`ENTRY_TASK`] when it is running the entry.
    ///
    /// This is the one answer to "which task" that every event naming a task
    /// is written from, exactly as `Interpreter::task_id` is.
    task: u64,
    /// The task scopes this VM has entered and not yet left, with the frame
    /// depth each was entered at.
    ///
    /// A scope's *value* is an ordinary slot, which is what `scope.spawn`
    /// reads. This is the other half, and it is the half that makes "leaving
    /// the scope waits for or cancels its child tasks" true however the scope
    /// is left. A `leave-scope` pops one from here; a `cancel-scope` pops one
    /// and cancels it, which is what a `break` needs; and a frame that
    /// returns out of the middle of a scope — through `return`, or through a
    /// `?` that failed — has whatever it opened cancelled by [`Vm::leave`],
    /// which is the one place a frame is popped.
    scopes: Vec<OpenScope>,
    /// Flags raised by a host call that bounds the work it was given, one for
    /// each such call this thread is inside, checked at every safepoint
    /// exactly as [`crate::interp::Interpreter`] checks them.
    ///
    /// [`Reentry::call_until`] is what pushes one, so this is empty until a
    /// host runs a Cove callback under a bound — which is something this
    /// backend can do now that closures lower, and could not before.
    stops: Vec<Cancellation>,
    /// Active timing contexts, one for the body this VM is running. A host
    /// call's wait is charged against every one, which is what lets
    /// `EntryExit` separate the work an entry did from the time it spent
    /// waiting for a host to answer.
    timings: Vec<Timing>,
    /// What the last finished run spent waiting on hosts, kept after its
    /// timing context ended so a caller can read it back.
    wait: Duration,
    /// Where the most recent assertion failed, and the message it produced.
    ///
    /// A failed assertion is an ordinary `Err`, which carries a message and
    /// no source position; this is the position, recorded by the one party
    /// that saw the assertion, so a test runner can point at it.
    assertion_failure: Option<(Span, String)>,
}

impl<'a> Vm<'a> {
    /// A VM for `program`, running against `runtime` and calling through
    /// `hosts`.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Arc<Program>) -> Self {
        Vm {
            runtime,
            hosts,
            program,
            sources: runtime.sources(),
            stack: Vec::new(),
            scalars: Vec::new(),
            places: Vec::new(),
            frames: Vec::new(),
            shapes: struct_shapes(runtime, program),
            enums: enum_shapes(runtime, program),
            constants: program.constants.iter().map(constant).collect(),
            heap: Heap::new(),
            fuel: 0,
            instructions: 0,
            cancellation: None,
            task: ENTRY_TASK,
            scopes: Vec::new(),
            stops: Vec::new(),
            timings: Vec::new(),
            wait: Duration::ZERO,
            assertion_failure: None,
        }
    }

    /// A VM for the body of the spawned task `id`, which stops when
    /// `cancellation` is raised.
    ///
    /// ADR 0008 gives each task an evaluator of its own, and this is the VM's
    /// side of that: the same two fields `Interpreter::for_task` sets, over
    /// the same `Runtime` and the same lowered program, on a thread of its
    /// own. Everything else — the stacks, the frames, the heap, the constant
    /// values — is built here rather than carried across, because none of it
    /// could cross and none of it needs to.
    pub(crate) fn for_task(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Arc<Program>,
        id: u64,
        cancellation: Cancellation,
    ) -> Self {
        let mut vm = Vm::new(runtime, hosts, program);
        vm.cancellation = Some(cancellation);
        vm.task = id;
        vm
    }

    /// Runs `function` with `args`, answering what the entry answered.
    ///
    /// The arguments are placed on the two operand stacks the way a `Call`
    /// would have placed them — each onto the stack its own parameter's slot
    /// kind names, in the order `params` gives — and become the first slots
    /// of the frame. It is done by hand here because there is no caller to
    /// have done it.
    ///
    /// The host speaks `Value`, because that is the language the embedding
    /// API is written in, so this is where an argument whose slot is a
    /// scalar one crosses. The entry's own answer crosses back at the other
    /// end of the same boundary: the return that has no caller.
    pub fn run(&mut self, function: FunctionId, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let entry = self.program.function(function);
        if args.len() as u32 != entry.arity {
            return Err(RuntimeError::new(format!(
                "`{}.{}` takes {} argument(s), but {} were given",
                entry.module,
                entry.name,
                entry.arity,
                args.len()
            ))
            .at(entry.span));
        }
        self.stack.clear();
        self.scalars.clear();
        self.places.clear();
        self.frames.clear();
        self.fuel = 0;
        for (kind, value) in entry.params.iter().zip(args) {
            match kind {
                SlotKind::Value => self.stack.push(value),
                SlotKind::Scalar(_) => self.scalars.push(promised_scalar(&value)),
                // There is no place for an embedder to hand over: a place
                // names a slot of this VM's own stack, and an entry is
                // called from outside it. `cove_ir::lower` emits none —
                // `var` is a property of a parameter and an entry declares
                // either none or one `Array<String>` — so this is a program
                // built by hand rather than one that was lowered.
                SlotKind::Place => {
                    return Err(RuntimeError::new(format!(
                        "`{}.{}` takes a `var` parameter, which an entry cannot be given",
                        entry.module, entry.name
                    ))
                    .at(entry.span))
                }
            }
        }
        self.stack
            .resize(entry.value_frame_size as usize, Value::Unit);
        self.scalars.resize(entry.scalar_frame_size as usize, 0);
        self.places
            .resize(entry.place_frame_size as usize, Place::rooted_at(0));
        self.frames.push(Frame {
            function,
            return_pc: 0,
            base: 0,
            scalar_base: 0,
            place_base: 0,
        });

        // The two events an entry is bracketed by are source-level, which is
        // the only kind ADR 0019 keeps on both backends: `cove trace` reads a
        // VM run and an interpreted run the same way, and an instruction-level
        // trace would be a different artifact.
        self.runtime.trace(TraceEvent::EntryEnter {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
        });
        self.timings.push(Timing::start());
        let outcome = self.execute(0);
        // A run that ended by raising abandoned its frames where they stood,
        // and a scope one of them had open still owns threads. The run is
        // ending, so what is left to do is what leaving a scope early does:
        // ask its children to stop, and wait for them.
        self.close_scopes_above(0);
        let timing = self
            .timings
            .pop()
            .expect("a run pushes exactly the one timing it pops");
        self.wait = timing.wait();
        self.runtime.trace(TraceEvent::EntryExit {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
            cpu: timing.cpu(),
            wait: timing.wait(),
        });
        outcome
    }

    /// Runs the entry `module.name` on the VM, handing it the process
    /// arguments, and reports how the run ended.
    ///
    /// This is the seam a backend is chosen at.
    /// [`crate::interp::Interpreter::run_entry`] takes the same three things
    /// and answers the same way, so selecting a backend selects which of the
    /// two to build and decides nothing else. Issue #111 gates the VM's
    /// adoption on the two agreeing, and two answers reached through
    /// differently shaped calls would be comparing the calls as well.
    ///
    /// A program the lowering refused never reaches here — ADR 0019's rule is
    /// that a VM run fails before any side effect — so the only entry this
    /// cannot find is one the caller named and the lowering did not emit.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_entry(module, name, args);
        let (classification, message) = match &outcome {
            // Cove's entry returns `Result<Unit, Error>`, so an `Err` is the
            // program saying what it was written to say, exactly as it is on
            // the interpreter: a failure of the program's work rather than of
            // the run.
            Ok(value) if value.is_err() => (RunOutcome::Error, returned_error_message(value)),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }

    /// The entry itself: finding it, and turning the process arguments into
    /// the one value it may take.
    ///
    /// What an entry may declare is the language's rule and not a backend's,
    /// so the shape checked here and the words it is refused in are
    /// [`crate::interp::Interpreter`]'s.
    fn invoke_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!("this package does not declare `{module}.{name}`"))
        })?;
        let entry = self.program.function(id);
        let arguments = match entry.arity {
            0 => Vec::new(),
            1 => vec![Value::Array(args.into_iter().map(Value::Str).collect())],
            other => {
                return Err(RuntimeError::new(format!(
                    "entry `{module}.{name}` declares {other} parameters"
                ))
                .at(entry.span)
                .with_rule(
                    "An entry function takes either no parameters or one `Array<String>` of process arguments.",
                )
                .with_help(format!(
                    "write `fn {name}()` or `fn {name}(args: Array<String>)`"
                )));
            }
        };
        self.run(id, arguments)
    }

    /// What this run allocated, and what the rest of the run allocated
    /// beside it.
    ///
    /// Read exactly as [`crate::interp::Interpreter::heap_stats`] reads it:
    /// the totals are the runtime's, because every retired task folded its
    /// own into them, and the live figures are this VM's, because its heap
    /// has not been retired yet.
    pub fn heap_stats(&self) -> HeapStats {
        let mut stats = self.runtime.heap_stats();
        let mine = self.heap.stats();
        stats.live_bytes = mine.live_bytes;
        stats.live_objects = mine.live_objects;
        stats
    }

    /// Where the most recent failed assertion was written, together with the
    /// message it produced, or `None` when no assertion has failed.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// How long the last finished run spent waiting on host calls.
    ///
    /// A host call's wait is not work the program did, so it is measured
    /// where it is spent and reported apart from the rest, exactly as
    /// [`Timing`] separates the two for the interpreter.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    /// How many instructions every run on this VM has executed between them.
    ///
    /// Cumulative rather than per-run, because a [`Vm`] runs one entry and a
    /// run is what a caller asks about; a caller that ran two would be asking
    /// about the pair.
    pub fn instructions(&self) -> u64 {
        self.instructions
    }

    // -------------------------------------------------------- the dispatch

    /// The loop, from the frame on top of the frame stack to the value it
    /// answers.
    ///
    /// The running function, its instructions, and its frame are held in
    /// locals rather than read back out of the frame stack on every
    /// instruction: they change only at a call and at a return, and reading
    /// them anywhere else is the re-derivation this backend exists to stop
    /// doing.
    ///
    /// `floor` is how many frames stood below the one this is to run, and it
    /// is what makes the loop re-entrant. A whole run is `floor = 0` and
    /// ends when the entry's frame is popped; a callback a host runs while
    /// this VM is inside a `call-host` is `floor = self.frames.len()` at the
    /// moment the host was called, and ends when *its* frame is popped —
    /// with the frames below it left standing, because the instruction that
    /// made the host call has not finished. See [`Vm::call_from_host`].
    fn execute(&mut self, floor: usize) -> Result<Value, RuntimeError> {
        let program: &'a Program = self.program;
        let mut frame = *self
            .frames
            .last()
            .expect("the caller pushes the frame this executes");
        let mut running = program.function(frame.function);
        let mut code: &[Inst] = &running.code;
        let mut blocks: &[u32] = &running.block_fuel;
        let mut pc = 0usize;

        // Entering a call is a safepoint and the entry is a call, so a run
        // that was cancelled before it began stops before its first
        // instruction — which is what `Interpreter::invoke` does for the
        // entry as well.
        self.safepoint(running.span)?;
        self.charge(blocks[0], || running.span_at(0))?;

        loop {
            let inst = code[pc];
            match inst {
                Inst::Const(id) => self.stack.push(self.constants[id.0 as usize].clone()),
                Inst::LoadLocal(slot) => {
                    let value = self.stack[frame.base + slot as usize].clone();
                    self.stack.push(value);
                }
                Inst::StoreLocal(slot) => {
                    let value = self.pop();
                    self.stack[frame.base + slot as usize] = value;
                }
                Inst::LoadCapture(index) => {
                    // A capture is a value slot: the call that entered this
                    // body copied it out of the closure into
                    // `arity + index`, which is where the captures stand
                    // because every argument of a closure travels on the
                    // value stack and so the parameters fill exactly
                    // `0..arity`. `cove_ir::lower::validate` checked both
                    // halves of that, so neither is asked about here.
                    let slot = frame.base + (running.arity + index) as usize;
                    let value = self.stack[slot].clone();
                    self.stack.push(value);
                }
                Inst::Pop => {
                    self.pop();
                }
                Inst::Dup => {
                    let value = self
                        .stack
                        .last()
                        .expect("`dup` has a value to copy")
                        .clone();
                    self.stack.push(value);
                }
                Inst::Unary(op) => {
                    let value = self.pop();
                    let answer = unary(unary_op(op), value, running.span_at(pc))?;
                    self.stack.push(answer);
                }
                Inst::Binary(op) => {
                    let rhs = self.pop();
                    let lhs = self.pop();
                    // Comparing two strings and comparing two integers are
                    // one instruction and not one cost, so the operand's own
                    // size is charged beside the instruction.
                    self.fuel += size_of_value(&lhs);
                    let answer = binary(binary_op(op), lhs, rhs, running.span_at(pc))?;
                    self.stack.push(answer);
                }
                Inst::IntBinary(op) => {
                    // No `size_of_value` beside the instruction: an `Int` is
                    // one word however large the number is, so this operator's
                    // cost is the constant fuel every instruction is charged
                    // and nothing proportional to what it was handed.
                    let rhs = self.pop_scalar();
                    let lhs = self.pop_scalar();
                    let answer = int_binary(op, lhs, rhs, running.span_at(pc))?;
                    self.scalars.push(answer);
                }
                Inst::ScalarConst(value) => self.scalars.push(value),
                Inst::LoadScalar(slot) => {
                    let value = self.scalars[frame.scalar_base + slot as usize];
                    self.scalars.push(value);
                }
                Inst::StoreScalar(slot) => {
                    let value = self.pop_scalar();
                    self.scalars[frame.scalar_base + slot as usize] = value;
                }
                Inst::ScalarPop => {
                    self.pop_scalar();
                }
                Inst::JumpIfFalseScalar(to) => {
                    // A scalar `Bool` is 0 or 1 and the lowering emitted this
                    // only where the checker settled one, so there is nothing
                    // to examine: the value *is* the answer.
                    let to = to as usize;
                    if self.pop_scalar() == 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::JumpIfTrueScalar(to) => {
                    // A scalar `Bool` is 0 or 1 and the lowering emitted this
                    // only where the checker settled one, so there is nothing
                    // to examine: the value *is* the answer.
                    let to = to as usize;
                    if self.pop_scalar() != 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::ScalarToValue(what) => {
                    let scalar = self.pop_scalar();
                    self.stack.push(match what {
                        Scalar::Int => Value::Int(scalar),
                        Scalar::Bool => Value::Bool(scalar != 0),
                    });
                }
                Inst::ValueToScalar => {
                    let value = self.pop();
                    self.scalars.push(promised_scalar(&value));
                }
                Inst::Jump(to) => {
                    let to = to as usize;
                    if to <= pc {
                        self.back_edge(running.span_at(pc))?;
                    }
                    self.charge(blocks[to], || running.span_at(to))?;
                    pc = to;
                    continue;
                }
                Inst::JumpIfFalse(to) | Inst::JumpIfTrue(to) => {
                    let taken_on = matches!(inst, Inst::JumpIfTrue(_));
                    let to = to as usize;
                    let test = self.pop();
                    let Value::Bool(test) = test else {
                        return Err(not_a_condition(&test, running.span_at(pc)));
                    };
                    if test == taken_on {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::Call {
                    function: target,
                    value_argc,
                    scalar_argc,
                    place_argc,
                    ..
                } => {
                    let span = running.span_at(pc);
                    let callee = program.function(target);
                    self.enter(callee, span)?;
                    // The callee's first block, charged where the callee is
                    // entered: the caller's own line ended at this `Call`, so
                    // nothing of the callee has been paid for yet.
                    self.charge(callee.block_fuel[0], || callee.span_at(0))?;
                    // Each stack's window opens where its own arguments
                    // begin, so those arguments *are* the callee's first
                    // slots in that stack and nothing is transferred: a
                    // scalar argument was pushed onto the scalar stack by
                    // the caller and is already the scalar slot it becomes.
                    // Resizing is what gives the rest of each window room,
                    // and a return truncates both back.
                    let base = self.stack.len() - value_argc as usize;
                    self.stack
                        .resize(base + callee.value_frame_size as usize, Value::Unit);
                    let scalar_base = self.scalars.len() - scalar_argc as usize;
                    self.scalars
                        .resize(scalar_base + callee.scalar_frame_size as usize, 0);
                    let place_base = self.places.len() - place_argc as usize;
                    // Only where the callee has a place window or was given
                    // a place. Almost no function has either, and `resize`
                    // on a `Vec<Place>` is a call with drop glue behind it
                    // rather than the two instructions the scalar stack's is
                    // — worth a compare to skip, in the arm every call runs.
                    if callee.place_frame_size as usize != place_argc as usize {
                        self.places.resize(
                            place_base + callee.place_frame_size as usize,
                            Place::rooted_at(0),
                        );
                    }
                    frame = Frame {
                        function: target,
                        return_pc: pc as u32 + 1,
                        base,
                        scalar_base,
                        place_base,
                    };
                    self.frames.push(frame);
                    running = callee;
                    code = &callee.code;
                    blocks = &callee.block_fuel;
                    pc = 0;
                    continue;
                }
                // Both closure instructions, out of line: see
                // `Vm::closure_inst` for why the bodies are not written
                // here. Only a call that opened a frame comes back with
                // one, and that is the only thing this arm can do that the
                // function cannot.
                Inst::MakeClosure { .. } | Inst::CallValue { .. } => {
                    if let Some(entered) = self.closure_inst(inst, pc, running.span_at(pc))? {
                        frame = entered;
                        running = program.function(frame.function);
                        code = &running.code;
                        blocks = &running.block_fuel;
                        self.charge(blocks[0], || running.span_at(0))?;
                        pc = 0;
                        continue;
                    }
                }
                // Both `dyn` instructions, out of line, for the reason the
                // two closure instructions above are: neither is on any
                // benchmark's path, and this `match` is on all of them. Only
                // a dispatch that opened a frame comes back with one.
                Inst::MakeDyn { .. } | Inst::CallDyn { .. } => {
                    if let Some(entered) = self.dyn_inst(inst, pc, running.span_at(pc))? {
                        frame = entered;
                        running = program.function(frame.function);
                        code = &running.code;
                        blocks = &running.block_fuel;
                        self.charge(blocks[0], || running.span_at(0))?;
                        pc = 0;
                        continue;
                    }
                }
                Inst::CallHost { module, op, argc } => {
                    let span = running.span_at(pc);
                    let module = name(program, module);
                    let op = name(program, op);
                    let values = self.take(argc as usize);
                    let value = self.call_host(module, op, values, span)?;
                    self.stack.push(value);
                }
                Inst::CallResource { op, argc } => {
                    let span = running.span_at(pc);
                    let op = name(program, op);
                    let values = self.take(argc as usize);
                    let receiver = self.pop();
                    // The receiver is not asked what it is: the lowering
                    // emitted this only where the checker settled the
                    // receiver as a host resource, so a handle standing here
                    // is an invariant of this backend rather than a fact to
                    // confirm.
                    let Value::Resource(handle) = &receiver else {
                        unreachable!(
                            "`call-resource` was emitted for a resource handle, and was handed a `{}`",
                            receiver.type_name()
                        );
                    };
                    let value = self.call_resource(handle, op, values, span)?;
                    // No `size_of_value`, unlike the builtin call below and
                    // like `Inst::CallHost` above: what a host call costs is
                    // charged inside `HostRegistry`, from the operation's own
                    // declaration, so pricing the answer again here would hold
                    // a VM run to more than an interpreted one is held to.
                    self.stack.push(value);
                }
                Inst::Snapshot => {
                    let span = running.span_at(pc);
                    let value = self.pop();
                    let copied = builtins::snapshot(self, &value, span)?;
                    // Priced like a builtin's answer, by `Inst::CallBuiltin`'s
                    // own rule and for its own reason: this is a call to
                    // `snapshot` written as an instruction rather than
                    // dispatched by name, so it costs what that call costs.
                    self.fuel += size_of_value(&copied);
                    self.stack.push(copied);
                }
                Inst::CallBuiltin { name: method, argc } => {
                    let span = running.span_at(pc);
                    let method = name(program, method);
                    let values = self.take(argc as usize);
                    let receiver = self.pop();
                    let value = builtins::call_method(self, &receiver, method, values, span)?;
                    // A builtin's cost follows what it produced: `chars()`
                    // builds one element per character, and charging for the
                    // instruction alone would price it like `length()`.
                    self.fuel += size_of_value(&value);
                    self.stack.push(value);
                }
                Inst::MakeArray(len) => {
                    let at = self.stack.len() - len as usize;
                    let items: Rc<[Value]> = self.stack.drain(at..).collect();
                    self.fuel += u64::from(len);
                    self.stack.push(Value::Array(items));
                }
                Inst::SpreadArgument => {
                    let span = running.span_at(pc);
                    let spread = self.pop();
                    let mut items: Vec<Value> = match self.pop() {
                        Value::Array(built) => built.to_vec(),
                        other => unreachable!(
                            "`spread-argument` appends to the array below it, and was handed a `{}`",
                            other.type_name()
                        ),
                    };
                    // The two `bind_params` reads and nothing else. A
                    // `Vector`'s elements are taken as it holds them now,
                    // which is the borrow the interpreter takes at the same
                    // moment.
                    match &spread {
                        Value::Array(values) => items.extend(values.iter().cloned()),
                        Value::Vector(storage) => {
                            items.extend(storage.elements.borrow().iter().cloned());
                        }
                        _ => return Err(builtins::spread_needs_a_sequence(span)),
                    }
                    self.fuel += items.len() as u64;
                    self.stack.push(Value::Array(items.into()));
                }
                Inst::MakeRange { inclusive_end } => {
                    // The end is above the start, because that is the order
                    // the lowering pushed them in and the order the source
                    // writes them in. `inclusive_end` is carried through
                    // rather than folded into the bounds: `0..<3` and `0..2`
                    // yield the same integers and are different values, and
                    // `Value::eq_value` and `Display` both read this flag.
                    let end = self.pop_scalar();
                    let start = self.pop_scalar();
                    self.stack.push(Value::Range {
                        start,
                        end,
                        inclusive_end,
                    });
                }
                Inst::Concat(parts) => {
                    let at = self.stack.len() - parts as usize;
                    let mut text = String::new();
                    for value in &self.stack[at..] {
                        // The same rendering interpolation uses, because it is
                        // the same operation: `Display` is what `"{x}"` means,
                        // and a literal part is a `Str` that renders as itself.
                        text.push_str(&value.to_string());
                    }
                    self.stack.truncate(at);
                    self.fuel += u64::from(parts) + text.len() as u64;
                    self.stack.push(Value::Str(text.into()));
                }
                Inst::MakeStruct { ty, .. } => {
                    let shape = self.shapes[ty.0 as usize]
                        .as_ref()
                        .expect("every `make-struct` names a type this VM shaped");
                    let width = shape.fields.len();
                    let at = self.stack.len() - width;
                    let fields: Vec<(Rc<str>, Value)> = shape
                        .fields
                        .iter()
                        .cloned()
                        .zip(self.stack.drain(at..))
                        .collect();
                    let value = Value::Struct(Rc::new(StructValue {
                        type_name: shape.type_name.clone(),
                        fields,
                        opaque: shape.opaque,
                    }));
                    self.fuel += width as u64;
                    self.stack.push(value);
                }
                Inst::GetField(field) => {
                    let span = running.span_at(pc);
                    let field = name(program, field);
                    let base_value = self.pop();
                    let Value::Struct(held) = &base_value else {
                        return Err(RuntimeError::new(format!(
                            "`{}` has no field `{field}`",
                            base_value.type_name()
                        ))
                        .at(span));
                    };
                    let Some(found) = held.get(field) else {
                        return Err(no_field(&held.type_name, field, span));
                    };
                    let found = found.clone();
                    self.stack.push(found);
                }
                Inst::GetFieldAt(index) => {
                    // Neither the type nor the position is asked about: the
                    // lowering emitted this only where the checker settled the
                    // receiver's type, and a struct's fields stand in
                    // declaration order wherever one is built, so both are
                    // invariants of this backend rather than facts to confirm.
                    let base_value = self.pop();
                    let Value::Struct(held) = &base_value else {
                        unreachable!(
                            "`get-field-at` was emitted for a struct, and was handed a `{}`",
                            base_value.type_name()
                        );
                    };
                    let found = held
                        .fields
                        .get(index as usize)
                        .expect("`get-field-at` names a field of the struct it was emitted for")
                        .1
                        .clone();
                    self.stack.push(found);
                }
                Inst::GetFieldAtScalar(index) => {
                    // The fusion of `Inst::GetFieldAt` with
                    // `Inst::ValueToScalar`: the same receiver, read by
                    // reference so that an `Int` or `Bool` field is converted
                    // rather than cloned onto the value stack just to be
                    // read back off it.
                    let base_value = self.pop();
                    let Value::Struct(held) = &base_value else {
                        unreachable!(
                            "`get-field-at-scalar` was emitted for a struct, and was handed a `{}`",
                            base_value.type_name()
                        );
                    };
                    let found = &held
                        .fields
                        .get(index as usize)
                        .expect(
                            "`get-field-at-scalar` names a field of the struct it was emitted for",
                        )
                        .1;
                    self.scalars.push(promised_scalar(found));
                }
                Inst::SetField(field) => {
                    let span = running.span_at(pc);
                    let field = name(program, field);
                    let value = self.pop();
                    let target = self.pop();
                    let Value::Struct(mut held) = target else {
                        return Err(not_a_struct(&target, field, span));
                    };
                    let type_name = held.type_name.clone();
                    // `make_mut` copies when another holder exists and does
                    // nothing when none does, which is what makes sharing a
                    // copied struct unobservable. It is the call
                    // `Place::with_mut` makes, for the same reason.
                    let Some(slot) = Rc::make_mut(&mut held).get_mut(field) else {
                        return Err(no_field(&type_name, field, span));
                    };
                    *slot = value;
                    self.fuel += held.fields.len() as u64;
                    self.stack.push(Value::Struct(held));
                }
                Inst::MakeBuiltin { name: which, argc } => {
                    let span = running.span_at(pc);
                    let which = name(program, which);
                    let values = self.take(argc as usize);
                    let value = self.make_builtin(which, values, running.arg_spans_at(pc), span)?;
                    self.stack.push(value);
                }
                Inst::MakeHostEnum { ty, case } => {
                    // The registry rather than the static schema, because
                    // that is what `Interpreter::eval_field` asks: an
                    // embedder's own host declares its types there, and a
                    // module the lowering saw a schema for may be one the
                    // run was never given.
                    let span = running.span_at(pc);
                    let qualified = name(program, ty);
                    let case = name(program, case);
                    let (module, short) = qualified
                        .rsplit_once('.')
                        .expect("`make-host-enum` names a type as `module.Name`");
                    let Some(declared) = self.hosts.host_type(module, short) else {
                        return Err(RuntimeError::new(format!(
                            "no host module `{module}` declares `{short}`"
                        ))
                        .at(span));
                    };
                    let value = crate::interp::host_enum_case(module, &declared, case, span)?;
                    self.stack.push(value);
                }
                Inst::MakeEnum { ty, case, argc } => {
                    let span = running.span_at(pc);
                    let case = name(program, case);
                    let payload = self.take(argc as usize);
                    let shape = self.enums[ty.0 as usize]
                        .as_ref()
                        .expect("every `make-enum` names an enum this VM shaped");
                    // The oracle's own constructor, so a case that does not
                    // exist and a payload of the wrong length are reported in
                    // the words `Interpreter::enum_case` reports them in.
                    let value = crate::interp::enum_case(
                        self.runtime.program(),
                        &shape.module,
                        &shape.decl,
                        case,
                        payload,
                        span,
                    )?;
                    // A case is built from its payload, so what it costs
                    // follows how much payload there was.
                    self.fuel += u64::from(argc);
                    self.stack.push(value);
                }
                Inst::CallBuiltinAssoc {
                    ty,
                    name: which,
                    argc,
                } => {
                    let span = running.span_at(pc);
                    let ty = name(program, ty);
                    let which = name(program, which);
                    let values = self.take(argc as usize);
                    let value = builtins::call_associated(self, ty, which, values, span)?;
                    // `Vector.of` and `Map.of` are variadic and build one
                    // element per argument, so the arguments are what the
                    // cost follows rather than the one instruction.
                    self.fuel += u64::from(argc);
                    self.stack.push(value);
                }
                // Every place instruction, out of line: see `Vm::place_inst`
                // for why the bodies are not written here.
                Inst::PlaceLocal(_)
                | Inst::LoadPlace(_)
                | Inst::PlaceField(_)
                | Inst::PlacePop
                | Inst::PlaceRead
                | Inst::PlaceWrite
                | Inst::Freeze => self.place_inst(inst, frame, running.span_at(pc))?,
                Inst::TestCase(case) => {
                    let case = name(program, case);
                    let subject = self.stack.last().expect("`test-case` has a value to test");
                    self.stack.push(Value::Bool(is_case(subject, case)));
                }
                Inst::GetPayload(index) => {
                    let span = running.span_at(pc);
                    let subject = self
                        .stack
                        .last()
                        .expect("`get-payload` has a value to read");
                    let Value::Enum(held) = subject else {
                        return Err(not_an_enum(subject, span));
                    };
                    let Some(found) = held.payload.get(index as usize).cloned() else {
                        return Err(no_payload(&held.case, held.payload.len(), index, span));
                    };
                    self.stack.push(found);
                }
                Inst::IterItems => {
                    let span = running.span_at(pc);
                    let value = self.pop();
                    // The oracle's own iteration, so a `Map` walks as the
                    // `MapEntry` of each pair, a `Set` in ascending order,
                    // and a value no `for` can walk is refused in the words
                    // `items_of` refuses it in.
                    let items = crate::interp::items_of(value, span)?;
                    // One element is one unit of work, so what the walk costs
                    // follows how many there were rather than the one
                    // instruction that asked.
                    self.fuel += items.len() as u64;
                    self.stack.push(Value::Array(items.into()));
                }
                // Six instructions in one arm, calling an `#[inline(never)]`
                // helper, for the reason the place and closure instructions
                // are: the dispatch body's footprint is a cost every program
                // pays, and no program in the benchmark suite executes any of
                // these.
                Inst::EnterScope(_)
                | Inst::LeaveScope
                | Inst::CancelScope
                | Inst::Spawn
                | Inst::Await
                | Inst::Cancel => self.task_inst(inst, running.span_at(pc))?,
                Inst::NoMatch => {
                    let span = running.span_at(pc);
                    let value = self.pop();
                    return Err(crate::interp::no_match(&value, span));
                }
                Inst::Try => {
                    let span = running.span_at(pc);
                    let value = self.pop();
                    match opened(value, span)? {
                        Ok(payload) => {
                            self.stack.push(payload);
                            // A `?` ends its line because it may leave the
                            // frame instead of falling through, so falling
                            // through arrives at the next head.
                            self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                        }
                        Err(failure) => {
                            self.safepoint(span)?;
                            match self.leave(Answered::Value(failure), floor) {
                                Answer::Done(value) => return Ok(value),
                                Answer::Caller(caller, resumed) => {
                                    frame = caller;
                                    running = program.function(frame.function);
                                    code = &running.code;
                                    blocks = &running.block_fuel;
                                    pc = resumed;
                                    // The caller resumes at the instruction
                                    // after its `Call`, which is a block head
                                    // for exactly that reason.
                                    self.charge(blocks[resumed], || running.span_at(resumed))?;
                                    continue;
                                }
                            }
                        }
                    }
                }
                Inst::Return => {
                    self.safepoint(running.span_at(pc))?;
                    let value = self.pop();
                    match self.leave(Answered::Value(value), floor) {
                        Answer::Done(value) => return Ok(value),
                        Answer::Caller(caller, resumed) => {
                            frame = caller;
                            running = program.function(frame.function);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = resumed;
                            // The caller resumes at the instruction after its
                            // `Call`, which is a block head for exactly that
                            // reason.
                            self.charge(blocks[resumed], || running.span_at(resumed))?;
                            continue;
                        }
                    }
                }
                Inst::ReturnScalar => {
                    // The same frame teardown, and the same safepoint,
                    // because a return is a return however its answer
                    // travels: only which stack the answer is taken off and
                    // put back on differs.
                    self.safepoint(running.span_at(pc))?;
                    let scalar = self.pop_scalar();
                    match self.leave(Answered::Scalar(scalar), floor) {
                        Answer::Done(value) => return Ok(value),
                        Answer::Caller(caller, resumed) => {
                            frame = caller;
                            running = program.function(frame.function);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = resumed;
                            // The caller resumes at the instruction after its
                            // `Call`, which is a block head for exactly that
                            // reason.
                            self.charge(blocks[resumed], || running.span_at(resumed))?;
                            continue;
                        }
                    }
                }
            }
            pc += 1;
        }
    }

    // ------------------------------------------------------- stack and frames

    /// The top of the operand stack.
    ///
    /// `cove_ir::lower::validate` simulated the depth of every instruction
    /// control can reach before the VM was handed the program, so an empty
    /// stack here is a broken invariant rather than a program that could be
    /// told about it.
    fn pop(&mut self) -> Value {
        self.stack
            .pop()
            .expect("a validated instruction takes only values that are there")
    }

    /// The top of the scalar stack.
    ///
    /// Empty here means the same thing an empty value stack means: a broken
    /// invariant of this backend, because `cove_ir::lower::validate`
    /// simulated both depths before the VM was handed the program.
    fn pop_scalar(&mut self) -> i64 {
        self.scalars
            .pop()
            .expect("a validated instruction takes only scalars that are there")
    }

    /// The seven instructions that read or write the place stack.
    ///
    /// Out of line, and not because they are long. `Vm::execute`'s `match`
    /// is the hottest code in this VM and `benches/arith` is small enough to
    /// feel its layout — the ablation study in `docs/VM_ARCHITECTURE.md`
    /// found `arith` moving several percent for changes that altered nothing
    /// it executes. Adding seven arms inline cost it about eight percent;
    /// collapsing them to one arm that calls this gave that back. No
    /// benchmark executes a single one of these, so nothing is paid for the
    /// call that is not paid by a program that uses places at all.
    #[inline(never)]
    fn place_inst(&mut self, inst: Inst, frame: Frame, span: Span) -> Result<(), RuntimeError> {
        match inst {
            Inst::PlaceLocal(slot) => {
                // Absolute, because a place travels into a call and the
                // callee's `base` is a different number — see `Place`.
                self.places
                    .push(Place::rooted_at(frame.base + slot as usize));
            }
            Inst::LoadPlace(slot) => {
                let place = self.places[frame.place_base + slot as usize].clone();
                self.places.push(place);
            }
            Inst::PlaceField(index) => {
                let refined = self
                    .places
                    .last()
                    .expect("`place-field` has a place to refine")
                    .field(index);
                // A path is copied rather than extended in place because
                // the place below may be the one a write consumes, and a
                // compound assignment builds both. `cove_ir::lower`
                // builds them separately for that reason, so this could
                // extend in place; it does not, so that the instruction
                // means the same thing wherever the lowering puts it.
                *self
                    .places
                    .last_mut()
                    .expect("`place-field` has a place to refine") = refined;
            }
            Inst::PlacePop => {
                self.pop_place();
            }
            Inst::PlaceRead => {
                let place = self.pop_place();
                // Walking a path is not constant work, so it is charged
                // where it happens; the clone below is not, for the
                // reason `Inst::LoadLocal`'s is not.
                self.fuel += place.path.len() as u64;
                // Reading a place clones: that is the value-semantics
                // rule, and it is `crate::interp::Place::read`'s comment.
                let value = self.place_ref(&place).clone();
                self.stack.push(value);
            }
            Inst::PlaceWrite => {
                let value = self.pop();
                let place = self.pop_place();
                self.fuel += place.path.len() as u64;
                *self.place_mut(&place) = value;
            }
            Inst::Freeze => {
                let place = self.pop_place();
                self.fuel += place.path.len() as u64;
                // Through the place, so that the uniqueness count sees
                // the caller's own handle exactly once: a read of the
                // receiver would be a second one. This is
                // `Interpreter::call_builtin_method`'s `freeze` arm, and
                // it calls the same function with the same handle.
                //
                // The receiver is asked what it is, unlike a typed
                // instruction's operand, because this instruction is
                // emitted from the *name* `freeze` rather than from a
                // settled receiver type — the same ground
                // `Inst::CallBuiltin` stands on. So a receiver that is
                // not a `Vector` is a program that fails, in the words
                // `builtins::call_method` fails in, and not a broken
                // invariant.
                let value = match self.place_mut(&place) {
                    Value::Vector(storage) => builtins::freeze(storage, span)?,
                    other => {
                        return Err(RuntimeError::new(format!(
                            "`{}` has no method `freeze`",
                            other.type_name()
                        ))
                        .at(span))
                    }
                };
                // `freeze` is O(1) in the storage it consumes and O(n)
                // in the `Array` it hands back, which is what
                // `Inst::CallBuiltin` charges a builtin's answer by.
                self.fuel += size_of_value(&value);
                self.stack.push(value);
            }
            other => unreachable!("`place_inst` was handed {other:?}"),
        }
        Ok(())
    }

    /// The top of the place stack.
    ///
    /// Empty here means what an empty value stack means: a broken invariant
    /// of this backend, because `cove_ir::lower::validate` simulated all
    /// three depths before the VM was handed the program.
    fn pop_place(&mut self) -> Place {
        self.places
            .pop()
            .expect("a validated instruction takes only places that are there")
    }

    /// What a place names, for reading.
    ///
    /// `crate::interp::Place::with_ref` walking a path of names, walked here
    /// as a path of positions. Neither the type nor the position is asked
    /// about, exactly as `Inst::GetFieldAt` asks about neither: a step is
    /// emitted only where the checker settled the type it is taken from, and
    /// a struct's fields stand in declaration order wherever one is built.
    fn place_ref(&self, place: &Place) -> &Value {
        let mut current = &self.stack[place.slot];
        for step in &place.path {
            let Value::Struct(held) = current else {
                unreachable!(
                    "a place step was emitted for a struct, and reached a `{}`",
                    current.type_name()
                );
            };
            current = &held
                .fields
                .get(*step as usize)
                .expect("a place step names a field of the struct it was emitted for")
                .1;
        }
        current
    }

    /// What a place names, for writing.
    ///
    /// `crate::interp::Place::with_mut`, and it has to stay that function:
    /// `Rc::make_mut` at every struct step is "the one place a struct's
    /// field is written, and so the one place its shared storage becomes
    /// private again", which is what makes sharing a copied struct
    /// unobservable. The same call at the same steps in the same order, or
    /// `is`, aliasing, and struct value semantics all change.
    fn place_mut(&mut self, place: &Place) -> &mut Value {
        let mut current = &mut self.stack[place.slot];
        for step in &place.path {
            let Value::Struct(held) = current else {
                unreachable!(
                    "a place step was emitted for a struct, and reached a `{}`",
                    current.type_name()
                );
            };
            current = &mut Rc::make_mut(held)
                .fields
                .get_mut(*step as usize)
                .expect("a place step names a field of the struct it was emitted for")
                .1;
        }
        current
    }

    /// The top `count` values, in the order they were pushed.
    fn take(&mut self, count: usize) -> Vec<Value> {
        let at = self.stack.len() - count;
        self.stack.drain(at..).collect()
    }

    // ----------------------------------------------------------- closures

    /// The two instructions that build a closure and enter one.
    ///
    /// Out of line, and not because they are long. `Vm::execute`'s `match`
    /// is the hottest code in this VM and `benches/field` is sensitive
    /// enough to feel its footprint — the place instructions cost `arith`
    /// about eight percent as seven inline arms and gave it back as one arm
    /// calling one function, which is the ablation study in
    /// `docs/VM_ARCHITECTURE.md` read from the other side. No benchmark
    /// executes either of these, so nothing is paid for the call that is not
    /// paid by a program that uses a closure at all.
    ///
    /// `Some` is the frame a call through a value opened, which is the one
    /// thing the caller has to do that this cannot: the running function,
    /// its code and its block table are locals of the loop.
    #[inline(never)]
    fn closure_inst(
        &mut self,
        inst: Inst,
        pc: usize,
        span: Span,
    ) -> Result<Option<Frame>, RuntimeError> {
        match inst {
            Inst::MakeClosure { function, captures } => {
                let value = self.close_over(function, captures);
                self.stack.push(value);
            }
            Inst::CallValue { argc } => {
                let callee = self.pop();
                match self.enter_value_call(&callee, argc, pc as u32 + 1, span)? {
                    // A callee that is not a lowered body answers without a
                    // frame: a bound host operation is a name the registry
                    // resolves, exactly as it is on the oracle.
                    Entered::Answer(value) => self.stack.push(value),
                    Entered::Frame(entered) => return Ok(Some(entered)),
                }
            }
            other => unreachable!("`closure_inst` was handed {other:?}"),
        }
        Ok(None)
    }

    /// Builds the closure `Inst::MakeClosure` names, over the top `captures`
    /// values.
    ///
    /// What it builds is a `Value::Closure` and not a variant of its own.
    /// Everything a host reads off a closure — the parameters it declares,
    /// the module it belongs to, what it captured — is the same fact
    /// whichever backend made it, and `cove_ir::Function` carries each of
    /// them for exactly this. Only the body differs, and
    /// [`crate::value::ClosureBody`] is where that difference is written
    /// down.
    fn close_over(&mut self, function: FunctionId, captures: u16) -> Value {
        let target = self.program.function(function);
        let at = self.stack.len() - captures as usize;
        // Paired with the names the lowering settled, in the order it
        // settled them, which is the order the values were pushed. The names
        // are copied rather than shared: the lowering holds one program for
        // every thread of a run and so writes an `Arc<str>`, and a closure is
        // a value of the task that built it and so holds an `Rc<str>`.
        let held: Vec<(Rc<str>, Value)> = target
            .captures
            .iter()
            .map(|name| Rc::from(&**name))
            .zip(self.stack.drain(at..))
            .collect();
        // A closure is built from what it captured, so what it costs follows
        // how much that was — the rule `Inst::MakeEnum` is charged by.
        self.fuel += u64::from(captures);
        Value::Closure(Rc::new(Closure {
            // `cove_ir::lower` refuses an `async` closure, so nothing builds
            // one here.
            is_async: false,
            params: target.written_params.clone(),
            // A lambda has no declaration, which is the same `None` the
            // interpreter's `make_closure` writes. The field is read for a
            // written return type, and a lowered body has already made that
            // conversion itself: `cove_ir::lower` emits a
            // `cove_ir::Inst::MakeDyn` before every return of a function
            // whose return type is written `dyn Trait`, so the answer that
            // comes back out is already the trait object the declaration
            // promised.
            decl: None,
            body: ClosureBody::Lowered(function),
            module: Rc::from(&*target.module),
            captures: held,
        }))
    }

    /// Opens the frame a call through a value enters, or answers the call
    /// where there is no frame to open.
    ///
    /// The arguments already stand on the value stack, `argc` of them, and
    /// the callee has been taken off the top — which is the arrangement
    /// `cove_ir::Inst::CallValue` describes and the reason the callee is
    /// pushed above its own arguments. So the arguments *are* the callee's
    /// first value slots, exactly as an ordinary `Call`'s are, and what is
    /// left is to copy the captures in behind them and give the rest of the
    /// frame room.
    ///
    /// The order of the checks is `Interpreter::invoke`'s, through
    /// [`Vm::enter`]: the depth limit, the host's own, and the safepoint
    /// every call is. The arity is checked first and in the interpreter's
    /// words, because `bind_params` reports it before it reaches any of
    /// those.
    #[inline(never)]
    fn enter_value_call(
        &mut self,
        callee: &Value,
        argc: u16,
        return_pc: u32,
        span: Span,
    ) -> Result<Entered, RuntimeError> {
        let closure = match callee {
            Value::Closure(closure) => closure,
            // A bound host operation is callable and is a name: the registry
            // resolves it, which is what `Interpreter::call_value_slots`
            // does with one. Nothing this backend lowers builds one — a host
            // operation used as a value is refused — so this is here for a
            // value a host handed back.
            Value::HostFn(host) => {
                let values = self.take(argc as usize);
                let (module, op) = (host.module.clone(), host.op.clone());
                return Ok(Entered::Answer(self.call_host(&module, &op, values, span)?));
            }
            other => {
                return Err(
                    RuntimeError::new(format!("`{}` is not callable", other.type_name())).at(span),
                )
            }
        };
        let ClosureBody::Lowered(target) = closure.body else {
            // The reverse of what `Interpreter::call_value_slots` says about
            // a lowered body, and unreachable for the same reason: a run has
            // one backend, and this one never builds a tree.
            return Err(RuntimeError::new(
                "this closure was built by the interpreter, and the VM runs lowered functions",
            )
            .at(span)
            .with_rule("A run has one backend, and a closure belongs to the run that made it."));
        };
        let callee = self.program.function(target);
        if callee.arity != u32::from(argc) {
            return Err(wrong_arity(callee, argc, span));
        }
        self.enter(callee, span)?;
        let base = self.stack.len() - argc as usize;
        for (_, value) in &closure.captures {
            self.stack.push(value.clone());
        }
        self.stack
            .resize(base + callee.value_frame_size as usize, Value::Unit);
        let scalar_base = self.scalars.len();
        self.scalars
            .resize(scalar_base + callee.scalar_frame_size as usize, 0);
        let place_base = self.places.len();
        // A closure has no place slots — a `var` parameter is refused on one
        // — so this is guarded the way the `Call` arm's is, and for the same
        // reason.
        if callee.place_frame_size > 0 {
            self.places.resize(
                place_base + callee.place_frame_size as usize,
                Place::rooted_at(0),
            );
        }
        let frame = Frame {
            function: target,
            return_pc,
            base,
            scalar_base,
            place_base,
        };
        self.frames.push(frame);
        Ok(Entered::Frame(frame))
    }

    /// The two instructions a `dyn Trait` value needs, out of the loop.
    ///
    /// `Some` is the frame a dispatch opened, which is the one thing the
    /// caller has to do that this cannot: the running function, its code and
    /// its block table are locals of the loop.
    #[inline(never)]
    fn dyn_inst(
        &mut self,
        inst: Inst,
        pc: usize,
        span: Span,
    ) -> Result<Option<Frame>, RuntimeError> {
        match inst {
            Inst::MakeDyn { trait_name, depth } => {
                let trait_name = self.shared_name(trait_name);
                let value = self.pop();
                let converted = self.make_dyn(value, &trait_name, depth);
                self.stack.push(converted);
            }
            Inst::CallDyn { site, argc } => {
                return self.enter_dyn_call(site, argc, pc as u32 + 1, span);
            }
            other => unreachable!("`dyn_inst` was handed {other:?}"),
        }
        Ok(None)
    }

    /// The six task instructions, off the dispatch loop's own body.
    ///
    /// Every decision any of them makes is `crate::task`'s, which is where
    /// the oracle makes the same one: a rule about what may cross a task
    /// boundary, about what a scope does with a child that failed, or about
    /// what a `spawn` charges is a rule about the language, and one stated
    /// twice is one that will drift. What is written here is the stack
    /// discipline and nothing else.
    ///
    /// None of these leaves the frame, which is why the arm above needs no
    /// tail. A `leave-scope` that found a failed child answers the `Err` that
    /// child produced, and the `try` the lowering writes after it is what
    /// returns it — `?` already means "return this failure from this call",
    /// and that is exactly what `Interpreter::leave_scope` does with one.
    #[inline(never)]
    fn task_inst(&mut self, inst: Inst, span: Span) -> Result<(), RuntimeError> {
        match inst {
            Inst::EnterScope(named) => {
                let scope = TaskScope::new(self.shared_name(named));
                self.scopes.push(OpenScope {
                    depth: self.frames.len(),
                    scope: Rc::clone(&scope),
                });
                self.stack.push(Value::TaskScope(scope));
            }
            Inst::LeaveScope => {
                let value = self.pop();
                let open = self
                    .scopes
                    .pop()
                    .expect("a validated `leave-scope` leaves a scope this frame entered");
                let answered = match task::wait_for_children(self, &open.scope) {
                    None => Value::ok(value),
                    Some(failure) => {
                        task::cancel_children(self, &open.scope);
                        match failure {
                            ChildFailure::Returned(value) => value,
                            ChildFailure::Raised(error) => {
                                open.scope.close();
                                return Err(error);
                            }
                        }
                    }
                };
                open.scope.close();
                self.stack.push(answered);
            }
            Inst::CancelScope => {
                let open = self
                    .scopes
                    .pop()
                    .expect("a validated `cancel-scope` leaves a scope this frame entered");
                task::cancel_children(self, &open.scope);
                open.scope.close();
            }
            Inst::Spawn => {
                let body = self.pop();
                let scope = self.pop();
                let Value::TaskScope(scope) = scope else {
                    unreachable!("a validated `spawn` stands on a task scope");
                };
                let program = Arc::clone(self.program);
                let spawned = task::spawn_into(
                    self,
                    &scope,
                    body,
                    span,
                    move |runtime, id, flag, body, span| {
                        run_task(&runtime, &program, id, flag, body, span)
                    },
                )?;
                self.stack.push(spawned);
            }
            Inst::Await => {
                // Every `await` is a safepoint, exactly as every call is:
                // `Interpreter::call_task_method` charges one before it
                // settles, and a task awaiting a task should notice a stop.
                self.safepoint(span)?;
                let value = self.pop();
                let Value::Task(handle) = value else {
                    unreachable!("a validated `await` stands on a task");
                };
                let settled = task::settle(self, &handle, span)?;
                self.stack.push(settled);
            }
            Inst::Cancel => {
                let value = self.pop();
                let Value::Task(handle) = value else {
                    unreachable!("a validated `cancel` stands on a task");
                };
                // Asking is all this does. A cancelled task stops at its next
                // safepoint, and whether it stopped or had already finished is
                // known only once something waits for it.
                handle.cancel();
                self.stack.push(Value::Unit);
            }
            other => unreachable!("`task_inst` was handed {other:?}"),
        }
        Ok(())
    }

    /// The conversion `cove_ir::Inst::MakeDyn` names: the trait object a
    /// written `dyn Trait` asks for, under however many `Array` or `Option`
    /// layers that type put between it and the value.
    ///
    /// The wrap itself and the step inwards are both `crate::interp`'s, so
    /// this is only the arithmetic that says how many steps to take — which
    /// is what the lowering already counted, out of the same written type
    /// `Interpreter::coerce` walks at run time.
    ///
    /// One wrapper is one allocation, so the conversion is charged by how
    /// many it made — the rule `Inst::MakeArray` and `Inst::MakeClosure` are
    /// charged by. The oracle charges nothing for the same conversion, which
    /// it can afford to because it makes it inside `bind_params` rather than
    /// at an instruction; ADR 0019 makes fuel backend-specific for exactly
    /// this kind of difference.
    /// A name an instruction carries, as the allocation this VM's own values
    /// share.
    ///
    /// A `dyn` value *carries* the trait's name, so every value one
    /// instruction converts should carry one `Rc` rather than a copy of the
    /// text. The pool's own `Arc<str>` cannot be that one — a `Value` is
    /// this task's and an `Arc` is the run's — so the `Rc` beside it in
    /// [`Vm::constants`] is, and this hands out a share of it.
    fn shared_name(&self, id: ConstId) -> Rc<str> {
        match &self.constants[id.0 as usize] {
            Value::Str(text) => text.clone(),
            other => unreachable!("an instruction named {other} rather than a name"),
        }
    }

    fn make_dyn(&mut self, value: Value, trait_name: &Rc<str>, depth: u16) -> Value {
        self.fuel += 1;
        match depth {
            0 => as_dyn(value, trait_name),
            _ => coerce_inside(value, |item| self.make_dyn(item, trait_name, depth - 1)),
        }
    }

    /// Opens the frame a call through a `dyn Trait` receiver enters, or
    /// answers the call where no implementation names the receiver's type.
    ///
    /// The receiver stands below the arguments and *is* the first of them,
    /// so the whole of `argc` is already in place as the callee's first
    /// value slots. What this does to the receiver is unwrap it, because the
    /// implementation runs on the concrete value and not on the wrapper —
    /// `crate::interp::dyn_receiver` is that step, and the interpreter takes
    /// the same one.
    ///
    /// The lookup is by the concrete type's name, which is the only thing
    /// that could have chosen between the candidates: the static type named
    /// the trait, and a trait has as many implementations as the package
    /// wrote `impl` blocks for it.
    ///
    /// A receiver whose type no candidate names is answered the way the
    /// oracle answers one, by falling through to `builtins::call_method` —
    /// which is where a receiver that reached no declaration falls in
    /// `Interpreter::eval_method_call`, and which reports `has no method` in
    /// its words. A checked program cannot get here, because the checker
    /// converts to `dyn Trait` only what conforms to it.
    #[inline(never)]
    fn enter_dyn_call(
        &mut self,
        site: DispatchId,
        argc: u16,
        return_pc: u32,
        span: Span,
    ) -> Result<Option<Frame>, RuntimeError> {
        let program = self.program;
        let dispatch = program.dispatch(site);
        let base = self.stack.len() - argc as usize;
        if let Some(concrete) = dyn_receiver(&self.stack[base]) {
            self.stack[base] = concrete;
        }
        let target = self.stack[base].declared_type_name().and_then(|type_name| {
            dispatch
                .cases
                .iter()
                .find(|(named, _)| **named == **type_name)
                .map(|(_, id)| *id)
        });
        let Some(target) = target else {
            let values = self.take(argc as usize - 1);
            let receiver = self.pop();
            let value = builtins::call_method(self, &receiver, &dispatch.method, values, span)?;
            self.fuel += size_of_value(&value);
            self.stack.push(value);
            return Ok(None);
        };
        let callee = program.function(target);
        if callee.arity != u32::from(argc) {
            return Err(wrong_arity(callee, argc, span));
        }
        self.enter(callee, span)?;
        self.stack
            .resize(base + callee.value_frame_size as usize, Value::Unit);
        let scalar_base = self.scalars.len();
        self.scalars
            .resize(scalar_base + callee.scalar_frame_size as usize, 0);
        let place_base = self.places.len();
        // A trait method reached through a `dyn` has no place slots — the
        // checker refuses `var self` through a trait object, and a `var`
        // parameter is refused on the value-stack convention — so this is
        // guarded the way the `Call` arm's is, and for the same reason.
        if callee.place_frame_size > 0 {
            self.places.resize(
                place_base + callee.place_frame_size as usize,
                Place::rooted_at(0),
            );
        }
        let frame = Frame {
            function: target,
            return_pc,
            base,
            scalar_base,
            place_base,
        };
        self.frames.push(frame);
        Ok(Some(frame))
    }

    /// Runs a Cove callable from outside the dispatch loop, which is what a
    /// host callback and a higher-order builtin both need.
    ///
    /// # The convention
    ///
    /// **The call opens its frame at the top of the three stacks as they
    /// stand, and leaves them exactly as it found them.** The arguments are
    /// pushed as a caller's would be, become the callee's first slots, and
    /// are truncated away by the return that answers; the answer is taken
    /// off and handed back in Rust rather than left standing. The frame
    /// stack grows above the frame the interrupted instruction belongs to
    /// and comes back down to it, which is what `floor` means in
    /// [`Vm::execute`].
    ///
    /// That the outer frames are *left* rather than unwound is the whole
    /// reason this is a second loop rather than a jump: the instruction that
    /// made the host call has not finished, its operands are on the stacks
    /// below, and its frame's slots are live. A place standing in one of
    /// those frames stays valid across this for the reason it stays valid
    /// across any call — it is an index, and nothing here truncates below
    /// where it points.
    ///
    /// # What a failure leaves
    ///
    /// Nothing. The outer run has no unwinding — an abandoned frame's slots
    /// stay on the stack until the run ends, which is sound because the run
    /// is ending — and that reasoning does not hold here: a host may catch
    /// what a callback failed with and carry on, `clock.timeout` being the
    /// one that does. So the three stacks and the frame stack are restored
    /// to what they were, and a host that continues continues onto the
    /// stacks it interrupted rather than onto ones the failure grew.
    ///
    /// # What is still accounted
    ///
    /// Everything the loop accounts, because it is the loop. Fuel is charged
    /// per block, the depth limit and the host's own `max_call_depth` are
    /// checked by [`Vm::enter`], and every safepoint the callee reaches asks
    /// what a safepoint asks — including `self.stops`, which
    /// [`Reentry::call_until`] pushes onto and which, until closures lowered,
    /// nothing could have raised while this backend ran Cove code.
    ///
    /// One safepoint is paid twice: [`Vm::enter`] takes one because a call
    /// is one, and [`Vm::execute`] takes one on entering the frame it was
    /// handed. A safepoint spends the fuel standing and asks the budget, so
    /// asking twice in a row costs a lock and changes no answer.
    fn call_from_host(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Ok(argc) = u16::try_from(args.len()) else {
            return Err(RuntimeError::new(format!(
                "a callback was given {} arguments",
                args.len()
            ))
            .at(span));
        };
        let values = self.stack.len();
        let scalars = self.scalars.len();
        let places = self.places.len();
        let floor = self.frames.len();
        let result = self.run_callback(callee, args, argc, span);
        if result.is_err() {
            // A host may catch what the callback failed with and carry on, so
            // a scope the callback opened and did not leave is left here
            // rather than at the end of the run: its threads would otherwise
            // outlive every frame that could name them.
            self.close_scopes_above(floor);
            self.frames.truncate(floor);
            self.stack.truncate(values);
            self.scalars.truncate(scalars);
            self.places.truncate(places);
        }
        result
    }

    /// [`Vm::call_from_host`] without the restoration, which is what makes
    /// the restoration one place rather than one per way out.
    fn run_callback(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        argc: u16,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let floor = self.frames.len();
        for value in args {
            self.stack.push(value);
        }
        // The resumption point is never read: `leave` answers at the floor
        // rather than resuming an instruction, because the instruction this
        // frame would return into is one no loop fetched.
        match self.enter_value_call(callee, argc, 0, span)? {
            Entered::Answer(value) => Ok(value),
            Entered::Frame(_) => self.execute(floor),
        }
    }

    /// Checks what a call is allowed to do before it does it.
    ///
    /// The three checks, in the order `Interpreter::invoke` makes them: the
    /// unconditional depth limit, the host's own `max_call_depth`, and the
    /// safepoint, because every call is one.
    fn enter(&mut self, callee: &Function, span: Span) -> Result<(), RuntimeError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                callee.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }
        let depth = self.frames.len() + 1;
        if let Some(Some(error)) =
            self.hosts
                .with_budget(|budget| match budget.limits().max_call_depth {
                    Some(limit) if depth > limit => {
                        Some(budget.to_runtime_error(Stopped::CallDepth))
                    }
                    _ => None,
                })
        {
            return Err(error.at(span));
        }
        self.safepoint(span)
    }

    /// Pops the running frame and hands `answer` back to whoever called it.
    ///
    /// All three windows are truncated to the frame's bases, which are where the
    /// caller's operands ended on each stack before it pushed the arguments,
    /// so the frame's slots and its arguments are discarded together — they
    /// are the same storage. The answer is then pushed onto whichever stack
    /// it came off, which is the one the caller's `Call` said to expect it
    /// on.
    ///
    /// The run itself ends in a `Value`, because that is what an embedder
    /// asked for and the scalar stack is an internal representation. So a
    /// scalar answer that has no caller becomes the `Value` it stands for
    /// here, at the outermost boundary there is.
    fn leave(&mut self, answer: Answered, floor: usize) -> Answer {
        let done = self.frames.pop().expect("a return leaves a frame");
        // A frame that returns out of the middle of a task scope — through a
        // `return`, or through a `?` that failed — never reaches the
        // `leave-scope` written after that scope's body, and leaving a scope
        // waits for or cancels its children whichever way it is left. This is
        // the one place a frame is popped, so it is the one place that can
        // notice. Guarded rather than called, because almost no program has a
        // scope open and every program returns.
        if !self.scopes.is_empty() {
            self.close_scopes_above(self.frames.len());
        }
        self.stack.truncate(done.base);
        self.scalars.truncate(done.scalar_base);
        // Guarded for the reason the `resize` in the `Call` arm is: a
        // truncation to the length the vector already has is free only once
        // the call to find that out has been made.
        if self.places.len() != done.place_base {
            self.places.truncate(done.place_base);
        }
        // Down to the floor is where this loop's work ends, whether or not
        // frames stand below it: a re-entrant run answers its caller in Rust
        // rather than resuming an instruction, because the instruction it
        // would resume is one this loop never fetched.
        let caller = (self.frames.len() > floor)
            .then(|| self.frames.last().copied())
            .flatten();
        match (caller, answer) {
            (Some(caller), Answered::Value(value)) => {
                self.stack.push(value);
                Answer::Caller(caller, done.return_pc as usize)
            }
            (Some(caller), Answered::Scalar(scalar)) => {
                self.scalars.push(scalar);
                Answer::Caller(caller, done.return_pc as usize)
            }
            (None, Answered::Value(value)) => Answer::Done(value),
            (None, Answered::Scalar(scalar)) => {
                let returns = self.program.function(done.function).returns;
                Answer::Done(as_value(returns, scalar))
            }
        }
    }

    /// Cancels and waits for every scope entered deeper than `depth`.
    ///
    /// What a scope left early does is `crate::task::cancel_children`, which
    /// is the same call `Interpreter::leave_scope` makes on its own early
    /// branch: every child is asked to stop first and waited for afterwards,
    /// so they stop at the same time rather than one after another.
    ///
    /// Called where a frame is popped, where a re-entrant call was abandoned,
    /// and once at the end of a run — the three ways a scope can be left
    /// without its `leave-scope` running.
    #[inline(never)]
    fn close_scopes_above(&mut self, depth: usize) {
        while self.scopes.last().is_some_and(|open| open.depth > depth) {
            let open = self.scopes.pop().expect("the loop just looked at one");
            task::cancel_children(self, &open.scope);
            open.scope.close();
        }
    }

    /// Ends this task's heap and folds what it did into the run's totals.
    ///
    /// `Interpreter::retire_heap` collects once first, because a `Weak` table
    /// dropped without a sweep takes nothing with it. There is nothing to
    /// collect here: this backend has no collector, so a heap it owns has
    /// never been swept and the counters are the whole of what it can report.
    fn retire_heap(&mut self) {
        let stats = self.heap.take_stats();
        self.runtime.retire_heap(&stats);
    }

    // ------------------------------------------------------------- budget

    /// Charges a whole basic block, on entering it.
    ///
    /// `block` is `cove_ir::Function::block_fuel` at the head this arrived
    /// at, which is how many instructions run from there before control can
    /// go somewhere else. Charging there rather than at each instruction is
    /// the same total over the same path — that line runs to its end, or the
    /// run ends — and it is what lets the dispatch loop carry no
    /// per-instruction bookkeeping at all.
    ///
    /// The forced safepoint moves with the charge for the same reason. What
    /// [`SAFEPOINT_INTERVAL`] now bounds is the fuel standing when a block is
    /// entered, so the work between two safepoints is that plus one straight
    /// line, and a straight line is bounded by the length of the function's
    /// code.
    ///
    /// The span is a closure because every call site has one to hand and none
    /// of them needs it: it is read only when the budget says stop, and
    /// `cove_ir::Function::span_at` is a bounds check and a twelve-byte copy
    /// that a taken jump should not pay per turn.
    #[inline]
    fn charge(&mut self, block: u32, span: impl FnOnce() -> Span) -> Result<(), RuntimeError> {
        let count = u64::from(block);
        self.instructions += count;
        self.fuel += count * INSTRUCTION_FUEL;
        if self.fuel >= SAFEPOINT_INTERVAL {
            self.safepoint(span())?;
        }
        Ok(())
    }

    /// Spends the fuel charged since the last safepoint, and checks the
    /// deadline, the run's cancellation, and every bounded call this thread
    /// is inside.
    ///
    /// This is `Interpreter::charge_safepoint` with the same budget and the
    /// same order of questions; only what is charged differs, because what
    /// this backend does between two safepoints is instructions rather than
    /// AST nodes. A stop surfaces as the ordinary [`RuntimeError`]
    /// [`crate::budget::Budget`] already produces, pointing at the
    /// instruction that reached the limit.
    ///
    /// # Where the safepoints are
    ///
    /// Every backward jump and every call, which are the interpreter's loop
    /// back edges and calls; every return, which is where the fuel of a body
    /// that made neither is finally spent; entering the entry, so a run
    /// cancelled before it began stops before its first instruction; and any
    /// block entered with [`SAFEPOINT_INTERVAL`] fuel already standing, so a
    /// long straight line is bounded too.
    /// A safepoint at a loop's back edge, taken once [`BACK_EDGE_FUEL`] has
    /// gathered.
    ///
    /// Everything a back edge asks is asked on that one schedule. It used to
    /// ask two questions on two: the stop flags this thread owns were read on
    /// *every* back edge, and only the run's shared budget waited for the fuel
    /// to gather. But a loop that takes two million back edges pays for the
    /// reading as well as for the lock, and the run's own cancellation was
    /// already on the gathered schedule — [`crate::budget::Budget::safepoint`]
    /// is where it is asked — so the eager read bought a tighter bound for one
    /// of the two stops and not for the other.
    ///
    /// So both wait together, and [`Vm::safepoint`] asks both. What that costs
    /// is granularity, and it is the granularity [`BACK_EDGE_FUEL`] already
    /// named: a loop notices a stop within that much fuel plus the one turn
    /// that crosses it, rather than at its next turn.
    fn back_edge(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.fuel >= BACK_EDGE_FUEL {
            self.safepoint(span)?;
        }
        Ok(())
    }

    fn safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        if let Some(cancellation) = &self.cancellation {
            if cancellation.is_cancelled() {
                return Err(crate::interp::task_cancelled(span));
            }
        }
        // A bounded call's flag stops only the body it bounds. The host that
        // raised it turns the stop into the answer it promised, so this need
        // only say that the body is not to continue.
        if self.stops.iter().any(Cancellation::is_cancelled) {
            return Err(work_stopped(span));
        }
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(Err(error)) = self.hosts.with_budget(|budget| {
            budget
                .safepoint(fuel)
                .map_err(|stopped| budget.to_runtime_error(stopped))
        }) {
            return Err(error.at(span));
        }
        Ok(())
    }

    /// Records `wait` against every active [`Timing`] context, so a trace can
    /// separate the work a body did from the time it spent waiting for a host
    /// to answer.
    fn charge_wait(&mut self, wait: Duration) {
        for timing in &mut self.timings {
            timing.add_wait(wait);
        }
    }

    // -------------------------------------------------------------- calls

    /// Dispatches a host call through the boundary both backends share, and
    /// records its wait.
    ///
    /// The grant check, the schema check on both sides, the budget charge,
    /// and the trace all live in [`HostRegistry`], so a VM run is held to
    /// exactly what an interpreted run is held to — this adds the timing and
    /// the span and nothing else.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let hosts = self.hosts;
        let started = Instant::now();
        let result = hosts.call_with(module, op, values, &mut Callback { vm: self, span });
        self.charge_wait(started.elapsed());
        result.map_err(|error| error.at(span))
    }

    /// Dispatches an operation on a resource handle, through the same
    /// boundary and with the same accounting as any other host call.
    ///
    /// [`Vm::call_host`] with the handle in place of the module name, for the
    /// reason `Inst::CallResource` exists: the handle is the address, and
    /// [`HostRegistry::call_resource`] reads the module, the resource kind,
    /// and whether that resource is still open out of it. The grant check,
    /// the schema checks, the budget charge, and the trace are that
    /// function's, exactly as for a call addressed to a module, so a stale
    /// handle fails here the way it fails on the interpreter.
    fn call_resource(
        &mut self,
        handle: &ResourceHandle,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let hosts = self.hosts;
        let started = Instant::now();
        let result = hosts.call_resource(handle, op, values, &mut Callback { vm: self, span });
        self.charge_wait(started.elapsed());
        result.map_err(|error| error.at(span))
    }

    /// `Ok`, `Err`, `Some`, `Error`, `None`, `assert`, and `assertEqual`.
    ///
    /// # How an assertion quotes its condition
    ///
    /// `assert` and `assertEqual` are builtins rather than library functions
    /// because their failure quotes the *source text* of the condition, in
    /// the words the test was written in. An instruction's own span covers
    /// the whole call, so the argument's span comes from
    /// [`cove_ir::Function::arg_spans`], which the lowering records for
    /// exactly these two; the text is then read out of the [`SourceMap`]
    /// through the interpreter's own [`source_text`], so a failure here is
    /// worded byte for byte as an interpreted one is.
    fn make_builtin(
        &mut self,
        which: &str,
        values: Vec<Value>,
        arg_spans: &[Span],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if which == NONE_CASE.name {
            // `None` is the one builtin case written as a bare name rather
            // than as a call, so it is a value and not a constructor.
            return Ok(Value::none());
        }
        if which == MAP_ENTRY.name {
            // The one builtin struct a program builds by calling its name.
            // `init_map_entry` puts the arguments in declaration order and
            // then fills the fields in that order; `arguments_in_order` did
            // the first half at lowering time, so what is left is the second.
            let fields: Vec<(Rc<str>, Value)> = MAP_ENTRY
                .fields
                .iter()
                .map(|field| Rc::from(field.name))
                .zip(values)
                .collect();
            return Ok(Value::Struct(Rc::new(StructValue {
                type_name: MAP_ENTRY.name.into(),
                fields,
                opaque: false,
            })));
        }
        let assertion =
            free_builtin(which).is_some_and(|schema| schema.kind == FreeBuiltinKind::Assertion);
        if !assertion {
            return builtins::call_constructor(which, values, span);
        }
        let sources: Vec<&str> = arg_spans
            .iter()
            .map(|span| source_text(self.sources, *span))
            .collect();
        let outcome = builtins::call_assertion(which, values, &sources, span)?;
        if let Some(payload) = outcome.err_payload() {
            self.assertion_failure = Some((span, payload[0].to_string()));
        }
        Ok(outcome)
    }
}

/// What a return did: ended the run, or handed control back to a caller at
/// the instruction after its `Call`.
enum Answer {
    Done(Value),
    Caller(Frame, usize),
}

/// What a call through a value did: opened a frame for the loop to run, or
/// answered without one.
///
/// The second is a bound host operation, which is a name the registry
/// resolves rather than a body with a frame. Both are callable values in the
/// language, so both arrive at [`Vm::enter_value_call`], and only one of
/// them is something to continue *into*.
enum Entered {
    Frame(Frame),
    Answer(Value),
}

/// A callable value was given the wrong number of arguments.
///
/// In `bind_params`'s words, because that is where the interpreter notices:
/// a callee that was given more than it declares is told so by
/// `assign_labels`, and one that was given too few is told which parameter
/// went unfilled. A checked program has neither — the checker settles a
/// closure's arity at the call — so what reaches this is a host that called
/// a callback with a list of its own choosing.
fn wrong_arity(callee: &Function, argc: u16, span: Span) -> RuntimeError {
    if u32::from(argc) > callee.arity {
        return RuntimeError::new(format!(
            "`this closure` takes {} argument(s), but more were given",
            callee.arity
        ))
        .at(span);
    }
    let missing = callee.written_params.get(argc as usize).map_or_else(
        || format!("argument {}", argc + 1),
        |param| param.name.node.clone(),
    );
    RuntimeError::new(format!("`this closure` needs an argument for `{missing}`")).at(span)
}

/// The answer a return is carrying, and which stack it came off.
///
/// The two are not interchangeable and nothing at run time could tell them
/// apart once the word is on the scalar stack, so the distinction is made
/// where it is still known: at the instruction that read it. A caller resumes
/// expecting the answer on the stack its `Call` named, and that name came
/// from the callee's own `returns`, so the two agree by construction —
/// `cove_ir::lower::validate` is where they were made to.
enum Answered {
    Value(Value),
    Scalar(i64),
}

/// `expr?`: the payload, or the failure to return from this call.
///
/// The rule is the interpreter's, for both of the types it is defined over: a
/// `Result` answers its `Ok` payload or returns the whole `Err`, and an
/// `Option` answers its `Some` payload or returns `None`. An empty payload
/// answers `()`, because the schema says one is carried and a host that broke
/// its word is not a reason to lose the shape of the answer.
fn opened(value: Value, span: Span) -> Result<Result<Value, Value>, RuntimeError> {
    match &value {
        Value::Enum(result) if &*result.type_name == RESULT.name => Ok(match value.ok_payload() {
            Some(payload) => Ok(payload.first().cloned().unwrap_or(Value::Unit)),
            None => Err(value),
        }),
        Value::Enum(option) if &*option.type_name == OPTION.name => {
            Ok(match value.some_payload() {
                Some(payload) => Ok(payload.first().cloned().unwrap_or(Value::Unit)),
                None => Err(Value::none()),
            })
        }
        other => Err(RuntimeError::new(format!(
            "`?` needs a `Result` or an `Option`, but found `{}`",
            other.type_name()
        ))
        .at(span)
        .with_rule("`expr?` returns the error from the current function.")),
    }
}

/// A conditional jump was handed something that is not a `Bool`.
///
/// The sentence is `cove-sema`'s rather than the interpreter's, which has one
/// for an `if`, one for a `while`, and one for `&&`. A jump does not say
/// which of the three it was lowered from, and the checker refuses a
/// non-`Bool` condition before any of them is lowered, so this is what a
/// program that reached here would have been told first.
fn not_a_condition(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "a condition must be a `Bool`, but found `{}`",
        value.type_name()
    ))
    .at(span)
    .with_rule("There are no implicit boolean conversions.")
}

/// One constant as the value it stands for.
fn constant(held: &Const) -> Value {
    match held {
        Const::Unit => Value::Unit,
        Const::Bool(value) => Value::Bool(*value),
        Const::Int(value) => Value::Int(*value),
        Const::Float(value) => Value::Float(*value),
        Const::Duration(value) => Value::Duration(*value),
        // The pool holds its text as an `Arc<str>`, because one lowered
        // program is read by every thread of a run, and a `Value::Str` holds
        // an `Rc<str>`, because a value belongs to the task that built it.
        // This is where the one becomes the other, and [`Vm::constants`] is
        // why it happens once per VM rather than once per load.
        Const::Str(text) => Value::Str(Rc::from(&**text)),
        // A name is carried by an instruction that already knows what to do
        // with it, and nothing loads one as a value. It is still a string, so
        // there is nothing to invent if something ever does.
        Const::Name(text) => Value::Str(Rc::from(&**text)),
    }
}

/// The text an instruction's constant names.
fn name(program: &Program, id: ConstId) -> &str {
    match program.constant(id) {
        Const::Name(text) | Const::Str(text) => text,
        other => unreachable!("an instruction named {other:?} rather than a name"),
    }
}

/// The IR's unary operator as the interpreter's.
fn unary_op(op: IrUnary) -> UnaryOp {
    match op {
        IrUnary::Not => UnaryOp::Not,
        IrUnary::Neg => UnaryOp::Neg,
    }
}

/// The IR's binary operator as the interpreter's.
///
/// `&&` and `||` have no IR operator: they short-circuit, so they lower to a
/// jump, and there is nothing here for them to be.
fn binary_op(op: IrBinary) -> BinaryOp {
    match op {
        IrBinary::Add => BinaryOp::Add,
        IrBinary::Sub => BinaryOp::Sub,
        IrBinary::Mul => BinaryOp::Mul,
        IrBinary::Div => BinaryOp::Div,
        IrBinary::Rem => BinaryOp::Rem,
        IrBinary::Eq => BinaryOp::Eq,
        IrBinary::Ne => BinaryOp::Ne,
        IrBinary::Lt => BinaryOp::Lt,
        IrBinary::Le => BinaryOp::Le,
        IrBinary::Gt => BinaryOp::Gt,
        IrBinary::Ge => BinaryOp::Ge,
        IrBinary::Is => BinaryOp::Is,
    }
}

/// The `Int` or `Bool` an [`Inst::ValueToScalar`] or [`Inst::GetFieldAtScalar`]
/// was promised, as the word the scalar stack keeps it as.
///
/// The lowering emits either only where the checker settled the value as one
/// of those two, so anything else arriving here is a broken invariant of this
/// backend and not a program that could be told about it — the same standing
/// as an operand stack that came up empty. These are the only instructions
/// that look at a `Value`'s tag on the way onto the scalar stack, and they
/// look because they are the boundary: everything above it there is a word
/// with no tag at all. Taking `value` by reference is what lets
/// `Inst::GetFieldAtScalar` read a field out of a struct it does not own
/// without cloning it first.
fn promised_scalar(value: &Value) -> i64 {
    match value {
        Value::Int(value) => *value,
        Value::Bool(value) => i64::from(*value),
        other => unreachable!(
            "a scalar was promised for an `Int` or a `Bool`, and was handed a `{}`",
            other.type_name()
        ),
    }
}

/// The `Value` a scalar answer stands for, at the one boundary a whole run
/// has: the return that has no caller.
///
/// Which of the two it is comes from `cove_ir::Function::returns` rather than
/// from beside the word, because the scalar stack carries no tag — that is
/// what it is for. `returns` named a scalar for every function whose returns
/// were lowered as [`Inst::ReturnScalar`], and `cove_ir::lower::validate`
/// refused a function where the two disagreed, so anything else arriving here
/// is a broken invariant of this backend and not a program that could be told
/// about it.
fn as_value(returns: SlotKind, scalar: i64) -> Value {
    match returns {
        SlotKind::Scalar(Scalar::Int) => Value::Int(scalar),
        SlotKind::Scalar(Scalar::Bool) => Value::Bool(scalar != 0),
        SlotKind::Place => unreachable!("no function answers a place; `validate` refuses one"),
        SlotKind::Value => unreachable!(
            "`return-scalar` was reached in a function that answers on the value stack"
        ),
    }
}

/// What [`Inst::IntBinary`] answers, and what it raises when `Int` has no
/// answer.
///
/// The failures are the interpreter's own, raised through the interpreter's
/// own helpers rather than restated here: an arithmetic operator that
/// overflowed, and division or remainder by zero, are rules of the language
/// and not rules of a backend, so each is one message however the operator
/// was reached. Specialising is allowed to change which instruction runs and
/// nothing about what it means.
///
/// `Div` and `Rem` test for zero before they ask, because `checked_div`
/// answers `None` for `i64::MIN / -1` and for `n / 0` alike and those are
/// two different failures with two different messages. `crate::interp::binary`
/// tests in that order for that reason, and so this does.
fn int_binary(op: IntOp, lhs: i64, rhs: i64, span: Span) -> Result<i64, RuntimeError> {
    Ok(match op {
        IntOp::Add => lhs
            .checked_add(rhs)
            .ok_or_else(|| overflow("addition", span))?,
        IntOp::Sub => lhs
            .checked_sub(rhs)
            .ok_or_else(|| overflow("subtraction", span))?,
        IntOp::Mul => lhs
            .checked_mul(rhs)
            .ok_or_else(|| overflow("multiplication", span))?,
        IntOp::Div => {
            if rhs == 0 {
                return Err(divide_by_zero("division", span));
            }
            lhs.checked_div(rhs)
                .ok_or_else(|| overflow("division", span))?
        }
        IntOp::Rem => {
            if rhs == 0 {
                return Err(divide_by_zero("remainder", span));
            }
            lhs.checked_rem(rhs)
                .ok_or_else(|| overflow("remainder", span))?
        }
        // A comparison answers a `Bool`, which is 0 or 1 in this stack.
        // `Inst::ScalarToValue` is what puts the tag back on where one is
        // wanted, and `Inst::JumpIfFalseScalar` is what reads it where none
        // is.
        IntOp::Eq => i64::from(lhs == rhs),
        IntOp::Ne => i64::from(lhs != rhs),
        IntOp::Lt => i64::from(lhs < rhs),
        IntOp::Le => i64::from(lhs <= rhs),
        IntOp::Gt => i64::from(lhs > rhs),
        IntOp::Ge => i64::from(lhs >= rhs),
    })
}

/// How big a value is, in the units the work over it is proportional to.
///
/// ADR 0019 asks that an operation whose cost is not constant — copying a
/// collection, comparing two strings, building a value proportional to its
/// input — is charged proportionally. This is that size, read in constant
/// time from what a value already knows about itself, so asking costs nothing
/// on the paths that ask.
fn size_of_value(value: &Value) -> u64 {
    match value {
        Value::Str(text) => text.len() as u64,
        Value::Array(items) => items.len() as u64,
        Value::Struct(held) => held.fields.len() as u64,
        Value::Enum(case) => case.payload.len() as u64,
        _ => 0,
    }
}

/// The shape of every struct the program builds, worked out once.
///
/// A `MakeStruct` names its type with one constant and its fields with
/// another, and the same type always carries the same pair, so one entry per
/// type constant is a complete table. Whether the type is opaque is the
/// checker's answer, read here rather than at every construction.
fn struct_shapes(runtime: &Runtime, program: &Program) -> Vec<Option<StructShape>> {
    let mut shapes: Vec<Option<StructShape>> = (0..program.constants.len()).map(|_| None).collect();
    for function in &program.functions {
        for inst in &function.code {
            let Inst::MakeStruct { ty, fields } = *inst else {
                continue;
            };
            if shapes[ty.0 as usize].is_some() {
                continue;
            }
            let type_name: Rc<str> = name(program, ty).into();
            let written = name(program, fields);
            let fields = if written.is_empty() {
                Vec::new()
            } else {
                written.split(',').map(Rc::<str>::from).collect()
            };
            let opaque = is_opaque(runtime, &type_name);
            shapes[ty.0 as usize] = Some(StructShape {
                type_name,
                fields,
                opaque,
            });
        }
    }
    shapes
}

/// Whether the module that declares `qualified` declared it `export opaque
/// struct`.
///
/// The checker is what knows, so it is asked, exactly as
/// `Interpreter::is_opaque` asks it.
fn is_opaque(runtime: &Runtime, qualified: &str) -> bool {
    let Some((module, name)) = qualified.rsplit_once('.') else {
        return false;
    };
    runtime
        .program()
        .modules
        .get(module)
        .and_then(|resolved| resolved.structs.get(name))
        .is_some_and(|entry| entry.opaque)
}

/// The declaration behind every enum the program builds a case of, worked
/// out once.
///
/// A `MakeEnum` the checker's tables have no enum for cannot arise — the
/// lowering read the name out of those same tables — so an absent entry is a
/// broken invariant rather than a program that could be told about it, and
/// the dispatch says so where it reads one.
fn enum_shapes(runtime: &Runtime, program: &Program) -> Vec<Option<EnumShape>> {
    let mut shapes: Vec<Option<EnumShape>> = (0..program.constants.len()).map(|_| None).collect();
    for function in &program.functions {
        for inst in &function.code {
            let Inst::MakeEnum { ty, .. } = *inst else {
                continue;
            };
            if shapes[ty.0 as usize].is_some() {
                continue;
            }
            let qualified = name(program, ty);
            let Some((module, short)) = qualified.rsplit_once('.') else {
                continue;
            };
            let Some(entry) = runtime
                .program()
                .modules
                .get(module)
                .and_then(|resolved| resolved.enums.get(short))
            else {
                continue;
            };
            shapes[ty.0 as usize] = Some(EnumShape {
                module: module.into(),
                decl: entry.decl.clone(),
            });
        }
    }
    shapes
}

/// Whether `value` is the enum case `tested` names.
///
/// `tested` is a case alone, or a type's short name and a case. The pair is
/// what `Interpreter::match_pattern` compares when a pattern wrote a path of
/// two or more segments: the case name always, and the short name of the
/// enum's own type as well, so that one enum's `Confirmed` does not match
/// another's. A value that is not an enum at all matches neither, which is
/// the `else` that pattern begins with.
fn is_case(value: &Value, tested: &str) -> bool {
    let Value::Enum(subject) = value else {
        return false;
    };
    let (expected_type, case) = match tested.rsplit_once('.') {
        Some((type_name, case)) => (Some(type_name), case),
        None => (None, tested),
    };
    if &*subject.case != case {
        return false;
    }
    match expected_type {
        // A declared enum carries `{module}.{Enum}` and a builtin carries its
        // name alone, so the short name is what the two have in common — and
        // it is what the pattern wrote.
        Some(expected) => {
            subject
                .type_name
                .rsplit('.')
                .next()
                .unwrap_or(&subject.type_name)
                == expected
        }
        None => true,
    }
}

/// A payload was asked of something that is not an enum.
///
/// Nothing a checked program can write reaches this: a `get-payload` is
/// emitted only after the `test-case` above it said the value is the case
/// this pattern binds out of. It is reported rather than assumed because the
/// VM reads what it is given, and a wrong answer would be read as a value.
fn not_an_enum(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "a payload was read from `{}`, which is not an enum",
        value.type_name()
    ))
    .at(span)
    .with_rule("A pattern binds out of the case it has already matched.")
}

/// A payload was asked for that the case does not carry.
fn no_payload(case: &str, carried: usize, index: u32, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "case `{case}` carries {carried} value(s), but value {index} was read"
    ))
    .at(span)
    .with_rule("A pattern binds out of the case it has already matched.")
}

/// The way back into a VM run that a host call was handed.
///
/// A host that runs a Cove closure enters the dispatch loop again on the
/// stacks as they stand — see [`Vm::call_from_host`], which is where the
/// convention for that is written down. The rest of the contract is what a
/// host that *waits* needs: it is standing where a safepoint would be, so it
/// is told what a safepoint would find.
struct Callback<'v, 'a> {
    vm: &'v mut Vm<'a>,
    /// Where the host call that is running this was written, so a failure
    /// inside it points at the call rather than at nothing.
    span: Span,
}

impl Reentry for Callback<'_, '_> {
    /// The callback, run on this VM, from inside the host call that was
    /// handed this.
    ///
    /// The span is the host call's own, so a failure inside the callback
    /// that has no span of its own points at the call that ran it rather
    /// than at nothing.
    fn call(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let span = self.span;
        self.vm.call_from_host(callee, args, span)
    }

    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        stop: &Cancellation,
    ) -> Result<Value, RuntimeError> {
        self.vm.stops.push(stop.clone());
        let result = self.call(callee, args);
        self.vm.stops.pop();
        result
    }

    /// Everything [`Vm::safepoint`] would stop on, asked from outside the
    /// loop: every bounded call this thread is inside, and the run's own
    /// cancellation.
    fn is_cancelled(&self) -> bool {
        if self.vm.stops.iter().any(Cancellation::is_cancelled) {
            return true;
        }
        self.vm
            .hosts
            .with_budget(|budget| budget.cancellation().is_cancelled())
            .unwrap_or(false)
    }

    /// What the run's deadline leaves, read from the one budget that knows
    /// when the run started.
    fn time_left(&self) -> Option<Duration> {
        self.vm.hosts.with_budget(|budget| {
            let deadline = budget.limits().deadline?;
            Some(deadline.saturating_sub(budget.elapsed()))
        })?
    }

    /// The task the boundary records the call against: this VM's own, which
    /// is the entry's when it is running the entry and the spawned task's
    /// when a `spawn` gave it a thread. A callback is on the same task as
    /// the instruction that called the host, because it is running on the
    /// same VM.
    fn task(&self) -> u64 {
        self.vm.task
    }
}

/// The VM's half of what a task needs, which is the same four questions the
/// interpreter answers. ADR 0008 gives every task an evaluator of its own,
/// and this is what makes a `Vm` one of them.
impl task::Tasking for Vm<'_> {
    fn runtime(&self) -> &Runtime {
        self.runtime
    }

    fn hosts(&self) -> &HostRegistry {
        self.hosts
    }

    fn charge_wait(&mut self, wait: Duration) {
        Vm::charge_wait(self, wait);
    }

    fn running_task(&self) -> Option<u64> {
        (self.task != ENTRY_TASK).then_some(self.task)
    }
}

/// Runs one spawned task's body on the thread `spawn` gave it.
///
/// This is `crate::interp`'s `run_task` with a `Vm` where the `Interpreter`
/// is, and the parts that are not the evaluator — the trace event, and the
/// form the value crosses back in — are `crate::task::finished`, so the two
/// backends cannot come to disagree about what a task's thread reports.
///
/// **What crossed is the program and the body, and nothing else.** The
/// program is the run's, shared: a `FunctionId` in the body's closure means a
/// position in it, so it has to be the same program and not a copy. The body
/// crossed as a `Transfer`, which is the task-safety rule applied. Everything
/// this VM works with is built here, on this thread: its three stacks, its
/// frames, its heap, and the values it turns the shared constant pool into.
fn run_task(
    runtime: &Runtime,
    program: &Arc<Program>,
    id: u64,
    cancellation: Cancellation,
    body: Transfer,
    span: Span,
) -> TaskOutcome {
    let mut vm = Vm::for_task(runtime, runtime.hosts(), program, id, cancellation.clone());
    vm.timings.push(Timing::start());
    let result = vm.call_from_host(&body.into_value(), Vec::new(), span);
    // A body that raised abandoned whatever it had open, and this thread is
    // ending, so the scopes go the way they go at the end of a run.
    vm.close_scopes_above(0);
    let timing = vm
        .timings
        .pop()
        .expect("a task pushes exactly the one timing it pops");
    // This task's heap ends with this thread. What it did joins the run's
    // totals before the value it produced crosses back.
    vm.retire_heap();
    task::finished(runtime, id, &cancellation, span, result, timing.cpu())
}

impl Callable for Vm<'_> {
    fn allocate_vector(&mut self, elements: Vec<Value>) -> Value {
        Value::Vector(self.heap.allocate(elements))
    }

    /// The half of `Snapshot` no conformance answers, and nothing else.
    ///
    /// The interpreter's own answer dispatches a struct or an enum to its
    /// `impl Snapshot for Type`, which is a whole Cove function; an
    /// instruction here cannot run one in the middle of itself, so this
    /// reports one instead. That is why `Body::method_call` emits
    /// `cove_ir::Inst::Snapshot` only where the checker settled a receiver
    /// type that no conformance can be reached through — the refusal is at
    /// lowering time, and this is the invariant it maintains rather than a
    /// second place the question is decided.
    fn snapshot(&mut self, value: &Value, span: Span) -> Result<Value, RuntimeError> {
        builtins::snapshot(self, value, span)
    }

    /// A higher-order builtin's callback — `Result.mapError`'s, today — run
    /// through the same re-entrant loop a host callback is.
    fn call_value(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.call_from_host(callee, args, span)
    }

    /// How many parameters a closure declares, read off the value.
    ///
    /// The interpreter's own answer, unchanged, and it stays correct for a
    /// closure this backend built because `cove_ir::Function::written_params`
    /// carries the parameters source wrote into the value. `mapError` reads
    /// this to decide whether to hand its callback the error it replaces, so
    /// a lowered closure that answered `0` where an interpreted one answered
    /// `1` would be the two backends disagreeing about what `mapError`
    /// passes.
    fn arity(&self, callee: &Value) -> Option<usize> {
        match callee {
            Value::Closure(closure) => Some(closure.params.len()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use cove_diag::{FileId, SourceMap};
    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};
    use cove_sema::resolve::Program as Checked;

    use crate::budget::{Budget, Limits};
    use crate::clock::{Clock, VirtualTime};
    use crate::files::Files;
    use crate::host::{Console, Env as EnvHost, Grants};
    use crate::http::Http;
    use crate::interp::Interpreter;

    /// Every capability a differential run is granted.
    ///
    /// The same set every time, because granting one is not what these tests
    /// are about: a program that calls no host is unaffected by holding the
    /// capability to, and a program that calls one is compared against an
    /// interpreted run holding exactly the same grants.
    const GRANTS: &[&str] = &["console", "clock", "env", "files", "http"];

    /// A `console` sink a test can read back.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("no test panics while printing")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("no test panics while printing")
                    .clone(),
            )
            .expect("console output is UTF-8")
        }
    }

    /// What one backend made of one program: the value or error it answered,
    /// and everything it wrote to `console`.
    ///
    /// The value is rendered rather than carried, because a [`Value`] is
    /// `Rc`-based and a run happens on a thread of its own.
    #[derive(Debug)]
    struct Outcome {
        answer: Result<String, RuntimeError>,
        output: String,
    }

    impl Outcome {
        fn value(&self) -> &str {
            match &self.answer {
                Ok(rendered) => rendered,
                Err(error) => panic!("the program ran without a runtime error: {error:?}"),
            }
        }

        fn error(&self) -> &RuntimeError {
            match &self.answer {
                Ok(rendered) => {
                    panic!("expected a runtime error, but the program answered {rendered}")
                }
                Err(error) => error,
            }
        }
    }

    /// One run's answer, rendered so it can leave the thread it happened on.
    fn described(answer: Result<Value, RuntimeError>) -> Result<String, RuntimeError> {
        answer.map(|value| format!("{value:?}"))
    }

    /// The hosts a differential run calls through: a `console` the test reads
    /// back, a clock whose virtual time never advances on its own, an `env`
    /// with nothing in it, and a `files` that is a map in this process.
    ///
    /// `files` is the one of them that issues resource handles — a `Reader`
    /// and a `Writer` — so it is what a test of `cove_ir::Inst::CallResource`
    /// calls through. Each run builds its own, because a run that writes a
    /// file must not be read back by the other backend's run of the same
    /// program.
    fn hosts(buffer: &Buffer, budget: Option<Budget>) -> Arc<HostRegistry> {
        let mut hosts = HostRegistry::new(Grants::new(GRANTS.to_vec()));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(EnvHost::new(BTreeMap::new())));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        hosts.register(Box::new(Files::in_memory(BTreeMap::new())));
        // Registered although nothing here reaches the network, because a
        // type a host module *declares* is asked of the registry rather than
        // of the static schema: `HostRegistry::host_type` answers `None` for
        // a module nobody registered, and the interpreter then reads
        // `http.Response(...)` as an operation and reports an unknown host
        // module. Every real runner registers every host, so a registry that
        // left one out would make this harness the only place the two
        // backends could differ about one.
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
        if let Some(budget) = budget {
            hosts.set_budget(budget);
        }
        Arc::new(hosts)
    }

    /// Runs `module.main` on the oracle.
    fn interpreted(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        budget: Option<Budget>,
    ) -> Outcome {
        let buffer = Buffer::default();
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts(&buffer, budget));
        let answer = Interpreter::new(&runtime).run_entry(module, "main", Vec::new());
        Outcome {
            answer: described(answer),
            output: buffer.text(),
        }
    }

    /// Lowers the program and runs `module.main` on the VM.
    ///
    /// The lowering and the validation happen here, inside the thread
    /// [`crate::on_cove_stack`] draws, because everything they are for is
    /// here: a `Vm`, its stacks, and every `Value` it builds belong to this
    /// thread. The program itself would cross — one is shared by every thread
    /// of a run, which is what lets a spawned task run one — and there is
    /// nothing to gain by lowering it on the other side of the boundary.
    fn lowered(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        budget: Option<Budget>,
    ) -> Outcome {
        let program = match cove_ir::lower::lower(checked) {
            Ok(program) => program,
            Err(why) => panic!("the program lowers, but stopped at {why}"),
        };
        cove_ir::lower::validate(&program)
            .unwrap_or_else(|why| panic!("the lowering holds the VM's invariants: {why}"));
        let entry = program
            .function_named(module, "main")
            .unwrap_or_else(|| panic!("`{module}.main` was lowered"));
        let buffer = Buffer::default();
        let hosts = hosts(&buffer, budget);
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
        let answer = Vm::new(&runtime, &hosts, &Arc::new(program)).run(entry, Vec::new());
        Outcome {
            answer: described(answer),
            output: buffer.text(),
        }
    }

    /// Runs one program on both backends, on a stack the runtime sized.
    ///
    /// Both runs happen inside one [`crate::on_cove_stack`] because the
    /// interpreter is a recursive tree walker and a test thread's stack is
    /// not one it chose; only the two rendered outcomes come back out.
    fn on_both(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        limits: Option<Limits>,
    ) -> (Outcome, Outcome) {
        crate::on_cove_stack(|| {
            let budget = || limits.clone().map(Budget::new);
            (
                interpreted(checked, sources, module, budget()),
                lowered(checked, sources, module, budget()),
            )
        })
        .expect("a thread to run Cove on")
    }

    /// Parses `source` as the single unit of module `m`.
    fn packaged(source: &str) -> (SourceMap, Package) {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("m/main.cove");
        let file = sources.add(path.clone(), source);
        let ast = match cove_syntax::parse_file(&sources, file) {
            Ok(ast) => ast,
            Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
        };
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
        (sources, package)
    }

    /// Parses and checks `source` the way `cove run` checks a package.
    ///
    /// Both halves of the check, because the lowering reads what the second
    /// one settled: a program that was only resolved carries no types, so
    /// every test here would run the untyped instructions and would prove
    /// nothing about the typed ones.
    fn checked_module(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
        let (sources, package) = packaged(source);
        match cove_sema::Compiler::new().compile(&package) {
            Ok(program) => (Arc::new(sources), Arc::new(program)),
            Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
        }
    }

    /// The same, resolved but not type-checked.
    ///
    /// Two failures below belong to the runtime and are unreachable through
    /// a checked program: a builtin method called with the wrong number of
    /// arguments is a diagnostic now, so a program holding one never reaches
    /// either backend. What both backends do with it is still worth pinning
    /// — an embedder may resolve without checking, and the two must not
    /// answer differently — so those tests are written against a program
    /// that skipped the half that would have refused it.
    fn resolved_module(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
        let (sources, package) = packaged(source);
        match cove_sema::resolve::resolve(&package) {
            Ok(program) => (Arc::new(sources), Arc::new(program)),
            Err(items) => panic!("the source resolves:\n{}", rendered(&sources, &items)),
        }
    }

    fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
        items
            .iter()
            .map(|item| cove_diag::render(sources, item))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **The differential test.** Runs `source` on both backends and asserts
    /// they agree about everything a program can be observed by: the value or
    /// the error it answered, and what it wrote to `console`.
    ///
    /// Every test here goes through this. ADR 0012 ranks the oracle above a
    /// backend, so a test that asserted only what the VM did would be a test
    /// of what somebody expected rather than of what Cove means.
    fn agree(source: &str) -> Outcome {
        agree_over(checked_module(source), source)
    }

    /// `agree`, over a program that was resolved and not checked.
    fn agree_unchecked(source: &str) -> Outcome {
        agree_over(resolved_module(source), source)
    }

    /// The comparison both of those make, over a program either one produced.
    fn agree_over(checked: (Arc<SourceMap>, Arc<Checked>), source: &str) -> Outcome {
        let (sources, checked) = checked;
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert_eq!(
            format!("{:?}", interpreted.answer),
            format!("{:?}", lowered.answer),
            "the two backends answered differently for:\n{source}"
        );
        assert_eq!(
            interpreted.output, lowered.output,
            "the two backends printed differently for:\n{source}"
        );
        lowered
    }

    /// `agree`, for a `main` written around `body` and returning `ty`.
    fn agree_main(ty: &str, body: &str) -> Outcome {
        agree(&format!(
            "use console.println\n\nexport fn main() -> {ty} {{\n{body}\n}}\n"
        ))
    }

    /// What both backends made of one expression, rendered as a `Value`.
    fn expression(ty: &str, expr: &str) -> String {
        agree_main(ty, &format!("  {expr}")).value().to_string()
    }

    /// What both backends made of a `main` written around `body`, with
    /// `items` declared beside it — which is what a test about closures
    /// needs, since a lambda is usually returned by or passed to something a
    /// module declares.
    fn value_of(ty: &str, items: &str, body: &str) -> String {
        agree(&format!(
            "use console.println\n\n{items}\nexport fn main() -> {ty} {{\n{body}\n}}\n"
        ))
        .value()
        .to_string()
    }

    /// The instructions `m.main` was lowered to, rendered.
    ///
    /// Which instruction ran is not something an outcome can show — that is
    /// the whole point of specialising — so a test that asserts the answer
    /// asserts the listing beside it. Otherwise a specialisation that
    /// stopped happening would go on passing every differential test there
    /// is.
    fn main_of(source: &str) -> String {
        let (_, checked) = checked_module(source);
        let program = cove_ir::lower::lower(&checked).expect("the program lowers");
        let id = program
            .function_named("m", "main")
            .expect("`m.main` was lowered");
        cove_ir::render(&program, id)
    }

    /// The message both backends refused one expression with, for an
    /// expression only the runtime refuses.
    ///
    /// Every other refusal in this file belongs to the checker now, so the
    /// program has to skip the half that would have caught it; see
    /// [`resolved_module`].
    fn refused_unchecked(ty: &str, expr: &str) -> String {
        agree_unchecked(&format!(
            "use console.println\n\nexport fn main() -> {ty} {{\n  {expr}\n}}\n"
        ))
        .error()
        .message
        .clone()
    }

    // -------------------------------------------------------- operators

    /// Every operator the IR carries, on every type the language defines it
    /// for.
    ///
    /// One test rather than one per operator, because what is being checked
    /// is the mapping from the IR's operator to the interpreter's: a
    /// `Sub` that reached `binary` as `Add` still answers a number, so only
    /// running every one of them against the oracle catches it.
    #[test]
    fn every_operator_answers_what_the_interpreter_answers() {
        let cases: &[(&str, &str, &str)] = &[
            ("Int", "7 + 5", "Int(12)"),
            ("Int", "7 - 5", "Int(2)"),
            ("Int", "7 * 5", "Int(35)"),
            ("Int", "7 / 5", "Int(1)"),
            ("Int", "7 % 5", "Int(2)"),
            ("Int", "-7", "Int(-7)"),
            ("Float", "7.5 + 0.25", "Float(7.75)"),
            ("Float", "7.5 - 0.25", "Float(7.25)"),
            ("Float", "7.5 * 2.0", "Float(15.0)"),
            ("Float", "7.5 / 2.0", "Float(3.75)"),
            ("Float", "7.5 % 2.0", "Float(1.5)"),
            ("Float", "-7.5", "Float(-7.5)"),
            ("Duration", "1ms + 500us", "Duration(1500000)"),
            ("Duration", "1ms - 500us", "Duration(500000)"),
            ("Duration", "-1ms", "Duration(-1000000)"),
            ("Bool", "7 == 7", "Bool(true)"),
            ("Bool", "7 != 7", "Bool(false)"),
            ("Bool", "7 < 5", "Bool(false)"),
            ("Bool", "7 <= 7", "Bool(true)"),
            ("Bool", "7 > 5", "Bool(true)"),
            ("Bool", "7 >= 8", "Bool(false)"),
            ("Bool", "\"a\" < \"b\"", "Bool(true)"),
            ("Bool", "\"a\" == \"a\"", "Bool(true)"),
            ("Bool", "1ms > 999us", "Bool(true)"),
            ("Bool", "0.5 <= 0.5", "Bool(true)"),
            ("Bool", "!true", "Bool(false)"),
            ("Bool", "true && false", "Bool(false)"),
            ("Bool", "true || false", "Bool(true)"),
            ("Bool", "[1, 2] == [1, 2]", "Bool(true)"),
        ];
        for (ty, expr, expected) in cases {
            assert_eq!(&expression(ty, expr), expected, "for `{expr}`");
        }
    }

    /// The failures arithmetic can have, in the words the interpreter reports
    /// them in.
    ///
    /// A mixed-type comparison is not here because no checked program has
    /// one: `cove-sema` refuses `1 == "a"` before either backend sees it, and
    /// so refuses every other operator applied across two types. What is left
    /// that a checked program can still do is overflow and divide by zero,
    /// and both are reported by the one `binary` both backends call.
    #[test]
    fn arithmetic_fails_the_way_the_interpreter_fails() {
        let most_negative = "  let least = -9223372036854775807 - 1\n";
        let cases: &[(&str, &str)] = &[
            (
                "  let big = 9223372036854775807\n  big + 1",
                "`Int` addition overflowed",
            ),
            (
                "  let least = -9223372036854775807 - 1\n  least - 1",
                "`Int` subtraction overflowed",
            ),
            (
                "  let big = 9223372036854775807\n  big * 2",
                "`Int` multiplication overflowed",
            ),
            ("  let zero = 0\n  1 / zero", "`Int` division by zero"),
            ("  let zero = 0\n  1 % zero", "`Int` remainder by zero"),
        ];
        for (body, message) in cases {
            assert_eq!(
                &agree_main("Int", body).error().message,
                message,
                "for:\n{body}"
            );
        }
        assert_eq!(
            agree_main("Int", &format!("{most_negative}  -least"))
                .error()
                .message,
            "`Int` negation overflowed"
        );
        assert_eq!(
            agree_main("Int", &format!("{most_negative}  least / -1"))
                .error()
                .message,
            "`Int` division overflowed"
        );
    }

    // ---------------------------------------- the instructions with a type

    /// Every operator the checker settles as `Int`, answered by the typed
    /// instruction and by the interpreter, message for message.
    ///
    /// The point of specialising is that nothing about the program changed,
    /// so the assertion is the same one every other test here makes: the
    /// oracle's answer. What is different is only which instruction produced
    /// it, and `an_int_operator_lowers_to_the_typed_instruction` is what
    /// pins that, because a specialisation that silently stopped happening
    /// would pass this test forever.
    #[test]
    fn every_int_operator_answers_what_the_interpreter_answers() {
        let cases: &[(&str, &str, &str)] = &[
            ("Int", "a + b", "Int(12)"),
            ("Int", "a - b", "Int(2)"),
            ("Int", "a * b", "Int(35)"),
            ("Int", "a / b", "Int(1)"),
            ("Int", "a % b", "Int(2)"),
            ("Bool", "a == b", "Bool(false)"),
            ("Bool", "a != b", "Bool(true)"),
            ("Bool", "a < b", "Bool(false)"),
            ("Bool", "a <= b", "Bool(false)"),
            ("Bool", "a > b", "Bool(true)"),
            ("Bool", "a >= b", "Bool(true)"),
        ];
        for (ty, expr, expected) in cases {
            let body = format!("  let a = 7\n  let b = 5\n  {expr}");
            assert_eq!(
                &agree_main(ty, &body).value().to_string(),
                expected,
                "for `{expr}`"
            );
            assert!(
                main_of(&format!("export fn main() -> {ty} {{\n{body}\n}}\n"))
                    .lines()
                    .any(|line| line.contains("  int ")),
                "`{expr}` lowers to the typed operator"
            );
        }
    }

    /// The failures `Int` has, raised by the typed instruction in the words
    /// the interpreter raises them in.
    ///
    /// Overflow at each of the three limits, and division and remainder by
    /// zero. `arithmetic_fails_the_way_the_interpreter_fails` asserts the
    /// same messages; this asserts them of the instruction that carries the
    /// type, which is a different instruction reaching the same helpers, and
    /// checks that it is the one that ran.
    #[test]
    fn the_typed_operator_fails_the_way_the_interpreter_fails() {
        let cases: &[(&str, &str)] = &[
            (
                "  let big = 9223372036854775807\n  let one = 1\n  big + one",
                "`Int` addition overflowed",
            ),
            (
                "  let least = -9223372036854775807 - 1\n  let one = 1\n  least - one",
                "`Int` subtraction overflowed",
            ),
            (
                "  let big = 9223372036854775807\n  let two = 2\n  big * two",
                "`Int` multiplication overflowed",
            ),
            (
                "  let least = -9223372036854775807 - 1\n  let minus = -1\n  least / minus",
                "`Int` division overflowed",
            ),
            (
                "  let one = 1\n  let zero = 0\n  one / zero",
                "`Int` division by zero",
            ),
            (
                "  let one = 1\n  let zero = 0\n  one % zero",
                "`Int` remainder by zero",
            ),
        ];
        for (body, message) in cases {
            assert_eq!(
                &agree_main("Int", body).error().message,
                message,
                "for:\n{body}"
            );
            let listing = main_of(&format!("export fn main() -> Int {{\n{body}\n}}\n"));
            assert!(
                listing.lines().any(|line| line.contains("  int ")),
                "the failure came from the typed operator:\n{listing}"
            );
        }
    }

    /// A `Float` operator is not an `Int` operator, so it keeps the untyped
    /// instruction and keeps agreeing.
    ///
    /// This is the other half of the rule, and the half a mistake would show
    /// in: specialising on a type the checker did not settle is how a backend
    /// starts answering a different program.
    #[test]
    fn float_arithmetic_keeps_the_untyped_operator_and_still_agrees() {
        let cases: &[(&str, &str, &str)] = &[
            ("Float", "a + b", "Float(7.75)"),
            ("Float", "a - b", "Float(7.25)"),
            ("Float", "a * b", "Float(1.875)"),
            ("Float", "a / b", "Float(30.0)"),
            ("Bool", "a > b", "Bool(true)"),
        ];
        for (ty, expr, expected) in cases {
            let body = format!("  let a = 7.5\n  let b = 0.25\n  {expr}");
            assert_eq!(
                &agree_main(ty, &body).value().to_string(),
                expected,
                "for `{expr}`"
            );
            let listing = main_of(&format!("export fn main() -> {ty} {{\n{body}\n}}\n"));
            assert!(
                listing.lines().any(|line| line.contains("  binary ")),
                "`{expr}` keeps the untyped operator:\n{listing}"
            );
            assert!(
                !listing.lines().any(|line| line.contains("  int ")),
                "`{expr}` is not integer arithmetic:\n{listing}"
            );
        }
    }

    /// A `Duration` is neither operand of an `Int` operator, and mixing one
    /// with an `Int` is the arithmetic the checker allows across two types.
    #[test]
    fn duration_arithmetic_keeps_the_untyped_operator_and_still_agrees() {
        let body = "  let a = 1ms\n  let b = 500us\n  a - b";
        assert_eq!(
            agree_main("Duration", body).value().to_string(),
            "Duration(500000)"
        );
        let listing = main_of(&format!("export fn main() -> Duration {{\n{body}\n}}\n"));
        assert!(
            !listing.lines().any(|line| line.contains("  int ")),
            "a `Duration` is not an `Int`:\n{listing}"
        );
    }

    /// A field read by position answers what the same read by name answers.
    ///
    /// Both programs are written, because the property is that the two are
    /// one program: the position is where the name stands, and a struct's
    /// fields stand in declaration order wherever one is built.
    #[test]
    fn a_field_read_by_position_answers_what_a_read_by_name_answers() {
        let source = "struct Point {\n  x: Int\n  y: Int\n}\n\n\
             export fn main() -> Int {\n\
             \x20 var p = Point(x: 3, y: 4)\n\
             \x20 p.y = p.y + p.x\n\
             \x20 p.x + p.y\n\
             }\n";
        assert_eq!(agree(source).value().to_string(), "Int(10)");
        let listing = main_of(source);
        // Both fields are `Int`, so a read of either is fused straight to
        // the scalar stack rather than stopping at `get-field-at`.
        assert!(
            listing
                .lines()
                .any(|line| line.contains("get-field-at-scalar 0"))
                && listing
                    .lines()
                    .any(|line| line.contains("get-field-at-scalar 1")),
            "both fields are read by position:\n{listing}"
        );
        assert!(
            !listing.lines().any(|line| line.contains("  get-field ")),
            "nothing is left reading by name:\n{listing}"
        );
    }

    /// A method a builtin type also names now lowers, because the checker
    /// recorded which of the two the call reaches.
    ///
    /// `Array` has a `length` and so does this `Box`, and both are called in
    /// one program. Until the lowering could read the checker's answer this
    /// refused to lower at all, so the assertion that matters is that there
    /// is an answer to compare.
    #[test]
    fn a_declared_method_a_builtin_also_names_lowers_and_agrees() {
        let source = "struct Box {\n  items: Array<Int>\n}\n\n\
             impl Box {\n\
             \x20 /// Doc.\n\
             \x20 fn length(self) -> Int {\n\
             \x20   99\n\
             \x20 }\n\
             }\n\n\
             export fn main() -> Int {\n\
             \x20 let b = Box(items: [1, 2, 3])\n\
             \x20 b.length() + [1, 2, 3].length()\n\
             }\n";
        // 99 from the declaration and 3 from the builtin: a call reaching the
        // wrong one of the two would answer 6 or 198 rather than fail.
        assert_eq!(agree(source).value().to_string(), "Int(102)");
        let listing = main_of(source);
        assert!(
            listing
                .lines()
                .any(|line| line.contains("call m.Box.length")),
            "the declared method is called:\n{listing}"
        );
        assert!(
            listing
                .lines()
                .any(|line| line.contains("call-builtin length")),
            "the builtin is called:\n{listing}"
        );
    }

    /// A failure carries the span of the instruction that raised it, so a
    /// diagnostic points at the same source it points at today.
    #[test]
    fn a_failure_points_at_the_operator_that_raised_it() {
        let source = "export fn main() -> Int {\n  let zero = 0\n  1 / zero\n}\n";
        let error = agree(source).error().clone();
        let span = error.span.expect("a runtime error points at source");
        assert_eq!(&source[span.start as usize..span.end as usize], "1 / zero");
    }

    // ----------------------------------------------------- control flow

    #[test]
    fn an_if_answers_the_branch_that_ran() {
        assert_eq!(expression("Int", "if 1 < 2 { 10 } else { 20 }"), "Int(10)");
        assert_eq!(expression("Int", "if 1 > 2 { 10 } else { 20 }"), "Int(20)");
        assert_eq!(
            expression("Unit", "if 1 < 2 {\n    let seen = 1\n  }"),
            "Unit"
        );
    }

    #[test]
    fn a_while_loop_counts_and_leaves() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  var i = 0\n  while i < 5 {\n    total += i\n    i += 1\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
    }

    /// A frame whose slots are in both stacks answers what the interpreter
    /// answers.
    ///
    /// `total` and `i` are `Int` and live in the scalar stack; `label` is a
    /// `String` and stays where every slot used to be. The two windows are
    /// numbered independently, so `label`'s value slot and `total`'s scalar
    /// slot can share a number without naming the same storage, which is
    /// what `cove_ir::lower::validate` proved and what this runs.
    #[test]
    fn a_frame_with_slots_in_both_stacks_answers_what_the_interpreter_answers() {
        assert_eq!(
            agree_main(
                "String",
                "  let label = \"n=\"\n  var total = 0\n  var i = 0\n  while i < 5 {\n    total += i\n    i += 1\n  }\n  \"{label}{total}\""
            )
            .value(),
            "Str(\"n=10\")"
        );
    }

    /// A `Bool` the checker settled is a scalar too, and the jump reads it
    /// where it stands.
    #[test]
    fn a_settled_bool_is_a_scalar_slot_and_a_condition_reads_it_there() {
        for (n, expected) in [(20, "Int(1)"), (2, "Int(2)")] {
            assert_eq!(
                agree_main(
                    "Int",
                    &format!("  let n = {n}\n  let big = n > 10\n  if big {{\n    1\n  }} else {{\n    2\n  }}")
                )
                .value(),
                expected
            );
        }
    }

    /// A `break` written inside a half-evaluated scalar expression takes what
    /// it left on the scalar stack with it.
    ///
    /// The loop's exit is reached at the depths the loop runs at, on both
    /// stacks: `total +` has already pushed `total`, so leaving without
    /// discarding it would reach the instruction after the loop one scalar
    /// deep and `validate` would have refused the function. This runs it.
    #[test]
    fn a_break_inside_a_half_evaluated_scalar_expression_leaves_nothing_behind() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  var i = 0\n  while i < 10 {\n    i += 1\n    total += if i == 3 {\n      break\n    } else {\n      i\n    }\n  }\n  total"
            )
            .value(),
            "Int(3)"
        );
    }

    /// Each call opens its own scalar window, so a recursion's scalar locals
    /// do not reach each other.
    #[test]
    fn recursion_gives_every_frame_its_own_scalar_slots() {
        assert_eq!(
            agree(
                "fn down(n: Int) -> Int {\n  let here = n * 2\n  if n <= 0 {\n    0\n  } else {\n    here + down(n - 1)\n  }\n}\n\nexport fn main() -> Int {\n  down(4)\n}\n"
            )
            .value(),
            "Int(20)"
        );
    }

    /// A `for` walks every collection the language has, and walks each one
    /// the way the interpreter does.
    ///
    /// All five are here rather than a sequence and a range, because the two
    /// that are not indexable are exactly the two an index walk was wrong
    /// about: a `Map` answers neither `length()` nor `get(i)`, and a `Set`
    /// answers `length()` but not `get(i)`. `iter-items` asks the oracle's
    /// own iteration what the loop walks, so what the VM walks is what the
    /// interpreter walks by construction, and these assert it.
    #[test]
    fn a_for_walks_every_collection_the_way_the_interpreter_walks_it() {
        // A range, which builds no value: the loop counts between its bounds.
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<5 {\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..5 {\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(15)"
        );
        // An `Array` and a `Vector`, the two sequences.
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for n in [3, 4, 5] {\n    total += n\n  }\n  total"
            )
            .value(),
            "Int(12)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for n in Vector.of(3, 4, 5) {\n    total += n\n  }\n  total"
            )
            .value(),
            "Int(12)"
        );
        // A `Set`, in ascending element order rather than in the order it was
        // written, which is why the elements are joined rather than added.
        assert_eq!(
            agree_main(
                "String",
                "  var joined = \"\"\n  for n in Set.of(3, 1, 2) {\n    joined = \"{joined}{n}\"\n  }\n  joined"
            )
            .value(),
            "Str(\"123\")"
        );
        // A `Map`, as the `MapEntry` of each pair in ascending key order. The
        // binding's `key` and `value` are read in the body, because that
        // shape is what the interpreter binds and a loop that bound anything
        // else would still count two iterations.
        assert_eq!(
            agree_main(
                "String",
                "  var pairs = \"\"\n  let ages = Map.of(MapEntry(key: \"b\", value: 2), MapEntry(key: \"a\", value: 1))\n  for entry in ages {\n    pairs = \"{pairs}{entry.key}={entry.value};\"\n  }\n  pairs"
            )
            .value(),
            "Str(\"a=1;b=2;\")"
        );
    }

    /// A `Range` used as a value, everywhere a value can be used.
    ///
    /// The oracle is `Interpreter::eval`'s `ExprKind::Range` arm, which
    /// evaluates a range like any other expression, and `agree_main` is what
    /// asserts against it. `inclusive_end` is what these are really about:
    /// it survives into `Display`, into `Value::eq_value`, and into
    /// `MapKey::Range`, so `0..<3` and `0..2` yield the same integers and
    /// are three different times not the same value.
    #[test]
    fn a_range_is_a_value_the_way_the_interpreter_makes_one() {
        // Rendered with the operator it was written with.
        assert_eq!(expression("String", "\"{0..<3}\""), "Str(\"0..<3\")");
        assert_eq!(expression("String", "\"{0..3}\""), "Str(\"0..3\")");
        assert_eq!(expression("String", "\"{-2..<-1}\""), "Str(\"-2..<-1\")");
        // Its own methods, which are `cove_schema::builtins::RANGE`'s and
        // reach the same `builtins::call_method` the interpreter reaches.
        assert_eq!(expression("Int", "(0..<3).length()"), "Int(3)");
        assert_eq!(expression("Int", "(0..3).length()"), "Int(4)");
        assert_eq!(expression("Bool", "(3..<3).isEmpty()"), "Bool(true)");
        assert_eq!(expression("Bool", "(0..<3).contains(3)"), "Bool(false)");
        assert_eq!(expression("Bool", "(0..3).contains(3)"), "Bool(true)");
        // Compared by the bounds it was written with, end included.
        assert_eq!(expression("Bool", "0..<3 == 0..<3"), "Bool(true)");
        assert_eq!(expression("Bool", "0..<3 == 0..3"), "Bool(false)");
        assert_eq!(expression("Bool", "0..<3 == 0..<2"), "Bool(false)");
        // Bound, passed, and iterated as the value it is rather than as a
        // header the loop took apart.
        assert_eq!(
            agree_main(
                "Int",
                "  let span = 0..<4\n  var total = 0\n  for i in span {\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(6)"
        );
        // And used as a `Map` key, which is where `MapKey::Range` orders by
        // the same flag.
        assert_eq!(
            expression(
                "Int",
                "Map.of(MapEntry(key: 0..<3, value: 1), MapEntry(key: 0..3, value: 2)).length()"
            ),
            "Int(2)"
        );
    }

    /// The instruction a range value lowers to, which no outcome can show.
    ///
    /// The bounds travel on the scalar stack because the checker settled
    /// both as `Int`, so the listing holds no `const` and no boundary
    /// instruction on the way in — see `cove_ir::Inst::MakeRange`.
    #[test]
    fn a_range_value_takes_its_bounds_off_the_scalar_stack() {
        let listed = main_of("export fn main() -> Range {\n  let n = 3\n  0..<n\n}\n");
        assert!(listed.contains("make-range ..<"), "{listed}");
        assert!(!listed.contains("value-to-scalar"), "{listed}");
        assert!(listed.contains("scalar-const 0"), "{listed}");
        assert!(listed.contains("load-scalar 0"), "{listed}");
    }

    /// An empty collection is walked zero times, whatever it is empty of.
    ///
    /// Zero is the length the loop's first test reads, so the body never
    /// runs and nothing is bound — and that has to hold for the collections
    /// whose emptiness `iter-items` reports as an empty `Array` rather than
    /// as a zero `length()`.
    #[test]
    fn an_empty_collection_is_walked_zero_times() {
        let cases: &[&str] = &["[]", "Vector.of()", "Set.of()", "Map.of()", "0..<0"];
        for iterable in cases {
            assert_eq!(
                agree_main(
                    "Int",
                    &format!(
                        "  var seen = 0\n  for item in {iterable} {{\n    seen += 1\n  }}\n  seen"
                    )
                )
                .value(),
                "Int(0)",
                "for `{iterable}`"
            );
        }
    }

    #[test]
    fn break_and_continue_leave_and_skip() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<10 {\n    if i == 5 {\n      break\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<10 {\n    if i % 2 == 0 {\n      continue\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(25)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  var i = 0\n  while true {\n    i += 1\n    if i > 3 {\n      break\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(6)"
        );
    }

    // --------------------------------- lowered for value, lowered for effect

    /// An `if`/`else` used as an expression answers the branch that ran.
    ///
    /// `cove_ir::lower` lowers an expression whose value nobody reads for its
    /// effect, and reaches inside a block, an `if`/`else`, and a `match` to do
    /// it. What those constructs *mean* is not allowed to change, so the
    /// oracle is asked: the same `if` is read into a `let`, nested as a
    /// block's tail, and written as the right-hand side of an assignment, and
    /// both backends have to agree about every one of them.
    #[test]
    fn an_if_else_used_as_an_expression_answers_what_the_interpreter_answers() {
        let source = "export fn main() -> Result<Unit, Error> {\n  let a = if 1 < 2 {\n    10\n  } else {\n    20\n  }\n  let b = {\n    if a == 10 {\n      a + 1\n    } else {\n      a - 1\n    }\n  }\n  var c = 0\n  if b == 11 {\n    c = if a == 10 {\n      5\n    } else {\n      6\n    }\n  }\n  let d = if a == 10 {\n    let ignored = 1\n  } else {\n    let ignored = 2\n  }\n  assertEqual(a, 10)?\n  assertEqual(b, 11)?\n  assertEqual(c, 5)?\n  assertEqual(d, ())?\n  Ok(())\n}\n";
        assert_eq!(
            agree(source).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    /// A `match` used as an expression answers the arm that ran, and one
    /// written as a statement still runs it.
    #[test]
    fn a_match_used_as_an_expression_answers_what_the_interpreter_answers() {
        let source = "enum Shape {\n  Dot\n  Line(Int)\n}\n\nexport fn main() -> Result<Unit, Error> {\n  let n = match Shape.Line(3) {\n    Shape.Dot => 0\n    Shape.Line(k) => k * 2\n  }\n  let m = {\n    match Shape.Dot {\n      Shape.Dot => n + 1\n      Shape.Line(k) => k\n    }\n  }\n  var seen = 0\n  match Shape.Line(5) {\n    Shape.Dot => seen = 1\n    Shape.Line(k) => seen = k\n  }\n  assertEqual(n, 6)?\n  assertEqual(m, 7)?\n  assertEqual(seen, 5)?\n  Ok(())\n}\n";
        assert_eq!(
            agree(source).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    /// A block used as an expression answers its tail, and a block with no
    /// tail answers `()`.
    #[test]
    fn a_block_used_as_an_expression_answers_what_the_interpreter_answers() {
        let source = "export fn main() -> Result<Unit, Error> {\n  let a = {\n    let x = 1\n    let y = 2\n    x + y\n  }\n  let b = {\n    let z = a\n  }\n  var t = 0\n  {\n    t = a * 2\n  }\n  assertEqual(a, 3)?\n  assertEqual(b, ())?\n  assertEqual(t, 6)?\n  Ok(())\n}\n";
        assert_eq!(
            agree(source).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    /// A statement lowered for its effect still does everything it did.
    ///
    /// Lowering for effect removes a value and never an operation, so the
    /// loops still count, the assignments still write, and the `if` with no
    /// `else` still takes the branch it was going to take. The oracle is what
    /// says so.
    #[test]
    fn a_statement_lowered_for_its_effect_still_runs_everything_in_it() {
        let source = "export fn main() -> Result<Unit, Error> {\n  var total = 0\n  var i = 0\n  while i < 10 {\n    if i % 3 == 0 {\n      total += i\n    }\n    i += 1\n  }\n  for j in 0..<4 {\n    total += j\n  }\n  assertEqual(total, 24)?\n  Ok(())\n}\n";
        assert_eq!(
            agree(source).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    #[test]
    fn an_early_return_ends_the_function_where_it_is_written() {
        let source = "fn first(items: Array<Int>) -> Int {\n  for n in items {\n    if n > 2 {\n      return n\n    }\n  }\n  0\n}\n\nexport fn main() -> Int {\n  first([1, 2, 3, 4])\n}\n";
        assert_eq!(agree(source).value(), "Int(3)");
    }

    // ---------------------------------------------------------- calls

    /// Recursion, deep enough that the frame stack has to be a stack.
    ///
    /// `fib(20)` is about 22,000 nested and sibling calls, which is the same
    /// workload `benches/pure` measures — enough that a frame layout that
    /// only worked one level deep could not answer it.
    #[test]
    fn recursion_answers_what_the_interpreter_answers() {
        let source = "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n\nexport fn main() -> Int {\n  fib(20)\n}\n";
        assert_eq!(agree(source).value(), "Int(6765)");
    }

    /// Recursion past the depth limit reports the limit rather than
    /// exhausting anything.
    #[test]
    fn unbounded_recursion_reports_the_depth_limit() {
        let source = "fn down(n: Int) -> Int {\n  down(n + 1)\n}\n\nexport fn main() -> Int {\n  down(0)\n}\n";
        assert_eq!(
            agree(source).error().message,
            format!("call depth limit of {MAX_CALL_DEPTH} reached while calling `down`")
        );
    }

    /// A variadic parameter, answered against the interpreter for every
    /// count of arguments a call can pass it.
    ///
    /// The oracle is `Interpreter::bind_params`, whose variadic arm builds
    /// `Value::Array` out of whatever `assign_labels` left in the
    /// parameter's slot and in `rest`, and declares it immutable. So a
    /// variadic parameter given nothing is an empty `Array` rather than a
    /// missing argument, and one given a label is the one element that label
    /// named.
    #[test]
    fn a_variadic_parameter_is_the_array_the_interpreter_binds() {
        let join = "fn join(sep: String, items: String...) -> String {\n  var text = \"\"\n  for item in items {\n    text = \"{text}{sep}{item}\"\n  }\n  \"{items.length()}{text}\"\n}\n\n";
        let cases: &[(&str, &str)] = &[
            ("join(\"-\", \"a\", \"b\", \"c\")", "Str(\"3-a-b-c\")"),
            ("join(\"-\", \"a\")", "Str(\"1-a\")"),
            ("join(\"-\")", "Str(\"0\")"),
            ("join(\"-\", items: \"a\")", "Str(\"1-a\")"),
            ("join(sep: \"-\", items: \"a\")", "Str(\"1-a\")"),
        ];
        for (call, expected) in cases {
            assert_eq!(
                agree(&format!(
                    "{join}export fn main() -> String {{\n  {call}\n}}\n"
                ))
                .value(),
                *expected,
                "for `{call}`"
            );
        }
    }

    /// A variadic parameter whose elements are `Int`, which is the case a
    /// signature read carelessly would get wrong.
    ///
    /// `record_signature` stores a variadic parameter as the element type it
    /// was written as, so `items: Int...` answers `Int` there while the body
    /// sees `Array<Int>`. The listing is asserted beside the answer because
    /// nothing about the answer would show which stack the slot was numbered
    /// in — the callee would load a word where an array was pushed, and the
    /// whole frame would be wrong from there.
    #[test]
    fn a_variadic_parameter_of_ints_arrives_as_an_array() {
        let source = "fn total(items: Int...) -> Int {\n  var sum = 0\n  for item in items {\n    sum += item\n  }\n  sum\n}\n\nexport fn main() -> Int {\n  total(1, 2, 3) + total()\n}\n";
        assert_eq!(agree(source).value(), "Int(6)");
        let listed = main_of(source);
        assert!(listed.contains("make-array 3"), "{listed}");
        assert!(listed.contains("make-array 0"), "{listed}");
        // One argument for one parameter, on the value stack, both times.
        assert_eq!(listed.matches("argc=1/0").count(), 2, "{listed}");
    }

    // -------------------------------------------------------- structs

    const CURSOR: &str = "struct Cursor {\n  at: Int\n  step: Int\n}\n\n";

    #[test]
    fn a_struct_is_built_read_and_written() {
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  let cursor = Cursor(at: 3, step: 2)\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(3)"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at = 9\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(9)"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at += cursor.step\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(5)"
        );
    }

    /// A struct is a value: writing a copy's field leaves the original alone.
    #[test]
    fn writing_a_copys_field_leaves_the_original_alone() {
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  let first = Cursor(at: 1, step: 1)\n  var second = first\n  second.at = 99\n  first.at\n}}\n"
            ))
            .value(),
            "Int(1)"
        );
    }

    #[test]
    fn a_method_takes_its_receiver_and_answers() {
        let source = format!(
            "{CURSOR}impl Cursor {{\n  fn position(self) -> Int {{\n    self.at\n  }}\n\n  fn ahead(self, by: Int) -> Int {{\n    self.at + by * self.step\n  }}\n}}\n\nexport fn main() -> Int {{\n  let cursor = Cursor(at: 4, step: 3)\n  cursor.position() + cursor.ahead(by: 2)\n}}\n"
        );
        assert_eq!(agree(&source).value(), "Int(14)");
    }

    /// An opaque struct renders as its name alone, on both backends.
    ///
    /// The IR carries the type's name and not whether it is opaque, so the VM
    /// asks the checker the same question `Interpreter::init_struct` asks.
    #[test]
    fn an_opaque_struct_renders_as_its_name() {
        let source = "export opaque struct Token {\n  secret: Int\n}\n\nexport fn main() -> String {\n  let token = Token(secret: 7)\n  \"{token}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"Token\")");
    }

    // ------------------------------------------------------- builtins

    #[test]
    fn builtin_methods_answer_what_the_interpreter_answers() {
        assert_eq!(expression("Int", "[1, 2, 3].length()"), "Int(3)");
        assert_eq!(expression("Int", "[1, 2, 3].get(1).unwrapOr(0)"), "Int(2)");
        assert_eq!(expression("Int", "[1, 2, 3].get(9).unwrapOr(0)"), "Int(0)");
        assert_eq!(expression("Int", "\"hello\".chars().length()"), "Int(5)");
        assert_eq!(
            expression("String", "\"hello\".chars().get(1).unwrapOr(\"\")"),
            "Str(\"e\")"
        );
        assert_eq!(expression("Int", "\"hello\".length()"), "Int(5)");
    }

    /// A builtin's own failure is the interpreter's, because it is the same
    /// call.
    #[test]
    fn a_builtin_fails_the_way_the_interpreter_fails() {
        assert_eq!(
            agree_main(
                "Int",
                "  let least = -9223372036854775807 - 1\n  least.abs()"
            )
            .error()
            .message,
            "`Int` abs overflowed"
        );
        assert_eq!(
            refused_unchecked("Int", "[1, 2, 3].get(1, 2).unwrapOr(0)"),
            "`Array.get` takes 1 argument(s), but 2 were given"
        );
    }

    // ----------------------------------------------- results and options

    #[test]
    fn the_builtin_constructors_build_what_the_interpreter_builds() {
        let source = concat!(
            "fn nothing() -> Option<Int> {\n  None\n}\n\n",
            "export fn main() -> String {\n",
            "  let good: Result<Int, Error> = Ok(1)\n",
            "  let bad: Result<Int, Error> = Err(Error(message: \"no\"))\n",
            "  let there = Some(2)\n",
            "  let boom = Error(message: \"boom\")\n",
            "  \"{good} {bad} {there} {nothing()} {boom}\"\n",
            "}\n"
        );
        assert_eq!(
            agree(source).value(),
            "Str(\"Ok(1) Err(no) Some(2) None boom\")"
        );
    }

    /// `?` on both of the types it is defined over, taking both paths.
    #[test]
    fn a_question_mark_opens_or_returns() {
        let ok = "fn answer() -> Result<Int, Error> {\n  Ok(7)\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
        assert_eq!(
            agree(ok).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(8)] })"
        );

        let err = "fn answer() -> Result<Int, Error> {\n  Err(Error(message: \"no\"))\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
        assert_eq!(
            agree(err).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Err\", payload: [Struct(StructValue { type_name: \"Error\", fields: [(\"message\", Str(\"no\"))], opaque: false })] })"
        );

        let some = "fn answer() -> Option<Int> {\n  Some(7)\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
        assert_eq!(
            agree(some).value(),
            "Enum(EnumValue { type_name: \"Option\", case: \"Some\", payload: [Int(8)] })"
        );

        let none = "fn answer() -> Option<Int> {\n  None\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
        assert_eq!(
            agree(none).value(),
            "Enum(EnumValue { type_name: \"Option\", case: \"None\", payload: [] })"
        );
    }

    // ------------------------------------------------------- rendering

    #[test]
    fn interpolation_renders_every_part_left_to_right() {
        assert_eq!(
            expression("String", "\"a{1 + 2}b{true}c\""),
            "Str(\"a3btruec\")"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> String {{\n  \"{{Cursor(at: 1, step: 2)}}\"\n}}\n"
            ))
            .value(),
            "Str(\"Cursor(at: 1, step: 2)\")"
        );
    }

    #[test]
    fn an_array_is_built_left_to_right() {
        assert_eq!(
            expression("String", "\"{[1 + 1, 2 + 2, 3 + 3]}\""),
            "Str(\"[2, 4, 6]\")"
        );
        assert_eq!(expression("Int", "[].length()"), "Int(0)");
    }

    // ------------------------------------------------------ assertions

    #[test]
    fn a_holding_assertion_answers_ok_on_both() {
        assert_eq!(
            agree_main(
                "Result<Unit, Error>",
                "  assert(1 < 2)?\n  assertEqual(1 + 1, 2)?\n  Ok(())"
            )
            .value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    // -------------------------------------------------------- closures

    /// A closure holds what it captured and answers with it, however far the
    /// value has travelled from the frame that made it.
    #[test]
    fn a_returned_closure_still_holds_what_it_captured() {
        assert_eq!(
            value_of(
                "Int",
                "fn adder(by: Int) -> fn(Int) -> Int {\n  fn(n: Int) {\n    n + by\n  }\n}\n",
                "  let add = adder(3)\n  add(4) + add(10)"
            ),
            "Int(20)"
        );
    }

    /// The oracle captures by value at creation time, so assigning to the
    /// binding afterwards does not change what the closure sees — including
    /// where the binding is a `var` one, which is the case the place model
    /// rests on.
    #[test]
    fn a_capture_is_the_value_the_binding_held_when_the_closure_was_written() {
        assert_eq!(
            value_of(
                "Int",
                "",
                "  var b = 10\n  let g = fn() {\n    b\n  }\n  b = 20\n  g() + b"
            ),
            "Int(30)"
        );
    }

    /// A closure is a value: it is bound, passed, returned, and called, and
    /// each of those is the same value on both backends.
    #[test]
    fn a_closure_is_passed_and_called_like_any_other_value() {
        assert_eq!(
            value_of(
                "Int",
                "fn apply(v: Int, t: fn(Int) -> Int) -> Int {\n  t(v)\n}\n",
                "  let double = fn(n: Int) {\n    n * 2\n  }\n  apply(5, double) + apply(5, fn(n: Int) { n + 1 })"
            ),
            "Int(16)"
        );
    }

    /// A declared function used as a value is a closure over nothing, and
    /// calling it through the value reaches the same body a direct call
    /// does.
    #[test]
    fn a_function_used_as_a_value_answers_what_a_direct_call_answers() {
        assert_eq!(
            value_of(
                "Int",
                "fn twice(n: Int) -> Int {\n  n * 2\n}\n",
                "  let g = twice\n  twice(3) + g(4)"
            ),
            "Int(14)"
        );
    }

    /// A closure renders and compares the way the oracle's does: `<fn>`, and
    /// equal to nothing including itself.
    #[test]
    fn a_closure_renders_and_compares_as_the_oracle_says() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  let h = fn() {\n    1\n  }\n  let i = fn() {\n    1\n  }\n  println(\"{h} {h == h} {h == i}\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "<fn> false false\n");
    }

    /// A higher-order builtin runs its callback by entering the loop again,
    /// and the run carries on afterwards exactly where it was.
    ///
    /// `mapError` is the one the language has, and it reads the callback's
    /// arity to decide whether to hand it the error it replaces — which is
    /// why a lowered closure has to answer that question the way an
    /// interpreted one does.
    #[test]
    fn a_builtin_runs_a_callback_and_the_run_continues() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  let n = 7\n  \
             let a = Int.parse(\"x\").mapError {\n    Error(\"bad {n}\")\n  }\n  \
             let b = Int.parse(\"3\").mapError {\n    Error(\"unreached\")\n  }\n  \
             println(\"{a} {b} {n}\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "Err(bad 7) Ok(3) 7\n");
    }

    /// A callback that fails carries its failure out, and the failure is the
    /// one the callback raised rather than anything the boundary added.
    #[test]
    fn a_callback_that_fails_ends_the_run_with_its_own_failure() {
        let (sources, checked) = checked_module(
            "use console.println\n\nexport fn main() -> Result<Int, Error> {\n  \
             Int.parse(\"x\").mapError {\n    Error(\"{1 / 0}\")\n  }\n}\n",
        );
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert_eq!(interpreted.error().message, "`Int` division by zero");
        assert_eq!(lowered.error().message, interpreted.error().message);
        assert_eq!(lowered.error().span, interpreted.error().span);
    }

    // ----------------------------------------------------------- tasks

    /// Two tasks in one scope, awaited, on a VM each.
    ///
    /// `agree` is the whole assertion: what the two backends answer and what
    /// they printed. A task's own VM is a second one over the same program,
    /// so a body that reaches a declared function is reaching the function
    /// the same `FunctionId` names on the spawning thread.
    #[test]
    fn two_tasks_in_one_scope_answer_what_the_interpreter_answers() {
        assert_eq!(
            value_of(
                "Result<Int, Error>",
                "fn twice(n: Int) -> Int {\n  n * 2\n}\n",
                "  scope work {\n    let a = work.spawn { twice(3) }\n    let b = work.spawn { twice(4) }\n    Ok(await a + await b)\n  }"
            ),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(14)] })"
        );
    }

    /// "Leaving the scope waits for or cancels its child tasks." A task
    /// nobody awaited has still run by the time the scope is left, so its
    /// line is printed before whatever follows the scope.
    #[test]
    fn leaving_a_scope_waits_for_a_task_nobody_awaited() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  scope work {\n    let quiet = work.spawn { println(\"from the task\") }\n  }\n  println(\"after the scope\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "from the task\nafter the scope\n");
    }

    /// A child whose value is `Err(...)` returns that failure from the call
    /// the scope was written in, exactly as `?` would — which is why the
    /// lowering writes a `try` after every `leave-scope`.
    #[test]
    fn a_child_whose_value_failed_returns_that_failure_from_the_call() {
        let outcome = agree(
            "use console.println\n\nfn boom() -> Result<Int, Error> {\n  Err(Error(\"boom\"))\n}\n\n             export fn main() -> Result<Unit, Error> {\n  scope work {\n    let t = work.spawn { boom() }\n  }\n               println(\"never\")?\n  Ok(())\n}\n",
        );
        assert_eq!(
            outcome.value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Err\", payload: [Struct(StructValue { type_name: \"Error\", fields: [(\"message\", Str(\"boom\"))], opaque: false })] })"
        );
        assert_eq!(outcome.output, "");
    }

    /// A child that *raised* is not a value, so it never reaches that `try`:
    /// it propagates as the error it is, pointing where it was raised.
    #[test]
    fn a_child_that_raised_propagates_as_the_error_it_is() {
        let (sources, checked) = checked_module(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n               scope work {\n    let t = work.spawn { 1 / 0 }\n  }\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert_eq!(interpreted.error().message, "`Int` division by zero");
        assert_eq!(lowered.error().message, interpreted.error().message);
        assert_eq!(lowered.error().span, interpreted.error().span);
    }

    /// A `break` leaves a scope without reaching its `leave-scope`, so the
    /// lowering emits the `cancel-scope` that does what leaving one does.
    /// The oracle cancels there too — `Interpreter::leave_scope`'s early
    /// branch treats a `Break` the way it treats a `return`.
    #[test]
    fn a_break_out_of_a_scope_cancels_it() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  var i = 0\n  while i < 3 {\n    scope work {\n      let t = work.spawn { i }\n      if i == 1 {\n        break\n      }\n      println(\"turn {await t}\")?\n    }\n    i += 1\n  }\n  println(\"done\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "turn 0\ndone\n");
        assert!(
            main_of(
                "export fn main() -> Result<Unit, Error> {\n  while true {\n    scope work {\n      break\n    }\n  }\n  Ok(())\n}\n"
            )
            .contains("cancel-scope"),
            "a `break` written inside a scope cancels it"
        );
    }

    /// The whole shape of a scope, in one listing: the scope is opened, bound
    /// to a slot, read back as the receiver of the `spawn`, and left through
    /// a `leave-scope` and the `try` that turns a failed child into a return.
    #[test]
    fn a_scope_with_a_spawn_lowers_to_the_scope_and_the_try_that_leaves_it() {
        assert_eq!(
            main_of(
                "export fn main() -> Result<Unit, Error> {\n  scope work {\n    let t = work.spawn { 1 }\n    await t\n  }\n  Ok(())\n}\n"
            ),
            concat!(
                "fn m.main arity=0 frame=2/0 -> value\n",
                "   0  enter-scope work\n",
                "   1  store 0\n",
                "   2  load 0\n",
                "   3  make-closure m.<closure 0> captures=0\n",
                "   4  spawn\n",
                "   5  store 1\n",
                "   6  load 1\n",
                "   7  await\n",
                "   8  leave-scope\n",
                "   9  try\n",
                "  10  pop\n",
                "  11  const Unit\n",
                "  12  make-builtin Ok argc=1\n",
                "  13  return\n",
            )
        );
    }

    /// The task-safety rule at the boundary is `crate::task`'s, so a capture
    /// that may not cross is refused at the `spawn` in the same words on
    /// either backend — including the path that names how it was reached.
    #[test]
    fn a_capture_that_may_not_cross_is_refused_at_the_spawn() {
        let (sources, checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  var items = Vector.of(1)\n               scope work {\n    let t = work.spawn { items.length() }\n  }\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert_eq!(
            interpreted.error().message,
            "`spawn` cannot capture `items`, which is a `Vector`"
        );
        assert_eq!(lowered.error().message, interpreted.error().message);
        assert_eq!(lowered.error().span, interpreted.error().span);
    }

    /// A scope inside a task is a scope like any other: the child's VM has
    /// its own frame stack, so the depth a scope is registered at is that
    /// VM's own and a nested `spawn` reaches the scope its own body opened.
    #[test]
    fn a_task_may_open_a_scope_of_its_own() {
        assert_eq!(
            value_of(
                "Result<Int, Error>",
                "fn inner() -> Result<Int, Error> {\n  scope deep {\n    let t = deep.spawn { 6 }\n    Ok(await t)\n  }\n}\n",
                "  scope outer {\n    let a = outer.spawn { inner() }\n    await a\n  }"
            ),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(6)] })"
        );
    }

    /// A run stopped at its concurrency limit is stopped identically on both
    /// backends: the charge happens in `crate::task::spawn_into`, before the
    /// task has an id, an event, or a thread.
    #[test]
    fn a_spawn_past_the_concurrency_limit_is_refused_the_same_way() {
        let (sources, checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  scope work {\n                 let a = work.spawn { 1 }\n    let b = work.spawn { 2 }\n  }\n  Ok(())\n}\n",
        );
        let limits = Limits {
            max_tasks: Some(1),
            ..Limits::default()
        };
        let (interpreted, lowered) = on_both(&checked, &sources, "m", Some(limits));
        assert!(
            interpreted
                .error()
                .message
                .contains("concurrency limit of 1 task(s) exceeded"),
            "{:?}",
            interpreted.error()
        );
        assert_eq!(lowered.error().message, interpreted.error().message);
        assert_eq!(lowered.error().span, interpreted.error().span);
    }

    // -------------------------------------------------------- the host

    /// The same calls, in the same order, with the same values.
    #[test]
    fn host_calls_reach_the_host_in_the_same_order() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  println(\"one\")?\n  for i in 0..<3 {\n    println(\"tick {i}\")?\n  }\n  println(\"done\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "one\ntick 0\ntick 1\ntick 2\ndone\n");
    }

    /// A capability the run was not granted is refused at the boundary both
    /// backends call through.
    #[test]
    fn an_ungranted_capability_is_refused_at_the_boundary() {
        let (sources, checked) = checked_module(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n  println(\"hello\")?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = crate::on_cove_stack(|| {
            let ungranted = || {
                let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
                hosts.register(Box::new(Console::new(Buffer::default())));
                Arc::new(hosts)
            };
            let interpreted = {
                let runtime = Runtime::new(checked.clone(), sources.clone(), ungranted());
                described(Interpreter::new(&runtime).run_entry("m", "main", Vec::new()))
            };
            let lowered = {
                let program = cove_ir::lower::lower(&checked).expect("it lowers");
                let entry = program.function_named("m", "main").expect("`main` lowered");
                let hosts = ungranted();
                let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
                described(Vm::new(&runtime, &hosts, &Arc::new(program)).run(entry, Vec::new()))
            };
            (interpreted, lowered)
        })
        .expect("a thread to run Cove on");
        assert_eq!(format!("{interpreted:?}"), format!("{lowered:?}"));
        assert_eq!(
            lowered.expect_err("the capability was not granted").message,
            "`console.println` requires the `console` capability, which this run was not granted"
        );
    }

    /// An operation on a resource handle is dispatched by the handle, not by
    /// a module name the instruction carries.
    ///
    /// The `Writer` and the `Reader` are two handles of two kinds issued by
    /// one host, and the run writes through one and reads back through the
    /// other, so an answer that came from anywhere but the handle each call
    /// stood on would be a different file or no file at all.
    #[test]
    fn a_resource_operation_is_dispatched_by_the_handle_it_stands_on() {
        let outcome = agree(
            "use console.println\nuse files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(\"first\")?\n  writer.write(\"second\")?\n  writer.close()?\n  let reader = files.open(\"notes.txt\")?\n  println(\"one {reader.readLine()?.unwrapOr(\"\")}\")?\n  println(\"two {reader.readLine()?.unwrapOr(\"\")}\")?\n  reader.close()?\n  Ok(())\n}\n",
        );
        assert_eq!(outcome.output, "one first\ntwo second\n");
    }

    /// The instruction that dispatched them, so a call that stopped being a
    /// resource call would fail here rather than go on passing.
    #[test]
    fn a_resource_call_is_lowered_to_the_instruction_that_routes_by_a_handle() {
        assert_eq!(
            main_of(
                "use files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(\"first\")?\n  writer.close()\n}\n"
            ),
            "fn m.main arity=0 frame=1/0 -> value\n\
             \x20  0  const Str(\"notes.txt\")\n\
             \x20  1  call-host files.create argc=1\n\
             \x20  2  try\n\
             \x20  3  store 0\n\
             \x20  4  load 0\n\
             \x20  5  const Str(\"first\")\n\
             \x20  6  call-resource writeLine argc=1\n\
             \x20  7  try\n\
             \x20  8  pop\n\
             \x20  9  load 0\n\
             \x20 10  call-resource close argc=0\n\
             \x20 11  return\n"
        );
    }

    /// A handle that outlived what it named fails inside the host, where the
    /// only record of what is still open lives — and fails the same way on
    /// both backends, which is what `tests/e2e:fail_http_stale_handle` is in
    /// the corpus for.
    #[test]
    fn a_stale_handle_fails_the_same_way_on_both_backends() {
        let outcome = agree(
            "use files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.close()?\n  writer.close()?\n  Ok(())\n}\n",
        );
        assert_eq!(
            outcome.error().message,
            "`files.Writer#1` is closed, so `close` has nothing to act on"
        );
    }

    // -------------------------------------------------------- budgets

    /// Fuel exhaustion stops the VM.
    ///
    /// The two backends do not stop at the same point and are not asked to:
    /// ADR 0019 makes `fuel_spent` backend-specific, because an instruction
    /// is not an AST node. What both must do is stop, with the message the
    /// shared budget writes.
    #[test]
    fn fuel_exhaustion_stops_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
        );
        let limits = Limits {
            fuel: Some(1_000),
            ..Limits::default()
        };
        let (interpreted, lowered) = on_both(&checked, &sources, "m", Some(limits));
        assert_eq!(
            interpreted.error().message,
            "execution stopped: fuel budget of 1000 exhausted"
        );
        assert_eq!(
            lowered.error().message,
            "execution stopped: fuel budget of 1000 exhausted"
        );
    }

    /// Cancellation stops the VM at its next safepoint.
    #[test]
    fn cancellation_stops_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
        );
        // Cancelled before the first instruction, so both backends stop at
        // the first safepoint they reach rather than at a moment a test would
        // have to race them for.
        let cancelled = || {
            let cancellation = Cancellation::new();
            let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
            cancellation.cancel();
            budget
        };
        let stopped = crate::on_cove_stack(|| {
            (
                interpreted(&checked, &sources, "m", Some(cancelled())),
                lowered(&checked, &sources, "m", Some(cancelled())),
            )
        })
        .expect("a thread to run Cove on");
        assert_eq!(
            stopped.0.error().message,
            "execution stopped: the run was cancelled"
        );
        assert_eq!(
            stopped.1.error().message,
            "execution stopped: the run was cancelled"
        );
    }

    // ---------------------------------------------- enums and `match`

    const STATUS: &str = "enum Status {\n  Confirmed\n  Pending(Int)\n}\n\n";

    /// Every case of a declared enum, built and rendered.
    #[test]
    fn a_declared_enum_case_is_built_the_way_the_interpreter_builds_it() {
        assert_eq!(
            agree(&format!(
                "{STATUS}export fn main() -> String {{\n  \"{{Status.Confirmed}} {{Status.Pending(3)}}\"\n}}\n"
            ))
            .value(),
            "Str(\"Confirmed Pending(3)\")"
        );
    }

    /// A case carries the qualified name of the enum that declares it, which
    /// is what keeps two modules' `Status` two types.
    #[test]
    fn a_case_carries_the_qualified_name_of_its_enum() {
        assert_eq!(
            agree(&format!(
                "{STATUS}export fn main() -> Status {{\n  Status.Pending(1)\n}}\n"
            ))
            .value(),
            "Enum(EnumValue { type_name: \"m.Status\", case: \"Pending\", payload: [Int(1)] })"
        );
    }

    /// An associated function declared in an `impl` block is a call, and a
    /// case of the same enum is not — the order `Interpreter::eval_call`
    /// asks in, reproduced.
    #[test]
    fn an_associated_function_of_an_enum_is_called_and_a_case_is_built() {
        let source = format!(
            "{STATUS}impl Status {{\n  fn start() -> Status {{\n    Status.Pending(0)\n  }}\n}}\n\nexport fn main() -> String {{\n  \"{{Status.start()}} {{Status.Confirmed}}\"\n}}\n"
        );
        assert_eq!(agree(&source).value(), "Str(\"Pending(0) Confirmed\")");
    }

    /// Every pattern form the language has, over one subject each.
    #[test]
    fn every_pattern_form_matches_what_the_interpreter_matches() {
        let variant = format!(
            "{STATUS}fn label(s: Status) -> String {{\n  match s {{\n    Status.Confirmed => \"yes\"\n    Status.Pending(n) => \"wait {{n}}\"\n  }}\n}}\n\nexport fn main() -> String {{\n  \"{{label(Status.Confirmed)}} {{label(Status.Pending(4))}}\"\n}}\n"
        );
        assert_eq!(agree(&variant).value(), "Str(\"yes wait 4\")");

        // A literal arm, a binder arm, and a `_` arm, in one `match` each.
        let literal = "fn name(n: Int) -> String {\n  match n {\n    1 => \"one\"\n    -2 => \"minus two\"\n    other => \"many {other}\"\n  }\n}\n\nexport fn main() -> String {\n  \"{name(1)} {name(-2)} {name(9)}\"\n}\n";
        assert_eq!(agree(literal).value(), "Str(\"one minus two many 9\")");

        let wildcard = "fn small(n: Int) -> Bool {\n  match n {\n    0 => true\n    _ => false\n  }\n}\n\nexport fn main() -> String {\n  \"{small(0)} {small(1)}\"\n}\n";
        assert_eq!(agree(wildcard).value(), "Str(\"true false\")");
    }

    /// `Ok(Some(x))`: a pattern two levels deep, matching and failing at each
    /// of them.
    #[test]
    fn a_pattern_nested_two_deep_matches_and_fails_at_each_level() {
        let source = "fn opened(r: Result<Option<Int>, Error>) -> Int {\n  match r {\n    Ok(Some(x)) => x\n    Err(e) => -1\n    _ => 0\n  }\n}\n\nexport fn main() -> String {\n  let there: Result<Option<Int>, Error> = Ok(Some(7))\n  let nothing: Result<Option<Int>, Error> = Ok(None)\n  let bad: Result<Option<Int>, Error> = Err(Error(message: \"no\"))\n  \"{opened(there)} {opened(nothing)} {opened(bad)}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"7 0 -1\")");
    }

    /// `None` written as a pattern is a case of `Option` and not a name, so
    /// it matches the case and nothing else.
    #[test]
    fn none_written_as_a_pattern_is_a_case_and_not_a_name() {
        let source = "fn told(o: Option<Int>) -> Int {\n  match o {\n    None => -1\n    Some(n) => n\n  }\n}\n\nexport fn main() -> String {\n  \"{told(Some(5))} {told(None)}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"5 -1\")");
    }

    /// The first arm that matches is the only one that runs, even where a
    /// later one would have matched too.
    #[test]
    fn an_earlier_arm_wins_over_a_later_one_that_would_also_match() {
        let source = "fn which(n: Int) -> String {\n  match n {\n    1 => \"first\"\n    other => \"binder\"\n  }\n}\n\nexport fn main() -> String {\n  \"{which(1)} {which(2)}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"first binder\")");
    }

    /// An arm's binder is released when the arm ends, so a name declared
    /// outside the `match` is what a later reference reaches.
    #[test]
    fn a_binder_is_out_of_scope_after_its_arm() {
        let source = "export fn main() -> String {\n  let n = 1\n  let seen = match Some(9) {\n    Some(n) => n\n    None => 0\n  }\n  \"{seen} {n}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"9 1\")");
    }

    /// A `match` on the result of a `match`, so that one nests inside
    /// another's arm and inside another's subject.
    #[test]
    fn a_match_nests_in_another_matchs_arm_and_subject() {
        let source = "fn inner(n: Int) -> Option<Int> {\n  match n {\n    0 => None\n    other => Some(other * 2)\n  }\n}\n\nexport fn main() -> String {\n  let outer = match inner(3) {\n    Some(v) => match v {\n      6 => \"six\"\n      _ => \"other\"\n    }\n    None => \"none\"\n  }\n  let nested = match match inner(0) {\n    Some(v) => v\n    None => -1\n  } {\n    -1 => \"empty\"\n    _ => \"full\"\n  }\n  \"{outer} {nested}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"six empty\")");
    }

    /// A subject no arm covers stops the run, in the interpreter's words.
    ///
    /// Exhaustiveness is checked case by case rather than pattern by
    /// pattern, so `Some(1)` covers `Some` as far as `cove-sema` is
    /// concerned and a `Some(2)` reaches no arm at run time. That is what
    /// makes `no-match` a thing a checked program can still arrive at.
    #[test]
    fn a_match_that_covers_nothing_stops_both_backends_the_same_way() {
        let source = "export fn main() -> String {\n  let o: Option<Int> = Some(2)\n  match o {\n    Some(1) => \"one\"\n    None => \"none\"\n  }\n}\n";
        let outcome = agree(source);
        assert_eq!(outcome.error().message, "no `match` arm covers `Some(2)`");
        assert_eq!(
            outcome.error().help.as_deref(),
            Some("add an arm for this case, or a `_` arm")
        );
    }

    // ----------------------------------- associated functions of builtins

    #[test]
    fn builtin_associated_functions_answer_what_the_interpreter_answers() {
        assert_eq!(expression("Int", "Vector.of(1, 2, 3).length()"), "Int(3)");
        assert_eq!(expression("Int", "Vector.of().length()"), "Int(0)");
        assert_eq!(expression("Int", "Set.of(3, 1, 2).length()"), "Int(3)");
        assert_eq!(
            expression("Int", "Map.of(MapEntry(key: \"a\", value: 1)).length()"),
            "Int(1)"
        );
        assert_eq!(
            expression("String", "\"{Int.parse(\"12\")}\""),
            "Str(\"Ok(12)\")"
        );
        assert_eq!(
            expression("String", "\"{Int.parse(\"twelve\")}\""),
            "Str(\"Err(`twelve` is not an Int)\")"
        );
        assert_eq!(
            expression("String", "\"{Float.parse(\"1.5\")}\""),
            "Str(\"Ok(1.5)\")"
        );
        assert_eq!(
            expression("String", "\"{Float.parse(\"x\")}\""),
            "Str(\"Err(`x` is not a Float)\")"
        );
    }

    /// A name a builtin type has no associated function for fails through the
    /// one dispatch both backends make.
    #[test]
    fn an_unknown_associated_function_fails_the_way_the_interpreter_fails() {
        assert_eq!(
            refused_unchecked(
                "Int",
                "Vector.of(1).length() + Int.parse(\"1\", \"2\").unwrapOr(0)"
            ),
            "`Int.parse` takes 1 argument(s), but 2 were given"
        );
    }

    /// `MapEntry` is the one builtin struct a program builds by calling its
    /// name, and its two fields are read back like any other struct's.
    #[test]
    fn a_map_entry_is_built_and_read_like_a_struct() {
        assert_eq!(
            expression("String", "\"{MapEntry(key: \"a\", value: 1)}\""),
            "Str(\"MapEntry(key: a, value: 1)\")"
        );
        assert_eq!(
            expression("String", "MapEntry(key: \"a\", value: 1).key"),
            "Str(\"a\")"
        );
        assert_eq!(
            expression("Int", "MapEntry(key: \"a\", value: 1).value"),
            "Int(1)"
        );
    }

    // ------------------------ where a program is refused before it runs

    /// What the lowering said when it refused `source`.
    ///
    /// A program the lowering refuses never reaches the VM, which is ADR
    /// 0019's no-silent-fallback rule from the other side: the run stops
    /// before any side effect and says what stopped it.
    fn not_lowered(checked: &Arc<Checked>) -> String {
        match cove_ir::lower::lower(checked) {
            Ok(_) => panic!("the program lowered, and was expected not to"),
            Err(why) => why.what,
        }
    }

    /// One interpreted run, on a stack the runtime sized.
    fn only_interpreted(checked: &Arc<Checked>, sources: &Arc<SourceMap>) -> Outcome {
        crate::on_cove_stack(|| interpreted(checked, sources, "m", None))
            .expect("a thread to run Cove on")
    }

    /// A value no `for` can walk fails in `interp::items_of`'s words on the
    /// VM, because they *are* its words: `IterItems` calls that function
    /// rather than restating what it decides.
    ///
    /// This does not run both backends from source, because no program can
    /// reach it on either. `cove-sema` refuses the mistake —
    /// `cove::type::iterable` — so a checked program has no `for` over a
    /// value that is not a collection, and there is nothing to lower that
    /// would arrive at one. What is left to hold is that the floor under a
    /// checker that stopped proving it is one floor and not two, so the
    /// instruction is executed over an IR written by hand and the answer is
    /// compared against the oracle's own function.
    #[test]
    fn a_value_that_cannot_be_walked_fails_in_the_interpreters_words() {
        let (sources, checked) = checked_module("export fn main() -> Int {\n  1\n}\n");
        let span = Span::new(FileId(0), 0, 1);
        let (on_the_vm, on_the_oracle) = crate::on_cove_stack(|| {
            // The IR holds `Rc`s, so it is built on the thread that runs it.
            let code = vec![
                cove_ir::Inst::Const(cove_ir::ConstId(0)),
                cove_ir::Inst::IterItems,
                cove_ir::Inst::Return,
            ];
            let program = Program {
                dispatches: Vec::new(),
                constants: vec![Const::Int(1)],
                functions: vec![cove_ir::Function {
                    module: "m".into(),
                    name: "main".into(),
                    value_frame_size: 0,
                    scalar_frame_size: 0,
                    place_frame_size: 0,
                    arity: 0,
                    params: Vec::new(),
                    returns: cove_ir::SlotKind::Value,
                    has_receiver: false,
                    captures: Vec::new(),
                    written_params: Vec::new(),
                    spans: vec![span; code.len()],
                    block_fuel: cove_ir::lower::block_fuel(&code),
                    code,
                    arg_spans: BTreeMap::new(),
                    span,
                }],
            };
            cove_ir::lower::validate(&program).unwrap_or_else(|why| {
                panic!("the hand-written IR holds the VM's invariants: {why}")
            });
            let buffer = Buffer::default();
            let hosts = hosts(&buffer, None);
            let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
            let on_the_vm = Vm::new(&runtime, &hosts, &Arc::new(program))
                .run(FunctionId(0), Vec::new())
                .expect_err("an `Int` cannot be walked")
                .message;
            let on_the_oracle = crate::interp::items_of(Value::Int(1), span)
                .expect_err("an `Int` cannot be walked")
                .message;
            (on_the_vm, on_the_oracle)
        })
        .expect("a thread to run Cove on");
        assert_eq!(on_the_vm, on_the_oracle);
        assert_eq!(
            on_the_vm,
            "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `Int`"
        );
    }

    /// A case that does not exist, and a payload of the wrong length, fail in
    /// `Interpreter::enum_case`'s words on the VM, because they *are* its
    /// words: the VM's `MakeEnum` calls that function rather than restating
    /// what it decides.
    ///
    /// This is the one place here that does not run both backends, because no
    /// program can reach it on either. `cove-sema` refuses both mistakes —
    /// `cove::type::unknown_case` and `cove::type::payload_arity` — so a
    /// checked program has neither, and there is nothing to lower that would
    /// arrive at one. What is left to hold is that the floor under a checker
    /// that stopped proving it is one floor and not two, so the instruction is
    /// executed over an IR written by hand and the answer is compared against
    /// the oracle's own function.
    #[test]
    fn a_case_that_does_not_exist_fails_in_the_interpreters_words() {
        let (sources, checked) = checked_module(
            "enum Status {\n  Confirmed\n  Pending(Int)\n}\n\nexport fn main() -> Status {\n  Status.Confirmed\n}\n",
        );
        let decl = checked
            .modules
            .get("m")
            .and_then(|resolved| resolved.enums.get("Status"))
            .map(|entry| entry.decl.clone())
            .expect("the module declares `Status`");

        // A case the declaration does not write, over no payload.
        let (vm_said, oracle_said) = built_by_hand(&checked, &sources, "Nope", 0, &decl);
        assert_eq!(vm_said, oracle_said);
        assert_eq!(
            vm_said,
            "enum `Status` has no case or associated function `Nope`"
        );

        // A case that exists, over a payload of the wrong length.
        let (vm_said, oracle_said) = built_by_hand(&checked, &sources, "Confirmed", 2, &decl);
        assert_eq!(vm_said, oracle_said);
        assert_eq!(
            vm_said,
            "case `Status.Confirmed` carries 0 value(s), but 2 were given"
        );
    }

    /// Runs one `MakeEnum` over `payload` `Unit`s on the VM, and asks
    /// [`crate::interp::enum_case`] the same question directly.
    ///
    /// The IR is written here rather than lowered because no source lowers to
    /// it: what is being checked is the instruction, not a program.
    fn built_by_hand(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        case: &str,
        payload: u32,
        decl: &Arc<cove_syntax::ast::EnumDecl>,
    ) -> (String, String) {
        let span = decl.span;
        crate::on_cove_stack(|| {
            // The IR holds `Rc`s, so it is built on the thread that runs it.
            let mut code = vec![cove_ir::Inst::Const(cove_ir::ConstId(2)); payload as usize];
            code.push(cove_ir::Inst::MakeEnum {
                ty: cove_ir::ConstId(0),
                case: cove_ir::ConstId(1),
                argc: payload,
            });
            code.push(cove_ir::Inst::Return);
            let program = Program {
                dispatches: Vec::new(),
                constants: vec![
                    Const::Name("m.Status".into()),
                    Const::Name(case.into()),
                    Const::Unit,
                ],
                functions: vec![cove_ir::Function {
                    module: "m".into(),
                    name: "main".into(),
                    value_frame_size: 0,
                    scalar_frame_size: 0,
                    place_frame_size: 0,
                    arity: 0,
                    params: Vec::new(),
                    returns: cove_ir::SlotKind::Value,
                    has_receiver: false,
                    captures: Vec::new(),
                    written_params: Vec::new(),
                    spans: vec![span; code.len()],
                    block_fuel: cove_ir::lower::block_fuel(&code),
                    code,
                    arg_spans: BTreeMap::new(),
                    span,
                }],
            };
            cove_ir::lower::validate(&program).unwrap_or_else(|why| {
                panic!("the hand-written IR holds the VM's invariants: {why}")
            });
            let buffer = Buffer::default();
            let hosts = hosts(&buffer, None);
            let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
            let on_the_vm = Vm::new(&runtime, &hosts, &Arc::new(program))
                .run(FunctionId(0), Vec::new())
                .expect_err("the case cannot be built")
                .message;
            let on_the_oracle = crate::interp::enum_case(
                checked,
                "m",
                decl,
                case,
                vec![Value::Unit; payload as usize],
                span,
            )
            .expect_err("the case cannot be built")
            .message;
            (on_the_vm, on_the_oracle)
        })
        .expect("a thread to run Cove on")
    }

    /// A failing assertion quotes its condition identically on both backends.
    ///
    /// `assert` and `assertEqual` are builtins because their failure names
    /// the condition in the words the test was written in. The interpreter
    /// reads the argument's span out of the `SourceMap`; the VM reads the
    /// same span out of `cove_ir::Function::arg_spans` and the same text out
    /// of the same map, so the two messages are compared byte for byte here
    /// rather than merely both being failures.
    #[test]
    fn a_failing_assertion_quotes_its_condition_on_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  assert(1 > 2)?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert!(
            interpreted.value().contains("assertion failed: `1 > 2`"),
            "{}",
            interpreted.value()
        );
        assert_eq!(interpreted.value(), lowered.value());

        let (equal_sources, equal_checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&equal_checked, &equal_sources, "m", None);
        assert!(
            interpreted
                .value()
                .contains("assertion failed: `1 + 1` is `2`, expected `3`"),
            "{}",
            interpreted.value()
        );
        assert_eq!(interpreted.value(), lowered.value());
    }

    /// Assigning to a `let` binding is refused by both backends: by the
    /// interpreter when the write happens, and by the lowering before the VM
    /// is handed anything.
    ///
    /// The interpreter's message is asserted here rather than only in
    /// `interp`, because what the two say is the point: a backend that
    /// performed this write would be more permissive than the oracle, and
    /// the two refusals have to stay comparable for a reader to see that
    /// they are the same rule.
    #[test]
    fn writing_a_let_binding_is_refused_by_both_backends() {
        let (sources, checked) =
            checked_module("export fn main() -> Int {\n  let x = 1\n  x = 2\n  x\n}\n");
        assert_eq!(
            only_interpreted(&checked, &sources).error().message,
            "cannot assign to `x`, which is a read-only place"
        );
        assert_eq!(
            not_lowered(&checked),
            "assignment to `x`, which is a read-only place"
        );
    }

    /// A field of a `let` binding is a read-only place too, and the write is
    /// refused on both backends for the same reason.
    #[test]
    fn writing_a_field_of_a_let_binding_is_refused_by_both_backends() {
        let (sources, checked) = checked_module(
            "struct P {\n  x: Int\n}\n\nexport fn main() -> Int {\n  let p = P(x: 1)\n  p.x = 2\n  p.x\n}\n",
        );
        assert_eq!(
            only_interpreted(&checked, &sources).error().message,
            "cannot assign to `p.x`, which is a read-only place"
        );
        assert_eq!(
            not_lowered(&checked),
            "assignment to `p.x`, which is a read-only place"
        );
    }

    /// A `var` binding is still written, on both backends.
    ///
    /// The refusal above is about a read-only place and not about assignment,
    /// and this is what says so.
    #[test]
    fn writing_a_var_binding_is_performed_by_both_backends() {
        assert_eq!(
            agree("export fn main() -> Int {\n  var x = 1\n  x = 2\n  x\n}\n").value(),
            "Int(2)"
        );
    }

    /// A method name a builtin type and a declared type both answer to is
    /// resolved by the receiver's type, which is the only thing that decides
    /// it.
    ///
    /// The interpreter tries a declared method of the receiver's *runtime*
    /// type first and falls back to the builtin table, so which applies is a
    /// fact about the receiver — and this used to be refused, because the
    /// lowering had no type table and `[1, 2, 3].length()` answering the
    /// builtin's `3` and a `Call` to the declared `Box.length` are two
    /// different programs. The checker settles which, so this asserts what
    /// the refusal used to protect: the array reaches the builtin with a
    /// `Box.length` declared in the same program.
    ///
    /// `a_declared_method_a_builtin_also_names_lowers_and_agrees` is the
    /// same fact read from the other side, with both calls written together.
    /// **A `var` parameter is an alias, not a copy that is written back.**
    ///
    /// `two(var x, var x)` is the case that separates the two, and the oracle
    /// answers 11: both parameters name the same cell, so the second
    /// parameter's `+= 10` is applied to what the first one's `+= 1` left.
    /// Copy-in/copy-out would answer 10, because the second write-back would
    /// overwrite the first with a value read before it happened. There is no
    /// design here that answers 10.
    #[test]
    fn two_var_parameters_naming_one_binding_are_one_binding() {
        assert_eq!(
            agree(
                "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nexport fn main() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n"
            )
            .value(),
            "Int(11)"
        );
    }

    /// A place can carry a field path, so `var c.hits` names one field of the
    /// caller's struct and leaves the rest of it alone.
    #[test]
    fn a_var_argument_can_name_a_field_of_the_callers_struct() {
        assert_eq!(
            agree(
                "struct C {\n  hits: Int\n  other: Int\n}\n\nfn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> String {\n  var c = C(hits: 0, other: 0)\n  bump(var c.hits)\n  \"{c}\"\n}\n"
            )
            .value(),
            "Str(\"C(hits: 1, other: 0)\")"
        );
    }

    /// A place is forwarded through a call: a `var` parameter passed on as a
    /// `var` argument aliases the original binding rather than the
    /// parameter's own slot, which is what `load-place` alone does.
    #[test]
    fn a_var_parameter_passed_on_still_names_the_original_binding() {
        assert_eq!(
            agree(
                "fn bump(var n: Int) {\n  n += 1\n}\n\nfn forward(var n: Int) {\n  bump(var n)\n}\n\nexport fn main() -> Int {\n  var x = 5\n  forward(var x)\n  x\n}\n"
            )
            .value(),
            "Int(6)"
        );
    }

    /// And it forwards with a step added on the way: the incoming place
    /// already has a path, and a field is appended to it at run time.
    #[test]
    fn a_place_forwarded_with_a_field_appended_reaches_two_deep() {
        assert_eq!(
            agree(
                "struct Inner {\n  hits: Int\n}\n\nstruct Outer {\n  inner: Inner\n}\n\nfn bump(var n: Int) {\n  n += 1\n}\n\nfn deep(var o: Outer) {\n  bump(var o.inner.hits)\n}\n\nexport fn main() -> String {\n  var o = Outer(inner: Inner(hits: 1))\n  deep(var o)\n  \"{o}\"\n}\n"
            )
            .value(),
            "Str(\"Outer(inner: Inner(hits: 2))\")"
        );
    }

    /// A `var self` receiver is a place like any other, and a method that
    /// takes one writes into the caller's binding.
    #[test]
    fn a_var_self_method_writes_into_the_callers_binding() {
        assert_eq!(
            agree(
                "struct Counter {\n  hits: Int\n}\n\nimpl Counter {\n  fn hit(var self) {\n    self.hits += 1\n  }\n\n  fn hitMany(var self, n: Int) -> Int {\n    for _step in 0..<n {\n      self.hits += 1\n    }\n    self.hits\n  }\n}\n\nexport fn main() -> String {\n  var counter = Counter(hits: 0)\n  counter.hit()\n  let total = counter.hitMany(3)\n  \"{counter} {total}\"\n}\n"
            )
            .value(),
            "Str(\"Counter(hits: 4) 4\")"
        );
    }

    /// Writing a field through a place makes the struct private again at the
    /// step it is written, which is what keeps a copy of a struct a copy.
    /// The same program without that call answers with both documents
    /// renamed.
    #[test]
    fn writing_through_a_place_leaves_a_copy_of_the_struct_alone() {
        assert_eq!(
            agree(
                "struct Meta {\n  version: Int\n}\n\nstruct Document {\n  title: String\n  meta: Meta\n}\n\nimpl Document {\n  fn rename(var self, title: String) {\n    self.title = title\n  }\n\n  fn bumpVersion(var self) {\n    self.meta.version += 1\n  }\n}\n\nexport fn main() -> String {\n  var original = Document(title: \"first\", meta: Meta(version: 1))\n  var copy = original\n  copy.rename(\"second\")\n  copy.bumpVersion()\n  \"{original} {copy}\"\n}\n"
            )
            .value(),
            "Str(\"Document(title: first, meta: Meta(version: 1)) Document(title: second, meta: Meta(version: 2))\")"
        );
    }

    /// `freeze` consumes uniquely owned storage, so it has to see the
    /// caller's own handle exactly once — which is what taking the place
    /// rather than a read of it arranges. Both backends answer the array,
    /// and both refuse when a second alias exists.
    #[test]
    fn freeze_through_a_place_consumes_the_storage_it_names() {
        assert_eq!(
            agree(
                "export fn main() -> String {\n  var v = Vector.of(\"a\", \"b\")\n  let frozen = v.freeze()\n  \"{frozen}\"\n}\n"
            )
            .value(),
            "Str(\"[a, b]\")"
        );
        let aliased = agree(
            "export fn main() -> String {\n  var v = Vector.of(\"a\")\n  let other = v\n  let frozen = v.freeze()\n  \"{frozen}\"\n}\n",
        );
        assert!(
            aliased.error().message.contains("uniquely owned"),
            "{}",
            aliased.error().message
        );
    }

    /// A `break` written inside a call that has already pushed a place has
    /// to take the place with it, or the loop's exit is reached at a depth
    /// nothing else reaches it at. That is what `place-pop` is for, and
    /// `validate` is what would have caught its absence.
    #[test]
    fn a_break_inside_a_half_built_call_leaves_no_place_standing() {
        assert_eq!(
            agree(
                "fn add(var n: Int, by: Int) {\n  n += by\n}\n\nexport fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 10 {\n    add(var total, by: if i == 3 {\n      break\n    } else {\n      i\n    })\n    i += 1\n  }\n  total\n}\n"
            )
            .value(),
            "Int(3)"
        );
    }

    /// A `var` parameter whose type the checker settled as `Int` still has a
    /// place, because the binding it names was kept on the value stack — a
    /// place is an index into that stack and there is nothing in the scalar
    /// one for it to address.
    #[test]
    fn a_binding_a_place_is_rooted_at_is_kept_on_the_value_stack() {
        let listed = main_of(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> Int {\n  var total = 1\n  var counted = 2\n  bump(var total)\n  total + counted\n}\n",
        );
        // `total` is slot 0 of the *value* frame and `counted` is slot 0 of
        // the scalar frame: only the name a place is rooted at moved.
        assert_eq!(
            listed,
            "fn m.main arity=0 frame=1/1 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  store 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 0\n\
             \x20  4  place 0\n\
             \x20  5  call m.bump argc=0/0/1\n\
             \x20  6  pop\n\
             \x20  7  load 0\n\
             \x20  8  value-to-scalar\n\
             \x20  9  load-scalar 0\n\
             \x20 10  int Add\n\
             \x20 11  return-scalar\n"
        );
    }

    #[test]
    fn a_method_name_a_builtin_and_a_declared_type_share_reaches_the_builtin() {
        let source = "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn length(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  [1, 2, 3].length()\n}\n";
        assert_eq!(agree(source).value(), "Int(3)");
        let listing = main_of(source);
        assert!(
            listing
                .lines()
                .any(|line| line.contains("call-builtin length")),
            "the array's `length` is the builtin's:\n{listing}"
        );
    }

    /// A declared method whose name no builtin has still lowers to a `Call`.
    ///
    /// The refusal above is about the collision and not about declared
    /// methods, and `benches/method` depends on this staying true.
    #[test]
    fn a_declared_method_no_builtin_shares_still_lowers() {
        assert_eq!(
            agree(
                "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn held(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  Box(n: 3).held()\n}\n"
            )
            .value(),
            "Int(3)"
        );
    }

    /// A `...` argument spreads an `Array` or a `Vector`, and both backends
    /// see the same elements in the same order.
    ///
    /// The four shapes `tests/e2e:fn_variadic` writes: no spread at all, one
    /// on its own, one mixed with an ordinary argument, and one over a
    /// `Vector` — whose elements are read as the vector holds them now,
    /// which is the borrow `bind_params` takes at the same moment.
    #[test]
    fn a_spread_argument_passes_a_sequence_where_the_elements_would_go() {
        assert_eq!(
            agree(
                "fn join(sep: String, items: String...) -> String {\n  var text = \"\"\n  for item in items {\n    text = \"{text}{sep}{item}\"\n  }\n  text\n}\n\nexport fn main() -> String {\n  let ready = [\"x\", \"y\"]\n  \"{join(\"-\", \"a\")}|{join(\"-\", ...ready)}|{join(\"-\", \"w\", ...ready)}|{join(\"-\", ...Vector.of(\"v\"))}\"\n}\n"
            )
            .value(),
            "Str(\"-a|-x-y|-w-x-y|-v\")"
        );
    }

    /// `snapshot` on a `Vector` allocates storage of its own, and both
    /// backends see the copy stop sharing.
    ///
    /// The observable difference between an ordinary copy and a snapshot is
    /// exactly this: pushing onto one is seen by the other or it is not.
    #[test]
    fn snapshot_of_a_vector_allocates_storage_the_original_does_not_share() {
        assert_eq!(
            agree(
                "export fn main() -> String {\n  var original = Vector.of(1)\n  var alias = original\n  var copy = original.snapshot()\n  alias.push(2)\n  copy.push(3)\n  \"{original.length()} {alias.length()} {copy.length()}\"\n}\n"
            )
            .value(),
            "Str(\"2 2 2\")"
        );
    }

    /// A struct's own conformance is reached through the `Call` the checker
    /// recorded, and the builtin half through the instruction.
    #[test]
    fn snapshot_dispatches_to_a_conformance_and_falls_back_to_the_instruction() {
        let source = "struct B {\n  guests: Vector<String>\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(guests: self.guests.snapshot())\n  }\n}\n\nexport fn main() -> String {\n  var original = B(guests: Vector.of(\"a\"))\n  var copy = original.snapshot()\n  copy.guests.push(\"b\")\n  \"{original.guests.length()} {copy.guests.length()} {5.snapshot()} {[1, 2].snapshot()}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"1 2 5 [1, 2]\")");
        let listed = main_of(source);
        assert!(
            listed
                .lines()
                .any(|line| line.contains("call m.B.snapshot")),
            "the struct's conformance is a call:\n{listed}"
        );
        assert!(
            listed
                .lines()
                .any(|line| line.trim().ends_with("snapshot") && !line.contains("call")),
            "the builtin half is the instruction:\n{listed}"
        );
    }

    /// A value of a type a host module declares is an ordinary struct on
    /// both backends.
    ///
    /// `Interpreter::init_host_type` builds a `Value::Struct` named
    /// `{module}.{Name}` with `opaque` false and nothing else, so the VM's
    /// `make-struct` is the whole of it: the shape table reads the qualified
    /// name off the instruction, and `is_opaque` answers false because no
    /// module of this package declares `Response`.
    #[test]
    fn a_type_a_host_declares_is_an_ordinary_struct_on_both_backends() {
        let source = "use http\n\nexport fn main() -> String {\n  let r = http.Response(status: 200, body: \"ok\")\n  \"{r} {r.status}\"\n}\n";
        assert_eq!(
            agree(source).value(),
            "Str(\"Response(status: 200, body: ok) 200\")"
        );
        assert!(
            main_of(source)
                .lines()
                .any(|line| line.contains("make-struct http.Response fields=status,body")),
            "the host type is built rather than asked for:\n{}",
            main_of(source)
        );
    }

    /// A default argument is computed inside the callee's frame, and the
    /// answer is the interpreter's.
    ///
    /// The specialisation is what makes the calling convention survive a
    /// call that skips a parameter: `measure(3, prefix: "d")` pushes two
    /// arguments, the callee takes two, and the third parameter is a slot
    /// the callee's own prologue writes. The listing is asserted beside the
    /// answer because an outcome cannot show which frame the default was
    /// evaluated in.
    #[test]
    fn a_parameter_left_to_its_default_is_computed_inside_the_callee() {
        let source = "fn measure(value: Int, unit: String = \"m\", prefix: String = \"length\") -> String {\n  \"{prefix}: {value}{unit}\"\n}\n\nexport fn main() -> String {\n  measure(3, prefix: \"d\")\n}\n";
        assert_eq!(agree(source).value(), "Str(\"d: 3m\")");
        assert_eq!(
            main_of(source),
            "fn m.main arity=0 frame=0/0 -> value\n\
             \x20  0  scalar-const 3\n\
             \x20  1  const Str(\"d\")\n\
             \x20  2  call m.measure argc=1/1\n\
             \x20  3  return\n"
        );
    }

    /// A default that reads an earlier parameter reads the argument this
    /// call passed, and a recursive call that supplies it reaches a second
    /// specialisation.
    #[test]
    fn a_default_reads_the_parameters_the_call_supplied() {
        assert_eq!(
            agree(
                "fn sumTo(n: Int, accumulated: Int = 0) -> Int {\n  if n == 0 {\n    accumulated\n  } else {\n    sumTo(n - 1, accumulated + n)\n  }\n}\n\nexport fn main() -> Int {\n  sumTo(10)\n}\n"
            )
            .value(),
            "Int(55)"
        );
    }

    // -------------------------------------------------------- benchmarks
    //
    // The `benches/` entries used to be checked here, one agreement test
    // each. `crates/cove-cli/tests/differential.rs` now runs the whole corpus
    // — every `tests/e2e` case and every `examples/` and `benches/` entry —
    // through both backends and compares the value, the console, the outcome,
    // and the filesystem the run left, so these were the same ground covered
    // less thoroughly and twice, at half a minute of every `cargo test`.
    //
    // What stays here is what is about the VM itself rather than about the two
    // backends agreeing: one instruction, one construct, one refusal at a
    // time.
}
