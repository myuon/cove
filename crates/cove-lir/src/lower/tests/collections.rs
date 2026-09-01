//! Sequences, ranges, and `for`.
//!
//! Two things are asserted here that the other cases do not reach. One is the
//! *shape* of a walk: a loop tests, reads one element, runs a body and steps,
//! and the step is above the test so that `continue` has somewhere to jump
//! to. The other is where every `Clear` lands, which for a loop is the whole
//! question — a walk that left the element binding set would hold one object
//! per turn until the frame returned, and the listing is the only place that
//! is visible.

use super::{checked, listing, refused};
use crate::layout::Shape;
use crate::lower::lower;

#[test]
fn an_array_literal_is_one_object_and_one_store_per_element() {
    // The elements are in the object rather than behind an indirection, and
    // the index is one slot written again rather than one per element: a
    // frame should not grow with the length of a literal.
    assert_eq!(
        listing("fn first() -> Array<Int> { [1, 2, 3] }", "first"),
        "\
fn0 m.first(0) -> ref
  frame 6: s0:ref s1:int s2:int s3:int s4:ref s5:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  alloc s4:ref Array<array> x3
     4  int s5:int 0
     5  set-elem s4:ref s5:int s1:int
     6  int s5:int 1
     7  set-elem s4:ref s5:int s2:int
     8  int s5:int 2
     9  set-elem s4:ref s5:int s3:int
    10  move s0:ref s4:ref
    11  clear s4:ref
    12  return s0:ref
"
    );
}

#[test]
fn an_index_is_a_bounds_test_and_one_element_read() {
    // The `None` is the allocation: `Inst::Alloc` zeroes the payload and
    // `None` is case 0, so an object nothing else is written into is already
    // the answer for a bad index. A negative index and one past the end are
    // one case with one answer, which is the rule `get` shares with the
    // operations that write.
    //
    // The element is cleared where it is read, on the one path that reads
    // it: it was stored into the answer and is dead from that word onwards.
    assert_eq!(
        listing(
            "fn pick(items: Array<String>, i: Int) -> Option<String> { items.get(i) }",
            "pick"
        ),
        "\
fn0 m.pick(2) -> ref
  frame 8: s0!:ref s1!:int s2:ref s3:int s4:ref s5:int s6:bool s7:ref
     0  len s3:int s0:ref
     1  alloc s4:ref Option<enum>
     2  int s5:int 0
     3  ge.int s6:bool s1:int s5:int
     4  branch-false s6:bool 12
     5  lt.int s6:bool s1:int s3:int
     6  branch-false s6:bool 12
     7  get-elem s7:ref s0:ref s1:int
     8  int s5:int 1
     9  set-word s4:ref +0 s5:int
    10  set-word s4:ref +1 s7:ref
    11  clear s7:ref
    12  move s2:ref s4:ref
    13  clear s4:ref
    14  return s2:ref
"
    );
}

#[test]
fn a_length_is_the_objects_own_header_and_emptiness_is_a_comparison() {
    assert_eq!(
        listing(
            "fn size(items: Array<String>) -> Int { items.length() }",
            "size"
        ),
        "\
fn0 m.size(1) -> int
  frame 3: s0!:ref s1:int s2:int
     0  len s2:int s0:ref
     1  move s1:int s2:int
     2  return s1:int
"
    );
    // `isEmpty()` is `length() == 0` and is lowered as one. The two
    // questions differ by a comparison the instruction set already has.
    assert_eq!(
        listing(
            "fn none(items: Array<Int>) -> Bool { items.isEmpty() }",
            "none"
        ),
        "\
fn0 m.none(1) -> bool
  frame 5: s0!:ref s1:bool s2:int s3:int s4:bool
     0  len s2:int s0:ref
     1  int s3:int 0
     2  eq.int s4:bool s2:int s3:int
     3  move s1:bool s4:bool
     4  return s1:bool
"
    );
}

#[test]
fn a_range_is_one_object_holding_its_two_bounds_and_which_end_it_has() {
    // `..` and `..<` are one family: which of the two a range was written
    // with is a word of the object rather than a layout of its own, so one
    // `Range` layout serves the whole program.
    assert_eq!(
        listing("fn span() -> Range { 0..<3 }", "span"),
        "\
fn0 m.span(0) -> ref
  frame 5: s0:ref s1:int s2:int s3:ref s4:bool
     0  int s1:int 0
     1  int s2:int 3
     2  alloc s3:ref Range<struct>
     3  set-word s3:ref +0 s1:int
     4  set-word s3:ref +1 s2:int
     5  bool s4:bool false
     6  set-word s3:ref +2 s4:bool
     7  move s0:ref s3:ref
     8  clear s3:ref
     9  return s0:ref
"
    );
}

#[test]
fn a_for_over_an_array_holds_one_element_at_a_time() {
    // The `clear` at pc 12 is the invariant the whole design was reviewed
    // for. Without it the binding would hold whichever element the last turn
    // read until the frame returned, and a walk over a large array would
    // retain every object it reached rather than one.
    //
    // The step is above the test and the first turn jumps over it, so
    // `continue` has a target that is known before the body is lowered — and
    // a `continue` that went to the test instead would read the same index
    // again and never finish.
    assert_eq!(
        listing(
            "fn join(items: Array<String>) -> Int {\n  var n = 0\n  for s in items { n += 1 }\n  n\n}",
            "join"
        ),
        "\
fn0 m.join(1) -> int
  frame 10: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:int
     0  int s2:int 0
     1  move s3:ref s0:ref
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 14
     9  get-elem s8:ref s3:ref s5:int
    10  int s9:int 1
    11  add.int s2:int s2:int s9:int
    12  clear s8:ref
    13  jump 6
    14  clear s3:ref
    15  move s1:int s2:int
    16  return s1:int
"
    );
}

#[test]
fn a_for_copies_the_handle_it_walks() {
    // The `move` at pc 1 is the snapshot an `Array` needs and the whole of
    // it: a loop walks the value the iterable had at the top, so rebinding
    // the name inside the body must not move the walk. An immutable
    // collection needs nothing further, because holding the object is
    // holding the elements.
    let text = listing(
        "fn sum(items: Array<Int>) -> Int {\n  var rest = items\n  var n = 0\n  \
         for x in rest { n += x }\n  n\n}",
        "sum",
    );
    assert!(text.contains("     2  move s4:ref s2:ref\n"), "{text}");
}

#[test]
fn a_for_over_a_range_normalises_its_end_once() {
    // `RangeBounds::of` written as two instructions: an inclusive range is
    // an exclusive one whose end is a step further on. An empty or reversed
    // range then iterates zero times with no case of its own, because the
    // first test already fails.
    //
    // The range object is released as soon as its three words are out of it,
    // so a loop that runs for a long time does not hold it.
    assert_eq!(
        listing(
            "fn count(n: Int) -> Int {\n  var sum = 0\n  for i in 0..n { sum += i }\n  sum\n}",
            "count"
        ),
        "\
fn0 m.count(1) -> int
  frame 8: s0!:int s1:int s2:int s3:int s4:ref s5:bool s6:int s7:int
     0  int s2:int 0
     1  int s3:int 0
     2  alloc s4:ref Range<struct>
     3  set-word s4:ref +0 s3:int
     4  set-word s4:ref +1 s0:int
     5  bool s5:bool true
     6  set-word s4:ref +2 s5:bool
     7  get-word s3:int s4:ref +0
     8  get-word s6:int s4:ref +1
     9  get-word s5:bool s4:ref +2
    10  clear s4:ref
    11  int s7:int 1
    12  branch-false s5:bool 14
    13  add.int s6:int s6:int s7:int
    14  jump 16
    15  add.int s3:int s3:int s7:int
    16  lt.int s5:bool s3:int s6:int
    17  branch-false s5:bool 20
    18  add.int s2:int s2:int s3:int
    19  jump 15
    20  move s1:int s2:int
    21  return s1:int
"
    );
}

#[test]
fn a_break_ends_the_element_bindings_live_range_too() {
    // A `for` binding is the loop's slot rather than the per-turn scope's,
    // because the scope gives its slots back when it ends and the next turn
    // writes this one again. So the one path that leaves a turn without
    // reaching its end has to clear it, and that is pc 13.
    assert_eq!(
        listing(
            "fn stop(items: Array<String>) -> Int {\n  var n = 0\n  \
             for s in items {\n    if n > 2 { break }\n    n += 1\n  }\n  n\n}",
            "stop"
        ),
        "\
fn0 m.stop(1) -> int
  frame 11: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:int s10:unit
     0  int s2:int 0
     1  move s3:ref s0:ref
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 19
     9  get-elem s8:ref s3:ref s5:int
    10  int s9:int 2
    11  gt.int s7:bool s2:int s9:int
    12  branch-false s7:bool 15
    13  clear s8:ref
    14  jump 19
    15  int s9:int 1
    16  add.int s2:int s2:int s9:int
    17  clear s8:ref
    18  jump 6
    19  clear s3:ref
    20  move s1:int s2:int
    21  return s1:int
"
    );
}

#[test]
fn a_nested_loop_continues_to_its_own_step() {
    // Each loop has a step of its own, and `continue` reaches the innermost
    // one: pc 22 jumps to pc 15, which is the inner walk's increment. Both
    // element bindings are cleared at the end of the outer turn, innermost
    // first, and the outer element's copy is what the inner loop walked.
    assert_eq!(
        listing(
            "fn skip(rows: Array<Array<Int>>) -> Int {\n  var n = 0\n  \
             for r in rows {\n    for c in r {\n      if c == 0 { continue }\n      \
             n += 1\n    }\n  }\n  n\n}",
            "skip"
        ),
        "\
fn0 m.skip(1) -> int
  frame 16: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:ref s10:int s11:int s12:int s13:int s14:int s15:unit
     0  int s2:int 0
     1  move s3:ref s0:ref
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 29
     9  get-elem s8:ref s3:ref s5:int
    10  move s9:ref s8:ref
    11  len s10:int s9:ref
    12  int s11:int 0
    13  int s12:int 1
    14  jump 16
    15  add.int s11:int s11:int s12:int
    16  lt.int s7:bool s11:int s10:int
    17  branch-false s7:bool 26
    18  get-elem s13:int s9:ref s11:int
    19  int s14:int 0
    20  eq.int s7:bool s13:int s14:int
    21  branch-false s7:bool 23
    22  jump 15
    23  int s14:int 1
    24  add.int s2:int s2:int s14:int
    25  jump 15
    26  clear s9:ref
    27  clear s8:ref
    28  jump 6
    29  clear s3:ref
    30  move s1:int s2:int
    31  return s1:int
"
    );
}

#[test]
fn a_vector_is_a_header_over_a_store() {
    // Both objects are allocated here, for the reason a struct literal's is:
    // the lowering knows the layouts and `Inst::Alloc` takes one. What the
    // machine is asked for is growth, which no instruction expresses.
    //
    // The length is in the header and not in the store, because a store is
    // as long as the last growth made it and the elements past the length
    // are spare room rather than value.
    assert_eq!(
        listing(
            "fn make() -> Vector<Int> {\n  var v = Vector.of(1, 2)\n  v.push(3)\n  v\n}",
            "make"
        ),
        "\
fn0 m.make(0) -> ref
  frame 7: s0:ref s1:int s2:int s3:ref s4:int s5:ref s6:unit
     0  int s1:int 1
     1  int s2:int 2
     2  alloc s3:ref Vector<store> x2
     3  int s4:int 0
     4  set-elem s3:ref s4:int s1:int
     5  int s4:int 1
     6  set-elem s3:ref s4:int s2:int
     7  alloc s5:ref Vector<vector>
     8  int s4:int 2
     9  set-word s5:ref +0 s4:int
    10  set-word s5:ref +1 s3:ref
    11  clear s3:ref
    12  int s1:int 3
    13  call-builtin s6:unit Vector.push (s5:ref s1:int)
    14  move s0:ref s5:ref
    15  clear s5:ref
    16  return s0:ref
"
    );
}

#[test]
fn a_vector_reads_its_length_out_of_its_own_header() {
    // Not `len`, which answers the object header's own length and would be
    // the store's capacity if it were asked of one. A vector's count is
    // payload word 0.
    assert_eq!(
        listing("fn size(v: Vector<String>) -> Int { v.length() }", "size"),
        "\
fn0 m.size(1) -> int
  frame 3: s0!:ref s1:int s2:int
     0  get-word s2:int s0:ref +0
     1  move s1:int s2:int
     2  return s1:int
"
    );
}

#[test]
fn a_for_over_a_vector_walks_a_copy() {
    // `items_of` clones the elements out before the first turn, because the
    // body may push onto the very vector it is walking. So does this: one
    // `Vector.toArray`, and then the same walk an `Array` gets.
    assert_eq!(
        listing(
            "fn walk(v: Vector<Int>) -> Int {\n  var n = 0\n  for x in v { n += x }\n  n\n}",
            "walk"
        ),
        "\
fn0 m.walk(1) -> int
  frame 9: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:int
     0  int s2:int 0
     1  call-builtin s3:ref Vector.toArray (s0:ref)
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 12
     9  get-elem s8:int s3:ref s5:int
    10  add.int s2:int s2:int s8:int
    11  jump 6
    12  clear s3:ref
    13  move s1:int s2:int
    14  return s1:int
"
    );
}

#[test]
fn comparing_two_objects_walks_them() {
    // A `String` compares by its bytes, which is one instruction. What `==`
    // means for an array, a struct or an enum is a rule of the language,
    // stated in the language reference, and the instruction set describes
    // families rather than carrying a case per family — so the walk is one
    // call.
    assert_eq!(
        listing(
            "struct P { x: Int }\nfn same(a: P, b: P) -> Bool { a == b }",
            "same"
        ),
        "\
fn0 m.same(2) -> bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  call-builtin s3:bool Any.equals (s0:ref s1:ref)
     1  move s2:bool s3:bool
     2  return s2:bool
"
    );
    // `!=` is the same call and a `not`, rather than a second builtin that
    // answers the negation.
    assert_eq!(
        listing(
            "fn differ(a: Array<Int>, b: Array<Int>) -> Bool { a != b }",
            "differ"
        ),
        "\
fn0 m.differ(2) -> bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  call-builtin s3:bool Any.equals (s0:ref s1:ref)
     1  not s3:bool s3:bool
     2  move s2:bool s3:bool
     3  return s2:bool
"
    );
}

#[test]
fn a_string_still_compares_by_its_bytes() {
    assert_eq!(
        listing("fn same(a: String, b: String) -> Bool { a == b }", "same"),
        "\
fn0 m.same(2) -> bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  eq.str s3:bool s0:ref s1:ref
     1  move s2:bool s3:bool
     2  return s2:bool
"
    );
}

#[test]
fn is_compares_two_words() {
    // A `Vector` is a header object that growth does not move, so two words
    // that are the same address are the same vector. The checker admits `is`
    // for `Vector` and refuses it everywhere else, so this is the whole of
    // what identity has to decide.
    assert_eq!(
        listing(
            "fn alias(a: Vector<Int>, b: Vector<Int>) -> Bool { a is b }",
            "alias"
        ),
        "\
fn0 m.alias(2) -> bool
  frame 4: s0!:ref s1!:ref s2:bool s3:bool
     0  eq.identity s3:bool s0:ref s1:ref
     1  move s2:bool s3:bool
     2  return s2:bool
"
    );
}

#[test]
fn an_operation_that_builds_an_object_is_the_machines() {
    // The receiver is the first operand and the arguments follow it in
    // source order, which is the shape every one of them has. What decides
    // that an operation is the machine's rather than the instruction set's
    // is that it builds an object whose family only the layout table knows,
    // or that it walks the elements with the language's own equality.
    assert_eq!(
        listing(
            "fn cut(items: Array<String>) -> Array<String> { items.slice(1, 2) }",
            "cut"
        ),
        "\
fn0 m.cut(1) -> ref
  frame 5: s0!:ref s1:ref s2:int s3:int s4:ref
     0  int s2:int 1
     1  int s3:int 2
     2  call-builtin s4:ref Array.slice (s0:ref s2:int s3:int)
     3  move s1:ref s4:ref
     4  clear s4:ref
     5  return s1:ref
"
    );
}

#[test]
fn a_layout_describes_a_family_and_a_vector_declares_two() {
    // `Array<String>` and `Array<Point>` are one layout, because a reference
    // is a reference and what an element is is a question its own object
    // answers. `Array<Int>` and `Array<Duration>` are two, because their
    // words differ and the boundary has to know which.
    let program = lower(&checked(
        "struct Point { x: Int }\n\
         fn f() -> Int {\n  \
           let a = [\"one\"]\n  let b = [Point(x: 1)]\n  \
           let c = [1]\n  let d = [1ms]\n  0\n}",
    ))
    .expect("the program lowers");
    let arrays: Vec<&Shape> = program
        .layouts
        .iter()
        .filter(|layout| matches!(layout.shape, Shape::Elements { .. }))
        .map(|layout| &layout.shape)
        .collect();
    assert_eq!(arrays.len(), 3, "{arrays:?}");

    // A vector is two layouts, and both are declared because growth replaces
    // the store beneath a header that stays where it is: the machine has to
    // find what a new store looks like, and this table is the only thing
    // that says.
    let program = lower(&checked("fn g() -> Int {\n  let v = Vector.of(1)\n  0\n}"))
        .expect("the program lowers");
    let vector: Vec<(&str, &Shape)> = program
        .layouts
        .iter()
        .filter(|layout| !matches!(layout.shape, Shape::Free | Shape::Str))
        .map(|layout| (&*layout.name, &layout.shape))
        .collect();
    assert_eq!(
        vector,
        vec![
            (
                "Vector",
                &Shape::Elements {
                    elem: crate::Repr::Int,
                    growable: true
                }
            ),
            (
                "Vector",
                &Shape::Vector {
                    elem: crate::Repr::Int
                }
            ),
        ]
    );
}

#[test]
fn the_walks_this_task_left_out_say_so() {
    // `Map` and `Set` iterate too, and neither is an index walk: a map
    // yields a `MapEntry` per pair and a set yields its elements in
    // ascending order. They are named rather than approximated.
    assert_eq!(
        refused("fn each(m: Map<String, Int>) -> Int {\n  for e in m { }\n  0\n}"),
        vec!["not yet lowered: a value of type `Map<String, Int>`"]
    );
    assert_eq!(
        refused(
            "fn sorted(items: Array<Int>) -> Array<Int> { items.sorted(by: fn(a, b) { a < b }) }"
        ),
        vec!["not yet lowered: a labelled argument to a builtin method"]
    );
    // The four that take a closure are gaps here and not on the machine: a
    // call through a function value is what is missing, and naming the
    // method would report the wrong thing.
    assert_eq!(
        refused("fn each(items: Array<Int>) -> Array<Int> { items.map(fn(x) { x }) }"),
        vec!["not yet lowered: `Array.map`"]
    );
    assert_eq!(
        refused("fn copy(items: Array<Int>) -> Array<Int> { items.snapshot() }"),
        vec!["not yet lowered: `Array.snapshot`"]
    );
}

#[test]
fn a_continue_ends_the_turn_it_leaves() {
    // The `clear` at pc 13 is a `continue`'s, and pc 17 is the end of a turn
    // that ran to its end. Both are there because a binding dies when the
    // turn it belonged to does: relying on the next turn's `get-elem` to
    // overwrite it would be relying on there being a next turn, and the last
    // turn of a loop may be the one that continues.
    assert_eq!(
        listing(
            "fn count(items: Array<String>) -> Int {\n  var n = 0\n  \
             for s in items {\n    if n > 2 { continue }\n    n += 1\n  }\n  n\n}",
            "count"
        ),
        "\
fn0 m.count(1) -> int
  frame 11: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:int s7:bool s8:ref s9:int s10:unit
     0  int s2:int 0
     1  move s3:ref s0:ref
     2  len s4:int s3:ref
     3  int s5:int 0
     4  int s6:int 1
     5  jump 7
     6  add.int s5:int s5:int s6:int
     7  lt.int s7:bool s5:int s4:int
     8  branch-false s7:bool 19
     9  get-elem s8:ref s3:ref s5:int
    10  int s9:int 2
    11  gt.int s7:bool s2:int s9:int
    12  branch-false s7:bool 15
    13  clear s8:ref
    14  jump 6
    15  int s9:int 1
    16  add.int s2:int s2:int s9:int
    17  clear s8:ref
    18  jump 6
    19  clear s3:ref
    20  move s1:int s2:int
    21  return s1:int
"
    );
}
