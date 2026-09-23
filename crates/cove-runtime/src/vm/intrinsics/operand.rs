//! How a builtin reads the words it was handed, and what it refuses when it
//! cannot.
//!
//! An operand is a layout and a run of words, because a word is untagged and
//! the location it came out of is the only thing that says what it means.
//! Reading one as an `Int`, as text, or as a receiver of a particular family
//! is therefore a question with two answers — the value, or a refusal — and
//! both halves are here so that the refusal is written once.
//!
//! # The messages are the oracle's
//!
//! Every message below that names a method is
//! [`crate::builtins`]' word for word, because a refusal is the *language's*
//! and not a backend's: the differential corpus runs the same program on both
//! and compares the text. Where a message here has no counterpart there, it
//! is about something only this representation can go wrong at — a null
//! reference, an object the collector already reclaimed, a family the program
//! does not declare — and the doc comment says so.
//!
//! # The operands are not re-checked
//!
//! `cove-sema` settled every receiver's type, every argument's type and every
//! call's arity, and `cove_ir::verify` holds every `IntrinsicCall` to its
//! intrinsic's [`cove_ir::Signature`] — the count, each operand's layout and
//! the answer's — before anything runs (#378, P5-3). So nothing here refuses
//! an operand for its count or its type any more: the readers below
//! `debug_assert!` what the verifier established, which the `checked` profile
//! every test and every measurement runs under keeps on, and read the word.

use cove_ir::{Arg, LayoutId, Repr, Shape};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;

/// One operand: the layout of the value location an argument names, and the
/// words at it.
///
/// The pair travels together everywhere, because neither half means anything
/// without the other — a word is untagged, and a layout describes nothing on
/// its own.
///
/// The words are borrowed **straight out of the caller's frame** — see
/// [`Frame::operand`] — and not out of a buffer they were copied into, so an
/// `Operand` lives no longer than the shared borrow of the machine it was
/// read through. That is also what makes the aliasing contract hold by
/// construction for a wide operand: nothing that writes the destination can
/// run while one is held.
///
/// It used to be a `Repr` and one word, and that was the shape of a call
/// rather than a choice this file made: an `IntrinsicCall`'s argument list was
/// base slots, so nothing said how wide an operand was. A scalar described
/// itself from its slot and a reference from its object's header, and an
/// inline struct or enum described itself from neither — so `"{p}"` rendered
/// a `Point`'s first word, `a == b` compared it, and the six operations that
/// put a whole value into a collection refused rather than store half of one.
/// [`cove_ir::Arg`] carries the layout now and all of those read the value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Operand<'w> {
    pub layout: LayoutId,
    pub words: &'w [u64],
}

/// One word, and the `Repr` that says what it means.
///
/// What is left of the old operand, and it is still the right currency for
/// the two things that genuinely are one word: a scalar comparison, and the
/// address a value of a family that lives in the heap consists of.
pub(super) type Word = (Repr, u64);

/// The operands of one intrinsic call, where they already are: the caller's
/// frame, and the argument list the instruction names.
///
/// ADR 0058: a runtime call does not "allocate an operand vector, or copy a
/// variable result through an untyped temporary solely to cross the
/// boundary". This is the half of that about operands (#378, P5-4). Nothing
/// is copied to make one: it is a frame base and the program's own
/// [`cove_ir::Arg`] list, and an arm reads operand `n` by reading the frame —
/// [`Frame::word`] for a one-word operand, [`Frame::operand`] for a value
/// location as wide as its layout.
///
/// # Every operand is read before the answer is written
///
/// The destination may be one of the operands' own slots — `x = x.trim()`
/// lowers to a call whose destination is `x` (#378, Q5.2). So an arm reads
/// everything it needs from the frame before its first write through
/// [`Dest`], and one that has to read after writing copies what it needs
/// first. The machine holds the contract under `debug_assertions`, which the
/// `checked` profile keeps on: a read through a `Frame` after a write through
/// the call's `Dest` panics.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame<'a> {
    base: u64,
    args: &'a [Arg],
}

impl<'a> Frame<'a> {
    /// The operands `args` names in the frame based at `base`.
    pub(crate) fn new(base: u64, args: &'a [Arg]) -> Frame<'a> {
        Frame { base, args }
    }

    /// The layout of operand `at`.
    pub(super) fn layout(self, at: usize) -> LayoutId {
        self.args[at].layout
    }

    /// The first word of operand `at`, read out of the frame.
    #[inline]
    pub(super) fn word(self, machine: &Machine, at: usize) -> u64 {
        machine.operand_word(self.base, self.args[at].slot)
    }

    /// Operand `at` as the value location it is: its layout, and its words
    /// borrowed out of the frame.
    #[inline]
    pub(super) fn operand<'m>(self, machine: &'m Machine, at: usize) -> Operand<'m> {
        let arg = self.args[at];
        Operand {
            layout: arg.layout,
            words: machine.operand_words(self.base, arg.slot, arg.layout),
        }
    }
}

/// Where an intrinsic's answer goes: slot `slot` of the frame based at
/// `base`, a value of `layout`.
///
/// The other half of ADR 0058's sentence: "Results are written directly to
/// the destination named by the slot ABI." An arm writes its answer here —
/// [`Dest::word`] for a one-word answer, `make`'s case builders for an
/// `Option` or a `Result` — and nothing carries it home afterwards.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Dest {
    base: u64,
    slot: u32,
    layout: LayoutId,
}

impl Dest {
    /// Slot `slot` of the frame based at `base`, for a value of `layout`.
    pub(crate) fn new(base: u64, slot: u32, layout: LayoutId) -> Dest {
        Dest { base, slot, layout }
    }

    /// The layout the answer is a value of — the instruction's, which is the
    /// only thing that tells two instantiations of one family apart.
    pub(super) fn layout(self) -> LayoutId {
        self.layout
    }

    /// Writes a one-word answer.
    #[inline]
    pub(super) fn word(self, machine: &mut Machine, word: u64) {
        machine.answer_word(self.base, self.slot, word);
    }

    /// The destination's `width` words, to be written in place.
    ///
    /// The run is the frame's own, so it is borrowed out of the machine and
    /// nothing that reads the frame can run while it is held.
    pub(super) fn run<'m>(self, machine: &'m mut Machine, width: u32) -> &'m mut [u64] {
        machine.answer_words(self.base, self.slot, width)
    }
}

/// Whether operand `at` of `frame` is one word of `repr`, which is what the
/// verifier held it to.
fn is_word(machine: &Machine, frame: Frame<'_>, at: usize, repr: Repr) -> bool {
    machine.program().layout(frame.layout(at)).shape == Shape::Word(repr)
}

/// The `Int` operand `at` is.
#[inline]
pub(super) fn int(machine: &Machine, frame: Frame<'_>, at: usize) -> i64 {
    debug_assert!(
        is_word(machine, frame, at, Repr::Int),
        "an operand verified to be an `Int`"
    );
    frame.word(machine, at) as i64
}

/// The `Float` operand `at` is.
#[inline]
pub(super) fn float(machine: &Machine, frame: Frame<'_>, at: usize) -> f64 {
    debug_assert!(
        is_word(machine, frame, at, Repr::Float),
        "an operand verified to be a `Float`"
    );
    f64::from_bits(frame.word(machine, at))
}

/// The address of the `String` operand `at` is.
#[inline]
pub(super) fn string(machine: &Machine, frame: Frame<'_>, at: usize) -> u64 {
    let addr = frame.word(machine, at);
    debug_assert!(
        super::is_string(machine, addr),
        "an operand verified to be a `String`"
    );
    addr
}

/// The text of the `String` operand `at` is.
///
/// The `Err` is a string object whose bytes are not UTF-8, which nothing that
/// builds one can make.
pub(super) fn text(machine: &Machine, frame: Frame<'_>, at: usize) -> Result<String, RuntimeError> {
    super::string_of(machine, string(machine, frame, at))
}

// `with_text` stood here: the text of a `String` operand handed to a closure
// out of a buffer the machine already owned, so that a steady-state run
// allocated nothing to read an operand. It was #446's answer to #442's
// finding that the *allocation* in [`text`] was about seventy per cent of the
// cost of reading one.
//
// It is gone three pull requests later, and not because the measurement was
// wrong. Its callers were the two whole-haystack searches, and both have since
// migrated out of this crate's arms: ADR 0065 gave `String.contains` an
// `Inst::RunFind` to stand on, which reads both runs where they are through a
// one-word cache, and then ADR 0064's `String.indexOf` migration took the
// last one. A mechanism with no caller cannot be measured and is not kept
// against a caller that might arrive; `Machine`'s scratch pool went with it.
// [`text`] is what the arms that are left use, as they always did.

/// What the language calls the value in `word`, read as `repr`.
///
/// [`crate::value::Value::type_name`] is the oracle's copy of this, and the
/// two are written twice for the reason [`super`]'s rendering is: one reads a
/// materialised tree and one reads the heap, and neither can be had from the
/// other without building what the other exists to avoid.
///
/// A family is named by its *shape* rather than by its layout's name wherever
/// the shape decides it — an `Array` is an `Array` whatever the lowering
/// called the layout — and by the layout's name for a struct or an enum,
/// where the name is the declaration's and is the whole of what the reader
/// wants.
pub(super) fn type_name(machine: &Machine, repr: Repr, word: u64) -> String {
    match repr {
        Repr::Unit => "Unit".to_string(),
        Repr::Bool => "Bool".to_string(),
        Repr::Int => "Int".to_string(),
        Repr::Float => "Float".to_string(),
        Repr::Duration => "Duration".to_string(),
        Repr::Ref => object_name(machine, word, 0),
        // Neither is a value, so neither has a type the language names. A
        // message that reached one is reporting on this run's bookkeeping,
        // which is a lowering bug, and saying so is more use than a type.
        Repr::Addr => "a place".to_string(),
        Repr::Host => "a host resource".to_string(),
        Repr::Task => "a task".to_string(),
        Repr::Scope => "a task scope".to_string(),
        Repr::Tag => "an enum case".to_string(),
    }
}

/// What the object at `addr` is called.
fn object_name(machine: &Machine, addr: u64, depth: usize) -> String {
    if addr == 0 {
        return "nothing".to_string();
    }
    if depth >= super::MAX_DEPTH {
        return "a value that nests too deeply to name".to_string();
    }
    let id = machine.object_layout(addr);
    let layout = machine.program().layout(id);
    match &layout.shape {
        // Erasure is looked through, because `Value::type_name` is asked of
        // an `erased()` value everywhere a comparison or a refusal asks it.
        // Payload word 0 is the layout of what the box holds, so the name is
        // that layout's — one lookup rather than a tag and a guess.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            match machine.program().layouts.get(held.index()) {
                Some(_) => layout_name(machine, held, machine.payload(addr, 1), depth + 1),
                None => "a value of no known type".to_string(),
            }
        }
        _ => layout_name(machine, id, addr, depth),
    }
}

/// What a value location of `layout` is called, given its first word.
///
/// A family is named by its *shape* wherever the shape decides it — an
/// `Array` is an `Array` whatever the lowering called the layout — and by the
/// layout's name for a struct or an enum, where the name is the
/// declaration's and is the whole of what the reader wants.
pub(super) fn layout_name(machine: &Machine, layout: LayoutId, first: u64, depth: usize) -> String {
    let described = machine.program().layout(layout);
    match &described.shape {
        Shape::Word(repr) => type_name(machine, *repr, first),
        Shape::Str => "String".to_string(),
        // Reached only by a debugger or an internal error message: no source
        // expression ever holds one of these, so there is no name a Cove
        // program would recognise. See `Shape::Bytes`.
        Shape::Bytes => "<byte run>".to_string(),
        // Likewise: an owner is named by the standard-library wrapper a program
        // declares over it, and until there is one there is no name to give.
        Shape::ByteBuffer => "<byte buffer>".to_string(),
        Shape::Struct { .. } | Shape::Enum { .. } => described.name.to_string(),
        Shape::Elements { growable, .. } => if *growable { "Vector" } else { "Array" }.to_string(),
        Shape::Vector { .. } => "Vector".to_string(),
        Shape::Members { .. } => "Set".to_string(),
        Shape::Entries { .. } => "Map".to_string(),
        Shape::Closure { .. } => "fn".to_string(),
        // `Value::type_name`'s word for one. A cell is a handle, and what it
        // holds is reachable only under a `lock`, so the name is the handle's.
        Shape::Shared { .. } => "Shared".to_string(),
        Shape::Boxed => object_name(machine, first, depth + 1),
        Shape::Free => "nothing".to_string(),
    }
}

/// `Float.format` refused a `digits` outside `0..=17`.
pub(super) fn format_digits(digits: i64) -> RuntimeError {
    RuntimeError::new(format!("`Float.format` cannot use `{digits}` digits")).with_rule(
        "A Float carries at most 17 significant decimal digits, so `digits` must be between 0 and 17.",
    )
}

/// A reference slot that was read before anything was written to it.
///
/// Not the oracle's: a `Value` is never absent, and a null `Repr::Ref` is
/// this representation's own way of being so. [`crate::vm::exec`] answers a
/// null object in these words and this is the same event.
pub(super) fn null_value() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

/// A reference into a run of words the sweeper reclaimed.
pub(super) fn reclaimed() -> RuntimeError {
    RuntimeError::new("this value was read after it was reclaimed")
}

/// A builtin has to build a value of a family this program does not declare.
///
/// [`crate::vm::boundary`] refuses in these words for the same reason: a
/// layout table describes the families a program *uses*, and a program that
/// never mentions an `Option<Int>` has no layout for one. Unlike the
/// boundary's, this one is unreachable from a checked program — the operation
/// whose result it is was type-checked, so the lowering interned the layout —
/// which makes it a lowering bug rather than a host's mistake.
pub(crate) fn unknown_family(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this program describes no `{name}` for a value of that shape to be built as"
    ))
    .with_rule(
        "A layout describes a family of values, and a program declares the families it uses.",
    )
}
