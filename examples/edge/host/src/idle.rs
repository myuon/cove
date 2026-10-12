//! Where a connection waits between requests: one thread, the kernel's
//! readiness queue, and no worker.
//!
//! A kept-alive connection spends most of its life idle, and a worker that
//! waited on it would be a worker not running isolates — the problem parking
//! solved for upstream calls, arriving again from the client's side. So an
//! idle connection is handed here, and this thread waits on every idle socket
//! at once: one that becomes readable goes back on the run queue, one that has
//! been idle for longer than the idle timeout is closed, and nothing in
//! between costs a thread. A new connection starts here too, so a client that
//! connects and says nothing holds no worker either.
//!
//! The wait is `kqueue` on macOS and FreeBSD and `epoll` on Linux, so a
//! wake-up costs O(ready), not O(idle) (issue #590). A parked connection is
//! armed once per park, one-shot (`EV_DISPATCH`, `EPOLLONESHOT`), and kept in a slab
//! indexed by its descriptor; the kernel hands back only the descriptors that
//! turned readable, and each leaves the slab as it goes back to a worker.
//! When the idle timeout falls is kept in a heap of `(expires, descriptor,
//! generation)`, so finding the next expiry and the expired is O(log n) per
//! connection rather than a scan of every idle one per wake-up. The thread is
//! woken to take a newly parked connection by `EVFILT_USER` or an `eventfd`.
//!
//! Before #590 this was `poll(2)` over every idle socket, rebuilt per wake-up;
//! `compare/idle_cost.py` measured that saturating this thread from about
//! 1,000 connections. It is still the back end on any other Unix, where it is
//! correct and O(idle) again.
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
    /// Times the thread came out of its wait, and the connections those
    /// wake-ups handed back as readable, summed. With a readiness back end a
    /// wake-up's cost follows the second, not how many are idle.
    pub wakes: AtomicU64,
    pub ready: AtomicU64,
    /// Nanoseconds spent inside the wait (`kevent`, `epoll_wait`, `poll`),
    /// and in the rest of the loop — arming what was parked, sorting what
    /// the wait answered, expiring. Wall time on the idle thread, not CPU: a
    /// wait that waits counts its wait.
    pub in_wait_ns: AtomicU64,
    pub around_wait_ns: AtomicU64,
    /// The idle thread's own CPU time (`CLOCK_THREAD_CPUTIME_ID`), sampled
    /// at most once a millisecond, so it can lag the truth by that much.
    pub cpu_ns: AtomicU64,
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

    /// How this host waits: `kqueue`, `epoll`, `poll`, or `thread` (a
    /// thread per idle connection).
    pub fn backend(&self) -> &'static str {
        imp::BACKEND
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
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    pub use sys::BACKEND;
    use sys::{Poller, Waker};

    use super::*;

    pub struct Idle {
        sender: Mutex<Sender<Conn>>,
        /// Wakes the thread out of its wait to take what was sent.
        waker: Waker,
        /// Whether a wake-up is already on its way: set by the park that
        /// sends one, cleared by the thread once its wait has returned the
        /// wake-up and before it takes what was sent, so a park between the
        /// two needs no wake-up of its own.
        pending: Arc<AtomicBool>,
    }

    impl Idle {
        pub fn start(
            timeout: Duration,
            ready: Ready,
            stats: Arc<IdleStats>,
        ) -> std::io::Result<Idle> {
            let (sender, received) = mpsc::channel();
            let (poller, waker) = Poller::new()?;
            let pending = Arc::new(AtomicBool::new(false));
            let cleared = Arc::clone(&pending);
            std::thread::Builder::new()
                .name("edge-idle".into())
                .spawn(move || run(timeout, received, poller, cleared, ready, stats))?;
            Ok(Idle {
                sender: Mutex::new(sender),
                waker,
                pending,
            })
        }

        pub fn park(&self, conn: Conn) {
            // The thread outlives every sender; a send cannot fail while the
            // server runs.
            let _ = self.sender.lock().unwrap().send(conn);
            // Sent before the flag is read. A thread that clears the flag after
            // this swap reads this swap's value, so it sees the connection; if
            // it cleared the flag before, this swap finds it clear and wakes it.
            if !self.pending.swap(true, Ordering::AcqRel) {
                self.waker.wake();
            }
        }
    }

    /// A connection in the slab, and which park of it this is: an expiry in
    /// the heap is for this one only if the generations match, because the
    /// descriptor may have gone back to a worker and been parked again since.
    struct Parked {
        conn: Conn,
        generation: u64,
    }

    /// The idle connections, indexed by descriptor. A descriptor names one
    /// open socket at a time, and a socket in the slab is owned by it, so the
    /// descriptor is a token the kernel can hand back that cannot be stale
    /// while its slot is full.
    struct Slab {
        slots: Vec<Option<Parked>>,
        live: usize,
    }

    impl Slab {
        fn insert(&mut self, fd: RawFd, parked: Parked) {
            let at = fd as usize;
            if at >= self.slots.len() {
                self.slots.resize_with(at + 1, || None);
            }
            debug_assert!(self.slots[at].is_none(), "descriptor {fd} parked twice");
            self.slots[at] = Some(parked);
            self.live += 1;
        }

        fn take(&mut self, fd: RawFd) -> Option<Conn> {
            let parked = self.slots.get_mut(fd as usize)?.take()?;
            self.live -= 1;
            Some(parked.conn)
        }

        fn holds(&self, fd: RawFd, generation: u64) -> bool {
            matches!(self.slots.get(fd as usize), Some(Some(p)) if p.generation == generation)
        }
    }

    fn run(
        timeout: Duration,
        received: Receiver<Conn>,
        mut poller: Poller,
        pending: Arc<AtomicBool>,
        ready: Ready,
        stats: Arc<IdleStats>,
    ) {
        let mut slab = Slab {
            slots: Vec::new(),
            live: 0,
        };
        // Every park pushes one; an entry whose connection has since left the
        // slab is dropped when it reaches the top, or by the compaction below
        // if those pile up faster than they expire.
        let mut expiries: BinaryHeap<Reverse<(Instant, RawFd, u64)>> = BinaryHeap::new();
        let mut generation = 0u64;
        let mut woken: Vec<RawFd> = Vec::new();
        let mut readable = Vec::new();
        let mut waited_at = Instant::now();
        let mut sampled_at = waited_at;
        loop {
            let now = Instant::now();
            for conn in received.try_iter() {
                let fd = conn.stream.as_raw_fd();
                if let Err(e) = poller.arm(fd) {
                    // Not watched, so not ours to wait on: a worker reads it
                    // with its own timeout, and finds out.
                    eprintln!("edge-idle: watching a connection: {e}");
                    readable.push(conn);
                    continue;
                }
                generation += 1;
                slab.insert(fd, Parked { conn, generation });
                expiries.push(Reverse((now + timeout, fd, generation)));
            }
            if expiries.len() > 4 * slab.live + 4096 {
                expiries.retain(|Reverse((_, fd, g))| slab.holds(*fd, *g));
            }
            while let Some(&Reverse((_, fd, g))) = expiries.peek() {
                if slab.holds(fd, g) {
                    break;
                }
                expiries.pop();
            }
            if !readable.is_empty() {
                stats
                    .idle
                    .fetch_sub(readable.len() as i64, Ordering::Relaxed);
                ready(std::mem::take(&mut readable));
            }
            let wait = expiries
                .peek()
                .map(|Reverse((at, _, _))| at.saturating_duration_since(now));
            let waiting = Instant::now();
            let woke = poller.wait(wait, &mut woken);
            let waited = Instant::now();
            stats.wakes.fetch_add(1, Ordering::Relaxed);
            stats.ready.fetch_add(woken.len() as u64, Ordering::Relaxed);
            stats
                .in_wait_ns
                .fetch_add((waited - waiting).as_nanos() as u64, Ordering::Relaxed);
            stats
                .around_wait_ns
                .fetch_add((waiting - waited_at).as_nanos() as u64, Ordering::Relaxed);
            waited_at = waited;
            if waited - sampled_at >= Duration::from_millis(1) {
                sampled_at = waited;
                stats.cpu_ns.store(thread_cpu_ns(), Ordering::Relaxed);
            }
            if woke {
                // Cleared after the wait has consumed the wake-up and before
                // the next `try_iter`: a park after the clear wakes the next
                // wait, and one before it — which sent no wake-up, seeing the
                // flag still set — is taken at the top of the loop. Cleared
                // before the wake-up is consumed, one sent in between would be
                // consumed with the flag left set, and no park would wake the
                // thread again.
                pending.swap(false, Ordering::AcqRel);
            }
            readable.extend(woken.drain(..).filter_map(|fd| slab.take(fd)));
            let now = Instant::now();
            while let Some(&Reverse((at, fd, g))) = expiries.peek() {
                if at > now {
                    break;
                }
                expiries.pop();
                if slab.holds(fd, g) {
                    poller.forget(fd);
                    // Dropped: the client sees the connection close, which is
                    // what an idle timeout is. Closing the descriptor also
                    // takes it out of the kernel's queue.
                    drop(slab.take(fd));
                    stats.expired.fetch_add(1, Ordering::Relaxed);
                    stats.idle.fetch_sub(1, Ordering::Relaxed);
                }
            }
            if !readable.is_empty() {
                stats
                    .idle
                    .fetch_sub(readable.len() as i64, Ordering::Relaxed);
                ready(std::mem::take(&mut readable));
            }
        }
    }

    fn thread_cpu_ns() -> u64 {
        // Safety: `clock_gettime` writes one `timespec`, which is plain data.
        unsafe {
            let mut now: libc::timespec = std::mem::zeroed();
            if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) != 0 {
                return 0;
            }
            now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
        }
    }

    fn last_error(call: &str) {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            eprintln!("edge-idle: {call}: {error}");
        }
    }

    /// `kqueue`: a descriptor is armed with `EV_ADD | EV_DISPATCH`, which
    /// disables it once it has fired, and the next park re-arms it with the
    /// same change plus `EV_ENABLE`. The `EV_ENABLE` is not redundant: on
    /// macOS an `EV_ADD` of an event `EV_DISPATCH` has disabled leaves it
    /// disabled, whatever the manual says of adding, and a first version
    /// without it never saw a kept-alive connection's second request. The arms are queued and handed to the next `kevent` with the wait,
    /// so a wake-up is one call however many connections were parked. The
    /// waker is an `EVFILT_USER` event with `EV_CLEAR`, which the wait that
    /// returns it also resets.
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    mod sys {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
        use std::sync::Arc;
        use std::time::Duration;

        use super::last_error;

        pub const BACKEND: &str = "kqueue";

        /// The `ident` of the waker's `EVFILT_USER` event. A user event's
        /// ident is its own namespace, so it cannot meet a descriptor's.
        const WAKE: libc::uintptr_t = 0;

        pub struct Poller {
            kq: Arc<OwnedFd>,
            changes: Vec<libc::kevent>,
            events: Vec<libc::kevent>,
        }

        pub struct Waker {
            kq: Arc<OwnedFd>,
        }

        // Safety: the only pointers in a `Poller` are the `udata` of its
        // `kevent`s, which carry a descriptor and are never dereferenced.
        unsafe impl Send for Poller {}

        fn event(ident: libc::uintptr_t, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
            // Safety: `kevent` is plain data, and zero is a valid value of
            // every field — including the ones only some systems have.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            event.ident = ident;
            event.filter = filter;
            event.flags = flags;
            event.fflags = fflags;
            event.udata = ident as _;
            event
        }

        impl Poller {
            pub fn new() -> std::io::Result<(Poller, Waker)> {
                // Safety: `kqueue` takes nothing and returns a new descriptor
                // or -1.
                let fd = unsafe { libc::kqueue() };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Safety: `fd` was just opened and nothing else owns it.
                let kq = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
                // Safety: `FD_CLOEXEC` on a descriptor this function owns.
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                let user = event(WAKE, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0);
                // Safety: one change, no events, on a live kqueue.
                let added = unsafe {
                    libc::kevent(fd, &user, 1, std::ptr::null_mut(), 0, std::ptr::null())
                };
                if added < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok((
                    Poller {
                        kq: Arc::clone(&kq),
                        changes: Vec::new(),
                        events: Vec::with_capacity(1024),
                    },
                    Waker { kq },
                ))
            }

            /// Queued for the next `wait`; an error in it comes back from
            /// there, as that descriptor turning readable.
            pub fn arm(&mut self, fd: RawFd) -> std::io::Result<()> {
                self.changes.push(event(
                    fd as libc::uintptr_t,
                    libc::EVFILT_READ,
                    libc::EV_ADD | libc::EV_ENABLE | libc::EV_DISPATCH,
                    0,
                ));
                Ok(())
            }

            /// Closing the descriptor removes its event, so there is nothing
            /// to do — unless its arm is still queued, which would then name
            /// a closed descriptor, or worse, the next one opened under its
            /// number.
            pub fn forget(&mut self, fd: RawFd) {
                if !self.changes.is_empty() {
                    self.changes.retain(|c| c.ident != fd as libc::uintptr_t);
                }
            }

            /// Waits up to `timeout` (forever if `None`) and leaves the
            /// readable descriptors in `ready`. Whether the waker fired.
            pub fn wait(&mut self, timeout: Option<Duration>, ready: &mut Vec<RawFd>) -> bool {
                ready.clear();
                // Room for every change's error as well as for events, so
                // that `kevent` never fails a change for want of space.
                let room = self.changes.len().max(1024);
                self.events.clear();
                self.events.reserve(room);
                let timeout = timeout.map(|left| libc::timespec {
                    tv_sec: left.as_secs().min(i32::MAX as u64) as libc::time_t,
                    tv_nsec: left.subsec_nanos() as _,
                });
                // Safety: `changes` is initialised for its length, `events`
                // has capacity for `room`, and `kevent` writes at most that
                // many, which `set_len` then covers.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        self.changes.as_ptr(),
                        self.changes.len() as _,
                        self.events.as_mut_ptr(),
                        room as _,
                        timeout
                            .as_ref()
                            .map_or(std::ptr::null(), |t| t as *const libc::timespec),
                    )
                };
                if n < 0 {
                    // The changes may not have been applied; an interrupted
                    // call applies them before it waits, but nothing says so
                    // of every error, so they are kept for the next call.
                    last_error("kevent");
                    return false;
                }
                self.changes.clear();
                // Safety: `kevent` initialised the first `n` events.
                unsafe { self.events.set_len(n as usize) };
                let mut woke = false;
                for event in &self.events {
                    if event.filter == libc::EVFILT_USER {
                        woke = true;
                    } else {
                        // Readable, hung up, or a change that failed
                        // (`EV_ERROR`): a worker reads it and finds out
                        // which.
                        ready.push(event.ident as RawFd);
                    }
                }
                woke
            }
        }

        impl Waker {
            pub fn wake(&self) {
                let trigger = event(WAKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
                // Safety: one change, no events, on a live kqueue.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        &trigger,
                        1,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                };
                if n < 0 {
                    last_error("kevent (wake)");
                }
            }
        }
    }

    /// `epoll`: a descriptor is armed with `EPOLLIN | EPOLLONESHOT`, which
    /// disables it once it has fired, so the next park re-arms it with
    /// `EPOLL_CTL_MOD` (and a descriptor epoll has not seen, or forgot when
    /// it was closed, is added). The waker is an `eventfd`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod sys {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
        use std::sync::Arc;
        use std::time::Duration;

        use super::last_error;

        pub const BACKEND: &str = "epoll";

        /// The waker's token. Descriptors are non-negative, so it cannot be
        /// one.
        const WAKE: u64 = u64::MAX;

        pub struct Poller {
            epoll: OwnedFd,
            wake: Arc<OwnedFd>,
            events: Vec<libc::epoll_event>,
        }

        pub struct Waker {
            wake: Arc<OwnedFd>,
        }

        fn control(
            epoll: RawFd,
            op: libc::c_int,
            fd: RawFd,
            events: u32,
            token: u64,
        ) -> std::io::Result<()> {
            let mut event = libc::epoll_event { events, u64: token };
            // Safety: `event` is a live `epoll_event` for the call.
            if unsafe { libc::epoll_ctl(epoll, op, fd, &mut event) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        impl Poller {
            pub fn new() -> std::io::Result<(Poller, Waker)> {
                // Safety: these take flags and return a new descriptor or -1.
                let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
                if epoll < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Safety: `epoll` was just opened and nothing else owns it.
                let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
                let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if wake < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Safety: as above.
                let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(wake) });
                control(
                    epoll.as_raw_fd(),
                    libc::EPOLL_CTL_ADD,
                    wake.as_raw_fd(),
                    libc::EPOLLIN as u32,
                    WAKE,
                )?;
                Ok((
                    Poller {
                        epoll,
                        wake: Arc::clone(&wake),
                        events: Vec::with_capacity(1024),
                    },
                    Waker { wake },
                ))
            }

            pub fn arm(&mut self, fd: RawFd) -> std::io::Result<()> {
                let interest = (libc::EPOLLIN | libc::EPOLLONESHOT) as u32;
                let epoll = self.epoll.as_raw_fd();
                match control(epoll, libc::EPOLL_CTL_MOD, fd, interest, fd as u64) {
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                        control(epoll, libc::EPOLL_CTL_ADD, fd, interest, fd as u64)
                    }
                    done => done,
                }
            }

            /// Closing the descriptor removes it from the epoll set.
            pub fn forget(&mut self, _fd: RawFd) {}

            pub fn wait(&mut self, timeout: Option<Duration>, ready: &mut Vec<RawFd>) -> bool {
                ready.clear();
                self.events.clear();
                let room = self.events.capacity();
                // Rounded up, so that a wait for an expiry does not come back
                // just before it and spin.
                let timeout = timeout.map_or(-1, |left| {
                    let ms = left.as_nanos().div_ceil(1_000_000);
                    ms.min(i32::MAX as u128) as libc::c_int
                });
                // Safety: `events` has capacity for `room`, and `epoll_wait`
                // writes at most that many, which `set_len` then covers.
                let n = unsafe {
                    libc::epoll_wait(
                        self.epoll.as_raw_fd(),
                        self.events.as_mut_ptr(),
                        room as libc::c_int,
                        timeout,
                    )
                };
                if n < 0 {
                    last_error("epoll_wait");
                    return false;
                }
                // Safety: `epoll_wait` initialised the first `n` events.
                unsafe { self.events.set_len(n as usize) };
                let mut woke = false;
                for event in &self.events {
                    let token = event.u64;
                    if token == WAKE {
                        woke = true;
                    } else {
                        ready.push(token as RawFd);
                    }
                }
                if woke {
                    let mut count = 0u64;
                    // Safety: an `eventfd` read is eight bytes into a `u64`.
                    unsafe {
                        libc::read(
                            self.wake.as_raw_fd(),
                            (&mut count as *mut u64).cast(),
                            std::mem::size_of::<u64>(),
                        )
                    };
                }
                woke
            }
        }

        impl Waker {
            pub fn wake(&self) {
                let one = 1u64;
                // Safety: an `eventfd` write is eight bytes from a `u64`. One
                // that would overflow the counter means a wake-up is already
                // pending.
                unsafe {
                    libc::write(
                        self.wake.as_raw_fd(),
                        (&one as *const u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
            }
        }
    }

    /// `poll(2)`, for any other Unix: what this thread did everywhere before
    /// #590, and O(idle) per wake-up again. The waker is one end of a socket
    /// pair.
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "android"
    )))]
    mod sys {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, RawFd};
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        use super::last_error;

        pub const BACKEND: &str = "poll";

        pub struct Poller {
            woken: UnixStream,
            /// What is armed: the descriptors a `pollfd` is built for, after
            /// the waker's.
            armed: Vec<RawFd>,
            fds: Vec<libc::pollfd>,
        }

        pub struct Waker {
            waker: UnixStream,
        }

        impl Poller {
            pub fn new() -> std::io::Result<(Poller, Waker)> {
                let (waker, woken) = UnixStream::pair()?;
                waker.set_nonblocking(true)?;
                woken.set_nonblocking(true)?;
                Ok((
                    Poller {
                        woken,
                        armed: Vec::new(),
                        fds: Vec::new(),
                    },
                    Waker { waker },
                ))
            }

            pub fn arm(&mut self, fd: RawFd) -> std::io::Result<()> {
                self.armed.push(fd);
                Ok(())
            }

            pub fn forget(&mut self, fd: RawFd) {
                if let Some(at) = self.armed.iter().position(|&armed| armed == fd) {
                    self.armed.swap_remove(at);
                }
            }

            pub fn wait(&mut self, timeout: Option<Duration>, ready: &mut Vec<RawFd>) -> bool {
                ready.clear();
                self.fds.clear();
                self.fds.extend(
                    std::iter::once(self.woken.as_raw_fd())
                        .chain(self.armed.iter().copied())
                        .map(|fd| libc::pollfd {
                            fd,
                            events: libc::POLLIN,
                            revents: 0,
                        }),
                );
                let timeout = timeout.map_or(-1, |left| {
                    let ms = left.as_nanos().div_ceil(1_000_000);
                    ms.min(i32::MAX as u128) as libc::c_int
                });
                // Safety: `fds` is a live, correctly sized array of `pollfd`s.
                let n = unsafe {
                    libc::poll(
                        self.fds.as_mut_ptr(),
                        self.fds.len() as libc::nfds_t,
                        timeout,
                    )
                };
                if n < 0 {
                    last_error("poll");
                    return false;
                }
                // One-shot, as the readiness back ends are: a descriptor that
                // answered is disarmed until it is parked again.
                for at in (0..self.armed.len()).rev() {
                    if self.fds[at + 1].revents != 0 {
                        ready.push(self.armed.swap_remove(at));
                    }
                }
                let woke = self.fds[0].revents != 0;
                if woke {
                    let mut drain = [0u8; 256];
                    while matches!(self.woken.read(&mut drain), Ok(n) if n > 0) {}
                }
                woke
            }
        }

        impl Waker {
            pub fn wake(&self) {
                // A full pipe means a wake-up is already pending.
                let _ = (&self.waker).write(&[1]);
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::sync::atomic::Ordering;

    use super::*;

    pub const BACKEND: &str = "thread";

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
