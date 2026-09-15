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
fn @m.parts(String) -> Array
  frame 3: s0!:ref s1:ref s2:ref
  local s -> s0:String [0, 3)
     0  str s2:ref \",\"
     1  call-builtin s1:Array String.split (s0:String s2:String)
     2  return s1:Array
"
    );
}

/// `Duration.nanos(1)` builds a duration and `d.nanos()` reads one back
/// out, and the language spells them the same. The machine tells them apart
/// by the `Repr` of operand 0, which is a static fact about the location
/// chosen here. `nanos` is the one unit that still works this way — every
/// other one moved to `std.duration`, below.
#[test]
fn an_associated_function_has_no_receiver() {
    assert_eq!(
        listing("fn wait() -> Duration { Duration.nanos(1) }", "wait"),
        "\
fn @m.wait() -> Duration
  frame 2: s0:duration s1:int
     0  int s1:int 1
     1  call-builtin s0:Duration Duration.nanos (s1:Int)
     2  return s0:Duration
"
    );
}

#[test]
fn a_duration_reader_passes_its_receiver_as_operand_zero() {
    assert_eq!(
        listing("fn ns(d: Duration) -> Int { d.nanos() }", "ns"),
        "\
fn @m.ns(Duration) -> Int
  frame 2: s0!:duration s1:int
  local d -> s0:Duration [0, 2)
     0  call-builtin s1:Int Duration.nanos (s0:Duration)
     1  return s1:Int
"
    );
}

/// `Duration.millis` and `d.millis()` are `std.duration`'s now, and each
/// call form is bound to a different function of it: the associated call
/// that builds a duration is `ofMillis`, and the method that reads one back
/// is `millis` — the same split every migrated unit makes, because a module
/// cannot declare `millis` twice.
#[test]
fn a_duration_builder_is_a_call_the_standard_library_implements() {
    assert_eq!(
        listing("fn wait() -> Duration { Duration.millis(1) }", "wait"),
        "\
fn @m.wait() -> Duration
  frame 5: s0:duration s1:int s2:duration s3:duration s4:int
     0  int s1:int 1
     1  mul.int.imm s4:int s1:int 1000000
     2  call-builtin s2:Duration Duration.nanos (s4:Int)
     3  copy s0:Duration s2:Duration
     4  return s0:Duration
"
    );
}

#[test]
fn a_duration_reader_is_a_call_the_standard_library_implements() {
    assert_eq!(
        listing("fn ms(d: Duration) -> Int { d.millis() }", "ms"),
        "\
fn @m.ms(Duration) -> Int
  frame 4: s0!:duration s1:int s2:int s3:int
  local d -> s0:Duration [0, 3)
     0  call-builtin s3:Int Duration.nanos (s0:Duration)
     1  div.int.imm s1:int s3:int 1000000
     2  return s1:Int
"
    );
}

/// `Option.isSome` does not compile through the special-cased inline
/// discriminant comparison: it is one of the methods
/// `cove_schema::builtins::STANDARD_LIBRARY` names, so `o.isSome()`
/// resolves to an ordinary call into `std.option.isSome`, never a
/// [`Body::call_builtin_method`](super::super::Body::call_builtin_method)
/// dispatch — the same path
/// `a_method_the_standard_library_implements_is_an_ordinary_call` shows for
/// `Array.isEmpty`. That target is a small leaf, so
/// `super::super::inline` expands it where the call was; the `switch` below
/// is `std.option.isSome`'s own body, not the special-cased dispatch
/// this test is contrasting it with.
#[test]
fn is_some_is_a_call_the_standard_library_implements() {
    assert_eq!(
        listing("fn has(o: Option<Int>) -> Bool { o.isSome() }", "has"),
        "\
fn @m.has(Option) -> Bool
  frame 4: s0!:tag s1!:int s2:bool s3:bool
  local o -> s0..s1:Option [0, 7)
     0  switch s0:tag [3 1] else 5
     1  bool s2:bool true
     2  jump 6
     3  bool s2:bool false
     4  jump 6
     5  trap \"no `match` arm covers this value\"
     6  return s2:Bool
"
    );
}

/// `Option.unwrapOr` migrated the same way: `o.unwrapOr(other)` resolves
/// to an ordinary call into `std.option.unwrapOr`, with `other` evaluated
/// as an ordinary argument before the call is prepared. `unwrapOr`'s body
/// is a small leaf, so `super::super::inline` expands it where the call was,
/// and the discriminant question that call used to hide inside its own
/// frame is the `switch` this caller's listing shows instead.
#[test]
fn unwrap_or_is_an_ordinary_call_into_the_standard_library() {
    assert_eq!(
        listing(
            "fn value(o: Option<Int>, other: Int) -> Int { o.unwrapOr(other) }",
            "value"
        ),
        "\
fn @m.value(Option Int) -> Int
  frame 6: s0!:tag s1!:int s2!:int s3:int s4:int s5:int
  local o -> s0..s1:Option [0, 8)
  local other -> s2:Int [0, 8)
     0  switch s0:tag [4 1] else 6
     1  copy s5:Int s1:Int
     2  copy s3:Int s5:Int
     3  jump 7
     4  copy s3:Int s2:Int
     5  jump 7
     6  trap \"no `match` arm covers this value\"
     7  return s3:Int
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
fn @m.parse(String) -> Int
  frame 8: s0!:ref s1:int s2:tag s3:int s4:ref s5:int s6:int s7:int
  local s -> s0:String [0, 10)
     0  call-builtin s2..s4:Result Int.parse (s0:String)
     1  int s5:int 0
     2  switch s2:tag [3 6] else 8
     3  copy s7:Int s3:Int
     4  copy s1:Int s7:Int
     5  jump 9
     6  copy s1:Int s5:Int
     7  jump 9
     8  trap \"no `match` arm covers this value\"
     9  return s1:Int
"
    );
}

/// The receiver is the first parameter and the written parameters follow
/// it, which is the calling convention a bounded call resolves against —
/// nothing about a method needs a second one. `Point.sum` is a small
/// leaf, so `super::super::inline` expands it where the call was, and the
/// receiver's own slots become the `add.int`'s operands, with no call
/// left to show the convention.
#[test]
fn a_method_on_a_declared_type_is_an_ordinary_call() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nimpl Point {\n  fn sum(self) -> Int { self.x + self.y }\n}\nfn f(p: Point) -> Int { p.sum() }",
            "f"
        ),
        "\
fn @m.f(m.Point) -> Int
  frame 4: s0!:int s1!:int s2:int s3:int
  local p -> s0..s1:m.Point [0, 2)
     0  add.int s2:int s0:int s1:int
     1  return s2:Int
"
    );
}

/// `Array.isEmpty` is not a machine builtin: it is the one method
/// `cove_schema::builtins::STANDARD_LIBRARY` names, so `items.isEmpty()`
/// resolves to an ordinary call into `std.array.isEmpty`, never a
/// [`Body::call_builtin_method`](super::super::Body::call_builtin_method)
/// dispatch — the same lowering a bare `isEmpty(items)` written in Cove
/// would reach, generic instantiation and all. That target is a small
/// leaf, so `super::super::inline` expands it where the call was; `len` and
/// `eq.int.imm` below are `std.array.isEmpty`'s own body, and the proof
/// the mechanism reaches all the way through is this listing alongside
/// `crates/cove-sema/src/stdlib.rs`'s and `crates/cove-schema/src/builtins.rs`'s
/// own tests of the same table.
#[test]
fn a_method_the_standard_library_implements_is_an_ordinary_call() {
    assert_eq!(
        listing("fn f(xs: Array<Int>) -> Bool { xs.isEmpty() }", "f"),
        "\
fn @m.f(Array) -> Bool
  frame 4: s0!:ref s1:bool s2:bool s3:int
  local xs -> s0:Array [0, 3)
     0  len s3:int s0:ref
     1  eq.int.imm s1:bool s3:int 0
     2  return s1:Bool
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
fn @m.Point.bump(<addr>) -> Unit
  frame 6: s0!:addr s1:unit s2:addr s3:int s4:int s5:unit
  local self -> s0:<addr> [0, 10)
     0  addr-of-part s2:addr s0:addr +1
     1  load s3:Int s2:addr
     2  clear s2:<addr>
     3  add.int.imm s4:int s3:int 1
     4  addr-of-part s2:addr s0:addr +1
     5  store s2:addr s4:Int
     6  clear s2:<addr>
     7  unit s5:unit
     8  copy s1:Unit s5:Unit
     9  return s1:Unit
"
    );
}

/// `Vector.push` is `std.vector.push`, whose body is ADR 0058's
/// `core.vectorPush`: a word `growable-push` of the element's layout and the
/// `()` the call answers. The wrapper is thin, so it is expanded wherever it is
/// called, and neither a `call` nor a `call-builtin` is left — at a one-word
/// element and at a two-word one, whose source is the whole run.
#[test]
fn a_push_is_a_word_growable_push_where_it_is_written() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\n\
             fn f(xs: Vector<Int>, ps: Vector<Point>, p: Point) -> Int {\n  \
               var ints = xs\n  var points = ps\n  ints.push(7)\n  points.push(p)\n  0\n}",
            "f"
        ),
        "\
fn @m.f(Vector Vector m.Point) -> Int
  frame 11: s0!:ref s1!:ref s2!:int s3!:int s4:int s5:ref s6:ref s7:int s8:unit s9:unit s10:unit
  local xs -> s0:Vector [0, 9)
  local ps -> s1:Vector [0, 9)
  local p -> s2..s3:m.Point [0, 9)
  local ints -> s5:Vector [1, 8)
  local points -> s6:Vector [2, 8)
     0  copy s5:Vector s0:Vector
     1  copy s6:Vector s1:Vector
     2  int s7:int 7
     3  growable-push.words Int s5:ref s7:Int
     4  unit s8:unit
     5  growable-push.words m.Point s6:ref s2..s3:m.Point
     6  unit s8:unit
     7  int s4:int 0
     8  return s4:Int
"
    );
}

/// `Vector.set` is `std.vector.set`: the range decision and the `Option` in
/// Cove, over ADR 0058's `core.vectorLoad` and `core.vectorStore`, which are a
/// `load-field` of the store and a `load-elem` or `store-elem` of it. What is
/// left at the call site is an ordinary call — the body is eighteen
/// instructions, past the leaf limit outside a loop — and no `call-builtin`.
#[test]
fn a_set_is_a_range_check_over_an_element_load_and_store() {
    assert_eq!(
        listing(
            "fn f(xs: Vector<Int>, i: Int) -> Option<Int> {\n  var ints = xs\n  ints.set(i, 7)\n}",
            "f"
        ),
        "\
fn @m.f(Vector Int) -> Option
  frame 6: s0!:ref s1!:int s2:tag s3:int s4:ref s5:int
  local xs -> s0:Vector [0, 4)
  local i -> s1:Int [0, 4)
  local ints -> s4:Vector [1, 3)
     0  copy s4:Vector s0:Vector
     1  int s5:int 7
     2  call s2..s3:Option std.vector.set<Int> (s4:Vector s1:Int s5:Int)
     3  return s2..s3:Option
"
    );
}

/// `Vector.freeze` is `std.vector.freeze`, whose body is ADR 0058's
/// `core.vectorFinish`: one word `run-finish` into the `Array` of the element,
/// with nothing to validate, expanded where it is called — and written straight
/// into the function's answer, because a standard-library binding hands its call
/// the destination the surrounding form asked for, as the builtin did.
#[test]
fn a_freeze_is_a_word_run_finish_where_it_is_written() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Array<Int> {\n  var building: Vector<Int> = Vector.of()\n  building.push(n)\n  building.freeze()\n}",
            "f"
        ),
        "\
fn @m.f(Int) -> Array
  frame 8: s0!:int s1:ref s2:ref s3:int s4:ref s5:unit s6:unit s7:ref
  local n -> s0:Int [0, 10)
  local building -> s4:Vector [6, 9)
     0  alloc s2:ref Vector<store> x0
     1  alloc s4:ref Vector<vector>
     2  int s3:int 0
     3  store-field s4:ref +0 s3:Int
     4  store-field s4:ref +1 s2:<ref>
     5  clear s2:<ref>
     6  growable-push.words Int s4:ref s0:Int
     7  unit s5:unit
     8  run-finish.words Int s1:ref s4:ref Array unchecked
     9  return s1:Array
"
    );
}

/// `mapError` moved out of the lowering the same way `isSome` and
/// `unwrapOr` did above: `cove_schema::builtins::STANDARD_LIBRARY` names it
/// too, so `Int.parse(t).mapError(fn(error) { ... })` is an ordinary
/// [`crate::Inst::Call`] into `std.result.mapError<Int, Error, m.E>`, with
/// the callback passed as an ordinary closure argument — built, captured,
/// and handed to the call exactly as any other argument would be.
///
/// The two `Result`s are still two layouts (`Int.parse` answers a
/// `Result<Int, Error>` and `f` answers a `Result<Int, m.E>`), which is why
/// the callee is instantiated at three type arguments rather than two — but
/// the branch between the `Ok` and `Err` arms is no longer in this listing
/// at all. It, and the question of what the callback is handed, now live
/// inside `std/result.cove`'s own body:
///
/// ```text
/// export fn mapError<T, E, F>(result: Result<T, E>, body: fn(error: E) -> F) -> Result<T, F> {
///   match result {
///     Ok(value) => Ok(value)
///     Err(error) => Err(body(error))
///   }
/// }
/// ```
///
/// This replaces two tests that used to describe that branch directly: one
/// showed the discriminant check choosing between a copy and a
/// `CallClosure`, the other showed the callback reading the `Err` payload
/// borrowed out of the receiver rather than through a copy (ADR 0044). Both
/// facts are true of `mapError` still, but neither is visible to the
/// *caller's* lowering any more — what is left to distinguish between a
/// callback that ignores its parameter and one that uses it is only whether
/// the closure captures an outer binding, which `closures.rs` already
/// covers generically. One test is what is left to say here.
#[test]
fn map_error_is_an_ordinary_call_into_the_standard_library() {
    assert_eq!(
        super::listing(
            "enum E { Bad(String) }\n\
             fn f(t: String) -> Result<Int, E> { Int.parse(t).mapError(fn(error) { E.Bad(t) }) }",
            "f"
        ),
        "\
fn @m.f(String) -> Result
  frame 10: s0!:ref s1:tag s2:int s3:tag s4:ref s5:tag s6:int s7:ref s8:ref s9:int
  local t -> s0:String [0, 7)
     0  call-builtin s5..s7:Result Int.parse (s0:String)
     1  alloc s8:ref closure m.f#0<closure>
     2  func-ref s9:int @m.f#0
     3  store-field s8:ref +0 s9:Int
     4  store-field s8:ref +1 s0:String
     5  call s1..s4:Result std.result.mapError<Int, Error, m.E> (s5..s7:Result s8:fn)
     6  return s1..s4:Result
"
    );
}
