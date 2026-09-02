//! Statements, and the blocks they sit in.
//!
//! A block is where a scope begins and ends, and the scope is what decides
//! when a local's run of slots goes back on the free list and when the
//! references in it are cleared. Nothing else in the lowering creates one.

use cove_syntax::ast::{Block, ItemKind, Stmt, StmtKind};

use super::gap;
use super::shapes;
use super::{Body, Dest};
use crate::inst::Inst;

impl Body<'_> {
    /// A block in a scope of its own: everything it declares dies with it.
    pub(super) fn scoped_block(&mut self, block: &Block, dst: Option<Dest>) {
        self.frame.push_scope();
        self.block(block, dst);
        let clears = self.frame.pop_scope();
        self.clear(&clears, block.span);
    }

    /// The statements of a block and, when the surrounding form wants one,
    /// its answer.
    ///
    /// `dst` is `None` where the block is being run for its effects. That is
    /// not the same as writing to a location nobody reads: a block with no
    /// destination does not evaluate its tail into one, so an expression
    /// statement costs no `Unit` nobody looks at.
    ///
    /// The scope is the caller's, because a function body's scope has to
    /// hold the parameters as well and the block did not declare those.
    pub(super) fn block(&mut self, block: &Block, dst: Option<Dest>) {
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
                // form wants some other kind of value, the checker only let
                // this block stand because it never completes — every path
                // out of it left the frame or the loop — and then there is
                // nothing here to write.
                if dst.layout == shapes::UNIT {
                    self.emit(Inst::Unit { dst: dst.slot }, block.span);
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
                let held = self.expr(value);
                // An annotation is a written type, and `let it: dyn Trait =
                // concrete` is one of the four places the language's one
                // implicit conversion happens. Only the `dyn` case is read
                // off the annotation: what a name means is the checker's to
                // say, and every other annotation is one the checker already
                // agreed with the initialiser about.
                let held = match ty.as_ref().and_then(|ty| self.written_dyn(ty)) {
                    Some(erased) => self.erase(held, value, &erased),
                    None => held,
                };
                // A binding whose initialiser made a temporary takes that
                // run over instead of copying out of it: the temporary is
                // dead the moment the binding is alive, and ADR 0001's
                // shallow copy of a value nothing else can observe is the
                // value itself. A *borrowed* location cannot be taken over,
                // because the binding it belongs to is still in scope — so
                // that is where the copy is emitted, and correctness never
                // depends on proving the other case.
                //
                // `let` and `var` reach the same location either way. What
                // `var` decides is whether the checker permits an assignment
                // to the name, and that has already been decided.
                let layout = held.layout;
                let slot = if held.temp {
                    // The scope owns the run from here, so it is the scope
                    // that clears it: the binding stops being a temporary
                    // this body is holding at the moment the name is given
                    // to it.
                    self.forget(held.slot);
                    held.slot
                } else {
                    let slot = self.alloc(layout);
                    self.copy(slot, held.slot, layout, stmt.span);
                    slot
                };
                let width = self.width(layout);
                self.frame.own(slot, layout, width);
                self.frame.bind(&name.node, slot, layout);
            }
            StmtKind::Expr(expr) => self.discard(expr),
            // A local `fn` is an ordinary closure the body writes, and the
            // name it declares is a binding of this scope like any other:
            // see [`Body::local_fn`]. Nothing else can stand here yet.
            StmtKind::Item(item) => match &item.kind {
                ItemKind::Fn(decl) => self.local_fn(decl),
                _ => self
                    .errors
                    .push(gap::gap("a declaration inside a body", stmt.span)),
            },
        }
    }
}
