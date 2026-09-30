//! [ADR 0074](../../../docs/adr/0074-an-int-also-carries-a-fixed-width-bit-pattern.md)'s
//! bit operations on an `Int`, specified once for the two evaluators.
//!
//! `crate::float` is the shape of this module and the reason for it: the
//! tree-walking interpreter and the encoded VM each answer `Int.bitAnd` and
//! its six neighbours, and one specification is what keeps them from coming
//! to two answers. The native tier's lowering is held to the same rows by
//! `cove-native`'s suite, and to this tier by `native_tier.rs` and
//! `int_bits.rs`.
//!
//! Every operation is written in explicitly fixed-width Rust. A shift's count
//! is checked *before* anything shifts, so no host shift is ever asked for a
//! count its CPU would mask: Rust's `<<` panics on one in a checked build and
//! masks it in an optimised one, and neither is Cove's contract.

use cove_ir::{shift_count_refused, BitOp, ShiftOp, SHIFT_COUNT_GREATEST, SHIFT_COUNT_LEAST};

use crate::error::RuntimeError;

/// [`cove_ir::Inst::Bits`]: `a op b` over the two words' 64 bits.
#[inline]
pub(crate) fn bits(op: BitOp, a: i64, b: i64) -> i64 {
    match op {
        BitOp::And => a & b,
        BitOp::Or => a | b,
        BitOp::Xor => a ^ b,
    }
}

/// [`cove_ir::Inst::BitNot`]: every bit of `a` complemented.
#[inline]
pub(crate) fn bit_not(a: i64) -> i64 {
    !a
}

/// [`cove_ir::Inst::Shift`]: `a` shifted by `n`, or `None` when `n` is not a
/// shift count.
///
/// - `Left` is the word read as unsigned, shifted, and read back: the bits
///   past bit 63 are gone and that is not an overflow.
/// - `Right` is `i64`'s own `>>`, which is arithmetic — `floor(a / 2^n)`,
///   the sign bit copied in.
/// - `RightLogical` is the word read as a `u64`, shifted, and its bits read
///   back as an `i64`. That is a reinterpretation, not a numeric conversion:
///   `as` between two integers of one width keeps the bits.
///
/// The count is checked first and only a count in `0..=63` reaches a shift,
/// which is what makes each `<<` and `>>` here defined on every build.
#[inline]
pub(crate) fn shift(op: ShiftOp, a: i64, n: i64) -> Option<i64> {
    if !(SHIFT_COUNT_LEAST..=SHIFT_COUNT_GREATEST).contains(&n) {
        return None;
    }
    let n = n as u32;
    Some(match op {
        ShiftOp::Left => ((a as u64) << n) as i64,
        ShiftOp::Right => a >> n,
        ShiftOp::RightLogical => ((a as u64) >> n) as i64,
    })
}

/// The error a shift by `count` stops the run with, spanless: each tier puts
/// its own span on it, which is the call's.
///
/// Out of line and cold, because it is the path a correct program never
/// takes and the arms that reach it are in the encoded VM's dispatch loop.
#[cold]
#[inline(never)]
pub(crate) fn shift_count(count: i64) -> RuntimeError {
    RuntimeError::new(shift_count_refused(count)).with_rule(
        "A shift count is 0 through 63. It is checked rather than masked, so a count of 64 \
         is an error and not a shift by 0.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADR's own table, row for row.
    #[test]
    fn the_adr_s_examples() {
        assert_eq!(bits(BitOp::And, 5, 3), 1);
        assert_eq!(bits(BitOp::Or, 5, 3), 7);
        assert_eq!(bits(BitOp::Xor, 5, 3), 6);
        assert_eq!(bit_not(0), -1);
        assert_eq!(bit_not(-1), 0);
        assert_eq!(shift(ShiftOp::Left, 1, 63), Some(i64::MIN));
        assert_eq!(shift(ShiftOp::Left, -1, 1), Some(-2));
        assert_eq!(shift(ShiftOp::Right, -3, 1), Some(-2));
        assert_eq!(shift(ShiftOp::RightLogical, -1, 1), Some(i64::MAX));
        assert_eq!(shift(ShiftOp::RightLogical, -1, 63), Some(1));
    }

    /// Every count outside `0..=63` is refused, on all three, including the
    /// two ends of `Int` — and the refusal is before any shift, which in a
    /// build with overflow checks is what keeps this test from panicking.
    #[test]
    fn a_count_outside_the_word_is_refused() {
        for op in [ShiftOp::Left, ShiftOp::Right, ShiftOp::RightLogical] {
            for n in [-1, 64, 65, i64::MIN, i64::MAX] {
                assert_eq!(shift(op, 1, n), None, "{op:?} by {n}");
            }
        }
        assert_eq!(
            shift_count(64).message,
            "a shift count must be between 0 and 63, got 64"
        );
    }
}
