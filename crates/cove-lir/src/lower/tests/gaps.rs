//! The two things that stop a lowering.
//!
//! Neither is a refusal. One is the checker having declined to settle a
//! type, which is a compile error about the program and not a limit of the
//! backend. The rest are constructs this task left for the next one, and
//! each is written down here so that the list fails when it changes rather
//! than quietly growing or shrinking.

use super::{checked, refused};
use crate::lower::lower;

#[test]
fn an_unsettled_type_is_a_compile_error_rather_than_a_boxed_value() {
    // ADR 0034 and `docs/LINEAR_VM.md` separate the two kinds of "no type
    // here". A value whose type is *intentionally* erased is a box; a
    // `Ty::Unknown` is the checker declining, and turning one into runtime
    // dispatch would make the two the same thing when only one of them is a
    // program. The code is its own, so a consumer can tell them apart.
    let faults = lower(&checked(
        "use mystery.thing\nfn f() -> Int {\n  let x = thing()\n  1\n}",
    ))
    .expect_err("an unsettled type stops the lowering");
    let codes: Vec<&str> = faults.iter().map(|item| item.code.as_str()).collect();
    assert!(codes.contains(&"cove::lower::unknown_type"), "{codes:?}");
}

#[test]
fn a_declaration_this_lowering_has_no_frame_for_is_named_once() {
    // The gap is reported where the declaration is written, and its body is
    // not walked at all: repeating the same news at every expression inside
    // it would bury the one line that says where the work is.
    assert_eq!(
        refused("fn greet() -> String { \"hi\" }"),
        vec!["not yet lowered: a value of type `String`"]
    );
    assert_eq!(
        refused("fn same<T>(value: T) -> T { value }"),
        vec![
            "not yet lowered: a generic function",
            "not yet lowered: a value of type `T`",
            "not yet lowered: a value of type `T`",
        ]
    );
    assert_eq!(
        refused("fn apply(f: fn(Int) -> Int, n: Int) -> Int { f(n) }"),
        vec!["not yet lowered: a value of type `fn(Int) -> Int`"]
    );
}

#[test]
fn a_declared_type_is_the_heap_task() {
    assert_eq!(
        refused("struct Point { x: Int, y: Int }\nfn origin() -> Int { 0 }"),
        vec!["not yet lowered: a `struct` declaration"]
    );
    assert_eq!(
        refused("enum Verdict { Keep, Drop }\nfn origin() -> Int { 0 }"),
        vec!["not yet lowered: an `enum` declaration"]
    );
}

#[test]
fn a_test_is_lowered_like_any_declaration_and_its_return_type_is_what_stops_it() {
    // `test fn` needs nothing of its own here: it is a declaration of its
    // module and its body is checked like any other. What is not here yet is
    // the `Result` the language fixes as a test's answer.
    assert_eq!(
        refused("test fn works() -> Result<Unit, Error> {\n  Ok(())\n}"),
        vec!["not yet lowered: a value of type `Result<(), Error>`"]
    );
}

#[test]
fn the_forms_this_task_left_out_say_so_one_by_one() {
    assert_eq!(
        refused(
            "fn each(n: Int) -> Int {\n  var total = 0\n  for i in 0..n { total += i }\n  total\n}"
        ),
        vec!["not yet lowered: `for`"]
    );
    assert_eq!(
        refused("fn later() -> Int {\n  let f = fn(x: Int) { x }\n  1\n}"),
        vec!["not yet lowered: a lambda"]
    );
    assert_eq!(
        refused("fn nap() -> Int {\n  let g = 1\n  fn inner() -> Int { 2 }\n  g\n}"),
        vec!["not yet lowered: a declaration inside a body"]
    );
}
