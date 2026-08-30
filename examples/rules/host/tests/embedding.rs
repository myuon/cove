//! What an application embedding `examples/rules` can rely on.
//!
//! Every case here compiles the rule package the way `examples/rules/host`
//! does — through [`cove_rules::RulePackage::load`], with the embedder's own
//! [`cove_rules::REVIEWS`] schema — and then asks one question of it. Between
//! them they cover what issue #90 asks an embedding to demonstrate: a valid
//! result, a schema mismatch caught before anything runs, a mismatch caught at
//! the boundary, a host that fails, a run that spends its fuel, a capability
//! that was not granted, a request identifier that links an invocation to
//! every host call it made, and an additive and a breaking schema change.
//!
//! Everything runs inside [`cove_runtime::on_cove_stack`], because a backend's
//! call-depth limit is a promise about a stack the runtime sized and a test
//! thread's stack is not one it chose.

use std::sync::Arc;

use cove_rules::{
    embedding, package_root, Decision, Fault, PullRequest, Recorded, ReviewPolicy, Reviews,
    RulePackage, REVIEWS, REVIEWS_NEXT, REVIEWS_RENAMED,
};
use cove_runtime::{Limits, Value};

/// The entry an embedding invokes, one pull request at a time.
const ENTRY: (&str, &str) = ("rules.embedded", "decideRequest");

/// The rule package, checked against the schema this crate registers.
fn compiled() -> RulePackage {
    RulePackage::load(&package_root(), REVIEWS).expect("the rule package checks")
}

/// What the decision for `request` is, over the shipped samples, on the VM.
fn decide(request: &str) -> Result<Decision, String> {
    cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            session.decide(ENTRY.0, ENTRY.1, request)
        })
    })
    .expect("a thread to run Cove on")
}

// ------------------------------------------------------------ a valid result

/// The whole point, in one case: one compiled package, six invocations, six
/// typed decisions, and the six the rules were written to reach.
#[test]
fn one_compiled_package_decides_every_open_request() {
    let (decisions, recorded) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        assert!(
            package.notices().is_empty(),
            "a package checked against the schema it will run against warns about nothing: {:?}",
            package.notices()
        );
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        let decisions = package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            cove_rules::samples()
                .keys()
                .map(|request| {
                    session
                        .decide(ENTRY.0, ENTRY.1, request)
                        .expect("every open request decides")
                })
                .collect::<Vec<_>>()
        });
        let recorded = embed.log.lock().unwrap().clone();
        (decisions, recorded)
    })
    .expect("a thread to run Cove on");

    let policies: Vec<ReviewPolicy> = decisions.iter().map(|d| d.policy.clone()).collect();
    assert_eq!(
        policies,
        vec![
            ReviewPolicy::Normal,
            ReviewPolicy::Require {
                reviewers: 2,
                reason: "large_change".to_string(),
            },
            ReviewPolicy::Block {
                reason: "guarded_path:auth/".to_string(),
            },
            ReviewPolicy::Require {
                reviewers: 2,
                reason: "guarded_path_waived:auth/".to_string(),
            },
            ReviewPolicy::Normal,
            ReviewPolicy::Require {
                reviewers: 3,
                reason: "label:breaking-change".to_string(),
            },
        ]
    );

    // The findings come back too, decoded, so the application can show its
    // user why rather than only what.
    assert_eq!(decisions[0].findings.len(), 0);
    assert_eq!(decisions[5].findings.len(), 2);
    assert_eq!(decisions[5].findings[0].rule, "branch");
    assert_eq!(decisions[5].findings[0].severity, "Required");

    // And every decision went back across the boundary under the request that
    // produced it, in order.
    assert_eq!(recorded.len(), 6);
    assert_eq!(
        recorded[2],
        Recorded {
            request: "req-3".to_string(),
            policy: "block".to_string(),
            reviewers: 0,
            trail: "guarded-path:blocking".to_string(),
        }
    );
}

/// The interpreter and the VM reach the same decisions over the embedded
/// entry.
///
/// The differential corpus in `crates/cove-cli/tests/differential.rs` runs
/// every `[run.<name>]` on both backends, and `rules.embedded` is not one:
/// it calls a host module no `cove` command has heard of, so no `[run.<name>]`
/// could name it. This is that comparison, made where the schema exists.
#[test]
fn both_backends_reach_the_same_decisions() {
    let (interpreted, executed) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let requests: Vec<String> = cove_rules::samples().keys().cloned().collect();

        let ast = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        let interpreted = package.serve(Arc::clone(&ast.hosts), None, |session| {
            requests
                .iter()
                .map(|request| session.decide(ENTRY.0, ENTRY.1, request))
                .collect::<Vec<_>>()
        });

        let vm = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        let executed = package.serve(Arc::clone(&vm.hosts), Some(&lowering), |session| {
            requests
                .iter()
                .map(|request| session.decide(ENTRY.0, ENTRY.1, request))
                .collect::<Vec<_>>()
        });
        (interpreted, executed)
    })
    .expect("a thread to run Cove on");

    assert_eq!(interpreted, executed);
}

/// The pull request the host builds in Rust and the one the package declares
/// in Cove are the same pull request.
///
/// They are written twice — `cove_rules::samples` and `rules.fixtures` — and
/// nothing but this holds them together. The two paths differ in everything
/// else: one crosses the Host API boundary as a `reviews.PullRequest` and is
/// converted field by field, the other is built by the package itself and
/// crosses nothing.
#[test]
fn the_host_s_fixtures_and_the_package_s_agree() {
    let (through_the_host, in_the_package) = cove_runtime::on_cove_stack(|| {
        let package = compiled();

        let embedded = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        let through_the_host = package.serve(Arc::clone(&embed.hosts), Some(&embedded), |s| {
            cove_rules::samples()
                .keys()
                .map(|request| s.decide(ENTRY.0, ENTRY.1, request))
                .collect::<Vec<_>>()
        });

        // The control makes no host call at all, so it needs no capability
        // and reaches no boundary.
        let sampled = package
            .lower("rules", "decideSample")
            .expect("the control lowers");
        let bare = embedding(Reviews::new(cove_rules::samples()), &[], Limits::default());
        let in_the_package = package.serve(Arc::clone(&bare.hosts), Some(&sampled), |s| {
            (0..6)
                .map(|at| s.decide("rules", "decideSample", &at.to_string()))
                .collect::<Vec<_>>()
        });
        (through_the_host, in_the_package)
    })
    .expect("a thread to run Cove on");

    assert_eq!(through_the_host, in_the_package);
}

// --------------------------------------------------- schemas and their drift

/// An additive change leaves a package written against the older schema
/// checking and running exactly as it did.
///
/// `REVIEWS_NEXT` adds one operation and one field. Nothing in the rule
/// package calls the operation or reads the field, and nothing about the
/// package changes: it checks, it lowers, and it decides the same way.
#[test]
fn an_additive_schema_change_breaks_nothing() {
    let decisions = cove_runtime::on_cove_stack(|| {
        let package = RulePackage::load(&package_root(), REVIEWS_NEXT)
            .expect("an additive change leaves the package checking");
        assert!(package.notices().is_empty(), "{:?}", package.notices());
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()).with_schema(REVIEWS_NEXT),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            session.decide(ENTRY.0, ENTRY.1, "req-2")
        })
    })
    .expect("a thread to run Cove on");

    assert_eq!(
        decisions.expect("the decision is unchanged").policy,
        ReviewPolicy::Require {
            reviewers: 2,
            reason: "large_change".to_string(),
        }
    );
}

/// A breaking change is reported by the checker, at the line that reads the
/// field, before anything runs.
///
/// This is what handing the schema to `cove_sema::Compiler` buys. Without it
/// the rename would have been discovered by the boundary, on the first
/// invocation, in production, in whatever host call happened to come first.
#[test]
fn a_renamed_field_is_a_check_time_error() {
    let Err(refusal) = RulePackage::load(&package_root(), REVIEWS_RENAMED) else {
        panic!("a field the schema no longer declares is an error");
    };

    assert!(
        refusal.contains("`reviews.PullRequest` has no field `changedLines`"),
        "{refusal}"
    );
    assert!(
        refusal.contains("changedLineCount"),
        "the diagnostic names what the type does declare: {refusal}"
    );
    assert!(
        refusal.contains("embedded.cove"),
        "the diagnostic points at the line that reads the field: {refusal}"
    );
}

// -------------------------------------------------- the boundary, both ways

/// The boundary holds a call to the schema the host declared, whatever the
/// checker did or did not see.
#[test]
fn an_argument_the_schema_does_not_admit_is_refused_at_the_boundary() {
    let embed = embedding(
        Reviews::new(cove_rules::samples()),
        &["reviews"],
        Limits::default(),
    );
    let error = embed
        .hosts
        .call("reviews", "pull", vec![Value::Int(3)])
        .expect_err("an `Int` where the schema declares a `String` is refused");

    assert_eq!(
        error.message,
        "`reviews.pull` was given `Int` as argument 1, but its schema declares `String` there"
    );
}

/// And it holds the host to the same schema on the way out: a `reviews` that
/// answers an `Int` where it declared a `Result<reviews.PullRequest, Error>`
/// is stopped before the value reaches the program.
#[test]
fn a_result_the_host_s_own_schema_does_not_admit_is_refused() {
    let outcome = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()).with_fault(Fault::WrongResultType),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            session.invoke(ENTRY.0, ENTRY.1, &["req-1"]).map(|_| ())
        })
    })
    .expect("a thread to run Cove on");

    let error = outcome.expect_err("a host that breaks its own schema is stopped");
    assert!(
        error.message.contains("reviews.pull") && error.message.contains("Int"),
        "{}",
        error.message
    );
}

/// A capability the run was not granted is refused before the host is
/// reached, so registering `reviews` is not the same as trusting a run to
/// call it.
#[test]
fn an_ungranted_capability_is_refused_before_the_host_is_reached() {
    let outcome = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(Reviews::new(cove_rules::samples()), &[], Limits::default());
        let outcome = package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            session.invoke(ENTRY.0, ENTRY.1, &["req-1"]).map(|_| ())
        });
        let recorded = embed.log.lock().unwrap().clone();
        (outcome, recorded)
    })
    .expect("a thread to run Cove on");

    let (outcome, recorded) = outcome;
    let error = outcome.expect_err("an ungranted call is refused");
    assert!(
        error
            .message
            .contains("requires the `reviews` capability, which this run was not granted"),
        "{}",
        error.message
    );
    assert!(
        recorded.is_empty(),
        "a refused call never reaches the host's implementation"
    );
}

// ----------------------------------------------------------------- failures

/// A request the host does not hold is an ordinary Cove `Err` reaching the
/// embedder, not a failure of the run.
///
/// The difference matters to a caller: this decision did not happen, and the
/// session is still good for the next one.
#[test]
fn a_request_the_host_does_not_hold_is_an_ordinary_err() {
    let refusal = decide("req-nothing").expect_err("an unknown request answers `Err`");
    assert!(
        refusal.contains("no request named `req-nothing`"),
        "{refusal}"
    );
}

/// A host that fails stops the invocation it failed in and leaves the session
/// able to serve the next one.
///
/// Error isolation is what makes compile-once/invoke-many worth doing at all:
/// an engine that had to be rebuilt after a failure would be an engine built
/// per request.
#[test]
fn a_failed_invocation_leaves_the_session_serving() {
    let (failed, after) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");

        // The host is broken for the first invocation and mended for the
        // second, which is one registry and one session throughout.
        let broken = embedding(
            Reviews::new(cove_rules::samples()).with_fault(Fault::Broken),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&broken.hosts), Some(&lowering), |session| {
            let failed = session
                .invoke(ENTRY.0, ENTRY.1, &["req-1"])
                .expect_err("a host that fails stops the invocation")
                .message;
            // The same session, invoked again: a request the host holds and
            // could answer if it were not broken, and then one it can.
            let after = session.decide(ENTRY.0, ENTRY.1, "req-2");
            (failed, after)
        })
    })
    .expect("a thread to run Cove on");

    assert!(
        failed.contains("the review queue is unreachable"),
        "{failed}"
    );
    assert!(
        after.is_err(),
        "the host is still broken, so the next invocation fails too"
    );

    // And with a host that works, the session that saw a failure serves the
    // next request as if nothing had happened.
    let recovered = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            let _ = session.invoke(ENTRY.0, ENTRY.1, &["req-nothing"]);
            session.decide(ENTRY.0, ENTRY.1, "req-2")
        })
    })
    .expect("a thread to run Cove on");

    assert_eq!(
        recovered
            .expect("the session serves after a failed request")
            .policy,
        ReviewPolicy::Require {
            reviewers: 2,
            reason: "large_change".to_string(),
        }
    );
}

/// A fuel limit stops a run, and — this is the part an embedder has to know —
/// it is spent over the whole session rather than reset per invocation.
///
/// A `Budget` belongs to the `HostRegistry`, `set_budget` needs `&mut`, and a
/// backend holds the registry by shared reference for as long as it exists.
/// So there is no way to hand one invocation its own fuel without building a
/// registry, a `Runtime` and a backend for it, which is the thing compiling
/// once was for avoiding. Issue #152 is the gap; this case pins the behaviour
/// as it is.
#[test]
fn fuel_is_spent_over_the_session_and_not_over_one_invocation() {
    let outcomes = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits {
                // One decision spends about 735 instructions, and fuel is
                // charged by the block, so this is enough for the first and
                // not for three.
                fuel: Some(1_200),
                ..Limits::default()
            },
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            (0..3)
                .map(|_| {
                    session
                        .invoke(ENTRY.0, ENTRY.1, &["req-2"])
                        .map(|_| ())
                        .map_err(|error| error.message)
                })
                .collect::<Vec<_>>()
        })
    })
    .expect("a thread to run Cove on");

    assert!(
        outcomes[0].is_ok(),
        "the first invocation is inside the limit: {outcomes:?}"
    );
    let exhausted = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().err())
        .next()
        .expect("the fuel runs out before the third invocation");
    assert!(exhausted.contains("fuel"), "{exhausted}");
}

/// A host-call limit is spent the same way, and stops an invocation at the
/// boundary rather than inside the program.
#[test]
fn a_host_call_limit_stops_an_invocation() {
    let outcome = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits {
                // One decision makes two calls: `pull` and `record`.
                max_host_calls: Some(3),
                ..Limits::default()
            },
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            (0..2)
                .map(|_| {
                    session
                        .invoke(ENTRY.0, ENTRY.1, &["req-2"])
                        .map(|_| ())
                        .map_err(|error| error.message)
                })
                .collect::<Vec<_>>()
        })
    })
    .expect("a thread to run Cove on");

    assert!(outcome[0].is_ok(), "{outcome:?}");
    let refused = outcome[1]
        .as_ref()
        .expect_err("the fourth host call is over the limit");
    assert!(
        refused.contains("host-call limit of 3 exceeded"),
        "{refused}"
    );
}

// ------------------------------------------------------------ attribution

/// Every host call an invocation made is linked to the request identifier
/// that invocation was given.
///
/// The identifier is the first argument of both `reviews` operations, so the
/// `HostCall` events a trace sink sees carry it without the runtime having to
/// know anything about what an application request is.
#[test]
fn every_call_is_linked_to_the_request_that_made_it() {
    let (total, by_request) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            for request in ["req-2", "req-3", "req-6"] {
                session
                    .decide(ENTRY.0, ENTRY.1, request)
                    .expect("every request decides");
            }
        });
        let by_request: Vec<Vec<String>> = ["req-2", "req-3", "req-6"]
            .iter()
            .map(|request| embed.calls.for_request(request))
            .collect();
        (embed.calls.all().len(), by_request)
    })
    .expect("a thread to run Cove on");

    assert_eq!(total, 6, "three invocations, two calls each");
    for made in by_request {
        assert_eq!(made, vec!["reviews.pull", "reviews.record"]);
    }
}

// ------------------------------------------------------------- conversion

/// A pull request built in Rust becomes the struct value the schema names,
/// with every field the schema declares and nothing else.
#[test]
fn a_rust_pull_request_becomes_the_struct_the_schema_declares() {
    let pr = PullRequest {
        id: "pr-1".to_string(),
        title: "t".to_string(),
        author: "a".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 3,
        files_touched: vec!["a.rs".to_string()],
        labels: vec!["docs".to_string()],
        approvals: 1,
        is_draft: false,
        has_tests: true,
    };
    let Value::Struct(value) = pr.to_cove() else {
        panic!("a pull request converts to a struct value");
    };

    assert_eq!(&*value.type_name, "reviews.PullRequest");
    let declared: Vec<&str> = REVIEWS.types[0].fields.iter().map(|f| f.name).collect();
    let built: Vec<&str> = value.fields.iter().map(|(name, _)| &**name).collect();
    assert_eq!(built, declared, "the conversion follows the schema's order");
}
