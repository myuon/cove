//! The request timeline (`cove-edge --timeline`): a few mixed requests run
//! in-process with recording on, and the timeline, the Perfetto trace and
//! the HTML drawn from it are checked for shape. Nothing here asserts a
//! duration.

use std::collections::BTreeMap;
use std::time::Duration;

use cove_edge::http::get;
use cove_edge::json::{self, Json};
use cove_edge::picture::{self, Began};
use cove_edge::{
    DeployOptions, Discipline, Isolates, KeepAlive, Latency, Recording, Server, ServerOptions,
};

fn start(timeline: Option<Recording>) -> Server {
    start_with(
        timeline,
        Discipline::Stealing,
        Some(Duration::from_millis(2)),
    )
}

fn start_with(
    timeline: Option<Recording>,
    scheduler: Discipline,
    slice: Option<Duration>,
) -> Server {
    Server::start(ServerOptions {
        listen: "127.0.0.1:0".to_string(),
        workers: 2,
        isolates: Isolates::PerRequest,
        keep_alive: KeepAlive::default(),
        fetchers: 2,
        timeline,
        scheduler,
        slice,
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

/// `crunch` beside tenants that park: the answers are right, the timeline
/// is well formed, and the strips under the swimlanes are drawn from step
/// functions that start and end at zero and never exceed what they count.
#[test]
fn a_cpu_and_io_mix_draws_the_concurrency_strips() {
    let server = start(Some(Recording::default()));
    let port = server.addr.port();
    let targets = [
        "/crunch/?n=2000".to_string(),
        "/crunch/?n=2000".to_string(),
        "/crunch/?n=200000".to_string(),
        "/crunch/?n=lots".to_string(),
        "/aggregate/?services=a,b".to_string(),
        "/aggregate/?services=c,d".to_string(),
        "/hello/?name=a".to_string(),
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
    for client in clients {
        let (target, (status, body)) = client.join().unwrap();
        match target.as_str() {
            "/crunch/?n=2000" => {
                assert_eq!(
                    (status, body.as_str()),
                    (200, "303 primes up to 2000, the largest 1999\n")
                )
            }
            // The cap, within the tenant's fuel.
            "/crunch/?n=200000" => assert_eq!(
                (status, body.as_str()),
                (200, "17984 primes up to 200000, the largest 199999\n")
            ),
            "/crunch/?n=lots" => assert_eq!(status, 400, "{body}"),
            _ => assert_eq!(status, 200, "{target}: {body}"),
        }
    }

    let trace = picture::read(&server.timeline().unwrap()).expect("the dump reads back");
    assert_eq!(
        trace.requests.len(),
        targets.len() + 1,
        "and the proxied hello"
    );
    for req in &trace.requests {
        assert!(req.written.is_some(), "#{} was never written", req.id);
        assert_eq!(
            req.segments.len(),
            req.parks.len() + req.yields.len() + 1,
            "#{}",
            req.id
        );
        if req.tenant == "crunch" {
            assert!(req.parks.is_empty(), "crunch never parks");
            assert_eq!(req.host_calls, 0);
        }
    }

    let series = picture::series(&trace);
    for (name, steps, bound) in [
        ("running", &series.running, trace.workers),
        ("parked", &series.parked, usize::MAX),
        ("waiting on I/O", &series.waiting_on_io, usize::MAX),
        ("queue", &series.queue, usize::MAX),
    ] {
        assert!(!steps.is_empty(), "{name} has no steps");
        assert!(
            steps.windows(2).all(|w| w[0].0 < w[1].0),
            "{name} is not in time order"
        );
        assert!(
            steps.iter().all(|&(_, v)| v <= bound),
            "{name} exceeds {bound}"
        );
        assert_eq!(steps.last().unwrap().1, 0, "{name} does not end at zero");
    }
    assert!(series.running.iter().any(|&(_, v)| v >= 1));
    // Four parks at least (two services each, twice), all waiting on I/O.
    assert!(series.waiting_on_io.iter().any(|&(_, v)| v >= 1));

    let stats = picture::stats(&trace);
    assert_eq!(
        stats.running.len(),
        trace.workers + 1,
        "a share per level, 0 to workers"
    );
    let total: f64 = stats.running.iter().sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "the shares cover the span: {total}"
    );
    assert!(stats.utilisation > 0.0 && stats.utilisation <= 1.0);
    let busy_share: f64 = stats
        .running
        .iter()
        .enumerate()
        .map(|(k, share)| k as f64 * share)
        .sum::<f64>()
        / trace.workers as f64;
    assert!(
        (busy_share - stats.utilisation).abs() < 1e-6,
        "time at each level weighs up to the utilisation: {busy_share} against {}",
        stats.utilisation
    );
    assert_eq!(stats.tenants["crunch"].requests, 4);
    assert!(stats.tenants["crunch"].cpu_ms > 0.0);

    // The page draws the strips and carries their data for the hover; the
    // Perfetto trace has a counter track for each.
    let html = picture::html(&trace);
    assert!(
        html.contains("Workers running at once, 0 to 2"),
        "no running strip"
    );
    assert!(
        html.contains("Parked runs waiting on I/O"),
        "no waiting strip"
    );
    assert!(html.contains("the run queue"), "no queue strip");
    assert!(html.contains("const STEPS = [[["), "no strip data");
    assert!(html.contains("of the span with 2 workers running"));
    assert!(
        html.contains("</i>crunch</span>"),
        "crunch missing from the legend"
    );
    let chrome = json::parse(&picture::chrome_trace(&trace)).expect("the trace is JSON");
    let counters: std::collections::BTreeSet<String> = chrome
        .get("traceEvents")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .filter(|e| e.get("ph").and_then(Json::as_str) == Some("C"))
        .filter_map(|e| e.get("name").and_then(Json::as_str).map(str::to_string))
        .collect();
    for name in [
        "parked runs",
        "waiting on I/O",
        "workers running",
        "waiting for a worker",
    ] {
        assert!(counters.contains(name), "no `{name}` counter: {counters:?}");
    }
}

#[test]
fn without_recording_the_timeline_is_refused() {
    let server = start(None);
    assert!(server.timeline().is_none());
    let (status, body) = get(server.addr, "/_timeline").unwrap();
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("--timeline"), "{body}");
}

/// Four long `crunch`es on two workers, under a 1 ms slice: every one of them
/// is asked to yield while the others wait, each yield is continued — on
/// either worker — and every answer is the one an unsliced run gives. The
/// timeline records the yields, and the picture draws a mark for each.
#[test]
fn long_runs_are_sliced_at_safepoints_and_answer_the_same() {
    for (scheduler, slice) in [
        (Discipline::Stealing, Some(Duration::from_millis(1))),
        (Discipline::Fifo, Some(Duration::from_millis(1))),
        (Discipline::Stealing, None),
    ] {
        let server = start_with(Some(Recording::default()), scheduler, slice);
        let clients: Vec<_> = (0..4)
            .map(|_| {
                let addr = server.addr;
                std::thread::spawn(move || get(addr, "/crunch/?n=150000").unwrap())
            })
            .collect();
        for client in clients {
            let (status, body) = client.join().unwrap();
            assert_eq!(status, 200, "{body}");
            assert!(body.starts_with("13848 primes up to 150000"), "{body}");
        }
        let trace = picture::read(&server.timeline().unwrap()).expect("the dump reads back");
        let yields: usize = trace.requests.iter().map(|r| r.yields.len()).sum();
        for req in &trace.requests {
            assert_eq!(req.segments.len(), req.yields.len() + 1, "#{}", req.id);
            assert!(
                req.yields.iter().all(|y| y.continued.is_some()),
                "#{} was left yielded",
                req.id
            );
        }
        let stats: Json = json::parse(&server.stats()).unwrap();
        let count = |key: &str| stats.get(key).and_then(Json::as_f64).unwrap() as usize;
        assert_eq!(count("yields"), yields, "{scheduler:?} {slice:?}");
        match slice {
            Some(_) => {
                assert!(yields >= 4, "{scheduler:?}: only {yields} yields");
                let svg = picture::svg(&trace, true);
                assert!(svg.contains("class=\"yieldmark\""), "no yield drawn");
                assert!(picture::stats(&trace).yields == yields);
            }
            None => assert_eq!(yields, 0, "nothing asks without a slice"),
        }
    }
}
