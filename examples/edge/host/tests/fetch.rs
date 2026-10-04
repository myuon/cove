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
        timeline: None,
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

#[test]
fn a_timed_out_run_s_fetch_is_aborted_and_the_upstream_sees_it_go() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;

    // An upstream that takes the request and never answers. What it reports
    // is what happened to the connection afterwards: closed by the server's
    // abort, or still open when it stopped waiting.
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = upstream.local_addr().unwrap().port();
    let (told, heard) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..n]);
        }
        // Far longer than `proxy`'s 500 ms deadline, and far shorter than
        // the 30 s the fetch would otherwise wait for an answer.
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let closed = matches!(stream.read(&mut chunk), Ok(0));
        told.send(closed).unwrap();
    });

    let server = start();
    let (status, body) = get(
        server.addr,
        &format!("/proxy/?url=http://127.0.0.1:{port}/never"),
    )
    .unwrap();
    assert_eq!(status, 504, "{body}");
    assert!(
        body.contains("execution stopped: wall-clock deadline of 500ms exceeded"),
        "{body}"
    );
    assert!(body.contains("proxy/proxy.cove:13"), "{body}");
    assert!(
        heard.recv().unwrap(),
        "the upstream saw its connection closed, not left waiting"
    );
    assert_eq!(stat(&server, "fetches_aborted"), 1);
    assert_eq!(stat(&server, "timeouts"), 1);
    assert_eq!(stat(&server, "parked"), 0);
}
