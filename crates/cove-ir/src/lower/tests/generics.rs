//! Generics, which are lowered by monomorphisation.
//!
//! `docs/LINEAR_VM.md` says why there was no choice about it: a slot's `Repr`
//! is fixed for the whole function, that is what makes one static reference
//! map correct at every program counter, and a generic value's width is a
//! fact about the type argument rather than about the declaration. So one
//! instantiation is one function and one layout, and the cases here pin that
//! the two instantiations really are two — different frames, different
//! widths — rather than one function that happens to be called twice.

use cove_schema::HostSchemas;

use super::{checked, listing, refused};
use crate::lower::lower;

/// The name of every function a program lowered to, in order.
fn functions(source: &str) -> Vec<String> {
    let (sources, held) = checked(source);
    lower(&held, &sources, &HostSchemas::new())
        .expect("the program lowers")
        .functions
        .iter()
        .map(|f| f.qualified())
        .collect()
}

/// The name and words of every layout of a declaration, by its short name.
fn instances(source: &str, declaration: &str) -> Vec<(String, usize)> {
    let (sources, held) = checked(source);
    lower(&held, &sources, &HostSchemas::new())
        .expect("the program lowers")
        .layouts
        .iter()
        .filter(|layout| layout.name.starts_with(declaration))
        .map(|layout| (layout.name.to_string(), layout.words.len()))
        .collect()
}

// ---- one function per instantiation --------------------------------------

const IDENTITY: &str = "fn id<T>(x: T) -> T { x }\n\
                        struct Point { x: Int, y: Int }\n\
                        fn f() -> Int {\n  \
                          let a = id(1)\n  \
                          let p = id(Point(x: 2, y: 3))\n  \
                          a + p.x\n\
                        }";

/// The whole of the argument, in one listing: `id` at two type arguments is
/// two functions, and their frames are two widths.
///
/// `id<Int>` takes one word and `id<m.Point>` takes two, and there is nothing
/// either of them could do to be the other: the parameter's slot count, the
/// answer's, and the width of the `copy` between them are all read off the
/// layout, and the layout is what the type argument decides. A single lowered
/// `id` would have to have picked one of these two frames.
#[test]
fn two_instantiations_are_two_functions_with_two_frames() {
    assert_eq!(
        listing(IDENTITY, "id<Int>"),
        "\
fn2 m.id<Int>(Int) -> Int
  frame 2: s0!:int s1:int
  local x -> s0:Int [0, 2)
     0  copy s1:int s0:int Int
     1  return s1:int Int
"
    );
    assert_eq!(
        listing(IDENTITY, "id<m.Point>"),
        "\
fn3 m.id<m.Point>(m.Point) -> m.Point
  frame 4: s0!:int s1!:int s2:int s3:int
  local x -> s0:m.Point [0, 2)
     0  copy s2:int s0:int m.Point
     1  return s2:int m.Point
"
    );
}

/// And the call sites name the two apart, at an ordinary `Call` each.
///
/// There is no dispatch here and nothing carried alongside the argument: the
/// callee is a `FunctionId` the lowering settled, exactly as it is for a call
/// to a declaration that binds no type parameters.
#[test]
fn a_call_names_the_instantiation_it_reaches() {
    assert_eq!(
        listing(IDENTITY, "f"),
        "\
fn0 m.f() -> Int
  frame 8: s0:int s1:int s2:int s3:int s4:int s5:int s6:int s7:int
  local a -> s2:Int [2, 9)
  local p -> s6:m.Point [7, 9)
     0  int s1:int 1
     1  call s2:int m.id<Int> (s1:Int) Int
     2  int s1:int 2
     3  int s3:int 3
     4  copy s4:int s1:int Int
     5  copy s5:int s3:int Int
     6  call s6:int m.id<m.Point> (s4:m.Point) m.Point
     7  add.int s1:int s2:int s6:int
     8  copy s0:int s1:int Int
     9  return s0:int Int
"
    );
}

/// A name a diagnostic and `Program::function_named` can live with, and one
/// a declaration cannot collide with.
///
/// `<` is not a name character, so no `fn` can be called `id<Int>`; and the
/// type arguments are written the way the *package* names them rather than
/// the way the call site's module does, so two modules passing one type ask
/// for one instantiation.
#[test]
fn an_instantiation_is_named_after_its_arguments() {
    let held = functions(IDENTITY);
    assert!(held.contains(&"m.id<Int>".to_string()), "{held:?}");
    assert!(held.contains(&"m.id<m.Point>".to_string()), "{held:?}");
}

/// Twice at one type is one function, and so is a body that calls itself.
///
/// The number is recorded before the body is lowered, which is what makes the
/// recursive case terminate rather than start again — and is the same thing
/// that makes two call sites share one function.
#[test]
fn a_generic_instantiated_once_costs_one_function() {
    let held = functions(
        "fn id<T>(x: T) -> T { x }\n\
         fn f() -> Int { id(1) + id(2) }",
    );
    assert_eq!(
        held.iter().filter(|name| name.contains('<')).count(),
        1,
        "{held:?}"
    );

    let held = functions(
        "fn count<T>(x: T, n: Int) -> Int { if n > 0 { count(x, n - 1) } else { 0 } }\n\
         fn f() -> Int { count(1, 3) }",
    );
    assert_eq!(
        held.iter().filter(|name| name.contains('<')).count(),
        1,
        "a recursive generic at one type is one function: {held:?}"
    );
}

/// The type arguments a call *writes* need no path of their own.
///
/// The checker resolved `id<Int>(1)` and settled every type at the call site
/// with the argument already applied, so `f<Int>(x)` and `f(x)` reach the
/// same reading. This used to be a gap.
#[test]
fn an_explicit_type_argument_reaches_the_same_instantiation() {
    assert_eq!(
        listing(
            "fn id<T>(x: T) -> T { x }\nfn f() -> Int { id<Int>(1) }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 1
     1  call s2:int m.id<Int> (s1:Int) Int
     2  copy s0:int s2:int Int
     3  return s0:int Int
"
    );
}

// ---- one layout per instantiation ----------------------------------------

/// A generic `struct` at two instantiations is two layouts, and a layout's
/// name is an identity: `m.Cell<Int>` is one word and `m.Cell<m.Point>` is
/// two, and nothing about them is shared but the declaration they came from.
#[test]
fn a_generic_struct_at_two_instantiations_is_two_layouts() {
    assert_eq!(
        instances(
            "struct Cell<T> { it: T }\n\
             struct Point { x: Int, y: Int }\n\
             fn f() -> Int {\n  \
               let a = Cell(it: 1)\n  \
               let b = Cell(it: Point(x: 2, y: 3))\n  \
               a.it + b.it.x\n\
             }",
            "m.Cell",
        ),
        vec![
            ("m.Cell<Int>".to_string(), 1),
            ("m.Cell<m.Point>".to_string(), 2),
        ]
    );
}

/// And a generic `enum` is two payload regions, because the region is wide
/// enough for the widest case and the case is the type argument.
#[test]
fn a_generic_enum_at_two_instantiations_is_two_layouts() {
    assert_eq!(
        instances(
            "enum Held<T> { Empty, Full(T) }\n\
             struct Point { x: Int, y: Int }\n\
             fn f() -> Int {\n  \
               let a = Held.Full(1)\n  \
               let b = Held.Full(Point(x: 2, y: 3))\n  \
               match a { Held.Empty => 0, Held.Full(n) => n }\n\
             }",
            "m.Held",
        ),
        // A discriminant word and then the payload region: one word for an
        // `Int` and two for a `Point`.
        vec![
            ("m.Held<Int>".to_string(), 2),
            ("m.Held<m.Point>".to_string(), 3),
        ]
    );
}

// ---- a bound -------------------------------------------------------------

const SUMMARY: &str = r#"trait Summary { fn summary(self) -> String }
struct Article { title: String, words: Int }
impl Summary for Article { fn summary(self) -> String { self.title } }
struct Note { body: String }
impl Summary for Note { fn summary(self) -> String { self.body } }
fn headline<T: Summary>(entry: T) -> String { entry.summary() }
fn f(a: Article, n: Note) -> String { "{headline(a)}{headline(n)}" }
"#;

/// A bounded type parameter dispatches **statically**, to the one
/// conformance the instantiation names.
///
/// The checker recorded no target for `entry.summary()` — which
/// implementation it reaches is decided by the argument rather than by the
/// source — and monomorphisation is what turns that into an answer: this body
/// is lowered for one type, ADR 0006 makes conformance explicit, so there is
/// exactly one implementation and the call names it. No dictionary is passed,
/// no vtable is read and no `Switch` is emitted; a bound costs nothing at run
/// time and one function per type it is used at.
#[test]
fn a_bounded_parameter_dispatches_to_its_conformance() {
    assert_eq!(
        listing(SUMMARY, "headline<m.Article>"),
        "\
fn4 m.headline<m.Article>(m.Article) -> String
  frame 4: s0!:ref s1!:int s2:ref s3:ref
  local entry -> s0:m.Article [0, 4)
     0  call s3:ref m.Article.summary (s0:m.Article) String
     1  copy s2:ref s3:ref String
     2  clear s3:ref String
     3  return s2:ref String
"
    );
    assert_eq!(
        listing(SUMMARY, "headline<m.Note>"),
        "\
fn5 m.headline<m.Note>(m.Note) -> String
  frame 3: s0!:ref s1:ref s2:ref
  local entry -> s0:m.Note [0, 4)
     0  call s2:ref m.Note.summary (s0:m.Note) String
     1  copy s1:ref s2:ref String
     2  clear s2:ref String
     3  return s1:ref String
"
    );
}

/// The receiver's own frame is the difference the two arms cannot share: an
/// `Article` is two words and a `Note` is one, so the parameter that holds
/// one is not the parameter that holds the other. That is the same fact the
/// widths above show, seen from the conformance's side.
#[test]
fn each_conformance_receives_its_own_width() {
    let (sources, held) = checked(SUMMARY);
    let program = lower(&held, &sources, &HostSchemas::new()).expect("the program lowers");
    let width = |name: &str| {
        let id = program
            .function_named("m", name)
            .unwrap_or_else(|| panic!("`{name}` was lowered"));
        program.function(id).param_words(&program.layouts)
    };
    assert_eq!(width("Article.summary"), 2);
    assert_eq!(width("Note.summary"), 1);
}

// ---- what a program is refused for ---------------------------------------

/// The one thing monomorphisation cannot answer, and the one refusal this
/// crate makes.
///
/// `f(Cell(x))` asks for `f<Cell<Int>>` from inside `f<Int>`, and every step
/// is a wider type than the one before it, so there is no finite set of
/// functions to lower it to. ADR 0035 does not catch it: its analysis is
/// about a *declaration*'s layout containing itself and `Cell<Cell<Int>>` is
/// perfectly finite.
///
/// The diagnostic names the chain step by step rather than reporting a depth,
/// because the count says only that something grew and the chain says what
/// grew it.
#[test]
fn an_unbounded_instantiation_chain_is_refused_and_names_itself() {
    let items = refused(
        "struct Cell<T> { it: T }\n\
         fn f<T>(x: T) -> Int { f(Cell(it: x)) }\n\
         fn g() -> Int { f(1) }",
    );
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        items[0],
        "this call instantiates a generic more than 8 deep, so there is no finite set of \
         functions to lower it to:\n  \
         m.f<Int>\n  \
         m.f<m.Cell<Int>>\n  \
         m.f<m.Cell<m.Cell<Int>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<Int>>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<m.Cell<Int>>>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<Int>>>>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<Int>>>>>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<Int>>>>>>>>\n  \
         m.f<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<m.Cell<Int>>>>>>>>>"
    );
}

/// It is not a gap, and the code says so.
///
/// A gap is a promise that a later task removes it. This one no task removes:
/// monomorphisation is what the value model admits, and this program has no
/// monomorphisation. The language's answer to "one copy of the code" is
/// `dyn Trait`, which the help names.
#[test]
fn the_depth_refusal_is_not_one_of_the_lowerings_gaps() {
    let (sources, held) = checked(
        "struct Cell<T> { it: T }\n\
         fn f<T>(x: T) -> Int { f(Cell(it: x)) }\n\
         fn g() -> Int { f(1) }",
    );
    let items = lower(&held, &sources, &HostSchemas::new()).expect_err("the program is refused");
    assert_eq!(items[0].code, super::super::gap::INSTANTIATION_DEPTH);
}

/// A type parameter in neither the parameters nor the answer is the one thing
/// a call site's recorded facts do not carry.
///
/// The call wrote it — `only<Int>()` — and the checker applied it before this
/// crate saw anything, but nothing it records mentions the parameter
/// afterwards. So it is reported rather than guessed at: what would be needed
/// is a fact `cove-sema` does not keep, and deriving a second answer to a
/// resolution question is exactly what reading the checker's answers is for.
#[test]
fn a_type_argument_no_recorded_fact_carries_is_reported() {
    assert_eq!(
        refused("fn only<T>() -> Int { 1 }\nfn f() -> Int { only<Int>() }"),
        vec![
            "not yet lowered: a call to `m.only` whose type argument `T` appears in neither its \
             parameters nor its answer, so no recorded fact says what it is"
        ]
    );
}

/// A method of a generic type is named as the work rather than left to fail
/// as an unlayoutable receiver.
///
/// The type parameters are the *type*'s rather than the declaration's, so
/// which arguments a call settles is read off the receiver rather than off
/// the signature — one reading more than what is built here, and one nothing
/// in the corpus asks for.
#[test]
fn a_method_of_a_generic_type_is_a_gap_naming_the_work() {
    assert_eq!(
        refused(
            "struct Cell<T> { it: T }\n\
             impl Cell { fn get(self) -> Int { 1 } }\n\
             fn f() -> Int { Cell(it: 1).get() }"
        ),
        vec!["not yet lowered: `Cell.get`, a method of a generic type"]
    );
}
