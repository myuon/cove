//! The values a builtin answers with, and the layouts they are built to.
//!
//! A builtin that answers an `Option`, a `Result`, an `Array` or a `Vector`
//! has to name a family, and [`cove_ir::Builtin`] does not carry one for its
//! operands — it names an operation by its receiver and its name rather than
//! by the types the checker resolved for it. So the family is found the way
//! [`crate::vm::boundary`] finds one for an erased destination: by searching
//! the program's layout table.
//!
//! The table describes *families*, so the search is exact rather than a
//! guess. There is one `Option` layout per payload layout, one `Array` layout
//! per element layout, one `Vector` layout per element layout; and the
//! element layout a builtin needs is one it can read — out of the receiver's
//! own layout for `Array.get`, or fixed by the operation for `String.chars`,
//! whose answer is an `Array<String>` whatever it was called on.
//!
//! # An `Option` is words, not an object
//!
//! Under the run-of-words model a fixed-size enum is inline: an
//! `Option<Int>` is `[disc, Int]`, two words *where the value is*. So the
//! builders below answer a run of words rather than an address, and a
//! builtin's result is written into the destination location the same way a
//! `Copy` writes one. Constructing a case **zeroes the payload region it does
//! not fill**, which is what makes the region's static reference map safe.
//!
//! # Everything that allocates, roots
//!
//! An allocation can collect, and a collection walks the frames and the
//! temporary roots and nothing else. A word this module was handed may name
//! an object that *nothing* walks — the string `String.chars` just made, the
//! element `Vector.pop` just took out of a store it then cleared — so it is
//! pushed as a temporary root before the allocation that would otherwise free
//! it, and released once the object that will own it exists. Where a word
//! came out of an operand instead, the caller's frame is already holding it
//! and the doc comment says so rather than rooting it twice.

use cove_ir::{Layout, LayoutId, Program, Shape};
use cove_schema::builtins::{
    ERROR, ERR_CASE, MAP, MESSAGE_FIELD, NONE_CASE, OK_CASE, OPTION, RESULT, SET, SOME_CASE,
};

use crate::error::RuntimeError;
use crate::vm::builtins::operand;
use crate::vm::exec::Machine;

// --- finding a family ------------------------------------------------------

/// The first layout `wanted` accepts.
fn find(program: &Program, wanted: impl Fn(&Layout) -> bool) -> Option<LayoutId> {
    program
        .layouts
        .iter()
        .position(wanted)
        .map(|at| LayoutId(at as u32))
}

/// The layout of a run of `elem` elements, growable or not.
///
/// One shape covers an `Array` and a `Vector`'s store, and `growable` is what
/// tells the two apart — so this is also how `freeze()` finds the `Array` a
/// store becomes and how `push` finds the larger store it grows into.
pub(super) fn elements(
    program: &Program,
    elem: LayoutId,
    growable: bool,
) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        matches!(layout.shape, Shape::Elements { elem: e, growable: g } if e == elem && g == growable)
    })
    .ok_or_else(|| operand::unknown_family(if growable { "Vector" } else { "Array" }))
}

/// The layout of a `Vector` header over `elem` elements.
pub(super) fn vector(program: &Program, elem: LayoutId) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Vector { elem: e } if e == elem),
    )
    .ok_or_else(|| operand::unknown_family("Vector"))
}

/// The layout of a `Set` of `elem`.
///
/// One layout per element layout, as everywhere else, and it is its own shape
/// rather than an `Elements` with a name because "these words are sorted and
/// distinct" is an invariant [`super::keyed`] relies on and an array's words
/// are neither.
pub(super) fn members(program: &Program, elem: LayoutId) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Members { elem: e } if e == elem),
    )
    .ok_or_else(|| operand::unknown_family(SET.name))
}

/// The layout of a `Map` from `key` to `value`.
///
/// One layout per *pair* of layouts: a `Map<String, Int>` traces half its
/// words and a `Map<Int, Int>` none of them, and the collector is told which
/// by the layout rather than by looking.
pub(super) fn entries(
    program: &Program,
    key: LayoutId,
    value: LayoutId,
) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Entries { key: k, value: v } if k == key && v == value),
    )
    .ok_or_else(|| operand::unknown_family(MAP.name))
}

/// The layout of the builtin `Error` struct.
///
/// One `String` field, so a value of it is one inline word: the message's
/// address. An `Error` is not an object of its own under this model, which is
/// why nothing below allocates one.
fn error(program: &Program) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        let Shape::Struct { fields, .. } = &layout.shape else {
            return false;
        };
        &*layout.name == ERROR.name
            && fields.len() == 1
            && &*fields[0].name == MESSAGE_FIELD.name
            && program.layout(fields[0].layout).is_one_address()
    })
    .ok_or_else(|| operand::unknown_family(ERROR.name))
}

/// The `Option` whose `Some` carries a `payload`, and the index of its
/// `case`.
fn option(
    program: &Program,
    payload: LayoutId,
    case: &str,
) -> Result<(LayoutId, u32), RuntimeError> {
    two_case(program, OPTION.name, SOME_CASE.name, payload, case)
        .ok_or_else(|| operand::unknown_family(OPTION.name))
}

/// The `Result` whose `Ok` carries an `ok`, and the index of its `case`.
///
/// The `Err` side is not asked about: every `Result` a builtin answers is a
/// `Result<T, Error>`, and an `Error` is one word whatever it holds.
fn result(program: &Program, ok: LayoutId, case: &str) -> Result<(LayoutId, u32), RuntimeError> {
    two_case(program, RESULT.name, OK_CASE.name, ok, case)
        .ok_or_else(|| operand::unknown_family(RESULT.name))
}

/// The enum called `name` whose case `carrier` holds one `payload`, and the
/// index of its case `wanted`.
///
/// The carrier is what tells one instantiation from another — `Option<Int>`
/// and `Option<String>` are two layouts with one name, and only `Some` is
/// different between them — so it is matched even when the case being asked
/// for is the empty one. A `None` built to the wrong `Option` would be the
/// same discriminant word, but the payload region would be the wrong width
/// and everything that later read it would read the wrong words.
fn two_case(
    program: &Program,
    name: &str,
    carrier: &str,
    payload: LayoutId,
    wanted: &str,
) -> Option<(LayoutId, u32)> {
    for (at, layout) in program.layouts.iter().enumerate() {
        let Shape::Enum { cases, .. } = &layout.shape else {
            continue;
        };
        if &*layout.name != name {
            continue;
        }
        let carries = cases.iter().any(|case| {
            &*case.name == carrier && case.parts.len() == 1 && case.parts[0].layout == payload
        });
        if !carries {
            continue;
        }
        if let Some(index) = layout.case(wanted) {
            return Some((LayoutId(at as u32), index));
        }
    }
    None
}

// --- building one ----------------------------------------------------------

/// The words of a case of the enum `layout`, with `parts` written into the
/// payload region and the rest of it zero.
///
/// The zeroing is not tidiness. The payload region has one static reference
/// map covering every case, so a word this case does not use has to read
/// null — otherwise a `None` would keep alive whatever a `Some` left in the
/// word before it.
fn case_words(
    machine: &Machine,
    layout: LayoutId,
    index: u32,
    parts: &[&[u64]],
) -> Result<Vec<u64>, RuntimeError> {
    let described = machine.program().layout(layout);
    let Shape::Enum { cases, .. } = &described.shape else {
        return Err(operand::unknown_family(&described.name));
    };
    let case = &cases[index as usize];
    let mut words = vec![0; described.width() as usize];
    words[0] = index as u64;
    for (part, held) in case.parts.iter().zip(parts) {
        let at = 1 + part.at as usize;
        words[at..at + held.len()].copy_from_slice(held);
    }
    Ok(words)
}

/// `None`, as an `Option` whose `Some` would carry a `payload`.
pub(super) fn none(machine: &mut Machine, payload: LayoutId) -> Result<Vec<u64>, RuntimeError> {
    let (id, case) = option(machine.program(), payload, NONE_CASE.name)?;
    case_words(machine, id, case, &[])
}

/// `Some(words)`, where `words` is a value of `payload`.
pub(super) fn some(
    machine: &mut Machine,
    payload: LayoutId,
    words: &[u64],
) -> Result<Vec<u64>, RuntimeError> {
    let (id, case) = option(machine.program(), payload, SOME_CASE.name)?;
    case_words(machine, id, case, &[words])
}

/// `Ok(words)`, where `words` is a value of `ok`.
pub(super) fn ok(
    machine: &mut Machine,
    ok: LayoutId,
    words: &[u64],
) -> Result<Vec<u64>, RuntimeError> {
    let (id, case) = result(machine.program(), ok, OK_CASE.name)?;
    case_words(machine, id, case, &[words])
}

/// `Err(Error(message))`, in a `Result` whose `Ok` would carry an `ok`.
///
/// One allocation — the message — because an `Error` is its one `String`
/// field inline and a `Result` is words. That is two objects fewer than the
/// same value cost when every value was an address.
pub(super) fn failed(
    machine: &mut Machine,
    ok: LayoutId,
    message: &str,
) -> Result<Vec<u64>, RuntimeError> {
    let (id, case) = result(machine.program(), ok, ERR_CASE.name)?;
    let carried = error_value(machine, message)?;
    case_words(machine, id, case, &[&carried])
}

/// An `Error` carrying `message`, as its words.
fn error_value(machine: &mut Machine, message: &str) -> Result<Vec<u64>, RuntimeError> {
    // The layout is looked up first, so that a program with no `Error` family
    // is refused before a string is allocated for a value it cannot build.
    error(machine.program())?;
    let text = machine.new_string(message)?;
    Ok(vec![text])
}

/// An `Array` of `elem` holding `words`, which is the elements' words
/// flattened at `elem`'s width.
///
/// The caller holds `words` rooted: every use of this reads them out of an
/// operand — the receiver's own elements, or the arguments themselves — and
/// an operand is a slot of the frame that called the builtin, which the
/// collector already walks.
pub(super) fn array_of(
    machine: &mut Machine,
    elem: LayoutId,
    words: &[u64],
) -> Result<u64, RuntimeError> {
    let id = elements(machine.program(), elem, false)?;
    let stride = machine.words_of(elem).max(1) as usize;
    let addr = machine.new_object(id, (words.len() / stride) as u32)?;
    machine.set_payload_run(addr, 0, words);
    Ok(addr)
}

/// An `Array<String>` of `parts`.
///
/// The array is allocated first and rooted, and each string is written into
/// it as it is made. Nothing is held in a Rust `Vec` across an allocation,
/// because the array's own words are what hold them: the payload is zeroed,
/// so the part of it that is not filled in yet traces nothing, and the part
/// that is holds exactly the strings made so far.
pub(super) fn strings<S: AsRef<str>>(
    machine: &mut Machine,
    parts: &[S],
) -> Result<u64, RuntimeError> {
    let text = machine.program().str_layout;
    let id = elements(machine.program(), text, false)?;
    let addr = machine.new_object(id, parts.len() as u32)?;
    let mark = machine.temps();
    machine.push_temp(addr);
    let filled = (|machine: &mut Machine| {
        for (at, part) in parts.iter().enumerate() {
            let word = machine.new_string(part.as_ref())?;
            machine.set_payload(addr, at as u32, word);
        }
        Ok(())
    })(machine);
    machine.release_temps(mark);
    filled.map(|()| addr)
}

/// A `Vector` of `elem` holding `words`, which the caller holds rooted.
///
/// The store is allocated to exactly the elements it was given. See
/// [`super::seq::grow`] for why it starts there and what happens when it
/// fills.
pub(super) fn vector_of(
    machine: &mut Machine,
    elem: LayoutId,
    words: &[u64],
) -> Result<u64, RuntimeError> {
    let store_layout = elements(machine.program(), elem, true)?;
    let header_layout = vector(machine.program(), elem)?;
    let stride = machine.words_of(elem).max(1) as usize;
    let len = words.len() / stride;
    let store = machine.new_object(store_layout, len as u32)?;
    // The store exists and nothing walks it, and allocating the header can
    // collect. It is released the moment the header exists, because the two
    // writes below cannot allocate and word 1 is what holds it afterwards.
    let mark = machine.temps();
    machine.push_temp(store);
    let header = machine.new_object(header_layout, 0);
    machine.release_temps(mark);
    let header = header?;
    machine.set_payload(header, 0, len as u64);
    machine.set_payload(header, 1, store);
    machine.set_payload_run(store, 0, words);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::tests::{scalar, world};
    use crate::vm::exec::tests::Build;
    use cove_ir::{Repr, Shape};

    /// One layout per payload family, so the `Option` a `Some` is built to is
    /// the one whose `Some` carries that payload — and a `None` is built to
    /// the same family rather than to whichever `Option` came first.
    #[test]
    fn an_option_is_built_to_the_family_its_payload_belongs_to() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let ints = scalar(&program, Repr::Int);
        let text = program.str_layout;

        let string = machine.new_string("x").unwrap();
        let held = some(&mut machine, text, &[string]).unwrap();
        let counted = some(&mut machine, ints, &[1]).unwrap();
        assert_eq!(held, vec![1, string]);
        assert_eq!(counted, vec![1, 1]);

        // `None` fills nothing, and what it does not fill reads null — which
        // is what makes one static reference map right for both cases.
        let empty = none(&mut machine, text).unwrap();
        assert_eq!(empty, vec![0, 0]);
    }

    #[test]
    fn a_failure_carries_an_error_carrying_its_message() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let ints = scalar(&program, Repr::Int);
        let words = failed(&mut machine, ints, "it did not").unwrap();
        // An `Error` is its one `String` field inline, so the payload word
        // *is* the message's address — one object where the old model needed
        // three. Where in the region that word sits is the payload-agreement
        // rule's answer and not a fixture's, so the case is asked.
        let (case, payload) = crate::vm::builtins::tests::result_of(&program, ints, &words);
        assert_eq!(case, "Err");
        assert_eq!(
            String::from_utf8(machine.string_bytes(payload[0])).unwrap(),
            "it did not"
        );
    }

    /// An `Array<Point>` is a run of two-word elements, so the words a
    /// builder is handed are the elements flattened and the header's length
    /// is what the stride divides them into.
    #[test]
    fn a_run_of_multiword_elements_counts_elements_and_not_words() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = crate::vm::builtins::tests::named(&program, "Point");
        let addr = array_of(&mut machine, point, &[1, 2, 3, 4]).unwrap();
        assert_eq!(machine.object_len(addr), 2);
        assert_eq!(machine.payload_run(addr, 0, 4), vec![1, 2, 3, 4]);
    }

    /// A program that never mentions a family has no layout for it. Nothing a
    /// checked program does reaches this — the operation whose result it is
    /// was type-checked, so the lowering interned the layout — which is why
    /// it says the same thing the boundary says rather than blaming the
    /// program.
    #[test]
    fn a_family_the_program_does_not_declare_is_named() {
        let mut build = Build::default();
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;
        let ints = build.word("Int", Repr::Int);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);

        let error = none(&mut machine, ints).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Option` for a value of that shape to be built as"
        );
        let error = array_of(&mut machine, ints, &[]).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Array` for a value of that shape to be built as"
        );
    }
}
