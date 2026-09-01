//! The order a `Map` and a `Set` are kept in, over words and heap objects.
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
//! | 5 | `EnumCase` | a [`Shape::Enum`] object |
//! | 6 | `Struct` | a [`Shape::Struct`] object that is not the `Range` |
//! | 7 | `Array` | a [`Shape::Elements`] object that cannot grow |
//! | 8 | `Set` | a [`Shape::Members`] object |
//! | 9 | `Map` | a [`Shape::Entries`] object |
//! | 10 | `Range` | the program's `Range` struct |
//!
//! Two of those are worth saying out loud, because the representation would
//! suggest otherwise. A `Range` is a struct in the heap and sorts *after*
//! every other family rather than among the structs, because `MapKey` declares
//! it last. An `Int` and a `Duration` are the same sixty-four bits and are
//! never compared as numbers: every `Int` sorts before every `Duration`.
//!
//! # A name here is the layout's, and the oracle's is the declaration's
//!
//! `MapKey::Struct` and `MapKey::EnumCase` are keyed by the type name the
//! *value* carries, which is qualified — `rules.policy.Decision`. A
//! [`cove_lir::Layout`] carries the unqualified name, which is also the name
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
//! [`check`] is that question asked of a word, and its refusals are
//! [`crate::builtins`]' word for word, path included.

use std::cmp::Ordering;
use std::fmt::Write as _;

use cove_lir::{Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::boundary::is_range;
use crate::lvm::builtins::operand::{self, Operand};
use crate::lvm::builtins::{equal, render};
use crate::lvm::exec::Machine;

/// What a `Map`'s key argument is called in a refusal.
pub(super) const MAP_KEY: &str = "map key";

/// What a `Set`'s element argument is called in one.
pub(super) const SET_ELEMENT: &str = "set element";

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
    operand: Operand,
) -> Result<(), RuntimeError> {
    admits(machine, method, role, None, operand, 0)
}

/// Where `a` sorts relative to `b`.
///
/// Both are keys: every word that reaches this either passed [`check`] or was
/// written into a sorted run by something that did.
pub(super) fn compare(machine: &Machine, a: Operand, b: Operand) -> Result<Ordering, RuntimeError> {
    order(machine, a, b, 0)
}

// --- admitting a key -------------------------------------------------------

/// `anchor` is the path to this operand from the value that was asked about,
/// so that a refusal several levels down names the part that is wrong rather
/// than blaming the whole struct. `None` at the root: a bare value has no name
/// to anchor a path to, and a struct or an enum invents one from its own type
/// name the first time a path is needed.
fn admits(
    machine: &Machine,
    method: &str,
    role: &str,
    anchor: Option<&str>,
    operand: Operand,
    depth: usize,
) -> Result<(), RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(equal::too_deep());
    }
    let deeper = depth + 1;
    // Erasure is looked through before anything else, as `MapKey::convert`
    // looks through `value.erased()` first: two values `==` calls equal have
    // to be usable as one key, and equality already looks through a `dyn`.
    if let Some(inner) = equal::unboxed(machine, operand) {
        return admits(machine, method, role, anchor, inner, deeper);
    }
    let (repr, word) = operand;
    match repr {
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
    }
}

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
        // A `Range` is an immutable value with a stable equality, so it is a
        // key like any other and there is nothing inside it to walk.
        Shape::Struct { fields, .. } if is_range(&layout.name, fields) => Ok(()),
        Shape::Struct { fields, .. } => {
            let base = path(anchor, || layout.name.to_string());
            for (at, field) in fields.iter().enumerate() {
                let word = machine.payload(addr, at as u32);
                let anchor = format!("{base}.{}", field.name);
                admits(
                    machine,
                    method,
                    role,
                    Some(&anchor),
                    (field.repr, word),
                    depth,
                )?;
            }
            Ok(())
        }
        Shape::Enum { cases } => {
            let index = machine.payload(addr, 0);
            let case = cases
                .get(index as usize)
                .ok_or_else(|| wrong_case(&layout.name))?;
            let base = path(anchor, || format!("{}.{}", layout.name, case.name));
            for (at, repr) in case.payload.iter().enumerate() {
                let word = machine.payload(addr, 1 + at as u32);
                let anchor = format!("{base}({at})");
                admits(machine, method, role, Some(&anchor), (*repr, word), depth)?;
            }
            Ok(())
        }
        // An array is fixed-length and immutable, so its equality cannot
        // change and every element decides for itself. A growable run is a
        // `Vector`'s store, and refusing it is refusing the vector.
        Shape::Elements {
            elem,
            growable: false,
        } => {
            let base = path(anchor, String::new);
            for at in 0..machine.object_len(addr) {
                let word = machine.payload(addr, at);
                let anchor = format!("{base}[{at}]");
                admits(machine, method, role, Some(&anchor), (*elem, word), depth)?;
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
        Shape::Entries { key, value } => {
            let base = path(anchor, String::new);
            for at in 0..machine.object_len(addr) {
                let shown = render(machine, *key, machine.payload(addr, at * 2), 0)?;
                let word = machine.payload(addr, at * 2 + 1);
                let anchor = format!("{base}[{shown}]");
                admits(machine, method, role, Some(&anchor), (*value, word), depth)?;
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

/// The anchor a nested part is reached through, or the one this value invents
/// for itself when it is the root.
fn path(anchor: Option<&str>, own: impl FnOnce() -> String) -> String {
    match anchor {
        Some(anchor) => anchor.to_string(),
        None => own(),
    }
}

// --- ordering two keys -----------------------------------------------------

fn order(
    machine: &Machine,
    a: Operand,
    b: Operand,
    depth: usize,
) -> Result<Ordering, RuntimeError> {
    if depth >= super::MAX_DEPTH {
        return Err(equal::too_deep());
    }
    let deeper = depth + 1;
    if let Some(inner) = equal::unboxed(machine, a) {
        return order(machine, inner, b, deeper);
    }
    if let Some(inner) = equal::unboxed(machine, b) {
        return order(machine, a, inner, deeper);
    }
    let (x, y) = (family(machine, a)?, family(machine, b)?);
    match x.rank().cmp(&y.rank()) {
        Ordering::Equal => {}
        other => return Ok(other),
    }
    let program = machine.program();
    Ok(match (x, y) {
        (Family::Unit, Family::Unit) => Ordering::Equal,
        (Family::Bool(a), Family::Bool(b)) => a.cmp(&b),
        (Family::Int(a), Family::Int(b)) => a.cmp(&b),
        (Family::Duration(a), Family::Duration(b)) => a.cmp(&b),
        // Byte-wise, which is what `String`'s own `Ord` is.
        (Family::Str(a), Family::Str(b)) => machine.string_bytes(a).cmp(&machine.string_bytes(b)),
        // Type name, then case name, then payload — and the case is read out
        // of the object, because which payload words are anything at all
        // depends on the case it is in.
        (Family::Case(a), Family::Case(b)) => {
            let (left, right) = (
                program.layout(machine.object_layout(a)),
                program.layout(machine.object_layout(b)),
            );
            let (Shape::Enum { cases: x }, Shape::Enum { cases: y }) = (&left.shape, &right.shape)
            else {
                unreachable!("`family` answers `Case` for an enum-shaped object");
            };
            let one = x
                .get(machine.payload(a, 0) as usize)
                .ok_or_else(|| wrong_case(&left.name))?;
            let other = y
                .get(machine.payload(b, 0) as usize)
                .ok_or_else(|| wrong_case(&right.name))?;
            match (*left.name)
                .cmp(&right.name)
                .then_with(|| (*one.name).cmp(&other.name))
            {
                Ordering::Equal => {}
                ordered => return Ok(ordered),
            }
            let words = |addr: u64, case: &cove_lir::Case| -> Vec<Operand> {
                case.payload
                    .iter()
                    .enumerate()
                    .map(|(at, repr)| (*repr, machine.payload(addr, 1 + at as u32)))
                    .collect()
            };
            return runs(machine, &words(a, one), &words(b, other), deeper);
        }
        // Type name, then the fields as pairs of name and value, then how
        // many there are, then whether the declaration was opaque. That is
        // `MapKey::Struct`'s derived order field for field: it carries
        // `(String, Vec<(String, MapKey)>, bool)` and compares them in that
        // order.
        (Family::Struct(a), Family::Struct(b)) => {
            let (left, right) = (
                program.layout(machine.object_layout(a)),
                program.layout(machine.object_layout(b)),
            );
            let (
                Shape::Struct {
                    fields: x,
                    opaque: a_opaque,
                },
                Shape::Struct {
                    fields: y,
                    opaque: b_opaque,
                },
            ) = (&left.shape, &right.shape)
            else {
                unreachable!("`family` answers `Struct` for a struct-shaped object");
            };
            match (*left.name).cmp(&right.name) {
                Ordering::Equal => {}
                ordered => return Ok(ordered),
            }
            for (at, (one, other)) in x.iter().zip(y).enumerate() {
                let at = at as u32;
                match (*one.name).cmp(&other.name) {
                    Ordering::Equal => {}
                    ordered => return Ok(ordered),
                }
                match order(
                    machine,
                    (one.repr, machine.payload(a, at)),
                    (other.repr, machine.payload(b, at)),
                    deeper,
                )? {
                    Ordering::Equal => {}
                    ordered => return Ok(ordered),
                }
            }
            x.len().cmp(&y.len()).then(a_opaque.cmp(b_opaque))
        }
        (Family::Array(a), Family::Array(b)) => {
            return runs(
                machine,
                &elements(machine, a),
                &elements(machine, b),
                deeper,
            )
        }
        // A set's members are already ascending, so two sets compare member
        // for member — which is what `BTreeSet`'s `Ord` does with the same
        // two runs.
        (Family::Set(a), Family::Set(b)) => {
            return runs(
                machine,
                &elements(machine, a),
                &elements(machine, b),
                deeper,
            )
        }
        // And a map compares entry for entry, key before value, which is
        // `BTreeMap`'s `Ord` over its ascending pairs.
        (Family::Map(a), Family::Map(b)) => {
            return runs(machine, &pairs(machine, a), &pairs(machine, b), deeper)
        }
        // The bounds as they were written, in the order `MapKey::Range`
        // declares its fields: an inclusive range sorts after the exclusive
        // one with the same two numbers, because `false < true`.
        (Family::Range(a), Family::Range(b)) => {
            let word = |addr: u64, at: u32| machine.payload(addr, at) as i64;
            word(a, 0)
                .cmp(&word(b, 0))
                .then_with(|| word(a, 1).cmp(&word(b, 1)))
                .then_with(|| (word(a, 2) != 0).cmp(&(word(b, 2) != 0)))
        }
        _ => unreachable!("two families of one rank are one family"),
    })
}

/// Lexicographic order over two runs, the shorter first when one is a prefix
/// of the other — which is `Vec`'s own `Ord` and therefore every `MapKey`
/// variant that holds one.
fn runs(
    machine: &Machine,
    left: &[Operand],
    right: &[Operand],
    depth: usize,
) -> Result<Ordering, RuntimeError> {
    for (a, b) in left.iter().zip(right) {
        match order(machine, *a, *b, depth)? {
            Ordering::Equal => {}
            ordered => return Ok(ordered),
        }
    }
    Ok(left.len().cmp(&right.len()))
}

/// The elements of an array or the members of a set, as operands.
fn elements(machine: &Machine, addr: u64) -> Vec<Operand> {
    let elem = match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements { elem, .. } | Shape::Members { elem } => elem,
        _ => unreachable!("`family` answers a run for a run-shaped object"),
    };
    (0..machine.object_len(addr))
        .map(|at| (elem, machine.payload(addr, at)))
        .collect()
}

/// The entries of a map, key before value, in the ascending order it holds
/// them in.
fn pairs(machine: &Machine, addr: u64) -> Vec<Operand> {
    let Shape::Entries { key, value } = machine.program().layout(machine.object_layout(addr)).shape
    else {
        unreachable!("`family` answers `Map` for an entries-shaped object");
    };
    (0..machine.object_len(addr))
        .flat_map(|at| {
            [
                (key, machine.payload(addr, at * 2)),
                (value, machine.payload(addr, at * 2 + 1)),
            ]
        })
        .collect()
}

/// Which of the eleven shapes a key may take this one is.
///
/// The scalars carry their value because the word is the whole of them; the
/// rest carry the object, because what they hold is read out of it.
enum Family {
    Unit,
    Bool(bool),
    Int(i64),
    Duration(i64),
    Str(u64),
    Case(u64),
    Struct(u64),
    Array(u64),
    Set(u64),
    Map(u64),
    Range(u64),
}

impl Family {
    /// Where this family sits in the one order. See the table in [`self`].
    fn rank(&self) -> u8 {
        match self {
            Family::Unit => 0,
            Family::Bool(_) => 1,
            Family::Int(_) => 2,
            Family::Duration(_) => 3,
            Family::Str(_) => 4,
            Family::Case(_) => 5,
            Family::Struct(_) => 6,
            Family::Array(_) => 7,
            Family::Set(_) => 8,
            Family::Map(_) => 9,
            Family::Range(_) => 10,
        }
    }
}

fn family(machine: &Machine, operand: Operand) -> Result<Family, RuntimeError> {
    let (repr, word) = operand;
    match repr {
        Repr::Unit => return Ok(Family::Unit),
        Repr::Bool => return Ok(Family::Bool(word != 0)),
        Repr::Int => return Ok(Family::Int(word as i64)),
        Repr::Duration => return Ok(Family::Duration(word as i64)),
        Repr::Ref => {}
        _ => return Err(not_a_key()),
    }
    if word == 0 {
        return Err(operand::null_value());
    }
    let layout = machine.program().layout(machine.object_layout(word));
    Ok(match &layout.shape {
        Shape::Str => Family::Str(word),
        Shape::Free => return Err(operand::reclaimed()),
        Shape::Struct { fields, .. } if is_range(&layout.name, fields) => Family::Range(word),
        Shape::Struct { .. } => Family::Struct(word),
        Shape::Enum { .. } => Family::Case(word),
        Shape::Elements {
            growable: false, ..
        } => Family::Array(word),
        Shape::Members { .. } => Family::Set(word),
        Shape::Entries { .. } => Family::Map(word),
        _ => return Err(not_a_key()),
    })
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

/// An object in a case its layout does not have.
///
/// [`super::equal`] answers the same event in the same words: a case index is
/// read out of the object, and one the table cannot name is a lowering bug.
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
    use crate::lvm::builtins::make;
    use crate::lvm::builtins::tests::{elements as array_layout, named, world};
    use crate::lvm::exec::tests::Build;
    use cove_lir::{Field, LayoutId, Shape};
    use std::sync::Arc;

    fn machine(program: &cove_lir::Program) -> Machine<'_> {
        Machine::new(program, 1 << 14)
    }

    /// An object of `layout` holding `words`.
    fn object(machine: &mut Machine, layout: LayoutId, words: &[u64]) -> u64 {
        let addr = machine
            .new_object(layout, words.len() as u32)
            .expect("the fixture's heap is large enough");
        for (at, word) in words.iter().enumerate() {
            machine.set_payload(addr, at as u32, *word);
        }
        addr
    }

    fn array(machine: &mut Machine, elem: Repr, words: &[u64]) -> u64 {
        let layout = array_layout(machine.program(), elem, false);
        object(machine, layout, words)
    }

    fn set(machine: &mut Machine, elem: Repr, words: &[u64]) -> u64 {
        let layout = make::members(machine.program(), elem).expect("the fixture declares a `Set`");
        object(machine, layout, words)
    }

    fn map(machine: &mut Machine, key: Repr, value: Repr, words: &[u64]) -> u64 {
        let layout =
            make::entries(machine.program(), key, value).expect("the fixture declares a `Map`");
        let addr = machine
            .new_object(layout, words.len() as u32 / 2)
            .expect("the fixture's heap is large enough");
        for (at, word) in words.iter().enumerate() {
            machine.set_payload(addr, at as u32, *word);
        }
        addr
    }

    fn point(machine: &mut Machine, x: i64, y: i64) -> u64 {
        let layout = named(machine.program(), "Point");
        object(machine, layout, &[x as u64, y as u64])
    }

    fn range(machine: &mut Machine, start: i64, end: i64, inclusive: bool) -> u64 {
        let layout = named(machine.program(), "Range");
        object(
            machine,
            layout,
            &[start as u64, end as u64, inclusive as u64],
        )
    }

    fn cmp(machine: &Machine, a: Operand, b: Operand) -> Ordering {
        compare(machine, a, b).expect("both are keys")
    }

    /// The ranks, in the order [`self`]'s table gives them, each family
    /// against each of the ones it must sort before.
    #[test]
    fn a_family_sorts_where_its_variant_is_declared() {
        let program = world();
        let mut machine = machine(&program);
        let text = machine.new_string("a").unwrap();
        let some = make::some(&mut machine, Repr::Int, 1).unwrap();
        let structure = point(&mut machine, 1, 2);
        let items = array(&mut machine, Repr::Int, &[1]);
        let members = set(&mut machine, Repr::Int, &[1]);
        let entries = map(&mut machine, Repr::Int, Repr::Int, &[1, 2]);
        let bounds = range(&mut machine, 0, 3, false);

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
    #[test]
    fn an_enum_orders_by_name_then_case_then_payload() {
        let program = world();
        let mut machine = machine(&program);
        let none = make::none(&mut machine, Repr::Int).unwrap();
        let one = make::some(&mut machine, Repr::Int, 1).unwrap();
        let two = make::some(&mut machine, Repr::Int, 2).unwrap();
        let ok = make::ok(&mut machine, Repr::Int, 1).unwrap();
        assert_eq!(
            cmp(&machine, (Repr::Ref, none), (Repr::Ref, one)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, one), (Repr::Ref, two)),
            Ordering::Less
        );
        // `"Option" < "Result"`, whatever either carries.
        assert_eq!(
            cmp(&machine, (Repr::Ref, two), (Repr::Ref, ok)),
            Ordering::Less
        );
    }

    #[test]
    fn a_struct_orders_by_name_then_field_by_field() {
        let program = world();
        let mut machine = machine(&program);
        let origin = point(&mut machine, 0, 0);
        let up = point(&mut machine, 0, 1);
        let over = point(&mut machine, 1, 0);
        assert_eq!(
            cmp(&machine, (Repr::Ref, origin), (Repr::Ref, up)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, up), (Repr::Ref, over)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, origin), (Repr::Ref, origin)),
            Ordering::Equal
        );
    }

    /// Lexicographic, and a prefix sorts before what extends it — `Vec`'s own
    /// order, which is what `MapKey::Array` derives.
    #[test]
    fn a_sequence_orders_element_by_element_and_then_by_length() {
        let program = world();
        let mut machine = machine(&program);
        let short = array(&mut machine, Repr::Int, &[1]);
        let long = array(&mut machine, Repr::Int, &[1, 0]);
        let larger = array(&mut machine, Repr::Int, &[2]);
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
        let one = set(&mut machine, Repr::Int, &[1]);
        let both = set(&mut machine, Repr::Int, &[1, 2]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, one), (Repr::Ref, both)),
            Ordering::Less
        );
        let low = map(&mut machine, Repr::Int, Repr::Int, &[1, 1]);
        let high = map(&mut machine, Repr::Int, Repr::Int, &[1, 2]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, low), (Repr::Ref, high)),
            Ordering::Less
        );
    }

    /// `1..3` and `1..<3` are two values, and the exclusive one sorts first
    /// because `false < true`.
    #[test]
    fn a_range_orders_by_the_bounds_it_was_written_with() {
        let program = world();
        let mut machine = machine(&program);
        let exclusive = range(&mut machine, 1, 3, false);
        let inclusive = range(&mut machine, 1, 3, true);
        let later = range(&mut machine, 2, 3, false);
        assert_eq!(
            cmp(&machine, (Repr::Ref, exclusive), (Repr::Ref, inclusive)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Ref, inclusive), (Repr::Ref, later)),
            Ordering::Less
        );
    }

    /// Erasure is looked through on either side, so where the checker put a
    /// `dyn` wrapper is not something the order can tell.
    #[test]
    fn a_box_orders_as_what_it_holds() {
        let program = world();
        let mut machine = machine(&program);
        let layout = named(machine.program(), "Boxed");
        let boxed = object(&mut machine, layout, &[Repr::Int.tag(), 3]);
        assert_eq!(
            cmp(&machine, (Repr::Ref, boxed), (Repr::Int, 4)),
            Ordering::Less
        );
        assert_eq!(
            cmp(&machine, (Repr::Int, 3), (Repr::Ref, boxed)),
            Ordering::Equal
        );
        check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, boxed)).unwrap();
    }

    /// A `Float` is refused with the rule that is its own, and a `Vector`
    /// with the rule about mutable handles.
    #[test]
    fn a_float_and_a_vector_are_refused_in_the_oracles_words() {
        let program = world();
        let mut machine = machine(&program);
        let error = check(
            &machine,
            "Set.of",
            SET_ELEMENT,
            (Repr::Float, 1.5f64.to_bits()),
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

        let items = make::vector_of(&mut machine, Repr::Int, &[1]).unwrap();
        let error = check(&machine, "Map.get", MAP_KEY, (Repr::Ref, items)).unwrap_err();
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
        build.layout(
            "Held",
            Shape::Struct {
                fields: vec![
                    Field {
                        name: Arc::from("tag"),
                        repr: Repr::Int,
                    },
                    Field {
                        name: Arc::from("weight"),
                        repr: Repr::Float,
                    },
                ],
                opaque: false,
            },
        );
        build.layout(
            "Array",
            Shape::Elements {
                elem: Repr::Ref,
                growable: false,
            },
        );
        build.layout(
            "Option",
            Shape::Enum {
                cases: vec![
                    cove_lir::Case {
                        name: Arc::from("None"),
                        payload: vec![],
                    },
                    cove_lir::Case {
                        name: Arc::from("Some"),
                        payload: vec![Repr::Float],
                    },
                ],
            },
        );
        build.layout(
            "Map",
            Shape::Entries {
                key: Repr::Int,
                value: Repr::Float,
            },
        );
        let program = build.done();
        let mut machine = machine(&program);

        let held = object(
            &mut machine,
            named(&program, "Held"),
            &[1, 1.5f64.to_bits()],
        );
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, held)).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` inside `Held.weight` as a set element"
        );

        // An array at the root anchors on nothing, so the path is the index
        // alone — and a struct inside it extends that.
        let items = object(&mut machine, named(&program, "Array"), &[held]);
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, items)).unwrap_err();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` inside `[0].weight` as a set element"
        );

        let some = object(
            &mut machine,
            named(&program, "Option"),
            &[1, 1.5f64.to_bits()],
        );
        let error = check(&machine, "Map.inserted", MAP_KEY, (Repr::Ref, some)).unwrap_err();
        assert_eq!(
            error.message,
            "`Map.inserted` cannot use a `Float` inside `Option.Some(0)` as a map key"
        );

        // A map's *values* are what nesting one as a key still asks about,
        // and the entry is named by the key as it renders.
        let entries = machine.new_object(named(&program, "Map"), 1).unwrap();
        machine.set_payload(entries, 0, 7);
        machine.set_payload(entries, 1, 1.5f64.to_bits());
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, entries)).unwrap_err();
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
        let members = set(&mut machine, Repr::Int, &[1, 2]);
        check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, members)).unwrap();
        let entries = map(&mut machine, Repr::Int, Repr::Int, &[1, 2]);
        check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, entries)).unwrap();
    }

    /// An object that holds itself is a legal heap graph and not a legal
    /// key, so both halves stop rather than running out of native stack.
    #[test]
    fn a_cycle_stops_rather_than_recursing_forever() {
        let program = world();
        let mut machine = machine(&program);
        let a = array(&mut machine, Repr::Ref, &[0]);
        machine.set_payload(a, 0, a);
        let b = array(&mut machine, Repr::Ref, &[0]);
        machine.set_payload(b, 0, b);
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, a)).unwrap_err();
        assert_eq!(error.message, "this value nests too deeply to compare");
        let error = compare(&machine, (Repr::Ref, a), (Repr::Ref, b)).unwrap_err();
        assert_eq!(error.message, "this value nests too deeply to compare");
    }

    /// The two things only this representation can go wrong at.
    #[test]
    fn a_null_or_reclaimed_reference_is_refused() {
        let program = world();
        let mut machine = machine(&program);
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, 0)).unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");

        let dead = array(&mut machine, Repr::Int, &[]);
        machine.relabel(dead, LayoutId::FREE, 0, 0);
        let error = check(&machine, "Set.of", SET_ELEMENT, (Repr::Ref, dead)).unwrap_err();
        assert_eq!(error.message, "this value was read after it was reclaimed");
        let error = compare(&machine, (Repr::Ref, dead), (Repr::Ref, dead)).unwrap_err();
        assert_eq!(error.message, "this value was read after it was reclaimed");
    }
}
