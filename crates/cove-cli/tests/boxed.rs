//! How much of the repository still reaches a boxed-value fallback, counted.
//!
//! [ADR 0068](../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)
//! moves the last four `Intrinsic` variants — `Any.equals`, `Value.order`,
//! `Value.admitKey` and `Value.renderInto` — into Cove over a structural view of
//! a type-erased value. Each is reached today only where its operand's layout is
//! `Shape::Boxed`: a `dyn Trait`, or a Host schema's `Any`, whose concrete layout
//! is known only when the box is opened. Everywhere a layout is known, ADR 0064's
//! Decision 3 has already written a walk the lowering composes.
//!
//! Its Phase 0 asks for a census of both populations, and its gates make one of
//! them a ratchet: *"an increase in its fallback count is a failed gate."* So this
//! file lowers every program the repository keeps — the corpus `copies.rs` and
//! `bytecode_corpus.rs` walk — and counts, per program:
//!
//! - **the fallback**: `IntrinsicCall` sites of each variant still in Rust. These
//!   are the operations ADR 0068's reflection walk will answer instead, and the
//!   number may fall and may never rise;
//! - **the reflected population**: calls of `std.dynamic.equals`, which is what
//!   `Any.equals` became in Phase 2 — the fallback already answered in Cove, over a
//!   `DynamicView` of each box. Ratcheted like the fallback, because it *is* the
//!   fallback, and held to Decision 5: every operand of every call is erased;
//! - **the specialized population**: functions the lowering synthesized
//!   (module `<synth>`), which is ADR 0064's Decision 3 working. Reported and not
//!   ratcheted — a program that compares more kinds of value legitimately has more
//!   of them — so that the reflected and the specialized can be read side by side,
//!   which ADR 0068's Phase 2 asks for.
//!
//! Static counts are not executed counts: a site in a function nothing calls is
//! counted here and never runs. The executed side is a run's `--boundary`
//! report, and the two are recorded together in the pull request that lands a
//! phase.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_ir::layout::Shape;
use cove_ir::{Inst, Intrinsic, Program, Repr};
use cove_sema::HostSchemas;

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use support::{Case, ModuleIndex, Prepared};

/// Every entry point of the repository, in a fixed order: `copies.rs`' set.
fn discover() -> Vec<Case> {
    let root = support::repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(support::nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));
    roots.push(root.join("benches"));
    roots
        .iter()
        .flat_map(|package| support::cases_of(&root, package))
        .collect()
}

/// The operations ADR 0068 moves that are still intrinsics, in its order.
///
/// It was four. `Any.equals` left in Phase 2 and is counted as
/// [`Counts::reflected`] instead.
const BOXED: [Intrinsic; 3] = [
    Intrinsic::ValueOrder,
    Intrinsic::ValueAdmitKey,
    Intrinsic::ValueRenderInto,
];

/// The Cove function `==` answers two erased values with since ADR 0068's
/// Phase 2.
const REFLECTED_EQUALS: (&str, &str) = ("std.dynamic", "equals");

/// Why a site calls one of [`BOXED`], read off its first operand's layout.
///
/// Only the first is what ADR 0068 moves. The census found the other two, and
/// they are recorded apart so that nobody reads a large number as a large
/// reflection migration:
///
/// - **boxed**: the operand is `Shape::Boxed`, and the concrete layout is known
///   only when the box is opened — the reflection walk's population;
/// - **scalar**: the operand is a `Float` or a `Duration` word, which
///   `Value.renderInto` renders because no Cove body writes those two scalars'
///   text. It is not a dynamic value at all, and a `DynamicView` does not move it;
/// - **known**: any other layout — the operand's layout is known and the site is
///   still there, which is issue #476's shape for `Value.admitKey`: the
///   synthesized walk *decides*, and the intrinsic stays at the site to word the
///   refusal in the same frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    Boxed,
    Scalar,
    Known,
}

/// One program's counts.
#[derive(Clone, Copy, Default)]
struct Counts {
    /// `IntrinsicCall` sites per entry of [`BOXED`], per [`Why`] in its order.
    sites: [[usize; 3]; 3],
    /// Calls of [`REFLECTED_EQUALS`]: `==` on two erased values, from a
    /// comparison or from inside a synthesized walk that reached a boxed part.
    reflected: usize,
    /// Functions the lowering synthesized.
    synthesized: usize,
}

fn why(program: &Program, args: cove_ir::ArgsId) -> Why {
    let Some(first) = program.arg_list(args).first() else {
        return Why::Known;
    };
    match &program.layout(first.layout).shape {
        Shape::Boxed => Why::Boxed,
        Shape::Word(Repr::Float) | Shape::Word(Repr::Duration) => Why::Scalar,
        _ => Why::Known,
    }
}

/// ADR 0068's Decision 5, as a fact about one call: a call of
/// [`REFLECTED_EQUALS`] whose operand has a layout the lowering knows would be a
/// static layout routed through reflection, which the gate calls a failure —
/// the walk the lowering writes for that layout is the fast path, and this is
/// the fallback.
fn reflected(
    program: &Program,
    function: &str,
    callee: cove_ir::FunctionId,
    args: cove_ir::ArgsId,
) -> bool {
    let called = &program.functions[callee.index()];
    if (&*called.module, &*called.name) != REFLECTED_EQUALS {
        return false;
    }
    for arg in program.arg_list(args) {
        let described = program.layout(arg.layout);
        assert!(
            matches!(described.shape, Shape::Boxed),
            "`{function}` calls `std.dynamic.equals` over a `{}`, whose layout is known: ADR \
             0068's Decision 5 keeps a static layout off the reflected path",
            described.name
        );
    }
    true
}

fn count(program: &Program) -> Counts {
    let mut found = Counts::default();
    for function in &program.functions {
        if &*function.module == "<synth>" {
            found.synthesized += 1;
        }
        let name = format!("{}.{}", function.module, function.name);
        for inst in &function.code {
            if let Inst::Call { callee, args, .. } = inst {
                if reflected(program, &name, *callee, *args) {
                    found.reflected += 1;
                }
            }
            if let Inst::IntrinsicCall { site, args, .. } = inst {
                let intrinsic = program.intrinsic_site(*site).intrinsic;
                if let Some(at) = BOXED.iter().position(|each| *each == intrinsic) {
                    let column = match why(program, *args) {
                        Why::Boxed => 0,
                        Why::Scalar => 1,
                        Why::Known => 2,
                    };
                    found.sites[at][column] += 1;
                }
            }
        }
    }
    found
}

/// The whole-corpus sites per operation and per [`Why`], which may fall and may
/// never rise. Recorded for ADR 0068's Phase 0, before any of the four has moved,
/// over 210 programs and 544 synthesized functions:
///
/// | | boxed | scalar | known |
/// | --- | ---: | ---: | ---: |
/// | `Any.equals` | 20 | 0 | 0 |
/// | `Value.order` | 41 | 0 | 0 |
/// | `Value.admitKey` | 17 | 1 | 112 |
/// | `Value.renderInto` | 29 | 669 | 4 |
///
/// **Two of the columns are not reflection's to empty**, and both are larger than
/// the column that is. `Value.renderInto`'s 669 scalar sites are `Float` and
/// `Duration` interpolations — a `Float`'s shortest round-trip text is an
/// algorithm of its own, and no `DynamicView` writes it — so deleting that variant
/// needs those two renderings in Cove as well. `Value.admitKey`'s 112 known sites
/// are issue #476's shape, a synthesized walk deciding and the intrinsic staying
/// only to word the refusal, which it did because a trap carried one string; ADR
/// 0067 lifted that, so they can move to `core.refuse` independently.
///
/// Every boxed site is in a corpus or a benchmark written to reach it: no example
/// program has one.
///
/// **ADR 0068's Phase 2 took the first row out of this table**, and it is
/// [`REFLECTED`] now: `Any.equals` is `std.dynamic.equals`, a call rather than an
/// intrinsic site, so it has no scalar or known column — Decision 5 is asserted
/// of every call instead.
///
/// **Then Phase 0 added the corpus that pins the boxed contract**, and the boxed
/// column rose by exactly what those three programs are for — measured with them
/// held out, where it is the table above to the site: `tests/e2e/values_boxed`
/// 104, 8, 10 and 14 boxed sites, and `fail_key_boxed_float` and
/// `fail_key_boxed_vector` 0, 3, 2 and 3 each. The scalar and known columns did
/// not move. A corpus written to reach a fallback raises its count by the rows it
/// has; what this ratchet is for is that nothing *else* does.
const FALLBACK_SITES: [[usize; 3]; 3] = [[55, 0, 0], [31, 1, 112], [49, 669, 4]];

/// The whole-corpus calls of `std.dynamic.equals`, which may fall and may never
/// rise: the reflected population, ADR 0068's Phase 2 asks for it reported apart
/// from the specialized one.
///
/// Measured when Phase 2 landed, over 215 programs:
///
/// - **114** in the 213 programs that were here before it, against the 124
///   `Any.equals` sites they held. The ten are not ten comparisons that stopped
///   reaching the fallback: a synthesized walk around a boxed part — an
///   `Option<dyn Summary>`'s, an `Array<dyn Summary>`'s — held one intrinsic site
///   and was a leaf, so `lower::inline` expanded it into every caller and the site
///   was counted once per expansion as well as in the walk. It holds a call now,
///   is no longer a leaf, and is counted once. `values_any_equals` and
///   `values_value_order` together went 19 to 9, and `values_boxed` stayed at 104
///   and `benches/equals` at 1;
/// - **14** more from the two programs Phase 2 added to pin what it changed,
///   `values_boxed_deep` and `values_boxed_generic`, 7 each.
const REFLECTED: usize = 128;

#[test]
fn the_corpus_says_how_much_of_it_still_reaches_a_boxed_fallback() {
    let mut total = Counts::default();
    let mut rows: Vec<(String, Counts)> = Vec::new();
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));
        let Ok(prepared) = Prepared::of(&case, index) else {
            continue;
        };
        let Ok(program) = cove_ir::lower(&prepared.checked, &prepared.sources, &HostSchemas::new())
        else {
            continue;
        };
        let found = count(&program);
        for at in 0..3 {
            for column in 0..3 {
                total.sites[at][column] += found.sites[at][column];
            }
        }
        total.reflected += found.reflected;
        total.synthesized += found.synthesized;
        rows.push((case.name.clone(), found));
    }
    println!(
        "{} program(s), {} synthesized function(s), {} reflected call(s) of \
         `std.dynamic.equals`; sites as boxed / scalar / known:",
        rows.len(),
        total.synthesized,
        total.reflected
    );
    for (intrinsic, sites) in BOXED.iter().zip(&total.sites) {
        println!(
            "  {:<18} {:>5} {:>5} {:>5}",
            intrinsic.to_string(),
            sites[0],
            sites[1],
            sites[2]
        );
    }
    println!("\n  reflected equals, boxed per operation (order admit render), then synth, program");
    for (name, found) in &rows {
        let boxed = [
            found.reflected,
            found.sites[0][0],
            found.sites[1][0],
            found.sites[2][0],
        ];
        if boxed.iter().any(|n| *n > 0) {
            println!(
                "  {:>4} {:>4} {:>4} {:>4} {:>5}  {name}",
                boxed[0], boxed[1], boxed[2], boxed[3], found.synthesized
            );
        }
    }
    assert!(
        total.reflected <= REFLECTED,
        "the corpus has {} call(s) of `std.dynamic.equals`, and the ratchet is {REFLECTED}. It \
         may fall and never rise: ADR 0068's gate is that a fallback count does not increase.",
        total.reflected
    );
    let columns = ["boxed", "scalar", "known"];
    for at in 0..3 {
        for column in 0..3 {
            assert!(
                total.sites[at][column] <= FALLBACK_SITES[at][column],
                "the corpus has {} {} `{}` site(s), and the ratchet is {}. It may fall and never \
                 rise: ADR 0068's gate is that a fallback count does not increase.",
                total.sites[at][column],
                columns[column],
                BOXED[at],
                FALLBACK_SITES[at][column],
            );
        }
    }
}
