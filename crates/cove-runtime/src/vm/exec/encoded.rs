//! The dispatch loop over [`EncodedInst`], and the refusal that keeps it
//! honest.
//!
//! [ADR 0041](../../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! decided the sixteen-byte instruction and `cove_ir::bytecode` built the
//! encoder, the decoder, the verifier and the disassembly. Phase 3 ran the
//! fourteen opcodes the `arith` benchmark reaches. This is
//! [issue #245](https://github.com/myuon/cove/issues/245)'s **Phase 4**: every
//! one of the hundred opcodes ADR 0041 defines, so that the complete
//! differential corpus runs here and is compared against the oracle the way
//! `crates/cove-cli/tests/differential.rs` compares the enum path.
//!
//! # Why this is a file of its own
//!
//! `ad5f160` measured something this crate now builds around. Writing the
//! debugger's question inline in [`Machine::dispatch`] cost **4.3% on
//! `arith`** — code that never ran when no debugger was installed — and
//! [`Machine::ask`] is `#[inline(never)]` because of it. The dispatch body's
//! footprint and its branch-target alignment are costs every program pays,
//! whether or not the added code is reached.
//!
//! A second dispatch loop is a great deal more than a `Stop` and an indirect
//! call. So it is not inside the first one, not reachable from inside it, and
//! not in the same function: [`dispatch`] is a free function in a module of
//! its own, and [`Machine::drive`] chooses between the two **once per run**,
//! before either loop starts. There is no per-instruction test anywhere that
//! asks which representation is executing, because there is nothing for such
//! a test to decide.
//!
//! It is a *child* module of [`super`] rather than a sibling, which is what
//! lets it read `Machine`'s private fields without widening them to the
//! crate. A second loop over the same machine is exactly as privileged as the
//! first; making the machine's state `pub(crate)` to allow it would have
//! handed that privilege to everything else as well.
//!
//! # What it covers, and what a refusal looks like
//!
//! Every opcode. [`implemented`] is an *exhaustive* match rather than a list,
//! so an opcode added to [`Op`] is a compile error here rather than a program
//! refused at run time — which is the form the scaffolding takes once the
//! list is complete.
//!
//! [`prepare`] still encodes, verifies, and then walks every instruction of
//! every function before the run begins, and it still **refuses the program**
//! rather than handing it back. Nothing in the corpus reaches that refusal
//! today; it stays because "cannot happen" and "does not exist" are different
//! claims, and because a byte that names no opcode is still a byte a loader
//! could one day produce.
//!
//! There is deliberately **no fallback to [`Machine::dispatch`]**. A quiet
//! hand-back would make the measurement meaningless — a run that reported the
//! encoded path's wall time while executing the enum's — and would make
//! issue #245's Phase 5 unverifiable, since "no silent fallback to enum
//! execution" cannot be checked against a path that silently falls back.
//!
//! # It is the same machine
//!
//! Nothing here is a second implementation of anything a program can
//! observe. The fuel accounting, the safepoint, the debugger question, the
//! collector poll and the span lookup are the lines [`Machine::dispatch`]
//! runs, in the same order; the arithmetic is [`super::int_arith`],
//! [`super::float_arith`] and [`super::compare`], the same functions; a call
//! pushes [`super::Frame`] onto the same stack, a host call is
//! [`Machine::call_host`], a spawn is [`Machine::spawn`], and a scope is left
//! by [`Machine::leave_scope`]. **One encoded instruction is one instruction
//! and one unit of fuel**, exactly as the enum's, which is what makes a
//! `fuel_spent` comparison between the two an equivalence check rather than
//! a coincidence.
//!
//! That sameness is the whole of what Phase 4 had to establish, and it is
//! stronger than agreeing on answers. A `spawn` here must leave the *same*
//! child in `Machine::children`, at the same index, with the same answer
//! object rooted by the same table; a `scope.leave` must join the same
//! threads in the same order; a failure must leave the frames, the cells and
//! the scopes in the state a host that catches it will find. None of that is
//! re-decided here — every one of those is a call into the method
//! [`Machine::dispatch`] calls, with the operands read out of sixteen bytes
//! instead of out of an enum.
//!
//! Bytecode pc *is* IR pc — ADR 0041's 1:1 encoding — so `Function::spans`
//! is indexed by the same number and a failure points at the same place
//! through both paths without a remapping.

use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};

use cove_diag::Span;

use cove_ir::bytecode::{disasm, encode_program, verify, Encoded, EncodedInst, Op};
use cove_ir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Convert, FunctionId, HostOpId, LayoutId, Num,
    Program, Repr, Slot, StrId, TableId,
};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::interp::stopped_here;
use crate::vm::cell;
use crate::vm::mem::Overflow;

use super::{
    compare, float_arith, int_arith, null_object, overflowed, reentrant_lock, wrong_arity,
    ChildState, Frame, Live, Machine, Outcome, ScopeEntry, SAFEPOINT_STRIDE,
};

// The opcodes this path runs, by the name ADR 0041 gives them rather than by
// number. `Op::number` is a `const fn` so that these are `match` patterns:
// the numbers are positions in a generated table and move when the table
// does, and nothing here should have to move with them.
const CONST_UNIT: u8 = Op::ConstUnit.number();
const CONST_BOOL: u8 = Op::ConstBool.number();
const CONST_INT: u8 = Op::ConstInt.number();
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
/// Exhaustive rather than a `matches!` list, and that is the whole of what
/// Phase 4 changed about it. While opcodes were missing, a list was the work
/// list; now that none are, the only useful thing this can be is a *proof
/// obligation* — a new [`Op`] fails to compile here until someone decides
/// what the loop does with it, instead of becoming a program refused at run
/// time. The answer is a constant, so the walk [`prepare`] makes is free.
pub(crate) fn implemented(op: Op) -> bool {
    match op {
        Op::ConstUnit
        | Op::ConstBool
        | Op::ConstInt
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

/// `program` in the form [`dispatch`] runs, or why this path will not run it.
///
/// Encode, verify once, then check that every opcode has an implementation —
/// in that order, because the verifier is what establishes the structural
/// facts the loop then trusts, and asking whether an opcode is implemented
/// before knowing it is a real opcode would be asking about a byte.
///
/// Every refusal is raised here, before the run pushes a frame, so a program
/// this path cannot execute has no observable effect at all rather than
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

/// What an opcode this path does not run is refused with.
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
        "The encoded path implements every opcode ADR 0041 defines and never hands a program back to the readable-IR loop, because a silent fallback would make the comparison between the two meaningless.",
    )
    .with_help(format!(
        "run this program on the ordinary path; the instruction is `{}` at `{}` pc {pc}",
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
            .copy_words(callee_base + at as u64, base + arg.slot as u64, width);
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

/// The loop, over encoded instructions.
///
/// `id`, `base`, `pc` and `code` are locals for the same reason
/// [`Machine::dispatch`] keeps them as locals, and are written back to the
/// frame at the same two points: a stop something else reads at, and a
/// failure.
///
/// `threads` and `running` are the thread scope a `spawn` starts children in
/// and the handles onto them, exactly as they are for the enum loop and for
/// the same reason: a scoped handle borrows the scope it was started in, so
/// it cannot be a field of the machine.
///
/// `floor` is [`Machine::dispatch`]'s: the frame depth this turn of the loop
/// was entered at, which a `return` below is what ends it.
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
    let mut pc = top.pc as usize;
    let mut code = encoded.function(id);

    loop {
        machine.instructions += 1;
        // The same one comparison the enum loop makes, answering the same two
        // questions: `next_check` is the smaller of the next safepoint and
        // the next debug stop. Everything inside is that loop's lines in that
        // loop's order, because a second accounting would be a second thing
        // to keep in step with ADR 0024 and ADR 0040.
        if machine.instructions >= machine.next_check {
            machine.sync(pc);
            if machine.debugger.is_some() {
                machine.ask(id, pc)?;
            }
            if machine.instructions.is_multiple_of(SAFEPOINT_STRIDE) {
                stopped_here(
                    machine.cancellation.as_ref(),
                    &machine.stops,
                    machine.span(id, pc),
                )?;
                let gathered = machine.instructions - machine.charged;
                machine.charged = machine.instructions;
                if let Err(stopped) = budget.safepoint(gathered) {
                    return Err(budget.to_runtime_error(stopped).at(machine.span(id, pc)));
                }
                let live = Live(machine);
                machine.mem.poll(&live);
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
        // `float_arith` and `compare` are the enum path's own functions, so
        // nothing can drift, and their inner `match` folds away because the
        // operator is known.
        macro_rules! int_op {
            ($op:expr) => {{
                let x = machine.mem.slot(base, b!()) as i64;
                let y = machine.mem.slot(base, c!()) as i64;
                // The same question `Inst::Arith` asks, for the same reason:
                // which of the two the operands are decides only what a
                // failure calls the operation.
                let duration = machine.repr(id, a!()) == Some(Repr::Duration);
                match int_arith($op, x, y, duration) {
                    Ok(value) => machine.mem.set_slot(base, a!(), value as u64),
                    Err(error) => fail!(error),
                }
            }};
        }
        macro_rules! float_op {
            ($op:expr) => {{
                let x = f64::from_bits(machine.mem.slot(base, b!()));
                let y = f64::from_bits(machine.mem.slot(base, c!()));
                machine
                    .mem
                    .set_slot(base, a!(), float_arith($op, x, y).to_bits());
            }};
        }
        macro_rules! arith_imm {
            ($op:expr) => {{
                let x = machine.mem.slot(base, b!());
                let duration = machine.repr(id, a!()) == Some(Repr::Duration);
                match int_arith($op, x as i64, held.payload() as i64, duration) {
                    Ok(value) => machine.mem.set_slot(base, a!(), value as u64),
                    Err(error) => fail!(error),
                }
            }};
        }
        macro_rules! cmp_imm {
            ($op:expr) => {{
                let x = machine.mem.slot(base, b!()) as i64;
                let answer = compare($op, x.cmp(&(held.payload() as i64)));
                machine.mem.set_slot(base, a!(), answer as u64);
            }};
        }
        macro_rules! cmp_int {
            ($op:expr) => {{
                let x = machine.mem.slot(base, b!()) as i64;
                let y = machine.mem.slot(base, c!()) as i64;
                let answer = compare($op, x.cmp(&y));
                machine.mem.set_slot(base, a!(), answer as u64);
            }};
        }
        macro_rules! cmp_float {
            ($answer:expr) => {{
                let x = f64::from_bits(machine.mem.slot(base, b!()));
                let y = f64::from_bits(machine.mem.slot(base, c!()));
                #[allow(clippy::redundant_closure_call)]
                let answer = ($answer)(x, y);
                machine.mem.set_slot(base, a!(), answer as u64);
            }};
        }
        macro_rules! cmp_str {
            ($op:expr) => {{
                let x = machine.mem.slot(base, b!());
                let y = machine.mem.slot(base, c!());
                let answer = compare($op, machine.compare_strings(x, y));
                machine.mem.set_slot(base, a!(), answer as u64);
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
                let x = machine.mem.slot(base, b!());
                let y = machine.mem.slot(base, c!());
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
                pc = 0;
                code = encoded.function(id);
            }};
        }

        match held.opcode() {
            // ---- constants and moves ---------------------------------
            CONST_UNIT => machine.mem.set_slot(base, a!(), 0),
            CONST_BOOL | CONST_INT | CONST_FLOAT => {
                machine.mem.set_slot(base, a!(), held.payload())
            }
            STR => {
                machine.sync(pc - 1);
                match machine.intern(StrId(held.lo())) {
                    Ok(addr) => machine.mem.set_slot(base, a!(), addr),
                    Err(error) => fail!(error),
                }
            }
            // ADR 0001's field-wise shallow copy, and the whole of it.
            COPY => {
                let width = machine.width(LayoutId(held.lo()));
                machine
                    .mem
                    .copy_words(base + held.a() as u64, base + held.b() as u64, width);
            }
            // The one instruction whose whole purpose is what it stops
            // happening: a reference the frame no longer needs is not a root.
            CLEAR => {
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.clear_words(base + held.a() as u64, width);
            }

            // ---- scalar operations -----------------------------------
            NEG_INT => {
                let x = machine.mem.slot(base, b!()) as i64;
                match x.checked_neg() {
                    Some(value) => machine.mem.set_slot(base, a!(), value as u64),
                    None => fail!(overflowed("negation")),
                }
            }
            NEG_FLOAT => {
                let x = f64::from_bits(machine.mem.slot(base, b!()));
                machine.mem.set_slot(base, a!(), (-x).to_bits());
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

            // Not `compare` over an `Ordering`: a `NaN` is unordered, and
            // `f64`'s own operators are what the enum path uses.
            EQ_FLOAT => cmp_float!(|x, y| x == y),
            NE_FLOAT => cmp_float!(|x: f64, y: f64| x != y),
            LT_FLOAT => cmp_float!(|x, y| x < y),
            LE_FLOAT => cmp_float!(|x, y| x <= y),
            GT_FLOAT => cmp_float!(|x, y| x > y),
            GE_FLOAT => cmp_float!(|x, y| x >= y),

            EQ_BOOL | EQ_REF => cmp_word!(true),
            NE_BOOL | NE_REF => cmp_word!(false),
            LT_BOOL | LE_BOOL | GT_BOOL | GE_BOOL | LT_REF | LE_REF | GT_REF | GE_REF => {
                not_ordered!()
            }

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
                let x = machine.mem.slot(base, b!());
                machine.mem.set_slot(base, a!(), (x == 0) as u64);
            }
            INT_TO_FLOAT => {
                let x = machine.mem.slot(base, b!()) as i64;
                machine.mem.set_slot(base, a!(), (x as f64).to_bits());
            }
            FLOAT_TO_INT => {
                let x = f64::from_bits(machine.mem.slot(base, b!()));
                machine.mem.set_slot(base, a!(), x as i64 as u64);
            }

            // ---- control flow ----------------------------------------
            // The displacement is `to - (pc + 1)` and `pc` is already past
            // the instruction, so this is one addition and no table.
            JUMP => pc = pc.wrapping_add_signed(held.payload() as i64 as isize),
            BRANCH_FALSE => {
                if machine.mem.slot(base, a!()) == 0 {
                    pc = pc.wrapping_add_signed(held.payload() as i64 as isize);
                }
            }
            // A switch table stays immutable program metadata with absolute
            // targets — ADR 0041's one exception to relative control flow,
            // because a table read from a `TableId` has no pc of its own.
            SWITCH => {
                let index = machine.mem.slot(base, a!()) as usize;
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
                let object = machine.mem.slot(base, b!());
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
                            machine.mem.set_slot(base, dst + at as u32, *word);
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
                            machine.mem.set_slot(base, dst + at as u32, *word);
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
                match machine.call_builtin(base, BuiltinId(held.lo()), ArgsId(held.hi())) {
                    Ok(words) => {
                        for (at, word) in words.iter().enumerate() {
                            machine.mem.set_slot(base, dst + at as u32, *word);
                        }
                    }
                    Err(error) => fail!(error),
                }
            }

            // ---- the heap --------------------------------------------
            // `Len`'s three forms are three opcodes rather than a
            // discriminant in a field, so nothing is stored and nothing is
            // asked.
            ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT => {
                let len = match held.opcode() {
                    ALLOC_FIXED => 0,
                    ALLOC_IMM => held.hi(),
                    _ => machine.mem.slot(base, b!()) as u32,
                };
                machine.sync(pc - 1);
                match machine.allocate(LayoutId(held.lo()), len) {
                    Ok(addr) => machine.mem.set_slot(base, a!(), addr),
                    Err(error) => fail!(error),
                }
            }
            // A field of a *heap object* is a run of words at a static
            // offset. A field of an inline struct is not here at all: it is a
            // slot number the lowering computed.
            LOAD_FIELD => {
                let addr = machine.mem.slot(base, b!());
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
                let addr = machine.mem.slot(base, a!());
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
                let addr = machine.mem.slot(base, b!());
                let index = machine.mem.slot(base, c!()) as i64;
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
                let addr = machine.mem.slot(base, a!());
                let index = machine.mem.slot(base, b!()) as i64;
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
            LEN => {
                let addr = machine.mem.slot(base, b!());
                if addr == 0 {
                    fail!(null_object());
                }
                let len = machine.mem.object_len(addr) as i64;
                machine.mem.set_slot(base, a!(), len as u64);
            }
            // The other half of the header word `len` reads. What an object
            // *is* is an `Int` here, so a dispatch over it is an ordinary
            // `switch`.
            LAYOUT_OF => {
                let addr = machine.mem.slot(base, b!());
                if addr == 0 {
                    fail!(null_object());
                }
                let layout = machine.mem.object_layout(addr).0 as i64;
                machine.mem.set_slot(base, a!(), layout as u64);
            }

            // ---- places ----------------------------------------------
            ADDR_OF_SLOT => {
                let word = base + held.b() as u64;
                machine.mem.set_slot(base, a!(), word);
            }
            ADDR_OF_FIELD => {
                let addr = machine.mem.slot(base, b!());
                let at = held.lo();
                match machine.checked(addr, at, 1) {
                    Ok(()) => {
                        let word = machine.mem.payload_addr(addr, at);
                        machine.mem.set_slot(base, a!(), word);
                    }
                    Err(error) => fail!(error),
                }
            }
            ADDR_OF_ELEM => {
                let addr = machine.mem.slot(base, b!());
                let index = machine.mem.slot(base, c!()) as i64;
                let width = machine.width(LayoutId(held.lo()));
                match machine.element(addr, index, width) {
                    Ok(at) => {
                        let word = machine.mem.payload_addr(addr, at);
                        machine.mem.set_slot(base, a!(), word);
                    }
                    Err(error) => fail!(error),
                }
            }
            // Arithmetic and nothing else: what an address names is a value
            // location, and a value location's parts are at static offsets
            // from its first word.
            ADDR_OF_PART => {
                let word = machine.mem.slot(base, b!());
                machine.mem.set_slot(base, a!(), word + held.lo() as u64);
            }
            LOAD => {
                let addr = machine.mem.slot(base, b!());
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.copy_words(base + held.a() as u64, addr, width);
            }
            STORE => {
                let addr = machine.mem.slot(base, a!());
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
                let boxed = match machine.allocate(machine.boxed_layout(), width) {
                    Ok(addr) => addr,
                    Err(error) => fail!(error),
                };
                machine.mem.set_payload(boxed, 0, layout.0 as u64);
                machine.mem.copy_words(
                    machine.mem.payload_addr(boxed, 1),
                    base + held.b() as u64,
                    width,
                );
                machine.mem.set_slot(base, a!(), boxed);
            }
            UNBOX => {
                let layout = LayoutId(held.lo());
                let addr = machine.mem.slot(base, b!());
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
                machine.mem.set_slot(base, a!(), word);
            }
            // The body reached its end, so this is the exit that waits. What
            // it answers about a failing child is a value here rather than
            // control flow.
            SCOPE_LEAVE => {
                machine.sync(pc - 1);
                let span = machine.span(id, pc - 1);
                let word = machine.mem.slot(base, a!());
                match machine.leave_scope(word, running, span) {
                    Ok(None) => machine.mem.set_slot(base, b!(), 0),
                    Ok(Some(child)) => {
                        let into = base + held.c() as u64;
                        match machine.write_child_error(child, into, LayoutId(held.lo())) {
                            Ok(()) => machine.mem.set_slot(base, b!(), 1),
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
                let word = machine.mem.slot(base, a!());
                match machine.scope_at(word, machine.span(id, pc - 1)) {
                    Ok(at) => machine.cancel_scope(at, running),
                    Err(error) => fail!(error),
                }
            }
            SPAWN => {
                machine.sync(pc - 1);
                let span = machine.span(id, pc - 1);
                let scope_word = machine.mem.slot(base, b!());
                let object = machine.mem.slot(base, c!());
                match machine.spawn(
                    scope_word,
                    object,
                    LayoutId(held.lo()),
                    budget,
                    span,
                    threads,
                    running,
                ) {
                    Ok(word) => machine.mem.set_slot(base, a!(), word),
                    Err(error) => fail!(error),
                }
            }
            AWAIT => {
                machine.sync(pc - 1);
                let dst = a!();
                let span = machine.span(id, pc - 1);
                let word = machine.mem.slot(base, b!());
                match machine.settle(word, LayoutId(held.lo()), running, span) {
                    Ok(words) => {
                        for (at, one) in words.iter().enumerate() {
                            machine.mem.set_slot(base, dst + at as u32, *one);
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
                    Ok(word) => machine.mem.set_slot(base, a!(), word),
                    Err(error) => fail!(error),
                }
            }
            // Asking is all it does. Whether the task stopped or had already
            // finished is known only where something waits for it.
            CANCEL => {
                let word = machine.mem.slot(base, a!());
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
                let addr = machine.mem.slot(base, a!());
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
                let addr = machine.mem.slot(base, a!());
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
                let addr = machine.mem.slot(base, a!());
                let text = String::from_utf8_lossy(&machine.string_bytes(addr)).into_owned();
                machine.assertion_failure = Some((machine.span(id, pc - 1), text));
            }

            // [`prepare`] refused every one of these before the run began, so
            // this cannot happen. It is written out because a loop that
            // trusted its own precondition silently would be a loop that
            // executed the wrong instruction when the precondition was one
            // day widened and this was not.
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

    use super::super::tests::{budget, Build};
    use super::*;

    /// Runs a hand-written program on the encoded path.
    ///
    /// `Machine::run` reaches `Machine::drive`, which chooses the loop, so
    /// this is `super::tests::run_words` with one setter added rather than a
    /// third way of starting a run.
    fn run_encoded(
        program: &Program,
        entry: FunctionId,
        args: &[u64],
    ) -> Result<Vec<u64>, RuntimeError> {
        let code = prepare(program)?;
        let mut machine = Machine::new(program, 1 << 16);
        machine.execute_encoded(code);
        machine.run(entry, args, &budget())
    }

    /// Every opcode ADR 0041 defines has an implementation.
    ///
    /// The completion condition of issue #245's Phase 4, as one assertion.
    /// [`implemented`] is exhaustive, so a new opcode cannot compile without
    /// an answer here — but "the author wrote `true`" and "the loop has an
    /// arm" are different claims, and the second is what
    /// `crates/cove-cli/tests/differential.rs` establishes over the corpus.
    /// The three opcodes the lowering emits nowhere, run.
    ///
    /// `crates/cove-cli/tests/bytecode_corpus.rs` names sixteen opcodes no
    /// program in the repository reaches, and three of them —
    /// `Convert(IntToFloat)`, `Convert(FloatToInt)` and `layout.of` — are not
    /// merely absent from the corpus: **the lowering has no site that emits
    /// them**, so no Cove source can reach them and neither the differential
    /// harness nor any fixture written in Cove can cover them. `addr.elem` is
    /// a fourth that the corpus does not reach and `Machine::dispatch`'s own
    /// tests do.
    ///
    /// A program written in the IR directly is the only thing that can, which
    /// is what `super::tests::Build` is for. What it asserts is parity: the
    /// same hand-written program on both loops answers the same words. An
    /// instruction that runs on one path and not the other would be exactly
    /// the kind of gap Phase 5 must not delete the enum path over.
    #[test]
    fn the_opcodes_no_cove_source_reaches_run_the_same_on_both_paths() {
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

        let enumerated = Machine::new(&program, 1 << 16)
            .run(entry, &[], &budget())
            .expect("the fixture runs on the enum path");
        let encoded = run_encoded(&program, entry, &[]).expect("and on the encoded path");
        assert_eq!(encoded, enumerated);
        assert_eq!(encoded, vec![7 + u64::from(ints.0)]);
    }

    #[test]
    fn every_opcode_is_implemented() {
        let missing: Vec<Op> = Op::all()
            .into_iter()
            .filter(|op| !implemented(*op))
            .collect();
        assert!(
            missing.is_empty(),
            "the encoded path refuses {} of the {} opcodes: {missing:?}",
            missing.len(),
            Op::all().len(),
        );
    }
}
