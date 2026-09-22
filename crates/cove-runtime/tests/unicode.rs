//! Cove's Unicode version, held exhaustively to the one the toolchain ships.
//!
//! [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 5 is that **Cove owns its Unicode version**. Issue #454's Step 5
//! moved `String.trim` and `String.words` into `std.string`, and with them the
//! two character sets those operations are about: Unicode's `White_Space`,
//! which is twenty-five code points, and ASCII whitespace, which is five
//! bytes. Both are written out in `crates/cove-sema/std/string.cove`, as UTF-8
//! byte patterns with each code point named.
//!
//! Before that they were `str::trim` and `split_ascii_whitespace`, so the sets
//! were **whatever the Rust toolchain was built against** and nothing in this
//! repository recorded which that was. A toolchain bump that moved a code
//! point would have changed what every Cove program meant, silently and with
//! no diff.
//!
//! This file is the other half of Decision 5, and it is the reproducible
//! generator in miniature. It sweeps **every code point from 0 to `0x10FFFF`**
//! — an exhaustive comparison and not a sample — and asserts that the set the
//! Cove bodies implement is exactly the set the toolchain's `char` answers. So
//! a future toolchain whose table differs is a *failing test naming the code
//! point*, which is what a version being owned rather than inherited means in
//! practice. When it fails, the thing to change is the Cove source and the
//! version recorded beside it; the assertion message prints the toolchain's
//! own `char::UNICODE_VERSION` so that the new version is in the failure.
//!
//! `String.toUpper` and `String.toLower` finished Step 5 with the generated
//! asset Decision 5 describes — 10,969 bytes in six `std.string` string
//! literals, which `crates/cove-sema/tests/unicase.rs` regenerates and holds
//! byte-identical — and the two tests at the bottom of this file are the same
//! sweep for them. There are two because one is not enough: `toLower` has a
//! rule that depends on a character's **neighbours**, and no comparison that
//! asks about one code point at a time can reach it.
//!
//! # Why the sweep runs a Cove program
//!
//! The sets are a decision tree over bytes in a Cove body, not a list a Rust
//! test could read. So the only way to ask the shipped implementation what it
//! thinks is to run it, which is what the program below does: one
//! `String.fromCodePoint` per code point, `trim()` on it and `words()` on it
//! between two letters, and a line for every code point either one reacted to.
//! Surrogates are skipped because `fromCodePoint` refuses them and they encode
//! no character.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::{Budget, Grants, HostRegistry, Limits, Runtime, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::Compiler;

/// How many runs a sweep of the whole code point space is cut into.
///
/// **A sweep does not fit in one run, and raising the heap is the wrong
/// answer.** Each of these programs allocates a `String` per code point and
/// several more per row — 7.8 million allocations over 1,112,064 turns for the
/// whitespace one — and the collector runs when the heap is full, so a sweep's
/// high-water mark is whatever heap it was given. The first sweep here
/// measured 4,194,303 words of `Vm::new`'s 4,194,304, one word spare, and the
/// case sweeps are heavier again: the sigma one lowercases four strings a code
/// point and did not fit in **sixteen times** the default heap.
///
/// So the range is cut instead. A chunk is about 35,000 code points, its live
/// set is a few hundred items, and every chunk runs on a fresh `Vm::new` with
/// the ordinary heap — which is both faster and a better test, because a
/// sweep that only passes on a heap nothing else uses is measuring the heap.
///
/// The program is compiled and lowered **once** and entered once per chunk,
/// so the cost of the cut is 32 entries and not 32 compilations.
const CHUNKS: i64 = 32;

/// The last code point.
const LAST: i64 = 0x10FFFF;

/// Every code point, asked of both operations.
///
/// `kinds` is a bitmap rather than two lists so that the program answers one
/// string: 1 is "`trim` removed it", 2 is "`words` split on it", and a code
/// point neither reacted to is not reported at all. So the answer is about
/// thirty items long however wide the sweep is.
const SWEEP: &str = r#"
export fn main(from: Int, to: Int) -> Result<String, Error> {
  var out: Vector<String> = Vector.of()
  var point = from
  while point <= to {
    if point < 55296 || point > 57343 {
      let one = String.fromCodePoint(point)?
      var kinds = 0
      if one.trim().isEmpty() {
        kinds = kinds + 1
      }
      if "a{one}b".words().length() == 2 {
        kinds = kinds + 2
      }
      if kinds != 0 {
        out.push("{point}:{kinds}")
      }
    }
    point = point + 1
  }
  Ok(" ".join(out.freeze()))
}
"#;

#[test]
fn the_two_whitespace_sets_are_the_toolchains_over_the_whole_of_unicode() {
    let answered = run(SWEEP);
    let (trimmed, split) = sets(&answered);

    let mut rust_trimmed = BTreeSet::new();
    let mut rust_split = BTreeSet::new();
    for point in 0u32..=0x10FFFF {
        let Some(character) = char::from_u32(point) else {
            continue;
        };
        if character.is_whitespace() {
            rust_trimmed.insert(point);
        }
        if character.is_ascii_whitespace() {
            rust_split.insert(point);
        }
    }

    let version = char::UNICODE_VERSION;
    assert_eq!(
        trimmed, rust_trimmed,
        "`std.string.trim` removes a different set from the one this toolchain's \
         `char::is_whitespace` answers. The toolchain is Unicode {}.{}.{}; \
         `crates/cove-sema/std/string.cove` records the version its table is drawn from, \
         and both it and `whiteSpaceWidthAt` have to move together.",
        version.0, version.1, version.2,
    );
    assert_eq!(
        split, rust_split,
        "`std.string.words` splits on a different set of bytes from the one this \
         toolchain's `char::is_ascii_whitespace` answers. `separates` in \
         `crates/cove-sema/std/string.cove` is the list.",
    );

    // What the two sets *are*, asserted here as well, so that a change which
    // moved both this file's expectation and the Cove source at once still has
    // to move a written-down number.
    assert_eq!(
        trimmed.len(),
        25,
        "`White_Space` is twenty-five code points"
    );
    assert_eq!(split.len(), 5, "ASCII whitespace is five bytes");
    assert!(
        trimmed.contains(&0x0B) && !split.contains(&0x0B),
        "`U+000B` is `White_Space` and is not one of the five ASCII separators"
    );
    assert!(
        !trimmed.contains(&0x200B) && !trimmed.contains(&0xFEFF),
        "`U+200B` and `U+FEFF` are not `White_Space`"
    );
}

/// The two sets the program reported, out of its `point:kinds` items.
fn sets(answered: &str) -> (BTreeSet<u32>, BTreeSet<u32>) {
    let mut trimmed = BTreeSet::new();
    let mut split = BTreeSet::new();
    for item in answered.split_whitespace() {
        let (point, kinds) = item
            .split_once(':')
            .unwrap_or_else(|| panic!("`{item}` is not a `point:kinds` item"));
        let point: u32 = point.parse().expect("a code point");
        let kinds: u32 = kinds.parse().expect("a bitmap");
        if kinds & 1 != 0 {
            trimmed.insert(point);
        }
        if kinds & 2 != 0 {
            split.insert(point);
        }
    }
    (trimmed, split)
}

/// A sweep's answer, as the text it built, over the whole code point space.
///
/// `Repr` is private — `tests/representation_is_private.rs` is what keeps it
/// so — and a `Value`'s `Display` is the interface a test outside this crate
/// has. What it renders is `Ok(<the text>)`, unquoted, and the text is digits,
/// colons and spaces, so stripping the wrapper is the whole of the parsing.
fn run(source: &str) -> String {
    let (sources, checked) = check(source);
    let lowered = Arc::new(
        cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new())
            .expect("the sweep lowers"),
    );
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_budget(Budget::new(Limits::default()));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let mut answered = Vec::new();
    let width = LAST / CHUNKS + 1;
    let mut from = 0;
    while from <= LAST {
        let to = (from + width - 1).min(LAST);
        // `invoke` rather than `run_entry`, which takes the strings a command
        // line would have carried: the bounds are two `Int`s and parsing them
        // in Cove would put `Int.parse` inside the sweep.
        let answer: Value = Vm::new(&runtime, &hosts, &lowered)
            .invoke("m", "main", vec![Value::int(from), Value::int(to)])
            .unwrap_or_else(|error| panic!("the sweep runs over {from}..={to}: {}", error.message));
        let rendered = format!("{answer}");
        let inside = rendered
            .strip_prefix("Ok(")
            .and_then(|held| held.strip_suffix(')'))
            .unwrap_or_else(|| panic!("the sweep answers `Ok(...)`, and this is `{rendered}`"));
        if !inside.is_empty() {
            answered.push(inside.to_string());
        }
        from = to + 1;
    }
    answered.join(" ")
}

/// Parses and checks `source` as the one module `m`, with the standard library
/// attached — which is the point, since what is under test is in it.
fn check(source: &str) -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = cove_syntax::parse_file(&sources, file).expect("the sweep parses");
    let mut modules = BTreeMap::from([(
        "m".to_string(),
        Module {
            name: "m".to_string(),
            dir: PathBuf::from("m"),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::from("."),
        config: Default::default(),
        modules,
    };
    let checked = Compiler::new().compile(&package).expect("the sweep checks");
    (Arc::new(sources), Arc::new(checked))
}

/// Every code point, both case mappings, against the toolchain's own.
///
/// One `String.fromCodePoint` per code point, `toUpper()` and `toLower()` on
/// it, and a line for every code point either one changed. The answer is
/// written out as **bytes** rather than as characters, because a case mapping
/// may answer more than one character — 102 uppercase ones do, and one
/// lowercase one does — and because bytes are what `byteAt` hands back without
/// a decode to argue about.
///
/// Reported by change rather than in full, for [`SWEEP`]'s reason: the answer
/// is then about three thousand items long however wide the sweep is.
const CASE_SWEEP: &str = r#"
export fn main(from: Int, to: Int) -> Result<String, Error> {
  var out: Vector<String> = Vector.of()
  var point = from
  while point <= to {
    if point < 55296 || point > 57343 {
      let one = String.fromCodePoint(point)?
      let up = one.toUpper()
      if up != one {
        out.push("{point}u{bytesOf(up)}")
      }
      let down = one.toLower()
      if down != one {
        out.push("{point}l{bytesOf(down)}")
      }
    }
    point = point + 1
  }
  Ok(" ".join(out.freeze()))
}

/// The bytes of `text`, separated by commas.
fn bytesOf(text: String) -> String {
  var out = ""
  var at = 0
  let end = text.byteLength()
  while at < end {
    if at == 0 {
      out = "{text.byteAt(at)}"
    } else {
      out = "{out},{text.byteAt(at)}"
    }
    at = at + 1
  }
  out
}
"#;

#[test]
fn the_two_case_mappings_are_the_toolchains_over_the_whole_of_unicode() {
    let answered = run(CASE_SWEEP);
    let (upper, lower) = mappings(&answered);

    let mut rust_upper: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut rust_lower: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    for point in 0u32..=0x10FFFF {
        let Some(character) = char::from_u32(point) else {
            continue;
        };
        let here = character.to_string();
        let up: String = character.to_uppercase().collect();
        if up != here {
            rust_upper.insert(point, up.into_bytes());
        }
        let down: String = character.to_lowercase().collect();
        if down != here {
            rust_lower.insert(point, down.into_bytes());
        }
    }

    let version = char::UNICODE_VERSION;
    assert_eq!(
        upper, rust_upper,
        "`std.string.toUpper` answers a different mapping from this toolchain's \
         `char::to_uppercase`. The toolchain is Unicode {}.{}.{}; the six tables are \
         generated by `crates/cove-sema/tests/unicase.rs`, and \
         `COVE_WRITE_CASE_TABLES=1 cargo test -p cove-sema --profile checked \
         --test unicase -- --ignored regenerates` rewrites them.",
        version.0, version.1, version.2,
    );
    assert_eq!(
        lower, rust_lower,
        "`std.string.toLower` answers a different mapping from this toolchain's \
         `char::to_lowercase`. See `crates/cove-sema/tests/unicase.rs`.",
    );

    // What the two tables *are*, so that a change which moved both the Cove
    // source and this file at once still has to move a written-down number.
    assert_eq!(
        upper.len(),
        1580,
        "1,580 code points uppercase to something else"
    );
    assert_eq!(
        lower.len(),
        1488,
        "1,488 code points lowercase to something else"
    );
    // How many characters a UTF-8 run holds, which is not a function of how
    // many bytes it holds: `ﬀ` uppercases to `FF`, two characters and two
    // bytes, and `ΰ` to three characters and six. A byte is the start of a
    // character exactly when it is not a continuation byte, `10xxxxxx`.
    let characters = |bytes: &Vec<u8>| bytes.iter().filter(|b| *b & 0xC0 != 0x80).count();
    assert_eq!(
        upper.values().filter(|bytes| characters(bytes) > 1).count(),
        102,
        "102 uppercase mappings answer more than one character"
    );
    assert_eq!(
        upper
            .values()
            .filter(|bytes| characters(bytes) == 3)
            .count(),
        16,
        "sixteen of them answer three"
    );
    assert_eq!(
        upper.get(&0xDF).map(Vec::as_slice),
        Some(b"SS".as_slice()),
        "`U+00DF` uppercases to `SS`, which is the row that says the answer may be longer"
    );
    assert_eq!(
        lower.get(&0x130).map(Vec::as_slice),
        Some("i\u{0307}".as_bytes()),
        "`U+0130` lowercases to `i` and a combining dot above, and is the only code point \
         whose lowercase mapping is more than one character"
    );
    assert_eq!(
        lower.values().filter(|bytes| characters(bytes) > 1).count(),
        1,
        "there is exactly one such code point"
    );
}

/// **The final sigma, which a sweep of single characters cannot reach.**
///
/// `Σ` lowercases to `ς` at the end of a word and to `σ` elsewhere, so what
/// one character answers depends on the characters beside it. This puts
/// **every code point** next to a sigma in four arrangements and holds all
/// four to `str::to_lowercase`, which is the rule's only other implementation
/// here.
///
/// It is the test for the mistake the corpus was written to name. 268 code
/// points are both `Cased` and `Case_Ignorable`; the scan skips ignorable
/// characters before it asks about cased ones, so each of those is *skipped*;
/// and an implementation that asked the other way round is right on `Α ʰ Σ`
/// and wrong on `Α ʰ Σ a`. Both arrangements are below, for all 1,112,064 code
/// points rather than for the 268.
///
/// The answer is a bitmap per code point, so it is one item a code point
/// however the four come out, and a code point that moved one arrangement and
/// not the others is named by the number that changed. What each arrangement
/// reads is the **last byte of the lowered sigma**: `U+03C2` is `CF 82` and
/// `U+03C3` is `CF 83`, so one byte separates them, and the sigma's position
/// is known in each arrangement without a decode — last in the first two, and
/// bytes 2 and 3 in the last two, because `Α` lowercases to two bytes.
const SIGMA_SWEEP: &str = r#"
export fn main(from: Int, to: Int) -> Result<String, Error> {
  var out: Vector<String> = Vector.of()
  var point = from
  while point <= to {
    if point < 55296 || point > 57343 {
      let one = String.fromCodePoint(point)?
      var kinds = 0
      if finalAtEnd("{one}Σ") {
        kinds = kinds + 1
      }
      if finalAtEnd("Α{one}Σ") {
        kinds = kinds + 2
      }
      if finalAtThree("ΑΣ{one}") {
        kinds = kinds + 4
      }
      if finalAtThree("ΑΣ{one}Α") {
        kinds = kinds + 8
      }
      // 12 is what a character that is neither cased nor case-ignorable
      // answers: it stops both scans, so the sigma is final when something
      // cased is on the *other* side of it and not otherwise. Every CJK
      // ideograph, every digit and every punctuation mark answers 12, so
      // reporting the exceptions is what keeps this list about seven
      // thousand items long instead of 1,112,064 — which is not a nicety,
      // because building the longer one exhausts the heap.
      if kinds != 12 {
        out.push("{point}:{kinds}")
      }
    }
    point = point + 1
  }
  Ok(" ".join(out.freeze()))
}

/// Whether the last two bytes of `text` lowercased are `U+03C2`.
fn finalAtEnd(text: String) -> Bool {
  let lowered = text.toLower()
  lowered.byteAt(lowered.byteLength() - 1) == 130
}

/// Whether bytes 2 and 3 of `text` lowercased are `U+03C2`.
fn finalAtThree(text: String) -> Bool {
  text.toLower().byteAt(3) == 130
}
"#;

/// # Why this one is `#[ignore]`d
///
/// It is **103 seconds**, against eight for the whitespace sweep and fifteen
/// for the case one, because it lowercases four strings for every code point
/// rather than one: 4,448,256 calls, each with a scan in each direction from
/// the sigma. That is more than `cargo t` costs in total today, and CI runs
/// the ignored cases in a step of its own — `cargo ratchet` — which is where a
/// check this exhaustive belongs. The two sweeps above stay in `cargo t`.
#[test]
#[ignore = "103s: four lowercasings per code point"]
fn the_final_sigma_is_the_toolchains_beside_every_code_point() {
    let answered = run(SIGMA_SWEEP);
    let mut here = BTreeMap::new();
    for item in answered.split_whitespace() {
        let (point, kinds) = item
            .split_once(':')
            .unwrap_or_else(|| panic!("`{item}` is not a `point:kinds` item"));
        here.insert(
            point.parse::<u32>().expect("a code point"),
            kinds.parse::<u32>().expect("a bitmap"),
        );
    }

    let mut rust = BTreeMap::new();
    for point in 0u32..=0x10FFFF {
        let Some(character) = char::from_u32(point) else {
            continue;
        };
        let mut kinds = 0;
        let last = |text: String| text.as_bytes()[text.len() - 1] == 130;
        let third = |text: String| text.as_bytes()[3] == 130;
        if last(format!("{character}\u{03A3}").to_lowercase()) {
            kinds += 1;
        }
        if last(format!("\u{0391}{character}\u{03A3}").to_lowercase()) {
            kinds += 2;
        }
        if third(format!("\u{0391}\u{03A3}{character}").to_lowercase()) {
            kinds += 4;
        }
        if third(format!("\u{0391}\u{03A3}{character}\u{0391}").to_lowercase()) {
            kinds += 8;
        }
        if kinds != 12 {
            rust.insert(point, kinds);
        }
    }

    assert_eq!(
        here, rust,
        "`std.string.sigmaIsFinal` decides `Final_Sigma` differently from this toolchain's \
         `str::to_lowercase`. The two sets it is made of are `casedRanges` and \
         `ignorableRanges` in `crates/cove-sema/std/string.cove`, and \
         `crates/cove-sema/tests/unicase.rs` generates both.",
    );

    // The row that dates the rule, and the reason the second table exists.
    // `U+02B0` MODIFIER LETTER SMALL H is `Other_Lowercase` and so cased, and
    // `Lm` and so case-ignorable. Arrangement 2 is `Α ʰ Σ` — final, because
    // the `ʰ` is skipped and the `Α` behind it is what answers — and
    // arrangement 8 is `Α Σ ʰ Α`, not final, because the `ʰ` is skipped the
    // other way and the `Α` after it is cased.
    assert_eq!(
        here.get(&0x02B0),
        Some(&(2 + 4)),
        "`ʰ` is both cased and case-ignorable, and the scan skips before it asks"
    );
    // A plain cased character, for contrast: arrangement 1 is final because
    // `X` is cased, and 4 is not because the sigma is followed by one.
    assert_eq!(
        here.get(&0x0041),
        Some(&(1 + 2)),
        "`A` is cased and not ignorable: it satisfies both Before scans and breaks both \
         After ones"
    );
    assert_eq!(
        here.get(&0x0301),
        Some(&(2 + 4)),
        "a combining acute is ignorable and not cased, so it is stepped over in both \
         directions — which makes arrangement 8 the one it is *not* final in, because \
         the scan steps over it and reaches the `Α` behind"
    );
    assert_eq!(
        here.get(&0x0030),
        None,
        "a digit is neither cased nor ignorable, so it answers the default 12"
    );
    assert_eq!(
        here.len(),
        7158,
        "`Cased` and `Case_Ignorable` are 7,158 code points between them, and they are \
         exactly the ones that answer something other than 12"
    );
}

/// The two mappings the case sweep reported, out of its `point<u|l><bytes>`
/// items.
fn mappings(answered: &str) -> (BTreeMap<u32, Vec<u8>>, BTreeMap<u32, Vec<u8>>) {
    let mut upper = BTreeMap::new();
    let mut lower = BTreeMap::new();
    for item in answered.split_whitespace() {
        let at = item
            .find(['u', 'l'])
            .unwrap_or_else(|| panic!("`{item}` names no direction"));
        let point: u32 = item[..at].parse().expect("a code point");
        let bytes: Vec<u8> = item[at + 1..]
            .split(',')
            .map(|one| one.parse().expect("a byte"))
            .collect();
        match &item[at..=at] {
            "u" => upper.insert(point, bytes),
            _ => lower.insert(point, bytes),
        };
    }
    (upper, lower)
}
