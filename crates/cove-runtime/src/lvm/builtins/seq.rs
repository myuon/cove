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
//! # An operand is one word, and not every element fits in one
//!
//! [`cove_lir::Builtin`] carries the layout of its *result* and of nothing
//! else, and a `CallBuiltin`'s arguments are base slots that need not be
//! adjacent — so a call says where an operand begins and never how wide it
//! is. One word is therefore all an operand can carry.
//!
//! Every operation that only *reads* elements is unaffected, because it reads
//! them out of the receiver where their width is known: `get`, `pop`,
//! `remove`, `slice`, `toArray`, `toVector`, `length` and `isEmpty` all work
//! at any stride. The four that need a whole element to arrive *as an
//! argument* — `contains`, `indexOf`, `push` and `set` — cannot be given one
//! wider than a word, and [`wide_element`] refuses rather than comparing or
//! storing a `Point`'s first word and calling that the value.
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
//! # `freeze()` is the one that refuses
//!
//! It consumes storage the caller must uniquely own, and uniqueness is not a
//! question this backend can answer: a handle is a word and words are not
//! counted. [`vector_freeze`] says so in the oracle's own words rather than
//! consuming anyway. The receiver check stays in front of it, because a
//! header whose store word is null is still a state the machine has to be
//! able to read.
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
//! turn — but they **zero the words they vacate**, because a store's shape
//! says its whole capacity is elements and the collector reads it that way. A
//! dead element left in the spare room would be a root, and a vector used as
//! a work queue would retain everything it had ever held.

use cove_lir::{LayoutId, Program, Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::builtins::operand::Operand;
use crate::lvm::builtins::{equal, make, operand};
use crate::lvm::exec::Machine;

/// The smallest store a `push` onto a full one asks for.
const MIN_CAPACITY: u32 = 4;

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
            stride: machine.words_of(elem),
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// A live `Vector`: its header, its store, and how much of the store is
/// value rather than spare room.
///
/// `len` and `capacity` are both element counts, as the header and the
/// store's own header state them; `stride` is what turns either into words.
struct Growable {
    header: u64,
    elem: LayoutId,
    stride: u32,
    len: u32,
    capacity: u32,
    store: u64,
}

/// Reads the receiver of a `Vector` method, refusing one `freeze()` consumed.
///
/// The liveness check happens here rather than in each operation because the
/// oracle asks it once, at the top of its `Vector` arm, before it looks at
/// the method name at all — so a consumed vector answers the same thing to
/// `length()` as to `push()`, and the message names whichever was called.
///
/// Nothing in this backend consumes a vector any more — see
/// [`vector_freeze`] — but a header whose store word is null is still a state
/// the machine can be handed, and reading one has to have an answer.
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
        stride: machine.words_of(elem),
        len: machine.payload(addr, 0) as u32,
        capacity: machine.object_len(store),
        store,
    })
}

/// The one-word family of `repr`, if this program declares one.
fn word_layout(program: &Program, repr: Repr) -> Option<LayoutId> {
    program
        .layouts
        .iter()
        .position(|layout| layout.shape == Shape::Word(repr))
        .map(|at| LayoutId(at as u32))
}

/// The `Int` family, which is what a position is a value of.
fn ints(program: &Program) -> Result<LayoutId, RuntimeError> {
    word_layout(program, Repr::Int).ok_or_else(|| operand::unknown_family("Int"))
}

/// The family of the value in `operand`, which is one word.
///
/// A scalar's family is the one-word layout of its `Repr`; a reference's is
/// the layout its own object header states, which is the one place the answer
/// exists — a `Repr::Ref` says a word is an address and nothing about what is
/// at the end of it.
fn family(machine: &Machine, operand: Operand) -> Result<LayoutId, RuntimeError> {
    match operand.0 {
        Repr::Ref if operand.1 != 0 => Ok(machine.object_layout(operand.1)),
        Repr::Ref => Err(operand::null_value()),
        repr => word_layout(machine.program(), repr)
            .ok_or_else(|| operand::unknown_family(&operand::type_name(machine, repr, operand.1))),
    }
}

/// A whole element cannot arrive as an argument when it is wider than a word.
///
/// Not the oracle's refusal: the oracle's values carry their own width and it
/// has nothing to refuse here. What this names is the operand ABI, which is
/// this backend's — a call hands over a base slot per argument and no width —
/// and the fix is to widen that rather than to widen anything below. Until
/// then a `Vector<Point>.push(p)` is refused, which is the honest answer: the
/// alternative is to write `p.x` into the store and call it a `Point`.
fn wide_element(machine: &Machine, shown: &str, elem: LayoutId) -> RuntimeError {
    let described = machine.program().layout(elem);
    RuntimeError::new(format!(
        "`{shown}` was given one word for a `{}`, which is {} words wide",
        described.name,
        described.width()
    ))
    .with_rule("An operand is one word: a call names where an argument begins, not how wide it is.")
    .with_help("give a builtin's operands their layouts, so that a call can hand over a value of more than one word")
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
    Ok(machine.payload_run(store, from * stride, (to - from) * stride))
}

/// The position of the first element equal to `wanted`, if there is one.
///
/// [`equal::same_word`] rather than [`equal::same_value`], because what is
/// being compared is two *operands* and an operand is a word: the element's
/// own `Repr` on one side and the argument's on the other. That is also why
/// a stride wider than one is refused here rather than compared — the
/// argument would be the first word of a value and nothing would say so.
fn position(
    machine: &Machine,
    shown: &str,
    elem: LayoutId,
    stride: u32,
    store: u64,
    len: u32,
    wanted: Operand,
) -> Result<Option<u32>, RuntimeError> {
    if stride != 1 {
        return Err(wide_element(machine, shown, elem));
    }
    let repr = machine.program().layout(elem).words[0];
    for at in 0..len {
        if equal::same_word(machine, (repr, machine.payload(store, at)), wanted)? {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

// --- Array -----------------------------------------------------------------

/// `Array.get(index) -> Option<T>`.
///
/// The answer is the `Option`'s words, with the element's run inline in the
/// payload region: an `Option<Point>` is `[disc, x, y]` and not an address.
pub(super) fn array_get(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Array.get", operands, 1)?;
    let items = array(machine, "get", receiver)?;
    match index(machine, "Array.get", args[0])? {
        Some(at) if at < items.len as usize => {
            let words = machine.payload_run(items.addr, at as u32 * items.stride, items.stride);
            make::some(machine, items.elem, &words)
        }
        _ => make::none(machine, items.elem),
    }
}

/// `Array.length() -> Int`.
///
/// Elements, not words: the header's length is the count the language asks
/// about, whatever an element of this array is made of.
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
    let at = position(
        machine,
        "Array.contains",
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
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Array.indexOf", operands, 1)?;
    let items = array(machine, "indexOf", receiver)?;
    let ints = ints(machine.program())?;
    match position(
        machine,
        "Array.indexOf",
        items.elem,
        items.stride,
        items.addr,
        items.len,
        args[0],
    )? {
        Some(at) => make::some(machine, ints, &[at as u64]),
        None => make::none(machine, ints),
    }
}

/// `Array.slice(from, to) -> Array<T>`.
pub(super) fn array_slice(
    machine: &mut Machine,
    operands: &[Operand],
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
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toVector", operands, 0)?;
    let items = array(machine, "toVector", receiver)?;
    let words = machine.payload_run(items.addr, 0, items.len * items.stride);
    make::vector_of(machine, items.elem, &words)
}

// --- Vector ----------------------------------------------------------------

/// `Vector.of(items...) -> Vector<T>`.
///
/// The element family comes from the first operand: the one-word layout of a
/// scalar's `Repr`, or, for a reference, the layout the object's own header
/// states. `Vector.of()` with no operands therefore says nothing about what
/// it is a vector of, and this refuses it rather than guessing — a store's
/// layout is what the collector traces its words by, so a `Vector<Int>` store
/// that a later `push` put a reference in would drop the object on the next
/// collection. The lowering knows the layout the checker resolved and can
/// allocate an empty vector itself, which is the one place the answer exists.
///
/// Only a one-word element can arrive this way, because only a one-word value
/// can be an operand at all. A `Vector.of(Point(1, 2))` is the case the
/// lowering has to build itself for the same reason `Vector.of()` is.
pub(super) fn vector_of(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let Some(first) = operands.first() else {
        return Err(RuntimeError::new(
            "`Vector.of()` with no elements does not say what it is a vector of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty vector where the element type is known"));
    };
    let elem = family(machine, *first)?;
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
    if items.stride != 1 {
        return Err(wide_element(machine, "Vector.push", items.elem));
    }
    let store = if items.len < items.capacity {
        items.store
    } else {
        grow(machine, &items)?
    };
    machine.set_payload_run(store, items.len * items.stride, &[args[0].1]);
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
/// The capacity is elements and so is the store's header length; the copy is
/// the whole payload, which is `len` elements at the element layout's width.
///
/// The old store is reachable from the header, which is an operand and
/// therefore a slot of the frame that called this builtin, so the allocation
/// below cannot free it. The new one is unrooted for exactly the copy, which
/// allocates nothing — and the elements are read *after* the allocation, so
/// nothing is held in a Rust `Vec` across a collection.
fn grow(machine: &mut Machine, items: &Growable) -> Result<u64, RuntimeError> {
    let layout = make::elements(machine.program(), items.elem, true)?;
    let capacity = items.capacity.saturating_mul(2).max(MIN_CAPACITY);
    let store = machine.new_object(layout, capacity)?;
    let words = machine.payload_run(items.store, 0, items.len * items.stride);
    machine.set_payload_run(store, 0, &words);
    machine.set_payload(items.header, 1, store);
    Ok(store)
}

/// `Vector.set(index, value) -> Option<T>`.
///
/// Answers what the index held before, which is what `get` would have
/// answered — so a caller that wants the displaced element does not have to
/// read it first and a caller that does not can ignore one word instead of
/// making two calls. An index outside the vector answers `None` and writes
/// nothing: a vector grows by `push`, and a `set` that sometimes grew would
/// make the length depend on the index.
pub(super) fn vector_set(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Vector.set", operands, 2)?;
    let items = vector(machine, "set", receiver)?;
    if items.stride != 1 {
        return Err(wide_element(machine, "Vector.set", items.elem));
    }
    let Some(at) = index(machine, "Vector.set", args[0])? else {
        return make::none(machine, items.elem);
    };
    if at >= items.len as usize {
        return make::none(machine, items.elem);
    }
    let at = at as u32 * items.stride;
    // What the index held before, read out before it is overwritten:
    // `v.set(i, x)` answers what `v.get(i)` would have.
    let was = machine.payload_run(items.store, at, items.stride);
    machine.set_payload_run(items.store, at, &[args[1].1]);
    make::some(machine, items.elem, &was)
}

/// `Vector.pop() -> Option<T>`.
///
/// The empty case is `remove(length() - 1)` on an empty vector, where that
/// index is `-1`, which `get`, `set` and `remove` all answer `None` for. One
/// rule about indices rather than a rule about indices and a rule about
/// emptiness.
pub(super) fn vector_pop(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, _) = operand::method("Vector.pop", operands, 0)?;
    let items = vector(machine, "pop", receiver)?;
    if items.len == 0 {
        return make::none(machine, items.elem);
    }
    let at = items.len - 1;
    let was = machine.payload_run(items.store, at * items.stride, items.stride);
    machine.set_payload_run(
        items.store,
        at * items.stride,
        &vec![0; items.stride as usize],
    );
    machine.set_payload(items.header, 0, at as u64);
    make::some(machine, items.elem, &was)
}

/// `Vector.remove(index) -> Option<T>`.
///
/// What follows the hole moves down by one *element*, which is one run copy
/// of `stride` words apiece rather than a word each — and the element the
/// vector no longer holds is zeroed out of the room it kept.
pub(super) fn vector_remove(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Vector.remove", operands, 1)?;
    let items = vector(machine, "remove", receiver)?;
    let Some(at) = index(machine, "Vector.remove", args[0])? else {
        return make::none(machine, items.elem);
    };
    if at >= items.len as usize {
        return make::none(machine, items.elem);
    }
    let at = at as u32;
    let stride = items.stride;
    let was = machine.payload_run(items.store, at * stride, stride);
    let tail = machine.payload_run(
        items.store,
        (at + 1) * stride,
        (items.len - at - 1) * stride,
    );
    machine.set_payload_run(items.store, at * stride, &tail);
    machine.set_payload_run(
        items.store,
        (items.len - 1) * stride,
        &vec![0; stride as usize],
    );
    machine.set_payload(items.header, 0, items.len as u64 - 1);
    make::some(machine, items.elem, &was)
}

/// `Vector.get(index) -> Option<T>`.
pub(super) fn vector_get(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Vector.get", operands, 1)?;
    let items = vector(machine, "get", receiver)?;
    match index(machine, "Vector.get", args[0])? {
        Some(at) if at < items.len as usize => {
            let words = machine.payload_run(items.store, at as u32 * items.stride, items.stride);
            make::some(machine, items.elem, &words)
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
    let at = position(
        machine,
        "Vector.contains",
        items.elem,
        items.stride,
        items.store,
        items.len,
        args[0],
    )?;
    Ok(at.is_some() as u64)
}

/// `Vector.indexOf(element) -> Option<Int>`.
pub(super) fn vector_index_of(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Vector.indexOf", operands, 1)?;
    let items = vector(machine, "indexOf", receiver)?;
    let ints = ints(machine.program())?;
    match position(
        machine,
        "Vector.indexOf",
        items.elem,
        items.stride,
        items.store,
        items.len,
        args[0],
    )? {
        Some(at) => make::some(machine, ints, &[at as u64]),
        None => make::none(machine, ints),
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
    let words = sliced(
        machine,
        "Vector.slice",
        items.store,
        items.stride,
        items.len,
        args,
    )?;
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
    let words = machine.payload_run(items.store, 0, items.len * items.stride);
    make::array_of(machine, items.elem, &words)
}

/// `Vector.freeze() -> Array<T>`, which this backend refuses.
///
/// `freeze()` is O(1) because it does not copy: it takes the storage away
/// from the vector and hands it back as an `Array`. That is only sound if the
/// caller holds the only handle to that storage, which is what the oracle
/// checks — it counts the handles to an `Rc` and refuses when there is more
/// than one.
///
/// **This machine cannot ask that question.** A handle here is a word, and
/// words are not counted: a `Vector` is one `Repr::Ref` in a slot, a copy of
/// it is the same word in another slot, and nothing anywhere records how many
/// there are. Local uniqueness is a static property of a program, and
/// establishing it is its own piece of work — a uniqueness analysis in the
/// lowering — not something a builtin can recover from the heap.
///
/// So the choice is between refusing every `freeze()` and consuming on every
/// `freeze()`, and only one of those is the safe side. Consuming anyway would
/// admit exactly the programs the oracle refuses, which are the programs where
/// another alias is still watching — and each of those aliases would then find
/// its vector emptied underneath it by an operation the language says is
/// checked. That is a divergence in the unsound direction, and
/// [issue #240](https://github.com/myuon/cove/issues/240) is where it was
/// decided that it must not stand.
///
/// Refusing is the sound direction. It is not a happy answer — a program the
/// oracle admits is refused here — but the message is the oracle's own, and
/// what it points at is the fallback the language already offers: `toArray()`,
/// which copies the elements in O(n) and asks nothing about who else is
/// holding the vector.
///
/// The receiver check comes first because a header whose store word is null
/// is still a state the machine may be handed, and it answers
/// [`operand::frozen`] for it — the same thing it answers `length()`.
pub(super) fn vector_freeze(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("freeze", operands, 0)?;
    vector(machine, "freeze", receiver)?;
    Err(RuntimeError::new(
        "`freeze()` needs uniquely owned vector storage, but another alias observes this vector",
    )
    .with_rule("`freeze()` consumes a locally unique vector and returns an immutable array in O(1).")
    .with_help(
        "call `toArray()` instead, which copies the elements in O(n), or drop the other alias before calling `freeze()`",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::tests::{
        elements, named, option_of, read, run, scalar, vector, word, words_of, world,
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

    /// What the `Option<Int>` in `words` holds.
    fn option_int(program: &Program, words: &[u64]) -> (String, Vec<u64>) {
        option_of(program, scalar(program, Repr::Int), words)
    }

    #[test]
    fn an_array_reports_what_it_holds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20, 30]);
        let empty = array_of(&mut machine, &[]);

        assert_eq!(
            word(&mut machine, "Array", "length", &[(Repr::Ref, items)]).unwrap(),
            3
        );
        assert_eq!(
            word(&mut machine, "Array", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );
        assert_eq!(
            word(&mut machine, "Array", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
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
            let words = run(
                machine,
                "Array",
                "get",
                &[(Repr::Ref, items), (Repr::Int, at as u64)],
            )
            .unwrap();
            option_int(&program, &words)
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
    /// six words, and a `get` answers a pair.
    #[test]
    fn an_array_of_points_is_walked_at_a_two_word_stride() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let layout = elements(&program, point, false);
        let items = machine.new_object(layout, 3).unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4, 5, 6]);

        // The header's length is elements, and so is what `length()` answers.
        assert_eq!(machine.object_len(items), 3);
        assert_eq!(
            word(&mut machine, "Array", "length", &[(Repr::Ref, items)]).unwrap(),
            3
        );
        assert_eq!(
            word(&mut machine, "Array", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );

        // `get` answers the whole element, inline in the `Some`'s payload
        // region: `[disc, x, y]` and not an address.
        let words = run(
            &mut machine,
            "Array",
            "get",
            &[(Repr::Ref, items), (Repr::Int, 1)],
        )
        .unwrap();
        assert_eq!(words, vec![1, 3, 4]);
        assert_eq!(
            option_of(&program, point, &words),
            ("Some".to_string(), vec![3, 4])
        );
        // The bound is in elements, so index 3 is past the end of three
        // `Point`s even though word 3 is inside the payload.
        let words = run(
            &mut machine,
            "Array",
            "get",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(option_of(&program, point, &words).0, "None");

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

    /// The other side of the stride: an operand is one word, so an element
    /// wider than one cannot arrive as an argument at all.
    ///
    /// Reading an element is unaffected — the receiver says how wide one is —
    /// but comparing against one or storing one means being handed a whole
    /// value, and a call hands over a base slot and no width. Refusing says
    /// so; the alternative is to compare a `Point`'s `x` and call that the
    /// `Point`.
    #[test]
    fn an_element_wider_than_a_word_cannot_arrive_as_an_operand() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let layout = elements(&program, point, false);
        let items = machine.new_object(layout, 1).unwrap();
        machine.set_payload_run(items, 0, &[1, 2]);
        let grown = word(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();

        for (receiver, operation, operands) in [
            (
                "Array",
                "contains",
                vec![(Repr::Ref, items), (Repr::Int, 1)],
            ),
            ("Array", "indexOf", vec![(Repr::Ref, items), (Repr::Int, 1)]),
            ("Vector", "push", vec![(Repr::Ref, grown), (Repr::Int, 1)]),
            (
                "Vector",
                "set",
                vec![(Repr::Ref, grown), (Repr::Int, 0), (Repr::Int, 1)],
            ),
        ] {
            let error = run(&mut machine, receiver, operation, &operands).unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "`{receiver}.{operation}` was given one word for a `Point`, which is 2 words wide"
                )
            );
            assert_eq!(
                error.rule.as_deref(),
                Some(
                    "An operand is one word: a call names where an argument begins, not how wide it is."
                )
            );
        }
        // And nothing was written: the vector is what it was.
        assert_eq!(words_of(&machine, machine.payload(grown, 1)), vec![1u64, 2]);
    }

    #[test]
    fn an_array_becomes_a_vector_and_a_vector_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = array_of(&mut machine, &[10, 20]);

        let grown = word(&mut machine, "Array", "toVector", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(machine.payload(grown, 0), 2);
        assert_eq!(words_of(&machine, machine.payload(grown, 1)), vec![10, 20]);

        let back = word(&mut machine, "Vector", "toArray", &[(Repr::Ref, grown)]).unwrap();
        assert_eq!(words_of(&machine, back), vec![10, 20]);
        // A copy, not the store: `toArray` is the O(n) conversion, and the
        // vector is still usable afterwards.
        assert_ne!(back, machine.payload(grown, 1));
        assert_eq!(
            word(&mut machine, "Vector", "length", &[(Repr::Ref, grown)]).unwrap(),
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

        word(
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
        word(
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
        word(
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

        word(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(
            word(&mut machine, "Vector", "length", &[(Repr::Ref, alias)]).unwrap(),
            3
        );
        let words = run(
            &mut machine,
            "Vector",
            "get",
            &[(Repr::Ref, alias), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(option_int(&program, &words), ("Some".to_string(), vec![3]));
    }

    #[test]
    fn set_writes_where_the_index_is_already_there_and_nowhere_else() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2, 3]);
        let ints = scalar(&program, Repr::Int);

        // The answer is what the index held before, which is what `get`
        // would have answered.
        let answer = run(
            &mut machine,
            "Vector",
            "set",
            &[(Repr::Ref, items), (Repr::Int, 1), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, ints, &answer),
            ("Some".to_string(), vec![2])
        );
        assert_eq!(
            words_of(&machine, machine.payload(items, 1)),
            vec![1, 20, 3]
        );

        // An index that is not already there writes nothing, which is `get`'s
        // answer to the same bad index said as a store that did not happen.
        for at in [-1i64, 3] {
            let answer = run(
                &mut machine,
                "Vector",
                "set",
                &[(Repr::Ref, items), (Repr::Int, at as u64), (Repr::Int, 99)],
            )
            .unwrap();
            assert_eq!(option_of(&program, ints, &answer).0, "None");
        }
        assert_eq!(
            words_of(&machine, machine.payload(items, 1)),
            vec![1, 20, 3]
        );
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
        assert_eq!(
            word(&mut machine, "Vector", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );

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

    /// `freeze()` refuses, in the oracle's own words, and leaves the vector
    /// exactly as it found it.
    ///
    /// The oracle refuses when another alias observes the vector. This backend
    /// cannot tell whether one does — a handle is a word and words are not
    /// counted — so it refuses always, which is the sound side of that
    /// question rather than the permissive one.
    #[test]
    fn freeze_refuses_because_uniqueness_is_not_a_question_a_word_can_answer() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);

        let error = word(&mut machine, "Vector", "freeze", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(
            error.message,
            "`freeze()` needs uniquely owned vector storage, but another alias observes this vector"
        );
        assert_eq!(
            error.rule.as_deref(),
            Some("`freeze()` consumes a locally unique vector and returns an immutable array in O(1).")
        );
        assert_eq!(
            error.help.as_deref(),
            Some("call `toArray()` instead, which copies the elements in O(n), or drop the other alias before calling `freeze()`")
        );

        // Nothing was consumed, so the vector is still a vector — and the
        // help's `toArray()` is a call a program can go on to make.
        assert_eq!(machine.payload(items, 1), store);
        assert_eq!(
            word(&mut machine, "Vector", "length", &[(Repr::Ref, items)]).unwrap(),
            2
        );
        let back = word(&mut machine, "Vector", "toArray", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(words_of(&machine, back), vec![1, 2]);
    }

    /// A header whose store word is null is still a state the machine can be
    /// handed, and every method answers the same thing for it — whichever one
    /// was called.
    ///
    /// Nothing in this backend produces one any more, now that `freeze()`
    /// refuses. It is built by hand here because the reading of it is what is
    /// under test: the check is at the top of the receiver, before the method
    /// name is looked at, which is where the oracle asks it.
    #[test]
    fn a_vector_with_no_storage_refuses_every_method() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let header = machine.new_object(vector(&program, int), 0).unwrap();

        for (operation, operands) in [
            ("length", vec![(Repr::Ref, header)]),
            ("push", vec![(Repr::Ref, header), (Repr::Int, 1)]),
            ("freeze", vec![(Repr::Ref, header)]),
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

        let items = word(
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
            vector(&program, scalar(&program, Repr::Int))
        );

        // A reference says which family it belongs to out of its own header,
        // which is the one place the answer is: a `Repr::Ref` says a word is
        // an address and nothing about what is at the end of it.
        let text = machine.new_string("a").unwrap();
        let items = word(&mut machine, "Vector", "of", &[(Repr::Ref, text)]).unwrap();
        assert_eq!(
            machine.object_layout(items),
            vector(&program, program.str_layout)
        );
        assert_eq!(
            read(&machine, machine.payload(machine.payload(items, 1), 0)),
            "a"
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
            word(&mut machine, "Array", "length", &[(Repr::Ref, items)]).unwrap(),
            1
        );
    }

    /// The one window a builtin has to get rooting wrong: `grow` allocates a
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

        // Dead strings, two words each, until the heap is exactly full — so
        // that the larger store below cannot fit and has to collect.
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        word(
            &mut machine,
            "Vector",
            "push",
            &[(Repr::Ref, items), (Repr::Ref, kept)],
        )
        .unwrap();
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
