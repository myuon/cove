//! Errors raised while executing Cove code.
//!
//! A `RuntimeError` is a broken invariant, an ungranted capability, or a limit
//! the host imposed. Ordinary expected failure uses `Result` inside the
//! language instead.

use cove_diag::{Diagnostic, Span};

#[derive(Clone, Debug)]
pub struct RuntimeError {
    pub message: String,
    pub span: Option<Span>,
    pub rule: Option<String>,
    pub help: Option<String>,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            span: None,
            rule: None,
            help: None,
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
