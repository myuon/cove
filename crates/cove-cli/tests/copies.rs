//! How much of the corpus is a value being moved from where it was made to
//! where it belongs.
//!
//! [Issue #302](https://github.com/myuon/cove/issues/302) asks the lowering to
//! stop materialising an expression into a temporary and then copying it into
//! the location that owns the answer:
//!
//! ```text
//! call s4:String playground.greeting (s3:String)
//! copy s1:String s4:String
//! return s1:String
//! ```
//!
//! It also says, first, to **measure the current shape**, and this is that
//! measurement. It walks the same corpus `bytecode_corpus.rs` and
//! `vm_coverage.rs` walk, lowers each program, and counts the shapes a
//! destination-forwarding lowering would remove. Nothing here runs a program,
//! so it is not `#[ignore]`d: the whole survey is a fraction of a second.
//!
//! # What is counted, and why each one
//!
//! A `copy` is not waste. ADR 0001's value semantics are a copy, and a
//! binding assigned from another binding has to make one. What issue #302 is
//! about is the copies that exist only because the producer did not know
//! where its answer was wanted, and those have a shape:
//!
//! - **after a producer** — the instruction before the `copy` wrote exactly
//!   the run the `copy` reads. This is the forwarding candidate: had the
//!   producer been handed the destination, there would be no second
//!   instruction at all. It is counted by *width*, not by slot alone,
//!   because a producer that wrote one word of a two-word run is not the
//!   producer of that value.
//!
//!   It is an **upper bound** and not a promise. Whether a given producer may
//!   be handed a given destination also depends on what is live in that
//!   destination while the producer runs, and on whether the destination is
//!   one of the producer's own operands. Deciding that is stage 2's work;
//!   what this number says is how much there is to decide about.
//! - **and then cleared** — of those, the ones whose source run is zeroed
//!   immediately after. That clear exists to release what the copy left
//!   behind, so forwarding removes two instructions rather than one, and
//!   this is the subset where the source is provably not read again.
//! - **before a `return`** — the `copy` writes the location the function
//!   answers and nothing but clears stands between it and the `Return`. The
//!   answer location is decided before the body is lowered, so this is the
//!   case where the destination was known *earliest* and is the one issue
//!   #302 opens with.
//!
//! `frame words` is the fourth number, because a temporary that is never
//! made is a run the frame does not need — but a narrower frame and fewer
//! instructions are different benefits and are reported apart.
//!
//! # The ratchet
//!
//! The totals are asserted against a bound, and the bound may fall and never
//! rise. That is the whole mechanism by which issue #302's stages are
//! measurable: a stage that removes copies lowers the number here, and a
//! change that quietly adds one fails instead of passing.
//!
//! Static counts are not executed counts, and on this corpus the two say very
//! different things. Statically 31% of copies are at one of the shapes above;
//! *executed*, on `benches:callback` it is 97% and on `examples:life` 75%,
//! because the copies in a loop body are exactly the call-answer and
//! block-answer ones. This file cannot tell a hot copy from a cold one — a
//! run is where that is read — so both numbers belong in the issue's
//! before-and-after and neither stands alone.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_ir::layout::LayoutId;
use cove_ir::{Inst, Program, Slot};
use cove_sema::HostSchemas;

// Discovery, parsing and type-checking. What a *run* is belongs to the two
// surveys that run something; nothing here runs a program.
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use support::{Case, ModuleIndex, Prepared};

/// Every entry point of the repository, in a fixed order: the same set
/// `bytecode_corpus.rs` walks, for the same reason.
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

/// What one program, or the whole corpus, is made of.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    functions: usize,
    instructions: usize,
    frame_words: usize,
    copies: usize,
    clears: usize,
    /// A `copy` whose source run was written by the instruction before it.
    after_producer: usize,
    /// Of those, the ones whose source run is cleared immediately after.
    after_producer_and_cleared: usize,
    /// A `copy` into the location a `Return` names, with only clears between.
    before_return: usize,
    /// A `copy` that is either of the two, counted once.
    either: usize,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.functions += other.functions;
        self.instructions += other.instructions;
        self.frame_words += other.frame_words;
        self.copies += other.copies;
        self.clears += other.clears;
        self.after_producer += other.after_producer;
        self.after_producer_and_cleared += other.after_producer_and_cleared;
        self.before_return += other.before_return;
        self.either += other.either;
    }

    /// The copies issue #302's stage 2 is aimed at: the union of the two
    /// shapes, counted once each. A `copy` is often both — a call's answer
    /// moved into the location the `return` names is the issue's own opening
    /// example — so adding the two would count those twice and taking the
    /// larger would lose the ones only the other found.
    fn forwardable(&self) -> usize {
        self.either
    }
}

/// The value location an instruction writes: its base slot and how many words
/// it covers, or `None` where it writes no value.
///
/// This is the same question `crate::verify`'s `fits` asks of a destination,
/// answered in words rather than in layouts so that a one-word producer — an
/// `int`, an `add`, an `alloc` — is comparable with a `copy` of a one-word
/// layout. A producer that wrote *some* of a run is not the producer of the
/// value in it, so the width has to match and not only the slot.
fn wrote(program: &Program, inst: &Inst) -> Option<(Slot, u32)> {
    let width = |layout: LayoutId| {
        program
            .layouts
            .get(layout.index())
            .map_or(1, |held| held.width())
    };
    let one = |slot: Slot| Some((slot, 1));
    match *inst {
        // A value location, at a layout the instruction or a declaration
        // names. Each of these is a `fits` in the verifier.
        Inst::Copy { dst, layout, .. }
        | Inst::Load { dst, layout, .. }
        | Inst::LoadField { dst, layout, .. }
        | Inst::LoadElem { dst, layout, .. }
        | Inst::Unbox { dst, layout, .. } => Some((dst, width(layout))),
        Inst::Await { dst, answer, .. } => Some((dst, width(answer))),
        Inst::CallClosure { dst, result, .. } => Some((dst, width(result))),
        Inst::Call { dst, callee, .. } => Some((
            dst,
            program
                .functions
                .get(callee.index())
                .map_or(1, |target| width(target.returns)),
        )),
        Inst::CallHost { dst, op, .. } | Inst::CallResource { dst, op, .. } => Some((
            dst,
            program
                .host_ops
                .get(op.index())
                .map_or(1, |op| width(op.result)),
        )),
        Inst::IntrinsicCall { dst, site, .. } => Some((
            dst,
            program
                .intrinsic_sites
                .get(site.index())
                .map_or(1, |held| width(held.result)),
        )),
        // One word, whatever it holds.
        Inst::Unit { dst }
        | Inst::Bool { dst, .. }
        | Inst::Int { dst, .. }
        | Inst::Float { dst, .. }
        | Inst::Str { dst, .. }
        | Inst::Tag { dst, .. }
        | Inst::FuncRef { dst, .. }
        | Inst::Neg { dst, .. }
        | Inst::Arith { dst, .. }
        | Inst::Cmp { dst, .. }
        | Inst::ArithImm { dst, .. }
        | Inst::CmpImm { dst, .. }
        | Inst::Not { dst, .. }
        | Inst::Convert { dst, .. }
        | Inst::Alloc { dst, .. }
        | Inst::Box { dst, .. }
        | Inst::Len { dst, .. }
        | Inst::LayoutOf { dst, .. }
        | Inst::AddrOfSlot { dst, .. }
        | Inst::AddrOfField { dst, .. }
        | Inst::AddrOfElem { dst, .. }
        | Inst::AddrOfPart { dst, .. }
        | Inst::ScopeEnter { dst, .. }
        | Inst::Spawn { dst, .. }
        | Inst::Settled { dst, .. } => one(dst),
        // Writes nothing into a frame location of its own.
        _ => None,
    }
}

/// Whether `inst` zeroes exactly the run `(slot, words)`.
fn clears(program: &Program, inst: &Inst, slot: Slot, words: u32) -> bool {
    match *inst {
        Inst::Clear {
            slot: zeroed,
            layout,
        } => {
            zeroed == slot
                && program
                    .layouts
                    .get(layout.index())
                    .map_or(1, |held| held.width())
                    == words
        }
        _ => false,
    }
}

fn count(program: &Program) -> Counts {
    let mut found = Counts {
        functions: program.functions.len(),
        ..Counts::default()
    };
    for function in &program.functions {
        found.instructions += function.code.len();
        found.frame_words += function.reprs.len();
        for (pc, inst) in function.code.iter().enumerate() {
            let (dst, src, layout) = match *inst {
                Inst::Copy { dst, src, layout } => (dst, src, layout),
                Inst::Clear { .. } => {
                    found.clears += 1;
                    continue;
                }
                _ => continue,
            };
            found.copies += 1;
            let mut either = false;
            let words = program
                .layouts
                .get(layout.index())
                .map_or(1, |held| held.width());

            if pc > 0 && wrote(program, &function.code[pc - 1]) == Some((src, words)) {
                found.after_producer += 1;
                either = true;
                if function
                    .code
                    .get(pc + 1)
                    .is_some_and(|next| clears(program, next, src, words))
                {
                    found.after_producer_and_cleared += 1;
                }
            }

            // Only clears may stand between the copy and the return: a clear
            // reads nothing, so nothing between the two can observe either
            // run. Anything else ends the look-ahead — see `lower::tails`,
            // which drops the clears in exactly this position for the same
            // reason.
            let mut at = pc + 1;
            while matches!(function.code.get(at), Some(Inst::Clear { .. })) {
                at += 1;
            }
            if matches!(function.code.get(at), Some(Inst::Return { src: answer }) if *answer == dst)
            {
                found.before_return += 1;
                either = true;
            }
            found.either += usize::from(either);
        }
    }
    found
}

/// The whole corpus, lowered and counted, one row per program.
fn survey() -> (Counts, Vec<(String, Counts)>) {
    let mut total = Counts::default();
    let mut rows: Vec<(String, Counts)> = Vec::new();
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));
        // A package that does not check has no program in it, and a gap in
        // the lowering is `vm_coverage.rs`'s finding rather than this one's.
        let Ok(prepared) = Prepared::of(&case, index) else {
            continue;
        };
        let Ok(program) = cove_ir::lower(&prepared.checked, &prepared.sources, &HostSchemas::new())
        else {
            continue;
        };
        let found = count(&program);
        total.add(&found);
        rows.push((case.name.clone(), found));
    }
    rows.sort_by(|a, b| {
        b.1.forwardable()
            .cmp(&a.1.forwardable())
            .then(a.0.cmp(&b.0))
    });
    (total, rows)
}

/// The corpus-wide bound issue #302's stages move.
///
/// It may fall and it may never rise. A change that removes copies lowers it;
/// a change that quietly adds one fails here instead of passing, which is the
/// only thing that makes "before and after" mean anything across several
/// changes by several hands.
///
/// 4319 is what the lowering emitted when this file was written, out of 13786
/// copies and 71041 instructions: 31% of the copies and 6% of the whole
/// corpus. Issue #302's stage 2 — destination forwarding — took it to 1539,
/// out of 10500 copies and 67441 instructions, and forwarding the destination
/// of a short circuit as well took it to 1492.
///
/// **It has risen twice, and that is the one direction this is not supposed
/// to move.** Both times it was `lower::inline`, which expands a call to a
/// small leaf where it is made — first by 21 and then, once the expansion
/// could carry a call site and stopped refusing bodies that fail, to 1713.
///
/// What rose is not a copy the pass *introduced*. It is a copy the pass made
/// **visible**. A call's answer was always copied from the callee's frame
/// into the caller's destination; before an expansion that copy was performed
/// by `Inst::Return` and by the machine's call protocol, and this survey
/// counts `Inst::Copy`, so it never saw one. An expanded `Return` writes the
/// same run of the same width with an instruction that has a name, and the
/// survey counts it. The same is true of an argument copied into a parameter
/// the body assigns: the call was already going to copy it into a fresh
/// frame.
///
/// So the work did not grow — `examples:life` runs 232,724 instructions
/// against 264,309, and `covefmt`'s lexer runs in 106 ms against 136 ms —
/// and neither did the opportunity, exactly: what these 200 are is a real
/// forwarding candidate that used to be hidden behind a call boundary where
/// no lowering could have taken it. Now it is in the open and stage 3 can.
///
/// It is written down rather than smoothed over: a ratchet raised without a
/// sentence is a ratchet worth nothing, and the sentence is that **1492 is
/// still the number destination forwarding is measured against**.
///
/// **The third rise was the corpus, not the lowering,** and that is a fourth
/// thing this number cannot tell apart by itself. Two programs joined the
/// repository — `examples/covefmt/bench.cove` and `benches/builtincall` —
/// and a program that is in the repository is in this survey, because "every
/// program here" is what makes the survey worth reading. `covefmtBench`
/// alone brought 138 of the 145: it is 4215 instructions holding 559 copies,
/// which is a high rate because timing three phases of a pipeline means
/// holding each phase's answer and handing it on.
///
/// So a corpus ratchet moves when the corpus moves, and nothing in the number
/// says which happened. What says it is the per-program table this test
/// prints: a rise that is one new row is not a rise in what the lowering
/// emits, and the way to check is to look for the rows rather than to argue
/// about the total.
///
/// **The fourth rise was a row growing rather than appearing,** and so was the
/// fifth. 1854 to 1862 to 1868, and the table put all of both in
/// `examples:covefmtBench`: 10310 instructions, then 11195 as covefmt learned
/// to break an expression at its operator and a chain before its dots, then
/// 12152 as it learned how much space goes between two tokens. The same
/// reading applies to a row that grows as to one that appears — more Cove in
/// the repository is more copies in the survey, and it says nothing about what
/// the lowering does with the Cove that was already here.
///
/// **The sixth rise is the first one the lowering caused**, and it is worth
/// separating from the five above it because they were all the corpus moving
/// and this one is not. Expanding a call to a small leaf where a loop reaches
/// it copies the callee's body into every site it expands, and a body holding
/// a producer-then-copy holds one per copy of it. 1867 to 2096, and every one
/// of the 229 is in the `prod` column with the `ret` column unmoved at 1326:
///
/// | | producer copies, before | after |
/// |---|---:|---:|
/// | `examples:cq`, `examples:cqSample` | 103 each | 165 |
/// | `examples:covecheck` | 102 | 156 |
/// | `examples:covefmtBench` | 45 | 71 |
/// | `examples:life` | 32 | 52 |
///
/// No new *kind* of copy appeared — the same shapes are at more sites. And
/// this survey counts the instructions a program **holds**, not the ones it
/// runs: the expansion removed a frame, an argument copy and a return copy at
/// each of those sites, and `cove fmt --check` over this repository went from
/// 905 ms to 864. A ratchet on static copies is the right ratchet for issue
/// #302, which is about what forwarding could remove, and it is the wrong one
/// for asking whether a run got cheaper.
///
/// **The seventh rise is the corpus again, and in the one place where one row
/// is every row.** 2096 to 2231, and 132 of the 139 are one function:
/// `std.stringbuilder`'s `withCapacity`, added with ADR 0052's byte builder.
///
/// A standard-library function is in *every* program's survey, because
/// [`cove_ir::lower`] lowers a whole package and the standard library is
/// attached to every package here. So the per-program table does not show this
/// rise as a row — it shows it as a little over one copy added to each of 132
/// rows, which is the shape to look for when a rise is not where the new source
/// is. The remaining 7 are the one new row, `tests/e2e:values_string_builder`.
///
/// What that one function holds is a struct initializer written as a body's
/// answer:
///
/// ```text
/// growable-alloc.bytes s2 s0
/// copy s1:StringBuilder s2:ByteBuffer
/// return s1
/// ```
///
/// `Body::struct_literal` builds every initializer in a temporary of its own
/// and copies the fields into it, because a destination that is also one of the
/// field operands would be written before it was read. Deciding when it is not
/// is exactly stage 3's work, and a one-field wrapper over a reference — which
/// is what ADR 0052 says every collection wrapper will be — is the case where
/// the copy is the whole of the function. So this 132 is not a copy the
/// lowering started making; it is 132 sites of a candidate it has always had,
/// in a function small enough that nothing else in it hides the shape.
///
/// The other five methods of the builder cost nothing here, and that was worth
/// checking rather than assuming: an append answers `()`, and a `Unit` built in
/// a temporary and copied into the answer would have been a fourth row per
/// program. `Body::unit_answer` is why they are not — see
/// `cove_ir::lower::core`, where the byte builder's appends are lowered now.
///
/// **The eighth rise is one new row, and it is at the floor.** 2231 to 2235,
/// all four of them `tests/e2e:fail_stringbuilder_byte_range`, the end-to-end
/// case for ADR 0058's blame through a library body the inliner will not
/// expand. Its `prod` and `ret` columns are the ones `fail_divide_by_zero` and
/// `fail_nested_divide_by_zero` already have, and those are as small as a
/// failing case gets. So it adds no shape the corpus
/// did not have; it adds one more program carrying the shapes every program
/// here carries, the standard library's among them, which is what any new row
/// costs.
///
/// **The ninth rise is one copy, and it is the inliner charging less.** 2235 to
/// 2236, in `examples:life`. `lower::inline`'s frame budget used to charge a
/// callee its whole frame, including the leading parameter words an expansion
/// reads where the caller already has them and never appends; charged by the
/// words it actually appends, three examples now fit expansions the budget
/// used to refuse — `reviewPolicy`, `life` and `covecheck` grow by 22, 26 and
/// 16 instructions — and the code `life` gained holds one more copy after a
/// producer: a candidate the expanded body carried as a function, now counted
/// in its caller. Measured apart: the same change with the
/// thin-library rule switched off moves the survey identically, so the rule
/// that makes a small standard-library wrapper a mandatory expansion adds
/// nothing here.
///
/// **The tenth rise is two new rows, and neither is the lowering.** 2236 to
/// 2251: `tests/e2e:coll_vector_edges` holds 5 and `tests/e2e:gc_vector_grow`
/// holds 10, measured one without the other. They are the fixtures that pin a
/// `Vector`'s index edges, its aliasing across growth and its `freeze` through
/// collections before ADR 0058 moves `push`, `set` and `freeze` into the
/// standard library (#378), so they land a commit ahead of the move and the
/// lowering is the base's.
///
/// **The eleventh rise is `Vector.push` moving into the standard library.**
/// 2251 to 2256, all five in the `ret` column: `examples:callbacks` 2,
/// `examples:covefmtBench`, `examples:values` and `tests/e2e:values_unit` 1
/// each. Each is a function whose last expression is a `push` —
/// `Router.get`, `BookingDraft.addGuest`, `record` — and so answers the push's
/// `()`. The builtin wrote that unit straight into the answer; the push is now
/// `std.vector.push` expanded where it is called, whose `unit` is written in
/// the expansion's own slot and copied out by its `return`, because the
/// expansion's answer forwarding declines a destination the arguments are
/// read around. It is one static copy of a `Unit` per such function, and not
/// per push: a push in statement position writes its unit and nothing else
/// (`lower::tests::methods`'s listing of one).
///
/// **The twelfth rise is `Vector.set` following it.** 2256 to 2264. The
/// builtin answered its `Option` straight into the call's destination;
/// `std.vector.set<T>` is a Cove body that builds `Some(was)` or `None` in a
/// temporary and copies it into its answer, the shape `Body::struct_literal`'s
/// seventh-rise note describes for an initializer. One `ret` copy per
/// instantiation — `examples:life` and `tests/e2e:coll_vector` 1 each,
/// `tests/e2e:coll_vector_edges` 3 (`Int`, `String`, a two-word struct) — and in
/// `examples:covefmtBench` 1 `ret` and 2 `prod`, where the body is also expanded
/// into a caller that holds the same shape.
///
/// **Then it falls, and by more than `Vector.freeze` added.** 2264 to 2241.
/// Moved alone, `freeze` was 2264 to 2285: twenty-one `ret` copies across
/// ten programs, one per function whose answer is a `freeze()` — `life` 7,
/// `coll_transform` 3, `cq`, `cqSample` and `coll_vector_edges` 2 each, and one
/// in each of five more. The builtin had written its `Array` into the call's
/// destination; `call_std_binding` handed a standard-library call none, so the
/// expanded finish wrote a temporary and a copy carried it out. Handing the
/// binding the destination its call site was given, as `call_target` already
/// does for every other call, removes those twenty-one and twenty-three more
/// that every earlier binding — `isEmpty`, `unwrapOr`, `filter`, `fold` in an
/// answer position — had been paying since it moved.
///
/// **The thirteenth rise is `Vector.pop` and `Vector.remove`, in `set`'s
/// shape.** 2241 to 2251, all ten in the `ret` column: `std.vector.pop<T>` and
/// `std.vector.remove<T>` build `Some(..)` or `None` in a temporary and copy it
/// into their answer, one per instantiation — `tests/e2e:coll_vector_edges` 6
/// (`Int`, `String` and a two-word struct, each popped and removed),
/// `tests/e2e:coll_vector` 2 and `examples:covefmtBench` 2. `Array.slice`,
/// `Vector.slice`, `toArray` and `toVector` moved a commit earlier and added
/// none.
///
/// **The fourteenth rise is one new row again.** 2251 to 2262, all eleven of
/// them `tests/e2e:values_string_bytes`, the fixture that pins every answer
/// `String.sliceBytes` and `String.join` give before ADR 0058 moves them into
/// the standard library (#378). It lands a commit ahead of the move, so the
/// lowering is the base's, and the row is a `match` per printed slice whose
/// arms each answer a `println`.
///
/// **The fifteenth rise is `String.sliceBytes` moving into the standard
/// library.** 2262 to 2291, all of it the `prod` column, and it is two
/// movements rather than one. `std.string.sliceBytes` is in every program's
/// survey and holds one copy after a producer — its byte `run-slice` answers a
/// temporary and the `Ok` copies it in — so each of the 136 rows gains one.
/// And the programs that call it lose some, because a `sliceBytes` that was a
/// builtin answered into a temporary its caller then copied, where the binding
/// is a call handed the destination: net of the one above, `examples:cq` and
/// `examples:cqSample` fall by 38 each, `examples:covecheck` by 19 and
/// `examples:covefmtBench` by 8. `refuseRange`
/// answers the whole `Result` so each refusal is a call into the body's answer
/// with no copy after it; its first shape, answering the `Error` alone, was
/// five more copies per row.
///
/// **The sixteenth rise is two new rows, at the floor.** 2291 to 2303, six
/// each for `tests/e2e:fail_stringbuilder_slice_range` and
/// `tests/e2e:fail_stringbuilder_invalid_utf8` — the `prod` and `ret` columns
/// `fail_divide_by_zero` and `fail_stringbuilder_byte_range` already have. They
/// pin the builder's other two faults before ADR 0058 moves the builder onto
/// core intrinsics (#378), so the lowering is the base's.
///
/// **The seventeenth rise is one new row, and it is a benchmark.** 2303 to
/// 2333, all thirty of them `benches:seqsearch` — 13 `prod` and 18 `ret`, one
/// copy counted in both — the rows that time `contains` and `indexOf` at every
/// sequence shape before ADR 0058 moves both searches into the standard
/// library (#378). The lowering is the base's; the copies are the row
/// functions' `Result`s and the searches' counts handed back through calls.
///
/// **The eighteenth rise is one new row, at the floor.** 2333 to 2347, all
/// fourteen `tests/e2e:coll_sequence_search`, the fixture that pins what
/// `contains`, `indexOf`, `get`, `length` and `String.codePointAtByte` answer
/// before ADR 0058 moves them into the standard library (#378). It lands ahead
/// of the moves, so the lowering is the base's.
///
/// **Then it falls, with `length` behind a thin wrapper.** 2347 to 2334. ADR
/// 0058's P3-15 (#378) made a sequence's `length()` `std.array.length` and
/// `std.vector.length`, one core intrinsic each, and an expanded wrapper writes
/// the call's destination directly where the inline lowering answered a
/// temporary its caller copied: a function whose answer is a `length()` loses
/// that copy (`examples:life` 2, `benches:callback`, `examples:restricted` and
/// six `tests/e2e:gc_*` rows 1 or 2 each, `gc_churn` and `gc_vector_grow` 3).
///
/// **The nineteenth rise is six new rows, over a constant that had not
/// followed the corpus down.** 2334 to 2339. Without the new rows the corpus
/// measures 2271 — the Phase 3 commits after the fall above removed 63 more
/// and left the constant where it was — and the rows that pin `Map` and `Set`
/// before ADR 0058's Phase 4 moves them into the standard library (#378, ADR
/// 0059) add 68: `benches:keyed` 36 (its row functions' `Result`s and counts
/// handed back through calls, as `benches:seqsearch`'s are),
/// `tests/e2e:fail_key_nested_float` 8, and `tests/e2e:coll_keyed_search`,
/// `fail_key_duplicate_map`, `fail_key_duplicate_set` and
/// `fail_key_float_empty_map` 6 each, which is the floor every program's
/// standard library sets. The lowering is the base's.
///
/// **The twentieth rise is `Map.get` and `contains` moving into the standard
/// library.** 2339 to 2354. ADR 0059's P4-4 (#378) made them binary searches
/// in Cove over `core.entryAt` and `core.memberAt`, and a search step reads a
/// whole entry out of the map with a `load-elem` and then copies the key field
/// out of it — a copy after a producer — as `get` does for the value it wraps
/// in `Some`, and a call answering a `Bool` or an `Option` where the builtin
/// wrote its word straight into the caller's destination. Only the programs
/// that search a map or a set move: `benches:keyed` +8, `examples:reviewPolicy`
/// +5 and `examples:covecheck` +2. A core intrinsic that loaded a key alone
/// would remove the field copies; it is not added for a count, and
/// `benches/keyed`'s lookup rows are faster than the builtin with them in.
///
/// **The twenty-first rise is `inserted` and `removed` moving into the
/// standard library.** 2354 to 2361. ADR 0059's P4-6 (#378) builds an updated
/// set or map in Cove — a seek, then a growable vector of exact room, the old
/// run's two ranges copied onto it around the new unit, and a keyed finish — and
/// a `Map.inserted` builds its `MapEntry` aside and copies it into the push, as
/// a call answering a set or a map where the builtin wrote its word straight
/// into the caller's destination. Only programs that update a map or a set
/// move: `benches:keyed` +4, `examples:cq` and `examples:cqSample` +1 each and
/// `examples:covecheck` +1.
///
/// **The twenty-second rise is `Set.of` and `Map.of` moving into the standard
/// library.** 2361 to 2390. ADR 0059's P4-8 (#378) makes a keyed literal a call
/// of `std.set.of` or `std.map.of` over the array its variadic parameter
/// receives, and each instantiation of `of` and of its private seek and place
/// carries a copy after a producer, so the rise follows how many key types a
/// program writes a literal of: `tests/e2e:coll_keyed_search` +18 (every key
/// family the fixture pins), `examples:callbacks` and `examples:reviewPolicy`
/// +2, and `benches:keyed`, `examples:covecheck`, `tests/e2e:coll_map`,
/// `coll_set`, `fail_invalid_map_key`, `fail_key_duplicate_map` and
/// `fail_key_duplicate_set` +1 each.
///
/// **The twenty-third rise is `lower::inline`'s frame budget.** 2390 to 2394,
/// all four of them in the `prod` column and in two programs:
/// `examples:covecheck` +2 and `examples:covefmtBench` +2. `FRAME_BUDGET` went
/// from 96 to 160 (#398), so a caller absorbs more leaves, and every `Return`
/// an expansion replaces becomes a copy of the answer into the call's
/// destination. Where the body assembled that answer with a producer, the
/// copy stands after one — which is a copy the call did not make, because a
/// call is *handed* the destination to write. So the bound follows the pass:
/// four copies here against 97,212 calls that are no longer made at all in
/// `covefmt`'s print phase.
///
/// **The twenty-fourth rise is the formatter building a vector only when it has
/// something to put in it.** 2394 to 2400, and `examples:covefmtBench` is the
/// only row that moves: 60 after a producer and 119 before a `return` become
/// 64 and 121. `examples/covefmt`'s `emit` used to open four `Vector.of()` for
/// every node it descended into and push into 362 of them over a whole corpus
/// (#398); it now holds two `Option<Vector<…>>` that begin at `None`, and the
/// six small functions that read and extend them each answer through the
/// location their caller named — which is the copy into a `return`'s answer
/// this counts. It buys 108,240 allocations and 216,936 words off the print
/// phase and 8.5% of its wall time, and the six are static: none of them is
/// inside a loop, and a program that runs the corpus makes at most a few
/// hundred of the calls that carry them.
///
/// **The twenty-fifth rise is the formatter's lexer answering one more
/// question.** 2400 to 2405, and `examples:covefmtBench` is again the only row
/// that moves: 64 and 121 become 67 and 123. `examples/covefmt`'s `tokens` is
/// now `lex` and answers a `Lexed` — the runs, and whether any comment in the
/// file is trailing — so a call that used to hand back a one-word `Array`
/// hands back a struct built beside the `return` and read through a field at
/// each of its callers. It removes a walk of every token of every file that
/// was **6.30% of the print phase's instructions** (#398): 3.47 M instructions
/// and 248 calls off print for 0.70 M onto lex, and 2.77 M off the run.
///
/// **The twenty-sixth rise is `String.indexOf` moving into the standard
/// library, and two new rows with it.** The corpus had fallen to 2394 under a
/// constant of 2405 — an upper bound is allowed to run ahead of it — and the
/// migration takes the same 157 programs to 2400. ADR 0064's fifth Phase 1
/// stage makes `String.indexOf` `std.string.indexOf`, one `core.stringFind`
/// and a walk, whose answer is a `Some(..)` built beside the `return`; every
/// program's standard library gains that body and the private
/// `charactersBefore` under it, which is the 314 more functions over 157
/// programs. Only the two programs that *call* it can move —
/// `tests/e2e:values_string` and `tests/e2e:values_string_length`, which
/// between them hold every `String.indexOf` site in the corpus, five and two.
///
/// Then 2400 to 2432, two new rows. `benches:indexof` adds 23 — 12 copies
/// after a producer and 12 into a `return`'s answer, one copy counted in both
/// — which is `benches:seqsearch`'s and `benches:contains`' shape: row
/// functions answering a `Row` struct and counting callees handing back an
/// `Int` through a call. `tests/e2e:values_string_index_of` adds 9, the `prod`
/// and `ret` columns a fixture of one `println` per line has. The lowering is
/// this commit's for all of it.
///
/// **The twenty-seventh rise is one new row and nothing else.** 2432 to 2448,
/// 159 programs to 160, and the sixteen are all in
/// `tests/e2e:values_float_abs` — four in the `prod` column and twelve in the
/// `ret` column. It is the corpus moving, in the plainest of the four ways
/// this constant cannot tell apart by itself, and the check is the one the
/// paragraphs above prescribe: the survey with that directory removed is
/// **2432 exactly**, so no row that was here before moved by a copy. The
/// program is ADR 0064's Decision 6 corpus for `Float.abs`, six functions of
/// `println` lines whose `ret` column is what a fixture that hands each
/// group's result back through a `Result<Unit, Error>` has.
///
/// **The twenty-eighth rise is one new row again, and the migration beside it
/// moved nothing at all.** 2448 to 2461, 160 programs to 161, and all thirteen
/// are `benches:floatabs` — five in the `prod` column and eleven in the `ret`
/// column, one copy counted in both. The same check as above says so twice
/// over: the survey with that directory removed is **2448 exactly**, over the
/// same 160 programs, the same 15,776 functions and the same 256,887
/// instructions. That second figure is the interesting one. ADR 0064's sixth
/// Phase 1 migration replaced `Float.abs`'s `Inst::IntrinsicCall` with one
/// `Inst::FloatAbs`, which is one instruction for one instruction, so **no
/// program in the corpus holds a different number of anything** — a migration
/// that leaves this survey byte-identical is a migration that changed the
/// operation and not the shape of the code around it.
///
/// **The twenty-ninth rise is one new row and nothing else, again.** 2461 to
/// 2477, 161 programs to 162, and all sixteen are
/// `tests/e2e:values_float_min_max` — two in the `prod` column and fourteen in
/// the `ret` column. The check the paragraphs above prescribe says so: the
/// survey with that directory removed is **2461 exactly**, so no row that was
/// here before moved by a copy. The program is ADR 0064's Decision 6 corpus
/// for `Float.min` and `Float.max`, seven functions of `println` lines whose
/// `ret` column is what a fixture that hands each group's result back through
/// a `Result<Unit, Error>` has — and whose `prod` column is small because the
/// lines are interpolations rather than bindings.
///
/// **The thirtieth rise is one new row again, and the migration beside it
/// moved nothing at all — for the second time running.** 2477 to 2493, 162
/// programs to 163, and all sixteen are `benches:floatminmax` — seven in the
/// `prod` column and thirteen in the `ret` column, four copies counted in
/// both. The survey with that directory removed is **2477 exactly**, over the
/// same 162 programs, the same 15,962 functions and the same 262,283
/// instructions. That last figure is the one worth reading: ADR 0064's last
/// two Phase 1 migrations replaced `Float.min`'s and `Float.max`'s
/// `Inst::IntrinsicCall` with one `Inst::FloatMinMax` each, one instruction
/// for one instruction, so **no program in the corpus holds a different
/// number of anything** — including the 89-line e2e corpus committed just
/// before them, which calls one or the other on nearly every line.
///
/// **The thirty-first rise is one new row and nothing else.** 2493 to 2512,
/// 163 programs to 164, and all nineteen are
/// `tests/e2e:values_float_round` — two in the `prod` column and seventeen in
/// the `ret` column, none counted in both. The check the paragraphs above
/// prescribe says so: the survey with that directory removed is **2493
/// exactly**, over the same 163 programs, the same 16,056 functions and the
/// same 263,867 instructions, which are the figures the row before it left.
/// The program is ADR 0064's Decision 6 corpus for `Float.round`, ten
/// functions of `println` lines whose `ret` column is what a fixture that
/// hands each group's result back through a `Result<Unit, Error>` has, and
/// whose `prod` column is two because `identity` and `computed` are the only
/// functions here that bind a rounded value rather than interpolating it.
///
/// **The thirty-second rise is one new row again, and the migration beside it
/// moved nothing at all — for the third time running.** 2512 to 2525, 164
/// programs to 165, and all thirteen are `benches:floatround` — five in the
/// `prod` column and eleven in the `ret` column, three copies counted in both.
/// The survey with that directory removed is **2512 exactly**, over the same
/// 164 programs, the same 16,153 functions and the same 267,845 instructions
/// the corpus commit left. That last figure is the one worth reading: issue
/// #454's Step 2 replaced `Float.round`'s `Inst::IntrinsicCall` with one
/// `Inst::FloatRound`, one instruction for one instruction, so **no program in
/// the corpus holds a different number of anything** — including the 80-line
/// e2e corpus committed just before it, which calls the operation on nearly
/// every line. The *machine code* is not one for one and nobody claimed it
/// was: the native lowering of that instruction is seventeen instructions
/// where the call was a crossing, which is what `benches/floatround` measures
/// and what this survey, being over the IR, cannot see.
///
/// **The thirty-third rise is one new row and nothing else.** 2525 to 2544,
/// 165 programs to 166, and all nineteen are `tests/e2e:values_float_sqrt` —
/// two in the `prod` column and seventeen in the `ret` column, none counted
/// in both. The check the paragraphs above prescribe says so: the survey with
/// that directory removed is **2525 exactly**, over the same 165 programs,
/// the same 16,245 functions and the same 269,241 instructions, which are the
/// figures the row before it left. The program is ADR 0064's Decision 6
/// corpus for `Float.sqrt`, ten functions of `println` lines whose `ret`
/// column is what a fixture that hands each group's result back through a
/// `Result<Unit, Error>` has, and whose `prod` column is two for
/// `values_float_round`'s reason: `identity` and `computed` are the only
/// functions here that bind a root rather than interpolate it.
///
/// **The thirty-fourth rise is thirteen from a new row and one from a bug the
/// new row found.** 2544 to 2558, 166 programs to 167. Thirteen are
/// `benches:floatsqrt` — five in the `prod` column and eleven in the `ret`
/// column, three copies counted in both — and the fourteenth is
/// `examples:covefmtBench`, which gained a `test fn` and with it one `ret`
/// copy. The survey with the bench directory removed is **2545**, which is
/// 2544 and that one, over 166 programs and 16,343 functions.
///
/// The extra function is worth the sentence, because it is the only rise in
/// this list that is a *repair*. covefmt's number scanner did not take the
/// sign of an exponent, so `5.0e-324` lexed as three tokens and printed as
/// `5.0e - 324`; every such literal already in the repository sat inside a
/// string interpolation or a call's parentheses, where `print` copies the
/// source, and `benches/floatsqrt`' `let sub = 5.0e-324` was the first
/// written anywhere else. `covefmtBench` failed on it, which is what that
/// gate is for.
///
/// **The migration itself still moved nothing at all — for the fourth time
/// running.** Hold the scanner fix aside and the figure to read is the
/// instruction count: issue #454's Step 2 replaced `Float.sqrt`'s
/// `Inst::IntrinsicCall` with one `Inst::FloatSqrt`, one instruction for one
/// instruction, so **no program in the corpus holds a different number of
/// anything** — including the 76-line e2e corpus committed just before it,
/// which calls the operation on nearly every line. The *machine code* is not
/// one for one either, and this time it is smaller rather than larger: the
/// native lowering of that instruction is two instructions and eighteen bytes
/// where the call was a crossing, which is what `benches/floatsqrt` measures
/// and what this survey, being over the IR, cannot see.
///
/// **The thirty-fifth rise is one new row and nothing else.** 2558 to 2573,
/// 167 programs to 168, and all fifteen are `tests/e2e:values_float_to_int` —
/// every one of them in the `ret` column and none in `prod`, which the two
/// sub-totals say on their own: the producer sub-total is 677 before and 677
/// after, the return sub-total is 1930 and then 1945, and the overlap between
/// them is 49 both times. The check the paragraphs above prescribe says the
/// rest: the survey with that directory removed is **2558 exactly**, over the
/// same 167 programs, the same 16,435 functions and the same 274,634
/// instructions the row before it left, and the twenty-five programs this
/// table prints are identical line for line.
///
/// The program is ADR 0064's Decision 6 corpus for `Float.toInt`, the last of
/// issue #454's Step 2 and the only one of that step's five operations a
/// representative program runs. A `prod` column of nought is what a file that
/// interpolates every answer rather than binding it has — `values_float_sqrt`
/// had two because two of its groups bound a root first, and this one binds
/// nothing a `println` then names.
///
/// It is an upper bound on what forwarding can remove and not a target, for
/// the reason the module documentation gives. What is left is mostly two
/// things: a producer this lowering does not hand a destination to yet (a
/// host call, a string literal, an argument list assembled elsewhere), and a
/// `copy` whose source is a **borrowed** location — a binding, a field — which
/// is ADR 0001's value semantics and is not waste at all.
const FORWARDABLE_COPIES: usize = 2573;

#[test]
fn the_corpus_says_how_much_of_it_is_a_value_being_moved() {
    let (total, rows) = survey();
    println!(
        "{} program(s), {} function(s), {} instruction(s), {} frame word(s)\n  \
         {} copy, {} clear\n  \
         {} copy after a producer ({} of them cleared straight after)\n  \
         {} copy into the answer a `return` names\n  \
         {} of the two together, which is the ratchet",
        rows.len(),
        total.functions,
        total.instructions,
        total.frame_words,
        total.copies,
        total.clears,
        total.after_producer,
        total.after_producer_and_cleared,
        total.before_return,
        total.forwardable(),
    );
    println!(
        "\n  {:>5} {:>5} {:>5} {:>5}  program",
        "instr", "copy", "prod", "ret"
    );
    for (name, found) in rows.iter().take(25) {
        println!(
            "  {:>5} {:>5} {:>5} {:>5}  {name}",
            found.instructions, found.copies, found.after_producer, found.before_return,
        );
    }

    assert!(
        total.forwardable() <= FORWARDABLE_COPIES,
        "the corpus holds {} copies a known destination would remove, and the ratchet is \
         {FORWARDABLE_COPIES}. It may fall and never rise: see issue #302.",
        total.forwardable(),
    );
}
