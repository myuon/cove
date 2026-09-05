//! The dispatch loop over [`EncodedInst`], and the refusal that keeps it
//! honest.
//!
//! [ADR 0041](../../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! decided the sixteen-byte instruction and `cove_ir::bytecode` built the
//! encoder, the decoder, the verifier and the disassembly. Nothing ran one.
//! This is [issue #245](https://github.com/myuon/cove/issues/245)'s **Phase
//! 3**: the vertical slice that executes the `arith` benchmark, and only what
//! `arith` reaches, from the encoded form.
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
//! [`prepare`] encodes, verifies, and then walks every instruction of every
//! function and **refuses the program** if any opcode is not implemented
//! here. The refusal happens before the first instruction runs, names the
//! opcode, and points at the source span the instruction came from.
//!
//! There is deliberately **no fallback to [`Machine::dispatch`]**. A quiet
//! hand-back would make the measurement meaningless — a run that reported the
//! encoded path's wall time while executing the enum's — and would make
//! issue #245's Phase 5 unverifiable, since "no silent fallback to enum
//! execution" cannot be checked against a path that silently falls back.
//! Phase 3 refuses; Phase 4 is where the refusals go away.
//!
//! A *family* is covered or refused, never one member of it. `arith` reaches
//! `eq.int`, `lt.int.imm`, `eq.int.imm`, `add.int.imm` and `rem.int.imm`;
//! what is implemented is every `Cmp(Int, _)`, every `CmpImm` and every
//! `ArithImm`, because the operator falls out of the opcode and splitting a
//! cross product by which member a benchmark happened to reach would be an
//! arbitrary line.
//!
//! # It is the same machine
//!
//! Nothing here is a second implementation of anything a program can
//! observe. The fuel accounting, the safepoint, the debugger question, the
//! collector poll and the span lookup are the lines [`Machine::dispatch`]
//! runs, in the same order; the arithmetic is [`super::int_arith`] and
//! [`super::compare`], the same functions; a builtin call is
//! [`Machine::call_builtin`]. **One encoded instruction is one instruction
//! and one unit of fuel**, exactly as the enum's, which is what makes a
//! `fuel_spent` comparison between the two an equivalence check rather than
//! a coincidence.
//!
//! Bytecode pc *is* IR pc — ADR 0041's 1:1 encoding — so `Function::spans`
//! is indexed by the same number and a failure points at the same place
//! through both paths without a remapping.

use std::sync::Arc;

use cove_ir::bytecode::{disasm, encode_program, verify, Encoded, EncodedInst, Op};
use cove_ir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, FunctionId, LayoutId, Program, Repr, Slot, StrId,
};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::interp::stopped_here;

use super::{compare, int_arith, Live, Machine, SAFEPOINT_STRIDE};

// The opcodes this path runs, by the name ADR 0041 gives them rather than by
// number. `Op::number` is a `const fn` so that these are `match` patterns:
// the numbers are positions in a generated table and move when the table
// does, and nothing here should have to move with them.
const CONST_UNIT: u8 = Op::ConstUnit.number();
const CONST_INT: u8 = Op::ConstInt.number();
const STR: u8 = Op::Str.number();
const COPY: u8 = Op::Copy.number();
const CLEAR: u8 = Op::Clear.number();

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

const EQ_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Eq).number();
const NE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Ne).number();
const LT_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Lt).number();
const LE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Le).number();
const GT_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Gt).number();
const GE_INT: u8 = Op::Cmp(Compare::Int, CmpOp::Ge).number();

const JUMP: u8 = Op::Jump.number();
const BRANCH_FALSE: u8 = Op::BranchFalse.number();
const RETURN: u8 = Op::Return.number();
const CALL_BUILTIN: u8 = Op::CallBuiltin.number();
const ASSERT_FAILED: u8 = Op::AssertFailed.number();

/// Whether [`dispatch`] implements this opcode.
///
/// The one list, read by [`prepare`] before a run and by the loop's last
/// `match` arm, which cannot be reached once [`prepare`] has passed and is
/// there because "cannot be reached" is not "does not exist". Growing this is
/// what Phase 4 is.
pub(crate) fn implemented(op: Op) -> bool {
    matches!(
        op,
        Op::ConstUnit
            | Op::ConstInt
            | Op::Str
            | Op::Copy
            | Op::Clear
            | Op::ArithImm(_)
            | Op::CmpImm(_)
            | Op::Cmp(Compare::Int, _)
            | Op::Jump
            | Op::BranchFalse
            | Op::Return
            | Op::CallBuiltin
            | Op::AssertFailed
    )
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
        "Issue #245's Phase 3 executes the opcodes the `arith` benchmark reaches and refuses every other one. It never hands the program back to the readable-IR loop, because a silent fallback would make the comparison between the two meaningless.",
    )
    .with_help(format!(
        "run this program on the ordinary path; the instruction is `{}` at `{}` pc {pc}",
        disasm::one(program, id, held, pc as u32),
        function.qualified(),
    ))
}

/// The loop, over encoded instructions.
///
/// `id`, `base`, `pc` and `code` are locals for the same reason
/// [`Machine::dispatch`] keeps them as locals, and are written back to the
/// frame at the same two points: a stop something else reads at, and a
/// failure.
///
/// `floor` is [`Machine::dispatch`]'s: the frame depth this turn of the loop
/// was entered at, which a `return` below is what ends it.
pub(super) fn dispatch(
    machine: &mut Machine<'_>,
    encoded: &Encoded,
    budget: &Meter,
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

        // The operator is a constant at each call site, which is the whole
        // point of one opcode per concrete operation: `int_arith` and
        // `compare` are the enum path's own functions, so nothing can drift,
        // and their inner `match` folds away because the operator is known.
        macro_rules! arith_imm {
            ($op:expr) => {{
                let x = machine.mem.slot(base, held.b() as Slot);
                // The same question `Inst::ArithImm` asks, for the same
                // reason: which of the two the operands are decides only what
                // a failure calls the operation.
                let duration = machine.repr(id, held.a() as Slot) == Some(Repr::Duration);
                match int_arith($op, x as i64, held.payload() as i64, duration) {
                    Ok(value) => machine.mem.set_slot(base, held.a() as Slot, value as u64),
                    Err(error) => fail!(error),
                }
            }};
        }
        macro_rules! cmp_imm {
            ($op:expr) => {{
                let x = machine.mem.slot(base, held.b() as Slot) as i64;
                let answer = compare($op, x.cmp(&(held.payload() as i64)));
                machine.mem.set_slot(base, held.a() as Slot, answer as u64);
            }};
        }
        macro_rules! cmp_int {
            ($op:expr) => {{
                let x = machine.mem.slot(base, held.b() as Slot) as i64;
                let y = machine.mem.slot(base, held.c() as Slot) as i64;
                let answer = compare($op, x.cmp(&y));
                machine.mem.set_slot(base, held.a() as Slot, answer as u64);
            }};
        }

        match held.opcode() {
            // ---- constants and moves ---------------------------------
            CONST_UNIT => machine.mem.set_slot(base, held.a() as Slot, 0),
            CONST_INT => machine.mem.set_slot(base, held.a() as Slot, held.payload()),
            STR => {
                machine.sync(pc - 1);
                match machine.intern(StrId(held.lo())) {
                    Ok(addr) => machine.mem.set_slot(base, held.a() as Slot, addr),
                    Err(error) => fail!(error),
                }
            }
            COPY => {
                let width = machine.width(LayoutId(held.lo()));
                machine
                    .mem
                    .copy_words(base + held.a() as u64, base + held.b() as u64, width);
            }
            CLEAR => {
                let width = machine.width(LayoutId(held.lo()));
                machine.mem.clear_words(base + held.a() as u64, width);
            }

            // ---- scalar operations -----------------------------------
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

            EQ_INT => cmp_int!(CmpOp::Eq),
            NE_INT => cmp_int!(CmpOp::Ne),
            LT_INT => cmp_int!(CmpOp::Lt),
            LE_INT => cmp_int!(CmpOp::Le),
            GT_INT => cmp_int!(CmpOp::Gt),
            GE_INT => cmp_int!(CmpOp::Ge),

            // ---- control flow ----------------------------------------
            // The displacement is `to - (pc + 1)` and `pc` is already past
            // the instruction, so this is one addition and no table.
            JUMP => pc = pc.wrapping_add_signed(held.payload() as i64 as isize),
            BRANCH_FALSE => {
                if machine.mem.slot(base, held.a() as Slot) == 0 {
                    pc = pc.wrapping_add_signed(held.payload() as i64 as isize);
                }
            }
            RETURN => {
                let src = held.a() as Slot;
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
            // Not a boundary, and not a frame: a builtin reads the words and
            // the objects the machine already holds and answers a value
            // location's worth of words.
            CALL_BUILTIN => {
                machine.sync(pc - 1);
                let dst = held.a() as Slot;
                match machine.call_builtin(base, BuiltinId(held.lo()), ArgsId(held.hi())) {
                    Ok(words) => {
                        for (at, word) in words.iter().enumerate() {
                            machine.mem.set_slot(base, dst + at as u32, *word);
                        }
                    }
                    Err(error) => fail!(error),
                }
            }

            // ---- failure ---------------------------------------------
            ASSERT_FAILED => {
                let addr = machine.mem.slot(base, held.a() as Slot);
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
