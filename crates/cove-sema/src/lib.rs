//! Package loading, module resolution, and derived facts about Cove source.
//!
//! A directory is a module and its name follows its path. Exported
//! declarations are the single source of truth, so everything here is derived
//! from source rather than declared twice.

pub mod capability;
pub mod config;
pub mod package;
pub mod resolve;
pub mod typeck;

pub use capability::{open_reasons, Capability, OpenCall};
pub use config::{Config, RunConfig};
pub use package::{Module, Package, Unit};
