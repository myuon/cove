//! Calls, which need no argument buffer: the callee's frame begins where the
//! caller's ends, so `Call` copies argument slot *i* to callee slot *i*.

use super::{checked, listing};
use crate::lower::lower;

#[test]
fn arguments_are_evaluated_in_source_order_into_slots_of_their_own() {
    assert_eq!(
        listing(
            "fn add(a: Int, b: Int) -> Int { a + b }\nfn three() -> Int { add(1, 2) }",
            "three"
        ),
        "\
fn1 m.three(0) -> int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 1
     1  int s2:int 2
     2  call s3:int m.add (s1:int s2:int)
     3  move s0:int s3:int
     4  return s0:int
"
    );
}

#[test]
fn a_call_answering_unit_still_names_a_destination() {
    // `Unit` is a value in Cove, so it takes a slot rather than being
    // absent. That is what keeps the calling convention free of a case for
    // a function that answers nothing.
    assert_eq!(
        listing("fn noop() {}\nfn twice() {\n  noop()\n  noop()\n}", "twice"),
        "\
fn1 m.twice(0) -> unit
  frame 2: s0:unit s1:unit
     0  call s1:unit m.noop ()
     1  call s1:unit m.noop ()
     2  move s0:unit s1:unit
     3  return s0:unit
"
    );
}

#[test]
fn a_function_can_call_itself() {
    assert_eq!(
        listing(
            "fn fact(n: Int) -> Int {\n  if n <= 1 { 1 } else { n * fact(n - 1) }\n}",
            "fact"
        ),
        "\
fn0 m.fact(1) -> int
  frame 7: s0!:int s1:int s2:int s3:int s4:bool s5:int s6:int
     0  int s3:int 1
     1  le.int s4:bool s0:int s3:int
     2  branch-false s4:bool 6
     3  int s3:int 1
     4  move s2:int s3:int
     5  jump 12
     6  int s5:int 1
     7  sub.int s6:int s0:int s5:int
     8  call s5:int m.fact (s6:int)
     9  mul.int s6:int s0:int s5:int
    10  move s3:int s6:int
    11  move s2:int s3:int
    12  move s1:int s2:int
    13  return s1:int
"
    );
}

#[test]
fn two_calls_of_the_same_shape_share_one_argument_list() {
    // The list is a program-wide `ArgsId` rather than something carried in
    // the instruction, so a repeated call shape costs one list.
    let program = lower(&checked(
        "fn add(a: Int, b: Int) -> Int { a + b }\n\
         fn twice(x: Int, y: Int) -> Int { add(x, y) + add(x, y) }",
    ))
    .expect("the program lowers");
    assert_eq!(program.args, vec![vec![0, 1]]);
}
