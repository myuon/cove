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
fn @m.total() -> Int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 1
     1  add.int.imm s2:int s1:int 2
     2  add.int.imm s1:int s2:int 3
     3  add.int.imm s0:int s1:int 4
     4  return s0:Int
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
fn @m.mix(Int Float) -> Float
  frame 7: s0!:int s1!:float s2:float s3:int s4:float s5:float s6:int
  local a -> s0:Int [0, 6)
  local b -> s1:Float [0, 6)
  local n -> s3:Int [1, 5)
  local x -> s5:Float [3, 5)
  local m -> s6:Int [4, 5)
     0  add.int.imm s3:int s0:int 1
     1  float s4:float 1
     2  add.float s5:float s1:float s4:float
     3  add.int.imm s6:int s3:int 2
     4  copy s2:Float s5:Float
     5  return s2:Float
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
fn @m.f() -> Int
  frame 7: s0:int s1:int s2:ref s3:int s4:ref s5:ref s6:int
  local a -> s3..s4:m.A [4, 9)
  local b -> s5..s6:m.B [8, 9)
     0  int s1:int 1
     1  str s2:ref \"x\"
     2  copy s3:Int s1:Int
     3  copy s4:String s2:String
     4  str s2:ref \"y\"
     5  int s1:int 2
     6  copy s5:String s2:String
     7  copy s6:Int s1:Int
     8  add.int s0:int s3:int s6:int
     9  return s0:Int
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
fn @m.shout(String String) -> Int
  frame 4: s0!:ref s1!:ref s2:int s3:ref
  local a -> s0:String [0, 3)
  local b -> s1:String [0, 3)
     0  call-builtin s3:String String.interpolate (s0:String s1:String)
     1  call-builtin s2:Int String.length (s3:String)
     2  return s2:Int
"
    );
}

/// The map says which slots a collection *reads*; only the data can say
/// when the value in one stopped being needed.
///
/// The string is interpolated rather than written down, because a literal's
/// object is placed once, before the run begins, and never collected — see
/// ADR 0045 — so clearing a slot that holds one releases nothing and
/// `lower::frees` drops it.
#[test]
fn a_local_holding_a_reference_is_cleared_when_its_scope_ends() {
    assert_eq!(
        listing(
            "fn f(what: String) -> Int {\n  var n = 0\n  {\n    let s = \"{what}!\"\n    n = s.length()\n  }\n  n\n}",
            "f"
        ),
        "\
fn @m.f(String) -> Int
  frame 5: s0!:ref s1:int s2:int s3:ref s4:ref
  local what -> s0:String [0, 7)
  local n -> s2:Int [1, 6)
  local s -> s4:String [3, 4)
     0  int s2:int 0
     1  str s3:ref \"!\"
     2  call-builtin s4:String String.interpolate (s0:String s3:String)
     3  call-builtin s2:Int String.length (s4:String)
     4  clear s4:String
     5  copy s1:Int s2:Int
     6  return s1:Int
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
fn @m.f() -> Int
  frame 3: s0:int s1:int s2:int
  local a -> s1:Int [1, 3)
  local b -> s2:Int [2, 3)
     0  int s1:int 1
     1  int s2:int 2
     2  add.int s0:int s1:int s2:int
     3  return s0:Int
"
    );
}

/// `Clear` takes a layout and zeroes the location's words, so a struct
/// with a string in it is ended by one instruction rather than by one per
/// field.
///
/// The scope is what keeps it: a clear the `return` renders pointless is
/// dropped by `lower::tails`, and this one is about the instruction rather
/// than about where it stands.
#[test]
fn a_location_with_one_reference_word_among_scalars_is_cleared_whole() {
    assert_eq!(
        listing(
            "struct User { name: String, age: Int }\nfn f() -> Int {\n  var n = 0\n  {\n    let u = User(name: \"a\", age: 1)\n    n = u.age\n  }\n  n\n}",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 6: s0:int s1:int s2:ref s3:int s4:ref s5:int
  local n -> s1:Int [1, 8)
  local u -> s4..s5:m.User [5, 6)
     0  int s1:int 0
     1  str s2:ref \"a\"
     2  int s3:int 1
     3  copy s4:String s2:String
     4  copy s5:Int s3:Int
     5  copy s1:Int s5:Int
     6  clear s4..s5:m.User
     7  copy s0:Int s1:Int
     8  return s0:Int
"
    );
}
