//! The executable IR a Cove program is lowered to, and the lowering itself.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) decides why this
//! exists. The short version is that a tree-walking interpreter re-derives, on
//! every evaluation, facts that were settled before the program ran — where a
//! binding lives, what a call targets, how big a frame is — and a tree has
//! nowhere to put an answer so that using it costs nothing. This is that
//! somewhere: an index is *in* the instruction, and a frame is a region of a
//! stack whose size was known when the function was lowered.
//!
//! # This is a lowering, not a second source of truth
//!
//! `cove-sema` already answers what every reference denotes. Nothing here
//! re-derives that; it records the answers in a shape the VM can act on
//! without asking again. Where the two could disagree, the checker is right by
//! construction, because the lowering reads its answers rather than
//! recomputing them.
//!
//! Lowering runs once per program and may allocate freely. Execution is the
//! thing being made fast, and nothing else is.
//!
//! # Two stacks, and a slot is in one of them
//!
//! An operand or a slot whose type the checker settled as `Int` or `Bool`
//! lives in a stack of `i64` — [`SlotKind::Scalar`] — and everything else
//! lives in the stack of `Value` it always did. The point is negative rather
//! than positive: a loop that adds two integers should not move a 40-byte
//! tagged value or run its drop glue to do it, and an instruction that says
//! which stack it reads is how that is arranged without asking at run time.
//!
//! Nothing is guessed. A slot is scalar only where `Facts::ty` answered
//! `Some(Ty::Int)` or `Some(Ty::Bool)`; `None` and `Some(Ty::Unknown(_))`
//! are the checker declining, and a function it declined about keeps the
//! representation it had. [`Inst::ScalarToValue`] and [`Inst::ValueToScalar`]
//! are the boundary between the two, emitted where a scalar meets something
//! general — an assertion, a struct field, a string being interpolated, a
//! builtin method's answer.
//!
//! A call is not one of those. [`Function::params`] and [`Function::returns`]
//! carry the same rule across a call boundary, so an argument whose type the
//! checker settled is pushed onto the scalar stack and becomes the callee's
//! scalar slot without moving, and an answer comes back the way it was
//! computed.
//!
//! # A third region, for the arguments that are not values either
//!
//! A `var` parameter does not receive a copy of anything. It names the
//! caller's own storage, so that `bump(var total)` adds to the binding the
//! caller wrote and not to a copy that is written back afterwards — which is
//! observably a different language, since `two(var x, var x)` answers 11
//! rather than 10 and only aliasing gives that answer.
//!
//! What names storage here is a *place*: one slot number in the one frame
//! numbering, together with the field positions to walk from what stands
//! there. [`SlotKind::Place`] is the third kind a slot can have,
//! [`Region::Place`] is the third region the numbering runs, after the
//! scalar and value regions, and [`Function::place_frame_size`] is that
//! region's width the way the other two have theirs. Nothing about it is new
//! machinery: it is [`SlotKind::Scalar`]'s arrangement asked of a third
//! representation, and `Inst::Call`'s per-stack counts, `Function::params`,
//! and `validate` already generalise over the question.
//!
//! **A place names a slot, and it does not care which region the slot is
//! in.** That is issue #162's one slot identity, and
//! [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
//! decision 1 — one logical frame, one slot numbering, one base — is what
//! makes it literally true rather than aspirational. [`Inst::PlaceLocal`]
//! and [`Inst::PlaceScalar`] hand out numbers from the same one numbering
//! rather than from two numbering spaces that separately called their first
//! slot zero, and [`Function::region_of`] alone is what says which region a
//! given number falls in — nothing about the number itself has to say so. A
//! `var` argument rooted at a binding the checker settled as `Int` therefore
//! leaves the binding where its arithmetic wants it. It could not before
//! there was one numbering to place it in — the lowering kept every such
//! binding on the value stack for the whole of the body that named it, and
//! paid a conversion on every read and every write of it.
//!
//! An index rather than a pointer, because a stack is one `Vec` that grows,
//! and an index survives a reallocation that a borrow could not even be
//! written across. It is sound for as long as the frame it addresses is
//! live, and a place never leaves the frame that built it — see the note on
//! [`Inst::PlaceLocal`], which is where that is argued now that closures
//! lower.
//!
//! # A closure is a function and the values it was handed
//!
//! A lambda is lowered to a [`Function`] of its own, and what the
//! environment around it handed that function is [`Function::captures`]: a
//! list of names settled when the lambda was lowered, whose values the
//! enclosing body pushes at [`Inst::MakeClosure`] and which the call copies
//! into the value slots after the parameters. The interpreter decides the
//! same set — the names the body mentions, intersected with what is live —
//! but decides it as the closure is created, so a capture's position is a
//! run-time fact there and a static layout here. That is ADR 0019's "slots,
//! not names" asked of a capture.
//!
//! [`Inst::CallValue`] is the other half, and it is why a closure's
//! convention is fixed rather than read off the callee: nothing at a call
//! through a value knows which function it will reach, so every argument
//! travels on the value stack and the answer comes back on it. A declared
//! function *used* as a value is numbered as a second specialisation under
//! that convention rather than called under its own.
//!
//! One closure is called by something other than a `call-value`, and it is
//! the only one whose convention differs. [`Inst::Lock`] hands its closure a
//! *place* naming the cell's contents, because a closure written
//! `fn(var value)` changes what the cell will hold rather than a copy of it;
//! that parameter therefore takes no value slot, and each
//! [`Capture`]'s own `slot` is one slot later as a result. It is sound
//! because the lowering builds such a closure only as the argument of the
//! `lock` that consumes it, so it never becomes a value a `call-value` could
//! reach.
//!
//! # One program, and every thread of a run reads it
//!
//! ADR 0008 runs a spawned task on a thread of its own, and a task's body is
//! a lowered function like any other — so the thread that runs it needs the
//! program the body's id names. That program is *this* one. A [`Program`] is
//! immutable once lowered and every string in it is an `Arc<str>`, so one of
//! them is shared by every thread of a run rather than rebuilt on each.
//!
//! It has to be the same program and not a copy of it, and that is the whole
//! reason for the `Arc`. A `cove_runtime::value::ClosureBody::Lowered` is a
//! [`FunctionId`], which means a position in one program's `functions`; a
//! task handed a program lowered a second time would be reading ids that
//! happened to line up, which is not an invariant anything states. Sharing
//! the one program is what makes a `FunctionId` mean the same function on
//! whichever thread reads it.
//!
//! What crosses is therefore the program and nothing else about the
//! execution. A `cove_runtime::Value` still cannot cross — it is `Rc`-based,
//! and `cove_runtime::task::Transfer` is the form a value crosses in — and
//! neither can a `Vm`, which owns stacks, a frame stack, and a heap. Those
//! are rebuilt on the far side out of the program and the run, which is
//! exactly what `cove_runtime::interp::Interpreter` already does with the
//! checked program.
//!
//! # What is not here
//!
//! No serialization, no version number, no compatibility promise. Nothing
//! outside this repository consumes the IR, and ADR 0019 says it stays that
//! way until there is evidence worth stabilizing.
//!
//! A construct the lowering does not cover yet is reported as
//! [`Unsupported`] rather than approximated. ADR 0019's no-silent-fallback
//! rule is what makes that the right answer: a VM that quietly finished a run
//! on the interpreter would be a VM whose measurements are about a mixture and
//! whose conformance is about whatever it happened to cover.

pub mod lower;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use cove_diag::{Diagnostic, Span};

/// One lowered program: every function it can execute, and the constants they
/// share.
#[derive(Debug, Default)]
pub struct Program {
    /// Every lowered function, addressed by [`FunctionId`].
    pub functions: Vec<Function>,
    /// Every constant any function loads, addressed by [`ConstId`].
    pub constants: Vec<Const>,
    /// Every dynamic dispatch site, addressed by [`DispatchId`].
    pub dispatches: Vec<Dispatch>,
    /// Every declared struct type the program builds or reads a field of,
    /// addressed by [`StructId`].
    ///
    /// This is the **type's** layout, not one construction's: see
    /// [`StructType`].
    pub structs: Vec<StructType>,
    /// Every declared enum type the program builds a case of or reads a
    /// payload of, addressed by [`EnumId`] and, for the cases
    /// [`Inst::MakeEnum`] reached before anything read one, in the order it
    /// reached them.
    ///
    /// This is the **type's** cases and each case's payload kinds, read off
    /// the checker's settlement of the case rather than off how one
    /// instance happened to be built — [`EnumType`] is [`StructType`]'s rule
    /// applied to a case's payload in place of a struct's fields.
    pub enums: Vec<EnumType>,
}

impl Program {
    /// The function `id` names.
    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id.0 as usize]
    }

    /// The constant `id` names.
    pub fn constant(&self, id: ConstId) -> &Const {
        &self.constants[id.0 as usize]
    }

    /// The dispatch site `id` names.
    pub fn dispatch(&self, id: DispatchId) -> &Dispatch {
        &self.dispatches[id.0 as usize]
    }

    /// The struct type `id` names.
    pub fn struct_type(&self, id: StructId) -> &StructType {
        &self.structs[id.0 as usize]
    }

    /// The enum type `id` names.
    ///
    /// [`Inst::GetPayload`] carries this rather than a name, for the reason
    /// [`Inst::GetFieldAt`] carries a [`StructId`] rather than one: an index
    /// is a lookup this backend does not have to scan a table for, and the
    /// direction of travel for a fact a hot path reads is away from a
    /// by-name one.
    pub fn enum_type(&self, id: EnumId) -> &EnumType {
        &self.enums[id.0 as usize]
    }

    /// The enum type declared under the qualified name `name`, if
    /// [`Inst::MakeEnum`] or [`Inst::GetPayload`] anywhere reached it.
    ///
    /// A linear scan rather than a table, because [`Program::enums`] is one
    /// entry per declared type a program actually builds a case of or reads
    /// a payload of — small for any program this crate lowers — and this is
    /// asked once per site while a backend is built, not once per execution
    /// of one. [`Program::enum_type`] is the same table addressed by
    /// [`EnumId`] instead, which is what an instruction that already carries
    /// one reads through.
    pub fn enum_type_named(&self, name: &str) -> Option<&EnumType> {
        self.enums.iter().find(|declared| &*declared.name == name)
    }

    /// The function a qualified name denotes, if this program lowered one.
    ///
    /// This is how an entry point is found, once, before a run. Nothing on a
    /// hot path looks a function up by name.
    pub fn function_named(&self, module: &str, name: &str) -> Option<FunctionId> {
        self.functions
            .iter()
            .position(|function| &*function.module == module && &*function.name == name)
            .map(|index| FunctionId(index as u32))
    }
}

/// One lowered function: what it needs to run and the instructions that run
/// it.
#[derive(Debug)]
pub struct Function {
    /// The module that declares it, and the name it declares — both kept for
    /// diagnostics and traces, and read by nothing on a hot path.
    pub module: Arc<str>,
    pub name: Arc<str>,
    /// How many slots the *value* region needs: `self` if it has a
    /// receiver, then each parameter `params` names a value slot for, then
    /// every `Value` local and temporary the body declares. The width of
    /// [`Region::Value`].
    ///
    /// [`Inst::LoadLocal`] and [`Inst::StoreLocal`] never carry a number
    /// outside this region — [`Function::region_of`] is what says a given
    /// slot number is in it, and `crate::lower::validate` checks every one
    /// against it. The numbers themselves are not one contiguous run the way
    /// they were before parameters named their own slot:
    /// `slots[0..arity]` holds every parameter in declaration order,
    /// whichever region each one's own kind names, so a value parameter's
    /// number can stand between two scalar parameters'. [`Function::offset`]
    /// is where a value slot's position *within* this region is answered —
    /// what a backend keeping this region in an array of its own needs in
    /// place of the number itself.
    ///
    /// That makes this the frame metadata a precise root set is read from: a
    /// frame's whole value window, `stack[base .. base + value_frame_size]`
    /// at run time, is its root set, with nothing to skip inside it, because
    /// a scalar slot's number never falls in this region —
    /// [`Function::region_of`] puts it in [`Region::Scalar`] instead,
    /// addressed by [`Inst::LoadScalar`] and [`Inst::StoreScalar`].
    pub value_frame_size: u32,
    /// How many slots the *scalar* region needs: every `Int` or `Bool`
    /// parameter, local, and temporary the body declares. The width of
    /// [`Region::Scalar`].
    ///
    /// [`Inst::LoadScalar`] and [`Inst::StoreScalar`] never carry a number
    /// outside this region, checked the way [`Function::value_frame_size`]
    /// describes for [`Inst::LoadLocal`]. A scalar parameter is counted here
    /// and a value parameter is counted in `value_frame_size`, because an
    /// argument arrives on the stack its own type names and becomes the
    /// callee's slot there without moving; see `params`. [`Function::offset`]
    /// answers where a scalar slot stands within this region, now that its
    /// number is not that position by construction.
    pub scalar_frame_size: u32,
    /// How many slots the *place* region needs, which today is every `var`
    /// parameter and a `var self` receiver and nothing else. The width of
    /// [`Region::Place`].
    ///
    /// [`Inst::LoadPlace`] never carries a number outside this region,
    /// checked the same way; nothing stores into it,
    /// because a place slot is filled by the calling convention and never
    /// assigned. A body declares no place slots of its own: `var` is a
    /// property of a parameter, and a local that a `var` argument is rooted
    /// at is an ordinary slot of whichever region its own type named, which
    /// a place *names* rather than being a place itself.
    ///
    /// Not a root set, and not a hole in one either. A place holds a slot
    /// number — one number in the one numbering, so [`Function::region_of`]
    /// alone says which region it names — and a path of field positions, so
    /// it reaches a `Value` only by way of the value region, whose own
    /// window is already scanned — and a place rooted at a scalar slot
    /// reaches no `Value` at all.
    pub place_frame_size: u32,
    /// The kind of every slot this function's frame has, indexed by its
    /// number in the one frame numbering — `slots.len()` is
    /// [`Function::slot_count`] and `slots[n]` is what
    /// [`Function::region_of`] would place slot `n` in, read one level more
    /// specifically.
    ///
    /// This is the per-slot layout
    /// [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
    /// decision 1 means by "what a slot means comes from `cove_ir::Function`'s
    /// per-slot layout": the three frame sizes above say how wide each region
    /// is, and this says what each of the numbers inside the frame *is* — a
    /// `Value`, an `Int`, a `Bool`, or a place — without a reader having to
    /// already know which instruction touches it. [`Function::region_of`] is
    /// this table read one level less specifically, for the reader — a
    /// collector, or `crate::lower::validate` — that only needs to tell
    /// [`Region::Value`] from the other two.
    ///
    /// `slots[0..arity]` is `params` exactly, because a parameter's slot
    /// number *is* its declaration index: an argument arrives on the stack
    /// its own kind names and becomes that stack's next slot without moving,
    /// and the one numbering has to agree with that arrival order rather than
    /// group every parameter by region first. Everything from `arity` on is
    /// this body's own — the remaining scalar slots, then the remaining
    /// value slots, then the place slots, each run dense within its own
    /// region.
    ///
    /// Built by `Body::finish` out of `Body`'s three per-region layouts,
    /// which is where the entries are actually decided: `Body::allocate`
    /// records a binding's kind at the region-local number it hands out, and
    /// `Body::finish` is what permutes a region-local number into this
    /// array's index — see it for the permutation. See `Body::allocate` for
    /// the one place this table cannot be exact by construction — a scalar
    /// number two scopes reuse for an `Int` and then a `Bool` — and the rule
    /// that keeps it exact anyway.
    pub slots: Vec<SlotKind>,
    /// Where slot `n` stands within its own region: the number of slots of
    /// the same region that precede it in the one numbering. Indexed by slot
    /// number, exactly as `slots` is — see [`Function::offset`], which is
    /// this table read.
    ///
    /// A three-array backend keeps each region in a `Vec` of its own and
    /// needs a slot's position in that `Vec`, not its number in the one
    /// numbering: `cove_runtime::vm` derives one base per region per frame
    /// and adds this to reach a word. Deriving it from the three frame sizes
    /// alone — as `Function::value_origin` used to let a backend do by
    /// subtraction — stopped being possible once a parameter's slot number
    /// could stand anywhere in `0..arity` regardless of its region, so this
    /// is a table rather than arithmetic: `Body::finish` builds it once, at
    /// lowering, alongside `slots`, so a hot path pays one indexed load and
    /// not a search.
    pub offsets: Vec<u32>,
    /// How many arguments a call must supply, `self` included.
    pub arity: u32,
    /// Which stack each of those arguments arrives on, in the order a call
    /// supplies them — the receiver first when `has_receiver`, then the
    /// declared parameters in declaration order.
    ///
    /// This is the calling convention, and it is the callee's to state
    /// because the callee is what a slot number means. An argument is
    /// pushed onto the stack its own settled type names and *becomes* the
    /// callee's slot in that stack without moving, and the one numbering
    /// puts every parameter — whichever kind it is — in `0..arity`, in this
    /// declaration order: `params[at]` is `slots[at]`, and `Function::offset`
    /// is where that argument's position within its own physical stack is
    /// answered. `params.len()` is `arity`, which `validate` checks rather
    /// than assumes.
    ///
    /// Written out rather than derived, because a caller has to place its
    /// arguments before the callee exists: a recursive call is lowered
    /// before its own `Function` does. [`Inst::Call`] therefore carries the
    /// counts, and `validate` is where the two are made to agree.
    pub params: Vec<SlotKind>,
    /// Which stack a call to this function leaves its answer on, and
    /// therefore which stack its return instruction reads.
    ///
    /// [`SlotKind::Scalar`] means the checker settled the declared return
    /// type as `Int` or `Bool`, every return of the body is an
    /// [`Inst::ReturnScalar`], and a caller finds the answer on the scalar
    /// stack. [`SlotKind::Value`] is [`Inst::Return`] and the value stack,
    /// which is what a function the checker settled nothing useful about
    /// keeps.
    pub returns: SlotKind,
    /// Whether slot 0 is a receiver rather than the first parameter.
    pub has_receiver: bool,
    /// Whether a call to this function answers a settled task rather than
    /// the value the body produced.
    ///
    /// True for an `async fn` and for an `async` lambda, and it is the whole
    /// of what `async` means to a run. `Interpreter::call_target` runs the body at
    /// the call site and wraps whatever came out in
    /// `cove_runtime::task::Task::settled`, because ADR 0008 gives a thread
    /// to `spawn` and not to every `async fn`: nothing may depend on when an
    /// `async fn` body ran, only on the value `await` produces.
    ///
    /// It is the *callee's* answer and not the call site's, for the reason
    /// `returns` is: an `async fn` used as a value is called through
    /// [`Inst::CallValue`], which knows nothing about the function it will
    /// reach. So the VM reads this where it opens the frame and wraps where
    /// it closes one, which catches every way a body can end — a `return`,
    /// the final return, and a `?` that failed.
    ///
    /// `returns` is [`SlotKind::Value`] whenever this is true, whatever the
    /// declared return type was, because a task is a value: `async fn f() ->
    /// Int` answers a `Task<Int>` and the caller reads it off the value
    /// stack.
    pub answers_a_task: bool,
    /// What the closure that created this body handed it, in the order the
    /// instructions expect. Empty for a declared function.
    ///
    /// The order is the interpreter's, because it has to be: `Env::captures`
    /// walks the environment that wrote the lambda outermost first and keeps
    /// one entry per name, and `crate::lower` walks the same bindings in the
    /// same order. What differs is *when*. There, a capture's position is a
    /// run-time fact — the list is built as the closure is created — and
    /// here it is decided when the lambda is lowered, which is what lets a
    /// capture be an ordinary slot the body loads by number.
    ///
    /// The names are kept although nothing reads one to find a capture:
    /// `cove_runtime::value::Closure::captures` is a list of `(name, value)`
    /// pairs on both backends, and a host that receives a closure reads it,
    /// so the pairs have to be rebuildable. They are also what a listing
    /// shows.
    ///
    /// The values themselves live in the frame slots this list gives them, put
    /// there by the call that entered this body, out of the closure it was
    /// called through. One entry per capture, in that order.
    ///
    /// A capture is a frame slot like any other, and issue #162 is where that
    /// became true of its *representation* as well as of its numbering. A
    /// capture the checker settled as `Int` or `Bool` takes a scalar slot, so
    /// a body that reads it reads it with an [`Inst::LoadScalar`] and the
    /// arithmetic that consumes it needs no boundary instruction at all.
    ///
    /// It could not before, and the cost was measured:
    /// `benches/convention`'s `conv_capture` row ran five boundary
    /// instructions a turn against `conv_closure`'s two, because the
    /// parameter, the capture *and* the answer all crossed, and cost 1.20x
    /// of it.
    ///
    /// Each capture is allocated a slot right after this function's own
    /// parameters of that same kind — `crate::lower::convention` allocates
    /// them in that order — but that is no longer a fact a reader can derive
    /// from one number a whole region's captures begin at: since a
    /// parameter's slot moved into `0..arity` in declaration order, the first
    /// non-parameter slot of a region is not a fixed offset from the frame's
    /// three sizes alone. So each [`Capture`] carries its own slot, read
    /// directly out of the lowering rather than computed from a base plus a
    /// position in this list.
    ///
    /// **What travels is still a `Value`.** The closure holds
    /// `(name, Value)` pairs whichever backend built it, because a host reads
    /// them and because `crate::lower` numbers one lambda per syntactic site
    /// however many specialisations of the body around it are lowered — so a
    /// capture's representation *at the site* is not a fact the callee may
    /// rely on. What the callee states is where the value lands, and the call
    /// converts it there. The type is the checker's either way, so the
    /// conversion cannot fail.
    ///
    /// Never [`SlotKind::Place`]: a closure captures the value a place names
    /// and never the place. [`Inst::PlaceLocal`] is where that is argued.
    pub captures: Vec<Capture>,
    /// The names source gave this function's parameters, for a function a
    /// closure value can be made of, and empty for every other.
    ///
    /// This is the whole of the parameter *declaration* the lowered form
    /// keeps, and it has one consumer: `cove_runtime::vm::wrong_arity` names
    /// the parameter that went unfilled when a caller supplied too few
    /// arguments, in the interpreter's words, and a backend that no longer
    /// holds the declaration has nowhere else to read that name from. It is
    /// a diagnostic and nothing runs on it.
    ///
    /// Every other question a lowered function was once asked about its
    /// written parameters is answered above, which is what issue #121
    /// established:
    ///
    /// - *how many there are* is `arity`, which `validate` already holds
    ///   equal to `params.len()`, and which a closure value carries as
    ///   `cove_runtime::value::Closure::arity`;
    /// - *which stack each arrives on* is `params`, and that is also where
    ///   `Vm::run_locked` reads whether a `lock` closure asked for its
    ///   parameter by `var` — a [`SlotKind::Place`] in first position is the
    ///   lowered form of the written `var`;
    /// - *whether one is variadic* nothing here ever read: a call to a
    ///   declared function collects the leftovers into one `Array` before
    ///   the call, so the callee sees an ordinary value slot, and
    ///   `crate::lower` refuses a variadic declared function used as a value
    ///   because a call through a value says how many arguments it brought
    ///   and nothing about which of them were leftovers;
    /// - *what a default is* is settled at the call site, out of the
    ///   `supplied` mask a call lowers with, and a parameter with a default
    ///   is refused on both roads to a closure value — so a lowered body
    ///   never evaluates a default and no default expression need survive
    ///   here;
    /// - *the written type* is the checker's answer, already spent: it chose
    ///   each slot's kind and every conversion the body performs;
    /// - *the source span* of a parameter produces no diagnostic. `span`
    ///   below covers the declaration and `spans` covers each instruction,
    ///   which is every position a run can report.
    pub param_names: Vec<Arc<str>>,
    pub code: Vec<Inst>,
    /// How many instructions run from each index control can arrive at before
    /// it can go somewhere else, and 0 at every index it cannot arrive at.
    ///
    /// Where a straight line begins and ends is a fact about the code,
    /// settled when the code was finished, so counting instructions again one
    /// at a time while running them is the re-derivation this whole IR exists
    /// to stop doing. The VM charges fuel and counts instructions a block at
    /// a time, where control *arrives* at a head, which is the same total
    /// over the same path reached with one addition instead of one per
    /// instruction.
    ///
    /// A head is the entry, every jump target, and the index after every
    /// instruction control can leave the straight line at: a jump, a
    /// [`Inst::Call`], a [`Inst::Try`], a return, and a [`Inst::NoMatch`].
    ///
    /// **The counts overlap, and they have to.** They are extents rather than
    /// a partition: `block_fuel[h]` reaches from `h` to the first instruction
    /// at or after it that control can leave from. An `if` with no `else`
    /// falls into the join its own jump also targets, so a head is reached
    /// both by jumping to it and by walking into it, and only the walk has
    /// nothing to announce it. An extent that reaches past the join covers
    /// the walk; a partition would not, and the instructions after such a
    /// join would run uncharged. `lower::block_fuel` says the rest, and
    /// `lower::validate` refuses a table that does not hold it.
    ///
    /// Parallel to `code` for the reason `spans` is: it is read at a block
    /// head and nowhere else, so it has no business inside an [`Inst`].
    pub block_fuel: Vec<u32>,
    /// One span per instruction, so a runtime error points at source.
    ///
    /// Parallel to `code` rather than inside `Inst`, so that an instruction
    /// stays small and a span costs nothing to skip.
    pub spans: Vec<Span>,
    /// The spans of an instruction's arguments, by instruction index, for the
    /// instructions whose diagnostic quotes source.
    ///
    /// An instruction's own span covers the whole call it came from, so a
    /// diagnostic that quotes the source of one *argument* — which is what a
    /// failing `assert` does, and the whole reason `assert` is a builtin
    /// rather than a library function — cannot be written from it. This is
    /// where the argument's own source is, and it is recorded only where such
    /// a diagnostic exists: a span nothing quotes would be a cost with no
    /// reader.
    pub arg_spans: BTreeMap<u32, Vec<Span>>,
    /// Where the function itself was declared, for a diagnostic about the
    /// function rather than about one of its instructions.
    pub span: Span,
}

impl Function {
    /// The span of the instruction at `pc`, or the function's own.
    pub fn span_at(&self, pc: usize) -> Span {
        self.spans.get(pc).copied().unwrap_or(self.span)
    }

    /// The spans of the arguments of the instruction at `pc`, and nothing
    /// where that instruction has no diagnostic that quotes them.
    pub fn arg_spans_at(&self, pc: usize) -> &[Span] {
        self.arg_spans
            .get(&(pc as u32))
            .map_or(&[][..], Vec::as_slice)
    }

    /// How many slots one frame of this function has, over all three
    /// regions: the whole of the one numbering, and the number of eight-byte
    /// words a one-array realization of the frame reserves.
    ///
    /// `slots.len()`, which `crate::lower::validate` holds equal to
    /// `value_frame_size + scalar_frame_size + place_frame_size` — the sum
    /// this answered directly before `slots` exists to ask instead. The two
    /// are one fact stated twice only until validation runs; after that,
    /// reading either answers the other.
    pub fn slot_count(&self) -> u32 {
        self.slots.len() as u32
    }

    /// Where slot `slot` stands within its own region — the number of slots
    /// of the same region that come before it in the one numbering.
    ///
    /// A three-array backend keeps each region in a `Vec` of its own, and a
    /// slot's number in the one numbering is no longer that `Vec`'s index by
    /// construction: `slots[0..arity]` holds every parameter in declaration
    /// order regardless of which region each one's own kind names, so a
    /// region's own slots are no longer one contiguous run beginning at a
    /// fixed origin the way they were before a parameter named its own slot.
    /// [`Function::offsets`] is the table this reads, built once at lowering
    /// by `crate::lower::body::Body::finish`, so this is one indexed load —
    /// what `cove_runtime::vm`'s dispatch loop needs, since it calls this
    /// once per slot-addressing instruction it runs.
    pub fn offset(&self, slot: u32) -> u32 {
        self.offsets[slot as usize]
    }

    /// Which region slot `slot` falls in, and `None` for a number this
    /// function has no slot for.
    ///
    /// This is the frame's reference map, read the way a collector needs it:
    /// [`Region::Value`] is a word to follow and the other two are words to
    /// leave alone. Nothing on a hot path calls it — a backend builds its
    /// map once per function — and `crate::lower::validate` calls it once per
    /// slot-addressing instruction of every lowered program, which is how a
    /// scalar instruction reaching a value slot is caught before a run.
    ///
    /// Reads [`SlotKind::region`] off `slots[slot]`, because `slots` is the
    /// authority for what a slot *is*. It used to be checked against the
    /// three frame sizes as a second, independent question — where a region
    /// *begins* — but a region no longer begins anywhere in particular once a
    /// parameter of another kind can stand ahead of it, so `slots` is the
    /// only authority left; see [`Function::slots`].
    pub fn region_of(&self, slot: u32) -> Option<Region> {
        self.slots.get(slot as usize).map(|kind| kind.region())
    }
}

/// One capture a closure's frame reserves a slot for.
///
/// A name, a kind, and a slot are three facts about one capture, kept
/// together for the reason [`Function::captures`] used to give for keeping
/// just the first two beside each other: a reader that could hold part of
/// one capture without the rest is a reader that could disagree with itself.
/// The slot joined the other two once it stopped being derivable from a
/// region's origin and a position in this list — see [`Function::captures`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// The name source gave the binding this capture reads, kept for the
    /// reason [`Function::captures`] keeps it: nothing here looks a capture
    /// up by name, but `cove_runtime::value::Closure::captures` is a list of
    /// `(name, Value)` pairs a host may read, and a listing shows the name.
    pub name: Arc<str>,
    /// Which stack this capture's value lives in. Never [`SlotKind::Place`]:
    /// a closure captures the value a place names and never the place.
    pub kind: SlotKind,
    /// This capture's slot, in the one frame numbering.
    ///
    /// `crate::lower::convention` reads it straight out of the lowering: a
    /// capture is allocated a region-local number by
    /// `crate::lower::body::Body::allocate`, right after this function's own
    /// parameters of that same kind, and `Body::finish`'s permutation is what
    /// turns that region-local number into this one — the same translation
    /// applied to every slot number an instruction carries. There is no
    /// shorter derivation left: a parameter's slot can stand anywhere in
    /// `0..arity` regardless of its region, so the first non-parameter slot
    /// of a region is not a fixed offset from the three frame sizes alone.
    pub slot: u32,
}

/// What a scalar slot or a scalar operand holds.
///
/// The scalar stack is a `Vec<i64>` and carries no tag, so what a word means
/// is in the instruction that reads it rather than beside the word. Two
/// types fit that description today, and they are the two the checker
/// settles most often.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    /// An `Int`, as itself: Cove's `Int` is a full 64 bits and so is this.
    Int,
    /// A `Bool`, as 0 or 1.
    Bool,
}

/// Where one of a function's frame slots lives.
///
/// Decided at lowering from what the checker settled, and only from that: a
/// slot is scalar where `Facts::ty` answered `Some(Ty::Int)` or
/// `Some(Ty::Bool)` for what the binding was declared from. An abstention —
/// `None`, or `Some(Ty::Unknown(_))` — is not a settled type and keeps the
/// representation every slot had before, which is what makes a function the
/// checker said nothing useful about run exactly as it ran yesterday.
///
/// [`SlotKind::Place`] is not settled that way, because it is not a question
/// about a type at all: a parameter written `var` names the caller's storage
/// whatever its type is, and a parameter written without it receives a copy
/// whatever its type is. So the declaration decides that one and the checker
/// decides between the other two.
///
/// [`SlotKind::region`] carries this to a slot *number*'s region of the one
/// frame numbering, dropping the one part a number cannot answer — see
/// [`Region`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// A `Value` in the value region.
    Value,
    /// An `i64` in the scalar region, meaning what [`Scalar`] says.
    Scalar(Scalar),
    /// A place in the place region: one slot number of the value or the
    /// scalar region, and the field positions to walk from what stands
    /// there.
    ///
    /// What a `var` parameter's slot holds. Reading such a parameter is a
    /// [`Inst::LoadPlace`] and then a [`Inst::PlaceRead`], and writing to it
    /// is a [`Inst::PlaceWrite`]; the slot itself is never the value.
    Place,
}

impl SlotKind {
    /// Whether this slot lives in the scalar stack.
    pub fn is_scalar(self) -> bool {
        matches!(self, SlotKind::Scalar(_))
    }

    /// Whether this slot lives in the place stack.
    pub fn is_place(self) -> bool {
        matches!(self, SlotKind::Place)
    }

    /// Which region of the one frame numbering a slot of this kind falls in.
    pub fn region(self) -> Region {
        match self {
            SlotKind::Value => Region::Value,
            SlotKind::Scalar(_) => Region::Scalar,
            SlotKind::Place => Region::Place,
        }
    }
}

/// Which run of the one frame numbering a slot number falls in, and so which
/// storage a backend keeps it in.
///
/// This is a [`SlotKind`] with the part a *number* cannot answer taken off.
/// A slot number says a slot is scalar; it does not say whether the word is
/// an `Int` or a `Bool`, because nothing addressed by a number needs to know
/// — [`Inst::LoadScalar`] pushes the word as itself and
/// [`Inst::PlaceScalar`] carries the [`Scalar`] where a read has to put a tag
/// back. So the layout answers this and the instruction answers the rest,
/// which is
/// [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
/// decision 1's "what a slot means comes from `cove_ir::Function`'s per-slot
/// layout and from the instruction that touches it".
///
/// It is also exactly the reference map a collector reads:
/// [`Region::Value`] is a word to follow and the other two are words to leave
/// alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// A slot holding a general value, and the only region a collector walks.
    Value,
    /// A slot holding the eight untagged bytes of an `Int` or a `Bool`.
    Scalar,
    /// A slot holding a place: the number of another slot, and a path.
    Place,
}

/// Addresses a [`Function`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// Addresses a [`Const`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstId(pub u32);

/// Addresses a [`StructType`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StructId(pub u32);

/// Addresses an [`EnumType`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumId(pub u32);

/// One declared struct type, as the lowering settled it: its qualified name,
/// and one field name and [`SlotKind`] per field, in declaration order.
///
/// # Why the type and not the construction
///
/// A struct's *shape* is a static fact about a declaration, and until this
/// existed nothing in the IR said so. [`Inst::MakeStruct`] named the type and
/// the field names and stopped there, so a backend that needs
/// [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
/// decision 2's reference map — "which of its words are handles, so a
/// collector scans those and not the scalars beside them" — had to read it
/// off the instructions that pushed one instance's fields, and had to refuse
/// a type whose two construction sites disagreed. Two sites for one type
/// cannot disagree about a fact neither of them states.
///
/// The kinds come from where a parameter's, a local's and a return's come
/// from: the checker's answer about the type, through
/// `lower::convention::slot_kind_of`. That is
/// [ADR 0027](../../../docs/adr/0027-a-place-and-a-capture-name-a-slot.md)'s
/// rule — nothing about a slot's *role* decides which stack it lives in, only
/// the checker's answer about its type does — asked about a field.
///
/// # What a kind means for a field
///
/// [`SlotKind::Scalar`] is a word a collector must not follow and
/// [`SlotKind::Value`] is one it must. [`SlotKind::Place`] never appears: a
/// place is what a *parameter* can be, and a field is storage rather than an
/// alias of it.
///
/// A generic field records the declaration's own `Ty::Param`, which is not a
/// type the scalar stack holds, so it is a [`SlotKind::Value`] on every
/// instantiation. One name, one layout — which is what makes a [`StructId`] a
/// complete answer rather than a key into a per-instantiation table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructType {
    /// The qualified name a value of this type carries: `module.Name`.
    pub name: Arc<str>,
    /// The fields, in declaration order — the order [`Inst::MakeStruct`]
    /// pushes them and the order [`Inst::GetFieldAt`] indexes.
    pub fields: Vec<StructField>,
}

impl StructType {
    /// The field names in declaration order, which is what a runtime building
    /// a named-field value pairs its words with.
    pub fn field_names(&self) -> Vec<&str> {
        self.fields.iter().map(|field| &*field.name).collect()
    }
}

/// One field of a [`StructType`]: its name, and where a slot holding it
/// lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructField {
    /// The name the declaration wrote.
    pub name: Arc<str>,
    /// What the checker settled the declared field type as, read through the
    /// one rule that decides every other slot's kind.
    pub kind: SlotKind,
}

/// One declared enum type, as the cases [`Inst::MakeEnum`] built or
/// [`Inst::GetPayload`] read reached: a name and, per case, its payload's
/// slot kinds. Addressed by [`EnumId`].
///
/// # Where this comes from
///
/// `cove_sema` records one `Signature` per case — "one case of an
/// enum, whose `params` is its payload types" — the same way it records one
/// for a struct's synthesized initializer, keyed by the case's own span
/// rather than the enum declaration's, because a case is what a program names
/// and a value carries. `crate::lower::index::Lowering::enum_type` reads
/// that signature through `crate::lower::convention::slot_kind_of`, the one
/// rule that settles a parameter's slot, a local's, and a struct field's
/// alike, so a case's payload kind is that rule's answer about the payload's
/// declared type and nothing else.
///
/// **The case is part of the type here, not a separate key.** A two-case enum
/// is two entries of [`EnumType::cases`], in declaration order, which is the
/// order [`Inst::MakeEnum`]'s `argc` payload words arrive in, the order
/// [`Inst::GetPayload`]'s `case` indexes, and the order [`Inst::GetPayload`]'s
/// `at` indexes within whichever entry `case` names.
///
/// A generic case's payload records the declaration's own `Ty::Param`, which
/// `slot_kind_of` always settles as [`SlotKind::Value`] — one name, one
/// layout, exactly as [`StructType`]'s own doc comment argues for a generic
/// field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumType {
    /// The qualified name a value of this type carries: `module.Name`.
    pub name: Arc<str>,
    /// The cases, in declaration order.
    pub cases: Vec<EnumCase>,
}

/// One case of an [`EnumType`]: its name, and its payload's slot kinds in
/// declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumCase {
    /// The name the declaration wrote, unqualified — `Confirmed`, not
    /// `booking.Status.Confirmed`.
    pub name: Arc<str>,
    /// What the checker settled each payload position's declared type as, in
    /// declaration order. Empty for a case with no payload.
    pub payload: Vec<SlotKind>,
}

/// Addresses a [`Dispatch`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DispatchId(pub u32);

/// What one call through a `dyn Trait` receiver can reach.
///
/// A call on a trait object is the one call in the language whose target is
/// not knowable before the run: the static type says which trait the method
/// comes from, and the *value* says which implementation. So the lowering
/// cannot write a [`FunctionId`] into the instruction, and writes the whole
/// answer instead — every implementation the method has, by the name of the
/// concrete type that carries it — for [`Inst::CallDyn`] to choose from with
/// the name the receiver turns out to hold.
///
/// # Why every implementation in the package, and not the visible ones
///
/// A trait object is how a value crosses a `use` edge that does not exist.
/// `tests/e2e/outline_dyn_field` is the shape: `lib` declares the trait and
/// holds a `dyn Summary` in a field, `plugin` supplies the conformance, and
/// `lib` never imports `plugin` — so the type standing in `lib`'s field is
/// one `lib` cannot name. That is what ADR 0015 calls capability-open, and it
/// is why bounding the candidates by what the calling module can reach — the
/// bound `crate::lower`'s `could_dispatch` puts on a call resolved by *name*
/// — would leave out exactly the case dynamic dispatch exists for.
#[derive(Debug)]
pub struct Dispatch {
    /// The qualified trait the receiver was used at, for the listing and for
    /// the diagnostic.
    pub trait_name: Arc<str>,
    /// The method's own name, which is what a value carrying no
    /// implementation of it is reported against.
    pub method: Arc<str>,
    /// One entry per type that conforms, keyed by the qualified name that
    /// type's values carry — `cove_runtime::value::Value::declared_type_name`
    /// — and holding the specialisation of its method that takes every
    /// argument on the value stack.
    ///
    /// A `Vec` scanned by name rather than a map: a trait has as many
    /// implementations as a package wrote `impl` blocks for it, which is a
    /// handful, and a scan of a handful of `Arc<str>` is cheaper than hashing
    /// one.
    pub cases: Vec<(Arc<str>, FunctionId)>,
}

/// A value an instruction can push without computing it.
///
/// Only what a literal can be, plus the names a structured operation needs to
/// carry. A name here is a constant because it is written once at lowering and
/// read by an instruction that already knows what to do with it — not because
/// anything looks it up.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(i64),
    Str(Arc<str>),
    /// A name carried by an instruction: a field, a host module, a host
    /// operation, a builtin method, or a declared type.
    Name(Arc<str>),
}

/// One VM instruction.
///
/// The set is deliberately small and deliberately not general: every variant
/// exists because some Cove construct lowers to it, and none exists in case
/// something might.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inst {
    /// Pushes a constant.
    Const(ConstId),
    /// Pushes the value in a frame slot.
    ///
    /// The slot is a number in the one frame numbering's value region;
    /// `validate` is where that is checked, through
    /// [`Function::region_of`], so nothing here asks.
    LoadLocal(u32),
    /// Pops a value into a frame slot, addressed the same way as
    /// [`Inst::LoadLocal`].
    StoreLocal(u32),
    /// Pushes one of this call's captures.
    ///
    /// Builds a closure over the top `captures` values, in the order
    /// [`Function::captures`] names them, and pushes it.
    ///
    /// This is `Interpreter::make_closure` with the hard part already done.
    /// There, the set of captures is worked out as the closure is created —
    /// the names the body mentions, intersected with what is live — and the
    /// list a closure ends up holding is therefore a run-time fact. Here the
    /// same set is worked out when the lambda is lowered, the enclosing body
    /// pushes exactly those values, and this only has to pair them with the
    /// names beside them.
    ///
    /// The count is in the instruction as well as in the callee's own
    /// `captures`, for the reason [`Inst::Call`]'s counts are in the
    /// instruction: `crate::lower::stack_shape` is a pure function of one
    /// instruction with no function table beside it, and a lambda is
    /// numbered before its own [`Function`] exists. `validate` reconciles
    /// the two.
    MakeClosure { function: FunctionId, captures: u16 },
    /// Calls the callable value on top of the stack, with `argc` arguments
    /// standing below it.
    ///
    /// The callee is *above* its arguments, which is the one place in this
    /// IR where an operand is not in source order. A call through a value is
    /// lowered only where the callee is a bare name that resolves to a local
    /// — `crate::lower` refuses every other shape — and reading a local has
    /// no effect, so the order the two are evaluated in is not observable.
    /// What it buys is that the callee is popped rather than left standing
    /// under the frame the call opens: the arguments become the callee's
    /// first slots without moving, exactly as [`Inst::Call`]'s do, and the
    /// captures are copied in after them.
    ///
    /// Every argument travels on the value stack and the answer comes back
    /// on it, whatever the callee's types are, because nothing at the call
    /// site knows which function this will reach. `crate::lower` numbers a
    /// separate specialisation of a declared function used as a value for
    /// exactly that reason, so that the convention a closure is called under
    /// is one convention rather than whatever the target happened to
    /// declare.
    CallValue { argc: u16 },
    /// Discards the top of the stack.
    Pop,
    /// Applies a unary operator to the top of the stack.
    Unary(UnaryOp),
    /// Applies a binary operator to the top two values, left below right.
    ///
    /// This is the operator that does not know what it is applied to. It goes
    /// through the interpreter's own `binary`, which takes two 40-byte
    /// `Value`s, answers a 120-byte `Result`, and matches three times to find
    /// out that it was adding two integers. Where the checker settled that it
    /// *is* two integers, [`Inst::IntBinary`] is emitted instead.
    Binary(BinaryOp),
    /// Applies a binary operator to the two scalars on top of the *scalar*
    /// stack, left below right, and leaves its answer there.
    ///
    /// Only emitted where the checker recorded both operands as `Int`, so the
    /// operands need no examining — the type is in the instruction. Overflow,
    /// division by zero, and remainder by zero raise what the untyped operator
    /// raises, because a broken invariant is the same broken invariant however
    /// it was reached.
    ///
    /// It reads the scalar stack rather than the value stack because that is
    /// the whole of what this instruction was ever for: two `i64` in, one
    /// `i64` out, with no 40-byte `Value` moved and no drop glue run to add
    /// two integers. A comparison leaves a `Bool` as 0 or 1, which
    /// [`Inst::JumpIfFalseScalar`] reads without building one.
    IntBinary(IntOp),
    /// Pushes an `Int`, or a `Bool` as 0 or 1, onto the scalar stack.
    ///
    /// The number is in the instruction rather than in the constant pool: a
    /// scalar constant is one word, and an id would be an indirection to
    /// fetch what it already fits.
    ScalarConst(i64),
    /// Pushes the scalar in a frame slot onto the scalar stack.
    ///
    /// The slot is a [`SlotKind::Scalar`] one, checked by `validate` through
    /// [`Function::region_of`] rather than asked about here. Its number in
    /// the one frame numbering and its offset within the scalar region are
    /// two different numbers now that a parameter of another kind can stand
    /// ahead of a body's own scalars — [`Function::offset`] answers the
    /// second.
    LoadScalar(u32),
    /// Pops the scalar stack into a frame slot, addressed the same way as
    /// [`Inst::LoadScalar`].
    StoreScalar(u32),
    /// Discards the top of the scalar stack.
    ///
    /// What a `break` written inside a half-evaluated scalar expression needs,
    /// exactly as [`Inst::Pop`] is what one written inside a half-evaluated
    /// value expression needs.
    ScalarPop,
    /// Pops a scalar `Bool` and jumps when it is zero.
    JumpIfFalseScalar(u32),
    /// Pops a scalar `Bool` and jumps when it is non-zero.
    ///
    /// A scalar `Bool` is 0 or 1, so there is nothing to examine beyond that
    /// bit; the lowering emitted it only where the checker settled one. The
    /// companion of [`Inst::JumpIfFalseScalar`], for the operand of a `||`
    /// that short-circuits on truth rather than falsity.
    JumpIfTrueScalar(u32),
    /// Pops the scalar stack and pushes what it holds onto the value stack.
    ///
    /// The boundary in the outward direction: a scalar has no tag, so the
    /// instruction carries the one it is to be given. Emitted where a value
    /// is wanted from something the scalar stack computed — an argument to a
    /// call, a returned value, a field written back.
    ScalarToValue(Scalar),
    /// Pops the value stack and pushes what it holds onto the scalar stack.
    ///
    /// The boundary in the inward direction, and half of "a value into a
    /// scalar slot": this and [`Inst::StoreScalar`] are what a scalar
    /// binding declared from something the value path computed lowers to.
    /// Emitted only where the checker settled the value as `Int` or `Bool`.
    ValueToScalar,
    /// Pushes a field of the struct on top of the stack, by position.
    ///
    /// Emitted where the checker settled the receiver's type, which is what
    /// makes the position knowable. [`Inst::GetField`] is what a receiver
    /// whose type the checker abstained about still gets.
    ///
    /// `of` is that settled type. The position alone was enough to *find* the
    /// word; naming the type as well is what says what the word **is** —
    /// `struct_type(of).fields[at].kind` — so a backend whose stack carries no
    /// tag can decide that from the instruction rather than from the object it
    /// just read. There is no other reading available at run time: the type is
    /// the receiver's settled type, so it is the same type on every execution
    /// of this instruction.
    GetFieldAt { of: StructId, at: u32 },
    /// Pops a struct off the value stack and pushes the field at this
    /// position onto the scalar stack.
    ///
    /// Emitted where the checker settled both the receiver's type — which is
    /// what makes the position knowable, exactly as for [`Inst::GetFieldAt`]
    /// — and the field's own type, as `Int` or `Bool`. It is a fusion of two
    /// instructions rather than a new capability: without it, the same read
    /// is [`Inst::GetFieldAt`] followed by [`Inst::ValueToScalar`], which
    /// builds a `Value` for the sole purpose of the next instruction
    /// discarding it.
    GetFieldAtScalar(u32),
    /// Jumps to an instruction index.
    Jump(u32),
    /// Pops a `Bool` and jumps when it is false.
    JumpIfFalse(u32),
    /// Pops a `Bool` and jumps when it is true.
    JumpIfTrue(u32),
    /// Duplicates the top of the stack.
    Dup,
    /// Calls a lowered function whose arguments are already on the three
    /// stacks: `value_argc` of them on the value stack, `scalar_argc` on the
    /// scalar stack, and `place_argc` on the place stack, each in the
    /// callee's parameter order within its own stack. `returns_scalar` says
    /// which stack the answer comes back on; no call answers a place,
    /// because a place is not a value a function can return.
    ///
    /// The four numbers are in the instruction rather than looked up in the
    /// callee, and both reasons are structural. `crate::lower::stack_shape`
    /// is a pure function of one instruction and has no function table to
    /// ask; and a recursive call is emitted before its own callee's
    /// [`Function`] exists, so there would be nothing to ask at the moment
    /// the instruction is written. `validate` reconciles them with the
    /// callee's own `params` and `returns`, which is what makes the
    /// convention an invariant rather than a convention.
    ///
    /// The counts are `u16` because this is the widest variant and an enum
    /// is as wide as its widest one. Three `u32` counts beside a
    /// [`FunctionId`] made `Inst` 24 bytes where 16 does, which is 50% more
    /// of every function's code array for a number that cannot use the
    /// width: an argument count is bounded by the parameters a declaration
    /// writes, and `crate::lower` refuses a declaration with more of them
    /// than this holds rather than truncating one. `benches/arith` reads its
    /// loop out of that array two million times, and it noticed the
    /// difference.
    Call {
        function: FunctionId,
        value_argc: u16,
        scalar_argc: u16,
        place_argc: u16,
        returns_scalar: bool,
    },
    /// Calls a Host operation. `module` and `op` are `Const::Name`.
    CallHost {
        module: ConstId,
        op: ConstId,
        argc: u32,
    },
    /// Calls an operation on a resource handle standing below `argc`
    /// arguments. `op` is a `Const::Name`.
    ///
    /// Separate from [`Inst::CallHost`] because of what routes the call.
    /// There, the instruction names the module the operation is addressed
    /// to, and the arguments are the whole of the stack it reads. Here the
    /// receiver is on the stack and it *is* the address: a `Value::Resource`
    /// is a handle the host issued, and the handle says which module issued
    /// it and which of that module's resources it names, so
    /// `HostRegistry::call_resource` reads both off it — including whether
    /// the resource it named is still there, which nothing the compiler
    /// knows could answer.
    ///
    /// That is also why the qualified type name is not carried beside `op`.
    /// The lowering reads it, out of what the checker settled for the
    /// receiver, to decide that this instruction is the right one: a
    /// `Ty::Host` whose module declares that name as a resource — a
    /// `cove_schema::ResourceSchema`, which the host keeps, rather than a
    /// `TypeSchema`, which it hands over — declaring an operation of this
    /// name. Having decided, there is nothing left for the instruction to
    /// say about the receiver that the handle does not say for itself, and a
    /// second answer to "which resource is this" would only be a second
    /// thing that could be wrong.
    CallResource { op: ConstId, argc: u32 },
    /// Calls a builtin method on a receiver below `argc` arguments. `name` is
    /// a `Const::Name`.
    CallBuiltin { name: ConstId, argc: u32 },
    /// Builds an `Array` from the top `len` values.
    MakeArray(u32),
    /// Builds a `Range` from the top two *scalars*: the start below the end.
    ///
    /// The bounds travel on the scalar stack because the checker settles
    /// both of them as `Int` — `a range runs between two `Int`s` is the
    /// expectation `cove_sema` checks each against — so they are exactly
    /// what that stack is for, and the lowering emits this only where it
    /// settled them. Every other typed operand is placed the same way, and
    /// the alternative would be two `Value`s built for this instruction to
    /// unwrap again.
    ///
    /// `inclusive_end` is in the instruction rather than beside the bounds
    /// because it is a property of the syntax — `..` against `..<` — and not
    /// a value anything computes. It is observable and is reproduced rather
    /// than normalised away: `Value::eq_value` compares it, `Display` writes
    /// the operator back out, `MapKey::Range` orders by it, and it crosses a
    /// task boundary in `Transfer::Range`.
    MakeRange { inclusive_end: bool },
    /// Renders the top `parts` values as one string, left to right.
    ///
    /// This is what `"a{b}c"` lowers to: interpolation is not concatenation
    /// in the language — `+` on two strings is refused — but it is one
    /// operation over a known number of parts here, which is what an
    /// instruction is for.
    Concat(u32),
    /// Appends what a `...` argument spreads to the `Array` below it.
    ///
    /// Pops a value and the `Array` under it, and pushes that array extended
    /// with the value's elements: an `Array`'s own, a `Vector`'s as it holds
    /// them now, and nothing else — `bind_params` reads exactly those two
    /// and reports anything else, and so does this.
    ///
    /// A variadic parameter receives one `Array`, and `Inst::MakeArray`
    /// builds it out of the leftover arguments. A spread is the same array
    /// built out of a value instead of out of a list of expressions, so a
    /// call that mixes the two builds it in runs: `MakeArray` for each run
    /// of ordinary arguments, the value itself for each spread, and this
    /// instruction to join what came before to what comes next. That is why
    /// it takes an array and a value rather than two arrays — the ordinary
    /// run is wrapped on its way in, and the spread is not wrapped at all.
    SpreadArgument,
    /// Builds a declared struct: the [`StructType`] this names, out of the
    /// words standing on the value stack, one per field in declaration order.
    ///
    /// The type carries the qualified name, the field names **and** each
    /// field's [`SlotKind`], so the instruction carries one id where it used
    /// to carry two names. That is the difference between a construction
    /// describing a type and a type describing itself: two `make-struct` sites
    /// for one declaration name the same [`StructId`], and a reader that wants
    /// to know which of the object's words are references asks the type.
    MakeStruct(StructId),
    /// Pushes a field of the struct on top of the stack. `name` is a
    /// `Const::Name`.
    GetField(ConstId),
    /// Replaces a field: pops a value and the struct below it, and pushes the
    /// struct with that field changed. `name` is a `Const::Name`.
    ///
    /// Writing a field is a whole-value update here rather than a mutation
    /// through a place, which is what `crate::lower` can do while a `var`
    /// parameter is not in the subset it lowers. A struct is a value, and the
    /// only holder of a local's struct is that local, so replacing the local
    /// with an updated struct is what assigning to its field means. That stops
    /// being true the moment two names can reach one struct — which is exactly
    /// what a `var` parameter is — so whoever lowers `var` has to give the VM
    /// a real place model first, and this instruction is not it.
    SetField(ConstId),
    /// Builds one of the builtin constructors: `Ok`, `Err`, `Some`, or
    /// `Error`, over one value, or `None` over none; and runs the two
    /// assertions, `assert` and `assertEqual`.
    ///
    /// A failing assertion quotes the source text of its condition, so the
    /// spans of an assertion's arguments are recorded in
    /// [`Function::arg_spans`] beside it.
    MakeBuiltin { name: ConstId, argc: u32 },
    /// Builds a case of an enum a *host* declares, such as `http.Method.Get`.
    ///
    /// Separate from [`Inst::MakeEnum`] because a host's enum has a
    /// `cove_schema::TypeSchema` rather than an `EnumDecl`, and the VM shapes
    /// a `MakeEnum` from a declaration this package holds. There is nothing
    /// to push either: a host's case carries no payload, so the qualified
    /// `module.Name` and the case's own name are the whole of it.
    MakeHostEnum { ty: ConstId, case: ConstId },
    /// Builds a case of a declared enum. `ty` is a `Const::Name` holding the
    /// qualified type name and `case` the case's own name.
    MakeEnum {
        ty: ConstId,
        case: ConstId,
        argc: u32,
    },
    /// Calls an associated function of a builtin type, such as `Vector.of` or
    /// `Int.parse`. `ty` and `name` are `Const::Name`.
    ///
    /// Separate from [`Inst::CallBuiltin`] because there is no receiver: the
    /// type is named rather than stood on, so the arguments are the whole of
    /// the stack this reads.
    CallBuiltinAssoc {
        ty: ConstId,
        name: ConstId,
        argc: u32,
    },
    /// Pushes whether the value on top is an enum of case `case`, without
    /// consuming it. `case` is a `Const::Name`.
    ///
    /// A pattern tests the subject and then binds out of it, and both need the
    /// subject still there — which is why this peeks. `Pop` is what a lowering
    /// writes when an arm is done with it.
    TestCase(ConstId),
    /// Pushes one payload of the enum on top, without consuming it.
    ///
    /// Emitted where the checker settled the pattern's subject as a
    /// declared enum this package owns, which is what makes the case
    /// knowable — the same settlement [`Inst::GetFieldAt`]'s `of` reads for
    /// a receiver, asked of a `match` arm's subject instead. `of` is that
    /// case: the [`EnumId`] the subject's type interned as and its position
    /// within [`EnumType::cases`]. `at` is the position within *that case's*
    /// payload, the order [`Inst::MakeEnum`]'s `argc` words arrive in.
    ///
    /// Naming the case is what says what the word **is** —
    /// `program.enum_type(of.0).cases[of.1].payload[at]` — the same reason
    /// [`Inst::GetFieldAt`]'s doc gives for naming a struct: a backend whose
    /// stack carries no tag can decide a payload position is a reference or
    /// a scalar from the instruction, rather than refusing every read
    /// because *some* position of *some* case could be either.
    ///
    /// `of` is `None` where the subject is not one of this package's own
    /// declared enums — `Result` and `Option`, whose case names
    /// `cove_schema::builtins` gives rather than a declaration, carry a
    /// payload of whatever type the program wrote at each site: `Ok(T)`
    /// records no `T` a single table entry could settle a [`SlotKind`] for,
    /// the way a generic field or case payload always resolves to
    /// [`SlotKind::Value`] and would be no answer at all for the `Ok(1)` a
    /// backend already knows is scalar at its own construction site. There
    /// is nothing this instruction can name there, so it names nothing,
    /// exactly as an abstained field read stays [`Inst::GetField`] rather
    /// than [`Inst::GetFieldAt`].
    GetPayload { of: Option<(EnumId, u32)>, at: u32 },
    /// Pops a value and pushes an `Array` of what `for` walks over it: the
    /// elements of a sequence, the `MapEntry` of each pair of a `Map`, a
    /// `Set`'s elements in ascending order.
    ///
    /// A `for` could walk a sequence by index, and did, and that was wrong for
    /// the two collections whose iteration is not indexing. So the question is
    /// asked once, by the same function the interpreter asks, and the loop
    /// walks what comes back.
    IterItems,
    /// Stops the run because no `match` arm covered the value on top.
    ///
    /// Exhaustiveness is the checker's to prove and it does not prove it yet,
    /// so a `match` that covers nothing has to fail rather than answer. It
    /// carries no name: what the message needs is the value it could not
    /// match, and that is on the stack.
    NoMatch,
    /// `expr?`: pops a `Result` or `Option`, pushes its payload, or returns
    /// the failure from this call.
    Try,
    /// Pushes the place that names one of this frame's *value* slots, with
    /// no path: the whole of what stands in that slot.
    ///
    /// This is where a `var` argument rooted at a local comes from, and it
    /// is where the place model's one soundness obligation lives. A place is
    /// an absolute slot number rather than a borrow of the stack it is in,
    /// which is what lets the `Vec` reallocate under a place that is
    /// standing, and it is valid for exactly as long as the frame it
    /// addresses is live.
    ///
    /// **That is sound because nothing a lowered program can build holds a
    /// place past the return of the frame that made it.** A place travels
    /// down into a call and back out as nothing at all: no call answers one,
    /// and no value contains one.
    ///
    /// A closure is the construct that could have broken that, because it
    /// can be returned, and this note used to say that nothing lowered one.
    /// One does now, and the answer is that **a closure captures the value a
    /// place names, never the place**. `Body::lambda` reads a captured `var`
    /// parameter with a [`Inst::LoadPlace`] and a [`Inst::PlaceRead`], which
    /// is the same read `Env::captures` makes with `place.read` in the
    /// interpreter — and the oracle agrees that it is a read: a closure over
    /// a `var` binding still answers what the binding held when the closure
    /// was written, after the binding has been assigned to. So
    /// [`Inst::MakeClosure`] takes values off the value stack and a
    /// `Value::Closure` holds `Value`s, and the place stack is still
    /// something only a call's arguments travel on.
    ///
    /// The slot is a [`SlotKind::Value`] one — a number in the one frame
    /// numbering's value region — checked by `validate` rather than asked
    /// about here. [`Inst::PlaceScalar`] is the same instruction for a slot
    /// of the scalar region, and the pair is what lets a place name storage
    /// in either representation.
    PlaceLocal(u32),
    /// Pushes the place that names one of this frame's *scalar* slots.
    ///
    /// The counterpart of [`Inst::PlaceLocal`] and the whole of what issue
    /// #162 calls one slot identity for a place: a place names a slot, and a
    /// slot is a region and a number in it, so a binding the checker settled
    /// as `Int` or `Bool` can be the root of a `var` argument without leaving
    /// the representation its arithmetic wants.
    ///
    /// It used not to be able to, and the cost of that was measured. A
    /// binding a `var` argument was rooted at was kept on the value stack for
    /// the whole of the body that declared it, so every read and every write
    /// of it crossed — `benches/convention`'s `conv_var` row against its
    /// `conv_local`, 1.31x for one `root(var v)` written *outside* the loop.
    ///
    /// The path is always empty. A scalar slot holds an `Int` or a `Bool` and
    /// neither has a field, so nothing can refine this place; `validate`
    /// says so by refusing an [`Inst::PlaceField`] that could reach one, and
    /// `crate::lower` never emits one because a field step needs a struct
    /// type under it.
    ///
    /// The [`Scalar`] travels with the instruction for the reason
    /// [`Inst::ScalarToValue`] carries one: the scalar stack keeps no tag, so
    /// a read through this place has to be told which of the two words it is
    /// putting a tag back on.
    PlaceScalar(u32, Scalar),
    /// Pushes a copy of the place in one of this frame's *place* slots.
    ///
    /// A `var` parameter's slot holds a place, so this is how the parameter
    /// is reached at all: passing it on as a `var` argument is this alone,
    /// which is what makes the callee's callee alias the original binding
    /// rather than the parameter's own slot; reading it is this and a
    /// [`Inst::PlaceRead`]; writing to it is this and a
    /// [`Inst::PlaceWrite`]. The slot is a number of the place region,
    /// checked by `validate` through [`Function::region_of`].
    LoadPlace(u32),
    /// Refines the place on top of the place stack by one field, named by
    /// its position.
    ///
    /// The position is knowable for the reason [`Inst::GetFieldAt`]'s is:
    /// the lowering emitted this only where the checker settled the type the
    /// step is taken from, and a struct's fields stand in declaration order
    /// wherever one is built. A step whose position the checker did not
    /// settle has no lowering at all and is refused, rather than falling
    /// back to a name the way a *read* can — a read by name is a scan of a
    /// list that is there, and a place is a path that has to be walked twice,
    /// once to read and once to write.
    PlaceField(u32),
    /// Discards the top of the place stack.
    ///
    /// What a `break` written inside a half-built call's arguments needs —
    /// `f(var x, if c { break } else { 1 })` leaves a place standing that
    /// the loop's exit is not reached with — exactly as [`Inst::Pop`] and
    /// [`Inst::ScalarPop`] are what one written inside a half-evaluated
    /// value or scalar expression needs.
    PlacePop,
    /// Pops a place and pushes what it names onto the value stack.
    ///
    /// Reading a place clones, which is the value-semantics rule and is what
    /// `Place::read` does in `crates/cove-runtime/src/interp.rs`.
    PlaceRead,
    /// Pops a value off the value stack and a place off the place stack, and
    /// writes the value where the place names.
    ///
    /// The walk down the path makes each struct it descends through private
    /// again, which is the whole of why a copied struct's fields can be
    /// written without the copy being observable. `Place::with_mut` in the
    /// interpreter is the same walk with the same call at the same steps,
    /// and it has to stay the same walk: `is`, aliasing, and struct value
    /// semantics are all decided by where that happens.
    PlaceWrite,
    /// Pops a place naming a `Vector`, consumes its storage, and pushes the
    /// `Array` that storage becomes.
    ///
    /// The one builtin that needs the place rather than a read of it.
    /// `freeze` is O(1) because it takes the elements out of storage nobody
    /// else observes, so `crate::builtins::freeze` counts the handles and
    /// refuses when the count is not one — and a read of the receiver would
    /// be the second handle, produced by the very instruction that was
    /// arranging for the count to be taken. `push`, the other `var self`
    /// builtin, needs no such thing: it mutates *through* a handle, so a
    /// copy of the handle does as well as the original.
    Freeze,
    /// Replaces the top of the value stack with the independent copy
    /// `Snapshot` makes of it.
    ///
    /// `x.snapshot()` where no declared conformance answers for `x`. A
    /// struct or an enum that wrote `impl Snapshot for Type` is an ordinary
    /// method call and lowers to [`Inst::Call`] like any other, because the
    /// checker recorded which declaration it reaches; this is the rest of
    /// the trait — a `Vector`, which allocates storage of its own, and every
    /// value with nothing mutable inside it, which returns itself.
    ///
    /// The lowering emits it only where the checker settled a receiver type
    /// that cannot reach a conformance, `Vector<T>` included: an instruction
    /// cannot run a whole Cove function in the middle of itself, so a
    /// `Vector<SomeStruct>` — whose elements each dispatch — is refused
    /// before the run rather than failed during it.
    Snapshot,
    /// Converts the value on top of the stack to the `dyn Trait` a written
    /// type asks for. `trait_name` is a `Const::Name` holding the qualified
    /// trait, and `depth` is how many `Array` or `Option` layers stand
    /// between the value and it.
    ///
    /// This is the language's one implicit conversion, and it is
    /// `Interpreter::coerce` with the type already read. There, a written
    /// type is walked at run time to find the `dyn` inside it; here the walk
    /// happened when the type was lowered, and what is left is the part that
    /// depends on the value — whether it is already a trait object, and how
    /// many elements an `Array` turned out to have.
    ///
    /// `depth` is one number rather than a list of `Array`/`Option` steps
    /// because the interpreter's walk does not branch on which of the two it
    /// is either: `cove_runtime::interp::coerce_inside` reaches into an
    /// `Array`'s elements and an `Option`'s payload and leaves everything
    /// else alone, so a layer is a layer and the value says which kind it
    /// was. The lowering emits a
    /// depth only for those two heads, which is what keeps a `Map<K, dyn V>`
    /// — whose elements the interpreter does *not* convert — from being
    /// reached by counting.
    MakeDyn { trait_name: ConstId, depth: u16 },
    /// Calls a trait method on the `dyn Trait` receiver standing below
    /// `argc - 1` arguments, choosing the implementation from the concrete
    /// value the receiver carries. `site` names the [`Dispatch`] holding the
    /// implementations.
    ///
    /// The dispatch is dynamic in the strict sense: nothing before the run
    /// says which function this reaches, because the static type names only
    /// the trait. So this is the second call whose target is a run-time fact
    /// — [`Inst::CallValue`] is the first — and it takes the same convention
    /// for the same reason: every argument travels on the value stack and the
    /// answer comes back on it, since no candidate's own signature can be the
    /// one the call site placed its arguments by. `crate::lower` numbers each
    /// candidate as a specialisation under that convention, exactly as it
    /// numbers a declared function used as a value.
    ///
    /// The receiver is the first argument rather than a fourth operand: it is
    /// `self`, it becomes the callee's slot 0, and unwrapping it is what this
    /// instruction does to it in place. A receiver that is not a trait object
    /// at all is dispatched from itself, which is what
    /// `Interpreter::eval_method_call` does with one — the checker converts
    /// where a type is written and does not convert a lambda's inferred
    /// result, and neither backend may tell the two apart.
    CallDyn { site: DispatchId, argc: u16 },
    /// Opens the task scope `scope name { ... }` binds, and pushes it.
    ///
    /// `name` is a `Const::Name` holding the name the scope is bound to,
    /// which is what its diagnostics call it. The value pushed is a
    /// `cove_runtime::value::Value::TaskScope`, and the instruction after
    /// this is the [`Inst::StoreLocal`] that binds it — the scope is an
    /// ordinary value slot, because `scope.spawn` reads it the way any other
    /// receiver is read.
    ///
    /// The VM also *registers* the scope against the frame that opened it,
    /// which is the half a slot cannot hold. Leaving a scope waits for or
    /// cancels its children, and a frame that returns out of the middle of
    /// one — through `return`, or through a `?` that failed — never reaches
    /// the [`Inst::LeaveScope`] written after its body. So the VM cancels
    /// whatever the popped frame had opened, and the lowering emits an
    /// [`Inst::CancelScope`] only for the exits that stay *inside* the
    /// frame: a `break` and a `continue`.
    EnterScope(ConstId),
    /// Leaves the scope this frame opened last, waiting for every child the
    /// body did not await, and answers a `Result`.
    ///
    /// The operand is the scope's own value — a scope is an expression, and
    /// its value is the value of its block — and what is pushed back is that
    /// value wrapped in `Ok`, or the `Err` of the first child that failed.
    /// The [`Inst::Try`] the lowering writes immediately after is what turns
    /// the second into a return from the enclosing call, which is exactly
    /// what the oracle does with it: `Interpreter::leave_scope` answers
    /// `Control::Return(Value::err(error))`, and `?` is the construct that
    /// already means that here.
    ///
    /// A child that *raised* rather than answering `Err` is not a value at
    /// all, so it propagates as the error it is and never reaches the `try`.
    LeaveScope,
    /// Cancels the scope this frame opened last and waits for its children
    /// to stop.
    ///
    /// What a `break` or a `continue` written inside a scope's body needs,
    /// exactly as [`Inst::Pop`] is what one written inside a half-evaluated
    /// expression needs: it leaves the scope without reaching the
    /// [`Inst::LeaveScope`] below it, and a scope never outlives a thread it
    /// started. Every other early exit leaves the *frame* as well, which the
    /// VM notices for itself when it pops one.
    CancelScope,
    /// `scope.spawn { ... }`: starts a thread for the closure on top of the
    /// stack, in the scope below it, and pushes the handle.
    ///
    /// Converting the closure for the new thread is the task-safety check,
    /// so a capture that may not cross a task boundary is reported here.
    /// `cove_runtime::task::spawn_into` is the one implementation both
    /// backends reach, and a lowered closure crosses because a
    /// [`FunctionId`] means the same function on every thread of a run — see
    /// this crate's "One program, and every thread of a run reads it".
    Spawn,
    /// `await task`, and the postfix `task.await()` that means the same
    /// thing: waits for the task on top of the stack and pushes its value.
    Await,
    /// `task.cancel()`: asks the task on top of the stack to stop at its next
    /// safepoint, and pushes `()`.
    ///
    /// Asking is all it does. Whether the task stopped or had already
    /// finished is known only once something waits for it, which is what
    /// `await` and leaving the scope do.
    Cancel,
    /// `shared.lock(fn(var value) { ... })`: runs the closure on top of the
    /// stack with the contents of the `Shared` below it, holding the cell for
    /// the whole of the call, and pushes what the closure answered.
    ///
    /// `lock` is a `Shared`'s only operation, because every access to what a
    /// cell holds is scoped: a read-modify-write cannot be written as two
    /// operations that race. `cove_runtime::shared::SharedCell::lock` is the
    /// whole of what that means, and both backends run this through it.
    ///
    /// # Why this is not a call through a value
    ///
    /// A closure written `fn(var value)` does not receive a copy of the
    /// cell's contents; it names them, so that `value.record(false)` changes
    /// what the cell will hold. Every argument of an [`Inst::CallValue`]
    /// travels on the value stack, and a place cannot, so this instruction
    /// makes the call itself: it stands the contents in a value slot of the
    /// *locking* frame, hands the closure a place rooted there, and reads
    /// that slot back when the closure returns — which is
    /// `Interpreter::call_shared_method`'s `Place::binding` and its
    /// `place.read` afterwards, said as a stack discipline.
    ///
    /// A closure written without `var` receives a copy and the cell keeps
    /// what it had, which is the same thing the oracle does with one. That
    /// one takes its argument on the value stack like any other closure, so
    /// which of the two this is is read off the callee's own `params`.
    Lock,
    /// Returns the top of the value stack.
    ///
    /// What a function whose `returns` is [`SlotKind::Value`] ends in, and
    /// what every one of its returns is.
    Return,
    /// Returns the top of the *scalar* stack.
    ///
    /// What a function whose declared return type the checker settled as
    /// `Int` or `Bool` ends in, and what every one of its returns is: the
    /// answer never becomes a `Value` on the way out, because the caller
    /// wanted a scalar and the callee computed one. A function that mixes
    /// this with [`Inst::Return`] is a `validate` failure, since `returns`
    /// names one stack and a caller reads exactly that one.
    ReturnScalar,
}

/// The unary operators the IR carries, which are the language's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

/// What [`Inst::IntBinary`] does to two integers.
///
/// A separate enum from [`BinaryOp`] because it is a smaller question: these
/// are the operators `Int` answers, and nothing here has a case for a type
/// that cannot arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The binary operators the IR carries.
///
/// `&&` and `||` are absent on purpose: they short-circuit, so they lower to
/// a jump rather than to an operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
}

/// A construct the lowering does not cover.
///
/// Named rather than approximated: ADR 0019 requires that a VM run either
/// finish on the VM or fail before any side effect, so an unsupported
/// construct has to stop the lowering and say what it was.
#[derive(Clone, Debug)]
pub struct Unsupported {
    /// What was not lowered, in the words a Cove programmer would use.
    pub what: String,
    pub span: Span,
}

impl fmt::Display for Unsupported {
    /// "yet" stands before the construct rather than after it.
    ///
    /// It used to be appended, which reads well for a construct named by a
    /// noun phrase — "the VM cannot run a task scope yet" — and falls over
    /// for one named by a phrase ending in a clause, which several are:
    /// "the VM cannot run `g` used as a value, whose parameter `n` has a
    /// default yet" attaches the "yet" to the wrong verb. Moving it leaves
    /// the sentence correct whatever [`Unsupported::what`] is, which is what
    /// a template has to be, since what goes into it is written a construct
    /// at a time.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the VM cannot yet run {}", self.what)
    }
}

impl Unsupported {
    /// Reports that `what` has no lowering.
    pub fn new(what: impl Into<String>, span: Span) -> Unsupported {
        Unsupported {
            what: what.into(),
            span,
        }
    }

    /// The diagnostic a refusal is reported as, wherever it is reported
    /// from.
    ///
    /// It names the construct and points at it, because the only useful
    /// thing to say about a backend that cannot run a program is which part
    /// of the program it was. It is an error rather than a note because
    /// ADR 0019 forbids the alternative: a run that continued here would be
    /// a run that finished on the interpreter while reporting the VM, and
    /// every measurement and every conformance claim taken from it would be
    /// about a mixture.
    ///
    /// It lives beside the refusal rather than in the CLI because four
    /// commands can now produce it -- `cove run`, `cove generate`, `cove
    /// test`, and `cove build` -- and a construct named in three wordings
    /// would read as three different problems.
    pub fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic::error("cove::backend::unsupported", self.to_string())
            .at(self.span)
            .rule(
                "A run on the VM either finishes on the VM or fails before any side effect; it never falls back to the interpreter.",
            )
            .help("run it on the interpreter with `--backend ast`")
    }
}

// ------------------------------------------------------------------ printing

/// Renders one function the way a golden test reads it.
///
/// Deterministic and stable under anything that does not change the
/// instructions, so a test can assert on the whole listing rather than on
/// whichever part it thought to check.
pub fn render(program: &Program, id: FunctionId) -> String {
    let function = program.function(id);
    let mut out = String::new();
    // A reader decodes a listing's slot numbers in two steps: `params=[...]`
    // below is slots `0..arity`, in that order, whatever kind each parameter
    // is; and the three widths written here are what everything from `arity`
    // on is grouped into — the remaining scalars, then the remaining values,
    // then the places — in this same order. Written scalars-first,
    // values-second, places-third for that reason, and not because a slot's
    // number can be decoded from these three widths alone anymore.
    out.push_str(&format!(
        "fn {}.{} arity={} frame={}/{}",
        function.module,
        function.name,
        function.arity,
        function.scalar_frame_size,
        function.value_frame_size
    ));
    // The third region is written only where there is one, exactly as
    // `params` and `captures` below are written only where they are not
    // empty. A listing should say what a function has and stay quiet about
    // what it does not, and almost no function has a place slot.
    if function.place_frame_size > 0 {
        out.push_str(&format!("/{}", function.place_frame_size));
    }
    if !function.params.is_empty() {
        let params: Vec<&str> = function.params.iter().copied().map(render_kind).collect();
        out.push_str(&format!(" params=[{}]", params.join(", ")));
    }
    if function.has_receiver {
        out.push_str(" receiver");
    }
    if !function.captures.is_empty() {
        let names: Vec<&str> = function.captures.iter().map(|c| &*c.name).collect();
        out.push_str(&format!(" captures=[{}]", names.join(", ")));
    }
    out.push_str(&format!(" -> {}", render_kind(function.returns)));
    out.push('\n');
    for (pc, inst) in function.code.iter().enumerate() {
        out.push_str(&format!("{pc:4}  {}\n", render_inst(program, *inst)));
    }
    out
}

/// Which stack a slot lives in, as a listing names it: the scalar's by what
/// it holds, and the value stack's by being the one that holds anything.
///
/// `pub(crate)` rather than private, because `crate::lower::validate` names a
/// mismatched [`SlotKind`] in its own diagnostics and a second rendering
/// beside this one is exactly the kind of duplicate description this crate's
/// doc comments keep arguing against.
pub(crate) fn render_kind(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Value => "value",
        SlotKind::Scalar(Scalar::Int) => "Int",
        SlotKind::Scalar(Scalar::Bool) => "Bool",
        SlotKind::Place => "place",
    }
}

/// Renders one instruction, resolving the constants it names so that a
/// listing reads without a second table beside it.
fn render_inst(program: &Program, inst: Inst) -> String {
    let name = |id: ConstId| match program.constant(id) {
        Const::Name(name) => name.to_string(),
        other => format!("{other:?}"),
    };
    match inst {
        Inst::Const(id) => format!("const {:?}", program.constant(id)),
        Inst::LoadLocal(slot) => format!("load {slot}"),
        Inst::StoreLocal(slot) => format!("store {slot}"),
        Inst::Pop => "pop".to_string(),
        Inst::Dup => "dup".to_string(),
        Inst::Unary(op) => format!("unary {op:?}"),
        Inst::Binary(op) => format!("binary {op:?}"),
        Inst::IntBinary(op) => format!("int {op:?}"),
        Inst::ScalarConst(value) => format!("scalar-const {value}"),
        Inst::LoadScalar(slot) => format!("load-scalar {slot}"),
        Inst::StoreScalar(slot) => format!("store-scalar {slot}"),
        Inst::ScalarPop => "scalar-pop".to_string(),
        Inst::JumpIfFalseScalar(to) => format!("jump-if-false-scalar {to}"),
        Inst::JumpIfTrueScalar(to) => format!("jump-if-true-scalar {to}"),
        Inst::ScalarToValue(what) => format!("scalar-to-value {what:?}"),
        Inst::ValueToScalar => "value-to-scalar".to_string(),
        Inst::GetFieldAt { at, .. } => format!("get-field-at {at}"),
        Inst::GetFieldAtScalar(index) => format!("get-field-at-scalar {index}"),
        Inst::Jump(to) => format!("jump {to}"),
        Inst::JumpIfFalse(to) => format!("jump-if-false {to}"),
        Inst::JumpIfTrue(to) => format!("jump-if-true {to}"),
        Inst::Call {
            function,
            value_argc,
            scalar_argc,
            place_argc,
            returns_scalar,
        } => {
            let target = program.function(function);
            let answer = if returns_scalar { " -> scalar" } else { "" };
            // The third count only where there is one, for the reason
            // `render` writes the third frame window only where there is
            // one: a call that passes no place has nothing to say about it.
            let places = if place_argc > 0 {
                format!("/{place_argc}")
            } else {
                String::new()
            };
            format!(
                "call {}.{} argc={value_argc}/{scalar_argc}{places}{answer}",
                target.module, target.name
            )
        }
        Inst::MakeClosure { function, captures } => {
            let target = program.function(function);
            format!(
                "make-closure {}.{} captures={captures}",
                target.module, target.name
            )
        }
        Inst::CallValue { argc } => format!("call-value argc={argc}"),
        Inst::CallHost { module, op, argc } => {
            format!("call-host {}.{} argc={argc}", name(module), name(op))
        }
        Inst::CallResource { op, argc } => {
            format!("call-resource {} argc={argc}", name(op))
        }
        Inst::CallBuiltin { name: n, argc } => format!("call-builtin {} argc={argc}", name(n)),
        Inst::MakeArray(len) => format!("make-array {len}"),
        // The operator the range was written with, because that is what the
        // flag means and a listing should read as the source does.
        Inst::MakeRange { inclusive_end } => {
            format!("make-range {}", if inclusive_end { ".." } else { "..<" })
        }
        Inst::Concat(parts) => format!("concat {parts}"),
        Inst::MakeStruct(of) => {
            let declared = program.struct_type(of);
            format!(
                "make-struct {} fields={}",
                declared.name,
                declared.field_names().join(",")
            )
        }
        Inst::GetField(n) => format!("get-field {}", name(n)),
        Inst::SetField(n) => format!("set-field {}", name(n)),
        Inst::MakeBuiltin { name: n, argc } => format!("make-builtin {} argc={argc}", name(n)),
        Inst::MakeHostEnum { ty, case } => {
            format!("make-host-enum {}.{}", name(ty), name(case))
        }
        Inst::MakeEnum { ty, case, argc } => {
            format!("make-enum {}.{} argc={argc}", name(ty), name(case))
        }
        Inst::CallBuiltinAssoc { ty, name: n, argc } => {
            format!("call-assoc {}.{} argc={argc}", name(ty), name(n))
        }
        Inst::PlaceLocal(slot) => format!("place {slot}"),
        Inst::PlaceScalar(slot, what) => format!("place-scalar {slot} {what:?}"),
        Inst::LoadPlace(slot) => format!("load-place {slot}"),
        Inst::PlaceField(index) => format!("place-field {index}"),
        Inst::PlacePop => "place-pop".to_string(),
        Inst::PlaceRead => "place-read".to_string(),
        Inst::PlaceWrite => "place-write".to_string(),
        Inst::Freeze => "freeze".to_string(),
        Inst::Snapshot => "snapshot".to_string(),
        // The depth is written only where there is one, exactly as a
        // listing writes a place window only where a function has one.
        Inst::MakeDyn { trait_name, depth } => match depth {
            0 => format!("make-dyn {}", name(trait_name)),
            _ => format!("make-dyn {} inside {depth}", name(trait_name)),
        },
        // The candidates are written out, because they are the whole of what
        // this instruction is: a listing that named the site by number would
        // say nothing about where the call can go.
        Inst::CallDyn { site, argc } => {
            let dispatch = &program.dispatches[site.0 as usize];
            let cases: Vec<&str> = dispatch.cases.iter().map(|(ty, _)| &**ty).collect();
            format!(
                "call-dyn {}.{} argc={argc} [{}]",
                dispatch.trait_name,
                dispatch.method,
                cases.join(", ")
            )
        }
        Inst::Lock => "lock".to_string(),
        Inst::EnterScope(scope) => format!("enter-scope {}", name(scope)),
        Inst::LeaveScope => "leave-scope".to_string(),
        Inst::CancelScope => "cancel-scope".to_string(),
        Inst::Spawn => "spawn".to_string(),
        Inst::Await => "await".to_string(),
        Inst::Cancel => "cancel".to_string(),
        Inst::SpreadArgument => "spread-argument".to_string(),
        Inst::TestCase(case) => format!("test-case {}", name(case)),
        Inst::GetPayload {
            of: Some((of, case)),
            at,
        } => {
            let declared = program.enum_type(of);
            format!(
                "get-payload {}.{} {at}",
                declared.name, declared.cases[case as usize].name
            )
        }
        Inst::GetPayload { of: None, at } => format!("get-payload {at}"),
        Inst::IterItems => "iter-items".to_string(),
        Inst::NoMatch => "no-match".to_string(),
        Inst::Try => "try".to_string(),
        Inst::Return => "return".to_string(),
        Inst::ReturnScalar => "return-scalar".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::Program;

    /// The point of the `Arc<str>` throughout this file. ADR 0008 gives a
    /// spawned task a thread, and that thread's VM runs a function of this
    /// program — so if this ever stopped holding, a task's body could not be
    /// reached from the thread that runs it.
    #[test]
    fn a_lowered_program_can_be_read_by_every_thread_of_a_run() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Program>();
    }
}
