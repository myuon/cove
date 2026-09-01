//! Statements, and the blocks they sit in.
//!
//! A block is where a scope begins and ends, and the scope is what decides
//! when a local's slot goes back on the free list — and, once there is a
//! heap, when the reference in it is cleared. Nothing else in the lowering
//! creates one.

use cove_syntax::ast::{Block, Stmt, StmtKind};

use super::gap;
use super::Body;
use crate::inst::{Inst, Slot};
use crate::repr::Repr;

impl Body<'_> {
    /// A block in a scope of its own: everything it declares dies with it.
    pub(super) fn scoped_block(&mut self, block: &Block, dst: Option<Slot>) {
        self.frame.push_scope();
        self.block(block, dst);
        let clears = self.frame.pop_scope();
        self.clear(&clears, block.span);
    }

    /// The statements of a block and, when the surrounding form wants one,
    /// its answer.
    ///
    /// `dst` is `None` where the block is being run for its effects. That is
    /// not the same as writing to a slot nobody reads: a block with no
    /// destination does not evaluate its tail into one, so an expression
    /// statement costs no `Unit` nobody looks at.
    ///
    /// The scope is the caller's, because a function body's scope has to
    /// hold the parameters as well and the block did not declare those.
    pub(super) fn block(&mut self, block: &Block, dst: Option<Slot>) {
        for stmt in &block.statements {
            self.stmt(stmt);
        }
        match (&block.tail, dst) {
            (Some(tail), Some(dst)) => {
                let value = self.expr(tail);
                self.store(dst, &value, tail);
                self.release(value, tail.span);
            }
            (Some(tail), None) => self.discard(tail),
            (None, Some(dst)) => {
                // A block with no tail answers `()`. Where the surrounding
                // form wants some other kind of word, the checker only let
                // this block stand because it never completes — every path
                // out of it left the frame or the loop — and then there is
                // nothing here to write.
                if self.frame.repr(dst) == Repr::Unit {
                    self.emit(Inst::Unit { dst }, block.span);
                }
            }
            (None, None) => {}
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                name, ty, value, ..
            } => {
                let value_slot = self.expr(value);
                // An annotation is a written type, and `let it: dyn Trait =
                // concrete` is one of the four places the language's one
                // implicit conversion happens. Only the `dyn` case is read
                // off the annotation: what a name means is the checker's to
                // say, and every other annotation is one the checker already
                // agreed with the initialiser about.
                let value_slot = match ty.as_ref().and_then(|ty| self.written_dyn(ty)) {
                    Some(erased) => self.erase(value_slot, value, &erased),
                    None => value_slot,
                };
                // A binding whose initialiser made a temporary takes that
                // slot over instead of copying out of it: the temporary is
                // dead the moment the binding is alive, so a `Move` between
                // two slots of the same kind would copy a word for nothing.
                // A borrowed slot cannot be taken over, because the binding
                // it belongs to is still in scope and a `var` local must not
                // alias one.
                //
                // `let` and `var` reach the same slot either way. What `var`
                // decides is whether the checker permits an assignment to
                // the name, and that has already been decided; a local of
                // either kind is one word of this frame.
                let slot = if value_slot.temp {
                    value_slot.slot
                } else {
                    let repr = self.frame.repr(value_slot.slot);
                    let slot = self.frame.alloc(repr);
                    self.emit(
                        Inst::Move {
                            dst: slot,
                            src: value_slot.slot,
                        },
                        stmt.span,
                    );
                    slot
                };
                self.frame.own(slot);
                self.frame.bind(&name.node, slot);
            }
            StmtKind::Expr(expr) => self.discard(expr),
            StmtKind::Item(_) => self
                .errors
                .push(gap::gap("a declaration inside a body", stmt.span)),
        }
    }
}
