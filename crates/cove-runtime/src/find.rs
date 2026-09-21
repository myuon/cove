//! The matcher beneath [`cove_ir::Inst::RunFind`]: one bounded search over a
//! run of packed bytes, written so that it can stop between any two units and
//! continue, and so that it allocates nothing on any path.
//!
//! [ADR 0065](../../../docs/adr/0065-a-run-search-is-the-one-loop-that-stays-below.md)'s
//! Decision 4 is what shapes this file, and it asks for three things at once:
//! the search is `O(n + m)`, it is charged as it goes, and the uninterruptible
//! span between two safepoints is `SAFEPOINT_STRIDE` units **in both phases**
//! — the needle's preparation included, which is the part an implementer would
//! not think of unless it were written down. A matcher that can only be called
//! and waited for satisfies the first and neither of the others: `str::find`
//! reports neither where it stopped nor anything to resume from.
//!
//! # Crochemore–Perrin, and why the first attempt was not
//!
//! This is the two-way algorithm — the one `str::find` runs, and the one ADR
//! 0065 names — in its resumable form: a critical factorization of the needle,
//! then attempts that compare the right half of the needle forwards and the
//! left half backwards and shift by the factorization's period. Its auxiliary
//! space is **`O(1)`**: seven machine words of state, all of them in
//! [`Matcher`], and no table, no copy of the needle and no buffer of any kind.
//! Both runs are read where they are, a unit at a time, through whatever
//! reader the caller hands it.
//!
//! **This file held a Knuth–Morris–Pratt matcher first, and that was a
//! mistake worth recording.** KMP was chosen because the adoption gate as
//! first written demanded that a whole scan be charged *exactly*
//! `m + (n - from)`, and only a matcher whose two phases have counters running
//! `0..m` and `from..n` can charge a count. KMP has those; two-way's
//! maximal-suffix scans do not. But an exact-`m` charge forces a table, a
//! table of `m` entries has to live somewhere, and the somewhere was a `Vec`
//! appended to one entry at a time — which reallocates and copies its whole
//! contents when its capacity runs out. On a ten-million-unit needle that is a
//! five-megabyte `memcpy` in the middle of the phase whose entire purpose is
//! that no step exceeds a stride, and it is invisible to every counter this
//! repository has, because it is a Rust-side allocation and
//! `--boundary`'s `allocs` and `words` columns count Cove's heap. That is
//! [issue #442](https://github.com/myuon/cove/issues/442)'s blind spot, in the
//! work that exists to have closed it.
//!
//! So the gate was wrong and has been amended, not worked around: fuel is an
//! upper bound on work done, not an equality — the intrinsic this replaces
//! charges "the receiver's whole length as an upper bound" and says so,
//! because `str::find` does not report how far it got. With the charge stated
//! as a bound, the algorithm may be the one with no table, and the blind spot
//! closes by construction rather than by a counter nobody can read.
//!
//! # What a turn is, what it is charged, and the bound
//!
//! [`Matcher::run`] is given a number of turns and takes at most that many.
//! **Every turn is one comparison of one pair of units**, in either phase, and
//! is charged one unit of work. That is a true upper bound on the units
//! examined — a turn reads at most two — and it is what a caller can count
//! without the matcher having to report where it stopped.
//!
//! The bound follows from the algorithm rather than from a measurement:
//!
//! - a maximal-suffix scan takes at most `2m` turns. Its state moves
//!   `start + at + offset` upwards by at least one every turn — the two
//!   advancing cases add exactly one, and the restarting case adds
//!   `1 - offset + (at - start)`, which is at least two because `offset` is
//!   always less than the period and the period never exceeds `at - start` —
//!   and that measure begins at 1 and never passes `2m`;
//! - there are two such scans, one per order, so `4m`;
//! - the periodicity test is `crit` turns, and `crit < m`;
//! - the search is Crochemore–Perrin's, which makes at most `2(n - from)`
//!   comparisons: in the periodic case the memory stops a unit being compared
//!   twice, and in the other the period exceeds half the needle, so two
//!   attempts never overlap.
//!
//! **`turns ≤ 5m + 2(n - from)`**, so the charge is under `5(m + (n - from))`
//! — `C = 5`, and every term of it is named above. `a_search_is_charged_within
//! _its_bound` pins it at a spread of shapes, and the differential suite
//! checks it again on every pair of its corpus.
//!
//! The two answers that examine nothing — an empty needle, and a needle longer
//! than what is left of the haystack — are the caller's, are reached before
//! any of this, and take no turn at all.

/// How many units one skip may step over: the eight a machine word holds.
const WINDOW: usize = 8;

/// Where `byte` first occurs in the low `take` units of `word`, which holds
/// eight units least-significant first.
///
/// The classic word-wide test: exclusive-or the word against `byte` repeated,
/// and a unit that matched is now zero. `(x - 0x01..01) & !x & 0x80..80` sets
/// the high bit of every zero unit — and of some units *above* one, because a
/// borrow runs upwards — but never of one below the lowest, so the **lowest**
/// set bit is the lowest match and is exact. That is why this answers
/// `trailing_zeros` rather than a mask a caller would have to unpick, and why
/// a caller may not read the other bits.
///
/// `take` is how many of the word's units are really the run's: a window at
/// the end of a haystack may be padded with whatever the heap left there, and
/// a needle whose critical unit is that padding would otherwise match it.
#[inline]
fn first_unit_at(word: u64, byte: u8, take: usize) -> Option<usize> {
    debug_assert!((1..=WINDOW).contains(&take));
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let x = word ^ (byte as u64).wrapping_mul(ONES);
    let mut found = x.wrapping_sub(ONES) & !x & HIGH;
    if take < WINDOW {
        found &= (1u64 << (take * 8)) - 1;
    }
    (found != 0).then(|| (found.trailing_zeros() / 8) as usize)
}

/// Where a maximal-suffix scan has got to.
///
/// The preparation's whole state, four words of it. Carried across a
/// safepoint like everything else here.
#[derive(Clone, Copy)]
struct Scan {
    /// The start of the best suffix found so far, which becomes the critical
    /// position.
    start: usize,
    /// The start of the candidate being compared against it.
    at: usize,
    /// How far into the two the comparison has got.
    offset: usize,
    /// The period of the best suffix found so far.
    period: usize,
}

impl Scan {
    const fn new() -> Self {
        Scan {
            start: 0,
            at: 1,
            offset: 0,
            period: 1,
        }
    }
}

/// Which of the matcher's four loops is running.
///
/// A phase change costs no turn — there are four of them in a whole search,
/// and each is a handful of arithmetic — but every phase's loop refuses to
/// turn once the budget is spent, so the span between two safepoints is
/// bounded whichever phase it falls in.
#[derive(Clone, Copy)]
enum Phase {
    /// A maximal-suffix scan. `greater` picks the order: the factorization
    /// needs the maximal suffix under each, and takes whichever has the later
    /// critical position.
    Suffix { greater: bool },
    /// Whether the factorization is periodic — whether the needle's left half
    /// is what lies a period into it — tested a unit at a time from `at`.
    Periodic { at: usize },
    /// The right half of an attempt, `probe` rising towards the needle's end.
    Right,
    /// The left half of an attempt, `probe` falling towards what the previous
    /// attempt already established.
    Left,
}

/// How far a matcher has got and what it is carrying, between two safepoints.
///
/// It owns no memory: no table, no copy of either run, nothing to allocate and
/// nothing to give back. What it has is this, and both runs are read through
/// the two closures [`Matcher::run`] is handed.
pub(crate) struct Matcher {
    /// The needle's length in units.
    m: usize,
    /// The haystack's length in units.
    n: usize,
    /// Where the search was asked to begin.
    from: usize,
    /// Turns taken, which is exactly what the caller charges.
    turns: u64,
    phase: Phase,
    /// The maximal-suffix scan in flight.
    scan: Scan,
    /// The first order's answer, held while the second order runs.
    earlier: (usize, usize),
    /// The critical position: where an attempt begins comparing.
    crit: usize,
    /// The unit at the critical position, read once when preparation ends.
    ///
    /// It is the one unit the skip asks the haystack about, so it is worth not
    /// reading out of the needle's run once an attempt.
    head: u8,
    /// The shift a fully-compared attempt makes.
    period: usize,
    /// Whether the factorization is periodic, which is whether an attempt
    /// carries memory of the last one across its shift.
    periodic: bool,
    /// The offset in the haystack the current attempt is at.
    pos: usize,
    /// How far the current half has compared.
    probe: usize,
    /// How much of the needle the previous attempt already established, which
    /// the left half stops at rather than comparing again.
    memory: usize,
}

impl Matcher {
    /// A search of `n` units for `m` of them, beginning at `from`.
    ///
    /// The caller has already answered the two questions this has no phase
    /// for: an empty needle, and a needle longer than `n - from`. Both are
    /// answered before any preparation begins, because there is no needle to
    /// prepare in the first and nothing to search in the second — and neither
    /// examines a unit, so neither is charged any bulk work.
    pub(crate) fn new(n: usize, m: usize, from: usize) -> Self {
        debug_assert!(m > 0 && from <= n && m <= n - from);
        Matcher {
            m,
            n,
            from,
            turns: 0,
            phase: Phase::Suffix { greater: false },
            scan: Scan::new(),
            earlier: (0, 1),
            crit: 0,
            head: 0,
            period: 1,
            periodic: false,
            pos: from,
            probe: 0,
            memory: 0,
        }
    }

    /// How much work this matcher has done, in the unit the caller charges.
    ///
    /// One per turn, and a turn is one comparison. The caller charges the
    /// *difference* after each step, which is what makes the charge follow the
    /// work rather than arrive after it.
    pub(crate) fn charge(&self) -> u64 {
        self.turns
    }

    /// The most turns a search of these lengths can take.
    ///
    /// `5m + 2(n - from)`, derived in the module's note: two maximal-suffix
    /// scans of at most `2m` turns each, a periodicity test of fewer than `m`,
    /// and Crochemore–Perrin's `2(n - from)` comparisons.
    ///
    /// `#[cfg(test)]` because nothing in a running machine needs it: a fuel
    /// bound is enforced by the meter a step at a time and does not have to
    /// know what a whole search will cost. What wants it is the case that pins
    /// it, here and in `vm::exec::encoded`, because a bound nothing checks is
    /// a sentence rather than a property.
    #[cfg(test)]
    pub(crate) fn bound(n: usize, m: usize, from: usize) -> u64 {
        5 * m as u64 + 2 * (n - from) as u64
    }

    /// Whether the needle is still being prepared.
    ///
    /// The phase a fuel bound stopped a run in is the one thing about this
    /// state a test outside it wants, and Decision 4's first case is about
    /// exactly that: a needle far longer than a stride must be interruptible
    /// while it is being prepared, not only once the haystack is being read.
    ///
    /// `#[cfg(test)]` for [`Matcher::bound`]'s reason: the phase is not a
    /// decision any caller makes, [`Matcher::run`] carries it.
    #[cfg(test)]
    pub(crate) fn preparing(&self) -> bool {
        matches!(self.phase, Phase::Suffix { .. } | Phase::Periodic { .. })
    }

    /// Begins an attempt at `pos`, carrying `memory` of the last one.
    ///
    /// The right half starts at the critical position, or at the memory where
    /// that is further in: a periodic needle whose previous attempt shifted by
    /// its period has already established everything before it.
    fn attempt(&mut self, pos: usize, memory: usize) {
        self.pos = pos;
        self.memory = memory;
        self.probe = self.crit.max(memory);
        self.phase = Phase::Right;
    }

    /// Takes at most `turns` turns and answers the search's answer once it has
    /// one.
    ///
    /// `None` means the budget ran out with the matcher part-way through: the
    /// caller charges what was done, polls, and calls this again. `Some` is
    /// final — an absolute unit offset into the haystack, or -1.
    ///
    /// `needle` answers one unit. `haystack` answers the units from a position
    /// to the end of the **eight-unit block** it lies in, least significant
    /// first: at least one, at most eight, never straddling two of the
    /// caller's machine words. Its low unit *is* the unit at that position, so
    /// an ordinary comparison and a skip ask the same question and a caller
    /// that caches the word it last touched answers both from one read. What
    /// lies past the run's own length may be anything; the skip masks it off
    /// and a comparison never looks past the low unit.
    pub(crate) fn run<N, H>(&mut self, turns: u64, needle: N, haystack: H) -> Option<i64>
    where
        N: FnMut(usize) -> u8,
        H: FnMut(usize) -> u64,
    {
        let mut left = turns;
        let answer = self.step(&mut left, needle, haystack);
        self.turns += turns - left;
        answer
    }

    /// [`Matcher::run`] with the budget in hand, so that every exit charges
    /// through one place.
    ///
    /// Each phase is a loop of its own rather than one turn of an outer
    /// `match`, and that is a measurement rather than a preference: a
    /// state machine re-dispatched per comparison measured **twice** the cost
    /// per unit of the straight loops it replaced, on a benchmark where the
    /// number of comparisons was the same. The arms keep their own hot state
    /// in locals where the borrow checker allows it and write it back at every
    /// exit.
    fn step<N, H>(&mut self, left: &mut u64, mut needle: N, mut haystack: H) -> Option<i64>
    where
        N: FnMut(usize) -> u8,
        H: FnMut(usize) -> u64,
    {
        loop {
            match self.phase {
                // The critical factorization: the maximal suffix of the needle
                // under each of the two orders, of which the one starting
                // later is the critical position and carries the period.
                Phase::Suffix { greater } => {
                    let mut scan = self.scan;
                    while scan.at + scan.offset < self.m {
                        if *left == 0 {
                            self.scan = scan;
                            return None;
                        }
                        *left -= 1;
                        let a = needle(scan.at + scan.offset);
                        let b = needle(scan.start + scan.offset);
                        if a == b {
                            // Through a repetition of the period so far.
                            if scan.offset + 1 == scan.period {
                                scan.at += scan.period;
                                scan.offset = 0;
                            } else {
                                scan.offset += 1;
                            }
                        } else if (a < b) != greater {
                            // The candidate is the smaller of the two in this
                            // order, so the best suffix keeps its start and
                            // the whole prefix so far becomes its period.
                            scan.at += scan.offset + 1;
                            scan.offset = 0;
                            scan.period = scan.at - scan.start;
                        } else {
                            // The candidate is the larger: start again from it.
                            scan.start = scan.at;
                            scan.at = scan.start + 1;
                            scan.offset = 0;
                            scan.period = 1;
                        }
                    }
                    self.scan = scan;
                    let found = (scan.start, scan.period);
                    if greater {
                        let (crit, period) = if self.earlier.0 > found.0 {
                            self.earlier
                        } else {
                            found
                        };
                        debug_assert!(crit + period <= self.m, "a period past the needle");
                        self.crit = crit;
                        self.period = period;
                        self.phase = Phase::Periodic { at: 0 };
                    } else {
                        self.earlier = found;
                        self.scan = Scan::new();
                        self.phase = Phase::Suffix { greater: true };
                    }
                }
                // Is the needle's left half what lies one period into it? If
                // it is, an attempt may remember what the last one matched
                // across the shift; if it is not, the period is replaced by
                // one past half the needle, which is what makes two attempts
                // unable to overlap.
                Phase::Periodic { at } => {
                    let mut at = at;
                    let mut periodic = true;
                    while at < self.crit {
                        if *left == 0 {
                            self.phase = Phase::Periodic { at };
                            return None;
                        }
                        *left -= 1;
                        if needle(at) != needle(self.period + at) {
                            periodic = false;
                            break;
                        }
                        at += 1;
                    }
                    self.periodic = periodic;
                    if !periodic {
                        self.period = self.crit.max(self.m - self.crit) + 1;
                    }
                    self.head = needle(self.crit);
                    self.attempt(self.from, 0);
                }
                // The right half, forwards from the critical position, one
                // whole attempt after another. A mismatch shifts past the
                // comparison that failed, which is the shift that makes the
                // algorithm skip rather than step.
                Phase::Right => loop {
                    if self.pos + self.m > self.n {
                        return Some(-1);
                    }
                    // **The skip.** An attempt begins by comparing the unit at
                    // the critical position, so a position whose unit there is
                    // not the needle's cannot match and may be stepped over —
                    // eight at a time with the word-wide zero test rather than
                    // one at a time. Only where there is no memory: a memory
                    // is a claim about *this* `pos`, and moving `pos` would
                    // make it false. `str::find` puts the same layer in front
                    // of the same algorithm, and measured, it is most of what
                    // the searching rows cost without it.
                    if self.memory == 0 && self.probe == self.crit {
                        if *left == 0 {
                            return None;
                        }
                        let at = self.pos + self.crit;
                        let window = haystack(at);
                        if (window & 0xFF) as u8 != self.head {
                            let block = WINDOW - at % WINDOW;
                            let take = block.min(self.n - at).min(*left as usize);
                            let ahead = match first_unit_at(window, self.head, take) {
                                Some(ahead) => {
                                    debug_assert!(ahead > 0, "the low unit was tested already");
                                    ahead
                                }
                                None => take,
                            };
                            self.pos += ahead;
                            *left -= ahead as u64;
                            continue;
                        }
                    }
                    while self.probe < self.m {
                        if *left == 0 {
                            return None;
                        }
                        *left -= 1;
                        if needle(self.probe) != (haystack(self.pos + self.probe) & 0xFF) as u8 {
                            break;
                        }
                        self.probe += 1;
                    }
                    if self.probe >= self.m {
                        self.probe = self.crit;
                        self.phase = Phase::Left;
                        break;
                    }
                    let shift = self.probe - self.crit + 1;
                    self.attempt(self.pos + shift, 0);
                },
                // The left half, backwards from the critical position, and it
                // stops at what the previous attempt established rather than
                // comparing it again.
                Phase::Left => loop {
                    if self.probe <= self.memory {
                        return Some(self.pos as i64);
                    }
                    if *left == 0 {
                        return None;
                    }
                    *left -= 1;
                    let at = self.pos + self.probe - 1;
                    if needle(self.probe - 1) == (haystack(at) & 0xFF) as u8 {
                        self.probe -= 1;
                    } else {
                        let memory = if self.periodic {
                            self.m - self.period
                        } else {
                            0
                        };
                        self.attempt(self.pos + self.period, memory);
                        break;
                    }
                },
            }
        }
    }
}

/// The same search over two slices, run to its answer in one call: the
/// tree-walking interpreter's side of `core.stringFind`, and the shape a test
/// can put beside `str::find`.
///
/// It is [`Matcher`] rather than a second algorithm, so the two execution
/// tiers cannot come to disagree about what the instruction means. What it
/// does not do is poll, because the interpreter has no instruction to be
/// part-way through: [`crate::interp`] is the reference tier, and the bound it
/// keeps is its own.
///
/// `from` is trusted to be in `0 ..= haystack.len()`; the caller refuses
/// anything else, as the instruction does.
pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> i64 {
    if needle.is_empty() {
        return from as i64;
    }
    if needle.len() > haystack.len() - from {
        return -1;
    }
    Matcher::new(haystack.len(), needle.len(), from)
        .run(u64::MAX, |at| needle[at], |at| window_of(haystack, at))
        .expect("an unbounded budget leaves no turn unspent")
}

/// The units of `run` from `at` to the end of the eight-unit block it lies in,
/// least-significant first — [`Matcher::run`]'s haystack reader over a slice.
///
/// Zero past the end of the run, which is why [`first_unit_at`] is told how
/// many units are really there.
#[inline]
fn window_of(run: &[u8], at: usize) -> u64 {
    let mut word = [0u8; WINDOW];
    let end = (at + WINDOW - at % WINDOW).min(run.len());
    word[..end - at].copy_from_slice(&run[at..end]);
    u64::from_le_bytes(word)
}

/// A global allocator for this crate's test binary that can count what one
/// thread allocates while it is asked to.
///
/// **It is here because a claim that a path allocates nothing has to be
/// observed rather than argued.** The matcher this file held before
/// Crochemore–Perrin allocated a table, and the table was invisible: a
/// Rust-side allocation is not a Cove-heap one, so `--boundary`'s `allocs`
/// column read the same with it as without, and that byte-identical column
/// was then cited as evidence that nothing had been allocated. It was not
/// evidence of anything. This is.
///
/// The count is **per thread** and off unless a thread has asked for it, which
/// is what makes it sound under `cargo test`'s parallelism: another test
/// allocating on another thread is not this one's business. The cell is
/// `const`-initialised and has no destructor, so reading it inside the
/// allocator cannot itself allocate or run during a thread's teardown.
///
/// Only the test build has this. A `#[global_allocator]` may be defined once
/// per binary, and a crate's `cfg(test)` binary is its own; the integration
/// tests link this crate built without `cfg(test)` and are unaffected.
#[cfg(test)]
pub(crate) mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        /// `None` when this thread is not counting.
        static COUNTED: Cell<Option<u64>> = const { Cell::new(None) };
    }

    struct Counting;

    /// One more allocation on this thread, if it is counting.
    #[inline]
    fn bump() {
        // `try_with` rather than `with`: a thread tearing down has no
        // thread-local left, and an allocator that panicked there would abort
        // a test run for a reason that has nothing to do with the test.
        let _ = COUNTED.try_with(|counted| {
            if let Some(so_far) = counted.get() {
                counted.set(Some(so_far + 1));
            }
        });
    }

    // Every arm that *obtains* memory counts; freeing does not. A `realloc` is
    // counted because it is the event this exists to catch: a table appended
    // to one entry at a time does not allocate per entry, it reallocates and
    // copies itself whole when its capacity runs out, and that is the
    // unbounded step inside a bounded phase.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump();
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            bump();
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            bump();
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Runs `body` and answers what it answered, and how many times this
    /// thread obtained memory while it ran.
    pub(crate) fn while_counting<R>(body: impl FnOnce() -> R) -> (R, u64) {
        let outer = COUNTED.with(|counted| counted.replace(Some(0)));
        let answer = body();
        let counted = COUNTED
            .with(|counted| counted.replace(outer))
            .expect("this thread was counting");
        (answer, counted)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The counter counts, and counts nothing when nothing is asked for —
        /// which is the assertion that stops `allocates nothing` passing
        /// because the counter was never wired to anything.
        #[test]
        fn the_counter_sees_an_allocation_and_only_inside_its_own_region() {
            let (_, none) = while_counting(|| 1 + 1);
            assert_eq!(none, 0, "arithmetic allocates nothing");
            let (held, some) = while_counting(|| vec![0u8; 64]);
            assert!(some >= 1, "a `Vec` of 64 bytes allocated {some} time(s)");
            drop(held);
            // And a growth that reallocates is counted as the event it is.
            let (_, grown) = while_counting(|| {
                let mut held: Vec<u8> = Vec::new();
                for at in 0..4096u32 {
                    held.push(at as u8);
                }
                held.len()
            });
            assert!(grown > 1, "a growing `Vec` reallocated {grown} time(s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `str::find` answers for the same bytes, which is the oracle every
    /// case here is written against.
    fn rust_find(haystack: &[u8], needle: &[u8], from: usize) -> i64 {
        if needle.is_empty() {
            return from as i64;
        }
        if from > haystack.len() || needle.len() > haystack.len() - from {
            return -1;
        }
        haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map_or(-1, |at| (at + from) as i64)
    }

    /// The answer and the turns it took, so that the bound is checked wherever
    /// the answer is.
    fn found(haystack: &[u8], needle: &[u8], from: usize) -> (i64, u64) {
        if needle.is_empty() {
            return (from as i64, 0);
        }
        if needle.len() > haystack.len() - from {
            return (-1, 0);
        }
        let mut matcher = Matcher::new(haystack.len(), needle.len(), from);
        let answer = matcher
            .run(u64::MAX, |at| needle[at], |at| window_of(haystack, at))
            .expect("an unbounded budget leaves no turn unspent");
        let bound = Matcher::bound(haystack.len(), needle.len(), from);
        assert!(
            matcher.charge() <= bound,
            "{} turn(s) for m={} n={} from={}, past the bound of {bound}",
            matcher.charge(),
            needle.len(),
            haystack.len(),
            from
        );
        (answer, matcher.charge())
    }

    /// A haystack whose bytes are not text: `0x00` and bytes at and over
    /// `0x80` are exactly what a `String`'s payload holds inside a multi-byte
    /// character, and the instruction is defined over bytes rather than over
    /// them.
    ///
    /// Two-way is a great deal harder to get right than the matcher this
    /// replaced — a critical factorization, two orders, a periodic case and a
    /// memoryless one — so this corpus is the load-bearing test of the change
    /// and is deliberately wider than the one before it: periodic needles at
    /// several periods, needles whose two maximal suffixes differ, needles
    /// that are their own haystack, near misses at every offset, and the
    /// degenerate one-unit and all-one-unit shapes where `crit` is 0.
    fn corpus() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"b".to_vec(),
            b"aa".to_vec(),
            b"ab".to_vec(),
            b"ba".to_vec(),
            b"aab".to_vec(),
            b"aba".to_vec(),
            b"baa".to_vec(),
            b"aaaaaaaaaa".to_vec(),
            b"abababababab".to_vec(),
            b"aabaabaabaab".to_vec(),
            b"aabaabaaabaab".to_vec(),
            b"abcabcabdabcabc".to_vec(),
            b"abcabd".to_vec(),
            b"aaaaaaaaab".to_vec(),
            b"baaaaaaaaa".to_vec(),
            b"banana banana bandana".to_vec(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            b"mississippi".to_vec(),
            b"issi".to_vec(),
            vec![0x00, 0x01, 0x00, 0x00, 0x01, 0x02],
            vec![
                0xff, 0x80, 0xc3, 0xa9, 0xe3, 0x81, 0x82, 0xf0, 0x9f, 0x98, 0x80,
            ],
            vec![0x80; 40],
            vec![0x00; 9],
        ];
        // Many near misses and a long repeated prefix: the shape a quadratic
        // scan takes quadratic time on and a wrong shift answers wrongly on.
        let mut near = Vec::new();
        for _ in 0..40 {
            near.extend_from_slice(b"aaaaaaab");
        }
        near.extend_from_slice(b"aaaaaaaa");
        out.push(near);
        // A needle far longer than one safepoint step, and a haystack that
        // holds it at the last offset it fits at.
        let long: Vec<u8> = (0..3079u32).map(|at| (at % 251) as u8).collect();
        let mut with_long = vec![0x7fu8; 2051];
        with_long.extend_from_slice(&long);
        out.push(long);
        out.push(with_long);
        // A long periodic needle, which is the case the memory exists for, and
        // a haystack that holds it once at the end.
        let periodic: Vec<u8> = b"abcab".repeat(400);
        let mut around = b"abcab".repeat(300);
        around.pop();
        around.extend_from_slice(&periodic);
        out.push(periodic);
        out.push(around);
        out
    }

    /// **The matcher answers what `str::find` answers, for every pair of the
    /// corpus.**
    ///
    /// Two-way is the hardest thing in this file to be sure of by reading, so
    /// this is the case that has to be wide: every haystack against every
    /// needle, with the turn bound checked on each as well.
    #[test]
    fn a_find_agrees_with_rust_over_the_corpus() {
        let corpus = corpus();
        let mut checked = 0;
        for haystack in &corpus {
            for needle in &corpus {
                assert_eq!(
                    found(haystack, needle, 0).0,
                    rust_find(haystack, needle, 0),
                    "haystack {} byte(s), needle {} byte(s)",
                    haystack.len(),
                    needle.len()
                );
                checked += 1;
            }
        }
        assert_eq!(checked, corpus.len() * corpus.len());
    }

    /// Every `from` in range, for the pairs where a start offset can move the
    /// answer.
    #[test]
    fn a_find_agrees_with_rust_at_every_start() {
        let haystacks: Vec<Vec<u8>> = vec![
            b"aaaaaaaaaa".to_vec(),
            b"abababababab".to_vec(),
            b"aabaabaabaab".to_vec(),
            b"abcabcabdabcabc".to_vec(),
            b"banana banana bandana".to_vec(),
            b"mississippi".to_vec(),
            vec![0x00, 0x80, 0x00, 0x80, 0x00],
        ];
        let needles: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"aa".to_vec(),
            b"aaaa".to_vec(),
            b"abab".to_vec(),
            b"aabaab".to_vec(),
            b"abcabd".to_vec(),
            b"ana".to_vec(),
            b"issi".to_vec(),
            vec![0x80, 0x00],
        ];
        for haystack in &haystacks {
            for needle in &needles {
                for from in 0..=haystack.len() {
                    assert_eq!(
                        found(haystack, needle, from).0,
                        rust_find(haystack, needle, from),
                        "haystack {haystack:?}, needle {needle:?}, from {from}"
                    );
                }
            }
        }
    }

    /// Every needle of every length cut out of one haystack, found where it
    /// was cut from or earlier — an exhaustive sweep over factorizations that
    /// no hand-written corpus reaches.
    #[test]
    fn every_substring_of_a_haystack_is_found() {
        let haystacks: [&[u8]; 4] = [
            b"abaabbabaababbaab",
            b"aaaaaaaaaaaa",
            b"abcabcabcabd",
            &[0x00, 0x80, 0xff, 0x00, 0x80, 0xff, 0x00, 0x80],
        ];
        for haystack in haystacks {
            for at in 0..haystack.len() {
                for end in at + 1..=haystack.len() {
                    let needle = &haystack[at..end];
                    assert_eq!(
                        found(haystack, needle, 0).0,
                        rust_find(haystack, needle, 0),
                        "haystack {haystack:?}, needle {needle:?}"
                    );
                }
            }
        }
    }

    /// A deterministic pseudorandom source, so that a sweep is a sweep and not
    /// a different sweep every run. Xorshift64, which is four lines and has no
    /// dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn upto(&mut self, past: usize) -> usize {
            (self.next() % past as u64) as usize
        }
    }

    /// **Thousands of pairs over a small alphabet, at every start.**
    ///
    /// A hand-written corpus is a list of the cases its author thought of, and
    /// this one was demonstrably short: a matcher broken to carry its periodic
    /// memory into the *non*-periodic case — which stops the backward half
    /// early and can answer a match that is not there — passed every case
    /// above. It does not pass this one.
    ///
    /// Two, three and four symbols, because that is where periodic
    /// factorizations and near misses are dense; needles cut out of the
    /// haystack as often as not, so that half the cases have an answer; and a
    /// start anywhere in range, which is the axis the fixed corpus sweeps only
    /// for a handful of pairs.
    #[test]
    fn a_find_agrees_with_rust_over_pseudorandom_runs() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut checked = 0;
        for alphabet in [2usize, 3, 4] {
            for _ in 0..4_000 {
                let n = 1 + rng.upto(64);
                let haystack: Vec<u8> = (0..n).map(|_| b'a' + rng.upto(alphabet) as u8).collect();
                let m = 1 + rng.upto(n.min(12));
                let needle: Vec<u8> = if rng.upto(2) == 0 {
                    let at = rng.upto(n - m + 1);
                    haystack[at..at + m].to_vec()
                } else {
                    (0..m).map(|_| b'a' + rng.upto(alphabet) as u8).collect()
                };
                let from = rng.upto(n + 1);
                assert_eq!(
                    found(&haystack, &needle, from).0,
                    rust_find(&haystack, &needle, from),
                    "haystack {haystack:?}, needle {needle:?}, from {from}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 12_000);
    }

    /// A needle at offset 0, at the last offset it fits at, and equal to the
    /// haystack — the three positions an off-by-one in the shift moves.
    #[test]
    fn a_find_answers_at_both_ends_and_for_the_whole() {
        let haystack = b"abcdefghij";
        assert_eq!(found(haystack, b"abc", 0).0, 0);
        assert_eq!(found(haystack, b"hij", 0).0, 7);
        assert_eq!(found(haystack, haystack, 0).0, 0);
        assert_eq!(found(haystack, b"hijk", 0).0, -1);
        assert_eq!(found(haystack, b"j", 0).0, 9);
        assert_eq!(found(haystack, b"", 10).0, 10);
        assert_eq!(found(haystack, b"a", 10).0, -1);
    }

    /// The same answer however the turns are sliced, which is the property a
    /// safepoint between two of them rests on. Every budget from one turn to
    /// past the whole search, over pairs whose preparation and search both
    /// take several — including the periodic case, whose memory is the state
    /// most easily lost across a stop.
    #[test]
    fn a_stopped_matcher_answers_what_an_unstopped_one_does() {
        let haystack: Vec<u8> = {
            let mut out = Vec::new();
            for _ in 0..20 {
                out.extend_from_slice(b"aaaaaaab");
            }
            out.extend_from_slice(b"aabaabaab");
            out.extend_from_slice(&b"abcab".repeat(9));
            out
        };
        let needles: Vec<&[u8]> = vec![
            b"aabaab",
            b"aaaaaaab",
            b"aaab",
            b"zzz",
            b"b",
            b"abcababcab",
            b"abcabd",
        ];
        for needle in needles {
            let whole = found(&haystack, needle, 0).0;
            for turns in 1..=40u64 {
                let mut matcher = Matcher::new(haystack.len(), needle.len(), 0);
                let mut steps = 0;
                let got = loop {
                    steps += 1;
                    assert!(steps < 1_000_000, "a stepped matcher made no progress");
                    if let Some(answer) =
                        matcher.run(turns, |at| needle[at], |at| window_of(&haystack, at))
                    {
                        break answer;
                    }
                };
                assert_eq!(got, whole, "needle {needle:?} at {turns} turn(s) a step");
            }
        }
    }

    /// The phase changes once the needle has been prepared, and a fuel bound's
    /// test reads that to say which phase a stopped run was in.
    #[test]
    fn a_matcher_leaves_preparation_before_it_reads_the_haystack() {
        let haystack = [7u8; 400];
        let needle = [3u8; 90];
        let mut matcher = Matcher::new(haystack.len(), needle.len(), 0);
        assert!(matcher.preparing());
        let mut turns = 0;
        while matcher.preparing() {
            turns += 1;
            assert!(turns < 1_000, "preparation did not finish");
            matcher.run(1, |at| needle[at], |at| window_of(&haystack, at));
        }
        // Preparation is bounded by the needle and by nothing else: it has not
        // looked at the haystack, and it is over well inside `5m`.
        assert!(matcher.charge() <= 5 * needle.len() as u64);
    }

    /// **The charge is a bound and is stated as one.**
    ///
    /// The gate this replaces asked for `m + (n - from)` exactly, and an exact
    /// charge is what forced a table into a matcher that has no other use for
    /// one. What a fuel bound needs is an upper bound on the work done, which
    /// this is: `5m + 2(n - from)`, every term of it named in the module's
    /// note. Pinned over a spread of shapes — periodic and not, matching and
    /// not, at several starts — and checked again on every pair of the
    /// corpus above.
    #[test]
    fn a_search_is_charged_within_its_bound() {
        let shapes: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (vec![b'B'; 4096], vec![b'A'; 300]),
            (vec![b'a'; 4096], vec![b'a'; 300]),
            (b"ab".repeat(2048), b"ab".repeat(150)),
            (b"abcab".repeat(800), b"abcab".repeat(40)),
            (b"aaaaaaab".repeat(500), b"aaaaaaaa".to_vec()),
            (
                (0..4096u32).map(|at| (at % 251) as u8).collect(),
                vec![250u8, 0, 1],
            ),
            (vec![b'x'; 1000], vec![b'x'; 1000]),
        ];
        for (haystack, needle) in shapes {
            for from in [0usize, 1, 17] {
                if needle.len() > haystack.len() - from {
                    continue;
                }
                let (_, turns) = found(&haystack, &needle, from);
                let bound = Matcher::bound(haystack.len(), needle.len(), from);
                assert!(
                    turns <= bound,
                    "m={} n={} from={from}: {turns} turn(s) past {bound}",
                    needle.len(),
                    haystack.len()
                );
                // And the charge is really proportional rather than a
                // constant: a search of four thousand units cannot be a
                // handful of turns.
                assert!(turns > 0, "a search that examined nothing");
            }
        }
    }

    /// A search that runs to the end of a haystack it never matches in grows
    /// with the haystack and stays inside the bound.
    ///
    /// **What it does not do is skip, and that is worth writing down.**
    /// Two-way shifts by `probe - crit + 1`, so a mismatch at the critical
    /// position itself shifts by **one** — and on a haystack that shares no
    /// unit with the needle, every attempt mismatches there. So the turns are
    /// about `n` whatever the needle is: a needle of one repeated unit and a
    /// needle of forty distinct ones take the same number, measured, because
    /// both fail at their first comparison every time. Crochemore–Perrin's
    /// `O(n + m)` is a bound on comparisons and not a promise to examine few
    /// of them, and it is exactly why `str::find` puts a word-wide skip in
    /// front of the same algorithm rather than relying on its shifts.
    #[test]
    fn a_whole_scan_is_charged_in_proportion_to_what_it_walked() {
        let same = vec![b'A'; 40];
        let distinct: Vec<u8> = (0..40u8).map(|at| b'a' + at).collect();
        let mut walked = Vec::new();
        for n in [1_000usize, 4_000, 16_000] {
            let haystack = vec![b'B'; n];
            for needle in [&same, &distinct] {
                let (answer, turns) = found(&haystack, needle, 0);
                assert_eq!(answer, -1);
                assert!(turns > 0, "a search that examined nothing");
                assert!(
                    turns <= Matcher::bound(n, needle.len(), 0),
                    "n={n}, m={}: {turns} turn(s)",
                    needle.len()
                );
            }
            walked.push(found(&haystack, &same, 0).1);
        }
        // Linear in the haystack and not better: sixteen times the haystack is
        // about sixteen times the turns.
        let (small, large) = (walked[0], walked[2]);
        assert!(
            large > small * 12 && large < small * 20,
            "{small} turn(s) at n=1,000 against {large} at n=16,000"
        );
    }
}
