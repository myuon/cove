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

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};

use cove_diag::Span;

use cove_ir::bytecode::{disasm, encode_program, verify, Encoded, EncodedInst, Op};
use cove_ir::{
    ArgsId, ArithOp, CmpOp, Compare, Convert, FunctionId, HostOpId, LayoutId, Num, Program, Repr,
    Shape, SiteId, Slot, Storage, StrId, TableId, Validation,
};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::find::Matcher;
use crate::vm::cell;
use crate::vm::mem::{header_layout, header_len, Overflow};
// `Outcome` here is the window census's — what became of one fused window —
// and not `super::Outcome`, which is a task's `Result`. The one signature in
// this file that wants that one says `super::Outcome`.
use crate::vm::report::{Decline, Outcome};

use super::{
    compare, float_arith, int_arith, native, null_object, overflowed, reentrant_lock, runs,
    wrong_arity, ChildState, Frame, Live, Machine, ScopeEntry, SAFEPOINT_STRIDE,
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
// ADR 0059's three-way order, a family of its own at the end of the table.
const ORDER_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Order).number();
const ORDER_FLOAT: u8 = Op::Cmp(Compare::Float, CmpOp::Order).number();
const ORDER_BOOL: u8 = Op::Cmp(Compare::Bool, CmpOp::Order).number();
const ORDER_STR: u8 = Op::Cmp(Compare::Str, CmpOp::Order).number();
const ORDER_REF: u8 = Op::Cmp(Compare::Identity, CmpOp::Order).number();
const ORDER_TAG: u8 = Op::Cmp(Compare::Tag, CmpOp::Order).number();

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
const DURATION_TO_INT: u8 = Op::Convert(Convert::DurationToInt).number();
const INT_TO_DURATION: u8 = Op::Convert(Convert::IntToDuration).number();

const JUMP: u8 = Op::Jump.number();
const BRANCH_FALSE: u8 = Op::BranchFalse.number();

// ADR 0054's fused pair, one name per member of the two families they mirror.
// The four that compare a `Bool` or a reference for order are here because the
// cross product is generated rather than hand-picked, exactly as `Op::Cmp`'s
// are, and the lowering emits none of them either.
const EQ_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Eq).number();
const NE_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Ne).number();
const LT_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Lt).number();
const LE_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Le).number();
const GT_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Gt).number();
const GE_INT_BRANCH: u8 = Op::CmpBranch(Compare::Int, CmpOp::Ge).number();

const EQ_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Eq).number();
const NE_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Ne).number();
const LT_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Lt).number();
const LE_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Le).number();
const GT_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Gt).number();
const GE_FLOAT_BRANCH: u8 = Op::CmpBranch(Compare::Float, CmpOp::Ge).number();

const EQ_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Eq).number();
const NE_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Ne).number();
const LT_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Lt).number();
const LE_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Le).number();
const GT_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Gt).number();
const GE_BOOL_BRANCH: u8 = Op::CmpBranch(Compare::Bool, CmpOp::Ge).number();

const EQ_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Eq).number();
const NE_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Ne).number();
const LT_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Lt).number();
const LE_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Le).number();
const GT_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Gt).number();
const GE_STR_BRANCH: u8 = Op::CmpBranch(Compare::Str, CmpOp::Ge).number();

const EQ_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Eq).number();
const NE_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Ne).number();
const LT_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Lt).number();
const LE_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Le).number();
const GT_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Gt).number();
const GE_REF_BRANCH: u8 = Op::CmpBranch(Compare::Identity, CmpOp::Ge).number();

const EQ_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Eq).number();
const NE_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Ne).number();
const LT_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Lt).number();
const LE_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Le).number();
const GT_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Gt).number();
const GE_TAG_BRANCH: u8 = Op::CmpBranch(Compare::Tag, CmpOp::Ge).number();

const EQ_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Eq).number();
const NE_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Ne).number();
const LT_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Lt).number();
const LE_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Le).number();
const GT_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Gt).number();
const GE_INT_IMM_BRANCH: u8 = Op::CmpImmBranch(CmpOp::Ge).number();

const SWITCH: u8 = Op::Switch.number();
const RETURN: u8 = Op::Return.number();

const CALL: u8 = Op::Call.number();
const CALL_CLOSURE: u8 = Op::CallClosure.number();
const CALL_HOST: u8 = Op::CallHost.number();
const CALL_RESOURCE: u8 = Op::CallResource.number();
const INTRINSIC_CALL: u8 = Op::IntrinsicCall.number();

const ALLOC_FIXED: u8 = Op::AllocFixed.number();
const ALLOC_IMM: u8 = Op::AllocImm.number();
const ALLOC_SLOT: u8 = Op::AllocSlot.number();
const LOAD_FIELD: u8 = Op::LoadField.number();
const STORE_FIELD: u8 = Op::StoreField.number();
const LOAD_ELEM: u8 = Op::LoadElem.number();
const STORE_ELEM: u8 = Op::StoreElem.number();
const RUN_LOAD_BYTES: u8 = Op::RunLoadBytes.number();
const RUN_COPY_BYTES: u8 = Op::RunCopyBytes.number();
const RUN_COPY_WORDS: u8 = Op::RunCopyWords.number();
const RUN_SLICE_BYTES: u8 = Op::RunSliceBytes.number();
const RUN_SLICE_WORDS: u8 = Op::RunSliceWords.number();
const RUN_FIND_BYTES: u8 = Op::RunFindBytes.number();
const GROWABLE_ALLOC_BYTES: u8 = Op::GrowableAllocBytes.number();
const GROWABLE_ALLOC_WORDS: u8 = Op::GrowableAllocWords.number();
const GROWABLE_TRUNCATE_WORDS: u8 = Op::GrowableTruncateWords.number();
const RUN_FINISH_BYTES: u8 = Op::RunFinishBytes.number();
const RUN_FINISH_WORDS: u8 = Op::RunFinishWords.number();
const GROWABLE_ENSURE_BYTES: u8 = Op::GrowableEnsureBytes.number();
const GROWABLE_ENSURE_WORDS: u8 = Op::GrowableEnsureWords.number();
const GROWABLE_COMMIT_BYTES: u8 = Op::GrowableCommitBytes.number();
const GROWABLE_COMMIT_WORDS: u8 = Op::GrowableCommitWords.number();
const RUN_STORE_BYTES: u8 = Op::RunStoreBytes.number();
const FUSED_PUSH_WORDS: u8 = Op::FusedPushWords.number();
const FUSED_PUSH_BYTE: u8 = Op::FusedPushByte.number();
const FUSED_APPEND_BYTES: u8 = Op::FusedAppendBytes.number();
const FUSED_APPEND_WORDS: u8 = Op::FusedAppendWords.number();
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
        | Op::CmpBranch(_, _)
        | Op::CmpImmBranch(_)
        | Op::Switch
        | Op::Return
        | Op::Call
        | Op::CallClosure
        | Op::CallHost
        | Op::CallResource
        | Op::IntrinsicCall
        | Op::AllocFixed
        | Op::AllocImm
        | Op::AllocSlot
        | Op::RunLoadBytes
        | Op::RunCopyBytes
        | Op::RunCopyWords
        | Op::RunSliceBytes
        | Op::RunSliceWords
        | Op::RunFindBytes
        | Op::GrowableAllocBytes
        | Op::GrowableAllocWords
        | Op::GrowableTruncateWords
        | Op::RunFinishBytes
        | Op::RunFinishWords
        | Op::GrowableEnsureBytes
        | Op::GrowableEnsureWords
        | Op::GrowableCommitBytes
        | Op::GrowableCommitWords
        | Op::RunStoreBytes
        | Op::FusedPushWords
        | Op::FusedPushByte
        | Op::FusedAppendBytes
        | Op::FusedAppendWords
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
pub(super) fn open_frame(
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

/// How many payload words a bulk operation moves between two safepoints.
///
/// One [`SAFEPOINT_STRIDE`] of work, so a chunk costs exactly the stride and
/// the poll that follows it is due. This is the `T` of
/// [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
/// `S + T` for a bulk operation: a cancelled or out-of-fuel run gets no
/// further than one chunk past the bound, whatever the length it was asked
/// to copy.
///
/// It is stated in words because work is. A byte chunk and an element chunk
/// are the same chunk measured in different units, so a run of `String`
/// handles stops as promptly as a run of text of the same size.
const BULK_CHUNK_WORDS: u64 = SAFEPOINT_STRIDE;

/// [`BULK_CHUNK_WORDS`] in bytes, which is the chunk a packed-byte copy takes.
const BULK_CHUNK_BYTES: i64 = (BULK_CHUNK_WORDS * 8) as i64;

/// The chunk loop every bulk run copy shares: `count` units, at most `chunk`
/// of them a piece, each piece charged the words `piece` answers it moved and
/// followed by a safepoint once a stride of work has gathered.
///
/// [`run_copy_bytes`], [`run_copy_words`] and [`append_bytes`] are this loop
/// with a different piece, so the three cannot disagree about when a bulk copy
/// polls or what it is charged. `#[inline(always)]` because each of them is
/// already out of line, and a closure called through a function that was not
/// inlined would be an indirect call per chunk for nothing.
///
/// The chunking is the correctness argument rather than a refinement of it.
/// One copy may move far more than a stride of work, and charging for all of
/// it afterwards would let a cancelled or out-of-fuel run copy the whole range
/// first — [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)
/// promises `S + T` of Cove work once a bound becomes true, not `S + T` plus
/// the length of the copy.
///
/// # Direction
///
/// `descending` walks the pieces from the tail. Splitting a `memmove` into
/// pieces does not preserve its meaning by itself: copying a run's units to a
/// higher offset in *itself*, front first, makes each piece overwrite the input
/// of the next — and every piece being correct in isolation does not save it.
/// So an overlapping forward shift is chunked from the tail, which is the same
/// reason each piece walks backwards inside itself.
///
/// # Why the addresses survive the poll
///
/// A piece closes over the two objects' addresses, read once before the first
/// piece, and a safepoint between pieces may collect. That is sound without
/// reading them again because the collector does not move objects —
/// `crate::vm::mem`'s "why the collector does not move objects" — and it needs
/// no write barrier either, because it is a stop-the-world mark from the roots
/// with no generations or remembered set for a store of a reference to keep
/// up to date. What a collection *does* need is both objects reachable, and
/// that is every caller's to establish before it gets here: each has `sync`ed,
/// and each object is named by a slot of that frame or by a payload word of an
/// object that is.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn in_chunks<'a>(
    machine: &mut Machine<'a>,
    budget: &Meter,
    id: FunctionId,
    pc: usize,
    count: u64,
    chunk: u64,
    descending: bool,
    mut piece: impl FnMut(&mut Machine<'a>, u64, u64) -> u64,
) -> Result<(), RuntimeError> {
    let mut done = 0;
    while done < count {
        let take = (count - done).min(chunk);
        let offset = if descending {
            count - done - take
        } else {
            done
        };
        machine.bulk_work += piece(machine, offset, take);
        done += take;
        if machine.work() - machine.charged_work >= SAFEPOINT_STRIDE {
            machine.safepoint(budget, id, pc)?;
            machine.next_check = machine.next_question();
        }
    }
    Ok(())
}

/// The five operands of an [`Inst::RunCopy`], in units, and the one relation
/// between them — which copy direction — that does not depend on the storage.
#[derive(Clone, Copy)]
struct RunRange {
    dst: u64,
    dst_at: i64,
    src: u64,
    src_at: i64,
    count: i64,
}

impl RunRange {
    /// A forward shift within one object, which has to be walked from the
    /// tail. See [`in_chunks`].
    fn descending(&self) -> bool {
        self.dst == self.src && self.dst_at > self.src_at
    }
}

/// Reads an [`Inst::RunCopy`]'s five operands and makes the checks that are
/// the same whatever a unit is: neither object is null and the count is not
/// negative.
fn run_range(
    machine: &Machine<'_>,
    base: u64,
    args: &[cove_ir::Arg],
) -> Result<RunRange, RuntimeError> {
    let range = RunRange {
        dst: machine.mem.slot(base, args[0].slot),
        dst_at: machine.mem.slot(base, args[1].slot) as i64,
        src: machine.mem.slot(base, args[2].slot),
        src_at: machine.mem.slot(base, args[3].slot) as i64,
        count: machine.mem.slot(base, args[4].slot) as i64,
    };
    if range.dst == 0 || range.src == 0 {
        return Err(null_object());
    }
    if range.count < 0 {
        return Err(RuntimeError::new(format!(
            "`runCopy`'s count is `{}`, and a copy cannot have a negative length",
            range.count
        )));
    }
    Ok(range)
}

/// The bounds of an [`Inst::RunCopy`], in units, against each object's header
/// length — which is its logical length in those same units, bytes for a
/// packed run and elements for a word run.
///
/// Both are checked before anything is written, so a refused copy leaves the
/// destination as it was. `checked_add` because a wrapped end would pass the
/// comparison and then be a write past the object.
fn run_bounds(machine: &Machine<'_>, range: &RunRange, unit: &str) -> Result<(), RuntimeError> {
    let RunRange {
        dst,
        dst_at,
        src,
        src_at,
        count,
    } = *range;
    let src_len = machine.mem.object_len(src) as i64;
    if src_at < 0 || src_at.checked_add(count).is_none_or(|end| end > src_len) {
        return Err(RuntimeError::new(format!(
            "`runCopy` reads {count} {unit} from {src_at} of a source of {src_len}"
        )));
    }
    let dst_len = machine.mem.object_len(dst) as i64;
    if dst_at < 0 || dst_at.checked_add(count).is_none_or(|end| end > dst_len) {
        return Err(RuntimeError::new(format!(
            "`runCopy` writes {count} {unit} to {dst_at} of a destination of {dst_len}"
        )));
    }
    Ok(())
}

/// [`Inst::RunCopy`] over [`cove_ir::Storage::PackedBytes`], checked and
/// copied in bounded chunks.
///
/// Out of line, and out of the dispatch loop's body, for the reason
/// [`crate::vm::debug`] records: this loop is sensitive to how much code sits
/// in it, not only to what that code does.
///
/// A byte run holds no references, so a collection at a chunk's poll has
/// nothing in the half-written destination to follow; `dst` and `src` are
/// rooted by the slots this read them out of.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_copy_bytes(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let range = run_range(machine, base, args).map_err(|error| refuse(machine, error))?;
    if !matches!(
        program.layout(machine.mem.object_layout(range.dst)).shape,
        Shape::Bytes
    ) {
        return Err(refuse(
            machine,
            RuntimeError::new(
                "`runCopy`'s destination is not a byte run under construction, and only one \
                 of those may be written into",
            ),
        ));
    }
    if !matches!(
        program.layout(machine.mem.object_layout(range.src)).shape,
        Shape::Str | Shape::Bytes
    ) {
        return Err(refuse(
            machine,
            RuntimeError::new(
                "`runCopy`'s source is neither a `String` nor a byte run under construction",
            ),
        ));
    }
    run_bounds(machine, &range, "byte(s)").map_err(|error| refuse(machine, error))?;
    let descending = range.descending();
    let RunRange {
        dst,
        dst_at,
        src,
        src_at,
        count,
    } = range;
    in_chunks(
        machine,
        budget,
        id,
        pc,
        count as u64,
        BULK_CHUNK_BYTES as u64,
        descending,
        |machine, offset, take| {
            machine.copy_string_bytes(
                dst,
                dst_at as usize + offset as usize,
                src,
                src_at as usize + offset as usize,
                take as usize,
            );
            words_of_bytes(take as i64)
        },
    )
}

/// [`Inst::RunCopy`] over [`cove_ir::Storage::Words`]: whole elements of
/// `elem`, checked in elements and copied in chunks of whole elements.
///
/// # What is checked
///
/// The destination must be a [`Shape::Elements`] of exactly `elem` — an
/// `Array`'s elements or a `Vector`'s store, since `growable` changes nothing
/// about the words. The source may also be a `Set` of `elem` or a `Map` whose
/// entry `elem` is ([`cove_ir::reads_as_units_of`]): a sorted run is the same
/// words at the same stride, and `std.set`/`std.map` build an updated run by
/// copying the unchanged ranges of the old one into a growable vector (#378,
/// P4-5). It is never a destination, because its order is an invariant only
/// its construction establishes. That is not a courtesy check. The collector
/// traces each object by its *own* layout's reference map, so a unit copied
/// between runs of two families would be words one map calls integers and the
/// other follows as addresses. The bounds are then [`run_bounds`]'s, in
/// elements, and only after both is anything multiplied by the stride.
///
/// # Why nothing more is needed for a run of references
///
/// A word copy moves references, and a collector that had to be told about
/// a stored reference would need a barrier here. This one does not: it is
/// non-moving and stop-the-world, it marks from the roots at each collection
/// rather than keeping a remembered set, and the builtins that already store
/// references into a store — `Vector.push`, `Array.toVector` — do nothing
/// beyond writing the words, which is what this does. What matters at a
/// chunk's poll is that the destination is walkable part-way through, and it
/// is: its payload was zeroed at allocation, so an element not yet written
/// traces as null, and a chunk is a whole number of elements, so no element is
/// ever half its old words and half its new ones.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_copy_words(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    elem: LayoutId,
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let range = run_range(machine, base, args).map_err(|error| refuse(machine, error))?;
    for (end, addr) in [("destination", range.dst), ("source", range.src)] {
        let shape = &program.layout(machine.mem.object_layout(addr)).shape;
        let is_run = match end {
            "source" => cove_ir::reads_as_units_of(&program.layouts, shape, elem),
            _ => matches!(shape, Shape::Elements { elem: held, .. } if *held == elem),
        };
        if !is_run {
            return Err(refuse(
                machine,
                RuntimeError::new(format!(
                    "`runCopy`'s {end} is not a run of `{}` elements",
                    program.layout(elem).name
                )),
            ));
        }
    }
    run_bounds(machine, &range, "element(s)").map_err(|error| refuse(machine, error))?;
    let stride = u64::from(machine.width(elem));
    if stride == 0 {
        return Ok(());
    }
    let descending = range.descending();
    let RunRange {
        dst,
        dst_at,
        src,
        src_at,
        count,
    } = range;
    // Whole elements a piece, and at least one: an element wider than a
    // stride of words is one piece on its own, which overshoots the chunk by
    // less than one element rather than splitting it.
    let chunk = (BULK_CHUNK_WORDS / stride).max(1);
    in_chunks(
        machine,
        budget,
        id,
        pc,
        count as u64,
        chunk,
        descending,
        |machine, offset, take| {
            // Every product fits a `u32`: the bounds above hold each end
            // inside an object whose payload, `len * stride` words, was sized
            // as a `u32` when it was allocated.
            let words = take * stride;
            let to = (dst_at as u64 + offset) * stride;
            let from = (src_at as u64 + offset) * stride;
            machine.mem.copy_words(
                machine.mem.payload_addr(dst, to as u32),
                machine.mem.payload_addr(src, from as u32),
                words as u32,
            );
            words
        },
    )
}

/// [`Inst::RunSlice`] over [`cove_ir::Storage::Words`]: a fresh fixed run of
/// `count` elements of `elem`, copied out of `src` from `from`, written into
/// `dst`.
///
/// Out of line, and never inlined into the dispatch loop, for
/// [`run_copy_bytes`]' reason — and for the one #378 measured on
/// `Machine::finish_words`, whose inlining into its rare arm cost the loop
/// around every other arm 15%.
///
/// # What is checked
///
/// Everything before the allocation, so a refused slice allocates nothing: the
/// source is not null, the count is not negative, the source is a
/// [`Shape::Elements`] of exactly `elem` — an `Array` or a `Vector`'s store —
/// or the `Set` or `Map` whose unit `elem` is, and `from .. from + count` is inside its header length. That length is a
/// store's capacity rather than a vector's length, which is why every caller
/// clamps into the logical length first; each refusal here is a broken
/// invariant of the lowering, in `runCopy`'s sentences with this instruction's
/// name.
///
/// # Why the answer is written last
///
/// The fresh run is held as a temporary root while [`in_chunks`] fills it, and
/// written into `dst` only when it is whole. Until then `src` is still named by
/// the frame even where `dst` is the same slot, so a collection at a chunk's
/// poll finds both — and a run stopped part way through leaves `dst` as it
/// was rather than holding a run that is part zeroes. The copy is `run_copy_words`'
/// chunks, charge and polls exactly: one chunk is a whole number of elements.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_slice_words(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    elem: LayoutId,
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let target = args[0].layout;
    let src = machine.mem.slot(base, args[1].slot);
    let from = machine.mem.slot(base, args[2].slot) as i64;
    let count = machine.mem.slot(base, args[3].slot) as i64;
    if src == 0 {
        return Err(refuse(machine, null_object()));
    }
    if count < 0 {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runSlice`'s count is `{count}`, and a copy cannot have a negative length"
            )),
        ));
    }
    // A `Set` or a `Map` whose unit `elem` is reads as the run it is: `Set.toArray`
    // is a slice of the whole set (#378, P4-7).
    let is_run = cove_ir::reads_as_units_of(
        &program.layouts,
        &program.layout(machine.mem.object_layout(src)).shape,
        elem,
    );
    if !is_run {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runSlice`'s source is not a run of `{}` elements",
                program.layout(elem).name
            )),
        ));
    }
    let len = machine.mem.object_len(src) as i64;
    if from < 0 || from.checked_add(count).is_none_or(|end| end > len) {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runSlice` reads {count} element(s) from {from} of a source of {len}"
            )),
        ));
    }
    let fresh = machine
        .allocate(target, count)
        .map_err(|error| refuse(machine, error))?;
    let stride = u64::from(machine.width(elem));
    if stride > 0 && count > 0 {
        let mark = machine.temps();
        machine.push_temp(fresh);
        // Whole elements a piece, as `run_copy_words` takes them.
        let chunk = (BULK_CHUNK_WORDS / stride).max(1);
        let copied = in_chunks(
            machine,
            budget,
            id,
            pc,
            count as u64,
            chunk,
            false,
            |machine, offset, take| {
                // Every product fits a `u32`: both runs' payloads, `len *
                // stride` words, were sized as a `u32` when they were allocated.
                let words = take * stride;
                let to = offset * stride;
                let at = (from as u64 + offset) * stride;
                machine.mem.copy_words(
                    machine.mem.payload_addr(fresh, to as u32),
                    machine.mem.payload_addr(src, at as u32),
                    words as u32,
                );
                words
            },
        );
        machine.release_temps(mark);
        copied?;
    }
    machine.mem.set_slot(base, args[0].slot, fresh);
    Ok(())
}

/// [`Inst::RunSlice`] over [`cove_ir::Storage::PackedBytes`]: a fresh `String` of
/// `count` bytes copied out of the `String` `src` from `from`, written into
/// `dst`.
///
/// Out of line, and never inlined into the dispatch loop, for
/// [`run_slice_words`]' reason: it is reached from one arm of `dispatch`, and a
/// slow path inlined into a rare arm has cost every other arm before (#378).
///
/// # What is checked, and what is not
///
/// Everything [`run_slice_words`] checks, before the allocation, with a byte in
/// place of an element: the source is not null, the count is not negative, the
/// source is a `String` and `from .. from + count` is inside its byte length.
/// Each is a broken invariant of the lowering, in `runSlice`'s sentences.
///
/// **Neither end is checked to be a character boundary, and the bytes are not
/// validated.** `std.string.sliceBytes` decides both before it asks — a range of
/// valid UTF-8 cut at two boundaries is valid UTF-8 — and this is the copy that
/// precondition exists for (#378, Q3). A byte run under construction is not a
/// source, because nothing slices one and its bytes are not yet text.
///
/// # Why the answer is written last
///
/// [`run_slice_words`]' reason. A string holds no references, so a collection at
/// a chunk's poll has nothing inside the half-written answer to follow — but the
/// answer itself has to survive it, so it is a temporary root until it is whole
/// and in `dst`. The chunks, the charge and the polls are [`run_copy_bytes`]'.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_slice_bytes(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let src = machine.mem.slot(base, args[1].slot);
    let from = machine.mem.slot(base, args[2].slot) as i64;
    let count = machine.mem.slot(base, args[3].slot) as i64;
    if src == 0 {
        return Err(refuse(machine, null_object()));
    }
    if count < 0 {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runSlice`'s count is `{count}`, and a copy cannot have a negative length"
            )),
        ));
    }
    if !matches!(
        program.layout(machine.mem.object_layout(src)).shape,
        Shape::Str
    ) {
        return Err(refuse(
            machine,
            RuntimeError::new("`runSlice`'s source is not a `String`"),
        ));
    }
    let len = machine.mem.object_len(src) as i64;
    if from < 0 || from.checked_add(count).is_none_or(|end| end > len) {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runSlice` reads {count} byte(s) from {from} of a source of {len}"
            )),
        ));
    }
    let fresh = machine
        .new_string_of(count)
        .map_err(|error| refuse(machine, error))?;
    if count > 0 {
        let mark = machine.temps();
        machine.push_temp(fresh);
        let copied = in_chunks(
            machine,
            budget,
            id,
            pc,
            count as u64,
            BULK_CHUNK_BYTES as u64,
            false,
            |machine, offset, take| {
                machine.copy_string_bytes(
                    fresh,
                    offset as usize,
                    src,
                    from as usize + offset as usize,
                    take as usize,
                );
                words_of_bytes(take as i64)
            },
        );
        machine.release_temps(mark);
        copied?;
    }
    machine.mem.set_slot(base, args[0].slot, fresh);
    Ok(())
}

/// [`Inst::RunFind`] over [`cove_ir::Storage::PackedBytes`]:
/// [ADR 0065](../../../../../docs/adr/0065-a-run-search-is-the-one-loop-that-stays-below.md)'s
/// run search, checked, then prepared and consumed in bounded steps.
///
/// Out of line and out of the dispatch loop's body for [`run_copy_bytes`]'
/// reason.
///
/// # What is checked, and in which order
///
/// Neither run is null and both are a `String` — a run under construction is
/// not admitted, as it is not a [`run_slice_bytes`] source — and `from` is in
/// `0 ..= haystack_len`. Each is a broken invariant of the lowering rather
/// than a program's mistake: `std.string.contains` passes zero, and a future
/// `split` computes `from` from a previous answer and a needle length, both
/// already in range.
///
/// **Nothing about the bytes is validated and no character is decoded.** The
/// comparison is bitwise, and whether a byte match is also a character match
/// is `std.string.contains`' question, argued there. The two runs may alias:
/// this only reads, so there is no order in which a write could be seen.
///
/// # The two answers that charge nothing
///
/// An empty needle answers `from`, and a needle longer than
/// `haystack_len - from` answers -1 — both before any preparation begins,
/// because there is no needle to prepare in the first and nothing to search in
/// the second, and neither examines a unit. The instruction's own single unit
/// of fuel is unchanged, so a fast path is one fuel and nothing else. That is
/// the accounting `std.string.endsWith`' length refusal already has.
///
/// # The steps, and what they are charged
///
/// [`Matcher`] takes at most [`SAFEPOINT_STRIDE`] turns a step, and a turn is
/// one comparison of one pair of units. After each step the turns taken since
/// the last one are charged, and if the answer is not in yet, a safepoint
/// runs. So the uninterruptible span is a stride of comparisons — and
/// therefore at most two strides of units examined — whatever `n` and `m` are
/// and whichever phase the matcher is in, which is
/// [ADR 0040](../../../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
/// `S + T` for this instruction.
///
/// **The charge is an upper bound on the work done, not an equality**, which
/// is what a fuel bound asks for and is what the intrinsic this replaces
/// charged too — "the receiver's whole length as an upper bound", because
/// `str::find` does not report how far it got. Here it is a count of
/// comparisons, and `crate::find` derives the bound it obeys from the
/// algorithm: `5m + 2(n - from)`.
///
/// The poll is unconditional rather than [`in_chunks`]' `work() -
/// charged_work >= SAFEPOINT_STRIDE`, and the difference is the phase change.
/// A step may spend part of its turns finishing the needle and the rest
/// starting the search, so the units a step consumes are not a fixed chunk and
/// a test against a fixed chunk would let two short steps run back to back. A
/// step that finishes the search does not poll at all, so the common case —
/// covefmt's longest `String` operand is 71 bytes — pays one step, no poll and
/// no extra safepoint.
///
/// # It allocates nothing
///
/// Not "little", and not "nothing after the first call": nothing, on every
/// path, for every needle. The matcher is Crochemore–Perrin, whose auxiliary
/// space is `O(1)` — seven words of state — so there is no table to hold and
/// no copy of either run to make, and both are read where they are through a
/// one-word cache. An earlier version of this instruction used a
/// Knuth–Morris–Pratt table out of [`Machine::take_scratch`]; see
/// [`find_in_runs`] for why a table is worse than it looks even when the pool
/// keeps it.
///
/// A poll may collect. The collector does not move objects, so the two
/// addresses read out of the frame stay the addresses of these runs, and both
/// are rooted by the slots they were read from; the buffers are Rust memory
/// and no collection reaches them.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_find_bytes(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    base: u64,
    args: &[cove_ir::Arg],
    id: FunctionId,
    pc: usize,
) -> Result<(), RuntimeError> {
    let refuse = |machine: &Machine<'_>, error: RuntimeError| error.at(machine.span(id, pc));
    let haystack = machine.mem.slot(base, args[1].slot);
    let needle = machine.mem.slot(base, args[2].slot);
    let from = machine.mem.slot(base, args[3].slot) as i64;
    if haystack == 0 || needle == 0 {
        return Err(refuse(machine, null_object()));
    }
    for (addr, role) in [(haystack, "haystack"), (needle, "needle")] {
        if !matches!(
            program.layout(machine.mem.object_layout(addr)).shape,
            Shape::Str
        ) {
            return Err(refuse(
                machine,
                RuntimeError::new(format!("`runFind`'s {role} is not a `String`")),
            ));
        }
    }
    let n = machine.mem.object_len(haystack) as i64;
    let m = machine.mem.object_len(needle) as i64;
    if from < 0 || from > n {
        return Err(refuse(
            machine,
            RuntimeError::new(format!(
                "`runFind` starts at {from} of a haystack of {n} byte(s), and a search starts \
                 at 0 to that length"
            )),
        ));
    }
    // The two answers reached before any preparation, neither of which
    // examines a unit.
    if m == 0 {
        machine.mem.set_slot(base, args[0].slot, from as u64);
        return Ok(());
    }
    if m > n - from {
        machine.mem.set_slot(base, args[0].slot, -1i64 as u64);
        return Ok(());
    }
    let answer = find_in_runs(
        machine,
        budget,
        haystack,
        needle,
        n as usize,
        m as usize,
        from as usize,
        id,
        pc,
    )?;
    machine.mem.set_slot(base, args[0].slot, answer as u64);
    Ok(())
}

/// The payload word of the string at `addr` that holds unit `at`, through a
/// one-word cache.
///
/// A run's payload is eight bytes to a word, and both of this instruction's
/// readers ask for units close to the ones they last asked for: preparation
/// compares two positions of the needle that walk upwards together, and an
/// attempt compares the needle forwards from its critical position and then
/// backwards to it. So the word a read landed in is very often the word the
/// next read wants, and the cache turns a payload read a unit into one a word.
///
/// This is all the reading the instruction does. There is no copy of either
/// run and no buffer: [`Matcher`] holds seven words of state and reads
/// everything through these two closures, which is why
/// [`run_find_bytes`] allocates nothing on any path.
///
/// A `String` is immutable and the collector does not move it, so a word held
/// across a safepoint is still that run's word.
#[inline]
fn cached_word(machine: &Machine<'_>, addr: u64, at: usize, cache: &mut (u32, u64)) -> u64 {
    let want = (at / 8) as u32;
    if cache.0 != want {
        *cache = (want, machine.mem.payload(addr, want));
    }
    cache.1
}

/// The unit at `at`, out of [`cached_word`]: the needle's reader.
///
/// One cached word and not two, which was measured rather than assumed. A
/// maximal-suffix scan compares `needle[at + offset]` against
/// `needle[start + offset]`, two positions as far apart as the suffix it has
/// found, so a one-word cache misses on nearly every read of a long needle and
/// a two-entry one would hold both. It does — and it made no row of
/// `benches/contains` faster and several slower, because the extra test costs
/// more than the payload reads it saves. The needle's reads are not where a
/// search's time is.
#[inline]
fn cached_byte(machine: &Machine<'_>, addr: u64, at: usize, cache: &mut (u32, u64)) -> u8 {
    ((cached_word(machine, addr, at, cache) >> ((at % 8) * 8)) & 0xFF) as u8
}

/// The units from `at` to the end of the payload word it lies in, least
/// significant first — the haystack's reader, which answers a comparison and a
/// skip from one read.
///
/// One payload read and never two: the matcher takes at most `8 - at % 8`
/// units of what this answers, so a window never straddles a word. Units past
/// the run's length sit in the high end of the last word and are whatever the
/// heap left there; the matcher masks them off, and a comparison reads only
/// the low one.
#[inline]
fn cached_window(machine: &Machine<'_>, addr: u64, at: usize, cache: &mut (u32, u64)) -> u64 {
    cached_word(machine, addr, at, cache) >> ((at % 8) * 8)
}

/// [`run_find_bytes`]' loop: the matcher, driven a step at a time.
///
/// **Nothing is allocated here, on any path**, which is the property ADR
/// 0065's Decision 4 turned out to rest on rather than merely to prefer. A
/// matcher with a table has to put it somewhere, and a table appended to one
/// entry at a time reallocates and copies its whole contents when its capacity
/// runs out — an unbounded `memcpy` inside a phase whose entire purpose is
/// that no step exceeds a stride, and one no counter in this repository can
/// see, because a Rust-side allocation is exactly what `--boundary`'s `allocs`
/// column does not count (#442). Crochemore–Perrin needs no table, so there is
/// nothing to bound: the state is [`Matcher`]'s seven words and the two runs
/// are read where they are.
#[allow(clippy::too_many_arguments)]
fn find_in_runs(
    machine: &mut Machine<'_>,
    budget: &Meter,
    haystack: u64,
    needle: u64,
    n: usize,
    m: usize,
    from: usize,
    id: FunctionId,
    pc: usize,
) -> Result<i64, RuntimeError> {
    let mut matcher = Matcher::new(n, m, from);
    let mut charged = 0u64;
    let mut needle_cache = (u32::MAX, 0u64);
    let mut haystack_cache = (u32::MAX, 0u64);
    loop {
        let found = matcher.run(
            SAFEPOINT_STRIDE,
            |at| cached_byte(machine, needle, at, &mut needle_cache),
            |at| cached_window(machine, haystack, at, &mut haystack_cache),
        );
        machine.bulk_work += matcher.charge() - charged;
        charged = matcher.charge();
        if let Some(answer) = found {
            return Ok(answer);
        }
        machine.safepoint(budget, id, pc)?;
        machine.next_check = machine.next_question();
    }
}

/// One of [ADR 0062]'s window instructions — `GROWABLE_ENSURE_*`,
/// `GROWABLE_COMMIT_*` or `RUN_STORE_BYTES` — read out of `held` and the frame
/// at `base_at`, and run.
///
/// # Why the dispatch arm is one call
///
/// Not only for `RUN_COPY_BYTES`' reason that the checks are more code than an
/// arm should hold. **`dispatch`'s own stack frame is a budget.** A run that
/// alternates compiled and encoded frames re-enters `dispatch` once per
/// crossing on the Rust stack, and `native_tier`'s
/// `a_var_survives_a_reallocation_under_an_alternating_chain` descends three
/// hundred of them. Three arms written out in the loop — each with its operands,
/// its `Storage` and its `Result` held across a call — grew that frame enough to
/// overflow the test thread's stack there; one arm and one out-of-line call did
/// not. So the byte store's fast path is here too, rather than inline beside
/// `RUN_LOAD_BYTES`: until a producer makes it hot, and ADR 0062's fused heads
/// replace the arm anyway, the frame is the dearer of the two.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[inline(never)]
fn buffer_window(
    machine: &mut Machine<'_>,
    held: EncodedInst,
    base_at: usize,
) -> Result<(), RuntimeError> {
    let a = machine.mem.word_at(base_at + held.a() as usize);
    let b = machine.mem.word_at(base_at + held.b() as usize) as i64;
    let words = Storage::Words(LayoutId(held.lo()));
    match held.opcode() {
        GROWABLE_ENSURE_BYTES => machine.ensure_growable(a, Storage::PackedBytes, b),
        GROWABLE_ENSURE_WORDS => machine.ensure_growable(a, words, b),
        GROWABLE_COMMIT_BYTES => machine.commit_growable(a, Storage::PackedBytes, b),
        GROWABLE_COMMIT_WORDS => machine.commit_growable(a, words, b),
        _ => {
            let value = machine.mem.word_at(base_at + held.c() as usize) as i64;
            machine.store_run_byte(a, b, value)
        }
    }
}

/// How many instructions past a fused head the loop must be able to count
/// without reaching `next_check` before the head runs its window: the longest
/// window's tail, whatever this one's is.
const WINDOW_TAIL: u64 = (cove_ir::legalize::MAX_ROWS - 1) as u64;

/// A fused head at `head`: [ADR 0062]'s window, run in one dispatch where it may
/// be. Answers how many rows after the head ran, which the loop steps over.
///
/// **The head is the length read it decodes to.** A push is [`fused_push`]
/// first, head and all, which the arm asks before this, and an append is
/// [`fused_append_bytes`] or [`fused_append_words`] first, which this asks
/// before anything else; what goes on from there is a window one of the three
/// declined. The head runs as `LOAD_FIELD` runs it, so a refusal there is that
/// arm's, in its words, at this pc.
///
/// **Then the rest, only when no question can fall inside.** The loop asks its
/// one question — a safepoint, and before every instruction a debugger's or a
/// profiler's — when `instructions` reaches `next_check`. If counting the
/// longest window's tail would reach it, nothing more runs here: the answer is
/// `0`, and the tail rows dispatch as the primitives they are still encoded as.
/// So a breakpoint on a tail row stops there, `--profile` counts every row, and
/// a safepoint happens at the instruction it always did. Otherwise everything
/// else — an append, a push a refusal is coming for — is [`fused_tail`].
///
/// # One call, and why the fast path is not in the loop
///
/// `buffer_window`'s reason, measured again: with the head's load and
/// `fused_push` written into the arm, and even with `fused_push` out of line
/// and only the head's load and a compare left in it, `dispatch`'s frame grew
/// enough that `native_tier`'s
/// `a_var_survives_a_reallocation_under_an_alternating_chain` overflowed its
/// stack. A direct call per window is still one dispatch where the rows are
/// seven or eight. A push's arm makes two calls in sequence — [`fused_push`],
/// and this only when that declined — which measured *below* one call of this
/// on that test's stack: see [`fused_push`].
///
/// # Why it takes `encoded` and not `code`
///
/// Because what the arm hands a call decides where the loop keeps it. With the
/// current function's `code` slice as an argument, the loop stopped holding its
/// pointer in a register and read it from the frame on every dispatch — visible
/// in the disassembly as one more load before the jump table — and covefmt ran
/// 5.0 s against 4.8 s with no window anywhere in it. `encoded` and `id` are
/// what the loop itself finds the slice from after a call or a return, and the
/// slice is found again here, once per window, for the price of an index.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[inline(never)]
fn fused_window(
    machine: &mut Machine<'_>,
    encoded: &Encoded,
    budget: &Meter,
    id: FunctionId,
    base: u64,
    head: usize,
) -> Result<usize, RuntimeError> {
    let program = machine.program;
    let code = encoded.function(id);
    let held = code[head];
    let base_at = machine.mem.stack_index(base);
    let fast = match held.opcode() {
        FUSED_APPEND_BYTES => {
            fused_append_bytes(machine, encoded, budget, id, base, base_at, head)?
        }
        FUSED_APPEND_WORDS => {
            fused_append_words(machine, encoded, budget, id, base, base_at, head)?
        }
        _ => 0,
    };
    if fast > 0 {
        return Ok(fast);
    }
    let open = machine.instructions + WINDOW_TAIL < machine.next_check;
    let addr = machine.mem.word_at(base_at + held.b() as usize);
    let field = held.lo();
    let width = machine.width(LayoutId(held.hi()));
    if let Err(error) = machine.checked(addr, field, width) {
        machine.sync(head);
        return Err(error.at(machine.span(id, head)));
    }
    let from = machine.mem.payload_addr(addr, field);
    machine.mem.copy_words(base + held.a() as u64, from, width);
    if !open {
        // The census records nothing here, and this is the one place in a
        // fused arm where a `return Ok(0)` does not. `open` is the negation of
        // the test each fast path makes on entry, and nothing between the two
        // moves `instructions` or `next_check` — a fast path that declines
        // writes nothing and counts nothing — so this return is reached
        // exactly when the fast path has already recorded
        // `Decline::Safepoint` for this head. A second record here would
        // count one window twice and put the safepoint row at double what it
        // is.
        return Ok(0);
    }
    fused_tail(machine, program, budget, code, id, base, head, head + 1)
}

/// A push window run whole, head included, in the one call the fused arm makes
/// for it: [ADR 0062]'s fast path.
///
/// # What it asks, and why the answers are the rows'
///
/// The window is `cove_ir::legalize`'s push — `int n <- 1`, the ensure, the
/// store read, the write, an optional clear, an optional second `int`, the
/// commit — and `bytecode::verify` has re-matched it, so the rows are where this
/// reads them. Each primitive refuses exactly one way the common case can be
/// absent, and this asks each of those questions once, up front, of the words
/// the rows would read:
///
/// - the owner is its storage's run — a `Vector` of the ensure's element, as
///   `Machine::vector_run` asks, or `Program::buffer_layout`, as
///   `GROWABLE_PUSH_BYTE` asks — so both family readers would answer;
/// - its store is live, so the ensure would not refuse it;
/// - for a byte, the unit is a byte and the store is `Program::bytes_layout`,
///   so `store_run_byte` would store it.
///
/// Any other answer writes nothing and answers `0`: [`fused_window`] then runs
/// the head as its own row and [`fused_tail`] the rest one by one, in their own
/// words.
///
/// # A growth is the ensure's own
///
/// A store with no room is grown here, by the call the ensure's arm makes and
/// after the rows before it have written the frame and been counted, synced to
/// the ensure's pc — so a collection inside the growth sees the frame, the pc
/// and the count the rows would have shown it, and a refusal is the ensure's,
/// at its span. A growth is one push in eleven on covefmt (504,049 of
/// 5,395,970), and leaving it to [`fused_tail`] — the head through `checked`,
/// then each row through its own arm's code — cost the whole of the saving:
/// covefmt's VM run measured about 5.00 s that way and 4.70 s this way, against
/// 4.76 s for the composite push (interleaved runs of the two builds).
///
/// # What it writes
///
/// Every frame word the rows write, in their order and with their values — the
/// length, the count, the store, the clear, the second count — as well as the
/// unit and the length. So a frame, a heap and a debugger reading either after
/// the window cannot tell whether it was fused. The owner's words and the
/// store's are found through `Space::run_at`
/// once each rather than a chunk lookup per word, which with the call itself is
/// the whole of the difference between this and the rows the composite
/// `GrowablePush` arm used to run; a unit that a chunk boundary cuts in two is
/// written the ordinary way.
///
/// # What it counts
///
/// `Machine::instructions` rises by the rows it ran, so fuel counts semantic
/// instructions whichever way a window ran. That is sound only because no
/// question falls inside: this runs only when `instructions + WINDOW_TAIL` is
/// short of `next_check`, and the longest tail rather than this window's is the
/// bound so that nothing need find the commit before it may ask. A growth does
/// not bring the question nearer: `next_check` moves only at a question.
///
/// # Why its own call
///
/// [`fused_window`]'s frame is the slow path's — the head's `checked`, the
/// tail's copies and refusals — and a push paid for it on every window when
/// this was inlined there. Out of line and alone, it saves and restores only
/// what it uses, and it takes `encoded` and `id` for the reason
/// [`fused_window`] does.
///
/// Measured on a loop of two million `Int` pushes onto vectors of a thousand
/// (the `checked` profile, medians of nine interleaved runs): 99 ms as the
/// composite `GROWABLE_PUSH_WORDS` this replaced, 118 ms with this inlined into
/// [`fused_window`] and reading through `Memory::read`, and 94 ms as it is. The
/// arm's second call did not cost the loop's frame: `native_tier`'s alternating
/// chain, bisected with `RUST_MIN_STACK`, overflows at 2,065,000 bytes before
/// this change and at 2,038,000 after it.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[inline(never)]
fn fused_push(
    machine: &mut Machine<'_>,
    encoded: &Encoded,
    id: FunctionId,
    base: u64,
    base_at: usize,
    head: usize,
) -> Result<usize, RuntimeError> {
    if machine.instructions + WINDOW_TAIL >= machine.next_check {
        // The head's opcode is read here rather than above, because the row is
        // not loaded yet and hoisting the load out of this branch would put a
        // memory read on the path of every window a run that asked for no
        // counts runs.
        if machine.counting.is_some() {
            let head = encoded.function(id)[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Safepoint));
        }
        return Ok(0);
    }
    let program = machine.program;
    let code = encoded.function(id);
    // Seven rows are always there: the shortest push is six, and a function's
    // last row is a terminator, which no window's commit is.
    let Some(&[first, count, ensure, read, write, after, next]) = code.get(head..head + 7) else {
        if machine.counting.is_some() {
            let head = code[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Shape));
        }
        return Ok(0);
    };
    let byte = first.opcode() == FUSED_PUSH_BYTE;
    let width = if byte {
        1
    } else {
        machine.width(LayoutId(ensure.lo())) as usize
    };
    let at = base_at + first.a() as usize;
    let src = base_at + write.c() as usize;
    let (clear, second) = match after.opcode() {
        CLEAR => (Some(after), (next.opcode() == CONST_INT).then_some(next)),
        CONST_INT => (None, Some(after)),
        _ => (None, None),
    };
    let rows = 5 + usize::from(clear.is_some()) + usize::from(second.is_some());
    debug_assert!(
        matches!(
            code.get(head + rows).map(|held| held.opcode()),
            Some(GROWABLE_COMMIT_BYTES | GROWABLE_COMMIT_WORDS)
        ),
        "a verified push window ends in its commit"
    );

    // The owner's header and both payload words, and the store's header, found
    // once each: a null owner, a consumed store, or an owner a chunk boundary
    // cuts short of its payload is left to the rows.
    let (frame, heap) = machine.mem.stack_and_heap();
    let owner = frame[base_at + first.b() as usize];
    let Some([header, length, stored]) = heap.run_at(owner).and_then(|run| run.get(..3)) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    };
    let family = header_layout(header.load(Relaxed));
    let owned = if byte {
        family == program.buffer_layout
    } else {
        matches!(program.layout(family).shape, Shape::Vector { elem } if elem.0 == ensure.lo())
    };
    if !owned {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    }
    let len = length.load(Relaxed);
    let store = stored.load(Relaxed);
    let Some(run) = heap.run_at(store) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Store));
        }
        return Ok(0);
    };
    let store_header = run[0].load(Relaxed);
    if byte {
        // The unit as the write would read it, after the head has written `at`.
        let value = if src == at { len } else { frame[src] };
        if value > 0xFF || header_layout(store_header) != program.bytes_layout {
            // Two questions in one test, told apart here rather than there:
            // the value is not a byte, or the store is not a byte store.
            if machine.counting.is_some() {
                let why = match value > 0xFF {
                    true => Decline::Range,
                    false => Decline::Store,
                };
                machine.count_window(first.opcode(), Outcome::Declined(why));
            }
            return Ok(0);
        }
    }

    // The rows before the ensure: the head's length read — an owner of either
    // family has its two payload words, so `Machine::checked` would have
    // answered yes — and the count.
    frame[at] = len;
    frame[base_at + count.a() as usize] = count.payload();
    let into = if byte {
        1 + len as usize / 8
    } else {
        1 + len as usize * width
    };
    let room = len < u64::from(header_len(store_header));
    if let Some(into) = run.get(into..into + width).filter(|_| room) {
        frame[base_at + read.a() as usize] = store;
        if byte {
            // `GROWABLE_PUSH_BYTE`'s blend of one byte into its word.
            let shift = (len % 8) * 8;
            let held = into[0].load(Relaxed);
            into[0].store((held & !(0xFF << shift)) | (frame[src] << shift), Relaxed);
        } else {
            for (word, unit) in into.iter().zip(&frame[src..src + width]) {
                word.store(*unit, Relaxed);
            }
        }
        // One word: `legalize::recognize` admits only a clear of the store
        // slot, whose layout is one word wide.
        if let Some(clear) = clear {
            frame[base_at + clear.a() as usize] = 0;
        }
        if let Some(second) = second {
            frame[base_at + second.a() as usize] = second.payload();
        }
        length.store(len + 1, Relaxed);
        machine.instructions += rows as u64;
        if machine.counting.is_some() {
            machine.count_fusion(first.opcode(), rows, true);
            machine.count_window(first.opcode(), Outcome::Fast);
        }
        return Ok(rows);
    }

    // A growth, or a unit a chunk boundary cuts in two: the same rows, the
    // ordinary way.
    if room {
        machine.instructions += rows as u64;
    } else {
        machine.instructions += 2;
        machine.sync(head + 2);
        let storage = match byte {
            true => Storage::PackedBytes,
            false => Storage::Words(LayoutId(ensure.lo())),
        };
        if let Err(error) = machine.ensure_growable(owner, storage, 1) {
            if machine.counting.is_some() {
                machine.count_fusion(first.opcode(), 2, false);
            }
            return Err(error.at(machine.span(id, head + 2)));
        }
        machine.instructions += rows as u64 - 2;
    }
    let store = machine.mem.payload(owner, runs::GROWABLE_STORE);
    machine.mem.set_word_at(base_at + read.a() as usize, store);
    if byte {
        let shift = (len % 8) * 8;
        let value = machine.mem.word_at(src);
        let held = machine.mem.payload(store, len as u32 / 8);
        machine.mem.set_payload(
            store,
            len as u32 / 8,
            (held & !(0xFF << shift)) | (value << shift),
        );
    } else {
        let into = machine.mem.payload_addr(store, len as u32 * width as u32);
        machine
            .mem
            .copy_words(into, base + write.c() as u64, width as u32);
    }
    if let Some(clear) = clear {
        machine.mem.set_word_at(base_at + clear.a() as usize, 0);
    }
    if let Some(second) = second {
        machine
            .mem
            .set_word_at(base_at + second.a() as usize, second.payload());
    }
    machine.mem.set_payload(owner, runs::GROWABLE_LEN, len + 1);
    if machine.counting.is_some() {
        machine.count_fusion(first.opcode(), rows, true);
        // One call for both ways in: the in-place write did not apply, and the
        // window finished the ordinary way. Whether the ensure above it moved
        // the store is `BoundaryReport::growths`' answer and not this one.
        machine.count_window(first.opcode(), Outcome::Slow);
    }
    Ok(rows)
}

/// An append window's rows, found by opcode: the members
/// [`fused_append_bytes`] and [`fused_append_words`] read their operands out
/// of, and where the commit is.
///
/// One decode for both storages, because there is one grammar. The two fast
/// paths differ in what a unit is and in which questions its family asks, and
/// in nothing about the shape of the window — so a change to
/// `cove_ir::legalize`'s append arrives at both of them or at neither, rather
/// than at whichever of two copies somebody remembered.
struct AppendRows {
    /// The head, the length read.
    first: EncodedInst,
    /// The count constant before the ensure, if one is there.
    count: Option<EncodedInst>,
    /// Where the ensure is, counted from the head — which is also how many
    /// instructions the head and the rows before it are.
    ensure_at: usize,
    ensure: EncodedInst,
    /// The offset constant, on either side of the store read.
    offset: Option<EncodedInst>,
    /// The store read.
    read: EncodedInst,
    /// The `run-copy` into the store.
    copy: EncodedInst,
    /// The clear of the store slot.
    clear: Option<EncodedInst>,
    /// The second count constant, which the commit names.
    second: Option<EncodedInst>,
    /// Where the commit is, counted from the head: the rows after the head,
    /// which is what a fused head answers.
    window: usize,
}

/// [`AppendRows`] of the window whose head is `code[head]`, over the storage
/// whose three opcodes are named.
///
/// `cove_ir::legalize`'s append grammar, and `bytecode::verify` has already
/// matched it against this head — so this is a decode rather than a second
/// recognition, and a `None` is a window shape this does not know, which the
/// rows then run as the primitives they are still encoded as.
///
/// `#[inline(always)]` because each caller is already out of line, and the
/// answer is a handful of registers a caller reads once.
#[inline(always)]
fn append_rows(
    code: &[EncodedInst],
    head: usize,
    ensure_op: u8,
    copy_op: u8,
    commit_op: u8,
) -> Option<AppendRows> {
    // The shortest append is five rows and a function's last row is a
    // terminator, which no window's commit is, so the slice below always holds
    // the whole window.
    let rows = code.get(head..code.len().min(head + cove_ir::legalize::MAX_ROWS))?;
    let is = |at: usize, op: u8| rows.get(at).is_some_and(|held| held.opcode() == op);
    let first = rows[0];
    let mut next = 1;
    let count = is(next, CONST_INT).then(|| rows[next]);
    next += usize::from(count.is_some());
    let ensure_at = next;
    if !is(ensure_at, ensure_op) {
        return None;
    }
    let ensure = rows[ensure_at];
    next += 1;
    let mut offset = is(next, CONST_INT).then(|| rows[next]);
    next += usize::from(offset.is_some());
    if !is(next, LOAD_FIELD) {
        return None;
    }
    let read = rows[next];
    next += 1;
    if offset.is_none() && is(next, CONST_INT) {
        offset = Some(rows[next]);
        next += 1;
    }
    if !is(next, copy_op) {
        return None;
    }
    let copy = rows[next];
    next += 1;
    let clear = is(next, CLEAR).then(|| rows[next]);
    next += usize::from(clear.is_some());
    let second = is(next, CONST_INT).then(|| rows[next]);
    next += usize::from(second.is_some());
    if !is(next, commit_op) {
        return None;
    }
    Some(AppendRows {
        first,
        count,
        ensure_at,
        ensure,
        offset,
        read,
        copy,
        clear,
        second,
        window: next,
    })
}

/// A byte append window run whole, head included, in one call: [ADR 0062]'s
/// append of a run, as [`fused_push`] is its append of one.
///
/// The window is `cove_ir::legalize`'s append over bytes — the length read, an
/// optional count constant, the ensure, an optional offset constant on either
/// side of the store read, the `run-copy` into the store, an optional clear and
/// an optional second count, and the commit — read out by [`append_rows`],
/// which [`fused_append_words`] shares.
///
/// # What it asks
///
/// Every question a row would refuse on, once and up front, of the words the
/// rows would read: the owner is a live `Program::buffer_layout` whose store is
/// `Program::bytes_layout`; the count is not negative; the source is a
/// `String` and the range is inside it; and the copy's charge does not bring
/// a safepoint due inside the window, which [`in_chunks`] would take between
/// the copy and the commit. Any other answer writes nothing and answers `0`:
/// [`fused_window`] then runs the rows the ordinary way, in their own words.
///
/// A `String` source and a `Bytes` store are never one object, so the copy
/// has no overlap to walk backwards from.
///
/// # A growth is the ensure's own
///
/// For [`fused_push`]'s reason. The rows before the ensure are written and
/// counted, the machine is synced to the ensure's pc and `Machine::ensure_growable`
/// grows the store — collecting if it must, refusing at the ensure's span if it
/// cannot — and [`fused_tail`] runs the rest from the row after it, so the copy
/// reads the store the growth left.
///
/// # What it writes and counts
///
/// Every frame word the rows write, in their order — the length, the count,
/// the offset, the store, the clear, the second count — the bytes, and the
/// length. `Machine::instructions` rises by the rows and `bulk_work` by the
/// copy's words, as the rows would have charged them.
///
/// # Why it exists, and why [`fused_window`] asks it
///
/// Without it, a byte append that fused ran its head through `Machine::checked`
/// and each row after it through [`fused_tail`]'s generic arms, and two million
/// appends of three bytes measured 439 ms against 234 ms for the composite
/// `GROWABLE_EXTEND_BYTES` it replaced; with it they measure 191 ms (the
/// `checked` profile, medians of interleaved runs). It is out of line and alone
/// for [`fused_push`]'s reasons.
///
/// It is reached through [`fused_window`], which asks it first, rather than
/// from an arm of the loop's own the way [`fused_push`] is: the loop is then
/// exactly the loop it was, and an append pays one more call's entry, which
/// is small beside a copy. With the loop calling it directly covefmt was no
/// faster (4.78 s against 4.76 s over five interleaved runs, inside the noise),
/// so the shape that leaves the loop alone is the one kept.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn fused_append_bytes(
    machine: &mut Machine<'_>,
    encoded: &Encoded,
    budget: &Meter,
    id: FunctionId,
    base: u64,
    base_at: usize,
    head: usize,
) -> Result<usize, RuntimeError> {
    if machine.instructions + WINDOW_TAIL >= machine.next_check {
        // The head's opcode is read inside the branch, for the reason
        // [`fused_push`]'s is: the row is not loaded yet.
        if machine.counting.is_some() {
            let head = encoded.function(id)[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Safepoint));
        }
        return Ok(0);
    }
    let program = machine.program;
    let code = encoded.function(id);
    let Some(AppendRows {
        first,
        count: count_const,
        ensure_at,
        ensure,
        offset,
        read,
        copy,
        clear,
        second,
        window,
    }) = append_rows(
        code,
        head,
        GROWABLE_ENSURE_BYTES,
        RUN_COPY_BYTES,
        GROWABLE_COMMIT_BYTES,
    )
    else {
        if machine.counting.is_some() {
            let head = code[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Shape));
        }
        return Ok(0);
    };
    let [_, _, src_arg, from_arg, _] = program.arg_list(ArgsId(copy.lo())) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Shape));
        }
        return Ok(0);
    };
    let (src_slot, from_slot) = (src_arg.slot as usize, from_arg.slot as usize);

    let (frame, heap) = machine.mem.stack_and_heap();
    let owner = frame[base_at + first.b() as usize];
    let Some([header, length, stored]) = heap.run_at(owner).and_then(|run| run.get(..3)) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    };
    if header_layout(header.load(Relaxed)) != program.buffer_layout {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    }
    let len = length.load(Relaxed);
    let store = stored.load(Relaxed);
    let Some(dst) = heap.run_at(store) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Store));
        }
        return Ok(0);
    };
    let store_header = dst[0].load(Relaxed);
    if header_layout(store_header) != program.bytes_layout {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Store));
        }
        return Ok(0);
    }
    let src = frame[base_at + src_slot];
    let Some(text) = heap.run_at(src) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Source));
        }
        return Ok(0);
    };
    let text_header = text[0].load(Relaxed);
    if header_layout(text_header) != program.str_layout {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Source));
        }
        return Ok(0);
    }
    // The count and the offset as the copy would read them, after the
    // constants before it have been written: `legalize::recognize` has already
    // held the offset's slot apart from the count's.
    let count_slot = ensure.b() as usize;
    let count = match count_const {
        Some(held) => held.payload(),
        None => frame[base_at + count_slot],
    } as i64;
    let from = match offset {
        Some(held) if held.a() as usize == from_slot => held.payload(),
        _ => frame[base_at + from_slot],
    } as i64;
    let text_len = i64::from(header_len(text_header));
    if count < 0 || from < 0 || from > text_len - count {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Range));
        }
        return Ok(0);
    }
    // In chunks the copy would poll once its charge reached a stride; one that
    // would is left to the rows, whose copy polls where theirs would. So is a
    // copy a chunk boundary of the heap cuts short.
    let (at, from, count) = (len as usize, from as usize, count as usize);
    let words = words_of_bytes(count as i64);
    let reaches =
        |run: &[std::sync::atomic::AtomicU64], bytes: usize| run.len() > bytes.div_ceil(8);
    if count > BULK_CHUNK_BYTES as usize
        || machine.instructions + window as u64 + machine.bulk_work + words
            >= machine.charged_work + SAFEPOINT_STRIDE
        || !reaches(text, from + count)
    {
        // Three reasons in one test, and the test stays one: it short-circuits
        // around `reaches`, which reads the source's run. They are told apart
        // here, in the order the chain asks them.
        if machine.counting.is_some() {
            let why = if count > BULK_CHUNK_BYTES as usize {
                Decline::Bulk
            } else if machine.instructions + window as u64 + machine.bulk_work + words
                >= machine.charged_work + SAFEPOINT_STRIDE
            {
                Decline::Charge
            } else {
                Decline::Chunk
            };
            machine.count_window(first.opcode(), Outcome::Declined(why));
        }
        return Ok(0);
    }

    // The rows before the ensure: the head's length read and the count.
    frame[base_at + first.a() as usize] = len;
    if let Some(held) = count_const {
        frame[base_at + held.a() as usize] = held.payload();
    }
    let capacity = header_len(store_header) as usize;
    if at + count > capacity || !reaches(dst, at + count) {
        // A growth — or a store a chunk boundary cuts short, which the rows
        // copy into the ordinary way — is the ensure's, and the rest is the
        // rows'.
        machine.instructions += ensure_at as u64;
        machine.sync(head + ensure_at);
        if let Err(error) = machine.ensure_growable(owner, Storage::PackedBytes, count as i64) {
            if machine.counting.is_some() {
                machine.count_fusion(first.opcode(), ensure_at, false);
            }
            return Err(error.at(machine.span(id, head + ensure_at)));
        }
        if machine.counting.is_some() {
            // The window ran; the fast path's copy did not. An ensure that
            // refused above records nothing, because no window ran at all.
            machine.count_window(first.opcode(), Outcome::Slow);
        }
        return fused_tail(
            machine,
            program,
            budget,
            code,
            id,
            base,
            head,
            head + ensure_at + 1,
        );
    }

    // Room: the rest of the window, in row order.
    if let Some(held) = offset {
        frame[base_at + held.a() as usize] = held.payload();
    }
    frame[base_at + read.a() as usize] = store;
    let mut done = 0;
    while done < count {
        let (s, d) = (from + done, at + done);
        let take = (8 - s % 8).min(8 - d % 8).min(count - done);
        let mask = if take == 8 {
            u64::MAX
        } else {
            (1u64 << (take * 8)) - 1
        };
        let bits = (text[1 + s / 8].load(Relaxed) >> ((s % 8) * 8)) & mask;
        let shift = (d % 8) * 8;
        let word = &dst[1 + d / 8];
        let held = word.load(Relaxed);
        word.store((held & !(mask << shift)) | (bits << shift), Relaxed);
        done += take;
    }
    if let Some(clear) = clear {
        frame[base_at + clear.a() as usize] = 0;
    }
    if let Some(second) = second {
        frame[base_at + second.a() as usize] = second.payload();
    }
    length.store(len + count as u64, Relaxed);
    machine.instructions += window as u64;
    machine.bulk_work += words;
    if machine.counting.is_some() {
        machine.count_fusion(first.opcode(), window, true);
        machine.count_window(first.opcode(), Outcome::Fast);
    }
    Ok(window)
}

/// A word append window run whole, head included, in one call:
/// [`fused_append_bytes`] over [`Storage::Words`], where a unit is an element
/// of `stride` words rather than a byte.
///
/// The window is the same grammar — [`append_rows`] reads both — with a word
/// ensure, a word `run-copy` and a word commit, and `bytecode::verify` has
/// re-matched it, so the rows are where this reads them.
///
/// # What it asks
///
/// Every question a row would refuse on, once and up front, of the words the
/// rows would read, and each in its own row's terms:
///
/// - the owner is a `Vector` of the ensure's element, as `Machine::vector_run`
///   asks, and its store is live;
/// - the store is a `Shape::Elements` of exactly that element and the source
///   `cove_ir::reads_as_units_of` it — a run, a `Set` or a `Map` — which is
///   [`run_copy_words`]' family rule and not a courtesy: the collector traces
///   each object by its *own* layout's reference map, so a unit copied between
///   runs of two families would be words one map calls integers and the other
///   follows as addresses;
/// - the count is not negative and the source range is inside the source;
/// - the copy's charge does not bring a safepoint due inside the window, which
///   [`in_chunks`] would take between the copy and the commit.
///
/// Any other answer writes nothing and answers `0`: [`fused_window`] then runs
/// the rows the ordinary way, in their own words. So an element of no words
/// is left to the rows too, rather than given a stride of zero here.
///
/// # Overlap, and why nothing more is needed for a run of references
///
/// A word `run-copy`'s two ends may be one object — `RunRange::descending` is
/// what says so — so this walks the words from the tail when the destination
/// is above the source in the store they share, which is the direction
/// `Space::copy` takes for the same range.
///
/// The words moved may be references, and this adds no barrier for the reason
/// [`run_copy_words`] gives: the collector is non-moving and stop-the-world
/// and marks from the roots, with no remembered set a store could fall behind.
/// What a collection needs is a destination it can walk, and no collection can
/// fall inside this copy at all — a poll due inside the window is what the
/// charge question above declines on, so the store is whole before anything
/// may look at it.
///
/// # A growth is the ensure's own
///
/// [`fused_append_bytes`]' arrangement exactly: the rows before the ensure are
/// written and counted, the machine is synced to the ensure's pc and
/// `Machine::ensure_growable` grows the store — collecting if it must,
/// refusing at the ensure's span if it cannot — and [`fused_tail`] runs the
/// rest from the row after it, so the copy reads the store the growth left.
///
/// # What it writes and counts
///
/// Every frame word the rows write, in their order — the length, the count,
/// the offset, the store, the clear, the second count — the elements, and the
/// length. `Machine::instructions` rises by the rows and `bulk_work` by the
/// copy's words, as the rows would have charged them.
///
/// # Why it exists
///
/// `Pattern::AppendWords` had a definition from the start and no producer
/// until the standard library's keyed extend became a window (#409), and until
/// then its head ran through `Machine::checked` and its rows through
/// [`fused_tail`]'s generic arms — the arrangement [`fused_push`] and
/// [`fused_append_bytes`] were each written to replace. Measured on
/// `examples/cq` over 20,000 bookings, which makes 391,679 word append
/// windows, fusing them that way *cost* 0.66% against a build that did not
/// fuse them at all: about 37 ns a window, which is what the five dispatches
/// each one saves are worth. See the module's own note on why this is out of
/// line and why [`fused_window`] asks it rather than the loop.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn fused_append_words(
    machine: &mut Machine<'_>,
    encoded: &Encoded,
    budget: &Meter,
    id: FunctionId,
    base: u64,
    base_at: usize,
    head: usize,
) -> Result<usize, RuntimeError> {
    if machine.instructions + WINDOW_TAIL >= machine.next_check {
        // The head's opcode is read inside the branch, for the reason
        // [`fused_push`]'s is: the row is not loaded yet.
        if machine.counting.is_some() {
            let head = encoded.function(id)[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Safepoint));
        }
        return Ok(0);
    }
    let program = machine.program;
    let code = encoded.function(id);
    let Some(AppendRows {
        first,
        count: count_const,
        ensure_at,
        ensure,
        offset,
        read,
        copy,
        clear,
        second,
        window,
    }) = append_rows(
        code,
        head,
        GROWABLE_ENSURE_WORDS,
        RUN_COPY_WORDS,
        GROWABLE_COMMIT_WORDS,
    )
    else {
        if machine.counting.is_some() {
            let head = code[head].opcode();
            machine.count_window(head, Outcome::Declined(Decline::Shape));
        }
        return Ok(0);
    };
    let [_, _, src_arg, from_arg, _] = program.arg_list(ArgsId(copy.lo())) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Shape));
        }
        return Ok(0);
    };
    let (src_slot, from_slot) = (src_arg.slot as usize, from_arg.slot as usize);
    // The element the ensure names, which `bytecode::verify` has matched with
    // the copy's. Its width is asked before the frame and the heap are
    // borrowed apart, because that reads the whole machine.
    let elem = LayoutId(ensure.lo());
    let stride = machine.width(elem) as usize;
    if stride == 0 {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Source));
        }
        return Ok(0);
    }

    let (frame, heap) = machine.mem.stack_and_heap();
    let owner = frame[base_at + first.b() as usize];
    let Some([header, length, stored]) = heap.run_at(owner).and_then(|run| run.get(..3)) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    };
    let family = program.layout(header_layout(header.load(Relaxed)));
    if !matches!(family.shape, Shape::Vector { elem: held } if held == elem) {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Owner));
        }
        return Ok(0);
    }
    let len = length.load(Relaxed);
    let store = stored.load(Relaxed);
    let Some(dst) = heap.run_at(store) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Store));
        }
        return Ok(0);
    };
    let store_header = dst[0].load(Relaxed);
    let store_shape = &program.layout(header_layout(store_header)).shape;
    if !matches!(store_shape, Shape::Elements { elem: held, .. } if *held == elem) {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Store));
        }
        return Ok(0);
    }
    let src = frame[base_at + src_slot];
    let Some(run) = heap.run_at(src) else {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Source));
        }
        return Ok(0);
    };
    let run_header = run[0].load(Relaxed);
    let run_shape = &program.layout(header_layout(run_header)).shape;
    if !cove_ir::reads_as_units_of(&program.layouts, run_shape, elem) {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Source));
        }
        return Ok(0);
    }
    // The count and the offset as the copy would read them, after the
    // constants before it have been written: `legalize::recognize` has already
    // held the offset's slot apart from the count's.
    let count_slot = ensure.b() as usize;
    let count = match count_const {
        Some(held) => held.payload(),
        None => frame[base_at + count_slot],
    } as i64;
    let from = match offset {
        Some(held) if held.a() as usize == from_slot => held.payload(),
        _ => frame[base_at + from_slot],
    } as i64;
    let run_len = i64::from(header_len(run_header));
    if count < 0 || from < 0 || from > run_len - count {
        if machine.counting.is_some() {
            machine.count_window(first.opcode(), Outcome::Declined(Decline::Range));
        }
        return Ok(0);
    }
    // In chunks the copy would poll once its charge reached a stride; one that
    // would is left to the rows, whose copy polls where theirs would. So is a
    // copy a chunk boundary of the heap cuts short.
    //
    // The chunk bound is [`run_copy_words`]' — whole elements, and at least
    // one — and it is stated here as well as implied by the charge beside it,
    // so that the rule a reader has to know is `in_chunks`' rather than an
    // argument about when `charged_work` was last moved.
    let (at, from, count) = (len as usize, from as usize, count as usize);
    let words = (count * stride) as u64;
    let reaches = |held: &[std::sync::atomic::AtomicU64], units: usize| {
        units
            .checked_mul(stride)
            .is_some_and(|words| held.len() > words)
    };
    if count > (BULK_CHUNK_WORDS as usize / stride).max(1)
        || machine.instructions + window as u64 + machine.bulk_work + words
            >= machine.charged_work + SAFEPOINT_STRIDE
        || !reaches(run, from + count)
    {
        // Three reasons in one test, told apart here and not there, for
        // [`fused_append_bytes`]' reason: the chain short-circuits around a
        // read of the source's run and must keep doing so.
        if machine.counting.is_some() {
            let why = if count > (BULK_CHUNK_WORDS as usize / stride).max(1) {
                Decline::Bulk
            } else if machine.instructions + window as u64 + machine.bulk_work + words
                >= machine.charged_work + SAFEPOINT_STRIDE
            {
                Decline::Charge
            } else {
                Decline::Chunk
            };
            machine.count_window(first.opcode(), Outcome::Declined(why));
        }
        return Ok(0);
    }

    // The rows before the ensure: the head's length read and the count.
    frame[base_at + first.a() as usize] = len;
    if let Some(held) = count_const {
        frame[base_at + held.a() as usize] = held.payload();
    }
    let capacity = header_len(store_header) as usize;
    if at + count > capacity || !reaches(dst, at + count) {
        // A growth — or a store a chunk boundary cuts short, which the rows
        // copy into the ordinary way — is the ensure's, and the rest is the
        // rows'.
        machine.instructions += ensure_at as u64;
        machine.sync(head + ensure_at);
        if let Err(error) = machine.ensure_growable(owner, Storage::Words(elem), count as i64) {
            if machine.counting.is_some() {
                machine.count_fusion(first.opcode(), ensure_at, false);
            }
            return Err(error.at(machine.span(id, head + ensure_at)));
        }
        if machine.counting.is_some() {
            // The window ran; the fast path's copy did not. An ensure that
            // refused above records nothing, because no window ran at all.
            machine.count_window(first.opcode(), Outcome::Slow);
        }
        return fused_tail(
            machine,
            program,
            budget,
            code,
            id,
            base,
            head,
            head + ensure_at + 1,
        );
    }

    // Room: the rest of the window, in row order.
    if let Some(held) = offset {
        frame[base_at + held.a() as usize] = held.payload();
    }
    frame[base_at + read.a() as usize] = store;
    let (into, outof) = (1 + at * stride, 1 + from * stride);
    if store == src && into > outof {
        // One object, shifted up: from the tail, as `Space::copy` walks it.
        for word in (0..words as usize).rev() {
            dst[into + word].store(run[outof + word].load(Relaxed), Relaxed);
        }
    } else {
        for word in 0..words as usize {
            dst[into + word].store(run[outof + word].load(Relaxed), Relaxed);
        }
    }
    if let Some(clear) = clear {
        frame[base_at + clear.a() as usize] = 0;
    }
    if let Some(second) = second {
        frame[base_at + second.a() as usize] = second.payload();
    }
    length.store(len + count as u64, Relaxed);
    machine.instructions += window as u64;
    machine.bulk_work += words;
    if machine.counting.is_some() {
        machine.count_fusion(first.opcode(), window, true);
        machine.count_window(first.opcode(), Outcome::Fast);
    }
    Ok(window)
}

/// The rows of a window after its head, run one by one without a dispatch
/// each: [ADR 0062]'s fused arm for everything [`fused_push`] declines — an
/// append, a push onto an owner it cannot vouch for, a refusal.
///
/// Each row is run by the same code its own arm runs, and in that arm's order:
/// the row is counted first, a refusal is synced to the row's pc and reported
/// at its span, the ensure, the commit and the byte store go through
/// [`buffer_window`] after a sync, and a copy through [`run_copy_bytes`] or
/// [`run_copy_words`]. So a window that fails fails at the pc, with the
/// sentence, the frame and the fuel the unfused rows would have, and an ensure
/// that grows is synced to its own pc before its allocation can collect.
///
/// It stops **before** a row whose count would reach `next_check`. The arm's
/// bound rules that out on entry, and nothing in a window brings the question
/// nearer today — a safepoint inside a bulk copy puts it a stride past the
/// copy — so the compare is what keeps "no question falls inside a fused
/// window" true of a row that one day does, rather than an argument about every
/// row. It stops after the commit, which ends every window. What is left
/// dispatches as ordinary rows. The answer is how many rows ran.
///
/// Out of line for `buffer_window`'s reason: it is the slow path of every
/// fused head, and the loop's frame is the budget.
///
/// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
///
/// `start` is the first row it runs: `head + 1`, or the row after an ensure
/// that [`fused_append_bytes`] or [`fused_append_words`] ran and grew. The
/// answer counts every row after the head either way, which is what the loop
/// steps over.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn fused_tail(
    machine: &mut Machine<'_>,
    program: &Program,
    budget: &Meter,
    code: &[EncodedInst],
    id: FunctionId,
    base: u64,
    head: usize,
    start: usize,
) -> Result<usize, RuntimeError> {
    let base_at = machine.mem.stack_index(base);
    let end = code.len().min(head + cove_ir::legalize::MAX_ROWS);
    let mut pc = start;
    let mut committed = false;
    let mut failed = None;
    while pc < end && !committed && failed.is_none() {
        let held = code[pc];
        let op = held.opcode();
        let member = matches!(
            op,
            CONST_INT
                | CLEAR
                | LOAD_FIELD
                | STORE_ELEM
                | RUN_COPY_BYTES
                | RUN_COPY_WORDS
                | GROWABLE_ENSURE_BYTES
                | GROWABLE_ENSURE_WORDS
                | GROWABLE_COMMIT_BYTES
                | GROWABLE_COMMIT_WORDS
                | RUN_STORE_BYTES
        );
        if !member || machine.instructions + 1 >= machine.next_check {
            break;
        }
        machine.instructions += 1;
        let at = pc;
        let refused = move |machine: &mut Machine<'_>, error: RuntimeError| {
            machine.sync(at);
            error.at(machine.span(id, at))
        };
        let ran = match op {
            CONST_INT => {
                machine
                    .mem
                    .set_word_at(base_at + held.a() as usize, held.payload());
                Ok(())
            }
            CLEAR => {
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.clear_words(base + held.a() as u64, width);
                Ok(())
            }
            LOAD_FIELD => {
                let addr = machine.mem.word_at(base_at + held.b() as usize);
                let field = held.lo();
                let width = machine.width(LayoutId(held.hi()));
                machine.checked(addr, field, width).map(|()| {
                    let from = machine.mem.payload_addr(addr, field);
                    machine.mem.copy_words(base + held.a() as u64, from, width);
                })
            }
            STORE_ELEM => {
                let addr = machine.mem.word_at(base_at + held.a() as usize);
                let index = machine.mem.word_at(base_at + held.b() as usize) as i64;
                let width = machine.width(LayoutId(held.lo()));
                machine.element(addr, index, width).map(|offset| {
                    let into = machine.mem.payload_addr(addr, offset);
                    machine.mem.copy_words(into, base + held.c() as u64, width);
                })
            }
            // A copy words its own refusals and gives them their span.
            RUN_COPY_BYTES => {
                machine.sync(at);
                let args = program.arg_list(ArgsId(held.lo()));
                if let Err(error) = run_copy_bytes(machine, program, budget, base, args, id, at) {
                    failed = Some(error);
                }
                Ok(())
            }
            RUN_COPY_WORDS => {
                machine.sync(at);
                let args = program.arg_list(ArgsId(held.lo()));
                let elem = LayoutId(held.hi());
                if let Err(error) =
                    run_copy_words(machine, program, budget, base, args, elem, id, at)
                {
                    failed = Some(error);
                }
                Ok(())
            }
            // The ensure, the commit and the byte store, as their arm runs them.
            _ => {
                machine.sync(at);
                committed = matches!(op, GROWABLE_COMMIT_BYTES | GROWABLE_COMMIT_WORDS);
                buffer_window(machine, held, base_at)
            }
        };
        if let Err(error) = ran {
            failed = Some(refused(machine, error));
        }
        pc += 1;
    }
    let rows = pc - (head + 1);
    if machine.counting.is_some() {
        let whole = committed && failed.is_none();
        machine.count_fusion(code[head].opcode(), rows, whole);
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(rows),
    }
}

pub(super) fn dispatch<'s, 'a>(
    machine: &mut Machine<'a>,
    encoded: &Encoded,
    budget: &Meter,
    threads: &'s Scope<'s, 'a>,
    running: &mut Vec<Option<ScopedJoinHandle<'s, super::Outcome>>>,
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
        // ADR 0054's fused comparison, which is the comparison above and then
        // the branch below it, in that order and with no condition: the `Bool`
        // is written exactly as the unfused pair wrote it, and *then* the
        // target is taken when it is false. Nothing here asks whether anything
        // reads the slot — see `Inst::CmpBranch` for why that question is not
        // worth its answer — so these arms are the `cmp_*` ones above with two
        // lines added.
        macro_rules! took {
            ($answer:expr) => {{
                let answer: bool = $answer;
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
                if !answer {
                    pc = pc.wrapping_add_signed(held.payload() as i64 as isize);
                }
            }};
        }
        macro_rules! cmp_int_branch {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let y = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                took!(compare($op, x.cmp(&y)))
            }};
        }
        macro_rules! cmp_float_branch {
            ($answer:expr) => {{
                let x = f64::from_bits(machine.mem.word_at(base_at + (b!() as usize)));
                let y = f64::from_bits(machine.mem.word_at(base_at + (c!() as usize)));
                #[allow(clippy::redundant_closure_call)]
                let answer = ($answer)(x, y);
                took!(answer)
            }};
        }
        macro_rules! cmp_str_branch {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let y = machine.mem.word_at(base_at + (c!() as usize));
                took!(compare($op, machine.compare_strings(x, y)))
            }};
        }
        macro_rules! cmp_word_branch {
            ($equal:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let y = machine.mem.word_at(base_at + (c!() as usize));
                took!((x == y) == $equal)
            }};
        }
        // The immediate form's payload is two halves rather than one word, so
        // the displacement is `hi` and read as an `i32`. That is the whole of
        // what ADR 0054's narrowing costs at run time.
        macro_rules! cmp_imm_branch {
            ($op:expr) => {{
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let answer = compare($op, x.cmp(&i64::from(held.lo() as i32)));
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
                if !answer {
                    pc = pc.wrapping_add_signed(held.hi() as i32 as isize);
                }
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
        // The frame is open and its arguments are in it. Which tier runs it is
        // [ADR 0055](../../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
        // function-entry table, and **this is where the encoded tier asks it** —
        // the one thing that makes native coverage compositional, because until
        // it did, a compiled function called from an encoded one stayed encoded.
        //
        // What a default run pays is [`Machine::tiered`]'s single `Option` test,
        // at a `call` and nowhere else. Everything past it is out of line, for
        // this loop's own stated reason: what sits inside `dispatch` is paid for
        // by every instruction of every program whether or not it runs.
        macro_rules! crossed {
            ($callee:expr, $callee_base:expr, $dst:expr) => {{
                match machine.tiered($callee) {
                    None => entered!($callee, $callee_base, $dst),
                    Some(entry) => {
                        // The caller's pc is where it resumes, one past the `call`,
                        // exactly as `entered!` leaves it: a frame waiting on a
                        // call is read at `pc - 1` by `Machine::call_chain` and by
                        // the debugger, and this one synced the `call` itself
                        // until an error raised in compiled code named the
                        // instruction before the call as its call site. The span
                        // a failure below is reported at is still the `call`'s,
                        // read at `pc - 1` explicitly. Execution resumes at the
                        // dispatch loop's own `pc`, because the answer arrives in
                        // `dst` rather than through a `return` this loop runs.
                        machine.sync(pc);
                        match native::from_encoded(
                            machine,
                            budget,
                            entry,
                            $callee,
                            $callee_base,
                            base,
                            $dst,
                        ) {
                            Ok(()) => {}
                            // Not `fail!`, and the difference is a bug this had
                            // first: `fail!` syncs the *top* frame, and on this
                            // path the top frame is the failed callee's — its
                            // frames are still standing, which is what the error's
                            // call chain is read out of. Syncing here would
                            // overwrite the innermost frame's pc with this call's
                            // and report the wrong span for where it failed. The
                            // caller's own pc was synced before the call, and
                            // `.at` is `get_or_insert`, so the span below is used
                            // only for a failure that carries none of its own.
                            Err(error) => return Err(error.at(machine.span(id, pc - 1))),
                        }
                    }
                }
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

            // ADR 0059's three-way order: `-1`, `0` or `1` as an `Int` word.
            // A `Bool` is `0` or `1` and a case index is small and never
            // negative, so the signed reading an `Int` takes orders all three —
            // `false` first, and cases by index, which the lowering asks for
            // only where that is the case-name order a key sorts by.
            ORDER_INT | ORDER_BOOL | ORDER_TAG => {
                let x = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let y = machine.mem.word_at(base_at + (c!() as usize)) as i64;
                let answer = i64::from(x > y) - i64::from(x < y);
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }
            // Out of line, and reading the bytes where they are: a search step
            // over `String` keys is one of these. `cmp_str!` reaches the same
            // helper now rather than copying both strings into two vectors of
            // its own, so the two arms differ in what they answer and no
            // longer in what they pay.
            ORDER_STR => {
                let x = machine.mem.word_at(base_at + (b!() as usize));
                let y = machine.mem.word_at(base_at + (c!() as usize));
                let answer = machine.order_strings(x, y);
                machine
                    .mem
                    .set_word_at(base_at + (a!()) as usize, answer as u64);
            }
            // A `Float` has no total order and an identity none a program may
            // see; `cove_ir::verify` refuses both before this could run.
            ORDER_FLOAT | ORDER_REF => not_ordered!(),

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
            // A relabel: the word is a count of nanoseconds on both sides, and
            // only the slot's `Repr` changes.
            DURATION_TO_INT | INT_TO_DURATION => {
                let x = machine.mem.word_at(base_at + (b!() as usize));
                machine.mem.set_word_at(base_at + (a!()) as usize, x);
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

            EQ_INT_BRANCH => cmp_int_branch!(CmpOp::Eq),
            NE_INT_BRANCH => cmp_int_branch!(CmpOp::Ne),
            LT_INT_BRANCH => cmp_int_branch!(CmpOp::Lt),
            LE_INT_BRANCH => cmp_int_branch!(CmpOp::Le),
            GT_INT_BRANCH => cmp_int_branch!(CmpOp::Gt),
            GE_INT_BRANCH => cmp_int_branch!(CmpOp::Ge),

            EQ_FLOAT_BRANCH => cmp_float_branch!(|x, y| x == y),
            NE_FLOAT_BRANCH => cmp_float_branch!(|x: f64, y: f64| x != y),
            LT_FLOAT_BRANCH => cmp_float_branch!(|x, y| x < y),
            LE_FLOAT_BRANCH => cmp_float_branch!(|x, y| x <= y),
            GT_FLOAT_BRANCH => cmp_float_branch!(|x, y| x > y),
            GE_FLOAT_BRANCH => cmp_float_branch!(|x, y| x >= y),

            EQ_BOOL_BRANCH | EQ_REF_BRANCH | EQ_TAG_BRANCH => cmp_word_branch!(true),
            NE_BOOL_BRANCH | NE_REF_BRANCH | NE_TAG_BRANCH => cmp_word_branch!(false),
            LT_BOOL_BRANCH | LE_BOOL_BRANCH | GT_BOOL_BRANCH | GE_BOOL_BRANCH | LT_REF_BRANCH
            | LE_REF_BRANCH | GT_REF_BRANCH | GE_REF_BRANCH | LT_TAG_BRANCH | LE_TAG_BRANCH
            | GT_TAG_BRANCH | GE_TAG_BRANCH => not_ordered!(),

            EQ_STR_BRANCH => cmp_str_branch!(CmpOp::Eq),
            NE_STR_BRANCH => cmp_str_branch!(CmpOp::Ne),
            LT_STR_BRANCH => cmp_str_branch!(CmpOp::Lt),
            LE_STR_BRANCH => cmp_str_branch!(CmpOp::Le),
            GT_STR_BRANCH => cmp_str_branch!(CmpOp::Gt),
            GE_STR_BRANCH => cmp_str_branch!(CmpOp::Ge),

            EQ_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Eq),
            NE_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Ne),
            LT_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Lt),
            LE_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Le),
            GT_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Gt),
            GE_INT_IMM_BRANCH => cmp_imm_branch!(CmpOp::Ge),

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
                    Ok(callee_base) => crossed!(callee, callee_base, dst),
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
                    // A closure's body is a function like any other, and its
                    // captures are already in the slots `Function::captures`
                    // names — `open_frame` put them there. So the tier table is
                    // asked here for the same reason it is asked at a `call`:
                    // coverage that stopped at a closure would not be
                    // compositional either.
                    Ok(callee_base) => crossed!(callee, callee_base, dst),
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
            INTRINSIC_CALL => {
                machine.sync(pc - 1);
                let dst = a!();
                if let Err(error) =
                    machine.call_intrinsic(base, dst, SiteId(held.lo()), ArgsId(held.hi()))
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
            // the calling and 10 was the reading. See `Inst::RunLoad`.
            RUN_LOAD_BYTES => {
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
            // ADR 0058's run copy, one arm per storage. All five operands —
            // `dst`, `dst_at`, `src`, `src_at`, `count` — live behind the
            // `ArgsId` in the payload's low half rather than in `a`, `b` and
            // `c`; see `Inst::RunCopy`'s doc for why. A word copy's element
            // layout is the high half, so the storage is never a tag this loop
            // reads: it is which arm the opcode already chose.
            //
            // `Machine::copy_string_bytes` and `Memory::copy_words` document
            // their callers as owning every bound they copy within, so each
            // helper checks everything before its first write.
            RUN_COPY_BYTES => {
                machine.sync(pc - 1);
                // The whole of this instruction lives behind one call. The
                // bounds checks and the chunk loop together are far more code
                // than a dispatch arm should put in the way of the arms around
                // it — ADR 0051 named that cost when it added the opcodes, and
                // `crate::vm::debug` measured 4.3% for a smaller body in this
                // same loop.
                let args = program.arg_list(ArgsId(held.lo()));
                run_copy_bytes(machine, program, budget, base, args, id, pc - 1)?;
            }
            RUN_COPY_WORDS => {
                machine.sync(pc - 1);
                let args = program.arg_list(ArgsId(held.lo()));
                let elem = LayoutId(held.hi());
                run_copy_words(machine, program, budget, base, args, elem, id, pc - 1)?;
            }
            // ADR 0058's exact construction: an allocation and the copy that
            // fills it, as one arm and one call per storage, for
            // `RUN_COPY_BYTES`' reason. `dst`, `src`, `from` and `count` are the
            // row; a word slice's element layout is the high half, and the
            // answer's layout is the row's `dst` — a `String` for bytes.
            RUN_SLICE_BYTES => {
                machine.sync(pc - 1);
                let args = program.arg_list(ArgsId(held.lo()));
                run_slice_bytes(machine, program, budget, base, args, id, pc - 1)?;
            }
            RUN_SLICE_WORDS => {
                machine.sync(pc - 1);
                let args = program.arg_list(ArgsId(held.lo()));
                let elem = LayoutId(held.hi());
                run_slice_words(machine, program, budget, base, args, elem, id, pc - 1)?;
            }
            // ADR 0065's run search, one arm and one call for
            // `RUN_COPY_BYTES`' reason, and one opcode rather than two because
            // it has one storage. `dst`, `haystack`, `needle` and `from` are
            // the row, and `dst` is the only one of the four it writes.
            RUN_FIND_BYTES => {
                machine.sync(pc - 1);
                let args = program.arg_list(ArgsId(held.lo()));
                run_find_bytes(machine, program, budget, base, args, id, pc - 1)?;
            }
            // ADR 0058's growable family over bytes, and its finish — ADR 0052's
            // four. Each arm is a read of its operands and one call,
            // for `RUN_COPY_BYTES`' reason: the checks, the capacity arithmetic and
            // the growth are far more code than a dispatch arm should put in the
            // way of the arms around it. `Machine::alloc_buffer` documents which
            // of its two allocations happens first and why nothing is lost
            // between them.
            GROWABLE_ALLOC_BYTES => {
                let capacity = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                machine.sync(pc - 1);
                match machine.alloc_buffer(capacity) {
                    Ok(owner) => machine.mem.set_word_at(base_at + (a!()) as usize, owner),
                    Err(error) => fail!(error),
                }
            }
            // The same allocation over words: `core.vectorWithCapacity`, the
            // empty `Vector` a keyed update is built in. The element layout is
            // the payload's low half and is the *only* layout the row carries —
            // `Machine::alloc_vector` reads the owner's and the store's off the
            // table the machine built from the program's layouts, because an
            // allocation is the one growable operation with no object to read a
            // layout from yet.
            GROWABLE_ALLOC_WORDS => {
                let capacity = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let elem = LayoutId(held.lo());
                machine.sync(pc - 1);
                match machine.alloc_vector(elem, capacity) {
                    Ok(owner) => machine.mem.set_word_at(base_at + (a!()) as usize, owner),
                    Err(error) => fail!(error),
                }
            }
            // `Vector.pop` and `Vector.remove`'s last step since ADR 0058: the
            // length lowered and the vacated element cleared, as one call.
            GROWABLE_TRUNCATE_WORDS => {
                let owner = machine.mem.word_at(base_at + (a!() as usize));
                let len = machine.mem.word_at(base_at + (b!() as usize)) as i64;
                let elem = LayoutId(held.lo());
                machine.sync(pc - 1);
                if let Err(error) = machine.truncate_words(owner, elem, len) {
                    fail!(error);
                }
            }
            // ADR 0052's finish: the *live prefix* validated once, and the store
            // relabelled down from its capacity to that length without copying a
            // byte. The owner is then emptied, because finishing consumes. The
            // opcode is the validation — a byte finish is a UTF-8 finish — and
            // the target layout is the payload's low half.
            RUN_FINISH_BYTES => {
                let owner = machine.mem.word_at(base_at + (b!() as usize));
                let target = LayoutId(held.lo());
                machine.sync(pc - 1);
                match machine.finish_buffer(owner, target, Validation::Utf8) {
                    Ok(text) => machine.mem.set_word_at(base_at + (a!()) as usize, text),
                    Err(error) => fail!(error),
                }
            }
            // A word finish: `Vector.freeze()`, since ADR 0058 moved it into the
            // standard library. The store is relabelled to the `Array` the
            // payload's low half names and the element layout is its high half;
            // there is nothing to validate in a run of whole elements.
            RUN_FINISH_WORDS => {
                let owner = machine.mem.word_at(base_at + (b!() as usize));
                let target = LayoutId(held.lo());
                let elem = LayoutId(held.hi());
                machine.sync(pc - 1);
                match machine.finish_words(owner, target, elem) {
                    Ok(array) => machine.mem.set_word_at(base_at + (a!()) as usize, array),
                    Err(error) => fail!(error),
                }
            }
            // ADR 0062's window: all five opcodes in one arm and one call. An
            // ensure may grow the store, and a growth allocates, so the arm is
            // synced first, as `GROWABLE_TRUNCATE_WORDS` is; the owner is a frame slot
            // and the old store is reachable from it, so a collection inside the
            // growth frees neither. See `buffer_window` for why the arm is no
            // more than this.
            GROWABLE_ENSURE_BYTES
            | GROWABLE_ENSURE_WORDS
            | GROWABLE_COMMIT_BYTES
            | GROWABLE_COMMIT_WORDS
            | RUN_STORE_BYTES => {
                machine.sync(pc - 1);
                if let Err(error) = buffer_window(machine, held, base_at) {
                    fail!(error);
                }
            }
            // ADR 0062's fused heads: one call, which runs the head and as much
            // of the window after it as may run without a dispatch, and answers
            // how many rows that was. See `fused_window`.
            FUSED_PUSH_WORDS | FUSED_PUSH_BYTE => {
                pc += match fused_push(machine, encoded, id, base, base_at, pc - 1)? {
                    0 => fused_window(machine, encoded, budget, id, base, pc - 1)?,
                    rows => rows,
                };
            }
            FUSED_APPEND_BYTES | FUSED_APPEND_WORDS => {
                pc += fused_window(machine, encoded, budget, id, base, pc - 1)?;
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
    use cove_ir::{Convert as ConvertTo, Inst, Len, Shape, Storage, Validation};

    use super::super::runs::MIN_GROWABLE_BYTES;
    use super::super::tests::{budget, run_words, Build};
    use super::super::{SCRATCH_BUFFERS, SCRATCH_BYTES};
    use super::*;

    /// Every opcode ADR 0041 defines has an implementation.
    ///
    /// `crates/cove-cli/tests/bytecode_corpus.rs` names sixteen opcodes no
    /// program in the repository reaches, and four of them — `addr.elem`,
    /// `Convert(IntToFloat)`, `Convert(FloatToInt)` and `layout.of` — are not
    /// merely absent from the corpus: **the lowering has no site that emits
    /// three of them**, so no Cove source can reach them and neither the
    /// differential harness nor any fixture written in Cove can cover them.
    /// (`Convert(IntToFloat)` has had a site since ADR 0058's Phase 5 made
    /// `Int.toFloat` one; it stays below as the way out to the `Float` the
    /// other conversion reads.)
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

    /// **ADR 0054's fused comparison is the two instructions it replaces, in
    /// that order.**
    ///
    /// Three fixtures, because the instruction makes three claims and a test
    /// of one of them would pass while another was wrong:
    ///
    /// - `answered` branches to the instruction *after* itself, so the branch
    ///   is a no-op whichever way it goes and what comes back is the `Bool` it
    ///   wrote. That is the claim the ADR rests the whole design on — the
    ///   write is kept — and it is the one an implementation that skipped the
    ///   store would fail here rather than somewhere downstream.
    /// - `took` answers 1 where it fell through and 2 where it branched, which
    ///   is the branch itself, in both directions.
    /// - `took_imm` is the same question of the immediate form, whose target
    ///   is in the payload's high half rather than in the whole word.
    ///
    /// Built in the IR directly, because nothing lowers a fused comparison: it
    /// is a peephole over finished code, so a fixture written in Cove would be
    /// testing that the peephole fired as well as what the instruction does.
    #[test]
    fn a_fused_comparison_writes_its_bool_and_then_branches() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let boolean = build.scalar(Repr::Bool);
        let lt = |target| Inst::CmpBranch {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: 2,
            a: 0,
            b: 1,
            target,
        };
        let answered = build.function(
            "answered",
            &[int, int],
            &[Repr::Int, Repr::Int, Repr::Bool],
            boolean,
            // The target is the next instruction, so the branch cannot be told
            // from the fall-through and the answer is the `Bool` alone.
            vec![lt(1), Inst::Return { src: 2 }],
        );
        let took = build.function(
            "took",
            &[int, int],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
            vec![
                lt(3),
                Inst::Int { dst: 3, value: 1 },
                Inst::Return { src: 3 },
                Inst::Int { dst: 3, value: 2 },
                Inst::Return { src: 3 },
            ],
        );
        let took_imm = build.function(
            "took_imm",
            &[int],
            &[Repr::Int, Repr::Bool, Repr::Int],
            int,
            vec![
                Inst::CmpImmBranch {
                    op: CmpOp::Eq,
                    dst: 1,
                    a: 0,
                    value: 32,
                    target: 3,
                },
                Inst::Int { dst: 2, value: 1 },
                Inst::Return { src: 2 },
                Inst::Int { dst: 2, value: 2 },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();

        // The `Bool`, which is what the fused instruction wrote and nothing
        // else touched.
        assert_eq!(
            run_words(&program, answered, &[1, 2]).expect("the fixture runs"),
            vec![1]
        );
        assert_eq!(
            run_words(&program, answered, &[2, 1]).expect("the fixture runs"),
            vec![0]
        );
        assert_eq!(
            run_words(&program, answered, &[2, 2]).expect("the fixture runs"),
            vec![0]
        );
        // And the branch: false takes the target, true falls through.
        assert_eq!(
            run_words(&program, took, &[1, 2]).expect("the fixture runs"),
            vec![1]
        );
        assert_eq!(
            run_words(&program, took, &[2, 1]).expect("the fixture runs"),
            vec![2]
        );
        assert_eq!(
            run_words(&program, took_imm, &[32]).expect("the fixture runs"),
            vec![1]
        );
        assert_eq!(
            run_words(&program, took_imm, &[33]).expect("the fixture runs"),
            vec![2]
        );
        // A negative immediate, because the low half is an `i32` and a reading
        // that lost the sign would call every negative bound enormous.
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let negative = build.function(
            "negative",
            &[int],
            &[Repr::Int, Repr::Bool, Repr::Int],
            int,
            vec![
                Inst::CmpImmBranch {
                    op: CmpOp::Lt,
                    dst: 1,
                    a: 0,
                    value: -5,
                    target: 3,
                },
                Inst::Int { dst: 2, value: 1 },
                Inst::Return { src: 2 },
                Inst::Int { dst: 2, value: 2 },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(
            run_words(&program, negative, &[(-6i64) as u64]).expect("the fixture runs"),
            vec![1]
        );
        assert_eq!(
            run_words(&program, negative, &[0]).expect("the fixture runs"),
            vec![2]
        );
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

    // ---- ADR 0058: a run copy of packed bytes ----------------------------

    /// A program with the one function the byte `run-copy` tests share, so
    /// each test builds a fixture and none builds a compiler.
    ///
    /// - `copy_into(dst, dst_at, src, src_at, len) -> Ref` copies and
    ///   answers `dst`, so both the successful matrix and every refusal path
    ///   run through one function.
    ///
    /// A destination is a `Shape::Bytes` run the test allocates directly with
    /// `Machine::allocate`, at exactly the length the case needs: no
    /// instruction allocates a bare fixed run, and a buffer's store would be
    /// raised to the growable floor.
    struct Fixture {
        program: Program,
        copy_into: FunctionId,
    }

    fn fixture() -> Fixture {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let bytes = build.bytes_layout();

        let args = build.args(&[(0, bytes), (1, int), (2, bytes), (3, int), (4, int)]);
        let copy_into = build.function(
            "copy_into",
            &[bytes, int, bytes, int, int],
            &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            bytes,
            vec![
                Inst::RunCopy {
                    args,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        Fixture { program, copy_into }
    }

    #[test]
    fn run_copy_bytes_refuses_a_null_destination() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let src = machine.new_string("source").unwrap();
        let error = machine
            .run(copy_into, &[0, 0, src, 0, 3], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn run_copy_bytes_refuses_a_null_source() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.allocate(program.bytes_layout, 3).unwrap();
        let error = machine
            .run(copy_into, &[dst, 0, 0, 0, 3], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    #[test]
    fn run_copy_bytes_refuses_a_negative_length() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.allocate(program.bytes_layout, 3).unwrap();
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
    fn run_copy_bytes_refuses_a_string_destination() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.new_string("already a string").unwrap();
        let src = machine.new_string("abc").unwrap();
        let error = machine
            .run(copy_into, &[dst, 0, src, 0, 3], &budget())
            .unwrap_err();
        assert!(error.message.contains("destination"), "{}", error.message);
    }

    #[test]
    fn run_copy_bytes_refuses_an_out_of_range_source() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.allocate(program.bytes_layout, 10).unwrap();
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
    fn run_copy_bytes_refuses_an_out_of_range_destination() {
        let Fixture { program, copy_into } = fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.allocate(program.bytes_layout, 3).unwrap();
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

    /// A byte `RunCopy` from a `String` source and from another `Shape::Bytes`
    /// run, at aligned and unaligned `dst_at`/`src_at`, agrees with Rust's
    /// own byte-slicing of the same data — including the zero-length copy,
    /// which is the one case that touches no byte at all.
    #[test]
    fn run_copy_bytes_agrees_with_rust_at_every_alignment() {
        let Fixture { program, copy_into } = fixture();
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
                    .allocate(program.bytes_layout, DST_LEN as i64)
                    .unwrap();
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

    // --- ADR 0065: a run search ------------------------------------------

    /// `find(haystack, needle, from) -> Int`: one byte `run-find` and a
    /// return, so every case below runs the instruction and nothing else.
    ///
    /// The row is `dst`, `haystack`, `needle`, `from` — `dst` first and
    /// written, as a run slice's is, and an `Int` rather than a reference,
    /// because the answer is an offset and not a run.
    fn find_fixture() -> (Program, FunctionId) {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let text = build.string_layout();
        let row = build.args(&[(3, int), (0, text), (1, text), (2, int)]);
        let entry = build.function(
            "find",
            &[text, text, int],
            &[Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::RunFind {
                    args: row,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 3 },
            ],
        );
        (build.done(), entry)
    }

    /// A `String` object holding exactly `bytes`, whatever they are.
    ///
    /// `Machine::new_string` takes a `&str` and so cannot make one of these.
    /// The instruction is defined over **bytes** and has no notion of a
    /// character, so the corpus it is tested against has to be able to hold a
    /// `0x00`, a lone `0x80` and an `0xff` — the byte patterns a valid
    /// `String`'s payload is full of *inside* its characters, and the ones an
    /// implementation that quietly decoded would answer differently for.
    fn run_of(machine: &mut Machine<'_>, bytes: &[u8]) -> u64 {
        let addr = machine
            .new_string_of(bytes.len() as i64)
            .expect("the heap has room");
        for (at, chunk) in bytes.chunks(8).enumerate() {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            machine
                .mem
                .set_payload(addr, at as u32, u64::from_le_bytes(word));
        }
        addr
    }

    /// What the instruction must answer, by `str::find`'s rule over bytes:
    /// the first offset at or after `from` where the needle occurs, or -1.
    fn want_find(haystack: &[u8], needle: &[u8], from: usize) -> i64 {
        if needle.is_empty() {
            return from as i64;
        }
        if from > haystack.len() || needle.len() > haystack.len() - from {
            return -1;
        }
        haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map_or(-1, |at| (at + from) as i64)
    }

    /// One search, run through the machine.
    fn found(
        program: &Program,
        entry: FunctionId,
        haystack: &[u8],
        needle: &[u8],
        from: i64,
    ) -> i64 {
        let mut machine = Machine::new(program, 1 << 20);
        let hay = run_of(&mut machine, haystack);
        let sought = run_of(&mut machine, needle);
        machine
            .run(entry, &[hay, sought, from as u64], &budget())
            .expect("a search answers")[0] as i64
    }

    /// The haystacks and needles every differential case is drawn from.
    ///
    /// Bytes rather than text, and deliberately: `0x00`, `0x80` and `0xff`
    /// are all in here, because the run this instruction searches is a run of
    /// bytes and the day one of them is read as a character is the day this
    /// suite has to notice.
    fn find_corpus() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"ab".to_vec(),
            b"aaaa".to_vec(),
            b"abab".to_vec(),
            b"aabaab".to_vec(),
            b"aaab".to_vec(),
            b"abcabcabd".to_vec(),
            b"abcabd".to_vec(),
            b"banana banana bandana".to_vec(),
            b"the quick brown fox".to_vec(),
            vec![0x00, 0x01, 0x00, 0x00, 0x01],
            vec![0xff, 0x80, 0xc3, 0xa9, 0xe3, 0x81, 0x82, 0x00],
            vec![0x80; 20],
            vec![0x00; 9],
        ];
        // Many near misses and a long repeated prefix: the shape a quadratic
        // scan takes quadratic time on and a wrong shift answers wrongly on.
        let mut near = Vec::new();
        for _ in 0..40 {
            near.extend_from_slice(b"aaaaaaab");
        }
        near.extend_from_slice(b"aaaaaaaa");
        out.push(near);
        // A needle far longer than one safepoint step, and a haystack that
        // holds it at the last offset it fits at.
        let long: Vec<u8> = (0..(SAFEPOINT_STRIDE as u32 * 3 + 7))
            .map(|at| (at % 251) as u8)
            .collect();
        let mut with_long = vec![0x7fu8; SAFEPOINT_STRIDE as usize * 2 + 3];
        with_long.extend_from_slice(&long);
        out.push(long);
        out.push(with_long);
        out
    }

    /// **The instruction answers what `str::find` answers, for every pair of
    /// the corpus.**
    ///
    /// The matcher under `run-find` is written by hand, so this is the case
    /// that matters most: a corpus crossing haystacks against needles,
    /// including periodic needles, needles equal to their haystack, needles
    /// at offset 0 and at the last offset they fit at, haystacks full of near
    /// misses, and a needle three safepoint steps long.
    #[test]
    fn run_find_bytes_answers_what_rust_answers() {
        let (program, entry) = find_fixture();
        let corpus = find_corpus();
        for haystack in &corpus {
            for needle in &corpus {
                assert_eq!(
                    found(&program, entry, haystack, needle, 0),
                    want_find(haystack, needle, 0),
                    "haystack {} byte(s), needle {} byte(s)",
                    haystack.len(),
                    needle.len()
                );
            }
        }
    }

    /// **And for every `from` in range**, on the pairs where a start offset
    /// can move the answer.
    ///
    /// `from == haystack_len` is in the range and is not an error: it answers
    /// -1, or `from` itself for an empty needle.
    #[test]
    fn run_find_bytes_answers_from_every_start() {
        let (program, entry) = find_fixture();
        let haystacks: Vec<Vec<u8>> = vec![
            b"aaaaaaaaaa".to_vec(),
            b"abababababab".to_vec(),
            b"banana banana".to_vec(),
            b"abcabcabd".to_vec(),
            vec![0x00, 0x80, 0x00, 0x80, 0x00],
        ];
        let needles: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"aa".to_vec(),
            b"aaaa".to_vec(),
            b"abab".to_vec(),
            b"aabaab".to_vec(),
            b"abcabd".to_vec(),
            b"ana".to_vec(),
            vec![0x80, 0x00],
        ];
        for haystack in &haystacks {
            for needle in &needles {
                for from in 0..=haystack.len() {
                    assert_eq!(
                        found(&program, entry, haystack, needle, from as i64),
                        want_find(haystack, needle, from),
                        "haystack {haystack:?}, needle {needle:?}, from {from}"
                    );
                }
            }
        }
    }

    /// The needle and the haystack the bound cases share: `m` and `n` bytes
    /// that agree nowhere, so the search runs to the end and every turn of
    /// both phases moves its counter — which makes the units the test names
    /// the units the instruction consumes.
    fn straight_scan(m: usize, n: usize) -> (Vec<u8>, Vec<u8>) {
        (vec![b'B'; n], vec![b'A'; m])
    }

    /// The machine and the budget a bound case runs one search under.
    fn under_fuel(
        program: &Program,
        entry: FunctionId,
        haystack: &[u8],
        needle: &[u8],
        from: i64,
        fuel: u64,
    ) -> (u64, u64, crate::trace::RunOutcome) {
        let budget = crate::budget::Budget::new(crate::budget::Limits {
            fuel: Some(fuel),
            ..crate::budget::Limits::default()
        });
        let mut machine = Machine::new(program, 1 << 20);
        let hay = run_of(&mut machine, haystack);
        let sought = run_of(&mut machine, needle);
        let error = machine
            .run(entry, &[hay, sought, from as u64], &budget.meter())
            .expect_err("a search past its fuel is stopped");
        (machine.bulk_work, budget.fuel_spent(), error.outcome)
    }

    /// **A needle far longer than one step is prepared in steps, and a fuel
    /// bound stops the run *during the preparation*.**
    ///
    /// This is the case the window rule got wrong and the reason ADR 0065's
    /// Decision 4 was rewritten: a rule stated over a window of
    /// `max(SAFEPOINT_STRIDE, m)` made the preparation of a long needle
    /// uninterruptible before the first window even began. The bound is over
    /// *work done*, so preparation is charged a unit per unit of the needle
    /// and polls between steps like everything else.
    #[test]
    fn a_run_find_stops_inside_a_long_needles_preparation() {
        const M: usize = SAFEPOINT_STRIDE as usize * 5;
        const N: usize = SAFEPOINT_STRIDE as usize * 20;
        let (program, entry) = find_fixture();
        let (haystack, needle) = straight_scan(M, N);
        for fuel in [1u64, 1_500, 3_000] {
            let (charged, spent, outcome) =
                under_fuel(&program, entry, &haystack, &needle, 0, fuel);
            assert_eq!(outcome, crate::trace::RunOutcome::Fuel);
            assert!(
                charged < M as u64,
                "a fuel limit of {fuel} left {charged} unit(s) charged, which is the whole \
                 {M}-unit preparation or past it"
            );
            assert!(
                spent <= fuel + 2 * SAFEPOINT_STRIDE,
                "a fuel limit of {fuel} spent {spent}, past one step of units and the stride \
                 the loop gathers before it looks"
            );
        }
    }

    /// **With fuel enough to finish preparing and not to search, the run
    /// stops during the first stretch of the search** — and within the same
    /// bound, because the step is the same step whichever phase it is in.
    #[test]
    fn a_run_find_stops_inside_the_first_stretch_of_the_search() {
        // Not a multiple of the stride, so the step that finishes the needle
        // is also the step that starts the search: the budget is one span of
        // turns whichever phase it is spent in, which is the property that
        // makes the bound hold at the phase change too.
        const M: usize = SAFEPOINT_STRIDE as usize * 5 + 5;
        const N: usize = SAFEPOINT_STRIDE as usize * 20;
        let (program, entry) = find_fixture();
        let (haystack, needle) = straight_scan(M, N);
        let fuel = M as u64 + SAFEPOINT_STRIDE / 8;
        let (charged, spent, outcome) = under_fuel(&program, entry, &haystack, &needle, 0, fuel);
        assert_eq!(outcome, crate::trace::RunOutcome::Fuel);
        assert!(
            charged >= M as u64,
            "{charged} unit(s) charged, so the needle was not finished"
        );
        assert!(
            charged <= M as u64 + SAFEPOINT_STRIDE,
            "{charged} unit(s) charged, which is past the first stretch of the search"
        );
        assert!(
            spent <= fuel + 2 * SAFEPOINT_STRIDE,
            "a fuel limit of {fuel} spent {spent}"
        );
    }

    /// **A search is charged an upper bound on the work it did, and the bound
    /// is `5m + 2(n - from)`.**
    ///
    /// This asked for `m + (n - from)` **exactly** until the matcher under it
    /// was rewritten, and the equality is the reason it had to be: only a
    /// matcher whose phases have counters running `0..m` and `from..n` can
    /// charge a count, which meant Knuth–Morris–Pratt, which meant a table of
    /// `m` entries, which meant a `Vec` that reallocates and copies itself
    /// whole in the middle of the phase whose whole purpose is that no step
    /// exceeds a stride — and invisibly, because a Rust-side allocation is
    /// what `--boundary`'s `allocs` column does not count (#442).
    ///
    /// Fuel is an upper bound on work done and not an equality. The intrinsic
    /// this instruction replaces charged "the receiver's whole length as an
    /// upper bound" and said so. So the charge is the comparisons the matcher
    /// made, and `crate::find` derives what those are bounded by from the
    /// algorithm rather than from this measurement. Checked here at four
    /// starts, and again on every pair of that module's corpus.
    ///
    /// The charge is still **proportional and non-trivial**, which the second
    /// assertion is for: a bound a matcher meets by charging one would be a
    /// bound that says nothing.
    #[test]
    fn a_whole_scan_is_charged_within_its_bound() {
        const M: usize = SAFEPOINT_STRIDE as usize * 2 + 5;
        const N: usize = SAFEPOINT_STRIDE as usize * 7 + 11;
        let (program, entry) = find_fixture();
        let (haystack, needle) = straight_scan(M, N);
        for from in [0usize, 1, 3_000, N - M] {
            let mut machine = Machine::new(&program, 1 << 20);
            let hay = run_of(&mut machine, &haystack);
            let sought = run_of(&mut machine, &needle);
            let answer = machine
                .run(entry, &[hay, sought, from as u64], &budget())
                .expect("a search answers");
            assert_eq!(answer[0] as i64, -1, "the two runs agree nowhere");
            let bound = crate::find::Matcher::bound(N, M, from);
            assert!(
                machine.bulk_work <= bound,
                "from {from}: {} charged, past the bound of {bound}",
                machine.bulk_work
            );
            assert!(
                machine.bulk_work >= (N - from) as u64 / 2,
                "from {from}: {} charged for a scan of {} unit(s), which is not \
                 proportional to anything",
                machine.bulk_work,
                N - from
            );
        }
    }

    /// **A match that straddles the point a poll fell at is still found.**
    ///
    /// The matcher carries its position and its partial match across the
    /// safepoint, so a needle lying across a step boundary is one the search
    /// finds without going back. Every offset in a window around the first
    /// two boundaries is tried, because which offset *is* the boundary is an
    /// arithmetic a reader should not have to reproduce to trust the case.
    #[test]
    fn a_match_across_a_poll_is_still_found() {
        let (program, entry) = find_fixture();
        let needle = b"needle!!".to_vec();
        let n = SAFEPOINT_STRIDE as usize * 4;
        for boundary in [SAFEPOINT_STRIDE as usize, SAFEPOINT_STRIDE as usize * 2] {
            for at in (boundary - needle.len() - 2)..(boundary + 2) {
                let mut haystack = vec![b'.'; n];
                haystack[at..at + needle.len()].copy_from_slice(&needle);
                assert_eq!(
                    found(&program, entry, &haystack, &needle, 0),
                    at as i64,
                    "a match at {at}, around the boundary at {boundary}"
                );
            }
        }
    }

    /// **A cancellation stops a search the way a fuel bound does**, at the
    /// next poll rather than at the end of the haystack.
    #[test]
    fn a_cancelled_run_find_stops_at_a_poll() {
        const M: usize = SAFEPOINT_STRIDE as usize * 2;
        const N: usize = SAFEPOINT_STRIDE as usize * 400;
        let (program, entry) = find_fixture();
        let (haystack, needle) = straight_scan(M, N);
        let stop = crate::budget::Cancellation::new();
        stop.cancel();
        let budget = crate::budget::Budget::with_cancellation(
            crate::budget::Limits::default(),
            stop.clone(),
        );
        let mut machine = Machine::new(&program, 1 << 22);
        let hay = run_of(&mut machine, &haystack);
        let sought = run_of(&mut machine, &needle);
        let error = machine
            .run(entry, &[hay, sought, 0], &budget.meter())
            .expect_err("a cancelled search is stopped");
        assert_eq!(error.outcome, crate::trace::RunOutcome::Cancelled);
        assert!(
            machine.bulk_work <= SAFEPOINT_STRIDE,
            "{} unit(s) charged before the first poll looked",
            machine.bulk_work
        );
    }

    /// **A deadline stops one too**, and at the same place: the poll between
    /// two steps is where all three bounds are asked.
    #[test]
    fn a_run_find_past_its_deadline_stops_at_a_poll() {
        const M: usize = SAFEPOINT_STRIDE as usize * 2;
        const N: usize = SAFEPOINT_STRIDE as usize * 400;
        let (program, entry) = find_fixture();
        let (haystack, needle) = straight_scan(M, N);
        let budget = crate::budget::Budget::new(crate::budget::Limits {
            deadline: Some(std::time::Duration::ZERO),
            ..crate::budget::Limits::default()
        });
        let mut machine = Machine::new(&program, 1 << 22);
        let hay = run_of(&mut machine, &haystack);
        let sought = run_of(&mut machine, &needle);
        let error = machine
            .run(entry, &[hay, sought, 0], &budget.meter())
            .expect_err("a search past its deadline is stopped");
        assert_eq!(error.outcome, crate::trace::RunOutcome::Deadline);
        assert!(
            machine.bulk_work <= SAFEPOINT_STRIDE,
            "{} unit(s) charged before the first poll looked",
            machine.bulk_work
        );
    }

    /// **The two fast paths answer while charging no bulk work at all.**
    ///
    /// On a haystack large enough that examining a hundredth of it would show
    /// in the charge, which is what makes the zero a statement rather than a
    /// rounding: an empty needle answers `from` and an over-long needle
    /// answers -1, both before any preparation begins. The instruction's own
    /// single unit of fuel is unchanged, so a fast path is one fuel and
    /// nothing else.
    #[test]
    fn the_two_fast_paths_charge_no_bulk_work() {
        const N: usize = SAFEPOINT_STRIDE as usize * 40;
        let (program, entry) = find_fixture();
        let haystack = vec![b'B'; N];
        let cases: [(&[u8], i64, i64); 4] = [
            (&[], 0, 0),
            (&[], 17, 17),
            (&[], N as i64, N as i64),
            (&[b'A'; 3], N as i64 - 2, -1),
        ];
        for (needle, from, want) in cases {
            let mut machine = Machine::new(&program, 1 << 20);
            let hay = run_of(&mut machine, &haystack);
            let sought = run_of(&mut machine, needle);
            let answer = machine
                .run(entry, &[hay, sought, from as u64], &budget())
                .expect("a fast path answers");
            assert_eq!(answer[0] as i64, want, "needle {needle:?} from {from}");
            assert_eq!(
                machine.bulk_work, 0,
                "needle {needle:?} from {from}: a fast path examines no unit"
            );
        }
    }

    /// A null run stops the run, on either side.
    #[test]
    fn run_find_bytes_refuses_a_null_run() {
        let (program, entry) = find_fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let text = run_of(&mut machine, b"haystack");
        for args in [[0, text, 0], [text, 0, 0]] {
            let error = machine.run(entry, &args, &budget()).unwrap_err();
            assert_eq!(error.message, null_object().message);
        }
    }

    /// A `from` outside `0 ..= haystack_len` stops the run: it is a broken
    /// invariant of the lowering and never a program's mistake, which is the
    /// rule a run slice's range is held to as well.
    #[test]
    fn run_find_bytes_refuses_a_start_outside_the_haystack() {
        let (program, entry) = find_fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let text = run_of(&mut machine, b"haystack");
        let needle = run_of(&mut machine, b"stack");
        for from in [-1i64, 9, 1 << 40] {
            let error = machine
                .run(entry, &[text, needle, from as u64], &budget())
                .unwrap_err();
            assert!(
                error.message.contains("runFind") && error.message.contains("starts at"),
                "from {from}: {}",
                error.message
            );
        }
        // And the one that is in range at its very edge answers rather than
        // refusing.
        assert_eq!(
            machine
                .run(entry, &[text, needle, 8], &budget())
                .expect("`from == haystack_len` is legal")[0] as i64,
            -1
        );
    }

    /// A run under construction is not a haystack and not a needle: both
    /// operands are fixed runs, for the reason a run slice's source is.
    #[test]
    fn run_find_bytes_refuses_a_run_under_construction() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let text = build.string_layout();
        let bytes = build.bytes_layout();
        let row = build.args(&[(3, int), (0, text), (1, text), (2, int)]);
        let entry = build.function(
            "find",
            &[bytes, bytes, int],
            &[Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::RunFind {
                    args: row,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 16);
        let store = machine.allocate(program.bytes_layout, 8).unwrap();
        let string = run_of(&mut machine, b"haystack");
        for (args, role) in [
            ([store, string, 0], "haystack"),
            ([string, store, 0], "needle"),
        ] {
            let error = machine.run(entry, &args, &budget()).unwrap_err();
            assert!(
                error.message.contains(role) && error.message.contains("`String`"),
                "{}",
                error.message
            );
        }
    }

    /// **The two runs may alias**, which is stated in the instruction because
    /// a run copy's rules are not this one's: a search only reads, so there is
    /// no order in which a write could be seen.
    #[test]
    fn run_find_bytes_admits_the_same_run_on_both_sides() {
        let (program, entry) = find_fixture();
        let mut machine = Machine::new(&program, 1 << 16);
        let text = run_of(&mut machine, b"abcabc");
        assert_eq!(
            machine
                .run(entry, &[text, text, 0], &budget())
                .expect("a run may be its own needle")[0] as i64,
            0
        );
        assert_eq!(
            machine
                .run(entry, &[text, text, 1], &budget())
                .expect("a run may be its own needle")[0] as i64,
            -1
        );
    }

    /// **The search allocates nothing, at every needle length, observed by
    /// the allocator rather than argued from a counter.**
    ///
    /// This replaces a case that asserted the matcher gave its scratch buffers
    /// back, and the replacement is the point: there are no buffers. The
    /// matcher before this one held a Knuth–Morris–Pratt table of `m` entries
    /// out of `Machine::scratch`, appended one entry at a time, and a `Vec`
    /// appended to reallocates and copies its whole contents when its capacity
    /// runs out — an unbounded `memcpy` inside a phase whose whole purpose is
    /// that no step exceeds a stride. **No counter in this repository could
    /// see it**: `--boundary`'s `allocs` and `words` count Cove's heap, and a
    /// Rust-side allocation is exactly the blind spot
    /// [#442](https://github.com/myuon/cove/issues/442) was about. So the zero
    /// is observed here by `crate::find::counting`, a global allocator the
    /// test binary installs, rather than inferred from columns that cannot
    /// see it.
    ///
    /// The lengths are chosen to be the capacity boundaries the old design
    /// would have crossed — a table of `4m` bytes doubling through 1 KiB and
    /// 4 KiB, and `SCRATCH_BYTES` itself — plus one far past every cap. With
    /// no buffer there is nothing to cross, which is what the case says.
    ///
    /// `find_in_runs` is called directly rather than through `Machine::run`
    /// because a run answers a `Vec` of words and so allocates once for
    /// reasons that have nothing to do with the search.
    #[test]
    fn a_run_find_allocates_nothing_at_any_needle_length() {
        let (program, entry) = find_fixture();
        let budget = budget();
        for m in [
            1usize, 7, 255, 256, 1_023, 1_024, 1_025, 4_096, 4_097, 40_000,
        ] {
            let mut machine = Machine::new(&program, 1 << 22);
            let needle = vec![b'A'; m];
            let mut haystack = vec![b'B'; m * 3 + 17];
            // At the very end, so the search walks the whole haystack and
            // every phase of the matcher runs.
            let at = haystack.len() - m;
            haystack[at..].copy_from_slice(&needle);
            let n = haystack.len();
            let hay = run_of(&mut machine, &haystack);
            let sought = run_of(&mut machine, &needle);

            let (answer, allocations) = crate::find::counting::while_counting(|| {
                find_in_runs(&mut machine, &budget, hay, sought, n, m, 0, entry, 0)
            });
            assert_eq!(
                answer.expect("a search answers"),
                at as i64,
                "a needle of {m} unit(s) at {at}"
            );
            assert_eq!(
                allocations, 0,
                "a needle of {m} unit(s) allocated {allocations} time(s)"
            );
            assert!(
                machine.bulk_work <= crate::find::Matcher::bound(n, m, 0),
                "a needle of {m} unit(s) charged {}",
                machine.bulk_work
            );
        }
    }

    /// And nothing is left in the scratch pool either, because nothing was
    /// taken from it: the search is not one of its callers any more.
    #[test]
    fn a_run_find_takes_no_scratch_buffer() {
        let (program, entry) = find_fixture();
        let mut machine = Machine::new(&program, 1 << 18);
        let haystack = run_of(&mut machine, b"a haystack with a needle in it");
        let needle = run_of(&mut machine, b"needle");

        let primed = 1024;
        for _ in 0..2 {
            machine.give_scratch(Vec::with_capacity(primed));
        }
        assert_eq!(machine.scratch_retained(), 2 * primed);
        for _ in 0..3 {
            assert_eq!(
                machine
                    .run(entry, &[haystack, needle, 0], &budget())
                    .expect("a search answers")[0] as i64,
                18
            );
            assert_eq!(
                machine.scratch_retained(),
                2 * primed,
                "the pool is exactly as the search found it"
            );
        }
        assert!(machine.scratch_retained() <= SCRATCH_BUFFERS * SCRATCH_BYTES);
    }

    // --- ADR 0052: bulk work is bounded work -------------------------------

    /// A byte run of `capacity` bytes in `dst`: a buffer allocated there and
    /// replaced by its own store.
    ///
    /// The store is a `Shape::Bytes` run whose header length is the capacity
    /// (above the growable floor), which is all a copy needs of either end;
    /// the owner is garbage from the instruction after it is allocated, and
    /// nothing reads its length.
    fn byte_run(dst: Slot, capacity: Slot, run: LayoutId) -> [Inst; 2] {
        [
            Inst::GrowableAlloc {
                dst,
                capacity,
                storage: Storage::PackedBytes,
            },
            Inst::LoadField {
                dst,
                obj: dst,
                at: 1,
                layout: run,
            },
        ]
    }

    /// A run that copies `bytes` bytes in one byte `run-copy`, with a fixture
    /// whose only other instructions are the two allocations and a return.
    fn one_big_copy(bytes: i64) -> (cove_ir::Program, cove_ir::FunctionId) {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let run = build.bytes_layout();
        build.buffer_layout();
        let copy = build.args(&[(1, run), (3, int), (2, run), (3, int), (0, int)]);
        let mut code = byte_run(1, 0, run).to_vec();
        code.extend(byte_run(2, 0, run));
        code.extend([
            Inst::Int { dst: 3, value: 0 },
            Inst::RunCopy {
                args: copy,
                storage: Storage::PackedBytes,
            },
            Inst::Return { src: 1 },
        ]);
        let entry = build.function(
            "copier",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
            run,
            code,
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
    /// The collection lands between two byte `run-copy`s rather than inside one,
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
        build.string_layout();
        let run = build.bytes_layout();
        let owner = build.buffer_layout();
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
            run,
            [
                vec![Inst::Int {
                    dst: 0,
                    value: BYTES,
                }],
                byte_run(1, 0, run).to_vec(),
                byte_run(2, 0, run).to_vec(),
                vec![
                    Inst::Int { dst: 3, value: 0 },
                    Inst::Int {
                        dst: 4,
                        value: BYTES / 2,
                    },
                    // The first half, so the run is half written from here on.
                    Inst::RunCopy {
                        args: first,
                        storage: Storage::PackedBytes,
                    },
                    // Garbage, cleared between allocations so the previous one
                    // is unreachable when the next is asked for. The heap holds
                    // the two runs and one spare, so every allocation after the
                    // first has to reclaim before it fits.
                    Inst::GrowableAlloc {
                        dst: 5,
                        capacity: 0,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: owner,
                    },
                    Inst::GrowableAlloc {
                        dst: 5,
                        capacity: 0,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: owner,
                    },
                    Inst::GrowableAlloc {
                        dst: 5,
                        capacity: 0,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: owner,
                    },
                    // And the second half, into a run a collection has now walked.
                    Inst::RunCopy {
                        args: second,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Return { src: 1 },
                ],
            ]
            .concat(),
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
            .expect("the run answers its run");
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
    /// `Inst::RunCopy` admits a `Shape::Bytes` source, so `src` and `dst` may
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
            vec![
                Inst::RunCopy {
                    args,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 0 },
            ],
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

    // --- ADR 0058: a run copy of whole elements -----------------------------

    /// `copy(dst, dst_at, src, src_at, count) -> dst` over `Words(elem)`, for
    /// an `elements` layout of `elem`: the word-run half of the byte fixture.
    fn word_copier(build: &mut Build, elements: LayoutId, elem: LayoutId) -> FunctionId {
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(0, elements), (1, int), (2, elements), (3, int), (4, int)]);
        build.function(
            "copy_words",
            &[elements, int, elements, int, int],
            &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            elements,
            vec![
                Inst::RunCopy {
                    args,
                    storage: Storage::Words(elem),
                },
                Inst::Return { src: 0 },
            ],
        )
    }

    /// **A word copy moves whole elements, at the element's stride, and bounds
    /// in elements.**
    ///
    /// A two-word `Point` is the case a copy that counted in words would get
    /// wrong in both directions at once: the offsets would land mid-element
    /// and the count would move half the elements. Every case is checked
    /// against Rust's own slice copy of the same words, the zero-length copies
    /// included, and the destination's words outside the range are held to
    /// what they were.
    #[test]
    fn a_word_copy_moves_whole_elements_at_the_stride() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array<Point>",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let entry = word_copier(&mut build, points, point);
        let program = build.done();

        const SRC: u32 = 10;
        const DST: u32 = 8;
        let source: Vec<u64> = (0..u64::from(SRC) * 2).map(|n| 100 + n).collect();
        for (dst_at, src_at, count) in [
            (0u32, 0u32, 5u32),
            (3, 0, 5),
            (0, 2, 8),
            (7, 9, 1),
            (1, 1, 6),
            (0, 0, 0),
            (8, 10, 0),
        ] {
            let mut machine = Machine::new(&program, 1 << 16);
            let src = machine.allocate(points, i64::from(SRC)).unwrap();
            machine.set_payload_run(src, 0, &source);
            let dst = machine.allocate(points, i64::from(DST)).unwrap();
            let filler: Vec<u64> = (0..u64::from(DST) * 2).map(|n| 900 + n).collect();
            machine.set_payload_run(dst, 0, &filler);
            let answer = machine
                .run(
                    entry,
                    &[
                        dst,
                        u64::from(dst_at),
                        src,
                        u64::from(src_at),
                        u64::from(count),
                    ],
                    &budget(),
                )
                .expect("a copy within its bounds answers")[0];
            assert_eq!(answer, dst);
            let mut want = filler.clone();
            want[(dst_at * 2) as usize..((dst_at + count) * 2) as usize]
                .copy_from_slice(&source[(src_at * 2) as usize..((src_at + count) * 2) as usize]);
            assert_eq!(
                machine.payload_run(dst, 0, DST * 2),
                want,
                "dst_at={dst_at} src_at={src_at} count={count}"
            );
            // Two elements' worth of work, not one word's and not one unit's.
            assert!(machine.work() >= u64::from(count) * 2);
        }
    }

    /// **Every refusal is made in elements, before anything is written.**
    ///
    /// Out of range by one *element* is refused even where it is in range by
    /// words, which is what a check against the payload's width would have
    /// missed; a run of another element family is refused at either end,
    /// because the collector traces each object by its own layout.
    #[test]
    fn a_word_copy_refuses_what_is_not_a_run_of_its_elements() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array<Point>",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let ints = build.layout(
            "Array<Int>",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let entry = word_copier(&mut build, points, point);
        let program = build.done();

        let refused = |dst_len: i64, src_len: i64, family: LayoutId, args: [i64; 3]| {
            let mut machine = Machine::new(&program, 1 << 16);
            let src = machine.allocate(points, src_len).unwrap();
            machine.set_payload_run(src, 0, &vec![7; (src_len * 2) as usize]);
            let dst = machine.allocate(family, dst_len).unwrap();
            let error = machine
                .run(
                    entry,
                    &[dst, args[0] as u64, src, args[1] as u64, args[2] as u64],
                    &budget(),
                )
                .expect_err("the copy is refused");
            let untouched = machine.payload_run(dst, 0, machine.object_len(dst));
            assert!(
                untouched.iter().all(|word| *word == 0),
                "nothing was written"
            );
            error.message
        };

        let past_the_source = refused(8, 4, points, [0, 1, 4]);
        assert!(
            past_the_source.contains("reads 4 element(s) from 1 of a source of 4"),
            "{past_the_source}"
        );
        let past_the_destination = refused(4, 8, points, [1, 0, 4]);
        assert!(
            past_the_destination.contains("writes 4 element(s) to 1 of a destination of 4"),
            "{past_the_destination}"
        );
        let negative = refused(4, 4, points, [0, 0, -1]);
        assert!(negative.contains("negative length"), "{negative}");
        // Eight words is room for eight `Int`s and four `Point`s: the family
        // is refused before its length is looked at.
        let wrong_family = refused(8, 4, ints, [0, 0, 1]);
        assert!(
            wrong_family.contains("destination is not a run of `Point` elements"),
            "{wrong_family}"
        );

        let mut machine = Machine::new(&program, 1 << 16);
        let dst = machine.allocate(points, 4).unwrap();
        let text = machine.new_string("not elements").unwrap();
        let error = machine
            .run(entry, &[dst, 0, text, 0, 1], &budget())
            .unwrap_err();
        assert!(
            error
                .message
                .contains("source is not a run of `Point` elements"),
            "{}",
            error.message
        );
        let error = machine
            .run(entry, &[dst, 0, 0, 0, 0], &budget())
            .unwrap_err();
        assert_eq!(error.message, null_object().message);
    }

    /// **An overlapping word copy answers the source as it was, across chunk
    /// boundaries, and a chunk is a whole number of elements.**
    ///
    /// A three-word element makes a chunk `BULK_CHUNK_WORDS / 3` elements,
    /// which does not divide the stride evenly — so a loop that chunked in
    /// words would split an element across a poll, and a loop that chunked
    /// front-first would overwrite its own input. Both are what `remove`
    /// shifting a vector's tail down, and an insertion shifting it up, will
    /// ask of this.
    #[test]
    fn an_overlapping_word_copy_moves_elements_as_memmove_does() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let triple = build.structure("Triple", &[("a", int), ("b", int), ("c", int)]);
        let store = build.layout(
            "Store<Triple>",
            Shape::Elements {
                elem: triple,
                growable: true,
            },
        );
        let args = build.args(&[(0, store), (1, int), (0, store), (2, int), (3, int)]);
        let entry = build.function(
            "shift",
            &[store, int, int, int],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            store,
            vec![
                Inst::RunCopy {
                    args,
                    storage: Storage::Words(triple),
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();

        let chunk = (BULK_CHUNK_WORDS / 3) as i64;
        let elements = 3 * chunk + 5;
        let pattern: Vec<u64> = (0..elements as u64 * 3).collect();
        for (dst_at, src_at, count) in [
            (chunk + 1, 0i64, 2 * chunk),
            (0, chunk + 1, 2 * chunk),
            (1, 0, elements - 1),
            (0, 1, elements - 1),
            (chunk, chunk - 1, chunk + 1),
        ] {
            let mut machine = Machine::new(&program, 1 << 16);
            let obj = machine.allocate(store, elements).expect("a store fits");
            machine.set_payload_run(obj, 0, &pattern);
            machine
                .run(
                    entry,
                    &[obj, dst_at as u64, src_at as u64, count as u64],
                    &budget(),
                )
                .expect("a copy within its bounds answers");
            let mut want = pattern.clone();
            want.copy_within(
                (src_at * 3) as usize..((src_at + count) * 3) as usize,
                (dst_at * 3) as usize,
            );
            assert_eq!(
                machine.payload_run(obj, 0, elements as u32 * 3),
                want,
                "copy_within({src_at}..{}, {dst_at}) over {elements} elements",
                src_at + count
            );
        }
    }

    /// **A word copy is charged for the words it moves and overspends its fuel
    /// by less than one chunk plus one stride.**
    ///
    /// `a_bulk_copy_overspends_its_fuel_by_less_than_one_chunk` for the other
    /// storage. The two share one chunk loop, and this is what holds the word
    /// arm to it: a copy that charged per *element*, or that made one chunk of
    /// the whole range, passes every agreement test above and fails here.
    #[test]
    fn a_word_copy_overspends_its_fuel_by_less_than_one_chunk() {
        const ELEMENTS: i64 = 1 << 17;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let ints = build.layout(
            "Array<Int>",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let copy = build.args(&[(1, ints), (3, int), (2, ints), (3, int), (0, int)]);
        let entry = build.function(
            "copier",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
            ints,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: ints,
                    len: Len::Slot(0),
                },
                Inst::Alloc {
                    dst: 2,
                    layout: ints,
                    len: Len::Slot(0),
                },
                Inst::Int { dst: 3, value: 0 },
                Inst::RunCopy {
                    args: copy,
                    storage: Storage::Words(int),
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let words = ELEMENTS as u64;
        for limit in [1_024u64, 8_192, 40_000] {
            let budget = crate::budget::Budget::new(crate::budget::Limits {
                fuel: Some(limit),
                ..crate::budget::Limits::default()
            });
            let mut machine = Machine::new(&program, 1 << 20);
            let error = machine
                .run(entry, &[ELEMENTS as u64], &budget.meter())
                .expect_err("a copy past its fuel is stopped");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);
            let bound = limit + BULK_CHUNK_WORDS + SAFEPOINT_STRIDE;
            let spent = budget.fuel_spent();
            assert!(
                spent <= bound && spent < words,
                "a {ELEMENTS}-element copy under a fuel limit of {limit} spent {spent}, \
                 past the bound of {bound}; the whole copy would have been {words}"
            );
        }
    }

    /// **References copied into a run are traced from it: a collection with a
    /// half-copied run of `String`s live keeps every string copied so far, and
    /// one after the source is dropped keeps the rest.**
    ///
    /// This is the property a word copy has that a byte copy never had — its
    /// units may be addresses — and so the one a barrier would exist for. The
    /// collector needs none (it is non-moving and marks from the roots), so
    /// what this holds is the thing it does need: the destination is walkable
    /// part-way through, and once the source is gone the destination alone
    /// keeps what was copied into it.
    ///
    /// The heap is sized so the garbage *must* collect, and the assertion on
    /// `collections` is what makes the test mean anything.
    #[test]
    fn references_copied_into_a_run_survive_a_collection() {
        const COUNT: i64 = 10;
        const GARBAGE: u32 = 200;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let text = build.string_layout();
        let strings = build.layout(
            "Array<String>",
            Shape::Elements {
                elem: text,
                growable: false,
            },
        );
        let ints = build.layout(
            "Array<Int>",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        // s0 is the source, s1 the destination, s2 the half, s3 zero, s4 the
        // count, s5 the garbage.
        let first = build.args(&[(1, strings), (3, int), (0, strings), (3, int), (2, int)]);
        let second = build.args(&[(1, strings), (2, int), (0, strings), (2, int), (2, int)]);
        let garbage = |code: &mut Vec<Inst>| {
            for _ in 0..3 {
                code.push(Inst::Alloc {
                    dst: 5,
                    layout: ints,
                    len: Len::Count(GARBAGE),
                });
                code.push(Inst::Clear {
                    slot: 5,
                    layout: ints,
                });
            }
        };
        let mut code = vec![
            Inst::Int {
                dst: 4,
                value: COUNT,
            },
            Inst::Alloc {
                dst: 1,
                layout: strings,
                len: Len::Slot(4),
            },
            Inst::Int {
                dst: 2,
                value: COUNT / 2,
            },
            Inst::Int { dst: 3, value: 0 },
            Inst::RunCopy {
                args: first,
                storage: Storage::Words(text),
            },
        ];
        garbage(&mut code);
        code.push(Inst::RunCopy {
            args: second,
            storage: Storage::Words(text),
        });
        code.push(Inst::Clear {
            slot: 0,
            layout: strings,
        });
        garbage(&mut code);
        code.push(Inst::Return { src: 1 });
        let entry = build.function(
            "copy_strings",
            &[strings],
            &[
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
            ],
            strings,
            code,
        );
        let program = build.done();

        let mut machine = Machine::new(&program, 600);
        let src = machine.allocate(strings, COUNT).unwrap();
        for at in 0..COUNT {
            let word = machine.new_string(&format!("string {at}")).unwrap();
            machine.set_payload(src, at as u32, word);
        }
        let before = machine.collected().collections;
        let dst = machine
            .run(entry, &[src], &budget())
            .expect("the run answers the destination")[0];
        let after = machine.collected().collections;
        assert!(
            after >= before + 2,
            "this fixture exists to collect with a half-copied run live and again \
             after the source is dropped, and it collected {} time(s)",
            after - before
        );
        for at in 0..COUNT {
            let word = machine.payload(dst, at as u32);
            assert_eq!(
                machine.object_layout(word),
                text,
                "element {at} is a string"
            );
            assert_eq!(
                machine.string_bytes(word),
                format!("string {at}").into_bytes(),
                "element {at} kept its text"
            );
        }
    }

    // --- ADR 0058: the run slice --------------------------------------------

    /// `slice_words(src, from, count) -> target`: one word `run-slice` of `elem`
    /// out of a run of `source`, answering a fresh `target`.
    fn word_slicer(
        build: &mut Build,
        source: LayoutId,
        target: LayoutId,
        elem: LayoutId,
    ) -> FunctionId {
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(3, target), (0, source), (1, int), (2, int)]);
        build.function(
            "slice_words",
            &[source, int, int],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
            target,
            vec![
                Inst::RunSlice {
                    args,
                    storage: Storage::Words(elem),
                },
                Inst::Return { src: 3 },
            ],
        )
    }

    /// **A word slice answers a fresh array of whole elements, from an array or
    /// from a vector's store, charged for the words it moves.**
    ///
    /// Two-word `Point`s, so an offset counted in words would land mid-element;
    /// every case is Rust's own slice of the same words, the empty ones included,
    /// and the source is held to what it was.
    #[test]
    fn a_word_slice_answers_a_fresh_array_of_whole_elements() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array<Point>",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let store = build.layout(
            "Store<Point>",
            Shape::Elements {
                elem: point,
                growable: true,
            },
        );
        let from_array = word_slicer(&mut build, points, points, point);
        let from_store = word_slicer(&mut build, store, points, point);
        let program = build.done();

        const SRC: u32 = 10;
        let source: Vec<u64> = (0..u64::from(SRC) * 2).map(|n| 100 + n).collect();
        for (family, entry) in [(points, from_array), (store, from_store)] {
            for (from, count) in [(0u32, 10u32), (3, 5), (9, 1), (0, 0), (10, 0)] {
                let mut machine = Machine::new(&program, 1 << 16);
                let src = machine.allocate(family, i64::from(SRC)).unwrap();
                machine.set_payload_run(src, 0, &source);
                let answer = machine
                    .run(entry, &[src, u64::from(from), u64::from(count)], &budget())
                    .expect("a slice within its bounds answers")[0];
                assert_ne!(answer, src, "a fresh run, not the source");
                assert_eq!(machine.object_layout(answer), points);
                assert_eq!(machine.object_len(answer), count);
                assert_eq!(
                    machine.payload_run(answer, 0, count * 2),
                    source[(from * 2) as usize..((from + count) * 2) as usize].to_vec(),
                    "from={from} count={count}"
                );
                assert_eq!(machine.payload_run(src, 0, SRC * 2), source);
                assert!(machine.work() >= u64::from(count) * 2);
            }
        }
    }

    /// **Every refusal is made before anything is allocated, and the answer's
    /// slot is left as it was.**
    ///
    /// None is reachable from a checked program — the standard library clamps
    /// into the length first — so each is a broken invariant, reported in
    /// `runCopy`'s sentences with this instruction's name.
    #[test]
    fn a_word_slice_refuses_a_range_outside_its_source() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array<Point>",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let ints = build.layout(
            "Array<Int>",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let entry = word_slicer(&mut build, points, points, point);
        let program = build.done();
        for (what, family, from, count, message) in [
            (
                "one element past the source",
                Some(points),
                1i64,
                4i64,
                "`runSlice` reads 4 element(s) from 1 of a source of 4",
            ),
            (
                "a negative offset",
                Some(points),
                -1,
                1,
                "`runSlice` reads 1 element(s) from -1 of a source of 4",
            ),
            (
                "a negative count",
                Some(points),
                0,
                -1,
                "`runSlice`'s count is `-1`, and a copy cannot have a negative length",
            ),
            (
                "another family",
                Some(ints),
                0,
                1,
                "`runSlice`'s source is not a run of `Point` elements",
            ),
            ("a null source", None, 0, 1, null_object().message.as_str()),
        ] {
            let mut machine = Machine::new(&program, 1 << 16);
            let src = match family {
                Some(family) => machine.allocate(family, 4).unwrap(),
                None => 0,
            };
            let before = machine.allocations();
            let error = machine
                .run(entry, &[src, from as u64, count as u64], &budget())
                .expect_err(what);
            assert_eq!(error.message, message, "{what}");
            assert!(error.span.is_some(), "{what}: at the instruction");
            assert_eq!(machine.allocations(), before, "{what}: nothing allocated");
        }
    }

    /// **The source of a slice survives the collection its own allocation
    /// makes, and so do the references it copies.**
    ///
    /// The heap is full of garbage when the slice allocates, so the allocation
    /// collects with the source named only by the frame; every string in the
    /// answer must still be the string it was.
    #[test]
    fn a_word_slice_holds_its_source_across_the_collection_it_makes() {
        const COUNT: i64 = 10;
        let mut build = Build::default();
        let text = build.string_layout();
        let strings = build.layout(
            "Array<String>",
            Shape::Elements {
                elem: text,
                growable: false,
            },
        );
        let entry = word_slicer(&mut build, strings, strings, text);
        let program = build.done();

        let mut machine = Machine::new(&program, 600);
        let src = machine.allocate(strings, COUNT).unwrap();
        machine.push_temp(src);
        for at in 0..COUNT {
            let word = machine.new_string(&format!("string {at}")).unwrap();
            machine.set_payload(src, at as u32, word);
        }
        while machine.heap_words() + 4 <= 600 {
            machine.new_string("dead").unwrap();
        }
        machine.release_temps(0);
        let before = machine.collected().collections;
        let answer = machine
            .run(entry, &[src, 2, (COUNT - 2) as u64], &budget())
            .expect("the slice answers")[0];
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        for at in 0..COUNT - 2 {
            let word = machine.payload(answer, at as u32);
            assert_eq!(
                machine.string_bytes(word),
                format!("string {}", at + 2).into_bytes(),
                "element {at} kept its text"
            );
        }
    }

    // --- #378 P4-5: a sorted run as a word source, and a keyed finish -------

    /// The layouts a keyed update over `String`s moves words between: a
    /// `Set<String>` and a `Map<String, String>`, the vectors and stores a
    /// standard-library body builds their next run in, the arrays a slice of
    /// one answers, and an `Array<Int>` for garbage.
    struct KeyedLayouts {
        int: LayoutId,
        text: LayoutId,
        entry: LayoutId,
        set: LayoutId,
        map: LayoutId,
        texts: LayoutId,
        entries: LayoutId,
        text_store: LayoutId,
        entry_store: LayoutId,
        text_vector: LayoutId,
        entry_vector: LayoutId,
        ints: LayoutId,
    }

    fn keyed_layouts(build: &mut Build) -> KeyedLayouts {
        let int = build.scalar(Repr::Int);
        let text = build.string_layout();
        let entry = build.structure("MapEntry", &[("key", text), ("value", text)]);
        let elements = |elem, growable| Shape::Elements { elem, growable };
        KeyedLayouts {
            int,
            text,
            entry,
            set: build.layout("Set<String>", Shape::Members { elem: text }),
            map: build.layout(
                "Map<String, String>",
                Shape::Entries {
                    key: text,
                    value: text,
                },
            ),
            texts: build.layout("Array<String>", elements(text, false)),
            entries: build.layout("Array<MapEntry>", elements(entry, false)),
            text_store: build.layout("store<String>", elements(text, true)),
            entry_store: build.layout("store<MapEntry>", elements(entry, true)),
            text_vector: build.layout("Vector<String>", Shape::Vector { elem: text }),
            entry_vector: build.layout("Vector<MapEntry>", Shape::Vector { elem: entry }),
            ints: build.layout("Array<Int>", elements(int, false)),
        }
    }

    /// `finish(owner) -> target`: one keyed word `run-finish` of `elem` into
    /// `target`, then the owner dropped and five allocations of garbage that
    /// the fixture's heap cannot hold without collecting.
    fn keyed_finisher(
        build: &mut Build,
        layouts: &KeyedLayouts,
        vector: LayoutId,
        target: LayoutId,
        elem: LayoutId,
    ) -> FunctionId {
        let mut code = vec![
            Inst::RunFinish {
                dst: 1,
                owner: 0,
                target,
                validation: Validation::None,
                storage: Storage::Words(elem),
            },
            Inst::Clear {
                slot: 0,
                layout: vector,
            },
        ];
        for _ in 0..5 {
            code.push(Inst::Alloc {
                dst: 2,
                layout: layouts.ints,
                len: Len::Count(200),
            });
            code.push(Inst::Clear {
                slot: 2,
                layout: layouts.ints,
            });
        }
        code.push(Inst::Return { src: 1 });
        build.function(
            "keyed_finish",
            &[vector],
            &[Repr::Ref, Repr::Ref, Repr::Ref],
            target,
            code,
        )
    }

    /// A vector of `stride`-word elements holding `words` and room for
    /// `spare` more, allocated the way `core.vectorWithCapacity` and the
    /// commits after it leave one.
    fn filled_vector(
        machine: &mut Machine<'_>,
        vector: LayoutId,
        store: LayoutId,
        stride: u32,
        words: &[u64],
        spare: u32,
    ) -> u64 {
        let len = words.len() as u32 / stride;
        let held = machine.allocate(store, i64::from(len + spare)).unwrap();
        machine.set_payload_run(held, 0, words);
        let mark = machine.temps();
        machine.push_temp(held);
        let owner = machine.allocate(vector, 0).unwrap();
        machine.release_temps(mark);
        machine.set_payload(owner, 0, u64::from(len));
        machine.set_payload(owner, 1, held);
        owner
    }

    /// **A keyed finish relabels a growable store into a `Set` or a `Map` in
    /// place, gives the spare room back, and the references it holds are
    /// traced by the target's own map across the collections that follow.**
    ///
    /// The owner is dropped before the garbage, so after the finish the answer
    /// alone holds every string — keys and, for the map, values.
    #[test]
    fn a_keyed_finish_keeps_what_its_run_holds_alive_across_a_collection() {
        const COUNT: u32 = 10;
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        let to_set = keyed_finisher(
            &mut build,
            &layouts,
            layouts.text_vector,
            layouts.set,
            layouts.text,
        );
        let to_map = keyed_finisher(
            &mut build,
            &layouts,
            layouts.entry_vector,
            layouts.map,
            layouts.entry,
        );
        let program = build.done();

        for (entry, target, stride) in [(to_set, layouts.set, 1u32), (to_map, layouts.map, 2)] {
            let mut machine = Machine::new(&program, 700);
            let mut words = Vec::new();
            let mark = machine.temps();
            for at in 0..COUNT * stride {
                let word = machine.new_string(&format!("text {at:02}")).unwrap();
                machine.push_temp(word);
                words.push(word);
            }
            let (vector, store) = match stride {
                1 => (layouts.text_vector, layouts.text_store),
                _ => (layouts.entry_vector, layouts.entry_store),
            };
            let owner = filled_vector(&mut machine, vector, store, stride, &words, 3);
            machine.release_temps(mark);
            let before = machine.collected().collections;
            let answer = machine
                .run(entry, &[owner], &budget())
                .expect("a sorted run finishes")[0];
            assert!(
                machine.collected().collections > before,
                "the fixture did not force a collection"
            );
            assert_eq!(machine.object_layout(answer), target);
            assert_eq!(machine.object_len(answer), COUNT);
            for at in 0..COUNT * stride {
                let word = machine.payload(answer, at);
                assert_eq!(
                    machine.string_bytes(word),
                    format!("text {at:02}").into_bytes(),
                    "word {at} of the keyed run kept its text"
                );
            }
        }
    }

    /// **A word allocation holds its store across the owner's allocation, so a
    /// collection between the two does not free it.**
    ///
    /// `Machine::alloc_vector` allocates the store first and holds it with
    /// `Machine::push_temp` for exactly the window in which the owner is
    /// allocated, which is `Machine::alloc_buffer`'s discipline and its
    /// argument: for that window the store is reachable from a Rust local and
    /// nothing else, and a Rust local is not a root.
    ///
    /// Making the collection land *between* the two is the whole of the
    /// fixture, and it is built rather than hoped for. The heap is given a dead
    /// block exactly the size of an owner, then a live wall, then the tail
    /// everything else is allocated out of — so freeing the dead block does not
    /// lengthen the tail. A capacity is then searched for at which the store
    /// fits the tail and the owner's three words do not: the store's allocation
    /// cannot be the one that collects, because a collection there would free
    /// an owner-sized hole and the store would still not fit, and the call
    /// would fail rather than answer. So a call that both answers *and*
    /// collected collected at the owner.
    #[test]
    fn a_word_allocation_holds_its_store_across_a_collection() {
        const HEAP: usize = 512;
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        let program = build.done();
        let found = (1..HEAP as i64).find_map(|capacity| {
            let mut machine = Machine::new(&program, HEAP);
            // A hole exactly an owner wide, dead the moment it is made.
            machine
                .allocate(layouts.text_vector, 0)
                .expect("the hole fits");
            // And a live wall after it, so that the hole cannot become part of
            // the tail the store is taken from.
            let wall = machine.allocate(layouts.ints, 4).expect("the wall fits");
            machine.push_temp(wall);
            let owner = machine.alloc_vector(layouts.text, capacity).ok()?;
            (machine.collected().collections == 1).then_some((machine, owner, capacity))
        });
        let (machine, owner, capacity) =
            found.expect("some capacity leaves the owner as the allocation that collects");
        assert_eq!(machine.object_layout(owner), layouts.text_vector);
        assert_eq!(machine.payload(owner, 0), 0, "a fresh vector is empty");
        let store = machine.payload(owner, 1);
        assert_ne!(store, 0, "the store survived the collection at the owner");
        assert_eq!(
            machine.object_layout(store),
            layouts.text_store,
            "and it is still the store, not a block the sweep folded back in"
        );
        assert_eq!(
            u64::from(machine.object_len(store)),
            capacity as u64,
            "at the capacity asked for"
        );
    }

    /// **A reference written into a freshly allocated store is traced through
    /// it**, from the allocation onwards and with no finish in between.
    ///
    /// The store's own header carries `Shape::Elements { elem, growable: true }`
    /// and `Machine::trace` reads the element's reference map out of it, so the
    /// words a vector holds are followed the moment the store exists — there is
    /// no window in which a `Vector<String>` built through this path is a run
    /// of integers to the collector. The string here is reachable through the
    /// owner's store and through nothing else when the collection happens.
    #[test]
    fn a_reference_in_a_freshly_allocated_store_is_traced() {
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        let program = build.done();
        let mut machine = Machine::new(&program, 700);
        // Room for the one element, asked for: a word allocation takes the
        // capacity it is given and does not floor it, so a capacity of nought
        // here would be a store with no payload word to write into.
        let owner = machine
            .alloc_vector(layouts.text, 1)
            .expect("a one-element vector fits");
        machine.push_temp(owner);
        let text = machine.new_string("held only by the store").unwrap();
        let store = machine.payload(owner, 1);
        machine.set_payload(store, 0, text);
        machine.set_payload(owner, 0, 1);
        // Garbage enough to collect. Nothing roots `text` but the store.
        for _ in 0..20 {
            let _ = machine.allocate(layouts.ints, 60);
        }
        assert!(
            machine.collected().collections > 0,
            "the fixture did not force a collection"
        );
        let store = machine.payload(owner, 1);
        assert_eq!(
            machine.string_bytes(machine.payload(store, 0)),
            b"held only by the store".to_vec()
        );
    }

    /// **A word allocation takes the capacity it is asked for, and refuses a
    /// negative one** — which is *not* `Machine::alloc_buffer`'s rule, and the
    /// difference is the two floors' own contract.
    ///
    /// [`MIN_GROWABLE_BYTES`] is "the smallest byte store a buffer is allocated
    /// with, and the floor a growth doubles up from";
    /// [`MIN_GROWABLE_ELEMENTS`](super::super::runs::MIN_GROWABLE_ELEMENTS)
    /// is "the floor of the first growth rather than of the first allocation",
    /// because a `Vector` built from known elements is allocated to exactly
    /// those. So nought gives a store of nothing here, and a vector that is
    /// grown gets the floor from `Growable::floor` when it is grown.
    ///
    /// The number that settled it: flooring at the allocation cost cq's 196,677
    /// constructions 666,740 allocated words, 6.94% of the run's, and avoided
    /// no growth — `core.vectorWithCapacity`'s callers in `std.map` and
    /// `std.set` size the vector to what they are about to push into it.
    ///
    /// A negative capacity is not a small number, it is the one arithmetic a
    /// caller could not have meant, and clamping it would turn it into a silent
    /// success.
    #[test]
    fn a_word_allocation_takes_the_capacity_asked_and_refuses_a_negative_one() {
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        for (asked, held) in [(0, 0), (1, 1), (9, 9)] {
            let owner = machine
                .alloc_vector(layouts.text, asked)
                .expect("a small vector fits");
            machine.push_temp(owner);
            assert_eq!(
                u64::from(machine.object_len(machine.payload(owner, 1))),
                held,
                "a capacity of {asked}"
            );
        }
        assert!(machine.alloc_vector(layouts.text, -1).is_err());
        // And an element this program declares no vector of is refused by the
        // table rather than guessed at.
        let refused = machine
            .alloc_vector(layouts.int, 4)
            .expect_err("no `Vector<Int>` is declared here");
        assert!(
            refused.message.contains("declares no vector"),
            "{}",
            refused.message
        );
    }

    /// **In this crate's tests a keyed finish of a run that is not ascending
    /// and distinct is a broken invariant of the body that built it** (#378,
    /// Q4.10): a finish does not sort, and a set that renders out of order is
    /// worse than a stopped run.
    #[test]
    #[should_panic(expected = "not ascending and distinct")]
    fn a_keyed_finish_of_an_unsorted_run_is_a_broken_invariant() {
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        let entry = keyed_finisher(
            &mut build,
            &layouts,
            layouts.text_vector,
            layouts.set,
            layouts.text,
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let b = machine.new_string("b").unwrap();
        let a = machine.new_string("a").unwrap();
        let owner = filled_vector(
            &mut machine,
            layouts.text_vector,
            layouts.text_store,
            1,
            &[b, a],
            0,
        );
        let _ = machine.run(entry, &[owner], &budget());
    }

    /// **A `Set` and a `Map` are word sources for a run copy and a run slice,
    /// at their own stride, and never a run copy's destination.**
    #[test]
    fn a_sorted_run_is_read_as_units_and_never_written() {
        let mut build = Build::default();
        let layouts = keyed_layouts(&mut build);
        // copy(dst, src): two units of `String` from 0 to 0.
        let row = build.args(&[
            (0, layouts.texts),
            (2, layouts.int),
            (1, layouts.set),
            (2, layouts.int),
            (3, layouts.int),
        ]);
        let copy = build.function(
            "copy_members",
            &[layouts.texts, layouts.set],
            &[Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            layouts.texts,
            vec![
                Inst::Int { dst: 2, value: 0 },
                Inst::Int { dst: 3, value: 2 },
                Inst::RunCopy {
                    args: row,
                    storage: Storage::Words(layouts.text),
                },
                Inst::Return { src: 0 },
            ],
        );
        let members = word_slicer(&mut build, layouts.set, layouts.texts, layouts.text);
        let pairs = word_slicer(&mut build, layouts.map, layouts.entries, layouts.entry);
        let program = build.done();

        let mut machine = Machine::new(&program, 1 << 14);
        let (a, b, c, d) = (
            machine.new_string("a").unwrap(),
            machine.new_string("b").unwrap(),
            machine.new_string("c").unwrap(),
            machine.new_string("d").unwrap(),
        );
        let set = machine.allocate(layouts.set, 2).unwrap();
        machine.set_payload_run(set, 0, &[a, b]);
        let map = machine.allocate(layouts.map, 2).unwrap();
        machine.set_payload_run(map, 0, &[a, c, b, d]);

        let array = machine.allocate(layouts.texts, 2).unwrap();
        machine
            .run(copy, &[array, set], &budget())
            .expect("a set is a word source");
        assert_eq!(machine.payload_run(array, 0, 2), vec![a, b]);

        let other = machine.allocate(layouts.set, 2).unwrap();
        let error = machine
            .run(copy, &[other, set], &budget())
            .expect_err("a set is not a destination");
        assert_eq!(
            error.message,
            "`runCopy`'s destination is not a run of `String` elements"
        );
        assert_eq!(
            machine.payload_run(other, 0, 2),
            vec![0, 0],
            "nothing written"
        );

        let sliced = machine
            .run(members, &[set, 1, 1], &budget())
            .expect("a set slices")[0];
        assert_eq!(machine.object_layout(sliced), layouts.texts);
        assert_eq!(machine.payload_run(sliced, 0, 1), vec![b]);
        let sliced = machine
            .run(pairs, &[map, 0, 2], &budget())
            .expect("a map slices as its entries")[0];
        assert_eq!(machine.object_layout(sliced), layouts.entries);
        assert_eq!(machine.object_len(sliced), 2);
        assert_eq!(machine.payload_run(sliced, 0, 4), vec![a, c, b, d]);

        // A map is not a run of its keys: a unit of the wrong width is refused.
        let wrong = machine.run(members, &[map, 0, 1], &budget()).unwrap_err();
        assert_eq!(
            wrong.message,
            "`runSlice`'s source is not a run of `String` elements"
        );
    }

    /// `slice_bytes(src, from, count) -> String`: one byte `run-slice` out of a
    /// string, answering a fresh `String`.
    fn byte_slicer(build: &mut Build) -> FunctionId {
        let text = build.string_layout();
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(3, text), (0, text), (1, int), (2, int)]);
        build.function(
            "slice_bytes",
            &[text, int, int],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
            text,
            vec![
                Inst::RunSlice {
                    args,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 3 },
            ],
        )
    }

    /// **A byte slice answers the same range a byte-at-a-time reading does, at
    /// every alignment, as a fresh string charged for the words it moves.**
    ///
    /// The cases that matter are the middles: a source offset that is not a
    /// multiple of eight makes each answer word two payload reads shifted against
    /// each other, and an off-by-one there answers a string of the right *length*
    /// and the wrong bytes. So every range of a string of several words is cut,
    /// and each is compared with the bytes themselves — and word for word with the
    /// same text written directly, because `eq.str` compares payload words and a
    /// cut that left anything in its last word's tail would be unequal to it.
    #[test]
    fn a_byte_slice_answers_every_range_at_every_alignment() {
        let mut build = Build::default();
        let entry = byte_slicer(&mut build);
        let program = build.done();
        // Deliberately not a multiple of eight, so the last word is partial.
        let source: String = (0..29u8).map(|n| (b'a' + n % 26) as char).collect();
        let bytes = source.as_bytes();
        for from in 0..=bytes.len() {
            for to in from..=bytes.len() {
                let mut machine = Machine::new(&program, 1 << 16);
                let src = machine.new_string(&source).unwrap();
                let before = machine.allocations();
                let answer = machine
                    .run(entry, &[src, from as u64, (to - from) as u64], &budget())
                    .expect("a slice within its string answers")[0];
                assert_ne!(answer, src, "a fresh string, not the source");
                assert_eq!(machine.allocations(), before + 1, "one allocation");
                assert_eq!(machine.object_layout(answer), program.str_layout);
                assert_eq!(machine.object_len(answer) as usize, to - from);
                assert_eq!(machine.string_bytes(answer), &bytes[from..to]);
                let direct = machine.new_string(&source[from..to]).unwrap();
                for word in 0..((to - from) as u32).div_ceil(8) {
                    assert_eq!(
                        machine.payload(answer, word),
                        machine.payload(direct, word),
                        "{from}..{to} payload word {word}, padding included"
                    );
                }
                assert!(machine.work() >= (to - from).div_ceil(8) as u64);
            }
        }
    }

    /// **Every refusal is made before anything is allocated.**
    ///
    /// None is reachable from a checked program — `std.string.sliceBytes` holds
    /// the range inside the string first — so each is a broken invariant,
    /// reported in `runSlice`'s sentences with a byte for a unit. A cut inside a
    /// character is *not* among them: that is the standard library's question,
    /// and this instruction copies the bytes it is told to.
    #[test]
    fn a_byte_slice_refuses_a_range_outside_its_string() {
        let mut build = Build::default();
        let entry = byte_slicer(&mut build);
        let ints = build.layout(
            "Array<Int>",
            Shape::Elements {
                elem: LayoutId(0),
                growable: false,
            },
        );
        let program = build.done();
        for (what, source, from, count, message) in [
            (
                "one byte past the string",
                Some(false),
                1i64,
                4i64,
                "`runSlice` reads 4 byte(s) from 1 of a source of 4",
            ),
            (
                "a negative offset",
                Some(false),
                -1,
                1,
                "`runSlice` reads 1 byte(s) from -1 of a source of 4",
            ),
            (
                "a negative count",
                Some(false),
                0,
                -1,
                "`runSlice`'s count is `-1`, and a copy cannot have a negative length",
            ),
            (
                "a source that is not a string",
                Some(true),
                0,
                1,
                "`runSlice`'s source is not a `String`",
            ),
            ("a null source", None, 0, 1, null_object().message.as_str()),
        ] {
            let mut machine = Machine::new(&program, 1 << 16);
            let src = match source {
                Some(false) => machine.new_string("abcd").unwrap(),
                Some(true) => machine.allocate(ints, 4).unwrap(),
                None => 0,
            };
            let before = machine.allocations();
            let error = machine
                .run(entry, &[src, from as u64, count as u64], &budget())
                .expect_err(what);
            assert_eq!(error.message, message, "{what}");
            assert!(error.span.is_some(), "{what}: at the instruction");
            assert_eq!(machine.allocations(), before, "{what}: nothing allocated");
        }
        // Inside a character, which is the body's refusal and not this one's.
        let mut machine = Machine::new(&program, 1 << 16);
        let src = machine.new_string("a\u{e9}").unwrap();
        let answer = machine
            .run(entry, &[src, 0, 2], &budget())
            .expect("the bytes it is told to copy")[0];
        assert_eq!(machine.string_bytes(answer), vec![b'a', 0xC3]);
    }

    /// **The source of a byte slice survives the collection its own allocation
    /// makes, and so does a long answer across its chunks' polls.**
    #[test]
    fn a_byte_slice_holds_its_source_across_the_collection_it_makes() {
        let mut build = Build::default();
        let entry = byte_slicer(&mut build);
        let program = build.done();
        let text: String = (0..4000u32)
            .map(|n| (b'a' + (n % 26) as u8) as char)
            .collect();

        let mut machine = Machine::new(&program, 1400);
        let src = machine.new_string(&text).unwrap();
        machine.push_temp(src);
        while machine.heap_words() + 4 <= 1400 {
            machine.new_string("dead").unwrap();
        }
        machine.release_temps(0);
        let before = machine.collected().collections;
        let answer = machine
            .run(entry, &[src, 7, 3000], &budget())
            .expect("the slice answers")[0];
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        assert_eq!(machine.string_bytes(answer), &text.as_bytes()[7..3007]);
        assert_eq!(machine.string_bytes(src), text.as_bytes());
    }

    // --- ADR 0052: the byte-buffer instructions ----------------------------

    /// A program with every function ADR 0052's tests share, so each test
    /// builds a fixture and none builds a compiler.
    ///
    /// - `alloc(capacity) -> Ref` answers a new owner.
    /// - `append_byte(owner, value) -> Ref` answers the same owner, so a
    ///   caller can keep appending to what it got back and watch the address
    ///   not move.
    /// - `append_bytes(owner, src, from, to) -> Ref` likewise.
    /// - `finish(owner) -> Ref` answers the `String`.
    ///
    /// The two appends are [ADR 0062] windows — the length, an ensure, the
    /// store, a write and a commit — which is what `std.stringbuilder` lowers
    /// to since the composite `growable-push` and `growable-extend` were
    /// deleted. What they check is the *buffer*: its growth, its address, the
    /// room a finish gives back and the bytes that survive a collection. Which
    /// dispatch path runs the rows is `mod window`'s question, not theirs.
    ///
    /// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
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
                Inst::GrowableAlloc {
                    dst: 1,
                    capacity: 0,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 1 },
            ],
        );
        // s2 the length, s3 the count, s4 the store.
        let append_byte = build.function(
            "append_byte",
            &[owner, int],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int, Repr::Ref],
            owner,
            vec![
                Inst::LoadField {
                    dst: 2,
                    obj: 0,
                    at: cove_ir::legalize::LENGTH,
                    layout: int,
                },
                Inst::Int { dst: 3, value: 1 },
                Inst::GrowableEnsure {
                    owner: 0,
                    additional: 3,
                    storage: Storage::PackedBytes,
                },
                Inst::LoadField {
                    dst: 4,
                    obj: 0,
                    at: cove_ir::legalize::STORE,
                    layout: bytes,
                },
                Inst::RunStore {
                    run: 4,
                    index: 2,
                    src: 1,
                    storage: Storage::PackedBytes,
                },
                Inst::GrowableCommit {
                    owner: 0,
                    count: 3,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 0 },
            ],
        );
        // `to - from` is computed *before* the head, because the reservation
        // rule admits no arithmetic inside a window. s4 the count, s5 the
        // length, s6 the store.
        let args = build.args(&[(6, bytes), (5, int), (1, bytes), (2, int), (4, int)]);
        let append_bytes = build.function(
            "append_bytes",
            &[owner, bytes, int, int],
            &[
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
            ],
            owner,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Sub,
                    dst: 4,
                    a: 3,
                    b: 2,
                },
                Inst::LoadField {
                    dst: 5,
                    obj: 0,
                    at: cove_ir::legalize::LENGTH,
                    layout: int,
                },
                Inst::GrowableEnsure {
                    owner: 0,
                    additional: 4,
                    storage: Storage::PackedBytes,
                },
                Inst::LoadField {
                    dst: 6,
                    obj: 0,
                    at: cove_ir::legalize::STORE,
                    layout: bytes,
                },
                Inst::RunCopy {
                    args,
                    storage: Storage::PackedBytes,
                },
                Inst::GrowableCommit {
                    owner: 0,
                    count: 4,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 0 },
            ],
        );
        let finish = build.function(
            "finish",
            &[owner],
            &[Repr::Ref],
            str_layout,
            vec![
                Inst::RunFinish {
                    dst: 0,
                    owner: 0,
                    target: str_layout,
                    validation: Validation::Utf8,
                    storage: Storage::PackedBytes,
                },
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
    /// `spare` is the whole reason `run-finish` passes anything to
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
        assert_eq!(u64::from(machine.object_len(store)), MIN_GROWABLE_BYTES);
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
        // finish of an empty buffer relabels a store of `MIN_GROWABLE_BYTES` down
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
    /// `MIN_GROWABLE_BYTES`, and 200 appends double it five times, so the store
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
            "200 appends from a floor of {MIN_GROWABLE_BYTES} should have grown \
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

    /// **`growable-extend` agrees with Rust's own slicing, from a `String` and
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

    // The character-boundary rule and the source-range refusal an append made
    // were the composite `growable-extend`'s, and both left with it. ADR 0062
    // moved the range policy into `std.stringbuilder`'s `appendRange` — five
    // questions in Cove, in `String.sliceBytes`' own words, raising through
    // `Intrinsic::StringRefuseByteRange` — precisely so that what is left under
    // it is a `run-copy` that validates nothing and can therefore be the write
    // half of a window. What that copy refuses about a range, in its own words,
    // is `run_range`'s "`runCopy` reads n byte(s) from x of a source of y".

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
            // The refusal is the window's write, `run-store`, in its words.
            assert!(
                error.message.contains("runStore") && error.message.contains("0 to 255"),
                "{value}: {}",
                error.message
            );
        }
    }

    /// A run that appends `BYTES` bytes in one window's `run-copy` onto the
    /// owner it is handed, with a fixture whose only other instructions are the
    /// source's allocation and a return.
    ///
    /// The owner is a parameter rather than an allocation of the run's own so
    /// that a test can read it after the run has been stopped.
    fn one_big_append() -> (Program, FunctionId) {
        const BYTES: i64 = 1 << 20;
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        build.string_layout();
        let run = build.bytes_layout();
        let owner = build.buffer_layout();
        // s0: the owner; s1: the source run; s2: its length and the window's
        // count; s3: zero; s4 and s5 the window's length and store.
        let args = build.args(&[(5, run), (4, int), (1, run), (3, int), (2, int)]);
        let mut code = vec![Inst::Int {
            dst: 2,
            value: BYTES,
        }];
        code.extend(byte_run(1, 2, run));
        code.extend([
            Inst::Int { dst: 3, value: 0 },
            // ADR 0062's byte append window. The copy is a megabyte, so it is
            // chunked and never fused, and the rows run one at a time.
            Inst::LoadField {
                dst: 4,
                obj: 0,
                at: cove_ir::legalize::LENGTH,
                layout: int,
            },
            Inst::GrowableEnsure {
                owner: 0,
                additional: 2,
                storage: Storage::PackedBytes,
            },
            Inst::LoadField {
                dst: 5,
                obj: 0,
                at: cove_ir::legalize::STORE,
                layout: run,
            },
            Inst::RunCopy {
                args,
                storage: Storage::PackedBytes,
            },
            Inst::GrowableCommit {
                owner: 0,
                count: 2,
                storage: Storage::PackedBytes,
            },
            Inst::Return { src: 0 },
        ]);
        let entry = build.function(
            "appender",
            &[owner],
            &[
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
            ],
            owner,
            code,
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
            let owner = machine.alloc_buffer(0).expect("an empty buffer fits");
            let error = machine
                .run(entry, &[owner], &budget.meter())
                .expect_err("an append past its fuel is stopped");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);

            // The ensure happened and the commit did not: the store was grown
            // for the whole range up front, and the length word still says
            // nothing was appended, so the bytes a stopped copy did move are
            // spare room rather than value.
            assert_eq!(
                machine.payload(owner, runs::GROWABLE_LEN),
                0,
                "a {BYTES}-byte append stopped at a fuel limit of {limit} committed nothing"
            );
            let store = machine.payload(owner, runs::GROWABLE_STORE);
            assert!(
                u64::from(machine.object_len(store)) >= BYTES,
                "and its store was grown for the whole range before the first chunk"
            );

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

    /// [`cove_native::RunCopyFn`], differentially: each fixture above with a caller
    /// in front of it, run once on the dispatch loop alone and once with the
    /// template compiler's table installed.
    ///
    /// Here rather than in `tests/native_tier.rs` because no Cove source reaches a
    /// byte copy, an overlapping copy or a refusal — `Vector.toArray` copies a
    /// whole live prefix into a fresh array, and that file has it — so the
    /// programs are written in the IR, as every other `RunCopy` fixture in this
    /// module is.
    ///
    /// The caller is what makes it a tier question: the outermost frame of a run
    /// is always the dispatch loop's, so the copy has to be reached through a
    /// `call`, and the `call` is what consults the table. Every case asserts that
    /// the callee was compiled and that the call crossed into it, because a case
    /// that did not would compare the VM with itself.
    #[cfg(feature = "template")]
    mod compiled {
        use super::*;
        use crate::native::NativeProgram;
        use crate::vm::exec::native::{Tiered, Tiers};

        /// `outer(params) = inner(params)`, whose one `call` is the crossing.
        ///
        /// Every parameter here is one word, which is what lets the frame be the
        /// parameters' own `Repr`s and one slot more for the answer.
        pub(super) fn through_a_call(
            build: &mut Build,
            inner: FunctionId,
            answer: Repr,
        ) -> FunctionId {
            let held = build.program.function(inner);
            let params = held.params.clone();
            let mut frame = held.reprs[..params.len()].to_vec();
            let returns = held.returns;
            let row: Vec<(Slot, LayoutId)> = params
                .iter()
                .enumerate()
                .map(|(at, layout)| (at as Slot, *layout))
                .collect();
            let args = build.args(&row);
            let dst = frame.len() as Slot;
            frame.push(answer);
            build.function(
                "outer",
                &params,
                &frame,
                returns,
                vec![
                    Inst::Call {
                        dst,
                        callee: inner,
                        args,
                    },
                    Inst::Return { src: dst },
                ],
            )
        }

        /// What a run said: its words, or its error's sentence, span and outcome.
        pub(super) type Said = Result<Vec<u64>, (String, Option<Span>, crate::trace::RunOutcome)>;

        /// What a case places before a run, and what it reads back after one.
        pub(super) type Prepare<'p> = &'p dyn Fn(&mut Machine<'_>) -> Vec<u64>;
        pub(super) type Inspect<'i, T> = &'i dyn Fn(&Machine<'_>, &[u64]) -> T;

        /// One run of `entry` on a fresh machine of `heap` words, on the dispatch
        /// loop alone or with `native` installed: `prepare` places the arguments,
        /// and `inspect` reads back whatever the case needs after the run.
        fn on<T>(
            program: &Program,
            native: Option<&NativeProgram>,
            heap: usize,
            budget: &Meter,
            entry: FunctionId,
            prepare: Prepare<'_>,
            inspect: Inspect<'_, T>,
        ) -> (Said, T, Tiers, u64) {
            let mut machine = Machine::new(program, heap);
            if let Some(native) = native {
                // Safety: `native` is borrowed for longer than this machine lives.
                unsafe { machine.install_native(native) };
            }
            let args = prepare(&mut machine);
            let said = machine
                .run(entry, &args, budget)
                .map_err(|error| (error.message, error.span, error.outcome));
            let seen = inspect(&machine, &args);
            let tiers = machine.tiers();
            (said, seen, tiers, machine.work())
        }

        /// [`on`] on both tiers, asserting they said and left the same thing and
        /// that the native run really crossed.
        pub(super) fn agree<T: PartialEq + std::fmt::Debug>(
            what: &str,
            program: &Program,
            native: &NativeProgram,
            entry: FunctionId,
            prepare: Prepare<'_>,
            inspect: Inspect<'_, T>,
        ) -> (Said, T) {
            let (vm, vm_seen, _, _) =
                on(program, None, 1 << 16, &budget(), entry, prepare, inspect);
            let (native_said, seen, tiers, _) = on(
                program,
                Some(native),
                1 << 16,
                &budget(),
                entry,
                prepare,
                inspect,
            );
            assert_eq!(native_said, vm, "what the run said: {what}");
            assert_eq!(seen, vm_seen, "what it left: {what}");
            assert!(
                tiers.vm_to_native >= 1,
                "the copy ran in compiled code: {what}: {tiers:?}"
            );
            (vm, vm_seen)
        }

        /// Compiles `program` and asserts `inner` is one of what compiled.
        pub(super) fn compiled(program: &Program, inner: FunctionId) -> NativeProgram {
            let native = crate::native::compile(program).expect("this host compiles");
            assert!(
                native.entry(inner).is_some(),
                "the copying function is compiled: {:?}",
                native.refusals()
            );
            native
        }

        /// `fixture()`'s byte copier, with a caller in front of it.
        fn byte_copier() -> (Program, FunctionId, FunctionId) {
            let Fixture { program, copy_into } = fixture();
            let mut build = Build { program };
            let entry = through_a_call(&mut build, copy_into, Repr::Ref);
            (build.done(), copy_into, entry)
        }

        /// **A byte copy from compiled code agrees with the VM at every alignment,
        /// from a `String` and from a byte run, and as memmove over one run in both
        /// directions across chunk edges.**
        #[test]
        fn a_compiled_byte_copy_agrees_with_the_vm() {
            let (program, copy_into, entry) = byte_copier();
            let native = compiled(&program, copy_into);
            let text = "abcdefghijklmnopqrstuvwxyz";
            for from_string in [true, false] {
                for (dst_at, src_at, len) in [
                    (0u64, 0u64, 5u64),
                    (3, 0, 5),
                    (0, 7, 9),
                    (8, 10, 6),
                    (1, 1, 10),
                    (0, 0, 0),
                    (5, 5, 0),
                ] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let src = if from_string {
                            machine.new_string(text).unwrap()
                        } else {
                            let run = machine
                                .allocate(machine.program.bytes_layout, text.len() as i64)
                                .unwrap();
                            machine.write_bytes(run, text.as_bytes());
                            run
                        };
                        let dst = machine.allocate(machine.program.bytes_layout, 16).unwrap();
                        vec![dst, dst_at, src, src_at, len]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| machine.string_bytes(args[0]);
                    let (said, bytes) = agree(
                        &format!("from a string: {from_string}, {dst_at} {src_at} {len}"),
                        &program,
                        &native,
                        entry,
                        &prepare,
                        &inspect,
                    );
                    assert!(said.is_ok());
                    let mut want = vec![0u8; 16];
                    want[dst_at as usize..(dst_at + len) as usize].copy_from_slice(
                        &text.as_bytes()[src_at as usize..(src_at + len) as usize],
                    );
                    assert_eq!(bytes, want);
                }
            }

            // memmove over one run, each overlap straddling a chunk edge.
            const BYTES: i64 = 3 * BULK_CHUNK_BYTES;
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            build.string_layout();
            let run = build.bytes_layout();
            let args = build.args(&[(0, run), (1, int), (0, run), (2, int), (3, int)]);
            let shift = build.function(
                "shift",
                &[run, int, int, int],
                &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
                run,
                vec![
                    Inst::RunCopy {
                        args,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let entry = through_a_call(&mut build, shift, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, shift);
            let pattern: Vec<u8> = (0..BYTES).map(|n| (n % 251) as u8).collect();
            for (dst_at, src_at, len) in [
                (BULK_CHUNK_BYTES + 3, 0i64, 2 * BULK_CHUNK_BYTES - 3),
                (0, BULK_CHUNK_BYTES + 3, 2 * BULK_CHUNK_BYTES - 3),
                (9, 1, 2 * BULK_CHUNK_BYTES),
                (1, 9, 2 * BULK_CHUNK_BYTES),
            ] {
                let prepare = |machine: &mut Machine<'_>| {
                    let obj = machine
                        .allocate(machine.program.bytes_layout, BYTES)
                        .unwrap();
                    machine.write_bytes(obj, &pattern);
                    vec![obj, dst_at as u64, src_at as u64, len as u64]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| machine.string_bytes(args[0]);
                let (_, bytes) = agree(
                    &format!("copy_within({src_at}..+{len}, {dst_at})"),
                    &program,
                    &native,
                    entry,
                    &prepare,
                    &inspect,
                );
                let mut want = pattern.clone();
                want.copy_within(src_at as usize..(src_at + len) as usize, dst_at as usize);
                assert_eq!(bytes, want, "and it is memmove's answer");
            }
        }

        /// **A word copy from compiled code moves whole elements at the stride,
        /// between two stores and as memmove within one, across chunk edges.**
        #[test]
        fn a_compiled_word_copy_agrees_with_the_vm() {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let triple = build.structure("Triple", &[("a", int), ("b", int), ("c", int)]);
            let store = build.layout(
                "Store<Triple>",
                Shape::Elements {
                    elem: triple,
                    growable: true,
                },
            );
            let copier = word_copier(&mut build, store, triple);
            let entry = through_a_call(&mut build, copier, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, copier);

            let chunk = BULK_CHUNK_WORDS / 3;
            let elements = 3 * chunk + 5;
            let pattern: Vec<u64> = (0..elements * 3).collect();
            for same in [false, true] {
                for (dst_at, src_at, count) in [
                    (chunk + 1, 0u64, 2 * chunk),
                    (0, chunk + 1, 2 * chunk),
                    (1, 0, elements - 1),
                    (0, 1, elements - 1),
                    (7, 9, 1),
                    (0, 0, 0),
                ] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let src = machine.allocate(store, elements as i64).unwrap();
                        machine.set_payload_run(src, 0, &pattern);
                        let dst = match same {
                            true => src,
                            false => machine.allocate(store, elements as i64).unwrap(),
                        };
                        vec![dst, dst_at, src, src_at, count]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        machine.payload_run(args[0], 0, elements as u32 * 3)
                    };
                    let (said, words) = agree(
                        &format!("one store: {same}, {dst_at} {src_at} {count}"),
                        &program,
                        &native,
                        entry,
                        &prepare,
                        &inspect,
                    );
                    assert!(said.is_ok());
                    let mut want = match same {
                        true => pattern.clone(),
                        false => vec![0; pattern.len()],
                    };
                    let from = &pattern[(src_at * 3) as usize..((src_at + count) * 3) as usize];
                    want.splice(
                        (dst_at * 3) as usize..((dst_at + count) * 3) as usize,
                        from.iter().copied(),
                    );
                    assert_eq!(words, want);
                }
            }
        }

        /// **A byte slice from compiled code answers the VM's fresh string across
        /// chunk edges, and refuses what the VM refuses, in its words.**
        #[test]
        fn a_compiled_byte_slice_agrees_with_the_vm() {
            let mut build = Build::default();
            let slicer = byte_slicer(&mut build);
            let entry = through_a_call(&mut build, slicer, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, slicer);

            let chunk = BULK_CHUNK_BYTES as u64;
            let len = 3 * chunk + 5;
            let text: String = (0..len).map(|n| (b'a' + (n % 26) as u8) as char).collect();
            for (from, count) in [
                (0u64, len),
                (chunk + 1, 2 * chunk),
                (1, len - 1),
                (7, 1),
                (0, 0),
                (len, 0),
                // Refused: one past the string, and a negative count.
                (1, len),
                (0, u64::MAX),
            ] {
                let prepare = |machine: &mut Machine<'_>| {
                    let src = machine.new_string(&text).unwrap();
                    vec![src, from, count]
                };
                let inspect = |machine: &Machine<'_>, _: &[u64]| machine.allocations();
                let (said, _) = agree(
                    &format!("{from} {count}"),
                    &program,
                    &native,
                    entry,
                    &prepare,
                    &inspect,
                );
                let in_range =
                    (count as i64) >= 0 && from.checked_add(count).is_some_and(|end| end <= len);
                match said {
                    Ok(_) => assert!(in_range, "{from} {count} answered"),
                    Err((message, ..)) => {
                        assert!(!in_range, "{from} {count}: {message}");
                        assert!(message.contains("runSlice"), "{message}");
                    }
                }
            }
            // What the answer holds, read on each tier.
            for tier in [None, Some(&native)] {
                let mut machine = Machine::new(&program, 1 << 16);
                if let Some(native) = tier {
                    // Safety: `native` outlives this machine.
                    unsafe { machine.install_native(native) };
                }
                let src = machine.new_string(&text).unwrap();
                let answer = machine
                    .run(entry, &[src, chunk + 1, 2 * chunk], &budget())
                    .expect("answers")[0];
                assert_eq!(machine.object_layout(answer), program.str_layout);
                assert_eq!(
                    machine.string_bytes(answer),
                    &text.as_bytes()[(chunk + 1) as usize..(3 * chunk + 1) as usize],
                    "native: {}",
                    tier.is_some()
                );
            }
        }

        /// **A word slice from compiled code answers the VM's fresh array, from an
        /// array and from a store, across chunk edges — and refuses what the VM
        /// refuses, in its words.**
        #[test]
        fn a_compiled_word_slice_agrees_with_the_vm() {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let triple = build.structure("Triple", &[("a", int), ("b", int), ("c", int)]);
            let array = build.layout(
                "Array<Triple>",
                Shape::Elements {
                    elem: triple,
                    growable: false,
                },
            );
            let store = build.layout(
                "Store<Triple>",
                Shape::Elements {
                    elem: triple,
                    growable: true,
                },
            );
            let slicer = word_slicer(&mut build, store, array, triple);
            let entry = through_a_call(&mut build, slicer, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, slicer);

            let chunk = BULK_CHUNK_WORDS / 3;
            let elements = 3 * chunk + 5;
            let pattern: Vec<u64> = (0..elements * 3).collect();
            for family in [array, store] {
                for (from, count) in [
                    (0u64, elements),
                    (chunk + 1, 2 * chunk),
                    (1, elements - 1),
                    (7, 1),
                    (0, 0),
                    (elements, 0),
                    // Refused: one past the source, and a negative count.
                    (1, elements),
                    (0, u64::MAX),
                ] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let src = machine.allocate(family, elements as i64).unwrap();
                        machine.set_payload_run(src, 0, &pattern);
                        vec![src, from, count]
                    };
                    let inspect = |machine: &Machine<'_>, _: &[u64]| machine.allocations();
                    let (said, _) = agree(
                        &format!("{family:?}: {from} {count}"),
                        &program,
                        &native,
                        entry,
                        &prepare,
                        &inspect,
                    );
                    let in_range = (count as i64) >= 0
                        && from.checked_add(count).is_some_and(|end| end <= elements);
                    match said {
                        Ok(_) => assert!(in_range, "{from} {count} answered"),
                        Err((message, ..)) => {
                            assert!(!in_range, "{from} {count}: {message}");
                            assert!(message.contains("runSlice"), "{message}");
                        }
                    }
                }
            }
            // What the answer holds, read on each tier.
            let prepare = |machine: &mut Machine<'_>| {
                let src = machine.allocate(store, elements as i64).unwrap();
                machine.set_payload_run(src, 0, &pattern);
                vec![src, chunk + 1, 2 * chunk]
            };
            for tier in [None, Some(&native)] {
                let mut machine = Machine::new(&program, 1 << 16);
                if let Some(native) = tier {
                    // Safety: `native` outlives this machine.
                    unsafe { machine.install_native(native) };
                }
                let args = prepare(&mut machine);
                let answer = machine.run(entry, &args, &budget()).expect("answers")[0];
                assert_eq!(machine.object_layout(answer), array);
                assert_eq!(
                    machine.payload_run(answer, 0, (2 * chunk * 3) as u32),
                    pattern[((chunk + 1) * 3) as usize..((3 * chunk + 1) * 3) as usize].to_vec(),
                    "native: {}",
                    tier.is_some()
                );
            }
        }

        /// **Every refusal a copy makes from compiled code is the VM's sentence, at
        /// the VM's span, with the VM's outcome — and leaves the destination as it
        /// was.**
        #[test]
        fn every_refusal_of_a_compiled_copy_is_the_vm_s() {
            let (program, copy_into, entry) = byte_copier();
            let native = compiled(&program, copy_into);
            // What each end is, their lengths, and `dst_at`, `src_at`, `count`.
            #[derive(Clone, Copy)]
            enum End {
                Null,
                Text,
                Run,
            }
            let rows = [
                (
                    "a null destination",
                    End::Null,
                    End::Text,
                    6i64,
                    3i64,
                    [0i64, 0, 3],
                ),
                ("a null source", End::Run, End::Null, 6, 3, [0, 0, 3]),
                ("a negative count", End::Run, End::Text, 6, 3, [0, 0, -1]),
                (
                    "a string destination",
                    End::Text,
                    End::Text,
                    6,
                    3,
                    [0, 0, 3],
                ),
                ("past the source", End::Run, End::Text, 3, 10, [0, 2, 5]),
                (
                    "a negative source offset",
                    End::Run,
                    End::Text,
                    3,
                    10,
                    [0, -1, 1],
                ),
                ("past the destination", End::Run, End::Text, 6, 3, [2, 0, 5]),
                (
                    "a negative destination offset",
                    End::Run,
                    End::Text,
                    6,
                    3,
                    [-1, 0, 1],
                ),
            ];
            for (what, dst_end, src_end, src_len, dst_len, [dst_at, src_at, count]) in rows {
                let prepare = |machine: &mut Machine<'_>| {
                    let mut place = |end: End, len: i64| match end {
                        End::Null => 0,
                        End::Text => machine.new_string(&"s".repeat(len as usize)).unwrap(),
                        End::Run => machine.allocate(machine.program.bytes_layout, len).unwrap(),
                    };
                    let src = place(src_end, src_len);
                    let dst = place(dst_end, dst_len);
                    vec![dst, dst_at as u64, src, src_at as u64, count as u64]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| match args[0] {
                    0 => Vec::new(),
                    dst => machine.string_bytes(dst),
                };
                let (said, left) = agree(what, &program, &native, entry, &prepare, &inspect);
                let (message, ..) = said.expect_err(what);
                assert!(
                    message.contains("runCopy") || message == null_object().message,
                    "{what}: {message}"
                );
                assert!(
                    left.iter().all(|byte| *byte == 0 || *byte == b's'),
                    "{what}: nothing was written"
                );
            }

            // The word copy's own refusals: another element family at either end,
            // one element past a source that is in range by words, and a negative
            // count.
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            build.string_layout();
            let point = build.structure("Point", &[("x", int), ("y", int)]);
            let points = build.layout(
                "Array<Point>",
                Shape::Elements {
                    elem: point,
                    growable: false,
                },
            );
            let ints = build.layout(
                "Array<Int>",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
            let copier = word_copier(&mut build, points, point);
            let entry = through_a_call(&mut build, copier, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, copier);
            for (what, dst_family, src_family, src_len, [dst_at, src_at, count]) in [
                (
                    "another family at the destination",
                    ints,
                    points,
                    4i64,
                    [0i64, 0, 1],
                ),
                ("another family at the source", points, ints, 8, [0, 0, 1]),
                ("one element past the source", points, points, 4, [0, 1, 4]),
                (
                    "a negative count of elements",
                    points,
                    points,
                    4,
                    [0, 0, -1],
                ),
            ] {
                let prepare = |machine: &mut Machine<'_>| {
                    let src = machine.allocate(src_family, src_len).unwrap();
                    let dst = machine.allocate(dst_family, 8).unwrap();
                    vec![dst, dst_at as u64, src, src_at as u64, count as u64]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| {
                    machine.payload_run(args[0], 0, machine.object_len(args[0]))
                };
                let (said, untouched) = agree(what, &program, &native, entry, &prepare, &inspect);
                said.expect_err(what);
                assert!(
                    untouched.iter().all(|word| *word == 0),
                    "{what}: nothing was written"
                );
            }
        }

        /// **A copy longer than a chunk, in compiled code, is stopped by fuel within
        /// ADR 0040's bound and by cancellation before it has done a chunk's work.**
        ///
        /// The objects are the test's own, so the callee is the copy and nothing
        /// else: the first safepoint a native run reaches is the helper's. A
        /// cancellation is answered there, before a byte is copied; fuel is gathered
        /// there and then spent chunk by chunk, so the overspend is the chunk loop's
        /// — one chunk plus one stride, as on the dispatch loop, and emphatically
        /// not the length of the copy.
        #[test]
        fn a_compiled_copy_longer_than_a_chunk_stops_within_its_bound() {
            const BYTES: i64 = 1 << 20;
            let (program, copy_into, entry) = byte_copier();
            let native = compiled(&program, copy_into);
            let words = (BYTES as u64).div_ceil(8);
            let prepare = |machine: &mut Machine<'_>| {
                let src = machine
                    .allocate(machine.program.bytes_layout, BYTES)
                    .unwrap();
                let dst = machine
                    .allocate(machine.program.bytes_layout, BYTES)
                    .unwrap();
                vec![dst, 0, src, 0, BYTES as u64]
            };
            let nothing = |_: &Machine<'_>, _: &[u64]| ();

            for limit in [1_024u64, 8_192, 40_000] {
                let bound = limit + words_of_bytes(BULK_CHUNK_BYTES) + SAFEPOINT_STRIDE;
                for tier in [None, Some(&native)] {
                    let budget = crate::budget::Budget::new(crate::budget::Limits {
                        fuel: Some(limit),
                        ..crate::budget::Limits::default()
                    });
                    let (said, (), tiers, _) = on(
                        &program,
                        tier,
                        1 << 22,
                        &budget.meter(),
                        entry,
                        &prepare,
                        &nothing,
                    );
                    let (.., outcome) = said.expect_err("a copy past its fuel is stopped");
                    assert_eq!(outcome, crate::trace::RunOutcome::Fuel);
                    if tier.is_some() {
                        assert!(tiers.vm_to_native >= 1, "{tiers:?}");
                    }
                    let spent = budget.fuel_spent();
                    assert!(
                        spent <= bound && spent < words,
                        "a {BYTES}-byte copy under a fuel limit of {limit} spent {spent} \
                         (native: {}), past the bound of {bound}; the whole copy would have \
                         been {words}",
                        tier.is_some()
                    );
                }
            }

            for tier in [None, Some(&native)] {
                let budget = crate::budget::Budget::new(crate::budget::Limits::default());
                budget.cancellation().cancel();
                let (said, (), tiers, work) = on(
                    &program,
                    tier,
                    1 << 22,
                    &budget.meter(),
                    entry,
                    &prepare,
                    &nothing,
                );
                let (.., outcome) = said.expect_err("a cancelled run does not answer");
                assert_eq!(outcome, crate::trace::RunOutcome::Cancelled);
                if tier.is_some() {
                    assert!(tiers.vm_to_native >= 1, "{tiers:?}");
                }
                let bound = words_of_bytes(BULK_CHUNK_BYTES) + SAFEPOINT_STRIDE;
                assert!(
                    work <= bound,
                    "a cancelled {BYTES}-byte copy did {work} work (native: {}), past {bound}",
                    tier.is_some()
                );
            }
        }

        /// **References copied by compiled code survive collections made from that
        /// compiled frame, half way through the copy and after the source is
        /// dropped.**
        ///
        /// `references_copied_into_a_run_survive_a_collection`'s fixture as the
        /// callee, so its garbage allocations are compiled `Inst::Alloc`s and every
        /// collection walks a compiled frame holding a half-copied run of `String`s.
        /// A collection *at a chunk's poll* is not something a single-task run can
        /// force — `Memory::poll` joins a collection another task asked for, and a
        /// copy allocates nothing — so, as on the dispatch loop, the collections
        /// are forced beside the copy rather than inside it.
        #[test]
        fn references_copied_by_compiled_code_survive_a_collection() {
            const COUNT: i64 = 10;
            const GARBAGE: u32 = 200;
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let text = build.string_layout();
            let strings = build.layout(
                "Array<String>",
                Shape::Elements {
                    elem: text,
                    growable: false,
                },
            );
            let ints = build.layout(
                "Array<Int>",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
            let first = build.args(&[(1, strings), (3, int), (0, strings), (3, int), (2, int)]);
            let second = build.args(&[(1, strings), (2, int), (0, strings), (2, int), (2, int)]);
            let garbage = |code: &mut Vec<Inst>| {
                for _ in 0..3 {
                    code.push(Inst::Alloc {
                        dst: 5,
                        layout: ints,
                        len: Len::Count(GARBAGE),
                    });
                    code.push(Inst::Clear {
                        slot: 5,
                        layout: ints,
                    });
                }
            };
            let mut code = vec![
                Inst::Int {
                    dst: 4,
                    value: COUNT,
                },
                Inst::Alloc {
                    dst: 1,
                    layout: strings,
                    len: Len::Slot(4),
                },
                Inst::Int {
                    dst: 2,
                    value: COUNT / 2,
                },
                Inst::Int { dst: 3, value: 0 },
                Inst::RunCopy {
                    args: first,
                    storage: Storage::Words(text),
                },
            ];
            garbage(&mut code);
            code.push(Inst::RunCopy {
                args: second,
                storage: Storage::Words(text),
            });
            code.push(Inst::Clear {
                slot: 0,
                layout: strings,
            });
            garbage(&mut code);
            code.push(Inst::Return { src: 1 });
            let inner = build.function(
                "copy_strings",
                &[strings],
                &[
                    Repr::Ref,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                ],
                strings,
                code,
            );
            let entry = through_a_call(&mut build, inner, Repr::Ref);
            let program = build.done();
            let native = compiled(&program, inner);

            let answered = |tier: Option<&NativeProgram>| {
                // One word more than the dispatch loop's fixture, for the caller's
                // frame: the heap is what has to be tight, and it is the same heap.
                let mut machine = Machine::new(&program, 600);
                if let Some(native) = tier {
                    // Safety: `native` outlives this machine.
                    unsafe { machine.install_native(native) };
                }
                let src = machine.allocate(strings, COUNT).unwrap();
                for at in 0..COUNT {
                    let word = machine.new_string(&format!("string {at}")).unwrap();
                    machine.set_payload(src, at as u32, word);
                }
                let before = machine.collected().collections;
                let dst = machine
                    .run(entry, &[src], &budget())
                    .expect("the run answers the destination")[0];
                let collections = machine.collected().collections - before;
                let texts: Vec<Vec<u8>> = (0..COUNT)
                    .map(|at| machine.string_bytes(machine.payload(dst, at as u32)))
                    .collect();
                (collections, texts, machine.tiers())
            };
            let (vm_collections, vm_texts, _) = answered(None);
            let (collections, texts, tiers) = answered(Some(&native));
            assert!(tiers.vm_to_native >= 1, "{tiers:?}");
            for (tier, collected) in [("vm", vm_collections), ("native", collections)] {
                assert!(
                    collected >= 1,
                    "{tier}: the fixture exists to collect with a half-copied run live, and \
                     it collected {collected} time(s)"
                );
            }
            assert_eq!(texts, vm_texts);
            for (at, text) in texts.iter().enumerate() {
                assert_eq!(text, format!("string {at}").as_bytes());
            }
        }
    }

    /// [ADR 0062]'s buffer window — `GROWABLE_ENSURE_*`, `GROWABLE_COMMIT_*` and
    /// `RUN_STORE_BYTES` — before any Cove source produces it.
    ///
    /// The programs are written in the IR for the reason [`compiled`] gives: no
    /// lowering emits these instructions yet, so a hand-written window is the only
    /// thing that can run one. Each well-formed window passes `cove_ir::verify`'s
    /// reservation rule, which `Build::done` asserts. The refusals a window could
    /// never reach — a commit past the capacity is exactly what the rule proves
    /// cannot happen — are built with `Build::program` directly, because the
    /// runtime check exists for bytecode that no static rule has read.
    ///
    /// [ADR 0062]: ../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    mod window {
        use super::*;
        // The census's rows are this, and the test module reads them by name.
        use cove_ir::legalize::Pattern;

        struct Windows {
            program: Program,
            /// `alloc(capacity) -> ByteBuffer`.
            alloc: FunctionId,
            /// `push_byte(owner, value) -> owner`, through a window.
            push_byte: FunctionId,
            /// `push_pair(owner, a, b) -> owner`, a two-word element through a
            /// window.
            push_pair: FunctionId,
            /// `append(owner, text) -> owner`, a whole `String` through a window
            /// whose write is a `run-copy`.
            append: FunctionId,
            /// `finish(owner) -> String`.
            finish: FunctionId,
            /// `ensure(owner, n) -> owner`, a byte ensure nothing is written into.
            ensure: FunctionId,
            /// `store(run, at, value) -> run`, a byte store on its own.
            store: FunctionId,
            pair_vector: LayoutId,
            /// The element, which only the compiled cases name, in the call they
            /// build in front of `push_pair`.
            #[cfg_attr(not(feature = "template"), allow(dead_code))]
            pair: LayoutId,
            pair_store: LayoutId,
        }

        fn windows() -> Windows {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let str_layout = build.string_layout();
            let bytes = build.bytes_layout();
            let owner = build.buffer_layout();
            let pair = build.structure("Pair", &[("a", int), ("b", int)]);
            let pair_store = build.layout(
                "Store<Pair>",
                Shape::Elements {
                    elem: pair,
                    growable: true,
                },
            );
            let pair_vector = build.layout("Vector<Pair>", Shape::Vector { elem: pair });
            let packed = Storage::PackedBytes;
            let words = Storage::Words(pair);
            let length = |dst, obj| Inst::LoadField {
                dst,
                obj,
                at: 0,
                layout: int,
            };
            let store_of = |dst, obj, layout| Inst::LoadField {
                dst,
                obj,
                at: 1,
                layout,
            };

            let alloc = build.function(
                "alloc",
                &[int],
                &[Repr::Int, Repr::Ref],
                owner,
                vec![
                    Inst::GrowableAlloc {
                        dst: 1,
                        capacity: 0,
                        storage: packed,
                    },
                    Inst::Return { src: 1 },
                ],
            );
            // s0 owner, s1 value, s2 length, s3 count, s4 store, s5 commit count.
            let push_byte = build.function(
                "push_byte",
                &[owner, int],
                &[
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                ],
                owner,
                vec![
                    length(2, 0),
                    Inst::Int { dst: 3, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 3,
                        storage: packed,
                    },
                    store_of(4, 0, bytes),
                    Inst::RunStore {
                        run: 4,
                        index: 2,
                        src: 1,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 4,
                        layout: bytes,
                    },
                    Inst::Int { dst: 5, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 5,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1..s2 the pair, s3 length, s4 count, s5 store, s6 commit.
            let push_pair = build.function(
                "push_pair",
                &[pair_vector, pair],
                &[
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                ],
                pair_vector,
                vec![
                    length(3, 0),
                    Inst::Int { dst: 4, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 4,
                        storage: words,
                    },
                    store_of(5, 0, pair_store),
                    Inst::StoreElem {
                        obj: 5,
                        index: 3,
                        src: 1,
                        layout: pair,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: pair_store,
                    },
                    Inst::Int { dst: 6, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 6,
                        storage: words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1 text, s2 count, s3 length, s4 store, s5 nought.
            let copy = build.args(&[(4, bytes), (3, int), (1, str_layout), (5, int), (2, int)]);
            let append = build.function(
                "append",
                &[owner, str_layout],
                &[
                    Repr::Ref,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                ],
                owner,
                vec![
                    Inst::Len { dst: 2, obj: 1 },
                    length(3, 0),
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 2,
                        storage: packed,
                    },
                    store_of(4, 0, bytes),
                    Inst::Int { dst: 5, value: 0 },
                    Inst::RunCopy {
                        args: copy,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 4,
                        layout: bytes,
                    },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 2,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let finish = build.function(
                "finish",
                &[owner],
                &[Repr::Ref],
                str_layout,
                vec![
                    Inst::RunFinish {
                        dst: 0,
                        owner: 0,
                        target: str_layout,
                        validation: Validation::Utf8,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let ensure = build.function(
                "ensure",
                &[owner, int],
                &[Repr::Ref, Repr::Int],
                owner,
                vec![
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 1,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let store = build.function(
                "store",
                &[bytes, int, int],
                &[Repr::Ref, Repr::Int, Repr::Int],
                bytes,
                vec![
                    Inst::RunStore {
                        run: 0,
                        index: 1,
                        src: 2,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            Windows {
                program: build.done(),
                alloc,
                push_byte,
                push_pair,
                append,
                finish,
                ensure,
                store,
                pair_vector,
                pair,
                pair_store,
            }
        }

        /// A `Vector<Pair>` of `capacity` elements and none of them value.
        fn a_pair_vector(machine: &mut Machine<'_>, w: &Windows, capacity: i64) -> u64 {
            let store = machine.allocate(w.pair_store, capacity).unwrap();
            machine.push_temp(store);
            let owner = machine.allocate(w.pair_vector, 0).unwrap();
            machine.set_payload(owner, runs::GROWABLE_LEN, 0);
            machine.set_payload(owner, runs::GROWABLE_STORE, store);
            owner
        }

        fn message(result: Result<Vec<u64>, RuntimeError>) -> String {
            result.expect_err("the run is refused").message
        }

        /// **A byte window is a push**: a hundred bytes one window at a time, from
        /// a buffer with room for none of them, grow the store several times and
        /// finish into the same `String` the composite push builds.
        #[test]
        fn a_byte_window_builds_what_a_push_builds() {
            let w = windows();
            let mut machine = Machine::new(&w.program, 1 << 16);
            let owner = machine.run(w.alloc, &[0], &budget()).unwrap()[0];
            let text: Vec<u8> = (0..100).map(|n| b'a' + (n % 26) as u8).collect();
            for byte in &text {
                let answered = machine
                    .run(w.push_byte, &[owner, u64::from(*byte)], &budget())
                    .unwrap();
                assert_eq!(answered[0], owner, "the owner does not move");
            }
            let string = machine.run(w.finish, &[owner], &budget()).unwrap()[0];
            assert_eq!(machine.string_bytes(string), text);
        }

        /// **A word window writes a whole element at the stride**, and the store
        /// it grows into keeps every element pushed before.
        #[test]
        fn a_word_window_pushes_a_two_word_element() {
            let w = windows();
            let mut machine = Machine::new(&w.program, 1 << 16);
            let owner = a_pair_vector(&mut machine, &w, 1);
            for n in 0..20u64 {
                machine
                    .run(w.push_pair, &[owner, n, 1000 + n], &budget())
                    .unwrap();
            }
            assert_eq!(machine.payload(owner, runs::GROWABLE_LEN), 20);
            let store = machine.payload(owner, runs::GROWABLE_STORE);
            for n in 0..20u32 {
                assert_eq!(machine.payload(store, 2 * n), u64::from(n));
                assert_eq!(machine.payload(store, 2 * n + 1), 1000 + u64::from(n));
            }
        }

        /// **An append window is an append**: a copy into the room an ensure made,
        /// published by one commit, across growths and word boundaries.
        #[test]
        fn an_append_window_copies_a_string() {
            let w = windows();
            let mut machine = Machine::new(&w.program, 1 << 16);
            let owner = machine.run(w.alloc, &[0], &budget()).unwrap()[0];
            let pieces = ["hello, ", "", "world", " — ", "a longer piece than a store"];
            for piece in pieces {
                let text = machine.new_string(piece).unwrap();
                machine.run(w.append, &[owner, text], &budget()).unwrap();
            }
            let string = machine.run(w.finish, &[owner], &budget()).unwrap()[0];
            assert_eq!(machine.string_bytes(string), pieces.concat().as_bytes());
        }

        /// **A growth inside a window that collects loses nothing**: the heap is
        /// small, garbage is allocated between windows, and the assertion on
        /// `collections` is what says a collection really happened while an
        /// ensure grew the store.
        #[test]
        fn a_window_whose_ensure_collects_keeps_every_byte() {
            const BYTES: i64 = 200;
            let mut build = Build::default();
            build.scalar(Repr::Int);
            let str_layout = build.string_layout();
            let bytes = build.bytes_layout();
            let owner = build.buffer_layout();
            let int = build.scalar(Repr::Int);
            let packed = Storage::PackedBytes;
            // s0 capacity, s1 owner, s2 value, s3 garbage, s4 length, s5 count,
            // s6 store.
            let mut code = vec![
                Inst::Int { dst: 0, value: 0 },
                Inst::GrowableAlloc {
                    dst: 1,
                    capacity: 0,
                    storage: packed,
                },
                Inst::Int {
                    dst: 2,
                    value: i64::from(b'x'),
                },
                Inst::Int { dst: 0, value: 512 },
            ];
            for at in 0..BYTES {
                code.extend([
                    Inst::LoadField {
                        dst: 4,
                        obj: 1,
                        at: 0,
                        layout: int,
                    },
                    Inst::Int { dst: 5, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 1,
                        additional: 5,
                        storage: packed,
                    },
                    Inst::LoadField {
                        dst: 6,
                        obj: 1,
                        at: 1,
                        layout: bytes,
                    },
                    Inst::RunStore {
                        run: 6,
                        index: 4,
                        src: 2,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 6,
                        layout: bytes,
                    },
                    Inst::GrowableCommit {
                        owner: 1,
                        count: 5,
                        storage: packed,
                    },
                ]);
                if at % 8 == 0 {
                    code.push(Inst::GrowableAlloc {
                        dst: 3,
                        capacity: 0,
                        storage: packed,
                    });
                    code.push(Inst::Clear {
                        slot: 3,
                        layout: owner,
                    });
                }
            }
            code.push(Inst::RunFinish {
                dst: 1,
                owner: 1,
                target: str_layout,
                validation: Validation::Utf8,
                storage: packed,
            });
            code.push(Inst::Return { src: 1 });
            let entry = build.function(
                "grow_in_windows",
                &[],
                &[
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                ],
                str_layout,
                code,
            );
            let program = build.done();
            let mut machine = Machine::new(&program, 320);
            let before = machine.collected().collections;
            let answer = machine.run(entry, &[], &budget()).expect("a string");
            assert!(
                machine.collected().collections > before,
                "this fixture exists to collect inside an ensure's growth"
            );
            assert_eq!(machine.string_bytes(answer[0]), vec![b'x'; BYTES as usize]);
        }

        /// **Each refusal is the machine's, in its own words**: a negative room,
        /// an ensure on a consumed buffer, a byte that is not one, an offset past
        /// the run, a store into a `String`.
        #[test]
        fn a_window_instruction_refuses_what_it_cannot_do() {
            let w = windows();
            let mut machine = Machine::new(&w.program, 1 << 16);
            let owner = machine.run(w.alloc, &[0], &budget()).unwrap()[0];
            assert!(
                message(machine.run(w.ensure, &[owner, (-1i64) as u64], &budget()))
                    .contains("room is never negative")
            );
            let run = machine.payload(owner, runs::GROWABLE_STORE);
            assert!(message(machine.run(w.store, &[run, 0, 256], &budget()))
                .contains("`runStore`'s value is `256`, and a byte is 0 to 255"));
            assert!(
                message(machine.run(w.store, &[run, 0, (-1i64) as u64], &budget()))
                    .contains("a byte is 0 to 255")
            );
            let capacity = u64::from(machine.mem.object_len(run));
            assert!(
                message(machine.run(w.store, &[run, capacity, 1], &budget()))
                    .contains("a byte offset into this run is 0 to")
            );
            let text = machine.new_string("text").unwrap();
            assert!(message(machine.run(w.store, &[text, 0, 1], &budget()))
                .contains("not a byte run under construction"));
            machine.run(w.finish, &[owner], &budget()).unwrap();
            assert_eq!(
                message(machine.run(w.ensure, &[owner, 1], &budget())),
                "`growableEnsure` was called on a byte buffer that `finish()` already consumed"
            );
        }

        /// **A commit the bytecode could hold and no verified lowering can** —
        /// past the room, or negative — is refused at run time with the length
        /// unchanged. Built without `Build::done`, because the reservation rule
        /// refuses a commit with no window, which is the point.
        #[test]
        fn a_commit_past_the_room_is_refused_and_changes_nothing() {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            build.string_layout();
            build.bytes_layout();
            let owner = build.buffer_layout();
            let alloc = build.function(
                "alloc",
                &[int],
                &[Repr::Int, Repr::Ref],
                owner,
                vec![
                    Inst::GrowableAlloc {
                        dst: 1,
                        capacity: 0,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Return { src: 1 },
                ],
            );
            let commit = build.function(
                "commit",
                &[owner, int],
                &[Repr::Ref, Repr::Int],
                owner,
                vec![
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 1,
                        storage: Storage::PackedBytes,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let program = build.program;
            assert!(
                cove_ir::verify(&program).is_err(),
                "the static rule refuses a commit with no window"
            );
            let mut machine = Machine::new(&program, 1 << 16);
            let buffer = machine.run(alloc, &[0], &budget()).unwrap()[0];
            let capacity = u64::from(
                machine
                    .mem
                    .object_len(machine.payload(buffer, runs::GROWABLE_STORE)),
            );
            for count in [capacity + 1, (-1i64) as u64] {
                assert!(message(machine.run(commit, &[buffer, count], &budget()))
                    .contains("a commit publishes only room an ensure made"));
                assert_eq!(machine.payload(buffer, runs::GROWABLE_LEN), 0);
            }
            machine.run(commit, &[buffer, capacity], &budget()).unwrap();
            assert_eq!(machine.payload(buffer, runs::GROWABLE_LEN), capacity);
        }

        // ---- ADR 0062's fused heads -------------------------------------------

        /// Windows whose functions answer their **whole frame**, so that a fused
        /// run and an unfused one are compared word for word: every slot a row
        /// writes, not only the owner. Each instruction has a span of its own, so
        /// that a refusal reported at the wrong row is a different answer.
        struct Framed {
            program: Program,
            /// `(Vector<Int>, Int)`: a push of one word, with no clear.
            push_int: FunctionId,
            /// `(Vector<Pair>, Pair)`: a push of two words, with both optional
            /// rows.
            push_pair: FunctionId,
            /// `(ByteBuffer, Int)`: a byte push, with both optional rows.
            push_byte: FunctionId,
            /// `(ByteBuffer, String)`: a byte append whose offset constant follows
            /// the store read.
            append_text: FunctionId,
            /// `(Vector<Pair>, Array<Pair>)`: a word append whose offset constant
            /// precedes it, with no clear.
            append_pairs: FunctionId,
            /// `(Vector<Int>, Array<Int>, from, count)`: a word append of
            /// one-word elements whose offset and count are the frame's, which
            /// is `std.map`'s keyed extend's shape — so a test may choose any
            /// range, the source may be the destination's own store, and a
            /// copy may be longer than a bulk chunk.
            append_run: FunctionId,
            /// `(Vector<String>, Array<String>)`: a word append of 32
            /// *references* from offset 1 with every optional row — a count
            /// constant, an offset constant after the store read, a clear and
            /// a second count, which is the longest window there is.
            append_texts: FunctionId,
            int_vector: LayoutId,
            int_store: LayoutId,
            pair_vector: LayoutId,
            pair_store: LayoutId,
            pairs: LayoutId,
            /// `Array<Int>`, the source `append_run` reads.
            ints: LayoutId,
            text_vector: LayoutId,
            text_store: LayoutId,
            /// `Array<String>`, the source `append_texts` reads.
            texts: LayoutId,
        }

        fn framed() -> Framed {
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let str_layout = build.string_layout();
            let bytes = build.bytes_layout();
            let owner = build.buffer_layout();
            let pair = build.structure("Pair", &[("a", int), ("b", int)]);
            let int_store = build.layout(
                "Store<Int>",
                Shape::Elements {
                    elem: int,
                    growable: true,
                },
            );
            let int_vector = build.layout("Vector<Int>", Shape::Vector { elem: int });
            let pair_store = build.layout(
                "Store<Pair>",
                Shape::Elements {
                    elem: pair,
                    growable: true,
                },
            );
            let pair_vector = build.layout("Vector<Pair>", Shape::Vector { elem: pair });
            let pairs = build.layout(
                "Array<Pair>",
                Shape::Elements {
                    elem: pair,
                    growable: false,
                },
            );
            let ints = build.layout(
                "Array<Int>",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
            // A run of `String`s: one word an element, and every one of them a
            // reference the collector follows.
            let text_store = build.layout(
                "Store<String>",
                Shape::Elements {
                    elem: str_layout,
                    growable: true,
                },
            );
            let text_vector = build.layout("Vector<String>", Shape::Vector { elem: str_layout });
            let texts = build.layout(
                "Array<String>",
                Shape::Elements {
                    elem: str_layout,
                    growable: false,
                },
            );
            let packed = Storage::PackedBytes;
            let length = |dst, obj| Inst::LoadField {
                dst,
                obj,
                at: 0,
                layout: int,
            };
            let store_of = |dst, obj, layout| Inst::LoadField {
                dst,
                obj,
                at: 1,
                layout,
            };
            // A function over a frame of `slots`, answering all of it.
            let framed = |build: &mut Build,
                          name: &str,
                          params: &[LayoutId],
                          slots: &[LayoutId],
                          code: Vec<Inst>| {
                let names: Vec<String> = (0..slots.len()).map(|at| format!("s{at}")).collect();
                let fields: Vec<(&str, LayoutId)> = names
                    .iter()
                    .map(String::as_str)
                    .zip(slots.iter().copied())
                    .collect();
                let frame = build.structure(&format!("Frame of {name}"), &fields);
                let reprs = build.program.layout(frame).words.clone();
                let id = build.function(name, params, &reprs, frame, code);
                let function = &mut build.program.functions[id.index()];
                function.spans = (0..function.code.len())
                    .map(|pc| Span::new(cove_diag::FileId(0), pc as _, pc as _))
                    .collect();
                id
            };

            // s0 owner, s1 value, s2 length, s3 count, s4 store, s5 second count.
            let words = Storage::Words(int);
            let push_int = framed(
                &mut build,
                "push_int",
                &[int_vector, int],
                &[int_vector, int, int, int, int_store, int],
                vec![
                    length(2, 0),
                    Inst::Int { dst: 3, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 3,
                        storage: words,
                    },
                    store_of(4, 0, int_store),
                    Inst::StoreElem {
                        obj: 4,
                        index: 2,
                        src: 1,
                        layout: int,
                    },
                    Inst::Int { dst: 5, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 5,
                        storage: words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1..s2 the pair, s3 length, s4 count, s5 store, s6 second.
            let words = Storage::Words(pair);
            let push_pair = framed(
                &mut build,
                "push_pair",
                &[pair_vector, pair],
                &[pair_vector, pair, int, int, pair_store, int],
                vec![
                    length(3, 0),
                    Inst::Int { dst: 4, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 4,
                        storage: words,
                    },
                    store_of(5, 0, pair_store),
                    Inst::StoreElem {
                        obj: 5,
                        index: 3,
                        src: 1,
                        layout: pair,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: pair_store,
                    },
                    Inst::Int { dst: 6, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 6,
                        storage: words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1 value, s2 length, s3 count, s4 store, s5 second count.
            let push_byte = framed(
                &mut build,
                "push_byte",
                &[owner, int],
                &[owner, int, int, int, bytes, int],
                vec![
                    length(2, 0),
                    Inst::Int { dst: 3, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 3,
                        storage: packed,
                    },
                    store_of(4, 0, bytes),
                    Inst::RunStore {
                        run: 4,
                        index: 2,
                        src: 1,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 4,
                        layout: bytes,
                    },
                    Inst::Int { dst: 5, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 5,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1 text, s2 count, s3 length, s4 store, s5 nought.
            let copy = build.args(&[(4, bytes), (3, int), (1, str_layout), (5, int), (2, int)]);
            let append_text = framed(
                &mut build,
                "append_text",
                &[owner, str_layout],
                &[owner, str_layout, int, int, bytes, int],
                vec![
                    Inst::Len { dst: 2, obj: 1 },
                    length(3, 0),
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 2,
                        storage: packed,
                    },
                    store_of(4, 0, bytes),
                    Inst::Int { dst: 5, value: 0 },
                    Inst::RunCopy {
                        args: copy,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 4,
                        layout: bytes,
                    },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 2,
                        storage: packed,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // s0 owner, s1 run, s2 count, s3 length, s4 store, s5 nought.
            let copy = build.args(&[(4, pair_store), (3, int), (1, pairs), (5, int), (2, int)]);
            let append_pairs = framed(
                &mut build,
                "append_pairs",
                &[pair_vector, pairs],
                &[pair_vector, pairs, int, int, pair_store, int],
                vec![
                    Inst::Len { dst: 2, obj: 1 },
                    length(3, 0),
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 2,
                        storage: words,
                    },
                    Inst::Int { dst: 5, value: 0 },
                    store_of(4, 0, pair_store),
                    Inst::RunCopy {
                        args: copy,
                        storage: words,
                    },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 2,
                        storage: words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // `std.map`'s keyed extend, in the shape ADR 0062 asks for it: the
            // range is the frame's, so a test says which elements move.
            // s0 owner, s1 run, s2 from, s3 count, s4 length, s5 store.
            let int_words = Storage::Words(int);
            let copy = build.args(&[(5, int_store), (4, int), (1, ints), (2, int), (3, int)]);
            let append_run = framed(
                &mut build,
                "append_run",
                &[int_vector, ints, int, int],
                &[int_vector, ints, int, int, int, int_store],
                vec![
                    length(4, 0),
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 3,
                        storage: int_words,
                    },
                    store_of(5, 0, int_store),
                    Inst::RunCopy {
                        args: copy,
                        storage: int_words,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: int_store,
                    },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 3,
                        storage: int_words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            // The longest window there is: every optional row, over elements
            // that are references.
            // s0 owner, s1 run, s2 count, s3 length, s4 store, s5 nought,
            // s6 second count.
            let text_words = Storage::Words(str_layout);
            let copy = build.args(&[(4, text_store), (3, int), (1, texts), (5, int), (2, int)]);
            let append_texts = framed(
                &mut build,
                "append_texts",
                &[text_vector, texts],
                &[text_vector, texts, int, int, text_store, int, int],
                vec![
                    length(3, 0),
                    Inst::Int { dst: 2, value: 32 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 2,
                        storage: text_words,
                    },
                    store_of(4, 0, text_store),
                    // Not nought: a fast path that did not write this word
                    // would be indistinguishable from one that did, since a
                    // fresh frame is zeroed.
                    Inst::Int { dst: 5, value: 1 },
                    Inst::RunCopy {
                        args: copy,
                        storage: text_words,
                    },
                    Inst::Clear {
                        slot: 4,
                        layout: text_store,
                    },
                    Inst::Int { dst: 6, value: 32 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 6,
                        storage: text_words,
                    },
                    Inst::Return { src: 0 },
                ],
            );
            let program = build.done();
            // Every window here is one `crate::legalize` recognises, of the
            // pattern the fixture is named for.
            for (id, pattern) in [
                (push_int, cove_ir::legalize::Pattern::PushWords),
                (push_pair, cove_ir::legalize::Pattern::PushWords),
                (push_byte, cove_ir::legalize::Pattern::PushByte),
                (append_text, cove_ir::legalize::Pattern::AppendBytes),
                (append_pairs, cove_ir::legalize::Pattern::AppendWords),
                (append_run, cove_ir::legalize::Pattern::AppendWords),
                (append_texts, cove_ir::legalize::Pattern::AppendWords),
            ] {
                let found = cove_ir::legalize::windows(&program, program.function(id));
                assert_eq!(
                    found
                        .iter()
                        .map(|window| window.pattern)
                        .collect::<Vec<_>>(),
                    [pattern]
                );
            }
            Framed {
                program,
                push_int,
                push_pair,
                push_byte,
                append_text,
                append_pairs,
                append_run,
                append_texts,
                int_vector,
                int_store,
                pair_vector,
                pair_store,
                pairs,
                ints,
                text_vector,
                text_store,
                texts,
            }
        }

        /// Replaces the machine's encoding with one in which no window is fused:
        /// every row its own instruction's encoding, which is what a machine runs
        /// when the question is what the rows would have done.
        fn unfused(machine: &mut Machine<'_>) {
            let functions = machine
                .program
                .functions
                .iter()
                .map(|function| {
                    function
                        .code
                        .iter()
                        .enumerate()
                        .map(|(pc, inst)| {
                            cove_ir::bytecode::encode(inst, pc as u32).expect("it encodes")
                        })
                        .collect()
                })
                .collect();
            machine.encoded = Ok(Arc::new(Encoded { functions }));
        }

        /// Everything one run left that a fused window could get wrong.
        #[derive(Debug, PartialEq)]
        struct Ran<T> {
            /// The answer's words, or the refusal with its span.
            said: String,
            /// What the test reads of the heap afterwards.
            heap: T,
            instructions: u64,
            fuel: u64,
            collections: u64,
        }

        /// One run of `entry`, fused or not, under an optional fuel limit, with
        /// its boundary counted.
        #[allow(clippy::too_many_arguments)]
        fn ran<T>(
            program: &Program,
            entry: FunctionId,
            fused: bool,
            heap: usize,
            fuel: Option<u64>,
            prepare: &dyn Fn(&mut Machine<'_>) -> Vec<u64>,
            inspect: &dyn Fn(&Machine<'_>, &[u64]) -> T,
        ) -> (Ran<T>, crate::vm::report::BoundaryReport, u64) {
            let limits = crate::budget::Limits {
                fuel,
                ..crate::budget::Limits::default()
            };
            let budget = crate::budget::Budget::new(limits);
            let mut machine = Machine::new(program, heap);
            if !fused {
                unfused(&mut machine);
            }
            let args = prepare(&mut machine);
            machine.count_boundary(native::Tiers::default());
            let result = machine.run(entry, &args, &budget.meter());
            let report = machine.boundary(None).expect("the run was counted");
            let ran = Ran {
                said: format!("{result:?}"),
                heap: inspect(&machine, &args),
                instructions: machine.instructions(),
                fuel: budget.fuel_spent(),
                collections: machine.collected().collections,
            };
            let fast = report.windows.fast.iter().sum::<u64>();
            (ran, report, fast)
        }

        /// **A fused window is the rows it stands for**: the same answer or the
        /// same refusal at the same span, the same heap, the same instructions,
        /// fuel and collections. Answers the fused run, its report — which says
        /// how many windows fused and how many dispatches they took — and how
        /// many windows a fused head's own fast path ran whole, which is
        /// `BoundaryReport::windows`' `fast` row summed over the patterns and
        /// the only thing that separates a fast path that is right from one
        /// that declines everything.
        #[allow(clippy::too_many_arguments)]
        fn fuses_as_unfused<T: PartialEq + std::fmt::Debug>(
            what: &str,
            program: &Program,
            entry: FunctionId,
            heap: usize,
            fuel: Option<u64>,
            prepare: &dyn Fn(&mut Machine<'_>) -> Vec<u64>,
            inspect: &dyn Fn(&Machine<'_>, &[u64]) -> T,
        ) -> (Ran<T>, crate::vm::report::BoundaryReport, u64) {
            let (fused, report, fast) = ran(program, entry, true, heap, fuel, prepare, inspect);
            let (rows, unreport, unfast) = ran(program, entry, false, heap, fuel, prepare, inspect);
            assert_eq!(fused, rows, "{what}");
            assert_eq!(
                (unreport.fusions, unreport.encoded_dispatches, unfast),
                ([0; 4], unreport.encoded_instructions, 0),
                "{what}: the unfused run fused nothing"
            );
            assert_eq!(
                unreport.windows,
                crate::vm::report::Windows::default(),
                "{what}: a run with no fused head reaches no fused arm, so the \
                 census records nothing at all"
            );
            assert_eq!(
                report.encoded_instructions, unreport.encoded_instructions,
                "{what}"
            );
            (fused, report, fast)
        }

        /// The words of a run's store, a unit being `stride` words.
        fn store_words(machine: &Machine<'_>, owner: u64, stride: u32) -> (u64, Vec<u64>) {
            let len = machine.payload(owner, runs::GROWABLE_LEN);
            let store = machine.payload(owner, runs::GROWABLE_STORE);
            let words = match store {
                0 => Vec::new(),
                _ => (0..machine.mem.object_len(store) * stride)
                    .map(|at| machine.payload(store, at))
                    .collect(),
            };
            (len, words)
        }

        /// A vector of `store` units of which `len` are value, or a consumed one.
        fn vector_of(
            machine: &mut Machine<'_>,
            vector: LayoutId,
            store: LayoutId,
            len: u64,
            capacity: i64,
            consumed: bool,
        ) -> u64 {
            let run = machine.allocate(store, capacity).unwrap();
            machine.push_temp(run);
            let owner = machine.allocate(vector, 0).unwrap();
            machine.set_payload(owner, runs::GROWABLE_LEN, if consumed { 0 } else { len });
            machine.set_payload(owner, runs::GROWABLE_STORE, if consumed { 0 } else { run });
            owner
        }

        const PUSH_WORDS: usize = 0;
        const PUSH_BYTE: usize = 1;
        const APPEND_BYTES: usize = 2;
        const APPEND_WORDS: usize = 3;

        /// **A push of a word and of a two-word element**, into room, into none,
        /// and onto a consumed vector: the first fuses in one dispatch, the
        /// second fuses through the growth, and the third is refused at the
        /// ensure in the ensure's words.
        #[test]
        fn a_fused_word_push_is_the_rows_it_stands_for() {
            let f = framed();
            for (len, capacity, consumed) in [(1u64, 4i64, false), (1, 1, false), (0, 4, true)] {
                let what = format!("an Int push at {len} of {capacity}, consumed: {consumed}");
                let prepare = |machine: &mut Machine<'_>| {
                    let owner =
                        vector_of(machine, f.int_vector, f.int_store, len, capacity, consumed);
                    vec![owner, 42]
                };
                let inspect =
                    |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.push_int,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                // The fast path runs a window whole only where the store has
                // room: a growth is `fused_push`' own slower half.
                assert_eq!(
                    fast,
                    u64::from(!consumed && len < capacity as u64),
                    "{what}"
                );
                // The census says the same thing with the reason attached: a
                // consumed vector is a store the fast path cannot read, and a
                // full one is the window it ran the ordinary way.
                assert_eq!(report.windows.run(Pattern::PushWords), 1, "{what}");
                if consumed {
                    assert!(fused.said.contains("consumed"), "{what}: {}", fused.said);
                    assert!(fused.said.contains("start: 2"), "{what}: at the ensure");
                    assert_eq!(report.fusions, [0; 4], "{what}");
                    assert_eq!(
                        report.windows.declined[PUSH_WORDS][Decline::Store.index()],
                        1,
                        "{what}"
                    );
                } else {
                    assert_eq!(fused.heap.0, len + 1, "{what}");
                    assert_eq!(report.fusions, [1, 0, 0, 0], "{what}");
                    let grew = u64::from(len == capacity as u64);
                    assert_eq!(report.windows.fast[PUSH_WORDS], 1 - grew, "{what}");
                    assert_eq!(report.windows.slow[PUSH_WORDS], grew, "{what}");
                    // A growth is counted where the store is replaced, which
                    // is why it is a word growth and not a byte one.
                    assert_eq!(report.growths, [0, grew], "{what}");
                    // Seven rows and a return, in two dispatches.
                    assert_eq!(
                        (report.encoded_instructions, report.encoded_dispatches),
                        (8, 2),
                        "{what}"
                    );
                }
            }
            // A null owner, and an object that is not a vector: refused at the
            // head as `LOAD_FIELD` refuses them, or run as rows.
            for owner in [None, Some(())] {
                let what = format!("a push onto {owner:?}");
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = match owner {
                        None => 0,
                        Some(()) => machine.new_string("not a vector at all").unwrap(),
                    };
                    vec![owner, 42]
                };
                let inspect = |_: &Machine<'_>, _: &[u64]| ();
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.push_int,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                assert!(fused.said.starts_with("Err("), "{what}: {}", fused.said);
                assert_eq!((report.fusions, fast), ([0; 4], 0), "{what}");
            }
            for capacity in [4i64, 1] {
                let what = format!("a Pair push into {capacity}");
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = vector_of(machine, f.pair_vector, f.pair_store, 1, capacity, false);
                    vec![owner, 7, 8]
                };
                let inspect =
                    |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 2);
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.push_pair,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                assert_eq!(fused.heap.0, 2, "{what}");
                assert_eq!(&fused.heap.1[2..4], &[7, 8], "{what}");
                assert_eq!(report.fusions[PUSH_WORDS], 1, "{what}");
                assert_eq!(report.encoded_dispatches, 2, "{what}");
                assert_eq!(fast, u64::from(capacity > 1), "{what}");
            }
        }

        /// **A push that has to grow is grown by the fused arm itself**, and a
        /// growth is where a push allocates: so one whose allocation collects
        /// first, and one the heap has no room for at all, must still be the
        /// rows — the same collections, and the same refusal at the ensure's
        /// span with the same instructions and fuel spent.
        #[test]
        fn a_fused_push_that_grows_collects_and_refuses_as_the_rows_do() {
            let f = framed();
            // A heap of 256 words: a full `Vector<Int>` of 32, and garbage enough
            // beside it that the growth to 64 has to collect before it fits.
            let collects = |machine: &mut Machine<'_>| {
                let owner = vector_of(machine, f.int_vector, f.int_store, 32, 32, false);
                machine.push_temp(owner);
                while machine.new_string("garbage, and nothing holds it").is_ok()
                    && machine.mem.heap_words() < 200
                {}
                vec![owner, 42]
            };
            // A heap of 128 words, and a full vector of 64 that cannot double.
            let refuses = |machine: &mut Machine<'_>| {
                let owner = vector_of(machine, f.int_vector, f.int_store, 64, 64, false);
                vec![owner, 42]
            };
            let inspect = |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
            let (fused, report, fast) = fuses_as_unfused(
                "a growth that collects",
                &f.program,
                f.push_int,
                256,
                None,
                &collects,
                &inspect,
            );
            assert!(fused.collections > 0, "the growth collected: {fused:?}");
            assert_eq!(fused.heap.0, 33);
            assert_eq!(fused.heap.1[32], 42);
            assert_eq!(report.fusions[PUSH_WORDS], 1);
            assert_eq!(report.encoded_dispatches, 2);
            assert_eq!(fast, 0, "a growth is not the fast path");
            // What `fusions` cannot say: the window ran, the fast path's write
            // did not, and the store was replaced.
            assert_eq!(report.windows.slow[PUSH_WORDS], 1);
            assert_eq!(report.growths, [0, 1]);

            let (fused, report, fast) = fuses_as_unfused(
                "a growth with no room anywhere",
                &f.program,
                f.push_int,
                128,
                None,
                &refuses,
                &inspect,
            );
            assert!(fused.said.contains("no memory left"), "{}", fused.said);
            assert!(
                fused.said.contains("start: 2"),
                "at the ensure: {}",
                fused.said
            );
            assert_eq!(fused.heap.0, 64, "nothing was published");
            assert_eq!((report.fusions, fast), ([0; 4], 0));
            // An ensure that refused ran no window, so the census records
            // nothing — not a decline, which is a window that ran as rows.
            assert_eq!(report.windows.run(Pattern::PushWords), 0);
            assert_eq!(report.growths, [0; 2], "nothing was reallocated");
        }

        /// **A byte push**: into room, into a full store, of a value that is not a
        /// byte — refused at the store, with the count and the store already in
        /// the frame — and onto a consumed buffer.
        #[test]
        fn a_fused_byte_push_is_the_rows_it_stands_for() {
            let f = framed();
            for (len, capacity, value, consumed) in [
                (3u64, 16i64, 0x41u64, false),
                (16, 16, 0x42, false),
                (0, 16, 256, false),
                (0, 16, (-1i64) as u64, false),
                (0, 16, 0x43, true),
            ] {
                let what = format!("a byte {value} at {len} of {capacity}, consumed: {consumed}");
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = machine.alloc_buffer(capacity).unwrap();
                    let store = machine.payload(owner, runs::GROWABLE_STORE);
                    machine.write_bytes(store, &vec![b'.'; len as usize]);
                    machine.set_payload(owner, runs::GROWABLE_LEN, len);
                    if consumed {
                        machine.set_payload(owner, runs::GROWABLE_STORE, 0);
                    }
                    vec![owner, value]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| {
                    let store = machine.payload(args[0], runs::GROWABLE_STORE);
                    let bytes = match store {
                        0 => Vec::new(),
                        _ => machine.string_bytes(store),
                    };
                    (machine.payload(args[0], runs::GROWABLE_LEN), bytes)
                };
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.push_byte,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                // Room for the byte, and a byte to store: the fast path's case.
                assert_eq!(
                    fast,
                    u64::from(value <= 255 && !consumed && len < capacity as u64),
                    "{what}"
                );
                match (value <= 255, consumed) {
                    (true, false) => {
                        assert_eq!(report.fusions, [0, 1, 0, 0], "{what}");
                        assert_eq!(report.encoded_dispatches, 2, "{what}");
                        assert_eq!(fused.heap.0, len + 1);
                    }
                    (false, _) => {
                        assert!(fused.said.contains("a byte is 0 to 255"), "{}", fused.said);
                        assert!(fused.said.contains("start: 4"), "{what}: at the store");
                        assert_eq!(report.fusions[PUSH_BYTE], 0);
                    }
                    (true, true) => {
                        assert!(fused.said.contains("already consumed"), "{}", fused.said);
                        assert!(fused.said.contains("start: 2"), "{what}: at the ensure");
                    }
                }
            }
        }

        /// **An append of bytes and of elements**: one that fits, one that
        /// grows, an empty one, one longer than a bulk chunk — whose copy
        /// safepoints part way and moves the next question, so the fast path
        /// declines it — and one onto a consumed buffer.
        #[test]
        fn a_fused_append_is_the_rows_it_stands_for() {
            let f = framed();
            let long = "0123456789abcdef".repeat(2_000);
            for (text, consumed) in [
                ("hi", false),
                ("a piece longer than the sixteen bytes of room", false),
                ("", false),
                (long.as_str(), false),
                ("x", true),
            ] {
                let what = format!("an append of {} byte(s), consumed: {consumed}", text.len());
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = machine.alloc_buffer(16).unwrap();
                    machine.push_temp(owner);
                    if consumed {
                        machine.set_payload(owner, runs::GROWABLE_STORE, 0);
                    }
                    let text = machine.new_string(text).unwrap();
                    vec![owner, text]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| {
                    let len = machine.payload(args[0], runs::GROWABLE_LEN);
                    let store = machine.payload(args[0], runs::GROWABLE_STORE);
                    match store {
                        0 => (len, Vec::new()),
                        _ => (len, machine.string_bytes(store)[..len as usize].to_vec()),
                    }
                };
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.append_text,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                // Sixteen bytes of room, and a chunk is 8,192: what runs whole
                // on the fast path is the piece that fits and the empty one.
                assert_eq!(
                    fast,
                    u64::from(!consumed && text.len() <= 16),
                    "{what}: {} byte(s)",
                    text.len()
                );
                // One head ran either way, and the census says what became of
                // it: a consumed buffer is a store it cannot read, a piece
                // longer than a bulk chunk is work one window may not charge,
                // a piece longer than the room is the ordinary way, and the
                // rest is the fast path.
                assert_eq!(report.windows.run(Pattern::AppendBytes), 1, "{what}");
                let declined = &report.windows.declined[APPEND_BYTES];
                if consumed {
                    assert!(fused.said.contains("already consumed"), "{}", fused.said);
                    assert_eq!(declined[Decline::Store.index()], 1, "{what}");
                } else {
                    assert_eq!(fused.heap.1, text.as_bytes(), "{what}");
                    assert_eq!(report.fusions[APPEND_BYTES], 1, "{what}");
                    if text.len() > BULK_CHUNK_BYTES as usize {
                        assert_eq!(declined[Decline::Bulk.index()], 1, "{what}");
                    } else if text.len() <= 16 {
                        assert_eq!(report.windows.fast[APPEND_BYTES], 1, "{what}");
                        assert_eq!(report.growths, [0; 2], "{what}: it fitted");
                    } else {
                        assert_eq!(report.windows.slow[APPEND_BYTES], 1, "{what}");
                        assert_eq!(report.growths, [1, 0], "{what}: a byte store");
                    }
                }
            }
            for capacity in [4i64, 1] {
                let what = format!("an append of pairs into {capacity}");
                let prepare = |machine: &mut Machine<'_>| {
                    let run = machine.allocate(f.pairs, 3).unwrap();
                    for at in 0..6u32 {
                        machine.set_payload(run, at, 100 + u64::from(at));
                    }
                    machine.push_temp(run);
                    let owner = vector_of(machine, f.pair_vector, f.pair_store, 1, capacity, false);
                    vec![owner, run]
                };
                let inspect =
                    |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 2);
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.append_pairs,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                assert_eq!(fused.heap.0, 4, "{what}");
                assert_eq!(&fused.heap.1[2..8], &[100, 101, 102, 103, 104, 105]);
                assert_eq!(report.fusions[APPEND_WORDS], 1, "{what}");
                // Three elements onto one: room in a store of four and not in
                // one of one, which is the growth.
                assert_eq!(fast, u64::from(capacity == 4), "{what}");
                let grew = u64::from(capacity != 4);
                assert_eq!(report.windows.fast[APPEND_WORDS], 1 - grew, "{what}");
                assert_eq!(report.windows.slow[APPEND_WORDS], grew, "{what}");
                assert_eq!(report.growths, [0, grew], "{what}");
                // A length, the window's six rows and a return, in three.
                assert_eq!(
                    (report.encoded_instructions, report.encoded_dispatches),
                    (8, 3),
                    "{what}"
                );
            }
        }

        /// **A word append over a range the frame chooses** — `std.map`'s keyed
        /// extend's shape — is the rows it stands for: into room, an empty one,
        /// one that grows, the whole source, and every way the range can be
        /// wrong. Each refusal is the row's own, at the row's span, with the
        /// frame and the fuel the rows would have left.
        #[test]
        fn a_fused_word_append_over_a_chosen_range_is_the_rows_it_stands_for() {
            let f = framed();
            const SOURCE: u64 = 8;
            for (len, capacity, from, count, consumed) in [
                (2u64, 16i64, 1i64, 3i64, false),
                (2, 16, 0, 0, false),
                (0, 16, 0, 8, false),
                (2, 4, 0, 4, false),
                (2, 16, 5, 10, false),
                (2, 16, -1, 2, false),
                (2, 16, 0, -1, false),
                (2, 16, 0, 2, true),
            ] {
                let what = format!(
                    "{count} element(s) from {from} onto {len} of {capacity}, \
                     consumed: {consumed}"
                );
                let prepare = |machine: &mut Machine<'_>| {
                    let run = machine.allocate(f.ints, SOURCE as i64).unwrap();
                    for at in 0..SOURCE as u32 {
                        machine.set_payload(run, at, 100 + u64::from(at));
                    }
                    machine.push_temp(run);
                    let owner =
                        vector_of(machine, f.int_vector, f.int_store, len, capacity, consumed);
                    let store = machine.payload(owner, runs::GROWABLE_STORE);
                    for at in (0..len as u32).take_while(|_| store != 0) {
                        machine.set_payload(store, at, u64::from(at));
                    }
                    vec![owner, run, from as u64, count as u64]
                };
                let inspect =
                    |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.append_run,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                // What the fast path runs whole: a live store with room for a
                // range the source holds.
                let room = !consumed
                    && (0..=SOURCE as i64).contains(&count)
                    && from >= 0
                    && from + count <= SOURCE as i64
                    && len as i64 + count <= capacity;
                assert_eq!(fast, u64::from(room), "{what}");
                if room {
                    let copied: Vec<u64> =
                        (0..count as u64).map(|at| 100 + from as u64 + at).collect();
                    assert_eq!(fused.heap.0, len + count as u64, "{what}");
                    assert_eq!(
                        &fused.heap.1[len as usize..][..count as usize],
                        copied,
                        "{what}"
                    );
                    assert_eq!(report.fusions[APPEND_WORDS], 1, "{what}");
                } else if consumed {
                    assert!(fused.said.contains("consumed"), "{what}: {}", fused.said);
                    assert!(fused.said.contains("start: 1"), "{what}: at the ensure");
                } else if count < 0 {
                    assert!(
                        fused.said.contains("room is never negative"),
                        "{what}: {}",
                        fused.said
                    );
                    assert!(fused.said.contains("start: 1"), "{what}: at the ensure");
                } else if from < 0 || from + count > SOURCE as i64 {
                    assert!(
                        fused.said.contains("`runCopy` reads"),
                        "{what}: {}",
                        fused.said
                    );
                    assert!(fused.said.contains("start: 3"), "{what}: at the copy");
                } else {
                    // The growth, which the ensure makes and `fused_tail`
                    // finishes: the elements still arrive.
                    assert_eq!(fused.heap.0, len + count as u64, "{what}");
                    assert_eq!(report.fusions[APPEND_WORDS], 1, "{what}");
                }
            }

            // A source that is not a run of these elements at all: the family
            // check the collector's reference maps depend on, refused at the
            // copy in `runCopy`'s words.
            let prepare = |machine: &mut Machine<'_>| {
                let text = machine.new_string("not a run of Int").unwrap();
                machine.push_temp(text);
                let owner = vector_of(machine, f.int_vector, f.int_store, 0, 4, false);
                vec![owner, text, 0, 1]
            };
            let inspect = |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
            let (fused, report, fast) = fuses_as_unfused(
                "a copy out of a String",
                &f.program,
                f.append_run,
                1 << 16,
                None,
                &prepare,
                &inspect,
            );
            assert!(
                fused.said.contains("is not a run of `int` elements"),
                "{}",
                fused.said
            );
            assert!(
                fused.said.contains("start: 3"),
                "at the copy: {}",
                fused.said
            );
            assert_eq!((report.fusions, fast), ([0; 4], 0));
        }

        /// **A word copy whose source is the destination's own store** is a
        /// move and not a smear: shifted up it is walked from the tail, shifted
        /// down from the front, and a range onto itself changes nothing. The
        /// fused run is the rows', which go through `Space::copy`.
        #[test]
        fn a_fused_word_append_that_overlaps_itself_moves_rather_than_smears() {
            let f = framed();
            const CAPACITY: i64 = 16;
            for (len, from, count) in [(2u64, 0i64, 4i64), (0, 2, 4), (4, 4, 4), (6, 0, 6)] {
                let what = format!("{count} element(s) from {from} onto {len} of itself");
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = vector_of(machine, f.int_vector, f.int_store, len, CAPACITY, false);
                    let store = machine.payload(owner, runs::GROWABLE_STORE);
                    for at in 0..CAPACITY as u32 {
                        machine.set_payload(store, at, 1000 + u64::from(at));
                    }
                    vec![owner, store, from as u64, count as u64]
                };
                let inspect =
                    |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.append_run,
                    1 << 16,
                    None,
                    &prepare,
                    &inspect,
                );
                // What a `memmove` of the same range leaves, written out here
                // rather than read back off the machine that is under test.
                let mut want: Vec<u64> = (0..CAPACITY as u64).map(|at| 1000 + at).collect();
                want.copy_within(from as usize..(from + count) as usize, len as usize);
                assert_eq!(fused.heap, (len + count as u64, want), "{what}");
                assert_eq!((report.fusions[APPEND_WORDS], fast), (1, 1), "{what}");
            }
        }

        /// **A word append longer than a bulk chunk is left to the rows**, whose
        /// copy polls between chunks: the fast path may not, because a
        /// safepoint inside a window is a question falling where a fused head
        /// promised none would. The answer is still the rows'.
        #[test]
        fn a_word_append_longer_than_a_chunk_is_left_to_the_rows() {
            let f = framed();
            const COUNT: u64 = 2_000;
            let prepare = |machine: &mut Machine<'_>| {
                let run = machine.allocate(f.ints, COUNT as i64).unwrap();
                for at in 0..COUNT as u32 {
                    machine.set_payload(run, at, u64::from(at));
                }
                machine.push_temp(run);
                let owner = vector_of(machine, f.int_vector, f.int_store, 0, COUNT as i64, false);
                vec![owner, run, 0, COUNT]
            };
            let inspect = |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
            let (fused, report, fast) = fuses_as_unfused(
                "two thousand elements, which is two bulk chunks",
                &f.program,
                f.append_run,
                1 << 16,
                None,
                &prepare,
                &inspect,
            );
            assert_eq!(fused.heap.0, COUNT);
            assert_eq!(fused.heap.1, (0..COUNT).collect::<Vec<_>>());
            // It still fuses — the rows run in the head's one dispatch — and it
            // still does not run on the fast path.
            assert_eq!((report.fusions[APPEND_WORDS], fast), (1, 0));
            // And the census says *why* it did not: two thousand elements is
            // more than one window may charge at once.
            assert_eq!(
                report.windows.declined[APPEND_WORDS][Decline::Bulk.index()],
                1
            );
        }

        /// **A word append of references**: the elements copied are addresses
        /// the collector follows, so a growth that collects inside the window
        /// must leave every one of them readable, and so must a copy the fast
        /// path runs whole. Both are the rows'.
        #[test]
        fn a_fused_word_append_of_references_keeps_every_one() {
            let f = framed();
            // The window copies 32 of them, which is what makes the growth
            // below big enough that the heap has to collect to satisfy it.
            let pieces: Vec<String> = (0..33).map(|at| format!("piece {at}")).collect();
            // The window copies `pieces[1..]`, which is its offset constant.
            let copied = &pieces[1..];
            let held = ["already here", "and here"];
            // `(capacity, heap, garbage)`: room with no growth, a growth, and a
            // growth on a heap small enough that it collects first.
            for (capacity, heap, garbage) in [
                (64i64, 1 << 16, false),
                (2, 1 << 16, false),
                (2, 1 << 10, true),
            ] {
                let what = format!("a store of {capacity} on a heap of {heap}, garbage: {garbage}");
                let prepare = |machine: &mut Machine<'_>| {
                    let run = machine.allocate(f.texts, pieces.len() as i64).unwrap();
                    machine.push_temp(run);
                    for (at, piece) in pieces.iter().enumerate() {
                        let text = machine.new_string(piece).unwrap();
                        machine.set_payload(run, at as u32, text);
                    }
                    // One more than the 32 the window copies from offset 1.
                    assert_eq!(pieces.len(), 33);
                    let owner = vector_of(
                        machine,
                        f.text_vector,
                        f.text_store,
                        held.len() as u64,
                        capacity,
                        false,
                    );
                    machine.push_temp(owner);
                    let store = machine.payload(owner, runs::GROWABLE_STORE);
                    for (at, piece) in held.iter().enumerate() {
                        let text = machine.new_string(piece).unwrap();
                        machine.set_payload(store, at as u32, text);
                    }
                    if garbage {
                        // Up to within a few words of the whole heap, so that
                        // the store the growth asks for does not fit until the
                        // garbage has been collected.
                        while machine.new_string("garbage, and nothing holds it").is_ok()
                            && machine.mem.heap_words() < 1_000
                        {}
                    }
                    vec![owner, run]
                };
                // Every element as the text it points at: a reference the copy
                // lost or a collection freed would not read back.
                let inspect = |machine: &Machine<'_>, args: &[u64]| {
                    let len = machine.payload(args[0], runs::GROWABLE_LEN);
                    let store = machine.payload(args[0], runs::GROWABLE_STORE);
                    let held: Vec<String> = (0..len as u32)
                        .map(|at| {
                            let text = machine.payload(store, at);
                            String::from_utf8(machine.string_bytes(text)).expect("text")
                        })
                        .collect();
                    (len, held)
                };
                let (fused, report, fast) = fuses_as_unfused(
                    &what,
                    &f.program,
                    f.append_texts,
                    heap,
                    None,
                    &prepare,
                    &inspect,
                );
                let want: Vec<String> = held
                    .iter()
                    .map(|piece| (*piece).to_owned())
                    .chain(copied.iter().cloned())
                    .collect();
                assert_eq!(fused.heap, (2 + copied.len() as u64, want), "{what}");
                assert_eq!(report.fusions[APPEND_WORDS], 1, "{what}");
                assert_eq!(
                    fast,
                    u64::from(capacity >= (held.len() + copied.len()) as i64),
                    "{what}"
                );
                if garbage {
                    assert!(fused.collections > 0, "{what}: the growth collected");
                }
                // The longest window there is: a length, its eight rows and a
                // return, in two dispatches.
                assert_eq!(
                    (report.encoded_instructions, report.encoded_dispatches),
                    (10, 2),
                    "{what}"
                );
            }
        }

        /// **A byte append that has to grow is grown by `fused_append_bytes` itself**,
        /// and the rest of its window runs as `fused_tail` runs it: so one whose
        /// growth collects first, and one the heap has no room for at all, must
        /// still be the rows — the same collections, the same bytes, and the same
        /// refusal at the ensure's span with the same instructions and fuel.
        #[test]
        fn a_fused_append_that_grows_collects_and_refuses_as_the_rows_do() {
            let f = framed();
            let text = "0123456789".repeat(60);
            // A heap of 256 words: a full buffer of sixteen bytes, six hundred
            // bytes to append to it, and garbage enough beside them that the
            // store the growth asks for does not fit until it collects.
            let collects = |machine: &mut Machine<'_>| {
                let owner = machine.alloc_buffer(16).unwrap();
                machine.push_temp(owner);
                let store = machine.payload(owner, runs::GROWABLE_STORE);
                machine.write_bytes(store, &[b'.'; 16]);
                machine.set_payload(owner, runs::GROWABLE_LEN, 16);
                let text = machine.new_string(&text).unwrap();
                machine.push_temp(text);
                while machine.new_string("garbage, and nothing holds it").is_ok()
                    && machine.mem.heap_words() < 200
                {}
                vec![owner, text]
            };
            // A heap of 128 words, which holds the text and the buffer and has no
            // room for a store that could hold both.
            let refuses = |machine: &mut Machine<'_>| {
                let owner = machine.alloc_buffer(16).unwrap();
                machine.push_temp(owner);
                let store = machine.payload(owner, runs::GROWABLE_STORE);
                machine.write_bytes(store, &[b'.'; 16]);
                machine.set_payload(owner, runs::GROWABLE_LEN, 16);
                let text = machine.new_string(&text).unwrap();
                vec![owner, text]
            };
            let inspect = |machine: &Machine<'_>, args: &[u64]| {
                let len = machine.payload(args[0], runs::GROWABLE_LEN);
                let store = machine.payload(args[0], runs::GROWABLE_STORE);
                (len, machine.string_bytes(store)[..len as usize].to_vec())
            };
            let (fused, report, fast) = fuses_as_unfused(
                "an append whose growth collects",
                &f.program,
                f.append_text,
                256,
                None,
                &collects,
                &inspect,
            );
            assert!(fused.collections > 0, "the growth collected: {fused:?}");
            assert_eq!(fused.heap.0, 616);
            assert_eq!(&fused.heap.1[16..], text.as_bytes());
            assert_eq!(report.fusions[APPEND_BYTES], 1);
            assert_eq!(fast, 0, "a growth is not the fast path");
            // The length, the window's seven rows and the return, in three.
            assert_eq!(
                (report.encoded_instructions, report.encoded_dispatches),
                (9, 3)
            );

            let (fused, report, fast) = fuses_as_unfused(
                "an append whose growth has no room anywhere",
                &f.program,
                f.append_text,
                128,
                None,
                &refuses,
                &inspect,
            );
            assert!(fused.said.contains("no memory left"), "{}", fused.said);
            assert!(
                fused.said.contains("start: 2"),
                "at the ensure: {}",
                fused.said
            );
            assert_eq!(fused.heap.0, 16, "nothing was published");
            assert_eq!((report.fusions, fast), ([0; 4], 0));
        }

        /// Three hundred byte windows in one function, with garbage between them
        /// on a heap small enough that growing the store collects: **the fused run
        /// is the unfused one** in its answer, its heap, its fuel and its
        /// collections — and under fuel limits that stop it part way, in the span
        /// it stops at. Some windows straddle a safepoint and run unfused, which is
        /// the arm declining; the rest fuse, through a growth or not.
        #[test]
        fn windows_that_grow_collect_and_straddle_safepoints_agree_with_their_rows() {
            const WINDOWS: i64 = 300;
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            let str_layout = build.string_layout();
            let bytes = build.bytes_layout();
            let owner = build.buffer_layout();
            let packed = Storage::PackedBytes;
            // s0 capacity, s1 owner, s2 value, s3 garbage, s4 length, s5 count,
            // s6 store.
            let mut code = vec![
                Inst::Int { dst: 0, value: 0 },
                Inst::GrowableAlloc {
                    dst: 1,
                    capacity: 0,
                    storage: packed,
                },
                Inst::Int {
                    dst: 2,
                    value: i64::from(b'y'),
                },
                Inst::Int { dst: 0, value: 512 },
            ];
            for at in 0..WINDOWS {
                code.extend([
                    Inst::LoadField {
                        dst: 4,
                        obj: 1,
                        at: 0,
                        layout: int,
                    },
                    Inst::Int { dst: 5, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 1,
                        additional: 5,
                        storage: packed,
                    },
                    Inst::LoadField {
                        dst: 6,
                        obj: 1,
                        at: 1,
                        layout: bytes,
                    },
                    Inst::RunStore {
                        run: 6,
                        index: 4,
                        src: 2,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 6,
                        layout: bytes,
                    },
                    Inst::GrowableCommit {
                        owner: 1,
                        count: 5,
                        storage: packed,
                    },
                ]);
                if at % 8 == 0 {
                    code.push(Inst::GrowableAlloc {
                        dst: 3,
                        capacity: 0,
                        storage: packed,
                    });
                    code.push(Inst::Clear {
                        slot: 3,
                        layout: owner,
                    });
                }
            }
            code.push(Inst::RunFinish {
                dst: 1,
                owner: 1,
                target: str_layout,
                validation: Validation::Utf8,
                storage: packed,
            });
            code.push(Inst::Return { src: 1 });
            let spans = code.len();
            let entry = build.function(
                "grow_in_windows",
                &[],
                &[
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                ],
                str_layout,
                code,
            );
            build.program.functions[entry.index()].spans = (0..spans)
                .map(|pc| Span::new(cove_diag::FileId(0), pc as _, pc as _))
                .collect();
            let program = build.done();

            let prepare = |_: &mut Machine<'_>| Vec::new();
            let inspect = |_: &Machine<'_>, _: &[u64]| ();
            let (fused, report, _) =
                fuses_as_unfused("whole", &program, entry, 320, None, &prepare, &inspect);
            assert!(fused.said.starts_with("Ok("), "{}", fused.said);
            assert!(fused.collections > 0, "a growth collected");
            let windows = report.fusions[PUSH_BYTE];
            assert!(
                windows > 0 && windows < WINDOWS as u64,
                "{windows} of {WINDOWS} windows fused, and some straddle a safepoint"
            );
            // Every head is counted once, whatever became of it — which is
            // what makes the census a census — and the ones that did not fuse
            // are the ones a safepoint fell inside.
            assert_eq!(report.windows.run(Pattern::PushByte), WINDOWS as u64);
            let declined = report.windows.declined[PUSH_BYTE];
            assert_eq!(
                declined[Decline::Safepoint.index()],
                WINDOWS as u64 - windows,
                "a window that did not fuse here is one a safepoint reached"
            );
            assert!(report.windows.fast[PUSH_BYTE] > 0, "and some ran whole");
            assert!(report.growths[0] > 0, "a byte store grew");
            assert_eq!(
                report.encoded_dispatches,
                report.encoded_instructions - 6 * windows
            );
            for limit in [1u64, 700, 1_000, 1_500, 2_000] {
                let (stopped, ..) = fuses_as_unfused(
                    &format!("under {limit} fuel"),
                    &program,
                    entry,
                    320,
                    Some(limit),
                    &prepare,
                    &inspect,
                );
                assert!(stopped.said.contains("fuel"), "{}", stopped.said);
            }
        }

        /// The same thing for **word** appends: a hundred and seventy of them in
        /// one function, three elements a window, with garbage between them on a
        /// heap small enough that growing the store collects. The fused run is
        /// the unfused one in its answer, its heap, its fuel and its
        /// collections, and so it is under fuel limits that stop it part way.
        ///
        /// It is where both halves of a word window are reached by one program:
        /// the fast path for the copies the store has room for, and
        /// `Machine::ensure_growable` — collecting, at the ensure's pc — for
        /// the ones it does not. The bounds on `fast` below are what say so.
        #[test]
        fn word_windows_that_grow_and_collect_agree_with_their_rows() {
            const WINDOWS: i64 = 170;
            const SOURCE: i64 = 8;
            // Room for the store the last growth asks for and the one it
            // replaces, and not much more: so a growth part way through has to
            // collect the garbage between the windows before it fits.
            const HEAP: usize = 1_300;
            const COUNT: i64 = 3;
            let mut build = Build::default();
            let int = build.scalar(Repr::Int);
            build.string_layout();
            build.bytes_layout();
            let buffer = build.buffer_layout();
            let int_store = build.layout(
                "Store<Int>",
                Shape::Elements {
                    elem: int,
                    growable: true,
                },
            );
            let int_vector = build.layout("Vector<Int>", Shape::Vector { elem: int });
            let ints = build.layout(
                "Array<Int>",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
            let words = Storage::Words(int);
            let copy = build.args(&[(5, int_store), (4, int), (1, ints), (2, int), (3, int)]);
            // s0 owner, s1 run, s2 from, s3 count, s4 length, s5 store,
            // s6 garbage, s7 the garbage's capacity.
            let mut code = vec![
                Inst::Int { dst: 2, value: 0 },
                Inst::Int {
                    dst: 3,
                    value: COUNT,
                },
                Inst::Int { dst: 7, value: 64 },
            ];
            for at in 0..WINDOWS {
                code.extend([
                    Inst::LoadField {
                        dst: 4,
                        obj: 0,
                        at: 0,
                        layout: int,
                    },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 3,
                        storage: words,
                    },
                    Inst::LoadField {
                        dst: 5,
                        obj: 0,
                        at: 1,
                        layout: int_store,
                    },
                    Inst::RunCopy {
                        args: copy,
                        storage: words,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: int_store,
                    },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 3,
                        storage: words,
                    },
                ]);
                if at % 8 == 0 {
                    // Sixty-four bytes nothing holds: enough garbage between
                    // the windows that collecting it is what lets a growth fit.
                    code.push(Inst::GrowableAlloc {
                        dst: 6,
                        capacity: 7,
                        storage: Storage::PackedBytes,
                    });
                    code.push(Inst::Clear {
                        slot: 6,
                        layout: buffer,
                    });
                }
            }
            code.push(Inst::Return { src: 0 });
            let spans = code.len();
            let entry = build.function(
                "append_in_windows",
                &[int_vector, ints],
                &[
                    Repr::Ref,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Ref,
                    Repr::Int,
                ],
                int_vector,
                code,
            );
            build.program.functions[entry.index()].spans = (0..spans)
                .map(|pc| Span::new(cove_diag::FileId(0), pc as _, pc as _))
                .collect();
            let program = build.done();

            let prepare = |machine: &mut Machine<'_>| {
                let run = machine.allocate(ints, SOURCE).unwrap();
                for at in 0..SOURCE as u32 {
                    machine.set_payload(run, at, 100 + u64::from(at));
                }
                machine.push_temp(run);
                let owner = vector_of(machine, int_vector, int_store, 0, 2, false);
                vec![owner, run]
            };
            let inspect = |machine: &Machine<'_>, args: &[u64]| {
                let len = machine.payload(args[0], runs::GROWABLE_LEN);
                let store = machine.payload(args[0], runs::GROWABLE_STORE);
                let held: Vec<u64> = (0..len as u32)
                    .map(|at| machine.payload(store, at))
                    .collect();
                (len, held)
            };
            let (fused, report, fast) =
                fuses_as_unfused("whole", &program, entry, HEAP, None, &prepare, &inspect);
            assert!(fused.said.starts_with("Ok("), "{}", fused.said);
            assert!(fused.collections > 0, "a growth collected");
            let want: Vec<u64> = (0..WINDOWS as u64)
                .flat_map(|_| (0..COUNT as u64).map(|at| 100 + at))
                .collect();
            assert_eq!(fused.heap, ((WINDOWS * COUNT) as u64, want));
            let windows = report.fusions[APPEND_WORDS];
            assert_eq!(windows, WINDOWS as u64, "every window fused");
            // A growth is not the fast path, so `fast` is short of the windows
            // that fused; and it is above nought, which is the whole of what
            // this change adds. A window a safepoint would fall inside declines
            // too — `a_word_append_longer_than_a_chunk_is_left_to_the_rows` is
            // where that one is reached on its own.
            assert!(fast > 0 && fast < windows, "{fast} of {windows} ran whole");
            // The same census, over the word append: every head counted once,
            // and the growths counted where the stores were replaced. `fast`
            // and `slow` do not sum to `windows` — a window the fast path
            // declined still fuses, because `fused_tail` runs its rows through
            // their commit in the head's one dispatch — so what holds is that
            // the three outcomes together are every window there was.
            assert_eq!(report.windows.run(Pattern::AppendWords), WINDOWS as u64);
            assert_eq!(report.windows.fast[APPEND_WORDS], fast);
            assert!(
                report.windows.slow[APPEND_WORDS] > 0,
                "and some finished the ordinary way, through the growth"
            );
            assert!(report.growths[1] > 0, "a word store grew");
            assert_eq!(
                report.encoded_dispatches,
                report.encoded_instructions - 5 * windows
            );
            for limit in [1u64, 500, 1_000] {
                let (stopped, ..) = fuses_as_unfused(
                    &format!("under {limit} fuel"),
                    &program,
                    entry,
                    HEAP,
                    Some(limit),
                    &prepare,
                    &inspect,
                );
                assert!(stopped.said.contains("fuel"), "{limit}: {}", stopped.said);
            }
        }

        /// **A run that did not ask for the census is the run it was.** Every
        /// count the census takes is behind an `Option` test on a path that had
        /// already decided what to do — a `return`, a growth, a window's last
        /// line — so an uncounted run takes the same path, leaves the same heap
        /// and spends the same fuel as a counted one.
        ///
        /// That is `ablate::CENSUS`'s discipline asked of this tier: the fast
        /// paths are the same function bodies whether or not anything is
        /// counting, rather than a copy with a branch in it. Each shape below
        /// reaches a different arm of the census — a window that runs whole,
        /// one that grows, and an append that copies — and none of them may
        /// differ.
        #[test]
        fn a_run_that_did_not_ask_for_the_census_is_the_run_it_was() {
            let f = framed();
            /// The whole of what a run leaves that a count could disturb.
            type Left = (String, (u64, Vec<u64>), u64, u64, u64);
            let pushed = |counted: bool, len: u64, capacity: i64| -> Left {
                let budget = crate::budget::Budget::new(crate::budget::Limits::default());
                let mut machine = Machine::new(&f.program, 1 << 16);
                let owner = vector_of(
                    &mut machine,
                    f.int_vector,
                    f.int_store,
                    len,
                    capacity,
                    false,
                );
                if counted {
                    machine.count_boundary(native::Tiers::default());
                }
                let said = format!(
                    "{:?}",
                    machine.run(f.push_int, &[owner, 42], &budget.meter())
                );
                (
                    said,
                    store_words(&machine, owner, 1),
                    machine.instructions(),
                    budget.fuel_spent(),
                    machine.collected().collections,
                )
            };
            for (len, capacity) in [(1u64, 4i64), (1, 1)] {
                assert_eq!(
                    pushed(true, len, capacity),
                    pushed(false, len, capacity),
                    "a push at {len} of {capacity}"
                );
            }
            let appended = |counted: bool, text: &str| -> Left {
                let budget = crate::budget::Budget::new(crate::budget::Limits::default());
                let mut machine = Machine::new(&f.program, 1 << 16);
                let owner = machine.alloc_buffer(16).unwrap();
                machine.push_temp(owner);
                let src = machine.new_string(text).unwrap();
                if counted {
                    machine.count_boundary(native::Tiers::default());
                }
                let said = format!(
                    "{:?}",
                    machine.run(f.append_text, &[owner, src], &budget.meter())
                );
                let store = machine.payload(owner, runs::GROWABLE_STORE);
                let bytes = machine
                    .string_bytes(store)
                    .into_iter()
                    .map(u64::from)
                    .collect();
                (
                    said,
                    (machine.payload(owner, runs::GROWABLE_LEN), bytes),
                    machine.instructions(),
                    budget.fuel_spent(),
                    machine.collected().collections,
                )
            };
            for text in ["hi", "a piece longer than the sixteen bytes of room"] {
                assert_eq!(
                    appended(true, text),
                    appended(false, text),
                    "an append of {} byte(s)",
                    text.len()
                );
            }
        }

        /// A debugger that writes down the pc of every stop, and halts at one.
        struct Stops {
            seen: std::sync::Mutex<Vec<u32>>,
            halt_at: Option<u32>,
        }

        impl crate::vm::debug::Debugger for Stops {
            fn at(&self, stop: &crate::vm::debug::Stop<'_>) -> crate::vm::debug::Resume {
                self.seen.lock().expect("a lock").push(stop.pc());
                match self.halt_at == Some(stop.pc()) {
                    true => crate::vm::debug::Resume::Halt,
                    false => crate::vm::debug::Resume::Go,
                }
            }
        }

        /// **A debugger sees every row of a window**, because an installed one
        /// makes every instruction a question and a fused arm never runs past a
        /// question: a stop at a tail row happens, and a halt there leaves the
        /// window uncommitted.
        #[test]
        fn a_debugger_stops_at_every_row_of_a_fused_window() {
            let f = framed();
            for halt_at in [None, Some(4u32)] {
                let stops = Stops {
                    seen: std::sync::Mutex::new(Vec::new()),
                    halt_at,
                };
                let mut machine = Machine::new(&f.program, 1 << 16);
                machine.watch(Some(&stops));
                machine.count_boundary(native::Tiers::default());
                let owner = machine.alloc_buffer(16).unwrap();
                let answered = machine.run(f.push_byte, &[owner, 0x41], &budget());
                let seen = stops.seen.lock().expect("a lock").clone();
                let report = machine.boundary(None).expect("counted");
                assert_eq!(report.fusions, [0; 4]);
                assert_eq!(report.encoded_dispatches, report.encoded_instructions);
                match halt_at {
                    None => {
                        assert!(answered.is_ok(), "{answered:?}");
                        // An installed debugger makes `next_check` the next
                        // instruction, so the window declines at the safepoint
                        // test and runs as rows. `fusions` can only say that
                        // nothing fused; the census says which window and why.
                        assert_eq!(report.windows.run(Pattern::PushByte), 1);
                        assert_eq!(
                            report.windows.declined[PUSH_BYTE][Decline::Safepoint.index()],
                            1
                        );
                        assert_eq!(seen, (0..=8).collect::<Vec<u32>>());
                        assert_eq!(machine.payload(owner, runs::GROWABLE_LEN), 1);
                    }
                    Some(pc) => {
                        let error = answered.expect_err("halted");
                        assert_eq!(error.span.map(|span| span.start), Some(pc as _));
                        assert_eq!(seen, (0..=pc).collect::<Vec<u32>>());
                        assert_eq!(machine.payload(owner, runs::GROWABLE_LEN), 0);
                    }
                }
            }
        }

        /// The same windows with the template compiler's table installed: every
        /// case above that a program reaches through a `call` answers what the
        /// dispatch loop answers — the words, the bytes, and each refusal's
        /// sentence and span.
        #[cfg(feature = "template")]
        mod compiled {
            use super::super::compiled::{agree, compiled, through_a_call};
            use super::*;
            use crate::vm::exec::native::Tiered;

            #[test]
            fn a_compiled_window_agrees_with_the_vm() {
                let w = windows();
                let mut build = Build {
                    program: w.program.clone(),
                };
                let byte_entry = through_a_call(&mut build, w.push_byte, Repr::Ref);
                // `through_a_call` is for one-word parameters, and a `Pair` is two.
                let pair_args = build.args(&[(0, w.pair_vector), (1, w.pair)]);
                let pair_entry = build.function(
                    "outer_pair",
                    &[w.pair_vector, w.pair],
                    &[Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
                    w.pair_vector,
                    vec![
                        Inst::Call {
                            dst: 3,
                            callee: w.push_pair,
                            args: pair_args,
                        },
                        Inst::Return { src: 3 },
                    ],
                );
                let append_entry = through_a_call(&mut build, w.append, Repr::Ref);
                let ensure_entry = through_a_call(&mut build, w.ensure, Repr::Ref);
                let store_entry = through_a_call(&mut build, w.store, Repr::Ref);
                let program = build.done();
                let native = compiled(&program, w.push_byte);
                for inner in [w.push_pair, w.append, w.ensure, w.store] {
                    assert!(native.entry(inner).is_some(), "{:?}", native.refusals());
                }

                // A byte push into a store with room, one that has to grow, and a
                // value that is not a byte.
                for (len, capacity, value) in [(3u64, 16i64, 0x41u64), (16, 16, 0x42), (0, 16, 300)]
                {
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner = machine.alloc_buffer(capacity).unwrap();
                        let store = machine.payload(owner, runs::GROWABLE_STORE);
                        machine.write_bytes(store, &vec![b'.'; len as usize]);
                        machine.set_payload(owner, runs::GROWABLE_LEN, len);
                        vec![owner, value]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        (
                            machine.payload(args[0], runs::GROWABLE_LEN),
                            machine.string_bytes(store),
                        )
                    };
                    let (said, _) = agree(
                        &format!("a byte window at {len} of {capacity}, value {value}"),
                        &program,
                        &native,
                        byte_entry,
                        &prepare,
                        &inspect,
                    );
                    assert_eq!(said.is_ok(), value <= 255, "{said:?}");
                }

                // A pair push with room and without.
                for capacity in [4i64, 1] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let store = machine.allocate(w.pair_store, capacity).unwrap();
                        machine.push_temp(store);
                        let owner = machine.allocate(w.pair_vector, 0).unwrap();
                        machine.set_payload(owner, runs::GROWABLE_STORE, store);
                        machine.set_payload(owner, runs::GROWABLE_LEN, 1);
                        vec![owner, 7, 8]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        (
                            machine.payload(args[0], runs::GROWABLE_LEN),
                            machine.payload(store, 2),
                            machine.payload(store, 3),
                        )
                    };
                    let (said, _) = agree(
                        &format!("a pair window into a store of {capacity}"),
                        &program,
                        &native,
                        pair_entry,
                        &prepare,
                        &inspect,
                    );
                    assert!(said.is_ok(), "{said:?}");
                }

                // An append that fits and one that grows.
                for piece in ["hi", "a piece longer than the sixteen bytes of room"] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner = machine.alloc_buffer(16).unwrap();
                        machine.push_temp(owner);
                        let text = machine.new_string(piece).unwrap();
                        vec![owner, text]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let len = machine.payload(args[0], runs::GROWABLE_LEN);
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        (len, machine.string_bytes(store)[..len as usize].to_vec())
                    };
                    let (said, _) = agree(
                        &format!("an append window of {piece:?}"),
                        &program,
                        &native,
                        append_entry,
                        &prepare,
                        &inspect,
                    );
                    assert!(said.is_ok(), "{said:?}");
                }

                // A negative room, refused in the same words at the same span.
                let prepare = |machine: &mut Machine<'_>| {
                    let owner = machine.alloc_buffer(16).unwrap();
                    vec![owner, (-5i64) as u64]
                };
                let inspect = |_: &Machine<'_>, _: &[u64]| ();
                let (said, ()) = agree(
                    "a negative ensure",
                    &program,
                    &native,
                    ensure_entry,
                    &prepare,
                    &inspect,
                );
                assert!(said.is_err());

                // A byte store that is not a byte, and one past the run.
                for (at, value) in [(0u64, 256u64), (16, 1), (3, 0x7A)] {
                    let prepare = |machine: &mut Machine<'_>| {
                        let run = machine.allocate(machine.program.bytes_layout, 16).unwrap();
                        vec![run, at, value]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| machine.string_bytes(args[0]);
                    let (said, _) = agree(
                        &format!("a byte store of {value} at {at}"),
                        &program,
                        &native,
                        store_entry,
                        &prepare,
                        &inspect,
                    );
                    assert_eq!(said.is_ok(), at < 16 && value <= 255, "{said:?}");
                }
            }
        }

        /// The framed windows with the template code generator's table
        /// installed: **a window compiled as one fast path is the rows the
        /// dispatch loop runs**, fused and unfused — the same frame answered or
        /// the same refusal in the same words at the same span, the same heap,
        /// the same fuel and the same collections. Each case is reached through a
        /// `call`, which is the crossing, and asserts it crossed.
        #[cfg(feature = "template")]
        mod tiered {
            use super::*;
            use crate::native::NativeProgram;
            use crate::vm::exec::native::Tiered;

            /// `outer(params) = inner(params)`, answering what `inner` answers:
            /// the one `call` is what consults the table.
            fn calling(build: &mut Build, inner: FunctionId) -> FunctionId {
                let held = build.program.function(inner);
                let (params, returns) = (held.params.clone(), held.returns);
                let mut reprs = Vec::new();
                let mut row = Vec::new();
                for layout in &params {
                    row.push((reprs.len() as Slot, *layout));
                    reprs.extend(build.program.layout(*layout).words.iter().copied());
                }
                let args = build.args(&row);
                let dst = reprs.len() as Slot;
                reprs.extend(build.program.layout(returns).words.iter().copied());
                build.function(
                    "outer",
                    &params,
                    &reprs,
                    returns,
                    vec![
                        Inst::Call {
                            dst,
                            callee: inner,
                            args,
                        },
                        Inst::Return { src: dst },
                    ],
                )
            }

            /// Everything one run left that a compiled window could get wrong.
            #[derive(Debug, PartialEq)]
            struct Left<T> {
                /// The answer's words, or the refusal's sentence, span and
                /// outcome.
                said: String,
                heap: T,
                fuel: u64,
                collections: u64,
            }

            /// One run of `entry` on a fresh machine: on the dispatch loop,
            /// fused or not, or with `native` installed. Answers what it left and
            /// how many times the run crossed into compiled code.
            fn left<T>(
                program: &Program,
                native: Option<&NativeProgram>,
                fused: bool,
                entry: FunctionId,
                heap: usize,
                prepare: &dyn Fn(&mut Machine<'_>) -> Vec<u64>,
                inspect: &dyn Fn(&Machine<'_>, &[u64]) -> T,
            ) -> (Left<T>, u64) {
                let budget = crate::budget::Budget::new(crate::budget::Limits::default());
                let mut machine = Machine::new(program, heap);
                if !fused {
                    unfused(&mut machine);
                }
                if let Some(native) = native {
                    // Safety: `native` outlives this machine.
                    unsafe { machine.install_native(native) };
                }
                let args = prepare(&mut machine);
                let said = machine
                    .run(entry, &args, &budget.meter())
                    .map_err(|error| (error.message, error.span, error.outcome));
                let left = Left {
                    said: format!("{said:?}"),
                    heap: inspect(&machine, &args),
                    fuel: budget.fuel_spent(),
                    collections: machine.collected().collections,
                };
                (left, machine.tiers().vm_to_native)
            }

            /// [`left`] three times — compiled, fused and unfused — asserting
            /// all three are one run and that the compiled one crossed.
            fn compiled_as_rows<T: PartialEq + std::fmt::Debug>(
                what: &str,
                program: &Program,
                native: &NativeProgram,
                entry: FunctionId,
                heap: usize,
                prepare: &dyn Fn(&mut Machine<'_>) -> Vec<u64>,
                inspect: &dyn Fn(&Machine<'_>, &[u64]) -> T,
            ) -> Left<T> {
                let (compiled, crossed) =
                    left(program, Some(native), true, entry, heap, prepare, inspect);
                let (fused, _) = left(program, None, true, entry, heap, prepare, inspect);
                let (mut rows, _) = left(program, None, false, entry, heap, prepare, inspect);
                assert_eq!(fused, rows, "{what}: fused and unfused");
                // Compiled code charges a block's rows when it enters the block,
                // so a run refused part way through one has paid for rows the
                // dispatch loop never reached — every row, not only a window's.
                // A run that finished has paid for exactly what ran.
                if !compiled.said.starts_with("Ok(") {
                    assert!(compiled.fuel >= rows.fuel, "{what}: {compiled:?}");
                    rows.fuel = compiled.fuel;
                }
                assert_eq!(compiled, rows, "{what}: compiled and the rows");
                assert!(crossed >= 1, "{what}: the window ran in compiled code");
                compiled
            }

            /// Every framed fixture, each behind a caller, and compiled.
            fn tiered() -> (Framed, [FunctionId; 7], NativeProgram) {
                let f = framed();
                let mut build = Build {
                    program: f.program.clone(),
                };
                let inners = [
                    f.push_int,
                    f.push_pair,
                    f.push_byte,
                    f.append_text,
                    f.append_pairs,
                    f.append_run,
                    f.append_texts,
                ];
                let outers = inners.map(|inner| calling(&mut build, inner));
                let f = Framed {
                    program: build.done(),
                    ..f
                };
                let native = crate::native::compile(&f.program).expect("this host compiles");
                for inner in inners {
                    assert!(native.entry(inner).is_some(), "{:?}", native.refusals());
                }
                (f, outers, native)
            }

            /// **A word push, compiled**: into room, through a growth, onto a
            /// consumed vector, onto null and onto a `String`; and a two-word
            /// element into room and through a growth.
            #[test]
            fn a_compiled_word_push_is_its_rows() {
                let (f, [push_int, push_pair, ..], native) = tiered();
                for (len, capacity, consumed) in [(1u64, 4i64, false), (1, 1, false), (0, 4, true)]
                {
                    let what = format!("an Int push at {len} of {capacity}, consumed: {consumed}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner =
                            vector_of(machine, f.int_vector, f.int_store, len, capacity, consumed);
                        vec![owner, 42]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        push_int,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    match consumed {
                        true => assert!(left.said.contains("consumed"), "{what}: {}", left.said),
                        false => assert_eq!(left.heap.0, len + 1, "{what}"),
                    }
                }
                for owner in [None, Some(())] {
                    let what = format!("an Int push onto {owner:?}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner = match owner {
                            None => 0,
                            Some(()) => machine.new_string("not a vector at all").unwrap(),
                        };
                        vec![owner, 42]
                    };
                    let inspect = |_: &Machine<'_>, _: &[u64]| ();
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        push_int,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    assert!(left.said.starts_with("Err("), "{what}: {}", left.said);
                }
                for capacity in [4i64, 1] {
                    let what = format!("a Pair push into {capacity}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner =
                            vector_of(machine, f.pair_vector, f.pair_store, 1, capacity, false);
                        vec![owner, 7, 8]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 2);
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        push_pair,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    assert_eq!(left.heap.0, 2, "{what}");
                    assert_eq!(&left.heap.1[2..4], &[7, 8], "{what}");
                }
            }

            /// **A byte push, compiled**: into room, into a full store, of 256
            /// and of -1 — refused at the store's span — and onto a consumed
            /// buffer, refused at the ensure's.
            #[test]
            fn a_compiled_byte_push_is_its_rows() {
                let (f, [_, _, push_byte, ..], native) = tiered();
                for (len, capacity, value, consumed) in [
                    (3u64, 16i64, 0x41u64, false),
                    (16, 16, 0x42, false),
                    (0, 16, 256, false),
                    (16, 16, (-1i64) as u64, false),
                    (0, 16, 0x43, true),
                ] {
                    let what =
                        format!("a byte {value} at {len} of {capacity}, consumed: {consumed}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner = machine.alloc_buffer(capacity).unwrap();
                        let store = machine.payload(owner, runs::GROWABLE_STORE);
                        machine.write_bytes(store, &vec![b'.'; len as usize]);
                        machine.set_payload(owner, runs::GROWABLE_LEN, len);
                        if consumed {
                            machine.set_payload(owner, runs::GROWABLE_STORE, 0);
                        }
                        vec![owner, value]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        let bytes = match store {
                            0 => Vec::new(),
                            _ => machine.string_bytes(store),
                        };
                        (machine.payload(args[0], runs::GROWABLE_LEN), bytes)
                    };
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        push_byte,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    match (value <= 255, consumed) {
                        (true, false) => assert_eq!(left.heap.0, len + 1, "{what}"),
                        (false, _) => {
                            assert!(left.said.contains("a byte is 0 to 255"), "{}", left.said);
                            assert!(left.said.contains("start: 4"), "{what}: at the store");
                        }
                        (true, true) => {
                            assert!(left.said.contains("already consumed"), "{}", left.said);
                            assert!(left.said.contains("start: 2"), "{what}: at the ensure");
                        }
                    }
                }
            }

            /// **An append, compiled**: bytes that fit, grow, are empty, are
            /// longer than a bulk chunk, and go onto a consumed buffer; and
            /// pairs into room and through a growth.
            #[test]
            fn a_compiled_append_is_its_rows() {
                let (f, [_, _, _, append_text, append_pairs, ..], native) = tiered();
                let long = "0123456789abcdef".repeat(2_000);
                for (text, consumed) in [
                    ("hi", false),
                    ("a piece longer than the sixteen bytes of room", false),
                    ("", false),
                    (long.as_str(), false),
                    ("x", true),
                ] {
                    let what = format!("an append of {} byte(s), consumed: {consumed}", text.len());
                    let prepare = |machine: &mut Machine<'_>| {
                        let owner = machine.alloc_buffer(16).unwrap();
                        machine.push_temp(owner);
                        if consumed {
                            machine.set_payload(owner, runs::GROWABLE_STORE, 0);
                        }
                        let text = machine.new_string(text).unwrap();
                        vec![owner, text]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let len = machine.payload(args[0], runs::GROWABLE_LEN);
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        match store {
                            0 => (len, Vec::new()),
                            _ => (len, machine.string_bytes(store)[..len as usize].to_vec()),
                        }
                    };
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        append_text,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    match consumed {
                        true => assert!(left.said.contains("already consumed"), "{}", left.said),
                        false => assert_eq!(left.heap.1, text.as_bytes(), "{what}"),
                    }
                }
                for capacity in [4i64, 1] {
                    let what = format!("an append of pairs into {capacity}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let run = machine.allocate(f.pairs, 3).unwrap();
                        for at in 0..6u32 {
                            machine.set_payload(run, at, 100 + u64::from(at));
                        }
                        machine.push_temp(run);
                        let owner =
                            vector_of(machine, f.pair_vector, f.pair_store, 1, capacity, false);
                        vec![owner, run]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 2);
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        append_pairs,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    assert_eq!(left.heap.0, 4, "{what}");
                    assert_eq!(&left.heap.1[2..8], &[100, 101, 102, 103, 104, 105]);
                }
            }

            /// **A word append over a range the frame chooses, compiled**: into
            /// room, empty, the whole source, through a growth, over a range
            /// the source does not hold, onto a consumed vector, and out of a
            /// source that is not a run of these elements at all. The compiled
            /// tier answers or refuses exactly as the rows do, and it is the
            /// one shape `std.map`'s keyed extend has.
            #[test]
            fn a_compiled_word_append_over_a_chosen_range_is_its_rows() {
                let (f, [.., append_run, _], native) = tiered();
                const SOURCE: i64 = 8;
                for (len, capacity, from, count, consumed) in [
                    (2u64, 16i64, 1i64, 3i64, false),
                    (2, 16, 0, 0, false),
                    (0, 16, 0, 8, false),
                    (2, 4, 0, 4, false),
                    (2, 16, 5, 10, false),
                    (2, 16, 0, -1, false),
                    (2, 16, 0, 2, true),
                ] {
                    let what = format!(
                        "{count} element(s) from {from} onto {len} of {capacity}, \
                         consumed: {consumed}"
                    );
                    let prepare = |machine: &mut Machine<'_>| {
                        let run = machine.allocate(f.ints, SOURCE).unwrap();
                        for at in 0..SOURCE as u32 {
                            machine.set_payload(run, at, 100 + u64::from(at));
                        }
                        machine.push_temp(run);
                        let owner =
                            vector_of(machine, f.int_vector, f.int_store, len, capacity, consumed);
                        vec![owner, run, from as u64, count as u64]
                    };
                    let inspect =
                        |machine: &Machine<'_>, args: &[u64]| store_words(machine, args[0], 1);
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        append_run,
                        1 << 16,
                        &prepare,
                        &inspect,
                    );
                    let ok = !consumed && count >= 0 && from + count <= SOURCE;
                    assert_eq!(left.said.starts_with("Ok("), ok, "{what}: {}", left.said);
                    if ok {
                        assert_eq!(left.heap.0, len + count as u64, "{what}");
                    }
                }
            }

            /// **A word append of references through a growth that collects,
            /// compiled**: the elements are addresses, so a tier that lost one
            /// or a collection that did not see the half-built store reads back
            /// as the wrong text.
            #[test]
            fn a_compiled_word_append_of_references_keeps_every_one() {
                let (f, [.., append_texts], native) = tiered();
                let pieces: Vec<String> = (0..33).map(|at| format!("piece {at}")).collect();
                let copied = pieces[1..].to_vec();
                for (capacity, heap, garbage) in [(64i64, 1 << 16, false), (2, 1 << 10, true)] {
                    let what = format!("a store of {capacity}, garbage: {garbage}");
                    let prepare = |machine: &mut Machine<'_>| {
                        let run = machine.allocate(f.texts, pieces.len() as i64).unwrap();
                        machine.push_temp(run);
                        for (at, piece) in pieces.iter().enumerate() {
                            let text = machine.new_string(piece).unwrap();
                            machine.set_payload(run, at as u32, text);
                        }
                        let owner =
                            vector_of(machine, f.text_vector, f.text_store, 0, capacity, false);
                        machine.push_temp(owner);
                        if garbage {
                            while machine.new_string("garbage, and nothing holds it").is_ok()
                                && machine.mem.heap_words() < 1_000
                            {}
                        }
                        vec![owner, run]
                    };
                    let inspect = |machine: &Machine<'_>, args: &[u64]| {
                        let len = machine.payload(args[0], runs::GROWABLE_LEN);
                        let store = machine.payload(args[0], runs::GROWABLE_STORE);
                        let held: Vec<String> = (0..len as u32)
                            .map(|at| {
                                let text = machine.payload(store, at);
                                String::from_utf8(machine.string_bytes(text)).expect("text")
                            })
                            .collect();
                        (len, held)
                    };
                    let left = compiled_as_rows(
                        &what,
                        &f.program,
                        &native,
                        append_texts,
                        heap,
                        &prepare,
                        &inspect,
                    );
                    assert_eq!(left.heap, (copied.len() as u64, copied.clone()), "{what}");
                    if garbage {
                        assert!(left.collections > 0, "{what}: the growth collected");
                    }
                }
            }

            /// **Pushes of an element that holds a fresh `String`, grown under
            /// collection, compiled.** One function builds each string by a byte
            /// push window and a finish, drops it from its slot, pushes it with
            /// its index through a word push window, and leaves a garbage buffer
            /// behind — on a heap small enough that growing either store collects.
            /// The only thing naming a string is the vector's store, so a growth
            /// that lost a reference, or a collection that did not see the
            /// element, reads back as the wrong bytes.
            #[test]
            fn a_compiled_push_of_a_reference_grows_under_collection_as_its_rows_do() {
                const PUSHES: u64 = 200;
                let mut build = Build::default();
                let int = build.scalar(Repr::Int);
                let str_layout = build.string_layout();
                let bytes = build.bytes_layout();
                let buffer = build.buffer_layout();
                let named = build.structure("Named", &[("n", int), ("s", str_layout)]);
                let named_store = build.layout(
                    "Store<Named>",
                    Shape::Elements {
                        elem: named,
                        growable: true,
                    },
                );
                let named_vector = build.layout("Vector<Named>", Shape::Vector { elem: named });
                let packed = Storage::PackedBytes;
                let words = Storage::Words(named);
                // s0 owner, s1 count, s2 k, s3 k < count, s4 capacity, s5 buffer,
                // s6 its length, s7 one, s8 its store, s9 the byte, s10 one,
                // s11..s12 the element, s13 the vector's length, s14 one, s15 its
                // store, s16 one, s17 garbage.
                let code = vec![
                    Inst::Int { dst: 2, value: 0 },
                    Inst::Int { dst: 4, value: 4 },
                    Inst::Int {
                        dst: 9,
                        value: 0x61,
                    },
                    Inst::Cmp {
                        on: Compare::Int,
                        op: CmpOp::Lt,
                        dst: 3,
                        a: 2,
                        b: 1,
                    },
                    Inst::BranchFalse { cond: 3, to: 30 },
                    Inst::GrowableAlloc {
                        dst: 5,
                        capacity: 4,
                        storage: packed,
                    },
                    Inst::LoadField {
                        dst: 6,
                        obj: 5,
                        at: 0,
                        layout: int,
                    },
                    Inst::Int { dst: 7, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 5,
                        additional: 7,
                        storage: packed,
                    },
                    Inst::LoadField {
                        dst: 8,
                        obj: 5,
                        at: 1,
                        layout: bytes,
                    },
                    Inst::RunStore {
                        run: 8,
                        index: 6,
                        src: 9,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 8,
                        layout: bytes,
                    },
                    Inst::Int { dst: 10, value: 1 },
                    Inst::GrowableCommit {
                        owner: 5,
                        count: 10,
                        storage: packed,
                    },
                    Inst::RunFinish {
                        dst: 12,
                        owner: 5,
                        target: str_layout,
                        validation: Validation::Utf8,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 5,
                        layout: buffer,
                    },
                    Inst::Copy {
                        dst: 11,
                        src: 2,
                        layout: int,
                    },
                    Inst::LoadField {
                        dst: 13,
                        obj: 0,
                        at: 0,
                        layout: int,
                    },
                    Inst::Int { dst: 14, value: 1 },
                    Inst::GrowableEnsure {
                        owner: 0,
                        additional: 14,
                        storage: words,
                    },
                    Inst::LoadField {
                        dst: 15,
                        obj: 0,
                        at: 1,
                        layout: named_store,
                    },
                    Inst::StoreElem {
                        obj: 15,
                        index: 13,
                        src: 11,
                        layout: named,
                    },
                    Inst::Clear {
                        slot: 15,
                        layout: named_store,
                    },
                    Inst::Int { dst: 16, value: 1 },
                    Inst::GrowableCommit {
                        owner: 0,
                        count: 16,
                        storage: words,
                    },
                    Inst::Clear {
                        slot: 12,
                        layout: str_layout,
                    },
                    Inst::GrowableAlloc {
                        dst: 17,
                        capacity: 4,
                        storage: packed,
                    },
                    Inst::Clear {
                        slot: 17,
                        layout: buffer,
                    },
                    Inst::ArithImm {
                        op: ArithOp::Add,
                        dst: 2,
                        a: 2,
                        value: 1,
                    },
                    Inst::Jump { to: 3 },
                    Inst::Return { src: 0 },
                ];
                let spans = code.len();
                let mut reprs = vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Bool, Repr::Int];
                reprs.extend([
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                ]);
                reprs.extend([
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                    Repr::Int,
                    Repr::Ref,
                    Repr::Int,
                ]);
                reprs.push(Repr::Ref);
                let fill = build.function("fill", &[named_vector, int], &reprs, named_vector, code);
                build.program.functions[fill.index()].spans = (0..spans)
                    .map(|pc| Span::new(cove_diag::FileId(0), pc as _, pc as _))
                    .collect();
                let entry = calling(&mut build, fill);
                let program = build.done();
                let found: Vec<_> = cove_ir::legalize::windows(&program, program.function(fill))
                    .iter()
                    .map(|window| window.pattern)
                    .collect();
                assert_eq!(
                    found,
                    [
                        cove_ir::legalize::Pattern::PushByte,
                        cove_ir::legalize::Pattern::PushWords
                    ]
                );
                let native = crate::native::compile(&program).expect("this host compiles");
                assert!(native.entry(fill).is_some(), "{:?}", native.refusals());

                let prepare = |machine: &mut Machine<'_>| {
                    let owner = vector_of(machine, named_vector, named_store, 0, 1, false);
                    vec![owner, PUSHES]
                };
                let inspect = |machine: &Machine<'_>, args: &[u64]| {
                    let len = machine.payload(args[0], runs::GROWABLE_LEN);
                    let store = machine.payload(args[0], runs::GROWABLE_STORE);
                    (0..len as u32)
                        .map(|at| {
                            let text = machine.payload(store, 2 * at + 1);
                            (machine.payload(store, 2 * at), machine.string_bytes(text))
                        })
                        .collect::<Vec<_>>()
                };
                let left = compiled_as_rows(
                    "pushes of fresh strings",
                    &program,
                    &native,
                    entry,
                    3_072,
                    &prepare,
                    &inspect,
                );
                assert!(
                    left.said.starts_with("Ok("),
                    "{}",
                    left.said.lines().next().unwrap_or_default()
                );
                assert!(left.collections > 0, "a growth collected");
                assert_eq!(left.heap.len() as u64, PUSHES);
                for (at, (n, text)) in left.heap.iter().enumerate() {
                    assert_eq!((*n, text.as_slice()), (at as u64, b"a".as_slice()));
                }
            }
        }
    }
}
