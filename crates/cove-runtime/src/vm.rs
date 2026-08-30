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
//!
//! **This VM collects.** What is written above used to be a statement about
//! where the roots are rather than code that reads them; `StackRoots` is
//! now the code, and it reads exactly that. Every frame's slots and every
//! frame's operands are in one `Vec<Value>`, so `self.stack` up to its
//! length is all of them at once and there is nothing to slice per frame;
//! the open task scopes are the one thing a `Vm` holds that the stack need
//! not reach, so they are walked beside it. Collection is non-moving, as ADR
//! 0011 and the Language Card say: nothing is relocated, so a place's index
//! stays valid across one, and a moving collector would need the one thing
//! more that this backend states and does not use — that a place is an index
//! into the storage being moved, so moving would have to rewrite it.
//!
//! Collection happens at safepoints and nowhere else, and it is safe there
//! for a reason that is worth stating rather than assuming: a `Value` the
//! dispatch loop has taken off the stack into a Rust local — a popped
//! receiver, the vector of arguments a host call is about to be given — is
//! invisible to a walk of the stack, and is therefore a *shortfall* in the
//! reference counting `crate::heap` does, and therefore a root. That is the
//! same rule that roots the interpreter's evaluator temporaries, and it is
//! what makes "collect at any safepoint" true rather than a list of
//! safepoints that happen to be tidy. `Vm::collect_if_due` is where the
//! whole of the argument is written out, site by site.
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
//! A capture takes the slot its own kind names, which is the second half of
//! what issue #162 settled about one slot identity. What a closure *holds* is
//! a list of `(name, Value)` pairs, because a host reads them and because a
//! lambda is one function however many specialisations of the body around it
//! are lowered; where a capture *lands* is `cove_ir::Function::capture_kinds`,
//! so one the checker settled as `Int` or `Bool` becomes a word of the scalar
//! window at the call and is read from there with no boundary instruction at
//! all. The conversion happens once per call in place of once per read.
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
//! `Vm`: `Vm::for_task` builds one on the new thread, over the same
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
//! happens to a scope whose frame returned out from under it. `Vm::leave`
//! is that answer, because it is the one place a frame is popped.
//!
//! Fuel, the deadline, the run's cancellation, the task's own cancellation,
//! the depth limit, and the trace are accounted inside a task exactly as they
//! are outside one, because they are accounted by the same `Vm`: a task's VM
//! reaches the run's one budget through the same [`HostRegistry`], and
//! `Vm::safepoint` asks the task's own flag beside the rest.
//!
//! # What is not here
//!
//! Whatever `cove_ir::lower` reports as [`cove_ir::Unsupported`], which no
//! longer includes anything about concurrency. ADR 0019's no-silent-fallback
//! rule is what makes that the right shape: a program the lowering refuses
//! never reaches this, so there is no construct this can be wrong about.

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

use crate::budget::{Budget, Cancellation, Meter, Stopped};
use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::heap::{Collection, Heap, HeapStats, Roots};
use crate::host::{HostRegistry, Reentry, ResourceHandle};
use crate::interp::{
    as_dyn, binary, coerce_inside, divide_by_zero, dyn_receiver, no_field, not_a_struct, overflow,
    returned_error_message, source_text, stopped_here, unary, MAX_CALL_DEPTH,
};
use crate::runtime::{Runtime, ENTRY_TASK};
use crate::task::{self, ChildFailure, Task, TaskOutcome, TaskScope, Transfer};
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
pub const SAFEPOINT_INTERVAL: u64 = 1024;

/// How much fuel may accumulate across back edges before the shared budget is
/// spent against.
///
/// A back edge is where a loop can be stopped, so one is checked at every one
/// of them — but *checking* took a lock the tasks share, and `benches/arith`
/// takes two million back edges, which was 13% of its run.
///
/// So a back edge checks only once this much fuel has gathered, and every
/// question a back edge asks waits for it: the run's cancellation, its
/// deadline, its fuel, and the stop flags of every bounded call this thread is
/// inside. What that costs is granularity: a loop notices a stop within this
/// much fuel plus the one turn that crosses it, rather than within one
/// iteration. What it buys is that a tight loop does not walk a list per turn
/// and does not charge the shared budget per turn either. The number is small
/// enough that the difference is not one a program can be written to observe,
/// and [`SAFEPOINT_INTERVAL`] still bounds a straight line that has no back
/// edge at all.
///
/// The lock this was first measured against is gone — issue #182 made the
/// budget's counters atomics, and [`crate::budget::Meter`] is where that is
/// argued — and the constant is unchanged, because it is not a tuning knob.
/// ADR 0024 states each stop as a bound in the backend's own fuel and names
/// this as the arithmetic of one, so moving it would move the language and
/// not just the speed.
pub const BACK_EDGE_FUEL: u64 = 64;

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

/// An assignable location: a slot of one of this VM's stacks, and the struct
/// fields to navigate from what stands in it.
///
/// This is `crate::interp::Place` with the one thing that made it allocate
/// taken out. The interpreter gives every binding an `Rc<RefCell<Value>>` of
/// its own, so a place can be a share of that cell; this VM put every
/// binding into one contiguous `Vec<Value>` precisely so that a call would
/// allocate nothing, and reintroducing a cell per binding to make places
/// possible would give back the whole of what the arrangement bought.
///
/// # Which stack, and one slot identity
///
/// A place names a *slot*, and a slot is a region and a number in it. That
/// is issue #162's answer, and it is what [`PlaceRoot`] is: a place rooted
/// at a value slot walks the value stack, and one rooted at a scalar slot
/// names a word of the scalar stack and has no path, because an `Int` and a
/// `Bool` have no fields.
///
/// It could only name the value stack until then, and what that cost was
/// measured. `cove_ir::lower` kept every binding a `var` argument was rooted
/// at on the value stack even where the checker had settled it as `Int`, so
/// a body that wrote `bump(var total)` anywhere paid a `scalar-to-value` and
/// a `value-to-scalar` on *every* read and write of `total` for the whole
/// body — 1.31x on `benches/convention`'s `conv_var` row against the same
/// loop without the one line, with the line outside the loop.
///
/// # Why an index, and what it rests on
///
/// An index into a stack, rather than a pointer into it, because the
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
    /// Which slot this is rooted at, and in which of the VM's stacks.
    root: PlaceRoot,
    /// The field positions to walk from there, outermost first. Empty for a
    /// place that names a binding, and always empty for a scalar root.
    path: Vec<u32>,
}

/// The slot a [`Place`] is rooted at: which stack, and where in it.
///
/// Absolute in both cases, for the reason [`Place`] gives: a place travels
/// into a call, where the callee's bases are different numbers.
///
/// The `Scalar` variant carries which of the two words it is naming, because
/// the scalar stack keeps no tag — the same fact `cove_ir::Inst::ScalarToValue`
/// carries an argument for, and for the same reason: a read through the place
/// has to put a tag back on.
#[derive(Clone, Copy, Debug)]
enum PlaceRoot {
    /// A slot of the value stack.
    Value(usize),
    /// A slot of the scalar stack, and what the word in it stands for.
    Scalar(usize, Scalar),
}

impl Place {
    /// The place naming `slot` of the value stack, with no path.
    fn rooted_at(slot: usize) -> Place {
        Place {
            root: PlaceRoot::Value(slot),
            path: Vec::new(),
        }
    }

    /// The place naming `slot` of the scalar stack.
    ///
    /// There is no path and there can never be one: `cove_ir::lower` emits a
    /// `place-field` only where the checker settled the struct type the step
    /// is taken in, and neither `Int` nor `Bool` is one.
    fn rooted_at_scalar(slot: usize, what: Scalar) -> Place {
        Place {
            root: PlaceRoot::Scalar(slot, what),
            path: Vec::new(),
        }
    }

    /// Which slot of the *value* stack this place walks from.
    ///
    /// A scalar root never reaches here: an `Int` and a `Bool` have no
    /// fields, so nothing that walks a path can be handed one, and the two
    /// instructions that read and write a place without walking branch on
    /// the root before they ask. So this is a broken invariant of this
    /// backend rather than a program that could be told about it — the same
    /// standing an operand stack that came up empty has.
    fn value_root(&self) -> usize {
        match self.root {
            PlaceRoot::Value(slot) => slot,
            PlaceRoot::Scalar(..) => {
                unreachable!("a place rooted at a scalar slot has no path to walk")
            }
        }
    }

    /// This place with one more field step on the end, which is
    /// `crate::interp::Place::field` by position rather than by name.
    fn field(&self, index: u32) -> Place {
        let mut path = self.path.clone();
        path.push(index);
        Place {
            root: self.root,
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

/// This VM's root set: its value stack, and the task scopes it has open.
///
/// # How this list was derived
///
/// By going through every field of [`Vm`] and asking whether it is a
/// [`Value`] or holds one, because a root missed here is a use-after-sweep of
/// a Cove value and the next person needs to see that the list was checked
/// rather than guessed.
///
/// - `stack` — **a root**, and the whole of the frame convention's
///   contribution. Every frame's slots and every frame's operands are in this
///   one vector, so `stack[..stack.len()]` is all of them at once: there is
///   nothing to slice per frame and nothing to skip inside a frame. A
///   closure's *value* captures are copied into that window by the call that
///   entered the body, so they are in it already; a capture the checker
///   settled as `Int` or `Bool` goes into the scalar window instead and is a
///   root nowhere, which is right for the reason `scalars` is not a root.
///   Either way the closure itself holds the value, and the closure is
///   reached from the slot that holds it.
/// - `scopes` — **a root**. An `OpenScope` holds an `Rc<TaskScope>`, and a
///   scope owns the tasks spawned into it, whose settled values are Cove
///   values of this task's heap. A scope's *value* is also an ordinary slot
///   of the frame that opened it, so walking this is very nearly redundant —
///   but "very nearly" is not an invariant anything enforces, and the cost is
///   one iteration over a vector that is empty in every program that writes
///   no `scope`.
/// - `scalars` — not a root. The two stacks are numbered separately, so a
///   scalar slot is not a number in the value stack's space at all; an `i64`
///   holds no reference.
/// - `places` — not a root, and issue #162 did not make it one. A place is a
///   slot number and which stack it is in: a place rooted at a value slot
///   reaches only what that slot holds, which is inside the window already
///   walked, and a place rooted at a scalar slot reaches an `i64` and so
///   reaches nothing at all. Neither adds a reference the walk above does not
///   already see, and neither may be walked *again* — see [`Vm::places`] for
///   what that rests on and for what a moving collector would owe.
/// - `frames` — not a root. A `Frame` is four indices and a `FunctionId`.
/// - `constants` — not walked, and safe not to be. See [`Vm::constants`]: no
///   entry can reach a `Vector`, and walking one would put every constant
///   string into this backend's live-bytes figure and not the other's.
/// - `shapes`, `enums` — not roots. A `StructShape` is a name, a list of
///   field names, and a flag; an `EnumShape` is a name and an `Arc<EnumDecl>`
///   of the checker's. Neither holds a `Value`.
/// - `async_frames`, `stops`, `timings`, `fuel`, `instructions`,
///   `cancellation`, `task`, `wait` — not roots. Depths, flags, counters and
///   durations.
/// - `assertion_failure` — not a root. A `Span` and a `String`, which is
///   Rust's own and not a Cove value.
/// - `runtime`, `hosts`, `program`, `sources` — not roots, and not this
///   task's to walk. They are shared, immutable, and `Arc`-based for that
///   reason; a `cove_ir::Program` holds `Arc<str>` and no `Value` at all.
/// - `heap` — the heap doing the walking.
///
/// # What is deliberately not here
///
/// A `Value` the dispatch loop is holding in a Rust local at the moment a
/// safepoint is reached: a popped receiver, the `Vec<Value>` a host call was
/// handed, the failure a `?` is about to leave a frame with. None of those is
/// on a stack, and none of them needs to be. A Rust local *is* a reference,
/// so what it holds is short of the count the collector can see, and
/// `crate::heap` roots exactly that. [`Vm::collect_if_due`] goes through the
/// safepoints one at a time.
struct StackRoots<'v> {
    stack: &'v [Value],
    scopes: &'v [OpenScope],
}

impl Roots for StackRoots<'_> {
    /// Yields every value slot and operand, and then every open scope.
    ///
    /// Each reference is yielded once, which is what [`Roots`] asks for: the
    /// stack is a vector of distinct slots, and `scopes` holds one entry per
    /// `enter-scope` that has not been left. A scope's value is reachable
    /// twice — from its slot and from here — but those are two references and
    /// not one seen twice, so counting both is counting what is there.
    ///
    /// The `Value::TaskScope` a scope is yielded as is built here, which
    /// takes a reference of the collector's own and so makes the scope's
    /// count one higher than the program's. The effect is that a scope is
    /// rooted by the shortfall rule rather than directly, which reaches the
    /// same conclusion: an open scope is a root either way, and nothing else
    /// reads that count.
    fn walk(&self, visit: &mut dyn FnMut(&Value)) {
        for value in self.stack {
            visit(value);
        }
        for open in self.scopes {
            visit(&Value::TaskScope(Rc::clone(&open.scope)));
        }
    }
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
    /// place holds a slot number and the stack that number is in, so whatever
    /// it reaches is already reachable from that stack's own window — and for
    /// a place rooted at a scalar slot there is nothing to reach, because the
    /// slot holds an `i64`.
    ///
    /// **It must not be walked, rather than merely need not be.** Every
    /// `Value` a place can name is a value slot the walk above already
    /// counts, so walking a place as well would charge one value twice. That
    /// is the failure mode PR #192 kept `Vm::arg_vectors` out of the root set
    /// for, and it is the same argument: the collector's accounting survives
    /// a root it reaches by two routes only if it reaches it by one.
    ///
    /// That rests on a place never outliving the window it indexes, which is
    /// the property [`Place`] argues at length and which the collector does
    /// not re-derive. A *moving* collector is where it would stop being
    /// enough: relocating what a slot holds would leave the index naming the
    /// slot, which is right, but relocating the *stack* would not, and a
    /// collector that compacted the value stack would have to rewrite every
    /// place standing in this vector. This one moves nothing.
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
    ///
    /// Not walked as a root, and it does not have to be. `constant` builds a
    /// `Value::Unit`, `Bool`, `Int`, `Float`, `Duration`, or `Str` and
    /// nothing else, so no entry here reaches a `Vector` and none is a
    /// reference the collector's counting could be short of. Walking it
    /// anyway would be safe but would add every constant string to the live
    /// bytes this VM reports, which the interpreter has no equivalent of and
    /// would make the two backends' memory figures mean different things.
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
    /// It is collected at safepoints, from [`StackRoots`], and swept once
    /// more when it is retired. What it reports therefore means what the
    /// interpreter's heap reports: live storage rather than everything ever
    /// allocated.
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
    /// The run's budget, as this VM's safepoints charge it.
    ///
    /// `None` is a run with no budget installed, which is what an embedder
    /// that installed none has, and what it has always meant here: no limit.
    ///
    /// Taken once, where the run begins, rather than fetched at each
    /// safepoint through [`HostRegistry::with_budget`]'s mutex. That mutex
    /// was 36% of `benches/call` — every call and every return is a safepoint
    /// — and it was protecting counters that wanted to be atomics;
    /// [`crate::budget::Meter`] is where the whole of that argument is, and
    /// [`Vm::bind_budget`] is where this is filled in.
    budget: Option<Meter>,
    /// The host's `max_call_depth`, read off the budget when it was bound.
    ///
    /// Flattened out of the budget rather than asked for per call, because it
    /// cannot change while a run lasts and every call would otherwise ask.
    /// PR #144 established that and cached it behind an
    /// `Option<Option<usize>>` meaning "not asked yet" and "asked, no limit";
    /// there is nothing left to be lazy about now that the budget is bound
    /// where a run starts, so this is the limit or the absence of one.
    call_depth_limit: Option<usize>,
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
    /// The frame depths at which a call to a function that answers a settled
    /// task is standing, innermost last.
    ///
    /// An `async fn` runs its body at the call site and hands back a handle
    /// that is already settled, so what a call to one answers is not what its
    /// body produced. Wrapping has to happen where the frame *closes*, since
    /// a body ends by returning, by reaching its last instruction, or by a
    /// `?` that failed, and all three go through [`Vm::leave`].
    ///
    /// A stack of depths rather than a flag on the frame, because a `Frame`
    /// is five words copied into a local of the dispatch loop and read by
    /// every instruction that addresses a slot: a sixth field would be
    /// register pressure in the loop `benches/arith` spends its run in. This
    /// is the arrangement [`Vm::scopes`] uses, and it costs `leave` the same
    /// thing — one length check, on a vector that is empty in every program
    /// that writes no `async fn`.
    async_frames: Vec<usize>,
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
    /// The capture names each lowered function's closures hold, made once
    /// per VM rather than once per closure.
    ///
    /// `cove_ir::Function::captures` is a `Vec<Arc<str>>`, because a lowered
    /// program is read by every thread of a run;
    /// `value::Closure::captures` pairs an `Rc<str>` with each captured
    /// value, because a closure belongs to the task that built it. Turning
    /// one into the other is an allocation, and `Inst::MakeClosure` runs
    /// once per closure a program builds — so it is done once here instead,
    /// exactly as [`Vm::constants`] turns a constant's `Arc<str>` into a
    /// `Value::Str`'s `Rc<str>` once and for the same reason.
    ///
    /// What that saves is one `Rc<str>` per capture per closure. Issue #185
    /// asked whether anything but `benches/convention`'s `conv_fresh` pays
    /// for building closures, and the corpus says yes: a small helper that
    /// builds a `map` or `filter` callback in its body, called once per
    /// element by its caller, is the shape — `examples/life`'s
    /// `population()` is one, called once per creature per tick.
    ///
    /// Shared as an `Rc<[Rc<str>]>` so that `Vm::close_over` can hold the
    /// names while it drains the value stack, which it could not do through
    /// a borrow of `self`.
    ///
    /// Not a GC root, and it holds no `Value` to be one with.
    capture_names: Vec<Rc<[Rc<str>]>>,
    /// Argument vectors, lent to a builtin call and taken back after it.
    ///
    /// A builtin is handed its arguments in a `Vec<Value>` because several
    /// of them move an argument rather than reading it — `Vector.push` moves
    /// one into the storage, `fold` moves its accumulator through every
    /// callback call, `Ok` moves its payload into the value it builds — so a
    /// slice of the value stack would not do, quite apart from the reentry
    /// below. Issue #184 is the cost that made this worth arranging: one
    /// `Vec` allocated and dropped per builtin call, which
    /// `benches/arrayget` paid twice a turn.
    ///
    /// So the vectors are lent instead of made. [`Vm::borrow_args`] takes
    /// one from here and drains the arguments into it,
    /// [`Vm::return_args`] empties it and puts it back, and after the first
    /// turn of any loop the capacity is already there.
    ///
    /// A stack rather than a single scratch vector because a builtin
    /// re-enters: `map` calls a Cove callback through
    /// [`Vm::call_from_host`], and that callback may call a builtin of its
    /// own, which asks for a second vector while the first is still lent
    /// out. The nesting is the call depth, which the budget already bounds.
    ///
    /// **Nothing in here is a GC root, and it must not become one.**
    /// [`Vm::return_args`] clears a vector before storing it, so a stored
    /// vector holds no `Value` at all. A vector that is *lent out* holds
    /// values that are no longer on [`Vm::stack`], exactly as the vector
    /// `Vm::take` used to build did, and the collector reaches them the same
    /// way it always has — through [`crate::heap`]'s shortfall rule, which
    /// [`StackRoots`]'s documentation names as covering "the `Vec<Value>` a
    /// host call was handed". Walking these as roots as well would count
    /// each reference twice, which that module's documentation says is the
    /// one thing the accounting cannot survive.
    arg_vectors: Vec<Vec<Value>>,
}

impl<'a> Vm<'a> {
    /// A VM for `program`, running against `runtime` and calling through
    /// `hosts`.
    ///
    /// The run's budget is bound here, which is the one lock this takes and
    /// the last one a safepoint of this VM will be behind. It is sound to
    /// bind it this early because a budget cannot be installed once a VM
    /// exists: `HostRegistry::set_budget` needs `&mut HostRegistry` and this
    /// borrows the registry shared for `'a`. The one other way a budget is
    /// installed is `HostRegistry::begin_run`, which is reached only through
    /// [`Vm::invoke_within`] and its siblings, each of which rebinds.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Arc<Program>) -> Self {
        let mut vm = Vm {
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
            budget: None,
            call_depth_limit: None,
            cancellation: None,
            task: ENTRY_TASK,
            scopes: Vec::new(),
            async_frames: Vec::new(),
            stops: Vec::new(),
            timings: Vec::new(),
            wait: Duration::ZERO,
            assertion_failure: None,
            capture_names: program
                .functions
                .iter()
                .map(|function| {
                    function
                        .captures
                        .iter()
                        .map(|name| Rc::from(&**name))
                        .collect()
                })
                .collect(),
            arg_vectors: Vec::new(),
        };
        vm.bind_budget();
        vm
    }

    /// Takes the run's budget, in the form every safepoint of this VM will
    /// charge it, together with the call-depth limit that comes off it.
    ///
    /// Called where a run begins and nowhere else, for the reason
    /// [`crate::budget::Meter`] gives: a `Meter` names the accounting of the
    /// run it was taken from, and `HostRegistry::begin_run` gives the budget
    /// it installs fresh accounting. So [`Vm::new`] takes one, and the two
    /// ways in that install a budget of their own take another straight
    /// after installing it.
    fn bind_budget(&mut self) {
        self.budget = self.hosts.budget_meter();
        self.call_depth_limit = self
            .budget
            .as_ref()
            .and_then(|budget| budget.limits().max_call_depth);
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
    ///
    /// It trusts what it is given past the argument count. A `Value` whose
    /// parameter's slot is a scalar one is read as the `Int` or `Bool` the
    /// lowering promised it would be, and a caller that promised wrongly ends
    /// the process rather than getting an error back — because the lowering
    /// has spent the checker's answer by now and there is nothing here to hold
    /// the value to. [`Vm::invoke`] is the door that holds it, against the
    /// declaration the checker resolved, and is what an embedder should call.
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
        self.async_frames.clear();
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
        let outcome = self.execute(0).and_then(|value| match value {
            // The host awaits the entry it chose, so an `async fn` entry
            // hands back its value rather than a handle the host cannot
            // settle. `Interpreter::enter` does the same thing at the same
            // place, and through the same `crate::task::settle`.
            Value::Task(handle) => {
                let span = self.program.function(function).span;
                task::settle(self, &handle, span)
            }
            value => Ok(value),
        });
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
        // Whatever this run charged and had not yet handed over is handed
        // over now, however the run ended. See `Vm::spend_pending_fuel`.
        self.spend_pending_fuel();
        // Every task's thread has been joined by now — leaving a scope waits
        // for or cancels its children — so every heap but this one has been
        // retired and the totals are complete. `Interpreter::enter` ends the
        // same way and for the same reason, and the summary is what makes
        // `cove run --stats` and the trace's `heap_summary` say the same
        // thing on the two backends: what a run allocated, how often it
        // collected, and what it was still holding.
        self.retire_heap();
        let heap = self.heap_stats();
        self.runtime.trace(TraceEvent::HeapSummary {
            allocated: heap.allocated_objects,
            allocated_bytes: heap.allocated_bytes,
            collections: heap.collections,
            live_bytes: heap.live_bytes,
            peak_bytes: heap.peak_bytes,
            pause: heap.pause,
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
        self.ended(outcome)
    }

    /// Calls `module.name` with the arguments `args`, and reports how the run
    /// ended.
    ///
    /// [`crate::interp::Interpreter::invoke`] takes the same three things and
    /// answers the same way, and its documentation is the description of both:
    /// what a host may call, what holds its arguments, and what a refusal
    /// says. Selecting a backend selects which of the two to build and decides
    /// nothing else, exactly as it does for `run_entry`.
    ///
    /// Two refusals belong to this backend rather than to the language, and
    /// both are about the *lowering* rather than about the program.
    /// [`cove_ir::lower::lower_entry`] lowers what one entry can reach and
    /// nothing else, so a VM built for one entry cannot invoke a function no
    /// path from that entry leads to; this says so, and says what to lower
    /// instead. And a lowered `var` parameter is a slot of this VM's own
    /// stack, which a host has nothing to put in — though nothing reaches that
    /// refusal, because the check `invoke` makes first has already refused a
    /// `var` parameter from the declaration.
    pub fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_checked(module, name, args);
        self.ended(outcome)
    }

    /// The same call, bounded by `budget` and by nothing else.
    ///
    /// [`crate::interp::Interpreter::invoke_within`] takes the same four
    /// things and answers the same way, and its documentation is the
    /// description of both: what a budget belongs to, when the deadline starts
    /// running, and why there is no way to install one that does not take
    /// `&mut self`. Selecting a backend selects which of the two to build and
    /// decides nothing else, exactly as it does for `invoke`.
    pub fn invoke_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.checked_within(budget, module, name, args);
        self.ended(outcome)
    }

    /// The check, the budget, and then the call.
    ///
    /// In that order, so a call refused for a wrong argument spends none of
    /// the budget it was handed and leaves whatever bounded the backend where
    /// it was.
    fn checked_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        self.hosts.begin_run(budget);
        self.bind_budget();
        self.invoke_checked(module, name, args)
    }

    /// [`Vm::run_entry`], bounded by `budget` and by nothing else.
    ///
    /// The command-shaped way in, bounded the way [`Vm::invoke_within`] bounds
    /// the application-shaped one.
    pub fn run_entry_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        self.hosts.begin_run(budget);
        self.bind_budget();
        let outcome = self.invoke_entry(module, name, args);
        self.ended(outcome)
    }

    /// The check, and then the call.
    fn invoke_checked(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        let Some(id) = self.program.function_named(module, name) else {
            // The check above passed, so the package *does* declare this
            // function and the reader should not be told it does not. What is
            // missing is the lowering, and the remedy is the caller's.
            return Err(RuntimeError::new(format!(
                "this run's lowering does not include `{module}.{name}`"
            ))
            .with_rule(
                "A VM runs the functions one entry can reach, because that is what `lower_entry` lowers.",
            )
            .with_help(format!(
                "lower it too, with `cove_ir::lower::lower_entry(program, \"{module}\", \"{name}\")`, and build a `Vm` on that program"
            )));
        };
        self.run(id, args)
    }

    /// Writes a run's terminal event, whichever way in produced it.
    fn ended(&self, outcome: Result<Value, RuntimeError>) -> Result<Value, RuntimeError> {
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
    /// Nothing reads the dispatched instruction as a whole. Every helper
    /// an arm calls takes `running` and `pc` — both already live here — and
    /// reads `running.code[pc]` for itself, rather than being handed the
    /// `Inst` the `match` was on. That is not tidiness. An `Inst` is two
    /// words, and one that has to survive the dispatch is one the register
    /// allocator has to put somewhere: handed to five out-of-line helpers it
    /// went to the stack, so every instruction dispatched stored both words
    /// and each arm reloaded the field it wanted from there, and `pc` was
    /// spilled beside it. Letting it die at the `match` gives back the
    /// per-arm field loads the loop had before the helpers existed, and was
    /// worth about nine percent of `benches/arith` — measured, and recorded
    /// in `docs/VM_ARCHITECTURE.md`. Anything added here that wants the
    /// instruction after the dispatch should read it again.
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
        // instruction — which is what `Interpreter::call_target` does for the
        // entry as well.
        self.safepoint(running.span)?;
        self.charge(blocks[0], || running.span_at(0))?;

        loop {
            match code[pc] {
                Inst::Const(id) => self.stack.push(self.constants[id.0 as usize].clone()),
                Inst::LoadLocal(slot) => {
                    let value = self.stack[frame.base + slot as usize].clone();
                    self.stack.push(value);
                }
                Inst::StoreLocal(slot) => {
                    let value = self.pop();
                    self.stack[frame.base + slot as usize] = value;
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
                    self.stack.push(as_value_of(what, scalar));
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
                    // Read again rather than kept from the `match`, which
                    // is the loop's own rule: see the note on `Vm::execute`.
                    // This arm is the one place a payload alone does not say
                    // which of two instructions is running.
                    let taken_on = matches!(code[pc], Inst::JumpIfTrue(_));
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
                    if callee.answers_a_task {
                        self.async_frames.push(self.frames.len());
                    }
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
                    if let Some(entered) = self.closure_inst(running, pc)? {
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
                    if let Some(entered) = self.dyn_inst(running, pc)? {
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
                // Five instructions in one arm, calling an `#[inline(never)]`
                // helper, for the reason the place, closure, `dyn` and task
                // instructions have one: see `Vm::cold_inst`.
                Inst::CallResource { .. }
                | Inst::Snapshot
                | Inst::SpreadArgument
                | Inst::MakeRange { .. }
                | Inst::MakeHostEnum { .. } => self.cold_inst(running, pc)?,
                Inst::CallBuiltin { name: method, argc } => {
                    let span = running.span_at(pc);
                    let method = name(program, method);
                    let mut values = self.borrow_args(argc as usize);
                    let receiver = self.pop();
                    let answer = builtins::call_method(self, &receiver, method, &mut values, span);
                    self.return_args(values);
                    let value = answer?;
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
                    let mut values = self.borrow_args(argc as usize);
                    let answer =
                        self.make_builtin(which, &mut values, running.arg_spans_at(pc), span);
                    self.return_args(values);
                    self.stack.push(answer?);
                }
                Inst::MakeEnum { ty, case, argc } => {
                    let span = running.span_at(pc);
                    let case = name(program, case);
                    let mut payload = self.borrow_args(argc as usize);
                    let shape = self.enums[ty.0 as usize]
                        .as_ref()
                        .expect("every `make-enum` names an enum this VM shaped");
                    // The oracle's own constructor, so a case that does not
                    // exist and a payload of the wrong length are reported in
                    // the words `Interpreter::enum_case` reports them in.
                    let answer = crate::interp::enum_case(
                        self.runtime.program(),
                        &shape.module,
                        &shape.decl,
                        case,
                        &mut payload,
                        span,
                    );
                    self.return_args(payload);
                    let value = answer?;
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
                    let mut values = self.borrow_args(argc as usize);
                    let answer = builtins::call_associated(self, ty, which, &mut values, span);
                    self.return_args(values);
                    let value = answer?;
                    // `Vector.of` and `Map.of` are variadic and build one
                    // element per argument, so the arguments are what the
                    // cost follows rather than the one instruction.
                    self.fuel += u64::from(argc);
                    self.stack.push(value);
                }
                // Every place instruction, out of line: see `Vm::place_inst`
                // for why the bodies are not written here.
                Inst::PlaceLocal(_)
                | Inst::PlaceScalar(..)
                | Inst::LoadPlace(_)
                | Inst::PlaceField(_)
                | Inst::PlacePop
                | Inst::PlaceRead
                | Inst::PlaceWrite
                | Inst::Freeze => self.place_inst(running, pc, frame)?,
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
                | Inst::Cancel
                | Inst::Lock => self.task_inst(running, pc)?,
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
    fn place_inst(
        &mut self,
        running: &Function,
        pc: usize,
        frame: Frame,
    ) -> Result<(), RuntimeError> {
        let span = running.span_at(pc);
        match running.code[pc] {
            Inst::PlaceLocal(slot) => {
                // Absolute, because a place travels into a call and the
                // callee's `base` is a different number — see `Place`.
                self.places
                    .push(Place::rooted_at(frame.base + slot as usize));
            }
            Inst::PlaceScalar(slot, what) => {
                // The same thing on the other stack, and absolute for the
                // same reason: `frame.scalar_base` is the callee's own.
                self.places.push(Place::rooted_at_scalar(
                    frame.scalar_base + slot as usize,
                    what,
                ));
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
                // A scalar root has nothing to clone and the tag it puts
                // back on is the one the place carried — the same
                // conversion `Inst::ScalarToValue` performs, at the one
                // point a place is the thing that knows.
                let value = match place.root {
                    PlaceRoot::Scalar(slot, what) => as_value_of(what, self.scalars[slot]),
                    PlaceRoot::Value(_) => self.place_ref(&place).clone(),
                };
                self.stack.push(value);
            }
            Inst::PlaceWrite => {
                let value = self.pop();
                let place = self.pop_place();
                self.fuel += place.path.len() as u64;
                match place.root {
                    // `promised_scalar` is the boundary in the inward
                    // direction and stands on the same ground here: the
                    // place was built from a slot the checker settled as
                    // `Int` or `Bool`, and the checker settled what may be
                    // assigned through it as the same type.
                    PlaceRoot::Scalar(slot, _) => self.scalars[slot] = promised_scalar(&value),
                    PlaceRoot::Value(_) => *self.place_mut(&place) = value,
                }
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
        let mut current = &self.stack[place.value_root()];
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
        let mut current = &mut self.stack[place.value_root()];
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
    ///
    /// Kept for the calls that hand the values to something that takes them
    /// away for good — a Host operation, which crosses the public
    /// [`crate::host::HostModule`] boundary owning what it was given. Every
    /// call that gets its vector back uses [`Vm::borrow_args`] instead.
    fn take(&mut self, count: usize) -> Vec<Value> {
        let at = self.stack.len() - count;
        self.stack.drain(at..).collect()
    }

    /// The top `count` values, in a vector this VM lends and expects back.
    ///
    /// The counterpart of [`Vm::return_args`], and the two must be used as a
    /// pair — including on the failing path, since a builtin that returns an
    /// error has still finished with the vector. See [`Vm::arg_vectors`] for
    /// why a lent vector is not a GC root and why a returned one holds
    /// nothing.
    fn borrow_args(&mut self, count: usize) -> Vec<Value> {
        let mut args = self.arg_vectors.pop().unwrap_or_default();
        let at = self.stack.len() - count;
        args.extend(self.stack.drain(at..));
        args
    }

    /// Takes back a vector [`Vm::borrow_args`] lent, emptied.
    ///
    /// Emptied here rather than by the callee, so that a stored vector never
    /// holds a `Value` and the pool is never something the collector has to
    /// know about. Capacity survives, which is the whole point.
    fn return_args(&mut self, mut args: Vec<Value>) {
        args.clear();
        self.arg_vectors.push(args);
    }

    /// The five instructions that call a host resource, copy a value, spread
    /// an argument, build a range, or name a host enum's case.
    ///
    /// Out of line for the reason the place, closure, `dyn` and task
    /// instructions are: the dispatch body's footprint is a cost every
    /// program pays, and no benchmark executes a single one of these. They
    /// have nothing else in common — the grouping is by what the loop pays
    /// to carry them, not by what they do, which is why this is not named
    /// after a capability the way the other four are. Together they were
    /// about ninety-five lines inside the `match`, and taking them out was
    /// worth about three percent of `benches/arith`.
    #[inline(never)]
    fn cold_inst(&mut self, running: &Function, pc: usize) -> Result<(), RuntimeError> {
        let program: &Program = self.program;
        let span = running.span_at(pc);
        match running.code[pc] {
            Inst::CallResource { op, argc } => {
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
                let value = self.pop();
                let copied = builtins::snapshot(self, &value, span)?;
                // Priced like a builtin's answer, by `Inst::CallBuiltin`'s
                // own rule and for its own reason: this is a call to
                // `snapshot` written as an instruction rather than
                // dispatched by name, so it costs what that call costs.
                self.fuel += size_of_value(&copied);
                self.stack.push(copied);
            }
            Inst::SpreadArgument => {
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
            Inst::MakeHostEnum { ty, case } => {
                // The registry rather than the static schema, because
                // that is what `Interpreter::eval_field` asks: an
                // embedder's own host declares its types there, and a
                // module the lowering saw a schema for may be one the
                // run was never given.
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
            _ => unreachable!("`cold_inst` is called for the five instructions it names"),
        }
        Ok(())
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
        running: &Function,
        pc: usize,
    ) -> Result<Option<Frame>, RuntimeError> {
        let span = running.span_at(pc);
        match running.code[pc] {
            Inst::MakeClosure { function, captures } => {
                let value = self.close_over(function, captures);
                self.stack.push(value);
            }
            Inst::CallValue { argc } => {
                let callee = self.pop();
                match self.enter_value_call(&callee, argc, 0, pc as u32 + 1, span)? {
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
    /// Everything a host reads off a closure — how many parameters it
    /// declares, the module it belongs to, what it captured, whether it is
    /// `async` — is the same fact whichever backend made it, and
    /// `cove_ir::Function` carries each of them for exactly this. Only the
    /// body differs, and [`crate::value::ClosureBody`] is where that
    /// difference is written down.
    ///
    /// Nothing here copies syntax. It did: a lowered function carried the
    /// parameters source wrote so that this could clone them into the value,
    /// and every reader of them read their length. So what travels is the
    /// length — see [`crate::value::Closure::arity`], and issue #121 for the
    /// audit that looked for a second reader and found none.
    fn close_over(&mut self, function: FunctionId, captures: u16) -> Value {
        let target = self.program.function(function);
        let at = self.stack.len() - captures as usize;
        // Paired with the names the lowering settled, in the order it
        // settled them, which is the order the values were pushed.
        //
        // The names are taken from `Vm::capture_names` rather than made
        // here. They have to be copied out of the lowering at some point —
        // the lowering holds one program for every thread of a run and so
        // writes an `Arc<str>`, and a closure is a value of the task that
        // built it and so holds an `Rc<str>` — but that copy is one string
        // per capture *site*, not one per closure, and this instruction runs
        // once per closure. Issue #185 is what measured the difference:
        // `examples/life` builds a `filter` callback once per creature per
        // tick, and every one of them was allocating the string `creature`
        // afresh. The whole of the table's argument is on the field.
        let names = Rc::clone(&self.capture_names[function.0 as usize]);
        let held: Vec<(Rc<str>, Value)> =
            names.iter().cloned().zip(self.stack.drain(at..)).collect();
        // A closure is built from what it captured, so what it costs follows
        // how much that was — the rule `Inst::MakeEnum` is charged by.
        self.fuel += u64::from(captures);
        Value::Closure(Rc::new(Closure {
            // Read off the lowered function, because that is where `async`
            // ends up: a host that receives this closure reads the same field
            // off one the interpreter built.
            is_async: target.answers_a_task,
            // The same count the call convention checks an argument list
            // against, which `cove_ir::lower::validate` holds equal to
            // `params.len()`. A closure the interpreter built answers the
            // length of the parameters it will bind, and this is the lowered
            // form of that same number.
            arity: target.arity as usize,
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
    /// The order of the checks is `Interpreter::call_target`'s, through
    /// [`Vm::enter`]: the depth limit, the host's own, and the safepoint
    /// every call is. The arity is checked first and in the interpreter's
    /// words, because `bind_params` reports it before it reaches any of
    /// those.
    #[inline(never)]
    fn enter_value_call(
        &mut self,
        callee: &Value,
        argc: u16,
        place_argc: u16,
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
        if callee.arity != u32::from(argc + place_argc) {
            return Err(wrong_arity(callee, argc + place_argc, span));
        }
        self.enter(callee, span)?;
        if callee.answers_a_task {
            self.async_frames.push(self.frames.len());
        }
        let base = self.stack.len() - argc as usize;
        let scalar_base = self.scalars.len();
        self.scalars
            .resize(scalar_base + callee.scalar_frame_size as usize, 0);
        // Each capture goes into the slot its own kind names, which is the
        // whole of what issue #162 changed about one. A closure holds
        // `(name, Value)` pairs on both backends because a host reads them,
        // so this is the one point at which a capture the checker settled as
        // `Int` or `Bool` takes the representation its arithmetic wants —
        // once per call, in place of once per read.
        //
        // The two counters are the layout `cove_ir::Function::capture_kinds`
        // states and `cove_ir::lower::validate` checked: the value captures
        // are dense from `capture_base`, which is where these pushes land
        // because the arguments filled everything below it, and the scalar
        // captures are dense from scalar slot 0, which they can be because a
        // function a closure is made of takes no scalar argument.
        let mut next_scalar = scalar_base;
        for ((_, value), kind) in closure.captures.iter().zip(&callee.capture_kinds) {
            match kind {
                SlotKind::Scalar(_) => {
                    self.scalars[next_scalar] = promised_scalar(value);
                    next_scalar += 1;
                }
                // `validate` refuses a capture in a place slot, so the
                // remaining kind is the value stack.
                _ => self.stack.push(value.clone()),
            }
        }
        self.stack
            .resize(base + callee.value_frame_size as usize, Value::Unit);
        let place_base = self.places.len() - place_argc as usize;
        // Almost no closure has a place slot: `Inst::Lock`'s is the one whose
        // parameter can name storage rather than hold a value. So this is
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
        Ok(Entered::Frame(frame))
    }

    /// The two instructions a `dyn Trait` value needs, out of the loop.
    ///
    /// `Some` is the frame a dispatch opened, which is the one thing the
    /// caller has to do that this cannot: the running function, its code and
    /// its block table are locals of the loop.
    #[inline(never)]
    fn dyn_inst(&mut self, running: &Function, pc: usize) -> Result<Option<Frame>, RuntimeError> {
        let span = running.span_at(pc);
        match running.code[pc] {
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
    fn task_inst(&mut self, running: &Function, pc: usize) -> Result<(), RuntimeError> {
        let span = running.span_at(pc);
        match running.code[pc] {
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
            Inst::Lock => {
                let body = self.pop();
                let receiver = self.pop();
                let Value::Shared(cell) = receiver else {
                    unreachable!("a validated `lock` stands on a `Shared`");
                };
                // `SharedCell::lock` holds the cell for the whole of the
                // call, converts its contents into this task's own `Value` on
                // the way in, and converts back whatever the closure left in
                // it — all of which is the oracle's, unchanged. What is here
                // is where that value stands while the closure runs.
                let answered = cell.lock(span, |value| self.locked(&body, value, span))?;
                self.stack.push(answered);
            }
            other => unreachable!("`task_inst` was handed {other:?}"),
        }
        Ok(())
    }

    /// Runs a `lock`'s closure over `value`, and answers what it left behind
    /// beside what it returned.
    ///
    /// The contents stand in one slot of *this* frame's operands, at `at`,
    /// and the closure is entered above them — so `Vm::leave` truncates to a
    /// base above that slot and the value is still there to be read back.
    /// That slot is what a `var` parameter's place names, which is
    /// `Interpreter::call_shared_method`'s `Place::binding(value, true)`, and
    /// reading it afterwards is that method's `place.read`.
    ///
    /// The three stacks and the frame stack are restored to what they were
    /// whichever way the closure went, for the reason `Vm::call_from_host`
    /// restores them on a failure: the instruction that made this call has
    /// not finished, and its operands are below.
    fn locked(
        &mut self,
        body: &Value,
        value: Value,
        span: Span,
    ) -> Result<(Value, Value), RuntimeError> {
        let at = self.stack.len();
        let scalars = self.scalars.len();
        let places = self.places.len();
        let floor = self.frames.len();
        self.stack.push(value);
        let answered = self.run_locked(body, at, floor, span);
        let updated = self.stack[at].clone();
        self.stack.truncate(at);
        self.scalars.truncate(scalars);
        self.places.truncate(places);
        self.frames.truncate(floor);
        Ok((answered?, updated))
    }

    /// Enters the `lock` closure and runs it to its return.
    ///
    /// Which convention it takes is the callee's own `params`, because it is
    /// the callee's to state: a closure written `fn(var value)` takes a place
    /// naming the slot at `at`, and one written `fn(value)` takes a copy of
    /// what stands there — which is exactly the difference
    /// `Interpreter::call_shared_method` reads off `param.is_var`.
    fn run_locked(
        &mut self,
        body: &Value,
        at: usize,
        floor: usize,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let takes_place = match body {
            Value::Closure(closure) => match closure.body {
                ClosureBody::Lowered(target) => {
                    matches!(
                        self.program.function(target).params.first(),
                        Some(SlotKind::Place)
                    )
                }
                ClosureBody::Tree { .. } => false,
            },
            _ => false,
        };
        let (argc, place_argc) = match takes_place {
            true => {
                self.places.push(Place::rooted_at(at));
                (0, 1)
            }
            false => {
                let copy = self.stack[at].clone();
                self.stack.push(copy);
                (1, 0)
            }
        };
        // The resumption point is never read, for the reason
        // `Vm::run_callback`'s is not: this frame answers at the floor rather
        // than resuming an instruction.
        match self.enter_value_call(body, argc, place_argc, 0, span)? {
            Entered::Answer(value) => Ok(value),
            Entered::Frame(_) => self.execute(floor),
        }
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
            let mut values = self.borrow_args(argc as usize - 1);
            let receiver = self.pop();
            let answer =
                builtins::call_method(self, &receiver, &dispatch.method, &mut values, span);
            self.return_args(values);
            let value = answer?;
            self.fuel += size_of_value(&value);
            self.stack.push(value);
            return Ok(None);
        };
        let callee = program.function(target);
        if callee.arity != u32::from(argc) {
            return Err(wrong_arity(callee, argc, span));
        }
        self.enter(callee, span)?;
        if callee.answers_a_task {
            self.async_frames.push(self.frames.len());
        }
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
    /// asking twice in a row charges zero fuel the second time and changes no
    /// answer.
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
            // A frame abandoned by a failure never reached `Vm::leave`, so
            // what it recorded here goes with it.
            self.async_frames.retain(|depth| *depth < floor);
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
        match self.enter_value_call(callee, argc, 0, 0, span)? {
            Entered::Answer(value) => Ok(value),
            Entered::Frame(_) => self.execute(floor),
        }
    }

    /// Checks what a call is allowed to do before it does it.
    ///
    /// The three checks, in the order `Interpreter::call_target` makes them: the
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
        if let Some(limit) = self.call_depth_limit {
            if depth > limit {
                // There is a budget: the limit was read off one. The error
                // names the value it was configured with, which is why it is
                // built there rather than here.
                if let Some(budget) = &self.budget {
                    return Err(budget.to_runtime_error(Stopped::CallDepth).at(span));
                }
            }
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
    fn leave(&mut self, mut answer: Answered, floor: usize) -> Answer {
        let done = self.frames.pop().expect("a return leaves a frame");
        // An `async fn` and an `async` lambda answer a task that is already
        // settled: the body ran here, at the call, and only `await` produces
        // the value. Wrapping where the frame closes catches every way a body
        // can end — a `return`, the last instruction, and a `?` that failed —
        // which is what `Interpreter::call_target` gets by wrapping the result of
        // the whole call. Guarded rather than read off the callee, for the
        // reason `Vm::async_frames` gives.
        if self.async_frames.last() == Some(&self.frames.len()) {
            self.async_frames.pop();
            let value = match answer {
                Answered::Value(value) => value,
                // Unreachable: a function that answers a task answers on the
                // value stack, whatever it declared, and `crate::lower` is
                // what makes that so.
                Answered::Scalar(scalar) => {
                    as_value(self.program.function(done.function).returns, scalar)
                }
            };
            answer = Answered::Value(Value::Task(Task::settled(value)));
        }
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
    /// One last collection runs first, for the reason
    /// `Interpreter::retire_heap` gives and which applies here word for word:
    /// a heap dies with the thread that owns it, and a table of `Weak`s
    /// dropped without a sweep takes nothing with it — so a task that ends
    /// while a cycle it built is still reachable would leave that cycle
    /// behind, which is the one thing this collector exists to prevent.
    ///
    /// By the time this runs the stacks are empty: a return truncates to the
    /// frame's base, the entry's base is zero, and `Vm::close_scopes_above(0)`
    /// has left every scope. What is left to survive is what the value the
    /// task produced still holds, which is a Rust local of the caller and so
    /// is found by the reference counts, exactly as any other value the
    /// collector cannot read is.
    fn retire_heap(&mut self) {
        if !self.heap.is_empty() {
            self.collect();
        }
        let stats = self.heap.take_stats();
        self.runtime.retire_heap(&stats);
    }

    // --------------------------------------------------------- collection

    /// Marks and sweeps this task's heap from [`StackRoots`], and records
    /// what it did.
    ///
    /// The event is `Interpreter::collect`'s, with the same fields written
    /// from the same task id, so `cove trace` reads a collection on this
    /// backend exactly as it reads one on the other.
    #[inline(never)]
    fn collect(&mut self) -> Collection {
        let collected = {
            let roots = StackRoots {
                stack: &self.stack,
                scopes: &self.scopes,
            };
            self.heap.collect(&roots)
        };
        self.runtime.trace(TraceEvent::HeapCollected {
            task: self.task,
            allocated: collected.allocated,
            freed: collected.freed_objects,
            live_objects: collected.live_objects,
            live_bytes: collected.live_bytes,
            pause: collected.pause,
        });
        collected
    }

    /// Collects when enough has been allocated since the last collection to
    /// be worth another one.
    ///
    /// # Why every safepoint is a safe one
    ///
    /// A collection is correct at a point where every live value is either
    /// walked by [`StackRoots`] or invisible to it. The second half is the
    /// one that does the work, and it is not a loophole: `crate::heap`
    /// compares the references it can see against `Rc::strong_count`, and a
    /// `Value` in a Rust local is a reference it cannot see, so a value the
    /// dispatch loop has taken off the stack is short by one and is a root.
    /// That is the same rule that roots the interpreter's evaluator
    /// temporaries, and neither backend would be sound without it.
    ///
    /// So what has to be checked at a safepoint is not "is everything on the
    /// stack" — it need not be — but that nothing is walked twice, since a
    /// reference counted twice is a shortfall concealed. Every site was
    /// checked, and this is what each holds:
    ///
    /// - **Entering the entry**, in [`Vm::execute`]. The frame is pushed and
    ///   its window resized before the loop starts, so the arguments are
    ///   slots and nothing is off the stack.
    /// - **A call**, in [`Vm::enter`]. The caller pushed the arguments as its
    ///   own operands and the callee's window opens on them, so they are on
    ///   the stack before and after; nothing is in a local. The same is true
    ///   of the second safepoint the `Call` arm takes through [`Vm::charge`],
    ///   which happens before the window is resized and so sees the arguments
    ///   as operands rather than as slots — the same values either way.
    /// - **A return**, at `Inst::Return` and `Inst::ReturnScalar`. The
    ///   safepoint is taken *before* the answer is popped, so the answer is
    ///   still an operand. This was already so and is worth keeping so.
    /// - **A `?` that failed**, at `Inst::Try`. Here the failure *is* in a
    ///   local: it was popped, opened, and found to be an `Err` before the
    ///   safepoint. It is rooted by its own reference, and it is reached from
    ///   nowhere the walk goes, so it is counted once and not at all.
    /// - **A back edge**, in [`Vm::back_edge`]. The condition was already
    ///   consumed by the jump that took it; the stack holds the frame's slots
    ///   and whatever operands the block left standing.
    /// - **The per-block charge**, in [`Vm::charge`]. A block head is a point
    ///   control arrives at, and the operands standing there are whatever the
    ///   previous line left — all of them on the stack, since an instruction
    ///   that has not run has taken nothing off it.
    /// - **A host call**, at `Inst::CallHost` and `Inst::CallResource`, and
    ///   any Cove callback the host re-enters with. [`Vm::take`] drains the
    ///   arguments into a `Vec<Value>` and `Inst::CallResource` pops the
    ///   receiver besides, so at every safepoint the callback reaches, those
    ///   values are in Rust locals below the re-entrant frames. Each is
    ///   rooted by its own reference. The re-entrant call's own frames are
    ///   pushed above the standing ones, so the walk sees the interrupted
    ///   frames and the callback's together, once each.
    /// - **An `await`**, at `Inst::Await`. The safepoint is taken before the
    ///   handle is popped, so the task is an operand.
    /// - **Inside a `lock`**, at `Inst::Lock`. The closure runs on frames
    ///   above, and the cell's contents stand in a slot of this frame; the
    ///   closure value itself is a local, rooted by its reference. The
    ///   collector never takes the cell's lock, which is what stops a
    ///   collection under one from deadlocking; `crate::heap` says why a
    ///   `Shared` is a leaf.
    /// - **Leaving or cancelling a scope**, at `Inst::LeaveScope` and
    ///   `Inst::CancelScope`. The scope is popped out of [`Vm::scopes`]
    ///   before its children are waited for, so during that wait it is a
    ///   local rather than a walked root — and is rooted by its reference,
    ///   like anything else the collector cannot read.
    ///
    /// There is no site at which a live value is neither walked nor held by
    /// an invisible reference, which is the property that makes the list
    /// above a check rather than a hope.
    #[inline]
    fn collect_if_due(&mut self) {
        if self.heap.should_collect() {
            self.collect();
        }
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
    ///
    /// A safepoint is also where this task's heap is collected, when enough
    /// has been allocated to be worth it. It is asked last, after the stops,
    /// because a run that is ending has no use for a collection — and
    /// `Interpreter::charge_safepoint` asks in that order too.
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

    /// Spends whatever this VM has charged and not yet handed to the run's
    /// budget, at the end of a run or of a task's thread.
    ///
    /// Every ordinary way out of a body already flushes: a `return` and a
    /// `?` that failed take a safepoint before they leave the frame, and a
    /// safepoint is what spends. What does not flush is every other way a
    /// run can end — a raised error, an exhausted budget, a cancelled task,
    /// a bounded call abandoned by the host that bounded it — because each
    /// of those leaves through `?` in Rust rather than through an
    /// instruction. The fuel charged since the last safepoint was work the
    /// run really did, so it is spent here rather than dropped with the
    /// stacks.
    ///
    /// [`crate::budget::Budget::spend`] rather than [`Vm::safepoint`],
    /// because this runs after the answer is settled: a stop raised here
    /// would replace the reason the run actually ended.
    fn spend_pending_fuel(&mut self) {
        let fuel = std::mem::take(&mut self.fuel);
        if fuel != 0 {
            if let Some(budget) = &self.budget {
                budget.spend(fuel);
            }
        }
    }

    fn safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        stopped_here(self.cancellation.as_ref(), &self.stops, span)?;
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(budget) = &self.budget {
            if let Err(stopped) = budget.safepoint(fuel) {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        self.collect_if_due();
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
    /// exactly what an interpreted run is held to — this adds the timing,
    /// the span, and the two stop flags a `Budget` shared by every task
    /// cannot ask about. [`stopped_here`] is the whole of that last part,
    /// and the interpreter calls it here too.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        stopped_here(self.cancellation.as_ref(), &self.stops, span)?;
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
        stopped_here(self.cancellation.as_ref(), &self.stops, span)?;
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
        values: &mut Vec<Value>,
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
                .zip(values.drain(..))
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
///
/// It is the only reader of `cove_ir::Function::param_names`, and the reason
/// a lowered function keeps any part of its written parameter list at all: a
/// name is what makes "needs an argument for `label`" say which one, and a
/// slot kind cannot say it.
fn wrong_arity(callee: &Function, argc: u16, span: Span) -> RuntimeError {
    if u32::from(argc) > callee.arity {
        return RuntimeError::new(format!(
            "`this closure` takes {} argument(s), but more were given",
            callee.arity
        ))
        .at(span);
    }
    let missing = callee
        .param_names
        .get(argc as usize)
        .map_or_else(|| format!("argument {}", argc + 1), |name| name.to_string());
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

/// The `Value` a scalar word stands for, told which of the two it is.
///
/// [`as_value`] asks a [`SlotKind`] the same question; this asks the
/// [`Scalar`] itself, because the two callers that reach it — the
/// `Inst::ScalarToValue` arm and a read through a place rooted at a scalar
/// slot — hold that and not a slot kind.
fn as_value_of(what: Scalar, scalar: i64) -> Value {
    match what {
        Scalar::Int => Value::Int(scalar),
        Scalar::Bool => Value::Bool(scalar != 0),
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
    /// loop: this task's own cancellation, every bounded call this thread is
    /// inside, and the run's own cancellation.
    ///
    /// The task's flag is read here because a host that polls between rounds
    /// is asking the question a safepoint asks, and a spawned task's
    /// cancellation is one of the answers: `clock.every` in a cancelled task
    /// ends its timer rather than raising out of it, and it can only do that
    /// if this says so. It was missing, so the VM answered `false` where
    /// `Interpreter`'s answer was `true` — the one place the two backends
    /// gave a host different accounts of the same run.
    fn is_cancelled(&self) -> bool {
        if self
            .vm
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return true;
        }
        if self.vm.stops.iter().any(Cancellation::is_cancelled) {
            return true;
        }
        self.vm.budget.as_ref().is_some_and(Meter::is_cancelled)
    }

    /// What the run's deadline leaves, read from the one budget that knows
    /// when the run started.
    fn time_left(&self) -> Option<Duration> {
        let budget = self.vm.budget.as_ref()?;
        let deadline = budget.limits().deadline?;
        Some(deadline.saturating_sub(budget.elapsed()))
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
    // What this task charged and had not yet spent is the run's fuel as much
    // as the entry's is, and a task that raised or was cancelled reached no
    // safepoint to spend it at. See `Vm::spend_pending_fuel`.
    vm.spend_pending_fuel();
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
    /// closure this backend built because `close_over` writes the lowered
    /// function's `arity` into the value. `mapError` reads this to decide
    /// whether to hand its callback the error it replaces, so a lowered
    /// closure that answered `0` where an interpreted one answered `1` would
    /// be the two backends disagreeing about what `mapError` passes.
    fn arity(&self, callee: &Value) -> Option<usize> {
        match callee {
            Value::Closure(closure) => Some(closure.arity),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
