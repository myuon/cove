//! Where a run of words becomes a public [`Value`], and back.
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
//! # A value is a run of words, so a conversion takes a layout
//!
//! A word is untagged, so nothing about `7` says whether it is an `Int`, a
//! `Duration` of seven nanoseconds, or a `Bool` that got there by a lowering
//! bug — and nothing about *one* word says whether the value it belongs to
//! ends there. What says both is the static metadata the words came from: the
//! [`LayoutId`] of the value location. A `Point` is two words where the
//! `Point` is, and materialising one is reading its two words rather than
//! following an address to somewhere else.
//!
//! There is no narrow entry point beside them. There used to be one, for a
//! Host call's arguments: those were named by slot, and a slot declares a
//! `Repr` rather than a layout — which is enough for every one-word family
//! and is not enough for an inline struct or enum, so a multiword value
//! reached a host by having been boxed first. An argument carries the layout
//! of the location it names now, so a `Point` crosses as the `Point` the
//! schema declared.
//!
//! # The way in is told the family, except where the family is the question
//!
//! Going out, a value location says what it is and an object's header says
//! what *it* is. Coming in, the destination's layout says what to build — an
//! entry's parameter, a Host operation's declared result — so a struct is
//! built to the layout the declaration named rather than looked up by shape.
//!
//! The exception is erasure. A destination whose layout is [`Shape::Boxed`]
//! accepts any value, and a box has to record *which* — so that is the one
//! path that reads the value's own description and searches the program's
//! layout table for the family it names. A value the table describes no
//! family for is refused by name, which is not a gap here: a program that
//! never mentions a `Point` has no `Point` layout, and a host that hands it
//! one is handing it a type it does not have.
//!
//! # Building a value is not atomic
//!
//! [`from_value`] allocates, and an allocation can collect. A word it has
//! produced may name an object that nothing the collector walks names yet —
//! it is in a Rust `Vec`, which nothing walks — so every object this file
//! makes is held as a temporary root from the moment it exists until the
//! whole conversion is done: [`Machine::push_temp`] at each allocation, one
//! [`Machine::release_temps`] at the end, on the failing path as well as the
//! succeeding one. The caller is what owns them afterwards, and it writes
//! them into a frame before anything else can allocate.

use cove_lir::{Layout, LayoutId, Program, Repr, Shape};
use cove_schema::builtins::{MAP, RANGE, SET};

use crate::error::RuntimeError;
use crate::lvm::exec::Machine;
use crate::value::{MapKey, Value, ValueView, VectorStorage};

/// How deep a value may nest as it crosses.
///
/// A `Value` is a tree and a heap object graph is not: `StoreField` can make
/// an object hold itself, and a conversion that met one would recurse until
/// the native stack ran out. The limit is not a language fact and is not
/// reachable by any value a program can write down; it is here so that the
/// failure is a message rather than an abort.
const MAX_DEPTH: usize = 128;

/// The public value of the `words` at a value location of `layout`.
pub(crate) fn to_value(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
) -> Result<Value, RuntimeError> {
    out(machine, layout, words, 0)
}

/// The value at a location of `layout` holding `words`.
fn out(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    depth: usize,
) -> Result<Value, RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let described = program.layout(layout);
    match &described.shape {
        Shape::Word(repr) => word_out(machine, *repr, word_at(words, 0)?, depth),
        // A `Range` is a struct of three words and is not one to a reader:
        // the oracle answers `Value::range_of`, which prints `0..<3` and
        // compares by the bounds it was written with. Materialising the three
        // words as `Range(start: 0, end: 3, inclusive: false)` would be
        // handing a host this representation instead of the value, so the
        // family is recognised and answered as what it is.
        Shape::Struct { .. } if is_range(program, described) => Ok(Value::range_of(
            word_at(words, RANGE_START)? as i64,
            word_at(words, RANGE_END)? as i64,
            word_at(words, RANGE_INCLUSIVE)? != 0,
        )),
        // A struct is its fields, in place. Each field is a run of words at a
        // static offset, and reaching one is arithmetic rather than a load.
        Shape::Struct { fields, .. } => {
            let mut out_fields = Vec::with_capacity(fields.len());
            for field in fields {
                let value = out(machine, field.layout, run(program, words, field)?, deeper)?;
                out_fields.push((field.name.to_string(), value));
            }
            Ok(Value::structure(&*described.name, out_fields))
        }
        // Word 0 is the case index and the words after it are the payload
        // region. The collector no longer reads that word — the region's map
        // is static — but a *reader* still must: which of the payload words
        // are part of this value is exactly what the case says.
        Shape::Enum { cases, .. } => {
            let index = word_at(words, 0)?;
            let case = cases.get(index as usize).ok_or_else(|| {
                RuntimeError::new(format!(
                    "this `{}` is in case {index}, which it does not have",
                    described.name
                ))
            })?;
            let mut payload = Vec::with_capacity(case.parts.len());
            for part in &case.parts {
                let at = 1 + part.at as usize;
                let width = program.layout(part.layout).width() as usize;
                let held = words
                    .get(at..at + width)
                    .ok_or_else(|| short_run(&described.name))?;
                payload.push(out(machine, part.layout, held, deeper)?);
            }
            Ok(Value::enumeration(&*described.name, &*case.name, payload))
        }
        Shape::Free => Err(reclaimed()),
        // Every family left lives in the heap, so the location is one
        // address.
        _ => object_to_value(machine, word_at(words, 0)?, depth),
    }
}

/// The value of one word, read as `repr`.
fn word_out(machine: &Machine, repr: Repr, word: u64, depth: usize) -> Result<Value, RuntimeError> {
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
        return Err(null_value());
    }
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let id = machine.object_layout(addr);
    let layout = program.layout(id);
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
        // A struct, an enum or a scalar whose *object* this is: a layout the
        // lowering deliberately broke a recursion at holds the value's own
        // inline words as its payload, and `Layout::payload_words` answers
        // that same width. So the payload is read as a value location and
        // handed back to [`out`].
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            let words = machine.payload_run(addr, 0, layout.width());
            out(machine, id, &words, depth)
        }
        // The stride is the element layout's width, so an `Array<Point>` is a
        // run of two-word elements rather than a run of addresses.
        Shape::Elements { elem, growable } => {
            let items = elements(machine, addr, *elem, machine.object_len(addr), 0, deeper)?;
            // The `growable` flag is what tells an `Array`'s storage from a
            // `Vector`'s, and it is in the layout because that is the one
            // place that knows: both are a run of words with a length.
            Ok(if *growable {
                Value(crate::value::Repr::Vector(VectorStorage::new(items)))
            } else {
                Value::array(items)
            })
        }
        // The length is the header's and not the store's. A store is as long
        // as the last growth made it, and the words past the length are the
        // spare room a `push` will use rather than elements the value has —
        // reading the store's own length would hand a host the trailing zeros
        // as if they were part of the vector.
        Shape::Vector { elem } => {
            let len = machine.payload(addr, 0);
            let store = machine.payload(addr, 1);
            let items = if store == 0 {
                Vec::new()
            } else {
                elements(machine, store, *elem, len as u32, 0, deeper)?
            };
            Ok(Value(crate::value::Repr::Vector(VectorStorage::new(items))))
        }
        // A set materialises as a set and not as the run of words it is:
        // `Value::set` takes `MapKey`s, which is the restriction the language
        // puts on what may be one, showing through. Every member passed
        // `builtins::key`'s check before it was written, so a member that is
        // not a key is a heap that stopped being a set.
        //
        // The members are already ascending — that is what the shape means —
        // and `MapKey`'s own `Ord` is what the `BTreeSet` re-sorts them by.
        // Nothing changes places unless the two orders disagree, which is the
        // one thing that would say `builtins::key` has drifted from the
        // oracle.
        Shape::Members { elem } => {
            let items = elements(machine, addr, *elem, machine.object_len(addr), 0, deeper)?;
            let mut keys = Vec::with_capacity(items.len());
            for item in &items {
                keys.push(as_key(item)?);
            }
            Ok(Value::set(keys))
        }
        // Key then value, each at its own width, and only the key carries the
        // restriction.
        Shape::Entries { key, value } => {
            let (key, value) = (*key, *value);
            let widths = (
                program.layout(key).width(),
                program.layout(value).width(),
            );
            let stride = widths.0 + widths.1;
            let len = machine.object_len(addr);
            let mut entries = Vec::with_capacity(len as usize);
            for at in 0..len {
                let one = out(
                    machine,
                    key,
                    &machine.payload_run(addr, at * stride, widths.0),
                    deeper,
                )?;
                let other = out(
                    machine,
                    value,
                    &machine.payload_run(addr, at * stride + widths.0, widths.1),
                    deeper,
                )?;
                entries.push((as_key(&one)?, other));
            }
            Ok(Value::map(entries))
        }
        // A box is erasure, and a reader looks through it: `Value::view` and
        // `Display` both look through a `dyn`, so materialising the wrapper
        // would put back something no reader on the far side can see.
        //
        // Payload word 0 is the layout of what it holds and the words after
        // it are that value, inline — so a boxed `Point` is a two-word
        // payload rather than a reference to somewhere else again.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            let described = program
                .layouts
                .get(held.index())
                .ok_or_else(|| RuntimeError::new("this boxed value carries no known type"))?;
            let words = machine.payload_run(addr, 1, described.width());
            out(machine, held, &words, deeper)
        }
        // The one family this backend can make and cannot hand over.
        //
        // A public closure is a `Value::Closure` carrying a
        // `crate::value::ClosureBody`, and the only lowered variant of one
        // names a function of the *predecessor's* program. Building that here
        // would mean answering a closure whose body a host's callback would go
        // looking for in a program this run does not have — a wrong answer
        // rather than a missing one. `ClosureView` does not rescue it: a host
        // reads a closure's arity and whether it is `async`, but what it reads
        // them *for* is deciding whether to call it, and a callback it cannot
        // call is worth less than a refusal that says so.
        //
        // So this refuses, and `Back::call` refuses on the other side for the
        // same reason. It stops being true when `ClosureBody` can name a
        // `cove-lir` function, which is a change to `value.rs` and not to this
        // file.
        Shape::Closure { .. } => Err(RuntimeError::new(
            "a closure cannot cross the boundary out of the linear-memory backend",
        )
        .with_rule(
            "A host that is handed a closure may call it back, and only the backend that made one can run it.",
        )),
        Shape::Free => Err(reclaimed()),
    }
}

/// `len` values of `elem`, read from the payload of `addr` at its own stride.
fn elements(
    machine: &Machine,
    addr: u64,
    elem: LayoutId,
    len: u32,
    from: u32,
    depth: usize,
) -> Result<Vec<Value>, RuntimeError> {
    let stride = machine.program().layout(elem).width();
    let mut items = Vec::with_capacity(len as usize);
    for at in 0..len {
        let words = machine.payload_run(addr, from + at * stride, stride);
        items.push(out(machine, elem, &words, depth)?);
    }
    Ok(items)
}

/// The words of `value` at a location of `layout`.
///
/// Every object built on the way is held as a temporary root until this
/// returns, because until the caller writes the words somewhere the collector
/// walks, a Rust `Vec` is all that names them.
pub(crate) fn from_value(
    machine: &mut Machine,
    layout: LayoutId,
    value: &Value,
) -> Result<Vec<u64>, RuntimeError> {
    let mark = machine.temps();
    let words = into(machine, layout, value, 0);
    machine.release_temps(mark);
    words
}

fn into(
    machine: &mut Machine,
    layout: LayoutId,
    value: &Value,
    depth: usize,
) -> Result<Vec<u64>, RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let described = program.layout(layout);
    let width = described.width() as usize;
    match &described.shape {
        Shape::Word(repr) => Ok(vec![word_into(machine, *repr, value, depth)?]),
        // The written bounds, not the normalised ones. `ValueView::Range`
        // answers a half-open [`crate::value::RangeBounds`], which is the
        // right thing for a host asking what a range *covers* and the wrong
        // thing to store: `1..3` and `1..<4` cover the same integers and are
        // still two values, because `==` compares the bounds a range was
        // written with.
        Shape::Struct { .. } if is_range(program, described) => {
            let Value(crate::value::Repr::Range {
                start,
                end,
                inclusive_end,
            }) = *value.erased()
            else {
                return Err(not_the_expected(&described.name));
            };
            Ok(vec![start as u64, end as u64, inclusive_end as u64])
        }
        Shape::Struct { fields, .. } => {
            let fields = fields.clone();
            let ValueView::Struct(view) = value.view() else {
                return Err(not_the_expected(&described.name));
            };
            // The qualified name on both sides, so two modules each declaring
            // a `Point` cannot be matched to each other's layout.
            if view.type_name() != &*described.name {
                return Err(not_the_expected(&described.name));
            }
            let mut words = vec![0; width];
            for field in &fields {
                let Some(held) = view.field(&field.name) else {
                    return Err(RuntimeError::new(format!(
                        "this `{}` has no field `{}`",
                        described.name, field.name
                    )));
                };
                let held = held.clone();
                let written = into(machine, field.layout, &held, deeper)?;
                let at = field.at as usize;
                words[at..at + written.len()].copy_from_slice(&written);
            }
            Ok(words)
        }
        // Constructing a case zeroes the payload region it does not fill, so
        // a reference word belonging to another case reads null. That is what
        // makes the region's static map safe: the collector never asks which
        // case a value is in, so no case may leave an address behind in a
        // word another case calls a reference.
        Shape::Enum { cases, .. } => {
            let cases = cases.clone();
            let name = described.name.clone();
            let ValueView::Enum(view) = value.view() else {
                return Err(not_the_expected(&name));
            };
            if view.type_name() != &*name {
                return Err(not_the_expected(&name));
            }
            let index = cases
                .iter()
                .position(|case| &*case.name == view.case())
                .ok_or_else(|| {
                    RuntimeError::new(format!("this `{name}` has no case `{}`", view.case()))
                })?;
            let case = &cases[index];
            let payload: Vec<Value> = view.payload().to_vec();
            if case.parts.len() != payload.len() {
                return Err(RuntimeError::new(format!(
                    "`{name}.{}` carries {} value(s), but {} were given",
                    case.name,
                    case.parts.len(),
                    payload.len()
                )));
            }
            let mut words = vec![0; width];
            words[0] = index as u64;
            for (part, held) in case.parts.iter().zip(&payload) {
                let written = into(machine, part.layout, held, deeper)?;
                let at = 1 + part.at as usize;
                words[at..at + written.len()].copy_from_slice(&written);
            }
            Ok(words)
        }
        Shape::Free => Err(reclaimed()),
        // Everything left lives in the heap, so the location is one address
        // and the layout says which family to build it to.
        _ => Ok(vec![object_from_value(machine, layout, value, depth)?]),
    }
}

/// The word `value` occupies in a slot of `repr`.
fn word_into(
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
        // A `Repr::Ref` is all a Host call's argument slot declares, so the
        // family has to come from the value: this is the erasure path, and it
        // boxes exactly as a `Shape::Boxed` destination does — unless the
        // family already *is* one address, in which case that address is the
        // word and a box would be a second indirection nobody asked for.
        //
        // The question is asked of the shape rather than of
        // [`Layout::is_ref`], which cannot answer it: a
        // `struct Error { message: String }` is one `Repr::Ref` word wide and
        // is an inline struct, not a reference to an `Error` somewhere.
        Repr::Ref => {
            let layout = layout_for(machine.program(), value)?;
            let held = into(machine, layout, value, depth)?;
            if one_address(machine.program(), layout) && held.len() == 1 {
                Ok(held[0])
            } else {
                let id = boxed_layout(machine.program())?;
                boxed(machine, id, layout, &held)
            }
        }
        _ => Err(mismatch()),
    }
}

/// A new object of `layout` holding `value`, and its address.
fn object_from_value(
    machine: &mut Machine,
    layout: LayoutId,
    value: &Value,
    depth: usize,
) -> Result<u64, RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let described = program.layout(layout);
    let name = described.name.clone();
    match described.shape.clone() {
        Shape::Str => {
            let ValueView::Str(text) = value.view() else {
                return Err(not_the_expected("String"));
            };
            let text = text.to_string();
            let addr = machine.new_string(&text)?;
            machine.push_temp(addr);
            Ok(addr)
        }
        Shape::Elements { elem, growable } => {
            let items: Vec<Value> = match value.view() {
                ValueView::Array(items) if !growable => items.to_vec(),
                ValueView::Vector(items) if growable => items.to_vec(),
                _ => return Err(not_the_expected(&name)),
            };
            run_of(machine, layout, elem, &items, deeper)
        }
        // Two objects, because that is what a `Vector` is: the header is the
        // identity a program holds and `is` asks about, and the store beneath
        // it is what a later `push` replaces without moving anything a
        // program is naming. Building only the store — which is what an
        // `Array` is one flag away from — would hand back a value nothing
        // could grow.
        Shape::Vector { elem } => {
            let ValueView::Vector(items) = value.view() else {
                return Err(not_the_expected(&name));
            };
            let store_layout = layout_for_store(machine.program(), elem)?;
            let header = machine.new_object(layout, 0)?;
            machine.push_temp(header);
            let store = run_of(machine, store_layout, elem, &items, deeper)?;
            machine.set_payload(header, 0, items.len() as u64);
            machine.set_payload(header, 1, store);
            Ok(header)
        }
        // The members arrive ascending, because a `BTreeSet<MapKey>` iterates
        // that way and [`crate::lvm::builtins::key`] reproduces the order it
        // iterates in. So the run is sorted as it is written and nothing
        // sorts it afterwards, which is the invariant the shape promises and
        // every builtin over it relies on.
        Shape::Members { elem } => {
            let ValueView::Set(items) = value.view() else {
                return Err(not_the_expected(&name));
            };
            let items: Vec<Value> = items.iter().map(MapKey::to_value).collect();
            run_of(machine, layout, elem, &items, deeper)
        }
        // Ascending by key, for the reason a set is ascending by member.
        Shape::Entries { key, value: held } => {
            let ValueView::Map(entries) = value.view() else {
                return Err(not_the_expected(&name));
            };
            let entries: Vec<(Value, Value)> = entries
                .iter()
                .map(|(one, other)| (one.to_value(), other.clone()))
                .collect();
            let addr = machine.new_object(layout, entries.len() as u32)?;
            machine.push_temp(addr);
            let widths = (machine.words_of(key), machine.words_of(held));
            let stride = widths.0 + widths.1;
            for (at, (one, other)) in entries.iter().enumerate() {
                let at = at as u32;
                let words = into(machine, key, one, deeper)?;
                machine.set_payload_run(addr, at * stride, &words);
                let words = into(machine, held, other, deeper)?;
                machine.set_payload_run(addr, at * stride + widths.0, &words);
            }
            Ok(addr)
        }
        // Erasure: the box records the layout of what it holds and holds that
        // value's words inline.
        Shape::Boxed => {
            let held = layout_for(machine.program(), value)?;
            let words = into(machine, held, value, deeper)?;
            boxed(machine, layout, held, &words)
        }
        _ => Err(RuntimeError::new(format!(
            "a `{name}` cannot cross the boundary into the linear-memory backend yet"
        ))),
    }
}

/// An object of `layout` holding `items`, each inline at `elem`'s width.
fn run_of(
    machine: &mut Machine,
    layout: LayoutId,
    elem: LayoutId,
    items: &[Value],
    depth: usize,
) -> Result<u64, RuntimeError> {
    let stride = machine.words_of(elem);
    let addr = machine.new_object(layout, items.len() as u32)?;
    // The object is rooted before anything else is built into it. Its payload
    // is zeroed, so the part not filled in yet traces nothing and the part
    // that is holds what has been converted.
    machine.push_temp(addr);
    for (at, item) in items.iter().enumerate() {
        let words = into(machine, elem, item, depth)?;
        machine.set_payload_run(addr, at as u32 * stride, &words);
    }
    Ok(addr)
}

/// A box of `id` holding `words`, tagged with the layout they belong to.
///
/// The header's length is the width of what is inside, because a `Boxed`
/// layout cannot know it: erasure is where a value stops having a static
/// width, and the object is where a value without one has to live.
fn boxed(
    machine: &mut Machine,
    id: LayoutId,
    held: LayoutId,
    words: &[u64],
) -> Result<u64, RuntimeError> {
    let addr = machine.new_object(id, words.len() as u32)?;
    machine.push_temp(addr);
    machine.set_payload(addr, 0, held.0 as u64);
    machine.set_payload_run(addr, 1, words);
    Ok(addr)
}

/// The program's `Shape::Boxed` layout.
fn boxed_layout(program: &Program) -> Result<LayoutId, RuntimeError> {
    layout_of(program, |shape| matches!(shape, Shape::Boxed))
        .ok_or_else(|| RuntimeError::new("this program has no boxed layout to erase into"))
}

/// Whether a value of `layout` is one address rather than inline words.
///
/// Asked of the shape, because the width cannot answer it. A struct of one
/// `String` field is one `Repr::Ref` word wide and is still an inline struct,
/// so [`Layout::is_ref`] says yes where the truth is no — which matters at
/// exactly one place, the erasure path, where the difference decides whether
/// a box is needed.
fn one_address(program: &Program, layout: LayoutId) -> bool {
    match &program.layout(layout).shape {
        Shape::Word(repr) => repr.is_ref(),
        Shape::Struct { .. } | Shape::Enum { .. } | Shape::Free => false,
        _ => true,
    }
}

// --- finding the family of a value, for the erasure path -------------------

/// The layout a value of unknown static type is built to.
///
/// Only erasure asks: a destination whose layout is known builds to it. This
/// reads the value's own description — a struct's declared type name, an
/// enum's name and case, an array's elements — and looks it up in the
/// program's table of *families*, so `Array<Int>` and `Array<String>` are told
/// apart by whether the element layout admits the elements.
fn layout_for(program: &Program, value: &Value) -> Result<LayoutId, RuntimeError> {
    match value.view() {
        ValueView::Unit => scalar(program, Repr::Unit),
        ValueView::Bool(_) => scalar(program, Repr::Bool),
        ValueView::Int(_) => scalar(program, Repr::Int),
        ValueView::Float(_) => scalar(program, Repr::Float),
        ValueView::Duration(_) => scalar(program, Repr::Duration),
        ValueView::Str(_) => layout_of(program, |shape| matches!(shape, Shape::Str))
            .ok_or_else(|| unknown_family("String")),
        ValueView::Struct(view) => {
            let name = view.type_name();
            find(program, |layout| {
                let Shape::Struct { fields, .. } = &layout.shape else {
                    return false;
                };
                &*layout.name == name
                    && fields.len() == view.len()
                    && fields.iter().all(|field| {
                        view.field(&field.name)
                            .is_some_and(|v| fits(program, field.layout, v))
                    })
            })
            .ok_or_else(|| unknown_family(name))
        }
        // One layout per payload shape, so `Option<Int>` and `Option<String>`
        // are two and the payload is what tells them apart. A case that
        // carries nothing matches every one of them, which is right: `None`
        // is the same word whichever `Option` it is in, and the layout the
        // value ends up with is the first the program declared.
        ValueView::Enum(view) => {
            let name = view.type_name();
            find(program, |layout| {
                let Shape::Enum { cases, .. } = &layout.shape else {
                    return false;
                };
                if &*layout.name != name {
                    return false;
                }
                let Some(case) = cases.iter().find(|case| &*case.name == view.case()) else {
                    return false;
                };
                case.parts.len() == view.payload().len()
                    && case
                        .parts
                        .iter()
                        .zip(view.payload())
                        .all(|(part, held)| fits(program, part.layout, held))
            })
            .ok_or_else(|| unknown_family(name))
        }
        ValueView::Array(items) => layout_for_run(program, items, false),
        ValueView::Vector(items) => find(program, |layout| {
            let Shape::Vector { elem } = layout.shape else {
                return false;
            };
            items.iter().all(|item| fits(program, elem, item))
        })
        .ok_or_else(|| unknown_family("Vector")),
        ValueView::Set(items) => {
            let items: Vec<Value> = items.iter().map(MapKey::to_value).collect();
            find(program, |layout| {
                let Shape::Members { elem } = layout.shape else {
                    return false;
                };
                items.iter().all(|item| fits(program, elem, item))
            })
            .ok_or_else(|| unknown_family(SET.name))
        }
        ValueView::Map(held) => {
            let held: Vec<(Value, Value)> = held
                .iter()
                .map(|(key, value)| (key.to_value(), value.clone()))
                .collect();
            find(program, |layout| {
                let Shape::Entries { key, value } = layout.shape else {
                    return false;
                };
                held.iter()
                    .all(|(one, other)| fits(program, key, one) && fits(program, value, other))
            })
            .ok_or_else(|| unknown_family(MAP.name))
        }
        ValueView::Range(_) => find(program, |layout| is_range(program, layout))
            .ok_or_else(|| unknown_family(RANGE.name)),
        other => Err(RuntimeError::new(format!(
            "a `{}` cannot cross the boundary into the linear-memory backend yet",
            named(&other)
        ))),
    }
}

/// The layout of a run of elements these items fit.
///
/// An empty list names no element type, and one layout per element family is
/// what the table holds, so nothing here can tell an empty `Array<Int>` from
/// an empty `Array<String>`. The first declared layout of the right
/// growability is taken, and it is correct for either: an object with no
/// elements has no element word for the difference to show in, and the length
/// in the header is what every reader of it consults.
fn layout_for_run(
    program: &Program,
    items: &[Value],
    growable: bool,
) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        let Shape::Elements {
            elem,
            growable: is_growable,
        } = layout.shape
        else {
            return false;
        };
        is_growable == growable && items.iter().all(|item| fits(program, elem, item))
    })
    .ok_or_else(|| unknown_family(if growable { "Vector" } else { "Array" }))
}

/// The layout of the growable store a `Vector` header of `elem` sits over.
///
/// A program that describes the header describes the store, because the
/// lowering interns both together — so a miss here is the same missing family
/// as a miss on the header and says so in the same words.
fn layout_for_store(program: &Program, elem: LayoutId) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        matches!(
            layout.shape,
            Shape::Elements {
                elem: e,
                growable: true
            } if e == elem
        )
    })
    .ok_or_else(|| unknown_family("Vector"))
}

/// The one-word layout of a scalar.
fn scalar(program: &Program, repr: Repr) -> Result<LayoutId, RuntimeError> {
    layout_of(program, |shape| shape == &Shape::Word(repr))
        .ok_or_else(|| unknown_family(repr.name()))
}

/// Payload word 0 of a `Range`: the first value it can yield.
const RANGE_START: usize = 0;

/// Word 1: the end as it was written — the last value the range yields when
/// it is inclusive, and the first one past it when it is not.
const RANGE_END: usize = 1;

/// Word 2: which of the two word 1 is.
const RANGE_INCLUSIVE: usize = 2;

/// Whether a layout is the program's `Range`.
///
/// `docs/LINEAR_VM.md` fixes the shape as `Struct { start: Int, end: Int,
/// inclusive: Bool }`, one layout for the program, and the whole of it is
/// checked rather than only the name — a `Range` is a builtin type a module
/// cannot redeclare, so the name is the checker's, and a family is the right
/// one when its fields say so. This is the one place that reads them as a
/// range's rather than as a struct's.
pub(crate) fn is_range(program: &Program, layout: &Layout) -> bool {
    let Shape::Struct { fields, .. } = &layout.shape else {
        return false;
    };
    let word = |at: usize, name: &str, repr: Repr| {
        fields.get(at).is_some_and(|field| {
            &*field.name == name && program.layout(field.layout).words == [repr]
        })
    };
    &*layout.name == RANGE.name
        && fields.len() == 3
        && layout.words == [Repr::Int, Repr::Int, Repr::Bool]
        && word(0, "start", Repr::Int)
        && word(1, "end", Repr::Int)
        && word(2, "inclusive", Repr::Bool)
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

/// Whether a value location of `layout` could hold `value`.
///
/// The question a family search asks, and it is deliberately the same one
/// [`into`] would answer by succeeding. It recurses only where the layout
/// does: a value location's width is a static fact, so a struct's fields are
/// checked and a reference is checked as a reference — which is the point of
/// describing families rather than instantiations.
fn fits(program: &Program, layout: LayoutId, value: &Value) -> bool {
    let described = program.layout(layout);
    match &described.shape {
        Shape::Word(Repr::Unit) => value.is_unit(),
        Shape::Word(Repr::Bool) => value.as_bool().is_some(),
        Shape::Word(Repr::Int) => value.as_int().is_some(),
        Shape::Word(Repr::Float) => value.as_float().is_some(),
        Shape::Word(Repr::Duration) => value.as_duration_nanos().is_some(),
        // What a boundary may put behind a reference, which is every family
        // a `Value` carries and nothing this run owns for itself.
        Shape::Word(Repr::Ref) | Shape::Boxed => matches!(
            value.view(),
            ValueView::Unit
                | ValueView::Bool(_)
                | ValueView::Int(_)
                | ValueView::Float(_)
                | ValueView::Duration(_)
                | ValueView::Str(_)
                | ValueView::Array(_)
                | ValueView::Vector(_)
                | ValueView::Set(_)
                | ValueView::Map(_)
                | ValueView::Struct(_)
                | ValueView::Enum(_)
                | ValueView::Range(_)
        ),
        // Neither is a value a host holds. See `word_out`.
        Shape::Word(Repr::Addr) | Shape::Word(Repr::Host) | Shape::Free => false,
        Shape::Str => matches!(value.view(), ValueView::Str(_)),
        Shape::Struct { fields, .. } if is_range(program, described) => {
            matches!(value.view(), ValueView::Range(_)) && fields.len() == 3
        }
        Shape::Struct { fields, .. } => match value.view() {
            ValueView::Struct(view) => {
                view.type_name() == &*described.name
                    && fields.len() == view.len()
                    && fields.iter().all(|field| {
                        view.field(&field.name)
                            .is_some_and(|v| fits(program, field.layout, v))
                    })
            }
            _ => false,
        },
        Shape::Enum { cases, .. } => match value.view() {
            ValueView::Enum(view) => {
                view.type_name() == &*described.name
                    && cases.iter().any(|case| {
                        &*case.name == view.case()
                            && case.parts.len() == view.payload().len()
                            && case
                                .parts
                                .iter()
                                .zip(view.payload())
                                .all(|(part, held)| fits(program, part.layout, held))
                    })
            }
            _ => false,
        },
        Shape::Elements { elem, growable } => match value.view() {
            ValueView::Array(items) if !growable => {
                items.iter().all(|item| fits(program, *elem, item))
            }
            ValueView::Vector(items) if *growable => {
                items.iter().all(|item| fits(program, *elem, item))
            }
            _ => false,
        },
        Shape::Vector { elem } => match value.view() {
            ValueView::Vector(items) => items.iter().all(|item| fits(program, *elem, item)),
            _ => false,
        },
        Shape::Members { elem } => match value.view() {
            ValueView::Set(items) => items
                .iter()
                .all(|item| fits(program, *elem, &item.to_value())),
            _ => false,
        },
        Shape::Entries { key, value: held } => match value.view() {
            ValueView::Map(entries) => entries.iter().all(|(one, other)| {
                fits(program, *key, &one.to_value()) && fits(program, *held, other)
            }),
            _ => false,
        },
        Shape::Closure { .. } => false,
    }
}

/// The declared name without its module: `rules.policy.Decision` is a
/// `Decision`.
///
/// A `Value` and a [`Layout`] both carry the qualified name, so a match
/// between them needs none of this. What needs it is a *rendering*, which
/// shows a type by the name its declaration wrote — exactly as the public
/// `Display` does with the same string.
pub(crate) fn short(name: &str) -> &str {
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

/// A value arrived where a location of a named family was declared.
///
/// Unreachable from a checked program on the way out of a host call — the
/// schema held the answer to its declared type before this ran — and reachable
/// on the way in only from a host that ignored what the checker resolved.
fn not_the_expected(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this value is not the `{}` that was expected here",
        short(name)
    ))
}

/// A value location held fewer words than its layout says it has.
///
/// A lowering bug rather than anything a program can do, reported because the
/// alternative is reading whatever followed the run.
fn short_run(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this `{}` is narrower than the layout that describes it",
        short(name)
    ))
}

/// The word at `at` of a value location.
fn word_at(words: &[u64], at: usize) -> Result<u64, RuntimeError> {
    words
        .get(at)
        .copied()
        .ok_or_else(|| short_run("value location"))
}

/// The words of `field` within a struct's run.
fn run<'w>(
    program: &Program,
    words: &'w [u64],
    field: &cove_lir::Field,
) -> Result<&'w [u64], RuntimeError> {
    let at = field.at as usize;
    let width = program.layout(field.layout).width() as usize;
    words
        .get(at..at + width)
        .ok_or_else(|| short_run(&field.name))
}

/// A member of a sorted run that is not a value a key may be.
///
/// Not the oracle's, and not something a program reaches: a `Set`'s elements
/// and a `Map`'s keys are `MapKey`s on that side by construction, and here
/// every one of them passed [`crate::lvm::builtins::key`]'s check before it
/// was written. This reports a heap that stopped being a set, and it carries
/// the rule the refusal would have carried so that a reader is told which of
/// the two restrictions was broken.
fn as_key(value: &Value) -> Result<MapKey, RuntimeError> {
    MapKey::from_value(value).map_err(|invalid| {
        RuntimeError::new(format!(
            "a `{}` cannot be a `Map` key or a `Set` element",
            invalid.type_name
        ))
        .with_rule(invalid.rule())
        .with_help(invalid.help())
    })
}

fn too_deep() -> RuntimeError {
    RuntimeError::new("this value nests too deeply to cross the boundary")
}

fn null_value() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

fn reclaimed() -> RuntimeError {
    RuntimeError::new("this value was read after it was reclaimed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::exec::tests::Build;
    use cove_lir::{Repr, Shape};

    /// A fixture's scalars, declared once so that everything else can name
    /// them: a family is a `LayoutId` now, and an `Array<Point>` cannot be
    /// written down without a `Point` to point at.
    struct World {
        program: Program,
        int: LayoutId,
        boolean: LayoutId,
        string: LayoutId,
    }

    impl World {
        fn new(more: impl FnOnce(&mut Build, LayoutId, LayoutId, LayoutId)) -> World {
            let mut build = Build::default();
            let int = build.word("Int", Repr::Int);
            let boolean = build.word("Bool", Repr::Bool);
            let string = build.layout("String", Shape::Str);
            build.program.str_layout = string;
            more(&mut build, int, boolean, string);
            World {
                program: build.done(),
                int,
                boolean,
                string,
            }
        }

        fn machine(&self) -> Machine<'_> {
            Machine::new(&self.program, 1 << 12)
        }

        /// The first layout named `name`.
        fn named(&self, name: &str) -> LayoutId {
            self.program
                .layouts
                .iter()
                .position(|layout| &*layout.name == name)
                .map(|at| LayoutId(at as u32))
                .expect("the fixture declares every family")
        }
    }

    /// A struct is its fields in place, so it crosses as a run of words
    /// rather than as an address to follow.
    #[test]
    fn a_struct_round_trips_with_its_name_and_its_fields() {
        // Qualified, as the lowering names it and as a `Value` carries it: a
        // layout is an identity, and two modules may each declare a `Point`.
        let world = World::new(|build, int, _, string| {
            build.structure("m.geometry.Point", &[("x", int), ("y", string)]);
        });
        let point = world.named("m.geometry.Point");
        let mut machine = world.machine();

        let value = Value::structure(
            "m.geometry.Point",
            [("x", Value::int(3)), ("y", Value::string("up"))],
        );
        let words = from_value(&mut machine, point, &value).unwrap();
        assert_eq!(words.len(), 2, "two words, where the value is");
        assert_eq!(words[0], 3);
        let back = to_value(&machine, point, &words).unwrap();
        assert_eq!(back.declared_type(), Some("m.geometry.Point"));
        // A rendering shows the name the declaration wrote.
        assert_eq!(back.to_string(), "Point(x: 3, y: up)");
    }

    /// Nesting is inline and recursive, so a `Line` is four words and no
    /// indirection — which is what makes `l.from.x` a slot offset rather than
    /// a load.
    #[test]
    fn a_nested_struct_is_one_flat_run() {
        let world = World::new(|build, int, _, _| {
            let point = build.structure("Point", &[("x", int), ("y", int)]);
            build.structure("Line", &[("from", point), ("to", point)]);
        });
        let line = world.named("Line");
        let mut machine = world.machine();

        let point = |x, y| Value::structure("Point", [("x", Value::int(x)), ("y", Value::int(y))]);
        let value = Value::structure("Line", [("from", point(1, 2)), ("to", point(3, 4))]);
        let words = from_value(&mut machine, line, &value).unwrap();
        assert_eq!(words, vec![1, 2, 3, 4]);
        assert_eq!(
            to_value(&machine, line, &words).unwrap().to_string(),
            "Line(from: Point(x: 1, y: 2), to: Point(x: 3, y: 4))"
        );
    }

    /// An enum is a discriminant and a payload region, and constructing a
    /// case zeroes the words it does not fill — which is what makes one
    /// static reference map right whatever case a value holds.
    #[test]
    fn an_enum_is_a_discriminant_and_a_payload_it_zeroes() {
        let world = World::new(|build, int, _, string| {
            // `enum E { A(Int, String), B(Float) }` from `docs/LINEAR_VM.md`:
            // `B` can use neither of `A`'s words, so the region is three.
            let float = build.word("Float", Repr::Float);
            build.enumeration("E", &[("A", vec![int, string]), ("B", vec![float])]);
        });
        let e = world.named("E");
        assert_eq!(
            world.program.layout(e).words,
            vec![Repr::Int, Repr::Int, Repr::Ref, Repr::Float]
        );
        let mut machine = world.machine();

        let a = Value::enumeration("E", "A", [Value::int(7), Value::string("hi")]);
        let words = from_value(&mut machine, e, &a).unwrap();
        assert_eq!(words[0], 0);
        assert_eq!(words[1], 7);
        assert_eq!(words[3], 0, "`B`'s word is not `A`'s to write");
        assert_eq!(
            to_value(&machine, e, &words).unwrap().to_string(),
            "A(7, hi)"
        );

        let b = Value::enumeration("E", "B", [Value::float(1.5)]);
        let words = from_value(&mut machine, e, &b).unwrap();
        assert_eq!(words[0], 1);
        assert_eq!(
            words[2], 0,
            "the reference word `A` would use reads null, so the collector traces nothing from it"
        );
        assert_eq!(f64::from_bits(words[3]), 1.5);
    }

    /// An `Array<Point>` is a run of two-word elements, so the boundary walks
    /// it at that stride and the header's length counts elements.
    #[test]
    fn an_array_of_multiword_elements_crosses_at_its_stride() {
        let world = World::new(|build, int, _, _| {
            let point = build.structure("Point", &[("x", int), ("y", int)]);
            build.layout(
                "Array",
                Shape::Elements {
                    elem: point,
                    growable: false,
                },
            );
        });
        let array = world.named("Array");
        let point = world.named("Point");
        let mut machine = world.machine();

        let value = Value::array([
            Value::structure("Point", [("x", Value::int(1)), ("y", Value::int(2))]),
            Value::structure("Point", [("x", Value::int(3)), ("y", Value::int(4))]),
        ]);
        let words = from_value(&mut machine, array, &value).unwrap();
        let addr = words[0];
        assert_eq!(machine.object_len(addr), 2, "elements, not words");
        assert_eq!(machine.payload_run(addr, 0, 4), vec![1, 2, 3, 4]);
        assert_eq!(
            to_value(&machine, array, &words).unwrap().to_string(),
            "[Point(x: 1, y: 2), Point(x: 3, y: 4)]"
        );
        let _ = point;
    }

    #[test]
    fn an_array_round_trips_and_a_vector_stays_growable() {
        let world = World::new(|build, int, _, string| {
            build.layout(
                "Array",
                Shape::Elements {
                    elem: string,
                    growable: false,
                },
            );
            // The two objects a `Vector` is: the store, and the header over
            // it. A program that uses one declares both, because the lowering
            // interns them together.
            build.layout(
                "Vector",
                Shape::Elements {
                    elem: int,
                    growable: true,
                },
            );
            build.layout("Vector", Shape::Vector { elem: int });
        });
        let array = world.named("Array");
        let header = world
            .program
            .layouts
            .iter()
            .position(|layout| matches!(layout.shape, Shape::Vector { .. }));
        let header = LayoutId(header.expect("the fixture declares a vector") as u32);
        let mut machine = world.machine();

        let value = Value::array([Value::string("a"), Value::string("b")]);
        let words = from_value(&mut machine, array, &value).unwrap();
        let back = to_value(&machine, array, &words).unwrap();
        assert_eq!(back.to_string(), "[a, b]");
        assert!(matches!(back.view(), ValueView::Array(_)));

        let vector = Value(crate::value::Repr::Vector(VectorStorage::new(vec![
            Value::int(1),
            Value::int(2),
        ])));
        let words = from_value(&mut machine, header, &vector).unwrap();
        let back = to_value(&machine, header, &words).unwrap();
        assert_eq!(back.to_string(), "[1, 2]");
        assert!(matches!(back.view(), ValueView::Vector(_)));
    }

    /// A vector arrives as the two objects it is, and the header is what a
    /// reference to it names — so growing it later replaces what is under the
    /// header rather than moving the value a program is holding.
    #[test]
    fn a_vector_arrives_as_a_header_over_a_store() {
        let world = World::new(|build, int, _, _| {
            build.layout(
                "Vector",
                Shape::Elements {
                    elem: int,
                    growable: true,
                },
            );
            build.layout("VectorHeader", Shape::Vector { elem: int });
        });
        let store = world.named("Vector");
        let header = world.named("VectorHeader");
        let mut machine = world.machine();

        let vector = Value(crate::value::Repr::Vector(VectorStorage::new(vec![
            Value::int(7),
            Value::int(8),
            Value::int(9),
        ])));
        let words = from_value(&mut machine, header, &vector).unwrap();
        let addr = words[0];
        assert_eq!(machine.object_layout(addr), header);
        assert_eq!(machine.payload(addr, 0), 3);
        let held = machine.payload(addr, 1);
        assert_eq!(machine.object_layout(held), store);
        // Exactly these elements, as `Vector.of` allocates: spare room nobody
        // asked for is room the run pays for.
        assert_eq!(machine.object_len(held), 3);
        assert_eq!(machine.payload(held, 2) as i64, 9);
    }

    /// The length is the header's word 0, and the store is as long as the last
    /// growth made it. A boundary that read the store's own length would hand
    /// a host the spare room as if it were part of the value.
    #[test]
    fn a_vector_crosses_out_by_its_header_and_not_its_store() {
        let world = World::new(|build, int, _, _| {
            build.layout(
                "Vector",
                Shape::Elements {
                    elem: int,
                    growable: true,
                },
            );
            build.layout("VectorHeader", Shape::Vector { elem: int });
        });
        let store = world.named("Vector");
        let header = world.named("VectorHeader");
        let mut machine = world.machine();

        // What a `push` onto a full store of two leaves behind: four words of
        // room, two of them elements.
        let addr = machine.new_object(store, 4).unwrap();
        machine.set_payload_run(addr, 0, &[1, 2, 99, 0]);
        let word = machine.new_object(header, 0).unwrap();
        machine.set_payload(word, 0, 2);
        machine.set_payload(word, 1, addr);

        let back = to_value(&machine, header, &[word]).unwrap();
        assert_eq!(back.to_string(), "[1, 2]");
        assert!(matches!(back.view(), ValueView::Vector(_)));

        // A header with no store at all is the empty vector, which is what a
        // reader is shown for one that never had elements written into it.
        let empty = machine.new_object(header, 0).unwrap();
        assert_eq!(
            to_value(&machine, header, &[empty]).unwrap().to_string(),
            "[]"
        );
    }

    /// A set and a map cross as themselves, and the run they arrive as is
    /// sorted because it was written in the order it arrived in: a
    /// `BTreeSet<MapKey>` iterates ascending, and
    /// [`crate::lvm::builtins::key`] is the same order over words.
    #[test]
    fn a_set_and_a_map_round_trip_in_ascending_order() {
        let world = World::new(|build, int, _, string| {
            build.layout("Set", Shape::Members { elem: int });
            build.layout(
                "Map",
                Shape::Entries {
                    key: string,
                    value: int,
                },
            );
        });
        let set = world.named("Set");
        let map = world.named("Map");
        let mut machine = world.machine();

        // Written out of order on the way in, and ascending in the object.
        let items = Value::set([MapKey::Int(3), MapKey::Int(1), MapKey::Int(2)]);
        let words = from_value(&mut machine, set, &items).unwrap();
        let addr = words[0];
        assert_eq!(machine.payload_run(addr, 0, 3), vec![1, 2, 3]);
        let back = to_value(&machine, set, &words).unwrap();
        assert_eq!(back.to_string(), "{1, 2, 3}");
        assert!(matches!(back.view(), ValueView::Set(_)));

        let held = Value::map([
            (MapKey::Str("b".to_string()), Value::int(2)),
            (MapKey::Str("a".to_string()), Value::int(1)),
        ]);
        let words = from_value(&mut machine, map, &held).unwrap();
        let addr = words[0];
        assert_eq!(machine.object_len(addr), 2);
        assert_eq!(machine.payload(addr, 1), 1, "the lowest key is first");
        let back = to_value(&machine, map, &words).unwrap();
        assert_eq!(back.to_string(), "{a: 1, b: 2}");
        assert!(matches!(back.view(), ValueView::Map(_)));
    }

    /// A `Map<String, Point>` is a run of three-word entries, key then value
    /// at their own widths.
    #[test]
    fn a_maps_entries_are_a_run_of_key_words_then_value_words() {
        let world = World::new(|build, int, _, string| {
            let point = build.structure("Point", &[("x", int), ("y", int)]);
            build.layout(
                "Map",
                Shape::Entries {
                    key: string,
                    value: point,
                },
            );
        });
        let map = world.named("Map");
        let mut machine = world.machine();

        let point = |x, y| Value::structure("Point", [("x", Value::int(x)), ("y", Value::int(y))]);
        let held = Value::map([
            (MapKey::Str("a".to_string()), point(1, 2)),
            (MapKey::Str("b".to_string()), point(3, 4)),
        ]);
        let words = from_value(&mut machine, map, &held).unwrap();
        let addr = words[0];
        assert_eq!(machine.object_len(addr), 2, "entries, not words");
        assert_eq!(machine.payload_run(addr, 1, 2), vec![1, 2]);
        assert_eq!(machine.payload_run(addr, 4, 2), vec![3, 4]);
        assert_eq!(
            to_value(&machine, map, &words).unwrap().to_string(),
            "{a: Point(x: 1, y: 2), b: Point(x: 3, y: 4)}"
        );
    }

    /// A `Range` is three words and a range to a reader, both ways. `..` and
    /// `..<` are two ways of writing one family and stay two values, because
    /// `==` compares the bounds a range was written with — so a round trip
    /// that normalised them would answer something the oracle says is a
    /// different value.
    #[test]
    fn a_range_crosses_with_the_bounds_it_was_written_with() {
        let world = World::new(|build, int, boolean, _| {
            build.structure(
                "Range",
                &[("start", int), ("end", int), ("inclusive", boolean)],
            );
        });
        let range = world.named("Range");
        let mut machine = world.machine();

        for (value, shown) in [
            (Value::range_of(0, 3, false), "0..<3"),
            (Value::range_of(0, 3, true), "0..3"),
            (Value::range_of(-2, -2, false), "-2..<-2"),
        ] {
            let words = from_value(&mut machine, range, &value).unwrap();
            let back = to_value(&machine, range, &words).unwrap();
            assert_eq!(back.to_string(), shown);
            assert!(back.eq_value(&value), "{back} is not {value}");
        }

        // The written form survives, so the two are still two values.
        let exclusive = from_value(&mut machine, range, &Value::range_of(1, 4, false)).unwrap();
        let inclusive = from_value(&mut machine, range, &Value::range_of(1, 3, true)).unwrap();
        let exclusive = to_value(&machine, range, &exclusive).unwrap();
        let inclusive = to_value(&machine, range, &inclusive).unwrap();
        assert!(!exclusive.eq_value(&inclusive));
    }

    /// A closure is the one value family this backend makes and cannot hand
    /// over: a public `Value::Closure` carries a body a host may ask to be
    /// called back, and this backend has no way to name one that a host's
    /// callback would find. So it refuses by name rather than answering a
    /// closure nothing could run.
    #[test]
    fn a_closure_is_refused_on_its_way_out() {
        let world = World::new(|build, int, _, _| {
            build.layout(
                "closure",
                Shape::Closure {
                    function: cove_lir::FunctionId(0),
                    captures: vec![int],
                },
            );
        });
        let closure = world.named("closure");
        let mut machine = world.machine();
        let addr = machine.new_object(closure, 0).unwrap();

        let error = to_value(&machine, closure, &[addr]).unwrap_err();
        assert_eq!(
            error.message,
            "a closure cannot cross the boundary out of the linear-memory backend"
        );
    }

    /// A box records the layout of what it holds and holds that value's words
    /// inline, so a boxed `Point` is a two-word payload rather than a
    /// reference to somewhere else again — and a reader looks through it.
    #[test]
    fn a_boxed_value_holds_its_layout_and_then_its_words() {
        let world = World::new(|build, int, _, _| {
            build.structure("Point", &[("x", int), ("y", int)]);
            build.layout("Any", Shape::Boxed);
        });
        let any = world.named("Any");
        let point = world.named("Point");
        let int = world.int;
        let mut machine = world.machine();

        let value = Value::structure("Point", [("x", Value::int(1)), ("y", Value::int(2))]);
        let words = from_value(&mut machine, any, &value).unwrap();
        let addr = words[0];
        assert_eq!(machine.object_layout(addr), any);
        assert_eq!(machine.payload(addr, 0), point.0 as u64);
        assert_eq!(machine.payload_run(addr, 1, 2), vec![1, 2]);
        assert_eq!(
            machine.object_len(addr),
            2,
            "the width, which a box cannot know from its own layout"
        );
        assert_eq!(
            to_value(&machine, any, &words).unwrap().to_string(),
            "Point(x: 1, y: 2)"
        );

        // The same for a scalar: an `Int` written as an `Int` is one inline
        // word, and the same `Int` written as a `dyn` allocates. That is the
        // right place to pay.
        let words = from_value(&mut machine, any, &Value::int(41)).unwrap();
        assert_eq!(machine.payload(words[0], 0), int.0 as u64);
        assert_eq!(to_value(&machine, any, &words).unwrap().to_string(), "41");
    }

    /// A destination that is one `Repr::Ref` word and says nothing more is
    /// the erasure path: the family comes from the value, and a scalar is
    /// boxed to get a description to go with it.
    #[test]
    fn a_scalar_arriving_at_a_reference_location_is_boxed() {
        let world = World::new(|build, _, _, _| {
            build.layout("Any", Shape::Boxed);
            build.word("<ref>", Repr::Ref);
        });
        let mut machine = world.machine();
        let reference = world.named("<ref>");

        let words = from_value(&mut machine, reference, &Value::int(41)).unwrap();
        assert!(matches!(
            world.program.layout(machine.object_layout(words[0])).shape,
            Shape::Boxed
        ));
        assert_eq!(
            to_value(&machine, reference, &words).unwrap().to_string(),
            "41"
        );
        // A string is already one reference, so it crosses as itself rather
        // than through a box.
        let words = from_value(&mut machine, reference, &Value::string("hi")).unwrap();
        assert!(matches!(
            world.program.layout(machine.object_layout(words[0])).shape,
            Shape::Str
        ));
    }

    /// The reason [`Machine::push_temp`] exists.
    ///
    /// The heap is sized so that the array is allocated, then a collection
    /// has to run before its last element can be, while the only thing naming
    /// the array is a Rust local. Without a temporary root the sweep reclaims
    /// it and the writes that follow land in a free block; the round trip then
    /// answers something other than what went in, or nothing at all.
    ///
    /// Under the run-of-words model the *struct* that used to be the outer
    /// object is not an object at all — it is words in a `Vec` the collector
    /// does not walk — so what has to be rooted is every object those words
    /// name, for as long as the conversion runs. That is why `from_value`
    /// takes one mark at the top and releases it at the end rather than
    /// nesting a pair per level, and why this fixture reaches for the one
    /// family that is still an object with references in it.
    #[test]
    fn a_collection_in_the_middle_of_building_does_not_free_what_is_being_built() {
        let world = World::new(|build, _, _, string| {
            build.layout(
                "Array",
                Shape::Elements {
                    elem: string,
                    growable: false,
                },
            );
        });
        let array = world.named("Array");
        // Small enough that filling the array cannot be done without
        // reclaiming, and large enough that it can be done with it.
        let mut machine = Machine::new(&world.program, 56);

        // Garbage nothing roots, so the first collection has something to
        // find and the heap starts nearly full.
        for _ in 0..12 {
            machine.new_string("........................").unwrap();
        }
        assert_eq!(machine.collected().collections, 0);

        let value = Value::array([
            Value::string("aaaaaaaaaaaaaaaa"),
            Value::string("bbbbbbbbbbbbbbbb"),
            Value::string("cccccccccccccccc"),
        ]);
        let words = from_value(&mut machine, array, &value).unwrap();
        assert!(
            machine.collected().collections > 0,
            "the fixture is meant to collect while the value is being built"
        );
        assert_eq!(
            to_value(&machine, array, &words).unwrap().to_string(),
            "[aaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbb, cccccccccccccccc]"
        );
    }

    #[test]
    fn a_family_the_program_does_not_describe_is_refused_by_name() {
        let world = World::new(|build, _, _, _| {
            build.layout("Any", Shape::Boxed);
        });
        let any = world.named("Any");
        let mut machine = world.machine();
        let value = Value::structure("Point", [("x", Value::int(1))]);
        let error = from_value(&mut machine, any, &value).unwrap_err();
        assert_eq!(
            error.message,
            "this program describes no `Point` for a value of that shape to be built as"
        );
    }

    /// A value that is not what the destination's layout declares is refused
    /// rather than written as whatever its words happen to be.
    #[test]
    fn a_value_of_another_family_is_refused_by_the_layout_it_arrived_at() {
        let world = World::new(|build, int, _, _| {
            build.structure("Point", &[("x", int), ("y", int)]);
        });
        let point = world.named("Point");
        let mut machine = world.machine();
        let error = from_value(&mut machine, point, &Value::int(1)).unwrap_err();
        assert_eq!(
            error.message,
            "this value is not the `Point` that was expected here"
        );
    }

    #[test]
    fn a_cycle_is_refused_rather_than_recursed_into() {
        let world = World::new(|build, _, _, _| {
            build.layout(
                "Loop",
                Shape::Elements {
                    elem: LayoutId(0),
                    growable: false,
                },
            );
        });
        // An element layout of a run that holds references to runs: the
        // simplest object that can be made to hold itself.
        let mut build = Build::default();
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;
        let holder = build.layout(
            "Holder",
            Shape::Elements {
                elem: LayoutId(0),
                growable: false,
            },
        );
        let program = {
            let mut program = build.done();
            // The element layout is the holder itself, which is a shape no
            // lowering produces and the shortest way to a cyclic object.
            program.layouts[holder.index()].shape = Shape::Elements {
                elem: holder,
                growable: false,
            };
            program
        };
        let mut machine = Machine::new(&program, 1 << 12);
        let addr = machine.new_object(holder, 1).unwrap();
        machine.set_payload(addr, 0, addr);
        let error = to_value(&machine, holder, &[addr]).unwrap_err();
        assert_eq!(
            error.message,
            "this value nests too deeply to cross the boundary"
        );
        let _ = world;
    }
}
