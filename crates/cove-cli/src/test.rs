//! `cove test`: run every `test fn` the package declares.
//!
//! A test is an ordinary declaration of its module, so this command derives
//! nothing of its own. The compiler already found the tests, checked their
//! bodies, and derived the capabilities each one's call graph requires; the
//! runner grants exactly those, runs each test, and reports what happened.
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
use cove_sema::resolve::DeclaredTest;

use crate::{load, CliError};

/// The diagnostic a failing test is reported as.
const FAILED: &str = "cove::test::failed";
/// The diagnostic a test that needs a capability no host provides is
/// reported as.
const NO_HOST: &str = "cove::test::no_host";

/// Runs `cove test [path] [--filter <substring>]`.
pub(crate) fn cmd_test(args: &[String]) -> Result<(), CliError> {
    let mut filter: Option<&str> = None;
    let mut path: Option<&Path> = None;
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
        match run_test(test, &package.root, &allow_real, &sources, &program) {
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

    let runtime = Runtime::new(Arc::clone(program), Arc::clone(sources), Arc::new(hosts));
    let mut interpreter = Interpreter::new(&runtime);
    let outcome = interpreter.run_entry(test.module, test.name, Vec::new());
    let assertion = interpreter
        .assertion_failure()
        .map(|(span, message)| (span, message.to_string()));

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
            Some(diagnostic)
        }
    }
}

/// The message a test's returned value reports, or `None` when it passed.
fn failure_message(value: &Value) -> Option<String> {
    let Value::Enum(result) = value else {
        return None;
    };
    if &*result.type_name != "Result" || &*result.case != "Err" {
        return None;
    }
    Some(
        result
            .payload
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
    // prints cannot interleave with the runner's own report.
    if real("console") {
        hosts.register(Box::new(Console::new(std::io::stdout())));
    } else {
        hosts.register(Box::new(Console::new(std::io::sink())));
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
    // recorded answers and a listener has no scripted requests, so a test
    // that serves finds its queue already empty. A test that wants either
    // needs data a `cove.toml` has no way to carry, so it belongs in a Rust
    // test that can supply it — which is where the representative servers are
    // exercised.
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
    use crate::fixture::{load_fixture, write, TempDir};

    /// Writes a one-module package and returns its root.
    fn package(name: &str, config: &str, source: &str) -> TempDir {
        let dir = TempDir::new(name);
        write(dir.path(), "cove.toml", config);
        write(dir.path(), "unit/unit.cove", source);
        dir
    }

    /// Runs every test of the package at `root`, in order, reporting each
    /// one's name and the message it failed with.
    fn run_all(root: &Path, allow_real: &[&str]) -> Vec<(String, Option<String>)> {
        let (sources, _, program) = load_fixture(root);
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let allow_real: BTreeSet<&str> = allow_real.iter().copied().collect();
        program
            .tests()
            .iter()
            .map(|test| {
                let outcome = run_test(test, root, &allow_real, &sources, &program);
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

    #[test]
    fn a_failing_assertion_points_at_the_assertion_it_failed_in() {
        let source = "test fn fails() -> Result<Unit, Error> {\n  assert(1 == 2)?\n  Ok(())\n}\n";
        let dir = package("assert-span", "", source);
        let (sources, _, program) = load_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        let diagnostic = run_test(&test, dir.path(), &BTreeSet::new(), &sources, &program)
            .expect("the test fails");
        let rendered = render(&sources, &diagnostic);
        assert!(rendered.contains("unit.cove:2:3"), "{rendered}");
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
        let (_, package, _) = load_fixture(dir.path());
        assert_eq!(package.config.test.allow_real, vec!["clock".to_string()]);
        assert_eq!(run_all(dir.path(), &["clock"])[0].1, None);
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
        let (_, _, program) = load_fixture(dir.path());
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
