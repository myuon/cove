//! What stops a lowering, and what does not.
//!
//! There is no `Unsupported` here and no admission predicate: the target is
//! that **every valid checked program lowers**, so a construct this lowering
//! meets and cannot emit code for is a hole in this crate rather than a
//! program the backend declines. Every case below names one, and each is
//! scheduled to be removed by a later task.
//!
//! Two cases left this file rather than this repository: an `async fn` and an
//! `async` function value were gaps here and are listings in
//! [`super::tasks`], which is where a construct's case belongs once there is
//! a construct rather than a refusal to describe.

use cove_schema::HostSchemas;

use super::{listing, refused};

/// A scope asks its function for a failure only where a child can make one,
/// and the case where none can is the one that used to be refused.
///
/// `crate::task::wait_for_children` returns a failing child's `Err` from the
/// function the scope was written in, whatever that function's type is — a
/// `Value::err` out of a body typed `Unit`, which the linear model cannot
/// represent, since a return copies the words `Function::returns` describes.
///
/// So the question is asked of the *children*, which is the same question
/// `cove_sema::Checker::spawned` asks. A scope over `Task<Verdict>` children
/// has no failure to pass on and lowers; the gap is left for a scope whose
/// children answer a `Result` in a function that answers none.
///
/// **That gap is unreachable from a checked program**, and deliberately:
/// `cove::type::scope_child_failure` refuses exactly that shape before the
/// lowering sees it, which is what issue #240 asked for — "enforce this
/// before lowering, so the LVM does not manufacture an `Err` in a frame whose
/// return layout cannot carry one". It is kept as the thing that would notice
/// if that rule ever stopped holding.
#[test]
fn a_scope_asks_for_a_failure_only_where_a_child_can_make_one() {
    let text = listing(
        "export fn f() {\n  scope tasks {\n    let n = 1\n  }\n}",
        "f",
    );
    assert!(text.contains("scope.enter"), "the scope lowers:\n{text}");
    assert!(
        !text.contains("branch-false"),
        "and leaving it needs no failure branch:\n{text}"
    );
}

#[test]
fn a_function_value_with_a_var_parameter_names_the_parameter_rather_than_the_lambda() {
    // ADR 0032 fixes a closure's parameter list, and a function type names
    // what a call passes but not whether it aliases. So a `var` on a lambda
    // is work this lowering has not done rather than a shape it refuses.
    //
    // `Shared.lock` admits exactly this parameter and does not weaken this:
    // there the closure never becomes a value some other call reaches
    // through, because the environment is built and consumed by the same
    // instruction sequence — and that sequence is the one that formed the
    // address it passes. Here the closure is bound to a name and called
    // through it, which is the case the refusal is about.
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

/// The same lambda written where a `lock` does not take it is still refused.
///
/// A `var` first parameter is admitted on one path — the argument of a
/// `Shared.lock` — and this is the sentence that says the admission is about
/// the *path* and not about the lambda: the identical closure, handed to
/// `Array.map`, meets the general refusal in the general refusal's words.
#[test]
fn a_var_parameter_is_admitted_by_the_lock_path_and_not_by_the_lambda() {
    let items = refused("fn f() -> Array<Int> {\n  [1].map(fn(var n: Int) { n = 1\n 0 })\n}");
    assert!(
        items
            .iter()
            .any(|item| item == "not yet lowered: a function value with a `var` parameter"),
        "{items:?}"
    );
}

/// A generic declaration is not one function, and one nothing instantiates
/// is not any.
///
/// It used to be three gaps — the declaration, and its parameter's type
/// twice. There is nothing to report now and nothing to lower: a `T` has no
/// words, so the only thing that can have words is an instantiation, and
/// nothing here asks for one.
#[test]
fn a_generic_nothing_instantiates_costs_no_function() {
    let (sources, checked) = super::checked("fn id<T>(x: T) -> T { x }");
    let program =
        crate::lower(&checked, &sources, &HostSchemas::new()).expect("the program lowers");
    assert_eq!(program.function_named("m", "id"), None);
    assert!(
        !program.functions.iter().any(|f| f.name.contains('<')),
        "nothing asked for an instantiation"
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

/// A handle is neither a scalar the machine computes with nor an address it
/// can trace, and no comparison instruction admits one.
///
/// The word is an index into the run's resource table, and whether two
/// indices being equal is two handles naming one resource is that table's
/// question rather than an instruction's. The verifier says as much, so this
/// names the work instead of emitting a `Cmp` that would be a fault.
#[test]
fn a_comparison_of_two_host_resource_handles_is_named_rather_than_emitted() {
    assert_eq!(
        refused(
            "use files\nfn f() -> Result<Bool, Error> {\n  \
             let a = files.open(\"a\")?\n  let b = files.open(\"b\")?\n  Ok(a == b)\n}"
        ),
        vec!["not yet lowered: a comparison of two host resource handles"]
    );
}

/// The converse: a run of programs over the families and the parameter
/// shapes this lowering has been taught, asserted only to *lower*.
///
/// Every other case in these tests pins a listing, and this one deliberately
/// does not. What it is for is the check `lower` makes on the way out —
/// [`crate::verify`], which panics rather than reporting, so a program whose
/// locations, jumps or argument widths this lowering got wrong fails here
/// with the fault named. One listing per line of this would say the same
/// thing at twenty times the length, and the shapes that are worth pinning
/// are pinned above.
#[test]
fn the_shapes_these_families_are_written_in_all_lower() {
    let cases = [
        "fn f(s: Set<String>) -> Array<String> { s.toArray() }",
        "fn f(s: Set<Int>) -> Bool { s.contains(1) }",
        "fn f(s: Set<Int>) -> Set<Int> { s.removed(1) }",
        "fn f(m: Map<String, Int>) -> Map<String, Int> { m.inserted(\"a\", 1) }",
        "fn f(m: Map<String, Int>) -> Map<String, Int> { m.removed(\"a\") }",
        "fn f(m: Map<String, Int>) -> Array<Int> { m.values() }",
        "fn f(m: Map<String, Int>) -> Bool { m.isEmpty() }",
        "struct P { x: Int, y: Int }\nfn f(s: Set<P>) -> Int { s.length() }",
        "struct P { x: Int, y: Int }\nfn f(m: Map<Int, P>) -> Int {\n  var n = 0\n  for e in m { n = n + e.value.x }\n  n\n}",
        "fn f(m: Map<String, Int>) -> Int {\n  for e in m { if e.value > 0 { break } }\n  0\n}",
        "fn f(xs: Vector<Int>) -> Array<Int> { xs.sorted(by: fn(a, b) { a < b }) }",
        "struct P { x: Int }\nfn f(xs: Array<P>) -> Array<P> { xs.sorted(by: fn(a, b) { a.x < b.x }) }",
        "fn total(items: String...) -> Int { items.length() }\nfn f() -> Int { total(\"a\", \"b\") }",
        "fn total(items: String...) -> Int { items.length() }\nfn f(a: Array<String>, b: Array<String>) -> Int { total(...a, ...b) }",
        "trait D { fn shown(self) -> String }\nstruct P { x: Int }\nimpl D for P { fn shown(self) -> String { \"p\" } }\nfn take(items: dyn D...) -> Int { items.length() }\nfn f(p: P) -> Int { take(p, p) }",
        "fn tag(n: Int, label: String = \"n\") -> String { label }\nfn f() -> String { tag(1) }",
        "fn tag(n: Int, label: String = \"n\") -> String { label }\nfn f() -> String { tag(1, label: \"x\") }",
        "fn tag(n: Int = 1, label: String = \"n\") -> String { label }\nfn f() -> String { tag() }",
        "fn each(xs: Array<Int>, extra: Int = xs.length()) -> Int { extra }\nfn f(a: Array<Int>) -> Int { each(a) }",
        "fn f(m: Map<String, Int>, n: Map<String, Int>) -> Bool { m == n }",
        "fn f(s: Set<Int>) -> String { \"{s}\" }",
        "fn f(cell: Shared<Array<Int>>) -> Int { cell.lock(fn(v) { v.length() }) }",
        "struct Node { cell: Option<Shared<Node>>, n: Int }\nfn f() -> Int {\n  let n = Shared(Node(cell: None, n: 1))\n  n.lock(fn(var v) { v = Node(cell: Some(n), n: 2) })\n  n.lock(fn(v) { v.n })\n}",
        "fn f(cell: Shared<Int>, other: Shared<Int>) -> Int {\n  cell.lock(fn(var a) { other.lock(fn(var b) { b = a }) })\n  0\n}",
    ];
    let mut bad = Vec::new();
    for src in cases {
        let (sources, checked) = super::checked(src);
        if let Err(items) = crate::lower(&checked, &sources, &HostSchemas::new()) {
            bad.push(format!(
                "{src}\n  -> {:?}",
                items.iter().map(|i| i.message.clone()).collect::<Vec<_>>()
            ));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
