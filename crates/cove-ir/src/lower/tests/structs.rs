//! Structs, which are their fields where the value is.
//!
//! The cases here are what the representation was changed for. A struct is
//! the consecutive words of its fields, so a copy is a copy — two words in,
//! two words out — and a field is a slot number this lowering computes
//! rather than an instruction the machine runs.

use super::listing;

/// There is no allocation and no indirection: `Point` is two words of the
/// frame, and building one is two copies. Every field is evaluated before
/// anything is stored, in source order, because an initializer's arguments
/// are ordinary expressions and one of them may do something the next one
/// sees.
#[test]
fn a_struct_literal_is_its_fields_written_where_the_value_is() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn origin() -> Point { Point(x: 1, y: 2) }",
            "origin"
        ),
        "\
fn0 m.origin() -> m.Point
  frame 6: s0:int s1:int s2:int s3:int s4:int s5:int
     0  int s2:int 1
     1  int s3:int 2
     2  copy s4:int s2:int Int
     3  copy s5:int s3:int Int
     4  copy s0:int s4:int m.Point
     5  return s0:int
"
    );
}

/// This is `docs/LINEAR_VM.md`'s first worked case. `var b = a` is one
/// `Copy` of two words, `b.x = 7` writes `b`'s first word, and `a.x` is
/// still slot 3 and was never touched. No bit was set and no protocol ran.
#[test]
fn a_two_word_struct_is_copied_by_one_copy() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn f() -> Int {\n  var a = Point(x: 1, y: 2)\n  var b = a\n  b.x = 7\n  a.x\n}",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 7: s0:int s1:int s2:int s3:int s4:int s5:int s6:int
  local a -> s3:m.Point [4, 8)
  local b -> s5:m.Point [5, 8)
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  copy s5:int s3:int m.Point
     5  int s1:int 7
     6  copy s5:int s1:int Int
     7  copy s0:int s3:int Int
     8  return s0:int
"
    );
}

/// `Line` is `[from.x, from.y, to.x, to.y]`, four words with no
/// indirection in it at all. One `Copy` moves all four, `m.from.x` is
/// slot + 0 of the copy, and `l.from.x` is slot 0 of the original.
#[test]
fn a_nested_struct_is_copied_whole() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nstruct Line { from: Point, to: Point }\nfn f(l: Line) -> Int {\n  var m = l\n  m.from.x = 7\n  l.from.x + m.from.x\n}",
            "f"
        ),
        "\
fn0 m.f(m.Line) -> Int
  frame 10: s0!:int s1!:int s2!:int s3!:int s4:int s5:int s6:int s7:int s8:int s9:int
  local l -> s0:m.Line [0, 6)
  local m -> s5:m.Line [1, 5)
     0  copy s5:int s0:int m.Line
     1  int s9:int 7
     2  copy s5:int s9:int Int
     3  add.int s9:int s0:int s5:int
     4  copy s4:int s9:int Int
     5  return s4:int
"
    );
}

/// `l.to.y` is `base + Field::at + Field::at`, added here rather than read
/// by two instructions. The whole body is the copy into the answer.
#[test]
fn a_field_of_a_field_is_arithmetic_and_emits_nothing() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nstruct Line { from: Point, to: Point }\nfn f(l: Line) -> Int { l.to.y }",
            "f"
        ),
        "\
fn0 m.f(m.Line) -> Int
  frame 5: s0!:int s1!:int s2!:int s3!:int s4:int
  local l -> s0:m.Line [0, 2)
     0  copy s4:int s3:int Int
     1  return s4:int
"
    );
}

/// `docs/LINEAR_VM.md`'s third worked case. One `Copy` of three words
/// makes the `Point` independent — its words were copied — and leaves the
/// `Vector` shared, because what was copied is its address and a
/// `Vector`'s storage is shared by the language's own rule. Both answers
/// fall out of the same copy; neither needs a policy.
#[test]
fn a_struct_holding_a_vector_copies_the_words_and_shares_the_address() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nstruct Wrapper { p: Point, v: Vector<Int> }\nfn f(w: Wrapper) -> Bool {\n  var other = w\n  other.p.x = 7\n  other.v is w.v\n}",
            "f"
        ),
        "\
fn0 m.f(m.Wrapper) -> Bool
  frame 9: s0!:int s1!:int s2!:ref s3:bool s4:int s5:int s6:ref s7:int s8:bool
  local w -> s0:m.Wrapper [0, 7)
  local other -> s4:m.Wrapper [1, 5)
     0  copy s4:int s0:int m.Wrapper
     1  int s7:int 7
     2  copy s4:int s7:int Int
     3  eq.identity s8:bool s6:ref s2:ref
     4  copy s3:bool s8:bool Bool
     5  clear s4:int m.Wrapper
     6  return s3:bool
"
    );
}

/// There is no place to walk back to, so the base is evaluated and the
/// field is an offset into the location it left behind.
#[test]
fn a_field_of_a_call_s_answer_is_copied_out_of_the_temporary() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn mk() -> Point { Point(x: 1, y: 2) }\nfn f() -> Int { mk().y }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
     0  call s1:int m.mk ()
     1  copy s3:int s2:int Int
     2  copy s0:int s3:int Int
     3  return s0:int
"
    );
}

/// `Function::returns` is a layout, so a return copies as many words as it
/// says and the caller's destination is a location rather than a slot.
#[test]
fn a_struct_returned_by_value_is_the_answer_location_s_words() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn mk(a: Int) -> Point { Point(x: a, y: a) }\nfn f() -> Point { mk(3) }",
            "f"
        ),
        "\
fn0 m.f() -> m.Point
  frame 5: s0:int s1:int s2:int s3:int s4:int
     0  int s2:int 3
     1  call s3:int m.mk (s2:Int)
     2  copy s0:int s3:int m.Point
     3  return s0:int
"
    );
}
