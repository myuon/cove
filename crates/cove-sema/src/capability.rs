//! Capabilities required by Cove code.
//!
//! Cove code has no ambient authority. External operations are typed Host APIs,
//! and the compiler derives which capabilities each function needs from its
//! call graph.

use std::fmt;

/// A coarse capability named in `cove.toml`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Capability(pub String);

impl Capability {
    pub fn new(name: impl Into<String>) -> Self {
        Capability(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a declaration's derived capability set is a lower bound rather than
/// the whole of what calling it can reach.
///
/// ADR 0014 makes a derived set a lower bound and nothing more: it names the
/// capabilities the call graph can see, and a call the call graph cannot
/// follow is reported here rather than left out in silence. A declaration
/// carrying none of these is *capability-closed* — the call graph followed
/// every call it makes, so its set is the whole of what it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpenCall {
    /// The body calls a value rather than a declaration: `work()` where
    /// `work` is a parameter, a local bound out of a collection, or the
    /// result of another call. What that value requires belongs to whoever
    /// wrote it, which is somewhere this call graph does not lead.
    FunctionValue,
    /// The body calls a method on a value whose implementation its caller
    /// chose: a `dyn Trait` value, or a value of a generic parameter. The
    /// conformance that runs is picked where the value was made.
    DynamicDispatch,
    /// Every call in this body is one the call graph followed, but one of
    /// them leads to a capability-open declaration, so the incompleteness
    /// reaches here too.
    ReachedOpenCall,
}

impl OpenCall {
    /// The clause a report prints to say why a set is a lower bound.
    pub fn reason(self) -> &'static str {
        match self {
            OpenCall::FunctionValue => "calls a function value",
            OpenCall::DynamicDispatch => "dispatches through a `dyn` or generic value",
            OpenCall::ReachedOpenCall => "calls a capability-open declaration",
        }
    }
}

/// The reasons `open` carries, as the one clause every report prints after
/// `capability-open:`.
///
/// One rendering rather than one per command: `cove outline`, `cove impact`,
/// and `cove test` are all saying the same thing about the same fact.
pub fn open_reasons<'a>(open: impl IntoIterator<Item = &'a OpenCall>) -> String {
    open.into_iter()
        .map(|reason| reason.reason())
        .collect::<Vec<_>>()
        .join(", ")
}
