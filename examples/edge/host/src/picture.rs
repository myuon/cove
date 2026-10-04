//! Pictures of a request timeline ([`crate::timeline`]): a Chrome / Perfetto
//! trace, and a self-contained HTML page with one SVG.
//!
//! The dump is read back into one [`Req`] per request — its run segments,
//! each on the worker that ran it, and its parks, each with when its answer
//! was ready and where it resumed — and both pictures are drawn from that.
//!
//! The HTML follows the dataviz method this repository's tooling uses: two
//! views sharing one time axis (worker swimlanes above, a lane per request
//! below), colour for tenant identity only and in a fixed order, a timeout
//! as status (red, with a glyph and a label, never colour alone), a legend
//! always, recessive grid, a hover readout, and a table view. Between the
//! two views, three small charts of their own (never a dual axis) count
//! across tenants in neutral ink ([`Series`]): workers running, parked runs
//! waiting on I/O, and requests waiting for a free worker.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::json::{self, Json};
use crate::timeline::quote;

/// The tenants the palette knows, in its fixed order.
pub const TENANTS: [&str; 6] = [
    "hello",
    "counter",
    "aggregate",
    "proxy",
    "impatient",
    "crunch",
];
const LIGHT: [&str; 6] = [
    "#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300",
];
const DARK: [&str; 6] = [
    "#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300",
];

/// How a run segment began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Began {
    Run,
    Resume,
    Cancel,
}

/// One stretch of a request's run on one worker.
#[derive(Clone, Debug)]
pub struct Seg {
    pub worker: usize,
    pub start: f64,
    /// `None` while still running when the timeline was dumped.
    pub end: Option<f64>,
    pub began: Began,
}

/// One park: where the run parked, when its answer was ready, where it
/// resumed.
#[derive(Clone, Debug)]
pub struct Park {
    pub worker: usize,
    pub at: f64,
    pub op: String,
    pub target: String,
    pub ready: Option<f64>,
    /// `timer`, `fetcher`, `deadline` or `host`.
    pub by: String,
    pub resumed: Option<f64>,
    pub resume_worker: Option<usize>,
    pub cancelled: bool,
    /// A simulated call's chosen latency, in milliseconds: when its answer
    /// was due.
    pub due_ms: Option<f64>,
}

/// One request. Times are microseconds since the server started.
#[derive(Clone, Debug)]
pub struct Req {
    pub id: u64,
    pub tenant: String,
    pub path: String,
    pub accepted: Option<f64>,
    pub queued: f64,
    pub segments: Vec<Seg>,
    pub parks: Vec<Park>,
    pub status: Option<u16>,
    pub ended: Option<f64>,
    pub written: Option<f64>,
    pub fuel: u64,
    pub host_calls: u64,
    pub heap_bytes: u64,
}

impl Req {
    /// When it was last seen: written, else ended, else its last event.
    pub fn last(&self) -> f64 {
        self.written
            .or(self.ended)
            .or_else(|| self.segments.last().and_then(|s| s.end.or(Some(s.start))))
            .unwrap_or(self.queued)
    }

    /// From queued to written, in microseconds, once it has been answered.
    pub fn latency(&self) -> Option<f64> {
        self.written.or(self.ended).map(|end| end - self.queued)
    }
}

/// A whole timeline.
#[derive(Clone, Debug)]
pub struct Trace {
    pub workers: usize,
    /// Requests in arrival (queued) order.
    pub requests: Vec<Req>,
    /// The earliest and latest time any request was seen.
    pub t0: f64,
    pub t1: f64,
}

fn phase(ev: &str) -> u8 {
    match ev {
        "accepted" => 0,
        "queued" => 1,
        "run_start" => 2,
        "park" => 3,
        "answer_ready" => 4,
        "resume" | "cancel" => 5,
        "run_end" => 6,
        _ => 7,
    }
}

/// Reads a dump back into requests.
pub fn read(text: &str) -> Result<Trace, String> {
    let doc = json::parse(text)?;
    if doc.get("format").and_then(Json::as_str) != Some("cove-edge-timeline") {
        return Err("not a cove-edge timeline (no \"format\": \"cove-edge-timeline\")".into());
    }
    let workers = doc.get("workers").and_then(Json::as_f64).unwrap_or(0.0) as usize;
    let events = doc
        .get("events")
        .and_then(Json::as_array)
        .ok_or("the timeline has no events array")?;
    let mut by_request: BTreeMap<u64, Vec<&Json>> = BTreeMap::new();
    for event in events {
        let id = event
            .get("req")
            .and_then(Json::as_f64)
            .ok_or("an event has no request id")? as u64;
        by_request.entry(id).or_default().push(event);
    }
    let num = |e: &Json, k: &str| e.get(k).and_then(Json::as_f64).unwrap_or(0.0);
    let text = |e: &Json, k: &str| e.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let mut requests = Vec::new();
    for (id, mut events) in by_request {
        events.sort_by(|a, b| {
            num(a, "t")
                .total_cmp(&num(b, "t"))
                .then(phase(&text(a, "ev")).cmp(&phase(&text(b, "ev"))))
        });
        let mut req = Req {
            id,
            tenant: String::new(),
            path: String::new(),
            accepted: None,
            queued: num(events[0], "t"),
            segments: Vec::new(),
            parks: Vec::new(),
            status: None,
            ended: None,
            written: None,
            fuel: 0,
            host_calls: 0,
            heap_bytes: 0,
        };
        let mut started = false;
        for e in events {
            let t = num(e, "t");
            let worker = num(e, "worker") as usize;
            let close = |req: &mut Req| {
                if let Some(seg) = req.segments.last_mut() {
                    if seg.end.is_none() {
                        seg.end = Some(t);
                    }
                }
            };
            match text(e, "ev").as_str() {
                "accepted" => req.accepted = Some(t),
                "queued" => req.queued = t,
                "run_start" => {
                    started = true;
                    req.tenant = text(e, "tenant");
                    req.path = text(e, "path");
                    req.segments.push(Seg {
                        worker,
                        start: t,
                        end: None,
                        began: Began::Run,
                    });
                }
                "park" => {
                    close(&mut req);
                    req.parks.push(Park {
                        worker,
                        at: t,
                        op: text(e, "op"),
                        target: text(e, "target"),
                        ready: None,
                        by: String::new(),
                        resumed: None,
                        resume_worker: None,
                        cancelled: false,
                        due_ms: e.get("due_ms").and_then(Json::as_f64),
                    });
                }
                "answer_ready" => {
                    if let Some(park) = req.parks.last_mut() {
                        park.ready = Some(t);
                        park.by = text(e, "by");
                    }
                }
                ev @ ("resume" | "cancel") => {
                    let cancelled = ev == "cancel";
                    if let Some(park) = req.parks.last_mut() {
                        park.resumed = Some(t);
                        park.resume_worker = Some(worker);
                        park.cancelled = cancelled;
                    }
                    req.segments.push(Seg {
                        worker,
                        start: t,
                        end: None,
                        began: if cancelled {
                            Began::Cancel
                        } else {
                            Began::Resume
                        },
                    });
                }
                "run_end" => {
                    close(&mut req);
                    req.ended = Some(t);
                    req.status = Some(num(e, "status") as u16);
                    req.fuel = num(e, "fuel") as u64;
                    req.host_calls = num(e, "host_calls") as u64;
                    req.heap_bytes = num(e, "heap_bytes") as u64;
                }
                "response_written" => req.written = Some(t),
                _ => {}
            }
        }
        // A request whose run started before the timeline was last reset
        // has lost its beginning; it is left out rather than half drawn.
        if started {
            requests.push(req);
        }
    }
    requests.sort_by(|a, b| a.queued.total_cmp(&b.queued).then(a.id.cmp(&b.id)));
    let t0 = requests
        .iter()
        .map(|r| r.accepted.unwrap_or(r.queued).min(r.queued))
        .fold(f64::INFINITY, f64::min);
    let t1 = requests
        .iter()
        .map(Req::last)
        .fold(f64::NEG_INFINITY, f64::max);
    let (t0, t1) = if requests.is_empty() {
        (0.0, 1.0)
    } else {
        (t0, t1.max(t0 + 1.0))
    };
    Ok(Trace {
        workers,
        requests,
        t0,
        t1,
    })
}

// ----------------------------------------------------------------- series

/// A step function over time: `(t, value)`, the value from `t` until the
/// next step, starting at zero before the first.
pub type Steps = Vec<(f64, usize)>;

/// What the strips under the worker swimlanes draw.
#[derive(Clone, Debug, Default)]
pub struct Series {
    /// Workers running a tenant's isolate.
    pub running: Steps,
    /// Runs parked, from the park to the resume (or the cancel) — the
    /// `ParkedVm`s that exist, the same count as [`Stats::max_parked`].
    pub parked: Steps,
    /// Runs parked whose answer is not in yet, from the park to the answer
    /// (or the deadline): the waits on I/O, without the run queue.
    pub waiting_on_io: Steps,
    /// Requests ready to run with no worker free: queued and not yet
    /// started, or answered and not yet resumed — the run queue.
    pub queue: Steps,
}

/// Folds `+1`/`-1` edges into steps. At equal times a `-1` goes first, so a
/// run that ends as another starts does not count twice.
fn steps(mut edges: Vec<(f64, i32)>) -> Steps {
    edges.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out: Steps = Vec::with_capacity(edges.len());
    let mut level = 0i64;
    for (t, delta) in edges {
        level = (level + delta as i64).max(0);
        match out.last_mut() {
            Some(last) if last.0 == t => last.1 = level as usize,
            _ => out.push((t, level as usize)),
        }
    }
    out
}

/// The three step functions of a trace.
pub fn series(trace: &Trace) -> Series {
    let (mut running, mut parked, mut queue) = (Vec::new(), Vec::new(), Vec::new());
    let mut io = Vec::new();
    for req in &trace.requests {
        for seg in &req.segments {
            running.push((seg.start, 1));
            running.push((seg.end.unwrap_or(trace.t1), -1));
        }
        if let Some(first) = req.segments.first() {
            queue.push((req.queued, 1));
            queue.push((first.start, -1));
        }
        for park in &req.parks {
            parked.push((park.at, 1));
            parked.push((park.resumed.unwrap_or(trace.t1), -1));
            io.push((park.at, 1));
            io.push((park.ready.unwrap_or(trace.t1), -1));
            if let Some(ready) = park.ready {
                queue.push((ready, 1));
                queue.push((park.resumed.unwrap_or(trace.t1).max(ready), -1));
            }
        }
    }
    Series {
        running: steps(running),
        parked: steps(parked),
        waiting_on_io: steps(io),
        queue: steps(queue),
    }
}

/// For each level, the time a step function spent at it within `[t0, t1]`.
fn time_at_level(steps: &Steps, t0: f64, t1: f64, levels: usize) -> Vec<f64> {
    let mut out = vec![0.0; levels];
    let mut level = 0usize;
    let mut since = t0;
    for &(t, next) in steps {
        let t = t.clamp(t0, t1);
        if let Some(slot) = out.get_mut(level.min(levels - 1)) {
            *slot += t - since;
        }
        since = t;
        level = next;
    }
    if let Some(slot) = out.get_mut(level.min(levels - 1)) {
        *slot += t1 - since;
    }
    out
}

// ------------------------------------------------------------------ stats

/// One tenant's requests, server side: queued to written.
#[derive(Clone, Debug, Default)]
pub struct TenantStats {
    pub requests: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    /// Worker time its runs took, in milliseconds.
    pub cpu_ms: f64,
}

/// The figures the summary row shows, and the README's interpretation reads.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub requests: usize,
    pub workers: usize,
    pub span_ms: f64,
    pub parks: usize,
    /// Resumed with an answer (not cancelled).
    pub resumes: usize,
    /// …of which on a worker other than the one it parked on.
    pub resumed_elsewhere: usize,
    pub max_parked: usize,
    pub timeouts: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    /// From answer ready to resumed: the wait for a free worker.
    pub resume_wait_p50_ms: f64,
    pub resume_wait_max_ms: f64,
    /// From queued to the run's start.
    pub start_wait_p50_ms: f64,
    pub start_wait_max_ms: f64,
    /// Per worker: the share of the span it spent running a tenant.
    pub busy: Vec<f64>,
    /// Runs (segments) begun by other requests while one was parked, mean
    /// over parks.
    pub runs_while_parked: f64,
    /// How late the parking lot's clock put a due answer on the run queue:
    /// ready minus (parked + chosen latency), for simulated calls.
    pub timer_late_p50_ms: f64,
    pub timer_late_max_ms: f64,
    pub per_tenant: BTreeMap<String, usize>,
    /// Per tenant: count, latency percentiles and worker time.
    pub tenants: BTreeMap<String, TenantStats>,
    /// `running[k]`: the share of the span with exactly `k` workers running
    /// (`k` from 0 to `workers`).
    pub running: Vec<f64>,
    /// Worker time over workers × span: the pool's CPU utilisation.
    pub utilisation: f64,
    /// The most requests ready to run with no worker free at once.
    pub max_queue: usize,
    /// The most parked runs whose answer was not in yet, at once.
    pub max_waiting_on_io: usize,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Computes [`Stats`].
pub fn stats(trace: &Trace) -> Stats {
    let mut s = Stats {
        requests: trace.requests.len(),
        workers: trace.workers,
        span_ms: (trace.t1 - trace.t0) / 1e3,
        busy: vec![0.0; trace.workers],
        ..Stats::default()
    };
    let mut latencies = Vec::new();
    let mut resume_waits = Vec::new();
    let mut late = Vec::new();
    let mut start_waits = Vec::new();
    let mut edges: Vec<(f64, i32)> = Vec::new();
    let mut starts: Vec<f64> = Vec::new();
    for req in &trace.requests {
        *s.per_tenant.entry(req.tenant.clone()).or_default() += 1;
        if let Some(latency) = req.latency() {
            latencies.push(latency / 1e3);
        }
        if req.status == Some(504) {
            s.timeouts += 1;
        }
        if let Some(first) = req.segments.first() {
            start_waits.push((first.start - req.queued) / 1e3);
        }
        for seg in &req.segments {
            starts.push(seg.start);
            if let (Some(end), Some(busy)) = (seg.end, s.busy.get_mut(seg.worker)) {
                *busy += end - seg.start;
            }
        }
        for park in &req.parks {
            s.parks += 1;
            let until = park.resumed.unwrap_or(trace.t1);
            edges.push((park.at, 1));
            edges.push((until, -1));
            if let (Some(ready), Some(resumed)) = (park.ready, park.resumed) {
                resume_waits.push((resumed - ready) / 1e3);
            }
            if let (Some(ready), Some(due), "timer") = (park.ready, park.due_ms, park.by.as_str()) {
                late.push((ready - park.at) / 1e3 - due);
            }
            if !park.cancelled && park.resumed.is_some() {
                s.resumes += 1;
                if park.resume_worker != Some(park.worker) {
                    s.resumed_elsewhere += 1;
                }
            }
        }
    }
    let span = (trace.t1 - trace.t0).max(1.0);
    for busy in &mut s.busy {
        *busy /= span;
    }
    edges.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut parked = 0i64;
    for (_, delta) in edges {
        parked += delta as i64;
        s.max_parked = s.max_parked.max(parked as usize);
    }
    starts.sort_by(f64::total_cmp);
    let mut during = 0usize;
    for req in &trace.requests {
        for park in &req.parks {
            let until = park.resumed.unwrap_or(trace.t1);
            let lo = starts.partition_point(|&t| t <= park.at);
            let hi = starts.partition_point(|&t| t < until);
            during += hi.saturating_sub(lo);
        }
    }
    s.runs_while_parked = during as f64 / s.parks.max(1) as f64;
    for list in [
        &mut latencies,
        &mut resume_waits,
        &mut start_waits,
        &mut late,
    ] {
        list.sort_by(f64::total_cmp);
    }
    s.p50_ms = percentile(&latencies, 0.5);
    s.p99_ms = percentile(&latencies, 0.99);
    s.resume_wait_p50_ms = percentile(&resume_waits, 0.5);
    s.resume_wait_max_ms = percentile(&resume_waits, 1.0);
    s.timer_late_p50_ms = percentile(&late, 0.5);
    s.timer_late_max_ms = percentile(&late, 1.0);
    s.start_wait_p50_ms = percentile(&start_waits, 0.5);
    s.start_wait_max_ms = percentile(&start_waits, 1.0);

    let mut by_tenant: BTreeMap<String, (Vec<f64>, f64, usize)> = BTreeMap::new();
    for req in &trace.requests {
        let entry = by_tenant.entry(req.tenant.clone()).or_default();
        entry.2 += 1;
        if let Some(latency) = req.latency() {
            entry.0.push(latency / 1e3);
        }
        for seg in &req.segments {
            entry.1 += (seg.end.unwrap_or(trace.t1) - seg.start) / 1e3;
        }
    }
    for (tenant, (mut latencies, cpu_ms, requests)) in by_tenant {
        latencies.sort_by(f64::total_cmp);
        s.tenants.insert(
            tenant,
            TenantStats {
                requests,
                p50_ms: percentile(&latencies, 0.5),
                p99_ms: percentile(&latencies, 0.99),
                cpu_ms,
            },
        );
    }
    let series = series(trace);
    s.running = time_at_level(&series.running, trace.t0, trace.t1, trace.workers + 1)
        .into_iter()
        .map(|t| t / span)
        .collect();
    s.utilisation = s.busy.iter().sum::<f64>() / trace.workers.max(1) as f64;
    s.max_queue = series.queue.iter().map(|&(_, v)| v).max().unwrap_or(0);
    s.max_waiting_on_io = series
        .waiting_on_io
        .iter()
        .map(|&(_, v)| v)
        .max()
        .unwrap_or(0);
    s
}

impl Stats {
    /// The summary as plain lines, for the command line.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} requests over {:.1} ms on {} workers; {} parks, at most {} parked at once",
            self.requests, self.span_ms, self.workers, self.parks, self.max_parked
        );
        let _ = writeln!(
            out,
            "resumed on a different worker: {} of {} ({:.0}%); timeouts (504): {}",
            self.resumed_elsewhere,
            self.resumes,
            100.0 * self.resumed_elsewhere as f64 / self.resumes.max(1) as f64,
            self.timeouts
        );
        let _ = writeln!(
            out,
            "latency p50 {:.2} ms, p99 {:.2} ms; wait for a worker at start p50 {:.3} ms (max {:.2}), \
             at resume p50 {:.3} ms (max {:.2})",
            self.p50_ms,
            self.p99_ms,
            self.start_wait_p50_ms,
            self.start_wait_max_ms,
            self.resume_wait_p50_ms,
            self.resume_wait_max_ms
        );
        let busy: Vec<String> = self
            .busy
            .iter()
            .enumerate()
            .map(|(w, b)| format!("w{w} {:.1}%", b * 100.0))
            .collect();
        let _ = writeln!(
            out,
            "workers busy: {}; runs begun elsewhere while a request was parked: {:.1} on average",
            busy.join(", "),
            self.runs_while_parked
        );
        let _ = writeln!(
            out,
            "simulated answers put on the run queue after their due time by p50 {:.2} ms, max {:.2} ms",
            self.timer_late_p50_ms, self.timer_late_max_ms
        );
        let _ = writeln!(
            out,
            "pool CPU utilisation {:.1}%; time with N workers running: {}; \
             at most {} waiting on I/O and {} waiting for a worker at once",
            self.utilisation * 100.0,
            self.running_text(),
            self.max_waiting_on_io,
            self.max_queue
        );
        for (tenant, t) in &self.tenants {
            let _ = writeln!(
                out,
                "  {tenant:<10} {:>4} requests  p50 {:>7.2} ms  p99 {:>7.2} ms  worker time {:>8.1} ms",
                t.requests, t.p50_ms, t.p99_ms, t.cpu_ms
            );
        }
        out
    }

    /// `0: 12% · 1: 30% · …`, the share of the span at each level.
    pub fn running_text(&self) -> String {
        self.running
            .iter()
            .enumerate()
            .map(|(k, share)| format!("{k}: {:.1}%", share * 100.0))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The share of the span with at least `k` workers running.
    pub fn running_at_least(&self, k: usize) -> f64 {
        self.running.iter().skip(k).sum()
    }
}

// ----------------------------------------------------------- chrome trace

const TID_ACCEPT: u64 = 1;
const TID_LOT: u64 = 2;
const TID_FETCH: u64 = 3;
const TID_WORKER: u64 = 10;

/// The trace as Chrome Trace Event JSON, for <https://ui.perfetto.dev> or
/// `chrome://tracing`: a track per worker with a slice per run segment,
/// flow arrows from each park to its resume, the parked intervals as async
/// slices per request, the accept, parking-lot and fetch-pool events on
/// tracks of their own, and four counter tracks — parked runs, waiting on
/// I/O, workers running, waiting for a worker ([`series`]).
pub fn chrome_trace(trace: &Trace) -> String {
    let mut events: Vec<String> = Vec::new();
    let ts = |t: f64| t - trace.t0;
    let meta = |events: &mut Vec<String>, tid: u64, name: &str, order: u64| {
        events.push(format!(
            "{{\"ph\":\"M\",\"pid\":1,\"tid\":{tid},\"name\":\"thread_name\",\"args\":{{\"name\":{}}}}}",
            quote(name)
        ));
        events.push(format!(
            "{{\"ph\":\"M\",\"pid\":1,\"tid\":{tid},\"name\":\"thread_sort_index\",\"args\":{{\"sort_index\":{order}}}}}"
        ));
    };
    events.push(
        "{\"ph\":\"M\",\"pid\":1,\"name\":\"process_name\",\"args\":{\"name\":\"cove-edge\"}}"
            .to_string(),
    );
    for w in 0..trace.workers {
        meta(
            &mut events,
            TID_WORKER + w as u64,
            &format!("worker {w}"),
            w as u64,
        );
    }
    meta(&mut events, TID_ACCEPT, "accept / queue", 100);
    meta(&mut events, TID_LOT, "parking lot (timer)", 101);
    meta(&mut events, TID_FETCH, "fetch pool", 102);
    let mut flow = 0u64;
    for req in &trace.requests {
        let name = format!("{} #{}", req.tenant, req.id);
        if let Some(accepted) = req.accepted {
            events.push(format!(
                "{{\"ph\":\"i\",\"s\":\"t\",\"pid\":1,\"tid\":{TID_ACCEPT},\"ts\":{:.3},\"name\":{},\"cat\":\"accept\"}}",
                ts(accepted),
                quote(&format!("connection for #{}", req.id))
            ));
        }
        // Each segment, and what began and ended it.
        let mut park_at = 0;
        for (index, seg) in req.segments.iter().enumerate() {
            let end = seg.end.unwrap_or(trace.t1);
            let began = match seg.began {
                Began::Run => format!("run {}", req.path),
                Began::Resume => {
                    let p = &req.parks[index - 1];
                    format!("resumed after {} {}", p.op, p.target)
                }
                Began::Cancel => "cancelled: deadline passed while parked".to_string(),
            };
            let ended = match req.parks.get(park_at) {
                Some(p) if (p.at - end).abs() < 1e-6 => {
                    park_at += 1;
                    format!("parked at {} {}", p.op, p.target)
                }
                _ => match req.status {
                    Some(status) if seg.end.is_some() => format!("answered {status}"),
                    _ => "still running at the dump".to_string(),
                },
            };
            events.push(format!(
                "{{\"ph\":\"X\",\"pid\":1,\"tid\":{},\"ts\":{:.3},\"dur\":{:.3},\"name\":{},\"cat\":{},\
                 \"args\":{{\"request\":{},\"path\":{},\"began\":{},\"ended\":{}}}}}",
                TID_WORKER + seg.worker as u64,
                ts(seg.start),
                (end - seg.start).max(0.001),
                quote(&name),
                quote(&req.tenant),
                req.id,
                quote(&req.path),
                quote(&began),
                quote(&ended)
            ));
        }
        // Flow arrows: the segment that parked to the one that resumed.
        for (index, park) in req.parks.iter().enumerate() {
            let (Some(before), Some(after)) =
                (req.segments.get(index), req.segments.get(index + 1))
            else {
                continue;
            };
            let mid = |s: &Seg| (s.start + s.end.unwrap_or(trace.t1)) / 2.0;
            flow += 1;
            let flow_name = if park.resume_worker == Some(park.worker) {
                "resume (same worker)"
            } else {
                "resume (other worker)"
            };
            events.push(format!(
                "{{\"ph\":\"s\",\"pid\":1,\"tid\":{},\"ts\":{:.3},\"id\":{flow},\"name\":\"{flow_name}\",\"cat\":\"park\"}}",
                TID_WORKER + before.worker as u64,
                ts(mid(before))
            ));
            events.push(format!(
                "{{\"ph\":\"f\",\"bp\":\"e\",\"pid\":1,\"tid\":{},\"ts\":{:.3},\"id\":{flow},\"name\":\"{flow_name}\",\"cat\":\"park\"}}",
                TID_WORKER + after.worker as u64,
                ts(mid(after))
            ));
        }
        // Async slices on a track per request: the whole request, and
        // inside it waiting in the queue, parked, and ready but waiting for
        // a worker. Perfetto names the track after the outer slice.
        let slice = |events: &mut Vec<String>, label: String, from: f64, to: f64| {
            if to < from {
                return;
            }
            let id = format!("\"0x{:x}\"", req.id);
            let args = format!(
                "\"args\":{{\"request\":{},\"tenant\":{}}}",
                req.id,
                quote(&req.tenant)
            );
            events.push(format!(
                "{{\"ph\":\"b\",\"pid\":1,\"tid\":{TID_ACCEPT},\"ts\":{:.3},\"id2\":{{\"local\":{id}}},\"cat\":\"requests\",\"name\":{},{args}}}",
                ts(from),
                quote(&label)
            ));
            events.push(format!(
                "{{\"ph\":\"e\",\"pid\":1,\"tid\":{TID_ACCEPT},\"ts\":{:.3},\"id2\":{{\"local\":{id}}},\"cat\":\"requests\",\"name\":{}}}",
                ts(to),
                quote(&label)
            ));
        };
        let whole = format!(
            "{name} {} → {}",
            req.path,
            req.status.map_or("running".to_string(), |s| s.to_string())
        );
        let outer = events.len();
        slice(&mut events, whole, req.queued, req.last());
        // The outer slice's end goes after every inner one.
        let outer_end = events.remove(outer + 1);
        if let Some(first) = req.segments.first() {
            slice(&mut events, "queued".to_string(), req.queued, first.start);
        }
        for park in &req.parks {
            let ready = park.ready.unwrap_or(trace.t1);
            slice(
                &mut events,
                format!("parked: {} {}", park.op, park.target),
                park.at,
                ready,
            );
            if let Some(resumed) = park.resumed {
                slice(
                    &mut events,
                    "ready, waiting for a worker".to_string(),
                    ready,
                    resumed,
                );
            }
            if let Some(ready) = park.ready {
                let tid = if park.by == "fetcher" {
                    TID_FETCH
                } else {
                    TID_LOT
                };
                events.push(format!(
                    "{{\"ph\":\"i\",\"s\":\"t\",\"pid\":1,\"tid\":{tid},\"ts\":{:.3},\"name\":{},\"cat\":\"answer\",\
                     \"args\":{{\"by\":{},\"target\":{}}}}}",
                    ts(ready),
                    quote(&format!(
                        "{} #{}: {}",
                        if park.by == "deadline" {
                            "deadline"
                        } else {
                            "answer"
                        },
                        req.id,
                        park.target
                    )),
                    quote(&park.by),
                    quote(&park.target)
                ));
            }
        }
        events.push(outer_end);
    }
    // Counter tracks: the strips under the HTML's swimlanes.
    let series = series(trace);
    for (name, key, steps) in [
        ("parked runs", "parked", &series.parked),
        ("waiting on I/O", "waiting", &series.waiting_on_io),
        ("workers running", "running", &series.running),
        ("waiting for a worker", "waiting", &series.queue),
    ] {
        for &(t, value) in steps {
            events.push(format!(
                "{{\"ph\":\"C\",\"pid\":1,\"ts\":{:.3},\"name\":\"{name}\",\"args\":{{\"{key}\":{value}}}}}",
                ts(t)
            ));
        }
    }
    let mut out = String::from("{\"displayTimeUnit\":\"ms\",\"traceEvents\":[\n");
    out.push_str(&events.join(",\n"));
    out.push_str("\n]}\n");
    out
}

// -------------------------------------------------------------------- svg

/// Escapes text for XML.
fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn tenant_slot(tenant: &str) -> Option<usize> {
    TENANTS.iter().position(|t| *t == tenant)
}

fn tenant_class(tenant: &str) -> String {
    match tenant_slot(tenant) {
        Some(at) => format!("t{at}"),
        None => "tx".to_string(),
    }
}

/// The colours, as custom properties with a dark-mode block.
fn style() -> String {
    let mut css = String::from(
        ":root, svg.edge-timeline { --surface:#fcfcfb; --ink:#0b0b0b; --ink2:#52514e; \
         --grid:#e7e6e1; --wait:#a9a8a2; --critical:#e34948; --other:#8a8984;",
    );
    for (at, hex) in LIGHT.iter().enumerate() {
        let _ = write!(css, " --c{at}:{hex};");
    }
    css.push_str(" }\n@media (prefers-color-scheme: dark) { :root, svg.edge-timeline { --surface:#1a1a19; --ink:#ffffff; --ink2:#c3c2b7; --grid:#33332f; --wait:#6f6e68; --critical:#e34948; --other:#8a8984;");
    for (at, hex) in DARK.iter().enumerate() {
        let _ = write!(css, " --c{at}:{hex};");
    }
    css.push_str(" } }\n");
    for at in 0..TENANTS.len() {
        let _ = writeln!(
            css,
            ".t{at} {{ fill: var(--c{at}); }} .l{at} {{ stroke: var(--c{at}); }}"
        );
    }
    css.push_str(
        ".tx { fill: var(--other); } .lx { stroke: var(--other); }\n\
         svg.edge-timeline text { font-family: system-ui, -apple-system, 'Segoe UI', sans-serif; fill: var(--ink2); font-size: 11px; }\n\
         svg.edge-timeline .ink { fill: var(--ink); }\n\
         svg.edge-timeline .head { fill: var(--ink); font-size: 13px; font-weight: 600; }\n\
         svg.edge-timeline .title { fill: var(--ink); font-size: 16px; font-weight: 600; }\n\
         svg.edge-timeline .tick { font-variant-numeric: tabular-nums; }\n\
         svg.edge-timeline .grid { stroke: var(--grid); stroke-width: 1; }\n\
         svg.edge-timeline .wait { stroke: var(--wait); stroke-width: 1; }\n\
         svg.edge-timeline .bg { fill: var(--surface); }\n\
         svg.edge-timeline .moved { fill: var(--ink); stroke: var(--surface); stroke-width: 1; }\n\
         svg.edge-timeline .crit { fill: var(--critical); font-weight: 700; }\n\
         svg.edge-timeline .critline { stroke: var(--critical); stroke-width: 2; stroke-linecap: round; }\n\
         svg.edge-timeline .hover { fill: var(--ink); opacity: 0.06; }\n\
         svg.edge-timeline .halo { paint-order: stroke; stroke: var(--surface); stroke-width: 3px; stroke-linejoin: round; }\n\
         svg.edge-timeline .area { fill: var(--ink2); fill-opacity: 0.22; stroke: none; }\n\
         svg.edge-timeline .step { fill: none; stroke: var(--ink2); stroke-width: 1; stroke-linejoin: round; }\n",
    );
    css
}

/// Nice tick spacing for a span of `span` milliseconds and about `count`
/// ticks.
fn tick_step(span: f64, count: f64) -> f64 {
    let raw = span / count;
    let magnitude = 10f64.powf(raw.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if m * magnitude >= raw {
            return m * magnitude;
        }
    }
    10.0 * magnitude
}

/// The geometry both views share.
pub struct Layout {
    pub width: f64,
    pub left: f64,
    pub right: f64,
    pub lane: f64,
    pub row: f64,
    /// Where each part starts, top to bottom.
    pub workers_top: f64,
    /// The three strips under the swimlanes, each `(top, height)`: workers
    /// running, parked runs waiting on I/O, requests waiting for a worker.
    pub strips: [(f64, f64); 3],
    pub axis_y: f64,
    pub requests_top: f64,
    pub height: f64,
}

/// Room above each strip for its title.
const STRIP_TITLE: f64 = 20.0;

impl Layout {
    fn new(trace: &Trace, header: f64) -> Layout {
        let width = 1240.0;
        let lane = 26.0;
        let n = trace.requests.len().max(1) as f64;
        // Rows as tall as fit in about 900 px, between 3 and 14 px each.
        let row = (900.0 / n).clamp(3.0, 14.0).floor();
        let workers_top = header + 22.0;
        // The running strip is 12 px a worker, so a step of one is legible.
        let running_h = (12.0 * trace.workers as f64).max(24.0);
        let mut top = workers_top + lane * trace.workers as f64 + 4.0 + STRIP_TITLE;
        let mut strips = [(0.0, 0.0); 3];
        for (at, height) in [running_h, 40.0, 40.0].into_iter().enumerate() {
            strips[at] = (top, height);
            top += height + 8.0 + STRIP_TITLE;
        }
        let axis_y = top - STRIP_TITLE;
        let requests_top = axis_y + 44.0;
        let height = requests_top + row * n + 40.0;
        Layout {
            width,
            left: 92.0,
            right: 70.0,
            lane,
            row,
            workers_top,
            strips,
            axis_y,
            requests_top,
            height,
        }
    }

    fn plot(&self) -> f64 {
        self.width - self.left - self.right
    }
}

/// The figure as one SVG. `standalone` puts the title, summary and legend
/// inside it (for an image file); the HTML page draws those as HTML.
pub fn svg(trace: &Trace, standalone: bool) -> String {
    let s = stats(trace);
    let header = if standalone { 124.0 } else { 6.0 };
    let lay = Layout::new(trace, header);
    let span = trace.t1 - trace.t0;
    let x = |t: f64| lay.left + (t - trace.t0) / span * lay.plot();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg class=\"edge-timeline\" xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" \
         viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Request timeline: {n} requests on {k} workers\">",
        w = lay.width,
        h = lay.height,
        n = s.requests,
        k = trace.workers
    );
    if standalone {
        let _ = writeln!(out, "<style>\n{}</style>", style());
    }
    let _ = writeln!(
        out,
        "<rect class=\"bg\" x=\"0\" y=\"0\" width=\"{}\" height=\"{}\"/>",
        lay.width, lay.height
    );
    if standalone {
        let _ = writeln!(
            out,
            "<text class=\"title\" x=\"{}\" y=\"26\">cove-edge: where each request ran, parked and resumed</text>",
            lay.left
        );
        let _ = writeln!(
            out,
            "<text class=\"ink\" x=\"{}\" y=\"48\">{}</text>",
            lay.left,
            esc(&summary_line(&s))
        );
        let _ = writeln!(
            out,
            "<text class=\"ink\" x=\"{}\" y=\"68\">{}</text>",
            lay.left,
            esc(&format!(
                "pool CPU {:.0}% · share of the span with N workers running: {} · at most {} waiting for a worker",
                s.utilisation * 100.0,
                s.running_text(),
                s.max_queue
            ))
        );
        legend_svg(&mut out, lay.left, 96.0);
    }

    // Gridlines and the one time axis, between the two views.
    let span_ms = span / 1e3;
    let step = tick_step(span_ms, 10.0);
    let mut tick = 0.0;
    while tick <= span_ms + 1e-9 {
        let tx = x(trace.t0 + tick * 1e3);
        let _ = writeln!(
            out,
            "<line class=\"grid\" x1=\"{tx:.1}\" y1=\"{:.1}\" x2=\"{tx:.1}\" y2=\"{:.1}\"/>",
            lay.workers_top - 4.0,
            lay.height - 30.0
        );
        let _ = writeln!(
            out,
            "<text class=\"tick\" x=\"{tx:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            lay.axis_y + 16.0,
            format_ms(tick, step)
        );
        tick += step;
    }
    let _ = writeln!(
        out,
        "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">ms</text>",
        lay.left - 8.0,
        lay.axis_y + 16.0
    );

    // (i) Worker swimlanes.
    let _ = writeln!(
        out,
        "<text class=\"head\" x=\"{}\" y=\"{:.1}\">Worker threads: a bar wherever one is running a tenant's isolate</text>",
        lay.left,
        lay.workers_top - 8.0
    );
    let mut per_worker: Vec<Vec<(f64, f64, usize)>> = vec![Vec::new(); trace.workers];
    for (index, req) in trace.requests.iter().enumerate() {
        for seg in &req.segments {
            if let Some(lane) = per_worker.get_mut(seg.worker) {
                lane.push((seg.start, seg.end.unwrap_or(trace.t1), index));
            }
        }
    }
    for (w, lane) in per_worker.iter_mut().enumerate() {
        lane.sort_by(|a, b| a.0.total_cmp(&b.0));
        let y = lay.workers_top + lay.lane * w as f64;
        let _ = writeln!(
            out,
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">worker {w}</text>",
            lay.left - 8.0,
            y + lay.lane / 2.0 + 4.0
        );
        let _ = writeln!(
            out,
            "<line class=\"grid\" x1=\"{}\" y1=\"{:.1}\" x2=\"{}\" y2=\"{:.1}\"/>",
            lay.left,
            y + lay.lane - 2.0,
            lay.width - lay.right,
            y + lay.lane - 2.0
        );
        for at in 0..lane.len() {
            let (start, end, index) = lane[at];
            let x0 = x(start);
            let mut w_px = (x(end) - x0).max(1.5);
            // The 2 px surface gap before the next bar, where there is room.
            if let Some(next) = lane.get(at + 1) {
                let room = x(next.0) - 2.0 - x0;
                if room < w_px {
                    w_px = room.max(1.0);
                }
            }
            let _ = writeln!(
                out,
                "<rect class=\"{}\" x=\"{x0:.2}\" y=\"{:.1}\" width=\"{w_px:.2}\" height=\"{:.1}\"/>",
                tenant_class(&trace.requests[index].tenant),
                y + 3.0,
                lay.lane - 8.0
            );
        }
    }

    // The strips: how many workers ran at each instant, how many parked runs
    // were waiting on I/O, how many requests waited for a worker. Neutral
    // ink, not a tenant's colour: they count every tenant together.
    let series = series(trace);
    let titles = [
        format!(
            "Workers running at once, 0 to {} — pool CPU {:.0}%, two or more running {:.0}% of the span",
            trace.workers,
            s.utilisation * 100.0,
            s.running_at_least(2) * 100.0
        ),
        format!(
            "Parked runs waiting on I/O (park to answer), at most {} at once",
            s.max_waiting_on_io
        ),
        format!(
            "Requests ready to run with no worker free (the run queue), at most {} at once",
            s.max_queue
        ),
    ];
    let tops = [
        trace.workers.max(1),
        s.max_waiting_on_io.max(1),
        s.max_queue.max(1),
    ];
    for (at, steps) in [&series.running, &series.waiting_on_io, &series.queue]
        .into_iter()
        .enumerate()
    {
        strip(
            &mut out,
            &lay,
            &x,
            steps,
            lay.strips[at],
            tops[at],
            &titles[at],
            (trace.t0, trace.t1),
            at == 0,
        );
    }

    // (ii) Request lanes.
    let _ = writeln!(
        out,
        "<text class=\"head\" x=\"{}\" y=\"{:.1}\">Requests, in arrival order: bar = running, thin line = parked, grey = waiting for a worker</text>",
        lay.left,
        lay.requests_top - 10.0
    );
    let mut labelled: Vec<(f64, f64)> = Vec::new();
    let mut named = std::collections::BTreeSet::new();
    for (index, req) in trace.requests.iter().enumerate() {
        let top = lay.requests_top + lay.row * index as f64;
        let mid = top + lay.row / 2.0;
        let class = tenant_class(&req.tenant);
        let slot = class.trim_start_matches('t');
        let line = |out: &mut String, cls: &str, a: f64, b: f64, width: f64| {
            if b > a {
                let _ = writeln!(
                    out,
                    "<line class=\"{cls}\" x1=\"{:.2}\" y1=\"{mid:.2}\" x2=\"{:.2}\" y2=\"{mid:.2}\" stroke-width=\"{width}\"/>",
                    x(a),
                    x(b)
                );
            }
        };
        if let Some(first) = req.segments.first() {
            line(&mut out, "wait", req.queued, first.start, 1.0);
        }
        let parked_width = if lay.row >= 8.0 { 2.0 } else { 1.0 };
        for park in &req.parks {
            let ready = park.ready.unwrap_or(trace.t1);
            line(&mut out, &format!("l{slot}"), park.at, ready, parked_width);
            if let Some(resumed) = park.resumed {
                line(&mut out, "wait", ready, resumed, 1.0);
            }
        }
        let bar_h = (lay.row - 1.0).max(2.0);
        for seg in &req.segments {
            let x0 = x(seg.start);
            let w_px = (x(seg.end.unwrap_or(trace.t1)) - x0).max(1.5);
            let _ = writeln!(
                out,
                "<rect class=\"{class}\" x=\"{x0:.2}\" y=\"{:.2}\" width=\"{w_px:.2}\" height=\"{bar_h:.1}\"/>",
                top + (lay.row - bar_h) / 2.0
            );
        }
        // A resume on a different worker: a small ink diamond.
        for park in &req.parks {
            if let (Some(resumed), Some(to)) = (park.resumed, park.resume_worker) {
                if to != park.worker && !park.cancelled {
                    let r = (lay.row * 0.5).clamp(2.5, 4.5);
                    let cx = x(resumed);
                    let _ = writeln!(
                        out,
                        "<path class=\"moved\" d=\"M{cx:.2},{:.2} L{:.2},{mid:.2} L{cx:.2},{:.2} L{:.2},{mid:.2} Z\"/>",
                        mid - r,
                        cx + r,
                        mid + r,
                        cx - r
                    );
                }
            }
        }
        // A timeout: status, not tenant — a red cross and its label.
        if req.status == Some(504) {
            let at = req.ended.unwrap_or(trace.t1);
            let cx = x(at);
            let r = (lay.row * 0.5).clamp(2.5, 4.0);
            let _ = writeln!(
                out,
                "<path class=\"critline\" d=\"M{:.2},{:.2} L{:.2},{:.2} M{:.2},{:.2} L{:.2},{:.2}\"/>",
                cx - r,
                mid - r,
                cx + r,
                mid + r,
                cx - r,
                mid + r,
                cx + r,
                mid - r
            );
            let free = labelled
                .iter()
                .all(|&(lx, ly)| (lx - cx).abs() > 40.0 || (ly - mid).abs() > 11.0);
            if free {
                labelled.push((cx, mid));
                let _ = writeln!(
                    out,
                    "<text class=\"ink halo\" font-weight=\"600\" x=\"{:.1}\" y=\"{:.1}\">504</text>",
                    cx + 7.0,
                    mid + 4.0
                );
            }
        } else if !named.contains(&req.tenant) && tenant_slot(&req.tenant).is_some() {
            // A direct label: each tenant's name once, at the end of its
            // first request long enough to carry it (a run of a few
            // microseconds is a dot on the arrival diagonal, and a label there
            // would sit on its neighbours) and with room beside it.
            let lx = x(req.last()) + 6.0;
            let free = lx + 70.0 < lay.width
                && x(req.last()) - x(req.queued) >= 30.0
                && labelled
                    .iter()
                    .all(|&(ox, oy)| (ox - lx).abs() > 80.0 || (oy - mid).abs() > 11.0);
            if free {
                named.insert(req.tenant.clone());
                labelled.push((lx, mid));
                let _ = writeln!(
                    out,
                    "<text class=\"halo\" x=\"{lx:.1}\" y=\"{:.1}\">{}</text>",
                    mid + 4.0,
                    esc(&req.tenant)
                );
            }
        }
    }
    let _ = writeln!(
        out,
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">first</text>",
        lay.left - 8.0,
        lay.requests_top + 8.0
    );
    let _ = writeln!(
        out,
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">last</text>",
        lay.left - 8.0,
        lay.requests_top + lay.row * trace.requests.len() as f64
    );
    let _ = writeln!(
        out,
        "<text x=\"{}\" y=\"{:.1}\">Runs shorter than 1.5 px are drawn 1.5 px wide (the strips count them at their true length); hover (in the HTML) or the table gives exact times.</text>",
        lay.left,
        lay.height - 12.0
    );
    out.push_str("</svg>\n");
    out
}

/// One strip: a step line over a light area, on an axis from 0 to `top`,
/// with a gridline at every level when `every_level` (the running strip,
/// whose levels are the workers) and at 0 and `top` otherwise.
#[allow(clippy::too_many_arguments)]
fn strip(
    out: &mut String,
    lay: &Layout,
    x: &dyn Fn(f64) -> f64,
    steps: &Steps,
    (top, height): (f64, f64),
    max: usize,
    title: &str,
    (t0, t1): (f64, f64),
    every_level: bool,
) {
    let base = top + height;
    let y = |v: usize| base - v as f64 / max as f64 * height;
    let _ = writeln!(
        out,
        "<text class=\"ink\" x=\"{}\" y=\"{:.1}\">{}</text>",
        lay.left,
        top - 7.0,
        esc(title)
    );
    let levels: Vec<usize> = if every_level {
        (0..=max).collect()
    } else {
        vec![0, max]
    };
    for level in levels {
        let ly = y(level);
        let _ = writeln!(
            out,
            "<line class=\"grid\" x1=\"{}\" y1=\"{ly:.1}\" x2=\"{}\" y2=\"{ly:.1}\"/>\
             <text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{level}</text>",
            lay.left,
            lay.width - lay.right,
            lay.left - 8.0,
            ly + 4.0
        );
    }
    let mut line = format!("M{:.2},{base:.2}", x(t0));
    for &(t, value) in steps {
        let _ = write!(line, " H{:.2} V{:.2}", x(t.clamp(t0, t1)), y(value));
    }
    let _ = write!(line, " H{:.2}", x(t1));
    let _ = writeln!(
        out,
        "<path class=\"area\" d=\"{line} V{base:.2} Z\"/>\n<path class=\"step\" d=\"{line}\"/>"
    );
}

fn format_ms(value: f64, step: f64) -> String {
    if step >= 1.0 {
        format!("{value:.0}")
    } else if step >= 0.1 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn summary_line(s: &Stats) -> String {
    format!(
        "{} requests · {} workers · {} parked at most at once · {} of {} resumes on a different worker ({:.0}%) · {} timeouts · latency p50 {:.1} ms, p99 {:.1} ms",
        s.requests,
        s.workers,
        s.max_parked,
        s.resumed_elsewhere,
        s.resumes,
        100.0 * s.resumed_elsewhere as f64 / s.resumes.max(1) as f64,
        s.timeouts,
        s.p50_ms,
        s.p99_ms
    )
}

/// The legend, drawn inside the SVG (for the standalone image).
fn legend_svg(out: &mut String, left: f64, y: f64) {
    let mut lx = left;
    for (at, name) in TENANTS.iter().enumerate() {
        let _ = writeln!(
            out,
            "<rect class=\"t{at}\" x=\"{lx:.1}\" y=\"{:.1}\" width=\"12\" height=\"12\" rx=\"2\"/>\
             <text class=\"ink\" x=\"{:.1}\" y=\"{:.1}\">{name}</text>",
            y - 10.0,
            lx + 17.0,
            y
        );
        lx += 30.0 + name.len() as f64 * 7.0;
    }
    lx += 12.0;
    let keys = [
        (
            "<line class=\"wait\" x1=\"0\" y1=\"-4\" x2=\"16\" y2=\"-4\" stroke-width=\"1\"/>",
            "waiting for a worker",
        ),
        (
            "<line style=\"stroke: var(--ink2)\" x1=\"0\" y1=\"-4\" x2=\"16\" y2=\"-4\" stroke-width=\"2\"/>",
            "parked (tenant colour)",
        ),
        (
            "<path class=\"moved\" d=\"M8,-8.5 L12.5,-4 L8,0.5 L3.5,-4 Z\"/>",
            "resumed on another worker",
        ),
        (
            "<path class=\"critline\" d=\"M4,-8 L12,0 M4,0 L12,-8\"/>",
            "504 timeout",
        ),
    ];
    for (mark, label) in keys {
        let _ = writeln!(
            out,
            "<g transform=\"translate({lx:.1},{y:.1})\">{mark}<text class=\"ink\" x=\"21\" y=\"0\">{label}</text></g>"
        );
        lx += 34.0 + label.len() as f64 * 6.2;
    }
}

// ------------------------------------------------------------------- html

/// The page: summary, legend, the SVG, a hover readout and a table view, in
/// one file with no external resource.
pub fn html(trace: &Trace) -> String {
    let s = stats(trace);
    let lay = Layout::new(trace, 6.0);
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>cove-edge request timeline</title>\n<style>\n");
    out.push_str(&style());
    out.push_str(
        "body { margin: 0; padding: 24px 28px; background: var(--surface); color: var(--ink);\
         font: 14px/1.45 system-ui, -apple-system, 'Segoe UI', sans-serif; }\n\
         h1 { font-size: 20px; margin: 0 0 4px; }\n\
         p.sub { color: var(--ink2); margin: 0 0 16px; }\n\
         .stats { display: flex; flex-wrap: wrap; gap: 8px 28px; margin: 0 0 14px; }\n\
         .stat { display: flex; flex-direction: column; }\n\
         .stat b { font-size: 20px; font-weight: 600; color: var(--ink); }\n\
         .stat span { color: var(--ink2); font-size: 12px; }\n\
         .legend { display: flex; flex-wrap: wrap; gap: 6px 18px; margin: 0 0 8px; color: var(--ink); font-size: 13px; align-items: center; }\n\
         .legend i { display: inline-block; width: 12px; height: 12px; border-radius: 2px; margin-right: 6px; vertical-align: -1px; }\n\
         .legend svg { vertical-align: -2px; margin-right: 6px; }\n\
         .figure { position: relative; overflow-x: auto; }\n\
         #tip { position: fixed; pointer-events: none; display: none; max-width: 460px; background: var(--surface);\
         color: var(--ink); border: 1px solid var(--grid); border-radius: 6px; padding: 8px 10px; font-size: 12px;\
         box-shadow: 0 2px 10px rgba(0,0,0,.12); white-space: pre-line; z-index: 2; }\n\
         #tip strong { font-size: 13px; }\n\
         details { margin-top: 18px; } summary { cursor: pointer; color: var(--ink); }\n\
         table { border-collapse: collapse; font-size: 12px; margin-top: 8px; font-variant-numeric: tabular-nums; }\n\
         th, td { padding: 2px 10px 2px 0; text-align: left; border-bottom: 1px solid var(--grid); }\n\
         th { color: var(--ink2); font-weight: 600; }\n\
         .note { color: var(--ink2); font-size: 12px; }\n",
    );
    out.push_str("</style></head><body>\n");
    out.push_str("<h1>cove-edge: where each request ran, parked and resumed</h1>\n");
    let _ = writeln!(
        out,
        "<p class=\"sub\">{:.1} ms of a mixed load. Above, the worker threads; below, one row per request on the same time axis. Hover either view for details.</p>",
        s.span_ms
    );
    let stat = |out: &mut String, value: String, label: &str| {
        let _ = write!(
            out,
            "<div class=\"stat\"><b>{}</b><span>{}</span></div>",
            esc(&value),
            esc(label)
        );
    };
    out.push_str("<div class=\"stats\">");
    stat(&mut out, s.requests.to_string(), "requests");
    stat(&mut out, s.workers.to_string(), "workers");
    stat(&mut out, s.max_parked.to_string(), "max parked at once");
    stat(
        &mut out,
        format!(
            "{:.0}%",
            100.0 * s.resumed_elsewhere as f64 / s.resumes.max(1) as f64
        ),
        &format!(
            "resumes on a different worker ({} of {})",
            s.resumed_elsewhere, s.resumes
        ),
    );
    stat(&mut out, s.timeouts.to_string(), "timeouts (504)");
    stat(&mut out, format!("{:.1} ms", s.p50_ms), "p50 latency");
    stat(&mut out, format!("{:.1} ms", s.p99_ms), "p99 latency");
    stat(
        &mut out,
        format!("{:.0}%", s.utilisation * 100.0),
        "pool CPU utilisation",
    );
    stat(
        &mut out,
        format!("{:.0}%", s.running_at_least(2) * 100.0),
        "of the span with 2+ workers running",
    );
    stat(
        &mut out,
        s.max_waiting_on_io.to_string(),
        "max waiting on I/O at once",
    );
    stat(
        &mut out,
        s.max_queue.to_string(),
        "max waiting for a worker at once",
    );
    out.push_str("</div>\n<div class=\"stats\">");
    for (k, share) in s.running.iter().enumerate() {
        stat(
            &mut out,
            format!("{:.1}%", share * 100.0),
            &format!(
                "of the span with {k} worker{} running",
                if k == 1 { "" } else { "s" }
            ),
        );
    }
    out.push_str("</div>\n<div class=\"legend\">");
    for (at, name) in TENANTS.iter().enumerate() {
        let _ = write!(
            out,
            "<span><i style=\"background: var(--c{at})\"></i>{name}</span>"
        );
    }
    out.push_str(
        "<span><svg width=\"16\" height=\"8\"><line x1=\"0\" y1=\"4\" x2=\"16\" y2=\"4\" style=\"stroke: var(--wait)\" stroke-width=\"1\"/></svg>waiting for a worker</span>\
         <span><svg width=\"16\" height=\"8\"><line x1=\"0\" y1=\"4\" x2=\"16\" y2=\"4\" style=\"stroke: var(--ink2)\" stroke-width=\"2\"/></svg>parked (tenant colour)</span>\
         <span><svg width=\"10\" height=\"10\"><path d=\"M5,0.5 L9.5,5 L5,9.5 L0.5,5 Z\" style=\"fill: var(--ink)\"/></svg>resumed on another worker</span>\
         <span><svg width=\"10\" height=\"10\"><path d=\"M1,1 L9,9 M1,9 L9,1\" style=\"stroke: var(--critical)\" stroke-width=\"2\" stroke-linecap=\"round\"/></svg>504 timeout</span>",
    );
    out.push_str("</div>\n<div class=\"figure\" id=\"figure\">\n");
    out.push_str(&svg(trace, false));
    out.push_str("</div>\n<div id=\"tip\"></div>\n");

    // Per tenant, then every request.
    out.push_str(
        "<details open><summary>Per tenant</summary>\n<table><thead><tr>\
         <th>tenant</th><th>requests</th><th>p50 ms</th><th>p99 ms</th><th>worker time ms</th>\
         </tr></thead><tbody>\n",
    );
    for (tenant, t) in &s.tenants {
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{:.2}</td><td>{:.2}</td><td>{:.1}</td></tr>",
            esc(tenant),
            t.requests,
            t.p50_ms,
            t.p99_ms,
            t.cpu_ms
        );
    }
    out.push_str("</tbody></table></details>\n");
    out.push_str("<details><summary>Every request as a table</summary>\n<table><thead><tr>\
                  <th>id</th><th>tenant</th><th>path</th><th>status</th><th>queued ms</th><th>answered ms</th>\
                  <th>latency ms</th><th>workers</th><th>parks</th></tr></thead><tbody>\n");
    let ms = |t: f64| (t - trace.t0) / 1e3;
    for req in &trace.requests {
        let workers: Vec<String> = req
            .segments
            .iter()
            .map(|seg| format!("w{}", seg.worker))
            .collect();
        let parks: Vec<String> = req
            .parks
            .iter()
            .map(|p| {
                format!(
                    "{} {} {:.1} ms ({})",
                    p.op,
                    p.target,
                    (p.ready.unwrap_or(trace.t1) - p.at) / 1e3,
                    p.by
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.3}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            req.id,
            esc(&req.tenant),
            esc(&req.path),
            req.status.map_or("-".to_string(), |s| s.to_string()),
            ms(req.queued),
            req.written.or(req.ended).map_or("-".to_string(), |t| format!("{:.3}", ms(t))),
            req.latency().map_or("-".to_string(), |l| format!("{:.3}", l / 1e3)),
            workers.join(" → "),
            esc(&parks.join("; "))
        );
    }
    out.push_str("</tbody></table></details>\n");
    let _ = writeln!(
        out,
        "<p class=\"note\">Recorded by <code>cove-edge --timeline</code>; drawn by <code>cove-edge-timeline</code>. \
         The same timeline as a Perfetto trace: <code>--perfetto</code>, opened at https://ui.perfetto.dev.</p>"
    );

    // The data the hover readout reads, and the script.
    out.push_str("<script>\n");
    let _ = writeln!(
        out,
        "const T0 = {}, T1 = {}, LEFT = {}, PLOT = {}, LANE = {}, ROW = {}, WTOP = {}, RTOP = {}, WORKERS = {};",
        trace.t0,
        trace.t1,
        lay.left,
        lay.plot(),
        lay.lane,
        lay.row,
        lay.workers_top,
        lay.requests_top,
        trace.workers
    );
    out.push_str("const REQ = [\n");
    for req in &trace.requests {
        let segs: Vec<String> = req
            .segments
            .iter()
            .map(|seg| {
                format!(
                    "[{},{:.3},{:.3},\"{}\"]",
                    seg.worker,
                    seg.start,
                    seg.end.unwrap_or(trace.t1),
                    match seg.began {
                        Began::Run => "run",
                        Began::Resume => "resume",
                        Began::Cancel => "cancel",
                    }
                )
            })
            .collect();
        let parks: Vec<String> = req
            .parks
            .iter()
            .map(|p| {
                format!(
                    "[{},{},{},{:.3},{},{},{},{},{}]",
                    quote(&p.op),
                    quote(&p.target),
                    p.worker,
                    p.at,
                    p.ready.map_or("null".to_string(), |t| format!("{t:.3}")),
                    p.resumed.map_or("null".to_string(), |t| format!("{t:.3}")),
                    p.resume_worker
                        .map_or("null".to_string(), |w| w.to_string()),
                    quote(&p.by),
                    p.due_ms.map_or("null".to_string(), |d| format!("{d:.3}"))
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "[{},{},{},{},{:.3},{},[{}],[{}]],",
            req.id,
            quote(&req.tenant),
            quote(&req.path),
            req.status.map_or("null".to_string(), |s| s.to_string()),
            req.queued,
            req.written
                .or(req.ended)
                .map_or("null".to_string(), |t| format!("{t:.3}")),
            segs.join(","),
            parks.join(",")
        );
    }
    out.push_str("];\n");
    // The strips' step functions, `[t, value]` each, and where each is drawn.
    let series = series(trace);
    out.push_str("const STEPS = [");
    for steps in [&series.running, &series.waiting_on_io, &series.queue] {
        out.push('[');
        for (t, value) in steps {
            let _ = write!(out, "[{t:.3},{value}],");
        }
        out.push_str("],");
    }
    out.push_str("];\nconst STRIPS = [");
    for (top, height) in lay.strips {
        let _ = write!(out, "[{top},{height}],");
    }
    out.push_str("];\n");
    out.push_str(SCRIPT);
    out.push_str("</script>\n</body></html>\n");
    out
}

/// The hover readout: the pointer's row (a request) or lane and nearest run
/// (a worker), found by position, so the hit target is the whole row rather
/// than a mark a pixel wide.
const SCRIPT: &str = r#"
const svg = document.querySelector('#figure svg');
const tip = document.getElementById('tip');
const ms = t => ((t - T0) / 1000).toFixed(3);
const segs = [];
REQ.forEach((r, i) => r[6].forEach(s => segs.push([s[0], s[1], s[2], i])));
let band = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
band.setAttribute('class', 'hover');
band.style.display = 'none';
svg.appendChild(band);
function describe(i) {
  const [id, tenant, path, status, queued, done, ss, ps] = REQ[i];
  const lines = [];
  lines.push(['strong', tenant + ' #' + id + ' · ' + (status === null ? 'still running' : status === 504 ? '✕ 504 timeout' : status)]);
  lines.push(['', path]);
  lines.push(['', 'queued ' + ms(queued) + ' ms' + (done === null ? '' : ', answered ' + ms(done) + ' ms (' + ((done - queued) / 1000).toFixed(2) + ' ms)')]);
  const run = s => lines.push(['', (s[3] === 'run' ? 'ran' : s[3] === 'resume' ? 'resumed' : 'cancelled') + ' on worker ' + s[0] + ': ' + ms(s[1]) + ' to ' + ms(s[2]) + ' ms (' + Math.round(s[2] - s[1]) + ' µs)']);
  ss.forEach((s, k) => {
    run(s);
    const p = ps[k];
    if (!p) return;
    const [op, target, from, at, ready, resumed, to, by, due] = p;
    let text = 'parked on worker ' + from + ' at ' + op + ' ' + target + ' for ' + (((ready ?? T1) - at) / 1000).toFixed(1) + ' ms (' + by + (due !== null ? ', due after ' + due.toFixed(1) + ' ms' : '') + ')';
    if (resumed !== null) text += ', then waited ' + (resumed - (ready ?? resumed)).toFixed(0) + ' µs for worker ' + to + (to !== from ? ' (moved)' : '');
    lines.push(['', '  ' + text]);
  });
  return lines;
}
function show(evt, lines) {
  tip.textContent = '';
  lines.forEach(([tag, text], k) => {
    const el = document.createElement(tag || 'div');
    el.textContent = text;
    if (tag) { tip.appendChild(el); tip.appendChild(document.createElement('br')); } else tip.appendChild(el);
  });
  tip.style.display = 'block';
  const pad = 14, w = tip.offsetWidth, h = tip.offsetHeight;
  let x = evt.clientX + pad, y = evt.clientY + pad;
  if (x + w > window.innerWidth) x = evt.clientX - w - pad;
  if (y + h > window.innerHeight) y = evt.clientY - h - pad;
  tip.style.left = x + 'px'; tip.style.top = y + 'px';
}
function hide() { tip.style.display = 'none'; band.style.display = 'none'; }
svg.addEventListener('pointermove', evt => {
  const pt = svg.createSVGPoint(); pt.x = evt.clientX; pt.y = evt.clientY;
  const p = pt.matrixTransform(svg.getScreenCTM().inverse());
  const t = T0 + (p.x - LEFT) / PLOT * (T1 - T0);
  if (p.y >= RTOP && p.y < RTOP + ROW * REQ.length) {
    const i = Math.floor((p.y - RTOP) / ROW);
    band.setAttribute('x', LEFT); band.setAttribute('width', PLOT);
    band.setAttribute('y', RTOP + i * ROW); band.setAttribute('height', ROW);
    band.style.display = '';
    show(evt, describe(i));
    return;
  }
  const inStrip = STRIPS.findIndex(([top, h]) => p.y >= top - 4 && p.y < top + h + 4);
  if (inStrip >= 0 && t >= T0 && t <= T1) {
    const at = steps => { let v = 0; for (const [st, sv] of steps) { if (st > t) break; v = sv; } return v; };
    const [top, h] = STRIPS[inStrip];
    band.setAttribute('x', LEFT); band.setAttribute('width', PLOT);
    band.setAttribute('y', top); band.setAttribute('height', h);
    band.style.display = '';
    show(evt, [['strong', ms(t) + ' ms'],
      ['', at(STEPS[0]) + ' of ' + WORKERS + ' workers running'],
      ['', at(STEPS[1]) + ' parked runs waiting on I/O'],
      ['', at(STEPS[2]) + ' requests waiting for a worker']]);
    return;
  }
  if (p.y >= WTOP && p.y < WTOP + LANE * WORKERS) {
    const w = Math.floor((p.y - WTOP) / LANE);
    const slack = 6 / PLOT * (T1 - T0);
    let best = null, gap = Infinity;
    for (const s of segs) {
      if (s[0] !== w) continue;
      const d = t < s[1] ? s[1] - t : t > s[2] ? t - s[2] : 0;
      if (d < gap) { gap = d; best = s; }
    }
    if (best && gap <= slack) {
      band.setAttribute('x', LEFT); band.setAttribute('width', PLOT);
      band.setAttribute('y', WTOP + w * LANE); band.setAttribute('height', LANE - 2);
      band.style.display = '';
      const lines = describe(best[3]);
      lines[0] = ['strong', 'worker ' + w + ': ' + lines[0][1]];
      show(evt, lines);
      return;
    }
  }
  hide();
});
svg.addEventListener('pointerleave', hide);
"#;
