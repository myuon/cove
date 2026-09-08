//! `map` and `sorted`, which are loops in the IR rather than builtins — and
//! `filter` and `fold`, which are ordinary calls into the standard library and
//! are here to say so.
//!
//! `docs/LINEAR_VM.md` gives the reason `map` and `sorted` are loops: a
//! builtin that invoked the closure would re-enter the dispatch loop from
//! inside a Rust function, putting a Rust frame under every Cove frame the
//! closure creates — and giving back the property the loop was built to
//! have. So each of those two is a listing with an
//! [`Inst::CallClosure`](crate::Inst::CallClosure) inside a loop and no
//! builtin doing the walking.
//!
//! `filter` and `fold` needed none of that: they are `std.array` and
//! `std.vector` functions written in Cove, resolved to an ordinary
//! [`Inst::Call`](crate::Inst::Call) before this crate's per-receiver dispatch
//! ever sees the method name, and their two cases below are what that call
//! looks like rather than what a walk does.

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
fn @m.f(Array) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:ref s10:ref
  local xs -> s0:Array [0, 22)
     0  copy s2:Array s0:Array
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 18
    12  load-elem s9:String s2:ref s6:int
    13  call-closure s10:ref s3:ref (s9:String)
    14  store-elem s5:ref s6:int s10:String
    15  clear s10:String
    16  clear s9:String
    17  jump 9
    18  clear s3:fn
    19  clear s2:Array
    20  copy s1:Array s5:Array
    21  return s1:Array
"
    );
}

/// `filter` is not a walk in the IR at all any more: `Body::call_builtin_method`
/// resolves it, before either `Body::array_method` or `Body::walk_with` is
/// ever asked, to an ordinary call into `std.array.filter` — the same
/// generic-instantiated `` `call` `` a program's own `filter(xs, keep)` would
/// reach. The `for` and the `Vector` it fills are inside that callee's own
/// body, in `crates/cove-sema/std/array.cove`, not in this listing.
#[test]
fn filter_is_an_ordinary_call_into_the_standard_library() {
    assert_eq!(
        listing(
            "fn f(xs: Array<String>) -> Array<String> { xs.filter(fn(s) { s.isEmpty() }) }",
            "f"
        ),
        "\
fn @m.f(Array) -> Array
  frame 5: s0!:ref s1:ref s2:ref s3:int s4:ref
  local xs -> s0:Array [0, 7)
     0  alloc s2:ref closure m.f#0<closure>
     1  func-ref s3:int @m.f#0
     2  store-field s2:ref +0 s3:Int
     3  call s4:Array std.array.filter<String> (s0:Array s2:fn)
     4  clear s2:fn
     5  copy s1:Array s4:Array
     6  return s1:Array
"
    );
}

/// `fold` moved the same way `filter` did: `Body::call_builtin_method`
/// resolves it to an ordinary call into `std.array.fold`, generic over both
/// the element and the accumulator types, and the accumulator that used to
/// thread through a call's destination in this crate now threads through an
/// ordinary `var` inside that callee's own `for`, in
/// `crates/cove-sema/std/array.cove`.
#[test]
fn fold_is_an_ordinary_call_into_the_standard_library() {
    assert_eq!(
        listing(
            "fn f(xs: Array<String>) -> Int { xs.fold(0, fn(t, s) { t + s.length() }) }",
            "f"
        ),
        "\
fn @m.f(Array) -> Int
  frame 5: s0!:ref s1:int s2:int s3:ref s4:int
  local xs -> s0:Array [0, 8)
     0  int s2:int 0
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:Int
     4  call s4:Int std.array.fold<String, Int> (s0:Array s2:Int s3:fn)
     5  clear s3:fn
     6  copy s1:Int s4:Int
     7  return s1:Int
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
fn @m.f(Vector) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int
  local v -> s0:Vector [0, 20)
     0  call-builtin s2:Array Vector.toArray (s0:Vector)
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:Int s2:ref s6:int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10:Int
    15  jump 9
    16  clear s3:fn
    17  clear s2:Array
    18  copy s1:Array s5:Array
    19  return s1:Array
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
fn @m.f(Array) -> Array
  frame 11: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int
  local xs -> s0:Array [0, 20)
     0  copy s2:Array s0:Array
     1  alloc s3:ref closure m.double<closure>
     2  func-ref s4:int @m.double
     3  store-field s3:ref +0 s4:Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:Int s2:ref s6:int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10:Int
    15  jump 9
    16  clear s3:fn
    17  clear s2:Array
    18  copy s1:Array s5:Array
    19  return s1:Array
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
fn @m.f(Array) -> Array
  frame 12: s0!:ref s1:ref s2:ref s3:ref s4:int s5:ref s6:int s7:int s8:bool s9:int s10:int s11:int
  local xs -> s0:Array [0, 20)
     0  copy s2:Array s0:Array
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:Int
     4  len s4:int s2:ref
     5  alloc s5:ref Array<array> xs4:int
     6  int s6:int 0
     7  int s7:int 1
     8  jump 10
     9  add.int s6:int s6:int s7:int
    10  lt.int s8:bool s6:int s4:int
    11  branch-false s8:bool 16
    12  load-elem s9:Int s2:ref s6:int
    13  call-closure s10:int s3:ref (s9:Int)
    14  store-elem s5:ref s6:int s10..s11:m.Point
    15  jump 9
    16  clear s3:fn
    17  clear s2:Array
    18  copy s1:Array s5:Array
    19  return s1:Array
"
    );
}

/// `sorted` is a bottom-up stable merge, written out in the IR.
///
/// The oracle's `merge_sort` gives both halves of why it is a merge rather
/// than a call to one. `by` is a Cove closure, so it can fail or be
/// cancelled, and a `FnMut(&T, &T) -> Ordering` has nowhere to put a failure;
/// and `by` can contradict itself, where the schema promises *some*
/// permutation and nothing more — so there is no invariant here to break.
///
/// The shape, in slots: `s6` is `source`, the working copy `Array.slice`
/// makes of the receiver, and `s2` is `merged`, the run of the same length
/// the pass fills. `s8` is `width`, which doubles at 62; `s10` is `start`,
/// `s11` and `s12` the block's `middle` and `end` after the two clamps at
/// 19–24, and `s13`/`s14` the two runs' cursors.
///
/// Three loops make one pass over the blocks. 27–41 is the merge: the right
/// run's element is taken only when `by` answers **true** for it against the
/// left run's, which is the oracle's own operand order at 33 and is what
/// makes the sort stable — equal elements meet with the earlier one on the
/// left and 34 falls through to 38, which takes the left. 42–48 and 49–55
/// are the two tails, of which at most one runs.
///
/// 58–61 swap the two runs rather than copying one back, so what was merged
/// is what the next pass reads. `while width < len` at 11 is the outer test,
/// so a receiver of nothing or of one element makes no pass at all and
/// answers the copy — which is also why the receiver is never written
/// through on any path.
#[test]
fn sorted_is_a_bottom_up_stable_merge_over_two_runs() {
    assert_eq!(
        listing(
            "fn f(xs: Array<Int>) -> Array<Int> { xs.sorted(by: fn(a, b) { a < b }) }",
            "f"
        ),
        "\
fn @m.f(Array) -> Array
  frame 20: s0!:ref s1:ref s2:ref s3:ref s4:int s5:int s6:ref s7:int s8:int s9:int s10:int s11:int s12:int s13:int s14:int s15:bool s16:int s17:int s18:bool s19:ref
  local xs -> s0:Array [0, 68)
     0  copy s2:Array s0:Array
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:Int
     4  len s4:int s2:ref
     5  int s5:int 0
     6  call-builtin s6:Array Array.slice (s2:Array s5:Int s4:Int)
     7  clear s2:Array
     8  alloc s2:ref Array<array> xs4:int
     9  int s7:int 1
    10  int s8:int 1
    11  lt.int s15:bool s8:int s4:int
    12  branch-false s15:bool 64
    13  copy s9:Int s5:Int
    14  copy s10:Int s5:Int
    15  lt.int s15:bool s10:int s4:int
    16  branch-false s15:bool 58
    17  add.int s11:int s10:int s8:int
    18  add.int s12:int s11:int s8:int
    19  gt.int s18:bool s11:int s4:int
    20  branch-false s18:bool 22
    21  copy s11:Int s4:Int
    22  gt.int s18:bool s12:int s4:int
    23  branch-false s18:bool 25
    24  copy s12:Int s4:Int
    25  copy s13:Int s10:Int
    26  copy s14:Int s11:Int
    27  lt.int s15:bool s13:int s11:int
    28  branch-false s15:bool 42
    29  lt.int s15:bool s14:int s12:int
    30  branch-false s15:bool 42
    31  load-elem s16:Int s6:ref s14:int
    32  load-elem s17:Int s6:ref s13:int
    33  call-closure s15:bool s3:ref (s16:Int s17:Int)
    34  branch-false s15:bool 38
    35  store-elem s2:ref s9:int s16:Int
    36  add.int s14:int s14:int s7:int
    37  jump 40
    38  store-elem s2:ref s9:int s17:Int
    39  add.int s13:int s13:int s7:int
    40  add.int s9:int s9:int s7:int
    41  jump 27
    42  lt.int s18:bool s13:int s11:int
    43  branch-false s18:bool 49
    44  load-elem s16:Int s6:ref s13:int
    45  store-elem s2:ref s9:int s16:Int
    46  add.int s13:int s13:int s7:int
    47  add.int s9:int s9:int s7:int
    48  jump 42
    49  lt.int s18:bool s14:int s12:int
    50  branch-false s18:bool 56
    51  load-elem s16:Int s6:ref s14:int
    52  store-elem s2:ref s9:int s16:Int
    53  add.int s14:int s14:int s7:int
    54  add.int s9:int s9:int s7:int
    55  jump 49
    56  copy s10:Int s12:Int
    57  jump 15
    58  copy s19:Array s6:Array
    59  copy s6:Array s2:Array
    60  copy s2:Array s19:Array
    61  clear s19:Array
    62  add.int s8:int s8:int s8:int
    63  jump 11
    64  clear s2:Array
    65  clear s3:fn
    66  copy s1:Array s6:Array
    67  return s1:Array
"
    );
}

/// A sort over elements that hold a reference clears both of the two the
/// comparison was holding, at the end of every comparison and of every tail
/// step.
///
/// The same discipline every other walk's element is under, for the same
/// reason: a sort of a large sequence must hold two elements at a time
/// rather than every element it has compared. `clear s16`/`clear s17` at
/// 41–42 are the merge's, and `clear s16` at 47 and 55 the two tails'.
#[test]
fn a_sort_of_references_clears_both_elements_it_compared() {
    let text = listing(
        "fn f(xs: Array<String>) -> Array<String> { xs.sorted(by: fn(a, b) { a < b }) }",
        "f",
    );
    assert!(
        text.contains("    41  clear s17:String\n    42  clear s16:String\n"),
        "{text}"
    );
    assert_eq!(text.matches("clear s16:String").count(), 3, "{text}");
}
