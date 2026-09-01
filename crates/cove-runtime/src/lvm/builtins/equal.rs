//! Value equality, over words and heap objects.
//!
//! `==` on anything that is not one word of scalar bits lowers to a call
//! here. [`crate::value::Value::eq_value`] is the oracle's copy of the same
//! rule, and this is written twice for the reason the rendering beside it is:
//! that one walks a materialised tree and this one walks the heap, and
//! neither can be had from the other without building what the other exists
//! to avoid.
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
//! reference, an object the sweeper reclaimed, and a graph that nests deeper
//! than a walk of it can.

use cove_lir::{Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::builtins::operand::{self, Operand};
use crate::lvm::exec::Machine;

/// `a == b`, as the `Bool` word `0` or `1`.
pub(super) fn equals(machine: &Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let [a, b] = operands else {
        return Err(operand::operands("Any.equals", 2, operands.len()));
    };
    Ok(same(machine, *a, *b, 0)? as u64)
}

/// Whether the two operands are the same value.
pub(super) fn same(
    machine: &Machine,
    a: Operand,
    b: Operand,
    depth: usize,
) -> Result<bool, RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(too_deep());
    }
    // Erasure is looked through on either side before anything is compared,
    // which is `eq_value`'s first move as well: a `dyn Display` holding `1`
    // and an `Int` `1` are one value, and where the checker put the wrapper
    // is not something a program can ask about.
    if let Some(inner) = unboxed(machine, a) {
        return same(machine, inner, b, depth + 1);
    }
    if let Some(inner) = unboxed(machine, b) {
        return same(machine, a, inner, depth + 1);
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

/// Whether the two objects are the same value.
fn objects(machine: &Machine, a: u64, b: u64, depth: usize) -> Result<bool, RuntimeError> {
    if a == 0 || b == 0 {
        return Err(operand::null_value());
    }
    let program = machine.program();
    let (left, right) = (
        program.layout(machine.object_layout(a)),
        program.layout(machine.object_layout(b)),
    );
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
    if let (Some(x), Some(y)) = (run(machine, a), run(machine, b)) {
        if x.growable != y.growable || x.len != y.len {
            return Ok(false);
        }
        for at in 0..x.len {
            let one = (x.elem, machine.payload(x.store, at));
            let other = (y.elem, machine.payload(y.store, at));
            if !same(machine, one, other, deeper)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    Ok(match (&left.shape, &right.shape) {
        (Shape::Str, Shape::Str) => machine.string_bytes(a) == machine.string_bytes(b),
        // Fields are compared by position and not by name, which is the
        // oracle's own reading: two objects of one declared type have one
        // field order, and the names would be the same comparison done twice.
        (Shape::Struct { fields: x, .. }, Shape::Struct { fields: y, .. }) => {
            if left.name != right.name || x.len() != y.len() {
                return Ok(false);
            }
            for (at, (one, other)) in x.iter().zip(y).enumerate() {
                let at = at as u32;
                let pair = (
                    (one.repr, machine.payload(a, at)),
                    (other.repr, machine.payload(b, at)),
                );
                if !same(machine, pair.0, pair.1, deeper)? {
                    return Ok(false);
                }
            }
            true
        }
        // The case is compared by *name*, as the oracle compares it, because
        // two `Option` layouts are two instantiations of one declared enum
        // and an index means the same thing in both only by construction. A
        // name means it by declaration.
        (Shape::Enum { cases: x }, Shape::Enum { cases: y }) => {
            if left.name != right.name {
                return Ok(false);
            }
            let (one, other) = (machine.payload(a, 0), machine.payload(b, 0));
            let (Some(one), Some(other)) = (x.get(one as usize), y.get(other as usize)) else {
                return Err(RuntimeError::new(format!(
                    "this `{}` is in a case it does not have",
                    left.name
                )));
            };
            if one.name != other.name || one.payload.len() != other.payload.len() {
                return Ok(false);
            }
            for (at, (left, right)) in one.payload.iter().zip(&other.payload).enumerate() {
                let at = 1 + at as u32;
                let pair = (
                    (*left, machine.payload(a, at)),
                    (*right, machine.payload(b, at)),
                );
                if !same(machine, pair.0, pair.1, deeper)? {
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

/// The elements of `addr`, whether it is an `Array`, a `Vector` or a store.
struct Run {
    elem: Repr,
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

/// The `Repr` and the word inside a box, if `operand` is one.
fn unboxed(machine: &Machine, operand: Operand) -> Option<Operand> {
    if operand.0 != Repr::Ref || operand.1 == 0 {
        return None;
    }
    if !matches!(
        machine
            .program()
            .layout(machine.object_layout(operand.1))
            .shape,
        Shape::Boxed
    ) {
        return None;
    }
    let repr = Repr::from_tag(machine.payload(operand.1, 0))?;
    Some((repr, machine.payload(operand.1, 1)))
}

fn too_deep() -> RuntimeError {
    RuntimeError::new("this value nests too deeply to compare")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::make;
    use crate::lvm::builtins::tests::{elements, named, run, vector, world};
    use cove_lir::LayoutId;

    fn equal(machine: &mut Machine, a: Operand, b: Operand) -> bool {
        run(machine, "Any", "equals", &[a, b]).unwrap() != 0
    }

    fn array(machine: &mut Machine, elem: Repr, words: &[u64]) -> u64 {
        let layout = elements(machine.program(), elem, false);
        let addr = machine.new_object(layout, words.len() as u32).unwrap();
        for (at, word) in words.iter().enumerate() {
            machine.set_payload(addr, at as u32, *word);
        }
        addr
    }

    fn point(machine: &mut Machine, x: i64, y: i64) -> u64 {
        let addr = machine
            .new_object(named(machine.program(), "Point"), 0)
            .unwrap();
        machine.set_payload(addr, 0, x as u64);
        machine.set_payload(addr, 1, y as u64);
        addr
    }

    fn boxed(machine: &mut Machine, repr: Repr, word: u64) -> u64 {
        let addr = machine
            .new_object(named(machine.program(), "Boxed"), 0)
            .unwrap();
        machine.set_payload(addr, 0, repr.tag());
        machine.set_payload(addr, 1, word);
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
        let a = array(&mut machine, Repr::Int, &[1, 2]);
        let b = array(&mut machine, Repr::Int, &[1, 2]);
        let short = array(&mut machine, Repr::Int, &[1]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, short)));

        // Nested, and through references: the elements are compared as what
        // the layout says they are, not as words.
        let one = machine.new_string("x").unwrap();
        let other = machine.new_string("x").unwrap();
        let a = array(&mut machine, Repr::Ref, &[one]);
        let b = array(&mut machine, Repr::Ref, &[other]);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
    }

    /// A `Vector` compares its current elements structurally, exactly as an
    /// `Array` does — storage identity is the separate question `is` answers.
    /// The two are still different types, so the pair is never equal.
    #[test]
    fn a_vector_compares_by_value_but_is_not_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let a = make::vector_of(&mut machine, Repr::Int, &[1, 2]).unwrap();
        let b = make::vector_of(&mut machine, Repr::Int, &[1, 2]).unwrap();
        assert_ne!(a, b);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));

        // The store is longer than the vector after a push, and the spare
        // room is not part of the value.
        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, a), (Repr::Int, 3)],
        )
        .unwrap();
        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, b), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(machine.object_len(machine.payload(a, 1)), 4);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));

        let items = array(&mut machine, Repr::Int, &[1, 2, 3]);
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, items)));
        assert_eq!(
            machine.object_layout(a),
            vector(machine.program(), Repr::Int)
        );
    }

    #[test]
    fn a_struct_compares_by_name_and_field() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let a = point(&mut machine, 1, 2);
        let b = point(&mut machine, 1, 2);
        let c = point(&mut machine, 1, 3);
        assert!(equal(&mut machine, (Repr::Ref, a), (Repr::Ref, b)));
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, c)));
        // A different family answers `false` rather than raising: the
        // checker's `==` is what refuses a comparison between two types, and
        // inside a comparison a mismatch is an answer.
        let text = machine.new_string("x").unwrap();
        assert!(!equal(&mut machine, (Repr::Ref, a), (Repr::Ref, text)));
    }

    /// The case is compared by *name*, so two instantiations of one declared
    /// enum agree about which case is which without agreeing about indices.
    #[test]
    fn an_enum_compares_by_case_name_and_payload() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let some = make::some(&mut machine, Repr::Int, 1).unwrap();
        let same = make::some(&mut machine, Repr::Int, 1).unwrap();
        let other = make::some(&mut machine, Repr::Int, 2).unwrap();
        let none = make::none(&mut machine, Repr::Int).unwrap();
        assert!(equal(&mut machine, (Repr::Ref, some), (Repr::Ref, same)));
        assert!(!equal(&mut machine, (Repr::Ref, some), (Repr::Ref, other)));
        assert!(!equal(&mut machine, (Repr::Ref, some), (Repr::Ref, none)));

        // `None` is `None` whichever `Option` it came out of, because a case
        // with no payload has nothing to disagree about.
        let empty = make::none(&mut machine, Repr::Ref).unwrap();
        assert_ne!(machine.object_layout(none), machine.object_layout(empty));
        assert!(equal(&mut machine, (Repr::Ref, none), (Repr::Ref, empty)));
    }

    /// Erasure is looked through on either side, so where the checker put a
    /// `dyn` wrapper is not something a comparison can tell.
    #[test]
    fn a_box_compares_as_what_it_holds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let one = boxed(&mut machine, Repr::Int, 7);
        assert!(equal(&mut machine, (Repr::Ref, one), (Repr::Int, 7)));
        assert!(!equal(&mut machine, (Repr::Ref, one), (Repr::Int, 8)));

        let text = machine.new_string("x").unwrap();
        let other = machine.new_string("x").unwrap();
        let held = boxed(&mut machine, Repr::Ref, text);
        assert!(equal(&mut machine, (Repr::Ref, held), (Repr::Ref, other)));
    }

    /// The two things only this representation can go wrong at.
    #[test]
    fn a_null_or_reclaimed_reference_is_refused() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
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
            .new_object(elements(machine.program(), Repr::Int, false), 0)
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
    }

    /// An object that holds itself is a legal heap graph and not a legal
    /// `Value`, so the walk stops rather than running out of native stack.
    #[test]
    fn a_cycle_stops_rather_than_recursing_forever() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let a = array(&mut machine, Repr::Ref, &[0]);
        let b = array(&mut machine, Repr::Ref, &[0]);
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
