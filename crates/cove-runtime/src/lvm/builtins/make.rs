//! The objects a builtin answers with, and the layouts they are built to.
//!
//! A builtin that answers an `Option`, a `Result`, an `Array` or a `Vector`
//! has to allocate one, and allocating needs a [`LayoutId`] — which
//! [`cove_lir::Builtin`] does not carry, because it names an operation by its
//! receiver and its name rather than by the types the checker resolved for
//! it. So the family is found the way [`crate::lvm::boundary`] finds one: by
//! searching the program's layout table for the family the answer belongs to.
//!
//! The table describes *families*, so the search is exact rather than a
//! guess. There is one `Option` layout per payload `Repr`, one `Array` layout
//! per element `Repr`, one `Vector` layout per element `Repr`; and the
//! element `Repr` a builtin needs is one it can read — out of the receiver's
//! own layout for `Array.get`, out of the operand's `Repr` for
//! `Vector.of(1, 2)`, or fixed by the operation for `String.chars`, whose
//! answer is an `Array<String>` whatever it was called on.
//!
//! # Everything here allocates, so everything here roots
//!
//! An allocation can collect, and a collection walks the frames and the
//! temporary roots and nothing else. A word this module was handed may name
//! an object that *nothing* walks — the string `String.chars` just made, the
//! element `Vector.pop` just took out of a store it then cleared — so it is
//! pushed as a temporary root before the allocation that would otherwise free
//! it, and released once the object that will own it exists. Where a word
//! came out of an operand instead, the caller's frame is already holding it
//! and the doc comment says so rather than rooting it twice.

use cove_lir::{Layout, LayoutId, Program, Repr, Shape};
use cove_schema::builtins::{
    ERROR, ERR_CASE, MAP, MESSAGE_FIELD, NONE_CASE, OK_CASE, OPTION, RESULT, SET, SOME_CASE,
};

use crate::error::RuntimeError;
use crate::lvm::builtins::operand;
use crate::lvm::exec::Machine;

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
    elem: Repr,
    growable: bool,
) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        matches!(layout.shape, Shape::Elements { elem: e, growable: g } if e == elem && g == growable)
    })
    .ok_or_else(|| operand::unknown_family(if growable { "Vector" } else { "Array" }))
}

/// The layout of a `Vector` header over `elem` elements.
pub(super) fn vector(program: &Program, elem: Repr) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Vector { elem: e } if e == elem),
    )
    .ok_or_else(|| operand::unknown_family("Vector"))
}

/// The layout of a `Set` of `elem`.
///
/// One layout per element `Repr`, as everywhere else, and it is its own shape
/// rather than an `Elements` with a name because "these words are sorted and
/// distinct" is an invariant [`super::keyed`] relies on and an array's words
/// are neither.
pub(super) fn members(program: &Program, elem: Repr) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Members { elem: e } if e == elem),
    )
    .ok_or_else(|| operand::unknown_family(SET.name))
}

/// The layout of a `Map` from `key` to `value`.
///
/// One layout per *pair* of `Repr`s: a `Map<String, Int>` traces half its
/// words and a `Map<Int, Int>` none of them, and the collector is told which
/// by the layout rather than by looking.
pub(super) fn entries(program: &Program, key: Repr, value: Repr) -> Result<LayoutId, RuntimeError> {
    find(
        program,
        |layout| matches!(layout.shape, Shape::Entries { key: k, value: v } if k == key && v == value),
    )
    .ok_or_else(|| operand::unknown_family(MAP.name))
}

/// The layout of the builtin `Error` struct.
fn error(program: &Program) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        let Shape::Struct { fields, .. } = &layout.shape else {
            return false;
        };
        &*layout.name == ERROR.name
            && fields.len() == 1
            && &*fields[0].name == MESSAGE_FIELD.name
            && fields[0].repr == Repr::Ref
    })
    .ok_or_else(|| operand::unknown_family(ERROR.name))
}

/// The `Option` whose `Some` carries one word of `payload`, and the index of
/// its `case`.
fn option(program: &Program, payload: Repr, case: &str) -> Result<(LayoutId, u32), RuntimeError> {
    two_case(program, OPTION.name, SOME_CASE.name, payload, case)
        .ok_or_else(|| operand::unknown_family(OPTION.name))
}

/// The `Result` whose `Ok` carries one word of `ok`, and the index of its
/// `case`.
///
/// The `Err` side is not asked about: every `Result` a builtin answers is a
/// `Result<T, Error>`, and an `Error` is a reference whatever it holds.
fn result(program: &Program, ok: Repr, case: &str) -> Result<(LayoutId, u32), RuntimeError> {
    two_case(program, RESULT.name, OK_CASE.name, ok, case)
        .ok_or_else(|| operand::unknown_family(RESULT.name))
}

/// The enum called `name` whose case `carrier` holds one word of `payload`,
/// and the index of its case `wanted`.
///
/// The carrier is what tells one instantiation from another — `Option<Int>`
/// and `Option<String>` are two layouts with one name, and only `Some` is
/// different between them — so it is matched even when the case being asked
/// for is the empty one. A `None` built to the wrong `Option` would be the
/// same word, but the object would answer the wrong layout to everything that
/// later read it.
fn two_case(
    program: &Program,
    name: &str,
    carrier: &str,
    payload: Repr,
    wanted: &str,
) -> Option<(LayoutId, u32)> {
    for (at, layout) in program.layouts.iter().enumerate() {
        let Shape::Enum { cases } = &layout.shape else {
            continue;
        };
        if &*layout.name != name {
            continue;
        }
        let carries = cases.iter().any(|case| {
            &*case.name == carrier && case.payload.len() == 1 && case.payload[0] == payload
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

/// `None`, as an `Option` whose `Some` would carry one word of `payload`.
pub(super) fn none(machine: &mut Machine, payload: Repr) -> Result<u64, RuntimeError> {
    let (id, case) = option(machine.program(), payload, NONE_CASE.name)?;
    let addr = machine.new_object(id, 0)?;
    machine.set_payload(addr, 0, case as u64);
    Ok(addr)
}

/// `Some(word)`, where `word` is one word of `payload`.
pub(super) fn some(machine: &mut Machine, payload: Repr, word: u64) -> Result<u64, RuntimeError> {
    let (id, case) = option(machine.program(), payload, SOME_CASE.name)?;
    let addr = held(machine, payload, word, id)?;
    machine.set_payload(addr, 0, case as u64);
    machine.set_payload(addr, 1, word);
    Ok(addr)
}

/// `Ok(word)`, where `word` is one word of `ok`.
pub(super) fn ok(machine: &mut Machine, ok: Repr, word: u64) -> Result<u64, RuntimeError> {
    let (id, case) = result(machine.program(), ok, OK_CASE.name)?;
    let addr = held(machine, ok, word, id)?;
    machine.set_payload(addr, 0, case as u64);
    machine.set_payload(addr, 1, word);
    Ok(addr)
}

/// `Err(Error(message))`, in a `Result` whose `Ok` would carry one word of
/// `ok`.
///
/// Three allocations deep — the message, the `Error` that holds it, the
/// `Result` that holds that — and each one can collect what the last
/// produced, which is why each is rooted across the next.
pub(super) fn failed(machine: &mut Machine, ok: Repr, message: &str) -> Result<u64, RuntimeError> {
    let (id, case) = result(machine.program(), ok, ERR_CASE.name)?;
    let carried = error_value(machine, message)?;
    let addr = held(machine, Repr::Ref, carried, id)?;
    machine.set_payload(addr, 0, case as u64);
    machine.set_payload(addr, 1, carried);
    Ok(addr)
}

/// An `Error` carrying `message`.
fn error_value(machine: &mut Machine, message: &str) -> Result<u64, RuntimeError> {
    let id = error(machine.program())?;
    let text = machine.new_string(message)?;
    let addr = held(machine, Repr::Ref, text, id)?;
    machine.set_payload(addr, 0, text);
    Ok(addr)
}

/// A new object of `layout`, allocated with `word` held as a root.
///
/// The one line every wrapper above shares, and the reason each of them is a
/// function rather than a `set_payload` at the call site: between reading
/// `word` and writing it into the object that will own it there is an
/// allocation, and a `word` that is a reference nothing else names would not
/// survive one.
fn held(
    machine: &mut Machine,
    repr: Repr,
    word: u64,
    layout: LayoutId,
) -> Result<u64, RuntimeError> {
    let mark = machine.temps();
    if repr.is_ref() {
        machine.push_temp(word);
    }
    let addr = machine.new_object(layout, 0);
    machine.release_temps(mark);
    addr
}

/// An `Array` of `elem` holding `words`.
///
/// The caller holds `words` rooted: every use of this reads them out of an
/// operand — the receiver's own elements, or the arguments themselves — and
/// an operand is a slot of the frame that called the builtin, which the
/// collector already walks.
pub(super) fn array_of(
    machine: &mut Machine,
    elem: Repr,
    words: &[u64],
) -> Result<u64, RuntimeError> {
    let id = elements(machine.program(), elem, false)?;
    let addr = machine.new_object(id, words.len() as u32)?;
    for (at, word) in words.iter().enumerate() {
        machine.set_payload(addr, at as u32, *word);
    }
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
    let id = elements(machine.program(), Repr::Ref, false)?;
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
    elem: Repr,
    words: &[u64],
) -> Result<u64, RuntimeError> {
    let store_layout = elements(machine.program(), elem, true)?;
    let header_layout = vector(machine.program(), elem)?;
    let store = machine.new_object(store_layout, words.len() as u32)?;
    // The store exists and nothing walks it, and allocating the header can
    // collect. It is released the moment the header exists, because the two
    // writes below cannot allocate and word 1 is what holds it afterwards.
    let mark = machine.temps();
    machine.push_temp(store);
    let header = machine.new_object(header_layout, 0);
    machine.release_temps(mark);
    let header = header?;
    machine.set_payload(header, 0, words.len() as u64);
    machine.set_payload(header, 1, store);
    for (at, word) in words.iter().enumerate() {
        machine.set_payload(store, at as u32, *word);
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::tests::{case_of, world};
    use crate::lvm::exec::tests::Build;
    use cove_lir::Shape;

    /// One layout per payload `Repr`, so the family a `Some` is built to is
    /// the one whose `Some` carries that word — and a `None` is built to the
    /// same family rather than to whichever `Option` came first.
    #[test]
    fn an_option_is_built_to_the_family_its_payload_belongs_to() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);

        let text = machine.new_string("x").unwrap();
        let held = some(&mut machine, Repr::Ref, text).unwrap();
        let counted = some(&mut machine, Repr::Int, 1).unwrap();
        assert_ne!(machine.object_layout(held), machine.object_layout(counted));
        assert_eq!(case_of(&machine, held), ("Some".to_string(), vec![text]));

        let empty = none(&mut machine, Repr::Int).unwrap();
        assert_eq!(machine.object_layout(empty), machine.object_layout(counted));
        let empty = none(&mut machine, Repr::Ref).unwrap();
        assert_eq!(machine.object_layout(empty), machine.object_layout(held));
    }

    #[test]
    fn a_failure_carries_an_error_carrying_its_message() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = failed(&mut machine, Repr::Int, "it did not").unwrap();
        let (case, payload) = case_of(&machine, word);
        assert_eq!(case, "Err");
        let message = machine.payload(payload[0], 0);
        assert_eq!(
            String::from_utf8(machine.string_bytes(message)).unwrap(),
            "it did not"
        );
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
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);

        let error = none(&mut machine, Repr::Int).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Option` for a value of that shape to be built as"
        );
        let error = array_of(&mut machine, Repr::Int, &[]).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Array` for a value of that shape to be built as"
        );
    }
}
