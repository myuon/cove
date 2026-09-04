//! Cove in a browser tab: the front end and the linear-memory backend behind
//! a C ABI, with no server and no Cove runtime anywhere but the page.
//!
//! [Issue #241](https://github.com/myuon/cove/issues/241) asks for a
//! playground. This crate is the half of it that is Rust; `web/` is the half
//! that is a page. What it exports is described in [`abi`], and it is four
//! functions and one import.
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
    let front = front(source);
    let Some((checked, program)) = front.lowered else {
        return json::object([
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
        ]);
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
    let (answer, instructions, fuel_spent) = {
        let mut vm = Vm::new(&runtime, runtime.hosts(), &program);
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

    json::object([
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
    ])
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
