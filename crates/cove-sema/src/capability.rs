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
