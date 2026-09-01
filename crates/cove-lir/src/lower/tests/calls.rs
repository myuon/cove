//! Calls, and the frame boundary they have to match.

use super::listing;

/// The machine copies each argument's words into the callee's frame,
/// which begins where this one ends. Nothing is pushed, permuted or
/// copied back.
#[test]
fn a_call_names_the_arguments_and_the_destination_location() {
    assert_eq!(
        listing(
            "fn add(a: Int, b: Int) -> Int { a + b }\nfn f() -> Int { add(1, 2) }",
            "f"
        ),
        "\
fn1 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 1
     1  int s2:int 2
     2  call s3:int m.add (s1:Int s2:Int)
     3  copy s0:int s3:int Int
     4  return s0:int
"
    );
}

#[test]
fn recursion_is_an_ordinary_call() {
    assert_eq!(
        listing(
            "fn fib(n: Int) -> Int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }",
            "fib"
        ),
        "\
fn0 m.fib(Int) -> Int
  frame 7: s0!:int s1:int s2:int s3:int s4:bool s5:int s6:int
     0  int s3:int 2
     1  lt.int s4:bool s0:int s3:int
     2  branch-false s4:bool 5
     3  copy s2:int s0:int Int
     4  jump 13
     5  int s3:int 1
     6  sub.int s5:int s0:int s3:int
     7  call s3:int m.fib (s5:Int)
     8  int s5:int 2
     9  sub.int s6:int s0:int s5:int
    10  call s5:int m.fib (s6:Int)
    11  add.int s6:int s3:int s5:int
    12  copy s2:int s6:int Int
    13  copy s1:int s2:int Int
    14  return s1:int
"
    );
}

/// `docs/LINEAR_VM.md`'s fifth worked case: a `(Int, Point, Int)` list
/// occupies slots 0, 1–2 and 3. A mixed list is not sorted into type
/// groups; there are no type groups.
#[test]
fn multiword_parameters_occupy_the_frame_from_slot_zero_in_order() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn take(a: Int, p: Point, b: Int) -> Int { a + p.x + p.y + b }",
            "take"
        ),
        "\
fn0 m.take(Int m.Point Int) -> Int
  frame 7: s0!:int s1!:int s2!:int s3!:int s4:int s5:int s6:int
     0  add.int s5:int s0:int s1:int
     1  add.int s6:int s5:int s2:int
     2  add.int s5:int s6:int s3:int
     3  copy s4:int s5:int Int
     4  return s4:int
"
    );
}

#[test]
fn a_call_passing_a_multiword_argument_names_its_base_slot() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn take(a: Int, p: Point, b: Int) -> Int { a + p.x + p.y + b }\nfn f() -> Int { take(1, Point(x: 2, y: 3), 4) }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 6: s0:int s1:int s2:int s3:int s4:int s5:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  copy s4:int s2:int Int
     4  copy s5:int s3:int Int
     5  int s2:int 4
     6  call s3:int m.take (s1:Int s4:m.Point s2:Int)
     7  copy s0:int s3:int Int
     8  return s0:int
"
    );
}

/// `bump(var total)` writes the caller's own words: the parameter is an
/// ordinary slot whose `Repr` is `Addr`, and there is no copy back.
#[test]
fn a_var_parameter_is_a_slot_holding_an_address() {
    assert_eq!(
        listing("fn bump(var n: Int) { n = n + 1 }", "bump"),
        "\
fn0 m.bump(<addr>) -> Unit
  frame 6: s0!:addr s1:unit s2:int s3:int s4:int s5:unit
     0  load s2:int s0:addr Int
     1  int s3:int 1
     2  add.int s4:int s2:int s3:int
     3  store s0:addr s4:int Int
     4  unit s5:unit
     5  copy s1:unit s5:unit Unit
     6  return s1:unit
"
    );
}

#[test]
fn a_var_argument_is_the_address_of_the_caller_s_location() {
    assert_eq!(
        listing(
            "fn bump(var n: Int) { n = n + 1 }\nfn f() -> Int {\n  var total = 0\n  bump(var total)\n  total\n}",
            "f"
        ),
        "\
fn1 m.f() -> Int
  frame 4: s0:int s1:int s2:addr s3:unit
     0  int s1:int 0
     1  addr-of-slot s2:addr s1:int
     2  call s3:unit m.bump (s2:<addr>)
     3  clear s2:addr <addr>
     4  copy s0:int s1:int Int
     5  return s0:int
"
    );
}

/// A field of a `var` parameter is that parameter's address plus the field's
/// offset, and a write through it is one store of the field's words.
///
/// Both were out of reach while a place could only be the *first* word of a
/// value location: `p.y = 7` was a load of the whole `Point`, a write into
/// the words and a store of the whole `Point` back, and `bump(var p.y)` could
/// not be lowered at all because there was no way to form the address.
#[test]
fn a_field_of_a_var_parameter_is_that_address_plus_the_offset() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn bump(var n: Int) { n = n + 1 }\nfn shift(var p: Point) {\n  p.y = 7\n  bump(var p.y)\n}",
            "shift"
        ),
        "\
fn1 m.shift(<addr>) -> Unit
  frame 5: s0!:addr s1:unit s2:int s3:addr s4:unit
     0  int s2:int 7
     1  addr-of-part s3:addr s0:addr +1
     2  store s3:addr s2:int Int
     3  clear s3:addr <addr>
     4  addr-of-part s3:addr s0:addr +1
     5  call s4:unit m.bump (s3:<addr>)
     6  clear s3:addr <addr>
     7  copy s1:unit s4:unit Unit
     8  return s1:unit
"
    );
}

/// An inline field needs no indirection to name, so the address of
/// `p.y` is the address of a slot of this frame — one `AddrOfSlot`, and
/// nothing has to be held alive across the call.
#[test]
fn a_var_argument_naming_a_field_is_the_address_of_that_word() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn bump(var n: Int) { n = n + 1 }\nfn f() -> Int {\n  var p = Point(x: 1, y: 2)\n  bump(var p.y)\n  p.y\n}",
            "f"
        ),
        "\
fn1 m.f() -> Int
  frame 7: s0:int s1:int s2:int s3:int s4:int s5:addr s6:unit
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  addr-of-slot s5:addr s4:int
     5  call s6:unit m.bump (s5:<addr>)
     6  clear s5:addr <addr>
     7  copy s0:int s4:int Int
     8  return s0:int
"
    );
}

/// The checker already refused a label out of declaration order, so the
/// list lines up with the parameters one for one.
#[test]
fn a_labelled_argument_is_not_a_permutation() {
    assert_eq!(
        listing(
            "fn scaled(value: Int, by: Int) -> Int { value * by }\nfn f() -> Int { scaled(2, by: 3) }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
     0  int s1:int 2
     1  int s2:int 3
     2  call s3:int m.scaled (s1:Int s2:Int)
     3  copy s0:int s3:int Int
     4  return s0:int
"
    );
}
