//! The float operations that are **specified here rather than inherited from
//! the standard library**, shared by every tier so that no two of them can
//! come to a different answer.
//!
//! `crate::find` is the shape of this module and the precedent for it: one
//! fact, held in one place, because two execution tiers reading two copies of
//! it is two copies to keep in step. What is different here is which copies —
//! this crate has *two* evaluators, and ADR 0055's native tier has a code
//! generator below one of them, so a float operation the language decides has
//! **three** implementations and needs one specification. It had four until
//! [ADR 0066](../../../docs/adr/0066-a-comparison-ends-when-its-question-is-answered.md)
//! retired the Cranelift arm: a change to the count and to nothing else about
//! the argument, since one copy fewer is still more than one.
//!
//! Only what Rust declines to decide lives here. `Float.abs` does not: IEEE
//! 754 makes it a sign-bit operation and `f64::abs` is specified to clear bit
//! 63 and touch nothing else, so every tier may and does call it.
//!
//! **`Float.sqrt` does not either, for a stronger reason with one measured
//! exception.** IEEE 754 §5.4.1 makes `squareRoot` correctly rounded, so for
//! every operand whose answer is a *number* there is one answer and it is the
//! same on every conforming machine — which was checked rather than repeated,
//! over 3.1 million operands, by comparing `f64::sqrt`'s answer against the
//! exact midpoints to both of its neighbours in integer arithmetic. What the
//! standard does **not** fix is the quiet NaN an *invalid* operation answers:
//! §6.2 leaves its sign and payload to the implementation, so `(-1.0).sqrt()`
//! is `0xfff8_0000_0000_0000` on x86-64 — sign bit set — and need not be that
//! anywhere else. That is not a reason for a `float::sqrt` beside
//! [`extremum`]: a Cove program cannot observe a NaN's sign or payload, so
//! there is no contract to state, and stating one would mean computing a root
//! by hand in order to fix bits nobody can read. It is a reason for
//! `cove-native`'s `SQUARE_ROOTS` to be a table about x86-64 and for
//! `cove-runtime`'s to assert *quietness* and stop, which is what each of
//! them does.
//!
//! A method whose own documentation settles the question is a method this
//! module has no business restating.

use cove_ir::MinMax;

/// [`Inst::FloatMinMax`](cove_ir::Inst::FloatMinMax), spelled out.
///
/// ```text
/// min(a, b) = b is NaN -> a | a is NaN -> b | a < b -> a | otherwise b
/// max(a, b) = b is NaN -> a | a is NaN -> b | a > b -> a | otherwise b
/// ```
///
/// **It does not call `f64::min`, and that is deliberate — do not simplify it
/// back.** The method's answer on operands that compare equal is documented as
/// *not determined*: "if the inputs compare equal (such as for the case of
/// `+0.0` and `-0.0`), either input may be returned non-deterministically".
/// This operation's answer on those operands **is** determined — it is the
/// second operand, and `tests/e2e/values_float_min_max` is where a Cove
/// program is seen depending on it, through the three observers that can tell
/// the zeros apart, in both argument orders.
///
/// **Three implementations, one specification, and this is it.** The native
/// code generator spells these four cases out because no machine operation has
/// them — x86-64's `minsd` answers its second operand when *either* operand is
/// a NaN, where this absorbs one, and an IEEE 754-2019 `minimum`, which is what
/// a compiler back end's `fmin` usually is and what the retired Cranelift arm's
/// was, propagates one. This function is the other two callers: the encoded
/// VM's `FLOAT_MIN` and `FLOAT_MAX` arms, and **the tree-walking interpreter's
/// `Float.min` and `Float.max`, which is the semantic oracle the other two are
/// differentially tested against**. An oracle that inherited its answer from
/// whichever `rustc` built the binary would be an oracle that could not say the
/// others were wrong; `crates/cove-native`'s `tests/suite`'s `EXTREMA` holds
/// them to one table of bits, and `crates/cove-cli/tests/e2e.rs` runs the
/// corpus on both evaluators against one golden file.
///
/// The alternative — letting each tier ask `f64::min` — is not stable even in
/// principle. A future `rustc` is entitled to flip the tie, and then the tiers
/// that delegate would move while the one that spells it out would not. That
/// is the complaint [ADR 0064]'s Decision 5 makes about the Unicode tables: a
/// behaviour that is a fact about which compiler built the binary, with
/// nothing in the repository able to say which answer is right.
///
/// It takes and answers **words** rather than `f64`s, so the chosen operand is
/// handed back bit for bit: a signalling NaN that wins stays signalling, and
/// nothing here can quiet or round. `EXTREMA`'s last rows are what say so, and
/// the word in and the word out are why the interpreter's arm bit-casts on
/// both sides rather than passing its `f64` through.
///
/// One function and one flag, for the reason [`MinMax`] is a flag at all: the
/// contract is written once and not once per member.
///
/// [ADR 0064]: ../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
#[inline]
pub(crate) fn extremum(x: u64, y: u64, op: MinMax) -> u64 {
    let (a, b) = (f64::from_bits(x), f64::from_bits(y));
    if b.is_nan() {
        x
    } else if a.is_nan() {
        y
    } else {
        // Strictly, so that operands which compare equal fall through to the
        // second — `-0.0` against `+0.0` is the pair that can tell.
        let a_wins = match op {
            MinMax::Min => a < b,
            MinMax::Max => a > b,
        };
        if a_wins {
            x
        } else {
            y
        }
    }
}
