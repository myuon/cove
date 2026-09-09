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
fn @m.f() -> Result
  frame 7: s0:tag s1:unit s2:ref s3:ref s4:tag s5:unit s6:ref
     0  str s3:ref \"hi\"
     1  call-host s4..s6:Result console.println (s3:String)
     2  copy s0..s2:Result s4..s6:Result
     3  return s0..s2:Result
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
fn @m.f(String) -> String
  frame 6: s0!:ref s1:ref s2:tag s3:ref s4:ref s5:ref
  local key -> s0:String [0, 6)
     0  call-host s2..s3:Option env.get (s0:String)
     1  str s4:ref \"\"
     2  call s5:String std.option.unwrapOr<String> (s2..s3:Option s4:String)
     3  clear s2..s3:Option
     4  copy s1:String s5:String
     5  return s1:String
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
fn @m.f() -> Result
  frame 7: s0:tag s1:unit s2:ref s3:ref s4:tag s5:unit s6:ref
     0  str s3:ref \"hi\"
     1  call-host s4..s6:Result console.println (s3:String)
     2  copy s0..s2:Result s4..s6:Result
     3  return s0..s2:Result
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
fn @m.f() -> Result
  frame 7: s0:tag s1:host s2:ref s3:ref s4:tag s5:host s6:ref
     0  str s3:ref \"a.txt\"
     1  call-host s4..s6:Result files.open (s3:String)
     2  copy s0..s2:Result s4..s6:Result
     3  return s0..s2:Result
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
fn @m.f(http.Response) -> Int
  frame 3: s0!:int s1!:ref s2:int
  local r -> s0..s1:http.Response [0, 2)
     0  copy s2:Int s0:Int
     1  return s2:Int
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
fn @m.f() -> Result
  frame 15: s0:tag s1:tag s2:host s3:ref s4:ref s5:tag s6:host s7:ref s8:host s9:tag s10:tag s11:host s12:ref s13:tag s14:host
     0  str s4:ref \"a\"
     1  call-host s5..s7:Result files.create (s4:String)
     2  switch s5:tag [3 5] else 5
     3  copy s8:<host> s6:<host>
     4  jump 8
     5  tag s9:tag Result.Err
     6  copy s12:Error s7:Error
     7  return s9..s12:Result
     8  clear s5..s7:Result
     9  tag s13:tag m.Sink.File
    10  copy s14:<host> s8:<host>
    11  tag s9:tag Result.Ok
    12  copy s10..s11:m.Sink s13..s14:m.Sink
    13  copy s0..s3:Result s9..s12:Result
    14  return s0..s3:Result
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
fn @m.f(<host> String) -> Result
  frame 8: s0!:host s1!:ref s2:tag s3:unit s4:ref s5:tag s6:unit s7:ref
  local w -> s0:<host> [0, 3)
  local line -> s1:String [0, 3)
     0  call-resource s5..s7:Result s0:host files.Writer.writeLine (s1:String)
     1  copy s2..s4:Result s5..s7:Result
     2  return s2..s4:Result
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
fn @m.f() -> Result
  frame 15: s0:tag s1:unit s2:ref s3:ref s4:tag s5:host s6:ref s7:host s8:tag s9:unit s10:ref s11:unit s12:tag s13:unit s14:ref
  local reader -> s7:<host> [9, 22)
     0  str s3:ref \"a.txt\"
     1  call-host s4..s6:Result files.open (s3:String)
     2  switch s4:tag [3 5] else 5
     3  copy s7:<host> s5:<host>
     4  jump 8
     5  tag s8:tag Result.Err
     6  copy s10:Error s6:Error
     7  return s8..s10:Result
     8  clear s4..s6:Result
     9  call-resource s8..s10:Result s7:host files.Reader.close ()
    10  switch s8:tag [11 13] else 13
    11  copy s11:Unit s9:Unit
    12  jump 16
    13  tag s12:tag Result.Err
    14  copy s14:Error s10:Error
    15  return s12..s14:Result
    16  clear s8..s10:Result
    17  unit s11:unit
    18  tag s8:tag Result.Ok
    19  clear s10:<ref>
    20  copy s9:Unit s11:Unit
    21  copy s0..s2:Result s8..s10:Result
    22  return s0..s2:Result
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
fn @m.f(ledger.Entry) -> Int
  frame 3: s0!:int s1!:ref s2:int
  local e -> s0..s1:ledger.Entry [0, 2)
     0  copy s2:Int s0:Int
     1  return s2:Int
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
fn @m.f(<host> ledger.Entry) -> Result
  frame 9: s0!:host s1!:int s2!:ref s3:tag s4:unit s5:ref s6:tag s7:unit s8:ref
  local b -> s0:<host> [0, 3)
  local e -> s1..s2:ledger.Entry [0, 3)
     0  call-resource s6..s8:Result s0:host ledger.Book.record (s1..s2:ledger.Entry)
     1  copy s3..s5:Result s6..s8:Result
     2  return s3..s5:Result
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
fn @m.f() -> ledger.Entry
  frame 6: s0:int s1:ref s2:int s3:ref s4:int s5:ref
     0  int s2:int 1
     1  str s3:ref \"rent\"
     2  copy s4:Int s2:Int
     3  copy s5:String s3:String
     4  copy s0..s1:ledger.Entry s4..s5:ledger.Entry
     5  return s0..s1:ledger.Entry
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
fn @m.f() -> http.Route
  frame 11: s0:tag s1:ref s2:ref s3:tag s4:ref s5:ref s6:int s7:ref s8:tag s9:ref s10:ref
     0  tag s3:tag http.Method.Get
     1  str s4:ref \"/health\"
     2  alloc s5:ref closure m.health<closure>
     3  func-ref s6:int @m.health
     4  store-field s5:ref +0 s6:Int
     5  box s7:ref s5:fn
     6  clear s5:fn
     7  copy s8:http.Method s3:http.Method
     8  copy s9:String s4:String
     9  copy s10:Any s7:Any
    10  clear s7:Any
    11  copy s0..s2:http.Route s8..s10:http.Route
    12  return s0..s2:http.Route
"
    );
}

/// A trailing lambda on a host operation is an ordinary last argument.
///
/// It used to stop at the boundary: a public closure carried a body only the
/// predecessor could run, so a host handed one would have gone looking for
/// something this run does not have. Now the location is one reference naming
/// the closure's environment and the boundary follows the word, so the call
/// lowers like any other and the host may call it back.
#[test]
fn a_trailing_lambda_reaches_a_host_operation() {
    let text = listing(
        "use clock\nexport fn f() -> Result<Int, Error> {\n  let v = clock.timeout(1s) { 1 }?\n  Ok(v + 1)\n}",
        "f",
    );
    assert!(
        text.contains("call-host") && text.contains("alloc"),
        "the closure is built and the call is emitted:\n{text}"
    );
    assert!(
        !text.contains("not yet lowered"),
        "and nothing is refused:\n{text}"
    );
}
