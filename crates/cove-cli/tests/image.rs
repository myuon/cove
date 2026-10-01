//! A program read back from an image runs as the program it was written from.
//!
//! [ADR 0077](../../../docs/adr/0077-cove-fmt-is-covefmt.md) ships covefmt in
//! the `cove` binary as an image `cove_ir::serial` wrote, and `cove fmt` runs
//! what it reads back. `bytecode_corpus.rs` holds that every program in the
//! repository reads back as the program an image keeps; this holds the claim
//! that matters to a user, which is about *running*: the same answer and the
//! same console output from the program read back as from the program freshly
//! lowered, on the encoded VM and — where this build and host have one — on
//! the native tier.
//!
//! The corpus is `tests/e2e`'s console-only cases: two hundred small programs
//! that between them reach most of the language and finish in milliseconds.
//! A failure is compared by its rendered diagnostic, which is drawn from the
//! image's own source map on one side and the front end's on the other — so
//! this is also the test that an image carries enough of the source to name a
//! file and a line.

use std::io::Write;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::{render, SourceMap};
use cove_runtime::host::{Console, Grants, HostRegistry};
use cove_runtime::{Runtime, RuntimeError, Value, Vm};
use cove_sema::HostSchemas;

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use support::{Case, ModuleIndex, Prepared};

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("no run panics while printing")).into_owned()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no run panics while printing")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Which tier a run is on.
#[derive(Clone, Copy, Debug)]
enum Tier {
    Vm,
    #[cfg_attr(
        not(all(feature = "template", target_arch = "x86_64", unix)),
        allow(dead_code)
    )]
    Native,
}

/// What one run left behind: its answer, or its failure rendered against the
/// source map the run had, and both console streams.
#[derive(Debug, PartialEq, Eq)]
struct Ran {
    answer: String,
    out: String,
    err: String,
}

fn rendered(sources: &SourceMap, answer: Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("ok: {value:?}"),
        Err(error) => format!("failed: {}", render(sources, &error.to_diagnostic())),
    }
}

/// Runs `case`'s entry in `program`, over `sources`, on `tier`.
fn run(
    case: &Case,
    program: &cove_ir::Program,
    sources: Arc<SourceMap>,
    module: &str,
    entry: &str,
    tier: Tier,
) -> Ran {
    let out = Buffer::default();
    let err = Buffer::default();
    let mut hosts = HostRegistry::new(Grants::new(case.run.allow.clone()));
    hosts.register(Box::new(Console::new(out.clone(), err.clone())));
    // The checked program is the front end's, and the image has none; nothing
    // `run_entry` does reads it, which is what lets the two runs share this.
    let runtime = Runtime::new(Arc::default(), Arc::clone(&sources), Arc::new(hosts));
    let args: Vec<Rc<str>> = case.args.iter().map(|a| a.as_str().into()).collect();
    let answer = match tier {
        Tier::Vm => Vm::new(&runtime, runtime.hosts(), program).run_entry(module, entry, args),
        Tier::Native => {
            let native = cove_runtime::compile_native(program).expect("this host has the tier");
            Vm::with_native(&runtime, runtime.hosts(), program, &native)
                .run_entry(module, entry, args)
        }
    };
    Ran {
        answer: rendered(&sources, answer),
        out: out.text(),
        err: err.text(),
    }
}

fn tiers() -> Vec<Tier> {
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    return vec![Tier::Vm, Tier::Native];
    #[cfg(not(all(feature = "template", target_arch = "x86_64", unix)))]
    return vec![Tier::Vm];
}

/// Runs one case fresh and read back on every tier, answering whether it was
/// compared at all and each difference found.
fn compare(case: &Case, index: &ModuleIndex) -> (bool, Vec<String>) {
    let mut differences = Vec::new();
    let Ok(prepared) = Prepared::of(case, index) else {
        return (false, differences);
    };
    let (module, entry) = prepared.entry();
    let Ok(fresh) = cove_ir::lower_entry(
        &prepared.checked,
        &prepared.sources,
        &HostSchemas::new(),
        module,
        entry,
    ) else {
        return (false, differences);
    };
    let bytes = cove_ir::serial::encode(&fresh, &prepared.sources, module, entry)
        .unwrap_or_else(|why| panic!("{}: does not write as an image: {why}", case.name));
    let image = cove_ir::serial::decode(&bytes)
        .unwrap_or_else(|why| panic!("{}: its image does not read back: {why}", case.name));
    let read_back = Arc::new(image.sources);
    for tier in tiers() {
        let expected = run(
            case,
            &fresh,
            Arc::clone(&prepared.sources),
            module,
            entry,
            tier,
        );
        let found = run(
            case,
            &image.program,
            Arc::clone(&read_back),
            module,
            entry,
            tier,
        );
        if expected != found {
            differences.push(format!(
                "{} on {tier:?}:\n  fresh:     {expected:?}\n  read back: {found:?}",
                case.name
            ));
        }
    }
    (true, differences)
}

#[test]
fn a_program_read_back_from_an_image_runs_as_the_program_it_was_written_from() {
    let root = support::repo_root();
    let package = root.join("tests/e2e");
    let index = ModuleIndex::of(&package);
    // Console only: a test of the image, not of the hosts, and a program that
    // reads a file or a clock would need the fakes `differential.rs` builds to
    // be run twice alike.
    let cases: Vec<Case> = support::cases_of(&root, &package)
        .into_iter()
        .filter(|case| case.run.allow.iter().all(|grant| grant == "console"))
        .collect();
    // Across threads, each on the stack the runtime sizes, because the cases
    // are independent and one at a time they are most of ten seconds.
    let lanes = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let results: Vec<(bool, Vec<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..lanes)
            .map(|lane| {
                let (cases, index) = (&cases, &index);
                std::thread::Builder::new()
                    .stack_size(cove_runtime::STACK_SIZE)
                    .spawn_scoped(scope, move || {
                        cases
                            .iter()
                            .skip(lane)
                            .step_by(lanes)
                            .map(|case| compare(case, index))
                            .collect::<Vec<_>>()
                    })
                    .expect("a test thread starts")
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("no lane panics"))
            .collect()
    });
    let compared = results.iter().filter(|(ran, _)| *ran).count();
    let differences: Vec<String> = results.into_iter().flat_map(|(_, found)| found).collect();
    println!(
        "{compared} program(s) run fresh and read back from an image, on {:?}",
        tiers()
    );
    assert!(
        compared > 150,
        "only {compared} programs were compared; the corpus this test reads has moved"
    );
    assert!(
        differences.is_empty(),
        "{} run(s) of a program read back from an image differ from the fresh program's:\n{}",
        differences.len(),
        differences.join("\n")
    );
}
