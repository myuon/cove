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

use cove_ir::{Arg, LayoutId};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;

// `Operand` and `Word` stood here: a value location's layout and the words
// at it, borrowed out of the caller's frame, and the one-word `(Repr, u64)`
// left of the operand before it. Their readers were the Rust walks of a
// value's words — `equal`'s equality and `key`'s order — which ADR 0068's
// Phase 5 deleted; every arm left reads its operands a word at a time
// through [`Frame::word`].

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

    /// The first word of operand `at`, read out of the frame.
    #[inline]
    pub(super) fn word(self, machine: &Machine, at: usize) -> u64 {
        machine.operand_word(self.base, self.args[at].slot)
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
    ///
    /// Only this crate's tests write one: `Value.admitKey`'s zero word was the
    /// last arm's until ADR 0068's Phase 4c, and every intrinsic left answers
    /// a `Result` or nothing it writes.
    #[cfg(test)]
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

// `is_word`, and `Frame::layout` beneath it, stood here: the check that an
// operand is one word of a `Repr`, which `float` below asked of its operand.
// Issue #432 took the last word operand an intrinsic read.

// `int`, the `Int` operand `at` is, stood here until issue #432 took
// `String.refuseByteRange`'s two offsets, the last `Int` operands an intrinsic
// read.

// `float`, the `Float` operand `at` is, stood here until issue #432 made
// `Float.toInt` `std.float.toInt`: its receiver was the last `Float` operand
// an intrinsic read.

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

// `type_name`, `object_name` and `layout_name` stood here: what the machine
// called a value a key refusal named — `Float`, `Vector`, `Shared`, `fn`,
// `a task` — read off a word and a layout. Their one caller was
// `key::admit_key`'s wording, and ADR 0068's Phase 4c moved it into
// `std.dynamic.keyWord` for a box and `cove_ir::lower::synth::refused_word`
// for a known layout, which said those words exactly until issue #506 gave
// every evaluator the oracle's: `Task`, `TaskScope`, `http.Server`.

// `null_value` and `reclaimed` stood here: a walk's refusal of a null
// reference and of one into a reclaimed run. Their callers were the Rust
// walks ADR 0068's Phase 5 deleted.

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
