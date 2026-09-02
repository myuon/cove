//! A value whose type was *intentionally* erased.
//!
//! `docs/LINEAR_VM.md` draws the line these cases are about:
//!
//! > A value whose type is *intentionally* erased — `dyn Trait`, a Host
//! > result a schema declared `Any` — is one `Ref` word naming a `Boxed`
//! > object.
//!
//! > A `Ty::Unknown` is not that. It is the checker declining, and a program
//! > the checker declined about is a compile error.
//!
//! Both halves are asserted here, because only asserting the first would let
//! "every unknown is boxed" pass. The checker cannot tell them apart —
//! [ADR 0016](../../../../docs/adr/0016-four-kinds-of-unknown.md) gives a
//! schema's `Any` and a type parameter nothing settles the same
//! `Unknown::Unconstrained` — so what tells them apart here is the *schema*,
//! read at the call, and nothing else in the lowering may consult an
//! unknown's kind.

use cove_schema::{Effect, HostSchemas, HostType, ModuleSchema, OperationSchema, ResourceSchema};

use super::{listing_with, refused};

/// A host module that declares `Any` in the three places a schema can: a
/// result, a result nested inside a `Result`, and a resource operation's
/// result.
///
/// An embedder's rather than a shipped one, because that is the harder case:
/// the lowering has to read the schemas *this compilation was given*, and a
/// pass that consulted `cove_schema::hosts` would answer for fewer programs
/// than the checker accepted. `clock.timeout` is the shipped one and is
/// covered by the corpus.
const ORACLE: ModuleSchema = ModuleSchema {
    name: "oracle",
    capability: "oracle",
    operations: &[
        OperationSchema {
            name: "ask",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Any,
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "open",
            params: &[],
            variadic: false,
            result: HostType::Named("oracle.Seat"),
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[ResourceSchema {
        name: "Seat",
        task_safe: true,
        operations: &[OperationSchema {
            name: "next",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Any, &HostType::Error),
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        }],
    }],
};

fn oracle() -> HostSchemas {
    HostSchemas::new().with(ORACLE)
}

/// The answer is one `Repr::Ref` word at the program-wide `Boxed` layout,
/// and the operation that reads it opens it at the type the other operand
/// names.
#[test]
fn a_host_result_a_schema_declared_any_is_one_boxed_word() {
    assert_eq!(
        listing_with(
            "use oracle.ask\nfn f() -> Int { ask(\"n\") + 1 }",
            &oracle(),
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 6: s0:int s1:ref s2:ref s3:int s4:int s5:int
     0  str s1:ref \"n\"
     1  call-host s2:ref oracle.ask (s1:String)
     2  clear s1:ref String
     3  int s3:int 1
     4  unbox s4:int s2:ref Int
     5  clear s2:ref Any
     6  add.int s5:int s4:int s3:int
     7  copy s0:int s5:int Int
     8  return s0:int
"
    );
}

/// `Result<Any, Error>` is an ordinary enum whose `Ok` carries a box, so `?`
/// is the ordinary path and answers the box.
///
/// The checker records the whole `?` as an unconstrained unknown, which has
/// no layout. What decides the destination's width is the case being
/// unwrapped, and that is a fact the layout table holds whatever the checker
/// settled.
#[test]
fn a_question_mark_on_an_erased_result_answers_the_box() {
    assert_eq!(
        listing_with(
            "use oracle.open\n\
             fn f() -> Result<Int, Error> {\n  \
               let s = open()\n  \
               let v = s.next()?\n  \
               Ok(v * 2)\n\
             }",
            &oracle(),
            "f"
        ),
        "\
fn0 m.f() -> Result
  frame 14: s0:int s1:int s2:ref s3:host s4:int s5:ref s6:int s7:bool s8:ref s9:int s10:int s11:ref s12:int s13:int
     0  call-host s3:host oracle.open ()
     1  call-resource s4:int s3:host oracle.Seat.next ()
     2  int s6:int 0
     3  eq.int s7:bool s4:int s6:int
     4  branch-false s7:bool 7
     5  copy s8:ref s5:ref Any
     6  jump 11
     7  int s9:int 1
     8  clear s10:int Int
     9  copy s11:ref s5:ref Error
    10  return s9:int
    11  clear s4:int Result
    12  int s6:int 2
    13  unbox s12:int s8:ref Int
    14  mul.int s13:int s12:int s6:int
    15  int s9:int 0
    16  clear s11:ref <ref>
    17  copy s10:int s13:int Int
    18  copy s0:int s9:int Result
    19  clear s9:int Result
    20  clear s8:ref Any
    21  return s0:int
"
    );
}

/// Where the unbox target comes from when the operand beside it is not what
/// says: a declared parameter's own type.
///
/// This is the general rule and the operator is the special case. A written
/// type at the place the value is used is what the box is opened at, and
/// `Body::fit` is where every such place meets it — an argument, a return, a
/// field, an annotated binding.
#[test]
fn a_declared_parameter_says_what_an_erased_argument_is_opened_at() {
    assert_eq!(
        listing_with(
            "use oracle.ask\nfn g(n: Int) -> Int { n }\nfn f() -> Int { g(ask(\"n\")) }",
            &oracle(),
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 5: s0:int s1:ref s2:ref s3:int s4:int
     0  str s1:ref \"n\"
     1  call-host s2:ref oracle.ask (s1:String)
     2  clear s1:ref String
     3  unbox s3:int s2:ref Int
     4  clear s2:ref Any
     5  call s4:int m.g (s3:Int)
     6  copy s0:int s4:int Int
     7  return s0:int
"
    );
}

/// A written `dyn Trait` and a schema's `Any` are one representation, so
/// they are one layout and the same two instructions.
///
/// This is the "unify rather than adding a second one" question, asked of a
/// program that does both: a `Tag` erased into a written `dyn Named`, and a
/// host answer a schema declared `Any`. The table has one `Shape::Boxed`
/// entry in it — `lower::tests::layouts` holds every program to that — and
/// what this adds is that the erasure the *host* introduced is that entry
/// rather than one of its own. `Inst::Box` and `Inst::Unbox` are the only
/// two instructions either kind reaches, and `Body::fit` is the only place
/// that emits them.
#[test]
fn a_schema_s_any_and_a_written_dyn_are_the_same_box() {
    let (sources, checked) = super::checked_with(
        "use oracle.ask\n\
         trait Named { fn name(self) -> String }\n\
         struct Tag { name: String }\n\
         impl Named for Tag { fn name(self) -> String { self.name } }\n\
         fn f() -> Int {\n  \
           let seen: dyn Named = Tag(name: \"t\")\n  \
           ask(seen.name())\n\
         }",
        &oracle(),
    );
    let program = super::super::lower(&checked, &sources, &oracle()).expect("the program lowers");
    let boxes: Vec<(usize, &str)> = program
        .layouts
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it.shape, crate::layout::Shape::Boxed))
        .map(|(at, it)| (at, &*it.name))
        .collect();
    assert_eq!(boxes.len(), 1, "{boxes:?}");
    let (boxed, _) = boxes[0];
    let boxed = crate::layout::LayoutId(boxed as u32);

    // The `dyn` local's box and the host answer's location are the same
    // layout, and the host operation's declared result says so before any
    // instruction is emitted.
    assert_eq!(program.boxed_layout, boxed);
    let op = program
        .host_ops
        .iter()
        .find(|op| &*op.operation == "ask")
        .expect("the call to `oracle.ask` is in the program");
    assert_eq!(op.result, boxed);

    // Two `Unbox`es and one `Box`, and every one of them names that layout's
    // contents rather than a second erasure of its own: the `dyn` receiver
    // opened for the call it dispatches, and the host answer opened at the
    // `Int` the return type names.
    let f = program
        .functions
        .iter()
        .find(|f| &*f.name == "f")
        .expect("`f` was lowered");
    let opened: Vec<&str> = f
        .code
        .iter()
        .filter_map(|inst| match inst {
            crate::Inst::Box { layout, .. } => Some(&*program.layouts[layout.index()].name),
            crate::Inst::Unbox { layout, .. } => Some(&*program.layouts[layout.index()].name),
            _ => None,
        })
        .collect();
    assert_eq!(opened, vec!["m.Tag", "m.Tag", "Int"]);
}

/// A type parameter nothing settles is still a compile error, and this is
/// the case that says the two kinds of not-knowing did not become one.
///
/// `Vector.of()` whose element type nothing states, and `Ok(1)` in a place
/// expecting no `Result`, are both `Unknown::Unconstrained` — the same value
/// a schema's `Any` produces. ADR 0016 names the second as one of the two
/// silences a clean check does not cover, and neither is erasure: nothing
/// said "any value", something failed to say anything. A lowering that
/// boxed an unknown rather than reading the schema would run these.
#[test]
fn a_type_parameter_nothing_settles_is_not_erasure() {
    assert_eq!(
        refused("fn f() { var v = Vector.of()\n  v.push(v) }"),
        vec![
            "not yet lowered: a value of type `Vector<_>`".to_string(),
            "the type of this expression was never settled, so it cannot be lowered: `_`"
                .to_string(),
            "not yet lowered: a value of type `Vector<_>`".to_string(),
        ]
    );
    assert_eq!(
        refused("fn f() -> Int {\n  let ok = Ok(1)\n  2\n}"),
        vec!["not yet lowered: a value of type `Result<Int, _>`".to_string()]
    );
}

/// A method call on an erased value: nothing says what to open the box at,
/// and the gap says so rather than guessing.
///
/// The receiver's type is the unknown itself, so no written type and no
/// operand beside it names one. Which `length` is meant is the question, and
/// only a run-time type test could answer it — a dispatch over every layout
/// that has such a method, which is a language decision and not a lowering's.
#[test]
fn a_method_on_an_erased_value_is_a_use_nothing_says_the_type_of() {
    let (sources, checked) = super::checked_with(
        "use oracle.ask\nfn f() -> Int { ask(\"n\").length() }",
        &oracle(),
    );
    let items = super::super::lower(&checked, &sources, &oracle())
        .expect_err("a method call on an erased value has no receiver type");
    let said: Vec<String> = items.into_iter().map(|item| item.message).collect();
    assert!(
        said.iter()
            .any(|item| item == "not yet lowered: a method call"),
        "{said:?}"
    );
}
