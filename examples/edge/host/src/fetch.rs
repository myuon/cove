//! The outbound I/O pool: a few threads that perform `upstream.fetch`es
//! while the runs that asked for them are parked.
//!
//! A fetch is a blocking HTTP/1.1 `GET` ([`crate::http::fetch`]) on one of
//! `--fetchers` threads, and its answer goes to whatever `done` was given at
//! start — the server's parking lot, which resumes the run. A fetch waits on
//! a pool thread rather than on a worker, so a slow upstream costs the pool's
//! threads and not the isolates', and more fetches than threads queue here.
//! std alone: a production server would multiplex these sockets the way
//! [`crate::idle`] multiplexes idle connections, and the change would be this
//! file's.
//!
//! Every fetch is known by the id the server gave it, so it can be
//! [`Fetcher::abort`]ed: a fetch still queued is dropped before it starts,
//! and one in progress has its socket shut down from this side, which ends
//! the pool thread's read at once and tells the upstream the request is no
//! longer wanted.

use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::http::{fetch, Url};

/// What a fetch came to: the status and body, or why there is none.
pub type Fetched = Result<(u16, String), String>;

/// A fetch the pool holds, by id.
enum State {
    /// Waiting for a pool thread.
    Queued,
    /// Connected, on a pool thread; a clone of its socket, which is what an
    /// abort shuts down.
    Running(TcpStream),
}

/// What the pool counts, read by `/_stats`.
#[derive(Default)]
pub struct FetchStats {
    /// Fetches handed to the pool.
    pub started: AtomicU64,
    /// Fetches that came back with an answer or an error.
    pub finished: AtomicU64,
    /// Fetches aborted while queued: never started.
    pub aborted_queued: AtomicU64,
    /// Fetches aborted while connected: their socket shut down mid-request.
    pub aborted_running: AtomicU64,
}

struct Inner {
    queue: Mutex<VecDeque<(u64, Url)>>,
    ready: Condvar,
    states: Mutex<HashMap<u64, State>>,
    stats: FetchStats,
}

/// The pool.
pub struct Fetcher {
    inner: Arc<Inner>,
}

/// What [`Fetcher::abort`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aborted {
    /// It had not started, and now it never will.
    Queued,
    /// It was in progress, and its connection is shut down.
    Running,
    /// The pool no longer held it: it had already finished.
    Finished,
}

impl Fetcher {
    /// Starts `threads` pool threads; `done` is called on one of them with
    /// each fetch's id and what it came to — never for an aborted one.
    pub fn start(threads: usize, done: impl Fn(u64, Fetched) + Send + Sync + 'static) -> Fetcher {
        let inner = Arc::new(Inner {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            states: Mutex::new(HashMap::new()),
            stats: FetchStats::default(),
        });
        let done = Arc::new(done);
        for at in 0..threads.max(1) {
            let (inner, done) = (Arc::clone(&inner), Arc::clone(&done));
            std::thread::Builder::new()
                .name(format!("edge-fetch-{at}"))
                .spawn(move || run(&inner, &*done))
                .expect("a fetch thread starts");
        }
        Fetcher { inner }
    }

    /// Queues a fetch of `url` under `id`.
    pub fn submit(&self, id: u64, url: Url) {
        self.inner.stats.started.fetch_add(1, Ordering::Relaxed);
        self.inner.states.lock().unwrap().insert(id, State::Queued);
        self.inner.queue.lock().unwrap().push_back((id, url));
        self.inner.ready.notify_one();
    }

    /// Says the fetch `id` is no longer wanted.
    pub fn abort(&self, id: u64) -> Aborted {
        let state = self.inner.states.lock().unwrap().remove(&id);
        match state {
            Some(State::Queued) => {
                self.inner
                    .stats
                    .aborted_queued
                    .fetch_add(1, Ordering::Relaxed);
                Aborted::Queued
            }
            Some(State::Running(stream)) => {
                let _ = stream.shutdown(Shutdown::Both);
                self.inner
                    .stats
                    .aborted_running
                    .fetch_add(1, Ordering::Relaxed);
                Aborted::Running
            }
            None => Aborted::Finished,
        }
    }

    pub fn stats(&self) -> &FetchStats {
        &self.inner.stats
    }
}

fn run(inner: &Inner, done: &(dyn Fn(u64, Fetched) + Send + Sync)) {
    loop {
        let (id, url) = {
            let mut queue = inner.queue.lock().unwrap();
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = inner.ready.wait(queue).unwrap();
            }
        };
        // Aborted while it waited: its state is gone, and it never starts.
        if !inner.states.lock().unwrap().contains_key(&id) {
            continue;
        }
        let fetched = fetch(&url, |stream| {
            let mut states = inner.states.lock().unwrap();
            match (states.get_mut(&id), stream.try_clone()) {
                (Some(state), Ok(clone)) => {
                    *state = State::Running(clone);
                    true
                }
                // Aborted while it connected.
                _ => false,
            }
        });
        // An aborted fetch's state was removed by the abort; nobody is
        // waiting for what it came to.
        if inner.states.lock().unwrap().remove(&id).is_some() {
            inner.stats.finished.fetch_add(1, Ordering::Relaxed);
            done(id, fetched);
        }
    }
}
