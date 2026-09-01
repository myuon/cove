//! Calls across the boundary.

use super::listing;

/// The boundary looks the pair up in the registry exactly as the
/// interpreter does, and the arguments are the locations in source order.
#[test]
fn a_host_call_names_the_module_and_the_operation_as_the_source_writes_them() {
    assert_eq!(
        listing(
            "use console.println\nfn f() -> Result<Unit, Error> { println(\"hi\") }",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 7: s0:int s1:unit s2:ref s3:ref s4:int s5:unit s6:ref
     0  str s3:ref \"hi\"
     1  call-host s4:int console.println (s3:String)
     2  clear s3:ref String
     3  copy s0:int s4:int Result
     4  clear s4:int Result
     5  return s0:int
"
    );
}

/// It is read where the checker recorded it rather than out of the schema
/// a second time: the checker resolved the operation against the schemas
/// this compilation was given, which includes an embedder's. An
/// `Option<String>` is two inline words, so the host writes a location
/// rather than a slot.
#[test]
fn the_answer_is_written_into_the_layout_the_schema_declared() {
    assert_eq!(
        listing(
            "use env.get\nfn f(key: String) -> String { get(key).unwrapOr(\"\") }",
            "f"
        ),
        "\
fn0 m.f(String) -> String
  frame 8: s0!:ref s1:ref s2:ref s3:int s4:ref s5:ref s6:int s7:bool
     0  call-host s3:int env.get (s0:String)
     1  str s5:ref \"\"
     2  int s6:int 1
     3  eq.int s7:bool s3:int s6:int
     4  branch-false s7:bool 7
     5  copy s2:ref s4:ref String
     6  jump 8
     7  copy s2:ref s5:ref String
     8  clear s5:ref String
     9  clear s3:int Option
    10  copy s1:ref s2:ref String
    11  clear s2:ref String
    12  return s1:ref
"
    );
}

#[test]
fn a_host_call_written_through_the_module_reaches_the_same_operation() {
    assert_eq!(
        listing(
            "use console\nfn f() -> Result<Unit, Error> { console.println(\"hi\") }",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 7: s0:int s1:unit s2:ref s3:ref s4:int s5:unit s6:ref
     0  str s3:ref \"hi\"
     1  call-host s4:int console.println (s3:String)
     2  clear s3:ref String
     3  copy s0:int s4:int Result
     4  clear s4:int Result
     5  return s0:int
"
    );
}
