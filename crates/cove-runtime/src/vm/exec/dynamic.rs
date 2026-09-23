//! [ADR 0068](../../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! structural observations, over the words the machine holds.
//!
//! The seven reflection opcodes are thin: [`super::encoded`] reads a view out
//! of the frame, asks one function here, and writes the answer back. What a
//! view *is* — three words, the layout of the viewed value, the object that
//! roots it and the payload word it begins at — is `cove_ir::dynamic`'s; this
//! is what those words mean to this machine.
//!
//! # A view never denotes a box or a bare reference
//!
//! Every view this module answers has been **normalised**, and [`settle`] is
//! the one place that does it. Where the value's words are a single reference
//! — a `String`, a collection, a closure, a box, or a word a static layout only
//! calls `<ref>` — the reference is followed, the object's layout is read from
//! its own header rather than from the static layout, and a box is opened again,
//! through as many boxes as there are. So a heap value is viewed as
//! `(its header layout, the object, 0)`, an inline value as
//! `(its layout, the object it is in, where)`, and erasure is looked through
//! everywhere — as [`crate::vm::intrinsics`]' equality looks through it.
//!
//! # Nothing here allocates
//!
//! A child is three words computed from its parent's three and the layout
//! table: an inline child keeps the parent's owner and adds an offset, a child
//! in a run has the run as its owner, and a child that is a reference is
//! followed. So a walk over a value of any size allocates nothing, which is the
//! ADR's gate — "no child allocation required merely to traverse a value" —
//! and `tests` below reads the allocation counter to hold it.
//!
//! # A disagreement is the standard library's bug
//!
//! Reading a `Bool` out of a string view, asking the case of a struct, or
//! projecting a child past the count are **internal** runtime errors, not
//! refusals a program can reach: the standard library asks the kind and the
//! count first. They are refused rather than answered because the
//! alternative is reading a word as something it is not — the one fault this
//! machine exists to make loud.

use cove_ir::bytecode::Op;
use cove_ir::dynamic::{declared_name, VIEW_AT, VIEW_LAYOUT, VIEW_OWNER};
use cove_ir::{DynamicKind, FunctionId, Layout, LayoutId, Repr, Shape, Slot};

use super::{null_object, Machine};
use crate::error::RuntimeError;

/// A view, as the three words a frame holds it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct View {
    /// The layout of the viewed value: the header's for a heap value, the
    /// static one for an inline value.
    pub(crate) layout: LayoutId,
    /// The object that roots the value. Never null in a view this module
    /// answered.
    pub(crate) owner: u64,
    /// The payload word of `owner` the value begins at.
    pub(crate) at: u32,
}

impl View {
    /// The view whose first word is frame word `index`.
    pub(crate) fn read(machine: &Machine, index: usize) -> View {
        let word = |at: u32| machine.mem.word_at(index + at as usize);
        View {
            layout: LayoutId(word(VIEW_LAYOUT) as u32),
            owner: word(VIEW_OWNER),
            at: word(VIEW_AT) as u32,
        }
    }

    /// Writes this view's three words at frame word `index`.
    pub(crate) fn write(self, machine: &mut Machine, index: usize) {
        machine
            .mem
            .set_word_at(index + VIEW_LAYOUT as usize, u64::from(self.layout.0));
        machine
            .mem
            .set_word_at(index + VIEW_OWNER as usize, self.owner);
        machine
            .mem
            .set_word_at(index + VIEW_AT as usize, u64::from(self.at));
    }
}

/// Executes one of the seven reflection opcodes `op` in the frame whose first
/// word is `frame`, with slot operands `a`, `b` and `c` of function `id`.
///
/// Out of line on purpose: see the reflection arm of
/// [`super::encoded`]'s dispatch loop, whose stack frame this keeps small.
#[inline(never)]
pub(crate) fn execute(
    machine: &mut Machine,
    op: u8,
    frame: usize,
    a: Slot,
    b: Slot,
    c: Slot,
    id: FunctionId,
) -> Result<(), RuntimeError> {
    let at = |slot: Slot| frame + slot as usize;
    let word = match Op::from_number(op) {
        Some(Op::DynOpen) => {
            let boxed = machine.mem.word_at(at(b));
            open(machine, boxed)?.write(machine, at(a));
            return Ok(());
        }
        Some(Op::DynChild) => {
            let view = View::read(machine, at(b));
            let index = machine.mem.word_at(at(c)) as i64;
            child(machine, view, index)?.write(machine, at(a));
            return Ok(());
        }
        Some(Op::DynKind) => kind(machine, View::read(machine, at(b)))?.code() as u64,
        Some(Op::DynSameType) => u64::from(same_type(
            machine,
            View::read(machine, at(b)),
            View::read(machine, at(c)),
        )?),
        // The one opcode whose meaning is its destination's `Repr`: which
        // scalar is read is the word the frame says `a` is, and a view whose
        // kind disagrees is refused rather than reinterpreted.
        Some(Op::DynRead) => {
            let want = machine.program.function(id).repr(a).unwrap_or(Repr::Unit);
            read(machine, View::read(machine, at(b)), want)?
        }
        Some(Op::DynCase) => case(machine, View::read(machine, at(b)))? as u64,
        Some(Op::DynCount) => count(machine, View::read(machine, at(b)))? as u64,
        other => unreachable!("{other:?} is not a reflection opcode"),
    };
    machine.mem.set_word_at(at(a), word);
    Ok(())
}

/// `dyn.open`: the view of the value the box at `boxed` holds.
///
/// That `boxed` is a box is checked from its header, because the verifier can
/// only say the slot is a reference: the lowering emits this for an erased
/// value and nothing else, and a box is the only thing an erased value is.
pub(crate) fn open(machine: &Machine, boxed: u64) -> Result<View, RuntimeError> {
    if boxed == 0 {
        return Err(null_object());
    }
    let header = machine.mem.object_layout(boxed);
    let described = layout(machine, header)?;
    if !matches!(described.shape, Shape::Boxed) {
        return Err(internal(format!(
            "a dynamic view was opened on a `{}`, which is not an erased value",
            described.name
        )));
    }
    let inner = LayoutId(machine.mem.payload(boxed, 0) as u32);
    settle(machine, inner, boxed, 1)
}

/// The view of the value of `layout` whose words begin at payload word `at` of
/// `owner`, normalised: a reference is followed and a box is opened, until what
/// is left is a heap object that is not a box or an inline value.
///
/// The loop is bounded by the boxes the value is inside, and a box holds a
/// value that was boxed before it, so there is no cycle for it to go round.
fn settle(
    machine: &Machine,
    mut layout: LayoutId,
    mut owner: u64,
    mut at: u32,
) -> Result<View, RuntimeError> {
    loop {
        let described = self::layout(machine, layout)?;
        if !described.is_one_address() {
            machine.checked(owner, at, described.width())?;
            return Ok(View { layout, owner, at });
        }
        machine.checked(owner, at, 1)?;
        let object = machine.mem.payload(owner, at);
        if object == 0 {
            return Err(null_object());
        }
        // The header's layout, not `layout`: a static `<ref>` names no
        // family, and even a static `String` is a claim the object itself is
        // the authority on.
        let header = machine.mem.object_layout(object);
        match &self::layout(machine, header)?.shape {
            Shape::Boxed => {
                layout = LayoutId(machine.mem.payload(object, 0) as u32);
                owner = object;
                at = 1;
            }
            _ => {
                return Ok(View {
                    layout: header,
                    owner: object,
                    at: 0,
                })
            }
        }
    }
}

/// The layout `id` names, refusing a reclaimed run and an id the program does
/// not have.
fn layout<'p>(machine: &Machine<'p>, id: LayoutId) -> Result<&'p Layout, RuntimeError> {
    match machine.program.layouts.get(id.index()) {
        Some(described) if matches!(described.shape, Shape::Free) => Err(RuntimeError::new(
            "this value was read after it was reclaimed",
        )),
        Some(described) => Ok(described),
        None => Err(internal(format!(
            "a dynamic view names layout {}, which this program does not have",
            id.0
        ))),
    }
}

/// `dyn.kind`: which of [`DynamicKind`]'s kinds the viewed value is.
pub(crate) fn kind(machine: &Machine, view: View) -> Result<DynamicKind, RuntimeError> {
    let described = layout(machine, view.layout)?;
    Ok(kind_of(machine, described))
}

/// The kind of a value of `described`, which [`settle`] has already made
/// neither a box nor a bare reference.
fn kind_of(machine: &Machine, described: &Layout) -> DynamicKind {
    match &described.shape {
        Shape::Word(Repr::Unit) => DynamicKind::Unit,
        Shape::Word(Repr::Bool) => DynamicKind::Bool,
        Shape::Word(Repr::Int) => DynamicKind::Int,
        Shape::Word(Repr::Float) => DynamicKind::Float,
        Shape::Word(Repr::Duration) => DynamicKind::Duration,
        Shape::Str => DynamicKind::String,
        // An `opaque` struct's fields belong to the module that declared it,
        // so it is no more readable through a view than through a rendering.
        Shape::Struct { opaque: true, .. } => DynamicKind::Opaque,
        Shape::Struct { .. } if crate::vm::boundary::is_range(machine.program, described) => {
            DynamicKind::Range
        }
        Shape::Struct { .. } => DynamicKind::Struct,
        Shape::Enum { .. } => DynamicKind::Enum,
        Shape::Elements { .. } => DynamicKind::Array,
        Shape::Vector { .. } => DynamicKind::Vector,
        Shape::Members { .. } => DynamicKind::Set,
        Shape::Entries { .. } => DynamicKind::Map,
        Shape::Closure { .. } => DynamicKind::Function,
        // ADR 0068's Decision 7: a Host handle, a task, a scope, an address,
        // a case tag, a synchronized cell and a byte run are capabilities or
        // machinery, and boxing one does not make it readable.
        Shape::Word(_)
        | Shape::Shared { .. }
        | Shape::Bytes
        | Shape::ByteBuffer
        | Shape::Boxed
        | Shape::Free => DynamicKind::Opaque,
    }
}

/// `dyn.same-type`: whether the two viewed values have one semantic type.
///
/// The kinds agree, and for the three nominal kinds so do the declared names
/// — compared with any instantiation left off, so an `Option<Int>` and an
/// `Option<String>` are one type, as they are to the oracle, whose values
/// carry no type arguments at all.
pub(crate) fn same_type(machine: &Machine, a: View, b: View) -> Result<bool, RuntimeError> {
    let (left, right) = (layout(machine, a.layout)?, layout(machine, b.layout)?);
    let kind = kind_of(machine, left);
    Ok(kind == kind_of(machine, right)
        && (!kind.is_nominal() || declared_name(&left.name) == declared_name(&right.name)))
}

/// `dyn.read`: the scalar the viewed value is, as the word a slot of `want`
/// holds.
///
/// A `String` answers the object the view already names — the string is
/// immutable, so that *is* the value — and allocates nothing.
pub(crate) fn read(machine: &Machine, view: View, want: Repr) -> Result<u64, RuntimeError> {
    let kind = kind(machine, view)?;
    match kind.read_as() {
        Some(repr) if repr == want => Ok(match kind {
            DynamicKind::String => view.owner,
            _ => machine.mem.payload(view.owner, view.at),
        }),
        // Named by the kind the destination reads, as the oracle names it,
        // so that the two evaluators say the same sentence.
        _ => Err(internal(format!(
            "a dynamic view of a {} was read as a `{}`",
            kind.name(),
            DynamicKind::ALL
                .iter()
                .find(|read| read.read_as() == Some(want))
                .map_or(want.name(), |read| read.name())
        ))),
    }
}

/// `dyn.case`: the case index of the viewed enum, which is its first word.
pub(crate) fn case(machine: &Machine, view: View) -> Result<i64, RuntimeError> {
    let described = layout(machine, view.layout)?;
    match &described.shape {
        Shape::Enum { .. } => Ok(machine.mem.payload(view.owner, view.at) as i64),
        _ => Err(internal(format!(
            "the case of a dynamic view of a {} was asked",
            kind_of(machine, described).name()
        ))),
    }
}

/// `dyn.count`: how many children the viewed value has, in [`child`]'s order.
pub(crate) fn count(machine: &Machine, view: View) -> Result<i64, RuntimeError> {
    let described = layout(machine, view.layout)?;
    let kind = kind_of(machine, described);
    Ok(match (&described.shape, kind) {
        (Shape::Struct { fields, .. }, DynamicKind::Struct | DynamicKind::Range) => {
            fields.len() as i64
        }
        (Shape::Enum { .. }, _) => enum_case(machine, view, described)?.parts.len() as i64,
        (Shape::Elements { .. } | Shape::Members { .. }, _) => {
            i64::from(machine.mem.object_len(view.owner))
        }
        (Shape::Entries { .. }, _) => 2 * i64::from(machine.mem.object_len(view.owner)),
        (Shape::Vector { .. }, _) => machine.mem.payload(view.owner, VECTOR_LEN) as i64,
        _ => 0,
    })
}

/// Payload word 0 of a `Shape::Vector` owner: its element count. The lowering's
/// `VECTOR_LEN`, which is `super::runs`' `GROWABLE_LEN`.
const VECTOR_LEN: u32 = super::GROWABLE_LEN;

/// Payload word 1 of a `Shape::Vector` owner: the run its elements are in.
const VECTOR_STORE: u32 = super::GROWABLE_STORE;

/// The case the enum `view` names holds, refusing a discriminant the layout
/// does not have — which a checked program cannot build and a word read as
/// the wrong thing can.
fn enum_case<'l>(
    machine: &Machine,
    view: View,
    described: &'l Layout,
) -> Result<&'l cove_ir::Case, RuntimeError> {
    let Shape::Enum { cases, .. } = &described.shape else {
        unreachable!("asked only of an enum");
    };
    let index = machine.mem.payload(view.owner, view.at);
    cases.get(index as usize).ok_or_else(|| {
        RuntimeError::new(format!(
            "this `{}` is in a case it does not have",
            described.name
        ))
    })
}

/// `dyn.child`: the view of child `index`, in the canonical order the existing
/// walks use.
///
/// A struct's and a range's fields in declaration order; the current case's
/// parts, which begin after the discriminant; an array's and a set's elements
/// at the element's stride; a map's entries as key then value, so child `2i`
/// is entry `i`'s key and `2i + 1` its value; and a vector's elements in its
/// store, the count taken from its own length word. Every other kind has no
/// children.
pub(crate) fn child(machine: &Machine, view: View, index: i64) -> Result<View, RuntimeError> {
    let count = count(machine, view)?;
    if index < 0 || index >= count {
        return Err(internal(format!(
            "child {index} of a dynamic view with {count} children was asked"
        )));
    }
    let at = index as u32;
    let described = layout(machine, view.layout)?;
    let width = |id: LayoutId| machine.program.layout(id).width();
    match &described.shape {
        Shape::Struct { fields, .. } => {
            let field = &fields[at as usize];
            settle(machine, field.layout, view.owner, view.at + field.at)
        }
        Shape::Enum { .. } => {
            let part = &enum_case(machine, view, described)?.parts[at as usize];
            settle(machine, part.layout, view.owner, view.at + 1 + part.at)
        }
        Shape::Elements { elem, .. } | Shape::Members { elem } => {
            settle(machine, *elem, view.owner, at * width(*elem))
        }
        Shape::Entries { key, value } => {
            let entry = (at / 2) * (width(*key) + width(*value));
            if at.is_multiple_of(2) {
                settle(machine, *key, view.owner, entry)
            } else {
                settle(machine, *value, view.owner, entry + width(*key))
            }
        }
        Shape::Vector { elem } => {
            let store = machine.mem.payload(view.owner, VECTOR_STORE);
            if store == 0 {
                return Err(super::consumed_vector());
            }
            settle(machine, *elem, store, at * width(*elem))
        }
        _ => unreachable!("a kind with no children counted {count}"),
    }
}

/// An internal runtime error: a view asked a question its kind has no answer
/// to, which the standard library's walk asks the kind first to avoid.
fn internal(message: String) -> RuntimeError {
    RuntimeError::new(format!("internal error: {message}")).with_help(
        "a dynamic view is read by the standard library's own walks, so this is a bug in the \
         standard library rather than in the program",
    )
}

#[cfg(test)]
mod tests;
