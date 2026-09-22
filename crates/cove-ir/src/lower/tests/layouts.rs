//! The layout table a program lowers to.
//!
//! A layout describes a *family*, not an instantiation, and it is interned —
//! so one shape is one [`LayoutId`](crate::LayoutId) however many times the
//! source writes it. These cases pin what a family is, which is the question
//! every width, every reference map and every field offset is answered from.

use cove_schema::HostSchemas;

use super::checked;
use crate::layout::{Layout, Shape};
use crate::lower::lower;
use crate::repr::Repr;

/// Every layout of a lowered program, by name and words.
fn layouts(source: &str) -> Vec<Layout> {
    let (sources, checked) = checked(source);
    lower(&checked, &sources, &HostSchemas::new())
        .expect("the program lowers")
        .layouts
        .clone()
}

/// The words of the one layout called `name`, and a panic if there is not
/// exactly one.
fn words(source: &str, name: &str) -> Vec<Repr> {
    let held = layouts(source);
    let found: Vec<&Layout> = held.iter().filter(|it| &*it.name == name).collect();
    assert_eq!(found.len(), 1, "expected one `{name}` in {held:?}");
    found[0].words.clone()
}

#[test]
fn a_struct_is_the_words_of_its_fields_and_its_name_is_qualified() {
    // Two modules may each declare a `Point`, so a layout is an identity
    // rather than a shape: the name carries the module that declared it.
    assert_eq!(
        words(
            "struct Point { x: Int, y: Int }\nfn f() -> Point { Point(x: 1, y: 2) }",
            "m.Point"
        ),
        vec![Repr::Int, Repr::Int]
    );
}

#[test]
fn nesting_is_inline_and_recursive() {
    // A `Line` has no indirection in it at all, which is what makes
    // `l.from.x` a slot offset known at lowering time.
    assert_eq!(
        words(
            "struct Point { x: Int, y: Int }\n\
             struct Line { from: Point, to: Point }\n\
             fn f(l: Line) -> Int { l.from.x }",
            "m.Line"
        ),
        vec![Repr::Int; 4]
    );
}

#[test]
fn a_family_that_lives_in_the_heap_is_one_reference() {
    // A value has a static width or it lives in the heap. A vector is the
    // second, and its header and the store beneath it are two layouts —
    // both declared where a value of the type is met, because growth
    // replaces the store and the only thing that says what a new one looks
    // like is this table.
    //
    // **Four, not two, and the other two are not this program's.** Issue
    // #454's Step 3 gave `std.string.chars` a `Vector<String>` of its own, and
    // `lower` lowers the whole standard library attached to every package, so
    // a header and a store for *that* instantiation are declared here whatever
    // the program under test says. What the assertion is about is unchanged and
    // is the loop below rather than the count: every one of them is **one
    // reference wide**, which is what "lives in the heap" means as a layout.
    let held = layouts("fn f(v: Vector<Int>) -> Int { v.length() }");
    let vectors: Vec<&Layout> = held.iter().filter(|it| &*it.name == "Vector").collect();
    assert_eq!(vectors.len(), 4);
    for layout in vectors {
        assert_eq!(layout.words, vec![Repr::Ref]);
    }
}

#[test]
fn a_struct_holding_a_vector_is_words_then_an_address() {
    // ADR 0001's rule, as a layout: the `Point` words are inline and the
    // `Vector` is one address, so one copy makes the first independent and
    // leaves the second shared.
    assert_eq!(
        words(
            "struct Point { x: Int, y: Int }\n\
             struct Wrapper { p: Point, v: Vector<Int> }\n\
             fn f(w: Wrapper) -> Int { w.p.x }",
            "m.Wrapper"
        ),
        vec![Repr::Int, Repr::Int, Repr::Ref]
    );
}

#[test]
fn an_enums_payload_words_agree_across_its_cases() {
    // `A` takes payload words 0 and 1; `B` can use neither, so its `Float`
    // takes a third. Four words between them, wider than either case, and
    // that is the price of a map a collection can read without asking which
    // case a value is in.
    assert_eq!(
        words(
            "enum E { A(Int, String), B(Float) }\nfn f(x: Float) -> E { E.B(x) }",
            "m.E"
        ),
        vec![Repr::Tag, Repr::Int, Repr::Ref, Repr::Float]
    );
}

#[test]
fn two_cases_of_one_shape_share_their_payload_words() {
    assert_eq!(
        words(
            "enum E { Left(Int), Right(Int) }\nfn f() -> E { E.Left(1) }",
            "m.E"
        ),
        vec![Repr::Tag, Repr::Int]
    );
}

#[test]
fn a_layout_describes_a_family_rather_than_an_instantiation() {
    // `Array<String>` and `Array<Point>` would be one layout because a
    // reference is a reference; `Option<Int>` and `Option<Float>` are two
    // because their words differ and a boundary has to know which.
    let held = layouts(
        "fn f(a: Option<Int>, b: Option<Float>, c: Option<Int>) -> Int {\n  a.unwrapOr(0)\n}",
    );
    let options: Vec<&Layout> = held.iter().filter(|it| &*it.name == "Option").collect();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0].words, vec![Repr::Tag, Repr::Int]);
    assert_eq!(options[1].words, vec![Repr::Tag, Repr::Float]);
}

#[test]
fn a_recursion_that_passes_through_a_reference_is_inline_words_and_one_address() {
    // ADR 0035 makes an *implicitly* recursive value layout a checker error,
    // so nothing here breaks a cycle any more. What is left is the shape the
    // ADR says a recursive declaration must be written in: the cycle passes
    // through a family whose values are a reference, and the layout is
    // finite because that family is one word.
    assert_eq!(
        words(
            "struct Node { value: Int, peers: Vector<Node> }\n\
             fn f(n: Node) -> Int { n.value }",
            "m.Node"
        ),
        vec![Repr::Int, Repr::Ref]
    );
}

#[test]
fn the_only_boxed_layout_is_the_one_erasure_uses() {
    // The point of ADR 0035, as a table: `Shape::Boxed` has exactly one
    // meaning — a value whose type was *intentionally* erased. Erasure and
    // recursion no longer share a mechanism, so a program that erases nothing
    // has one `Boxed` layout and it is the program-wide `Any`.
    let held = layouts(
        "struct Node { value: Int, peers: Vector<Node> }\n\
         fn f(n: Node) -> Int { n.value }",
    );
    let boxes: Vec<&str> = held
        .iter()
        .filter(|it| matches!(it.shape, Shape::Boxed))
        .map(|it| &*it.name)
        .collect();
    assert_eq!(boxes, vec!["Any"]);
}

#[test]
fn a_function_value_is_one_word_whatever_its_signature() {
    // A location holding a function value is one address, and one layout
    // covers every signature for the reason `Array<Int>` and `Array<String>`
    // are one: a reference is a reference. Which environment a word names is
    // a question the object's own header answers.
    let held =
        layouts("fn apply(g: fn(Int) -> Int, h: fn(String) -> Bool, n: Int) -> Int { g(n) }");
    let values: Vec<&Layout> = held.iter().filter(|it| &*it.name == "fn").collect();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].words, vec![Repr::Ref]);
}

#[test]
fn a_closure_environment_is_one_layout_per_lowered_lambda() {
    // The *object* is the other half, and it is an identity rather than a
    // family: payload word 0 is that lambda's own `FunctionId` and the
    // captures after it are the ones that body reads.
    let held = layouts(
        "fn f(n: Int) -> Int {\n  \
           let a = fn() { n + 1 }\n  \
           let b = fn() { 2 }\n  \
           a() + b()\n}",
    );
    let closures: Vec<(&str, &Shape)> = held
        .iter()
        .filter(|it| matches!(it.shape, Shape::Closure { .. }))
        .map(|it| (&*it.name, &it.shape))
        .collect();
    assert_eq!(
        closures.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        vec!["closure m.f#0", "closure m.f#1"]
    );
    // The first captures `n` inline; the second captures nothing, and is the
    // same shape with the list empty rather than a shape of its own.
    let widths: Vec<usize> = closures
        .iter()
        .map(|(_, shape)| match shape {
            Shape::Closure { captures, .. } => captures.len(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(widths, vec![1, 0]);
}

#[test]
fn a_program_declares_the_scalars_whether_or_not_it_names_them() {
    // A one-word value is the width-one case of the model rather than a
    // family of its own, so naming one should not depend on a program
    // having mentioned it.
    let held = layouts("fn f() {}");
    let names: Vec<&str> = held.iter().map(|it| &*it.name).collect();
    assert_eq!(
        names,
        vec![
            "<free>",
            "String",
            "Unit",
            "Bool",
            "Int",
            "Float",
            "Duration",
            "<ref>",
            "<addr>",
            "<host>",
            "Any",
            "Task",
            "TaskScope",
            "<tag>",
            "Bytes",
            "ByteBuffer",
            // `std.int.parse` answers a `Result<Int, Error>`, and `lower`
            // lowers the whole standard library attached to every package, so
            // its two layouts are declared here: the `Error`, and then the
            // `Result` around it. Issue #454's Step 4 put them at the head of
            // this run — before Step 4 the first library signature to name
            // either was `std.string.sliceBytes`, four rows further down, and
            // the pair sat there instead.
            "Error",
            "Result",
            // `std.string.join` takes an `Array<String>` and `std.string.chars`
            // answers one, and they are the only non-generic library functions
            // whose *signature* names a collection at all — `std.array`'s are
            // generic and are interned only where a program instantiates one.
            // The run of one-word references is a family, so `Array<String>`
            // and `Array<Int>` are one layout and this row is every array a
            // program will ever have. It sat below `Result` until issue #454's
            // Step 3 added `chars`, and moved up because that signature is
            // interned before the bodies that name the three below it.
            "Array",
            // `std.string.codePointAtByte` answers an `Option<Int>` and is the
            // first non-generic library function that does, so whole-package
            // lowering declares it before anything else can.
            "Option",
            // `std.string.sliceBytes`' `Result<String, Error>`, which is a
            // **second** `Result` row and not the one above: a `Result`'s
            // layout is the words it holds, and an `Ok` that is an `Int` is a
            // different run from an `Ok` that is a reference. The `Error` it
            // carries is the row already interned above it, which is the whole
            // of what `a_parser_answers_a_result_and_interns_the_error_it_may_carry`
            // is about — the `Result` describes its `Err` words without saying
            // what declared them.
            "Result",
            // The one *declaration* in this list, and it is here because
            // `lower` lowers a whole package: `std.stringbuilder` is attached
            // to every package, and a method of it names its own receiver, so
            // the wrapper's layout is interned however little the program under
            // test has to do with it.
            "std.stringbuilder.StringBuilder",
            // Two rows and one type: `std.string.chars` builds its answer in a
            // `Vector<String>`, and a vector is a header and the store beneath
            // it, which are two layouts for the reason
            // `a_family_that_lives_in_the_heap_is_one_reference` gives —
            // growth replaces the store and the only thing that says what a new
            // one looks like is this table. Issue #454's Step 3 put them here,
            // and they are the first `Vector` rows a program that names no
            // vector has ever had: `std.map`, `std.set` and `std.vector` all
            // deal in a `Vector<T>` whose `T` is a parameter, and a generic
            // signature interns nothing until a program instantiates it.
            "Vector",
            "Vector",
        ]
    );
}

/// [ADR 0051](../../../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
/// `Program::bytes_layout` names the `Shape::Bytes` layout seeded above, and
/// that shape's payload holds no references — the fact that lets a
/// half-filled run be collected as safely as a finished `String`.
#[test]
fn the_program_names_a_bytes_layout_whose_shape_holds_no_references() {
    let (sources, checked) = checked("fn f() {}");
    let program = lower(&checked, &sources, &HostSchemas::new()).expect("the program lowers");
    let described = program.layout(program.bytes_layout);
    assert_eq!(described.shape, Shape::Bytes);
    assert!(!described.may_hold_refs(&program.layouts));
}

/// `docs/LINEAR_VM.md`'s table: a `Set<T>` is `Members { elem }`, one layout
/// per element layout, and a `Map<K, V>` is `Entries { key, value }`, one per
/// *pair*.
///
/// The pair is what a map needs and an element layout would not give: a
/// `Map<String, Int>` traces half its words and a `Map<Int, Int>` none of
/// them, and the collector is told which by the layout rather than by
/// looking. Both are one `Repr::Ref` word where a value of them sits, because
/// both live in the heap.
#[test]
fn a_set_is_one_layout_per_element_and_a_map_one_per_pair() {
    let held = layouts(
        "fn f(a: Set<Int>, b: Set<String>, c: Set<Int>, d: Map<String, Int>, e: Map<Int, Int>) \
         -> Int { a.length() + d.length() }",
    );
    let sets: Vec<&Layout> = held.iter().filter(|it| &*it.name == "Set").collect();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].words, vec![Repr::Ref]);
    let maps: Vec<&Layout> = held.iter().filter(|it| &*it.name == "Map").collect();
    assert_eq!(maps.len(), 2);
    assert!(matches!(sets[0].shape, Shape::Members { .. }));
    assert!(matches!(maps[0].shape, Shape::Entries { .. }));
    let Shape::Entries { key, value } = maps[0].shape else {
        unreachable!("a `Map` is a run of entries")
    };
    assert_eq!(held[key.index()].words, vec![Repr::Ref]);
    assert_eq!(held[value.index()].words, vec![Repr::Int]);
}

/// A `MapEntry` is an ordinary inline struct, and its two words are the
/// key's then the value's — which is exactly one entry of the `Entries` run a
/// `Map` is.
///
/// That correspondence is load-bearing rather than a coincidence: it is what
/// lets a `for` over a `Map` bind a pair with one `load-elem` at this
/// layout's width, and what lets `Map.of` hand the machine the words a
/// literal already wrote.
#[test]
fn a_map_entry_is_the_key_s_words_then_the_value_s() {
    assert_eq!(
        words(
            "fn f() -> Map<String, Int> { Map.of(MapEntry(key: \"a\", value: 1)) }",
            "MapEntry"
        ),
        vec![Repr::Ref, Repr::Int]
    );
}

/// A declaration's fields are read in the vocabulary of the module that
/// *declares* it, not of the module that mentions it.
///
/// The checker records a struct's field types once, at the declaration, and
/// a module's own name is written bare there and qualified everywhere else.
/// So `Tile`'s `kind: Kind` says `Kind` — a name `app` never imported and
/// cannot resolve — and resolving it where `Tile` was mentioned would give a
/// type the package plainly declares no layout at all. It is resolved where
/// it was written; a type argument is qualified before it becomes part of an
/// instantiation's name, so the two halves of a `Cell<app.Thing>` are still
/// both readable from `shape`.
#[test]
fn a_field_s_type_is_read_where_the_declaration_wrote_it() {
    assert_eq!(
        super::listing_in(
            &[
                (
                    "shape",
                    "export enum Kind { Round, Square }\n\
                     export struct Tile { kind: Kind, size: Int }\n"
                ),
                ("app", "use shape.Tile\nfn f(t: Tile) -> Int { t.size }\n"),
            ],
            "app",
            "f",
        ),
        "\
fn @app.f(shape.Tile) -> Int
  frame 3: s0!:tag s1!:int s2:int
  local t -> s0..s1:shape.Tile [0, 2)
     0  copy s2:Int s1:Int
     1  return s2:Int
"
    );
}
