//! `match`, and the patterns its arms are written with.
//!
//! # An enum dispatches; everything else compares
//!
//! A `match` over an enum reads the case index out of word 0 of the object
//! and hands it to [`Inst::Switch`], so an enum with twenty cases costs one
//! indexed jump rather than twenty comparisons. A `match` over anything else
//! — an `Int`, a `String`, a `Bool` — is a chain of comparisons, because
//! there is no index to switch on and the arms' literals are values rather
//! than a dense numbering.
//!
//! The table has a default even where the checker proved the `match`
//! exhaustive, because the index came out of a heap object and the machine
//! does not take the lowering's word for what is in it. The default, and the
//! end of a comparison chain, is [`Inst::Trap`].
//!
//! # An arm's body is emitted once
//!
//! The switch's targets and the arms' failure edges are patched afterwards,
//! which is what lets every arm's body be emitted exactly once even though
//! several cases may reach the same arm. A `_` arm is the tail of every
//! case's chain; an arm for one case is in that case's chain alone, so where
//! it goes when its payload does not match is a fact known statically.
//!
//! # A test leaves nothing behind
//!
//! Deciding whether an arm matches can read an object out of a payload word,
//! and that read is a reference in a slot. An arm that then fails jumps away
//! without running its body, so the slot is cleared on that path too — which
//! is what the per-arm failure block below is for. Everything a pattern
//! *binds* is emitted only after every test has passed, so no binding is
//! ever live on a path that did not match.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{MatchArm, Pattern, PatternKind};

use super::frame::Val;
use super::gap;
use super::shapes;
use super::{Body, PENDING};
use crate::inst::{CmpOp, Compare, Inst, Pc, Slot};
use crate::program::{Table, TableId};
use crate::repr::Repr;

/// The table id a [`Inst::Switch`] carries until its arms have been laid out
/// and their entry points are known.
const UNPLACED: TableId = TableId(u32::MAX);

/// Which values of the scrutinee an arm can match.
enum Reach {
    /// Every one of them: `_`, or a name the arm binds.
    Any,
    /// One case, which the switch dispatches to. The arm may still fail on
    /// the case's payload, and the jumps it leaves behind when it does are
    /// what the chain for that case is patched from.
    Case { index: u32 },
}

/// What deciding one arm left behind: where it branches when it does not
/// match, and the slots its tests read objects into.
#[derive(Default)]
struct Tests {
    failures: Vec<Pc>,
    /// Slots holding a reference a test read out of an object. They are dead
    /// on both paths out of the test, and the failing path is the one that
    /// would otherwise leave one set.
    held: Vec<Slot>,
}

impl Body<'_> {
    /// `match scrutinee { ... }`, with or without an answer.
    pub(super) fn match_expr(
        &mut self,
        scrutinee: &cove_syntax::ast::Expr,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Slot>,
    ) {
        let Some(ty) = self.owned_ty(scrutinee) else {
            return;
        };
        let subject = self.expr(scrutinee);
        if shapes::enum_cases(self.checked, self.module, &ty).is_some() {
            self.match_enum(&ty, subject.slot, arms, span, dst);
        } else {
            self.match_chain(&ty, subject.slot, arms, span, dst);
        }
        self.release(subject, span);
    }

    /// A `match` over an enum: one indexed jump, then one arm.
    fn match_enum(
        &mut self,
        ty: &Ty,
        subject: Slot,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Slot>,
    ) {
        let cases = shapes::enum_cases(self.checked, self.module, ty).expect("an enum-shaped type");
        let mut reach = Vec::with_capacity(arms.len());
        for arm in arms {
            reach.push(self.reach_of(ty, &arm.pattern));
        }

        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: tag,
                obj: subject,
                at: 0,
            },
            span,
        );
        let switch = self.emit(
            Inst::Switch {
                on: tag,
                table: UNPLACED,
            },
            span,
        );
        self.frame.free(tag);

        // Which arms each case may reach, in order. The list stops at the
        // first arm that matches everything, because nothing after it is
        // reachable for that case.
        let mut chains: Vec<Vec<usize>> = vec![Vec::new(); cases.len()];
        for (case, chain) in chains.iter_mut().enumerate() {
            for (index, arm) in reach.iter().enumerate() {
                match arm {
                    Reach::Any => {
                        chain.push(index);
                        break;
                    }
                    Reach::Case { index: at, .. } if *at as usize == case => chain.push(index),
                    Reach::Case { .. } => {}
                }
            }
        }

        let mut entries = vec![0; arms.len()];
        let mut failures: Vec<Vec<Pc>> = vec![Vec::new(); arms.len()];
        let mut ends = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            entries[index] = self.here();
            // The switch has already established which case the object is
            // in, so only the payload is still in question.
            let dispatched = matches!(reach[index], Reach::Case { .. });
            failures[index] = self.arm(arm, ty, subject, dst, dispatched, &mut ends);
        }

        let trap = self.here();
        let message = self.string("no `match` arm covers this value");
        self.emit(Inst::Trap { message }, span);

        for (index, pending) in failures.into_iter().enumerate() {
            let Reach::Case { index: case, .. } = reach[index] else {
                continue;
            };
            let chain = &chains[case as usize];
            let next = chain
                .iter()
                .position(|held| *held == index)
                .and_then(|at| chain.get(at + 1))
                .map(|next| entries[*next])
                .unwrap_or(trap);
            for at in pending {
                self.patch(at, next);
            }
        }

        let targets = chains
            .iter()
            .map(|chain| chain.first().map(|arm| entries[*arm]).unwrap_or(trap))
            .collect();
        let table = self.pool.table(Table {
            targets,
            default: trap,
        });
        self.place_table(switch, table);

        let end = self.here();
        for at in ends {
            self.patch(at, end);
        }
    }

    /// A `match` over anything that is not an enum: a comparison per arm.
    fn match_chain(
        &mut self,
        ty: &Ty,
        subject: Slot,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Slot>,
    ) {
        let mut ends = Vec::new();
        for arm in arms {
            let failures = self.arm(arm, ty, subject, dst, false, &mut ends);
            let next = self.here();
            for at in failures {
                self.patch(at, next);
            }
        }
        let message = self.string("no `match` arm covers this value");
        self.emit(Inst::Trap { message }, span);
        let end = self.here();
        for at in ends {
            self.patch(at, end);
        }
    }

    /// One arm: its tests, its bindings, its body, and where it goes when it
    /// does not match.
    ///
    /// The answer is the jumps still to be patched to whatever comes next
    /// for this arm — which is the next arm in a chain, and the next
    /// candidate for this case under a switch.
    fn arm(
        &mut self,
        arm: &MatchArm,
        ty: &Ty,
        subject: Slot,
        dst: Option<Slot>,
        dispatched: bool,
        ends: &mut Vec<Pc>,
    ) -> Vec<Pc> {
        self.frame.push_scope();
        let mut tests = Tests::default();
        self.test(&arm.pattern, subject, ty, dispatched, &mut tests);

        // The tests have passed, so what they read is dead. The same slots
        // are cleared again on the failing path below, which is the path
        // that would otherwise leave an object reachable from a frame that
        // has moved on.
        let held = tests.held.clone();
        self.clear(&held, arm.pattern.span);

        self.bind(&arm.pattern, subject, ty, arm.pattern.span);
        match dst {
            Some(dst) => {
                let value = self.expr(&arm.body);
                self.store(dst, &value, &arm.body);
                self.release(value, arm.body.span);
            }
            None => self.discard(&arm.body),
        }
        let clears = self.frame.pop_scope();
        self.clear(&clears, arm.span);
        ends.push(self.emit(Inst::Jump { to: PENDING }, arm.span));

        let pending = if tests.failures.is_empty() || held.is_empty() {
            tests.failures
        } else {
            // One failure block per arm rather than one clear per branch:
            // every test of this arm leaves the same slots set, and the arm
            // has one place to go when it does not match.
            let failing = self.here();
            self.clear(&held, arm.pattern.span);
            let onwards = self.emit(Inst::Jump { to: PENDING }, arm.pattern.span);
            for at in tests.failures {
                self.patch(at, failing);
            }
            vec![onwards]
        };
        for slot in held {
            self.frame.free(slot);
        }
        pending
    }

    /// Which values of a scrutinee of type `ty` an arm's pattern can match.
    fn reach_of(&mut self, ty: &Ty, pattern: &Pattern) -> Reach {
        match &pattern.kind {
            PatternKind::Wildcard => Reach::Any,
            PatternKind::Binding(name) => match self.bare_case(ty, name) {
                Some(index) => Reach::Case { index },
                None => Reach::Any,
            },
            PatternKind::Variant { path, .. } => {
                let case = path
                    .last()
                    .map(|segment| segment.node.as_str())
                    .unwrap_or("");
                match shapes::case_at(self.checked, self.module, ty, case) {
                    Some((index, _)) => Reach::Case { index },
                    None => {
                        self.errors.push(gap::gap(
                            "a pattern naming a case of another type",
                            pattern.span,
                        ));
                        Reach::Any
                    }
                }
            }
            PatternKind::Literal(_) => {
                self.errors
                    .push(gap::gap("a literal pattern over an enum", pattern.span));
                Reach::Any
            }
        }
    }

    /// The case a bare name means rather than binds.
    ///
    /// `None` is the one case in the language written where a name would be,
    /// and the interpreter reads it as a case only for an `Option` — so a
    /// declared enum with a case called `None` still binds, and this answers
    /// the same way.
    fn bare_case(&self, ty: &Ty, name: &str) -> Option<u32> {
        if name != "None" || !matches!(ty, Ty::Option(_)) {
            return None;
        }
        shapes::case_at(self.checked, self.module, ty, name).map(|(index, _)| index)
    }

    /// Whether a pattern can fail to match a value it is given.
    fn refutable(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard => false,
            PatternKind::Binding(_) => false,
            PatternKind::Literal(_) | PatternKind::Variant { .. } => true,
        }
    }

    /// Whether a pattern names anything.
    fn binds(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) => false,
            PatternKind::Binding(_) => true,
            PatternKind::Variant { payload, .. } => payload.iter().any(|sub| self.binds(sub)),
        }
    }

    /// Emits what decides whether `pattern` matches the value in `subject`.
    ///
    /// `dispatched` says the case has already been established — by the
    /// switch, at the top of an enum `match` — so only the payload is still
    /// in question. Nothing is bound here: bindings come after every test
    /// has passed, so that a failing arm leaves no name holding anything.
    fn test(
        &mut self,
        pattern: &Pattern,
        subject: Slot,
        ty: &Ty,
        dispatched: bool,
        tests: &mut Tests,
    ) {
        let span = pattern.span;
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                let Some(index) = self.bare_case(ty, name) else {
                    return;
                };
                if !dispatched {
                    self.test_case(subject, index, span, tests);
                }
            }
            PatternKind::Literal(literal) => {
                let value = self.expr(literal);
                let on = compare_of(self.frame.repr(subject));
                let cond = self.frame.alloc(Repr::Bool);
                self.emit(
                    Inst::Cmp {
                        on,
                        op: CmpOp::Eq,
                        dst: cond,
                        a: subject,
                        b: value.slot,
                    },
                    span,
                );
                // The literal is dead the moment it has been compared, on
                // both paths out of the branch below, so it is ended here
                // rather than held for the failure block.
                self.release(value, span);
                tests
                    .failures
                    .push(self.emit(Inst::BranchFalse { cond, to: PENDING }, span));
                self.frame.free(cond);
            }
            PatternKind::Variant { path, payload } => {
                let case = path
                    .last()
                    .map(|segment| segment.node.as_str())
                    .unwrap_or("");
                let Some((index, types)) = shapes::case_at(self.checked, self.module, ty, case)
                else {
                    self.errors
                        .push(gap::gap("a pattern naming a case of another type", span));
                    return;
                };
                if !dispatched {
                    self.test_case(subject, index, span, tests);
                }
                for (at, sub) in payload.iter().enumerate() {
                    if !self.refutable(sub) {
                        continue;
                    }
                    let Some(sub_ty) = types.get(at) else {
                        continue;
                    };
                    let Some(repr) = shapes::word_of(sub_ty) else {
                        self.errors.push(super::describe(sub_ty, span));
                        continue;
                    };
                    let word = self.frame.alloc(repr);
                    self.emit(
                        Inst::GetWord {
                            dst: word,
                            obj: subject,
                            at: 1 + at as u32,
                        },
                        span,
                    );
                    let sub_ty = sub_ty.clone();
                    self.test(sub, word, &sub_ty, false, tests);
                    // A payload word that is a reference stays live across
                    // every branch the sub-pattern emitted, so it is the
                    // failure block's to clear rather than this line's.
                    if matches!(repr, Repr::Ref | Repr::Addr) {
                        tests.held.push(word);
                    } else {
                        self.frame.free(word);
                    }
                }
            }
        }
    }

    /// The one test an enum case needs: word 0 against the index.
    fn test_case(&mut self, subject: Slot, index: u32, span: Span, tests: &mut Tests) {
        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: tag,
                obj: subject,
                at: 0,
            },
            span,
        );
        let wanted = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: wanted,
                value: index as i64,
            },
            span,
        );
        let cond = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: cond,
                a: tag,
                b: wanted,
            },
            span,
        );
        self.frame.free(wanted);
        self.frame.free(tag);
        tests
            .failures
            .push(self.emit(Inst::BranchFalse { cond, to: PENDING }, span));
        self.frame.free(cond);
    }

    /// Names what a pattern binds, once every test has passed.
    ///
    /// A binding's slot belongs to the arm's scope, so it is cleared when
    /// the arm ends — which is the same event that ends a `let`'s.
    fn bind(&mut self, pattern: &Pattern, subject: Slot, ty: &Ty, span: Span) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
            PatternKind::Binding(name) => {
                if self.bare_case(ty, name).is_some() {
                    return;
                }
                let repr = self.frame.repr(subject);
                let slot = self.frame.alloc(repr);
                self.emit(
                    Inst::Move {
                        dst: slot,
                        src: subject,
                    },
                    span,
                );
                self.frame.own(slot);
                self.frame.bind(name, slot);
            }
            PatternKind::Variant { path, payload } => {
                let case = path
                    .last()
                    .map(|segment| segment.node.as_str())
                    .unwrap_or("");
                let Some((_, types)) = shapes::case_at(self.checked, self.module, ty, case) else {
                    return;
                };
                for (at, sub) in payload.iter().enumerate() {
                    if !self.binds(sub) {
                        continue;
                    }
                    let Some(sub_ty) = types.get(at).cloned() else {
                        continue;
                    };
                    let Some(repr) = shapes::word_of(&sub_ty) else {
                        continue;
                    };
                    let word = self.frame.alloc(repr);
                    self.emit(
                        Inst::GetWord {
                            dst: word,
                            obj: subject,
                            at: 1 + at as u32,
                        },
                        span,
                    );
                    match &sub.kind {
                        // The word the payload holds *is* what the name
                        // means, so the slot it was read into becomes the
                        // binding rather than being copied out of.
                        PatternKind::Binding(name) if self.bare_case(&sub_ty, name).is_none() => {
                            self.frame.own(word);
                            self.frame.bind(name, word);
                        }
                        _ => {
                            self.bind(sub, word, &sub_ty, span);
                            self.release(Val::temp(word), span);
                        }
                    }
                }
            }
        }
    }

    /// Fills in a [`Inst::Switch`]'s table, once the arms it jumps into have
    /// been laid out.
    fn place_table(&mut self, at: Pc, id: TableId) {
        match &mut self.code[at as usize] {
            Inst::Switch { table, .. } => *table = id,
            other => unreachable!("placed a table on a {other:?}, which is not a switch"),
        }
    }
}

/// What a comparison of two words of this kind is comparing.
///
/// A reference compares as a string, because a string is the only heap value
/// the language lets a literal pattern name.
fn compare_of(repr: Repr) -> Compare {
    match repr {
        Repr::Float => Compare::Float,
        Repr::Bool => Compare::Bool,
        Repr::Ref => Compare::Str,
        _ => Compare::Int,
    }
}
