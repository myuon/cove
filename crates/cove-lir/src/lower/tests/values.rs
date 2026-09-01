//! Values, operators and bindings.

use super::listing;

#[test]
fn a_parameter_is_the_slot_a_caller_writes() {
    // Slot 0 is the parameter, slot 1 is the answer, and everything after is
    // a temporary. Nothing is permuted into type groups on the way in.
    assert_eq!(
        listing("fn double(n: Int) -> Int { n * 2 }", "double"),
        "\
fn0 m.double(1) -> int
  frame 4: s0!:int s1:int s2:int s3:int
     0  int s2:int 2
     1  mul.int s3:int s0:int s2:int
     2  move s1:int s3:int
     3  return s1:int
"
    );
}

#[test]
fn arithmetic_and_comparison_read_the_operands_kind() {
    assert_eq!(
        listing(
            "fn ordered(a: Int, b: Int) -> Bool { a - 1 <= b }",
            "ordered"
        ),
        "\
fn0 m.ordered(2) -> bool
  frame 6: s0!:int s1!:int s2:bool s3:int s4:int s5:bool
     0  int s3:int 1
     1  sub.int s4:int s0:int s3:int
     2  le.int s5:bool s4:int s1:int
     3  move s2:bool s5:bool
     4  return s2:bool
"
    );
}

#[test]
fn a_float_keeps_its_bits_and_reads_as_a_float() {
    assert_eq!(
        listing("fn half(x: Float) -> Float { -x / 2.0 }", "half"),
        "\
fn0 m.half(1) -> float
  frame 5: s0!:float s1:float s2:float s3:float s4:float
     0  neg.float s2:float s0:float
     1  float s3:float 2
     2  div.float s4:float s2:float s3:float
     3  move s1:float s4:float
     4  return s1:float
"
    );
}

#[test]
fn a_duration_is_nanoseconds_and_adds_like_an_integer() {
    // The word is a `Duration` and the arithmetic is `Num::Int`. Only the
    // boundary cares which name the slot's kind gives it, which is why one
    // instruction covers both.
    assert_eq!(
        listing("fn wait() -> Duration { 5ms + 3ms }", "wait"),
        "\
fn0 m.wait(0) -> duration
  frame 4: s0:duration s1:duration s2:duration s3:duration
     0  int s1:duration 5000000
     1  int s2:duration 3000000
     2  add.int s3:duration s1:duration s2:duration
     3  move s0:duration s3:duration
     4  return s0:duration
"
    );
}

#[test]
fn a_duration_compares_as_an_integer_too() {
    assert_eq!(
        listing("fn slow(d: Duration) -> Bool { d > 1s }", "slow"),
        "\
fn0 m.slow(1) -> bool
  frame 4: s0!:duration s1:bool s2:duration s3:bool
     0  int s2:duration 1000000000
     1  gt.int s3:bool s0:duration s2:duration
     2  move s1:bool s3:bool
     3  return s1:bool
"
    );
}

#[test]
fn not_negates_a_bool() {
    assert_eq!(
        listing("fn flip(flag: Bool) -> Bool { !flag }", "flip"),
        "\
fn0 m.flip(1) -> bool
  frame 3: s0!:bool s1:bool s2:bool
     0  not s2:bool s0:bool
     1  move s1:bool s2:bool
     2  return s1:bool
"
    );
}

#[test]
fn comparing_two_units_is_the_answer_rather_than_an_instruction() {
    // `()` compares equal to `()` and there is nothing in the word to look
    // at, so no `Compare` needs a case for it. Both sides are still
    // evaluated, because either of them may have done something.
    assert_eq!(
        listing("fn same() -> Bool { () == () }", "same"),
        "\
fn0 m.same(0) -> bool
  frame 4: s0:bool s1:unit s2:unit s3:bool
     0  unit s1:unit
     1  unit s2:unit
     2  bool s3:bool true
     3  move s0:bool s3:bool
     4  return s0:bool
"
    );
}

#[test]
fn a_var_local_is_one_slot_written_again() {
    // Reassignment is a store into the binding's own slot. A `var` local
    // needs no address and no second store: what `var` decided was whether
    // the checker allows the assignment at all.
    assert_eq!(
        listing(
            "fn count() -> Int {\n  var n = 0\n  n = n + 1\n  n += 2\n  n\n}",
            "count"
        ),
        "\
fn0 m.count(0) -> int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 0
     1  int s2:int 1
     2  add.int s3:int s1:int s2:int
     3  move s1:int s3:int
     4  int s3:int 2
     5  add.int s1:int s1:int s3:int
     6  move s0:int s1:int
     7  return s0:int
"
    );
}

#[test]
fn a_body_that_falls_off_the_end_answers_unit() {
    assert_eq!(
        listing("fn nothing() {}", "nothing"),
        "\
fn0 m.nothing(0) -> unit
  frame 1: s0:unit
     0  unit s0:unit
     1  return s0:unit
"
    );
}

#[test]
fn a_block_is_a_scope_whose_locals_die_with_it() {
    assert_eq!(
        listing(
            "fn scoped() -> Int {\n  let a = 1\n  {\n    let b = 2\n    b\n  }\n}",
            "scoped"
        ),
        "\
fn0 m.scoped(0) -> int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 1
     1  int s3:int 2
     2  move s2:int s3:int
     3  move s0:int s2:int
     4  return s0:int
"
    );
}
