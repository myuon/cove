//! `Inst` to sixteen bytes.
//!
//! The encoder is **total**: ADR 0041's audit covers all forty-nine variants,
//! so there is no instruction it can refuse and no fallback to enum execution
//! to design around. The one thing it can answer `Err` about is an operand
//! that does not fit — a slot past 65,535, which the compiler's own frame
//! limit already refuses at the declaration, and a branch displacement that
//! overflows, which no pair of `u32` program counters can produce. Both are
//! checked anyway, because *"the encoder rejects overflow"* should be a line
//! of code rather than an argument.
//!
//! It is **deterministic**: encoding is a pure function of the instruction and
//! its program counter, every field an opcode does not use is written zero,
//! and two encodings of one program are byte-identical.

use crate::inst::{Inst, Len, Pc, Slot};
use crate::program::{Function, Program};

use super::op::Op;
use super::EncodedInst;

/// An operand that does not fit the field ADR 0041 gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TooWide {
    /// A slot a sixteen-bit operand cannot name.
    ///
    /// `crate::lower` refuses a frame of more than
    /// [`MAX_FRAME_WORDS`](super::MAX_FRAME_WORDS) with a diagnostic at the
    /// declaration, so a program that reached the encoder cannot hold one.
    /// This is the assertion that says so.
    Slot { slot: Slot },
    /// A branch whose displacement is not an `i64`.
    ///
    /// Unreachable while [`Pc`] is a `u32`: every representable pair of
    /// program counters has a representable difference. It is checked so that
    /// a wider `Pc` is a refusal here rather than a wrong jump somewhere else.
    Displacement { from: Pc, to: Pc },
}

impl std::fmt::Display for TooWide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TooWide::Slot { slot } => write!(
                f,
                "slot {slot} cannot be encoded: a slot operand is sixteen bits, so the largest \
                 is {}",
                u16::MAX
            ),
            TooWide::Displacement { from, to } => {
                write!(f, "a branch from {from} to {to} has no displacement")
            }
        }
    }
}

/// A whole program's instructions, encoded, one run per function.
///
/// Parallel to [`Program::functions`], because a 1:1 encoding keeps a
/// function's program counters exactly as they were: `code[id][pc]` is the
/// encoding of `program.functions[id].code[pc]`, and everything indexed by pc
/// — spans, local ranges, switch targets — goes on meaning what it meant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoded {
    pub functions: Vec<Vec<EncodedInst>>,
}

impl Encoded {
    /// One function's instructions.
    pub fn function(&self, id: crate::FunctionId) -> &[EncodedInst] {
        &self.functions[id.index()]
    }

    /// How many bytes the whole program's code occupies.
    pub fn bytes(&self) -> usize {
        self.functions
            .iter()
            .map(|code| code.len() * EncodedInst::BYTES)
            .sum()
    }
}

/// Encodes every function of a program.
pub fn encode_program(program: &Program) -> Result<Encoded, TooWide> {
    let mut functions = Vec::with_capacity(program.functions.len());
    for function in &program.functions {
        functions.push(encode_function(function)?);
    }
    Ok(Encoded { functions })
}

/// Encodes one function's code.
pub fn encode_function(function: &Function) -> Result<Vec<EncodedInst>, TooWide> {
    function
        .code
        .iter()
        .enumerate()
        .map(|(pc, inst)| encode(inst, pc as Pc))
        .collect()
}

/// Encodes one instruction.
///
/// `pc` is where the instruction sits, and only a branch reads it: ADR 0041
/// makes [`Inst::Jump`] and [`Inst::BranchFalse`] carry `to - (pc + 1)`, so
/// that a target is a displacement rather than an absolute address.
pub fn encode(inst: &Inst, pc: Pc) -> Result<EncodedInst, TooWide> {
    let build = |op: Op, a: u16, b: u16, c: u16, payload: u64| {
        EncodedInst::new(op.number(), a, b, c, payload)
    };
    Ok(match *inst {
        // ---- constants and moves ------------------------------------------
        Inst::Unit { dst } => build(Op::ConstUnit, slot(dst)?, 0, 0, 0),
        Inst::Bool { dst, value } => build(Op::ConstBool, slot(dst)?, 0, 0, u64::from(value)),
        Inst::Int { dst, value } => build(Op::ConstInt, slot(dst)?, 0, 0, value as u64),
        Inst::Float { dst, bits } => build(Op::ConstFloat, slot(dst)?, 0, 0, bits),
        Inst::Str { dst, text } => build(Op::Str, slot(dst)?, 0, 0, halves(text.0, 0)),
        Inst::Copy { dst, src, layout } => {
            build(Op::Copy, slot(dst)?, slot(src)?, 0, halves(layout.0, 0))
        }
        Inst::Clear { slot: at, layout } => build(Op::Clear, slot(at)?, 0, 0, halves(layout.0, 0)),

        // ---- scalar operations --------------------------------------------
        Inst::Neg { num, dst, a } => build(Op::Neg(num), slot(dst)?, slot(a)?, 0, 0),
        Inst::Arith { num, op, dst, a, b } => {
            build(Op::Arith(num, op), slot(dst)?, slot(a)?, slot(b)?, 0)
        }
        Inst::Cmp { on, op, dst, a, b } => {
            build(Op::Cmp(on, op), slot(dst)?, slot(a)?, slot(b)?, 0)
        }
        Inst::ArithImm { op, dst, a, value } => {
            build(Op::ArithImm(op), slot(dst)?, slot(a)?, 0, value as u64)
        }
        Inst::CmpImm { op, dst, a, value } => {
            build(Op::CmpImm(op), slot(dst)?, slot(a)?, 0, value as u64)
        }
        Inst::Not { dst, a } => build(Op::Not, slot(dst)?, slot(a)?, 0, 0),
        Inst::Convert { to, dst, a } => build(Op::Convert(to), slot(dst)?, slot(a)?, 0, 0),

        // ---- control flow --------------------------------------------------
        Inst::Jump { to } => build(Op::Jump, 0, 0, 0, displacement(pc, to)? as u64),
        Inst::BranchFalse { cond, to } => build(
            Op::BranchFalse,
            slot(cond)?,
            0,
            0,
            displacement(pc, to)? as u64,
        ),
        Inst::Switch { on, table } => build(Op::Switch, slot(on)?, 0, 0, halves(table.0, 0)),
        Inst::Return { src } => build(Op::Return, slot(src)?, 0, 0, 0),

        // ---- calls ----------------------------------------------------------
        Inst::Call { dst, callee, args } => {
            build(Op::Call, slot(dst)?, 0, 0, halves(callee.0, args.0))
        }
        Inst::CallClosure { dst, closure, args } => build(
            Op::CallClosure,
            slot(dst)?,
            slot(closure)?,
            0,
            halves(args.0, 0),
        ),
        Inst::CallHost { dst, op, args } => {
            build(Op::CallHost, slot(dst)?, 0, 0, halves(op.0, args.0))
        }
        Inst::CallResource {
            dst,
            receiver,
            op,
            args,
        } => build(
            Op::CallResource,
            slot(dst)?,
            slot(receiver)?,
            0,
            halves(op.0, args.0),
        ),
        Inst::CallBuiltin { dst, builtin, args } => {
            build(Op::CallBuiltin, slot(dst)?, 0, 0, halves(builtin.0, args.0))
        }

        // ---- the heap --------------------------------------------------------
        // Three opcodes rather than a discriminant in a field: `Len`'s three
        // forms are three encodings, and nothing stores which one it is.
        Inst::Alloc { dst, layout, len } => match len {
            Len::Fixed => build(Op::AllocFixed, slot(dst)?, 0, 0, halves(layout.0, 0)),
            Len::Count(n) => build(Op::AllocImm, slot(dst)?, 0, 0, halves(layout.0, n)),
            Len::Slot(at) => build(Op::AllocSlot, slot(dst)?, slot(at)?, 0, halves(layout.0, 0)),
        },
        Inst::LoadField {
            dst,
            obj,
            at,
            layout,
        } => build(
            Op::LoadField,
            slot(dst)?,
            slot(obj)?,
            0,
            halves(at, layout.0),
        ),
        Inst::StoreField {
            obj,
            at,
            src,
            layout,
        } => build(
            Op::StoreField,
            slot(obj)?,
            slot(src)?,
            0,
            halves(at, layout.0),
        ),
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => build(
            Op::LoadElem,
            slot(dst)?,
            slot(obj)?,
            slot(index)?,
            halves(layout.0, 0),
        ),
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } => build(
            Op::StoreElem,
            slot(obj)?,
            slot(index)?,
            slot(src)?,
            halves(layout.0, 0),
        ),
        Inst::Len { dst, obj } => build(Op::Len, slot(dst)?, slot(obj)?, 0, 0),
        Inst::LayoutOf { dst, obj } => build(Op::LayoutOf, slot(dst)?, slot(obj)?, 0, 0),

        // ---- places ----------------------------------------------------------
        Inst::AddrOfSlot { dst, slot: at } => build(Op::AddrOfSlot, slot(dst)?, slot(at)?, 0, 0),
        Inst::AddrOfField { dst, obj, at } => {
            build(Op::AddrOfField, slot(dst)?, slot(obj)?, 0, halves(at, 0))
        }
        Inst::AddrOfElem {
            dst,
            obj,
            index,
            layout,
        } => build(
            Op::AddrOfElem,
            slot(dst)?,
            slot(obj)?,
            slot(index)?,
            halves(layout.0, 0),
        ),
        Inst::AddrOfPart { dst, addr, at } => {
            build(Op::AddrOfPart, slot(dst)?, slot(addr)?, 0, halves(at, 0))
        }
        Inst::Load { dst, addr, layout } => {
            build(Op::Load, slot(dst)?, slot(addr)?, 0, halves(layout.0, 0))
        }
        Inst::Store { addr, src, layout } => {
            build(Op::Store, slot(addr)?, slot(src)?, 0, halves(layout.0, 0))
        }

        // ---- erasure ----------------------------------------------------------
        Inst::Box { dst, src, layout } => {
            build(Op::Box, slot(dst)?, slot(src)?, 0, halves(layout.0, 0))
        }
        Inst::Unbox { dst, src, layout } => {
            build(Op::Unbox, slot(dst)?, slot(src)?, 0, halves(layout.0, 0))
        }

        // ---- tasks -------------------------------------------------------------
        Inst::ScopeEnter { dst, name } => {
            build(Op::ScopeEnter, slot(dst)?, 0, 0, halves(name.0, 0))
        }
        Inst::ScopeLeave {
            scope,
            failed,
            error,
            layout,
        } => build(
            Op::ScopeLeave,
            slot(scope)?,
            slot(failed)?,
            slot(error)?,
            halves(layout.0, 0),
        ),
        Inst::ScopeCancel { scope } => build(Op::ScopeCancel, slot(scope)?, 0, 0, 0),
        Inst::Spawn {
            dst,
            scope,
            closure,
            answer,
        } => build(
            Op::Spawn,
            slot(dst)?,
            slot(scope)?,
            slot(closure)?,
            halves(answer.0, 0),
        ),
        Inst::Await { dst, task, answer } => {
            build(Op::Await, slot(dst)?, slot(task)?, 0, halves(answer.0, 0))
        }
        Inst::Cancel { task } => build(Op::Cancel, slot(task)?, 0, 0, 0),
        Inst::Settled { dst, src, answer } => {
            build(Op::Settled, slot(dst)?, slot(src)?, 0, halves(answer.0, 0))
        }

        // ---- cells ---------------------------------------------------------------
        Inst::SharedLock { cell } => build(Op::SharedLock, slot(cell)?, 0, 0, 0),
        Inst::SharedUnlock { cell } => build(Op::SharedUnlock, slot(cell)?, 0, 0, 0),

        // ---- failure ----------------------------------------------------------
        Inst::Trap { message } => build(Op::Trap, 0, 0, 0, halves(message.0, 0)),
        Inst::AssertFailed { message } => build(Op::AssertFailed, slot(message)?, 0, 0, 0),
    })
}

/// A slot as the sixteen bits the format gives it.
fn slot(slot: Slot) -> Result<u16, TooWide> {
    u16::try_from(slot).map_err(|_| TooWide::Slot { slot })
}

/// Two 32-bit halves as one payload, low first.
fn halves(lo: u32, hi: u32) -> u64 {
    u64::from(lo) | (u64::from(hi) << 32)
}

/// `to - (pc + 1)`, which is what a relative branch carries.
fn displacement(from: Pc, to: Pc) -> Result<i64, TooWide> {
    i64::from(to)
        .checked_sub(i64::from(from) + 1)
        .ok_or(TooWide::Displacement { from, to })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::bytecode::decode::decode;
    use crate::bytecode::op::Op;
    use crate::inst::{ArithOp, CmpOp, Compare, Convert, Num};
    use crate::layout::LayoutId;
    use crate::{ArgsId, BuiltinId, FunctionId, HostOpId, StrId, TableId};

    const L: LayoutId = LayoutId(3);

    /// One instruction per opcode, built the way the opcode table is: the
    /// families that are cross products are iterated, not listed.
    ///
    /// This is what makes the round trip structural. A new `Inst` variant is
    /// a compile error in [`encode`], which forces a new [`Op`], which grows
    /// [`Op::all`] — and then
    /// [`every_opcode_is_reached_by_a_sample`](tests::every_opcode_is_reached_by_a_sample)
    /// fails until an instruction that produces it is written here. A list
    /// somebody remembered to extend would catch none of that.
    fn samples() -> Vec<(Pc, Inst)> {
        let mut held = vec![
            (0, Inst::Unit { dst: 1 }),
            (
                0,
                Inst::Bool {
                    dst: 1,
                    value: true,
                },
            ),
            (0, Inst::Int { dst: 1, value: 7 }),
            (
                0,
                Inst::Float {
                    dst: 1,
                    bits: 1.5f64.to_bits(),
                },
            ),
            (
                0,
                Inst::Str {
                    dst: 1,
                    text: StrId(4),
                },
            ),
            (
                0,
                Inst::Copy {
                    dst: 1,
                    src: 2,
                    layout: L,
                },
            ),
            (0, Inst::Clear { slot: 1, layout: L }),
        ];
        for num in [Num::Int, Num::Float] {
            held.push((0, Inst::Neg { num, dst: 1, a: 2 }));
            for op in [
                ArithOp::Add,
                ArithOp::Sub,
                ArithOp::Mul,
                ArithOp::Div,
                ArithOp::Rem,
            ] {
                held.push((
                    0,
                    Inst::Arith {
                        num,
                        op,
                        dst: 1,
                        a: 2,
                        b: 3,
                    },
                ));
            }
        }
        for on in [
            Compare::Int,
            Compare::Float,
            Compare::Bool,
            Compare::Str,
            Compare::Identity,
        ] {
            for op in [
                CmpOp::Eq,
                CmpOp::Ne,
                CmpOp::Lt,
                CmpOp::Le,
                CmpOp::Gt,
                CmpOp::Ge,
            ] {
                held.push((
                    0,
                    Inst::Cmp {
                        on,
                        op,
                        dst: 1,
                        a: 2,
                        b: 3,
                    },
                ));
            }
        }
        for op in [
            ArithOp::Add,
            ArithOp::Sub,
            ArithOp::Mul,
            ArithOp::Div,
            ArithOp::Rem,
        ] {
            held.push((
                0,
                Inst::ArithImm {
                    op,
                    dst: 1,
                    a: 2,
                    value: -9,
                },
            ));
        }
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            held.push((
                0,
                Inst::CmpImm {
                    op,
                    dst: 1,
                    a: 2,
                    value: 11,
                },
            ));
        }
        held.push((0, Inst::Not { dst: 1, a: 2 }));
        for to in [Convert::IntToFloat, Convert::FloatToInt] {
            held.push((0, Inst::Convert { to, dst: 1, a: 2 }));
        }
        held.extend([
            // A forward jump, a backward one, and one to the instruction
            // after this — the displacement zero a fall-through would have.
            (5, Inst::Jump { to: 9 }),
            (9, Inst::Jump { to: 5 }),
            (4, Inst::Jump { to: 5 }),
            (5, Inst::BranchFalse { cond: 1, to: 2 }),
            (
                0,
                Inst::Switch {
                    on: 1,
                    table: TableId(2),
                },
            ),
            (0, Inst::Return { src: 1 }),
            (
                0,
                Inst::Call {
                    dst: 1,
                    callee: FunctionId(2),
                    args: ArgsId(3),
                },
            ),
            (
                0,
                Inst::CallClosure {
                    dst: 1,
                    closure: 2,
                    args: ArgsId(3),
                },
            ),
            (
                0,
                Inst::CallHost {
                    dst: 1,
                    op: HostOpId(2),
                    args: ArgsId(3),
                },
            ),
            (
                0,
                Inst::CallResource {
                    dst: 1,
                    receiver: 2,
                    op: HostOpId(3),
                    args: ArgsId(4),
                },
            ),
            (
                0,
                Inst::CallBuiltin {
                    dst: 1,
                    builtin: BuiltinId(2),
                    args: ArgsId(3),
                },
            ),
            (
                0,
                Inst::Alloc {
                    dst: 1,
                    layout: L,
                    len: Len::Fixed,
                },
            ),
            (
                0,
                Inst::Alloc {
                    dst: 1,
                    layout: L,
                    len: Len::Count(12),
                },
            ),
            (
                0,
                Inst::Alloc {
                    dst: 1,
                    layout: L,
                    len: Len::Slot(2),
                },
            ),
            (
                0,
                Inst::LoadField {
                    dst: 1,
                    obj: 2,
                    at: 3,
                    layout: L,
                },
            ),
            (
                0,
                Inst::StoreField {
                    obj: 1,
                    at: 2,
                    src: 3,
                    layout: L,
                },
            ),
            (
                0,
                Inst::LoadElem {
                    dst: 1,
                    obj: 2,
                    index: 3,
                    layout: L,
                },
            ),
            (
                0,
                Inst::StoreElem {
                    obj: 1,
                    index: 2,
                    src: 3,
                    layout: L,
                },
            ),
            (0, Inst::Len { dst: 1, obj: 2 }),
            (0, Inst::LayoutOf { dst: 1, obj: 2 }),
            (0, Inst::AddrOfSlot { dst: 1, slot: 2 }),
            (
                0,
                Inst::AddrOfField {
                    dst: 1,
                    obj: 2,
                    at: 3,
                },
            ),
            (
                0,
                Inst::AddrOfElem {
                    dst: 1,
                    obj: 2,
                    index: 3,
                    layout: L,
                },
            ),
            (
                0,
                Inst::AddrOfPart {
                    dst: 1,
                    addr: 2,
                    at: 3,
                },
            ),
            (
                0,
                Inst::Load {
                    dst: 1,
                    addr: 2,
                    layout: L,
                },
            ),
            (
                0,
                Inst::Store {
                    addr: 1,
                    src: 2,
                    layout: L,
                },
            ),
            (
                0,
                Inst::Box {
                    dst: 1,
                    src: 2,
                    layout: L,
                },
            ),
            (
                0,
                Inst::Unbox {
                    dst: 1,
                    src: 2,
                    layout: L,
                },
            ),
            (
                0,
                Inst::ScopeEnter {
                    dst: 1,
                    name: StrId(2),
                },
            ),
            (
                0,
                Inst::ScopeLeave {
                    scope: 1,
                    failed: 2,
                    error: 3,
                    layout: L,
                },
            ),
            (0, Inst::ScopeCancel { scope: 1 }),
            (
                0,
                Inst::Spawn {
                    dst: 1,
                    scope: 2,
                    closure: 3,
                    answer: L,
                },
            ),
            (
                0,
                Inst::Await {
                    dst: 1,
                    task: 2,
                    answer: L,
                },
            ),
            (0, Inst::Cancel { task: 1 }),
            (
                0,
                Inst::Settled {
                    dst: 1,
                    src: 2,
                    answer: L,
                },
            ),
            (0, Inst::SharedLock { cell: 1 }),
            (0, Inst::SharedUnlock { cell: 1 }),
            (0, Inst::Trap { message: StrId(2) }),
            (0, Inst::AssertFailed { message: 1 }),
        ]);
        held
    }

    /// Every value an operand can take that is one step from not fitting.
    ///
    /// Slot 0 and slot 65,535; the extreme immediates; the widest branch
    /// either way; an id at `u32::MAX`. These are the encodings that would
    /// have been silently wrong under a narrower field, so each of them is
    /// round-tripped rather than merely built.
    fn boundaries() -> Vec<(Pc, Inst)> {
        let top = u16::MAX as Slot;
        vec![
            (0, Inst::Unit { dst: 0 }),
            (0, Inst::Unit { dst: top }),
            (
                0,
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: top,
                    a: top,
                    b: top,
                },
            ),
            (
                0,
                Inst::Int {
                    dst: 0,
                    value: i64::MIN,
                },
            ),
            (
                0,
                Inst::Int {
                    dst: top,
                    value: i64::MAX,
                },
            ),
            (0, Inst::Int { dst: 0, value: -1 }),
            (
                0,
                Inst::Float {
                    dst: 0,
                    bits: u64::MAX,
                },
            ),
            (0, Inst::Float { dst: 0, bits: 0 }),
            (
                0,
                Inst::ArithImm {
                    op: ArithOp::Sub,
                    dst: 0,
                    a: top,
                    value: i64::MIN,
                },
            ),
            (
                0,
                Inst::CmpImm {
                    op: CmpOp::Lt,
                    dst: top,
                    a: 0,
                    value: i64::MAX,
                },
            ),
            // The widest displacement in each direction that two `u32`
            // program counters can name.
            (0, Inst::Jump { to: Pc::MAX }),
            (Pc::MAX, Inst::Jump { to: 0 }),
            (
                0,
                Inst::BranchFalse {
                    cond: 0,
                    to: Pc::MAX,
                },
            ),
            (Pc::MAX, Inst::BranchFalse { cond: top, to: 0 }),
            (
                0,
                Inst::Str {
                    dst: 0,
                    text: StrId(u32::MAX),
                },
            ),
            (
                0,
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(u32::MAX),
                    args: ArgsId(u32::MAX),
                },
            ),
            (
                0,
                Inst::LoadField {
                    dst: 0,
                    obj: 1,
                    at: u32::MAX,
                    layout: LayoutId(u32::MAX),
                },
            ),
            (
                0,
                Inst::Alloc {
                    dst: 0,
                    layout: LayoutId(u32::MAX),
                    len: Len::Count(u32::MAX),
                },
            ),
        ]
    }

    /// Every one of the hundred opcodes is produced by some sample.
    ///
    /// The structural half of the round trip: a variant added to `Inst`
    /// cannot pass this without an instruction here that encodes to it.
    #[test]
    fn every_opcode_is_reached_by_a_sample() {
        let reached: BTreeSet<u8> = samples()
            .into_iter()
            .map(|(pc, inst)| encode(&inst, pc).expect("the sample encodes").opcode())
            .collect();
        let defined: BTreeSet<u8> = Op::all().into_iter().map(Op::number).collect();
        let missing: Vec<Op> = defined
            .difference(&reached)
            .map(|number| Op::from_number(*number).expect("a defined opcode"))
            .collect();
        assert!(missing.is_empty(), "no sample encodes to {missing:?}");
        assert_eq!(reached, defined);
    }

    /// The encoding is 1:1, so decoding is a genuine inverse for every
    /// instruction and every boundary value.
    #[test]
    fn decoding_an_encoded_instruction_gives_the_instruction_back() {
        for (pc, inst) in samples().into_iter().chain(boundaries()) {
            let bytes = encode(&inst, pc).expect("the sample encodes");
            assert_eq!(decode(bytes, pc), Ok(inst.clone()), "{inst:?} at {pc}");
        }
    }

    /// The other half, which is what "canonical" means: an encoding is the
    /// *only* encoding of what it says, so re-encoding what was decoded gives
    /// the same sixteen bytes back.
    #[test]
    fn encoding_a_decoded_instruction_gives_the_bytes_back() {
        for (pc, inst) in samples().into_iter().chain(boundaries()) {
            let bytes = encode(&inst, pc).expect("the sample encodes");
            let read = decode(bytes, pc).expect("the encoding decodes");
            assert_eq!(encode(&read, pc), Ok(bytes), "{inst:?} at {pc}");
        }
    }

    /// Encoding is a function of the instruction and its pc and of nothing
    /// else, so two encodings of one program are byte-identical.
    #[test]
    fn encoding_the_same_instruction_twice_gives_the_same_bytes() {
        for (pc, inst) in samples().into_iter().chain(boundaries()) {
            assert_eq!(encode(&inst, pc), encode(&inst, pc));
        }
    }

    /// `flags` carries nothing and there is no way to set it, which is what
    /// lets the verifier reject a nonzero one outright.
    #[test]
    fn nothing_the_encoder_produces_sets_flags() {
        for (pc, inst) in samples().into_iter().chain(boundaries()) {
            assert_eq!(encode(&inst, pc).expect("encodes").flags(), 0, "{inst:?}");
        }
    }

    /// A slot a sixteen-bit operand cannot name is refused rather than
    /// truncated or wrapped. `crate::lower` refuses the frame that would
    /// contain one, so this is the assertion behind that promise.
    #[test]
    fn a_slot_past_sixty_five_thousand_five_hundred_and_thirty_five_is_refused() {
        let top = u16::MAX as Slot;
        assert!(encode(&Inst::Unit { dst: top }, 0).is_ok());
        assert_eq!(
            encode(&Inst::Unit { dst: top + 1 }, 0),
            Err(TooWide::Slot { slot: 65_536 })
        );
        assert_eq!(
            encode(
                &Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 0,
                    a: 0,
                    b: 70_000,
                },
                0
            ),
            Err(TooWide::Slot { slot: 70_000 })
        );
        assert!(
            format!("{}", TooWide::Slot { slot: 65_536 }).contains("sixteen bits"),
            "the refusal says which limit it is"
        );
    }

    /// A branch is relative, and the displacement is `to - (pc + 1)` — so a
    /// fall-through is zero and the sign says which way it goes.
    #[test]
    fn a_branch_carries_the_distance_to_its_target_and_not_the_target() {
        let forward = encode(&Inst::Jump { to: 9 }, 5).expect("encodes");
        assert_eq!(forward.payload() as i64, 3);
        let back = encode(&Inst::Jump { to: 5 }, 9).expect("encodes");
        assert_eq!(back.payload() as i64, -5);
        let next = encode(&Inst::Jump { to: 5 }, 4).expect("encodes");
        assert_eq!(next.payload() as i64, 0);
        // Every pc a `u32` can name has a displacement an `i64` can hold, so
        // the encoder's overflow arm is unreachable — which is why it is an
        // arm rather than a paragraph.
        assert_eq!(
            encode(&Inst::Jump { to: Pc::MAX }, 0)
                .expect("encodes")
                .payload() as i64,
            i64::from(Pc::MAX) - 1
        );
        assert_eq!(
            encode(&Inst::Jump { to: 0 }, Pc::MAX)
                .expect("encodes")
                .payload() as i64,
            -(i64::from(Pc::MAX) + 1)
        );
    }

    /// ADR 0041's own example rows, byte for byte, so that the audit table
    /// and the encoder are pinned to each other rather than to a reading of
    /// each other.
    #[test]
    fn the_audits_own_rows_encode_where_the_audit_says_they_do() {
        let int = encode(&Inst::Int { dst: 5, value: -2 }, 0).expect("encodes");
        assert_eq!(int.a(), 5);
        assert_eq!(int.payload(), (-2i64) as u64);

        let add = encode(
            &Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 2,
                b: 3,
            },
            0,
        )
        .expect("encodes");
        assert_eq!((add.a(), add.b(), add.c(), add.payload()), (1, 2, 3, 0));

        // `load.field` packs the word offset and the layout into the two
        // halves, and both keep their full thirty-two bits.
        let field = encode(
            &Inst::LoadField {
                dst: 1,
                obj: 2,
                at: 7,
                layout: LayoutId(9),
            },
            0,
        )
        .expect("encodes");
        assert_eq!((field.a(), field.b(), field.lo(), field.hi()), (1, 2, 7, 9));

        // `call.resource` is the densest call: two slots and both halves,
        // with `c` still empty.
        let resource = encode(
            &Inst::CallResource {
                dst: 1,
                receiver: 2,
                op: HostOpId(3),
                args: ArgsId(4),
            },
            0,
        )
        .expect("encodes");
        assert_eq!(
            (
                resource.a(),
                resource.b(),
                resource.c(),
                resource.lo(),
                resource.hi()
            ),
            (1, 2, 0, 3, 4)
        );

        // `scope.leave` is the three-slot case the issue predicted would be
        // tight, and it fits with the payload half empty.
        let leave = encode(
            &Inst::ScopeLeave {
                scope: 1,
                failed: 2,
                error: 3,
                layout: LayoutId(4),
            },
            0,
        )
        .expect("encodes");
        assert_eq!(
            (leave.a(), leave.b(), leave.c(), leave.lo(), leave.hi()),
            (1, 2, 3, 4, 0)
        );
    }

    /// `Len`'s three forms are three opcodes, so nothing stores a
    /// discriminant and `alloc.imm x0` is not `alloc.fixed`.
    #[test]
    fn the_three_alloc_forms_are_three_opcodes_and_not_a_tagged_field() {
        let at = |len| {
            encode(
                &Inst::Alloc {
                    dst: 1,
                    layout: L,
                    len,
                },
                0,
            )
            .expect("encodes")
            .opcode()
        };
        assert_eq!(at(Len::Fixed), Op::AllocFixed.number());
        assert_eq!(at(Len::Count(0)), Op::AllocImm.number());
        assert_eq!(at(Len::Slot(2)), Op::AllocSlot.number());
        assert_ne!(at(Len::Fixed), at(Len::Count(0)));
    }
}
