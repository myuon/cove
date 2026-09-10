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
//! It must also be small ([`LIMIT`]), take no captures — a lambda's captures
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

/// How many instructions a function may hold and still be expanded.
///
/// Sixteen, which is `Scan.at`'s eight and `utf8Width`'s four with room, and
/// which is under the size at which a second copy of a body starts to cost
/// more in instruction cache than it saves in frames. It is a number chosen
/// against the workload that asked for the pass rather than derived, and the
/// measurement in `examples/covefmt/README.md` is what would move it.
const LIMIT: usize = 16;

/// Expands every call this pass is willing to expand.
pub(super) fn expand_small_leaf_calls(program: &mut Program) {
    let leaves: Vec<bool> = (0..program.functions.len())
        .map(|at| is_expandable(&program.functions[at]))
        .collect();
    if !leaves.iter().any(|held| *held) {
        return;
    }
    for at in 0..program.functions.len() {
        expand(program, FunctionId(at as u32), &leaves);
    }
}

/// Whether a call to this function may be expanded where it is made.
fn is_expandable(f: &Function) -> bool {
    if f.stub || f.is_async || !f.captures.is_empty() || f.code.len() > LIMIT {
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
            | Inst::Alloc { dst, .. }
            | Inst::Box { dst, .. }
            | Inst::Len { dst, .. }
            | Inst::LayoutOf { dst, .. }
            | Inst::AddrOfSlot { dst, .. }
            | Inst::AddrOfField { dst, .. }
            | Inst::AddrOfElem { dst, .. }
            | Inst::AddrOfPart { dst, .. } => mark(dst, 1),
            // A store writes an object or an address rather than a frame
            // word, and everything else `is_expandable` refused.
            _ => {}
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
fn expand(program: &mut Program, id: FunctionId, leaves: &[bool]) {
    let caller = program.function(id).clone();
    let wanted: Vec<bool> = caller
        .code
        .iter()
        .map(|inst| match inst {
            Inst::Call { callee, .. } => *callee != id && leaves[callee.index()],
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

        // Where each of the leaf's instructions lands. A `Return` becomes a
        // copy *and* a jump, so the leaf's program counters are not the new
        // ones shifted by a constant — and a rule that shifted them by one
        // was the bug this found: `utf8Width` returns from three places, so
        // every branch past them landed one, two and three instructions
        // early, and a comment holding a `—` ended at the dash again.
        let body = code.len();
        let mut place: Vec<usize> = Vec::with_capacity(leaf.code.len() + 1);
        let mut at_new = body;
        for (pc, held) in leaf.code.iter().enumerate() {
            place.push(at_new);
            at_new += match held {
                Inst::Return { .. } if pc + 1 < leaf.code.len() => 2,
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
            Inst::CallBuiltin { args, .. } if args.0 >= PLACED => {
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
    // Already in the new numbering — an expansion knows where it put itself —
    // and the `Clear`s that follow each body are *outside* its range, which is
    // right: a reference the expansion left behind is the caller's to give up,
    // and an error raised at one of those was not raised inside the callee.
    // `super::dropping` moves these when it moves the locals.
    held.inlined = records;
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
        // An argument list is `Program::args` and not part of the
        // instruction, so shifting the slots the instruction names does not
        // reach it. A builtin is the one call a leaf may hold, and this is
        // the list it names, relocated into a list of its own.
        Inst::CallBuiltin { args, .. } => {
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
        // Every remaining variant is one `is_expandable` refused, so a leaf
        // never holds one and this is unreachable rather than incomplete.
        _ => Vec::new(),
    }
}
