//! A chain of `==` tests of one `Int` against small literals, dispatched as an
//! [`Inst::Switch`].
//!
//! Issue #535. A `match` on an `Int` with literal arms lowers to one
//! comparison per arm ([`super::pattern`] says why: there is no index to
//! switch on), and so does an `if`/`else if` ladder over one variable — which
//! is how `std.dynamic` names the kinds it handles, one `kind == 7` or
//! `kind != 8` at a time, since issue #533. A value tested late in such a
//! chain pays a comparison and a branch for every test in front of it, and
//! the whole chain counts against the inliner's limits instruction by
//! instruction.
//!
//! When the literals are dense over `0..=hi`, the chain *is* a table: each
//! literal goes to where its test sent it, and every other value goes to
//! where the last test sent a value it did not name. That table is the one an
//! enum's `match` already dispatches through, so no instruction is added —
//! the chain's first test becomes the `Switch` and the rest are dropped.
//!
//! # It is a peephole over emitted code
//!
//! For [`super::branches`]' reason: recognising the shape after lowering
//! keeps every lowering that produces a comparison as it was, and a form this
//! pass cannot see is a missed table rather than a wrong program. A test is
//! the unfused pair a comparison and its branch are before
//! [`super::branches`] runs — an [`Inst::CmpImm`] with `==` or `!=` and the
//! [`Inst::BranchFalse`] on what it wrote — and the next test of a chain is
//! the one the previous test's "some other value" edge lands on, through any
//! number of plain jumps.
//!
//! It runs **before** [`super::inline`], because the inliner weighs a
//! callee by its instruction count: a chain of sixteen tests is thirty-two
//! instructions, and the table it becomes is one. `std.dynamic.children` was
//! 91 instructions where the inliner weighs it, against a `LOOP_LIMIT` of 96,
//! and is 60 after this pass — which is the issue's other half: the next kind
//! added to it no longer turns it into a call.
//!
//! # What has to hold, and why each condition is there
//!
//! - **Every test reads the same `Int` slot, and none writes it.** A test
//!   writes only its own `Bool`, and a `Bool` slot is never the `Int` one, so
//!   one read of the slot at the head answers every test of the chain.
//! - **Nothing but the chain reaches a test after the first, or any test's
//!   branch.** Each of them is dropped, so a jump into one from elsewhere
//!   would land on whatever followed it — the condition [`super::branches`]
//!   states about a branch, for the same reason.
//! - **Every `Bool` a test wrote is dead where the switch can go.** The
//!   switch writes none of them, so a slot something reads afterwards would
//!   read whatever an earlier instruction left there. A slot a name holds
//!   over the chain is refused too, because a debugger reads it.
//! - **Every literal is in `0..=i32::MAX`.** A switch indexes its table from
//!   zero and the machine reads anything outside it as the default, which is
//!   exactly where a value no test names goes — so a chain whose literals are
//!   small and non-negative needs no subtraction to become one. A negative
//!   literal ends the chain where it stands; the tests from it on stay
//!   comparisons, reached through the table's default.
//! - **A literal named twice goes where its first test sent it.** The second
//!   test of it could never be reached by that value, which is what the chain
//!   did too.
//!
//! # The threshold was chosen by measurement
//!
//! A switch is never more dispatches than the chain it replaces — one, where
//! the chain's value at position `i` took `i` — but it is not free. Measured
//! on 2017b4b with a callee of six `if k == v { return … }` tests, called in a
//! loop, one binary with this pass on and off, 9 interleaved pairs each:
//!
//! | value hits test | VM | native |
//! |---|---|---|
//! | 1 | **+6.7%** (19.56 → 20.88 ns/call) | ±0 |
//! | 2 | −7.3% | −14.2% |
//! | 3 | −15.6% | −25.1% |
//! | 6 | −39.4% | −49.9% |
//! | none | −42.9% | −47.8% |
//!
//! So on the VM a switch costs about one and a half compare-branches, and a
//! chain whose *first* literal is the one that arrives beats it. Source order
//! tends to put the common literal first — issue #528 ordered `std.dynamic`'s
//! chains that way, by measurement — which is what [`MIN_ARMS`] guards.
//!
//! - **[`MIN_ARMS`] is 3.** With 2, the pass also takes `covefmt.lineAt`'s
//!   `breaks == 1` / `== 2`, `covefmt.completes` and
//!   `std.dynamic.orderBelow`: two-literal chains, where the row above is a
//!   loss whenever the first literal is the common one, for 20,496 fewer
//!   instructions of covefmt's 2.24 billion. Three is where a chain's second
//!   and third positions outweigh the first even when the first is the
//!   commonest.
//! - **[`MIN_FILL`] is one half.** Three quarters loses
//!   `std.dynamic.opening` (six of `0..=11`), and with it a deep enum's render
//!   goes from −4.58% to −3.67% VM instructions and a deep vector's from
//!   −1.83% to −1.22%. One quarter adds `std.dynamic.keyWord` and `trail`,
//!   which no measured row executes: every count is identical, so it would
//!   only add table entries.
//!
//! A hole costs the native tier nothing: its [`Inst::Switch`] is a compare
//! chain over the entries, and an entry that goes where the default does is
//! skipped.

use std::collections::BTreeMap;

use super::dropping;
use super::frees::{Bounds, Flow};
use crate::inst::{CmpOp, Inst, Pc, Slot};
use crate::program::{Function, Program, Table, TableId};
use crate::repr::Repr;

/// The fewest distinct literals a chain must name to become a table.
pub(super) const MIN_ARMS: usize = 3;

/// The fewest of a table's entries, `0..=hi`, that a literal must fill, as
/// `named * MIN_FILL.1 >= entries * MIN_FILL.0`.
pub(super) const MIN_FILL: (usize, usize) = (1, 2);

/// Rewrites every dense chain of literal tests in `program` as a switch, and
/// answers how many chains it rewrote.
pub(super) fn chains_into_switches(program: &mut Program) -> usize {
    let bounds = Bounds::of(program);
    let mut rewritten = 0;
    for at in 0..program.functions.len() {
        let chains = {
            let function = &program.functions[at];
            match Flow::of(function, program, &bounds) {
                Some(flow) => chains_in(function, &flow),
                None => continue,
            }
        };
        if chains.is_empty() {
            continue;
        }
        let mut dropped = vec![false; program.functions[at].code.len()];
        for chain in chains {
            let id = TableId(program.tables.len() as u32);
            program.tables.push(chain.table);
            program.functions[at].code[chain.head] = Inst::Switch {
                on: chain.on,
                table: id,
            };
            for pc in chain.dropped {
                dropped[pc] = true;
            }
            rewritten += 1;
        }
        let function = &mut program.functions[at];
        dropping::rewrite(function, &mut program.tables, &dropped);
    }
    rewritten
}

/// One chain this pass will rewrite.
struct Chain {
    /// The first test's comparison, which becomes the switch.
    head: usize,
    /// The slot every test reads.
    on: Slot,
    table: Table,
    /// Everything else the chain was: every branch, every later comparison,
    /// and every jump between two tests.
    dropped: Vec<usize>,
}

/// A test: the comparison at `pc` and the branch on what it wrote.
#[derive(Clone, Copy)]
struct Test {
    pc: usize,
    dst: Slot,
    on: Slot,
    value: i64,
    /// Where the value the test names goes.
    named: Pc,
    /// Where every other value goes.
    other: Pc,
}

fn test_at(function: &Function, pc: usize) -> Option<Test> {
    let Inst::CmpImm { op, dst, a, value } = *function.code.get(pc)? else {
        return None;
    };
    let Inst::BranchFalse { cond, to } = *function.code.get(pc + 1)? else {
        return None;
    };
    if cond != dst || dst == a || function.reprs.get(a as usize) != Some(&Repr::Int) {
        return None;
    }
    let next = (pc + 2) as Pc;
    let (named, other) = match op {
        CmpOp::Eq => (next, to),
        CmpOp::Ne => (to, next),
        _ => return None,
    };
    Some(Test {
        pc,
        dst,
        on: a,
        value,
        named,
        other,
    })
}

/// The chains of `function` that this pass rewrites, none overlapping.
fn chains_in(function: &Function, flow: &Flow<'_>) -> Vec<Chain> {
    let len = function.code.len();
    // How many edges arrive at each counter, the entry's included.
    let mut arriving = vec![0u32; len + 1];
    arriving[0] += 1;
    for pc in 0..len {
        flow.successors(pc, &mut |to| arriving[to.min(len)] += 1);
    }
    let mut taken = vec![false; len];
    let mut chains = Vec::new();
    for head in 0..len {
        if taken[head] {
            continue;
        }
        let Some(first) = test_at(function, head) else {
            continue;
        };
        if arriving[head + 1] != 1 || !in_table(first.value) {
            continue;
        }
        // The tests, and the jumps each later one was reached through.
        let mut tests = vec![(first, Vec::new())];
        let mut seen = vec![head];
        loop {
            let last = tests.last().expect("a chain has a first test").0;
            let mut at = last.other as usize;
            let mut hops = Vec::new();
            while let Some(Inst::Jump { to }) = function.code.get(at) {
                if arriving[at] != 1 || hops.contains(&at) {
                    break;
                }
                hops.push(at);
                at = *to as usize;
            }
            let Some(next) = test_at(function, at) else {
                break;
            };
            if next.on != first.on
                || !in_table(next.value)
                || arriving[at] != 1
                || arriving[at + 1] != 1
                || seen.contains(&at)
                || taken[at]
            {
                break;
            }
            seen.push(at);
            tests.push((next, hops));
        }
        // The longest prefix that is dense enough, if one is.
        let Some(chain) = (1..=tests.len())
            .rev()
            .find_map(|count| dense(function, flow, &tests[..count]))
        else {
            continue;
        };
        for &pc in chain.dropped.iter().chain(std::iter::once(&chain.head)) {
            taken[pc] = true;
        }
        chains.push(chain);
    }
    chains
}

fn in_table(value: i64) -> bool {
    (0..=i64::from(i32::MAX)).contains(&value)
}

/// `tests` as a switch, when they are dense enough and nothing reads the
/// `Bool`s they wrote.
fn dense(function: &Function, flow: &Flow<'_>, tests: &[(Test, Vec<usize>)]) -> Option<Chain> {
    let mut named: BTreeMap<i64, Pc> = BTreeMap::new();
    for (test, _) in tests {
        named.entry(test.value).or_insert(test.named);
    }
    let hi = *named.keys().next_back()? as usize;
    let entries = hi + 1;
    if named.len() < MIN_ARMS || named.len() * MIN_FILL.1 < entries * MIN_FILL.0 {
        return None;
    }
    let default = tests.last()?.0.other;
    let mut targets = vec![default; entries];
    for (value, to) in &named {
        targets[*value as usize] = *to;
    }
    let mut dsts: Vec<Slot> = tests.iter().map(|(test, _)| test.dst).collect();
    dsts.sort_unstable();
    dsts.dedup();
    let pcs: Vec<usize> = tests.iter().map(|(test, _)| test.pc).collect();
    let (from, to) = (*pcs.iter().min()?, pcs.iter().max()? + 2);
    for &dst in &dsts {
        if flow.addressed[dst as usize] {
            return None;
        }
        let named_over = function.locals.iter().any(|local| {
            let width = flow.width(local.layout);
            local.slot <= dst
                && dst < local.slot + width
                && (local.from as usize) < to
                && from < local.to as usize
        });
        if named_over {
            return None;
        }
        let exits = targets.iter().chain(std::iter::once(&default));
        if exits
            .clone()
            .any(|&exit| read_before_written(function, flow, exit as usize, dst))
        {
            return None;
        }
    }
    let mut dropped = Vec::new();
    for (index, (test, hops)) in tests.iter().enumerate() {
        if index > 0 {
            dropped.push(test.pc);
        }
        dropped.push(test.pc + 1);
        dropped.extend(hops);
    }
    Some(Chain {
        head: tests[0].0.pc,
        on: tests[0].0.on,
        table: Table { targets, default },
        dropped,
    })
}

/// Whether some path from `start` reads `slot` before writing it.
fn read_before_written(function: &Function, flow: &Flow<'_>, start: usize, slot: Slot) -> bool {
    let len = function.code.len();
    let mut seen = vec![false; len];
    let mut work = vec![start];
    while let Some(pc) = work.pop() {
        if pc >= len || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let inst = &function.code[pc];
        let mut read = false;
        flow.reads(inst, &mut |base, width| {
            read |= base <= slot && slot < base + width;
        });
        if read {
            return true;
        }
        let mut written = false;
        flow.writes(inst, &mut |base, width| {
            written |= base <= slot && slot < base + width;
        });
        if !written {
            flow.successors(pc, &mut |to| work.push(to));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::Span;

    use super::*;
    use crate::layout::{Layout, LayoutId};
    use crate::program::Local;
    use crate::repr::RefMap;

    const INT: LayoutId = LayoutId(0);
    const BOOL: LayoutId = LayoutId(1);

    fn span() -> Span {
        Span::new(cove_diag::FileId(0), 0, 0)
    }

    /// `s0` is the `Int` tested, `s1` the `Bool` each test writes, `s2` the
    /// answer.
    fn ran(code: Vec<Inst>, locals: Vec<Local>) -> Program {
        let reprs = vec![Repr::Int, Repr::Bool, Repr::Int];
        let function = Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: vec![INT],
            spans: (0..code.len()).map(|_| span()).collect(),
            refs: RefMap::of(&reprs),
            reprs,
            returns: INT,
            captures: Vec::new(),
            code,
            locals,
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        };
        let mut program = Program {
            functions: vec![function],
            layouts: vec![
                Layout::word("Int", Repr::Int),
                Layout::word("Bool", Repr::Bool),
            ],
            ..Program::default()
        };
        chains_into_switches(&mut program);
        program
    }

    fn code(program: &Program) -> &[Inst] {
        &program.function(crate::FunctionId(0)).code
    }

    fn test(op: CmpOp, value: i64, to: Pc) -> [Inst; 2] {
        [
            Inst::CmpImm {
                op,
                dst: 1,
                a: 0,
                value,
            },
            Inst::BranchFalse { cond: 1, to },
        ]
    }

    fn answer(value: i64) -> [Inst; 2] {
        [Inst::Int { dst: 2, value }, Inst::Return { src: 2 }]
    }

    /// `if k == v0 { return 10 } if k == v1 { return 11 } ... return -1`,
    /// which is what both a literal `match` and a ladder of `if`s lower to.
    fn ladder(values: &[i64]) -> Vec<Inst> {
        let mut code = Vec::new();
        for (at, value) in values.iter().enumerate() {
            let next = (code.len() + 4) as Pc;
            code.extend(test(CmpOp::Eq, *value, next));
            code.extend(answer(10 + at as i64));
        }
        code.extend(answer(-1));
        code
    }

    /// Where each entry of the only table sends a value, read as the answer
    /// the instruction there writes.
    fn answers(program: &Program) -> (Vec<Inst>, Inst) {
        let table = &program.tables[0];
        let at = |to: Pc| code(program)[to as usize].clone();
        (
            table.targets.iter().map(|to| at(*to)).collect(),
            at(table.default),
        )
    }

    /// The whole of the pass: three dense tests become one switch, and every
    /// value goes where its test sent it.
    #[test]
    fn a_dense_chain_becomes_one_switch() {
        let program = ran(ladder(&[0, 1, 2]), Vec::new());
        assert_eq!(
            code(&program),
            [
                Inst::Switch {
                    on: 0,
                    table: TableId(0)
                },
                Inst::Int { dst: 2, value: 10 },
                Inst::Return { src: 2 },
                Inst::Int { dst: 2, value: 11 },
                Inst::Return { src: 2 },
                Inst::Int { dst: 2, value: 12 },
                Inst::Return { src: 2 },
                Inst::Int { dst: 2, value: -1 },
                Inst::Return { src: 2 },
            ]
        );
        assert_eq!(
            program.tables,
            [Table {
                targets: vec![1, 3, 5],
                default: 7,
            }]
        );
    }

    /// Order does not matter, and a hole goes where a value no test names
    /// goes.
    #[test]
    fn a_hole_in_the_table_is_the_default() {
        let program = ran(ladder(&[3, 0, 1]), Vec::new());
        assert!(matches!(code(&program)[0], Inst::Switch { .. }));
        assert_eq!(
            answers(&program),
            (
                vec![
                    Inst::Int { dst: 2, value: 11 },
                    Inst::Int { dst: 2, value: 12 },
                    Inst::Int { dst: 2, value: -1 },
                    Inst::Int { dst: 2, value: 10 },
                ],
                Inst::Int { dst: 2, value: -1 },
            )
        );
    }

    /// `!=` sends the value it names down the branch and everything else on,
    /// which is how `std.dynamic` writes most of its tests.
    #[test]
    fn a_not_equal_test_names_its_branch_target() {
        let mut held = Vec::new();
        held.extend(test(CmpOp::Ne, 0, 8));
        held.extend(test(CmpOp::Ne, 1, 10));
        held.extend(test(CmpOp::Ne, 2, 12));
        held.extend(answer(-1));
        held.extend(answer(10));
        held.extend(answer(11));
        held.extend(answer(12));
        let program = ran(held, Vec::new());
        assert_eq!(code(&program).len(), 9);
        assert_eq!(
            answers(&program),
            (
                vec![
                    Inst::Int { dst: 2, value: 10 },
                    Inst::Int { dst: 2, value: 11 },
                    Inst::Int { dst: 2, value: 12 },
                ],
                Inst::Int { dst: 2, value: -1 },
            )
        );
    }

    /// The next test may be reached through plain jumps, which is what an
    /// `else if` leaves between two tests; the jumps go with the chain.
    #[test]
    fn a_jump_between_two_tests_goes_with_them() {
        let mut held = Vec::new();
        // 0: k == 0 else -> 4; 2: return 10
        held.extend(test(CmpOp::Eq, 0, 4));
        held.extend(answer(10));
        // 4: jump 5; 5: k == 1 else -> 9; 7: return 11
        held.push(Inst::Jump { to: 5 });
        held.extend(test(CmpOp::Eq, 1, 9));
        held.extend(answer(11));
        // 9: k == 2 else -> 13; 11: return 12; 13: return -1
        held.extend(test(CmpOp::Eq, 2, 13));
        held.extend(answer(12));
        held.extend(answer(-1));
        let program = ran(held, Vec::new());
        assert_eq!(code(&program).len(), 9);
        assert!(!code(&program)
            .iter()
            .any(|i| matches!(i, Inst::Jump { .. })));
    }

    /// Too few literals over too wide a range stay comparisons: a table of
    /// a thousand entries for three values is not what the chain was.
    #[test]
    fn a_sparse_chain_is_left_as_it_is() {
        let held = ladder(&[1, 100, 1000]);
        assert_eq!(code(&ran(held.clone(), Vec::new())), held);
        // And two literals are below the arm count however dense they are.
        let two = ladder(&[0, 1]);
        assert_eq!(code(&ran(two.clone(), Vec::new())), two);
    }

    /// A switch indexes from zero, so a negative literal ends the chain where
    /// it stands; the tests after it are still a chain of their own.
    #[test]
    fn a_negative_literal_is_not_in_the_table() {
        let held = ladder(&[-1, -2, -3]);
        assert_eq!(code(&ran(held.clone(), Vec::new())), held);
        let program = ran(ladder(&[-1, 0, 1, 2]), Vec::new());
        assert!(matches!(code(&program)[0], Inst::CmpImm { value: -1, .. }));
        assert!(matches!(code(&program)[4], Inst::Switch { .. }));
        assert_eq!(program.tables[0].targets.len(), 3);
        // And one after the dense tests ends the chain there: it is the
        // table's default, still a comparison.
        let program = ran(ladder(&[0, 1, 2, -1]), Vec::new());
        assert!(matches!(code(&program)[0], Inst::Switch { .. }));
        let (_, default) = answers(&program);
        assert!(matches!(default, Inst::CmpImm { value: -1, .. }));
    }

    /// A literal tested twice goes where its first test sent it: the second
    /// test could never have been reached by that value.
    #[test]
    fn a_duplicate_literal_goes_where_its_first_test_went() {
        let program = ran(ladder(&[0, 1, 0, 2]), Vec::new());
        assert!(matches!(code(&program)[0], Inst::Switch { .. }));
        assert_eq!(
            answers(&program).0,
            [
                Inst::Int { dst: 2, value: 10 },
                Inst::Int { dst: 2, value: 11 },
                Inst::Int { dst: 2, value: 13 },
            ]
        );
    }

    /// The switch writes none of the `Bool`s, so a chain whose answer is read
    /// after it is left alone.
    #[test]
    fn a_test_whose_answer_is_read_again_is_left_as_it_is() {
        let mut held = ladder(&[0, 1, 2]);
        let last = held.len() - 1;
        held[last] = Inst::Return { src: 1 };
        assert_eq!(code(&ran(held.clone(), Vec::new())), held);
    }

    /// Nor is one whose `Bool` a name holds, which a debugger can read.
    #[test]
    fn a_test_whose_answer_is_named_is_left_as_it_is() {
        let held = ladder(&[0, 1, 2]);
        let named = vec![Local {
            name: Arc::from("zero"),
            slot: 1,
            layout: BOOL,
            from: 0,
            to: held.len() as Pc,
        }];
        assert_eq!(code(&ran(held.clone(), named)), held);
    }

    /// A later test that something else jumps to has to stay where it is,
    /// because dropping it would send that jump somewhere else.
    #[test]
    fn a_test_reached_from_elsewhere_ends_the_chain() {
        let mut held = ladder(&[0, 1, 2, 3]);
        // `return 10` becomes a jump to the third test.
        held[2] = Inst::Jump { to: 8 };
        held.remove(3);
        held.insert(3, Inst::Return { src: 2 });
        let program = ran(held, Vec::new());
        // The two tests in front of it are below the arm count.
        assert!(program.tables.is_empty());
    }
}
