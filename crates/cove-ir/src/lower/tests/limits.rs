//! The frame limit a sixteen-bit slot operand costs, at its exact boundary.
//!
//! ADR 0041 adopts `Function::reprs.len() <= 65_536` and gives the exact
//! number so that the boundary can be tested exactly rather than
//! approximately: **65,536 compiles and 65,537 does not.** The cases below are
//! those two and nothing between them, because a limit tested at 60,000 and at
//! 200,000 is a limit whose position nobody has checked.
//!
//! # The construction, and the one place the ADR's own is not usable
//!
//! It is ADR 0041's — a generic struct and a nested type annotation — and it
//! is worth saying why it is that and not a function with a great many locals.
//! Reaching the cap with locals takes about 65,000 lines: the lowering hands a
//! dead run to the next value of the same shape, so only simultaneously live
//! bindings count. A deeply nested *expression* meets the parser's 64-level
//! limit first. What works, and works in a hundred and sixty bytes, is
//! **inline value width**: a value occupies as many frame words as its layout,
//! a struct's fields are inline, and nothing caps how deep a generic may be
//! instantiated — so a width doubles at every level of nesting.
//!
//! The ADR writes it as `struct Pair<T> { a: T, b: T }` with
//! `let x: Pair<Pair<…>> = build()`, and that is [`pair`] below, which is what
//! the over-the-limit cases use. It cannot be the *fitting* case, and the
//! reason is worth recording: at fifteen levels `main`'s frame is exactly
//! 65,536 as the ADR says, but the `build` that has to produce the value
//! carries the argument, the answer and a temporary, so its own frame is
//! 81,920 — over the cap that `main` is exactly at. The ADR measured the
//! binding and not the builder.
//!
//! [`node`] is the same construction with a third, one-word field, which
//! subtracts one from every level: `Node<T>` is `2 × width(T) + 1`, so fifteen
//! levels is `2^16 - 1` words and a frame of exactly 65,536 with the answer
//! beside it. Nothing else is different, and it needs no builder at all —
//! a parameter's layout comes from its annotation, and an uncalled function is
//! lowered like any other.

use cove_diag::Diagnostic;
use cove_schema::HostSchemas;

use super::checked;
use crate::lower::lower;

/// `fn wide(x: Node<Node<…>>) -> Int`, whose frame is exactly `2^(depth + 1)`.
///
/// `x` occupies `2^(depth + 1) - 1` words and the answer is the one beside it.
/// The body is `x.v` rather than a literal because a field of an inline value
/// is slot arithmetic and not an instruction, so it costs no temporary — which
/// is what makes the total a round number rather than one over it.
fn node(depth: usize, extra: &str) -> String {
    let mut annotation = String::from("Int");
    for _ in 0..depth {
        annotation = format!("Node<{annotation}>");
    }
    format!(
        "struct Node<T> {{ l: T, r: T, v: Int }}\n\
         fn wide(x: {annotation}) -> Int {{\n\
         {extra}  x.v\n\
         }}\n"
    )
}

/// ADR 0041's own construction: `let x: Pair<Pair<…>> = build()`, whose frame
/// is `2^(depth + 1)`.
fn pair(depth: usize) -> String {
    let mut annotation = String::from("Int");
    let mut call = String::from("1");
    for _ in 0..depth {
        annotation = format!("Pair<{annotation}>");
        call = format!("dup({call})");
    }
    format!(
        "struct Pair<T> {{ a: T, b: T }}\n\
         fn dup<T>(v: T) -> Pair<T> {{ Pair(a: v, b: v) }}\n\
         export fn main() -> Int {{\n  \
           let x: {annotation} = {call}\n  \
           0\n\
         }}\n"
    )
}

/// How wide `m.wide`'s frame is, for a program that lowers.
fn frame(source: &str) -> usize {
    let (sources, held) = checked(source);
    let program = lower(&held, &sources, &HostSchemas::new()).expect("the program lowers");
    program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "wide")
        .expect("`wide` was lowered")
        .reprs
        .len()
}

/// What stopped a lowering, whole, so that a case can read the code, the rule
/// and the help as well as the message.
fn refusals(source: &str) -> Vec<Diagnostic> {
    let (sources, held) = checked(source);
    match lower(&held, &sources, &HostSchemas::new()) {
        Ok(_) => panic!("the program lowered, and this case is about what stops one"),
        Err(items) => items,
    }
}

/// A frame of exactly 65,536 words is exactly encodable, and compiles.
///
/// The upper half of the boundary. A `u16` names slots 0 through 65,535, so
/// the largest frame whose every slot has a number is one *more* than the
/// largest slot number — which is why the limit is 65,536 and not the 65,535
/// issue #245 states, and why this case is the one that has to be exact.
#[test]
fn a_frame_of_exactly_sixty_five_thousand_five_hundred_and_thirty_six_words_compiles() {
    assert_eq!(frame(&node(15, "")), 65_536);
    assert_eq!(frame(&node(15, "")), crate::MAX_FRAME_WORDS);
    assert_eq!(node(15, "").len(), 161, "a hundred and sixty-one bytes");
    // The doubling this rests on, so that a change in the lowering which moved
    // the boundary would fail here rather than quietly retarget the case
    // below.
    assert_eq!(frame(&node(14, "")), 32_768);
    assert_eq!(frame(&node(1, "")), 4);
}

/// One word more is refused, and refused at compile time.
///
/// The lower half of the boundary. `let y = 1` is that one word — an `Int`
/// local with no dead run of its shape to reuse — so the two programs are
/// twelve bytes apart and one of them is a frame the encoding cannot name.
#[test]
fn a_frame_of_one_word_more_than_the_limit_is_refused() {
    let said = refusals(&node(15, "  let y = 1\n"));
    let about: Vec<&str> = said.iter().map(|item| item.code.as_str()).collect();
    assert_eq!(about, ["cove::lower::frame_too_large"]);
    assert!(
        said[0].message.starts_with(
            "this function's frame is 65537 words, and a function's frame may hold at most 65536"
        ),
        "{}",
        said[0].message
    );
    // Nothing truncated and nothing wrapped: 65,537 is reported as 65,537, and
    // not as slot 0 written twice.
    assert!(!said[0].message.contains("65536 words"));
}

/// The message names the layout that produced the frame, not only the number.
///
/// ADR 0041 is explicit that a bare word count points at the wrong cause: a
/// frame over the limit is almost never too many locals, and almost always one
/// binding whose layout is enormous. So the diagnostic names the widest
/// locations and what each of them holds.
#[test]
fn the_refusal_names_the_layout_and_the_binding_rather_than_only_the_size() {
    let said = refusals(&node(16, ""));
    let mut lines = said[0].message.lines();
    assert_eq!(
        lines.next(),
        Some(
            "this function's frame is 131072 words, and a function's frame may hold at most \
             65536:"
        )
    );
    let cause = lines.next().expect("the message names a cause");
    assert!(
        cause.starts_with("  the parameter `x` is a `m.Node<"),
        "{cause}"
    );
    assert!(cause.ends_with(">`, 131071 words"), "{cause}");
    assert!(said[0].primary.is_some(), "it points at the declaration");
    assert!(said[0]
        .help
        .as_deref()
        .expect("it says what to do")
        .contains("doubles at every level of nesting"));
}

/// The same, on ADR 0041's own counter-example: 131,072 words from a program
/// of a few hundred bytes, and the local that is half of it.
///
/// This is the row the ADR weighs the whole decision against — *"twelve bytes
/// of source are the difference between fitting and not"* — so it is here as
/// itself rather than as a paraphrase.
#[test]
fn the_adrs_own_counter_example_is_refused_and_names_the_local() {
    let said = refusals(&pair(16));
    let about = said
        .iter()
        .find(|item| item.message.starts_with("this function's frame is 131072"))
        .expect("`main`'s frame is 131,072 words");
    let cause = about.message.lines().nth(1).expect("it names a cause");
    assert!(
        cause.starts_with("  the local `x` is a `m.Pair<"),
        "{cause}"
    );
    assert!(cause.ends_with(">`, 65536 words"), "{cause}");
    // Every function at fault, not the first: one enormous layout reaches
    // several frames — the binding that holds it and whatever built it — and a
    // reader should not have to compile again to find the next.
    assert!(said.len() > 1, "the builder's frame is over too: {said:#?}");
    assert!(said
        .iter()
        .all(|item| item.code == "cove::lower::frame_too_large"));
}

/// It is a compile-time refusal about one *function*, and it says which — the
/// whole improvement over what the same program did before.
///
/// A `Pair` nested twenty deep used to be accepted by the checker, accepted by
/// the lowering, and then fail at its first call with `"this call nests too
/// deeply"`: a run-time message about recursion, for a program that did not
/// recurse. The run's stack budget is a different limit, at a different time,
/// with a different message, and this diagnostic tells itself apart from it in
/// the rule it states.
#[test]
fn the_frame_limit_tells_itself_apart_from_the_runs_stack_budget() {
    let said = refusals(&pair(20));
    let about = &said[0];
    assert_eq!(about.code, "cove::lower::frame_too_large");
    let rule = about.rule.as_deref().expect("it states the rule");
    assert!(rule.contains("A slot number is sixteen bits"), "{rule}");
    assert!(
        rule.contains("not the run's stack budget"),
        "it is told apart from the limit it is not: {rule}"
    );
    assert!(said
        .iter()
        .any(|item| item.message.contains("2097152 words")));
}

/// Every program this repository keeps is far under the limit, so nothing in
/// the corpus is affected — and a frame this size is not a shape anyone writes
/// by accident.
///
/// ADR 0041 measured the corpus maximum at 122 words over 1,223 functions.
/// This is the same claim made about something small enough to assert on: an
/// ordinary function's frame is two orders of magnitude under the cap.
#[test]
fn an_ordinary_function_is_nowhere_near_the_limit() {
    assert!(frame(&node(3, "")) < 100);
    assert!(frame(&node(6, "")) * 100 < crate::MAX_FRAME_WORDS);
}
