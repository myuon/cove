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
//! There are two ways into the program and both are here. `evaluate` takes the
//! pull request as an argument, which is what issue #150 asked for and what an
//! application should use; `decideRequest` takes a request identifier as a
//! process argument and fetches the pull request back across the Host API
//! boundary, which is what an embedding had to do before and is now the
//! control the boundary is measured with. The cases that matter most are the
//! two that hold them to each other: they must reach the same decision, and
//! each must reach the same decision on both backends.
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
const EVALUATE: (&str, &str) = ("rules.embedded", "evaluate");

/// The control it is measured against: the same decision, reached with the
/// request identifier as a process argument and the pull request fetched back
/// across the Host API boundary.
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

/// The same decision reached the other way: the pull request handed in as an
/// argument, on `backend`.
fn evaluate(pr: &PullRequest, grants: &[&str], vm: bool) -> Result<Decision, String> {
    cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package
            .lower(EVALUATE.0, EVALUATE.1)
            .expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            grants,
            Limits::default(),
        );
        let answer = package.serve(
            Arc::clone(&embed.hosts),
            if vm { Some(&lowering) } else { None },
            |session| session.evaluate(EVALUATE.0, EVALUATE.1, pr),
        );
        assert!(
            embed.calls.all().is_empty(),
            "a direct invocation crosses no Host API boundary: {:?}",
            embed.calls.all()
        );
        answer
    })
    .expect("a thread to run Cove on")
}

// -------------------------------------------------------------- the way in

/// What issue #150 asked for, in one case: the application hands the rules a
/// pull request it built and reads the decision back.
///
/// The six samples, the six decisions, and — the part that is not about
/// values — not one host call between them. The pull request goes in as an
/// argument and the decision comes out as a result, so the embedder's own
/// `reviews` module is not reached, no capability is asked for, and a trace
/// sink watching the boundary sees nothing to record.
#[test]
fn a_host_evaluates_every_open_request_with_a_value_it_built() {
    let policies: Vec<ReviewPolicy> = cove_rules::samples()
        .values()
        .map(|pr| {
            evaluate(pr, &[], true)
                .expect("every pull request decides")
                .policy
        })
        .collect();
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
}

/// The two ways in reach the same decision.
///
/// `evaluate` takes the pull request as an argument and `decideRequest` fetches
/// it across the Host API boundary, and this is what says the difference
/// between them is cost and nothing else. It is also what would notice if the
/// field-by-field rebuild in `rules.embedded.pullRequest` and the struct the
/// host builds for an invocation stopped meaning the same thing.
#[test]
fn the_two_ways_in_reach_the_same_decision() {
    for (request, pr) in cove_rules::samples() {
        assert_eq!(
            evaluate(&pr, &[], true).expect("the direct invocation decides"),
            decide(&request).expect("the boundary invocation decides"),
            "the two ways into `{request}` disagree"
        );
    }
}

/// Both backends answer one invocation the same way, which the differential
/// corpus cannot say: it runs entries the way `cove run` does, and no
/// `[run.<name>]` table can hand a struct to a function.
#[test]
fn both_backends_answer_the_direct_invocation_the_same_way() {
    for pr in cove_rules::samples().values() {
        assert_eq!(
            evaluate(pr, &[], true).expect("the VM decides"),
            evaluate(pr, &[], false).expect("the interpreter decides"),
            "the two backends disagree about `{}`",
            pr.id
        );
    }
}

/// An argument the declaration does not admit is refused before the first
/// instruction, in the checker's words.
///
/// Three ways to get it wrong, and all three are the host's rather than the
/// program's: the wrong type, the wrong struct, and the right struct carrying
/// the wrong fields. The last is the one that has to be refused rather than
/// merely reported — the lowering reads a declared struct's field by index, so
/// a value whose fields are not the declaration's would read past the end of
/// one or answer the wrong field silently.
#[test]
fn an_argument_the_rules_do_not_declare_is_refused_before_anything_runs() {
    // The argument is built inside the closure rather than handed to it,
    // because a `Value` is `Rc`-based and cannot cross a thread boundary --
    // which is the same reason `RulePackage::serve` takes a closure.
    let refused = |argument: fn() -> Value| -> String {
        cove_runtime::on_cove_stack(move || {
            let package = compiled();
            let lowering = package
                .lower(EVALUATE.0, EVALUATE.1)
                .expect("the entry lowers");
            let embed = embedding(Reviews::new(cove_rules::samples()), &[], Limits::default());
            package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
                session
                    .invoke(EVALUATE.0, EVALUATE.1, vec![argument()])
                    .expect_err("this argument must be refused")
                    .message
            })
        })
        .expect("a thread to run Cove on")
    };

    assert_eq!(
        refused(|| Value::Int(1)),
        "`rules.embedded.evaluate` was given `Int` as argument 1, but it declares `rules.policy.PullRequest` there"
    );
    assert_eq!(
        refused(|| {
            cove_rules::samples()
                .remove("req-1")
                .expect("the sample exists")
                .to_cove()
        }),
        "`rules.embedded.evaluate` was given `reviews.PullRequest` as argument 1, but it declares `rules.policy.PullRequest` there",
        "the host's own type and the package's are two types, and always were"
    );
    assert!(
        refused(missing_fields)
        .starts_with(
            "`rules.embedded.evaluate` was given a `rules.policy.PullRequest` carrying `id` as argument 1, but"
        ),
        "{}",
        refused(missing_fields)
    );
}

/// A `rules.policy.PullRequest` carrying one of its ten fields.
fn missing_fields() -> Value {
    Value::structure(
        "rules.policy.PullRequest",
        [("id", Value::Str("pr-1".into()))],
    )
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
            session.run(ENTRY.0, ENTRY.1, &["req-1"]).map(|_| ())
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
            session.run(ENTRY.0, ENTRY.1, &["req-1"]).map(|_| ())
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
                .run(ENTRY.0, ENTRY.1, &["req-1"])
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
            let _ = session.run(ENTRY.0, ENTRY.1, &["req-nothing"]);
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

/// A budget installed on the registry is spent over the whole session.
///
/// This is what a `cove run` gets and it is right for what a `cove run` is,
/// which is one invocation. For an application that decides one pull request
/// per request it is a limit on the *process*: the first decision is inside
/// it, and by the third there is no fuel left for anybody. Held here because
/// it is still the behaviour of `embedding(..., limits)`, and because it is
/// the control the case below is read against.
#[test]
fn a_budget_on_the_registry_is_spent_over_every_invocation() {
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
                        .run(ENTRY.0, ENTRY.1, &["req-2"])
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

/// **The same fuel, handed to each invocation, bounds each invocation.**
///
/// The case above and this one differ in one call — `evaluate` against
/// `evaluate_within` — and in nothing else: one compiled package, one
/// `Runtime`, one `Vm`, one registry. Three requests that each fit the limit
/// all answer, where three requests sharing it did not, and the fourth here
/// is a request too big for its own limit, which stops and leaves the session
/// serving the fifth.
///
/// That is the whole of issue #152. An application running somebody else's
/// rules wants to be told when a rule loops rather than to stop serving, and
/// before this the only way to get a limit per request was to build a
/// registry, a `Runtime` and a backend per request — the 168 allocations of
/// table rebuilding `examples/rules/README.md` measures, on top of a request
/// that costs 237.
#[test]
fn fuel_handed_to_one_invocation_bounds_that_invocation_alone() {
    let (generous, mean, after) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package
            .lower(EVALUATE.0, EVALUATE.1)
            .expect("the entry lowers");
        // Nothing bounds the session, so every bound below is the request's.
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        let samples = cove_rules::samples();
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            let mut each = |limits: Limits, request: &str| {
                session.evaluate_within(limits, EVALUATE.0, EVALUATE.1, &samples[request])
            };
            let generous: Vec<Result<Decision, String>> = ["req-2", "req-2", "req-2"]
                .into_iter()
                .map(|request| {
                    each(
                        Limits {
                            fuel: Some(1_200),
                            ..Limits::default()
                        },
                        request,
                    )
                })
                .collect();
            let mean = each(
                Limits {
                    fuel: Some(50),
                    ..Limits::default()
                },
                "req-2",
            );
            // And the session still serves, with nothing carried over from
            // the request that ran out.
            let after = each(
                Limits {
                    fuel: Some(1_200),
                    ..Limits::default()
                },
                "req-3",
            );
            (generous, mean, after)
        })
    })
    .expect("a thread to run Cove on");

    assert!(
        generous.iter().all(Result::is_ok),
        "each request has its own fuel: {generous:?}"
    );
    assert!(
        mean.as_ref()
            .expect_err("a request under its own limit stops")
            .contains("fuel"),
        "{mean:?}"
    );
    assert_eq!(
        after.expect("the session serves the next request").policy,
        ReviewPolicy::Block {
            reason: "guarded_path:auth/".to_string(),
        }
    );
}

/// A host-call limit belongs to a request the same way, and is the control
/// that bounds what one request may do to the outside world.
///
/// ADR 0024: `max_host_calls` bounds effects exactly, where fuel bounds work
/// and bounds effects only to within a straight line. So this is the limit an
/// application actually reaches for, and it is per request or it is not much
/// use — one decision makes two calls, `pull` and `record`, and a limit of two
/// spent over a session is a limit that permits one request ever.
#[test]
fn a_host_call_limit_handed_to_one_invocation_bounds_that_invocation_alone() {
    let (allowed, refused, after) = cove_runtime::on_cove_stack(|| {
        let package = compiled();
        let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
        let embed = embedding(
            Reviews::new(cove_rules::samples()),
            &["reviews"],
            Limits::default(),
        );
        package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
            let two = || Limits {
                max_host_calls: Some(2),
                ..Limits::default()
            };
            let allowed: Vec<Result<Decision, String>> = (0..3)
                .map(|_| session.decide_within(two(), ENTRY.0, ENTRY.1, "req-2"))
                .collect();
            let refused = session.decide_within(
                Limits {
                    max_host_calls: Some(1),
                    ..Limits::default()
                },
                ENTRY.0,
                ENTRY.1,
                "req-2",
            );
            let after = session.decide_within(two(), ENTRY.0, ENTRY.1, "req-2");
            (allowed, refused, after)
        })
    })
    .expect("a thread to run Cove on");

    assert!(
        allowed.iter().all(Result::is_ok),
        "two calls each, three requests, two allowed each: {allowed:?}"
    );
    assert!(
        refused
            .as_ref()
            .expect_err("a second call is over a limit of one")
            .contains("host-call limit of 1 exceeded"),
        "{refused:?}"
    );
    assert!(after.is_ok(), "{after:?}");
}

/// The same limit per invocation on both backends.
///
/// A budget is not a backend's, so the interpreter and the VM answer it the
/// same way. What they do not owe each other is the *number*: ADR 0024 makes a
/// fuel limit non-portable, so this uses `max_host_calls`, which counts calls
/// and not work and therefore means the same thing on both.
#[test]
fn both_backends_bound_one_invocation_the_same_way() {
    for vm in [false, true] {
        let (allowed, refused) = cove_runtime::on_cove_stack(|| {
            let package = compiled();
            let lowering = package.lower(ENTRY.0, ENTRY.1).expect("the entry lowers");
            let embed = embedding(
                Reviews::new(cove_rules::samples()),
                &["reviews"],
                Limits::default(),
            );
            package.serve(
                Arc::clone(&embed.hosts),
                if vm { Some(&lowering) } else { None },
                |session| {
                    let allowed = session.decide_within(
                        Limits {
                            max_host_calls: Some(2),
                            ..Limits::default()
                        },
                        ENTRY.0,
                        ENTRY.1,
                        "req-2",
                    );
                    let refused = session.decide_within(
                        Limits {
                            max_host_calls: Some(1),
                            ..Limits::default()
                        },
                        ENTRY.0,
                        ENTRY.1,
                        "req-2",
                    );
                    (allowed, refused)
                },
            )
        })
        .expect("a thread to run Cove on");

        assert!(allowed.is_ok(), "on backend vm={vm}: {allowed:?}");
        assert!(
            refused
                .as_ref()
                .expect_err("the second call is over the limit")
                .contains("host-call limit of 1 exceeded"),
            "on backend vm={vm}: {refused:?}"
        );
    }
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
    let value = pr.to_cove();
    let Some(fields) = value.fields() else {
        panic!("a pull request converts to a struct value");
    };

    assert_eq!(value.declared_type(), Some("reviews.PullRequest"));
    let declared: Vec<&str> = REVIEWS.types[0].fields.iter().map(|f| f.name).collect();
    let built: Vec<&str> = fields.map(|(name, _)| name).collect();
    assert_eq!(built, declared, "the conversion follows the schema's order");
}

/// `labels` crosses as the `Set` it is, and nothing on the Cove side turns it
/// into one.
///
/// It used to cross as an `Array<String>` and `rules.policy.PullRequest`
/// carried a `labelSet` that walked it into a set wherever a rule asked a
/// membership question — a loop written into the module that is supposed to be
/// about review policy, for a limitation of the schema vocabulary. Issue #153
/// gave `HostType` a `Set`, so the host builds one and the rules ask it
/// directly.
#[test]
fn labels_cross_as_the_set_a_membership_question_wants() {
    let pr = PullRequest {
        labels: vec!["docs".to_string(), "docs".to_string()],
        ..cove_rules::samples()["req-1"].clone()
    };
    let value = pr.to_cove();
    let labels = value
        .field("labels")
        .expect("the field the schema declares");
    let Some(carried) = labels.elements() else {
        panic!("`labels` crosses as a `Set`, not as {labels}");
    };
    assert_eq!(carried.count(), 1, "a label written twice is carried once");

    // And the schema says so, which is what makes it checked at both ends.
    let declared = REVIEWS.types[0]
        .fields
        .iter()
        .find(|field| field.name == "labels")
        .expect("`labels` is declared");
    assert_eq!(declared.ty.to_string(), "Set<String>");
}

/// The schema this crate registers declares only types some value could have.
///
/// One thing a `HostType` can say and no value satisfy: a `Set` element or a
/// `Map` key that Cove's `MapKey` restriction does not admit. It is refused
/// where the schema is read, and for an embedder that means one assertion over
/// its own table, in the test file it already has. `Set<reviews.PullRequest>`
/// would be the mistake here, and it would otherwise be found by whichever
/// call first carried a value.
#[test]
fn the_schema_declares_only_types_a_value_could_have() {
    for schema in [REVIEWS, REVIEWS_NEXT, REVIEWS_RENAMED] {
        if let Err(fault) = schema.validate() {
            panic!("`{}`: {fault}", schema.name);
        }
    }
}
