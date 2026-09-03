//! The order a `Map` and a `Set` are kept in, over runs of words and heap
//! objects.
//!
//! A `Set` iterates and renders in ascending element order and a `Map` in
//! ascending key order, so the order is *part of the value* rather than an
//! implementation's leftovers — which is why both are sorted runs here and a
//! lookup is a binary search. This is the comparison that search is over.
//!
//! [`crate::value::MapKey`] is the oracle's copy of it: a value converted to
//! the shapes a key may take, ordered by the `Ord` its declaration derives.
//! The two are written twice for the reason [`super`]'s rendering and
//! [`super::equal`]'s equality are — one reads a materialised tree and one
//! reads the heap, and neither can be had from the other without building
//! what the other exists to avoid. What keeps them saying the same thing is
//! that a `Map` built here and materialised at the boundary is re-sorted by
//! `MapKey`'s own `Ord` on the way out: a disagreement shows up as an order
//! that changed while crossing.
//!
//! # A value is a run of words, and an operand is one of them
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) says one slot is one
//! eight-byte word and one value may occupy several, so a struct and an enum
//! are inline: a `Point` member of a `Set<Point>` is two words *where the
//! member is*. Everything below therefore compares a [`Key`], which is either
//! the run of words a known layout describes or one operand word.
//!
//! The two halves exist because a [`cove_ir::Builtin`] carries no layout for
//! its operands and an argument list is base slots that need not be adjacent,
//! so the machine cannot know how wide an operand is and an operand stays one
//! word. A set's member and a map's key are not so restricted — the receiver's
//! own layout says what they are — which is why a search compares a value of a
//! known layout against an operand rather than two operands.
//!
//! # The order between families is the order the variants are declared in
//!
//! A derived `Ord` compares the discriminant first, so `MapKey`'s *declaration
//! order* is the order between families and has to be reproduced exactly — a
//! nested key can be a struct, an array, a set or a map, and a set holding two
//! of them renders in whichever order this says.
//!
//! | rank | `MapKey` | here |
//! |---|---|---|
//! | 0 | `Unit` | a `Repr::Unit` word |
//! | 1 | `Bool` | a `Repr::Bool` word |
//! | 2 | `Int` | a `Repr::Int` word |
//! | 3 | `Duration` | a `Repr::Duration` word |
//! | 4 | `Str` | a [`Shape::Str`] object |
//! | 5 | `EnumCase` | the inline words of a [`Shape::Enum`] |
//! | 6 | `Struct` | the inline words of a [`Shape::Struct`] that is not the `Range` |
//! | 7 | `Array` | a [`Shape::Elements`] object that cannot grow |
//! | 8 | `Set` | a [`Shape::Members`] object |
//! | 9 | `Map` | a [`Shape::Entries`] object |
//! | 10 | `Range` | the program's `Range` struct |
//!
//! Two of those are worth saying out loud, because the representation would
//! suggest otherwise. A `Range` is a three-word struct and sorts *after* every
//! other family rather than among the structs, because `MapKey` declares it
//! last. An `Int` and a `Duration` are the same sixty-four bits and are never
//! compared as numbers: every `Int` sorts before every `Duration`.
//!
//! # A name here is the layout's, and the oracle's is the declaration's
//!
//! `MapKey::Struct` and `MapKey::EnumCase` are keyed by the type name the
//! *value* carries, which is qualified — `rules.policy.Decision`. A
//! [`cove_ir::Layout`] carries the unqualified name, which is also the name
//! the boundary materialises with, so a set of two different struct types is
//! ordered by their short names on this side and by their qualified ones on
//! the oracle's. The two agree wherever the qualification does not decide the
//! comparison, which is every program whose key types are declared in one
//! module, and disagree about the *order* — never about membership or
//! equality — where they are not. The fix is a qualified name in the layout
//! table, which is a change to the lowering rather than to this file.
//!
//! # What is refused
//!
//! ADR 0001 draws the line at mutability: a key's equality must not be able to
//! change while a collection holds it. So a `Vector` and everything that holds
//! one is refused, and a `Float` is refused for the unrelated reason that
//! `NaN` is not equal to itself, which breaks the total order every key needs.
//! [`check`] is that question asked of a value, and its refusals are
//! [`crate::builtins`]' word for word, path included.

use std::cmp::Ordering;
use std::fmt::Write as _;

use cove_ir::{Field, LayoutId, Part, Program, Repr, Shape};

use crate::error::RuntimeError;
use crate::vm::boundary::is_range;
use crate::vm::builtins::operand::{self, Operand};
use crate::vm::builtins::{equal, render_value};
use crate::vm::exec::Machine;

/// What a `Map`'s key argument is called in a refusal.
pub(super) const MAP_KEY: &str = "map key";

/// What a `Set`'s element argument is called in one.
pub(super) const SET_ELEMENT: &str = "set element";

/// A value on its way through the order.
///
/// A member of a set, a key of a map, a field of a struct, an element of an
/// array and an argument all arrive as [`Key::Held`]: a layout that says the
/// width and the parts, and the run of words the value occupies. An argument
/// used to be a [`Key::Word`] because that was all an operand could be, and
/// now it is not — what is left of that half is what a *reduction* answers,
/// the address a value of a heap family consists of and the bits a scalar is.
#[derive(Clone, Copy)]
enum Key<'w> {
    /// One word, described only by its `Repr`.
    Word(Repr, u64),
    /// A value of a known layout, as the run of words it occupies.
    Held(LayoutId, &'w [u64]),
}

/// One step further in towards a family the order has an arm for.
///
/// Owned rather than borrowed because two of the three steps read words out
/// of the heap: what is inside a box, and what a struct-shaped object holds.
/// The caller keeps the run alive for exactly the recursive call it makes.
enum Step {
    Word(Repr, u64),
    Held(LayoutId, Vec<u64>),
}

impl Step {
    fn key(&self) -> Key<'_> {
        match self {
            Step::Word(repr, word) => Key::Word(*repr, *word),
            Step::Held(layout, words) => Key::Held(*layout, words),
        }
    }
}

/// Whether `operand` may be a key or an element of a set at all.
///
/// Asked of the whole value before anything is compared, which is where the
/// oracle asks it: `Map.get` converts its argument to a `MapKey` before it
/// looks at a single entry, so a `Float` is refused by an empty map as loudly
/// as by a full one.
pub(super) fn check(
    machine: &Machine,
    method: &str,
    role: &str,
    operand: Operand<'_>,
) -> Result<(), RuntimeError> {
    admits(
        machine,
        method,
        role,
        None,
        Key::Held(operand.layout, operand.words),
        0,
    )
}

/// The same, of a value that arrived as the words of a known layout.
///
/// What `Map.of` asks of the key it read out of a `MapEntry`: a key that is a
/// struct is a run of words rather than an address, so there is no operand to
/// ask about and the layout is what says which words are what.
pub(super) fn check_value(
    machine: &Machine,
    method: &str,
    role: &str,
    layout: LayoutId,
    words: &[u64],
) -> Result<(), RuntimeError> {
    admits(machine, method, role, None, Key::Held(layout, words), 0)
}

/// Where the value `a` sorts relative to the value `b`, both of `layout`.
///
/// Both are keys: every word that reaches this either passed [`check`] or was
/// written into a sorted run by something that did.
pub(super) fn cmp_value(
    machine: &Machine,
    layout: LayoutId,
    a: &[u64],
    b: &[u64],
) -> Result<Ordering, RuntimeError> {
    order(machine, Key::Held(layout, a), Key::Held(layout, b), 0)
}

/// Where the stored value `held`, of `layout`, sorts relative to the operand
/// `wanted`.
///
/// The shape a binary search over a run has: what is in the run is a value of
/// the run's element layout and what is being looked for is the operand's
/// own, and the two are compared without either being read as the other.
/// Reading the operand *as* the element layout would be the wrong answer
/// wherever the two disagree — a boxed `Int` looked for in a `Set<Int>` is
/// one address and one integer, and comparing them as integers would compare
/// a heap address with a number.
pub(super) fn cmp_held(
    machine: &Machine,
    layout: LayoutId,
    held: &[u64],
    wanted: Operand<'_>,
) -> Result<Ordering, RuntimeError> {
    order(
        machine,
        Key::Held(layout, held),
        Key::Held(wanted.layout, wanted.words),
        0,
    )
}

// --- looking through a description -----------------------------------------

/// The value `key` names, one description in, or `None` when it is already a
/// family the order has an arm for.
///
/// Three things reduce, and each of them costs a depth so that a graph that
/// holds itself stops rather than running out of native stack.
///
/// A value of a family that *lives in* the heap is one word — a `String`, an
/// `Array`, a `Set`, a `Map` — so a layout that describes one reduces to the
/// address it holds. An address naming a struct-shaped or enum-shaped object
/// reduces the other way: those families are inline, so the object's payload
/// *is* the value's words, which is how a `Map.of` entry the lowering
/// allocated is read. And a box reduces to what it was given: payload word 0
/// is the [`LayoutId`] of what it holds and the words after it are that
/// value, inline.
///
/// Erasure is looked through before anything else, and by
/// [`super::equal::unboxed`] rather than by a second reading of a box here:
/// two values `==` calls equal have to be usable as one key, and one rule
/// written once is what makes that so.
fn inward(machine: &Machine, key: Key) -> Result<Option<Step>, RuntimeError> {
    Ok(match key {
        Key::Held(layout, words) => {
            let described = machine.program().layout(layout);
            let Some(first) = words.first().copied() else {
                return Ok(None);
            };
            match described.shape {
                Shape::Word(repr) => Some(Step::Word(repr, first)),
                _ if described.is_one_address() => Some(Step::Word(Repr::Ref, first)),
                _ => None,
            }
        }
        Key::Word(Repr::Ref, addr) if addr != 0 => {
            if let Some((held, words)) = equal::unboxed(machine, (Repr::Ref, addr))? {
                return Ok(Some(Step::Held(held, words)));
            }
            let id = machine.object_layout(addr);
            let described = machine.program().layout(id);
            match &described.shape {
                Shape::Struct { .. } | Shape::Enum { .. } => Some(Step::Held(
                    id,
                    machine.payload_run(addr, 0, described.width()),
                )),
                _ => None,
            }
        }
        _ => None,
    })
}

/// The words of one field of a struct, as the value they are.
fn field_of<'w>(program: &Program, words: &'w [u64], field: &Field) -> Key<'w> {
    let width = program.layout(field.layout).width();
    Key::Held(field.layout, run(words, field.at, width))
}

/// The words of one part of an enum case's payload, as the value they are.
///
/// `part.at` is an offset within the *payload region*, which begins after the
/// discriminant, so within the whole value it is `1 + part.at`.
fn part_of<'w>(program: &Program, words: &'w [u64], part: &Part) -> Key<'w> {
    let width = program.layout(part.layout).width();
    Key::Held(part.layout, run(words, 1 + part.at, width))
}

/// `words[at..at + width]`, or nothing when the run is shorter than the
/// layout says.
///
/// Empty rather than a panic. A run that does not hold the words its layout
/// describes is a lowering bug, and a comparison is a bad place to discover
/// one by unwinding: an empty run reaches [`family`] and is refused there,
/// with a message, like every other value that cannot be a key.
fn run(words: &[u64], at: u32, width: u32) -> &[u64] {
    let (at, width) = (at as usize, width as usize);
    words.get(at..at + width).unwrap_or(&[])
}

// --- admitting a key -------------------------------------------------------

/// `anchor` is the path to this value from the one that was asked about, so
/// that a refusal several levels down names the part that is wrong rather
/// than blaming the whole struct. `None` at the root: a bare value has no name
/// to anchor a path to, and a struct or an enum invents one from its own type
/// name the first time a path is needed.
fn admits(
    machine: &Machine,
    method: &str,
    role: &str,
    anchor: Option<&str>,
    key: Key,
    depth: usize,
) -> Result<(), RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(equal::too_deep());
    }
    let deeper = depth + 1;
    if let Some(step) = inward(machine, key)? {
        return admits(machine, method, role, anchor, step.key(), deeper);
    }
    match key {
        Key::Word(repr, word) => match repr {
            Repr::Unit | Repr::Bool | Repr::Int | Repr::Duration => Ok(()),
            Repr::Ref => admits_object(machine, method, role, anchor, word, deeper),
            // Every other `Repr` is refused by the name the language gives it,
            // which for a `Float` is the one rejection with a rule of its own.
            _ => Err(refused(
                method,
                role,
                anchor,
                &operand::type_name(machine, repr, word),
            )),
        },
        Key::Held(layout, words) => {
            admits_value(machine, method, role, anchor, layout, words, deeper)
        }
    }
}

/// Whether the object at `addr` may be a key.
///
/// Only the families that *live in* the heap reach this. A struct and an enum
/// are inline, so [`inward`] has already turned an address naming one into
/// the words it holds.
fn admits_object(
    machine: &Machine,
    method: &str,
    role: &str,
    anchor: Option<&str>,
    addr: u64,
    depth: usize,
) -> Result<(), RuntimeError> {
    if addr == 0 {
        return Err(operand::null_value());
    }
    let layout = machine.program().layout(machine.object_layout(addr));
    match &layout.shape {
        Shape::Str => Ok(()),
        Shape::Free => Err(operand::reclaimed()),
        // An array is fixed-length and immutable, so its equality cannot
        // change and every element decides for itself. A growable run is a
        // `Vector`'s store, and refusing it is refusing the vector.
        Shape::Elements {
            elem,
            growable: false,
        } => {
            let base = path(anchor, String::new);
            for at in 0..machine.object_len(addr) {
                let words = element(machine, addr, *elem, at);
                let anchor = format!("{base}[{at}]");
                admits(
                    machine,
                    method,
                    role,
                    Some(&anchor),
                    Key::Held(*elem, &words),
                    depth,
                )?;
            }
            Ok(())
        }
        // A set's members are keys by construction, so nesting one never
        // fails and nothing inside it is walked.
        Shape::Members { .. } => Ok(()),
        // A map's keys are keys by construction too; only its values need
        // asking, and the first one that cannot be is why nesting a `Map` as
        // a key can still fail. The path names the entry by its key, exactly
        // as the key would render anywhere else.
        Shape::Entries { .. } => {
            let pairs = pairs_of(machine, addr);
            let base = path(anchor, String::new);
            for at in 0..machine.object_len(addr) {
                let key = pairs.key_words(machine, addr, at);
                let shown = render_value(machine, pairs.key, &key, 0)?;
                let value = pairs.value_words(machine, addr, at);
                let anchor = format!("{base}[{shown}]");
                admits(
                    machine,
                    method,
                    role,
                    Some(&anchor),
                    Key::Held(pairs.value, &value),
                    depth,
                )?;
            }
            Ok(())
        }
        _ => Err(refused(
            method,
            role,
            anchor,
            &operand::type_name(machine, Repr::Ref, addr),
        )),
    }
}

/// Whether the value `words`, read as `layout`, may be a key.
///
/// Only the inline families reach this: a layout that describes a value
/// living in the heap has already reduced to the address it holds.
fn admits_value(
    machine: &Machine,
    method: &str,
    role: &str,
    anchor: Option<&str>,
    layout: LayoutId,
    words: &[u64],
    depth: usize,
) -> Result<(), RuntimeError> {
    let program = machine.program();
    let described = program.layout(layout);
    match &described.shape {
        // A `Range` is an immutable value with a stable equality, so it is a
        // key like any other and there is nothing inside it to walk.
        Shape::Struct { .. } if is_range(program, described) => Ok(()),
        Shape::Struct { fields, .. } => {
            let base = path(anchor, || described.name.to_string());
            for field in fields {
                let anchor = format!("{base}.{}", field.name);
                admits(
                    machine,
                    method,
                    role,
                    Some(&anchor),
                    field_of(program, words, field),
                    depth,
                )?;
            }
            Ok(())
        }
        Shape::Enum { cases, .. } => {
            let index = words.first().copied().unwrap_or_default();
            let case = cases
                .get(index as usize)
                .ok_or_else(|| wrong_case(&described.name))?;
            let base = path(anchor, || format!("{}.{}", described.name, case.name));
            for (at, part) in case.parts.iter().enumerate() {
                let anchor = format!("{base}({at})");
                admits(
                    machine,
                    method,
                    role,
                    Some(&anchor),
                    part_of(program, words, part),
                    depth,
                )?;
            }
            Ok(())
        }
        Shape::Free => Err(operand::reclaimed()),
        _ => Err(refused(
            method,
            role,
            anchor,
            &operand::layout_name(
                machine,
                layout,
                words.first().copied().unwrap_or_default(),
                depth,
            ),
        )),
    }
}

/// The anchor a nested part is reached through, or the one this value invents
/// for itself when it is the root.
fn path(anchor: Option<&str>, own: impl FnOnce() -> String) -> String {
    match anchor {
        Some(anchor) => anchor.to_string(),
        None => own(),
    }
}

// --- ordering two keys -----------------------------------------------------

fn order(machine: &Machine, a: Key, b: Key, depth: usize) -> Result<Ordering, RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(equal::too_deep());
    }
    let deeper = depth + 1;
    if let Some(step) = inward(machine, a)? {
        return order(machine, step.key(), b, deeper);
    }
    if let Some(step) = inward(machine, b)? {
        return order(machine, a, step.key(), deeper);
    }
    let (x, y) = (family(machine, a)?, family(machine, b)?);
    match x.rank().cmp(&y.rank()) {
        Ordering::Equal => {}
        other => return Ok(other),
    }
    let program = machine.program();
    match (x, y) {
        (Family::Unit, Family::Unit) => Ok(Ordering::Equal),
        (Family::Bool(a), Family::Bool(b)) => Ok(a.cmp(&b)),
        (Family::Int(a), Family::Int(b)) => Ok(a.cmp(&b)),
        (Family::Duration(a), Family::Duration(b)) => Ok(a.cmp(&b)),
        // Byte-wise, which is what `String`'s own `Ord` is.
        (Family::Str(a), Family::Str(b)) => {
            Ok(machine.string_bytes(a).cmp(&machine.string_bytes(b)))
        }
        // Type name, then case name, then payload — and the case is read out
        // of word 0, because which of the payload words are anything at all
        // depends on the case the value is in.
        (Family::Case(x, a), Family::Case(y, b)) => {
            let (left, right) = (program.layout(x), program.layout(y));
            let (Shape::Enum { cases: ones, .. }, Shape::Enum { cases: others, .. }) =
                (&left.shape, &right.shape)
            else {
                unreachable!("`family` answers `Case` for an enum-shaped value");
            };
            let index = |words: &[u64]| words.first().copied().unwrap_or_default() as usize;
            let one = ones.get(index(a)).ok_or_else(|| wrong_case(&left.name))?;
            let other = others
                .get(index(b))
                .ok_or_else(|| wrong_case(&right.name))?;
            match (*left.name)
                .cmp(&right.name)
                .then_with(|| (*one.name).cmp(&other.name))
            {
                Ordering::Equal => {}
                ordered => return Ok(ordered),
            }
            for (part, counterpart) in one.parts.iter().zip(&other.parts) {
                match order(
                    machine,
                    part_of(program, a, part),
                    part_of(program, b, counterpart),
                    deeper,
                )? {
                    Ordering::Equal => {}
                    ordered => return Ok(ordered),
                }
            }
            Ok(one.parts.len().cmp(&other.parts.len()))
        }
        // Type name, then the fields as pairs of name and value, then how
        // many there are, then whether the declaration was opaque. That is
        // `MapKey::Struct`'s derived order field for field: it carries
        // `(String, Vec<(String, MapKey)>, bool)` and compares them in that
        // order.
        (Family::Struct(x, a), Family::Struct(y, b)) => {
            let (left, right) = (program.layout(x), program.layout(y));
            let (
                Shape::Struct {
                    fields: ones,
                    opaque: a_opaque,
                },
                Shape::Struct {
                    fields: others,
                    opaque: b_opaque,
                },
            ) = (&left.shape, &right.shape)
            else {
                unreachable!("`family` answers `Struct` for a struct-shaped value");
            };
            match (*left.name).cmp(&right.name) {
                Ordering::Equal => {}
                ordered => return Ok(ordered),
            }
            for (one, other) in ones.iter().zip(others) {
                match (*one.name).cmp(&other.name) {
                    Ordering::Equal => {}
                    ordered => return Ok(ordered),
                }
                match order(
                    machine,
                    field_of(program, a, one),
                    field_of(program, b, other),
                    deeper,
                )? {
                    Ordering::Equal => {}
                    ordered => return Ok(ordered),
                }
            }
            Ok(ones.len().cmp(&others.len()).then(a_opaque.cmp(b_opaque)))
        }
        // An array compares element for element, and a set does the same over
        // members that are already ascending — which is what `BTreeSet`'s
        // `Ord` does with the same two runs.
        (Family::Array(a), Family::Array(b)) | (Family::Set(a), Family::Set(b)) => {
            sequences(machine, a, b, deeper)
        }
        // And a map compares entry for entry, key before value, which is
        // `BTreeMap`'s `Ord` over its ascending pairs.
        (Family::Map(a), Family::Map(b)) => maps(machine, a, b, deeper),
        // The bounds as they were written, in the order `MapKey::Range`
        // declares its fields: an inclusive range sorts after the exclusive
        // one with the same two numbers, because `false < true`.
        (Family::Range(a), Family::Range(b)) => {
            let word = |words: &[u64], at: usize| words.get(at).copied().unwrap_or_default() as i64;
            Ok(word(a, 0)
                .cmp(&word(b, 0))
                .then_with(|| word(a, 1).cmp(&word(b, 1)))
                .then_with(|| (word(a, 2) != 0).cmp(&(word(b, 2) != 0))))
        }
        _ => unreachable!("two families of one rank are one family"),
    }
}

/// Lexicographic order over two runs, the shorter first when one is a prefix
/// of the other — which is `Vec`'s own `Ord` and therefore every `MapKey`
/// variant that holds one.
///
/// The elements are read one at a time rather than collected first, because
/// an element is a run of words: a `Set<Point>` is a run of two-word members,
/// and materialising both whole runs to answer a question about their fronts
/// would be paying for the length to compare the head.
fn sequences(machine: &Machine, a: u64, b: u64, depth: usize) -> Result<Ordering, RuntimeError> {
    let (x, y) = (elem_of(machine, a), elem_of(machine, b));
    let (left, right) = (machine.object_len(a), machine.object_len(b));
    for at in 0..left.min(right) {
        let one = element(machine, a, x, at);
        let other = element(machine, b, y, at);
        match order(machine, Key::Held(x, &one), Key::Held(y, &other), depth)? {
            Ordering::Equal => {}
            ordered => return Ok(ordered),
        }
    }
    Ok(left.cmp(&right))
}

/// The same, entry for entry and key before value.
fn maps(machine: &Machine, a: u64, b: u64, depth: usize) -> Result<Ordering, RuntimeError> {
    let (x, y) = (pairs_of(machine, a), pairs_of(machine, b));
    let (left, right) = (machine.object_len(a), machine.object_len(b));
    for at in 0..left.min(right) {
        let (one, other) = (x.key_words(machine, a, at), y.key_words(machine, b, at));
        match order(
            machine,
            Key::Held(x.key, &one),
            Key::Held(y.key, &other),
            depth,
        )? {
            Ordering::Equal => {}
            ordered => return Ok(ordered),
        }
        let (one, other) = (x.value_words(machine, a, at), y.value_words(machine, b, at));
        match order(
            machine,
            Key::Held(x.value, &one),
            Key::Held(y.value, &other),
            depth,
        )? {
            Ordering::Equal => {}
            ordered => return Ok(ordered),
        }
    }
    Ok(left.cmp(&right))
}

/// The element layout of an `Array` or the member layout of a `Set`.
fn elem_of(machine: &Machine, addr: u64) -> LayoutId {
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements { elem, .. } | Shape::Members { elem } => elem,
        _ => unreachable!("`family` answers a run for a run-shaped object"),
    }
}

/// Element `at` of the run at `addr`, as the words it occupies.
///
/// The header's `len` counts elements and not words, so the offset is the
/// index times the element layout's width.
fn element(machine: &Machine, addr: u64, elem: LayoutId, at: u32) -> Vec<u64> {
    let width = machine.words_of(elem);
    machine.payload_run(addr, at * width, width)
}

/// How a map keeps the two halves of an entry: key words then value words, at
/// their two layouts' widths.
struct Pairs {
    key: LayoutId,
    value: LayoutId,
    keys: u32,
    values: u32,
}

impl Pairs {
    /// The words of entry `at`'s key.
    fn key_words(&self, machine: &Machine, addr: u64, at: u32) -> Vec<u64> {
        machine.payload_run(addr, at * self.stride(), self.keys)
    }

    /// The words of entry `at`'s value, which begin at the key's width.
    fn value_words(&self, machine: &Machine, addr: u64, at: u32) -> Vec<u64> {
        machine.payload_run(addr, at * self.stride() + self.keys, self.values)
    }

    fn stride(&self) -> u32 {
        self.keys + self.values
    }
}

fn pairs_of(machine: &Machine, addr: u64) -> Pairs {
    let Shape::Entries { key, value } = machine.program().layout(machine.object_layout(addr)).shape
    else {
        unreachable!("`family` answers `Map` for an entries-shaped object");
    };
    Pairs {
        key,
        value,
        keys: machine.words_of(key),
        values: machine.words_of(value),
    }
}

/// Which of the eleven shapes a key may take this one is.
///
/// The scalars carry their value because the word is the whole of them; a
/// family that lives in the heap carries its object, because what it holds is
/// read out of it; and an inline family carries its layout and its words,
/// because those *are* it.
enum Family<'w> {
    Unit,
    Bool(bool),
    Int(i64),
    Duration(i64),
    Str(u64),
    Case(LayoutId, &'w [u64]),
    Struct(LayoutId, &'w [u64]),
    Array(u64),
    Set(u64),
    Map(u64),
    Range(&'w [u64]),
}

impl Family<'_> {
    /// Where this family sits in the one order. See the table in [`self`].
    fn rank(&self) -> u8 {
        match self {
            Family::Unit => 0,
            Family::Bool(_) => 1,
            Family::Int(_) => 2,
            Family::Duration(_) => 3,
            Family::Str(_) => 4,
            Family::Case(..) => 5,
            Family::Struct(..) => 6,
            Family::Array(_) => 7,
            Family::Set(_) => 8,
            Family::Map(_) => 9,
            Family::Range(_) => 10,
        }
    }
}

/// Which family `key` belongs to, asked only of a key [`inward`] has nothing
/// left to look through on.
fn family<'w>(machine: &Machine, key: Key<'w>) -> Result<Family<'w>, RuntimeError> {
    match key {
        Key::Word(repr, word) => match repr {
            Repr::Unit => Ok(Family::Unit),
            Repr::Bool => Ok(Family::Bool(word != 0)),
            Repr::Int => Ok(Family::Int(word as i64)),
            Repr::Duration => Ok(Family::Duration(word as i64)),
            Repr::Ref => {
                if word == 0 {
                    return Err(operand::null_value());
                }
                match machine.program().layout(machine.object_layout(word)).shape {
                    Shape::Str => Ok(Family::Str(word)),
                    Shape::Free => Err(operand::reclaimed()),
                    Shape::Elements {
                        growable: false, ..
                    } => Ok(Family::Array(word)),
                    Shape::Members { .. } => Ok(Family::Set(word)),
                    Shape::Entries { .. } => Ok(Family::Map(word)),
                    _ => Err(not_a_key()),
                }
            }
            _ => Err(not_a_key()),
        },
        Key::Held(layout, words) => {
            let program = machine.program();
            let described = program.layout(layout);
            match &described.shape {
                Shape::Struct { .. } if is_range(program, described) => Ok(Family::Range(words)),
                Shape::Struct { .. } => Ok(Family::Struct(layout, words)),
                Shape::Enum { .. } => Ok(Family::Case(layout, words)),
                Shape::Free => Err(operand::reclaimed()),
                // Every remaining shape describes a value that lives in the
                // heap, which `inward` reduced to its address — so what is
                // left is a run too short to hold the words its layout
                // describes.
                _ => Err(not_a_key()),
            }
        }
    }
}

// --- refusals --------------------------------------------------------------

/// `` `{method}` cannot use a `{type}` as a {role} ``, and the same naming the
/// part it is nested in.
///
/// [`crate::builtins`]' `invalid_key_error` word for word, over the path
/// [`admits`] built rather than the one `MapKey::convert` did. The rule and
/// the help are [`crate::value::InvalidKey`]'s two, restated here for the
/// reason [`operand`]'s messages are restated: a refusal is the *language's*,
/// and the differential corpus compares the text.
fn refused(method: &str, role: &str, anchor: Option<&str>, type_name: &str) -> RuntimeError {
    let path = anchor.unwrap_or_default();
    let mut message = format!("`{method}` cannot use a `{type_name}`");
    if !path.is_empty() {
        write!(message, " inside `{path}`").expect("a string never fails to be written to");
    }
    write!(message, " as a {role}").expect("a string never fails to be written to");
    // A `Float` is excluded for a reason distinct from every other rejection —
    // `NaN != NaN` breaks the total order a key needs, which has nothing to do
    // with mutability — and the two answers are kept apart so that nobody
    // later "fixes" `Float` as if it were one more mutable handle.
    let (rule, help) = if type_name == "Float" {
        (
            "A `Float` cannot be a map key or set element: `NaN` is not equal to itself, which breaks the total order every key needs.",
            "convert it to a stable key first, such as rounding to an `Int` or formatting it as a `String`",
        )
    } else {
        (
            "Mutable handles and structs containing them are not valid map keys: a key's equality must not change while a collection holds it.",
            "use a value built only from `Bool`, `Int`, `Str`, `Duration`, `Unit`, a range, arrays, structs, enum cases, `Map`, or `Set` — all free of mutable handles",
        )
    };
    RuntimeError::new(message).with_rule(rule).with_help(help)
}

/// A value in a case its layout does not have.
///
/// [`super::equal`] answers the same event in the same words: a case index is
/// read out of word 0, and one the table cannot name is a lowering bug.
fn wrong_case(name: &str) -> RuntimeError {
    RuntimeError::new(format!("this `{name}` is in a case it does not have"))
}

/// A value reached the ordering without being something a key may be.
///
/// Not the oracle's, and not reachable from a checked program: every word
/// compared here either passed [`check`] or was written into a sorted run by
/// something that did. It is written out because "should never" is not
/// "cannot", and a silent wrong answer from a comparison costs more than the
/// arm that reports one.
fn not_a_key() -> RuntimeError {
    RuntimeError::new("this value cannot be a map key or a set element")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::make;
    use crate::vm::builtins::tests::{
        at, elements as array_layout, named, scalar, two_case, world,
    };
    use crate::vm::exec::tests::Build;

    fn machine(program: &cove_ir::Program) -> Machine<'_> {
        Machine::new(program, 1 << 14)
    }

    /// A run object of `layout` holding `words`, whose header counts the
    /// elements the stride divides them into.
    fn run_of(machine: &mut Machine, layout: LayoutId, elem: LayoutId, words: &[u64]) -> u64 {
        let stride = machine.words_of(elem).max(1) as usize;
        let addr = machine
            .new_object(layout, (words.len() / stride) as u32)
            .expect("the fixture's heap is large enough");
        machine.set_payload_run(addr, 0, words);
        addr
    }

    fn array(machine: &mut Machine, elem: LayoutId, words: &[u64]) -> u64 {
        let layout = array_layout(machine.program(), elem, false);
        run_of(machine, layout, elem, words)
    }

    fn set(machine: &mut Machine, elem: LayoutId, words: &[u64]) -> u64 {
        let layout = make::members(machine.program(), elem).expect("the fixture declares a `Set`");
        run_of(machine, layout, elem, words)
    }

    fn map(machine: &mut Machine, key: LayoutId, value: LayoutId, words: &[u64]) -> u64 {
        let layout =
            make::entries(machine.program(), key, value).expect("the fixture declares a `Map`");
        let stride = machine.words_of(key) + machine.words_of(value);
        let addr = machine
            .new_object(layout, words.len() as u32 / stride)
            .expect("the fixture's heap is large enough");
        machine.set_payload_run(addr, 0, words);
        addr
    }

    /// A `Boxed` object holding the words of a value of `held`.
    ///
    /// Payload word 0 is the layout and the words after it are the value
    /// inline, and the header's length is the width — which the `Boxed`
    /// layout itself cannot know.
    fn boxed(machine: &mut Machine, held: LayoutId, words: &[u64]) -> u64 {
        let layout = named(machine.program(), "Boxed");
        let addr = machine
            .new_object(layout, words.len() as u32)
            .expect("the fixture's heap is large enough");
        machine.set_payload(addr, 0, held.0 as u64);
        machine.set_payload_run(addr, 1, words);
        addr
    }

    /// Two operands, which is what an argument and an argument are.
    /// An operand naming the object at `addr`, whose layout its own header
    /// states — which is what a lowering would have put in the argument.
    fn object<'w>(machine: &Machine, addr: &'w u64) -> Operand<'w> {
        at(machine.object_layout(*addr), std::slice::from_ref(addr))
    }

    fn cmp(machine: &Machine, a: (Repr, u64), b: (Repr, u64)) -> Ordering {
        order(machine, Key::Word(a.0, a.1), Key::Word(b.0, b.1), 0).expect("both are keys")
    }

    /// The ranks, in the order [`self`]'s table gives them, each family
    /// against each of the ones it must sort before.
    ///
    /// The three inline families are compared as values of a known layout,
    /// because that is the only way one can be reached: a struct, an enum and
    /// a `Range` are runs of words rather than addresses.
    #[test]
    fn a_family_sorts_where_its_variant_is_declared() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let point = named(&program, "Point");
        let range = named(&program, "Range");
        let option = two_case(&program, "Option", "Some", int);

        let text = machine.new_string("a").unwrap();
        let some = boxed(&mut machine, option, &[1, 1]);
        let structure = boxed(&mut machine, point, &[1, 2]);
        let items = array(&mut machine, int, &[1]);
        let members = set(&mut machine, int, &[1]);
        let entries = map(&mut machine, int, int, &[1, 2]);
        let bounds = boxed(&mut machine, range, &[0, 3, 0]);

        let ordered = [
            (Repr::Unit, 0),
            (Repr::Bool, 1),
            (Repr::Int, 7),
            (Repr::Duration, 7),
            (Repr::Ref, text),
            (Repr::Ref, some),
            (Repr::Ref, structure),
            (Repr::Ref, items),
            (Repr::Ref, members),
            (Repr::Ref, entries),
            (Repr::Ref, bounds),
        ];
        for (at, one) in ordered.iter().enumerate() {
            for other in &ordered[at + 1..] {
                assert_eq!(
                    cmp(&machine, *one, *other),
                    Ordering::Less,
                    "{one:?} sorts before {other:?}"
                );
                assert_eq!(cmp(&machine, *other, *one), Ordering::Greater);
            }
            assert_eq!(cmp(&machine, *one, *one), Ordering::Equal);
        }
    }

    /// An `Int` and a `Duration` are the same sixty-four bits and are never
    /// compared as numbers: the families decide first, so every `Int` sorts
    /// before every `Duration` however large it is.
    #[test]
    fn a_duration_is_not_an_int_that_happens_to_be_larger() {
        let program = world();
        let machine = machine(&program);
        assert_eq!(
            cmp(&machine, (Repr::Int, 9_000), (Repr::Duration, 1)),
            Ordering::Less
        );
    }

    #[test]
    fn a_scalar_orders_by_its_value() {
        let program = world();
        let machine = machine(&program);
        assert_eq!(
            cmp(&machine, (Repr::Int, (-2i64) as u64), (Repr::Int, 1)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Bool, 0), (Repr::Bool, 1)),
            Ordering::Less
        );
        assert_eq!(
            cmp(
                &machine,
                (Repr::Duration, (-1i64) as u64),
                (Repr::Duration, 0)
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Unit, 0), (Repr::Unit, 0)),
            Ordering::Equal
        );
    }

    /// Byte-wise, which is what `String`'s `Ord` is: `"Z"` before `"a"`, and
    /// a prefix before what extends it.
    #[test]
    fn a_string_orders_by_its_bytes() {
        let program = world();
        let mut machine = machine(&program);
        let upper = machine.new_string("Z").unwrap();
        let lower = machine.new_string("a").unwrap();
        let long = machine.new_string("ab").unwrap();
        assert_eq!(
            cmp(&machine, (Repr::Ref, upper), (Repr::Ref, lower)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, lower), (Repr::Ref, long)),
            Ordering::Less
        );
    }

    /// A case is ordered by the enum's name, then the case's, then the
    /// payload — so `None` sorts before `Some` because `"None" < "Some"`,
    /// and not because of where the layout lists them.
    ///
    /// An enum is inline, so what is compared is two runs of words read as
    /// the `Option` they belong to.
    #[test]
    fn an_enum_orders_by_name_then_case_then_payload() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let option = two_case(&program, "Option", "Some", int);
        let result = two_case(&program, "Result", "Ok", int);

        let none = make::none(&mut machine, int).unwrap();
        let one = make::some(&mut machine, int, &[1]).unwrap();
        let two = make::some(&mut machine, int, &[2]).unwrap();
        assert_eq!(
            cmp_value(&machine, option, &none, &one).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            cmp_value(&machine, option, &one, &two).unwrap(),
            Ordering::Less
        );

        // `"Option" < "Result"`, whatever either carries — and the two are
        // different layouts, so this is the comparison a box makes.
        let ok = make::ok(&mut machine, int, &[1]).unwrap();
        let held = boxed(&mut machine, option, &two);
        let other = boxed(&mut machine, result, &ok);
        assert_eq!(
            cmp(&machine, (Repr::Ref, held), (Repr::Ref, other)),
            Ordering::Less
        );
    }

    #[test]
    fn a_struct_orders_by_name_then_field_by_field() {
        let program = world();
        let machine = machine(&program);
        let point = named(&program, "Point");
        assert_eq!(
            cmp_value(&machine, point, &[0, 0], &[0, 1]).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            cmp_value(&machine, point, &[0, 1], &[1, 0]).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            cmp_value(&machine, point, &[0, 0], &[0, 0]).unwrap(),
            Ordering::Equal
        );
    }

    /// Lexicographic, and a prefix sorts before what extends it — `Vec`'s own
    /// order, which is what `MapKey::Array` derives.
    #[test]
    fn a_sequence_orders_element_by_element_and_then_by_length() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let short = array(&mut machine, int, &[1]);
        let long = array(&mut machine, int, &[1, 0]);
        let larger = array(&mut machine, int, &[2]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, short), (Repr::Ref, long)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, long), (Repr::Ref, larger)),
            Ordering::Less
        );

        // A set and a map are the same comparison over the runs they keep in
        // ascending order.
        let one = set(&mut machine, int, &[1]);
        let both = set(&mut machine, int, &[1, 2]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, one), (Repr::Ref, both)),
            Ordering::Less
        );
        let low = map(&mut machine, int, int, &[1, 1]);
        let high = map(&mut machine, int, int, &[1, 2]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, low), (Repr::Ref, high)),
            Ordering::Less
        );
    }

    /// An element is a run of words at the element layout's width, so a
    /// two-word member is compared as a `Point` and not as the two integers
    /// it is made of.
    #[test]
    fn a_run_of_multiword_elements_is_walked_at_its_stride() {
        let program = world();
        let mut machine = machine(&program);
        let point = named(&program, "Point");
        let low = array(&mut machine, point, &[1, 2, 9, 9]);
        let high = array(&mut machine, point, &[1, 3, 0, 0]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, low), (Repr::Ref, high)),
            Ordering::Less
        );
        // The second element decides only when the first does not.
        let same = array(&mut machine, point, &[1, 2, 9, 9]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, low), (Repr::Ref, same)),
            Ordering::Equal
        );
    }

    /// `1..3` and `1..<3` are two values, and the exclusive one sorts first
    /// because `false < true`.
    #[test]
    fn a_range_orders_by_the_bounds_it_was_written_with() {
        let program = world();
        let machine = machine(&program);
        let range = named(&program, "Range");
        assert_eq!(
            cmp_value(&machine, range, &[1, 3, 0], &[1, 3, 1]).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            cmp_value(&machine, range, &[1, 3, 1], &[2, 3, 0]).unwrap(),
            Ordering::Less
        );
    }

    /// Erasure is looked through on either side, so where the checker put a
    /// `dyn` wrapper is not something the order can tell — and a box records
    /// the *layout* of what it holds, so a boxed `Point` is two words rather
    /// than an address to two more.
    #[test]
    fn a_box_orders_as_what_it_holds() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let point = named(&program, "Point");
        let boxes = named(&program, "Boxed");
        let held = boxed(&mut machine, int, &[3]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, held), (Repr::Int, 4)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Int, 3), (Repr::Ref, held)),
            Ordering::Equal
        );
        check(&machine, "Set.of", SET_ELEMENT, at(boxes, &[held])).unwrap();

        // A boxed `Point` is a `Point`, so it sorts among the structs and a
        // member of a `Set<Point>` finds it.
        let structure = boxed(&mut machine, point, &[1, 2]);
        assert_eq!(
            cmp_held(&machine, point, &[1, 2], at(boxes, &[structure])).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            cmp_held(&machine, point, &[1, 1], at(boxes, &[structure])).unwrap(),
            Ordering::Less
        );
    }

    /// A `Float` is refused with the rule that is its own, and a `Vector`
    /// with the rule about mutable handles.
    #[test]
    fn a_float_and_a_vector_are_refused_in_the_oracles_words() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let floats = scalar(&program, Repr::Float);
        let error = check(
            &machine,
            "Set.of",
            SET_ELEMENT,
            at(floats, &[1.5f64.to_bits()]),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` as a set element"
        );
        assert_eq!(
            error.rule.as_deref(),
            Some("A `Float` cannot be a map key or set element: `NaN` is not equal to itself, which breaks the total order every key needs.")
        );

        let items = make::vector_of(&mut machine, int, &[1]).unwrap();
        let error = check(&machine, "Map.get", MAP_KEY, object(&machine, &items)).unwrap_err();
        assert_eq!(
            error.message,
            "`Map.get` cannot use a `Vector` as a map key"
        );
        assert_eq!(
            error.rule.as_deref(),
            Some("Mutable handles and structs containing them are not valid map keys: a key's equality must not change while a collection holds it.")
        );
    }

    /// The path names the part that cannot be a key rather than blaming the
    /// whole value, and it is built the way `MapKey::convert` builds one: a
    /// struct anchors on its own name, an enum on its name and case, an array
    /// and a map on nothing at all.
    #[test]
    fn a_refusal_names_the_nested_part_it_is_about() {
        let mut build = Build::default();
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;
        let int = build.word("Int", Repr::Int);
        let float = build.word("Float", Repr::Float);
        let held = build.structure("Held", &[("tag", int), ("weight", float)]);
        build.layout(
            "Array",
            Shape::Elements {
                elem: held,
                growable: false,
            },
        );
        let option = build.enumeration("Option", &[("None", vec![]), ("Some", vec![float])]);
        build.layout(
            "Map",
            Shape::Entries {
                key: int,
                value: float,
            },
        );
        let program = build.done();
        let mut machine = machine(&program);

        let error = check_value(
            &machine,
            "Set.of",
            SET_ELEMENT,
            held,
            &[1, 1.5f64.to_bits()],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` inside `Held.weight` as a set element"
        );

        // An array at the root anchors on nothing, so the path is the index
        // alone — and a struct inside it extends that. The elements are
        // inline, at the `Held` layout's width.
        let items = array(&mut machine, held, &[1, 1.5f64.to_bits()]);
        let error = check(&machine, "Set.of", SET_ELEMENT, object(&machine, &items)).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` inside `[0].weight` as a set element"
        );

        let error = check_value(
            &machine,
            "Map.inserted",
            MAP_KEY,
            option,
            &[1, 1.5f64.to_bits()],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Map.inserted` cannot use a `Float` inside `Option.Some(0)` as a map key"
        );

        // A map's *values* are what nesting one as a key still asks about,
        // and the entry is named by the key as it renders.
        let entries = map(&mut machine, int, float, &[7, 1.5f64.to_bits()]);
        let error = check(&machine, "Set.of", SET_ELEMENT, object(&machine, &entries)).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` inside `[7]` as a set element"
        );
    }

    /// A set's members and a map's keys are keys by construction, so nesting
    /// one never asks again.
    #[test]
    fn a_nested_set_is_admitted_without_walking_it() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        let members = set(&mut machine, int, &[1, 2]);
        check(&machine, "Set.of", SET_ELEMENT, object(&machine, &members)).unwrap();
        let entries = map(&mut machine, int, int, &[1, 2]);
        check(&machine, "Set.of", SET_ELEMENT, object(&machine, &entries)).unwrap();
    }

    /// An object that holds itself is a legal heap graph and not a legal
    /// key, so both halves stop rather than running out of native stack.
    #[test]
    fn a_cycle_stops_rather_than_recursing_forever() {
        let program = world();
        let mut machine = machine(&program);
        let text = program.str_layout;
        let a = array(&mut machine, text, &[0]);
        machine.set_payload(a, 0, a);
        let b = array(&mut machine, text, &[0]);
        machine.set_payload(b, 0, b);
        let error = check(&machine, "Set.of", SET_ELEMENT, object(&machine, &a)).unwrap_err();
        assert_eq!(error.message, "this value nests too deeply to compare");
        let error = order(
            &machine,
            Key::Word(Repr::Ref, a),
            Key::Word(Repr::Ref, b),
            0,
        )
        .unwrap_err();
        assert_eq!(error.message, "this value nests too deeply to compare");
    }

    /// The two things only this representation can go wrong at.
    #[test]
    fn a_null_or_reclaimed_reference_is_refused() {
        let program = world();
        let mut machine = machine(&program);
        let int = scalar(&program, Repr::Int);
        // The layout is the one a lowering would have passed — the static
        // type of the argument — and the word is what went wrong: a null
        // reference, and then an address the sweeper reclaimed underneath it.
        let arrays = array_layout(&program, int, false);
        let null = 0;
        let error = check(&machine, "Set.of", SET_ELEMENT, at(arrays, &[null])).unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");

        let dead = array(&mut machine, int, &[]);
        machine.relabel(dead, LayoutId::FREE, 0, 0);
        let error = check(&machine, "Set.of", SET_ELEMENT, at(arrays, &[dead])).unwrap_err();
        assert_eq!(error.message, "this value was read after it was reclaimed");
        let error = order(
            &machine,
            Key::Word(Repr::Ref, dead),
            Key::Word(Repr::Ref, dead),
            0,
        )
        .unwrap_err();
        assert_eq!(error.message, "this value was read after it was reclaimed");
    }
}
