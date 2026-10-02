//! What `foresee` guesses about a slice, and that the guess never changes the
//! program a slice is.
//!
//! The corpus is where the second half is shown for real programs — every
//! lowering of every program in the repository was compared, before and
//! after — and these cases hold it in the two places a corpus may not visit:
//! a guess that is too wide, and a guess that has to be made again from what
//! a round found missing.

use std::collections::HashSet;

use cove_schema::HostSchemas;

use super::super::{foresee, sliced, Guess, Plan};
use super::checked;

/// A program whose bodies call the standard library in every way `foresee`
/// reads: a vector's `push` and `length`, a string's `length`, an associated
/// `Duration.millis`, a `Set.of`, an interpolation of each kind of piece, and
/// an `assertEqual`.
const SOURCE: &str = "
struct Point { x: Int, y: Int }

fn total(text: String) -> Int {
  var xs = Vector.of(1, 2)
  xs.push(text.length())
  let pause = Duration.millis(5)
  let seen = Set.of(1, 2)
  let p = Point(x: 1, y: 2)
  let line = \"{xs.length()} {text} {p} {1.5} {pause} {seen.contains(1)}\"
  line.length()
}

test fn totals() -> Result<Unit, Error> {
  assertEqual(total(\"ab\"), 2)
}
";

/// The program a slice lowers to, as everything it holds.
fn lowered(guess: Guess<'_>, entry: &str) -> String {
    let (sources, checked) = checked(SOURCE);
    match sliced(
        &checked,
        &sources,
        &HostSchemas::new(),
        &[("m", entry)],
        guess,
    ) {
        Ok(program) => format!("{program:?}"),
        Err(items) => panic!("the slice lowers: {items:?}"),
    }
}

/// The guess finds the calls a body makes without naming them, which the
/// call graph's seed does not hold.
#[test]
fn the_guess_finds_the_library_calls_no_call_site_names() {
    let (_, checked) = checked(SOURCE);
    let plan = Plan::index(&checked);
    let seed = plan.reachable_from(&checked, &[("m", "total")]);
    let found = foresee::library_calls(&checked, &plan, &seed, &seed, false);
    let named: HashSet<String> = found
        .iter()
        .map(|id| {
            let decl = &plan.decls[id.index()];
            format!("{}.{}", decl.module, decl.name)
        })
        .collect();
    for wanted in [
        "std.vector.push",
        "std.vector.length",
        "std.string.length",
        "std.duration.ofMillis",
        "std.set.of",
        "std.stringbuilder.appendText",
        "std.stringbuilder.appendByteInto",
        "std.int.renderInto",
        "std.float.renderInto",
        "std.duration.renderInto",
    ] {
        assert!(named.contains(wanted), "{wanted} is not in {named:?}");
    }
    assert!(found.iter().all(|id| !seed.contains(id)));
}

/// A guess changes how many rounds a slice takes and nothing else.
#[test]
fn a_slice_is_the_same_program_with_the_guess_and_without_it() {
    for entry in ["total", "totals"] {
        assert_eq!(
            lowered(Guess::Foresee, entry),
            lowered(Guess::Nothing, entry),
            "{entry}"
        );
    }
}

/// A guess that names a declaration nothing calls is dropped once a round
/// wants nothing more, and the slice is the one the rounds reach alone —
/// rather than one holding a body nobody calls, its strings and its layouts.
#[test]
fn a_guess_too_wide_is_dropped() {
    let wide = [("std.string", "split"), ("std.float", "parse")];
    for entry in ["total", "totals"] {
        assert_eq!(
            lowered(Guess::Also(&wide), entry),
            lowered(Guess::Nothing, entry),
            "{entry}"
        );
    }
}
