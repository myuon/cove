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
//! # A third stack, for the arguments that are not values either
//!
//! A `var` parameter does not receive a copy of anything. It names the
//! caller's own storage, so that `bump(var total)` adds to the binding the
//! caller wrote and not to a copy that is written back afterwards — which is
//! observably a different language, since `two(var x, var x)` answers 11
//! rather than 10 and only aliasing gives that answer.
//!
//! What names storage here is a *place*: an index into the value stack,
//! together with the field positions to walk from what stands there.
//! [`SlotKind::Place`] is the third kind a slot can have, a `var`
//! parameter's argument travels on the third stack, and
//! [`Function::place_frame_size`] is that stack's window the way the other
//! two have theirs. Nothing about it is new machinery: it is
//! [`SlotKind::Scalar`]'s arrangement asked of a third representation, and
//! `Inst::Call`'s per-stack counts, `Function::params`, and `validate`
//! already generalise over the question.
//!
//! An index rather than a pointer, because the value stack is one `Vec` that
//! grows, and an index survives a reallocation that a borrow could not even
//! be written across. It is sound for as long as the frame it addresses is
//! live, and a callee cannot outlive its caller while nothing lowers a
//! closure — see the note on [`Inst::PlaceLocal`], which is where that
//! dependency is stated for whoever lowers one.
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
use std::rc::Rc;

use cove_diag::Span;

/// One lowered program: every function it can execute, and the constants they
/// share.
#[derive(Debug, Default)]
pub struct Program {
    /// Every lowered function, addressed by [`FunctionId`].
    pub functions: Vec<Function>,
    /// Every constant any function loads, addressed by [`ConstId`].
    pub constants: Vec<Const>,
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
    pub module: Rc<str>,
    pub name: Rc<str>,
    /// How many slots of the *value* stack one call needs: `self` if it has
    /// a receiver, then each parameter `params` names a value slot for, then
    /// every `Value` local and temporary the body declares.
    ///
    /// [`Inst::LoadLocal`] and [`Inst::StoreLocal`] address `0..value_frame_size`
    /// of the value stack; nothing else does. That makes this the frame
    /// metadata a precise root set is read from: a frame's whole value
    /// window, `stack[base .. base + value_frame_size]`, is its root set,
    /// with nothing to skip inside it, because a scalar slot is not numbered
    /// in this space at all — it lives in the other stack, addressed by
    /// [`Inst::LoadScalar`] and [`Inst::StoreScalar`] through
    /// `scalar_frame_size` instead.
    pub value_frame_size: u32,
    /// How many slots of the *scalar* stack one call needs: every `Int` or
    /// `Bool` parameter, local, and temporary the body declares.
    ///
    /// [`Inst::LoadScalar`] and [`Inst::StoreScalar`] address
    /// `0..scalar_frame_size` of the scalar stack; nothing else does. A
    /// scalar parameter is counted here and a value parameter is counted in
    /// `value_frame_size`, because an argument arrives on the stack its own
    /// type names and becomes the callee's slot there without moving; see
    /// `params`.
    pub scalar_frame_size: u32,
    /// How many slots of the *place* stack one call needs, which today is
    /// every `var` parameter and a `var self` receiver and nothing else.
    ///
    /// [`Inst::LoadPlace`] addresses `0..place_frame_size` of the place
    /// stack; nothing else does, and nothing stores into it, because a place
    /// slot is filled by the calling convention and never assigned. A body
    /// declares no place slots of its own: `var` is a property of a
    /// parameter, and a local that a `var` argument is rooted at is an
    /// ordinary value slot that a place *names* rather than a place itself.
    ///
    /// Not a root set, and not a hole in one either. A place holds an index
    /// and a path of field positions, so it reaches a `Value` only by way of
    /// the value stack, whose own window is already scanned.
    pub place_frame_size: u32,
    /// How many arguments a call must supply, `self` included.
    pub arity: u32,
    /// Which stack each of those arguments arrives on, in the order a call
    /// supplies them — the receiver first when `has_receiver`, then the
    /// declared parameters in declaration order.
    ///
    /// This is the calling convention, and it is the callee's to state
    /// because the callee is what a slot number means. An argument is
    /// pushed onto the stack its own settled type names and *becomes* the
    /// callee's slot in that stack without moving, so the value parameters
    /// occupy the first value slots in the order they appear here and the
    /// scalar parameters the first scalar slots, each dense within its own
    /// stack. `params.len()` is `arity`, which `validate` checks rather than
    /// assumes.
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
    /// What the closure that created this body handed it, in the order the
    /// instructions expect. Empty for a declared function.
    pub captures: Vec<Rc<str>>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// A `Value` in the value stack.
    Value,
    /// An `i64` in the scalar stack, meaning what [`Scalar`] says.
    Scalar(Scalar),
    /// A place in the place stack: an index into the value stack and the
    /// field positions to walk from it.
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
}

/// Addresses a [`Function`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// Addresses a [`Const`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstId(pub u32);

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
    Str(Rc<str>),
    /// A name carried by an instruction: a field, a host module, a host
    /// operation, a builtin method, or a declared type.
    Name(Rc<str>),
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
    LoadLocal(u32),
    /// Pops a value into a frame slot.
    StoreLocal(u32),
    /// Pushes one of this call's captures.
    LoadCapture(u32),
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
    /// The slot is a [`SlotKind::Scalar`] one; `validate` is where that is
    /// checked, so nothing here asks.
    LoadScalar(u32),
    /// Pops the scalar stack into a frame slot.
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
    GetFieldAt(u32),
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
    /// Builds a declared struct. `ty` is a `Const::Name` holding the qualified
    /// type name, and `fields` names each field in the order the values were
    /// pushed.
    MakeStruct { ty: ConstId, fields: ConstId },
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
    GetPayload(u32),
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
    /// an absolute index into the value stack rather than a borrow of it,
    /// which is what lets the `Vec` reallocate under a place that is
    /// standing, and it is valid for exactly as long as the frame it
    /// addresses is live.
    ///
    /// **That is sound because a callee cannot outlive its caller.** Nothing
    /// lowers a closure, so nothing a lowered program can build holds a
    /// place past the return of the frame that made it: a place travels down
    /// into a call and back out as nothing at all. A closure would break
    /// exactly that, since it can be returned, and whoever lowers one has to
    /// decide what a captured `var` binding is before this instruction can
    /// keep being an index. This paragraph is the note that says so.
    ///
    /// The slot is a [`SlotKind::Value`] one, checked by `validate` rather
    /// than asked about here: a place cannot address the scalar stack, which
    /// is why `crate::lower` puts a binding that a `var` argument is rooted
    /// at into a value slot even where the checker settled it as `Int`.
    PlaceLocal(u32),
    /// Pushes a copy of the place in one of this frame's *place* slots.
    ///
    /// A `var` parameter's slot holds a place, so this is how the parameter
    /// is reached at all: passing it on as a `var` argument is this alone,
    /// which is what makes the callee's callee alias the original binding
    /// rather than the parameter's own slot; reading it is this and a
    /// [`Inst::PlaceRead`]; writing to it is this and a
    /// [`Inst::PlaceWrite`].
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the VM cannot run {} yet", self.what)
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
    out.push_str(&format!(
        "fn {}.{} arity={} frame={}/{}",
        function.module,
        function.name,
        function.arity,
        function.value_frame_size,
        function.scalar_frame_size
    ));
    // The third window is written only where there is one, exactly as
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
        out.push_str(&format!(" captures=[{}]", function.captures.join(", ")));
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
fn render_kind(kind: SlotKind) -> &'static str {
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
        Inst::LoadCapture(index) => format!("capture {index}"),
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
        Inst::GetFieldAt(index) => format!("get-field-at {index}"),
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
        Inst::MakeStruct { ty, fields } => {
            format!("make-struct {} fields={}", name(ty), name(fields))
        }
        Inst::GetField(n) => format!("get-field {}", name(n)),
        Inst::SetField(n) => format!("set-field {}", name(n)),
        Inst::MakeBuiltin { name: n, argc } => format!("make-builtin {} argc={argc}", name(n)),
        Inst::MakeEnum { ty, case, argc } => {
            format!("make-enum {}.{} argc={argc}", name(ty), name(case))
        }
        Inst::CallBuiltinAssoc { ty, name: n, argc } => {
            format!("call-assoc {}.{} argc={argc}", name(ty), name(n))
        }
        Inst::PlaceLocal(slot) => format!("place {slot}"),
        Inst::LoadPlace(slot) => format!("load-place {slot}"),
        Inst::PlaceField(index) => format!("place-field {index}"),
        Inst::PlacePop => "place-pop".to_string(),
        Inst::PlaceRead => "place-read".to_string(),
        Inst::PlaceWrite => "place-write".to_string(),
        Inst::Freeze => "freeze".to_string(),
        Inst::Snapshot => "snapshot".to_string(),
        Inst::SpreadArgument => "spread-argument".to_string(),
        Inst::TestCase(case) => format!("test-case {}", name(case)),
        Inst::GetPayload(index) => format!("get-payload {index}"),
        Inst::IterItems => "iter-items".to_string(),
        Inst::NoMatch => "no-match".to_string(),
        Inst::Try => "try".to_string(),
        Inst::Return => "return".to_string(),
        Inst::ReturnScalar => "return-scalar".to_string(),
    }
}
