//! String literals, and the interpolations in them.

use super::listing;

/// A string literal in a loop allocates once for the run, not once per
/// turn.
#[test]
fn a_literal_is_one_instruction_and_one_object_for_the_run() {
    assert_eq!(
        listing("fn hello() -> String { \"hello\" }", "hello"),
        "\
fn @m.hello() -> String
  frame 1: s0:ref
     0  str s0:ref \"hello\"
     1  return s0:String
"
    );
}

/// An interpolation is a byte buffer, one append per part, and a finish
/// (#403). The buffer is sized for the literal bytes and a small allowance
/// per piece (3 + 1 + 16 = 20). A literal run of several bytes is copied in
/// from the string pool and a `String` piece whole — with no rendering call and
/// no temporary string — by `std.stringbuilder.appendText`, and a one-byte run
/// is its byte, by `appendByteInto`. Both are expanded where they are called,
/// so each append is ADR 0062's window in the listing — the text's length, then
/// the buffer's length, ensure, store read, `run-copy` or `run-store`, clear
/// and commit — and neither leaves the `()` its call answers, which `frees`
/// drops. An empty run is left out.
#[test]
fn an_interpolation_appends_each_part_to_one_buffer() {
    assert_eq!(
        listing(
            "fn greet(name: String) -> String { \"hi {name}!\" }",
            "greet"
        ),
        "\
fn @m.greet(String) -> String
  frame 16: s0!:ref s1:ref s2:int s3:ref s4:ref s5:unit s6:unit s7:int s8:int s9:int s10:ref s11:unit s12:int s13:int s14:ref s15:int
  local name -> s0:String [0, 30)
     0  int s2:int 20
     1  growable-alloc.bytes s3:ref s2:int
     2  str s4:ref \"hi \"
     3  len s7:int s4:ref
     4  load-field s8:Int s3:ref +0
     5  growable-ensure.bytes s3:ref s7:int
     6  int s9:int 0
     7  load-field s10:<ref> s3:ref +1
     8  run-copy.bytes (s10:<ref> s8:Int s4:String s9:Int s7:Int)
     9  clear s10:<ref>
    10  growable-commit.bytes s3:ref s7:int
    11  len s7:int s0:ref
    12  load-field s8:Int s3:ref +0
    13  growable-ensure.bytes s3:ref s7:int
    14  int s9:int 0
    15  load-field s10:<ref> s3:ref +1
    16  run-copy.bytes (s10:<ref> s8:Int s0:String s9:Int s7:Int)
    17  clear s10:<ref>
    18  growable-commit.bytes s3:ref s7:int
    19  int s2:int 33
    20  load-field s12:Int s3:ref +0
    21  int s13:int 1
    22  growable-ensure.bytes s3:ref s13:int
    23  load-field s14:<ref> s3:ref +1
    24  run-store.bytes s14:ref s12:int s2:int
    25  clear s14:<ref>
    26  int s15:int 1
    27  growable-commit.bytes s3:ref s15:int
    28  run-finish.bytes s1:ref s3:ref String utf8
    29  return s1:String
"
    );
}

/// An argument carries the layout of the location it names, so a `Point`
/// crosses into its rendering as the two words it already is: a piece that is
/// neither a `String` nor an `Int` is **one call to the walk `lower::synth`
/// composed for its layout**, over the value and the buffer.
///
/// It used to be an `intrinsic-call` of `Value.renderInto`, and ADR 0064's
/// Decision 3 made it a call to a private function. What the call site keeps
/// is the *shape* the assertion below is about: the `Point` is passed where
/// it sits, two words wide, and the buffer beside it — a byte buffer is a
/// handle, so the callee's appends are this assembly's.
///
/// It used to be boxed, before either of those: a builtin was handed slot
/// numbers and nothing else, so an operand wider than a word had to carry its
/// own description. That cost an allocation per interpolated struct on a path
/// the predecessor did not allocate on, and the answer was `1` rather than
/// `Point(x: 1, y: 2)` wherever the box was skipped.
#[test]
fn an_inline_value_crosses_into_an_interpolation_where_it_sits() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn show(p: Point) -> String { \"p={p}\" }",
            "show"
        ),
        "\
fn @m.show(m.Point) -> String
  frame 12: s0!:int s1!:int s2:ref s3:int s4:ref s5:ref s6:unit s7:unit s8:int s9:int s10:int s11:ref
  local p -> s0..s1:m.Point [0, 14)
     0  int s3:int 18
     1  growable-alloc.bytes s4:ref s3:int
     2  str s5:ref \"p=\"
     3  len s8:int s5:ref
     4  load-field s9:Int s4:ref +0
     5  growable-ensure.bytes s4:ref s8:int
     6  int s10:int 0
     7  load-field s11:<ref> s4:ref +1
     8  run-copy.bytes (s11:<ref> s9:Int s5:String s10:Int s8:Int)
     9  clear s11:<ref>
    10  growable-commit.bytes s4:ref s8:int
    11  call s6:Unit <synth>.renders<m.Point#16> (s0..s1:m.Point s4:ByteBuffer)
    12  run-finish.bytes s2:ref s4:ref String utf8
    13  return s2:String
"
    );
}

/// Each piece is appended as soon as it has been evaluated, so a later piece
/// cannot change what an earlier one shows (#389): the `Point` is rendered
/// before `n + 1` is computed, and the sum is formatted after it.
///
/// The `Point`'s rendering is a call to a synthesized walk and the `Int`'s is
/// a call to `std.int.renderInto`, which is two different callees and one
/// order — the order the pieces were written in.
#[test]
fn a_piece_is_appended_before_the_next_piece_is_evaluated() {
    let listed = listing(
        "struct Point { x: Int, y: Int }\nfn show(p: Point, n: Int) -> String { \"{p}{n + 1}\" }",
        "show",
    );
    let rendered = listed
        .find("<synth>.renders<m.Point#16> (s0..s1:m.Point")
        .unwrap_or_else(|| panic!("{listed}"));
    let summed = listed.find("add.int").unwrap_or_else(|| panic!("{listed}"));
    let formatted = listed
        .find("call s6:Unit std.int.renderInto (s4:Int")
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(rendered < summed && summed < formatted, "{listed}");
}

#[test]
fn two_strings_compare_by_their_bytes() {
    assert_eq!(
        listing("fn same(a: String, b: String) -> Bool { a == b }", "same"),
        "\
fn @m.same(String String) -> Bool
  frame 3: s0!:ref s1!:ref s2:bool
  local a -> s0:String [0, 2)
  local b -> s1:String [0, 2)
     0  eq.str s2:bool s0:ref s1:ref
     1  return s2:Bool
"
    );
}

/// [ADR 0045](../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md)
/// places every entry of `Program::strings` in the heap once, so what makes
/// two occurrences of the same literal one object is the lowering
/// interning them into one entry before that ever runs — confirmed here
/// rather than assumed.
#[test]
fn two_occurrences_of_the_same_text_share_one_string_id() {
    let (sources, checked) = super::checked("fn twice() -> Bool { \"dup\" == \"dup\" }");
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    // Counted by text rather than by the table's length: the standard library
    // is lowered with every package, and `std.string`'s own messages are
    // literals of their own.
    assert_eq!(
        program
            .strings
            .iter()
            .filter(|text| &***text == "dup")
            .count(),
        1,
        "one text, written twice, is one entry: {:?}",
        program.strings
    );
}

/// An unmentioned literal still costs an entry: the lowering does not prune
/// the pool to what a particular run reaches, because it cannot know that —
/// and [ADR 0045] places every entry regardless. See
/// `crates/cove-runtime/src/vm/exec.rs`'s `Machine::place_literals`.
///
/// [ADR 0045]: ../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md
#[test]
fn a_literal_no_instruction_loads_is_still_in_the_pool() {
    let (sources, checked) = super::checked(
        "fn pick(flag: Bool) -> String { if flag { \"used\" } else { \"also used\" } }",
    );
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    // Both arms are reachable from `pick` and both are in the pool — the
    // interesting case, `\"also used\"`, is the one a run that always passes
    // `flag: true` would never load, and the pool holds it anyway.
    assert!(program.strings.iter().any(|s| &**s == "used"));
    assert!(program.strings.iter().any(|s| &**s == "also used"));
}

/// An `Int` piece is appended by `std.int.renderInto`, a standard-library
/// function the lowering calls on its own account (#403), with the value and
/// the buffer as its two operands.
///
/// It is an ordinary call, and the body it reaches is a leaf that renders
/// through ADR 0062's byte appends alone — a `run-store` window per byte — with
/// no intrinsic, no string, and nothing it calls. So the inliner decides it as
/// it decides any leaf: a site that runs once keeps the call, and a piece inside
/// a loop is expanded where it stands.
#[test]
fn an_int_piece_is_rendered_by_the_standard_library() {
    let cold = listing("fn show(n: Int) -> String { \"{n}\" }", "show");
    assert!(
        cold.contains("call s4:Unit std.int.renderInto (s0:Int s3:ByteBuffer)"),
        "{cold}"
    );
    let hot = listing(
        "fn count(n: Int) -> Int {\n  var bytes = 0\n  var i = 0\n  \
         while i < 10 {\n    bytes = bytes + \"{n}\".byteLength()\n    i = i + 1\n  }\n  bytes\n}",
        "count",
    );
    assert!(!hot.contains("call "), "{hot}");
    assert!(hot.contains("run-store.bytes"), "{hot}");

    let (sources, checked) = super::checked("fn show(n: Int) -> String { \"{n}\" }");
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    let id = program
        .function_named("std.int", "renderInto")
        .expect("the renderer is lowered");
    let body = crate::print::function(&program, id);
    assert!(!body.contains("call"), "{body}");
    assert!(!body.contains("str "), "{body}");
    let function = program.function(id);
    let windows = crate::legalize::windows(&program, function);
    assert_eq!(
        windows.len(),
        2,
        "the sign and each digit are one byte push window apiece: {body}"
    );
    assert!(
        windows
            .iter()
            .all(|window| window.pattern == crate::legalize::Pattern::PushByte),
        "{body}"
    );
}
