//! The standard library's core intrinsics, lowered to the instructions that
//! perform them.
//!
//! [ADR 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! moves a collection's public algorithm into Cove and keeps beneath it only
//! the smallest representation-dependent operation, which the standard library
//! spells `core.<name>(...)` — `cove_schema::builtins::CORE_INTRINSICS` is the
//! table. The checker admits such a call only inside a standard-library module,
//! and this is the lowering's half: each entry becomes run instructions in the
//! frame the call is written in, and **never** an [`Inst::CallBuiltin`]. A core
//! intrinsic is not a name for the machine to dispatch on; it is the operation
//! the name stands for.
//!
//! So nothing downstream of this file learns that a public method moved. The
//! verifier, both encoders and the native code generators see the same
//! instruction they already see for an `Array`'s length, and the function the
//! standard library wraps around it is small enough that `super::inline`
//! expands it where it is called.

use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes;
use super::{Body, Dest};
use crate::inst::Inst;

impl Body<'_> {
    /// `core.name(args)`, written in a standard-library module.
    ///
    /// Asked by [`Body::call_qualified`] before anything else a qualified
    /// name could be, and only for a module `cove_sema::stdlib` declares —
    /// the same question the checker asked before it typed the call, so a
    /// program the checker admitted cannot reach the gap below with a name it
    /// declares.
    pub(super) fn core_call(
        &mut self,
        expr: &Expr,
        name: &str,
        args: &[Arg],
        want: Option<Dest>,
    ) -> Val {
        match (name, args) {
            ("byteLength", [text]) => self.core_byte_length(expr, &text.value, want),
            _ => self.gap(&format!("`core.{name}`"), expr),
        }
    }

    /// `core.byteLength(text)`: the string object's header length, which is
    /// its length in bytes.
    ///
    /// [`Inst::Len`] and nothing else. The header word it reads is the one an
    /// `Array`'s `length()` reads, and for a `String` the count in it is bytes —
    /// so the machine's `LEN` arm, with its null refusal, is already the whole
    /// of this operation, and the native code generators already emit it.
    fn core_byte_length(&mut self, expr: &Expr, text: &Expr, want: Option<Dest>) -> Val {
        let obj = self.expr(text);
        let dst = self.answer_at(want, shapes::INT);
        self.emit(
            Inst::Len {
                dst: dst.slot,
                obj: obj.slot,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        dst
    }
}
