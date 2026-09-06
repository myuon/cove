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

/// How many call-site spans [`RuntimeError::with_chain`] keeps, innermost
/// first.
///
/// A bound rather than the whole call stack, because the chain is built for
/// every error whether the recursion behind it was three frames deep or
/// the tree-walking interpreter's 256-frame limit — and a diagnostic naming 256
/// callers would be unreadable long before it is untruthful. Eight is enough
/// to show a handful of library layers above the fault and to say, honestly,
/// that there were more: it is not derived from anything else that is
/// eight, it is simply small enough that a chain this long is already a
/// wall of `-->` blocks rather than a hint.
pub const MAX_CALL_CHAIN: usize = 8;

/// The call-site spans a [`RuntimeError`] carries, and how many more there
/// were than [`MAX_CALL_CHAIN`] keeps.
///
/// Its own type, boxed inside [`RuntimeError`], because most errors never
/// have one: the entry's own failures — most of the eleven end-to-end cases
/// that raise one, going into issue #258 — carry an empty chain and would
/// otherwise pay for `Vec`'s three words and this `usize` inline on every
/// `RuntimeError` there is. `Option<Box<Chain>>` pays a pointer's width
/// instead, and nothing at all for the common empty case.
#[derive(Clone, Debug, Default)]
struct Chain {
    /// Innermost first — not including this error's own
    /// [`RuntimeError::span`], which is where it happened rather than who
    /// called it.
    sites: Vec<Span>,
    /// How many call-site spans past [`MAX_CALL_CHAIN`] were dropped to keep
    /// [`Chain::sites`] bounded — the frames further from the fault, since
    /// the innermost ones are the ones kept.
    omitted: usize,
}

#[derive(Clone, Debug)]
pub struct RuntimeError {
    pub message: String,
    pub span: Option<Span>,
    /// `Box<str>` rather than `String`, here and for `help` and
    /// `denied_capability`, because none of the three is ever grown after it
    /// is set and a `String`'s capacity word is eight bytes this type pays
    /// on every `Result` that can carry it — of which there are some three
    /// hundred and sixty signatures. The three together are twenty-four
    /// bytes, which is the difference between tripping
    /// `clippy::result_large_err` and clearing it with room.
    pub rule: Option<Box<str>>,
    pub help: Option<Box<str>>,
    /// `None` until [`RuntimeError::with_chain`] attaches one; see [`Chain`]
    /// for why this is boxed rather than the two fields it holds.
    chain: Option<Box<Chain>>,
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
    /// The capability the Host API boundary refused this call for, when a
    /// capability was the reason.
    ///
    /// [`RunOutcome::HostBoundary`] is set for everything the boundary
    /// rejects — an unknown module, an operation that does not exist, an
    /// argument or result the schema does not admit, an exhausted budget —
    /// so it cannot answer whether this particular run was simply not
    /// granted enough. This field can: only the grant check in
    /// [`crate::host::HostRegistry`] sets it, and only with the capability it
    /// refused.
    pub denied_capability: Option<Box<str>>,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            span: None,
            rule: None,
            help: None,
            chain: None,
            outcome: RunOutcome::Invariant,
            denied_capability: None,
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.span.get_or_insert(span);
        self
    }

    /// The call-site spans of the calls that were live when this was raised,
    /// innermost first, bounded to [`MAX_CALL_CHAIN`] entries by
    /// [`RuntimeError::with_chain`].
    pub fn chain(&self) -> &[Span] {
        self.chain.as_deref().map_or(&[], |chain| &chain.sites)
    }

    /// How many call-site spans past [`MAX_CALL_CHAIN`] were dropped to keep
    /// [`RuntimeError::chain`] bounded.
    pub fn chain_omitted(&self) -> usize {
        self.chain.as_deref().map_or(0, |chain| chain.omitted)
    }

    /// Attaches `sites` as the call chain, innermost first, keeping the
    /// innermost [`MAX_CALL_CHAIN`] and recording how many more were dropped.
    ///
    /// A no-op once a chain is attached. The VM and the interpreter each have
    /// exactly one place that calls this — where the error leaves the
    /// machine, and inside `call_target` for every level of the
    /// interpreter's own recursion — and the second of those runs once per
    /// frame the error unwinds through. The guard is what makes that safe:
    /// the first frame to see the error attaches the whole chain it can
    /// still see, and every frame further out finds one already there and
    /// leaves it alone, rather than overwriting it with the shorter chain
    /// its own, later vantage point would otherwise compute.
    pub fn with_chain(mut self, sites: impl IntoIterator<Item = Span>) -> Self {
        if self.chain.is_some() {
            return self;
        }
        let mut sites = sites.into_iter();
        let sites_kept = (&mut sites).take(MAX_CALL_CHAIN).collect();
        let omitted = sites.count();
        self.chain = Some(Box::new(Chain {
            sites: sites_kept,
            omitted,
        }));
        self
    }

    pub fn with_rule(mut self, rule: impl Into<Box<str>>) -> Self {
        self.rule = Some(rule.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<Box<str>>) -> Self {
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

    /// Records `capability` as the one the Host API boundary refused this
    /// call for.
    ///
    /// Call this only from the grant check itself: it is what lets a caller
    /// tell "this run was simply not granted enough" apart from the rest of
    /// what [`RunOutcome::HostBoundary`] covers.
    pub fn with_denied_capability(mut self, capability: impl Into<Box<str>>) -> Self {
        self.denied_capability = Some(capability.into());
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
        // Every call-site span becomes a secondary label, innermost first, so
        // a fault raised inside a library call still shows the source line
        // that called it and not only the library's own. The outermost one
        // shown also says when the bound cut the chain short, because that
        // is the label a reader who wants the rest would look at next.
        let chain = self.chain();
        let chain_omitted = self.chain_omitted();
        let last = chain.len().saturating_sub(1);
        for (i, span) in chain.iter().enumerate() {
            let message = if i == last && chain_omitted > 0 {
                format!(
                    "called from here ({chain_omitted} more call{} not shown)",
                    if chain_omitted == 1 { "" } else { "s" }
                )
            } else {
                "called from here".to_string()
            };
            diagnostic = diagnostic.label(*span, message);
        }
        diagnostic
    }
}
