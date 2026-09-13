//! What the scalar and control-flow slice of the native tier actually does,
//! run.
//!
//! # Why the expectations are literals
//!
//! These tests cannot compare against `Machine`, because `cove-native` does
//! not depend on `cove-runtime` and must not — see that crate's documentation
//! for the inversion. So the encoded tier's behaviour is written out here as
//! literal expectations, and **every one of them cites the arm of
//! `crates/cove-runtime/src/vm/exec/encoded.rs` it mirrors, by file and
//! line**, so that a reader can check the mirror rather than trust it.
//!
//! The line numbers are as of the commit that added this file. They are a
//! pointer to an arm, not a contract; the arm's *name* is given beside every
//! one of them because a name survives an edit above it.
//!
//! A differential test against the real VM is the right test and it is the
//! next slice's, once the tier is reachable from a `Machine`. Until then this
//! is the strongest thing available, and it is stronger than nothing by
//! exactly the amount of trouble a wrong `+` would cause later.

#![cfg(feature = "native")]

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use cove_diag::{FileId, Span};
use cove_ir::{
    ArithOp, CmpOp, Compare, Function, FunctionId, Inst, Layout, LayoutId, Num, Program, RefMap,
    Repr, StrId,
};
use cove_native::{Compiled, Jit, NativeCtx, NativeHelpers, Outcome, Raise};

// --- the safepoint helper -----------------------------------------------------

thread_local! {
    /// Every safepoint this thread's compiled code has taken, as
    /// `(pc, work_since_last)`.
    ///
    /// A thread-local rather than a static, because `cargo test` runs these
    /// in parallel and each test body is one thread. The helper is a plain
    /// `extern "C"` function and therefore cannot close over a recorder, so
    /// the recorder has to be reachable from a bare function — which is what
    /// a thread-local is for.
    static POLLS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    /// How many safepoints to allow before answering "stop".
    static POLLS_ALLOWED: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// The runtime's half of the boundary, as a test double.
///
/// A real one is where [ADR 0040]'s three-step order lives — cancellation and
/// task-local stops, then fuel and deadline accounting, then the collector
/// rendezvous. This one records and optionally stops, which is the whole of
/// what compiled code can observe about it.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
unsafe extern "C" fn safepoint(_ctx: *mut NativeCtx, pc: u32, work: u64) -> bool {
    POLLS.with(|polls| polls.borrow_mut().push((pc, work)));
    let taken = POLLS.with(|polls| polls.borrow().len());
    taken < POLLS_ALLOWED.with(Cell::get)
}

fn helpers() -> NativeHelpers {
    NativeHelpers { safepoint }
}

fn polls() -> Vec<(u32, u64)> {
    POLLS.with(|polls| polls.borrow().clone())
}

fn forget_polls() {
    POLLS.with(|polls| polls.borrow_mut().clear());
    POLLS_ALLOWED.with(|allowed| allowed.set(usize::MAX));
}

// --- building a program by hand ----------------------------------------------

// `LayoutId(0)` is `LayoutId::FREE` and names no value, so the table below
// starts with it and nothing here uses it.
const INT: LayoutId = LayoutId(1);
const BOOL: LayoutId = LayoutId(2);
const UNIT: LayoutId = LayoutId(3);
/// Two `Int` words inline, for the only multi-word thing this slice moves.
const PAIR: LayoutId = LayoutId(4);
const DURATION: LayoutId = LayoutId(5);
/// An `Int` and a reference inline, which is a `struct Pair { n: Int, s: String }`.
const REF_PAIR: LayoutId = LayoutId(6);

fn span() -> Span {
    Span::new(FileId(0), 0, 0)
}

fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
    Function {
        module: Arc::from("m"),
        name: Arc::from("f"),
        params: Vec::new(),
        spans: vec![span(); code.len()],
        refs: RefMap::of(&reprs),
        reprs,
        returns,
        captures: Vec::new(),
        code,
        locals: Vec::new(),
        inlined: Vec::new(),
        span: span(),
        is_async: false,
        stub: false,
    }
}

fn program(function: Function) -> Program {
    Program {
        functions: vec![function],
        layouts: vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::word("Bool", Repr::Bool),
            Layout::word("Unit", Repr::Unit),
            Layout::inline(
                "Pair",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::word("Duration", Repr::Duration),
            Layout::inline(
                "RefPair",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Ref],
            ),
        ],
        ..Program::default()
    }
}

/// What one entry into compiled code answered.
struct Answer {
    outcome: Outcome,
    return_slot: u32,
    raise: Option<Raise>,
    raise_detail: u32,
    pending_work: u64,
}

/// Compiles the one function of `program` and enters it over `words`.
///
/// `base` is deliberately not zero in every caller: it is a *word index* into
/// the segment, so a frame that does not begin at word zero is the case that
/// catches an address formed as if it did.
fn run(program: &Program, words: &mut [u64], base: u64) -> Answer {
    let mut jit = Jit::new(helpers()).expect("this host supports native execution");
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize().expect("the code finalizes");
    enter(&jit, compiled, words, base)
}

fn enter(jit: &Jit, compiled: Compiled, words: &mut [u64], base: u64) -> Answer {
    let mut ctx = NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr());
    let entry = jit.entry(compiled);
    // Safety: `ctx.words` is `words`, `base` indexes into it, and the
    // functions below are all built with frames that fit inside it.
    let outcome = unsafe { entry(&mut ctx, base) };
    Answer {
        outcome,
        return_slot: ctx.return_slot,
        raise: ctx.raise(),
        raise_detail: ctx.raise_detail,
        pending_work: ctx.pending_work,
    }
}

/// Whether the one function of `program` is inside the slice at all.
fn compiles(program: &Program) -> bool {
    let mut jit = Jit::new(helpers()).expect("this host supports native execution");
    jit.compile(program, FunctionId(0)).is_some()
}

// --- a loop ------------------------------------------------------------------

/// `total = 0; i = 1; while i <= n { total += i; i += 1 }; return total`
///
/// Slots: `s0` the bound, `s1` the running total, `s2` the counter, `s3` the
/// condition.
///
/// ```text
/// 0: int    s1 = 0
/// 1: int    s2 = 1
/// 2: le.int s3 = s2, s0
/// 3: branch-false s3 -> 7
/// 4: add.int s1 = s1, s2
/// 5: add.int.imm s2 = s2, 1
/// 6: jump -> 2
/// 7: return s1
/// ```
fn summing_loop() -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int, Repr::Bool],
        INT,
        vec![
            Inst::Int { dst: 1, value: 0 },
            Inst::Int { dst: 2, value: 1 },
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst: 3,
                a: 2,
                b: 0,
            },
            Inst::BranchFalse { cond: 3, to: 7 },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 1,
                b: 2,
            },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 2,
                a: 2,
                value: 1,
            },
            Inst::Jump { to: 2 },
            Inst::Return { src: 1 },
        ],
    ))
}

/// The loop runs, answers, and polls exactly once per backedge.
///
/// The answer mirrors `encoded.rs`'s `int_op!` (`ADD_INT`, line 1112) and
/// `arith_imm!` (`ADD_INT_IMM`, line 1155), its `cmp_int!` (`LE_INT`) and its
/// `BRANCH_FALSE` arm at line 1191 — which tests the *word* against zero,
/// which is why a `Bool` slot holding one is taken as true.
///
/// The poll count is the interesting half. There is one backedge, the `jump`
/// at pc 6, and the loop body executes ten times for `n = 10`, so the
/// safepoint helper is entered ten times and not eleven: the iteration that
/// fails the condition leaves through the forward branch, which is not a
/// backedge and is not a safepoint.
#[test]
fn a_loop_answers_and_polls_once_per_backedge() {
    forget_polls();
    let mut words = vec![0u64; 16];
    // A frame at word 4, not word 0, so a base the code ignored would read
    // zeroes and answer zero.
    words[4] = 10;
    let answer = run(&summing_loop(), &mut words, 4);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(answer.return_slot, 1);
    assert_eq!(words[4 + 1], 55);
    assert_eq!(words[4 + 2], 11, "the counter is left one past the bound");

    let polls = polls();
    assert_eq!(polls.len(), 10, "one safepoint per backedge, ten backedges");
    assert!(
        polls.iter().all(|(pc, _)| *pc == 2),
        "a backedge's safepoint reports the pc the frame is about to resume at, \
         which is the jump's target: {polls:?}"
    );
}

/// The work charge is the static instruction count of the blocks actually
/// entered, and it is reset at every safepoint.
///
/// Spelled out rather than merely "nonzero", because ADR 0055's block
/// accounting is the part with no other test. The blocks of
/// [`summing_loop`] are `[0,2)`, `[2,4)`, `[4,7)` and `[7,8)` — four leaders:
/// the entry, the `jump`'s target, the `branch-false`'s fall-through, and the
/// `branch-false`'s target — so they are 2, 2, 3 and 1 instructions long.
///
/// The first backedge is reached having entered blocks 0, 2 and 4: 2 + 2 + 3
/// = 7. Every later one is reached from the safepoint that reset the
/// accumulator, through blocks 2 and 4: 2 + 3 = 5. The return is reached
/// after the last reset through blocks 2 and 7: 2 + 1 = 3, and that is what
/// is left pending, which is ADR 0055's "pending work charged on every exit".
#[test]
fn the_work_charge_is_the_static_block_count() {
    forget_polls();
    let mut words = vec![0u64; 8];
    words[0] = 3;
    let answer = run(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(polls(), vec![(2, 7), (2, 5), (2, 5)]);
    assert_eq!(answer.pending_work, 3);
}

/// A safepoint that answers "stop" stops the run, and leaves nothing pending.
///
/// This is the only test in which the helper returns `false`, and it is also
/// the only one that proves the frame pointer is re-derived after a call:
/// compiled code that had cached a pointer across the helper would still
/// answer here, so what this really pins is the *outcome* of a stop and the
/// accounting at it. ADR 0040's bounded stop is the helper's, and this is
/// the compiled side of it: leave at once, and do not charge twice.
#[test]
fn a_safepoint_can_stop_the_run() {
    forget_polls();
    POLLS_ALLOWED.with(|allowed| allowed.set(3));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(polls().len(), 3);
    assert_eq!(
        answer.pending_work, 0,
        "the charge went to the helper before it answered, so nothing is pending"
    );
}

// --- arithmetic --------------------------------------------------------------

/// `s2 = s0 op s1; return s2`, over `Int` slots.
fn binary(op: ArithOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    ))
}

fn arith(op: ArithOp, a: i64, b: i64) -> (Answer, Vec<u64>) {
    forget_polls();
    let mut words = vec![a as u64, b as u64, 0];
    let answer = run(&binary(op), &mut words, 0);
    (answer, words)
}

/// The ordinary answers, including the two rounding questions `sdiv` and
/// `srem` could get wrong.
///
/// `encoded.rs`'s `int_op!` (line 877) calls `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3705`), whose `Div` and `Rem` arms are
/// `checked_div` and `checked_rem` — Rust's truncating division, so `-7 / 2`
/// is `-3` and `-7 % 2` is `-1`. Cranelift's `sdiv` and `srem` are the same
/// two, which is why this passes; it is here because "the same two" is a
/// claim and not an axiom.
#[test]
fn integer_arithmetic_answers_what_the_vm_answers() {
    for (op, a, b, expected) in [
        (ArithOp::Add, 2i64, 3i64, 5i64),
        (ArithOp::Add, i64::MAX, -1, i64::MAX - 1),
        (ArithOp::Sub, 2, 3, -1),
        (ArithOp::Mul, -6, 7, -42),
        (ArithOp::Div, 7, 2, 3),
        (ArithOp::Div, -7, 2, -3),
        (ArithOp::Rem, 7, 2, 1),
        (ArithOp::Rem, -7, 2, -1),
        (ArithOp::Rem, 7, -2, 1),
    ] {
        let (answer, words) = arith(op, a, b);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(answer.return_slot, 2);
        assert_eq!(words[2] as i64, expected, "{op:?} {a} {b}");
    }
}

/// Every way `int_arith` can fail, and the error it names.
///
/// This is the test the whole slice is worth having. `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3713`) is `checked_add`,
/// `checked_sub`, `checked_mul`, and a zero test *before* `checked_div` and
/// `checked_rem`. A native `+` that wrapped where this raises would be a
/// silent wrong answer, and `Raise` is how the runtime is told which of
/// `overflowed`/`divided_by_zero` to build — so the mapping is asserted, not
/// assumed.
///
/// The two `i64::MIN` rows are the ones easiest to get wrong in either
/// direction: `checked_div(i64::MIN, -1)` is `None`, which `int_arith` reports
/// as an *overflow* of division rather than as a division by zero, and
/// Cranelift's `sdiv` would have trapped on it — a machine signal, not a Cove
/// error — if the lowering had not branched around it.
///
/// The zero-first order matters too: `int_arith`'s `Div` arm tests `b == 0`
/// before `checked_div`, so `i64::MIN / 0` is "by zero" and not "overflowed".
#[test]
fn every_arithmetic_failure_is_the_vms() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::AddOverflowed),
        (ArithOp::Add, i64::MIN, -1, Raise::AddOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::SubOverflowed),
        (ArithOp::Sub, i64::MAX, -1, Raise::SubOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::MulOverflowed),
        (ArithOp::Mul, i64::MIN, -1, Raise::MulOverflowed),
        (ArithOp::Div, 1, 0, Raise::DividedByZero),
        (ArithOp::Div, 0, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, 1, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        let (answer, _) = arith(op, a, b);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {b}");
        assert_eq!(answer.raise, Some(expected), "{op:?} {a} {b}");
        assert_eq!(
            answer.pending_work, 2,
            "the whole block is charged at its entry — both the arithmetic and \
             the return it never reached — so a raise leaves that much pending"
        );
    }
}

/// `Inst::ArithImm` is the same arithmetic with the operand in the
/// instruction, and it fails the same way.
///
/// `encoded.rs`'s `arith_imm!` (line 902) is `int_op!` with
/// `held.payload()` where the second slot read was, calling the same
/// `int_arith`. The IR says the same thing — `Inst::ArithImm`'s own
/// documentation: "The same arithmetic `Inst::Arith` does — the same overflow,
/// the same division and remainder by zero, the same `Duration` naming".
#[test]
fn an_immediate_operand_fails_the_same_way() {
    for (op, a, value, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Some(Raise::AddOverflowed)),
        (ArithOp::Sub, i64::MIN, 1, Some(Raise::SubOverflowed)),
        (ArithOp::Mul, i64::MAX, 2, Some(Raise::MulOverflowed)),
        (ArithOp::Div, 1, 0, Some(Raise::DividedByZero)),
        (ArithOp::Rem, 1, 0, Some(Raise::RemainderByZero)),
        (ArithOp::Div, i64::MIN, -1, Some(Raise::DivOverflowed)),
        (ArithOp::Add, 40, 2, None),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::ArithImm {
                    op,
                    dst: 1,
                    a: 0,
                    value,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0];
        let answer = run(&held, &mut words, 0);
        match expected {
            Some(raise) => {
                assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {value}");
                assert_eq!(answer.raise, Some(raise), "{op:?} {a} {value}");
            }
            None => {
                assert_eq!(answer.outcome, Outcome::Returned);
                assert_eq!(words[1], 42);
            }
        }
    }
}

/// A `Duration` destination renames the overflow, and only for the three
/// operations that consult the name.
///
/// `encoded.rs`'s `int_op!` asks `machine.repr(id, a!()) ==
/// Some(Repr::Duration)` and hands the answer to `int_arith`, whose `named`
/// closure is called by the `Add`, `Sub` and `Mul` arms and **not** by `Div`
/// or `Rem` (`crates/cove-runtime/src/vm/exec.rs:3713`). So a duration
/// division that overflows is still `overflowed("division")`, and that
/// asymmetry is the whole of what this test is for — it is the sort of thing a
/// reimplementation tidies up by accident.
#[test]
fn a_duration_destination_renames_only_three_overflows() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::DurationOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::DurationOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::DurationOverflowed),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Duration],
            DURATION,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0];
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?}");
        assert_eq!(answer.raise, Some(expected), "{op:?}");
    }
}

// --- fused comparisons, which are ADR 0054's -------------------------------

/// `if !(s0 op s1) goto 3; s3 = 10; return s3; s3 = 20; return s3`, with the
/// comparison fused into the branch.
fn fused(op: CmpOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
        INT,
        vec![
            Inst::CmpBranch {
                on: Compare::Int,
                op,
                dst: 2,
                a: 0,
                b: 1,
                target: 3,
            },
            Inst::Int { dst: 3, value: 10 },
            Inst::Return { src: 3 },
            Inst::Int { dst: 3, value: 20 },
            Inst::Return { src: 3 },
        ],
    ))
}

/// `Inst::CmpBranch` branches *and* writes the `Bool`, both ways.
///
/// [ADR 0054] is explicit that the fusion "is exactly the two instructions it
/// replaces, in that order, with no condition attached — the `Bool` is still
/// written to `dst`", and `encoded.rs`'s `cmp_int_branch!` macro (line 1197
/// onward, `LT_INT_BRANCH` and its siblings) does the store before the test.
/// So the write is asserted on both paths and not only on the one that falls
/// through: a lowering that skipped it on the taken path would pass a test
/// that only looked at the answer.
///
/// [ADR 0054]: ../../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md
#[test]
fn a_fused_comparison_branches_and_writes_its_bool() {
    for (op, a, b, answered, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 10i64, 1u64),
        (CmpOp::Lt, 2, 1, 20, 0),
        (CmpOp::Lt, 2, 2, 20, 0),
        (CmpOp::Ge, 2, 2, 10, 1),
        (CmpOp::Eq, -3, -3, 10, 1),
        (CmpOp::Ne, -3, -3, 20, 0),
        (CmpOp::Gt, i64::MIN, i64::MAX, 20, 0),
        (CmpOp::Le, i64::MIN, i64::MAX, 10, 1),
    ] {
        forget_polls();
        let mut words = vec![a as u64, b as u64, 0xdead, 0];
        let answer = run(&fused(op), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {b}");
        assert_eq!(
            words[2], flag,
            "the fused comparison still writes its `Bool`: {op:?} {a} {b}"
        );
    }
}

/// `Inst::CmpImmBranch` is the same, with the right operand in the
/// instruction.
///
/// `encoded.rs`'s `cmp_imm_branch!` (line 1040) is `cmp_int_branch!` reading
/// `held.lo() as i32` where the second slot was — which is why the IR's
/// immediate is an `i32` here and an `i64` on `Inst::CmpImm`. The negative
/// row is the one that matters: a lowering that zero-extended the immediate
/// instead of sign-extending it would answer the opposite.
#[test]
fn a_fused_immediate_comparison_branches_and_writes_its_bool() {
    for (op, a, value, answered, flag) in [
        (CmpOp::Lt, 1i64, 5i32, 10i64, 1u64),
        (CmpOp::Lt, 5, 5, 20, 0),
        (CmpOp::Ge, 5, 5, 10, 1),
        (CmpOp::Gt, 0, -1, 10, 1),
        (CmpOp::Lt, 0, -1, 20, 0),
        (CmpOp::Eq, -7, -7, 10, 1),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            INT,
            vec![
                Inst::CmpImmBranch {
                    op,
                    dst: 2,
                    a: 0,
                    value,
                    target: 3,
                },
                Inst::Int { dst: 3, value: 10 },
                Inst::Return { src: 3 },
                Inst::Int { dst: 3, value: 20 },
                Inst::Return { src: 3 },
            ],
        ));
        let mut words = vec![a as u64, 0, 0xdead, 0];
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {value}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {value}");
        assert_eq!(words[2], flag, "{op:?} {a} {value}");
    }
}

/// An unfused `Inst::Cmp` and `Inst::CmpImm` write the same `Bool`.
///
/// `encoded.rs`'s `cmp_int!` (line 921) and `cmp_imm!` (line 913):
/// `compare(op, x.cmp(&y))` stored as `answer as u64`, so one or zero and
/// never a mask. A lowering that stored a comparison's raw result without
/// narrowing it would put something other than 0 or 1 in a `Bool` slot, and
/// then `branch-false` — which tests the whole word — would still work while
/// `Bool` equality would not.
#[test]
fn a_comparison_writes_one_or_zero() {
    for (op, a, b, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 1u64),
        (CmpOp::Lt, 2, 1, 0),
        (CmpOp::Ge, -1, -1, 1),
        (CmpOp::Ne, -1, -1, 0),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Int,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0xdead];
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[2], flag, "{op:?} {a} {b}");

        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::CmpImm {
                    op,
                    dst: 1,
                    a: 0,
                    value: b,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0xdead];
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[1], flag, "imm {op:?} {a} {b}");
    }
}

// --- copies, traps, refusals -------------------------------------------------

/// `Inst::Copy` moves a layout's whole run of words, and moves rather than
/// smears.
///
/// `encoded.rs`'s `COPY` arm (line 1082) is `Memory::copy_slots`, which is a
/// `copy_within` — a `memmove` — and its documentation says why: "two slots
/// of one frame may overlap and a lowering is free to emit that rather than
/// having to prove it does not". The second row here is that overlap, and a
/// forward run of load-store pairs would answer `[7, 7, 7]` for it.
#[test]
fn a_copy_moves_every_word_and_does_not_smear() {
    for (dst, src, before, after) in [
        (2u32, 0u32, [7u64, 9, 0, 0], [7u64, 9, 7, 9]),
        (1, 0, [7, 9, 0, 0], [7, 7, 9, 0]),
        (0, 1, [7, 9, 11, 0], [9, 11, 11, 0]),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int; 4],
            PAIR,
            vec![
                Inst::Copy {
                    dst,
                    src,
                    layout: PAIR,
                },
                Inst::Return { src: dst },
            ],
        ));
        let mut words = before.to_vec();
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(answer.return_slot, dst);
        assert_eq!(words, after.to_vec(), "copy {src} -> {dst}");
    }
}

/// `Inst::Trap` leaves with the message's `StrId` and nothing else.
///
/// `encoded.rs`'s `TRAP` arm (line 1870) is
/// `fail!(RuntimeError::new(program.string(StrId(held.lo())).to_string()))`.
/// The lookup is the runtime's, so what crosses the boundary is the id: this
/// crate has no strings table and must not grow one, which is the same rule
/// that keeps the overflow messages out of `Raise`.
#[test]
fn a_trap_names_its_message_by_id() {
    forget_polls();
    let held = program(function(
        vec![Repr::Int],
        UNIT,
        vec![
            Inst::Int { dst: 0, value: 1 },
            Inst::Trap { message: StrId(37) },
        ],
    ));
    let mut words = vec![0u64];
    let answer = run(&held, &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::Trapped));
    assert_eq!(answer.raise_detail, 37);
    assert_eq!(answer.pending_work, 2, "both instructions of the block");
}

/// A function holding anything outside the slice compiles to `None`, whole.
///
/// ADR 0055: "A function containing an operation the native lowering does not
/// yet support runs entirely on the encoded VM. The initial implementation
/// does not split one function into native and interpreted regions." So the
/// answer is `None` and not a partly compiled entry — including for
/// `Inst::Unit`, which is one store and is refused anyway, because the line
/// is drawn at the adoption gate's list and not at what happens to be easy.
#[test]
fn anything_outside_the_slice_refuses_the_whole_function() {
    let refused: Vec<(&str, Program)> = vec![
        (
            "a unit constant, which is not on the adoption gate's list",
            program(function(
                vec![Repr::Unit],
                UNIT,
                vec![Inst::Unit { dst: 0 }, Inst::Return { src: 0 }],
            )),
        ),
        (
            "negation",
            program(function(
                vec![Repr::Int],
                INT,
                vec![
                    Inst::Neg {
                        num: Num::Int,
                        dst: 0,
                        a: 0,
                    },
                    Inst::Return { src: 0 },
                ],
            )),
        ),
        (
            "float arithmetic",
            program(function(
                vec![Repr::Float, Repr::Float],
                INT,
                vec![
                    Inst::Arith {
                        num: Num::Float,
                        op: ArithOp::Add,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a float comparison",
            program(function(
                vec![Repr::Float, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Float,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "an ordered `Bool` comparison, which the VM answers with a runtime error",
            program(function(
                vec![Repr::Bool, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Bool,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a call, which needs the entry table the next slice builds",
            program(function(
                vec![Repr::Int],
                INT,
                vec![
                    Inst::Call {
                        dst: 0,
                        callee: FunctionId(0),
                        args: cove_ir::ArgsId(0),
                    },
                    Inst::Return { src: 0 },
                ],
            )),
        ),
        (
            "a frame holding a reference, however scalar its instructions are",
            program(function(
                vec![Repr::Int, Repr::Ref],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
            )),
        ),
        // The two reference checks overlap in any *real* program — a function
        // that copies a layout holding a reference has a reference slot to
        // copy it into, so the frame check above would already have refused
        // it. This row exercises the layout check on its own, which is why its
        // frame is scalar and the program is one no lowering would emit: a
        // check that is only ever reached behind another check is a check
        // nothing tests.
        (
            "a copy of a layout holding a reference",
            program(function(
                vec![Repr::Int, Repr::Int, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::Copy {
                        dst: 2,
                        src: 0,
                        layout: REF_PAIR,
                    },
                    Inst::Return { src: 2 },
                ],
            )),
        ),
        (
            "a jump past the end, which the IR verifier refuses too",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Jump { to: 9 }],
            )),
        ),
        (
            "a body whose last instruction is not a terminator",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }],
            )),
        ),
    ];
    for (what, held) in refused {
        assert!(!compiles(&held), "should have refused: {what}");
    }
}

/// A `Bool` equality *is* inside the slice, which is the other side of the
/// refusal above.
///
/// `encoded.rs` groups `EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true)` and
/// refuses the ordered forms beside them at `not_ordered!()` (line 1213). The
/// admitted half is a word comparison; only the refused half is a runtime
/// error.
#[test]
fn a_bool_equality_is_inside_the_slice() {
    forget_polls();
    let held = program(function(
        vec![Repr::Bool, Repr::Bool, Repr::Bool],
        BOOL,
        vec![
            Inst::Cmp {
                on: Compare::Bool,
                op: CmpOp::Eq,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    ));
    let mut words = vec![1, 1, 0xdead];
    let answer = run(&held, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[2], 1);
}

/// `Inst::Bool` writes a word, which is `encoded.rs`'s
/// `CONST_BOOL | CONST_INT | CONST_FLOAT` arm (line 1057) storing
/// `held.payload()`.
#[test]
fn a_boolean_constant_is_a_word() {
    for (value, word) in [(true, 1u64), (false, 0)] {
        forget_polls();
        let held = program(function(
            vec![Repr::Bool],
            BOOL,
            vec![Inst::Bool { dst: 0, value }, Inst::Return { src: 0 }],
        ));
        let mut words = vec![0xdead];
        let answer = run(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[0], word);
    }
}

/// Two functions in one code generator, both entered, and the second one
/// compiled after the first was defined.
///
/// Not a correctness question so much as a shape one: ADR 0055 makes "a
/// function the unit of compilation, caching and selection", so a code
/// generator that could only hold one would be the wrong shape however well
/// it worked.
#[test]
fn one_code_generator_holds_many_functions() {
    forget_polls();
    let mut jit = Jit::new(helpers()).expect("this host supports native execution");
    let doubling = program(function(
        vec![Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 0,
                b: 0,
            },
            Inst::Return { src: 1 },
        ],
    ));
    let first = jit.compile(&doubling, FunctionId(0)).expect("inside");
    let second = jit.compile(&summing_loop(), FunctionId(0)).expect("inside");
    jit.finalize().expect("the code finalizes");

    let mut words = vec![21u64, 0];
    assert_eq!(enter(&jit, first, &mut words, 0).outcome, Outcome::Returned);
    assert_eq!(words[1], 42);

    let mut words = vec![0u64; 8];
    words[0] = 4;
    assert_eq!(
        enter(&jit, second, &mut words, 0).outcome,
        Outcome::Returned
    );
    assert_eq!(words[1], 10);
}
