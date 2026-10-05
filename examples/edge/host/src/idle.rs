//! Where a connection waits between requests: one thread, `poll(2)`, and no
//! worker.
//!
//! A kept-alive connection spends most of its life idle, and a worker that
//! waited on it would be a worker not running isolates — the problem parking
//! solved for upstream calls, arriving again from the client's side. So an
//! idle connection is handed here, and this thread polls every idle socket at
//! once: one that becomes readable goes back on the run queue, one that has
//! been idle for longer than the idle timeout is closed, and nothing in
//! between costs a thread. A new connection starts here too, so a client that
//! connects and says nothing holds no worker either.
//!
//! `poll` rather than `epoll` or `kqueue` because it is the same call on
//! Linux and macOS and the `libc` crate already in the tree has it. It is
//! O(idle connections) per wake-up; with a thousand idle connections that is
//! a thousand `pollfd`s rebuilt per wake, which measured as nothing next to
//! running the isolates, and a server with a hundred thousand would want the
//! readiness APIs instead.
//!
//! On a host without `poll` (not Unix), an idle connection gets a thread of
//! its own that waits for its first byte — correct, and what this module
//! exists to avoid where it can.

use std::net::TcpStream;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A client connection, between requests or about to be read.
pub struct Conn {
    pub stream: TcpStream,
    /// Bytes read past the end of the last request: the start of the next
    /// one, when the client pipelines.
    pub buffer: Vec<u8>,
    /// Requests answered on this connection so far.
    pub served: u32,
    /// When it was accepted, which the request timeline records.
    pub opened: std::time::Instant,
    /// Whether the socket holds the options a worker reads it with (its read
    /// timeout and `TCP_NODELAY`). They outlive a request, so a worker sets
    /// them once rather than per request; whatever changes one clears this.
    pub configured: bool,
}

impl Conn {
    pub fn new(stream: TcpStream) -> Conn {
        Conn {
            stream,
            buffer: Vec::new(),
            served: 0,
            opened: std::time::Instant::now(),
            configured: false,
        }
    }
}

/// What the idle thread counts, read by `/_stats`.
#[derive(Default)]
pub struct IdleStats {
    /// Connections waiting here now.
    pub idle: AtomicI64,
    /// Connections closed here because they stayed idle past the timeout.
    pub expired: AtomicU64,
    /// Times the thread came out of `poll`, and the `pollfd`s it had handed
    /// it, summed: what one wake-up costs grows with the second over the
    /// first.
    pub wakes: AtomicU64,
    pub polled: AtomicU64,
    /// Nanoseconds spent inside `poll`, and in the rest of the loop —
    /// rebuilding the `pollfd`s and sorting what it answered. Wall time on
    /// the idle thread, not CPU: a `poll` that waits counts its wait.
    pub in_poll_ns: AtomicU64,
    pub around_poll_ns: AtomicU64,
}

/// The handle every thread parks idle connections through.
pub struct Idle {
    inner: imp::Idle,
    pub stats: Arc<IdleStats>,
}

impl Idle {
    /// Starts the idle thread. `ready` is called, on that thread, with every
    /// connection that has something to read — or has been closed by the
    /// client, which the reader finds out.
    pub fn start(
        timeout: Duration,
        ready: impl Fn(Vec<Conn>) + Send + Sync + 'static,
    ) -> std::io::Result<Idle> {
        let stats = Arc::new(IdleStats::default());
        Ok(Idle {
            inner: imp::Idle::start(timeout, Arc::new(ready), Arc::clone(&stats))?,
            stats,
        })
    }

    /// Hands a connection over to wait for its next request.
    pub fn park(&self, conn: Conn) {
        self.stats.idle.fetch_add(1, Ordering::Relaxed);
        self.inner.park(conn);
    }
}

type Ready = Arc<dyn Fn(Vec<Conn>) + Send + Sync>;

#[cfg(unix)]
mod imp {
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    pub struct Idle {
        sender: Mutex<Sender<Conn>>,
        /// One byte written here wakes the thread out of `poll` to take what
        /// was sent.
        waker: UnixStream,
    }

    impl Idle {
        pub fn start(
            timeout: Duration,
            ready: Ready,
            stats: Arc<IdleStats>,
        ) -> std::io::Result<Idle> {
            let (sender, received) = mpsc::channel();
            let (waker, woken) = UnixStream::pair()?;
            waker.set_nonblocking(true)?;
            woken.set_nonblocking(true)?;
            std::thread::Builder::new()
                .name("edge-idle".into())
                .spawn(move || run(timeout, received, woken, ready, stats))?;
            Ok(Idle {
                sender: Mutex::new(sender),
                waker,
            })
        }

        pub fn park(&self, conn: Conn) {
            // The thread outlives every sender; a send cannot fail while the
            // server runs.
            let _ = self.sender.lock().unwrap().send(conn);
            // A full pipe means a wake-up is already pending.
            let _ = (&self.waker).write(&[1]);
        }
    }

    fn run(
        timeout: Duration,
        received: Receiver<Conn>,
        mut woken: UnixStream,
        ready: Ready,
        stats: Arc<IdleStats>,
    ) {
        let mut idle: Vec<(Conn, Instant)> = Vec::new();
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let mut drain = [0u8; 256];
        let mut polled_at = Instant::now();
        loop {
            let now = Instant::now();
            idle.extend(received.try_iter().map(|conn| (conn, now + timeout)));
            fds.clear();
            fds.push(libc::pollfd {
                fd: woken.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.extend(idle.iter().map(|(conn, _)| libc::pollfd {
                fd: conn.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
            let wait = idle
                .iter()
                .map(|(_, expires)| expires.saturating_duration_since(now))
                .min()
                .map_or(-1, |left| (left.as_millis() as i32).saturating_add(1));
            let polling = Instant::now();
            // Safety: `fds` is a live, correctly sized array of `pollfd`s, and
            // every descriptor in it is owned by a value this thread holds.
            let woke = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
            let polled = Instant::now();
            stats.wakes.fetch_add(1, Ordering::Relaxed);
            stats.polled.fetch_add(fds.len() as u64, Ordering::Relaxed);
            stats
                .in_poll_ns
                .fetch_add((polled - polling).as_nanos() as u64, Ordering::Relaxed);
            stats
                .around_poll_ns
                .fetch_add((polling - polled_at).as_nanos() as u64, Ordering::Relaxed);
            polled_at = polled;
            if woke < 0 && std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
                eprintln!("edge-idle: poll: {}", std::io::Error::last_os_error());
            }
            if fds[0].revents != 0 {
                while matches!(woken.read(&mut drain), Ok(n) if n > 0) {}
            }
            let now = Instant::now();
            let mut readable = Vec::new();
            let mut kept = Vec::with_capacity(idle.len());
            for (at, (conn, expires)) in idle.drain(..).enumerate() {
                // A connection parked after `fds` was built has no entry yet.
                let revents = fds.get(at + 1).map_or(0, |fd| fd.revents);
                if revents != 0 {
                    readable.push(conn);
                } else if expires <= now {
                    stats.expired.fetch_add(1, Ordering::Relaxed);
                    stats.idle.fetch_sub(1, Ordering::Relaxed);
                    // Dropped: the client sees the connection close, which
                    // is what an idle timeout is.
                } else {
                    kept.push((conn, expires));
                }
            }
            idle = kept;
            if !readable.is_empty() {
                stats
                    .idle
                    .fetch_sub(readable.len() as i64, Ordering::Relaxed);
                ready(readable);
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::sync::atomic::Ordering;

    use super::*;

    pub struct Idle {
        timeout: Duration,
        ready: Ready,
        stats: Arc<IdleStats>,
    }

    impl Idle {
        pub fn start(
            timeout: Duration,
            ready: Ready,
            stats: Arc<IdleStats>,
        ) -> std::io::Result<Idle> {
            Ok(Idle {
                timeout,
                ready,
                stats,
            })
        }

        pub fn park(&self, conn: Conn) {
            let (timeout, ready, stats) = (
                self.timeout,
                Arc::clone(&self.ready),
                Arc::clone(&self.stats),
            );
            std::thread::spawn(move || {
                let mut conn = conn;
                let _ = conn.stream.set_read_timeout(Some(timeout));
                // The idle wait's timeout is not the one a worker reads with.
                conn.configured = false;
                let mut byte = [0u8; 1];
                let woke = conn.stream.peek(&mut byte).is_ok();
                stats.idle.fetch_sub(1, Ordering::Relaxed);
                if woke {
                    ready(vec![conn]);
                } else {
                    stats.expired.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    }
}
