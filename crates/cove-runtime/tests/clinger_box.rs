//! Where a decimal-to-binary64 conversion can be done with `Float` arithmetic
//! alone, and where it cannot.
//!
//! [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)
//! sends `Intrinsic::FloatParse` to "Cove over packed bytes", and issue #454's
//! Step 6 asked whether that is reachable. This file is the measurement the
//! answer rests on, and it is a sweep rather than an argument: the claim it
//! makes is about 3,157,200 specific pairs and about the least counterexample
//! past each edge of the region they cover.
//!
//! # What a Cove body has to work with
//!
//! `Float` has eight methods and an associated function — `toInt`, `round`,
//! `abs`, `sqrt`, `min`, `max`, `format`, the shared `snapshot`, and `parse` —
//! and `Float.parse` is the one being asked about.
//! `crates/cove-schema/src/builtins.rs`'s `FLOAT` is the whole list; there is
//! no `toBits` and no `fromBits`, and `INT` beside it has `toFloat`, `abs`,
//! `min`, `max`, `parse` and `parseRadix` and no bit surface either. Cove has
//! no bitwise operators at all: `crates/cove-syntax/src/token.rs` has `AmpAmp`
//! and `PipePipe` and nothing else in that family, which is why
//! `std.string.fromCodePoint` encodes UTF-8 with division and
//! multiply-subtract.
//!
//! So the route from a decimal `m * 10^e` to a `Float` that a Cove body can
//! take today is [`by_arithmetic`]: read the digits into an `Int`, call
//! `Int.toFloat`, and multiply or divide by a power of ten. [`REACHABLE`] is
//! that power of ten for the exponents a `Float` holds exactly; past those it
//! has to be built by multiplying, which is what [`by_arithmetic`] does and
//! what the edge cases below measure.
//!
//! The missing bit surface reads like a wall and is not one.
//! [`a_power_of_two_reaches_every_binary64_without_a_bit_surface`] is the
//! second half of the answer and the half that was expected to go the other
//! way: a conversion that has decided its significand and its binary exponent
//! does **not** need to assemble them into 64 bits, because `m * 2^e` with
//! `m < 2^53` is an exact `Float` multiplication — subnormals included. What
//! a general parser is short of is therefore arithmetic and not
//! representation, and Cove can do that arithmetic in software too. What it
//! costs is the question, and it is not one this file answers.
//!
//! # The box
//!
//! One IEEE 754 multiply or divide is correctly rounded, so when **both**
//! operands are exact the answer is exact. `m` is exact when it fits in 53
//! bits and `10^e` is exact when `|e| <= 22`, which is Clinger's fast path and
//! which [`the_box_is_exact`] sweeps rather than assumes.
//!
//! What matters more for the migration question is that the box is **tight**,
//! which [`every_edge_of_the_box_is_reached_immediately`] shows: one step past
//! any of its four edges the arithmetic route is wrong, on roughly a quarter
//! of mantissas, by one unit in the last place. There is no wider region to
//! find.
//!
//! # What the reference is
//!
//! `format!("{m}e{e}").parse::<f64>()` — which is `Intrinsic::FloatParse`'s
//! own arm in `crates/cove-runtime/src/vm/intrinsics/scalar.rs`, reached
//! directly. That is deliberate and is what the question needs: this file asks
//! whether an arithmetic body would **agree with the operation as it ships**,
//! so the operation as it ships is the reference by construction. That it is
//! also correctly rounded is established elsewhere and not assumed here —
//! `tests/e2e/values_float_parse` pins 173 rows of it against a bignum oracle
//! that does not call `str::parse::<f64>` at all.
//!
//! The sweep is about 3.2 million conversions and takes under a second, so it
//! is not `#[ignore]`d.

/// `10^k` for every `k` a binary64 holds exactly.
///
/// `10^22` is the last one: `10^23` needs 54 bits of significand and is
/// rounded. Every entry here is a literal rather than a product, so nothing
/// in this file builds its own reference by the method it is testing.
const REACHABLE: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// The conversion a Cove body could write today: an `Int` made a `Float`, then
/// one multiply or divide by a power of ten.
///
/// Past `REACHABLE`'s last entry the power of ten is built by multiplying,
/// which is what a Cove body would have to do and is where the second rounding
/// enters.
fn by_arithmetic(m: u64, e: i32) -> f64 {
    let mantissa = m as f64;
    let magnitude = e.unsigned_abs() as usize;
    let mut power = if magnitude < REACHABLE.len() {
        REACHABLE[magnitude]
    } else {
        let mut built = REACHABLE[REACHABLE.len() - 1];
        for _ in REACHABLE.len() - 1..magnitude {
            built *= 10.0;
        }
        built
    };
    if magnitude >= REACHABLE.len() && power.is_infinite() {
        power = f64::MAX;
    }
    if e >= 0 {
        mantissa * power
    } else {
        mantissa / power
    }
}

/// What `Intrinsic::FloatParse` answers for `m * 10^e`.
fn by_intrinsic(m: u64, e: i32) -> f64 {
    format!("{m}e{e}")
        .parse::<f64>()
        .expect("a mantissa and an exponent spell a Float")
}

/// A deterministic stream, so that a failure names a pair that can be looked
/// at again.
struct Stream(u64);

impl Stream {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Every mantissa the sweeps below use: the first twenty thousand integers,
/// every power of two and its two neighbours, and fifty thousand drawn from
/// the whole 53-bit range.
fn mantissas() -> Vec<u64> {
    let mut stream = Stream(0x9E37_79B9_7F4A_7C15);
    let mut all: Vec<u64> = (0..=20_000).collect();
    for bit in 0..53u32 {
        all.push(1 << bit);
        if bit > 0 {
            all.push((1 << bit) - 1);
            all.push((1 << bit) + 1);
        }
    }
    all.push((1 << 53) - 1);
    all.push(1 << 53);
    for _ in 0..50_000 {
        all.push(stream.next() % (1 << 53));
    }
    all
}

/// Inside the box, the arithmetic route is the operation, on every pair.
#[test]
fn the_box_is_exact() {
    let mantissas = mantissas();
    let mut pairs = 0u64;
    let mut wrong = Vec::new();
    for &m in &mantissas {
        for e in -22..=22i32 {
            pairs += 1;
            if by_arithmetic(m, e).to_bits() != by_intrinsic(m, e).to_bits() && wrong.len() < 8 {
                wrong.push((m, e));
            }
        }
    }
    assert_eq!(pairs, 3_157_200, "the sweep is the size it claims to be");
    assert!(
        wrong.is_empty(),
        "inside the box, yet disagreeing: {wrong:?}"
    );
}

/// One step past any edge, the arithmetic route is wrong at once — and wrong
/// on a large fraction of mantissas rather than on a contrived one.
///
/// The counts are the whole reason [`the_box_is_exact`] is not an argument for
/// a fast path being enough. They say the region has no margin: there is no
/// `|e| <= 23` and no `m <= 2^54` that could be reached by being a little more
/// careful.
///
/// **The four exponent rows are not monotone, and that is itself the point.**
/// `10^23` and `10^-23` disagree on more than half the sample and `10^24` and
/// `10^-24` on about a ninth, because how wrong the answer is depends on how
/// the power of ten was built — [`by_arithmetic`] multiplies `10^22` up one
/// factor at a time, and a different way of reaching the same power moves
/// every count in this test. A region whose error rate is a fact about the
/// construction rather than about the input is not a region anything can be
/// argued from.
#[test]
fn every_edge_of_the_box_is_reached_immediately() {
    let mantissas = mantissas();
    let sample: Vec<u64> = mantissas.iter().copied().take(30_000).skip(1).collect();

    // The exponent edges, with the mantissa still inside.
    assert_eq!(sample.len(), 29_999);
    for (exponent, least, count) in [
        (23i32, 3u64, 16_113u64),
        (-23, 1, 16_462),
        (24, 5, 3_339),
        (-24, 1, 3_273),
    ] {
        let mut disagree = 0u64;
        let mut first = None;
        for &m in &sample {
            if by_arithmetic(m, exponent).to_bits() != by_intrinsic(m, exponent).to_bits() {
                disagree += 1;
                if first.is_none() {
                    first = Some(m);
                }
            }
        }
        assert_eq!(
            (first, disagree),
            (Some(least), count),
            "the least mantissa that disagrees at 10^{exponent}, and how many do"
        );
    }

    // The mantissa edge, with the exponent still inside. `2^53 + 1` is the
    // least integer a binary64 does not hold, so `Int.toFloat` rounds it
    // before the multiply ever happens.
    let mut stream = Stream(0x2545_F491_4F6C_DD1D);
    let mut big: Vec<u64> = vec![(1 << 53) + 1, (1 << 53) + 3, (1 << 53) + 5];
    for _ in 0..30_000 {
        big.push((1 << 53) + stream.next() % (1 << 60));
    }
    let mut pairs = 0u64;
    let mut disagree = 0u64;
    let mut first = None;
    for &m in &big {
        for e in -22..=22i32 {
            pairs += 1;
            if by_arithmetic(m, e).to_bits() != by_intrinsic(m, e).to_bits() {
                disagree += 1;
                if first.is_none() {
                    first = Some((m, e));
                }
            }
        }
    }
    assert_eq!(
        (first, disagree, pairs),
        (Some(((1 << 53) + 1, -22)), 330_029, 1_350_135),
        "past the mantissa edge, with the exponent still inside"
    );
}

/// The two smallest counterexamples, written out, because a percentage is not
/// a thing anybody can check by hand.
///
/// Each is wrong by one unit in the last place, which is the whole distance a
/// second rounding can move an answer and is also the whole distance that
/// makes a conversion incorrect.
#[test]
fn the_least_counterexamples_are_one_unit_apart() {
    for (m, e) in [(1u64, -23i32), ((1u64 << 53) + 1, 1)] {
        let mine = by_arithmetic(m, e).to_bits();
        let theirs = by_intrinsic(m, e).to_bits();
        assert_ne!(mine, theirs, "{m}e{e} was expected to disagree");
        assert_eq!(mine.abs_diff(theirs), 1, "{m}e{e} is off by one place");
    }
}

/// What cq's inputs look like against the box, recorded because the number is
/// the thing a later fast path would be argued from.
///
/// cq hands `Float.parse` 60,000 decimals on the 20,000-record run ADR 0064's
/// measurement section names — three per record, from
/// `examples/cq/json/json.cove`'s `parseNumber`. Every one of the 60,000 is
/// inside the box, and by a wide margin: the widest mantissa is 14 bits and
/// the exponents are `0`, `-1` and `-2` only. There are **ten distinct
/// values** among the sixty thousand.
///
/// That is the census issue #454 asked for and it is the reason the answer is
/// still no. A body exact on 100% of a real program's input and wrong on more
/// than half the decimals one step outside it is not this operation; it is a
/// different operation that agrees with this one on cq. The ten values are a
/// fixture rather than a walk over the file because the file is generated —
/// `cove run cqSample --files-root cq/data -- 20000 bookings-20k.jsonl` — and
/// is not in the repository, so the ten are also a fact about that generator
/// and not about JSON.
#[test]
fn every_decimal_cq_reads_is_inside_the_box() {
    // The ten distinct number tokens in `bookings-20k.jsonl`, as `m * 10^e`.
    let cq: [(u64, i32); 10] = [
        (1, 0),
        (2, 0),
        (3, 0),
        (4, 0),
        (5, 0),
        (6, 0),
        (7, 0),
        (1840, -1),
        (1295, -1),
        (9625, -2),
    ];
    for (m, e) in cq {
        assert!(m < (1 << 53) && e.abs() <= 22, "{m}e{e} is inside the box");
        assert_eq!(
            by_arithmetic(m, e).to_bits(),
            by_intrinsic(m, e).to_bits(),
            "{m}e{e}"
        );
    }
}

/// Every finite binary64 is reachable from a 53-bit `Int` and a power of two,
/// with no bit surface anywhere.
///
/// This is the fact that decides what `Float.parse`'s migration is actually
/// blocked on, and it was expected to go the other way. A correctly-rounded
/// decimal-to-binary64 conversion ends by *assembling* a result — a sign, an
/// eleven-bit exponent and a 52-bit fraction — and Cove has no
/// `Float.fromBits` and no bitwise operator to assemble it with, which reads
/// like a wall. It is not one: `m * 2^e` with `m < 2^53` is an exact
/// binary64 multiplication whenever the result is representable, because both
/// operands are exact and scaling by a power of two only moves an exponent.
/// So the assembly is a multiply, and the multiply is a `Float` operation Cove
/// has had since the beginning.
///
/// It holds at both ends and the ends are where it would fail if it failed:
/// `2^-1074` is itself exact, `1 * 2^-1074` is the least subnormal, and
/// `(2^52) * 2^-1074` is `DBL_MIN` — so the subnormal range, where a
/// significand has fewer than 53 bits and a naive scaling would round twice,
/// is exact as well.
///
/// The power of two is built by doubling or halving from `1.0`, which is what
/// a Cove body would have to do and is exact at every step. A body that wanted
/// it in eleven multiplications rather than up to 1,074 would hold a table of
/// `2^(2^k)`, every entry of which is exact too.
///
/// **What this leaves as the real obstruction is arithmetic, not
/// representation.** Deciding *which* `(m, e)` a decimal names, for a decimal
/// this corpus' `wide` group writes with 750 digits, is exact integer work on
/// numbers up to about `10^768` — and Cove can do that too, in software, over
/// a `Vector<Int>` of limbs small enough that no product overflows an `Int`.
/// Nothing in the language is missing. What is missing is the body, and its
/// cost: a slow path of that shape is hundreds of thousands of `Int`
/// operations and a `Vector` allocation where the operation today is one
/// `Inst::IntrinsicCall` and no allocation at all, which is what ADR 0064's
/// Decision 8 asks about and not something a fast path that covers 100% of cq
/// answers.
#[test]
fn a_power_of_two_reaches_every_binary64_without_a_bit_surface() {
    /// `2^e`, from `1.0`, doubled or halved — the only way a Cove body could.
    fn power_of_two(e: i32) -> f64 {
        let mut power = 1.0f64;
        for _ in 0..e.unsigned_abs() {
            if e >= 0 {
                power *= 2.0;
            } else {
                power /= 2.0;
            }
        }
        power
    }

    let mut stream = Stream(0x243F_6A88_85A3_08D3);
    let mut rebuilt = 0u64;
    for _ in 0..400_000 {
        let word = stream.next();
        let value = f64::from_bits(word);
        if !value.is_finite() {
            continue;
        }
        let bits = word & 0x7FFF_FFFF_FFFF_FFFF;
        let exponent_field = (bits >> 52) & 0x7FF;
        let fraction = bits & 0x000F_FFFF_FFFF_FFFF;
        let (m, e) = if exponent_field == 0 {
            (fraction, -1074)
        } else {
            (fraction + (1 << 52), exponent_field as i32 - 1075)
        };
        assert!(m < (1 << 53));
        assert_eq!(
            ((m as f64) * power_of_two(e)).to_bits(),
            bits,
            "m={m} e={e}"
        );
        rebuilt += 1;
    }
    assert_eq!(rebuilt, 399_813, "the sweep is the size it claims to be");

    // The three values where the claim would break first, spelled out.
    assert_eq!(
        (1.0 * power_of_two(-1074)).to_bits(),
        1,
        "the least subnormal"
    );
    assert_eq!(
        (((1u64 << 52) as f64) * power_of_two(-1074)).to_bits(),
        0x0010_0000_0000_0000,
        "the smallest normal"
    );
    assert_eq!(
        ((((1u64 << 53) - 1) as f64) * power_of_two(971)).to_bits(),
        0x7FEF_FFFF_FFFF_FFFF,
        "the largest finite"
    );
    assert_eq!(
        power_of_two(-1075),
        0.0,
        "one step below the least subnormal"
    );
}
