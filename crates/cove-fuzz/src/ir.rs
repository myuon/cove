//! The VM-level harnesses: a [`cove_ir::Program`] handed to the machine as it
//! is, with no front end in front of it.
//!
//! [ADR 0120] states the contract these test: **for every `Program`, either the
//! machine refuses it before its first instruction — `cove_ir::verify`, the
//! encoder or the bytecode verifier says no — or a run of it, with any entry
//! arguments, under a deadline, a call-depth limit and a heap capacity, ends in
//! `Ok(value)` or `Err(RuntimeError)`**: no panic, no abort, no Rust stack
//! overflow, no hang past the deadline, no undefined behaviour. The source
//! harnesses can only reach the programs the lowering writes; these reach the
//! rest.
//!
//! # The three ways in
//!
//! - [`ir`](fn@ir): a program [`crate::irgen`] builds from the fuzzer's bytes,
//!   by `Repr` and layout so that most of them verify;
//! - [`ir_mutate`]: a lowered program read from an image body in the input,
//!   then edited by [`crate::mutate`] — an operand, a layout, an id, a jump;
//! - [`crate::image`]: a decoded image, which since ADR 0120 is also run when
//!   it verifies.
//!
//! # What is compared
//!
//! There is no source, so there is no oracle that says what the program
//! *should* answer. What there is instead:
//!
//! - **the modes against each other**, under [ADR 0113]'s rules
//!   ([`crate::differential::disagreement`]): `vm`, `vm-again` (the same mode
//!   twice, which only determinism can make agree), `vm-gc` (a heap small
//!   enough that a program that allocates collects), `native` and `native-gc`
//!   where this host has the tier, and `image` — the program written by
//!   `cove_ir::serial` and read back;
//! - **the refusal against the run**: a program `verify` refuses must be
//!   refused by the machine too, with the verifier's sentence, and not run;
//! - **the answer against its type**, which needs no comparing: the machine
//!   converts the entry's answer at the entry's declared return layout, so an
//!   answer that is not one is a panic in that conversion, which is a finding
//!   like any other.
//!
//! [ADR 0113]: ../../../docs/adr/0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md
//! [ADR 0120]: ../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use std::fmt::Write as _;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_ir::{LayoutId, Program, Shape};
use cove_runtime::{OwnedVm, PreparedProgram, Runtime, Value};

use crate::differential::{self, gc_heap_words, Mode, Observed, Report};
use crate::{on_stack, HEAP_WORDS};

/// The wall-clock time one run of a program here may take.
///
/// Shorter than [`crate::DEADLINE`]: a generated body jumps backwards as
/// readily as forwards, so a share of programs loop until they are stopped,
/// and each is run in up to six modes. What the deadline is for — that a
/// program which never returns is stopped at a safepoint, and that the stop
/// is an `Err` — is the same at any length.
pub const IR_DEADLINE: std::time::Duration = std::time::Duration::from_millis(25);

/// [`crate::limits`], with [`IR_DEADLINE`].
pub fn limits() -> cove_runtime::Limits {
    cove_runtime::Limits {
        deadline: Some(IR_DEADLINE),
        ..crate::limits()
    }
}

/// One entry argument, as something that can cross a thread: a [`Value`] is
/// not `Send`, so the machine's own thread builds it from this.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Unit,
    Bool(bool),
    Int(i64),
    /// The bits, so that a NaN's payload survives.
    Float(u64),
    Duration(i64),
    Str(String),
    /// `Array<String>`, the one argument an entry `cove run` calls takes.
    Strings(Vec<String>),
}

impl Arg {
    /// The value the machine's boundary converts at the parameter's layout.
    pub fn value(&self) -> Value {
        match self {
            Arg::Unit => Value::unit(),
            Arg::Bool(b) => Value::bool(*b),
            Arg::Int(n) => Value::int(*n),
            Arg::Float(bits) => Value::float(f64::from_bits(*bits)),
            Arg::Duration(n) => Value::duration(*n),
            Arg::Str(text) => Value::string(text.as_str()),
            Arg::Strings(items) => Value::array(items.iter().map(|s| Value::string(s.as_str()))),
        }
    }
}

/// A program and how to enter it.
#[derive(Clone)]
pub struct Subject {
    pub program: Program,
    /// The files the program's spans point into. A generated program has one.
    pub sources: Arc<SourceMap>,
    pub module: Arc<str>,
    pub name: Arc<str>,
    pub args: Vec<Arg>,
}

/// The plausible argument for a parameter of `layout`: the value of its family
/// that the boundary would accept, or [`Arg::Unit`] where there is none to
/// guess — which the boundary refuses, and that is an answer too.
pub fn plausible_arg(program: &Program, layout: LayoutId) -> Arg {
    let Some(held) = program.layouts.get(layout.index()) else {
        return Arg::Unit;
    };
    match &held.shape {
        Shape::Word(repr) => match repr {
            cove_ir::Repr::Bool => Arg::Bool(true),
            cove_ir::Repr::Int => Arg::Int(7),
            cove_ir::Repr::Float => Arg::Float(1.5f64.to_bits()),
            cove_ir::Repr::Duration => Arg::Duration(1_000),
            _ => Arg::Unit,
        },
        Shape::Str => Arg::Str("fuzz".to_string()),
        Shape::Elements { .. } => Arg::Strings(vec!["a".to_string(), "bc".to_string()]),
        _ => Arg::Unit,
    }
}

/// What one subject did: whether it verified, and every mode's run.
#[derive(Debug)]
pub struct IrReport {
    /// `cove_ir::verify`'s answer: `Err` with its first fault.
    pub verified: Result<(), String>,
    /// The runs, and the first disagreement among them. For a program that
    /// did not verify, the one `vm` run that has to refuse it.
    pub runs: Report,
}

impl IrReport {
    /// The report as lines a person reads.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        match &self.verified {
            Ok(()) => out.push_str("verify: ok\n"),
            Err(why) => {
                let _ = writeln!(out, "verify: refused: {why}");
            }
        }
        out.push_str(&self.runs.describe());
        out
    }

    /// Whether this is a finding: two modes disagreed, or the machine and the
    /// verifier did.
    pub fn is_finding(&self) -> bool {
        self.runs.mismatch.is_some()
    }
}

/// Runs `subject` in every mode, on the stack `cove` runs on.
pub fn run(subject: Subject) -> IrReport {
    on_stack(move || run_here(subject))
}

/// [`run`], on the calling thread, which must already have the stack.
pub(crate) fn run_here(subject: Subject) -> IrReport {
    let verified = cove_ir::verify(&subject.program).map_err(|faults| {
        faults
            .first()
            .map_or_else(|| "refused".to_string(), ToString::to_string)
    });
    let sources = Arc::clone(&subject.sources);
    let program = Arc::new(subject.program.clone());
    let encoded = PreparedProgram::new(Arc::clone(&program));

    if verified.is_err() {
        // The machine's own boundary has to say the same: a refused program
        // is refused before it runs, and refused with the verifier's words.
        let vm = machine_run(Mode::Vm, &sources, encoded, HEAP_WORDS, &subject);
        let mismatch = (!vm.answer.contains("does not verify")).then(|| {
            format!(
                "`cove_ir::verify` refused the program and the machine answered `{}`",
                vm.outcome.as_str()
            )
        });
        return IrReport {
            verified,
            runs: Report {
                observed: vec![vm],
                skipped: Vec::new(),
                pairs: Vec::new(),
                mismatch,
                native: None,
            },
        };
    }

    let native = PreparedProgram::new(Arc::clone(&program)).with_native();
    let summary = native
        .as_ref()
        .ok()
        .and_then(PreparedProgram::native)
        .map(|n| {
            format!(
                "compiled {} of {} reachable functions",
                n.compiled(),
                n.reachable()
            )
        });
    let mut observed = Vec::new();
    let mut skipped = Vec::new();
    observed.push(machine_run(
        Mode::Vm,
        &sources,
        encoded.clone(),
        HEAP_WORDS,
        &subject,
    ));
    observed.push(machine_run(
        Mode::Again,
        &sources,
        encoded.clone(),
        HEAP_WORDS,
        &subject,
    ));
    // The small heap above what the program's literals took on `vm`.
    let small = gc_heap_words(observed[0].literal_words.unwrap_or(0));
    observed.push(machine_run(Mode::VmGc, &sources, encoded, small, &subject));
    match &native {
        Ok(native) => {
            observed.push(machine_run(
                Mode::Native,
                &sources,
                native.clone(),
                HEAP_WORDS,
                &subject,
            ));
            observed.push(machine_run(
                Mode::NativeGc,
                &sources,
                native.clone(),
                small,
                &subject,
            ));
        }
        Err(why) => {
            skipped.push((Mode::Native, why.to_string()));
            skipped.push((Mode::NativeGc, why.to_string()));
        }
    }
    let mut mismatch = None;
    match cove_ir::serial::encode(
        &subject.program,
        &subject.sources,
        &subject.module,
        &subject.name,
    ) {
        Err(why) => skipped.push((Mode::Image, format!("the program does not encode: {why}"))),
        Ok(bytes) => match cove_ir::serial::decode(&bytes) {
            Ok(image) => {
                let image_sources = Arc::new(image.sources);
                let prepared = PreparedProgram::new(Arc::new(image.program));
                observed.push(machine_run(
                    Mode::Image,
                    &image_sources,
                    prepared,
                    HEAP_WORDS,
                    &subject,
                ));
            }
            // `encode` and `decode` are each other's inverse, so a program one
            // writes and the other cannot read is a finding of its own.
            Err(why) => mismatch = Some(format!("the program's image does not read back: {why}")),
        },
    }
    let (pairs, first) = differential::compare_all(&observed);
    let mismatch = mismatch.or(first);
    IrReport {
        verified,
        runs: Report {
            observed,
            skipped,
            pairs,
            mismatch,
            native: summary,
        },
    }
}

/// One run of `prepared` in `mode`, entered by `subject`'s entry and arguments.
fn machine_run(
    mode: Mode,
    sources: &Arc<SourceMap>,
    prepared: PreparedProgram,
    heap_words: usize,
    subject: &Subject,
) -> Observed {
    let (hosts, out, err) = differential::hosts(limits());
    let hosts = Arc::new(hosts);
    // No checked program: `invoke_lowered` holds the call to what the
    // lowering kept, as `cove fmt` runs covefmt.
    let runtime = Arc::new(Runtime::new(
        Arc::default(),
        Arc::clone(sources),
        Arc::clone(&hosts),
    ));
    let mut vm = OwnedVm::with_heap_words(runtime, hosts, prepared, heap_words);
    let args = subject.args.iter().map(Arg::value).collect();
    let answer = vm.invoke_lowered(&subject.module, &subject.name, args);
    let collections = vm.collections();
    let native_calls = vm.tiers().native();
    let under_compiled = vm.collections_while_compiled();
    let instructions = vm.instructions();
    let literal_words = vm.literal_words();
    drop(vm);
    let mut observed = differential::observe(mode, answer, &out, &err);
    observed.literal_words = Some(literal_words);
    observed.collections = Some(collections);
    observed.native_calls = Some(native_calls);
    observed.collections_while_compiled = Some(under_compiled);
    observed.instructions = Some(instructions);
    observed
}

/// The message a finding panics with — the program and every run — if
/// `report` is one.
pub(crate) fn finding(
    subject_listing: impl FnOnce() -> String,
    report: &IrReport,
) -> Option<String> {
    report.runs.mismatch.as_ref().map(|why| {
        format!(
            "{why}\n--- program ---\n{}--- runs ---\n{}",
            subject_listing(),
            report.describe()
        )
    })
}

/// The program as a person reads it: [`cove_ir::print`]'s listing where it
/// verified — the printer reads ids as freely as the machine does — and its
/// `Debug` form where it did not.
pub fn listing(program: &Program) -> String {
    match cove_ir::verify(program) {
        Ok(()) => cove_ir::print::program(program),
        Err(_) => format!("{program:#?}\n"),
    }
}

/// The `ir` target: a program [`crate::irgen`] builds from `data`, run in
/// every mode.
///
/// # Panics
///
/// On a finding: two modes that disagree, or a refused program the machine
/// ran — with the program and every run.
pub fn ir(data: &[u8]) {
    let (classification, finding) = ir_classified(data);
    crate::stats::record(&classification, finding.is_some());
    if let Some(why) = finding {
        panic!("{why}");
    }
}

/// [`ir`](fn@ir) without the panic: the input's classification, and the
/// message it would have panicked with if it is a finding. For `ir` itself and
/// for `cove-fuzz stats`.
pub fn ir_classified(data: &[u8]) -> (crate::stats::Classification, Option<String>) {
    let subject = crate::irgen::from_bytes(data);
    classified("ir", subject)
}

/// `subject` run in every mode, classified, with the finding's message.
fn classified(
    target: &'static str,
    subject: Subject,
) -> (crate::stats::Classification, Option<String>) {
    let listed = subject.program.clone();
    let front = crate::stats::Front {
        verified: false,
        has_entry: subject
            .program
            .function_named(&subject.module, &subject.name)
            .is_some(),
    };
    let report = run(subject);
    let finding = finding(|| listing(&listed), &report);
    (classify(target, &report, front), finding)
}

/// `report` classified for `target`.
pub(crate) fn classify(
    target: &'static str,
    report: &IrReport,
    front: crate::stats::Front,
) -> crate::stats::Classification {
    match &report.verified {
        Ok(()) => crate::stats::classify(
            target,
            &report.runs,
            crate::stats::Front {
                verified: true,
                ..front
            },
        ),
        Err(why) => crate::stats::Classification::verify_refused(
            target,
            why,
            Some(&report.runs),
            report.runs.mismatch.is_some(),
        ),
    }
}

/// The `ir_mutate` target: the lowered program `data` carries as an image
/// body, edited by [`crate::mutate`] with the rest of `data`, run in every
/// mode. Bytes that do not hold a readable body are no input at all.
///
/// # Panics
///
/// As [`ir`](fn@ir).
pub fn ir_mutate(data: &[u8]) {
    let (classification, finding) = ir_mutate_classified(data);
    crate::stats::record(&classification, finding.is_some());
    if let Some(why) = finding {
        panic!("{why}");
    }
}

/// [`ir_mutate`] without the panic; see [`ir_classified`].
pub fn ir_mutate_classified(data: &[u8]) -> (crate::stats::Classification, Option<String>) {
    match crate::mutate::from_bytes(data) {
        Some(subject) => classified("ir_mutate", subject),
        None => (crate::stats::Classification::unreadable("ir_mutate"), None),
    }
}

/// The subject a decoded image is: its entry, called with a plausible
/// argument for each parameter.
pub fn image_subject(image: cove_ir::serial::Image) -> Subject {
    let entry = image
        .program
        .function_named(&image.module, &image.name)
        .and_then(|id| image.program.functions.get(id.index()));
    let args = match entry {
        Some(function) => function
            .params
            .iter()
            .map(|layout| plausible_arg(&image.program, *layout))
            .collect(),
        None => Vec::new(),
    };
    Subject {
        program: image.program,
        sources: Arc::new(image.sources),
        module: image.module,
        name: image.name,
        args,
    }
}

/// The subject an `image` input decodes to, if it decodes.
pub fn image_input(data: &[u8]) -> Option<Subject> {
    cove_ir::serial::decode(&crate::with_header(data))
        .ok()
        .map(image_subject)
}

/// Bytes for the generator, from a fixed seed: an xorshift, so that the
/// inputs are the same on every run and every machine.
pub fn choices(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// The fixed input [`stats`] generates seed `seed` from.
pub fn stats_input(seed: u64) -> Vec<u8> {
    choices(seed, 64 + (seed as usize * 37) % 2048)
}

/// What the `ir` generator makes of `count` fixed inputs: how many verify,
/// which kinds of instruction appear in the programs that do, how their runs
/// ended, and any finding — for tuning the generator and for a report.
pub fn stats(count: u64) -> String {
    use std::collections::BTreeMap;
    let mut verified = 0u64;
    let mut refusals: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<crate::irgen::Kind, u64> = BTreeMap::new();
    let mut outcomes: BTreeMap<String, u64> = BTreeMap::new();
    let mut findings = Vec::new();
    // A panic is a finding to count, not the end of the count. The hook keeps
    // where the first panic of an input was raised, which is what tells two
    // findings apart.
    static RAISED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
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
    // A run that outlives every deadline is a finding of its own, and one the
    // loop below cannot report, because it never comes back: this watches it.
    static CURRENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    std::thread::spawn(|| {
        let mut last = (u64::MAX, std::time::Instant::now());
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let now = CURRENT.load(std::sync::atomic::Ordering::Relaxed);
            if now != last.0 {
                last = (now, std::time::Instant::now());
            } else if now != u64::MAX && last.1.elapsed() > std::time::Duration::from_secs(20) {
                println!("HANG: seed {now} has run for 20 seconds");
                std::process::exit(3);
            }
        }
    });
    for seed in 0..count {
        CURRENT.store(seed, std::sync::atomic::Ordering::Relaxed);
        if std::env::var_os("COVE_FUZZ_TRACE").is_some() {
            eprintln!("seed {seed}");
        }
        let data = stats_input(seed);
        let subject = crate::irgen::from_bytes(&data);
        let program = subject.program.clone();
        if let Ok(mut raised) = RAISED.lock() {
            *raised = None;
        }
        let report = match std::panic::catch_unwind(move || run(subject)) {
            Ok(report) => report,
            Err(_) => {
                let why = RAISED
                    .lock()
                    .ok()
                    .and_then(|r| r.clone())
                    .unwrap_or_default();
                findings.push(format!("panic at {why} (seed {seed})"));
                continue;
            }
        };
        match &report.verified {
            Ok(()) => {
                verified += 1;
                for function in &program.functions {
                    for inst in &function.code {
                        *kinds.entry(crate::irgen::kind_of(inst)).or_default() += 1;
                    }
                }
                if let Some(vm) = report.runs.observed.first() {
                    let mut key = vm.outcome.as_str().to_string();
                    if let Some(stop) = vm.stopped {
                        key = format!("{key} ({stop})");
                    } else if vm.outcome != cove_runtime::trace::RunOutcome::Success {
                        // The error's sentence, numbers and names out.
                        let first = vm.answer.lines().next().unwrap_or("");
                        let first: String = first
                            .chars()
                            .map(|c| if c.is_ascii_digit() { '#' } else { c })
                            .collect();
                        key = format!("{key}: {}", first.split('`').next().unwrap_or(""));
                    }
                    *outcomes.entry(key).or_default() += 1;
                }
            }
            Err(why) => {
                // The verifier's sentence up to its first backquote, so that
                // one rule is one row whatever it names.
                let rule = why.split(": ").nth(1).unwrap_or(why);
                let rule = rule.split('`').next().unwrap_or(rule).trim().to_string();
                *refusals.entry(rule).or_default() += 1;
            }
        }
        if let Some(why) = &report.runs.mismatch {
            findings.push(format!(
                "mismatch: {} (seed {seed})",
                why.lines().next().unwrap_or("")
            ));
        }
    }
    std::panic::set_hook(hook);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{verified} of {count} programs verify ({:.1}%)",
        100.0 * verified as f64 / count.max(1) as f64
    );
    let _ = writeln!(out, "how the verified ones ended on `vm`:");
    for (outcome, n) in &outcomes {
        let _ = writeln!(out, "  {n:6} {outcome}");
    }
    let _ = writeln!(out, "the most common refusals:");
    let mut rows: Vec<_> = refusals.into_iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    for (rule, n) in rows.iter().take(15) {
        let _ = writeln!(out, "  {n:6} {rule}");
    }
    let missing: Vec<_> = crate::irgen::KINDS
        .iter()
        .filter(|kind| !kinds.contains_key(kind))
        .collect();
    let _ = writeln!(
        out,
        "{} of {} kinds appear in a verified program; missing: {missing:?}",
        kinds.len(),
        crate::irgen::KINDS.len()
    );
    // One row per distinct finding — the panic's place, or the mismatch's
    // sentence — with how often it was met and the first seed that met it.
    let mut distinct: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for finding in &findings {
        let (what, seed) = finding.rsplit_once(" (seed ").unwrap_or((finding, ""));
        let key: String = what
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect();
        let entry = distinct
            .entry(key)
            .or_insert_with(|| (0, format!("{what} (seed {seed}")));
        entry.0 += 1;
    }
    let _ = writeln!(
        out,
        "{} findings, {} distinct",
        findings.len(),
        distinct.len()
    );
    for (n, first) in distinct.values() {
        let _ = writeln!(out, "  {n:5} {first}");
    }
    out
}
