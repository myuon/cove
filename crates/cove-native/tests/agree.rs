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

use suite::{Arm, Heap, INT, UNIT};

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
    suite::forget_calls();
    let mut cranelift_words = words.to_vec();
    let cranelift = suite::run::<Cranelift>(program, &mut cranelift_words, base);
    let cranelift_polls = suite::polls();
    let cranelift_calls = suite::calls();

    suite::forget_polls();
    suite::forget_calls();
    let mut template_words = words.to_vec();
    let template = suite::run::<Template>(program, &mut template_words, base);
    let template_polls = suite::polls();
    let template_calls = suite::calls();

    assert_eq!(cranelift.outcome, template.outcome, "outcome: {what}");
    assert_eq!(
        cranelift.returned, template.returned,
        "the destination, word for word: {what}"
    );
    assert_eq!(cranelift.raise, template.raise, "raise: {what}");
    assert_eq!(
        cranelift.raise_detail, template.raise_detail,
        "detail: {what}"
    );
    assert_eq!(cranelift.raise_pc, template.raise_pc, "raise pc: {what}");
    assert_eq!(cranelift.raise_a, template.raise_a, "raise a: {what}");
    assert_eq!(cranelift.raise_b, template.raise_b, "raise b: {what}");
    assert_eq!(
        cranelift.pending_work, template.pending_work,
        "pending work: {what}"
    );
    assert_eq!(cranelift_words, template_words, "the frame: {what}");
    assert_eq!(
        cranelift_polls, template_polls,
        "the safepoints, in order: {what}"
    );
    assert_eq!(
        cranelift_calls, template_calls,
        "the calls, in order: {what}"
    );
}

/// The same, over a heap both arms read the same words of.
///
/// A second function rather than an argument on the first, because a heap is
/// what the covefmt slice added and nothing that came before it has one: the
/// callers above would all pass a heap with nothing in it.
fn agree_over(what: &str, program: &Program, words: &[u64], base: u64, build: impl Fn() -> Heap) {
    suite::forget_polls();
    suite::forget_calls();
    let cranelift_heap = build();
    let mut cranelift_words = words.to_vec();
    let cranelift =
        suite::run_over::<Cranelift>(program, &mut cranelift_words, base, &cranelift_heap);
    let cranelift_polls = suite::polls();

    suite::forget_polls();
    suite::forget_calls();
    let template_heap = build();
    let mut template_words = words.to_vec();
    let template = suite::run_over::<Template>(program, &mut template_words, base, &template_heap);
    let template_polls = suite::polls();

    assert_eq!(cranelift.outcome, template.outcome, "outcome: {what}");
    assert_eq!(
        cranelift.returned, template.returned,
        "the destination, word for word: {what}"
    );
    assert_eq!(cranelift.raise, template.raise, "raise: {what}");
    assert_eq!(cranelift.raise_pc, template.raise_pc, "raise pc: {what}");
    assert_eq!(cranelift.raise_a, template.raise_a, "raise a: {what}");
    assert_eq!(cranelift.raise_b, template.raise_b, "raise b: {what}");
    assert_eq!(
        cranelift.pending_work, template.pending_work,
        "pending work: {what}"
    );
    assert_eq!(cranelift_words, template_words, "the frame: {what}");
    assert_eq!(cranelift_polls, template_polls, "the safepoints: {what}");
    // The two heaps started equal, so a difference here is one arm having read or
    // written a word the other did not. Until `Inst::Store` this was the weaker
    // claim that neither arm writes to the heap at all; a store *through an
    // address* does, and comparing the words afterwards is how the two decoders
    // are held to the same one.
    let touched: Vec<u64> = (0..64).map(|at| cranelift_heap.get(at)).collect();
    let also: Vec<u64> = (0..64).map(|at| template_heap.get(at)).collect();
    assert_eq!(touched, also, "the heap: {what}");
    // `build` hands back a fresh heap each time and the two are dropped here,
    // which is what keeps the chunk pointers the arms were given alive for
    // exactly as long as they were used.
    drop(cranelift_heap);
    drop(template_heap);
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

    // The covefmt slice: a heap read, a bound, a byte, a tag and a switch. Both
    // arms over the same words of the same heap, and the heap asserted unchanged
    // afterwards — neither arm writes to it.
    //
    // The object's address is a constant rather than something the heap has to
    // hand back, because it is one: a heap address is `HEAP_ORIGIN_WORDS` plus a
    // word index, and where the object was put is this test's own choice.
    let object = cove_native::HEAP_ORIGIN_WORDS + 1;
    for index in [0i64, 1, 2, 3, -1, i64::MIN] {
        agree_over(
            &format!("a load-elem at {index}"),
            &element(),
            &[object, index as u64, 0, 0],
            0,
            || {
                let mut heap = Heap::new(2);
                heap.object(1, INT, 3);
                for word in 0..6u64 {
                    heap.set(2 + word, 500 + word);
                }
                heap
            },
        );
    }
    for at in [0i64, 7, 8, 9, 10, -1] {
        agree_over(
            &format!("a byte-at at {at}"),
            &byte(),
            &[object, at as u64, 0],
            0,
            || {
                let mut heap = Heap::new(2);
                heap.object(1, INT, 10);
                heap.set(2, 0x0807_0605_0403_0201);
                heap.set(3, 0x0a09);
                heap
            },
        );
    }
    agree_over(
        "a load-elem of a null reference",
        &element(),
        &[0, 0, 0, 0],
        0,
        || Heap::new(1),
    );

    // The address family, which is the one part of the slice where the two arms
    // *decide* something at run time rather than computing it: `is_stack(addr)`
    // is a branch, and the two arms take it with a `brif` on an `icmp` and with a
    // `jb` on a `cmp` against a `movabs`. Two decoders that disagreed about the
    // boundary would read words a billion places apart and only one of them would
    // be the VM's, so they are compared on both sides of it — and on the *same*
    // side twice, because the words a load answers are the whole of what is
    // asserted.
    let heap_words = || {
        let mut heap = Heap::new(2);
        heap.set(4, 710);
        heap.set(5, 711);
        heap
    };
    for (what, addr) in [
        ("a stack address", suite::SEGMENT_ORIGIN + 4),
        ("a heap address", cove_native::HEAP_ORIGIN_WORDS + 4),
    ] {
        agree_over(
            &format!("a load through {what}"),
            &loading(),
            &[addr, 0, 0, 0, 700, 701],
            0,
            heap_words,
        );
        agree_over(
            &format!("a store through {what}"),
            &storing(),
            &[addr, 800, 801, 0, 0, 0],
            0,
            heap_words,
        );
    }
    // An address this frame formed, followed back into this frame: the one case
    // where the two runs overlap and the `memmove` order matters.
    agree(
        "a store through an address of a part of a slot",
        &through_a_slot(),
        &[0, 900, 901, 0],
        0,
    );
    agree(
        "a clear of a reference and a pair",
        &clearing(),
        &[1, 2, 3, 4],
        0,
    );

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
            vec![Repr::Int, Repr::Host],
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

/// `dst = obj[index]`, at a two-word stride.
fn element() -> Program {
    suite::program(suite::function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        suite::PAIR,
        vec![
            Inst::LoadElem {
                dst: 2,
                obj: 0,
                index: 1,
                layout: suite::PAIR,
            },
            Inst::Return { src: 2 },
        ],
    ))
}

/// `dst = <byte `at` of the string `obj`>`.
fn byte() -> Program {
    suite::program(suite::function(
        vec![Repr::Ref, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::ByteAt {
                dst: 2,
                obj: 0,
                at: 1,
            },
            Inst::Return { src: 2 },
        ],
    ))
}

/// `s1..s2 = *s0`, two words through an address.
fn loading() -> Program {
    suite::program(suite::function(
        vec![Repr::Addr, Repr::Int, Repr::Int],
        suite::PAIR,
        vec![
            Inst::Load {
                dst: 1,
                addr: 0,
                layout: suite::PAIR,
            },
            Inst::Return { src: 1 },
        ],
    ))
}

/// `*s0 = s1..s2`, two words through an address.
fn storing() -> Program {
    suite::program(suite::function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: suite::PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ))
}

/// `*(&s1 + 1) = s1..s2`: an address of this frame, moved on by a word, stored
/// through — so the two runs overlap and the `memmove` order matters.
fn through_a_slot() -> Program {
    suite::program(suite::function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::AddrOfSlot { dst: 0, slot: 1 },
            Inst::AddrOfPart {
                dst: 0,
                addr: 0,
                at: 1,
            },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: suite::PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ))
}

/// A clear of one reference word and of a two-word value location.
fn clearing() -> Program {
    suite::program(suite::function(
        vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
        INT,
        vec![
            Inst::Clear {
                slot: 1,
                layout: suite::REF,
            },
            Inst::Clear {
                slot: 1,
                layout: suite::REF_PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ))
}
