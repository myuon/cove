//! The request timeline (`cove-edge --timeline`): a few mixed requests run
//! in-process with recording on, and the timeline, the Perfetto trace and
//! the HTML drawn from it are checked for shape. Nothing here asserts a
//! duration.

use std::collections::BTreeMap;
use std::time::Duration;

use cove_edge::http::get;
use cove_edge::json::{self, Json};
use cove_edge::picture::{self, Began};
use cove_edge::{DeployOptions, Isolates, KeepAlive, Latency, Recording, Server, ServerOptions};

fn start(timeline: Option<Recording>) -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers: 2,
        isolates: Isolates::PerRequest,
        keep_alive: KeepAlive::default(),
        fetchers: 2,
        timeline,
        deploy: DeployOptions {
            tenants: cove_edge::tenants_root(),
            latency: Latency {
                min: Duration::from_millis(2),
                max: Duration::from_millis(8),
            },
            quiet: true,
            blocking_upstream: false,
        },
    })
    .expect("the server starts")
}

#[test]
fn a_mixed_load_records_a_well_formed_timeline() {
    let server = start(Some(Recording::default()));
    let port = server.addr.port();
    let targets = [
        "/hello/?name=a".to_string(),
        "/hello/?name=b".to_string(),
        "/counter/home".to_string(),
        "/counter/away".to_string(),
        "/aggregate/?services=a,b,c".to_string(),
        "/aggregate/?services=d,e".to_string(),
        "/impatient/?services=weather,hang".to_string(),
        format!("/proxy/?url=http://127.0.0.1:{port}/hello/?name=proxied"),
    ];
    let clients: Vec<_> = targets
        .iter()
        .cloned()
        .map(|target| {
            let addr = server.addr;
            std::thread::spawn(move || (target.clone(), get(addr, &target).unwrap()))
        })
        .collect();
    let mut statuses = BTreeMap::new();
    for client in clients {
        let (target, (status, body)) = client.join().unwrap();
        let tenant = target.split('/').nth(1).unwrap().to_string();
        assert_eq!(
            status,
            if tenant == "impatient" { 504 } else { 200 },
            "{target}: {body}"
        );
        statuses.insert(target, status);
    }

    let dump = server.timeline().expect("recording is on");
    let trace = picture::read(&dump).expect("the dump reads back");
    // Eight requests, and the `hello` the proxy fetched from the same server.
    assert_eq!(trace.requests.len(), 9, "{dump}");
    assert_eq!(trace.workers, 2);
    let mut parks = 0;
    for req in &trace.requests {
        let first = req.segments.first().expect("every request has run_start");
        assert_eq!(first.began, Began::Run);
        assert!(req.status.is_some(), "#{} has no run_end", req.id);
        assert!(req.written.is_some(), "#{} was never written", req.id);
        assert!(req.segments.iter().all(|s| s.end.is_some()));
        assert!(req.segments.iter().all(|s| s.worker < 2));
        // One segment to begin with, and one more after every park.
        assert_eq!(req.segments.len(), req.parks.len() + 1, "#{}", req.id);
        for park in &req.parks {
            parks += 1;
            assert!(park.ready.is_some(), "#{} parked with no answer", req.id);
            assert!(
                park.resumed.is_some() && park.resume_worker.is_some(),
                "#{} parked and was neither resumed nor cancelled",
                req.id
            );
        }
        match req.tenant.as_str() {
            "aggregate" => {
                assert!(req
                    .parks
                    .iter()
                    .all(|p| p.op == "upstream.get" && p.by == "timer"));
                assert!(req.parks.iter().all(|p| p.due_ms.is_some()));
            }
            "impatient" => {
                assert_eq!(req.status, Some(504));
                let last = req.parks.last().expect("impatient parks");
                assert_eq!(last.target, "hang");
                assert_eq!(last.by, "deadline");
                assert!(last.cancelled);
                assert_eq!(req.segments.last().unwrap().began, Began::Cancel);
            }
            "proxy" => {
                assert_eq!(req.parks.len(), 1);
                assert_eq!(req.parks[0].op, "upstream.fetch");
                assert_eq!(req.parks[0].by, "fetcher");
            }
            "hello" | "counter" => assert!(req.parks.is_empty()),
            other => panic!("unexpected tenant {other}"),
        }
    }
    // a,b,c + d,e + weather,hang + the fetch.
    assert_eq!(parks, 3 + 2 + 2 + 1);
    let stats = picture::stats(&trace);
    assert_eq!(stats.timeouts, 1);
    assert_eq!(stats.parks, 8);
    assert_eq!(stats.resumes, 7, "the cancelled park is not a resume");

    // The Perfetto trace parses, and every park is one flow: a start on the
    // worker it parked on and a finish on the worker it resumed on.
    let chrome = json::parse(&picture::chrome_trace(&trace)).expect("the trace is JSON");
    let events = chrome
        .get("traceEvents")
        .and_then(Json::as_array)
        .expect("traceEvents");
    let mut flows: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
    for event in events {
        let phase = event.get("ph").and_then(Json::as_str).unwrap_or("");
        if phase == "s" || phase == "f" {
            let id = event.get("id").and_then(Json::as_f64).unwrap() as u64;
            let entry = flows.entry(id).or_default();
            if phase == "s" {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }
    assert_eq!(flows.len(), 8, "a flow per park, resumed or cancelled");
    assert!(flows.values().all(|&pair| pair == (1, 1)), "{flows:?}");
    let slices = events
        .iter()
        .filter(|e| e.get("ph").and_then(Json::as_str) == Some("X"))
        .count();
    assert_eq!(slices, 9 + 8, "a slice per run segment");

    // The page is one file with the figure and the legend in it.
    let html = picture::html(&trace);
    assert!(html.contains("<svg"), "no figure");
    for tenant in picture::TENANTS {
        assert!(
            html.contains(&format!("</i>{tenant}</span>")),
            "{tenant} missing from the legend"
        );
    }
    assert!(!html.contains("src=\"http"), "an external resource");
    let svg = picture::svg(&trace, true);
    assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
    assert!(svg.contains(">504<"), "the timeout is labelled");

    // Reset forgets it.
    assert_eq!(get(server.addr, "/_timeline?reset").unwrap().0, 200);
    let empty = picture::read(&server.timeline().unwrap()).unwrap();
    assert!(empty.requests.is_empty());
}

#[test]
fn without_recording_the_timeline_is_refused() {
    let server = start(None);
    assert!(server.timeline().is_none());
    let (status, body) = get(server.addr, "/_timeline").unwrap();
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("--timeline"), "{body}");
}
