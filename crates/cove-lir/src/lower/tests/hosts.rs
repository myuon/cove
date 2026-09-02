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

// ---- the types a host module declares ---------------------------------

/// A handle is one word, and it is neither a scalar nor a reference.
///
/// ADR 0013: the host keeps whatever a `files.Reader` really is and Cove
/// holds the name of it, so the word is an index into the run's resource
/// table and the collector traces nothing through it. The `Result` over one
/// is a discriminant, that word, and the `Err`'s own — which cannot share
/// the handle's word, because a payload word is one `Repr` for every case.
#[test]
fn a_host_resource_is_one_word_that_is_not_a_root() {
    assert_eq!(
        listing(
            "use files\nfn f() -> Result<files.Reader, Error> { files.open(\"a.txt\") }",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 7: s0:int s1:host s2:ref s3:ref s4:int s5:host s6:ref
     0  str s3:ref \"a.txt\"
     1  call-host s4:int files.open (s3:String)
     2  clear s3:ref String
     3  copy s0:int s4:int Result
     4  clear s4:int Result
     5  return s0:int
"
    );
}

/// A host type the host *hands over* is ordinary data, so it is its fields
/// in place and reaching one emits nothing at all.
///
/// `http.Response` is `[status: Int, body: Ref]`, two words, and the
/// parameter occupies both. `TypeSchema`'s own documentation is what this
/// follows: a host type needs no representation of its own, and the layout's
/// name is the qualified one the boundary materialises a `Value::Struct`
/// under.
#[test]
fn a_host_type_the_host_hands_over_is_its_fields_in_place() {
    assert_eq!(
        listing("use http\nfn f(r: http.Response) -> Int { r.status }", "f"),
        "\
fn0 m.f(http.Response) -> Int
  frame 3: s0!:int s1!:ref s2:int
     0  copy s2:int s0:int Int
     1  return s2:int
"
    );
}

/// A handle is an ordinary payload, and the case that does not carry one
/// zeroes its word like any other.
#[test]
fn a_host_resource_is_a_case_s_payload_like_anything_else() {
    assert_eq!(
        listing(
            "use files\nenum Sink { Console, File(files.Writer) }\n\
             fn f() -> Result<Sink, Error> { Ok(Sink.File(files.create(\"a\")?)) }",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 17: s0:int s1:int s2:host s3:ref s4:ref s5:int s6:host s7:ref s8:int s9:bool s10:host s11:int s12:int s13:host s14:ref s15:int s16:host
     0  str s4:ref \"a\"
     1  call-host s5:int files.create (s4:String)
     2  clear s4:ref String
     3  int s8:int 0
     4  eq.int s9:bool s5:int s8:int
     5  branch-false s9:bool 8
     6  copy s10:host s6:host <host>
     7  jump 13
     8  int s11:int 1
     9  clear s12:int Int
    10  clear s13:host <host>
    11  copy s14:ref s7:ref Error
    12  return s11:int
    13  clear s5:int Result
    14  int s15:int 1
    15  copy s16:host s10:host <host>
    16  int s11:int 0
    17  clear s14:ref <ref>
    18  copy s12:int s15:int m.Sink
    19  copy s0:int s11:int Result
    20  clear s11:int Result
    21  return s0:int
"
    );
}
