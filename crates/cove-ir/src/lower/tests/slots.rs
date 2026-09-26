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
  frame 12: s0!:ref s1!:ref s2:int s3:int s4:ref s5:unit s6:ref s7:unit s8:int s9:int s10:int s11:ref
  local a -> s0:String [0, 22)
  local b -> s1:String [0, 22)
     0  int s3:int 32
     1  growable-alloc.bytes s4:ref s3:int
     2  len s8:int s0:ref
     3  load-field s9:Int s4:ref +0
     4  growable-ensure.bytes s4:ref s8:int
     5  int s10:int 0
     6  load-field s11:<ref> s4:ref +1
     7  run-copy.bytes (s11:<ref> s9:Int s0:String s10:Int s8:Int)
     8  clear s11:<ref>
     9  growable-commit.bytes s4:ref s8:int
    10  len s8:int s1:ref
    11  load-field s9:Int s4:ref +0
    12  growable-ensure.bytes s4:ref s8:int
    13  int s10:int 0
    14  load-field s11:<ref> s4:ref +1
    15  run-copy.bytes (s11:<ref> s9:Int s1:String s10:Int s8:Int)
    16  clear s11:<ref>
    17  growable-commit.bytes s4:ref s8:int
    18  run-finish.bytes s6:ref s4:ref String utf8
    19  clear s4:ByteBuffer
    20  call s2:Int std.string.length (s6:String)
    21  return s2:Int
"
    );
}

/// The map says which slots a collection *reads*; only the data can say
/// when the value in one stopped being needed.
///
/// The string is interpolated rather than written down, because a literal's
/// object is placed once, before the run begins, and never collected — see
/// ADR 0045 — so clearing a slot that holds one releases nothing and
/// `lower::frees` drops it. And the scope is followed by a call, because a
/// clear with nothing but quiet work between it and the `return` is one the
/// `return` renders pointless, and `lower::redefined` drops that too.
#[test]
fn a_local_holding_a_reference_is_cleared_when_its_scope_ends() {
    assert_eq!(
        listing(
            "fn f(what: String) -> Int {\n  var n = 0\n  {\n    let s = \"{what}!\"\n    n = s.length()\n  }\n  n + what.length()\n}",
            "f"
        ),
        "\
fn @m.f(String) -> Int
  frame 17: s0!:ref s1:int s2:int s3:int s4:ref s5:unit s6:ref s7:unit s8:int s9:int s10:int s11:ref s12:unit s13:int s14:int s15:ref s16:int
  local what -> s0:String [0, 27)
  local n -> s2:Int [1, 26)
  local s -> s6:String [22, 23)
     0  int s2:int 0
     1  int s3:int 17
     2  growable-alloc.bytes s4:ref s3:int
     3  len s8:int s0:ref
     4  load-field s9:Int s4:ref +0
     5  growable-ensure.bytes s4:ref s8:int
     6  int s10:int 0
     7  load-field s11:<ref> s4:ref +1
     8  run-copy.bytes (s11:<ref> s9:Int s0:String s10:Int s8:Int)
     9  clear s11:<ref>
    10  growable-commit.bytes s4:ref s8:int
    11  int s3:int 33
    12  load-field s13:Int s4:ref +0
    13  int s14:int 1
    14  growable-ensure.bytes s4:ref s14:int
    15  load-field s15:<ref> s4:ref +1
    16  run-store.bytes s15:ref s13:int s3:int
    17  clear s15:<ref>
    18  int s16:int 1
    19  growable-commit.bytes s4:ref s16:int
    20  run-finish.bytes s6:ref s4:ref String utf8
    21  clear s4:ByteBuffer
    22  call s2:Int std.string.length (s6:String)
    23  clear s6:String
    24  call s3:Int std.string.length (s0:String)
    25  add.int s1:int s2:int s3:int
    26  return s1:Int
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
/// The scope, and the call after it, are what keep it: a clear the `return`
/// renders pointless — straight after it, or after nothing but quiet work —
/// is dropped by `lower::tails` or `lower::redefined`, and this one is about
/// the instruction rather than about where it stands.
#[test]
fn a_location_with_one_reference_word_among_scalars_is_cleared_whole() {
    assert_eq!(
        listing(
            "struct User { name: String, age: Int }\nfn f() -> Int {\n  var n = 0\n  {\n    let u = User(name: \"a\", age: 1)\n    n = u.age\n  }\n  n + \"ab\".length()\n}",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 6: s0:int s1:int s2:ref s3:int s4:ref s5:int
  local n -> s1:Int [1, 10)
  local u -> s4..s5:m.User [5, 6)
     0  int s1:int 0
     1  str s2:ref \"a\"
     2  int s3:int 1
     3  copy s4:String s2:String
     4  copy s5:Int s3:Int
     5  copy s1:Int s5:Int
     6  clear s4..s5:m.User
     7  str s2:ref \"ab\"
     8  call s3:Int std.string.length (s2:String)
     9  add.int s0:int s1:int s3:int
    10  return s0:Int
"
    );
}
