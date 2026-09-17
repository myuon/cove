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
/// per piece (3 + 1 + 16 = 20). A literal run of several bytes is extended
/// from the string pool, a `String` piece is extended whole — with no
/// rendering call and no temporary string — and a one-byte run is a push of
/// its byte. An empty run is left out.
#[test]
fn an_interpolation_appends_each_part_to_one_buffer() {
    assert_eq!(
        listing(
            "fn greet(name: String) -> String { \"hi {name}!\" }",
            "greet"
        ),
        "\
fn @m.greet(String) -> String
  frame 6: s0!:ref s1:ref s2:int s3:ref s4:ref s5:int
  local name -> s0:String [0, 12)
     0  int s2:int 20
     1  growable-alloc.bytes s3:ref s2:int
     2  str s4:ref \"hi \"
     3  int s2:int 3
     4  int s5:int 0
     5  growable-extend.bytes (s3:ByteBuffer s4:String s5:Int s2:Int)
     6  len s2:int s0:ref
     7  growable-extend.bytes (s3:ByteBuffer s0:String s5:Int s2:Int)
     8  int s2:int 33
     9  growable-push.bytes s3:ref s2:int
    10  run-finish.bytes s1:ref s3:ref String utf8
    11  return s1:String
"
    );
}

/// An argument carries the layout of the location it names, so a `Point`
/// crosses into its rendering as the two words it already is: a piece that is
/// neither a `String` nor an `Int` is one `Value.renderInto` into the buffer.
///
/// It used to be boxed: a builtin was handed slot numbers and nothing else,
/// so an operand wider than a word had to carry its own description. That
/// cost an allocation per interpolated struct on a path the predecessor did
/// not allocate on, and the answer was `1` rather than `Point(x: 1, y: 2)`
/// wherever the box was skipped.
#[test]
fn an_inline_value_crosses_into_an_interpolation_where_it_sits() {
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn show(p: Point) -> String { \"p={p}\" }",
            "show"
        ),
        "\
fn @m.show(m.Point) -> String
  frame 8: s0!:int s1!:int s2:ref s3:int s4:ref s5:ref s6:int s7:unit
  local p -> s0..s1:m.Point [0, 9)
     0  int s3:int 18
     1  growable-alloc.bytes s4:ref s3:int
     2  str s5:ref \"p=\"
     3  int s3:int 2
     4  int s6:int 0
     5  growable-extend.bytes (s4:ByteBuffer s5:String s6:Int s3:Int)
     6  intrinsic-call s7:Unit Value.renderInto (s0..s1:m.Point s4:ByteBuffer)
     7  run-finish.bytes s2:ref s4:ref String utf8
     8  return s2:String
"
    );
}

/// Each piece is appended as soon as it has been evaluated, so a later piece
/// cannot change what an earlier one shows (#389): the `Point` is rendered
/// before `n + 1` is computed, and the sum is formatted after it.
#[test]
fn a_piece_is_appended_before_the_next_piece_is_evaluated() {
    let listed = listing(
        "struct Point { x: Int, y: Int }\nfn show(p: Point, n: Int) -> String { \"{p}{n + 1}\" }",
        "show",
    );
    let rendered = listed
        .find("Value.renderInto (s0..s1:m.Point")
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
/// through byte pushes alone: no intrinsic, no string, and nothing it calls. So
/// the inliner decides it as it decides any leaf: a site that runs once keeps
/// the call, and a piece inside a loop is expanded where it stands.
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
    assert!(hot.contains("growable-push.bytes"), "{hot}");

    let (sources, checked) = super::checked("fn show(n: Int) -> String { \"{n}\" }");
    let program = super::lower(&checked, &sources, &cove_schema::HostSchemas::new())
        .expect("the program lowers");
    let id = program
        .function_named("std.int", "renderInto")
        .expect("the renderer is lowered");
    let body = crate::print::function(&program, id);
    assert!(!body.contains("call"), "{body}");
    assert!(!body.contains("str "), "{body}");
    assert!(!body.contains("growable-extend"), "{body}");
    assert!(body.contains("growable-push.bytes"), "{body}");
}
