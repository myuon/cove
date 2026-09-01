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
//! answers one word. Nothing here materialises a public `Value`, and this
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

use cove_lir::{Builtin, Repr, Shape};
use cove_schema::builtins::{ERROR, MESSAGE_FIELD};

use crate::error::RuntimeError;
use crate::lvm::exec::Machine;

/// How deep a rendering may nest.
///
/// For the reason [`crate::lvm::boundary`]'s limit exists: an object graph
/// can hold itself and a renderer that met one would recurse until the native
/// stack ran out.
const MAX_DEPTH: usize = 128;

/// Runs `builtin` over `operands`, answering the word it produces.
///
/// Each operand is the word of a slot and the `Repr` that slot declares.
/// A word is untagged, so the pair is the whole of what a builtin has to work
/// from, and reading the `Repr` out of the frame is the caller's job because
/// only the caller has a frame.
pub(crate) fn call(
    machine: &mut Machine,
    builtin: &Builtin,
    operands: &[(Repr, u64)],
) -> Result<u64, RuntimeError> {
    // One match over the pair the IR names, so that teaching the machine an
    // operation is adding an arm and nothing else.
    match (&*builtin.receiver, &*builtin.operation) {
        ("String", "text") => {
            let [(repr, word)] = operands else {
                return Err(arity("String.text", 1, operands.len()));
            };
            let text = render(machine, *repr, *word, 0)?;
            machine.new_string(&text)
        }
        ("String", "concat") => {
            let mut text = String::new();
            for (repr, word) in operands {
                if *repr != Repr::Ref || !is_string(machine, *word) {
                    return Err(RuntimeError::new(
                        "`String.concat` joins strings, and this operand is not one",
                    ));
                }
                text.push_str(&string_of(machine, *word)?);
            }
            machine.new_string(&text)
        }
        ("String", "interpolate") => {
            let mut text = String::new();
            for (repr, word) in operands {
                text.push_str(&render(machine, *repr, *word, 0)?);
            }
            machine.new_string(&text)
        }
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
/// What `"{x}"` puts in the string, for every `x` the language admits there.
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

/// The text of the object at `addr`.
fn render_object(machine: &Machine, addr: u64, depth: usize) -> Result<String, RuntimeError> {
    if addr == 0 {
        return Err(RuntimeError::new(
            "this value was read before it was given one",
        ));
    }
    if depth >= MAX_DEPTH {
        return Err(RuntimeError::new("this value nests too deeply to render"));
    }
    let deeper = depth + 1;
    let layout = machine.program().layout(machine.object_layout(addr));
    let mut out = String::new();
    match &layout.shape {
        Shape::Str => out.push_str(&string_of(machine, addr)?),
        // A builtin `Error` renders as the message it carries, not as the
        // struct it happens to be. The oracle special-cases it in
        // `Display for Value` for the reason this one does: a program that
        // prints an error is printing what went wrong, and `Error(message: x)`
        // says the same thing twice. Recognising it by the layout's name is
        // sound because the name is the checker's, and `Error` is a builtin
        // type a module cannot redeclare.
        Shape::Struct { fields, .. }
            if &*layout.name == ERROR.name
                && fields.first().map(|field| &*field.name) == Some(MESSAGE_FIELD.name) =>
        {
            let word = machine.payload(addr, 0);
            out.push_str(&render(machine, fields[0].repr, word, deeper)?);
        }
        // An opaque type renders as its name and nothing else. Its fields are
        // the declaring module's own business, and a rendering is read by
        // whoever the string reaches, so showing them here would publish
        // through `println` what the checker refuses to let a caller name.
        // That is ADR 0014's whole point, and it is why the layout carries
        // the flag rather than this deriving it.
        Shape::Struct { opaque: true, .. } => out.push_str(&layout.name),
        Shape::Struct { fields, .. } => {
            write!(out, "{}(", layout.name).expect("a string never fails to be written to");
            for (at, field) in fields.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                let word = machine.payload(addr, at as u32);
                write!(out, "{}: ", field.name).expect("a string never fails to be written to");
                out.push_str(&render(machine, field.repr, word, deeper)?);
            }
            out.push(')');
        }
        Shape::Enum { cases } => {
            // The case is a fact about this object, so the object is asked.
            let index = machine.payload(addr, 0);
            let case = cases.get(index as usize).ok_or_else(|| {
                RuntimeError::new(format!(
                    "this `{}` is in case {index}, which it does not have",
                    layout.name
                ))
            })?;
            out.push_str(&case.name);
            if !case.payload.is_empty() {
                out.push('(');
                for (at, repr) in case.payload.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    let word = machine.payload(addr, 1 + at as u32);
                    out.push_str(&render(machine, *repr, word, deeper)?);
                }
                out.push(')');
            }
        }
        // An `Array` and a `Vector` render alike, which is why one shape
        // covers both.
        Shape::Elements { elem, .. } => {
            out.push('[');
            for at in 0..machine.object_len(addr) {
                if at > 0 {
                    out.push_str(", ");
                }
                let word = machine.payload(addr, at);
                out.push_str(&render(machine, *elem, word, deeper)?);
            }
            out.push(']');
        }
        // Erasure is looked through: a `dyn Display` shows the value it
        // holds, because the wrapper is a representation and not something
        // the program put there.
        Shape::Boxed => {
            let tag = machine.payload(addr, 0);
            let repr = Repr::from_tag(tag)
                .ok_or_else(|| RuntimeError::new("this boxed value carries no known type"))?;
            out.push_str(&render(machine, repr, machine.payload(addr, 1), deeper)?);
        }
        Shape::Closure { .. } => out.push_str("<fn>"),
        Shape::Free => {
            return Err(RuntimeError::new(
                "this value was read after it was reclaimed",
            ))
        }
    }
    Ok(out)
}

/// Whether the object at `addr` is a string.
fn is_string(machine: &Machine, addr: u64) -> bool {
    addr != 0
        && matches!(
            machine.program().layout(machine.object_layout(addr)).shape,
            Shape::Str
        )
}

/// The text of the string object at `addr`.
fn string_of(machine: &Machine, addr: u64) -> Result<String, RuntimeError> {
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

fn arity(shown: &str, wanted: usize, given: usize) -> RuntimeError {
    RuntimeError::new(format!(
        "`{shown}` takes {wanted} operand(s), but {given} were given"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::exec::tests::{budget, Build};
    use cove_lir::{BuiltinId, Case, Field, Inst, LayoutId, Program, Repr, Shape};
    use std::sync::Arc;

    fn field(name: &str, repr: Repr) -> Field {
        Field {
            name: Arc::from(name),
            repr,
        }
    }

    /// A one-argument builtin over a value the program can build, run through
    /// the dispatch loop rather than called directly, so what is under test is
    /// the instruction as well as the operation.
    fn text_of(build_value: impl FnOnce(&mut Build) -> (Vec<Repr>, Vec<Inst>)) -> String {
        let mut build = Build::default();
        let (mut reprs, mut code) = build_value(&mut build);
        // The value is in slot 0 by construction; slot 1 takes the text.
        let operand = build.args(&[0]);
        let dst = reprs.len() as u32;
        reprs.push(Repr::Ref);
        let builtin = builtin(&mut build.program, "String", "text");
        code.push(Inst::CallBuiltin {
            dst,
            builtin,
            args: operand,
        });
        code.push(Inst::Return { src: dst });
        let returns = Repr::Ref;
        let f = build.function("f", 0, &reprs, returns, code);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = machine.run(f, &[], &budget()).unwrap();
        String::from_utf8(machine.string_bytes(word)).unwrap()
    }

    fn builtin(program: &mut Program, receiver: &str, operation: &str) -> BuiltinId {
        program.builtins.push(Builtin {
            receiver: Arc::from(receiver),
            operation: Arc::from(operation),
            result: Repr::Ref,
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
        let operand = build.args(&[0]);
        let builtin = builtin(&mut build.program, "String", "text");
        let f = build.function(
            "f",
            0,
            &[Repr::Ref, Repr::Ref],
            Repr::Ref,
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
        assert_eq!(String::from_utf8(machine.string_bytes(word)).unwrap(), "ha");
    }

    #[test]
    fn a_compound_value_renders_the_way_the_oracle_shows_it() {
        let mut build = Build::default();
        let point = build.layout(
            "Point",
            Shape::Struct {
                fields: vec![field("x", Repr::Int), field("y", Repr::Int)],
                opaque: false,
            },
        );
        let option = build.layout(
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
        let array = build.layout(
            "Array",
            Shape::Elements {
                elem: Repr::Ref,
                growable: false,
            },
        );
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);

        let addr = machine.new_object(point, 0).unwrap();
        machine.set_payload(addr, 0, 1);
        machine.set_payload(addr, 1, (-2i64) as u64);
        assert_eq!(
            render(&machine, Repr::Ref, addr, 0).unwrap(),
            "Point(x: 1, y: -2)"
        );

        let some = machine.new_object(option, 0).unwrap();
        machine.set_payload(some, 0, 1);
        machine.set_payload(some, 1, addr);
        assert_eq!(
            render(&machine, Repr::Ref, some, 0).unwrap(),
            "Some(Point(x: 1, y: -2))"
        );

        let none = machine.new_object(option, 0).unwrap();
        assert_eq!(render(&machine, Repr::Ref, none, 0).unwrap(), "None");

        let items = machine.new_object(array, 2).unwrap();
        machine.set_payload(items, 0, some);
        machine.set_payload(items, 1, none);
        assert_eq!(
            render(&machine, Repr::Ref, items, 0).unwrap(),
            "[Some(Point(x: 1, y: -2)), None]"
        );
    }

    #[test]
    fn interpolation_joins_the_text_of_every_operand() {
        let mut build = Build::default().strings(&["n is ", "!"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let parts = build.args(&[0, 1, 2]);
        let builtin = builtin(&mut build.program, "String", "interpolate");
        let f = build.function(
            "f",
            0,
            &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Ref],
            Repr::Ref,
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
            String::from_utf8(machine.string_bytes(word)).unwrap(),
            "n is 7!"
        );
    }

    #[test]
    fn concat_joins_strings_and_refuses_anything_else() {
        let mut build = Build::default().strings(&["ab", "cd"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let both = build.args(&[0, 1]);
        let joined = builtin(&mut build.program, "String", "concat");
        let f = build.function(
            "f",
            0,
            &[Repr::Ref, Repr::Ref, Repr::Ref],
            Repr::Ref,
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
            String::from_utf8(machine.string_bytes(word)).unwrap(),
            "abcd"
        );

        // The one thing `concat` is stricter about than `interpolate`: it
        // joins strings, and there are no implicit conversions.
        let mut build = Build::default();
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let both = build.args(&[0]);
        let joined = builtin(&mut build.program, "String", "concat");
        let f = build.function(
            "f",
            0,
            &[Repr::Int, Repr::Ref],
            Repr::Ref,
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
        let unknown = builtin(&mut build.program, "String", "reverse");
        let f = build.function(
            "f",
            0,
            &[Repr::Ref],
            Repr::Ref,
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
        let mut build = Build::default();
        let array = build.layout(
            "Array",
            Shape::Elements {
                elem: Repr::Int,
                growable: false,
            },
        );
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = machine.new_object(array, 3).unwrap();
        for at in 0..3u32 {
            machine.set_payload(items, at, at as u64 + 1);
        }
        let before = machine.allocated_words();
        let word = call(
            &mut machine,
            &Builtin {
                receiver: Arc::from("String"),
                operation: Arc::from("text"),
                result: Repr::Ref,
            },
            &[(Repr::Ref, items)],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word)).unwrap(),
            "[1, 2, 3]"
        );
        // One header and one payload word: "[1, 2, 3]" is nine bytes.
        assert_eq!(machine.allocated_words() - before, 3);
        assert_ne!(machine.object_layout(word), LayoutId::FREE);
    }
}
