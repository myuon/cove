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

use cove_ir::{
    ArithOp, CmpOp, FunctionId, Inst, Len, Num, Program, Repr, Storage, StrId, Validation,
};
use cove_native::{Entry, NativeHelpers, Outcome, HEAP_CHUNK_WORDS};

mod suite;

use suite::{Arm, Heap, INT, PAIR, UNIT};

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

    fn code_bytes(handle: Self::Handle) -> u32 {
        handle.code_bytes
    }

    fn window_code(handle: Self::Handle) -> cove_native::WindowCode {
        handle.windows
    }

    const ATTRIBUTES_WINDOWS: bool = false;
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

    fn code_bytes(handle: Self::Handle) -> u32 {
        handle.code_bytes
    }

    fn window_code(handle: Self::Handle) -> cove_native::WindowCode {
        handle.windows
    }

    const ATTRIBUTES_WINDOWS: bool = true;
}

/// Runs `program` on both arms over identical frames and asserts they agree.
///
/// The frame is handed to each arm as a fresh copy of `words`, so the second
/// arm cannot be reading what the first left behind.
fn agree(what: &str, program: &Program, words: &[u64], base: u64) {
    suite::forget_polls();
    suite::forget_calls();
    suite::forget_allocations();
    suite::forget_mediated();
    suite::forget_built();
    suite::forget_copied();
    let mut cranelift_words = words.to_vec();
    let cranelift = suite::run::<Cranelift>(program, &mut cranelift_words, base);
    let cranelift_polls = suite::polls();
    let cranelift_calls = suite::calls();
    let cranelift_allocs = suite::allocations();
    let cranelift_mediated = suite::mediated();
    let cranelift_built = suite::built();
    let cranelift_copied = suite::copied();

    suite::forget_polls();
    suite::forget_calls();
    suite::forget_allocations();
    suite::forget_mediated();
    suite::forget_built();
    suite::forget_copied();
    let mut template_words = words.to_vec();
    let template = suite::run::<Template>(program, &mut template_words, base);
    let template_polls = suite::polls();
    let template_calls = suite::calls();
    let template_allocs = suite::allocations();
    let template_mediated = suite::mediated();
    let template_built = suite::built();
    let template_copied = suite::copied();

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
    // Two arms that agreed about the *answer* while one of them allocated and the
    // other did not would be two arms doing different work, and one of them would
    // not be the VM's.
    assert_eq!(
        cranelift_allocs, template_allocs,
        "the allocations, in order: {what}"
    );
    assert_eq!(
        cranelift_mediated, template_mediated,
        "the intrinsic calls handed to the runtime, in order: {what}"
    );
    assert_eq!(
        cranelift_built, template_built,
        "the growable operations handed to the runtime, in order: {what}"
    );
    assert_eq!(
        cranelift_copied, template_copied,
        "the run copies handed to the runtime, in order: {what}"
    );
}

/// The same, over a heap both arms read the same words of.
///
/// A second function rather than an argument on the first, because a heap is
/// what the covefmt slice added and nothing that came before it has one: the
/// callers above would all pass a heap with nothing in it.
fn agree_over(what: &str, program: &Program, words: &[u64], base: u64, build: impl Fn() -> Heap) {
    agree_with_literals(what, program, words, base, build, &[], &[]);
}

/// [`agree_over`], with what the runtime's buffer helper answers scripted.
///
/// The script has to be *re-applied for each arm* rather than set once around the
/// pair, which is the whole reason this is not four lines at a call site:
/// `suite::forget_built` clears the queue along with the log, so a script set
/// before the first arm would be gone by the second and the two would be compared
/// on two different runs.
fn agree_answering(
    what: &str,
    program: &Program,
    words: &[u64],
    base: u64,
    build: impl Fn() -> Heap,
    answers: &[Outcome],
) {
    agree_with_literals(what, program, words, base, build, &[], answers);
}

/// [`agree_over`], with a literal-address table published to both arms.
///
/// The one thing `Inst::Str` can differ on is *which* entry of that table an arm
/// reads, and an empty table cannot say so — so a case that lowers a literal
/// hands the same three addresses to both arms and the frames are compared as
/// they always are.
fn agree_with_literals(
    what: &str,
    program: &Program,
    words: &[u64],
    base: u64,
    build: impl Fn() -> Heap,
    literals: &[u64],
    answers: &[Outcome],
) {
    suite::forget_polls();
    suite::forget_calls();
    suite::forget_allocations();
    suite::forget_mediated();
    suite::mediated_answers(answers);
    suite::forget_built();
    suite::built_answers(answers);
    // The same script for a run copy, which no program here mixes with a builder:
    // each queue is read only by its own helper.
    suite::forget_copied();
    suite::copied_answers(answers);
    let cranelift_heap = build();
    let mut cranelift_words = words.to_vec();
    let cranelift = suite::run_with_literals::<Cranelift>(
        program,
        &mut cranelift_words,
        base,
        &cranelift_heap,
        literals,
    );
    let cranelift_polls = suite::polls();
    let cranelift_allocs = suite::allocations();
    let cranelift_mediated = suite::mediated();
    let cranelift_built = suite::built();
    let cranelift_copied = suite::copied();

    suite::forget_polls();
    suite::forget_calls();
    suite::forget_allocations();
    suite::forget_mediated();
    suite::mediated_answers(answers);
    suite::forget_built();
    suite::built_answers(answers);
    // The same script for a run copy, which no program here mixes with a builder:
    // each queue is read only by its own helper.
    suite::forget_copied();
    suite::copied_answers(answers);
    let template_heap = build();
    let mut template_words = words.to_vec();
    let template = suite::run_with_literals::<Template>(
        program,
        &mut template_words,
        base,
        &template_heap,
        literals,
    );
    let template_polls = suite::polls();
    let template_allocs = suite::allocations();
    let template_mediated = suite::mediated();
    let template_built = suite::built();
    let template_copied = suite::copied();

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
    assert_eq!(
        cranelift_allocs, template_allocs,
        "the allocations, in order: {what}"
    );
    assert_eq!(
        cranelift_mediated, template_mediated,
        "the intrinsic calls handed to the runtime, in order: {what}"
    );
    assert_eq!(
        cranelift_built, template_built,
        "the growable operations handed to the runtime, in order: {what}"
    );
    assert_eq!(
        cranelift_copied, template_copied,
        "the run copies handed to the runtime, in order: {what}"
    );
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

/// **Both arms order two strings the same way, through the same hand-overs.**
///
/// Every pair of `suite::ordered_strings` — equal, a prefix, the ninth byte, a
/// two-byte character, empty, straddling a chunk, and null, which orders as the
/// empty string on the encoded tier and so on both arms — over
/// `suite::ordering_strings`, with the frame, the outcome and the unpaid work
/// compared by [`agree_over`] and the leaf's hand-overs compared here.
#[test]
fn both_arms_order_strings_alike() {
    let mut names = Heap::new(2);
    let strings = suite::ordered_strings(&mut names);
    let build = || {
        let mut heap = Heap::new(2);
        suite::ordered_strings(&mut heap);
        heap
    };
    let program = suite::ordering_strings();
    for (x, a) in &strings {
        for (y, b) in &strings {
            let words = [*a, *b, 0, 0, 0];
            let what = format!("{x} against {y}");
            suite::forget_ordered();
            let mut heap = build();
            let mut left = words.to_vec();
            let cranelift = suite::run_over::<Cranelift>(&program, &mut left, 0, &heap);
            let cranelift_ordered = suite::ordered();
            suite::forget_ordered();
            heap = build();
            let mut right = words.to_vec();
            let template = suite::run_over::<Template>(&program, &mut right, 0, &heap);
            let template_ordered = suite::ordered();
            assert_eq!(cranelift.outcome, template.outcome, "outcome: {what}");
            assert_eq!(left, right, "the frame: {what}");
            assert_eq!(cranelift_ordered, template_ordered, "hand-overs: {what}");
            agree_over(&what, &program, &words, 0, build);
        }
    }
}

/// One arm's poll threshold, and what the loop left unpaid under it.
fn polling_at<A: Arm>(at: u64) -> (Vec<(u32, u64)>, u64, Vec<u64>) {
    suite::forget_polls();
    suite::poll_at(at);
    let mut words = vec![10u64, 0, 0, 0];
    let answer = suite::run::<A>(&suite::summing_loop(), &mut words, 0);
    (suite::polls(), answer.pending_work, words)
}

/// The two arms test the stride at the same turns and leave the same amount
/// unpaid, at every threshold.
///
/// ADR 0060 moved a test out of the helper and into two code generators, which
/// is two places for it to be written differently — and a difference here would
/// not be an answer a case could catch, because both arms would still sum to
/// 55. What differs is *when the runtime is asked*, which is the thing the
/// bound is about.
///
/// The thresholds are the boundaries rather than a spread: nought is the
/// default and polls always; one is the smallest threshold a turn can reach; 7
/// and 5 are exactly the first backedge's work and a later one's, so an arm
/// that wrote `>` where the other wrote `>=` disagrees at one of them; 1024 is
/// the real stride, which this loop never reaches, so both arms must poll
/// *never* and charge everything at the exit.
#[test]
fn both_arms_poll_at_the_same_turns_under_a_threshold() {
    for at in [0, 1, 5, 7, 12, 16, 1024] {
        assert_eq!(
            polling_at::<Cranelift>(at),
            polling_at::<Template>(at),
            "the poll threshold {at}"
        );
    }
    let (polls, pending, words) = polling_at::<Cranelift>(1024);
    assert!(polls.is_empty(), "a stride this loop never reaches");
    assert_eq!(words[1], 55, "and it still answers");
    assert_eq!(
        pending, 55,
        "everything it did is charged at the exit instead: 2 + 10 turns of 5 \
         + the 2 and the 1 it leaves through"
    );
    suite::forget_polls();
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

    // Negation, including the one operand `checked_neg` refuses. The two arms
    // reach the refusal differently — a flag the `neg` set against a comparison
    // with `i64::MIN` — so agreeing on it is the whole point of the row.
    for a in [0i64, 1, -1, i64::MAX, i64::MIN + 1, i64::MIN] {
        agree(
            &format!("-({a})"),
            &suite::negation(Repr::Int),
            &[a as u64, 0],
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
    // The literal table, which is the one thing in this slice that neither arm
    // computes: both read `ctx.literals[text]`, and the whole of the difference
    // between them is how the displacement is formed. The third read is followed
    // to a byte, and the offset walks off the end of the object on purpose so the
    // refusal is compared too.
    let placed = |heap: &Heap| {
        [
            heap.addr(1),
            heap.addr(HEAP_CHUNK_WORDS + 1),
            heap.addr(HEAP_CHUNK_WORDS + 5),
        ]
    };
    let literal_heap = || {
        let mut heap = Heap::new(2);
        heap.object(1, INT, 3);
        heap.set(2, 0x0000_0000_0065_6e6f);
        heap.object(HEAP_CHUNK_WORDS + 1, INT, 3);
        heap.set(HEAP_CHUNK_WORDS + 2, 0x0000_0000_006f_7774);
        heap.object(HEAP_CHUNK_WORDS + 5, INT, 5);
        heap.set(HEAP_CHUNK_WORDS + 6, 0x0000_0065_6572_6874);
        heap
    };
    let table = placed(&literal_heap());
    for at in [0i64, 4, 5, -1] {
        agree_with_literals(
            &format!("three literals and a byte at {at}"),
            &literals(),
            &[0, 0, 0, at as u64, 0],
            0,
            literal_heap,
            &table,
            &[],
        );
        agree_with_literals(
            &format!("three literals and a byte at {at}, off word zero"),
            &literals(),
            &[9, 9, 0, 0, 0, at as u64, 0],
            2,
            literal_heap,
            &table,
            &[],
        );
    }

    // ADR 0062's window, one instruction at a time: an ensure and a commit over
    // each storage with room, at exactly the room, past it, negative, onto a
    // consumed owner, another family and a length past the capacity; and a byte
    // store at each offset and each cold path. The heap is inside the words
    // compared, so every length either arm writes is compared.
    for storage in [Storage::PackedBytes, Storage::Words(INT)] {
        for commit in [false, true] {
            for (what, len, count, consumed, other) in [
                ("with room", 10u64, 3u64, false, false),
                ("of exactly the room", 10, 6, false, false),
                ("of nothing", 10, 0, false, false),
                ("past the room", 10, 7, false, false),
                ("of a negative count", 10, u64::MAX, false, false),
                ("onto a consumed owner", 0, 1, true, false),
                ("onto another family", 0, 1, false, true),
                ("past the capacity", 1 << 40, 0, false, false),
            ] {
                agree_over(
                    &format!("{storage:?} commit {commit} {what}"),
                    &suite::reserving(storage, commit),
                    &[cove_native::HEAP_ORIGIN_WORDS + 20, count, 0],
                    0,
                    move || {
                        let mut heap = Heap::new(2);
                        match storage {
                            Storage::PackedBytes => {
                                suite::a_byte_buffer(&mut heap, 20, len, 16);
                            }
                            Storage::Words(_) => {
                                suite::a_vector(&mut heap, 20, suite::VECTOR, 0, 16);
                                heap.set(21, len);
                            }
                        }
                        if consumed {
                            heap.set(22, 0);
                        }
                        if other {
                            heap.object(20, suite::PAIR_VECTOR, 0);
                        }
                        heap
                    },
                );
            }
        }
    }
    for (what, index, value, not_a_run) in [
        ("at byte 0", 0u64, 0x5Cu64, false),
        ("at byte 7", 7, 255, false),
        ("at byte 8", 8, 0, false),
        ("at the end of the run", 16, 1, false),
        ("at a negative offset", u64::MAX, 1, false),
        ("of 256", 0, 256, false),
        ("of -1", 0, u64::MAX, false),
        ("into a buffer rather than its run", 0, 1, true),
    ] {
        agree_over(
            &format!("a byte store {what}"),
            &suite::storing_a_byte(),
            &[
                cove_native::HEAP_ORIGIN_WORDS + if not_a_run { 20 } else { 28 },
                index,
                value,
                0,
            ],
            0,
            || {
                let mut heap = Heap::new(2);
                suite::a_byte_buffer(&mut heap, 20, 0, 16);
                heap
            },
        );
    }
    // And whole windows, each emitted as one fast path with its rows as the cold
    // half — so what the arms have to agree on is every way out of it. A push
    // over each storage and stride: with room; into a full store whose ensure
    // refuses, stops, or returns having made room; onto a consumed owner whose
    // ensure returns and is asked again; onto another family and onto null. A
    // byte push's own two questions: a value that is not a byte, answered and
    // refused, and a store that is not a byte run. An append over each storage:
    // with room, of nothing, past the room refused and grown, and of a negative
    // count.
    for storage in [
        Storage::PackedBytes,
        Storage::Words(INT),
        Storage::Words(PAIR),
    ] {
        let other = match storage {
            Storage::Words(elem) if elem == INT => suite::PAIR_VECTOR,
            _ => suite::VECTOR,
        };
        for (what, len, owner, room, answers) in [
            ("with room", 3u32, None, None, vec![]),
            (
                "into a full store, refused",
                8,
                None,
                None,
                vec![Outcome::Raised],
            ),
            (
                "into a full store, stopped",
                8,
                None,
                None,
                vec![Outcome::Stopped],
            ),
            ("into a full store, grown", 8, None, Some(16), vec![]),
            (
                "onto a consumed owner, asked twice",
                0,
                Some(0u64),
                None,
                vec![Outcome::Returned, Outcome::Raised],
            ),
            (
                "onto another family",
                3,
                Some(1),
                None,
                vec![Outcome::Raised],
            ),
        ] {
            agree_answering(
                &format!("a push window over {storage:?} {what}"),
                &suite::a_push_window(storage),
                &[
                    cove_native::HEAP_ORIGIN_WORDS + 20,
                    0,
                    0,
                    0,
                    0x41,
                    0x42,
                    0,
                    0,
                ],
                0,
                move || {
                    let mut heap = Heap::new(2);
                    match storage {
                        Storage::PackedBytes => {
                            suite::a_byte_buffer(&mut heap, 20, u64::from(len), 8);
                        }
                        Storage::Words(elem) => {
                            let vector = if elem == PAIR {
                                suite::PAIR_VECTOR
                            } else {
                                suite::VECTOR
                            };
                            suite::a_vector(&mut heap, 20, vector, len, 8);
                        }
                    }
                    match owner {
                        Some(0) => heap.set(22, 0),
                        Some(_) => {
                            heap.object(20, other, 0);
                        }
                        None => {}
                    }
                    if let Some(capacity) = room {
                        suite::room_on_ensure(capacity);
                    }
                    heap
                },
                &answers,
            );
        }
        agree_over(
            &format!("a push window over {storage:?} onto null"),
            &suite::a_push_window(storage),
            &[0, 0, 0, 0, 0x41, 0x42, 0, 0],
            0,
            || Heap::new(2),
        );
    }
    for (what, value, other_store, answers) in [
        ("of 256, refused", 256u64, false, vec![Outcome::Raised]),
        ("of -1, answered", u64::MAX, false, vec![]),
        (
            "into a store of another family",
            0x41,
            true,
            vec![Outcome::Raised],
        ),
    ] {
        agree_answering(
            &format!("a byte push window {what}"),
            &suite::a_push_window(Storage::PackedBytes),
            &[cove_native::HEAP_ORIGIN_WORDS + 20, 0, 0, 0, value, 0, 0],
            0,
            move || {
                let mut heap = Heap::new(2);
                suite::a_byte_buffer(&mut heap, 20, 3, 16);
                if other_store {
                    heap.object(28, suite::STORE, 16);
                }
                heap
            },
            &answers,
        );
    }
    for storage in [Storage::PackedBytes, Storage::Words(INT)] {
        for (what, len, count, room, answers) in [
            ("with room", 3u32, 4u64, None, vec![]),
            ("of nothing", 8, 0, None, vec![]),
            ("past the room, refused", 6, 4, None, vec![Outcome::Raised]),
            ("past the room, grown", 6, 4, Some(16), vec![]),
            (
                "of a negative count",
                3,
                u64::MAX,
                None,
                vec![Outcome::Raised],
            ),
        ] {
            agree_answering(
                &format!("an append window over {storage:?} {what}"),
                &suite::an_append_window(storage),
                &[cove_native::HEAP_ORIGIN_WORDS + 20, 0, count, 0, 0x77, 0, 0],
                0,
                move || {
                    let mut heap = Heap::new(2);
                    match storage {
                        Storage::PackedBytes => {
                            suite::a_byte_buffer(&mut heap, 20, u64::from(len), 8);
                        }
                        Storage::Words(_) => {
                            suite::a_vector(&mut heap, 20, suite::VECTOR, len, 8);
                        }
                    }
                    if let Some(capacity) = room {
                        suite::room_on_ensure(capacity);
                    }
                    heap
                },
                &answers,
            );
        }
        agree_over(
            &format!("an append window over {storage:?} onto null"),
            &suite::an_append_window(storage),
            &[0, 0, 1, 0, 0x77, 0, 0],
            0,
            || Heap::new(2),
        );
    }

    // Three of ADR 0052's four, each handed to the runtime whole: what the two
    // arms have to agree on is the hand-over, its operands and its order, and on
    // what the helper's outcome does to the frame when it is not `Returned`.
    for words in [vec![0u64, 3, 0], vec![9, 9, 0, 3, 0]] {
        let base = if words.len() > 3 { 2 } else { 0 };
        agree_over(
            "a builder allocated, appended to and finished",
            &buffers(),
            &words,
            base,
            || Heap::new(1),
        );
    }
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        agree_answering(
            &format!("a builder whose first operation answered {outcome:?}"),
            &buffers(),
            &[0, 3, 0],
            0,
            || Heap::new(1),
            &[outcome],
        );
    }

    // ADR 0058's run copy, over both storages, handed to the runtime whole — and
    // what a refusal or a stop from inside it does to the frame.
    for (words, base) in [(vec![7u64, 1, 9, 2, 3], 0), (vec![5, 5, 7, 1, 9, 2, 3], 2)] {
        agree_over(
            "a byte copy and a word copy",
            &suite::run_copies(),
            &words,
            base,
            || Heap::new(1),
        );
    }
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        agree_answering(
            &format!("a run copy that answered {outcome:?}"),
            &suite::run_copies(),
            &[7, 1, 9, 2, 3],
            0,
            || Heap::new(1),
            &[outcome],
        );
    }
    // And the run slice, which is the same helper writing the row's `dst`.
    for (words, base) in [(vec![9u64, 2, 3, 77], 0), (vec![5, 5, 9, 2, 3, 77], 2)] {
        agree_over("a word slice", &suite::run_slices(), &words, base, || {
            Heap::new(1)
        });
    }
    for (words, base) in [(vec![9u64, 2, 3, 77], 0), (vec![5, 5, 9, 2, 3, 77], 2)] {
        agree_over(
            "a byte slice",
            &suite::run_byte_slices(),
            &words,
            base,
            || Heap::new(1),
        );
    }
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        agree_answering(
            &format!("a run slice that answered {outcome:?}"),
            &suite::run_slices(),
            &[9, 2, 3, 77],
            0,
            || Heap::new(1),
            &[outcome],
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

    // `Inst::Alloc`, in all three of `Len`'s forms and in the shape where the
    // runtime refuses. Both arms hand the *same three numbers* to the same helper
    // and store the same answer, which is what the recorded allocations say, and a
    // refusal has to leave both of them the same way.
    for (what, len, word) in [
        ("fixed", Len::Fixed, 0i64),
        ("a count", Len::Count(7), 0),
        ("a slot", Len::Slot(0), 5),
        // A count no `u32` holds, handed over as it lies: the narrowing is
        // `Machine::allocate`'s and neither arm may do it first.
        ("a slot holding more than a u32", Len::Slot(0), -1),
    ] {
        agree_over(
            &format!("an allocation of {what}"),
            &suite::allocating(len),
            &[word as u64, 0],
            0,
            || Heap::new(2),
        );
    }
    // And the refusal. `allocations_allowed` is set before *each* arm because both
    // helpers forget it, which is what keeps the two runs identical.
    for arm in [0usize, 1] {
        suite::allocations_allowed(arm);
        agree_over(
            &format!("an allocation refused after {arm}"),
            &suite::allocating(Len::Count(3)),
            &[0, 0],
            0,
            || Heap::new(2),
        );
    }

    // `Inst::StoreElem`: the stride, the bound, and a null receiver. The heap
    // comparison is what makes this a test — the words go *into* the heap, so two
    // arms that disagreed about the payload offset disagree here and nowhere else.
    for index in [0i64, 1, 2, 3, -1, i64::MIN] {
        agree_over(
            &format!("a store-elem at {index}"),
            &storing_an_element(),
            &[object, index as u64, 0x1111, 0x2222],
            0,
            || {
                let mut heap = Heap::new(2);
                heap.object(1, INT, 3);
                heap
            },
        );
    }

    // An intrinsic call of each effect class, answering `Returned` and then
    // `Raised`: the two arms publish the same work, test the same outcomes and
    // leave the same frame, which is the protocol written twice from one
    // `IntrinsicProtocol`.
    for (receiver, operation) in suite::INTRINSIC_CLASSES {
        let intrinsic = cove_ir::Intrinsic::from_names(receiver, operation).expect("an intrinsic");
        for scripted in [Outcome::Returned, Outcome::Raised] {
            agree_answering(
                &format!("`{intrinsic}` answering {scripted:?}"),
                &suite::intrinsic_calling(receiver, operation),
                &[0, 0, 0, 0, 0xfeed, 0, 0, 0],
                3,
                || Heap::new(1),
                &[scripted],
            );
        }
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
        // Float negation, and not `Num::Int` — which both arms now lower. The row
        // is still a `Neg`, because what it asserts is that admitting one `Num`
        // did not admit the other.
        suite::program(suite::function(
            vec![Repr::Float],
            INT,
            vec![
                Inst::Neg {
                    num: Num::Float,
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

/// Three literals read in one body, and a byte taken out of the last of them.
///
/// One `Inst::Str` could not separate two arms that both read the table's first
/// entry, so this reads three different entries and then *follows* one — the
/// address the third store answered is the object a `byte-at` reads through, so
/// an arm that scaled the displacement differently answers a different byte
/// rather than a merely different number.
fn literals() -> Program {
    suite::program_with_strings(
        suite::function(
            vec![Repr::Ref, Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(2),
                },
                Inst::Str {
                    dst: 2,
                    text: StrId(1),
                },
                Inst::RunLoad {
                    dst: 4,
                    run: 2,
                    index: 3,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 4 },
            ],
        ),
        &["one", "two", "three"],
    )
}

/// The byte buffer's mediated pair in one body, in the order a builder is used.
///
/// Neither arm emits a fast path for either of them, so what has to agree is
/// what each hands over and in what order — and `agree` already compares
/// `suite::built()` between the two arms for exactly that. What fills a buffer
/// between them is ADR 0062's window, whose rows both arms emit, and the cases
/// above compare those.
fn buffers() -> Program {
    suite::program(suite::function(
        vec![Repr::Ref, Repr::Int, Repr::Ref],
        suite::REF,
        vec![
            Inst::GrowableAlloc {
                dst: 0,
                capacity: 1,
                storage: Storage::PackedBytes,
            },
            Inst::RunFinish {
                dst: 2,
                owner: 0,
                target: suite::REF,
                validation: Validation::Utf8,
                storage: Storage::PackedBytes,
            },
            Inst::Return { src: 2 },
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

/// `obj[index] = s2..s3`, at a two-word stride.
fn storing_an_element() -> Program {
    suite::program(suite::function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::StoreElem {
                obj: 0,
                index: 1,
                src: 2,
                layout: PAIR,
            },
            Inst::Return { src: 1 },
        ],
    ))
}

/// `dst = <byte `at` of the string `obj`>`.
fn byte() -> Program {
    suite::program(suite::function(
        vec![Repr::Ref, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::RunLoad {
                dst: 2,
                run: 0,
                index: 1,
                storage: Storage::PackedBytes,
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
