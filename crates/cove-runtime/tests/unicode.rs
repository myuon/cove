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
//! `String.toUpper` and `String.toLower` are the half of Step 5 that this
//! cannot be written for: their tables are the full Unicode case mappings,
//! including the one-to-many ones, and a set of twenty-five code points can be
//! written directly where those need the generated, checked-in asset Decision 5
//! describes.
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

/// Every code point, asked of both operations.
///
/// `kinds` is a bitmap rather than two lists so that the program answers one
/// string: 1 is "`trim` removed it", 2 is "`words` split on it", and a code
/// point neither reacted to is not reported at all. So the answer is about
/// thirty items long however wide the sweep is.
const SWEEP: &str = r#"
export fn main() -> Result<String, Error> {
  var out: Vector<String> = Vector.of()
  var point = 0
  while point <= 1114111 {
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

/// [`SWEEP`]'s answer, as the text it built.
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
    let answer: Value = Vm::new(&runtime, &hosts, &lowered)
        .run_entry("m", "main", Vec::new())
        .expect("the sweep runs");
    let rendered = format!("{answer}");
    let inside = rendered
        .strip_prefix("Ok(")
        .and_then(|held| held.strip_suffix(')'))
        .unwrap_or_else(|| panic!("the sweep answers `Ok(...)`, and this is `{rendered}`"));
    inside.to_string()
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
