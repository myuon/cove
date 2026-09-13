//! Calls, and the frame boundary they have to match.
//!
//! # `keep()`
//!
//! Several fixtures here declare a `fn keep() {}` and call it from the callee
//! whose call they are about. It is not part of what any of them assert and it
//! never appears in a listing, because every listing here is the *caller's*.
//!
//! `super::super::inline` expands a call to a small leaf where it is made,
//! and a two-instruction callee is exactly the shape it expands. A fixture whose
//! callee is that small has no `call` left in it, and a test of how a call
//! lowers that holds no call is a test of nothing. Calling anything at all is
//! what makes a function not a leaf, and `keep()` is the smallest thing there
//! is to call.
//!
//! Where the callee's *own* listing is what a case is about — the multiword
//! parameters one — the fixture is left alone, because nothing expands a
//! function into itself. Where the *call* is incidental to what a case
//! asserts — the two about defaults, whose subject is what a default reads —
//! the fixture is left alone too and the listing shows the expansion, because
//! a default read into an expanded body is still a default read.

use super::listing;

/// The machine copies each argument's words into the callee's frame,
/// which begins where this one ends. Nothing is pushed, permuted or
/// copied back.
#[test]
fn a_call_names_the_arguments_and_the_destination_location() {
    assert_eq!(
        listing(
            "fn keep() {}\nfn add(a: Int, b: Int) -> Int { keep()\n  a + b }\nfn f() -> Int { add(1, 2) }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 1
     1  int s2:int 2
     2  call s0:Int m.add (s1:Int s2:Int)
     3  return s0:Int
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
fn @m.fib(Int) -> Int
  frame 6: s0!:int s1:int s2:bool s3:int s4:int s5:int
  local n -> s0:Int [0, 9)
     0  lt.int.imm.branch s2:bool s0:int 2 3
     1  copy s1:Int s0:Int
     2  jump 8
     3  sub.int.imm s3:int s0:int 1
     4  call s4:Int m.fib (s3:Int)
     5  sub.int.imm s3:int s0:int 2
     6  call s5:Int m.fib (s3:Int)
     7  add.int s1:int s4:int s5:int
     8  return s1:Int
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
fn @m.take(Int m.Point Int) -> Int
  frame 7: s0!:int s1!:int s2!:int s3!:int s4:int s5:int s6:int
  local a -> s0:Int [0, 4)
  local p -> s1..s2:m.Point [0, 4)
  local b -> s3:Int [0, 4)
     0  add.int s5:int s0:int s1:int
     1  add.int s6:int s5:int s2:int
     2  add.int s4:int s6:int s3:int
     3  return s4:Int
"
    );
}

#[test]
fn a_call_passing_a_multiword_argument_names_its_base_slot() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn keep() {}\nfn take(a: Int, p: Point, b: Int) -> Int { keep()\n  a + p.x + p.y + b }\nfn f() -> Int { take(1, Point(x: 2, y: 3), 4) }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 6: s0:int s1:int s2:int s3:int s4:int s5:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  copy s4:Int s2:Int
     4  copy s5:Int s3:Int
     5  int s2:int 4
     6  call s0:Int m.take (s1:Int s4..s5:m.Point s2:Int)
     7  return s0:Int
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
fn @m.bump(<addr>) -> Unit
  frame 5: s0!:addr s1:unit s2:int s3:int s4:unit
  local n -> s0:<addr> [0, 6)
     0  load s2:Int s0:addr
     1  add.int.imm s3:int s2:int 1
     2  store s0:addr s3:Int
     3  unit s4:unit
     4  copy s1:Unit s4:Unit
     5  return s1:Unit
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
fn @m.f() -> Int
  frame 4: s0:int s1:int s2:addr s3:unit
  local total -> s1:Int [1, 5)
     0  int s1:int 0
     1  addr-of-slot s2:addr s1:int
     2  call s3:Unit m.bump (s2:<addr>)
     3  clear s2:<addr>
     4  copy s0:Int s1:Int
     5  return s0:Int
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
fn @m.shift(<addr>) -> Unit
  frame 4: s0!:addr s1:unit s2:int s3:addr
  local p -> s0:<addr> [0, 7)
     0  int s2:int 7
     1  addr-of-part s3:addr s0:addr +1
     2  store s3:addr s2:Int
     3  clear s3:<addr>
     4  addr-of-part s3:addr s0:addr +1
     5  call s1:Unit m.bump (s3:<addr>)
     6  return s1:Unit
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
fn @m.f() -> Int
  frame 7: s0:int s1:int s2:int s3:int s4:int s5:addr s6:unit
  local p -> s3..s4:m.Point [4, 8)
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:Int s1:Int
     3  copy s4:Int s2:Int
     4  addr-of-slot s5:addr s4:int
     5  call s6:Unit m.bump (s5:<addr>)
     6  clear s5:<addr>
     7  copy s0:Int s4:Int
     8  return s0:Int
"
    );
}

/// The checker already refused a label out of declaration order, so the
/// list lines up with the parameters one for one.
#[test]
fn a_labelled_argument_is_not_a_permutation() {
    assert_eq!(
        listing(
            "fn keep() {}\nfn scaled(value: Int, by: Int) -> Int { keep()\n  value * by }\nfn f() -> Int { scaled(2, by: 3) }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 2
     1  int s2:int 3
     2  call s0:Int m.scaled (s1:Int s2:Int)
     3  return s0:Int
"
    );
}

// ------------------------------------- variadic parameters and their defaults

/// A variadic parameter is an immutable `Array<T>` inside the body whatever
/// the call wrote, and **the caller builds it**.
///
/// `interp::bind_params` says so: the arguments no earlier parameter took are
/// collected and bound as one array. Nothing about the callee's frame changes
/// for it — a variadic parameter is one ordinary location holding one
/// ordinary array — so the calling convention has nothing to say about how it
/// was filled, and this is an array literal by another spelling.
#[test]
fn a_variadic_parameter_collects_its_arguments_into_an_array() {
    assert_eq!(
        listing(
            "fn keep() {}\nfn total(items: Int...) -> Int { keep()\n  items.length() }\nfn f() -> Int { total(1, 2, 3) }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 7: s0:int s1:int s2:int s3:int s4:ref s5:int s6:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  alloc s4:ref Array<array> x3
     4  int s5:int 1
     5  int s6:int 0
     6  store-elem s4:ref s6:int s1:Int
     7  add.int s6:int s6:int s5:int
     8  store-elem s4:ref s6:int s2:Int
     9  add.int s6:int s6:int s5:int
    10  store-elem s4:ref s6:int s3:Int
    11  add.int s6:int s6:int s5:int
    12  call s0:Int m.total (s4:Array)
    13  return s0:Int
"
    );
}

/// A variadic parameter given nothing is an empty `Array<T>`, which is the
/// one collection literal the lowering allocates outright: there is nothing
/// to count and nothing to step.
#[test]
fn a_variadic_parameter_given_nothing_is_an_empty_array() {
    assert_eq!(
        listing(
            "fn keep() {}\nfn total(items: Int...) -> Int { keep()\n  items.length() }\nfn f() -> Int { total() }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 2: s0:int s1:ref
     0  alloc s1:ref Array<array> x0
     1  call s0:Int m.total (s1:Array)
     2  return s0:Int
"
    );
}

/// A spread contributes the *elements* of the sequence it names rather than
/// the sequence, so the length stops being a fact the lowering knows.
///
/// It is counted first — one for each plain argument at 2, one `len` per
/// spread at 3 — and then the run is filled, with a walk per spread. 12–18 is
/// that walk, and 19 is the plain argument that comes after it: one index
/// runs through the whole of it, so the two kinds of argument write into the
/// same counter and no joined list is ever built.
#[test]
fn a_spread_argument_is_counted_and_then_walked_into_the_run() {
    assert_eq!(
        listing(
            "fn keep() {}\nfn total(items: Int...) -> Int { keep()\n  items.length() }\n\
             fn f(xs: Array<Int>) -> Int { total(0, ...xs, 9) }",
            "f"
        ),
        "\
fn @m.f(Array) -> Int
  frame 12: s0!:ref s1:int s2:int s3:int s4:int s5:int s6:ref s7:int s8:int s9:int s10:bool s11:int
  local xs -> s0:Array [0, 22)
     0  int s2:int 0
     1  int s3:int 9
     2  int s4:int 2
     3  len s5:int s0:ref
     4  add.int s4:int s4:int s5:int
     5  alloc s6:ref Array<array> xs4:int
     6  int s5:int 1
     7  int s7:int 0
     8  store-elem s6:ref s7:int s2:Int
     9  add.int s7:int s7:int s5:int
    10  len s8:int s0:ref
    11  int s9:int 0
    12  lt.int.branch s10:bool s9:int s8:int 18
    13  load-elem s11:Int s0:ref s9:int
    14  store-elem s6:ref s7:int s11:Int
    15  add.int s9:int s9:int s5:int
    16  add.int s7:int s7:int s5:int
    17  jump 12
    18  store-elem s6:ref s7:int s3:Int
    19  add.int s7:int s7:int s5:int
    20  call s1:Int m.total (s6:Array)
    21  return s1:Int
"
    );
}

/// A `Vector` spread is copied out with `Vector.toArray` before it is walked,
/// which is the clone `bind_params` makes of `storage.elements` and for the
/// same reason: what is spread is the elements the vector had.
#[test]
fn a_vector_spread_is_copied_out_before_it_is_walked() {
    let text = listing(
        "fn keep() {}\nfn total(items: Int...) -> Int { keep()\n  items.length() }\n\
         fn f(xs: Vector<Int>) -> Int { total(...xs) }",
        "f",
    );
    assert!(
        text.contains("     0  call-builtin s2:Array Vector.toArray (s0:Vector)\n"),
        "{text}"
    );
}

/// A default is evaluated **in the callee's scope**, and 2 is the whole of
/// what that means: `n` is the parameter before it, at the location the call
/// has already evaluated its argument into.
///
/// `interp::bind_params` puts it there — *"Default arguments are evaluated by
/// the callee"*, with the parameters before this one already declared in the
/// environment it is evaluated in. The words are the caller's, because the
/// argument has to end up in the caller's frame either way; only the names
/// are the callee's.
#[test]
fn a_default_reads_the_parameters_before_it() {
    assert_eq!(
        listing(
            "fn near(n: Int, by: Int = n + 1) -> Int { by }\nfn f() -> Int { near(3) }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local n -> s1:Int [1, 2)
     0  int s1:int 3
     1  add.int.imm s2:int s1:int 1
     2  copy s0:Int s2:Int
     3  return s0:Int
"
    );
}

/// The other half of "in the callee's scope": a name the *caller* binds does
/// not shadow the one the declaration meant.
///
/// `f` has a local `base`, and `scaled`'s default was written where `base` is
/// the module's declaration — so 4 calls `m.base` and not the closure `f`
/// built. What arranges it is an isolated scope: a lookup inside a default
/// sees the callee's parameters and then stops, and everything past that is
/// resolved against the callee's module rather than the caller's frame.
#[test]
fn a_default_does_not_see_what_the_caller_happens_to_have_bound() {
    assert_eq!(
        listing(
            "fn base() -> Int { 7 }\n\
             fn scaled(n: Int, by: Int = base()) -> Int { n * by }\n\
             fn f() -> Int {\n  let base = fn() { 100 }\n  scaled(3)\n}",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 6: s0:int s1:ref s2:int s3:int s4:int s5:int
  local base -> s1:fn [3, 6)
  local n -> s2:Int [4, 5)
     0  alloc s1:ref closure m.f#0<closure>
     1  func-ref s2:int @m.f#0
     2  store-field s1:ref +0 s2:Int
     3  int s2:int 3
     4  int s3:int 7
     5  mul.int s0:int s2:int s3:int
     6  return s0:Int
"
    );
}

/// A method's default may read the receiver, because the receiver is a
/// parameter and it is bound before the ones that follow it.
#[test]
fn a_default_on_a_method_reads_the_receiver() {
    assert_eq!(
        listing(
            "struct P { x: Int }\n\
             impl P { fn scaled(self, by: Int = self.x) -> Int { self.x * by } }\n\
             fn f(p: P) -> Int { p.scaled() }",
            "f"
        ),
        "\
fn @m.f(m.P) -> Int
  frame 3: s0!:int s1:int s2:int
  local p -> s0:m.P [0, 2)
  local self -> s0:m.P [0, 2)
     0  mul.int s1:int s0:int s0:int
     1  return s1:Int
"
    );
}

/// `f(x) { ... }` is sugar and nothing more.
///
/// The parser has already built the block as a parameterless lambda, and
/// `interp::eval_args` pushes it on the end of the written arguments —
/// unlabelled, not `var`, not spread. So the closure lands in the parameter
/// a written argument would have filled, and no path in this lowering knows
/// which spelling it arrived in.
#[test]
fn a_trailing_lambda_is_the_call_s_last_argument() {
    assert_eq!(
        listing(
            "fn twice(n: Int, f: fn() -> Int) -> Int { n + f() }\n\
             fn f() -> Int { twice(1) { 2 } }",
            "f"
        ),
        "\
fn @m.f() -> Int
  frame 4: s0:int s1:int s2:ref s3:int
     0  int s1:int 1
     1  alloc s2:ref closure m.f#0<closure>
     2  func-ref s3:int @m.f#0
     3  store-field s2:ref +0 s3:Int
     4  call s0:Int m.twice (s1:Int s2:fn)
     5  return s0:Int
"
    );
}

// ---- a module imported whole ------------------------------------------

/// `use forager` then `forager.decide(...)`: a call reached through the name
/// a module is visible under.
///
/// `ResolvedModule::module_imports` is the fact, and it is read here the way
/// every other consumer of it reads it — the checker's `qualified_key`, the
/// oracle's `imported_module`, the predecessor's index. What comes out is an
/// ordinary [`crate::Inst::Call`] naming the declaration in the module that
/// exports it: a qualified name is a way of writing a name, not a second
/// calling convention.
#[test]
fn a_call_through_a_module_imported_whole_names_the_declaration_it_exports() {
    assert_eq!(
        super::listing_in(
            &[
                (
                    "greet",
                    "fn keep() {}\nexport fn twice(n: Int) -> Int { keep()\n  n * 2 }\n"
                ),
                ("app", "use greet\nfn f() -> Int { greet.twice(21) }\n"),
            ],
            "app",
            "f",
        ),
        "\
fn @app.f() -> Int
  frame 2: s0:int s1:int
     0  int s1:int 21
     1  call s0:Int greet.twice (s1:Int)
     2  return s0:Int
"
    );
}

/// The other half of the same name: a struct the module exports, initialized
/// through it.
///
/// The oracle asks the two in this order — `exported_function`, then an
/// exported struct's `init_struct` — and a qualified initializer is the
/// unqualified one with the fields read in the declaring module's
/// vocabulary, which is where they were already being read.
#[test]
fn an_initializer_through_a_module_imported_whole_is_an_ordinary_one() {
    assert_eq!(
        super::listing_in(
            &[
                ("shape", "export struct Point { x: Int, y: Int }\n"),
                (
                    "app",
                    "use shape\nfn f() -> shape.Point { shape.Point(x: 1, y: 2) }\n"
                ),
            ],
            "app",
            "f",
        ),
        "\
fn @app.f() -> shape.Point
  frame 6: s0:int s1:int s2:int s3:int s4:int s5:int
     0  int s2:int 1
     1  int s3:int 2
     2  copy s4:Int s2:Int
     3  copy s5:Int s3:Int
     4  copy s0..s1:shape.Point s4..s5:shape.Point
     5  return s0..s1:shape.Point
"
    );
}
