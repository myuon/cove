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
use crate::inst::Inst;
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
/// What it catches is the cheap way out of the next three migrations. When
/// `Value.order`'s producer is written, a layout whose arm is awkward —
/// a `Shared`, a host handle, a case with an unusual payload — can be made to
/// work by handing it to the intrinsic, and everything passes: the answer is
/// right, the corpus is green, and the architecture is exactly where it was.
/// This is what says no.
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

/// How many `Any.equals` sites the program holds, checking as it counts that
/// every operand of every one of them is erased.
///
/// The second half duplicates `crate::verify`'s rule on purpose: the verifier
/// panics through `lower::finish`, and a panic is a worse thing for a test to
/// read than an assertion is.
fn fallbacks(program: &Program) -> usize {
    let mut found = 0;
    for function in &program.functions {
        for inst in &function.code {
            let Inst::IntrinsicCall { site, args, .. } = inst else {
                continue;
            };
            if program.intrinsic_site(*site).intrinsic != crate::Intrinsic::AnyEquals {
                continue;
            }
            found += 1;
            for arg in program.arg_list(*args) {
                assert!(
                    matches!(program.layout(arg.layout).shape, Shape::Boxed),
                    "`Any.equals` was handed a `{}`, whose layout is known",
                    program.layout(arg.layout).name
                );
            }
        }
    }
    found
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
    let forbidden = ["<synth>", "equals<", "synth::", "lower::synth"];
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
