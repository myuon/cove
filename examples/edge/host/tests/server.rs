//! The server, started in-process on a free port and asked over TCP.
//!
//! Nothing here asserts a duration. The one claim about concurrency — that
//! more runs are parked at once than there are workers — is observed by
//! polling `/_stats` while the runs are held at a slow upstream, and the
//! upstream's latency is chosen so that every run is certainly parked long
//! before the first one could answer.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use cove_edge::http::get;
use cove_edge::{DeployOptions, Isolates, Latency, Server, ServerOptions, State};

fn start(latency_ms: u64, workers: usize, isolates: Isolates) -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers,
        isolates,
        deploy: DeployOptions {
            tenants: cove_edge::tenants_root(),
            latency: Latency {
                min: Duration::from_millis(latency_ms),
                max: Duration::from_millis(latency_ms),
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
fn each_tenant_answers_and_the_over_reaching_one_is_refused() {
    let server = start(1, 2, Isolates::PerRequest);

    let tenant = |name: &str| {
        server
            .tenants()
            .iter()
            .find(|t| t.name == name)
            .expect("the tenant is listed")
    };
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| n.to_string()).collect() };
    assert_eq!(tenant("hello").required, set(&["edge"]));
    assert_eq!(tenant("counter").required, set(&["edge", "kv", "log"]));
    assert_eq!(tenant("aggregate").required, set(&["edge", "upstream"]));
    assert_eq!(tenant("greedy").required, set(&["edge", "kv", "upstream"]));
    for name in ["hello", "counter", "aggregate"] {
        assert!(
            matches!(tenant(name).state, State::Deployed(_)),
            "{name}: {}",
            tenant(name).describe()
        );
    }
    let State::Refused(why) = &tenant("greedy").state else {
        panic!("greedy is deployed: {}", tenant("greedy").describe());
    };
    assert!(why.contains("requires `upstream`"), "{why}");

    assert_eq!(
        get(server.addr, "/hello/?name=Test").unwrap(),
        (200, "Hello, Test! (GET /)\n".to_string())
    );
    assert_eq!(
        get(server.addr, "/counter/home").unwrap(),
        (200, "/home has been visited 1 time(s)\n".to_string())
    );
    assert_eq!(
        get(server.addr, "/counter/home").unwrap(),
        (200, "/home has been visited 2 time(s)\n".to_string())
    );

    let parks_before = stat(&server, "parks");
    let (status, body) = get(server.addr, "/aggregate/?services=weather,fail-db,news").unwrap();
    assert_eq!(status, 200, "{body}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "{body}");
    assert!(lines[0].starts_with("weather: sunny, 21C"), "{body}");
    assert_eq!(lines[1], "fail-db: unavailable (`fail-db` is down)");
    assert!(lines[2].starts_with("news: "), "{body}");
    // Three upstream calls, and each one parked the run rather than blocking
    // a worker: a blocking host never hands a run back.
    assert_eq!(stat(&server, "parks") - parks_before, 3);

    let (status, body) = get(server.addr, "/hello/spin").unwrap();
    assert_eq!(status, 500);
    assert!(body.contains("fuel budget of 2000000 exhausted"), "{body}");
    assert!(body.contains("hello/hello.cove:23"), "{body}");

    let (status, body) = get(server.addr, "/greedy/").unwrap();
    assert_eq!(status, 503);
    assert!(body.contains("requires `upstream`"), "{body}");
    assert_eq!(get(server.addr, "/nobody/").unwrap().0, 404);
}

#[test]
fn more_runs_wait_on_the_upstream_than_there_are_workers() {
    // Two workers and twenty requests whose every upstream call takes 400 ms.
    // Each run parks at its first call within a millisecond of starting, so
    // all twenty are parked at once long before any answer is due.
    let server = start(400, 2, Isolates::Pooled(4));
    let clients: Vec<_> = (0..20)
        .map(|_| {
            let addr = server.addr;
            std::thread::spawn(move || get(addr, "/aggregate/?services=a,b").unwrap())
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut most = 0;
    while most < 20 && Instant::now() < deadline {
        most = most.max(stat(&server, "parked"));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(most, 20, "parked at once, on two workers");
    for client in clients {
        let (status, body) = client.join().unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body.lines().count(), 2, "{body}");
    }
    assert_eq!(stat(&server, "parks"), 40);
    assert_eq!(stat(&server, "errors"), 0);
    // Resident isolates are reused, so no more than the pool's cap are kept.
    assert!(stat(&server, "pooled_isolates") <= 4);
}
