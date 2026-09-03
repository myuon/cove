//! `Set` and `Map`.
//!
//! Both are sorted runs. A `Set` is a run of members and a `Map` a run of
//! entries — key words then value words — each in the ascending order
//! [`super::key`] defines. The order is part of the value — the language says
//! a set iterates and renders in ascending order — so it is kept rather than
//! recovered, and every lookup is a binary search over it.
//!
//! # A member is as wide as its layout says
//!
//! One value may occupy several consecutive words, so a member is a *run* at
//! the element layout's width and an entry is a run at the key's width plus
//! the value's: a `Set<Point>` is two words per member and a
//! `Map<String, Point>` three per entry. The header's `len` still counts
//! members and entries rather than words, so every offset below is an index
//! times a stride while every length stays a count. Getting those two the same
//! way round is the whole of what changed when a value stopped being one word.
//!
//! # An argument is a member, at whatever width one is
//!
//! An argument names a value location and carries its layout, so a whole
//! member arrives as a whole member and `Set.inserted` on a `Set<Point>` is
//! handed both words. Reading one was never in doubt — the receiver's own
//! layout says how wide one is — and *putting one in* used to be: an operand
//! was one word, so those were refused rather than truncated.
//!
//! What is left is [`operand::run_of`], which holds an incoming member or
//! value to the receiver's own layout. A sorted run is traced by that
//! layout's reference map and searched at its width, so a value of another
//! family written into one would be both a collection following the wrong
//! words and a search comparing the wrong ones.
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

use cove_ir::{LayoutId, Repr, Shape};
use cove_schema::builtins::MAP_ENTRY;

use crate::error::RuntimeError;
use crate::vm::builtins::operand::{self, Operand};
use crate::vm::builtins::{key, make, render_value};
use crate::vm::exec::Machine;

// --- reading a receiver ----------------------------------------------------

/// A `Set`'s members: what family they belong to, how wide one is, how many
/// there are, and where.
struct Members {
    elem: LayoutId,
    /// The words one member occupies, which is the stride the run is walked
    /// at.
    width: u32,
    len: u32,
    addr: u64,
}

fn set(machine: &Machine, method: &str, receiver: Operand<'_>) -> Result<Members, RuntimeError> {
    let Some((Repr::Ref, addr)) = operand::as_word(machine, receiver) else {
        return Err(operand::no_method(machine, receiver, method));
    };
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Members { elem } => Ok(Members {
            elem,
            width: machine.words_of(elem),
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// A `Map`'s entries: the two families a pair of runs is read as, how wide
/// each is, how many entries there are, and where.
struct Entries {
    key: LayoutId,
    value: LayoutId,
    /// The words one key occupies, which is also where its value begins.
    keys: u32,
    values: u32,
    len: u32,
    addr: u64,
}

impl Entries {
    /// The words one entry occupies: the key's, then the value's.
    fn stride(&self) -> u32 {
        self.keys + self.values
    }

    /// The words of entry `at`'s key.
    fn key_words(&self, machine: &Machine, at: u32) -> Vec<u64> {
        machine.payload_run(self.addr, at * self.stride(), self.keys)
    }

    /// The words of entry `at`'s value, which begin at the key's width.
    fn value_words(&self, machine: &Machine, at: u32) -> Vec<u64> {
        machine.payload_run(self.addr, at * self.stride() + self.keys, self.values)
    }
}

fn map(machine: &Machine, method: &str, receiver: Operand<'_>) -> Result<Entries, RuntimeError> {
    let Some((Repr::Ref, addr)) = operand::as_word(machine, receiver) else {
        return Err(operand::no_method(machine, receiver, method));
    };
    if addr == 0 {
        return Err(operand::null_value());
    }
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Entries { key, value } => Ok(Entries {
            key,
            value,
            keys: machine.words_of(key),
            values: machine.words_of(value),
            len: machine.object_len(addr),
            addr,
        }),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// The layout of the family an operand belongs to.
///
/// The argument's own, which is what a call carries now. It used to be read
/// back out of the word — an object's header for a reference, a search of the
/// one-word layouts for a scalar — because that was the only place the answer
/// existed, and that reading could not describe an inline value at all.
fn family_of(machine: &Machine, operand: Operand<'_>) -> Result<LayoutId, RuntimeError> {
    if matches!(operand::as_word(machine, operand), Some((Repr::Ref, 0))) {
        return Err(operand::null_value());
    }
    Ok(operand.layout)
}

// --- searching and building a sorted run -----------------------------------

/// Where the value `order` is asked about is in the sorted run at `addr`, or
/// where it would go.
///
/// `Ok(at)` and `Err(at)` mean what [`slice::binary_search`] means by them,
/// and the search is written out rather than borrowed from it because the
/// comparison is fallible: a key that nests too deeply stops the run instead
/// of answering an order.
///
/// `stride` is how many words one element occupies and `width` how many of
/// them the key is — the same number for a set's member, and the key's width
/// for a map's entry, whose key is always the first of its words. `order` is
/// handed that run and answers where it sorts relative to what is being looked
/// for, which is what lets one search serve a wanted value that arrived as an
/// operand and one that arrived as words.
fn seek(
    machine: &Machine,
    addr: u64,
    stride: u32,
    width: u32,
    len: u32,
    order: impl Fn(&[u64]) -> Result<Ordering, RuntimeError>,
) -> Result<Result<u32, u32>, RuntimeError> {
    let (mut low, mut high) = (0, len);
    while low < high {
        let at = low + (high - low) / 2;
        let held = machine.payload_run(addr, at * stride, width);
        match order(&held)? {
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
/// Backwards, so that no word is overwritten before it has been read. The
/// bounds are in words and the counts in elements, which is what `stride`
/// converts between.
fn open(machine: &mut Machine, addr: u64, stride: u32, at: u32, len: u32) {
    for word in (at * stride..len * stride).rev() {
        let held = machine.payload(addr, word);
        machine.set_payload(addr, word + stride, held);
    }
}

/// Copies `count` words from `from[at..]` to `into[to..]`.
///
/// Words rather than elements: every caller has a stride in hand and
/// multiplying at the call site is what keeps this from having to know which
/// of the two shapes it is copying.
fn copy(machine: &mut Machine, from: u64, at: u32, into: u64, to: u32, count: u32) {
    for word in 0..count {
        let held = machine.payload(from, at + word);
        machine.set_payload(into, to + word, held);
    }
}

// --- Set -------------------------------------------------------------------

/// `Set.of(items...) -> Set<T>`.
///
/// The element family comes from the first operand — its object's own header,
/// or the one-word layout its `Repr` names — so `Set.of()` says nothing about
/// what it is a set of and is refused rather than guessed at. That is the
/// reason [`super::seq`]'s `Vector.of` gives, and the same answer: the
/// lowering knows the layout the checker resolved and allocates the empty set
/// itself.
///
/// Each element is placed where it belongs as it arrives, so the run is
/// sorted at every step. A duplicate is refused rather than collapsed,
/// because a literal with the same element twice is a mistake and not an
/// intent — which also makes the length known before the first write: exactly
/// one member per operand.
pub(super) fn set_of(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let Some(first) = operands.first().copied() else {
        return Err(RuntimeError::new(
            "`Set.of()` with no elements does not say what it is a set of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty set where the element type is known"));
    };
    let elem = family_of(machine, first)?;
    let width = machine.words_of(elem);
    let layout = make::members(machine.program(), elem)?;
    let addr = machine.new_object(layout, operands.len() as u32)?;
    let mut len = 0;
    for element in operands {
        let words = operand::run_of(machine, "Set.of", elem, *element)?;
        key::check(machine, "Set.of", key::SET_ELEMENT, *element)?;
        let found = seek(machine, addr, width, width, len, |held| {
            key::cmp_value(machine, elem, held, words)
        })?;
        match found {
            Ok(_) => {
                return Err(duplicate(
                    "Set.of",
                    "element",
                    render_value(machine, elem, words, 0),
                ))
            }
            Err(at) => {
                open(machine, addr, width, at, len);
                machine.set_payload_run(addr, at * width, words);
                len += 1;
            }
        }
    }
    Ok(addr)
}

/// `Set.length() -> Int`.
pub(super) fn set_length(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(set(machine, "length", receiver)?.len as u64)
}

/// `Set.isEmpty() -> Bool`.
pub(super) fn set_is_empty(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((set(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Set.contains(element) -> Bool`.
pub(super) fn set_contains(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.contains", operands, 1)?;
    let items = set(machine, "contains", receiver)?;
    key::check(machine, "Set.contains", key::SET_ELEMENT, args[0])?;
    let found = seek(
        machine,
        items.addr,
        items.width,
        items.width,
        items.len,
        |held| key::cmp_held(machine, items.elem, held, args[0]),
    )?;
    Ok(found.is_ok() as u64)
}

/// `Set.toArray() -> Array<T>`, in ascending order.
///
/// Which is the order the members are already in, so this copies rather than
/// sorts: the ascending order is how a set is stored, and `toArray()` is
/// where a program says it wants that order to be its own. The copy is one
/// run of `len * width` words, because an array of two-word elements holds
/// them exactly as the set did.
pub(super) fn set_to_array(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("toArray", operands, 0)?;
    let items = set(machine, "toArray", receiver)?;
    let words = machine.payload_run(items.addr, 0, items.len * items.width);
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
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.inserted", operands, 1)?;
    let items = set(machine, "inserted", receiver)?;
    key::check(machine, "Set.inserted", key::SET_ELEMENT, args[0])?;
    let element = operand::run_of(machine, "Set.inserted", items.elem, args[0])?.to_vec();
    let found = seek(
        machine,
        items.addr,
        items.width,
        items.width,
        items.len,
        |held| key::cmp_held(machine, items.elem, held, args[0]),
    )?;
    let layout = machine.object_layout(items.addr);
    let len = match found {
        Ok(_) => items.len,
        Err(_) => items.len + 1,
    };
    let addr = machine.new_object(layout, len)?;
    let stride = items.width;
    match found {
        Ok(_) => copy(machine, items.addr, 0, addr, 0, items.len * stride),
        Err(at) => {
            copy(machine, items.addr, 0, addr, 0, at * stride);
            machine.set_payload_run(addr, at * stride, &element);
            copy(
                machine,
                items.addr,
                at * stride,
                addr,
                (at + 1) * stride,
                (items.len - at) * stride,
            );
        }
    }
    Ok(addr)
}

/// `Set.removed(element) -> Set<T>`.
///
/// A new set either way: an element that was not there answers a copy, as
/// `BTreeSet::remove` leaves a map it did not find anything in. Nothing is
/// put in, so a member wider than an operand is only searched for and the
/// search answers what it can.
pub(super) fn set_removed(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Set.removed", operands, 1)?;
    let items = set(machine, "removed", receiver)?;
    key::check(machine, "Set.removed", key::SET_ELEMENT, args[0])?;
    let found = seek(
        machine,
        items.addr,
        items.width,
        items.width,
        items.len,
        |held| key::cmp_held(machine, items.elem, held, args[0]),
    )?;
    let layout = machine.object_layout(items.addr);
    let len = match found {
        Ok(_) => items.len - 1,
        Err(_) => items.len,
    };
    let addr = machine.new_object(layout, len)?;
    let stride = items.width;
    match found {
        Ok(at) => {
            copy(machine, items.addr, 0, addr, 0, at * stride);
            copy(
                machine,
                items.addr,
                (at + 1) * stride,
                addr,
                at * stride,
                (items.len - at - 1) * stride,
            );
        }
        Err(_) => copy(machine, items.addr, 0, addr, 0, items.len * stride),
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
///
/// A duplicate key is refused for the reason a duplicate element is: keeping
/// the first or the last silently would be resolving a mistake rather than
/// reporting it.
pub(super) fn map_of(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let Some(operand) = operands.first().copied() else {
        return Err(RuntimeError::new(
            "`Map.of()` with no entries does not say what it is a map of",
        )
        .with_rule(
            "A layout describes a family of values, and a builtin is told which one by the values it is given.",
        )
        .with_help("allocate the empty map where the key and value types are known"));
    };
    let first = entry_of(machine, operand)?;
    let layout = make::entries(machine.program(), first.key, first.value)?;
    let keys = machine.words_of(first.key);
    let stride = keys + machine.words_of(first.value);
    let addr = machine.new_object(layout, operands.len() as u32)?;
    let mut len = 0;
    for operand in operands {
        let entry = entry_of(machine, *operand)?;
        if entry.key != first.key || entry.value != first.value {
            return Err(mixed(machine, &entry));
        }
        let held = operand.words[..keys as usize].to_vec();
        let value = operand.words[entry.at as usize..][..(stride - keys) as usize].to_vec();
        key::check_value(machine, "Map.of", key::MAP_KEY, first.key, &held)?;
        let found = seek(machine, addr, stride, keys, len, |stored| {
            key::cmp_value(machine, first.key, stored, &held)
        })?;
        match found {
            Ok(_) => {
                return Err(duplicate(
                    "Map.of",
                    "key",
                    render_value(machine, first.key, &held, 0),
                ))
            }
            Err(at) => {
                open(machine, addr, stride, at, len);
                machine.set_payload_run(addr, at * stride, &held);
                machine.set_payload_run(addr, at * stride + keys, &value);
                len += 1;
            }
        }
    }
    Ok(addr)
}

/// One `MapEntry` operand: the layouts of its two fields and where the
/// value's words begin in it.
struct Entry {
    key: LayoutId,
    value: LayoutId,
    /// Where the value's words begin, which the struct's own layout says
    /// rather than this deriving it from the key's width.
    at: u32,
}

/// The key and value families of one `MapEntry` operand.
///
/// A `MapEntry` is the builtin struct `MapEntry(key:, value:)`, recognised by
/// its layout's name and its two fields — the same reading
/// [`crate::vm::boundary::is_range`] makes of a `Range`, and sound for the
/// same reason: the name is the checker's and a module cannot redeclare it.
///
/// It is read out of the operand's own layout, because a struct is inline:
/// the entry a literal built *is* the words the argument names, and the pair
/// arrives whole for the same reason a `Point` member does.
fn entry_of(machine: &Machine, operand: Operand<'_>) -> Result<Entry, RuntimeError> {
    let layout = machine.program().layout(operand.layout);
    if let Shape::Struct { fields, .. } = &layout.shape {
        if &*layout.name == MAP_ENTRY.name
            && fields.len() == 2
            && &*fields[0].name == MAP_ENTRY.fields[0].name
            && &*fields[1].name == MAP_ENTRY.fields[1].name
            && operand.words.len() == layout.width() as usize
        {
            return Ok(Entry {
                key: fields[0].layout,
                value: fields[1].layout,
                at: fields[1].at,
            });
        }
    }
    Err(RuntimeError::new(format!(
        "`Map.of` expects `MapEntry` values, but found `{}`",
        operand::name(machine, operand)
    ))
    .with_rule(
        "`Map.of(entries: MapEntry<K, V>...)` takes values built with `MapEntry(key:, value:)`.",
    ))
}

/// `Map.of` was given entries of more than one family.
///
/// Unreachable from a checked program — `cove-sema` settled that a map
/// literal has one key type and one value type — and written out because the
/// run's stride comes from the *first* entry: an entry of another width would
/// be written across the one beside it rather than into its own.
fn mixed(machine: &Machine, entry: &Entry) -> RuntimeError {
    RuntimeError::new(format!(
        "`Map.of` was given an entry of `{}` to `{}` among entries of another kind",
        machine.program().layout(entry.key).name,
        machine.program().layout(entry.value).name
    ))
    .with_rule("A map literal has one key type and one value type.")
}

/// `Map.get(key) -> Option<V>`.
///
/// The answer is the `Option`'s words rather than an address: a fixed-size
/// enum is inline, so `Some(Point(1, 2))` is a run the caller writes into its
/// destination location the way a copy writes one.
pub(super) fn map_get(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    let (receiver, args) = operand::method("Map.get", operands, 1)?;
    let entries = map(machine, "get", receiver)?;
    key::check(machine, "Map.get", key::MAP_KEY, args[0])?;
    let found = seek(
        machine,
        entries.addr,
        entries.stride(),
        entries.keys,
        entries.len,
        |held| key::cmp_held(machine, entries.key, held, args[0]),
    )?;
    match found {
        Ok(at) => {
            let words = entries.value_words(machine, at);
            make::some(machine, entries.value, &words)
        }
        Err(_) => make::none(machine, entries.value),
    }
}

/// `Map.contains(key) -> Bool`.
pub(super) fn map_contains(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.contains", operands, 1)?;
    let entries = map(machine, "contains", receiver)?;
    key::check(machine, "Map.contains", key::MAP_KEY, args[0])?;
    let found = seek(
        machine,
        entries.addr,
        entries.stride(),
        entries.keys,
        entries.len,
        |held| key::cmp_held(machine, entries.key, held, args[0]),
    )?;
    Ok(found.is_ok() as u64)
}

/// `Map.length() -> Int`.
pub(super) fn map_length(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("length", operands, 0)?;
    Ok(map(machine, "length", receiver)?.len as u64)
}

/// `Map.isEmpty() -> Bool`.
pub(super) fn map_is_empty(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("isEmpty", operands, 0)?;
    Ok((map(machine, "isEmpty", receiver)?.len == 0) as u64)
}

/// `Map.keys() -> Array<K>`, in ascending order.
pub(super) fn map_keys(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("keys", operands, 0)?;
    let entries = map(machine, "keys", receiver)?;
    let mut words = Vec::with_capacity((entries.len * entries.keys) as usize);
    for at in 0..entries.len {
        words.extend_from_slice(&entries.key_words(machine, at));
    }
    make::array_of(machine, entries.key, &words)
}

/// `Map.values() -> Array<V>`, in ascending order of their keys.
pub(super) fn map_values(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, _) = operand::method("values", operands, 0)?;
    let entries = map(machine, "values", receiver)?;
    let mut words = Vec::with_capacity((entries.len * entries.values) as usize);
    for at in 0..entries.len {
        words.extend_from_slice(&entries.value_words(machine, at));
    }
    make::array_of(machine, entries.value, &words)
}

/// `Map.inserted(key, value) -> Map<K, V>`.
///
/// A key already there keeps the key the map was holding and takes the new
/// value, which is what `BTreeMap::insert` does: the two keys are equal, and
/// the entry a program reads back is the same entry either way.
pub(super) fn map_inserted(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.inserted", operands, 2)?;
    let entries = map(machine, "inserted", receiver)?;
    key::check(machine, "Map.inserted", key::MAP_KEY, args[0])?;
    let key = operand::run_of(machine, "Map.inserted", entries.key, args[0])?.to_vec();
    let value = operand::run_of(machine, "Map.inserted", entries.value, args[1])?.to_vec();
    let found = seek(
        machine,
        entries.addr,
        entries.stride(),
        entries.keys,
        entries.len,
        |held| key::cmp_held(machine, entries.key, held, args[0]),
    )?;
    let layout = machine.object_layout(entries.addr);
    let len = match found {
        Ok(_) => entries.len,
        Err(_) => entries.len + 1,
    };
    let addr = machine.new_object(layout, len)?;
    let stride = entries.stride();
    match found {
        Ok(at) => {
            copy(machine, entries.addr, 0, addr, 0, entries.len * stride);
            machine.set_payload_run(addr, at * stride + entries.keys, &value);
        }
        Err(at) => {
            copy(machine, entries.addr, 0, addr, 0, at * stride);
            machine.set_payload_run(addr, at * stride, &key);
            machine.set_payload_run(addr, at * stride + entries.keys, &value);
            copy(
                machine,
                entries.addr,
                at * stride,
                addr,
                (at + 1) * stride,
                (entries.len - at) * stride,
            );
        }
    }
    Ok(addr)
}

/// `Map.removed(key) -> Map<K, V>`.
pub(super) fn map_removed(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (receiver, args) = operand::method("Map.removed", operands, 1)?;
    let entries = map(machine, "removed", receiver)?;
    key::check(machine, "Map.removed", key::MAP_KEY, args[0])?;
    let found = seek(
        machine,
        entries.addr,
        entries.stride(),
        entries.keys,
        entries.len,
        |held| key::cmp_held(machine, entries.key, held, args[0]),
    )?;
    let layout = machine.object_layout(entries.addr);
    let len = match found {
        Ok(_) => entries.len - 1,
        Err(_) => entries.len,
    };
    let addr = machine.new_object(layout, len)?;
    let stride = entries.stride();
    match found {
        Ok(at) => {
            copy(machine, entries.addr, 0, addr, 0, at * stride);
            copy(
                machine,
                entries.addr,
                (at + 1) * stride,
                addr,
                at * stride,
                (entries.len - at - 1) * stride,
            );
        }
        Err(_) => copy(machine, entries.addr, 0, addr, 0, entries.len * stride),
    }
    Ok(addr)
}

// --- refusals --------------------------------------------------------------

/// `` `{method}` was given the {role} `{key}` more than once ``.
///
/// [`crate::builtins`]' `duplicate_key_error`, over the key as it renders —
/// which is what `MapKey`'s `Display` is on that side, and why the rendering
/// is what names it here. The caller does the rendering because a key that
/// arrived as an operand and one that arrived as a run of words are rendered
/// by two different readers.
fn duplicate(method: &str, role: &str, shown: Result<String, RuntimeError>) -> RuntimeError {
    match shown {
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
    use crate::vm::builtins::tests::{
        named, option_of, read, run, scalar, values, word, words_of, world,
    };
    use crate::vm::exec::tests::Build;
    use cove_ir::Program;

    fn machine(program: &Program) -> Machine<'_> {
        Machine::new(program, 1 << 14)
    }

    /// A `Set` holding `words`, which the caller writes in ascending order —
    /// by hand, so that what is under test is the operation and not whatever
    /// built the fixture.
    fn members(machine: &mut Machine, elem: LayoutId, words: &[u64]) -> u64 {
        let layout = make::members(machine.program(), elem).expect("the fixture declares a `Set`");
        let width = machine.words_of(elem).max(1);
        let addr = machine
            .new_object(layout, words.len() as u32 / width)
            .expect("the fixture's heap is large enough");
        machine.set_payload_run(addr, 0, words);
        addr
    }

    /// A `Map` holding `pairs`, in ascending key order. One word each side,
    /// which every fixture here but the wide one uses.
    fn entries(machine: &mut Machine, key: LayoutId, value: LayoutId, pairs: &[(u64, u64)]) -> u64 {
        let layout =
            make::entries(machine.program(), key, value).expect("the fixture declares a `Map`");
        let addr = machine
            .new_object(layout, pairs.len() as u32)
            .expect("the fixture's heap is large enough");
        for (at, (k, v)) in pairs.iter().enumerate() {
            machine.set_payload_run(addr, at as u32 * 2, &[*k, *v]);
        }
        addr
    }

    /// A `MapEntry(key:, value:)` of two `Int`s, as the operand one is.
    ///
    /// A struct is inline, so an entry is the run of words its two fields
    /// occupy — the same thing a `Map.of` literal's lowering would leave in
    /// the caller's frame.
    fn entry(program: &Program, key: u64, value: u64) -> (LayoutId, [u64; 2]) {
        (named(program, "MapEntry"), [key, value])
    }

    /// The words of a run, at the stride its elements are kept at.
    fn held(machine: &Machine, addr: u64, stride: u32) -> Vec<u64> {
        machine.payload_run(addr, 0, machine.object_len(addr) * stride)
    }

    /// A world with a two-word family in it, which the shared fixture has no
    /// `Set` or `Map` of: a `Set<Point>` is a run of two-word members and a
    /// `Map<Int, Point>` a run of three-word entries.
    fn wide() -> Program {
        let mut build = Build::default();
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;
        let int = build.word("Int", Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        build.layout(
            "Array",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        build.layout(
            "Array",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        build.layout("Set", Shape::Members { elem: point });
        build.layout(
            "Map",
            Shape::Entries {
                key: int,
                value: point,
            },
        );
        build.enumeration("Option", &[("None", vec![]), ("Some", vec![point])]);
        build.done()
    }

    /// The elements arrive in whatever order the program wrote them and the
    /// set is in ascending order regardless, because each one is placed where
    /// it belongs as it arrives.
    #[test]
    fn a_set_is_built_sorted_whatever_order_it_was_written_in() {
        let program = world();
        let mut machine = machine(&program);
        let addr = word(
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
    /// layout is what says how wide a member is and which of its words the
    /// collector traces, so it is refused rather than guessed at.
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
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, int, &[1, 2, 3]);
        let empty = members(&mut machine, int, &[]);
        assert_eq!(
            word(&mut machine, "Set", "length", &[(Repr::Ref, items)]).unwrap(),
            3
        );
        assert_eq!(
            word(&mut machine, "Set", "isEmpty", &[(Repr::Ref, items)]).unwrap(),
            0
        );
        assert_eq!(
            word(&mut machine, "Set", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
            1
        );

        let array = word(&mut machine, "Set", "toArray", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(words_of(&machine, array), vec![1, 2, 3]);
    }

    /// Membership is the binary search the sorted run is for, and it answers
    /// the same thing at both ends and in the middle.
    #[test]
    fn a_set_answers_membership_by_searching() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, int, &[1, 3, 5, 7]);
        for (element, expected) in [(1, 1), (3, 1), (5, 1), (7, 1), (0, 0), (4, 0), (9, 0)] {
            assert_eq!(
                word(
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
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, int, &[1, 3]);

        let with = word(
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
        let low = word(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 0)],
        )
        .unwrap();
        assert_eq!(held(&machine, low, 1), vec![0, 1, 3]);
        let high = word(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 9)],
        )
        .unwrap();
        assert_eq!(held(&machine, high, 1), vec![1, 3, 9]);
        let again = word(
            &mut machine,
            "Set",
            "inserted",
            &[(Repr::Ref, items), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(held(&machine, again, 1), vec![1, 3]);

        let without = word(
            &mut machine,
            "Set",
            "removed",
            &[(Repr::Ref, items), (Repr::Int, 1)],
        )
        .unwrap();
        assert_eq!(held(&machine, without, 1), vec![3]);
        // An element that was not there answers a copy.
        let same = word(
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
            entry(&program, 3, 30),
            entry(&program, 1, 10),
            entry(&program, 2, 20),
        );
        let built = values(
            &mut machine,
            "Map",
            "of",
            &[(a.0, &a.1), (b.0, &b.1), (c.0, &c.1)],
        )
        .unwrap();
        let addr = built[0];
        assert_eq!(held(&machine, addr, 2), vec![1, 10, 2, 20, 3, 30]);
        assert_eq!(machine.object_len(addr), 3);
    }

    #[test]
    fn a_map_refuses_the_same_key_twice_and_anything_that_is_not_an_entry() {
        let program = world();
        let mut machine = machine(&program);
        let (a, b) = (entry(&program, 1, 10), entry(&program, 1, 20));
        let error = values(&mut machine, "Map", "of", &[(a.0, &a.1), (b.0, &b.1)]).unwrap_err();
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
        let int = scalar(&program, Repr::Int);
        let held_map = entries(&mut machine, int, int, &[(1, 10), (2, 20)]);
        let empty = entries(&mut machine, int, int, &[]);

        assert_eq!(
            word(&mut machine, "Map", "length", &[(Repr::Ref, held_map)]).unwrap(),
            2
        );
        assert_eq!(
            word(&mut machine, "Map", "isEmpty", &[(Repr::Ref, held_map)]).unwrap(),
            0
        );
        assert_eq!(
            word(&mut machine, "Map", "isEmpty", &[(Repr::Ref, empty)]).unwrap(),
            1
        );
        assert_eq!(
            word(
                &mut machine,
                "Map",
                "contains",
                &[(Repr::Ref, held_map), (Repr::Int, 2)]
            )
            .unwrap(),
            1
        );
        assert_eq!(
            word(
                &mut machine,
                "Map",
                "contains",
                &[(Repr::Ref, held_map), (Repr::Int, 3)]
            )
            .unwrap(),
            0
        );

        let keys = word(&mut machine, "Map", "keys", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(words_of(&machine, keys), vec![1, 2]);
        let values = word(&mut machine, "Map", "values", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(words_of(&machine, values), vec![10, 20]);
    }

    /// `get` answers an `Option` of the value family, which is where a
    /// missing key and a present one are one answer rather than two — and it
    /// is a run of words now rather than an object.
    #[test]
    fn a_map_get_answers_an_option() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let held_map = entries(&mut machine, int, int, &[(1, 10), (2, 20)]);
        let found = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, int, &found),
            ("Some".to_string(), vec![20])
        );
        let missing = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, int, &missing),
            ("None".to_string(), vec![])
        );
    }

    #[test]
    fn inserting_and_removing_answer_new_maps() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let held_map = entries(&mut machine, int, int, &[(1, 10), (3, 30)]);

        let with = word(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Int, 2), (Repr::Int, 20)],
        )
        .unwrap();
        assert_eq!(held(&machine, with, 2), vec![1, 10, 2, 20, 3, 30]);
        assert_eq!(held(&machine, held_map, 2), vec![1, 10, 3, 30]);

        // A key already there keeps its place and takes the new value.
        let over = word(
            &mut machine,
            "Map",
            "inserted",
            &[(Repr::Ref, held_map), (Repr::Int, 3), (Repr::Int, 99)],
        )
        .unwrap();
        assert_eq!(held(&machine, over, 2), vec![1, 10, 3, 99]);

        let without = word(
            &mut machine,
            "Map",
            "removed",
            &[(Repr::Ref, held_map), (Repr::Int, 1)],
        )
        .unwrap();
        assert_eq!(held(&machine, without, 2), vec![3, 30]);
        let same = word(
            &mut machine,
            "Map",
            "removed",
            &[(Repr::Ref, held_map), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(held(&machine, same, 2), vec![1, 10, 3, 30]);
    }

    /// A member is a run at the element layout's width, so every offset is an
    /// index times a stride and every length stays a count of members.
    #[test]
    fn a_run_of_multiword_members_is_walked_at_its_stride() {
        let program = wide();
        let mut machine = machine(&program);
        let int = named(&program, "Int");
        let point = named(&program, "Point");

        let items = members(&mut machine, point, &[1, 2, 3, 4]);
        assert_eq!(machine.object_len(items), 2, "two members, four words");
        assert_eq!(
            word(&mut machine, "Set", "length", &[(Repr::Ref, items)]).unwrap(),
            2
        );
        let array = word(&mut machine, "Set", "toArray", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(machine.object_len(array), 2);
        assert_eq!(machine.payload_run(array, 0, 4), vec![1, 2, 3, 4]);

        // A map's entry is the key's words then the value's, so `values`
        // reads from the key's width and `get` answers the whole `Point`.
        let layout = make::entries(&program, int, point).unwrap();
        let held_map = machine.new_object(layout, 2).unwrap();
        machine.set_payload_run(held_map, 0, &[1, 10, 20, 2, 30, 40]);
        let keys = word(&mut machine, "Map", "keys", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(machine.payload_run(keys, 0, 2), vec![1, 2]);
        let values = word(&mut machine, "Map", "values", &[(Repr::Ref, held_map)]).unwrap();
        assert_eq!(machine.object_len(values), 2);
        assert_eq!(machine.payload_run(values, 0, 4), vec![10, 20, 30, 40]);

        let found = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Int, 2)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, point, &found),
            ("Some".to_string(), vec![30, 40])
        );
    }

    /// A two-word member goes into a set as both of its words, because an
    /// operand is a value location. This refused until an argument carried
    /// its layout: a call said where the `Point` began and never that it was
    /// two words, and writing the one word that arrived into a run whose
    /// stride is two would have been a silently wrong set.
    #[test]
    fn a_member_wider_than_a_word_is_inserted_whole() {
        let program = wide();
        let mut machine = machine(&program);
        let point = named(&program, "Point");
        let items = members(&mut machine, point, &[3, 4]);
        let sets = machine.object_layout(items);
        let grown = values(
            &mut machine,
            "Set",
            "inserted",
            &[(sets, &[items]), (point, &[1, 2])],
        )
        .unwrap();
        // Sorted because it was built sorted: the new member is smaller, so
        // it goes in front of the one the set already held.
        assert_eq!(words_of(&machine, grown[0]), vec![1u64, 2, 3, 4]);
        assert_eq!(machine.object_len(grown[0]), 2);
        // And the set that was handed over is untouched: `inserted` is a past
        // participle, and neither of them writes through the receiver.
        assert_eq!(
            word(&mut machine, "Set", "length", &[(Repr::Ref, items)]).unwrap(),
            1
        );
    }

    /// A sorted run is traced by its element layout's map and searched at its
    /// width, so a member of another family is refused rather than written.
    #[test]
    fn a_member_of_another_family_is_refused_rather_than_stored() {
        let program = wide();
        let mut machine = machine(&program);
        let point = named(&program, "Point");
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, point, &[1, 2]);
        let sets = machine.object_layout(items);
        let error = values(
            &mut machine,
            "Set",
            "inserted",
            &[(sets, &[items]), (int, &[5])],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Set.inserted` expects `Point` here, but found `Int`"
        );
    }

    /// A key the language does not admit stops the operation before anything
    /// is searched, in the words the oracle refuses it in — an empty map as
    /// loudly as a full one.
    #[test]
    fn an_argument_that_cannot_be_a_key_is_refused_before_the_search() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let empty = entries(&mut machine, int, int, &[]);
        let error = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, empty), (Repr::Float, 1.5f64.to_bits())],
        )
        .unwrap_err();
        assert_eq!(error.message, "`Map.get` cannot use a `Float` as a map key");

        let items = members(&mut machine, int, &[]);
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
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, int, &[1]);
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
        let int = scalar(&program, Repr::Int);
        let items = members(&mut machine, int, &[1]);
        let error = run(&mut machine, "Set", "contains", &[(Repr::Ref, items)]).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.contains` takes 1 argument(s), but 0 were given"
        );

        let held_map = entries(&mut machine, int, int, &[(1, 10)]);
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
        let int = scalar(&program, Repr::Int);
        let text = program.str_layout;
        let a = machine.new_string("a").unwrap();
        let b = machine.new_string("b").unwrap();
        let held_map = entries(&mut machine, text, int, &[(a, 1), (b, 2)]);
        let wanted = machine.new_string("b").unwrap();
        assert_ne!(wanted, b);
        let found = run(
            &mut machine,
            "Map",
            "get",
            &[(Repr::Ref, held_map), (Repr::Ref, wanted)],
        )
        .unwrap();
        assert_eq!(
            option_of(&program, int, &found),
            ("Some".to_string(), vec![2])
        );

        // And the key the map keeps is the one it already held.
        let over = word(
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
        let addr = word(
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
