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
     2  call s3:int m.add (s1:Int s2:Int) Int
     3  copy s0:int s3:int Int
     4  return s0:int Int
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
  frame 7: s0!:int s1:int s2:int s3:bool s4:int s5:int s6:int
  local n -> s0:Int [0, 12)
     0  lt.int.imm s3:bool s0:int 2
     1  branch-false s3:bool 4
     2  copy s2:int s0:int Int
     3  jump 10
     4  sub.int.imm s4:int s0:int 1
     5  call s5:int m.fib (s4:Int) Int
     6  sub.int.imm s4:int s0:int 2
     7  call s6:int m.fib (s4:Int) Int
     8  add.int s4:int s5:int s6:int
     9  copy s2:int s4:int Int
    10  copy s1:int s2:int Int
    11  return s1:int Int
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
  local a -> s0:Int [0, 5)
  local p -> s1:m.Point [0, 5)
  local b -> s3:Int [0, 5)
     0  add.int s5:int s0:int s1:int
     1  add.int s6:int s5:int s2:int
     2  add.int s5:int s6:int s3:int
     3  copy s4:int s5:int Int
     4  return s4:int Int
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
     6  call s3:int m.take (s1:Int s4:m.Point s2:Int) Int
     7  copy s0:int s3:int Int
     8  return s0:int Int
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
  frame 5: s0!:addr s1:unit s2:int s3:int s4:unit
  local n -> s0:<addr> [0, 6)
     0  load s2:int s0:addr Int
     1  add.int.imm s3:int s2:int 1
     2  store s0:addr s3:int Int
     3  unit s4:unit
     4  copy s1:unit s4:unit Unit
     5  return s1:unit Unit
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
  local total -> s1:Int [1, 5)
     0  int s1:int 0
     1  addr-of-slot s2:addr s1:int
     2  call s3:unit m.bump (s2:<addr>) Unit
     3  clear s2:addr <addr>
     4  copy s0:int s1:int Int
     5  return s0:int Int
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
  local p -> s0:<addr> [0, 9)
     0  int s2:int 7
     1  addr-of-part s3:addr s0:addr +1
     2  store s3:addr s2:int Int
     3  clear s3:addr <addr>
     4  addr-of-part s3:addr s0:addr +1
     5  call s4:unit m.bump (s3:<addr>) Unit
     6  clear s3:addr <addr>
     7  copy s1:unit s4:unit Unit
     8  return s1:unit Unit
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
  local p -> s3:m.Point [4, 8)
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  addr-of-slot s5:addr s4:int
     5  call s6:unit m.bump (s5:<addr>) Unit
     6  clear s5:addr <addr>
     7  copy s0:int s4:int Int
     8  return s0:int Int
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
     2  call s3:int m.scaled (s1:Int s2:Int) Int
     3  copy s0:int s3:int Int
     4  return s0:int Int
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
            "fn total(items: Int...) -> Int { items.length() }\nfn f() -> Int { total(1, 2, 3) }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 7: s0:int s1:int s2:int s3:int s4:ref s5:int s6:int
     0  int s1:int 1
     1  int s2:int 2
     2  int s3:int 3
     3  alloc s4:ref Array<array> x3
     4  int s5:int 1
     5  int s6:int 0
     6  store-elem s4:ref s6:int s1:int Int
     7  add.int s6:int s6:int s5:int
     8  store-elem s4:ref s6:int s2:int Int
     9  add.int s6:int s6:int s5:int
    10  store-elem s4:ref s6:int s3:int Int
    11  add.int s6:int s6:int s5:int
    12  call s1:int m.total (s4:Array) Int
    13  clear s4:ref Array
    14  copy s0:int s1:int Int
    15  return s0:int Int
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
            "fn total(items: Int...) -> Int { items.length() }\nfn f() -> Int { total() }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 3: s0:int s1:ref s2:int
     0  alloc s1:ref Array<array> x0
     1  call s2:int m.total (s1:Array) Int
     2  clear s1:ref Array
     3  copy s0:int s2:int Int
     4  return s0:int Int
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
            "fn total(items: Int...) -> Int { items.length() }\n\
             fn f(xs: Array<Int>) -> Int { total(0, ...xs, 9) }",
            "f"
        ),
        "\
fn0 m.f(Array) -> Int
  frame 12: s0!:ref s1:int s2:int s3:int s4:int s5:int s6:ref s7:int s8:int s9:int s10:bool s11:int
  local xs -> s0:Array [0, 25)
     0  int s2:int 0
     1  int s3:int 9
     2  int s4:int 2
     3  len s5:int s0:ref
     4  add.int s4:int s4:int s5:int
     5  alloc s6:ref Array<array> xs4:int
     6  int s5:int 1
     7  int s7:int 0
     8  store-elem s6:ref s7:int s2:int Int
     9  add.int s7:int s7:int s5:int
    10  len s8:int s0:ref
    11  int s9:int 0
    12  lt.int s10:bool s9:int s8:int
    13  branch-false s10:bool 19
    14  load-elem s11:int s0:ref s9:int Int
    15  store-elem s6:ref s7:int s11:int Int
    16  add.int s9:int s9:int s5:int
    17  add.int s7:int s7:int s5:int
    18  jump 12
    19  store-elem s6:ref s7:int s3:int Int
    20  add.int s7:int s7:int s5:int
    21  call s2:int m.total (s6:Array) Int
    22  clear s6:ref Array
    23  copy s1:int s2:int Int
    24  return s1:int Int
"
    );
}

/// A `Vector` spread is copied out with `Vector.toArray` before it is walked,
/// which is the clone `bind_params` makes of `storage.elements` and for the
/// same reason: what is spread is the elements the vector had.
#[test]
fn a_vector_spread_is_copied_out_before_it_is_walked() {
    let text = listing(
        "fn total(items: Int...) -> Int { items.length() }\n\
         fn f(xs: Vector<Int>) -> Int { total(...xs) }",
        "f",
    );
    assert!(
        text.contains("     0  call-builtin s2:ref Vector.toArray (s0:Vector) Array\n"),
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
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local n -> s1:Int [1, 2)
     0  int s1:int 3
     1  add.int.imm s2:int s1:int 1
     2  call s3:int m.near (s1:Int s2:Int) Int
     3  copy s0:int s3:int Int
     4  return s0:int Int
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
fn1 m.f() -> Int
  frame 5: s0:int s1:ref s2:int s3:int s4:int
  local base -> s1:fn [3, 7)
  local n -> s2:Int [4, 5)
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 3
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 3
     4  call s3:int m.base () Int
     5  call s4:int m.scaled (s2:Int s3:Int) Int
     6  copy s0:int s4:int Int
     7  clear s1:ref fn
     8  return s0:int Int
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
fn0 m.f(m.P) -> Int
  frame 3: s0!:int s1:int s2:int
  local p -> s0:m.P [0, 3)
  local self -> s0:m.P [0, 3)
     0  call s2:int m.P.scaled (s0:m.P s0:Int) Int
     1  copy s1:int s2:int Int
     2  return s1:int Int
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
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:ref s3:int
     0  int s1:int 1
     1  alloc s2:ref closure m.f#0<closure>
     2  int s3:int 2
     3  store-field s2:ref +0 s3:int Int
     4  call s3:int m.twice (s1:Int s2:fn) Int
     5  clear s2:ref fn
     6  copy s0:int s3:int Int
     7  return s0:int Int
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
                ("greet", "export fn twice(n: Int) -> Int { n * 2 }\n"),
                ("app", "use greet\nfn f() -> Int { greet.twice(21) }\n"),
            ],
            "app",
            "f",
        ),
        "\
fn0 app.f() -> Int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 21
     1  call s2:int greet.twice (s1:Int) Int
     2  copy s0:int s2:int Int
     3  return s0:int Int
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
fn0 app.f() -> shape.Point
  frame 6: s0:int s1:int s2:int s3:int s4:int s5:int
     0  int s2:int 1
     1  int s3:int 2
     2  copy s4:int s2:int Int
     3  copy s5:int s3:int Int
     4  copy s0:int s4:int shape.Point
     5  return s0:int shape.Point
"
    );
}
