//! `match`, and the patterns its arms are written with.
//!
//! # An enum dispatches; everything else compares
//!
//! A `match` over an enum hands word 0 of the value — the discriminant — to
//! [`Inst::Switch`], so an enum with twenty cases costs one indexed jump
//! rather than twenty comparisons. The discriminant needs no read: an enum
//! is inline, so the word is already in the frame and the switch names the
//! location itself.
//!
//! A `match` over anything else — an `Int`, a `String`, a `Bool` — is a
//! chain of comparisons, because there is no index to switch on and the
//! arms' literals are values rather than a dense numbering.
//!
//! The table has a default even where the checker proved the `match`
//! exhaustive, because the machine does not take the lowering's word for
//! what is in a word. The default, and the end of a comparison chain, is
//! [`Inst::Trap`].
//!
//! # A test reads nothing, so it leaves nothing behind
//!
//! A case's payload is *part of the value*: the parts of case `i` are at
//! `base + 1 + Part::at`, and a sub-pattern is tested against that location
//! directly. No word is copied out to be looked at, so a failing arm has
//! nothing to clear on its way to the next one — which is a whole class of
//! liveness bug the inline representation removes rather than solves.
//!
//! What a pattern *binds* is a copy, emitted only after every test has
//! passed. It has to be a copy: the binding outlives the arm's tests and is
//! cleared when the arm's scope ends, and clearing a borrowed sub-location
//! would zero part of the value being matched.
//!
//! # An arm's body is emitted once
//!
//! The switch's targets and the arms' failure edges are patched afterwards,
//! which is what lets every arm's body be emitted exactly once even though
//! several cases may reach the same arm. A `_` arm is the tail of every
//! case's chain; an arm for one case is in that case's chain alone, so where
//! it goes when its payload does not match is a fact known statically.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{MatchArm, Pattern, PatternKind};

use super::gap;
use super::shapes;
use super::{Body, Dest, PENDING};
use crate::inst::{CmpOp, Compare, Inst, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::{Table, TableId};
use crate::repr::Repr;

/// The table id a [`Inst::Switch`] carries until its arms have been laid out
/// and their entry points are known.
pub(super) const UNPLACED: TableId = TableId(u32::MAX);

/// Which values of the scrutinee an arm can match.
enum Reach {
    /// Every one of them: `_`, or a name the arm binds.
    Any,
    /// One case, which the switch dispatches to. The arm may still fail on
    /// the case's payload, and the jumps it leaves behind when it does are
    /// what the chain for that case is patched from.
    Case { index: u32 },
}

impl Body<'_> {
    /// `match scrutinee { ... }`, with or without an answer.
    pub(super) fn match_expr(
        &mut self,
        scrutinee: &cove_syntax::ast::Expr,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Dest>,
    ) {
        let Some(ty) = self.settled_ty(scrutinee) else {
            return;
        };
        let subject = self.expr(scrutinee);
        if shapes::enum_cases(self.checked, self.module, &ty).is_some() {
            self.match_enum(&ty, subject.slot, subject.layout, arms, span, dst);
        } else {
            self.match_chain(&ty, subject.slot, subject.layout, arms, span, dst);
        }
        self.release(subject, span);
    }

    /// A `match` over an enum: one indexed jump, then one arm.
    fn match_enum(
        &mut self,
        ty: &Ty,
        subject: Slot,
        layout: LayoutId,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Dest>,
    ) {
        let cases = shapes::enum_cases(self.checked, self.module, ty).expect("an enum-shaped type");
        let mut reach = Vec::with_capacity(arms.len());
        for arm in arms {
            reach.push(self.reach_of(ty, &arm.pattern));
        }

        // The discriminant is word 0 of the value, so the switch reads the
        // location itself rather than a copy of it.
        let switch = self.emit(
            Inst::Switch {
                on: subject,
                table: UNPLACED,
            },
            span,
        );

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
                    Reach::Case { index: at } if *at as usize == case => chain.push(index),
                    Reach::Case { .. } => {}
                }
            }
        }

        let mut entries = vec![0; arms.len()];
        let mut failures: Vec<Vec<Pc>> = vec![Vec::new(); arms.len()];
        let mut ends = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            entries[index] = self.here();
            // The switch has already established which case the value is
            // in, so only the payload is still in question.
            let dispatched = matches!(reach[index], Reach::Case { .. });
            failures[index] = self.arm(arm, ty, subject, layout, dst, dispatched, &mut ends);
        }

        let trap = self.here();
        let message = self.string("no `match` arm covers this value");
        self.emit(Inst::Trap { message }, span);

        for (index, pending) in failures.into_iter().enumerate() {
            let Reach::Case { index: case } = reach[index] else {
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
        layout: LayoutId,
        arms: &[MatchArm],
        span: Span,
        dst: Option<Dest>,
    ) {
        let mut ends = Vec::new();
        for arm in arms {
            let failures = self.arm(arm, ty, subject, layout, dst, false, &mut ends);
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
    #[allow(clippy::too_many_arguments)]
    fn arm(
        &mut self,
        arm: &MatchArm,
        ty: &Ty,
        subject: Slot,
        layout: LayoutId,
        dst: Option<Dest>,
        dispatched: bool,
        ends: &mut Vec<Pc>,
    ) -> Vec<Pc> {
        self.frame.push_scope();
        let mut failures = Vec::new();
        self.test(&arm.pattern, subject, layout, ty, dispatched, &mut failures);
        self.bind(&arm.pattern, subject, layout, ty, arm.pattern.span);
        match dst {
            Some(dst) => {
                let value = self.expr(&arm.body);
                self.store(dst, &value, &arm.body);
                self.release(value, arm.body.span);
            }
            None => self.discard(&arm.body),
        }
        let at = self.here();
        let clears = self.frame.pop_scope(at);
        self.clear(&clears, arm.span);
        ends.push(self.emit(Inst::Jump { to: PENDING }, arm.span));
        failures
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

    /// Emits what decides whether `pattern` matches the value at `subject`.
    ///
    /// `dispatched` says the case has already been established — by the
    /// switch, at the top of an enum `match` — so only the payload is still
    /// in question. Nothing is bound here: bindings come after every test
    /// has passed, so that a failing arm leaves no name holding anything.
    #[allow(clippy::too_many_arguments)]
    fn test(
        &mut self,
        pattern: &Pattern,
        subject: Slot,
        layout: LayoutId,
        ty: &Ty,
        dispatched: bool,
        failures: &mut Vec<Pc>,
    ) {
        let span = pattern.span;
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                let Some(index) = self.bare_case(ty, name) else {
                    return;
                };
                if !dispatched {
                    self.test_case(subject, index, span, failures);
                }
            }
            PatternKind::Literal(literal) => {
                let value = self.expr(literal);
                let on = compare_of(self.frame.repr(subject));
                let cond = self.temp(shapes::BOOL);
                self.emit(
                    Inst::Cmp {
                        on,
                        op: CmpOp::Eq,
                        dst: cond.slot,
                        a: subject,
                        b: value.slot,
                    },
                    span,
                );
                // The literal is dead the moment it has been compared, on
                // both paths out of the branch below.
                self.release(value, span);
                failures.push(self.emit(
                    Inst::BranchFalse {
                        cond: cond.slot,
                        to: PENDING,
                    },
                    span,
                ));
                self.give_back(cond.slot, cond.layout);
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
                    self.test_case(subject, index, span, failures);
                }
                let Some((parts, _)) = self.case_of(layout, index) else {
                    return;
                };
                for (at, sub) in payload.iter().enumerate() {
                    if !self.refutable(sub) {
                        continue;
                    }
                    let (Some(sub_ty), Some(part)) = (types.get(at), parts.get(at)) else {
                        continue;
                    };
                    let sub_ty = sub_ty.clone();
                    // The payload is part of the value, so the sub-pattern
                    // is tested where it already is.
                    self.test(
                        sub,
                        subject + 1 + part.at,
                        part.layout,
                        &sub_ty,
                        false,
                        failures,
                    );
                }
            }
        }
    }

    /// The one test an enum case needs: word 0 against the index.
    fn test_case(&mut self, subject: Slot, index: u32, span: Span, failures: &mut Vec<Pc>) {
        let wanted = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: wanted.slot,
                value: index as i64,
            },
            span,
        );
        let cond = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: cond.slot,
                a: subject,
                b: wanted.slot,
            },
            span,
        );
        self.give_back(wanted.slot, wanted.layout);
        failures.push(self.emit(
            Inst::BranchFalse {
                cond: cond.slot,
                to: PENDING,
            },
            span,
        ));
        self.give_back(cond.slot, cond.layout);
    }

    /// Names what a pattern binds, once every test has passed.
    ///
    /// A binding is a copy of the words it names. It has to be: the binding
    /// belongs to the arm's scope and is cleared when that scope ends, and
    /// clearing a borrowed part of the value being matched would zero the
    /// value itself.
    fn bind(&mut self, pattern: &Pattern, subject: Slot, layout: LayoutId, ty: &Ty, span: Span) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
            PatternKind::Binding(name) => {
                if self.bare_case(ty, name).is_some() {
                    return;
                }
                let slot = self.alloc(layout);
                self.copy(slot, subject, layout, span);
                let width = self.width(layout);
                self.frame.own(slot, layout, width);
                let at = self.here();
                self.frame.bind(name, slot, layout, at);
            }
            PatternKind::Variant { path, payload } => {
                let case = path
                    .last()
                    .map(|segment| segment.node.as_str())
                    .unwrap_or("");
                let Some((index, types)) = shapes::case_at(self.checked, self.module, ty, case)
                else {
                    return;
                };
                let Some((parts, _)) = self.case_of(layout, index) else {
                    return;
                };
                for (at, sub) in payload.iter().enumerate() {
                    if !self.binds(sub) {
                        continue;
                    }
                    let (Some(sub_ty), Some(part)) = (types.get(at), parts.get(at)) else {
                        continue;
                    };
                    let sub_ty = sub_ty.clone();
                    self.bind(sub, subject + 1 + part.at, part.layout, &sub_ty, span);
                }
            }
        }
    }

    /// Fills in a [`Inst::Switch`]'s table, once the arms it jumps into have
    /// been laid out.
    pub(super) fn place_table(&mut self, at: Pc, id: TableId) {
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
