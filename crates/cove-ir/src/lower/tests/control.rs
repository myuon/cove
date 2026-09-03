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
  local c -> s0:Bool [0, 7)
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
  local c -> s0:Bool [0, 5)
  local n -> s2:Int [1, 5)
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
  local c -> s0:Bool [0, 5)
  local a -> s1:m.Point [0, 5)
  local b -> s3:m.Point [0, 5)
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
  local n -> s0:Int [0, 8)
  local t -> s2:Int [1, 8)
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
  local t -> s1:Int [1, 12)
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
  local n -> s0:Int [0, 6)
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

/// A temporary held across a **diverging** sub-expression is cleared on the
/// path that leaves, and that is a fix rather than a tidying.
///
/// `both("{x}", if total > 0 { x } else { break })` evaluates the
/// interpolated string into `s9` and then leaves the call through the
/// `break`. Nothing ever reaches the release that would have ended `s9`'s
/// live range, and `s9` belongs to no scope and is not the loop's element —
/// so before this the object stayed reachable from a slot of a live frame
/// for the rest of the frame. A leak rather than a crash, and one at every
/// call site rather than only in a walk.
///
/// Instructions 16–18 are the answer, in the order a turn ends in: the
/// temporaries this turn made, innermost first, then the bindings its scopes
/// own, then the element. `s8` is the element and is cleared by the loop
/// because the loop owns it; `s9` and `s10` are cleared because
/// [`Body::held`](super::super::Body) records every temporary that holds a
/// reference and the loop took a mark of that list when it began.
///
/// What is *not* cleared is as much of the point: `s3`, the array being
/// walked, is below the mark and is read again at 26, where the `break`'s
/// jump lands.
#[test]
fn a_break_clears_the_temporaries_the_turn_was_holding() {
    assert_eq!(
        listing(
            "fn both(a: String, b: String) -> Int { 0 }\n\
             fn f(xs: Array<String>) -> Int {\n  \
               var total = 0\n  \
               for x in xs {\n    \
                 total = both(\"{x}\", if total > 0 { x } else { break })\n  \
               }\n  \
               total\n\
             }",
            "f"
        ),
        "\
fn1 m.f(Array) -> Int
  frame 13: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:ref s10:ref s11:int s12:unit
  local xs -> s0:Array [0, 28)
  local total -> s2:Int [1, 28)
  local x -> s8:String [10, 24)
     0  int s2:int 0
     1  copy s3:ref s0:ref Array
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 26
     9  load-elem s8:ref s3:ref s5:int String
    10  call-builtin s9:ref String.interpolate (s8:String)
    11  int s11:int 0
    12  gt.int s7:bool s2:int s11:int
    13  branch-false s7:bool 16
    14  copy s10:ref s8:ref String
    15  jump 20
    16  clear s10:ref String
    17  clear s9:ref String
    18  clear s8:ref String
    19  jump 26
    20  call s11:int m.both (s9:String s10:String)
    21  clear s10:ref String
    22  clear s9:ref String
    23  copy s2:int s11:int Int
    24  clear s8:ref String
    25  jump 6
    26  clear s3:ref Array
    27  copy s1:int s2:int Int
    28  return s1:int
"
    );
}
