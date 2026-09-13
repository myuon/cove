//! One [`Function`] to one run of x86-64 machine code, by hand.
//!
//! The second arm of the comparison ADR 0055's code-generator choice needs: the
//! same [`crate::abi`] entry point, over the same frame, compiling exactly the
//! subset `crate::subset`'s `supported` admits — which is the same predicate
//! Cranelift's arm asks — and emitting the bytes itself.
//!
//! # It is a template compiler, and it stays one
//!
//! Every IR instruction becomes a fixed sequence: load the operands from their
//! slots into scratch registers, compute, store the result back to its slot.
//! Nothing is kept in a register across an instruction boundary, there is no
//! register allocator, and there are no peepholes. Two registers hold state
//! across instructions and both are demanded by the ABI rather than chosen as
//! an optimisation: the unpaid work accumulator, and the frame pointer that
//! `crate::abi` requires be re-derived at every block and after every call.
//!
//! # x86-64 only
//!
//! [`Jit::new`] refuses every other architecture, and nothing is emitted on
//! one. The encoder below is ordinary Rust that appends bytes, so it *compiles*
//! anywhere; what would be wrong elsewhere is executing the bytes, and the
//! refusal is in front of the only path that can.

use std::mem::offset_of;
use std::ptr;

use cove_ir::{ArithOp, CmpOp, Function, FunctionId, Inst, Num, Program, Slot};

use crate::abi::{Entry, NativeCtx, NativeHelpers, Outcome, Raise};
use crate::subset::{by_zero_of, leaders, overflow_of, slot_offset, supported};
use crate::Unavailable;

// The `NativeCtx` field offsets, read from the declaration rather than written
// out, exactly as the Cranelift arm reads them.
const OFF_WORDS: i32 = offset_of!(NativeCtx, words) as i32;
const OFF_PENDING_WORK: i32 = offset_of!(NativeCtx, pending_work) as i32;
const OFF_RETURN_SLOT: i32 = offset_of!(NativeCtx, return_slot) as i32;
const OFF_RAISE_CODE: i32 = offset_of!(NativeCtx, raise_code) as i32;
const OFF_RAISE_DETAIL: i32 = offset_of!(NativeCtx, raise_detail) as i32;

// Register numbers, as the encoding uses them.
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

// What the four long-lived registers hold. All are callee-saved, so they
// survive the safepoint call; `RAX`, `RCX` and `RDX` are the scratch the
// templates compute in, and `RDX` is also what `idiv` clobbers.
//
// `R15` holds nothing. It is pushed so that five pushes leave `rsp` 16-byte
// aligned at a `call`, which the System V ABI requires.
const CTX: u8 = RBX;
const BASE_BYTES: u8 = R12;
const WORK: u8 = R13;
const FRAME: u8 = R14;
const PAD: u8 = R15;

// Condition codes, as the low nibble of a `jcc`/`setcc` opcode.
const CC_NO: u8 = 0x1;
const CC_E: u8 = 0x4;
const CC_NE: u8 = 0x5;
const CC_L: u8 = 0xc;
const CC_GE: u8 = 0xd;
const CC_LE: u8 = 0xe;
const CC_G: u8 = 0xf;

/// A function this code generator has compiled.
///
/// The same shape as the Cranelift arm's, so that a caller — a test, or the
/// comparison harness — reads the two the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compiled {
    /// Which mapping of its [`Jit`] this is.
    at: usize,
    /// Which Cove function this is.
    pub function: FunctionId,
    /// How many bytes of machine code this function is.
    pub code_bytes: u32,
}

/// One mapping: the machine code of one function.
///
/// # W^X
///
/// A mapping is `PROT_READ | PROT_WRITE` while the code is written into it and
/// `PROT_READ | PROT_EXEC` from [`Jit::finalize`] onwards. It is never both,
/// and `executable` is the flag that says which state it is in, so
/// [`Jit::entry`] cannot hand out a pointer into a writable page.
struct Mapping {
    at: *mut u8,
    /// The mapped length, which is the page-rounded code length.
    len: usize,
    executable: bool,
}

impl Mapping {
    /// Maps `code` writable and copies it in.
    ///
    /// The mapping is *not* executable when this returns; nothing may be called
    /// through it until [`Jit::finalize`] has flipped it.
    fn write(code: &[u8]) -> Option<Mapping> {
        let page = page_size();
        let len = code.len().div_ceil(page) * page;
        // Safety: a fresh anonymous mapping of a non-zero length, and the copy
        // is bounded by the length that was asked for.
        let at = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return None;
        }
        let at = at.cast::<u8>();
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), at, code.len()) };
        Some(Mapping {
            at,
            len,
            executable: false,
        })
    }

    /// Drops the write permission and takes the execute one.
    ///
    /// On x86-64 the instruction cache is coherent with the data cache, so the
    /// `mprotect` is the whole of what is needed; there is no cache to flush by
    /// hand, and this arm runs nowhere else.
    fn make_executable(&mut self) -> Result<(), Unavailable> {
        // Safety: `at` and `len` are what `mmap` answered, and `len` is a
        // multiple of the page size.
        let ok =
            unsafe { libc::mprotect(self.at.cast(), self.len, libc::PROT_READ | libc::PROT_EXEC) };
        if ok != 0 {
            return Err(Unavailable(
                "`mprotect` refused to make a mapping executable".to_string(),
            ));
        }
        self.executable = true;
        Ok(())
    }
}

fn page_size() -> usize {
    // Safety: a read of one sysconf variable, which cannot fail for
    // `_SC_PAGESIZE`.
    let answered = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if answered > 0 {
        answered as usize
    } else {
        4096
    }
}

/// A template code generator, and the memory its code lives in.
///
/// One of these owns every page it has written, so it must outlive every
/// [`Entry`] taken out of it. Dropping it leaks the pages rather than freeing
/// them, which is the safe direction and the same choice the Cranelift arm
/// makes: a freed page under a running Cove frame is not a bug anything could
/// diagnose.
pub struct Jit {
    safepoint: usize,
    code: Vec<Mapping>,
    finalized: bool,
}

impl Jit {
    /// A code generator for this host, calling back through `helpers`.
    ///
    /// Refuses every host that is not x86-64, which is ADR 0055's capability
    /// diagnostic rather than a lowering that would emit the wrong
    /// instructions.
    pub fn new(helpers: NativeHelpers) -> Result<Self, Unavailable> {
        if !cfg!(target_arch = "x86_64") {
            return Err(Unavailable(format!(
                "this code generator emits x86-64 and this host is {}",
                std::env::consts::ARCH
            )));
        }
        Ok(Jit {
            // The helper is bound as an address a `movabs` carries, which is
            // this arm's equivalent of the Cranelift arm's relocation.
            safepoint: helpers.safepoint as usize,
            code: Vec::new(),
            finalized: false,
        })
    }

    /// Compiles `program`'s function `id`, or answers `None` if any part of it
    /// is outside the subset — or if a mapping could not be had.
    pub fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Compiled> {
        let function = program.function(id);
        if !supported(program, function) {
            return None;
        }
        let code = Emit::new(program, function, self.safepoint).run();
        let mapping = Mapping::write(&code)?;
        self.code.push(mapping);
        self.finalized = false;
        Some(Compiled {
            at: self.code.len() - 1,
            function: id,
            code_bytes: code.len() as u32,
        })
    }

    /// Makes every function compiled so far executable.
    ///
    /// The W^X flip, and the only place it happens. Before it, every mapping is
    /// writable and not executable; after it, executable and not writable.
    pub fn finalize(&mut self) -> Result<(), Unavailable> {
        for mapping in &mut self.code {
            if !mapping.executable {
                mapping.make_executable()?;
            }
        }
        self.finalized = true;
        Ok(())
    }

    /// The entry point of a compiled function.
    ///
    /// # Panics
    ///
    /// If [`Jit::finalize`] has not been called since the last
    /// [`Jit::compile`]. Calling into a page that is still writable is exactly
    /// what must not happen, so it is a panic and not a `None` a caller could
    /// ignore.
    pub fn entry(&self, compiled: Compiled) -> Entry {
        assert!(
            self.finalized,
            "`Jit::finalize` has to run before a compiled function is entered"
        );
        let mapping = &self.code[compiled.at];
        assert!(mapping.executable, "the mapping is not executable");
        // Safety: `at` is the start of a finalized, read-execute mapping
        // holding exactly the prologue this module emits, which is `Entry`'s
        // shape.
        unsafe { std::mem::transmute::<*const u8, Entry>(mapping.at.cast_const()) }
    }
}

/// Where a jump goes.
#[derive(Clone, Copy)]
enum Target {
    /// The block that begins at this IR instruction.
    Pc(u32),
    /// A site inside one instruction's template — the far side of a raise, or
    /// the far side of a safepoint.
    Label(usize),
}

/// A `rel32` field waiting for its displacement.
struct Fixup {
    /// Where the four bytes are.
    at: usize,
    to: Target,
}

/// One function's lowering.
struct Emit<'a> {
    program: &'a Program,
    function: &'a Function,
    safepoint: usize,
    code: Vec<u8>,
    /// Every `rel32` emitted, patched in [`Emit::patch`] once the whole body is
    /// out — which is why a forward jump needs no second pass over the IR.
    fixups: Vec<Fixup>,
    /// Where a label was bound, or `None` while it is still ahead.
    labels: Vec<Option<usize>>,
    /// Per IR instruction: where the block that begins there starts in the
    /// code, and how long the block is.
    blocks: Vec<Option<u32>>,
    block_at: Vec<Option<usize>>,
    /// Whether [`FRAME`] currently holds the frame pointer.
    ///
    /// False at the start of every block and after every call: `NativeCtx`'s
    /// `words` is a `Vec`'s buffer, and a helper may have reallocated it.
    frame_live: bool,
}

impl<'a> Emit<'a> {
    fn new(program: &'a Program, function: &'a Function, safepoint: usize) -> Self {
        let blocks = leaders(function);
        Emit {
            program,
            function,
            safepoint,
            code: Vec::new(),
            fixups: Vec::new(),
            labels: Vec::new(),
            block_at: vec![None; blocks.len()],
            blocks,
            frame_live: false,
        }
    }

    fn run(mut self) -> Vec<u8> {
        self.prologue();
        for pc in 0..self.function.code.len() {
            if let Some(length) = self.blocks[pc] {
                self.block_at[pc] = Some(self.code.len());
                // A block is entered from anywhere, so nothing a predecessor
                // left in a register is readable here.
                self.frame_live = false;
                self.charge(length);
            }
            self.inst(pc);
        }
        self.patch();
        self.code
    }

    /// `extern "C" fn(ctx: *mut NativeCtx, base: u64) -> Outcome`, received.
    ///
    /// `base` is a word index, and the only thing this function ever wants from
    /// it is the byte offset, so the shift is paid once here rather than at
    /// every block that re-derives the frame pointer.
    fn prologue(&mut self) {
        for reg in [CTX, BASE_BYTES, WORK, FRAME, PAD] {
            self.push(reg);
        }
        self.mov_rr(CTX, RDI);
        self.mov_rr(BASE_BYTES, RSI);
        self.shl_imm8(BASE_BYTES, 3);
        self.xor_rr(WORK, WORK);
    }

    /// Adds a block's static instruction count to the work accumulator.
    ///
    /// One add per block, at block entry, which is what the Cranelift arm does
    /// and for the same reason: a block with two predecessors is reached having
    /// done two different amounts of work, so the accumulator is a run-time
    /// value and only the charge is a constant.
    fn charge(&mut self, instructions: u32) {
        self.add_imm32(WORK, instructions as i32);
    }

    /// Emits one IR instruction's template.
    fn inst(&mut self, pc: usize) {
        match &self.function.code[pc] {
            Inst::Bool { dst, value } => {
                self.mov_imm64(RAX, i64::from(*value));
                self.store_slot(*dst, RAX);
            }
            Inst::Int { dst, value } => {
                self.mov_imm64(RAX, *value);
                self.store_slot(*dst, RAX);
            }
            Inst::Copy { dst, src, layout } => {
                self.copy(*dst, *src, self.program.layout(*layout).width());
            }
            Inst::Arith {
                num: Num::Int,
                op,
                dst,
                a,
                b,
            } => {
                self.load_slot(RAX, *a);
                self.load_slot(RCX, *b);
                self.arith(*op, *dst);
            }
            Inst::ArithImm { op, dst, a, value } => {
                self.load_slot(RAX, *a);
                self.mov_imm64(RCX, *value);
                self.arith(*op, *dst);
            }
            Inst::Cmp {
                on: _,
                op,
                dst,
                a,
                b,
            } => {
                self.load_slot(RAX, *a);
                self.load_slot(RCX, *b);
                self.compare(*op, *dst);
            }
            Inst::CmpImm { op, dst, a, value } => {
                self.load_slot(RAX, *a);
                self.mov_imm64(RCX, *value);
                self.compare(*op, *dst);
            }
            Inst::CmpBranch {
                on: _,
                op,
                dst,
                a,
                b,
                target,
            } => {
                self.load_slot(RAX, *a);
                self.load_slot(RCX, *b);
                self.compare(*op, *dst);
                self.branch_when_false(pc, *target);
            }
            Inst::CmpImmBranch {
                op,
                dst,
                a,
                value,
                target,
            } => {
                self.load_slot(RAX, *a);
                // Sign-extended, because the IR's fused immediate is an `i32`
                // and the comparison is over `i64`.
                self.mov_imm64(RCX, i64::from(*value));
                self.compare(*op, *dst);
                self.branch_when_false(pc, *target);
            }
            // `encoded.rs`'s `BRANCH_FALSE` arm tests the whole *word* against
            // zero, so that is what is tested here and not the low byte.
            Inst::BranchFalse { cond, to } => {
                self.load_slot(RAX, *cond);
                self.test_rr(RAX, RAX);
                self.branch_when_false(pc, *to);
            }
            Inst::Jump { to } => {
                if (*to as usize) <= pc {
                    self.safepoint(*to);
                }
                self.jmp(Target::Pc(*to));
            }
            Inst::Return { src } => self.ret(*src),
            // The message is a program string, so the `StrId` is what crosses
            // the boundary and `cove-runtime` looks it up.
            Inst::Trap { message } => self.raise(Raise::Trapped, message.0),
            other => unreachable!("`supported` admitted {other:?}, which is not lowered"),
        }
    }

    // --- memory ------------------------------------------------------------

    /// Re-derives the frame pointer, if this block has not already.
    ///
    /// `words` is re-loaded from the context rather than remembered, which is
    /// the whole of `crate::abi`'s reallocation discipline.
    fn frame(&mut self) {
        if self.frame_live {
            return;
        }
        self.load(FRAME, CTX, OFF_WORDS);
        self.add_rr(FRAME, BASE_BYTES);
        self.frame_live = true;
    }

    fn load_slot(&mut self, into: u8, slot: Slot) {
        self.frame();
        let at = slot_offset(slot).expect("`supported` bounded every slot");
        self.load(into, FRAME, at);
    }

    fn store_slot(&mut self, slot: Slot, from: u8) {
        self.frame();
        let at = slot_offset(slot).expect("`supported` bounded every slot");
        self.store(FRAME, at, from);
    }

    /// ADR 0001's field-wise shallow copy, which `encoded.rs`'s `COPY` arm
    /// performs with `Memory::copy_slots` — a `memmove`.
    ///
    /// Every word is loaded before any is stored, because two slots of one
    /// frame may overlap: a forward run of load-store pairs would smear the
    /// source over itself. The machine stack is where the words wait, which is
    /// what a template compiler has instead of sixteen free registers, and the
    /// pops come back in the opposite order for free.
    fn copy(&mut self, dst: Slot, src: Slot, width: u32) {
        for word in 0..width {
            self.load_slot(RAX, src + word);
            self.push(RAX);
        }
        for word in (0..width).rev() {
            self.pop(RAX);
            self.store_slot(dst + word, RAX);
        }
    }

    // --- arithmetic --------------------------------------------------------

    /// `encoded.rs`'s `int_op!`/`arith_imm!`, which are one call to
    /// `int_arith`: `checked_add`, `checked_sub`, `checked_mul`, and a zero test
    /// *before* `checked_div` and `checked_rem`.
    ///
    /// The left operand is in `RAX` and the right in `RCX`. `add`, `sub` and
    /// `imul` set the overflow flag for exactly the cases `checked_*` answers
    /// `None` for, so each is one instruction and one `jno`. Division is not:
    /// `idiv` raises a machine exception on both of its failures, so both are
    /// tested before it — the zero divisor first, because `i64::MIN / 0` has to
    /// say "by zero" and not "overflowed".
    fn arith(&mut self, op: ArithOp, dst: Slot) {
        let overflow = overflow_of(self.function, op, dst);
        match op {
            ArithOp::Add => {
                self.add_rr(RAX, RCX);
                self.raise_unless(CC_NO, overflow);
                self.store_slot(dst, RAX);
            }
            ArithOp::Sub => {
                self.sub_rr(RAX, RCX);
                self.raise_unless(CC_NO, overflow);
                self.store_slot(dst, RAX);
            }
            ArithOp::Mul => {
                self.imul_rr(RAX, RCX);
                self.raise_unless(CC_NO, overflow);
                self.store_slot(dst, RAX);
            }
            ArithOp::Div | ArithOp::Rem => {
                self.test_rr(RCX, RCX);
                self.raise_unless(CC_NE, by_zero_of(op));
                // `i64::MIN / -1` is `checked_div`'s other `None`, which
                // `int_arith` reports as an overflow of the named operation.
                // Both halves have to hold for it, so the ordinary path leaves
                // at the first that does not.
                let fine = self.label();
                self.mov_imm64(RDX, i64::MIN);
                self.cmp_rr(RAX, RDX);
                self.jcc(CC_NE, Target::Label(fine));
                self.mov_imm64(RDX, -1);
                self.cmp_rr(RCX, RDX);
                self.jcc(CC_NE, Target::Label(fine));
                self.raise(overflow, 0);
                self.bind(fine);
                // `cqo` sign-extends `RAX` into `RDX:RAX`, which is the
                // dividend `idiv` reads; the quotient lands in `RAX` and the
                // remainder in `RDX`.
                self.cqo();
                self.idiv(RCX);
                let answer = if matches!(op, ArithOp::Rem) { RDX } else { RAX };
                self.store_slot(dst, answer);
            }
        }
    }

    /// `encoded.rs`'s `cmp_int!`: a signed comparison of the two words, stored
    /// as `answer as u64` — a zero or a one and never a mask.
    ///
    /// The flags are left as `cmp` set them, so a fused branch can test them
    /// again; `setcc` and `movzx` do not disturb them.
    fn compare(&mut self, op: CmpOp, dst: Slot) {
        let cc = match op {
            CmpOp::Eq => CC_E,
            CmpOp::Ne => CC_NE,
            CmpOp::Lt => CC_L,
            CmpOp::Le => CC_LE,
            CmpOp::Gt => CC_G,
            CmpOp::Ge => CC_GE,
        };
        self.cmp_rr(RAX, RCX);
        self.setcc(cc);
        self.movzx_eax_al();
        self.store_slot(dst, RAX);
        // What the fused form branches on, and what `BranchFalse` would have
        // set for itself: the stored word against zero.
        self.test_rr(RAX, RAX);
    }

    // --- control flow ------------------------------------------------------

    /// Takes `target` when the flags say zero, and falls through otherwise.
    ///
    /// The caller has already left a `test` in the flags. When `target` is at
    /// or behind this instruction the branch is a loop backedge, and the
    /// safepoint goes *on that edge* — so the iteration that leaves the loop
    /// does not pay for a poll it did not need.
    fn branch_when_false(&mut self, pc: usize, target: u32) {
        if (target as usize) <= pc {
            let through = self.label();
            self.jcc(CC_NE, Target::Label(through));
            self.safepoint(target);
            self.jmp(Target::Pc(target));
            self.bind(through);
        } else {
            self.jcc(CC_E, Target::Pc(target));
        }
    }

    /// A safepoint: hand the runtime the unpaid work, and leave if it says to.
    ///
    /// Emitted on every backedge, with the same accumulated static work count
    /// the Cranelift arm hands the same helper, so the two arms pay the same
    /// runtime cost at the same places.
    fn safepoint(&mut self, pc: u32) {
        self.mov_rr(RDI, CTX);
        self.mov_imm32(RSI, pc as i32);
        self.mov_rr(RDX, WORK);
        self.mov_imm64(RAX, self.safepoint as i64);
        self.call(RAX);
        // Rust's `bool` across a C boundary is the low byte, 0 or 1.
        self.test_al_al();
        let carry_on = self.label();
        self.jcc(CC_NE, Target::Label(carry_on));
        // Nothing is pending at a stop: the charge went to the helper before it
        // answered.
        self.xor_rr(RAX, RAX);
        self.store(CTX, OFF_PENDING_WORK, RAX);
        self.leave(Outcome::Stopped);
        self.bind(carry_on);
        self.xor_rr(WORK, WORK);
        // The helper is allowed to have grown the stack, so the frame pointer
        // derived before the call is not to be used after it.
        self.frame_live = false;
    }

    /// `encoded.rs`'s `RETURN` arm, minus the copy: the slot is reported and the
    /// caller does the copying, because the caller's frame is the caller's.
    fn ret(&mut self, src: Slot) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.store_imm32(CTX, OFF_RETURN_SLOT, src as i32);
        self.leave(Outcome::Returned);
    }

    /// Leaves with a runtime error named rather than built.
    fn raise(&mut self, code: Raise, detail: u32) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.store_imm32(CTX, OFF_RAISE_CODE, code.abi() as i32);
        self.store_imm32(CTX, OFF_RAISE_DETAIL, detail as i32);
        self.leave(Outcome::Raised);
    }

    /// Raises unless the flags satisfy `cc`, and carries on otherwise.
    ///
    /// The raise is inline and jumped over, so the ordinary path is a
    /// not-taken branch. The frame pointer is *not* invalidated by it: a raise
    /// is a return, not a call, so nothing between here and the next
    /// instruction can grow the stack.
    fn raise_unless(&mut self, cc: u8, code: Raise) {
        let good = self.label();
        self.jcc(cc, Target::Label(good));
        self.raise(code, 0);
        self.bind(good);
    }

    /// The epilogue, and the one place it is written.
    ///
    /// `eax` carries the [`Outcome`], which is `#[repr(u32)]`. The five pops
    /// undo the five pushes of the prologue; nothing else is ever left on the
    /// stack at an exit, because the only thing that pushes is
    /// [`Emit::copy`] and it pops what it pushed before the instruction ends.
    fn leave(&mut self, outcome: Outcome) {
        self.mov_imm32(RAX, outcome.abi() as i32);
        for reg in [PAD, FRAME, WORK, BASE_BYTES, CTX] {
            self.pop(reg);
        }
        self.ret_near();
    }

    // --- labels and fixups -------------------------------------------------

    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn bind(&mut self, label: usize) {
        self.labels[label] = Some(self.code.len());
    }

    /// Fills in every `rel32` now that every target is known.
    ///
    /// Displacements are relative to the *end* of the instruction, which is
    /// four bytes past the field.
    fn patch(&mut self) {
        for fixup in std::mem::take(&mut self.fixups) {
            let to = match fixup.to {
                Target::Pc(pc) => {
                    self.block_at[pc as usize].expect("a branch target begins a block")
                }
                Target::Label(label) => self.labels[label].expect("every label is bound"),
            };
            let from = fixup.at + 4;
            let displacement = (to as i64 - from as i64) as i32;
            self.code[fixup.at..from].copy_from_slice(&displacement.to_le_bytes());
        }
    }

    // --- the encoder -------------------------------------------------------
    //
    // Only the forms below are emitted, and each is written once. The operand
    // encoding is `REX` then opcode then `ModRM`, with `mod = 0b10` for every
    // memory operand — a `disp32` even when the displacement is zero, because a
    // template compiler does not choose between encodings.

    fn byte(&mut self, byte: u8) {
        self.code.push(byte);
    }

    /// `REX`, with `W` for a 64-bit operation and the high bits of the two
    /// register fields.
    ///
    /// `reg` is the `ModRM.reg` field and `rm` the `ModRM.rm` one, so the bits
    /// are `REX.R` and `REX.B`. Nothing here uses an index register, so
    /// `REX.X` is always zero.
    fn rex(&mut self, wide: bool, reg: u8, rm: u8) {
        let byte = 0x40 | u8::from(wide) << 3 | (reg & 8) >> 1 | (rm & 8) >> 3;
        if byte != 0x40 {
            self.byte(byte);
        }
    }

    fn modrm_reg(&mut self, reg: u8, rm: u8) {
        self.byte(0xc0 | (reg & 7) << 3 | (rm & 7));
    }

    fn modrm_mem(&mut self, reg: u8, base: u8, disp: i32) {
        debug_assert!(base & 7 != 4, "a `rsp`-shaped base would need a SIB byte");
        self.byte(0x80 | (reg & 7) << 3 | (base & 7));
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    /// `mov r64, r64`
    fn mov_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x89);
        self.modrm_reg(src, dst);
    }

    /// `mov r64, [base + disp]`
    fn load(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x8b);
        self.modrm_mem(dst, base, disp);
    }

    /// `mov [base + disp], r64`
    fn store(&mut self, base: u8, disp: i32, src: u8) {
        self.rex(true, src, base);
        self.byte(0x89);
        self.modrm_mem(src, base, disp);
    }

    /// `mov dword [base + disp], imm32`, for the `u32` fields of the context.
    fn store_imm32(&mut self, base: u8, disp: i32, value: i32) {
        self.rex(false, 0, base);
        self.byte(0xc7);
        self.modrm_mem(0, base, disp);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `movabs r64, imm64`
    fn mov_imm64(&mut self, dst: u8, value: i64) {
        self.rex(true, 0, dst);
        self.byte(0xb8 | (dst & 7));
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `mov r32, imm32`, which zeroes the upper half of the register.
    fn mov_imm32(&mut self, dst: u8, value: i32) {
        self.rex(false, 0, dst);
        self.byte(0xb8 | (dst & 7));
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `add r64, r64`
    fn add_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x01);
        self.modrm_reg(src, dst);
    }

    /// `sub r64, r64`
    fn sub_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x29);
        self.modrm_reg(src, dst);
    }

    /// `imul r64, r64`, whose overflow flag is the signed one.
    fn imul_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, dst, src);
        self.byte(0x0f);
        self.byte(0xaf);
        self.modrm_reg(dst, src);
    }

    /// `add r64, imm32`
    fn add_imm32(&mut self, dst: u8, value: i32) {
        self.rex(true, 0, dst);
        self.byte(0x81);
        self.modrm_reg(0, dst);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `shl r64, imm8`
    fn shl_imm8(&mut self, dst: u8, by: u8) {
        self.rex(true, 0, dst);
        self.byte(0xc1);
        self.modrm_reg(4, dst);
        self.byte(by);
    }

    /// `xor r64, r64`
    fn xor_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x31);
        self.modrm_reg(src, dst);
    }

    /// `cmp a, b`, which sets the flags for `a - b`.
    fn cmp_rr(&mut self, a: u8, b: u8) {
        self.rex(true, b, a);
        self.byte(0x39);
        self.modrm_reg(b, a);
    }

    /// `test a, b`
    fn test_rr(&mut self, a: u8, b: u8) {
        self.rex(true, b, a);
        self.byte(0x85);
        self.modrm_reg(b, a);
    }

    /// `test al, al`
    fn test_al_al(&mut self) {
        self.byte(0x84);
        self.byte(0xc0);
    }

    /// `setcc al`
    fn setcc(&mut self, cc: u8) {
        self.byte(0x0f);
        self.byte(0x90 | cc);
        self.byte(0xc0);
    }

    /// `movzx eax, al`, which leaves a zero or a one in the whole of `rax`.
    fn movzx_eax_al(&mut self) {
        self.byte(0x0f);
        self.byte(0xb6);
        self.byte(0xc0);
    }

    /// `cqo`
    fn cqo(&mut self) {
        self.rex(true, 0, 0);
        self.byte(0x99);
    }

    /// `idiv r64`
    fn idiv(&mut self, by: u8) {
        self.rex(true, 0, by);
        self.byte(0xf7);
        self.modrm_reg(7, by);
    }

    fn push(&mut self, reg: u8) {
        if reg & 8 != 0 {
            self.byte(0x41);
        }
        self.byte(0x50 | (reg & 7));
    }

    fn pop(&mut self, reg: u8) {
        if reg & 8 != 0 {
            self.byte(0x41);
        }
        self.byte(0x58 | (reg & 7));
    }

    /// `call r64`
    fn call(&mut self, reg: u8) {
        self.rex(false, 0, reg);
        self.byte(0xff);
        self.modrm_reg(2, reg);
    }

    fn ret_near(&mut self) {
        self.byte(0xc3);
    }

    /// `jmp rel32`, recorded for patching.
    fn jmp(&mut self, to: Target) {
        self.byte(0xe9);
        self.rel32(to);
    }

    /// `jcc rel32`, recorded for patching.
    fn jcc(&mut self, cc: u8, to: Target) {
        self.byte(0x0f);
        self.byte(0x80 | cc);
        self.rel32(to);
    }

    /// Four zero bytes and a note of where they are.
    ///
    /// Always a `rel32`, never the `rel8` a short jump could use: the distance
    /// is not known when the branch is emitted, and choosing the short form
    /// afterwards is exactly the peephole a template compiler does not have.
    fn rel32(&mut self, to: Target) {
        self.fixups.push(Fixup {
            at: self.code.len(),
            to,
        });
        self.code.extend_from_slice(&0i32.to_le_bytes());
    }
}
