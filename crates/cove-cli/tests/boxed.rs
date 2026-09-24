//! How much of the repository still reaches a boxed-value fallback, counted.
//!
//! [ADR 0068](../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)
//! moved the last four `Intrinsic` variants — `Any.equals`, `Value.order`,
//! `Value.admitKey` and `Value.renderInto` — into Cove over a structural view of
//! a type-erased value. Each was reached only where its operand's layout is
//! `Shape::Boxed`: a `dyn Trait`, or a Host schema's `Any`, whose concrete layout
//! is known only when the box is opened. Everywhere a layout is known, ADR 0064's
//! Decision 3 has written a walk the lowering composes.
//!
//! Its Phase 0 asks for a census of both populations, and its gates make one of
//! them a ratchet: *"an increase in its fallback count is a failed gate."* So this
//! file lowers every program the repository keeps — the corpus `copies.rs` and
//! `bytecode_corpus.rs` walk — and counts, per program:
//!
//! - **the reflected population**: calls of `std.dynamic.equals`, which is what
//!   `Any.equals` became in Phase 2, of `std.dynamic.order`, which is what
//!   `Value.order` became in Phase 3, of `std.dynamic.refusesKey`, which is what
//!   `Value.admitKey`'s *decision* became in Phase 3, of
//!   `std.dynamic.renderInto`, which is what `Value.renderInto` became in Phase
//!   4b-ii, and of `std.dynamic.refuseKey`, which is what `Value.admitKey`'s
//!   *wording* became in Phase 4c — the fallback answered in Cove, over a
//!   `DynamicView` of each box. Ratcheted, because it *is* the fallback, and held
//!   to Decision 5: every value operand of every call is erased;
//! - **the wording walks**: the functions the lowering synthesized to word the
//!   refusal of a key whose layout is known, `describes<L>` and `describesAt<L>`
//!   (Phase 4c), which run only on the path that ends the run. Ratcheted too,
//!   because each is one more function a program carries for a refusal, and a
//!   lowering that made one where no key can be refused would be paying for text
//!   nothing can print;
//! - **the specialized population**: every function the lowering synthesized
//!   (module `<synth>`), which is ADR 0064's Decision 3 working. Reported and not
//!   ratcheted — a program that compares more kinds of value legitimately has more
//!   of them — so that the reflected and the specialized can be read side by side,
//!   which ADR 0068's Phase 2 asks for.
//!
//! **The fallback itself is not counted any more, because there is none.** Until
//! Phase 4c this file counted the `IntrinsicCall` sites of each variant still in
//! Rust, and the last row, `Value.admitKey`'s, went with the variant: the
//! variant set is `cove_ir::intrinsic`'s to hold, and it holds no rule over a
//! value's layout at all now. The history of that row is below, where the
//! constant stood.
//!
//! Static counts are not executed counts: a site in a function nothing calls is
//! counted here and never runs. The executed side is a run's `--boundary`
//! report, and the two are recorded together in the pull request that lands a
//! phase.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_ir::layout::Shape;
use cove_ir::{Inst, Program};
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

/// The Cove functions the moved operations became, in [`Counts::reflected`]'s
/// order: what `==` answers two erased values with since ADR 0068's Phase 2,
/// what `core.order` answers two erased keys with since its Phase 3, what
/// `core.admitKey` decides an erased key with since that phase too, the text
/// of an erased value since Phase 4b-ii, and the refusal of an erased key since
/// Phase 4c.
///
/// The third and the fifth are the two halves of one operation. `core.admitKey`
/// over a box calls `std.dynamic.refusesKey`, and `std.dynamic.refuseKey` only
/// under the branch on its answer: a run whose keys are all admitted reaches
/// the second on no turn, which is what a `--boundary` report of
/// `benches/admission` says. A wording walk composed for a known key layout
/// calls both where the part it names is a box.
///
/// The third field is how many of the leading operands are the *values* the
/// call is about, which Decision 5 holds to a box: every operand of the first
/// three; the first of `renderInto`, whose other two are the buffer it appends
/// to and the path it is handed — a handle and a capability, not values of
/// anything; and the first of `refuseKey`, whose others are the two names, the
/// path to the box and the render path a quoted key renders under.
const REFLECTED_FUNCTIONS: [(&str, &str, usize); 5] = [
    ("std.dynamic", "equals", 2),
    ("std.dynamic", "order", 2),
    ("std.dynamic", "refusesKey", 1),
    ("std.dynamic", "renderInto", 1),
    ("std.dynamic", "refuseKey", 1),
];

/// One program's counts.
#[derive(Clone, Copy, Default)]
struct Counts {
    /// Calls per entry of [`REFLECTED_FUNCTIONS`]: `==` on two erased values,
    /// `core.order` over two erased keys, `core.admitKey`'s decision over an
    /// erased key, the text of an erased value and the refusal of an erased
    /// key, each from the operation itself or from inside a synthesized walk
    /// that reached a boxed part.
    reflected: [usize; 5],
    /// The wording walks the lowering synthesized: `describes<L>` and
    /// `describesAt<L>`.
    wording: usize,
    /// Functions the lowering synthesized.
    synthesized: usize,
}

/// ADR 0068's Decision 5, as a fact about one call: a call of any of
/// [`REFLECTED_FUNCTIONS`] whose operand has a layout the lowering knows would be
/// a static layout routed through reflection, which the gate calls a failure —
/// the walk the lowering writes for that layout is the fast path, and this is
/// the fallback. Every value operand is asked — the third field of
/// [`REFLECTED_FUNCTIONS`] says how many that is — so it is the first operand
/// of `std.dynamic.refusesKey` and of `std.dynamic.refuseKey`, both of
/// `equals` and `order`, and the value `std.dynamic.renderInto` renders. Which
/// of them the call is, or `None` for any other call.
fn reflected(
    program: &Program,
    function: &str,
    callee: cove_ir::FunctionId,
    args: cove_ir::ArgsId,
) -> Option<usize> {
    let called = &program.functions[callee.index()];
    let which = REFLECTED_FUNCTIONS
        .iter()
        .position(|(module, name, _)| (&*called.module, &*called.name) == (*module, *name))?;
    let values = REFLECTED_FUNCTIONS[which].2;
    for arg in program.arg_list(args).iter().take(values) {
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
            if function.name.starts_with("describes<") || function.name.starts_with("describesAt<")
            {
                found.wording += 1;
            }
        }
        let name = format!("{}.{}", function.module, function.name);
        for inst in &function.code {
            if let Inst::Call { callee, args, .. } = inst {
                if let Some(which) = reflected(program, &name, *callee, *args) {
                    found.reflected[which] += 1;
                }
            }
        }
    }
    found
}

// ---- the fallback sites, retired ------------------------------------------
//
// `FALLBACK_SITES` stood here: the whole-corpus `IntrinsicCall` sites of each
// variant ADR 0068 moves, per why the site reached it — boxed, scalar or
// known. It was `[[43, 1, 115]]` for `Value.admitKey` when the ADR's Phase 4c
// deleted that variant, the last, and with it the row: the 43 boxed sites
// are calls of `std.dynamic.refuseKey` under their branch, the 115 known ones
// calls of a `describes<L>` walk or nothing at all, and the one scalar site
// is `fail_key_float_empty_map`'s `Float` key, which is a literal sentence
// and a trap at the site. What its doc said, as it stood:
//
// The whole-corpus sites per operation and per [`Why`], which may fall and may
// never rise. Recorded for ADR 0068's Phase 0, before any of the four has moved,
// over 210 programs and 544 synthesized functions:
//
// | | boxed | scalar | known |
// | --- | ---: | ---: | ---: |
// | `Any.equals` | 20 | 0 | 0 |
// | `Value.order` | 41 | 0 | 0 |
// | `Value.admitKey` | 17 | 1 | 112 |
// | `Value.renderInto` | 29 | 669 | 4 |
//
// **Two of the columns are not reflection's to empty**, and both are larger than
// the column that is. `Value.renderInto`'s 669 scalar sites are `Float` and
// `Duration` interpolations — a `Float`'s shortest round-trip text is an
// algorithm of its own, and no `DynamicView` writes it — so deleting that variant
// needs those two renderings in Cove as well. `Value.admitKey`'s 112 known sites
// are issue #476's shape, a synthesized walk deciding and the intrinsic staying
// only to word the refusal, which it did because a trap carried one string; ADR
// 0067 lifted that, so they can move to `core.refuse` independently.
//
// Every boxed site is in a corpus or a benchmark written to reach it: no example
// program has one.
//
// **ADR 0068's Phase 2 took the first row out of this table**, and it is
// [`REFLECTED`] now: `Any.equals` is `std.dynamic.equals`, a call rather than an
// intrinsic site, so it has no scalar or known column — Decision 5 is asserted
// of every call instead.
//
// **Then Phase 0 added the corpus that pins the boxed contract**, and the boxed
// column rose by exactly what those three programs are for — measured with them
// held out, where it is the table above to the site: `tests/e2e/values_boxed`
// 104, 8, 10 and 14 boxed sites, and `fail_key_boxed_float` and
// `fail_key_boxed_vector` 0, 3, 2 and 3 each. The scalar and known columns did
// not move. A corpus written to reach a fallback raises its count by the rows it
// has; what this ratchet is for is that nothing *else* does.
//
// **ADR 0068's Phase 3 took the `Value.order` row out as well**, and it is the
// second entry of [`REFLECTED`] now. Measured with the three programs it added
// held out, over the 222 programs before it, `Value.admitKey`'s boxed column
// went 31 to **29** and nothing else here moved: `benches/admission` and
// `benches/ordering` lost one boxed admission site each, which is a static site
// count and not a run — a standard-library body around a boxed key no longer
// holds an intrinsic site but a call, so what `lower::inline` expands and where
// changed. Then the three programs added what they are for:
// `values_boxed_order` 5, 3 and 6 boxed admission, known admission and boxed
// rendering sites — its `Set<Holder>` is a known layout with a box in it — and
// `fail_key_boxed_struct` 1 and 3 and `fail_key_boxed_generic` 2 and 3 boxed
// admission and rendering sites.
//
// **ADR 0068's Phase 3b moved `Value.admitKey`'s decision into Cove, and no
// site moved.** A boxed key is decided by `std.dynamic.refusesKey` now, and the
// intrinsic stays at every site it was at, under the branch on that answer,
// because it words the refusal. Measured with the four programs it added held
// out and `values_boxed_order` as it was, over the same 225 programs, this
// table is 37, 1 and 115 and 61, 669 and 4 to the site. Then the four added
// what they are for: `fail_key_boxed_map_value` and `fail_key_boxed_enum` 2
// and 3 boxed admission and rendering sites each, and `fail_key_boxed_deep_200`
// and `fail_key_boxed_deep_1000` 1 and 2 each; the `chain` rows added to
// `values_boxed_order` moved none. What fell is the *executed* count, which a
// static census cannot see: an admitting run reaches `Value.admitKey` on no
// turn, where it reached it on every boxed lookup.
//
// **ADR 0068's Phase 4a emptied `Value.renderInto`'s scalar column, 669 to
// 0.** A `Float` and a `Duration` are written by `std.float.renderInto` and
// `std.duration.renderInto` now, called where the intrinsic stood — at a piece
// of an interpolation, and at a part of a walk the lowering composes, which is
// why none is left inside a synthesized function either. The boxed and known
// columns did not move, 71 and 4, and neither did the synthesized count, 596:
// a walk over a struct holding a `Float` was already a walk, with the
// intrinsic at the one field. The `duration` row `benches/rendering` gained
// adds no site. What is left below is what is inside a box, which Phase 4b
// walks in Cove.
//
// **ADR 0068's Phase 4b-i took `Value.renderInto`'s boxed column from 71 to
// 70, and the lowering took it to 65.** Measured with the three programs it
// added held out and `benches/rendering` as it was, over the same 229
// programs, the column is 65 and the synthesized count 612: a layout whose
// rendering reaches a box now carries the path of vectors it is inside to the
// box (Phase 4b-ii's renderer reads it there), so its `renders<L>` is a
// wrapper around a tracked walk — sixteen more functions — and the wrapper is
// a call `lower::inline` does not expand, where the walk it replaced was
// expanded into its callers with the intrinsic site in it:
// `values_value_admit_key` holds four boxed rendering sites fewer and
// `values_value_order` two. Then `values_render_deep` added 2 and
// `values_render_opaque` 3, which is what they are for. The known column did
// not move: a host resource and a task scope still refuse on a static walk,
// because their text is in the run's tables and handing their layouts to the
// intrinsic would raise it (issue #499).
//
// **ADR 0068's Phase 4b-ii took the `Value.renderInto` row out**, the third
// to leave, and it is the fourth entry of [`REFLECTED`] now. Its four known
// sites were a function value, whose location is one bare reference word:
// `<fn>` whichever function it names, which a walk writes from that layout
// alone, so none of them reaches reflection.

/// The whole-corpus calls of each of [`REFLECTED_FUNCTIONS`], which may fall and
/// may never rise: the reflected population, which ADR 0068's Phase 2 asks for
/// reported apart from the specialized one.
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
///
/// `std.dynamic.renderInto`, measured when Phase 4b-ii landed, over 233
/// programs:
///
/// - **53** in the 232 programs that were here before it, with
///   `values_render_cycle` and `values_render_opaque` as they were, against
///   the 70 boxed `Value.renderInto` sites they held. Phase 2's finding once
///   more: a site in a small body `lower::inline` expanded into each caller was
///   counted once per expansion and once in the body, and a call of a walk as
///   large as this one is not expanded, so it is counted once. Fourteen
///   programs hold one or two fewer — `values_boxed` 14 to 12, every
///   `fail_key_boxed_*` 3 to 2 or 2 to 1 — and none holds more;
/// - **28** more from what Phase 4b-ii added to pin it: `values_boxed_render`,
///   every kind a box can hold, and the `box.*` rows of `values_render_cycle`
///   and the boxed rows of `values_render_opaque`. The synthesized count went
///   646 to 648 with those held out — the two walks of a function value's
///   location, which were intrinsic sites — and to 654 with them.
///
/// ADR 0068's Phase 4c, measured when it landed, over 239 programs:
///
/// - `std.dynamic.refuseKey` is new, and it is **48** in the 233 programs that
///   were here before it: one call under every one of the 43 boxed
///   `Value.admitKey` sites the intrinsic's last row held, which are its
///   successor's now, and **5** from inside the wording walks the lowering
///   composes for a known key layout holding a box, where the part they name
///   is the box — `values_boxed_order` 1, `values_value_admit_key` 2 and
///   `values_value_order` 2, the programs whose admission walks call
///   `refusesKey` for the same reason;
/// - `std.dynamic.refusesKey` went **48 to 53** with the lowering, and the
///   five are those same wording walks deciding, at the box, whether the box is
///   the part to name. They are on the path that ends the run: a key the
///   admission walk admits never reaches a wording walk, so none of the five
///   runs on a turn that admits;
/// - `std.dynamic.order` and `std.dynamic.renderInto` did not move with the
///   lowering, 38 and 81;
/// - then the six programs Phase 4c added to pin it raised the columns by what
///   they hold: `fail_key_boxed_map_key` 2 in each of the order, the decision,
///   the rendering and the wording, and the five `fail_key_known_*` programs
///   none, because their keys are known layouts.
const REFLECTED: [usize; 5] = [136, 40, 55, 83, 50];

/// The wording walks the whole corpus carries, `describes<L>` and
/// `describesAt<L>`, which may fall and may never rise: ADR 0068's Phase 4c.
///
/// Measured when ADR 0068's Phase 4c landed, over 239 programs:
///
/// - **19** in the 233 programs that were here before it, one `describes<L>`
///   for each known key layout a site can refuse, in seven programs —
///   `values_value_admit_key` 9, `benches/admission` 4, `values_value_order`
///   2, and `fail_key_enum_case`, `fail_key_map_value`,
///   `fail_key_nested_float` and `values_boxed_order` 1 each — and no
///   `describesAt<L>`, because no program before it refused a key whose layout
///   holds itself. The synthesized count went 654 to 673, the 19 exactly: the
///   admission walks are what they were, and a layout that holds itself and
///   every part of which is a key, `benches/admission`'s `Node`, is asked
///   nothing at all now, where it was handed to the runtime whole;
/// - **6** more from the five `fail_key_known_*` programs Phase 4c added, one
///   `describes<L>` each and a `describesAt<Tree>` for the one whose key holds
///   itself, `fail_key_known_deep`. The synthesized count went 673 to 712
///   with the six programs, and those six are of the 39.
const WORDING_WALKS: usize = 25;

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
        for at in 0..REFLECTED_FUNCTIONS.len() {
            total.reflected[at] += found.reflected[at];
        }
        total.wording += found.wording;
        total.synthesized += found.synthesized;
        rows.push((case.name.clone(), found));
    }
    println!(
        "{} program(s), {} synthesized function(s) of which {} word a refusal, {} reflected \
         call(s) of `std.dynamic.equals`, {} of `std.dynamic.order`, {} of \
         `std.dynamic.refusesKey`, {} of `std.dynamic.renderInto` and {} of \
         `std.dynamic.refuseKey`",
        rows.len(),
        total.synthesized,
        total.wording,
        total.reflected[0],
        total.reflected[1],
        total.reflected[2],
        total.reflected[3],
        total.reflected[4]
    );
    println!(
        "\n  reflected equals, order, refusesKey, renderInto and refuseKey, wording walks, \
         then synth, program"
    );
    for (name, found) in &rows {
        let shown = [
            found.reflected[0],
            found.reflected[1],
            found.reflected[2],
            found.reflected[3],
            found.reflected[4],
            found.wording,
        ];
        if shown.iter().any(|n| *n > 0) {
            println!(
                "  {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>5}  {name}",
                shown[0], shown[1], shown[2], shown[3], shown[4], shown[5], found.synthesized
            );
        }
    }
    for (at, (module, function, _)) in REFLECTED_FUNCTIONS.iter().enumerate() {
        assert!(
            total.reflected[at] <= REFLECTED[at],
            "the corpus has {} call(s) of `{module}.{function}`, and the ratchet is {}. It may \
             fall and never rise: ADR 0068's gate is that a fallback count does not increase.",
            total.reflected[at],
            REFLECTED[at]
        );
    }
    assert!(
        total.wording <= WORDING_WALKS,
        "the corpus has {} wording walk(s), and the ratchet is {WORDING_WALKS}. It may fall \
         and never rise: a wording walk is made only where a key can be refused.",
        total.wording
    );
}
