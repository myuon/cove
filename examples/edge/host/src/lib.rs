//! A multi-tenant, Workers-style HTTP server whose tenants are Cove programs
//! run as isolates.
//!
//! `examples/edge/README.md` is the walkthrough. In one paragraph: each
//! directory under `examples/edge/tenants/` is a tenant, `cove.toml` there
//! grants each one its capabilities, and the server compiles every tenant
//! once at startup ([`deploy`]), refusing any whose entry requires more than
//! it was granted. A request to `/<tenant>/...` gets a fresh [`OwnedVm`] over
//! the tenant's [`PreparedProgram`] and is run on a fixed pool of worker
//! threads ([`server`]). A tenant that calls `upstream.get` — a slow
//! outbound call — parks: the run becomes a [`ParkedVm`] that waits in a
//! timer heap rather than on a thread, and resumes on whichever worker is
//! free when its answer arrives.
//!
//! The APIs it is a demonstration of are `cove_runtime`'s
//! [`PreparedProgram`] and [`OwnedVm`] (one program's encoding shared by
//! every run of it), [`ParkedVm`], [`Step`] and
//! [`HostApi::call_parkable`](cove_runtime::HostApi::call_parkable) (ADR
//! 0080), and the pacing collector of ADR 0081 that keeps a resident run's
//! heap small; and [`YieldRequest`](cove_runtime::YieldRequest) and
//! [`YieldedVm`](cove_runtime::YieldedVm) (ADR 0084), which let the
//! scheduler slice a long run at a safepoint.
//!
//! [`OwnedVm`]: cove_runtime::OwnedVm
//! [`PreparedProgram`]: cove_runtime::PreparedProgram
//! [`ParkedVm`]: cove_runtime::ParkedVm
//! [`Step`]: cove_runtime::Step

pub mod deploy;
pub mod fetch;
pub mod hosts;
pub mod http;
pub mod idle;
pub mod json;
pub mod os;
pub mod picture;
pub mod runq;
pub mod server;
pub mod timeline;
pub mod toolchain;

pub use deploy::{Backend, DeployOptions, State, Tenant};
pub use hosts::Latency;
pub use runq::Discipline;
pub use server::{Isolates, KeepAlive, Server, ServerOptions};
pub use timeline::Recording;

/// Where the demo's tenants live, relative to this crate.
pub fn tenants_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the host crate sits inside examples/edge")
        .join("tenants")
}
