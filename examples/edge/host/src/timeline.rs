//! The request timeline: an opt-in recorder of where each request ran, where
//! it parked, and where it resumed (`cove-edge --timeline PATH`).
//!
//! Every event carries a timestamp in microseconds since the server started
//! (with nanosecond fraction, so events a microsecond apart still sort), the
//! request's id, and what happened. The recorder must not serialise the
//! workers it watches, so it has one buffer per thread that writes to it —
//! a buffer per worker and one for the parking lot — each behind a mutex
//! only its own thread takes, except while `/_timeline` copies them out.
//! Nothing else is shared: no global sequence number, no channel.
//!
//! The dump is JSON with one event per line, which
//! [`crate::picture`] reads back to draw it.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

/// Turns the recorder on: `file`, if given, is rewritten on every dump.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub file: Option<PathBuf>,
}

/// Who answered a parked run's host call — or did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The parking lot's clock: a simulated `upstream.get` came due.
    Timer,
    /// A fetch-pool thread finished a real `upstream.fetch`.
    Fetcher,
    /// The run's deadline came first; it will be cancelled.
    Deadline,
    /// Answered at once with an error, without waiting.
    Host,
}

impl By {
    fn name(self) -> &'static str {
        match self {
            By::Timer => "timer",
            By::Fetcher => "fetcher",
            By::Deadline => "deadline",
            By::Host => "host",
        }
    }
}

/// What happened to a request.
#[derive(Clone, Debug)]
pub enum What {
    /// Its connection was accepted (the first request on a connection only).
    Accepted,
    /// It became readable and went on the run queue.
    Queued,
    /// A worker started its run.
    RunStart {
        worker: usize,
        tenant: String,
        path: String,
    },
    /// Its run parked at a host call on this worker.
    Park {
        worker: usize,
        op: &'static str,
        target: String,
        /// A simulated call's chosen latency: when its answer is due.
        latency: Option<std::time::Duration>,
    },
    /// The parking lot put its resume on the run queue.
    AnswerReady { by: By },
    /// A worker resumed it with its answer.
    Resume { worker: usize },
    /// A worker cancelled it, its deadline having passed while it was parked.
    Cancel { worker: usize },
    /// Its run answered, on this worker.
    RunEnd {
        worker: usize,
        status: u16,
        fuel: u64,
        host_calls: u64,
        heap_bytes: u64,
    },
    /// Its response was written to the socket.
    Written { worker: usize },
}

/// One event.
#[derive(Clone, Debug)]
pub struct Event {
    /// Nanoseconds since the server started.
    pub at_ns: u64,
    pub request: u64,
    pub what: What,
}

/// The recorder: a buffer per writing thread.
pub struct Recorder {
    started: Instant,
    workers: usize,
    /// `workers` buffers, one per worker, then the parking lot's.
    shards: Vec<Mutex<Vec<Event>>>,
    pub recording: Recording,
}

impl Recorder {
    pub fn new(workers: usize, started: Instant, recording: Recording) -> Recorder {
        Recorder {
            started,
            workers,
            shards: (0..=workers)
                .map(|_| Mutex::new(Vec::with_capacity(4096)))
                .collect(),
            recording,
        }
    }

    /// The parking lot's buffer.
    pub fn lot(&self) -> usize {
        self.workers
    }

    /// Records `what` for `request` at `at`, into `shard` (a worker's index,
    /// or [`Recorder::lot`]). Only that shard's thread calls this with it.
    pub fn record(&self, shard: usize, at: Instant, request: u64, what: What) {
        let at_ns = at.saturating_duration_since(self.started).as_nanos() as u64;
        self.shards[shard].lock().unwrap().push(Event {
            at_ns,
            request,
            what,
        });
    }

    /// Every event so far, in time order.
    pub fn events(&self) -> Vec<Event> {
        let mut all = Vec::new();
        for shard in &self.shards {
            all.extend(shard.lock().unwrap().iter().cloned());
        }
        all.sort_by_key(|event| event.at_ns);
        all
    }

    /// Forgets every event, so that a load run records only its own.
    pub fn reset(&self) {
        for shard in &self.shards {
            shard.lock().unwrap().clear();
        }
    }

    /// The dump: a header, then one event per line.
    pub fn dump(&self, tenants: &[String]) -> String {
        render(self.workers, tenants, &self.events())
    }
}

/// [`Recorder::dump`]'s format, from its parts.
pub fn render(workers: usize, tenants: &[String], events: &[Event]) -> String {
    let mut out = String::with_capacity(64 + events.len() * 96);
    let names: Vec<String> = tenants.iter().map(|t| quote(t)).collect();
    let _ = writeln!(
        out,
        "{{\"format\": \"cove-edge-timeline\", \"version\": 1, \"workers\": {workers}, \
         \"tenants\": [{}],\n\"events\": [",
        names.join(", ")
    );
    for (at, event) in events.iter().enumerate() {
        let t = event.at_ns as f64 / 1e3;
        let _ = write!(out, "{{\"t\": {t:.3}, \"req\": {}, \"ev\": ", event.request);
        let _ = match &event.what {
            What::Accepted => write!(out, "\"accepted\""),
            What::Queued => write!(out, "\"queued\""),
            What::RunStart {
                worker,
                tenant,
                path,
            } => write!(
                out,
                "\"run_start\", \"worker\": {worker}, \"tenant\": {}, \"path\": {}",
                quote(tenant),
                quote(path)
            ),
            What::Park {
                worker,
                op,
                target,
                latency,
            } => {
                let _ = write!(
                    out,
                    "\"park\", \"worker\": {worker}, \"op\": {}, \"target\": {}",
                    quote(op),
                    quote(target)
                );
                match latency {
                    Some(latency) => {
                        write!(out, ", \"due_ms\": {:.3}", latency.as_secs_f64() * 1e3)
                    }
                    None => Ok(()),
                }
            }
            What::AnswerReady { by } => {
                write!(out, "\"answer_ready\", \"by\": \"{}\"", by.name())
            }
            What::Resume { worker } => write!(out, "\"resume\", \"worker\": {worker}"),
            What::Cancel { worker } => write!(out, "\"cancel\", \"worker\": {worker}"),
            What::RunEnd {
                worker,
                status,
                fuel,
                host_calls,
                heap_bytes,
            } => write!(
                out,
                "\"run_end\", \"worker\": {worker}, \"status\": {status}, \"fuel\": {fuel}, \
                 \"host_calls\": {host_calls}, \"heap_bytes\": {heap_bytes}"
            ),
            What::Written { worker } => write!(out, "\"response_written\", \"worker\": {worker}"),
        };
        out.push('}');
        if at + 1 < events.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]}\n");
    out
}

/// A JSON string literal.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
