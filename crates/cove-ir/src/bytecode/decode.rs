//! Sixteen bytes back to `Inst`.
//!
//! ADR 0041 makes the encoding 1:1, so this is a genuine inverse and not a
//! best effort: `decode(encode(i, pc), pc) == i` for every instruction, and
//! `encode(decode(b, pc), pc) == b` for every byte pattern this accepts.
//!
//! # It is strict, and that is what makes the encoding canonical
//!
//! A field an opcode does not use must be zero, `flags` must be zero, and a
//! `const.bool` payload must be `0` or `1`. Bytes that say the same thing in
//! two ways are refused rather than normalised, which is what gives the second
//! half of the round trip: a program has exactly one encoding, so two
//! encodings of it are byte-identical and a diff over encoded code means
//! something.
//!
//! # What it is for
//!
//! Tests, the debugger, and [`disasm`](super::disasm) — never the dispatch
//! loop. A verified program is executed from its bytes; this is how a human
//! reads them back.

use crate::inst::{Inst, Len, Pc, Slot};
use crate::layout::LayoutId;
use crate::{ArgsId, BuiltinId, FunctionId, HostOpId, StrId, TableId};

use super::op::{Half, Op, Operand, Payload};
use super::EncodedInst;

/// Why sixteen bytes are not an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// The opcode byte names no defined operation.
    Opcode(u8),
    /// `flags` is reserved and must be zero.
    Flags(u8),
    /// A field the opcode does not use, and it is not zero.
    NotCanonical {
        /// `a`, `b`, `c`, `payload`, `payload.low` or `payload.high`.
        field: &'static str,
        value: u64,
    },
    /// A `const.bool` whose payload is neither `0` nor `1`.
    Bool(u64),
    /// A branch whose target is not a program counter at all — before the
    /// first instruction, or past what a [`Pc`] can name.
    ///
    /// Whether it is inside *this function* is [`verify`](super::verify())'s
    /// question, because only the function knows how long it is.
    Target { pc: Pc, displacement: i64 },
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Malformed::Opcode(byte) => write!(f, "opcode {byte} names no operation"),
            Malformed::Flags(flags) => {
                write!(f, "flags is {flags}, and it is reserved and must be zero")
            }
            Malformed::NotCanonical { field, value } => write!(
                f,
                "this opcode does not use {field}, and it holds {value} rather than zero"
            ),
            Malformed::Bool(value) => {
                write!(f, "a bool constant holds {value}, which is neither 0 nor 1")
            }
            Malformed::Target { pc, displacement } => write!(
                f,
                "a branch at {pc} displaced by {displacement} lands on no program counter"
            ),
        }
    }
}

/// Reads one encoded instruction back.
///
/// `pc` is where it sits, and only a branch reads it: the displacement in the
/// payload is relative to the instruction after this one.
pub fn decode(code: EncodedInst, pc: Pc) -> Result<Inst, Malformed> {
    if code.flags() != 0 {
        return Err(Malformed::Flags(code.flags()));
    }
    let Some(op) = Op::from_number(code.opcode()) else {
        return Err(Malformed::Opcode(code.opcode()));
    };
    canonical(code, op)?;

    let a = code.a() as Slot;
    let b = code.b() as Slot;
    let c = code.c() as Slot;
    let lo = code.lo();
    let hi = code.hi();
    let layout = LayoutId(lo);
    Ok(match op {
        Op::ConstUnit => Inst::Unit { dst: a },
        Op::ConstBool => Inst::Bool {
            dst: a,
            value: match code.payload() {
                0 => false,
                1 => true,
                held => return Err(Malformed::Bool(held)),
            },
        },
        Op::ConstInt => Inst::Int {
            dst: a,
            value: code.payload() as i64,
        },
        Op::ConstFloat => Inst::Float {
            dst: a,
            bits: code.payload(),
        },
        Op::Str => Inst::Str {
            dst: a,
            text: StrId(lo),
        },
        Op::Copy => Inst::Copy {
            dst: a,
            src: b,
            layout,
        },
        Op::Clear => Inst::Clear { slot: a, layout },
        Op::Neg(num) => Inst::Neg { num, dst: a, a: b },
        Op::Arith(num, op) => Inst::Arith {
            num,
            op,
            dst: a,
            a: b,
            b: c,
        },
        Op::Cmp(on, op) => Inst::Cmp {
            on,
            op,
            dst: a,
            a: b,
            b: c,
        },
        Op::ArithImm(op) => Inst::ArithImm {
            op,
            dst: a,
            a: b,
            value: code.payload() as i64,
        },
        Op::CmpImm(op) => Inst::CmpImm {
            op,
            dst: a,
            a: b,
            value: code.payload() as i64,
        },
        Op::Not => Inst::Not { dst: a, a: b },
        Op::Convert(to) => Inst::Convert { to, dst: a, a: b },
        Op::Jump => Inst::Jump {
            to: target(pc, code.payload() as i64)?,
        },
        Op::BranchFalse => Inst::BranchFalse {
            cond: a,
            to: target(pc, code.payload() as i64)?,
        },
        Op::Switch => Inst::Switch {
            on: a,
            table: TableId(lo),
        },
        Op::Return => Inst::Return { src: a },
        Op::Call => Inst::Call {
            dst: a,
            callee: FunctionId(lo),
            args: ArgsId(hi),
        },
        Op::CallClosure => Inst::CallClosure {
            dst: a,
            closure: b,
            args: ArgsId(lo),
        },
        Op::CallHost => Inst::CallHost {
            dst: a,
            op: HostOpId(lo),
            args: ArgsId(hi),
        },
        Op::CallResource => Inst::CallResource {
            dst: a,
            receiver: b,
            op: HostOpId(lo),
            args: ArgsId(hi),
        },
        Op::CallBuiltin => Inst::CallBuiltin {
            dst: a,
            builtin: BuiltinId(lo),
            args: ArgsId(hi),
        },
        Op::AllocFixed => Inst::Alloc {
            dst: a,
            layout,
            len: Len::Fixed,
        },
        Op::AllocImm => Inst::Alloc {
            dst: a,
            layout,
            len: Len::Count(hi),
        },
        Op::AllocSlot => Inst::Alloc {
            dst: a,
            layout,
            len: Len::Slot(b),
        },
        Op::LoadField => Inst::LoadField {
            dst: a,
            obj: b,
            at: lo,
            layout: LayoutId(hi),
        },
        Op::StoreField => Inst::StoreField {
            obj: a,
            at: lo,
            src: b,
            layout: LayoutId(hi),
        },
        Op::LoadElem => Inst::LoadElem {
            dst: a,
            obj: b,
            index: c,
            layout,
        },
        Op::StoreElem => Inst::StoreElem {
            obj: a,
            index: b,
            src: c,
            layout,
        },
        Op::Len => Inst::Len { dst: a, obj: b },
        Op::LayoutOf => Inst::LayoutOf { dst: a, obj: b },
        Op::AddrOfSlot => Inst::AddrOfSlot { dst: a, slot: b },
        Op::AddrOfField => Inst::AddrOfField {
            dst: a,
            obj: b,
            at: lo,
        },
        Op::AddrOfElem => Inst::AddrOfElem {
            dst: a,
            obj: b,
            index: c,
            layout,
        },
        Op::AddrOfPart => Inst::AddrOfPart {
            dst: a,
            addr: b,
            at: lo,
        },
        Op::Load => Inst::Load {
            dst: a,
            addr: b,
            layout,
        },
        Op::Store => Inst::Store {
            addr: a,
            src: b,
            layout,
        },
        Op::Box => Inst::Box {
            dst: a,
            src: b,
            layout,
        },
        Op::Unbox => Inst::Unbox {
            dst: a,
            src: b,
            layout,
        },
        Op::ScopeEnter => Inst::ScopeEnter {
            dst: a,
            name: StrId(lo),
        },
        Op::ScopeLeave => Inst::ScopeLeave {
            scope: a,
            failed: b,
            error: c,
            layout,
        },
        Op::ScopeCancel => Inst::ScopeCancel { scope: a },
        Op::Spawn => Inst::Spawn {
            dst: a,
            scope: b,
            closure: c,
            answer: layout,
        },
        Op::Await => Inst::Await {
            dst: a,
            task: b,
            answer: layout,
        },
        Op::Cancel => Inst::Cancel { task: a },
        Op::Settled => Inst::Settled {
            dst: a,
            src: b,
            answer: layout,
        },
        Op::SharedLock => Inst::SharedLock { cell: a },
        Op::SharedUnlock => Inst::SharedUnlock { cell: a },
        Op::Trap => Inst::Trap { message: StrId(lo) },
        Op::AssertFailed => Inst::AssertFailed { message: a },
    })
}

/// Every field the opcode does not use is zero.
///
/// One rule over the same table [`verify`](super::verify) reads, so that
/// "canonical" is a property of the format rather than of whichever reader
/// remembered to check it.
fn canonical(code: EncodedInst, op: Op) -> Result<(), Malformed> {
    let fields = op.fields();
    for (operand, (name, held)) in
        fields
            .operands()
            .into_iter()
            .zip([("a", code.a()), ("b", code.b()), ("c", code.c())])
    {
        if operand == Operand::Unused && held != 0 {
            return Err(Malformed::NotCanonical {
                field: name,
                value: u64::from(held),
            });
        }
    }
    match fields.payload {
        Payload::Empty => {
            if code.payload() != 0 {
                return Err(Malformed::NotCanonical {
                    field: "payload",
                    value: code.payload(),
                });
            }
        }
        // A `Bool` payload's own range is checked where it is read, so that
        // the fault names the constant rather than the field.
        Payload::Bool | Payload::Imm | Payload::Displacement => {}
        Payload::Halves(lo, hi) => {
            for (half, (name, held)) in [lo, hi]
                .into_iter()
                .zip([("payload.low", code.lo()), ("payload.high", code.hi())])
            {
                if half == Half::Unused && held != 0 {
                    return Err(Malformed::NotCanonical {
                        field: name,
                        value: u64::from(held),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Where a displacement points, as a program counter.
fn target(pc: Pc, displacement: i64) -> Result<Pc, Malformed> {
    let refused = Malformed::Target { pc, displacement };
    let to = (i64::from(pc) + 1)
        .checked_add(displacement)
        .ok_or(refused)?;
    Pc::try_from(to).map_err(|_| refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::encode::encode;
    use crate::inst::Num;

    /// The bytes of `const.int s1 = 7`, as a starting point for bending one
    /// field at a time.
    fn int() -> EncodedInst {
        encode(&Inst::Int { dst: 1, value: 7 }, 0).expect("encodes")
    }

    /// Sets one byte, which is how a test writes bytes no encoder produced.
    fn with(code: EncodedInst, at: usize, byte: u8) -> EncodedInst {
        let mut bytes = *code.bytes();
        bytes[at] = byte;
        EncodedInst::from_bytes(bytes)
    }

    /// A byte that names no operation is refused rather than indexed with.
    /// A hundred opcodes are defined out of 256, so more than half of all
    /// bytes reach this.
    #[test]
    fn an_opcode_no_encoder_produced_is_refused() {
        for byte in 100u8..=255 {
            assert_eq!(
                decode(with(int(), 0, byte), 0),
                Err(Malformed::Opcode(byte))
            );
        }
    }

    /// `flags` is reserved and carries nothing, so a nonzero one is bytes
    /// that mean something this format does not define.
    #[test]
    fn a_nonzero_flags_byte_is_refused() {
        assert_eq!(decode(int(), 0), Ok(Inst::Int { dst: 1, value: 7 }));
        assert_eq!(decode(with(int(), 1, 1), 0), Err(Malformed::Flags(1)));
        assert_eq!(decode(with(int(), 1, 0x80), 0), Err(Malformed::Flags(0x80)));
    }

    /// A field the opcode does not use must be zero. That is what makes the
    /// encoding canonical: one program, one byte string, and a diff over
    /// encoded code that means something.
    #[test]
    fn a_field_the_opcode_does_not_use_must_be_zero() {
        // `const.int` uses `a` and the payload, and neither `b` nor `c`.
        assert_eq!(
            decode(with(int(), 4, 1), 0),
            Err(Malformed::NotCanonical {
                field: "b",
                value: 1
            })
        );
        assert_eq!(
            decode(with(int(), 6, 3), 0),
            Err(Malformed::NotCanonical {
                field: "c",
                value: 3
            })
        );
        // `neg.int` uses no payload at all.
        let neg = encode(
            &Inst::Neg {
                num: Num::Int,
                dst: 1,
                a: 2,
            },
            0,
        )
        .expect("encodes");
        assert_eq!(
            decode(with(neg, 8, 1), 0),
            Err(Malformed::NotCanonical {
                field: "payload",
                value: 1
            })
        );
        // `str` uses the low half and not the high one.
        let text = encode(
            &Inst::Str {
                dst: 1,
                text: crate::StrId(2),
            },
            0,
        )
        .expect("encodes");
        assert_eq!(
            decode(with(text, 12, 1), 0),
            Err(Malformed::NotCanonical {
                field: "payload.high",
                value: 1
            })
        );
    }

    /// A `Bool` is one bit in sixty-four, and every other value of the
    /// payload is bytes that decode to no instruction — not to `true`.
    #[test]
    fn a_bool_constant_holds_zero_or_one_and_nothing_else() {
        let held = |value: u64| {
            let base = encode(
                &Inst::Bool {
                    dst: 1,
                    value: false,
                },
                0,
            )
            .expect("encodes");
            let mut bytes = *base.bytes();
            bytes[8..16].copy_from_slice(&value.to_le_bytes());
            decode(EncodedInst::from_bytes(bytes), 0)
        };
        assert_eq!(
            held(0),
            Ok(Inst::Bool {
                dst: 1,
                value: false
            })
        );
        assert_eq!(
            held(1),
            Ok(Inst::Bool {
                dst: 1,
                value: true
            })
        );
        assert_eq!(held(2), Err(Malformed::Bool(2)));
        assert_eq!(held(u64::MAX), Err(Malformed::Bool(u64::MAX)));
    }

    /// A displacement that lands before the first instruction, or past what a
    /// program counter can name, is not a program counter — which is a
    /// different question from whether it is inside *this* function, and that
    /// one is the verifier's.
    #[test]
    fn a_displacement_that_names_no_program_counter_is_refused() {
        let jump = |pc: Pc, displacement: i64| {
            let base = encode(&Inst::Jump { to: 0 }, 0).expect("encodes");
            let mut bytes = *base.bytes();
            bytes[8..16].copy_from_slice(&displacement.to_le_bytes());
            decode(EncodedInst::from_bytes(bytes), pc)
        };
        assert_eq!(jump(0, -1), Ok(Inst::Jump { to: 0 }));
        assert_eq!(
            jump(0, -2),
            Err(Malformed::Target {
                pc: 0,
                displacement: -2
            })
        );
        assert_eq!(
            jump(0, i64::MAX),
            Err(Malformed::Target {
                pc: 0,
                displacement: i64::MAX
            })
        );
        assert_eq!(
            jump(Pc::MAX, i64::MIN),
            Err(Malformed::Target {
                pc: Pc::MAX,
                displacement: i64::MIN
            })
        );
    }

    /// Nothing in the decoder panics, whatever the sixteen bytes are. The
    /// format is internal, and a reader of it is still a reader of input.
    #[test]
    fn arbitrary_bytes_answer_rather_than_panic() {
        let mut bytes = [0u8; EncodedInst::BYTES];
        for seed in 0u32..4_000 {
            for (at, byte) in bytes.iter_mut().enumerate() {
                *byte = (seed.wrapping_mul(2_654_435_761).rotate_left(at as u32 * 3)) as u8;
            }
            let _ = decode(EncodedInst::from_bytes(bytes), seed);
        }
    }
}
