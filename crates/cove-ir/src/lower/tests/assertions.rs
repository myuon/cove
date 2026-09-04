//! `assert` and `assertEqual`, and the slice an entry lowers.
//!
//! The two are together because both are about what a lowering is *for*: an
//! assertion is a construct the language has and the machine does not, and a
//! slice is the set of declarations one entry needs. Neither is a fact the
//! IR carries.

use super::{listing, refused, sliced, sliced_to};

/// `assert` is a branch over the two cases of the answer it declares.
///
/// The failing arm is one interned string and the record of where it
/// failed, because the message names the condition and nothing else.
/// Nothing here is a call: `Ok(())` and `Err(Error(...))` are a case and an
/// initializer, both written where the value is.
#[test]
fn an_assertion_is_a_branch_over_the_two_cases_it_answers() {
    assert_eq!(
        listing("fn f(n: Int) -> Result<Unit, Error> { assert(n > 0) }", "f"),
        "\
fn0 m.f(Int) -> Result
  frame 11: s0!:int s1:int s2:unit s3:ref s4:int s5:unit s6:ref s7:bool s8:unit s9:ref s10:ref
  local n -> s0:Int [0, 18)
     0  gt.int.imm s7:bool s0:int 0
     1  branch-false s7:bool 7
     2  unit s8:unit
     3  int s4:int 0
     4  clear s6:ref <ref>
     5  copy s5:unit s8:unit Unit
     6  jump 15
     7  str s9:ref \"assertion failed: `n > 0`\"
     8  assert.failed s9:ref
     9  copy s10:ref s9:ref String
    10  clear s9:ref String
    11  int s4:int 1
    12  clear s5:unit Unit
    13  copy s6:ref s10:ref Error
    14  clear s10:ref Error
    15  copy s1:int s4:int Result
    16  clear s4:int Result
    17  return s1:int Result
"
    );
}

/// The condition's own source text, in the words the test was written in.
///
/// That is the whole reason these are builtins rather than a library, and
/// it is why the lowering reads the [`SourceMap`](cove_diag::SourceMap): the
/// text is a constant of the program, interned where the call is lowered,
/// so the machine never looks a span up.
#[test]
fn the_message_quotes_the_argument_as_the_source_wrote_it() {
    let listed = listing(
        "fn f(items: Array<Int>) -> Result<Unit, Error> { assert(items.length() == 3) }",
        "f",
    );
    assert!(
        listed.contains("\"assertion failed: `items.length() == 3`\""),
        "{listed}"
    );
}

/// `assertEqual` compares the way `==` does and reports both values, which
/// is the one interpolation the language already has.
#[test]
fn an_equality_assertion_renders_both_values_into_its_message() {
    let listed = listing(
        "fn f(n: Int) -> Result<Unit, Error> { assertEqual(n, 3) }",
        "f",
    );
    assert!(listed.contains("eq.int"), "{listed}");
    assert!(
        listed.contains("\"assertion failed: `n` is `\""),
        "{listed}"
    );
    assert!(listed.contains("\"`, expected `\""), "{listed}");
    assert!(listed.contains("String.interpolate"), "{listed}");
}

/// A value the instruction set cannot compare in one step is walked, which
/// is what `==` on one already does.
#[test]
fn an_equality_assertion_on_a_struct_walks_it() {
    let listed = listing(
        "struct P { x: Int }\nfn f(a: P, b: P) -> Result<Unit, Error> { assertEqual(a, b) }",
        "f",
    );
    assert!(listed.contains("Any.equals"), "{listed}");
}

/// A declaration of the package wins over the shared table's name, exactly
/// as it does on the interpreter.
#[test]
fn a_package_that_declares_its_own_assert_gets_its_own() {
    let listed = listing(
        "fn assert(c: Bool) -> Result<Unit, Error> { Ok(()) }\n\
         fn f() -> Result<Unit, Error> { assert(true) }",
        "f",
    );
    assert!(listed.contains("call"), "{listed}");
    assert!(!listed.contains("assertion failed"), "{listed}");
}

// ---- the slice ----------------------------------------------------------

/// A gap in a declaration the entry does not reach is not this entry's
/// problem.
///
/// The whole-package lowering still reports it, because a whole-package
/// listing means everything the package declares is part of it.
#[test]
fn a_gap_the_entry_does_not_reach_does_not_stop_it() {
    // `bump` used as a value is a gap in `wide`'s body: a function type
    // drops `var`, so a call through the value would copy a word into a
    // parameter the callee reads as an address.
    let source = "fn bump(var n: Int) { n = n + 1 }\n\
                  fn wide() -> Int {\n  let g = bump\n  0\n}\n\
                  fn helper() -> Int { 1 }\n\
                  fn main() -> Int { helper() }";
    assert_eq!(
        refused(source),
        ["not yet lowered: `bump`, which takes a `var` parameter, used as a function value"]
    );
    assert_eq!(
        sliced(source, "main", "main"),
        "\
fn2 m.main() -> Int
  frame 2: s0:int s1:int
     0  call s1:int m.helper () Int
     1  copy s0:int s1:int Int
     2  return s0:int Int
"
    );
}

/// What the slice leaves out is a stub, and a stub is nameable by nothing:
/// it is not an entry point and no call reaches it.
#[test]
fn what_the_slice_leaves_out_is_a_stub() {
    let source = "fn unused() -> Int { 1 }\nfn main() -> Int { 2 }";
    assert_eq!(
        sliced(source, "main", "unused"),
        "\
fn1 m.unused() -> Unit
  frame 1: s0:unit
     0  return s0:unit Unit
"
    );
}

/// **A declaration a slice does not reach is a stub, and a declaration it
/// does reach is not.**
///
/// `Function::is_stub` is read from `cove-cli`'s debugger, resolving a
/// breakpoint or a stack frame against a program it did not lower itself. It
/// has to tell a stand-in from a lowered body without reading the shape
/// `lower::stub` happens to leave — one instruction, at the declaration's own
/// span, no parameters, no locals — because that shape is an accident of how
/// a stub is built, not a promise about what a lowered body can never look
/// like. This pins the fact the field is supposed to carry, on the case it
/// was added for: `unused` is left out of `main`'s slice and gets the stub
/// `lower::stub` builds, `used` is what `main` calls and gets `used`'s own
/// lowered body, and `main` itself — the root the slice is built from — is
/// unreached by nothing and is a real body too.
#[test]
fn a_declaration_the_slice_does_not_reach_is_a_stub_and_one_it_does_is_not() {
    let source = "fn unused() -> Int { 1 }\nfn used() -> Int { 2 }\nfn main() -> Int { used() }";
    let (sources, checked) = super::checked(source);
    let program = crate::lower_roots(
        &checked,
        &sources,
        &cove_schema::HostSchemas::new(),
        &[("m", "main")],
    )
    .expect("the roots' program lowers");

    let is_stub = |name: &str| {
        program
            .functions
            .iter()
            .find(|f| &*f.module == "m" && &*f.name == name)
            .unwrap_or_else(|| panic!("`{name}` is in the program"))
            .is_stub()
    };

    assert!(is_stub("unused"), "a declaration nothing calls stands in");
    assert!(!is_stub("main"), "the root itself has a real body");
    assert!(
        !is_stub("used"),
        "what the root reaches has a real body too"
    );
}

/// A declaration written where a *value* goes is not a call, so the
/// checker's call graph records no edge to it. The slice learns it from the
/// lowering instead, and the next pass has it.
#[test]
fn a_declaration_used_as_a_function_value_is_pulled_into_the_slice() {
    let source = "fn double(n: Int) -> Int { n * 2 }\n\
                  fn main(xs: Array<Int>) -> Array<Int> { xs.map(double) }";
    assert_eq!(
        sliced(source, "main", "double"),
        "\
fn0 m.double(Int) -> Int
  frame 3: s0!:int s1:int s2:int
  local n -> s0:Int [0, 3)
     0  mul.int.imm s2:int s0:int 2
     1  copy s1:int s2:int Int
     2  return s1:int Int
"
    );
}

/// A conformance a `dyn` dispatch picks is named by no call site either:
/// which one runs is a fact about the value. Every one of them is in the
/// slice, or the `Switch` this builds would have an arm that traps.
#[test]
fn every_conformance_a_dyn_dispatch_can_reach_is_in_the_slice() {
    let source = "trait D { fn shown(self) -> String }\n\
                  struct P { x: Int }\n\
                  struct Q { y: Int }\n\
                  impl D for P { fn shown(self) -> String { \"p\" } }\n\
                  impl D for Q { fn shown(self) -> String { \"q\" } }\n\
                  fn main(d: dyn D) -> String { d.shown() }";
    for conformance in ["P.shown", "Q.shown"] {
        let listed = sliced(source, "main", conformance);
        assert!(listed.contains("str "), "{conformance}: {listed}");
    }
}

/// Several roots slice to the union of what they reach, and to nothing else.
///
/// This is what a command with more than one thing to run asks for: the
/// entries it selected, together, without it having to know that `left` and
/// `right` share `common` or that `wide` is reached by neither.
#[test]
fn a_slice_over_several_roots_is_the_union_of_what_they_reach() {
    let source = "fn wide<T>(x: T) -> T { x }\n\
                  fn common() -> Int { 1 }\n\
                  fn left() -> Int { common() }\n\
                  fn right() -> Int { common() + 1 }";
    for reached in ["left", "right", "common"] {
        let listed = sliced_to(source, &["left", "right"], reached);
        assert!(!listed.contains("-> Unit"), "{reached}: {listed}");
    }
    assert_eq!(
        sliced_to(source, &["left", "right"], "wide"),
        "\
fn3 m.wide() -> Unit
  frame 1: s0:unit
     0  return s0:unit Unit
"
    );
}

/// A root that names nothing this package declares contributes nothing, and
/// is not an error here.
///
/// What a name denotes is the checker's question, and the caller has already
/// asked it: `run_entry` says "this package does not declare `m.f`" about
/// the program that was going to run, which is a better answer than a
/// lowering could give.
#[test]
fn a_root_that_names_nothing_contributes_nothing() {
    let source = "fn main() -> Int { 1 }";
    assert_eq!(
        sliced_to(source, &["main", "nowhere"], "main"),
        "\
fn0 m.main() -> Int
  frame 2: s0:int s1:int
     0  int s1:int 1
     1  copy s0:int s1:int Int
     2  return s0:int Int
"
    );
}
