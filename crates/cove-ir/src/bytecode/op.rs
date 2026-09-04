//! The hundred opcodes, and what each one makes of the four fields.
//!
//! # One opcode per concrete operation
//!
//! [`Inst`](crate::Inst)'s own doc argues that *"the instruction set describes
//! families, not cases"* — one `Arith`, not one per numeric type — and that
//! argument is about the *language* growing a concept. The bytecode grows
//! none by enumerating members that already exist, and it removes a nested
//! dispatch by doing so. ADR 0041 decides the enumeration:
//!
//! - [`Inst::Arith`](crate::Inst::Arith) becomes ten, `Num` × `ArithOp`;
//! - [`Inst::Cmp`](crate::Inst::Cmp) becomes thirty, `Compare` × `CmpOp`;
//! - [`Inst::ArithImm`](crate::Inst::ArithImm) five and
//!   [`Inst::CmpImm`](crate::Inst::CmpImm) six, the operator alone;
//! - [`Inst::Neg`](crate::Inst::Neg) two, [`Convert`] two;
//! - [`Inst::Alloc`](crate::Inst::Alloc) three, one per [`Len`](crate::Len)
//!   form, so no discriminant is stored anywhere.
//!
//! # The cross products are generated, not hand-picked
//!
//! [`Op::all`] is the enumeration and an opcode *number is a position in it*.
//! Nothing here lists a hundred numbers, and nothing lists which operator
//! pairs with which comparison: `crate::verify` already constrains which
//! `Repr`s a `Compare` admits and does not constrain the pairing, so a
//! hand-picked table would be a second and weaker copy of the type rules,
//! living in the encoder. An opcode the lowering never emits costs one number
//! out of 256 and one row of a generated table; a rule about which pairs are
//! legal costs a place for two copies to disagree.
//!
//! [`Op::number`] computes the same number by arithmetic, so that it is not a
//! search, and [`Op::from_number`] inverts it through a table built from
//! [`Op::all`] — one direction derived from the other rather than two lists
//! to keep in step. The tests below pin all three against each other.

use std::sync::LazyLock;

use crate::inst::{ArithOp, CmpOp, Compare, Convert, Num};
use crate::repr::Repr;

/// Every [`Num`], in opcode order.
const NUMS: [Num; 2] = [Num::Int, Num::Float];
/// Every [`ArithOp`], in opcode order.
const ARITH_OPS: [ArithOp; 5] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Rem,
];
/// Every [`CmpOp`], in opcode order.
const CMP_OPS: [CmpOp; 6] = [
    CmpOp::Eq,
    CmpOp::Ne,
    CmpOp::Lt,
    CmpOp::Le,
    CmpOp::Gt,
    CmpOp::Ge,
];
/// Every [`Compare`], in opcode order.
const COMPARES: [Compare; 5] = [
    Compare::Int,
    Compare::Float,
    Compare::Bool,
    Compare::Str,
    Compare::Identity,
];
/// Every [`Convert`], in opcode order.
const CONVERTS: [Convert; 2] = [Convert::IntToFloat, Convert::FloatToInt];

/// Where each family's opcodes begin.
///
/// Each base is the one before it plus that family's size, so no number is
/// written down twice and inserting a family renumbers the ones after it —
/// which ADR 0041 permits, because opcode numbers are explicitly not stable.
mod base {
    use super::{ARITH_OPS, CMP_OPS, COMPARES, CONVERTS, NUMS};

    pub const CONST_UNIT: u8 = 0;
    pub const CONST_BOOL: u8 = CONST_UNIT + 1;
    pub const CONST_INT: u8 = CONST_BOOL + 1;
    pub const CONST_FLOAT: u8 = CONST_INT + 1;
    pub const STR: u8 = CONST_FLOAT + 1;
    pub const COPY: u8 = STR + 1;
    pub const CLEAR: u8 = COPY + 1;
    pub const NEG: u8 = CLEAR + 1;
    pub const ARITH: u8 = NEG + NUMS.len() as u8;
    pub const CMP: u8 = ARITH + (NUMS.len() * ARITH_OPS.len()) as u8;
    pub const ARITH_IMM: u8 = CMP + (COMPARES.len() * CMP_OPS.len()) as u8;
    pub const CMP_IMM: u8 = ARITH_IMM + ARITH_OPS.len() as u8;
    pub const NOT: u8 = CMP_IMM + CMP_OPS.len() as u8;
    pub const CONVERT: u8 = NOT + 1;
    pub const JUMP: u8 = CONVERT + CONVERTS.len() as u8;
    pub const BRANCH_FALSE: u8 = JUMP + 1;
    pub const SWITCH: u8 = BRANCH_FALSE + 1;
    pub const RETURN: u8 = SWITCH + 1;
    pub const CALL: u8 = RETURN + 1;
    pub const CALL_CLOSURE: u8 = CALL + 1;
    pub const CALL_HOST: u8 = CALL_CLOSURE + 1;
    pub const CALL_RESOURCE: u8 = CALL_HOST + 1;
    pub const CALL_BUILTIN: u8 = CALL_RESOURCE + 1;
    pub const ALLOC_FIXED: u8 = CALL_BUILTIN + 1;
    pub const ALLOC_IMM: u8 = ALLOC_FIXED + 1;
    pub const ALLOC_SLOT: u8 = ALLOC_IMM + 1;
    pub const LOAD_FIELD: u8 = ALLOC_SLOT + 1;
    pub const STORE_FIELD: u8 = LOAD_FIELD + 1;
    pub const LOAD_ELEM: u8 = STORE_FIELD + 1;
    pub const STORE_ELEM: u8 = LOAD_ELEM + 1;
    pub const LEN: u8 = STORE_ELEM + 1;
    pub const LAYOUT_OF: u8 = LEN + 1;
    pub const ADDR_OF_SLOT: u8 = LAYOUT_OF + 1;
    pub const ADDR_OF_FIELD: u8 = ADDR_OF_SLOT + 1;
    pub const ADDR_OF_ELEM: u8 = ADDR_OF_FIELD + 1;
    pub const ADDR_OF_PART: u8 = ADDR_OF_ELEM + 1;
    pub const LOAD: u8 = ADDR_OF_PART + 1;
    pub const STORE: u8 = LOAD + 1;
    pub const BOX: u8 = STORE + 1;
    pub const UNBOX: u8 = BOX + 1;
    pub const SCOPE_ENTER: u8 = UNBOX + 1;
    pub const SCOPE_LEAVE: u8 = SCOPE_ENTER + 1;
    pub const SCOPE_CANCEL: u8 = SCOPE_LEAVE + 1;
    pub const SPAWN: u8 = SCOPE_CANCEL + 1;
    pub const AWAIT: u8 = SPAWN + 1;
    pub const CANCEL: u8 = AWAIT + 1;
    pub const SETTLED: u8 = CANCEL + 1;
    pub const SHARED_LOCK: u8 = SETTLED + 1;
    pub const SHARED_UNLOCK: u8 = SHARED_LOCK + 1;
    pub const TRAP: u8 = SHARED_UNLOCK + 1;
    pub const ASSERT_FAILED: u8 = TRAP + 1;
    /// One past the last, which is how many opcodes there are.
    pub const END: u8 = ASSERT_FAILED + 1;
}

/// How many opcodes are defined, out of the 256 an opcode byte can name.
pub const OPCODES: usize = base::END as usize;

/// One concrete operation.
///
/// The parameterised variants are the cross products ADR 0041 generates: an
/// `Op::Arith(Num::Int, ArithOp::Add)` *is* `add.int`, and there is no second
/// name for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    ConstUnit,
    ConstBool,
    ConstInt,
    ConstFloat,
    Str,
    Copy,
    Clear,
    Neg(Num),
    Arith(Num, ArithOp),
    Cmp(Compare, CmpOp),
    ArithImm(ArithOp),
    CmpImm(CmpOp),
    Not,
    Convert(Convert),
    Jump,
    BranchFalse,
    Switch,
    Return,
    Call,
    CallClosure,
    CallHost,
    CallResource,
    CallBuiltin,
    AllocFixed,
    AllocImm,
    AllocSlot,
    LoadField,
    StoreField,
    LoadElem,
    StoreElem,
    Len,
    LayoutOf,
    AddrOfSlot,
    AddrOfField,
    AddrOfElem,
    AddrOfPart,
    Load,
    Store,
    Box,
    Unbox,
    ScopeEnter,
    ScopeLeave,
    ScopeCancel,
    Spawn,
    Await,
    Cancel,
    Settled,
    SharedLock,
    SharedUnlock,
    Trap,
    AssertFailed,
}

/// Which of `a`, `b` and `c` an opcode uses, and for what.
///
/// This is the table ADR 0041 calls the format's central saving: *"every slot
/// is inside the function frame"* is not forty-nine rules but one rule over
/// three fields, driven by which of the three an opcode declares live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    /// The opcode does not use this field, and it must be zero.
    ///
    /// Requiring the zero is what makes the encoding *canonical*: two
    /// encodings of one program are byte-identical, and `encode(decode(b))`
    /// is `b`.
    Unused,
    /// One word of the frame, holding one of these [`Repr`]s.
    ///
    /// [`ANY`] is the empty list and means the opcode constrains nothing —
    /// the destination of a closure call, whose width nothing static knows,
    /// and the operand of an `addr.slot`, which is any location at all.
    Word(&'static [Repr]),
    /// The first slot of a value location whose width comes from the layout
    /// in the payload.
    ///
    /// The check is the `fits` one `crate::verify` makes: `slot + width` must
    /// be inside the frame, because a run of words copied off the top of a
    /// frame reads or writes the frame above it.
    Value,
}

/// An operand whose `Repr` the opcode does not constrain. See
/// [`Operand::Word`].
pub const ANY: &[Repr] = &[];

/// A numeric operand: a `Duration` is nanoseconds and adds like an integer,
/// which is `crate::verify`'s rule kept word for word.
const INT: &[Repr] = &[Repr::Int, Repr::Duration];
const FLOAT: &[Repr] = &[Repr::Float];
const BOOL: &[Repr] = &[Repr::Bool];
const UNIT: &[Repr] = &[Repr::Unit];
const REF: &[Repr] = &[Repr::Ref];
const ADDR: &[Repr] = &[Repr::Addr];
const HOST: &[Repr] = &[Repr::Host];
const TASK: &[Repr] = &[Repr::Task];
const SCOPE: &[Repr] = &[Repr::Scope];

/// What the payload's eight bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    /// Nothing, and all eight bytes must be zero.
    Empty,
    /// `0` or `1`, and nothing else.
    Bool,
    /// All sixty-four bits, as the instruction's own immediate: an `i64`
    /// value, or the bits of an `f64`.
    Imm,
    /// `to - (pc + 1)`, two's complement, so a branch is relative.
    Displacement,
    /// Two 32-bit halves, low first.
    Halves(Half, Half),
}

/// What one 32-bit half of a payload holds.
///
/// Every id keeps its full 32 bits: ADR 0041 narrows slot operands and
/// nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    /// Unused, and must be zero.
    Unused,
    Function,
    Str,
    Layout,
    Table,
    Args,
    Builtin,
    HostOp,
    /// An element count: `Len::Count`'s `n`.
    Count,
    /// A word offset into an object or into the value an address names.
    Offset,
}

impl Half {
    /// What a fault calls this, and what a table it indexes is called.
    pub fn name(self) -> &'static str {
        match self {
            Half::Unused => "unused",
            Half::Function => "function",
            Half::Str => "string",
            Half::Layout => "layout",
            Half::Table => "table",
            Half::Args => "argument list",
            Half::Builtin => "builtin",
            Half::HostOp => "host op",
            Half::Count => "count",
            Half::Offset => "offset",
        }
    }
}

/// What an opcode makes of the four fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields {
    pub a: Operand,
    pub b: Operand,
    pub c: Operand,
    pub payload: Payload,
}

impl Fields {
    /// `a`, `b` and `c` in order, so that one loop is the whole slot check.
    pub fn operands(&self) -> [Operand; 3] {
        [self.a, self.b, self.c]
    }
}

/// Builds a [`Fields`], filling in the fields an opcode leaves alone.
fn fields(a: Operand, b: Operand, c: Operand, payload: Payload) -> Fields {
    Fields { a, b, c, payload }
}

/// Nothing at all: the field is unused and must be zero.
const NONE: Operand = Operand::Unused;

/// The position of `wanted` in `held`, as an opcode offset.
fn index_of<T: PartialEq>(held: &[T], wanted: T) -> u8 {
    held.iter()
        .position(|one| *one == wanted)
        .expect("every member of the enum is in the table it is enumerated by") as u8
}

impl Op {
    /// Every opcode, in the order that gives them their numbers.
    ///
    /// This is the generated table ADR 0041 asks for. A family with members
    /// contributes its cross product, in the order of the arrays above.
    pub fn all() -> Vec<Op> {
        let mut all = vec![
            Op::ConstUnit,
            Op::ConstBool,
            Op::ConstInt,
            Op::ConstFloat,
            Op::Str,
            Op::Copy,
            Op::Clear,
        ];
        all.extend(NUMS.map(Op::Neg));
        for num in NUMS {
            for op in ARITH_OPS {
                all.push(Op::Arith(num, op));
            }
        }
        for on in COMPARES {
            for op in CMP_OPS {
                all.push(Op::Cmp(on, op));
            }
        }
        all.extend(ARITH_OPS.map(Op::ArithImm));
        all.extend(CMP_OPS.map(Op::CmpImm));
        all.push(Op::Not);
        all.extend(CONVERTS.map(Op::Convert));
        all.extend([
            Op::Jump,
            Op::BranchFalse,
            Op::Switch,
            Op::Return,
            Op::Call,
            Op::CallClosure,
            Op::CallHost,
            Op::CallResource,
            Op::CallBuiltin,
            Op::AllocFixed,
            Op::AllocImm,
            Op::AllocSlot,
            Op::LoadField,
            Op::StoreField,
            Op::LoadElem,
            Op::StoreElem,
            Op::Len,
            Op::LayoutOf,
            Op::AddrOfSlot,
            Op::AddrOfField,
            Op::AddrOfElem,
            Op::AddrOfPart,
            Op::Load,
            Op::Store,
            Op::Box,
            Op::Unbox,
            Op::ScopeEnter,
            Op::ScopeLeave,
            Op::ScopeCancel,
            Op::Spawn,
            Op::Await,
            Op::Cancel,
            Op::Settled,
            Op::SharedLock,
            Op::SharedUnlock,
            Op::Trap,
            Op::AssertFailed,
        ]);
        all
    }

    /// The byte this opcode is written as.
    pub fn number(self) -> u8 {
        match self {
            Op::ConstUnit => base::CONST_UNIT,
            Op::ConstBool => base::CONST_BOOL,
            Op::ConstInt => base::CONST_INT,
            Op::ConstFloat => base::CONST_FLOAT,
            Op::Str => base::STR,
            Op::Copy => base::COPY,
            Op::Clear => base::CLEAR,
            Op::Neg(num) => base::NEG + index_of(&NUMS, num),
            Op::Arith(num, op) => {
                base::ARITH
                    + index_of(&NUMS, num) * ARITH_OPS.len() as u8
                    + index_of(&ARITH_OPS, op)
            }
            Op::Cmp(on, op) => {
                base::CMP + index_of(&COMPARES, on) * CMP_OPS.len() as u8 + index_of(&CMP_OPS, op)
            }
            Op::ArithImm(op) => base::ARITH_IMM + index_of(&ARITH_OPS, op),
            Op::CmpImm(op) => base::CMP_IMM + index_of(&CMP_OPS, op),
            Op::Not => base::NOT,
            Op::Convert(to) => base::CONVERT + index_of(&CONVERTS, to),
            Op::Jump => base::JUMP,
            Op::BranchFalse => base::BRANCH_FALSE,
            Op::Switch => base::SWITCH,
            Op::Return => base::RETURN,
            Op::Call => base::CALL,
            Op::CallClosure => base::CALL_CLOSURE,
            Op::CallHost => base::CALL_HOST,
            Op::CallResource => base::CALL_RESOURCE,
            Op::CallBuiltin => base::CALL_BUILTIN,
            Op::AllocFixed => base::ALLOC_FIXED,
            Op::AllocImm => base::ALLOC_IMM,
            Op::AllocSlot => base::ALLOC_SLOT,
            Op::LoadField => base::LOAD_FIELD,
            Op::StoreField => base::STORE_FIELD,
            Op::LoadElem => base::LOAD_ELEM,
            Op::StoreElem => base::STORE_ELEM,
            Op::Len => base::LEN,
            Op::LayoutOf => base::LAYOUT_OF,
            Op::AddrOfSlot => base::ADDR_OF_SLOT,
            Op::AddrOfField => base::ADDR_OF_FIELD,
            Op::AddrOfElem => base::ADDR_OF_ELEM,
            Op::AddrOfPart => base::ADDR_OF_PART,
            Op::Load => base::LOAD,
            Op::Store => base::STORE,
            Op::Box => base::BOX,
            Op::Unbox => base::UNBOX,
            Op::ScopeEnter => base::SCOPE_ENTER,
            Op::ScopeLeave => base::SCOPE_LEAVE,
            Op::ScopeCancel => base::SCOPE_CANCEL,
            Op::Spawn => base::SPAWN,
            Op::Await => base::AWAIT,
            Op::Cancel => base::CANCEL,
            Op::Settled => base::SETTLED,
            Op::SharedLock => base::SHARED_LOCK,
            Op::SharedUnlock => base::SHARED_UNLOCK,
            Op::Trap => base::TRAP,
            Op::AssertFailed => base::ASSERT_FAILED,
        }
    }

    /// Which opcode a byte names, or `None` for one no encoder produced.
    ///
    /// The inverse is a table built from [`Op::all`] rather than a second
    /// match, so there is one enumeration and not two.
    pub fn from_number(number: u8) -> Option<Op> {
        static BY_NUMBER: LazyLock<[Option<Op>; 256]> = LazyLock::new(|| {
            let mut table = [None; 256];
            for op in Op::all() {
                table[op.number() as usize] = Some(op);
            }
            table
        });
        BY_NUMBER[number as usize]
    }

    /// What this opcode makes of the four fields.
    ///
    /// This is ADR 0041's audit table, one row per opcode, and it is what
    /// both the decoder's canonicality check and the verifier's slot check
    /// are driven by.
    pub fn fields(self) -> Fields {
        let ids = |lo: Half, hi: Half| Payload::Halves(lo, hi);
        let one = |lo: Half| Payload::Halves(lo, Half::Unused);
        match self {
            Op::ConstUnit => fields(Operand::Word(UNIT), NONE, NONE, Payload::Empty),
            Op::ConstBool => fields(Operand::Word(BOOL), NONE, NONE, Payload::Bool),
            Op::ConstInt => fields(Operand::Word(INT), NONE, NONE, Payload::Imm),
            Op::ConstFloat => fields(Operand::Word(FLOAT), NONE, NONE, Payload::Imm),
            Op::Str => fields(Operand::Word(REF), NONE, NONE, one(Half::Str)),
            Op::Copy => fields(Operand::Value, Operand::Value, NONE, one(Half::Layout)),
            Op::Clear => fields(Operand::Value, NONE, NONE, one(Half::Layout)),
            Op::Neg(num) => {
                let want = numeric(num);
                fields(
                    Operand::Word(want),
                    Operand::Word(want),
                    NONE,
                    Payload::Empty,
                )
            }
            Op::Arith(num, _) => {
                let want = numeric(num);
                fields(
                    Operand::Word(want),
                    Operand::Word(want),
                    Operand::Word(want),
                    Payload::Empty,
                )
            }
            Op::Cmp(on, _) => {
                let want = compared(on);
                fields(
                    Operand::Word(BOOL),
                    Operand::Word(want),
                    Operand::Word(want),
                    Payload::Empty,
                )
            }
            Op::ArithImm(_) => fields(Operand::Word(INT), Operand::Word(INT), NONE, Payload::Imm),
            Op::CmpImm(_) => fields(Operand::Word(BOOL), Operand::Word(INT), NONE, Payload::Imm),
            Op::Not => fields(
                Operand::Word(BOOL),
                Operand::Word(BOOL),
                NONE,
                Payload::Empty,
            ),
            Op::Convert(to) => {
                let (from, into) = match to {
                    Convert::IntToFloat => (INT, FLOAT),
                    Convert::FloatToInt => (FLOAT, INT),
                };
                fields(
                    Operand::Word(into),
                    Operand::Word(from),
                    NONE,
                    Payload::Empty,
                )
            }
            Op::Jump => fields(NONE, NONE, NONE, Payload::Displacement),
            Op::BranchFalse => fields(Operand::Word(BOOL), NONE, NONE, Payload::Displacement),
            // The discriminant of an enum location is its first word and is
            // an `Int`; so is the layout id a `dyn` dispatch switches on.
            Op::Switch => fields(Operand::Word(INT), NONE, NONE, one(Half::Table)),
            // `src` is a value location of `Function::returns`, which is not
            // in the instruction: the width check is the verifier's, from the
            // function being checked.
            Op::Return => fields(Operand::Word(ANY), NONE, NONE, Payload::Empty),
            // A call's destination is a value location too, at the *callee's*
            // `returns`. Same reason, same place.
            Op::Call => fields(
                Operand::Word(ANY),
                NONE,
                NONE,
                ids(Half::Function, Half::Args),
            ),
            Op::CallClosure => fields(
                Operand::Word(ANY),
                Operand::Word(REF),
                NONE,
                one(Half::Args),
            ),
            Op::CallHost => fields(
                Operand::Word(ANY),
                NONE,
                NONE,
                ids(Half::HostOp, Half::Args),
            ),
            Op::CallResource => fields(
                Operand::Word(ANY),
                Operand::Word(HOST),
                NONE,
                ids(Half::HostOp, Half::Args),
            ),
            Op::CallBuiltin => fields(
                Operand::Word(ANY),
                NONE,
                NONE,
                ids(Half::Builtin, Half::Args),
            ),
            Op::AllocFixed => fields(Operand::Word(REF), NONE, NONE, one(Half::Layout)),
            Op::AllocImm => fields(
                Operand::Word(REF),
                NONE,
                NONE,
                ids(Half::Layout, Half::Count),
            ),
            Op::AllocSlot => fields(
                Operand::Word(REF),
                Operand::Word(INT),
                NONE,
                one(Half::Layout),
            ),
            Op::LoadField => fields(
                Operand::Value,
                Operand::Word(REF),
                NONE,
                ids(Half::Offset, Half::Layout),
            ),
            Op::StoreField => fields(
                Operand::Word(REF),
                Operand::Value,
                NONE,
                ids(Half::Offset, Half::Layout),
            ),
            Op::LoadElem => fields(
                Operand::Value,
                Operand::Word(REF),
                Operand::Word(INT),
                one(Half::Layout),
            ),
            Op::StoreElem => fields(
                Operand::Word(REF),
                Operand::Word(INT),
                Operand::Value,
                one(Half::Layout),
            ),
            Op::Len => fields(Operand::Word(INT), Operand::Word(REF), NONE, Payload::Empty),
            Op::LayoutOf => fields(Operand::Word(INT), Operand::Word(REF), NONE, Payload::Empty),
            Op::AddrOfSlot => fields(
                Operand::Word(ADDR),
                Operand::Word(ANY),
                NONE,
                Payload::Empty,
            ),
            Op::AddrOfField => fields(
                Operand::Word(ADDR),
                Operand::Word(REF),
                NONE,
                one(Half::Offset),
            ),
            Op::AddrOfElem => fields(
                Operand::Word(ADDR),
                Operand::Word(REF),
                Operand::Word(INT),
                one(Half::Layout),
            ),
            // Nothing bounds `at` against the value the address names, and
            // that gap is inherited rather than introduced: a frame records
            // no value's extent. `crate::verify` says the same, for the same
            // reason, and `at` keeps its full 32 bits.
            Op::AddrOfPart => fields(
                Operand::Word(ADDR),
                Operand::Word(ADDR),
                NONE,
                one(Half::Offset),
            ),
            Op::Load => fields(Operand::Value, Operand::Word(ADDR), NONE, one(Half::Layout)),
            Op::Store => fields(Operand::Word(ADDR), Operand::Value, NONE, one(Half::Layout)),
            Op::Box => fields(Operand::Word(REF), Operand::Value, NONE, one(Half::Layout)),
            Op::Unbox => fields(Operand::Value, Operand::Word(REF), NONE, one(Half::Layout)),
            Op::ScopeEnter => fields(Operand::Word(SCOPE), NONE, NONE, one(Half::Str)),
            Op::ScopeLeave => fields(
                Operand::Word(SCOPE),
                Operand::Word(BOOL),
                Operand::Value,
                one(Half::Layout),
            ),
            Op::ScopeCancel => fields(Operand::Word(SCOPE), NONE, NONE, Payload::Empty),
            // The answer's layout is what the machine allocates an object of,
            // not a location in this frame, so all three fields are one word.
            Op::Spawn => fields(
                Operand::Word(TASK),
                Operand::Word(SCOPE),
                Operand::Word(REF),
                one(Half::Layout),
            ),
            Op::Await => fields(Operand::Value, Operand::Word(TASK), NONE, one(Half::Layout)),
            Op::Cancel => fields(Operand::Word(TASK), NONE, NONE, Payload::Empty),
            Op::Settled => fields(Operand::Word(TASK), Operand::Value, NONE, one(Half::Layout)),
            Op::SharedLock => fields(Operand::Word(REF), NONE, NONE, Payload::Empty),
            Op::SharedUnlock => fields(Operand::Word(REF), NONE, NONE, Payload::Empty),
            Op::Trap => fields(NONE, NONE, NONE, one(Half::Str)),
            Op::AssertFailed => fields(Operand::Word(REF), NONE, NONE, Payload::Empty),
        }
    }
}

/// What a numeric operand may hold.
fn numeric(num: Num) -> &'static [Repr] {
    match num {
        Num::Int => INT,
        Num::Float => FLOAT,
    }
}

/// What a comparison's two operands may hold.
fn compared(on: Compare) -> &'static [Repr] {
    match on {
        Compare::Int => INT,
        Compare::Float => FLOAT,
        Compare::Bool => BOOL,
        Compare::Str => REF,
        // `is` compares words, and the only words whose identity is a
        // language-level question are references.
        Compare::Identity => REF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0041's count, which is the one number the format's headroom is
    /// argued from: a hundred opcodes out of the 256 a byte names.
    #[test]
    fn there_are_a_hundred_opcodes() {
        assert_eq!(Op::all().len(), 100);
        assert_eq!(OPCODES, 100);
    }

    /// The numbering *is* the enumeration. `number` computes by arithmetic
    /// what `all` produces by iteration, and a family base that drifted would
    /// make the two disagree here rather than silently in an encoding.
    #[test]
    fn an_opcode_number_is_its_position_in_the_generated_table() {
        for (at, op) in Op::all().into_iter().enumerate() {
            assert_eq!(op.number() as usize, at, "{op:?}");
        }
    }

    /// Two opcodes with one number would make decoding a guess.
    #[test]
    fn no_two_opcodes_share_a_number() {
        let mut seen = vec![None; 256];
        for op in Op::all() {
            let at = op.number() as usize;
            assert_eq!(seen[at], None, "{op:?} and {:?} share {at}", seen[at]);
            seen[at] = Some(op);
        }
    }

    /// `from_number` is a genuine inverse over the defined numbers, and
    /// answers `None` over every other byte — which is what makes an unknown
    /// opcode a refusal rather than an index into a table.
    #[test]
    fn every_byte_either_names_one_opcode_or_none_at_all() {
        for byte in 0u8..=255 {
            match Op::from_number(byte) {
                Some(op) => {
                    assert!((byte as usize) < OPCODES);
                    assert_eq!(op.number(), byte);
                }
                None => assert!((byte as usize) >= OPCODES, "{byte} names nothing"),
            }
        }
    }

    /// The cross products are the whole of the arithmetic families, so
    /// `add.int` and `add.float` are two numbers and `Num` is not read at run
    /// time.
    #[test]
    fn the_arithmetic_families_are_the_cross_products_the_adr_gives() {
        let all = Op::all();
        let count = |f: fn(&Op) -> bool| all.iter().filter(|op| f(op)).count();
        assert_eq!(count(|op| matches!(op, Op::Arith(_, _))), 10);
        assert_eq!(count(|op| matches!(op, Op::Cmp(_, _))), 30);
        assert_eq!(count(|op| matches!(op, Op::ArithImm(_))), 5);
        assert_eq!(count(|op| matches!(op, Op::CmpImm(_))), 6);
        assert_eq!(count(|op| matches!(op, Op::Neg(_))), 2);
        assert_eq!(count(|op| matches!(op, Op::Convert(_))), 2);
        assert_eq!(
            count(|op| matches!(op, Op::AllocFixed | Op::AllocImm | Op::AllocSlot)),
            3
        );
    }

    /// No opcode names more than three slots and none carries more than the
    /// payload, which is the invariant the whole sixteen-byte decision rests
    /// on. A `Fields` that used a fourth field could not be written down, and
    /// this is what says the table never wanted to.
    #[test]
    fn no_opcode_uses_a_field_the_format_does_not_have() {
        for op in Op::all() {
            let used = op.fields();
            // Once a field is unused the ones after it are too: the encoder
            // fills `a`, then `b`, then `c`, and a hole would make the table
            // ambiguous to read.
            let live: Vec<bool> = used
                .operands()
                .iter()
                .map(|one| *one != Operand::Unused)
                .collect();
            assert!(
                live.windows(2).all(|pair| pair[0] || !pair[1]),
                "{op:?} leaves a hole in a, b, c"
            );
        }
    }

    /// A `Value` operand's width comes from a layout, so an opcode that
    /// declares one must carry a layout to read it from.
    #[test]
    fn a_value_operand_always_has_a_layout_in_the_payload() {
        for op in Op::all() {
            if !op.fields().operands().contains(&Operand::Value) {
                continue;
            }
            let held = match op.fields().payload {
                Payload::Halves(lo, hi) => [lo, hi],
                other => panic!("{op:?} has a value operand and a {other:?} payload"),
            };
            assert!(
                held.contains(&Half::Layout),
                "{op:?} has a value operand and names no layout"
            );
        }
    }
}
