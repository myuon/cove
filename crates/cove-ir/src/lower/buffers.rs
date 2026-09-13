//! `ByteBuffer`: ADR 0052's growable packed byte run, lowered inline.
//!
//! [ADR 0052](../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)
//! gives the IR four instructions for a byte builder — `alloc-builder`,
//! `append-byte`, `append-bytes`, `finish-builder` — and one sentence about how
//! they may be reached: *"It must not turn one appended byte or one element into
//! a `call-builtin` merely to reuse the source API."* This module is that
//! sentence. Every operation `cove_schema::builtins::BYTE_BUFFER` declares
//! becomes the instruction that performs it, in the frame the call site is
//! already in, and nothing here goes through [`crate::Inst::CallBuiltin`].
//!
//! It is the same split `cove_ir::lower::collections` makes for a sequence and
//! that `String.byteAt` makes for a character, and it is made for the reason
//! those are: a builtin call is a dispatch and an argument list, which is worth
//! paying for `split` or `replace` and is *most* of what an appended byte costs.
//! A formatter's inner loop is appends.
//!
//! # The owner is a reference, so nothing here needs an address
//!
//! A `ByteBuffer` value is one word: the address of the two-word owner, which
//! is a heap object exactly as a `Vector` header is. That is what makes a
//! `mutating` method's lowering unremarkable — `appendByte`, `appendSlice` and
//! `finish` all write *through* the reference, so the receiver is evaluated to a
//! value like any other receiver and the machine reaches the owner's words from
//! it. `Vector.push` is lowered the same way and for the same reason: the thing
//! being mutated is the object, not the slot holding its address.
//!
//! This is precisely what ADR 0052 bought. A growth replaces the store beneath
//! the owner and the owner does not move, so a buffer copied into a callee's
//! frame, or passed as a `var` argument through a recursion, keeps naming the
//! same bytes while the store under it doubles. `std.stringbuilder` is the
//! typed wrapper a program actually holds; nothing about it reaches this file,
//! because a one-field struct over a reference is one reference word.
//!
//! # `length()` is a field read and not a call
//!
//! The logical length is payload word 0 of the owner — [`shapes::BUFFER_LEN`] —
//! which is where a `Vector` keeps its own and is read here exactly as
//! `Vector.length` reads that one. What it must *not* read is the store's
//! header length: that is the capacity, and ADR 0052's "capacity is not an
//! Array length" is this one distinction holding.

use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, BUFFER_LEN};
use super::{Body, Dest};
use crate::inst::Inst;

impl Body<'_> {
    /// A method call on a `ByteBuffer`.
    ///
    /// Four names, four instructions, and a gap for anything else — because
    /// `cove_schema::builtins::BYTE_BUFFER` declares exactly these and a name
    /// the checker admitted that this does not emit would be a refusal at run
    /// time where a gap naming the work belongs.
    pub(super) fn buffer_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        name: &str,
        args: &[Arg],
        want: Option<Dest>,
    ) -> Val {
        match (name, args.len()) {
            ("length", 0) => self.buffer_length(expr, base, want),
            ("appendByte", 1) => self.buffer_append_byte(expr, base, &args[0].value, want),
            ("appendSlice", 3) => self.buffer_append_slice(expr, base, args, want),
            ("finish", 0) => self.buffer_finish(expr, base, want),
            _ => self.gap(&format!("`ByteBuffer.{name}`"), expr),
        }
    }

    /// `ByteBuffer.allocate(capacity)`, as [`Inst::AllocBuffer`].
    ///
    /// It is an instruction rather than a builtin call for `Vector.of`'s
    /// reason: both of the layouts it allocates — the owner and its store —
    /// are program-wide constants the machine already holds, so there is
    /// nothing for a call site to say and nothing to dispatch on.
    ///
    /// `capacity` is a hint. A store too small for what is appended grows, and
    /// one larger than the final length gives the tail back at
    /// [`Inst::FinishBuffer`], so no value here can change what a program
    /// answers.
    pub(super) fn buffer_allocate(&mut self, expr: &Expr, args: &[Arg], want: Option<Dest>) -> Val {
        if args.len() != 1 {
            return self.gap(
                "`ByteBuffer.allocate` with the wrong number of arguments",
                expr,
            );
        }
        let capacity = self.expr(&args[0].value);
        let dst = self.answer_at(want, shapes::BYTE_BUFFER);
        self.emit(
            Inst::AllocBuffer {
                dst: dst.slot,
                capacity: capacity.slot,
            },
            expr.span,
        );
        self.release(capacity, expr.span);
        dst
    }

    /// `buffer.length()`: payload word 0 of the owner.
    ///
    /// See the module doc for why this is the owner's word and never the
    /// store's header.
    fn buffer_length(&mut self, expr: &Expr, base: &Expr, want: Option<Dest>) -> Val {
        let obj = self.expr(base);
        let dst = self.answer_at(want, shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: dst.slot,
                obj: obj.slot,
                at: BUFFER_LEN,
                layout: shapes::INT,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        dst
    }

    /// `buffer.appendByte(value)`, as [`Inst::AppendByte`].
    ///
    /// The answer is `()` and the instruction writes no destination, so the
    /// unit is written separately — into the location the surrounding form asked
    /// for, which is why `want` is carried this far. A unit built in a temporary
    /// and copied out is a copy per append, and `crates/cove-cli/tests/copies.rs`
    /// counts every one of them.
    fn buffer_append_byte(
        &mut self,
        expr: &Expr,
        base: &Expr,
        value: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let buffer = self.expr(base);
        let value = self.expr(value);
        self.emit(
            Inst::AppendByte {
                buffer: buffer.slot,
                value: value.slot,
            },
            expr.span,
        );
        self.release(value, expr.span);
        self.release(buffer, expr.span);
        self.unit_answer(expr, want)
    }

    /// `buffer.appendSlice(text, from, to)`, as [`Inst::AppendBytes`].
    ///
    /// Four operands and an encoded instruction with room for three, so the row
    /// goes in the argument pool the way a call's does — `[buffer, src, from,
    /// to]`, in that order, each carrying its own layout so the bytecode
    /// verifier checks them by the rule it checks a call's arguments by. See
    /// [`Inst::AppendBytes`] for why this is not two instructions.
    ///
    /// The range is checked by the machine, in `String.sliceBytes`'s words, and
    /// a range that fails those checks stops the run. Nothing is checked here:
    /// the machine is already holding the header the bounds are read from, and
    /// ADR 0052 requires the refusal to be the same refusal in the same
    /// sentence rather than a second one written at a second altitude.
    fn buffer_append_slice(
        &mut self,
        expr: &Expr,
        base: &Expr,
        args: &[Arg],
        want: Option<Dest>,
    ) -> Val {
        let buffer = self.expr(base);
        let text = self.expr(&args[0].value);
        let from = self.expr(&args[1].value);
        let to = self.expr(&args[2].value);
        let row = self
            .pool
            .args
            .intern(vec![buffer.arg(), text.arg(), from.arg(), to.arg()]);
        self.emit(Inst::AppendBytes { args: row }, expr.span);
        self.release(to, expr.span);
        self.release(from, expr.span);
        self.release(text, expr.span);
        self.release(buffer, expr.span);
        self.unit_answer(expr, want)
    }

    /// The `()` an append answers, in the location the surrounding form asked
    /// for.
    ///
    /// [`Body::unit_value`] with a destination, which is the whole difference:
    /// an append is a statement in almost every program that writes one, and a
    /// `Unit` written into a temporary that is then copied into the location the
    /// answer belongs in is an instruction per append that nothing reads.
    fn unit_answer(&mut self, expr: &Expr, want: Option<Dest>) -> Val {
        let dst = self.answer_at(want, shapes::UNIT);
        self.emit(Inst::Unit { dst: dst.slot }, expr.span);
        dst
    }

    /// `buffer.finish()`, as [`Inst::FinishBuffer`].
    ///
    /// The live prefix is validated once and its store is relabelled down from
    /// the capacity to the logical length, so the `String` this answers *is*
    /// the bytes that were appended and not a copy of them. The owner is
    /// emptied by the instruction, which is why `cove_sema::unique` has to have
    /// proved that nothing else holds it — a second holder would be left
    /// naming a buffer whose store is gone, and the machine's liveness check
    /// would refuse it rather than read it as empty.
    fn buffer_finish(&mut self, expr: &Expr, base: &Expr, want: Option<Dest>) -> Val {
        let buffer = self.expr(base);
        let dst = self.answer_at(want, shapes::STR);
        self.emit(
            Inst::FinishBuffer {
                dst: dst.slot,
                buffer: buffer.slot,
            },
            expr.span,
        );
        self.release(buffer, expr.span);
        dst
    }
}

/// Whether `head.name(...)` is `ByteBuffer.allocate(capacity)`.
///
/// The type the checker settled is asked as well as the name, for the reason
/// [`super::methods::associated`] asks it: a module or an enum can be written
/// in front of a `.` too, and only this call answers a `ByteBuffer` under this
/// name.
pub(super) fn namespace_allocate(head: &str, name: &str, ty: &Ty) -> bool {
    head == cove_schema::builtins::BYTE_BUFFER.name
        && name == "allocate"
        && matches!(ty, Ty::ByteBuffer)
}
