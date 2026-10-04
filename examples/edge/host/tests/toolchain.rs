//! `cove-edge check` and `cove-edge test`, called as functions.
//!
//! The answer to issue #151 for this embedder: the server's schemas and the
//! server's hosts, in a checker and a test runner it ships itself.

use std::path::PathBuf;

use cove_edge::toolchain::{check, test, TestOptions};
use cove_edge::Latency;

fn no_latency() -> TestOptions {
    TestOptions {
        latency: Latency {
            min: std::time::Duration::ZERO,
            max: std::time::Duration::ZERO,
        },
        filter: None,
    }
}

/// A tenants directory of its own, with one tenant, `bad`, granted `kv`.
fn scratch(name: &str, source: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("cove-edge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bad")).unwrap();
    std::fs::write(
        root.join("cove.toml"),
        "[run.bad]\nentry = \"bad.handle\"\nallow = [\"kv\"]\n",
    )
    .unwrap();
    std::fs::write(root.join("bad/bad.cove"), source).unwrap();
    root
}

#[test]
fn check_sees_the_server_s_schemas_and_refuses_what_the_server_refuses() {
    let report = check(&cove_edge::tenants_root(), &[]).unwrap();
    // `cove check` in `tenants/` warns nineteen times that `edge`, `kv`,
    // `log` and `upstream` are undescribed; with the schemas, nothing.
    assert_eq!(report.err, "", "no notices against the real schemas");
    assert!(
        report
            .out
            .contains("hello      requires [-]  granted [-]  ok\n"),
        "{}",
        report.out
    );
    assert!(
        report
            .out
            .contains("counter    requires [kv, log]  granted [kv, log]  ok\n"),
        "{}",
        report.out
    );
    assert!(
        report.out.contains(
            "greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant\n"
        ),
        "{}",
        report.out
    );
    assert!(
        report.out.contains(
            "proxy      requires [upstream]  granted [upstream]  fetch [127.0.0.1, localhost]  ok\n"
        ),
        "the fetch allowlist from edge.toml: {}",
        report.out
    );
    assert!(!report.ok, "a refused tenant fails the check");

    let report = check(
        &cove_edge::tenants_root(),
        &["hello".into(), "counter".into(), "aggregate".into()],
    )
    .unwrap();
    assert!(report.ok, "{}{}", report.err, report.out);
    assert!(check(&cove_edge::tenants_root(), &["nobody".into()]).is_err());
}

#[test]
fn test_runs_each_tenant_s_tests_with_the_server_s_hosts() {
    let report = test(&cove_edge::tenants_root(), &[], &no_latency()).unwrap();
    assert!(report.ok, "{}{}", report.err, report.out);
    for line in [
        "ok    hello      hello.greetsWhoeverTheQueryNames",
        // `kv` is the server's store, so a count survives from one call of
        // `handle` to the next within a test.
        "ok    counter    counter.countsEachPathApart",
        "ok    aggregate  aggregate.aServiceThatIsDownIsOneLineOfTheAnswer",
        // The same module under the other tenant's grant and limits.
        "ok    impatient  aggregate.aServiceThatIsDownIsOneLineOfTheAnswer",
        // The real `upstream`, filtered by `proxy`'s allowlist.
        "ok    proxy      proxy.aHostOffTheAllowlistIsRefused",
    ] {
        assert!(report.out.contains(line), "{line}\n{}", report.out);
    }
}

#[test]
fn a_failing_test_points_at_its_assertion_and_an_ungranted_one_names_the_grant() {
    let root = scratch(
        "failing",
        "use edge\nuse kv\nuse log\n\n/// Answers.\n\
         export fn handle(request: edge.Request) -> edge.Response {\n  \
         kv.put(\"k\", request.path)\n  \
         edge.Response(status: 200, contentType: \"text/plain\", body: \"x\")\n}\n\n\
         /// Fails.\ntest fn fails() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n\n\
         /// Reaches `log`, which `bad` is not granted.\n\
         test fn logs() -> Result<Unit, Error> {\n  log.info(\"hi\")\n  Ok(())\n}\n",
    );
    let report = test(&root, &[], &no_latency()).unwrap();
    assert!(!report.ok);
    assert!(
        report.out.ends_with("ran 2 test(s), 0 passed, 2 failed\n"),
        "{}",
        report.out
    );
    assert!(
        report
            .err
            .contains("test `bad.fails` failed: assertion failed: `1 + 1` is `2`, expected `3`"),
        "{}",
        report.err
    );
    assert!(report.err.contains("bad/bad.cove:13:3"), "{}", report.err);
    assert!(
        report.err.contains(
            "test `bad.logs` requires `log`, which cove.toml does not grant tenant `bad`"
        ),
        "{}",
        report.err
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn check_prints_a_type_error_as_cove_check_would() {
    let root = scratch(
        "mistyped",
        "use edge\n\n/// Answers.\n\
         export fn handle(request: edge.Request) -> edge.Response {\n  \
         edge.Response(status: \"200\", contentType: \"text/plain\", body: request.path)\n}\n",
    );
    let report = check(&root, &[]).unwrap();
    assert!(!report.ok);
    assert!(report.err.starts_with("error["), "{}", report.err);
    assert!(report.err.contains("bad/bad.cove:5:"), "{}", report.err);
    assert!(
        report.out.contains("REFUSED: does not compile"),
        "{}",
        report.out
    );
    let _ = std::fs::remove_dir_all(root);
}
