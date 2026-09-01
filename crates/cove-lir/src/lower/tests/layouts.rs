//! The layout table a program lowers to.
//!
//! A layout describes a *family*, not an instantiation, and it is interned —
//! so one shape is one [`LayoutId`](crate::LayoutId) however many times the
//! source writes it. These cases pin what a family is, which is the question
//! every width, every reference map and every field offset is answered from.

use super::checked;
use crate::layout::{Layout, Shape};
use crate::lower::lower;
use crate::repr::Repr;

/// Every layout of a lowered program, by name and words.
fn layouts(source: &str) -> Vec<Layout> {
    lower(&checked(source))
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
    let held = layouts("fn f(v: Vector<Int>) -> Int { v.length() }");
    let vectors: Vec<&Layout> = held.iter().filter(|it| &*it.name == "Vector").collect();
    assert_eq!(vectors.len(), 2);
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
        vec![Repr::Int, Repr::Int, Repr::Ref, Repr::Float]
    );
}

#[test]
fn two_cases_of_one_shape_share_their_payload_words() {
    assert_eq!(
        words(
            "enum E { Left(Int), Right(Int) }\nfn f() -> E { E.Left(1) }",
            "m.E"
        ),
        vec![Repr::Int, Repr::Int]
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
    assert_eq!(options[0].words, vec![Repr::Int, Repr::Int]);
    assert_eq!(options[1].words, vec![Repr::Int, Repr::Float]);
}

#[test]
fn a_layout_that_would_contain_itself_is_broken_with_a_box() {
    // `struct Node { value: Int, next: Option<Node> }` has no finite inline
    // width. The cycle is broken at the occurrence *inside* the layout, and
    // the decision is recorded where a reader can see it: the table holds a
    // `Shape::Boxed` layout called `box m.Node` and, beside it, the inline
    // words that box holds.
    let held = layouts(
        "struct Node { value: Int, next: Option<Node> }\n\
         fn f(n: Node) -> Int { n.value }",
    );
    let boxed = held
        .iter()
        .find(|it| &*it.name == "box m.Node")
        .expect("the cycle was broken with a box");
    assert!(matches!(boxed.shape, Shape::Boxed));
    // One `Ref` word wherever a `Node` is mentioned, which is what
    // `docs/LINEAR_VM.md`'s table says a recursive layout is.
    assert_eq!(boxed.words, vec![Repr::Ref]);

    let inline = held
        .iter()
        .find(|it| &*it.name == "m.Node")
        .expect("the box holds the struct's own inline layout");
    assert_eq!(
        inline.words,
        // `value`, then the `Option`'s discriminant and its one `Ref`.
        vec![Repr::Int, Repr::Int, Repr::Ref]
    );
}

#[test]
fn nothing_about_a_point_changes_because_a_node_exists() {
    // Boxing is a *layout* decision about one declaration, not a
    // representation for structs.
    assert_eq!(
        words(
            "struct Node { value: Int, next: Option<Node> }\n\
             struct Point { x: Int, y: Int }\n\
             fn f(n: Node, p: Point) -> Int { n.value + p.x }",
            "m.Point"
        ),
        vec![Repr::Int, Repr::Int]
    );
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
            "<free>", "String", "Unit", "Bool", "Int", "Float", "Duration", "<ref>", "<addr>",
            "<host>",
        ]
    );
}
