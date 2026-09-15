//! `Int` and `Float`.
//!
//! A scalar is one word, and a `Result` is a run of words rather than an
//! object, so the only thing any of these allocates is text: the `String` a
//! `format` builds, and the message an `Err` explains itself with. What each
//! one *means* is the oracle's, including the two places the answer is not
//! the obvious one:
//!
//! - **`Float.toInt()` answers a `Result`**, because three floats have no
//!   truncation that fits: `NaN`, an infinity, and a magnitude at or past
//!   2^63. Each is named separately.
//!
//! `Int.toFloat` and `Duration.nanos` are not here: each is an
//! [`Inst::Convert`](cove_ir::Inst::Convert) since ADR 0058's Phase 5 (#378,
//! P5-2), because a conversion of one word is not a runtime call's worth of
//! work.

use cove_ir::{LayoutId, Repr};

use crate::error::RuntimeError;
use crate::vm::builtins::operand::Operand;
use crate::vm::builtins::{make, operand};
use crate::vm::exec::Machine;

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

// --- Int -------------------------------------------------------------------

/// `Int.parse(text) -> Result<Int, Error>`.
///
/// Rust's `str::parse::<i64>` reads a leading `+` or `-` and no digit
/// separators, which is why a `1_000` that a literal may be written with is
/// an `Err` here.
pub(super) fn int_parse(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let args = operand::free("Int.parse", operands, 1)?;
    let text = operand::text(machine, "Int.parse", "text", args[0])?;
    match text.parse::<i64>() {
        Ok(value) => make::ok(machine, result, &[value as u64], out),
        Err(_) => make::failed(machine, result, &format!("`{text}` is not an Int"), out),
    }
}

/// `Int.parseRadix(text, radix) -> Result<Int, Error>`.
///
/// A `radix` outside `2..=36` names no notation, so it stops the run the way
/// an empty `String.split` separator does; text that is not a number in a
/// radix that does exist is the data's failure and answers `Err`.
pub(super) fn int_parse_radix(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let args = operand::free("Int.parseRadix", operands, 2)?;
    let text = operand::text(machine, "Int.parseRadix", "text", args[0])?;
    let radix = operand::int(machine, "Int.parseRadix", "radix", args[1])?;
    let Some(base) = (2..=36).contains(&radix).then_some(radix as u32) else {
        return Err(operand::radix(radix));
    };
    match i64::from_str_radix(&text, base) {
        Ok(value) => make::ok(machine, result, &[value as u64], out),
        Err(_) => {
            let message = format!("`{text}` is not an Int in radix {base}");
            make::failed(machine, result, &message, out)
        }
    }
}

// --- Float -----------------------------------------------------------------

/// `Float.toInt() -> Result<Int, Error>`, truncating toward zero.
pub(super) fn float_to_int(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (self_, _) = operand::method("toInt", operands, 0)?;
    let x = float_receiver(machine, "toInt", self_)?;
    if x.is_nan() {
        return make::failed(
            machine,
            result,
            "`Float.toInt` cannot convert `NaN`, which is not a number",
            out,
        );
    }
    if x.is_infinite() {
        let message = format!("`Float.toInt` cannot convert `{x}`, which has no truncation");
        return make::failed(machine, result, &message, out);
    }
    let truncated = x.trunc();
    if truncated < i64::MIN as f64 || truncated >= i64::MAX as f64 {
        let message = format!("`Float.toInt` cannot convert `{x}`, which is outside Int's range");
        return make::failed(machine, result, &message, out);
    }
    make::ok(machine, result, &[truncated as i64 as u64], out)
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

/// `Float.sqrt() -> Float`.
///
/// IEEE 754 requires a correctly-rounded square root, so this is the one
/// irrational operation whose bits are the same on every conforming
/// machine — the whole reason issue #250 asked for it. It traps on
/// nothing: Rust's `f64::sqrt` answers `NaN` for a negative operand
/// (`-0.0` included, whose root is `-0.0` rather than `NaN`) exactly as
/// IEEE 754 does, and `Float`'s other primitives already leave `NaN` and
/// signed-zero semantics undecided — see issue #254 — so this does not
/// decide them either.
pub(super) fn float_sqrt(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("sqrt", operands, 0)?;
    Ok(float_receiver(machine, "sqrt", self_)?.sqrt().to_bits())
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
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let args = operand::free("Float.parse", operands, 1)?;
    let text = operand::text(machine, "Float.parse", "text", args[0])?;
    match text.parse::<f64>() {
        Ok(value) => make::ok(machine, result, &[value.to_bits()], out),
        Err(_) => make::failed(machine, result, &format!("`{text}` is not a Float"), out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::tests::{message_of, read, result_of, run, scalar, word, world};

    fn float_of(machine: &mut Machine, operation: &str, operands: &[(Repr, u64)]) -> f64 {
        f64::from_bits(word(machine, "Float", operation, operands).unwrap())
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

    /// `sqrt` answers what IEEE 754 answers: the correctly-rounded root for
    /// a non-negative operand, `NaN` for a negative one, and `-0.0` for
    /// `-0.0` — the one case where a negative operand's root is not `NaN`.
    #[test]
    fn sqrt_answers_what_ieee_754_answers() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        assert_eq!(
            float_of(&mut machine, "sqrt", &[(Repr::Float, 4.0f64.to_bits())]),
            2.0
        );
        assert_eq!(
            float_of(&mut machine, "sqrt", &[(Repr::Float, 2.0f64.to_bits())]),
            std::f64::consts::SQRT_2
        );
        assert_eq!(
            float_of(&mut machine, "sqrt", &[(Repr::Float, 0.0f64.to_bits())]),
            0.0
        );
        let negative_zero = float_of(&mut machine, "sqrt", &[(Repr::Float, (-0.0f64).to_bits())]);
        assert_eq!(negative_zero, 0.0);
        assert!(negative_zero.is_sign_negative());
        assert!(float_of(&mut machine, "sqrt", &[(Repr::Float, (-1.0f64).to_bits())]).is_nan());
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

    #[test]
    fn a_receiver_of_the_wrong_kind_says_so() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let error = run(&mut machine, "Float", "round", &[(Repr::Int, 0)]).unwrap_err();
        assert_eq!(error.message, "`Int` has no method `round`");
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
