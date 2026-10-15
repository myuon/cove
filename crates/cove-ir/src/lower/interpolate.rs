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
//! # One append per piece, chosen by the piece's type and then by its layout
//!
//! [`Body::append_piece`] is two arms on the checked type and then one
//! question of a layout table:
//!
//! - a `String` is appended whole by `appendText` — no rendering and no
//!   temporary string;
//! - an `Int` is a call to `std.int.renderInto` over the value and the
//!   buffer: standard-library Cove that appends one digit at a time,
//!   reached through [`Body::call_library`] and expanded where the inliner
//!   finds it worth it, as any call is;
//! - anything else is [`Body::render_piece`], which asks
//!   [`synth::rendered`] and gets back one of six answers. Almost always it
//!   is a **walk**: [ADR 0064]'s Decision 3 composes one private function per
//!   `(rendering, layout)` pair, out of this module's own two appends and
//!   `std.int`'s, `std.float`'s and `std.duration`'s `renderInto`, so an
//!   `Error`, an opaque value, a `Range`, a struct, an enum and every
//!   collection are ordinary IR the printer, the verifier, the optimizer and
//!   both code generators can see. A `Float` and a `Duration` are a call of
//!   their module's `renderInto`, as an `Int` is, since ADR 0068's Phase 4a.
//!   What is left is a box, whose layout does not say what it is: since ADR
//!   0068's Phase 4b-ii it is `std.dynamic.renderInto`, a Cove walk over a
//!   view of it, and `Value.renderInto` is gone.
//!
//! The first two arms are the **short circuit**, and they are why the two
//! commonest interpolations in the repository cost what they always did: a
//! piece the checked type already answers for never reaches the layout table
//! at all.
//!
//! A literal run of text is not a piece and never reaches that match: its
//! bytes are known here, so it is `appendByteInto` of a constant when it is
//! one byte and `appendText` of a string from the pool otherwise. A
//! synthesized walk appends its own punctuation the same two ways, through
//! the same two bodies, because [`synth::Leaves`] hands it their
//! `FunctionId`s.
//!
//! # A tagged literal: `html"…"`
//!
//! [ADR 0101]'s `html"a{x}b"` is the same assembly with one more question per
//! piece, asked of the piece's checked type by
//! [`cove_sema::stdlib::LiteralTag::piece`]: a piece that is already the
//! tag's markup is appended as it is, an `Array` of it is appended element by
//! element, a `String` is escaped straight into the buffer, and anything else
//! is rendered exactly as above into a scratch buffer of its own and that
//! text escaped. The literal text between the holes is appended raw, and the
//! finished `String` is wrapped as the tag's type. Every one of those calls
//! is a function of the tag's module named by the one row
//! [`cove_sema::stdlib::literal_tag`] answers, so what escaping means is
//! `std.html`'s Cove and nothing here.
//!
//! [ADR 0103]'s attribute holes, `name={x}`, are found by
//! [`cove_sema::stdlib::lay_out`] — the oracle and the checker ask the same
//! function — and written by [`cove_sema::stdlib::LiteralTag::attribute`]'s
//! answer: a `Bool` by the module's `flag`, which writes the name or
//! nothing; an `Option` by a switch on its case, and for `Some` its payload
//! as a value; anything else as a value, by the module's `attribute`, over
//! the `String` or over its rendering into a scratch run. The quotes and the
//! escaping are the module's Cove too.
//!
//! [ADR 0103]: ../../../../docs/adr/0103-in-html-a-line-break-is-spacing-and-an-attribute-value-is-one-hole.md
//!
//! [ADR 0101]: ../../../../docs/adr/0101-a-page-is-markup-with-an-expression-in-every-hole.md
//! [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
//!
//! [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
//! [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md

use cove_diag::Span;
use cove_sema::stdlib::{lay_out, literal_tag, AttributeValue, Laid, LiteralTag, TaggedPiece};
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Expr, StrPart};

use super::dispatch::{DURATION_TEXT, FLOAT_TEXT};
use super::frame::Val;
use super::synth;
use super::{shapes, Body, Dest};
use crate::inst::{Inst, Storage, Validation};
use crate::program::Arg;

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
pub(super) const INT_RENDERING: (&str, &str) = ("std.int", "renderInto");

/// The standard-library function a whole `String` is appended by.
///
/// `crates/cove-sema/std/stringbuilder.cove` writes it, and
/// `StringBuilder.append` is the same call over a builder's buffer.
pub(super) const TEXT_APPEND: (&str, &str) = ("std.stringbuilder", "appendText");

/// The standard-library function one byte is appended by, which
/// `StringBuilder.appendByte` also calls.
pub(super) const BYTE_APPEND: (&str, &str) = ("std.stringbuilder", "appendByteInto");

/// A string under assembly: the buffer it is appended to.
pub(super) struct Assembly {
    buffer: Val,
}

impl Body<'_> {
    /// A byte buffer sized for the literal whose parts are `parts`.
    pub(super) fn assembly_for(&mut self, parts: &[StrPart], span: Span) -> Assembly {
        let (literal, pieces) = parts
            .iter()
            .fold((0, 0), |(bytes, pieces), part| match part {
                StrPart::Text(text) => (bytes + text.len(), pieces),
                StrPart::Interpolation(_) => (bytes, pieces + 1),
            });
        self.assembly_open(literal, pieces, span)
    }

    /// A literal tagged `tag`, ADR 0101: the assembly of a plain literal,
    /// each hole appended as [`LiteralTag::piece`] says, and the finished
    /// text wrapped as the tag's type.
    ///
    /// A literal with no holes is assembled too, rather than being one
    /// constant: its value is not a `String` but what the tag's module
    /// makes of one.
    pub(super) fn tagged_expr(&mut self, expr: &Expr, tag: &str, parts: &[StrPart]) -> Val {
        let Some(known) = literal_tag(tag) else {
            // `cove_sema`'s checker refuses any other tag.
            return self.gap("a string literal with a tag the checker refuses", expr);
        };
        let mut assembly = self.assembly_for(parts, expr.span);
        for laid in lay_out(parts) {
            match laid {
                Laid::Text(text) => self.append_literal(&mut assembly, text, expr.span),
                Laid::Hole(inner) => {
                    let value = self.expr(inner);
                    self.append_tagged(known, &mut assembly, inner, &value);
                    self.release(value, inner.span);
                }
                Laid::Attribute { name, hole } => {
                    let value = self.expr(hole);
                    self.append_attribute(known, &assembly, name, hole, &value);
                    self.release(value, hole.span);
                }
            }
        }
        let said = self.assembly_finish(assembly, None, expr.span);
        let wrapped = self.call_library(known.module, known.wrap, &[&said], expr.span);
        self.release(said, expr.span);
        // `None` is a round that has not lowered the module's wrap yet; see
        // [`Body::render_by_walk`]. Its program is discarded.
        wrapped.unwrap_or_else(|| self.dead(expr))
    }

    /// Appends one evaluated hole of a tagged literal: whole when its type is
    /// the tag's markup, escaped otherwise.
    ///
    /// The question is asked of the type the checker settled for the hole as
    /// written, [`Body::raw_ty`], and not of this instantiation's — so a hole
    /// of a type parameter is escaped whatever the parameter becomes, as the
    /// tree-walking oracle escapes it.
    fn append_tagged(
        &mut self,
        known: &'static LiteralTag,
        assembly: &mut Assembly,
        inner: &Expr,
        value: &Val,
    ) {
        let span = inner.span;
        let piece = known.piece(self.raw_ty(inner));
        let module = known.module;
        match piece {
            TaggedPiece::Markup => self.append_by((module, known.markup), assembly, value, span),
            TaggedPiece::EachMarkup => self.append_by((module, known.each), assembly, value, span),
            TaggedPiece::Text => self.append_by((module, known.escape), assembly, value, span),
            TaggedPiece::Rendered => {
                let ty = self.ty(inner);
                // An `Int`'s text is digits and a sign, which escaping
                // leaves as they are, so it is written straight in.
                if matches!(ty, Some(Ty::Int)) && value.layout == shapes::INT {
                    self.render_by(INT_RENDERING, assembly, value, span);
                    return;
                }
                // ADR 0101's "a rendered value into a scratch run": the walk
                // writes a buffer of its own, and that text is escaped.
                let mut scratch = self.assembly_open(0, 1, span);
                self.append_piece(&mut scratch, ty.as_ref(), value, span);
                let text = self.assembly_finish(scratch, None, span);
                self.append_by((module, known.escape), assembly, &text, span);
                self.release(text, span);
            }
        }
    }

    /// Appends one evaluated attribute hole of a tagged literal, `name={x}`,
    /// as [`LiteralTag::attribute`] says, [ADR 0103]: a `Bool` by the
    /// module's `flag`, an `Option` by a switch on its case and then its
    /// payload as a value, and anything else as a value.
    ///
    /// `name` is the attribute's name with the whitespace in front of it, a
    /// constant the module's functions write or leave out. The type is
    /// asked as [`Body::append_tagged`] asks it, of the hole as written.
    ///
    /// [ADR 0103]: ../../../../docs/adr/0103-in-html-a-line-break-is-spacing-and-an-attribute-value-is-one-hole.md
    fn append_attribute(
        &mut self,
        known: &'static LiteralTag,
        assembly: &Assembly,
        name: &str,
        inner: &Expr,
        value: &Val,
    ) {
        let span = inner.span;
        match known.attribute(self.raw_ty(inner)) {
            AttributeValue::Flag => {
                let name = self.constant_text(name, span);
                if let Some(unit) = self.call_library(
                    known.module,
                    known.flag,
                    &[&assembly.buffer, &name, value],
                    span,
                ) {
                    self.release(unit, span);
                }
                self.release(name, span);
            }
            AttributeValue::Value(piece) => {
                let ty = self.ty(inner);
                self.append_attribute_value(known, assembly, name, piece, ty.as_ref(), value, span);
            }
            AttributeValue::Optional(piece) => {
                let ty = self.ty(inner);
                let Some(Ty::Option(payload_ty)) = ty else {
                    self.errors.push(super::gap::gap(
                        "an `Option` attribute hole whose instantiated type is not an `Option`",
                        span,
                    ));
                    return;
                };
                let found = shapes::case_at(
                    self.checked,
                    self.module,
                    &Ty::Option(payload_ty.clone()),
                    "Some",
                )
                .and_then(|(index, _)| Some((index, self.case_of(value.layout, index)?.0)));
                let Some((index, parts)) = found else {
                    self.errors.push(super::gap::gap(
                        "an `Option` attribute hole whose layout has no `Some` case",
                        span,
                    ));
                    return;
                };
                let Some(part) = parts.first() else {
                    return;
                };
                // `None` jumps past the attribute; `Some` writes its payload,
                // read where it lies in the value and not consumed.
                let mut missed = Vec::new();
                self.test_case(value.slot, value.layout, index, span, &mut missed);
                let payload = Val::borrowed(value.slot + 1 + part.at, part.layout);
                self.append_attribute_value(
                    known,
                    assembly,
                    name,
                    piece,
                    Some(&payload_ty),
                    &payload,
                    span,
                );
                let end = self.here();
                for at in missed {
                    self.patch(at, end);
                }
            }
            // `cove_sema`'s checker refuses markup as an attribute's value.
            AttributeValue::Markup => self.errors.push(super::gap::gap(
                "markup as an attribute's value, which the checker refuses",
                span,
            )),
        }
    }

    /// Appends ` name="…"` with `value` as the value: by the module's
    /// `attribute` over the value itself when it is a `String`, and over its
    /// rendering, into a scratch run as [`Body::append_tagged`] renders,
    /// otherwise.
    #[allow(clippy::too_many_arguments)]
    fn append_attribute_value(
        &mut self,
        known: &'static LiteralTag,
        assembly: &Assembly,
        name: &str,
        piece: TaggedPiece,
        ty: Option<&Ty>,
        value: &Val,
        span: Span,
    ) {
        let rendered = match piece {
            TaggedPiece::Text => None,
            _ => {
                let mut scratch = self.assembly_open(0, 1, span);
                self.append_piece(&mut scratch, ty, value, span);
                Some(self.assembly_finish(scratch, None, span))
            }
        };
        let name = self.constant_text(name, span);
        let text = rendered.as_ref().unwrap_or(value);
        if let Some(unit) = self.call_library(
            known.module,
            known.attribute,
            &[&assembly.buffer, &name, text],
            span,
        ) {
            self.release(unit, span);
        }
        self.release(name, span);
        if let Some(rendered) = rendered {
            self.release(rendered, span);
        }
    }

    /// A `String` constant from the pool, in a temporary of its own.
    fn constant_text(&mut self, text: &str, span: Span) -> Val {
        let id = self.string(text);
        let literal = self.temp(shapes::STR);
        self.emit(
            Inst::Str {
                dst: literal.slot,
                text: id,
            },
            span,
        );
        literal
    }

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
                self.render_by(INT_RENDERING, assembly, value, span);
            }
            _ => self.render_piece(assembly, value, span),
        }
    }

    /// The text of a piece that is neither a `String` nor an `Int`.
    ///
    /// One question of one table — [`synth::rendered`] — asked here and
    /// asked again at every part of whatever walk it produces, which is
    /// `synth::ordered_by`'s arrangement and is what keeps the call site and
    /// the walk from disagreeing about which of them writes a part.
    ///
    /// The two arms above it are the *short circuit*, and they are the
    /// reason a `String` piece costs one append and an `Int` piece one call:
    /// both are answers this table would give a moment later, taken from the
    /// checked type before a layout is looked at.
    fn render_piece(&mut self, assembly: &Assembly, value: &Val, span: Span) {
        let shape = self.pool.shapes.layout(value.layout).shape.clone();
        match synth::rendered(&shape) {
            // A box, whose layout does not say what the value is: ADR 0064's
            // Decision 4, answered in Cove since ADR 0068's Phase 4b-ii.
            synth::Rendered::Dynamic => self.render_erased(assembly, value, span),
            // A piece whose checked type was not `Ty::Str` or `Ty::Int` and
            // whose layout is one of theirs anyway. The same two appends the
            // short circuit makes, reached the long way round.
            synth::Rendered::Text => {
                self.append_by(TEXT_APPEND, assembly, value, span);
            }
            synth::Rendered::Digits => self.render_by(INT_RENDERING, assembly, value, span),
            // ADR 0068's Phase 4a: a `Float` and a `Duration` are written by
            // `std.float` and `std.duration` in Cove, as an `Int` is by
            // `std.int`.
            synth::Rendered::Float => self.render_by(FLOAT_TEXT, assembly, value, span),
            synth::Rendered::Duration => self.render_by(DURATION_TEXT, assembly, value, span),
            synth::Rendered::Walk => self.render_by_walk(assembly, value, span),
        }
    }

    /// A call of one of the standard-library functions that write a scalar's
    /// text, `(module, name)`, over the piece and the buffer: the value
    /// first, the buffer second.
    fn render_by(
        &mut self,
        (module, function): (&str, &str),
        assembly: &Assembly,
        value: &Val,
        span: Span,
    ) {
        if let Some(unit) = self.call_library(module, function, &[value, &assembly.buffer], span) {
            self.release(unit, span);
        }
    }

    /// The walk [`synth`] composes for this piece's layout, called over the
    /// value and the buffer.
    ///
    /// The buffer is a *parameter* and not an answer: a byte buffer is a
    /// handle (ADR 0052), so the callee's appends are this assembly's and a
    /// level of nesting costs no `String`. What the call answers is the `()`
    /// every append answers, which nothing reads.
    fn render_by_walk(&mut self, assembly: &Assembly, value: &Val, span: Span) {
        if self.render_leaves_for(value.layout, span).is_none() {
            // A round in which one of the three appends, a scalar writer or
            // the rendering of a box the walk calls was not in the slice.
            // `Body::reached` has recorded it and `lower_roots` will lower the
            // package again with it; this round's program is discarded before
            // it is verified, so nothing needs to stand in for the walk.
            return;
        }
        let decls = self.plan.decls.len();
        let callee = synth::function_for(
            synth::Operation::Rendering,
            value.layout,
            self.pool,
            decls,
            span,
        );
        let unit = self.temp(shapes::UNIT);
        let args = self
            .pool
            .args
            .intern(vec![value.arg(), assembly.buffer.arg()]);
        self.emit(
            Inst::Call {
                dst: unit.slot,
                callee,
                args,
            },
            span,
        );
        self.release(unit, span);
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

    /// `std.dynamic.renderInto` over the box `value` and the buffer, with an
    /// empty path.
    ///
    /// ADR 0068's Phase 4b-ii: the text of an erased value is a Cove walk over
    /// a view of it. An interpolation is inside no vector, so the path it
    /// hands over is of depth nought, and its address — the path's own first
    /// word, which is a place in this frame whatever it holds — is one
    /// nothing reads. A walk composed for a known layout that reaches a box
    /// hands over the path it is carrying instead; see `synth`'s `below`.
    fn render_erased(&mut self, assembly: &Assembly, value: &Val, span: Span) {
        let Some(callee) = self.dynamic_render(span) else {
            // Not in this round's slice: see [`Body::render_by_walk`].
            return;
        };
        let path = self.temp(shapes::RENDER_PATH);
        self.emit(
            Inst::AddrOfSlot {
                dst: path.slot,
                slot: path.slot,
            },
            span,
        );
        self.emit(
            Inst::Int {
                dst: path.slot + crate::dynamic::PATH_DEPTH as crate::inst::Slot,
                value: 0,
            },
            span,
        );
        let unit = self.temp(shapes::UNIT);
        let args = self.pool.args.intern(vec![
            value.arg(),
            assembly.buffer.arg(),
            Arg {
                slot: path.slot,
                layout: shapes::RENDER_PATH,
            },
        ]);
        self.emit(
            Inst::Call {
                dst: unit.slot,
                callee,
                args,
            },
            span,
        );
        self.release(unit, span);
        self.release(path, span);
    }
}
