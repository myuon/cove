//! The float operations that are **specified here rather than inherited from
//! the standard library**, shared by every tier so that no two of them can
//! come to a different answer.
//!
//! `crate::find` is the shape of this module and the precedent for it: one
//! fact, held in one place, because two execution tiers reading two copies of
//! it is two copies to keep in step. What is different here is which copies —
//! this crate has *two* evaluators, and ADR 0055's native tier has two code
//! generators below one of them, so a float operation the language decides has
//! **four** implementations and needs one specification.
//!
//! Only what Rust declines to decide lives here. `Float.abs` does not: IEEE
//! 754 makes it a sign-bit operation and `f64::abs` is specified to clear bit
//! 63 and touch nothing else, so every tier may and does call it. `Float.sqrt`
//! does not, for the stronger version of the same reason — IEEE 754 requires a
//! correctly-rounded root, so the bits are the same on every conforming
//! machine. A method whose own documentation settles the question is a method
//! this module has no business restating.

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
/// **Four implementations, one specification, and this is it.** Both code
/// generators spell these four cases out because neither machine operation has
/// them — x86-64's `minsd` answers its second operand when *either* operand is
/// a NaN, where this absorbs one, and Cranelift's `fmin` is IEEE 754-2019's
/// `minimum` and propagates. This function is the other two callers: the
/// encoded VM's `FLOAT_MIN` and `FLOAT_MAX` arms, and **the tree-walking
/// interpreter's `Float.min` and `Float.max`, which is the semantic oracle the
/// other three are differentially tested against**. An oracle that inherited
/// its answer from whichever `rustc` built the binary would be an oracle that
/// could not say the others were wrong; `crates/cove-native`'s `tests/suite`'s
/// `EXTREMA` holds all of them to one table of bits, and
/// `crates/cove-cli/tests/e2e.rs` runs the corpus on both evaluators against
/// one golden file.
///
/// The alternative — letting each tier ask `f64::min` — is not stable even in
/// principle. A future `rustc` is entitled to flip the tie, and then the two
/// delegating tiers would move while the two that spell it out would not. That
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
