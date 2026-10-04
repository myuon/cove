//! `upstream.fetch`: a real outbound HTTP request, made while the run that
//! asked for it is parked, to the hosts the tenant's allowlist names and
//! nowhere else.
//!
//! The upstream is the server itself — `proxy` fetching `hello` — or a
//! listener the test owns. Nothing here asserts a duration.

use std::time::Duration;

use cove_edge::http::get;
use cove_edge::{DeployOptions, Isolates, KeepAlive, Latency, Server, ServerOptions};

fn start() -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers: 2,
        isolates: Isolates::PerRequest,
        keep_alive: KeepAlive::default(),
        fetchers: 2,
        deploy: DeployOptions {
            tenants: cove_edge::tenants_root(),
            latency: Latency {
                min: Duration::from_millis(1),
                max: Duration::from_millis(1),
            },
            quiet: true,
            blocking_upstream: false,
        },
    })
    .expect("the server starts")
}

/// The number `/_stats` reports under `key`.
fn stat(server: &Server, key: &str) -> i64 {
    let (_, body) = get(server.addr, "/_stats").expect("stats answer");
    let needle = format!("\"{key}\": ");
    let at = body.find(&needle).expect("the key is reported") + needle.len();
    body[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a number")
}

#[test]
fn a_tenant_fetches_another_tenant_on_the_same_server() {
    let server = start();
    let port = server.addr.port();
    let url = format!("http://127.0.0.1:{port}/hello/?name=fetched");
    let parks = stat(&server, "parks");
    let (status, body) = get(server.addr, &format!("/proxy/?url={url}")).unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, format!("{url} said:\nHello, fetched! (GET /)\n"));
    // The proxy's run parked at the fetch, and a worker served `hello` in
    // the meantime — on a server with two.
    assert_eq!(stat(&server, "parks") - parks, 1);
    assert_eq!(stat(&server, "fetches"), 1);

    // An upstream's failure status is the tenant's `Err`, with what it said.
    let (status, body) = get(
        server.addr,
        &format!("/proxy/?url=http://localhost:{port}/nobody/"),
    )
    .unwrap();
    assert_eq!(status, 502, "{body}");
    assert_eq!(
        body,
        format!("`http://localhost:{port}/nobody/` answered 404: no tenant named `nobody`\n")
    );
}

#[test]
fn a_host_off_the_allowlist_is_refused_before_anything_is_sent() {
    let server = start();
    let (status, body) = get(server.addr, "/proxy/?url=http://example.com/").unwrap();
    assert_eq!(status, 502, "{body}");
    assert_eq!(
        body,
        "`example.com` is not on tenant `proxy`'s fetch allowlist (127.0.0.1, localhost)\n"
    );
    let (status, body) = get(server.addr, "/proxy/?url=https://127.0.0.1/").unwrap();
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("https needs TLS"), "{body}");
    // Refused at the boundary: answered at once, not parked, not fetched.
    assert_eq!(stat(&server, "parks"), 0);
    assert_eq!(stat(&server, "fetches"), 0);
}

#[test]
fn a_tenant_with_no_allowlist_may_fetch_from_nowhere() {
    use cove_runtime::{HostApi, Value};
    // `aggregate` is granted `upstream` and named in no `edge.toml` table:
    // the capability lets it call out, and the empty allowlist lets the
    // call go nowhere.
    let host = cove_edge::hosts::Upstream {
        latency: Latency {
            min: Duration::ZERO,
            max: Duration::ZERO,
        },
        blocking: false,
        tenant: "aggregate".to_string(),
        allow: Default::default(),
    };
    let answer = host
        .call("fetch", vec![Value::string("http://127.0.0.1:1/")])
        .unwrap();
    let message = answer.err_payload().expect("an `Err`")[0].to_string();
    assert!(
        message.contains(
            "`127.0.0.1` is not on tenant `aggregate`'s fetch allowlist (it has none; see tenants/edge.toml)"
        ),
        "{message}"
    );
}
