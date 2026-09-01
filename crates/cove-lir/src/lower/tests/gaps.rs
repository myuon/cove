//! What stops a lowering, and what does not.
//!
//! There is no `Unsupported` here and no admission predicate: the target is
//! that **every valid checked program lowers**, so a construct this lowering
//! meets and cannot emit code for is a hole in this crate rather than a
//! program the backend declines. Every case below names one, and each is
//! scheduled to be removed by a later task.

use super::refused;

#[test]
fn a_lambda_is_named_rather_than_refused() {
    assert_eq!(
        refused("fn f() -> Int {\n  let g = fn(x: Int) { x + 1 }\n  1\n}"),
        vec!["not yet lowered: a lambda"]
    );
}

#[test]
fn a_closure_taking_sequence_method_names_the_operation() {
    // `docs/LINEAR_VM.md` says such a method lowers to a loop in the IR
    // rather than to a builtin that calls back, and the loop needs a call
    // through a function value first. The operation is what is named, not
    // the lambda: the message is what says where the next piece of work is.
    assert_eq!(
        refused("fn f(xs: Array<Int>) -> Array<Int> { xs.map(fn(x) { x * 2 }) }"),
        vec!["not yet lowered: `Array.map`"]
    );
}

#[test]
fn a_map_and_a_set_have_no_layout_yet() {
    assert_eq!(
        refused("fn f(m: Map<String, Int>) -> Int { m.length() }"),
        vec!["not yet lowered: a value of type `Map<String, Int>`"]
    );
    assert_eq!(
        refused("fn f(s: Set<Int>) -> Int { s.length() }"),
        vec!["not yet lowered: a value of type `Set<Int>`"]
    );
}

#[test]
fn a_generic_declaration_names_itself_once_and_its_uses_after() {
    assert_eq!(
        refused("fn id<T>(x: T) -> T { x }"),
        vec![
            "not yet lowered: a generic function",
            "not yet lowered: a value of type `T`",
            "not yet lowered: a value of type `T`",
        ]
    );
}

#[test]
fn an_async_declaration_is_a_gap_rather_than_a_refusal() {
    assert_eq!(
        refused("async fn g() -> Int { 1 }"),
        vec!["not yet lowered: an `async fn`"]
    );
}

#[test]
fn a_trait_method_s_default_body_is_named_where_it_is_written() {
    // A default body belongs to the trait, and the checker checks it once
    // there rather than once per conformance — so there is no per-type
    // declaration to read a boundary off.
    assert_eq!(
        refused(
            "trait T { fn a(self) -> Int\n  fn b(self) -> Int { 0 } }\n\
             struct P { x: Int }\n\
             impl T for P { fn a(self) -> Int { self.x } }\n\
             fn f(p: P) -> Int { p.b() }"
        ),
        vec!["not yet lowered: `T.b`, a trait method's default body"]
    );
}

#[test]
fn an_unsettled_type_is_a_compile_error_rather_than_a_gap() {
    // A `Ty::Unknown` is the checker declining, and a program the checker
    // declined about should not have reached a backend at all. That is not
    // the same thing as erasure, which is a type the program *wrote*.
    let items = refused("use unknownhost.thing\nfn f() -> Int { thing() }");
    assert!(
        items.iter().any(|item| item.contains("never settled")),
        "{items:?}"
    );
}
