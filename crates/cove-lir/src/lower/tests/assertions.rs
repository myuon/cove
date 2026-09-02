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
        listing(
            "fn f(n: Int) -> Result<Unit, Error> { assert(n > 0) }",
            "f"
        ),
        "\
fn0 m.f(Int) -> Result
  frame 12: s0!:int s1:int s2:unit s3:ref s4:int s5:unit s6:ref s7:int s8:bool s9:unit s10:ref s11:ref
     0  int s7:int 0
     1  gt.int s8:bool s0:int s7:int
     2  branch-false s8:bool 8
     3  unit s9:unit
     4  int s4:int 0
     5  clear s6:ref <ref>
     6  copy s5:unit s9:unit Unit
     7  jump 16
     8  str s10:ref \"assertion failed: `n > 0`\"
     9  assert.failed s10:ref
    10  copy s11:ref s10:ref String
    11  clear s10:ref String
    12  int s4:int 1
    13  clear s5:unit Unit
    14  copy s6:ref s11:ref Error
    15  clear s11:ref Error
    16  copy s1:int s4:int Result
    17  clear s4:int Result
    18  return s1:int
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
    let source = "fn wide<T>(x: T) -> T { x }\n\
                  fn helper() -> Int { 1 }\n\
                  fn main() -> Int { helper() }";
    assert_eq!(
        refused(source),
        [
            "not yet lowered: a generic function",
            "not yet lowered: a value of type `T`",
            "not yet lowered: a value of type `T`",
        ]
    );
    assert_eq!(
        sliced(source, "main", "main"),
        "\
fn1 m.main() -> Int
  frame 2: s0:int s1:int
     0  call s1:int m.helper ()
     1  copy s0:int s1:int Int
     2  return s0:int
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
     0  return s0:unit
"
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
  frame 4: s0!:int s1:int s2:int s3:int
     0  int s2:int 2
     1  mul.int s3:int s0:int s2:int
     2  copy s1:int s3:int Int
     3  return s1:int
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
     0  return s0:unit
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
     2  return s0:int
"
    );
}
