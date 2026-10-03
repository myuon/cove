//! The scheduler: one acceptor, a fixed pool of workers, one timer.
//!
//! ```text
//!   acceptor ──Connection──▶ ┌──────────┐ ◀──Resume── timer (the "upstream")
//!                            │ run queue │                ▲
//!                            └──────────┘                │ ParkedVm + due time
//!                     workers × N  ── Step::Parked ───────┘
//!                                  ── Step::Answered ──▶ response written
//! ```
//!
//! A worker takes a job off the queue and runs a tenant's isolate until it
//! answers or parks. A parked run is a [`ParkedVm`] — `Send`, a few kilobytes
//! — and it goes to the timer with the socket it owes a response to; no
//! thread waits on it. When its upstream "answers", the timer puts a resume
//! job on the same queue and whichever worker is free takes it. So the
//! number of requests in flight is bounded by memory and sockets, not by
//! threads: the pool is `--workers` threads however many requests are
//! waiting.
//!
//! None of this is the runtime's. ADR 0080 gives a run that can be parked
//! and resumed anywhere; when and where is this file's policy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cove_diag::render;
use cove_runtime::{Budget, OwnedVm, ParkedVm, RuntimeError, Step, Transfer, Value};

use crate::deploy::{deploy_all, request_value, DeployOptions, Deployed, State, Tenant};
use crate::hosts::{upstream_answer, UpstreamCall};
use crate::http::{read_request, Request, Response};
use crate::os;

/// Whether a request gets a fresh isolate or a resident one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolates {
    /// A fresh [`OwnedVm`] per request, dropped when it answers: nothing of
    /// one request can be seen by the next, by construction.
    PerRequest,
    /// Up to this many resident [`OwnedVm`]s per tenant, reused. Cove has no
    /// global state, so a reused run starts from the same program with a
    /// heap the pacing collector (ADR 0081) keeps small; what it saves is
    /// the heap's first chunk and the literals placed in it.
    Pooled(usize),
}

impl std::fmt::Display for Isolates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Isolates::PerRequest => write!(f, "a fresh isolate per request"),
            Isolates::Pooled(n) => write!(f, "up to {n} resident isolates per tenant"),
        }
    }
}

/// How to start a server.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Where to listen; `127.0.0.1:0` picks a free port.
    pub listen: String,
    /// How many worker threads run isolates.
    pub workers: usize,
    pub isolates: Isolates,
    pub deploy: DeployOptions,
}

/// A running server.
pub struct Server {
    /// Where it listens.
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

impl Server {
    /// Deploys the tenants, binds, and starts the threads. Returns once the
    /// server is accepting.
    pub fn start(options: ServerOptions) -> Result<Server, String> {
        let tenants = deploy_all(&options.deploy)?;
        let listener = TcpListener::bind(&options.listen)
            .map_err(|e| format!("cannot listen on `{}`: {e}", options.listen))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let (timer, timed) = mpsc::channel::<Timed>();
        let by_name = tenants
            .iter()
            .enumerate()
            .map(|(at, tenant)| (tenant.name.clone(), at))
            .collect();
        let shared = Arc::new(Shared {
            stats: Stats::new(&tenants),
            pools: tenants.iter().map(|_| Mutex::new(Vec::new())).collect(),
            tenants,
            by_name,
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            timer: Mutex::new(timer),
            options,
            started: Instant::now(),
            calls: AtomicU64::new(0),
        });
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("edge-timer".into())
                .spawn(move || shared.run_timer(timed))
                .map_err(|e| e.to_string())?;
        }
        for at in 0..shared.options.workers.max(1) {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("edge-worker-{at}"))
                .spawn(move || shared.run_worker())
                .map_err(|e| e.to_string())?;
        }
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("edge-acceptor".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        match stream {
                            Ok(stream) => shared.push(Job::Connection(stream, Instant::now())),
                            Err(error) => eprintln!("accept: {error}"),
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(Server { addr, shared })
    }

    /// Every tenant, deployed or refused.
    pub fn tenants(&self) -> &[Tenant] {
        &self.shared.tenants
    }

    /// The `/_stats` body.
    pub fn stats(&self) -> String {
        self.shared.render_stats()
    }
}

/// A request in flight: the socket it owes an answer to, and its tenant.
struct Flight {
    stream: TcpStream,
    tenant: usize,
    accepted: Instant,
}

enum Job {
    /// A connection the acceptor took, whose request is not read yet.
    Connection(TcpStream, Instant),
    /// A parked run whose host call has its answer. Boxed, because a
    /// `ParkedVm` is a whole machine and a connection is a socket.
    Resume(Box<Resume>),
}

struct Resume {
    parked: ParkedVm,
    answer: Result<Transfer, RuntimeError>,
    flight: Flight,
}

/// A parked run waiting on the simulated upstream.
struct Timed {
    due: Instant,
    service: String,
    latency: Duration,
    parked: ParkedVm,
    flight: Flight,
}

struct Shared {
    tenants: Vec<Tenant>,
    by_name: HashMap<String, usize>,
    /// Resident isolates, per tenant, under [`Isolates::Pooled`].
    pools: Vec<Mutex<Vec<OwnedVm>>>,
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    timer: Mutex<mpsc::Sender<Timed>>,
    stats: Stats,
    options: ServerOptions,
    started: Instant,
    /// Upstream calls made, which seeds each call's latency.
    calls: AtomicU64,
}

impl Shared {
    fn push(&self, job: Job) {
        let depth = {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back(job);
            queue.len() as i64
        };
        self.stats.queue_peak.fetch_max(depth, Ordering::Relaxed);
        self.ready.notify_one();
    }

    fn run_worker(&self) {
        loop {
            let job = {
                let mut queue = self.queue.lock().unwrap();
                loop {
                    if let Some(job) = queue.pop_front() {
                        break job;
                    }
                    queue = self.ready.wait(queue).unwrap();
                }
            };
            match job {
                Job::Connection(stream, accepted) => self.serve(stream, accepted),
                Job::Resume(resume) => {
                    let Resume {
                        parked,
                        answer,
                        flight,
                    } = *resume;
                    self.stats.parked.fetch_sub(1, Ordering::Relaxed);
                    let step = parked.resume(answer);
                    self.settle(step, flight);
                }
            }
        }
    }

    /// Reads a request and answers it, or starts the tenant's run.
    fn serve(&self, mut stream: TcpStream, accepted: Instant) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_nodelay(true);
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err(why) => {
                let _ = Response::text(400, format!("{why}\n")).send(&mut stream);
                return;
            }
        };
        let path = request.path.trim_start_matches('/');
        let (name, rest) = match path.split_once('/') {
            Some((name, rest)) => (name, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        let response = match name {
            "" => Response::text(200, self.index()),
            "_stats" => {
                let body = self.render_stats();
                if request.query.iter().any(|(k, _)| k == "reset") {
                    self.stats.reset();
                }
                Response {
                    status: 200,
                    content_type: "application/json".to_string(),
                    body,
                }
            }
            _ => match self.by_name.get(name) {
                None => Response::text(404, format!("no tenant named `{name}`\n")),
                Some(&at) => match &self.tenants[at].state {
                    State::Refused(why) => {
                        Response::text(503, format!("tenant `{name}` was not deployed: {why}\n"))
                    }
                    State::Deployed(deployed) => {
                        let flight = Flight {
                            stream,
                            tenant: at,
                            accepted,
                        };
                        self.start(deployed, at, &request, rest, flight);
                        return;
                    }
                },
            },
        };
        let _ = response.send(&mut stream);
    }

    /// Starts a tenant's run on this worker, and settles what it comes to.
    fn start(
        &self,
        deployed: &Deployed,
        at: usize,
        request: &Request,
        path: String,
        flight: Flight,
    ) {
        let vm = match self.options.isolates {
            Isolates::PerRequest => None,
            Isolates::Pooled(_) => self.pools[at].lock().unwrap().pop(),
        }
        .unwrap_or_else(|| deployed.isolate());
        let in_flight = self.stats.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.stats
            .in_flight_peak
            .fetch_max(in_flight, Ordering::Relaxed);
        let argument = request_value(&request.method, &path, &request.query, &request.body);
        let budget = Budget::new(self.tenants[at].limits.clone());
        let step =
            vm.invoke_within_parkable(budget, &deployed.module, &deployed.function, vec![argument]);
        self.settle(step, flight);
    }

    /// What a run came to: an answer to write, or a park to hand on.
    fn settle(&self, step: Step, mut flight: Flight) {
        match step {
            Step::Answered(vm, outcome) => {
                let tenant = &self.tenants[flight.tenant];
                let deployed = tenant.deployed().expect("only a deployed tenant runs");
                let response = match outcome {
                    Ok(value) => response_of(&value),
                    Err(error) => Err(render(&deployed.sources, &error.to_diagnostic())),
                };
                let failed = response.is_err();
                let response = response.unwrap_or_else(|why| Response::text(500, why));
                self.stats.heap_bytes.record(vm.heap_words() * 8);
                if let Isolates::Pooled(cap) = self.options.isolates {
                    let mut pool = self.pools[flight.tenant].lock().unwrap();
                    if pool.len() < cap {
                        pool.push(vm);
                    }
                }
                let _ = response.send(&mut flight.stream);
                self.stats
                    .answered(flight.tenant, failed, flight.accepted.elapsed());
            }
            Step::Parked(mut parked) => {
                let parked_now = self.stats.parked.fetch_add(1, Ordering::Relaxed) + 1;
                self.stats
                    .parked_peak
                    .fetch_max(parked_now, Ordering::Relaxed);
                self.stats.parks.fetch_add(1, Ordering::Relaxed);
                let request = parked.take_request();
                match request.map(|r| r.downcast::<UpstreamCall>()) {
                    Some(Ok(call)) => {
                        let seed = self.calls.fetch_add(1, Ordering::Relaxed);
                        let latency = self.options.deploy.latency.pick(seed);
                        let timed = Timed {
                            due: Instant::now() + latency,
                            service: call.service,
                            latency,
                            parked,
                            flight,
                        };
                        // The timer outlives every worker; a send cannot fail
                        // while the server runs.
                        let _ = self.timer.lock().unwrap().send(timed);
                    }
                    _ => {
                        let answer = Err(RuntimeError::new(
                            "the host parked with a request this server does not know",
                        ));
                        self.push(Job::Resume(Box::new(Resume {
                            parked,
                            answer,
                            flight,
                        })));
                    }
                }
            }
        }
    }

    /// The simulated upstream: every parked run waits here, in a heap
    /// ordered by when its answer is due, and nowhere else.
    fn run_timer(&self, timed: mpsc::Receiver<Timed>) {
        let mut due: BinaryHeap<Reverse<(Instant, u64)>> = BinaryHeap::new();
        let mut waiting: HashMap<u64, Timed> = HashMap::new();
        let mut next = 0u64;
        loop {
            let received = match due.peek() {
                None => timed.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(Reverse((at, _))) => {
                    timed.recv_timeout(at.saturating_duration_since(Instant::now()))
                }
            };
            match received {
                Ok(first) => {
                    for item in std::iter::once(first).chain(timed.try_iter()) {
                        due.push(Reverse((item.due, next)));
                        waiting.insert(next, item);
                        next += 1;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            let mut ready = Vec::new();
            while let Some(Reverse((at, key))) = due.peek().copied() {
                if at > now {
                    break;
                }
                due.pop();
                ready.push(waiting.remove(&key).expect("every key is waiting"));
            }
            if ready.is_empty() {
                continue;
            }
            let count = ready.len();
            {
                let mut queue = self.queue.lock().unwrap();
                for item in ready {
                    // The answer is built here, as a `Value` and then a
                    // `Transfer`, because the parked run is resumed on some
                    // other thread and a `Value` cannot go there.
                    let value = match upstream_answer(&item.service, item.latency) {
                        Ok(text) => Value::ok(Value::string(text)),
                        Err(message) => Value::err(Value::error(message)),
                    };
                    let answer = Transfer::of(&value).map_err(|unsafe_value| {
                        RuntimeError::new(format!("{} is not task-safe", unsafe_value.type_name))
                    });
                    queue.push_back(Job::Resume(Box::new(Resume {
                        parked: item.parked,
                        answer,
                        flight: item.flight,
                    })));
                }
                self.stats
                    .queue_peak
                    .fetch_max(queue.len() as i64, Ordering::Relaxed);
            }
            if count == 1 {
                self.ready.notify_one();
            } else {
                self.ready.notify_all();
            }
        }
    }

    fn index(&self) -> String {
        let mut out =
            String::from("cove-edge: tenants are Cove programs, one isolate per request\n\n");
        for tenant in &self.tenants {
            out.push_str(&format!("  /{}/  {}\n", tenant.name, tenant.describe()));
        }
        out.push_str("\n  /_stats  requests, parked runs, latency, memory\n");
        out
    }

    fn render_stats(&self) -> String {
        let stats = &self.stats;
        let (p50, p99, max, samples) = stats.latency_percentiles();
        let pooled: usize = self.pools.iter().map(|p| p.lock().unwrap().len()).sum();
        let in_flight = stats.in_flight.load(Ordering::Relaxed);
        let (heap_mean, heap_max) = stats.heap_bytes.summary();
        let mut tenants = BTreeMap::new();
        for (at, tenant) in self.tenants.iter().enumerate() {
            let state = match &tenant.state {
                State::Deployed(_) => "deployed".to_string(),
                State::Refused(_) => "refused".to_string(),
            };
            tenants.insert(
                tenant.name.clone(),
                format!(
                    "{{\"state\": \"{state}\", \"served\": {}, \"errors\": {}}}",
                    stats.per_tenant[at].0.load(Ordering::Relaxed),
                    stats.per_tenant[at].1.load(Ordering::Relaxed)
                ),
            );
        }
        let tenants: Vec<String> = tenants
            .into_iter()
            .map(|(name, body)| format!("    \"{name}\": {body}"))
            .collect();
        format!(
            "{{\n  \"uptime_s\": {:.1},\n  \"workers\": {},\n  \"isolates\": \"{}\",\n  \
             \"served\": {},\n  \"errors\": {},\n  \"in_flight\": {in_flight},\n  \
             \"in_flight_peak\": {},\n  \"parked\": {},\n  \"parked_peak\": {},\n  \
             \"parks\": {},\n  \"queue_peak\": {},\n  \"live_isolates\": {},\n  \
             \"pooled_isolates\": {pooled},\n  \"isolate_heap_bytes\": {{\"mean\": {heap_mean}, \"max\": {heap_max}}},\n  \
             \"latency_ms\": {{\"p50\": {:.2}, \"p99\": {:.2}, \"max\": {:.2}, \"samples\": {samples}}},\n  \
             \"rss_kib\": {},\n  \"peak_rss_kib\": {},\n  \"tenants\": {{\n{}\n  }}\n}}\n",
            self.started.elapsed().as_secs_f64(),
            self.options.workers,
            self.options.isolates,
            stats.served.load(Ordering::Relaxed),
            stats.errors.load(Ordering::Relaxed),
            stats.in_flight_peak.load(Ordering::Relaxed),
            stats.parked.load(Ordering::Relaxed),
            stats.parked_peak.load(Ordering::Relaxed),
            stats.parks.load(Ordering::Relaxed),
            stats.queue_peak.load(Ordering::Relaxed),
            in_flight + pooled as i64,
            p50.as_secs_f64() * 1e3,
            p99.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3,
            os::rss_kib(),
            os::peak_rss_kib(),
            tenants.join(",\n"),
        )
    }
}

/// An `edge.Response` value as the response to write, or why it is not one.
fn response_of(value: &Value) -> Result<Response, String> {
    let field = |name: &str| {
        value
            .field(name)
            .ok_or_else(|| format!("the handler answered {value}, not an `edge.Response`\n"))
    };
    let status = field("status")?.as_int().unwrap_or(500);
    Ok(Response {
        status: u16::try_from(status)
            .ok()
            .filter(|s| (100..600).contains(s))
            .unwrap_or(500),
        content_type: field("contentType")?
            .as_str()
            .unwrap_or("text/plain")
            .to_string(),
        body: field("body")?.as_str().unwrap_or_default().to_string(),
    })
}

// ------------------------------------------------------------------ stats

/// The server's counters. Everything is a relaxed atomic or a short lock;
/// nothing here is on a path that waits.
struct Stats {
    served: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicI64,
    in_flight_peak: AtomicI64,
    parked: AtomicI64,
    parked_peak: AtomicI64,
    parks: AtomicU64,
    queue_peak: AtomicI64,
    /// Served and failed, per tenant.
    per_tenant: Vec<(AtomicU64, AtomicU64)>,
    /// The last [`LATENCY_SAMPLES`] request latencies, in microseconds.
    latencies: Mutex<(Vec<u32>, usize)>,
    heap_bytes: Summary,
}

const LATENCY_SAMPLES: usize = 1 << 16;

impl Stats {
    fn new(tenants: &[Tenant]) -> Stats {
        Stats {
            served: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            in_flight: AtomicI64::new(0),
            in_flight_peak: AtomicI64::new(0),
            parked: AtomicI64::new(0),
            parked_peak: AtomicI64::new(0),
            parks: AtomicU64::new(0),
            queue_peak: AtomicI64::new(0),
            per_tenant: tenants
                .iter()
                .map(|_| (AtomicU64::new(0), AtomicU64::new(0)))
                .collect(),
            latencies: Mutex::new((Vec::with_capacity(LATENCY_SAMPLES), 0)),
            heap_bytes: Summary::default(),
        }
    }

    fn answered(&self, tenant: usize, failed: bool, latency: Duration) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.served.fetch_add(1, Ordering::Relaxed);
        self.per_tenant[tenant].0.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
            self.per_tenant[tenant].1.fetch_add(1, Ordering::Relaxed);
        }
        let micros = latency.as_micros().min(u32::MAX as u128) as u32;
        let mut guard = self.latencies.lock().unwrap();
        let (samples, next) = &mut *guard;
        if samples.len() < LATENCY_SAMPLES {
            samples.push(micros);
        } else {
            samples[*next] = micros;
        }
        *next = (*next + 1) % LATENCY_SAMPLES;
    }

    fn latency_percentiles(&self) -> (Duration, Duration, Duration, usize) {
        let mut samples = self.latencies.lock().unwrap().0.clone();
        if samples.is_empty() {
            return (Duration::ZERO, Duration::ZERO, Duration::ZERO, 0);
        }
        samples.sort_unstable();
        let at = |p: f64| {
            Duration::from_micros(samples[((samples.len() - 1) as f64 * p).round() as usize] as u64)
        };
        (at(0.5), at(0.99), at(1.0), samples.len())
    }

    /// Forgets the peaks and the latency samples, so that a load run reads
    /// its own.
    fn reset(&self) {
        self.in_flight_peak
            .store(self.in_flight.load(Ordering::Relaxed), Ordering::Relaxed);
        self.parked_peak
            .store(self.parked.load(Ordering::Relaxed), Ordering::Relaxed);
        self.queue_peak.store(0, Ordering::Relaxed);
        *self.latencies.lock().unwrap() = (Vec::with_capacity(LATENCY_SAMPLES), 0);
    }
}

/// A running mean and maximum.
#[derive(Default)]
struct Summary {
    total: AtomicU64,
    count: AtomicU64,
    max: AtomicU64,
}

impl Summary {
    fn record(&self, value: u64) {
        self.total.fetch_add(value, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max.fetch_max(value, Ordering::Relaxed);
    }

    fn summary(&self) -> (u64, u64) {
        let count = self.count.load(Ordering::Relaxed).max(1);
        (
            self.total.load(Ordering::Relaxed) / count,
            self.max.load(Ordering::Relaxed),
        )
    }
}
