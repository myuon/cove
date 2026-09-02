//! `cove test`: run every `test fn` the package declares.
//!
//! A test is an ordinary declaration of its module, so this command derives
//! nothing of its own. The compiler already found the tests, checked their
//! bodies, and derived the capabilities each one's call graph requires; the
//! runner grants exactly those, runs each test, and reports what happened.
//!
//! # A capability-open test
//!
//! "Exactly those" is a floor, not a ceiling (ADR 0015). A test that reaches
//! a call the compiler cannot follow — a closure invoked through a
//! parameter, a `dyn Trait` method — may ask for a capability the derived
//! set does not name, and the boundary refuses it, because the runtime's
//! grants are the only thing that decides. The runner does not widen the
//! grants to cover the gap; it says, when a refusal happens to a
//! capability-open test, that the derived set was the reason.
//!
//! In practice the floor holds for most higher-order code, because a lambda
//! is analysed where it is *written*: a test that builds a closure that
//! prints has already been charged `console`, whoever ends up calling it.
//!
//! # Fakes are the default
//!
//! Each capability is granted with its host's *fake* implementation unless
//! `cove.toml`'s `[test] allow_real` names it. That is what makes a suite
//! deterministic and safe to run anywhere: a test that reads the clock sees
//! virtual time, a test that writes a file writes to memory, and a test that
//! prints prints into a buffer nobody reads.
//!
//! # A failing test
//!
//! A test reports failure as an `Err`, exactly as every other Cove function
//! reports expected failure. The runner renders that failure as a
//! diagnostic, so a failing assertion points at source like every other
//! error, and exits non-zero when any test failed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use cove_diag::{render, Diagnostic, SourceMap, Span};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::files::Files;
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::runtime::Runtime;
use cove_runtime::value::Value;
use cove_runtime::vm::Vm;
use cove_runtime::Lvm;
use cove_sema::capability::open_reasons;
use cove_sema::resolve::DeclaredTest;

use crate::{load, Backend, CliError, Executable};

/// The diagnostic a failing test is reported as.
const FAILED: &str = "cove::test::failed";
/// The diagnostic a test that needs a capability no host provides is
/// reported as.
const NO_HOST: &str = "cove::test::no_host";

/// Runs `cove test [path] [--filter <substring>] [--backend <ast|vm|lvm>]`.
pub(crate) fn cmd_test(args: &[String]) -> Result<(), CliError> {
    let mut filter: Option<&str> = None;
    let mut path: Option<&Path> = None;
    let mut backend = Backend::default_for_a_run();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::Message("`--filter` needs a value".to_string()))?;
                filter = Some(value.as_str());
                i += 1;
            }
            "--backend" => {
                let value = args.get(i + 1).ok_or_else(|| {
                    CliError::Message(format!("`--backend` needs a value: {}", Backend::NAMES))
                })?;
                backend = Backend::parse(value).ok_or_else(|| {
                    CliError::Message(format!(
                        "`--backend` must be {}, found `{value}`",
                        Backend::NAMES
                    ))
                })?;
                i += 1;
            }
            other => path = Some(Path::new(other)),
        }
        i += 1;
    }

    let (sources, package, program) = load(path)?;
    let allow_real: BTreeSet<&str> = package
        .config
        .test
        .allow_real
        .iter()
        .map(String::as_str)
        .collect();

    let sources = Arc::new(sources);
    let program = Arc::new(program);

    let all = program.tests();
    let selected = select(&all, filter);

    let mut failed = 0;
    for test in &selected {
        let name = test.qualified_name();
        match run_test(
            test,
            &package.root,
            &allow_real,
            &sources,
            &program,
            backend,
        ) {
            None => println!("{}", result_line("ok", &name)),
            Some(diagnostic) => {
                failed += 1;
                println!("{}", result_line("fail", &name));
                eprint!("{}", render(&sources, &diagnostic));
            }
        }
    }

    println!(
        "{}",
        summary(selected.len(), all.len(), selected.len() - failed, failed)
    );
    if failed > 0 {
        return Err(CliError::TestsFailed);
    }
    Ok(())
}

/// The tests `--filter` selects: those whose qualified name contains the
/// substring, or all of them when there is no filter.
///
/// The filter matches the qualified name, so `--filter text.` selects one
/// module's tests and `--filter Words` selects by what the tests are about.
fn select<'a>(tests: &'a [DeclaredTest<'a>], filter: Option<&str>) -> Vec<&'a DeclaredTest<'a>> {
    tests
        .iter()
        .filter(|test| match filter {
            Some(filter) => test.qualified_name().contains(filter),
            None => true,
        })
        .collect()
}

/// Runs one test, returning the diagnostic to report when it failed.
///
/// Every test gets a registry of its own: a fake host holds state, and one
/// test must not observe what another one left behind.
fn run_test(
    test: &DeclaredTest,
    root: &Path,
    allow_real: &BTreeSet<&str>,
    sources: &Arc<SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
    backend: Backend,
) -> Option<Diagnostic> {
    let required: Vec<&str> = test
        .entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect();

    let mut hosts = HostRegistry::new(Grants::new(required.clone()));
    register_hosts(&mut hosts, root, allow_real);

    // A capability no registered host module answers to cannot be granted at
    // all, fake or real. Saying so is more useful than letting the first call
    // report an unknown module.
    if let Some(missing) = required
        .iter()
        .find(|capability| !hosts.contains(capability))
    {
        return Some(
            Diagnostic::error(
                NO_HOST,
                format!(
                    "test `{}` requires the `{missing}` capability, which no host module provides",
                    test.qualified_name()
                ),
            )
            .at(test.entry.decl.name.span)
            .rule("`cove test` grants a test's required capabilities from the host modules the toolchain provides.")
            .help(format!(
                "remove the dependency on `{missing}`, or run this code through `cove run` with a host that provides it"
            )),
        );
    }

    // A test is an entry, so it is lowered as one: this names the test as
    // the root and the lowering works out what it reaches. Selecting a root
    // is all a command does; reachability lives in `cove_lir`, which is
    // where the fixed point that closes a slice against what the lowering
    // emits already is.
    //
    // One root per lowering rather than the whole suite in one, and that is
    // the decision rather than an omission. `cove_lir::lower_roots` takes as
    // many roots as a caller has, but its answer is one answer for the set:
    // the gaps come back together with no telling which root each belongs
    // to, so a suite lowered in one call would turn one unlowerable test
    // into every test's refusal. Lowering per test is what keeps a construct
    // a backend cannot run from refusing the tests that do not reach it, and
    // lowering runs once per test against an execution that runs for as long
    // as the test does, which is the ratio ADR 0019 allows the lowering to be
    // slow on.
    //
    // The refusal is reported as this test's failure rather than as the
    // command's, for the same reason: the other tests still ran, and a
    // suite that stopped at the first unlowerable test would report nothing
    // about them.
    //
    // Both lowered backends do this now. `--backend lvm` used to lower the
    // package once for the whole command, because `cove_lir` had no
    // reachable-set slice; `cove_lir::lower_entry` is that slice, so one
    // test's gap is no longer every test's.
    let lowered = match backend {
        Backend::Ast => None,
        Backend::Lvm => match cove_lir::lower_entry(program, sources, test.module, test.name) {
            Ok(ir) => Some(Executable::Linear(Arc::new(ir))),
            Err(items) => {
                return Some(
                    Diagnostic::error(
                        FAILED,
                        format!(
                            "test `{}` could not be lowered: {}",
                            test.qualified_name(),
                            items
                                .iter()
                                .map(|item| item.message.clone())
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                    )
                    .at(test.entry.decl.name.span),
                )
            }
        },
        Backend::Vm => match cove_ir::lower::lower_entry(program, test.module, test.name) {
            Ok(lowered) => match cove_ir::lower::validate(&lowered.program) {
                Ok(()) => Some(Executable::Vm(Arc::new(lowered.program))),
                Err(why) => {
                    return Some(
                        Diagnostic::error(
                            FAILED,
                            format!(
                                "test `{}` could not be lowered: {why}",
                                test.qualified_name()
                            ),
                        )
                        .at(test.entry.decl.name.span),
                    )
                }
            },
            Err(why) => return Some(crate::unsupported_by_backend(&why)),
        },
    };

    let runtime = Runtime::new(Arc::clone(program), Arc::clone(sources), Arc::new(hosts));
    let (outcome, assertion) = match &lowered {
        Some(Executable::Vm(ir)) => {
            let mut vm = Vm::new(&runtime, runtime.hosts(), ir);
            let outcome = vm.run_entry(test.module, test.name, Vec::new());
            let assertion = vm
                .assertion_failure()
                .map(|(span, message)| (span, message.to_string()));
            (outcome, assertion)
        }
        Some(Executable::Linear(ir)) => {
            let mut lvm = Lvm::new(&runtime, runtime.hosts(), ir);
            let outcome = lvm.run_entry(test.module, test.name, Vec::new());
            let assertion = lvm
                .assertion_failure()
                .map(|(span, message)| (span, message.to_string()));
            (outcome, assertion)
        }
        None => {
            let mut interpreter = Interpreter::new(&runtime);
            let outcome = interpreter.run_entry(test.module, test.name, Vec::new());
            let assertion = interpreter
                .assertion_failure()
                .map(|(span, message)| (span, message.to_string()));
            (outcome, assertion)
        }
    };

    match outcome {
        Ok(value) => {
            let message = failure_message(&value)?;
            Some(failure(test, &message, assertion))
        }
        // A `RuntimeError` is a broken invariant, an ungranted capability, or
        // a limit — not an expected failure. It already points at source and
        // states its own rule, so it is reported as it stands, with the test
        // it came from named.
        Err(error) => {
            let mut diagnostic = error.to_diagnostic();
            diagnostic.message =
                format!("test `{}` failed: {}", test.qualified_name(), error.message);
            if error.denied_capability.is_some() && test.entry.is_capability_open() {
                let note = capability_open_help(test);
                diagnostic.help = Some(match diagnostic.help {
                    Some(help) => format!("{help}; {note}"),
                    None => note,
                });
            }
            Some(diagnostic)
        }
    }
}

/// What a capability-open test owes a refusal at the Host boundary.
///
/// The runner grants what the call graph could see, and this test reaches a
/// call it could not follow, so the missing capability was never derivable.
/// Saying which indirect form is in the way is what turns "the boundary
/// refused it" into something a reader can act on.
fn capability_open_help(test: &DeclaredTest) -> String {
    format!(
        "`cove test` grants what the call graph derives, and `{}` is capability-open ({}), so the derived set is a floor rather than the whole of what it needs; call the host operation somewhere the call graph can follow, or exercise this path through `cove run` with an explicit `allow`",
        test.qualified_name(),
        open_reasons(&test.entry.open_calls)
    )
}

/// The message a test's returned value reports, or `None` when it passed.
fn failure_message(value: &Value) -> Option<String> {
    Some(
        value
            .err_payload()?
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// The diagnostic one failed test is reported as.
///
/// It points at the assertion that failed when the error is that assertion's,
/// and at the test itself otherwise: an `Err` carries a message and no source
/// position, so the runner uses the position the evaluator recorded only when
/// the message it recorded is the one being reported.
fn failure(test: &DeclaredTest, message: &str, assertion: Option<(Span, String)>) -> Diagnostic {
    let span = match assertion {
        Some((span, recorded)) if recorded == message => span,
        _ => test.entry.decl.name.span,
    };
    Diagnostic::error(
        FAILED,
        format!("test `{}` failed: {message}", test.qualified_name()),
    )
    .at(span)
    .rule(
        "A test reports failure as an `Err`, the way every Cove function reports expected failure.",
    )
}

/// Registers every host module a test may reach, fake unless `allow_real`
/// names its capability.
///
/// Every module is registered whether or not this test needs it; the grants
/// are what decide, so a capability the compiler did not derive is refused
/// with the reason rather than with a missing module.
fn register_hosts(hosts: &mut HostRegistry, root: &Path, allow_real: &BTreeSet<&str>) {
    let real = |capability: &str| allow_real.contains(capability);

    // `Console` is a fake by where it writes: the real one writes to the
    // process's stdout, and a test's writes into a sink, so a test that
    // prints cannot interleave with the runner's own report. Both streams
    // answer to the one capability, so both are real together or faked
    // together: `allow_real = ["console"]` names what a test may reach, and
    // a test that reaches the console reaches the whole of it.
    if real("console") {
        hosts.register(Box::new(Console::new(std::io::stdout(), std::io::stderr())));
    } else {
        hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    }
    // A fake environment exposes nothing: a test that depends on a variable
    // the developer's shell happens to set is not a test.
    if real("env") {
        hosts.register(Box::new(Env::from_process()));
    } else {
        hosts.register(Box::new(Env::new(BTreeMap::new())));
    }
    if real("documents") {
        hosts.register(Box::new(Documents::rooted(root.join("documents"))));
    } else {
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
    }
    if real("clock") {
        hosts.register(Box::new(Clock::real()));
    } else {
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    }
    if real("files") {
        hosts.register(Box::new(Files::rooted(root.join("files"))));
    } else {
        hosts.register(Box::new(Files::in_memory(BTreeMap::new())));
    }
    // The real `process` host starts no executable unless one is named, and
    // `cove test` names none: a test that shells out is not deterministic,
    // and there is no flag here to say otherwise.
    if real("process") {
        hosts.register(Box::new(Process::real(Vec::new(), Vec::new())));
    } else {
        hosts.register(Box::new(Process::recorded(
            Vec::new(),
            BTreeMap::new(),
            ProcessLog::new(),
        )));
    }
    // There is no real `database`, here or in `cove run`: connecting to one
    // means speaking a wire protocol this toolchain does not implement. So
    // `allow_real = ["database"]` gets the denied implementation, which
    // reports what is missing rather than pretending to be a database.
    if real("database") {
        hosts.register(Box::new(Database::denied()));
    } else {
        hosts.register(Box::new(Database::recorded(BTreeMap::new())));
    }
    // A fake `http` reaches nothing and listens to nothing: `fetch` has no
    // recorded responses and a listener has no scripted requests, so a test
    // that serves finds its queue already empty. A test that wants either
    // needs data a `cove.toml` has no way to carry — a recorded response is a
    // status and a body per URL, which is a table and not a setting — so it
    // belongs in a Rust test that can supply it, which is where the
    // representative servers are exercised.
    if real("http") {
        hosts.register(Box::new(Http::real()));
    } else {
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
    }
}

/// One test's line of output.
fn result_line(status: &str, name: &str) -> String {
    format!("{status:<4}  {name}")
}

/// The one-line summary `cove test` prints to stdout.
///
/// A filter is visible in the summary: "ran 1 of 4" says what was left out,
/// which a bare count would not.
fn summary(selected: usize, total: usize, passed: usize, failed: usize) -> String {
    let ran = if selected == total {
        format!("ran {selected} test(s)")
    } else {
        format!("ran {selected} of {total} test(s)")
    };
    if failed > 0 {
        format!("{ran}, {passed} passed, {failed} failed")
    } else {
        format!("{ran}, {passed} passed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{check_fixture, write, TempDir};

    /// Writes a one-module package and returns its root.
    fn package(name: &str, config: &str, source: &str) -> TempDir {
        let dir = TempDir::new(name);
        write(dir.path(), "cove.toml", config);
        write(dir.path(), "unit/unit.cove", source);
        dir
    }

    /// Runs every test of the package at `root` on both backends, in order,
    /// reporting each one's name and the message it failed with.
    ///
    /// Both, and asserting they agree, because `cove test` runs on the VM
    /// since ADR 0022 and every assertion below used to be about the
    /// interpreter. Running only the new default would have retired that
    /// coverage silently; running both keeps it and adds the property that
    /// matters more than either — that a suite reports the same thing
    /// whichever backend ran it.
    fn run_all(root: &Path, allow_real: &[&str]) -> Vec<(String, Option<String>)> {
        let on_the_vm = run_all_on(root, allow_real, Backend::Vm);
        let on_the_oracle = run_all_on(root, allow_real, Backend::Ast);
        assert_eq!(
            on_the_vm, on_the_oracle,
            "the two backends report this suite differently"
        );
        on_the_vm
    }

    /// One backend's answer for every test of the package at `root`.
    fn run_all_on(
        root: &Path,
        allow_real: &[&str],
        backend: Backend,
    ) -> Vec<(String, Option<String>)> {
        let (sources, _, program) = check_fixture(root);
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let allow_real: BTreeSet<&str> = allow_real.iter().copied().collect();
        program
            .tests()
            .iter()
            .map(|test| {
                let outcome = run_test(test, root, &allow_real, &sources, &program, backend);
                (
                    test.qualified_name(),
                    outcome.map(|diagnostic| diagnostic.message),
                )
            })
            .collect()
    }

    #[test]
    fn a_passing_assertion_passes_and_a_failing_one_reports_the_conditions_source() {
        let dir = package(
            "assert",
            "",
            "/// Doubles `n`.\n\
             export fn double(n: Int) -> Int {\n  n * 2\n}\n\n\
             test fn doublesTwo() -> Result<Unit, Error> {\n  assert(double(2) == 4)?\n  Ok(())\n}\n\n\
             test fn doublesThree() -> Result<Unit, Error> {\n  assert(double(3) == 7)?\n  Ok(())\n}\n",
        );
        let results = run_all(dir.path(), &[]);
        assert_eq!(
            results[0],
            (
                "unit.doublesThree".to_string(),
                Some(
                    "test `unit.doublesThree` failed: assertion failed: `double(3) == 7`"
                        .to_string()
                )
            )
        );
        assert_eq!(results[1], ("unit.doublesTwo".to_string(), None));
    }

    #[test]
    fn assert_equal_reports_both_values_and_the_actual_expressions_source() {
        let dir = package(
            "assert-equal",
            "",
            "/// Doubles `n`.\n\
             export fn double(n: Int) -> Int {\n  n * 2\n}\n\n\
             test fn doubles() -> Result<Unit, Error> {\n  assertEqual(double(3), 7)?\n  Ok(())\n}\n",
        );
        let results = run_all(dir.path(), &[]);
        assert_eq!(
            results[0].1.as_deref(),
            Some("test `unit.doubles` failed: assertion failed: `double(3)` is `6`, expected `7`")
        );
    }

    /// Every backend, because where a failure points is part of what a
    /// suite reports and a backend that pointed somewhere else would be
    /// reporting something else.
    ///
    /// An `Err` carries a message and no position, so each evaluator has to
    /// record the assertion it saw: the oracle and the predecessor keep the
    /// span of the call they performed, and the replacement is told by the
    /// `AssertFailed` its failing arm was lowered with. Three mechanisms,
    /// and this is the one line that says they answer the same thing.
    #[test]
    fn a_failing_assertion_points_at_the_assertion_it_failed_in() {
        let source = "test fn fails() -> Result<Unit, Error> {\n  assert(1 == 2)?\n  Ok(())\n}\n";
        let dir = package("assert-span", "", source);
        let (sources, _, program) = check_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        for backend in [Backend::Ast, Backend::Vm, Backend::Lvm] {
            let diagnostic = run_test(
                &test,
                dir.path(),
                &BTreeSet::new(),
                &sources,
                &program,
                backend,
            )
            .expect("the test fails");
            let rendered = render(&sources, &diagnostic);
            assert!(
                rendered.contains("unit.cove:2:3"),
                "{backend:?}: {rendered}"
            );
        }
    }

    /// An assertion that failed and was then handled is not where a later,
    /// unrelated failure is reported.
    ///
    /// The record survives the assertion — every evaluator keeps the last
    /// one it saw — so what stops it being read is that the message does not
    /// match the `Err` the test answered with. Every backend has to make
    /// that distinction, and a backend that recorded a span and no message
    /// could not.
    #[test]
    fn a_handled_assertion_is_not_where_a_later_failure_is_reported() {
        let source = "test fn fails() -> Result<Unit, Error> {\n  \
                      let handled = assert(1 == 2)\n  \
                      Err(Error(\"something else\"))\n}\n";
        let dir = package("assert-handled", "", source);
        let (sources, _, program) = check_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        for backend in [Backend::Ast, Backend::Vm, Backend::Lvm] {
            let diagnostic = run_test(
                &test,
                dir.path(),
                &BTreeSet::new(),
                &sources,
                &program,
                backend,
            )
            .expect("the test fails");
            let rendered = render(&sources, &diagnostic);
            assert!(
                rendered.contains("something else"),
                "{backend:?}: {rendered}"
            );
            assert!(
                rendered.contains("unit.cove:1:9"),
                "{backend:?}: {rendered}"
            );
        }
    }

    #[test]
    fn a_test_is_granted_a_fake_host_by_default() {
        let dir = package(
            "fake-clock",
            "",
            "use clock\n\n\
             test fn timeStandsStill() -> Result<Unit, Error> {\n  \
             assertEqual(clock.now(), clock.now())?\n  Ok(())\n}\n",
        );
        assert_eq!(run_all(dir.path(), &[])[0].1, None);
    }

    #[test]
    fn allow_real_grants_the_real_host_instead() {
        // The real clock advances between two reads; the virtual one does
        // not, which is exactly the difference `allow_real` selects.
        let dir = package(
            "real-clock",
            "[test]\nallow_real = [\"clock\"]\n",
            "use clock\n\n\
             test fn timeMoves() -> Result<Unit, Error> {\n  \
             assert(clock.now() <= clock.now())?\n  Ok(())\n}\n",
        );
        let (_, package, _) = check_fixture(dir.path());
        assert_eq!(package.config.test.allow_real, vec!["clock".to_string()]);
        assert_eq!(run_all(dir.path(), &["clock"])[0].1, None);
    }

    /// The case ADR 0015 is about, from the runner's side: the closure is
    /// invoked through a parameter, so no edge leads from `run` to what it
    /// runs — but the closure is *written* in the test, so `console` is
    /// derived there and the runner grants it a fake console. The floor is
    /// enough, and the host call happens.
    #[test]
    fn a_closure_calling_a_host_through_a_parameter_is_granted_what_it_needs() {
        let dir = package(
            "closure-host-call",
            "",
            "use console.println\n\n\
             /// Runs whatever it was handed.\n\
             fn run(work: fn() -> Result<Unit, Error>) -> Result<Unit, Error> {\n  work()\n}\n\n\
             test fn printsThroughAParameter() -> Result<Unit, Error> {\n  \
             run(fn() {\n    println(\"hello from a closure\")\n  })?\n  Ok(())\n}\n",
        );

        let (_, _, program) = check_fixture(dir.path());
        let test = program.tests()[0];
        assert!(
            test.entry
                .required_capabilities
                .contains(&cove_sema::Capability::new("console")),
            "the closure's body belongs to the test that wrote it"
        );
        assert!(
            test.entry.is_capability_open(),
            "the test still reaches a call the compiler cannot follow"
        );
        assert_eq!(run_all(dir.path(), &[])[0].1, None);
    }

    /// And the case where the floor is not enough. The conformance that runs
    /// is declared in a module `dyn`-dispatching code cannot reach, so the
    /// capability it needs is derivable nowhere the runner looks. The
    /// boundary refuses the call — it is the only thing that decides — and
    /// the runner says which indirect call left the grant list short.
    #[test]
    fn a_capability_open_test_refused_at_the_boundary_is_told_why() {
        let dir = TempDir::new("capability-open-refusal");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "render/render.cove",
            "\
/// Something that can describe itself.
export trait Summary {
  /// One line about this value.
  fn summarize(self) -> String
}

/// Describes a value whose type this module never sees.
export fn describe(entry: dyn Summary) -> String {
  entry.summarize()
}
",
        );
        write(
            dir.path(),
            "unit/unit.cove",
            "\
use console.println
use render.Summary
use render.describe

/// A noisy value: describing it reaches the console.
struct Loud {
  text: String
}

impl Summary for Loud {
  fn summarize(self) -> String {
    console.println(self.text)
    self.text
  }
}

test fn describesThroughDynDispatch() -> Result<Unit, Error> {
  assertEqual(describe(Loud(text: \"hi\")), \"hi\")?
  Ok(())
}
",
        );

        let (sources, _, program) = check_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        assert!(
            !test
                .entry
                .required_capabilities
                .contains(&cove_sema::Capability::new("console")),
            "the conformance is not reachable from where the call is written"
        );
        assert!(test.entry.is_capability_open());

        let diagnostic = run_test(
            &test,
            dir.path(),
            &BTreeSet::new(),
            &sources,
            &program,
            Backend::Vm,
        )
        .expect("the boundary refuses the call");
        assert!(
            diagnostic.message.contains("`console` capability"),
            "{}",
            diagnostic.message
        );
        let help = diagnostic.help.expect("a refusal explains itself");
        // The note is added to the runtime's own help rather than put in
        // place of it: "add `console` to `allow`" is the actionable half, and
        // overwriting it would trade the fix for the explanation.
        assert!(
            help.starts_with("add `console` to `allow`"),
            "the boundary's own help survives: {help}"
        );
        assert!(help.contains("capability-open"), "{help}");
        // The `dyn` dispatch is one hop away, in `render.describe`, so what
        // this test carries is the reason that reached it.
        assert!(
            help.contains("calls a capability-open declaration"),
            "{help}"
        );
    }

    #[test]
    fn a_test_reaches_its_modules_private_declarations() {
        let dir = package(
            "private",
            "",
            "fn secret() -> Int {\n  7\n}\n\n\
             test fn seesSecret() -> Result<Unit, Error> {\n  assertEqual(secret(), 7)?\n  Ok(())\n}\n",
        );
        assert_eq!(run_all(dir.path(), &[])[0].1, None);
    }

    #[test]
    fn a_capability_no_host_module_provides_is_reported_rather_than_granted() {
        let dir = package(
            "no-host",
            "",
            "use network\n\n\
             test fn callsOut() -> Result<Unit, Error> {\n  network.get(\"http://example.com\")?\n  Ok(())\n}\n",
        );
        let results = run_all(dir.path(), &[]);
        assert_eq!(
            results[0].1.as_deref(),
            Some("test `unit.callsOut` requires the `network` capability, which no host module provides")
        );
    }

    #[test]
    fn a_filter_selects_by_substring_of_the_qualified_name() {
        let dir = TempDir::new("filter");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "unit/unit.cove",
            "test fn alpha() -> Result<Unit, Error> {\n  Ok(())\n}\n\n             test fn beta() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        write(
            dir.path(),
            "other/other.cove",
            "test fn alpha() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        let (_, _, program) = check_fixture(dir.path());
        let all = program.tests();
        let names = |filter: Option<&str>| -> Vec<String> {
            select(&all, filter)
                .iter()
                .map(|test| test.qualified_name())
                .collect()
        };

        assert_eq!(names(None), ["other.alpha", "unit.alpha", "unit.beta"]);
        assert_eq!(names(Some("alpha")), ["other.alpha", "unit.alpha"]);
        assert_eq!(names(Some("unit.")), ["unit.alpha", "unit.beta"]);
        assert!(names(Some("gamma")).is_empty());
    }

    #[test]
    fn the_summary_counts_what_ran() {
        assert_eq!(summary(3, 3, 3, 0), "ran 3 test(s), 3 passed");
        assert_eq!(summary(3, 3, 1, 2), "ran 3 test(s), 1 passed, 2 failed");
        assert_eq!(summary(1, 4, 1, 0), "ran 1 of 4 test(s), 1 passed");
        assert_eq!(summary(0, 0, 0, 0), "ran 0 test(s), 0 passed");
    }

    #[test]
    fn a_result_line_aligns_both_statuses() {
        assert_eq!(result_line("ok", "unit.a"), "ok    unit.a");
        assert_eq!(result_line("fail", "unit.a"), "fail  unit.a");
    }
}
