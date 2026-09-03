//! Where a run of words becomes a public [`Value`], and back.
//!
//! This is the only place in the linear-memory backend that knows what a
//! `Value` is. [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! keeps the materialised `Value` as the host and oracle boundary
//! representation and forbids it everywhere else: not as an operand store,
//! not as a call buffer, not as a spill area, not as a fallback path. Keeping
//! the conversion in a file of its own is how that stays checkable — if
//! anything else in `vm` ever needs `Value`, it has to say so by importing
//! it, and the import is the thing to argue with.
//!
//! Four things cross here and nothing else does: a Host call's arguments and
//! answer, an entry's arguments and answer, a trace capture, and a host
//! callback's arguments and answer — which is the first of those crossed the
//! other way round, and is converted by the same two functions rather than by
//! a path of its own.
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
//! # A host resource crosses as a name
//!
//! A [`Repr::Host`] word is an index into the run's host resource table, and
//! what crosses at one is the [`crate::host::ResourceHandle`] that table
//! holds. [ADR 0013](../../../../docs/adr/0013-host-resource-handles.md)
//! makes a handle a *name* — the host keeps the resource and Cove holds an
//! identity that addresses it, and there is no field for state because the
//! state is not here — so both directions are a lookup and neither is a
//! materialisation. Going out reads the name the word indexes; coming in
//! writes the name down when the run has not been handed it before.
//!
//! [ADR 0031](../../../../docs/adr/0031-a-host-handle-is-not-a-vm-handle.md)
//! is why that table is not the heap. A handle the host owns and a handle the
//! VM owns share a noun and nothing else: only the second is a reference into
//! storage this run allocated, and only the second is a thing a collection
//! may decide the lifetime of. So a resource is never an object here, never a
//! root, and never reachable from one — `Machine::resources` says the rest.
//!
//! [`Repr::Addr`] is refused in both directions and stays refused, for a
//! reason that is not the same one: an address names a word of *this run's*
//! memory and means nothing outside it. [`Repr::Task`] and [`Repr::Scope`]
//! are refused beside it, on the narrower ground that the task-safety rule
//! already keeps either from leaving the task that formed it.
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

use cove_ir::{Layout, LayoutId, Program, Repr, Shape};
use cove_schema::builtins::{MAP, RANGE, SET};

use crate::error::RuntimeError;
use crate::value::{Closure, ClosureBody, LinearClosure, MapKey, Value, ValueView, VectorStorage};
use crate::vm::exec::Machine;

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
            Ok(Value::structure(declared(&described.name), out_fields))
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
            Ok(Value::enumeration(
                declared(&described.name),
                &*case.name,
                payload,
            ))
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
        // The name the word indexes, which is the whole of what a resource is
        // on this side. The host is being handed back something it minted.
        Repr::Host => {
            return match machine.resource(word) {
                Some(handle) => Ok(Value::from_resource(handle.clone())),
                // A frame is zeroed on entry, so a `Host` slot nothing has
                // written reads zero — the same state a `Ref` slot is in when
                // it reads null, reported in the same words.
                None if word == 0 => Err(null_value()),
                None => Err(no_such_resource()),
            };
        }
        // An address cannot leave the machine. It names a word of *this*
        // run's memory — a slot of a live frame, or a field inside an object
        // this heap placed — and means nothing outside it, so a host that was
        // handed one would be holding this run's own bookkeeping.
        //
        // A task handle and a task scope are refused the same way and for a
        // reason of their own: neither may cross a *task* boundary, so
        // neither can cross this one either, and what the word indexes is
        // the scheduler table of one task of one run.
        Repr::Addr | Repr::Task | Repr::Scope => {
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
            let widths = (program.layout(key).width(), program.layout(value).width());
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
        // A closure crosses as itself, and what makes that answerable is
        // that [`crate::value::ClosureBody`] can now name a `cove-ir`
        // function. It could not before: the only lowered variant named the
        // *predecessor's* program, so building one here would have answered a
        // closure whose body a host's callback would go looking for in a
        // program this run does not have — a wrong answer rather than a
        // missing one.
        //
        // Three things are read off the object and nothing is copied out of
        // it. The callee is payload word 0, exactly as `Inst::CallClosure`
        // reads it. `Closure::arity` and `is_async` are the callee's own,
        // because a host reads them to decide whether and how to call — and
        // `Closure::captures` stays empty, because a `cove-ir` closure's
        // captures are inline in this object at the widths its layout says,
        // and copying them into a `Vec<(name, Value)>` would materialise
        // values nothing asked for and lose the storage they came from.
        //
        // The object is pinned rather than left to the frame that built it.
        // The lowering ends a temporary's live range at its last use, which
        // for a closure handed to a host is the instruction after the call,
        // and the `Reentry` contract says a host may keep a callback for
        // later. So the value roots the object for as long as it exists; see
        // [`crate::vm::mem::Rooted`].
        Shape::Closure { .. } => {
            let callee = machine.callee_of(addr)?;
            let target = program.function(callee);
            Ok(Value(crate::value::Repr::Closure(std::rc::Rc::new(
                Closure {
                    is_async: target.is_async,
                    arity: target.params.len(),
                    module: std::rc::Rc::from(&*target.module),
                    captures: Vec::new(),
                    body: ClosureBody::Linear(LinearClosure {
                        function: callee,
                        env: machine.pin(addr),
                    }),
                },
            ))))
        }
        // A cell does not cross, and this is a refusal rather than a gap
        // waiting to be filled. A public `Value`'s `Shared` is an
        // `Arc<SharedCell>` holding a `Transfer` — one cell — and this one is
        // an object of this run's heap. Building the first from the second
        // would build a *second* cell holding a copy of the words, and a
        // `Shared` whose identity is not the identity it was made with is not
        // the value the language describes. What crosses in every program that
        // asks is what `lock` hands its closure, which is an ordinary value.
        Shape::Shared { .. } => Err(RuntimeError::new(
            "a `Shared` names one cell of this run's heap, so it cannot be handed out as a value",
        )
        .with_rule(
            "`lock` is a `Shared`'s only operation: every access to the value it holds is scoped, so there is no `get` and no `set`.",
        )
        .with_help("hand out what a `lock` reads rather than the cell")),
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
            // a `Point` cannot be matched to each other's layout, and the
            // *declared* name on this side, for the reason [`declared`] gives.
            if view.type_name() != declared(&described.name) {
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
            if view.type_name() != declared(&name) {
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
            // A closure this run made, handed back. It is already an object
            // in this heap, so the word is its address and there is nothing
            // to materialise: `layout_for` below could not answer for it in
            // any case, because what family a closure belongs to is the
            // callee's business rather than a value's.
            if let Some(addr) = linear_closure(machine, value)? {
                return Ok(addr);
            }
            let layout = held_layout(machine, value)?;
            let held = into(machine, layout, value, depth)?;
            if one_address(machine.program(), layout) && held.len() == 1 {
                Ok(held[0])
            } else {
                let id = boxed_layout(machine.program())?;
                boxed(machine, id, layout, &held)
            }
        }
        // A handle is a name, and the table is where this run keeps the
        // names it has been given, so crossing in is a lookup that writes the
        // name down when it is new. Nothing here allocates: a resource is not
        // an object, so there is no window in which a half-built thing needs
        // rooting and no collection this can provoke.
        //
        // The refusal is unreachable from a host that answered its schema.
        // `HostRegistry` holds an operation to the type it declared before
        // this runs, and a `HostType::Named` is admitted only by a handle
        // whose qualified type is that name.
        Repr::Host => match value.resource() {
            Some(handle) => Ok(machine.resource_word(handle)),
            None => Err(RuntimeError::new(
                "this value is not the host resource that was expected here",
            )),
        },
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
        // that way and [`crate::vm::builtins::key`] reproduces the order it
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
            let held = held_layout(machine, value)?;
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

/// The family a box records for a value crossing in at an erased position.
///
/// [`layout_for`] is the general answer and it reads the value's own
/// description, which is not always enough to name one family. Two families
/// can describe a value equally well and be different runs of words:
/// `Result<Int, Error>` and `Result<http.Response, Error>` both describe
/// `Err(Error("no"))` exactly, and nothing about that value says which of
/// them it is. The tag decides whether an [`crate::Inst::Unbox`] at the use
/// succeeds, so "whichever the lowering interned first" is a wrong answer
/// rather than an arbitrary one.
///
/// A callback's answer is the case where the machine already knows. It is a
/// value that left this machine a moment ago, at a layout the callback's
/// declaration fixed, and a host that declared its result `Any` — which is
/// what `clock.timeout` does — hands that same value straight back. So the
/// family it left with is preferred over anything a search could find, and
/// [`Machine::callback_answer`] is where the way out wrote it down.
///
/// It is still checked against the value rather than trusted blind. A host
/// may answer something of its own after running a callback, and a family
/// that does not describe what arrived is not this value's family whoever
/// suggested it.
fn held_layout(machine: &Machine, value: &Value) -> Result<LayoutId, RuntimeError> {
    if let Some(id) = machine.callback_answer() {
        if fits(machine.program(), id, value, Precision::Described) {
            return Ok(id);
        }
    }
    layout_for(machine.program(), value)
}

/// The layout a value of unknown static type is built to.
///
/// Only erasure asks: a destination whose layout is known builds to it. The
/// search is [`family_of`] and it is asked twice, because a family that
/// erases somewhere admits every value a family that describes it does. See
/// [`Precision`].
fn layout_for(program: &Program, value: &Value) -> Result<LayoutId, RuntimeError> {
    match family_of(program, value, Precision::Described) {
        Ok(id) => Ok(id),
        // Nothing describes it, so a family that erases somewhere is the
        // answer if there is one. The refusal is the second pass's, because
        // it is the one that looked everywhere.
        Err(_) => family_of(program, value, Precision::Erased),
    }
}

/// How closely a family has to describe a value before it is that value's
/// family.
///
/// A `Shape::Boxed` position admits *every* value, so a family with one in it
/// admits every value the described family does and the search has two
/// answers where it needs one. Which of them is right is not a coin toss:
/// what the box's tag is for is [`crate::Inst::Unbox`], which compares layout
/// ids exactly and is asked for a layout a *static type* named — and no
/// static type names `Result<Any, Error>`'s `Ok`, because that is precisely
/// the position the type declined to describe. So the described family is
/// looked for first and the erasing one is the fallback.
///
/// This matters exactly where a schema nests an `Any`. `clock.timeout`
/// declares `Result<Any, Error>` and a body under it answers
/// `Result<http.Response, Error>`; both are families in the table, both admit
/// the value, and only one of them is what the program's annotation says to
/// open it at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Precision {
    /// Every position describes the value that stands in it. A
    /// `Shape::Boxed` describes nothing, so it admits nothing.
    Described,
    /// A position may be erased, which is what a family holding a box there
    /// is.
    Erased,
}

/// The family a value belongs to, at one precision.
///
/// This reads the value's own description — a struct's declared type name, an
/// enum's name and case, an array's elements — and looks it up in the
/// program's table of *families*, so `Array<Int>` and `Array<String>` are told
/// apart by whether the element layout admits the elements. What `precision`
/// decides is whether an erased position counts as describing one; see
/// [`layout_for`], which asks this at both.
fn family_of(
    program: &Program,
    value: &Value,
    precision: Precision,
) -> Result<LayoutId, RuntimeError> {
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
                declared(&layout.name) == name
                    && fields.len() == view.len()
                    && fields.iter().all(|field| {
                        view.field(&field.name)
                            .is_some_and(|v| fits(program, field.layout, v, precision))
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
                if declared(&layout.name) != name {
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
                        .all(|(part, held)| fits(program, part.layout, held, precision))
            })
            .ok_or_else(|| unknown_family(name))
        }
        ValueView::Array(items) => layout_for_run(program, items, false, precision),
        ValueView::Vector(items) => find(program, |layout| {
            let Shape::Vector { elem } = layout.shape else {
                return false;
            };
            items
                .iter()
                .all(|item| fits(program, elem, item, precision))
        })
        .ok_or_else(|| unknown_family("Vector")),
        ValueView::Set(items) => {
            let items: Vec<Value> = items.iter().map(MapKey::to_value).collect();
            find(program, |layout| {
                let Shape::Members { elem } = layout.shape else {
                    return false;
                };
                items
                    .iter()
                    .all(|item| fits(program, elem, item, precision))
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
                held.iter().all(|(one, other)| {
                    fits(program, key, one, precision) && fits(program, value, other, precision)
                })
            })
            .ok_or_else(|| unknown_family(MAP.name))
        }
        ValueView::Range(_) => find(program, |layout| is_range(program, layout))
            .ok_or_else(|| unknown_family(RANGE.name)),
        // One family for every kind of resource, because a `Repr::Host` word
        // is a `Repr::Host` word — the same reason `Array<String>` and
        // `Array<Point>` are one layout. What the refusal names is the *kind*
        // the host handed over, which is what a reader who has to go and
        // find out needs; the program is missing the word, and the handle is
        // how it would have come by one.
        ValueView::Resource(handle) => {
            layout_of(program, |shape| shape == &Shape::Word(Repr::Host))
                .ok_or_else(|| unknown_family(&handle.qualified_type()))
        }
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
    precision: Precision,
) -> Result<LayoutId, RuntimeError> {
    find(program, |layout| {
        let Shape::Elements {
            elem,
            growable: is_growable,
        } = layout.shape
        else {
            return false;
        };
        is_growable == growable
            && items
                .iter()
                .all(|item| fits(program, elem, item, precision))
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
///
/// `precision` is carried the whole way down, because the position that
/// erases may be at any depth: a `Result<Any, Error>`'s is one part of one
/// case.
fn fits(program: &Program, layout: LayoutId, value: &Value, precision: Precision) -> bool {
    let described = program.layout(layout);
    match &described.shape {
        Shape::Word(Repr::Unit) => value.is_unit(),
        Shape::Word(Repr::Bool) => value.as_bool().is_some(),
        Shape::Word(Repr::Int) => value.as_int().is_some(),
        Shape::Word(Repr::Float) => value.as_float().is_some(),
        Shape::Word(Repr::Duration) => value.as_duration_nanos().is_some(),
        // What a boundary may put behind a reference: every family a `Value`
        // carries and nothing this run owns for itself.
        Shape::Word(Repr::Ref) => materialisable(&value.view()),
        // A box holds the words of what it was given, and a host word is one
        // of them — so an erased resource is a box over one word that is not
        // a root, and the collector reads its held layout's map and finds
        // nothing to follow. What a resource may *not* be is the thing behind
        // a `Repr::Ref`: it is no object this run allocated, so there would be
        // nothing at the far end of the address.
        //
        // And it says yes only where the search has already said it will
        // settle for a family that describes this position with nothing,
        // because otherwise it says yes to everything. See [`Precision`].
        Shape::Boxed => {
            let view = value.view();
            precision == Precision::Erased
                && (materialisable(&view) || matches!(view, ValueView::Resource(_)))
        }
        // The one word a host is on both ends of. See `word_out`.
        Shape::Word(Repr::Host) => matches!(value.view(), ValueView::Resource(_)),
        // Not a value a host holds. See `word_out`.
        Shape::Word(Repr::Addr | Repr::Task | Repr::Scope) | Shape::Free => false,
        Shape::Str => matches!(value.view(), ValueView::Str(_)),
        Shape::Struct { fields, .. } if is_range(program, described) => {
            matches!(value.view(), ValueView::Range(_)) && fields.len() == 3
        }
        Shape::Struct { fields, .. } => match value.view() {
            ValueView::Struct(view) => {
                view.type_name() == declared(&described.name)
                    && fields.len() == view.len()
                    && fields.iter().all(|field| {
                        view.field(&field.name)
                            .is_some_and(|v| fits(program, field.layout, v, precision))
                    })
            }
            _ => false,
        },
        Shape::Enum { cases, .. } => match value.view() {
            ValueView::Enum(view) => {
                view.type_name() == declared(&described.name)
                    && cases.iter().any(|case| {
                        &*case.name == view.case()
                            && case.parts.len() == view.payload().len()
                            && case
                                .parts
                                .iter()
                                .zip(view.payload())
                                .all(|(part, held)| fits(program, part.layout, held, precision))
                    })
            }
            _ => false,
        },
        Shape::Elements { elem, growable } => match value.view() {
            ValueView::Array(items) if !growable => items
                .iter()
                .all(|item| fits(program, *elem, item, precision)),
            ValueView::Vector(items) if *growable => items
                .iter()
                .all(|item| fits(program, *elem, item, precision)),
            _ => false,
        },
        Shape::Vector { elem } => match value.view() {
            ValueView::Vector(items) => items
                .iter()
                .all(|item| fits(program, *elem, item, precision)),
            _ => false,
        },
        Shape::Members { elem } => match value.view() {
            ValueView::Set(items) => items
                .iter()
                .all(|item| fits(program, *elem, &item.to_value(), precision)),
            _ => false,
        },
        Shape::Entries { key, value: held } => match value.view() {
            ValueView::Map(entries) => entries.iter().all(|(one, other)| {
                fits(program, *key, &one.to_value(), precision)
                    && fits(program, *held, other, precision)
            }),
            _ => false,
        },
        // Neither of the two a `Value` cannot be turned into words: a
        // closure's environment is this run's object graph, and a cell's
        // identity is a place in this run's heap.
        Shape::Closure { .. } | Shape::Shared { .. } => false,
    }
}

/// Which function a host's callback runs, and the environment it reads its
/// captures out of.
///
/// Both come off the value rather than one off the value and one out of the
/// object: naming a `cove-ir` function is the whole of what
/// [`ClosureBody::Linear`] added, and a way back that went looking for the
/// callee in the object again would be reading a word to answer a question
/// the value already answers. The object is still what the captures are read
/// from, because that is where they are.
///
/// The refusal is [`crate::host::NoReentry`]'s sentence with this run named
/// in place of the missing program, because it is the same situation from the
/// host's side: what it is holding is not something the run it is inside can
/// call.
pub(crate) fn callback_target(
    machine: &Machine,
    callee: &Value,
) -> Result<(cove_ir::FunctionId, u64), RuntimeError> {
    let Value(crate::value::Repr::Closure(closure)) = callee.erased() else {
        return Err(not_a_callback(callee));
    };
    let ClosureBody::Linear(body) = &closure.body else {
        return Err(not_a_callback(callee));
    };
    if !machine.holds(&body.env) {
        return Err(RuntimeError::new(
            "this closure belongs to another run and cannot be called back into this one",
        ));
    }
    if body.function.index() >= machine.program().functions.len() {
        return Err(RuntimeError::new(
            "this closure names a function this program does not have",
        ));
    }
    Ok((body.function, body.env.addr()))
}

fn not_a_callback(callee: &Value) -> RuntimeError {
    RuntimeError::new(format!(
        "this host call cannot run {}, because it is not a callback of this run",
        callee.type_name()
    ))
}

/// The environment object of a closure *this* run made, if `value` is one.
///
/// `Ok(None)` is "not a closure at all", which is the ordinary case and the
/// caller's to go on from. The refusals are the two ways a closure can be the
/// wrong one: a body only the other backend can run, and an object of another
/// run's address space — where the same number names a different object, so
/// reading through it would be a wrong answer rather than a missing one.
fn linear_closure(machine: &Machine, value: &Value) -> Result<Option<u64>, RuntimeError> {
    let Value(crate::value::Repr::Closure(closure)) = value.erased() else {
        return Ok(None);
    };
    let ClosureBody::Linear(body) = &closure.body else {
        return Err(RuntimeError::new(
            "this closure was made by another backend and cannot cross into the linear-memory one",
        ));
    };
    if !machine.holds(&body.env) {
        return Err(RuntimeError::new(
            "this closure belongs to another run and cannot cross into this one",
        ));
    }
    Ok(Some(body.env.addr()))
}

/// Whether a value is one of the families this backend can build in the heap.
///
/// The question a reference location asks, and it is about the `Value` and
/// not about a layout: a `Repr::Ref` word names an object, and these are the
/// values an object can be made of. What is missing from the list is what
/// this run owns for itself or does not own at all — a closure, a task, a
/// task scope, a host module, a host operation, a type, a `Shared`, and a
/// resource, which is the host's and is one word rather than an object.
fn materialisable(view: &ValueView<'_>) -> bool {
    matches!(
        view,
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
    )
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

/// The name a *value* of a layout carries, which is the declaration's and not
/// the instantiation's.
///
/// [`cove_ir`] names a layout by the instantiation it is: `m.Boxed<Int>` and
/// `m.Boxed<String>` are two names because they are two widths, and that
/// identity is the whole of why monomorphisation is the representation this
/// machine can have. A [`Value`] carries no such thing. A host writes
/// `Value::structure("m.Boxed", ...)` because the type argument is not
/// something it is in a position to know it is supplying, `crate::invoke`
/// checks a nominal type by the declared name on both backends, and the
/// oracle answers that name for a value it produces. So the two are compared
/// on the declaration they agree about, and *which* instantiation a value
/// belongs to is settled by the fields — which every one of these comparisons
/// goes on to check anyway, at the width the layout says.
///
/// The first `<` after the first character, so that the layout table's own
/// bracketed names — `<free>`, `<ref>` — are left whole rather than reduced
/// to nothing.
pub(crate) fn declared(name: &str) -> &str {
    match name.char_indices().find(|(at, ch)| *at > 0 && *ch == '<') {
        Some((at, _)) => &name[..at],
        None => name,
    }
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
    field: &cove_ir::Field,
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
/// every one of them passed [`crate::vm::builtins::key`]'s check before it
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

/// A `Repr::Host` word that indexes nothing.
///
/// Not something a program can bring about. The word was written by this file
/// from a handle the host had already given, and the table it indexes only
/// grows, so a word past its end came from somewhere that is not this run —
/// a lowering that reused a slot at a `Repr` it was not fixed at, or a word
/// read as a `Host` that was never written as one. Reporting it is what keeps
/// that from being answered with whichever resource the number landed on.
fn no_such_resource() -> RuntimeError {
    RuntimeError::new("this value names a host resource this run was never handed")
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
    use crate::vm::exec::tests::Build;
    use cove_ir::{Repr, Shape};

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
    /// [`crate::vm::builtins::key`] is the same order over words.
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

    /// A world holding one lambda, a closure layout over it, and the one-word
    /// family a location holding a function value has.
    ///
    /// `fn(n: Int) -> Int { n }`, with one capture — a body nothing here
    /// runs, because what these tests are about is the value that names it.
    /// The `Fn` layout is `Word(Ref)` for the reason `docs/LINEAR_VM.md`
    /// gives: the location holding a function value is one reference word
    /// under one layout for every signature, and which environment that word
    /// names is the object header's business.
    fn with_a_lambda() -> World {
        World::new(|build, int, _, _| {
            let lambda = build.lambda(
                "lambda",
                &[int],
                &[Repr::Int, Repr::Int],
                int,
                &[int],
                vec![cove_ir::Inst::Return { src: 0 }],
            );
            build.word("Fn", Repr::Ref);
            build.layout(
                "closure",
                Shape::Closure {
                    function: lambda,
                    captures: vec![int],
                },
            );
        })
    }

    /// An environment object of `world`'s closure layout, holding `capture`.
    fn an_environment(machine: &mut Machine<'_>, closure: LayoutId, capture: u64) -> u64 {
        let addr = machine.new_object(closure, 0).unwrap();
        // Payload word 0 is the callee, and the captures follow it inline.
        machine.set_payload(addr, 0, 0);
        machine.set_payload(addr, 1, capture);
        addr
    }

    /// A closure crosses out as a closure, carrying what a host reads of one.
    ///
    /// It could not before: the only lowered `ClosureBody` named a function
    /// of the *predecessor's* program, so answering one here would have been
    /// a callback whose body a host would go looking for in a program this
    /// run does not have. `ClosureBody::Linear` is what names this one, and
    /// the arity and the `async` flag are the callee's own, because deciding
    /// whether and how to call is what a host reads them for.
    #[test]
    fn a_closure_crosses_out_as_a_closure() {
        let world = with_a_lambda();
        let closure = world.named("closure");
        let mut machine = world.machine();
        let addr = an_environment(&mut machine, closure, 7);

        let value = to_value(&machine, closure, &[addr]).unwrap();
        let ValueView::Closure(view) = value.view() else {
            panic!("a closure crosses as a closure: {value:?}");
        };
        assert_eq!(view.arity(), 1, "the callee's own parameter count");
        assert!(!view.is_async());
    }

    /// And back in as the address it already is.
    ///
    /// A closure is an object of this heap, so nothing is materialised on the
    /// way in: the word is the address it went out as, and a host that hands
    /// a callback back is handing back the thing rather than a copy of it.
    #[test]
    fn a_closure_crosses_back_in_as_the_object_it_is() {
        let world = with_a_lambda();
        let (closure, reference) = (world.named("closure"), world.named("Fn"));
        let mut machine = world.machine();
        let addr = an_environment(&mut machine, closure, 7);

        let value = to_value(&machine, closure, &[addr]).unwrap();
        assert_eq!(
            from_value(&mut machine, reference, &value).unwrap(),
            vec![addr]
        );
    }

    /// The value keeps its object alive, and nothing in a frame does.
    ///
    /// This is the whole of the rooting question. The lowering ends a
    /// temporary's live range at its last use — for a closure handed to a
    /// host, the instruction after the call — and the `Reentry` contract says
    /// a host may keep the callback for later. So a collection with no frame
    /// on the stack at all must still find the object, and must stop finding
    /// it once the value is dropped.
    #[test]
    fn a_closure_a_host_holds_survives_a_collection() {
        let world = with_a_lambda();
        let closure = world.named("closure");
        let mut machine = world.machine();
        let addr = an_environment(&mut machine, closure, 4242);
        let value = to_value(&machine, closure, &[addr]).unwrap();

        machine.collect();
        assert_eq!(machine.object_layout(addr), closure);
        assert_eq!(
            machine.payload(addr, 1),
            4242,
            "the value is the only thing naming it, and it is enough"
        );

        drop(value);
        machine.collect();
        assert_eq!(
            machine.object_layout(addr),
            cove_ir::LayoutId::FREE,
            "and the object goes when the last holder does"
        );
    }

    /// Two holders are two claims, so the first to go does not take the
    /// object with it.
    #[test]
    fn a_cloned_closure_value_keeps_its_own_claim() {
        let world = with_a_lambda();
        let closure = world.named("closure");
        let mut machine = world.machine();
        let addr = an_environment(&mut machine, closure, 11);
        let value = to_value(&machine, closure, &[addr]).unwrap();
        let copy = value.clone();

        drop(value);
        machine.collect();
        assert_eq!(machine.object_layout(addr), closure);
        drop(copy);
        machine.collect();
        assert_eq!(machine.object_layout(addr), cove_ir::LayoutId::FREE);
    }

    /// A closure of another run is refused rather than read through.
    ///
    /// An address is a word index into one address space, and the same number
    /// names a different object in the next one. Two runs in one process is
    /// an ordinary thing for an embedder to do, so this is asked rather than
    /// assumed.
    #[test]
    fn a_closure_of_another_run_cannot_cross_in() {
        let world = with_a_lambda();
        let (closure, reference) = (world.named("closure"), world.named("Fn"));

        let value = {
            let mut elsewhere = world.machine();
            let addr = an_environment(&mut elsewhere, closure, 1);
            to_value(&elsewhere, closure, &[addr]).unwrap()
        };

        let mut machine = world.machine();
        let error = from_value(&mut machine, reference, &value).unwrap_err();
        assert_eq!(
            error.message,
            "this closure belongs to another run and cannot cross into this one"
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

    // ---- host resources ------------------------------------------------

    /// A handle a host issued, written out rather than minted through
    /// `ResourceHandle::new` so that a fixture needs no `ResourceSchema` to
    /// name one resource. Every field is part of the name (ADR 0013).
    fn issued(id: u64) -> crate::host::ResourceHandle {
        crate::host::ResourceHandle {
            module: "vault".to_string(),
            type_name: "Reader".to_string(),
            id,
            task_safe: true,
        }
    }

    /// A fixture whose one family is the host word.
    fn vault() -> World {
        World::new(|build, _, _, _| {
            build.word("vault.Reader", Repr::Host);
        })
    }

    /// A resource crosses out as the name the host gave it, and as nothing
    /// else: the host keeps the state, so there is nothing else to hand over.
    #[test]
    fn a_resource_crosses_out_as_the_name_the_host_gave_it() {
        let world = vault();
        let reader = world.named("vault.Reader");
        let mut machine = world.machine();
        let handle = issued(7);
        let word = machine.resource_word(&handle);

        let value = to_value(&machine, reader, &[word]).unwrap();
        assert_eq!(value.to_string(), "<vault.Reader#7>");
        assert!(
            value
                .resource()
                .is_some_and(|named| named.names_same(&handle)),
            "the same resource, by every field of the name"
        );
        assert!(
            matches!(value.view(), ValueView::Resource(_)),
            "a host reads it as the resource it is"
        );
    }

    /// A `Host` slot a frame's own zeroing left alone reads as no resource,
    /// and says so in the words a null reference says it in. Zero cannot be
    /// an index, which is why the word is one past one.
    #[test]
    fn an_unwritten_host_slot_names_no_resource() {
        let world = vault();
        let reader = world.named("vault.Reader");
        let mut machine = world.machine();
        assert_ne!(machine.resource_word(&issued(1)), 0, "zero is no resource");

        let error = to_value(&machine, reader, &[0]).unwrap_err();
        assert_eq!(error.message, "this value was read before it was given one");
        let error = to_value(&machine, reader, &[99]).unwrap_err();
        assert_eq!(
            error.message,
            "this value names a host resource this run was never handed"
        );
    }

    /// A resource crosses in as one word, and one resource is one word: two
    /// handles that name the same resource index the same entry, so comparing
    /// the words is comparing the resources.
    #[test]
    fn a_resource_crosses_in_as_one_word_for_one_resource() {
        let world = vault();
        let reader = world.named("vault.Reader");
        let mut machine = world.machine();

        let one = from_value(&mut machine, reader, &Value::from_resource(issued(1))).unwrap();
        assert_eq!(one.len(), 1, "a handle is a name, and a name is one word");
        assert_ne!(one[0], 0);
        let again = from_value(&mut machine, reader, &Value::from_resource(issued(1))).unwrap();
        assert_eq!(again, one, "the same resource is the same word");
        let other = from_value(&mut machine, reader, &Value::from_resource(issued(2))).unwrap();
        assert_ne!(other, one, "another resource is another word");

        // Nothing was allocated, because a resource is not an object. This is
        // the whole of ADR 0031's distinction, measured: the heap does not
        // know a resource crossed.
        assert_eq!(machine.allocated_words(), 0);
        assert_eq!(
            to_value(&machine, reader, &one).unwrap().to_string(),
            "<vault.Reader#1>"
        );
    }

    /// A value that is not a resource is refused where one was declared.
    ///
    /// Unreachable from a host that answered its schema — `HostRegistry`
    /// holds an operation to the type it declared, and a `HostType::Named` is
    /// admitted only by a handle whose qualified type is that name.
    #[test]
    fn a_value_that_is_not_a_resource_is_refused_at_a_host_word() {
        let world = vault();
        let reader = world.named("vault.Reader");
        let mut machine = world.machine();
        let error = from_value(&mut machine, reader, &Value::int(1)).unwrap_err();
        assert_eq!(
            error.message,
            "this value is not the host resource that was expected here"
        );
    }

    /// An erased resource is a box over one word that is not a root, so a
    /// collection reads the held layout's map and finds nothing to follow.
    #[test]
    fn an_erased_resource_is_a_box_over_a_word_the_collector_cannot_follow() {
        let world = World::new(|build, _, _, _| {
            build.word("vault.Reader", Repr::Host);
            build.boxed();
        });
        let reader = world.named("vault.Reader");
        let any = world.program.boxed_layout;
        let mut machine = world.machine();

        let words = from_value(&mut machine, any, &Value::from_resource(issued(3))).unwrap();
        let addr = words[0];
        assert_eq!(machine.payload(addr, 0), reader.0 as u64, "what it holds");
        assert!(
            !world
                .program
                .layout(reader)
                .may_hold_refs(&world.program.layouts),
            "a host word is not a reference, so the box traces nothing"
        );
        // A reader looks through the box, exactly as it looks through a `dyn`.
        assert_eq!(
            to_value(&machine, any, &words).unwrap().to_string(),
            "<vault.Reader#3>"
        );
    }

    /// An address is refused in both directions and stays refused: it names a
    /// word of this run's memory and means nothing outside it.
    #[test]
    fn an_address_cannot_cross_the_boundary() {
        let world = World::new(|build, _, _, _| {
            build.word("place", Repr::Addr);
        });
        let place = world.named("place");
        let mut machine = world.machine();
        let error = to_value(&machine, place, &[16]).unwrap_err();
        assert_eq!(
            error.message,
            "this value cannot cross the boundary as it is represented"
        );
        // And nothing a host can build is admitted by one either.
        let error = from_value(&mut machine, place, &Value::int(16)).unwrap_err();
        assert_eq!(
            error.message,
            "this value is not the `addr` that was expected here"
        );
    }
}
