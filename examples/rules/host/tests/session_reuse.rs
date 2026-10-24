//! A session's next invocation after one that ran out of its own limit:
//! issue [#681](https://github.com/myuon/cove/issues/681), priority 3, as an
//! embedder meets it.
//!
//! `Session::evaluate` is the unbounded way in and `Session::evaluate_within`
//! the bounded one, and an application serving requests mixes them on one
//! session. `docs/ISOLATION_TESTING.md` says a run that failed leaves nothing
//! the next run can see; `crates/cove-runtime/tests/lifecycle.rs`
//! (`an_unbudgeted_invocation_does_not_inherit_the_last_run_s_budget_*`)
//! found that the budget was the exception — an `evaluate` after an
//! `evaluate_within` whose deadline passed ran under that passed deadline,
//! and failed at once — and this holds the fix of
//! [#684](https://github.com/myuon/cove/issues/684) to this crate's public
//! surface.

use std::sync::Arc;
use std::time::Duration;

use cove_rules::{embedding, package_root, PullRequest, Reviews, RulePackage, REVIEWS};
use cove_runtime::Limits;

const EVALUATE: (&str, &str) = ("rules.embedded", "evaluate");

fn oversized() -> PullRequest {
    let mut big = cove_rules::samples()["req-2"].clone();
    big.files_touched = (0..200).map(|at| format!("src/mod{at}.rs")).collect();
    big.labels = (0..100).map(|at| format!("label-{at}")).collect();
    big
}

#[test]
fn an_unbounded_evaluation_after_a_bounded_one_that_ran_out_is_not_bounded_by_it() {
    for vm in [true, false] {
        let (stopped, next) = cove_runtime::on_cove_stack(|| {
            let package = RulePackage::load(&package_root(), REVIEWS).expect("the rules check");
            let lowering = package
                .lower(EVALUATE.0, EVALUATE.1)
                .expect("the entry lowers");
            let embed = embedding(
                Reviews::new(cove_rules::samples()),
                &["reviews"],
                Limits::default(),
            );
            let samples = cove_rules::samples();
            package.serve(
                Arc::clone(&embed.hosts),
                if vm { Some(&lowering) } else { None },
                |session| {
                    let stopped = session.evaluate_within(
                        Limits {
                            deadline: Some(Duration::ZERO),
                            ..Limits::default()
                        },
                        EVALUATE.0,
                        EVALUATE.1,
                        &oversized(),
                    );
                    let next = session.evaluate(EVALUATE.0, EVALUATE.1, &samples["req-3"]);
                    (stopped, next)
                },
            )
        })
        .expect("a thread to run Cove on");
        assert!(stopped.is_err(), "vm: {vm}: the bounded one ran out");
        assert!(
            next.is_ok(),
            "vm: {vm} (false is the interpreter): the unbounded one after it was stopped by the spent deadline: {next:?}"
        );
    }
}
