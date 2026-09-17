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
//! # The builder's appends, not `StringBuilder`'s methods
//!
//! The buffer is allocated and finished here as the run instructions
//! `super::core` lowers `core.bytesAllocate` and `core.bytesFinish` to, rather
//! than through `StringBuilder`, so it needs no uniqueness proof for `finish`
//! — the buffer is a temporary nothing else can name (#403). The appends in
//! between are calls, through [`Body::call_library`], of the two functions
//! `StringBuilder.append` and `appendByte` are written over:
//! `std.stringbuilder`'s `appendText` and `appendByteInto`, which take the
//! buffer itself. Each is [ADR 0062]'s ensure, write and commit in Cove, a
//! thin library leaf that `super::inline` expands at every site, so an append
//! here is the same window a builder's is, recognised by the same
//! [`crate::legalize`] — and no lowering writes the protocol a second time.
//!
//! # One append per piece, chosen by the piece's type
//!
//! [`Body::append_piece`] is a single `match` on the checked type of a piece:
//!
//! - a `String` is appended whole by `appendText` — no rendering and no
//!   temporary string;
//! - an `Int` is a call to `std.int.renderInto` over the value and the
//!   buffer: standard-library Cove that appends one digit at a time,
//!   reached through [`Body::call_library`] and expanded where the inliner
//!   finds it worth it, as any call is;
//! - anything else is rendered into the buffer by `Value.renderInto`, which is
//!   the runtime's one layout-directed rendering walk, so an `Error`, an
//!   opaque value, a `Range`, a collection, a box and a closure all show
//!   exactly as they did.
//!
//! A literal run of text is not a piece and never reaches that match: its
//! bytes are known here, so it is `appendByteInto` of a constant when it is
//! one byte and `appendText` of a string from the pool otherwise.
//!
//! [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
//! [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md

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

/// The standard-library function an `Int` piece is appended by: module, then
/// name.
///
/// `crates/cove-sema/std/int.cove` writes it over ADR 0062's byte append, and
/// says why it cannot overflow. It is not exported, because nothing but this
/// lowering calls it.
const INT_RENDERING: (&str, &str) = ("std.int", "renderInto");

/// The standard-library function a whole `String` is appended by.
///
/// `crates/cove-sema/std/stringbuilder.cove` writes it, and
/// `StringBuilder.append` is the same call over a builder's buffer.
const TEXT_APPEND: (&str, &str) = ("std.stringbuilder", "appendText");

/// The standard-library function one byte is appended by, which
/// `StringBuilder.appendByte` also calls.
const BYTE_APPEND: (&str, &str) = ("std.stringbuilder", "appendByteInto");

/// A string under assembly: the buffer it is appended to.
pub(super) struct Assembly {
    buffer: Val,
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
        Assembly { buffer }
    }

    /// Appends a run of literal text.
    pub(super) fn append_literal(&mut self, assembly: &mut Assembly, text: &str, span: Span) {
        match text.as_bytes() {
            [] => {}
            // One byte — a separator, a quote, a newline — is a byte append of
            // a constant, which needs no string loaded and no length.
            [byte] => {
                let value = self.temp(shapes::INT);
                self.emit(
                    Inst::Int {
                        dst: value.slot,
                        value: i64::from(*byte),
                    },
                    span,
                );
                self.append_by(BYTE_APPEND, assembly, &value, span);
                self.release(value, span);
            }
            _ => {
                let id = self.string(text);
                let literal = self.temp(shapes::STR);
                self.emit(
                    Inst::Str {
                        dst: literal.slot,
                        text: id,
                    },
                    span,
                );
                self.append_by(TEXT_APPEND, assembly, &literal, span);
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
                self.append_by(TEXT_APPEND, assembly, value, span);
            }
            Some(Ty::Int) if value.layout == shapes::INT => {
                let (module, function) = INT_RENDERING;
                if let Some(unit) =
                    self.call_library(module, function, &[value, &assembly.buffer], span)
                {
                    self.release(unit, span);
                }
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

    /// A call of one of `std.stringbuilder`'s appends over the buffer and
    /// `piece`, whose answer nobody reads.
    fn append_by(
        &mut self,
        (module, function): (&str, &str),
        assembly: &Assembly,
        piece: &Val,
        span: Span,
    ) {
        if let Some(unit) = self.call_library(module, function, &[&assembly.buffer, piece], span) {
            self.release(unit, span);
        }
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
