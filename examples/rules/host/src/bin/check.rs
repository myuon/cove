//! `cove check`, for a rule package written against this application's own
//! host module.
//!
//! # Why this exists
//!
//! The person who writes rules is not the person who wrote the embedder. Their
//! toolchain is `cove fmt`, `cove check` and `cove test`, and `cove check`
//! stops at `use reviews`:
//!
//! ```text
//! warning[cove::resolve::unchecked_host]: no Host API schema describes the
//! host module `reviews`, so calls into it are unchecked
//! ```
//!
//! which is accurate. `reviews` is this crate's, and no `cove` command has
//! heard of it. Issue #151 asked for a flag or a `cove.toml` key that would
//! let one; `cove_sema::compile`'s module doc is where the answer is argued,
//! and the answer is no. A description in a config file is a second
//! description of a module whose first one is Rust, and two descriptions of
//! one thing is the drift ADR 0017 exists to prevent; and a schema would let
//! `cove check` check a package that `cove test` still could not run, because
//! what answers a call is an implementation and no format carries one.
//!
//! So the toolchain is the embedder's to provide, and this is the whole of it.
//! It is not a fork of `cove check`: it reads the same package from disk, runs
//! the same `cove_sema::Compiler`, and renders diagnostics with the same
//! `cove_diag::render`, with one line's difference — the schemas it was handed.
//!
//! ```console
//! $ cargo run -p cove-rules --bin cove-rules-check
//! `rules` checks against `reviews`: 10 files, 6 modules, no notices
//! ```
//!
//! # What it does not do
//!
//! It does not run the package's `test fn` declarations. That needs the
//! `reviews` *implementation* as well as its description, which is the same
//! argument arriving from the other end: an embedder that wants `cove test`
//! registers [`cove_rules::Reviews`] beside the schema and runs the tests
//! against it. Nothing in the way stops that; it is simply more than one line,
//! and this file is the one-line half.

use std::process::ExitCode;

use cove_rules::{package_root, RulePackage, REVIEWS};

fn main() -> ExitCode {
    // The same value `Reviews::module_schema` answers with. That it is one
    // value is the whole point: a checker reading a copy of the description
    // the boundary enforces would be a checker that can be right about a
    // module the run is not going to have.
    let package = match RulePackage::load(&package_root(), REVIEWS) {
        Ok(package) => package,
        Err(report) => {
            eprint!("{report}");
            return ExitCode::FAILURE;
        }
    };

    let notices = package.notices();
    for notice in &notices {
        eprint!("{notice}");
    }

    let cost = package.cost();
    let count = match notices.len() {
        0 => "no notices".to_string(),
        1 => "1 notice".to_string(),
        many => format!("{many} notices"),
    };
    println!(
        "`rules` checks against `{}`: {} files, {} modules, {count}",
        REVIEWS.name, cost.files, cost.modules
    );
    ExitCode::SUCCESS
}
