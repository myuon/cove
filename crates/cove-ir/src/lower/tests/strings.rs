//! String literals, and the interpolations in them.

use super::listing;

/// A string literal in a loop allocates once for the run, not once per
/// turn.
#[test]
fn a_literal_is_one_instruction_and_one_object_for_the_run() {
    assert_eq!(
        listing("fn hello() -> String { \"hello\" }", "hello"),
        "\
fn @m.hello() -> String
  frame 1: s0:ref
     0  str s0:ref \"hello\"
     1  return s0:String
"
    );
}

/// What `{x}` puts in a string is a rule of the language, not an
/// instruction: the whole literal becomes one call, with the runs of
/// literal text as operands of their own. An empty run is left out.
#[test]
fn an_interpolation_is_one_builtin_over_the_pieces() {
    assert_eq!(
        listing(
            "fn greet(name: String) -> String { \"hi {name}!\" }",
            "greet"
        ),
        "\
fn @m.greet(String) -> String
  frame 4: s0!:ref s1:ref s2:ref s3:ref
  local name -> s0:String [0, 4)
     0  str s2:ref \"hi \"
     1  str s3:ref \"!\"
     2  call-builtin s1:String String.interpolate (s2:String s0:String s3:String)
     3  return s1:String
"
    );
}

/// An argument carries the layout of the location it names, so a `Point`
/// crosses into an interpolation as the two words it already is.
///
/// It used to be boxed: a builtin was handed slot numbers and nothing else,
/// so an operand wider than a word had to carry its own description. That
/// cost an allocation per interpolated struct on a path the predecessor did
/// not allocate on, and the answer was `1` rather than `Point(x: 1, y: 2)`
/// wherever the box was skipped.
#[test]
fn an_inline_value_crosses_into_an_interpolation_where_it_sits() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn show(p: Point) -> String { \"p={p}\" }",
            "show"
        ),
        "\
fn @m.show(m.Point) -> String
  frame 4: s0!:int s1!:int s2:ref s3:ref
  local p -> s0..s1:m.Point [0, 3)
     0  str s3:ref \"p=\"
     1  call-builtin s2:String String.interpolate (s3:String s0..s1:m.Point)
     2  return s2:String
"
    );
}

#[test]
fn two_strings_compare_by_their_bytes() {
    assert_eq!(
        listing("fn same(a: String, b: String) -> Bool { a == b }", "same"),
        "\
fn @m.same(String String) -> Bool
  frame 3: s0!:ref s1!:ref s2:bool
  local a -> s0:String [0, 2)
  local b -> s1:String [0, 2)
     0  eq.str s2:bool s0:ref s1:ref
     1  return s2:Bool
"
    );
}

/// [ADR 0045](../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md)
/// places every entry of `Program::strings` in the heap once, so what makes
/// two occurrences of the same literal one object is the lowering
/// interning them into one entry before that ever runs — confirmed here
/// rather than assumed.
#[test]
fn two_occurrences_of_the_same_text_share_one_string_id() {
    let (sources, checked) = super::checked("fn twice() -> Bool { \"dup\" == \"dup\" }");
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    assert_eq!(
        program.strings.len(),
        1,
        "one text, written twice, is one entry: {:?}",
        program.strings
    );
}

/// An unmentioned literal still costs an entry: the lowering does not prune
/// the pool to what a particular run reaches, because it cannot know that —
/// and [ADR 0045] places every entry regardless. See
/// `crates/cove-runtime/src/vm/exec.rs`'s `Machine::place_literals`.
///
/// [ADR 0045]: ../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md
#[test]
fn a_literal_no_instruction_loads_is_still_in_the_pool() {
    let (sources, checked) = super::checked(
        "fn pick(flag: Bool) -> String { if flag { \"used\" } else { \"also used\" } }",
    );
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    // Both arms are reachable from `pick` and both are in the pool — the
    // interesting case, `\"also used\"`, is the one a run that always passes
    // `flag: true` would never load, and the pool holds it anyway.
    assert!(program.strings.iter().any(|s| &**s == "used"));
    assert!(program.strings.iter().any(|s| &**s == "also used"));
}
