//! The fuzz harnesses of [issue 681], as a library.
//!
//! Every harness a fuzzer drives is a function here, taking the fuzzer's bytes
//! and returning nothing. `fuzz/` is a cargo-fuzz crate whose targets are one
//! line each, calling into this crate; `tests/regressions.rs` calls the same
//! functions over the inputs committed under `fuzz/regressions/`. So a crash
//! a fuzzer found is replayed on stable Rust by `cargo t`, under the same
//! harness that found it, and nothing about the replay depends on nightly or
//! on cargo-fuzz being installed. [ADR 0112] records that split.
//!
//! # What a finding is
//!
//! Each harness hands its input to the toolchain the way `cove run` would and
//! **discards every answer the toolchain is allowed to give**:
//!
//! - a source that does not parse, does not check, or does not lower is a
//!   list of diagnostics, which is the toolchain refusing a program;
//! - a run that stops — a guest trap such as an overflow, a division by zero
//!   or an out-of-range `sliceBytes`, an out-of-memory refusal from the
//!   heap's capacity, the deadline, the call-depth or host-call limit, a
//!   refused host call — is an `Err(RuntimeError)`, which is a program
//!   stopping;
//! - an image that does not read is an [`Unreadable`](cove_ir::serial::Unreadable).
//!
//! What is left is a finding: a Rust panic (including a debug assertion and an
//! overflow check, both of which the fuzz build keeps on), an abort, a stack
//! overflow, a sanitizer report, or a run that outlives the fuzzer's own
//! `-timeout` — which is set well above [`DEADLINE`], so the only way to reach
//! it is a loop the runtime's deadline cannot see.
//!
//! The one exception runs the other way: [`structured`] generates programs
//! that are well typed by construction, so a generated program the front end
//! refuses is a bug — in the generator, or in the checker — and panics with the
//! program's text.
//!
//! The differential harnesses, [`differential`](fn@differential) and
//! [`differential_source`] (issue 681's priority 2; see the
//! [`differential`](mod@differential) module), add a second kind of finding:
//! two ways of running one program that answer differently.
//!
//! # Bytes and text
//!
//! The source harnesses read their bytes with [`std::str::from_utf8`] and
//! return at once on anything that is not UTF-8. Every way a source reaches the
//! front end — a file `cove` reads, a string the playground passes — is
//! already a `String`, so a byte sequence that is not UTF-8 is not an input the
//! front end can be given; and a lossy conversion would map every malformed
//! sequence to the same U+FFFD and spend the fuzzer's effort on one input many
//! times over.
//!
//! # What a run may touch
//!
//! A run is granted `console` and nothing else, and its console writes into
//! [`std::io::sink`] (the differential harnesses write into buffers, and grant
//! the virtual clock as well). No file, network, process, environment, database or
//! clock host is registered, so a call into any of them is refused by the
//! registry; and the grant source is sealed, so a refusal does not point at a
//! `cove.toml`. The deadline is the only wall-clock dependence, and it can only
//! turn a long run into a stopped one.
//!
//! [issue 681]: https://github.com/myuon/cove/issues/681
//! [ADR 0112]: ../../../docs/adr/0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use cove_diag::SourceMap;
use cove_runtime::{
    Budget, Cancellation, Console, GrantSource, Grants, HostRegistry, Limits, OwnedVm,
    PreparedProgram, Runtime,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

pub mod differential;
pub mod generate;
pub mod ir;
pub mod irgen;
pub mod mutate;
pub mod stats;

pub use differential::{differential, differential_source};
pub use ir::{ir, ir_mutate};

/// The module a fuzzed source is compiled as, and the function that is run.
///
/// The playground's names, so a fuzzed program is the shape of a program a
/// reader pastes into the page: one file, one module, `main` its entry.
pub const MODULE: &str = "playground";
/// The entry function.
pub const ENTRY: &str = "main";
const PATH: &str = "playground/main.cove";

/// The wall-clock time one run may take before the runtime stops it.
///
/// A guest is allowed to loop forever, and this is what makes that an outcome
/// rather than a hang: the run answers `Err` with the deadline's stop. The
/// fuzzer's own `-timeout` (see `scripts/fuzz.sh`) is several seconds, an
/// order of magnitude above this, so it fires only on a loop that does not
/// reach a safepoint.
pub const DEADLINE: Duration = Duration::from_millis(150);

/// The words a run's heap may grow to: two mebibytes.
///
/// Small, so that a program asking for a huge `Vector.filled` or a huge
/// `StringBuilder.withCapacity` meets the heap's out-of-memory refusal inside
/// the fuzzer's `-rss_limit_mb` rather than the fuzzer's own memory limit.
pub const HEAP_WORDS: usize = 1 << 18;

/// The deepest a call may nest, so that a deep recursion is stopped by a
/// limit the host chose and not only by the VM's stack region running out —
/// both are `Err` outcomes, and both are exercised.
pub const MAX_CALL_DEPTH: usize = 512;

/// Host calls one run may make: enough for a corpus program's printing, few
/// enough that a loop that prints is stopped by this rather than spending
/// the deadline on the sink.
pub const MAX_HOST_CALLS: u64 = 4096;

/// The limits every fuzzed run is held to.
pub fn limits() -> Limits {
    Limits {
        deadline: Some(DEADLINE),
        max_host_calls: Some(MAX_HOST_CALLS),
        max_call_depth: Some(MAX_CALL_DEPTH),
        // A task is a thread. A fuzzed run gets none, as a playground run
        // gets none; `spawn` is refused by the limit.
        max_tasks: Some(0),
    }
}

/// Every target, by the name `fuzz/` and `fuzz/regressions/` give it.
pub const TARGETS: [&str; 9] = [
    "parse",
    "compile",
    "run",
    "structured",
    "image",
    "differential",
    "differential_source",
    "ir",
    "ir_mutate",
];

/// The harness a target name names.
pub fn harness(target: &str) -> Option<fn(&[u8])> {
    Some(match target {
        "parse" => parse,
        "compile" => compile,
        "run" => run,
        "structured" => structured,
        "image" => image,
        "differential" => differential,
        "differential_source" => differential_source,
        "ir" => ir,
        "ir_mutate" => ir_mutate,
        _ => return None,
    })
}

/// Lexes and parses `data`.
pub fn parse(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let text = text.to_string();
    on_stack(move || {
        let mut sources = SourceMap::new();
        let file = sources.add(PathBuf::from(PATH), text);
        let _ = cove_syntax::parse_file(&sources, file);
    });
}

/// Parses, checks and lowers `data` as the one file of a package, and runs
/// nothing.
pub fn compile(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let text = text.to_string();
    on_stack(move || {
        let _ = front(&text);
    });
}

/// Compiles `data` and, if it lowers, runs it: on the encoded VM, and then on
/// the native tier where this build and host have one.
///
/// The two runs are not compared here; each is only required not to crash.
/// [`differential_source`] is the same input with the runs compared.
pub fn run(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let text = text.to_string();
    on_stack(move || {
        if let Ok(lowered) = front(&text) {
            execute(lowered);
        }
    });
}

/// Generates a well-typed program from `data`, compiles it, and runs it on
/// both tiers.
///
/// # Panics
///
/// If the generated program does not check or does not lower, with its text:
/// that is a generator bug or a front-end bug, and either is worth a report.
pub fn structured(data: &[u8]) {
    let program = generate::from_bytes(data);
    on_stack(move || match front(&program) {
        Ok(lowered) => {
            execute(lowered);
        }
        Err(why) => panic!("a generated program was refused:\n{why}\n--- program ---\n{program}"),
    });
}

/// Reads `data` as the body of a lowered-IR image with a valid header, and
/// runs what it reads if it verifies.
///
/// The header — [`MAGIC`](cove_ir::serial::MAGIC), this build's
/// [`FINGERPRINT`](cove_ir::serial::FINGERPRINT) and the body's length — is
/// supplied, because a fuzzer would otherwise spend its whole budget learning
/// a 64-bit hash and every input would stop at the fingerprint check.
///
/// `serial::decode` refuses bytes it can recognise as wrong and promises never
/// to panic, and it does not check what the ids in an accepted image mean.
/// `cove_ir::verify` is the check that does, and it promises to answer for any
/// program `decode` returns with an `Err`, never a panic
/// ([ADR 0117](../../docs/adr/0117-verify-refuses-a-program-whose-ids-are-out-of-range.md)).
/// Since [ADR 0120](../../docs/adr/0120-the-vm-runs-only-what-verifies.md) the
/// machine's contract covers what `verify` accepts, so a decoded program that
/// verifies is run, in every mode, as the `ir` target runs one
/// ([`ir::run`]): its entry with a plausible argument for each parameter.
///
/// # Panics
///
/// On a finding, as [`ir`](fn@ir) does.
pub fn image(data: &[u8]) {
    let bytes = with_header(data);
    on_stack(move || {
        let Ok(image) = cove_ir::serial::decode(&bytes) else {
            return;
        };
        if cove_ir::verify(&image.program).is_err() {
            return;
        }
        let subject = ir::image_subject(image);
        let listed = subject.program.clone();
        let report = ir::run_here(subject);
        ir::require(|| ir::listing(&listed), &report);
    });
}

/// `data` behind a header this build's `decode` accepts.
pub(crate) fn with_header(data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(cove_ir::serial::HEADER_BYTES + data.len());
    bytes.extend_from_slice(&cove_ir::serial::MAGIC);
    bytes.extend_from_slice(&cove_ir::serial::FINGERPRINT.to_le_bytes());
    bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
    bytes.extend_from_slice(data);
    bytes
}

/// The body of the image `source` lowers to — what [`image`] takes — or
/// `None` if it does not lower. For seeding the `image` target with bodies
/// the decoder accepts.
pub fn image_body(source: &str) -> Option<Vec<u8>> {
    let source = source.to_string();
    on_stack(move || {
        let lowered = front(&source).ok()?;
        let bytes =
            cove_ir::serial::encode(&lowered.program, &lowered.sources, MODULE, ENTRY).ok()?;
        Some(bytes[cove_ir::serial::HEADER_BYTES..].to_vec())
    })
}

/// Checks and lowers `source` as [`compile`] does, answering the rendered
/// diagnostics if it is refused. For a person replaying an input, and for the
/// generator's tests.
pub fn diagnose(source: &str) -> Result<(), String> {
    let source = source.to_string();
    on_stack(move || front(&source).map(|_| ()))
}

/// A source checked and lowered, with what a run of it needs.
struct Lowered {
    sources: SourceMap,
    checked: cove_sema::resolve::Program,
    program: cove_ir::Program,
}

/// The step of [`front`] that refused a source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontStage {
    /// The lexer or the parser.
    Parse,
    /// Attaching the standard library, name resolution, type and uniqueness
    /// checking.
    Check,
    /// Lowering `playground.main` to IR.
    Lower,
}

/// `cove run`'s front end over one file: parse, attach the standard library,
/// check against the shipped host schemas, lower `playground.main`. The
/// error is the diagnostics' messages, one a line.
fn front(text: &str) -> Result<Lowered, String> {
    front_staged(text).map_err(|(_, why)| why)
}

/// [`front`], saying which step refused: for the statistics of
/// [`stats`], which count how far each input got.
fn front_staged(text: &str) -> Result<Lowered, (FrontStage, String)> {
    let mut sources = SourceMap::new();
    let path = PathBuf::from(PATH);
    let file = sources.add(path.clone(), text.to_string());
    let ast = cove_syntax::parse_file(&sources, file)
        .map_err(|d| (FrontStage::Parse, rendered(&sources, d)))?;
    let mut modules = BTreeMap::new();
    modules.insert(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    );
    let std_modules = match cove_sema::stdlib::attach(&mut sources) {
        Ok(std_modules) => std_modules,
        Err(diagnostics) => return Err((FrontStage::Check, rendered(&sources, diagnostics))),
    };
    for (name, module) in std_modules {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    let checked = Compiler::new()
        .with_schemas(HostSchemas::new())
        .compile(&package)
        .map_err(|d| (FrontStage::Check, rendered(&sources, d)))?;
    let program = cove_ir::lower_entry(&checked, &sources, &HostSchemas::new(), MODULE, ENTRY)
        .map_err(|d| (FrontStage::Lower, rendered(&sources, d)))?;
    Ok(Lowered {
        sources,
        checked,
        program,
    })
}

/// Diagnostics as `cove check` prints them.
fn rendered(sources: &SourceMap, diagnostics: Vec<cove_diag::Diagnostic>) -> String {
    diagnostics
        .iter()
        .map(|d| cove_diag::render(sources, d))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs a lowered program on the encoded VM and then, where there is one, on
/// the native tier, each with a fresh registry and a fresh budget.
///
/// Answers how each run ended, one line a tier, for [`explain`]; the
/// harnesses discard it.
fn execute(lowered: Lowered) -> Vec<String> {
    let checked = Arc::new(lowered.checked);
    let sources = Arc::new(lowered.sources);
    let program = Arc::new(lowered.program);

    let mut outcomes = vec![format!(
        "vm: {}",
        run_once(
            &checked,
            &sources,
            PreparedProgram::new(Arc::clone(&program))
        )
    )];
    // `Unavailable` is this host or build having no native tier, which is
    // not a finding: the VM run above is the whole test there.
    match PreparedProgram::new(Arc::clone(&program)).with_native() {
        Ok(native) => outcomes.push(format!("native: {}", run_once(&checked, &sources, native))),
        Err(why) => outcomes.push(format!("native: unavailable ({why})")),
    }
    outcomes
}

/// How `source` ends on each tier, under the harnesses' limits: `ok`, or the
/// runtime error's outcome and message, or the front end's refusal. For
/// triage and for tuning the generator — a generator whose programs all stop
/// on the first statement is reaching nothing.
pub fn explain(source: &str) -> Vec<String> {
    let source = source.to_string();
    on_stack(move || match front(&source) {
        Ok(lowered) => execute(lowered),
        Err(why) => vec![format!("refused: {why}")],
    })
}

/// One run of `prepared` under [`limits`], described in a line.
fn run_once(
    checked: &Arc<cove_sema::resolve::Program>,
    sources: &Arc<SourceMap>,
    prepared: PreparedProgram,
) -> String {
    let mut hosts = HostRegistry::new(Grants::new(["console"]));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.set_grant_source(GrantSource::Sealed);
    // The deadline's clock starts here, so the budget is made after the
    // native tier has compiled and just before the run.
    hosts.set_budget(Budget::with_cancellation(limits(), Cancellation::new()));
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(Runtime::new(
        Arc::clone(checked),
        Arc::clone(sources),
        Arc::clone(&hosts),
    ));
    let mut vm = OwnedVm::with_heap_words(runtime, hosts, prepared, HEAP_WORDS);
    match vm.run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new()) {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{} — {}", error.outcome.as_str(), error.message),
    }
}

/// Runs `body` on the stack `cove` runs a whole command on.
///
/// `cove`'s `main` runs everything — the front end included — on a thread of
/// [`cove_runtime::STACK_SIZE`], and the front end's nesting limits are
/// calibrated against that stack. A harness on libFuzzer's main thread would
/// have a different stack and so report overflows `cove` cannot have, or miss
/// ones it can.
///
/// A thread per input, as `cove` has one per command. Keeping one worker
/// thread for every input instead was tried and measured no faster under
/// AddressSanitizer — 256 against 299 inputs a second on `parse`, inside the
/// noise — because what an input costs is the size of the source, not the
/// thread.
///
/// A panic on that thread is re-raised on this one, so a test replaying an
/// input sees it as its own; under libFuzzer the panic hook aborts first.
fn on_stack<T: Send>(body: impl FnOnce() -> T + Send) -> T {
    match cove_runtime::on_cove_stack(body) {
        Ok(answer) => answer,
        Err(why) => panic!("the runtime could not start a thread to run on: {why}"),
    }
}
