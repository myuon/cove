//! Where a word becomes a public [`Value`], and back.
//!
//! This is the only place in the linear-memory backend that knows what a
//! `Value` is. [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! keeps the materialised `Value` as the host and oracle boundary
//! representation and forbids it everywhere else: not as an operand store,
//! not as a call buffer, not as a spill area, not as a fallback path. Keeping
//! the conversion in a file of its own is how that stays checkable — if
//! anything else in `lvm` ever needs `Value`, it has to say so by importing
//! it, and the import is the thing to argue with.
//!
//! Three things cross here and nothing else does: a Host call's arguments and
//! answer, an entry's arguments and answer, and a trace capture.
//!
//! # A conversion needs the `Repr`
//!
//! A word is untagged, so nothing about `7` says whether it is an `Int`, a
//! `Duration` of seven nanoseconds, or a `Bool` that got there by a lowering
//! bug. What says so is the static metadata the word came from: a slot's
//! `Repr`, a host operation's declared result, a layout's field. The
//! conversion therefore takes the `Repr` as an argument rather than
//! inspecting the word, which is the same discipline the collector follows
//! and for the same reason.
//!
//! # The way in has to find a layout
//!
//! Going out, an object says what it is: the header names a [`LayoutId`] and
//! the table says the rest. Coming in there is no object yet, and `Repr::Ref`
//! says only that one is wanted — a struct, an array and a string are one
//! `Repr`. So the way in reads the *value*'s own description, which is what a
//! `Value` carries and a word does not: a struct's declared type name, an
//! enum's name and case, an array's elements. [`layout_for_struct`] and the
//! two beside it look that description up in the program's layout table,
//! which is a table of *families* — so `Array<Int>` and `Array<Duration>` are
//! told apart by whether the element `Repr` admits the elements, and two
//! layouts named `Option` by whether the case's payload admits the payload.
//!
//! A value the table describes no family for is refused by name. That is not
//! a gap in this file: a program that never mentions a `Point` has no `Point`
//! layout, and a host that hands one to it is handing it a type it does not
//! have.
//!
//! # Building an object is not atomic
//!
//! [`from_value`] allocates, and an allocation can collect. Between the
//! allocation of a struct and the write of its last field the object is
//! reachable only from a Rust local, which nothing walks. So each level of
//! the recursion takes a temporary root on the object it is filling in and
//! releases it on the way out — [`Machine::push_temp`] and
//! [`Machine::release_temps`]. Every allocation below the mark then happens
//! with the half-built object rooted, and no allocation at all happens
//! between the release and the write that gives the object its real owner.

use cove_lir::{Layout, LayoutId, Program, Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::exec::Machine;
use crate::value::{Value, ValueView, VectorStorage};

/// How deep a value may nest as it crosses.
///
/// A `Value` is a tree and a heap object graph is not: `SetWord` can make an
/// object hold itself, and a conversion that met one would recurse until the
/// native stack ran out. The limit is not a language fact and is not
/// reachable by any value a program can write down; it is here so that the
/// failure is a message rather than an abort.
const MAX_DEPTH: usize = 128;

/// The public value of `word`, read as `repr`.
pub(crate) fn to_value(machine: &Machine, repr: Repr, word: u64) -> Result<Value, RuntimeError> {
    out(machine, repr, word, 0)
}

fn out(machine: &Machine, repr: Repr, word: u64, depth: usize) -> Result<Value, RuntimeError> {
    Ok(match repr {
        Repr::Unit => Value::unit(),
        Repr::Bool => Value::bool(word != 0),
        Repr::Int => Value::int(word as i64),
        Repr::Float => Value::float(f64::from_bits(word)),
        Repr::Duration => Value::duration(word as i64),
        Repr::Ref => return object_to_value(machine, word, depth),
        // Neither can leave the machine. An address names a word of this
        // run's memory and means nothing outside it, and a host handle is
        // the host's to hand out — a boundary that received one back would
        // be receiving its own bookkeeping.
        Repr::Addr | Repr::Host => {
            return Err(RuntimeError::new(
                "this value cannot cross the boundary as it is represented",
            ))
        }
    })
}

/// The public value of the object at `addr`.
fn object_to_value(machine: &Machine, addr: u64, depth: usize) -> Result<Value, RuntimeError> {
    if addr == 0 {
        return Err(RuntimeError::new(
            "this value was read before it was given one",
        ));
    }
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let layout = machine.program().layout(machine.object_layout(addr));
    match &layout.shape {
        Shape::Str => {
            let bytes = machine.string_bytes(addr);
            let text = String::from_utf8(bytes).map_err(|_| {
                // Every string in the heap was written from a `&str` or from
                // a host's `String`, so this cannot happen without a bug in
                // whatever wrote it. Reporting it is cheaper than the
                // alternative, which is a `Value` holding invalid UTF-8.
                RuntimeError::new("this string's bytes are not valid UTF-8")
            })?;
            Ok(Value::string(text))
        }
        Shape::Struct { fields } => {
            let mut out_fields = Vec::with_capacity(fields.len());
            for (at, field) in fields.iter().enumerate() {
                let word = machine.payload(addr, at as u32);
                out_fields.push((
                    field.name.to_string(),
                    out(machine, field.repr, word, deeper)?,
                ));
            }
            Ok(Value::structure(&*layout.name, out_fields))
        }
        Shape::Enum { cases } => {
            // Word 0 is the case, and which of the payload words are anything
            // at all depends on it. The object is the only thing that can say,
            // which is the same reason the collector reads it.
            let index = machine.payload(addr, 0);
            let case = cases.get(index as usize).ok_or_else(|| {
                RuntimeError::new(format!(
                    "this `{}` is in case {index}, which it does not have",
                    layout.name
                ))
            })?;
            let mut payload = Vec::with_capacity(case.payload.len());
            for (at, repr) in case.payload.iter().enumerate() {
                let word = machine.payload(addr, 1 + at as u32);
                payload.push(out(machine, *repr, word, deeper)?);
            }
            Ok(Value::enumeration(&*layout.name, &*case.name, payload))
        }
        Shape::Elements { elem, growable } => {
            let len = machine.object_len(addr);
            let mut items = Vec::with_capacity(len as usize);
            for at in 0..len {
                let word = machine.payload(addr, at);
                items.push(out(machine, *elem, word, deeper)?);
            }
            // The `growable` flag is what tells an `Array` from a `Vector`,
            // and it is in the layout because that is the one place that
            // knows: both are a run of words with a length.
            Ok(if *growable {
                Value(crate::value::Repr::Vector(VectorStorage::new(items)))
            } else {
                Value::array(items)
            })
        }
        // A box is erasure, and a reader looks through it: `Value::view` and
        // `Display` both look through a `dyn`, so materialising the wrapper
        // would put back something no reader on the far side can see.
        Shape::Boxed => {
            let tag = machine.payload(addr, 0);
            let repr = Repr::from_tag(tag)
                .ok_or_else(|| RuntimeError::new("this boxed value carries no known type"))?;
            out(machine, repr, machine.payload(addr, 1), deeper)
        }
        other => Err(RuntimeError::new(format!(
            "a `{}` cannot cross the boundary yet ({other:?})",
            layout.name
        ))),
    }
}

/// The word `value` occupies in a slot of `repr`.
pub(crate) fn from_value(
    machine: &mut Machine,
    repr: Repr,
    value: &Value,
) -> Result<u64, RuntimeError> {
    into(machine, repr, value, 0)
}

fn into(
    machine: &mut Machine,
    repr: Repr,
    value: &Value,
    depth: usize,
) -> Result<u64, RuntimeError> {
    let mismatch = || {
        RuntimeError::new(format!(
            "this value is not the `{}` that was expected here",
            repr.name()
        ))
    };
    match repr {
        Repr::Unit if value.is_unit() => Ok(0),
        Repr::Bool => value.as_bool().map(|b| b as u64).ok_or_else(mismatch),
        Repr::Int => value.as_int().map(|n| n as u64).ok_or_else(mismatch),
        Repr::Float => value.as_float().map(|x| x.to_bits()).ok_or_else(mismatch),
        Repr::Duration => value
            .as_duration_nanos()
            .map(|n| n as u64)
            .ok_or_else(mismatch),
        Repr::Ref => object_from_value(machine, value, depth),
        _ => Err(mismatch()),
    }
}

/// A new object holding `value`, and its address.
fn object_from_value(
    machine: &mut Machine,
    value: &Value,
    depth: usize,
) -> Result<u64, RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    match value.view() {
        // A scalar arriving where a reference was declared is a value whose
        // type was erased: a Host result the schema declared `Any`. That is
        // what a box is for, and it is the one thing on this side that
        // allocates without having been asked to by a layout.
        ValueView::Unit => boxed(machine, Repr::Unit, 0),
        ValueView::Bool(b) => boxed(machine, Repr::Bool, b as u64),
        ValueView::Int(n) => boxed(machine, Repr::Int, n as u64),
        ValueView::Float(x) => boxed(machine, Repr::Float, x.to_bits()),
        ValueView::Duration(ns) => boxed(machine, Repr::Duration, ns as u64),
        ValueView::Str(text) => machine.new_string(text),
        ValueView::Struct(view) => {
            let name = short(view.type_name());
            let id = layout_for_struct(program, name, &view)?;
            let Shape::Struct { fields } = &program.layout(id).shape else {
                unreachable!("`layout_for_struct` answers a struct-shaped layout");
            };
            let addr = machine.new_object(id, 0)?;
            let mark = machine.temps();
            machine.push_temp(addr);
            let filled = (|machine: &mut Machine| {
                for (at, field) in fields.iter().enumerate() {
                    let Some(value) = view.field(&field.name) else {
                        return Err(RuntimeError::new(format!(
                            "this `{name}` has no field `{}`",
                            field.name
                        )));
                    };
                    let word = into(machine, field.repr, value, deeper)?;
                    machine.set_payload(addr, at as u32, word);
                }
                Ok(())
            })(machine);
            machine.release_temps(mark);
            filled.map(|()| addr)
        }
        ValueView::Enum(view) => {
            let name = short(view.type_name());
            let (id, index) = layout_for_enum(program, name, view.case(), view.payload())?;
            let Shape::Enum { cases } = &program.layout(id).shape else {
                unreachable!("`layout_for_enum` answers an enum-shaped layout");
            };
            let case = &cases[index as usize];
            let addr = machine.new_object(id, 0)?;
            machine.set_payload(addr, 0, index as u64);
            let mark = machine.temps();
            machine.push_temp(addr);
            let filled = (|machine: &mut Machine| {
                for (at, (repr, value)) in case.payload.iter().zip(view.payload()).enumerate() {
                    let word = into(machine, *repr, value, deeper)?;
                    machine.set_payload(addr, 1 + at as u32, word);
                }
                Ok(())
            })(machine);
            machine.release_temps(mark);
            filled.map(|()| addr)
        }
        ValueView::Array(items) => elements(machine, program, items, false, deeper),
        ValueView::Vector(items) => elements(machine, program, &items, true, deeper),
        other => Err(RuntimeError::new(format!(
            "a `{}` cannot cross the boundary into the linear-memory backend yet",
            named(&other)
        ))),
    }
}

/// A `Shape::Elements` object holding `items`.
fn elements(
    machine: &mut Machine,
    program: &Program,
    items: &[Value],
    growable: bool,
    depth: usize,
) -> Result<u64, RuntimeError> {
    let id = layout_for_elements(program, items, growable)?;
    let Shape::Elements { elem, .. } = program.layout(id).shape else {
        unreachable!("`layout_for_elements` answers an elements-shaped layout");
    };
    let addr = machine.new_object(id, items.len() as u32)?;
    let mark = machine.temps();
    machine.push_temp(addr);
    let filled = (|machine: &mut Machine| {
        for (at, item) in items.iter().enumerate() {
            let word = into(machine, elem, item, depth)?;
            machine.set_payload(addr, at as u32, word);
        }
        Ok(())
    })(machine);
    machine.release_temps(mark);
    filled.map(|()| addr)
}

/// A box holding `word`, tagged `repr`.
fn boxed(machine: &mut Machine, repr: Repr, word: u64) -> Result<u64, RuntimeError> {
    let id = layout_of(machine.program(), |shape| matches!(shape, Shape::Boxed))
        .ok_or_else(|| RuntimeError::new("this program has no boxed layout to erase into"))?;
    let addr = machine.new_object(id, 0)?;
    machine.set_payload(addr, 0, repr.tag());
    machine.set_payload(addr, 1, word);
    Ok(addr)
}

/// The layout of a struct called `name` whose fields this value has.
fn layout_for_struct(
    program: &Program,
    name: &str,
    view: &crate::value::StructView<'_>,
) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        let Shape::Struct { fields } = &layout.shape else {
            return false;
        };
        &*layout.name == name
            && fields.len() == view.len()
            && fields
                .iter()
                .all(|field| view.field(&field.name).is_some_and(|v| fits(field.repr, v)))
    })
    .ok_or_else(|| unknown_family(name))
}

/// The layout of an enum called `name` with a case `case` this payload fits,
/// and that case's index.
fn layout_for_enum(
    program: &Program,
    name: &str,
    case: &str,
    payload: &[Value],
) -> Result<(LayoutId, u32), RuntimeError> {
    // One layout per payload `Repr`, so `Option<Int>` and `Option<String>`
    // are two and the payload is what tells them apart. A case that carries
    // nothing matches every one of them, which is right: `None` is the same
    // word whichever `Option` it is in, and the layout the object ends up
    // with is the first the program declared.
    for (index, layout) in program.layouts.iter().enumerate() {
        let Shape::Enum { cases } = &layout.shape else {
            continue;
        };
        if &*layout.name != name {
            continue;
        }
        let Some(at) = layout.case(case) else {
            continue;
        };
        let declared = &cases[at as usize].payload;
        if declared.len() == payload.len()
            && declared
                .iter()
                .zip(payload)
                .all(|(repr, value)| fits(*repr, value))
        {
            return Ok((LayoutId(index as u32), at));
        }
    }
    Err(unknown_family(name))
}

/// The layout of a run of elements these items fit.
fn layout_for_elements(
    program: &Program,
    items: &[Value],
    growable: bool,
) -> Result<LayoutId, RuntimeError> {
    // An empty list names no element type, and one layout per element `Repr`
    // is what the table holds, so nothing here can tell an empty
    // `Array<Int>` from an empty `Array<String>`. The first declared layout
    // of the right growability is taken, and it is correct for either: an
    // object with no elements has no element word for the difference to show
    // in, and the length in the header is what every reader of it consults.
    find(program, |layout| {
        let Shape::Elements {
            elem,
            growable: is_growable,
        } = layout.shape
        else {
            return false;
        };
        is_growable == growable && items.iter().all(|item| fits(elem, item))
    })
    .ok_or_else(|| unknown_family(if growable { "Vector" } else { "Array" }))
}

/// The first layout `wanted` accepts.
fn find(program: &Program, wanted: impl Fn(&Layout) -> bool) -> Option<LayoutId> {
    program
        .layouts
        .iter()
        .position(wanted)
        .map(|at| LayoutId(at as u32))
}

/// The first layout whose shape `wanted` accepts.
fn layout_of(program: &Program, wanted: impl Fn(&Shape) -> bool) -> Option<LayoutId> {
    find(program, |layout| wanted(&layout.shape))
}

/// Whether a word of `repr` could hold `value`.
///
/// The question a layout search asks, and it is deliberately the same one
/// [`into`] would answer by succeeding: a family is the right one when every
/// part of the value fits the `Repr` the family declares for it. It does not
/// recurse, because a `Repr::Ref` is a reference whatever the object is —
/// which is the point of describing families rather than instantiations.
fn fits(repr: Repr, value: &Value) -> bool {
    match repr {
        Repr::Unit => value.is_unit(),
        Repr::Bool => value.as_bool().is_some(),
        Repr::Int => value.as_int().is_some(),
        Repr::Float => value.as_float().is_some(),
        Repr::Duration => value.as_duration_nanos().is_some(),
        Repr::Ref => matches!(
            value.view(),
            ValueView::Unit
                | ValueView::Bool(_)
                | ValueView::Int(_)
                | ValueView::Float(_)
                | ValueView::Duration(_)
                | ValueView::Str(_)
                | ValueView::Array(_)
                | ValueView::Vector(_)
                | ValueView::Struct(_)
                | ValueView::Enum(_)
        ),
        // Neither is a value a host holds. See `to_value`.
        Repr::Addr | Repr::Host => false,
    }
}

/// The declared name without its module: `rules.policy.Decision` is a
/// `Decision`.
///
/// A `Value` carries the qualified name the checker resolved and a
/// [`Layout`] carries the name the declaration wrote, which is what a
/// rendering of the object shows. Comparing the two means dropping the
/// qualification, exactly as the public `Display` does.
fn short(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// What a [`ValueView`] is called, for a refusal that has to name it.
fn named(view: &ValueView<'_>) -> &'static str {
    match view {
        ValueView::Map(_) => "Map",
        ValueView::Set(_) => "Set",
        ValueView::Closure(_) => "closure",
        ValueView::HostModule(_) => "host module",
        ValueView::HostFn { .. } => "host operation",
        ValueView::Resource(_) => "resource handle",
        ValueView::Type(_) => "type",
        ValueView::Range(_) => "Range",
        ValueView::Task(_) => "Task",
        ValueView::TaskScope(_) => "task scope",
        ValueView::Shared(_) => "Shared",
        _ => "value",
    }
}

fn unknown_family(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this program describes no `{name}` for a value of that shape to be built as"
    ))
    .with_rule(
        "A layout describes a family of values, and a program declares the families it uses.",
    )
}

fn too_deep() -> RuntimeError {
    RuntimeError::new("this value nests too deeply to cross the boundary")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::exec::tests::Build;
    use cove_lir::{Case, Field, Repr, Shape};
    use std::sync::Arc;

    fn field(name: &str, repr: Repr) -> Field {
        Field {
            name: Arc::from(name),
            repr,
        }
    }

    #[test]
    fn a_struct_round_trips_with_its_name_and_its_fields() {
        let mut build = Build::default();
        build.layout(
            "Point",
            Shape::Struct {
                fields: vec![field("x", Repr::Int), field("y", Repr::Ref)],
            },
        );
        let program = build.bare();
        let mut machine = Machine::new(&program, 1 << 12);

        let value = Value::structure(
            "m.geometry.Point",
            [("x", Value::int(3)), ("y", Value::string("up"))],
        );
        let word = from_value(&mut machine, Repr::Ref, &value).unwrap();
        let back = to_value(&machine, Repr::Ref, word).unwrap();
        assert_eq!(back.declared_type(), Some("Point"));
        assert_eq!(back.to_string(), "Point(x: 3, y: up)");
    }

    #[test]
    fn an_option_picks_the_layout_its_payload_fits() {
        let mut build = Build::default();
        // Two families named `Option`, told apart by what `Some` carries.
        let ints = build.layout(
            "Option",
            Shape::Enum {
                cases: vec![
                    Case {
                        name: Arc::from("None"),
                        payload: vec![],
                    },
                    Case {
                        name: Arc::from("Some"),
                        payload: vec![Repr::Int],
                    },
                ],
            },
        );
        let refs = build.layout(
            "Option",
            Shape::Enum {
                cases: vec![
                    Case {
                        name: Arc::from("None"),
                        payload: vec![],
                    },
                    Case {
                        name: Arc::from("Some"),
                        payload: vec![Repr::Ref],
                    },
                ],
            },
        );
        let program = build.bare();
        let mut machine = Machine::new(&program, 1 << 12);

        let some_int = Value::enumeration("Option", "Some", [Value::int(7)]);
        let word = from_value(&mut machine, Repr::Ref, &some_int).unwrap();
        assert_eq!(machine.object_layout(word), ints);
        assert_eq!(
            to_value(&machine, Repr::Ref, word).unwrap().to_string(),
            "Some(7)"
        );

        let some_str = Value::enumeration("Option", "Some", [Value::string("hi")]);
        let word = from_value(&mut machine, Repr::Ref, &some_str).unwrap();
        assert_eq!(machine.object_layout(word), refs);
        assert_eq!(
            to_value(&machine, Repr::Ref, word).unwrap().to_string(),
            "Some(hi)"
        );
    }

    #[test]
    fn an_array_round_trips_and_a_vector_stays_growable() {
        let mut build = Build::default();
        build.layout(
            "Array",
            Shape::Elements {
                elem: Repr::Ref,
                growable: false,
            },
        );
        build.layout(
            "Vector",
            Shape::Elements {
                elem: Repr::Int,
                growable: true,
            },
        );
        let program = build.bare();
        let mut machine = Machine::new(&program, 1 << 12);

        let array = Value::array([Value::string("a"), Value::string("b")]);
        let word = from_value(&mut machine, Repr::Ref, &array).unwrap();
        let back = to_value(&machine, Repr::Ref, word).unwrap();
        assert_eq!(back.to_string(), "[a, b]");
        assert!(matches!(back.view(), ValueView::Array(_)));

        let vector = Value(crate::value::Repr::Vector(VectorStorage::new(vec![
            Value::int(1),
            Value::int(2),
        ])));
        let word = from_value(&mut machine, Repr::Ref, &vector).unwrap();
        let back = to_value(&machine, Repr::Ref, word).unwrap();
        assert_eq!(back.to_string(), "[1, 2]");
        assert!(matches!(back.view(), ValueView::Vector(_)));
    }

    /// An `Any` result is a box, and a reader looks through it.
    #[test]
    fn a_scalar_arriving_at_a_reference_is_boxed() {
        let mut build = Build::default();
        build.layout("Any", Shape::Boxed);
        let program = build.bare();
        let mut machine = Machine::new(&program, 1 << 12);

        let word = from_value(&mut machine, Repr::Ref, &Value::int(41)).unwrap();
        assert!(matches!(
            program.layout(machine.object_layout(word)).shape,
            Shape::Boxed
        ));
        assert_eq!(
            to_value(&machine, Repr::Ref, word).unwrap().to_string(),
            "41"
        );
    }

    /// The reason [`Machine::push_temp`] exists.
    ///
    /// The heap is sized so that the outer object is allocated, then a
    /// collection has to run before the last field can be, while the only
    /// thing naming the outer object is a Rust local. Without a temporary
    /// root the sweep reclaims it and the writes that follow land in a free
    /// block; the round trip then answers something other than what went in,
    /// or nothing at all.
    #[test]
    fn a_collection_in_the_middle_of_building_does_not_free_what_is_being_built() {
        let mut build = Build::default();
        build.layout(
            "Pair",
            Shape::Struct {
                fields: vec![field("left", Repr::Ref), field("right", Repr::Ref)],
            },
        );
        build.layout(
            "Array",
            Shape::Elements {
                elem: Repr::Ref,
                growable: false,
            },
        );
        let program = build.bare();
        // Small enough that filling the nested value cannot be done without
        // reclaiming, and large enough that it can be done with it.
        let mut machine = Machine::new(&program, 56);

        // Garbage nothing roots, so the first collection has something to
        // find and the heap starts nearly full.
        for _ in 0..12 {
            machine.new_string("........................").unwrap();
        }
        assert_eq!(machine.collected().collections, 0);

        let value = Value::structure(
            "Pair",
            [
                (
                    "left",
                    Value::array([
                        Value::string("aaaaaaaaaaaaaaaa"),
                        Value::string("bbbbbbbbbbbbbbbb"),
                    ]),
                ),
                ("right", Value::string("cccccccccccccccc")),
            ],
        );
        let word = from_value(&mut machine, Repr::Ref, &value).unwrap();
        assert!(
            machine.collected().collections > 0,
            "the fixture is meant to collect while the value is being built"
        );
        assert_eq!(
            to_value(&machine, Repr::Ref, word).unwrap().to_string(),
            "Pair(left: [aaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbb], right: cccccccccccccccc)"
        );
    }

    #[test]
    fn a_family_the_program_does_not_describe_is_refused_by_name() {
        let program = Build::default().bare();
        let mut machine = Machine::new(&program, 1 << 12);
        let value = Value::structure("Point", [("x", Value::int(1))]);
        let error = from_value(&mut machine, Repr::Ref, &value).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Point` for a value of that shape to be built as"
        );
    }

    #[test]
    fn a_cycle_is_refused_rather_than_recursed_into() {
        let mut build = Build::default();
        let pair = build.layout(
            "Loop",
            Shape::Struct {
                fields: vec![field("self", Repr::Ref)],
            },
        );
        let program = build.bare();
        let mut machine = Machine::new(&program, 1 << 12);
        let addr = machine.new_object(pair, 0).unwrap();
        machine.set_payload(addr, 0, addr);
        let error = to_value(&machine, Repr::Ref, addr).unwrap_err();
        assert_eq!(
            error.message,
            "this value nests too deeply to cross the boundary"
        );
    }
}
