//! Branches and loops, which are jumps with a patched target and nothing
//! else: there are no basic blocks here and no block arguments.

use super::listing;

#[test]
fn and_jumps_over_the_right_hand_side() {
    // The shape is the whole point: the answer slot is written from the left
    // operand, and the branch skips the instruction that would evaluate the
    // right one. There is no `And` instruction and adding one would be
    // adding an operator that has already run what it must not run.
    assert_eq!(
        listing("fn both(a: Bool, b: Bool) -> Bool { a && b }", "both"),
        "\
fn0 m.both(2) -> bool
  frame 4: s0!:bool s1!:bool s2:bool s3:bool
     0  move s3:bool s0:bool
     1  branch-false s3:bool 3
     2  move s3:bool s1:bool
     3  move s2:bool s3:bool
     4  return s2:bool
"
    );
}

#[test]
fn or_falls_into_the_right_hand_side_and_jumps_over_it_when_the_left_settled_it() {
    // One conditional branch covers both polarities: the false case falls
    // through to the right-hand side and the true case jumps past it.
    assert_eq!(
        listing("fn either(a: Bool, b: Bool) -> Bool { a || b }", "either"),
        "\
fn0 m.either(2) -> bool
  frame 4: s0!:bool s1!:bool s2:bool s3:bool
     0  move s3:bool s0:bool
     1  branch-false s3:bool 3
     2  jump 4
     3  move s3:bool s1:bool
     4  move s2:bool s3:bool
     5  return s2:bool
"
    );
}

#[test]
fn a_short_circuit_runs_the_right_hand_side_for_its_effects_too() {
    // The right-hand side of an `&&` is a call here, so what the branch
    // skips is the call itself rather than a word being copied.
    assert_eq!(
        listing(
            "fn ready() -> Bool { true }\nfn go(a: Bool) -> Bool { a && ready() }",
            "go"
        ),
        "\
fn0 m.go(1) -> bool
  frame 4: s0!:bool s1:bool s2:bool s3:bool
     0  move s2:bool s0:bool
     1  branch-false s2:bool 4
     2  call s3:bool m.ready ()
     3  move s2:bool s3:bool
     4  move s1:bool s2:bool
     5  return s1:bool
"
    );
}

#[test]
fn an_if_with_both_arms_assembles_one_answer() {
    assert_eq!(
        listing("fn pick(c: Bool) -> Int { if c { 1 } else { 2 } }", "pick"),
        "\
fn0 m.pick(1) -> int
  frame 5: s0!:bool s1:int s2:int s3:int s4:int
     0  branch-false s0:bool 4
     1  int s3:int 1
     2  move s2:int s3:int
     3  jump 7
     4  int s4:int 2
     5  move s3:int s4:int
     6  move s2:int s3:int
     7  move s1:int s2:int
     8  return s1:int
"
    );
}

#[test]
fn an_if_with_no_else_answers_unit_before_it_branches() {
    // Its answer is the same whichever way it goes, so it is written once
    // rather than on both paths — and in statement position, where nothing
    // reads it, it is not written at all.
    assert_eq!(
        listing(
            "fn guard(n: Int) -> Int {\n  if n < 0 { return 0 }\n  n\n}",
            "guard"
        ),
        "\
fn0 m.guard(1) -> int
  frame 5: s0!:int s1:int s2:int s3:bool s4:unit
     0  int s2:int 0
     1  lt.int s3:bool s0:int s2:int
     2  branch-false s3:bool 5
     3  int s2:int 0
     4  return s2:int
     5  move s1:int s0:int
     6  return s1:int
"
    );
}

#[test]
fn an_if_with_no_else_in_value_position_writes_its_unit_once() {
    assert_eq!(
        listing(
            "fn maybe(c: Bool) {\n  let done = if c { }\n  done\n}",
            "maybe"
        ),
        "\
fn0 m.maybe(1) -> unit
  frame 3: s0!:bool s1:unit s2:unit
     0  unit s2:unit
     1  branch-false s0:bool 2
     2  move s1:unit s2:unit
     3  return s1:unit
"
    );
}

#[test]
fn a_while_re_decides_its_condition_and_break_and_continue_are_jumps() {
    // `continue` goes to the condition rather than to the body, because
    // whether there is another turn is a question the loop has to ask again.
    // `break` leaves a jump behind that the loop patches once it knows where
    // its end is.
    assert_eq!(
        listing(
            "fn tick(n: Int) -> Int {\n  var total = 0\n  var i = 0\n  while i < n {\n    i += 1\n    if i == 3 { continue }\n    if i > 5 { break }\n    total += i\n  }\n  total\n}",
            "tick"
        ),
        "\
fn0 m.tick(1) -> int
  frame 7: s0!:int s1:int s2:int s3:int s4:bool s5:int s6:unit
     0  int s2:int 0
     1  int s3:int 0
     2  lt.int s4:bool s3:int s0:int
     3  branch-false s4:bool 16
     4  int s5:int 1
     5  add.int s3:int s3:int s5:int
     6  int s5:int 3
     7  eq.int s4:bool s3:int s5:int
     8  branch-false s4:bool 10
     9  jump 2
    10  int s5:int 5
    11  gt.int s4:bool s3:int s5:int
    12  branch-false s4:bool 14
    13  jump 16
    14  add.int s2:int s2:int s3:int
    15  jump 2
    16  move s1:int s2:int
    17  return s1:int
"
    );
}

#[test]
fn a_nested_loop_belongs_to_the_break_that_is_innermost() {
    assert_eq!(
        listing(
            "fn nested(n: Int) -> Int {\n  var i = 0\n  while i < n {\n    while true { break }\n    i += 1\n  }\n  i\n}",
            "nested"
        ),
        "\
fn0 m.nested(1) -> int
  frame 6: s0!:int s1:int s2:int s3:bool s4:unit s5:int
     0  int s2:int 0
     1  lt.int s3:bool s2:int s0:int
     2  branch-false s3:bool 10
     3  bool s3:bool true
     4  branch-false s3:bool 7
     5  jump 7
     6  jump 3
     7  int s5:int 1
     8  add.int s2:int s2:int s5:int
     9  jump 1
    10  move s1:int s2:int
    11  return s1:int
"
    );
}

#[test]
fn a_return_leaves_and_the_trailing_return_is_still_emitted() {
    // The last instruction is unreachable, and it is here on purpose. Every
    // forward jump this lowering patches lands on an instruction, and a
    // branch whose destination is "after everything" needs something to be
    // after. One dead word buys that, and tracking which pending patches
    // point past the end would cost more than the word.
    assert_eq!(
        listing("fn one() -> Int { return 1 }", "one"),
        "\
fn0 m.one(0) -> int
  frame 3: s0:int s1:int s2:unit
     0  int s1:int 1
     1  return s1:int
     2  return s0:int
"
    );
}

#[test]
fn a_bare_return_answers_the_unit_the_frame_was_zeroed_with() {
    assert_eq!(
        listing("fn stop(c: Bool) {\n  if c { return }\n}", "stop"),
        "\
fn0 m.stop(1) -> unit
  frame 4: s0!:bool s1:unit s2:unit s3:unit
     0  unit s2:unit
     1  branch-false s0:bool 3
     2  return s1:unit
     3  move s1:unit s2:unit
     4  return s1:unit
"
    );
}
