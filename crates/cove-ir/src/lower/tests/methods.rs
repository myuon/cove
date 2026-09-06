//! The methods of the types the language ships.

use super::listing;

/// The receiver is the first operand where there is one and the arguments
/// follow it in source order, which is the one shape every operation in
/// the table has.
#[test]
fn a_builtin_method_is_one_call_over_its_operands() {
    assert_eq!(
        listing(
            "fn parts(s: String) -> Array<String> { s.split(\",\") }",
            "parts"
        ),
        "\
fn0 m.parts(String) -> Array
  frame 4: s0!:ref s1:ref s2:ref s3:ref
  local s -> s0:String [0, 4)
     0  str s2:ref \",\"
     1  call-builtin s3:ref String.split (s0:String s2:String) Array
     2  copy s1:ref s3:ref Array
     3  return s1:ref Array
"
    );
}

/// `Duration.seconds(1)` builds a duration and `d.seconds()` reads one
/// back out, and the language spells them the same. The machine tells them
/// apart by the `Repr` of operand 0, which is a static fact about the
/// location chosen here.
#[test]
fn an_associated_function_has_no_receiver() {
    assert_eq!(
        listing("fn wait() -> Duration { Duration.seconds(1) }", "wait"),
        "\
fn0 m.wait() -> Duration
  frame 3: s0:duration s1:int s2:duration
     0  int s1:int 1
     1  call-builtin s2:duration Duration.seconds (s1:Int) Duration
     2  copy s0:duration s2:duration Duration
     3  return s0:duration Duration
"
    );
}

#[test]
fn a_duration_reader_passes_its_receiver_as_operand_zero() {
    assert_eq!(
        listing("fn ms(d: Duration) -> Int { d.millis() }", "ms"),
        "\
fn0 m.ms(Duration) -> Int
  frame 3: s0!:duration s1:int s2:int
  local d -> s0:Duration [0, 3)
     0  call-builtin s2:int Duration.millis (s0:Duration) Int
     1  copy s1:int s2:int Int
     2  return s1:int Int
"
    );
}

/// `Option.isSome` no longer compiles to an inline discriminant comparison:
/// it is one of the methods `cove_schema::builtins::STANDARD_LIBRARY` names,
/// so `o.isSome()` is an ordinary [`crate::Inst::Call`] into
/// `std.option.isSome` and never a
/// [`Body::call_builtin_method`](super::super::Body::call_builtin_method)
/// dispatch — the same path `a_method_the_standard_library_implements_is_an_ordinary_call`
/// shows for `Array.isEmpty`.
#[test]
fn is_some_is_a_call_the_standard_library_implements() {
    assert_eq!(
        listing("fn has(o: Option<Int>) -> Bool { o.isSome() }", "has"),
        "\
fn0 m.has(Option) -> Bool
  frame 4: s0!:int s1!:int s2:bool s3:bool
  local o -> s0:Option [0, 3)
     0  call s3:bool std.option.isSome<Int> (s0:Option) Bool
     1  copy s2:bool s3:bool Bool
     2  return s2:bool Bool
"
    );
}

/// `Option.unwrapOr` migrated the same way: `o.unwrapOr(other)` is an
/// ordinary [`crate::Inst::Call`] into `std.option.unwrapOr`, with `other`
/// evaluated as an ordinary argument before the call — there is no branch
/// here for the discriminant to drive, because the discriminant question is
/// now inside the callee's own body rather than in this caller's listing.
#[test]
fn unwrap_or_is_an_ordinary_call_into_the_standard_library() {
    assert_eq!(
        listing(
            "fn value(o: Option<Int>, other: Int) -> Int { o.unwrapOr(other) }",
            "value"
        ),
        "\
fn0 m.value(Option Int) -> Int
  frame 5: s0!:int s1!:int s2!:int s3:int s4:int
  local o -> s0:Option [0, 3)
  local other -> s2:Int [0, 3)
     0  call s4:int std.option.unwrapOr<Int> (s0:Option s2:Int) Int
     1  copy s3:int s4:int Int
     2  return s3:int Int
"
    );
}

/// The machine builds the `Error` carrying a failure's message itself, so
/// the `Error` layout is interned here as well as the `Result`'s: the
/// `Result` describes its `Err` words without saying what declared them.
#[test]
fn a_parser_answers_a_result_and_interns_the_error_it_may_carry() {
    assert_eq!(
        listing(
            "fn parse(s: String) -> Int { Int.parse(s).unwrapOr(0) }",
            "parse"
        ),
        "\
fn0 m.parse(String) -> Int
  frame 7: s0!:ref s1:int s2:int s3:int s4:ref s5:int s6:int
  local s -> s0:String [0, 6)
     0  call-builtin s2:int Int.parse (s0:String) Result
     1  int s5:int 0
     2  call s6:int std.result.unwrapOr<Int, Error> (s2:Result s5:Int) Int
     3  clear s2:int Result
     4  copy s1:int s6:int Int
     5  return s1:int Int
"
    );
}

/// The receiver is the first parameter and the written parameters follow
/// it. Nothing about a method needs a second calling convention.
#[test]
fn a_method_on_a_declared_type_is_an_ordinary_call() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nimpl Point {\n  fn sum(self) -> Int { self.x + self.y }\n}\nfn f(p: Point) -> Int { p.sum() }",
            "f"
        ),
        "\
fn0 m.f(m.Point) -> Int
  frame 4: s0!:int s1!:int s2:int s3:int
  local p -> s0:m.Point [0, 3)
     0  call s3:int m.Point.sum (s0:m.Point) Int
     1  copy s2:int s3:int Int
     2  return s2:int Int
"
    );
}

/// `Array.isEmpty` is not a machine builtin: it is the one method
/// `cove_schema::builtins::STANDARD_LIBRARY` names, so `items.isEmpty()` is
/// an ordinary [`crate::Inst::Call`] into `std.array.isEmpty` and never a
/// [`Body::call_builtin_method`](super::super::Body::call_builtin_method)
/// dispatch — the same lowering a bare `isEmpty(items)` written in Cove
/// would reach, generic instantiation and all. This is the proof the
/// mechanism reaches all the way through, alongside
/// `crates/cove-sema/src/stdlib.rs`'s and `crates/cove-schema/src/builtins.rs`'s
/// own tests of the same table.
#[test]
fn a_method_the_standard_library_implements_is_an_ordinary_call() {
    assert_eq!(
        listing("fn f(xs: Array<Int>) -> Bool { xs.isEmpty() }", "f"),
        "\
fn0 m.f(Array) -> Bool
  frame 3: s0!:ref s1:bool s2:bool
  local xs -> s0:Array [0, 3)
     0  call s2:bool std.array.isEmpty<Int> (s0:Array) Bool
     1  copy s1:bool s2:bool Bool
     2  return s1:bool Bool
"
    );
}

/// The method names the caller's storage, so a write to a field of `self`
/// reaches the caller's own words with no copy back. There is no
/// instruction that offsets an address, so a field of one is a load, a
/// write into the words, and a store.
#[test]
fn a_var_self_receiver_is_an_address() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nimpl Point {\n  fn bump(var self) { self.y = self.y + 1 }\n}",
            "Point.bump"
        ),
        "\
fn0 m.Point.bump(<addr>) -> Unit
  frame 6: s0!:addr s1:unit s2:addr s3:int s4:int s5:unit
  local self -> s0:<addr> [0, 10)
     0  addr-of-part s2:addr s0:addr +1
     1  load s3:int s2:addr Int
     2  clear s2:addr <addr>
     3  add.int.imm s4:int s3:int 1
     4  addr-of-part s2:addr s0:addr +1
     5  store s2:addr s4:int Int
     6  clear s2:addr <addr>
     7  unit s5:unit
     8  copy s1:unit s5:unit Unit
     9  return s1:unit Unit
"
    );
}

/// `mapError` is a branch and one call through a closure.
///
/// The module docs above named it as the one `Result` method that is not a
/// question about the discriminant: it takes a callback, and
/// `docs/LINEAR_VM.md` says a builtin never calls back into Cove — so what
/// runs the callback is an ordinary [`crate::Inst::CallClosure`] frame here.
///
/// The two `Result`s are two layouts, which is why the `Ok` is copied rather
/// than passed along: `Int.parse` answers a `Result<Int, Error>` and this
/// answers a `Result<Int, m.E>`. The closure is built before the branch,
/// because it is an ordinary argument and the language evaluates a call's
/// arguments before the call.
#[test]
fn map_error_is_a_branch_and_one_call_through_a_closure() {
    assert_eq!(
        super::listing(
            "enum E { Bad(String) }\n\
             fn f(t: String) -> Result<Int, E> { Int.parse(t).mapError(fn(error) { E.Bad(t) }) }",
            "f"
        ),
        "\
fn0 m.f(String) -> Result
  frame 15: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:ref s7:int s8:int s9:ref s10:ref s11:int s12:int s13:ref s14:bool
  local t -> s0:String [0, 19)
     0  call-builtin s7:int Int.parse (s0:String) Result
     1  alloc s10:ref closure m.f#0<closure>
     2  int s11:int 14
     3  store-field s10:ref +0 s11:int Int
     4  store-field s10:ref +1 s0:ref String
     5  int s11:int 0
     6  eq.int s14:bool s7:int s11:int
     7  branch-false s14:bool 11
     8  int s4:int 0
     9  copy s5:int s8:int Int
    10  jump 14
    11  call-closure s12:int s10:ref (s9:Error)
    12  int s4:int 1
    13  copy s5:int s12:int m.E
    14  clear s12:int m.E
    15  clear s10:ref fn
    16  clear s7:int Result
    17  copy s1:int s4:int Result
    18  return s1:int Result
"
    );
}

/// The callback is always handed the error it replaces (ADR 0044): the
/// operand the closure runs with is the `Err` payload the branch above just
/// tested, borrowed directly out of the receiver rather than copied.
#[test]
fn map_error_passes_the_failure_to_a_callback_that_takes_one() {
    assert_eq!(
        super::listing(
            "enum E { Bad(String) }\n\
             fn f(t: String) -> Result<Int, E> { Int.parse(t).mapError(fn(e) { E.Bad(\"{e}\") }) }",
            "f"
        ),
        "\
fn0 m.f(String) -> Result
  frame 15: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:ref s7:int s8:int s9:ref s10:ref s11:int s12:int s13:ref s14:bool
  local t -> s0:String [0, 18)
     0  call-builtin s7:int Int.parse (s0:String) Result
     1  alloc s10:ref closure m.f#0<closure>
     2  int s11:int 14
     3  store-field s10:ref +0 s11:int Int
     4  int s11:int 0
     5  eq.int s14:bool s7:int s11:int
     6  branch-false s14:bool 10
     7  int s4:int 0
     8  copy s5:int s8:int Int
     9  jump 13
    10  call-closure s12:int s10:ref (s9:Error)
    11  int s4:int 1
    12  copy s5:int s12:int m.E
    13  clear s12:int m.E
    14  clear s10:ref fn
    15  clear s7:int Result
    16  copy s1:int s4:int Result
    17  return s1:int Result
"
    );
}
