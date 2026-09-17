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
fn @m.pick(Bool) -> Int
  frame 2: s0!:bool s1:int
  local c -> s0:Bool [0, 5)
     0  branch-false s0:bool 3
     1  int s1:int 1
     2  jump 4
     3  int s1:int 2
     4  return s1:Int
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
fn @m.maybe(Bool) -> Int
  frame 3: s0!:bool s1:int s2:int
  local c -> s0:Bool [0, 5)
  local n -> s2:Int [1, 4)
     0  int s2:int 0
     1  branch-false s0:bool 3
     2  int s2:int 1
     3  copy s1:Int s2:Int
     4  return s1:Int
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
fn @m.pick(Bool m.Point m.Point) -> m.Point
  frame 7: s0!:bool s1!:int s2!:int s3!:int s4!:int s5:int s6:int
  local c -> s0:Bool [0, 5)
  local a -> s1..s2:m.Point [0, 5)
  local b -> s3..s4:m.Point [0, 5)
     0  branch-false s0:bool 3
     1  copy s5..s6:m.Point s1..s2:m.Point
     2  jump 4
     3  copy s5..s6:m.Point s3..s4:m.Point
     4  return s5..s6:m.Point
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
fn @m.count(Int) -> Int
  frame 4: s0!:int s1:int s2:int s3:bool
  local n -> s0:Int [0, 6)
  local t -> s2:Int [1, 5)
     0  int s2:int 0
     1  lt.int.branch s3:bool s2:int s0:int 4
     2  add.int.imm s2:int s2:int 1
     3  jump 1
     4  copy s1:Int s2:Int
     5  return s1:Int
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
fn @m.first() -> Int
  frame 4: s0:int s1:int s2:bool s3:unit
  local t -> s1:Int [1, 8)
     0  int s1:int 0
     1  bool s2:bool true
     2  branch-false s2:bool 7
     3  add.int.imm s1:int s1:int 1
     4  gt.int.imm.branch s2:bool s1:int 3 6
     5  jump 7
     6  jump 1
     7  copy s0:Int s1:Int
     8  return s0:Int
"
    );
}

/// The frame ends at the `Return`, so nothing is cleared on the way out: a
/// location whose frame is gone retains nothing.
///
/// Both `return`s name `s1`, which is the function's answer location, and
/// that is destination forwarding reaching an explicit `return` — issue #302,
/// which had reached a body's tail expression and not this. It used to build
/// the `0` in a slot of its own and answer that, so the two `return`s named
/// two slots and the frame was a word wider.
///
/// The width is the small half of it. `lower::inline` lends a caller's
/// destination to an expanded body only when *every* `Return` names one
/// slot, so a function shaped like this one could not be renamed into its
/// caller and every call site copied the answer back out.
#[test]
fn a_return_leaves_without_clearing_what_the_frame_was_holding() {
    assert_eq!(
        listing(
            "fn early(n: Int) -> Int {\n  if n < 0 { return 0 }\n  n\n}",
            "early"
        ),
        "\
fn @m.early(Int) -> Int
  frame 4: s0!:int s1:int s2:bool s3:unit
  local n -> s0:Int [0, 5)
     0  lt.int.imm.branch s2:bool s0:int 0 3
     1  int s1:int 0
     2  return s1:Int
     3  copy s1:Int s0:Int
     4  return s1:Int
"
    );
}

/// A temporary held across a **diverging** sub-expression is cleared on the
/// path that leaves, and that is a fix rather than a tidying.
///
/// `both("{x}", if total > 0 { x } else { break })` assembles the
/// interpolated string into `s12` and then leaves the call through the
/// `break`. Nothing ever reaches the release that would have ended `s12`'s
/// live range, and `s12` belongs to no scope and is not the loop's element —
/// so before this the object stayed reachable from a slot of a live frame
/// for the rest of the frame. A leak rather than a crash, and one at every
/// call site rather than only in a walk.
///
/// Instructions 19–21 are the answer, in the order a turn ends in: the
/// temporaries this turn made, innermost first, then the bindings its scopes
/// own, then the element. `s8` is the element and is cleared by the loop
/// because the loop owns it; `s10` and `s12` are cleared because
/// [`Body::held`](super::super::Body) records every temporary that holds a
/// reference and the loop took a mark of that list when it began. The byte
/// buffer the string was assembled in is not among them: it was released at
/// 15, as soon as the finish had consumed it.
///
/// What is *not* cleared is as much of the point: `s3`, the array being
/// walked, is below the mark and is read again at 28, where the `break`'s
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
fn @m.f(Array) -> Int
  frame 19: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:int s10:ref s11:unit s12:ref s13:unit s14:int s15:int s16:int s17:ref s18:int
  local xs -> s0:Array [0, 36)
  local total -> s2:Int [1, 35)
  local x -> s8:String [9, 31)
     0  int s2:int 0
     1  copy s3:Array s0:Array
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int.branch s7:bool s5:int s4:int 33
     8  load-elem s8:String s3:ref s5:int
     9  int s9:int 16
    10  growable-alloc.bytes s10:ref s9:int
    11  len s14:int s8:ref
    12  load-field s15:Int s10:ref +0
    13  growable-ensure.bytes s10:ref s14:int
    14  int s16:int 0
    15  load-field s17:<ref> s10:ref +1
    16  run-copy.bytes (s17:<ref> s15:Int s8:String s16:Int s14:Int)
    17  clear s17:<ref>
    18  growable-commit.bytes s10:ref s14:int
    19  run-finish.bytes s12:ref s10:ref String utf8
    20  clear s10:ByteBuffer
    21  gt.int.imm.branch s7:bool s2:int 0 24
    22  copy s10:String s8:String
    23  jump 28
    24  clear s10:String
    25  clear s12:String
    26  clear s8:String
    27  jump 33
    28  int s2:int 0
    29  clear s10:String
    30  clear s12:String
    31  clear s8:String
    32  jump 6
    33  clear s3:Array
    34  copy s1:Int s2:Int
    35  return s1:Int
"
    );
}
