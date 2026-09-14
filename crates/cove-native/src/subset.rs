//! What both code generators compile, decided once.
//!
//! Two arms compile Cove IR in this crate — Cranelift under the `cranelift`
//! feature and a hand-written x86-64 template compiler under `template` — and
//! the
//! whole point of having two is to measure one against the other. A
//! measurement over two different subsets of the IR would not be that
//! measurement, so the subset is not written twice: [`supported`] is the one
//! predicate both arms ask, and `leaders` is the one block partition both
//! arms charge work over.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use cove_ir::{ArithOp, CmpOp, Compare, Function, Inst, Num, Program, Repr, Slot};

use crate::abi::Raise;

/// The widest run of words this slice moves in one instruction.
///
/// An [`Inst::Copy`], an [`Inst::LoadElem`]'s element and an [`Inst::Call`]'s
/// answer are each emitted as a run of loads and a run of stores — see each
/// arm's `copy` for why a copy is in that order — so the code they produce is
/// linear in the width and there is no memmove helper to fall back to yet. A
/// bound is therefore worth having, and it is deliberately generous: sixteen
/// words is a wider inline value than anything the corpus lowers, and a
/// `covefmt.Token` is three.
const MAX_RUN_WORDS: u32 = 16;

/// Whether a slot of this `Repr` is one this slice will touch.
///
/// The scalars, and [`Repr::Ref`] — see [`crate::abi`]'s "References are live
/// here, and the frame is why that is safe". A reference is admitted because
/// the frame is the canonical home of every value at every instruction
/// boundary, so a `Repr::Ref` slot is a root the existing walk already finds,
/// and because the covefmt slice takes a `String` and an `Array` as its
/// parameters: refusing a reference would refuse the measurement.
///
/// [`Repr::Addr`], [`Host`](Repr::Host), [`Task`](Repr::Task) and
/// [`Scope`](Repr::Scope) are excluded, and for a reason that has nothing to do
/// with the collector: they are not roots, but every operation that produces or
/// consumes one is a runtime call this slice does not lower, so a frame holding
/// one is a frame whose function will be refused anyway.
///
/// [`Repr::Float`] is admitted although no float *operation* is lowered. A
/// float slot that is only copied is a run of bits like any other, and
/// refusing the whole function because one of its frame slots is a `Float`
/// would refuse it for a reason that is not true.
fn is_lowered(repr: Repr) -> bool {
    match repr {
        Repr::Unit
        | Repr::Bool
        | Repr::Int
        | Repr::Float
        | Repr::Duration
        | Repr::Tag
        | Repr::Ref => true,
        Repr::Addr | Repr::Host | Repr::Task | Repr::Scope => false,
    }
}

/// The byte offset of a slot from the frame's first word, if it fits the
/// `i32` displacement both arms address a frame with.
///
/// A frame is bounded by `cove_ir::MAX_FRAME_WORDS`, which is far inside
/// this, so the `None` is unreachable in practice. It is checked rather than
/// asserted because "unreachable in practice" is a claim about today's
/// constant.
pub(crate) fn slot_offset(slot: Slot) -> Option<i32> {
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
/// [`Compare::Tag`] takes equality only, and for exactly the same reason: it
/// shares `encoded.rs`'s `EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true)` arm and
/// its `LT_TAG | LE_TAG | GT_TAG | GE_TAG => not_ordered!()` neighbour. It is
/// here because `token.kind != Kind.Punct` is what a formatter asks in every
/// loop it has — see [`cove_ir::Compare::Tag`]'s own note on what walking it
/// instead cost.
///
/// Everything else — [`Compare::Float`], [`Str`](Compare::Str),
/// [`Identity`](Compare::Identity) — is outside the slice. `Identity` would be
/// one integer comparison and is left out because nothing the raced slice does
/// asks it, which is the rule this predicate is widened by.
fn comparison_supported(on: Compare, op: CmpOp) -> bool {
    match on {
        Compare::Int => true,
        Compare::Bool | Compare::Tag => matches!(op, CmpOp::Eq | CmpOp::Ne),
        Compare::Float | Compare::Str | Compare::Identity => false,
    }
}

/// Why a function has no machine code, as one stable reason.
///
/// [ADR 0055] asks a native run to report "one stable refusal reason per refused
/// function", and *stable* is the load-bearing word: the reason is what a reader
/// sorts a table by and decides what to build next from, so it names a **family**
/// rather than an instruction. Two functions refused for `LoadField` and for
/// `AllocFixed` share [`Reason::Instruction`] and are told apart by the
/// instruction [`Refusal::at`] names; two refused because a slot holds a
/// `Repr::Host` share [`Reason::SlotRepr`] and there is no instruction to name.
///
/// The division that matters is between the last two. [`Reason::Instruction`] is
/// an operation nothing lowers — a family to write — and [`Reason::Operands`] is
/// an operation that *is* lowered, refused because a bound it names was
/// exceeded. Those point at different work, and a single "unsupported" would
/// have hidden the difference.
///
/// [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `lower::stub` left a stand-in where a body would be.
    ///
    /// There is nothing to compile, and compiling it would present a run as
    /// more native than it is.
    Stub,
    /// A frame slot holds a representation this tier does not keep in a slot.
    ///
    /// [`Repr::Addr`], [`Host`](Repr::Host), [`Task`](Repr::Task) and
    /// [`Scope`](Repr::Scope); see this module's `is_lowered`.
    SlotRepr(Repr),
    /// The body does not end in a terminator, so its last block falls off the
    /// end.
    NoTerminator,
    /// An instruction no arm emits code for.
    Instruction,
    /// An instruction both arms lower, whose operands are outside a bound one
    /// of them needs.
    ///
    /// A value wider than this module's `MAX_RUN_WORDS`, a slot past the end of
    /// the frame,
    /// a slot offset an `i32` displacement cannot name, a comparison that is a
    /// runtime error rather than an answer, or a jump table with more cases
    /// than an `i32` immediate can hold.
    Operands,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Stub => write!(f, "the body is a stub"),
            Reason::SlotRepr(repr) => write!(f, "a frame slot holds a `{repr:?}`"),
            Reason::NoTerminator => write!(f, "the body does not end in a terminator"),
            Reason::Instruction => write!(f, "an instruction is not lowered"),
            Reason::Operands => write!(f, "an operand is outside a bound"),
        }
    }
}

/// One refused function's reason, and where the reason was found.
///
/// `at` is the **first** unsupported instruction, which is the other half of
/// what ADR 0055's report asks for. First rather than every one, because a
/// function is refused whole: the second refusal in a body is not work anybody
/// can do next, and a list of them would sort a long function above a hot one.
/// It is `None` when the refusal is not about an instruction at all — a stub, a
/// slot's representation, a missing terminator — because there is no pc to name
/// and a zero would read as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Which family of refusal this is.
    pub reason: Reason,
    /// The pc of the first instruction that could not be lowered.
    pub at: Option<u32>,
}

/// Whether every part of `function` is inside this slice.
///
/// Called before lowering begins, which is what makes lowering infallible.
/// The instruction match here and each arm's `inst` are two halves of one
/// decision and have to agree: a form admitted here and not lowered there is
/// a panic, which is why that arm is `unreachable!` and says so.
///
/// It is [`refusal`] answering `None`, and it stays as the predicate both arms
/// ask because a `bool` is what a code generator needs: *why* a function was
/// refused is a question for the report and not for the emitter.
pub fn supported(program: &Program, function: &Function) -> bool {
    refusal(program, function).is_none()
}

/// Why `function` is outside this slice, or `None` if it is inside it.
///
/// The order the checks are made in is the order the reasons are reported in,
/// and it is deliberate: a stub is refused before its slots are read and its
/// slots before its instructions, so the reason a reader is given is the
/// *coarsest* true one. A stub whose slots also hold a `Repr::Host` is reported
/// as a stub, because writing the missing lowering is not what would make it
/// compile.
pub fn refusal(program: &Program, function: &Function) -> Option<Refusal> {
    let of = |reason: Reason| Some(Refusal { reason, at: None });
    // A stub is a stand-in for a body the lowering did not lower, so there is
    // nothing to compile: `lower::stub` leaves a `return` of a cleared slot.
    // Compiling it would answer the same thing the encoded tier answers, and
    // it would also present a run as more native than it is.
    if function.stub {
        return of(Reason::Stub);
    }
    if let Some(repr) = function.reprs.iter().copied().find(|r| !is_lowered(*r)) {
        return of(Reason::SlotRepr(repr));
    }
    // The verifier requires it, and the lowering depends on it: a function
    // whose last instruction is not a terminator would fall off the end of
    // its last basic block, and there is nowhere for it to fall to.
    if !matches!(
        function.code.last(),
        Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Trap { .. } | Inst::Switch { .. })
    ) {
        return of(Reason::NoTerminator);
    }
    function.code.iter().enumerate().find_map(|(pc, inst)| {
        inst_refused(program, function, inst).map(|reason| Refusal {
            reason,
            at: Some(pc as u32),
        })
    })
}

/// Why one instruction is outside the slice, or `None` if it is inside it.
///
/// Every arm answers [`Reason::Operands`] and the fallback answers
/// [`Reason::Instruction`], which is the whole of the division: an arm exists
/// because both code generators emit that form, so reaching one and failing it
/// is a bound and never a missing family.
fn inst_refused(program: &Program, function: &Function, inst: &Inst) -> Option<Reason> {
    let slots = function.reprs.len();
    let end = function.code.len() as u32;
    let slot = |at: Slot| (at as usize) < slots && slot_offset(at).is_some();
    let run = |at: Slot, width: u32| {
        at.checked_add(width)
            .is_some_and(|last| (last as usize) <= slots)
            && slot_offset(at.saturating_add(width)).is_some()
    };
    // `true` is "this instruction is inside the slice", so that each arm below
    // reads the way it read while it was a predicate.
    let inside = match inst {
        Inst::Bool { dst, .. } | Inst::Int { dst, .. } => slot(*dst),
        // A case index is one word and the word is a compile-time constant, so
        // this is `encoded.rs`'s `FUNC_REF | CONST_TAG` arm: the same store
        // `CONST_INT` makes, of a number the layout already fixed. The layout
        // and the case are bounded by `cove_ir::verify` before this is reached.
        Inst::Tag { dst, .. } => slot(*dst),
        Inst::Copy { dst, src, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*dst, layout.width())
                && run(*src, layout.width())
        }
        Inst::Not { dst, a } => slot(*dst) && slot(*a),
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
        // Every target and the default, because the machine does not take the
        // lowering's word for the index it reads out of a slot: `encoded.rs`
        // takes `targets.get(index).unwrap_or(&default)`, so the default is as
        // reachable as any target and is bounded with them.
        Inst::Switch { on, table } => {
            let table = program.table(*table);
            // The case count has to fit an `i32`, and that is a bound the
            // *template* arm needs rather than a bound on the language: its
            // compare chain tests `cmp r64, imm32`, whose immediate is
            // sign-extended, so a case index above `i32::MAX` would be compared
            // against a negative number. No enum has two billion cases and
            // `cove_ir::lower` could not build one, so this refuses nothing real —
            // but an arm that was silently wrong above a threshold is worse than
            // one that refuses at it, and the two arms share this predicate so
            // neither may admit what the other cannot lower.
            i32::try_from(table.targets.len()).is_ok()
                && slot(*on)
                && table.default < end
                && table.targets.iter().all(|target| *target < end)
        }
        Inst::Len { dst, obj } => slot(*dst) && slot(*obj),
        // The stride is the element layout's width and the destination is that
        // many words wide, so an `Array<Token>` writes three slots and an
        // `Array<Int>` one. The element's own words have to be ones this slice
        // can hold in a frame, which is `Inst::Copy`'s rule for the same
        // reason: what arrives is a run of words and they land in slots.
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*dst, layout.width())
                && slot(*obj)
                && slot(*index)
        }
        Inst::ByteAt { dst, obj, at } => slot(*dst) && slot(*obj) && slot(*at),
        // A call is admitted whatever the callee is: it is handed to
        // `NativeHelpers::call`, which opens the frame with the runtime's own
        // `open_frame` and runs the callee on whichever tier it is on. So the
        // callee's *body* is not this function's business and is not examined —
        // only that the callee exists, that the arguments name slots this frame
        // has, and that the answer fits where it is going.
        Inst::Call { dst, callee, args } => {
            let answer = program.layout(program.function(*callee).returns).width();
            callee.index() < program.functions.len()
                && answer <= MAX_RUN_WORDS
                && run(*dst, answer)
                && program.arg_list(*args).iter().all(|arg| {
                    let width = program.layout(arg.layout).width();
                    width <= MAX_RUN_WORDS && run(arg.slot, width)
                })
        }
        Inst::Return { src } => run(*src, program.layout(function.returns).width()),
        Inst::Trap { .. } => true,
        _ => return Some(Reason::Instruction),
    };
    (!inside).then_some(Reason::Operands)
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
pub(crate) fn leaders(program: &Program, function: &Function) -> Vec<Option<u32>> {
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
            // Every case and the default, and then the fall-through — which a
            // `switch` has none of, but marking `pc + 1` is what makes the
            // instruction after a terminator a block whether anything jumps to
            // it or not, and a `switch` is a terminator.
            Inst::Switch { table, .. } => {
                let table = program.table(*table);
                for target in table.targets.iter().chain(std::iter::once(&table.default)) {
                    mark(&mut leader, *target as usize);
                }
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

/// Which overflow `int_arith` would name for `op` writing to `dst`.
///
/// `int_arith`'s `named` closure answers "duration arithmetic" instead of the
/// operation's own name when the destination is a [`Repr::Duration`] — the
/// question `encoded.rs` asks as `machine.repr(id, a!()) ==
/// Some(Repr::Duration)`. Here the destination's `Repr` is a static fact, so
/// the question is asked once, at compile time, and the answer is a constant in
/// the generated code.
///
/// It is asked only of addition, subtraction and multiplication, because those
/// are the three arms of `int_arith` that call `named`. Division and remainder
/// name themselves whatever the destination is.
///
/// Shared by both arms for the same reason [`supported`] is: this is a rule of
/// the *language*, and two code generators disagreeing about it would be two
/// different languages.
pub(crate) fn overflow_of(function: &Function, op: ArithOp, dst: Slot) -> Raise {
    let duration = function.reprs.get(dst as usize) == Some(&Repr::Duration);
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

/// Which "divided by zero" a division-shaped operation names.
///
/// `int_arith`'s `Div` and `Rem` arms test the divisor before they divide, and
/// they name the operation they are: `divided_by_zero("division")` and
/// `divided_by_zero("remainder")`.
pub(crate) fn by_zero_of(op: ArithOp) -> Raise {
    match op {
        ArithOp::Rem => Raise::RemainderByZero,
        _ => Raise::DividedByZero,
    }
}
