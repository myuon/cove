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

use cove_ir::{
    ArithOp, CmpOp, Compare, Convert, Function, FunctionId, Inst, Len, Num, Program, Slot, Storage,
    StrId,
};
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
    Entry, GrowableOp, IntrinsicProtocol, NativeCtx, NativeHelpers, Outcome, Raise, RunOp,
    HEAP_CHUNK_SHIFT, HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS,
};
use crate::subset::{
    by_zero_of, leaders, literal_offset, overflow_of, slot_offset, supported, word_finish,
    word_push, WordFinish, WordPush,
};
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

/// The name the allocation helper is imported under. [`SAFEPOINT`]'s note applies.
const ALLOC: &str = "cove_native_alloc";

/// The name the growable-run helper is imported under. [`SAFEPOINT`]'s note
/// applies.
const GROWABLE: &str = "cove_native_growable";

/// The name the run-copy helper is imported under. [`SAFEPOINT`]'s note applies.
const RUN_COPY: &str = "cove_native_run_copy";

/// The name the field-load helper is imported under. [`SAFEPOINT`]'s note
/// applies.
const FIELD_LOAD: &str = "cove_native_field_load";

/// The name the field-store helper is imported under. [`SAFEPOINT`]'s note
/// applies.
const FIELD_STORE: &str = "cove_native_field_store";

/// The name the string-order helper is imported under. [`SAFEPOINT`]'s note
/// applies.
const ORDER_STR: &str = "cove_native_order_str";

/// The name the intrinsic helper is imported under. [`SAFEPOINT`]'s note
/// applies.
const INTRINSIC: &str = "cove_native_intrinsic";

// The `NativeCtx` field offsets, read from the declaration rather than
// written out. `offset_of!` is a const expression, so the numbers compiled
// into the machine code and the numbers Rust uses to read the struct are the
// same numbers by construction; reordering the fields cannot desynchronise
// them.
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
    alloc: FuncId,
    intrinsic: FuncId,
    growable: FuncId,
    run_copy: FuncId,
    field_load: FuncId,
    field_store: FuncId,
    order_str: FuncId,
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
        builder.symbol(ALLOC, helpers.alloc as usize as *const u8);
        builder.symbol(INTRINSIC, helpers.intrinsic as usize as *const u8);
        builder.symbol(GROWABLE, helpers.growable as usize as *const u8);
        builder.symbol(RUN_COPY, helpers.run_copy as usize as *const u8);
        builder.symbol(FIELD_LOAD, helpers.field_load as usize as *const u8);
        builder.symbol(FIELD_STORE, helpers.field_store as usize as *const u8);
        builder.symbol(ORDER_STR, helpers.order_str as usize as *const u8);
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
        let signature = alloc_signature(&module);
        let alloc = module.declare_function(ALLOC, Linkage::Import, &signature)?;
        let signature = intrinsic_signature(&module);
        let intrinsic = module.declare_function(INTRINSIC, Linkage::Import, &signature)?;
        // `GrowableFn` is one pointer, one `I64` and four `I32`s — `IntrinsicFn`'s
        // shape.
        let signature = intrinsic_signature(&module);
        let growable = module.declare_function(GROWABLE, Linkage::Import, &signature)?;
        // And again: `RunCopyFn` is the same six.
        let signature = intrinsic_signature(&module);
        let run_copy = module.declare_function(RUN_COPY, Linkage::Import, &signature)?;
        let signature = field_signature(&module);
        let field_load = module.declare_function(FIELD_LOAD, Linkage::Import, &signature)?;
        let field_store = module.declare_function(FIELD_STORE, Linkage::Import, &signature)?;
        let signature = order_str_signature(&module);
        let order_str = module.declare_function(ORDER_STR, Linkage::Import, &signature)?;
        Ok(Jit {
            ctx: module.make_context(),
            module,
            builder: FunctionBuilderContext::new(),
            safepoint,
            call,
            alloc,
            intrinsic,
            growable,
            run_copy,
            field_load,
            field_store,
            order_str,
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
            let alloc = self.module.declare_func_in_func(self.alloc, builder.func);
            let intrinsic = self
                .module
                .declare_func_in_func(self.intrinsic, builder.func);
            let growable = self
                .module
                .declare_func_in_func(self.growable, builder.func);
            let run_copy = self
                .module
                .declare_func_in_func(self.run_copy, builder.func);
            let field_load = self
                .module
                .declare_func_in_func(self.field_load, builder.func);
            let field_store = self
                .module
                .declare_func_in_func(self.field_store, builder.func);
            let order_str = self
                .module
                .declare_func_in_func(self.order_str, builder.func);
            Lower::new(
                &mut builder,
                program,
                function,
                Bound {
                    safepoint,
                    call,
                    alloc,
                    intrinsic,
                    growable,
                    run_copy,
                    field_load,
                    field_store,
                    order_str,
                },
            )
            .run();
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

/// [`Entry`], in Cranelift's terms.
///
/// The return is `I32` because [`Outcome`] is `#[repr(u32)]`; `base` and
/// `return_base` are `I64` because both are word indices rather than pointers;
/// and `return_slot` is `I32` because a slot is a `u32`, which is one zero
/// extension in [`Lower::ret`] and is the shape ADR 0057 writes down. See
/// [`crate::abi`] for why the signature is this and not something else.
fn entry_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature.params.push(AbiParam::new(types::I64));
    signature.params.push(AbiParam::new(types::I64));
    signature.params.push(AbiParam::new(types::I32));
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

/// [`crate::abi::AllocFn`], in Cranelift's terms.
///
/// `I64` for the answer because it is a linear word address, and `I64` for `len`
/// because it is the `i64` `Machine::allocate` takes — see
/// [`crate::abi::AllocFn`] for why the count is not narrowed on the way.
fn alloc_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    // `pc`, then `layout`.
    signature.params.push(AbiParam::new(types::I32));
    signature.params.push(AbiParam::new(types::I32));
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I64));
    signature
}

/// [`crate::abi::IntrinsicFn`]'s shape, in Cranelift's terms, which
/// [`crate::abi::GrowableFn`] and [`crate::abi::RunCopyFn`] share.
///
/// [`call_signature`]'s shape, for [`call_signature`]'s reason: the answer is an
/// [`Outcome`] and is returned from the compiled function unchanged, so the two
/// widths have to be the one width.
fn intrinsic_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    // `base`, then `pc`, `dst`, `site`, `args`.
    signature.params.push(AbiParam::new(types::I64));
    for _ in 0..4 {
        signature.params.push(AbiParam::new(types::I32));
    }
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

/// [`crate::abi::FieldLoadFn`] and [`crate::abi::FieldStoreFn`], in Cranelift's
/// terms.
///
/// [`call_signature`]'s shape, with two of the five operands as `I64`: `addr`
/// and the destination-or-source address are linear addresses emitted code
/// already formed — [`Lower::load_slot`]'s object and [`Lower::frame_addr`]'s
/// own answer — so there is no `base` to resolve either against.
fn field_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature.params.push(AbiParam::new(types::I32)); // pc
    signature.params.push(AbiParam::new(types::I64)); // addr
    signature.params.push(AbiParam::new(types::I32)); // at
    signature.params.push(AbiParam::new(types::I32)); // width
    signature.params.push(AbiParam::new(types::I64)); // into / from
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

/// [`crate::abi::OrderStrFn`], in Cranelift's terms: the context pointer, the
/// two string words, and the `I64` order.
fn order_str_signature(module: &JITModule) -> Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature.params.push(AbiParam::new(types::I64)); // a
    signature.params.push(AbiParam::new(types::I64)); // b
    signature.returns.push(AbiParam::new(types::I64));
    signature
}

/// The helpers this arm calls, as references inside one function.
///
/// A struct rather than that many parameters of [`Lower::new`], because they
/// arrive together and are never chosen between — which is the shape the fourth
/// one already made necessary and the fifth confirms.
#[derive(Clone, Copy)]
struct Bound {
    safepoint: FuncRef,
    call: FuncRef,
    alloc: FuncRef,
    intrinsic: FuncRef,
    growable: FuncRef,
    run_copy: FuncRef,
    field_load: FuncRef,
    field_store: FuncRef,
    order_str: FuncRef,
}

/// One function's lowering.
struct Lower<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    program: &'a Program,
    function: &'a Function,
    bound: Bound,
    pointer: Type,
    /// The entry parameters. Defined in the entry block, which dominates
    /// every other, so they are readable from anywhere without a block
    /// parameter of their own.
    ctx: Value,
    base: Value,
    /// The destination, as the two indices [`Entry`] is handed. Read only by
    /// [`Lower::ret`], and held here rather than threaded because a `return` can
    /// be the last instruction of any block.
    return_base: Value,
    return_slot: Value,
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
    /// The literal-address table, as a pointer, if it has been loaded in the
    /// block being emitted.
    ///
    /// Cached and forgotten exactly as [`Lower::chunks`] is, and that is
    /// deliberately stricter than the field needs: the table is placed before the
    /// run's first instruction and nothing republishes it, so a helper cannot
    /// stale it. Forgetting all three in one place is worth more than the reload a
    /// function with a literal after a call pays, because a cache with a rule of
    /// its own is a rule somebody has to remember.
    literals: Option<Value>,
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
        bound: Bound,
    ) -> Self {
        let pointer = b.func.signature.params[0].value_type;
        let blocks: Vec<Option<(Block, u32)>> = leaders(program, function)
            .into_iter()
            .map(|length| length.map(|length| (b.create_block(), length)))
            .collect();
        let work = b.declare_var(types::I64);

        // The entry block and its parameters are set up here rather than in
        // `run` so that `ctx` and `base` are never a placeholder: they are
        // read by nearly every method, and a `Value` that is valid only after
        // some other method has run is the sort of invariant that survives
        // exactly as long as nobody reorders anything.
        let (entry, _) = blocks[0].expect("`supported` refused an empty body");
        b.switch_to_block(entry);
        b.append_block_params_for_function_params(entry);
        let ctx = b.block_params(entry)[0];
        let base = b.block_params(entry)[1];
        let return_base = b.block_params(entry)[2];
        let return_slot = b.block_params(entry)[3];
        let zero = b.ins().iconst(types::I64, 0);
        b.def_var(work, zero);

        Lower {
            b,
            program,
            function,
            bound,
            pointer,
            ctx,
            base,
            return_base,
            return_slot,
            work,
            frame: None,
            chunks: None,
            literals: None,
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
            // `encoded.rs`'s `CONST_UNIT` arm: one store of a zero word.
            Inst::Unit { dst } => {
                let word = self.b.ins().iconst(types::I64, 0);
                self.store_slot(*dst, word);
                false
            }
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
            Inst::Str { dst, text } => {
                self.literal(*dst, *text);
                false
            }
            Inst::Copy { dst, src, layout } => {
                self.copy(*dst, *src, self.program.layout(*layout).width());
                false
            }
            // `encoded.rs`'s `CLEAR` arm: `clear_words(base + slot, width)`. A
            // frame slot is a stack address by construction, so this is the
            // `is_stack` branch's stack arm with nothing to decide — one store of
            // a zero word per word of the layout.
            Inst::Clear { slot, layout } => {
                let width = self.program.layout(*layout).width();
                if width > 0 {
                    let zero = self.b.ins().iconst(types::I64, 0);
                    for word in 0..width {
                        self.store_slot(slot + word, zero);
                    }
                }
                false
            }
            // `encoded.rs`'s `ADDR_OF_SLOT` arm: `base + slot`, where `base` is
            // the frame's *linear* address and not the index the entry point was
            // handed. The two differ by the segment origin, which is why
            // `NativeCtx::stack_origin` exists — see `crate::abi`.
            Inst::AddrOfSlot { dst, slot } => {
                let base = self.frame_addr();
                let addr = self.b.ins().iadd_imm_s(base, i64::from(*slot));
                self.store_slot(*dst, addr);
                false
            }
            // `encoded.rs`'s `ADDR_OF_PART` arm, and the comment there is the
            // whole of it: "Arithmetic and nothing else."
            Inst::AddrOfPart { dst, addr, at } => {
                let held = self.load_slot(*addr);
                let moved = self.b.ins().iadd_imm_s(held, i64::from(*at));
                self.store_slot(*dst, moved);
                false
            }
            Inst::Load { dst, addr, layout } => {
                self.load_through(*dst, *addr, self.program.layout(*layout).width());
                false
            }
            Inst::Store { addr, src, layout } => {
                self.store_through(*addr, *src, self.program.layout(*layout).width());
                false
            }
            Inst::LoadField {
                dst,
                obj,
                at,
                layout,
            } => {
                self.load_field(*dst, *obj, *at, self.program.layout(*layout).width());
                false
            }
            Inst::StoreField {
                obj,
                at,
                src,
                layout,
            } => {
                self.store_field(*obj, *at, *src, self.program.layout(*layout).width());
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
            // `encoded.rs`'s `INT_TO_FLOAT` arm, `x as f64`, and the float's
            // bits stored as the word they are.
            Inst::Convert {
                to: Convert::IntToFloat,
                dst,
                a,
            } => {
                let x = self.load_slot(*a);
                let float = self.b.ins().fcvt_from_sint(types::F64, x);
                let bits = self.b.ins().bitcast(types::I64, MemFlagsData::new(), float);
                self.store_slot(*dst, bits);
                false
            }
            // A relabel: the word moves unchanged.
            Inst::Convert {
                to: Convert::DurationToInt | Convert::IntToDuration,
                dst,
                a,
            } => {
                let x = self.load_slot(*a);
                self.store_slot(*dst, x);
                false
            }
            Inst::Len { dst, obj } => {
                self.len_of(*dst, *obj);
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
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            } => {
                self.store_elem(*obj, *index, *src, self.program.layout(*layout).width());
                false
            }
            Inst::RunLoad {
                dst,
                run,
                index,
                storage: Storage::PackedBytes,
            } => {
                self.byte_at(*dst, *run, *index);
                false
            }
            Inst::Call { dst, callee, args } => {
                self.callee(*dst, callee.0, args.0);
                false
            }
            // ADR 0052's four, each handed to the runtime whole. See
            // [`crate::abi::GrowableFn`] for why none of them has an emitted fast
            // path — one rooting discipline that is not the frame's, one chunked
            // safepoint contract, and one UTF-8 walk.
            Inst::GrowableAlloc {
                dst,
                capacity,
                storage: Storage::PackedBytes,
            } => {
                self.growable_op(GrowableOp::Alloc, *dst, *capacity);
                false
            }
            Inst::GrowablePush {
                owner,
                src,
                storage: Storage::PackedBytes,
            } => {
                self.growable_op(GrowableOp::Push, *owner, *src);
                false
            }
            // `Vector.push`: a fast path into spare capacity and the whole push as
            // its cold half. See [`WordPush`](crate::subset::WordPush), decoded
            // by the subset so that the two arms read one set of facts.
            Inst::GrowablePush {
                owner,
                src,
                storage: Storage::Words(elem),
            } => {
                let push = word_push(self.program, *owner, *src, *elem)
                    .expect("`supported` admitted a word push it could decode");
                self.vector_push(push);
                false
            }
            Inst::GrowableExtend {
                args,
                storage: Storage::PackedBytes,
            } => {
                self.growable_op(GrowableOp::Extend, args.0, 0);
                false
            }
            // `Vector.pop` and `Vector.remove`'s truncate, handed over whole.
            Inst::GrowableTruncate {
                owner,
                len,
                storage: Storage::Words(_),
            } => {
                self.growable_op(GrowableOp::TruncateWords, *owner, *len);
                false
            }
            Inst::RunFinish {
                dst,
                owner,
                storage: Storage::PackedBytes,
                ..
            } => {
                self.growable_op(GrowableOp::Finish, *dst, *owner);
                false
            }
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
                self.vector_freeze(finish);
                false
            }
            // ADR 0058's `run-copy`, handed to the runtime whole. See
            // [`crate::abi::RunCopyFn`] for why it has no emitted loop — memmove in
            // bounded chunks with a poll between them, and refusals whose
            // sentences only the runtime can build.
            Inst::RunCopy { args, storage } => {
                let (kind, elem) = match storage {
                    Storage::PackedBytes => (RunOp::CopyBytes, 0),
                    Storage::Words(elem) => (RunOp::CopyWords, elem.0),
                };
                self.run_copy(args.0, kind, elem);
                false
            }
            // ADR 0058's `run-slice`: the same helper, which allocates the run
            // and writes it into the row's `dst` before it copies into it.
            Inst::RunSlice { args, storage } => {
                let (kind, elem) = match storage {
                    Storage::PackedBytes => (RunOp::SliceBytes, 0),
                    Storage::Words(elem) => (RunOp::SliceWords, elem.0),
                };
                self.run_copy(args.0, kind, elem);
                false
            }
            Inst::Alloc { dst, layout, len } => {
                self.allocate(*dst, layout.0, *len);
                false
            }
            Inst::Switch { on, table } => {
                self.switch(*on, *table);
                true
            }
            // `encoded.rs`'s `NEG_INT` arm: `checked_neg`, whose `None` is
            // `overflowed("negation")`. Cranelift has no `sneg_overflow`, so the
            // one operand `checked_neg` refuses is named directly — `i64::MIN`,
            // whose negation is not an `i64` — and `ineg` runs on everything else.
            Inst::Neg {
                num: Num::Int,
                dst,
                a,
            } => {
                let x = self.load_slot(*a);
                let least = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                self.raise_if(least, Raise::NegOverflowed, 0);
                let value = self.b.ins().ineg(x);
                self.store_slot(*dst, value);
                false
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
            // `encoded.rs`'s `ORDER_STR`: one call of the leaf
            // [`crate::abi::OrderStrFn`] over the two words, its `I64` answer
            // stored whole. A leaf, so nothing is published before the call and
            // nothing is forgotten after it: the frame and chunk pointers this
            // block cached are still the pointers (#378, Q4.14).
            Inst::Cmp {
                on: Compare::Str,
                op: CmpOp::Order,
                dst,
                a,
                b,
            } => {
                let x = self.load_slot(*a);
                let y = self.load_slot(*b);
                let call = self.b.ins().call(self.bound.order_str, &[self.ctx, x, y]);
                let answer = self.b.inst_results(call)[0];
                self.store_slot(*dst, answer);
                false
            }
            // `encoded.rs`'s `ORDER_INT | ORDER_BOOL | ORDER_TAG`: `(x > y) -
            // (x < y)` over the words read as `i64`, which `crate::subset`
            // admits for those three and no other.
            Inst::Cmp {
                on: _,
                op: CmpOp::Order,
                dst,
                a,
                b,
            } => {
                let x = self.load_slot(*a);
                let y = self.load_slot(*b);
                let above = self.b.ins().icmp(IntCC::SignedGreaterThan, x, y);
                let below = self.b.ins().icmp(IntCC::SignedLessThan, x, y);
                let above = self.b.ins().uextend(types::I64, above);
                let below = self.b.ins().uextend(types::I64, below);
                let answer = self.b.ins().isub(above, below);
                self.store_slot(*dst, answer);
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
            // `encoded.rs`'s `INTRINSIC_CALL` arm, through the one helper. See
            // [`Lower::intrinsic_call`].
            Inst::IntrinsicCall { dst, site, args } => {
                self.intrinsic_call(*dst, *site, *args);
                false
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
        self.literals = None;
    }

    /// `encoded.rs`'s `STR` arm: `literal_addr(text)`, into a slot.
    ///
    /// Two loads and a store, and the address is not an immediate: see
    /// [`crate::abi`]'s "A literal's address is a run-time load" for why it cannot
    /// be one. The table pointer is cached exactly as [`Lower::heap_chunks`]'s is,
    /// which is stricter than it has to be — the literal table is placed before
    /// any frame exists and is never republished, so no helper can stale it — and
    /// is written that way so the two tables are forgotten in one place rather
    /// than in two with different rules.
    fn literal(&mut self, dst: Slot, text: StrId) {
        let at = literal_offset(text).expect("`supported` bounded every literal");
        let table = self.literals();
        let addr = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), table, at);
        self.store_slot(dst, addr);
    }

    /// The literal-address table, as a pointer. See [`Lower::literal`].
    fn literals(&mut self) -> Value {
        if let Some(literals) = self.literals {
            return literals;
        }
        let literals = self.b.ins().load(
            self.pointer,
            MemFlagsData::trusted(),
            self.ctx,
            OFF_LITERALS,
        );
        self.literals = Some(literals);
        literals
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

    /// The address of the heap word at the linear address `addr`, as a pointer.
    ///
    /// `Memory::read`'s heap half, which is `Space::load`: subtract the heap
    /// origin, find the chunk, and index inside it.
    fn heap_ptr(&mut self, addr: Value) -> Value {
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
        self.b.ins().iadd(chunk, offset)
    }

    /// The heap word at the linear address `addr`.
    ///
    /// The `Relaxed` atomic load `Space::load` performs is a plain load on every
    /// target either arm runs on, and ADR 0034's argument for why `Relaxed` is
    /// enough — the ordering that makes one task's writes visible to another is
    /// the release/acquire pair on a cell's lock word — is unchanged by the load
    /// being emitted here instead.
    fn heap_word(&mut self, addr: Value) -> Value {
        let word = self.heap_ptr(addr);
        self.b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), word, 0)
    }

    /// The address of the stack word at the linear address `addr`, as a pointer.
    ///
    /// `Stack::at`'s subtraction and nothing else: `words[addr - origin]`. The
    /// origin is re-read from the context rather than cached, because it is read
    /// only by the address family and a register held across a block would cost
    /// every function that has no address in it.
    fn stack_ptr(&mut self, addr: Value) -> Value {
        let words = self
            .b
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), self.ctx, OFF_WORDS);
        let origin = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.ctx,
            OFF_STACK_ORIGIN,
        );
        let index = self.b.ins().isub(addr, origin);
        let bytes = self.b.ins().ishl_imm_u(index, 3);
        self.b.ins().iadd(words, bytes)
    }

    /// The address of the word at the linear address `addr`, whichever region it
    /// names.
    ///
    /// `Memory::read`'s and `Memory::write`'s shared first line — `is_stack(addr)`,
    /// which is `addr < HEAP_ORIGIN_WORDS` — emitted rather than called. See
    /// [`crate::abi`]'s "An address names either region" for why it is emitted:
    /// the two arms are four instructions and eleven, and a helper call would cost
    /// more than either *and* end the span in which a cached
    /// [`NativeCtx::words`] may be trusted.
    ///
    /// The branch cannot be hoisted out of a multi-word run and is not: each word
    /// of a `Load` asks again. That is the same shape `Memory::copy_words` has for
    /// `words == 1` — a read and a write, region-decoded each time — and it is
    /// what makes a run that straddles a heap chunk boundary correct without a
    /// second rule.
    fn word_ptr(&mut self, addr: Value) -> Value {
        // What is cached *here* is what the join block is dominated by, and it is
        // restored below for exactly that reason: a pointer derived inside one of
        // the two arms is defined in a block that does not dominate the other arm
        // or the join, so reusing it there is a verifier error — and it is one this
        // did have, as `uses value v23 from non-dominating inst20`, the second time
        // a two-word load asked for the chunk table.
        let outer = (self.frame, self.chunks);
        let stack = self.b.create_block();
        let heap = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, self.pointer);

        let origin = self.b.ins().iconst(types::I64, HEAP_ORIGIN_WORDS as i64);
        let below = self.b.ins().icmp(IntCC::UnsignedLessThan, addr, origin);
        self.b.ins().brif(below, stack, &[], heap, &[]);

        self.b.switch_to_block(stack);
        let held = self.stack_ptr(addr);
        self.b.ins().jump(join, &[held.into()]);

        self.b.switch_to_block(heap);
        let held = self.heap_ptr(addr);
        self.b.ins().jump(join, &[held.into()]);

        // Neither is *forgotten* — nothing here can grow the stack or commit a
        // chunk, so a pointer that was live before the branch is still live after
        // it — but neither may have gained a definition inside an arm.
        self.b.switch_to_block(join);
        (self.frame, self.chunks) = outer;
        self.b.block_params(join)[0]
    }

    /// This frame's first word, as a **linear address** rather than a pointer.
    ///
    /// What `encoded.rs` calls `base`, which is the number an `addr-of-slot` adds
    /// its slot to. The entry point is handed the frame as a segment-relative
    /// *index*, for the reallocation reason [`crate::abi`] gives, so the origin
    /// has to be added back to get the address a `Repr::Addr` word carries — and
    /// it has to be the same number the VM would have formed, because the two
    /// tiers pass these words to each other.
    fn frame_addr(&mut self) -> Value {
        let origin = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.ctx,
            OFF_STACK_ORIGIN,
        );
        self.b.ins().iadd(origin, self.base)
    }

    /// `encoded.rs`'s `LOAD` arm: `copy_words(base + dst, addr, width)`.
    ///
    /// Every word is read before any is written, for [`Lower::copy`]'s reason and
    /// with one more behind it: `copy_words` is a `memmove` where both runs are on
    /// the stack, and an address formed by `addr-of-slot` from *this* frame makes
    /// that case reachable — `load s3 <- &s1` with the runs overlapping is
    /// something the lowering may emit and does not have to prove it does not.
    fn load_through(&mut self, dst: Slot, addr: Slot, width: u32) {
        let base = self.load_slot(addr);
        let mut held = Vec::with_capacity(width as usize);
        for word in 0..width {
            let at = self.b.ins().iadd_imm_s(base, i64::from(word));
            let ptr = self.word_ptr(at);
            held.push(
                self.b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), ptr, 0),
            );
        }
        for (word, value) in held.into_iter().enumerate() {
            self.store_slot(dst + word as u32, value);
        }
    }

    /// `encoded.rs`'s `STORE` arm: `copy_words(addr, base + src, width)`.
    ///
    /// [`Lower::load_through`]'s order, in the other direction and for the same
    /// reason.
    fn store_through(&mut self, addr: Slot, src: Slot, width: u32) {
        let base = self.load_slot(addr);
        let mut held = Vec::with_capacity(width as usize);
        for word in 0..width {
            held.push(self.load_slot(src + word));
        }
        for (word, value) in held.into_iter().enumerate() {
            let at = self.b.ins().iadd_imm_s(base, word as i64);
            let ptr = self.word_ptr(at);
            self.b.ins().store(MemFlagsData::trusted(), value, ptr, 0);
        }
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

    /// `encoded.rs`'s `ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT` arm: the runtime
    /// allocates, and this stores the address it answered.
    ///
    /// See [`crate::abi::AllocFn`] for why none of `Machine::allocate` is emitted
    /// and for what the zero answer means. What is emitted is the hand-over and
    /// [`Lower::callee`]'s three things around it: the unpaid work published and the
    /// accumulator cleared, because the helper is a safepoint and charges what it
    /// finds; both cached pointers dropped, because a collection may have grown the
    /// stack and an allocation may have committed a heap chunk; and a zero answer
    /// left with as [`Raise::Called`], because the runtime is holding the whole
    /// error.
    fn allocate(&mut self, dst: Slot, layout: u32, len: Len) {
        // The count is formed before the work is published, which is arbitrary here
        // — Cranelift orders by data flow — and is written this way so the two arms
        // read alike.
        let count = match len {
            // `Len::Fixed`'s count is nought, which is what the encoded arm hands
            // `Machine::allocate` for it: the layout already fixes the size.
            Len::Fixed => self.b.ins().iconst(types::I64, 0),
            Len::Count(count) => self.b.ins().iconst(types::I64, i64::from(count)),
            // Read as a whole word and handed over as one. A negative count, or one
            // past what the header's length field holds, is the *helper's* to refuse
            // — see `Machine::allocate` — so narrowing it here would be a second
            // refusal with a different message.
            Len::Slot(at) => self.load_slot(at),
        };
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);

        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        let which = self.b.ins().iconst(types::I32, i64::from(layout));
        let call = self
            .b
            .ins()
            .call(self.bound.alloc, &[self.ctx, at, which, count]);
        let addr = self.b.inst_results(call)[0];
        self.forget();

        // Zero is not an address — the heap begins at `HEAP_ORIGIN_WORDS` — so the
        // one test says both "it refused" and "the runtime has the sentence".
        let refused = self.b.ins().icmp_imm_s(IntCC::Equal, addr, 0);
        self.raise_if(refused, Raise::Called, 0);
        self.store_slot(dst, addr);
    }

    /// A word `growable-push`'s fast path — `Vector.push` — the element into
    /// spare capacity, and the length bumped.
    ///
    /// See [`WordPush`](crate::subset::WordPush) for which of
    /// `Machine::push_words`' preconditions are emitted and which go to
    /// [`GrowableFn`](crate::abi::GrowableFn), and why. The three tests in front
    /// of the write are the vector layout the element implies against the
    /// object's own header, the store word against nought — which is what
    /// `freeze()` leaves — and the length against the capacity. Each failure is
    /// a *cold* path and all three share one, because what happens there is the
    /// same thing: the VM performs the whole push.
    ///
    /// The comparison of length against capacity is unsigned and that is exact
    /// rather than clever: the length is a payload word narrowed to `u32` and the
    /// capacity is a header's low half, so both are below 2^32 and
    /// `UnsignedGreaterThanOrEqual` is `items.len < items.capacity` read the other
    /// way.
    fn vector_push(&mut self, push: WordPush) {
        let WordPush {
            owner,
            vector,
            src,
            stride,
        } = push;
        let cold = self.b.create_block();
        let join = self.b.create_block();

        let header = self.load_slot(owner);
        // `Machine::vector_run`'s `if owner == 0 { null_object() }`, which is the
        // one refusal of a push this crate can name.
        self.refuse_null(header);

        // `machine.object_layout(addr)`: the header's high half. The element
        // layout is what every static fact below was derived from, so a header
        // that is not the vector of it is an owner this code cannot push to.
        let word = self.heap_word(header);
        let named = self.b.ins().ushr_imm_u(word, 32);
        let wrong = self
            .b
            .ins()
            .icmp_imm_u(IntCC::NotEqual, named, i64::from(vector.0));
        let known = self.b.create_block();
        self.b.ins().brif(wrong, cold, &[], known, &[]);
        self.b.switch_to_block(known);

        // `machine.payload(addr, 1)`: the store. `freeze()` leaves nought here.
        let one = self.b.ins().iconst(types::I64, 1);
        let store = self.payload(header, one);
        let frozen = self.b.ins().icmp_imm_s(IntCC::Equal, store, 0);
        let live = self.b.create_block();
        self.b.ins().brif(frozen, cold, &[], live, &[]);
        self.b.switch_to_block(live);

        // `machine.payload(addr, 0) as u32` against `machine.object_len(store)`.
        let zero = self.b.ins().iconst(types::I64, 0);
        let held = self.payload(header, zero);
        let len = self.b.ins().band_imm_u(held, LEN_MASK);
        let capacity = self.object_len(store);
        let full = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, len, capacity);
        let room = self.b.create_block();
        self.b.ins().brif(full, cold, &[], room, &[]);
        self.b.switch_to_block(room);

        // Where the element goes: `store + 1 + len * stride`. The stride is a
        // compile-time constant and the product is formed in `u64`, which agrees
        // with `set_payload_run`'s `u32` on every product the test above admits —
        // the store's payload words are a `u32` and this is inside them.
        let at = self.b.ins().imul_imm_s(len, i64::from(stride));
        for word in 0..stride {
            let held = self.load_slot(src + word);
            let into = self.b.ins().iadd_imm_s(at, i64::from(word));
            self.set_payload(store, into, held);
        }
        // `growable_commit`: `machine.set_payload(owner, 0, len + 1)`.
        let grown = self.b.ins().iadd_imm_s(len, 1);
        self.set_payload(header, zero, grown);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(cold);
        self.growable_op(GrowableOp::PushWords, owner, src);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(join);
        // One predecessor of this join came through a helper, so neither pointer the
        // other one derived is to be trusted here.
        self.forget();
    }

    /// A word `run-finish` — `Vector.freeze()` — as `Memory::relabel` turning
    /// the store into the `Array<T>` it already holds, in place, and the two
    /// words of the `Vector` header that mark it consumed.
    ///
    /// See [`WordFinish`](crate::subset::WordFinish) for which preconditions are
    /// emitted and which go to [`GrowableFn`](crate::abi::GrowableFn): the vector
    /// layout the element implies against the object's own header, and the store
    /// word against nought, exactly as [`Lower::vector_push`]'s are. What is new is that there is no
    /// third cold half — `relabel` is O(1) whatever `len` and `capacity` are, so
    /// every precondition that holds is answered here and nothing is bounded by
    /// a run.
    ///
    /// `spare`, `payload` and the new header word are `Memory::relabel`'s own
    /// arithmetic, read off `Machine::relabel`'s wrapper: `payload = len *
    /// stride` is the `Array`'s own payload width, which is where the free
    /// block — if there is one — begins, and `spare = (capacity - len) *
    /// stride` is what it releases in words.
    fn vector_freeze(&mut self, finish: WordFinish) {
        let WordFinish {
            dst,
            owner: recv,
            vector,
            stride,
            array,
        } = finish;
        let cold = self.b.create_block();
        let join = self.b.create_block();

        let header = self.load_slot(recv);
        // `vector()`'s `if addr == 0 { null_value() }`, which is the one
        // refusal of this builtin a program reaches and this crate can name.
        self.refuse_null(header);

        // `machine.object_layout(addr)`: the header's high half.
        let word = self.heap_word(header);
        let named = self.b.ins().ushr_imm_u(word, 32);
        let wrong = self
            .b
            .ins()
            .icmp_imm_u(IntCC::NotEqual, named, i64::from(vector.0));
        let known = self.b.create_block();
        self.b.ins().brif(wrong, cold, &[], known, &[]);
        self.b.switch_to_block(known);

        // `machine.payload(addr, 1)`: the store. A second `freeze()` leaves
        // nought here.
        let one = self.b.ins().iconst(types::I64, 1);
        let store = self.payload(header, one);
        let frozen = self.b.ins().icmp_imm_s(IntCC::Equal, store, 0);
        let live = self.b.create_block();
        self.b.ins().brif(frozen, cold, &[], live, &[]);
        self.b.switch_to_block(live);

        // `items.len` and `items.capacity`, `Lower::vector_push`'s own reads.
        let zero = self.b.ins().iconst(types::I64, 0);
        let held = self.payload(header, zero);
        let len = self.b.ins().band_imm_u(held, LEN_MASK);
        let capacity = self.object_len(store);

        let stride_val = self.b.ins().iconst(types::I64, i64::from(stride));
        // `payload = len * stride`: the `Array`'s own payload width, and where
        // the free block — if there is one — begins.
        let payload_words = self.b.ins().imul(len, stride_val);
        // `spare = (capacity - len) * stride`, in words.
        let spare_elems = self.b.ins().isub(capacity, len);
        let spare_words = self.b.ins().imul(spare_elems, stride_val);

        // `self.write(addr, header(layout, len))`: the array's own bits do not
        // overlap the length's, so the OR `mem::header` performs is an add.
        let array_id = self.b.ins().iconst(types::I64, i64::from(array.0));
        let shifted = self.b.ins().ishl_imm_u(array_id, 32);
        let new_header = self.b.ins().iadd(shifted, len);
        let store_ptr = self.heap_ptr(store);
        self.b
            .ins()
            .store(MemFlagsData::trusted(), new_header, store_ptr, 0);

        // `if spare > 0 { self.write(addr + 1 + payload, header(FREE, spare - 1)) }`.
        // `LayoutId::FREE` is `0`, so that header word is `spare - 1` alone.
        let has_spare = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThan, spare_words, 0);
        let write_spare = self.b.create_block();
        let after_spare = self.b.create_block();
        self.b
            .ins()
            .brif(has_spare, write_spare, &[], after_spare, &[]);
        self.b.switch_to_block(write_spare);
        let free_len = self.b.ins().iadd_imm_s(spare_words, -1);
        let at_free = self.b.ins().iadd_imm_s(store, 1);
        let at_free = self.b.ins().iadd(at_free, payload_words);
        let free_ptr = self.heap_ptr(at_free);
        self.b
            .ins()
            .store(MemFlagsData::trusted(), free_len, free_ptr, 0);
        self.b.ins().jump(after_spare, &[]);
        self.b.switch_to_block(after_spare);

        // `machine.set_payload(items.header, 0, 0)` and `(items.header, 1, 0)`:
        // the `Vector`'s own two words, cleared — `freeze()`'s mark, the same
        // nought `vector()` refuses a later push or set against.
        self.set_payload(header, zero, zero);
        self.set_payload(header, one, zero);

        // The answer is the store's own address: `relabel` moved nothing.
        self.store_slot(dst, store);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(cold);
        self.growable_op(GrowableOp::FinishWords, dst, recv);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(join);
        self.forget();
    }

    /// One of [ADR 0052]'s four growable-buffer instructions, handed to the
    /// runtime whole.
    ///
    /// [`Lower::callee`]'s shape, with the operand pair and one more argument
    /// saying which operation this is in place of the callee and its argument
    /// list. See [`crate::abi::GrowableFn`] for what each operand means and
    /// why all of it is the helper rather than a fast path and a cold one.
    ///
    /// It is a safepoint: a `growable-alloc` allocates twice, an `append` may grow
    /// the store, and a `finish` walks the live prefix and charges what it moved.
    ///
    /// [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
    fn growable_op(&mut self, op: GrowableOp, a: u32, b: u32) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);

        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        let which = self.b.ins().iconst(types::I32, i64::from(op.abi()));
        let first = self.b.ins().iconst(types::I32, i64::from(a));
        let second = self.b.ins().iconst(types::I32, i64::from(b));
        let call = self.b.ins().call(
            self.bound.growable,
            &[self.ctx, self.base, at, which, first, second],
        );
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
        // Not `leave`: what this returns is the helper's outcome and not one this
        // function chose, and every field that outcome needs the helper has written.
        self.b.ins().return_(&[outcome]);

        self.b.switch_to_block(on);
        self.forget();
    }

    /// One [ADR 0058] `run-copy` or `run-slice`, handed to the runtime whole.
    ///
    /// [`Lower::growable_op`]'s shape exactly, with the argument list, the
    /// [`RunOp`] and the element in place of the operation and its pair. See
    /// [`crate::abi::RunCopyFn`] for what the operands mean and why the copy is the
    /// helper's.
    ///
    /// It is a safepoint: the helper takes one before the copy, a slice
    /// allocates, and a long copy polls between chunks, any of which may collect.
    ///
    /// [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    fn run_copy(&mut self, args: u32, kind: RunOp, elem: u32) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);

        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        let list = self.b.ins().iconst(types::I32, i64::from(args));
        let words = self.b.ins().iconst(types::I32, i64::from(kind.abi()));
        let elem = self.b.ins().iconst(types::I32, i64::from(elem));
        let call = self.b.ins().call(
            self.bound.run_copy,
            &[self.ctx, self.base, at, list, words, elem],
        );
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
        // Not `leave`: what this returns is the helper's outcome and not one this
        // function chose, and every field that outcome needs the helper has written.
        self.b.ins().return_(&[outcome]);

        self.b.switch_to_block(on);
        self.forget();
    }

    /// One `intrinsic-call`, handed to [`crate::abi::IntrinsicFn`] with the
    /// protocol its effects ask for.
    ///
    /// [`Lower::growable_op`]'s shape, with the destination, the site and the
    /// argument list in place of the operation and its pair — and everything
    /// around the call read off one [`IntrinsicProtocol`], which is the whole of
    /// this arm's decision and the template arm's `Emit::intrinsic_call` reads the
    /// same one:
    ///
    /// - a **safepoint** publishes and clears the work before the call and forgets
    ///   every cached pointer after it, exactly as [`Lower::growable_op`] does;
    /// - an intrinsic that **cannot collect** does neither: the helper charges
    ///   nothing, so the work stays in its variable, and it can neither grow the
    ///   stack nor commit a chunk, so the frame pointer derived before the call is
    ///   still the frame's;
    /// - the outcome is tested only where [`IntrinsicProtocol::tests_outcome`] says
    ///   an answer other than `Returned` can come back, and an exit that did not
    ///   publish before the call publishes on the way out, for
    ///   [`Lower::field_call`]'s reason.
    fn intrinsic_call(&mut self, dst: Slot, site: cove_ir::SiteId, args: cove_ir::ArgsId) {
        let protocol = IntrinsicProtocol::of(self.program.intrinsic_site(site).intrinsic);
        if protocol.safepoint {
            let work = self.b.use_var(self.work);
            self.store_ctx(OFF_PENDING_WORK, work);
            let zero = self.b.ins().iconst(types::I64, 0);
            self.b.def_var(self.work, zero);
        }

        let at = self.b.ins().iconst(types::I32, self.pc as i64);
        let into = self.b.ins().iconst(types::I32, i64::from(dst));
        let which = self.b.ins().iconst(types::I32, i64::from(site.0));
        let list = self.b.ins().iconst(types::I32, i64::from(args.0));
        let call = self.b.ins().call(
            self.bound.intrinsic,
            &[self.ctx, self.base, at, into, which, list],
        );
        let outcome = self.b.inst_results(call)[0];
        if protocol.safepoint {
            self.forget();
        }

        if protocol.tests_outcome() {
            let left = self.b.create_block();
            let on = self.b.create_block();
            let returned =
                self.b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, outcome, i64::from(Outcome::Returned.abi()));
            self.b.ins().brif(returned, on, &[], left, &[]);

            self.b.switch_to_block(left);
            if !protocol.safepoint {
                let work = self.b.use_var(self.work);
                self.store_ctx(OFF_PENDING_WORK, work);
            }
            // Not `leave`: what this returns is the helper's outcome and not one
            // this function chose, and every field that outcome needs the helper
            // has written.
            self.b.ins().return_(&[outcome]);

            self.b.switch_to_block(on);
            // The only predecessor is the call's block, so a pointer cached
            // before a call that cannot collect still dominates this one.
            if protocol.safepoint {
                self.forget();
            }
        }
    }

    /// `Memory::set_payload`: payload word `at` of the object at `addr`, written.
    ///
    /// [`Lower::payload`] in the other direction, and the `+ 1` is the same one.
    fn set_payload(&mut self, addr: Value, at: Value, word: Value) {
        let one = self.b.ins().iadd_imm_s(at, 1);
        let which = self.b.ins().iadd(addr, one);
        let ptr = self.heap_ptr(which);
        self.b.ins().store(MemFlagsData::trusted(), word, ptr, 0);
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
        let addr = self.load_slot(obj);
        self.refuse_null(addr);
        let len = self.object_len(addr);
        self.store_slot(dst, len);
    }

    /// Refuses a null reference, which every reader of an object does first.
    ///
    /// `Machine::element`, `encoded.rs`'s `LEN` and its `RUN_LOAD_BYTES` each begin
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

    /// `encoded.rs`'s `STORE_ELEM` arm: `Machine::element`, and then a copy of
    /// `width` words *into* the payload.
    ///
    /// [`Lower::load_elem`] backwards, with the same one unsigned comparison and
    /// the same stride. Nothing is held back for overlap the way [`Lower::copy`]
    /// holds words back: the source is a frame and the destination is the heap,
    /// which are two regions.
    fn store_elem(&mut self, obj: Slot, index: Slot, src: Slot, width: u32) {
        let addr = self.load_slot(obj);
        self.refuse_null(addr);
        let index = self.load_slot(index);
        let len = self.object_len(addr);
        let outside = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, len);
        self.raise_range(outside, Raise::IndexOutOfRange, index, len);
        let at = self.b.ins().imul_imm_s(index, i64::from(width));
        for word in 0..width {
            let held = self.load_slot(src + word);
            let into = self.b.ins().iadd_imm_s(at, i64::from(word));
            self.set_payload(addr, into, held);
        }
    }

    /// `encoded.rs`'s `RUN_LOAD_BYTES` arm: a payload read, a shift and a mask.
    ///
    /// The bound is the string's *byte* length and the refusal is not
    /// `Array.get`'s — see [`Inst::RunLoad`](cove_ir::Inst::RunLoad) for why a
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

    /// `NativeCtx::fixed_payload_words[layout]`, where `layout` is the header's
    /// high half of the object at `addr` — `0` for a variable-payload shape.
    ///
    /// One load of the table pointer and one of the entry it names. The table is
    /// not cached the way [`Lower::frame`], [`Lower::heap_chunks`] and
    /// [`Lower::literals`] are, because a field access reads it once and a second
    /// read within the same instruction never happens.
    fn fixed_payload_words(&mut self, addr: Value) -> Value {
        let word = self.heap_word(addr);
        let layout = self.b.ins().ushr_imm_u(word, 32);
        let table = self.b.ins().load(
            self.pointer,
            MemFlagsData::trusted(),
            self.ctx,
            OFF_FIXED_PAYLOAD_WORDS,
        );
        // Four bytes per `u32` entry, not eight: this table is not `chunks` or
        // `literals`.
        let offset = self.b.ins().ishl_imm_u(layout, 2);
        let entry = self.b.ins().iadd(table, offset);
        let value = self
            .b
            .ins()
            .load(types::I32, MemFlagsData::trusted(), entry, 0);
        self.b.ins().uextend(types::I64, value)
    }

    /// `encoded.rs`'s `LOAD_FIELD` arm: `Machine::checked` and a copy of `width`
    /// words out of the object's payload — with the bound answered in emitted
    /// code wherever [`Lower::fixed_payload_words`] can answer it.
    ///
    /// `at` and `width` are both compile-time constants, so the comparison this
    /// makes is `words < at + width` against one immediate rather than a runtime
    /// addition — `Machine::checked`'s `at + width > words` turned around and
    /// folded. A `0` table entry always takes the cold path for a non-empty
    /// field, which is what sends a variable-payload object to
    /// [`crate::abi::FieldLoadFn`] without this arm ever asking which shape it
    /// is.
    fn load_field(&mut self, dst: Slot, obj: Slot, at: u32, width: u32) {
        let cold = self.b.create_block();
        let fast = self.b.create_block();
        let join = self.b.create_block();

        let addr = self.load_slot(obj);
        self.refuse_null(addr);

        let words = self.fixed_payload_words(addr);
        let short = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThan, words, i64::from(at + width));
        self.b.ins().brif(short, cold, &[], fast, &[]);

        self.b.switch_to_block(fast);
        for word in 0..width {
            let off = self.b.ins().iadd_imm_s(addr, i64::from(1 + at + word));
            let ptr = self.heap_ptr(off);
            let value = self
                .b
                .ins()
                .load(types::I64, MemFlagsData::trusted(), ptr, 0);
            self.store_slot(dst + word, value);
        }
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(cold);
        // `into`: the frame's own linear address plus the destination slot,
        // [`Inst::AddrOfSlot`](cove_ir::Inst::AddrOfSlot)'s arithmetic, because
        // that is what [`crate::abi::FieldLoadFn`] copies the answer to.
        let frame_addr = self.frame_addr();
        let into = self.b.ins().iadd_imm_s(frame_addr, i64::from(dst));
        self.field_call(self.bound.field_load, addr, at, width, into);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(join);
        self.forget();
    }

    /// [`Lower::load_field`], the other direction: `encoded.rs`'s `STORE_FIELD`
    /// arm.
    fn store_field(&mut self, obj: Slot, at: u32, src: Slot, width: u32) {
        let cold = self.b.create_block();
        let fast = self.b.create_block();
        let join = self.b.create_block();

        let addr = self.load_slot(obj);
        self.refuse_null(addr);

        let words = self.fixed_payload_words(addr);
        let short = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThan, words, i64::from(at + width));
        self.b.ins().brif(short, cold, &[], fast, &[]);

        self.b.switch_to_block(fast);
        for word in 0..width {
            let held = self.load_slot(src + word);
            let off = self.b.ins().iadd_imm_s(addr, i64::from(1 + at + word));
            let ptr = self.heap_ptr(off);
            self.b.ins().store(MemFlagsData::trusted(), held, ptr, 0);
        }
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(cold);
        let frame_addr = self.frame_addr();
        let from = self.b.ins().iadd_imm_s(frame_addr, i64::from(src));
        self.field_call(self.bound.field_store, addr, at, width, from);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(join);
        self.forget();
    }

    /// One [`crate::abi::FieldLoadFn`]/[`crate::abi::FieldStoreFn`] call, handed
    /// to the runtime whole. [`Lower::growable_op`]'s shape, with no safepoint
    /// discipline around it — neither helper can allocate — and two of its
    /// operands already linear addresses rather than immediates.
    fn field_call(&mut self, callee: FuncRef, addr: Value, at: u32, width: u32, into: Value) {
        let pc = self.b.ins().iconst(types::I32, self.pc as i64);
        let atv = self.b.ins().iconst(types::I32, i64::from(at));
        let widthv = self.b.ins().iconst(types::I32, i64::from(width));
        let call = self
            .b
            .ins()
            .call(callee, &[self.ctx, pc, addr, atv, widthv, into]);
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
        // This frame's unpaid work, published before leaving. `native::call`
        // charges `pending_work` "on every exit — a return, a raise and a stop
        // alike", so an exit that does not publish does not under-charge by a
        // little: the whole block's work is never charged at all, and ADR 0040's
        // `S + T` bound is then computed from a number that is short.
        //
        // [`Lower::allocate`] and [`Lower::growable_op`] publish *before* the
        // call instead, and clear the accumulator, because each of them is a
        // safepoint and the helper may charge. This one cannot do that: a field
        // helper is deliberately **not** a safepoint — neither
        // [`crate::abi::FieldLoadFn`] nor [`crate::abi::FieldStoreFn`] can
        // allocate — so publishing early would put a charge where there is no
        // safepoint. It publishes here instead, on the one path that leaves, and
        // does not clear: there is nothing after this for a cleared accumulator
        // to be right for.
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        // Not `leave`: what this returns is the helper's outcome and not one this
        // function chose, and every field that outcome needs the helper has written.
        self.b.ins().return_(&[outcome]);

        self.b.switch_to_block(on);
        self.forget();
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
            CmpOp::Order => unreachable!("a three-way order is lowered by its own `Inst::Cmp` arm"),
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

    /// A poll: test the stride, and only then hand the runtime the unpaid work.
    ///
    /// **The test is emitted; the safepoint is called.** Three instructions
    /// stand in front of the hand-over — a load of [`NativeCtx::poll_at`], a
    /// compare against the work accumulator and a branch — and the helper is
    /// entered only when the accumulator has reached the threshold the runtime
    /// published. ADR 0060 is why: on `examples/covefmt` this site was entered
    /// 1,845,706 times in the print phase to be told "not yet" almost every
    /// time, at ≈20 ns a call, and the stride it was testing is the *machine's*
    /// and was already being tested inside `Machine::safepoint`.
    ///
    /// What that costs the bound is nothing, because the threshold is
    /// `SAFEPOINT_STRIDE` minus the work the machine has already done and not
    /// charged: the poll lands where `encoded::dispatch`'s own
    /// `work() - charged_work >= SAFEPOINT_STRIDE` would have landed, so the
    /// interval is ADR 0040's `S + T` with `T` one turn of this loop. See
    /// [`NativeCtx::poll_at`] for who publishes it and when.
    ///
    /// Emitted on every backedge, which is the floor ADR 0055 sets
    /// ("Safepoints occur at least: on loop backedges; …") read as ADR 0060
    /// reads it — a poll at every backedge and a safepoint when the stride is
    /// reached. It is no longer the
    /// only one: [`Lower::allocate`] and [`Lower::growable_op`] are the ADR's
    /// "around allocation or runtime calls which may collect", and each of their
    /// helpers takes the same three steps in the same order before it does
    /// anything else. Host effects are still outside this slice. One of the
    /// ADR's five remains a real and stated gap: **this slice does not split a
    /// long straight-line block.** A loop-free function of a hundred
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
        // Unsigned, and that is exact rather than defensive: both numbers are
        // counts, the accumulator only ever grows between two safepoints, and
        // a published threshold of nought — a context that was given no budget
        // — makes this always true, which is the "poll at every backedge"
        // behaviour this site had before the threshold existed.
        let threshold =
            self.b
                .ins()
                .load(types::I64, MemFlagsData::trusted(), self.ctx, OFF_POLL_AT);
        let due = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, work, threshold);
        let poll = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(due, poll, &[], on, &[]);

        self.b.switch_to_block(poll);
        let work = self.b.use_var(self.work);
        let at = self.b.ins().iconst(types::I32, i64::from(pc));
        let call = self
            .b
            .ins()
            .call(self.bound.safepoint, &[self.ctx, at, work]);
        let carry_on = self.b.inst_results(call)[0];
        // Charged, so no longer pending — on both sides of the branch below.
        // The fall-through above keeps what it had, which is what makes the
        // next turn's test a test of the accumulated total.
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(self.work, zero);
        // The helper is allowed to have grown the stack and to have committed a
        // heap chunk, so neither pointer derived before the call is to be used
        // after it. This one line is the whole of the reallocation discipline;
        // see `crate::abi`.
        self.forget();

        let stop = self.b.create_block();
        self.b.ins().brif(carry_on, on, &[], stop, &[]);

        self.b.switch_to_block(stop);
        // Nothing is pending at a stop: the charge went to the helper before
        // it answered.
        let nothing = self.b.ins().iconst(types::I64, 0);
        self.store_ctx(OFF_PENDING_WORK, nothing);
        self.leave(Outcome::Stopped);

        // Reached both ways: from the branch that found the poll not due, and
        // from the helper that said carry on. Nothing derived on either side
        // of it is live here — which is the rule anyway, because a `Value` a
        // predecessor defined does not dominate this block.
        self.b.switch_to_block(on);
        self.forget();
    }

    /// `encoded.rs`'s `RETURN` arm, whole: `Function::returns`' width of words
    /// from `base + src` to the destination, and then leave.
    ///
    /// ADR 0057. The encoded arm copies into `caller_base + dst` because it has
    /// the caller's frame in front of it; a native callee is *given* that
    /// destination, as the two indices [`Entry`] carries, so it copies the same
    /// words to the same place. What used to be here — report the slot and let
    /// the caller copy — was an owned vector and a second copy on a path
    /// measured at 1.08x the VM.
    ///
    /// Three things about the shape of it:
    ///
    /// - **the address is formed from `NativeCtx::words` re-read here**, not from
    ///   the frame pointer this block may be holding. They are the same pointer,
    ///   but the destination is not in this frame and deriving it from something
    ///   named `frame` would read as though it were;
    /// - **a zero-width return emits nothing at all**, not even the address. The
    ///   lowering gives a width-0 destination the next free slot number, which
    ///   may be one the caller's frame does not have;
    /// - **loads and stores interleave.** The source is this frame and the
    ///   destination is the caller's, which is below it, so the two runs cannot
    ///   overlap — unlike [`Lower::copy`], where they can and where every word is
    ///   therefore loaded before any is stored.
    fn ret(&mut self, src: Slot) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let width = self.program.layout(self.function.returns).width();
        if width > 0 {
            let words =
                self.b
                    .ins()
                    .load(self.pointer, MemFlagsData::trusted(), self.ctx, OFF_WORDS);
            // `return_slot` is a `u32` and the upper half of the register it
            // arrived in is not the caller's to promise, so it is extended
            // rather than used as it lies.
            let slot = self.b.ins().uextend(types::I64, self.return_slot);
            let index = self.b.ins().iadd(self.return_base, slot);
            let bytes = self.b.ins().ishl_imm_u(index, 3);
            let into = self.b.ins().iadd(words, bytes);
            for word in 0..width {
                let value = self.load_slot(src + word);
                self.b
                    .ins()
                    .store(MemFlagsData::trusted(), value, into, (word * 8) as i32);
            }
        }
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
        let call = self.b.ins().call(
            self.bound.call,
            &[self.ctx, self.base, at, callee, args, into],
        );
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
