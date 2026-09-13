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
fn @m.f(m.E) -> Int
  frame 5: s0!:tag s1!:tag s2!:int s3:int s4:int
  local e -> s0..s2:m.E [0, 15)
  local n -> s4:Int [4, 5)
     0  switch s0:tag [1 11] else 13
     1  switch s1:tag [2 3] else 2
     2  jump 6
     3  copy s4:Int s2:Int
     4  copy s3:Int s4:Int
     5  jump 14
     6  switch s1:tag [8 7] else 7
     7  jump 13
     8  int s4:int 1
     9  neg.int s3:int s4:int
    10  jump 14
    11  int s3:int 0
    12  jump 14
    13  trap \"no `match` arm covers this value\"
    14  return s3:Int
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
fn @m.f(m.Msg) -> String
  frame 4: s0!:tag s1!:ref s2:ref s3:ref
  local m -> s0..s1:m.Msg [0, 9)
  local s -> s3:String [2, 3)
     0  switch s0:tag [5 1] else 7
     1  copy s3:String s1:String
     2  copy s2:String s3:String
     3  clear s3:String
     4  jump 8
     5  str s2:ref \"\"
     6  jump 8
     7  trap \"no `match` arm covers this value\"
     8  return s2:String
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
fn @m.name(Int) -> String
  frame 3: s0!:int s1:ref s2:bool
  local n -> s0:Int [0, 10)
     0  eq.int.imm.branch s2:bool s0:int 0 3
     1  str s1:ref \"zero\"
     2  jump 9
     3  eq.int.imm.branch s2:bool s0:int 1 6
     4  str s1:ref \"one\"
     5  jump 9
     6  str s1:ref \"many\"
     7  jump 9
     8  trap \"no `match` arm covers this value\"
     9  return s1:String
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
fn @m.score(String) -> Int
  frame 4: s0!:ref s1:int s2:ref s3:bool
  local s -> s0:String [0, 8)
     0  str s2:ref \"a\"
     1  eq.str.branch s3:bool s0:ref s2:ref 4
     2  int s1:int 1
     3  jump 7
     4  int s1:int 0
     5  jump 7
     6  trap \"no `match` arm covers this value\"
     7  return s1:Int
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
fn @m.f(m.Shape) -> Int
  frame 5: s0!:tag s1!:int s2!:int s3:int s4:int
  local s -> s0..s2:m.Shape [0, 8)
  local a -> s4:Int [2, 3)
     0  switch s0:tag [4 1 4] else 6
     1  copy s4:Int s1:Int
     2  copy s3:Int s4:Int
     3  jump 7
     4  int s3:int 0
     5  jump 7
     6  trap \"no `match` arm covers this value\"
     7  return s3:Int
"
    );
}
