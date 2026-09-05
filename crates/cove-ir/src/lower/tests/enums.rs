//! Enums, which are a discriminant word and a payload region.

use super::listing;

/// `docs/LINEAR_VM.md`'s fourth worked case. `Shape` is `[disc, Int,
/// Int]`: `Dot` writes the discriminant and zeroes the rest, and `Box(3, 4)`
/// writes all three. The zeroing is not tidiness — the payload region's
/// reference map is static, so a word another case would put a reference
/// in has to read null.
///
/// The `Box` in the inner scope is what makes the zeroing *visible*, and it
/// is there for that. `Memory::push_frame` reserves a frame with
/// `resize(…, 0)`, so a payload word this frame has not written is already
/// null and `lower::frees` drops a clear that would zero it again. Here the
/// scope that closed left two payload words behind, and the run they occupy
/// is the one `Dot` is built in.
#[test]
fn a_case_writes_the_discriminant_and_zeroes_what_it_does_not_fill() {
    assert_eq!(
        listing(
            "enum Shape { Dot, Line(Int), Box(Int, Int) }\nfn f() -> Shape {\n  {\n    let wide = Shape.Box(3, 4)\n  }\n  Shape.Dot\n}",
            "f"
        ),
        "\
fn0 m.f() -> m.Shape
  frame 8: s0:int s1:int s2:int s3:int s4:int s5:int s6:int s7:int
  local wide -> s5:m.Shape [5, 5)
     0  int s3:int 3
     1  int s4:int 4
     2  int s5:int 2
     3  copy s6:int s3:int Int
     4  copy s7:int s4:int Int
     5  int s5:int 0
     6  clear s6:int Int
     7  clear s7:int Int
     8  copy s0:int s5:int m.Shape
     9  return s0:int m.Shape
"
    );
}

#[test]
fn a_case_that_fills_the_region_zeroes_nothing() {
    assert_eq!(
        listing(
            "enum Shape { Dot, Line(Int), Box(Int, Int) }\nfn f() -> Shape { Shape.Box(3, 4) }",
            "f"
        ),
        "\
fn0 m.f() -> m.Shape
  frame 8: s0:int s1:int s2:int s3:int s4:int s5:int s6:int s7:int
     0  int s3:int 3
     1  int s4:int 4
     2  int s5:int 2
     3  copy s6:int s3:int Int
     4  copy s7:int s4:int Int
     5  copy s0:int s5:int m.Shape
     6  return s0:int m.Shape
"
    );
}

/// `enum Msg { Ping, Text(String) }` is `[disc, Ref]`, and `Ping` leaves
/// the reference word null — so the collector reads null rather than a
/// stale address, without ever looking at the discriminant.
///
/// The `Text` in the inner scope puts an address in that word first, which
/// is what leaves `Ping` something to zero: a word the frame has never
/// written is already null, and `lower::frees` drops the clear that would
/// write null over it. Its string is interpolated rather than a literal for
/// the same reason — a literal is interned, and clearing a slot that holds
/// an interned string releases nothing either.
#[test]
fn a_reference_word_of_another_case_reads_null() {
    assert_eq!(
        listing(
            "enum Msg { Ping, Text(String) }\nfn f(what: String) -> Msg {\n  {\n    let said = Msg.Text(\"{what}!\")\n  }\n  Msg.Ping\n}",
            "f"
        ),
        "\
fn0 m.f(String) -> m.Msg
  frame 7: s0!:ref s1:int s2:ref s3:ref s4:ref s5:int s6:ref
  local what -> s0:String [0, 10)
  local said -> s5:m.Msg [5, 5)
     0  str s3:ref \"!\"
     1  call-builtin s4:ref String.interpolate (s0:String s3:String) String
     2  int s5:int 1
     3  copy s6:ref s4:ref String
     4  clear s4:ref String
     5  clear s5:int m.Msg
     6  int s5:int 0
     7  clear s6:ref <ref>
     8  copy s1:int s5:int m.Msg
     9  return s1:int m.Msg
"
    );
}

/// `A(Int, String)` and `B(Float)`: `A` takes payload words 0 and 1, and
/// `B` can use neither — an `Int` is not a `Float` and a `Ref` is not a
/// `Float` — so its payload takes a third. The value is four words, wider
/// than either case, and that is the price of a map a collection can read
/// without asking which case it is in.
#[test]
fn the_payload_words_of_two_cases_agree_or_do_not_overlap() {
    assert_eq!(
        listing(
            "enum E { A(Int, String), B(Float) }\nfn f(x: Float) -> E { E.B(x) }",
            "f"
        ),
        "\
fn0 m.f(Float) -> m.E
  frame 9: s0!:float s1:int s2:int s3:ref s4:float s5:int s6:int s7:ref s8:float
  local x -> s0:Float [0, 4)
     0  int s5:int 1
     1  copy s8:float s0:float Float
     2  copy s1:int s5:int m.E
     3  return s1:int m.E
"
    );
}

/// The discriminant is word 0 of the value, already in the frame, so the
/// switch names the location itself and nothing is read out of anything.
/// Each arm's payload is at `base + 1 + Part::at`, which is why the arms
/// below contain no loads either.
#[test]
fn a_match_reads_the_discriminant_at_offset_zero() {
    assert_eq!(
        listing(
            "enum Shape { Dot, Line(Int), Box(Int, Int) }\nfn f(s: Shape) -> Int {\n  match s {\n    Shape.Dot => 0,\n    Shape.Line(a) => a,\n    Shape.Box(a, b) => a + b,\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(m.Shape) -> Int
  frame 8: s0!:int s1!:int s2!:int s3:int s4:int s5:int s6:int s7:int
  local s -> s0:m.Shape [0, 15)
  local a -> s5:Int [5, 6)
  local a -> s5:Int [8, 11)
  local b -> s6:Int [9, 11)
     0  switch s0:int [1 4 7] else 12
     1  int s5:int 0
     2  copy s4:int s5:int Int
     3  jump 13
     4  copy s5:int s1:int Int
     5  copy s4:int s5:int Int
     6  jump 13
     7  copy s5:int s1:int Int
     8  copy s6:int s2:int Int
     9  add.int s7:int s5:int s6:int
    10  copy s4:int s7:int Int
    11  jump 13
    12  trap \"no `match` arm covers this value\"
    13  copy s3:int s4:int Int
    14  return s3:int Int
"
    );
}

#[test]
fn an_option_is_two_words_and_none_is_the_zeroed_one() {
    assert_eq!(
        listing(
            "fn f(o: Option<Int>) -> Int {\n  match o {\n    Some(v) => v,\n    None => 0,\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(Option) -> Int
  frame 5: s0!:int s1!:int s2:int s3:int s4:int
  local o -> s0:Option [0, 10)
  local v -> s4:Int [2, 3)
     0  switch s0:int [4 1] else 7
     1  copy s4:int s1:int Int
     2  copy s3:int s4:int Int
     3  jump 8
     4  int s4:int 0
     5  copy s3:int s4:int Int
     6  jump 8
     7  trap \"no `match` arm covers this value\"
     8  copy s2:int s3:int Int
     9  return s2:int Int
"
    );
}

/// A copy of an enum is a copy of the discriminant and the whole payload
/// region, which is what makes the copy independent of what it came from.
#[test]
fn a_case_is_copied_whole() {
    assert_eq!(
        listing(
            "enum Shape { Dot, Line(Int), Box(Int, Int) }\nfn f(s: Shape) -> Shape { let t = s\n t }",
            "f"
        ),
        "\
fn0 m.f(m.Shape) -> m.Shape
  frame 9: s0!:int s1!:int s2!:int s3:int s4:int s5:int s6:int s7:int s8:int
  local s -> s0:m.Shape [0, 3)
  local t -> s6:m.Shape [1, 2)
     0  copy s6:int s0:int m.Shape
     1  copy s3:int s6:int m.Shape
     2  return s3:int m.Shape
"
    );
}

/// The value `?` was applied to is a `Result` of some other pair of types,
/// so the `Err` it leaves through is built here rather than passed along —
/// two `Result`s whose words differ are two layouts, and reusing the value
/// would hand the caller one whose payload is not what its layout says.
#[test]
fn a_question_mark_leaves_through_the_enclosing_function_s_own_failure() {
    assert_eq!(
        listing(
            "fn g() -> Result<Int, Error> { Ok(1) }\nfn f() -> Result<Int, Error> {\n  let v = g()?\n  Ok(v + 1)\n}",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 12: s0:int s1:int s2:ref s3:int s4:int s5:ref s6:int s7:bool s8:int s9:int s10:ref s11:int
  local v -> s6:Int [10, 15)
     0  call s3:int m.g () Result
     1  int s6:int 0
     2  eq.int s7:bool s3:int s6:int
     3  branch-false s7:bool 6
     4  copy s6:int s4:int Int
     5  jump 9
     6  int s8:int 1
     7  copy s10:ref s5:ref Error
     8  return s8:int Result
     9  clear s3:int Result
    10  add.int.imm s11:int s6:int 1
    11  int s3:int 0
    12  clear s5:ref <ref>
    13  copy s4:int s11:int Int
    14  copy s0:int s3:int Result
    15  return s0:int Result
"
    );
}

#[test]
fn a_question_mark_on_an_option_leaves_through_none() {
    assert_eq!(
        listing(
            "fn g() -> Option<Int> { Some(1) }\nfn f() -> Option<Int> {\n  let v = g()?\n  Some(v + 1)\n}",
            "f"
        ),
        "\
fn0 m.f() -> Option
  frame 9: s0:int s1:int s2:int s3:int s4:int s5:bool s6:int s7:int s8:int
  local v -> s4:Int [8, 12)
     0  call s2:int m.g () Option
     1  int s4:int 1
     2  eq.int s5:bool s2:int s4:int
     3  branch-false s5:bool 6
     4  copy s4:int s3:int Int
     5  jump 8
     6  int s6:int 0
     7  return s6:int Option
     8  add.int.imm s8:int s4:int 1
     9  int s2:int 1
    10  copy s3:int s8:int Int
    11  copy s0:int s2:int Option
    12  return s0:int Option
"
    );
}

/// Nesting is inline and recursive, so a `Wrapper` holding an enum is the
/// enum's words followed by the rest of the fields, and the `match` reads
/// the discriminant at the field's own offset.
#[test]
fn an_enum_inside_a_struct_is_inline_there_too() {
    assert_eq!(
        listing(
            "enum E { A, B(Int) }\nstruct S { e: E, n: Int }\nfn f(s: S) -> Int {\n  match s.e {\n    E.A => 0,\n    E.B(v) => v + s.n,\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(m.S) -> Int
  frame 7: s0!:int s1!:int s2!:int s3:int s4:int s5:int s6:int
  local s -> s0:m.S [0, 11)
  local v -> s5:Int [5, 7)
     0  switch s0:int [1 4] else 8
     1  int s5:int 0
     2  copy s4:int s5:int Int
     3  jump 9
     4  copy s5:int s1:int Int
     5  add.int s6:int s5:int s2:int
     6  copy s4:int s6:int Int
     7  jump 9
     8  trap \"no `match` arm covers this value\"
     9  copy s3:int s4:int Int
    10  return s3:int Int
"
    );
}
