//! [ADR 0068](../../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! structural observations, over the words the machine holds.
//!
//! The reflection opcodes are thin: [`super::encoded`] reads a view out of
//! the frame, asks one function here, and writes the answer back. What a view
//! *is* — three words, the layout of the viewed value, the object that roots
//! it and the payload word it begins at — is `cove_ir::dynamic`'s; this is
//! what those words mean to this machine.
//!
//! Nine of them are ADR 0068's Phases 1 to 3. Six more are its Phase 4b-ii's,
//! for the rendering of an erased value: three names — a struct's, a field's,
//! a case's — each the address of a literal `cove_ir`'s `lower::names` placed
//! before the run; whether a struct is `opaque`; an opaque value's text, the
//! one of them that allocates; and whether a vector is on the path a walk the
//! lowering composed handed over, which follows that path's frames below the
//! boundary. Beside them is [`audit_box`], which holds the placed names to the
//! boxes a run really makes.
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
//! and `tests` below reads the allocation counter to hold it. The one
//! exception is [`handle_text`], the text of a Host resource, a scope or a task
//! inside a box, which is a new `String` because which handle it is lives in a
//! table of the run's; it is not part of any traversal.
//!
//! # A disagreement is the standard library's bug
//!
//! Reading a `Bool` out of a string view, asking the case of a struct, or
//! projecting a child past the count are **internal** runtime errors, not
//! refusals a program can reach: the standard library asks the kind and the
//! count first. They are refused rather than answered because the
//! alternative is reading a word as something it is not — the one fault this
//! machine exists to make loud.

use std::cmp::Ordering;

use cove_ir::bytecode::Op;
use cove_ir::dynamic::{declared_name, PATH_DEPTH, PATH_ENTRY, VIEW_AT, VIEW_LAYOUT, VIEW_OWNER};
use cove_ir::{
    DynamicKind, FunctionId, Layout, LayoutId, LayoutNames, Program, Repr, Shape, Slot, StrId,
};
use cove_native::{DYN_ASK, DYN_COUNT_SHIFT, DYN_NAME_SHIFT};

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

/// Executes one of the reflection opcodes that do not allocate, `op`, in the
/// frame whose first word is `frame`, with slot operands `a`, `b` and `c` of
/// function `id`: the nine of ADR 0068's Phases 1 to 3, and five of the six
/// its Phase 4b-ii brought. The sixth, [`handle_text`], allocates, and is an
/// arm of its own.
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
    // The opcodes as constants rather than through `Op::from_number`, whose
    // table is behind a `LazyLock`: this is asked once per observation on both
    // tiers, and the native tier's reflection helper asks nothing else.
    const OPEN: u8 = Op::DynOpen.number();
    const CHILD: u8 = Op::DynChild.number();
    const KIND: u8 = Op::DynKind.number();
    const SAME_TYPE: u8 = Op::DynSameType.number();
    const READ: u8 = Op::DynRead.number();
    const CASE: u8 = Op::DynCase.number();
    const COUNT: u8 = Op::DynCount.number();
    const SAME_OBJECT: u8 = Op::DynSameObject.number();
    const NAME_ORDER: u8 = Op::DynNameOrder.number();
    const TYPE_NAME: u8 = Op::DynTypeName.number();
    const FIELD_NAME: u8 = Op::DynFieldName.number();
    const CASE_NAME: u8 = Op::DynCaseName.number();
    const OPAQUE: u8 = Op::DynOpaque.number();
    const ON_PATH: u8 = Op::DynOnPath.number();
    let word = match op {
        OPEN => {
            let boxed = machine.mem.word_at(at(b));
            open(machine, boxed)?.write(machine, at(a));
            return Ok(());
        }
        CHILD => {
            let view = View::read(machine, at(b));
            let index = machine.mem.word_at(at(c)) as i64;
            let answer = match inline_child(machine, view, index) {
                Some(answer) => answer,
                None => child(machine, view, index)?,
            };
            answer.write(machine, at(a));
            return Ok(());
        }
        KIND => kind(machine, View::read(machine, at(b)))?.code() as u64,
        SAME_TYPE => u64::from(same_type(
            machine,
            View::read(machine, at(b)),
            View::read(machine, at(c)),
        )?),
        // The one opcode whose meaning is its destination's `Repr`: which
        // scalar is read is the word the frame says `a` is, and a view whose
        // kind disagrees is refused rather than reinterpreted.
        READ => {
            let want = machine.program.function(id).repr(a).unwrap_or(Repr::Unit);
            read(machine, View::read(machine, at(b)), want)?
        }
        CASE => case(machine, View::read(machine, at(b)))? as u64,
        COUNT => count(machine, View::read(machine, at(b)))? as u64,
        SAME_OBJECT => u64::from(same_object(
            machine,
            View::read(machine, at(b)),
            View::read(machine, at(c)),
        )),
        NAME_ORDER => match name_order(
            machine,
            View::read(machine, at(b)),
            View::read(machine, at(c)),
        )? {
            Ordering::Less => -1i64 as u64,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
        TYPE_NAME => type_name(machine, View::read(machine, at(b)))?,
        FIELD_NAME => {
            let index = machine.mem.word_at(at(c)) as i64;
            field_name(machine, View::read(machine, at(b)), index)?
        }
        CASE_NAME => case_name(machine, View::read(machine, at(b)))?,
        OPAQUE => u64::from(opaque(machine, View::read(machine, at(b)))?),
        ON_PATH => {
            let entry = machine.mem.word_at(at(c) + PATH_ENTRY as usize);
            let depth = machine.mem.word_at(at(c) + PATH_DEPTH as usize) as i64;
            u64::from(on_path(machine, View::read(machine, at(b)), entry, depth))
        }
        other => unreachable!("{:?} is not a reflection opcode", Op::from_number(other)),
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
    classify(machine.program, described)
}

/// [`kind_of`] over a program rather than a machine: the one classification,
/// which the dispatch loop's reflection arm asks through [`kind_of`] and
/// [`descriptors`] asks once per layout before a run, so that what compiled
/// code reads out of the table is what this arm answers.
fn classify(program: &Program, described: &Layout) -> DynamicKind {
    match &described.shape {
        Shape::Word(Repr::Unit) => DynamicKind::Unit,
        Shape::Word(Repr::Bool) => DynamicKind::Bool,
        Shape::Word(Repr::Int) => DynamicKind::Int,
        Shape::Word(Repr::Float) => DynamicKind::Float,
        Shape::Word(Repr::Duration) => DynamicKind::Duration,
        Shape::Str => DynamicKind::String,
        Shape::Struct { .. } if crate::vm::boundary::is_range(program, described) => {
            DynamicKind::Range
        }
        // An `opaque` struct is a struct too. Its fields are private to the
        // module that declared it, and a view is read only by the standard
        // library's own walks, which compare them as `==` always has and show
        // none of them: a rendering that prints only the name decides that in
        // Cove, not here. Decision 7 is about capabilities — a handle, a task, a
        // cell — and a struct is not one.
        Shape::Struct { .. } => DynamicKind::Struct,
        Shape::Enum { .. } => DynamicKind::Enum,
        Shape::Elements { .. } => DynamicKind::Array,
        Shape::Vector { .. } => DynamicKind::Vector,
        Shape::Members { .. } => DynamicKind::Set,
        Shape::Entries { .. } => DynamicKind::Map,
        Shape::Closure { .. } => DynamicKind::Function,
        // A synchronized cell is Decision 7's too, with a kind of its own so
        // that a rendering can show it as `<shared>` (issue #499's decision 5).
        Shape::Shared { .. } => DynamicKind::Shared,
        // ADR 0068's Decision 7: a Host handle, a task, a scope, an address,
        // a case tag and a byte run are capabilities or machinery, and boxing
        // one does not make it readable.
        Shape::Word(_) | Shape::Bytes | Shape::ByteBuffer | Shape::Boxed | Shape::Free => {
            DynamicKind::Opaque
        }
    }
}

/// Every layout's reflection descriptor, in `LayoutId` order: the table
/// `cove_native::NativeCtx::dyn_layouts` publishes to compiled code.
///
/// One word a layout — its kind code, a number for its declared name, and a
/// struct's or a range's field count — from [`classify`] and
/// [`declared_name`], the two functions [`kind`], [`same_type`] and [`count`]
/// answer from, so an observation compiled code answers out of the table is the
/// one this arm answers. What the table cannot settle it marks
/// `cove_native::DYN_ASK`, and compiled code hands that observation to the
/// reflection helper, which is [`execute`]:
///
/// - a reclaimed layout, which [`layout`] refuses in its own words;
/// - a program with more declared names than the 24 bits hold, whose nominal
///   layouts past the last number all ask — a bound, not a family.
///
/// Built once before the run, and allocating nothing afterwards.
pub(crate) fn descriptors(program: &Program) -> std::sync::Arc<[u64]> {
    let mut names: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    program
        .layouts
        .iter()
        .map(|described| {
            if matches!(described.shape, Shape::Free) {
                return DYN_ASK;
            }
            let kind = classify(program, described);
            let name = if kind.is_nominal() {
                let next = names.len() as u64 + 1;
                let number = *names.entry(declared_name(&described.name)).or_insert(next);
                if number >= 1 << (DYN_COUNT_SHIFT - DYN_NAME_SHIFT) {
                    return DYN_ASK;
                }
                number
            } else {
                0
            };
            let fields = match &described.shape {
                Shape::Struct { fields, .. } => fields.len() as u64,
                _ => 0,
            };
            if fields >= 1 << (64 - DYN_COUNT_SHIFT) {
                return DYN_ASK;
            }
            kind.code() as u64 | name << DYN_NAME_SHIFT | fields << DYN_COUNT_SHIFT
        })
        .collect()
}

/// The two tables compiled code reads its observations out of, built once
/// before a run and shared by every task of it: [`descriptors`] and
/// [`children`].
pub(crate) struct Tables {
    /// `cove_native::NativeCtx::dyn_layouts`: see [`descriptors`].
    pub(crate) descriptors: std::sync::Arc<[u64]>,
    /// `cove_native::NativeCtx::dyn_children`: see [`children`].
    pub(crate) children: Box<[u64]>,
}

/// Both of a program's tables. See [`Tables`].
pub(crate) fn tables(program: &Program) -> std::sync::Arc<Tables> {
    std::sync::Arc::new(Tables {
        descriptors: descriptors(program),
        children: children(program),
    })
}

/// Where every layout's children are: the table
/// `cove_native::NativeCtx::dyn_children` publishes to compiled code, whose
/// documentation is the format.
///
/// Words `0..layouts` are each layout's block, as the word index it begins at,
/// or nought where there is none; the blocks follow. Every entry is made from
/// the layout table [`child`] reads — a field's `at`, a part's `1 + at`, an
/// element's width — and an entry's child is marked
/// [`DYN_SETTLE`](cove_native::DYN_SETTLE) exactly when [`settle`] would have
/// something to do with it: its layout is one address, or is reclaimed, which
/// [`layout`] refuses. So a child compiled code writes unmarked is the child
/// [`child`] answers — `(its layout, the parent's owner, where it begins)`,
/// which [`settle`] returns unchanged for a layout that is not one address.
///
/// Built once before the run, and allocating nothing afterwards.
pub(crate) fn children(program: &Program) -> Box<[u64]> {
    use cove_native::{DYN_OFFSET_SHIFT, DYN_SETTLE};
    let layouts = &program.layouts;
    // A child entry: its layout, its offset, and whether it is inline.
    let entry = |layout: LayoutId, offset: u32| -> u64 {
        let inline = layouts.get(layout.index()).is_some_and(|described| {
            !matches!(described.shape, Shape::Free) && !described.is_one_address()
        });
        match (inline, offset < 1 << 31) {
            (true, true) => u64::from(layout.0) | u64::from(offset) << DYN_OFFSET_SHIFT,
            _ => u64::from(layout.0) | DYN_SETTLE,
        }
    };
    let width = |id: LayoutId| layouts.get(id.index()).map_or(0, Layout::width);
    let mut table = vec![0u64; layouts.len()];
    for (at, described) in layouts.iter().enumerate() {
        let start = table.len() as u64;
        match &described.shape {
            Shape::Struct { fields, .. } if !fields.is_empty() => {
                table.extend(fields.iter().map(|field| entry(field.layout, field.at)));
            }
            Shape::Enum { cases, .. } if !cases.is_empty() => {
                table.push(cases.len() as u64);
                let starts = table.len();
                table.extend(std::iter::repeat_n(0, cases.len()));
                for (nth, case) in cases.iter().enumerate() {
                    table[starts + nth] = table.len() as u64;
                    table.push(case.parts.len() as u64);
                    table.extend(
                        case.parts
                            .iter()
                            .map(|part| entry(part.layout, 1 + part.at)),
                    );
                }
            }
            Shape::Elements { elem, .. } | Shape::Members { elem } | Shape::Vector { elem } => {
                table.push(entry(*elem, width(*elem)));
            }
            Shape::Entries { key, value } => {
                table.push(u64::from(width(*key) + width(*value)));
                table.push(entry(*key, 0));
                table.push(entry(*value, width(*key)));
            }
            _ => continue,
        }
        table[at] = start;
    }
    table.into_boxed_slice()
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

/// `dyn.name-order`: where the name of the value `a` views sorts against
/// `b`'s.
///
/// [`same_type`]'s names, compared rather than matched: the declared name of a
/// struct, an enum or a range — qualified, and with any instantiation left
/// off, so `m.Cell<Duration>` and `m.Cell<Int>` are one name — bytewise, and
/// then for two enums the names of their cases, bytewise. A kind with no name
/// sorts before one with a name and two of them are equal, which is the
/// oracle's `Option` order over the same names.
///
/// Both names are the layout table's and are compared where they are, so this
/// allocates nothing and places no string.
pub(crate) fn name_order(machine: &Machine, a: View, b: View) -> Result<Ordering, RuntimeError> {
    let (left, right) = (layout(machine, a.layout)?, layout(machine, b.layout)?);
    fn name<'l>(machine: &Machine, described: &'l Layout) -> Option<&'l str> {
        kind_of(machine, described)
            .is_nominal()
            .then(|| declared_name(&described.name))
    }
    let named = name(machine, left).cmp(&name(machine, right));
    if named != Ordering::Equal {
        return Ok(named);
    }
    match (&left.shape, &right.shape) {
        (Shape::Enum { .. }, Shape::Enum { .. }) => {
            let (one, other) = (enum_case(machine, a, left)?, enum_case(machine, b, right)?);
            Ok(one.name.as_bytes().cmp(other.name.as_bytes()))
        }
        _ => Ok(Ordering::Equal),
    }
}

/// `dyn.same-object`: whether the two views denote one `Vector` object.
///
/// A view of a heap value is `(its header layout, the object, 0)` once
/// [`settle`] has normalised it, so a view that begins at word 0 of its owner
/// and has its owner's own layout *is* its owner, and two such views with one
/// owner are one object. A view of an inline value — a field, a part, an
/// element of a run, the value inside a box — has a layout its owner's header
/// does not, even at word 0, and is not a heap object of its own. The owner
/// that roots a `Vector`'s view is the object the vector *is*, not its store,
/// which growth replaces, so a vector is recognised however often it has grown.
/// The collector does not move an object, so for as long as both views are
/// live the answer is stable.
///
/// **Only a vector is ever one object with anything.** A vector is the one
/// value whose identity the language can observe — a push through one alias is
/// seen through every other — and so the one whose identity both evaluators
/// agree on. Whether two equal strings or arrays are one object is an
/// allocation decision each evaluator makes its own way, and answering it
/// would let the two disagree about a fact no program is meant to see.
pub(crate) fn same_object(machine: &Machine, a: View, b: View) -> bool {
    let vector = |view: View| {
        view.at == 0
            && machine.mem.object_layout(view.owner) == view.layout
            && matches!(
                machine
                    .program
                    .layouts
                    .get(view.layout.index())
                    .map(|l| &l.shape),
                Some(Shape::Vector { .. })
            )
    };
    a.owner == b.owner && vector(a) && vector(b)
}

/// The names placed for the layout a view names, or the internal error a
/// read of a name `lower::names` never placed is.
///
/// A resource's are not here but in
/// [`cove_ir::Program::resource_names`]: see [`type_name`].
///
/// The pass places exactly the names a box can need, so a view of a struct or
/// an enum always finds its own here; a miss is a lowering bug, and it is
/// refused rather than answered because the alternative is a name made up at
/// run time — an allocation, and a text nobody checked.
fn placed<'p>(machine: &Machine<'p>, layout: LayoutId) -> Result<&'p LayoutNames, RuntimeError> {
    machine
        .program
        .names
        .get(layout.index())
        .ok_or_else(|| unplaced(machine, layout))
}

/// A name `lower::names` did not place for `layout`.
fn unplaced(machine: &Machine, layout: LayoutId) -> RuntimeError {
    let name = machine
        .program
        .layouts
        .get(layout.index())
        .map_or("?", |described| &described.name);
    internal(format!(
        "a rendering asked for a name of `{name}`, and none was placed for it"
    ))
}

/// The address of the placed literal `text`.
fn literal(machine: &Machine, text: StrId) -> u64 {
    machine.literal_addr(text)
}

/// `dyn.type-name`: the name a rendering shows for the struct a view names,
/// or the name a refused key's path begins with for the enum it names, or the
/// type a refused key is called by for the opaque value it names — `Task`,
/// `TaskScope`, a Host resource's `http.Server` — as the address of the
/// literal `lower::names` placed for it.
///
/// A resource's name is found by the module and the kind the run's resource
/// table records for the handle, among [`cove_ir::Program::resource_names`]:
/// every resource shares one layout, so the layout cannot say which. The
/// handle's number is never read, and nothing is allocated (issue #506).
pub(crate) fn type_name(machine: &Machine, view: View) -> Result<u64, RuntimeError> {
    let described = layout(machine, view.layout)?;
    if matches!(described.shape, Shape::Word(Repr::Host)) {
        let word = machine.mem.payload(view.owner, view.at);
        let handle = machine
            .resource(word)
            .ok_or_else(crate::vm::boundary::no_such_resource)?;
        let placed = machine
            .program
            .resource_names
            .iter()
            .find(|named| *named.module == handle.module && *named.resource == handle.type_name)
            .ok_or_else(|| {
                internal(format!(
                    "a refusal asked for the name of a `{}`, and none was placed for it",
                    handle.qualified_type()
                ))
            })?;
        return Ok(literal(machine, placed.text));
    }
    if !matches!(
        kind_of(machine, described),
        DynamicKind::Struct | DynamicKind::Enum | DynamicKind::Opaque
    ) {
        return Err(internal(format!(
            "the name of a dynamic view of a {} was asked",
            kind_of(machine, described).name()
        )));
    }
    let text = placed(machine, view.layout)?
        .name
        .ok_or_else(|| unplaced(machine, view.layout))?;
    Ok(literal(machine, text))
}

/// `dyn.field-name`: the name of field `index` of the struct a view names.
pub(crate) fn field_name(machine: &Machine, view: View, index: i64) -> Result<u64, RuntimeError> {
    let described = layout(machine, view.layout)?;
    let fields = match (&described.shape, kind_of(machine, described)) {
        (Shape::Struct { fields, .. }, DynamicKind::Struct) => fields.len(),
        (_, kind) => {
            return Err(internal(format!(
                "a field name of a dynamic view of a {} was asked",
                kind.name()
            )))
        }
    };
    let Some(at) = usize::try_from(index).ok().filter(|at| *at < fields) else {
        return Err(internal(format!(
            "the name of field {index} of a dynamic view with {fields} fields was asked"
        )));
    };
    let text = *placed(machine, view.layout)?
        .parts
        .get(at)
        .ok_or_else(|| unplaced(machine, view.layout))?;
    Ok(literal(machine, text))
}

/// `dyn.case-name`: the name of the case the enum a view names is in.
pub(crate) fn case_name(machine: &Machine, view: View) -> Result<u64, RuntimeError> {
    let described = layout(machine, view.layout)?;
    if !matches!(described.shape, Shape::Enum { .. }) {
        return Err(internal(format!(
            "the case name of a dynamic view of a {} was asked",
            kind_of(machine, described).name()
        )));
    }
    enum_case(machine, view, described)?;
    let index = machine.mem.payload(view.owner, view.at) as usize;
    let text = *placed(machine, view.layout)?
        .parts
        .get(index)
        .ok_or_else(|| unplaced(machine, view.layout))?;
    Ok(literal(machine, text))
}

/// `dyn.opaque`: whether the struct a view names was declared `opaque`, which
/// its layout carries as a flag; `false` for every other view.
pub(crate) fn opaque(machine: &Machine, view: View) -> Result<bool, RuntimeError> {
    Ok(matches!(
        layout(machine, view.layout)?.shape,
        Shape::Struct { opaque: true, .. }
    ))
}

/// `dyn.on-path`: whether the vector a view names is one of the `depth`
/// entries of the render path whose innermost entry is at address `entry`.
///
/// An entry is word 0 of a `rendersTracked<Vector<…>>` frame — a walk
/// `lower::synth` composed — whose word 0 is the vector it is rendering and
/// whose word 2 is the address of the entry before it. The frames are live:
/// every one of them is a caller of the rendering that is asking, so the walk
/// down them reads words that are there. It compares each entry's vector with
/// the view's object as [`same_object`] compares two views, and a view that
/// does not denote a whole vector is on no path. It allocates nothing and
/// answers nothing but the `Bool`.
pub(crate) fn on_path(machine: &Machine, view: View, entry: u64, depth: i64) -> bool {
    let vector = view.at == 0
        && machine.mem.object_layout(view.owner) == view.layout
        && matches!(
            machine
                .program
                .layouts
                .get(view.layout.index())
                .map(|l| &l.shape),
            Some(Shape::Vector { .. })
        );
    if !vector {
        return false;
    }
    let mut entry = entry;
    for _ in 0..depth.max(0) {
        let at = machine.mem.stack_index(entry);
        if machine.mem.word_at(at) == view.owner {
            return true;
        }
        entry = machine.mem.word_at(at + 2);
    }
    false
}

/// `dyn.handle-text`: the text a rendering shows for the opaque value the
/// view in slot `src` of the frame at `frame` names, written as a new `String`
/// into slot `dst`.
///
/// A Host resource, a task scope and a task are
/// [`crate::vm::intrinsics::handle_text`]'s, which `Inst::HandleText` writes
/// with too, so the two cannot say different things of one handle. A byte
/// run and a byte buffer are the text the runtime's rendering always gave
/// them; an address and a case tag are not values, and are refused in its
/// words. It allocates, so the dispatch loop syncs before it asks.
#[inline(never)]
pub(crate) fn handle_text(
    machine: &mut Machine,
    frame: usize,
    dst: Slot,
    src: Slot,
) -> Result<(), RuntimeError> {
    let view = View::read(machine, frame + src as usize);
    let described = layout(machine, view.layout)?;
    let mut text = String::new();
    match &described.shape {
        Shape::Word(repr @ (Repr::Host | Repr::Scope | Repr::Task)) => {
            let word = machine.mem.payload(view.owner, view.at);
            crate::vm::intrinsics::handle_text(machine, *repr, word, &mut text)?;
        }
        Shape::Bytes => text.push_str("<byte run>"),
        Shape::ByteBuffer => text.push_str("<byte buffer>"),
        Shape::Word(Repr::Addr | Repr::Tag) => {
            return Err(RuntimeError::new("this value has no text of its own"))
        }
        _ => {
            return Err(internal(format!(
                "the opaque text of a dynamic view of a {} was asked",
                kind_of(machine, described).name()
            )))
        }
    }
    let string = machine.new_string(&text)?;
    machine.mem.set_word_at(frame + dst as usize, string);
    Ok(())
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
///
/// The index is bounded by the count [`count`] answers, read from the one fact
/// of the parent's shape it needs — the field count, the current case's parts,
/// the header length, the length word — rather than by asking [`count`], which
/// classifies the layout, and then looking the layout up again. The refusals
/// and their order are [`count`]'s and then the bound's, as they were.
pub(crate) fn child(machine: &Machine, view: View, index: i64) -> Result<View, RuntimeError> {
    let described = layout(machine, view.layout)?;
    let width = |id: LayoutId| machine.program.layout(id).width();
    let within = |count: i64| -> Result<u32, RuntimeError> {
        if index < 0 || index >= count {
            return Err(internal(format!(
                "child {index} of a dynamic view with {count} children was asked"
            )));
        }
        Ok(index as u32)
    };
    match &described.shape {
        Shape::Struct { fields, .. } => {
            let field = &fields[within(fields.len() as i64)? as usize];
            settle(machine, field.layout, view.owner, view.at + field.at)
        }
        Shape::Enum { .. } => {
            let parts = &enum_case(machine, view, described)?.parts;
            let part = &parts[within(parts.len() as i64)? as usize];
            settle(machine, part.layout, view.owner, view.at + 1 + part.at)
        }
        Shape::Elements { elem, .. } | Shape::Members { elem } => {
            let at = within(i64::from(machine.mem.object_len(view.owner)))?;
            settle(machine, *elem, view.owner, at * width(*elem))
        }
        Shape::Entries { key, value } => {
            let at = within(2 * i64::from(machine.mem.object_len(view.owner)))?;
            let entry = (at / 2) * (width(*key) + width(*value));
            if at.is_multiple_of(2) {
                settle(machine, *key, view.owner, entry)
            } else {
                settle(machine, *value, view.owner, entry + width(*key))
            }
        }
        Shape::Vector { elem } => {
            let at = within(machine.mem.payload(view.owner, VECTOR_LEN) as i64)?;
            let store = machine.mem.payload(view.owner, VECTOR_STORE);
            if store == 0 {
                return Err(super::consumed_vector());
            }
            settle(machine, *elem, store, at * width(*elem))
        }
        // Every other kind counts nought, so every index is past it.
        _ => Err(within(0).expect_err("no index is below nought")),
    }
}

/// `dyn.child` of an **inline** child, read out of [`children`] exactly as
/// compiled code reads `cove_native::NativeCtx::dyn_children`: the parent's
/// kind from [`descriptors`], its block, the index bounded by the count, and
/// the child's entry — or `None`, for everything [`child`] has to answer
/// itself: a parent with no children or no block, an index past the count, a
/// case the enum does not have, a consumed vector, and a child marked
/// [`DYN_SETTLE`](cove_native::DYN_SETTLE).
///
/// It is the encoded machine's fast path and the written-down meaning of the
/// native one, and `tests` holds it to [`child`] for every child of every
/// fixture value: an inline child is `(its layout, the parent's owner, where it
/// begins)`, which is what [`settle`] answers of a layout that is not one
/// address once the parent's own words were checked.
pub(crate) fn inline_child(machine: &Machine, view: View, index: i64) -> Option<View> {
    use cove_native::{DYN_OFFSET_SHIFT, DYN_SETTLE};
    let tables = &*machine.reflection;
    let descriptor = *tables.descriptors.get(view.layout.index())?;
    let kind = DynamicKind::from_code((descriptor & cove_native::DYN_KIND_MASK) as i64)?;
    let children = &tables.children;
    let block = *children.get(view.layout.index())? as usize;
    if block == 0 || view.owner == 0 {
        return None;
    }
    let index = u64::try_from(index).ok()?;
    let below = |at: u64, count: u64| (at < count).then_some(());
    // The entry, the word its offset is added to, and the owner.
    let (entry, from, owner) = match kind {
        DynamicKind::Struct | DynamicKind::Range => {
            below(index, descriptor >> DYN_COUNT_SHIFT)?;
            (
                children[block + index as usize],
                u64::from(view.at),
                view.owner,
            )
        }
        DynamicKind::Enum => {
            let case = machine.mem.payload(view.owner, view.at);
            below(case, children[block])?;
            let parts = children[block + 1 + case as usize] as usize;
            below(index, children[parts])?;
            (
                children[parts + 1 + index as usize],
                u64::from(view.at),
                view.owner,
            )
        }
        // A run's element: the entry's offset is the stride.
        DynamicKind::Array | DynamicKind::Set | DynamicKind::Vector => {
            let owner = if kind == DynamicKind::Vector {
                below(index, machine.mem.payload(view.owner, VECTOR_LEN))?;
                machine.mem.payload(view.owner, VECTOR_STORE)
            } else {
                below(index, u64::from(machine.mem.object_len(view.owner)))?;
                view.owner
            };
            let entry = children[block];
            if owner == 0 || entry & DYN_SETTLE != 0 {
                return None;
            }
            return Some(View {
                layout: LayoutId(entry as u32),
                owner,
                at: u32::try_from(index * (entry >> DYN_OFFSET_SHIFT)).ok()?,
            });
        }
        DynamicKind::Map => {
            below(index, 2 * u64::from(machine.mem.object_len(view.owner)))?;
            let stride = children[block];
            (
                children[block + 1 + (index & 1) as usize],
                (index / 2) * stride,
                view.owner,
            )
        }
        _ => return None,
    };
    if entry & DYN_SETTLE != 0 {
        return None;
    }
    Some(View {
        layout: LayoutId(entry as u32),
        owner,
        at: u32::try_from(from + (entry >> DYN_OFFSET_SHIFT)).ok()?,
    })
}

/// An internal runtime error: a view asked a question its kind has no answer
/// to, which the standard library's walk asks the kind first to avoid.
fn internal(message: String) -> RuntimeError {
    RuntimeError::new(format!("internal error: {message}")).with_help(
        "a dynamic view is read by the standard library's own walks, so this is a bug in the \
         standard library rather than in the program",
    )
}

// ---- an audit of the placed names -------------------------------------------

/// Whether every box this process makes is audited for its placed names: see
/// [`audit_box`]. Off unless a survey turns it on.
static AUDITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the audit found: one line per nominal layout a box held whose names
/// `lower::names` did not place.
static UNPLACED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Turns the audit of [`audit_box`] on or off for every machine in this
/// process.
pub(crate) fn audit_placed_names(on: bool) {
    AUDITING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Every finding the audit has made since the last call, taken.
pub(crate) fn unplaced_names() -> Vec<String> {
    std::mem::take(
        &mut *UNPLACED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// Walks the value in the box at `boxed`, which was made a moment ago, and
/// records every nominal layout in it whose names were not placed — when the
/// audit is on, and not otherwise.
///
/// # Why this is not the pass asked again
///
/// `cove_ir`'s `lower::names` decides which layouts a box can hold from the
/// program's code: the layout every `Inst::Box` names and the parts its layout
/// table records, or every layout at all where a Host answers `Any`. A test
/// that asked that closure whether it covered itself would be the pass judging
/// itself. This reads the other side: the box that was **actually made**, by
/// whichever path made it — an `Inst::Box`, or the boundary boxing what a host
/// answered at a layout of its own search — and the value **actually in it**,
/// followed through the headers of the objects it reaches, as a view is. A
/// vector, an array, a set and a map are asked their element's layout too,
/// from their own header, so that a collection empty when it was boxed still
/// answers for what can be pushed into it. A box inside is not followed: it was
/// audited when it was made.
///
/// It is for the survey that runs every program in the repository
/// (`cove-cli`'s `tests/vm_coverage.rs`), and costs a run nothing but one
/// relaxed load a box when it is off.
pub(crate) fn audit_box(machine: &Machine, boxed: u64) {
    if !AUDITING.load(std::sync::atomic::Ordering::Relaxed) || boxed == 0 {
        return;
    }
    let found = unplaced_in(machine, boxed);
    if !found.is_empty() {
        let mut held = UNPLACED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.extend(found);
    }
}

/// [`audit_box`]'s walk of the box at `boxed`, answering the name of every
/// nominal layout in it whose names were not placed.
pub(crate) fn unplaced_in(machine: &Machine, boxed: u64) -> Vec<String> {
    let program = machine.program;
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut checked: std::collections::HashSet<LayoutId> = std::collections::HashSet::new();
    let mut pending: Vec<(LayoutId, u64, u32)> =
        vec![(LayoutId(machine.mem.payload(boxed, 0) as u32), boxed, 1)];
    let mut named = |layout: LayoutId, found: &mut Vec<String>| {
        if !checked.insert(layout) {
            return;
        }
        let Some(described) = program.layouts.get(layout.index()) else {
            return;
        };
        let names = program.names.get(layout.index());
        let whole = match &described.shape {
            Shape::Struct { .. } if crate::vm::boundary::is_range(program, described) => true,
            Shape::Struct { fields, opaque } => names.is_some_and(|names| {
                names.name.is_some() && (*opaque || names.parts.len() == fields.len())
            }),
            Shape::Enum { cases, .. } => {
                names.is_some_and(|names| names.name.is_some() && names.parts.len() == cases.len())
            }
            // The type a refused key is called by (issue #506).
            Shape::Word(Repr::Task | Repr::Scope) => {
                names.is_some_and(|names| names.name.is_some())
            }
            _ => true,
        };
        if !whole {
            found.push(described.name.to_string());
        }
    };
    while let Some((layout, owner, at)) = pending.pop() {
        let Some(described) = program.layouts.get(layout.index()) else {
            continue;
        };
        named(layout, &mut found);
        match &described.shape {
            Shape::Struct { fields, .. } => {
                pending.extend(
                    fields
                        .iter()
                        .map(|field| (field.layout, owner, at + field.at)),
                );
            }
            Shape::Enum { cases, .. } => {
                let index = machine.mem.payload(owner, at) as usize;
                if let Some(case) = cases.get(index) {
                    pending.extend(
                        case.parts
                            .iter()
                            .map(|part| (part.layout, owner, at + 1 + part.at)),
                    );
                }
            }
            // A resource's name is found by its kind, which is the handle's
            // and not the layout's: see [`type_name`].
            Shape::Word(Repr::Host) => {
                let word = machine.mem.payload(owner, at);
                if let Some(handle) = machine.resource(word) {
                    let placed = program.resource_names.iter().any(|named| {
                        *named.module == handle.module && *named.resource == handle.type_name
                    });
                    if !placed {
                        found.push(handle.qualified_type());
                    }
                }
            }
            _ if described.is_one_address() => {
                let object = machine.mem.payload(owner, at);
                if object == 0 || !seen.insert(object) {
                    continue;
                }
                let header = machine.mem.object_layout(object);
                let Some(held) = program.layouts.get(header.index()) else {
                    continue;
                };
                let width = |id: LayoutId| program.layout(id).width();
                match &held.shape {
                    // An object a recursion was broken at holds the value's
                    // own words.
                    Shape::Struct { .. } | Shape::Enum { .. } => pending.push((header, object, 0)),
                    Shape::Elements { elem, .. } | Shape::Members { elem } => {
                        named(*elem, &mut found);
                        let stride = width(*elem);
                        for nth in 0..machine.mem.object_len(object) {
                            pending.push((*elem, object, nth * stride));
                        }
                    }
                    Shape::Vector { elem } => {
                        named(*elem, &mut found);
                        let len = machine.mem.payload(object, VECTOR_LEN) as u32;
                        let store = machine.mem.payload(object, VECTOR_STORE);
                        if store != 0 {
                            let stride = width(*elem);
                            for nth in 0..len {
                                pending.push((*elem, store, nth * stride));
                            }
                        }
                    }
                    Shape::Entries { key, value } => {
                        named(*key, &mut found);
                        named(*value, &mut found);
                        let (keys, values) = (width(*key), width(*value));
                        for nth in 0..machine.mem.object_len(object) {
                            let entry = nth * (keys + values);
                            pending.push((*key, object, entry));
                            pending.push((*value, object, entry + keys));
                        }
                    }
                    // A box inside was audited when it was made; a string, a
                    // closure, a cell and a byte run hold no name a rendering
                    // shows.
                    _ => {}
                }
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests;
