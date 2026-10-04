//! `cove-edge check` and `cove-edge test`: the toolchain a tenant's author
//! runs, with the server's schemas and the server's hosts.
//!
//! # Why this exists
//!
//! `cove check` in `tenants/` cannot see `edge`, `kv`, `log` or `upstream`:
//! they are this crate's, registered by the server, and no `cove` command has
//! heard of them. So it warns nine times that `edge.Request` and
//! `edge.Response` are unchecked, it cannot tell building an `edge.Response`
//! from calling an operation, and `cove test` has no host to answer
//! `kv.get`. Issue #151 asked for a way to hand `cove` a schema, and was
//! closed by decision: a serialized schema is a second description of a
//! module whose first description is Rust, and `cove test` would still need
//! the implementation, which no file format carries. The embedder ships its
//! own checker instead — `examples/rules/host/src/bin/check.rs` is the
//! precedent — and this is that, for the edge server, with the test half the
//! rules example left out.
//!
//! Neither is a fork. `check` runs the compile and the admission the server
//! runs at deploy ([`deploy::compile`], [`deploy::admit`]) and renders with
//! `cove_diag::render`, so the checker cannot pass a tenant the server would
//! refuse. `test` runs each `test fn` the way `cove test` does — lowered as
//! an entry of its own, on the VM, failing on an `Err` and pointing at the
//! assertion that produced it — with two differences, both the server's
//! policy rather than the toolchain's: a test is granted what `cove.toml`
//! grants its tenant, not what its call graph derives, and it runs under the
//! tenant's limits. The hosts are the server's own (`kv` in memory and empty
//! per test, `log` silent, `upstream.get` at the latency asked for,
//! `upstream.fetch` a real fetch filtered by the tenant's allowlist),
//! answered through their blocking path, because a test is run to its end on
//! one thread.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use cove_diag::{render, Diagnostic, Severity, Span};
use cove_runtime::{Budget, Runtime, Value, Vm};
use cove_sema::resolve::DeclaredTest;
use cove_sema::{HostSchemas, RunConfig};

use crate::deploy::{self, Compiled, DeployOptions, Tenant};
use crate::hosts::{Latency, SCHEMAS};

/// What a command printed, and whether it succeeded.
#[derive(Debug, Default)]
pub struct Report {
    /// Standard output: one line per tenant or test, and a summary.
    pub out: String,
    /// Standard error: the diagnostics, rendered as `cove check` renders
    /// them.
    pub err: String,
    /// Whether the command exits zero.
    pub ok: bool,
}

/// The tenants `only` names, in `cove.toml`'s order, or all of them.
fn select<'a>(
    root: &Path,
    only: &[String],
    config: &'a cove_sema::Config,
) -> Result<Vec<(&'a String, &'a RunConfig)>, String> {
    for name in only {
        if !config.runs.contains_key(name) {
            return Err(format!(
                "no tenant named `{name}` in `{}`",
                root.join("cove.toml").display()
            ));
        }
    }
    Ok(config
        .runs
        .iter()
        .filter(|(name, _)| only.is_empty() || only.contains(name))
        .collect())
}

/// What a module that did not compile prints: the diagnostics alone, as
/// `cove check` prints them, without the `does not check:` a deploy refusal
/// leads with. A failure that is not a diagnostic — a directory that cannot
/// be read — is named with its module.
fn diagnostics_of(module: &str, why: &str) -> String {
    for stage in ["does not parse:\n", "does not check:\n"] {
        if let Some(rendered) = why.strip_prefix(stage) {
            return rendered.to_string();
        }
    }
    format!("{module}: {why}\n")
}

/// How many `.cove` files `module`'s directory holds, compiled or not.
fn files_of(root: &Path, module: &str) -> usize {
    std::fs::read_dir(root.join(module)).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "cove"))
            .count()
    })
}

fn list(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "-".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// `cove-edge check [tenant…]`.
///
/// Every module the selected tenants name is checked once against the
/// server's schemas, and its notices printed the way `cove check` prints
/// them. Then each tenant gets one line: what its entry requires, what
/// `cove.toml` grants, and whether the server would deploy it — the same
/// [`deploy::admit`], and the same lowering. Fails if any tenant would be
/// refused.
pub fn check(root: &Path, only: &[String]) -> Result<Report, String> {
    let config = deploy::read_manifest(root)?;
    let policy = deploy::read_policy(root, &config)?;
    let selected = select(root, only, &config)?;
    let mut report = Report::default();
    let mut modules: Vec<(String, Result<Compiled, String>)> = Vec::new();
    for (_, run) in &selected {
        let module = run.entry.split_once('.').map_or("", |(m, _)| m).to_string();
        if modules.iter().any(|(name, _)| *name == module) {
            continue;
        }
        let compiled = deploy::compile(root, &module);
        match &compiled {
            Ok(compiled) => {
                for notice in &compiled.program.notices {
                    report.err.push_str(&render(&compiled.sources, notice));
                }
            }
            Err(why) => report.err.push_str(&diagnostics_of(&module, why)),
        }
        modules.push((module, compiled));
    }

    let mut refused = 0;
    for (name, run) in &selected {
        let mut tenant = deploy::describe(name, run, &policy);
        let compiled = modules
            .iter()
            .find(|(module, _)| run.entry.starts_with(&format!("{module}.")))
            .map(|(_, compiled)| compiled);
        let verdict = match compiled {
            Some(Ok(compiled)) => deploy::admit(&mut tenant, compiled).and_then(|()| {
                let (module, function) = run.entry.split_once('.').unwrap_or_default();
                cove_ir::lower_entry(
                    &compiled.program,
                    &compiled.sources,
                    &HostSchemas::only(SCHEMAS),
                    module,
                    function,
                )
                .map(drop)
                .map_err(|items| {
                    format!(
                        "does not lower:\n{}",
                        deploy::report(&compiled.sources, &items)
                    )
                })
            }),
            Some(Err(_)) => Err("does not compile (see above)".to_string()),
            None => Err(format!("entry `{}` is not `module.function`", run.entry)),
        };
        let verdict = match verdict {
            Ok(()) => "ok".to_string(),
            Err(why) => {
                refused += 1;
                // The diagnostics behind a refusal are already printed above;
                // the line says which kind it was.
                format!("REFUSED: {}", why.lines().next().unwrap_or_default())
            }
        };
        let open = if tenant.open { " (lower bound)" } else { "" };
        report.out.push_str(&format!(
            "{:<10} requires [{}]{open}  granted [{}]{}  {verdict}\n",
            tenant.name,
            list(&tenant.required),
            list(&tenant.granted),
            tenant.fetch_note(),
        ));
    }

    let checked: Vec<&Compiled> = modules
        .iter()
        .filter_map(|(_, compiled)| compiled.as_ref().ok())
        .collect();
    let count = |severity| {
        checked
            .iter()
            .flat_map(|compiled| &compiled.program.notices)
            .filter(|notice| notice.severity == severity)
            .count()
    };
    let mut summary = format!(
        "checked {} module(s), {} file(s)",
        modules.len(),
        modules
            .iter()
            .map(|(module, _)| files_of(root, module))
            .sum::<usize>()
    );
    let warnings = count(Severity::Warning);
    let notes = count(Severity::Note);
    if warnings > 0 {
        summary.push_str(&format!(", {warnings} warning(s)"));
    }
    if notes > 0 {
        summary.push_str(&format!(", {notes} note(s)"));
    }
    summary.push_str(&format!(
        " against the server's schemas; {} tenant(s), {refused} refused\n",
        selected.len()
    ));
    report.out.push_str(&summary);
    report.ok = refused == 0 && checked.len() == modules.len();
    Ok(report)
}

/// How `cove-edge test` runs.
#[derive(Clone, Debug)]
pub struct TestOptions {
    /// How long the simulated `upstream.get` takes; zero unless asked.
    pub latency: Latency,
    /// Only the tests whose qualified name contains this.
    pub filter: Option<String>,
}

/// The diagnostic a failing test is reported as — `cove test`'s.
const FAILED: &str = "cove::test::failed";

/// `cove-edge test [tenant…]`.
///
/// Runs every `test fn` in each selected tenant's module, once per tenant:
/// `aggregate` and `impatient` share a module, and each runs its tests under
/// its own grant and limits. Fails if any test failed or any tenant did not
/// compile.
pub fn test(root: &Path, only: &[String], options: &TestOptions) -> Result<Report, String> {
    let config = deploy::read_manifest(root)?;
    let policy = deploy::read_policy(root, &config)?;
    let selected = select(root, only, &config)?;
    let mut report = Report::default();
    let deploy_options = DeployOptions {
        tenants: root.to_path_buf(),
        latency: options.latency,
        quiet: true,
        blocking_upstream: true,
    };
    let (mut ran, mut failed, mut uncompiled) = (0, 0, 0);
    for (name, run) in &selected {
        let tenant = deploy::describe(name, run, &policy);
        let module = run.entry.split_once('.').map_or("", |(m, _)| m);
        let compiled = match deploy::compile(root, module) {
            Ok(compiled) => compiled,
            Err(why) => {
                uncompiled += 1;
                report
                    .out
                    .push_str(&format!("fail  {name:<10} does not compile\n"));
                report.err.push_str(&diagnostics_of(module, &why));
                continue;
            }
        };
        let sources = Arc::new(compiled.sources);
        let program = Arc::new(compiled.program);
        let tests = program.tests();
        for test in tests.iter().filter(|test| {
            test.module == module
                && options
                    .filter
                    .as_deref()
                    .is_none_or(|filter| test.qualified_name().contains(filter))
        }) {
            ran += 1;
            let line = format!("{name:<10} {}", test.qualified_name());
            match run_test(test, &tenant, &deploy_options, &sources, &program) {
                None => report.out.push_str(&format!("ok    {line}\n")),
                Some(diagnostic) => {
                    failed += 1;
                    report.out.push_str(&format!("fail  {line}\n"));
                    report.err.push_str(&render(&sources, &diagnostic));
                }
            }
        }
    }
    let mut summary = format!("ran {ran} test(s), {} passed", ran - failed);
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    if uncompiled > 0 {
        summary.push_str(&format!("; {uncompiled} tenant(s) did not compile"));
    }
    report.out.push_str(&summary);
    report.out.push('\n');
    report.ok = failed == 0 && uncompiled == 0;
    Ok(report)
}

/// Runs one test as `cove test` would, with the tenant's grant, hosts and
/// limits; the diagnostic to report when it failed.
fn run_test(
    test: &DeclaredTest,
    tenant: &Tenant,
    options: &DeployOptions,
    sources: &Arc<cove_diag::SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
) -> Option<Diagnostic> {
    let required: BTreeSet<String> = test
        .entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect();
    if let Some(missing) = required.difference(&tenant.granted).next() {
        return Some(
            Diagnostic::error(
                FAILED,
                format!(
                    "test `{}` requires `{missing}`, which cove.toml does not grant tenant `{}`",
                    test.qualified_name(),
                    tenant.name
                ),
            )
            .at(test.entry.decl.name.span)
            .rule("`cove-edge test` grants a test what the server grants its tenant."),
        );
    }
    let lowered = match cove_ir::lower_entry(
        program,
        sources,
        &HostSchemas::only(SCHEMAS),
        test.module,
        test.name,
    ) {
        Ok(ir) => ir,
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
    };
    let runtime = Runtime::new(
        Arc::clone(program),
        Arc::clone(sources),
        Arc::new(deploy::registry(tenant, options)),
    );
    let mut vm = Vm::new(&runtime, runtime.hosts(), &lowered);
    let outcome = vm.run_entry_within(
        Budget::new(tenant.limits.clone()),
        test.module,
        test.name,
        Vec::new(),
    );
    let assertion = vm
        .assertion_failure()
        .map(|(span, message)| (span, message.to_string()));
    match outcome {
        Ok(value) => {
            let message = failure_message(&value)?;
            Some(failure(test, &message, assertion))
        }
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
    Some(
        value
            .err_payload()?
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// The diagnostic one failed test is reported as: at the assertion that
/// failed when the error is that assertion's, at the test otherwise.
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
