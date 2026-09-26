//! The methods of the types the language ships.

use super::listing;

/// The receiver is the first operand where there is one and the arguments
/// follow it in source order, which is the one shape every operation in
/// the table has.
///
/// The sample was `s.split(",")` until issue #454's Step 3 moved `split` into
/// `std.string`, and `x.format(2)` until `format` moved into `std.float`.
/// `Float.toInt` is the last method on the table that is a runtime call over
/// its receiver, and it has no argument after it, so "in source order" is a
/// claim this case can only make about the one operand now.
#[test]
fn a_builtin_method_is_one_call_over_its_operands() {
    assert_eq!(
        listing(
            "fn whole(x: Float) -> Result<Int, Error> { x.toInt() }",
            "whole"
        ),
        "\
fn @m.whole(Float) -> Result
  frame 4: s0!:float s1:tag s2:int s3:ref
  local x -> s0:Float [0, 2)
     0  intrinsic-call s1..s3:Result Float.toInt (s0:Float)
     1  return s1..s3:Result
"
    );
}

/// `Duration.nanos(1)` builds a duration and `d.nanos()` reads one back
/// out, and the language spells them the same. Neither is a runtime call:
/// a `Duration` is a count of nanoseconds in one word, so each is a relabel
/// conversion that moves the word into a slot of the other `Repr` (#378,
/// P5-2). `nanos` is the one unit that works this way — every other one
/// moved to `std.duration`, below.
#[test]
fn an_associated_function_has_no_receiver() {
    assert_eq!(
        listing("fn wait() -> Duration { Duration.nanos(1) }", "wait"),
        "\
fn @m.wait() -> Duration
  frame 2: s0:duration s1:int
     0  int s1:int 1
     1  int-to-duration s0:duration s1:int
     2  return s0:Duration
"
    );
}

#[test]
fn a_duration_reader_converts_its_receiver() {
    assert_eq!(
        listing("fn ns(d: Duration) -> Int { d.nanos() }", "ns"),
        "\
fn @m.ns(Duration) -> Int
  frame 2: s0!:duration s1:int
  local d -> s0:Duration [0, 2)
     0  duration-to-int s1:int s0:duration
     1  return s1:Int
"
    );
}

/// `Float.abs` is one instruction and not a runtime call.
///
/// [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
/// Decision 2 admits "a typed scalar operation that maps to a CPU or backend
/// operation" below the standard library and refuses an operation named after
/// a method, so `Intrinsic::FloatAbs` — which was `f64::abs` reached through an
/// `intrinsic-call` carrying the method's own name — is `Inst::FloatAbs`. The
/// listing is what says so: **no `intrinsic-call` at all**, where the same
/// function held one before, and the same two-slot frame `Int.toFloat` above
/// has.
#[test]
fn a_float_absolute_is_one_instruction() {
    assert_eq!(
        listing("fn f(x: Float) -> Float { x.abs() }", "f"),
        "\
fn @m.f(Float) -> Float
  frame 2: s0!:float s1:float
  local x -> s0:Float [0, 2)
     0  abs.float s1:float s0:float
     1  return s1:Float
"
    );
}

/// `Float.min` and `Float.max` are one instruction each and not runtime
/// calls, and the printer tells them apart.
///
/// The other half of ADR 0064's Decision 2 typed scalar operation, and the
/// listing is what says the pair became an instruction rather than being
/// renamed: **no `intrinsic-call` at all**, where the same function held one
/// before, and the three-slot frame a two-operand instruction has.
///
/// Both members, because they are one `Inst` with a flag: a lowering that
/// resolved `max` to `MinMax::Min` would answer the same frame, the same
/// slots and the same opcode count, and `min.float` against `max.float` in
/// the listing is the only thing here that would notice.
#[test]
fn a_float_extremum_is_one_instruction() {
    assert_eq!(
        listing("fn f(x: Float, y: Float) -> Float { x.min(y) }", "f"),
        "\
fn @m.f(Float Float) -> Float
  frame 3: s0!:float s1!:float s2:float
  local x -> s0:Float [0, 2)
  local y -> s1:Float [0, 2)
     0  min.float s2:float s0:float s1:float
     1  return s2:Float
"
    );
    assert_eq!(
        listing("fn f(x: Float, y: Float) -> Float { x.max(y) }", "f"),
        "\
fn @m.f(Float Float) -> Float
  frame 3: s0!:float s1!:float s2:float
  local x -> s0:Float [0, 2)
  local y -> s1:Float [0, 2)
     0  max.float s2:float s0:float s1:float
     1  return s2:Float
"
    );
}

/// `Float.round` is one instruction and not a runtime call.
///
/// The third of ADR 0064's Decision 2 typed scalar operations, and the
/// listing says the same thing the two above it say: **no `intrinsic-call` at
/// all**, where the same function held one before, and the same two-slot
/// frame `Float.abs` has.
///
/// That the *lowering* is one instruction and the *machine code* is seventeen
/// is the whole point of the migration and not a tension in it: what the call
/// cost was a crossing, and `benches/floatround` is where the two are
/// measured against each other.
#[test]
fn a_float_rounding_is_one_instruction() {
    assert_eq!(
        listing("fn f(x: Float) -> Float { x.round() }", "f"),
        "\
fn @m.f(Float) -> Float
  frame 2: s0!:float s1:float
  local x -> s0:Float [0, 2)
     0  round.float s1:float s0:float
     1  return s1:Float
"
    );
}

/// `Float.sqrt` is one instruction and not a runtime call.
///
/// The last of ADR 0064's Decision 2 typed scalar operations, and the listing
/// says what the three above it say: **no `intrinsic-call` at all**, where the
/// same function held one before, and the same two-slot frame `Float.abs` has.
///
/// Unlike `Float.round` there is no tension to explain away here — the
/// lowering is one instruction and the machine code is one instruction plus
/// the store, because x86-64 has `sqrtsd`.
#[test]
fn a_float_square_root_is_one_instruction() {
    assert_eq!(
        listing("fn f(x: Float) -> Float { x.sqrt() }", "f"),
        "\
fn @m.f(Float) -> Float
  frame 2: s0!:float s1:float
  local x -> s0:Float [0, 2)
     0  sqrt.float s1:float s0:float
     1  return s1:Float
"
    );
}

/// `Int.toFloat` is the conversion instruction the IR has always had, and
/// not a runtime call.
#[test]
fn an_int_converts_to_a_float_in_one_instruction() {
    assert_eq!(
        listing("fn f(n: Int) -> Float { n.toFloat() }", "f"),
        "\
fn @m.f(Int) -> Float
  frame 2: s0!:int s1:float
  local n -> s0:Int [0, 2)
     0  int-to-float s1:float s0:int
     1  return s1:Float
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
     2  int-to-duration s2:duration s4:int
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
     0  duration-to-int s3:int s0:duration
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
  frame 6: s0!:tag s1!:int s2:bool s3:bool s4:ref s5:ref
  local o -> s0..s1:Option [0, 9)
     0  switch s0:tag [3 1] else 5
     1  bool s2:bool true
     2  jump 8
     3  bool s2:bool false
     4  jump 8
     5  str s4:ref \"no `match` arm covers this value\"
     6  str s5:ref \"\"
     7  trap s4:ref, s5:ref, s5:ref
     8  return s2:Bool
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
  frame 8: s0!:tag s1!:int s2!:int s3:int s4:int s5:int s6:ref s7:ref
  local o -> s0..s1:Option [0, 10)
  local other -> s2:Int [0, 10)
     0  switch s0:tag [4 1] else 6
     1  copy s5:Int s1:Int
     2  copy s3:Int s5:Int
     3  jump 9
     4  copy s3:Int s2:Int
     5  jump 9
     6  str s6:ref \"no `match` arm covers this value\"
     7  str s7:ref \"\"
     8  trap s6:ref, s7:ref, s7:ref
     9  return s3:Int
"
    );
}

/// The machine builds the `Error` carrying a failure's message itself, so
/// the `Error` layout is interned here as well as the `Result`'s: the
/// `Result` describes its `Err` words without saying what declared them.
///
/// It was `Int.parse` until issue #454's Step 4 made that one
/// `std.int.parse`, whose `Err` is an ordinary Cove `Error(...)` and so says
/// nothing about what the *machine* interns. `Float.parse` is the parser that
/// is still the machine's, and the fact under test is unchanged: a `Result`
/// the runtime writes describes its `Err` words without naming what declared
/// them, so the `Error` layout has to be interned beside it.
#[test]
fn a_parser_answers_a_result_and_interns_the_error_it_may_carry() {
    assert_eq!(
        listing(
            "fn parse(s: String) -> Float { Float.parse(s).unwrapOr(0.0) }",
            "parse"
        ),
        "\
fn @m.parse(String) -> Float
  frame 10: s0!:ref s1:float s2:tag s3:float s4:ref s5:float s6:float s7:float s8:ref \
s9:ref
  local s -> s0:String [0, 12)
     0  intrinsic-call s2..s4:Result Float.parse (s0:String)
     1  float s5:float 0
     2  switch s2:tag [3 6] else 8
     3  copy s7:Float s3:Float
     4  copy s1:Float s7:Float
     5  jump 11
     6  copy s1:Float s5:Float
     7  jump 11
     8  str s8:ref \"no `match` arm covers this value\"
     9  str s9:ref \"\"
    10  trap s8:ref, s9:ref, s9:ref
    11  return s1:Float
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
  local self -> s0:<addr> [0, 8)
     0  addr-of-part s2:addr s0:addr +1
     1  load s3:Int s2:addr
     2  clear s2:<addr>
     3  add.int.imm s4:int s3:int 1
     4  addr-of-part s2:addr s0:addr +1
     5  store s2:addr s4:Int
     6  copy s1:Unit s5:Unit
     7  return s1:Unit
"
    );
}

/// `Vector.push` is `std.vector.push`, whose body is ADR 0062's append over the
/// element's words: the length, `growable-ensure` of one, the store read after
/// it, a `store-elem` at the length, and `growable-commit` of one. No statement
/// of the body writes a `()` of its own, the `()` the call answers is written
/// into a word nothing wrote before — which `lower::frees` drops as the zero it
/// already is — and the expansion does not clear the store slot the body
/// already cleared, so each push is exactly `crate::legalize`'s window: one row
/// fewer than the composite `growable-push` and its `unit` were. The body is
/// expanded wherever it is called, and neither a `call` nor an `intrinsic-call`
/// is left — at a one-word element and at a two-word one, whose source is the
/// whole run.
#[test]
fn a_push_is_ensure_store_commit_where_it_is_written() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\n\
             fn f(xs: Vector<Int>, ps: Vector<Point>, p: Point) -> Int {\n  \
               var ints = xs\n  var points = ps\n  ints.push(7)\n  points.push(p)\n  0\n}",
            "f"
        ),
        "\
fn @m.f(Vector Vector m.Point) -> Int
  frame 19: s0!:ref s1!:ref s2!:int s3!:int s4:int s5:ref s6:ref s7:int s8:unit s9:unit s10:int s11:int s12:ref s13:int s14:unit s15:int s16:int s17:ref s18:int
  local xs -> s0:Vector [0, 21)
  local ps -> s1:Vector [0, 21)
  local p -> s2..s3:m.Point [0, 21)
  local ints -> s5:Vector [1, 20)
  local points -> s6:Vector [2, 20)
     0  copy s5:Vector s0:Vector
     1  copy s6:Vector s1:Vector
     2  int s7:int 7
     3  load-field s10:Int s5:ref +0
     4  int s11:int 1
     5  growable-ensure.words Int s5:ref s11:int
     6  load-field s12:<ref> s5:ref +1
     7  store-elem s12:ref s10:int s7:Int
     8  clear s12:<ref>
     9  int s13:int 1
    10  growable-commit.words Int s5:ref s13:int
    11  load-field s15:Int s6:ref +0
    12  int s16:int 1
    13  growable-ensure.words m.Point s6:ref s16:int
    14  load-field s17:<ref> s6:ref +1
    15  store-elem s17:ref s15:int s2..s3:m.Point
    16  clear s17:<ref>
    17  int s18:int 1
    18  growable-commit.words m.Point s6:ref s18:int
    19  int s4:int 0
    20  return s4:Int
"
    );
}

/// `Vector.set` is `std.vector.set`: the range decision and the `Option` in
/// Cove, over ADR 0058's `core.vectorLoad` and `core.vectorStore`, which are a
/// `load-field` of the store and a `load-elem` or `store-elem` of it. What is
/// left at the call site is an ordinary call — the body is eighteen
/// instructions, past the leaf limit outside a loop — and no `intrinsic-call`.
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
  frame 12: s0!:int s1:ref s2:ref s3:int s4:ref s5:unit s6:unit s7:int s8:int s9:ref s10:int s11:ref
  local n -> s0:Int [0, 16)
  local building -> s4:Vector [6, 15)
     0  alloc s2:ref Vector<store> x0
     1  alloc s4:ref Vector<vector>
     2  int s3:int 0
     3  store-field s4:ref +0 s3:Int
     4  store-field s4:ref +1 s2:<ref>
     5  clear s2:<ref>
     6  load-field s7:Int s4:ref +0
     7  int s8:int 1
     8  growable-ensure.words Int s4:ref s8:int
     9  load-field s9:<ref> s4:ref +1
    10  store-elem s9:ref s7:int s0:Int
    11  clear s9:<ref>
    12  int s10:int 1
    13  growable-commit.words Int s4:ref s10:int
    14  run-finish.words Int s1:ref s4:ref Array unchecked
    15  return s1:Array
"
    );
}

/// `Vector.toArray` is `std.vector.toArray`, whose body is `core.vectorSlice` of
/// the whole vector: a length, a store and one word `run-slice` into the
/// `Array` of the element, expanded where it is called.
#[test]
fn a_to_array_is_a_word_run_slice_where_it_is_written() {
    assert_eq!(
        listing(
            "fn f(v: Vector<Int>) -> Array<Int> {\n  v.toArray()\n}",
            "f"
        ),
        "\
fn @m.f(Vector) -> Array
  frame 6: s0!:ref s1:ref s2:ref s3:int s4:int s5:ref
  local v -> s0:Vector [0, 5)
     0  int s3:int 0
     1  load-field s4:Int s0:ref +0
     2  load-field s5:<ref> s0:ref +1
     3  run-slice.words Int (s1:Array s5:<ref> s3:Int s4:Int)
     4  return s1:Array
"
    );
}

/// `Array.slice` is `std.array.slice`: the clamping is Cove, and beneath it
/// `core.arraySlice` is one word `run-slice` of the array itself.
#[test]
fn an_array_slice_clamps_in_cove_over_a_word_run_slice() {
    let source = "fn f(a: Array<Int>, x: Int, y: Int) -> Array<Int> {\n  a.slice(x, y)\n}";
    assert!(
        listing(source, "f")
            .contains("call s3:Array std.array.slice<Int> (s0:Array s1:Int s2:Int)"),
        "a cold call site calls the body"
    );
    let (sources, checked) = super::checked(source);
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == "std.array" && f.name.starts_with("slice<"))
        .map(|at| crate::FunctionId(at as u32))
        .expect("the body was lowered");
    assert_eq!(
        crate::print::function(&program, id),
        "\
fn @std.array.slice<Int>(Array Int Int) -> Array
  frame 9: s0!:ref s1!:int s2!:int s3:ref s4:int s5:int s6:bool s7:int s8:int
  local items -> s0:Array [0, 18)
  local from -> s1:Int [0, 18)
  local to -> s2:Int [0, 18)
  local length -> s4:Int [1, 17)
  local start -> s5:Int [8, 17)
  local end -> s7:Int [15, 17)
     0  len s4:int s0:ref
     1  lt.int.imm.branch s6:bool s1:int 0 4
     2  int s5:int 0
     3  jump 8
     4  gt.int.branch s6:bool s1:int s4:int 7
     5  copy s5:Int s4:Int
     6  jump 8
     7  copy s5:Int s1:Int
     8  lt.int.branch s6:bool s2:int s5:int 11
     9  copy s7:Int s5:Int
    10  jump 15
    11  gt.int.branch s6:bool s2:int s4:int 14
    12  copy s7:Int s4:Int
    13  jump 15
    14  copy s7:Int s2:Int
    15  sub.int s8:int s7:int s5:int
    16  run-slice.words Int (s3:Array s0:Array s5:Int s8:Int)
    17  return s3:Array
"
    );
}

/// `Vector.pop` and `Vector.remove` are `std.vector` bodies: the index decided
/// and the `Option` built in Cove, over an element load, a word `run-copy` of
/// the store into itself for `remove`'s tail, and a word `growable-truncate`
/// that lowers the length and clears what it vacates.
#[test]
fn pop_and_remove_are_cove_over_a_growable_truncate() {
    let source = "fn f(v: Vector<Int>) -> Option<Int> {\n  var w = v\n  w.remove(1)\n  w.pop()\n}";
    let (sources, checked) = super::checked(source);
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    let body = |name: &str| {
        let id = program
            .functions
            .iter()
            .position(|f| &*f.module == "std.vector" && f.name.starts_with(name))
            .map(|at| crate::FunctionId(at as u32))
            .unwrap_or_else(|| panic!("`{name}` was lowered"));
        crate::print::function(&program, id)
    };
    assert_eq!(
        body("pop<"),
        "\
fn @std.vector.pop<Int>(Vector) -> Option
  frame 11: s0!:ref s1:tag s2:int s3:int s4:bool s5:tag s6:int s7:int s8:ref s9:int s10:unit
  local items -> s0:Vector [0, 15)
  local length -> s3:Int [1, 14)
  local last -> s9:Int [9, 14)
     0  load-field s3:Int s0:ref +0
     1  eq.int.imm.branch s4:bool s3:int 0 5
     2  tag s5:tag Option.None
     3  copy s1..s2:Option s5..s6:Option
     4  jump 14
     5  sub.int.imm s7:int s3:int 1
     6  load-field s8:<ref> s0:ref +1
     7  load-elem s9:Int s8:ref s7:int
     8  clear s8:<ref>
     9  sub.int.imm s7:int s3:int 1
    10  growable-truncate.words Int s0:ref s7:int
    11  tag s5:tag Option.Some
    12  copy s6:Int s9:Int
    13  copy s1..s2:Option s5..s6:Option
    14  return s1..s2:Option
"
    );
    assert_eq!(
        body("remove<"),
        "\
fn @std.vector.remove<Int>(Vector Int) -> Option
  frame 14: s0!:ref s1!:int s2:tag s3:int s4:int s5:bool s6:ref s7:int s8:int s9:int s10:int s11:unit s12:tag s13:int
  local items -> s0:Vector [0, 21)
  local index -> s1:Int [0, 21)
  local length -> s4:Int [1, 20)
  local was -> s7:Int [6, 17)
     0  load-field s4:Int s0:ref +0
     1  ge.int.imm.branch s5:bool s1:int 0 3
     2  lt.int s5:bool s1:int s4:int
     3  branch-false s5:bool 18
     4  load-field s6:<ref> s0:ref +1
     5  load-elem s7:Int s6:ref s1:int
     6  add.int.imm s8:int s1:int 1
     7  sub.int s9:int s4:int s1:int
     8  sub.int.imm s10:int s9:int 1
     9  load-field s6:<ref> s0:ref +1
    10  run-copy.words Int (s6:<ref> s1:Int s6:<ref> s8:Int s10:Int)
    11  clear s6:<ref>
    12  sub.int.imm s8:int s4:int 1
    13  growable-truncate.words Int s0:ref s8:int
    14  tag s12:tag Option.Some
    15  copy s13:Int s7:Int
    16  copy s2..s3:Option s12..s13:Option
    17  jump 20
    18  tag s12:tag Option.None
    19  copy s2..s3:Option s12..s13:Option
    20  return s2..s3:Option
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
     0  call s5..s7:Result std.int.parse (s0:String)
     1  alloc s8:ref closure m.f#0<closure>
     2  func-ref s9:int @m.f#0
     3  store-field s8:ref +0 s9:Int
     4  store-field s8:ref +1 s0:String
     5  call s1..s4:Result std.result.mapError<Int, Error, m.E> (s5..s7:Result s8:fn)
     6  return s1..s4:Result
"
    );
}

/// `String.sliceBytes` is `std.string.sliceBytes`: the range decided in Cove,
/// and beneath it `core.stringSlice` is one byte `run-slice` of the string.
///
/// The listing is the body's whole cost on the path that answers: a length,
/// five fused comparisons, a byte load per end and a comparison of each against
/// 128 (an ASCII byte stops there), the count, the slice, and the `Ok` written
/// straight into what is returned. Every refusal is a `call` of `refuseRange`
/// answering into the body's own answer, so none of the five sentences is on
/// that path.
#[test]
fn slice_bytes_decides_its_range_in_cove_over_a_byte_run_slice() {
    let source =
        "fn f(s: String, x: Int, y: Int) -> Result<String, Error> {\n  s.sliceBytes(x, y)\n}";
    let (sources, checked) = super::checked(source);
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == "std.string" && &*f.name == "sliceBytes")
        .map(|at| crate::FunctionId(at as u32))
        .expect("the body was lowered");
    assert_eq!(
        crate::print::function(&program, id),
        "\
fn @std.string.sliceBytes(String Int Int) -> Result
  frame 12: s0!:ref s1!:int s2!:int s3:tag s4:ref s5:int s6:bool s7:unit s8:int s9:ref s10:tag s11:ref
  local text -> s0:String [0, 28)
  local from -> s1:Int [0, 28)
  local to -> s2:Int [0, 28)
  local length -> s5:Int [1, 27)
  local first -> s8:Int [12, 16)
  local last -> s8:Int [18, 22)
     0  len s5:int s0:ref
     1  lt.int.imm.branch s6:bool s1:int 0 4
     2  call s3..s4:Result std.string.refuseRange (s0:String s1:Int s2:Int)
     3  return s3..s4:Result
     4  gt.int.branch s6:bool s1:int s2:int 7
     5  call s3..s4:Result std.string.refuseRange (s0:String s1:Int s2:Int)
     6  return s3..s4:Result
     7  gt.int.branch s6:bool s2:int s5:int 10
     8  call s3..s4:Result std.string.refuseRange (s0:String s1:Int s2:Int)
     9  return s3..s4:Result
    10  lt.int.branch s6:bool s1:int s5:int 16
    11  run-load.bytes s8:int s0:ref s1:int
    12  ge.int.imm.branch s6:bool s8:int 128 16
    13  lt.int.imm.branch s6:bool s8:int 192 16
    14  call s3..s4:Result std.string.refuseRange (s0:String s1:Int s2:Int)
    15  return s3..s4:Result
    16  lt.int.branch s6:bool s2:int s5:int 22
    17  run-load.bytes s8:int s0:ref s2:int
    18  ge.int.imm.branch s6:bool s8:int 128 22
    19  lt.int.imm.branch s6:bool s8:int 192 22
    20  call s3..s4:Result std.string.refuseRange (s0:String s1:Int s2:Int)
    21  return s3..s4:Result
    22  sub.int s8:int s2:int s1:int
    23  run-slice.bytes (s9:String s0:String s1:Int s8:Int)
    24  tag s10:tag Result.Ok
    25  copy s11:String s9:String
    26  return s10..s11:Result
    27  return s3..s4:Result
"
    );
}
