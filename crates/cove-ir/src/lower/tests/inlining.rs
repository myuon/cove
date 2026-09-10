//! What an expansion leaves behind, asserted against the counters it names.
//!
//! `super::super::inline` writes no listing of its own — what it does is
//! visible in every other file here, because a lowered call to a small leaf is
//! not a call any more. What is *not* visible anywhere else is
//! [`Inlined`](crate::program::Inlined): nothing in a lowered function reads
//! it, so a range that has drifted still verifies, still runs, and still
//! answers. It answers about the wrong instructions, which only an error
//! chain or a backtrace would ever notice.

use cove_schema::HostSchemas;

use super::checked;
use crate::{lower, ArithOp, Function, Inst, Program};

/// The lowered program, and `m.main` within it.
fn program(source: &str) -> (Program, Function) {
    let (sources, held) = checked(source);
    let program = lower(&held, &sources, &HostSchemas::new()).expect("the program lowers");
    let main = program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "main")
        .expect("`main` was lowered")
        .clone();
    (program, main)
}

/// A call to a small leaf is gone, and what took its place says where it
/// was called from.
///
/// The two halves are one fact. An expansion that removed the call without
/// recording it would pass the first assertion and leave an error raised
/// inside the expanded body with no caller to name — which is what the
/// oracle reports and the machine used not to.
#[test]
fn an_expanded_call_records_the_site_it_removed() {
    let (_, main) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    assert!(
        !main
            .code
            .iter()
            .any(|inst| matches!(inst, Inst::Call { .. })),
        "the call was expanded"
    );
    let at = main
        .code
        .iter()
        .position(|inst| {
            matches!(
                inst,
                Inst::Arith {
                    op: ArithOp::Div,
                    ..
                }
            )
        })
        .expect("the leaf's division is in `main` now");
    let sites: Vec<_> = main.inlined_at(at as u32).map(|held| held.site).collect();
    assert_eq!(sites.len(), 1, "one expansion covers the division");
}

/// The range still names the division after the passes that renumber.
///
/// `super::super::inline` runs before `tails` and `frees`, both of which
/// drop [`Inst::Clear`]s and move every counter after them. `dropping::rewrite`
/// moves a `Local`'s pair and has to move an `Inlined`'s the same way; when
/// it did not, this program's range sat two counters past the division and
/// the error it raises reported no caller at all.
///
/// The `var` is what makes the difference: it is a reference the caller
/// clears, so there are clears between the top of `main` and the expansion
/// for `frees` to take out.
#[test]
fn an_expansion_s_range_survives_the_passes_that_renumber() {
    let (_, plain) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    let (_, after) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\n\
         fn main() -> Int {\n  let s = \"ab\"\n  divide(s.length(), 0)\n}",
    );
    let division = |f: &Function| {
        f.code
            .iter()
            .position(|inst| {
                matches!(
                    inst,
                    Inst::Arith {
                        op: ArithOp::Div,
                        ..
                    }
                )
            })
            .expect("the leaf's division is in `main` now")
    };
    assert!(
        division(&after) > division(&plain),
        "the second program has instructions before the expansion"
    );
    assert_eq!(
        after.inlined_at(division(&after) as u32).count(),
        1,
        "the recorded range still covers the division it was made for"
    );
}

/// A body that can fail is expanded.
///
/// This is the rule [`Inlined`](crate::program::Inlined) removed, pinned so
/// that it is not put back by accident: `divide` divides, a division by zero
/// stops the run, and the pass expands it anyway because the record is what
/// keeps the chain rather than the frame.
#[test]
fn a_leaf_that_can_fail_is_expanded_because_the_record_keeps_its_caller() {
    let (_, main) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    assert!(!main
        .code
        .iter()
        .any(|inst| matches!(inst, Inst::Call { .. })));
}

/// The names an expanded body bound are recorded with it, not merged into
/// the caller's table.
///
/// The two cannot be told apart afterwards, which is the whole reason. In
/// `raise` below, `let raised = n + 1` binds at the counter the expansion
/// begins at — the argument needed no copy, so the body's first instruction
/// *is* that counter — and `twice`'s own parameter `n` is bound there too.
/// A reader sorting one table by "which expansion contains this counter"
/// would put both in the same body, and a debugger stopped inside `twice`
/// would offer a name `twice` never bound.
#[test]
fn an_expansion_records_the_names_the_expanded_body_bound() {
    let (program, _) = program(
        "fn twice(n: Int) -> Int {\n  let doubled = n * 2\n  doubled\n}\n\
         fn raise(n: Int) -> Int {\n  let raised = n + 1\n  twice(raised)\n}\n\
         fn main() -> Int { raise(1) }",
    );
    let raise = program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "raise")
        .expect("`raise` was lowered");
    let held = raise
        .inlined
        .first()
        .expect("`twice` was expanded into `raise`");
    let mut names: Vec<&str> = held.locals.iter().map(|local| &*local.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["doubled", "n"],
        "the expansion carries the callee's names and only the callee's"
    );
    assert!(
        raise.locals.iter().all(|local| &*local.name != "doubled"),
        "and the caller's table is not where they went"
    );
}
