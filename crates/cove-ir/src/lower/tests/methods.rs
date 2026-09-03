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
  local s -> s0:String [0, 5)
     0  str s2:ref \",\"
     1  call-builtin s3:ref String.split (s0:String s2:String)
     2  clear s2:ref String
     3  copy s1:ref s3:ref Array
     4  clear s3:ref Array
     5  return s1:ref
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
     1  call-builtin s2:duration Duration.seconds (s1:Int)
     2  copy s0:duration s2:duration Duration
     3  return s0:duration
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
  local d -> s0:Duration [0, 2)
     0  call-builtin s2:int Duration.millis (s0:Duration)
     1  copy s1:int s2:int Int
     2  return s1:int
"
    );
}

/// The discriminant is word 0 of the value, so the answer is a comparison
/// against the location itself — a builtin for it would be a call into the
/// runtime to read one word the instruction set reads on its own.
#[test]
fn is_some_is_the_question_a_match_already_asks() {
    assert_eq!(
        listing("fn has(o: Option<Int>) -> Bool { o.isSome() }", "has"),
        "\
fn0 m.has(Option) -> Bool
  frame 5: s0!:int s1!:int s2:bool s3:int s4:bool
  local o -> s0:Option [0, 3)
     0  int s3:int 1
     1  eq.int s4:bool s0:int s3:int
     2  copy s2:bool s4:bool Bool
     3  return s2:bool
"
    );
}

/// The fallback is evaluated before the branch and whichever way it goes,
/// because it is an ordinary argument and the language evaluates a call's
/// arguments before the call.
#[test]
fn unwrap_or_is_that_question_and_a_branch() {
    assert_eq!(
        listing(
            "fn value(o: Option<Int>, other: Int) -> Int { o.unwrapOr(other) }",
            "value"
        ),
        "\
fn0 m.value(Option Int) -> Int
  frame 7: s0!:int s1!:int s2!:int s3:int s4:int s5:int s6:bool
  local o -> s0:Option [0, 7)
  local other -> s2:Int [0, 7)
     0  int s5:int 1
     1  eq.int s6:bool s0:int s5:int
     2  branch-false s6:bool 5
     3  copy s4:int s1:int Int
     4  jump 6
     5  copy s4:int s2:int Int
     6  copy s3:int s4:int Int
     7  return s3:int
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
  frame 9: s0!:ref s1:int s2:int s3:int s4:int s5:ref s6:int s7:int s8:bool
  local s -> s0:String [0, 10)
     0  call-builtin s3:int Int.parse (s0:String)
     1  int s6:int 0
     2  int s7:int 0
     3  eq.int s8:bool s3:int s7:int
     4  branch-false s8:bool 7
     5  copy s2:int s4:int Int
     6  jump 8
     7  copy s2:int s6:int Int
     8  clear s3:int Result
     9  copy s1:int s2:int Int
    10  return s1:int
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
  local p -> s0:m.Point [0, 2)
     0  call s3:int m.Point.sum (s0:m.Point)
     1  copy s2:int s3:int Int
     2  return s2:int
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
  frame 7: s0!:addr s1:unit s2:addr s3:int s4:int s5:int s6:unit
  local self -> s0:<addr> [0, 10)
     0  addr-of-part s2:addr s0:addr +1
     1  load s3:int s2:addr Int
     2  clear s2:addr <addr>
     3  int s4:int 1
     4  add.int s5:int s3:int s4:int
     5  addr-of-part s2:addr s0:addr +1
     6  store s2:addr s5:int Int
     7  clear s2:addr <addr>
     8  unit s6:unit
     9  copy s1:unit s6:unit Unit
    10  return s1:unit
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
             fn f(t: String) -> Result<Int, E> { Int.parse(t).mapError { E.Bad(t) } }",
            "f"
        ),
        "\
fn0 m.f(String) -> Result
  frame 15: s0!:ref s1:int s2:int s3:ref s4:int s5:int s6:ref s7:int s8:int s9:ref s10:ref s11:int s12:int s13:ref s14:bool
  local t -> s0:String [0, 20)
     0  call-builtin s7:int Int.parse (s0:String)
     1  alloc s10:ref closure m.f#0<closure>
     2  int s11:int 1
     3  store-field s10:ref +0 s11:int Int
     4  store-field s10:ref +1 s0:ref String
     5  int s11:int 0
     6  eq.int s14:bool s7:int s11:int
     7  branch-false s14:bool 12
     8  int s4:int 0
     9  clear s6:ref <ref>
    10  copy s5:int s8:int Int
    11  jump 15
    12  call-closure s12:int s10:ref ()
    13  int s4:int 1
    14  copy s5:int s12:int m.E
    15  clear s12:int m.E
    16  clear s10:ref fn
    17  clear s7:int Result
    18  copy s1:int s4:int Result
    19  clear s4:int Result
    20  return s1:int
"
    );
}

/// Whether the callback is handed the error it replaces is read off the
/// function type the checker settled, not off the syntax.
///
/// The oracle asks `Host::arity`, and `Checker::map_error` accepts a
/// callback written either way — so the settled type is the one place both
/// spellings have already agreed.
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
  local t -> s0:String [0, 19)
     0  call-builtin s7:int Int.parse (s0:String)
     1  alloc s10:ref closure m.f#0<closure>
     2  int s11:int 1
     3  store-field s10:ref +0 s11:int Int
     4  int s11:int 0
     5  eq.int s14:bool s7:int s11:int
     6  branch-false s14:bool 11
     7  int s4:int 0
     8  clear s6:ref <ref>
     9  copy s5:int s8:int Int
    10  jump 14
    11  call-closure s12:int s10:ref (s9:Error)
    12  int s4:int 1
    13  copy s5:int s12:int m.E
    14  clear s12:int m.E
    15  clear s10:ref fn
    16  clear s7:int Result
    17  copy s1:int s4:int Result
    18  clear s4:int Result
    19  return s1:int
"
    );
}
