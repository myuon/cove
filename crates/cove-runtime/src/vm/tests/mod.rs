//! What the VM makes of a program, checked against the tree-walking
//! interpreter that is this language's definition.
//!
//! Every case here is differential: the same checked program is run on both
//! backends and their answers, their console output, and their failures are
//! compared. The interpreter is the oracle, so a test does not say what the
//! VM should answer — it says the two must not disagree, which is a stronger
//! statement and one that stays true as the language changes.
//!
//! The `benches/` entries used to be checked here, one agreement test each.
//! `crates/cove-cli/tests/differential.rs` now runs the whole corpus — every
//! `tests/e2e` case and every `examples/` and `benches/` entry — through both
//! backends and compares the value, the console, the outcome, and the
//! filesystem the run left, so those were the same ground covered less
//! thoroughly and twice, at half a minute of every `cargo test`. What stays
//! here is what is about the VM itself rather than about the two backends
//! agreeing: one instruction, one construct, one refusal at a time.
//!
//! This module is the harness the cases in its siblings are written in terms
//! of. [`agree`] runs one source on both backends and answers what they
//! agreed on; [`main_of`] renders the lowering of `m.main`, for the cases
//! where an outcome cannot show which instruction produced it; and
//! [`built_by_hand`](handwritten) — in the one sibling that needs it — reaches
//! an instruction no checked program can.

mod budget;
mod builtins;
mod calls;
mod closures;
mod control;
mod enums;
mod handwritten;
mod heap;
mod host;
mod operators;
mod places;
mod structs;
mod tasks;

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::{FileId, SourceMap};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use crate::budget::{Budget, Limits};
use crate::clock::{Clock, VirtualTime};
use crate::files::Files;
use crate::host::{Console, Env as EnvHost, Grants};
use crate::http::Http;
use crate::interp::Interpreter;

/// Every capability a differential run is granted.
///
/// The same set every time, because granting one is not what these tests
/// are about: a program that calls no host is unaffected by holding the
/// capability to, and a program that calls one is compared against an
/// interpreted run holding exactly the same grants.
const GRANTS: &[&str] = &["console", "clock", "env", "files", "http"];

/// A `console` sink a test can read back.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no test panics while printing")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("no test panics while printing")
                .clone(),
        )
        .expect("console output is UTF-8")
    }
}

/// What one backend made of one program: the value or error it answered,
/// and everything it wrote to `console`.
///
/// The value is rendered rather than carried, because a [`Value`] is
/// `Rc`-based and a run happens on a thread of its own.
#[derive(Debug)]
struct Outcome {
    answer: Result<String, RuntimeError>,
    output: String,
}

impl Outcome {
    fn value(&self) -> &str {
        match &self.answer {
            Ok(rendered) => rendered,
            Err(error) => panic!("the program ran without a runtime error: {error:?}"),
        }
    }

    fn error(&self) -> &RuntimeError {
        match &self.answer {
            Ok(rendered) => {
                panic!("expected a runtime error, but the program answered {rendered}")
            }
            Err(error) => error,
        }
    }
}

/// One run's answer, rendered so it can leave the thread it happened on.
fn described(answer: Result<Value, RuntimeError>) -> Result<String, RuntimeError> {
    answer.map(|value| format!("{value:?}"))
}

/// The hosts a differential run calls through: a `console` the test reads
/// back, a clock whose virtual time never advances on its own, an `env`
/// with nothing in it, and a `files` that is a map in this process.
///
/// `files` is the one of them that issues resource handles — a `Reader`
/// and a `Writer` — so it is what a test of `cove_ir::Inst::CallResource`
/// calls through. Each run builds its own, because a run that writes a
/// file must not be read back by the other backend's run of the same
/// program.
fn hosts(buffer: &Buffer, budget: Option<Budget>) -> Arc<HostRegistry> {
    let mut hosts = HostRegistry::new(Grants::new(GRANTS.to_vec()));
    hosts.register(Box::new(Console::new(buffer.clone(), Buffer::default())));
    hosts.register(Box::new(EnvHost::new(BTreeMap::new())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.register(Box::new(Files::in_memory(BTreeMap::new())));
    // Registered although nothing here reaches the network, because a
    // type a host module *declares* is asked of the registry rather than
    // of the static schema: `HostRegistry::host_type` answers `None` for
    // a module nobody registered, and the interpreter then reads
    // `http.Response(...)` as an operation and reports an unknown host
    // module. Every real runner registers every host, so a registry that
    // left one out would make this harness the only place the two
    // backends could differ about one.
    hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
    if let Some(budget) = budget {
        hosts.set_budget(budget);
    }
    Arc::new(hosts)
}

/// Runs `module.main` on the oracle.
fn interpreted(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    module: &str,
    budget: Option<Budget>,
) -> Outcome {
    let buffer = Buffer::default();
    let runtime = Runtime::new(checked.clone(), sources.clone(), hosts(&buffer, budget));
    let answer = Interpreter::new(&runtime).run_entry(module, "main", Vec::new());
    Outcome {
        answer: described(answer),
        output: buffer.text(),
    }
}

/// Lowers the program and runs `module.main` on the VM.
///
/// The lowering and the validation happen here, inside the thread
/// [`crate::on_cove_stack`] draws, because everything they are for is
/// here: a `Vm`, its stacks, and every `Value` it builds belong to this
/// thread. The program itself would cross — one is shared by every thread
/// of a run, which is what lets a spawned task run one — and there is
/// nothing to gain by lowering it on the other side of the boundary.
fn lowered(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    module: &str,
    budget: Option<Budget>,
) -> Outcome {
    let program = match cove_ir::lower::lower(checked) {
        Ok(program) => program,
        Err(why) => panic!("the program lowers, but stopped at {why}"),
    };
    cove_ir::lower::validate(&program)
        .unwrap_or_else(|why| panic!("the lowering holds the VM's invariants: {why}"));
    let entry = program
        .function_named(module, "main")
        .unwrap_or_else(|| panic!("`{module}.main` was lowered"));
    let buffer = Buffer::default();
    let hosts = hosts(&buffer, budget);
    let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
    let answer = Vm::new(&runtime, &hosts, &Arc::new(program)).run(entry, Vec::new());
    Outcome {
        answer: described(answer),
        output: buffer.text(),
    }
}

/// Runs one program on both backends, on a stack the runtime sized.
///
/// Both runs happen inside one [`crate::on_cove_stack`] because the
/// interpreter is a recursive tree walker and a test thread's stack is
/// not one it chose; only the two rendered outcomes come back out.
fn on_both(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    module: &str,
    limits: Option<Limits>,
) -> (Outcome, Outcome) {
    crate::on_cove_stack(|| {
        let budget = || limits.clone().map(Budget::new);
        (
            interpreted(checked, sources, module, budget()),
            lowered(checked, sources, module, budget()),
        )
    })
    .expect("a thread to run Cove on")
}

/// Parses `source` as the single unit of module `m`.
fn packaged(source: &str) -> (SourceMap, Package) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules: BTreeMap::from([(
            "m".to_string(),
            Module {
                name: "m".to_string(),
                dir: PathBuf::from("m"),
                units: vec![Unit { file, path, ast }],
            },
        )]),
    };
    (sources, package)
}

/// Parses and checks `source` the way `cove run` checks a package.
///
/// Both halves of the check, because the lowering reads what the second
/// one settled: a program that was only resolved carries no types, so
/// every test here would run the untyped instructions and would prove
/// nothing about the typed ones.
fn checked_module(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let (sources, package) = packaged(source);
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
    }
}

/// The same, resolved but not type-checked.
///
/// Two failures below belong to the runtime and are unreachable through
/// a checked program: a builtin method called with the wrong number of
/// arguments is a diagnostic now, so a program holding one never reaches
/// either backend. What both backends do with it is still worth pinning
/// — an embedder may resolve without checking, and the two must not
/// answer differently — so those tests are written against a program
/// that skipped the half that would have refused it.
fn resolved_module(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let (sources, package) = packaged(source);
    match cove_sema::resolve::resolve(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!("the source resolves:\n{}", rendered(&sources, &items)),
    }
}

fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

/// **The differential test.** Runs `source` on both backends and asserts
/// they agree about everything a program can be observed by: the value or
/// the error it answered, and what it wrote to `console`.
///
/// Every test here goes through this. ADR 0012 ranks the oracle above a
/// backend, so a test that asserted only what the VM did would be a test
/// of what somebody expected rather than of what Cove means.
fn agree(source: &str) -> Outcome {
    agree_over(checked_module(source), source)
}

/// `agree`, over a program that was resolved and not checked.
fn agree_unchecked(source: &str) -> Outcome {
    agree_over(resolved_module(source), source)
}

/// The comparison both of those make, over a program either one produced.
fn agree_over(checked: (Arc<SourceMap>, Arc<Checked>), source: &str) -> Outcome {
    let (sources, checked) = checked;
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(
        format!("{:?}", interpreted.answer),
        format!("{:?}", lowered.answer),
        "the two backends answered differently for:\n{source}"
    );
    assert_eq!(
        interpreted.output, lowered.output,
        "the two backends printed differently for:\n{source}"
    );
    lowered
}

/// `agree`, for a `main` written around `body` and returning `ty`.
fn agree_main(ty: &str, body: &str) -> Outcome {
    agree(&format!(
        "use console.println\n\nexport fn main() -> {ty} {{\n{body}\n}}\n"
    ))
}

/// What both backends made of one expression, rendered as a `Value`.
fn expression(ty: &str, expr: &str) -> String {
    agree_main(ty, &format!("  {expr}")).value().to_string()
}

/// What both backends made of a `main` written around `body`, with
/// `items` declared beside it — which is what a test about closures
/// needs, since a lambda is usually returned by or passed to something a
/// module declares.
fn value_of(ty: &str, items: &str, body: &str) -> String {
    agree(&format!(
        "use console.println\n\n{items}\nexport fn main() -> {ty} {{\n{body}\n}}\n"
    ))
    .value()
    .to_string()
}

/// The instructions `m.main` was lowered to, rendered.
///
/// Which instruction ran is not something an outcome can show — that is
/// the whole point of specialising — so a test that asserts the answer
/// asserts the listing beside it. Otherwise a specialisation that
/// stopped happening would go on passing every differential test there
/// is.
fn main_of(source: &str) -> String {
    let (_, checked) = checked_module(source);
    let program = cove_ir::lower::lower(&checked).expect("the program lowers");
    let id = program
        .function_named("m", "main")
        .expect("`m.main` was lowered");
    cove_ir::render(&program, id)
}

/// The message both backends refused one expression with, for an
/// expression only the runtime refuses.
///
/// Every other refusal in this file belongs to the checker now, so the
/// program has to skip the half that would have caught it; see
/// [`resolved_module`].
fn refused_unchecked(ty: &str, expr: &str) -> String {
    agree_unchecked(&format!(
        "use console.println\n\nexport fn main() -> {ty} {{\n  {expr}\n}}\n"
    ))
    .error()
    .message
    .clone()
}

/// A struct two of the sibling modules build on: `structs` for what building,
/// reading and writing one does, and `builtins` for what one renders as.
const CURSOR: &str = "struct Cursor {\n  at: Int\n  step: Int\n}\n\n";
