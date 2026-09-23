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
//
// `int_parse_radix` stood here, the last `Int` operation below the boundary.
// It raised on a radix outside `2..=36`, which is why it could not move with
// `Int.parse` in issue #454's Step 4; ADR 0067's `core.refuse` let it follow,
// and it is `std.int.parseRadix`.

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

// `float_format` stood here: `format!("{:.*}")`, and a raise on a `digits`
// outside `0..=17`. It is `std.float.format` — the value taken apart into
// `m * 2^e` with `Float` arithmetic, then written from an exact integer in
// base-`10^9` limbs, ties to even — and the raise is ADR 0067's `core.refuse`.

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
    use crate::vm::intrinsics::tests::{message_of, result_of, run, scalar, world};
    use cove_ir::Repr;

    // `parsing_an_int_separates_bad_data_from_a_bad_call` stood here and asked
    // `Int.parseRadix`'s arm the question its name says — text that is not a
    // number answers `Err`, a radix that names no notation stops the run. The
    // arm is gone (`std.int.parseRadix`, through ADR 0067's `core.refuse`), and
    // the question is answered where it can be asked of all three backends:
    // `tests/e2e/values_int_parse_radix`'s seventy lines against an oracle
    // written from the notation, and `tests/e2e/fail_int_parse_radix`'s three
    // sentences and blame.

    // `a_float_formats` stood here, asking this module's `format` arm for
    // `1.5` at three digits and then for eighteen. The arm is `std.float.format`
    // now, and what answers for it is `tests/e2e/values_float_format`,
    // `values_float_roundtrip` and `fail_float_format_digits` on all three
    // backends, and `vm::differential`'s sweep of twenty thousand binary64
    // values against `format!` itself.

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
