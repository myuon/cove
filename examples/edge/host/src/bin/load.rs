//! `cove-edge-load`: holds N requests in flight against a running server and
//! reports what came back.
//!
//! ```text
//! cargo run --release -p cove-edge --bin cove-edge-load -- \
//!     [--addr 127.0.0.1:8787] [--path /aggregate/] \
//!     [--concurrency 1000] [--requests 10000] [--threads 8] [--keep-alive] \
//!     [--mix hello=50,counter=20,aggregate=20,proxy=5,impatient=5 | --mix default | --mix cpu-io] \
//!     [--timeline-out timeline.json] [--rate 500 [--from-intended]] \
//!     [--summary-out run.json]
//! ```
//!
//! `--mix` replaces `--path` with a weighted choice of tenants, made per
//! request from its index, so the same command asks the same sequence:
//! `hello` and `counter` are short pure and `kv` runs, `aggregate` parks
//! three times, `proxy` makes a real fetch of `hello` on the same server,
//! and half of `impatient`'s requests ask the `hang` service and are
//! answered 504 at its 300 ms deadline. `--mix cpu-io` adds `crunch`, which
//! counts primes on the worker for milliseconds to tens of milliseconds and
//! never parks, to the tenants that wait. `--timeline-out` resets the server's
//! request timeline (`cove-edge --timeline`) before the run and writes it to
//! the file after. `--rate` starts request number `i` no sooner than
//! `i / rate` seconds into the run, so arrivals spread over time instead of
//! all coming at once; a kept-alive connection waits for its next turn
//! rather than closing.
//!
//! **Under `--rate`, the latency is from when the request was sent unless
//! `--from-intended` is given.** A request whose turn comes while every one
//! of the `--concurrency` connections is busy waits for one, and measuring
//! from the send leaves that wait out — coordinated omission: the slower the
//! server, the fewer requests the generator sends at their time, and the
//! less of the slowness it records. `--from-intended` measures from request
//! `i`'s intended start, `i / rate` into the run, which is what an open-loop
//! arrival process at that rate would see. Either way the generator reports
//! how late it sent (`send lag`): if the lag is more than a poll interval,
//! the concurrency cap or the client itself was binding, and a sent-time
//! latency understates. The sent-time default is kept because the numbers in
//! `examples/edge/README.md` were measured with it. `--summary-out` writes
//! the run's figures as JSON, for `compare/sweep.py`.
//!
//! std only, and not a thread per request on this side either: each of
//! `--threads` threads owns its share of the connections, writes each request
//! and then polls its sockets without blocking. Without `--keep-alive` each
//! request has a connection of its own, opened when it starts and closed by
//! the server when it is answered; with it, each of the `--concurrency`
//! connections asks its next request as soon as the last one is answered,
//! and is opened again only if the server closes it. The server's own counters — peak parked runs, peak
//! in-flight requests, its RSS — are read from `/_stats` after the run, having
//! been reset before it.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_edge::http::{closes, get, parse_response, request_bytes, response_length};
use cove_edge::os;

const USAGE: &str = "\
usage: cove-edge-load [--addr 127.0.0.1:8787] [--path /aggregate/]
                      [--concurrency 1000] [--requests 10000] [--threads 8]
                      [--keep-alive (reuse each connection)]
                      [--mix NAME=WEIGHT,... | --mix default | --mix cpu-io (instead of --path)]
                      [--timeline-out FILE (dump the server's /_timeline here)]
                      [--rate N (start at most N requests per second)]
                      [--from-intended (under --rate: latency from i / rate, not from the send)]
                      [--summary-out FILE (the run's figures as JSON)]";

/// `--mix default`: every behaviour the server has, in proportions that keep
/// a 300-request picture legible.
const DEFAULT_MIX: &str = "hello=50,counter=20,aggregate=20,proxy=5,impatient=5";

/// `--mix cpu-io`: CPU-heavy `crunch` beside the tenants that park —
/// `aggregate`'s three simulated upstreams, `proxy`'s real fetch,
/// `impatient`'s deadlines — and `hello`, the short run that shows what
/// waiting behind a `crunch` costs.
const CPU_IO_MIX: &str = "crunch=35,aggregate=30,proxy=10,hello=20,impatient=5";

/// The presets `--mix` knows by name.
fn preset(name: &str) -> &str {
    match name {
        "default" => DEFAULT_MIX,
        "cpu-io" => CPU_IO_MIX,
        other => other,
    }
}

/// The sizes `crunch` is asked for under a mix, chosen per request: from
/// about 4.5 ms on the VM to about 65 ms, so that a few hundred requests keep
/// four workers busy for a second or two.
const CRUNCH_SIZES: [u32; 4] = [20_000, 50_000, 100_000, 150_000];

/// What each request asks for.
enum Paths {
    One(String),
    /// Tenants and their cumulative weights, and the server's address for
    /// `proxy` to fetch from.
    Mix {
        tenants: Vec<(String, u64)>,
        total: u64,
        addr: String,
    },
}

impl Paths {
    fn parse_mix(text: &str, addr: &str) -> Paths {
        let text = preset(text);
        let mut tenants = Vec::new();
        let mut total = 0;
        for part in text.split(',').filter(|p| !p.is_empty()) {
            let (name, weight) = part
                .split_once('=')
                .unwrap_or_else(|| fail(&format!("`{part}` is not NAME=WEIGHT")));
            total += number(weight) as u64;
            tenants.push((name.to_string(), total));
        }
        if total == 0 {
            fail("`--mix` needs a positive weight");
        }
        Paths::Mix {
            tenants,
            total,
            addr: addr.to_string(),
        }
    }

    /// The tenant and the target of request number `index`.
    fn choose(&self, index: usize) -> (&str, String) {
        match self {
            Paths::One(path) => ("", path.clone()),
            Paths::Mix {
                tenants,
                total,
                addr,
            } => {
                let roll = splitmix(index as u64) % total;
                let name = &tenants
                    .iter()
                    .find(|(_, upto)| roll < *upto)
                    .expect("the last weight is the total")
                    .0;
                let target = match name.as_str() {
                    "hello" => format!("/hello/?name=load{index}"),
                    "counter" => format!("/counter/page{}", index % 4),
                    "aggregate" => "/aggregate/".to_string(),
                    "proxy" => format!("/proxy/?url=http://{addr}/hello/?name=proxy{index}"),
                    // Half of them ask `hang`, which answers after an hour,
                    // and are cancelled at the tenant's 300 ms deadline.
                    "impatient" if splitmix(index as u64 ^ 0x5eed).is_multiple_of(2) => {
                        "/impatient/?services=weather,hang".to_string()
                    }
                    "impatient" => "/impatient/?services=weather,stocks".to_string(),
                    "crunch" => {
                        let size = splitmix(index as u64 ^ 0xc0de) % CRUNCH_SIZES.len() as u64;
                        format!("/crunch/?n={}", CRUNCH_SIZES[size as usize])
                    }
                    other => format!("/{other}/"),
                };
                (name, target)
            }
        }
    }
}

/// A well-mixed hash of a request's index, so that consecutive requests do
/// not ask the same tenant.
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// How long one request may take before it is counted as failed.
const GIVE_UP: Duration = Duration::from_secs(30);

/// How long a kept-alive connection may wait under `--rate` for its next
/// request before the generator drops it, measured from its last request's
/// send: well inside the 5 s idle timeout of the servers it is pointed at.
const IDLE_DROP: Duration = Duration::from_secs(2);

struct Outcome {
    latencies: Vec<Duration>,
    statuses: Vec<u16>,
    errors: Vec<String>,
    /// The first response that was not a 200, status and body.
    refused: Option<(u16, String)>,
    /// Connections opened.
    connections: usize,
    /// Per tenant under `--mix`: how many of each status came back.
    by_tenant: BTreeMap<String, BTreeMap<u16, usize>>,
    /// Per tenant under `--mix`: every latency, as the client saw it.
    tenant_latencies: BTreeMap<String, Vec<Duration>>,
    /// How late each request was sent after its intended start.
    lags: Vec<Duration>,
    /// Every latency from the send, whichever `latencies` holds, so that a
    /// summary has both: from the intended start is right once the
    /// generator falls behind, from the send is the server's alone while it
    /// does not.
    sent: Vec<Duration>,
    tenant_sent: BTreeMap<String, Vec<Duration>>,
}

impl Outcome {
    fn new(requests: usize) -> Outcome {
        Outcome {
            latencies: Vec::with_capacity(requests),
            statuses: Vec::with_capacity(requests),
            errors: Vec::new(),
            refused: None,
            connections: 0,
            by_tenant: BTreeMap::new(),
            tenant_latencies: BTreeMap::new(),
            lags: Vec::new(),
            sent: Vec::new(),
            tenant_sent: BTreeMap::new(),
        }
    }
}

struct Open {
    stream: TcpStream,
    raw: Vec<u8>,
    /// When the request being waited for was written.
    started: Instant,
    /// When it was meant to start: `i / rate` into the run under `--rate`,
    /// and `started` without it.
    intended: Instant,
    /// The tenant it asked, under `--mix`.
    tenant: String,
    /// Kept alive with no request on it: waiting for `--rate` to allow the
    /// next.
    idle: bool,
}

/// The requests still to make, shared by every client thread, and when each
/// may start under `--rate`.
struct Plan {
    requests: usize,
    remaining: AtomicUsize,
    start: Instant,
    rate: Option<f64>,
}

impl Plan {
    /// Claims the next request, and its index among all of them — if there
    /// is one left and it is due.
    fn claim(&self) -> Option<usize> {
        // A compare-and-swap loop rather than `fetch_update`, which Rust 1.99
        // deprecated for a `try_update` earlier toolchains do not have.
        let mut left = self.remaining.load(Ordering::Relaxed);
        loop {
            if left == 0 || !self.due(self.requests - left) {
                return None;
            }
            match self.remaining.compare_exchange_weak(
                left,
                left - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(self.requests - left),
                Err(seen) => left = seen,
            }
        }
    }

    /// When request `index` is meant to start: `index / rate` into the run,
    /// or `now` when there is no rate.
    fn intended(&self, index: usize, now: Instant) -> Instant {
        match self.rate {
            Some(rate) => self.start + Duration::from_secs_f64(index as f64 / rate),
            None => now,
        }
    }

    fn due(&self, index: usize) -> bool {
        self.rate
            .is_none_or(|rate| self.start.elapsed().as_secs_f64() >= index as f64 / rate)
    }

    /// Whether every request has been claimed.
    fn done(&self) -> bool {
        self.remaining.load(Ordering::Relaxed) == 0
    }
}

fn main() {
    let mut addr = "127.0.0.1:8787".to_string();
    let mut path = "/aggregate/".to_string();
    let mut concurrency = 1000usize;
    let mut requests = 10_000usize;
    let mut threads = 8usize;
    let mut keep_alive = false;
    let mut mix = None;
    let mut timeline_out: Option<String> = None;
    let mut rate = None;
    let mut from_intended = false;
    let mut summary_out: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail("a flag takes a value"));
        match arg.as_str() {
            "--addr" => addr = value(),
            "--path" => path = value(),
            "--concurrency" => concurrency = number(&value()),
            "--requests" => requests = number(&value()),
            "--threads" => threads = number(&value()),
            "--keep-alive" => keep_alive = true,
            "--mix" => mix = Some(value()),
            "--timeline-out" => timeline_out = Some(value()),
            "--rate" => rate = Some(number(&value()) as f64),
            "--from-intended" => from_intended = true,
            "--summary-out" => summary_out = Some(value()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument `{other}`")),
        }
    }
    let target: SocketAddr = addr
        .to_socket_addrs()
        .ok()
        .and_then(|mut found| found.next())
        .unwrap_or_else(|| fail(&format!("cannot resolve `{addr}`")));
    let files = os::raise_open_files();
    let concurrency = concurrency.min(requests).max(1);
    let threads = threads.min(concurrency).max(1);
    if files < concurrency as u64 + 64 {
        eprintln!("warning: the open-file limit is {files}, below {concurrency} connections");
    }

    get(target, "/_stats?reset").unwrap_or_else(|e| fail(&format!("is the server up? {e}")));
    if timeline_out.is_some() {
        match get(target, "/_timeline?reset") {
            Ok((200, _)) => {}
            Ok((_, body)) => fail(body.trim_end()),
            Err(e) => fail(&e),
        }
    }
    let paths = Arc::new(match &mix {
        Some(mix) => Paths::parse_mix(mix, &addr),
        None => Paths::One(path.clone()),
    });
    let what = match &mix {
        Some(mix) => format!("a mix of {}", preset(mix)),
        None => format!("http://{addr}{path}"),
    };
    println!(
        "{requests} requests to {what}, {concurrency} in flight, {threads} client thread(s), {}",
        if keep_alive {
            "connections kept alive"
        } else {
            "a connection per request"
        }
    );

    let outcome = Arc::new(Mutex::new(Outcome::new(requests)));
    let wall = Instant::now();
    let plan = Arc::new(Plan {
        requests,
        remaining: AtomicUsize::new(requests),
        start: wall,
        rate,
    });
    let handles: Vec<_> = (0..threads)
        .map(|at| {
            let share = concurrency / threads + usize::from(at < concurrency % threads);
            let (plan, outcome) = (Arc::clone(&plan), Arc::clone(&outcome));
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || {
                let next = |index| {
                    let (tenant, target) = paths.choose(index);
                    (
                        tenant.to_string(),
                        request_bytes("GET", &target, "", keep_alive),
                    )
                };
                drive(
                    target,
                    &next,
                    keep_alive,
                    from_intended,
                    share,
                    &plan,
                    &outcome,
                )
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a client thread");
    }
    let wall = wall.elapsed();

    let mut outcome = std::mem::replace(&mut *outcome.lock().unwrap(), Outcome::new(0));
    outcome.latencies.sort();
    let ok = outcome.statuses.iter().filter(|&&s| s == 200).count();
    let pct = |p: f64| {
        let all = &outcome.latencies;
        if all.is_empty() {
            return 0.0;
        }
        all[((all.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
    };
    println!(
        "  answered {} ({ok} with 200), {} failed to connect or read, in {:.2} s: {:.0} req/s",
        outcome.statuses.len(),
        outcome.errors.len(),
        wall.as_secs_f64(),
        outcome.statuses.len() as f64 / wall.as_secs_f64()
    );
    println!("  connections opened: {}", outcome.connections);
    println!(
        "  latency ms: p50 {:.1}  p90 {:.1}  p99 {:.1}  max {:.1}",
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0)
    );
    let mut lags = std::mem::take(&mut outcome.lags);
    lags.sort();
    let lag_at = |p: f64| {
        lags.get(((lags.len().max(1) - 1) as f64 * p).round() as usize)
            .map_or(0.0, |d| d.as_secs_f64() * 1e3)
    };
    // Late past a millisecond: more than a poll interval (200 us) and a
    // connect, so the cap or the client was what held it back.
    let late = lags
        .iter()
        .filter(|lag| **lag > Duration::from_millis(1))
        .count();
    if rate.is_some() {
        println!(
            "  send lag ms: p50 {:.2}  p99 {:.2}  max {:.2}; {late} of {} sent more than 1 ms late; latency measured from {}",
            lag_at(0.5),
            lag_at(0.99),
            lag_at(1.0),
            lags.len(),
            if from_intended {
                "the intended start"
            } else {
                "the send"
            }
        );
    }
    let mut sent = std::mem::take(&mut outcome.sent);
    sent.sort();
    let sent_at = |all: &[Duration], p: f64| {
        all.get(((all.len().max(1) - 1) as f64 * p).round() as usize)
            .map_or(0.0, |d| d.as_secs_f64() * 1e3)
    };
    let mut summary = format!(
        "{{\"requests\": {requests}, \"concurrency\": {concurrency}, \"rate\": {}, \"from_intended\": {from_intended}, \"keep_alive\": {keep_alive}, \"answered\": {}, \"ok\": {ok}, \"errors\": {}, \"wall_s\": {:.4}, \"throughput\": {:.2}, \"connections\": {}, \"p50_ms\": {:.4}, \"p90_ms\": {:.4}, \"p99_ms\": {:.4}, \"max_ms\": {:.4}, \"lag_p50_ms\": {:.4}, \"lag_p99_ms\": {:.4}, \"lag_max_ms\": {:.4}, \"late\": {late}, \"p50_sent_ms\": {:.4}, \"p99_sent_ms\": {:.4}, \"tenants\": {{",
        rate.map_or("null".to_string(), |r| r.to_string()),
        outcome.statuses.len(),
        outcome.errors.len(),
        wall.as_secs_f64(),
        outcome.statuses.len() as f64 / wall.as_secs_f64(),
        outcome.connections,
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0),
        lag_at(0.5),
        lag_at(0.99),
        lag_at(1.0),
        sent_at(&sent, 0.5),
        sent_at(&sent, 0.99),
    );
    for (n, (tenant, statuses)) in outcome.by_tenant.iter().enumerate() {
        let mut latencies = outcome
            .tenant_latencies
            .get(tenant)
            .cloned()
            .unwrap_or_default();
        latencies.sort();
        let at = |p: f64| {
            latencies
                .get(((latencies.len().max(1) - 1) as f64 * p).round() as usize)
                .map_or(0.0, |d| d.as_secs_f64() * 1e3)
        };
        let statuses: Vec<String> = statuses
            .iter()
            .map(|(status, count)| format!("\"{status}\": {count}"))
            .collect();
        let mut from_send = outcome.tenant_sent.get(tenant).cloned().unwrap_or_default();
        from_send.sort();
        summary.push_str(&format!(
            "{}\"{tenant}\": {{\"count\": {}, \"p50_ms\": {:.4}, \"p99_ms\": {:.4}, \"p50_sent_ms\": {:.4}, \"p99_sent_ms\": {:.4}, \"statuses\": {{{}}}}}",
            if n > 0 { ", " } else { "" },
            latencies.len(),
            at(0.5),
            at(0.99),
            sent_at(&from_send, 0.5),
            sent_at(&from_send, 0.99),
            statuses.join(", ")
        ));
    }
    summary.push_str("}}\n");
    for (tenant, statuses) in &outcome.by_tenant {
        let statuses: Vec<String> = statuses
            .iter()
            .map(|(status, count)| format!("{count} x {status}"))
            .collect();
        let mut latencies = outcome
            .tenant_latencies
            .get(tenant)
            .cloned()
            .unwrap_or_default();
        latencies.sort();
        let at = |p: f64| {
            latencies
                .get(((latencies.len().max(1) - 1) as f64 * p).round() as usize)
                .map_or(0.0, |d| d.as_secs_f64() * 1e3)
        };
        println!(
            "  {tenant:<10} {:<20} {:>7.1} req/s  p50 {:>6.1} ms  p99 {:>6.1} ms",
            statuses.join(", "),
            latencies.len() as f64 / wall.as_secs_f64(),
            at(0.5),
            at(0.99)
        );
    }
    if let Some(first) = outcome.errors.first() {
        println!("  first error: {first}");
    }
    if let Some((status, body)) = &outcome.refused {
        println!("  first non-200 ({status}): {}", body.trim_end());
    }
    match get(target, "/_stats") {
        Ok((_, body)) => println!("server /_stats after the run:\n{body}"),
        Err(e) => println!("server /_stats unreadable: {e}"),
    }
    if let Some(file) = summary_out {
        std::fs::write(&file, summary)
            .unwrap_or_else(|e| fail(&format!("cannot write {file}: {e}")));
    }
    if let Some(file) = timeline_out {
        match get(target, "/_timeline") {
            Ok((200, body)) => match std::fs::write(&file, &body) {
                Ok(()) => println!(
                    "timeline: {} events written to {file}",
                    body.lines().filter(|l| l.starts_with("{\"t\"")).count()
                ),
                Err(e) => fail(&format!("cannot write {file}: {e}")),
            },
            Ok((status, body)) => fail(&format!("/_timeline answered {status}: {body}")),
            Err(e) => fail(&format!("/_timeline unreadable: {e}")),
        }
    }
}

/// Keeps `share` requests in flight until the plan runs out.
fn drive(
    target: SocketAddr,
    next: &dyn Fn(usize) -> (String, String),
    keep_alive: bool,
    from_intended: bool,
    share: usize,
    plan: &Plan,
    outcome: &Mutex<Outcome>,
) {
    let mut open: Vec<Open> = Vec::with_capacity(share);
    let mut latencies = Vec::new();
    let mut statuses = Vec::new();
    let mut errors = Vec::new();
    let mut refused = None;
    let mut connections = 0;
    let mut by_tenant: BTreeMap<String, BTreeMap<u16, usize>> = BTreeMap::new();
    let mut tenant_latencies: BTreeMap<String, Vec<Duration>> = BTreeMap::new();
    let mut lags = Vec::new();
    let mut sent = Vec::new();
    let mut tenant_sent: BTreeMap<String, Vec<Duration>> = BTreeMap::new();
    let mut chunk = [0u8; 4096];
    loop {
        while open.len() < share {
            let Some(index) = plan.claim() else {
                break;
            };
            let (tenant, request) = next(index);
            let started = Instant::now();
            let intended = plan.intended(index, started);
            lags.push(started.saturating_duration_since(intended));
            connections += 1;
            let opened = TcpStream::connect_timeout(&target, Duration::from_secs(10)).and_then(
                |mut stream| {
                    stream.write_all(request.as_bytes())?;
                    stream.set_nonblocking(true)?;
                    Ok(stream)
                },
            );
            match opened {
                Ok(stream) => open.push(Open {
                    stream,
                    raw: Vec::with_capacity(512),
                    started,
                    intended,
                    tenant,
                    idle: false,
                }),
                Err(e) => errors.push(format!("connect: {e}")),
            }
        }
        if open.is_empty() && plan.done() {
            break;
        }
        let mut progressed = false;
        let mut at = 0;
        while at < open.len() {
            if open[at].idle {
                if let Some(index) = plan.claim() {
                    let (tenant, request) = next(index);
                    let now = Instant::now();
                    open[at].started = now;
                    open[at].intended = plan.intended(index, now);
                    lags.push(now.saturating_duration_since(open[at].intended));
                    open[at].tenant = tenant;
                    open[at].idle = false;
                    progressed = true;
                    if let Err(e) = write_all_nonblocking(&mut open[at].stream, request.as_bytes())
                    {
                        errors.push(format!("write: {e}"));
                        open.swap_remove(at);
                        continue;
                    }
                } else if plan.done() || open[at].started.elapsed() > IDLE_DROP {
                    // Done, or idle long enough that the server may be about
                    // to close it (both servers compared close an idle
                    // connection after 5 s): a request written into that
                    // close would come back as a failure that is the
                    // generator's. The loop above opens a fresh one when a
                    // request is due.
                    reset(&open.swap_remove(at).stream);
                    continue;
                }
                at += 1;
                continue;
            }
            // What reading this connection came to: nothing yet, a whole
            // response and whether the connection is still open, or an
            // error.
            let done = loop {
                if keep_alive {
                    if let Some(length) = response_length(&open[at].raw) {
                        let raw: Vec<u8> = open[at].raw.drain(..length).collect();
                        break Some(Ok((raw, true)));
                    }
                }
                match open[at].stream.read(&mut chunk) {
                    Ok(0) => break Some(Ok((std::mem::take(&mut open[at].raw), false))),
                    Ok(n) => {
                        open[at].raw.extend_from_slice(&chunk[..n]);
                        progressed = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break None,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => break Some(Err(format!("read: {e}"))),
                }
            };
            let done = match done {
                // A connection that has not answered in this long is given
                // up on and counted, so that a lost connection cannot hang
                // the run. Without keep-alive on macOS it is a real hazard:
                // a connect that lands on a tuple still in the server's
                // TIME_WAIT from the last run can stall.
                None if open[at].started.elapsed() > GIVE_UP => {
                    Some(Err(format!("no answer within {} s", GIVE_UP.as_secs())))
                }
                done => done,
            };
            let Some(result) = done else {
                at += 1;
                continue;
            };
            progressed = true;
            let mut reusable = false;
            match result.and_then(|(raw, still_open)| {
                reusable = still_open && !closes(&raw);
                parse_response(&raw)
            }) {
                Ok((status, body)) => {
                    let from = if from_intended {
                        open[at].intended
                    } else {
                        open[at].started
                    };
                    let took = from.elapsed();
                    latencies.push(took);
                    let since_sent = open[at].started.elapsed();
                    sent.push(since_sent);
                    statuses.push(status);
                    if !open[at].tenant.is_empty() {
                        *by_tenant
                            .entry(open[at].tenant.clone())
                            .or_default()
                            .entry(status)
                            .or_default() += 1;
                        tenant_latencies
                            .entry(open[at].tenant.clone())
                            .or_default()
                            .push(took);
                        tenant_sent
                            .entry(open[at].tenant.clone())
                            .or_default()
                            .push(since_sent);
                    }
                    if status != 200 && refused.is_none() {
                        refused = Some((status, body));
                    }
                }
                Err(e) => {
                    reusable = false;
                    errors.push(e);
                }
            }
            // The next request on the same connection, if it may carry one
            // and there is one to make; otherwise the connection is done,
            // and the loop above opens a fresh one for the next request.
            let claimed = if reusable { plan.claim() } else { None };
            if reusable && claimed.is_none() && !plan.done() {
                // Not this one's turn yet under `--rate`: keep it open.
                open[at].idle = true;
                at += 1;
                continue;
            }
            if let Some(index) = claimed {
                let (tenant, request) = next(index);
                let now = Instant::now();
                open[at].started = now;
                open[at].intended = plan.intended(index, now);
                lags.push(now.saturating_duration_since(open[at].intended));
                open[at].tenant = tenant;
                if let Err(e) = write_all_nonblocking(&mut open[at].stream, request.as_bytes()) {
                    errors.push(format!("write: {e}"));
                    open.swap_remove(at);
                }
                continue;
            }
            let finished = open.swap_remove(at);
            if reusable {
                reset(&finished.stream);
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    let mut outcome = outcome.lock().unwrap();
    outcome.latencies.extend(latencies);
    outcome.statuses.extend(statuses);
    outcome.errors.extend(errors);
    outcome.connections += connections;
    outcome.lags.extend(lags);
    outcome.sent.extend(sent);
    for (tenant, latencies) in tenant_sent {
        outcome
            .tenant_sent
            .entry(tenant)
            .or_default()
            .extend(latencies);
    }
    for (tenant, statuses) in by_tenant {
        let into = outcome.by_tenant.entry(tenant).or_default();
        for (status, count) in statuses {
            *into.entry(status).or_default() += count;
        }
    }
    for (tenant, latencies) in tenant_latencies {
        outcome
            .tenant_latencies
            .entry(tenant)
            .or_default()
            .extend(latencies);
    }
    if outcome.refused.is_none() {
        outcome.refused = refused;
    }
}

/// `write_all` on a socket in nonblocking mode: a request is a few dozen
/// bytes, so the send buffer is all but never full, but it is not an error
/// when it is.
fn write_all_nonblocking(stream: &mut TcpStream, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_micros(50))
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Makes dropping a kept-alive connection the generator has no more use for
/// a reset rather than a close, so that it leaves no `TIME_WAIT` behind.
///
/// The side that closes first holds the connection's `TIME_WAIT`, and
/// without keep-alive that is the server, which answers and closes. With it
/// the client closes, and its `TIME_WAIT`s hold their ephemeral ports for
/// twice the MSL — 30 s on macOS, whose range is 16,384 ports — so a second
/// 10,000-connection run straight after the first failed 13,718 of its
/// 20,000 connects with `EADDRNOTAVAIL`. A load generator abandoning
/// connections it opened is what a reset is for.
#[cfg(unix)]
fn reset(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // Safety: `setsockopt` reads `linger`, which outlives the call, for the
    // length it is told, on a descriptor `stream` owns.
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn reset(_stream: &TcpStream) {}

fn number(text: &str) -> usize {
    text.parse()
        .unwrap_or_else(|_| fail(&format!("`{text}` is not a number")))
}

fn fail(message: &str) -> ! {
    eprintln!("cove-edge-load: {message}\n{USAGE}");
    std::process::exit(2)
}
