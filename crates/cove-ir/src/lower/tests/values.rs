//! Values, operators and bindings.

use super::listing;

/// Slot 0 is the parameter, slot 1 is the answer, and everything after is
/// a temporary. Nothing is permuted into type groups on the way in, and a
/// one-word value is the width-one case of the model rather than a family
/// of its own.
#[test]
fn a_parameter_is_the_run_a_caller_writes() {
    assert_eq!(
        listing("fn double(n: Int) -> Int { n * 2 }", "double"),
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

/// A negated literal is still a literal, and nothing else is folded.
///
/// `-1` is parsed as a `Neg` of `1`, so without the one line in
/// `expr::int_literal` that looks through it, `n > -1` would materialise a
/// `1`, negate it into a second temporary, and compare against that: three
/// instructions where the source wrote one number. That line is the whole of
/// the constant folding in this lowering, and the second half of this listing
/// is what says so — `1 + 1` is two literals rather than one, and the left
/// one is materialised exactly as it always was. Nothing here evaluates
/// anything; it reads syntax.
#[test]
fn a_negated_literal_is_an_immediate_and_a_sum_of_two_is_not_folded() {
    assert_eq!(
        listing("fn f(n: Int) -> Bool { n > -1 && n < 1 + 1 }", "f"),
        "\
fn0 m.f(Int) -> Bool
  frame 6: s0!:int s1:bool s2:bool s3:bool s4:int s5:int
  local n -> s0:Int [0, 9)
     0  gt.int.imm s3:bool s0:int -1
     1  copy s2:bool s3:bool Bool
     2  branch-false s2:bool 7
     3  int s4:int 1
     4  add.int.imm s5:int s4:int 1
     5  lt.int s3:bool s0:int s5:int
     6  copy s2:bool s3:bool Bool
     7  copy s1:bool s2:bool Bool
     8  return s1:bool Bool
"
    );
}

/// Which numeric reading an instruction gives its operands comes from the
/// operands' own `Repr`, which is the checker's answer written down.
#[test]
fn arithmetic_and_comparison_read_the_operands_kind() {
    assert_eq!(
        listing(
            "fn ordered(a: Int, b: Int) -> Bool { a - 1 <= b }",
            "ordered"
        ),
        "\
fn0 m.ordered(Int Int) -> Bool
  frame 5: s0!:int s1!:int s2:bool s3:int s4:bool
  local a -> s0:Int [0, 4)
  local b -> s1:Int [0, 4)
     0  sub.int.imm s3:int s0:int 1
     1  le.int s4:bool s3:int s1:int
     2  copy s2:bool s4:bool Bool
     3  return s2:bool Bool
"
    );
}

#[test]
fn a_float_keeps_its_bits_and_reads_as_a_float() {
    assert_eq!(
        listing("fn half(x: Float) -> Float { -x / 2.0 }", "half"),
        "\
fn0 m.half(Float) -> Float
  frame 5: s0!:float s1:float s2:float s3:float s4:float
  local x -> s0:Float [0, 5)
     0  neg.float s2:float s0:float
     1  float s3:float 2
     2  div.float s4:float s2:float s3:float
     3  copy s1:float s4:float Float
     4  return s1:float Float
"
    );
}

/// The word is a `Duration` and the arithmetic is `Num::Int`. Only the
/// boundary cares which name the location's layout gives it, which is why
/// one instruction covers both.
#[test]
fn a_duration_is_nanoseconds_and_adds_like_an_integer() {
    assert_eq!(
        listing("fn wait() -> Duration { 5ms + 3ms }", "wait"),
        "\
fn0 m.wait() -> Duration
  frame 3: s0:duration s1:duration s2:duration
     0  int s1:duration 5000000
     1  add.int.imm s2:duration s1:duration 3000000
     2  copy s0:duration s2:duration Duration
     3  return s0:duration Duration
"
    );
}

#[test]
fn not_negates_a_bool() {
    assert_eq!(
        listing("fn flip(flag: Bool) -> Bool { !flag }", "flip"),
        "\
fn0 m.flip(Bool) -> Bool
  frame 3: s0!:bool s1:bool s2:bool
  local flag -> s0:Bool [0, 3)
     0  not s2:bool s0:bool
     1  copy s1:bool s2:bool Bool
     2  return s1:bool Bool
"
    );
}

/// `()` compares equal to `()` and there is nothing in the word to look
/// at, so no `Compare` needs a case for it. Both sides are still
/// evaluated, because either of them may have done something.
#[test]
fn comparing_two_units_is_the_answer_rather_than_an_instruction() {
    assert_eq!(
        listing("fn same() -> Bool { () == () }", "same"),
        "\
fn0 m.same() -> Bool
  frame 4: s0:bool s1:unit s2:unit s3:bool
     0  unit s1:unit
     1  unit s2:unit
     2  bool s3:bool true
     3  copy s0:bool s3:bool Bool
     4  return s0:bool Bool
"
    );
}

/// Reassignment is a copy into the binding's own location. A `var` local
/// needs no address and no second store: what `var` decided was whether
/// the checker allows the assignment at all. A compound assignment writes
/// the destination directly, because the destination is the accumulator.
#[test]
fn a_var_local_is_one_location_written_again() {
    assert_eq!(
        listing(
            "fn count() -> Int {\n  var n = 0\n  n = n + 1\n  n += 2\n  n\n}",
            "count"
        ),
        "\
fn0 m.count() -> Int
  frame 3: s0:int s1:int s2:int
  local n -> s1:Int [1, 5)
     0  int s1:int 0
     1  add.int.imm s2:int s1:int 1
     2  copy s1:int s2:int Int
     3  add.int.imm s1:int s1:int 2
     4  copy s0:int s1:int Int
     5  return s0:int Int
"
    );
}

#[test]
fn a_body_that_falls_off_the_end_answers_unit() {
    assert_eq!(
        listing("fn nothing() {}", "nothing"),
        "\
fn0 m.nothing() -> Unit
  frame 1: s0:unit
     0  unit s0:unit
     1  return s0:unit Unit
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
fn0 m.scoped() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local a -> s1:Int [1, 4)
  local b -> s3:Int [2, 3)
     0  int s1:int 1
     1  int s3:int 2
     2  copy s2:int s3:int Int
     3  copy s0:int s2:int Int
     4  return s0:int Int
"
    );
}

/// `&&` and `||` are not instructions: their meaning is that the
/// right-hand side may not run, and an instruction taking two operands has
/// already run it. One conditional branch covers both, with the condition
/// arranged to suit.
#[test]
fn short_circuiting_is_a_branch_over_the_right_hand_side() {
    assert_eq!(
        listing("fn both(a: Bool, b: Bool) -> Bool { a && b }", "both"),
        "\
fn0 m.both(Bool Bool) -> Bool
  frame 4: s0!:bool s1!:bool s2:bool s3:bool
  local a -> s0:Bool [0, 5)
  local b -> s1:Bool [0, 5)
     0  copy s3:bool s0:bool Bool
     1  branch-false s3:bool 3
     2  copy s3:bool s1:bool Bool
     3  copy s2:bool s3:bool Bool
     4  return s2:bool Bool
"
    );
}

#[test]
fn an_or_inverts_the_polarity_with_a_jump_rather_than_an_instruction() {
    assert_eq!(
        listing("fn either(a: Bool, b: Bool) -> Bool { a || b }", "either"),
        "\
fn0 m.either(Bool Bool) -> Bool
  frame 4: s0!:bool s1!:bool s2:bool s3:bool
  local a -> s0:Bool [0, 6)
  local b -> s1:Bool [0, 6)
     0  copy s3:bool s0:bool Bool
     1  branch-false s3:bool 3
     2  jump 4
     3  copy s3:bool s1:bool Bool
     4  copy s2:bool s3:bool Bool
     5  return s2:bool Bool
"
    );
}

/// A fresh temporary is dead the moment the binding is alive, so the
/// binding is the same location rather than a copy of it. That is the one
/// elision the model permits, and correctness does not depend on it: a
/// borrowed location — one a binding still in scope owns — is copied.
#[test]
fn a_binding_takes_over_the_temporary_its_initialiser_made() {
    assert_eq!(
        listing(
            "fn twice(n: Int) -> Int {\n  let a = n + 1\n  let b = a\n  a + b\n}",
            "twice"
        ),
        "\
fn0 m.twice(Int) -> Int
  frame 5: s0!:int s1:int s2:int s3:int s4:int
  local n -> s0:Int [0, 5)
  local a -> s2:Int [1, 4)
  local b -> s3:Int [2, 4)
     0  add.int.imm s2:int s0:int 1
     1  copy s3:int s2:int Int
     2  add.int s4:int s2:int s3:int
     3  copy s1:int s4:int Int
     4  return s1:int Int
"
    );
}
