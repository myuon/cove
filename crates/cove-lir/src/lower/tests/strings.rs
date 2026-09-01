//! Strings: a literal, and the interpolation that turns words into text.

use super::{checked, listing};
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn a_literal_is_one_instruction_and_one_object() {
    // The object is allocated on first use and shared afterwards, so a
    // literal costs one instruction here however many times it is reached.
    assert_eq!(
        listing("fn greet() -> String { \"hi\" }", "greet"),
        "\
fn0 m.greet(0) -> ref
  frame 2: s0:ref s1:ref
     0  str s1:ref \"hi\"
     1  move s0:ref s1:ref
     2  clear s1:ref
     3  return s0:ref
"
    );
}

#[test]
fn an_interpolation_is_one_call_over_every_piece() {
    // The runs of literal text are operands too, so the whole literal is one
    // builtin rather than a conversion per operand and a join after them:
    // the machine reads each operand's kind out of the frame, and the
    // lowering has nothing to add that the slot table does not already say.
    //
    // Every piece is cleared the moment the join has read it. Without that
    // the frame would hold two dead strings for as long as it ran, and a
    // static reference map has no way to say they died.
    assert_eq!(
        listing("fn greet(n: Int) -> String { \"a{n}b\" }", "greet"),
        "\
fn0 m.greet(1) -> ref
  frame 5: s0!:int s1:ref s2:ref s3:ref s4:ref
     0  str s2:ref \"a\"
     1  str s3:ref \"b\"
     2  call-builtin s4:ref String.interpolate (s2:ref s0:int s3:ref)
     3  clear s3:ref
     4  clear s2:ref
     5  move s1:ref s4:ref
     6  clear s4:ref
     7  return s1:ref
"
    );
}

#[test]
fn an_empty_run_of_text_is_not_a_piece() {
    // The parser leaves an empty run wherever an interpolation sits at an
    // end of the literal, and joining one would be an allocation and an
    // argument per `"{x}"` in the program. A `String` operand is a piece
    // like any other: rendering one answers the same text, so there is
    // nothing for the lowering to leave out.
    assert_eq!(
        listing("fn shout(who: String) -> String { \"{who}!\" }", "shout"),
        "\
fn0 m.shout(1) -> ref
  frame 4: s0!:ref s1:ref s2:ref s3:ref
     0  str s2:ref \"!\"
     1  call-builtin s3:ref String.interpolate (s0:ref s2:ref)
     2  clear s2:ref
     3  move s1:ref s3:ref
     4  clear s3:ref
     5  return s1:ref
"
    );
}

#[test]
fn a_string_built_in_a_loop_is_cleared_at_the_end_of_every_turn() {
    // This is the case a static reference map cannot answer on its own. The
    // binding's slot is a root at every program counter, so without the
    // clear at the end of the scope the frame would hold every string the
    // loop ever built until it returned.
    assert_eq!(
        listing(
            "fn rows(n: Int) {\n  var i = 0\n  while i < n {\n    let label = \"row {i}\"\n    i += 1\n  }\n}",
            "rows"
        ),
        "\
fn0 m.rows(1) -> unit
  frame 8: s0!:int s1:unit s2:int s3:bool s4:ref s5:ref s6:int s7:unit
     0  int s2:int 0
     1  lt.int s3:bool s2:int s0:int
     2  branch-false s3:bool 10
     3  str s4:ref \"row \"
     4  call-builtin s5:ref String.interpolate (s4:ref s2:int)
     5  clear s4:ref
     6  int s6:int 1
     7  add.int s2:int s2:int s6:int
     8  clear s5:ref
     9  jump 1
    10  unit s7:unit
    11  move s1:unit s7:unit
    12  return s1:unit
"
    );
}

#[test]
fn one_builtin_covers_every_kind_of_operand() {
    // A builtin is named rather than numbered because the set of them
    // belongs to the language reference rather than to the IR: adding one is
    // a change in the runtime and not a renumbering here. There is one of
    // them for interpolation however many kinds of word a program puts
    // between braces, because the kind is in the frame and the machine reads
    // it there.
    let program = lower(&checked(
        "fn all(a: Int, b: Float, c: Bool, d: Duration, e: String) -> String {\n\
           \"{a}{b}{c}{d}{e}{a}\"\n\
         }",
    ))
    .expect("the program lowers");
    let named: Vec<(String, String, Repr)> = program
        .builtins
        .iter()
        .map(|builtin| {
            (
                builtin.receiver.to_string(),
                builtin.operation.to_string(),
                builtin.result,
            )
        })
        .collect();
    assert_eq!(
        named,
        vec![("String".to_string(), "interpolate".to_string(), Repr::Ref)]
    );
}

#[test]
fn a_string_the_program_writes_twice_is_one_entry() {
    let program = lower(&checked(
        "fn a() -> String { \"same\" }\nfn b() -> String { \"same\" }",
    ))
    .expect("the program lowers");
    assert_eq!(program.strings.len(), 1);
    assert_eq!(&*program.strings[0], "same");
}
