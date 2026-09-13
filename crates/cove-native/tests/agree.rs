//! The two arms, on the same IR, answering the same thing.
//!
//! The suite holds each arm to the encoded tier's behaviour written out as
//! literals, which is the strongest thing available while this crate cannot
//! depend on `cove-runtime`. This file asks the other question: not "is each
//! one right" but "are they the same run" — the same outcome, the same words
//! left in the frame, the same unpaid work, and the same safepoints in the same
//! order.
//!
//! It is what makes the comparison a comparison. A performance number from two
//! code generators that computed different things would be a number about
//! nothing, and a subset one arm admits and the other refuses would be the same
//! problem one step earlier — so the refusals are compared too.
//!
//! Compiled only when both features are on, which is why CI runs a third pass
//! over this crate with `--features cranelift,template`.

#![cfg(all(feature = "cranelift", feature = "template"))]

use cove_ir::{ArithOp, CmpOp, FunctionId, Inst, Num, Program, Repr, StrId};
use cove_native::{Entry, NativeHelpers};

mod suite;

use suite::{Arm, INT, UNIT};

struct Cranelift(cove_native::Jit);

impl Arm for Cranelift {
    type Handle = cove_native::Compiled;

    fn new(helpers: NativeHelpers) -> Self {
        Cranelift(cove_native::Jit::new(helpers).expect("this host supports native execution"))
    }

    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Self::Handle> {
        self.0.compile(program, id)
    }

    fn finalize(&mut self) {
        self.0.finalize().expect("the code finalizes");
    }

    fn entry(&self, handle: Self::Handle) -> Entry {
        self.0.entry(handle)
    }
}

struct Template(cove_native::template::Jit);

impl Arm for Template {
    type Handle = cove_native::template::Compiled;

    fn new(helpers: NativeHelpers) -> Self {
        Template(cove_native::template::Jit::new(helpers).expect("this host is x86-64"))
    }

    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Self::Handle> {
        self.0.compile(program, id)
    }

    fn finalize(&mut self) {
        self.0.finalize().expect("the code finalizes");
    }

    fn entry(&self, handle: Self::Handle) -> Entry {
        self.0.entry(handle)
    }
}

/// Runs `program` on both arms over identical frames and asserts they agree.
///
/// The frame is handed to each arm as a fresh copy of `words`, so the second
/// arm cannot be reading what the first left behind.
fn agree(what: &str, program: &Program, words: &[u64], base: u64) {
    suite::forget_polls();
    let mut cranelift_words = words.to_vec();
    let cranelift = suite::run::<Cranelift>(program, &mut cranelift_words, base);
    let cranelift_polls = suite::polls();

    suite::forget_polls();
    let mut template_words = words.to_vec();
    let template = suite::run::<Template>(program, &mut template_words, base);
    let template_polls = suite::polls();

    assert_eq!(cranelift.outcome, template.outcome, "outcome: {what}");
    assert_eq!(cranelift.return_slot, template.return_slot, "slot: {what}");
    assert_eq!(cranelift.raise, template.raise, "raise: {what}");
    assert_eq!(
        cranelift.raise_detail, template.raise_detail,
        "detail: {what}"
    );
    assert_eq!(
        cranelift.pending_work, template.pending_work,
        "pending work: {what}"
    );
    assert_eq!(cranelift_words, template_words, "the frame: {what}");
    assert_eq!(
        cranelift_polls, template_polls,
        "the safepoints, in order: {what}"
    );
}

/// The loop, the arithmetic, the comparisons, the copy and the trap — the whole
/// subset, on both arms, word for word.
#[test]
fn both_arms_answer_the_same_thing() {
    agree(
        "the summing loop",
        &suite::summing_loop(),
        &[10, 0, 0, 0],
        0,
    );
    agree(
        "the summing loop, at a frame that does not start at word zero",
        &suite::summing_loop(),
        &[0, 0, 0, 0, 7, 0, 0, 0],
        4,
    );
    agree("a trap", &trap(), &[0], 0);

    for (op, a, b) in [
        (ArithOp::Add, 2i64, 3i64),
        (ArithOp::Add, i64::MAX, 1),
        (ArithOp::Sub, i64::MIN, 1),
        (ArithOp::Mul, -6, 7),
        (ArithOp::Mul, i64::MAX, 2),
        (ArithOp::Div, -7, 2),
        (ArithOp::Div, 1, 0),
        (ArithOp::Div, i64::MIN, -1),
        (ArithOp::Rem, -7, 2),
        (ArithOp::Rem, 1, 0),
        (ArithOp::Rem, i64::MIN, -1),
    ] {
        agree(
            &format!("{op:?} {a} {b}"),
            &suite::binary(op),
            &[a as u64, b as u64, 0],
            0,
        );
    }

    for op in [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ] {
        for (a, b) in [(1i64, 2i64), (2, 2), (2, 1), (i64::MIN, i64::MAX)] {
            agree(
                &format!("fused {op:?} {a} {b}"),
                &suite::fused(op),
                &[a as u64, b as u64, 0xdead, 0],
                0,
            );
        }
    }
}

/// Both arms refuse the same programs.
///
/// The subset is one predicate, shared, so this cannot drift — which is exactly
/// why it is worth asserting once: the sharing is the claim.
#[test]
fn both_arms_refuse_the_same_programs() {
    let refused = [
        suite::program(suite::function(
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
        suite::program(suite::function(
            vec![Repr::Int, Repr::Ref],
            INT,
            vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
        )),
        suite::program(suite::function(
            vec![Repr::Int],
            INT,
            vec![Inst::Int { dst: 0, value: 1 }],
        )),
    ];
    for program in &refused {
        assert!(!suite::compiles::<Cranelift>(program));
        assert!(!suite::compiles::<Template>(program));
    }
    // And the other way round: a program inside the subset is admitted by both,
    // so the assertions above are not both vacuous.
    assert!(suite::compiles::<Cranelift>(&suite::summing_loop()));
    assert!(suite::compiles::<Template>(&suite::summing_loop()));
}

fn trap() -> Program {
    suite::program(suite::function(
        vec![Repr::Int],
        UNIT,
        vec![
            Inst::Int { dst: 0, value: 1 },
            Inst::Trap { message: StrId(37) },
        ],
    ))
}
