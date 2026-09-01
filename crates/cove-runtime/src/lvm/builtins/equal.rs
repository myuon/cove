//! Value equality, over runs of words and over heap objects.
//!
//! `==` on anything that is not one word of scalar bits lowers to a call
//! here. [`crate::value::Value::eq_value`] is the oracle's copy of the same
//! rule, and this is written twice for the reason the rendering beside it is:
//! that one walks a materialised tree and this one walks the words the
//! machine holds, and neither can be had from the other without building what
//! the other exists to avoid.
//!
//! # A value is a run of words, so a comparison is layout-driven
//!
//! A struct is its fields in place and an enum is a discriminant and a
//! payload region, so comparing two of either is comparing runs of words
//! under the layout that describes them. There is no object to follow and no
//! header to read: `Point(1, 2) == Point(1, 2)` reads four words and nothing
//! else. Only the families that live in the heap are one address, and those
//! are the ones this walks objects for — a string's bytes, an array's
//! elements at the element's own stride, a map's entries key before value.
//!
//! That is why there are two entry points rather than one. [`same_value`]
//! compares two values of a layout, which is what everything *inside* a value
//! is. [`same_word`] compares two operands, which is all a builtin's
//! arguments ever are: [`cove_lir::Builtin`] carries no layout for an operand
//! and a `CallBuiltin`'s arguments are base slots that need not be adjacent,
//! so one word and the `Repr` of the slot it came out of is the whole of what
//! a caller can hand over.
//!
//! # This answers equality; it does not police types
//!
//! [`crate::interp`] refuses `1 == "a"` *before* it asks
//! [`crate::value::Value::eq_value`], because `==` is an operator over two
//! written expressions and the check belongs where the types are. That check
//! is the checker's, and by the time a program reaches this backend it has
//! passed. So what is implemented here is `eq_value` and only `eq_value`:
//! everything it answers `false` for is answered `false` here, including two
//! values of different families, rather than raised. A refusal here would be
//! a second opinion about a question the checker already settled, and it
//! would be wrong in the one place `eq_value` is right — inside a comparison,
//! where two values of different families is an answer and not an error.
//!
//! What is raised is what only this representation can go wrong at: a null
//! reference, an object the sweeper reclaimed, a run of words narrower than
//! the layout that describes it, a box naming a family the program does not
//! have, and a graph that nests deeper than a walk of it can.

use cove_lir::{LayoutId, Program, Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::builtins::operand::{self, Operand};
use crate::lvm::exec::Machine;

/// A value: the layout that describes it, and the words it occupies.
///
/// The pair travels together for the reason an [`Operand`] does — a word is
/// untagged, and a layout describes nothing on its own.
type Held<'w> = (LayoutId, &'w [u64]);

/// `a == b`, as the `Bool` word `0` or `1`.
pub(super) fn equals(machine: &Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let [a, b] = operands else {
        return Err(operand::operands("Any.equals", 2, operands.len()));
    };
    Ok(same_word(machine, *a, *b)? as u64)
}

/// Whether two values of `layout` are equal, given their words.
///
/// What every reader of a value inside another value asks: an array's
/// element, a struct's field, an enum's part, the value inside a box.
pub(super) fn same_value(
    machine: &Machine,
    layout: LayoutId,
    a: &[u64],
    b: &[u64],
) -> Result<bool, RuntimeError> {
    value(machine, (layout, a), (layout, b), 0)
}

/// Whether two one-word operands are equal.
///
/// What a builtin asks, because a builtin's operands are one word each. See
/// the module docs for why that is not a restriction this file chose.
pub(super) fn same_word(machine: &Machine, a: Operand, b: Operand) -> Result<bool, RuntimeError> {
    word(machine, a, b, 0)
}

/// Whether the two values are equal, each read as its own layout.
///
/// The two layouts need not be the same one, and [`same_value`] passing one
/// twice is the common case rather than the only one: erasure is looked
/// through on either side, and two boxes need not record the same family.
/// `None` out of an `Option<Int>` and `None` out of an `Option<String>` are
/// two layouts and one value, which is what comparing the *names* of the
/// declaration and of the case says and what comparing indices would not.
fn value(machine: &Machine, x: Held<'_>, y: Held<'_>, depth: usize) -> Result<bool, RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let (left, right) = (program.layout(x.0), program.layout(y.0));
    if matches!(left.shape, Shape::Free) || matches!(right.shape, Shape::Free) {
        return Err(operand::reclaimed());
    }
    Ok(match (&left.shape, &right.shape) {
        // One word each, compared as the operands they would be if they had
        // arrived as arguments — which is also what looks through a box.
        (Shape::Word(one), Shape::Word(other)) => {
            return word(
                machine,
                (*one, super::at(x.1, 0)?),
                (*other, super::at(y.1, 0)?),
                deeper,
            )
        }
        // Fields are compared by position and not by name, which is the
        // oracle's own reading: two values of one declared type have one
        // field order, and the names would be the same comparison done twice.
        // Each field is the run of words its layout claims, so a nested
        // struct is compared where it sits.
        (Shape::Struct { fields: one, .. }, Shape::Struct { fields: other, .. }) => {
            if left.name != right.name || one.len() != other.len() {
                return Ok(false);
            }
            for (field, counterpart) in one.iter().zip(other) {
                if !value(
                    machine,
                    part(program, x, field.layout, field.at)?,
                    part(program, y, counterpart.layout, counterpart.at)?,
                    deeper,
                )? {
                    return Ok(false);
                }
            }
            true
        }
        // Word 0 is the discriminant and the words after it are the payload
        // region, of which the case says which belong to this value. The case
        // is compared by *name*, as the oracle compares it, because two
        // `Option` layouts are two instantiations of one declared enum and an
        // index means the same thing in both only by construction. A name
        // means it by declaration.
        (Shape::Enum { cases: one, .. }, Shape::Enum { cases: other, .. }) => {
            if left.name != right.name {
                return Ok(false);
            }
            let (index, counterindex) = (super::at(x.1, 0)?, super::at(y.1, 0)?);
            let (Some(case), Some(countercase)) =
                (one.get(index as usize), other.get(counterindex as usize))
            else {
                return Err(wrong_case(&left.name));
            };
            if case.name != countercase.name || case.parts.len() != countercase.parts.len() {
                return Ok(false);
            }
            for (held, counterpart) in case.parts.iter().zip(&countercase.parts) {
                // A part's offset is within the payload region, which begins
                // after the discriminant.
                if !value(
                    machine,
                    part(program, x, held.layout, 1 + held.at)?,
                    part(program, y, counterpart.layout, 1 + counterpart.at)?,
                    deeper,
                )? {
                    return Ok(false);
                }
            }
            true
        }
        // A struct and an enum are the two inline families, so a pair that is
        // not two of one of them is two values of different families. Width
        // does not decide it: an `Error` is one word — its message's address —
        // and is still not a `String`.
        (Shape::Struct { .. } | Shape::Enum { .. }, _)
        | (_, Shape::Struct { .. } | Shape::Enum { .. }) => false,
        // Everything left lives in the heap, so each side is one address.
        _ if left.is_ref() && right.is_ref() => {
            return word(
                machine,
                (Repr::Ref, super::at(x.1, 0)?),
                (Repr::Ref, super::at(y.1, 0)?),
                deeper,
            )
        }
        _ => false,
    })
}

/// The words of the part of `held` that begins at `at` and is a value of
/// `layout`.
fn part<'w>(
    program: &Program,
    held: Held<'w>,
    layout: LayoutId,
    at: u32,
) -> Result<Held<'w>, RuntimeError> {
    let from = at as usize;
    let width = program.layout(layout).width() as usize;
    let words = held
        .1
        .get(from..from + width)
        .ok_or_else(|| super::short_run(&program.layout(held.0).name))?;
    Ok((layout, words))
}

/// Whether the two operands are the same value.
fn word(machine: &Machine, a: Operand, b: Operand, depth: usize) -> Result<bool, RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    // Erasure is looked through on either side before anything is compared,
    // which is `eq_value`'s first move as well: a `dyn Display` holding `1`
    // and an `Int` `1` are one value, and where the checker put the wrapper
    // is not something a program can ask about.
    //
    // What comes out of a box is a layout and a run of words rather than
    // another word, because a box holds its value *inline* — so the pair goes
    // to [`value`] and not back through here.
    match (unboxed(machine, a)?, unboxed(machine, b)?) {
        (Some(one), Some(other)) => {
            return value(machine, (one.0, &one.1), (other.0, &other.1), deeper)
        }
        (Some(one), None) => return held(machine, one, b, deeper),
        (None, Some(other)) => return held(machine, other, a, deeper),
        (None, None) => {}
    }
    Ok(match (a.0, b.0) {
        (Repr::Unit, Repr::Unit) => true,
        (Repr::Bool, Repr::Bool) | (Repr::Int, Repr::Int) | (Repr::Duration, Repr::Duration) => {
            a.1 == b.1
        }
        // Through `f64` rather than through the bits, so that a `NaN` is not
        // equal to itself and `0.0` is equal to `-0.0`. IEEE-754 equality is
        // what the language's `==` on a `Float` means, and what the oracle
        // compares.
        (Repr::Float, Repr::Float) => f64::from_bits(a.1) == f64::from_bits(b.1),
        (Repr::Ref, Repr::Ref) => return objects(machine, a.1, b.1, depth),
        _ => false,
    })
}

/// Whether the value a box holds is the operand `other`.
///
/// A box on one side and a bare word on the other. What the box holds has a
/// static width and `other` is one word, so the two can be equal only when
/// the box holds a one-word value — a boxed `Point` is two words and an
/// operand is never that. Equality is symmetric, so which side the box was on
/// is not remembered.
///
/// A struct and an enum are refused even when they are one word wide, because
/// an operand's `Repr` names no declaration: a bare word cannot *be* an
/// `Error`, whatever it is as wide as, so a boxed one is not equal to the
/// `String` its message happens to be. An inline value reaches a comparison
/// through a box or not at all — a `Point` could not be an operand if it
/// wanted to be.
fn held(
    machine: &Machine,
    inside: (LayoutId, Vec<u64>),
    other: Operand,
    depth: usize,
) -> Result<bool, RuntimeError> {
    let described = machine.program().layout(inside.0);
    if matches!(
        described.shape,
        Shape::Struct { .. } | Shape::Enum { .. } | Shape::Free
    ) {
        return Ok(false);
    }
    let ([only], [repr]) = (&inside.1[..], &described.words[..]) else {
        return Ok(false);
    };
    word(machine, (*repr, *only), other, depth)
}

/// Whether the two objects are the same value.
fn objects(machine: &Machine, a: u64, b: u64, depth: usize) -> Result<bool, RuntimeError> {
    if a == 0 || b == 0 {
        return Err(operand::null_value());
    }
    let program = machine.program();
    let (x, y) = (machine.object_layout(a), machine.object_layout(b));
    let (left, right) = (program.layout(x), program.layout(y));
    if matches!(left.shape, Shape::Free) || matches!(right.shape, Shape::Free) {
        return Err(operand::reclaimed());
    }
    let deeper = depth + 1;
    // A sequence is asked about first, because two of the three shapes that
    // are one — an `Array`, a `Vector` and a `Vector`'s store — are the same
    // question with the elements one indirection apart. What they are *not*
    // is interchangeable: an `Array` and a `Vector` are different types and
    // `eq_value` answers `false` for the pair, so the growability is part of
    // what is compared.
    if let (Some(one), Some(other)) = (run(machine, a), run(machine, b)) {
        if one.growable != other.growable || one.len != other.len {
            return Ok(false);
        }
        return runs(
            machine,
            (one.elem, &elements(machine, &one)),
            (other.elem, &elements(machine, &other)),
            one.len,
            deeper,
        );
    }
    Ok(match (&left.shape, &right.shape) {
        (Shape::Str, Shape::Str) => machine.string_bytes(a) == machine.string_bytes(b),
        // An object whose layout is a scalar, a struct or an enum *is* that
        // value: its payload is the value's own inline words, which is what a
        // recursion the lowering had to break looks like from here. So the
        // words are read and the comparison carries on as a comparison of
        // values.
        (
            Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. },
            Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. },
        ) => {
            let (one, other) = (
                machine.payload_run(a, 0, left.width()),
                machine.payload_run(b, 0, right.width()),
            );
            return value(machine, (x, &one), (y, &other), deeper);
        }
        // Two sorted runs line up member for member, which is what the
        // oracle's `BTreeSet` equality does with the same two runs — the
        // order is part of the value, so there is nothing to search.
        (Shape::Members { elem: one }, Shape::Members { elem: other }) => {
            let len = machine.object_len(a);
            if len != machine.object_len(b) {
                return Ok(false);
            }
            let (widths, counterwidths) = (stride(machine, *one), stride(machine, *other));
            return runs(
                machine,
                (*one, &machine.payload_run(a, 0, len * widths)),
                (*other, &machine.payload_run(b, 0, len * counterwidths)),
                len,
                deeper,
            );
        }
        // And two maps line up entry for entry, key before value, because
        // both are in their one true ascending order. An entry is the key's
        // words followed by the value's, so the stride is the two widths
        // together.
        (
            Shape::Entries {
                key: one,
                value: held,
            },
            Shape::Entries {
                key: other,
                value: counterpart,
            },
        ) => {
            let len = machine.object_len(a);
            if len != machine.object_len(b) {
                return Ok(false);
            }
            let (keys, values) = (stride(machine, *one), stride(machine, *held));
            let (counterkeys, countervalues) =
                (stride(machine, *other), stride(machine, *counterpart));
            let (x, y) = (
                machine.payload_run(a, 0, len * (keys + values)),
                machine.payload_run(b, 0, len * (counterkeys + countervalues)),
            );
            for nth in 0..len as usize {
                let (from, counterfrom) = (
                    nth * (keys + values) as usize,
                    nth * (counterkeys + countervalues) as usize,
                );
                let key = (
                    entry(&x, from, keys, &left.name)?,
                    entry(&y, counterfrom, counterkeys, &right.name)?,
                );
                let value_ = (
                    entry(&x, from + keys as usize, values, &left.name)?,
                    entry(
                        &y,
                        counterfrom + counterkeys as usize,
                        countervalues,
                        &right.name,
                    )?,
                );
                if !value(machine, (*one, key.0), (*other, key.1), deeper)?
                    || !value(machine, (*held, value_.0), (*counterpart, value_.1), deeper)?
                {
                    return Ok(false);
                }
            }
            true
        }
        // A closure is not equal to anything, itself included: the language
        // gives `fn` no equality, and the oracle's `eq_value` falls through
        // to `false` for it rather than comparing captures.
        _ => false,
    })
}

/// Whether two runs of `len` elements are equal, element for element.
///
/// The words of each run are read once and sliced at the element's stride,
/// rather than read one element at a time: an element may be several words
/// wide, and reading the run once is both the shorter code and the fewer
/// walks of the memory.
fn runs(
    machine: &Machine,
    x: Held<'_>,
    y: Held<'_>,
    len: u32,
    depth: usize,
) -> Result<bool, RuntimeError> {
    let program = machine.program();
    let (one, other) = (stride(machine, x.0), stride(machine, y.0));
    for nth in 0..len {
        let held = (
            entry(x.1, (nth * one) as usize, one, &program.layout(x.0).name)?,
            entry(
                y.1,
                (nth * other) as usize,
                other,
                &program.layout(y.0).name,
            )?,
        );
        if !value(machine, (x.0, held.0), (y.0, held.1), depth)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The `words` words of `run` at `from`.
fn entry<'w>(
    run: &'w [u64],
    from: usize,
    words: u32,
    name: &str,
) -> Result<&'w [u64], RuntimeError> {
    run.get(from..from + words as usize)
        .ok_or_else(|| super::short_run(name))
}

/// How many words one element of `elem` occupies.
///
/// At least one, so that a run of a zero-width family is walked rather than
/// stepped through forever. Nothing the lowering builds has one, and the
/// floor is here because a loop is a bad place to find that out.
fn stride(machine: &Machine, elem: LayoutId) -> u32 {
    machine.words_of(elem).max(1)
}

/// The elements of a run, as one read.
fn elements(machine: &Machine, of: &Run) -> Vec<u64> {
    machine.payload_run(of.store, 0, of.len * stride(machine, of.elem))
}

/// The elements of `addr`, whether it is an `Array`, a `Vector` or a store.
struct Run {
    elem: LayoutId,
    len: u32,
    store: u64,
    growable: bool,
}

fn run(machine: &Machine, addr: u64) -> Option<Run> {
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements { elem, growable } => Some(Run {
            elem,
            len: machine.object_len(addr),
            store: addr,
            growable,
        }),
        // The length is the vector's own word 0 and not the store's header:
        // a store is as long as the last growth made it, and the elements
        // past the length are spare room rather than value.
        Shape::Vector { elem } => Some(Run {
            elem,
            len: machine.payload(addr, 0) as u32,
            store: machine.payload(addr, 1),
            growable: true,
        }),
        _ => None,
    }
}

/// The value inside a box, if `operand` is one: the layout its payload word 0
/// records, and the words after it read as a value of that layout.
///
/// `pub(super)` because looking through erasure is one rule and not this
/// file's: [`super::key`] orders a `dyn Display` holding `1` exactly where it
/// orders the `Int` `1`, for the reason this compares them equal.
pub(super) fn unboxed(
    machine: &Machine,
    operand: Operand,
) -> Result<Option<(LayoutId, Vec<u64>)>, RuntimeError> {
    if operand.0 != Repr::Ref || operand.1 == 0 {
        return Ok(None);
    }
    let program = machine.program();
    if !matches!(
        program.layout(machine.object_layout(operand.1)).shape,
        Shape::Boxed
    ) {
        return Ok(None);
    }
    let inside = LayoutId(machine.payload(operand.1, 0) as u32);
    let Some(described) = program.layouts.get(inside.index()) else {
        return Err(RuntimeError::new("this boxed value carries no known type"));
    };
    Ok(Some((
        inside,
        machine.payload_run(operand.1, 1, described.width()),
    )))
}

/// An enum value whose discriminant names a case its layout does not have.
fn wrong_case(name: &str) -> RuntimeError {
    RuntimeError::new(format!("this `{name}` is in a case it does not have"))
}

/// A walk that met a graph deeper than it can follow.
///
/// `pub(super)` for [`super::key`], which walks the same graphs to the same
/// depth and stops at it for the same reason.
pub(super) fn too_deep() -> RuntimeError {
    RuntimeError::new("this value nests too deeply to compare")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::make;
    use crate::lvm::builtins::tests::{elements, named, run, scalar, two_case, vector, world};

    fn equal(machine: &mut Machine, a: Operand, b: Operand) -> bool {
        let answer = run(machine, "Any", "equals", &[a, b]).unwrap();
        assert_eq!(answer.len(), 1, "`Any.equals` answers a `Bool`");
        answer[0] != 0
    }

    /// An `Array` of `elem` holding `words`, which is the elements' words
    /// flattened at the element's width — so a two-word element makes an
    /// array half as long as the words it was given.
    fn array(machine: &mut Machine, elem: LayoutId, words: &[u64]) -> u64 {
        let layout = elements(machine.program(), elem, false);
        let len = words.len() as u32 / stride(machine, elem);
        let addr = machine.new_object(layout, len).unwrap();
        machine.set_payload_run(addr, 0, words);
        addr
    }

    /// A box holding `words` as a value of `layout`.
    ///
    /// Payload word 0 is the layout and the words after it are that value
    /// inline, so a boxed `Point` is a two-word payload rather than an
    /// address to somewhere else again.
    fn boxed(machine: &mut Machine, layout: LayoutId, words: &[u64]) -> u64 {
        let held = named(machine.program(), "Boxed");
        let addr = machine.new_object(held, words.len() as u32).unwrap();
        machine.set_payload(addr, 0, layout.0 as u64);
        machine.set_payload_run(addr, 1, words);
        addr
    }

    /// IEEE-754 equality for a `Float`, which is the language's `==`: a `NaN`
    /// is not equal to itself and `0.0` is equal to `-0.0`, neither of which
    /// comparing the bits would answer.
    #[test]
    fn a_float_compares_as_a_float_and_not_as_bits() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let nan = (Repr::Float, f64::NAN.to_bits());
        assert!(!equal(&mut machine, nan, nan));
        assert!(equal(
            &mut machine,
            (Repr::Float, 0.0f64.to_bits()),
            (Repr::Float, (-0.0f64).to_bits())
        ));
        assert!(equal(&mut machine, (Repr::Int, 3), (Repr::Int, 3)));
        assert!(!equal(&mut machine, (Repr::Int, 3), (Repr::Duration, 3)));
    }

    #[test]
    fn a_string_compares_by_its_bytes() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let a = machine.new_string("héllo").unwrap();
        let b = machine.new_string("héllo").unwrap();
        let c = machine.new_string("héllp").unwrap();
        assert_ne!(a, b, "two objects, not one");
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, c)));
    }

    #[test]
    fn a_sequence_compares_element_by_element() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let a = array(&mut machine, int, &[1, 2]);
        let b = array(&mut machine, int, &[1, 2]);
        let short = array(&mut machine, int, &[1]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, short)));

        // Nested, and through references: the elements are compared as what
        // the layout says they are, not as words.
        let text = program.str_layout;
        let one = machine.new_string("x").unwrap();
        let other = machine.new_string("x").unwrap();
        let a = array(&mut machine, text, &[one]);
        let b = array(&mut machine, text, &[other]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
    }

    /// A struct is its fields in place, so two of them are compared field by
    /// field out of the words the values occupy — there is no object to
    /// follow and no header to read.
    #[test]
    fn a_struct_compares_field_by_field() {
        let program = world();
        let machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        assert_eq!(machine.words_of(point), 2, "a `Point` is two words");
        assert!(same_value(&machine, point, &[1, 2], &[1, 2]).unwrap());
        // The first word agreeing is not the value agreeing, which is the
        // whole of what "field-wise" means here.
        assert!(!same_value(&machine, point, &[1, 2], &[1, 3]).unwrap());
        assert!(!same_value(&machine, point, &[1, 2], &[9, 2]).unwrap());
    }

    /// An element's stride is its layout's width, so an `Array<Point>` is a
    /// run of two-word elements. A walk that took the words one at a time
    /// would call the first two of these equal and the third a different
    /// length, and all three answers would be wrong.
    #[test]
    fn an_array_of_structs_walks_at_the_elements_stride() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let a = array(&mut machine, point, &[1, 2, 3, 4]);
        let b = array(&mut machine, point, &[1, 2, 3, 4]);
        let other = array(&mut machine, point, &[1, 2, 9, 9]);
        let short = array(&mut machine, point, &[1, 2]);
        assert_eq!(machine.object_len(a), 2, "two elements, four words");
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, other)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, short)));
    }

    /// A `Vector` compares its current elements structurally, exactly as an
    /// `Array` does — storage identity is the separate question `is` answers.
    /// The two are still different types, so the pair is never equal.
    #[test]
    fn a_vector_compares_by_value_but_is_not_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let a = make::vector_of(&mut machine, int, &[1, 2]).unwrap();
        let b = make::vector_of(&mut machine, int, &[1, 2]).unwrap();
        assert_ne!(a, b);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));

        // A store is as long as the last growth made it, and the elements
        // past the vector's own length are spare room rather than value. The
        // roomy store is built here rather than pushed into, so that what is
        // under test is the comparison.
        let roomy = machine
            .new_object(elements(machine.program(), int, true), 4)
            .unwrap();
        machine.set_payload_run(roomy, 0, &[1, 2, 9, 9]);
        machine.set_payload(a, 1, roomy);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));

        let items = array(&mut machine, int, &[1, 2]);
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, items)));
        assert_eq!(machine.object_layout(a), vector(machine.program(), int));
    }

    /// A different family answers `false` rather than raising: the checker's
    /// `==` is what refuses a comparison between two types, and inside a
    /// comparison a mismatch is an answer.
    #[test]
    fn two_values_of_different_families_are_not_equal() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let items = array(&mut machine, int, &[1, 2]);
        let text = machine.new_string("x").unwrap();
        assert!(!equal(&mut machine, (Repr::Ref, items), (Repr::Ref, text)));

        // An `Error` is one word — the address of its message — and is still
        // not a `String`, because a struct is compared as a struct whatever
        // it is as wide as.
        let error = named(&program, "Error");
        let boxed_error = boxed(&mut machine, error, &[text]);
        assert!(!equal(
            &mut machine,
            (Repr::Ref, boxed_error),
            (Repr::Ref, text)
        ));
    }

    /// The case is compared by *name*, so two instantiations of one declared
    /// enum agree about which case is which without agreeing about indices.
    #[test]
    fn an_enum_compares_by_case_name_and_payload() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let option = two_case(&program, "Option", "Some", int);

        let some = make::some(&mut machine, int, &[1]).unwrap();
        let alike = make::some(&mut machine, int, &[1]).unwrap();
        let other = make::some(&mut machine, int, &[2]).unwrap();
        let none = make::none(&mut machine, int).unwrap();
        assert!(same_value(&machine, option, &some, &alike).unwrap());
        assert!(!same_value(&machine, option, &some, &other).unwrap());
        assert!(!same_value(&machine, option, &some, &none).unwrap());

        // `None` is `None` whichever `Option` it came out of, because a case
        // with no payload has nothing to disagree about. Two boxes are where
        // two layouts meet, since a box records the family of what it holds.
        let text = program.str_layout;
        let empty = make::none(&mut machine, text).unwrap();
        let one = boxed(&mut machine, option, &none);
        let another = boxed(
            &mut machine,
            two_case(&program, "Option", "Some", text),
            &empty,
        );
        assert!(equal(&mut machine, (Repr::Ref, one), (Repr::Ref, another)));
    }

    /// Two sorted runs are equal when they line up, which is what makes a
    /// set's equality a walk rather than a search: both are already in the one
    /// ascending order the language gives them.
    #[test]
    fn a_set_and_a_map_compare_run_for_run() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);

        let members = |machine: &mut Machine, words: &[u64]| {
            let layout = make::members(machine.program(), int).unwrap();
            let addr = machine.new_object(layout, words.len() as u32).unwrap();
            machine.set_payload_run(addr, 0, words);
            addr
        };
        let a = members(&mut machine, &[1, 2]);
        let b = members(&mut machine, &[1, 2]);
        let short = members(&mut machine, &[1]);
        let other = members(&mut machine, &[1, 3]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, short)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, other)));
        // A different family answers `false` rather than raising, as
        // everywhere else here.
        let items = array(&mut machine, int, &[1, 2]);
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, items)));

        let entries = |machine: &mut Machine, words: &[u64]| {
            let layout = make::entries(machine.program(), int, int).unwrap();
            let addr = machine.new_object(layout, words.len() as u32 / 2).unwrap();
            machine.set_payload_run(addr, 0, words);
            addr
        };
        let a = entries(&mut machine, &[1, 10, 2, 20]);
        let b = entries(&mut machine, &[1, 10, 2, 20]);
        let valued = entries(&mut machine, &[1, 10, 2, 21]);
        let keyed = entries(&mut machine, &[1, 10, 3, 20]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, valued)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, keyed)));
    }

    /// Erasure is looked through on either side, so where the checker put a
    /// `dyn` wrapper is not something a comparison can tell.
    #[test]
    fn a_box_compares_as_what_it_holds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let one = boxed(&mut machine, int, &[7]);
        assert!(equal(&mut machine, (Repr::Ref, one), (Repr::Int, 7)));
        assert!(!equal(&mut machine, (Repr::Ref, one), (Repr::Int, 8)));

        let text = machine.new_string("x").unwrap();
        let other = machine.new_string("x").unwrap();
        let held = boxed(&mut machine, program.str_layout, &[text]);
        assert!(equal(&mut machine, (Repr::Ref, held), (Repr::Ref, other)));

        // A box holds a multiword value inline, so two boxed `Point`s are
        // compared as `Point`s — and a boxed `Point` is not an `Int`,
        // whatever its first word holds.
        let point = named(&program, "Point");
        let a = boxed(&mut machine, point, &[1, 2]);
        let b = boxed(&mut machine, point, &[1, 2]);
        let elsewhere = boxed(&mut machine, point, &[1, 3]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, elsewhere)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Int, 1)));
    }

    /// The three things only this representation can go wrong at.
    #[test]
    fn a_null_reclaimed_or_untyped_reference_is_refused() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let text = machine.new_string("x").unwrap();
        let error = run(
            &mut machine,
            "Any",
            "equals",
            &[(Repr::Ref, text), (Repr::Ref, 0)],
        )
        .unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");

        let dead = machine
            .new_object(elements(machine.program(), int, false), 0)
            .unwrap();
        machine.relabel(dead, LayoutId::FREE, 0, 0);
        let error = run(
            &mut machine,
            "Any",
            "equals",
            &[(Repr::Ref, text), (Repr::Ref, dead)],
        )
        .unwrap_err();
        assert_eq!(error.message, "this value was read after it was reclaimed");

        // A box names the family of what it holds, and one that names a
        // family the program does not have cannot be looked through.
        let stray = boxed(&mut machine, int, &[1]);
        machine.set_payload(stray, 0, program.layouts.len() as u64);
        let error = run(
            &mut machine,
            "Any",
            "equals",
            &[(Repr::Ref, stray), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(error.message, "this boxed value carries no known type");
    }

    /// An object that holds itself is a legal heap graph and not a legal
    /// `Value`, so the walk stops rather than running out of native stack.
    #[test]
    fn a_cycle_stops_rather_than_recursing_forever() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let text = program.str_layout;
        let a = array(&mut machine, text, &[0]);
        let b = array(&mut machine, text, &[0]);
        machine.set_payload(a, 0, a);
        machine.set_payload(b, 0, b);
        let error = run(
            &mut machine,
            "Any",
            "equals",
            &[(Repr::Ref, a), (Repr::Ref, b)],
        )
        .unwrap_err();
        assert_eq!(error.message, "this value nests too deeply to compare");
    }

    #[test]
    fn equals_takes_two_operands() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let error = run(&mut machine, "Any", "equals", &[(Repr::Int, 1)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Any.equals` takes 2 operand(s), but 1 were given"
        );
        // And nothing else on `Any` is an operation this backend has.
        let error = run(&mut machine, "Any", "compare", &[]).unwrap_err();
        assert_eq!(
            error.message,
            "`Any.compare` is not an operation this backend has been taught"
        );
    }
}
