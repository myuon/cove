//! `Array` and `Vector`.
//!
//! The two are one shape apart. An `Array` holds its elements in the object,
//! one indirection nearer than a `Vector`, and cannot grow. A `Vector` is a
//! two-word header — `[len, store]` — over a growable store, and it pays that
//! indirection for the one thing an `Array` does not need: its identity is
//! observable, so growing must not move the object a program is holding.
//!
//! # An element is a run of words, and the length still counts elements
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) states the rule this
//! module turns on:
//!
//! > One slot is one eight-byte word. One value may occupy one or more
//! > consecutive slots.
//!
//! So an `Array<Point>` is a run of *two-word* elements laid end to end, not
//! a run of addresses that each name two words somewhere else. An element's
//! **stride** is its element layout's width, and every offset into a payload
//! below is an element position multiplied by it.
//!
//! What is *not* multiplied is anything a program can see. A header's `len`
//! is elements, a bound handed to `slice` is elements, the position
//! `indexOf` answers is elements, and the capacity a `push` compares against
//! is elements. Keeping the two apart is the whole of the arithmetic here:
//! lengths and positions in elements, offsets in words.
//!
//! # An operand is an element, at whatever width one is
//!
//! An argument names a value location and carries its layout, so a whole
//! element arrives as a whole element. `contains`, `indexOf`, `push` and
//! `set` therefore work at any stride, as the operations that only *read*
//! elements always did — those read them out of the receiver, where the
//! width was never in doubt.
//!
//! Until an argument carried a layout those four refused. A call said where
//! an operand began and never how wide it was, so a `Vector<Point>.push(p)`
//! would have written `p.x` into the store and called it a `Point`, and
//! refusing was the honest answer. What remains is [`operand::run_of`],
//! which holds an incoming element to the receiver's element layout: a store
//! is traced by that layout's reference map, so a value of another family
//! written into one would be a collection following the wrong words.
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
//! slot.
//!
//! # `freeze()` is the one that consumes
//!
//! It takes the store away from the header and hands it back as an `Array`,
//! in place and in O(1). That is only sound where the caller holds the only
//! handle, and uniqueness is not a question this backend can answer — a
//! handle is a word and words are not counted. It does not have to: the
//! checker proves it before the program runs. Since ADR 0058 it is not this
//! module's either: `freeze()` is `std.vector.freeze` over a word
//! `Inst::RunFinish`, which is `Machine::finish_words`.
//!
//! # Growth
//!
//! A store is allocated to exactly the elements it is built from, because
//! `Vector.of(1, 2, 3)` and `Array.toVector()` know the count and spare room
//! nobody asked for is room a program pays for. A `push` onto a full store
//! allocates one of **twice the capacity, from a floor of four**, copies the
//! elements across, and writes the new store into word 1 — the header does
//! not move, so no reference to it anywhere goes stale. Appending is
//! therefore amortised O(1). That growth, and `freeze()`'s relabel, are
//! [`crate::vm::exec::runs`]' — the one growable-run core a byte buffer grows
//! through too.
//!
//! A store never shrinks. `pop` and `remove` leave the room they vacate, so
//! that a program that fills and empties one does not reallocate on every
//! turn — but they **zero the words they vacate**, because a store's shape
//! says its whole capacity is elements and the collector reads it that way. A
//! dead element left in the spare room would be a root, and a vector used as
//! a work queue would retain everything it had ever held.

#[cfg(test)]
use cove_ir::Program;
use cove_ir::{LayoutId, Repr, Shape, Storage};

use crate::error::RuntimeError;
use crate::vm::builtins::operand::Operand;
use crate::vm::builtins::{equal, make, operand};
use crate::vm::exec::runs::{Growable, GROWABLE_LEN, GROWABLE_STORE};
use crate::vm::exec::Machine;

// --- reading a receiver ----------------------------------------------------

/// The elements of an `Array`.
///
/// `len` is elements and `stride` is the words one of them occupies, so the
/// payload offset of element `at` is `at * stride` and the object's payload
/// is `len * stride` words long.
struct Fixed {
    elem: LayoutId,
    stride: u32,
    len: u32,
    addr: u64,
}

fn array(machine: &Machine, method: &str, receiver: Operand<'_>) -> Result<Fixed, RuntimeError> {
    let Some((Repr::Ref, addr)) = operand::as_word(machine, receiver) else {
        return Err(operand::no_method(machine, receiver, method));
    };
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements {
            elem,
            growable: false,
        } => Ok(Fixed {
            elem,
            stride: machine.words_of(elem),
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// A live `Vector`: the growable run under its header, and the element that
/// run is a run of.
///
/// `run.len` and `run.capacity` are both element counts, as the header and
/// the store's own header state them; `stride` is what turns either into
/// words.
struct Items {
    run: Growable,
    elem: LayoutId,
    stride: u32,
}

/// Reads the receiver of a `Vector` method, refusing one `freeze()` consumed.
///
/// The liveness check happens here rather than in each operation because the
/// oracle asks it once, at the top of its `Vector` arm, before it looks at
/// the method name at all — so a consumed vector answers the same thing to
/// `length()` as to `push()`, and the message names whichever was called.
fn vector(machine: &Machine, method: &str, receiver: Operand<'_>) -> Result<Items, RuntimeError> {
    let Some((Repr::Ref, addr)) = operand::as_word(machine, receiver) else {
        return Err(operand::no_method(machine, receiver, method));
    };
    if addr == 0 {
        return Err(operand::null_value());
    }
    let Shape::Vector { elem } = machine.program().layout(machine.object_layout(addr)).shape else {
        return Err(operand::no_method(machine, receiver, method));
    };
    let store = machine.payload(addr, GROWABLE_STORE);
    if store == 0 {
        return Err(operand::frozen(method));
    }
    Ok(Items {
        run: Growable {
            owner: addr,
            store,
            len: machine.payload(addr, GROWABLE_LEN) as u32,
            capacity: machine.object_len(store),
            storage: Storage::Words(elem),
        },
        elem,
        stride: machine.words_of(elem),
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
    argument: Operand<'_>,
) -> Result<Option<usize>, RuntimeError> {
    let at = operand::int(machine, method, "index", argument)?;
    Ok((at >= 0).then_some(at as usize))
}

/// The words of the elements `store[from..to]`, with both bounds clamped into
/// the sequence and a `to` at or below `from` answering nothing.
///
/// The bounds are elements and the answer is their words flattened, which is
/// what [`make::array_of`] takes — so the one multiplication by the stride is
/// the one that turns the clamped element range into a payload run.
fn sliced(
    machine: &Machine,
    shown: &str,
    store: u64,
    stride: u32,
    len: u32,
    args: &[Operand<'_>],
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
    Ok(machine.payload_run(store, from * stride, (to - from) * stride))
}

/// The position of the first element equal to `wanted`, if there is one.
///
/// The element is read out of the store as the value location it is — the
/// element layout and the run of words at its position — and compared with
/// the argument as the value location *it* is. Neither side is read as the
/// other's layout, which is what lets a boxed `Int` be found in a
/// `Set<Int>`, and neither is narrowed to a word, which is what lets a
/// `Point` be found in an `Array<Point>` at all.
fn position(
    machine: &Machine,
    elem: LayoutId,
    stride: u32,
    store: u64,
    len: u32,
    wanted: Operand<'_>,
) -> Result<Option<u32>, RuntimeError> {
    for at in 0..len {
        let words = machine.payload_run(store, at * stride, stride);
        let held = Operand {
            layout: elem,
            words: &words,
        };
        if equal::same(machine, held, wanted)? {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

// --- Array -----------------------------------------------------------------

/// `Array.contains(element) -> Bool`.
pub(super) fn array_contains(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.contains", operands, 1)?;
    let items = array(machine, "contains", receiver)?;
    let at = position(
        machine,
        items.elem,
        items.stride,
        items.addr,
        items.len,
        args[0],
    )?;
    Ok(at.is_some() as u64)
}

/// `Array.indexOf(element) -> Option<Int>`.
pub(super) fn array_index_of(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (receiver, args) = operand::method("Array.indexOf", operands, 1)?;
    let items = array(machine, "indexOf", receiver)?;
    match position(
        machine,
        items.elem,
        items.stride,
        items.addr,
        items.len,
        args[0],
    )? {
        Some(at) => make::some(machine, result, &[at as u64], out),
        None => make::none(machine, result, out),
    }
}

/// `Array.slice(from, to) -> Array<T>`.
pub(super) fn array_slice(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Array.slice", operands, 2)?;
    let items = array(machine, "slice", receiver)?;
    let words = sliced(
        machine,
        "Array.slice",
        items.addr,
        items.stride,
        items.len,
        args,
    )?;
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
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toVector", operands, 0)?;
    let items = array(machine, "toVector", receiver)?;
    let words = machine.payload_run(items.addr, 0, items.len * items.stride);
    make::vector_of(machine, items.elem, &words)
}

// --- Vector ----------------------------------------------------------------

/// `Vector.pop() -> Option<T>`.
///
/// The empty case is `remove(length() - 1)` on an empty vector, where that
/// index is `-1`, which `get`, `set` and `remove` all answer `None` for. One
/// rule about indices rather than a rule about indices and a rule about
/// emptiness.
pub(super) fn vector_pop(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (receiver, _) = operand::method("Vector.pop", operands, 0)?;
    let items = vector(machine, "pop", receiver)?;
    if items.run.len == 0 {
        return make::none(machine, result, out);
    }
    let at = items.run.len - 1;
    let was = machine.payload_run(items.run.store, at * items.stride, items.stride);
    machine.set_payload_run(
        items.run.store,
        at * items.stride,
        &vec![0; items.stride as usize],
    );
    machine.set_payload(items.run.owner, 0, at as u64);
    make::some(machine, result, &was, out)
}

/// `Vector.remove(index) -> Option<T>`.
///
/// What follows the hole moves down by one *element*, which is one run copy
/// of `stride` words apiece rather than a word each — and the element the
/// vector no longer holds is zeroed out of the room it kept.
pub(super) fn vector_remove(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (receiver, args) = operand::method("Vector.remove", operands, 1)?;
    let items = vector(machine, "remove", receiver)?;
    let Some(at) = index(machine, "Vector.remove", args[0])? else {
        return make::none(machine, result, out);
    };
    if at >= items.run.len as usize {
        return make::none(machine, result, out);
    }
    let at = at as u32;
    let stride = items.stride;
    let was = machine.payload_run(items.run.store, at * stride, stride);
    let tail = machine.payload_run(
        items.run.store,
        (at + 1) * stride,
        (items.run.len - at - 1) * stride,
    );
    machine.set_payload_run(items.run.store, at * stride, &tail);
    machine.set_payload_run(
        items.run.store,
        (items.run.len - 1) * stride,
        &vec![0; stride as usize],
    );
    machine.set_payload(items.run.owner, 0, items.run.len as u64 - 1);
    make::some(machine, result, &was, out)
}

/// `Vector.contains(element) -> Bool`.
pub(super) fn vector_contains(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.contains", operands, 1)?;
    let items = vector(machine, "contains", receiver)?;
    let at = position(
        machine,
        items.elem,
        items.stride,
        items.run.store,
        items.run.len,
        args[0],
    )?;
    Ok(at.is_some() as u64)
}

/// `Vector.indexOf(element) -> Option<Int>`.
pub(super) fn vector_index_of(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (receiver, args) = operand::method("Vector.indexOf", operands, 1)?;
    let items = vector(machine, "indexOf", receiver)?;
    match position(
        machine,
        items.elem,
        items.stride,
        items.run.store,
        items.run.len,
        args[0],
    )? {
        Some(at) => make::some(machine, result, &[at as u64], out),
        None => make::none(machine, result, out),
    }
}

/// `Vector.slice(from, to) -> Array<T>`.
///
/// An **`Array`**, not a `Vector`: the oracle answers one, and it is the
/// right answer — a slice is a reading of a sequence and nothing about it
/// asks to be grown.
pub(super) fn vector_slice(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Vector.slice", operands, 2)?;
    let items = vector(machine, "slice", receiver)?;
    let words = sliced(
        machine,
        "Vector.slice",
        items.run.store,
        items.stride,
        items.run.len,
        args,
    )?;
    make::array_of(machine, items.elem, &words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::tests::{
        elements, named, option_of, read, run, scalar, values, vector, word, words_of, world,
    };

    /// An `Array<Int>` holding `values`.
    fn array_of(machine: &mut Machine, values: &[i64]) -> u64 {
        let int = scalar(machine.program(), Repr::Int);
        let layout = elements(machine.program(), int, false);
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
        let int = scalar(machine.program(), Repr::Int);
        let words: Vec<u64> = values.iter().map(|value| *value as u64).collect();
        make::vector_of(machine, int, &words).expect("the fixture declares every family")
    }

    /// `Vector.push(value)`, which is `Machine::push_words` since ADR 0058 moved
    /// `push` into the standard library over a word `growable-push`.
    ///
    /// The instruction reads the element straight out of the frame by address;
    /// there is no frame here, so the words are placed in an object of their
    /// own, rooted for the call, and handed over by the address of its payload.
    fn push(
        machine: &mut Machine,
        items: u64,
        elem: LayoutId,
        words: &[u64],
    ) -> Result<(), RuntimeError> {
        let holder = machine
            .new_object(elements(machine.program(), elem, false), 1)
            .expect("the fixture's heap is large enough");
        let mark = machine.temps();
        machine.push_temp(holder);
        machine.set_payload_run(holder, 0, words);
        let answer = machine.push_words(items, elem, holder + 1);
        machine.release_temps(mark);
        answer
    }

    /// What the `Option<Int>` in `words` holds.
    fn option_int(program: &Program, words: &[u64]) -> (String, Vec<u64>) {
        option_of(program, scalar(program, Repr::Int), words)
    }

    #[test]
    fn an_array_finds_an_element_by_value() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 20]);

        for (wanted, found) in [(20i64, 1u64), (99, 0)] {
            assert_eq!(
                word(
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
        let words = run(
            &mut machine,
            "Array",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![1]));
        let words = run(
            &mut machine,
            "Array",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 99)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words).0, "None");
    }

    /// Both bounds are clamped and a `to` at or below `from` answers nothing,
    /// so no bound can stop the run.
    #[test]
    fn an_array_slice_clamps_both_bounds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 30]);
        let slice = |machine: &mut Machine, from: i64, to: i64| {
            let addr = word(
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
            words_of(machine, addr)
        };
        assert_eq!(slice(&mut machine, 1, 3), vec![20, 30]);
        assert_eq!(slice(&mut machine, -5, 99), vec![10, 20, 30]);
        assert_eq!(slice(&mut machine, 2, 1), Vec::<u64>::new());
    }

    /// An `Array<Point>` is a run of two-word elements. Everything that walks
    /// one counts in elements and offsets in words, and this is where that
    /// distinction is load-bearing: a length of three is three `Point`s and
    /// six words, and a `pop` answers a pair.
    #[test]
    fn an_array_of_points_is_walked_at_a_two_word_stride() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let layout = elements(&program, point, false);
        let items = machine.new_object(layout, 3).unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4, 5, 6]);

        // The header's length is elements.
        assert_eq!(machine.object_len(items), 3);

        // A slice is a shorter run of the same elements: two of them, which
        // is four words.
        let slice = word(
            &mut machine,
            "Array",
            "slice",
            &[(Repr::Ref, items), (Repr::Int, 1), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(machine.object_len(slice), 2);
        assert_eq!(words_of(&machine, slice), vec![3, 4, 5, 6]);

        // And a `Vector` over the same elements keeps both: three in the
        // header's count, six in the store.
        let grown = word(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(machine.payload(grown, 0), 3);
        let store = machine.payload(grown, 1);
        assert_eq!(machine.object_len(store), 3);
        assert_eq!(words_of(&machine, store), vec![1, 2, 3, 4, 5, 6]);

        // `pop` takes a whole element off and zeroes both of its words.
        let words = run(&mut machine, "Vector", "pop", &[(Repr::Ref, grown)]).unwrap();
        assert_eq!(
            option_of(&program, point, &words),
            ("Some".to_string(), vec![5, 6])
        );
        assert_eq!(machine.payload(grown, 0), 2);
        assert_eq!(words_of(&machine, store), vec![1, 2, 3, 4, 0, 0]);

        // As does `remove`, and what follows it moves down by an element.
        let words = run(
            &mut machine,
            "Vector",
            "remove",
            &[(Repr::Ref, grown), (Repr::Int, 0)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, point, &words),
            ("Some".to_string(), vec![1, 2])
        );
        assert_eq!(words_of(&machine, store), vec![3, 4, 0, 0, 0, 0]);
    }

    /// The other side of the stride: an operand is a value location, so an
    /// element of any width arrives as an argument.
    ///
    /// All four of these refused until an argument carried its layout —
    /// reading an element was never in doubt, because the receiver says how
    /// wide one is, and comparing against one or storing one meant being
    /// handed a whole value that a base slot could not describe.
    #[test]
    fn an_element_wider_than_a_word_arrives_whole() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let layout = elements(&program, point, false);
        let items = machine.new_object(layout, 2).unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4]);
        let arrays = machine.object_layout(items);
        let grown = word(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();
        let vectors = machine.object_layout(grown);

        assert_eq!(
            values(
                &mut machine,
                "Array",
                "contains",
                &[(arrays, &[items]), (point, &[3, 4])]
            )
            .unwrap(),
            vec![1]
        );
        assert_eq!(
            values(
                &mut machine,
                "Array",
                "contains",
                &[(arrays, &[items]), (point, &[3, 9])]
            )
            .unwrap(),
            vec![0]
        );
        let found = values(
            &mut machine,
            "Array",
            "indexOf",
            &[(arrays, &[items]), (point, &[3, 4])],
        )
        .unwrap();
        let int = scalar(&program, Repr::Int);
        assert_eq!(
            option_of(&program, int, &found),
            ("Some".to_string(), vec![1])
        );

        // A `push` writes both words at the element's own stride.
        push(&mut machine, grown, point, &[5, 6]).unwrap();
        // The store grew, so its spare room is zeroed room past the length —
        // the three elements are the first six words of it.
        let store = machine.payload(grown, 1);
        assert_eq!(machine.payload_run(store, 0, 6), vec![1u64, 2, 3, 4, 5, 6]);
        assert_eq!(machine.object_layout(grown), vectors);
    }

    /// A store is traced by its element layout's reference map and searched
    /// at its width, so a value of another family put into one would be both
    /// a collection following the wrong words and a search comparing them.
    /// That is the one thing an operand's layout still has to be held to.
    #[test]
    fn an_element_of_another_family_is_refused_rather_than_stored() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let int = scalar(&program, Repr::Int);
        let layout = elements(&program, point, true);
        let store = machine.new_object(layout, 1).unwrap();
        let grown = machine.new_object(vector(&program, point), 0).unwrap();
        machine.set_payload(grown, 1, store);
        // A word `growable-push` is given its element layout by the lowering
        // rather than by an operand, so what it holds to the store's family is
        // the owner: a vector of another element is refused before a word moves.
        let error = push(&mut machine, grown, int, &[1]).unwrap_err();
        assert_eq!(
            error.message,
            "a growable run of `Int` was expected here, and this object is not one"
        );
    }

    #[test]
    fn an_array_becomes_a_vector_and_a_vector_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20]);

        let grown = word(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(machine.payload(grown, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(grown, 1)), vec![10, 20]);

        let back = word(
            &mut machine,
            "Vector",
            "slice",
            &[(Repr::Ref, grown), (Repr::Int, 0), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(words_of(&machine, back), vec![10, 20]);
        // A copy, not the store: a slice of the whole is the O(n) conversion,
        // and the vector is still what it was afterwards.
        assert_ne!(back, machine.payload(grown, 1));
        assert_eq!(machine.payload(grown, 0), 2);
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

        let int = scalar(&program, Repr::Int);
        push(&mut machine, items, int, &[3]).unwrap();
        let grown = machine.payload(items, 1);
        assert_ne!(grown, store, "a full store is replaced");
        // Twice the capacity, from a floor of four.
        assert_eq!(machine.object_len(grown), 4);
        assert_eq!(machine.payload(items, 0), 3);
        assert_eq!(words_of(&machine, grown), vec![1, 2, 3, 0]);

        // And the next push fits without replacing anything.
        push(&mut machine, items, int, &[4]).unwrap();
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
        push(&mut machine, items, scalar(&program, Repr::Int), &[7]).unwrap();
        assert_eq!(
            u64::from(machine.object_len(machine.payload(items, 1))),
            crate::vm::exec::runs::MIN_GROWABLE_ELEMENTS
        );
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

        push(&mut machine, items, scalar(&program, Repr::Int), &[3]).unwrap();
        assert_eq!(machine.payload(alias, 0), 3);
        let words = run(
            &mut machine,
            "Vector",
            "indexOf",
            &[(Repr::Ref, alias), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![2]));
    }

    /// The store keeps its room and loses its dead element: the words a `pop`
    /// vacates are zeroed, because a store's whole capacity is elements as far
    /// as the collector is concerned.
    #[test]
    fn pop_shortens_the_vector_and_clears_the_words_it_vacates() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);

        let words = run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![2]));
        assert_eq!(machine.payload(items, 0), 1);
        assert_eq!(
            machine.payload(items, 1),
            store,
            "the store is not replaced"
        );
        assert_eq!(words_of(&machine, store), vec![1, 0]);

        run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        let words = run(&mut machine, "Vector", "pop", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(option_int(&program, &words).0, "None");
    }

    #[test]
    fn remove_moves_what_follows_down_one() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);

        let words = run(
            &mut machine,
            "Vector",
            "remove",
            &[(Repr::Ref, items), (Repr::Int, 0)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![1]));
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(items, 1)), vec![2, 3, 0]);

        let words = run(
            &mut machine,
            "Vector",
            "remove",
            &[(Repr::Ref, items), (Repr::Int, 9)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words).0, "None");
    }

    #[test]
    fn a_vector_finds_an_element_and_slices_into_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);

        assert_eq!(
            word(
                &mut machine,
                "Vector",
                "contains",
                &[(Repr::Ref, items), (Repr::Int, 3)]
            )
            .unwrap(),
            1
        );
        let words = run(
            &mut machine,
            "Vector",
            "indexOf",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![2]));

        // `isEmpty` is not a machine builtin for `Vector` either: it is
        // `std.vector.isEmpty`, and it is `cove-sema`'s and `cove-ir`'s
        // tests that check it rather than a word read off the machine here.

        // An `Array`, not a `Vector`: a slice is a reading of a sequence.
        let addr = word(
            &mut machine,
            "Vector",
            "slice",
            &[(Repr::Ref, items), (Repr::Int, 0), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(words_of(&machine, addr), vec![1, 2]);
        assert!(matches!(
            machine.program().layout(machine.object_layout(addr)).shape,
            Shape::Elements {
                growable: false,
                ..
            }
        ));
    }

    /// `freeze()` hands back the store it was already holding, and empties
    /// the vector.
    ///
    /// The array's address *is* the store's, which is the whole of what makes
    /// this O(1): nothing was copied, the header was rewritten from the
    /// growable family to the fixed one, and every element stayed where it
    /// was.
    #[test]
    fn freeze_takes_the_store_and_hands_it_back_as_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);
        let int = scalar(&program, Repr::Int);

        let frozen = machine
            .finish_words(items, elements(&program, int, false), int)
            .unwrap();
        assert_eq!(frozen, store);
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);
        assert!(matches!(
            machine
                .program()
                .layout(machine.object_layout(frozen))
                .shape,
            Shape::Elements {
                growable: false,
                ..
            }
        ));

        // Consumed: the header stays where it is and answers that it has no
        // storage, which is the state a checked program cannot reach.
        assert_eq!(machine.payload(items, 1), 0);
        let error = word(
            &mut machine,
            "Vector",
            "contains",
            &[(Repr::Ref, items), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert!(error.message.contains("freeze"), "{}", error.message);
    }

    /// A `push` grows the store past the length, and what `freeze()` answers
    /// is the length rather than the capacity.
    ///
    /// The words in between are given back as a free block, which the sweep
    /// has to be able to walk over: an object whose header says two elements
    /// sitting in room for four would otherwise leave two words that belong
    /// to nothing.
    #[test]
    fn freeze_gives_back_the_room_the_vector_grew_into() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1]);
        push(&mut machine, items, scalar(&program, Repr::Int), &[2]).unwrap();
        let store = machine.payload(items, 1);
        assert_eq!(
            u64::from(machine.object_len(store)),
            crate::vm::exec::runs::MIN_GROWABLE_ELEMENTS
        );

        let int = scalar(&program, Repr::Int);
        let frozen = machine
            .finish_words(items, elements(&program, int, false), int)
            .unwrap();
        assert_eq!(frozen, store);
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);

        // The heap is still walkable, which a collection is the way to ask:
        // the sweep walks every object from the first to the bump pointer,
        // and a released run that is not a free block of its own is where
        // that walk would leave the sequence.
        machine.push_temp(frozen);
        while machine.collected().collections == 0 {
            growable(&mut machine, &[3, 4, 5, 6, 7, 8, 9, 10]);
        }
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);
    }

    /// A header whose store word is null is still a state the machine can be
    /// handed, and every operation refuses it.
    ///
    /// A checked program never produces one — `cove_sema::unique` proves a
    /// vector is not read after its `freeze()` — so it is built by hand here,
    /// because the reading of it is what is under test. A method still
    /// dispatched here names itself, as the oracle's does; a run instruction
    /// over the vector — `push` and `freeze` since ADR 0058 — is below every
    /// method, and answers the one internal-invariant sentence.
    #[test]
    fn a_vector_with_no_storage_refuses_every_method() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let header = machine.new_object(vector(&program, int), 0).unwrap();

        let error = run(
            &mut machine,
            "Vector",
            "contains",
            &[(Repr::Ref, header), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`contains` was called on a vector that `freeze()` already consumed"
        );
        assert_eq!(
            error.rule.as_deref(),
            Some("`freeze()` consumes its vector; the source vector is no longer usable.")
        );

        let pushed = push(&mut machine, header, int, &[1]).unwrap_err();
        let finished = machine
            .finish_words(header, elements(&program, int, false), int)
            .unwrap_err();
        for error in [pushed, finished] {
            assert_eq!(error.message, crate::builtins::CONSUMED_VECTOR);
            assert_eq!(error.rule, None);
        }
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

        let error = run(&mut machine, "Array", "contains", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Array.contains` takes 1 argument(s), but 0 were given"
        );

        let error = run(
            &mut machine,
            "Array",
            "slice",
            &[(Repr::Ref, items), (Repr::Ref, text), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Array.slice` expects `Int` for `from`, but found `String`"
        );

        let error = run(
            &mut machine,
            "Vector",
            "contains",
            &[(Repr::Ref, items), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(error.message, "`Array` has no method `contains`");

        let error = run(
            &mut machine,
            "Array",
            "contains",
            &[(Repr::Ref, 0), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");

        assert_eq!(
            word(
                &mut machine,
                "Array",
                "contains",
                &[(Repr::Ref, items), (Repr::Int, 1)]
            )
            .unwrap(),
            1
        );
    }

    /// The one window a builtin has to get rooting wrong: a growth allocates a
    /// larger store while the elements it is about to copy are reachable only
    /// through the header.
    ///
    /// The heap is small and full of dead objects, so that allocation
    /// collects. The header is pushed as a temporary root by hand because
    /// there is no frame here to hold it — which is what a real call has, and
    /// what the operand words rely on everywhere else. That is the whole
    /// invariant: the old store is traced *through* the header, so it and its
    /// elements survive the allocation that replaces it.
    #[test]
    fn a_growth_holds_the_store_it_copies_from_across_the_allocation() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let text = program.str_layout;
        let store_layout = elements(&program, text, true);
        let header_layout = vector(&program, text);

        let store = machine.new_object(store_layout, 1).unwrap();
        machine.push_temp(store);
        let items = machine.new_object(header_layout, 0).unwrap();
        machine.push_temp(items);
        let kept = machine.new_string("the one that must survive").unwrap();
        machine.push_temp(kept);
        machine.set_payload(store, 0, kept);
        machine.set_payload(items, 0, 1);
        machine.set_payload(items, 1, store);
        // The element a push reads by address, placed and rooted before the heap
        // is filled: a frame's slot, in a fixture that has no frame.
        let holder = machine
            .new_object(elements(&program, text, false), 1)
            .unwrap();
        machine.push_temp(holder);
        machine.set_payload(holder, 0, kept);

        // Dead strings, two words each, until the heap is exactly full — so
        // that the larger store below cannot fit and has to collect.
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        machine.push_words(items, text, holder + 1).unwrap();
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        let grown = machine.payload(items, 1);
        assert_ne!(grown, store, "a full store is replaced");
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(
            read(&machine, machine.payload(grown, 0)),
            "the one that must survive"
        );
        assert_eq!(
            read(&machine, machine.payload(grown, 1)),
            "the one that must survive"
        );
    }
}
