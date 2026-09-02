//! The instructions.
//!
//! Every instruction names its operands and its destination by **slot
//! number**. There is no operand stack: no push, no pop, no stack-effect
//! table, no discipline to get wrong.
//!
//! That is [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md)'s
//! *"parameters, locals, temporaries and captures share the one slot
//! numbering"* taken literally. If a temporary is a slot, then an
//! instruction that consumes a temporary names a slot, and the thing an
//! operand stack exists to provide is already there.
//!
//! Two things fall out of that, and they are why it is worth choosing:
//!
//! - **A frame's roots are a static fact.** A stack machine's set of live
//!   references changes as operands are pushed and popped, so its reference
//!   map has to be indexed by program counter. Here the map does not change
//!   between a function's first instruction and its last, and
//!   [`crate::RefMap`] is one bit per slot.
//! - **A call needs no argument buffer.** The callee's frame begins where
//!   the caller's ends, so [`Inst::Call`] copies the words of argument *i*
//!   into the run parameter *i* occupies and transfers control. Nothing is
//!   pushed, permuted, or copied back.
//!
//! # The instruction set describes families, not cases
//!
//! There is one `LoadField`, not one per value kind that has fields; one
//! `Arith`, not one per numeric type; one `Alloc`, not one per collection.
//! A field of an *inline* value needs no instruction at all — it is a slot
//! offset the lowering computes.
//! What an object *is* is a question the object answers at run time, from
//! its own header. Nothing here grows a case because a corpus program was
//! refused, because nothing here refuses anything.

use crate::layout::LayoutId;
use crate::{ArgsId, BuiltinId, FunctionId, HostOpId, StrId, TableId};

/// A slot in the current frame: `memory[frame_base + slot]`.
pub type Slot = u32;

/// An index into a function's instructions.
pub type Pc = u32;

/// Which numeric interpretation an arithmetic or comparison instruction
/// gives its operand words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Num {
    /// Two's-complement `i64`. Also what a `Duration` is arithmetic on:
    /// nanoseconds add like integers, and only the boundary cares that the
    /// answer is called a `Duration`.
    Int,
    /// An IEEE-754 double, bit-cast out of the word.
    Float,
}

/// What a comparison compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    Int,
    Float,
    Bool,
    /// The bytes of two [`crate::Shape::Str`] objects.
    Str,
    /// Two words, as words.
    ///
    /// This is `is`: the identity comparison the language reserves for
    /// shared storage, and it is the one comparison that is allowed to look
    /// at a reference as bits, because that is what it is asking about.
    Identity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A conversion between two scalar representations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Convert {
    /// `Int` to `Float`, as `as`-style widening.
    IntToFloat,
    /// `Float` to `Int`, truncating toward zero.
    FloatToInt,
}

/// How many elements an [`Inst::Alloc`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Len {
    /// A shape whose size the layout already fixes: a struct, an enum, a
    /// closure, a box.
    Fixed,
    /// A count the lowering knew: a literal array's element count, a string
    /// literal's byte count.
    Count(u32),
    /// A count in a slot, as an `Int`.
    Slot(Slot),
}

/// One instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    // ---- constants and moves ------------------------------------------
    /// `dst = ()`
    Unit { dst: Slot },
    /// `dst = value`
    Bool { dst: Slot, value: bool },
    /// `dst = value`, also how a `Duration` literal reaches a slot.
    Int { dst: Slot, value: i64 },
    /// `dst = f64::from_bits(bits)`
    ///
    /// The bits rather than the `f64` so that [`Inst`] can be `Eq` and
    /// `Hash`ed, and so that a NaN in the source survives the IR unchanged.
    Float { dst: Slot, bits: u64 },
    /// `dst = <a string object for `text`>`
    ///
    /// The object is allocated on first use and shared afterwards: a string
    /// literal in a loop allocates once for the run, not once per turn.
    Str { dst: Slot, text: StrId },
    /// `dst = src`, for the words `layout` describes.
    ///
    /// This is ADR 0001's field-wise shallow copy, and it is one operation
    /// because a value's words are where the value is. Copying a
    /// `Wrapper { p: Point, v: Vector }` copies three words: the `Point`
    /// becomes independent because its words were copied, and the `Vector`
    /// stays shared because what was copied is its address. Both answers
    /// fall out of the same copy and neither needs a policy.
    ///
    /// There is no sharing bit, no copy-on-write and no unsharing of a write
    /// path. Those were needed only while every struct was one address, and
    /// they existed to conceal an alias the representation had created.
    ///
    /// `let` and `var` lower to the same thing: ADR 0001 says they do not
    /// change expression semantics, and Cove has no move semantics. A
    /// lowering may elide a copy whose source is a fresh temporary, but that
    /// is an optimisation — correctness never depends on proving uniqueness,
    /// and a lowering that cannot tell emits the copy.
    Copy {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// Zeroes the words `layout` describes at `slot`.
    ///
    /// A slot whose value is dead. The lowering emits one at the end of the
    /// scope a binding belonged to, and at a temporary's last use, for every
    /// slot whose [`Repr`](crate::Repr) is [`Ref`](crate::Repr::Ref) or
    /// [`Addr`](crate::Repr::Addr).
    ///
    /// This is what keeps a static reference map from turning into a leak.
    /// The map says which slots the collector *reads*; it cannot say when
    /// the value in one stopped being needed, because that is a fact about a
    /// program point and the map is a fact about a function. Clearing the
    /// slot moves the answer into the data: a dead reference slot holds
    /// null, the collector reads null, and the object is unreachable at the
    /// next collection rather than at the next return.
    ///
    /// It costs one store on a path that was going to leave the value behind
    /// anyway, and it is emitted only where the slot would otherwise retain
    /// something — never for a scalar, and never where the slot is about to
    /// be overwritten.
    Clear { slot: Slot, layout: LayoutId },

    // ---- scalar operations --------------------------------------------
    /// `dst = -a`
    Neg { num: Num, dst: Slot, a: Slot },
    /// `dst = a op b`
    Arith {
        num: Num,
        op: ArithOp,
        dst: Slot,
        a: Slot,
        b: Slot,
    },
    /// `dst = a op b`, answering a `Bool`.
    Cmp {
        on: Compare,
        op: CmpOp,
        dst: Slot,
        a: Slot,
        b: Slot,
    },
    /// `dst = !a`
    Not { dst: Slot, a: Slot },
    /// `dst = <a, converted>`
    Convert { to: Convert, dst: Slot, a: Slot },

    // ---- control flow --------------------------------------------------
    /// Continue at `to`.
    Jump { to: Pc },
    /// Continue at `to` when `cond` is false; otherwise fall through.
    ///
    /// One conditional branch rather than two: `&&`, `||`, `if` and `while`
    /// all lower through it, and the lowering inverts the condition rather
    /// than the instruction set carrying both polarities.
    BranchFalse { cond: Slot, to: Pc },
    /// Continue at the entry of `table` selected by the `Int` in `on`.
    ///
    /// This is how a `match` over an enum's cases dispatches: `on` is the
    /// case index read out of the object, and the table has one target per
    /// case plus a default.
    Switch { on: Slot, table: TableId },
    /// Leave the function, answering the word in `src`.
    Return { src: Slot },

    // ---- calls ----------------------------------------------------------
    /// `dst = callee(args...)`
    ///
    /// The machine writes `args[i]` into the callee's slot `i` and gives it
    /// a frame beginning at the end of this one. Nothing else happens: the
    /// argument list is static, the destination is declared, and there is no
    /// buffer between the two frames.
    Call {
        dst: Slot,
        callee: FunctionId,
        args: ArgsId,
    },
    /// `dst = closure(args...)`, where `closure` holds a reference to a
    /// [`crate::Shape::Closure`] object.
    ///
    /// The callee is the function id in the object's first payload word, and
    /// its captures are copied into the slots after the parameters.
    CallClosure {
        dst: Slot,
        closure: Slot,
        args: ArgsId,
    },
    /// `dst = <host op>(args...)`
    ///
    /// This is a boundary: the arguments are materialised into public
    /// public `Value`s, the host answers one, and the answer
    /// is written back into a word. It is the only place in ordinary
    /// execution where a `Value` exists.
    CallHost {
        dst: Slot,
        op: HostOpId,
        args: ArgsId,
    },
    /// `dst = <host op>(*receiver, args...)`, addressed to the resource
    /// the [`Repr::Host`](crate::Repr::Host) word in `receiver` names.
    ///
    /// The same boundary [`Inst::CallHost`] is, reached the other way a
    /// callee can be found. `Call` and `CallClosure` are already that pair
    /// on this side of the boundary — a callee named statically, and a
    /// callee in a slot — and a host resource's operations are the same
    /// distinction one boundary further out: ADR 0013 gives the *host* the
    /// table of what is open, so `files.Writer.writeLine` is dispatched on
    /// the handle and not on the module the source wrote in front of it.
    ///
    /// The receiver is an operand of its own rather than `args[0]`, and that
    /// is the difference that decides there are two instructions here rather
    /// than a flag on one. An [`crate::Arg`] is a value location the
    /// boundary *materialises*, and the registry does not take the handle as
    /// an argument — `HostRegistry::call_resource` takes it as the thing
    /// being addressed and hands the host only what follows. So putting it
    /// in the list would mean materialising a name into a `Value` in order
    /// to take it apart again, and the argument list would no longer be the
    /// arguments.
    CallResource {
        dst: Slot,
        receiver: Slot,
        op: HostOpId,
        args: ArgsId,
    },
    /// `dst = <builtin>(args...)`
    ///
    /// A builtin operates on words and heap objects directly. It is not a
    /// boundary and it does not materialise anything.
    CallBuiltin {
        dst: Slot,
        builtin: BuiltinId,
        args: ArgsId,
    },

    // ---- the heap --------------------------------------------------------
    /// `dst = <a new object of `layout`>`
    ///
    /// The payload is zeroed, so a reference field of a half-built object is
    /// null rather than garbage if a collection happens before it is
    /// filled in.
    Alloc {
        dst: Slot,
        layout: LayoutId,
        len: Len,
    },
    /// `dst = <the value at payload word `at` of `obj`>`
    ///
    /// One instruction for every fixed-position read there is: a struct
    /// field, an enum's case index (`at == 0`) or payload word, a closure's
    /// capture. The lowering computes `at` from the layout it knows
    /// statically; the machine bounds-checks it against the layout the
    /// object names, because a reference slot carries no layout of its own.
    LoadField {
        dst: Slot,
        obj: Slot,
        at: u32,
        layout: LayoutId,
    },
    /// `<payload word `at` of `obj`> = src`
    StoreField {
        obj: Slot,
        at: u32,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = obj[index]`, for an object whose elements are `layout` wide.
    ///
    /// The stride is the element layout's width, so an `Array<Point>` is a
    /// run of two-word elements rather than a run of addresses.
    LoadElem {
        dst: Slot,
        obj: Slot,
        index: Slot,
        layout: LayoutId,
    },
    /// `obj[index] = src`
    StoreElem {
        obj: Slot,
        index: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = <obj's header length>`: an element count, or a string's bytes.
    Len { dst: Slot, obj: Slot },
    /// `dst = <the [`LayoutId`] in obj's header>`, as an `Int`.
    ///
    /// The other half of the header word [`Inst::Len`] reads, and it is here
    /// for the same reason: *what an object is* is a question the object
    /// answers at run time, from its own header, and a `Ref` slot carries no
    /// layout of its own.
    ///
    /// It exists because a dispatch has to ask it. A `dyn Trait` value's
    /// implementation is decided by the type behind it, and nothing static
    /// says which that is; the object's header does. Reading it into a slot
    /// turns "which implementation" into an ordinary [`Inst::Switch`] over a
    /// table the lowering builds from the trait's declared conformances,
    /// which is why there is no dispatch instruction — one general question
    /// about an object, answered with the control flow that is already here.
    LayoutOf { dst: Slot, obj: Slot },

    // ---- places ----------------------------------------------------------
    /// `dst = &frame[slot]`
    ///
    /// A place is one word. There is no place object, no place stack and no
    /// table of places; a `var` parameter is an ordinary slot whose
    /// [`Repr`](crate::Repr) is [`Addr`](crate::Repr::Addr).
    AddrOfSlot { dst: Slot, slot: Slot },
    /// `dst = &<payload word `at` of `obj`>`
    ///
    /// The lowering keeps `obj` in a live reference slot for exactly the
    /// address's live range, and clears that slot with [`Inst::Clear`] when
    /// the address dies — not unconditionally for the rest of the frame,
    /// which would retain the object across everything a long-running body
    /// does afterwards. The collector therefore needs no interior-pointer
    /// logic, and the heap does not move, so the address stays correct
    /// across a collection for as long as it is live and no longer.
    AddrOfField { dst: Slot, obj: Slot, at: u32 },
    /// `dst = &obj[index]`, at a stride of `layout`'s width.
    AddrOfElem {
        dst: Slot,
        obj: Slot,
        index: Slot,
        layout: LayoutId,
    },
    /// `dst = addr + at`, a static word offset into the value at `addr`.
    ///
    /// The one place instruction whose operand is itself a place, and what
    /// makes a place composable. A place is the address of the *first* word
    /// of a value location, so without this a `var` parameter could only name
    /// the whole of what it was given: `p.y = 1` through a `var p: Point` had
    /// to load both words, write one and store both back — observationally
    /// the same on one thread, but not what the address was for — and
    /// `f(var p.y)` could not be lowered at all, because there was no way to
    /// form the address to pass.
    ///
    /// `at` is a word offset within the value the address names, computed by
    /// the lowering from the layout the checker settled. It is the same
    /// arithmetic a field of an inline struct is, done to an address instead
    /// of to a slot number, and the answer is again the address of the first
    /// word of a value location — so it goes back through [`Inst::Load`],
    /// [`Inst::Store`] or another of these with no second rule about what an
    /// address is.
    ///
    /// Nothing checks `at` against the value's extent, because a frame does
    /// not record one: what an address names is a fact about the instruction
    /// that formed it, and [`mod@crate::verify`] says the same of
    /// [`Inst::Switch`]'s operand for the same reason.
    AddrOfPart { dst: Slot, addr: Slot, at: u32 },
    /// `dst = *addr`, for the words `layout` describes.
    Load {
        dst: Slot,
        addr: Slot,
        layout: LayoutId,
    },
    /// `*addr = src`, for the words `layout` describes.
    ///
    /// A nested write through a `var` parameter updates the destination words
    /// in place. There is nothing between the address and the words, which is
    /// what a place being an address of the *first word* of a value location
    /// buys.
    Store {
        addr: Slot,
        src: Slot,
        layout: LayoutId,
    },

    // ---- erasure ----------------------------------------------------------
    /// `dst = <a box holding the words of `src`, tagged `layout`>`
    ///
    /// What a value becomes when its static type is not known: `dyn Trait`,
    /// a Host result a schema declared `Any`, an expression the checker
    /// declined to type. One word in the slot either way.
    Box {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = <the value inside the box in `src`>`, trapping if its tag is
    /// not `layout`.
    Unbox {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },

    // ---- failure ----------------------------------------------------------
    /// Fail the run with `message`.
    ///
    /// This is what an exhausted `match` and a failed `Unbox` reach. It is
    /// not a refusal to run the program: the program ran, and this is what
    /// it did.
    Trap { message: StrId },

    /// Record that an assertion failed here, carrying the `String` in
    /// `message`.
    ///
    /// The one instruction that writes nothing a program can read. An
    /// assertion is lowered rather than performed — see this crate's
    /// `lower::assertions` — so by the time the failing arm runs, the
    /// `Err(Error("assertion failed: ..."))` is an ordinary value and the
    /// only thing left that the machine knows and the value does not is
    /// *where it was written*. A test runner points at the assertion the way
    /// every other error points at source, and this is how it is told.
    ///
    /// The span is the instruction's own, which is the assertion call's, so
    /// nothing has to be threaded through the program to carry it. The
    /// message is a slot rather than a [`StrId`] because `assertEqual`
    /// renders the two values it compared and that string is built at run
    /// time; a runner compares it against the `Err` it is holding, so that a
    /// later unrelated failure is not reported at this assertion.
    AssertFailed { message: Slot },
}
