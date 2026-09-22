//! What [`super::super::synth`] writes, and the two properties it is held to.
//!
//! [ADR 0064](../../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 3 makes a layout-directed operation a function the lowering
//! composes out of the layout, and Decision 4 admits exactly one fallback for
//! the one layout that is not known until a box is opened. The listings below
//! are what the composition produces; the two tests at the end are the
//! properties, and they are the ones worth keeping if the listings ever
//! become tiresome to maintain.

use std::path::{Path, PathBuf};

use cove_schema::HostSchemas;

use super::{checked, listing};
use crate::inst::{CmpOp, Inst};
use crate::layout::Shape;
use crate::lower::{lower, synth};
use crate::Program;

/// The whole lowered program, which is what a structural question is asked
/// of.
fn lowered(source: &str) -> Program {
    let (sources, held) = checked(source);
    lower(&held, &sources, &HostSchemas::new()).expect("the program lowers")
}

/// The listing of the one synthesized function whose name holds `what`.
fn synthesized(program: &Program, what: &str) -> String {
    let found: Vec<usize> = program
        .functions
        .iter()
        .enumerate()
        .filter(|(_, held)| &*held.module == synth::MODULE && held.name.contains(what))
        .map(|(at, _)| at)
        .collect();
    let names: Vec<&str> = program
        .functions
        .iter()
        .filter(|held| &*held.module == synth::MODULE)
        .map(|held| &*held.name)
        .collect();
    let [at] = found[..] else {
        panic!(
            "one synthesized function holds `{what}`, and these are the ones there are: {names:?}"
        );
    };
    crate::print::function(program, crate::FunctionId(at as u32))
}

/// Every synthesized function's name, in the order they were made.
fn walks_of(program: &Program) -> Vec<String> {
    program
        .functions
        .iter()
        .filter(|held| &*held.module == synth::MODULE && !held.stub)
        .map(|held| held.name.to_string())
        .collect()
}

/// A struct with a field of each of the two kinds a walk composes
/// differently: a word the instruction set compares, and a reference to an
/// object it does not.
///
/// The call is gone — `super::super::inline` expanded a leaf of six
/// instructions into its caller — which is the whole claim of Decision 3 in
/// one listing: what was an `intrinsic-call` into a Rust algorithm is now
/// three comparisons the optimizer can see, two of which it has already fused
/// into their branches under ADR 0054.
#[test]
fn a_struct_is_compared_field_by_field_in_declaration_order() {
    assert_eq!(
        listing(
            "struct Row { n: Int, name: String, flag: Bool }\n\
             fn same(a: Row, b: Row) -> Bool { a == b }",
            "same"
        ),
        "\
fn @m.same(m.Row m.Row) -> Bool
  frame 8: s0!:int s1!:ref s2!:bool s3!:int s4!:ref s5!:bool s6:bool s7:bool
  local a -> s0..s2:m.Row [0, 4)
  local b -> s3..s5:m.Row [0, 4)
     0  eq.int.branch s6:bool s0:int s3:int 3
     1  eq.str.branch s6:bool s1:ref s4:ref 3
     2  eq.bool s6:bool s2:bool s5:bool
     3  return s6:Bool
"
    );
}

/// A `Float` field is compared as a **number**, which is the arm a walk over
/// words gets wrong.
///
/// `eq.float` is IEEE-754 equality: a `NaN` is equal to nothing, itself
/// included, and `-0.0` is equal to `0.0`. Two `Float` words compared as
/// words answer the opposite of both. `tests/e2e/values_any_equals` is where
/// that is pinned as an answer a program prints; this is where it is pinned
/// as the instruction that was chosen.
#[test]
fn a_float_field_is_compared_as_a_number_and_not_as_a_word() {
    let listed = listing(
        "struct Reading { at: Int, value: Float }\n\
         fn same(a: Reading, b: Reading) -> Bool { a == b }",
        "same",
    );
    assert!(listed.contains("eq.float"), "{listed}");
    assert!(!listed.contains("Any.equals"), "{listed}");
}

/// An enum is its case and then the payload that case names.
///
/// One `eq.tag` — two values of one layout are two values of one declared
/// enum, so the *word* is the case, where the runtime walk compares case
/// names because its two operands may be two instantiations of one
/// declaration — and then a `switch` into one arm per case.
#[test]
fn an_enum_compares_its_case_and_then_its_payload() {
    let program = lowered(
        "enum Mark { Plain\n  Count(Int)\n  Named(String) }\n\
         fn same(a: Mark, b: Mark) -> Bool { a == b }",
    );
    let listed = synthesized(&program, "equals<m.Mark");
    assert!(listed.contains("eq.tag"), "{listed}");
    assert!(listed.contains("switch"), "{listed}");
    assert!(listed.contains("eq.int"), "{listed}");
    assert!(listed.contains("eq.str"), "{listed}");
    assert!(!listed.contains("Any.equals"), "{listed}");
}

/// A map is a run of entries, each the key's words then the value's, and the
/// walk takes them in the one order the collection keeps.
///
/// Two maps built in two insertion orders are one value, and what makes that
/// true here is that nothing searches: the runs line up entry for entry
/// because both are ascending by key, which is ADR 0059's canonical order
/// being part of the value.
#[test]
fn a_map_walks_its_entries_key_before_value() {
    let program = lowered("fn same(a: Map<String, Int>, b: Map<String, Int>) -> Bool { a == b }");
    let listed = synthesized(&program, "equals<Map#");
    assert!(listed.contains("len "), "{listed}");
    assert!(listed.contains("load-elem"), "{listed}");
    assert!(listed.contains("jump"), "{listed}");
    let entry = synthesized(&program, "equals<MapEntry");
    assert!(entry.contains("eq.str"), "{entry}");
    assert!(entry.contains("eq.int"), "{entry}");
}

/// A vector's length is its own word and not its store's header, which is the
/// capacity.
#[test]
fn a_vector_reads_its_own_length_and_not_its_stores() {
    let program = lowered("fn same(a: Vector<Int>, b: Vector<Int>) -> Bool { a == b }");
    let listed = synthesized(&program, "equals<Vector");
    assert!(listed.contains("load-field"), "{listed}");
    assert!(!listed.contains("len "), "{listed}");
}

/// A layout that reaches itself is synthesized once, and asking for it from
/// inside its own walk finds the number rather than starting again.
///
/// The `Node`'s walk calls the `Array<Node>`'s, which calls the `Node`'s —
/// the cycle the memo closes. Two functions, not two thousand and not a stack
/// overflow.
#[test]
fn a_layout_that_reaches_itself_is_synthesized_once() {
    let program = lowered(
        "struct Node { tag: Int, kids: Array<Node> }\n\
         fn same(a: Node, b: Node) -> Bool { a == b }",
    );
    let walks = walks_of(&program);
    assert_eq!(walks.len(), 2, "{walks:?}");
    assert!(
        walks.iter().any(|name| name.starts_with("equals<m.Node#")),
        "{walks:?}"
    );
    assert!(
        walks.iter().any(|name| name.starts_with("equals<Array#")),
        "{walks:?}"
    );
}

/// The one fallback, and the one place it is reached from.
///
/// A `dyn Trait` keeps its family in its own payload word 0, so there is no
/// layout here to compose a walk out of and the runtime's own walk is what
/// answers. This is the test that the fallback still *exists* — the rule
/// below says nothing may reach it wrongly, and a rule nothing can satisfy is
/// satisfied by deleting the arm.
#[test]
fn an_erased_value_reaches_the_one_dynamic_fallback() {
    let program = lowered(
        "trait Summary { fn summarize(self) -> String }\n\
         struct Booking { id: Int }\n\
         impl Summary for Booking { fn summarize(self) -> String { \"{self.id}\" } }\n\
         fn same(a: dyn Summary, b: dyn Summary) -> Bool { a == b }",
    );
    let sites = fallbacks(&program);
    assert_eq!(sites, 1, "one `Any.equals`, and it is the erased one");
}

/// **ADR 0064's Decision 4 as a fact about every program this crate lowers.**
///
/// > a structural test asserts that no statically-known-layout site reaches
/// > the fallback, and the intended count on an ordinary program is zero.
///
/// The check itself is [`crate::verify`]'s — `check_one_dynamic_boundary` —
/// because that is where *every* program arrives and `lower::finish` panics
/// on a program the verifier refuses. So what is left for a test is the half
/// a verifier rule cannot state: that the rule bites, and that the programs
/// below reach the intrinsic exactly as often as they hold an erased
/// comparison and no oftener.
///
/// What it catches is the cheap way out of the migrations that follow. It
/// already caught one: when `Value.order`'s producer was written, every
/// awkward arm it has — an enum declared out of case-name order, a run whose
/// lengths are compared after its elements, a `Float` field that has to raise
/// — could have been made to work by handing the layout to the intrinsic, and
/// everything would have passed, because the answers would still have been
/// right. `Value.admitKey` and `Value.renderInto` are the two left, and the
/// same is true of each.
#[test]
fn no_statically_known_layout_reaches_the_fallback() {
    let known: &[&str] = &[
        "struct Row { n: Int, name: String, flag: Bool }\n\
             enum Mark { Plain\n  Count(Int)\n  Named(String) }\n\
             fn a(x: Row, y: Row) -> Bool { x == y }\n\
             fn b(x: Mark, y: Mark) -> Bool { x == y }\n\
             fn c(x: Array<Row>, y: Array<Row>) -> Bool { x == y }\n\
             fn d(x: Vector<Mark>, y: Vector<Mark>) -> Bool { x == y }\n\
             fn e(x: Set<String>, y: Set<String>) -> Bool { x == y }\n\
             fn f(x: Map<String, Row>, y: Map<String, Row>) -> Bool { x == y }\n\
             fn g(x: Option<Row>, y: Option<Row>) -> Bool { x == y }\n\
             fn h(x: Result<Row, String>, y: Result<Row, String>) -> Bool { x == y }\n\
             fn i(x: Range, y: Range) -> Bool { x == y }",
        // A recursion, which is the one that has to reach itself rather than
        // give up and hand the cycle to the runtime.
        "struct Node { tag: Int, kids: Array<Node> }\n\
         fn a(x: Node, y: Node) -> Bool { x == y }",
    ];
    for source in known {
        let program = lowered(source);
        assert_eq!(fallbacks(&program), 0, "`Any.equals` sites in:\n{source}");
    }

    // And the other direction, because a rule nothing can satisfy is
    // satisfied by deleting the arm: an erased field *inside* a known layout
    // is walked down to, and the one field that is a box is the one thing
    // handed over.
    //
    // The count is not asserted and the reason is `super::super::inline`: the
    // walk is six instructions, so it is expanded into its caller *and* left
    // standing, because a whole-package lowering is every declaration's root
    // and `super::super::sweep` has nothing to stand down. What matters here
    // is that the fallback was reached at all, and `fallbacks` has already
    // checked every operand of every one of them.
    let program = lowered(
        "trait Summary { fn summarize(self) -> String }\n\
         struct Booking { id: Int }\n\
         impl Summary for Booking { fn summarize(self) -> String { \"{self.id}\" } }\n\
         struct Held { tag: Int, item: dyn Summary }\n\
         fn a(x: Held, y: Held) -> Bool { x == y }",
    );
    assert!(fallbacks(&program) > 0);
}

// ---- `core.order(a, b)` ------------------------------------------------

/// **The short circuit still fires, and this is the load-bearing test of the
/// order's half of this module.**
///
/// [`super::super::synth::ordered_by`] answers a comparison instruction for
/// every key a *single* instruction orders, and `core.order` emits it there
/// rather than calling anything. That is why `cq` — twenty thousand records
/// looked up by `String` key — executes `Value.order` at no site and on no
/// turn, and it is a property a synthesis is in a position to destroy by
/// making a function for every key and letting the inliner sort it out. What
/// would be left is a program whose counts moved for no reason anybody asked
/// for.
///
/// So: not one synthesized function, not one intrinsic call, and at least one
/// three-way comparison, for each of the five families the instruction set
/// orders on its own.
#[test]
fn a_key_one_instruction_orders_is_not_a_function() {
    let keys: &[(&str, &str)] = &[
        ("", "Int"),
        ("", "String"),
        ("", "Bool"),
        ("", "Duration"),
        // A payload-free enum whose cases were *declared* in ascending name
        // order, where the discriminant is the rank.
        ("enum Flag { Off\n  On }", "Flag"),
    ];
    for (declarations, key) in keys {
        let program = keyed(declarations, key);
        assert_eq!(
            orders_of(&program),
            Vec::<String>::new(),
            "a walk for {key}"
        );
        assert_eq!(orderings(&program), 0, "a `Value.order` site for {key}");
        assert!(
            count(&program, |inst| matches!(
                inst,
                Inst::Cmp {
                    op: CmpOp::Order,
                    ..
                }
            )) > 0,
            "no three-way comparison for {key}"
        );
    }
}

/// A struct key is ordered field by field in declaration order.
///
/// `key::order` compares the two type names first and then, field by field,
/// the field *names* before the field values. Two values of one layout are
/// two values of one declaration, so every one of those name comparisons is
/// equal and nothing is left but the fields — which is the same thing a
/// statically known layout buys at an enum's discriminant, in the one place
/// it costs nothing to spend.
///
/// The early exit is a `cmp-imm` against `0` and the branch beside it, which
/// is the pair `super::super::peephole` fuses; the last field needs no branch
/// at all, because its answer is the answer of the whole.
#[test]
fn a_struct_key_is_ordered_field_by_field_in_declaration_order() {
    let program = keyed("struct Row { n: Int, name: String, flag: Bool }", "Row");
    let listed = synthesized(&program, "order<");
    assert!(listed.contains("order.int"), "{listed}");
    assert!(listed.contains("order.str"), "{listed}");
    assert!(listed.contains("order.bool"), "{listed}");
    assert!(!listed.contains("Value.order"), "{listed}");
    // Three fields, two early exits.
    assert_eq!(listed.matches("eq.int.imm").count(), 2, "{listed}");
}

/// **An enum orders by its case NAME, and not by its case index.**
///
/// `Mark` is declared `Plain`, `Count`, `Named` and orders `Count`, `Named`,
/// `Plain`, because that is alphabetical. A walk that compared the
/// discriminants would answer the declaration order, which is a different
/// total order and a wrong one — and nothing but this would notice, because
/// it is still a total order and every law a test might check of it holds.
///
/// So the discriminant is turned into a rank: one `switch` per operand into
/// one `int` per case, and a three-way comparison of the two ranks. The
/// permutation is compile-time, and this is it written as the only
/// structural read the instruction set has.
#[test]
fn an_enum_orders_by_case_name_and_not_by_case_index() {
    assert_eq!(
        synthesized(
            &keyed("enum Mark { Plain\n  Count(Int)\n  Named(String) }", "Mark"),
            "order<"
        ),
        "\
fn @<synth>.order<m.Mark#16>(m.Mark m.Mark) -> Int
  frame 10: s0!:tag s1!:int s2!:ref s3!:tag s4!:int s5!:ref s6:int s7:int s8:int s9:bool
     0  switch s0:tag [1 3 5] else 7
     1  int s7:int 2
     2  jump 8
     3  int s7:int 0
     4  jump 8
     5  int s7:int 1
     6  jump 8
     7  trap \"this `m.Mark` is in a case it does not have\"
     8  switch s3:tag [9 11 13] else 15
     9  int s8:int 2
    10  jump 16
    11  int s8:int 0
    12  jump 16
    13  int s8:int 1
    14  jump 16
    15  trap \"this `m.Mark` is in a case it does not have\"
    16  order.int s6:int s7:int s8:int
    17  eq.int.imm.branch s9:bool s6:int 0 25
    18  switch s0:tag [19 20 22] else 24
    19  jump 25
    20  order.int s6:int s1:int s4:int
    21  jump 25
    22  order.str s6:int s2:ref s5:ref
    23  jump 25
    24  trap \"this `m.Mark` is in a case it does not have\"
    25  return s6:Int
"
    );
}

/// And where the declaration *is* in name order, the discriminant is the
/// rank and one instruction is the whole comparison of the case.
///
/// `Option` is this: `None` then `Some`. [`super::super::synth::ordered_by`]
/// does not answer for it, because it has a payload and the payload still has
/// to be walked — but the case itself is one `order.tag` and there is no
/// permutation to build.
#[test]
fn an_enum_declared_in_name_order_orders_by_its_discriminant() {
    let program = keyed("", "Option<Int>");
    let listed = synthesized(&program, "order<");
    assert!(listed.contains("order.tag"), "{listed}");
    // The payload switch, and no rank switch.
    assert_eq!(listed.matches("switch").count(), 1, "{listed}");
}

/// **A run compares its lengths after its elements, where equality compares
/// them first.**
///
/// `[1]` sorts before `[1, 0]` and both sort before `[2]`, so a length that
/// decided first would put `[1, 0]` after `[2]`. The loop therefore runs over
/// the positions *both* runs have — two comparisons and two branches rather
/// than a computed minimum — and the two lengths are the last thing compared.
#[test]
fn a_run_compares_its_lengths_after_its_elements() {
    let program = keyed("", "Array<Int>");
    let listed = synthesized(&program, "order<");
    let lines: Vec<&str> = listed.lines().collect();
    let returned = lines
        .iter()
        .position(|line| line.contains("return"))
        .expect("the walk returns");
    assert!(
        lines[returned - 1].contains("order.int"),
        "the last thing before the return is the lengths:\n{listed}"
    );
    assert_eq!(listed.matches("len ").count(), 2, "{listed}");
    assert_eq!(listed.matches("lt.int").count(), 2, "{listed}");
}

/// A map compares entry for entry, and the key of an entry before its value.
///
/// One `load-elem` at the `MapEntry` layout's width reads both halves, and
/// the value's offset inside it is the key's width — which is
/// `Shapes::entry_of`, the same layout equality's walk reads a map with.
#[test]
fn a_map_key_compares_its_key_before_its_value() {
    let program = keyed("", "Map<String, Int>");
    let listed = synthesized(&program, "order<");
    let at = |what: &str| {
        listed
            .find(what)
            .unwrap_or_else(|| panic!("{what}\n{listed}"))
    };
    assert!(at("order.str") < at("order.int"), "{listed}");
    assert_eq!(listed.matches("load-elem").count(), 2, "{listed}");
}

/// A value that is not a key raises, in the runtime walk's own sentence.
///
/// This is the first arm in this module that *raises*, and the reason it may
/// is that the sentence is a constant: `Inst::Trap`'s string is chosen by the
/// lowering that emits it, exactly as an uncovered `match`'s is, and nothing
/// in it quotes a value computed at run time. Issue #461 is about the other
/// kind and this is not it.
///
/// Unreachable from a checked program — `core.admitKey` refuses such a key
/// before a single comparison is made — and written out for the reason the
/// runtime's own arm is written out.
#[test]
fn a_value_that_is_not_a_key_raises_in_the_runtime_s_words() {
    let program = keyed("struct Reading { at: Int, value: Float }", "Reading");
    let listed = synthesized(&program, "order<");
    assert!(listed.contains("trap"), "{listed}");
    assert!(
        program
            .strings
            .iter()
            .any(|held| &**held == "this value cannot be a map key or a set element"),
        "the refusal is the runtime's, word for word"
    );
}

/// ADR 0064's Decision 4 for the order, which is the same test as
/// [`no_statically_known_layout_reaches_the_fallback`] over the same rule.
#[test]
fn no_statically_known_layout_reaches_the_order_fallback() {
    let known: &[(&str, &str)] = &[
        ("struct Row { n: Int, name: String, flag: Bool }", "Row"),
        ("enum Mark { Plain\n  Count(Int)\n  Named(String) }", "Mark"),
        (
            "struct Row { n: Int, name: String, flag: Bool }",
            "Array<Row>",
        ),
        ("", "Set<String>"),
        ("", "Map<String, Int>"),
        ("", "Option<Int>"),
        ("", "Result<Int, String>"),
        ("", "Range"),
        ("", "Unit"),
        // A recursion, which has to reach itself rather than give up and hand
        // the cycle to the runtime.
        ("struct Node { tag: Int, kids: Array<Node> }", "Node"),
    ];
    for (declarations, key) in known {
        let program = keyed(declarations, key);
        assert_eq!(orderings(&program), 0, "`Value.order` sites for {key}");
    }

    // And the other direction: an erased field *inside* a known layout is
    // walked down to, and the one field that is a box is the one thing handed
    // over.
    let program = keyed(
        "trait Summary { fn summarize(self) -> String }\n\
         struct Booking { id: Int }\n\
         impl Summary for Booking { fn summarize(self) -> String { \"{self.id}\" } }\n\
         struct Held { tag: Int, item: dyn Summary }",
        "Held",
    );
    assert!(orderings(&program) > 0);
}

/// Every synthesized *order*'s name, in the order they were made.
fn orders_of(program: &Program) -> Vec<String> {
    walks_of(program)
        .into_iter()
        .filter(|name| name.starts_with("order<"))
        .collect()
}

// ---- `core.admitKey(key, method, role)` --------------------------------

/// **The short circuit still fires, and this is the load-bearing test of the
/// admission's half of this module.**
///
/// [`super::super::synth::admission`] answers [`synth::Admission::Always`] for
/// every layout no value of which is ever refused, and `core.admitKey` emits
/// **nothing at all** there — no walk, no call, no intrinsic. That is why
/// `covefmt` and `cq` reach `Value.admitKey` at no site and on no turn: every
/// key either program uses is a `String` or an `Int`. It is also the property
/// a synthesis is in a position to destroy by making a function for every key
/// and letting the inliner sort it out.
///
/// The families here are ADR 0001's whole admitted list, and a composite of
/// each: a scalar, a string, a range, a struct, an enum, an array, a set, a
/// map, and the two the standard library declares.
#[test]
fn a_key_no_value_of_which_is_refused_is_not_asked_about() {
    let keys: &[(&str, &str)] = &[
        ("", "Int"),
        ("", "String"),
        ("", "Bool"),
        ("", "Duration"),
        ("", "Unit"),
        ("", "Range"),
        ("", "Array<Int>"),
        ("", "Set<String>"),
        ("", "Map<String, Int>"),
        ("", "Option<Int>"),
        ("", "Result<Int, String>"),
        ("struct Row { n: Int, name: String, flag: Bool }", "Row"),
        ("enum Flag { On\n  Off }", "Flag"),
        (
            "struct Row { n: Int, name: String, flag: Bool }",
            "Array<Row>",
        ),
        (
            "struct Row { n: Int, name: String, flag: Bool }",
            "Map<String, Row>",
        ),
    ];
    for (declarations, key) in keys {
        let program = keyed(declarations, key);
        assert_eq!(admissions(&program), 0, "a `Value.admitKey` site for {key}");
        assert_eq!(
            walks_named(&program, "refuses<"),
            Vec::<String>::new(),
            "a walk for {key}"
        );
    }
}

/// An enum whose payload one case refuses is read at its discriminant.
///
/// This is the family the admission exists for: `Mark.Count(3)` is a key and
/// `Mark.Weight(1.5)` is not, and nothing about the *type* decides it. So the
/// walk is one `switch`, two arms that do nothing at all, and one that
/// answers `true`.
///
/// It answers a `Bool` and never raises: the sentence a refusal is carries a
/// `rule:` and a `help:` beside it and `Inst::Trap` carries one string, so
/// what the walk hands back is the bit and `core.admitKey` runs the intrinsic
/// under a `branch-false`.
#[test]
fn an_enum_is_read_at_its_discriminant_and_answers_a_bool() {
    let program = keyed("enum Mark { Plain\n  Count(Int)\n  Weight(Float) }", "Mark");
    let listed = synthesized(&program, "refuses<");
    assert!(listed.contains("-> Bool"), "{listed}");
    assert!(listed.contains("switch"), "{listed}");
    assert!(!listed.contains("trap"), "{listed}");
    assert!(!listed.contains("Value.admitKey"), "{listed}");
    // `true` from the refused case and from the discriminant no case names,
    // and the one `false` the good path falls through with — and nothing
    // unreachable between them.
    assert_eq!(listed.matches("  bool ").count(), 3, "{listed}");
    assert_eq!(listed.matches("  jump ").count(), 4, "{listed}");
}

/// A struct is its fields, and the ones already known to be keys cost
/// nothing.
///
/// `Holder`'s `id` and `label` are an `Int` and a `String`, so the walk has
/// no instruction for either of them: what it holds is the `switch` on the
/// middle field, reached by a static word offset.
#[test]
fn a_struct_walks_only_the_field_that_decides() {
    let program = keyed(
        "enum Mark { Plain\n  Count(Int)\n  Weight(Float) }\n\
         struct Holder { id: Int, mark: Mark, label: String }",
        "Holder",
    );
    let listed = synthesized(&program, "refuses<m.Holder");
    assert_eq!(listed.matches("switch").count(), 1, "{listed}");
    assert!(!listed.contains("load-field"), "{listed}");
}

/// A run is a loop over its elements, and an empty one is a key.
///
/// Which is the whole of why `Array<Float>` cannot be answered from the type:
/// `[]` is a key and `[1.5]` is not.
#[test]
fn a_run_is_a_loop_and_an_empty_one_falls_through() {
    let program = keyed("", "Array<Float>");
    let listed = synthesized(&program, "refuses<Array");
    assert!(listed.contains("len"), "{listed}");
    assert!(listed.contains("load-elem"), "{listed}");
}

/// A map is asked about its **values** and not about its keys.
///
/// A map's keys are keys by construction — it could not have been built
/// otherwise — so the walk reads the entry at the `MapEntry` layout's width
/// and looks past the key's words. `key::admits` does exactly this, and a
/// walk that asked about the keys too would be slower and no more correct.
#[test]
fn a_map_is_asked_about_its_values_and_not_its_keys() {
    let program = keyed("", "Map<Int, Float>");
    let listed = synthesized(&program, "refuses<Map");
    assert!(listed.contains("load-elem"), "{listed}");
    // One loop, not two.
    assert_eq!(listed.matches("load-elem").count(), 1, "{listed}");
}

/// The two things no walk can be composed for, and the fallback they reach.
///
/// A box's family is a word in its own header. A layout that holds itself has
/// values that nest as deep as they like, where a walk expanded in place is
/// finite — and handing it over is also what keeps the runtime's depth bound
/// governing the values that could reach it, which is the one place this
/// migration does *not* move a bound the two before it moved.
#[test]
fn a_box_and_a_layout_that_holds_itself_reach_the_fallback() {
    let boxed = keyed(
        "trait Summary { fn summarize(self) -> String }\n\
         struct Booking { id: Int }\n\
         impl Summary for Booking { fn summarize(self) -> String { \"{self.id}\" } }",
        "dyn Summary",
    );
    assert!(admissions(&boxed) > 0, "no `Value.admitKey` for a box");
    assert_eq!(walks_named(&boxed, "refuses<"), Vec::<String>::new());

    let deep = keyed("struct Node { tag: Int, kids: Array<Node> }", "Node");
    assert!(
        admissions(&deep) > 0,
        "no `Value.admitKey` for a layout that holds itself"
    );
    assert_eq!(walks_named(&deep, "refuses<"), Vec::<String>::new());
}

/// **ADR 0064's Decision 4 for the admission, as a fact about every program
/// this crate lowers.**
///
/// The check itself is [`crate::verify::one_admission_boundary`], and it is a
/// pass of its own rather than a line in the verifier because what it checks
/// is what the *lowering* chose and `lower::finish` expands a small leaf
/// before it verifies. What is left for a test is the half a rule cannot
/// state: that the rule bites, and that a program of many key families
/// reaches the intrinsic exactly as often as it holds a key no layout can
/// answer for.
///
/// The programs below hold every composite family whose admission a walk
/// decides — a struct, an enum, an array, a map, an `Option` — and reach
/// `Value.admitKey` **under a branch and never unguarded**, which
/// [`admissions`] checks as it counts.
#[test]
fn no_layout_the_walk_decides_is_asked_about_unguarded() {
    let program = lowered(
        "enum Mark { Plain\n  Count(Int)\n  Weight(Float) }\n\
         struct Holder { id: Int, mark: Mark, label: String }\n\
         fn a(m: Set<Mark>, k: Mark) -> Bool { m.contains(k) }\n\
         fn b(m: Set<Holder>, k: Holder) -> Bool { m.contains(k) }\n\
         fn c(m: Set<Array<Mark>>, k: Array<Mark>) -> Bool { m.contains(k) }\n\
         fn d(m: Set<Map<Int, Mark>>, k: Map<Int, Mark>) -> Bool { m.contains(k) }\n\
         fn e(m: Set<Option<Mark>>, k: Option<Mark>) -> Bool { m.contains(k) }\n\
         fn f(m: Set<Int>, k: Int) -> Bool { m.contains(k) }",
    );
    let names = walks_named(&program, "refuses<");
    assert!(
        names.len() >= 5,
        "one walk per composite family, and these are what there are: {names:?}"
    );
    // Every `Value.admitKey` the program holds is one of those five under its
    // branch; `Int` reaches none at all.
    assert!(admissions(&program) > 0);
}

/// How many `Value.admitKey` sites the program holds, checking as it counts
/// that each is either a layout no walk can be composed for or one under the
/// branch on what a walk answered.
///
/// The second half duplicates [`crate::verify::one_admission_boundary`] on
/// purpose, for [`reached`]'s reason: the pass panics through
/// `lower::finish`, and a panic is a worse thing for a test to read than an
/// assertion is. It is asked of the *finished* program, where the pass is
/// asked of the one the lowering emitted, so what it can still say is that no
/// site is about a layout the admission already answered.
fn admissions(program: &Program) -> usize {
    let mut found = 0;
    for function in &program.functions {
        for inst in &function.code {
            let Inst::IntrinsicCall { site, args, .. } = inst else {
                continue;
            };
            if program.intrinsic_site(*site).intrinsic != crate::Intrinsic::ValueAdmitKey {
                continue;
            }
            found += 1;
            let Some(arg) = program.arg_list(*args).first() else {
                continue;
            };
            assert_ne!(
                synth::admission(&program.layouts, arg.layout),
                synth::Admission::Always,
                "`Value.admitKey` was asked about a `{}`, every value of which is a key",
                program.layout(arg.layout).name
            );
        }
    }
    found
}

/// Every synthesized function whose name holds `what`.
fn walks_named(program: &Program, what: &str) -> Vec<String> {
    walks_of(program)
        .into_iter()
        .filter(|name| name.contains(what))
        .collect()
}

/// How many `Any.equals` sites the program holds.
fn fallbacks(program: &Program) -> usize {
    reached(program, crate::Intrinsic::AnyEquals)
}

/// How many `Value.order` sites it holds.
fn orderings(program: &Program) -> usize {
    reached(program, crate::Intrinsic::ValueOrder)
}

/// How many sites of `intrinsic` the program holds, checking as it counts
/// that every operand of every one of them is erased.
///
/// The second half duplicates `crate::verify`'s rule on purpose: the verifier
/// panics through `lower::finish`, and a panic is a worse thing for a test to
/// read than an assertion is.
fn reached(program: &Program, intrinsic: crate::Intrinsic) -> usize {
    let mut found = 0;
    for function in &program.functions {
        for inst in &function.code {
            let Inst::IntrinsicCall { site, args, .. } = inst else {
                continue;
            };
            if program.intrinsic_site(*site).intrinsic != intrinsic {
                continue;
            }
            found += 1;
            for arg in program.arg_list(*args) {
                assert!(
                    matches!(program.layout(arg.layout).shape, Shape::Boxed),
                    "`{intrinsic}` was handed a `{}`, whose layout is known",
                    program.layout(arg.layout).name
                );
            }
        }
    }
    found
}

/// How many instructions of the whole program satisfy `wanted`.
fn count(program: &Program, wanted: impl Fn(&Inst) -> bool) -> usize {
    program
        .functions
        .iter()
        .flat_map(|function| &function.code)
        .filter(|inst| wanted(inst))
        .count()
}

/// A program whose `std.map` or `std.set` is instantiated at a key layout,
/// which is the only way a Cove source reaches `core.order`.
///
/// `core.*` is reserved to the standard library, so nothing a test can write
/// calls the order directly: what reaches it is a lookup in a keyed
/// collection, and the instantiation of `seekMap` or `seekSet` at that key is
/// where the comparison is lowered.
fn keyed(declarations: &str, key: &str) -> Program {
    lowered(&format!(
        "{declarations}\nfn look(m: Map<{key}, Int>, k: {key}) -> Bool {{ m.contains(k) }}"
    ))
}

/// **ADR 0064's Decision 3, second paragraph, as a fact about the two
/// backends' source.**
///
/// > The synthesized function is ordinary function and control-flow IR. It is
/// > visible to the printer, the verifier, the optimizer and both code
/// > generators as a function, and **no backend may recognize it by name.** A
/// > backend that matches on a standard-library function name has reinvented
/// > the string dispatch ADR 0058 deleted.
///
/// A synthesized function has no `FunctionId` a backend can be told about in
/// advance and no entry in [`crate::Program::by_name`]; the only handle on it
/// is the two strings it carries for a listing, a profile row and a
/// backtrace. `lower::synth::MODULE` is `pub(super)` inside a private module,
/// so a backend cannot import either of them — the only way to match on one
/// is to write the string out, and that is what this looks for.
///
/// **What it catches**, concretely, is an arm like
/// `if function.module == "<synth>" { /* the fast path */ }` in the encoded
/// VM, or `name.starts_with("equals<")` in the native tier's `subset`, put
/// there to give a walk special treatment — an inline expansion, a refusal, a
/// hand-written helper. Each of those is a backend deciding what a function
/// *means* from what it is called, which is exactly ADR 0058's two-string
/// dispatch coming back one name at a time, and each of them would pass every
/// other test in this repository because the answers would still be right.
///
/// It is a source scan and it does not pretend to be more. What it cannot
/// catch is a backend that recognized a walk by its *shape*. That would be an
/// optimizer doing its job on ordinary IR, which is the thing Decision 3
/// wants.
#[test]
fn no_backend_names_a_synthesized_function() {
    // One entry per verb `synth::Operation::verb` answers, and the two
    // paths a backend could reach the module by. A verb added there owes a
    // string here: what makes the scan worth anything is that it names every
    // form a walk's name can take.
    let forbidden = [
        "<synth>",
        "equals<",
        "order<",
        "refuses<",
        "synth::",
        "lower::synth",
    ];
    let mut faults = Vec::new();
    for crate_name in ["cove-native", "cove-runtime"] {
        let root = workspace().join("crates").join(crate_name).join("src");
        for file in rust_files(&root) {
            let text = std::fs::read_to_string(&file).expect("a source file reads");
            for (number, line) in text.lines().enumerate() {
                // A doc comment or a note may name the mechanism; what may
                // not is code that acts on the name.
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                for held in forbidden {
                    if line.contains(held) {
                        faults.push(format!(
                            "{}:{}: names `{held}`\n  {}",
                            file.display(),
                            number + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        faults.is_empty(),
        "a backend recognizes a synthesized function by its name, which ADR 0064's \
         Decision 3 refuses:\n{}",
        faults.join("\n")
    );
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root exists")
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|held| held == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}
