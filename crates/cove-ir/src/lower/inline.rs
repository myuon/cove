//! Expanding a call to a small leaf function where it is made.
//!
//! `examples/covefmt`'s profile is what asked for this. Over half a megabyte
//! of Cove through a lexer, a parser and a printer, **43% of every instruction
//! executed was inside a tiny leaf function**: `Scan.at` is eight instructions
//! and ran 3.46 million times, `utf8Width` is four and ran 1.32 million times.
//!
//! Eight instructions is not what a call to one of them costs. The caller
//! evaluates the arguments, an `Inst::Call` pushes a frame and zeroes it,
//! `self` — a two-word struct — is copied in, and a `Return` copies the answer
//! out and pops. The native profile of the same run puts 22% in
//! `Memory::read`, `Memory::write` and `Memory::copy_words` and 5% in
//! `open_frame`, which is what three and a half million of those look like
//! from below.
//!
//! # What is expanded, and why the rule is this one
//!
//! A **leaf**: a function that calls nothing at all — no `Call`, no
//! `CallClosure`, no host or resource call, no `spawn`, no `await`, no scope
//! and no cell. That is a narrow rule and it is chosen for a reason beyond
//! narrowness: a leaf cannot reach its own caller, so **recursion is
//! impossible by construction** and this pass needs no call graph, no depth
//! counter and no cycle check. The 43% the profile named is leaves; a rule
//! that also caught non-leaves would buy the rest of the tail and cost the
//! proof.
//!
//! It must also be small enough for where it is being put — [`LIMIT`] at a
//! site that runs once and [`HOT_LIMIT`] at one a loop reaches, which is what
//! [`hot_functions`] decides — take no captures — a lambda's captures
//! are copied by the call and are not arguments — and not be `async`, whose
//! answer is a task the caller wraps rather than the value the body produced.
//!
//! # Why there is no second rule about failing
//!
//! There was one, and it is worth recording what it was and what removed it,
//! because it is the rule anyone reaching for this pass will reach for again.
//! A `RuntimeError` names where it happened *and the frames above it* —
//! `Machine::call_chain` reads the live frames — and an expansion has no
//! frame. So an error raised inside an expanded body kept its span and lost
//! its chain, and `differential.rs` reported exactly that: the oracle named
//! `std/int.cove` and the call site in `main`, and the machine named
//! `std/int.cove` and nothing. ADR 0012 ranks the oracle above the backend,
//! so that was the backend being wrong, and this pass first answered it by
//! refusing to expand any body that could fail.
//!
//! That answer cost more than it looked like it did. `Scan.at` — 22% of the
//! instructions `covefmt` executes on its own — holds four instructions such
//! a rule refuses and **not one of them can fail**: a `neg` of the constant
//! one, a builtin that answers an `Option` rather than stopping the run, a
//! `switch` whose table holds every case of the enum it switches on, and the
//! `trap` on the default that switch can therefore never take. Sharpening the
//! rule until it could see all four is four small analyses, each of which is
//! a thing to keep right.
//!
//! [`Inlined`] replaces the rule instead of sharpening it. Each expansion
//! records the run of counters it wrote and the call site it removed, and
//! `Machine::call_chain` reads that range and puts the site back. A body that
//! fails no longer loses anything, so there is nothing left for a rule about
//! failing to protect — and the record is worth having on the bodies that
//! *cannot* fail too, because a debugger's backtrace and a profile's
//! attribution ask the same question an error chain asks.
//!
//! The record is a pair of program counters, so it moves when they do:
//! [`super::dropping`] renumbers it beside a [`Local`](crate::Local)'s pair.
//! Nothing reads it during a run, which is what makes forgetting that easy
//! and quiet — a range two counters out of place verifies, runs, and answers
//! about the wrong instructions.
//!
//! # Where the callee's slots go
//!
//! Appended to the caller's frame, once per callee rather than once per call
//! site. Two calls to one leaf cannot be live at the same time — a leaf calls
//! nothing, so one of them has finished before the other begins — so they
//! share the run, and a caller that reads a character forty times grows by one
//! `Scan` frame instead of forty.
//!
//! The arguments are copied into the callee's parameter slots exactly as the
//! machine would have copied them into a fresh frame, and every `Return` in
//! the callee becomes a copy of the answer into the call's destination
//! followed by a jump past the expansion.
//!
//! # What it leaves behind, and who cleans it
//!
//! A frame that is popped takes its references with it, and an expansion has
//! no frame to pop: a `Repr::Ref` word in the callee's run stays a root of the
//! *caller's* frame until the next expansion overwrites it. So this emits a
//! `Clear` of each reference word after the body, and leaves deciding which of
//! them frees nothing to [`super::frees`], which is the pass that already
//! answers that question about a path through finished code.

use crate::inst::{Inst, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::{Function, FunctionId, Inlined, Program, Table};
use crate::repr::RefMap;

use super::shapes;

/// How many instructions a function may hold and still be expanded into a
/// call site that runs once.
///
/// Sixteen, which is `Scan.at`'s eight and `utf8Width`'s four with room, and
/// which is under the size at which a second copy of a body starts to cost
/// more in instruction cache than it saves in frames. It is a number chosen
/// against the workload that asked for the pass rather than derived, and the
/// measurement in `examples/covefmt/README.md` is what would move it.
///
/// It is the limit for a *cold* site now. What a call costs is the same
/// wherever it stands, but what expanding one costs is a copy of the body per
/// site, and what it buys is a frame per time the site runs — so the two are
/// weighed against different numbers and want different limits. See
/// [`HOT_LIMIT`] and [`hot_functions`].
const LIMIT: usize = 16;

/// Expands every call this pass is willing to expand.
pub(super) fn expand_small_leaf_calls(program: &mut Program) {
    for _ in 0..ROUNDS {
        let small: Vec<bool> = (0..program.functions.len())
            .map(|at| is_expandable(&program.functions[at], LIMIT))
            .collect();
        let wide: Vec<bool> = (0..program.functions.len())
            .map(|at| is_expandable(&program.functions[at], HOT_LIMIT))
            .collect();
        if !wide.iter().any(|held| *held) {
            return;
        }
        let hot = hot_functions(program);
        let before: usize = program.functions.iter().map(|f| f.code.len()).sum();
        for (at, called_often) in hot.iter().enumerate() {
            expand(program, FunctionId(at as u32), &small, &wide, *called_often);
        }
        if program
            .functions
            .iter()
            .map(|f| f.code.len())
            .sum::<usize>()
            == before
        {
            return;
        }
    }
}

/// How many times the pass is run over its own answer.
///
/// A body that took a leaf's instructions may be a leaf itself now — `Scan.at`
/// is expanded into `Scan.word`, and `Scan.word` called nothing else — so the
/// question is asked again. It terminates on its own: a round that expanded
/// nothing is the last, and the cap is here for the reader rather than for the
/// loop.
///
/// **Rounds alone are worth nothing**, and that is worth writing down because
/// it is the obvious half to reach for. Measured on `examples/covefmt`, with
/// [`LIMIT`] where it was, iterating changed the instruction count the run
/// executed by *zero* — 281,263,951 either way — while adding 1,839
/// instructions of code. Everything a second round finds has grown by exactly
/// what it absorbed, so nothing new fits under a limit the first round was
/// already measuring against. The rounds pay only beside the budget below.
const ROUNDS: usize = 8;

/// How many words of frame one caller may take on from everything it expands.
///
/// The callee's slots are appended to the caller's frame, so a caller that
/// absorbs many of them is a caller whose every call zeroes a wider frame —
/// and `open_frame` zeroes it whether the expansion runs or not.
///
/// Without this, `examples/covefmt`'s `emit` went from 66 words to 223. It is
/// the hottest function there is in that program and it recurses once per node
/// of the tree, so what a wide budget bought in frames it was handing back in
/// the zeroing of the one frame that is pushed most. The whole run was still
/// 5% faster, which is the sort of number that hides a mistake rather than
/// showing it.
///
/// Ninety-six, which is where the curve stops. Swept against the same corpus:
///
/// | budget | widest frame | `cove fmt --check`'s work |
/// |---:|---:|---:|
/// | none (16, one round) | 66 | 905 ms |
/// | 64 | 63 | 880 ms |
/// | **96** | **93** | **864 ms** |
/// | 128 | 122 | 864 ms |
/// | 192 | 174 | 865 ms |
/// | unbounded | 223 | 864 ms |
///
/// Everything past ninety-six is frame words for nothing. The unbounded pass
/// looked like the fastest one until the curve was swept, and it was not: it
/// was the same speed having also tripled the frame of the function that is
/// pushed most.
///
/// The ratchet in `crates/cove-cli/tests/bytecode_corpus.rs` watches the rest,
/// and this keeps it where it was.
const FRAME_BUDGET: usize = 96;

/// How many instructions a function may hold and still be expanded into a
/// call site that runs often.
///
/// [`LIMIT`] was chosen against the lexer, where `Scan.at` is seven
/// instructions and `utf8Width` is four. The printer's hot leaves are not that
/// shape at all: they are the functions that take a byte or a token and answer
/// a small thing, and every one of them is over it —
///
/// | | instructions | call sites | share of the run |
/// |---|---:|---:|---:|
/// | `byteOfPunct` | 29 | 29 | 5.3% |
/// | `previousSignificant` | 38 | 9 | 2.0% |
/// | `isTrivia` | 33 | 4 | 2.3% |
/// | `isARange` | 35 | 3 | 1.6% |
/// | `isOperatorByte` | 56 | 2 | 1.5% |
///
/// Forty-eight reaches four of the five. It is a number measured against that
/// table rather than derived, as [`LIMIT`] is, and the same measurement is
/// what would move it.
const HOT_LIMIT: usize = 48;

/// Which functions are reached from a loop, and so run often enough to spend
/// code size on.
///
/// Two rules, and they refer to each other:
///
/// - a **call site** is hot when it stands inside a loop, or when the function
///   holding it is hot;
/// - a **function** is hot when a hot call site calls it.
///
/// A fixed point over the call graph, which terminates because the set only
/// grows and is bounded by the functions there are.
///
/// # Why the loop a function holds is not the question
///
/// `wantsASpaceBetween` holds no loop at all — it is a run of `if`s — and it
/// is 2.1% of what `examples/covefmt` executes, because `emit` walks a loop
/// that reaches it through `spacing`. Read one function at a time, its call to
/// `byteOfPunct` is a call that happens once. Read through the graph, it is
/// the 5.3% that `byteOfPunct` turned out to be.
///
/// Measured at the [`FRAME_BUDGET`] below, that is the difference between the
/// loop a function holds and the loops that reach it: 901 ms against 872 for
/// the same corpus, three runs each.
///
/// # What it is not
///
/// It is reachability and not a count. A heavy function called once from
/// `main` stays cold, and a function called once from inside a loop is as hot
/// as one called forty times there. `BlockFrequencyInfo` weighs a loop as ten
/// turns and carries a number; this carries a bit. The next grain of this is a
/// weight per loop depth, and what would ask for it is a program where the
/// budget is spent in the wrong place.
fn hot_functions(program: &Program) -> Vec<bool> {
    let loops: Vec<Vec<bool>> = program.functions.iter().map(inside_a_loop).collect();
    let mut hot = vec![false; program.functions.len()];
    loop {
        let mut moved = false;
        for (at, f) in program.functions.iter().enumerate() {
            for (pc, inst) in f.code.iter().enumerate() {
                let Inst::Call { callee, .. } = inst else {
                    continue;
                };
                if (loops[at][pc] || hot[at]) && !hot[callee.index()] {
                    hot[callee.index()] = true;
                    moved = true;
                }
            }
        }
        if !moved {
            return hot;
        }
    }
}

/// The pcs a backward jump encloses.
///
/// Every loop the lowering emits closes with a jump back to its own head, so
/// the range between a jump and its target is the body of one. It is the
/// whole of the loop analysis this pass has, and the whole of what it needs:
/// what the budget asks is whether a call runs many times, and being inside a
/// backward jump is the only way the instruction stream says so.
fn inside_a_loop(f: &Function) -> Vec<bool> {
    let mut held = vec![false; f.code.len()];
    for (at, inst) in f.code.iter().enumerate() {
        let back = match inst {
            Inst::Jump { to } => Some(*to as usize),
            Inst::BranchFalse { to, .. } => Some(*to as usize),
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                Some(*target as usize)
            }
            _ => None,
        };
        if let Some(to) = back {
            if to <= at {
                for held in held.iter_mut().take(at + 1).skip(to) {
                    *held = true;
                }
            }
        }
    }
    held
}

/// Whether a call to this function may be expanded where it is made.
fn is_expandable(f: &Function, limit: usize) -> bool {
    if f.stub || f.is_async || !f.captures.is_empty() || f.code.len() > limit {
        return false;
    }
    // A `var` parameter is an address into the *caller's* frame, which an
    // expansion would leave pointing at a run the expansion itself owns.
    // Nothing about that is unsound, and nothing about it is simple either,
    // so it waits for a program that shows the cost of leaving it out.
    if f.params.contains(&shapes::ADDR) {
        return false;
    }
    f.code.iter().all(reaches_nothing)
}

/// Whether an instruction leaves the function it is in.
///
/// Everything that can reach another body, and everything that can reach the
/// *runtime* in a way an expansion would have to think about: a scope is a
/// stack discipline the machine keeps per frame, and a cell's lock is held by
/// a task rather than by a frame.
///
/// This is a `matches!` exclusion list rather than an exhaustive match, so
/// nothing here forces a new [`Inst`] variant to be considered — unlike
/// [`written`] and [`slots_of`] below, a variant left out compiles silently.
/// [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
/// `AllocBytes`, `WriteByte`, `CopyBytes` and `FinishString` make no call and
/// touch no scope or cell, so they belong in the list of things this refuses
/// nothing for — correctly left out of the `matches!` above — but that is a
/// fact worth writing down here precisely because the compiler cannot check
/// it. The same is true of
/// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
/// `AllocBuffer`, `AppendByte`, `AppendBytes` and `FinishBuffer`: they
/// allocate and they copy, and an allocation is not a call.
fn reaches_nothing(inst: &Inst) -> bool {
    !matches!(
        inst,
        Inst::Call { .. }
            | Inst::CallClosure { .. }
            | Inst::CallHost { .. }
            | Inst::CallResource { .. }
            | Inst::Spawn { .. }
            | Inst::Await { .. }
            | Inst::Settled { .. }
            | Inst::Cancel { .. }
            | Inst::ScopeEnter { .. }
            | Inst::ScopeLeave { .. }
            | Inst::ScopeCancel { .. }
            | Inst::SharedLock { .. }
            | Inst::SharedUnlock { .. }
    )
}

/// The one slot every `Return` of this function names, where they agree.
///
/// A leaf that answers from two places usually answers from one *location* —
/// the lowering assembles an answer where the form that wanted it said — and
/// where it does not, there is nothing here to rename.
fn single_return(f: &Function) -> Option<Slot> {
    let mut held: Option<Slot> = None;
    for inst in &f.code {
        if let Inst::Return { src } = inst {
            match held {
                Some(seen) if seen != *src => return None,
                _ => held = Some(*src),
            }
        }
    }
    held
}

/// Which words of a function's frame something writes.
///
/// The destination of every instruction, as the verifier's `fits` reads one:
/// a base slot and the words the layout it names covers. A parameter no
/// instruction here writes is a parameter an expansion need not copy — see
/// [`Region::renamed`].
fn written(program: &Program, f: &Function) -> Vec<bool> {
    let mut held = vec![false; f.reprs.len()];
    let mut mark = |slot: Slot, width: u32| {
        for at in slot..slot.saturating_add(width) {
            if let Some(place) = held.get_mut(at as usize) {
                *place = true;
            }
        }
    };
    let width = |layout: LayoutId| program.layout(layout).width();
    for inst in &f.code {
        match *inst {
            Inst::Copy { dst, layout, .. }
            | Inst::Load { dst, layout, .. }
            | Inst::LoadField { dst, layout, .. }
            | Inst::LoadElem { dst, layout, .. }
            | Inst::Unbox { dst, layout, .. } => mark(dst, width(layout)),
            Inst::Clear { slot, layout } => mark(slot, width(layout)),
            Inst::CallBuiltin { dst, builtin, .. } => {
                mark(dst, width(program.builtin(builtin).result))
            }
            Inst::Unit { dst }
            | Inst::Bool { dst, .. }
            | Inst::Int { dst, .. }
            | Inst::Float { dst, .. }
            | Inst::Str { dst, .. }
            | Inst::Tag { dst, .. }
            | Inst::FuncRef { dst, .. }
            | Inst::Neg { dst, .. }
            | Inst::Not { dst, .. }
            | Inst::Convert { dst, .. }
            | Inst::Arith { dst, .. }
            | Inst::Cmp { dst, .. }
            | Inst::ArithImm { dst, .. }
            | Inst::CmpImm { dst, .. }
            | Inst::CmpBranch { dst, .. }
            | Inst::CmpImmBranch { dst, .. }
            | Inst::Alloc { dst, .. }
            | Inst::Box { dst, .. }
            | Inst::ByteAt { dst, .. }
            | Inst::AllocBytes { dst, .. }
            | Inst::FinishString { dst, .. }
            | Inst::AllocBuffer { dst, .. }
            | Inst::FinishBuffer { dst, .. }
            | Inst::Len { dst, .. }
            | Inst::LayoutOf { dst, .. }
            | Inst::AddrOfSlot { dst, .. }
            | Inst::AddrOfField { dst, .. }
            | Inst::AddrOfElem { dst, .. }
            | Inst::AddrOfPart { dst, .. } => mark(dst, 1),
            // A store writes an object or an address rather than a frame
            // word, so it marks nothing; the rest are what `reaches_nothing`
            // refuses. Written out rather than caught by a `_` for the reason
            // `slots_of`'s own tail gives at length: a `_` here is a promise
            // that two lists are complements, and `Inst::ByteAt` is the
            // instruction that broke it.
            Inst::StoreField { .. }
            | Inst::StoreElem { .. }
            | Inst::Store { .. }
            | Inst::WriteByte { .. }
            | Inst::CopyBytes { .. }
            | Inst::AppendByte { .. }
            | Inst::AppendBytes { .. }
            | Inst::Jump { .. }
            | Inst::BranchFalse { .. }
            | Inst::Switch { .. }
            | Inst::Return { .. }
            | Inst::Trap { .. }
            | Inst::AssertFailed { .. }
            | Inst::Call { .. }
            | Inst::CallClosure { .. }
            | Inst::CallHost { .. }
            | Inst::CallResource { .. }
            | Inst::Spawn { .. }
            | Inst::Await { .. }
            | Inst::Settled { .. }
            | Inst::Cancel { .. }
            | Inst::ScopeEnter { .. }
            | Inst::ScopeLeave { .. }
            | Inst::ScopeCancel { .. }
            | Inst::SharedLock { .. }
            | Inst::SharedUnlock { .. } => {}
        }
    }
    held
}

/// Where one callee's run begins in a caller's frame, and what it holds.
struct Region {
    base: Slot,
    /// The reference words of the run, which the expansion clears after it.
    refs: Vec<Slot>,
    /// How many of the callee's leading slots are parameters it never writes.
    ///
    /// Those need no copy and no run of their own: the body can read the
    /// caller's argument where it stands.
    ///
    /// `fn id<T>(x: T) -> T { x }` is the shape at its smallest, and what it
    /// costs afterwards is one instruction:
    ///
    /// ```text
    /// call s2:Int m.id<Int> (s1:Int)   becomes   copy s2:Int s1:Int
    /// ```
    ///
    /// the parameter read where the caller has it and the answer written
    /// where the caller wanted it, with no frame between them.
    renamed: u32,
}

/// Expands the calls in one function.
fn expand(program: &mut Program, id: FunctionId, small: &[bool], wide: &[bool], called_hot: bool) {
    let caller = program.function(id).clone();
    let hot: Vec<bool> = if called_hot {
        vec![true; caller.code.len()]
    } else {
        inside_a_loop(&caller)
    };
    // What this caller may still take on. A callee's run is appended once
    // however many sites call it, so the budget is spent per *callee* and the
    // sites after the first are free.
    let mut room = FRAME_BUDGET.saturating_sub(caller.reprs.len());
    let mut taken: Vec<FunctionId> = Vec::new();
    let wanted: Vec<bool> = caller
        .code
        .iter()
        .enumerate()
        .map(|(at, inst)| match inst {
            Inst::Call { callee, .. } => {
                if *callee == id {
                    return false;
                }
                let eligible = if hot[at] {
                    wide[callee.index()]
                } else {
                    small[callee.index()]
                };
                if !eligible {
                    return false;
                }
                if taken.contains(callee) {
                    return true;
                }
                let words = program.function(*callee).reprs.len();
                if words > room {
                    return false;
                }
                room -= words;
                taken.push(*callee);
                true
            }
            _ => false,
        })
        .collect();
    if !wanted.iter().any(|held| *held) {
        return;
    }

    let mut reprs = caller.reprs.clone();
    let mut regions: std::collections::HashMap<u32, Region> = std::collections::HashMap::new();
    let mut code: Vec<Inst> = Vec::with_capacity(caller.code.len());
    let mut spans = Vec::with_capacity(caller.code.len());
    // Where each of the caller's own instructions ended up, so its jumps and
    // its locals can be renumbered once everything has moved.
    let mut moved: Vec<Pc> = Vec::with_capacity(caller.code.len() + 1);
    // The jumps an expansion's `return` became, and where each has to land.
    let mut ends: Vec<(usize, usize)> = Vec::new();
    let mut tables: Vec<Table> = Vec::new();
    let mut lists: Vec<Vec<crate::program::Arg>> = Vec::new();
    // What each expansion removed, so that a chain, a backtrace and a profile
    // can put it back. See `Inlined`.
    let mut records: Vec<Inlined> = Vec::new();

    for (at, inst) in caller.code.iter().enumerate() {
        moved.push(code.len() as Pc);
        if !wanted[at] {
            code.push(inst.clone());
            spans.push(caller.spans[at]);
            continue;
        }
        let Inst::Call { dst, callee, args } = inst else {
            unreachable!("only a call is wanted");
        };
        let span = caller.spans[at];
        let leaf = program.function(*callee).clone();
        // A frame this pass grew past what a slot operand can name would be
        // reported by `super::limits` as a fault in the program, and the
        // program would be innocent: it is this pass that widened it. So the
        // budget is checked here and a call that would cross it is left a
        // call.
        if !regions.contains_key(&callee.0)
            && reprs.len() + leaf.reprs.len() > crate::MAX_FRAME_WORDS
        {
            code.push(inst.clone());
            spans.push(caller.spans[at]);
            continue;
        }
        let region = regions.entry(callee.0).or_insert_with(|| {
            // The leading parameter words the body never writes are read
            // where the caller already has them, so the run begins after them
            // and they cost neither a slot nor a copy.
            let assigned = written(program, &leaf);
            let taken = leaf.param_words(&program.layouts) as usize;
            let renamed = assigned
                .iter()
                .take(taken)
                .position(|held| *held)
                .unwrap_or(taken) as u32;
            let base = reprs.len() as Slot;
            reprs.extend(leaf.reprs.iter().skip(renamed as usize).copied());
            Region {
                base,
                refs: leaf
                    .reprs
                    .iter()
                    .enumerate()
                    .skip(renamed as usize)
                    .filter(|(_, repr)| repr.is_ref())
                    .map(|(at, _)| base + at as Slot - renamed)
                    .collect(),
                renamed,
            }
        });
        let base = region.base;
        let renamed = region.renamed;

        // The arguments, into the parameter slots the machine would have
        // copied them into. A parameter takes the words its layout says, in
        // order, from slot zero — which is `Function::param_words`' rule read
        // one parameter at a time.
        // And where the answer goes. Every `Return` of a leaf that names one
        // slot names the location the leaf assembled its answer in, and that
        // location can be the caller's destination itself — which is issue
        // #302's destination forwarding, applied to a body being expanded
        // rather than to a form being lowered. The copy that would have
        // carried the answer out then does not exist.
        //
        // Two conditions. The `Return`s must agree on one slot, because two
        // that did not would need two destinations. And the destination must
        // not overlap an argument the body reads where it stands, because
        // writing the answer would then overwrite an argument still to be
        // read.
        let answering = single_return(&leaf);
        let mut where_of: Vec<Slot> = (0..leaf.reprs.len() as u32)
            .map(|at| if at < renamed { 0 } else { base + at - renamed })
            .collect();
        let mut at_slot: Slot = 0;
        for (arg, layout) in program.arg_list(*args).to_vec().iter().zip(&leaf.params) {
            let width = program.layout(*layout).width();
            if at_slot < renamed {
                for offset in 0..width {
                    where_of[(at_slot + offset) as usize] = arg.slot + offset;
                }
            } else {
                code.push(Inst::Copy {
                    dst: base + at_slot - renamed,
                    src: arg.slot,
                    layout: arg.layout,
                });
                spans.push(span);
            }
            at_slot += width;
        }

        // The answer's run, once the arguments are placed: an overlap with one
        // of them is what stops it.
        let answers = answering.filter(|src| {
            let width = program.layout(leaf.returns).width();
            let over = |a: Slot, b: Slot| a < b + width && b < a + width;
            (0..renamed).all(|at| !over(*dst, where_of[at as usize]))
                && (*src as usize) < leaf.reprs.len()
        });
        if let Some(src) = answers {
            for offset in 0..program.layout(leaf.returns).width() {
                where_of[(src + offset) as usize] = *dst + offset;
            }
        }

        // Where each of the leaf's instructions lands. A `Return` becomes up
        // to a copy *and* a jump, so the leaf's program counters are not the
        // new ones shifted by a constant — and a rule that shifted them by
        // one was the bug this found: `utf8Width` returns from three places,
        // so every branch past them landed one, two and three instructions
        // early, and a comment holding a `—` ended at the dash again.
        //
        // The two conditions below are the two the emitting loop applies, and
        // they have to be the same two: the copy is skipped when the answer
        // is already where the call wanted it, and the jump is skipped when
        // the `return` is the last instruction and falls out of the body
        // rather than leaving it. A prediction that assumed both would be
        // emitted was a second bug of the same kind, found by the assertion
        // below once `return` began forwarding its destination and a leaf
        // started answering in place.
        let body = code.len();
        let mut place: Vec<usize> = Vec::with_capacity(leaf.code.len() + 1);
        let mut at_new = body;
        for (pc, held) in leaf.code.iter().enumerate() {
            place.push(at_new);
            at_new += match held {
                Inst::Return { src } => {
                    let copies = usize::from(where_of[*src as usize] != *dst);
                    let jumps = usize::from(pc + 1 < leaf.code.len());
                    copies + jumps
                }
                _ => 1,
            };
        }
        place.push(at_new);

        for (pc, held) in leaf.code.iter().enumerate() {
            let moved_here = code.len();
            debug_assert_eq!(moved_here, place[pc], "the leaf was placed as planned");
            match held {
                Inst::Return { src } => {
                    // A copy of a run onto itself is what an answer written
                    // straight into the destination looks like here, and it
                    // is not emitted.
                    let from = where_of[*src as usize];
                    if from != *dst {
                        code.push(Inst::Copy {
                            dst: *dst,
                            src: from,
                            layout: leaf.returns,
                        });
                        spans.push(leaf.spans[pc]);
                    }
                    // The last instruction of a body falls out of it, so the
                    // jump it would need is a jump to the next instruction.
                    if pc + 1 < leaf.code.len() {
                        ends.push((code.len(), 0));
                        code.push(Inst::Jump { to: PENDING });
                        spans.push(leaf.spans[pc]);
                    }
                }
                other => {
                    code.push(relocated(
                        other,
                        &where_of,
                        &place,
                        &mut tables,
                        &mut lists,
                        program,
                    ));
                    spans.push(leaf.spans[pc]);
                }
            }
            let _ = moved_here;
        }
        let after = code.len();
        for (jump, land) in ends.iter_mut() {
            if *land == 0 && *jump >= body {
                *land = after;
            }
        }

        records.push(Inlined {
            from: moved[at],
            to: code.len() as Pc,
            callee: *callee,
            site: span,
            // The leaf's own names, through the two maps this expansion
            // already built: `where_of` says where each of its slots went and
            // `place` says where each of its counters went. Without them a
            // stop inside an expanded body could name nothing the source had
            // written — `print doubled` in a two-line function that binds
            // `doubled`.
            locals: leaf
                .locals
                .iter()
                .map(|local| crate::Local {
                    name: local.name.clone(),
                    slot: where_of[local.slot as usize],
                    layout: local.layout,
                    from: place[local.from as usize] as Pc,
                    to: place[local.to as usize] as Pc,
                })
                .collect(),
        });

        // A leaf may hold expansions of its own — `twice` is inside `raise`
        // before `raise` is inside `main` — and a round that dropped them
        // would leave a chain one call short of the truth while looking
        // complete. `Function::inlined_at` reads nesting ranges outermost
        // first, so these go in after the record for the body holding them.
        let nested: Vec<Inlined> = leaf
            .inlined
            .iter()
            .map(|held| Inlined {
                from: place[held.from as usize] as Pc,
                to: place[held.to as usize] as Pc,
                callee: held.callee,
                site: held.site,
                locals: held
                    .locals
                    .iter()
                    .map(|local| crate::Local {
                        name: local.name.clone(),
                        slot: where_of[local.slot as usize],
                        layout: local.layout,
                        from: place[local.from as usize] as Pc,
                        to: place[local.to as usize] as Pc,
                    })
                    .collect(),
            })
            .collect();
        records.extend(nested);

        // A reference the expansion leaves in the caller's frame is a root
        // until something overwrites it, because there is no frame to pop.
        // `super::frees` drops the ones that free nothing.
        for slot in &region.refs {
            code.push(Inst::Clear {
                slot: *slot,
                layout: shapes::REF,
            });
            spans.push(span);
        }
    }
    moved.push(code.len() as Pc);

    for (jump, land) in &ends {
        code[*jump] = Inst::Jump { to: *land as Pc };
    }
    renumber(&mut code, &caller.code, &moved, program, &mut tables);

    let first = program.tables.len() as u32;
    program.tables.extend(tables);
    let listed = program.args.len() as u32;
    program.args.extend(lists);
    for inst in code.iter_mut() {
        match inst {
            Inst::Switch { table, .. } if table.0 >= PLACED => {
                *table = crate::TableId(first + (table.0 - PLACED));
            }
            Inst::CallBuiltin { args, .. }
            | Inst::CopyBytes { args }
            | Inst::AppendBytes { args }
                if args.0 >= PLACED =>
            {
                *args = crate::ArgsId(listed + (args.0 - PLACED));
            }
            _ => {}
        }
    }

    let mut locals = caller.locals.clone();
    for local in locals.iter_mut() {
        local.from = moved[local.from as usize];
        local.to = moved[local.to as usize];
    }

    let held = &mut program.functions[id.index()];
    held.refs = RefMap::of(&reprs);
    held.reprs = reprs;
    held.code = code;
    held.spans = spans;
    held.locals = locals;
    // What an earlier round of this pass already recorded, in the numbering
    // this round produced. Without it a second round is a round that forgets:
    // the chain would be right after one expansion and short after two, and
    // nothing reads these during a run to say so.
    //
    // These come first because a new expansion is never inside an old one —
    // an expanded body is a leaf's, and a leaf holds no call to expand — so
    // the two groups do not nest and the order between them is free, while
    // the order *within* each is what `inlined_at` reads.
    let mut inlined = caller.inlined.clone();
    for record in inlined.iter_mut() {
        record.from = moved[record.from as usize];
        record.to = moved[record.to as usize];
        for local in record.locals.iter_mut() {
            local.from = moved[local.from as usize];
            local.to = moved[local.to as usize];
        }
    }
    // The rest are already in the new numbering — an expansion knows where it
    // put itself — and the `Clear`s that follow each body are *outside* its
    // range, which is right: a reference the expansion left behind is the
    // caller's to give up, and an error raised at one of those was not raised
    // inside the callee. `super::dropping` moves these when it moves the
    // locals.
    inlined.extend(records);
    held.inlined = inlined;
}

/// The target a jump this pass has not landed yet carries.
const PENDING: Pc = Pc::MAX;

/// Where a table this pass builds is numbered from, before the tables are
/// appended to the program and the numbers are made absolute.
const PLACED: u32 = 1 << 31;

/// One instruction of a leaf, moved into a caller's frame and code.
fn relocated(
    inst: &Inst,
    where_of: &[Slot],
    place: &[usize],
    tables: &mut Vec<Table>,
    lists: &mut Vec<Vec<crate::program::Arg>>,
    program: &Program,
) -> Inst {
    let mut held = inst.clone();
    for slot in slots_of(&mut held) {
        *slot = where_of[*slot as usize];
    }
    match &mut held {
        Inst::Jump { to } => *to = place[*to as usize] as Pc,
        Inst::BranchFalse { to, .. } => *to = place[*to as usize] as Pc,
        // A fused comparison's target is a program counter of the *leaf* and
        // has to move like every other. The arms of this match are the
        // instructions that name one, and it is the one place in this pass
        // where a missing arm is silent rather than a compile error: the slot
        // walk above is exhaustive, this is not. An expansion that left a
        // fused branch pointing into the leaf's own numbering would branch
        // into the middle of the caller.
        Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
            *target = place[*target as usize] as Pc
        }
        // An argument list is `Program::args` and not part of the
        // instruction, so shifting the slots the instruction names does not
        // reach it. A builtin is the one call a leaf may hold, and
        // `Inst::CopyBytes` and `Inst::AppendBytes` are the non-call
        // instructions that also name one — each is the list relocated into a
        // list of its own.
        Inst::CallBuiltin { args, .. } | Inst::CopyBytes { args } | Inst::AppendBytes { args } => {
            lists.push(
                program
                    .arg_list(*args)
                    .iter()
                    .map(|arg| crate::program::Arg {
                        slot: where_of[arg.slot as usize],
                        layout: arg.layout,
                    })
                    .collect(),
            );
            *args = crate::ArgsId(PLACED + lists.len() as u32 - 1);
        }
        Inst::Switch { table, .. } => {
            // A table's targets are absolute program counters of the function
            // it was built for, and this is a different function now. One
            // table per switch site — `lower::mod` interns none — so a new one
            // per expansion is one more entry and never a shared one changed
            // under somebody else.
            let held = program.table(*table);
            tables.push(Table {
                targets: held
                    .targets
                    .iter()
                    .map(|to| place[*to as usize] as Pc)
                    .collect(),
                default: place[held.default as usize] as Pc,
            });
            *table = crate::TableId(PLACED + tables.len() as u32 - 1);
        }
        _ => {}
    }
    held
}

/// Renumbers the caller's own jumps, which moved when instructions were
/// inserted in front of them.
fn renumber(
    code: &mut [Inst],
    before: &[Inst],
    moved: &[Pc],
    program: &Program,
    tables: &mut Vec<Table>,
) {
    for (at, inst) in before.iter().enumerate() {
        let to = moved[at] as usize;
        match inst {
            Inst::Jump { to: target } => {
                code[to] = Inst::Jump {
                    to: moved[*target as usize],
                };
            }
            Inst::BranchFalse { cond, to: target } => {
                code[to] = Inst::BranchFalse {
                    cond: *cond,
                    to: moved[*target as usize],
                };
            }
            // The caller's own fused branches, moved for the reason its jumps
            // are: instructions were inserted in front of them. Written as an
            // edit of the copy already at `to` rather than as a rebuild,
            // because the two variants have five fields between them that are
            // not the target and none of them changes.
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                if let Inst::CmpBranch { target: held, .. }
                | Inst::CmpImmBranch { target: held, .. } = &mut code[to]
                {
                    *held = moved[*target as usize];
                }
            }
            Inst::Switch { on, table } => {
                let held = program.table(*table);
                tables.push(Table {
                    targets: held
                        .targets
                        .iter()
                        .map(|target| moved[*target as usize])
                        .collect(),
                    default: moved[held.default as usize],
                });
                code[to] = Inst::Switch {
                    on: *on,
                    table: crate::TableId(PLACED + tables.len() as u32 - 1),
                };
            }
            _ => {}
        }
    }
}

/// Every slot an instruction names, to be added to.
fn slots_of(inst: &mut Inst) -> Vec<&mut Slot> {
    match inst {
        Inst::Unit { dst } | Inst::Bool { dst, .. } | Inst::Int { dst, .. } => vec![dst],
        Inst::Float { dst, .. } | Inst::Str { dst, .. } | Inst::Tag { dst, .. } => vec![dst],
        Inst::FuncRef { dst, .. } => vec![dst],
        Inst::Copy { dst, src, .. } => vec![dst, src],
        Inst::Clear { slot, .. } => vec![slot],
        Inst::Neg { dst, a, .. } | Inst::Not { dst, a } | Inst::Convert { dst, a, .. } => {
            vec![dst, a]
        }
        Inst::ArithImm { dst, a, .. } | Inst::CmpImm { dst, a, .. } => vec![dst, a],
        Inst::Arith { dst, a, b, .. } | Inst::Cmp { dst, a, b, .. } => vec![dst, a, b],
        Inst::CmpImmBranch { dst, a, .. } => vec![dst, a],
        Inst::CmpBranch { dst, a, b, .. } => vec![dst, a, b],
        Inst::BranchFalse { cond, .. } => vec![cond],
        Inst::Switch { on, .. } => vec![on],
        Inst::Return { src } => vec![src],
        Inst::Alloc { dst, len, .. } => match len {
            crate::Len::Slot(slot) => vec![dst, slot],
            _ => vec![dst],
        },
        Inst::LoadField { dst, obj, .. } => vec![dst, obj],
        Inst::StoreField { obj, src, .. } => vec![obj, src],
        Inst::LoadElem {
            dst, obj, index, ..
        } => vec![dst, obj, index],
        Inst::StoreElem {
            obj, index, src, ..
        } => vec![obj, index, src],
        Inst::ByteAt { dst, obj, at } => vec![dst, obj, at],
        Inst::AllocBytes { dst, len } => vec![dst, len],
        Inst::WriteByte { bytes, at, value } => vec![bytes, at, value],
        Inst::FinishString { dst, bytes } => vec![dst, bytes],
        Inst::AllocBuffer { dst, capacity } => vec![dst, capacity],
        Inst::AppendByte { buffer, value } => vec![buffer, value],
        Inst::FinishBuffer { dst, buffer } => vec![dst, buffer],
        // The five operands live in the args row rather than on the
        // instruction, exactly as a call's do — `relocated` moves that row
        // and repoints `args` at the copy, the same way it does for
        // `Inst::CallBuiltin`.
        Inst::CopyBytes { .. } | Inst::AppendBytes { .. } => Vec::new(),
        Inst::Len { dst, obj } | Inst::LayoutOf { dst, obj } => vec![dst, obj],
        Inst::AddrOfSlot { dst, slot } => vec![dst, slot],
        Inst::AddrOfField { dst, obj, .. } => vec![dst, obj],
        Inst::AddrOfElem {
            dst, obj, index, ..
        } => vec![dst, obj, index],
        Inst::AddrOfPart { dst, addr, .. } => vec![dst, addr],
        Inst::Load { dst, addr, .. } => vec![dst, addr],
        Inst::Store { addr, src, .. } => vec![addr, src],
        Inst::Box { dst, src, .. } | Inst::Unbox { dst, src, .. } => vec![dst, src],
        Inst::CallBuiltin { dst, .. } => vec![dst],
        Inst::AssertFailed { message } => vec![message],
        Inst::Jump { .. } | Inst::Trap { .. } => Vec::new(),
        // Every variant below is one `reaches_nothing` refuses, so a leaf
        // never holds one and none of them can arrive here.
        //
        // Listed rather than caught by a `_`, and the difference is not
        // tidiness. A `_` here makes this function's correctness depend on a
        // *promise* made in `reaches_nothing` — that the two lists are each
        // other's complement — and nothing checked it. `Inst::ByteAt` was
        // added, `reaches_nothing` let it through because it reaches nothing,
        // and this returned no slots for it: an expansion that renumbered
        // every other instruction left that one pointing into the callee's
        // frame. It was caught by `verify`, one layer further on, and only
        // because the slot it kept happened to hold a `bool`.
        //
        // Written out, a new instruction fails to compile here until somebody
        // says which of its fields are slots — which is what
        // `vm::exec::encoded::implemented` does for the same reason.
        Inst::Call { .. }
        | Inst::CallClosure { .. }
        | Inst::CallHost { .. }
        | Inst::CallResource { .. }
        | Inst::Spawn { .. }
        | Inst::Await { .. }
        | Inst::Settled { .. }
        | Inst::Cancel { .. }
        | Inst::ScopeEnter { .. }
        | Inst::ScopeLeave { .. }
        | Inst::ScopeCancel { .. }
        | Inst::SharedLock { .. }
        | Inst::SharedUnlock { .. } => Vec::new(),
    }
}
