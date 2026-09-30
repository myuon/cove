//! `Float.parse` against Rust's `str::parse::<f64>`, bit for bit.
//!
//! `tests/e2e/values_float_parse` pins 173 rows with a golden a bignum oracle
//! wrote, and those rows are the contract's named edges. What a golden cannot
//! be is *many*: a correctly rounded conversion is wrong, when it is wrong, on
//! an input nobody thought to write down. This file is the other half, and it
//! is issue #432's: every answer the subject gives is compared with the answer
//! `str::parse::<f64>` gives for the same text, as 64 bits — a `NaN` as "is
//! `NaN`", because a program cannot tell two of them apart — and every refusal
//! with its message, byte for byte.
//!
//! It was written while `Float.parse` *was* `str::parse::<f64>`, behind
//! `Intrinsic::FloatParse`, so on the day it landed it compared Rust with
//! itself through the whole of the VM's intrinsic path, and that is what it was
//! for: to be green against the operation as it shipped before a line of the
//! replacement existed. From the commit that made the parse `std.float.parse`,
//! a Cove body, `str::parse` is an oracle the subject no longer ships.
//!
//! # The inputs
//!
//! - **Adversarial rows**, built rather than listed where they are long: the
//!   exact midpoint between two neighbouring `Float`s — normal, subnormal and
//!   next to the largest — written with 17, 19, 20, 40, 767, 768 and 769
//!   significant digits (padded with zeros where the exact expansion is
//!   shorter, cut where it is longer) and one unit either side of that in the
//!   last digit; the midpoint followed by `0…01` past the 768th digit and by
//!   zeros past it; the midpoints either side of the least normal value and of
//!   the largest finite one; the least subnormal, its half and their exact
//!   751- and 752-digit expansions; Clinger's box at `e = ±22, ±23` and
//!   `m = 2^53 ± 1`; exponents no `Int` holds and exponents a long mantissa
//!   cancels; runs of leading and trailing zeros; every sign of zero; the three
//!   words in mixed case; and text that is not a number.
//! - **Seeded random inputs**: random bit patterns spelled with `{:e}`, with
//!   `Display` (the shortest round-trip, positional) and with 17 significant
//!   digits; random digit strings of 1 to 800 digits with a random point and a
//!   random exponent; and random strings over the grammar's own alphabet, most
//!   of which are refused.
//! - **The middle tier's rows** (ADR 0072's Eisel–Lemire product), in
//!   [`middle_tier`]: the edges of its table of powers of five, its exact
//!   ties and the inputs its ambiguity test sends on, midpoints cut to 18 and
//!   19 digits where a dropped digit makes it ask twice, the subnormals it
//!   rounds itself and the least normal value's boundary.
//! - **Seeded round-trips**, in [`round_trips`]: random bit patterns spelled
//!   shortest, with 17 digits and with 18, and random strings of 15 to 19
//!   digits with an exponent anywhere from −350 to 310.
//!
//! The default run is about eighteen thousand inputs. The four
//! `a_wide_random_sweep_agrees_*` cases are a hundred thousand more, the two
//! `a_wide_round_trip_sweep_agrees_*` cases another hundred thousand, and all
//! six are `#[ignore]`d, which is what `cargo ratchet` runs.
//!
//! # The tiers
//!
//! Every case runs on the encoded VM. Under the `template` feature the same
//! inputs run on the native tier too, through a refused caller of a compiled
//! callee — the shape `native_tier.rs` uses — and the case asserts that the
//! crossing into compiled code was taken, so that it cannot compare the VM
//! with itself.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::{Grants, HostRegistry, Runtime, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

const MODULE: &str = "m";

/// The subject, in a function the native tier compiles, and a caller it
/// refuses.
///
/// `counts` is recursive, so `parses` is not a leaf and
/// `cove_ir::lower::inline` cannot expand it into `callsParses`: an expanded
/// parse would run on the caller's tier, which is the VM.
const SOURCE: &str = "\
/// Recursive, so no caller of this can be inlined away. `counts(0)` is zero.
export fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// `Float.parse`, in a body the native tier compiles.
export fn parses(text: String, n: Int) -> Result<Float, Error> {
  let guard = counts(n)
  Float.parse(text)
}

/// A caller the native tier refuses, so that the parse is reached across the
/// boundary: the outermost frame is always encoded.
export fn callsParses(text: String, n: Int) -> Result<Float, Error> {
  let nothing = Shared(0).lock(fn(v) { v })
  parses(text, n)
}
";

/// What a parse answered, in the terms the comparison is made in.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    /// A `Float` that is not a `NaN`, as its 64 bits.
    Bits(u64),
    /// A `NaN`, whichever one.
    Nan,
    /// An `Err`, as its message.
    Refused(String),
}

/// What Rust's own parser answers, and what the subject is held to.
fn oracle(text: &str) -> Answer {
    match text.parse::<f64>() {
        Ok(x) if x.is_nan() => Answer::Nan,
        Ok(x) => Answer::Bits(x.to_bits()),
        Err(_) => Answer::Refused(format!("`{text}` is not a Float")),
    }
}

/// A `Result<Float, Error>` value, in the same terms.
fn answered(value: &Value) -> Answer {
    if let Some(payload) = value.ok_payload() {
        let x = payload[0].as_float().expect("an `Ok` carries a `Float`");
        return if x.is_nan() {
            Answer::Nan
        } else {
            Answer::Bits(x.to_bits())
        };
    }
    let payload = value.err_payload().expect("a parse answers `Ok` or `Err`");
    let message = payload[0]
        .error_message()
        .and_then(Value::as_str)
        .expect("an `Err` carries an `Error` with a message");
    Answer::Refused(message.to_string())
}

/// [`SOURCE`] checked, with the standard library attached.
fn checked() -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::from([(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("")
        ),
    }
}

/// One program, lowered once, that every input of a case runs through.
struct Subject {
    program: Arc<cove_sema::resolve::Program>,
    sources: Arc<SourceMap>,
    lowered: Arc<cove_ir::Program>,
}

impl Subject {
    fn new() -> Subject {
        let (sources, program) = checked();
        let lowered = Arc::new(
            cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
                .expect("the fixture lowers"),
        );
        Subject {
            program,
            sources,
            lowered,
        }
    }

    /// Every input's answer on the encoded VM, each on a machine of its own.
    fn on_the_vm(&self, inputs: &[String]) -> Vec<Answer> {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        inputs
            .iter()
            .map(|text| {
                let value = Vm::new(&runtime, &hosts, &self.lowered)
                    .invoke(
                        MODULE,
                        "parses",
                        vec![Value::string(text.as_str()), Value::int(0)],
                    )
                    .unwrap_or_else(|error| panic!("{text:?} stopped the VM: {}", error.message));
                answered(&value)
            })
            .collect()
    }

    /// Every input's answer from the native tier's compiled `parses`.
    #[cfg(feature = "template")]
    fn on_the_native_tier(&self, inputs: &[String]) -> Vec<Answer> {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        let native = cove_runtime::compile_native(&self.lowered).expect("this host compiles");
        let refused: Vec<&str> = native
            .refusals()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert!(
            !refused.contains(&"m.parses"),
            "`parses` is meant to be compiled, and the tier refused it"
        );
        assert!(
            refused.contains(&"m.callsParses"),
            "`callsParses` is meant to be refused, and the tier took it"
        );
        // Since issue #432 the parse is Cove, and all of it is compiled: a
        // row that fell back to the VM in the middle tier or the slow path
        // would be the VM's answer again.
        for name in [
            "std.float.parse",
            "std.float.parseMiddle",
            "std.float.eiselLemire",
            "std.float.parseSlow",
        ] {
            assert!(
                !refused.contains(&name),
                "`{name}` is meant to be compiled, and the tier refused it"
            );
        }
        inputs
            .iter()
            .map(|text| {
                let mut vm = Vm::with_native(&runtime, &hosts, &self.lowered, &native);
                let value = vm
                    .invoke(
                        MODULE,
                        "callsParses",
                        vec![Value::string(text.as_str()), Value::int(0)],
                    )
                    .unwrap_or_else(|error| {
                        panic!("{text:?} stopped the native tier: {}", error.message)
                    });
                assert!(
                    vm.tiers().vm_to_native >= 1,
                    "{text:?}: the crossing into the compiled parse was taken: {:?}",
                    vm.tiers()
                );
                answered(&value)
            })
            .collect()
    }
}

/// Holds `answers` to the oracle, naming every input that disagreed.
fn agree(tier: &str, inputs: &[String], answers: &[Answer]) {
    let mut wrong = Vec::new();
    for (text, answer) in inputs.iter().zip(answers) {
        let expected = oracle(text);
        if *answer != expected {
            let shown: String = if text.len() > 120 {
                let head: String = text.chars().take(120).collect();
                format!("{head}…({} bytes)", text.len())
            } else {
                text.clone()
            };
            wrong.push(format!(
                "  {shown:?}: {tier} {answer:?}, str::parse {expected:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} input(s) disagree with str::parse::<f64> on the {tier}:\n{}",
        wrong.len(),
        inputs.len(),
        wrong.join("\n")
    );
}

// ---------------------------------------------------------------- decimals

/// A positive decimal `0.d1 d2 … dn × 10^point`, with `d1` not nought: the
/// digits a row is built from, held exactly.
#[derive(Clone, Debug)]
struct Decimal {
    digits: Vec<u8>,
    point: i64,
}

impl Decimal {
    /// The exact value of `m × 2^e`, which is always a finite decimal.
    fn of(m: u64, e: i32) -> Decimal {
        assert!(m > 0);
        // Little-endian base-10^9 limbs of the integer `m × 2^e` or
        // `m × 5^-e`; the second is the first's digits with the point moved.
        let mut limbs: Vec<u64> = vec![m % 1_000_000_000, m / 1_000_000_000 % 1_000_000_000];
        limbs.push(m / 1_000_000_000_000_000_000);
        let (factor, times) = if e >= 0 { (2, e) } else { (5, -e) };
        for _ in 0..times {
            let mut carry = 0;
            for limb in limbs.iter_mut() {
                let product = *limb * factor + carry;
                *limb = product % 1_000_000_000;
                carry = product / 1_000_000_000;
            }
            if carry > 0 {
                limbs.push(carry);
            }
        }
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        let mut text = limbs.last().unwrap().to_string();
        for limb in limbs.iter().rev().skip(1) {
            text.push_str(&format!("{limb:09}"));
        }
        let whole = text.len() as i64;
        let mut digits: Vec<u8> = text.bytes().map(|b| b - b'0').collect();
        while digits.last() == Some(&0) {
            digits.pop();
        }
        let point = if e >= 0 { whole } else { whole + e as i64 };
        Decimal { digits, point }
    }

    /// The exact midpoint between `x` and the next `Float` up, which for the
    /// largest finite value is `2^1024`.
    fn midpoint_above(x: f64) -> Decimal {
        let (m, e) = decomposed(x);
        Decimal::of(2 * m + 1, e - 1)
    }

    /// The exact value of `x`, which is positive and finite.
    fn exactly(x: f64) -> Decimal {
        let (m, e) = decomposed(x);
        Decimal::of(m, e)
    }

    /// The same number written with exactly `n` significant digits: padded
    /// with zeros when it has fewer, which is still the same number, and cut
    /// when it has more, which is just below it.
    fn with_digits(&self, n: usize) -> Decimal {
        let mut digits = self.digits.clone();
        digits.resize(n, 0);
        Decimal {
            digits,
            point: self.point,
        }
    }

    /// One unit of the last digit more.
    fn up(&self) -> Decimal {
        let mut digits = self.digits.clone();
        let mut at = digits.len();
        loop {
            if at == 0 {
                digits.insert(0, 1);
                return Decimal {
                    digits,
                    point: self.point + 1,
                };
            }
            at -= 1;
            if digits[at] == 9 {
                digits[at] = 0;
            } else {
                digits[at] += 1;
                return Decimal {
                    digits,
                    point: self.point,
                };
            }
        }
    }

    /// One unit of the last digit less.
    fn down(&self) -> Decimal {
        let mut digits = self.digits.clone();
        let mut at = digits.len();
        loop {
            at -= 1;
            if digits[at] == 0 {
                digits[at] = 9;
            } else {
                digits[at] -= 1;
                break;
            }
        }
        let mut point = self.point;
        while digits.first() == Some(&0) {
            digits.remove(0);
            point -= 1;
        }
        // A one-digit `1` less one is nought, which is written as a zero.
        if digits.is_empty() {
            return Decimal {
                digits: vec![0],
                point: 1,
            };
        }
        Decimal { digits, point }
    }

    /// `d.ddd…e<exp>`.
    fn scientific(&self) -> String {
        let mut text = String::new();
        text.push((b'0' + self.digits[0]) as char);
        if self.digits.len() > 1 {
            text.push('.');
            text.extend(self.digits[1..].iter().map(|d| (b'0' + d) as char));
        }
        format!("{text}e{}", self.point - 1)
    }

    /// Every digit in its place, with no exponent: `0.000…ddd` or `ddd…000`.
    fn positional(&self) -> String {
        let digits: String = self.digits.iter().map(|d| (b'0' + d) as char).collect();
        let n = digits.len() as i64;
        if self.point <= 0 {
            format!("0.{}{digits}", "0".repeat((-self.point) as usize))
        } else if self.point >= n {
            format!("{digits}{}", "0".repeat((self.point - n) as usize))
        } else {
            let (whole, fraction) = digits.split_at(self.point as usize);
            format!("{whole}.{fraction}")
        }
    }
}

/// `x` as `m × 2^e` with `m` a whole number, `x` positive and finite.
fn decomposed(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent - 1075)
    }
}

// ------------------------------------------------------------ the inputs

/// The rows a reimplementation fails first.
fn adversarial() -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let least_normal = f64::MIN_POSITIVE;
    let largest_subnormal = f64::from_bits(least_normal.to_bits() - 1);
    let least_subnormal = f64::from_bits(1);
    // Floats whose midpoints are asked about at every width: normal ones in
    // the middle of the range and at both ends, subnormal ones at both ends of
    // theirs, and the neighbours of each boundary.
    let floats = [
        1.0,
        2.0,
        1.2345678901234567,
        0.1,
        3.0e-5,
        6.02214076e23,
        9007199254740992.0,
        1.0e300,
        1.0e-300,
        least_normal,
        f64::from_bits(least_normal.to_bits() + 1),
        largest_subnormal,
        f64::from_bits(largest_subnormal.to_bits() - 1),
        f64::from_bits(1 << 51),
        f64::from_bits(12345),
        least_subnormal,
        f64::from_bits(2),
        f64::MAX,
        f64::from_bits(f64::MAX.to_bits() - 1),
    ];
    for x in floats {
        let mid = Decimal::midpoint_above(x);
        let exact = Decimal::exactly(x);
        for value in [&mid, &exact] {
            rows.push(value.scientific());
            rows.push(value.positional());
            rows.push(value.up().scientific());
            rows.push(value.down().scientific());
        }
        for n in [17, 19, 20, 40, 767, 768, 769] {
            let at = mid.with_digits(n);
            rows.push(at.scientific());
            rows.push(at.up().scientific());
            rows.push(at.down().scientific());
        }
        // The midpoint, then `0…01` at a digit past the 768th: above the
        // midpoint by less than any digit the buffer holds.
        for n in [769, 770, 800, 1000] {
            let length = n.max(mid.digits.len() + 1);
            let mut above = mid.with_digits(length);
            *above.digits.last_mut().unwrap() = 1;
            rows.push(above.scientific());
            // And zeros to past the 768th, which is the midpoint still.
            rows.push(mid.with_digits(length).scientific());
        }
        rows.push(mid.with_digits(mid.digits.len() + 1).up().positional());
    }
    // The least subnormal and its half, as their shortest spellings, their
    // neighbours in the last digit, and their exact expansions.
    for text in [
        "2.4703282292062327e-324",
        "2.4703282292062328e-324",
        "2.4703282292062326e-324",
        "2.470328229206232720882e-324",
        "4.9406564584124654e-324",
        "4.9406564584124655e-324",
        "4.9406564584124653e-324",
        "7.4109846876186981e-324",
        "7.4109846876186982e-324",
        "2.2250738585072011e-308",
        "2.2250738585072012e-308",
        "2.2250738585072014e-308",
        "2.2250738585072009e-308",
        "1.7976931348623157e308",
        "1.7976931348623158e308",
        "1.7976931348623159e308",
    ] {
        rows.push(text.to_string());
        rows.push(format!("-{text}"));
    }
    // Clinger's box: `m` either side of `2^53`, `e` either side of `±22`.
    for m in [
        9007199254740991u64,
        9007199254740992,
        9007199254740993,
        9007199254740994,
        4503599627370497,
        123456789,
        1,
    ] {
        for e in [-23, -22, -21, 0, 21, 22, 23] {
            rows.push(format!("{m}e{e}"));
            rows.push(format!("-{m}.0e{e}"));
        }
    }
    // Exponents no `Int` holds, exponents at every width, and exponents a
    // long mantissa cancels.
    for text in [
        "1e99999999999999999999",
        "1e-99999999999999999999",
        "1e9223372036854775807",
        "1e9223372036854775808",
        "1e-9223372036854775808",
        "1e-9223372036854775809",
        "1e18446744073709551616",
        "1e100000000000000000",
        "1e-100000000000000000",
        "1e99999999999999999",
        "1e-99999999999999999",
        "0e999999999999999999999",
        "0e-999999999999999999999",
        "-0e999999999999999999999",
        "0.0e99999999999999999999999999999999999999",
        "1e308",
        "1e309",
        "1e-323",
        "1e-324",
        "1e-325",
        "1e+00000000000000000000000000000000000308",
        "1e-00000000000000000000000000000000000308",
        "1e65535",
        "1e65536",
        "1e-65536",
        "1e2147483647",
        "1e2147483648",
        "1e-2147483649",
    ] {
        rows.push(text.to_string());
    }
    for zeros in [300, 320, 400, 1000, 5000] {
        rows.push(format!("1{}e-{zeros}", "0".repeat(zeros)));
        rows.push(format!("0.{}1e{zeros}", "0".repeat(zeros)));
        rows.push(format!("0.{}1e{}", "0".repeat(zeros), zeros + 1));
        rows.push(format!("0.{}1e{}", "0".repeat(zeros), zeros - 300));
        rows.push(format!("1{}e-{}", "0".repeat(zeros), zeros + 300));
        rows.push(format!("{}1", "0".repeat(zeros)));
        rows.push(format!("1.{}", "0".repeat(zeros)));
        rows.push(format!("0.{}", "0".repeat(zeros)));
        rows.push(format!("-0.{}", "0".repeat(zeros)));
        rows.push(format!("{}.{}", "0".repeat(zeros), "0".repeat(zeros)));
        rows.push(format!("1.{}1", "0".repeat(zeros)));
        rows.push(format!("9{}", "9".repeat(zeros)));
    }
    // The zeros, every way a sign reaches one.
    for text in [
        "0",
        "-0",
        "+0",
        "0.0",
        "-0.0",
        "0.",
        "-0.",
        ".0",
        "-.0",
        "+.0",
        "0e0",
        "-0e10",
        "-0e-10",
        "-0.000",
        "00000",
        "-00000.00000e5",
        "-1e-400",
        "-1e-99999999999999999999",
        "-2.4703282292062327e-324",
        "-2.4703282292062328e-324",
    ] {
        rows.push(text.to_string());
    }
    // The three words, in every case for the short two and a spread of cases
    // for the long one, with every sign; and their near misses.
    for word in ["inf", "nan", "infinity"] {
        let n = word.len();
        let masks: Vec<u32> = if n <= 3 {
            (0..1 << n).collect()
        } else {
            vec![0, 1, 0b10101010, 0b01010101, (1 << n) - 1, 0b11110000]
        };
        for mask in masks {
            let cased: String = word
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if mask & (1 << i) != 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            for sign in ["", "+", "-"] {
                rows.push(format!("{sign}{cased}"));
            }
        }
    }
    for text in [
        "infinit",
        "infinityy",
        "infinit y",
        "na",
        "nann",
        "inff",
        "in",
        "i",
        "n",
        "nan.0",
        "inf.0",
        "nan ",
        " nan",
        "++inf",
        "-+nan",
        "infnan",
        "nanf",
        "1inf",
        "inf1",
        "infe5",
        "nane1",
        "INFINITYX",
        "-",
        "+",
        "",
        ".",
        "-.",
        "+.",
        "e",
        "E5",
        ".e1",
        "1e",
        "1e+",
        "1e-",
        "1e+-5",
        "1e--5",
        "1ee5",
        "1e5e5",
        "1e5.",
        "1.e",
        "1..0",
        "1.0.",
        "..1",
        "1e0x",
        "0x1p3",
        "0x10",
        "1_0",
        "1 0",
        " 1",
        "1 ",
        "\t1",
        "1\n",
        "1,0",
        "1d5",
        "1f",
        "1.0f",
        "1%",
        "--1",
        "+-1",
        "-+1",
        "1-",
        "1+",
        "½",
        "１",
        "1é",
        "\u{0}",
        "1\u{0}",
        "`",
        "1e1_0",
        "1.5E",
        "Infinity1",
        "0.5.5",
        "5e5e",
        "e",
        "-e5",
        "+e",
        ".",
        "0x",
        "1a",
    ] {
        rows.push(text.to_string());
    }
    rows
}

/// A small, fixed-seed generator, so that every run asks the same inputs.
struct Seeded(u64);

impl Seeded {
    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// `count` random inputs of each of the three random kinds, from `seed`.
fn random(seed: u64, count: usize) -> Vec<String> {
    let mut rng = Seeded(seed);
    let mut rows = Vec::new();
    // A random bit pattern, spelled three ways: `{:e}`, which is the shortest
    // round-trip in scientific notation; `Display`, which is the shortest
    // round-trip written out positionally, so a subnormal is 330 characters;
    // and 17 significant digits, which is not the shortest and lands off the
    // value's own spelling.
    let mut made = 0;
    while made < count {
        let x = f64::from_bits(rng.next());
        if !x.is_finite() {
            continue;
        }
        rows.push(format!("{x:e}"));
        rows.push(format!("{x}"));
        rows.push(format!("{x:.16e}"));
        made += 1;
    }
    // A random digit string: mostly short, sometimes as long as 800 digits,
    // with a point somewhere or nowhere and an exponent or none.
    for _ in 0..count {
        let length = match rng.below(4) {
            0 => 1 + rng.below(8),
            1 => 1 + rng.below(25),
            2 => 1 + rng.below(120),
            _ => 1 + rng.below(800),
        } as usize;
        let mut text = String::new();
        if rng.below(4) == 0 {
            text.push('-');
        }
        let point = rng.below(length as u64 + 2) as usize;
        for at in 0..length {
            if at == point {
                text.push('.');
            }
            text.push((b'0' + rng.below(10) as u8) as char);
        }
        match rng.below(4) {
            0 => {}
            1 => text.push_str(&format!("e{}", rng.below(700) as i64 - 350)),
            2 => text.push_str(&format!("e{}", rng.below(60) as i64 - 30)),
            _ => text.push_str(&format!("E{}", rng.below(2000) as i64 - 1000)),
        }
        rows.push(text);
    }
    // A random string over the grammar's alphabet and a few bytes outside it.
    let alphabet = b"0123456789.eE+-infatyINFATY_ x,";
    for _ in 0..count {
        let length = rng.below(12) as usize;
        let text: String = (0..length)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize] as char)
            .collect();
        rows.push(text);
    }
    rows
}

/// The rows ADR 0072's middle tier — Eisel and Lemire's truncated product,
/// in thirty-bit limbs over a table of `5^q` for `q` in `[-342, 308]` — fails
/// first, if it fails.
///
/// - **The table's edges**: mantissas of one to eighteen digits at the
///   first and last `q` the table holds, one past each (decided before the
///   table is read), and the `q`s where its entries stop being exact (38, 39)
///   and where its exact-division case stops (−26, −27); each also with a
///   nineteenth digit that is not nought and with three dropped zeros.
/// - **Ties and the ambiguity test**: the exact midpoints of floats between
///   `2^40` and `2^64`, whose decimal expansions are short enough for the
///   tier to hold — an integer, or a fraction whose `5^-q` divides the
///   mantissa — and those cut to 17, 18 and 19 digits, with one unit either
///   side in the last; the same cuts of the midpoints of floats across the
///   whole range, where a nineteenth digit is dropped and `m` and `m + 1`
///   are asked; and the named ties `1e23` and the 40-digit halfway.
/// - **Subnormals** the tier rounds itself, and the least normal value's
///   boundary at 17 to 20 digits.
fn middle_tier() -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mantissas: [u64; 9] = [
        1,
        2,
        5,
        9,
        17976931348623157,
        247032822920623272,
        494065645841246544,
        222507385850720138,
        999999999999999999,
    ];
    for q in [
        -345, -344, -343, -342, -341, -340, -326, -325, -324, -323, -310, -309, -308, -307, -28,
        -27, -26, -25, -1, 0, 1, 22, 23, 37, 38, 39, 40, 290, 291, 292, 306, 307, 308, 309, 310,
    ] {
        for m in mantissas {
            rows.push(format!("{m}e{q}"));
            rows.push(format!("{m}1e{}", q - 1));
            rows.push(format!("{m}000e{}", q - 3));
            rows.push(format!("-{m}9e{}", q - 1));
        }
    }
    let mut rng = Seeded(0x0e15_e11e_3f1e);
    // Floats whose midpoints are short: between `2^40` and `2^64`.
    for _ in 0..300 {
        let exponent = 40 + rng.below(24) as i32;
        let x = f64::from_bits(((1023 + exponent as u64) << 52) | (rng.next() >> 12));
        let mid = Decimal::midpoint_above(x);
        rows.push(mid.scientific());
        rows.push(mid.positional());
        for n in [17, 18, 19] {
            let at = mid.with_digits(n);
            rows.push(at.scientific());
            rows.push(at.up().scientific());
            rows.push(at.down().scientific());
        }
    }
    // Floats across the whole range, subnormals included, their midpoints
    // cut to 18 and 19 digits.
    for _ in 0..300 {
        let x = f64::from_bits(rng.next() & !(1 << 63));
        if !x.is_finite() || x == 0.0 {
            continue;
        }
        let mid = Decimal::midpoint_above(x);
        for n in [18, 19] {
            let at = mid.with_digits(n);
            rows.push(at.scientific());
            rows.push(at.up().scientific());
            rows.push(at.down().scientific());
        }
    }
    // Subnormals: random ones, spelled shortest and with 17 digits.
    for _ in 0..300 {
        let x = f64::from_bits(1 + rng.below((1 << 52) - 1));
        rows.push(format!("{x:e}"));
        rows.push(format!("{x:.16e}"));
    }
    for text in [
        "1e23",
        "-1e23",
        "9007199254740993",
        "9007199254740993.000000000000000000000000",
        "9007199254740993.000000000000000000000001",
        "9007199254740992.999999999999999999999999",
        "9007199254740993.00001",
        "9007199254740992.5",
        "9007199254740993.5",
        "11920928955078125",
        "59604644775390625",
        "298023223876953125",
        "1490116119384765625",
        "4.9406564584124654e-320",
        "5e-324",
        "1e-320",
        "2.4703282292062328e-324",
        "2.4703282292062327e-324",
        "7.4109846876186982e-324",
        "2.225073858507201e-308",
        "2.2250738585072009e-308",
        "2.2250738585072010e-308",
        "2.2250738585072011e-308",
        "2.2250738585072012e-308",
        "2.2250738585072013e-308",
        "2.2250738585072014e-308",
        "2.22507385850720113e-308",
        "2.22507385850720114e-308",
        "2.225073858507201136e-308",
        "2.225073858507201137e-308",
        "2.2250738585072011360574e-308",
        "2.22507385850720138e-308",
        "2.225073858507201383e-308",
        "1.7976931348623157e308",
        "1.79769313486231580e308",
        "1.797693134862315807e308",
        "1.797693134862315808e308",
    ] {
        rows.push(text.to_string());
    }
    rows
}

/// `count` random bit patterns, each spelled shortest (`{:e}`), with 17
/// digits and with 18, and as many random strings of 15 to 19 digits with a
/// point somewhere and an exponent anywhere from −350 to 310: the inputs the
/// middle tier exists for, across the whole range.
fn round_trips(seed: u64, count: usize) -> Vec<String> {
    let mut rng = Seeded(seed);
    let mut rows = Vec::new();
    let mut made = 0;
    while made < count {
        let x = f64::from_bits(rng.next());
        if !x.is_finite() {
            continue;
        }
        rows.push(format!("{x:e}"));
        rows.push(format!("{x:.16e}"));
        rows.push(format!("{x:.17e}"));
        made += 1;
    }
    for _ in 0..count {
        let length = 15 + rng.below(5) as usize;
        let point = rng.below(length as u64 + 1) as usize;
        let mut text = String::new();
        if rng.below(2) == 0 {
            text.push('-');
        }
        for at in 0..length {
            if at == point {
                text.push('.');
            }
            let digit = if at == 0 {
                1 + rng.below(9)
            } else {
                rng.below(10)
            };
            text.push((b'0' + digit as u8) as char);
        }
        text.push_str(&format!("e{}", rng.below(661) as i64 - 350));
        rows.push(text);
    }
    rows
}

// ------------------------------------------------------------------ cases

/// The adversarial rows, on the VM.
#[test]
fn every_adversarial_row_agrees_on_the_vm() {
    let inputs = adversarial();
    let answers = Subject::new().on_the_vm(&inputs);
    agree("VM", &inputs, &answers);
}

/// A few thousand seeded random inputs, on the VM.
#[test]
fn seeded_random_inputs_agree_on_the_vm() {
    let inputs = random(0x5eed_f10a7, 700);
    let answers = Subject::new().on_the_vm(&inputs);
    agree("VM", &inputs, &answers);
}

/// The adversarial rows, on the native tier.
#[cfg(feature = "template")]
#[test]
fn every_adversarial_row_agrees_on_the_native_tier() {
    let inputs = adversarial();
    let answers = Subject::new().on_the_native_tier(&inputs);
    agree("native tier", &inputs, &answers);
}

/// The same seeded random inputs, on the native tier.
#[cfg(feature = "template")]
#[test]
fn seeded_random_inputs_agree_on_the_native_tier() {
    let inputs = random(0x5eed_f10a7, 700);
    let answers = Subject::new().on_the_native_tier(&inputs);
    agree("native tier", &inputs, &answers);
}

/// The middle tier's rows, on the VM.
#[test]
fn every_middle_tier_row_agrees_on_the_vm() {
    let inputs = middle_tier();
    let answers = Subject::new().on_the_vm(&inputs);
    agree("VM", &inputs, &answers);
}

/// Seeded round-trips and 15- to 19-digit strings, on the VM.
#[test]
fn seeded_round_trips_agree_on_the_vm() {
    let inputs = round_trips(0x5eed_1e3f, 1_500);
    let answers = Subject::new().on_the_vm(&inputs);
    agree("VM", &inputs, &answers);
}

/// The middle tier's rows, on the native tier.
#[cfg(feature = "template")]
#[test]
fn every_middle_tier_row_agrees_on_the_native_tier() {
    let inputs = middle_tier();
    let answers = Subject::new().on_the_native_tier(&inputs);
    agree("native tier", &inputs, &answers);
}

/// The same round-trips, on the native tier.
#[cfg(feature = "template")]
#[test]
fn seeded_round_trips_agree_on_the_native_tier() {
    let inputs = round_trips(0x5eed_1e3f, 1_500);
    let answers = Subject::new().on_the_native_tier(&inputs);
    agree("native tier", &inputs, &answers);
}

/// Five seeds of the wide sweep: five thousand inputs of each random kind,
/// twenty-five thousand in all.
fn sweep(seeds: std::ops::RangeInclusive<u64>) {
    let subject = Subject::new();
    for seed in seeds {
        let inputs = random(seed.wrapping_mul(0x0123_4567_89ab_cdef), 1_000);
        let answers = subject.on_the_vm(&inputs);
        agree("VM", &inputs, &answers);
    }
}

// A hundred thousand more, from twenty other seeds, on the VM: `cargo
// ratchet`. Four cases rather than one so that the harness runs them side by
// side; one case was 321 seconds on its own.

/// The wide sweep's first quarter.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_random_sweep_agrees_1() {
    sweep(1..=5);
}

/// The wide sweep's second quarter.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_random_sweep_agrees_2() {
    sweep(6..=10);
}

/// The wide sweep's third quarter.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_random_sweep_agrees_3() {
    sweep(11..=15);
}

/// The wide sweep's last quarter.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_random_sweep_agrees_4() {
    sweep(16..=20);
}

/// Five seeds of the middle tier's sweep: 2,500 bit patterns in three
/// spellings and 2,500 strings of 15 to 19 digits each, fifty thousand in
/// all.
fn round_trip_sweep(seeds: std::ops::RangeInclusive<u64>) {
    let subject = Subject::new();
    for seed in seeds {
        let inputs = round_trips(seed.wrapping_mul(0x0fed_cba9_8765_4321), 2_500);
        let answers = subject.on_the_vm(&inputs);
        agree("VM", &inputs, &answers);
    }
}

// A hundred thousand more for the middle tier, from ten seeds, on the VM:
// `cargo ratchet`, in two cases for the same reason as the four above.

/// The middle tier's sweep, first half.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_round_trip_sweep_agrees_1() {
    round_trip_sweep(1..=5);
}

/// The middle tier's sweep, second half.
#[test]
#[ignore = "a wide sweep, run by `cargo ratchet`"]
fn a_wide_round_trip_sweep_agrees_2() {
    round_trip_sweep(6..=10);
}
