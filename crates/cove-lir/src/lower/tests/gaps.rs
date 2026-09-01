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
    assert_eq!(
        refused("fn hold(items: Map<String, Int>) -> Int { 0 }"),
        vec!["not yet lowered: a value of type `Map<String, Int>`"]
    );
    // A sequence is one reference whatever it holds, but its element's word
    // is what its layout is keyed by — so an element with no word is a
    // sequence this lowering cannot describe rather than a reference to an
    // object it could not build.
    assert_eq!(
        refused("fn hold(rows: Array<Map<String, Int>>) -> Int { 0 }"),
        vec!["not yet lowered: a value of type `Array<Map<String, Int>>`"]
    );
}

#[test]
fn a_declared_type_is_a_shape_rather_than_a_declaration_to_lower() {
    // A `struct` and an `enum` produce no code: they say what an object is
    // made of, and the layout is built where a value of the type is met. So
    // a declaration on its own stops nothing.
    let program = lower(&checked(
        "struct Point { x: Int, y: Int }\n\
         enum Verdict { Keep, Drop }\n\
         fn origin() -> Int { 0 }",
    ))
    .expect("a declared type is not a gap");
    // And it is built only where it is used, so a program that declares one
    // and never names a value of it declares no layout for it either.
    assert!(!program
        .layouts
        .iter()
        .any(|layout| &*layout.name == "Point" || &*layout.name == "Verdict"));
}

#[test]
fn a_generic_declaration_is_what_is_left_of_the_declared_types() {
    // A generic declaration's fields are type parameters, and a type
    // parameter has no word — so there is no shape to build until there are
    // generics.
    assert_eq!(
        refused("struct Holder<T> { value: T }\nfn origin() -> Int { 0 }"),
        vec!["not yet lowered: a generic `struct` declaration"]
    );
    assert_eq!(
        refused("enum Maybe<T> { Yes(T), No }\nfn origin() -> Int { 0 }"),
        vec!["not yet lowered: a generic `enum` declaration"]
    );
}

#[test]
fn a_test_is_lowered_like_any_declaration() {
    // `test fn` needs nothing of its own here: it is a declaration of its
    // module, its body is checked like any other, and the `Result` the
    // language fixes as a test's answer is an enum like any other.
    let program = lower(&checked(
        "test fn works() -> Result<Unit, Error> {\n  Ok(())\n}",
    ))
    .expect("a test lowers like any declaration");
    assert!(program.function_named("m", "works").is_some());
}

#[test]
fn the_forms_this_task_left_out_say_so_one_by_one() {
    assert_eq!(
        refused("fn later() -> Int {\n  let f = fn(x: Int) { x }\n  1\n}"),
        vec!["not yet lowered: a lambda"]
    );
    assert_eq!(
        refused("fn nap() -> Int {\n  let g = 1\n  fn inner() -> Int { 2 }\n  g\n}"),
        vec!["not yet lowered: a declaration inside a body"]
    );
    assert_eq!(
        refused(
            "struct P { x: Int }\nimpl P {\n  fn get(self) -> Int { self.x }\n}\n\
             fn read(p: P) -> Int { p.get() }"
        ),
        vec![
            "not yet lowered: a method or associated function",
            "not yet lowered: a method call",
        ]
    );
}
