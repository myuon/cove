//! The three questions a `Range` answers, which are arithmetic rather than
//! builtins.
//!
//! A `Range` is three inline words, so none of these reads an object and none
//! of them reaches `cove_runtime::vm::builtins` — the machine's table has no
//! `Range` arm and does not want one, for the reason `Option` and `Result`
//! have none. What each listing pins is the same thing: that the `+ 1` the
//! oracle's `RangeBounds::of` makes to normalise an inclusive end is *not*
//! made here, and that the `inclusive` word chooses a comparison instead.

use super::listing;

/// `RangeBounds::is_empty` is `end <= start` once the end is normalised.
/// Un-normalised it is one comparison either way: an inclusive range yields
/// nothing when its end is below its start, and an exclusive one when the two
/// are equal as well.
#[test]
fn is_empty_asks_the_written_end_rather_than_a_normalised_one() {
    assert_eq!(
        listing("fn e(r: Range) -> Bool { r.isEmpty() }", "e"),
        "\
fn0 m.e(Range) -> Bool
  frame 5: s0!:int s1!:int s2!:bool s3:bool s4:bool
  local r -> s0:Range [0, 5)
     0  branch-false s2:bool 3
     1  lt.int s4:bool s1:int s0:int
     2  jump 4
     3  le.int s4:bool s1:int s0:int
     4  copy s3:bool s4:bool Bool
     5  return s3:bool
"
    );
}

/// `RangeBounds::len` is `(end - start).max(0)`, and the emptiness test is
/// what stands in for the `max`: a range that yields nothing answers zero
/// without subtracting anything, so the subtraction that is emitted is always
/// of two words the larger of which is first.
#[test]
fn length_answers_zero_before_it_subtracts_anything() {
    assert_eq!(
        listing("fn n(r: Range) -> Int { r.length() }", "n"),
        "\
fn0 m.n(Range) -> Int
  frame 7: s0!:int s1!:int s2!:bool s3:int s4:int s5:bool s6:int
  local r -> s0:Range [0, 12)
     0  branch-false s2:bool 3
     1  lt.int s5:bool s1:int s0:int
     2  jump 4
     3  le.int s5:bool s1:int s0:int
     4  branch-false s5:bool 7
     5  int s4:int 0
     6  jump 11
     7  sub.int s4:int s1:int s0:int
     8  branch-false s2:bool 11
     9  int s6:int 1
    10  add.int s4:int s4:int s6:int
    11  copy s3:int s4:int Int
    12  return s3:int
"
    );
}

/// `RangeBounds::contains` is `start <= value && value < end`. The first half
/// is what a false answer leaves in the destination — a range starting past
/// the value never reaches the second question — and the second is the
/// comparison the `inclusive` word chooses.
#[test]
fn contains_leaves_the_first_comparison_as_the_answer_when_it_fails() {
    assert_eq!(
        listing("fn c(r: Range, v: Int) -> Bool { r.contains(v) }", "c"),
        "\
fn0 m.c(Range Int) -> Bool
  frame 6: s0!:int s1!:int s2!:bool s3!:int s4:bool s5:bool
  local r -> s0:Range [0, 7)
  local v -> s3:Int [0, 7)
     0  le.int s5:bool s0:int s3:int
     1  branch-false s5:bool 6
     2  branch-false s2:bool 5
     3  le.int s5:bool s3:int s1:int
     4  jump 6
     5  lt.int s5:bool s3:int s1:int
     6  copy s4:bool s5:bool Bool
     7  return s4:bool
"
    );
}
