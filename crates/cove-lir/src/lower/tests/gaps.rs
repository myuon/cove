//! What stops a lowering, and what does not.
//!
//! There is no `Unsupported` here and no admission predicate: the target is
//! that **every valid checked program lowers**, so a construct this lowering
//! meets and cannot emit code for is a hole in this crate rather than a
//! program the backend declines. Every case below names one, and each is
//! scheduled to be removed by a later task.

use cove_schema::HostSchemas;

use super::refused;

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
fn an_async_function_value_is_a_gap_of_its_own() {
    assert_eq!(
        refused("fn f() -> Int {\n  let g = async fn() { 1 }\n  0\n}"),
        vec!["not yet lowered: an `async` function value"]
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
fn an_async_declaration_is_a_gap_rather_than_a_refusal() {
    assert_eq!(
        refused("async fn g() -> Int { 1 }"),
        vec!["not yet lowered: an `async fn`"]
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

/// A trailing lambda on a host operation lowers as far as the call and stops
/// at the boundary.
///
/// `clock.timeout(500ms) { ... }` is an ordinary last argument here — that
/// part is done — but a public closure carries a body a host's callback
/// would go looking for, and only the backend that made one can run it. The
/// machine refuses to materialise one, so a call emitted here would run and
/// answer a failure the oracle does not. A wrong answer is worse than a gap.
#[test]
fn a_function_value_handed_to_a_host_operation_stops_at_the_boundary() {
    assert_eq!(
        refused("use clock\nfn f() -> Result<Int, Error> { clock.timeout(1s) { 1 } }"),
        vec!["not yet lowered: a function value passed to a host operation"]
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
