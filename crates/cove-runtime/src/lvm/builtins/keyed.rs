//! `Set` and `Map`.
//!
//! Both are sorted runs: a `Set` is one word per member and a `Map` two words
//! per entry, key then value, each in the ascending order
//! [`super::key`] defines. The order is part of the value — the language says
//! a set iterates and renders in ascending order — so it is kept rather than
//! recovered, and every lookup is a binary search over it.
//!
//! # Both are immutable, so an update is a new object
//!
//! `inserted` and `removed` are past participles for a reason: neither writes
//! through the receiver. Each allocates the run the answer needs and fills it
//! in one pass, so the result is sorted because it was built sorted and never
//! because something sorted it. That is also why the object is allocated to
//! its final length first: the length is known before the first word is
//! written — the search that found where the element goes also answered
//! whether it was already there.
//!
//! # Nothing here needs a temporary root
//!
//! Every operation below allocates at most once, and it allocates *before* it
//! writes anything. The words it then copies come from the receiver and the
//! arguments, which are slots of the frame that called the builtin and are
//! therefore already walked. There is no window in which a half-built object
//! exists and something that could collect runs — which is the same reason
//! [`super::make::array_of`] takes none, said of a run that is filled in
//! sorted order instead of in order of arrival.
//!
//! # What the oracle calls these, and what it refuses them for
//!
//! [`crate::builtins`]' `Map` and `Set` arms are the specification, and every
//! message below is theirs word for word: an argument that cannot be a key,
//! a literal with the same key twice, a method a family does not have. The
//! first of those is [`super::key::check`], asked before anything is compared
//! because that is where the oracle asks it.

use std::cmp::Ordering;

use cove_lir::{Repr, Shape};
use cove_schema::builtins::MAP_ENTRY;

use crate::error::RuntimeError;
use crate::lvm::builtins::operand::{self, Operand};
use crate::lvm::builtins::{key, make, render};
use crate::lvm::exec::Machine;

// --- reading a receiver ----------------------------------------------------

/// A `Set`'s members: what it holds them as, how many, and where.
struct Members {
    elem: Repr,
    len: u32,
    addr: u64,
}

fn set(machine: &Machine, method: &str, receiver: Operand) -> Result<Members, RuntimeError> {
    let (repr, addr) = receiver;
    if repr != Repr::Ref {
        return Err(operand::no_method(machine, receiver, method));
    }
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Members { elem } => Ok(Members {
            elem,
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// A `Map`'s entries: the two families a pair of words is read as, how many
/// pairs there are, and where.
struct Entries {
    key: Repr,
    value: Repr,
    len: u32,
    addr: u64,
}

fn map(machine: &Machine, method: &str, receiver: Operand) -> Result<Entries, RuntimeError> {
    let (repr, addr) = receiver;
    if repr != Repr::Ref {
        return Err(operand::no_method(machine, receiver, method));
    }
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Entries { key, value } => Ok(Entries {
            key,
            value,
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

// --- searching and building a sorted run -----------------------------------

/// Where `wanted` is in the sorted run at `addr`, or where it would go.
///
/// `Ok(at)` and `Err(at)` mean what [`slice::binary_search`] means by them,
/// and the search is written out rather than borrowed from it because the
/// comparison is fallible: a key that nests too deeply stops the run instead
/// of answering an order.
///
/// `stride` is how many words an element occupies — one for a member, two for
/// an entry — and the key is always the first of them.
fn seek(
    machine: &Machine,
    elem: Repr,
    addr: u64,
    stride: u32,
    len: u32,
    wanted: Operand,
) -> Result<Result<u32, u32>, RuntimeError> {
    let (mut low, mut high) = (0, len);
    while low < high {
        let at = low + (high - low) / 2;
        match key::compare(machine, (elem, machine.payload(addr, at * stride)), wanted)? {
            Ordering::Less => low = at + 1,
            Ordering::Greater => high = at,
            Ordering::Equal => return Ok(Ok(at)),
        }
    }
    Ok(Err(low))
}

/// Moves the `len - at` elements above `at` up by one, opening the room a new
/// one goes in.
///
/// Backwards, so that no word is overwritten before it has been read.
fn open(machine: &mut Machine, addr: u64, stride: u32, at: u32, len: u32) {
    for word in (at * stride..len * stride).rev() {
        let held = machine.payload(addr, word);
        machine.set_payload(addr, word + stride, held);
    }
}

/// Copies `count` words from `from[at..]` to `into[to..]`.
fn copy(machine: &mut Machine, from: u64, at: u32, into: u64, to: u32, count: u32) {
    for word in 0..count {
        let held = machine.payload(from, at + word);
        machine.set_payload(into, to + word, held);
    }
}

// --- Set -------------------------------------------------------------------

/// `Set.of(items...) -> Set<T>`.
///
/// The element family comes from the first operand's `Repr`, which is a
/// static fact about the slot it came out of, so `Set.of()` says nothing
/// about what it is a set of and is refused rather than guessed at — the
/// reason [`super::seq`]'s `Vector.of` gives, and the same answer: the
/// lowering knows the layout the checker resolved and allocates the empty set
/// itself.
///
/// Each element is placed where it belongs as it arrives, so the run is
/// sorted at every step. A duplicate is refused rather than collapsed,
/// because a literal with the same element twice is a mistake and not an
/// intent — which also makes the length known before the first write: exactly
/// one word per operand.
pub(super) fn set_of(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let Some((elem, _)) = operands.first().copied() else {
        return Err(RuntimeError::new(
            "`Set.of()` with no elements does not say what it is a set of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty set where the element type is known"));
    };
    let layout = make::members(machine.program(), elem)?;
    let addr = machine.new_object(layout, operands.len() as u32)?;
    let mut len = 0;
    for element in operands {
        key::check(machine, "Set.of", key::SET_ELEMENT, *element)?;
        match seek(machine, elem, addr, 1, len, *element)? {
            Ok(_) => return Err(duplicate(machine, "Set.of", "element", *element)),
            Err(at) => {
                open(machine, addr, 1, at, len);
                machine.set_payload(addr, at, element.1);
                len += 1;
            }
        }
    }
    Ok(addr)
}

/// `Set.length() -> Int`.
pub(super) fn set_length(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(set(machine, "length", receiver)?.len as u64)
}

/// `Set.isEmpty() -> Bool`.
pub(super) fn set_is_empty(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((set(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Set.contains(element) -> Bool`.
pub(super) fn set_contains(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.contains", operands, 1)?;
    let items = set(machine, "contains", receiver)?;
    key::check(machine, "Set.contains", key::SET_ELEMENT, args[0])?;
    let found = seek(machine, items.elem, items.addr, 1, items.len, args[0])?;
    Ok(found.is_ok() as u64)
}

/// `Set.toArray() -> Array<T>`, in ascending order.
///
/// Which is the order the members are already in, so this copies rather than
/// sorts: the ascending order is how a set is stored, and `toArray()` is
/// where a program says it wants that order to be its own.
pub(super) fn set_to_array(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toArray", operands, 0)?;
    let items = set(machine, "toArray", receiver)?;
    let words: Vec<u64> = (0..items.len)
        .map(|at| machine.payload(items.addr, at))
        .collect();
    make::array_of(machine, items.elem, &words)
}

/// `Set.inserted(element) -> Set<T>`.
///
/// A new set. An element already there answers a copy and keeps the member
/// the set was holding rather than the one it was handed, which is what
/// `BTreeSet::insert` does with a member it finds — the two are equal, and
/// which object the set holds afterwards is not something a program can ask.
pub(super) fn set_inserted(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.inserted", operands, 1)?;
    let items = set(machine, "inserted", receiver)?;
    key::check(machine, "Set.inserted", key::SET_ELEMENT, args[0])?;
    let found = seek(machine, items.elem, items.addr, 1, items.len, args[0])?;
    let layout = machine.object_layout(items.addr);
    let len = match found {
        Ok(_) => items.len,
        Err(_) => items.len + 1,
    };
    let addr = machine.new_object(layout, len)?;
    match found {
        Ok(_) => copy(machine, items.addr, 0, addr, 0, items.len),
        Err(at) => {
            copy(machine, items.addr, 0, addr, 0, at);
            machine.set_payload(addr, at, args[0].1);
            copy(machine, items.addr, at, addr, at + 1, items.len - at);
        }
    }
    Ok(addr)
}

/// `Set.removed(element) -> Set<T>`.
///
/// A new set either way: an element that was not there answers a copy, as
/// `BTreeSet::remove` leaves a map it did not find anything in.
pub(super) fn set_removed(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.removed", operands, 1)?;
    let items = set(machine, "removed", receiver)?;
    key::check(machine, "Set.removed", key::SET_ELEMENT, args[0])?;
    let found = seek(machine, items.elem, items.addr, 1, items.len, args[0])?;
    let layout = machine.object_layout(items.addr);
    let len = match found {
        Ok(_) => items.len - 1,
        Err(_) => items.len,
    };
    let addr = machine.new_object(layout, len)?;
    match found {
        Ok(at) => {
            copy(machine, items.addr, 0, addr, 0, at);
            copy(machine, items.addr, at + 1, addr, at, items.len - at - 1);
        }
        Err(_) => copy(machine, items.addr, 0, addr, 0, items.len),
    }
    Ok(addr)
}

// --- Map -------------------------------------------------------------------

/// `Map.of(entries...) -> Map<K, V>`.
///
/// The operands are the `MapEntry(key:, value:)` values the literal built, and
/// the two families come from the first one's layout — a static fact about the
/// object the lowering allocated, and the same fact for every entry, because
/// the checker has already settled that a map literal has one key type and one
/// value type.
///
/// A duplicate key is refused for the reason a duplicate element is: keeping
/// the first or the last silently would be resolving a mistake rather than
/// reporting it.
pub(super) fn map_of(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let Some(first) = operands.first().copied() else {
        return Err(RuntimeError::new(
            "`Map.of()` with no entries does not say what it is a map of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty map where the key and value types are known"));
    };
    let (key_repr, value_repr) = entry_of(machine, first)?;
    let layout = make::entries(machine.program(), key_repr, value_repr)?;
    let addr = machine.new_object(layout, operands.len() as u32)?;
    let mut len = 0;
    for entry in operands {
        entry_of(machine, *entry)?;
        let (held, value) = (machine.payload(entry.1, 0), machine.payload(entry.1, 1));
        let held = (key_repr, held);
        key::check(machine, "Map.of", key::MAP_KEY, held)?;
        match seek(machine, key_repr, addr, 2, len, held)? {
            Ok(_) => return Err(duplicate(machine, "Map.of", "key", held)),
            Err(at) => {
                open(machine, addr, 2, at, len);
                machine.set_payload(addr, at * 2, held.1);
                machine.set_payload(addr, at * 2 + 1, value);
                len += 1;
            }
        }
    }
    Ok(addr)
}

/// The key and value families of one `MapEntry` operand.
///
/// A `MapEntry` is the builtin struct `MapEntry(key:, value:)`, recognised by
/// its layout's name and its two fields — the same reading
/// [`crate::lvm::boundary::is_range`] makes of a `Range`, and sound for the
/// same reason: the name is the checker's and a module cannot redeclare it.
fn entry_of(machine: &Machine, operand: Operand) -> Result<(Repr, Repr), RuntimeError> {
    let (repr, addr) = operand;
    if repr == Repr::Ref && addr != 0 {
        let layout = machine.program().layout(machine.object_layout(addr));
        if let Shape::Struct { fields, .. } = &layout.shape {
            if &*layout.name == MAP_ENTRY.name
                && fields.len() == 2
                && &*fields[0].name == MAP_ENTRY.fields[0].name
                && &*fields[1].name == MAP_ENTRY.fields[1].name
            {
                return Ok((fields[0].repr, fields[1].repr));
            }
        }
    }
    Err(RuntimeError::new(format!(
        "`Map.of` expects `MapEntry` values, but found `{}`",
        operand::type_name(machine, repr, addr)
    ))
    .with_rule(
        "`Map.of(entries: MapEntry<K, V>...)` takes values built with `MapEntry(key:, value:)`.",
    ))
}

/// `Map.get(key) -> Option<V>`.
pub(super) fn map_get(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.get", operands, 1)?;
    let entries = map(machine, "get", receiver)?;
    key::check(machine, "Map.get", key::MAP_KEY, args[0])?;
    match seek(machine, entries.key, entries.addr, 2, entries.len, args[0])? {
        Ok(at) => {
            let word = machine.payload(entries.addr, at * 2 + 1);
            make::some(machine, entries.value, word)
        }
        Err(_) => make::none(machine, entries.value),
    }
}

/// `Map.contains(key) -> Bool`.
pub(super) fn map_contains(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.contains", operands, 1)?;
    let entries = map(machine, "contains", receiver)?;
    key::check(machine, "Map.contains", key::MAP_KEY, args[0])?;
    let found = seek(machine, entries.key, entries.addr, 2, entries.len, args[0])?;
    Ok(found.is_ok() as u64)
}

/// `Map.length() -> Int`.
pub(super) fn map_length(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(map(machine, "length", receiver)?.len as u64)
}

/// `Map.isEmpty() -> Bool`.
pub(super) fn map_is_empty(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((map(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Map.keys() -> Array<K>`, in ascending order.
pub(super) fn map_keys(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("keys", operands, 0)?;
    let entries = map(machine, "keys", receiver)?;
    let words: Vec<u64> = (0..entries.len)
        .map(|at| machine.payload(entries.addr, at * 2))
        .collect();
    make::array_of(machine, entries.key, &words)
}

/// `Map.values() -> Array<V>`, in ascending order of their keys.
pub(super) fn map_values(machine: &mut Machine, operands: &[Operand]) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("values", operands, 0)?;
    let entries = map(machine, "values", receiver)?;
    let words: Vec<u64> = (0..entries.len)
        .map(|at| machine.payload(entries.addr, at * 2 + 1))
        .collect();
    make::array_of(machine, entries.value, &words)
}

/// `Map.inserted(key, value) -> Map<K, V>`.
///
/// A key already there keeps the key the map was holding and takes the new
/// value, which is what `BTreeMap::insert` does: the two keys are equal, and
/// the entry a program reads back is the same entry either way.
pub(super) fn map_inserted(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.inserted", operands, 2)?;
    let entries = map(machine, "inserted", receiver)?;
    key::check(machine, "Map.inserted", key::MAP_KEY, args[0])?;
    let found = seek(machine, entries.key, entries.addr, 2, entries.len, args[0])?;
    let layout = machine.object_layout(entries.addr);
    let len = match found {
        Ok(_) => entries.len,
        Err(_) => entries.len + 1,
    };
    let addr = machine.new_object(layout, len)?;
    match found {
        Ok(at) => {
            copy(machine, entries.addr, 0, addr, 0, entries.len * 2);
            machine.set_payload(addr, at * 2 + 1, args[1].1);
        }
        Err(at) => {
            copy(machine, entries.addr, 0, addr, 0, at * 2);
            machine.set_payload(addr, at * 2, args[0].1);
            machine.set_payload(addr, at * 2 + 1, args[1].1);
            copy(
                machine,
                entries.addr,
                at * 2,
                addr,
                at * 2 + 2,
                (entries.len - at) * 2,
            );
        }
    }
    Ok(addr)
}

/// `Map.removed(key) -> Map<K, V>`.
pub(super) fn map_removed(
    machine: &mut Machine,
    operands: &[Operand],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.removed", operands, 1)?;
    let entries = map(machine, "removed", receiver)?;
    key::check(machine, "Map.removed", key::MAP_KEY, args[0])?;
    let found = seek(machine, entries.key, entries.addr, 2, entries.len, args[0])?;
    let layout = machine.object_layout(entries.addr);
    let len = match found {
        Ok(_) => entries.len - 1,
        Err(_) => entries.len,
    };
    let addr = machine.new_object(layout, len)?;
    match found {
        Ok(at) => {
            copy(machine, entries.addr, 0, addr, 0, at * 2);
            copy(
                machine,
                entries.addr,
                at * 2 + 2,
                addr,
                at * 2,
                (entries.len - at - 1) * 2,
            );
        }
        Err(_) => copy(machine, entries.addr, 0, addr, 0, entries.len * 2),
    }
    Ok(addr)
}

// --- refusals --------------------------------------------------------------

/// `` `{method}` was given the {role} `{key}` more than once ``.
///
/// [`crate::builtins`]' `duplicate_key_error`, over the key as it renders —
/// which is what `MapKey`'s `Display` is on that side, and why the rendering
/// is what names it here.
fn duplicate(machine: &Machine, method: &str, role: &str, key: Operand) -> RuntimeError {
    match render(machine, key.0, key.1, 0) {
        Ok(shown) => RuntimeError::new(format!(
            "`{method}` was given the {role} `{shown}` more than once"
        ))
        .with_rule(
            "A literal with two identical keys is a mistake, not an intent; duplicate keys are rejected rather than silently resolved by keeping the last one.",
        )
        .with_help(format!("remove the duplicate, or give it a different {role}")),
        // A key this run cannot render is a key it cannot name, and the
        // rendering's own refusal says more than a message with a hole in it.
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::builtins::tests::{case_of, named, read, run, words_of, world};
    use cove_lir::LayoutId;

    fn machine(program: &cove_lir::Program) -> Machine<'_> {
        Machine::new(program, 1 << 14)
    }

    /// A `Set` holding `words`, which the caller writes in ascending order —
    /// by hand, so that what is under test is the operation and not whatever
    /// built the fixture.
    fn members(machine: &mut Machine, elem: Repr, words: &[u64]) -> u64 {
        let layout = make::members(machine.program(), elem).expect("the fixture declares a `Set`");
        let addr = machine
            .new_object(layout, words.len() as u32)
            .expect("the fixture's heap is large enough");
        for (at, word) in words.iter().enumerate() {
            machine.set_payload(addr, at as u32, *word);
        }
        addr
    }

    /// A `Map` holding `pairs`, in ascending key order.
    fn entries(machine: &mut Machine, key: Repr, value: Repr, pairs: &[(u64, u64)]) -> u64 {
        let layout =
            make::entries(machine.program(), key, value).expect("the fixture declares a `Map`");
        let addr = machine
            .new_object(layout, pairs.len() as u32)
            .expect("the fixture's heap is large enough");
        for (at, (k, v)) in pairs.iter().enumerate() {
            machine.set_payload(addr, at as u32 * 2, *k);
            machine.set_payload(addr, at as u32 * 2 + 1, *v);
        }
        addr
    }

    /// A `MapEntry(key:, value:)` of two `Int`s.
    fn entry(machine: &mut Machine, key: u64, value: u64) -> u64 {
        let layout = named(machine.program(), "MapEntry");
        let addr = machine.new_object(layout, 0).unwrap();
        machine.set_payload(addr, 0, key);
        machine.set_payload(addr, 1, value);
        addr
    }

    /// The words of a `Set`, or the key-then-value words of a `Map`.
    fn held(machine: &Machine, addr: u64, stride: u32) -> Vec<u64> {
        (0..machine.object_len(addr) * stride)
            .map(|at| machine.payload(addr, at))
            .collect()
    }

    /// The elements arrive in whatever order the program wrote them and the
    /// set is in ascending order regardless, because each one is placed where
    /// it belongs as it arrives.
    #[test]
    fn a_set_is_built_sorted_whatever_order_it_was_written_in() {
        let program = world();
        let mut machine = machine(&program);
        let addr = run(
            &mut machine,
            "Set",
            "of",
            &[(Repr::Int, 3), (Repr::Int, 1), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(held(&machine, addr, 1), vec![1, 2, 3]);
        assert_eq!(machine.object_len(addr), 3);
    }

    /// A duplicate is a mistake and not an intent, so it stops the run rather
    /// than collapsing — and the message names the element as it renders.
    #[test]
    fn a_set_refuses_the_same_element_twice() {
        let program = world();
        let mut machine = machine(&program);
        let error = run(
            &mut machine,
            "Set",
            "of",
            &[(Repr::Int, 1), (Repr::Int, 2), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` was given the element `1` more than once"
        );

        // Equal by value rather than by word: two string objects holding the
        // same bytes are one element.
        let a = machine.new_string("x").unwrap();
        let b = machine.new_string("x").unwrap();
        assert_ne!(a, b);
        let error = run(&mut machine, "Set", "of", &[(Repr::Ref, a), (Repr::Ref, b)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` was given the element `x` more than once"
        );
    }

    /// An empty literal says nothing about what it holds, and a set's element
    /// `Repr` is what the collector traces its words by, so it is refused
    /// rather than guessed at.
    #[test]
    fn an_empty_literal_is_refused_for_saying_nothing() {
        let program = world();
        let mut machine = machine(&program);
        let error = run(&mut machine, "Set", "of", &[]).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of()` with no elements does not say what it is a set of"
        );
        let error = run(&mut machine, "Map", "of", &[]).unwrap_err();
        assert_eq!(
            error.message,
            "`Map.of()` with no entries does not say what it is a map of"
        );
    }

    #[test]
    fn a_set_reports_what_it_holds() {
        let program = world();
        let mut machine = machine(&program);
        let items = members(&mut machine, Repr::Int, &[1, 2, 3]);
        let empty = members(&mut machine, Repr::Int, &[]);
        assert_eq!(
            run(&mut machine, "Set", "length", &[(Repr::Ref, items)]).unwrap(),
            3
        );
        assert_eq!(
            run(&mut machine, "Set", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );
        assert_eq!(
            run(&mut machine, "Set", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
            1
        );

        let array = run(&mut machine, "Set", "toArray", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(words_of(&machine, array), vec![1, 2, 3]);
    }

    /// Membership is the binary search the sorted run is for, and it answers
    /// the same thing at both ends and in the middle.
    #[test]
    fn a_set_answers_membership_by_searching() {
        let program = world();
        let mut machine = machine(&program);
        let items = members(&mut machine, Repr::Int, &[1, 3, 5, 7]);
        for (element, expected) in [(1, 1), (3, 1), (5, 1), (7, 1), (0, 0), (4, 0), (9, 0)] {
            assert_eq!(
                run(
                    &mut machine,
                    "Set",
                    "contains",
                    &[(Repr::Ref, items), (Repr::Int, element)]
                )
                .unwrap(),
                expected,
                "contains({element})"
            );
        }
    }

    /// A new set every time, sorted, and the receiver untouched — which is
    /// what an immutable value has to be.
    #[test]
    fn inserting_and_removing_answer_new_sets() {
        let program = world();
        let mut machine = machine(&program);
        let items = members(&mut machine, Repr::Int, &[1, 3]);

        let with = run(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 2)],
        )
        .unwrap();
        assert_ne!(with, items);
        assert_eq!(held(&machine, with, 1), vec![1, 2, 3]);
        assert_eq!(
            held(&machine, items, 1),
            vec![1, 3],
            "the receiver is not written"
        );

        // At either end, and an element already there answers a copy of the
        // same length.
        let low = run(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 0)],
        )
        .unwrap();
        assert_eq!(held(&machine, low, 1), vec![0, 1, 3]);
        let high = run(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 9)],
        )
        .unwrap();
        assert_eq!(held(&machine, high, 1), vec![1, 3, 9]);
        let again = run(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(held(&machine, again, 1), vec![1, 3]);

        let without = run(
            &mut machine,
            "Set",
            "removed",
            &[(Repr::Ref, items), (Repr::Int, 1)],
        )
        .unwrap();
        assert_eq!(held(&machine, without, 1), vec![3]);
        // An element that was not there answers a copy.
        let same = run(
            &mut machine,
            "Set",
            "removed",
            &[(Repr::Ref, items), (Repr::Int, 2)],
        )
        .unwrap();
        assert_ne!(same, items);
        assert_eq!(held(&machine, same, 1), vec![1, 3]);
    }

    #[test]
    fn a_map_is_built_sorted_from_its_entries() {
        let program = world();
        let mut machine = machine(&program);
        let (a, b, c) = (
            entry(&mut machine, 3, 30),
            entry(&mut machine, 1, 10),
            entry(&mut machine, 2, 20),
        );
        let addr = run(
            &mut machine,
            "Map",
            "of",
            &[(Repr::Ref, a), (Repr::Ref, b), (Repr::Ref, c)],
        )
        .unwrap();
        assert_eq!(held(&machine, addr, 2), vec![1, 10, 2, 20, 3, 30]);
        assert_eq!(machine.object_len(addr), 3);
    }

    #[test]
    fn a_map_refuses_the_same_key_twice_and_anything_that_is_not_an_entry() {
        let program = world();
        let mut machine = machine(&program);
        let (a, b) = (entry(&mut machine, 1, 10), entry(&mut machine, 1, 20));
        let error = run(&mut machine, "Map", "of", &[(Repr::Ref, a), (Repr::Ref, b)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Map.of` was given the key `1` more than once"
        );

        let text = machine.new_string("x").unwrap();
        let error = run(&mut machine, "Map", "of", &[(Repr::Ref, text)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Map.of` expects `MapEntry` values, but found `String`"
        );
    }

    #[test]
    fn a_map_answers_what_it_holds() {
        let program = world();
        let mut machine = machine(&program);
        let held_map = entries(&mut machine, Repr::Int, Repr::Int, &[(1, 10), (2, 20)]);
        let empty = entries(&mut machine, Repr::Int, Repr::Int, &[]);

        assert_eq!(
            run(&mut machine, "Map", "length", &[(Repr::Ref, held_map)]).unwrap(),
            2
        );
        assert_eq!(
            run(&mut machine, "Map", "isEmpty", &[(Repr::Ref, held_map)]).unwrap(),
            0
        );
        assert_eq!(
            run(&mut machine, "Map", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
            1
        );
        assert_eq!(
            run(
                &mut machine,
                "Map",
                "contains",
                &[(Repr::Ref, held_map), (Repr::Int, 2)]
            )
            .unwrap(),
            1
        );
        assert_eq!(
            run(
                &mut machine,
                "Map",
                "contains",
                &[(Repr::Ref, held_map), (Repr::Int, 3)]
            )
            .unwrap(),
            0
        );

        let keys = run(&mut machine, "Map", "keys", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(words_of(&machine, keys), vec![1, 2]);
        let values = run(&mut machine, "Map", "values", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(words_of(&machine, values), vec![10, 20]);
    }

    /// `get` answers an `Option` of the value family, which is where a
    /// missing key and a present one are one answer rather than two.
    #[test]
    fn a_map_get_answers_an_option() {
        let program = world();
        let mut machine = machine(&program);
        let held_map = entries(&mut machine, Repr::Int, Repr::Int, &[(1, 10), (2, 20)]);
        let found = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, found), ("Some".to_string(), vec![20]));
        let missing = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, missing), ("None".to_string(), vec![]));
    }

    #[test]
    fn inserting_and_removing_answer_new_maps() {
        let program = world();
        let mut machine = machine(&program);
        let held_map = entries(&mut machine, Repr::Int, Repr::Int, &[(1, 10), (3, 30)]);

        let with = run(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Int, 2), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(held(&machine, with, 2), vec![1, 10, 2, 20, 3, 30]);
        assert_eq!(held(&machine, held_map, 2), vec![1, 10, 3, 30]);

        // A key already there keeps its place and takes the new value.
        let over = run(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Int, 3), (Repr::Int, 99)],
        )
        .unwrap();
        assert_eq!(held(&machine, over, 2), vec![1, 10, 3, 99]);

        let without = run(
            &mut machine,
            "Map",
            "removed",
            &[(Repr::Ref, held_map), (Repr::Int, 1)],
        )
        .unwrap();
        assert_eq!(held(&machine, without, 2), vec![3, 30]);
        let same = run(
            &mut machine,
            "Map",
            "removed",
            &[(Repr::Ref, held_map), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(held(&machine, same, 2), vec![1, 10, 3, 30]);
    }

    /// A key the language does not admit stops the operation before anything
    /// is searched, in the words the oracle refuses it in — an empty map as
    /// loudly as a full one.
    #[test]
    fn an_argument_that_cannot_be_a_key_is_refused_before_the_search() {
        let program = world();
        let mut machine = machine(&program);
        let empty = entries(&mut machine, Repr::Int, Repr::Int, &[]);
        let error = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, empty), (Repr::Float, 1.5f64.to_bits())],
        )
        .unwrap_err();
        assert_eq!(error.message, "`Map.get` cannot use a `Float` as a map key");

        let items = members(&mut machine, Repr::Int, &[]);
        let error = run(
            &mut machine,
            "Set",
            "contains",
            &[(Repr::Ref, items), (Repr::Float, 1.5f64.to_bits())],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Set.contains` cannot use a `Float` as a set element"
        );
    }

    /// A receiver of the wrong family is told it has no such method, which is
    /// where the oracle's `match` on the receiver's representation ends up.
    #[test]
    fn a_receiver_of_the_wrong_family_has_no_such_method() {
        let program = world();
        let mut machine = machine(&program);
        let items = members(&mut machine, Repr::Int, &[1]);
        let error = run(&mut machine, "Map", "length", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(error.message, "`Set` has no method `length`");

        let text = machine.new_string("x").unwrap();
        let error = run(&mut machine, "Set", "toArray", &[(Repr::Ref, text)]).unwrap_err();
        assert_eq!(error.message, "`String` has no method `toArray`");
    }

    /// The arity a method takes is the schema's, and the message is the
    /// oracle's — counted in arguments, which is operands less the receiver.
    #[test]
    fn an_operation_holds_its_arguments_to_the_count_it_takes() {
        let program = world();
        let mut machine = machine(&program);
        let items = members(&mut machine, Repr::Int, &[1]);
        let error = run(&mut machine, "Set", "contains", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.contains` takes 1 argument(s), but 0 were given"
        );

        let held_map = entries(&mut machine, Repr::Int, Repr::Int, &[(1, 10)]);
        let error = run(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Map.inserted` takes 2 argument(s), but 1 were given"
        );
    }

    /// A key that is a reference is searched by value, so a lookup finds an
    /// entry a different object with the same bytes put there.
    #[test]
    fn a_reference_key_is_found_by_what_it_is_and_not_by_where_it_is() {
        let program = world();
        let mut machine = machine(&program);
        let a = machine.new_string("a").unwrap();
        let b = machine.new_string("b").unwrap();
        let held_map = entries(&mut machine, Repr::Ref, Repr::Int, &[(a, 1), (b, 2)]);
        let wanted = machine.new_string("b").unwrap();
        assert_ne!(wanted, b);
        let found = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Ref, wanted)],
        )
        .unwrap();
        assert_eq!(case_of(&machine, found), ("Some".to_string(), vec![2]));

        // And the key the map keeps is the one it already held.
        let over = run(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Ref, wanted), (Repr::Int, 9)],
        )
        .unwrap();
        assert_eq!(machine.payload(over, 2), b);
        assert_eq!(machine.payload(over, 3), 9);
        assert_eq!(read(&machine, machine.payload(over, 2)), "b");
    }

    /// A set of references is a set of what they point at, so it sorts by the
    /// strings rather than by the addresses they happen to have.
    #[test]
    fn a_set_of_references_sorts_by_the_values() {
        let program = world();
        let mut machine = machine(&program);
        let (c, a, b) = (
            machine.new_string("c").unwrap(),
            machine.new_string("a").unwrap(),
            machine.new_string("b").unwrap(),
        );
        let addr = run(
            &mut machine,
            "Set",
            "of",
            &[(Repr::Ref, c), (Repr::Ref, a), (Repr::Ref, b)],
        )
        .unwrap();
        let shown: Vec<String> = held(&machine, addr, 1)
            .into_iter()
            .map(|word| read(&machine, word))
            .collect();
        assert_eq!(shown, vec!["a", "b", "c"]);
        assert_ne!(machine.object_layout(addr), LayoutId::FREE);
    }
}
