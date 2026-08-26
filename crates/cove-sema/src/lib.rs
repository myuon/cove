//! Package loading, module resolution, and derived facts about Cove source.
//!
//! A directory is a module and its name follows its path. Exported
//! declarations are the single source of truth, so everything here is derived
//! from source rather than declared twice.
//!
//! [`Compiler`] is the way in for anything that checks a whole package, and
//! the one thing it can be configured with is the set of Host API schemas it
//! reads: an embedder's modules are checked like shipped ones once it has
//! been handed their descriptions. [`compile`] says why that is the only
//! knob.

pub mod capability;
pub mod compile;
pub mod config;
pub mod package;
pub mod resolve;
pub mod typeck;

pub use capability::{open_reasons, Capability, OpenCall};
pub use compile::Compiler;
pub use config::{Config, RunConfig};
pub use package::{Module, Package, Unit};

// Re-exported so an embedder configuring the checker names one crate: the
// schema it hands to [`Compiler`] is the same type its host module registers
// with, and `cove-runtime` re-exports it for the same reason.
pub use cove_schema::{HostSchemas, ModuleSchema};
