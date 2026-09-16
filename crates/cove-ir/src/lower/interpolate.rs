//! A string assembled from pieces, one append per piece, in the order the
//! pieces are evaluated.
//!
//! `"a{x}b"` is a byte buffer — the one [ADR 0052] decides and
//! `std.stringbuilder`'s `StringBuilder` wraps — allocated before the first
//! piece, appended to **immediately after each piece is evaluated**, and
//! finished into the `String` the expression answers. The order is the point
//! (#389): a piece is rendered when it is evaluated, so a later piece that
//! mutates what an earlier one showed cannot reach back into its text. It used
//! to be one variadic `String.interpolate` over every piece, called after all
//! of them had run, and `"{v} {v.pop()}"` showed the vector after the pop.
//!
//! # Run instructions, not `StringBuilder` calls
//!
//! The appends are emitted here as the run instructions `super::core` lowers
//! the `core.bytes*` intrinsics to, rather than as calls to `StringBuilder`'s
//! methods for `super::inline` to expand. The instructions are what those
//! methods become anyway, and writing them directly needs no synthetic call,
//! no resolution of a standard-library declaration from inside an expression
//! the program wrote, and no uniqueness proof for `finish` — the buffer is a
//! temporary nothing else can name (#403).
//!
//! # One append per piece, chosen by the piece's type
//!
//! [`Body::append_piece`] is a single `match` on the checked type of a piece:
//!
//! - a `String` is appended whole, as the byte `growable-extend` a
//!   `StringBuilder.append` is — no rendering and no temporary string;
//! - an `Int` is formatted straight into the buffer by `Int.renderInto`;
//! - anything else is rendered into the buffer by `Value.renderInto`, which is
//!   the runtime's one layout-directed rendering walk, so an `Error`, an
//!   opaque value, a `Range`, a collection, a box and a closure all show
//!   exactly as they did.
//!
//! A literal run of text is not a piece and never reaches that match: its
//! bytes are known here, so it is a byte push when it is one byte and an
//! extend from the string pool otherwise.
//!
//! [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md

use cove_diag::Span;
use cove_sema::typeck::Ty;

use super::frame::Val;
use super::{shapes, Body, Dest};
use crate::inst::{Inst, Storage, Validation};
use crate::intrinsic::Intrinsic;

/// The bytes a buffer is sized for per piece whose text is not known until it
/// runs, on top of the literal text's own.
///
/// A capacity is a hint: too small costs a growth, too large is handed back
/// when the buffer is finished. Sixteen is the runtime's own floor for a byte
/// store, and it holds any `Int` and the short strings a message interpolates.
const PIECE_ALLOWANCE: usize = 16;

/// A string under assembly: the buffer it is appended to, and the `0` every
/// whole-string extend starts its range at.
pub(super) struct Assembly {
    buffer: Val,
    zero: Option<Val>,
}

impl Body<'_> {
    /// A byte buffer with room for `literal` bytes and `pieces` values of
    /// text not known yet.
    pub(super) fn assembly_open(&mut self, literal: usize, pieces: usize, span: Span) -> Assembly {
        let capacity = literal.saturating_add(pieces.saturating_mul(PIECE_ALLOWANCE));
        let size = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: size.slot,
                value: i64::try_from(capacity).unwrap_or(i64::MAX),
            },
            span,
        );
        let buffer = self.temp(shapes::BYTE_BUFFER);
        self.emit(
            Inst::GrowableAlloc {
                dst: buffer.slot,
                capacity: size.slot,
                storage: Storage::PackedBytes,
            },
            span,
        );
        self.release(size, span);
        Assembly { buffer, zero: None }
    }

    /// Appends a run of literal text.
    pub(super) fn append_literal(&mut self, assembly: &mut Assembly, text: &str, span: Span) {
        match text.as_bytes() {
            [] => {}
            // One byte — a separator, a quote, a newline — is a push of a
            // constant, which needs no string loaded and no range.
            [byte] => {
                let value = self.temp(shapes::INT);
                self.emit(
                    Inst::Int {
                        dst: value.slot,
                        value: i64::from(*byte),
                    },
                    span,
                );
                self.emit(
                    Inst::GrowablePush {
                        owner: assembly.buffer.slot,
                        src: value.slot,
                        storage: Storage::PackedBytes,
                    },
                    span,
                );
                self.release(value, span);
            }
            bytes => {
                let id = self.string(text);
                let literal = self.temp(shapes::STR);
                self.emit(
                    Inst::Str {
                        dst: literal.slot,
                        text: id,
                    },
                    span,
                );
                let len = self.temp(shapes::INT);
                self.emit(
                    Inst::Int {
                        dst: len.slot,
                        value: bytes.len() as i64,
                    },
                    span,
                );
                self.extend(assembly, &literal, &len, span);
                self.release(len, span);
                self.release(literal, span);
            }
        }
    }

    /// Appends the text of one evaluated piece whose checked type is `ty`.
    ///
    /// The one place a piece's type decides how it becomes text. A type that
    /// shows itself through a trait of its own would be one more arm here.
    ///
    /// The piece is read and not consumed: its live range is the caller's to
    /// end, because an assertion's message appends values it compared first
    /// and releases afterwards.
    pub(super) fn append_piece(
        &mut self,
        assembly: &mut Assembly,
        ty: Option<&Ty>,
        value: &Val,
        span: Span,
    ) {
        match ty {
            Some(Ty::Str) if value.layout == shapes::STR => {
                let len = self.temp(shapes::INT);
                self.emit(
                    Inst::Len {
                        dst: len.slot,
                        obj: value.slot,
                    },
                    span,
                );
                self.extend(assembly, value, &len, span);
                self.release(len, span);
            }
            Some(Ty::Int) if value.layout == shapes::INT => {
                self.render_into(Intrinsic::IntRenderInto, assembly, value, span);
            }
            _ => self.render_into(Intrinsic::ValueRenderInto, assembly, value, span),
        }
    }

    /// The `String` the appended bytes are, written where the surrounding form
    /// asked for it.
    ///
    /// The run is validated as UTF-8 and relabelled, not copied, and the
    /// buffer is emptied — which nothing can observe, because nothing but this
    /// assembly ever named it.
    pub(super) fn assembly_finish(
        &mut self,
        assembly: Assembly,
        want: Option<Dest>,
        span: Span,
    ) -> Val {
        if let Some(zero) = assembly.zero {
            self.release(zero, span);
        }
        let dst = self.answer_at(want, shapes::STR);
        self.emit(
            Inst::RunFinish {
                dst: dst.slot,
                owner: assembly.buffer.slot,
                target: shapes::STR,
                validation: Validation::Utf8,
                storage: Storage::PackedBytes,
            },
            span,
        );
        self.release(assembly.buffer, span);
        dst
    }

    /// One byte `growable-extend` of the whole of `text`, whose byte length is
    /// in `len`.
    fn extend(&mut self, assembly: &mut Assembly, text: &Val, len: &Val, span: Span) {
        let zero = match &assembly.zero {
            Some(zero) => zero.slot,
            None => {
                let zero = self.temp(shapes::INT);
                self.emit(
                    Inst::Int {
                        dst: zero.slot,
                        value: 0,
                    },
                    span,
                );
                let slot = zero.slot;
                assembly.zero = Some(zero);
                slot
            }
        };
        let row = self.pool.args.intern(vec![
            assembly.buffer.arg(),
            text.arg(),
            crate::program::Arg {
                slot: zero,
                layout: shapes::INT,
            },
            len.arg(),
        ]);
        self.emit(
            Inst::GrowableExtend {
                args: row,
                storage: Storage::PackedBytes,
            },
            span,
        );
    }

    /// One call of a rendering intrinsic over `value` and the buffer.
    fn render_into(&mut self, intrinsic: Intrinsic, assembly: &Assembly, value: &Val, span: Span) {
        let unit = self.temp(shapes::UNIT);
        self.intrinsic_call(
            intrinsic,
            shapes::UNIT,
            unit.slot,
            &[value, &assembly.buffer],
            span,
        );
        self.release(unit, span);
    }
}
