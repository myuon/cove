//! Arrays, ranges, vectors, and the `for` that walks them.

use super::listing;

/// One index location is written again rather than one per element: the
/// frame should not grow with the length of a literal.
#[test]
fn an_array_literal_is_one_object_and_one_store_per_element() {
    assert_eq!(
        listing("fn xs() -> Array<Int> { [1, 2, 3] }", "xs"),
        "\
fn0 m.xs() -> Array
  frame 6: s0:ref s1:int s2:int s3:int s4:ref s5:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  alloc s4:ref Array<array> x3
     4  int s5:int 0
     5  store-elem s4:ref s5:int s1:int Int
     6  int s5:int 1
     7  store-elem s4:ref s5:int s2:int Int
     8  int s5:int 2
     9  store-elem s4:ref s5:int s3:int Int
    10  copy s0:ref s4:ref Array
    11  clear s4:ref Array
    12  return s0:ref
"
    );
}

/// An `Array<Point>` is a run of two-word elements rather than a run of
/// addresses, so the element layout is what the store names.
#[test]
fn an_array_of_multiword_elements_stores_at_the_layout_s_stride() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn xs() -> Array<Point> { [Point(x: 1, y: 2)] }",
            "xs"
        ),
        "\
fn0 m.xs() -> Array
  frame 6: s0:ref s1:int s2:int s3:int s4:int s5:ref
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  alloc s5:ref Array<array> x1
     5  int s1:int 0
     6  store-elem s5:ref s1:int s3:int m.Point
     7  copy s0:ref s5:ref Array
     8  clear s5:ref Array
     9  return s0:ref
"
    );
}

/// `docs/LINEAR_VM.md` gives it one layout for the program,
/// `Struct { start, end, inclusive }`, which is what keeps `..` and `..<`
/// one family rather than two. It is a value like any other and can be
/// bound, passed and iterated later.
#[test]
fn a_range_is_three_inline_words() {
    assert_eq!(
        listing("fn r() -> Range { 0..<3 }", "r"),
        "\
fn0 m.r() -> Range
  frame 8: s0:int s1:int s2:bool s3:int s4:int s5:int s6:int s7:bool
     0  int s3:int 0
     1  int s4:int 3
     2  copy s5:int s3:int Int
     3  copy s6:int s4:int Int
     4  bool s7:bool false
     5  copy s0:int s5:int Range
     6  return s0:int
"
    );
}

/// The end is not adjusted. A turn happens at `index` when `index < end`,
/// and the step is emitted only where that is already known — so the
/// counter never passes the end and nothing can overflow.
#[test]
fn a_for_over_an_exclusive_range_never_touches_the_bound() {
    assert_eq!(
        listing(
            "fn total(n: Int) -> Int {\n  var t = 0\n  for i in 0..<n { t = t + i }\n  t\n}",
            "total"
        ),
        "\
fn0 m.total(Int) -> Int
  frame 12: s0!:int s1:int s2:int s3:int s4:int s5:int s6:bool s7:int s8:bool s9:int s10:bool s11:int
     0  int s2:int 0
     1  int s3:int 0
     2  copy s4:int s3:int Int
     3  copy s5:int s0:int Int
     4  bool s6:bool false
     5  copy s3:int s4:int Int
     6  copy s7:int s5:int Int
     7  copy s8:bool s6:bool Bool
     8  int s9:int 1
     9  jump 13
    10  lt.int s10:bool s3:int s7:int
    11  branch-false s10:bool 22
    12  add.int s3:int s3:int s9:int
    13  lt.int s10:bool s3:int s7:int
    14  branch-false s10:bool 18
    15  add.int s11:int s2:int s3:int
    16  copy s2:int s11:int Int
    17  jump 10
    18  branch-false s8:bool 22
    19  eq.int s10:bool s3:int s7:int
    20  branch-false s10:bool 22
    21  jump 15
    22  copy s1:int s2:int Int
    23  return s1:int
"
    );
}

/// The `inclusive` word is kept and the comparison is chosen from it: the
/// one extra turn is where `index == end`, reached by a branch rather than
/// by a larger bound. `0..Int.MAX` therefore yields every value and stops.
#[test]
fn a_for_over_an_inclusive_range_earns_one_more_turn_at_the_end() {
    assert_eq!(
        listing(
            "fn total(n: Int) -> Int {\n  var t = 0\n  for i in 0..n { t = t + i }\n  t\n}",
            "total"
        ),
        "\
fn0 m.total(Int) -> Int
  frame 12: s0!:int s1:int s2:int s3:int s4:int s5:int s6:bool s7:int s8:bool s9:int s10:bool s11:int
     0  int s2:int 0
     1  int s3:int 0
     2  copy s4:int s3:int Int
     3  copy s5:int s0:int Int
     4  bool s6:bool true
     5  copy s3:int s4:int Int
     6  copy s7:int s5:int Int
     7  copy s8:bool s6:bool Bool
     8  int s9:int 1
     9  jump 13
    10  lt.int s10:bool s3:int s7:int
    11  branch-false s10:bool 22
    12  add.int s3:int s3:int s9:int
    13  lt.int s10:bool s3:int s7:int
    14  branch-false s10:bool 18
    15  add.int s11:int s2:int s3:int
    16  copy s2:int s11:int Int
    17  jump 10
    18  branch-false s8:bool 22
    19  eq.int s10:bool s3:int s7:int
    20  branch-false s10:bool 22
    21  jump 15
    22  copy s1:int s2:int Int
    23  return s1:int
"
    );
}

/// One location holds the element for every turn, and it is cleared at the
/// end of each — so a walk over a large array holds one element at a time
/// rather than every element it has reached.
#[test]
fn a_for_over_an_array_walks_the_object_and_clears_the_element() {
    assert_eq!(
        listing(
            "fn count(xs: Array<String>) -> Int {\n  var t = 0\n  for x in xs { t = t + x.length() }\n  t\n}",
            "count"
        ),
        "\
fn0 m.count(Array) -> Int
  frame 11: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:int s10:int
     0  int s2:int 0
     1  copy s3:ref s0:ref Array
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 15
     9  load-elem s8:ref s3:ref s5:int String
    10  call-builtin s9:int String.length (s8:String)
    11  add.int s10:int s2:int s9:int
    12  copy s2:int s10:int Int
    13  clear s8:ref String
    14  jump 6
    15  clear s3:ref Array
    16  copy s1:int s2:int Int
    17  return s1:int
"
    );
}

/// The body may push onto the very vector it is walking, so the copy is
/// taken before the first turn — the same copy `items_of` makes when it
/// clones the elements out.
#[test]
fn a_for_over_a_vector_walks_a_snapshot() {
    assert_eq!(
        listing(
            "fn count(v: Vector<Int>) -> Int {\n  var t = 0\n  for x in v { t = t + x }\n  t\n}",
            "count"
        ),
        "\
fn0 m.count(Vector) -> Int
  frame 10: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:int s9:int
     0  int s2:int 0
     1  call-builtin s3:ref Vector.toArray (s0:Vector)
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 13
     9  load-elem s8:int s3:ref s5:int Int
    10  add.int s9:int s2:int s8:int
    11  copy s2:int s9:int Int
    12  jump 6
    13  clear s3:ref Array
    14  copy s1:int s2:int Int
    15  return s1:int
"
    );
}

/// The loop owns the element's location, because the per-turn scope gives
/// its slots back when it ends. That leaves nobody to clear it on the one
/// path that does not reach the end of a turn, which is what this is.
#[test]
fn a_break_out_of_a_for_clears_the_element_it_was_holding() {
    assert_eq!(
        listing(
            "fn first(xs: Array<String>) -> Int {\n  var t = 0\n  for x in xs {\n    if x == \"\" { continue }\n    if x == \"q\" { break }\n    t = t + 1\n  }\n  t\n}",
            "first"
        ),
        "\
fn0 m.first(Array) -> Int
  frame 13: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:ref s10:unit s11:int s12:int
     0  int s2:int 0
     1  copy s3:ref s0:ref Array
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 27
     9  load-elem s8:ref s3:ref s5:int String
    10  str s9:ref \"\"
    11  eq.str s7:bool s8:ref s9:ref
    12  clear s9:ref String
    13  branch-false s7:bool 16
    14  clear s8:ref String
    15  jump 6
    16  str s9:ref \"q\"
    17  eq.str s7:bool s8:ref s9:ref
    18  clear s9:ref String
    19  branch-false s7:bool 22
    20  clear s8:ref String
    21  jump 27
    22  int s11:int 1
    23  add.int s12:int s2:int s11:int
    24  copy s2:int s12:int Int
    25  clear s8:ref String
    26  jump 6
    27  clear s3:ref Array
    28  copy s1:int s2:int Int
    29  return s1:int
"
    );
}

/// The two objects are the whole of what a vector is, and both are
/// allocated here because the lowering knows the layouts. What the machine
/// is asked for is growth, which no instruction expresses.
#[test]
fn a_vector_is_a_header_and_a_store() {
    assert_eq!(
        listing("fn v() -> Vector<Int> { Vector.of(1, 2) }", "v"),
        "\
fn0 m.v() -> Vector
  frame 6: s0:ref s1:int s2:int s3:ref s4:int s5:ref
     0  int s1:int 1
     1  int s2:int 2
     2  alloc s3:ref Vector<store> x2
     3  int s4:int 0
     4  store-elem s3:ref s4:int s1:int Int
     5  int s4:int 1
     6  store-elem s3:ref s4:int s2:int Int
     7  alloc s5:ref Vector<vector>
     8  int s4:int 2
     9  store-field s5:ref +0 s4:int Int
    10  store-field s5:ref +1 s3:ref <ref>
    11  clear s3:ref <ref>
    12  copy s0:ref s5:ref Vector
    13  clear s5:ref Vector
    14  return s0:ref
"
    );
}

/// The length is payload word 0 and the elements are in the store payload
/// word 1 names, so `get` is a bounds test and a load rather than a call.
#[test]
fn reading_a_vector_element_is_ordinary_instructions() {
    assert_eq!(
        listing(
            "fn head(v: Vector<Int>) -> Option<Int> { v.get(0) }",
            "head"
        ),
        "\
fn0 m.head(Vector) -> Option
  frame 10: s0!:ref s1:int s2:int s3:int s4:int s5:ref s6:int s7:int s8:int s9:bool
     0  int s3:int 0
     1  load-field s4:int s0:ref +0 Int
     2  load-field s5:ref s0:ref +1 <ref>
     3  int s6:int 0
     4  clear s7:int Int
     5  int s8:int 0
     6  ge.int s9:bool s3:int s8:int
     7  branch-false s9:bool 12
     8  lt.int s9:bool s3:int s4:int
     9  branch-false s9:bool 12
    10  int s6:int 1
    11  load-elem s7:int s5:ref s3:int Int
    12  clear s5:ref <ref>
    13  copy s1:int s6:int Option
    14  return s1:int
"
    );
}

/// What `==` means for a struct is a rule of the language rather than an
/// instruction, so it is a call — and an argument carries the layout of the
/// location it names, so the call hands over both `Point`s where they are.
///
/// This used to box each of them, because an argument was a slot and a slot
/// says where a value begins and not how wide it is. That was one allocation
/// per comparison, on a path the predecessor did not allocate on.
#[test]
fn two_inline_values_are_compared_where_they_sit() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn same(a: Point, b: Point) -> Bool { a == b }",
            "same"
        ),
        "\
fn0 m.same(m.Point m.Point) -> Bool
  frame 6: s0!:int s1!:int s2!:int s3!:int s4:bool s5:bool
     0  call-builtin s5:bool Any.equals (s0:m.Point s2:m.Point)
     1  copy s4:bool s5:bool Bool
     2  return s4:bool
"
    );
}

/// A reference carries its description in the object's own header, so
/// there is nothing to attach.
#[test]
fn two_arrays_compare_without_being_boxed() {
    assert_eq!(
        listing(
            "fn same(a: Array<Int>, b: Array<Int>) -> Bool { a == b }",
            "same"
        ),
        "\
fn0 m.same(Array Array) -> Bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  call-builtin s3:bool Any.equals (s0:Array s1:Array)
     1  copy s2:bool s3:bool Bool
     2  return s2:bool
"
    );
}

/// A `Vector` is a header object that growth does not move, so two words
/// that are the same address are the same vector.
#[test]
fn is_compares_two_words_as_words() {
    assert_eq!(
        listing(
            "fn same(a: Vector<Int>, b: Vector<Int>) -> Bool { a is b }",
            "same"
        ),
        "\
fn0 m.same(Vector Vector) -> Bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  eq.identity s3:bool s0:ref s1:ref
     1  copy s2:bool s3:bool Bool
     2  return s2:bool
"
    );
}
