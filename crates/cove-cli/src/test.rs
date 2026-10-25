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
//!
//! # Which tier ran
//!
//! With no `--backend` a suite runs on the fastest tier this build and host
//! have — the native tier of ADR 0055 where there is one, the VM otherwise —
//! as every command that runs a program does since ADR 0122. Each test is
//! lowered and compiled once, and every case of a property runs over that one
//! table. A function the code generator refuses runs on the encoded VM, so a
//! native suite is a mixture, and ADR 0055 forbids presenting one as fully
//! native: the suite reports its mixture once, aggregated over every test, on
//! stderr after the last one (see [`SuiteTiers`]). A suite on the VM prints
//! no such report, as `cove run` on the VM prints none.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cove_diag::{render, Diagnostic, SourceMap};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::files::Files;
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::testing::{Crossings, TestBackend, TestFailure, TestRun};
use cove_runtime::{NativeProgram, TierUp, Tiers};
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

/// Runs `cove test [path] [--filter <substring>] [--seed <n>] [--backend <ast|vm|native>]`.
pub(crate) fn cmd_test(args: &[String]) -> Result<(), CliError> {
    let mut filter: Option<&str> = None;
    let mut path: Option<&Path> = None;
    let mut backend = Backend::default_for_a_run();
    let mut named = false;
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
                // Named on a host or build without the tier, ADR 0055's
                // capability diagnostic, as `cove run --backend native` gives
                // it: never a suite quietly run on the VM under the name.
                if backend == Backend::Native {
                    cove_runtime::native_available()
                        .map_err(|unavailable| CliError::Message(unavailable.to_string()))?;
                }
                named = true;
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
    let mut tiers = SuiteTiers::default();
    for test in &selected {
        let name = test.qualified_name();
        match run_test(
            test,
            &package.root,
            &allow_real,
            &sources,
            &program,
            backend,
            crate::tier_up(named),
            properties,
            &mut tiers,
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

    eprint!("{}", tiers.report());
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
/// another one left behind. On the native tier, what was compiled and how each
/// run crossed between the tiers is added to `tiers`.
#[allow(clippy::too_many_arguments)]
fn run_test(
    test: &DeclaredTest,
    root: &Path,
    allow_real: &BTreeSet<&str>,
    sources: &Arc<SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
    backend: Backend,
    tier_up: cove_runtime::TierUp,
    properties: Properties,
    tiers: &mut SuiteTiers,
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
            Backend::Vm => TestBackend::Vm,
            Backend::Native => TestBackend::Native(tier_up),
        },
        limits: None,
    };
    let explained = |failure| explain(test, failure);
    let prepared = run
        .prepare(test)
        .map_err(|failure| Box::new(explained(*failure)))?;
    // One table, built in `prepare`, for this test and every case of it. What
    // it compiled is read after the last run, because a table that compiles as
    // functions become hot compiles during the runs (ADR 0123).
    let native = prepared.native();
    let mut runs = Vec::new();
    let mut counted = |(failure, crossings): (Option<TestFailure>, Option<Crossings>)| {
        if let Some(crossings) = crossings {
            runs.push(crossings);
        }
        failure
    };
    if test.entry.decl.params.is_empty() {
        let outcome = run.run_prepared_counting(test, &prepared, registry, Vec::new());
        let failure = counted(outcome);
        if let Some(native) = native {
            tiers.compiled(native, &runs);
        }
        return match failure {
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
        counted(run.run_prepared_counting(
            test,
            &prepared,
            hosts(),
            args.iter().map(Arg::value).collect(),
        ))
    });
    if let Some(native) = native {
        tiers.compiled(native, &runs);
    }
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

/// What a suite on the native tier ran where, over every test it ran.
///
/// ADR 0055 asks a native run to report "how many calls and functions used each
/// tier, so a benchmark cannot present a mixed run as fully native", and `cove
/// run`'s report is that for one program. A suite is one program per test —
/// each is lowered on its own — so a report per test would be a hundred and
/// sixty-five tables and no answer. This is the one answer: the same figures,
/// in the same words, summed over the suite.
///
/// Two things about the sums are deliberate:
///
/// - **functions are counted per lowering.** The standard library's `map` is
///   compiled once in every test that reaches it, and it is compiled that many
///   times, so the figure is what the suite compiled rather than how many
///   distinct functions it has;
/// - **refusals are aggregated by reason, and a function is one function.** A
///   refusal is a fact about a function and not about the test that reached
///   it, so the table counts each refused function once under its reason, and
///   charges it every dynamic call it kept in the VM over every run — the
///   ranking `cove run`'s table uses, by the work a refusal prevented.
#[derive(Default)]
pub(crate) struct SuiteTiers {
    /// Tests compiled, once each.
    tests: usize,
    /// Runs on compiled tables: one per ordinary test, one per property case.
    runs: u64,
    reachable: usize,
    compiled: usize,
    refused: usize,
    /// Functions not called often enough, in their test, to be compiled — which
    /// are not refusals (ADR 0123).
    not_hot: usize,
    /// When the tables compiled: one policy for a suite.
    policy: Option<TierUp>,
    compile: Duration,
    tiers: Tiers,
    /// Runs whose outermost frame — the test function itself — was entered
    /// compiled. A run enters its outermost frame on the encoded tier today, so
    /// this is nought, and is printed so that a reader of the call counts knows
    /// the test's own body is not among them.
    outermost_native: u64,
    /// Refused functions by reason: each function's name and the dynamic
    /// calls it kept in the VM over every run.
    reasons: BTreeMap<String, BTreeMap<String, u64>>,
}

impl SuiteTiers {
    /// One test's table, compiled.
    /// One test's table, after its last run, and every run made over it: one
    /// for an ordinary test, one per case for a property.
    ///
    /// After rather than before, because a table that compiles as functions
    /// become hot compiles during the runs (ADR 0123), and what it compiled
    /// and refused is known only once they are over.
    fn compiled(&mut self, native: &NativeProgram, runs: &[Crossings]) {
        self.tests += 1;
        self.policy = Some(native.policy());
        self.reachable += native.reachable();
        self.compiled += native.compiled();
        self.not_hot += native.not_hot();
        self.compile += native.compile_time();
        let refusals = native.refusals();
        self.refused += refusals.len();
        for row in &refusals {
            self.reasons
                .entry(row.reason.clone())
                .or_default()
                .entry(row.name.clone())
                .or_default();
        }
        for crossings in runs {
            self.ran(&refusals, crossings);
        }
    }

    /// One run over that table.
    fn ran(&mut self, refusals: &[cove_runtime::Refused], crossings: &Crossings) {
        self.runs += 1;
        let (sum, run) = (&mut self.tiers, crossings.tiers);
        sum.vm_to_vm += run.vm_to_vm;
        sum.vm_to_native += run.vm_to_native;
        sum.native_to_vm += run.native_to_vm;
        sum.native_to_native_direct += run.native_to_native_direct;
        sum.native_to_native_inline += run.native_to_native_inline;
        sum.native_to_native_mediated += run.native_to_native_mediated;
        sum.host_to_native += run.host_to_native;
        sum.host_to_vm += run.host_to_vm;
        if run.host_to_native > 0 {
            self.outermost_native += 1;
        }
        for row in refusals {
            let made = crossings
                .refused_calls
                .get(row.id.index())
                .copied()
                .unwrap_or(0);
            *self
                .reasons
                .entry(row.reason.clone())
                .or_default()
                .entry(row.name.clone())
                .or_default() += made;
        }
    }

    /// The report, for stderr: empty for a suite that compiled nothing, which
    /// is a suite on the VM or the interpreter.
    pub(crate) fn report(&self) -> String {
        use crate::thousands;
        use std::fmt::Write;
        if self.tests == 0 {
            return String::new();
        }
        let mut out = String::new();
        let share = match self.reachable {
            0 => 0.0,
            whole => 100.0 * self.compiled as f64 / whole as f64,
        };
        match self.policy {
            Some(TierUp::AtCall(hot)) => {
                let _ = writeln!(
                    out,
                    "native: {} test(s), one table each, a function compiled at its call number \
                     {}: {} of {} reachable function(s) compiled ({:.1}%), {} refused, {} not hot; \
                     compilation {:.1} ms",
                    self.tests,
                    hot,
                    thousands(self.compiled as u64),
                    thousands(self.reachable as u64),
                    share,
                    thousands(self.refused as u64),
                    thousands(self.not_hot as u64),
                    self.compile.as_secs_f64() * 1000.0,
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "native: {} test(s) compiled once each: {} of {} reachable function(s) \
                     compiled ({:.1}%), {} refused; compilation {:.1} ms",
                    self.tests,
                    thousands(self.compiled as u64),
                    thousands(self.reachable as u64),
                    share,
                    thousands(self.refused as u64),
                    self.compile.as_secs_f64() * 1000.0,
                );
            }
        }
        let tiers = self.tiers;
        let _ = writeln!(
            out,
            "native: {} run(s), a property's cases counted one by one; the outermost frame \
             native in {} and encoded in {}",
            thousands(self.runs),
            thousands(self.outermost_native),
            thousands(self.runs - self.outermost_native),
        );
        let _ = writeln!(
            out,
            "native: calls VM -> VM {}, VM -> native {}, native -> VM {}, native -> native {}; \
             {:.1}% of {} Cove call(s) used native code",
            thousands(tiers.vm_to_vm),
            thousands(tiers.vm_to_native),
            thousands(tiers.native_to_vm),
            thousands(tiers.native_to_native_direct + tiers.native_to_native_mediated),
            crate::native_share(tiers),
            thousands(tiers.calls()),
        );
        if self.reasons.is_empty() {
            return out;
        }
        // Ranked by the dynamic calls each reason kept in the VM, then by the
        // reason, so the table is the same table twice for one suite.
        let mut reasons: Vec<(&String, usize, u64, String)> = self
            .reasons
            .iter()
            .map(|(reason, functions)| {
                let calls = functions.values().sum();
                // The function this reason kept in the VM most, by name after
                // that so a tie is broken the same way twice.
                let top = match functions
                    .iter()
                    .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                {
                    Some((name, &made)) if made > 0 => format!(" ({name}, {})", thousands(made)),
                    _ => String::new(),
                };
                (reason, functions.len(), calls, top)
            })
            .collect();
        reasons.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        let _ = writeln!(
            out,
            "native: refusals by reason, ranked by the dynamic calls each kept in the VM"
        );
        let _ = writeln!(
            out,
            "  {:>13} {:>10}  refusal reason (the function it kept in the VM most)",
            "dynamic calls", "functions"
        );
        for (reason, functions, calls, top) in reasons {
            let _ = writeln!(
                out,
                "  {:>13} {:>10}  {reason}{top}",
                thousands(calls),
                thousands(functions as u64)
            );
        }
        out
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

    /// Runs every test of the package at `root` on every backend this host
    /// has, in order, reporting each one's name and the message it failed with.
    ///
    /// Every one, and asserting they agree, because `cove test` runs on the
    /// fastest tier the host has and every assertion below used to be about
    /// the interpreter. Running only the default would have retired that
    /// coverage silently; running each keeps it and adds the property that
    /// matters more than any — that a suite reports the same thing whichever
    /// evaluator ran it. The native tier is one of them where it runs (ADR
    /// 0122), gated as every native test is.
    fn run_all(root: &Path, allow_real: &[&str]) -> Vec<(String, Option<String>)> {
        let on_the_backend = run_all_on(root, allow_real, Backend::Vm);
        let on_the_oracle = run_all_on(root, allow_real, Backend::Ast);
        assert_eq!(
            on_the_backend, on_the_oracle,
            "the two evaluators report this suite differently"
        );
        if cfg!(all(feature = "template", target_arch = "x86_64", unix)) {
            let on_the_tier = run_all_on(root, allow_real, Backend::Native);
            assert_eq!(
                on_the_backend, on_the_tier,
                "the native tier and the encoded VM report this suite differently"
            );
        }
        on_the_backend
    }

    /// One backend's answer for every test of the package at `root`.
    fn run_all_on(
        root: &Path,
        allow_real: &[&str],
        backend: Backend,
    ) -> Vec<(String, Option<String>)> {
        run_all_counted(root, allow_real, backend, &mut SuiteTiers::default())
    }

    /// [`run_all_on`], adding what each test compiled and ran to `tiers`.
    fn run_all_counted(
        root: &Path,
        allow_real: &[&str],
        backend: Backend,
        tiers: &mut SuiteTiers,
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
                    TierUp::Eager,
                    Properties::default(),
                    tiers,
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
                TierUp::Eager,
                Properties::default(),
                &mut SuiteTiers::default(),
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
                TierUp::Eager,
                Properties::default(),
                &mut SuiteTiers::default(),
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
            TierUp::Eager,
            Properties::default(),
            &mut SuiteTiers::default(),
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
                TierUp::Eager,
                Properties { cases, seed },
                &mut SuiteTiers::default(),
            )
            .expect_err("the property fails")
            .message
        };
        assert!(message(Some(1), 100).contains("(seed 1, "));
        assert_eq!(message(Some(1), 100), message(Some(1), 100));
        let derived = property::seed_for("unit.small");
        assert_eq!(message(None, 100), message(Some(derived), 100));
    }

    /// A function the inliner leaves a call to, because it calls itself, so
    /// that a test of it makes Cove calls the tier counts.
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    const RECURSIVE: &str = "/// The sum of `1` to `n`, or `0` for a negative `n`.\n\
         export fn sumTo(n: Int) -> Int {\n  if n <= 0 {\n    0\n  } else {\n    n + sumTo(n - 1)\n  }\n}\n\n";

    /// A suite on the VM or the interpreter compiles nothing and reports no
    /// mixture: there is none, and a report would claim a tier that did not
    /// run.
    #[test]
    fn a_suite_off_the_native_tier_reports_no_mixture() {
        let dir = package(
            "no-mixture",
            "",
            "test fn passes() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        for backend in [Backend::Vm, Backend::Ast] {
            let mut tiers = SuiteTiers::default();
            run_all_counted(dir.path(), &[], backend, &mut tiers);
            assert_eq!(tiers.report(), "", "{backend}");
        }
    }

    /// A suite on the native tier reports its mixture once, over every test:
    /// what was compiled, how the runs crossed between the tiers, and the
    /// refusals by reason. Every line of it begins `native: ` or is indented
    /// under one, which is the shape the end-to-end harness takes out.
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    #[test]
    fn a_native_suite_reports_its_mixture_once() {
        let dir = package(
            "mixture",
            "",
            &format!(
                "{RECURSIVE}\
                 test fn sumsThree() -> Result<Unit, Error> {{\n  assertEqual(sumTo(3), 6)?\n  Ok(())\n}}\n\n\
                 test fn sumsFour() -> Result<Unit, Error> {{\n  assertEqual(sumTo(4), 10)?\n  Ok(())\n}}\n"
            ),
        );
        let mut tiers = SuiteTiers::default();
        let results = run_all_counted(dir.path(), &[], Backend::Native, &mut tiers);
        assert!(
            results.iter().all(|(_, failed)| failed.is_none()),
            "{results:?}"
        );
        assert_eq!(tiers.tests, 2);
        assert_eq!(tiers.runs, 2);
        assert!(tiers.compiled > 0, "nothing was compiled");
        let report = tiers.report();
        assert!(
            report.starts_with("native: 2 test(s) compiled once each: "),
            "{report}"
        );
        assert!(report.contains("Cove call(s) used native code"), "{report}");
        assert!(tiers.tiers.native() > 0, "{report}");
        assert!(
            report
                .lines()
                .all(|line| line.starts_with("native: ") || line.starts_with(' ')),
            "{report}"
        );
    }

    /// A property on the native tier is compiled once and every case runs
    /// over that table — and the cases do run compiled code: the crossing into
    /// it is counted, not assumed.
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    #[test]
    fn a_property_runs_on_the_native_tier_compiled_once() {
        let dir = package(
            "native-property",
            "",
            &format!(
                "{RECURSIVE}\
                 test fn sumsAreNotNegative(n: Int) -> Result<Unit, Error> {{\n  \
                 assert(sumTo(n % 20) >= 0)?\n  Ok(())\n}}\n"
            ),
        );
        let mut tiers = SuiteTiers::default();
        let results = run_all_counted(dir.path(), &[], Backend::Native, &mut tiers);
        assert_eq!(results, [("unit.sumsAreNotNegative".to_string(), None)]);
        assert_eq!(tiers.tests, 1, "compiled once for the property");
        assert_eq!(tiers.runs, u64::from(property::DEFAULT_CASES));
        assert!(
            tiers.tiers.native() > 0,
            "no call entered compiled code: {:?}",
            tiers.tiers
        );
    }

    /// Named on a host without the tier, `--backend native` is ADR 0055's
    /// capability diagnostic, as on `cove run`; where the tier runs it is
    /// accepted.
    #[test]
    fn backend_native_is_accepted_where_the_tier_runs_and_refused_elsewhere() {
        let dir = package(
            "native-named",
            "",
            "test fn passes() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        let args = [
            dir.path().display().to_string(),
            "--backend".to_string(),
            "native".to_string(),
        ];
        match (
            cmd_test(&args),
            cfg!(all(feature = "template", target_arch = "x86_64", unix)),
        ) {
            (Ok(()), true) => {}
            (Err(CliError::Message(message)), false) => {
                assert!(
                    message.starts_with("native execution is unavailable: "),
                    "{message}"
                )
            }
            (outcome, runs) => panic!(
                "`cove test --backend native` answered {:?} on a host where the tier runs: {runs}",
                outcome.is_ok()
            ),
        }
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
