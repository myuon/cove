//! `map`, `filter` and `fold`, which are loops in the IR rather than
//! builtins.
//!
//! `docs/LINEAR_VM.md` gives the reason: a builtin that invoked the closure
//! would re-enter the dispatch loop from inside a Rust function, putting a
//! Rust frame under every Cove frame the closure creates — and giving back
//! the property the loop was built to have. So every case here is a listing
//! with an [`Inst::CallClosure`](crate::Inst::CallClosure) inside a loop and
//! no builtin doing the walking.

use super::listing;

/// `map` allocates the answer at the receiver's length and fills it one call
/// at a time, and **both the element and the turn's answer are cleared per
/// turn**.
///
/// That is the same discipline a `for` binding is under, because it is the
/// same lowering: one location holds the element for every turn, so a walk
/// over a large sequence holds one element at a time rather than every
/// element it has reached. The clears are at the end of the body rather than
/// left to the next turn's overwrite, because the last turn has no next one.
///
/// Allocating at the receiver's length is also what makes an empty receiver
/// answer an empty array with no calls: `len` is zero, the test fails on the
/// first turn, and the object the loop allocated is the answer.
#[test]
fn map_is_a_loop_that_clears_the_element_and_the_turn_s_answer() {
    assert_eq!(
        listing(
            "fn f(xs: Array<String>) -> Array<String> { xs.map(fn(s) { s.trim() }) }",
            "f"
        ),
        "\
fn0 m.f(Array) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:ref s10:ref
     0  copy s2:ref s0:ref Array
     1  alloc s3:ref closure m.f#0<closure>
     2  int s4:int 1
     3  store-field s3:ref +0 s4:int Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 18
    12  load-elem s9:ref s2:ref s6:int String
    13  call-closure s10:ref s3:ref (s9:String)
    14  store-elem s5:ref s6:int s10:ref String
    15  clear s10:ref String
    16  clear s9:ref String
    17  jump 9
    18  clear s3:ref fn
    19  clear s2:ref Array
    20  copy s1:ref s5:ref Array
    21  clear s5:ref Array
    22  return s1:ref
"
    );
}

/// `filter` cannot know how many elements it will keep until the last call,
/// and an `Array` object is as long as it was allocated.
///
/// So it fills a run of the receiver's length — the most there can be —
/// counts what it kept in `s6`, and answers `Array.slice(0, kept)`. The words
/// past the count are the zeroes the allocation left, so a reference among
/// them reads null and the collector traces nothing from one.
#[test]
fn filter_fills_a_run_of_the_receiver_s_length_and_slices_it() {
    assert_eq!(
        listing(
            "fn f(xs: Array<String>) -> Array<String> { xs.filter(fn(s) { s.isEmpty() }) }",
            "f"
        ),
        "\
fn0 m.f(Array) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:int s9:bool s10:ref
     0  copy s2:ref s0:ref Array
     1  alloc s3:ref closure m.f#0<closure>
     2  int s4:int 1
     3  store-field s3:ref +0 s4:int Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 0
     8  int s8:int 1
     9  jump 11
    10  add.int s7:int s7:int s8:int
    11  lt.int s9:bool s7:int s4:int
    12  branch-false s9:bool 20
    13  load-elem s10:ref s2:ref s7:int String
    14  call-closure s9:bool s3:ref (s10:String)
    15  branch-false s9:bool 18
    16  store-elem s5:ref s6:int s10:ref String
    17  add.int s6:int s6:int s8:int
    18  clear s10:ref String
    19  jump 10
    20  clear s3:ref fn
    21  clear s2:ref Array
    22  int s4:int 0
    23  call-builtin s2:ref Array.slice (s5:Array s4:Int s6:Int)
    24  clear s5:ref Array
    25  copy s1:ref s2:ref Array
    26  clear s2:ref Array
    27  return s1:ref
"
    );
}

/// `fold` threads one accumulator through every element, and the accumulator
/// is the call's **destination** as well as its first argument.
///
/// A turn is therefore one instruction rather than a call and a copy: the
/// machine copies the arguments into the callee's frame on the way in and the
/// answer back on the way out, so nothing reads the location between the two.
/// It is the arrangement `n += 2` already has, where the destination *is* the
/// accumulator.
///
/// An empty receiver answers `initial`, because nothing overwrote it.
#[test]
fn fold_threads_the_accumulator_through_the_call_s_destination() {
    assert_eq!(
        listing(
            "fn f(xs: Array<String>) -> Int { xs.fold(0, fn(t, s) { t + s.length() }) }",
            "f"
        ),
        "\
fn0 m.f(Array) -> Int
  frame 10: s0!:ref s1:int s2:ref s3:int s4:int s5:ref s6:int s7:int s8:bool s9:ref
     0  copy s2:ref s0:ref Array
     1  int s3:int 0
     2  copy s4:int s3:int Int
     3  alloc s5:ref closure m.f#0<closure>
     4  int s3:int 1
     5  store-field s5:ref +0 s3:int Int
     6  len s3:int s2:ref
     7  int s6:int 0
     8  int s7:int 1
     9  jump 11
    10  add.int s6:int s6:int s7:int
    11  lt.int s8:bool s6:int s3:int
    12  branch-false s8:bool 17
    13  load-elem s9:ref s2:ref s6:int String
    14  call-closure s4:int s5:ref (s4:Int s9:String)
    15  clear s9:ref String
    16  jump 10
    17  clear s5:ref fn
    18  clear s2:ref Array
    19  copy s1:int s4:int Int
    20  return s1:int
"
    );
}

/// A `Vector` is walked through a copy taken **before the first call**.
///
/// A vector shares its storage and a callback can reach the very vector being
/// walked, so what is walked has to be settled first. `Vector.toArray` is
/// that copy, and it is the same one `cove_runtime::builtins` takes at the
/// same point and for the same reason. An `Array` needs none of it: the
/// object *is* the snapshot.
#[test]
fn a_vector_is_walked_through_a_copy_taken_before_the_first_call() {
    assert_eq!(
        listing(
            "fn f(v: Vector<Int>) -> Array<Int> { v.map(fn(x) { x + 1 }) }",
            "f"
        ),
        "\
fn0 m.f(Vector) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int
     0  call-builtin s2:ref Vector.toArray (s0:Vector)
     1  alloc s3:ref closure m.f#0<closure>
     2  int s4:int 1
     3  store-field s3:ref +0 s4:int Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:int s2:ref s6:int Int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10:int Int
    15  jump 9
    16  clear s3:ref fn
    17  clear s2:ref Array
    18  copy s1:ref s5:ref Array
    19  clear s5:ref Array
    20  return s1:ref
"
    );
}

/// A declared function handed to `map` is the same loop.
///
/// The environment names `m.double` and holds nothing, so the loop cannot
/// tell — and does not ask — whether the closure it is calling was written as
/// a lambda.
#[test]
fn a_declared_function_handed_to_map_is_the_same_loop() {
    assert_eq!(
        listing(
            "fn double(n: Int) -> Int { n * 2 }\n\
             fn f(xs: Array<Int>) -> Array<Int> { xs.map(double) }",
            "f"
        ),
        "\
fn1 m.f(Array) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int
     0  copy s2:ref s0:ref Array
     1  alloc s3:ref closure m.double<closure>
     2  int s4:int 0
     3  store-field s3:ref +0 s4:int Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:int s2:ref s6:int Int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10:int Int
    15  jump 9
    16  clear s3:ref fn
    17  clear s2:ref Array
    18  copy s1:ref s5:ref Array
    19  clear s5:ref Array
    20  return s1:ref
"
    );
}

/// A multiword element and a multiword answer are runs of words like any
/// other: the stride is the element layout's width on both sides, and the
/// call's destination is a two-word location.
#[test]
fn a_walk_over_multiword_elements_is_a_stride_rather_than_an_address() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\n\
             fn f(xs: Array<Int>) -> Array<Point> { xs.map(fn(x) { Point(x: x, y: x) }) }",
            "f"
        ),
        "\
fn0 m.f(Array) -> Array
  frame 12: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int s11:int
     0  copy s2:ref s0:ref Array
     1  alloc s3:ref closure m.f#0<closure>
     2  int s4:int 1
     3  store-field s3:ref +0 s4:int Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:int s2:ref s6:int Int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10:int m.Point
    15  jump 9
    16  clear s3:ref fn
    17  clear s2:ref Array
    18  copy s1:ref s5:ref Array
    19  clear s5:ref Array
    20  return s1:ref
"
    );
}
