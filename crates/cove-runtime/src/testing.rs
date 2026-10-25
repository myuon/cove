//! Running one `test fn` and reporting what happened, the way `cove test`
//! reports it.
//!
//! `cove test` and an embedder's own test command differ in policy — what a
//! test is granted, which hosts answer it, what it is limited to — and in
//! nothing else. What they share is what this module is: lower the test as an
//! entry of its own, run it, and turn the outcome into a diagnostic by the
//! rules a Cove test fails by. An `Err` is a failure, reported at the
//! assertion that produced it when the message is that assertion's and at the
//! test otherwise; a runtime error is reported as it stands, with the test
//! named; a test the lowering refuses is that test's failure, not the
//! command's. Issue 601 (`examples/edge`'s README, friction item 10): those
//! rules were stated twice, in `cove-cli` and in the edge server's
//! `cove-edge test`, about eighty lines each.
//!
//! The registry is the caller's, built per test — a fake host holds state, and
//! one test must not see what another left behind — and so is any check made
//! before the run, such as whether every capability the test requires is
//! granted.

use std::sync::Arc;

use cove_diag::{Diagnostic, SourceMap, Span};
use cove_schema::HostSchemas;
use cove_sema::resolve::{DeclaredTest, Program};

use crate::budget::{Budget, Limits};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::interp::Interpreter;
use crate::runtime::Runtime;
use crate::value::Value;
use crate::{PreparedProgram, Vm};

/// The code a failing test is reported under.
pub const FAILED: &str = "cove::test::failed";

/// Which evaluator a test runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TestBackend {
    /// Lowered as an entry of its own and run on the VM.
    #[default]
    Vm,
    /// The tree-walking interpreter, which lowers nothing.
    Ast,
}

/// How a suite's tests are run: what they are compiled against, on which
/// evaluator, under which limits.
#[derive(Clone)]
pub struct TestRun<'a> {
    /// The checked package the tests are declared in.
    pub program: &'a Arc<Program>,
    /// Its sources, which a failure's diagnostic is rendered against.
    pub sources: &'a Arc<SourceMap>,
    /// The host modules the lowering is told of — the ones a test can call.
    pub schemas: &'a HostSchemas,
    pub backend: TestBackend,
    /// What each test is bounded by, or `None` for the runtime's defaults.
    pub limits: Option<Limits>,
}

/// A test that failed, and why.
#[derive(Clone, Debug)]
pub struct TestFailure {
    /// What to render: at the failed assertion, at the test, or the runtime
    /// error's own position.
    pub diagnostic: Diagnostic,
    /// The capability the host boundary refused, when that is how the test
    /// failed — which a runner that granted the derived set explains for a
    /// capability-open test.
    pub denied_capability: Option<String>,
}

/// A test made ready to run: lowered and prepared, on the VM, and nothing on
/// the interpreter, which lowers nothing.
///
/// A property — a `test fn` with parameters (ADR 0121) — runs once per
/// generated case, and this is what lets it be lowered and encoded once for
/// all of them: [`TestRun::prepare`] does the program-wide work, and
/// [`TestRun::run_prepared`] runs one case with a registry, a machine and
/// arguments of its own. Building each case's machine with [`Vm::new`]
/// instead re-encodes the program per case, which measured 0.35 ms a case on
/// a property whose body does nothing, against 0.003 ms on the interpreter.
pub struct PreparedTest {
    lowered: Option<PreparedProgram>,
}

impl TestRun<'_> {
    /// Runs `test` over `hosts`, answering its failure, or `None` when it
    /// passed.
    pub fn run(&self, test: &DeclaredTest, hosts: HostRegistry) -> Option<TestFailure> {
        match self.prepare(test) {
            Ok(prepared) => self.run_prepared(test, &prepared, hosts, Vec::new()),
            Err(failure) => Some(*failure),
        }
    }

    /// Lowers `test` for the VM, or answers the failure a test the lowering
    /// refuses is reported as.
    pub fn prepare(&self, test: &DeclaredTest) -> Result<PreparedTest, Box<TestFailure>> {
        // One root per lowering rather than the whole suite in one: a set of
        // roots is lowered to one answer, so one unlowerable test would
        // become every test's refusal. Per test, a construct the backend
        // cannot run refuses only the tests that reach it.
        let lowered = match self.backend {
            TestBackend::Ast => None,
            TestBackend::Vm => match cove_ir::lower_entry(
                self.program,
                self.sources,
                self.schemas,
                test.module,
                test.name,
            ) {
                Ok(ir) => Some(PreparedProgram::new(Arc::new(ir))),
                Err(items) => {
                    return Err(Box::new(TestFailure {
                        diagnostic: unlowered(test, &items),
                        denied_capability: None,
                    }))
                }
            },
        };
        Ok(PreparedTest { lowered })
    }

    /// Runs a prepared `test` over `hosts` with `args`, answering its
    /// failure, or `None` when it passed.
    ///
    /// A test that declares no parameters is entered as an entry, exactly as
    /// it always was, and `args` must be empty. A property is *invoked* with
    /// `args` — the arguments its runner generated — through the same door an
    /// embedder calls a function with values it built, so each one is held to
    /// the parameter type the checker resolved before anything runs.
    pub fn run_prepared(
        &self,
        test: &DeclaredTest,
        prepared: &PreparedTest,
        hosts: HostRegistry,
        args: Vec<Value>,
    ) -> Option<TestFailure> {
        let invoked = !test.entry.decl.params.is_empty();
        let runtime = Runtime::new(
            Arc::clone(self.program),
            Arc::clone(self.sources),
            Arc::new(hosts),
        );
        let budget = self
            .limits
            .as_ref()
            .map(|limits| Budget::new(limits.clone()));
        let (outcome, assertion) = match &prepared.lowered {
            Some(ir) => {
                let mut vm = Vm::with_prepared(&runtime, runtime.hosts(), ir);
                let outcome = match (budget, invoked) {
                    (Some(budget), false) => {
                        vm.run_entry_within(budget, test.module, test.name, Vec::new())
                    }
                    (None, false) => vm.run_entry(test.module, test.name, Vec::new()),
                    (Some(budget), true) => vm.invoke_within(budget, test.module, test.name, args),
                    (None, true) => vm.invoke(test.module, test.name, args),
                };
                let assertion = vm
                    .assertion_failure()
                    .map(|(span, message)| (span, message.to_string()));
                (outcome, assertion)
            }
            None => {
                let mut interpreter = Interpreter::new(&runtime);
                let outcome = match (budget, invoked) {
                    (Some(budget), false) => {
                        interpreter.run_entry_within(budget, test.module, test.name, Vec::new())
                    }
                    (None, false) => interpreter.run_entry(test.module, test.name, Vec::new()),
                    (Some(budget), true) => {
                        interpreter.invoke_within(budget, test.module, test.name, args)
                    }
                    (None, true) => interpreter.invoke(test.module, test.name, args),
                };
                let assertion = interpreter
                    .assertion_failure()
                    .map(|(span, message)| (span, message.to_string()));
                (outcome, assertion)
            }
        };
        report(test, outcome, assertion)
    }
}

/// What a test's outcome is reported as: `None` for a pass.
///
/// For a runner that ran the test some other way — parked, say, on an
/// [`OwnedVm`](crate::OwnedVm), whose
/// [`assertion_failure`](crate::OwnedVm::assertion_failure) is the second
/// argument here.
pub fn report(
    test: &DeclaredTest,
    outcome: Result<Value, RuntimeError>,
    assertion: Option<(Span, String)>,
) -> Option<TestFailure> {
    match outcome {
        Ok(value) => {
            let message = failure_message(&value)?;
            Some(TestFailure {
                diagnostic: failure(test, &message, assertion),
                denied_capability: None,
            })
        }
        // A `RuntimeError` is a broken invariant, an ungranted capability, or
        // a limit — not an expected failure. It already points at source and
        // states its own rule, so it is reported as it stands, with the test
        // it came from named.
        Err(error) => {
            let mut diagnostic = error.to_diagnostic();
            diagnostic.message =
                format!("test `{}` failed: {}", test.qualified_name(), error.message);
            Some(TestFailure {
                diagnostic,
                denied_capability: error.denied_capability.as_deref().map(str::to_string),
            })
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

/// A test the lowering refused, as that test's failure, with each gap
/// labelled where it was found, so a reader is not left with a message and no
/// location (issue #602).
fn unlowered(test: &DeclaredTest, items: &[Diagnostic]) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
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
    .at(test.entry.decl.name.span);
    for item in items {
        if let Some(span) = item.primary {
            diagnostic = diagnostic.label(span, item.message.clone());
        }
    }
    diagnostic
}
