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
//!   the caller's ends, so [`Inst::Call`] copies argument slot *i* to callee
//!   slot *i* and transfers control. Nothing is pushed, permuted, or copied
//!   back.
//!
//! # The instruction set describes families, not cases
//!
//! There is one `GetWord`, not one per value kind that has fields; one
//! `Arith`, not one per numeric type; one `Alloc`, not one per collection.
//! What an object *is* is a question the object answers at run time, from
//! its own header. Nothing here grows a case because a corpus program was
//! refused, because nothing here refuses anything.

use crate::layout::LayoutId;
use crate::repr::Repr;
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
    /// `dst = src`
    Move { dst: Slot, src: Slot },
    /// `slot = 0`
    ///
    /// A slot whose value is dead. The lowering emits one at the end of the
    /// scope a binding belonged to, and at a temporary's last use, for every
    /// slot whose [`Repr`] is [`Repr::Ref`] or [`Repr::Addr`].
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
    Clear { slot: Slot },

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
    /// `dst = obj.payload[at]`
    ///
    /// One instruction for every fixed-position read there is: a struct
    /// field, an enum's case index (`at == 0`) or payload word, a closure's
    /// capture. The lowering computes `at` from the layout it knows
    /// statically; the machine bounds-checks it against the layout the
    /// object names, because a reference slot carries no layout of its own.
    GetWord { dst: Slot, obj: Slot, at: u32 },
    /// `obj.payload[at] = src`
    SetWord { obj: Slot, at: u32, src: Slot },
    /// `dst = obj[index]`, for a [`crate::Shape::Elements`] object.
    GetElem { dst: Slot, obj: Slot, index: Slot },
    /// `obj[index] = src`
    SetElem { obj: Slot, index: Slot, src: Slot },
    /// `dst = <obj's header length>`: an element count, or a string's bytes.
    Len { dst: Slot, obj: Slot },

    // ---- places ----------------------------------------------------------
    /// `dst = &frame[slot]`
    ///
    /// A place is one word. There is no place object, no place stack and no
    /// table of places; a `var` parameter is an ordinary slot whose
    /// [`Repr`] is [`Repr::Addr`].
    AddrOfSlot { dst: Slot, slot: Slot },
    /// `dst = &obj.payload[at]`
    ///
    /// The lowering keeps `obj` in a live reference slot for exactly the
    /// address's live range, and clears that slot with [`Inst::Clear`] when
    /// the address dies — not unconditionally for the rest of the frame,
    /// which would retain the object across everything a long-running body
    /// does afterwards. The collector therefore needs no interior-pointer
    /// logic, and the heap does not move, so the address stays correct
    /// across a collection for as long as it is live and no longer.
    AddrOfWord { dst: Slot, obj: Slot, at: u32 },
    /// `dst = &obj[index]`
    AddrOfElem { dst: Slot, obj: Slot, index: Slot },
    /// `dst = *addr`
    Load { dst: Slot, addr: Slot },
    /// `*addr = src`
    Store { addr: Slot, src: Slot },

    // ---- erasure ----------------------------------------------------------
    /// `dst = <a box holding `src`, tagged `repr`>`
    ///
    /// What a value becomes when its static type is not known: `dyn Trait`,
    /// a Host result a schema declared `Any`, an expression the checker
    /// declined to type. One word in the slot either way.
    Box { dst: Slot, src: Slot, repr: Repr },
    /// `dst = <the word inside the box in `src`>`, trapping if its tag is
    /// not `repr`.
    Unbox { dst: Slot, src: Slot, repr: Repr },

    // ---- failure ----------------------------------------------------------
    /// Fail the run with `message`.
    ///
    /// This is what an `assert` that did not hold, an exhausted `match` and
    /// a failed `Unbox` reach. It is not a refusal to run the program: the
    /// program ran, and this is what it did.
    Trap { message: StrId },
}
