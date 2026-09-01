//! `Array` and `Vector`.
//!
//! The two are one shape apart. An `Array` holds its elements in the object,
//! one indirection nearer than a `Vector`, and cannot grow. A `Vector` is a
//! two-word header — `[len, store]` — over a growable store, and it pays that
//! indirection for the one thing an `Array` does not need: its identity is
//! observable, so growing must not move the object a program is holding.
//!
//! # The receiver is the vector, not a place that holds one
//!
//! `push`, `set`, `pop`, `remove` and `freeze` declare `var self` in the
//! schema, and a `var` parameter is ordinarily a [`Repr::Addr`]. None of them
//! is passed one here: the lowering hands over the vector itself, as a
//! [`Repr::Ref`]. That is not a shortcut, it is what the language says a
//! `Vector` is — a copy of one is an alias, mutation through one copy is
//! visible through every other, and every one of them names the same two
//! words. Writing through the header is therefore already visible everywhere
//! the value went, and there is nothing to write back to the receiver's own
//! slot. `freeze()` is the strongest case: it has to be observable through
//! every alias, and clearing the header's two words is exactly how.
//!
//! # Growth
//!
//! A store is allocated to exactly the elements it is built from, because
//! `Vector.of(1, 2, 3)` and `Array.toVector()` know the count and spare room
//! nobody asked for is room a program pays for. A `push` onto a full store
//! allocates one of **twice the capacity, from a floor of four**, copies the
//! elements across, and writes the new store into word 1 — the header does
//! not move, so no reference to it anywhere goes stale. Appending is
//! therefore amortised O(1).
//!
//! A store never shrinks. `pop` and `remove` leave the room they vacate, so
//! that a program that fills and empties one does not reallocate on every
//! turn — but they **zero the word they vacate**, because a store's shape
//! says its whole capacity is elements and the collector reads it that way. A
//! dead element left in the spare room would be a root, and a vector used as
//! a work queue would retain everything it had ever held.

use cove_lir::{Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::builtins::operand::Operand;
use crate::lvm::builtins::{equal, make, operand};
use crate::lvm::exec::Machine;

/// The smallest store a `push` onto a full one asks for.
const MIN_CAPACITY: u32 = 4;

// --- reading a receiver ----------------------------------------------------

/// The elements of an `Array`.
struct Fixed {
    elem: Repr,
    len: u32,
    addr: u64,
}

fn array(machine: &Machine, method: &str, receiver: Operand) -> Result<Fixed, RuntimeError> {
    let (repr, addr) = receiver;
    if repr != Repr::Ref {
        return Err(operand::no_method(machine, receiver, method));
    }
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements {
            elem,
            growable: false,
        } => Ok(Fixed {
            elem,
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// A live `Vector`: its header, its store, and how much of the store is
/// value rather than spare room.
struct Growable {
    header: u64,
    elem: Repr,
    len: u32,
    capacity: u32,
    store: u64,
}

/// Reads the receiver of a `Vector` method, refusing one `freeze()` consumed.
///
/// The liveness check happens here rather than in each operation because the
/// oracle asks it once, at the top of its `Vector` arm, before it looks at
/// the method name at all — so a frozen vector answers the same thing to
/// `length()` as to `push()`, and the message names whichever was called.
fn vector(machine: &Machine, method: &str, receiver: Operand) -> Result<Growable, RuntimeError> {
    let (repr, addr) = receiver;
    if repr != Repr::Ref {
        return Err(operand::no_method(machine, receiver, method));
    }
    if addr == 0 {
        return Err(operand::null_value());
    }
    let Shape::Vector { elem } = machine.program().layout(machine.object_layout(addr)).shape else {
        return Err(operand::no_method(machine, receiver, method));
    };
    let store = machine.payload(addr, 1);
    if store == 0 {
        return Err(operand::frozen(method));
    }
    Ok(Growable {
        header: addr,
        elem,
        len: machine.payload(addr, 0) as u32,
        capacity: machine.object_len(store),
        store,
    })
}

/// A non-negative `Int` names a position, a negative one names none.
///
/// The oracle's `index_of`, and the reason `get`, `set` and `remove` answer
/// `None` for `-1` rather than stopping the run: a program has one rule about
/// indices, and an index outside the collection is not one of them.
fn index(
    machine: &Machine,
    method: &str,
    argument: Operand,
) -> Result<Option<usize>, RuntimeError> {
    let at = operand::int(machine, method, "index", argument)?;
    Ok((at >= 0).then_some(at as usize))
}

/// The elements of `store[from..to]`, with both bounds clamped into the
/// sequence and a `to` at or below `from` answering nothing.
fn sliced(
    machine: &Machine,
    shown: &str,
    store: u64,
    len: u32,
    args: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    if args.len() != 2 {
        return Err(operand::arity(shown, 2, args.len()));
    }
    let bound = |at: usize, parameter: &str| {
        operand::int(machine, shown, parameter, args[at]).map(|i| i.clamp(0, len as i64) as u32)
    };
    let from = bound(0, "from")?;
    let to = bound(1, "to")?;
    if to <= from {
        return Ok(Vec::new());
    }
    Ok((from..to).map(|at| machine.payload(store, at)).collect())
}

/// The position of the first element equal to `wanted`, if there is one.
fn position(
    machine: &Machine,
    elem: Repr,
    store: u64,
    len: u32,
    wanted: Operand,
) -> Result<Option<u32>, RuntimeError> {
    for at in 0..len {
        if equal::same(machine, (elem, machine.payload(store, at)), wanted, 0)? {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

// --- Array -----------------------------------------------------------------

/// `Array.get(index) -> Option<T>`.
pub(super) fn array_get(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.get", operands, 1)?;
    let items = array(machine, "get", receiver)?;
    match index(machine, "Array.get", args[0])? {
        Some(at) if at < items.len as usize => {
            let word = machine.payload(items.addr, at as u32);
            make::some(machine, items.elem, word)
        }
        _ => make::none(machine, items.elem),
    }
}

/// `Array.length() -> Int`.
pub(super) fn array_length(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(array(machine, "length", receiver)?.len as u64)
}

/// `Array.isEmpty() -> Bool`.
pub(super) fn array_is_empty(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((array(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Array.contains(element) -> Bool`.
pub(super) fn array_contains(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.contains", operands, 1)?;
    let items = array(machine, "contains", receiver)?;
    let at = position(machine, items.elem, items.addr, items.len, args[0])?;
    Ok(at.is_some() as u64)
}

/// `Array.indexOf(element) -> Option<Int>`.
pub(super) fn array_index_of(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.indexOf", operands, 1)?;
    let items = array(machine, "indexOf", receiver)?;
    match position(machine, items.elem, items.addr, items.len, args[0])? {
        Some(at) => make::some(machine, Repr::Int, at as u64),
        None => make::none(machine, Repr::Int),
    }
}

/// `Array.slice(from, to) -> Array<T>`.
pub(super) fn array_slice(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.slice", operands, 2)?;
    let items = array(machine, "slice", receiver)?;
    let words = sliced(machine, "Array.slice", items.addr, items.len, args)?;
    make::array_of(machine, items.elem, &words)
}

/// `Array.toVector() -> Vector<T>`.
///
/// `Vector.toArray()` run backwards: a growable copy of these elements that
/// nothing else holds a handle to. The elements are copied as they are rather
/// than snapshotted, which is `toArray`'s own rule read the other way — this
/// separates the sequence and nothing inside it.
pub(super) fn array_to_vector(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toVector", operands, 0)?;
    let items = array(machine, "toVector", receiver)?;
    let words: Vec<u64> = (0..items.len)
        .map(|at| machine.payload(items.addr, at))
        .collect();
    make::vector_of(machine, items.elem, &words)
}

// --- Vector ----------------------------------------------------------------

/// `Vector.of(items...) -> Vector<T>`.
///
/// The element family comes from the first operand's `Repr`, which is a
/// static fact about the slot it came out of. `Vector.of()` with no operands
/// therefore says nothing about what it is a vector of, and this refuses it
/// rather than guessing: a store's `Repr` is what the collector traces its
/// words by, so a `Vector<Int>` store that a later `push` put a reference in
/// would drop the object on the next collection. The lowering knows the
/// layout the checker resolved and can allocate an empty vector itself, which
/// is the one place the answer exists.
pub(super) fn vector_of(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let Some((first, _)) = operands.first() else {
        return Err(RuntimeError::new(
            "`Vector.of()` with no elements does not say what it is a vector of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty vector where the element type is known"));
    };
    let elem = *first;
    let words: Vec<u64> = operands.iter().map(|(_, word)| *word).collect();
    make::vector_of(machine, elem, &words)
}

/// `Vector.push(value)`.
pub(super) fn vector_push(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("push", operands, 1)?;
    let items = vector(machine, "push", receiver)?;
    let store = if items.len < items.capacity {
        items.store
    } else {
        grow(machine, &items)?
    };
    machine.set_payload(store, items.len, args[0].1);
    machine.set_payload(items.header, 0, items.len as u64 + 1);
    Ok(0)
}

/// A larger store for a vector whose own is full, written into word 1.
///
/// The header does not move. That is the whole reason a `Vector` has a store
/// at all: `is` is defined for it, and mutation through one copy is visible
/// through every other, so the object a program is holding has to stay where
/// it is while what is under it is replaced.
///
/// The old store is reachable from the header, which is an operand and
/// therefore a slot of the frame that called this builtin, so the allocation
/// below cannot free it. The new one is unrooted for exactly the copy, which
/// allocates nothing.
fn grow(machine: &mut Machine, items: &Growable) -> Result<u64, RuntimeError> {
    let layout = make::elements(machine.program(), items.elem, true)?;
    let capacity = items.capacity.saturating_mul(2).max(MIN_CAPACITY);
    let store = machine.new_object(layout, capacity)?;
    for at in 0..items.len {
        let word = machine.payload(items.store, at);
        machine.set_payload(store, at, word);
    }
    machine.set_payload(items.header, 1, store);
    Ok(store)
}

/// `Vector.set(index, value) -> Option<T>`.
///
/// Answers what was there, or answers `None` and writes nothing when `index`
/// is not already in the vector — which is `get`'s answer to the same bad
/// index.
pub(super) fn vector_set(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.set", operands, 2)?;
    let items = vector(machine, "set", receiver)?;
    let Some(at) = index(machine, "Vector.set", args[0])? else {
        return make::none(machine, items.elem);
    };
    if at >= items.len as usize {
        return make::none(machine, items.elem);
    }
    let was = machine.payload(items.store, at as u32);
    machine.set_payload(items.store, at as u32, args[1].1);
    // `was` is now named by nothing the collector walks — the store's word is
    // the value that replaced it — and `some` allocates. It is rooted there.
    make::some(machine, items.elem, was)
}

/// `Vector.pop() -> Option<T>`.
///
/// The empty case is `remove(length() - 1)` on an empty vector, where that
/// index is `-1`, which `get`, `set` and `remove` all answer `None` for. One
/// rule about indices rather than a rule about indices and a rule about
/// emptiness.
pub(super) fn vector_pop(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("Vector.pop", operands, 0)?;
    let items = vector(machine, "pop", receiver)?;
    if items.len == 0 {
        return make::none(machine, items.elem);
    }
    let at = items.len - 1;
    let was = machine.payload(items.store, at);
    machine.set_payload(items.store, at, 0);
    machine.set_payload(items.header, 0, at as u64);
    make::some(machine, items.elem, was)
}

/// `Vector.remove(index) -> Option<T>`.
pub(super) fn vector_remove(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.remove", operands, 1)?;
    let items = vector(machine, "remove", receiver)?;
    let Some(at) = index(machine, "Vector.remove", args[0])? else {
        return make::none(machine, items.elem);
    };
    if at >= items.len as usize {
        return make::none(machine, items.elem);
    }
    let at = at as u32;
    let was = machine.payload(items.store, at);
    for from in at + 1..items.len {
        let word = machine.payload(items.store, from);
        machine.set_payload(items.store, from - 1, word);
    }
    machine.set_payload(items.store, items.len - 1, 0);
    machine.set_payload(items.header, 0, items.len as u64 - 1);
    make::some(machine, items.elem, was)
}

/// `Vector.get(index) -> Option<T>`.
pub(super) fn vector_get(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.get", operands, 1)?;
    let items = vector(machine, "get", receiver)?;
    match index(machine, "Vector.get", args[0])? {
        Some(at) if at < items.len as usize => {
            let word = machine.payload(items.store, at as u32);
            make::some(machine, items.elem, word)
        }
        _ => make::none(machine, items.elem),
    }
}

/// `Vector.contains(element) -> Bool`.
pub(super) fn vector_contains(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.contains", operands, 1)?;
    let items = vector(machine, "contains", receiver)?;
    let at = position(machine, items.elem, items.store, items.len, args[0])?;
    Ok(at.is_some() as u64)
}

/// `Vector.indexOf(element) -> Option<Int>`.
pub(super) fn vector_index_of(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.indexOf", operands, 1)?;
    let items = vector(machine, "indexOf", receiver)?;
    match position(machine, items.elem, items.store, items.len, args[0])? {
        Some(at) => make::some(machine, Repr::Int, at as u64),
        None => make::none(machine, Repr::Int),
    }
}

/// `Vector.slice(from, to) -> Array<T>`.
///
/// An **`Array`**, not a `Vector`: the oracle answers one, and it is the
/// right answer — a slice is a reading of a sequence and nothing about it
/// asks to be grown.
pub(super) fn vector_slice(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.slice", operands, 2)?;
    let items = vector(machine, "slice", receiver)?;
    let words = sliced(machine, "Vector.slice", items.store, items.len, args)?;
    make::array_of(machine, items.elem, &words)
}

/// `Vector.length() -> Int`.
pub(super) fn vector_length(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(vector(machine, "length", receiver)?.len as u64)
}

/// `Vector.isEmpty() -> Bool`.
pub(super) fn vector_is_empty(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((vector(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Vector.toArray() -> Array<T>`, copying the elements.
pub(super) fn vector_to_array(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toArray", operands, 0)?;
    let items = vector(machine, "toArray", receiver)?;
    let words: Vec<u64> = (0..items.len)
        .map(|at| machine.payload(items.store, at))
        .collect();
    make::array_of(machine, items.elem, &words)
}

/// `Vector.freeze() -> Array<T>`, in O(1).
///
/// The store *becomes* the array. Nothing is copied: the header word is
/// rewritten to name the `Array` family and the vector's length, and the
/// spare room goes back to the heap as a free block, which is what keeps the
/// heap a walkable sequence of objects. Then the vector's own two words are
/// cleared, so that every alias of it — and a copy of a `Vector` *is* an
/// alias — finds a vector `freeze()` has consumed. That is what the oracle's
/// flag on the shared storage says to every alias of it, said in the two
/// words this representation shares instead.
///
/// # The one thing this cannot ask
///
/// The oracle refuses `freeze()` when another alias observes the vector,
/// because it can count the handles to an `Rc`. A handle here is a word, and
/// words are not counted: this backend cannot tell one alias from three. It
/// therefore *consumes* where the oracle would have *refused*, which is the
/// more permissive of the two and not the unsound one — every alias is left
/// holding a consumed vector and is told so the moment it is used, which is
/// exactly what the oracle leaves behind after a `freeze()` it did allow. A
/// program that the oracle refuses and this admits gets an `Array` where it
/// expected a diagnostic; nothing observes a half-frozen vector either way.
pub(super) fn vector_freeze(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("freeze", operands, 0)?;
    let items = vector(machine, "freeze", receiver)?;
    let layout = make::elements(machine.program(), items.elem, false)?;
    machine.relabel(items.store, layout, items.len, items.capacity - items.len);
    machine.set_payload(items.header, 0, 0);
    machine.set_payload(items.header, 1, 0);
    Ok(items.store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::tests::{case_of, elements, read, run, vector, words_of, world};
    use cove_lir::LayoutId;

    /// An `Array<Int>` holding `values`.
    fn array_of(machine: &mut Machine, values: &[i64]) -> u64 {
        let layout = elements(machine.program(), Repr::Int, false);
        let addr = machine
            .new_object(layout, values.len() as u32)
            .expect("the fixture's heap is large enough");
        for (at, value) in values.iter().enumerate() {
            machine.set_payload(addr, at as u32, *value as u64);
        }
        addr
    }

    /// A `Vector<Int>` holding `values`, with a store of exactly that many.
    fn growable(machine: &mut Machine, values: &[i64]) -> u64 {
        let words: Vec<u64> = values.iter().map(|value| *value as u64).collect();
        make::vector_of(machine, Repr::Int, &words).expect("the fixture declares every family")
    }

    fn int(word: u64) -> i64 {
        word as i64
    }

    #[test]
    fn an_array_reports_what_it_holds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 30]);
        let empty = array_of(&mut machine, &[]);

        assert_eq!(
            run(&mut machine, "Array", "length", &[(Repr::Ref, items)]).unwrap(),
            3
        );
        assert_eq!(
            run(&mut machine, "Array", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );
        assert_eq!(
            run(&mut machine, "Array", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
            1
        );
    }

    /// `get` answers `None` for every index that is not already there, which
    /// is one rule rather than one for a negative index and one for a large
    /// one.
    #[test]
    fn an_array_get_answers_none_outside_itself() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 30]);
        let get = |machine: &mut Machine, at: i64| {
            let word = run(
                machine,
                "Array",
                "get",
                &[(Repr::Ref, items), (Repr::Int, at as u64)],
            )
            .unwrap();
            case_of(machine, word)
        };
        assert_eq!(get(&mut machine, 1), ("Some".to_string(), vec![20]));
        assert_eq!(get(&mut machine, -1).0, "None");
        assert_eq!(get(&mut machine, 3).0, "None");
    }

    #[test]
    fn an_array_finds_an_element_by_value() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 20]);

        for (wanted, found) in [(20i64, 1u64), (99, 0)] {
            assert_eq!(
                run(
                    &mut machine,
                    "Array",
                    "contains",
                    &[(Repr::Ref, items), (Repr::Int, wanted as u64)]
                )
                .unwrap(),
                found
            );
        }
        // The *first* position, so a repeated element answers the earlier one.
        let word = run(
            &mut machine,
            "Array",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![1]));
        let word = run(
            &mut machine,
            "Array",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 99)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word).0, "None");
    }

    /// Both bounds are clamped and a `to` at or below `from` answers nothing,
    /// so no bound can stop the run.
    #[test]
    fn an_array_slice_clamps_both_bounds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 30]);
        let slice = |machine: &mut Machine, from: i64, to: i64| {
            let word = run(
                machine,
                "Array",
                "slice",
                &[
                    (Repr::Ref, items),
                    (Repr::Int, from as u64),
                    (Repr::Int, to as u64),
                ],
            )
            .unwrap();
            words_of(machine, word)
        };
        assert_eq!(slice(&mut machine, 1, 3), vec![20, 30]);
        assert_eq!(slice(&mut machine, -5, 99), vec![10, 20, 30]);
        assert_eq!(slice(&mut machine, 2, 1), Vec::<u64>::new());
    }

    #[test]
    fn an_array_becomes_a_vector_and_a_vector_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20]);

        let vector = run(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(machine.payload(vector, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(vector, 1)), vec![10, 20]);

        let back = run(&mut machine, "Vector", "toArray", &[(Repr::Ref, vector)]).unwrap();
        assert_eq!(words_of(&machine, back), vec![10, 20]);
        // A copy, not the store: `toArray` is the O(n) conversion, and the
        // vector is still usable afterwards.
        assert_ne!(back, machine.payload(vector, 1));
        assert_eq!(
            run(&mut machine, "Vector", "length", &[(Repr::Ref, vector)]).unwrap(),
            2
        );
    }

    /// The whole point of the indirection: the header a program is holding
    /// keeps its address while the store beneath it is replaced.
    #[test]
    fn a_vector_grows_without_moving() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);
        assert_eq!(machine.object_len(store), 2);

        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        let grown = machine.payload(items, 1);
        assert_ne!(grown, store, "a full store is replaced");
        // Twice the capacity, from a floor of four.
        assert_eq!(machine.object_len(grown), 4);
        assert_eq!(machine.payload(items, 0), 3);
        assert_eq!(words_of(&machine, grown), vec![1, 2, 3, 0]);

        // And the next push fits without replacing anything.
        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 4)],
        )
        .unwrap();
        assert_eq!(machine.payload(items, 1), grown);
        assert_eq!(machine.payload(items, 0), 4);
    }

    /// An empty vector's store starts at nothing, so the first push is the
    /// one that takes the floor.
    #[test]
    fn an_empty_vector_grows_to_the_floor() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[]);
        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 7)],
        )
        .unwrap();
        assert_eq!(machine.object_len(machine.payload(items, 1)), MIN_CAPACITY);
        assert_eq!(machine.payload(items, 0), 1);
    }

    /// A copy of a `Vector` is an alias, and every one of them names the same
    /// two words — so a growth through one is a growth every other sees.
    #[test]
    fn a_growth_is_visible_through_every_copy() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let alias = items;

        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(
            run(&mut machine, "Vector", "length", &[(Repr::Ref, alias)]).unwrap(),
            3
        );
        let word = run(
            &mut machine,
            "Vector",
            "get",
            &[(Repr::Ref, alias), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![3]));
    }

    #[test]
    fn set_answers_what_was_there_and_refuses_no_index() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);

        let word = run(
            &mut machine,
            "Vector",
            "set",
            &[(Repr::Ref, items), (Repr::Int, 1), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![2]));
        assert_eq!(
            words_of(&machine, machine.payload(items, 1)),
            vec![1, 20, 3]
        );

        // An index that is not already there writes nothing and answers the
        // same `None` `get` answers.
        for at in [-1i64, 3] {
            let word = run(
                &mut machine,
                "Vector",
                "set",
                &[(Repr::Ref, items), (Repr::Int, at as u64), (Repr::Int, 99)],
            )
            .unwrap();
            assert_eq!(case_of(&machine, word).0, "None");
        }
        assert_eq!(
            words_of(&machine, machine.payload(items, 1)),
            vec![1, 20, 3]
        );
    }

    /// The store keeps its room and loses its dead element: the word a `pop`
    /// vacates is zeroed, because a store's whole capacity is elements as far
    /// as the collector is concerned.
    #[test]
    fn pop_shortens_the_vector_and_clears_the_word_it_vacates() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);

        let word = run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![2]));
        assert_eq!(machine.payload(items, 0), 1);
        assert_eq!(
            machine.payload(items, 1),
            store,
            "the store is not replaced"
        );
        assert_eq!(words_of(&machine, store), vec![1, 0]);

        run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        let word = run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(case_of(&machine, word).0, "None");
    }

    #[test]
    fn remove_moves_what_follows_down_one() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);

        let word = run(
            &mut machine,
            "Vector",
            "remove",
            &[(Repr::Ref, items), (Repr::Int, 0)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![1]));
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(items, 1)), vec![2, 3, 0]);

        let word = run(
            &mut machine,
            "Vector",
            "remove",
            &[(Repr::Ref, items), (Repr::Int, 9)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word).0, "None");
    }

    #[test]
    fn a_vector_finds_an_element_and_slices_into_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);

        assert_eq!(
            run(
                &mut machine,
                "Vector",
                "contains",
                &[(Repr::Ref, items), (Repr::Int, 3)]
            )
            .unwrap(),
            1
        );
        let word = run(
            &mut machine,
            "Vector",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, word), ("Some".to_string(), vec![2]));
        assert_eq!(
            run(&mut machine, "Vector", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );

        // An `Array`, not a `Vector`: a slice is a reading of a sequence.
        let word = run(
            &mut machine,
            "Vector",
            "slice",
            &[(Repr::Ref, items), (Repr::Int, 0), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(words_of(&machine, word), vec![1, 2]);
        assert!(matches!(
            machine.program().layout(machine.object_layout(word)).shape,
            Shape::Elements {
                growable: false,
                ..
            }
        ));
    }

    /// `freeze()` is the O(1) conversion: the store *becomes* the array, at
    /// the same address, and the words the spare room occupied go back to the
    /// heap as a free block so that the heap stays walkable.
    #[test]
    fn freeze_turns_the_store_into_the_array_in_place() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        run(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        let store = machine.payload(items, 1);
        assert_eq!(machine.object_len(store), 4);
        let allocated = machine.allocated_words();

        let array = run(&mut machine, "Vector", "freeze", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(array, store, "nothing was copied");
        assert_eq!(
            machine.allocated_words(),
            allocated,
            "and nothing allocated"
        );
        assert_eq!(words_of(&machine, array), vec![1, 2, 3]);
        assert!(matches!(
            machine.program().layout(machine.object_layout(array)).shape,
            Shape::Elements {
                growable: false,
                ..
            }
        ));
        // The one spare word is a free block of its own, which is what keeps
        // the heap a walkable sequence of objects.
        assert_eq!(machine.object_layout(array + 1 + 3), LayoutId::FREE);
        assert_eq!(machine.object_len(array + 1 + 3), 0);
    }

    /// And every alias finds a vector `freeze()` consumed, in the oracle's
    /// words, whichever method it called.
    #[test]
    fn a_frozen_vector_refuses_every_method() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let alias = items;
        run(&mut machine, "Vector", "freeze", &[(Repr::Ref, items)]).unwrap();

        for (operation, operands) in [
            ("length", vec![(Repr::Ref, alias)]),
            ("push", vec![(Repr::Ref, alias), (Repr::Int, 1)]),
        ] {
            let error = run(&mut machine, "Vector", operation, &operands).unwrap_err();
            assert_eq!(
                error.message,
                format!("`{operation}` was called on a vector that `freeze()` already consumed")
            );
            assert_eq!(
                error.rule.as_deref(),
                Some("`freeze()` consumes its vector; the source vector is no longer usable.")
            );
        }
    }

    /// The elements say what family the vector belongs to, so a call with
    /// none says nothing and is refused rather than guessed at.
    #[test]
    fn vector_of_builds_from_its_elements_and_needs_one() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);

        let items = run(
            &mut machine,
            "Vector",
            "of",
            &[(Repr::Int, 1), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(items, 1)), vec![1, 2]);
        assert_eq!(
            machine.object_layout(items),
            vector(machine.program(), Repr::Int)
        );

        let error = run(&mut machine, "Vector", "of", &[]).unwrap_err();
        assert_eq!(
            error.message,
            "`Vector.of()` with no elements does not say what it is a vector of"
        );
    }

    /// The refusals a call that got the shape wrong reaches, in the oracle's
    /// words. None is reachable from a checked program; each is a lowering
    /// bug reported rather than a silent wrong answer.
    #[test]
    fn a_call_of_the_wrong_shape_is_refused_the_way_the_oracle_refuses_it() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[1]);
        let text = machine.new_string("no").unwrap();

        let error = run(&mut machine, "Array", "get", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Array.get` takes 1 argument(s), but 0 were given"
        );

        let error = run(
            &mut machine,
            "Array",
            "get",
            &[(Repr::Ref, items), (Repr::Ref, text)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Array.get` expects `Int` for `index`, but found `String`"
        );

        let error = run(&mut machine, "Vector", "length", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(error.message, "`Array` has no method `length`");

        let error = run(&mut machine, "Array", "length", &[(Repr::Ref, 0)]).unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");

        assert_eq!(
            int(run(&mut machine, "Array", "length", &[(Repr::Ref, items)]).unwrap()),
            1
        );
    }

    /// The one window a builtin has to get rooting wrong: `pop` takes the
    /// last reference to an element out of the store, and then allocates the
    /// `Some` that will hold it.
    ///
    /// The heap is small and full of dead objects, so that allocation
    /// collects. The vector is pushed as a temporary root by hand because
    /// there is no frame here to hold it — which is what a real call has, and
    /// what the operand words rely on everywhere else.
    #[test]
    fn pop_holds_the_element_it_took_out_across_the_allocation() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let store_layout = elements(machine.program(), Repr::Ref, true);
        let header_layout = vector(machine.program(), Repr::Ref);

        let store = machine.new_object(store_layout, 1).unwrap();
        machine.push_temp(store);
        let items = machine.new_object(header_layout, 0).unwrap();
        machine.push_temp(items);
        let kept = machine.new_string("the one that must survive").unwrap();
        machine.set_payload(store, 0, kept);
        machine.set_payload(items, 0, 1);
        machine.set_payload(items, 1, store);

        // Dead strings, two words each, until the heap is exactly full — so
        // that the three-word `Some` below cannot fit and has to collect.
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        let word = run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        let (case, payload) = case_of(&machine, word);
        assert_eq!(case, "Some");
        assert_eq!(read(&machine, payload[0]), "the one that must survive");
    }
}
