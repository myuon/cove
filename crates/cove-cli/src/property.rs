//! Properties: a `test fn` with parameters, run over arguments `cove test`
//! generates from the parameters' types ([ADR 0121]).
//!
//! Everything here is the runner's and none of it is the language's. The
//! checker has already decided which parameter types a property may take
//! ([`Ty::is_generatable`]); this module generates values of those types,
//! runs the property on each case, and, when one fails, shrinks the failing
//! arguments to a smaller case that still fails.
//!
//! # Deterministic by construction
//!
//! A property's cases are a function of its seed and nothing else: the seed
//! is a hash of the test's qualified name unless `cove test --seed` names one,
//! the generator is SplitMix64, and every choice below is integer arithmetic.
//! So a suite that passed passes again, on every machine, and a failure's
//! report names the seed that reproduces it.
//!
//! # What is generated
//!
//! Generation is biased toward the values that break code, which are not the
//! values a uniform draw finds: an `Int` is an edge (0, ±1, the extremes,
//! ±2^53, …) a quarter of the time, a `String` mixes ASCII with the
//! characters `std` escapes and with multi-byte and astral code points. A
//! size grows from 1 to [`MAX_SIZE`] across a run, as QuickCheck's does, and
//! bounds every length, so the first cases are the small ones.
//!
//! # Shrinking
//!
//! Greedy, on the generated tree rather than on runtime values: try each
//! simpler candidate of each argument in turn, keep the first that still
//! fails, and start again from it, until no candidate fails or
//! [`MAX_SHRINK_RUNS`] runs have been spent. An integer moves toward 0; a
//! string or array loses chunks, then single elements, and then has each
//! element simplified — a character toward `a`.
//!
//! [ADR 0121]: ../../../docs/adr/0121-a-test-fn-may-take-parameters.md
//! [`Ty::is_generatable`]: cove_sema::typeck::Ty::is_generatable

use std::fmt::Write as _;
use std::rc::Rc;

use cove_runtime::Value;
use cove_sema::typeck::Ty;

/// How many cases a property runs when `cove.toml` does not say.
pub(crate) const DEFAULT_CASES: u32 = 100;

/// The size the last case of a run is generated at.
const MAX_SIZE: u64 = 100;

/// The most runs one shrink may spend before it reports what it has.
const MAX_SHRINK_RUNS: usize = 2000;

/// What `cove test` can generate: the runner's reading of a generatable
/// [`Ty`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Int,
    Bool,
    Str,
    Array(Box<Shape>),
}

impl Shape {
    /// The shape of `ty`, or `None` for a type the runner cannot generate —
    /// which the checker has already refused, so a `None` here means the
    /// two disagree.
    pub(crate) fn of(ty: &Ty) -> Option<Shape> {
        match ty {
            Ty::Int => Some(Shape::Int),
            Ty::Bool => Some(Shape::Bool),
            Ty::Str => Some(Shape::Str),
            Ty::Array(element) => Some(Shape::Array(Box::new(Shape::of(element)?))),
            _ => None,
        }
    }
}

/// One generated argument, as the shrinker sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Arg {
    Int(i64),
    Bool(bool),
    Str(String),
    Array(Vec<Arg>),
}

impl Arg {
    /// The runtime value this argument is passed as.
    pub(crate) fn value(&self) -> Value {
        match self {
            Arg::Int(n) => Value::int(*n),
            Arg::Bool(b) => Value::bool(*b),
            Arg::Str(text) => Value::string(text.as_str()),
            Arg::Array(items) => Value::array(items.iter().map(Arg::value)),
        }
    }

    /// This argument written as the Cove literal that denotes it.
    pub(crate) fn literal(&self) -> String {
        match self {
            Arg::Int(n) => n.to_string(),
            Arg::Bool(b) => b.to_string(),
            Arg::Str(text) => string_literal(text),
            Arg::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(Arg::literal)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// `text` as a one-line Cove string literal: quoted, with `{` and `}`
/// escaped so nothing reads as interpolation, and every control character
/// written as an escape so the literal survives being printed.
fn string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            c if c.is_control() || matches!(c, '\u{feff}' | '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{{{:X}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The seed a property runs under when `--seed` names none: FNV-1a over its
/// qualified name, which is stable across runs, builds and machines where
/// the standard library's hasher is deliberately none of those.
pub(crate) fn seed_for(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// SplitMix64: small, fast, and the same answer everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..bound`; `bound` is not zero.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// The integers code most often gets wrong.
const EDGE_INTS: &[i64] = &[
    0,
    1,
    -1,
    2,
    -2,
    10,
    -10,
    127,
    128,
    -128,
    255,
    256,
    i32::MAX as i64,
    i32::MIN as i64,
    1 << 32,
    (1 << 53) - 1,
    1 << 53,
    (1 << 53) + 1,
    -(1 << 53),
    -(1 << 53) - 1,
    i64::MAX - 1,
    i64::MAX,
    i64::MIN + 1,
    i64::MIN,
];

/// The ASCII characters the standard library treats specially — escapes,
/// separators, the characters of URL, HTML and JSON syntax — and the
/// controls.
const SPECIAL_CHARS: &[char] = &[
    '%', '&', '<', '>', '"', '\'', '\\', '+', '=', ' ', '\n', '\t', '\r', '\0', '\u{1}', '\u{1f}',
    '\u{7f}', '{', '}', '#', '/', '?', ':', ';', '.', '-', '_', '~', '[', ']', ',',
];

/// Multi-byte characters inside the Basic Multilingual Plane, chosen at the
/// encoding's edges and from among the ones text code mistakes.
const WIDE_CHARS: &[char] = &[
    '\u{80}', '\u{a0}', 'é', 'ß', 'ñ', '\u{7ff}', '\u{800}', '€', 'あ', '中', '\u{2028}',
    '\u{feff}', '\u{fffd}', '\u{ffff}',
];

/// Characters outside the Basic Multilingual Plane, which take four bytes of
/// UTF-8 and two UTF-16 units.
const ASTRAL_CHARS: &[char] = &['\u{10000}', '😀', '👍', '𝄞', '\u{10ffff}'];

/// The size case `case` of `cases` is generated at: 1 for the first, growing
/// evenly to [`MAX_SIZE`] for the last.
fn size_at(case: u32, cases: u32) -> u64 {
    let last = u64::from(cases.saturating_sub(1)).max(1);
    1 + u64::from(case) * (MAX_SIZE - 1) / last
}

fn generate(shape: &Shape, rng: &mut Rng, size: u64) -> Arg {
    match shape {
        Shape::Int => Arg::Int(int(rng, size)),
        Shape::Bool => Arg::Bool(rng.below(2) == 1),
        Shape::Str => {
            let length = length(rng, size);
            Arg::Str((0..length).map(|_| character(rng)).collect())
        }
        Shape::Array(element) => {
            let length = length(rng, size);
            // An element is generated at half the size, so a nested array
            // does not square the run's lengths.
            let inner = (size / 2).max(1);
            Arg::Array((0..length).map(|_| generate(element, rng, inner)).collect())
        }
    }
}

fn int(rng: &mut Rng, size: u64) -> i64 {
    match rng.below(4) {
        0 => rng.pick(EDGE_INTS),
        1 => rng.below(2 * size + 1) as i64 - size as i64,
        2 => {
            // A magnitude of every bit width equally often, so the middle of
            // the range is reached and not only its ends.
            let bits = 1 + rng.below(62);
            let magnitude = (rng.next() & ((1u64 << bits) - 1)) as i64;
            if rng.below(2) == 0 {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => rng.next() as i64,
    }
}

/// A length, mostly well under `size` and now and then several times it.
fn length(rng: &mut Rng, size: u64) -> u64 {
    if rng.below(10) == 0 {
        rng.below(4 * size + 1)
    } else {
        let bound = rng.below(size + 1);
        rng.below(bound + 1)
    }
}

fn character(rng: &mut Rng) -> char {
    match rng.below(20) {
        0..=8 => char::from(0x20 + rng.below(95) as u8),
        9..=13 => rng.pick(SPECIAL_CHARS),
        14..=15 => rng.pick(WIDE_CHARS),
        16 => loop {
            // Anywhere in the plane past ASCII, skipping the surrogates,
            // which are not characters.
            if let Some(c) = char::from_u32(0x80 + rng.below(0xFFFF - 0x80) as u32) {
                break c;
            }
        },
        17..=18 => rng.pick(ASTRAL_CHARS),
        _ => char::from_u32(0x1_0000 + rng.below(0x10_0000) as u32).unwrap_or('\u{10000}'),
    }
}

/// How a property's run came out.
#[derive(Debug)]
pub(crate) enum Verdict<F> {
    /// Every case passed.
    Passed { cases: u32 },
    /// A case failed; `args` is the smallest failing case the shrinker found
    /// and `failure` what running it reported.
    Failed {
        /// How many cases passed before the first failing one.
        passed: u32,
        /// How many times the shrinker found a smaller failing case.
        shrinks: usize,
        args: Vec<Arg>,
        failure: F,
    },
}

/// Runs a property over `cases` cases generated from `shapes` under `seed`,
/// and shrinks the first failing case.
///
/// `run` runs the property on one case and answers its failure, or `None`
/// when it held; it is called once per case and once per shrink candidate.
pub(crate) fn check<F>(
    shapes: &[Shape],
    cases: u32,
    seed: u64,
    mut run: impl FnMut(&[Arg]) -> Option<F>,
) -> Verdict<F> {
    let mut rng = Rng(seed);
    for case in 0..cases {
        let size = size_at(case, cases);
        let args: Vec<Arg> = shapes
            .iter()
            .map(|shape| generate(shape, &mut rng, size))
            .collect();
        if let Some(failure) = run(&args) {
            let (args, failure, shrinks) = shrink(args, failure, &mut run);
            return Verdict::Failed {
                passed: case,
                shrinks,
                args,
                failure,
            };
        }
    }
    Verdict::Passed { cases }
}

/// Greedily shrinks a failing case, answering the smallest failing case
/// found, what it reported, and how many smaller cases were accepted.
fn shrink<F>(
    mut args: Vec<Arg>,
    mut failure: F,
    run: &mut impl FnMut(&[Arg]) -> Option<F>,
) -> (Vec<Arg>, F, usize) {
    let mut runs = 0;
    let mut accepted = 0;
    'smaller: loop {
        for position in 0..args.len() {
            for candidate in shrinks(&args[position]) {
                if runs == MAX_SHRINK_RUNS {
                    break 'smaller;
                }
                runs += 1;
                let mut trial = args.clone();
                trial[position] = candidate;
                if let Some(smaller) = run(&trial) {
                    args = trial;
                    failure = smaller;
                    accepted += 1;
                    continue 'smaller;
                }
            }
        }
        break;
    }
    (args, failure, accepted)
}

/// Every simpler argument than `arg`, simplest first, produced lazily: an
/// array of strings has more candidates than are worth building at once,
/// and the shrinker usually stops at one of the first.
fn shrinks(arg: &Arg) -> Box<dyn Iterator<Item = Arg>> {
    match arg {
        Arg::Int(n) => Box::new(int_shrinks(*n).into_iter().map(Arg::Int)),
        Arg::Bool(true) => Box::new(std::iter::once(Arg::Bool(false))),
        Arg::Bool(false) => Box::new(std::iter::empty()),
        Arg::Str(text) => {
            let chars: Rc<Vec<char>> = Rc::new(text.chars().collect());
            let removed =
                removals(Rc::clone(&chars)).map(|kept| Arg::Str(kept.into_iter().collect()));
            let simplified = (0..chars.len()).flat_map(move |at| {
                let chars = Rc::clone(&chars);
                simpler_chars(chars[at]).into_iter().map(move |simpler| {
                    let mut text: Vec<char> = (*chars).clone();
                    text[at] = simpler;
                    Arg::Str(text.into_iter().collect())
                })
            });
            Box::new(removed.chain(simplified))
        }
        Arg::Array(items) => {
            let items = Rc::new(items.clone());
            let removed = removals(Rc::clone(&items)).map(Arg::Array);
            let simplified = (0..items.len()).flat_map(move |at| {
                let items = Rc::clone(&items);
                shrinks(&items[at]).map(move |simpler| {
                    let mut items: Vec<Arg> = (*items).clone();
                    items[at] = simpler;
                    Arg::Array(items)
                })
            });
            Box::new(removed.chain(simplified))
        }
    }
}

/// Simpler integers than `n`, as QuickCheck orders them: 0, then `-n` for a
/// negative `n`, then `n` less ever smaller halves of itself, which ends at
/// the neighbour of `n` toward 0.
fn int_shrinks(n: i64) -> Vec<i64> {
    let mut out = Vec::new();
    if n == 0 {
        return out;
    }
    out.push(0);
    if n < 0 && n != i64::MIN {
        out.push(-n);
    }
    let whole = i128::from(n);
    let mut step = whole / 2;
    while step != 0 {
        // Between `n` and 0, so it fits.
        let candidate = (whole - step) as i64;
        if !out.contains(&candidate) {
            out.push(candidate);
        }
        step /= 2;
    }
    out
}

/// `items` with a chunk removed: the whole, then halves, then quarters, down
/// to single elements, each at every position it fits.
fn removals<T: Clone + 'static>(items: Rc<Vec<T>>) -> impl Iterator<Item = Vec<T>> {
    let len = items.len();
    let sizes = std::iter::successors((len > 0).then_some(len), |size| {
        (*size > 1).then_some(size / 2)
    });
    sizes.flat_map(move |size| {
        let items = Rc::clone(&items);
        (0..len).step_by(size).map(move |start| {
            let end = (start + size).min(len);
            let mut kept = Vec::with_capacity(len - (end - start));
            kept.extend_from_slice(&items[..start]);
            kept.extend_from_slice(&items[end..]);
            kept
        })
    })
}

/// Simpler characters than `c`: `a`, and for a character past ASCII, `é`,
/// which keeps a failure that needs a multi-byte character at one familiar
/// two-byte character.
fn simpler_chars(c: char) -> Vec<char> {
    let mut out = Vec::new();
    if c != 'a' {
        out.push('a');
    }
    if !c.is_ascii() && c != 'é' {
        out.push('é');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_strings(property: impl Fn(&str) -> bool) -> Verdict<()> {
        check(
            &[Shape::Str],
            DEFAULT_CASES,
            seed_for("unit.strings"),
            |args| {
                let Arg::Str(text) = &args[0] else {
                    panic!("a string property is handed a string")
                };
                (!property(text)).then_some(())
            },
        )
    }

    fn generated(shape: &Shape, seed: u64) -> Vec<Arg> {
        let mut rng = Rng(seed);
        (0..DEFAULT_CASES)
            .map(|case| generate(shape, &mut rng, size_at(case, DEFAULT_CASES)))
            .collect()
    }

    #[test]
    fn generation_is_a_function_of_the_seed() {
        let shape = Shape::Array(Box::new(Shape::Str));
        assert_eq!(generated(&shape, 7), generated(&shape, 7));
        assert_ne!(generated(&shape, 7), generated(&shape, 8));
    }

    #[test]
    fn the_seed_is_a_stable_hash_of_the_name() {
        // FNV-1a's published value for the empty input and for "a".
        assert_eq!(seed_for(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(seed_for("a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(seed_for("unit.a"), seed_for("unit.b"));
    }

    #[test]
    fn the_size_grows_from_one_to_the_maximum() {
        assert_eq!(size_at(0, 100), 1);
        assert_eq!(size_at(99, 100), MAX_SIZE);
        assert_eq!(size_at(0, 1), 1);
        assert!(size_at(50, 100) > size_at(10, 100));
    }

    #[test]
    fn integers_reach_the_edges_and_strings_reach_every_width() {
        // A suite of runs rather than one: there are two dozen edges, and a
        // quarter of a hundred draws does not reach every one of them.
        let ints: Vec<Arg> = (0..20)
            .flat_map(|seed| generated(&Shape::Int, seed))
            .collect();
        for edge in [0, 1, -1, 1 << 53, i64::MIN, i64::MAX] {
            assert!(ints.contains(&Arg::Int(edge)), "{edge} never generated");
        }
        let strings: String = generated(&Shape::Str, 1)
            .into_iter()
            .map(|arg| match arg {
                Arg::Str(text) => text,
                _ => unreachable!(),
            })
            .collect();
        assert!(strings.contains('%'));
        assert!(strings.chars().any(|c| c.len_utf8() == 2));
        assert!(strings.chars().any(|c| c.len_utf8() == 3));
        assert!(strings.chars().any(|c| c.len_utf8() == 4));
    }

    #[test]
    fn an_integer_shrinks_to_the_boundary_of_what_fails() {
        let verdict = check(&[Shape::Int], DEFAULT_CASES, 3, |args| {
            let Arg::Int(n) = args[0] else { unreachable!() };
            (n > 100).then_some(())
        });
        let Verdict::Failed { args, .. } = verdict else {
            panic!("an Int over 100 is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Int(101)]);
    }

    #[test]
    fn a_negative_integer_shrinks_too() {
        let verdict = check(&[Shape::Int], DEFAULT_CASES, 3, |args| {
            let Arg::Int(n) = args[0] else { unreachable!() };
            (n < -100).then_some(())
        });
        let Verdict::Failed { args, .. } = verdict else {
            panic!("an Int under -100 is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Int(-101)]);
    }

    #[test]
    fn a_string_containing_a_percent_sign_shrinks_to_the_sign() {
        let Verdict::Failed { args, .. } = run_strings(|text| !text.contains('%')) else {
            panic!("a '%' is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Str("%".to_string())]);
    }

    #[test]
    fn a_string_shrinks_its_other_characters_toward_a() {
        let Verdict::Failed { args, .. } = run_strings(|text| text.chars().count() < 3) else {
            panic!("a three-character string is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Str("aaa".to_string())]);
    }

    #[test]
    fn a_non_ascii_failure_keeps_one_multi_byte_character() {
        let Verdict::Failed { args, .. } = run_strings(|text| text.is_ascii()) else {
            panic!("a non-ASCII string is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Str("é".to_string())]);
    }

    #[test]
    fn an_array_shrinks_by_elements_and_then_within_them() {
        let shape = Shape::Array(Box::new(Shape::Int));
        let verdict = check(&[shape], DEFAULT_CASES, 5, |args| {
            let Arg::Array(items) = &args[0] else {
                unreachable!()
            };
            let sum: i128 = items
                .iter()
                .map(|item| match item {
                    Arg::Int(n) => i128::from(*n),
                    _ => unreachable!(),
                })
                .sum();
            (sum > 10).then_some(())
        });
        let Verdict::Failed { args, .. } = verdict else {
            panic!("an array summing past 10 is generated within 100 cases")
        };
        assert_eq!(args, [Arg::Array(vec![Arg::Int(11)])]);
    }

    #[test]
    fn a_passing_property_reports_its_case_count() {
        let Verdict::Passed { cases } = run_strings(|_| true) else {
            panic!("a property that always holds passes")
        };
        assert_eq!(cases, DEFAULT_CASES);
    }

    #[test]
    fn the_first_case_that_fails_is_counted_and_shrunk_once_per_step() {
        let mut seen = 0;
        let verdict = check(&[Shape::Bool], DEFAULT_CASES, 9, |args| {
            seen += 1;
            (args[0] == Arg::Bool(true)).then_some(())
        });
        let Verdict::Failed {
            passed,
            shrinks,
            args,
            ..
        } = verdict
        else {
            panic!("`true` is generated within 100 cases")
        };
        // `true` shrinks only to `false`, which passes.
        assert_eq!(args, [Arg::Bool(true)]);
        assert_eq!(shrinks, 0);
        assert_eq!(seen as u32, passed + 2);
    }

    #[test]
    fn a_literal_reads_back_as_the_value() {
        assert_eq!(Arg::Int(-3).literal(), "-3");
        assert_eq!(Arg::Bool(true).literal(), "true");
        assert_eq!(
            Arg::Str("a%\"\\{x}\n\u{1}é😀".to_string()).literal(),
            "\"a%\\\"\\\\\\{x\\}\\n\\u{1}é😀\""
        );
        assert_eq!(
            Arg::Array(vec![Arg::Int(1), Arg::Int(-3)]).literal(),
            "[1, -3]"
        );
        assert_eq!(Arg::Array(Vec::new()).literal(), "[]");
    }
}
