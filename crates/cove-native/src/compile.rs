//! One [`Function`] to one run of machine code, through Cranelift.
//!
//! The shape of this module is the shape of [ADR 0055]'s decision: a function
//! is the unit of compilation, a basic block is the unit of work accounting
//! and safepoint placement, and a function the lowering does not fully
//! understand is refused whole rather than split.
//!
//! # Refusal is the interesting half
//!
//! `crate::subset`'s `supported` walks the whole function before a single
//! Cranelift instruction is emitted and answers whether every one of its
//! instructions, slots and layouts is inside this slice. It is shared with the
//! other code generator, because two arms admitting different programs would
//! not be comparable. Only then does lowering begin, and lowering is
//! therefore infallible — which is what lets [`Jit::compile`] answer `None`
//! without leaving a half-built function behind in the module.
//!
//! The alternative, lowering until something is not understood and then
//! backing out, is the shape that produces a partially compiled function, and
//! ADR 0055 forbids one: "A function containing an operation the native
//! lowering does not yet support runs entirely on the encoded VM."
//!
//! # The encoded tier is the specification
//!
//! Every arithmetic and comparison decision here mirrors a named arm of
//! `cove_runtime::vm::exec::encoded`'s dispatch loop, and the mirror is cited
//! at the point it is made. That is not documentation courtesy: a native `+`
//! that wraps where the VM raises is a *wrong answer*, and the only thing
//! that would eventually catch it is the differential corpus, at the far end
//! of a long and confusing hunt.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use std::mem::offset_of;

use cove_ir::{ArithOp, CmpOp, Function, FunctionId, Inst, Num, Program, Slot};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, BlockCall, FuncRef, InstBuilder, JumpTableData, MemFlagsData, Signature,
};
use cranelift_codegen::ir::{Type, Value};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module, ModuleError};

use crate::abi::{
    Entry, NativeCtx, NativeHelpers, Outcome, Raise, HEAP_CHUNK_SHIFT, HEAP_CHUNK_WORDS,
    HEAP_ORIGIN_WORDS,
};
use crate::subset::{by_zero_of, leaders, overflow_of, slot_offset, supported};
use crate::Unavailable;

/// The name the safepoint helper is imported under.
///
/// An internal detail of the binding — [`JITBuilder::symbol`] is given this
/// name and the helper's address, and every compiled function calls it
/// through an ordinary relocation. It is not part of the ABI: nothing outside
/// this crate can see it, and a second code generator is free to bind the
/// same [`NativeHelpers`] differently.
const SAFEPOINT: &str = "cove_native_safepoint";

/// The name the call helper is imported under. [`SAFEPOINT`]'s note applies.
const CALL: &str = "cove_native_call";

// The `NativeCtx` field offsets, read from the declaration rather than
// written out. `offset_of!` is a const expression, so the numbers compiled
// into the machine code and the numbers Rust uses to read the struct are the
// same numbers by construction; reordering the fields cannot desynchronise
// them.
const OFF_WORDS: i32 = offset_of!(NativeCtx, words) as i32;
const OFF_CHUNKS: i32 = offset_of!(NativeCtx, chunks) as i32;
const OFF_PENDING_WORK: i32 = offset_of!(NativeCtx, pending_work) as i32;
const OFF_RETURN_SLOT: i32 = offset_of!(NativeCtx, return_slot) as i32;
const OFF_RAISE_CODE: i32 = offset_of!(NativeCtx, raise_code) as i32;
const OFF_RAISE_DETAIL: i32 = offset_of!(NativeCtx, raise_detail) as i32;
const OFF_RAISE_PC: i32 = offset_of!(NativeCtx, raise_pc) as i32;
const OFF_RAISE_A: i32 = offset_of!(NativeCtx, raise_a) as i32;
const OFF_RAISE_B: i32 = offset_of!(NativeCtx, raise_b) as i32;

/// The low half of an object header, which is its length field.
const LEN_MASK: i64 = u32::MAX as i64;

impl From<ModuleError> for Unavailable {
    fn from(error: ModuleError) -> Self {
        Unavailable(error.to_string())
    }
}

/// A function this code generator has compiled.
///
/// Opaque, and it carries which Cove function it is so that a caller building
/// ADR 0055's `Program + FunctionId -> encoded entry | native entry` table
/// does not have to keep that association itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compiled {
    id: FuncId,
    /// Which Cove function this is.
    pub function: FunctionId,
    /// How many bytes of machine code this function is.
    ///
    /// Cranelift's own count — `CodeInfo::total_size` for the emitted
    /// buffer — rather than a difference of addresses, which would include
    /// whatever alignment padding the module put between two functions. Here
    /// so that the two arms' code size is one number read the same way from
    /// both.
    pub code_bytes: u32,
}

/// A baseline code generator, and the memory its code lives in.
///
/// One of these owns every page it has written, so it must outlive every
/// [`Entry`] taken out of it. Dropping it leaks the pages rather than freeing
/// them, which is the safe direction: a freed page under a running Cove frame
/// is not a bug anything could diagnose.
pub struct Jit {
    module: JITModule,
    ctx: Context,
    builder: FunctionBuilderContext,
    safepoint: FuncId,
    call: FuncId,
    /// How many functions have been declared, which is how the symbol names
    /// are kept distinct. Compiling the same [`FunctionId`] twice is a
    /// caller's policy question, not an error here, so the name cannot be
    /// derived from the id alone.
    declared: u32,
    finalized: bool,
}

impl Jit {
    /// A code generator for this host, calling back through `helpers`.
    ///
    /// `helpers` is the whole of the runtime's side of the boundary; see the
    /// crate documentation for why it arrives as function pointers and not as
    /// a dependency.
    pub fn new(helpers: NativeHelpers) -> Result<Self, Unavailable> {
        let mut builder = JITBuilder::new(default_libcall_names())?;
        // The one binding. `as usize as *const u8` rather than a direct cast
        // because a function pointer is not castable to a data pointer in one
        // step; the integer in between is the same address either way.
        builder.symbol(SAFEPOINT, helpers.safepoint as usize as *const u8);
        builder.symbol(CALL, helpers.call as usize as *const u8);
        let mut module = JITModule::new(builder);

        // Pointers are added to a `u64` word index scaled by eight, so a
        // target whose pointer is not 64 bits would need a different address
        // computation. Refusing here is ADR 0055's capability diagnostic
        // rather than a lowering that is subtly wrong on a 32-bit host.
        let pointer = module.target_config().pointer_type();
        if pointer != types::I64 {
            return Err(Unavailable(format!(
                "this lowering forms a frame address as a 64-bit pointer, and this target's is {pointer}"
            )));
        }

        let signature = safepoint_signature(&module);
        let safepoint = module.declare_function(SAFEPOINT, Linkage::Import, &signature)?;
        let signature = call_signature(&module);
        let call = module.declare_function(CALL, Linkage::Import, &signature)?;
        Ok(Jit {
            ctx: module.make_context(),
            module,
            builder: FunctionBuilderContext::new(),
            safepoint,
            call,
            declared: 0,
            finalized: false,
        })
    }

    /// Compiles `program`'s function `id`, or answers `None` if any part of it
    /// is outside this slice.
    ///
    /// `None` is not a failure. It is the answer ADR 0055 asks for — the
    /// caller runs the whole function on the encoded VM — and it is the
    /// common answer today, because this slice lowers scalars and control
    /// flow and nothing else.
    pub fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Compiled> {
        let function = program.function(id);
        if !supported(program, function) {
            return None;
        }

        let name = format!("cove_native_{}_{}", id.0, self.declared);
        self.declared += 1;
        let signature = entry_signature(&self.module);
        let func = self
            .module
            .declare_function(&name, Linkage::Local, &signature)
            .ok()?;

        self.ctx.clear();
        self.ctx.func.signature = signature;
        {
            let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.builder);
            let safepoint = self
                .module
                .declare_func_in_func(self.safepoint, builder.func);
            let call = self.module.declare_func_in_func(self.call, builder.func);
            Lower::new(&mut builder, program, function, safepoint, call).run();
            builder.seal_all_blocks();
            builder.finalize(self.module.target_config());
        }
        self.module.define_function(func, &mut self.ctx).ok()?;
        let code_bytes = self
            .ctx
            .compiled_code()
            .map(|code| code.code_info().total_size)
            .unwrap_or(0);
        self.finalized = false;
        Some(Compiled {
            id: func,
            function: id,
            code_bytes,
        })
    }

    /// Makes every function compiled so far executable.
    ///
    /// ADR 0055: "JIT code pages are never simultaneously writable and
    /// executable. The implementation writes through a writable mapping,
    /// finalizes it, and executes only a non-writable mapping." This is that
    /// finalization, and it is Cranelift's own — the mapping, the protection
    /// change and the instruction-cache flush are
    /// [`JITModule::finalize_definitions`]'s, which is one of the reasons the
    /// ADR names Cranelift for the first implementation. There is nothing in
    /// this crate that maps a page itself.
    pub fn finalize(&mut self) -> Result<(), Unavailable> {
        self.module.finalize_definitions()?;
        self.finalized = true;
        Ok(())
    }

    /// The entry point of a compiled function.
    ///
    /// # Panics
    ///
    /// If [`Jit::finalize`] has not been called since the last
    /// [`Jit::compile`]. Calling into a page that is still writable is
    /// exactly what the ADR says must not happen, so it is a panic and not a
    /// `None` a caller could ignore.
    pub fn entry(&self, compiled: Compiled) -> Entry {
        assert!(
            self.finalized,
            "`Jit::finalize` has to run before a compiled function is entered"
        );
        let code = self.module.get_finalized_function(compiled.id);
        // Safety: `code` is the start of a finalized, executable function
        // that this crate emitted with exactly `entry_signature`'s shape,
        // which is `Entry`'s shape.
        unsafe { std::mem::transmute::<*const u8, Entry>(code) }
    }
}

/// `extern "C" fn(ctx: *mut NativeCtx, base: u64) -> Outcome`, in Cranelift's
/// terms.
///
/// The return is `I32` because [`Outcome`] is `#[repr(u32)]`, and the second
/// parameter is `I64` because `base` is a word index rather than a pointer.
/// See [`crate::abi`] for why the signature is this and not something else.
fn entry_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

/// [`crate::abi::SafepointFn`], in Cranelift's terms.
///
/// `I8` for the answer because that is Rust's `bool` across a C boundary: the
/// low byte is 0 or 1, and `brif` on it reads exactly that byte.
fn safepoint_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature.params.push(AbiParam::new(types::I32));
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I8));
    signature
}

/// [`crate::abi::CallFn`], in Cranelift's terms.
///
/// `I32` for the answer because it is an [`Outcome`], which is `#[repr(u32)]`,
/// and the same `I32` an entry point returns — a raise or a stop from a callee
/// is returned from this function unchanged, so the two widths have to be the
/// one width.
fn call_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    // `base`, then `pc`, `callee`, `args`, `dst`.
    signature.params.push(AbiParam::new(types::I64));
    for _ in 0..4 {
        signature.params.push(AbiParam::new(types::I32));
    }
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

/// One function's lowering.
struct Lower<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    program: &'a Program,
    function: &'a Function,
    safepoint: FuncRef,
    call: FuncRef,
    pointer: Type,
    /// The two entry parameters. Defined in the entry block, which dominates
    /// every other, so they are readable from anywhere without a block
    /// parameter of their own.
    ctx: Value,
    base: Value,
    /// The unpaid work accumulator: IR instructions executed since the last
    /// safepoint.
    ///
    /// A `Variable` rather than a value threaded by hand, because a block
    /// with two predecessors needs the merge and `cranelift-frontend`'s SSA
    /// builder is what inserts it.
    work: Variable,
    /// The frame's first word, as a pointer, if it has been derived in the
    /// block being emitted.
    ///
    /// `None` at the start of every block and after every call, which is the
    /// whole of the discipline `crate::abi` describes: `NativeCtx::words` is
    /// a `Vec`'s buffer and a helper may have reallocated it.
    frame: Option<Value>,
    /// The heap's chunk-base table, as a pointer, if it has been loaded in the
    /// block being emitted.
    ///
    /// [`Lower::frame`]'s discipline for the other region: the table may be
    /// reallocated by a helper that allocates, so it is re-loaded from
    /// [`NativeCtx::chunks`] at every block and after every call. Cached within
    /// a block because a `load-elem` of a three-word element reads four heap
    /// words and would otherwise load the table four times.
    chunks: Option<Value>,
    /// Which IR instruction is being emitted.
    ///
    /// Only a raise reads it — [`NativeCtx::raise_pc`] is how the runtime finds
    /// the span — and a raise can be emitted from four methods down, so it is a
    /// field rather than an argument threaded through `arith`, `element` and
    /// `bytes` alike.
    pc: usize,
    /// Per instruction: the block that begins there and its length, or `None`
    /// if no block begins there.
    blocks: Vec<Option<(Block, u32)>>,
}

impl<'a, 'f> Lower<'a, 'f> {
    fn new(
        b: &'a mut FunctionBuilder<'f>,
        program: &'a Program,
        function: &'a Function,
        safepoint: FuncRef,
        call: FuncRef,
    ) -> Self {
        let pointer = b.func.signature.params[0].value_type;
        let blocks: Vec<Option<(Block, u32)>> = leaders(program, function)
            .into_iter()
            .map(|length| length.map(|length| (b.create_block(), length)))
            .collect();
        let work = b.declare_var(types::I64);

        // The entry block and its two parameters are set up here rather than
        // in `run` so that `ctx` and `base` are never a placeholder: they are
        // read by nearly every method, and a `Value` that is valid only after
        // some other method has run is the sort of invariant that survives
        // exactly as long as nobody reorders anything.
        let (entry, _) = blocks[0].expect("`supported` refused an empty body");
        b.switch_to_block(entry);
        b.append_block_params_for_function_params(entry);
        let ctx = b.block_params(entry)[0];
        let base = b.block_params(entry)[1];
        let zero = b.ins().iconst(types::I64, 0);
        b.def_var(work, zero);

        Lower {
            b,
            program,
            function,
            safepoint,
            call,
            pointer,
            ctx,
            base,
            work,
            frame: None,
            chunks: None,
            pc: 0,
            blocks,
        }
    }

    fn run(&mut self) {
        let (_, length) = self.blocks[0].expect("`supported` refused an empty body");
        self.charge(length);

        let mut terminated = false;
        for pc in 0..self.function.code.len() {
            if pc > 0 {
                if let Some((block, length)) = self.blocks[pc] {
                    if !terminated {
                        self.b.ins().jump(block, &[]);
                    }
                    self.b.switch_to_block(block);
                    self.forget();
                    self.charge(length);
                }
            }
            self.pc = pc;
            terminated = self.inst(pc);
        }
        debug_assert!(
            terminated,
            "`supported` admitted a function whose last instruction is not a terminator"
        );
    }

    /// Adds a block's static instruction count to the work accumulator.
    ///
    /// One add per block rather than one per instruction, which is ADR 0055's
    /// "Native code does not update an atomic or call the Meter for every IR
    /// instruction. Lowering computes a work charge for a bounded run of IR
    /// and pays it at safepoints."
    ///
    /// It is added at block *entry* rather than accumulated along a path,
    /// because a block with two predecessors is reached having done two
    /// different amounts of work and a compile-time constant cannot say which.
    /// The accumulator is a run-time value and the merge is the SSA builder's.
    fn charge(&mut self, instructions: u32) {
        let work = self.b.use_var(self.work);
        let work = self.b.ins().iadd_imm_s(work, i64::from(instructions));
        self.b.def_var(self.work, work);
    }

    /// Emits one IR instruction. Answers whether it terminated its block.
    fn inst(&mut self, pc: usize) -> bool {
        match &self.function.code[pc] {
            // `encoded.rs`'s `CONST_BOOL | CONST_INT | CONST_FLOAT` arm: one
            // store of a word the encoder already computed.
            Inst::Bool { dst, value } => {
                let word = self.b.ins().iconst(types::I64, i64::from(*value));
                self.store_slot(*dst, word);
                false
            }
            Inst::Int { dst, value } => {
                let word = self.b.ins().iconst(types::I64, *value);
                self.store_slot(*dst, word);
                false
            }
            // The same store `Inst::Int` makes, of a number the layout already
            // fixed: `encoded.rs` shares its `FUNC_REF | CONST_TAG` arm with
            // that one.
            Inst::Tag { dst, case, .. } => {
                let word = self.b.ins().iconst(types::I64, i64::from(case.0));
                self.store_slot(*dst, word);
                false
            }
            Inst::Copy { dst, src, layout } => {
                self.copy(*dst, *src, self.program.layout(*layout).width());
                false
            }
            // `encoded.rs`'s `NOT` arm tests the whole *word* against zero, not
            // the low byte, so that is what is tested here.
            Inst::Not { dst, a } => {
                let x = self.load_slot(*a);
                let answer = self.b.ins().icmp_imm_s(IntCC::Equal, x, 0);
                self.store_flag(*dst, answer);
                false
            }
            Inst::Len { dst, obj } => {
                let addr = self.load_slot(*obj);
                self.refuse_null(addr);
                let len = self.object_len(addr);
                self.store_slot(*dst, len);
                false
            }
            Inst::LoadElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.load_elem(*dst, *obj, *index, self.program.layout(*layout).width());
                false
            }
            Inst::ByteAt { dst, obj, at } => {
                self.byte_at(*dst, *obj, *at);
                false
            }
            Inst::Call { dst, callee, args } => {
                self.callee(*dst, callee.0, args.0);
                false
            }
            Inst::Switch { on, table } => {
                self.switch(*on, *table);
                true
            }
            Inst::Arith {
                num: Num::Int,
                op,
                dst,
                a,
                b,
            } => {
                let x = self.load_slot(*a);
                let y = self.load_slot(*b);
                self.arith(*op, *dst, x, y);
                false
            }
            Inst::ArithImm { op, dst, a, value } => {
                let x = self.load_slot(*a);
                let y = self.b.ins().iconst(types::I64, *value);
                self.arith(*op, *dst, x, y);
                false
            }
            Inst::Cmp {
                on: _,
                op,
                dst,
                a,
                b,
            } => {
                let x = self.load_slot(*a);
                let y = self.load_slot(*b);
                let answer = self.compare(*op, x, y);
                self.store_flag(*dst, answer);
                false
            }
            Inst::CmpImm { op, dst, a, value } => {
                let x = self.load_slot(*a);
                let y = self.b.ins().iconst(types::I64, *value);
                let answer = self.compare(*op, x, y);
                self.store_flag(*dst, answer);
                false
            }
            Inst::CmpBranch {
                on: _,
                op,
                dst,
                a,
                b,
                target,
            } => {
                let x = self.load_slot(*a);
                let y = self.load_slot(*b);
                let answer = self.compare(*op, x, y);
                self.store_flag(*dst, answer);
                self.branch_when_false(pc, answer, *target);
                true
            }
            Inst::CmpImmBranch {
                op,
                dst,
                a,
                value,
                target,
            } => {
                let x = self.load_slot(*a);
                let y = self.b.ins().iconst(types::I64, i64::from(*value));
                let answer = self.compare(*op, x, y);
                self.store_flag(*dst, answer);
                self.branch_when_false(pc, answer, *target);
                true
            }
            // `encoded.rs`'s `BRANCH_FALSE` arm tests the *word* against
            // zero, so the word is what `brif` is given: `brif` takes its
            // then-branch when its argument is non-zero, which is the same
            // test with the arms the other way round.
            Inst::BranchFalse { cond, to } => {
                let word = self.load_slot(*cond);
                self.branch_when_false(pc, word, *to);
                true
            }
            Inst::Jump { to } => {
                if (*to as usize) <= pc {
                    self.safepoint(*to);
                }
                let (block, _) = self.blocks[*to as usize].expect("a jump target begins a block");
                self.b.ins().jump(block, &[]);
                true
            }
            Inst::Return { src } => {
                self.ret(*src);
                true
            }
            // `encoded.rs`'s `TRAP` arm: the message is a program string, so
            // the `StrId` is what crosses the boundary and `cove-runtime`
            // looks it up. ADR 0055's "runtime errors … remain runtime
            // helpers" applies to the *text* as much as to the machinery.
            Inst::Trap { message } => {
                self.raise(Raise::Trapped, message.0);
                true
            }
            other => unreachable!("`supported` admitted {other:?}, which is not lowered"),
        }
    }

    // --- memory ------------------------------------------------------------

    /// The frame's first word, as a pointer.
    ///
    /// Derived from [`NativeCtx::words`] and the `base` *word index*, which is
    /// the reason the entry takes an index: the `Vec` behind `words` moves,
    /// and the index does not. Cached for the rest of the block and dropped
    /// after every call, because a helper that grew the stack will have
    /// stored a different pointer into the field.
    fn frame(&mut self) -> Value {
        if let Some(frame) = self.frame {
            return frame;
        }
        let words = self
            .b
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), self.ctx, OFF_WORDS);
        let bytes = self.b.ins().ishl_imm_u(self.base, 3);
        let frame = self.b.ins().iadd(words, bytes);
        self.frame = Some(frame);
        frame
    }

    /// Forgets everything a block cannot carry into the next one.
    ///
    /// Both cached pointers, in one place, because both are cached for exactly
    /// as long and the cost of forgetting one and not the other is a load of
    /// stale memory rather than a compile failure.
    fn forget(&mut self) {
        self.frame = None;
        self.chunks = None;
    }

    /// The heap's chunk-base table, as a pointer.
    ///
    /// See [`Lower::chunks`]. The table is not the heap: it is one pointer per
    /// committed chunk, which is what makes a heap word two indexings.
    fn heap_chunks(&mut self) -> Value {
        if let Some(chunks) = self.chunks {
            return chunks;
        }
        let chunks = self
            .b
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), self.ctx, OFF_CHUNKS);
        self.chunks = Some(chunks);
        chunks
    }

    /// The heap word at the linear address `addr`.
    ///
    /// `Memory::read`'s heap half, which is `Space::load`: subtract the heap
    /// origin, find the chunk, and index inside it. The `Relaxed` atomic load
    /// that Rust half performs is a plain load on every target either arm runs
    /// on, and ADR 0034's argument for why `Relaxed` is enough — the ordering
    /// that makes one task's writes visible to another is the release/acquire
    /// pair on a cell's lock word — is unchanged by the load being emitted here
    /// instead.
    fn heap_word(&mut self, addr: Value) -> Value {
        let chunks = self.heap_chunks();
        let index = self.b.ins().iadd_imm_s(addr, -(HEAP_ORIGIN_WORDS as i64));
        let which = self.b.ins().ushr_imm_u(index, i64::from(HEAP_CHUNK_SHIFT));
        let at = self.b.ins().ishl_imm_u(which, 3);
        let entry = self.b.ins().iadd(chunks, at);
        let chunk = self
            .b
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), entry, 0);
        let inside = self
            .b
            .ins()
            .band_imm_u(index, (HEAP_CHUNK_WORDS - 1) as i64);
        let offset = self.b.ins().ishl_imm_u(inside, 3);
        let word = self.b.ins().iadd(chunk, offset);
        self.b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), word, 0)
    }

    /// `Memory::payload`: payload word `at` of the object whose header is at
    /// `addr`, where `at` is a run-time value.
    ///
    /// The header is one word, so a payload word is one past it. That `+ 1` is
    /// `Memory::payload_addr`'s and is written once here rather than at each of
    /// the three instructions that reads a payload.
    fn payload(&mut self, addr: Value, at: Value) -> Value {
        let one = self.b.ins().iadd_imm_s(at, 1);
        let word = self.b.ins().iadd(addr, one);
        self.heap_word(word)
    }

    /// `Memory::object_len`: the low half of the header, as a non-negative
    /// `Int`.
    ///
    /// A `u32` masked out of a word, so the `as i64` the encoded arms all
    /// perform on it is already done: the answer cannot be negative and the
    /// unsigned comparisons below depend on that.
    fn object_len(&mut self, addr: Value) -> Value {
        let header = self.heap_word(addr);
        self.b.ins().band_imm_u(header, LEN_MASK)
    }

    /// Refuses a null reference, which every reader of an object does first.
    ///
    /// `Machine::element`, `encoded.rs`'s `LEN` and its `BYTE_AT` each begin
    /// with `if addr == 0`, and each answers `null_object()`. One method,
    /// because one message.
    fn refuse_null(&mut self, addr: Value) {
        let null = self.b.ins().icmp_imm_s(IntCC::Equal, addr, 0);
        self.raise_if(null, Raise::NullObject, 0);
    }

    /// `encoded.rs`'s `LOAD_ELEM` arm, which is `Machine::element` and then a
    /// copy of `width` words.
    ///
    /// The index check is *one* unsigned comparison and that is exact rather
    /// than clever: `Machine::element` refuses `at < 0 || at >= len`, and a
    /// negative `i64` read as unsigned is larger than any `len` — which is a
    /// `u32` masked out of a header and so below 2^32. One compare answers both
    /// halves and cannot answer either of them wrongly.
    fn load_elem(&mut self, dst: Slot, obj: Slot, index: Slot, width: u32) {
        let addr = self.load_slot(obj);
        self.refuse_null(addr);
        let index = self.load_slot(index);
        let len = self.object_len(addr);
        let outside = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, len);
        self.raise_range(outside, Raise::IndexOutOfRange, index, len);
        // The stride, which is what makes an `Array<Point>` a run of two-word
        // elements. `Machine::element` multiplies in `u32`; this multiplies in
        // `u64`, which agrees on every product the check above admits.
        let at = self.b.ins().imul_imm_s(index, i64::from(width));
        for word in 0..width {
            let at = self.b.ins().iadd_imm_s(at, i64::from(word));
            let value = self.payload(addr, at);
            self.store_slot(dst + word, value);
        }
    }

    /// `encoded.rs`'s `BYTE_AT` arm: a payload read, a shift and a mask.
    ///
    /// The bound is the string's *byte* length and the refusal is not
    /// `Array.get`'s — see [`Inst::ByteAt`](cove_ir::Inst::ByteAt) for why a
    /// byte offset out of range stops the run rather than answering an
    /// `Option`. The one unsigned comparison is [`Lower::load_elem`]'s.
    fn byte_at(&mut self, dst: Slot, obj: Slot, at: Slot) {
        let addr = self.load_slot(obj);
        self.refuse_null(addr);
        let at = self.load_slot(at);
        let len = self.object_len(addr);
        let outside = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, at, len);
        self.raise_range(outside, Raise::ByteOffset, at, len);
        // Eight bytes to a word, least-significant byte first.
        let which = self.b.ins().ushr_imm_u(at, 3);
        let word = self.payload(addr, which);
        let inside = self.b.ins().band_imm_u(at, 7);
        let shift = self.b.ins().ishl_imm_u(inside, 3);
        let moved = self.b.ins().ushr(word, shift);
        let byte = self.b.ins().band_imm_u(moved, 0xFF);
        self.store_slot(dst, byte);
    }

    fn load_slot(&mut self, slot: Slot) -> Value {
        let frame = self.frame();
        let at = slot_offset(slot).expect("`supported` bounded every slot");
        self.b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), frame, at)
    }

    fn store_slot(&mut self, slot: Slot, word: Value) {
        let frame = self.frame();
        let at = slot_offset(slot).expect("`supported` bounded every slot");
        self.b.ins().store(MemFlagsData::trusted(), word, frame, at);
    }

    /// Writes a comparison's answer, as `encoded.rs`'s `cmp_int!` does:
    /// `answer as u64`, so a zero or a one and never a mask.
    fn store_flag(&mut self, slot: Slot, flag: Value) {
        let word = self.b.ins().uextend(types::I64, flag);
        self.store_slot(slot, word);
    }

    fn store_ctx(&mut self, at: i32, value: Value) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), value, self.ctx, at);
    }

    /// ADR 0001's field-wise shallow copy, which `encoded.rs`'s `COPY` arm
    /// performs with `Memory::copy_slots`.
    ///
    /// Every word is loaded before any is stored, and that is not tidiness:
    /// `copy_slots` is a `memmove`, because two slots of one frame may
    /// overlap and the lowering is free to emit that without proving it does
    /// not. A forward run of load-store pairs would smear the source over
    /// itself when the destination is the higher of two overlapping runs.
    fn copy(&mut self, dst: Slot, src: Slot, width: u32) {
        let mut held = Vec::with_capacity(width as usize);
        for word in 0..width {
            held.push(self.load_slot(src + word));
        }
        for (word, value) in held.into_iter().enumerate() {
            self.store_slot(dst + word as u32, value);
        }
    }

    // --- arithmetic --------------------------------------------------------

    /// `encoded.rs`'s `int_op!`/`arith_imm!` macros, which are one call to
    /// `int_arith` — `checked_add`, `checked_sub`, `checked_mul`, and a zero
    /// test before `checked_div` and `checked_rem`.
    ///
    /// Cranelift's `sadd_overflow`, `ssub_overflow` and `smul_overflow` are
    /// exactly `checked_*`: the wrapped result and a flag. Division is not,
    /// so its two failures are tested explicitly — and they are tested in the
    /// order `int_arith` tests them, zero first, because `i64::MIN / 0` has
    /// to say "by zero" and not "overflowed".
    fn arith(&mut self, op: ArithOp, dst: Slot, x: Value, y: Value) {
        let overflow = overflow_of(self.function, op, dst);
        let value = match op {
            ArithOp::Add => {
                let (value, flag) = self.b.ins().sadd_overflow(x, y);
                self.raise_if(flag, overflow, 0);
                value
            }
            ArithOp::Sub => {
                let (value, flag) = self.b.ins().ssub_overflow(x, y);
                self.raise_if(flag, overflow, 0);
                value
            }
            ArithOp::Mul => {
                let (value, flag) = self.b.ins().smul_overflow(x, y);
                self.raise_if(flag, overflow, 0);
                value
            }
            ArithOp::Div | ArithOp::Rem => {
                let by_zero = by_zero_of(op);
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                self.raise_if(zero, by_zero, 0);
                // `checked_div` and `checked_rem` answer `None` for
                // `i64::MIN / -1` too, and `int_arith` reports that as an
                // overflow of the named operation. Cranelift's `sdiv` and
                // `srem` trap on it, and a trap is a signal rather than a
                // Cove error, so it is branched around instead.
                let least = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let both = self.b.ins().band(least, minus_one);
                self.raise_if(both, overflow, 0);
                if matches!(op, ArithOp::Rem) {
                    self.b.ins().srem(x, y)
                } else {
                    self.b.ins().sdiv(x, y)
                }
            }
        };
        self.store_slot(dst, value);
    }

    /// `encoded.rs`'s `cmp_int!`: `compare(op, x.cmp(&y))` on the words read
    /// as `i64`, which is a signed comparison.
    ///
    /// The same six conditions serve [`Compare::Bool`], which
    /// `crate::subset` has already narrowed to equality — and
    /// equality is the one comparison for which signedness cannot matter.
    fn compare(&mut self, op: CmpOp, x: Value, y: Value) -> Value {
        let cc = match op {
            CmpOp::Eq => IntCC::Equal,
            CmpOp::Ne => IntCC::NotEqual,
            CmpOp::Lt => IntCC::SignedLessThan,
            CmpOp::Le => IntCC::SignedLessThanOrEqual,
            CmpOp::Gt => IntCC::SignedGreaterThan,
            CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
        };
        self.b.ins().icmp(cc, x, y)
    }

    // --- control flow -----------------------------------------------------

    /// Takes `target` when `word` is zero, and falls through otherwise.
    ///
    /// The fall-through is a block of its own because Cranelift's `brif`
    /// names both successors. When `target` is at or behind this instruction
    /// the branch is a loop backedge, and the safepoint goes on a block
    /// interposed on that edge rather than in front of the branch — so the
    /// iteration that leaves the loop does not pay for a poll it did not
    /// need.
    fn branch_when_false(&mut self, pc: usize, word: Value, target: u32) {
        let (through, _) = self.blocks[pc + 1].expect("a branch's fall-through begins a block");
        let (taken, _) = self.blocks[target as usize].expect("a branch target begins a block");
        if (target as usize) <= pc {
            let edge = self.b.create_block();
            self.b.ins().brif(word, through, &[], edge, &[]);
            self.b.switch_to_block(edge);
            self.forget();
            self.safepoint(target);
            self.b.ins().jump(taken, &[]);
        } else {
            self.b.ins().brif(word, through, &[], taken, &[]);
        }
    }

    /// A safepoint: hand the runtime the unpaid work, and leave if it says to.
    ///
    /// Emitted on every backedge, which is the floor ADR 0055 sets
    /// ("Safepoints occur at least: on loop backedges; …"). The other four
    /// places the ADR names are not reachable in this slice — there are no
    /// Host effects, no allocation and no runtime calls to put one around —
    /// with one exception that is a real and stated gap: **this slice does not
    /// split a long straight-line block.** A loop-free function of a hundred
    /// thousand instructions therefore polls once, at its return, and the
    /// ADR's "at bounded intervals inside long straight-line code" is not yet
    /// honoured. That is the next slice's work and it is why this tier is not
    /// yet selectable.
    ///
    /// The helper is where ADR 0040's order lives: cancellation and
    /// task-local stops, then fuel and deadline accounting, then the collector
    /// rendezvous, in that order. None of the three is emitted here, and none
    /// should be.
    fn safepoint(&mut self, pc: u32) {
        let work = self.b.use_var(self.work);
        let at = self.b.ins().iconst(types::I32, i64::from(pc));
        let call = self.b.ins().call(self.safepoint, &[self.ctx, at, work]);
        let carry_on = self.b.inst_results(call)[0];
        // Charged, so no longer pending — on both sides of the branch below.
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);
        // The helper is allowed to have grown the stack and to have committed a
        // heap chunk, so neither pointer derived before the call is to be used
        // after it. This one line is the whole of the reallocation discipline;
        // see `crate::abi`.
        self.forget();

        let stop = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(carry_on, on, &[], stop, &[]);

        self.b.switch_to_block(stop);
        // Nothing is pending at a stop: the charge went to the helper before
        // it answered.
        let nothing = self.b.ins().iconst(types::I64, 0);
        self.store_ctx(OFF_PENDING_WORK, nothing);
        self.leave(Outcome::Stopped);

        self.b.switch_to_block(on);
        self.forget();
    }

    /// `encoded.rs`'s `RETURN` arm, minus the copy.
    ///
    /// The encoded arm copies `Function::returns`' width from `base + src` to
    /// `caller_base + dst`, and it can, because it has the caller's frame in
    /// front of it. A native callee does not: the caller's frame is the
    /// caller's, and reaching into it would put the calling convention in two
    /// places. So the slot is reported and the copy stays with the caller,
    /// which is the tier-independent half of the same arm.
    fn ret(&mut self, src: Slot) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let slot = self.b.ins().iconst(types::I32, i64::from(src));
        self.store_ctx(OFF_RETURN_SLOT, slot);
        self.leave(Outcome::Returned);
    }

    /// Leaves with a runtime error named rather than built.
    ///
    /// The pc goes out with it, because the span every runtime error carries is
    /// `Function::span_at(pc)` and only compiled code knows which instruction
    /// it was on. It is stored on this path only, so the ordinary path pays
    /// nothing for it.
    fn raise(&mut self, code: Raise, detail: u32) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let named = self.b.ins().iconst(types::I32, i64::from(code.abi()));
        self.store_ctx(OFF_RAISE_CODE, named);
        let carried = self.b.ins().iconst(types::I32, i64::from(detail));
        self.store_ctx(OFF_RAISE_DETAIL, carried);
        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        self.store_ctx(OFF_RAISE_PC, at);
        self.leave(Outcome::Raised);
    }

    /// [`Lower::raise_if`] for the two refusals whose message names numbers.
    ///
    /// "index {a} is outside a collection of {b}" and "`byteAt` is {a}, and a
    /// byte offset into this string is 0 to {b} - 1" are the two, and both
    /// numbers are run-time values, so they go out through
    /// [`NativeCtx::raise_a`] and [`NativeCtx::raise_b`] rather than through the
    /// `u32` a `Raise` otherwise carries. The runtime writes the sentence; this
    /// hands it the operands, which is the same division as every other raise.
    fn raise_range(&mut self, flag: Value, code: Raise, a: Value, b: Value) {
        let bad = self.b.create_block();
        let good = self.b.create_block();
        self.b.ins().brif(flag, bad, &[], good, &[]);
        self.b.switch_to_block(bad);
        self.store_ctx(OFF_RAISE_A, a);
        self.store_ctx(OFF_RAISE_B, b);
        self.raise(code, 0);
        self.b.switch_to_block(good);
    }

    /// [`Inst::Call`](cove_ir::Inst::Call), handed to the runtime whole.
    ///
    /// See [`crate::abi::CallFn`] for why the frame is not opened here. What is
    /// emitted is the hand-over and the three things around it:
    ///
    /// - the unpaid work is published before the call and the accumulator
    ///   cleared, because a call may allocate and an allocation may collect, so
    ///   this is a safepoint whether the callee reaches one or not;
    /// - an outcome that is not [`Outcome::Returned`] is returned from this
    ///   function unchanged, so a raise eight frames down leaves through one
    ///   `ret` per frame and there is no unwinding;
    /// - both cached pointers are dropped, because the callee may have grown the
    ///   stack and may have committed a heap chunk.
    fn callee(&mut self, dst: Slot, callee: u32, args: u32) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);

        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        let callee = self.b.ins().iconst(types::I32, i64::from(callee));
        let args = self.b.ins().iconst(types::I32, i64::from(args));
        let into = self.b.ins().iconst(types::I32, i64::from(dst));
        let call = self
            .b
            .ins()
            .call(self.call, &[self.ctx, self.base, at, callee, args, into]);
        let outcome = self.b.inst_results(call)[0];
        self.forget();

        let left = self.b.create_block();
        let on = self.b.create_block();
        let returned =
            self.b
                .ins()
                .icmp_imm_s(IntCC::Equal, outcome, i64::from(Outcome::Returned.abi()));
        self.b.ins().brif(returned, on, &[], left, &[]);

        self.b.switch_to_block(left);
        // Not `leave`: what this returns is the callee's outcome and not one
        // this function chose, and every field that outcome needs — the raise,
        // the pending work — the helper has already written.
        self.b.ins().return_(&[outcome]);

        self.b.switch_to_block(on);
        self.forget();
    }

    /// `encoded.rs`'s `SWITCH` arm:
    /// `targets.get(index).unwrap_or(&default)`.
    ///
    /// A jump table, which is the facility this code generator has and the
    /// other arm does not — see `template.rs`'s `switch` for the compare chain
    /// it emits instead, and the harness's report for what the difference
    /// measured.
    ///
    /// The range check in front of it is not redundant. `br_table` selects on an
    /// `i32` and the index is a whole word, so a word above `u32::MAX` would be
    /// truncated into the table; `encoded.rs` takes the default for it. One
    /// unsigned comparison against the table length says so first, and it is
    /// also the comparison that sends a case index past the last case to the
    /// default the way that arm does.
    fn switch(&mut self, on: Slot, table: cove_ir::TableId) {
        let table = self.program.table(table);
        let (default, _) =
            self.blocks[table.default as usize].expect("a switch default begins a block");
        let index = self.load_slot(on);
        let outside = self.b.ins().icmp_imm_u(
            IntCC::UnsignedGreaterThanOrEqual,
            index,
            table.targets.len() as i64,
        );
        let inside = self.b.create_block();
        self.b.ins().brif(outside, default, &[], inside, &[]);
        self.b.switch_to_block(inside);

        let targets: Vec<Block> = table
            .targets
            .iter()
            .map(|target| {
                self.blocks[*target as usize]
                    .expect("a switch target begins a block")
                    .0
            })
            .collect();
        let pool = &mut self.b.func.dfg.value_lists;
        let calls: Vec<BlockCall> = targets
            .iter()
            .map(|block| BlockCall::new(*block, [], pool))
            .collect();
        let fallback = BlockCall::new(default, [], pool);
        let jump = self
            .b
            .func
            .create_jump_table(JumpTableData::new(fallback, &calls));
        let small = self.b.ins().ireduce(types::I32, index);
        self.b.ins().br_table(small, jump);
    }

    /// Raises when `flag` is set, and carries on in a fresh block otherwise.
    ///
    /// The raise goes in a block of its own so the ordinary path is a
    /// not-taken branch rather than a jump over a stretch of stores.
    fn raise_if(&mut self, flag: Value, code: Raise, detail: u32) {
        let bad = self.b.create_block();
        let good = self.b.create_block();
        self.b.ins().brif(flag, bad, &[], good, &[]);
        self.b.switch_to_block(bad);
        self.raise(code, detail);
        self.b.switch_to_block(good);
        // The frame pointer is *not* dropped here. Both blocks are dominated
        // by the one that derived it and nothing between can grow the stack:
        // a raise is a return, not a call.
    }

    fn leave(&mut self, outcome: Outcome) {
        let answer = self.b.ins().iconst(types::I32, i64::from(outcome.abi()));
        self.b.ins().return_(&[answer]);
    }
}
