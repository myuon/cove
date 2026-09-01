//! The operations the language has but the instruction set does not.
//!
//! A builtin is a method of a type the language ships — `String`, `Array`,
//! `Int` — that is too large or too specific to be an [`Inst`](cove_lir::Inst)
//! and too fixed to be a Host call. [`cove_lir::Builtin`] names one by its
//! receiver and its operation rather than numbering it, because the set of
//! them is the language reference's: adding one is a change here, not a
//! renumbering of the IR.
//!
//! # A builtin is not a boundary
//!
//! It reads the words and the heap objects the machine already holds and
//! answers the words of a value location. Nothing here materialises a public `Value`, and this
//! file does not import one — which is the same check `boundary` makes of the
//! rest of the backend, made of this file. ADR 0034 puts `Value` at the Host
//! boundary and nowhere else, and a builtin is not at it: `"n is {n}"` is
//! Cove talking to itself.
//!
//! # Rendering is the language's, and it is written twice
//!
//! What `{n}` puts in a string is a rule of the language, and the oracle's
//! copy of it is `Display for Value` in [`crate::value`]. That one reads a
//! materialised tree; this one reads the heap. They cannot share an
//! implementation without one of them building what the other exists to
//! avoid, so the rule is written down twice and the differential corpus is
//! what keeps the two copies saying the same thing — the same arrangement
//! [`crate::lvm::exec`]'s arithmetic messages are under, and for the same
//! reason.
//!
//! Two of the rules are facts about a *declaration* rather than about a
//! family, and the layout table carries each of them for that reason: an
//! `export opaque struct` renders as its bare name, and a builtin `Error`
//! renders as its message. Neither can be derived here, because by the time a
//! value is a word the declaration is gone.

use std::fmt::Write as _;

use cove_lir::{Builtin, LayoutId, Repr, Shape};
use cove_schema::builtins::{ERROR, MESSAGE_FIELD};

use crate::lvm::boundary::{is_range, short};
use crate::lvm::builtins::operand::Operand;

use crate::error::RuntimeError;
use crate::lvm::exec::Machine;

mod equal;
mod key;
mod keyed;
mod make;
pub(crate) mod operand;
mod scalar;
mod seq;
mod text;

/// How deep a rendering may nest.
///
/// For the reason [`crate::lvm::boundary`]'s limit exists: an object graph
/// can hold itself and a renderer that met one would recurse until the native
/// stack ran out.
const MAX_DEPTH: usize = 128;

/// Runs `builtin` over `operands`, answering the words it produces.
///
/// Each operand is a value location: the layout the call's argument names and
/// the words at it. A word is untagged, so the pair is the whole of what a
/// builtin has to work from, and reading it out of the frame is the caller's
/// job because only the caller has a frame.
pub(crate) fn call(
    machine: &mut Machine,
    builtin: &Builtin,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    // One match over the pair the IR names, so that teaching the machine an
    // operation is adding an arm and nothing else.
    match (&*builtin.receiver, &*builtin.operation) {
        ("String", "text") => {
            let [operand] = operands else {
                return Err(operand::operands("String.text", 1, operands.len()));
            };
            let text = render_value(machine, operand.layout, operand.words, 0)?;
            machine.new_string(&text).map(one)
        }
        ("String", "concat") => {
            let mut text = String::new();
            for operand in operands {
                match operand::as_word(machine, *operand) {
                    Some((Repr::Ref, word)) if is_string(machine, word) => {
                        text.push_str(&string_of(machine, word)?)
                    }
                    _ => {
                        return Err(RuntimeError::new(
                            "`String.concat` joins strings, and this operand is not one",
                        ))
                    }
                }
            }
            machine.new_string(&text).map(one)
        }
        // What `"{p}"` puts in the string. An operand is a value location, so
        // an inline struct or enum renders as the value it is rather than as
        // its first word — which is what `"{Point(x: 1)}"` answering `1` was.
        ("String", "interpolate") => {
            let mut text = String::new();
            for operand in operands {
                text.push_str(&render_value(machine, operand.layout, operand.words, 0)?);
            }
            machine.new_string(&text).map(one)
        }

        // ---- Array -------------------------------------------------------
        //
        // Every arm below is one line, so that the whole set of operations
        // this backend has been taught reads as a table. What each one means
        // is in the module it delegates to, beside the reading of the oracle
        // it follows.
        ("Array", "get") => seq::array_get(machine, operands),
        ("Array", "length") => seq::array_length(machine, operands).map(one),
        ("Array", "isEmpty") => seq::array_is_empty(machine, operands).map(one),
        ("Array", "contains") => seq::array_contains(machine, operands).map(one),
        ("Array", "indexOf") => seq::array_index_of(machine, operands),
        ("Array", "slice") => seq::array_slice(machine, operands).map(one),
        ("Array", "toVector") => seq::array_to_vector(machine, operands).map(one),

        // ---- Vector ------------------------------------------------------
        ("Vector", "of") => seq::vector_of(machine, operands).map(one),
        ("Vector", "push") => seq::vector_push(machine, operands).map(one),
        ("Vector", "set") => seq::vector_set(machine, operands),
        ("Vector", "pop") => seq::vector_pop(machine, operands),
        ("Vector", "remove") => seq::vector_remove(machine, operands),
        ("Vector", "get") => seq::vector_get(machine, operands),
        ("Vector", "contains") => seq::vector_contains(machine, operands).map(one),
        ("Vector", "indexOf") => seq::vector_index_of(machine, operands),
        ("Vector", "slice") => seq::vector_slice(machine, operands).map(one),
        ("Vector", "length") => seq::vector_length(machine, operands).map(one),
        ("Vector", "isEmpty") => seq::vector_is_empty(machine, operands).map(one),
        ("Vector", "toArray") => seq::vector_to_array(machine, operands).map(one),
        ("Vector", "freeze") => seq::vector_freeze(machine, operands).map(one),

        // ---- Set ---------------------------------------------------------
        //
        // A `Set` and a `Map` are sorted runs, so every one of these is a
        // binary search over `key`'s order or a walk of a run already in it.
        ("Set", "of") => keyed::set_of(machine, operands).map(one),
        ("Set", "length") => keyed::set_length(machine, operands).map(one),
        ("Set", "isEmpty") => keyed::set_is_empty(machine, operands).map(one),
        ("Set", "contains") => keyed::set_contains(machine, operands).map(one),
        ("Set", "toArray") => keyed::set_to_array(machine, operands).map(one),
        ("Set", "inserted") => keyed::set_inserted(machine, operands).map(one),
        ("Set", "removed") => keyed::set_removed(machine, operands).map(one),

        // ---- Map ---------------------------------------------------------
        ("Map", "of") => keyed::map_of(machine, operands).map(one),
        ("Map", "get") => keyed::map_get(machine, operands),
        ("Map", "contains") => keyed::map_contains(machine, operands).map(one),
        ("Map", "length") => keyed::map_length(machine, operands).map(one),
        ("Map", "isEmpty") => keyed::map_is_empty(machine, operands).map(one),
        ("Map", "keys") => keyed::map_keys(machine, operands).map(one),
        ("Map", "values") => keyed::map_values(machine, operands).map(one),
        ("Map", "inserted") => keyed::map_inserted(machine, operands).map(one),
        ("Map", "removed") => keyed::map_removed(machine, operands).map(one),

        // ---- String ------------------------------------------------------
        ("String", "length") => text::length(machine, operands).map(one),
        ("String", "isEmpty") => text::is_empty(machine, operands).map(one),
        ("String", "words") => text::words(machine, operands).map(one),
        ("String", "chars") => text::chars(machine, operands).map(one),
        ("String", "split") => text::split(machine, operands).map(one),
        ("String", "join") => text::join(machine, operands).map(one),
        ("String", "slice") => text::slice(machine, operands).map(one),
        ("String", "trim") => text::trim(machine, operands).map(one),
        ("String", "contains") => text::contains(machine, operands).map(one),
        ("String", "startsWith") => text::starts_with(machine, operands).map(one),
        ("String", "endsWith") => text::ends_with(machine, operands).map(one),
        ("String", "indexOf") => text::index_of(machine, operands),
        ("String", "replace") => text::replace(machine, operands).map(one),
        ("String", "toUpper") => text::to_upper(machine, operands).map(one),
        ("String", "toLower") => text::to_lower(machine, operands).map(one),
        ("String", "fromCodePoint") => text::from_code_point(machine, operands),

        // ---- Int ---------------------------------------------------------
        ("Int", "toFloat") => scalar::int_to_float(machine, operands).map(one),
        ("Int", "abs") => scalar::int_abs(machine, operands).map(one),
        ("Int", "min") => scalar::int_min(machine, operands).map(one),
        ("Int", "max") => scalar::int_max(machine, operands).map(one),
        ("Int", "parse") => scalar::int_parse(machine, operands),
        ("Int", "parseRadix") => scalar::int_parse_radix(machine, operands),

        // ---- Float -------------------------------------------------------
        ("Float", "toInt") => scalar::float_to_int(machine, operands),
        ("Float", "round") => scalar::float_round(machine, operands).map(one),
        ("Float", "abs") => scalar::float_abs(machine, operands).map(one),
        ("Float", "min") => scalar::float_min(machine, operands).map(one),
        ("Float", "max") => scalar::float_max(machine, operands).map(one),
        ("Float", "format") => scalar::float_format(machine, operands).map(one),
        ("Float", "parse") => scalar::float_parse(machine, operands),

        // ---- Duration ----------------------------------------------------
        //
        // Six names, each of which is both a reader and a builder. The guard
        // is the whole of what tells a `Duration` operation from any other
        // name on the same receiver; which of the two it is, is the operand's
        // `Repr`, and `scalar::duration` is where that is read.
        //
        // `Bool` is not below this line because `Bool` has no operations: the
        // schema gives it none beyond `snapshot`, and `!`, `&&` and `||` are
        // instructions rather than builtins.
        ("Duration", name) if scalar::unit(name).is_some() => {
            scalar::duration(machine, name, operands).map(one)
        }

        // ---- equality ----------------------------------------------------
        //
        // `==` on anything that is not one word of scalar bits. The receiver
        // is `Any` because the operation is one rule over every value the
        // language gives an equality, rather than a method a type declares —
        // `crates/cove-runtime/src/builtins.rs` has no entry for it, and
        // `crate::interp` reaches it as an operator.
        ("Any", "equals") => equal::equals(machine, operands).map(one),

        (receiver, operation) => Err(RuntimeError::new(format!(
            "`{receiver}.{operation}` is not an operation this backend has been taught"
        ))
        .with_rule(
            "Every valid checked program runs; an operation this backend has not been taught is a gap in the backend.",
        )),
    }
}

/// The text of `word`, read as `repr`.
///
/// The width-one case of [`render_value`], and what every walk below reaches
/// when it gets down to one word of scalar bits or one address.
fn render(machine: &Machine, repr: Repr, word: u64, depth: usize) -> Result<String, RuntimeError> {
    Ok(match repr {
        Repr::Unit => "()".to_string(),
        Repr::Bool => if word != 0 { "true" } else { "false" }.to_string(),
        Repr::Int => (word as i64).to_string(),
        Repr::Float => float(f64::from_bits(word)),
        Repr::Duration => duration(word as i64),
        Repr::Ref => return render_object(machine, word, depth),
        // Neither is a value: an address is a place and a handle is the
        // host's. Interpolating one would be putting this run's bookkeeping
        // into a string a program prints.
        Repr::Addr | Repr::Host => {
            return Err(RuntimeError::new("this value has no text of its own"))
        }
    })
}

/// The text of the value location of `layout` holding `words`.
///
/// A struct is its fields in place and an enum is a discriminant and a
/// payload region, so rendering one is reading runs of words rather than
/// following an address per field. This is the same walk
/// [`crate::lvm::boundary`] makes, and it is written twice for the reason the
/// module docs give.
fn render_value(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    depth: usize,
) -> Result<String, RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let described = program.layout(layout);
    let mut out = String::new();
    match &described.shape {
        Shape::Word(repr) => return render(machine, *repr, at(words, 0)?, depth),
        // A builtin `Error` renders as the message it carries, not as the
        // struct it happens to be. The oracle special-cases it in
        // `Display for Value` for the reason this one does: a program that
        // prints an error is printing what went wrong, and `Error(message: x)`
        // says the same thing twice. Recognising it by the layout's name is
        // sound because the name is the checker's, and `Error` is a builtin
        // type a module cannot redeclare.
        Shape::Struct { fields, .. }
            if &*described.name == ERROR.name
                && fields.first().map(|field| &*field.name) == Some(MESSAGE_FIELD.name) =>
        {
            let field = &fields[0];
            out.push_str(&render_value(
                machine,
                field.layout,
                run(program, words, field)?,
                deeper,
            )?);
        }
        // An opaque type renders as its name and nothing else. Its fields are
        // the declaring module's own business, and a rendering is read by
        // whoever the string reaches, so showing them here would publish
        // through `println` what the checker refuses to let a caller name.
        // That is ADR 0014's whole point, and it is why the layout carries
        // the flag rather than this deriving it.
        Shape::Struct { opaque: true, .. } => out.push_str(short(&described.name)),
        // A `Range` renders as the operator it was written with: `1..3` and
        // `1..<4` cover the same values and are two different renderings,
        // because they are two different values — `==` on ranges compares the
        // bounds a program wrote, not the set they describe.
        Shape::Struct { .. } if is_range(program, described) => {
            let start = at(words, 0)? as i64;
            let end = at(words, 1)? as i64;
            let operator = if at(words, 2)? != 0 { ".." } else { "..<" };
            write!(out, "{start}{operator}{end}").expect("a string never fails to be written to");
        }
        Shape::Struct { fields, .. } => {
            // The declared name without its module, which is what the
            // public `Display` shows. The layout carries the qualified one
            // because a layout is an identity.
            write!(out, "{}(", short(&described.name))
                .expect("a string never fails to be written to");
            for (nth, field) in fields.iter().enumerate() {
                if nth > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}: ", field.name).expect("a string never fails to be written to");
                out.push_str(&render_value(
                    machine,
                    field.layout,
                    run(program, words, field)?,
                    deeper,
                )?);
            }
            out.push(')');
        }
        // The collector no longer reads the discriminant — the payload
        // region's reference map is static — but a *reader* still must:
        // which of the payload words belong to this value is exactly what
        // the case says.
        Shape::Enum { cases, .. } => {
            let index = at(words, 0)?;
            let case = cases.get(index as usize).ok_or_else(|| {
                RuntimeError::new(format!(
                    "this `{}` is in case {index}, which it does not have",
                    described.name
                ))
            })?;
            out.push_str(&case.name);
            if !case.parts.is_empty() {
                out.push('(');
                for (nth, part) in case.parts.iter().enumerate() {
                    if nth > 0 {
                        out.push_str(", ");
                    }
                    let from = 1 + part.at as usize;
                    let width = program.layout(part.layout).width() as usize;
                    let held = words
                        .get(from..from + width)
                        .ok_or_else(|| short_run(&described.name))?;
                    out.push_str(&render_value(machine, part.layout, held, deeper)?);
                }
                out.push(')');
            }
        }
        Shape::Free => return Err(reclaimed()),
        // Everything left lives in the heap, so the location is one address.
        _ => return render_object(machine, at(words, 0)?, depth),
    }
    Ok(out)
}

/// The text of the object at `addr`.
fn render_object(machine: &Machine, addr: u64, depth: usize) -> Result<String, RuntimeError> {
    if addr == 0 {
        return Err(RuntimeError::new(
            "this value was read before it was given one",
        ));
    }
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let id = machine.object_layout(addr);
    let layout = program.layout(id);
    let mut out = String::new();
    match &layout.shape {
        Shape::Str => out.push_str(&string_of(machine, addr)?),
        // A value whose *object* this is: a layout the lowering broke a
        // recursion at holds the value's own inline words as its payload, and
        // `Layout::payload_words` answers that same width.
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            let words = machine.payload_run(addr, 0, layout.width());
            return render_value(machine, id, &words, depth);
        }
        // A vector renders like an array, because the indirection is what
        // lets it grow without moving and is not a fact about the value:
        // `[1, 2]` is what a program that wrote `Vector.of(1, 2)` sees.
        //
        // The length comes from the vector and not from the store, which is
        // the whole reason the two are separate: a store is as long as the
        // last growth made it, and the elements past the length are the
        // spare room, not the value.
        Shape::Vector { elem } => {
            let len = machine.payload(addr, 0) as u32;
            let store = machine.payload(addr, 1);
            out.push('[');
            if store != 0 {
                out.push_str(&joined(machine, store, *elem, len, ", ", deeper)?);
            }
            out.push(']');
        }
        // An `Array` and a vector's store render alike, which is why one
        // shape covers both — and the stride is the element's width, so an
        // `Array<Point>` renders two words at a time.
        Shape::Elements { elem, .. } => {
            out.push('[');
            out.push_str(&joined(
                machine,
                addr,
                *elem,
                machine.object_len(addr),
                ", ",
                deeper,
            )?);
            out.push(']');
        }
        // A set and a map both render inside braces, which is how the
        // language writes them and why they are ordered families rather than
        // hashed ones: the order is part of what a program sees.
        Shape::Members { elem } => {
            out.push('{');
            out.push_str(&joined(
                machine,
                addr,
                *elem,
                machine.object_len(addr),
                ", ",
                deeper,
            )?);
            out.push('}');
        }
        Shape::Entries { key, value } => {
            let widths = (program.layout(*key).width(), program.layout(*value).width());
            let stride = widths.0 + widths.1;
            out.push('{');
            for nth in 0..machine.object_len(addr) {
                if nth > 0 {
                    out.push_str(", ");
                }
                let one = machine.payload_run(addr, nth * stride, widths.0);
                let other = machine.payload_run(addr, nth * stride + widths.0, widths.1);
                out.push_str(&render_value(machine, *key, &one, deeper)?);
                out.push_str(": ");
                out.push_str(&render_value(machine, *value, &other, deeper)?);
            }
            out.push('}');
        }
        // Erasure is looked through: a `dyn Display` shows the value it
        // holds, because the wrapper is a representation and not something
        // the program put there. Payload word 0 is the layout of what it
        // holds and the words after it are that value, inline.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            let described = program
                .layouts
                .get(held.index())
                .ok_or_else(|| RuntimeError::new("this boxed value carries no known type"))?;
            let words = machine.payload_run(addr, 1, described.width());
            return render_value(machine, held, &words, deeper);
        }
        Shape::Closure { .. } => out.push_str("<fn>"),
        Shape::Free => return Err(reclaimed()),
    }
    Ok(out)
}

/// `len` elements of `elem` from the payload of `addr`, rendered and joined.
fn joined(
    machine: &Machine,
    addr: u64,
    elem: LayoutId,
    len: u32,
    between: &str,
    depth: usize,
) -> Result<String, RuntimeError> {
    let stride = machine.program().layout(elem).width();
    let mut out = String::new();
    for nth in 0..len {
        if nth > 0 {
            out.push_str(between);
        }
        let words = machine.payload_run(addr, nth * stride, stride);
        out.push_str(&render_value(machine, elem, &words, depth)?);
    }
    Ok(out)
}

/// One word, as the run a builtin answering a single word produces.
///
/// A builtin's answer is a value location like any other, so the machine
/// writes a run of words at the destination base slot. Most operations
/// answer one, and this is what says so at the one place that knows which.
fn one(word: u64) -> Vec<u64> {
    vec![word]
}

/// The word at `at` of a value location.
fn at(words: &[u64], at: usize) -> Result<u64, RuntimeError> {
    words
        .get(at)
        .copied()
        .ok_or_else(|| short_run("value location"))
}

/// The words of `field` within a struct's run.
fn run<'w>(
    program: &cove_lir::Program,
    words: &'w [u64],
    field: &cove_lir::Field,
) -> Result<&'w [u64], RuntimeError> {
    let at = field.at as usize;
    let width = program.layout(field.layout).width() as usize;
    words
        .get(at..at + width)
        .ok_or_else(|| short_run(&field.name))
}

fn too_deep() -> RuntimeError {
    RuntimeError::new("this value nests too deeply to render")
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

fn reclaimed() -> RuntimeError {
    RuntimeError::new("this value was read after it was reclaimed")
}

/// Whether the object at `addr` is a string.
pub(super) fn is_string(machine: &Machine, addr: u64) -> bool {
    addr != 0
        && matches!(
            machine.program().layout(machine.object_layout(addr)).shape,
            Shape::Str
        )
}

/// The text of the string object at `addr`.
pub(super) fn string_of(machine: &Machine, addr: u64) -> Result<String, RuntimeError> {
    String::from_utf8(machine.string_bytes(addr))
        .map_err(|_| RuntimeError::new("this string's bytes are not valid UTF-8"))
}

/// Renders a `Float` so that it is never mistaken for an `Int`.
///
/// The language performs no implicit numeric conversions, so a float with no
/// fractional part still shows its point.
fn float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x.is_sign_negative() { "-inf" } else { "inf" }.to_string();
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// Nanoseconds per duration unit, largest first, in the suffixes the lexer
/// accepts.
const DURATION_UNITS: [(i64, &str); 6] = [
    (3_600_000_000_000, "h"),
    (60_000_000_000, "m"),
    (1_000_000_000, "s"),
    (1_000_000, "ms"),
    (1_000, "us"),
    (1, "ns"),
];

/// Renders a `Duration` in the largest unit that divides it exactly.
fn duration(ns: i64) -> String {
    if ns == 0 {
        return "0ns".to_string();
    }
    for (factor, suffix) in DURATION_UNITS {
        if ns % factor == 0 {
            return format!("{}{suffix}", ns / factor);
        }
    }
    unreachable!("every duration is divisible by one nanosecond")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::exec::tests::{budget, Build};
    use cove_lir::{BuiltinId, Inst, LayoutId, Program, Repr, Shape};
    use std::sync::Arc;

    /// The program every builtin test is run against.
    ///
    /// One fixture with every family a builtin reaches for, because a builtin
    /// that answers an `Option<Int>` has to find one in the layout table and a
    /// test that declared only the families it thought it needed would be
    /// testing its own fixture. `pub(super)` so that each module's tests build
    /// their objects into the same world; a hand-written program is the only
    /// kind any of them uses, for the reason
    /// [`crate::lvm::exec::tests::Build`] gives.
    ///
    /// A family is named by a `LayoutId` rather than by a `Repr` now, so the
    /// scalars are declared first and everything else is built out of them —
    /// which is also what makes an `Array<Point>` expressible here at all.
    pub(super) fn world() -> Program {
        let mut build = Build::default();
        let _unit = build.word("Unit", Repr::Unit);
        let boolean = build.word("Bool", Repr::Bool);
        let int = build.word("Int", Repr::Int);
        let float = build.word("Float", Repr::Float);
        let _duration = build.word("Duration", Repr::Duration);
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;

        // An `Error` is its one `String` field, inline: one word.
        let error = build.structure("Error", &[("message", string)]);
        let point = build.structure("Point", &[("x", int), ("y", int)]);

        for elem in [string, int, point] {
            build.layout(
                "Array",
                Shape::Elements {
                    elem,
                    growable: false,
                },
            );
            build.layout(
                "Vector",
                Shape::Elements {
                    elem,
                    growable: true,
                },
            );
            build.layout("Vector", Shape::Vector { elem });
            build.enumeration("Option", &[("None", vec![]), ("Some", vec![elem])]);
        }
        for ok in [int, float, string] {
            build.enumeration("Result", &[("Ok", vec![ok]), ("Err", vec![error])]);
        }
        build.layout("Boxed", Shape::Boxed);
        // A `Range` is a struct with the three fields the design fixes, and
        // it is in here because a key sorts after every other family when it
        // is one.
        build.structure(
            "Range",
            &[("start", int), ("end", int), ("inclusive", boolean)],
        );
        for elem in [int, string] {
            build.layout("Set", Shape::Members { elem });
        }
        build.layout(
            "Map",
            Shape::Entries {
                key: int,
                value: int,
            },
        );
        build.layout(
            "Map",
            Shape::Entries {
                key: string,
                value: int,
            },
        );
        build.structure("MapEntry", &[("key", int), ("value", int)]);
        build.done()
    }

    /// Calls `receiver.operation` over hand-built operands.
    ///
    /// Direct rather than through the dispatch loop: what a builtin reads is
    /// words and heap objects, so building those by hand is what makes a
    /// failure unambiguously the operation's rather than the lowering's or the
    /// loop's. `result` is not read by [`call`] — every builtin finds the
    /// family of its answer in the layout table — and is the free layout
    /// throughout.
    pub(super) fn run(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(Repr, u64)],
    ) -> Result<Vec<u64>, RuntimeError> {
        let held: Vec<(LayoutId, u64)> = operands
            .iter()
            .map(|(repr, word)| (described(machine, *repr, *word), *word))
            .collect();
        let passed: Vec<(LayoutId, &[u64])> = held
            .iter()
            .map(|(layout, word)| (*layout, std::slice::from_ref(word)))
            .collect();
        values(machine, receiver, operation, &passed)
    }

    /// The layout of the value a one-word operand is, as a test hands one
    /// over.
    ///
    /// A scalar's is the fixture's one-word layout for its `Repr`. A
    /// reference's is the layout of the object it names, which is where that
    /// answer has always lived — a `Repr::Ref` says a word is an address and
    /// nothing about what is at the end of it. A null one is a `String`,
    /// which is only a stand-in for "some family that lives in the heap":
    /// what a builtin does with a null reference is refuse it, and every
    /// family refuses it in the same words.
    fn described(machine: &Machine, repr: Repr, word: u64) -> LayoutId {
        match repr {
            Repr::Ref if word == 0 => machine.program().str_layout,
            Repr::Ref => machine.object_layout(word),
            _ => scalar(machine.program(), repr),
        }
    }

    /// Calls `receiver.operation` over operands that are value locations.
    ///
    /// What [`run`] is in terms of, and what a test of a value wider than one
    /// word uses directly: an operand is a layout and the words at a
    /// location, so a `Point` argument is the layout and both of its words.
    pub(super) fn values(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(LayoutId, &[u64])],
    ) -> Result<Vec<u64>, RuntimeError> {
        let passed: Vec<Operand<'_>> = operands
            .iter()
            .map(|(layout, words)| Operand {
                layout: *layout,
                words,
            })
            .collect();
        call(
            machine,
            &Builtin {
                receiver: Arc::from(receiver),
                operation: Arc::from(operation),
                result: LayoutId::FREE,
            },
            &passed,
        )
    }

    /// An operand naming the value location `words` of `layout`.
    ///
    /// What a test writes where it means a value rather than a word: a
    /// `Point` argument is `at(point, &[1, 2])`, and the borrow lives as long
    /// as the call it is an argument of.
    pub(super) fn at(layout: LayoutId, words: &[u64]) -> Operand<'_> {
        Operand { layout, words }
    }

    /// The same, for an operation whose answer is one word.
    pub(super) fn word(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(Repr, u64)],
    ) -> Result<u64, RuntimeError> {
        run(machine, receiver, operation, operands).map(|words| {
            assert_eq!(words.len(), 1, "`{receiver}.{operation}` answers one word");
            words[0]
        })
    }

    /// The text of the string object at `addr`.
    pub(super) fn read(machine: &Machine, addr: u64) -> String {
        String::from_utf8(machine.string_bytes(addr)).expect("a builtin writes valid UTF-8")
    }

    /// The layout of a run of `elem` elements, as a test builds one.
    pub(super) fn elements(program: &Program, elem: LayoutId, growable: bool) -> LayoutId {
        super::make::elements(program, elem, growable).expect("the fixture declares every family")
    }

    /// The layout of a `Vector` header over `elem` elements.
    pub(super) fn vector(program: &Program, elem: LayoutId) -> LayoutId {
        super::make::vector(program, elem).expect("the fixture declares every family")
    }

    /// The one-word layout of `repr`.
    pub(super) fn scalar(program: &Program, repr: Repr) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| layout.shape == Shape::Word(repr))
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every scalar")
    }

    /// The first layout the fixture declares under `name`.
    pub(super) fn named(program: &Program, name: &str) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| &*layout.name == name)
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every family")
    }

    /// The enum called `name` whose case `carrier` holds one `payload`.
    ///
    /// The same search `make::two_case` makes, so a test names an
    /// `Option<Int>` the way a builtin finds one.
    pub(super) fn two_case(
        program: &Program,
        name: &str,
        carrier: &str,
        payload: LayoutId,
    ) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| {
                let Shape::Enum { cases, .. } = &layout.shape else {
                    return false;
                };
                &*layout.name == name
                    && cases.iter().any(|case| {
                        &*case.name == carrier
                            && case.parts.len() == 1
                            && case.parts[0].layout == payload
                    })
            })
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every family")
    }

    /// The case name and payload words of the enum value `words`, read as the
    /// family `layout` describes.
    ///
    /// An enum is inline now, so what a test asserts on is a run of words
    /// rather than an object — and which of the payload words belong to the
    /// value is what the case says.
    pub(super) fn case_of(
        program: &Program,
        layout: LayoutId,
        words: &[u64],
    ) -> (String, Vec<u64>) {
        let described = program.layout(layout);
        let Shape::Enum { cases, .. } = &described.shape else {
            panic!("`{}` is not an enum", described.name);
        };
        let case = &cases[words[0] as usize];
        let mut payload = Vec::new();
        for part in &case.parts {
            let at = 1 + part.at as usize;
            let width = program.layout(part.layout).width() as usize;
            payload.extend_from_slice(&words[at..at + width]);
        }
        (case.name.to_string(), payload)
    }

    /// What the `Option` whose `Some` carries a `payload` holds.
    pub(super) fn option_of(
        program: &Program,
        payload: LayoutId,
        words: &[u64],
    ) -> (String, Vec<u64>) {
        case_of(program, two_case(program, "Option", "Some", payload), words)
    }

    /// What the `Result` whose `Ok` carries an `ok` holds.
    pub(super) fn result_of(program: &Program, ok: LayoutId, words: &[u64]) -> (String, Vec<u64>) {
        case_of(program, two_case(program, "Result", "Ok", ok), words)
    }

    /// The message of the `Error` the `Result` in `words` failed with.
    ///
    /// One dereference rather than two: an `Error` is its `String` field
    /// inline, so the payload word *is* the message's address.
    pub(super) fn message_of(machine: &Machine, ok: LayoutId, words: &[u64]) -> String {
        let (case, payload) = result_of(machine.program(), ok, words);
        assert_eq!(case, "Err", "this `Result` did not fail");
        read(machine, payload[0])
    }

    /// The element words of a run-shaped object at `addr`.
    pub(super) fn words_of(machine: &Machine, addr: u64) -> Vec<u64> {
        let layout = machine.program().layout(machine.object_layout(addr));
        let stride = match layout.shape {
            Shape::Elements { elem, .. } | Shape::Members { elem } => machine.words_of(elem),
            _ => 1,
        };
        machine.payload_run(addr, 0, machine.object_len(addr) * stride)
    }

    /// A one-argument builtin over a value the program can build, run through
    /// the dispatch loop rather than called directly, so what is under test is
    /// the instruction as well as the operation.
    fn text_of(build_value: impl FnOnce(&mut Build) -> (Vec<Repr>, Vec<Inst>)) -> String {
        let mut build = Build::default();
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let (mut reprs, mut code) = build_value(&mut build);
        // The value is in slot 0 by construction; the next slot takes the text.
        // An operand carries the layout of the location it names, and every
        // value this fixture builds is one word of it.
        let held = match reprs[0] {
            Repr::Ref => str_layout,
            repr => build.scalar(repr),
        };
        let operand = build.args(&[(0, held)]);
        let dst = reprs.len() as u32;
        reprs.push(Repr::Ref);
        let builtin = builtin(&mut build.program, "String", "text", str_layout);
        code.push(Inst::CallBuiltin {
            dst,
            builtin,
            args: operand,
        });
        code.push(Inst::Return { src: dst });
        let f = build.function("f", &[], &reprs, str_layout, code);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        String::from_utf8(machine.string_bytes(word[0])).unwrap()
    }

    fn builtin(
        program: &mut Program,
        receiver: &str,
        operation: &str,
        result: LayoutId,
    ) -> BuiltinId {
        program.builtins.push(Builtin {
            receiver: Arc::from(receiver),
            operation: Arc::from(operation),
            result,
        });
        BuiltinId(program.builtins.len() as u32 - 1)
    }

    #[test]
    fn a_scalar_renders_the_way_the_language_shows_it() {
        assert_eq!(
            text_of(|_| (vec![Repr::Int], vec![Inst::Int { dst: 0, value: -12 }])),
            "-12"
        );
        assert_eq!(
            text_of(|_| (
                vec![Repr::Bool],
                vec![Inst::Bool {
                    dst: 0,
                    value: true
                }]
            )),
            "true"
        );
        assert_eq!(
            text_of(|_| (vec![Repr::Unit], vec![Inst::Unit { dst: 0 }])),
            "()"
        );
        // A float never loses its point, and a duration takes the largest
        // unit that divides it.
        assert_eq!(
            text_of(|_| (
                vec![Repr::Float],
                vec![Inst::Float {
                    dst: 0,
                    bits: 4.0f64.to_bits()
                }]
            )),
            "4.0"
        );
        assert_eq!(
            text_of(|_| (
                vec![Repr::Duration],
                vec![Inst::Int {
                    dst: 0,
                    value: 1_500_000_000
                }]
            )),
            "1500ms"
        );
    }

    #[test]
    fn a_string_renders_as_itself_rather_than_quoted() {
        let mut build = Build::default().strings(&["ha"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let operand = build.args(&[(0, str_layout)]);
        let builtin = builtin(&mut build.program, "String", "text", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref, Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: cove_lir::StrId(0),
                },
                Inst::CallBuiltin {
                    dst: 1,
                    builtin,
                    args: operand,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word[0])).unwrap(),
            "ha"
        );
    }

    /// A compound value is a run of words now, so a rendering reads runs
    /// rather than following an address per field — and an `Option<Point>`
    /// shows its `Point` inline, out of the same run.
    #[test]
    fn a_compound_value_renders_the_way_the_oracle_shows_it() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let point = named(&program, "Point");
        let option = two_case(&program, "Option", "Some", point);

        assert_eq!(
            render_value(&machine, point, &[1, (-2i64) as u64], 0).unwrap(),
            "Point(x: 1, y: -2)"
        );
        // `[disc, x, y]`: the `Point` is inline in the payload region.
        assert_eq!(
            render_value(&machine, option, &[1, 1, (-2i64) as u64], 0).unwrap(),
            "Some(Point(x: 1, y: -2))"
        );
        assert_eq!(
            render_value(&machine, option, &[0, 0, 0], 0).unwrap(),
            "None"
        );

        // An `Array<Point>` is a run of two-word elements, walked at that
        // stride.
        let items = machine
            .new_object(elements(&program, point, false), 2)
            .unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4]);
        assert_eq!(
            render(&machine, Repr::Ref, items, 0).unwrap(),
            "[Point(x: 1, y: 2), Point(x: 3, y: 4)]"
        );
        let _ = int;
    }

    #[test]
    fn interpolation_joins_the_text_of_every_operand() {
        let mut build = Build::default().strings(&["n is ", "!"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let ints = build.scalar(Repr::Int);
        let parts = build.args(&[(0, str_layout), (1, ints), (2, str_layout)]);
        let builtin = builtin(&mut build.program, "String", "interpolate", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: cove_lir::StrId(0),
                },
                Inst::Int { dst: 1, value: 7 },
                Inst::Str {
                    dst: 2,
                    text: cove_lir::StrId(1),
                },
                Inst::CallBuiltin {
                    dst: 3,
                    builtin,
                    args: parts,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word[0])).unwrap(),
            "n is 7!"
        );
    }

    #[test]
    fn concat_joins_strings_and_refuses_anything_else() {
        let mut build = Build::default().strings(&["ab", "cd"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let both = build.args(&[(0, str_layout), (1, str_layout)]);
        let joined = builtin(&mut build.program, "String", "concat", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref, Repr::Ref, Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: cove_lir::StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: cove_lir::StrId(1),
                },
                Inst::CallBuiltin {
                    dst: 2,
                    builtin: joined,
                    args: both,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word[0])).unwrap(),
            "abcd"
        );

        // The one thing `concat` is stricter about than `interpolate`: it
        // joins strings, and there are no implicit conversions.
        let mut build = Build::default();
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let ints = build.scalar(Repr::Int);
        let both = build.args(&[(0, ints)]);
        let joined = builtin(&mut build.program, "String", "concat", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Int, Repr::Ref],
            str_layout,
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::CallBuiltin {
                    dst: 1,
                    builtin: joined,
                    args: both,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = Machine::new(&program, 1 << 14)
            .run(f, &[], &budget())
            .unwrap_err();
        assert_eq!(
            error.message,
            "`String.concat` joins strings, and this operand is not one"
        );
    }

    #[test]
    fn an_operation_the_backend_has_not_been_taught_says_so() {
        let mut build = Build::default();
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let none = build.args(&[]);
        let unknown = builtin(&mut build.program, "String", "reverse", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref],
            str_layout,
            vec![
                Inst::CallBuiltin {
                    dst: 0,
                    builtin: unknown,
                    args: none,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let error = Machine::new(&program, 1 << 14)
            .run(f, &[], &budget())
            .unwrap_err();
        assert_eq!(
            error.message,
            "`String.reverse` is not an operation this backend has been taught"
        );
    }

    /// A rendering that allocates once, whatever it renders.
    ///
    /// `Inst::Alloc` is not reached from here: the text is built in Rust and
    /// the heap is touched once, at the end. That is what keeps a builtin
    /// free of the rooting discipline the boundary needs — there is no
    /// half-built object for a collection to land on.
    #[test]
    fn rendering_allocates_exactly_the_answer() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let items = machine
            .new_object(elements(&program, int, false), 3)
            .unwrap();
        for at in 0..3u32 {
            machine.set_payload(items, at, at as u64 + 1);
        }
        let before = machine.allocated_words();
        let word = word(&mut machine, "String", "text", &[(Repr::Ref, items)]).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word)).unwrap(),
            "[1, 2, 3]"
        );
        // One header and one payload word: "[1, 2, 3]" is nine bytes.
        assert_eq!(machine.allocated_words() - before, 3);
        assert_ne!(machine.object_layout(word), LayoutId::FREE);
    }
}
