//! What the frame is made of: reuse, and where a reference's live range
//! ends.
//!
//! Two invariants are pinned here, and both are what keeps a *static*
//! reference map correct and cheap. A run of slots is handed on only to a
//! value whose words are the same, word for word — so no single bit of
//! [`crate::RefMap`](crate::RefMap) is ever wrong at any program counter.
//! And a location holding a reference is cleared at its last use, so the map
//! costs no retention beyond a value's live range.

use super::listing;

/// A long body mentions far more temporaries than it holds at once, so a
/// frame should grow with what is live rather than with the source. Every
/// one of the four literals below is the same slot.
#[test]
fn a_run_is_reused_by_a_later_value_of_the_same_words() {
    assert_eq!(
        listing("fn total() -> Int { ((1 + 2) + 3) + 4 }", "total"),
        "\
fn0 m.total() -> Int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 1
     1  int s2:int 2
     2  add.int s3:int s1:int s2:int
     3  int s1:int 3
     4  add.int s2:int s3:int s1:int
     5  int s3:int 4
     6  add.int s1:int s2:int s3:int
     7  copy s0:int s1:int Int
     8  return s0:int
"
    );
}

/// The `Int` temporaries and the `Float` ones draw from different lists,
/// because one bit per slot has to be right for the whole function.
#[test]
fn a_run_is_never_reused_by_a_value_whose_words_differ() {
    assert_eq!(
        listing(
            "fn mix(a: Int, b: Float) -> Float {\n  let n = a + 1\n  let x = b + 1.0\n  let m = n + 2\n  x\n}",
            "mix"
        ),
        "\
fn0 m.mix(Int Float) -> Float
  frame 8: s0!:int s1!:float s2:float s3:int s4:int s5:float s6:float s7:int
  local a -> s0:Int [0, 8)
  local b -> s1:Float [0, 8)
  local n -> s4:Int [2, 7)
  local x -> s6:Float [4, 7)
  local m -> s7:Int [6, 7)
     0  int s3:int 1
     1  add.int s4:int s0:int s3:int
     2  float s5:float 1
     3  add.float s6:float s1:float s5:float
     4  int s3:int 2
     5  add.int s7:int s4:int s3:int
     6  copy s2:float s6:float Float
     7  return s2:float
"
    );
}

/// A `[Int, Ref]` and a `[Ref, Int]` are the same width and never share a
/// run, because the map would then be wrong for one of them.
#[test]
fn a_two_word_location_is_reused_only_by_a_two_word_one_of_the_same_shape() {
    assert_eq!(
        listing(
            "struct A { n: Int, s: String }\nstruct B { s: String, n: Int }\nfn f() -> Int {\n  let a = A(n: 1, s: \"x\")\n  let b = B(s: \"y\", n: 2)\n  a.n + b.n\n}",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 7: s0:int s1:int s2:ref s3:int s4:ref s5:ref s6:int
  local a -> s3:m.A [5, 12)
  local b -> s5:m.B [10, 12)
     0  int s1:int 1
     1  str s2:ref \"x\"
     2  copy s3:int s1:int Int
     3  copy s4:ref s2:ref String
     4  clear s2:ref String
     5  str s2:ref \"y\"
     6  int s1:int 2
     7  copy s5:ref s2:ref String
     8  copy s6:int s1:int Int
     9  clear s2:ref String
    10  add.int s1:int s3:int s6:int
    11  copy s0:int s1:int Int
    12  clear s3:int m.A
    13  clear s5:ref m.B
    14  return s0:int
"
    );
}

/// Without it, a body that built one string per turn would retain every
/// one of them until it returned.
#[test]
fn a_temporary_holding_a_reference_is_cleared_at_its_last_use() {
    assert_eq!(
        listing(
            "fn shout(a: String, b: String) -> Int { \"{a}{b}\".length() }",
            "shout"
        ),
        "\
fn0 m.shout(String String) -> Int
  frame 5: s0!:ref s1!:ref s2:int s3:ref s4:int
  local a -> s0:String [0, 5)
  local b -> s1:String [0, 5)
     0  call-builtin s3:ref String.interpolate (s0:String s1:String)
     1  call-builtin s4:int String.length (s3:String)
     2  clear s3:ref String
     3  copy s2:int s4:int Int
     4  return s2:int
"
    );
}

/// The map says which slots a collection *reads*; only the data can say
/// when the value in one stopped being needed.
#[test]
fn a_local_holding_a_reference_is_cleared_when_its_scope_ends() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var n = 0\n  {\n    let s = \"held\"\n    n = s.length()\n  }\n  n\n}",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:ref s3:int
  local n -> s1:Int [1, 6)
  local s -> s2:String [2, 4)
     0  int s1:int 0
     1  str s2:ref \"held\"
     2  call-builtin s3:int String.length (s2:String)
     3  copy s1:int s3:int Int
     4  clear s2:ref String
     5  copy s0:int s1:int Int
     6  return s0:int
"
    );
}

/// It costs one store on a path that was going to leave the value behind
/// anyway, and it is emitted only where the location would otherwise
/// retain something.
#[test]
fn a_scalar_is_never_cleared() {
    assert_eq!(
        listing("fn f() -> Int {\n  let a = 1\n  let b = 2\n  a + b\n}", "f"),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local a -> s1:Int [1, 4)
  local b -> s2:Int [2, 4)
     0  int s1:int 1
     1  int s2:int 2
     2  add.int s3:int s1:int s2:int
     3  copy s0:int s3:int Int
     4  return s0:int
"
    );
}

/// `Clear` takes a layout and zeroes the location's words, so a struct
/// with a string in it is ended by one instruction rather than by one per
/// field.
#[test]
fn a_location_with_one_reference_word_among_scalars_is_cleared_whole() {
    assert_eq!(
        listing(
            "struct User { name: String, age: Int }\nfn f() -> Int {\n  let u = User(name: \"a\", age: 1)\n  u.age\n}",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 5: s0:int s1:ref s2:int s3:ref s4:int
  local u -> s3:m.User [5, 6)
     0  str s1:ref \"a\"
     1  int s2:int 1
     2  copy s3:ref s1:ref String
     3  copy s4:int s2:int Int
     4  clear s1:ref String
     5  copy s0:int s4:int Int
     6  clear s3:ref m.User
     7  return s0:int
"
    );
}
