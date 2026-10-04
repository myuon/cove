//! The run queue the workers take their jobs from: one FIFO, or a queue per
//! worker with work stealing.
//!
//! [`Discipline::Fifo`] is the queue this server had first: one deque, every
//! job pushed to its back, every worker popping its front. It is fair in
//! arrival order and it is one lock every worker contends for.
//!
//! [`Discipline::Stealing`] is the shape of Go's scheduler, cut down. Each
//! worker has a local deque, and there is one global injection queue:
//!
//! - **Work from outside a worker goes to the global queue** — a connection
//!   the idle thread found readable, a parked run the parking lot has an
//!   answer for, and a run that yielded at the end of its slice. The idle
//!   thread and the lot are not workers, so they have no local queue to put
//!   it on, and whichever worker comes free first is the one that should take
//!   it: every worker looks at the global queue as soon as its own is empty.
//!   A yielded run goes there too, as a preempted goroutine goes to Go's
//!   global queue: behind the work already waiting anywhere, which is what
//!   makes a slice a turn rather than a pause.
//! - **Work a worker makes for itself goes to its own queue** — a pipelined
//!   request already buffered on the connection it has just answered.
//! - **A worker takes, in order**: from the global queue first on every 61st
//!   take (Go's number, so a busy local queue cannot starve the global one);
//!   its own queue's front; a batch from the global queue — its fair share,
//!   `len / workers + 1`, the first to run and the rest to its own queue, so
//!   the next takes need no shared lock; and failing all of those, **half of
//!   a random victim's queue**, oldest first, tried round the other workers
//!   from a random start.
//! - **A worker with nothing to take sleeps** on one condition variable, and
//!   every push wakes one. A count of the jobs in all the queues is what a
//!   worker checks under the sleep lock, so a push between its last look and
//!   its wait cannot be missed.
//!
//! There is no `runnext` slot (Go's local LIFO slot for the run just made
//! ready): the work a worker readies for itself here is a pipelined request,
//! which is rare, and the slot's point — keeping a producer and its consumer
//! on one core — has no producer here to keep.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// How the workers share their work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discipline {
    /// One FIFO queue for every worker.
    Fifo,
    /// A queue per worker, a global injection queue, and stealing.
    Stealing,
}

impl std::fmt::Display for Discipline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Discipline::Fifo => write!(f, "one FIFO run queue"),
            Discipline::Stealing => write!(f, "a run queue per worker, with work stealing"),
        }
    }
}

/// What the queue needs to know about a job: the request it belongs to, if
/// it has one yet, so that a steal can be recorded against it.
pub trait Queued {
    fn request(&self) -> Option<u64>;
}

/// A steal: from whom, and the requests of the jobs taken.
#[derive(Debug, Default)]
pub struct Stole {
    pub from: usize,
    /// How many jobs were taken — some are connections with no request yet.
    pub jobs: usize,
    pub requests: Vec<u64>,
}

/// One worker's own state: its count of takes, and its random number.
pub struct Taker {
    worker: usize,
    takes: u64,
    seed: u64,
}

impl Taker {
    pub fn new(worker: usize) -> Taker {
        Taker {
            worker,
            takes: 0,
            // xorshift wants a non-zero seed; any per-worker one will do.
            seed: 0x9e37_79b9_7f4a_7c15 ^ (worker as u64 + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9),
        }
    }

    fn random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
}

/// The queues, and the counters `/_stats` reports.
pub struct RunQueue<T> {
    discipline: Discipline,
    global: Mutex<VecDeque<T>>,
    locals: Vec<Mutex<VecDeque<T>>>,
    /// Jobs in every queue together; what a worker checks before it sleeps.
    queued: AtomicUsize,
    /// The most jobs waiting at once.
    pub peak: AtomicUsize,
    /// Steals that took something, and the jobs they took.
    pub steals: AtomicU64,
    pub stolen: AtomicU64,
    sleep: Mutex<()>,
    wake: Condvar,
}

/// How often a worker looks at the global queue before its own.
const GLOBAL_EVERY: u64 = 61;

impl<T: Queued> RunQueue<T> {
    pub fn new(discipline: Discipline, workers: usize) -> RunQueue<T> {
        RunQueue {
            discipline,
            global: Mutex::new(VecDeque::new()),
            locals: (0..workers.max(1))
                .map(|_| Mutex::new(VecDeque::new()))
                .collect(),
            queued: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            steals: AtomicU64::new(0),
            stolen: AtomicU64::new(0),
            sleep: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    pub fn discipline(&self) -> Discipline {
        self.discipline
    }

    /// Jobs waiting in every queue together.
    pub fn len(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Counts `added` more jobs in — before they are put where a worker can
    /// take them, so that a take can never count one out before it was
    /// counted in.
    fn counting(&self, added: usize) {
        let now = self.queued.fetch_add(added, Ordering::Relaxed) + added;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Wakes as many sleeping workers as jobs were just put in.
    fn added(&self, added: usize) {
        if added == 0 {
            return;
        }
        // Taken so that a worker between its last look and its wait is either
        // already waiting (and is woken) or has not looked yet (and sees it).
        let _guard = self.sleep.lock().unwrap();
        if added == 1 {
            self.wake.notify_one();
        } else {
            self.wake.notify_all();
        }
    }

    /// Onto the back of the global queue: work from outside the workers, and
    /// a run that yielded.
    pub fn push_global(&self, job: T) {
        self.counting(1);
        self.global.lock().unwrap().push_back(job);
        self.added(1);
    }

    /// Several onto the back of the global queue, in order.
    pub fn push_global_all(&self, jobs: impl IntoIterator<Item = T>) {
        let jobs: Vec<T> = jobs.into_iter().collect();
        let added = jobs.len();
        if added == 0 {
            return;
        }
        self.counting(added);
        self.global.lock().unwrap().extend(jobs);
        self.added(added);
    }

    /// Onto the back of `worker`'s own queue — the global one, under FIFO.
    pub fn push_local(&self, worker: usize, job: T) {
        match self.discipline {
            Discipline::Fifo => self.push_global(job),
            Discipline::Stealing => {
                self.counting(1);
                self.locals[worker].lock().unwrap().push_back(job);
                self.added(1);
            }
        }
    }

    /// The next job for `taker`'s worker, waiting for one if there is none,
    /// and the steal that found it, if one did.
    pub fn take(&self, taker: &mut Taker) -> (T, Option<Stole>) {
        loop {
            if let Some(found) = self.try_take(taker) {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                return found;
            }
            let guard = self.sleep.lock().unwrap();
            if self.queued.load(Ordering::Relaxed) == 0 {
                // Spurious wake-ups are harmless: the loop looks again.
                drop(self.wake.wait(guard).unwrap());
            }
        }
    }

    fn try_take(&self, taker: &mut Taker) -> Option<(T, Option<Stole>)> {
        if self.discipline == Discipline::Fifo {
            return self
                .global
                .lock()
                .unwrap()
                .pop_front()
                .map(|job| (job, None));
        }
        taker.takes += 1;
        let me = taker.worker;
        if taker.takes.is_multiple_of(GLOBAL_EVERY) {
            if let Some(job) = self.global.lock().unwrap().pop_front() {
                return Some((job, None));
            }
        }
        if let Some(job) = self.locals[me].lock().unwrap().pop_front() {
            return Some((job, None));
        }
        if let Some(job) = self.global_share(me) {
            return Some((job, None));
        }
        self.steal(taker)
    }

    /// A fair share of the global queue: the first to run, the rest onto
    /// `me`'s own queue. They stay counted in `queued` throughout.
    fn global_share(&self, me: usize) -> Option<T> {
        let mut global = self.global.lock().unwrap();
        let first = global.pop_front()?;
        let share = (global.len() / self.locals.len()).min(64);
        if share > 0 {
            let batch: Vec<T> = global.drain(..share).collect();
            drop(global);
            self.locals[me].lock().unwrap().extend(batch);
        }
        Some(first)
    }

    /// Half of a victim's queue, oldest first, trying every other worker
    /// from a random one. The first job taken is run; the rest go onto `me`'s
    /// own queue.
    fn steal(&self, taker: &mut Taker) -> Option<(T, Option<Stole>)> {
        let workers = self.locals.len();
        if workers < 2 {
            return None;
        }
        let me = taker.worker;
        let start = (taker.random() % workers as u64) as usize;
        for offset in 0..workers {
            let victim = (start + offset) % workers;
            if victim == me {
                continue;
            }
            let mut taken: VecDeque<T> = {
                let mut queue = self.locals[victim].lock().unwrap();
                let half = queue.len().div_ceil(2);
                if half == 0 {
                    continue;
                }
                queue.drain(..half).collect()
            };
            let stole = Stole {
                from: victim,
                jobs: taken.len(),
                requests: taken.iter().filter_map(Queued::request).collect(),
            };
            self.steals.fetch_add(1, Ordering::Relaxed);
            self.stolen.fetch_add(taken.len() as u64, Ordering::Relaxed);
            let first = taken.pop_front().expect("half of a non-empty queue");
            if !taken.is_empty() {
                self.locals[me].lock().unwrap().extend(taken);
            }
            return Some((first, Some(stole)));
        }
        None
    }

    /// Forgets the peak, so that a load run reads its own.
    pub fn reset(&self) {
        self.peak.store(self.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    impl Queued for u64 {
        fn request(&self) -> Option<u64> {
            Some(*self)
        }
    }

    #[test]
    fn fifo_takes_in_arrival_order_whoever_pushed() {
        let queue = RunQueue::new(Discipline::Fifo, 2);
        queue.push_global(1u64);
        queue.push_local(1, 2);
        queue.push_global(3);
        let mut taker = Taker::new(0);
        let order: Vec<u64> = (0..3).map(|_| queue.take(&mut taker).0).collect();
        assert_eq!(order, [1, 2, 3]);
        assert!(queue.is_empty());
    }

    #[test]
    fn an_idle_worker_steals_half_of_a_busy_one_s_queue() {
        let queue = RunQueue::new(Discipline::Stealing, 2);
        for job in 0..6u64 {
            queue.push_local(0, job);
        }
        let mut thief = Taker::new(1);
        let (first, stole) = queue.take(&mut thief);
        let stole = stole.expect("worker 1 had nothing of its own");
        assert_eq!((first, stole.from, stole.jobs), (0, 0, 3));
        assert_eq!(stole.requests, [0, 1, 2]);
        // The rest of the half is the thief's now, and the victim kept its
        // newer half.
        let (next, stole) = queue.take(&mut thief);
        assert_eq!(next, 1);
        assert!(stole.is_none(), "from its own queue");
        let mut owner = Taker::new(0);
        assert_eq!(queue.take(&mut owner).0, 3);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn a_worker_takes_its_share_of_the_global_queue() {
        let queue = RunQueue::new(Discipline::Stealing, 2);
        queue.push_global_all(0..5u64);
        let mut taker = Taker::new(0);
        assert_eq!(queue.take(&mut taker).0, 0);
        // `4 / 2` more went to worker 0's own queue, and two stay global.
        assert_eq!(queue.locals[0].lock().unwrap().len(), 2);
        assert_eq!(queue.global.lock().unwrap().len(), 2);
        assert_eq!(queue.len(), 4);
    }

    #[test]
    fn a_sleeping_worker_is_woken_by_a_push() {
        let queue = Arc::new(RunQueue::new(Discipline::Stealing, 4));
        let workers: Vec<_> = (0..4)
            .map(|at| {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    let mut taker = Taker::new(at);
                    (0..250).map(|_| queue.take(&mut taker).0).sum::<u64>()
                })
            })
            .collect();
        for job in 0..1000u64 {
            if job % 3 == 0 {
                queue.push_local((job % 4) as usize, job);
            } else {
                queue.push_global(job);
            }
        }
        let total: u64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
        assert_eq!(total, (0..1000).sum::<u64>());
        assert!(queue.is_empty());
    }
}
