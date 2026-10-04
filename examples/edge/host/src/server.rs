//! The scheduler: one acceptor, one idle thread, a fixed pool of workers,
//! one parking lot, and a small pool of fetch threads.
//!
//! ```text
//!   acceptor ──▶ idle (poll) ──readable──▶ ┌──────────┐ ◀──Resume── lot ◀──answer── fetch pool × M
//!                    ▲                     │ run queue │              ▲                 ▲
//!                    │ kept alive          └──────────┘              │ ParkedVm        │ url
//!                    │              workers × N  ── Step::Parked ─────┴─────────────────┘
//!                    └──────────────────────── ── Step::Answered ──▶ response written
//! ```
//!
//! A connection waits for its request — the first, and every one after it
//! on a kept-alive connection — in [`crate::idle`], where one thread polls
//! every idle socket; a worker sees a connection only once it has something
//! to read.
//!
//! A worker takes a job off the queue and runs a tenant's isolate until it
//! answers or parks. A parked run is a [`ParkedVm`] — `Send`, a few kilobytes
//! — and it goes to the parking lot (the "timer") with the connection it
//! owes a response to; no thread waits on it. `upstream.get` is simulated,
//! and the lot answers it itself when its latency is up; `upstream.fetch` is
//! a real HTTP request, which a fetch-pool thread performs and answers to the
//! lot under the run's id. When its upstream answers, the lot puts a resume
//! job on the same queue and whichever worker is free takes it. So the
//! number of requests in flight is bounded by memory and sockets, not by
//! threads: the pool is `--workers` threads however many requests are
//! waiting.
//!
//! The timer also holds each parked run's deadline. A run's deadline is
//! wall-clock and keeps running while it is parked, and the runtime does not
//! wake a parked run by itself (ADR 0082), so the timer wakes at the earlier
//! of the answer's due time and the deadline, and a run whose deadline came
//! first is cancelled — `ParkedVm::cancel` — and answered 504.
//!
//! What the cancelled run was waiting on is withdrawn with it: a fetch is
//! aborted in the pool ([`Fetcher::abort`]), which shuts its socket so the
//! upstream sees the request abandoned rather than read to the end. ADR 0082
//! left telling a host this to the embedder, and no runtime API was needed
//! for it: the embedder took the request out of the parked run with
//! `ParkedVm::take_request` and handed it to its host itself, so it already
//! holds the handle — here the id — that the host needs to stop. (An
//! embedder that leaves the request in the run sees it dropped by
//! `ParkedVm::cancel`, so a host can observe that too, by a `Drop` on its
//! request type.)
//!
//! None of this is the runtime's. ADR 0080 gives a run that can be parked
//! and resumed anywhere; when and where is this file's policy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cove_diag::render;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{Budget, OwnedVm, ParkedVm, RuntimeError, Step, Transfer, Value};

use crate::deploy::{deploy_all, request_value, DeployOptions, Deployed, State, Tenant};
use crate::fetch::{Fetched, Fetcher};
use crate::hosts::{fetch_answer, result_value, upstream_answer, upstream_latency, UpstreamCall};
use crate::http::{holds_a_head, read_request, Request, Response};
use crate::idle::{Conn, Idle};
use crate::os;
use crate::timeline::{By, Recorder, Recording, What};

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
    pub keep_alive: KeepAlive,
    /// How many threads perform `upstream.fetch`es.
    pub fetchers: usize,
    /// Record every request's timeline (`--timeline`), dumped by
    /// `GET /_timeline`.
    pub timeline: Option<Recording>,
}

/// Whether, and for how long, a connection is kept open between requests.
#[derive(Clone, Copy, Debug)]
pub struct KeepAlive {
    /// Off: every response says `Connection: close`, whatever the client
    /// asked for.
    pub enabled: bool,
    /// How long a connection may wait for its next request — or its first —
    /// before the server closes it.
    pub idle: Duration,
    /// How many requests one connection may make; the last is answered with
    /// `Connection: close`.
    pub max_requests: u32,
}

impl Default for KeepAlive {
    fn default() -> KeepAlive {
        KeepAlive {
            enabled: true,
            idle: Duration::from_secs(5),
            max_requests: 1000,
        }
    }
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
        let (timer, timed) = mpsc::channel::<Lot>();
        let fetcher = {
            let lot = timer.clone();
            Fetcher::start(options.fetchers, move |id, fetched| {
                // The lot outlives the pool; a send cannot fail while the
                // server runs.
                let _ = lot.send(Lot::Fetched(id, fetched));
            })
        };
        let by_name = tenants
            .iter()
            .enumerate()
            .map(|(at, tenant)| (tenant.name.clone(), at))
            .collect();
        let started = Instant::now();
        let timeline = options
            .timeline
            .clone()
            .map(|recording| Recorder::new(options.workers.max(1), started, recording));
        let shared = Arc::new(Shared {
            stats: Stats::new(&tenants),
            timeline,
            requests: AtomicU64::new(0),
            pools: tenants.iter().map(|_| Mutex::new(Vec::new())).collect(),
            tenants,
            by_name,
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            timer: Mutex::new(timer),
            fetcher,
            idle: OnceLock::new(),
            options,
            started,
            calls: AtomicU64::new(0),
        });
        {
            let ready = Arc::clone(&shared);
            let idle = Idle::start(shared.options.keep_alive.idle, move |conns| {
                ready.push_connections(conns)
            })
            .map_err(|e| e.to_string())?;
            let _ = shared.idle.set(idle);
        }
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("edge-timer".into())
                .spawn(move || shared.run_lot(timed))
                .map_err(|e| e.to_string())?;
        }
        for at in 0..shared.options.workers.max(1) {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("edge-worker-{at}"))
                .spawn(move || shared.run_worker(at))
                .map_err(|e| e.to_string())?;
        }
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("edge-acceptor".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        match stream {
                            Ok(stream) => {
                                shared.stats.connections.fetch_add(1, Ordering::Relaxed);
                                shared.idle().park(Conn::new(stream));
                            }
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

    /// The request timeline recorded so far, if recording is on — what
    /// `GET /_timeline` answers.
    pub fn timeline(&self) -> Option<String> {
        self.shared.dump_timeline()
    }
}

/// A request in flight: the connection it owes an answer to, and its tenant.
struct Flight {
    /// The request's id, which the timeline records it under.
    id: u64,
    conn: Conn,
    tenant: usize,
    accepted: Instant,
    /// Whether the connection stays open after the answer.
    keep_alive: bool,
}

enum Job {
    /// A connection with a request to read — new, or back from idle — and
    /// when it became ready, which is where the request's latency starts.
    Connection(Conn, Instant),
    /// A parked run whose host call has its answer. Boxed, because a
    /// `ParkedVm` is a whole machine and a connection is a socket.
    Resume(Box<Resume>),
}

struct Resume {
    parked: ParkedVm,
    /// The host call's answer, or `None` for a run whose deadline came
    /// before its answer did, which is cancelled instead.
    answer: Option<Result<Transfer, RuntimeError>>,
    flight: Flight,
}

/// What a parked run is waiting for.
enum Wait {
    /// `upstream.get`: a simulated service, which the lot answers itself
    /// when `due`.
    Service {
        service: String,
        latency: Duration,
        due: Instant,
    },
    /// `upstream.fetch`: answered by the fetch pool, under the run's id.
    Fetch { url: String },
}

/// A parked run, held by the parking lot until its answer or its deadline.
struct Timed {
    /// The run's key in the lot, and its fetch's in the pool.
    id: u64,
    /// When the run's deadline passes, read off the run as it parked.
    deadline: Option<Instant>,
    wait: Wait,
    parked: ParkedVm,
    flight: Flight,
}

impl Timed {
    /// When the lot has to look at this run again without being told: the
    /// simulated answer's due time or the deadline, whichever is first. A
    /// fetch's answer comes by message, so a fetch is woken only by its
    /// deadline, and one with no deadline is never woken by the clock.
    fn wake(&self) -> Option<Instant> {
        match (&self.wait, self.deadline) {
            (Wait::Service { due, .. }, Some(deadline)) => Some(deadline.min(*due)),
            (Wait::Service { due, .. }, None) => Some(*due),
            (Wait::Fetch { .. }, deadline) => deadline,
        }
    }

    /// The resume job for this run: with `answer`, or cancelled if `None`.
    fn resume(self, answer: Option<Result<Transfer, RuntimeError>>) -> Job {
        Job::Resume(Box::new(Resume {
            parked: self.parked,
            answer,
            flight: self.flight,
        }))
    }
}

/// What the parking lot is told.
enum Lot {
    /// A run parked at an upstream call.
    Park(Box<Timed>),
    /// The fetch pool's answer for the fetch with this id.
    Fetched(u64, Fetched),
}

/// An answer as the `Transfer` a parked run is resumed with. Built as a
/// `Value` and then a `Transfer`, because the run is resumed on some other
/// thread and a `Value` cannot go there.
fn transfer(result: Result<String, String>) -> Result<Transfer, RuntimeError> {
    Transfer::of(&result_value(result)).map_err(|unsafe_value| {
        RuntimeError::new(format!("{} is not task-safe", unsafe_value.type_name))
    })
}

struct Shared {
    tenants: Vec<Tenant>,
    by_name: HashMap<String, usize>,
    /// Resident isolates, per tenant, under [`Isolates::Pooled`].
    pools: Vec<Mutex<Vec<OwnedVm>>>,
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    timer: Mutex<mpsc::Sender<Lot>>,
    /// The threads that perform `upstream.fetch`.
    fetcher: Fetcher,
    /// Where connections wait between requests; set once, at start.
    idle: OnceLock<Idle>,
    stats: Stats,
    options: ServerOptions,
    started: Instant,
    /// The request timeline, when `--timeline` is on.
    timeline: Option<Recorder>,
    /// Tenant requests started: each one's id in the timeline.
    requests: AtomicU64,
    /// Upstream calls made: each one's id in the lot, and the seed of a
    /// simulated call's latency.
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

    fn idle(&self) -> &Idle {
        self.idle.get().expect("set before the acceptor starts")
    }

    /// Connections the idle thread found readable, onto the run queue.
    fn push_connections(&self, conns: Vec<Conn>) {
        let now = Instant::now();
        let count = conns.len();
        let depth = {
            let mut queue = self.queue.lock().unwrap();
            queue.extend(conns.into_iter().map(|conn| Job::Connection(conn, now)));
            queue.len() as i64
        };
        self.stats.queue_peak.fetch_max(depth, Ordering::Relaxed);
        if count == 1 {
            self.ready.notify_one();
        } else {
            self.ready.notify_all();
        }
    }

    /// Writes a response and decides what becomes of the connection: closed,
    /// back on the queue if a pipelined request is already buffered, or
    /// parked with the idle thread until the next one arrives.
    ///
    /// `written` is the worker and request id the timeline records the
    /// write under: noted before the connection is closed or handed on, so
    /// that a client which has read its answer finds it recorded.
    fn finish(
        &self,
        mut conn: Conn,
        response: &Response,
        keep_alive: bool,
        written: Option<(usize, u64)>,
    ) {
        let sent = response.send(&mut conn.stream, keep_alive);
        if let Some((worker, id)) = written {
            self.note(worker, id, || What::Written { worker });
        }
        if sent.is_err() || !keep_alive {
            return;
        }
        if holds_a_head(&conn.buffer) {
            self.push(Job::Connection(conn, Instant::now()));
        } else {
            self.idle().park(conn);
        }
    }

    /// Records `what` for `request` now, if the timeline is on.
    fn note(&self, shard: usize, request: u64, what: impl FnOnce() -> What) {
        if let Some(timeline) = &self.timeline {
            timeline.record(shard, Instant::now(), request, what());
        }
    }

    /// The `/_timeline` body: the dump, also written to `--timeline`'s file.
    fn dump_timeline(&self) -> Option<String> {
        let timeline = self.timeline.as_ref()?;
        let names: Vec<String> = self.tenants.iter().map(|t| t.name.clone()).collect();
        let dump = timeline.dump(&names);
        if let Some(file) = &timeline.recording.file {
            if let Err(error) = std::fs::write(file, &dump) {
                eprintln!("timeline: cannot write {}: {error}", file.display());
            }
        }
        Some(dump)
    }

    fn run_worker(&self, worker: usize) {
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
                Job::Connection(conn, accepted) => self.serve(conn, accepted, worker),
                Job::Resume(resume) => {
                    let Resume {
                        parked,
                        answer,
                        flight,
                    } = *resume;
                    self.stats.parked.fetch_sub(1, Ordering::Relaxed);
                    let step = match answer {
                        Some(answer) => {
                            self.note(worker, flight.id, || What::Resume { worker });
                            parked.resume(answer)
                        }
                        None => {
                            self.note(worker, flight.id, || What::Cancel { worker });
                            let (vm, error) = parked.cancel();
                            Step::Answered(vm, Err(error))
                        }
                    };
                    self.settle(step, flight, worker);
                }
            }
        }
    }

    /// Reads a request and answers it, or starts the tenant's run.
    ///
    /// The connection is readable when it gets here, so the read waits only
    /// for the rest of a request that has started arriving — at most five
    /// seconds, for a client that sends half a head and stops.
    fn serve(&self, mut conn: Conn, accepted: Instant, worker: usize) {
        let _ = conn.stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.stream.set_nodelay(true);
        let request = match read_request(&mut conn.stream, &mut conn.buffer) {
            Ok(Some(request)) => request,
            // Closed by the client between requests: nothing is owed.
            Ok(None) => return,
            Err(why) => {
                let _ = Response::text(400, format!("{why}\n")).send(&mut conn.stream, false);
                return;
            }
        };
        conn.served += 1;
        let options = self.options.keep_alive;
        let keep_alive =
            options.enabled && request.keep_alive && conn.served < options.max_requests;
        if conn.served > 1 {
            self.stats.reused.fetch_add(1, Ordering::Relaxed);
        }
        let path = request.path.trim_start_matches('/');
        let (name, rest) = match path.split_once('/') {
            Some((name, rest)) => (name, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        let response = match name {
            "" => Response::text(200, self.index()),
            "_timeline" => match request.query.iter().any(|(k, _)| k == "reset") {
                _ if self.timeline.is_none() => Response::text(
                    409,
                    "the timeline is not being recorded; start the server with --timeline PATH\n",
                ),
                true => {
                    if let Some(timeline) = &self.timeline {
                        timeline.reset();
                    }
                    Response::text(200, "timeline reset\n")
                }
                false => Response {
                    status: 200,
                    content_type: "application/json".to_string(),
                    body: self.dump_timeline().unwrap_or_default(),
                },
            },
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
                        let id = self.requests.fetch_add(1, Ordering::Relaxed);
                        if let Some(timeline) = &self.timeline {
                            if conn.served == 1 {
                                timeline.record(worker, conn.opened, id, What::Accepted);
                            }
                            timeline.record(worker, accepted, id, What::Queued);
                            timeline.record(
                                worker,
                                Instant::now(),
                                id,
                                What::RunStart {
                                    worker,
                                    tenant: name.to_string(),
                                    path: with_query(&rest, &request.query),
                                },
                            );
                        }
                        let flight = Flight {
                            id,
                            conn,
                            tenant: at,
                            accepted,
                            keep_alive,
                        };
                        self.start(deployed, at, &request, rest, flight, worker);
                        return;
                    }
                },
            },
        };
        self.finish(conn, &response, keep_alive, None);
    }

    /// Starts a tenant's run on this worker, and settles what it comes to.
    fn start(
        &self,
        deployed: &Deployed,
        at: usize,
        request: &Request,
        path: String,
        flight: Flight,
        worker: usize,
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
        self.settle(step, flight, worker);
    }

    /// What a run came to: an answer to write, or a park to hand on.
    fn settle(&self, step: Step, flight: Flight, worker: usize) {
        match step {
            Step::Answered(vm, outcome) => {
                let tenant = &self.tenants[flight.tenant];
                let deployed = tenant.deployed().expect("only a deployed tenant runs");
                // A run stopped by its deadline is the gateway's timeout, not
                // the tenant's bug: 504, with the runtime's own diagnostic.
                let status = match &outcome {
                    Err(error) if error.outcome == RunOutcome::Deadline => {
                        self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                        504
                    }
                    _ => 500,
                };
                let response = match outcome {
                    Ok(value) => response_of(&value),
                    Err(error) => Err(render(&deployed.sources, &error.to_diagnostic())),
                };
                let failed = response.is_err();
                let response = response.unwrap_or_else(|why| Response::text(status, why));
                let heap_bytes = vm.heap_words() * 8;
                self.stats.heap_bytes.record(heap_bytes);
                self.note(worker, flight.id, || What::RunEnd {
                    worker,
                    status: response.status,
                    fuel: vm.meter().fuel_spent(),
                    host_calls: vm.meter().host_calls(),
                    heap_bytes,
                });
                if let Isolates::Pooled(cap) = self.options.isolates {
                    let mut pool = self.pools[flight.tenant].lock().unwrap();
                    if pool.len() < cap {
                        pool.push(vm);
                    }
                }
                self.stats
                    .answered(flight.tenant, failed, flight.accepted.elapsed());
                self.finish(
                    flight.conn,
                    &response,
                    flight.keep_alive,
                    Some((worker, flight.id)),
                );
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
                        let id = self.calls.fetch_add(1, Ordering::Relaxed);
                        let now = Instant::now();
                        let (wait, fetch) = match *call {
                            UpstreamCall::Service(service) => {
                                let latency = upstream_latency(&service)
                                    .unwrap_or_else(|| self.options.deploy.latency.pick(id));
                                let due = now + latency;
                                let wait = Wait::Service {
                                    service,
                                    latency,
                                    due,
                                };
                                (wait, None)
                            }
                            UpstreamCall::Fetch(url) => {
                                let wait = Wait::Fetch {
                                    url: url.to_string(),
                                };
                                (wait, Some(url))
                            }
                        };
                        if let Some(timeline) = &self.timeline {
                            let what = match &wait {
                                Wait::Service {
                                    service, latency, ..
                                } => What::Park {
                                    worker,
                                    op: "upstream.get",
                                    target: service.clone(),
                                    latency: Some(*latency),
                                },
                                Wait::Fetch { url } => What::Park {
                                    worker,
                                    op: "upstream.fetch",
                                    target: url.clone(),
                                    latency: None,
                                },
                            };
                            timeline.record(worker, now, flight.id, what);
                        }
                        let timed = Timed {
                            id,
                            deadline: parked.time_left().map(|left| now + left),
                            wait,
                            parked,
                            flight,
                        };
                        // Parked first and fetched second: the pool answers
                        // down the same channel, so the answer cannot reach
                        // the lot before the run it answers. The lot outlives
                        // every worker; a send cannot fail while the server
                        // runs.
                        let _ = self.timer.lock().unwrap().send(Lot::Park(Box::new(timed)));
                        if let Some(url) = fetch {
                            self.fetcher.submit(id, url);
                        }
                    }
                    _ => {
                        self.note(worker, flight.id, || What::Park {
                            worker,
                            op: "unknown",
                            target: String::new(),
                            latency: None,
                        });
                        self.note(worker, flight.id, || What::AnswerReady { by: By::Host });
                        let answer = Err(RuntimeError::new(
                            "the host parked with a request this server does not know",
                        ));
                        self.push(Job::Resume(Box::new(Resume {
                            parked,
                            answer: Some(answer),
                            flight,
                        })));
                    }
                }
            }
        }
    }

    /// The parking lot: every parked run waits here, keyed by its id, and
    /// nowhere else.
    ///
    /// A heap ordered by when each run must be looked at — a simulated
    /// answer's due time, or a deadline — drives the clock; a fetch's answer
    /// arrives as a message. Whichever comes first settles the run: its
    /// answer resumes it, its deadline cancels it, and what comes second
    /// finds nothing under the id and is dropped.
    fn run_lot(&self, lot: mpsc::Receiver<Lot>) {
        let mut due: BinaryHeap<Reverse<(Instant, u64)>> = BinaryHeap::new();
        let mut waiting: HashMap<u64, Timed> = HashMap::new();
        let lot_shard = self.timeline.as_ref().map_or(0, Recorder::lot);
        loop {
            let received = match due.peek() {
                None => lot.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(Reverse((at, _))) => {
                    lot.recv_timeout(at.saturating_duration_since(Instant::now()))
                }
            };
            let mut ready = Vec::new();
            match received {
                Ok(first) => {
                    for message in std::iter::once(first).chain(lot.try_iter()) {
                        match message {
                            Lot::Park(timed) => {
                                if let Some(wake) = timed.wake() {
                                    due.push(Reverse((wake, timed.id)));
                                }
                                waiting.insert(timed.id, *timed);
                            }
                            Lot::Fetched(id, fetched) => {
                                // Not waiting: its deadline came first, and
                                // the run was cancelled without it.
                                let Some(timed) = waiting.remove(&id) else {
                                    continue;
                                };
                                let Wait::Fetch { url } = &timed.wait else {
                                    unreachable!("only a fetch is answered by the pool");
                                };
                                let answer = transfer(fetch_answer(url, fetched));
                                self.note(lot_shard, timed.flight.id, || What::AnswerReady {
                                    by: By::Fetcher,
                                });
                                ready.push(timed.resume(Some(answer)));
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            while let Some(Reverse((at, id))) = due.peek().copied() {
                if at > now {
                    break;
                }
                due.pop();
                // Not waiting: a fetch already answered.
                let Some(timed) = waiting.remove(&id) else {
                    continue;
                };
                // Woken by its deadline rather than its answer: the run is
                // cancelled on a worker, and the answer never comes. The work
                // it was waiting on is no longer wanted, so it is withdrawn
                // too — a simulated call's entry is already gone from the
                // heap, and a fetch is aborted in the pool, queued or on the
                // wire. The id is all that takes: this server took the
                // request out of the parked run (`ParkedVm::take_request`)
                // and handed it to the pool itself, so it already holds what
                // it needs to tell its host to stop.
                if timed.deadline.is_some_and(|deadline| deadline <= now) {
                    if let Wait::Fetch { .. } = timed.wait {
                        self.fetcher.abort(timed.id);
                    }
                    self.note(lot_shard, timed.flight.id, || What::AnswerReady {
                        by: By::Deadline,
                    });
                    ready.push(timed.resume(None));
                    continue;
                }
                let answer = match &timed.wait {
                    Wait::Service {
                        service, latency, ..
                    } => transfer(upstream_answer(service, *latency)),
                    Wait::Fetch { .. } => unreachable!("a fetch wakes only at its deadline"),
                };
                self.note(lot_shard, timed.flight.id, || What::AnswerReady {
                    by: By::Timer,
                });
                ready.push(timed.resume(Some(answer)));
            }
            if ready.is_empty() {
                continue;
            }
            let count = ready.len();
            {
                let mut queue = self.queue.lock().unwrap();
                queue.extend(ready);
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
        out.push_str("  /_timeline  every request's runs, parks and resumes (with --timeline)\n");
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
             \"parks\": {},\n  \"timeouts\": {},\n  \"queue_peak\": {},\n  \
             \"connections\": {},\n  \"keep_alive_reuses\": {},\n  \"idle_connections\": {},\n  \
             \"idle_expired\": {},\n  \"fetches\": {},\n  \"fetches_aborted\": {},\n  \
             \"live_isolates\": {},\n  \
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
            stats.timeouts.load(Ordering::Relaxed),
            stats.queue_peak.load(Ordering::Relaxed),
            stats.connections.load(Ordering::Relaxed),
            stats.reused.load(Ordering::Relaxed),
            self.idle().stats.idle.load(Ordering::Relaxed),
            self.idle().stats.expired.load(Ordering::Relaxed),
            self.fetcher.stats().started.load(Ordering::Relaxed),
            self.fetcher.stats().aborted_queued.load(Ordering::Relaxed)
                + self.fetcher.stats().aborted_running.load(Ordering::Relaxed),
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

/// A tenant's path and its query, as the timeline shows it.
fn with_query(path: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return path.to_string();
    }
    let pairs: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{path}?{}", pairs.join("&"))
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
    /// Requests whose run was stopped by its deadline and answered 504.
    timeouts: AtomicU64,
    queue_peak: AtomicI64,
    /// Connections accepted.
    connections: AtomicU64,
    /// Requests that arrived on a connection an earlier request had opened.
    reused: AtomicU64,
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
            timeouts: AtomicU64::new(0),
            queue_peak: AtomicI64::new(0),
            connections: AtomicU64::new(0),
            reused: AtomicU64::new(0),
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
