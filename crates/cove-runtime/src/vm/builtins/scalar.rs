//! `Int`, `Float` and `Duration`.
//!
//! A scalar is one word, and a `Result` is a run of words rather than an
//! object, so the only thing any of these allocates is text: the `String` a
//! `format` builds, and the message an `Err` explains itself with. What each
//! one *means* is the oracle's, including the three places the answer is not
//! the obvious one:
//!
//! - **`Int.abs()` at `Int.MIN` stops the run.** Integer overflow is a broken
//!   invariant in this language and not a wrapped result, so `abs` raises
//!   what `-x` raises, in the same words.
//! - **`Float.toInt()` answers a `Result`**, because three floats have no
//!   truncation that fits: `NaN`, an infinity, and a magnitude at or past
//!   2^63. Each is named separately.
//! - **`d.millis()` truncates toward zero**, which is what `Int` division
//!   already does — so `1500ms.seconds()` is 1 and `(-1500ms).seconds()` is
//!   -1, and `d.seconds()` is `d.nanos() / 1_000_000_000` whichever way a
//!   program asks.
//!
//! # `Duration.<unit>` is two operations under one name
//!
//! `Duration.seconds(1)` builds a duration and `d.seconds()` reads one back
//! out, and the language spells them the same. [`cove_ir::Builtin`] names an
//! operation by its receiver and its name, so the two arrive here
//! indistinguishable by name — and are told apart by the **`Repr` of the
//! operand**, which is a static fact about the slot the lowering chose:
//! `Repr::Duration` is the receiver of a reader, and anything else is the
//! count of a builder. Nothing is inferred from a word.

use cove_ir::{LayoutId, Program, Repr, Shape};

use crate::error::RuntimeError;
use crate::vm::builtins::operand::Operand;
use crate::vm::builtins::{make, operand};
use crate::vm::exec::Machine;

/// The `Int` a method was called on.
fn int_receiver(
    machine: &Machine,
    method: &str,
    receiver: Operand<'_>,
) -> Result<i64, RuntimeError> {
    match operand::as_word(machine, receiver) {
        Some((Repr::Int, word)) => Ok(word as i64),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// The `Float` a method was called on.
fn float_receiver(
    machine: &Machine,
    method: &str,
    receiver: Operand<'_>,
) -> Result<f64, RuntimeError> {
    match operand::as_word(machine, receiver) {
        Some((Repr::Float, word)) => Ok(f64::from_bits(word)),
        _ => Err(operand::no_method(machine, receiver, method)),
    }
}

/// The one-word layout of a scalar family.
///
/// An operation that answers a `Result<Int, Error>` has to name the `Int` its
/// `Ok` carries, and [`cove_ir::Builtin`] carries no layout for it — so the
/// family is found in the layout table, the way [`make`] finds the `Result`
/// around it. A miss is the same missing family [`make`] reports and says so
/// in the same words.
///
/// `pub(super)` because [`super::text`] needs the same one word: `indexOf`
/// answers an `Option<Int>`.
pub(super) fn word_layout(program: &Program, repr: Repr) -> Result<LayoutId, RuntimeError> {
    program
        .layouts
        .iter()
        .position(|layout| layout.shape == Shape::Word(repr))
        .map(|at| LayoutId(at as u32))
        .ok_or_else(|| operand::unknown_family(repr.name()))
}

// --- Int -------------------------------------------------------------------

/// `Int.toFloat() -> Float`.
pub(super) fn int_to_float(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("toFloat", operands, 0)?;
    Ok((int_receiver(machine, "toFloat", self_)? as f64).to_bits())
}

/// `Int.abs() -> Int`, which `Int.MIN` has none of.
pub(super) fn int_abs(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("abs", operands, 0)?;
    let n = int_receiver(machine, "abs", self_)?;
    n.checked_abs()
        .map(|value| value as u64)
        .ok_or_else(|| operand::overflowed("abs"))
}

/// `Int.min(other) -> Int`.
pub(super) fn int_min(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("Int.min", operands, 1)?;
    let n = int_receiver(machine, "min", self_)?;
    let other = operand::int(machine, "Int.min", "other", args[0])?;
    Ok(n.min(other) as u64)
}

/// `Int.max(other) -> Int`.
pub(super) fn int_max(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("Int.max", operands, 1)?;
    let n = int_receiver(machine, "max", self_)?;
    let other = operand::int(machine, "Int.max", "other", args[0])?;
    Ok(n.max(other) as u64)
}

/// `Int.parse(text) -> Result<Int, Error>`.
///
/// Rust's `str::parse::<i64>` reads a leading `+` or `-` and no digit
/// separators, which is why a `1_000` that a literal may be written with is
/// an `Err` here.
pub(super) fn int_parse(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    let args = operand::free("Int.parse", operands, 1)?;
    let text = operand::text(machine, "Int.parse", "text", args[0])?;
    let int = word_layout(machine.program(), Repr::Int)?;
    match text.parse::<i64>() {
        Ok(value) => make::ok(machine, int, &[value as u64]),
        Err(_) => make::failed(machine, int, &format!("`{text}` is not an Int")),
    }
}

/// `Int.parseRadix(text, radix) -> Result<Int, Error>`.
///
/// A `radix` outside `2..=36` names no notation, so it stops the run the way
/// an empty `String.split` separator does; text that is not a number in a
/// radix that does exist is the data's failure and answers `Err`.
pub(super) fn int_parse_radix(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    let args = operand::free("Int.parseRadix", operands, 2)?;
    let text = operand::text(machine, "Int.parseRadix", "text", args[0])?;
    let radix = operand::int(machine, "Int.parseRadix", "radix", args[1])?;
    let Some(base) = (2..=36).contains(&radix).then_some(radix as u32) else {
        return Err(operand::radix(radix));
    };
    let int = word_layout(machine.program(), Repr::Int)?;
    match i64::from_str_radix(&text, base) {
        Ok(value) => make::ok(machine, int, &[value as u64]),
        Err(_) => {
            let message = format!("`{text}` is not an Int in radix {base}");
            make::failed(machine, int, &message)
        }
    }
}

// --- Float -----------------------------------------------------------------

/// `Float.toInt() -> Result<Int, Error>`, truncating toward zero.
pub(super) fn float_to_int(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    let (self_, _) = operand::method("toInt", operands, 0)?;
    let x = float_receiver(machine, "toInt", self_)?;
    let int = word_layout(machine.program(), Repr::Int)?;
    if x.is_nan() {
        return make::failed(
            machine,
            int,
            "`Float.toInt` cannot convert `NaN`, which is not a number",
        );
    }
    if x.is_infinite() {
        let message = format!("`Float.toInt` cannot convert `{x}`, which has no truncation");
        return make::failed(machine, int, &message);
    }
    let truncated = x.trunc();
    if truncated < i64::MIN as f64 || truncated >= i64::MAX as f64 {
        let message = format!("`Float.toInt` cannot convert `{x}`, which is outside Int's range");
        return make::failed(machine, int, &message);
    }
    make::ok(machine, int, &[truncated as i64 as u64])
}

/// `Float.round() -> Float`.
pub(super) fn float_round(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("round", operands, 0)?;
    Ok(float_receiver(machine, "round", self_)?.round().to_bits())
}

/// `Float.abs() -> Float`.
pub(super) fn float_abs(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("abs", operands, 0)?;
    Ok(float_receiver(machine, "abs", self_)?.abs().to_bits())
}

/// `Float.min(other) -> Float`.
pub(super) fn float_min(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("Float.min", operands, 1)?;
    let x = float_receiver(machine, "min", self_)?;
    let other = operand::float(machine, "Float.min", "other", args[0])?;
    Ok(x.min(other).to_bits())
}

/// `Float.max(other) -> Float`.
pub(super) fn float_max(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("Float.max", operands, 1)?;
    let x = float_receiver(machine, "max", self_)?;
    let other = operand::float(machine, "Float.max", "other", args[0])?;
    Ok(x.max(other).to_bits())
}

/// `Float.format(digits) -> String`, fixed-point.
pub(super) fn float_format(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("Float.format", operands, 1)?;
    let x = float_receiver(machine, "format", self_)?;
    let digits = operand::int(machine, "Float.format", "digits", args[0])?;
    if !(0..=17).contains(&digits) {
        return Err(operand::format_digits(digits));
    }
    let text = format!("{:.*}", digits as usize, x);
    machine.new_string(&text)
}

/// `Float.parse(text) -> Result<Float, Error>`.
///
/// Rust's `f64::from_str` accepts `inf`, `-inf` and `NaN`, which is why this
/// does too, and rejects the `_` separators a literal may be written with —
/// the same thing `Int.parse` does.
pub(super) fn float_parse(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<Vec<u64>, RuntimeError> {
    let args = operand::free("Float.parse", operands, 1)?;
    let text = operand::text(machine, "Float.parse", "text", args[0])?;
    let float = word_layout(machine.program(), Repr::Float)?;
    match text.parse::<f64>() {
        Ok(value) => make::ok(machine, float, &[value.to_bits()]),
        Err(_) => make::failed(machine, float, &format!("`{text}` is not a Float")),
    }
}

// --- Duration --------------------------------------------------------------

/// The nanoseconds in one of the six units a `Duration` is written in.
///
/// One table for both directions: `Duration.millis(n)` multiplies by what
/// `d.millis()` divides by, so a duration built in a unit and read back in it
/// is the same number. The names are the schema's and the factors are the
/// ones the lexer gives the matching literal suffix — `ns`, `us`, `ms`, `s`,
/// `m`, `h` — so `1s` and `Duration.seconds(1)` cannot come apart.
pub(super) fn unit(name: &str) -> Option<i64> {
    Some(match name {
        "nanos" => 1,
        "micros" => 1_000,
        "millis" => 1_000_000,
        "seconds" => 1_000_000_000,
        "minutes" => 60 * 1_000_000_000,
        "hours" => 60 * 60 * 1_000_000_000,
        _ => return None,
    })
}

/// `d.<unit>() -> Int` and `Duration.<unit>(count) -> Duration`, told apart
/// by the operand's `Repr`. See the module docs.
pub(super) fn duration(
    machine: &mut Machine,
    name: &str,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let factor = unit(name).expect("the dispatch matched one of the six units");
    let first = operands
        .first()
        .and_then(|operand| operand::as_word(machine, *operand));
    if let Some((Repr::Duration, nanos)) = first {
        operand::method(name, operands, 0)?;
        // Truncating toward zero, which is what `Int` division does. None of
        // the six can fail: every unit divides into a count that fits where
        // the nanoseconds already did.
        return Ok(((nanos as i64) / factor) as u64);
    }
    let shown = format!("Duration.{name}");
    let args = operand::free(&shown, operands, 1)?;
    let count = operand::int(machine, &shown, "count", args[0])?;
    // A negative count is a negative duration, because a `Duration` is signed
    // nanoseconds and `-1s` is already writable. A count whose nanoseconds do
    // not fit stops the run in the words `Duration` arithmetic already stops
    // it in.
    count
        .checked_mul(factor)
        .map(|nanos| nanos as u64)
        .ok_or_else(|| operand::overflowed("duration arithmetic"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::tests::{message_of, read, result_of, run, scalar, word, world};

    fn int_of(machine: &mut Machine, operation: &str, operands: &[(Repr, u64)]) -> i64 {
        word(machine, "Int", operation, operands).unwrap() as i64
    }

    fn float_of(machine: &mut Machine, operation: &str, operands: &[(Repr, u64)]) -> f64 {
        f64::from_bits(word(machine, "Float", operation, operands).unwrap())
    }

    #[test]
    fn an_int_converts_and_compares() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        assert_eq!(
            f64::from_bits(word(&mut machine, "Int", "toFloat", &[(Repr::Int, 3)]).unwrap()),
            3.0
        );
        assert_eq!(
            int_of(
                &mut machine,
                "min",
                &[(Repr::Int, 3), (Repr::Int, -1i64 as u64)]
            ),
            -1
        );
        assert_eq!(
            int_of(
                &mut machine,
                "max",
                &[(Repr::Int, 3), (Repr::Int, -1i64 as u64)]
            ),
            3
        );
        assert_eq!(int_of(&mut machine, "abs", &[(Repr::Int, -7i64 as u64)]), 7);
    }

    /// `Int.MIN` has no absolute value, and integer overflow is a broken
    /// invariant rather than a wrapped result — so `abs` raises what `-x`
    /// raises, in the same words.
    #[test]
    fn abs_of_the_smallest_int_overflows() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let error = run(&mut machine, "Int", "abs", &[(Repr::Int, i64::MIN as u64)]).unwrap_err();
        assert_eq!(error.message, "`Int` abs overflowed");
        assert_eq!(
            error.rule.as_deref(),
            Some("Integer overflow is a broken invariant, not a wrapped result.")
        );
    }

    /// Text that is not a number is the *data's* failure and answers `Err`; a
    /// radix that names no notation is the *call's* and stops the run.
    #[test]
    fn parsing_an_int_separates_bad_data_from_a_bad_call() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let parse = |machine: &mut Machine, text: &str| {
            let word = machine.new_string(text).unwrap();
            run(machine, "Int", "parse", &[(Repr::Ref, word)]).unwrap()
        };
        // A `Result` is a run of words — `[disc, Int]` — and not an object,
        // so what the answer is read out of is the words themselves.
        let words = parse(&mut machine, "-12");
        assert_eq!(
            result_of(&program, int, &words),
            ("Ok".to_string(), vec![-12i64 as u64])
        );
        // Rust's `parse` reads no digit separators, which a literal may be
        // written with.
        let words = parse(&mut machine, "1_000");
        assert_eq!(message_of(&machine, int, &words), "`1_000` is not an Int");

        let text = machine.new_string("ff").unwrap();
        let words = run(
            &mut machine,
            "Int",
            "parseRadix",
            &[(Repr::Ref, text), (Repr::Int, 16)],
        )
        .unwrap();
        assert_eq!(
            result_of(&program, int, &words),
            ("Ok".to_string(), vec![255])
        );
        let words = run(
            &mut machine,
            "Int",
            "parseRadix",
            &[(Repr::Ref, text), (Repr::Int, 10)],
        )
        .unwrap();
        assert_eq!(
            message_of(&machine, int, &words),
            "`ff` is not an Int in radix 10"
        );

        let error = run(
            &mut machine,
            "Int",
            "parseRadix",
            &[(Repr::Ref, text), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Int.parseRadix` cannot read a number in radix `1`"
        );
    }

    #[test]
    fn a_float_rounds_compares_and_formats() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let x = (-2.5f64).to_bits();
        assert_eq!(float_of(&mut machine, "round", &[(Repr::Float, x)]), -3.0);
        assert_eq!(float_of(&mut machine, "abs", &[(Repr::Float, x)]), 2.5);
        assert_eq!(
            float_of(
                &mut machine,
                "min",
                &[(Repr::Float, x), (Repr::Float, 1.0f64.to_bits())]
            ),
            -2.5
        );
        assert_eq!(
            float_of(
                &mut machine,
                "max",
                &[(Repr::Float, x), (Repr::Float, 1.0f64.to_bits())]
            ),
            1.0
        );

        let text = word(
            &mut machine,
            "Float",
            "format",
            &[(Repr::Float, 1.5f64.to_bits()), (Repr::Int, 3)],
        )
        .unwrap();
        assert_eq!(read(&machine, text), "1.500");
        let error = run(
            &mut machine,
            "Float",
            "format",
            &[(Repr::Float, x), (Repr::Int, 18)],
        )
        .unwrap_err();
        assert_eq!(error.message, "`Float.format` cannot use `18` digits");
    }

    /// Three floats have no truncation an `Int` can hold, and each is named
    /// separately rather than answered with one message about conversion.
    #[test]
    fn to_int_names_each_of_the_three_failures() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let to_int = |machine: &mut Machine, x: f64| {
            run(machine, "Float", "toInt", &[(Repr::Float, x.to_bits())]).unwrap()
        };
        let words = to_int(&mut machine, -2.9);
        assert_eq!(
            result_of(&program, int, &words),
            ("Ok".to_string(), vec![-2i64 as u64]),
            "truncated toward zero"
        );
        let words = to_int(&mut machine, f64::NAN);
        assert_eq!(
            message_of(&machine, int, &words),
            "`Float.toInt` cannot convert `NaN`, which is not a number"
        );
        let words = to_int(&mut machine, f64::INFINITY);
        assert_eq!(
            message_of(&machine, int, &words),
            "`Float.toInt` cannot convert `inf`, which has no truncation"
        );
        let words = to_int(&mut machine, 1e30);
        assert_eq!(
            message_of(&machine, int, &words),
            "`Float.toInt` cannot convert `1000000000000000000000000000000`, which is outside Int's range"
        );
    }

    #[test]
    fn parsing_a_float_answers_a_result() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let float = scalar(&program, Repr::Float);
        let text = machine.new_string("1.5").unwrap();
        let words = run(&mut machine, "Float", "parse", &[(Repr::Ref, text)]).unwrap();
        assert_eq!(
            result_of(&program, float, &words),
            ("Ok".to_string(), vec![1.5f64.to_bits()])
        );
        let text = machine.new_string("x").unwrap();
        let words = run(&mut machine, "Float", "parse", &[(Repr::Ref, text)]).unwrap();
        assert_eq!(message_of(&machine, float, &words), "`x` is not a Float");
    }

    /// The same name reads a duration and builds one, and the operand's
    /// `Repr` is what tells them apart. Read and built in the same unit, the
    /// count comes back unchanged.
    #[test]
    fn a_duration_is_read_in_a_unit_and_built_from_one() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);

        // The builder: an `Int` count.
        let built = word(&mut machine, "Duration", "millis", &[(Repr::Int, 1500)]).unwrap();
        assert_eq!(built as i64, 1_500_000_000);
        // The reader: a `Duration` receiver, truncating toward zero.
        assert_eq!(
            word(
                &mut machine,
                "Duration",
                "millis",
                &[(Repr::Duration, built)]
            )
            .unwrap() as i64,
            1500
        );
        assert_eq!(
            word(
                &mut machine,
                "Duration",
                "seconds",
                &[(Repr::Duration, built)]
            )
            .unwrap() as i64,
            1
        );
        let negative = (-1_500_000_000i64) as u64;
        assert_eq!(
            word(
                &mut machine,
                "Duration",
                "seconds",
                &[(Repr::Duration, negative)]
            )
            .unwrap() as i64,
            -1,
            "toward zero, not down"
        );
    }

    /// A negative count is a negative duration; a count whose nanoseconds do
    /// not fit stops the run in the words `Duration` arithmetic stops it in.
    #[test]
    fn a_duration_builder_takes_a_negative_count_and_refuses_one_that_does_not_fit() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let built = word(
            &mut machine,
            "Duration",
            "hours",
            &[(Repr::Int, -1i64 as u64)],
        )
        .unwrap();
        assert_eq!(built as i64, -3_600_000_000_000);

        let error = run(
            &mut machine,
            "Duration",
            "hours",
            &[(Repr::Int, i64::MAX as u64)],
        )
        .unwrap_err();
        assert_eq!(error.message, "`Int` duration arithmetic overflowed");
    }

    #[test]
    fn a_receiver_of_the_wrong_kind_says_so() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let error = run(&mut machine, "Int", "abs", &[(Repr::Float, 0)]).unwrap_err();
        assert_eq!(error.message, "`Float` has no method `abs`");
        let error = run(
            &mut machine,
            "Float",
            "min",
            &[(Repr::Float, 0), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`Float.min` expects `Float` for `other`, but found `Int`"
        );
    }
}
