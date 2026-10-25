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

use cove_diag::{render, Diagnostic, SourceMap};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::files::Files;
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::testing::{TestBackend, TestFailure, TestRun};
use cove_sema::capability::open_reasons;
use cove_sema::resolve::DeclaredTest;
use cove_sema::HostSchemas;

use crate::property::{self, Arg, Shape, Verdict};
use crate::{load, Backend, CliError};

/// The diagnostic a test that needs a capability no host provides is
/// reported as.
const NO_HOST: &str = "cove::test::no_host";

/// How a suite runs its properties: how many cases each, and under which
/// seed when one was named rather than derived from the property's name.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Properties {
    pub(crate) cases: u32,
    pub(crate) seed: Option<u64>,
}

impl Default for Properties {
    fn default() -> Self {
        Properties {
            cases: property::DEFAULT_CASES,
            seed: None,
        }
    }
}

/// Runs `cove test [path] [--filter <substring>] [--seed <n>] [--backend <ast|vm>]`.
pub(crate) fn cmd_test(args: &[String]) -> Result<(), CliError> {
    let mut filter: Option<&str> = None;
    let mut path: Option<&Path> = None;
    let mut backend = Backend::default_for_a_run();
    let mut seed: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::Message("`--seed` needs a value".to_string()))?;
                seed = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--seed` must be an unsigned 64-bit integer, found `{value}`"
                    ))
                })?);
                i += 1;
            }
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
                // Refused rather than accepted and run on the VM, for the reason
                // `native_is_not_here` gives: a suite that reported "native" for a
                // run that was not is the silent mixture ADR 0055 forbids. What a
                // suite needs and does not have yet is one aggregate report over
                // every case's own tier table, and 165 per-case reports are not
                // that.
                if backend == Backend::Native {
                    return Err(crate::native_is_not_here("test"));
                }
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
    let properties = Properties {
        cases: package.config.test.cases.unwrap_or(property::DEFAULT_CASES),
        seed,
    };

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
            properties,
        ) {
            Ok(None) => println!("{}", result_line("ok", &name)),
            Ok(Some(cases)) => println!("{} ({cases} cases)", result_line("ok", &name)),
            Err(diagnostic) => {
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
    drop(selected);
    drop(all);
    crate::leak_on_exit((sources, package, program));
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

/// Runs one test, answering how it passed — `Some(cases)` for a property,
/// `None` for an ordinary test — or the diagnostic to report when it failed.
///
/// Every test gets a registry of its own, and so does every case of a
/// property: a fake host holds state, and one run must not observe what
/// another one left behind.
fn run_test(
    test: &DeclaredTest,
    root: &Path,
    allow_real: &BTreeSet<&str>,
    sources: &Arc<SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
    backend: Backend,
    properties: Properties,
) -> Result<Option<u32>, Box<Diagnostic>> {
    let required: Vec<&str> = test
        .entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect();

    let hosts = || {
        let mut hosts = HostRegistry::new(Grants::new(required.clone()));
        register_hosts(&mut hosts, root, allow_real);
        hosts
    };

    // A capability no registered host module answers to cannot be granted at
    // all, fake or real. Saying so is more useful than letting the first call
    // report an unknown module.
    let registry = hosts();
    if let Some(missing) = required
        .iter()
        .find(|capability| !registry.contains(capability))
    {
        return Err(Box::new(
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
        ));
    }

    // A test is an entry, so it is lowered as one, one root per lowering, and
    // its outcome is reported by the rules every Cove test fails by: both are
    // `cove_runtime::testing`'s, shared with an embedder's own runner. What
    // is this command's is the policy above — what a test is granted and by
    // which hosts — and the note below.
    let run = TestRun {
        program,
        sources,
        schemas: &HostSchemas::new(),
        backend: match backend {
            Backend::Ast => TestBackend::Ast,
            Backend::Vm | Backend::Native => TestBackend::Vm,
        },
        limits: None,
    };
    let explained = |failure| explain(test, failure);
    let prepared = run
        .prepare(test)
        .map_err(|failure| Box::new(explained(*failure)))?;
    if test.entry.decl.params.is_empty() {
        return match run.run_prepared(test, &prepared, registry, Vec::new()) {
            None => Ok(None),
            Some(failure) => Err(Box::new(explained(failure))),
        };
    }
    drop(registry);

    // A property: lowered once above, and run once per generated case with
    // a registry of its own (ADR 0121).
    let (names, shapes) = property_parameters(test, program)?;
    let name = test.qualified_name();
    let seed = properties.seed.unwrap_or_else(|| property::seed_for(&name));
    let verdict = property::check(&shapes, properties.cases, seed, |args| {
        run.run_prepared(
            test,
            &prepared,
            hosts(),
            args.iter().map(Arg::value).collect(),
        )
    });
    match verdict {
        Verdict::Passed { cases } => Ok(Some(cases)),
        Verdict::Failed {
            passed,
            shrinks,
            args,
            failure,
        } => {
            let mut diagnostic = explained(failure);
            let shown = names
                .iter()
                .zip(&args)
                .map(|(name, arg)| format!("{name} = {}", arg.literal()))
                .collect::<Vec<_>>()
                .join(", ");
            let context = format!(
                "for `{shown}` (seed {seed}, {passed} case(s) passed, shrunk {shrinks} time(s))"
            );
            let prefix = format!("test `{name}` failed");
            diagnostic.message = match diagnostic.message.strip_prefix(&prefix) {
                Some(rest) => format!("{prefix} {context}{rest}"),
                None => format!("{} {context}", diagnostic.message),
            };
            let rerun =
                format!("`cove test --seed {seed} --filter {name}` runs this property again");
            diagnostic.help = Some(match diagnostic.help {
                Some(help) => format!("{help}; {rerun}"),
                None => rerun,
            });
            Err(Box::new(diagnostic))
        }
    }
}

/// A property's parameter names and the shapes the runner generates them
/// at, read from the types the checker resolved.
fn property_parameters(
    test: &DeclaredTest,
    program: &cove_sema::resolve::Program,
) -> Result<(Vec<String>, Vec<Shape>), Box<Diagnostic>> {
    let decl = &test.entry.decl;
    let names = decl
        .params
        .iter()
        .map(|param| param.name.node.clone())
        .collect();
    let types = program
        .facts
        .signature(decl.span.file, decl.span)
        .map(|signature| signature.params.as_slice())
        .unwrap_or_default();
    let shapes: Option<Vec<Shape>> = types.iter().map(Shape::of).collect();
    match shapes {
        Some(shapes) if shapes.len() == decl.params.len() => Ok((names, shapes)),
        // The checker refuses a property whose parameters the runner cannot
        // generate, so this is the two disagreeing rather than a program.
        _ => Err(Box::new(
            Diagnostic::error(
                cove_runtime::testing::FAILED,
                format!(
                    "test `{}` takes a parameter `cove test` cannot generate",
                    test.qualified_name()
                ),
            )
            .at(decl.name.span),
        )),
    }
}

/// A failure, with what a capability-open test owes a refusal at the Host
/// boundary.
fn explain(test: &DeclaredTest, failure: TestFailure) -> Diagnostic {
    let TestFailure {
        mut diagnostic,
        denied_capability,
    } = failure;
    if denied_capability.is_some() && test.entry.is_capability_open() {
        let note = capability_open_help(test);
        diagnostic.help = Some(match diagnostic.help {
            Some(help) => format!("{help}; {note}"),
            None => note,
        });
    }
    diagnostic
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
    /// Both, and asserting they agree, because `cove test` runs on the
    /// linear-memory backend and every assertion below used to be about the
    /// interpreter. Running only the default would have retired that coverage
    /// silently; running both keeps it and adds the property that matters
    /// more than either — that a suite reports the same thing whichever
    /// evaluator ran it.
    fn run_all(root: &Path, allow_real: &[&str]) -> Vec<(String, Option<String>)> {
        let on_the_backend = run_all_on(root, allow_real, Backend::Vm);
        let on_the_oracle = run_all_on(root, allow_real, Backend::Ast);
        assert_eq!(
            on_the_backend, on_the_oracle,
            "the two evaluators report this suite differently"
        );
        on_the_backend
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
                let outcome = run_test(
                    test,
                    root,
                    &allow_real,
                    &sources,
                    &program,
                    backend,
                    Properties::default(),
                );
                (
                    test.qualified_name(),
                    outcome.err().map(|diagnostic| diagnostic.message),
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
    /// record the assertion it saw: the oracle keeps the span of the call it
    /// performed, and the backend is told by the `AssertFailed` its failing
    /// arm was lowered with. Two mechanisms, and this is the one line that
    /// says they answer the same thing.
    #[test]
    fn a_failing_assertion_points_at_the_assertion_it_failed_in() {
        let source = "test fn fails() -> Result<Unit, Error> {\n  assert(1 == 2)?\n  Ok(())\n}\n";
        let dir = package("assert-span", "", source);
        let (sources, _, program) = check_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        for backend in [Backend::Ast, Backend::Vm] {
            let diagnostic = run_test(
                &test,
                dir.path(),
                &BTreeSet::new(),
                &sources,
                &program,
                backend,
                Properties::default(),
            )
            .expect_err("the test fails");
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
        for backend in [Backend::Ast, Backend::Vm] {
            let diagnostic = run_test(
                &test,
                dir.path(),
                &BTreeSet::new(),
                &sources,
                &program,
                backend,
                Properties::default(),
            )
            .expect_err("the test fails");
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
            Properties::default(),
        )
        .expect_err("the boundary refuses the call");
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

    /// A property runs on both evaluators, generated and shrunk the same way,
    /// and its failure names the smallest case, the seed and the count of
    /// cases that passed — `run_all` asserting the two agree is what pins
    /// that the arguments reached each backend as the same values.
    #[test]
    fn a_property_runs_generated_cases_and_reports_a_shrunk_counterexample() {
        let dir = package(
            "property",
            "",
            "test fn small(n: Int) -> Result<Unit, Error> {\n  assert(n <= 100)?\n  Ok(())\n}\n\n\
             test fn utf8RoundTrips(text: String, flags: Array<Bool>) -> Result<Unit, Error> {\n  \
             assertEqual(String.fromUtf8(text.toUtf8())?, text)?\n  \
             assert(flags.length() >= 0)?\n  Ok(())\n}\n\n\
             test fn noPercent(text: String) -> Result<Unit, Error> {\n  assert(!text.contains(\"%\"))?\n  Ok(())\n}\n",
        );
        let results = run_all(dir.path(), &[]);
        let seed = property::seed_for("unit.noPercent");
        let message = results[0].1.as_deref().expect("a '%' is generated");
        assert!(
            message.starts_with(&format!(
                "test `unit.noPercent` failed for `text = \"%\"` (seed {seed}, "
            )),
            "{message}"
        );
        assert!(
            message.ends_with(": assertion failed: `!text.contains(\"%\")`"),
            "{message}"
        );
        let message = results[1]
            .1
            .as_deref()
            .expect("an Int over 100 is generated");
        assert!(
            message.starts_with("test `unit.small` failed for `n = 101` ("),
            "{message}"
        );
        assert_eq!(results[2], ("unit.utf8RoundTrips".to_string(), None));
    }

    /// A trap is a failure like an `Err`, and is shrunk like one.
    #[test]
    fn a_property_that_traps_is_shrunk_to_the_smallest_trapping_case() {
        let dir = package(
            "property-trap",
            "",
            "test fn doubles(n: Int) -> Result<Unit, Error> {\n  assert(n * 2 / 2 == n)?\n  Ok(())\n}\n",
        );
        let results = run_all(dir.path(), &[]);
        let message = results[0]
            .1
            .as_deref()
            .expect("an overflowing Int is generated");
        assert!(
            message.starts_with("test `unit.doubles` failed for `n = 4611686018427387904` ("),
            "{message}"
        );
    }

    #[test]
    fn a_named_seed_changes_the_cases_and_reproduces_them() {
        let dir = package(
            "property-seed",
            "",
            "test fn small(n: Int) -> Result<Unit, Error> {\n  assert(n <= 100)?\n  Ok(())\n}\n",
        );
        let (sources, _, program) = check_fixture(dir.path());
        let (sources, program) = (Arc::new(sources), Arc::new(program));
        let test = program.tests()[0];
        let message = |seed: Option<u64>, cases: u32| {
            run_test(
                &test,
                dir.path(),
                &BTreeSet::new(),
                &sources,
                &program,
                Backend::Vm,
                Properties { cases, seed },
            )
            .expect_err("the property fails")
            .message
        };
        assert!(message(Some(1), 100).contains("(seed 1, "));
        assert_eq!(message(Some(1), 100), message(Some(1), 100));
        let derived = property::seed_for("unit.small");
        assert_eq!(message(None, 100), message(Some(derived), 100));
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
