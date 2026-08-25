//! The one place the compiler's host-module list and the runtime's can be
//! compared.
//!
//! `cove_sema::resolve::HOST_MODULES` is what refuses a package module that
//! would shadow a host module, and its own doc comment says it "mirrors the
//! modules `cove_runtime::host` registers, which the compiler cannot ask
//! directly because it does not depend on the runtime". A mirror nobody looks
//! into drifts: `http` was registered by the runtime and missing from the
//! compiler's list, so a package module named `http` shadowed the host module
//! silently — `cove check` reported nothing, the checker resolved
//! `http.fetch` to the package's module, and the interpreter resolved it to
//! the host's.
//!
//! `cove-cli` depends on both crates, so this is where the two lists can be
//! held against each other. It is a test rather than a generated table
//! because the dependency only points one way: the compiler must not gain a
//! dependency on the runtime to say what it already knows.

use std::collections::BTreeSet;

use cove_sema::resolve::HOST_MODULES;

/// Every module a run registers must be one the compiler refuses as a package
/// module, and the compiler must refuse no name a run does not register.
///
/// Both directions matter. A missing name shadows a host silently, which is
/// the bug this test was written for. An extra name refuses a package module
/// for colliding with a host that is not there, which the Language Card's
/// "The host chooses the entry function and grants authority at the execution
/// boundary" gives no ground for.
#[test]
fn the_compiler_refuses_exactly_the_host_modules_a_run_registers() {
    let registered: BTreeSet<String> = cove_runtime::shipped_schema()
        .iter()
        .map(|module| module.name.to_string())
        .collect();
    let refused: BTreeSet<String> = HOST_MODULES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(
        refused, registered,
        "`cove_sema::resolve::HOST_MODULES` and `cove_runtime::shipped_schema()` have drifted; \
         a name in only one of them is a host module a package can shadow without a diagnostic"
    );
}

/// The list is sorted, so a host added at the end of it is added in the one
/// place a reader looks.
#[test]
fn the_compiler_s_host_module_list_is_sorted() {
    let mut sorted = HOST_MODULES;
    sorted.sort_unstable();
    assert_eq!(HOST_MODULES, sorted);
}
