//! Control flow: `if`, `while`, `for`, `break`, `continue`, `return`.

use super::listing;

/// A branch join is two copies into one destination location, one per
/// arm — which is `docs/LINEAR_VM.md`'s fifth worked case, and is the same
/// whether the value is one word or several.
#[test]
fn an_if_with_an_else_is_two_writes_into_one_destination() {
    assert_eq!(
        listing("fn pick(c: Bool) -> Int { if c { 1 } else { 2 } }", "pick"),
        "\
fn0 m.pick(Bool) -> Int
  frame 4: s0!:bool s1:int s2:int s3:int
     0  branch-false s0:bool 4
     1  int s3:int 1
     2  copy s2:int s3:int Int
     3  jump 6
     4  int s3:int 2
     5  copy s2:int s3:int Int
     6  copy s1:int s2:int Int
     7  return s1:int
"
    );
}

/// It answers `()` whichever way it goes, so there is nothing for the
/// taken side to produce.
#[test]
fn an_if_without_an_else_writes_its_unit_once_before_the_branch() {
    assert_eq!(
        listing(
            "fn maybe(c: Bool) -> Int {\n  var n = 0\n  if c { n = 1 }\n  n\n}",
            "maybe"
        ),
        "\
fn0 m.maybe(Bool) -> Int
  frame 4: s0!:bool s1:int s2:int s3:int
     0  int s2:int 0
     1  branch-false s0:bool 4
     2  int s3:int 1
     3  copy s2:int s3:int Int
     4  copy s1:int s2:int Int
     5  return s1:int
"
    );
}

#[test]
fn a_branch_join_of_a_struct_is_two_copies_of_its_words() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn pick(c: Bool, a: Point, b: Point) -> Point { if c { a } else { b } }",
            "pick"
        ),
        "\
fn0 m.pick(Bool m.Point m.Point) -> m.Point
  frame 9: s0!:bool s1!:int s2!:int s3!:int s4!:int s5:int s6:int s7:int s8:int
     0  branch-false s0:bool 3
     1  copy s7:int s1:int m.Point
     2  jump 4
     3  copy s7:int s3:int m.Point
     4  copy s5:int s7:int m.Point
     5  return s5:int
"
    );
}

/// The condition is inside the loop, because `continue` has to re-decide
/// whether there is another turn rather than assume one.
#[test]
fn a_while_re_decides_the_condition_every_turn() {
    assert_eq!(
        listing(
            "fn count(n: Int) -> Int {\n  var t = 0\n  while t < n { t = t + 1 }\n  t\n}",
            "count"
        ),
        "\
fn0 m.count(Int) -> Int
  frame 6: s0!:int s1:int s2:int s3:bool s4:int s5:int
     0  int s2:int 0
     1  lt.int s3:bool s2:int s0:int
     2  branch-false s3:bool 7
     3  int s4:int 1
     4  add.int s5:int s2:int s4:int
     5  copy s2:int s5:int Int
     6  jump 1
     7  copy s1:int s2:int Int
     8  return s1:int
"
    );
}

#[test]
fn a_break_leaves_the_loop_and_its_jump_is_patched_at_the_end() {
    assert_eq!(
        listing(
            "fn first() -> Int {\n  var t = 0\n  while true {\n    t = t + 1\n    if t > 3 { break }\n  }\n  t\n}",
            "first"
        ),
        "\
fn0 m.first() -> Int
  frame 6: s0:int s1:int s2:bool s3:int s4:int s5:unit
     0  int s1:int 0
     1  bool s2:bool true
     2  branch-false s2:bool 11
     3  int s3:int 1
     4  add.int s4:int s1:int s3:int
     5  copy s1:int s4:int Int
     6  int s4:int 3
     7  gt.int s2:bool s1:int s4:int
     8  branch-false s2:bool 10
     9  jump 11
    10  jump 1
    11  copy s0:int s1:int Int
    12  return s0:int
"
    );
}

/// The frame ends at the `Return`, so nothing is cleared on the way out: a
/// location whose frame is gone retains nothing.
#[test]
fn a_return_leaves_without_clearing_what_the_frame_was_holding() {
    assert_eq!(
        listing(
            "fn early(n: Int) -> Int {\n  if n < 0 { return 0 }\n  n\n}",
            "early"
        ),
        "\
fn0 m.early(Int) -> Int
  frame 5: s0!:int s1:int s2:int s3:bool s4:unit
     0  int s2:int 0
     1  lt.int s3:bool s0:int s2:int
     2  branch-false s3:bool 5
     3  int s2:int 0
     4  return s2:int
     5  copy s1:int s0:int Int
     6  return s1:int
"
    );
}
