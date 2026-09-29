//! `Int` and `Float`.
//!
//! A scalar is one word, and a `Result` is a run of words rather than an
//! object, so the only thing any of these allocates is text: the message an
//! `Err` explains itself with. What each one *means* is the oracle's.
//!
//! `Float.toInt()` is not here: it is `std.float.toInt`, one
//! `Inst::FloatTruncate` and its three refusals in Cove (issue #432, ADR
//! 0071).
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

// `float_to_int` stood here: `f64::trunc`, and the three refusals worded with
// Rust's `{}`. It is `std.float.toInt` — `core.floatTruncate`, which is
// `Inst::FloatTruncate`, and the sentences in Cove on the path that refuses —
// and the range sentence quotes the value as Cove renders a `Float` (issue
// #432, ADR 0071).

// `float_format` stood here: `format!("{:.*}")`, and a raise on a `digits`
// outside `0..=17`. It is `std.float.format` — the value taken apart into
// `m * 2^e` with `Float` arithmetic, then written from an exact integer in
// base-`10^9` limbs, ties to even — and the raise is ADR 0067's `core.refuse`.

/// `Intrinsic::FloatParse`, the arm of an intrinsic call **no program emits**.
///
/// It was `Float.parse` until issue #432 made that `std.float.parse`: the
/// grammar, Clinger's fast path and simple decimal conversion over at most 768
/// digits, in Cove (ADR 0072), held to this arm's answers bit for bit by
/// `tests/float_parse.rs` before and after. The lowering no longer reaches
/// `Intrinsic::FloatParse`, and it was the last variant, so no source makes an
/// `Inst::IntrinsicCall` at all.
///
/// **It stays until the mechanism goes, and only for the mechanism's own
/// cases.** The reporting and the native helper protocol are held to account
/// by cases that need an intrinsic which runs — `vm::report`'s and
/// `native_tier.rs`'s put the call back into lowered IR by hand — and this is
/// the only arm there is to run. The change that deletes `Intrinsic` and
/// `IntrinsicCall` deletes this with them.
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

    // `to_int_names_each_of_the_three_failures` stood here, asking this
    // module's `toInt` arm for a truncation and its three refusals. The arm is
    // `std.float.toInt` now, and what answers for it is
    // `tests/e2e/values_float_to_int` on the interpreter and the VM,
    // `values_float_to_int_native` and `native_tier.rs` on the native tier,
    // and `vm::exec`'s and `cove-native`'s `TRUNCATIONS` for the instruction
    // beneath it.

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
