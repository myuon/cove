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
        Inst::CallBuiltin { dst, builtin, .. } => Some((
            dst,
            program
                .builtins
                .get(builtin.index())
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
/// It is an upper bound on what forwarding can remove and not a target, for
/// the reason the module documentation gives. What is left is mostly two
/// things: a producer this lowering does not hand a destination to yet (a
/// host call, a string literal, an argument list assembled elsewhere), and a
/// `copy` whose source is a **borrowed** location — a binding, a field — which
/// is ADR 0001's value semantics and is not waste at all.
const FORWARDABLE_COPIES: usize = 1868;

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
