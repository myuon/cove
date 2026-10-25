//! What a differential session compared ([issue 704]).
//!
//! A differential target that ran for twenty minutes and found nothing says
//! as much as the programs it compared, and the fuzzer's own statistics —
//! executions, coverage edges — do not say which those were. A program the
//! front end refused, or whose every mode stopped on the deadline before it
//! printed anything, is an execution that compared nothing. This module counts
//! what was compared, by one function from an input's [`Report`] to a
//! [`Classification`], over two paths:
//!
//! - **in the session** ([`record`]): `differential` and `differential_source`
//!   classify every input before they panic on it, and add it to a
//!   process-wide aggregate. When `COVE_FUZZ_STATS_DIR` is set, the aggregate
//!   is written to `$COVE_FUZZ_STATS_DIR/<target>-<pid>.json` after the first
//!   input, every [`FLUSH_EVERY`] inputs, at least every [`FLUSH_AFTER`], and
//!   before an input that is about to panic — atomically, by a rename. It is
//!   written as it goes because libFuzzer does not promise to run anything at
//!   exit, and kills a `-fork` child. Unset, nothing is aggregated and no file
//!   is touched. `cove-fuzz stats merge` adds the files together.
//! - **over a corpus** ([`replay`]): `cove-fuzz stats <target> DIR...` runs
//!   every input again and counts it, a finding included, without stopping.
//!
//! The in-session count is of **executions**: libFuzzer runs its corpus at
//! the start and each `-fork` child runs part of it again, so an input is
//! counted every time it runs. The replay counts each file once.
//!
//! # The metrics
//!
//! Every counter is a count of inputs unless it says otherwise. Categories
//! overlap and are kept apart; nothing here is a score.
//!
//! - `inputs`, and `targets.<name>`: inputs counted, per target.
//! - `stages`: how far through the front end the inputs got, each stage
//!   counting the inputs that passed it — `utf8` (the bytes are text; every
//!   `differential` input is), `parsed`, `checked`, `lowered`, `verified`
//!   (`cove_ir::verify` accepts the lowered program), `entry` (it has
//!   `playground.main`: the source targets' form of "the arguments were
//!   accepted", since `main` takes none), and `ran` (every mode was run and
//!   compared). A program that does not verify is run anyway, as it always
//!   was, and every lowered mode refuses it alike; so `ran` may exceed
//!   `verified`. For `differential` a refusal at any stage is itself a finding.
//! - `mismatches`: inputs that were findings — two modes disagreed, the image
//!   did not read back, or (for `differential`) the front end refused a
//!   generated program. Nought in a session that passed. `crashes`: inputs
//!   the replay caught panicking anywhere else, which a session reports as a
//!   crash and does not get to count.
//! - `comparisons`: of the programs that ran, every pair of modes is compared
//!   either **in full** or **by prefix**, as `disagreement` decides — by
//!   prefix when either of the pair stopped in a way that belongs to its mode
//!   (the deadline, a heap of a different size running out, a call-depth
//!   limit). `full` counts programs whose every pair was compared in full;
//!   `prefix` those with at least one pair compared by prefix only; and
//!   `no_entry` those with no `main`, which every mode refuses alike before
//!   its first instruction and which are neither (their pairs are not
//!   counted). The three partition `ran`. `full_with_native` and `full_with_gc_while_compiled` (and
//!   the `prefix_` pair) are the programs of each that also count under the
//!   `programs` metrics of the same name.
//! - **empty prefix**: a pair compared by prefix checks only that the shorter
//!   of each console stream is a prefix of the longer, so the bytes it
//!   actually compared are the shorter stdout and the shorter stderr. A pair's
//!   **common prefix is empty** when both of those are empty: it compared no
//!   output at all. `pairs_prefix` and `pairs_empty_prefix` count such pairs
//!   (`pairs_full` the others); `empty_prefix` counts the `prefix` programs
//!   whose every prefix-compared pair had an empty common prefix — a program
//!   whose stops left nothing to compare between the modes that stopped and
//!   the rest.
//! - `programs.native_ran`: the `native` mode entered compiled code
//!   (`native_calls > 0`). `programs.vm_gc_collected` and
//!   `programs.native_gc_collected`: that mode collected at least once.
//!   `programs.gc_while_compiled`: some mode collected **while a compiled frame
//!   was on the stack** (`OwnedVm::collections_while_compiled`), which is what
//!   tests a compiled frame's roots — a run that collected and ran compiled
//!   code at different moments did not. `collections_while_compiled` is the
//!   number of such collections, summed.
//! - `modes.<mode>`: per mode, `ran`; `ended.<outcome>` for runs that did not
//!   stop, by `RunOutcome` name (`success`, `error` for an entry returning
//!   `Err`, `invariant` for a trap, …); `stopped.<reason>` for runs that
//!   stopped, by the harness's reason; `skipped.<reason>` for a mode that did
//!   not run; and `entered_compiled_code`, `collected`,
//!   `collected_while_compiled`.
//!
//! The JSON is a tree of objects whose leaves are non-negative integers, and
//! two of them merge by adding leaf to leaf. The IR targets (`ir`,
//! `ir_mutate`, `image`) are not counted yet; they can be, as new keys under
//! the same tree, without changing what is here.
//!
//! [issue 704]: https://github.com/myuon/cove/issues/704

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::differential::{compared_by_prefix, Mode, Report};
use crate::FrontStage;

/// What the statistics know of a lowered program before it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Front {
    /// `cove_ir::verify` accepts it.
    pub verified: bool,
    /// It has the entry, `playground.main`.
    pub has_entry: bool,
}

/// How far an input got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reached {
    /// The bytes are not UTF-8, so not a source.
    NotUtf8,
    /// The front end refused it at this stage.
    Refused(FrontStage),
    /// It lowered, and every mode ran.
    Ran,
}

/// One mode's run, as the statistics count it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeRun {
    /// The mode's name.
    pub mode: &'static str,
    /// How the run ended, by `RunOutcome` name.
    pub outcome: &'static str,
    /// The harness's reason, if the run stopped in a way that belongs to the
    /// mode.
    pub stopped: Option<&'static str>,
    /// Calls entered compiled code.
    pub entered_compiled_code: bool,
    /// The run collected at least once.
    pub collected: bool,
    /// Collections made while a compiled frame was on the stack.
    pub collected_while_compiled: u64,
}

/// One input, classified. See the module documentation for what each part
/// counts towards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    /// The target that ran it.
    pub target: &'static str,
    /// How far it got.
    pub reached: Reached,
    /// What was known of the lowered program; all false unless it lowered.
    pub front: Front,
    /// Every mode that ran, in order.
    pub runs: Vec<ModeRun>,
    /// Every mode that did not, and why.
    pub skipped: Vec<(&'static str, String)>,
    /// Pairs of modes compared in full.
    pub pairs_full: u64,
    /// Pairs of modes compared by prefix only.
    pub pairs_prefix: u64,
    /// Of [`Classification::pairs_prefix`], those whose common prefix was
    /// empty on both streams.
    pub pairs_empty_prefix: u64,
    /// The input is a finding.
    pub mismatch: bool,
}

impl Classification {
    /// An input whose bytes are not UTF-8.
    pub fn not_utf8(target: &'static str) -> Classification {
        Classification::stopped_at(target, Reached::NotUtf8, false)
    }

    /// An input the front end refused at `stage`. For `differential` that is
    /// a finding, since its programs are well typed by construction.
    pub fn refused(target: &'static str, stage: FrontStage) -> Classification {
        Classification::stopped_at(target, Reached::Refused(stage), target == "differential")
    }

    fn stopped_at(target: &'static str, reached: Reached, mismatch: bool) -> Classification {
        Classification {
            target,
            reached,
            front: Front::default(),
            runs: Vec::new(),
            skipped: Vec::new(),
            pairs_full: 0,
            pairs_prefix: 0,
            pairs_empty_prefix: 0,
            mismatch,
        }
    }

    /// It has an entry and every pair was compared in full.
    pub fn full(&self) -> bool {
        self.reached == Reached::Ran && self.front.has_entry && self.pairs_prefix == 0
    }

    /// It has an entry and some pair was compared by prefix only.
    pub fn prefix(&self) -> bool {
        self.reached == Reached::Ran && self.front.has_entry && self.pairs_prefix > 0
    }

    /// Every pair compared by prefix had an empty common prefix.
    pub fn empty_prefix(&self) -> bool {
        self.prefix() && self.pairs_empty_prefix == self.pairs_prefix
    }

    /// The `native` mode entered compiled code.
    pub fn native_ran(&self) -> bool {
        self.mode_did(Mode::Native, |r| r.entered_compiled_code)
    }

    /// Some mode collected while a compiled frame was on the stack.
    pub fn gc_while_compiled(&self) -> bool {
        self.runs.iter().any(|r| r.collected_while_compiled > 0)
    }

    fn mode_did(&self, mode: Mode, did: impl Fn(&ModeRun) -> bool) -> bool {
        self.runs.iter().any(|r| r.mode == mode.name() && did(r))
    }
}

/// `report`, the runs of a program that lowered, classified.
pub fn classify(target: &'static str, report: &Report, front: Front) -> Classification {
    let mut classification = Classification::stopped_at(target, Reached::Ran, false);
    classification.front = front;
    classification.mismatch = report.mismatch.is_some();
    classification.runs = report
        .observed
        .iter()
        .map(|o| ModeRun {
            mode: o.mode.name(),
            outcome: o.outcome.as_str(),
            stopped: o.stopped,
            entered_compiled_code: o.native_calls.unwrap_or(0) > 0,
            collected: o.collections.unwrap_or(0) > 0,
            collected_while_compiled: o.collections_while_compiled.unwrap_or(0),
        })
        .collect();
    classification.skipped = report
        .skipped
        .iter()
        .map(|(mode, why)| (mode.name(), why.clone()))
        .collect();
    for (i, a) in report.observed.iter().enumerate() {
        for b in &report.observed[i + 1..] {
            if compared_by_prefix(a, b) {
                classification.pairs_prefix += 1;
                let common = |x: &[u8], y: &[u8]| x.len().min(y.len());
                if common(&a.stdout, &b.stdout) == 0 && common(&a.stderr, &b.stderr) == 0 {
                    classification.pairs_empty_prefix += 1;
                }
            } else {
                classification.pairs_full += 1;
            }
        }
    }
    classification
}

/// Counters, as a tree whose leaves are counts: see the module documentation
/// for the keys. Adding two is adding leaf to leaf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    root: BTreeMap<String, Node>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Count(u64),
    Group(BTreeMap<String, Node>),
}

impl Stats {
    /// No input counted.
    pub fn new() -> Stats {
        Stats::default()
    }

    /// The count at `path`, nought where nothing was counted.
    pub fn get(&self, path: &[&str]) -> u64 {
        let mut group = &self.root;
        for (i, key) in path.iter().enumerate() {
            match group.get(*key) {
                Some(Node::Count(n)) if i + 1 == path.len() => return *n,
                Some(Node::Group(next)) => group = next,
                _ => return 0,
            }
        }
        0
    }

    /// The counts under `path`, by key: for a group of reasons.
    pub fn group(&self, path: &[&str]) -> BTreeMap<String, u64> {
        let mut group = &self.root;
        for key in path {
            match group.get(*key) {
                Some(Node::Group(next)) => group = next,
                _ => return BTreeMap::new(),
            }
        }
        group
            .iter()
            .filter_map(|(k, n)| match n {
                Node::Count(n) => Some((k.clone(), *n)),
                Node::Group(_) => None,
            })
            .collect()
    }

    /// The keys of the groups under `path`.
    fn groups(&self, path: &[&str]) -> Vec<String> {
        let mut group = &self.root;
        for key in path {
            match group.get(*key) {
                Some(Node::Group(next)) => group = next,
                _ => return Vec::new(),
            }
        }
        group
            .iter()
            .filter(|(_, n)| matches!(n, Node::Group(_)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Adds `n` at `path`.
    ///
    /// # Panics
    ///
    /// If `path` passes through a count or ends at a group: the keys are this
    /// module's own, so that is a bug here.
    pub fn add(&mut self, path: &[&str], n: u64) {
        let (last, groups) = path.split_last().expect("a path has a key");
        let mut group = &mut self.root;
        for key in groups {
            let node = group
                .entry((*key).to_string())
                .or_insert_with(|| Node::Group(BTreeMap::new()));
            group = match node {
                Node::Group(next) => next,
                Node::Count(_) => panic!("`{key}` is a count, not a group"),
            };
        }
        match group.entry((*last).to_string()).or_insert(Node::Count(0)) {
            Node::Count(count) => *count += n,
            Node::Group(_) => panic!("`{last}` is a group, not a count"),
        }
    }

    /// Counts one input.
    pub fn record(&mut self, c: &Classification) {
        self.add(&["inputs"], 1);
        self.add(&["targets", c.target], 1);
        if c.mismatch {
            self.add(&["mismatches"], 1);
        }
        // Each stage counts the inputs that passed it.
        let passed = match c.reached {
            Reached::NotUtf8 => 0,
            Reached::Refused(FrontStage::Parse) => 1,
            Reached::Refused(FrontStage::Check) => 2,
            Reached::Refused(FrontStage::Lower) => 3,
            Reached::Ran => 4,
        };
        for stage in ["utf8", "parsed", "checked", "lowered"].iter().take(passed) {
            self.add(&["stages", stage], 1);
        }
        if c.reached != Reached::Ran {
            return;
        }
        if c.front.verified {
            self.add(&["stages", "verified"], 1);
        }
        if c.front.has_entry {
            self.add(&["stages", "entry"], 1);
        }
        self.add(&["stages", "ran"], 1);

        let (native, under) = (c.native_ran(), c.gc_while_compiled());
        if c.front.has_entry {
            let kind = if c.full() { "full" } else { "prefix" };
            self.add(&["comparisons", kind], 1);
            if native {
                self.add(&["comparisons", &format!("{kind}_with_native")], 1);
            }
            if under {
                self.add(
                    &["comparisons", &format!("{kind}_with_gc_while_compiled")],
                    1,
                );
            }
            if c.empty_prefix() {
                self.add(&["comparisons", "empty_prefix"], 1);
            }
            self.add(&["comparisons", "pairs_full"], c.pairs_full);
            self.add(&["comparisons", "pairs_prefix"], c.pairs_prefix);
            self.add(&["comparisons", "pairs_empty_prefix"], c.pairs_empty_prefix);
        } else {
            // Every mode refuses a program with no `main` alike, before its
            // first instruction: the agreement compares a refusal, not a run.
            self.add(&["comparisons", "no_entry"], 1);
        }

        if native {
            self.add(&["programs", "native_ran"], 1);
        }
        if c.mode_did(Mode::VmGc, |r| r.collected) {
            self.add(&["programs", "vm_gc_collected"], 1);
        }
        if c.mode_did(Mode::NativeGc, |r| r.collected) {
            self.add(&["programs", "native_gc_collected"], 1);
        }
        if under {
            self.add(&["programs", "gc_while_compiled"], 1);
        }

        for run in &c.runs {
            let mode = ["modes", run.mode];
            self.add(&[mode[0], mode[1], "ran"], 1);
            match run.stopped {
                Some(why) => self.add(&[mode[0], mode[1], "stopped", why], 1),
                None => self.add(&[mode[0], mode[1], "ended", run.outcome], 1),
            }
            if run.entered_compiled_code {
                self.add(&[mode[0], mode[1], "entered_compiled_code"], 1);
            }
            if run.collected {
                self.add(&[mode[0], mode[1], "collected"], 1);
            }
            if run.collected_while_compiled > 0 {
                self.add(&[mode[0], mode[1], "collected_while_compiled"], 1);
                self.add(
                    &["collections_while_compiled"],
                    run.collected_while_compiled,
                );
            }
        }
        for (mode, why) in &c.skipped {
            self.add(&["modes", mode, "skipped", why], 1);
        }
    }

    /// Counts an input that panicked outside the harness's own findings: the
    /// replay's, which catches it.
    pub fn record_crash(&mut self, target: &str) {
        self.add(&["inputs"], 1);
        self.add(&["targets", target], 1);
        self.add(&["crashes"], 1);
    }

    /// Adds `other`'s counts to these.
    ///
    /// # Errors
    ///
    /// If a key is a count in one and a group in the other: the two were not
    /// written by the same schema.
    pub fn merge(&mut self, other: &Stats) -> Result<(), String> {
        merge_into(&mut self.root, &other.root, "")
    }

    /// The counters as JSON, keys sorted, one per line.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        write_group(&mut out, &self.root, 0);
        out.push('\n');
        out
    }

    /// Counters [`Stats::to_json`] wrote.
    ///
    /// # Errors
    ///
    /// If `text` is not JSON, or is JSON with anything but objects and
    /// non-negative integers in it.
    pub fn from_json(text: &str) -> Result<Stats, String> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
        };
        parser.space();
        let root = parser.group()?;
        parser.space();
        if parser.at != parser.bytes.len() {
            return Err(format!("trailing bytes at offset {}", parser.at));
        }
        Ok(Stats { root })
    }

    /// The counters as a person reads them, the headline first.
    pub fn summary(&self) -> String {
        let g = |path: &[&str]| self.get(path);
        let c = |key: &str| self.get(&["comparisons", key]);
        let (prefix, empty) = (c("prefix"), c("empty_prefix"));
        let mut out = format!(
            "{} programs compared in full, {} of them with native execution, {} with a \
             collection while compiled code was live; {prefix} prefix-only, {empty} of those \
             with an empty common prefix ({}); {} mismatches and {} crashes over {} inputs.\n",
            c("full"),
            c("full_with_native"),
            c("full_with_gc_while_compiled"),
            share(empty, prefix),
            g(&["mismatches"]),
            g(&["crashes"]),
            g(&["inputs"]),
        );
        let s = |key: &str| self.get(&["stages", key]);
        let _ = writeln!(
            out,
            "front end: {} inputs, {} UTF-8, {} parsed, {} checked, {} lowered, {} verified, \
             {} with an entry, {} ran ({} with no entry, compared as a refusal)",
            g(&["inputs"]),
            s("utf8"),
            s("parsed"),
            s("checked"),
            s("lowered"),
            s("verified"),
            s("entry"),
            s("ran"),
            c("no_entry"),
        );
        let _ = writeln!(
            out,
            "pairs of modes: {} compared in full, {} by prefix only, {} of those with an empty \
             common prefix ({})",
            c("pairs_full"),
            c("pairs_prefix"),
            c("pairs_empty_prefix"),
            share(c("pairs_empty_prefix"), c("pairs_prefix")),
        );
        let p = |key: &str| self.get(&["programs", key]);
        let _ = writeln!(
            out,
            "programs: native ran in {}, vm-gc collected in {}, native-gc collected in {}, a \
             collection under a compiled frame in {} ({} such collections); prefix-only with \
             native execution {}, with a collection under a compiled frame {}",
            p("native_ran"),
            p("vm_gc_collected"),
            p("native_gc_collected"),
            p("gc_while_compiled"),
            g(&["collections_while_compiled"]),
            c("prefix_with_native"),
            c("prefix_with_gc_while_compiled"),
        );
        for mode in self.groups(&["modes"]) {
            let m = |key: &str| self.get(&["modes", &mode, key]);
            let listed = |key: &str| {
                self.group(&["modes", &mode, key])
                    .iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = write!(out, "  {mode}: ran {}", m("ran"));
            for (key, label) in [
                ("ended", "ended"),
                ("stopped", "stopped"),
                ("skipped", "skipped"),
            ] {
                let items = listed(key);
                if !items.is_empty() {
                    let _ = write!(out, "; {label}: {items}");
                }
            }
            for (key, label) in [
                ("entered_compiled_code", "entered compiled code"),
                ("collected", "collected"),
                (
                    "collected_while_compiled",
                    "collected under a compiled frame",
                ),
            ] {
                if m(key) > 0 {
                    let _ = write!(out, "; {label} {}", m(key));
                }
            }
            out.push('\n');
        }
        out
    }
}

/// `part` of `whole` as a percentage, or `n/a` of nothing.
fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

fn merge_into(
    into: &mut BTreeMap<String, Node>,
    from: &BTreeMap<String, Node>,
    at: &str,
) -> Result<(), String> {
    for (key, node) in from {
        let path = format!("{at}/{key}");
        match (into.get_mut(key), node) {
            (None, node) => {
                into.insert(key.clone(), node.clone());
            }
            (Some(Node::Count(a)), Node::Count(b)) => *a += b,
            (Some(Node::Group(a)), Node::Group(b)) => merge_into(a, b, &path)?,
            _ => {
                return Err(format!(
                    "`{path}` is a count in one file and a group in the other"
                ))
            }
        }
    }
    Ok(())
}

fn write_group(out: &mut String, group: &BTreeMap<String, Node>, depth: usize) {
    if group.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{\n");
    for (i, (key, node)) in group.iter().enumerate() {
        out.push_str(&"  ".repeat(depth + 1));
        write_string(out, key);
        out.push_str(": ");
        match node {
            Node::Count(n) => {
                let _ = write!(out, "{n}");
            }
            Node::Group(inner) => write_group(out, inner, depth + 1),
        }
        if i + 1 < group.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&"  ".repeat(depth));
    out.push('}');
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The JSON [`write_group`] writes: objects, strings as keys, and
/// non-negative integers. Anything else is refused.
struct Parser<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        self.space();
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected `{}` at offset {}", byte as char, self.at))
        }
    }

    fn group(&mut self) -> Result<BTreeMap<String, Node>, String> {
        self.expect(b'{')?;
        let mut group = BTreeMap::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(group);
        }
        loop {
            self.space();
            let key = self.string()?;
            self.expect(b':')?;
            self.space();
            let node = match self.bytes.get(self.at) {
                Some(b'{') => Node::Group(self.group()?),
                Some(b) if b.is_ascii_digit() => Node::Count(self.number()?),
                _ => {
                    return Err(format!(
                        "expected a count or an object at offset {}",
                        self.at
                    ))
                }
            };
            if group.insert(key.clone(), node).is_some() {
                return Err(format!("`{key}` appears twice"));
            }
            self.space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(group);
                }
                _ => return Err(format!("expected `,` or `}}` at offset {}", self.at)),
            }
        }
    }

    fn number(&mut self) -> Result<u64, String> {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(|| format!("a count out of range at offset {start}"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(&byte) = self.bytes.get(self.at) else {
                return Err("an unterminated string".to_string());
            };
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.at) else {
                        return Err("an unterminated escape".to_string());
                    };
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let high = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&high) {
                                if self.bytes.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return Err("a lone high surrogate".to_string());
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err("a high surrogate without its low".to_string());
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                high
                            };
                            out.push(
                                char::from_u32(c).ok_or_else(|| "a lone surrogate".to_string())?,
                            );
                        }
                        _ => return Err(format!("an unknown escape at offset {}", self.at)),
                    }
                }
                _ => {
                    // A UTF-8 sequence, copied whole: the input is a `&str`.
                    let start = self.at - 1;
                    let len = match byte {
                        0x00..=0x7F => 1,
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        _ => 4,
                    };
                    let end = (start + len).min(self.bytes.len());
                    let piece = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| format!("bad UTF-8 at offset {start}"))?;
                    out.push_str(piece);
                    self.at = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| format!("a bad `\\u` escape at offset {}", self.at))?;
        self.at += 4;
        Ok(digits)
    }
}

/// How many inputs between two writes of a session's file.
pub const FLUSH_EVERY: u64 = 256;

/// The longest a session goes without writing its file, once an input has
/// been counted since the last write.
pub const FLUSH_AFTER: Duration = Duration::from_secs(10);

/// The environment variable naming the directory a session writes to.
pub const STATS_DIR: &str = "COVE_FUZZ_STATS_DIR";

struct Session {
    stats: Stats,
    unwritten: u64,
    written_at: Option<Instant>,
}

/// Every target's session in this process. A fuzz target is one thread, but
/// a lock costs nothing next to six runs of a program, and keeps this sound
/// whatever calls it.
static SESSIONS: Mutex<BTreeMap<&'static str, Session>> = Mutex::new(BTreeMap::new());

fn stats_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os(STATS_DIR)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}

/// Counts `classification` in this process's session for its target, and
/// writes the session's file if one is due — always if `failing`, because
/// the input is about to panic and the process with it.
///
/// Does nothing at all unless [`STATS_DIR`] is set.
pub fn record(classification: &Classification, failing: bool) {
    let Some(dir) = stats_dir() else {
        return;
    };
    // A poisoned lock is a panic that happened while counting, which is not
    // the fuzzer's business; the counts go on.
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let session = sessions
        .entry(classification.target)
        .or_insert_with(|| Session {
            stats: Stats::new(),
            unwritten: 0,
            written_at: None,
        });
    session.stats.record(classification);
    session.unwritten += 1;
    let due = match session.written_at {
        None => true,
        Some(at) => session.unwritten >= FLUSH_EVERY || at.elapsed() >= FLUSH_AFTER,
    };
    if due || failing {
        write_session(dir, classification.target, &session.stats);
        session.unwritten = 0;
        session.written_at = Some(Instant::now());
    }
}

/// Writes `stats` to `<dir>/<target>-<pid>.json` by way of a temporary file
/// and a rename, so a reader never sees half a file. A failure is reported
/// once and otherwise ignored: the statistics are not what is being tested.
fn write_session(dir: &Path, target: &str, stats: &Stats) {
    let pid = std::process::id();
    let path = dir.join(format!("{target}-{pid}.json"));
    let temporary = dir.join(format!(".{target}-{pid}.json.tmp"));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&temporary, stats.to_json()))
        .and_then(|()| std::fs::rename(&temporary, &path));
    if let Err(why) = written {
        static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("cove-fuzz: could not write {}: {why}", path.display());
        }
    }
}

/// The targets [`replay`] can count.
pub const TARGETS: [&str; 2] = ["differential", "differential_source"];

/// What [`replay`] found: the counts, and every input that was a finding.
#[derive(Debug, Default)]
pub struct Replay {
    /// Every input, counted.
    pub stats: Stats,
    /// Each finding's input and its message's first line.
    pub findings: Vec<(PathBuf, String)>,
}

/// Runs every input under `paths` — files, and the files of directories,
/// recursively, in a stable order, leaving out `*.md` and dot-files — through
/// `target` without panicking, and counts each. A finding is counted and
/// listed; a panic anywhere else is counted as a crash and listed.
///
/// # Errors
///
/// If `target` is not one of [`TARGETS`], or a path cannot be read.
pub fn replay(target: &str, paths: &[PathBuf]) -> Result<Replay, String> {
    let classify: fn(&[u8]) -> (Classification, Option<String>) = match target {
        "differential" => crate::differential::differential_classified,
        "differential_source" => crate::differential::differential_source_classified,
        _ => {
            return Err(format!(
                "no statistics for `{target}`: {}",
                TARGETS.join(", ")
            ))
        }
    };
    let mut files = Vec::new();
    for path in paths {
        collect_files(path, &mut files)?;
    }
    files.sort();

    static RAISED: Mutex<Option<String>> = Mutex::new(None);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut raised) = RAISED.lock() {
            if raised.is_none() {
                let at = info
                    .location()
                    .map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
                let what = info
                    .payload()
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                *raised = Some(format!("{at}: {}", what.lines().next().unwrap_or("")));
            }
        }
    }));
    let mut replay = Replay::default();
    for file in files {
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(why) => {
                std::panic::set_hook(hook);
                return Err(format!("{}: {why}", file.display()));
            }
        };
        if let Ok(mut raised) = RAISED.lock() {
            *raised = None;
        }
        match std::panic::catch_unwind(|| classify(&bytes)) {
            Ok((classification, finding)) => {
                replay.stats.record(&classification);
                if let Some(why) = finding {
                    let first = why.lines().next().unwrap_or("").to_string();
                    replay.findings.push((file, first));
                }
            }
            Err(_) => {
                replay.stats.record_crash(target);
                let why = RAISED
                    .lock()
                    .ok()
                    .and_then(|r| r.clone())
                    .unwrap_or_default();
                replay.findings.push((file, format!("panic at {why}")));
            }
        }
    }
    std::panic::set_hook(hook);
    Ok(replay)
}

fn collect_files(path: &Path, into: &mut Vec<PathBuf>) -> Result<(), String> {
    let hidden = path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'));
    if path.is_dir() {
        let entries =
            std::fs::read_dir(path).map_err(|why| format!("{}: {why}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|why| format!("{}: {why}", path.display()))?;
            let child = entry.path();
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if !hidden {
                collect_files(&child, into)?;
            }
        }
    } else if path.is_file() {
        if !hidden && path.extension().is_none_or(|e| e != "md") {
            into.push(path.to_path_buf());
        }
    } else {
        return Err(format!("{}: no such file or directory", path.display()));
    }
    Ok(())
}

/// The files `cove-fuzz stats merge` is given, added together.
///
/// # Errors
///
/// If a file cannot be read or is not a statistics file.
pub fn merge_files(paths: &[PathBuf]) -> Result<Stats, String> {
    let mut files = Vec::new();
    for path in paths {
        collect_files(path, &mut files)?;
    }
    files.sort();
    let mut stats = Stats::new();
    for file in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "json"))
    {
        let text =
            std::fs::read_to_string(file).map_err(|why| format!("{}: {why}", file.display()))?;
        let one = Stats::from_json(&text).map_err(|why| format!("{}: {why}", file.display()))?;
        stats
            .merge(&one)
            .map_err(|why| format!("{}: {why}", file.display()))?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::differential::Observed;
    use cove_runtime::trace::RunOutcome;

    fn ran(mode: Mode, stdout: &str, stopped: Option<&'static str>) -> Observed {
        Observed {
            mode,
            outcome: if stopped.is_some() {
                RunOutcome::Deadline
            } else {
                RunOutcome::Success
            },
            answer: "()".to_string(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            stopped,
            collections: mode.heap_words().map(|_| 0),
            native_calls: mode.heap_words().map(|_| 0),
            collections_while_compiled: mode.heap_words().map(|_| 0),
        }
    }

    fn report(observed: Vec<Observed>) -> Report {
        Report {
            observed,
            skipped: Vec::new(),
            mismatch: None,
            native: None,
        }
    }

    const FRONT: Front = Front {
        verified: true,
        has_entry: true,
    };

    #[test]
    fn every_pair_finished_is_a_full_comparison() {
        let c = classify(
            "differential",
            &report(vec![
                ran(Mode::Ast, "1\n", None),
                ran(Mode::Vm, "1\n", None),
                ran(Mode::Native, "1\n", None),
            ]),
            FRONT,
        );
        assert!(c.full() && !c.prefix() && !c.empty_prefix());
        assert_eq!((c.pairs_full, c.pairs_prefix), (3, 0));
        let mut stats = Stats::new();
        stats.record(&c);
        assert_eq!(stats.get(&["comparisons", "full"]), 1);
        assert_eq!(stats.get(&["comparisons", "prefix"]), 0);
        assert_eq!(stats.get(&["stages", "ran"]), 1);
        assert_eq!(stats.get(&["stages", "utf8"]), 1);
        assert_eq!(stats.get(&["modes", "vm", "ended", "success"]), 1);

        // With no `main`, the agreement is of a refusal: neither kind.
        let none = classify(
            "differential_source",
            &report(vec![ran(Mode::Vm, "", None), ran(Mode::Native, "", None)]),
            Front {
                verified: true,
                has_entry: false,
            },
        );
        assert!(!none.full() && !none.prefix());
        stats.record(&none);
        assert_eq!(stats.get(&["comparisons", "no_entry"]), 1);
        assert_eq!(stats.get(&["comparisons", "full"]), 1);
        assert_eq!(stats.get(&["comparisons", "pairs_full"]), 3);
    }

    #[test]
    fn a_stop_makes_a_prefix_comparison_and_an_empty_one_is_told_apart() {
        // The interpreter met the deadline after printing a line: its pairs
        // compared that line.
        let printed = classify(
            "differential",
            &report(vec![
                ran(Mode::Ast, "1\n", Some("deadline")),
                ran(Mode::Vm, "1\n2\n", None),
                ran(Mode::Native, "1\n2\n", None),
            ]),
            FRONT,
        );
        assert!(printed.prefix() && !printed.full());
        assert_eq!(
            (
                printed.pairs_full,
                printed.pairs_prefix,
                printed.pairs_empty_prefix
            ),
            (1, 2, 0)
        );
        assert!(!printed.empty_prefix());

        // It met it before printing anything: its pairs compared nothing.
        let silent = classify(
            "differential",
            &report(vec![
                ran(Mode::Ast, "", Some("deadline")),
                ran(Mode::Vm, "1\n2\n", None),
                ran(Mode::Native, "1\n2\n", None),
            ]),
            FRONT,
        );
        assert!(silent.empty_prefix());
        assert_eq!(silent.pairs_empty_prefix, 2);

        // A prefix pair with an empty stdout but a common stderr compared
        // something.
        let mut a = ran(Mode::Ast, "", Some("deadline"));
        a.stderr = b"e".to_vec();
        let mut b = ran(Mode::Vm, "1\n", None);
        b.stderr = b"e".to_vec();
        let stderr = classify("differential", &report(vec![a, b]), FRONT);
        assert_eq!(stderr.pairs_empty_prefix, 0);

        let mut stats = Stats::new();
        stats.record(&printed);
        stats.record(&silent);
        assert_eq!(stats.get(&["comparisons", "prefix"]), 2);
        assert_eq!(stats.get(&["comparisons", "empty_prefix"]), 1);
        assert_eq!(stats.get(&["comparisons", "pairs_prefix"]), 4);
        assert_eq!(stats.get(&["comparisons", "pairs_empty_prefix"]), 2);
        assert_eq!(stats.get(&["modes", "ast", "stopped", "deadline"]), 2);
        assert!(stats
            .summary()
            .starts_with("0 programs compared in full, 0 of them with native execution, 0 with a collection while compiled code was live; 2 prefix-only, 1 of those with an empty common prefix (50.0%)"));
    }

    #[test]
    fn running_out_on_equal_heaps_is_still_a_full_comparison() {
        let oom = |mode| {
            let mut o = ran(mode, "", Some(crate::differential::OUT_OF_MEMORY));
            o.outcome = RunOutcome::Invariant;
            o
        };
        let c = classify(
            "differential",
            &report(vec![oom(Mode::Vm), oom(Mode::Native)]),
            FRONT,
        );
        assert!(c.full(), "{c:?}");
    }

    #[test]
    fn native_execution_and_collections_under_compiled_frames_are_counted() {
        let mut native = ran(Mode::Native, "", None);
        native.native_calls = Some(3);
        let mut native_gc = ran(Mode::NativeGc, "", None);
        native_gc.native_calls = Some(3);
        native_gc.collections = Some(2);
        native_gc.collections_while_compiled = Some(1);
        let mut vm_gc = ran(Mode::VmGc, "", None);
        vm_gc.collections = Some(2);
        let mut r = report(vec![ran(Mode::Vm, "", None), native, vm_gc, native_gc]);
        r.skipped
            .push((Mode::Image, "the program has no entry".to_string()));
        let c = classify("differential", &r, FRONT);
        assert!(c.native_ran() && c.gc_while_compiled() && c.full());

        let mut stats = Stats::new();
        stats.record(&c);
        assert_eq!(stats.get(&["programs", "native_ran"]), 1);
        assert_eq!(stats.get(&["programs", "vm_gc_collected"]), 1);
        assert_eq!(stats.get(&["programs", "native_gc_collected"]), 1);
        assert_eq!(stats.get(&["programs", "gc_while_compiled"]), 1);
        assert_eq!(stats.get(&["collections_while_compiled"]), 1);
        assert_eq!(stats.get(&["comparisons", "full_with_native"]), 1);
        assert_eq!(
            stats.get(&["comparisons", "full_with_gc_while_compiled"]),
            1
        );
        assert_eq!(
            stats.get(&["modes", "image", "skipped", "the program has no entry"]),
            1
        );
        assert_eq!(
            stats.get(&["modes", "native-gc", "collected_while_compiled"]),
            1
        );

        // Collections and compiled code at different moments are not one.
        let mut apart = ran(Mode::NativeGc, "", None);
        apart.native_calls = Some(3);
        apart.collections = Some(2);
        apart.collections_while_compiled = Some(0);
        let c = classify("differential", &report(vec![apart]), FRONT);
        assert!(!c.gc_while_compiled());
    }

    #[test]
    fn the_front_end_stages_count_what_passed_them() {
        let mut stats = Stats::new();
        stats.record(&Classification::not_utf8("differential_source"));
        stats.record(&Classification::refused(
            "differential_source",
            FrontStage::Parse,
        ));
        stats.record(&Classification::refused(
            "differential_source",
            FrontStage::Check,
        ));
        stats.record(&Classification::refused(
            "differential_source",
            FrontStage::Lower,
        ));
        assert_eq!(stats.get(&["inputs"]), 4);
        assert_eq!(stats.get(&["stages", "utf8"]), 3);
        assert_eq!(stats.get(&["stages", "parsed"]), 2);
        assert_eq!(stats.get(&["stages", "checked"]), 1);
        assert_eq!(stats.get(&["stages", "lowered"]), 0);
        assert_eq!(
            stats.get(&["mismatches"]),
            0,
            "a source refused is no finding"
        );
        let generated = Classification::refused("differential", FrontStage::Check);
        assert!(generated.mismatch, "a generated program refused is one");
    }

    #[test]
    fn json_round_trips_and_merges_by_adding() {
        let mut a = Stats::new();
        a.add(&["inputs"], 3);
        a.add(
            &["modes", "vm", "skipped", "a \"quoted\"\\ reason\n\u{1}é😀"],
            2,
        );
        a.add(&["comparisons", "full"], 1);
        let text = a.to_json();
        assert_eq!(Stats::from_json(&text).expect("it reads back"), a);

        let mut b = Stats::new();
        b.add(&["inputs"], 4);
        b.add(&["comparisons", "prefix"], 5);
        b.add(
            &["modes", "vm", "skipped", "a \"quoted\"\\ reason\n\u{1}é😀"],
            1,
        );
        a.merge(&b).expect("one schema");
        assert_eq!(a.get(&["inputs"]), 7);
        assert_eq!(a.get(&["comparisons", "full"]), 1);
        assert_eq!(a.get(&["comparisons", "prefix"]), 5);
        assert_eq!(
            a.get(&["modes", "vm", "skipped", "a \"quoted\"\\ reason\n\u{1}é😀"]),
            3
        );
        assert_eq!(Stats::from_json("{}").expect("empty"), Stats::new());
        assert_eq!(
            Stats::from_json("{\"a\": {\"b\": 1}}")
                .expect("compact")
                .get(&["a", "b"]),
            1
        );

        let mut clash = Stats::new();
        clash.add(&["inputs", "x"], 1);
        assert!(a.merge(&clash).is_err());
        for bad in [
            "",
            "[]",
            "{\"a\": -1}",
            "{\"a\": 1.5}",
            "{\"a\": 1",
            "{\"a\": 1} x",
        ] {
            assert!(Stats::from_json(bad).is_err(), "{bad:?} was read");
        }
    }

    #[test]
    fn a_session_writes_nothing_without_its_variable() {
        // `cargo t` does not set it; if it were set, this process would be
        // writing into the directory it names, which a test must not do.
        assert!(std::env::var_os(STATS_DIR).is_none());
        record(&Classification::not_utf8("differential_source"), true);
        assert!(SESSIONS.lock().map(|s| s.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_session_file_is_written_whole() {
        let dir = std::env::temp_dir().join(format!("cove-fuzz-stats-{}", std::process::id()));
        let mut stats = Stats::new();
        stats.record(&Classification::not_utf8("differential_source"));
        write_session(&dir, "differential_source", &stats);
        let merged = merge_files(std::slice::from_ref(&dir)).expect("the file reads");
        assert_eq!(merged, stats);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
