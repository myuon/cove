//! HTTP/1.1 keep-alive: one connection, many requests, and every way a
//! connection ends.
//!
//! Nothing here asserts a duration. The idle timeout is observed by waiting
//! — with a generous bound — for the server to close a connection it was
//! told to close after 100 ms.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use cove_edge::http::{get, Client};
use cove_edge::{DeployOptions, Isolates, KeepAlive, Latency, Server, ServerOptions};

fn start(keep_alive: KeepAlive) -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers: 2,
        isolates: Isolates::PerRequest,
        keep_alive,
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

/// The number `/_stats` reports under `key`, asked on a connection of its
/// own.
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
fn one_connection_carries_many_requests_until_the_client_says_close() {
    let server = start(KeepAlive::default());
    let mut client = Client::connect(server.addr).unwrap();
    for name in ["a", "b", "c"] {
        let (status, body, closes) = client.get(&format!("/hello/?name={name}"), false).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, format!("Hello, {name}! (GET /)\n"));
        assert!(!closes, "the server keeps the connection");
    }
    // A run that parks — three upstream calls, resumed on whichever worker —
    // answers on the same connection it was asked on.
    let (status, body, _) = client.get("/aggregate/?services=x,y", false).unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().count(), 2, "{body}");
    assert_eq!(stat(&server, "keep_alive_reuses"), 3);

    let (status, _, closes) = client.get("/hello/", true).unwrap();
    assert_eq!(status, 200);
    assert!(closes, "`Connection: close` is honoured");
    assert!(client.closed_within(Duration::from_secs(5)));
}

#[test]
fn the_last_request_a_connection_may_make_is_answered_with_close() {
    let server = start(KeepAlive {
        max_requests: 2,
        ..KeepAlive::default()
    });
    let mut client = Client::connect(server.addr).unwrap();
    assert!(!client.get("/hello/", false).unwrap().2);
    assert!(client.get("/hello/", false).unwrap().2, "the second of two");
    assert!(client.closed_within(Duration::from_secs(5)));
}

#[test]
fn an_idle_connection_is_closed_after_the_idle_timeout() {
    let server = start(KeepAlive {
        idle: Duration::from_millis(100),
        ..KeepAlive::default()
    });
    let mut client = Client::connect(server.addr).unwrap();
    assert!(!client.get("/hello/", false).unwrap().2);
    assert!(
        client.closed_within(Duration::from_secs(5)),
        "closed by the server, not by the bound"
    );
    assert!(stat(&server, "idle_expired") >= 1);
    assert_eq!(stat(&server, "idle_connections"), 0);
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let server = start(KeepAlive::default());
    let mut client = Client::connect(server.addr).unwrap();
    let both = "GET /hello/?name=one HTTP/1.1\r\nHost: x\r\n\r\n\
                GET /hello/?name=two HTTP/1.1\r\nHost: x\r\n\r\n";
    client.stream().write_all(both.as_bytes()).unwrap();
    assert_eq!(client.read_response().unwrap().1, "Hello, one! (GET /)\n");
    assert_eq!(client.read_response().unwrap().1, "Hello, two! (GET /)\n");
}

#[test]
fn http_1_0_and_a_server_without_keep_alive_close_after_one_request() {
    let server = start(KeepAlive::default());
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .write_all(b"GET /hello/ HTTP/1.0\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("Connection: close\r\n"), "{raw}");

    let server = start(KeepAlive {
        enabled: false,
        ..KeepAlive::default()
    });
    let mut client = Client::connect(server.addr).unwrap();
    let (status, _, closes) = client.get("/hello/", false).unwrap();
    assert_eq!(status, 200);
    assert!(closes, "asked to keep it, and refused");
    assert!(client.closed_within(Duration::from_secs(5)));
}
