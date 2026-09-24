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
//!   `Any.equals` became in Phase 2, of `std.dynamic.order`, which is what
//!   `Value.order` became in Phase 3, and of `std.dynamic.refusesKey`, which is
//!   what `Value.admitKey`'s *decision* became in Phase 3 — the fallback already
//!   answered in Cove, over a `DynamicView` of each box. Ratcheted like the
//!   fallback, because it *is* the fallback, and held to Decision 5: every
//!   operand of every call is erased;
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
/// It was four. `Any.equals` left in Phase 2 and `Value.order` in Phase 3, and
/// each is counted as a reflected call in [`Counts::reflected`] instead.
const BOXED: [Intrinsic; 2] = [Intrinsic::ValueAdmitKey, Intrinsic::ValueRenderInto];

/// The Cove functions the moved operations became, in [`Counts::reflected`]'s
/// order: what `==` answers two erased values with since ADR 0068's Phase 2,
/// what `core.order` answers two erased keys with since its Phase 3, and what
/// `core.admitKey` decides an erased key with since that phase too.
///
/// The third is half of an operation. `Value.admitKey` stays at every site it
/// was at, under a branch on what `std.dynamic.refusesKey` answered, because it
/// words the refusal and a view cannot render the key a path through a map
/// quotes until Phase 4. So [`FALLBACK_SITES`]' boxed admission column does not
/// fall with this, and what falls is the **executed** count: a run whose keys
/// are all admitted reaches `Value.admitKey` on no turn, which is what a
/// `--boundary` report of `benches/admission` says.
const REFLECTED_FUNCTIONS: [(&str, &str); 3] = [
    ("std.dynamic", "equals"),
    ("std.dynamic", "order"),
    ("std.dynamic", "refusesKey"),
];

/// Why a site calls one of [`BOXED`], read off its first operand's layout.
///
/// Only the first is what ADR 0068 moves. The census found the other two, and
/// they are recorded apart so that nobody reads a large number as a large
/// reflection migration:
///
/// - **boxed**: the operand is `Shape::Boxed`, and the concrete layout is known
///   only when the box is opened — the reflection walk's population;
/// - **scalar**: the operand is a `Float` or a `Duration` word, which
///   `Value.renderInto` rendered because no Cove body wrote those two scalars'
///   text. It is not a dynamic value at all, and a `DynamicView` did not move
///   it: ADR 0068's Phase 4a did, by writing both in Cove, and the column is
///   nought for `Value.renderInto` now;
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
    sites: [[usize; 3]; 2],
    /// Calls per entry of [`REFLECTED_FUNCTIONS`]: `==` on two erased values,
    /// `core.order` over two erased keys and `core.admitKey`'s decision over an
    /// erased key, each from the operation itself or from inside a synthesized
    /// walk that reached a boxed part.
    reflected: [usize; 3],
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

/// ADR 0068's Decision 5, as a fact about one call: a call of any of
/// [`REFLECTED_FUNCTIONS`] whose operand has a layout the lowering knows would be
/// a static layout routed through reflection, which the gate calls a failure —
/// the walk the lowering writes for that layout is the fast path, and this is
/// the fallback. Every operand is asked, so it is the first operand of
/// `std.dynamic.refusesKey`, which has only the one, and both of the other two.
/// Which of them the call is, or `None` for any other call.
fn reflected(
    program: &Program,
    function: &str,
    callee: cove_ir::FunctionId,
    args: cove_ir::ArgsId,
) -> Option<usize> {
    let called = &program.functions[callee.index()];
    let which = REFLECTED_FUNCTIONS
        .iter()
        .position(|named| (&*called.module, &*called.name) == *named)?;
    for arg in program.arg_list(args) {
        let described = program.layout(arg.layout);
        assert!(
            matches!(described.shape, Shape::Boxed),
            "`{function}` calls `{}.{}` over a `{}`, whose layout is known: ADR 0068's \
             Decision 5 keeps a static layout off the reflected path",
            called.module,
            called.name,
            described.name
        );
    }
    Some(which)
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
                if let Some(which) = reflected(program, &name, *callee, *args) {
                    found.reflected[which] += 1;
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
///
/// **ADR 0068's Phase 3 took the `Value.order` row out as well**, and it is the
/// second entry of [`REFLECTED`] now. Measured with the three programs it added
/// held out, over the 222 programs before it, `Value.admitKey`'s boxed column
/// went 31 to **29** and nothing else here moved: `benches/admission` and
/// `benches/ordering` lost one boxed admission site each, which is a static site
/// count and not a run — a standard-library body around a boxed key no longer
/// holds an intrinsic site but a call, so what `lower::inline` expands and where
/// changed. Then the three programs added what they are for:
/// `values_boxed_order` 5, 3 and 6 boxed admission, known admission and boxed
/// rendering sites — its `Set<Holder>` is a known layout with a box in it — and
/// `fail_key_boxed_struct` 1 and 3 and `fail_key_boxed_generic` 2 and 3 boxed
/// admission and rendering sites.
///
/// **ADR 0068's Phase 3b moved `Value.admitKey`'s decision into Cove, and no
/// site moved.** A boxed key is decided by `std.dynamic.refusesKey` now, and the
/// intrinsic stays at every site it was at, under the branch on that answer,
/// because it words the refusal. Measured with the four programs it added held
/// out and `values_boxed_order` as it was, over the same 225 programs, this
/// table is 37, 1 and 115 and 61, 669 and 4 to the site. Then the four added
/// what they are for: `fail_key_boxed_map_value` and `fail_key_boxed_enum` 2
/// and 3 boxed admission and rendering sites each, and `fail_key_boxed_deep_200`
/// and `fail_key_boxed_deep_1000` 1 and 2 each; the `chain` rows added to
/// `values_boxed_order` moved none. What fell is the *executed* count, which a
/// static census cannot see: an admitting run reaches `Value.admitKey` on no
/// turn, where it reached it on every boxed lookup.
///
/// **ADR 0068's Phase 4a emptied `Value.renderInto`'s scalar column, 669 to
/// 0.** A `Float` and a `Duration` are written by `std.float.renderInto` and
/// `std.duration.renderInto` now, called where the intrinsic stood — at a piece
/// of an interpolation, and at a part of a walk the lowering composes, which is
/// why none is left inside a synthesized function either. The boxed and known
/// columns did not move, 71 and 4, and neither did the synthesized count, 596:
/// a walk over a struct holding a `Float` was already a walk, with the
/// intrinsic at the one field. The `duration` row `benches/rendering` gained
/// adds no site. What is left below is what is inside a box, which Phase 4b
/// walks in Cove.
const FALLBACK_SITES: [[usize; 3]; 2] = [[43, 1, 115], [71, 0, 4]];

/// The whole-corpus calls of `std.dynamic.equals`, of `std.dynamic.order` and of
/// `std.dynamic.refusesKey`, which may fall and may never rise: the reflected
/// population, ADR 0068's Phase
/// 2 asks for it reported apart from the specialized one.
///
/// `std.dynamic.equals`, measured when Phase 2 landed, over 215 programs:
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
///   `values_boxed_deep` and `values_boxed_generic`, 7 each;
/// - **8** more from the programs issue #493 added to pin its refusal of a value
///   that contains itself: `fail_equals_cycle_boxed` 1 and `values_equals_shared`
///   7. The lowering moved the count by 0 — 128 with those held out — and the
///   boxed tree row `benches/equals` gained compares through the `countBoxed` it
///   already had. Phase 3 did not move it.
///
/// `std.dynamic.order`, measured when Phase 3 landed, over 225 programs:
///
/// - **24** in the 222 programs that were here before it, against the 55
///   `Value.order` sites they held — Phase 2's finding again, and for its reason:
///   the site sat in a small standard-library search that `lower::inline` expanded
///   into each caller, and was counted once per expansion as well as in the body;
///   a call is not a leaf, so it is counted once. `values_value_admit_key` went 20
///   to 6, `values_value_order` 11 to 4, `values_boxed` 8 to 6, both benches 5 to
///   2 and the two `fail_key_boxed_*` programs 3 to 2;
/// - **8** more from the three programs Phase 3 added: `values_boxed_order` 5,
///   `fail_key_boxed_generic` 2 and `fail_key_boxed_struct` 1;
/// - **6** more from the four programs Phase 3b added, which order what they
///   admit: `fail_key_boxed_map_value` and `fail_key_boxed_enum` 2 each, and
///   `fail_key_boxed_deep_200` and `fail_key_boxed_deep_1000` 1 each. Phase 3b's
///   lowering moved it by 0.
///
/// `std.dynamic.refusesKey`, measured when Phase 3b landed, over 229 programs:
///
/// - **42** in the 225 programs that were here before it, with
///   `values_boxed_order` as it was. That is one call under every one of the
///   37 boxed `Value.admitKey` sites, which is the guard, and **5** from inside
///   the five admission walks the lowering now composes for a known key layout
///   holding a box — a `Holder { tag: Int, item: dyn Summary }` handed the
///   whole key to the runtime until a box became a part a walk can decide, so
///   the synthesized count went 589 to 594 over those programs:
///   `values_boxed_order` 1, `values_value_admit_key` 2 and `values_value_order`
///   2;
/// - **6** more from the four programs Phase 3b added, one under each of their
///   boxed admission sites. The `chain` rows it added to `values_boxed_order`
///   reach the calls that program already held.
const REFLECTED: [usize; 3] = [136, 38, 48];

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
        for at in 0..BOXED.len() {
            for column in 0..3 {
                total.sites[at][column] += found.sites[at][column];
            }
        }
        for at in 0..REFLECTED_FUNCTIONS.len() {
            total.reflected[at] += found.reflected[at];
        }
        total.synthesized += found.synthesized;
        rows.push((case.name.clone(), found));
    }
    println!(
        "{} program(s), {} synthesized function(s), {} reflected call(s) of \
         `std.dynamic.equals`, {} of `std.dynamic.order` and {} of \
         `std.dynamic.refusesKey`; sites as boxed / scalar / known:",
        rows.len(),
        total.synthesized,
        total.reflected[0],
        total.reflected[1],
        total.reflected[2]
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
    println!(
        "\n  reflected equals, order and refusesKey, boxed per operation (admit render), then \
         synth, program"
    );
    for (name, found) in &rows {
        let boxed = [
            found.reflected[0],
            found.reflected[1],
            found.reflected[2],
            found.sites[0][0],
            found.sites[1][0],
        ];
        if boxed.iter().any(|n| *n > 0) {
            println!(
                "  {:>4} {:>4} {:>4} {:>4} {:>4} {:>5}  {name}",
                boxed[0], boxed[1], boxed[2], boxed[3], boxed[4], found.synthesized
            );
        }
    }
    for (at, (module, function)) in REFLECTED_FUNCTIONS.iter().enumerate() {
        assert!(
            total.reflected[at] <= REFLECTED[at],
            "the corpus has {} call(s) of `{module}.{function}`, and the ratchet is {}. It may \
             fall and never rise: ADR 0068's gate is that a fallback count does not increase.",
            total.reflected[at],
            REFLECTED[at]
        );
    }
    let columns = ["boxed", "scalar", "known"];
    for at in 0..BOXED.len() {
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
