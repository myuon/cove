//! The dispatch loop over [`EncodedInst`], and the refusal that keeps it
//! honest.
//!
//! [ADR 0041](../../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! decided the sixteen-byte instruction and `cove_ir::bytecode` built the
//! encoder, the decoder, the verifier and the disassembly. This is
//! [issue #245](https://github.com/myuon/cove/issues/245)'s **Phase 5**:
//! **the only loop**. The `Inst` loop that stood beside it through Phases 3
//! and 4 is deleted, `Vm::new` encodes and verifies before it hands back a
//! machine, and there is no flag, no constructor and no fallback that
//! selects a second representation, because there is no second
//! representation to select.
//!
//! # What runs, and what `Inst` is now for
//!
//! ```text
//! checked AST -> lowering -> cove_ir::Inst -> encoder -> bytecode -> verify once -> here
//! ```
//!
//! `Inst` is the compiler's vocabulary and stays exactly as it was: the
//! lowering builds it, `cove_ir::print` renders it, the optimiser and the
//! tests read it, `cove_ir::verify` checks it, and `cove debug` shows it as
//! the lowered IR beside the source. What changed at the cutover is that
//! nothing *executes* it.
//!
//! # Why it is still a file of its own
//!
//! `ad5f160` measured something this crate is built around. Writing the
//! debugger's question inline in the loop cost **4.3% on `arith`** — code
//! that never ran when no debugger was installed — and [`Machine::ask`] is
//! `#[inline(never)]` because of it. The dispatch body's footprint and its
//! branch-target alignment are costs every program pays, whether or not the
//! added code is reached. Phase 4 measured the same thing an order of
//! magnitude larger: growing this loop from fourteen opcodes to a hundred
//! cost 22.7% on `arith` *in the fourteen it already had*.
//!
//! So the loop keeps a module to itself, and the rule that comes with it is
//! not about tidiness: **nothing whose cost a program does not pay belongs
//! inside `dispatch`.** [`open_frame`] and [`Machine::ask`] are out of line
//! for that reason and measured to be.
//!
//! It is a *child* module of [`super`] rather than a sibling, which is what
//! lets it read `Machine`'s private fields without widening them to the
//! crate. The loop over the machine is exactly as privileged as the machine;
//! making its state `pub(crate)` would have handed that privilege to
//! everything else as well.
//!
//! # Verified once, then trusted
//!
//! [`prepare`] encodes, verifies, and then walks every instruction of every
//! function before a machine exists, and it **refuses the program** rather
//! than handing it back. That is what lets the loop below read `held.a()` as
//! a frame slot without a bound, and `held.lo()` as a `LayoutId` without a
//! table lookup that could fail.
//!
//! Nothing in the corpus reaches either refusal. They stay because "cannot
//! happen" and "does not exist" are different claims: [`implemented`] is an
//! *exhaustive* match, so an opcode added to [`Op`] is a compile error here
//! rather than a program that runs the wrong instruction, and a byte that
//! names no opcode is still a byte a loader could one day produce.
//!
//! There is **no fallback**. There is nothing to fall back to, which is the
//! form issue #245's *"no silent fallback to enum execution"* takes once the
//! enum execution is gone — but the refusal is still a refusal and not a
//! quiet hand-back, because a program this machine cannot execute must stop
//! before it has done anything rather than partway through.
//!
//! # It is the same machine
//!
//! Nothing here is a second implementation of anything a program can
//! observe. The fuel accounting, the safepoint, the debugger question, the
//! collector poll and the span lookup are [`Machine`]'s own lines in
//! [`Machine`]'s own order; the arithmetic is [`super::int_arith`],
//! [`super::float_arith`] and [`super::compare`]; a call pushes
//! [`super::Frame`] onto the stack, a host call is [`Machine::call_host`], a
//! spawn is [`Machine::spawn`], and a scope is left by
//! [`Machine::leave_scope`]. **One encoded instruction is one instruction
//! and one unit of fuel**, which is what
//! [ADR 0040](../../../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
//! bounds are stated in and `crates/cove-runtime/tests/responsiveness.rs`
//! measures.
//!
//! Bytecode pc *is* IR pc — ADR 0041's 1:1 encoding — so `Function::spans`
//! is indexed by the same number, a failure points at the same place without
//! a remapping, and the debugger's `Local` ranges, `Call::pc` and marked
//! line all mean what they meant when an `Inst` was what ran.

use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};

use cove_diag::Span;

use cove_ir::bytecode::{disasm, encode_program, verify, Encoded, EncodedInst, Op};
use cove_ir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Convert, FunctionId, HostOpId, LayoutId, Num,
    Program, Repr, Shape, Slot, StrId, TableId,
};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::vm::cell;
use crate::vm::mem::Overflow;

use super::{
    compare, float_arith, int_arith, null_object, overflowed, reentrant_lock, wrong_arity,
    ChildState, Frame, Live, Machine, Outcome, ScopeEntry, BUFFER_LEN, SAFEPOINT_STRIDE,
};

// The opcodes this path runs, by the name ADR 0041 gives them rather than by
// number. `Op::number` is a `const fn` so that these are `match` patterns:
// the numbers are positions in a generated table and move when the table
// does, and nothing here should have to move with them.
const CONST_UNIT: u8 = Op::ConstUnit.number();
const CONST_BOOL: u8 = Op::ConstBool.number();
const CONST_INT: u8 = Op::ConstInt.number();
const CONST_TAG: u8 = Op::ConstTag.number();
const FUNC_REF: u8 = Op::FuncRef.number();
const CONST_FLOAT: u8 = Op::ConstFloat.number();
const STR: u8 = Op::Str.number();
const COPY: u8 = Op::Copy.number();
const CLEAR: u8 = Op::Clear.number();

const NEG_INT: u8 = Op::Neg(Num::Int).number();
const NEG_FLOAT: u8 = Op::Neg(Num::Float).number();

const ADD_INT: u8 = Op::Arith(Num::Int, ArithOp::Add).number();
const SUB_INT: u8 = Op::Arith(Num::Int, ArithOp::Sub).number();
const MUL_INT: u8 = Op::Arith(Num::Int, ArithOp::Mul).number();
const DIV_INT: u8 = Op::Arith(Num::Int, ArithOp::Div).number();
const REM_INT: u8 = Op::Arith(Num::Int, ArithOp::Rem).number();

const ADD_FLOAT: u8 = Op::Arith(Num::Float, ArithOp::Add).number();
const SUB_FLOAT: u8 = Op::Arith(Num::Float, ArithOp::Sub).number();
const MUL_FLOAT: u8 = Op::Arith(Num::Float, ArithOp::Mul).number();
const DIV_FLOAT: u8 = Op::Arith(Num::Float, ArithOp::Div).number();
const REM_FLOAT: u8 = Op::Arith(Num::Float, ArithOp::Rem).number();

const EQ_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Eq).number();
const NE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Ne).number();
const LT_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Lt).number();
const LE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Le).number();
const GT_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Gt).number();
const GE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Ge).number();

const EQ_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Eq).number();
const NE_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Ne).number();
const LT_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Lt).number();
const LE_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Le).number();
const GT_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Gt).number();
const GE_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Ge).number();

const EQ_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Eq).number();
const NE_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Ne).number();
const LT_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Lt).number();
const LE_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Le).number();
const GT_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Gt).number();
const GE_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Ge).number();

const EQ_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Eq).number();
const NE_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Ne).number();
const LT_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Lt).number();
const LE_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Le).number();
const GT_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Gt).number();
const GE_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Ge).number();

const EQ_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Eq).number();
const NE_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Ne).number();
const LT_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Lt).number();
const LE_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Le).number();
const GT_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Gt).number();
const GE_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Ge).number();
const EQ_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Eq).number();
const NE_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Ne).number();
const LT_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Lt).number();
const LE_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Le).number();
const GT_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Gt).number();
const GE_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Ge).number();

const ADD_INT_IMM: u8 = Op::ArithImm(ArithOp::Add).number();
const SUB_INT_IMM: u8 = Op::ArithImm(ArithOp::Sub).number();
const MUL_INT_IMM: u8 = Op::ArithImm(ArithOp::Mul).number();
const DIV_INT_IMM: u8 = Op::ArithImm(ArithOp::Div).number();
const REM_INT_IMM: u8 = Op::ArithImm(ArithOp::Rem).number();

const EQ_INT_IMM: u8 = Op::CmpImm(CmpOp::Eq).number();
const NE_INT_IMM: u8 = Op::CmpImm(CmpOp::Ne).number();
const LT_INT_IMM: u8 = Op::CmpImm(CmpOp::Lt).number();
const LE_INT_IMM: u8 = Op::CmpImm(CmpOp::Le).number();
const GT_INT_IMM: u8 = Op::CmpImm(CmpOp::Gt).number();
const GE_INT_IMM: u8 = Op::CmpImm(CmpOp::Ge).number();

const NOT: u8 = Op::Not.number();
const INT_TO_FLOAT: u8 = Op::Convert(Convert::IntToFloat).number();
const FLOAT_TO_INT: u8 = Op::Convert(Convert::FloatToInt).number();

const JUMP: u8 = Op::Jump.number();
const BRANCH_FALSE: u8 = Op::BranchFalse.number();
const SWITCH: u8 = Op::Switch.number();
const RETURN: u8 = Op::Return.number();

const CALL: u8 = Op::Call.number();
const CALL_CLOSURE: u8 = Op::CallClosure.number();
const CALL_HOST: u8 = Op::CallHost.number();
const CALL_RESOURCE: u8 = Op::CallResource.number();
const CALL_BUILTIN: u8 = Op::CallBuiltin.number();

const ALLOC_FIXED: u8 = Op::AllocFixed.number();
const ALLOC_IMM: u8 = Op::AllocImm.number();
const ALLOC_SLOT: u8 = Op::AllocSlot.number();
const LOAD_FIELD: u8 = Op::LoadField.number();
const STORE_FIELD: u8 = Op::StoreField.number();
const LOAD_ELEM: u8 = Op::LoadElem.number();
const STORE_ELEM: u8 = Op::StoreElem.number();
const BYTE_AT: u8 = Op::ByteAt.number();
const ALLOC_BYTES: u8 = Op::AllocBytes.number();
const WRITE_BYTE: u8 = Op::WriteByte.number();
const COPY_BYTES: u8 = Op::CopyBytes.number();
const FINISH_STRING: u8 = Op::FinishString.number();
const ALLOC_BUFFER: u8 = Op::AllocBuffer.number();
const APPEND_BYTE: u8 = Op::AppendByte.number();
const APPEND_BYTES: u8 = Op::AppendBytes.number();
const FINISH_BUFFER: u8 = Op::FinishBuffer.number();
const LEN: u8 = Op::Len.number();
const LAYOUT_OF: u8 = Op::LayoutOf.number();

const ADDR_OF_SLOT: u8 = Op::AddrOfSlot.number();
const ADDR_OF_FIELD: u8 = Op::AddrOfField.number();
const ADDR_OF_ELEM: u8 = Op::AddrOfElem.number();
const ADDR_OF_PART: u8 = Op::AddrOfPart.number();
const LOAD: u8 = Op::Load.number();
const STORE: u8 = Op::Store.number();

const BOX: u8 = Op::Box.number();
const UNBOX: u8 = Op::Unbox.number();

const SCOPE_ENTER: u8 = Op::ScopeEnter.number();
const SCOPE_LEAVE: u8 = Op::ScopeLeave.number();
const SCOPE_CANCEL: u8 = Op::ScopeCancel.number();
const SPAWN: u8 = Op::Spawn.number();
const AWAIT: u8 = Op::Await.number();
const CANCEL: u8 = Op::Cancel.number();
const SETTLED: u8 = Op::Settled.number();

const SHARED_LOCK: u8 = Op::SharedLock.number();
const SHARED_UNLOCK: u8 = Op::SharedUnlock.number();

const TRAP: u8 = Op::Trap.number();
const ASSERT_FAILED: u8 = Op::AssertFailed.number();

/// Whether [`dispatch`] implements this opcode.
///
/// Exhaustive rather than a `matches!` list, and since the cutover that is
/// the only useful thing it can be: a *proof obligation*. A new [`Op`] fails
/// to compile here until somebody decides what the loop does with it,
/// instead of becoming a program that has no way to run at all. The answer
/// is a constant, so the walk [`prepare`] makes is free.
pub(crate) fn implemented(op: Op) -> bool {
    match op {
        Op::ConstUnit
        | Op::ConstBool
        | Op::ConstInt
        | Op::FuncRef
        | Op::ConstTag
        | Op::ConstFloat
        | Op::Str
        | Op::Copy
        | Op::Clear
        | Op::Neg(_)
        | Op::Arith(_, _)
        | Op::Cmp(_, _)
        | Op::ArithImm(_)
        | Op::CmpImm(_)
        | Op::Not
        | Op::Convert(_)
        | Op::Jump
        | Op::BranchFalse
        | Op::Switch
        | Op::Return
        | Op::Call
        | Op::CallClosure
        | Op::CallHost
        | Op::CallResource
        | Op::CallBuiltin
        | Op::AllocFixed
        | Op::AllocImm
        | Op::AllocSlot
        | Op::ByteAt
        | Op::AllocBytes
        | Op::WriteByte
        | Op::CopyBytes
        | Op::FinishString
        | Op::AllocBuffer
        | Op::AppendByte
        | Op::AppendBytes
        | Op::FinishBuffer
        | Op::LoadField
        | Op::StoreField
        | Op::LoadElem
        | Op::StoreElem
        | Op::Len
        | Op::LayoutOf
        | Op::AddrOfSlot
        | Op::AddrOfField
        | Op::AddrOfElem
        | Op::AddrOfPart
        | Op::Load
        | Op::Store
        | Op::Box
        | Op::Unbox
        | Op::ScopeEnter
        | Op::ScopeLeave
        | Op::ScopeCancel
        | Op::Spawn
        | Op::Await
        | Op::Cancel
        | Op::Settled
        | Op::SharedLock
        | Op::SharedUnlock
        | Op::Trap
        | Op::AssertFailed => true,
    }
}

/// `program` in the form [`dispatch`] runs, or why it cannot be run.
///
/// Encode, verify once, then check that every opcode has an implementation —
/// in that order, because the verifier is what establishes the structural
/// facts the loop then trusts, and asking whether an opcode is implemented
/// before knowing it is a real opcode would be asking about a byte.
///
/// Called once per machine, from `Machine::for_run`, and its answer is kept
/// there. Every refusal is raised before a run pushes a frame, so a program
/// this machine cannot execute has no observable effect at all rather than
/// stopping partway through one.
pub(crate) fn prepare(program: &Program) -> Result<Arc<Encoded>, RuntimeError> {
    let encoded = encode_program(program).map_err(|too_wide| {
        RuntimeError::new(format!("this program does not encode: {too_wide}")).with_rule(
            "ADR 0041 gives a slot operand sixteen bits, so a frame of more than 65,536 words has no encoding.",
        )
    })?;
    if let Err(faults) = verify(program, &encoded) {
        let first = faults
            .first()
            .expect("a rejection names at least one fault");
        return Err(RuntimeError::new(format!(
            "the encoded program did not verify: {first}"
        ))
        .with_rule(
            "Encoded instructions are verified once and then trusted, so a program that does not verify is never executed.",
        ));
    }
    for (index, code) in encoded.functions.iter().enumerate() {
        let id = FunctionId(index as u32);
        for (pc, held) in code.iter().enumerate() {
            let op = Op::from_number(held.opcode());
            if op.is_none_or(|op| !implemented(op)) {
                return Err(refusal(program, id, pc, *held));
            }
        }
    }
    Ok(Arc::new(encoded))
}

/// What an opcode this loop does not run is refused with.
///
/// It names the operation, the function, the pc and the instruction as the
/// disassembler renders it, and it points at the source the instruction was
/// lowered from — so the reader is told which construct to stop using or
/// which family to build next, rather than a number.
fn refusal(program: &Program, id: FunctionId, pc: usize, held: EncodedInst) -> RuntimeError {
    let function = program.function(id);
    let named = match Op::from_number(held.opcode()) {
        Some(op) => format!("{op:?}"),
        None => format!("opcode {}", held.opcode()),
    };
    RuntimeError::new(format!(
        "the encoded execution path does not run `{named}` yet"
    ))
    .at(function.span_at(pc))
    .with_rule(
        "The machine implements every opcode ADR 0041 defines, and there is no second representation to hand a program back to: an instruction with no implementation is a gap in the machine rather than a program it declines.",
    )
    .with_help(format!(
        "the instruction is `{}` at `{}` pc {pc}",
        disasm::one(program, id, held, pc as u32),
        function.qualified(),
    ))
}

/// The frame a call opens, out of line: the arity check, the admission, the
/// push, the arguments, and a closure's captures.
///
/// `#[inline(never)]`, and it is the same measurement `Machine::ask` records
/// one level down. Written inline it appears **twice** in [`dispatch`] — once
/// for `call` and once for `call.closure` — and each copy carries the two
/// `String`-building refusals with it. What that costs is not paid by calls;
/// it is paid by every instruction of every program, because the dispatch
/// body's footprint is what decides how much of the loop stays in cache.
/// See [`dispatch`]'s own note on what Phase 4 measured.
///
/// `captures` is the closure environment for a `call.closure` and `None` for
/// a `call`. The two differ in nothing else: the arguments go into the
/// callee's frame from slot 0 at the *parameter's* width, and a capture goes
/// into the slot `Function::captures` names.
#[inline(never)]
fn open_frame(
    machine: &mut Machine<'_>,
    budget: &Meter,
    base: u64,
    span: Span,
    callee: FunctionId,
    args: ArgsId,
    captures: Option<u64>,
) -> Result<u64, RuntimeError> {
    let program = machine.program;
    let target = program.function(callee);
    let list = program.arg_list(args);
    if list.len() != target.params.len() {
        return Err(wrong_arity(
            target.qualified(),
            target.params.len(),
            list.len(),
        ));
    }
    machine.admit_frame(budget, span)?;
    let callee_base = match machine.mem.push_frame(target.frame_size()) {
        Ok(base) => base,
        Err(Overflow) => return Err(machine.too_deep_error()),
    };
    let mut at = 0;
    for (arg, layout) in list.iter().zip(&target.params) {
        let width = machine.width(*layout);
        machine
            .mem
            .copy_slots(callee_base + at as u64, base + arg.slot as u64, width);
        at += width;
    }
    // The object stays reachable across every one of these reads because it
    // is named by a `Repr::Ref` slot of a frame this has not left, and
    // nothing between the read and the last write allocates.
    if let Some(object) = captures {
        let mut carried = 1;
        for capture in &program.function(callee).captures {
            let width = machine.width(capture.layout);
            machine.mem.copy_words(
                callee_base + capture.slot as u64,
                machine.mem.payload_addr(object, carried),
                width,
            );
            carried += width;
        }
    }
    Ok(callee_base)
}

/// The loop, over encoded instructions. There is one.
///
/// `id`, `base`, `pc` and `code` are locals rather than fields read through
/// the top frame on every instruction, and are written back at the two
/// points where something else looks: a stop, and a failure.
///
/// `threads` and `running` are the thread scope a `spawn` starts children in
/// and the handles onto them. They are parameters rather than fields of the
/// machine because a scoped handle borrows the scope it was started in, so
/// it cannot outlive [`Machine::drive`].
///
/// `floor` is the frame depth this turn of the loop was entered at, which a
/// `return` below is what ends it — one loop serves both a whole run and a
/// host's callback into the middle of one.
/// How many payload words a run of `bytes` bytes touches, as a unit of work.
///
/// A word is the unit because a word is what the memory moves: charging per
/// byte would price a one-word copy at eight and make a byte run eight times
/// dearer than the `Array` it shares a heap with.
#[inline]
fn words_of_bytes(bytes: i64) -> u64 {
    (bytes.max(0) as u64).div_ceil(8)
}

/// How many bytes a bulk operation moves between two safepoints.
///
/// One [`SAFEPOINT_STRIDE`] of work, expressed in bytes, so a chunk costs
/// exactly the stride and the poll that follows it is due. This is the `T` of
/// [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
/// `S + T` for a bulk operation: a cancelled or out-of-fuel run gets no
/// further than one chunk past the bound, whatever the length it was asked
/// to copy.
const BULK_CHUNK_BYTES: i64 = (SAFEPOINT_STRIDE * 8) as i64;

/// [`Inst::CopyBytes`], checked and copied in bounded chunks.
///
/// Out of line, and out of the dispatch loop's body, for the reason
/// [`crate::vm::debug`] records: this loop is sensitive to how much code sits
/// in it, not only to what that code does.
///
/// The chunking is the correctness argument rather than a refinement of it.
/// One `copy-bytes` may move far more than a stride of work, and charging for
/// all of it afterwards would let a cancelled or out-of-fuel run copy the
/// whole range first —
/// [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)
/// promises `S + T` of Cove work once a bound becomes true, not `S + T` plus
/// the length of the copy.
///
/// The caller has already `sync`ed, so a collection reached from inside here
/// walks a current frame, and `dst` and `src` are rooted by the slots this
/// read them out of.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn copy_bytes(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let dst = machine.mem.slot(base, args[0].slot);
    let dst_at = machine.mem.slot(base, args[1].slot) as i64;
    let src = machine.mem.slot(base, args[2].slot);
    let src_at = machine.mem.slot(base, args[3].slot) as i64;
    let len = machine.mem.slot(base, args[4].slot) as i64;
    if dst == 0 || src == 0 {
        return Err(refuse(machine, null_object()));
    }
    if len < 0 {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`copyBytes`'s length is `{len}`, and a copy cannot have a negative length"
            )),
        ));
    }
    if !matches!(
        program.layout(machine.mem.object_layout(dst)).shape,
        Shape::Bytes
    ) {
        return Err(refuse(
            machine,
            RuntimeError::new(
                "`copyBytes`'s destination is not a byte run under construction, and only one \
                 of those may be written into",
            ),
        ));
    }
    if !matches!(
        program.layout(machine.mem.object_layout(src)).shape,
        Shape::Str | Shape::Bytes
    ) {
        return Err(refuse(
            machine,
            RuntimeError::new(
                "`copyBytes`'s source is neither a `String` nor a byte run under construction",
            ),
        ));
    }
    let src_len = machine.mem.object_len(src) as i64;
    if src_at < 0 || src_at.checked_add(len).is_none_or(|end| end > src_len) {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`copyBytes` reads {len} byte(s) from {src_at} of a source of {src_len}"
            )),
        ));
    }
    let dst_len = machine.mem.object_len(dst) as i64;
    if dst_at < 0 || dst_at.checked_add(len).is_none_or(|end| end > dst_len) {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`copyBytes` writes {len} byte(s) to {dst_at} of a destination of {dst_len}"
            )),
        ));
    }
    // The chunks run in the direction the whole copy runs in. Splitting a
    // `memmove` into pieces does not preserve its meaning by itself: copying
    // a run's bytes to a higher offset in *itself*, front first, makes each
    // chunk overwrite the input of the next — and every chunk being correct
    // in isolation does not save it. So an overlapping forward shift is
    // chunked from the tail, which is the same reason
    // `Machine::copy_string_bytes` walks it backwards inside one chunk.
    let descending = dst == src && dst_at > src_at;
    let mut done: i64 = 0;
    while done < len {
        let take = (len - done).min(BULK_CHUNK_BYTES);
        let offset = if descending { len - done - take } else { done };
        machine.copy_string_bytes(
            dst,
            (dst_at + offset) as usize,
            src,
            (src_at + offset) as usize,
            take as usize,
        );
        machine.bulk_work += words_of_bytes(take);
        done += take;
        if machine.work() - machine.charged_work >= SAFEPOINT_STRIDE {
            machine.safepoint(budget, id, pc)?;
            machine.next_check = machine.next_question();
        }
    }
    Ok(())
}

/// [`Inst::AppendBytes`], checked, grown once and copied in bounded chunks.
///
/// Out of line and out of the dispatch loop's body for [`copy_bytes`]'s
/// reason, which is the only reason that matters here: this loop is sensitive
/// to how much code sits in it, not only to what that code does.
///
/// # What is checked, and in whose words
///
/// The bounds and the character-boundary rule are `String.sliceBytes`'s, in
/// `String.sliceBytes`'s sentences —
/// [`crate::vm::builtins::text`]'s `byte_range` is where they are written, and
/// ADR 0052 requires that `appendSlice` "checks the same bounds and UTF-8
/// boundaries as `String.sliceBytes`". Two operations that make the same
/// refusal in different words are two rules a reader has to learn.
///
/// The boundary check applies to a `String` source and not to a
/// [`Shape::Bytes`] one, because a run under construction is not claiming to be
/// text: the bytes it holds are checked once, at
/// [`Inst::FinishBuffer`](cove_ir::Inst::FinishBuffer).
///
/// # Why the growth happens once, before the first chunk
///
/// The whole range is reserved up front. A growth part way through would have
/// to copy a prefix the earlier chunks had already written into a store that is
/// about to be replaced, which is the same bytes moved twice; worse, it would
/// put an allocation inside the loop that a safepoint already makes collectable,
/// for no gain over asking for the final length at the start.
///
/// After the reservation nothing here allocates, so the chunk loop's safepoints
/// are safe for the reason [`copy_bytes`]'s are and one more: the store is
/// reachable from the owner's word 1 and the owner is a frame slot this read it
/// out of, so a collection walking mid-copy finds both ends of the copy where it
/// finds every other live reference.
///
/// The owner's length word is written **last**, after the final chunk. A run
/// stopped by a safepoint part way through therefore leaves the appended bytes
/// above the logical length, where they are spare room rather than value.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn append_bytes(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let owner = machine.mem.slot(base, args[0].slot);
    let src = machine.mem.slot(base, args[1].slot);
    let from = machine.mem.slot(base, args[2].slot) as i64;
    let to = machine.mem.slot(base, args[3].slot) as i64;
    if owner == 0 || src == 0 {
        return Err(refuse(machine, null_object()));
    }
    let buffer = machine.buffer("appendBytes", owner).map_err(|error| {
        // `Machine::buffer` reports without a span, because two of its three
        // callers are dispatch arms that have one to add.
        refuse(machine, error)
    })?;
    let is_text = match program.layout(machine.mem.object_layout(src)).shape {
        Shape::Str => true,
        Shape::Bytes => false,
        _ => return Err(refuse(
            machine,
            RuntimeError::new(
                "`appendBytes`'s source is neither a `String` nor a byte run under construction",
            ),
        )),
    };
    let len = machine.mem.object_len(src) as i64;
    // `byte_range`'s two refusals, in `byte_range`'s words.
    for (name, value) in [("from", from), ("to", to)] {
        if value < 0 || value > len {
            return Err(refuse(
                machine,
                RuntimeError::new(format!(
                    "`{name}` is `{value}`, and a byte offset into this string is 0 to {len}"
                )),
            ));
        }
    }
    if from > to {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`from` is `{from}` and `to` is `{to}`, so this range runs backwards"
            )),
        ));
    }
    if is_text {
        for (name, at) in [("from", from), ("to", to)] {
            // The end of the string is a boundary and has no byte to look at.
            if at < len && machine.byte_of(src, at as usize) & 0xC0 == 0x80 {
                return Err(refuse(
                    machine,
                    RuntimeError::new(format!(
                        "`{name}` is `{at}`, which is inside a character rather than at the \
                         start of one"
                    )),
                ));
            }
        }
    }
    // `checked_add` rather than a plain `+`, even though both operands came out
    // of `u32`-wide header lengths: a sum that wrapped would under-reserve and
    // then be written to by a loop sized from the original, which is the one
    // arithmetic mistake in here that would be a write past an object rather
    // than a wrong answer.
    let take = to - from;
    let Some(needed) = u64::from(buffer.len).checked_add(take as u64) else {
        return Err(refuse(
            machine,
            RuntimeError::new("this run has no memory left"),
        ));
    };
    let store = machine
        .reserve_bytes(&buffer, needed)
        .map_err(|error| refuse(machine, error))?;
    let mut done: i64 = 0;
    while done < take {
        let chunk = (take - done).min(BULK_CHUNK_BYTES);
        machine.copy_string_bytes(
            store,
            buffer.len as usize + done as usize,
            src,
            (from + done) as usize,
            chunk as usize,
        );
        machine.bulk_work += words_of_bytes(chunk);
        done += chunk;
        if machine.work() - machine.charged_work >= SAFEPOINT_STRIDE {
            machine.safepoint(budget, id, pc)?;
            machine.next_check = machine.next_question();
        }
    }
    machine.set_payload(buffer.owner, BUFFER_LEN, needed);
    Ok(())
}

pub(super) fn dispatch<'s, 'a>(
    machine: &mut Machine<'a>,
    encoded: &Encoded,
    budget: &Meter,
    threads: &'s Scope<'s, 'a>,
    running: &mut Vec<Option<ScopedJoinHandle<'s, Outcome>>>,
    floor: usize,
) -> Result<Vec<u64>, RuntimeError> {
    let program = machine.program;
    let top = machine.frames.last().expect("run pushed a frame");
    let mut id = top.function;
    let mut base = top.base;
    let mut base_at = machine.mem.stack_index(base);
    let mut pc = top.pc as usize;
    let mut code = encoded.function(id);

    loop {
        machine.instructions += 1;
        // One increment and one comparison, which is what this loop has
        // always been and what it measurably has to stay: `next_check` is the
        // smaller of the next safepoint and the next debug stop, and it is in
        // *instruction* coordinates so that the bulk work of ADR 0052 is
        // absorbed by the threshold rather than by a second counter here.
        // Everything inside is that loop's lines in that loop's order,
        // because a second accounting would be a second thing to keep in step
        // with ADR 0024 and ADR 0040.
        if machine.instructions >= machine.next_check {
            machine.sync(pc);
            if machine.debugger.is_some() {
                machine.ask(id, pc)?;
            }
            // Elapsed work since the last charge, not equality with a
            // multiple of it: an instruction that charges for the words it
            // moved steps *over* the multiple it would have landed on, and
            // the old condition then answered false, losing the cancellation
            // check, the fuel accounting and the collector's poll together.
            if machine.work() - machine.charged_work >= SAFEPOINT_STRIDE {
                machine.safepoint(budget, id, pc)?;
            }
            machine.next_check = machine.next_question();
        }

        let held = code[pc];
        pc += 1;

        macro_rules! fail {
            ($error:expr) => {{
                machine.sync(pc - 1);
                return Err($error.at(machine.span(id, pc - 1)));
            }};
        }

        // The three slot fields and the payload's two halves, read the way
        // ADR 0041's audit says each opcode reads them.
        macro_rules! a {
            () => {
                held.a() as Slot
            };
        }
        macro_rules! b {
            () => {
                held.b() as Slot
            };
        }
        macro_rules! c {
            () => {
                held.c() as Slot
            };
        }

        // The operator is a constant at each call site, which is the whole
        // point of one opcode per concrete operation: `int_arith`,
        // `float_arith` and `compare` are the machine's own functions, shared
        // with everything else that does arithmetic, and their inner `match`
        // folds away because the operator is known.
        macro_rules! int_op {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let y = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                // The same question `Inst::Arith` asks, for the same reason:
                // which of the two the operands are decides only what a
                // failure calls the operation.
                let duration = machine.repr(id, a!()) == Some(Repr::Duration);
                match int_arith($op, x, y, duration) {
                    Ok(value) => machine
                        .mem
                        .set_word_at(base_at + (a!()) as usize, value as u64),
                    Err(error) => fail!(error),
                }
            }};
        }
        macro_rules! float_op {
            ($op:expr) => {{
                let x = f64::from_bits(machine.mem.word_at(base_at + (b!() as usize)));
                let y = f64::from_bits(machine.mem.word_at(base_at + (c!() as usize)));
                machine
                    .mem
                    .set_slot(base, a!(), float_arith($op, x, y).to_bits());
            }};
        }
        macro_rules! arith_imm {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let duration = machine.repr(id, a!()) == Some(Repr::Duration);
                match int_arith($op, x as i64, held.payload() as i64, duration) {
                    Ok(value) => machine
                        .mem
                        .set_word_at(base_at + (a!()) as usize, value as u64),
                    Err(error) => fail!(error),
                }
            }};
        }
        macro_rules! cmp_imm {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let answer = compare($op, x.cmp(&(held.payload() as i64)));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }};
        }
        macro_rules! cmp_int {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let y = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                let answer = compare($op, x.cmp(&y));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }};
        }
        macro_rules! cmp_float {
            ($answer:expr) => {{
                let x = f64::from_bits(machine.mem.word_at(base_at + (b!() as usize)));
                let y = f64::from_bits(machine.mem.word_at(base_at + (c!() as usize)));
                #[allow(clippy::redundant_closure_call)]
                let answer = ($answer)(x, y);
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }};
        }
        macro_rules! cmp_str {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let y = machine.mem.word_at(base_at + (c!() as usize));
                let answer = compare($op, machine.compare_strings(x, y));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }};
        }
        // A `Bool` and an identity answer `Eq` and `Ne` and nothing else,
        // which is `crate::verify`'s rule: ordering either is not a question
        // the language asks. The four remaining opcodes exist because ADR
        // 0041 generates the cross product mechanically rather than
        // hand-picking the legal pairs — a hand-picked table would be a
        // second, weaker copy of the type rules — and the lowering emits
        // none of them.
        macro_rules! cmp_word {
            ($equal:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let y = machine.mem.word_at(base_at + (c!() as usize));
                machine
                    .mem
                    .set_slot(base, a!(), ((x == y) == $equal) as u64);
            }};
        }
        macro_rules! not_ordered {
            () => {{
                fail!(RuntimeError::new(
                    "this comparison is not defined for these operands"
                ))
            }};
        }
        macro_rules! entered {
            ($callee:expr, $callee_base:expr, $dst:expr) => {{
                machine.sync(pc);
                machine.frames.push(Frame {
                    function: $callee,
                    base: $callee_base,
                    pc: 0,
                    dst: $dst,
                });
                id = $callee;
                base = $callee_base;
                base_at = machine.mem.stack_index(base);
                pc = 0;
                code = encoded.function(id);
            }};
        }

        match held.opcode() {
            // ---- constants and moves ---------------------------------
            CONST_UNIT => machine.mem.set_word_at(base_at + (a!()) as usize, 0),
            CONST_BOOL | CONST_INT | CONST_FLOAT => machine
                .mem
                .set_word_at(base_at + (a!()) as usize, held.payload()),
            // The callee's dense id, written as a word — the same one store
            // `CONST_INT` makes, and no name lookup: `held.lo()` is already
            // the `FunctionId` the encoder put there.
            // A case index is written by the same arm a callee id is, and
            // that is the whole of what the machine knows about either: both
            // are one metadata number in the payload's low half, and neither
            // has a runtime representation the loop can tell from the other.
            // The distinction they carry is the verifier's and the printer's.
            FUNC_REF | CONST_TAG => machine
                .mem
                .set_word_at(base_at + (a!()) as usize, held.lo() as u64),
            // A load of a precomputed address, exactly as `CONST_INT` loads
            // a precomputed word: `Machine::for_run` placed every literal
            // before this loop's first turn, so there is nothing here that
            // can fail and nothing to `sync` before. See ADR 0045.
            STR => machine
                .mem
                .set_slot(base, a!(), machine.literal_addr(StrId(held.lo()))),
            // ADR 0001's field-wise shallow copy, and the whole of it.
            COPY => {
                let width = machine.width(LayoutId(held.lo()));
                machine
                    .mem
                    .copy_slots(base + held.a() as u64, base + held.b() as u64, width);
            }
            // The one instruction whose whole purpose is what it stops
            // happening: a reference the frame no longer needs is not a root.
            CLEAR => {
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.clear_words(base + held.a() as u64, width);
            }

            // ---- scalar operations -----------------------------------
            NEG_INT => {
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                match x.checked_neg() {
                    Some(value) => machine
                        .mem
                        .set_word_at(base_at + (a!()) as usize, value as u64),
                    None => fail!(overflowed("negation")),
                }
            }
            NEG_FLOAT => {
                let x = f64::from_bits(machine.mem.word_at(base_at + (b!() as usize)));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, (-x).to_bits());
            }

            ADD_INT => int_op!(ArithOp::Add),
            SUB_INT => int_op!(ArithOp::Sub),
            MUL_INT => int_op!(ArithOp::Mul),
            DIV_INT => int_op!(ArithOp::Div),
            REM_INT => int_op!(ArithOp::Rem),

            ADD_FLOAT => float_op!(ArithOp::Add),
            SUB_FLOAT => float_op!(ArithOp::Sub),
            MUL_FLOAT => float_op!(ArithOp::Mul),
            DIV_FLOAT => float_op!(ArithOp::Div),
            REM_FLOAT => float_op!(ArithOp::Rem),

            EQ_INT => cmp_int!(CmpOp::Eq),
            NE_INT => cmp_int!(CmpOp::Ne),
            LT_INT => cmp_int!(CmpOp::Lt),
            LE_INT => cmp_int!(CmpOp::Le),
            GT_INT => cmp_int!(CmpOp::Gt),
            GE_INT => cmp_int!(CmpOp::Ge),

            // Not `compare` over an `Ordering`: a `NaN` is unordered, so
            // `f64`'s own operators are what answer, which is also what the
            // tree-walking oracle does.
            EQ_FLOAT => cmp_float!(|x, y| x == y),
            NE_FLOAT => cmp_float!(|x: f64, y: f64| x != y),
            LT_FLOAT => cmp_float!(|x, y| x < y),
            LE_FLOAT => cmp_float!(|x, y| x <= y),
            GT_FLOAT => cmp_float!(|x, y| x > y),
            GE_FLOAT => cmp_float!(|x, y| x >= y),

            // A case index is a word and compares as one, which is the whole
            // of what `Kind.Space == Kind.Word` asks.
            EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true),
            NE_BOOL | NE_REF | NE_TAG => cmp_word!(false),
            LT_BOOL | LE_BOOL | GT_BOOL | GE_BOOL | LT_REF | LE_REF | GT_REF | GE_REF | LT_TAG
            | LE_TAG | GT_TAG | GE_TAG => not_ordered!(),

            EQ_STR => cmp_str!(CmpOp::Eq),
            NE_STR => cmp_str!(CmpOp::Ne),
            LT_STR => cmp_str!(CmpOp::Lt),
            LE_STR => cmp_str!(CmpOp::Le),
            GT_STR => cmp_str!(CmpOp::Gt),
            GE_STR => cmp_str!(CmpOp::Ge),

            ADD_INT_IMM => arith_imm!(ArithOp::Add),
            SUB_INT_IMM => arith_imm!(ArithOp::Sub),
            MUL_INT_IMM => arith_imm!(ArithOp::Mul),
            DIV_INT_IMM => arith_imm!(ArithOp::Div),
            REM_INT_IMM => arith_imm!(ArithOp::Rem),

            EQ_INT_IMM => cmp_imm!(CmpOp::Eq),
            NE_INT_IMM => cmp_imm!(CmpOp::Ne),
            LT_INT_IMM => cmp_imm!(CmpOp::Lt),
            LE_INT_IMM => cmp_imm!(CmpOp::Le),
            GT_INT_IMM => cmp_imm!(CmpOp::Gt),
            GE_INT_IMM => cmp_imm!(CmpOp::Ge),

            NOT => {
                let x = machine.mem.word_at(base_at + (b!() as usize));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, (x == 0) as u64);
            }
            INT_TO_FLOAT => {
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, (x as f64).to_bits());
            }
            FLOAT_TO_INT => {
                let x = f64::from_bits(machine.mem.word_at(base_at + (b!() as usize)));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, x as i64 as u64);
            }

            // ---- control flow ----------------------------------------
            // The displacement is `to - (pc + 1)` and `pc` is already past
            // the instruction, so this is one addition and no table.
            JUMP => pc = pc.wrapping_add_signed(held.payload() as i64 as isize),
            BRANCH_FALSE => {
                if machine.mem.word_at(base_at + (a!() as usize)) == 0 {
                    pc = pc.wrapping_add_signed(held.payload() as i64 as isize);
                }
            }
            // A switch table stays immutable program metadata with absolute
            // targets — ADR 0041's one exception to relative control flow,
            // because a table read from a `TableId` has no pc of its own.
            SWITCH => {
                let index = machine.mem.word_at(base_at + (a!() as usize)) as usize;
                let table = program.table(TableId(held.lo()));
                pc = *table.targets.get(index).unwrap_or(&table.default) as usize;
            }
            RETURN => {
                let src = a!();
                let width = machine.width(program.function(id).returns);
                let done = machine.frames.pop().expect("a frame is executing");
                match machine
                    .frames
                    .last()
                    .filter(|_| machine.frames.len() > floor)
                {
                    None => {
                        let answer = machine.mem.read_words(base + src as u64, width);
                        machine.mem.pop_frame(base);
                        return Ok(answer);
                    }
                    Some(caller) => {
                        id = caller.function;
                        let caller_base = caller.base;
                        pc = caller.pc as usize;
                        code = encoded.function(id);
                        machine.mem.copy_words(
                            caller_base + done.dst as u64,
                            base + src as u64,
                            width,
                        );
                        machine.mem.pop_frame(base);
                        base = caller_base;
                        base_at = machine.mem.stack_index(base);
                    }
                }
            }

            // ---- calls -----------------------------------------------
            CALL => {
                let dst = a!();
                let callee = FunctionId(held.lo());
                let span = machine.span(id, pc - 1);
                match open_frame(machine, budget, base, span, callee, ArgsId(held.hi()), None) {
                    Ok(callee_base) => entered!(callee, callee_base, dst),
                    Err(error) => fail!(error),
                }
            }
            // A closure call is a frame like any other. The callee is not in
            // the instruction — it is a word of the object the slot names —
            // and the captures follow the arguments into the slots
            // `Function::captures` names.
            CALL_CLOSURE => {
                let dst = a!();
                let object = machine.mem.word_at(base_at + (b!() as usize));
                let callee = match machine.callee_of(object) {
                    Ok(callee) => callee,
                    Err(error) => fail!(error),
                };
                let span = machine.span(id, pc - 1);
                match open_frame(
                    machine,
                    budget,
                    base,
                    span,
                    callee,
                    ArgsId(held.lo()),
                    Some(object),
                ) {
                    Ok(callee_base) => entered!(callee, callee_base, dst),
                    Err(error) => fail!(error),
                }
            }
            // The one instruction that leaves the machine. Everything it
            // needs out of the frame is read before the call, so the frames
            // are consistent for the length of it: a host may collect through
            // the boundary.
            CALL_HOST => {
                machine.sync(pc - 1);
                let dst = a!();
                let span = machine.span(id, pc - 1);
                match machine.call_host(
                    base,
                    HostOpId(held.lo()),
                    ArgsId(held.hi()),
                    budget,
                    span,
                    threads,
                    running,
                ) {
                    Ok(words) => {
                        for (at, word) in words.iter().enumerate() {
                            machine
                                .mem
                                .set_word_at(base_at + (dst + at as u32) as usize, *word);
                        }
                    }
                    Err(error) => fail!(error),
                }
            }
            // The same boundary, addressed to a handle rather than to a
            // module.
            CALL_RESOURCE => {
                machine.sync(pc - 1);
                let dst = a!();
                let span = machine.span(id, pc - 1);
                match machine.call_resource(
                    base,
                    b!(),
                    HostOpId(held.lo()),
                    ArgsId(held.hi()),
                    budget,
                    span,
                    threads,
                    running,
                ) {
                    Ok(words) => {
                        for (at, word) in words.iter().enumerate() {
                            machine
                                .mem
                                .set_word_at(base_at + (dst + at as u32) as usize, *word);
                        }
                    }
                    Err(error) => fail!(error),
                }
            }
            // Not a boundary, and not a frame: a builtin reads the words and
            // the objects the machine already holds and answers a value
            // location's worth of words.
            CALL_BUILTIN => {
                machine.sync(pc - 1);
                let dst = a!();
                if let Err(error) =
                    machine.call_builtin(base, dst, BuiltinId(held.lo()), ArgsId(held.hi()))
                {
                    fail!(error)
                }
            }

            // ---- the heap --------------------------------------------
            // `Len`'s three forms are three opcodes rather than a
            // discriminant in a field, so nothing is stored and nothing is
            // asked.
            ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT => {
                // `Len::Slot`'s word is read as `i64` and handed to
                // `Machine::allocate` whole, not narrowed here: a negative
                // count or one past what a `u32` header field can hold is
                // that call's to reject, the same way it rejects
                // `ALLOC_IMM`'s `Half::Count` — the other half `verify`
                // does not range-check, for want of a table to check it
                // against.
                let len = match held.opcode() {
                    ALLOC_FIXED => 0,
                    ALLOC_IMM => held.hi() as i64,
                    _ => machine.mem.word_at(base_at + (b!() as usize)) as i64,
                };
                machine.sync(pc - 1);
                match machine.allocate(LayoutId(held.lo()), len) {
                    Ok(addr) => machine.mem.set_word_at(base_at + (a!()) as usize, addr),
                    Err(error) => fail!(error),
                }
            }
            // A field of a *heap object* is a run of words at a static
            // offset. A field of an inline struct is not here at all: it is a
            // slot number the lowering computed.
            LOAD_FIELD => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                let at = held.lo();
                let width = machine.width(LayoutId(held.hi()));
                match machine.checked(addr, at, width) {
                    Ok(()) => machine.mem.copy_words(
                        base + held.a() as u64,
                        machine.mem.payload_addr(addr, at),
                        width,
                    ),
                    Err(error) => fail!(error),
                }
            }
            STORE_FIELD => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                let at = held.lo();
                let width = machine.width(LayoutId(held.hi()));
                match machine.checked(addr, at, width) {
                    Ok(()) => machine.mem.copy_words(
                        machine.mem.payload_addr(addr, at),
                        base + held.b() as u64,
                        width,
                    ),
                    Err(error) => fail!(error),
                }
            }
            // The stride is the element layout's width, so an `Array<Point>`
            // is a run of two-word elements rather than a run of addresses.
            LOAD_ELEM => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                let index = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                let width = machine.width(LayoutId(held.lo()));
                match machine.element(addr, index, width) {
                    Ok(at) => machine.mem.copy_words(
                        base + held.a() as u64,
                        machine.mem.payload_addr(addr, at),
                        width,
                    ),
                    Err(error) => fail!(error),
                }
            }
            STORE_ELEM => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                let index = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let width = machine.width(LayoutId(held.lo()));
                match machine.element(addr, index, width) {
                    Ok(at) => machine.mem.copy_words(
                        machine.mem.payload_addr(addr, at),
                        base + held.c() as u64,
                        width,
                    ),
                    Err(error) => fail!(error),
                }
            }
            // The one instruction that reaches inside a word. A `String`'s
            // payload is bytes, eight to a word, so this is a payload read, a
            // shift and a mask — and it is an instruction rather than a
            // builtin because as a builtin it measured 58 ns of which 48 was
            // the calling and 10 was the reading. See `Inst::ByteAt`.
            BYTE_AT => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                let at = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                let len = machine.mem.object_len(addr) as i64;
                if at < 0 || at >= len {
                    machine.sync(pc - 1);
                    fail!(RuntimeError::new(format!(
                        "`byteAt` is `{at}`, and a byte offset into this string is 0 to {}",
                        len - 1
                    )));
                }
                let at = at as u32;
                let word = machine.mem.payload(addr, at / 8);
                let byte = (word >> ((at % 8) * 8)) & 0xFF;
                machine.mem.set_word_at(base_at + (a!()) as usize, byte);
            }
            // ADR 0051's allocation. `Program::bytes_layout` is a
            // program-wide constant exactly as `Program::str_layout` is —
            // see `Op::AllocBytes`'s fields — so unlike `ALLOC_SLOT` there is
            // no `Half::Layout` to read out of the payload, only the length.
            // `Machine::allocate` is what rejects a negative or oversized
            // `len` through the "no memory left" refusal every other
            // allocation shares.
            ALLOC_BYTES => {
                let len = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                machine.sync(pc - 1);
                match machine.allocate(program.bytes_layout, len) {
                    Ok(addr) => machine.mem.set_word_at(base_at + (a!()) as usize, addr),
                    Err(error) => fail!(error),
                }
            }
            // One checked byte of a run under construction. Bounds and range
            // are both checked ahead of the write, the same way `BYTE_AT`
            // checks `at` ahead of its read: an out-of-range offset or an
            // out-of-range value would otherwise write past the object or
            // write a word another `Inst::FinishString` would trust as a
            // byte.
            WRITE_BYTE => {
                let bytes = machine.mem.word_at(base_at + (a!() as usize));
                if bytes == 0 {
                    fail!(null_object());
                }
                let at = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let len = machine.mem.object_len(bytes) as i64;
                if at < 0 || at >= len {
                    machine.sync(pc - 1);
                    fail!(RuntimeError::new(format!(
                        "`writeByte`'s `at` is `{at}`, and a byte offset into this run is 0 to {}",
                        len - 1
                    )));
                }
                let value = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                if !(0..=255).contains(&value) {
                    machine.sync(pc - 1);
                    fail!(RuntimeError::new(format!(
                        "`writeByte`'s value is `{value}`, and a byte is 0 to 255"
                    )));
                }
                let at = at as u32;
                let word_at = at / 8;
                let shift = (at % 8) * 8;
                let word = machine.mem.payload(bytes, word_at);
                let mask = 0xFFu64 << shift;
                machine
                    .mem
                    .set_payload(bytes, word_at, (word & !mask) | ((value as u64) << shift));
            }
            // The bulk copy ADR 0051 exists for. All five operands — `dst`,
            // `dst_at`, `src`, `src_at`, `len` — live behind the `ArgsId` in
            // the payload's low half rather than in `a`, `b` and `c`; see
            // `Inst::CopyBytes`'s doc for why.
            //
            // `Machine::copy_string_bytes` documents its caller as owning
            // every bound it copies within, so everything below the read of
            // the five words is a check this dispatch arm must make before
            // calling it — nothing past this point may fail.
            COPY_BYTES => {
                machine.sync(pc - 1);
                // The whole of this instruction lives behind one call. The
                // bounds checks and the chunk loop together are far more code
                // than a dispatch arm should put in the way of the arms around
                // it — ADR 0051 named that cost when it added the opcodes, and
                // `crate::vm::debug` measured 4.3% for a smaller body in this
                // same loop.
                let args = program.arg_list(ArgsId(held.lo()));
                copy_bytes(machine, program, budget, base, args, id, pc - 1)?;
            }
            // ADR 0051's finish: validated once, and turned into the answer
            // without copying its payload. A `Shape::Bytes` run and a
            // `Shape::Str` of the same byte length occupy the same number of
            // words — both are `len.div_ceil(8)` payload words — so this is a
            // header re-label with `spare` zero rather than an allocation and
            // a copy. Not copying the payload is the whole performance
            // argument the ADR makes.
            FINISH_STRING => {
                let bytes = machine.mem.word_at(base_at + (b!() as usize));
                if bytes == 0 {
                    fail!(null_object());
                }
                machine.sync(pc - 1);
                if !matches!(
                    program.layout(machine.mem.object_layout(bytes)).shape,
                    Shape::Bytes
                ) {
                    fail!(RuntimeError::new(
                        "`finishString` needs a byte run under construction, and this is not one"
                    ));
                }
                let text = machine.string_bytes(bytes);
                if std::str::from_utf8(&text).is_err() {
                    fail!(RuntimeError::new("this string's bytes are not valid UTF-8"));
                }
                let len = machine.mem.object_len(bytes);
                machine.relabel(bytes, program.str_layout, len, 0);
                machine.mem.set_word_at(base_at + (a!()) as usize, bytes);
            }
            // ADR 0052's four. Each arm is a read of its operands and one call,
            // for `COPY_BYTES`'s reason: the checks, the capacity arithmetic and
            // the growth are far more code than a dispatch arm should put in the
            // way of the arms around it. `Machine::alloc_buffer` documents which
            // of its two allocations happens first and why nothing is lost
            // between them.
            ALLOC_BUFFER => {
                let capacity = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                machine.sync(pc - 1);
                match machine.alloc_buffer(capacity) {
                    Ok(owner) => machine.mem.set_word_at(base_at + (a!()) as usize, owner),
                    Err(error) => fail!(error),
                }
            }
            // One checked byte at the logical length, which then becomes one
            // more. There is no `at` to bounds-check — that is the difference
            // between a buffer and `WRITE_BYTE`'s fixed run — and no capacity to
            // check either, because a full store grows.
            APPEND_BYTE => {
                let owner = machine.mem.word_at(base_at + (a!() as usize));
                let value = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                machine.sync(pc - 1);
                if let Err(error) = machine.append_byte(owner, value) {
                    fail!(error);
                }
            }
            // The bulk append. All four operands — `buffer`, `src`, `from`,
            // `to` — live behind the `ArgsId` in the payload's low half rather
            // than in `a`, `b` and `c`; see `Inst::AppendBytes`'s doc for why.
            APPEND_BYTES => {
                machine.sync(pc - 1);
                let args = program.arg_list(ArgsId(held.lo()));
                append_bytes(machine, program, budget, base, args, id, pc - 1)?;
            }
            // ADR 0052's finish: the *live prefix* validated once, and the store
            // relabelled down from its capacity to that length without copying a
            // byte. The owner is then emptied, because finishing consumes.
            FINISH_BUFFER => {
                let owner = machine.mem.word_at(base_at + (b!() as usize));
                machine.sync(pc - 1);
                match machine.finish_buffer(owner) {
                    Ok(text) => machine.mem.set_word_at(base_at + (a!()) as usize, text),
                    Err(error) => fail!(error),
                }
            }
            LEN => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                let len = machine.mem.object_len(addr) as i64;
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, len as u64);
            }
            // The other half of the header word `len` reads. What an object
            // *is* is an `Int` here, so a dispatch over it is an ordinary
            // `switch`.
            LAYOUT_OF => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                let layout = machine.mem.object_layout(addr).0 as i64;
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, layout as u64);
            }

            // ---- places ----------------------------------------------
            ADDR_OF_SLOT => {
                let word = base + held.b() as u64;
                machine.mem.set_word_at(base_at + (a!()) as usize, word);
            }
            ADDR_OF_FIELD => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                let at = held.lo();
                match machine.checked(addr, at, 1) {
                    Ok(()) => {
                        let word = machine.mem.payload_addr(addr, at);
                        machine.mem.set_word_at(base_at + (a!()) as usize, word);
                    }
                    Err(error) => fail!(error),
                }
            }
            ADDR_OF_ELEM => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                let index = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                let width = machine.width(LayoutId(held.lo()));
                match machine.element(addr, index, width) {
                    Ok(at) => {
                        let word = machine.mem.payload_addr(addr, at);
                        machine.mem.set_word_at(base_at + (a!()) as usize, word);
                    }
                    Err(error) => fail!(error),
                }
            }
            // Arithmetic and nothing else: what an address names is a value
            // location, and a value location's parts are at static offsets
            // from its first word.
            ADDR_OF_PART => {
                let word = machine.mem.word_at(base_at + (b!() as usize));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, word + held.lo() as u64);
            }
            LOAD => {
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.copy_words(base + held.a() as u64, addr, width);
            }
            STORE => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.copy_words(addr, base + held.b() as u64, width);
            }

            // ---- erasure ---------------------------------------------
            // A box holds the layout of what it carries in payload word 0 and
            // that value's words after it, so a boxed `Point` is a two-word
            // payload rather than a reference to somewhere else again.
            BOX => {
                let layout = LayoutId(held.lo());
                let width = machine.width(layout);
                machine.sync(pc - 1);
                let boxed = match machine.allocate(machine.boxed_layout(), width as i64) {
                    Ok(addr) => addr,
                    Err(error) => fail!(error),
                };
                machine.mem.set_payload(boxed, 0, layout.0 as u64);
                machine.mem.copy_words(
                    machine.mem.payload_addr(boxed, 1),
                    base + held.b() as u64,
                    width,
                );
                machine.mem.set_word_at(base_at + (a!()) as usize, boxed);
            }
            UNBOX => {
                let layout = LayoutId(held.lo());
                let addr = machine.mem.word_at(base_at + (b!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                if machine.mem.payload(addr, 0) != layout.0 as u64 {
                    fail!(RuntimeError::new(
                        "this value is not of the type it is being read as"
                    ));
                }
                let width = machine.width(layout);
                machine.mem.copy_words(
                    base + held.a() as u64,
                    machine.mem.payload_addr(addr, 1),
                    width,
                );
            }

            // ---- tasks -----------------------------------------------
            SCOPE_ENTER => {
                let named = program.string(StrId(held.lo())).clone();
                machine.scopes.push(ScopeEntry {
                    name: named,
                    tasks: Vec::new(),
                    closed: false,
                });
                // One past the index, so a `Repr::Scope` slot a zeroed frame
                // has not written names no scope.
                let word = machine.scopes.len() as u64;
                machine.mem.set_word_at(base_at + (a!()) as usize, word);
            }
            // The body reached its end, so this is the exit that waits. What
            // it answers about a failing child is a value here rather than
            // control flow.
            SCOPE_LEAVE => {
                machine.sync(pc - 1);
                let span = machine.span(id, pc - 1);
                let word = machine.mem.word_at(base_at + (a!() as usize));
                match machine.leave_scope(word, running, span) {
                    Ok(None) => machine.mem.set_word_at(base_at + (b!()) as usize, 0),
                    Ok(Some(child)) => {
                        let into = base + held.c() as u64;
                        match machine.write_child_error(child, into, LayoutId(held.lo())) {
                            Ok(()) => machine.mem.set_word_at(base_at + (b!()) as usize, 1),
                            Err(error) => fail!(error),
                        }
                    }
                    Err(error) => fail!(error),
                }
            }
            // The other exit, and the one a jump takes. Nothing is answered:
            // a scope being left early is already leaving with something to
            // say.
            SCOPE_CANCEL => {
                machine.sync(pc - 1);
                let word = machine.mem.word_at(base_at + (a!() as usize));
                match machine.scope_at(word, machine.span(id, pc - 1)) {
                    Ok(at) => machine.cancel_scope(at, running),
                    Err(error) => fail!(error),
                }
            }
            SPAWN => {
                machine.sync(pc - 1);
                let span = machine.span(id, pc - 1);
                let scope_word = machine.mem.word_at(base_at + (b!() as usize));
                let object = machine.mem.word_at(base_at + (c!() as usize));
                match machine.spawn(
                    scope_word,
                    object,
                    LayoutId(held.lo()),
                    budget,
                    span,
                    threads,
                    running,
                ) {
                    Ok(word) => machine.mem.set_word_at(base_at + (a!()) as usize, word),
                    Err(error) => fail!(error),
                }
            }
            AWAIT => {
                machine.sync(pc - 1);
                let dst = a!();
                let span = machine.span(id, pc - 1);
                let word = machine.mem.word_at(base_at + (b!() as usize));
                match machine.settle(word, LayoutId(held.lo()), running, span) {
                    Ok(words) => {
                        for (at, one) in words.iter().enumerate() {
                            machine
                                .mem
                                .set_word_at(base_at + (dst + at as u32) as usize, *one);
                        }
                    }
                    Err(error) => fail!(error),
                }
            }
            // A call to an `async fn` already ran, here, on this stack. What
            // is left is the handle.
            SETTLED => {
                machine.sync(pc - 1);
                let answer = LayoutId(held.lo());
                let words = machine
                    .mem
                    .read_words(base + held.b() as u64, machine.width(answer));
                match machine.settled(&words, answer, running) {
                    Ok(word) => machine.mem.set_word_at(base_at + (a!()) as usize, word),
                    Err(error) => fail!(error),
                }
            }
            // Asking is all it does. Whether the task stopped or had already
            // finished is known only where something waits for it.
            CANCEL => {
                let word = machine.mem.word_at(base_at + (a!() as usize));
                match machine.child_at(word, machine.span(id, pc - 1)) {
                    Ok(at) => {
                        if matches!(machine.children[at].state, ChildState::Running) {
                            machine.children[at].cancellation.cancel();
                        }
                    }
                    Err(error) => fail!(error),
                }
            }

            // ---- cells -----------------------------------------------
            // Acquire, and then an ordinary closure call and the unlock the
            // lowering emitted around it. The roots are published for the
            // length of the wait, because a task waiting for a cell cannot
            // reach a safepoint of its own.
            SHARED_LOCK => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                machine.sync(pc - 1);
                let taken = {
                    let live = Live(machine);
                    cell::lock(&machine.mem, addr, &live)
                };
                match taken {
                    Ok(()) => machine.held.push(addr),
                    Err(cell::Reentrant) => fail!(reentrant_lock()),
                }
            }
            SHARED_UNLOCK => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                if addr == 0 {
                    fail!(null_object());
                }
                cell::unlock(&machine.mem, addr);
                debug_assert_eq!(
                    machine.held.last().copied(),
                    Some(addr),
                    "a lock region is left in the order it was entered"
                );
                machine.held.pop();
            }

            // ---- failure ---------------------------------------------
            TRAP => {
                let message = program.string(StrId(held.lo())).to_string();
                fail!(RuntimeError::new(message))
            }
            // The only instruction that changes nothing the program can read.
            // The bytes are copied, because a run goes on after a failed
            // assertion and the object holding them is unreachable as soon as
            // the arm clears its slot.
            ASSERT_FAILED => {
                let addr = machine.mem.word_at(base_at + (a!() as usize));
                let text = String::from_utf8_lossy(&machine.string_bytes(addr)).into_owned();
                machine.assertion_failure = Some((machine.span(id, pc - 1), text));
            }

            // [`prepare`] refused every one of these before the machine was
            // built, so this cannot happen. It is written out because a loop
            // that trusted its own precondition silently would be a loop
            // that executed the wrong instruction when the precondition was
            // one day widened and this was not.
            _ => {
                machine.sync(pc - 1);
                return Err(refusal(program, id, pc - 1, held));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use cove_ir::{Convert as ConvertTo, Inst, Len, Shape};

    use super::super::tests::{budget, run_words, Build};
    use super::super::MIN_BUFFER_BYTES;
    use super::*;

    /// Every opcode ADR 0041 defines has an implementation.
    ///
    /// `crates/cove-cli/tests/bytecode_corpus.rs` names sixteen opcodes no
    /// program in the repository reaches, and four of them — `addr.elem`,
    /// `Convert(IntToFloat)`, `Convert(FloatToInt)` and `layout.of` — are not
    /// merely absent from the corpus: **the lowering has no site that emits
    /// three of them**, so no Cove source can reach them and neither the
    /// differential harness nor any fixture written in Cove can cover them.
    ///
    /// A program written in the IR directly is the only thing that can, which
    /// is what `super::tests::Build` is for. Before the cutover this
    /// compared the two loops against each other; there is one loop now, so
    /// what it asserts is the answer itself — a number chosen so that a
    /// misread of any of the four is a wrong number rather than a discarded
    /// one.
    #[test]
    fn the_opcodes_no_cove_source_reaches_run() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let ints = build.layout(
            "Array",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let reprs = &[
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Addr,
            Repr::Int,
            Repr::Float,
            Repr::Int,
            Repr::Int,
        ];
        let entry = build.function(
            "erased",
            &[],
            reprs,
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: ints,
                    len: Len::Count(3),
                },
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 7 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: int,
                },
                // The address of element 0, then the word through it.
                Inst::AddrOfElem {
                    dst: 3,
                    obj: 0,
                    index: 1,
                    layout: int,
                },
                Inst::Load {
                    dst: 4,
                    addr: 3,
                    layout: int,
                },
                // Out to `Float` and back, which is the only round trip that
                // reaches either `Convert`.
                Inst::Convert {
                    to: ConvertTo::IntToFloat,
                    dst: 5,
                    a: 4,
                },
                Inst::Convert {
                    to: ConvertTo::FloatToInt,
                    dst: 6,
                    a: 5,
                },
                // And what the object says it is, folded into the answer so
                // that a wrong reading is a wrong number rather than a
                // discarded one.
                Inst::LayoutOf { dst: 7, obj: 0 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 6,
                    a: 6,
                    b: 7,
                },
                Inst::Return { src: 6 },
            ],
        );
        let program = build.done();

        let answer = run_words(&program, entry, &[]).expect("the fixture runs");
        // Seven, out to `Float` and back, plus the layout the object says it
        // has: every one of the four opcodes contributes to it.
        assert_eq!(answer, vec![7 + u64::from(ints.0)]);
    }

    #[test]
    fn every_opcode_is_implemented() {
        let missing: Vec<Op> = Op::all()
            .into_iter()
            .filter(|op| !implemented(*op))
            .collect();
        assert!(
            missing.is_empty(),
            "the machine refuses {} of the {} opcodes: {missing:?}",
            missing.len(),
            Op::all().len(),
        );
    }

    // ---- ADR 0051: the byte-run instructions --------------------------

    /// A program with every function [ADR 0051](../../../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
    /// tests share, so each test builds a fixture and none builds a
    /// compiler.
    ///
    /// - `alloc_bytes_case(len) -> Ref` allocates and answers a byte run.
    /// - `write_byte_case(bytes, at, value) -> Ref` writes one byte and
    ///   answers the same run, so a caller can inspect it afterward.
    /// - `finish_string_case(bytes) -> Ref` finishes a run into a `String`.
    /// - `copy_into(dst, dst_at, src, src_at, len) -> Ref` copies and
    ///   answers `dst`, so both the successful matrix and every refusal path
    ///   run through one function.
    struct Fixture {
        program: Program,
        alloc_bytes_case: FunctionId,
        write_byte_case: FunctionId,
        finish_string_case: FunctionId,
        copy_into: FunctionId,
    }

    fn fixture() -> Fixture {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let bytes = build.bytes_layout();

        let alloc_bytes_case = build.function(
            "alloc_bytes_case",
            &[int],
            &[Repr::Int, Repr::Ref],
            bytes,
            vec![Inst::AllocBytes { dst: 1, len: 0 }, Inst::Return { src: 1 }],
        );
        let write_byte_case = build.function(
            "write_byte_case",
            &[bytes, int, int],
            &[Repr::Ref, Repr::Int, Repr::Int],
            bytes,
            vec![
                Inst::WriteByte {
                    bytes: 0,
                    at: 1,
                    value: 2,
                },
                Inst::Return { src: 0 },
            ],
        );
        let finish_string_case = build.function(
            "finish_string_case",
            &[bytes],
            &[Repr::Ref],
            bytes,
            vec![
                Inst::FinishString { dst: 0, bytes: 0 },
                Inst::Return { src: 0 },
            ],
        );
        let args = build.args(&[(0, bytes), (1, int), (2, bytes), (3, int), (4, int)]);
        let copy_into = build.function(
            "copy_into",
            &[bytes, int, bytes, int, int],
            &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            bytes,
            vec![Inst::CopyBytes { args }, Inst::Return { src: 0 }],
        );
        let program = build.done();
        Fixture {
            program,
            alloc_bytes_case,
            write_byte_case,
            finish_string_case,
            copy_into,
        }
    }

    /// `AllocBytes`, a sequence of `WriteByte`s and `FinishString`, chained
    /// by hand in one function, answer the expected string — and the result
    /// is `eq.str`-equal to the same text written directly, padding of the
    /// last partial word included. That last part is the one a `relabel`
    /// leaving the tail of the old `Bytes` allocation dirty would fail: the
    /// text is thirteen bytes, so the last of its two payload words is only
    /// five bytes full.
    #[test]
    fn alloc_write_finish_answers_the_expected_string() {
        let mut build = Build::default();
        build.string_layout();
        let bytes = build.bytes_layout();
        let text = "Hello, World!";
        assert_eq!(text.len(), 13, "a length whose last word is partial");
        let mut code = vec![Inst::Int {
            dst: 1,
            value: text.len() as i64,
        }];
        code.push(Inst::AllocBytes { dst: 0, len: 1 });
        for (at, byte) in text.bytes().enumerate() {
            code.push(Inst::Int {
                dst: 2,
                value: at as i64,
            });
            code.push(Inst::Int {
                dst: 3,
                value: byte as i64,
            });
            code.push(Inst::WriteByte {
                bytes: 0,
                at: 2,
                value: 3,
            });
        }
        code.push(Inst::FinishString { dst: 0, bytes: 0 });
        code.push(Inst::Return { src: 0 });
        let entry = build.function(
            "round_trip",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            bytes,
            code,
        );
        let program = build.done();

        let mut machine = Machine::new(&program, 1 << 16);
        let finished = machine.run(entry, &[], &budget()).unwrap()[0];
        assert_eq!(
            machine.object_layout(finished),
            program.str_layout,
            "finishing relabels the run to `String` in place"
        );
        assert_eq!(
            String::from_utf8(machine.string_bytes(finished)).unwrap(),
            text
        );

        let direct = machine.new_string(text).unwrap();
        assert_eq!(machine.object_len(finished), machine.object_len(direct));
        for at in 0..machine.object_len(direct).div_ceil(8) {
            assert_eq!(
                machine.payload(finished, at),
                machine.payload(direct, at),
                "word {at}: a finished run and a written string must be the same words, \
                 padding included"
            );
        }
    }

    #[test]
    fn alloc_bytes_of_zero_length_works_without_error() {
        let Fixture {
            program,
            alloc_bytes_case,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let addr = machine.run(alloc_bytes_case, &[0], &budget()).unwrap()[0];
        assert_ne!(addr, 0);
        assert_eq!(machine.object_layout(addr), program.bytes_layout);
        assert_eq!(machine.object_len(addr), 0);
    }

    #[test]
    fn alloc_bytes_of_a_negative_length_is_the_shared_no_memory_refusal() {
        let Fixture {
            program,
            alloc_bytes_case,
            ..
        } = fixture();
        let error = run_words(&program, alloc_bytes_case, &[(-1i64) as u64]).unwrap_err();
        assert_eq!(error.message, "this run has no memory left");
    }

    #[test]
    fn write_byte_refuses_a_null_run() {
        let Fixture {
            program,
            write_byte_case,
            ..
        } = fixture();
        let error = run_words(&program, write_byte_case, &[0, 0, 0]).unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn write_byte_refuses_an_out_of_range_at() {
        let Fixture {
            program,
            alloc_bytes_case,
            write_byte_case,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let addr = machine.run(alloc_bytes_case, &[3], &budget()).unwrap()[0];
        for at in [3u64, (-1i64) as u64] {
            let error = machine
                .run(write_byte_case, &[addr, at, 65], &budget())
                .unwrap_err();
            assert!(
                error.message.contains("writeByte") && error.message.contains("byte offset"),
                "{at}: {}",
                error.message
            );
        }
    }

    #[test]
    fn write_byte_refuses_a_value_above_255() {
        let Fixture {
            program,
            alloc_bytes_case,
            write_byte_case,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let addr = machine.run(alloc_bytes_case, &[3], &budget()).unwrap()[0];
        for value in [256u64, (-1i64) as u64] {
            let error = machine
                .run(write_byte_case, &[addr, 0, value], &budget())
                .unwrap_err();
            assert!(
                error.message.contains("writeByte") && error.message.contains("0 to 255"),
                "{value}: {}",
                error.message
            );
        }
    }

    #[test]
    fn finish_string_refuses_a_null_run() {
        let Fixture {
            program,
            finish_string_case,
            ..
        } = fixture();
        let error = run_words(&program, finish_string_case, &[0]).unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn finish_string_refuses_a_value_that_is_not_a_byte_run() {
        let Fixture {
            program,
            finish_string_case,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let string = machine.new_string("already a string").unwrap();
        let error = machine
            .run(finish_string_case, &[string], &budget())
            .unwrap_err();
        assert!(error.message.contains("byte run"), "{}", error.message);
    }

    #[test]
    fn finish_string_refuses_invalid_utf8() {
        let Fixture {
            program,
            alloc_bytes_case,
            write_byte_case,
            finish_string_case,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let addr = machine.run(alloc_bytes_case, &[1], &budget()).unwrap()[0];
        // 0xFF is not a valid UTF-8 lead byte on its own.
        machine
            .run(write_byte_case, &[addr, 0, 0xFF], &budget())
            .unwrap();
        let error = machine
            .run(finish_string_case, &[addr], &budget())
            .unwrap_err();
        assert_eq!(error.message, "this string's bytes are not valid UTF-8");
    }

    #[test]
    fn copy_bytes_refuses_a_null_destination() {
        let Fixture {
            program, copy_into, ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let src = machine.new_string("source").unwrap();
        let error = machine
            .run(copy_into, &[0, 0, src, 0, 3], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn copy_bytes_refuses_a_null_source() {
        let Fixture {
            program,
            alloc_bytes_case,
            copy_into,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.run(alloc_bytes_case, &[3], &budget()).unwrap()[0];
        let error = machine
            .run(copy_into, &[dst, 0, 0, 0, 3], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn copy_bytes_refuses_a_negative_length() {
        let Fixture {
            program,
            alloc_bytes_case,
            copy_into,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.run(alloc_bytes_case, &[3], &budget()).unwrap()[0];
        let src = machine.new_string("abc").unwrap();
        let error = machine
            .run(copy_into, &[dst, 0, src, 0, (-1i64) as u64], &budget())
            .unwrap_err();
        assert!(
            error.message.contains("negative length"),
            "{}",
            error.message
        );
    }

    #[test]
    fn copy_bytes_refuses_a_string_destination() {
        let Fixture {
            program, copy_into, ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.new_string("already a string").unwrap();
        let src = machine.new_string("abc").unwrap();
        let error = machine
            .run(copy_into, &[dst, 0, src, 0, 3], &budget())
            .unwrap_err();
        assert!(error.message.contains("destination"), "{}", error.message);
    }

    #[test]
    fn copy_bytes_refuses_an_out_of_range_source() {
        let Fixture {
            program,
            alloc_bytes_case,
            copy_into,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.run(alloc_bytes_case, &[10], &budget()).unwrap()[0];
        let src = machine.new_string("abc").unwrap();
        let error = machine
            .run(copy_into, &[dst, 0, src, 2, 5], &budget())
            .unwrap_err();
        assert!(
            error.message.contains("reads") && error.message.contains("source"),
            "{}",
            error.message
        );
    }

    #[test]
    fn copy_bytes_refuses_an_out_of_range_destination() {
        let Fixture {
            program,
            alloc_bytes_case,
            copy_into,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.run(alloc_bytes_case, &[3], &budget()).unwrap()[0];
        let src = machine.new_string("abcdef").unwrap();
        let error = machine
            .run(copy_into, &[dst, 2, src, 0, 5], &budget())
            .unwrap_err();
        assert!(
            error.message.contains("writes") && error.message.contains("destination"),
            "{}",
            error.message
        );
    }

    /// `CopyBytes` from a `String` source and from another `Shape::Bytes`
    /// run, at aligned and unaligned `dst_at`/`src_at`, agrees with Rust's
    /// own byte-slicing of the same data — including the zero-length copy,
    /// which is the one case that touches no byte at all.
    #[test]
    fn copy_bytes_agrees_with_rust_at_every_alignment() {
        let Fixture {
            program,
            alloc_bytes_case,
            copy_into,
            ..
        } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let text = "abcdefghijklmnopqrstuvwxyz";
        let as_string = machine.new_string(text).unwrap();
        let as_bytes_run = machine
            .allocate(program.bytes_layout, text.len() as i64)
            .unwrap();
        machine.write_bytes(as_bytes_run, text.as_bytes());

        const DST_LEN: usize = 16;
        let cases: &[(usize, usize, usize)] = &[
            (0, 0, 5),
            (3, 0, 5),
            (0, 7, 9),
            (8, 10, 6),
            (1, 1, 10),
            (0, 0, 0),
            (5, 5, 0),
        ];
        for &source in &[as_string, as_bytes_run] {
            for &(dst_at, src_at, len) in cases {
                let dst = machine
                    .run(alloc_bytes_case, &[DST_LEN as u64], &budget())
                    .unwrap()[0];
                let result = machine
                    .run(
                        copy_into,
                        &[dst, dst_at as u64, source, src_at as u64, len as u64],
                        &budget(),
                    )
                    .unwrap()[0];
                assert_eq!(result, dst);
                let mut want = vec![0u8; DST_LEN];
                want[dst_at..dst_at + len].copy_from_slice(&text.as_bytes()[src_at..src_at + len]);
                assert_eq!(
                    machine.string_bytes(result),
                    want,
                    "source {source} dst_at={dst_at} src_at={src_at} len={len}"
                );
            }
        }
    }

    // --- ADR 0052: bulk work is bounded work -------------------------------

    /// A run that copies `bytes` bytes in one `copy-bytes`, with a fixture
    /// whose only other instructions are the two allocations and a return.
    fn one_big_copy(bytes: i64) -> (cove_ir::Program, cove_ir::FunctionId) {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let run = build.bytes_layout();
        let copy = build.args(&[(1, run), (3, int), (2, run), (3, int), (0, int)]);
        let entry = build.function(
            "copier",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
            run,
            vec![
                Inst::AllocBytes { dst: 1, len: 0 },
                Inst::AllocBytes { dst: 2, len: 0 },
                Inst::Int { dst: 3, value: 0 },
                Inst::CopyBytes { args: copy },
                Inst::Return { src: 1 },
            ],
        );
        let _ = bytes;
        (build.done(), entry)
    }

    /// **A copy is charged for the words it moves, and overspends its fuel by
    /// less than one chunk plus one stride — not by the length of the copy.**
    ///
    /// This is `responsiveness.rs`'s
    /// `an_exhausted_fuel_budget_is_overspent_by_less_than_one_gathering`
    /// for an instruction no Cove source can reach yet, and it is the
    /// assertion that makes the proportional charge honest. Asserting only
    /// that the run *stops* would pass just as well for a copy that ran to
    /// the end of a megabyte first, which is what ADR 0040's `S + T` forbids.
    #[test]
    fn a_bulk_copy_overspends_its_fuel_by_less_than_one_chunk() {
        const BYTES: i64 = 1 << 20;
        let (program, entry) = one_big_copy(BYTES);
        let words = (BYTES as u64).div_ceil(8);
        for limit in [1_024u64, 8_192, 40_000] {
            let budget = crate::budget::Budget::new(crate::budget::Limits {
                fuel: Some(limit),
                ..crate::budget::Limits::default()
            });
            let mut machine = Machine::new(&program, 1 << 22);
            let error = machine
                .run(entry, &[BYTES as u64], &budget.meter())
                .expect_err("a copy past its fuel is stopped");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);

            // The bound: one chunk of work, plus the stride the loop may
            // gather before it looks. Emphatically not `words`.
            let bound = limit + words_of_bytes(BULK_CHUNK_BYTES) + SAFEPOINT_STRIDE;
            let spent = budget.fuel_spent();
            assert!(
                spent <= bound,
                "a {BYTES}-byte copy under a fuel limit of {limit} spent {spent}, \
                 past the bound of {bound}; the whole copy would have been {words}"
            );
            assert!(
                spent < words,
                "and it must not have copied the whole {words} words first"
            );
        }
    }

    /// **A cancelled run stops inside a large copy rather than after it.**
    ///
    /// The flag is set before the run begins, so the first safepoint the copy
    /// reaches is the one that answers. Without chunking there is no safepoint
    /// until the copy has finished, and the assertion on `work` is what tells
    /// the two apart: a megabyte is 131,072 words, and a run that stopped
    /// promptly has done a few thousand.
    #[test]
    fn a_cancelled_run_stops_inside_a_large_copy() {
        const BYTES: i64 = 1 << 20;
        let (program, entry) = one_big_copy(BYTES);
        let budget = crate::budget::Budget::new(crate::budget::Limits::default());
        budget.cancellation().cancel();
        let mut machine = Machine::new(&program, 1 << 22);
        let error = machine
            .run(entry, &[BYTES as u64], &budget.meter())
            .expect_err("a cancelled run does not answer");
        assert_eq!(error.outcome, crate::trace::RunOutcome::Cancelled);
        let bound = words_of_bytes(BULK_CHUNK_BYTES) + SAFEPOINT_STRIDE;
        assert!(
            machine.work() <= bound,
            "a cancelled run did {} words of work inside a {}-word copy, past \
             the bound of {bound}",
            machine.work(),
            (BYTES as u64).div_ceil(8)
        );
    }

    /// **A collection with a half-filled run live keeps it, and keeps every
    /// byte already written into it.**
    ///
    /// The heap is small and the run allocates garbage on purpose, so the
    /// allocation between the two copies *must* collect — and the assertion on
    /// `collections` is what makes this test mean anything. Without it the test
    /// passes when no collection happens at all, which is what the first
    /// version of it did.
    ///
    /// The collection lands between two `copy-bytes` rather than inside one,
    /// and that is not a weaker test than it sounds: it is the only place a
    /// single-task run can put one. `Memory::poll` collects when another task
    /// has *requested* a stop-the-world, and a copy allocates nothing, so
    /// nothing raises that request mid-copy here — the chunk poll is where
    /// this task would join a collection another task asked for. What the
    /// property needs either way is that a `Shape::Bytes` run half full of
    /// bytes is traced as a live object and found through the frame slot
    /// holding it, and that is what this walks.
    #[test]
    fn a_collection_with_a_half_filled_run_live_keeps_every_byte_written() {
        const BYTES: i64 = 1024;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let str_layout = build.string_layout();
        let run = build.bytes_layout();
        // s1 is the run being filled; s2 is the source; s5 is garbage,
        // reallocated on every turn of a loop so the heap has to collect.
        let first = build.args(&[(1, run), (3, int), (2, run), (3, int), (4, int)]);
        let second = build.args(&[(1, run), (4, int), (2, run), (4, int), (4, int)]);
        let entry = build.function(
            "half",
            &[],
            &[
                Repr::Int,
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
            ],
            str_layout,
            vec![
                Inst::Int {
                    dst: 0,
                    value: BYTES,
                },
                Inst::AllocBytes { dst: 1, len: 0 },
                Inst::AllocBytes { dst: 2, len: 0 },
                Inst::Int { dst: 3, value: 0 },
                Inst::Int {
                    dst: 4,
                    value: BYTES / 2,
                },
                // The first half, so the run is half written from here on.
                Inst::CopyBytes { args: first },
                // Garbage, cleared between allocations so the previous one
                // is unreachable when the next is asked for. The heap holds
                // the two runs and one spare, so every allocation after the
                // first has to reclaim before it fits.
                Inst::AllocBytes { dst: 5, len: 0 },
                Inst::Clear {
                    slot: 5,
                    layout: run,
                },
                Inst::AllocBytes { dst: 5, len: 0 },
                Inst::Clear {
                    slot: 5,
                    layout: run,
                },
                Inst::AllocBytes { dst: 5, len: 0 },
                Inst::Clear {
                    slot: 5,
                    layout: run,
                },
                // And the second half, into a run a collection has now walked.
                Inst::CopyBytes { args: second },
                Inst::FinishString { dst: 1, bytes: 1 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        // Two runs of 129 words each and room for about one more, so the
        // second piece of garbage cannot be handed out until the first is
        // reclaimed — which is what makes the collection certain rather than
        // merely possible.
        let mut machine = Machine::new(&program, 400);
        let before = machine.collected().collections;
        let answer = machine
            .run(entry, &[], &budget())
            .expect("the run answers a string");
        let after = machine.collected().collections;
        assert!(
            after > before,
            "this fixture exists to collect with a half-filled run live, and \
             it collected {} time(s)",
            after - before
        );
        let text = answer[0];
        assert_eq!(machine.object_len(text) as i64, BYTES);
        assert_eq!(machine.string_bytes(text), vec![0u8; BYTES as usize]);
    }

    /// **An overlapping copy answers the source as it was, in both directions
    /// and across chunk boundaries.**
    ///
    /// `Inst::CopyBytes` admits a `Shape::Bytes` source, so `src` and `dst` may
    /// be one run and the ranges may overlap. A range copy means `memmove`: a
    /// forward shift has to be walked from the tail, or each write lands on a
    /// byte the copy has not read yet.
    ///
    /// Chunking is the reason the lengths here are what they are. Splitting a
    /// `memmove` into chunks does not preserve its meaning by itself — chunk
    /// each piece correctly, front first, and the pieces still overwrite one
    /// another. So every case below is longer than `BULK_CHUNK_BYTES` and each
    /// overlap straddles a chunk boundary, which is exactly what a
    /// per-chunk-only fix passes and this does not.
    #[test]
    fn an_overlapping_copy_moves_bytes_as_memmove_does() {
        const BYTES: i64 = 3 * BULK_CHUNK_BYTES;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let run = build.bytes_layout();
        // shift(run, dst_at, src_at, len) over one object.
        let args = build.args(&[(0, run), (1, int), (0, run), (2, int), (3, int)]);
        let entry = build.function(
            "shift",
            &[run, int, int, int],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            run,
            vec![Inst::CopyBytes { args }, Inst::Return { src: 0 }],
        );
        let program = build.done();

        let pattern: Vec<u8> = (0..BYTES).map(|n| (n % 251) as u8).collect();
        // Forward and backward shifts, each crossing at least one chunk edge.
        for (dst_at, src_at, len) in [
            (BULK_CHUNK_BYTES + 3, 0i64, 2 * BULK_CHUNK_BYTES - 3),
            (0, BULK_CHUNK_BYTES + 3, 2 * BULK_CHUNK_BYTES - 3),
            (9, 1, 2 * BULK_CHUNK_BYTES),
            (1, 9, 2 * BULK_CHUNK_BYTES),
            (BULK_CHUNK_BYTES, BULK_CHUNK_BYTES - 1, BULK_CHUNK_BYTES + 1),
        ] {
            let mut machine = Machine::new(&program, 1 << 16);
            let obj = machine
                .allocate(program.bytes_layout, BYTES)
                .expect("a run fits");
            machine.write_bytes(obj, &pattern);
            machine
                .run(
                    entry,
                    &[obj, dst_at as u64, src_at as u64, len as u64],
                    &budget(),
                )
                .expect("a copy within its bounds answers");

            let mut want = pattern.clone();
            want.copy_within(src_at as usize..(src_at + len) as usize, dst_at as usize);
            assert_eq!(
                machine.string_bytes(obj),
                want,
                "copy_within({src_at}..{}, {dst_at}) over {BYTES} bytes",
                src_at + len
            );
        }
    }

    // --- ADR 0051: the two cases the sweep above cannot make ----------

    /// `Inst::WriteByte`s that write `text` into the run at `bytes`, one
    /// byte at a time, using `at` and `value` as scratch slots.
    fn write_text(bytes: Slot, at: Slot, value: Slot, text: &[u8]) -> Vec<Inst> {
        let mut code = Vec::new();
        for (index, byte) in text.iter().enumerate() {
            code.push(Inst::Int {
                dst: at,
                value: index as i64,
            });
            code.push(Inst::Int {
                dst: value,
                value: i64::from(*byte),
            });
            code.push(Inst::WriteByte { bytes, at, value });
        }
        code
    }

    /// The catch for a `relabel` that left the tail of the last word dirty:
    /// seven bytes is short of a whole word, so the top byte of that word is
    /// padding, and this compares payload words rather than only the text.
    #[test]
    fn a_finished_string_matches_the_same_text_written_directly_padding_included() {
        let text: &[u8] = b"hello!!";
        let mut build = Build::default();
        let str_layout = build.string_layout();
        build.bytes_layout();
        let mut code = vec![
            Inst::Int {
                dst: 0,
                value: text.len() as i64,
            },
            Inst::AllocBytes { dst: 1, len: 0 },
        ];
        code.extend(write_text(1, 2, 3, text));
        code.push(Inst::FinishString { dst: 1, bytes: 1 });
        code.push(Inst::Return { src: 1 });
        let f = build.function(
            "build",
            &[],
            &[Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            str_layout,
            code,
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let words = machine.run(f, &[], &budget()).unwrap();
        let from_bytes = words[0];
        let direct = machine
            .new_string(std::str::from_utf8(text).unwrap())
            .unwrap();
        let len = machine.object_len(from_bytes);
        assert_eq!(len, machine.object_len(direct));
        for word in 0..len.div_ceil(8) {
            assert_eq!(
                machine.payload(from_bytes, word),
                machine.payload(direct, word),
                "payload word {word} should match, padding included"
            );
        }
    }

    #[test]
    fn a_zero_length_run_finishes_to_the_empty_string() {
        let mut build = Build::default();
        let str_layout = build.string_layout();
        build.bytes_layout();
        let f = build.function(
            "empty",
            &[],
            &[Repr::Int, Repr::Ref],
            str_layout,
            vec![
                Inst::Int { dst: 0, value: 0 },
                Inst::AllocBytes { dst: 1, len: 0 },
                Inst::FinishString { dst: 1, bytes: 1 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let words = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(machine.string_bytes(words[0]), Vec::<u8>::new());
        assert_eq!(machine.object_len(words[0]), 0);
    }

    // --- ADR 0052: the byte-buffer instructions ----------------------------

    /// A program with every function ADR 0052's tests share, so each test
    /// builds a fixture and none builds a compiler.
    ///
    /// - `alloc(capacity) -> Ref` answers a new owner.
    /// - `append_byte(buffer, value) -> Ref` answers the same owner, so a
    ///   caller can keep appending to what it got back and watch the address
    ///   not move.
    /// - `append_bytes(buffer, src, from, to) -> Ref` likewise.
    /// - `finish(buffer) -> Ref` answers the `String`.
    struct Buffers {
        program: Program,
        alloc: FunctionId,
        append_byte: FunctionId,
        append_bytes: FunctionId,
        finish: FunctionId,
    }

    fn buffers() -> Buffers {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let str_layout = build.string_layout();
        let bytes = build.bytes_layout();
        let owner = build.buffer_layout();

        let alloc = build.function(
            "alloc",
            &[int],
            &[Repr::Int, Repr::Ref],
            owner,
            vec![
                Inst::AllocBuffer {
                    dst: 1,
                    capacity: 0,
                },
                Inst::Return { src: 1 },
            ],
        );
        let append_byte = build.function(
            "append_byte",
            &[owner, int],
            &[Repr::Ref, Repr::Int],
            owner,
            vec![
                Inst::AppendByte {
                    buffer: 0,
                    value: 1,
                },
                Inst::Return { src: 0 },
            ],
        );
        let args = build.args(&[(0, owner), (1, bytes), (2, int), (3, int)]);
        let append_bytes = build.function(
            "append_bytes",
            &[owner, bytes, int, int],
            &[Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            owner,
            vec![Inst::AppendBytes { args }, Inst::Return { src: 0 }],
        );
        let finish = build.function(
            "finish",
            &[owner],
            &[Repr::Ref],
            str_layout,
            vec![
                Inst::FinishBuffer { dst: 0, buffer: 0 },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        Buffers {
            program,
            alloc,
            append_byte,
            append_bytes,
            finish,
        }
    }

    /// **The words a finish gives back are handed out again.**
    ///
    /// `spare` is the whole reason `finish-buffer` passes anything to
    /// `relabel` beyond the new length: a buffer that reserved 4 KB and kept
    /// eight bytes of it is holding 510 words the run has no further use for.
    /// The heap here is a little over one such store, so the second buffer
    /// only fits if the first one's tail really was released.
    #[test]
    fn the_capacity_a_finish_gives_back_is_allocated_again() {
        let f = buffers();
        // One 4 KB store is 513 words; two do not fit in this heap.
        let mut machine = Machine::new(&f.program, 700);
        let owner = machine.run(f.alloc, &[4096], &budget()).unwrap()[0];
        for byte in b"hello!!!" {
            machine
                .run(f.append_byte, &[owner, u64::from(*byte)], &budget())
                .unwrap();
        }
        let text = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        assert_eq!(machine.string_bytes(text), b"hello!!!".to_vec());
        // The same reservation again. Without the tail back this is
        // "this run has no memory left".
        let second = machine
            .run(f.alloc, &[4096], &budget())
            .expect("the tail the finish released is available again")[0];
        assert_ne!(second, 0);
    }

    /// A buffer of `capacity`, and the bytes of `text` appended one at a time.
    fn built(machine: &mut Machine<'_>, f: &Buffers, capacity: i64, text: &[u8]) -> u64 {
        let owner = machine
            .run(f.alloc, &[capacity as u64], &budget())
            .expect("a buffer fits")[0];
        for byte in text {
            let answered = machine
                .run(f.append_byte, &[owner, u64::from(*byte)], &budget())
                .expect("an append fits")[0];
            assert_eq!(answered, owner, "an append must not move the owner");
        }
        owner
    }

    #[test]
    fn a_buffer_of_zero_capacity_is_allocated_and_answers_the_empty_string() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        assert_ne!(owner, 0);
        assert_eq!(machine.object_layout(owner), f.program.buffer_layout);
        // Zero capacity is a hint of nothing, not a store of nothing: the floor
        // is what the allocator is asked for.
        let store = machine.payload(owner, 1);
        assert_ne!(store, 0);
        assert_eq!(machine.object_layout(store), f.program.bytes_layout);
        assert_eq!(u64::from(machine.object_len(store)), MIN_BUFFER_BYTES);
        assert_eq!(machine.payload(owner, 0), 0, "and it holds nothing yet");

        let text = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        assert_eq!(machine.object_layout(text), f.program.str_layout);
        assert_eq!(machine.object_len(text), 0);
        assert_eq!(machine.string_bytes(text), Vec::<u8>::new());
    }

    #[test]
    fn an_empty_buffer_finishes_to_the_empty_string_and_consumes_its_owner() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        let text = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        assert_eq!(machine.string_bytes(text), Vec::<u8>::new());
        // `freeze()`'s ending, for a buffer: length zero and store null.
        assert_eq!(machine.payload(owner, 0), 0);
        assert_eq!(machine.payload(owner, 1), 0);
        let error = machine.run(f.finish, &[owner], &budget()).unwrap_err();
        assert!(
            error.message.contains("already consumed"),
            "{}",
            error.message
        );
        // And the heap is still a walkable sequence of objects afterwards. A
        // finish of an empty buffer relabels a store of `MIN_BUFFER_BYTES` down
        // to nothing, which is the largest `spare` a finish can release relative
        // to what it keeps — so a free block written one word wrong would leave
        // the sweep walking into the middle of an object, and this is where that
        // fails rather than in whatever allocates next.
        machine.collect();
    }

    #[test]
    fn a_negative_capacity_is_the_shared_no_memory_refusal() {
        let f = buffers();
        let error = run_words(&f.program, f.alloc, &[(-1i64) as u64]).unwrap_err();
        assert_eq!(error.message, "this run has no memory left");
    }

    /// **The owner's address does not change when the store does.**
    ///
    /// This is the property the whole design exists for — ADR 0052's "if the
    /// object itself moves when it grows, every alias and `var` address to it
    /// goes stale" — so it is asserted directly rather than inferred from the
    /// answer being right. A capacity of zero gives a store of
    /// `MIN_BUFFER_BYTES`, and 200 appends double it five times, so the store
    /// address is asserted to have actually moved as well: a test that watched
    /// an owner not move while nothing grew would pass for the wrong reason.
    #[test]
    fn an_owner_keeps_its_address_across_several_growths() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        let first_store = machine.payload(owner, 1);
        let mut stores = vec![first_store];
        for at in 0..200u64 {
            let answered = machine
                .run(f.append_byte, &[owner, b'a' as u64 + at % 26], &budget())
                .unwrap()[0];
            assert_eq!(answered, owner, "the owner moved at append {at}");
            assert_eq!(
                machine.payload(owner, 0),
                at + 1,
                "and its length is what was appended"
            );
            let store = machine.payload(owner, 1);
            if store != *stores.last().unwrap() {
                stores.push(store);
            }
        }
        assert!(
            stores.len() >= 5,
            "200 appends from a floor of {MIN_BUFFER_BYTES} should have grown \
             several times, and grew {} time(s)",
            stores.len() - 1
        );
        let want: Vec<u8> = (0..200).map(|at| b'a' + (at % 26) as u8).collect();
        let text = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        assert_eq!(machine.string_bytes(text), want);
    }

    /// **An underestimated capacity grows rather than failing.**
    ///
    /// ADR 0052's "capacity is a performance hint: exceeding it grows rather
    /// than changes the program's result", which is the whole reason capacity
    /// is not an `Array` length.
    #[test]
    fn an_underestimated_capacity_grows_and_answers_the_same_text() {
        let f = buffers();
        let text = b"the estimate was three";
        for capacity in [0i64, 1, 3, 21, 22, 64] {
            let mut machine = Machine::new(&f.program, 1 << 16);
            let owner = built(&mut machine, &f, capacity, text);
            let answer = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
            assert_eq!(
                machine.string_bytes(answer),
                text.to_vec(),
                "capacity {capacity}"
            );
        }
    }

    /// **A finished String is the same payload words as the same text written
    /// directly, padding included — after a growth.**
    ///
    /// The catch for a bad `spare` or a dirty tail. Twenty bytes from a floor
    /// of sixteen forces one growth, and twenty bytes is two whole payload
    /// words and four bytes of a third — so the top four bytes of the last word
    /// are padding that a growth copy is in a position to have dirtied.
    /// `eq.str` compares payload words, so a string unequal here is a string
    /// unequal to itself written another way.
    #[test]
    fn a_finished_string_matches_the_same_text_written_directly_after_a_growth() {
        let f = buffers();
        let text = "twenty bytes exactly";
        assert_eq!(text.len(), 20, "two whole words and part of a third");
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = built(&mut machine, &f, 0, text.as_bytes());
        assert!(
            machine.object_len(machine.payload(owner, 1)) > 20,
            "the store grew past the length, so there is a tail to be dirty"
        );
        let built = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        let direct = machine.new_string(text).unwrap();
        assert_eq!(machine.object_len(built), machine.object_len(direct));
        for word in 0..machine.object_len(direct).div_ceil(8) {
            assert_eq!(
                machine.payload(built, word),
                machine.payload(direct, word),
                "payload word {word} should match, padding included"
            );
        }
    }

    /// The same property for the bulk path, which is the one that copies whole
    /// words rather than writing single bytes.
    #[test]
    fn a_bulk_append_leaves_the_tail_of_the_last_word_zero() {
        let f = buffers();
        let text = "twenty bytes exactly";
        let mut machine = Machine::new(&f.program, 1 << 16);
        let src = machine.new_string(text).unwrap();
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        machine
            .run(
                f.append_bytes,
                &[owner, src, 0, text.len() as u64],
                &budget(),
            )
            .unwrap();
        let built = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        let direct = machine.new_string(text).unwrap();
        for word in 0..machine.object_len(direct).div_ceil(8) {
            assert_eq!(
                machine.payload(built, word),
                machine.payload(direct, word),
                "payload word {word} should match, padding included"
            );
        }
    }

    #[test]
    fn finishing_refuses_invalid_utf8() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        // 0xFF is not a valid UTF-8 lead byte on its own.
        let owner = built(&mut machine, &f, 0, &[b'o', b'k', 0xFF]);
        let error = machine.run(f.finish, &[owner], &budget()).unwrap_err();
        assert_eq!(error.message, "this string's bytes are not valid UTF-8");
    }

    /// **Only the live prefix is validated.**
    ///
    /// The store is longer than the length after a growth, and the bytes above
    /// the length are spare room. A `finish` that validated the whole store
    /// would read zero bytes past the text and still pass — so this one writes
    /// a byte *into* the spare room behind the buffer's back and checks that
    /// the finish neither sees it nor keeps it.
    #[test]
    fn finishing_validates_the_live_prefix_and_not_the_spare_room() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = built(&mut machine, &f, 0, b"short");
        let store = machine.payload(owner, 1);
        assert!(machine.object_len(store) > 5);
        // A byte the program never appended, in the spare room. Not reachable
        // from the instructions — which is the point: it stands in for whatever
        // a previous, longer occupant of a reused block left there.
        machine.put_bytes(store, 7, 1, 0xFF);
        let text = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
        assert_eq!(machine.string_bytes(text), b"short".to_vec());
        assert_eq!(machine.object_len(text), 5);
    }

    /// **`append-bytes` agrees with Rust's own slicing, from a `String` and
    /// from another buffer's store, at every alignment.**
    ///
    /// The destination offset is the buffer's own logical length, so the
    /// unaligned cases are made by appending a prefix first; the source offset
    /// is `from`. Every case below is checked against
    /// `prefix + &text[from..to]`.
    #[test]
    fn a_bulk_append_agrees_with_rust_at_every_alignment() {
        let f = buffers();
        let text = "abcdefghijklmnopqrstuvwxyz";
        let mut machine = Machine::new(&f.program, 1 << 16);
        let as_string = machine.new_string(text).unwrap();
        // Another buffer's store, which is a `Shape::Bytes` run: ADR 0052's
        // "appendSlice ... copies directly from the source String" and ADR
        // 0051's fused slice both want a source that is not finished yet.
        let other = built(&mut machine, &f, text.len() as i64, text.as_bytes());
        let as_run = machine.payload(other, 1);

        let cases: &[(usize, usize, usize)] = &[
            (0, 0, 5),
            (3, 0, 5),
            (0, 7, 16),
            (8, 10, 16),
            (1, 1, 11),
            (0, 0, 0),
            (5, 5, 5),
            (7, 0, 26),
        ];
        for &source in &[as_string, as_run] {
            for &(prefix, from, to) in cases {
                let owner = built(&mut machine, &f, 0, &text.as_bytes()[..prefix]);
                let answered = machine
                    .run(
                        f.append_bytes,
                        &[owner, source, from as u64, to as u64],
                        &budget(),
                    )
                    .unwrap()[0];
                assert_eq!(answered, owner);
                let answer = machine.run(f.finish, &[owner], &budget()).unwrap()[0];
                let mut want = text.as_bytes()[..prefix].to_vec();
                want.extend_from_slice(&text.as_bytes()[from..to]);
                assert_eq!(
                    machine.string_bytes(answer),
                    want,
                    "source {source} prefix={prefix} from={from} to={to}"
                );
            }
        }
    }

    /// **A `from` or `to` inside a character is refused in
    /// `String.sliceBytes`'s words.**
    ///
    /// ADR 0052: "`appendSlice` checks the same bounds and UTF-8 boundaries as
    /// `String.sliceBytes`". Two operations that make the same refusal in
    /// different words are two rules a reader has to learn, so the sentence is
    /// pinned rather than paraphrased. A `Shape::Bytes` source is held to no
    /// such rule, and that half is asserted too.
    #[test]
    fn a_bulk_append_refuses_an_offset_inside_a_character() {
        let f = buffers();
        // `a` is one byte, `é` is two and `漢` is three, so the boundaries are
        // 0, 1, 3 and 6, and 2, 4 and 5 are each inside a character.
        let text = "aé漢";
        assert_eq!(text.len(), 6);
        let mut machine = Machine::new(&f.program, 1 << 16);
        let as_string = machine.new_string(text).unwrap();
        let as_run = {
            let owner = built(&mut machine, &f, 6, text.as_bytes());
            machine.payload(owner, 1)
        };

        for (from, to, name, at) in [(2u64, 6u64, "from", 2u64), (0, 4, "to", 4)] {
            let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
            let error = machine
                .run(f.append_bytes, &[owner, as_string, from, to], &budget())
                .unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "`{name}` is `{at}`, which is inside a character rather than at the \
                     start of one"
                )
            );
            // The same offsets out of a run under construction are ordinary
            // bytes, because a run is not claiming to be text.
            let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
            machine
                .run(f.append_bytes, &[owner, as_run, from, to], &budget())
                .expect("a byte run has no character boundaries");
        }
    }

    #[test]
    fn a_bulk_append_refuses_an_out_of_range_or_backwards_slice() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let src = machine.new_string("abcdef").unwrap();
        for (from, to, want) in [
            (
                0u64,
                7u64,
                "`to` is `7`, and a byte offset into this string is 0 to 6".to_string(),
            ),
            (
                (-1i64) as u64,
                3,
                "`from` is `-1`, and a byte offset into this string is 0 to 6".to_string(),
            ),
            (
                4,
                2,
                "`from` is `4` and `to` is `2`, so this range runs backwards".to_string(),
            ),
        ] {
            let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
            let error = machine
                .run(f.append_bytes, &[owner, src, from, to], &budget())
                .unwrap_err();
            assert_eq!(error.message, want);
        }
    }

    #[test]
    fn every_buffer_instruction_refuses_a_null_owner() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let src = machine.new_string("abc").unwrap();
        for error in [
            machine.run(f.append_byte, &[0, 65], &budget()).unwrap_err(),
            machine
                .run(f.append_bytes, &[0, src, 0, 3], &budget())
                .unwrap_err(),
            machine.run(f.finish, &[0], &budget()).unwrap_err(),
        ] {
            assert_eq!(error.message, null_object().message);
        }
    }

    #[test]
    fn a_bulk_append_refuses_a_null_source() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        let error = machine
            .run(f.append_bytes, &[owner, 0, 0, 0], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn a_bulk_append_refuses_a_source_that_is_neither_a_string_nor_a_run() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        let other = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        // An owner is not a run: the raw store never leaves it.
        let error = machine
            .run(f.append_bytes, &[owner, other, 0, 0], &budget())
            .unwrap_err();
        assert!(
            error.message.contains("neither a `String` nor a byte run"),
            "{}",
            error.message
        );
    }

    #[test]
    fn every_buffer_instruction_refuses_a_value_that_is_not_a_buffer() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let string = machine.new_string("already a string").unwrap();
        for error in [
            machine
                .run(f.append_byte, &[string, 65], &budget())
                .unwrap_err(),
            machine
                .run(f.append_bytes, &[string, string, 0, 3], &budget())
                .unwrap_err(),
            machine.run(f.finish, &[string], &budget()).unwrap_err(),
        ] {
            assert!(error.message.contains("byte buffer"), "{}", error.message);
        }
    }

    #[test]
    fn appending_to_a_finished_buffer_is_refused_rather_than_read_as_empty() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let src = machine.new_string("abc").unwrap();
        let owner = built(&mut machine, &f, 0, b"done");
        machine.run(f.finish, &[owner], &budget()).unwrap();
        for error in [
            machine
                .run(f.append_byte, &[owner, 65], &budget())
                .unwrap_err(),
            machine
                .run(f.append_bytes, &[owner, src, 0, 3], &budget())
                .unwrap_err(),
        ] {
            assert!(
                error.message.contains("already consumed"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn appending_refuses_a_value_outside_a_byte() {
        let f = buffers();
        let mut machine = Machine::new(&f.program, 1 << 16);
        let owner = machine.run(f.alloc, &[0], &budget()).unwrap()[0];
        for value in [256u64, (-1i64) as u64] {
            let error = machine
                .run(f.append_byte, &[owner, value], &budget())
                .unwrap_err();
            assert!(
                error.message.contains("appendByte") && error.message.contains("0 to 255"),
                "{value}: {}",
                error.message
            );
        }
    }

    /// **A growth that collects keeps the buffer and every byte already
    /// appended.**
    ///
    /// The heap is small and the fixture allocates garbage on purpose, so the
    /// growths *must* collect — and the assertion on `collections` is what
    /// makes the test mean anything. A version of this without it passes when
    /// no collection happens at all.
    ///
    /// What it walks is the owner: the collector reaches it through the frame
    /// slot holding it, and reaches the store through the owner's word 1 and
    /// nowhere else. A trace that followed word 0 instead, or skipped the owner
    /// as a leaf, would lose the store and the bytes with it.
    #[test]
    fn a_growth_that_collects_keeps_every_byte_appended() {
        const BYTES: i64 = 200;
        const GARBAGE: i64 = 512;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let str_layout = build.string_layout();
        let run = build.bytes_layout();
        let owner = build.buffer_layout();
        // s0: the capacity and then the garbage length; s1: the owner;
        // s2: the byte appended; s3: garbage, cleared between allocations so
        // the previous one is unreachable when the next is asked for.
        let mut code = vec![
            Inst::Int { dst: 0, value: 0 },
            Inst::AllocBuffer {
                dst: 1,
                capacity: 0,
            },
            Inst::Int {
                dst: 2,
                value: i64::from(b'x'),
            },
            Inst::Int {
                dst: 0,
                value: GARBAGE,
            },
        ];
        for at in 0..BYTES {
            code.push(Inst::AppendByte {
                buffer: 1,
                value: 2,
            });
            // Garbage between every append, so no growth has a quiet heap.
            if at % 8 == 0 {
                code.push(Inst::AllocBytes { dst: 3, len: 0 });
                code.push(Inst::Clear {
                    slot: 3,
                    layout: run,
                });
            }
        }
        code.push(Inst::FinishBuffer { dst: 1, buffer: 1 });
        code.push(Inst::Return { src: 1 });
        let entry = build.function(
            "grow_under_pressure",
            &[],
            &[Repr::Int, Repr::Ref, Repr::Int, Repr::Ref],
            str_layout,
            code,
        );
        let _ = (int, owner);
        let program = build.done();

        // Room for the buffer, one piece of garbage and a little slack, so a
        // second piece cannot be handed out until the first is reclaimed —
        // which is what makes the collection certain rather than merely
        // possible.
        let mut machine = Machine::new(&program, 320);
        let before = machine.collected().collections;
        let answer = machine
            .run(entry, &[], &budget())
            .expect("the run answers a string");
        let after = machine.collected().collections;
        assert!(
            after > before,
            "this fixture exists to collect while a buffer grows, and it \
             collected {} time(s)",
            after - before
        );
        assert_eq!(
            machine.string_bytes(answer[0]),
            vec![b'x'; BYTES as usize],
            "every byte appended before the collection survived it"
        );
    }

    /// A run that appends `BYTES` bytes in one `append-bytes`, with a fixture
    /// whose only other instructions are the two allocations and a return.
    fn one_big_append() -> (Program, FunctionId) {
        const BYTES: i64 = 1 << 20;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let run = build.bytes_layout();
        let owner = build.buffer_layout();
        let args = build.args(&[(1, owner), (2, run), (4, int), (3, int)]);
        let entry = build.function(
            "appender",
            &[],
            &[Repr::Int, Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            owner,
            vec![
                Inst::Int {
                    dst: 3,
                    value: BYTES,
                },
                Inst::AllocBytes { dst: 2, len: 3 },
                Inst::Int { dst: 0, value: 0 },
                Inst::AllocBuffer {
                    dst: 1,
                    capacity: 0,
                },
                Inst::Int { dst: 4, value: 0 },
                Inst::AppendBytes { args },
                Inst::Return { src: 1 },
            ],
        );
        (build.done(), entry)
    }

    /// **A bulk append is charged for the words it moves, and overspends its
    /// fuel by less than one chunk plus one stride — not by the length of the
    /// append.**
    ///
    /// `a_bulk_copy_overspends_its_fuel_by_less_than_one_chunk` for the
    /// growable path. Asserting only that the run *stops* would pass just as
    /// well for an append that ran to the end of a megabyte first, which is what
    /// ADR 0040's `S + T` forbids and what an unchunked append would do.
    #[test]
    fn a_bulk_append_overspends_its_fuel_by_less_than_one_chunk() {
        const BYTES: u64 = 1 << 20;
        let (program, entry) = one_big_append();
        let words = BYTES.div_ceil(8);
        for limit in [1_024u64, 8_192, 40_000] {
            let budget = crate::budget::Budget::new(crate::budget::Limits {
                fuel: Some(limit),
                ..crate::budget::Limits::default()
            });
            let mut machine = Machine::new(&program, 1 << 22);
            let error = machine
                .run(entry, &[], &budget.meter())
                .expect_err("an append past its fuel is stopped");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);

            let bound = limit + words_of_bytes(BULK_CHUNK_BYTES) + SAFEPOINT_STRIDE;
            let spent = budget.fuel_spent();
            assert!(
                spent <= bound,
                "a {BYTES}-byte append under a fuel limit of {limit} spent {spent}, \
                 past the bound of {bound}; the whole append would have been {words}"
            );
            assert!(
                spent < words,
                "and it must not have appended the whole {words} words first"
            );
        }
    }
}
