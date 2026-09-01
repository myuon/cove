//! What stops a lowering, and what does not.
//!
//! There is no `Unsupported` here and no admission predicate: the target is
//! that **every valid checked program lowers**, so a construct this lowering
//! meets and cannot emit code for is a hole in this crate rather than a
//! program the backend declines. Every case below names one, and each is
//! scheduled to be removed by a later task.

use super::refused;

#[test]
fn a_sort_in_the_ir_is_the_one_sequence_walk_still_named() {
    // `map`, `filter` and `fold` are loops now. `sorted` is not, because a
    // stable merge under a Cove comparison is a second data structure and a
    // merge step in the IR rather than a counter and a call. The operation is
    // what is named, not the lambda: the message is what says where the next
    // piece of work is.
    assert_eq!(
        refused("fn f(xs: Array<Int>) -> Array<Int> { xs.sorted(fn(a, b) { a < b }) }"),
        vec!["not yet lowered: `Array.sorted`"]
    );
}

#[test]
fn a_shared_stops_at_the_boundary_rather_than_at_its_lock() {
    // `Shared.lock` is not a call through a closure that happens to be
    // missing, and this is the sentence that says so: `Shared` has no layout
    // in this backend at all, so a declaration that mentions one has no
    // boundary and its body is never walked. Reaching the `lock` would need
    // the family represented first — and then the mutual exclusion, and then
    // a closure whose parameter is `var`, which a function type does not
    // carry.
    assert_eq!(
        refused(
            "fn f(cell: Shared<Int>) -> Int { cell.lock(fn(var value) { value = value + 1\n 0 }) }"
        ),
        vec!["not yet lowered: a value of type `Shared<Int>`"]
    );
}

#[test]
fn a_function_value_with_a_var_parameter_names_the_parameter_rather_than_the_lambda() {
    // ADR 0032 fixes a closure's parameter list, and a function type names
    // what a call passes but not whether it aliases. So a `var` on a lambda
    // is work this lowering has not done rather than a shape it refuses.
    let items = refused(
        "fn f() -> Int {\n  let g = fn(var n: Int) { n = 1\n 0 }\n  var x = 0\n  g(var x)\n}",
    );
    assert!(
        items
            .iter()
            .any(|item| item == "not yet lowered: a function value with a `var` parameter"),
        "{items:?}"
    );
}

#[test]
fn a_closure_capturing_a_var_parameter_names_the_binding() {
    // The oracle captures the *value* behind a `var` parameter, and the word
    // this frame holds is an address — so the capture would be a load, of a
    // layout the frame does not record for an `Addr` slot.
    assert_eq!(
        refused("fn f(var n: Int) -> Int {\n  let g = fn() { n + 1 }\n  g()\n}"),
        vec!["not yet lowered: a function value capturing `n`, a `var` parameter"]
    );
}

#[test]
fn an_async_function_value_is_a_gap_of_its_own() {
    assert_eq!(
        refused("fn f() -> Int {\n  let g = async fn() { 1 }\n  0\n}"),
        vec!["not yet lowered: an `async` function value"]
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

#[test]
fn a_declaration_taking_a_var_parameter_cannot_be_used_as_a_function_value() {
    // A function type says what a call passes and not whether it aliases —
    // `Signature::as_value` drops `var`, so `fn bump(var n: Int)` reads as
    // `fn(Int)`. A call through the value would copy an `Int` into a
    // parameter the callee reads as an address, so the two disagree about
    // what a word is.
    assert_eq!(
        refused("fn bump(var n: Int) { n = n + 1 }\nfn f() -> Int {\n  let g = bump\n  0\n}"),
        vec!["not yet lowered: `bump`, which takes a `var` parameter, used as a function value"]
    );
}
