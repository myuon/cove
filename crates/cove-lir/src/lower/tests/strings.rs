//! String literals, and the interpolations in them.

use super::listing;

/// A string literal in a loop allocates once for the run, not once per
/// turn.
#[test]
fn a_literal_is_one_instruction_and_one_object_for_the_run() {
    assert_eq!(
        listing("fn hello() -> String { \"hello\" }", "hello"),
        "\
fn0 m.hello() -> String
  frame 2: s0:ref s1:ref
     0  str s1:ref \"hello\"
     1  copy s0:ref s1:ref String
     2  clear s1:ref String
     3  return s0:ref
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
fn0 m.greet(String) -> String
  frame 5: s0!:ref s1:ref s2:ref s3:ref s4:ref
     0  str s2:ref \"hi \"
     1  str s3:ref \"!\"
     2  call-builtin s4:ref String.interpolate (s2:ref s0:ref s3:ref)
     3  clear s3:ref String
     4  clear s2:ref String
     5  copy s1:ref s4:ref String
     6  clear s4:ref String
     7  return s1:ref
"
    );
}

/// A builtin is handed slot numbers and nothing else, so an operand that
/// is not one word carries its own layout in a box. That is a consequence
/// of the instruction's shape rather than of the representation, and it is
/// the one place the model costs an allocation the predecessor did not.
#[test]
fn an_inline_value_is_boxed_on_the_way_into_an_interpolation() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn show(p: Point) -> String { \"p={p}\" }",
            "show"
        ),
        "\
fn0 m.show(m.Point) -> String
  frame 6: s0!:int s1!:int s2:ref s3:ref s4:ref s5:ref
     0  str s3:ref \"p=\"
     1  box s4:ref s0:int m.Point
     2  call-builtin s5:ref String.interpolate (s3:ref s4:ref)
     3  clear s4:ref <ref>
     4  clear s3:ref String
     5  copy s2:ref s5:ref String
     6  clear s5:ref String
     7  return s2:ref
"
    );
}

#[test]
fn two_strings_compare_by_their_bytes() {
    assert_eq!(
        listing("fn same(a: String, b: String) -> Bool { a == b }", "same"),
        "\
fn0 m.same(String String) -> Bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  eq.str s3:bool s0:ref s1:ref
     1  copy s2:bool s3:bool Bool
     2  return s2:bool
"
    );
}
