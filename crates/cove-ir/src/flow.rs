//! What an instruction writes and where it can go: two questions every pass
//! that reasons about a block asks, answered once.
//!
//! Each of `lower::inline`'s `written`, `lower::frees`' `writes` and
//! `verify`'s slot facts enumerates the words an instruction writes, and
//! `lower::branches` and `cove-native`'s `subset::leaders` each enumerate the
//! counters one names. They answer slightly different questions — `frees`
//! sizes a closure call's answer by the widest one in the program, and the
//! inliner never sees a call — so they are left as they are.
//! [ADR 0062](../../../docs/adr/0062-an-append-is-ensure-store-commit.md)'s
//! reservation rule is the first reader that needs *both* questions answered
//! for every instruction, exhaustively, and the one place a new instruction
//! should be taught them is here rather than in a sixth copy.

use crate::inst::{Inst, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::{Function, FunctionId, Program};

impl Inst {
    /// Calls `f` with the base and width of every run of frame words this
    /// instruction writes.
    ///
    /// Widths come from the layouts the instruction names, and a layout, a
    /// callee or a host operation the program does not have is read as one
    /// word rather than a panic: the verifier calls this over code it has not
    /// yet bounded, and reports the missing id itself.
    ///
    /// A `var` argument's words are not here. What a callee stores through an
    /// address lands in this frame, but it is the *call* that does it, and a
    /// reader that cares — the reservation rule does — treats every call as
    /// ending what it knows.
    pub fn writes(&self, program: &Program, f: &mut dyn FnMut(Slot, u32)) {
        let width = |id: LayoutId| {
            program
                .layouts
                .get(id.index())
                .map_or(1, |layout| layout.width())
        };
        match *self {
            Inst::Unit { dst }
            | Inst::Bool { dst, .. }
            | Inst::Int { dst, .. }
            | Inst::Tag { dst, .. }
            | Inst::FuncRef { dst, .. }
            | Inst::Float { dst, .. }
            | Inst::Str { dst, .. }
            | Inst::Neg { dst, .. }
            | Inst::Not { dst, .. }
            | Inst::Arith { dst, .. }
            | Inst::Cmp { dst, .. }
            | Inst::ArithImm { dst, .. }
            | Inst::CmpImm { dst, .. }
            | Inst::CmpBranch { dst, .. }
            | Inst::CmpImmBranch { dst, .. }
            | Inst::Convert { dst, .. }
            | Inst::FloatAbs { dst, .. }
            | Inst::FloatMinMax { dst, .. }
            | Inst::FloatRound { dst, .. }
            | Inst::FloatSqrt { dst, .. }
            | Inst::RunLoad { dst, .. }
            | Inst::GrowableAlloc { dst, .. }
            | Inst::RunFinish { dst, .. }
            | Inst::Len { dst, .. }
            | Inst::LayoutOf { dst, .. }
            | Inst::Alloc { dst, .. }
            | Inst::Box { dst, .. }
            | Inst::AddrOfSlot { dst, .. }
            | Inst::AddrOfField { dst, .. }
            | Inst::AddrOfElem { dst, .. }
            | Inst::AddrOfPart { dst, .. }
            | Inst::ScopeEnter { dst, .. }
            | Inst::Spawn { dst, .. } => f(dst, 1),
            Inst::Clear { slot, layout } => f(slot, width(layout)),
            Inst::Copy { dst, layout, .. }
            | Inst::Load { dst, layout, .. }
            | Inst::LoadField { dst, layout, .. }
            | Inst::LoadElem { dst, layout, .. }
            | Inst::Unbox { dst, layout, .. } => f(dst, width(layout)),
            Inst::Await { dst, answer, .. } | Inst::Settled { dst, answer, .. } => {
                f(dst, width(answer))
            }
            Inst::ScopeLeave {
                failed,
                error,
                layout,
                ..
            } => {
                f(failed, 1);
                f(error, width(layout));
            }
            Inst::Call { dst, callee, .. } => {
                let answer = program
                    .functions
                    .get(callee.index())
                    .map_or(1, |target| width(target.returns));
                f(dst, answer);
            }
            Inst::CallClosure { dst, result, .. } => f(dst, width(result)),
            Inst::CallHost { dst, op, .. } | Inst::CallResource { dst, op, .. } => {
                let answer = program
                    .host_ops
                    .get(op.index())
                    .map_or(1, |op| width(op.result));
                f(dst, answer);
            }
            Inst::IntrinsicCall { dst, site, .. } => {
                let answer = program
                    .intrinsic_sites
                    .get(site.index())
                    .map_or(1, |builtin| width(builtin.result));
                f(dst, answer);
            }
            // The two rows whose first entry is written rather than read: a
            // run slice's `dst`, which receives a fresh run's address, and a
            // run find's, which receives an offset. One word either way.
            Inst::RunSlice { args, .. } | Inst::RunFind { args, .. } => {
                if let Some(dst) = program.args.get(args.index()).and_then(|row| row.first()) {
                    f(dst.slot, 1);
                }
            }
            // A store writes an object's or an address's words, the growable
            // family an owner's and its store's, a scope instruction the
            // scheduler's table, a lock the cell's own word, and `AssertFailed`
            // the run's report: none of them a word of this frame.
            Inst::Store { .. }
            | Inst::StoreField { .. }
            | Inst::StoreElem { .. }
            | Inst::RunCopy { .. }
            | Inst::RunStore { .. }
            | Inst::GrowableEnsure { .. }
            | Inst::GrowableCommit { .. }
            | Inst::GrowableTruncate { .. }
            | Inst::ScopeCancel { .. }
            | Inst::Cancel { .. }
            | Inst::SharedLock { .. }
            | Inst::SharedUnlock { .. }
            | Inst::AssertFailed { .. }
            | Inst::Jump { .. }
            | Inst::BranchFalse { .. }
            | Inst::Switch { .. }
            | Inst::Return { .. }
            | Inst::Trap { .. } => {}
        }
    }

    /// The function this instruction names, where it names one.
    ///
    /// Two do. [`Inst::Call`] names the body it enters, and [`Inst::FuncRef`]
    /// names the one it writes into a word — a callback handed to `map`, a
    /// conformance a `dyn` dispatch will pick, the callee field of a closure
    /// object. The second is why this is not "the callee of a call": a
    /// function reached only as a *value* is named by no call site, and a
    /// reader that asked only about calls would conclude nothing reaches it.
    /// [`crate::lower_roots`] closes its slice against the lowering rather
    /// than against the checker's call graph for exactly that reason, and
    /// `lower::sweep` asks the same question again of the finished program.
    ///
    /// The match is exhaustive and has no wildcard arm, which is the whole
    /// point of it being here: a new instruction that carries a
    /// [`FunctionId`] does not compile until it is listed, and a reader that
    /// greps for `Inst::Call` gets no such warning.
    ///
    /// The three other places a [`FunctionId`] is written down are not
    /// instructions and are not here: `Shape::Closure`'s `function` in
    /// [`Program::layouts`], `Inlined::callee` in a function's expansion
    /// record, and the values of [`Program::by_name`].
    pub fn callee(&self) -> Option<FunctionId> {
        match *self {
            Inst::Call { callee, .. } | Inst::FuncRef { callee, .. } => Some(callee),
            Inst::Unit { .. }
            | Inst::Bool { .. }
            | Inst::Int { .. }
            | Inst::Tag { .. }
            | Inst::Float { .. }
            | Inst::Str { .. }
            | Inst::Copy { .. }
            | Inst::Clear { .. }
            | Inst::Neg { .. }
            | Inst::Not { .. }
            | Inst::Arith { .. }
            | Inst::Cmp { .. }
            | Inst::ArithImm { .. }
            | Inst::CmpImm { .. }
            | Inst::Convert { .. }
            | Inst::FloatAbs { .. }
            | Inst::FloatMinMax { .. }
            | Inst::FloatRound { .. }
            | Inst::FloatSqrt { .. }
            | Inst::Jump { .. }
            | Inst::BranchFalse { .. }
            | Inst::CmpBranch { .. }
            | Inst::CmpImmBranch { .. }
            | Inst::Switch { .. }
            | Inst::Return { .. }
            | Inst::CallClosure { .. }
            | Inst::CallHost { .. }
            | Inst::CallResource { .. }
            | Inst::IntrinsicCall { .. }
            | Inst::Alloc { .. }
            | Inst::LoadField { .. }
            | Inst::StoreField { .. }
            | Inst::LoadElem { .. }
            | Inst::StoreElem { .. }
            | Inst::RunLoad { .. }
            | Inst::RunCopy { .. }
            | Inst::RunSlice { .. }
            | Inst::RunFind { .. }
            | Inst::RunStore { .. }
            | Inst::RunFinish { .. }
            | Inst::GrowableAlloc { .. }
            | Inst::GrowableEnsure { .. }
            | Inst::GrowableCommit { .. }
            | Inst::GrowableTruncate { .. }
            | Inst::Len { .. }
            | Inst::LayoutOf { .. }
            | Inst::AddrOfSlot { .. }
            | Inst::AddrOfField { .. }
            | Inst::AddrOfElem { .. }
            | Inst::AddrOfPart { .. }
            | Inst::Load { .. }
            | Inst::Store { .. }
            | Inst::Box { .. }
            | Inst::Unbox { .. }
            | Inst::ScopeEnter { .. }
            | Inst::ScopeLeave { .. }
            | Inst::ScopeCancel { .. }
            | Inst::Spawn { .. }
            | Inst::Await { .. }
            | Inst::Cancel { .. }
            | Inst::Settled { .. }
            | Inst::SharedLock { .. }
            | Inst::SharedUnlock { .. }
            | Inst::Trap { .. }
            | Inst::AssertFailed { .. } => None,
        }
    }

    /// Calls `f` with every counter this instruction may transfer control to,
    /// other than the next one.
    ///
    /// A switch's cases and its default, read through the table it names; a
    /// table the program does not have names nothing here.
    pub fn targets(&self, program: &Program, f: &mut dyn FnMut(Pc)) {
        match *self {
            Inst::Jump { to } | Inst::BranchFalse { to, .. } => f(to),
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => f(target),
            Inst::Switch { table, .. } => {
                if let Some(table) = program.tables.get(table.index()) {
                    for target in &table.targets {
                        f(*target);
                    }
                    f(table.default);
                }
            }
            _ => {}
        }
    }

    /// Whether control never continues to the next instruction on its own.
    pub fn ends_a_block(&self) -> bool {
        matches!(
            self,
            Inst::Jump { .. }
                | Inst::BranchFalse { .. }
                | Inst::CmpBranch { .. }
                | Inst::CmpImmBranch { .. }
                | Inst::Switch { .. }
                | Inst::Return { .. }
                | Inst::Trap { .. }
        )
    }
}

/// Which of `function`'s instructions begin a basic block: the first, every
/// target, and every instruction after one that branches or ends the
/// function.
///
/// `cove-native`'s `subset::leaders` is the same rule, with the lengths it
/// charges work by. A target past the end of the code is ignored here, as it
/// is there: the verifier refuses it on its own.
pub fn leaders(program: &Program, function: &Function) -> Vec<bool> {
    leaders_in(program, &function.code)
}

/// [`leaders`] over a run of instructions that is not a [`Function`]'s own:
/// what `bytecode::verify` holds, having decoded it out of bytes.
pub fn leaders_in(program: &Program, code: &[Inst]) -> Vec<bool> {
    let end = code.len();
    let mut leader = vec![false; end];
    if let Some(first) = leader.first_mut() {
        *first = true;
    }
    for (pc, inst) in code.iter().enumerate() {
        inst.targets(program, &mut |to| {
            if let Some(held) = leader.get_mut(to as usize) {
                *held = true;
            }
        });
        if inst.ends_a_block() {
            if let Some(next) = leader.get_mut(pc + 1) {
                *next = true;
            }
        }
    }
    leader
}
