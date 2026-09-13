//! One [`Function`] to one run of machine code, through Cranelift.
//!
//! The shape of this module is the shape of [ADR 0055]'s decision: a function
//! is the unit of compilation, a basic block is the unit of work accounting
//! and safepoint placement, and a function the lowering does not fully
//! understand is refused whole rather than split.
//!
//! # Refusal is the interesting half
//!
//! [`supported`] walks the whole function before a single Cranelift
//! instruction is emitted and answers whether every one of its instructions,
//! slots and layouts is inside this slice. Only then does lowering begin, and
//! lowering is therefore infallible — which is what lets [`Jit::compile`]
//! answer `None` without leaving a half-built function behind in the module.
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

use cove_ir::{ArithOp, CmpOp, Compare, Function, FunctionId, Inst, Num, Program, Repr, Slot};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Signature,
};
use cranelift_codegen::ir::{Type, Value};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module, ModuleError};

use crate::abi::{Entry, NativeCtx, NativeHelpers, Outcome, Raise};

/// The name the safepoint helper is imported under.
///
/// An internal detail of the binding — [`JITBuilder::symbol`] is given this
/// name and the helper's address, and every compiled function calls it
/// through an ordinary relocation. It is not part of the ABI: nothing outside
/// this crate can see it, and a second code generator is free to bind the
/// same [`NativeHelpers`] differently.
const SAFEPOINT: &str = "cove_native_safepoint";

/// The widest [`Inst::Copy`] this slice lowers, in words.
///
/// A copy is emitted as a run of loads and then a run of stores — see
/// [`Lower::copy`] for why it is in that order — so the code it produces is
/// linear in the width and there is no memmove helper to fall back to yet. A
/// bound is therefore worth having, and it is deliberately generous: sixteen
/// words is a wider inline value than anything the corpus lowers.
const MAX_COPY_WORDS: u32 = 16;

// The `NativeCtx` field offsets, read from the declaration rather than
// written out. `offset_of!` is a const expression, so the numbers compiled
// into the machine code and the numbers Rust uses to read the struct are the
// same numbers by construction; reordering the fields cannot desynchronise
// them.
const OFF_WORDS: i32 = offset_of!(NativeCtx, words) as i32;
const OFF_PENDING_WORK: i32 = offset_of!(NativeCtx, pending_work) as i32;
const OFF_RETURN_SLOT: i32 = offset_of!(NativeCtx, return_slot) as i32;
const OFF_RAISE_CODE: i32 = offset_of!(NativeCtx, raise_code) as i32;
const OFF_RAISE_DETAIL: i32 = offset_of!(NativeCtx, raise_detail) as i32;

/// Native execution is not available here.
///
/// ADR 0055's "Executable memory is optional, not assumed": a target that
/// prohibits or cannot provide executable memory, or that this lowering has
/// not been written for, produces a capability diagnostic. It does not
/// produce an attempted fallback to something else — the caller's fallback is
/// the encoded VM, which is a complete execution path and not a fallback at
/// all.
#[derive(Debug)]
pub struct Unavailable(String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native execution is unavailable: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

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
        Ok(Jit {
            ctx: module.make_context(),
            module,
            builder: FunctionBuilderContext::new(),
            safepoint,
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
            Lower::new(&mut builder, program, function, safepoint).run();
            builder.seal_all_blocks();
            builder.finalize(self.module.target_config());
        }
        self.module.define_function(func, &mut self.ctx).ok()?;
        self.finalized = false;
        Some(Compiled {
            id: func,
            function: id,
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

/// Whether a slot of this `Repr` is one this slice will touch.
///
/// The scalars, and deliberately not [`Repr::Ref`] — see [`crate::abi`]'s
/// "There are no references here yet". [`Repr::Addr`], [`Host`](Repr::Host),
/// [`Task`](Repr::Task) and [`Scope`](Repr::Scope) are excluded for a
/// different reason: they are not roots, so they are not a collector problem,
/// but every operation that produces or consumes one is a runtime call this
/// slice does not lower, so a frame holding one is a frame whose function
/// will be refused anyway.
///
/// [`Repr::Float`] is admitted although no float *operation* is lowered. A
/// float slot that is only copied is a run of bits like any other, and
/// refusing the whole function because one of its frame slots is a `Float`
/// would refuse it for a reason that is not true.
fn is_scalar(repr: Repr) -> bool {
    match repr {
        Repr::Unit | Repr::Bool | Repr::Int | Repr::Float | Repr::Duration | Repr::Tag => true,
        Repr::Ref | Repr::Addr | Repr::Host | Repr::Task | Repr::Scope => false,
    }
}

/// The byte offset of a slot from the frame's first word, if it fits the
/// `i32` displacement a Cranelift load carries.
///
/// A frame is bounded by `cove_ir::MAX_FRAME_WORDS`, which is far inside
/// this, so the `None` is unreachable in practice. It is checked rather than
/// asserted because "unreachable in practice" is a claim about today's
/// constant.
fn slot_offset(slot: Slot) -> Option<i32> {
    i32::try_from(i64::from(slot) * 8).ok()
}

/// Whether a comparison is one this slice lowers.
///
/// [`Compare::Int`] takes all six operators, as
/// `encoded.rs`'s `cmp_int!` does. [`Compare::Bool`] takes equality only,
/// which is the same division `encoded.rs` makes at its `EQ_BOOL`/`NE_BOOL`
/// arms against the `LT_BOOL | LE_BOOL | GT_BOOL | GE_BOOL => not_ordered!()`
/// arm beside them — an ordered comparison of `Bool` is a runtime error, and
/// emitting one would be lowering a refusal. Refusing the function instead
/// leaves it to the tier that already has the message.
///
/// Everything else — [`Compare::Float`], [`Str`](Compare::Str),
/// [`Identity`](Compare::Identity), [`Tag`](Compare::Tag) — is outside the
/// slice. `Identity` and `Tag` would each be one integer comparison, but
/// `Identity` reads a [`Repr::Ref`] word and `Tag` a [`Repr::Tag`] one, and
/// this slice's claim that it never touches a reference is worth more than
/// two instructions.
fn comparison_supported(on: Compare, op: CmpOp) -> bool {
    match on {
        Compare::Int => true,
        Compare::Bool => matches!(op, CmpOp::Eq | CmpOp::Ne),
        Compare::Float | Compare::Str | Compare::Identity | Compare::Tag => false,
    }
}

/// Whether every part of `function` is inside this slice.
///
/// Called before lowering begins, which is what makes lowering infallible.
/// The instruction match here and [`Lower::inst`]'s are two halves of one
/// decision and have to agree: a form admitted here and not lowered there is
/// a panic, which is why that arm is `unreachable!` and says so.
fn supported(program: &Program, function: &Function) -> bool {
    if !function.reprs.iter().copied().all(is_scalar) {
        return false;
    }
    // The verifier requires it, and the lowering depends on it: a function
    // whose last instruction is not a terminator would fall off the end of
    // its last basic block, and there is nowhere for it to fall to.
    if !matches!(
        function.code.last(),
        Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Trap { .. })
    ) {
        return false;
    }
    function
        .code
        .iter()
        .all(|inst| inst_supported(program, function, inst))
}

fn inst_supported(program: &Program, function: &Function, inst: &Inst) -> bool {
    let slots = function.reprs.len();
    let end = function.code.len() as u32;
    let slot = |at: Slot| (at as usize) < slots && slot_offset(at).is_some();
    let run = |at: Slot, width: u32| {
        at.checked_add(width)
            .is_some_and(|last| (last as usize) <= slots)
            && slot_offset(at.saturating_add(width)).is_some()
    };
    match inst {
        Inst::Bool { dst, .. } | Inst::Int { dst, .. } => slot(*dst),
        Inst::Copy { dst, src, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_COPY_WORDS
                && layout.words.iter().copied().all(is_scalar)
                && run(*dst, layout.width())
                && run(*src, layout.width())
        }
        Inst::Arith {
            num: Num::Int,
            dst,
            a,
            b,
            ..
        } => slot(*dst) && slot(*a) && slot(*b),
        Inst::ArithImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::Cmp { on, op, dst, a, b } => {
            comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b)
        }
        Inst::CmpImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::CmpBranch {
            on,
            op,
            dst,
            a,
            b,
            target,
        } => comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b) && *target < end,
        Inst::CmpImmBranch { dst, a, target, .. } => slot(*dst) && slot(*a) && *target < end,
        Inst::Jump { to } => *to < end,
        Inst::BranchFalse { cond, to } => slot(*cond) && *to < end,
        Inst::Return { src } => run(*src, program.layout(function.returns).width()),
        Inst::Trap { .. } => true,
        _ => false,
    }
}

/// Where a basic block begins, and how many instructions it holds.
///
/// A block is ADR 0055's unit of work accounting and safepoint placement, so
/// this is not only a code-generation convenience: the static instruction
/// count of a block is the charge added to the work accumulator when the
/// block is entered.
///
/// A leader is the first instruction, any branch or jump target, and the
/// instruction after any terminator. That last clause is what makes a
/// conditional branch's fall-through a block of its own, which Cranelift
/// needs because its `brif` names both successors explicitly.
fn leaders(function: &Function) -> Vec<Option<u32>> {
    let end = function.code.len();
    let mut leader = vec![false; end];
    if end > 0 {
        leader[0] = true;
    }
    fn mark(leader: &mut [bool], at: usize) {
        if at < leader.len() {
            leader[at] = true;
        }
    }
    for (pc, inst) in function.code.iter().enumerate() {
        match inst {
            Inst::Jump { to } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::BranchFalse { to, .. } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                mark(&mut leader, *target as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::Return { .. } | Inst::Trap { .. } => mark(&mut leader, pc + 1),
            _ => {}
        }
    }
    // Rewritten as "how long is the block starting here", counting forward to
    // the next leader, so the work charge is one lookup at block entry.
    let mut lengths = vec![None; end];
    let mut at = end;
    for pc in (0..end).rev() {
        if leader[pc] {
            lengths[pc] = Some((at - pc) as u32);
            at = pc;
        }
    }
    lengths
}

/// One function's lowering.
struct Lower<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    program: &'a Program,
    function: &'a Function,
    safepoint: FuncRef,
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
    ) -> Self {
        let pointer = b.func.signature.params[0].value_type;
        let blocks: Vec<Option<(Block, u32)>> = leaders(function)
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
            pointer,
            ctx,
            base,
            work,
            frame: None,
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
                    self.frame = None;
                    self.charge(length);
                }
            }
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
            Inst::Copy { dst, src, layout } => {
                self.copy(*dst, *src, self.program.layout(*layout).width());
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
        let overflow = self.overflow_of(op, dst);
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
                let by_zero = match op {
                    ArithOp::Rem => Raise::RemainderByZero,
                    _ => Raise::DividedByZero,
                };
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

    /// Which overflow `int_arith` would name.
    ///
    /// `int_arith`'s `named` closure answers "duration arithmetic" instead of
    /// the operation's own name when the destination is a
    /// [`Repr::Duration`] — the question `encoded.rs` asks as
    /// `machine.repr(id, a!()) == Some(Repr::Duration)`. Here the
    /// destination's `Repr` is a static fact, so the question is asked once,
    /// at compile time, and the answer is a constant in the code.
    ///
    /// It is asked only of addition, subtraction and multiplication, because
    /// those are the three arms of `int_arith` that call `named`. Division and
    /// remainder name themselves whatever the destination is.
    fn overflow_of(&self, op: ArithOp, dst: Slot) -> Raise {
        let duration = self.function.reprs.get(dst as usize) == Some(&Repr::Duration);
        match op {
            ArithOp::Add if duration => Raise::DurationOverflowed,
            ArithOp::Sub if duration => Raise::DurationOverflowed,
            ArithOp::Mul if duration => Raise::DurationOverflowed,
            ArithOp::Add => Raise::AddOverflowed,
            ArithOp::Sub => Raise::SubOverflowed,
            ArithOp::Mul => Raise::MulOverflowed,
            ArithOp::Div => Raise::DivOverflowed,
            ArithOp::Rem => Raise::RemOverflowed,
        }
    }

    /// `encoded.rs`'s `cmp_int!`: `compare(op, x.cmp(&y))` on the words read
    /// as `i64`, which is a signed comparison.
    ///
    /// The same six conditions serve [`Compare::Bool`], which
    /// [`comparison_supported`] has already narrowed to equality — and
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
            self.frame = None;
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
        // The helper is allowed to have grown the stack, so the frame pointer
        // derived before the call is not to be used after it. This one line
        // is the whole of the reallocation discipline; see `crate::abi`.
        self.frame = None;

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
        self.frame = None;
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
    fn raise(&mut self, code: Raise, detail: u32) {
        let work = self.b.use_var(self.work);
        self.store_ctx(OFF_PENDING_WORK, work);
        let named = self.b.ins().iconst(types::I32, i64::from(code.abi()));
        self.store_ctx(OFF_RAISE_CODE, named);
        let carried = self.b.ins().iconst(types::I32, i64::from(detail));
        self.store_ctx(OFF_RAISE_DETAIL, carried);
        self.leave(Outcome::Raised);
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
