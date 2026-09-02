//! Calls across the boundary.

use cove_schema::{
    Effect, FieldSchema, HostSchemas, HostType, ModuleSchema, OperationSchema, ResourceSchema,
    TypeSchema,
};

use super::{listing, listing_with};

/// A host module no toolchain ships, described exactly as a shipped one is.
///
/// It declares both kinds of type a schema can: a `Book` the host keeps, and
/// an `Entry` it takes as ordinary data. An embedder is not a lesser kind of
/// host — `HostApi` is a trait — so both must reach the lowering the way
/// `files.Reader` and `http.Response` do.
const LEDGER: ModuleSchema = ModuleSchema {
    name: "ledger",
    capability: "ledger",
    operations: &[OperationSchema {
        name: "open",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Named("ledger.Book"),
        capability: "ledger",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[TypeSchema {
        name: "Entry",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "amount",
                ty: HostType::Int,
            },
            FieldSchema {
                name: "memo",
                ty: HostType::String,
            },
        ],
    }],
    resources: &[ResourceSchema {
        name: "Book",
        task_safe: true,
        operations: &[OperationSchema {
            name: "record",
            params: &[HostType::Named("ledger.Entry")],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "ledger",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        }],
    }],
};

fn ledger() -> HostSchemas {
    HostSchemas::new().with(LEDGER)
}

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

// ---- an operation of a host resource ----------------------------------

/// A resource operation is the boundary addressed the other way: the
/// receiver is an operand of its own, and the operation names the kind it
/// belongs to.
///
/// ADR 0013 makes a handle a name and gives the host the record of what is
/// open, so `writeLine` is dispatched on the word in `s0` and not on the
/// `files` the source wrote in front of the type. The receiver is not in the
/// argument list, because `HostRegistry::call_resource` does not take it as
/// an argument.
#[test]
fn an_operation_of_a_resource_is_addressed_to_the_handle() {
    assert_eq!(
        listing(
            "use files\nfn f(w: files.Writer, line: String) -> Result<Unit, Error> \
             { w.writeLine(line) }",
            "f"
        ),
        "\
fn0 m.f(<host> String) -> Result
  frame 8: s0!:host s1!:ref s2:int s3:unit s4:ref s5:int s6:unit s7:ref
     0  call-resource s5:int s0:host files.Writer.writeLine (s1:String)
     1  copy s2:int s5:int Result
     2  clear s5:int Result
     3  return s2:int
"
    );
}

/// The handle the operation is addressed to is an ordinary value, so it
/// reaches the call the way every other value does — here out of the `Ok`
/// of the `files.open` that issued it.
#[test]
fn a_resource_operation_reads_its_receiver_out_of_the_frame() {
    assert_eq!(
        listing(
            "use files\nfn f() -> Result<Unit, Error> {\n  \
             let reader = files.open(\"a.txt\")?\n  reader.close()?\n  Ok(())\n}",
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 17: s0:int s1:unit s2:ref s3:ref s4:int s5:host s6:ref s7:int s8:bool s9:host s10:int s11:unit s12:ref s13:unit s14:int s15:unit s16:ref
     0  str s3:ref \"a.txt\"
     1  call-host s4:int files.open (s3:String)
     2  clear s3:ref String
     3  int s7:int 0
     4  eq.int s8:bool s4:int s7:int
     5  branch-false s8:bool 8
     6  copy s9:host s5:host <host>
     7  jump 12
     8  int s10:int 1
     9  clear s11:unit Unit
    10  copy s12:ref s6:ref Error
    11  return s10:int
    12  clear s4:int Result
    13  call-resource s10:int s9:host files.Reader.close ()
    14  int s7:int 0
    15  eq.int s8:bool s10:int s7:int
    16  branch-false s8:bool 19
    17  copy s13:unit s11:unit Unit
    18  jump 23
    19  int s14:int 1
    20  clear s15:unit Unit
    21  copy s16:ref s12:ref Error
    22  return s14:int
    23  clear s10:int Result
    24  unit s13:unit
    25  int s10:int 0
    26  clear s12:ref <ref>
    27  copy s11:unit s13:unit Unit
    28  copy s0:int s10:int Result
    29  clear s10:int Result
    30  return s0:int
"
    );
}

// ---- a type an embedder's module declares ------------------------------

/// A type an embedder's module hands over is its fields in place, exactly as
/// a shipped module's is.
///
/// The schema is what says so, and the schemas the lowering reads are the
/// ones the *compilation* was given rather than `cove_schema::hosts`. Reading
/// only the shipped tables would give `ledger.Entry` no layout and refuse a
/// program the checker accepted against the same description.
#[test]
fn a_type_an_embedder_s_module_declares_is_its_fields_in_place() {
    assert_eq!(
        listing_with(
            "use ledger\nfn f(e: ledger.Entry) -> Int { e.amount }",
            &ledger(),
            "f"
        ),
        "\
fn0 m.f(ledger.Entry) -> Int
  frame 3: s0!:int s1!:ref s2:int
     0  copy s2:int s0:int Int
     1  return s2:int
"
    );
}

/// And a resource an embedder's module keeps is one `Repr::Host` word whose
/// operations are addressed to the handle.
#[test]
fn a_resource_an_embedder_s_module_keeps_answers_its_own_operations() {
    assert_eq!(
        listing_with(
            "use ledger\nfn f(b: ledger.Book, e: ledger.Entry) -> Result<Unit, Error> \
             { b.record(e) }",
            &ledger(),
            "f"
        ),
        "\
fn0 m.f(<host> ledger.Entry) -> Result
  frame 9: s0!:host s1!:int s2!:ref s3:int s4:unit s5:ref s6:int s7:unit s8:ref
     0  call-resource s6:int s0:host ledger.Book.record (s1:ledger.Entry)
     1  copy s3:int s6:int Result
     2  clear s6:int Result
     3  return s3:int
"
    );
}

// ---- initializing one --------------------------------------------------

/// `http.Route(method: ..., path: ..., handler: ...)` is a struct literal,
/// and its labels are field names rather than anything the boundary sees.
///
/// The oracle asks the schema for a type of that name before it asks for an
/// operation, and `interp::init_host_type` is `interp::init_struct` "with the
/// fields read from a schema instead of from a declaration". So this emits no
/// `call-host` at all: an initializer never crosses the boundary, and the
/// labelled argument that used to be refused as one was refused for a call
/// that was never a host call.
#[test]
fn a_host_type_is_initialized_with_labels_and_never_crosses_the_boundary() {
    assert_eq!(
        listing_with(
            "use ledger\nfn f() -> ledger.Entry { ledger.Entry(amount: 1, memo: \"rent\") }",
            &ledger(),
            "f"
        ),
        "\
fn0 m.f() -> ledger.Entry
  frame 6: s0:int s1:ref s2:int s3:ref s4:int s5:ref
     0  int s2:int 1
     1  str s3:ref \"rent\"
     2  copy s4:int s2:int Int
     3  copy s5:ref s3:ref String
     4  clear s3:ref String
     5  copy s0:int s4:int ledger.Entry
     6  clear s4:int ledger.Entry
     7  return s0:int
"
    );
}

/// A field the schema declared `Any` is where the erasure happens, and a case
/// of a host enum written into one is the discriminant it always was.
///
/// `http.Route`'s `handler` is one boxed word. What goes into it is a
/// declared function used as a value — an environment naming it and holding
/// nothing — and it is boxed on the way in exactly as a concrete value
/// written into a `dyn Trait` field of a declared struct is, because
/// `docs/LINEAR_VM.md` gives the two one representation.
///
/// `http.Method.Get` is `int 0`, the case index the schema counts, and it
/// reaches the field with no allocation and no boundary crossing at all.
#[test]
fn a_host_field_declared_any_is_boxed_on_the_way_in() {
    assert_eq!(
        listing(
            "use http\nfn health(r: http.Request) -> http.Response { http.json(200, 1) }\n\
             fn f() -> http.Route \
             { http.Route(method: http.Method.Get, path: \"/health\", handler: health) }",
            "f"
        ),
        "\
fn0 m.f() -> http.Route
  frame 11: s0:int s1:ref s2:ref s3:int s4:ref s5:ref s6:int s7:ref s8:int s9:ref s10:ref
     0  int s3:int 0
     1  str s4:ref \"/health\"
     2  alloc s5:ref closure m.health<closure>
     3  int s6:int 1
     4  store-field s5:ref +0 s6:int Int
     5  box s7:ref s5:ref fn
     6  clear s5:ref fn
     7  copy s8:int s3:int http.Method
     8  copy s9:ref s4:ref String
     9  copy s10:ref s7:ref Any
    10  clear s7:ref Any
    11  clear s4:ref String
    12  copy s0:int s8:int http.Route
    13  clear s8:int http.Route
    14  return s0:int
"
    );
}
