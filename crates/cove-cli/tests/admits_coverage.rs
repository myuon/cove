//! What `crates/cove-runtime/src/frame.rs` refuses is the roadmap for what
//! to build into it next, and a roadmap nobody keeps current is not one.
//! Before the eight-byte frame could make a Host call, a survey was run by
//! hand over every entry point the repository has: 148 discovered, 116 of
//! them judged, 9 accepted, 107 refused — and a Host call stood in the
//! reachable graph of 94 of those 107, with nine programs blocked by nothing
//! else. That is what decided a Host call was the next family to build, and
//! it was right: all nine programs it alone blocked are accepted now. The
//! survey itself was then deleted, which was the mistake this file undoes.
//! Without it, the next choice of what to build goes back to guessing from a
//! plan instead of measuring from the corpus, and nothing catches a change
//! that quietly narrows what the frame admits.
//!
//! # Discovery
//!
//! Every `[run.<name>]` table this repository keeps is one entry point:
//! `tests/e2e/cove.toml`'s, `examples/cove.toml`'s, `benches/cove.toml`'s,
//! and — because `tests/e2e/` keeps a number of cases as packages of their
//! own, each with a `cove.toml` beside it — every nested package's too.
//! [`support::nested_packages`] is what finds the last kind, exactly as
//! `differential.rs` and `tests/e2e.rs` already rely on it to. `benches/` is
//! discovered here and is not in `differential.rs`: that harness excludes it
//! because running a benchmark's two million turns on two backends,
//! unoptimized, is expensive, and nothing here ever executes a program at
//! all, so a benchmark costs this test no more than any other entry does.
//!
//! [`support::cases_of`] and [`support::Prepared`] are `differential.rs`'s
//! own loading machinery, factored out to `tests/support/mod.rs` so that
//! there is one description of "parse and check the modules a case's entry
//! reaches" rather than a second one written here to do the same thing by a
//! slightly different route.
//!
//! # What is asked of each entry, and what is not "refused by the frame"
//!
//! Each entry is checked, lowered, and then handed to
//! `cove_runtime::frame::admits`, which is the question this whole file
//! exists to keep answered honestly. Three outcomes stop before that
//! question is even asked, and none of them is a refusal the frame made:
//!
//! - a package `tests/e2e` keeps on purpose to pin a check-time diagnostic
//!   does not check at all, so there is no checked program for anything
//!   downstream to look at ([`support::Unprepared::DoesNotCheck`]);
//! - an entry whose `module.name` names no module, or no function within
//!   one, that its package actually declares has nothing to lower either,
//!   which is a fact about the entry rather than about either backend
//!   ([`support::Unprepared::EntryNotResolved`]);
//! - `cove_ir::lower::lower_entry` itself refuses a construct the checker
//!   accepts but the IR has no instruction for, which is a gap in the
//!   lowering and not in the frame that runs what the lowering emits.
//!
//! Only an entry that clears all three and is then turned away by
//! `cove_runtime::frame::admits` is counted as the frame's own refusal.
//!
//! # Every refusal, not only the first
//!
//! `admits` stops at the first construct it cannot run, because that is the
//! only answer a caller deciding whether to execute a program needs. A
//! survey asking what to build next wants more: the first refusal often sits
//! in front of a whole family of functions the entry would otherwise reach,
//! and everything behind it never gets a chance to say what it, too, would
//! be refused for — which understates every family but the one blocking the
//! most programs *first*. `cove_runtime::frame::refusals` answers that by
//! walking the same reachable set with a sink that accumulates instead of
//! stopping — `frame.rs`'s own private `Sink` trait, which is what lets
//! `admits` and `refusals` share the one `match` that decides what the
//! frame refuses, rather than each holding a copy that could drift from the
//! other. This file prints both histograms, because the gap between them —
//! how much larger "reachable somewhere" is than "reached first" — is
//! itself part of what the corpus says.
//!
//! # Reading the report
//!
//! ```console
//! $ cargo test -p cove-cli --test admits_coverage -- --ignored --nocapture
//! ```
//!
//! The report is printed on every run and repeated in the message of a
//! failing assertion, so a failing run carries its own evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

#[path = "support/mod.rs"]
mod support;
use support::{Case, ModuleIndex, Prepared, Unprepared};

/// How many entries `cove_runtime::frame::admits` accepted the last time
/// this number was raised.
///
/// A floor, not a target: an entry the frame learns to run raises it, and
/// nothing may lower it, because an entry that stopped being admitted is
/// coverage lost silently. Raise this number in the same change that raises
/// what the frame admits.
///
/// 9 is where it stood on the day this file was written, matching the
/// hand-run survey that decided a Host call was the next family to build —
/// `host_console`, `host_console_streams`, `fail_divide_by_zero`,
/// `fail_int_overflow`, `fail_no_capability`, `fail_http_no_capability`,
/// `fail_database_connect_denied`, `fn_recursion` and `backend_vm` — before
/// that Host call existed. It was raised to 18 in the same change that added
/// this file, once the Host call itself was built and the same nine entries
/// it alone had blocked joined the nine that already ran, and to 19 once a
/// value slot could say it holds a `String`: `tests/e2e:flow_bindings` was
/// the one program every one of whose reachable refusals was "string
/// interpolation over a heap object this backend cannot show is a
/// `String`", so refining `cove_ir::SlotKind::Value` with
/// `cove_ir::ValueKind` admitted it outright. See this test's own printed
/// report for the current accepted list.
const ACCEPTED_FLOOR: usize = 19;

#[test]
#[ignore = "compiles and lowers the whole corpus, which is slow; CLAUDE.md's \
            local test command leaves the ignored cases out, and \
            `.github/workflows/ci.yml` runs them with \
            `cargo test --workspace --lib --tests -- --ignored`"]
fn every_entry_point_is_surveyed_for_what_the_frame_admits() {
    let report = survey();
    let text = report.render();
    print!("{text}");

    assert!(
        report.accepted.len() >= ACCEPTED_FLOOR,
        "the frame admitted {} entr{}, which is below the floor of \
         {ACCEPTED_FLOOR}; admitting more may raise the floor, admitting \
         fewer must not\n\n{text}",
        report.accepted.len(),
        if report.accepted.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
}

// ------------------------------------------------------------- the survey

/// Every entry point of the repository, checked, lowered, and asked of
/// `cove_runtime::frame::admits`.
fn survey() -> Report {
    let mut report = Report::default();
    let cases = discover();
    assert!(!cases.is_empty(), "no entry points were discovered");
    report.discovered = cases.len();

    // One index per package rather than one per case, exactly as
    // `differential.rs` keeps it: what a module reaches does not change
    // between two cases of the same package.
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();

    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));

        let prepared = match Prepared::of(&case, index) {
            Ok(prepared) => prepared,
            Err(Unprepared::DoesNotCheck) => {
                report.does_not_check.push(case.name);
                continue;
            }
            Err(Unprepared::EntryNotResolved) => {
                report.entry_not_resolved.push(case.name);
                continue;
            }
        };

        let (module, entry) = prepared.entry();
        let lowered = match cove_ir::lower::lower_entry(&prepared.checked, module, entry) {
            Ok(lowered) => lowered,
            Err(unsupported) => {
                report
                    .lower_refused
                    .push((case.name.clone(), unsupported.what));
                continue;
            }
        };

        match cove_runtime::frame::admits(&lowered.program, module, entry) {
            Ok(_) => report.accepted.push(case.name),
            Err(first) => {
                report.first_refusal.push((case.name.clone(), first.what));
                for refused in cove_runtime::frame::refusals(&lowered.program, module, entry) {
                    report.every_refusal.push((case.name.clone(), refused.what));
                }
            }
        }
    }
    report
}

/// Every entry point of the repository, in a fixed order.
///
/// The corpora are `tests/e2e/`, `examples/` and `benches/`, and an entry is
/// a `[run.<name>]` table of any `cove.toml` inside them — including the
/// ones an own-package `tests/e2e` case brings, which are packages of their
/// own exactly as `tests/e2e.rs` and `differential.rs` treat them. See this
/// file's own module docs for why `benches/` is counted here where
/// `differential.rs` leaves it out.
fn discover() -> Vec<Case> {
    let root = support::repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(support::nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));
    roots.push(root.join("benches"));

    roots
        .iter()
        .flat_map(|package| support::cases_of(&root, package))
        .collect()
}

// ------------------------------------------------------------- the report

/// What the survey found.
#[derive(Default)]
struct Report {
    discovered: usize,
    /// A package that deliberately does not check, so it has no program to
    /// lower or to ask the frame about.
    does_not_check: Vec<String>,
    /// An entry whose module or function this package does not declare.
    entry_not_resolved: Vec<String>,
    /// Each entry `cove_ir::lower` itself refused, and what it named.
    lower_refused: Vec<(String, String)>,
    /// Each entry `frame::admits` accepted.
    accepted: Vec<String>,
    /// Each refused entry's *first* refusal — `frame::admits`'s own answer,
    /// one row per entry.
    first_refusal: Vec<(String, String)>,
    /// Every refusal any function a refused entry's entry point can reach
    /// would raise — `frame::refusals`'s answer, a superset of
    /// `first_refusal` that may hold several rows per entry.
    every_refusal: Vec<(String, String)>,
}

impl Report {
    fn render(&self) -> String {
        let judged = self.accepted.len() + self.first_refusal.len();
        let mut out = format!(
            "\nadmits coverage over {} entry point(s):\n  \
             {:>3} accepted by the frame\n  \
             {:>3} refused by the frame\n  \
             {:>3} judged in total\n  \
             {:>3} refused by the lowering, before the frame was asked\n  \
             {:>3} do not check, so there is no program to lower\n  \
             {:>3} whose entry names no module or function this package \
             declares\n",
            self.discovered,
            self.accepted.len(),
            self.first_refusal.len(),
            judged,
            self.lower_refused.len(),
            self.does_not_check.len(),
            self.entry_not_resolved.len(),
        );

        if !self.accepted.is_empty() {
            out.push_str("\naccepted, by name:\n");
            for name in &self.accepted {
                let _ = writeln!(out, "       {name}");
            }
        }

        if !self.lower_refused.is_empty() {
            out.push_str("\nrefused by the lowering, before the frame was asked:\n");
            for (name, what) in &self.lower_refused {
                let _ = writeln!(out, "       {name}: {what}");
            }
        }

        render_histogram(
            &mut out,
            "what the frame refuses first, most common first:",
            &self.first_refusal,
        );
        render_histogram(
            &mut out,
            "what the frame refuses anywhere reachable, most common first \
             (a program counts once per reason, however many functions of \
             its own raise it):",
            &self.every_refusal,
        );

        let sole = sole_blockers(&self.every_refusal);
        if !sole.is_empty() {
            out.push_str(
                "\nfamilies that would fully admit a program if built next, most \
                 first (every one of that program's reachable refusals is this \
                 one reason):\n",
            );
            for (reason, cases) in &sole {
                let _ = writeln!(out, "  {:>3}  {reason}", cases.len());
                let _ = writeln!(out, "       first at {}", cases[0]);
            }
        }

        out
    }
}

/// `pairs`, folded to one row per [`normalized`] reason and ranked by how
/// many distinct entries raised it, most first.
fn render_histogram(out: &mut String, heading: &str, pairs: &[(String, String)]) {
    let ranked = ranked_by_reason(pairs);
    if ranked.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{heading}");
    for (reason, cases) in &ranked {
        let _ = writeln!(out, "  {:>3}  {reason}", cases.len());
        let _ = writeln!(out, "       first at {}", cases[0]);
    }
}

/// `pairs`, grouped by [`normalized`] reason into the distinct entry names
/// that raised it, ranked most-entries-first and then alphabetically.
fn ranked_by_reason(pairs: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut by_reason: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (case, what) in pairs {
        by_reason
            .entry(normalized(what))
            .or_default()
            .insert(case.clone());
    }
    let mut ranked: Vec<(String, Vec<String>)> = by_reason
        .into_iter()
        .map(|(reason, cases)| (reason, cases.into_iter().collect()))
        .collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    ranked
}

/// Every reason that is the *only* one its entry's reachable refusals ever
/// name, ranked by how many entries it alone would clear.
///
/// This is the answer "which family would admit the most programs if built
/// next": a program with more than one distinct reason reachable would not
/// run even after the family that stopped it first is built, because
/// something else downstream still would. Only a program whose whole
/// reachable set is one reason is cleared by building that one family.
fn sole_blockers(every_refusal: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut reasons_of: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (case, what) in every_refusal {
        reasons_of
            .entry(case.as_str())
            .or_default()
            .insert(normalized(what));
    }
    let mut by_reason: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (case, reasons) in &reasons_of {
        if let Ok(reason) = single(reasons) {
            by_reason
                .entry(reason)
                .or_default()
                .insert(case.to_string());
        }
    }
    let mut ranked: Vec<(String, Vec<String>)> = by_reason
        .into_iter()
        .map(|(reason, cases)| (reason, cases.into_iter().collect()))
        .collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    ranked
}

/// The one member of a single-element set, or `Err` for any other size.
fn single(set: &BTreeSet<String>) -> Result<String, ()> {
    let mut iter = set.iter();
    match (iter.next(), iter.next()) {
        (Some(only), None) => Ok(only.clone()),
        _ => Err(()),
    }
}

/// `what`, with every backtick-quoted function name blanked to a
/// placeholder, so that the same reason raised in two different functions
/// becomes one row of a histogram rather than two.
///
/// A refusal's message embeds the function it was raised in as
/// `` `module.name` `` — `frame::admits`'s own private `named()` closure —
/// wherever it names one, and nothing else a message holds looks like it: a
/// struct name, a field name and an instruction's own word are never a
/// dotted chain of identifiers. Spotting that shape rather than a fixed
/// position in the message is what makes this correct for every template at
/// once, including `Inst::Concat`'s, whose function name comes *before* a
/// second backtick-quoted word rather than after it — a position-based rule
/// would have to special-case that one and could drift from the others.
fn normalized(what: &str) -> String {
    let mut out = String::with_capacity(what.len());
    let mut rest = what;
    while let Some(start) = rest.find('`') {
        let (head, from_tick) = rest.split_at(start);
        let after_open = &from_tick[1..];
        let Some(end) = after_open.find('`') else {
            out.push_str(rest);
            return out;
        };
        let (inside, after_close) = after_open.split_at(end);
        out.push_str(head);
        if is_dotted_identifier(inside) {
            out.push_str("`_`");
        } else {
            out.push('`');
            out.push_str(inside);
            out.push('`');
        }
        // `after_close` still has its own closing backtick at the front.
        rest = &after_close[1..];
    }
    out.push_str(rest);
    out
}

/// Whether `text` is a dotted chain of identifiers — `module.function`,
/// `a.b.main` — which is the one shape `named()` produces and nothing else
/// in a refusal's message does: a field access is written `.name`, with
/// nothing before its dot, and a bare word like `Duration` or `?` has no dot
/// at all.
fn is_dotted_identifier(text: &str) -> bool {
    text.contains('.')
        && text.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|first| first.is_alphabetic() || first == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}
