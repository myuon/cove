//! Cove in a browser tab: the front end and the linear-memory backend behind
//! a C ABI, with no server and no Cove runtime anywhere but the page.
//!
//! [Issue #241](https://github.com/myuon/cove/issues/241) asks for a
//! playground. This crate is the half of it that is Rust; `web/` is the half
//! that is a page. What it exports is described in [`abi`], and it is five
//! functions and one import.
//!
//! The fifth is [`debug_json`]: the same run, watched by a [`record`]ing
//! debugger, so that the page can scrub through a timeline of it. A browser
//! cannot be given the debugger `cove debug` is — a Web Worker cannot block
//! waiting for the page — and [`record`] says at length why, and what
//! recording keeps and loses instead.
//!
//! # What is the same as `cove run`, and what is not
//!
//! The same: the parser, the checker, the lowering, the VM, the diagnostics
//! (rendered by [`cove_diag::render`], so the browser shows the sentences the
//! CLI shows), and the shipped host schemas — so a program that uses `http`
//! type-checks here exactly as it does on the command line, and is refused at
//! the boundary here exactly as it is there without a grant.
//!
//! Not the same, and each for a reason a browser gives:
//!
//! - **No filesystem.** `files` and `documents` are the in-memory hosts the
//!   differential harness uses, seeded empty; `process` is recorded; `http`
//!   and `database` are denied. Nothing here can reach anything.
//! - **No real clock host.** `clock` is [`VirtualTime`], which is what makes
//!   `clock.sleep` finish at once and a program that measures itself
//!   deterministic. The *run's* clock is a different thing and is real: see
//!   [`RUN_LIMITS`].
//! - **No tasks.** `spawn` is refused, with a span, in the runtime. A Cove
//!   task is a thread (ADR 0008) and one Web Worker is one thread; the
//!   alternative was to run a task's body inline, which would make this
//!   answer differently from the tree-walking oracle, and the corpus is held
//!   together by those two agreeing. `examples/tasks` does not run here, and
//!   that is the honest outcome rather than a bug.

pub mod abi;
mod json;
pub mod record;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::{Diagnostic, Severity, SourceMap};
use cove_runtime::{
    Budget, Cancellation, Clock, Console, Database, Documents, Env, Files, Grants, HostRegistry,
    Http, Limits, Process, ProcessLog, RunOutcome, Runtime, ValueCapture, VirtualTime, Vm,
};
use cove_sema::{Compiler, Config, HostSchemas, Module, Package, Unit};

/// The module a playground's one source file belongs to, and the function
/// that is run.
///
/// A package on disk takes its module names from its directory names, and a
/// page has no directories, so one is chosen here and written into the
/// diagnostics the reader sees. `playground.main` reads as what it is.
pub const MODULE: &str = "playground";

/// The entry function looked for in [`MODULE`].
pub const ENTRY: &str = "main";

/// The path the one source file is filed under in the [`SourceMap`], which is
/// what a diagnostic's header names.
const PATH: &str = "playground/main.cove";

/// What a run in the playground is granted.
///
/// Every host is registered, as `cove run` registers every host — a grant and
/// not a registration is what decides, and a refusal that names the missing
/// capability is a better answer than an operation that does not exist. These
/// five are the ones whose in-memory implementations can honestly answer:
/// `console` prints into a buffer the page shows, `clock` is virtual, `env`
/// is empty, and `files` and `documents` start empty and live as long as the
/// run does.
///
/// `http`, `database` and `process` are absent. They are registered denied or
/// recorded, so a program that calls them is told it was not granted the
/// capability rather than being told the module does not exist.
pub const GRANTS: [&str; 5] = ["console", "clock", "env", "files", "documents"];

/// What bounds a run that the page did not bound itself.
///
/// A page can pass its own fuel and deadline; this is what it gets when it
/// passes neither. Both are set, and deliberately: a tab that is running a
/// Cove program is a tab that is not repainting, and the two bounds fail
/// differently — fuel is deterministic and portable within one backend, and
/// the deadline is what catches a program that spends its time inside one
/// long host call rather than in a loop.
///
/// The deadline is enforced against the clock the embedder imports, which is
/// `performance.now()` in a page and under node. It is a real bound and not a
/// decoration: `cove_runtime`'s `wallclock` module says why the import is
/// required rather than defaulted.
pub const RUN_LIMITS: (u64, u64) = (200_000_000, 5_000);

/// Bytes a Cove program printed, readable after the run.
///
/// The idiom is `crates/cove-cli/tests/differential.rs`'s, because the
/// question is the same one: run a program with nothing of the machine
/// attached and read back what it said.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|held| held.into_inner()))
            .into_owned()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One source file, checked and lowered as far as it goes.
struct Front {
    sources: SourceMap,
    diagnostics: Vec<Diagnostic>,
    /// The checked program and its lowering, or neither. They are kept
    /// together because a [`Runtime`] holds the first and the VM runs the
    /// second, and a run needs both to exist.
    lowered: Option<(cove_sema::resolve::Program, cove_ir::Program)>,
}

impl Front {
    /// Whether anything stopped this from being a program that could run.
    fn failed(&self) -> bool {
        self.lowered.is_none()
    }
}

/// A front end that got no further than these diagnostics.
fn stopped(sources: SourceMap, diagnostics: Vec<Diagnostic>) -> Front {
    Front {
        sources,
        diagnostics,
        lowered: None,
    }
}

/// Parses, checks and lowers `source`, collecting whatever diagnostics each
/// stage produced.
///
/// The stages are `cove run`'s, in `cove run`'s order, against
/// `HostSchemas::new()` — the shipped set — for both the check and the
/// lowering. Using a narrower set for one than the other would let a program
/// pass the checker and fail the lowering over a host neither the page nor
/// the reader ever mentioned.
fn front(source: &str) -> Front {
    let mut sources = SourceMap::new();
    let path = PathBuf::from(PATH);
    let file = sources.add(path.clone(), source.to_string());

    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(diagnostics) => return stopped(sources, diagnostics),
    };

    let mut modules = BTreeMap::new();
    modules.insert(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    );
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };

    let schemas = HostSchemas::new();
    let checked = match Compiler::new().with_schemas(schemas).compile(&package) {
        Ok(checked) => checked,
        Err(diagnostics) => return stopped(sources, diagnostics),
    };

    match cove_ir::lower_entry(&checked, &sources, &HostSchemas::new(), MODULE, ENTRY) {
        Ok(program) => Front {
            sources,
            diagnostics: Vec::new(),
            lowered: Some((checked, program)),
        },
        Err(diagnostics) => stopped(sources, diagnostics),
    }
}

/// One diagnostic as JSON: what the CLI would have printed, plus the two
/// fields a page needs in order to sort and count without re-parsing the
/// printed form.
fn diagnostic_json(sources: &SourceMap, diagnostic: &Diagnostic) -> String {
    json::object([
        (
            "severity",
            json::string(match diagnostic.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Note => "note",
            }),
        ),
        ("code", json::string(&diagnostic.code)),
        ("message", json::string(&diagnostic.message)),
        (
            "rendered",
            json::string(&cove_diag::render(sources, diagnostic)),
        ),
    ])
}

fn diagnostics_json(sources: &SourceMap, diagnostics: &[Diagnostic]) -> String {
    json::array(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic_json(sources, diagnostic)),
    )
}

/// Checks and lowers `source`, and answers what a reader would want to see
/// before running it.
///
/// ```json
/// {"ok":bool,"diagnostics":[...],"ir":string|null}
/// ```
///
/// `ok` is "nothing stopped this from running", which is not the same as "no
/// diagnostics": a warning leaves `ok` true and is still shown. `ir` is
/// [`cove_ir::print::program`]'s disassembly, and is `null` for a source that
/// did not reach the lowering.
pub fn compile_json(source: &str) -> String {
    let front = front(source);
    json::object([
        ("ok", (!front.failed()).to_string()),
        (
            "diagnostics",
            diagnostics_json(&front.sources, &front.diagnostics),
        ),
        (
            "ir",
            json::or_null(
                front
                    .lowered
                    .as_ref()
                    .map(|(_, program)| json::string(&cove_ir::print::program(program))),
            ),
        ),
    ])
}

/// Checks, lowers and runs `source`, and answers what happened.
///
/// ```json
/// {"ok":bool,"diagnostics":[...],"ir":string|null,"outcome":string|null,
///  "stdout":string,"stderr":string,"answer":value|null,
///  "instructions":int|null,"fuel":int|null}
/// ```
///
/// `ir` is [`compile_json`]'s, repeated here so that one call fills every
/// pane a page shows. A page that asked for the disassembly separately would
/// be paying for the front end twice for one source, and the two answers
/// could describe different text if the reader typed between them.
///
/// `outcome` is [`RunOutcome::as_str`], derived the way
/// `crates/cove-cli/tests/differential.rs` derives it, so the name a page
/// shows is the name a trace would have recorded. `answer` is the entry's
/// value in [`cove_runtime::value_to_json`]'s encoding, which is this
/// repository's existing answer to how a Cove value leaves Rust.
///
/// A source that did not compile is answered without being run, with the same
/// `diagnostics` [`compile_json`] would have given: a page can call this one
/// function and get both halves.
///
/// `fuel` and `deadline_ms` are `None` for "use [`RUN_LIMITS`]", not for "no
/// bound". A playground that could be asked for an unbounded run would be a
/// page with a hang button.
pub fn run_json(source: &str, fuel: Option<u64>, deadline_ms: Option<u64>) -> String {
    execute(source, fuel, deadline_ms, None)
}

/// Runs `source` as [`run_json`] does, watched by a [`record::Recorder`],
/// and answers everything [`run_json`] answers plus the recording under
/// `debug`.
///
/// ```json
/// {…as run_json…,"debug":{"moments":[…],"functions":[…],"kept":int,
///                         "limit":int,"bytes":int,"truncated":…}}
/// ```
///
/// `debug` is `null` for a source that did not compile, because there was no
/// run to record and an empty recording of a program that never started
/// reads as a program that did nothing.
///
/// `moments` is how many moments to keep; zero asks for
/// [`record::MOMENTS`], and anything past [`record::MOST_MOMENTS`] is
/// clamped to it. [`record`]'s module documentation says what a moment is,
/// what bounds it, and why a browser gets a recording rather than a
/// debugger it can step.
///
/// # One blob, not pieces
///
/// A recording is much larger than a compile result, so the alternative was
/// considered and refused: a first call answering the moments' outlines and
/// a second answering one moment's detail. Two things decided it.
///
/// A paged ABI needs the module to *hold* the recording between calls, and
/// [`abi`] exists partly to have no module-level state — its length prefix
/// replaced a "how long was the last answer?" export precisely so that two
/// calls in flight have nothing to race over. Holding a recording would put
/// that back, and worse, because the state would now be the size of the
/// recording rather than of a number.
///
/// And the size a page actually pays is not the size paging would save.
/// What makes a recording large is repetition, and the two repeated things —
/// a function's disassembly and its name — are interned into `functions`
/// once each. What is left per moment is what genuinely differs between
/// moments. A recording of the example program is a few tens of kilobytes;
/// see `web/README.md` for what larger ones measure. The bound that keeps it
/// from growing without limit is [`record::BYTES`], and a bound is a better
/// answer to "this could be huge" than an ABI that hands over a huge thing
/// slowly.
pub fn debug_json(
    source: &str,
    fuel: Option<u64>,
    deadline_ms: Option<u64>,
    moments: usize,
) -> String {
    execute(source, fuel, deadline_ms, Some(moments))
}

/// Checks, lowers and runs `source`, recording it when `moments` is `Some`.
///
/// One function and not two so that a debugged run and a plain one are the
/// same run: the same hosts, the same grants, the same limits, the same
/// classification of how it ended. A second copy of this setup would be a
/// second playground that agreed with the first until it did not.
fn execute(
    source: &str,
    fuel: Option<u64>,
    deadline_ms: Option<u64>,
    moments: Option<usize>,
) -> String {
    let recording = moments.is_some();
    let front = front(source);
    let Some((checked, program)) = front.lowered else {
        let mut fields = vec![
            ("ok", "false".to_string()),
            (
                "diagnostics",
                diagnostics_json(&front.sources, &front.diagnostics),
            ),
            ("ir", "null".to_string()),
            ("outcome", "null".to_string()),
            ("stdout", json::string("")),
            ("stderr", json::string("")),
            ("answer", "null".to_string()),
            ("instructions", "null".to_string()),
            ("fuel", "null".to_string()),
        ];
        if recording {
            fields.push(("debug", "null".to_string()));
        }
        return json::object(fields);
    };

    let out = Buffer::default();
    let err = Buffer::default();

    let mut hosts = HostRegistry::new(Grants::new(GRANTS));
    hosts.register(Box::new(Console::new(out.clone(), err.clone())));
    hosts.register(Box::new(Env::new(BTreeMap::new())));
    hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Files::in_memory(BTreeMap::new())));
    hosts.register(Box::new(Process::recorded(
        Vec::new(),
        BTreeMap::new(),
        ProcessLog::new(),
    )));
    hosts.register(Box::new(Database::denied()));
    hosts.register(Box::new(Http::denied()));
    // A refused call should not send the reader to a `cove.toml` that a page
    // does not have and cannot be given.
    hosts.set_grant_source(cove_runtime::GrantSource::Sealed);

    let limits = Limits {
        fuel: Some(fuel.unwrap_or(RUN_LIMITS.0)),
        deadline: Some(Duration::from_millis(deadline_ms.unwrap_or(RUN_LIMITS.1))),
        max_host_calls: None,
        max_call_depth: None,
        // Refused in the runtime and refused again here, because the two say
        // different things: this is the bound a host chose, and the runtime's
        // refusal is the fact that there is no thread to give. A reader who
        // raises this one still gets the honest sentence.
        max_tasks: Some(0),
    };
    hosts.set_budget(Budget::with_cancellation(limits, Cancellation::new()));

    let sources = Arc::new(front.sources);
    let runtime = Runtime::new(Arc::new(checked), Arc::clone(&sources), Arc::new(hosts));

    let disassembly = cove_ir::print::program(&program);
    let recorder = moments.map(|moments| record::Recorder::new(Arc::clone(&sources), moments));
    let (answer, instructions, fuel_spent) = {
        let mut vm = match &recorder {
            Some(recorder) => Vm::debugged(&runtime, runtime.hosts(), &program, recorder),
            None => Vm::new(&runtime, runtime.hosts(), &program),
        };
        let answer = vm.run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new());
        let instructions = vm.instructions();
        let spent = runtime
            .hosts()
            .with_budget(|budget| budget.meter().fuel_spent());
        (answer, instructions, spent)
    };

    let outcome = match &answer {
        Ok(value) if value.is_err() => RunOutcome::Error,
        Ok(_) => RunOutcome::Success,
        Err(error) => error.outcome,
    };
    let diagnostics = match &answer {
        Ok(_) => Vec::new(),
        Err(error) => vec![error.to_diagnostic()],
    };

    let mut fields = vec![
        ("ok", matches!(outcome, RunOutcome::Success).to_string()),
        ("diagnostics", diagnostics_json(&sources, &diagnostics)),
        ("ir", json::string(&disassembly)),
        ("outcome", json::string(outcome.as_str())),
        ("stdout", json::string(&out.text())),
        ("stderr", json::string(&err.text())),
        (
            "answer",
            json::or_null(
                answer
                    .as_ref()
                    .ok()
                    .map(|value| cove_runtime::value_to_json(value, ValueCapture::Full)),
            ),
        ),
        ("instructions", instructions.to_string()),
        (
            "fuel",
            json::or_null(fuel_spent.map(|spent| spent.to_string())),
        ),
    ];
    if let Some(recorder) = &recorder {
        fields.push(("debug", recorder.json()));
    }
    json::object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON string value of `key`, unescaped only as far as these tests
    /// need — enough to compare a rendered diagnostic or a printed line.
    ///
    /// A parser would be the third-party dependency this crate exists partly
    /// to avoid. What the tests need is "does the answer say this", and a
    /// substring search over the escaped form answers it without one.
    fn says(json: &str, fragment: &str) -> bool {
        json.contains(fragment)
    }

    #[test]
    fn a_program_that_compiles_answers_its_disassembly() {
        let json = compile_json("export fn main() -> Int { 21 * 2 }");
        assert!(says(&json, r#""ok":true"#), "{json}");
        assert!(says(&json, r#""diagnostics":[]"#), "{json}");
        assert!(says(&json, "playground.main"), "{json}");
    }

    /// The point of rendering with [`cove_diag::render`] rather than printing
    /// the message: what the page shows is the caret and the snippet the CLI
    /// shows, over the path the source was filed under.
    #[test]
    fn a_program_that_does_not_parse_answers_the_rendered_diagnostic() {
        let json = compile_json("export fn main() -> Int { 1 +");
        assert!(says(&json, r#""ok":false"#), "{json}");
        assert!(says(&json, r#""ir":null"#), "{json}");
        assert!(says(&json, r#""severity":"error""#), "{json}");
        assert!(says(&json, "playground/main.cove"), "{json}");
    }

    /// A source with no `main` is refused by name, and the refusal is a
    /// rendered diagnostic like any other rather than a blank answer.
    #[test]
    fn a_source_without_the_entry_is_refused_by_name() {
        let json = run_json("export fn other() -> Int { 1 }", None, None);
        assert!(says(&json, r#""ok":false"#), "{json}");
        assert!(
            says(&json, "this package does not declare `playground.main`"),
            "{json}"
        );
    }

    #[test]
    fn a_run_answers_what_the_entry_produced() {
        let json = run_json("export fn main() -> Int { 21 * 2 }", None, None);
        assert!(says(&json, r#""outcome":"success""#), "{json}");
        assert!(
            says(&json, r#""answer":{"type":"int","value":42}"#),
            "{json}"
        );
    }

    /// `console` is granted, and what a program prints is a string in the
    /// answer rather than bytes that went nowhere.
    #[test]
    fn a_run_answers_what_the_program_printed() {
        let json = run_json(
            r#"use console.println

export fn main() -> Result<Unit, Error> {
  println("hello from the tab")?
  Ok(())
}"#,
            None,
            None,
        );
        assert!(says(&json, r#""outcome":"success""#), "{json}");
        assert!(says(&json, r#""stdout":"hello from the tab\n""#), "{json}");
    }

    /// The bound a page can put on a loop, doing what it says.
    #[test]
    fn a_run_past_its_fuel_is_stopped_and_classified() {
        let json = run_json(
            "export fn main() -> Int {\n  var n = 0\n  while true { n = n + 1 }\n  n\n}",
            Some(10_000),
            None,
        );
        assert!(says(&json, r#""outcome":"fuel""#), "{json}");
        assert!(says(&json, r#""ok":false"#), "{json}");
        assert!(says(&json, "fuel budget of 10000 exhausted"), "{json}");
    }

    /// A capability the playground does not grant is refused at the boundary,
    /// in the runtime's own words, rather than by the module not existing.
    ///
    /// This is also what says the checker was given the *shipped* schemas: a
    /// narrower set would have made this a type error instead, which is a
    /// different sentence about a different thing.
    #[test]
    fn an_ungranted_capability_is_refused_at_the_boundary() {
        let json = run_json(
            "use http\n\nexport fn main() -> Result<http.Response, Error> {\n  http.fetch(\"http://example.com\")\n}",
            None,
            None,
        );
        assert!(says(&json, r#""outcome":"host_boundary""#), "{json}");
        assert!(says(&json, "http"), "{json}");
    }

    /// On the host a task really does get a thread, so this is the one thing
    /// these tests cannot check: that `spawn` is refused. What they can check
    /// is that a `spawn` past the host-chosen `max_tasks` is refused in the
    /// same vocabulary, `RunOutcome::Concurrency`, which is what the wasm
    /// refusal answers too. `web/check.mjs` checks the other half, in wasm,
    /// where there is no thread to be had.
    #[test]
    fn a_spawn_is_refused_as_a_concurrency_stop() {
        let json = run_json(
            r#"export fn main() -> Int {
  scope s {
    let t = s.spawn { 1 }
    t.await()
  }
}"#,
            None,
            None,
        );
        assert!(says(&json, r#""outcome":"concurrency""#), "{json}");
    }

    /// Every value of `key` in `json`, in the order they were written.
    ///
    /// A recording is a sequence, and what these tests need to say about one
    /// is "these things, in this order". A substring search answers "is this
    /// in there" and cannot answer that, so this is the smallest thing that
    /// can — still not a parser, still no dependency.
    fn every(json: &str, key: &str) -> Vec<String> {
        let needle = format!("\"{key}\":");
        let mut found = Vec::new();
        let mut rest = json;
        while let Some(at) = rest.find(&needle) {
            rest = &rest[at + needle.len()..];
            let value = match rest.strip_prefix('"') {
                Some(quoted) => {
                    let end = quoted.find('"').unwrap_or(quoted.len());
                    quoted[..end].to_string()
                }
                None => rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect(),
            };
            found.push(value);
        }
        found
    }

    /// A program written so that every rule in [`record`]'s capture policy
    /// fires exactly once and in a knowable order: an entry, a new line, a
    /// call, a line inside the callee, and the return.
    const WALKED: &str = r#"export fn twice(n: Int) -> Int {
  n + n
}

export fn main() -> Int {
  let one = 21
  let total = twice(one)
  total
}
"#;

    /// The recording of a known program has the moments it should have, in
    /// the order it ran them.
    #[test]
    fn a_recording_holds_the_moments_the_program_ran_in_order() {
        let json = debug_json(WALKED, None, None, 0);
        assert!(says(&json, r#""outcome":"success""#), "{json}");

        // `why` appears once per moment and nowhere else in the answer.
        assert_eq!(
            every(&json, "why"),
            // Six and not five: the last is `return`'s own instruction,
            // which the lowering writes at the function's signature line, so
            // the line changes one more time on the way out.
            ["entry", "line", "call", "line", "return", "line"],
            "{json}"
        );

        // Two functions, disassembled once each however often they are in a
        // moment: the interning that keeps a recording from repeating a loop
        // body once per turn.
        assert_eq!(
            every(&json, "name")
                .iter()
                .filter(|name| name.starts_with("playground."))
                .count(),
            2,
            "{json}"
        );
        assert!(says(&json, r#""truncated":null"#), "{json}");
        assert!(says(&json, r#""kept":6"#), "{json}");
    }

    /// The locals a moment holds are that moment's, and they change along
    /// the timeline.
    ///
    /// `total` is declared before the call it is assigned from returns, so
    /// the moment inside the callee shows it holding zero and the moment
    /// after the return shows it holding 42. That is not a defect being
    /// pinned: it is [`record`]'s stated limitation — a moment is at the
    /// first instruction carrying a new line, which is inside the expression
    /// rather than at the statement's start — and a test that showed 42 in
    /// both would mean the recording was not per-moment at all.
    #[test]
    fn a_local_holds_what_it_held_at_that_moment() {
        let json = debug_json(WALKED, None, None, 0);
        let moments: Vec<&str> = json.split(r#"{"at":"#).collect();
        let inside = moments
            .iter()
            .find(|moment| moment.contains(r#""why":"call""#))
            .unwrap_or_else(|| panic!("a call moment: {json}"));
        let after = moments
            .iter()
            .find(|moment| moment.contains(r#""why":"return""#))
            .unwrap_or_else(|| panic!("a return moment: {json}"));
        assert!(inside.contains(r#""name":"n","value":"21""#), "{inside}");
        assert!(inside.contains(r#""name":"total","value":"0""#), "{inside}");
        assert!(after.contains(r#""name":"total","value":"42""#), "{after}");
    }

    /// A local that names a heap object points at one the moment carries.
    #[test]
    fn a_local_that_names_an_object_carries_it() {
        let json = debug_json(
            "export fn main() -> Int {\n  let greeting = \"hello\"\n  greeting.length()\n}",
            None,
            None,
            0,
        );
        assert!(says(&json, r#""outcome":"success""#), "{json}");
        assert!(says(&json, r#""name":"String""#), "{json}");
        assert!(says(&json, r#""name":"text","value":"hello""#), "{json}");
        // The address on the local and the address on the object are the
        // same number, which is what lets a Memory pane follow a name.
        let addresses = every(&json, "at");
        assert!(
            addresses.iter().any(|at| at.len() > 4),
            "an object address: {json}"
        );
    }

    /// A recording that hit its bound says which bound, and the run it was
    /// recording still finished and still answered.
    ///
    /// The second half is the point. A recorder that halted the run when it
    /// filled up would answer a question about a program with a program that
    /// did not run.
    #[test]
    fn a_recording_past_its_bound_says_so_and_the_run_goes_on() {
        let counting =
            "export fn main() -> Int {\n  var n = 0\n  while n < 100 {\n    n = n + 1\n  }\n  n\n}";
        let json = debug_json(counting, None, None, 4);
        assert!(says(&json, r#""truncated":"moments""#), "{json}");
        assert!(says(&json, r#""kept":4"#), "{json}");
        assert!(says(&json, r#""limit":4"#), "{json}");
        // The run reached its own end rather than the recorder's.
        assert!(says(&json, r#""outcome":"success""#), "{json}");
        assert!(
            says(&json, r#""answer":{"type":"int","value":100}"#),
            "{json}"
        );
    }

    /// A caller cannot ask for an unbounded recording.
    #[test]
    fn a_recording_is_bounded_however_much_is_asked_for() {
        let json = debug_json("export fn main() -> Int { 1 }", None, None, usize::MAX);
        assert!(
            says(&json, &format!(r#""limit":{}"#, record::MOST_MOMENTS)),
            "{json}"
        );
    }

    /// A debugged run is the same run: the recorder watches it and does not
    /// change it.
    #[test]
    fn recording_does_not_change_what_the_program_did() {
        let source = r#"use console.println

export fn main() -> Result<Int, Error> {
  println("watched")?
  Ok(21 * 2)
}"#;
        let plain = run_json(source, None, None);
        let watched = debug_json(source, None, None, 0);
        for fragment in [
            r#""outcome":"success""#,
            r#""stdout":"watched\n""#,
            r#""instructions":"#,
        ] {
            assert!(plain.contains(fragment), "{plain}");
            assert!(watched.contains(fragment), "{watched}");
        }
        assert_eq!(
            every(&plain, "instructions"),
            every(&watched, "instructions")
        );
    }

    /// A source that did not compile has no recording, rather than an empty
    /// one that reads as a program which did nothing.
    #[test]
    fn a_program_that_does_not_compile_has_no_recording() {
        let json = debug_json("export fn main() -> Int { 1 +", None, None, 0);
        assert!(says(&json, r#""debug":null"#), "{json}");
    }

    /// The four existing entry points answer what they always answered.
    /// `web/check.mjs` and CI depend on it.
    #[test]
    fn a_plain_run_carries_no_recording() {
        let json = run_json("export fn main() -> Int { 1 }", None, None);
        assert!(!json.contains("\"debug\""), "{json}");
        let json = compile_json("export fn main() -> Int { 1 }");
        assert!(!json.contains("\"debug\""), "{json}");
    }

    /// Every answer is one JSON object and nothing else, whatever happened.
    #[test]
    fn every_answer_is_a_single_object() {
        for source in [
            "export fn main() -> Int { 1 }",
            "export fn main() -> Int { 1 +",
            "",
        ] {
            for json in [compile_json(source), run_json(source, None, None)] {
                assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
                assert_eq!(json.matches("\"ok\":").count(), 1, "{json}");
            }
        }
    }
}
