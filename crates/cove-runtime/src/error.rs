//! Errors raised while executing Cove code.
//!
//! A `RuntimeError` is a broken invariant, an ungranted capability, or a limit
//! the host imposed. Ordinary expected failure uses `Result` inside the
//! language instead.
//!
//! Which of the three it is travels with the error, so that a run that ends
//! with one can say so in its trace without reading its message back.

use cove_diag::{Diagnostic, Span};

use crate::trace::RunOutcome;

#[derive(Clone, Debug)]
pub struct RuntimeError {
    pub message: String,
    pub span: Option<Span>,
    pub rule: Option<String>,
    pub help: Option<String>,
    /// Which of the three this error is, for the terminal trace event of a
    /// run that ends with it.
    ///
    /// The default is [`RunOutcome::Invariant`], because that is what most of
    /// them are and because it is the honest answer for an error raised by
    /// code that knows nothing about limits or boundaries. The two parties
    /// that do know say so: [`crate::budget::Budget`] names the limit it
    /// stopped the run for, and [`crate::host::HostRegistry`] names the Host
    /// API boundary when it is the boundary that refused. It is never
    /// [`RunOutcome::Success`] or [`RunOutcome::Error`], which are what a run
    /// that did not fail reports.
    pub outcome: RunOutcome,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            span: None,
            rule: None,
            help: None,
            outcome: RunOutcome::Invariant,
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.span.get_or_insert(span);
        self
    }

    pub fn with_rule(mut self, rule: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Classifies this error as `outcome` for the terminal trace event.
    ///
    /// A classification set once is kept: the innermost party to a failure is
    /// the one that knows what it was, and an error travelling outward
    /// through a host call or a callback must not be relabelled by whatever
    /// it passes through on the way.
    pub fn with_outcome(mut self, outcome: RunOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    pub fn to_diagnostic(&self) -> Diagnostic {
        let mut diagnostic = Diagnostic::error("cove::runtime", self.message.clone());
        if let Some(span) = self.span {
            diagnostic = diagnostic.at(span);
        }
        if let Some(rule) = &self.rule {
            diagnostic = diagnostic.rule(rule.clone());
        }
        if let Some(help) = &self.help {
            diagnostic = diagnostic.help(help.clone());
        }
        diagnostic
    }
}
