//! A channel whose receiver's timed wait wakes on time on macOS, for the
//! parking lot.
//!
//! The parking lot sleeps until its next answer is due, and it used to sleep
//! in `mpsc::Receiver::recv_timeout`, which on macOS is a
//! `pthread_cond_timedwait`. On the machine `compare/README.md` was measured
//! on, that wait woke up to **150 ms** late for timeouts from 20 ms up
//! (`compare/timer-probe`), and so did `nanosleep` and `mach_wait_until`, at
//! every QoS class a thread can ask for: the kernel gives those timers a
//! coalescing leeway, and this machine's sessions get a large one. A `kevent`
//! timer marked `NOTE_CRITICAL` asks for none, and woke within 0.1 ms. So on
//! macOS the receiver here waits in `kevent`, on that timer and on an
//! `EVFILT_USER` event that a send triggers.
//!
//! Elsewhere it is `mpsc` unchanged: Linux's timer slack for a condition
//! variable is 50 µs, and nothing measured a problem there.

use std::sync::mpsc::{self, RecvTimeoutError, SendError};
use std::time::Instant;

/// The sending half. Cloned freely; every clone wakes the one receiver.
pub struct Outbox<T> {
    sender: mpsc::Sender<T>,
    ring: imp::Ring,
}

impl<T> Clone for Outbox<T> {
    fn clone(&self) -> Self {
        Outbox {
            sender: self.sender.clone(),
            ring: self.ring.clone(),
        }
    }
}

/// The receiving half.
pub struct Inbox<T> {
    receiver: mpsc::Receiver<T>,
    wait: imp::Wait,
}

/// A connected pair.
pub fn inbox<T>() -> std::io::Result<(Outbox<T>, Inbox<T>)> {
    let (sender, receiver) = mpsc::channel();
    let (ring, wait) = imp::pair()?;
    Ok((Outbox { sender, ring }, Inbox { receiver, wait }))
}

impl<T> Outbox<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.sender.send(value)?;
        // After the send, so that a receiver this wakes finds the value.
        self.ring.ring();
        Ok(())
    }
}

impl<T> Inbox<T> {
    /// The next value, waiting until `deadline` at the latest — or for ever,
    /// given `None`: `mpsc::Receiver::recv_timeout`'s answers.
    ///
    /// A deadline rather than a timeout, because the parking lot's is an
    /// instant, and the same instant asked for again is the same timer: it is
    /// armed once, not once per message that wakes the lot before it.
    ///
    /// On macOS a receiver blocked with no deadline is not woken by the last
    /// sender going away; the parking lot's senders live as long as the
    /// server.
    pub fn recv_deadline(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        self.wait.recv(&self.receiver, deadline)
    }

    /// Every value already sent, without waiting.
    pub fn try_iter(&self) -> mpsc::TryIter<'_, T> {
        self.receiver.try_iter()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
    use std::sync::Arc;
    use std::time::Instant;

    /// The two events on the queue: a send, and the deadline.
    const SENT: usize = 1;
    const DUE: usize = 2;

    pub fn pair() -> std::io::Result<(Ring, Wait)> {
        // Safety: `kqueue` takes nothing and answers a new descriptor or -1.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Safety: `fd` is a fresh descriptor that nothing else owns.
        let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        // EV_CLEAR: a trigger is consumed by the wait that sees it.
        change(
            &queue,
            SENT,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
        )?;
        let pending = Arc::new(AtomicBool::new(false));
        Ok((
            Ring {
                queue: Arc::clone(&queue),
                pending: Arc::clone(&pending),
            },
            Wait {
                queue,
                pending,
                armed: Cell::new(None),
            },
        ))
    }

    fn change(
        queue: &OwnedFd,
        ident: usize,
        filter: i16,
        flags: u16,
        fflags: u32,
        data: isize,
    ) -> std::io::Result<()> {
        let event = libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data,
            udata: std::ptr::null_mut(),
        };
        // Safety: one change, no events wanted back, and no timeout.
        let done = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if done < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[derive(Clone)]
    pub struct Ring {
        queue: Arc<OwnedFd>,
        /// Whether a trigger is on its way: set by the ring that triggers,
        /// cleared by the receiver before it looks at the channel. A burst
        /// of sends costs one `kevent`.
        pending: Arc<AtomicBool>,
    }

    impl Ring {
        pub fn ring(&self) {
            if !self.pending.swap(true, Ordering::AcqRel) {
                let _ = change(
                    &self.queue,
                    SENT,
                    libc::EVFILT_USER,
                    0,
                    libc::NOTE_TRIGGER,
                    0,
                );
            }
        }
    }

    pub struct Wait {
        queue: Arc<OwnedFd>,
        pending: Arc<AtomicBool>,
        /// The deadline the queue's timer is armed for, if it is armed.
        armed: Cell<Option<Instant>>,
    }

    impl Wait {
        pub fn recv<T>(
            &self,
            receiver: &mpsc::Receiver<T>,
            deadline: Option<Instant>,
        ) -> Result<T, RecvTimeoutError> {
            loop {
                // Cleared before the channel is looked at: a send after this
                // triggers again, and one before it is taken just below.
                self.pending.swap(false, Ordering::AcqRel);
                match receiver.try_recv() {
                    Ok(value) => return Ok(value),
                    Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                    Err(TryRecvError::Empty) => {}
                }
                if let Some(deadline) = deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    if self.armed.get() != Some(deadline) {
                        // Re-adding replaces the last one. NOTE_CRITICAL asks
                        // for no coalescing leeway, which is the point.
                        let _ = change(
                            &self.queue,
                            DUE,
                            libc::EVFILT_TIMER,
                            libc::EV_ADD | libc::EV_ONESHOT,
                            libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
                            left.as_nanos().min(isize::MAX as u128) as isize,
                        );
                        self.armed.set(Some(deadline));
                    }
                }
                let mut seen = [libc::kevent {
                    ident: 0,
                    filter: 0,
                    flags: 0,
                    fflags: 0,
                    data: 0,
                    udata: std::ptr::null_mut(),
                }; 2];
                // Safety: room for two events, no changes, no timeout: it
                // returns when the user event triggers or the timer fires. A
                // timer armed for an earlier deadline that fires here only
                // sends the loop round again.
                let got = unsafe {
                    libc::kevent(
                        self.queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        seen.as_mut_ptr(),
                        2,
                        std::ptr::null(),
                    )
                };
                if seen[..got.max(0) as usize]
                    .iter()
                    .any(|event| event.filter == libc::EVFILT_TIMER)
                {
                    // One-shot: it is gone, whichever deadline it was for.
                    self.armed.set(None);
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Instant;

    pub fn pair() -> std::io::Result<(Ring, Wait)> {
        Ok((Ring, Wait))
    }

    #[derive(Clone)]
    pub struct Ring;

    impl Ring {
        pub fn ring(&self) {}
    }

    pub struct Wait;

    impl Wait {
        pub fn recv<T>(
            &self,
            receiver: &mpsc::Receiver<T>,
            deadline: Option<Instant>,
        ) -> Result<T, RecvTimeoutError> {
            match deadline {
                None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(at) => receiver.recv_timeout(at.saturating_duration_since(Instant::now())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_value_sent_before_the_wait_is_received_at_once() {
        let (out, inbox) = inbox().unwrap();
        out.send(7).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(inbox.recv_deadline(Some(deadline)), Ok(7));
    }

    #[test]
    fn a_wait_with_nothing_sent_times_out_and_not_early() {
        let (_out, inbox) = inbox::<u32>().unwrap();
        let deadline = Instant::now() + Duration::from_millis(30);
        assert_eq!(
            inbox.recv_deadline(Some(deadline)),
            Err(RecvTimeoutError::Timeout)
        );
        assert!(Instant::now() >= deadline);
        // The same deadline again, now passed, answers at once; a later one
        // is armed afresh.
        assert_eq!(
            inbox.recv_deadline(Some(deadline)),
            Err(RecvTimeoutError::Timeout)
        );
        let later = Instant::now() + Duration::from_millis(10);
        assert_eq!(
            inbox.recv_deadline(Some(later)),
            Err(RecvTimeoutError::Timeout)
        );
        assert!(Instant::now() >= later);
    }

    #[test]
    fn every_value_from_other_threads_is_received() {
        let (out, inbox) = inbox().unwrap();
        let senders: Vec<_> = (0..4)
            .map(|_| {
                let out = out.clone();
                std::thread::spawn(move || {
                    for at in 0..5_000u32 {
                        out.send(at).unwrap();
                    }
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = 0;
        while got < 20_000 {
            inbox.recv_deadline(Some(deadline)).unwrap();
            got += 1 + inbox.try_iter().count();
        }
        for sender in senders {
            sender.join().unwrap();
        }
        assert_eq!(got, 20_000);
    }
}
