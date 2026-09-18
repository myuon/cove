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

use cove_ir::{
    ArgsId, ArithOp, CmpOp, Compare, Convert, Function, FunctionId, Inst, Len, Num, Program, Slot,
    Storage, StrId,
};

use crate::abi::{
    Entry, GrowableOp, IntrinsicProtocol, NativeCtx, NativeHelpers, Outcome, Raise, RunOp,
    HEAP_CHUNK_SHIFT, HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS,
};
use crate::subset::{
    by_zero_of, byte_store, leaders, literal_offset, overflow_of, reserve, slot_offset, supported,
    windows, word_finish, BufferWindow, ByteStore, Reserve, WordFinish,
};
use crate::Unavailable;

// The `NativeCtx` field offsets, read from the declaration rather than written
// out, exactly as the Cranelift arm reads them.
const OFF_WORDS: i32 = offset_of!(NativeCtx, words) as i32;
const OFF_CHUNKS: i32 = offset_of!(NativeCtx, chunks) as i32;
const OFF_LITERALS: i32 = offset_of!(NativeCtx, literals) as i32;
const OFF_FIXED_PAYLOAD_WORDS: i32 = offset_of!(NativeCtx, fixed_payload_words) as i32;
const OFF_STACK_ORIGIN: i32 = offset_of!(NativeCtx, stack_origin) as i32;
const OFF_PENDING_WORK: i32 = offset_of!(NativeCtx, pending_work) as i32;
const OFF_POLL_AT: i32 = offset_of!(NativeCtx, poll_at) as i32;
const OFF_RAISE_CODE: i32 = offset_of!(NativeCtx, raise_code) as i32;
const OFF_RAISE_DETAIL: i32 = offset_of!(NativeCtx, raise_detail) as i32;
const OFF_RAISE_PC: i32 = offset_of!(NativeCtx, raise_pc) as i32;
const OFF_RAISE_A: i32 = offset_of!(NativeCtx, raise_a) as i32;
const OFF_RAISE_B: i32 = offset_of!(NativeCtx, raise_b) as i32;

// Register numbers, as the encoding uses them.
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RBP: u8 = 5;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

// What the five long-lived registers hold. All are callee-saved, so they
// survive the safepoint and call helpers; `RAX`, `RCX` and `RDX` are the scratch
// the templates compute in, and `RDX` is also what `idiv` clobbers.
const CTX: u8 = RBX;
const BASE_BYTES: u8 = R12;
const WORK: u8 = R13;
const FRAME: u8 = R14;

// Where the answer goes, as a *byte* offset from word zero of the segment:
// `(return_base + return_slot) * 8`, computed once in the prologue because
// [`Emit::ret`] is the only reader and a `return` may be the end of any block.
//
// `RBP` because the other callee-saved registers are taken. Nothing here keeps a
// frame pointer in it — this arm addresses the machine stack only through `push`
// and `pop` — so what it costs is that a profiler unwinding by frame pointers
// cannot walk through a compiled Cove frame, which is already true of the
// Cranelift arm's frames and of neither arm's Cove semantics.
const RETURN_BYTES: u8 = RBP;

// The three registers a heap word's address is formed in, which is the one
// template that needs more than the three scratch above: the chunk table, the
// chunk, and the index inside it are three live values at once.
//
// `RSI` and `RDI` are caller-saved and hold nothing between instructions — they
// are written at a `call` and nowhere else — so using them here costs nothing.
// `R15` is pushed by the prologue and used to be the push that left `rsp`
// 16-byte aligned at a `call`; see [`Emit::prologue`] for what keeps that true
// now that there are six registers to save. Holding a scratch value inside one
// template does not change what it is for.
const HEAP_TABLE: u8 = RSI;
const HEAP_INDEX: u8 = RDI;
const HEAP_SPARE: u8 = R15;

// Condition codes, as the low nibble of a `jcc`/`setcc` opcode.
const CC_NO: u8 = 0x1;
const CC_B: u8 = 0x2;
const CC_AE: u8 = 0x3;
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
    helpers: Helpers,
    code: Vec<Mapping>,
    finalized: bool,
    /// Whether a call whose callee is compiled is made by emitted code itself.
    ///
    /// Off by default, which is [ADR 0055]'s order of business: it names direct
    /// native-to-native calls as "later optimizations, not requirements of the
    /// first tier", and a switch is what lets the two be raced against each
    /// other in one process over one lowering — which is how
    /// [issue #365](https://github.com/myuon/cove/issues/365) asks for the
    /// answer. See [`Jit::calling_directly`] and [`Emit::callee_direct`].
    ///
    /// [ADR 0055]: ../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
    direct: bool,
}

/// The helper addresses, as the numbers a `movabs` carries.
///
/// This arm's equivalent of the Cranelift arm's relocations: there is no linker
/// here, so a helper's address is an immediate in the instruction stream.
#[derive(Clone, Copy)]
struct Helpers {
    safepoint: usize,
    call: usize,
    open: usize,
    close: usize,
    alloc: usize,
    intrinsic: usize,
    growable: usize,
    run_copy: usize,
    field_load: usize,
    field_store: usize,
    order_str: usize,
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
            helpers: Helpers {
                safepoint: helpers.safepoint as usize,
                call: helpers.call as usize,
                open: helpers.open as usize,
                close: helpers.close as usize,
                alloc: helpers.alloc as usize,
                intrinsic: helpers.intrinsic as usize,
                growable: helpers.growable as usize,
                run_copy: helpers.run_copy as usize,
                field_load: helpers.field_load as usize,
                field_store: helpers.field_store as usize,
                order_str: helpers.order_str as usize,
            },
            code: Vec::new(),
            finalized: false,
            direct: false,
        })
    }

    /// The same code generator, emitting a **direct call** where the callee has
    /// compiled code.
    ///
    /// See `Emit::callee_direct` for what that is, and the `direct` field this
    /// sets for why it is a choice rather than the only way — [`crate::abi::OpenFn`]
    /// is the contract either way. Every function compiled after
    /// this answers is emitted the new way; nothing already compiled changes, so
    /// a caller that wants one of each compiles the slice twice over two `Jit`s,
    /// which is what the comparison harness does.
    pub fn calling_directly(mut self) -> Self {
        self.direct = true;
        self
    }

    /// Compiles `program`'s function `id`, or answers `None` if any part of it
    /// is outside the subset — or if a mapping could not be had.
    pub fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Compiled> {
        let function = program.function(id);
        if !supported(program, function) {
            return None;
        }
        let code = Emit::new(program, function, &self.helpers, self.direct).run();
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
    call: usize,
    open: usize,
    close: usize,
    alloc: usize,
    intrinsic: usize,
    growable: usize,
    run_copy: usize,
    field_load: usize,
    field_store: usize,
    order_str: usize,
    /// Whether a compiled callee is reached by emitted code. See [`Jit::direct`].
    direct: bool,
    /// Which IR instruction is being emitted.
    ///
    /// Only a raise reads it — [`NativeCtx::raise_pc`] is how the runtime finds
    /// the span — and a raise is emitted from three methods down, so it is a
    /// field rather than an argument threaded through each of them.
    pc: usize,
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
    /// Per IR instruction: the [ADR 0062] window whose head it is, if one is.
    /// See [`BufferWindow`].
    ///
    /// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    windows: Vec<Option<BufferWindow>>,
    /// Whether [`FRAME`] currently holds the frame pointer.
    ///
    /// False at the start of every block and after every call: `NativeCtx`'s
    /// `words` is a `Vec`'s buffer, and a helper may have reallocated it.
    frame_live: bool,
}

impl<'a> Emit<'a> {
    fn new(program: &'a Program, function: &'a Function, helpers: &Helpers, direct: bool) -> Self {
        let blocks = leaders(program, function);
        let windows = windows(program, function, &blocks);
        Emit {
            program,
            function,
            safepoint: helpers.safepoint,
            call: helpers.call,
            open: helpers.open,
            close: helpers.close,
            alloc: helpers.alloc,
            intrinsic: helpers.intrinsic,
            growable: helpers.growable,
            run_copy: helpers.run_copy,
            field_load: helpers.field_load,
            field_store: helpers.field_store,
            order_str: helpers.order_str,
            direct,
            pc: 0,
            code: Vec::new(),
            fixups: Vec::new(),
            labels: Vec::new(),
            block_at: vec![None; blocks.len()],
            blocks,
            windows,
            frame_live: false,
        }
    }

    fn run(mut self) -> Vec<u8> {
        self.prologue();
        let mut pc = 0;
        while pc < self.function.code.len() {
            if let Some(length) = self.blocks[pc] {
                self.block_at[pc] = Some(self.code.len());
                // A block is entered from anywhere, so nothing a predecessor
                // left in a register is readable here.
                self.frame_live = false;
                self.charge(length);
            }
            self.pc = pc;
            // A window's rows after its head begin no block, so skipping them
            // skips no charge: the block around the window was charged for them.
            if let Some(window) = self.windows[pc] {
                self.window(window);
                pc += window.window.rows;
                continue;
            }
            self.inst(pc);
            pc += 1;
        }
        self.patch();
        self.code
    }

    /// [`Entry`] received: `ctx` in `rdi`, `base` in `rsi`, `return_base` in
    /// `rdx` and `return_slot` in `ecx`.
    ///
    /// `base` is a word index, and the only thing this function ever wants from
    /// it is the byte offset, so the shift is paid once here rather than at
    /// every block that re-derives the frame pointer. The destination is the
    /// same: two indices arrive and one byte offset is kept, because
    /// [`Emit::ret`] wants the offset and nothing wants the pair.
    ///
    /// **Seven pushes for six registers.** Six would leave `rsp` 8 mod 16 at a
    /// `call`, and the System V ABI asks for 0; one more push is the cheapest
    /// way to say so. It is a second push of `RETURN_BYTES` *before* that
    /// register is written, so both of [`Emit::leave_answered`]'s pops restore
    /// the caller's value and neither has to be a pop into a register the
    /// outcome is in.
    fn prologue(&mut self) {
        for reg in [CTX, BASE_BYTES, WORK, FRAME, HEAP_SPARE, RETURN_BYTES] {
            self.push(reg);
        }
        self.push(RETURN_BYTES);
        self.mov_rr(CTX, RDI);
        self.mov_rr(BASE_BYTES, RSI);
        self.shl_imm8(BASE_BYTES, 3);
        // `return_slot` is a `u32` in `ecx` and the upper half of `rcx` is not
        // the caller's to promise, so it is zeroed rather than trusted: `mov
        // ecx, ecx` is the extension in two bytes.
        self.mov_rr32(RCX, RCX);
        self.mov_rr(RETURN_BYTES, RDX);
        self.add_rr(RETURN_BYTES, RCX);
        self.shl_imm8(RETURN_BYTES, 3);
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
            // `encoded.rs`'s `CONST_UNIT` arm: one store of a zero word. `xor`
            // rather than a `mov` of nought, which is what every other zero in
            // this file is.
            Inst::Unit { dst } => {
                self.xor_rr(RAX, RAX);
                self.store_slot(*dst, RAX);
            }
            Inst::Bool { dst, value } => {
                self.mov_imm64(RAX, i64::from(*value));
                self.store_slot(*dst, RAX);
            }
            Inst::Int { dst, value } => {
                self.mov_imm64(RAX, *value);
                self.store_slot(*dst, RAX);
            }
            // The same store `Inst::Int` makes, of a number the layout already
            // fixed: `encoded.rs` shares its `FUNC_REF | CONST_TAG` arm with
            // that one.
            Inst::Tag { dst, case, .. } => {
                self.mov_imm64(RAX, i64::from(case.0));
                self.store_slot(*dst, RAX);
            }
            Inst::Str { dst, text } => self.literal(*dst, *text),
            Inst::Copy { dst, src, layout } => {
                self.copy(*dst, *src, self.program.layout(*layout).width());
            }
            // `encoded.rs`'s `CLEAR` arm: `clear_words(base + slot, width)`. A
            // frame slot is a stack address by construction, so this is the
            // `is_stack` branch's stack arm with nothing to decide — one store of
            // a zero word per word of the layout.
            Inst::Clear { slot, layout } => {
                let width = self.program.layout(*layout).width();
                if width > 0 {
                    self.xor_rr(RAX, RAX);
                    for word in 0..width {
                        self.store_slot(slot + word, RAX);
                    }
                }
            }
            // `encoded.rs`'s `ADDR_OF_SLOT` arm: `base + slot`, where `base` is
            // the frame's *linear* address and not the byte offset this arm keeps
            // in `BASE_BYTES`. See [`Emit::frame_addr`].
            Inst::AddrOfSlot { dst, slot } => {
                self.frame_addr(RAX);
                self.add_imm32(RAX, *slot as i32);
                self.store_slot(*dst, RAX);
            }
            // `encoded.rs`'s `ADDR_OF_PART` arm, and the comment there is the
            // whole of it: "Arithmetic and nothing else."
            Inst::AddrOfPart { dst, addr, at } => {
                self.load_slot(RAX, *addr);
                self.add_imm32(RAX, *at as i32);
                self.store_slot(*dst, RAX);
            }
            Inst::Load { dst, addr, layout } => {
                self.load_through(*dst, *addr, self.program.layout(*layout).width());
            }
            Inst::Store { addr, src, layout } => {
                self.store_through(*addr, *src, self.program.layout(*layout).width());
            }
            Inst::LoadField {
                dst,
                obj,
                at,
                layout,
            } => {
                self.load_field(*dst, *obj, *at, self.program.layout(*layout).width());
            }
            Inst::StoreField {
                obj,
                at,
                src,
                layout,
            } => {
                self.store_field(*obj, *at, *src, self.program.layout(*layout).width());
            }
            // `encoded.rs`'s `NOT` arm tests the whole *word* against zero, not
            // the low byte, so that is what is tested here.
            Inst::Not { dst, a } => {
                self.load_slot(RAX, *a);
                self.test_rr(RAX, RAX);
                self.setcc(CC_E);
                self.movzx_eax_al();
                self.store_slot(*dst, RAX);
            }
            // `encoded.rs`'s `INT_TO_FLOAT` arm, `x as f64`: `cvtsi2sd` rounds to
            // nearest under the default `MXCSR`, which is what `as` does, and the
            // float's bits go back into the frame as the word they are.
            Inst::Convert {
                to: Convert::IntToFloat,
                dst,
                a,
            } => {
                self.load_slot(RAX, *a);
                self.cvtsi2sd_xmm0_rax();
                self.movq_rax_xmm0();
                self.store_slot(*dst, RAX);
            }
            // A relabel: the word moves unchanged.
            Inst::Convert {
                to: Convert::DurationToInt | Convert::IntToDuration,
                dst,
                a,
            } => {
                self.load_slot(RAX, *a);
                self.store_slot(*dst, RAX);
            }
            Inst::Len { dst, obj } => self.len_of(*dst, *obj),
            Inst::LoadElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.load_elem(*dst, *obj, *index, self.program.layout(*layout).width());
            }
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            } => {
                self.store_elem(*obj, *index, *src, self.program.layout(*layout).width());
            }
            Inst::RunLoad {
                dst,
                run,
                index,
                storage: Storage::PackedBytes,
            } => self.byte_at(*dst, *run, *index),
            Inst::Call { dst, callee, args } => self.callee(*dst, callee.0, args.0),
            // ADR 0052's four. An alloc, an extend and a finish are handed to the
            // runtime whole: see [`crate::abi::GrowableFn`] for why none of them
            // has an emitted fast path — one rooting discipline that is not the
            // frame's, one chunked safepoint contract, and one UTF-8 walk.
            Inst::GrowableAlloc {
                dst,
                capacity,
                storage: Storage::PackedBytes,
            } => self.growable_op(GrowableOp::Alloc, *dst, *capacity),
            // `core.vectorWithCapacity`, handed over the same way: the helper's
            // temporary root is what rules out emitting either allocation, and
            // the element layout it needs is on the instruction at this pc.
            Inst::GrowableAlloc {
                dst,
                capacity,
                storage: Storage::Words(_),
            } => self.growable_op(GrowableOp::AllocWords, *dst, *capacity),
            // `Vector.pop` and `Vector.remove`'s truncate, handed over whole.
            Inst::GrowableTruncate {
                owner,
                len,
                storage: Storage::Words(_),
            } => self.growable_op(GrowableOp::TruncateWords, *owner, *len),
            // ADR 0062's window: a room test, a bound test and an add, and a
            // byte blend, each with the helper as its cold half. See
            // [`Reserve`](crate::subset::Reserve) and
            // [`ByteStore`](crate::subset::ByteStore).
            Inst::GrowableEnsure {
                owner,
                additional,
                storage,
            } => {
                let reserve = reserve(self.program, *owner, *additional, *storage)
                    .expect("`supported` admitted an ensure it could decode");
                self.reserve(reserve, false)
            }
            Inst::GrowableCommit {
                owner,
                count,
                storage,
            } => {
                let reserve = reserve(self.program, *owner, *count, *storage)
                    .expect("`supported` admitted a commit it could decode");
                self.reserve(reserve, true)
            }
            Inst::RunStore {
                run,
                index,
                src,
                storage: Storage::PackedBytes,
            } => {
                let store = byte_store(self.program, *run, *index, *src)
                    .expect("`supported` admitted a byte store it could decode");
                self.byte_store(store)
            }
            Inst::RunFinish {
                dst,
                owner,
                storage: Storage::PackedBytes,
                ..
            } => self.growable_op(GrowableOp::Finish, *dst, *owner),
            // `Vector.freeze()`: the relabel emitted, and the whole finish as its
            // cold half. See [`WordFinish`](crate::subset::WordFinish).
            Inst::RunFinish {
                dst,
                owner,
                target,
                storage: Storage::Words(elem),
                ..
            } => {
                let finish = word_finish(self.program, *dst, *owner, *target, *elem)
                    .expect("`supported` admitted a word finish it could decode");
                self.vector_freeze(finish)
            }
            // ADR 0058's `run-copy`, handed to the runtime whole. See
            // [`crate::abi::RunCopyFn`] for why it has no emitted loop — memmove in
            // bounded chunks with a poll between them, and refusals whose
            // sentences only the runtime can build.
            Inst::RunCopy { args, storage } => match storage {
                Storage::PackedBytes => self.run_copy(args.0, RunOp::CopyBytes, 0),
                Storage::Words(elem) => self.run_copy(args.0, RunOp::CopyWords, elem.0),
            },
            // ADR 0058's `run-slice`: the same helper, which allocates the run
            // and writes it into the row's `dst` before it copies into it.
            Inst::RunSlice { args, storage } => match storage {
                Storage::PackedBytes => self.run_copy(args.0, RunOp::SliceBytes, 0),
                Storage::Words(elem) => self.run_copy(args.0, RunOp::SliceWords, elem.0),
            },
            Inst::Alloc { dst, layout, len } => self.allocate(*dst, layout.0, *len),
            Inst::Switch { on, table } => self.switch(*on, *table),
            // `encoded.rs`'s `NEG_INT` arm: `checked_neg`, whose `None` is
            // `overflowed("negation")`. `neg` sets the overflow flag for exactly
            // the one operand `checked_neg` answers `None` for — `i64::MIN`, whose
            // negation is not an `i64` — so the test is the flag and not a
            // comparison against a constant.
            Inst::Neg {
                num: Num::Int,
                dst,
                a,
            } => {
                self.load_slot(RAX, *a);
                self.neg_r(RAX);
                self.raise_unless(CC_NO, Raise::NegOverflowed);
                self.store_slot(*dst, RAX);
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
            // `encoded.rs`'s `ORDER_STR`, through the leaf helper. See
            // [`Emit::order_str`].
            Inst::Cmp {
                on: Compare::Str,
                op: CmpOp::Order,
                dst,
                a,
                b,
            } => self.order_str(*dst, *a, *b),
            // `encoded.rs`'s `ORDER_INT | ORDER_BOOL | ORDER_TAG`, which
            // `crate::subset` admits for those three and no other.
            Inst::Cmp {
                on: _,
                op: CmpOp::Order,
                dst,
                a,
                b,
            } => {
                self.load_slot(RAX, *a);
                self.load_slot(RCX, *b);
                self.order(*dst);
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
            // `encoded.rs`'s `INTRINSIC_CALL` arm, through the one helper. See
            // [`Emit::intrinsic_call`].
            Inst::IntrinsicCall { dst, site, args } => self.intrinsic_call(*dst, *site, *args),
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

    /// `encoded.rs`'s `STR` arm: `literal_addr(text)`, into a slot.
    ///
    /// Three instructions, and a `mov r64, imm64` is not one of them: see
    /// [`crate::abi`]'s "A literal's address is a run-time load" for why the
    /// address cannot be an immediate. The table is loaded from the context every
    /// time this arm runs, which is this arm being a template compiler — the
    /// pointer is a loop invariant of the whole run and there is nothing here to
    /// hoist it into.
    ///
    /// `RAX` survives [`Emit::store_slot`], whose only other write is [`FRAME`],
    /// so the address is formed before the frame pointer is and no spill is
    /// needed.
    fn literal(&mut self, dst: Slot, text: StrId) {
        let at = literal_offset(text).expect("`supported` bounded every literal");
        self.load(RAX, CTX, OFF_LITERALS);
        self.load(RAX, RAX, at);
        self.store_slot(dst, RAX);
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

    /// The *address* of the heap word at the linear address in `reg`, into
    /// [`HEAP_TABLE`].
    ///
    /// `Memory::read`'s heap half, which is `Space::load`: subtract the heap
    /// origin, find the chunk, and index inside it.
    ///
    /// **Eleven instructions, and the table is re-loaded from the context every
    /// time.** That is this arm being a template compiler rather than an
    /// oversight: the Cranelift arm computes the index once and loads the table
    /// once for all three words of a `load-elem` of a `Token`, because it has a
    /// value graph to common those loads out of, and this has a sequence of
    /// templates. The difference is real code and is one of the things the
    /// comparison is for.
    ///
    /// `reg` must not be one of the three heap scratch registers, which every
    /// caller below satisfies by using `RAX`, `RCX` or `RDX`.
    fn heap_ptr(&mut self, reg: u8) {
        debug_assert!(
            reg != HEAP_TABLE && reg != HEAP_INDEX && reg != HEAP_SPARE,
            "a heap address is formed in the scratch, so it cannot live in it"
        );
        // The index into the heap region, which is what the chunk spine is
        // addressed by. `HEAP_ORIGIN_WORDS` does not fit an `imm32`, so it is a
        // `movabs` and a register subtraction rather than a `sub imm32`.
        self.mov_rr(HEAP_INDEX, reg);
        self.mov_imm64(HEAP_SPARE, HEAP_ORIGIN_WORDS as i64);
        self.sub_rr(HEAP_INDEX, HEAP_SPARE);
        // `chunks[index >> HEAP_CHUNK_SHIFT]`, addressed by adds rather than by
        // a scaled-index `mov`: a `SIB` byte is a second addressing form and
        // this encoder has one.
        self.load(HEAP_TABLE, CTX, OFF_CHUNKS);
        self.mov_rr(HEAP_SPARE, HEAP_INDEX);
        self.shr_imm8(HEAP_SPARE, HEAP_CHUNK_SHIFT as u8);
        self.shl_imm8(HEAP_SPARE, 3);
        self.add_rr(HEAP_TABLE, HEAP_SPARE);
        self.load(HEAP_TABLE, HEAP_TABLE, 0);
        // And the word inside the chunk.
        self.and_imm32(HEAP_INDEX, (HEAP_CHUNK_WORDS - 1) as i32);
        self.shl_imm8(HEAP_INDEX, 3);
        self.add_rr(HEAP_TABLE, HEAP_INDEX);
    }

    /// The heap word at the linear address in `reg`, into `reg`.
    ///
    /// The `Relaxed` atomic load `Space::load` performs is a plain `mov` on
    /// x86-64.
    fn heap_word(&mut self, reg: u8) {
        self.heap_ptr(reg);
        self.load(reg, HEAP_TABLE, 0);
    }

    /// The *address* of the word at the linear address in `reg`, whichever region
    /// it names, into [`HEAP_TABLE`]. `reg` is clobbered.
    ///
    /// `Memory::read`'s and `Memory::write`'s shared first line —
    /// `is_stack(addr)`, which is `addr < HEAP_ORIGIN_WORDS` — emitted rather than
    /// called. See [`crate::abi`]'s "An address names either region" for why it is
    /// emitted: the two arms are five instructions and eleven, and a helper call
    /// would cost more than either *and* end the span in which a cached
    /// [`NativeCtx::words`] may be trusted.
    ///
    /// `HEAP_ORIGIN_WORDS` does not fit an `imm32`, so the comparison is a
    /// `movabs` and a `cmp` — the same pair [`Emit::heap_ptr`] opens with, and the
    /// duplication is this arm having templates rather than a value graph.
    fn word_ptr(&mut self, reg: u8) {
        let on_stack = self.label();
        let done = self.label();
        self.mov_imm64(HEAP_SPARE, HEAP_ORIGIN_WORDS as i64);
        self.cmp_rr(reg, HEAP_SPARE);
        self.jcc(CC_B, Target::Label(on_stack));
        self.heap_ptr(reg);
        self.jmp(Target::Label(done));
        self.bind(on_stack);
        // `Stack::at`'s subtraction and nothing else: `words[addr - origin]`. The
        // origin is re-read from the context rather than held in a register,
        // because only the address family reads it and a sixth long-lived register
        // would be a cost on every function that has no address in it.
        self.load(HEAP_TABLE, CTX, OFF_WORDS);
        self.load(HEAP_INDEX, CTX, OFF_STACK_ORIGIN);
        self.sub_rr(reg, HEAP_INDEX);
        self.shl_imm8(reg, 3);
        self.add_rr(HEAP_TABLE, reg);
        self.bind(done);
    }

    /// This frame's first word, as a **linear address**, into `reg`.
    ///
    /// What `encoded.rs` calls `base`, which is the number an `addr-of-slot` adds
    /// its slot to. This arm keeps the frame as a *byte* offset from the segment's
    /// first word, and the address is the segment's origin plus the word index, so
    /// the shift the prologue paid once is undone here — the one place that
    /// decision costs an instruction besides the two call sequences.
    ///
    /// It has to be the same number the VM would have formed, because the two
    /// tiers pass these words to each other: an `addr-of-slot` in compiled code
    /// becomes a `var` argument an encoded callee writes through.
    fn frame_addr(&mut self, reg: u8) {
        self.mov_rr(reg, BASE_BYTES);
        self.shr_imm8(reg, 3);
        self.load(HEAP_TABLE, CTX, OFF_STACK_ORIGIN);
        self.add_rr(reg, HEAP_TABLE);
    }

    /// `encoded.rs`'s `LOAD` arm: `copy_words(base + dst, addr, width)`.
    ///
    /// `RCX` holds the address for the whole template and `RDX` is where each
    /// word's own address is formed; the words wait on the machine stack, which is
    /// what [`Emit::copy`] has instead of sixteen free registers.
    ///
    /// Every word is read before any is written, for [`Emit::copy`]'s reason and
    /// with one more behind it: `copy_words` is a `memmove` where both runs are on
    /// the stack, and an address formed by `addr-of-slot` from *this* frame makes
    /// that case reachable — `load s3 <- &s1` with the runs overlapping is
    /// something the lowering may emit and does not have to prove it does not.
    fn load_through(&mut self, dst: Slot, addr: Slot, width: u32) {
        if width == 0 {
            return;
        }
        self.load_slot(RCX, addr);
        for word in 0..width {
            self.mov_rr(RDX, RCX);
            self.add_imm32(RDX, word as i32);
            self.word_ptr(RDX);
            self.load(RAX, HEAP_TABLE, 0);
            self.push(RAX);
        }
        for word in (0..width).rev() {
            self.pop(RAX);
            self.store_slot(dst + word, RAX);
        }
    }

    /// `encoded.rs`'s `STORE` arm: `copy_words(addr, base + src, width)`.
    ///
    /// [`Emit::load_through`]'s order, in the other direction and for the same
    /// reason. The address stays in `RCX` across the stores, which nothing in
    /// [`Emit::word_ptr`] touches.
    fn store_through(&mut self, addr: Slot, src: Slot, width: u32) {
        if width == 0 {
            return;
        }
        for word in 0..width {
            self.load_slot(RAX, src + word);
            self.push(RAX);
        }
        self.load_slot(RCX, addr);
        for word in (0..width).rev() {
            self.pop(RAX);
            self.mov_rr(RDX, RCX);
            self.add_imm32(RDX, word as i32);
            self.word_ptr(RDX);
            self.store(HEAP_TABLE, 0, RAX);
        }
    }

    /// `Memory::object_len`: the header's low half, in `reg`, as a non-negative
    /// `Int`.
    ///
    /// `mov r32, r32` is the mask. A 32-bit move zeroes the upper half of its
    /// destination, so this is the `as u32` and the `as i64` together in two
    /// bytes — and `and r64, imm32` could not have been, because `0xFFFFFFFF` as
    /// an `imm32` is sign-extended to `-1`.
    fn object_len(&mut self, reg: u8) {
        self.heap_word(reg);
        self.mov_rr32(reg, reg);
    }

    /// `encoded.rs`'s `ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT` arm: the runtime
    /// allocates, and this stores the address it answered.
    ///
    /// See [`crate::abi::AllocFn`] for why none of `Machine::allocate` is emitted
    /// and for what the zero answer means. What is emitted is the hand-over and the
    /// three things around it, and they are [`Emit::callee_mediated`]'s three:
    ///
    /// - **the unpaid work is published and the accumulator cleared**, because the
    ///   helper is a safepoint — ADR 0055 asks for one "around allocation or
    ///   runtime calls which may collect" — and the helper charges what it finds;
    /// - **the frame pointer is dropped**, because a collection may have grown the
    ///   stack and an allocation may have committed a heap chunk;
    /// - **a zero answer leaves**, as [`Raise::Called`]: the runtime is holding a
    ///   whole `RuntimeError` and this crate names errors rather than building
    ///   them.
    ///
    /// The length is formed into `RCX` **before** the three argument registers,
    /// because [`Emit::load_slot`] touches the frame and nothing else: `RDI`, `RSI`
    /// and `RDX` are written after it and nothing between them can disturb it.
    fn allocate(&mut self, dst: Slot, layout: u32, len: Len) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.xor_rr(WORK, WORK);

        match len {
            // `Len::Fixed`'s count is nought, which is what the encoded arm hands
            // `Machine::allocate` for it: the layout already fixes the size.
            Len::Fixed => self.mov_imm32(RCX, 0),
            Len::Count(count) => self.mov_imm64(RCX, i64::from(count)),
            // Read as a whole word and handed over as one. A negative count, or
            // one past what the header's length field holds, is the *helper's* to
            // refuse — see `Machine::allocate` — so narrowing it here would be a
            // second refusal with a different message.
            Len::Slot(at) => self.load_slot(RCX, at),
        }
        self.mov_rr(RDI, CTX);
        self.mov_imm32(RSI, self.pc as i32);
        self.mov_imm32(RDX, layout as i32);
        self.mov_imm64(RAX, self.alloc as i64);
        self.call(RAX);
        self.frame_live = false;

        // Zero is not an address — the heap begins at `HEAP_ORIGIN_WORDS` — so the
        // one test says both "it refused" and "the runtime has the sentence".
        self.test_rr(RAX, RAX);
        self.raise_unless(CC_NE, Raise::Called);
        self.store_slot(dst, RAX);
    }

    /// A `growable-ensure` or, when `commit`, a `growable-commit`: the owner read
    /// as [`Emit::byte_store`] reads its run, the room `capacity - length` compared
    /// unsigned with the count, and — for a commit — the new length written.
    ///
    /// See [`Reserve`](crate::subset::Reserve) for what is emitted and what goes
    /// to [`GrowableFn`](crate::abi::GrowableFn). `RAX` holds the owner until the
    /// room is known, `RCX` the store and then the count, and `RDX` the length;
    /// every comparison is `jb`, so the capacity is compared with the length
    /// before it is subtracted and a subtraction that would wrap is cold.
    fn reserve(&mut self, reserve: Reserve, commit: bool) {
        let Reserve {
            owner,
            count,
            layout,
            words,
        } = reserve;
        let cold = self.label();
        let done = self.label();

        self.load_slot(RAX, owner);
        self.refuse_null(RAX);

        // The owner layout, as the header's high half.
        self.mov_rr(RDX, RAX);
        self.heap_word(RDX);
        self.shr_imm8(RDX, 32);
        self.cmp_imm32(RDX, layout.0 as i32);
        self.jcc(CC_NE, Target::Label(cold));

        // `payload(owner, GROWABLE_STORE)`, which a finish leaves nought.
        self.mov_rr(RCX, RAX);
        self.add_imm32(RCX, 2);
        self.heap_word(RCX);
        self.test_rr(RCX, RCX);
        self.jcc(CC_E, Target::Label(cold));

        // `payload(owner, GROWABLE_LEN)`, the whole word.
        self.mov_rr(RDX, RAX);
        self.add_imm32(RDX, 1);
        self.heap_word(RDX);

        // `object_len(store)`: the capacity. Below the length is cold.
        self.mov_rr(RAX, RCX);
        self.object_len(RAX);
        self.cmp_rr(RAX, RDX);
        self.jcc(CC_B, Target::Label(cold));

        // The room, and the count against it, unsigned.
        self.sub_rr(RAX, RDX);
        self.load_slot(RCX, count);
        self.cmp_rr(RAX, RCX);
        self.jcc(CC_B, Target::Label(cold));

        if commit {
            // `growable_commit`: `set_payload(owner, GROWABLE_LEN, len + count)`.
            self.add_rr(RDX, RCX);
            self.load_slot(RAX, owner);
            self.add_imm32(RAX, 1);
            self.heap_ptr(RAX);
            self.store(HEAP_TABLE, 0, RDX);
        }
        self.jmp(Target::Label(done));

        self.bind(cold);
        let op = match (commit, words) {
            (false, false) => GrowableOp::EnsureBytes,
            (false, true) => GrowableOp::EnsureWords,
            (true, false) => GrowableOp::CommitBytes,
            (true, true) => GrowableOp::CommitWords,
        };
        self.growable_op(op, owner, count);
        self.bind(done);
        // One predecessor of this join came through a helper.
        self.frame_live = false;
    }

    /// A byte `run-store`: `RUN_STORE_BYTES`' checks and `Machine::blend`'s one
    /// byte, at the offset the instruction names rather than at a length.
    ///
    /// `RAX` holds the run and `RCX` the offset; the value waits on the machine
    /// stack while the word's address is formed, for [`Emit::store_elem`]'s
    /// reason — three scratch registers, and [`Emit::heap_ptr`] needs the other
    /// three.
    fn byte_store(&mut self, store: ByteStore) {
        let ByteStore {
            run,
            index,
            src,
            bytes,
        } = store;
        let cold = self.label();
        let done = self.label();

        self.load_slot(RAX, run);
        self.refuse_null(RAX);

        // `Shape::Bytes`, as the header's high half.
        self.mov_rr(RDX, RAX);
        self.heap_word(RDX);
        self.shr_imm8(RDX, 32);
        self.cmp_imm32(RDX, bytes.0 as i32);
        self.jcc(CC_NE, Target::Label(cold));

        // The offset against the header length, unsigned.
        self.load_slot(RCX, index);
        self.mov_rr(RDX, RAX);
        self.object_len(RDX);
        self.cmp_rr(RCX, RDX);
        self.jcc(CC_AE, Target::Label(cold));

        // `(0..=255).contains(&value)`.
        self.load_slot(RDX, src);
        self.cmp_imm32(RDX, 256);
        self.jcc(CC_AE, Target::Label(cold));
        self.push(RDX);

        // The payload word holding byte `index`: `run + 1 + index / 8`.
        self.mov_rr(RDX, RCX);
        self.shr_imm8(RDX, 3);
        self.add_rr(RAX, RDX);
        self.add_imm32(RAX, 1);
        self.heap_ptr(RAX);

        // `blend(run, index / 8, index % 8, 1, value)`. `shl` by a variable
        // amount reads `cl`.
        self.pop(RAX);
        self.and_imm32(RCX, 7);
        self.shl_imm8(RCX, 3);
        self.shl_cl(RAX);
        self.mov_imm32(HEAP_SPARE, 0xFF);
        self.shl_cl(HEAP_SPARE);
        self.not_r(HEAP_SPARE);
        self.load(HEAP_INDEX, HEAP_TABLE, 0);
        self.and_rr(HEAP_INDEX, HEAP_SPARE);
        self.or_rr(HEAP_INDEX, RAX);
        self.store(HEAP_TABLE, 0, HEAP_INDEX);
        self.jmp(Target::Label(done));

        self.bind(cold);
        self.growable_op(GrowableOp::StoreBytes, run, index);
        self.bind(done);
        self.frame_live = false;
    }

    /// An [ADR 0062] push or append window, as one fast path: the composite
    /// push's questions asked once, the frame writes of every row made, and the
    /// rows themselves as the cold half.
    ///
    /// See [`BufferWindow`] for which failure goes where and why the cold half
    /// rejoins at the store row. The shape:
    ///
    /// ```text
    ///   rax = the owner                  -- refused if null, at the head
    ///   the header's layout is `layout`, or cold_family
    ///   rdx = payload(owner, 0)          -- the length word; the rows' writes before the ensure
    /// retry:                             -- rax the owner, rdx the length word
    ///   rcx = payload(owner, 1)          -- nought is consumed, cold_room
    ///   rdx < len(store), or cold_room   -- an append: len <= cap and count <= cap - len
    ///   the rows' writes up to the write -- the store slot among them
    ///   push:   the unit's words, or a byte blend after its two checks (cold_store)
    ///           the rows' writes after it; payload(owner, 0) = rdx + 1
    ///   append: the run-copy helper; the rows' writes after it; the commit row
    /// cold_family: the head's load-field by the field helper; any constant row
    /// cold_room:   the ensure helper; rax and rdx read again; jmp retry
    /// cold_store:  the run-store helper; rdx read again; jmp to the push's tail
    /// ```
    ///
    /// **Every jump to a cold path is emitted before the first `push`**, for
    /// [`Emit::reserve`]'s reason, and each cold path re-derives the frame
    /// pointer before it jumps back, so the path that never went cold keeps the
    /// one it had.
    ///
    /// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    fn window(&mut self, held: BufferWindow) {
        use cove_ir::legalize::Pattern;
        let BufferWindow {
            window,
            layout,
            bytes,
            words,
            args,
        } = held;
        let cold_family = self.label();
        let cold_room = self.label();
        let cold_store = self.label();
        let retry = self.label();
        let tail = self.label();
        let done = self.label();

        // The head: `load-field at <- owner +0`, with the push's own checks.
        self.pc = window.head;
        self.load_slot(RAX, window.owner);
        self.refuse_null(RAX);
        self.mov_rr(RDX, RAX);
        self.heap_word(RDX);
        self.shr_imm8(RDX, 32);
        self.cmp_imm32(RDX, layout.0 as i32);
        self.jcc(CC_NE, Target::Label(cold_family));
        self.mov_rr(RDX, RAX);
        self.add_imm32(RDX, 1);
        self.heap_word(RDX);
        for write in window
            .frame_writes()
            .filter(|write| write.pc < window.ensure)
        {
            self.frame_write(write.slot, write.written, RCX);
        }

        // The store row, rejoined from the cold ensure.
        self.bind(retry);
        self.pc = window.load_store;
        self.mov_rr(RCX, RAX);
        self.add_imm32(RCX, 2);
        self.heap_word(RCX);
        self.test_rr(RCX, RCX);
        self.jcc(CC_E, Target::Label(cold_room));
        if words {
            // `vector_run`'s `len as u32`, which is also what makes the second
            // turn through here after a returned ensure answer yes.
            self.mov_rr32(RDX, RDX);
        }
        self.mov_rr(RAX, RCX);
        self.heap_word(RAX);
        match window.pattern {
            Pattern::PushWords | Pattern::PushByte => {
                // The store's family, for a byte push's own question below; the
                // heap scratch is free until the next address is formed.
                self.mov_rr(HEAP_INDEX, RAX);
                self.shr_imm8(HEAP_INDEX, 32);
                self.mov_rr32(RAX, RAX);
                self.cmp_rr(RDX, RAX);
                self.jcc(CC_AE, Target::Label(cold_room));
            }
            Pattern::AppendBytes | Pattern::AppendWords => {
                self.mov_rr32(RAX, RAX);
                self.cmp_rr(RAX, RDX);
                self.jcc(CC_B, Target::Label(cold_room));
                self.sub_rr(RAX, RDX);
                self.load_slot(RDX, window.count);
                self.cmp_rr(RAX, RDX);
                self.jcc(CC_B, Target::Label(cold_room));
            }
        }
        let between = |pc: usize| window.ensure < pc && pc < window.write;
        for write in window.frame_writes().filter(|write| between(write.pc)) {
            // `RAX` is spent: a push's capacity and an append's room are not
            // read again.
            self.frame_write(write.slot, write.written, RAX);
        }

        self.pc = window.write;
        match window.pattern {
            Pattern::PushWords => {
                // `store + 1 + len * stride`: the element's address in the store.
                self.mov_imm64(RAX, i64::from(window.stride));
                self.imul_rr(RAX, RDX);
                self.add_rr(RCX, RAX);
                self.add_imm32(RCX, 1);
                if window.stride > 0 {
                    self.push(RDX);
                    for word in 0..window.stride {
                        self.load_slot(RAX, window.src + word);
                        self.push(RAX);
                    }
                    for word in (0..window.stride).rev() {
                        self.pop(RAX);
                        self.mov_rr(RDX, RCX);
                        self.add_imm32(RDX, word as i32);
                        self.heap_ptr(RDX);
                        self.store(HEAP_TABLE, 0, RAX);
                    }
                    self.pop(RDX);
                }
                self.push_tail(window, done);
            }
            Pattern::PushByte => {
                // `RUN_STORE_BYTES`' two questions a live store with room leaves:
                // the store is a byte run, and the value is a byte.
                self.cmp_imm32(HEAP_INDEX, bytes.0 as i32);
                self.jcc(CC_NE, Target::Label(cold_store));
                // The payload word holding byte `len`: `store + 1 + len / 8`.
                self.add_imm32(RCX, 1);
                self.mov_rr(RAX, RDX);
                self.shr_imm8(RAX, 3);
                self.add_rr(RCX, RAX);
                self.heap_ptr(RCX);
                self.load_slot(RAX, window.src);
                self.cmp_imm32(RAX, 256);
                self.jcc(CC_AE, Target::Label(cold_store));
                // `blend(store, len / 8, len % 8, 1, value)`, one byte into its word.
                self.mov_rr(RCX, RDX);
                self.and_imm32(RCX, 7);
                self.shl_imm8(RCX, 3);
                self.shl_cl(RAX);
                self.mov_imm32(HEAP_SPARE, 0xFF);
                self.shl_cl(HEAP_SPARE);
                self.not_r(HEAP_SPARE);
                self.load(HEAP_INDEX, HEAP_TABLE, 0);
                self.and_rr(HEAP_INDEX, HEAP_SPARE);
                self.or_rr(HEAP_INDEX, RAX);
                self.store(HEAP_TABLE, 0, HEAP_INDEX);
                self.bind(tail);
                self.push_tail(window, done);
            }
            Pattern::AppendBytes | Pattern::AppendWords => {
                let (kind, elem) = match window.storage {
                    Storage::PackedBytes => (RunOp::CopyBytes, 0),
                    Storage::Words(elem) => (RunOp::CopyWords, elem.0),
                };
                self.run_copy(args, kind, elem);
                for write in window
                    .frame_writes()
                    .filter(|write| write.pc > window.write)
                {
                    self.frame_write(write.slot, write.written, RAX);
                }
                self.pc = window.commit;
                self.inst(window.commit);
                self.jmp(Target::Label(done));
            }
        }

        // A header of another family: the head as the field helper answers it —
        // `RAX` still holds the owner — and the constant rows before the ensure,
        // which refuses the owner.
        self.bind(cold_family);
        self.frame_live = false;
        self.pc = window.head;
        self.field_call(self.field_load, 0, 1, window.at);
        for pc in window.head + 1..window.ensure {
            self.pc = pc;
            self.inst(pc);
        }

        // No room, or no store: the ensure row's cold half, and the store row
        // again.
        self.bind(cold_room);
        self.frame_live = false;
        self.pc = window.ensure;
        let op = match words {
            true => GrowableOp::EnsureWords,
            false => GrowableOp::EnsureBytes,
        };
        self.growable_op(op, window.owner, window.count);
        self.load_slot(RAX, window.owner);
        self.load_slot(RDX, window.at);
        self.jmp(Target::Label(retry));

        if window.pattern == Pattern::PushByte {
            // The byte store row's cold half, which refuses; its store slot was
            // written before either jump here.
            self.bind(cold_store);
            self.frame_live = false;
            self.pc = window.write;
            self.growable_op(GrowableOp::StoreBytes, window.store, window.at);
            self.load_slot(RDX, window.at);
            self.jmp(Target::Label(tail));
        }

        self.bind(done);
        // Every predecessor but the fast one came through a helper.
        self.frame_live = false;
    }

    /// One frame write a window's row makes: the length the head read, which is
    /// in `RDX`; the store, in `RCX`; a constant or the nought a clear leaves,
    /// formed in `scratch`.
    fn frame_write(&mut self, slot: Slot, written: cove_ir::legalize::Written, scratch: u8) {
        use cove_ir::legalize::Written;
        match written {
            Written::Length => self.store_slot(slot, RDX),
            Written::Store => self.store_slot(slot, RCX),
            Written::Constant(value) => {
                self.mov_imm64(scratch, value);
                self.store_slot(slot, scratch);
            }
            Written::Cleared => {
                self.xor_rr(scratch, scratch);
                self.store_slot(slot, scratch);
            }
        }
    }

    /// A push window's rows after its write — a clear of the store slot and a
    /// second constant, where the window has them — and its commit, which the
    /// room the fast path found makes one store of `RDX + 1`.
    fn push_tail(&mut self, window: cove_ir::legalize::Window, done: usize) {
        for write in window
            .frame_writes()
            .filter(|write| write.pc > window.write)
        {
            self.frame_write(write.slot, write.written, RAX);
        }
        self.pc = window.commit;
        self.add_imm32(RDX, 1);
        self.load_slot(RAX, window.owner);
        self.add_imm32(RAX, 1);
        self.heap_ptr(RAX);
        self.store(HEAP_TABLE, 0, RDX);
        self.jmp(Target::Label(done));
    }

    /// A word `run-finish` — `Vector.freeze()` — as `Memory::relabel` turning
    /// the store into the `Array<T>` it already holds, in place, and the two
    /// words of the `Vector` header that mark it consumed.
    ///
    /// See [`WordFinish`](crate::subset::WordFinish) for which preconditions are
    /// emitted and which go to [`GrowableFn`](crate::abi::GrowableFn) —
    /// [`Emit::reserve`]'s own two —
    /// and for why there is no *third* cold half: `relabel` is O(1) whatever
    /// `len` and `capacity` are, so once both preconditions hold, every
    /// remaining step is unconditional.
    ///
    /// **The owner's address is reloaded from the frame wherever it is needed**
    /// rather than kept live across the run, which is this arm having three
    /// scratch registers and more than three addresses to have used —
    /// `header`, `store`, and the two write targets `heap_ptr` forms from them.
    /// Nothing between the two precondition checks and the end can raise or
    /// call, so reloading costs a few bytes of code and never a wrong answer:
    /// nothing in the heap moves underneath it.
    ///
    /// `spare` is computed as `capacity * stride - len * stride` rather than
    /// as `(capacity - len) * stride`, which is the same number
    /// `Machine::relabel`'s wrapper computes and one fewer register in the
    /// middle of it.
    fn vector_freeze(&mut self, finish: WordFinish) {
        let WordFinish {
            dst,
            owner: recv,
            vector,
            stride,
            array,
        } = finish;
        let cold = self.label();
        let after_spare = self.label();
        let done = self.label();

        self.load_slot(RAX, recv);
        // `vector()`'s `if addr == 0 { null_value() }`, which is the one
        // refusal of this builtin a program reaches and this crate can name.
        self.refuse_null(RAX);

        // `machine.object_layout(addr)`: the header's high half.
        self.mov_rr(RDX, RAX);
        self.heap_word(RDX);
        self.shr_imm8(RDX, 32);
        self.cmp_imm32(RDX, vector.0 as i32);
        self.jcc(CC_NE, Target::Label(cold));

        // `machine.payload(addr, 1)`: the store. A second `freeze()` leaves
        // nought here.
        self.mov_rr(RCX, RAX);
        self.add_imm32(RCX, 2);
        self.heap_word(RCX);
        self.test_rr(RCX, RCX);
        self.jcc(CC_E, Target::Label(cold));

        // `items.len` and `items.capacity`, `Emit::reserve`'s own reads.
        self.load_slot(RDX, recv);
        self.add_imm32(RDX, 1);
        self.heap_word(RDX);
        self.mov_rr32(RDX, RDX); // RDX = len
        self.load_slot(RCX, recv);
        self.add_imm32(RCX, 2);
        self.heap_word(RCX);
        self.object_len(RCX); // RCX = capacity

        // `payload = len * stride`, and `spare = capacity * stride - payload`
        // — this method's own note on why it is not `(capacity - len) * stride`.
        self.mov_imm64(RAX, i64::from(stride));
        self.imul_rr(RDX, RAX); // RDX = payload
        self.imul_rr(RCX, RAX); // RCX = capacity * stride
        self.sub_rr(RCX, RDX); // RCX = spare (words)
        self.push(RCX); // [spare]
        self.push(RDX); // [spare, payload]

        // `len`, once more, to build the new header word: `(array << 32) |
        // len` — the array's bits and the length's do not overlap, so the OR
        // `mem::header` performs is an add.
        self.load_slot(RAX, recv);
        self.add_imm32(RAX, 1);
        self.heap_word(RAX);
        self.mov_rr32(RAX, RAX); // RAX = len
        self.mov_imm64(RCX, i64::from(array.0));
        self.shl_imm8(RCX, 32);
        self.add_rr(RCX, RAX); // RCX = new header word

        // `self.write(addr, header(layout, len))`, and the answer — `dst =
        // store` — read off the same address before it is spent on the write.
        self.load_slot(RDX, recv);
        self.add_imm32(RDX, 2);
        self.heap_word(RDX); // RDX = store
        self.store_slot(dst, RDX);
        self.heap_ptr(RDX);
        self.store(HEAP_TABLE, 0, RCX);

        // `if spare > 0 { self.write(addr + 1 + payload, header(FREE, spare -
        // 1)) }`. `RDX` is still `store`: `heap_ptr` and `store` above read it
        // and neither writes it.
        self.pop(RCX); // RCX = payload ; [spare]
        self.add_rr(RDX, RCX);
        self.add_imm32(RDX, 1); // RDX = store + 1 + payload
        self.pop(RCX); // RCX = spare ; []
        self.test_rr(RCX, RCX);
        self.jcc(CC_E, Target::Label(after_spare));
        // `LayoutId::FREE` is `0`, so that header word is `spare - 1` alone.
        self.add_imm32(RCX, -1);
        self.heap_ptr(RDX);
        self.store(HEAP_TABLE, 0, RCX);
        self.bind(after_spare);

        // `machine.set_payload(items.header, 0, 0)` and `(items.header, 1,
        // 0)`: the `Vector`'s own two words, cleared — `freeze()`'s mark, the
        // same nought `vector()` refuses a later push or set against.
        self.load_slot(RAX, recv);
        self.xor_rr(RCX, RCX);
        self.mov_rr(RDX, RAX);
        self.add_imm32(RDX, 1);
        self.heap_ptr(RDX);
        self.store(HEAP_TABLE, 0, RCX);
        self.mov_rr(RDX, RAX);
        self.add_imm32(RDX, 2);
        self.heap_ptr(RDX);
        self.store(HEAP_TABLE, 0, RCX);
        self.jmp(Target::Label(done));

        self.bind(cold);
        self.growable_op(GrowableOp::FinishWords, dst, recv);
        self.bind(done);
        // One predecessor of this join came through a helper, so the frame
        // pointer the other one derived is not to be trusted here.
        self.frame_live = false;
    }

    /// One of [ADR 0052]'s four growable-buffer instructions, handed to the
    /// runtime whole.
    ///
    /// [`Emit::callee_mediated`]'s shape — six registers, the same shift back to
    /// a word index, the same test of the outcome — with the operand pair and
    /// the operation in place of the callee and its argument list. See
    /// [`crate::abi::GrowableFn`] for what each operand means and, more to the
    /// point, why *all* of it is the helper rather than a fast path and a cold
    /// one.
    ///
    /// It is a safepoint and it is one for a reason each of the four has: an
    /// `growable-alloc` allocates twice, an `append` may grow the store, and a
    /// `finish` walks the live prefix and charges the bulk work it did. The
    /// unpaid work is published and the accumulator cleared before the call, and
    /// the frame pointer is dropped after it, because a collection may have grown
    /// the stack and an allocation may have committed a heap chunk.
    ///
    /// [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
    fn growable_op(&mut self, op: GrowableOp, a: u32, b: u32) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.xor_rr(WORK, WORK);

        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, BASE_BYTES);
        self.shr_imm8(RSI, 3);
        self.mov_imm32(RDX, self.pc as i32);
        self.mov_imm32(RCX, op.abi() as i32);
        self.mov_imm32(R8, a as i32);
        self.mov_imm32(R9, b as i32);
        self.mov_imm64(RAX, self.growable as i64);
        self.call(RAX);

        // Anything but `Returned` leaves, and leaves with that outcome: the helper
        // has already written every field it needs.
        let on = self.label();
        self.test_rr32(RAX, RAX);
        self.jcc(CC_E, Target::Label(on));
        self.leave_answered();
        self.bind(on);
        self.frame_live = false;
    }

    /// One [ADR 0058] `run-copy` or `run-slice`, handed to the runtime whole.
    ///
    /// [`Emit::growable_op`]'s shape exactly — the same six registers, the same
    /// shift back to a word index, the same test of the outcome — with the
    /// argument list, the [`RunOp`] and the element in place of the operation and
    /// its pair. See [`crate::abi::RunCopyFn`] for what the operands mean and why
    /// the copy is the helper's.
    ///
    /// It is a safepoint: the helper takes one before the copy, a slice
    /// allocates, and a long copy polls between chunks, any of which may collect.
    /// So the unpaid work is published before the call and the frame pointer
    /// dropped after it.
    ///
    /// [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    fn run_copy(&mut self, args: u32, kind: RunOp, elem: u32) {
        let words = kind.abi() as i32;
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.xor_rr(WORK, WORK);

        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, BASE_BYTES);
        self.shr_imm8(RSI, 3);
        self.mov_imm32(RDX, self.pc as i32);
        self.mov_imm32(RCX, args as i32);
        self.mov_imm32(R8, words);
        self.mov_imm32(R9, elem as i32);
        self.mov_imm64(RAX, self.run_copy as i64);
        self.call(RAX);

        // Anything but `Returned` leaves, and leaves with that outcome: the helper
        // has already written every field it needs.
        let on = self.label();
        self.test_rr32(RAX, RAX);
        self.jcc(CC_E, Target::Label(on));
        self.leave_answered();
        self.bind(on);
        self.frame_live = false;
    }

    /// One `intrinsic-call`, handed to [`IntrinsicFn`](crate::abi::IntrinsicFn)
    /// with the protocol its effects ask for.
    ///
    /// [`Emit::growable_op`]'s six registers, with the destination, the site and
    /// the argument list in place of the operation and its pair — and everything
    /// around the call read off one [`IntrinsicProtocol`], which is the whole of
    /// this arm's decision:
    ///
    /// - a **safepoint** publishes and clears [`WORK`] before the call and drops the
    ///   frame pointer after it, exactly as [`Emit::growable_op`] does;
    /// - an intrinsic that **cannot collect** does neither: the helper charges
    ///   nothing, so the work stays in [`WORK`], and it can neither grow the stack
    ///   nor commit a chunk, so [`FRAME`] — callee-saved — is still the frame;
    /// - the outcome is tested only where [`IntrinsicProtocol::tests_outcome`] says
    ///   an answer other than `Returned` can come back, and an exit that did not
    ///   publish before the call publishes on the way out, for
    ///   [`Emit::field_call`]'s reason.
    fn intrinsic_call(&mut self, dst: Slot, site: cove_ir::SiteId, args: ArgsId) {
        let protocol = IntrinsicProtocol::of(self.program.intrinsic_site(site).intrinsic);
        if protocol.safepoint {
            self.store(CTX, OFF_PENDING_WORK, WORK);
            self.xor_rr(WORK, WORK);
        }

        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, BASE_BYTES);
        self.shr_imm8(RSI, 3);
        self.mov_imm32(RDX, self.pc as i32);
        self.mov_imm32(RCX, dst as i32);
        self.mov_imm32(R8, site.0 as i32);
        self.mov_imm32(R9, args.0 as i32);
        self.mov_imm64(RAX, self.intrinsic as i64);
        self.call(RAX);

        if protocol.tests_outcome() {
            let on = self.label();
            self.test_rr32(RAX, RAX);
            self.jcc(CC_E, Target::Label(on));
            if !protocol.safepoint {
                // `RAX` holds the outcome `leave_answered` returns: one store of
                // `WORK` and nothing else.
                self.store(CTX, OFF_PENDING_WORK, WORK);
            }
            self.leave_answered();
            self.bind(on);
        }
        if protocol.safepoint {
            self.frame_live = false;
        }
    }

    /// `encoded.rs`'s `LEN` arm, whole: the null refusal and the header's low
    /// half.
    ///
    /// A method of its own because a `String.byteLength()` builtin used to reach
    /// it too. That method is now `std.string` over ADR 0058's
    /// `core.byteLength`, which lowers to [`Inst::Len`](cove_ir::Inst::Len)
    /// before this crate sees it — so this code generator no longer names the
    /// public method at all, which is what the ADR asks of it.
    fn len_of(&mut self, dst: Slot, obj: Slot) {
        self.load_slot(RAX, obj);
        self.refuse_null(RAX);
        self.object_len(RAX);
        self.store_slot(dst, RAX);
    }

    /// Refuses a null reference, which every reader of an object does first.
    ///
    /// `Machine::element`, `encoded.rs`'s `LEN` and its `RUN_LOAD_BYTES` each begin
    /// with `if addr == 0` and each answers `null_object()`.
    fn refuse_null(&mut self, reg: u8) {
        self.test_rr(reg, reg);
        self.raise_unless(CC_NE, Raise::NullObject);
    }

    /// Raises unless `a` is below `b` as an unsigned comparison, carrying both
    /// numbers out for the message.
    ///
    /// One comparison for `Machine::element`'s `at < 0 || at >= len`, and it is
    /// exact rather than clever: a negative `i64` read as unsigned is larger
    /// than any length, and a length is a `u32` masked out of a header.
    fn raise_unless_below(&mut self, code: Raise, a: u8, b: u8) {
        let fine = self.label();
        self.cmp_rr(a, b);
        self.jcc(CC_B, Target::Label(fine));
        self.store(CTX, OFF_RAISE_A, a);
        self.store(CTX, OFF_RAISE_B, b);
        self.raise(code, 0);
        self.bind(fine);
    }

    /// `encoded.rs`'s `LOAD_ELEM` arm: `Machine::element`, and then a copy of
    /// `width` words out of the payload.
    ///
    /// `RAX` holds the object for the whole template and `RCX` the element's
    /// payload offset; `RDX` is where each word is formed. No load is held back
    /// as [`Emit::copy`] holds them back, because the source is the heap and the
    /// destination is the frame: two regions, so there is nothing to overlap.
    fn load_elem(&mut self, dst: Slot, obj: Slot, index: Slot, width: u32) {
        self.load_slot(RAX, obj);
        self.refuse_null(RAX);
        self.load_slot(RCX, index);
        self.mov_rr(RDX, RAX);
        self.object_len(RDX);
        self.raise_unless_below(Raise::IndexOutOfRange, RCX, RDX);
        // The stride, which is what makes an `Array<Point>` a run of two-word
        // elements. `Machine::element` multiplies in `u32`; this multiplies in
        // `u64`, which agrees on every product the check above admits.
        self.mov_imm64(RDX, i64::from(width));
        self.imul_rr(RCX, RDX);
        for word in 0..width {
            self.mov_rr(RDX, RAX);
            self.add_rr(RDX, RCX);
            // The header is one word, so a payload word is one past it.
            self.add_imm32(RDX, 1 + word as i32);
            self.heap_word(RDX);
            self.store_slot(dst + word, RDX);
        }
    }

    /// `encoded.rs`'s `STORE_ELEM` arm: `Machine::element`, and then a copy of
    /// `width` words *into* the payload.
    ///
    /// [`Emit::load_elem`] backwards, and the bounds arithmetic is the same
    /// arithmetic — one unsigned comparison, and a stride that is the element
    /// layout's width. Two things differ.
    ///
    /// **The element's address is formed once and the object is then dead.** The
    /// object's register is needed for the words on the way in, and a linear
    /// address plus an offset is all the loop wants.
    ///
    /// **The words wait on the machine stack.** `RAX` is the word, `RCX` the
    /// element's base address and `RDX` where each word's own address is formed —
    /// three, and [`Emit::heap_ptr`] needs the other three — so a fourth live value
    /// goes where [`Emit::copy`]'s go. Nothing is *held back* for overlap the way a
    /// copy holds words back: the source is a frame and the destination is the
    /// heap, which are two regions.
    fn store_elem(&mut self, obj: Slot, index: Slot, src: Slot, width: u32) {
        self.load_slot(RAX, obj);
        self.refuse_null(RAX);
        self.load_slot(RCX, index);
        self.mov_rr(RDX, RAX);
        self.object_len(RDX);
        self.raise_unless_below(Raise::IndexOutOfRange, RCX, RDX);
        // The stride. `Machine::element` multiplies in `u32`; this multiplies in
        // `u64`, which agrees on every product the check above admits.
        self.mov_imm64(RDX, i64::from(width));
        self.imul_rr(RCX, RDX);
        self.add_rr(RCX, RAX);
        // The header is one word, so a payload word is one past it.
        self.add_imm32(RCX, 1);
        for word in 0..width {
            self.load_slot(RAX, src + word);
            self.push(RAX);
        }
        for word in (0..width).rev() {
            self.pop(RAX);
            self.mov_rr(RDX, RCX);
            self.add_imm32(RDX, word as i32);
            self.heap_ptr(RDX);
            self.store(HEAP_TABLE, 0, RAX);
        }
    }

    /// `encoded.rs`'s `RUN_LOAD_BYTES` arm: a payload read, a shift and a mask.
    ///
    /// The bound is the string's *byte* length and the refusal is not
    /// `Array.get`'s — see [`Inst::RunLoad`](cove_ir::Inst::RunLoad) for why a
    /// byte offset out of range stops the run rather than answering an `Option`.
    fn byte_at(&mut self, dst: Slot, obj: Slot, at: Slot) {
        self.load_slot(RAX, obj);
        self.refuse_null(RAX);
        self.load_slot(RCX, at);
        self.mov_rr(RDX, RAX);
        self.object_len(RDX);
        self.raise_unless_below(Raise::ByteOffset, RCX, RDX);
        // Eight bytes to a word, least-significant byte first, so the word is
        // `at / 8` and the byte inside it is `at % 8`.
        self.mov_rr(RDX, RCX);
        self.shr_imm8(RDX, 3);
        self.add_imm32(RDX, 1);
        self.add_rr(RDX, RAX);
        self.heap_word(RDX);
        // `shr` by a variable amount reads `cl` and nothing else, which is why
        // the offset was left in `RCX`.
        self.and_imm32(RCX, 7);
        self.shl_imm8(RCX, 3);
        self.shr_cl(RDX);
        self.and_imm32(RDX, 0xFF);
        self.store_slot(dst, RDX);
    }

    /// `NativeCtx::fixed_payload_words[layout]`, where `layout` is `reg`'s
    /// header's high half on entry — `0` for a variable-payload shape.
    ///
    /// `reg` holds the object's linear address on entry and the table's answer
    /// on exit; it must not be `RCX`, which this uses as scratch for the table
    /// pointer — deliberately not `RAX`, so that a caller keeping the address
    /// alive in `RAX` across this call still has it afterwards.
    fn fixed_payload_words(&mut self, reg: u8) {
        debug_assert!(
            reg != RCX,
            "`RCX` is this method's own scratch for the table pointer"
        );
        self.heap_word(reg);
        self.shr_imm8(reg, 32);
        // Four bytes per `u32` entry, not eight: this table is not `chunks` or
        // `literals`.
        self.shl_imm8(reg, 2);
        // `RCX`, not `RAX`: every caller keeps the object's address in `RAX`
        // across this call, for the fast-path copy or for `Emit::field_call`
        // afterwards, and clobbering it here would lose it.
        self.load(RCX, CTX, OFF_FIXED_PAYLOAD_WORDS);
        self.add_rr(reg, RCX);
        self.load32(reg, reg, 0);
    }

    /// `encoded.rs`'s `LOAD_FIELD` arm: `Machine::checked` and a copy of `width`
    /// words out of the object's payload — with the bound answered in emitted
    /// code wherever [`Emit::fixed_payload_words`] can answer it.
    ///
    /// `at` and `width` are compile-time constants, so this is one `cmp` against
    /// an immediate — `Machine::checked`'s `at + width > words` turned around —
    /// rather than a run-time addition. A `0` table entry is below any non-zero
    /// `at + width`, which is what sends a variable-payload object to
    /// [`Emit::field_call`]'s helper without this arm ever asking which shape it
    /// is.
    fn load_field(&mut self, dst: Slot, obj: Slot, at: u32, width: u32) {
        let cold = self.label();
        let done = self.label();

        self.load_slot(RAX, obj);
        self.refuse_null(RAX);

        self.mov_rr(RDX, RAX);
        self.fixed_payload_words(RDX);
        self.cmp_imm32(RDX, (at + width) as i32);
        self.jcc(CC_B, Target::Label(cold));

        for word in 0..width {
            self.mov_rr(RDX, RAX);
            self.add_imm32(RDX, (1 + at + word) as i32);
            self.heap_word(RDX);
            self.store_slot(dst + word, RDX);
        }
        self.jmp(Target::Label(done));

        self.bind(cold);
        self.field_call(self.field_load, at, width, dst);
        self.bind(done);
        self.frame_live = false;
    }

    /// [`Emit::load_field`], the other direction: `encoded.rs`'s `STORE_FIELD`
    /// arm.
    fn store_field(&mut self, obj: Slot, at: u32, src: Slot, width: u32) {
        let cold = self.label();
        let done = self.label();

        self.load_slot(RAX, obj);
        self.refuse_null(RAX);

        self.mov_rr(RDX, RAX);
        self.fixed_payload_words(RDX);
        self.cmp_imm32(RDX, (at + width) as i32);
        self.jcc(CC_B, Target::Label(cold));

        for word in 0..width {
            self.load_slot(RDX, src + word);
            self.mov_rr(RCX, RAX);
            self.add_imm32(RCX, (1 + at + word) as i32);
            self.heap_ptr(RCX);
            self.store(HEAP_TABLE, 0, RDX);
        }
        self.jmp(Target::Label(done));

        self.bind(cold);
        self.field_call(self.field_store, at, width, src);
        self.bind(done);
        self.frame_live = false;
    }

    /// One [`crate::abi::FieldLoadFn`]/[`crate::abi::FieldStoreFn`] call, handed
    /// to the runtime whole. [`Emit::growable_op`]'s shape, with no safepoint
    /// discipline around it — neither helper can allocate, so there is no unpaid
    /// work to publish.
    ///
    /// `RAX` must hold the object's linear address on entry — every caller's
    /// fast-path check leaves it there — and `slot` is the frame slot the words
    /// are copied to or from. `Emit::frame_addr` forms that address into `R9`
    /// **before** `RDI`/`RSI` are clobbered, because it is itself built out of
    /// the heap scratch trio; and `RAX` moves into `RDX` before `RAX` is spent on
    /// the callee's own address, because both name the same register.
    fn field_call(&mut self, target: usize, at: u32, width: u32, slot: u32) {
        self.frame_addr(R9);
        self.add_imm32(R9, slot as i32);
        self.mov_rr(RDX, RAX);
        self.mov_rr(RDI, CTX);
        self.mov_imm32(RSI, self.pc as i32);
        self.mov_imm32(RCX, at as i32);
        self.mov_imm32(R8, width as i32);
        self.mov_imm64(RAX, target as i64);
        self.call(RAX);

        // Anything but `Returned` leaves, and leaves with that outcome: the helper
        // has already written every field it needs.
        let on = self.label();
        self.test_rr32(RAX, RAX);
        self.jcc(CC_E, Target::Label(on));
        // This frame's unpaid work, published before leaving. `native::call`
        // charges `pending_work` "on every exit — a return, a raise and a stop
        // alike", and [`Emit::leave_answered`] does **not** publish it: it pops
        // the caller's [`WORK`] back, which is a different thing entirely. Its
        // other caller [`Emit::callee`] needs no store here because
        // [`Emit::callee_direct`] published and cleared before the call;
        // [`Emit::allocate`] and [`Emit::growable_op`] do the same. This one
        // cannot, because a field helper is deliberately **not** a safepoint —
        // neither [`crate::abi::FieldLoadFn`] nor [`crate::abi::FieldStoreFn`]
        // can allocate — so publishing early would put a charge where there is no
        // safepoint. Without this store the whole block's work is never charged
        // at all, and ADR 0040's `S + T` bound is computed from a short number.
        //
        // `RAX` holds the outcome that [`Emit::leave_answered`] returns, so this
        // may not touch it: one store of [`WORK`] and nothing else.
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.leave_answered();
        self.bind(on);
        self.frame_live = false;
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
            CmpOp::Order => unreachable!("a three-way order is lowered by `Emit::order`"),
        };
        self.cmp_rr(RAX, RCX);
        self.setcc(cc);
        self.movzx_eax_al();
        self.store_slot(dst, RAX);
        // What the fused form branches on, and what `BranchFalse` would have
        // set for itself: the stored word against zero.
        self.test_rr(RAX, RAX);
    }

    /// `encoded.rs`'s `ORDER_INT`: `(x > y) - (x < y)` over the two words as
    /// `i64`, stored as the `Int` `-1`, `0` or `1`.
    ///
    /// Both `setcc`s read the flags one `cmp` left, the two bytes are widened
    /// into whole registers, and the subtraction is over sixty-four bits, so a
    /// `0 - 1` is all ones rather than `255`.
    /// `encoded.rs`'s `ORDER_STR`: one call of the leaf
    /// [`crate::abi::OrderStrFn`], its answer stored whole.
    ///
    /// The System V argument registers are `RDI`, `RSI` and `RDX`, and
    /// [`Emit::load_slot`] writes only its target and [`FRAME`], so both words are
    /// loaded straight into place. Because the helper is a leaf nothing is
    /// published before the call — [`WORK`] keeps accumulating — and nothing is
    /// re-derived after it: [`FRAME`] is `R14`, which the callee preserves, and
    /// the helper cannot have moved the segment it points into, so the frame
    /// stays live. The heap scratch registers the call clobbers are loaded fresh
    /// by every heap access anyway.
    fn order_str(&mut self, dst: Slot, a: Slot, b: Slot) {
        self.load_slot(RSI, a);
        self.load_slot(RDX, b);
        self.mov_rr(RDI, CTX);
        self.mov_imm64(RAX, self.order_str as i64);
        self.call(RAX);
        self.store_slot(dst, RAX);
    }

    fn order(&mut self, dst: Slot) {
        self.cmp_rr(RAX, RCX);
        self.setcc(CC_G);
        self.setcc_cl(CC_L);
        self.movzx_eax_al();
        self.movzx_ecx_cl();
        self.sub_rr(RAX, RCX);
        self.store_slot(dst, RAX);
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

    /// A poll: test the stride, and only then hand the runtime the unpaid work.
    ///
    /// Emitted on every backedge, with the same accumulated static work count
    /// the Cranelift arm hands the same helper, so the two arms pay the same
    /// runtime cost at the same places.
    ///
    /// Two instructions stand in front of the hand-over and they are the whole
    /// of what a turn of a loop pays when the poll is not due:
    ///
    /// ```text
    /// cmp r13, [rbx + poll_at]   ; the accumulator against the threshold
    /// jb  through                ; below it: nothing is due, carry on
    /// ```
    ///
    /// One compare with a memory operand rather than a load into a scratch
    /// register and a register compare, because the threshold is read once and
    /// this arm has the encoding — [`Emit::cmp_r_mem`]. So no scratch register
    /// is touched at all, and the flags the caller left have already been spent
    /// by [`Emit::branch_when_false`]'s own `jcc` before this is reached.
    ///
    /// ADR 0060 is the decision and [`NativeCtx::poll_at`] is the contract: the
    /// threshold is what is *left* of the machine's stride, so this compare is
    /// `encoded::dispatch`'s `work() - charged_work >= SAFEPOINT_STRIDE` asked
    /// in the coordinate compiled code keeps.
    fn safepoint(&mut self, pc: u32) {
        let through = self.label();
        self.cmp_r_mem(WORK, CTX, OFF_POLL_AT);
        self.jcc(CC_B, Target::Label(through));
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
        // Reached both ways from here on: the poll that was not due jumps
        // straight to this label with its accumulator untouched, which is what
        // makes the next turn's test a test of the accumulated total.
        self.bind(through);
        // The helper is allowed to have grown the stack, so the frame pointer
        // derived before the call is not to be used after it. `through` is
        // bound above rather than below this line on purpose: a fall-through
        // that skipped the call did not invalidate the frame pointer, but the
        // two paths merge here and only one of them may be assumed.
        self.frame_live = false;
    }

    /// `encoded.rs`'s `SWITCH` arm:
    /// `targets.get(index).unwrap_or(&default)`.
    ///
    /// A compare chain, which is what a template compiler has: there is no jump
    /// table here, and building one would be choosing between two encodings from
    /// the shape of the table, which is the peephole this arm does not have. See
    /// `compile.rs`'s `switch` for the `br_table` the other arm emits, and the
    /// harness's report for what the difference measured.
    ///
    /// It needs no range check, and that is a property of the comparison rather
    /// than an omission: `cmp r64, imm32` compares the whole word against a small
    /// non-negative case index, so a word larger than any case — including one
    /// above `u32::MAX`, which is what forces the other arm's check — equals none
    /// of them and falls through to the default.
    fn switch(&mut self, on: Slot, table: cove_ir::TableId) {
        let table = self.program.table(table);
        let targets: Vec<u32> = table.targets.clone();
        let default = table.default;
        self.load_slot(RAX, on);
        for (case, target) in targets.iter().enumerate() {
            self.cmp_imm32(RAX, case as i32);
            self.jcc(CC_E, Target::Pc(*target));
        }
        self.jmp(Target::Pc(default));
    }

    /// [`Inst::Call`](cove_ir::Inst::Call), one of two ways.
    ///
    /// [`Emit::callee_direct`] where this generator was asked for direct calls
    /// and the call site admits one, and [`Emit::callee_mediated`] otherwise.
    /// Which one a *particular call* takes at run time is a third question, and
    /// the runtime answers it: a callee with no compiled code is reached through
    /// the mediated helper whatever this emitted.
    fn callee(&mut self, dst: Slot, callee: u32, args: u32) {
        if self.direct && self.direct_admits(callee, args) {
            self.callee_direct(dst, callee, args);
        } else {
            self.callee_mediated(dst, callee, args);
        }
    }

    /// Whether a direct call can be emitted for this call site.
    ///
    /// Two conditions, and both are about what emitted code has to know
    /// *statically* to copy the arguments itself:
    ///
    /// - the arity matches. A mismatch is a lowering bug, and the runtime has
    ///   the sentence for it — `wrong_arity` — inside `open_frame`, which only
    ///   the mediated path reaches. So a mismatched call site is emitted the old
    ///   way and refused by the old message rather than given a second one here;
    /// - every parameter word of the callee's frame is at an offset the encoder
    ///   can name. The subset already bounds the *caller's* slots; this is the
    ///   callee's, which this function is the first thing to address.
    fn direct_admits(&self, callee: u32, args: u32) -> bool {
        let target = self.program.function(FunctionId(callee));
        let list = self.program.arg_list(ArgsId(args));
        if list.len() != target.params.len() {
            return false;
        }
        let words: u32 = target
            .params
            .iter()
            .map(|layout| self.program.layout(*layout).width())
            .sum();
        words == 0 || slot_offset(words - 1).is_some()
    }

    /// [`Inst::Call`](cove_ir::Inst::Call), handed to the runtime whole.
    ///
    /// See [`crate::abi::CallFn`] for why the frame is not opened here. The six
    /// arguments are the six the System V ABI passes in registers, which is what
    /// makes this a call sequence and not a stack layout.
    ///
    /// `base` is handed over as the *word index* the ABI is written in terms of,
    /// so the byte offset this function keeps is shifted back down — the one
    /// place the prologue's decision to keep bytes rather than words costs an
    /// instruction.
    fn callee_mediated(&mut self, dst: Slot, callee: u32, args: u32) {
        // A call may allocate and an allocation may collect, so this is a
        // safepoint whether the callee reaches one or not: the unpaid work is
        // published and the accumulator cleared, and the helper charges it.
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.xor_rr(WORK, WORK);

        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, BASE_BYTES);
        self.shr_imm8(RSI, 3);
        self.mov_imm32(RDX, self.pc as i32);
        self.mov_imm32(RCX, callee as i32);
        self.mov_imm32(R8, args as i32);
        self.mov_imm32(R9, dst as i32);
        self.mov_imm64(RAX, self.call as i64);
        self.call(RAX);

        // `eax` is the callee's `Outcome`. Anything but `Returned` leaves, and
        // leaves *with that outcome*: a raise eight frames down travels out
        // through one `ret` per frame, and every field it needs the helper has
        // already written.
        let on = self.label();
        self.test_rr32(RAX, RAX);
        self.jcc(CC_E, Target::Label(on));
        self.leave_answered();
        self.bind(on);
        // The helper is allowed to have grown the stack, so the frame pointer
        // derived before the call is not to be used after it.
        self.frame_live = false;
    }

    /// [`Inst::Call`](cove_ir::Inst::Call) where the callee's code is reached
    /// **by this code**, rather than by the runtime on its behalf.
    ///
    /// See [`crate::abi::OpenFn`] and [`crate::abi::CloseFn`] for the two halves
    /// the runtime keeps and why it keeps them: everything that changes how long
    /// a `Vec` the runtime owns is, and everything [ADR 0040] counts. What moves
    /// here is the part the lowering settled and a code generator therefore knows
    /// without asking — the arguments, at their slots and their widths — and the
    /// entry itself.
    ///
    /// ```text
    ///   publish the unpaid work          (the helper charges it)
    ///   open(ctx, base, pc, callee, args, dst)
    ///   rax = the callee's entry, or null if the runtime finished the call
    ///   rdx = the callee's frame, as a word index, or that call's outcome
    ///   ---- rax != 0: the frame is open and empty ----
    ///   r15 = the frame; store each parameter word into it from this frame
    ///   entry(ctx, r15, this frame, dst)   -- the call, and it is a `call`
    ///   close(ctx, outcome, callee)        (the frame comes off)
    ///   ---- rax == 0 ----
    ///   the outcome is in rdx and there is nothing to enter
    ///   ---- both ----
    ///   anything but `Returned` leaves, with that outcome
    /// ```
    ///
    /// Three things in it are load-bearing and none is obvious.
    ///
    /// **The callee is handed this function's own `ctx`.** A mediated call builds
    /// the callee a context of its own, which is why it has to republish the
    /// caller's afterwards; sharing one means every helper the callee reached
    /// already stored the current words pointer and chunk table where this
    /// function will look. `pending_work` is not shared state between them
    /// either, because each frame accumulates its own in [`WORK`] and only
    /// publishes it at a hand-over.
    ///
    /// **`r15` holds the callee's frame across the entry call**, and it is
    /// [`HEAP_SPARE`] — a register that holds a value only *inside* the heap
    /// address template, so there is nothing live in it here. It is callee-saved,
    /// so the compiled callee's own prologue preserves it, and no value is pushed
    /// to keep it: the seven pushes of the prologue are what leave `rsp` aligned
    /// at a `call`, and a push here would undo that.
    ///
    /// **The frame pointer is dead twice**, once after `open` — which pushed a
    /// frame, so the words may have moved before the arguments are stored — and
    /// once after `close`.
    ///
    /// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
    fn callee_direct(&mut self, dst: Slot, callee: u32, args: u32) {
        // A call is a safepoint, whichever way it is made: the unpaid work is
        // published and the accumulator cleared, and `open` charges it.
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.xor_rr(WORK, WORK);

        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, BASE_BYTES);
        self.shr_imm8(RSI, 3);
        self.mov_imm32(RDX, self.pc as i32);
        self.mov_imm32(RCX, callee as i32);
        self.mov_imm32(R8, args as i32);
        self.mov_imm32(R9, dst as i32);
        self.mov_imm64(RAX, self.open as i64);
        self.call(RAX);

        let finished = self.label();
        let joined = self.label();
        self.test_rr(RAX, RAX);
        self.jcc(CC_E, Target::Label(finished));

        // The frame `open` made, kept where the entry call cannot clobber it.
        self.mov_rr(HEAP_SPARE, RDX);
        // `push_frame` is a `Vec::resize`, so the pointer derived before the
        // hand-over is not to be used after it.
        self.frame_live = false;

        let target = self.program.function(FunctionId(callee));
        let widths: Vec<u32> = target
            .params
            .iter()
            .map(|layout| self.program.layout(*layout).width())
            .collect();
        let slots: Vec<Slot> = self
            .program
            .arg_list(ArgsId(args))
            .iter()
            .map(|arg| arg.slot)
            .collect();
        if widths.iter().any(|width| *width > 0) {
            // `rdi` is the callee frame's first word. `RDI` is caller-saved and
            // holds nothing between instructions, and nothing below calls
            // anything until the stores are done.
            self.load(RDX, CTX, OFF_WORDS);
            self.mov_rr(RDI, HEAP_SPARE);
            self.shl_imm8(RDI, 3);
            self.add_rr(RDI, RDX);
            let mut at = 0;
            for (slot, width) in slots.iter().zip(&widths) {
                for word in 0..*width {
                    // Out of this frame and into the callee's, one word at a
                    // time: the two runs are in different frames and cannot
                    // overlap, so nothing has to be held first — which is the
                    // difference from [`Emit::copy`].
                    self.load_slot(RDX, slot + word);
                    let into = slot_offset(at + word).expect("`direct_admits` bounded every word");
                    self.store(RDI, into, RDX);
                }
                at += width;
            }
        }

        // The call: `entry(ctx, callee_base, return_base, return_slot)`, and the
        // destination is this frame and the slot the lowering settled — ADR
        // 0057's two indices, taken from the register the prologue put them in.
        self.mov_rr(RDI, CTX);
        self.mov_rr(RSI, HEAP_SPARE);
        self.mov_rr(RDX, BASE_BYTES);
        self.shr_imm8(RDX, 3);
        self.mov_imm32(RCX, dst as i32);
        self.call(RAX);

        // `close(ctx, outcome, callee)`, which answers the outcome it was given.
        self.mov_rr(RDI, CTX);
        self.mov_rr32(RSI, RAX);
        self.mov_imm32(RDX, callee as i32);
        self.mov_imm64(RAX, self.close as i64);
        self.call(RAX);
        self.jmp(Target::Label(joined));

        // The runtime finished the call itself — an encoded callee, or a refusal
        // — and the outcome is the second word it answered.
        self.bind(finished);
        self.mov_rr32(RAX, RDX);

        self.bind(joined);
        let on = self.label();
        self.test_rr32(RAX, RAX);
        self.jcc(CC_E, Target::Label(on));
        self.leave_answered();
        self.bind(on);
        self.frame_live = false;
    }

    /// `encoded.rs`'s `RETURN` arm, whole: the answer's words into the
    /// destination, and then leave.
    ///
    /// ADR 0057, and see the Cranelift arm's `ret` for the three things this
    /// shape is: the address comes from `NativeCtx::words` re-read here rather
    /// than from [`FRAME`], because the destination is not this frame; a
    /// zero-width return emits nothing, not even the address, because a width-0
    /// destination may name a slot the caller's frame does not have; and the
    /// loads and stores interleave, because the destination is the caller's frame
    /// and so cannot overlap this one — which is the difference between this and
    /// [`Emit::copy`].
    fn ret(&mut self, src: Slot) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        let width = self.program.layout(self.function.returns).width();
        if width > 0 {
            self.load(RDX, CTX, OFF_WORDS);
            self.add_rr(RDX, RETURN_BYTES);
            for word in 0..width {
                self.load_slot(RAX, src + word);
                self.store(RDX, (word * 8) as i32, RAX);
            }
        }
        self.leave(Outcome::Returned);
    }

    /// Leaves with a runtime error named rather than built.
    fn raise(&mut self, code: Raise, detail: u32) {
        self.store(CTX, OFF_PENDING_WORK, WORK);
        self.store_imm32(CTX, OFF_RAISE_CODE, code.abi() as i32);
        self.store_imm32(CTX, OFF_RAISE_DETAIL, detail as i32);
        // The span every runtime error carries is `Function::span_at(pc)`, and
        // only compiled code knows which instruction it was on. Stored on this
        // path only, so the ordinary path pays nothing for it.
        self.store_imm32(CTX, OFF_RAISE_PC, self.pc as i32);
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
    /// `eax` carries the [`Outcome`], which is `#[repr(u32)]`. The seven pops
    /// undo the seven pushes of the prologue; nothing else is ever left on the
    /// stack at an exit, because the only thing that pushes is
    /// [`Emit::copy`] and it pops what it pushed before the instruction ends.
    fn leave(&mut self, outcome: Outcome) {
        self.mov_imm32(RAX, outcome.abi() as i32);
        self.leave_answered();
    }

    /// The same epilogue, for an outcome that is already in `eax`.
    ///
    /// The one caller is [`Emit::callee`]: what it returns is the callee's
    /// outcome rather than one this function chose. None of the seven pops names
    /// `RAX`, so the answer survives them — and the first two are the alignment
    /// pair [`Emit::prologue`] pushed, both of which hold the caller's
    /// `RETURN_BYTES`.
    fn leave_answered(&mut self) {
        for reg in [
            RETURN_BYTES,
            RETURN_BYTES,
            HEAP_SPARE,
            FRAME,
            WORK,
            BASE_BYTES,
            CTX,
        ] {
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

    /// `mov r32, r32`, which zeroes the upper half of its destination.
    ///
    /// Which is what makes it the `u32` mask [`Emit::object_len`] needs.
    fn mov_rr32(&mut self, dst: u8, src: u8) {
        self.rex(false, src, dst);
        self.byte(0x89);
        self.modrm_reg(src, dst);
    }

    /// `mov r64, [base + disp]`
    fn load(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x8b);
        self.modrm_mem(dst, base, disp);
    }

    /// `mov r32, [base + disp]`, which zeroes the upper half of `dst` — the
    /// zero-extending load a table of `u32` entries needs, [`Emit::object_len`]'s
    /// `mov_rr32` in memory-operand form.
    fn load32(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(false, dst, base);
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

    /// `shr r64, imm8`, which is the logical shift: a heap index and a byte
    /// offset are both non-negative, and `Space::load` shifts an unsigned one.
    fn shr_imm8(&mut self, dst: u8, by: u8) {
        self.rex(true, 0, dst);
        self.byte(0xc1);
        self.modrm_reg(5, dst);
        self.byte(by);
    }

    /// `shr r64, cl`
    fn shr_cl(&mut self, dst: u8) {
        self.rex(true, 0, dst);
        self.byte(0xd3);
        self.modrm_reg(5, dst);
    }

    /// `shl r64, cl`
    fn shl_cl(&mut self, dst: u8) {
        self.rex(true, 0, dst);
        self.byte(0xd3);
        self.modrm_reg(4, dst);
    }

    /// `not r64`
    fn not_r(&mut self, dst: u8) {
        self.rex(true, 0, dst);
        self.byte(0xf7);
        self.modrm_reg(2, dst);
    }

    /// `and r64, r64`
    fn and_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x21);
        self.modrm_reg(src, dst);
    }

    /// `or r64, r64`
    fn or_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x09);
        self.modrm_reg(src, dst);
    }

    /// `and r64, imm32`, sign-extended — so every mask emitted through it is
    /// one whose top bit is clear. `Emit::object_len` is the mask that is not,
    /// and it is a `mov r32, r32` instead.
    fn and_imm32(&mut self, dst: u8, value: i32) {
        debug_assert!(value >= 0, "a sign-extended mask sets the upper half");
        self.rex(true, 0, dst);
        self.byte(0x81);
        self.modrm_reg(4, dst);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `cmp r64, imm32`, which sets the flags for `r - imm`.
    fn cmp_imm32(&mut self, reg: u8, value: i32) {
        self.rex(true, 0, reg);
        self.byte(0x81);
        self.modrm_reg(7, reg);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    /// `xor r64, r64`
    fn xor_rr(&mut self, dst: u8, src: u8) {
        self.rex(true, src, dst);
        self.byte(0x31);
        self.modrm_reg(src, dst);
    }

    /// `cmp r64, [base + disp]`, which sets the flags for `r - [base + disp]`.
    ///
    /// The `0x3b` direction of the compare — register against memory — so that
    /// the poll's threshold is read and compared in one instruction rather than
    /// loaded into a scratch register first. See [`Emit::safepoint`], which is
    /// the only caller and is on every backedge.
    fn cmp_r_mem(&mut self, reg: u8, base: u8, disp: i32) {
        self.rex(true, reg, base);
        self.byte(0x3b);
        self.modrm_mem(reg, base, disp);
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

    /// `test r32, r32`, for the `Outcome` a helper answers in `eax`.
    fn test_rr32(&mut self, a: u8, b: u8) {
        self.rex(false, b, a);
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

    /// `cvtsi2sd xmm0, rax`
    fn cvtsi2sd_xmm0_rax(&mut self) {
        for byte in [0xf2, 0x48, 0x0f, 0x2a, 0xc0] {
            self.byte(byte);
        }
    }

    /// `movq rax, xmm0`
    fn movq_rax_xmm0(&mut self) {
        for byte in [0x66, 0x48, 0x0f, 0x7e, 0xc0] {
            self.byte(byte);
        }
    }

    /// `setcc cl`
    fn setcc_cl(&mut self, cc: u8) {
        self.byte(0x0f);
        self.byte(0x90 | cc);
        self.byte(0xc1);
    }

    /// `movzx ecx, cl`, which leaves a zero or a one in the whole of `rcx`.
    fn movzx_ecx_cl(&mut self) {
        self.byte(0x0f);
        self.byte(0xb6);
        self.byte(0xc9);
    }

    /// `cqo`
    fn cqo(&mut self) {
        self.rex(true, 0, 0);
        self.byte(0x99);
    }

    /// `neg r64`, whose overflow flag is set only for `i64::MIN`.
    fn neg_r(&mut self, dst: u8) {
        self.rex(true, 0, dst);
        self.byte(0xf7);
        self.modrm_reg(3, dst);
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
