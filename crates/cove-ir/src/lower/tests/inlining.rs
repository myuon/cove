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

use super::super::inline;
use super::checked;
use crate::{lower, ArithOp, Function, FunctionId, Inst, Program, RefMap, Repr};

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

/// A caller built by hand around one call to `callee`, with a frame of exactly
/// `words` words: the answer in slot zero, the arguments after it, then `Int`
/// padding.
///
/// The lowering chooses a caller's frame, and a test that is about where a
/// budget rule falls needs to choose it instead — so the caller is written
/// out, appended to a lowered program, and expanded by
/// `inline::expand_cold`, which is the pass's own `expand` over one function.
fn caller_of(program: &mut Program, callee: &str, words: usize) -> FunctionId {
    let at = program
        .functions
        .iter()
        .position(|f| f.qualified() == callee)
        .unwrap_or_else(|| panic!("`{callee}` was lowered"));
    let leaf = program.functions[at].clone();
    let answer = program.layout(leaf.returns).width();
    let mut args = Vec::new();
    let mut slot = answer;
    for layout in &leaf.params {
        args.push(crate::program::Arg {
            slot,
            layout: *layout,
        });
        slot += program.layout(*layout).width();
    }
    let mut reprs = vec![Repr::Int; slot as usize];
    let mut param = 0;
    for arg in &args {
        let width = program.layout(arg.layout).width() as usize;
        let first = arg.slot as usize;
        reprs[first..first + width].copy_from_slice(&leaf.reprs[param..param + width]);
        param += width;
    }
    assert!(
        words >= reprs.len(),
        "a frame of {words} cannot hold the call"
    );
    reprs.resize(words, Repr::Int);
    let listed = program.args.len() as u32;
    program.args.push(args);
    let mut caller = leaf.clone();
    caller.name = "caller".into();
    caller.params = Vec::new();
    caller.refs = RefMap::of(&reprs);
    caller.reprs = reprs;
    caller.code = vec![
        Inst::Call {
            dst: 0,
            callee: FunctionId(at as u32),
            args: crate::ArgsId(listed),
        },
        Inst::Return { src: 0 },
    ];
    caller.spans = vec![leaf.span; 2];
    caller.locals = Vec::new();
    caller.inlined = Vec::new();
    program.functions.push(caller);
    FunctionId(program.functions.len() as u32 - 1)
}

/// Whether the hand-built caller still calls something.
fn still_calls(program: &Program, id: FunctionId) -> bool {
    program
        .function(id)
        .code
        .iter()
        .any(|inst| matches!(inst, Inst::Call { .. }))
}

/// The frame budget is charged the words an expansion appends, not the
/// callee's whole frame.
///
/// `offset` never writes its four parameters, so an expansion reads them
/// where the caller has them and appends only the rest of the frame. A caller
/// with exactly that much room left expands it and ends at the budget to the
/// word; one word less and it is left a call. Charged by the whole frame — the
/// rule this replaced — the first caller was refused too.
#[test]
fn the_frame_budget_is_charged_what_an_expansion_appends() {
    let (mut program, _) = program(
        "fn offset(a: Int, b: Int, c: Int, d: Int) -> Int {\n  let x = a + b\n  let y = c * d\n  x - y\n}\n\
         fn main() -> Int { offset(1, 2, 3, 4) }",
    );
    let leaf = program
        .functions
        .iter()
        .find(|f| f.qualified() == "m.offset")
        .expect("`offset` was lowered")
        .clone();
    let appended = inline::appended_words(&program, &leaf);
    assert!(
        appended > 0 && appended + 4 <= leaf.reprs.len(),
        "the parameters are not charged: {appended} of {}",
        leaf.reprs.len()
    );

    let fits = caller_of(&mut program, "m.offset", inline::FRAME_BUDGET - appended);
    inline::expand_cold(&mut program, fits);
    assert!(
        !still_calls(&program, fits),
        "the call that fits is expanded"
    );
    assert_eq!(
        program.function(fits).reprs.len(),
        inline::FRAME_BUDGET,
        "and it appended exactly what it was charged"
    );

    let over = caller_of(
        &mut program,
        "m.offset",
        inline::FRAME_BUDGET - appended + 1,
    );
    inline::expand_cold(&mut program, over);
    assert!(
        still_calls(&program, over),
        "one word past the budget, it is left a call"
    );
}

/// A thin standard-library wrapper is expanded in a caller that has spent
/// its whole budget, and the same body declared by the program is not.
///
/// `std.duration.millis` is a builtin read and a division, which is what
/// `is_thin_library` admits whatever the caller holds. `millisOf` is that body
/// word for word in the program's own module, and a caller already over the
/// budget leaves it a call — so what decided was whose function it is, not
/// its shape.
#[test]
fn a_thin_library_wrapper_is_expanded_past_the_budget() {
    let (mut program, _) = program(
        "fn millisOf(duration: Duration) -> Int {\n  duration.nanos() / 1_000_000\n}\n\
         fn main() -> Int {\n  let d = Duration.seconds(1)\n  d.millis() + millisOf(d)\n}",
    );
    let library = program
        .functions
        .iter()
        .find(|f| f.qualified() == "std.duration.millis")
        .expect("`std.duration.millis` was lowered");
    assert!(inline::is_thin_library(library), "{:?}", library.code);

    let wide = inline::FRAME_BUDGET + 8;
    let thin = caller_of(&mut program, "std.duration.millis", wide);
    inline::expand_cold(&mut program, thin);
    assert!(
        !still_calls(&program, thin),
        "the library's wrapper is expanded"
    );

    let own = caller_of(&mut program, "m.millisOf", wide);
    let mine = program
        .functions
        .iter()
        .find(|f| f.qualified() == "m.millisOf")
        .expect("`millisOf` was lowered");
    assert!(!inline::is_thin_library(mine));
    inline::expand_cold(&mut program, own);
    assert!(
        still_calls(&program, own),
        "the program's own body of the same shape is left a call"
    );
}
