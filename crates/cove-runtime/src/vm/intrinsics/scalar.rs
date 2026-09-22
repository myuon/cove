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

use crate::error::RuntimeError;
use crate::vm::exec::Machine;
use crate::vm::intrinsics::operand::{Dest, Frame};
use crate::vm::intrinsics::{make, operand};

// --- Int -------------------------------------------------------------------

/// `Int.parseRadix(text, radix) -> Result<Int, Error>`.
///
/// A `radix` outside `2..=36` names no notation, so it stops the run the way
/// an empty `String.split` separator does; text that is not a number in a
/// radix that does exist is the data's failure and answers `Err`.
///
/// **`Int.parse` used to sit above this, and does not any more.** It was
/// `text.parse::<i64>()`, and issue #454's Step 4 made it `std.int.parse`: a
/// `core.byteLength` and one `byteAt` a byte, with an accumulator that runs
/// negative so that `-9223372036854775808` — whose magnitude no `Int` holds —
/// is read without a trap and `9223372036854775808` answers `Err` without
/// one. The two variants could not move together and the order issue #454
/// planned is inverted, because of the `return Err` two lines into the body
/// below: a radix outside the range *raises*, and a Cove body has nothing to
/// raise with. That is issue
/// [#461](https://github.com/myuon/cove/issues/461), and when it is decided
/// this arm goes too and `parse` becomes a radix-10 call into it.
pub(super) fn int_parse_radix(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    let radix = operand::int(machine, frame, 1);
    let Some(base) = (2..=36).contains(&radix).then_some(radix as u32) else {
        return Err(operand::radix(radix));
    };
    match i64::from_str_radix(&text, base) {
        Ok(value) => make::ok(machine, dest, &[value as u64]),
        Err(_) => {
            let message = format!("`{text}` is not an Int in radix {base}");
            make::failed(machine, dest, &message)
        }
    }
}

// --- Float -----------------------------------------------------------------

/// `Float.toInt() -> Result<Int, Error>`, truncating toward zero.
pub(super) fn float_to_int(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let x = operand::float(machine, frame, 0);
    if x.is_nan() {
        return make::failed(
            machine,
            dest,
            "`Float.toInt` cannot convert `NaN`, which is not a number",
        );
    }
    if x.is_infinite() {
        let message = format!("`Float.toInt` cannot convert `{x}`, which has no truncation");
        return make::failed(machine, dest, &message);
    }
    let truncated = x.trunc();
    if truncated < i64::MIN as f64 || truncated >= i64::MAX as f64 {
        let message = format!("`Float.toInt` cannot convert `{x}`, which is outside Int's range");
        return make::failed(machine, dest, &message);
    }
    make::ok(machine, dest, &[truncated as i64 as u64])
}

/// `Float.format(digits) -> String`, fixed-point.
pub(super) fn float_format(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let x = operand::float(machine, frame, 0);
    let digits = operand::int(machine, frame, 1);
    if !(0..=17).contains(&digits) {
        return Err(operand::format_digits(digits));
    }
    let text = format!("{:.*}", digits as usize, x);
    let word = machine.new_string(&text)?;
    dest.word(machine, word);
    Ok(())
}

/// `Float.parse(text) -> Result<Float, Error>`.
///
/// Rust's `f64::from_str` accepts `inf`, `-inf` and `NaN`, which is why this
/// does too, and rejects the `_` separators a literal may be written with —
/// the same thing `Int.parse` does.
pub(super) fn float_parse(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    match text.parse::<f64>() {
        Ok(value) => make::ok(machine, dest, &[value.to_bits()]),
        Err(_) => make::failed(machine, dest, &format!("`{text}` is not a Float")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::intrinsics::tests::{message_of, read, result_of, run, scalar, word, world};
    use cove_ir::Repr;

    /// Text that is not a number is the *data's* failure and answers `Err`; a
    /// radix that names no notation is the *call's* and stops the run.
    ///
    /// **It asked the same pair of `Int.parse` until issue #454's Step 4**,
    /// which moved that one to `std.int.parse` — and the half it took with it
    /// is the half this arm could not follow. `parse`'s refusals are all
    /// `Err` values, so a Cove body builds every one of them; `parseRadix`'s
    /// radix refusal is a *raise*, which a Cove body has nothing to make
    /// (issue #461). So the distinction this test is named for now lives
    /// entirely inside one variant, which is why the `Int.parse` rows are
    /// gone rather than repointed: what answers them is
    /// `tests/e2e/values_int_parse`, 103 golden lines against an independent
    /// oracle.
    #[test]
    fn parsing_an_int_separates_bad_data_from_a_bad_call() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        // A `Result` is a run of words — `[disc, Int]` — and not an object,
        // so what the answer is read out of is the words themselves.
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

    /// `sqrt`, `round`, `min` and `max` are not here any more: ADR 0064's
    /// last two Phase 1 migrations made the pair `Inst::FloatMinMax` and
    /// issue #454's Step 2 made `round` `Inst::FloatRound` and then `sqrt`
    /// `Inst::FloatSqrt`, instructions rather than runtime calls, so there is
    /// no arm of this module left to ask. **What is left in this file is the
    /// three operations that allocate or refuse** — `toInt`, `format`,
    /// `parse` — which is the same sentence `Intrinsic::effects` now makes
    /// about the whole enum. What the four answer is asserted in bits, on
    /// both tiers, by `vm::exec`'s
    /// `a_float_extremum_answers_one_of_its_operands`,
    /// `a_float_rounding_answers_the_nearest_integer` and
    /// `a_float_square_root_is_correctly_rounded`, and by `cove-native`'s
    /// `EXTREMA`, `ROUNDINGS` and `SQUARE_ROOTS`.
    #[test]
    fn a_float_formats() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let x = (-2.5f64).to_bits();

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
}
