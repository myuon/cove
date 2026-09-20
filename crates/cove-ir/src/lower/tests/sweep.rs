//! What the finished program stopped naming, and what it still does.
//!
//! `lower::sweep` runs after `lower::inline`, over a sliced lowering, and
//! stands down every function nothing in the program names any more. The
//! cases here are the two halves of that: the one it must sweep — a function
//! every call site of which was expanded, which is issue #440 and what
//! `std.string.length` and `std.string.endsWith` each turned out to be on
//! `examples/covefmt` — and the several it must not.
//!
//! The ones it must not are the point. A function reached only as a *value*
//! is named by no call site, so a sweep that counted `Inst::Call` alone would
//! delete a live body and the program would run into a stub that answers
//! `()`. Each of those cases is written here against the pass that has to
//! know better.

use std::collections::HashSet;

use cove_schema::HostSchemas;

use crate::inst::Inst;
use crate::program::{FunctionId, Program};

use super::checked;

/// A lowering sliced to `entries`, the way a command lowers what it is about
/// to run.
fn sliced_program(source: &str, entries: &[&str]) -> Program {
    let (sources, checked) = checked(source);
    let roots: Vec<(&str, &str)> = entries.iter().map(|entry| ("m", *entry)).collect();
    crate::lower_roots(&checked, &sources, &HostSchemas::new(), &roots)
        .expect("the roots' program lowers")
}

/// Whether `module.name` is a stand-in rather than a body.
fn stood_down(program: &Program, module: &str, name: &str) -> bool {
    program
        .functions
        .iter()
        .find(|f| &*f.module == module && &*f.name == name)
        .unwrap_or_else(|| panic!("`{module}.{name}` is in the program"))
        .is_stub()
}

/// How many instructions the program emits, which is what a stub does not
/// count towards.
///
/// The same sum `cove_runtime::boundary`'s `Emitted` takes, and the figure
/// issue #440 is about: a dead body is in the emitted IR, and therefore in
/// the encoded bytecode and in the machine code, until something takes it
/// out.
fn emitted(program: &Program) -> usize {
    program.functions.iter().filter(|f| !f.is_stub()).count()
}

// ---- what it sweeps -----------------------------------------------------

/// **A function expanded at its only call site is not in the program.**
///
/// This is issue #440 in miniature. `double` is a small leaf, so
/// `lower::inline` puts its two instructions where the call stood, and after
/// that nothing calls it — the slice that decided to lower it was closed
/// before the expansion ran and nothing asked again. It was emitted anyway,
/// encoded anyway, and compiled by the native tier anyway: on
/// `examples/covefmt` that was 1,856 bytes of machine code for
/// `std.string.endsWith`, the entire machine-code delta of the change that
/// moved it into Cove.
#[test]
fn a_function_expanded_at_its_only_call_site_is_not_emitted() {
    let source = "fn double(n: Int) -> Int { n * 2 }\n\
                  fn main() -> Int { double(21) }";
    let program = sliced_program(source, &["main"]);
    assert!(
        stood_down(&program, "m", "double"),
        "nothing calls `double`, so nothing needs its body"
    );
    let main = program.function(
        program
            .function_named("m", "main")
            .expect("main is nameable"),
    );
    assert!(
        main.inlined
            .iter()
            .any(|held| { &*program.function(held.callee).name == "double" }),
        "and the record of the expansion still names it: {:?}",
        main.inlined
    );
}

/// The library case, which is the one that was measured twice.
///
/// `String.length` is a thin standard-library wrapper, expanded wherever it
/// is called (ADR 0058), and a program with one site therefore has no call to
/// it left. #438 measured exactly this on `examples/covefmt`: emitted 214 →
/// 215 functions, compiled 208 → 209, zero calls and no profile row.
#[test]
fn a_standard_library_body_every_site_expanded_is_not_emitted() {
    // Inside a loop, which is what makes the site a hot one: `length` is a
    // `while` over the bytes and is past the cold limit, so a call written
    // once keeps its call. That is issue #440's own point — the condition is
    // a property of the program, not of the function — and `examples/covefmt`
    // is the program where both of its sites are hot.
    let source = "fn main(s: String) -> Int {\n\
                  \x20 var total = 0\n\
                  \x20 var i = 0\n\
                  \x20 while i < 4 {\n\
                  \x20   total += s.length()\n\
                  \x20   i += 1\n\
                  \x20 }\n\
                  \x20 total\n\
                  }";
    let program = sliced_program(source, &["main"]);
    assert!(
        stood_down(&program, "std.string", "length"),
        "the only site expanded it, so the program does not carry it"
    );
}

/// **Transitively, to a fixed point.**
///
/// Standing `outer` down removes the call `outer` held, which was the last
/// one to `inner`. One round finds `outer` and the next finds `inner`, and a
/// pass that stopped after one round would leave the second body in the
/// program — which is the same bug one level down.
///
/// `inner` is past the cold limit so that it is not simply expanded into
/// `outer`: what removes the reference to it has to be the sweep.
#[test]
fn standing_one_function_down_reaches_what_only_it_named() {
    let source = "fn inner(n: Int) -> Int {\n\
                  \x20 var acc = n\n\
                  \x20 acc += 1\n\
                  \x20 acc += 2\n\
                  \x20 acc += 3\n\
                  \x20 acc += 4\n\
                  \x20 acc += 5\n\
                  \x20 acc += 6\n\
                  \x20 acc += 7\n\
                  \x20 acc += 8\n\
                  \x20 acc += 9\n\
                  \x20 acc += 10\n\
                  \x20 acc += 11\n\
                  \x20 acc += 12\n\
                  \x20 acc += 13\n\
                  \x20 acc += 14\n\
                  \x20 acc += 15\n\
                  \x20 acc += 16\n\
                  \x20 acc\n\
                  }\n\
                  fn outer(n: Int) -> Int { inner(n) }\n\
                  fn main() -> Int { 1 }";
    let program = sliced_program(source, &["main", "outer"]);
    assert!(
        !stood_down(&program, "m", "outer"),
        "`outer` is a root of this slice"
    );
    assert!(
        !stood_down(&program, "m", "inner"),
        "and `outer` calls it, so it stays"
    );

    // The same package, sliced to `main` alone. Now `outer` is reached by the
    // call graph and lowered, nothing calls it, and `inner` is named by
    // nothing but `outer`'s body.
    let program = sliced_program(source, &["main"]);
    assert!(stood_down(&program, "m", "outer"), "nothing names `outer`");
    assert!(
        stood_down(&program, "m", "inner"),
        "and once `outer` is gone, nothing names `inner` either"
    );
}

/// A root is what a run is about, and nothing inside the program calls it.
#[test]
fn a_root_with_no_callers_is_not_swept() {
    let source = "fn main() -> Int { 1 }\nfn other() -> Int { 2 }";
    let program = sliced_program(source, &["main", "other"]);
    assert!(!stood_down(&program, "m", "main"), "`main` is a root");
    assert!(
        !stood_down(&program, "m", "other"),
        "and so is `other`, which nothing calls"
    );
    // The same package with one root: the other declaration is a stub because
    // the slice never reached it, which is the older reason and still holds.
    let program = sliced_program(source, &["main"]);
    assert!(stood_down(&program, "m", "other"));
}

// ---- what it does not sweep ---------------------------------------------

/// **A function handed to a higher-order builtin as a value survives.**
///
/// This is the case a naive sweep gets wrong. `double` is two instructions
/// and `xs.map(double)` writes no call to it at all: the lowering emits an
/// `Inst::FuncRef` and the walk calls the closure. Count calls alone and
/// `double` is unreferenced, is stood down, and `map` runs into a stub that
/// answers `()` for every element.
#[test]
fn a_function_passed_as_a_value_is_not_swept() {
    let source = "fn double(n: Int) -> Int { n * 2 }\n\
                  fn main(xs: Array<Int>) -> Array<Int> { xs.map(double) }";
    let program = sliced_program(source, &["main"]);
    assert!(
        !stood_down(&program, "m", "double"),
        "`map` reaches it through a word, and a word is a reference"
    );
    // And it is named the way this says it is: by no call, and by a
    // `FuncRef`. If that ever stops being true the case above stops testing
    // what it says it tests.
    let double = program
        .functions
        .iter()
        .position(|f| &*f.module == "m" && &*f.name == "double")
        .map(|at| FunctionId(at as u32))
        .expect("`double` is in the program");
    assert_eq!(
        program
            .functions
            .iter()
            .flat_map(|f| &f.code)
            .filter(|inst| matches!(inst, Inst::Call { callee, .. } if *callee == double))
            .count(),
        0,
        "nothing calls it"
    );
    assert_eq!(
        program
            .functions
            .iter()
            .flat_map(|f| &f.code)
            .filter(|inst| matches!(inst, Inst::FuncRef { callee, .. } if *callee == double))
            .count(),
        1,
        "and one instruction names it as a value"
    );
}

/// A lambda is a function of the program too, and one a closure names.
///
/// It takes captures, so `lower::inline` will not expand it — but nothing
/// about the sweep knows that, and what keeps it is the same `FuncRef` that
/// keeps a declaration used as a value, together with the `Shape::Closure`
/// the environment object carries.
#[test]
fn a_lambda_a_closure_names_is_not_swept() {
    let source = "fn main(n: Int) -> Int {\n\
                  \x20 let add = fn(m: Int) { m + n }\n\
                  \x20 add(1)\n\
                  }";
    let program = sliced_program(source, &["main"]);
    let lambdas: Vec<_> = program
        .functions
        .iter()
        .filter(|f| f.name.contains('#'))
        .collect();
    assert_eq!(lambdas.len(), 1, "one lambda: {:?}", lambdas);
    assert!(!lambdas[0].is_stub(), "and it is not stood down");
}

/// A conformance a `dyn` dispatch can reach through a call it did not expand.
///
/// The bodies here are past the cold limit, so the dispatch's `Switch` keeps
/// its calls and the sweep must keep the conformances. The case where they
/// *are* expanded is [`super::assertions`]'s, and there the bodies end up in
/// the arms instead — both are right, and which one happens is a fact about
/// the program rather than about conformance.
#[test]
fn a_conformance_a_dyn_dispatch_calls_is_not_swept() {
    let wide = |answer: &str| {
        format!(
            "fn shown(self) -> Int {{\n\
             \x20 var acc = self.x\n\
             \x20 acc += 1\n\
             \x20 acc += 2\n\
             \x20 acc += 3\n\
             \x20 acc += 4\n\
             \x20 acc += 5\n\
             \x20 acc += 6\n\
             \x20 acc += 7\n\
             \x20 acc += 8\n\
             \x20 acc += 9\n\
             \x20 acc += 10\n\
             \x20 acc += 11\n\
             \x20 acc += 12\n\
             \x20 acc += 13\n\
             \x20 acc += 14\n\
             \x20 acc += 15\n\
             \x20 acc += {answer}\n\
             \x20 acc\n\
             }}"
        )
    };
    let source = format!(
        "trait D {{ fn shown(self) -> Int }}\n\
         struct P {{ x: Int }}\n\
         struct Q {{ x: Int }}\n\
         impl D for P {{ {} }}\n\
         impl D for Q {{ {} }}\n\
         fn main(d: dyn D) -> Int {{ d.shown() }}",
        wide("8"),
        wide("9")
    );
    let program = sliced_program(&source, &["main"]);
    for conformance in ["P.shown", "Q.shown"] {
        assert!(
            !stood_down(&program, "m", conformance),
            "`{conformance}` is called by the dispatch"
        );
    }
}

// ---- the pass by itself -------------------------------------------------

/// **A sweep that counts only calls deletes a live function.**
///
/// The test above says `double` survives; this says the survival is the
/// pass's doing and not an accident of what happens to be in the program.
/// The naive rule — a reference is an `Inst::Call` — is written out here and
/// run over the same lowering, and it stands `double` down. Anything that
/// weakens `sweep::named_anywhere` back to that rule fails this.
#[test]
fn counting_calls_alone_would_delete_a_function_used_as_a_value() {
    let source = "fn double(n: Int) -> Int { n * 2 }\n\
                  fn main(xs: Array<Int>) -> Array<Int> { xs.map(double) }";
    let program = sliced_program(source, &["main"]);
    let double = program
        .functions
        .iter()
        .position(|f| &*f.module == "m" && &*f.name == "double")
        .map(|at| FunctionId(at as u32))
        .expect("`double` is in the program");

    let roots: HashSet<FunctionId> = program.function_named("m", "main").into_iter().collect();
    let mut called: HashSet<FunctionId> = roots.clone();
    for function in &program.functions {
        for inst in &function.code {
            if let Inst::Call { callee, .. } = inst {
                called.insert(*callee);
            }
        }
    }
    assert!(
        !called.contains(&double),
        "a rule that counted calls alone would find nothing naming `double`, \
         and would stand down the function `map` is about to enter"
    );
    assert!(
        !program.function(double).is_stub(),
        "which is what the pass does not do"
    );
}

/// The whole-package lowering sweeps nothing.
///
/// `lower` means everything the package declares is part of the program, so
/// there is no such thing as a function nothing reaches: a listing, the
/// corpus survey and an embedding that invokes several functions through one
/// `Vm` all lower this way and all three can still name what they lowered.
#[test]
fn a_whole_package_lowering_sweeps_nothing() {
    let source = "fn double(n: Int) -> Int { n * 2 }\n\
                  fn main() -> Int { double(21) }";
    let (sources, checked) = checked(source);
    let program =
        crate::lower(&checked, &sources, &HostSchemas::new()).expect("the program lowers");
    assert!(
        !stood_down(&program, "m", "double"),
        "every declaration is part of a whole-package lowering, expanded or not"
    );
    // And the sliced lowering of the same source does sweep it, which is what
    // says the difference is the roots rather than the program.
    let sliced = sliced_program(source, &["main"]);
    assert!(stood_down(&sliced, "m", "double"));
    assert!(emitted(&sliced) < emitted(&program));
}
