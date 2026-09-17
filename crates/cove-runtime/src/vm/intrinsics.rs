//! The operations the language has but the instruction set does not.
//!
//! A builtin is a method of a type the language ships — `String`, `Array`,
//! `Int` — that is too large or too specific to be an [`Inst`](cove_ir::Inst)
//! and too fixed to be a Host call. [`cove_ir::IntrinsicSite`] names one by a
//! closed [`cove_ir::Intrinsic`] rather than by a pair of strings matched at
//! run time — see [ADR
//! 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! — so the match below is exhaustive: teaching the machine one more
//! operation is adding a variant to `cove_ir::intrinsic` and an arm here,
//! and there is no longer a name this backend has not been taught to fall
//! through to.
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
//! [`crate::vm::exec`]'s arithmetic messages are under, and for the same
//! reason.
//!
//! Two of the rules are facts about a *declaration* rather than about a
//! family, and the layout table carries each of them for that reason: an
//! `export opaque struct` renders as its bare name, and a builtin `Error`
//! renders as its message. Neither can be derived here, because by the time a
//! value is a word the declaration is gone.

use std::fmt::Write as _;

use cove_ir::{Intrinsic, LayoutId, Repr, Shape};
use cove_schema::builtins::{ERROR, MESSAGE_FIELD};

use crate::vm::boundary::{declared, is_range, short};
use crate::vm::intrinsics::operand::{Dest, Frame};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;

mod equal;
mod key;
#[cfg(test)]
pub(crate) use key::is_ascending_and_distinct;
mod make;
pub(crate) mod operand;
mod scalar;
mod seq;
mod text;

/// How deep a rendering may nest.
///
/// For the reason [`crate::vm::boundary`]'s limit exists: an object graph
/// can hold itself and a renderer that met one would recurse until the native
/// stack ran out.
const MAX_DEPTH: usize = 128;

/// Runs `intrinsic` over the operands `frame` names, writing its answer into
/// `dest`.
///
/// Each operand is a value location in the caller's frame: the layout the
/// call's argument names and the words at its slot. A word is untagged, so
/// the pair is the whole of what a builtin has to work from — and it is read
/// where it is, rather than copied into a buffer first, and the answer is
/// written where it goes, rather than carried home in another (#378, P5-4).
/// See [`Frame`] for the one rule that makes that sound: every operand is
/// read before the answer is written.
pub(crate) fn call(
    machine: &mut Machine,
    intrinsic: Intrinsic,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    // One match over the intrinsic the IR names, so that teaching the
    // machine an operation is adding a variant to `cove_ir::intrinsic` and
    // an arm here. The match is exhaustive: there is no pair left to fall
    // through on, because `Intrinsic` is the closed set this backend has
    // been taught.
    match intrinsic {
        // ---- rendering ---------------------------------------------------
        //
        // What `"{x}"` appends for a piece: an `Int` formatted where it goes,
        // and any other value through the one layout-directed walk. A `String`
        // piece is not here — it is a byte `Inst::GrowableExtend` — and neither
        // is the assembly around them, which is run instructions (#403).
        Intrinsic::ValueRenderInto => render_into(machine, frame, dest),
        Intrinsic::IntRenderInto => int_render_into(machine, frame, dest),

        // ---- Array -------------------------------------------------------
        //
        // Every arm below is one line, so that the whole set of operations
        // this backend has been taught reads as a table.
        //
        // A builtin's answer is a value location like any other, so what an
        // arm produces is a *run of words* at the destination, written there
        // through `Dest`. It used to be a `Vec` per call — an allocation, and
        // 39 ns of an 86 ns call — then a buffer the machine reused and copied
        // into the frame afterwards; now it is neither. What each one means is
        // in the module it delegates to, beside the reading of the oracle it
        // follows.
        //
        // `get` and `length` are not here, for `Array` or `Vector`: the
        // lowering has always answered both with instructions, and neither
        // had a caller.
        // `Array.isEmpty` is not here: it is `std.array.isEmpty`, the first
        // builtin method whose body is Cove rather than a machine builtin —
        // see `cove_schema::builtins::standard_binding` and
        // `cove_ir::lower::methods::Body::call_std_binding`.
        // `contains` and `indexOf` are not here: each is `std.array`, a loop
        // over `==` in Cove.
        // `slice` and `toVector` are not here: they are `std.array` over a
        // word `Inst::RunSlice` and a word `Inst::RunCopy`.

        // ---- Vector ------------------------------------------------------
        // `Vector.of` is not here: the lowering allocates it — see
        // `cove_ir::lower::collections`' `vector_of`.
        // `Vector.push` is not here: it is `std.vector.push` over the core
        // intrinsic that is a word `Inst::GrowablePush` — see
        // `Machine::push_words`.
        // `Vector.set` is not here: it is `std.vector.set`, a range check and
        // an `Option` in Cove over an element `LoadElem` and `StoreElem`.
        // `Vector.pop` and `Vector.remove` are not here: they are `std.vector`
        // over an element load, a word `Inst::RunCopy` of the tail and a word
        // `Inst::GrowableTruncate` — see `Machine::truncate_words`.
        // `Vector.contains` and `Vector.indexOf` are not here: each is
        // `std.vector`, a loop over `==` in Cove through `core.vectorLoad`.
        // `Vector.isEmpty` is not here: it is `std.vector.isEmpty` — see
        // `cove_schema::builtins::standard_binding`.
        // `Vector.freeze` is not here: it is `std.vector.freeze` over the core
        // intrinsic that is a word `Inst::RunFinish` — see
        // `Machine::finish_words`. `slice` and `toArray` are `std.vector` over
        // a word `Inst::RunSlice`.

        // ---- Set and Map -------------------------------------------------
        //
        // Nothing. Every public operation of both — the literals, the
        // searches, the updates and the projections — is `std.set` or
        // `std.map` over the three `Value` intrinsics at the end, run copies
        // and slices, and a keyed finish (ADR 0059, #378 Phase 4).

        // ---- String ------------------------------------------------------
        Intrinsic::StringLength => text::length(machine, frame, dest),
        // `String.isEmpty` is not here: it is `std.string.isEmpty` — see
        // `cove_schema::builtins::standard_binding`.
        Intrinsic::StringWords => text::words(machine, frame, dest),
        Intrinsic::StringChars => text::chars(machine, frame, dest),
        Intrinsic::StringSplit => text::split(machine, frame, dest),
        Intrinsic::StringJoin => text::join(machine, frame, dest),
        Intrinsic::StringSlice => text::slice(machine, frame, dest),
        Intrinsic::StringTrim => text::trim(machine, frame, dest),
        Intrinsic::StringContains => text::contains(machine, frame, dest),
        Intrinsic::StringStartsWith => text::starts_with(machine, frame, dest),
        Intrinsic::StringEndsWith => text::ends_with(machine, frame, dest),
        Intrinsic::StringIndexOf => text::index_of(machine, frame, dest),
        Intrinsic::StringReplace => text::replace(machine, frame, dest),
        Intrinsic::StringToUpper => text::to_upper(machine, frame, dest),
        Intrinsic::StringToLower => text::to_lower(machine, frame, dest),
        Intrinsic::StringFromCodePoint => text::from_code_point(machine, frame, dest),
        // No byte-counted operation is here any more. All three are
        // `std.string`: `byteLength` over the core intrinsic that is an
        // `Inst::Len`, `sliceBytes` over the one that is a byte
        // `Inst::RunSlice`, and `codePointAtByte` a UTF-8 decode in Cove over
        // `byteAt`, which is a byte `Inst::RunLoad`.

        // ---- Int ---------------------------------------------------------
        // `Int.toFloat` is not here: it is an `Inst::Convert`, as both halves
        // of `Duration.nanos` are (#378, P5-2).
        // `Int.min`, `Int.max`, and `Int.abs` are not here: they are
        // `std.int.min`, `std.int.max`, and `std.int.abs` — see
        // `cove_schema::builtins::standard_binding`.
        Intrinsic::IntParse => scalar::int_parse(machine, frame, dest),
        Intrinsic::IntParseRadix => scalar::int_parse_radix(machine, frame, dest),

        // ---- Float -------------------------------------------------------
        Intrinsic::FloatToInt => scalar::float_to_int(machine, frame, dest),
        Intrinsic::FloatRound => {
            scalar::float_round(machine, frame, dest);
            Ok(())
        }
        Intrinsic::FloatAbs => {
            scalar::float_abs(machine, frame, dest);
            Ok(())
        }
        Intrinsic::FloatSqrt => {
            scalar::float_sqrt(machine, frame, dest);
            Ok(())
        }
        Intrinsic::FloatMin => {
            scalar::float_min(machine, frame, dest);
            Ok(())
        }
        Intrinsic::FloatMax => {
            scalar::float_max(machine, frame, dest);
            Ok(())
        }
        Intrinsic::FloatFormat => scalar::float_format(machine, frame, dest),
        Intrinsic::FloatParse => scalar::float_parse(machine, frame, dest),

        // `Bool` has no operations: the schema gives it none beyond
        // `snapshot`, and `!`, `&&` and `||` are instructions rather than
        // builtins.

        // ---- equality ----------------------------------------------------
        //
        // `==` on anything that is not one word of scalar bits. The receiver
        // is `Any` because the operation is one rule over every value the
        // language gives an equality, rather than a method a type declares —
        // `crates/cove-runtime/src/builtins.rs` has no entry for it, and
        // `crate::interp` reaches it as an operator.
        Intrinsic::AnyEquals => equal::equals(machine, frame, dest),

        // ---- keys --------------------------------------------------------
        //
        // ADR 0059's three: the order a keyed search in the standard library
        // asks when one comparison instruction cannot answer it, the
        // admission it asks of a key before anything is compared, and the
        // refusal a literal with a key twice is given. Each answers a word or
        // nothing; the admission's `()` is the zero word.
        Intrinsic::ValueOrder => key::value_order(machine, frame, dest),
        Intrinsic::ValueAdmitKey => key::admit_key(machine, frame, dest),
        Intrinsic::ValueRefuseDuplicate => key::refuse_duplicate(machine, frame),
    }
}

/// `Value.renderInto(buffer)`: what `"{p}"` puts in the string, appended to
/// the byte buffer the interpolation is assembled in.
///
/// An operand is a value location, so an inline struct or enum renders as the
/// value it is rather than as its first word — which is what
/// `"{Point(x: 1)}"` answering `1` was. The piece is rendered into Rust text
/// first and appended once, so a growth of the buffer happens after the walk
/// and never under it.
///
/// The piece is rendered when the lowering calls this, which is right after
/// the piece was evaluated: a later piece that changes what this one showed
/// cannot change its text (#389).
fn render_into(machine: &mut Machine, frame: Frame<'_>, dest: Dest) -> Result<(), RuntimeError> {
    let piece = frame.operand(machine, 0);
    let mut text = String::new();
    render_value(machine, piece.layout, piece.words, 0, &mut text)?;
    let owner = frame.word(machine, 1);
    machine.append_text(owner, text.as_bytes())?;
    dest.word(machine, 0);
    Ok(())
}

/// `Int.renderInto(buffer)`: an `Int` piece's decimal digits, appended to the
/// byte buffer the interpolation is assembled in.
///
/// Formatted on the stack — twenty bytes hold `i64::MIN` and its sign — so
/// the piece allocates nothing of its own, which is the whole difference from
/// [`render_into`] over the same word.
fn int_render_into(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let value = operand::int(machine, frame, 0);
    let owner = frame.word(machine, 1);
    let mut digits = [0u8; 20];
    let text = decimal(value, &mut digits);
    machine.append_text(owner, text)?;
    dest.word(machine, 0);
    Ok(())
}

/// The decimal text of `value`, written into the end of `out`.
///
/// What `write!(out, "{value}")` writes, without a formatter: the digits of
/// the magnitude from the last byte back, then the sign.
fn decimal(value: i64, out: &mut [u8; 20]) -> &[u8] {
    let mut magnitude = value.unsigned_abs();
    let mut at = out.len();
    loop {
        at -= 1;
        out[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        at -= 1;
        out[at] = b'-';
    }
    &out[at..]
}

/// The text of `word`, read as `repr`, appended to `out`.
///
/// The width-one case of [`render_value`], and what every walk below reaches
/// when it gets down to one word of scalar bits or one address.
fn render(
    machine: &Machine,
    repr: Repr,
    word: u64,
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
    match repr {
        Repr::Unit => out.push_str("()"),
        Repr::Bool => out.push_str(if word != 0 { "true" } else { "false" }),
        Repr::Int => write!(out, "{}", word as i64).expect("a string never fails to be written to"),
        Repr::Float => float(out, f64::from_bits(word)),
        Repr::Duration => duration(out, word as i64),
        Repr::Ref => return render_object(machine, word, depth, out),
        // None of them is a value: an address is a place, a handle is the
        // host's, and a task or a scope is the scheduler's. Interpolating one
        // would be putting this run's bookkeeping into a string a program
        // prints.
        //
        // A tag is not a value either, for a reason of its own: it is word 0
        // of an enum and an enum renders whole, through its layout, as the
        // case it holds. A tag reaching here alone is a lowering bug.
        Repr::Addr | Repr::Host | Repr::Task | Repr::Scope | Repr::Tag => {
            return Err(RuntimeError::new("this value has no text of its own"))
        }
    }
    Ok(())
}

/// The text of the value location of `layout` holding `words`, appended to
/// `out`.
///
/// A struct is its fields in place and an enum is a discriminant and a
/// payload region, so rendering one is reading runs of words rather than
/// following an address per field. This is the same walk
/// [`crate::vm::boundary`] makes, and it is written twice for the reason the
/// module docs give.
fn render_value(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
    if depth >= MAX_DEPTH {
        return Err(too_deep());
    }
    let deeper = depth + 1;
    let program = machine.program();
    let described = program.layout(layout);
    match &described.shape {
        Shape::Word(repr) => return render(machine, *repr, at(words, 0)?, depth, out),
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
            render_value(
                machine,
                field.layout,
                run(program, words, field)?,
                deeper,
                out,
            )?;
        }
        // An opaque type renders as its name and nothing else. Its fields are
        // the declaring module's own business, and a rendering is read by
        // whoever the string reaches, so showing them here would publish
        // through `println` what the checker refuses to let a caller name.
        // That is ADR 0014's whole point, and it is why the layout carries
        // the flag rather than this deriving it.
        //
        // `declared` first, `short` second: the layout's name carries the
        // instantiation's type arguments (`m.Cell<m.Point>`), and `short`
        // cuts at the *last* `.` — which, for a qualified argument, is
        // inside the brackets. Stripping the arguments first leaves only
        // the module-qualified declared name for `short` to cut at.
        Shape::Struct { opaque: true, .. } => out.push_str(short(declared(&described.name))),
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
            // public `Display` shows. The layout carries the qualified
            // *instantiation* — type arguments included — because a layout
            // is an identity, so `declared` strips those before `short`
            // strips the module: `short` alone would cut at a qualified
            // argument's own `.` instead (#407).
            write!(out, "{}(", short(declared(&described.name)))
                .expect("a string never fails to be written to");
            for (nth, field) in fields.iter().enumerate() {
                if nth > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}: ", field.name).expect("a string never fails to be written to");
                render_value(
                    machine,
                    field.layout,
                    run(program, words, field)?,
                    deeper,
                    out,
                )?;
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
                    render_value(machine, part.layout, held, deeper, out)?;
                }
                out.push(')');
            }
        }
        Shape::Free => return Err(reclaimed()),
        // Everything left lives in the heap, so the location is one address.
        _ => return render_object(machine, at(words, 0)?, depth, out),
    }
    Ok(())
}

/// The text of the object at `addr`, appended to `out`.
fn render_object(
    machine: &Machine,
    addr: u64,
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
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
    match &layout.shape {
        Shape::Str => out.push_str(&string_of(machine, addr)?),
        // Not a Cove value, so nothing renders it deliberately — reached only
        // from a debugger inspecting a run under construction.
        Shape::Bytes => out.push_str("<byte run>"),
        // Nor this, for the same reason. A buffer's bytes may not be valid
        // UTF-8, so the owner shows as the handle it is.
        Shape::ByteBuffer => out.push_str("<byte buffer>"),
        // A value whose *object* this is: a layout the lowering broke a
        // recursion at holds the value's own inline words as its payload, and
        // `Layout::payload_words` answers that same width.
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            let words = machine.payload_run(addr, 0, layout.width());
            return render_value(machine, id, &words, depth, out);
        }
        // A cell shows as the handle it is rather than as what it currently
        // holds, which is `Display for Value`'s answer for the same value:
        // its contents are reachable only under a `lock`, and rendering one
        // would be reading it without taking it.
        Shape::Shared { .. } => out.push_str("<shared>"),
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
                joined(machine, store, *elem, len, ", ", deeper, out)?;
            }
            out.push(']');
        }
        // An `Array` and a vector's store render alike, which is why one
        // shape covers both — and the stride is the element's width, so an
        // `Array<Point>` renders two words at a time.
        Shape::Elements { elem, .. } => {
            out.push('[');
            joined(
                machine,
                addr,
                *elem,
                machine.object_len(addr),
                ", ",
                deeper,
                out,
            )?;
            out.push(']');
        }
        // A set and a map both render inside braces, which is how the
        // language writes them and why they are ordered families rather than
        // hashed ones: the order is part of what a program sees.
        Shape::Members { elem } => {
            out.push('{');
            joined(
                machine,
                addr,
                *elem,
                machine.object_len(addr),
                ", ",
                deeper,
                out,
            )?;
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
                render_value(machine, *key, &one, deeper, out)?;
                out.push_str(": ");
                render_value(machine, *value, &other, deeper, out)?;
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
            return render_value(machine, held, &words, deeper, out);
        }
        Shape::Closure { .. } => out.push_str("<fn>"),
        Shape::Free => return Err(reclaimed()),
    }
    Ok(())
}

/// `len` elements of `elem` from the payload of `addr`, rendered and joined
/// into `out`.
fn joined(
    machine: &Machine,
    addr: u64,
    elem: LayoutId,
    len: u32,
    between: &str,
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
    let stride = machine.program().layout(elem).width();
    for nth in 0..len {
        if nth > 0 {
            out.push_str(between);
        }
        let words = machine.payload_run(addr, nth * stride, stride);
        render_value(machine, elem, &words, depth, out)?;
    }
    Ok(())
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
    program: &cove_ir::Program,
    words: &'w [u64],
    field: &cove_ir::Field,
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

/// Renders a `Float` so that it is never mistaken for an `Int`, appended to
/// `out`.
///
/// The language performs no implicit numeric conversions, so a float with no
/// fractional part still shows its point.
fn float(out: &mut String, x: f64) {
    if x.is_nan() {
        out.push_str("NaN");
        return;
    }
    if x.is_infinite() {
        out.push_str(if x.is_sign_negative() { "-inf" } else { "inf" });
        return;
    }
    if x.fract() == 0.0 {
        write!(out, "{x:.1}").expect("a string never fails to be written to");
    } else {
        write!(out, "{x}").expect("a string never fails to be written to");
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

/// Renders a `Duration` in the largest unit that divides it exactly,
/// appended to `out`.
fn duration(out: &mut String, ns: i64) {
    if ns == 0 {
        out.push_str("0ns");
        return;
    }
    for (factor, suffix) in DURATION_UNITS {
        if ns % factor == 0 {
            write!(out, "{}{suffix}", ns / factor).expect("a string never fails to be written to");
            return;
        }
    }
    unreachable!("every duration is divisible by one nanosecond")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::exec::tests::{budget, Build};
    use crate::vm::intrinsics::operand::Operand;
    use cove_ir::{Inst, IntrinsicSite, LayoutId, Program, Repr, Shape, SiteId};

    /// The program every builtin test is run against.
    ///
    /// One fixture with every family a builtin reaches for, because a builtin
    /// that answers an `Option<Int>` has to find one in the layout table and a
    /// test that declared only the families it thought it needed would be
    /// testing its own fixture. `pub(super)` so that each module's tests build
    /// their objects into the same world; a hand-written program is the only
    /// kind any of them uses, for the reason
    /// [`crate::vm::exec::tests::Build`] gives.
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
        // A `Result` whose `Ok` carries a `String` and whose `Err` is two
        // words, declared *before* the `Result<String, Error>` below and
        // indistinguishable from it by name and by what `Ok` holds. It is
        // here so that a builtin answering the narrow one has a wider wrong
        // answer to find — see
        // `a_builtin_answers_the_result_its_instruction_declares`.
        build.enumeration("Result", &[("Ok", vec![string]), ("Err", vec![point])]);
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
    /// loop's.
    ///
    /// `result` is the free layout here, which is what a caller passes when
    /// it does not care: it names no enum, so `make` falls back to searching
    /// the layout table as it always did. [`answering`] is the form for a
    /// test that does care.
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
    /// The same, declaring the layout the answer is supposed to have.
    ///
    /// What `Inst::IntrinsicCall` carries, and the only thing that tells two
    /// instantiations of one family apart.
    pub(super) fn answering(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        result: LayoutId,
        operands: &[(LayoutId, &[u64])],
    ) -> Result<Vec<u64>, RuntimeError> {
        let intrinsic = Intrinsic::from_names(receiver, operation)
            .unwrap_or_else(|| panic!("`{receiver}.{operation}` has no `Intrinsic`"));
        in_frame(machine, operands, result, |machine, frame, dest| {
            call(machine, intrinsic, frame, dest)
        })
    }

    /// What `body` writes into a destination of `result`, given a frame
    /// holding `operands` in order.
    ///
    /// The frame is a test's own, pushed onto the machine's stack for the
    /// call and popped after it: operand 0 at slot 0, each next one where the
    /// one before it ends, and the destination after the last. A free
    /// `result` is a one-word answer whose layout the test does not care
    /// about.
    ///
    /// Two guard words follow the destination and are checked afterwards,
    /// because a builtin that wrote past its declared answer is the bug
    /// `make`'s `a_builtin_answers_the_result_its_instruction_declares` pins,
    /// and a destination in a frame cannot say how much of it was written.
    pub(super) fn in_frame(
        machine: &mut Machine,
        operands: &[(LayoutId, &[u64])],
        result: LayoutId,
        body: impl FnOnce(&mut Machine, Frame<'_>, Dest) -> Result<(), RuntimeError>,
    ) -> Result<Vec<u64>, RuntimeError> {
        const GUARD: [u64; 2] = [0x5afe_5afe_5afe_5afe, 0xdead_beef_dead_beef];
        let mut args = Vec::with_capacity(operands.len());
        let mut words = Vec::new();
        for (layout, held) in operands {
            args.push(cove_ir::Arg {
                slot: words.len() as u32,
                layout: *layout,
            });
            words.extend_from_slice(held);
        }
        let dst = words.len() as u32;
        let width = if result == LayoutId::FREE {
            1
        } else {
            machine.words_of(result)
        };
        words.resize(words.len() + width as usize, 0);
        words.extend_from_slice(&GUARD);

        let base = machine.push_test_frame(&words);
        machine.begin_intrinsic();
        let answered = body(
            machine,
            Frame::new(base, &args),
            Dest::new(base, dst, result),
        );
        let after = machine.pop_test_frame(base, words.len() as u32);
        assert_eq!(
            &after[(dst + width) as usize..],
            &GUARD,
            "a builtin wrote past the answer its instruction declares"
        );
        answered.map(|()| after[dst as usize..(dst + width) as usize].to_vec())
    }

    pub(super) fn values(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(LayoutId, &[u64])],
    ) -> Result<Vec<u64>, RuntimeError> {
        let result = declared(machine.program(), receiver, operation, operands);
        answering(machine, receiver, operation, result, operands)
    }

    /// The layout the lowering would have put in `Inst::IntrinsicCall`.
    ///
    /// A fixture standing in for the lowering, and it is here rather than in
    /// `make` on purpose: the machine is *given* the layout of the answer,
    /// and a test that builds its operands by hand has to decide it the same
    /// way. What this must not be is a search inside the builtin — that is
    /// the bug `a_builtin_answers_the_result_its_instruction_declares` pins,
    /// and the reason both this and production hand the layout down as data.
    ///
    /// Every operation that answers an `Option` or a `Result` is named here.
    /// Anything else answers a value whose layout the builtin already knows,
    /// so the free layout is right for it and is never read.
    fn declared(
        program: &Program,
        receiver: &str,
        operation: &str,
        operands: &[(LayoutId, &[u64])],
    ) -> LayoutId {
        let ints = || word_layout(program, Repr::Int);
        let held = |at: usize| operands.get(at).map(|(layout, _)| *layout);
        // The payload of the sequence or map the call is on.
        let inside = || {
            let layout = program.layout(held(0)?);
            match layout.shape {
                Shape::Elements { elem, .. } | Shape::Vector { elem } => Some(elem),
                Shape::Entries { value, .. } => Some(value),
                _ => None,
            }
        };
        let payload = match (receiver, operation) {
            ("Array" | "Vector", "get" | "set" | "pop" | "remove") => inside(),
            ("String", "indexOf") => ints(),
            ("Int", "parse" | "parseRadix") | ("Float", "toInt") => ints(),
            ("Float", "parse") => word_layout(program, Repr::Float),
            ("String", "fromCodePoint") => Some(program.str_layout),
            _ => None,
        };
        let (family, carrier) = match (receiver, operation) {
            ("Int", "parse" | "parseRadix")
            | ("Float", "parse" | "toInt")
            | ("String", "fromCodePoint") => ("Result", "Ok"),
            _ => ("Option", "Some"),
        };
        payload
            .and_then(|payload| carrying(program, family, carrier, payload))
            .unwrap_or(LayoutId::FREE)
    }

    /// The enum called `name` whose case `carrier` holds one `payload`.
    fn carrying(
        program: &Program,
        name: &str,
        carrier: &str,
        payload: LayoutId,
    ) -> Option<LayoutId> {
        program
            .layouts
            .iter()
            .enumerate()
            .find(|(_, layout)| {
                &*layout.name == name
                    && matches!(&layout.shape, Shape::Enum { cases, .. } if cases.iter().any(|case| {
                        &*case.name == carrier
                            && case.parts.len() == 1
                            && case.parts[0].layout == payload
                    }))
            })
            .map(|(at, _)| LayoutId(at as u32))
    }

    /// The one-word layout of `repr`, where the program declares one.
    fn word_layout(program: &Program, repr: Repr) -> Option<LayoutId> {
        program
            .layouts
            .iter()
            .position(|layout| layout.shape == Shape::Word(repr))
            .map(|at| LayoutId(at as u32))
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

    /// The text a value the program can build appends to an interpolation's
    /// buffer, run through the dispatch loop rather than called directly, so
    /// what is under test is the instruction as well as the operation.
    ///
    /// The value is in slot 0 by construction and one word wide; the buffer,
    /// the `()` the rendering answers and the finished text take the slots
    /// after it — the assembly the lowering emits for `"{value}"`.
    fn text_of(build_value: impl FnOnce(&mut Build) -> (Vec<Repr>, Vec<Inst>)) -> String {
        let mut build = Build::default();
        let str_layout = build.string_layout();
        let (reprs, code) = build_value(&mut build);
        // An operand carries the layout of the location it names, and every
        // value this fixture builds is one word of it.
        let held = match reprs[0] {
            Repr::Ref => str_layout,
            repr => build.scalar(repr),
        };
        rendered(build, held, reprs, code, &[Intrinsic::ValueRenderInto])
    }

    /// Runs `code`, then renders the value in slot 0 of `held` into a fresh
    /// buffer through each of `renderings` in turn, and answers the finished
    /// text.
    fn rendered(
        mut build: Build,
        held: LayoutId,
        mut reprs: Vec<Repr>,
        mut code: Vec<Inst>,
        renderings: &[Intrinsic],
    ) -> String {
        let str_layout = build.program.str_layout;
        build.bytes_layout();
        let buffer_layout = build.buffer_layout();
        let unit = build.scalar(Repr::Unit);
        let base = reprs.len() as u32;
        let (capacity, buffer, answer, text) = (base, base + 1, base + 2, base + 3);
        reprs.extend([Repr::Int, Repr::Ref, Repr::Unit, Repr::Ref]);
        let operands = build.args(&[(0, held), (buffer, buffer_layout)]);
        code.push(Inst::Int {
            dst: capacity,
            value: 16,
        });
        code.push(Inst::GrowableAlloc {
            dst: buffer,
            capacity,
            storage: cove_ir::Storage::PackedBytes,
        });
        for rendering in renderings {
            let site = site(
                &mut build.program,
                rendering.receiver(),
                rendering.operation(),
                unit,
            );
            code.push(Inst::IntrinsicCall {
                dst: answer,
                site,
                args: operands,
            });
        }
        code.push(Inst::RunFinish {
            dst: text,
            owner: buffer,
            target: str_layout,
            validation: cove_ir::Validation::Utf8,
            storage: cove_ir::Storage::PackedBytes,
        });
        code.push(Inst::Return { src: text });
        let f = build.function("f", &[], &reprs, str_layout, code);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        String::from_utf8(machine.string_bytes(word[0])).unwrap()
    }

    fn site(program: &mut Program, receiver: &str, operation: &str, result: LayoutId) -> SiteId {
        let intrinsic = Intrinsic::from_names(receiver, operation)
            .unwrap_or_else(|| panic!("`{receiver}.{operation}` has no `Intrinsic`"));
        program
            .intrinsic_sites
            .push(IntrinsicSite { intrinsic, result });
        SiteId(program.intrinsic_sites.len() as u32 - 1)
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
        let str_layout = build.string_layout();
        let code = vec![Inst::Str {
            dst: 0,
            text: cove_ir::StrId(0),
        }];
        assert_eq!(
            rendered(
                build,
                str_layout,
                vec![Repr::Ref],
                code,
                &[Intrinsic::ValueRenderInto]
            ),
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

        let mut out = String::new();
        render_value(&machine, point, &[1, (-2i64) as u64], 0, &mut out).unwrap();
        assert_eq!(out, "Point(x: 1, y: -2)");

        // `[disc, x, y]`: the `Point` is inline in the payload region.
        let mut out = String::new();
        render_value(&machine, option, &[1, 1, (-2i64) as u64], 0, &mut out).unwrap();
        assert_eq!(out, "Some(Point(x: 1, y: -2))");

        let mut out = String::new();
        render_value(&machine, option, &[0, 0, 0], 0, &mut out).unwrap();
        assert_eq!(out, "None");

        // An `Array<Point>` is a run of two-word elements, walked at that
        // stride.
        let items = machine
            .new_object(elements(&program, point, false), 2)
            .unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4]);
        let mut out = String::new();
        render(&machine, Repr::Ref, items, 0, &mut out).unwrap();
        assert_eq!(out, "[Point(x: 1, y: 2), Point(x: 3, y: 4)]");
        let _ = int;
    }

    /// An `Int` piece is formatted on the stack, and says what the general
    /// walk says about the same word — at both ends of the range and at zero.
    #[test]
    fn an_int_renders_into_the_buffer_as_the_walk_renders_it() {
        for value in [0, 7, -7, 1_000_000, i64::MAX, i64::MIN] {
            let mut build = Build::default();
            build.string_layout();
            let int = build.scalar(Repr::Int);
            let code = vec![Inst::Int { dst: 0, value }];
            let text = rendered(
                build,
                int,
                vec![Repr::Int],
                code,
                &[Intrinsic::IntRenderInto, Intrinsic::ValueRenderInto],
            );
            assert_eq!(text, format!("{value}{value}"));
        }
    }

    /// Appends go on where the last one ended, and a buffer grows past the
    /// capacity it was allocated with rather than refusing: twenty renderings
    /// of `-1234567` are 160 bytes in a sixteen-byte store.
    #[test]
    fn renderings_append_in_order_and_grow_the_buffer() {
        let mut build = Build::default();
        build.string_layout();
        let int = build.scalar(Repr::Int);
        let code = vec![Inst::Int {
            dst: 0,
            value: -1_234_567,
        }];
        let text = rendered(
            build,
            int,
            vec![Repr::Int],
            code,
            &[Intrinsic::IntRenderInto; 20],
        );
        assert_eq!(text, "-1234567".repeat(20));
    }

    /// An answer may be written over one of its own operands: `x = x.trim()`
    /// lowers to a call whose destination is `x`, and since the operands are
    /// read where they are rather than copied out first, every arm reads all
    /// of them before it writes (#378, Q5.2).
    #[test]
    fn an_answer_may_be_written_over_its_own_operand() {
        let mut build = Build::default().strings(&["  ha  "]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let trimmed = build.args(&[(0, str_layout)]);
        let trim = site(&mut build.program, "String", "trim", str_layout);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref],
            str_layout,
            vec![
                Inst::Str {
                    dst: 0,
                    text: cove_ir::StrId(0),
                },
                Inst::IntrinsicCall {
                    dst: 0,
                    site: trim,
                    args: trimmed,
                },
                Inst::Return { src: 0 },
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

    /// The contract that makes that sound is held, not hoped for: an arm that
    /// read an operand after writing its answer would read what it wrote, and
    /// under `debug_assertions` — which the `checked` profile keeps on — the
    /// read panics instead.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "read an operand after writing its answer")]
    fn reading_an_operand_after_writing_the_answer_panics() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let _ = in_frame(&mut machine, &[(int, &[1])], int, |machine, frame, dest| {
            dest.word(machine, 2);
            frame.word(machine, 0);
            Ok(())
        });
    }

    /// A rendering allocates nothing of its own when the text fits.
    ///
    /// `Inst::Alloc` is not reached from here: the text is built in Rust and
    /// copied into the buffer's store, so there is no half-built object for a
    /// collection to land on, and a store with room is not replaced.
    #[test]
    fn rendering_allocates_nothing_when_the_text_fits() {
        let program = world_with_buffer();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let unit = scalar(&program, Repr::Unit);
        let items = machine
            .new_object(elements(&program, int, false), 3)
            .unwrap();
        for at in 0..3u32 {
            machine.set_payload(items, at, at as u64 + 1);
        }
        let buffer = machine.alloc_buffer(64).unwrap();
        let before = machine.allocated_words();
        let answer = in_frame(
            &mut machine,
            &[
                (elements(&program, int, false), &[items]),
                (program.buffer_layout, &[buffer]),
            ],
            unit,
            render_into,
        )
        .unwrap();
        assert_eq!(answer, vec![0]);
        assert_eq!(machine.allocated_words() - before, 0);
        let text = machine.finish_buffer(buffer, program.str_layout, cove_ir::Validation::Utf8);
        assert_eq!(read(&machine, text.unwrap()), "[1, 2, 3]");
    }

    /// [`world`], with the byte buffer's two program-wide layouts declared.
    fn world_with_buffer() -> Program {
        let mut program = world();
        program.bytes_layout = LayoutId(program.layouts.len() as u32);
        program
            .layouts
            .push(cove_ir::Layout::object("Bytes", Shape::Bytes));
        program.buffer_layout = LayoutId(program.layouts.len() as u32);
        program
            .layouts
            .push(cove_ir::Layout::object("ByteBuffer", Shape::ByteBuffer));
        program
    }
}
