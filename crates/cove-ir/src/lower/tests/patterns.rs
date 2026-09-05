//! `match`, and the patterns its arms are written with.

use super::listing;

/// A case's payload is part of the value, so a sub-pattern is tested
/// against `base + 1 + Part::at` directly. Nothing is copied out to be
/// looked at, which is why a failing arm has nothing to clear on its way
/// to the next one.
#[test]
fn a_nested_pattern_tests_the_payload_where_it_already_is() {
    assert_eq!(
        listing(
            "enum E { A(Option<Int>), B }\nfn f(e: E) -> Int {\n  match e {\n    E.A(Some(n)) => n,\n    E.A(None) => -1,\n    E.B => 0,\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(m.E) -> Int
  frame 8: s0!:int s1!:int s2!:int s3:int s4:int s5:int s6:bool s7:int
  local e -> s0:m.E [0, 20)
  local n -> s5:Int [5, 6)
     0  switch s0:int [1 14] else 17
     1  int s5:int 1
     2  eq.int s6:bool s1:int s5:int
     3  branch-false s6:bool 7
     4  copy s5:int s2:int Int
     5  copy s4:int s5:int Int
     6  jump 18
     7  int s5:int 0
     8  eq.int s6:bool s1:int s5:int
     9  branch-false s6:bool 17
    10  int s5:int 1
    11  neg.int s7:int s5:int
    12  copy s4:int s7:int Int
    13  jump 18
    14  int s7:int 0
    15  copy s4:int s7:int Int
    16  jump 18
    17  trap \"no `match` arm covers this value\"
    18  copy s3:int s4:int Int
    19  return s3:int Int
"
    );
}

/// It has to be a copy: the binding belongs to the arm's scope and is
/// cleared when that scope ends, and clearing a borrowed part of the value
/// being matched would zero the value itself.
#[test]
fn a_binding_is_a_copy_of_the_words_it_names() {
    assert_eq!(
        listing(
            "enum Msg { Ping, Text(String) }\nfn f(m: Msg) -> String {\n  match m {\n    Msg.Text(s) => s,\n    Msg.Ping => \"\",\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(m.Msg) -> String
  frame 5: s0!:int s1!:ref s2:ref s3:ref s4:ref
  local m -> s0:m.Msg [0, 11)
  local s -> s4:String [2, 3)
     0  switch s0:int [5 1] else 8
     1  copy s4:ref s1:ref String
     2  copy s3:ref s4:ref String
     3  clear s4:ref String
     4  jump 9
     5  str s4:ref \"\"
     6  copy s3:ref s4:ref String
     7  jump 9
     8  trap \"no `match` arm covers this value\"
     9  copy s2:ref s3:ref String
    10  return s2:ref String
"
    );
}

/// There is no index to switch on, and the arms' literals are values
/// rather than a dense numbering.
#[test]
fn a_match_over_something_that_is_not_an_enum_is_a_chain() {
    assert_eq!(
        listing(
            "fn name(n: Int) -> String {\n  match n {\n    0 => \"zero\",\n    1 => \"one\",\n    _ => \"many\",\n  }\n}",
            "name"
        ),
        "\
fn0 m.name(Int) -> String
  frame 5: s0!:int s1:ref s2:ref s3:bool s4:ref
  local n -> s0:Int [0, 16)
     0  eq.int.imm s3:bool s0:int 0
     1  branch-false s3:bool 5
     2  str s4:ref \"zero\"
     3  copy s2:ref s4:ref String
     4  jump 14
     5  eq.int.imm s3:bool s0:int 1
     6  branch-false s3:bool 10
     7  str s4:ref \"one\"
     8  copy s2:ref s4:ref String
     9  jump 14
    10  str s4:ref \"many\"
    11  copy s2:ref s4:ref String
    12  jump 14
    13  trap \"no `match` arm covers this value\"
    14  copy s1:ref s2:ref String
    15  return s1:ref String
"
    );
}

#[test]
fn a_match_over_strings_compares_bytes() {
    assert_eq!(
        listing(
            "fn score(s: String) -> Int {\n  match s {\n    \"a\" => 1,\n    _ => 0,\n  }\n}",
            "score"
        ),
        "\
fn0 m.score(String) -> Int
  frame 6: s0!:ref s1:int s2:int s3:ref s4:bool s5:int
  local s -> s0:String [0, 12)
     0  str s3:ref \"a\"
     1  eq.str s4:bool s0:ref s3:ref
     2  branch-false s4:bool 6
     3  int s5:int 1
     4  copy s2:int s5:int Int
     5  jump 10
     6  int s5:int 0
     7  copy s2:int s5:int Int
     8  jump 10
     9  trap \"no `match` arm covers this value\"
    10  copy s1:int s2:int Int
    11  return s1:int Int
"
    );
}

/// A `_` arm is the tail of every case's chain, so nothing after it is
/// reachable for any of them.
#[test]
fn an_arm_that_covers_every_case_ends_each_chain() {
    assert_eq!(
        listing(
            "enum Shape { Dot, Line(Int), Box(Int, Int) }\nfn f(s: Shape) -> Int {\n  match s {\n    Shape.Line(a) => a,\n    _ => 0,\n  }\n}",
            "f"
        ),
        "\
fn0 m.f(m.Shape) -> Int
  frame 6: s0!:int s1!:int s2!:int s3:int s4:int s5:int
  local s -> s0:m.Shape [0, 10)
  local a -> s5:Int [2, 3)
     0  switch s0:int [4 1 4] else 7
     1  copy s5:int s1:int Int
     2  copy s4:int s5:int Int
     3  jump 8
     4  int s5:int 0
     5  copy s4:int s5:int Int
     6  jump 8
     7  trap \"no `match` arm covers this value\"
     8  copy s3:int s4:int Int
     9  return s3:int Int
"
    );
}
