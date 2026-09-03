//! One run's linear memory: a stack region divided into one segment per task,
//! an object heap above them, and one kind of address that names a word in any
//! of them.
//!
//! [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md) decides
//! that every Cove runtime value lives in one linear memory, and
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) writes the address
//! model out. This module is that memory and nothing more: it stores words,
//! hands out frames and objects, and reclaims the objects nothing reaches. It
//! does not know what a frame *means*, what an instruction is, or which slots
//! of a frame hold references. Those are facts about a program, and the only
//! one of them that reaches here arrives through [`Roots`].
//!
//! # One address space, two owners
//!
//! A linear address is a word index. `[0, STACK_WORDS)` is the stack region;
//! everything at or above [`STACK_WORDS`] is the heap region. [`is_stack`] is
//! the whole of the decoder, and it is the only thing anywhere that knows the
//! regions are currently separate Rust allocations.
//!
//! ADR 0034 permits that split as a temporary implementation state on one
//! condition: no address encoding, lowered layout, GC map or public API may
//! expose it. Nothing does, because **no address changes value when the
//! regions are later placed in one block** — a heap object is at
//! `STACK_WORDS + its offset within the heap` under either arrangement, and a
//! stack word is at its own index under both. Moving to one block is then a
//! change to a few indexing expressions, not a representation migration.
//!
//! Addresses are indices rather than pointers, which is what lets a region
//! reallocate as it grows while every live address stays correct. A growable
//! stack and a non-moving heap coexist with no fixup pass over anything.
//!
//! What is *owned* by whom is the part that answers issue #240's Q1. The stack
//! region is divided statically into [`SEGMENTS`] segments of
//! [`SEGMENT_WORDS`] words, and **a task owns one segment**: its frames are a
//! `Vec<u64>` nothing else can reach, addressed from the segment's origin. The
//! heap is a [`Space`] behind an `Arc`, and **a run has exactly one**, shared
//! by every task thread. [`Memory`] is one task's view of the pair.
//!
//! Two addresses formed in two tasks cannot be confused, and the reason is
//! arithmetic rather than a convention anybody has to keep: segment `k` is
//! `[k * SEGMENT_WORDS, (k + 1) * SEGMENT_WORDS)`, the ranges are disjoint by
//! construction, and a frame is refused the moment it would leave its own. The
//! decoder does not change at all — `addr < STACK_WORDS` still says which
//! region a word is in, and which *segment* is a question only the task that
//! owns one ever asks.
//!
//! # What the heap synchronises, and what it does not
//!
//! Q1 says to synchronise where correctness requires it and to leave the
//! single-task allocation path alone until a measurement asks for it. Three
//! things require it and nothing else does:
//!
//! - **Handing out words.** The bump pointer and the free list are one
//!   [`Mutex`]; two tasks cannot be given the same run of words.
//! - **Reading and writing a word.** The heap's words are [`AtomicU64`] and
//!   every ordinary access is `Relaxed`, which is a plain load or store on
//!   every target this runs on. What makes a value written by one task visible
//!   to another is the release/acquire pair on the lock word of the `Shared`
//!   cell it was written through — see [`crate::lvm::cell`] — because
//!   `Shared` is the only way the language lets two tasks reach one value.
//! - **Collecting.** A non-moving mark and sweep still may not run while
//!   another task is between two instructions, because that task's roots are
//!   in its own frames. So a collection is stop-the-world: the collector asks,
//!   every other task publishes its roots at its next safepoint and parks, and
//!   the sweep runs when they all have.
//!
//! The heap does **not** synchronise a value. Two tasks reaching one object
//! without a `Shared` between them is a program the task-safety rule already
//! refuses, and paying for it here would be paying for it on every access in
//! every program.
//!
//! # What that cost, measured
//!
//! Q1 asks for a measurement rather than an argument, so here is the one that
//! was taken: `benches/` on `--backend lvm`, the minimum of ten interleaved
//! runs of two builds of this workspace that differ only in this module.
//!
//! | row | what it does | delta |
//! |---|---|---|
//! | `arith`, `field`, `pure`, `call` | never leaves a frame | −1.4% to +0.9%, which is the noise |
//! | `arrayget` | one element load a turn | +0.4% to +2.4% |
//! | `method` | a method on a struct a turn | +3.6% to +5.4% |
//! | `chars` | ~1.9M allocations and a string read a turn | +5.2% to +6.3% |
//!
//! Two runs of the same pair are quoted rather than one, because the machine
//! this was taken on was building something else at the time and a single
//! figure would be claiming a precision the numbers do not have. What the two
//! runs agree on is the shape: nothing on a row that stays in a frame, a few
//! per cent on a row that reads the heap, and the most on the row that
//! allocates.
//!
//! Two things are worth having recorded rather than re-derived.
//!
//! **The remaining cost on the allocation-heavy row is the allocator's lock,
//! and nothing else.** A build with a second, entirely redundant lock and
//! unlock added to [`Space::alloc`] moved `chars` from +5.4% to +11.1% — the
//! delta doubles when the number of lock acquisitions doubles, which is what
//! attribution looks like. Roughly sixteen nanoseconds an allocation. That is
//! exactly the cost Q1 says to leave alone until a measurement asks, and the
//! measurement now says what a per-task allocation buffer would be worth if
//! one is ever wanted.
//!
//! **A one-word copy is worth its own path.** Before [`Memory::copy_words`]
//! answered a single word without splitting it across chunks, `arrayget` was
//! +10.4% and `method` +9.1%. One width is most of the widths.
//!
//! The alternative to a chunked store — one `Box<[AtomicU64]>` allocated to
//! the whole budget at the start of the run — was built and measured too. It
//! bought two of the remaining points on `chars` and cost sixteen milliseconds
//! of zeroing on *every* run, which is +22% on `arith` and +178% on `pure`. A
//! short program is the common case; the chunks stay.
//!
//! # A payload is a run of words, like a frame
//!
//! A value is one or more consecutive words, and both regions say so the same
//! way: a frame's value location is a base slot and a layout, and a heap
//! object's payload is a word array the same kind of layout describes. So a
//! struct in an array element or a closure environment is inline in that
//! payload, and the collector walks it with the map it would walk a frame
//! with. That is why the only thing this module knows about *values* is how
//! to copy and clear runs of words — [`Memory::copy_words`] is the whole of
//! ADR 0001's field-wise shallow copy — and why tracing is a walk of static
//! per-word `Repr`s rather than a question asked of each object.
//!
//! # Why the collector does not move objects
//!
//! An assignable expression lowers to a `Repr::Addr` word holding the address
//! of one mutable word — often a word *inside* an object. A moving collector
//! would have to find and rewrite every one of those, which means either
//! knowing where they all are or understanding interior pointers. A
//! non-moving collector needs neither: the address a `var` argument carries is
//! as correct after a collection as before it, and the collector never sees
//! that it exists. Keeping the base object alive for the address's live range
//! is the lowering's job, not this module's.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use cove_lir::{Layout, LayoutId, Repr, Shape, SHARED_VALUE};

/// The words one task's stack segment reserves.
///
/// One mebiword, eight mebibytes. The number is an implementation choice and
/// deliberately not a language fact: the tree-walking oracle and this machine
/// represent a frame differently and will run out at different depths, so
/// requiring them to agree on a depth would be requiring one of them to
/// represent a frame the other's way. What they must agree on is the *way*
/// they fail — a stack-overflow runtime error, with a span, deterministically,
/// inside the run's memory budget.
///
/// Reserved is not committed. A segment's backing store grows on demand and a
/// task that never nests deeply never pays for it, so the cost of reserving
/// one per task is a range of the index space and nothing else.
pub(crate) const SEGMENT_WORDS: u64 = 1 << 20;

/// How many stack segments the address space reserves, and so how many tasks
/// of one run may be executing at once.
///
/// Four thousand and ninety-six. This is a thread-per-task runtime
/// ([ADR 0008](../../../../docs/adr/0008-concurrent-task-execution.md)), so a
/// run with more live tasks than this has more live *threads* than any
/// operating system will schedule usefully; the limit is reached by a program
/// that was already going to fail, and it fails here with a name instead. It
/// costs nothing to reserve, because a segment is an index range until a task
/// writes a frame into it.
pub(crate) const SEGMENTS: u64 = 1 << 12;

/// The words reserved for the stack region, `[0, STACK_WORDS)`.
///
/// Every segment, back to back. What it buys — beyond the region decoder — is
/// that no heap object is ever placed below it, so address `0` is a stack
/// word, can never name an object, and is free to mean null for `Repr::Ref`.
pub(crate) const STACK_WORDS: u64 = SEGMENT_WORDS * SEGMENTS;

/// The most words the heap region may hold.
///
/// A reclaimed run of words describes its own length in its header's 32-bit
/// `len` field, so a heap the sweeper can walk is one whose largest possible
/// free run fits there. Thirty-two gibibytes of Cove objects is far past the
/// point where a bump allocator and a stop-the-world mark and sweep were the
/// right answer, so the cap costs nothing that this allocator could have
/// delivered anyway.
const MAX_HEAP_WORDS: u64 = u32::MAX as u64;

/// The words one chunk of the heap's backing store holds.
///
/// Eight kibiwords, sixty-four kibibytes. The heap is a fixed spine of chunks
/// created on demand rather than one growable `Vec`, because a `Vec` cannot
/// grow while another thread holds a reference into it and this store is read
/// by every task at once. A chunk is committed by the allocator, under the
/// allocator's own lock, and never replaced — so a word read is two indexings
/// and an atomic load, and never a lock.
const CHUNK_SHIFT: u32 = 13;
const CHUNK_WORDS: u64 = 1 << CHUNK_SHIFT;
const CHUNK_MASK: u64 = CHUNK_WORDS - 1;

/// Whether a linear address names a word of the stack region.
///
/// This one comparison is the entire region decoder, and the entire knowledge
/// that the regions live in separate allocations. See the module docs.
#[inline]
pub(crate) fn is_stack(addr: u64) -> bool {
    addr < STACK_WORDS
}

/// The first address of stack segment `at`.
#[inline]
pub(crate) fn segment_origin(at: u32) -> u64 {
    at as u64 * SEGMENT_WORDS
}

/// The header word of an object of `layout` whose length field is `len`.
///
/// `len` means whatever the layout says it means — a byte count for a string,
/// an *element* count for an array or a set whatever an element's width is,
/// the width of what it holds for a box, and nothing at all for a struct. The
/// header carries it rather than the payload word count because
/// [`Layout::payload_words`] can always recover the second from the first, and
/// the first is what a program asks for.
#[inline]
pub(crate) fn header(layout: LayoutId, len: u32) -> u64 {
    ((layout.0 as u64) << 32) | len as u64
}

/// The layout named by a header word.
#[inline]
pub(crate) fn header_layout(word: u64) -> LayoutId {
    LayoutId((word >> 32) as u32)
}

/// The length field of a header word.
#[inline]
pub(crate) fn header_len(word: u64) -> u32 {
    word as u32
}

/// A frame did not fit in the reserved stack region.
///
/// Carries nothing: the depth at which it happened is a property of the
/// implementation's frame sizes and says nothing a program can act on, and the
/// span the error is reported at belongs to the caller, which knows what it was
/// about to call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Overflow;

/// A run has no stack segment left to give a new task.
///
/// Distinct from [`Overflow`], which is one task nesting too deeply. This one
/// is a run with [`SEGMENTS`] tasks already executing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoSegment;

/// Something that can name every reference the program currently holds.
///
/// The collector asks rather than knows, because what a root *is* is a fact
/// about a program and this module has none. The dispatch loop walks its live
/// frames with each function's `RefMap` and yields the non-null words; nothing
/// about frames, slots or maps appears here.
///
/// A walk may be asked for more than once and must yield the same addresses
/// each time. It may yield an address twice — the collector marks before it
/// traces, so a duplicate costs a bit test.
pub(crate) trait Roots {
    /// Calls `f` once for every heap address reachable without tracing.
    fn each_root(&self, f: &mut dyn FnMut(u64));
}

/// No roots at all.
///
/// What a task publishes before it has run an instruction, and what a fixture
/// with nothing to keep alive hands the collector.
pub(crate) struct NoRoots;

impl Roots for NoRoots {
    fn each_root(&self, _f: &mut dyn FnMut(u64)) {}
}

/// What one collection did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Collected {
    /// Words that held an object at the start of this collection and are free
    /// after it, headers included.
    pub(crate) freed_words: u64,
    /// Words held by objects that survived, headers included.
    pub(crate) live_words: u64,
    /// How many collections this memory has run, this one counted.
    pub(crate) collections: u64,
}

// --- the heap's backing store -----------------------------------------------

/// The heap region's words, as a spine of chunks created on demand.
///
/// A `Vec<u64>` cannot be this store, and the reason is not performance: the
/// run's tasks all hold `&Space`, a `Vec` needs `&mut` to grow, and a growing
/// `Vec` moves the words a reader is holding a reference into. A fixed spine
/// of `OnceLock` chunks needs no `&mut` to commit one and never moves a word
/// that exists, so growth and concurrent reads are not in each other's way.
///
/// The words are [`AtomicU64`] for the reason a shared store has to be: two
/// tasks writing two different objects is an ordinary thing for a program to
/// do, and without atomics it is a data race whatever the program meant.
/// Every ordinary access is `Relaxed`, which costs nothing beyond a plain load
/// or store; the ordering that makes one task's writes visible to another is
/// the release/acquire pair on a `Shared` cell's lock word, which is the only
/// way the language lets a value be reached from two tasks at all.
struct Words {
    /// One entry per chunk of the run's heap budget, sized once and never
    /// resized. An entry is empty until the allocator commits it.
    chunks: Box<[OnceLock<Box<[AtomicU64]>>]>,
}

impl Words {
    /// A store that can hold `capacity` heap words, none of them committed.
    fn new(capacity: u64) -> Words {
        let chunks = capacity.div_ceil(CHUNK_WORDS) as usize;
        Words {
            chunks: (0..chunks).map(|_| OnceLock::new()).collect(),
        }
    }

    /// The word at heap offset `index`, which must be committed.
    #[inline]
    fn at(&self, index: u64) -> &AtomicU64 {
        let chunk = self.chunks[(index >> CHUNK_SHIFT) as usize]
            .get()
            .expect("a committed heap word is in a chunk that exists");
        &chunk[(index & CHUNK_MASK) as usize]
    }

    /// The rest of the chunk `index` falls in, and where in it `index` is.
    ///
    /// What a run of words is copied through: a run that crosses a chunk
    /// boundary is a few slices rather than a lookup per word.
    #[inline]
    fn run(&self, index: u64) -> &[AtomicU64] {
        let chunk = self.chunks[(index >> CHUNK_SHIFT) as usize]
            .get()
            .expect("a committed heap word is in a chunk that exists");
        &chunk[(index & CHUNK_MASK) as usize..]
    }

    /// Commits every chunk holding a word in `[from, to)`.
    ///
    /// Called by the allocator, under its lock, on the one path that makes a
    /// heap word exist.
    fn commit(&self, from: u64, to: u64) {
        if to == 0 {
            return;
        }
        for at in (from >> CHUNK_SHIFT)..=((to - 1) >> CHUNK_SHIFT) {
            self.chunks[at as usize].get_or_init(chunk);
        }
    }
}

/// One committed chunk, zeroed.
fn chunk() -> Box<[AtomicU64]> {
    (0..CHUNK_WORDS).map(|_| AtomicU64::new(0)).collect()
}

// --- the allocator ----------------------------------------------------------

/// What handing out and reclaiming words needs, all of it behind one lock.
struct Alloc {
    /// The first heap offset no object occupies.
    bump: u64,
    /// The first address the heap may not reach.
    limit: u64,
    /// Free blocks, by address. Rebuilt by every sweep, consumed by
    /// [`Space::alloc`].
    free: Vec<u64>,
    /// One mark bit per heap word, live only for the duration of a collection.
    ///
    /// Beside the heap rather than in the header because of how differently
    /// the two are read: a header is read on every field access, and a mark bit
    /// is read once per object per collection. Putting the bit in the header
    /// would spend a mask on the hot path to save an allocation on the cold
    /// one, and it would put a word the program can reach and a word only the
    /// collector may touch in the same place.
    marks: Vec<u64>,
    allocated_words: u64,
    collections: u64,
}

// --- stopping the world -----------------------------------------------------

/// What one task is doing about a collection.
#[derive(Default)]
struct Party {
    /// Whether a task is executing over this segment.
    live: bool,
    /// The roots it published on its way to a safepoint, while it is at one.
    ///
    /// `Some` is the whole of "arrived": the collector counts these and reads
    /// them, and a task that stays at a safepoint — waiting on a cell, or
    /// inside a host call — leaves them published until it is released. A
    /// running task's are `None`, because a running task's frames change
    /// between two instructions and a snapshot of them would be a lie by the
    /// time the collector read it.
    at: Option<Vec<u64>>,
}

/// The run's stop-the-world state.
struct Stw {
    /// One entry per stack segment ever handed out.
    parties: Vec<Party>,
    /// How many of them are live.
    live: usize,
    /// Whether a collection is running, or waiting for the others to arrive.
    collecting: bool,
    /// What the last collection did, for a task that waited one out instead of
    /// running one of its own.
    last: Collected,
}

impl Stw {
    /// How many live parties other than `me` are at a safepoint.
    fn arrived(&self, me: u32) -> usize {
        self.parties
            .iter()
            .enumerate()
            .filter(|(at, party)| *at != me as usize && party.live && party.at.is_some())
            .count()
    }
}

// --- the run's half ---------------------------------------------------------

/// The run's one linear address space: its object heap, and the ledger of
/// which stack segments are taken.
///
/// One per run, held by every task's [`Memory`] behind an `Arc`. It is the
/// whole of what issue #240's Q1 decides — *one heap per run, shared by the
/// run's task threads* — and it is the only thing in this backend that two
/// threads touch at once.
///
/// It holds no Cove value that is not in the heap. The stack-segment ledger is
/// a bitmap's worth of booleans, and the stop-the-world state holds addresses
/// a task published as roots, which are names for objects the heap already
/// owns rather than a second place to keep one. That is the same distinction
/// [`crate::lvm::exec::Machine`]'s interned string table rests on, and it is
/// what keeps this from being the second value store ADR 0034 forbids.
pub(crate) struct Space {
    words: Words,
    /// The bump pointer, for the readers that hold no lock: the collector's
    /// own bounds checks, and the debug assertions that say an address names
    /// an object that exists.
    bump: AtomicU64,
    alloc: Mutex<Alloc>,
    stw: Mutex<Stw>,
    /// Mirrors `Stw::collecting` so that a safepoint can ask without a lock.
    /// A safepoint runs every thousand instructions and a collection is rare,
    /// so the question has to be cheap when the answer is no.
    pending: AtomicBool,
    /// Woken when a party arrives, when a collection ends, and when a task
    /// leaves.
    turn: Condvar,
    /// One bucket for every task waiting on a word of this memory.
    ///
    /// A cell's lock is a word of the cell ([`crate::lvm::cell`]), so waiting
    /// for one is waiting for a word to change. One bucket wakes more waiters
    /// than it has to; it holds nothing and names nothing, and splitting it is
    /// a change no caller sees.
    waiting: Mutex<()>,
    woken: Condvar,
    /// The objects a public [`crate::value::Value`] outside this run's frames
    /// names, one entry per holder.
    ///
    /// A frame is a root because a static map says which of its slots are
    /// references, and that is the whole of what keeps an object alive — so a
    /// closure a host was handed would be swept the moment the frame that
    /// built it cleared its slot, which is the instruction after the call
    /// that handed it over. The host is allowed to keep it: the [`Reentry`]
    /// contract says a host that wants work done later *"keeps the callback —
    /// a `Value` is an ordinary owned value"*, and `http.Server.handle` is
    /// written that way.
    ///
    /// So a `Value` that names an object says so here for as long as it
    /// exists, and [`Space::collect`] reads this beside the roots the tasks
    /// published. It is a multiset rather than a set because a `Value` is
    /// cloned like any other: two holders of one object are two entries, and
    /// the object stops being a root when the second one goes.
    ///
    /// It is not a second value store, by the test ADR 0034 applies: an entry
    /// is an address of an object in the run's one heap, and nothing that
    /// wanted to dodge a heap representation could be put in it. It is a root
    /// provider, exactly as the scheduler table is.
    ///
    /// [`Reentry`]: crate::host::Reentry
    pinned: Mutex<Vec<u64>>,
}

impl Space {
    /// A space whose heap may grow to `heap_words_budget` words.
    ///
    /// The budget is a count of words rather than of objects: what exhausts a
    /// heap is the space its objects take, and a `Vector` of a million
    /// elements is one object.
    fn new(heap_words_budget: usize) -> Space {
        let budget = (heap_words_budget as u64).min(MAX_HEAP_WORDS);
        Space {
            words: Words::new(budget),
            bump: AtomicU64::new(STACK_WORDS),
            alloc: Mutex::new(Alloc {
                bump: 0,
                limit: STACK_WORDS + budget,
                free: Vec::new(),
                marks: Vec::new(),
                allocated_words: 0,
                collections: 0,
            }),
            stw: Mutex::new(Stw {
                parties: Vec::new(),
                live: 0,
                collecting: false,
                last: Collected::default(),
            }),
            pending: AtomicBool::new(false),
            turn: Condvar::new(),
            waiting: Mutex::new(()),
            woken: Condvar::new(),
            pinned: Mutex::new(Vec::new()),
        }
    }

    /// Takes the pinned table's lock, recovering from a poisoned one for the
    /// reason [`Space::allocator`] gives.
    fn pinned(&self) -> MutexGuard<'_, Vec<u64>> {
        self.pinned.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// Makes `addr` a root for as long as the [`Rooted`] this is taken for
    /// lives.
    fn pin(&self, addr: u64) {
        self.pinned().push(addr);
    }

    /// Gives back one holder's claim on `addr`.
    ///
    /// One occurrence, not every one: the table is a multiset, so a second
    /// holder of the same object keeps it alive after the first has gone.
    fn unpin(&self, addr: u64) {
        let mut held = self.pinned();
        if let Some(at) = held.iter().rposition(|kept| *kept == addr) {
            held.swap_remove(at);
        }
    }

    /// Takes the allocator's lock, recovering from a poisoned one.
    ///
    /// A task that panicked while holding it left the heap's bookkeeping in
    /// whatever state it was in, and that state is a bump pointer and a free
    /// list rather than a half-written invariant. Refusing every later
    /// allocation of the run because one task panicked would turn a task's
    /// failure into the run's, which is the opposite of what a task boundary
    /// is for.
    fn allocator(&self) -> MutexGuard<'_, Alloc> {
        self.alloc.lock().unwrap_or_else(|held| held.into_inner())
    }

    fn world(&self) -> MutexGuard<'_, Stw> {
        self.stw.lock().unwrap_or_else(|held| held.into_inner())
    }

    // --- words ------------------------------------------------------------

    #[inline]
    fn load(&self, addr: u64) -> u64 {
        self.words.at(addr - STACK_WORDS).load(Ordering::Relaxed)
    }

    #[inline]
    fn store(&self, addr: u64, word: u64) {
        self.words
            .at(addr - STACK_WORDS)
            .store(word, Ordering::Relaxed);
    }

    /// Reads the run of words at `addr` into `out`.
    fn read_into(&self, addr: u64, out: &mut [u64]) {
        let mut at = addr - STACK_WORDS;
        let mut done = 0;
        while done < out.len() {
            let run = self.words.run(at);
            let take = run.len().min(out.len() - done);
            for (slot, word) in out[done..done + take].iter_mut().zip(run) {
                *slot = word.load(Ordering::Relaxed);
            }
            done += take;
            at += take as u64;
        }
    }

    /// Writes `src` over the run of words at `addr`.
    fn write_from(&self, addr: u64, src: &[u64]) {
        let mut at = addr - STACK_WORDS;
        let mut done = 0;
        while done < src.len() {
            let run = self.words.run(at);
            let take = run.len().min(src.len() - done);
            for (word, slot) in run.iter().zip(&src[done..done + take]) {
                word.store(*slot, Ordering::Relaxed);
            }
            done += take;
            at += take as u64;
        }
    }

    /// Zeroes `words` words at `addr`.
    fn fill(&self, addr: u64, words: u64) {
        let mut at = addr - STACK_WORDS;
        let mut done = 0;
        while done < words {
            let run = self.words.run(at);
            let take = (run.len() as u64).min(words - done);
            for word in &run[..take as usize] {
                word.store(0, Ordering::Relaxed);
            }
            done += take;
            at += take;
        }
    }

    /// Copies `words` words from `src` to `dst`, both heap addresses.
    ///
    /// A word at a time and direction-aware, so a run may overlap itself. The
    /// two runs are in general in different chunks at different offsets, so
    /// there is no pair of slices to hand a `copy_within`; what a chunked
    /// store buys on this path is bounded rather than free.
    fn copy(&self, dst: u64, src: u64, words: u64) {
        if dst < src {
            for at in 0..words {
                self.store(dst + at, self.load(src + at));
            }
        } else {
            for at in (0..words).rev() {
                self.store(dst + at, self.load(src + at));
            }
        }
    }

    /// Sets the word at `addr` to `word` if it is `expect`, answering what was
    /// there when it was not.
    ///
    /// The one read-modify-write this memory offers, and the one a lock word
    /// needs. It succeeds with `Acquire`, so everything the previous holder
    /// wrote before its [`Space::release_word`] is visible to whoever takes
    /// the word next — which is what publishes a `Shared` cell's value from
    /// one task to another without any other word being anything but
    /// `Relaxed`.
    fn acquire_word(&self, addr: u64, expect: u64, word: u64) -> Result<(), u64> {
        self.words
            .at(addr - STACK_WORDS)
            .compare_exchange(expect, word, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| ())
    }

    /// Writes `word` at `addr` with `Release`, publishing every write made
    /// before it to whoever acquires the same word next.
    fn release_word(&self, addr: u64, word: u64) {
        self.words
            .at(addr - STACK_WORDS)
            .store(word, Ordering::Release);
    }

    // --- objects ----------------------------------------------------------

    /// Allocates an object of `layout`, answering the address of its header.
    ///
    /// `len` is the header's length field and `payload_words` is how many words
    /// the object occupies after the header. Both are passed because this is
    /// the one heap operation that does not hold the layout table, and the two
    /// are not the same number for any shape whose `len` counts something other
    /// than words. The caller must pass what [`Layout::payload_words`] answers
    /// for `layout` and `len`: the sweeper recovers an object's size from its
    /// header and the table, and a disagreement makes the heap unwalkable.
    ///
    /// Answers `None` when the object fits neither a free block nor the
    /// remaining budget. That is not an error — the caller collects and asks
    /// again, and a second `None` is the one that ends the run.
    fn alloc(&self, layout: LayoutId, len: u32, payload_words: u32) -> Option<u64> {
        let words = 1 + payload_words as u64;
        let mut alloc = self.allocator();
        let addr = match self.take_free(&mut alloc, words) {
            Some(addr) => {
                // A reclaimed block still holds the dead object's words, and a
                // `Ref` field of the new object must read as null until it is
                // written. The bump path below needs no such pass: its words
                // have never been used.
                self.fill(addr, words);
                addr
            }
            None => {
                let addr = STACK_WORDS + alloc.bump;
                if addr + words > alloc.limit {
                    return None;
                }
                self.words.commit(alloc.bump, alloc.bump + words);
                alloc.bump += words;
                self.bump.store(STACK_WORDS + alloc.bump, Ordering::Relaxed);
                addr
            }
        };
        self.store(addr, header(layout, len));
        alloc.allocated_words += words;
        Some(addr)
    }

    /// The first free block of at least `words` words, split to size.
    ///
    /// First fit over a list the sweeper leaves in address order. It is the
    /// simplest thing that makes "collect and retry" mean something, and ADR
    /// 0034 leaves the final allocator undecided, so nothing is committed by
    /// choosing it. A remainder always becomes a free block of its own, however
    /// small: the smallest one is a header and no payload, which is one word.
    fn take_free(&self, alloc: &mut Alloc, words: u64) -> Option<u64> {
        let mut at = 0;
        while at < alloc.free.len() {
            let addr = alloc.free[at];
            let have = self.block_words(addr);
            if have >= words {
                if have == words {
                    // `remove` rather than `swap_remove`: the list is in
                    // address order and first fit over an address-ordered
                    // list is what keeps small survivors from stranding the
                    // low end of the heap. A swap would trade that for a
                    // shift over a list the next sweep rebuilds anyway.
                    alloc.free.remove(at);
                } else {
                    let rest = addr + words;
                    self.store(rest, header(LayoutId::FREE, (have - words - 1) as u32));
                    alloc.free[at] = rest;
                }
                return Some(addr);
            }
            at += 1;
        }
        None
    }

    /// The layout of the object whose header is at `addr`.
    #[inline]
    fn object_layout(&self, addr: u64) -> LayoutId {
        header_layout(self.load(addr))
    }

    /// The length field of the object whose header is at `addr`.
    #[inline]
    fn object_len(&self, addr: u64) -> u32 {
        header_len(self.load(addr))
    }

    /// How many words the free block at `addr` occupies, header included.
    pub(crate) fn block_words(&self, addr: u64) -> u64 {
        1 + self.object_len(addr) as u64
    }

    /// How many words the object at `addr` occupies, header included.
    ///
    /// A free block is answered without consulting the table. That is not an
    /// optimisation: it means a sweep can walk a heap the sweeper itself wrote
    /// into whether or not the caller's table reserved index 0, so the one
    /// structure the collector must be able to traverse does not depend on a
    /// convention the lowering could get wrong.
    fn object_words(&self, layouts: &[Layout], addr: u64) -> u64 {
        let layout = self.object_layout(addr);
        let len = self.object_len(addr);
        if layout == LayoutId::FREE {
            return 1 + len as u64;
        }
        1 + layouts[layout.index()].payload_words(len, layouts) as u64
    }

    // --- joining and leaving ----------------------------------------------

    /// Takes a stack segment for a new task, answering which.
    ///
    /// A task that arrives while a collection is running waits it out before
    /// it starts. It has no frames yet and so no roots, but it is about to
    /// have both, and a party the collector counted as arrived on the strength
    /// of an empty publication would be a party whose first frame the next
    /// collection never saw.
    fn attach(self: &Arc<Space>) -> Result<u32, NoSegment> {
        let mut stw = self.world();
        while stw.collecting {
            stw = self.turn.wait(stw).unwrap_or_else(|held| held.into_inner());
        }
        let free = stw.parties.iter().position(|party| !party.live);
        let at = match free {
            Some(at) => at,
            None if (stw.parties.len() as u64) < SEGMENTS => {
                stw.parties.push(Party::default());
                stw.parties.len() - 1
            }
            None => return Err(NoSegment),
        };
        stw.parties[at] = Party {
            live: true,
            at: None,
        };
        stw.live += 1;
        Ok(at as u32)
    }

    /// Gives a task's stack segment back.
    ///
    /// A collector waiting for this party stops waiting for it: a task that
    /// has left has no frames, so there is nothing left to publish.
    fn detach(&self, at: u32) {
        let mut stw = self.world();
        stw.parties[at as usize] = Party::default();
        stw.live -= 1;
        self.turn.notify_all();
    }

    // --- mark and sweep -----------------------------------------------------

    /// Stops the world, marks everything the run's roots reach, and reclaims
    /// the rest.
    ///
    /// `mine` is the calling task's own roots; every other live task's are
    /// what it published at the safepoint it parked at. A task that is already
    /// waiting — on a cell, or inside a host call — published them when it
    /// began waiting and the collector needs nothing further from it.
    ///
    /// A caller that finds a collection already running waits for that one and
    /// answers what it did rather than starting a second. That is not only an
    /// economy: the caller is here because an allocation did not fit, and the
    /// collection it is waiting for may be the one that makes room.
    ///
    /// Non-moving, so every address a program holds — including one that points
    /// into an object — is still correct when this returns.
    fn collect(&self, me: u32, layouts: &[Layout], mine: &dyn Roots) -> Collected {
        let mut stw = self.world();
        if stw.collecting {
            stw.parties[me as usize].at = Some(gather(mine));
            self.turn.notify_all();
            while stw.collecting {
                stw = self.turn.wait(stw).unwrap_or_else(|held| held.into_inner());
            }
            stw.parties[me as usize].at = None;
            return stw.last;
        }

        stw.collecting = true;
        self.pending.store(true, Ordering::Relaxed);
        self.turn.notify_all();
        while stw.arrived(me) < stw.live - 1 {
            stw = self.turn.wait(stw).unwrap_or_else(|held| held.into_inner());
        }

        let mut roots = gather(mine);
        for party in &stw.parties {
            if let Some(published) = &party.at {
                roots.extend_from_slice(published);
            }
        }
        // And what is named from outside every frame: an object a public
        // `Value` a host is holding points at. A task's roots are its frames,
        // and a host's callback is deliberately not in one — see
        // [`Space::pinned`].
        roots.extend_from_slice(&self.pinned());

        let done = {
            let mut alloc = self.allocator();
            self.mark_sweep(&mut alloc, layouts, &roots)
        };

        stw.collecting = false;
        stw.last = done;
        self.pending.store(false, Ordering::Relaxed);
        self.turn.notify_all();
        done
    }

    /// The collection itself, with the world already stopped.
    fn mark_sweep(&self, alloc: &mut Alloc, layouts: &[Layout], roots: &[u64]) -> Collected {
        alloc.marks.clear();
        alloc.marks.resize((alloc.bump as usize).div_ceil(64), 0);

        // An explicit worklist rather than recursion. A linked list a million
        // long is an ordinary Cove value and a legal object graph; a collector
        // that recursed over it would overflow the Rust stack, and the one
        // moment a runtime cannot afford to abort is the one where it is
        // reclaiming memory because there is none left.
        let mut work: Vec<u64> = Vec::new();
        let bump = STACK_WORDS + alloc.bump;
        for &addr in roots {
            if reachable(addr, bump) && set_mark(&mut alloc.marks, addr) {
                work.push(addr);
            }
        }

        while let Some(addr) = work.pop() {
            self.trace(alloc, layouts, addr, &mut work);
        }

        let (freed_words, live_words) = self.sweep(alloc, layouts);
        alloc.collections += 1;
        Collected {
            freed_words,
            live_words,
            collections: alloc.collections,
        }
    }

    /// Marks and enqueues every object the object at `addr` refers to.
    ///
    /// What an object refers to is a question its *layout* answers, and — for
    /// one shape — one word of the object itself. That is the simplification
    /// the run-of-words model bought: an enum's payload region has a static
    /// per-word reference map, because the lowering assigns case offsets so
    /// that every case using a word agrees on its `Repr`. Nothing here reads a
    /// discriminant to decide what to trace, and a payload-less case cannot
    /// retain what another case left in a word it does not use, because
    /// construction zeroes the region it does not fill.
    ///
    /// So a payload is walked by flattened per-word `Repr`s, and every shape
    /// says where its runs of them are. The one object that still has to be
    /// read is a box: erasure is where a value stops having a static width,
    /// so the box carries the layout of what it holds in its first payload
    /// word, and that is the word this asks for.
    fn trace(&self, alloc: &mut Alloc, layouts: &[Layout], addr: u64, work: &mut Vec<u64>) {
        let layout = &layouts[self.object_layout(addr).index()];
        // The one question that can be answered without reading the object at
        // all. A string, an `Array<Int>` and a scalar struct leave here having
        // cost a table lookup.
        if !layout.may_hold_refs(layouts) {
            return;
        }
        let len = self.object_len(addr);
        match &layout.shape {
            Shape::Free | Shape::Str => {}
            // A scalar, a struct or an enum in the heap is a value whose
            // payload *is* the value, laid out exactly as it would be in a
            // frame. `Layout::payload_words` answers the same width for the
            // same reason, and this is the other half of that agreement.
            Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
                self.trace_run(alloc, addr, 0, &layout.words, work)
            }
            // The stride is the element's width, so an `Array<Point>` is a
            // run of two-word elements rather than a run of addresses, and a
            // `Set` of them is the same run kept sorted.
            Shape::Elements { elem, .. } | Shape::Members { elem } => {
                let elem = &layouts[elem.index()];
                let stride = elem.width();
                for at in 0..len {
                    self.trace_run(alloc, addr, at * stride, &elem.words, work);
                }
            }
            // Word 0 is the length and word 1 is the store, whose own layout
            // says what its elements are. A vector's header is a leaf apart
            // from the one reference that makes it growable.
            Shape::Vector { .. } => {
                let store = self.payload(addr, 1);
                self.enqueue(alloc, store, work)
            }
            // Key then value, each at its own width: a `Map<String, Point>`
            // is a run of three-word entries and only the first is traced.
            Shape::Entries { key, value } => {
                let (key, value) = (&layouts[key.index()], &layouts[value.index()]);
                let stride = key.width() + value.width();
                for at in 0..len {
                    self.trace_run(alloc, addr, at * stride, &key.words, work);
                    self.trace_run(alloc, addr, at * stride + key.width(), &value.words, work);
                }
            }
            // Word 0 is the lock and the value follows it, inline. So a
            // cell is traced the way a closure environment is, and a cycle
            // that passes through one is an ordinary object-graph cycle —
            // which is the whole of what
            // [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
            // costs here. The lock word is an `Int` in the map, so nothing
            // follows it.
            Shape::Shared { value } => {
                let value = &layouts[value.index()];
                self.trace_run(alloc, addr, SHARED_VALUE, &value.words, work)
            }
            // Word 0 is the callee. The captures follow it, each inline under
            // its own layout, which is how a captured struct is stored where
            // the capture is rather than behind another address.
            Shape::Closure { captures, .. } => {
                let mut at = 1;
                for id in captures {
                    let capture = &layouts[id.index()];
                    self.trace_run(alloc, addr, at, &capture.words, work);
                    at += capture.width();
                }
            }
            // The only object whose own words say what the rest of them mean.
            // A layout the table does not have is a lowering bug, and a
            // collection is the worst place to discover one by unwinding, so
            // nothing is traced — which can only fail by freeing something,
            // and the differential corpus is what catches that.
            Shape::Boxed => {
                let held = LayoutId(self.payload(addr, 0) as u32);
                if let Some(held) = layouts.get(held.index()) {
                    self.trace_run(alloc, addr, 1, &held.words, work);
                }
            }
        }
    }

    /// Enqueues every reference in the run of `words` beginning at payload
    /// word `at`.
    ///
    /// The one operation every shape above is written in terms of, because
    /// under this model every one of them is a run of words with a static
    /// per-word map — a frame's, an array element's and a capture's are the
    /// same function of the same kind of layout.
    fn trace_run(
        &self,
        alloc: &mut Alloc,
        addr: u64,
        at: u32,
        words: &[Repr],
        work: &mut Vec<u64>,
    ) {
        for (offset, repr) in words.iter().enumerate() {
            if repr.is_ref() {
                let held = self.payload(addr, at + offset as u32);
                self.enqueue(alloc, held, work);
            }
        }
    }

    /// Marks `addr` and enqueues it if this is the first time it was seen.
    ///
    /// Null is the ordinary case, not an error: a frame is zeroed on entry, a
    /// slot is cleared at its last use, and a `Ref` field of a half-built object
    /// has not been written yet.
    #[inline]
    fn enqueue(&self, alloc: &mut Alloc, addr: u64, work: &mut Vec<u64>) {
        let bump = STACK_WORDS + alloc.bump;
        debug_assert!(
            addr == 0 || reachable(addr, bump),
            "a Ref word named {addr}, which is not a heap object"
        );
        if reachable(addr, bump) && set_mark(&mut alloc.marks, addr) {
            work.push(addr);
        }
    }

    /// Payload word `at` of the object whose header is at `addr`.
    #[inline]
    fn payload(&self, addr: u64, at: u32) -> u64 {
        self.load(addr + 1 + at as u64)
    }

    /// Turns every unmarked object into a free block, answering the words
    /// freed and the words still live.
    ///
    /// The heap is a walkable sequence of objects from [`STACK_WORDS`] to the
    /// bump pointer — that is what reserving `LayoutId::FREE` buys, and it is
    /// why a reclaimed run keeps a header instead of being forgotten. Adjacent
    /// dead objects are written as *one* block, so a heap that dies in small
    /// pieces can still satisfy a large request; without that, a long run would
    /// fragment into blocks that nothing but the object that died there could
    /// ever fit into.
    ///
    /// The bump pointer never retreats, even when the last block is free. One
    /// reclamation mechanism is easier to reason about than two, and a trailing
    /// free block is reused by the same first fit as any other.
    fn sweep(&self, alloc: &mut Alloc, layouts: &[Layout]) -> (u64, u64) {
        alloc.free.clear();
        let mut freed = 0;
        let mut live = 0;
        let mut run: Option<u64> = None;
        let mut addr = STACK_WORDS;
        let end = STACK_WORDS + alloc.bump;
        while addr < end {
            let words = self.object_words(layouts, addr);
            if is_marked(&alloc.marks, addr) {
                live += words;
                if let Some(start) = run.take() {
                    self.close_free_run(alloc, start, addr);
                }
            } else {
                if self.object_layout(addr) != LayoutId::FREE {
                    freed += words;
                }
                run.get_or_insert(addr);
            }
            addr += words;
        }
        if let Some(start) = run {
            self.close_free_run(alloc, start, end);
        }
        (freed, live)
    }

    /// Writes `[start, end)` as one free block and records it.
    fn close_free_run(&self, alloc: &mut Alloc, start: u64, end: u64) {
        self.store(start, header(LayoutId::FREE, (end - start - 1) as u32));
        alloc.free.push(start);
    }

    // --- safepoints and waiting -------------------------------------------

    /// What a task does at a safepoint: nothing, unless a collection is
    /// waiting for it.
    ///
    /// The fast path is one relaxed load, which is what a safepoint can afford
    /// — it runs every [`crate::lvm::exec::SAFEPOINT_STRIDE`] instructions and
    /// a collection is rare. When the answer is yes, the task publishes the
    /// roots it is holding and parks until the collection is done.
    fn poll(&self, me: u32, roots: &dyn Roots) {
        if !self.pending.load(Ordering::Relaxed) {
            return;
        }
        let mut stw = self.world();
        if !stw.collecting {
            return;
        }
        stw.parties[me as usize].at = Some(gather(roots));
        self.turn.notify_all();
        while stw.collecting {
            stw = self.turn.wait(stw).unwrap_or_else(|held| held.into_inner());
        }
        stw.parties[me as usize].at = None;
    }

    /// Publishes `roots` and stays at a safepoint until the answer is dropped.
    ///
    /// What a task takes before it blocks — on a `Shared` cell's lock, or
    /// inside a host call — and the reason a collection cannot deadlock behind
    /// one. A task waiting for a lock another task holds cannot reach a
    /// safepoint of its own, and a collector that waited for it would be
    /// waiting for a task that is waiting for a task that is waiting for the
    /// collector. A blocked task is not running, so its frames do not change
    /// and the snapshot it leaves stays true for as long as it is blocked.
    fn blocking<'s>(&'s self, me: u32, roots: &dyn Roots) -> Blocking<'s> {
        self.arrive(me, roots);
        Blocking { space: self, me }
    }

    /// Publishes `roots` for `me` and says so, without waiting for anything.
    ///
    /// The half of a park that both guards share. [`Blocking`] borrows the
    /// space and [`Parked`] owns a handle on it, and neither difference is
    /// about what arriving and leaving *are*.
    fn arrive(&self, me: u32, roots: &dyn Roots) {
        let mut stw = self.world();
        stw.parties[me as usize].at = Some(gather(roots));
        self.turn.notify_all();
    }

    /// Leaves the safepoint, waiting out a collection that has already begun.
    ///
    /// Waiting is what makes the published snapshot honest: a task that
    /// resumed while the collector was still reading its roots would be
    /// changing the answer to a question already asked.
    fn depart(&self, me: u32) {
        let mut stw = self.world();
        while stw.collecting {
            stw = self.turn.wait(stw).unwrap_or_else(|held| held.into_inner());
        }
        stw.parties[me as usize].at = None;
        self.turn.notify_all();
    }

    /// Blocks until [`Space::wake`] names `addr` and the word is no longer
    /// `was`.
    ///
    /// The task counts as arrived for the whole wait, so a collection may run
    /// while it is here and will not wait for it.
    fn wait(&self, me: u32, addr: u64, was: u64, roots: &dyn Roots) {
        let held = self.blocking(me, roots);
        let mut waiting = self
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while self.load(addr) == was {
            waiting = self
                .woken
                .wait(waiting)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(waiting);
        drop(held);
    }

    /// Wakes every task waiting on a word of this memory.
    ///
    /// The lock is taken and dropped without doing anything under it, and that
    /// is the point: a waiter holds it from the moment it reads the word to the
    /// moment it is on the condition variable, so a wake that lands in between
    /// waits for the waiter rather than being lost.
    fn wake(&self, addr: u64) {
        debug_assert!(!is_stack(addr), "only a heap word is waited on");
        let held = self
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(held);
        self.woken.notify_all();
    }

    // --- reporting ----------------------------------------------------------

    /// Words the heap region currently occupies, free blocks included.
    fn heap_words(&self) -> u64 {
        self.bump.load(Ordering::Relaxed) - STACK_WORDS
    }

    /// Words handed out over the whole run, reuse counted each time.
    fn allocated_words(&self) -> u64 {
        self.allocator().allocated_words
    }

    /// How many collections this run has run.
    fn collections(&self) -> u64 {
        self.allocator().collections
    }

    /// How many tasks are executing over this space.
    fn tasks(&self) -> usize {
        self.world().live
    }
}

/// A task that is at a safepoint until this is dropped.
pub(crate) struct Blocking<'s> {
    space: &'s Space,
    me: u32,
}

impl Drop for Blocking<'_> {
    fn drop(&mut self) {
        self.space.depart(self.me);
    }
}

/// The same park, held by something that cannot borrow the memory.
///
/// [`Blocking`] borrows the [`Space`], which is what a caller with the
/// [`Memory`] in scope wants and what a caller *inside* a host call cannot
/// have: the way back a host is handed holds the machine mutably, so a guard
/// borrowing the same machine's memory could not exist beside it. This owns a
/// handle on the space instead, which is what an `Arc` is for, and does
/// exactly what the borrowed one does.
///
/// A callback drops it and takes another, in that order. A task running a
/// host's callback is running Cove again — its frames change between two
/// instructions — so the snapshot it published on the way into the host call
/// stops being true for exactly as long as the callback runs, and a task that
/// left it standing would be telling the collector to trace a frame that has
/// moved.
pub(crate) struct Parked {
    space: Arc<Space>,
    me: u32,
}

impl Drop for Parked {
    fn drop(&mut self) {
        self.space.depart(self.me);
    }
}

/// An object a public [`crate::value::Value`] names, kept a root for as long
/// as that value lives.
///
/// See [`Space::pinned`] for why a frame cannot answer this. Cloning takes a
/// second claim rather than sharing one, so a `Value` that was cloned and a
/// `Value` that was dropped each account for themselves.
pub(crate) struct Rooted {
    space: Arc<Space>,
    addr: u64,
}

impl Rooted {
    /// The object's address, which is what the machine calls it.
    pub(crate) fn addr(&self) -> u64 {
        self.addr
    }
}

impl Clone for Rooted {
    fn clone(&self) -> Rooted {
        self.space.pin(self.addr);
        Rooted {
            space: Arc::clone(&self.space),
            addr: self.addr,
        }
    }
}

impl Drop for Rooted {
    fn drop(&mut self) {
        self.space.unpin(self.addr);
    }
}

/// Prints as the address it is, and not as the run it belongs to.
///
/// A [`Space`] has no useful rendering and a whole heap's worth of one, so a
/// `Debug` that derived through the handle would print the run rather than
/// the object.
impl std::fmt::Debug for Rooted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rooted({})", self.addr)
    }
}

/// Every address `roots` names, as a run the collector can hold.
fn gather(roots: &dyn Roots) -> Vec<u64> {
    let mut held = Vec::new();
    roots.each_root(&mut |addr| held.push(addr));
    held
}

/// Whether `addr` names the header of an object that exists.
///
/// Null fails it, and so does a stack address: neither is an object, and the
/// first is what an unwritten or cleared reference slot reads as.
#[inline]
fn reachable(addr: u64, bump: u64) -> bool {
    !is_stack(addr) && addr < bump
}

/// Sets the mark bit for `addr`, answering whether it was not already set.
#[inline]
fn set_mark(marks: &mut [u64], addr: u64) -> bool {
    let bit = (addr - STACK_WORDS) as usize;
    let mask = 1 << (bit % 64);
    let word = &mut marks[bit / 64];
    let fresh = *word & mask == 0;
    *word |= mask;
    fresh
}

/// Whether the object at `addr` was marked by this collection.
#[inline]
fn is_marked(marks: &[u64], addr: u64) -> bool {
    let bit = (addr - STACK_WORDS) as usize;
    marks[bit / 64] & (1 << (bit % 64)) != 0
}

// --- one task's view --------------------------------------------------------

/// One task's stack segment.
///
/// Owned outright by the task, never shared, and addressed from its origin so
/// that a slot number means the same thing whichever segment it is in.
struct Stack {
    /// The first address of this task's segment.
    origin: u64,
    /// The committed words, from `origin` up.
    words: Vec<u64>,
}

impl Stack {
    #[inline]
    fn at(&self, addr: u64) -> usize {
        (addr - self.origin) as usize
    }

    #[inline]
    fn read(&self, addr: u64) -> u64 {
        self.words[self.at(addr)]
    }

    #[inline]
    fn write(&mut self, addr: u64, word: u64) {
        let at = self.at(addr);
        self.words[at] = word;
    }
}

/// One task's linear memory: its own stack segment, and the run's heap.
///
/// The type a [`crate::lvm::exec::Machine`] holds. Every address it reads or
/// writes is a word index into the run's one address space, and which half of
/// the pair answers is [`is_stack`] and nothing else.
pub(crate) struct Memory {
    stack: Stack,
    space: Arc<Space>,
    /// Which segment this task took, which is also its party in a collection.
    at: u32,
}

impl Drop for Memory {
    fn drop(&mut self) {
        self.space.detach(self.at);
    }
}

impl Memory {
    /// A new run's memory: a fresh heap of `heap_words_budget` words, and the
    /// first stack segment.
    ///
    /// The budget is a count of words rather than of objects: what exhausts a
    /// heap is the space its objects take, and a `Vector` of a million
    /// elements is one object.
    pub(crate) fn new(heap_words_budget: usize) -> Memory {
        let space = Arc::new(Space::new(heap_words_budget));
        let at = space.attach().expect("a new space has every segment free");
        Memory {
            stack: Stack {
                origin: segment_origin(at),
                words: Vec::new(),
            },
            space,
            at,
        }
    }

    /// A second task's memory over the same run: its own stack segment, the
    /// same heap.
    ///
    /// This is the whole of Q1's *one heap per run, shared by the run's task
    /// threads*. Nothing is copied, nothing is split, and the answer is `Send`
    /// so that it can be given to the thread the task will run on.
    pub(crate) fn for_task(&self) -> Result<Memory, NoSegment> {
        let at = self.space.attach()?;
        Ok(Memory {
            stack: Stack {
                origin: segment_origin(at),
                words: Vec::new(),
            },
            space: Arc::clone(&self.space),
            at,
        })
    }

    /// Which stack segment this task owns.
    pub(crate) fn segment(&self) -> u32 {
        self.at
    }

    /// How many tasks are executing over this run's heap, this one counted.
    pub(crate) fn tasks(&self) -> usize {
        self.space.tasks()
    }

    // --- the whole memory ---------------------------------------------------

    /// The word at `addr`, in whichever region it names.
    ///
    /// One entry point for both regions is what makes a `Repr::Addr` word
    /// uniform: `Load` and `Store` read and write through an address without
    /// asking whether the place it names is a local or a field, which is what
    /// keeps `bump(var total)` from needing a second instruction pair for each
    /// kind of target.
    #[inline]
    pub(crate) fn read(&self, addr: u64) -> u64 {
        if is_stack(addr) {
            self.stack.read(addr)
        } else {
            self.space.load(addr)
        }
    }

    /// Writes `word` at `addr`, in whichever region it names.
    #[inline]
    pub(crate) fn write(&mut self, addr: u64, word: u64) {
        if is_stack(addr) {
            self.stack.write(addr, word);
        } else {
            self.space.store(addr, word);
        }
    }

    /// Copies `words` words from `src` to `dst`, in whichever regions they
    /// name.
    ///
    /// This is the whole of ADR 0001's field-wise shallow copy. A value is a
    /// run of words where the value is, so copying one is copying its words:
    /// a `Wrapper { p: Point, v: Vector }` is three, the `Point` becomes
    /// independent because its two words were copied, and the `Vector` stays
    /// shared because what was copied is its address. Neither answer needed a
    /// policy, a sharing bit or a copy-on-write protocol, and there is none
    /// here to keep in step.
    ///
    /// A copy within one region moves rather than smears, so a run may overlap
    /// itself — which a copy between two slots of one frame can, and which a
    /// lowering is free to emit rather than having to prove it does not.
    pub(crate) fn copy_words(&mut self, dst: u64, src: u64, words: u32) {
        if words == 0 || dst == src {
            return;
        }
        debug_assert!(
            self.holds(dst, words) && self.holds(src, words),
            "a {words}-word copy between {src} and {dst} leaves the words that exist"
        );
        if words == 1 {
            // The common width by a long way — every scalar, every reference,
            // every address — and the one case where splitting the run across
            // the heap's chunks is all cost and no work. A load and a store.
            let word = self.read(src);
            self.write(dst, word);
            return;
        }
        let n = words as usize;
        match (is_stack(dst), is_stack(src)) {
            (true, true) => {
                let (d, s) = (self.stack.at(dst), self.stack.at(src));
                self.stack.words.copy_within(s..s + n, d);
            }
            (false, false) => self.space.copy(dst, src, words as u64),
            (true, false) => {
                let d = self.stack.at(dst);
                self.space.read_into(src, &mut self.stack.words[d..d + n]);
            }
            (false, true) => {
                let s = self.stack.at(src);
                self.space.write_from(dst, &self.stack.words[s..s + n]);
            }
        }
    }

    /// Zeroes `words` words at `addr`.
    ///
    /// What `Clear` writes over a value location whose value is dead, and
    /// what an enum's construction writes over the payload words its case
    /// does not fill. Both exist so that a static reference map reads null
    /// rather than a stale address: the map says which words the collector
    /// *reads*, and only the data can say when what was in one stopped being
    /// needed.
    pub(crate) fn clear_words(&mut self, addr: u64, words: u32) {
        if words == 0 {
            return;
        }
        debug_assert!(
            self.holds(addr, words),
            "a {words}-word clear at {addr} leaves the words that exist"
        );
        if is_stack(addr) {
            let at = self.stack.at(addr);
            self.stack.words[at..at + words as usize].fill(0);
        } else {
            self.space.fill(addr, words as u64);
        }
    }

    /// The `words` words at `addr`, copied out.
    ///
    /// The one reader is the boundary, which materialises a value location
    /// into a public `Value` and needs the run of words rather than one of
    /// them. Nothing in ordinary execution calls it: a copy inside the
    /// machine is [`Memory::copy_words`], which never leaves the memory.
    pub(crate) fn read_words(&self, addr: u64, words: u32) -> Vec<u64> {
        debug_assert!(
            self.holds(addr, words),
            "a {words}-word read at {addr} leaves the words that exist"
        );
        let n = words as usize;
        if is_stack(addr) {
            let at = self.stack.at(addr);
            self.stack.words[at..at + n].to_vec()
        } else {
            let mut out = vec![0; n];
            self.space.read_into(addr, &mut out);
            out
        }
    }

    /// Writes `words` over the run at `addr`.
    ///
    /// The mirror of [`Memory::read_words`] and it has the one caller that
    /// direction has: a host's answer, or a callback's argument, converted
    /// out of a `Value` into the words of a value location. A copy that never
    /// leaves the memory is [`Memory::copy_words`].
    pub(crate) fn write_words(&mut self, addr: u64, words: &[u64]) {
        debug_assert!(
            self.holds(addr, words.len() as u32),
            "a {}-word write at {addr} stays inside the words that exist",
            words.len()
        );
        if is_stack(addr) {
            let at = self.stack.at(addr);
            self.stack.words[at..at + words.len()].copy_from_slice(words);
        } else {
            self.space.write_from(addr, words);
        }
    }

    /// Whether the run of `words` words at `addr` is inside its region.
    ///
    /// A `debug_assert` rather than a check, because what makes it true is
    /// static: the verifier holds every instruction to the frame it names its
    /// slots in, and every offset into an object to that object's payload. A
    /// run that leaves its region is therefore a lowering bug, and this is
    /// here so that it is reported as one rather than as a slice index — the
    /// dangerous case is not the panic but the one where the stack happens to
    /// be long enough and the copy silently reads the frame above.
    ///
    /// A stack address of *another* task's segment fails it, which is the
    /// second thing segments buy: an address one task formed cannot be read by
    /// another even by accident, because it is not in the words that task has.
    fn holds(&self, addr: u64, words: u32) -> bool {
        let end = addr + words as u64;
        if is_stack(addr) {
            addr >= self.stack.origin && end <= self.stack.origin + self.stack.words.len() as u64
        } else {
            end <= self.space.bump.load(Ordering::Relaxed)
        }
    }

    // --- the stack region ---------------------------------------------------

    /// Reserves `size` zeroed words on top of this task's segment and answers
    /// their base.
    ///
    /// Zeroed, because a `Repr::Ref` slot that has not been written yet must
    /// read as null rather than as whatever the returned frame left in that
    /// word. The collector reads a frame's reference slots by a static map, so
    /// a slot the program has not reached yet is still walked, and a stale
    /// address there would retain an object — or, worse, name a word that is no
    /// longer an object header.
    pub(crate) fn push_frame(&mut self, size: u32) -> Result<u64, Overflow> {
        let used = self.stack.words.len() as u64;
        if used + size as u64 >= SEGMENT_WORDS {
            return Err(Overflow);
        }
        self.stack.words.resize(used as usize + size as usize, 0);
        Ok(self.stack.origin + used)
    }

    /// Drops every frame at or above `base`.
    ///
    /// Truncation does not clear the words it releases; [`Memory::push_frame`]
    /// zeroes them on the way back up. Doing it once, on the path that is about
    /// to write them anyway, is one pass rather than two.
    pub(crate) fn pop_frame(&mut self, base: u64) {
        let at = self.stack.at(base);
        self.stack.words.truncate(at);
    }

    /// Drops every frame this segment holds.
    ///
    /// What a top-level call begins with, and it has to be said rather than
    /// assumed: a call that was stopped where it stood — a budget that ran
    /// out, a raised cancellation, a host that failed — left its frames on the
    /// stack, because a runtime error is not a jump the lowering emits and
    /// nothing unwound them. The next top-level call on the same machine must
    /// not be built on top of them.
    pub(crate) fn reset_stack(&mut self) {
        self.stack.words.clear();
    }

    /// The word at `slot` of the frame based at `base`.
    #[inline]
    pub(crate) fn slot(&self, base: u64, slot: u32) -> u64 {
        self.read(base + slot as u64)
    }

    /// Writes `word` to `slot` of the frame based at `base`.
    #[inline]
    pub(crate) fn set_slot(&mut self, base: u64, slot: u32, word: u64) {
        self.write(base + slot as u64, word);
    }

    /// How many words of this task's segment are committed.
    pub(crate) fn stack_words(&self) -> u64 {
        self.stack.words.len() as u64
    }

    // --- the heap region ----------------------------------------------------

    /// Allocates an object of `layout`, answering the address of its header.
    ///
    /// See [`Space::alloc`]: `len` is the header's length field,
    /// `payload_words` is what [`Layout::payload_words`] answers for the two,
    /// and `None` is an invitation to collect and ask again rather than an
    /// error.
    pub(crate) fn alloc(&mut self, layout: LayoutId, len: u32, payload_words: u32) -> Option<u64> {
        self.space.alloc(layout, len, payload_words)
    }

    /// Re-labels the object at `addr` as `layout` with header length `len`,
    /// releasing the `spare` words the shorter object gives up.
    ///
    /// One operation rather than a header write, because the two halves are
    /// one invariant: the heap is a walkable sequence of objects from
    /// [`STACK_WORDS`] to the bump pointer, so a run of words an object stops
    /// occupying has to become a free block of its own rather than silently
    /// disappear. The caller must pass exactly the words the new object gives
    /// up — `1 + payload_words(before)` less `1 + payload_words(after)` — and
    /// a disagreement makes the heap unwalkable in the same way a wrong
    /// `payload_words` at [`Memory::alloc`] does. `payload` is the new
    /// object's own payload words, which is where the released block begins;
    /// it is not `len`, because a header's length counts *elements* and one
    /// element of an `Array<Point>` is two words.
    ///
    /// `Vector.freeze()` is what needs it: a growable store becomes the
    /// immutable array it is already holding, in place, so that the one O(1)
    /// sequence conversion the language has is O(1) here too. Nothing else
    /// re-labels an object, and nothing may re-label one *upward* — the new
    /// object has to fit in the words the old one had.
    ///
    /// The released block does not join the free list: the next sweep walks
    /// the heap and rebuilds that, and until then the words are neither
    /// reachable nor handed out.
    pub(crate) fn relabel(
        &mut self,
        addr: u64,
        layout: LayoutId,
        len: u32,
        payload: u32,
        spare: u32,
    ) {
        self.write(addr, header(layout, len));
        if spare > 0 {
            // The block is a header and `spare - 1` payload words, which is
            // the smallest thing a free run can be when `spare` is one.
            self.write(addr + 1 + payload as u64, header(LayoutId::FREE, spare - 1));
        }
    }

    /// The layout of the object whose header is at `addr`.
    #[inline]
    pub(crate) fn object_layout(&self, addr: u64) -> LayoutId {
        header_layout(self.read(addr))
    }

    /// The length field of the object whose header is at `addr`.
    #[inline]
    pub(crate) fn object_len(&self, addr: u64) -> u32 {
        header_len(self.read(addr))
    }

    /// Payload word `at` of the object whose header is at `addr`.
    #[inline]
    pub(crate) fn payload(&self, addr: u64, at: u32) -> u64 {
        self.read(addr + 1 + at as u64)
    }

    /// Writes payload word `at` of the object whose header is at `addr`.
    #[inline]
    pub(crate) fn set_payload(&mut self, addr: u64, at: u32, word: u64) {
        self.write(addr + 1 + at as u64, word);
    }

    /// The address of payload word `at`, for a place that names a field or an
    /// element.
    #[inline]
    pub(crate) fn payload_addr(&self, addr: u64, at: u32) -> u64 {
        addr + 1 + at as u64
    }

    /// Words the heap region occupies, free blocks included.
    pub(crate) fn heap_words(&self) -> u64 {
        self.space.heap_words()
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub(crate) fn allocated_words(&self) -> u64 {
        self.space.allocated_words()
    }

    /// How many collections have run.
    pub(crate) fn collections(&self) -> u64 {
        self.space.collections()
    }

    // --- mark and sweep -----------------------------------------------------

    /// Stops the world and collects. See [`Space::collect`].
    ///
    /// `&self`, and that is not an oversight about what a collection does to
    /// the heap: the heap is behind an `Arc` shared by every task of the run,
    /// so a collection was never this task's exclusive access to anything.
    /// What it buys the caller is that the same borrow can carry its roots —
    /// which are its own frames, read out of this same memory.
    pub(crate) fn collect(&self, layouts: &[Layout], roots: &dyn Roots) -> Collected {
        self.space.collect(self.at, layouts, roots)
    }

    /// What this task does at a safepoint. See [`Space::poll`].
    ///
    /// One relaxed load when no collection is pending, which is every time but
    /// the rare one.
    pub(crate) fn poll(&self, roots: &dyn Roots) {
        self.space.poll(self.at, roots);
    }

    /// Publishes `roots` and stays at a safepoint until the answer is dropped.
    ///
    /// What a task takes around anything that blocks and is not an
    /// instruction: a host call, and the join at an `await`. Neither reaches
    /// [`Memory::poll`] while it waits, so without this a collection would
    /// wait for a task that is waiting for something outside the run
    /// altogether. [`Memory::wait`] is the same thing for a word of this
    /// memory and is written in terms of it.
    pub(crate) fn blocking(&self, roots: &dyn Roots) -> Blocking<'_> {
        self.space.blocking(self.at, roots)
    }

    /// The same park, in a guard that borrows nothing. See [`Parked`].
    pub(crate) fn park(&self, roots: &dyn Roots) -> Parked {
        self.space.arrive(self.at, roots);
        Parked {
            space: Arc::clone(&self.space),
            me: self.at,
        }
    }

    /// Makes `addr` a root for as long as the answer lives. See [`Rooted`].
    pub(crate) fn pin(&self, addr: u64) -> Rooted {
        self.space.pin(addr);
        Rooted {
            space: Arc::clone(&self.space),
            addr,
        }
    }

    /// Whether `rooted` names an object of *this* memory.
    ///
    /// A closure built by one run cannot be called by another, and this is
    /// what says so rather than a convention: an address is a word index into
    /// one address space, and the same number names a different object in the
    /// next one. Two runs in one process is an ordinary thing for an embedder
    /// to do, so the question is asked rather than assumed.
    pub(crate) fn is_mine(&self, rooted: &Rooted) -> bool {
        Arc::ptr_eq(&self.space, &rooted.space)
    }

    // --- waiting on a word --------------------------------------------------

    /// Sets the word at `addr` to `word` if it is `expect`. See
    /// [`Space::acquire_word`].
    pub(crate) fn acquire_word(&self, addr: u64, expect: u64, word: u64) -> Result<(), u64> {
        self.space.acquire_word(addr, expect, word)
    }

    /// Writes `word` at `addr`, publishing every write made before it. See
    /// [`Space::release_word`].
    pub(crate) fn release_word(&self, addr: u64, word: u64) {
        self.space.release_word(addr, word);
    }

    /// Blocks until [`Memory::wake`] names `addr` and the word there is no
    /// longer `was`, staying at a safepoint for the whole wait.
    pub(crate) fn wait(&self, addr: u64, was: u64, roots: &dyn Roots) {
        self.space.wait(self.at, addr, was, roots);
    }

    /// Wakes every task waiting on a word of this memory.
    pub(crate) fn wake(&self, addr: u64) {
        self.space.wake(addr);
    }
}

#[cfg(test)]
impl Memory {
    /// The free blocks the last sweep left, in address order.
    fn free_blocks(&self) -> Vec<u64> {
        self.space.allocator().free.clone()
    }

    /// How many words the free block at `addr` occupies, header included.
    fn block_words(&self, addr: u64) -> u64 {
        self.space.block_words(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Barrier;

    /// The roots of a test: a list, walked in order.
    struct Held(Vec<u64>);

    impl Roots for Held {
        fn each_root(&self, f: &mut dyn FnMut(u64)) {
            for &addr in &self.0 {
                f(addr);
            }
        }
    }

    /// A table whose index 0 is the reserved free layout, as a program's is,
    /// and whose next three are the scalars every fixture below builds on.
    ///
    /// A layout table is now what every width, offset and reference map is
    /// read out of, so a fixture builds one rather than writing `Repr`s at
    /// each use — which is also what stops a test from agreeing with a
    /// machine that had a width wrong.
    struct Table(Vec<Layout>);

    const INT: LayoutId = LayoutId(1);
    const REF: LayoutId = LayoutId(2);
    const FLOAT: LayoutId = LayoutId(3);

    impl Table {
        fn new() -> Table {
            Table(vec![
                Layout::free(),
                Layout::word("Int", Repr::Int),
                Layout::word("String", Repr::Ref),
                Layout::word("Float", Repr::Float),
            ])
        }

        /// An inline family, whose words are the concatenation of its parts'.
        fn inline(&mut self, name: &str, shape: Shape, words: Vec<Repr>) -> LayoutId {
            self.0.push(Layout::inline(name, shape, words));
            LayoutId(self.0.len() as u32 - 1)
        }

        /// A family that lives in the heap, so a value of it is one address.
        fn object(&mut self, name: &str, shape: Shape) -> LayoutId {
            self.0.push(Layout::object(name, shape));
            LayoutId(self.0.len() as u32 - 1)
        }

        /// A struct of these fields, laid out inline.
        fn structure(&mut self, name: &str, fields: &[(&str, LayoutId)]) -> LayoutId {
            let named: Vec<(Arc<str>, LayoutId)> = fields
                .iter()
                .map(|(name, id)| (Arc::from(*name), *id))
                .collect();
            let (fields, words) = cove_lir::struct_layout(&named, &self.0);
            self.inline(
                name,
                Shape::Struct {
                    fields,
                    opaque: false,
                },
                words,
            )
        }

        /// An enum of these cases, under the payload-agreement rule.
        fn enumeration(&mut self, name: &str, cases: &[(&str, Vec<LayoutId>)]) -> LayoutId {
            let named: Vec<(Arc<str>, Vec<LayoutId>)> = cases
                .iter()
                .map(|(name, parts)| (Arc::from(*name), parts.clone()))
                .collect();
            let (cases, payload) = cove_lir::enum_layout(&named, &self.0);
            let mut words = vec![Repr::Int];
            words.extend_from_slice(&payload);
            self.inline(name, Shape::Enum { cases, payload }, words)
        }

        fn layouts(&self) -> &[Layout] {
            &self.0
        }

        fn payload_words(&self, id: LayoutId, len: u32) -> u32 {
            self.0[id.index()].payload_words(len, &self.0)
        }
    }

    /// A leaf: an array of integers, which nothing traces into.
    fn leaf(table: &mut Table) -> LayoutId {
        table.object(
            "Array",
            Shape::Elements {
                elem: INT,
                growable: false,
            },
        )
    }

    /// Allocates an object of `id` with header length `len`, sized the way
    /// the machine sizes one.
    fn alloc(mem: &mut Memory, table: &Table, id: LayoutId, len: u32) -> u64 {
        let words = table.payload_words(id, len);
        mem.alloc(id, len, words).expect("the fixture has room")
    }

    #[test]
    fn a_frame_starts_zeroed() {
        let mut mem = Memory::new(16);
        let base = mem.push_frame(4).unwrap();
        assert_eq!(base, 0);
        for slot in 0..4 {
            assert_eq!(mem.slot(base, slot), 0);
        }
    }

    #[test]
    fn popping_restores_the_base_and_the_zeroes() {
        let mut mem = Memory::new(16);
        let first = mem.push_frame(3).unwrap();
        mem.set_slot(first, 2, 0xdead_beef);
        let second = mem.push_frame(3).unwrap();
        assert_eq!(second, first + 3);

        mem.pop_frame(second);
        assert_eq!(mem.stack_words(), 3);
        // The same words come back, and they come back zero: a `Ref` slot the
        // callee has not written must read as null, not as the last frame's
        // address.
        let again = mem.push_frame(3).unwrap();
        assert_eq!(again, second);
        assert_eq!(mem.slot(again, 0), 0);
        assert_eq!(mem.slot(again, 2), 0);
    }

    #[test]
    fn the_stack_segment_is_the_limit() {
        let mut mem = Memory::new(16);
        // One frame just short of the segment, then one word too many.
        let base = mem.push_frame(SEGMENT_WORDS as u32 - 2).unwrap();
        assert_eq!(base, 0);
        assert_eq!(mem.push_frame(2), Err(Overflow));
        assert_eq!(mem.push_frame(1).unwrap(), SEGMENT_WORDS - 2);
    }

    /// A second task's frames are in a segment of its own, and the two ranges
    /// cannot meet.
    ///
    /// This is the whole of why an address formed in one task cannot be
    /// confused with one formed in another: the segments partition the
    /// reserved region, a frame is refused the moment it would leave one, and
    /// the region decoder — `addr < STACK_WORDS` — never had to learn about
    /// any of it.
    #[test]
    fn a_second_task_gets_a_segment_of_its_own() {
        let mut first = Memory::new(64);
        let mut second = first.for_task().unwrap();
        assert_eq!(first.segment(), 0);
        assert_eq!(second.segment(), 1);
        assert_eq!(first.tasks(), 2);

        let here = first.push_frame(4).unwrap();
        let there = second.push_frame(4).unwrap();
        assert_eq!(here, 0);
        assert_eq!(there, SEGMENT_WORDS);
        assert!(is_stack(here) && is_stack(there));
        // The same slot number, two different addresses, and neither task's
        // frame can reach the other's however deep it nests: the segment ends
        // first.
        assert!(there >= here + SEGMENT_WORDS);
        first.set_slot(here, 0, 11);
        second.set_slot(there, 0, 22);
        assert_eq!(first.slot(here, 0), 11);
        assert_eq!(second.slot(there, 0), 22);

        // A segment comes back when its task ends.
        drop(second);
        assert_eq!(first.tasks(), 1);
        let third = first.for_task().unwrap();
        assert_eq!(third.segment(), 1);
    }

    #[test]
    fn an_address_reads_the_same_way_in_either_region() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(16);
        let base = mem.push_frame(2).unwrap();
        let object = alloc(&mut mem, &table, array, 2);

        let local = base + 1;
        let element = mem.payload_addr(object, 1);
        assert!(is_stack(local));
        assert!(!is_stack(element));

        // One `write`, one `read`, two regions. This is what lets a `Repr::Addr`
        // word name a local or a field without the instruction knowing which.
        mem.write(local, 7);
        mem.write(element, 9);
        assert_eq!(mem.read(local), 7);
        assert_eq!(mem.read(element), 9);
        assert_eq!(mem.slot(base, 1), 7);
        assert_eq!(mem.payload(object, 1), 9);
    }

    /// The copy ADR 0001 asks for, in the two regions and between them.
    ///
    /// A `Wrapper { p: Point, v: Vector }` is three words. Copying it copies
    /// all three: the two `Point` words become independent of the source and
    /// the `Vector` address is duplicated, so both wrappers name one vector.
    /// Nothing decided which of the two happened — one word-range copy did
    /// both, which is the whole of what replaced the sharing bit.
    #[test]
    fn a_copy_is_a_run_of_words_wherever_it_goes() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(64);
        let base = mem.push_frame(8).unwrap();
        let object = alloc(&mut mem, &table, array, 3);

        // `a` at slots 0..3: two `Point` words and a `Vector` address.
        for (slot, word) in [(0, 1), (1, 2), (2, object)] {
            mem.set_slot(base, slot, word);
        }
        mem.copy_words(base + 3, base, 3);
        assert_eq!(
            (0..6).map(|s| mem.slot(base, s)).collect::<Vec<_>>(),
            vec![1, 2, object, 1, 2, object]
        );

        // Writing through the copy leaves the source alone: `b.x = 7` is one
        // word of `b`, and `a.x` is a different word.
        mem.set_slot(base, 3, 7);
        assert_eq!(mem.slot(base, 0), 1);

        // Out to a heap payload and back, which is the same operation: a
        // struct in an array element is inline in that payload.
        mem.copy_words(mem.payload_addr(object, 0), base + 3, 3);
        assert_eq!(mem.payload(object, 0), 7);
        assert_eq!(mem.payload(object, 2), object);
        mem.copy_words(base + 6, mem.payload_addr(object, 0), 2);
        assert_eq!(mem.slot(base, 6), 7);
        assert_eq!(mem.slot(base, 7), 2);
    }

    /// A copy within one region moves rather than smears, so a lowering may
    /// emit one whose source and destination overlap rather than having to
    /// prove they do not.
    #[test]
    fn an_overlapping_copy_moves_rather_than_smears() {
        let mut mem = Memory::new(16);
        let base = mem.push_frame(5).unwrap();
        for slot in 0..5 {
            mem.set_slot(base, slot, slot as u64 + 1);
        }
        mem.copy_words(base + 1, base, 4);
        assert_eq!(
            (0..5).map(|s| mem.slot(base, s)).collect::<Vec<_>>(),
            vec![1, 1, 2, 3, 4]
        );
    }

    /// And so does one in the heap, in either direction.
    #[test]
    fn an_overlapping_heap_copy_moves_in_either_direction() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(64);
        let object = alloc(&mut mem, &table, array, 6);
        for at in 0..6 {
            mem.set_payload(object, at, at as u64 + 1);
        }
        // Up: the destination is above the source and overlaps it.
        mem.copy_words(mem.payload_addr(object, 1), mem.payload_addr(object, 0), 4);
        assert_eq!(
            (0..6).map(|at| mem.payload(object, at)).collect::<Vec<_>>(),
            vec![1, 1, 2, 3, 4, 6]
        );
        // Down: the other direction of the same overlap.
        mem.copy_words(mem.payload_addr(object, 0), mem.payload_addr(object, 1), 4);
        assert_eq!(
            (0..6).map(|at| mem.payload(object, at)).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 4, 6]
        );
    }

    /// A run that crosses a chunk boundary is still one run.
    ///
    /// The heap's backing store is a spine of chunks, and every copy, read and
    /// clear splits itself across them. This allocates an object longer than a
    /// chunk so that every one of those paths has a boundary in the middle of
    /// it.
    #[test]
    fn a_run_crosses_a_chunk_boundary_whole() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let words = (CHUNK_WORDS + 32) as u32;
        let mut mem = Memory::new(4 * CHUNK_WORDS as usize);
        let object = alloc(&mut mem, &table, array, words);
        let base = mem.push_frame(words + 1).unwrap();
        for at in 0..words {
            mem.set_slot(base, at, at as u64 + 1);
        }

        // Stack to heap, heap to stack, and heap to heap, each across the
        // boundary the chunk at `CHUNK_WORDS` puts in the middle.
        mem.copy_words(mem.payload_addr(object, 0), base, words);
        assert_eq!(mem.payload(object, 0), 1);
        assert_eq!(mem.payload(object, words - 1), words as u64);
        assert_eq!(
            mem.read_words(mem.payload_addr(object, 0), words).last(),
            Some(&(words as u64))
        );

        mem.clear_words(base, words);
        mem.copy_words(base, mem.payload_addr(object, 0), words);
        assert_eq!(mem.slot(base, 0), 1);
        assert_eq!(mem.slot(base, words - 1), words as u64);

        mem.clear_words(mem.payload_addr(object, 0), words);
        assert_eq!(mem.payload(object, 0), 0);
        assert_eq!(mem.payload(object, words - 1), 0);
    }

    #[test]
    fn clearing_zeroes_the_words_a_layout_names() {
        let mut mem = Memory::new(16);
        let base = mem.push_frame(4).unwrap();
        for slot in 0..4 {
            mem.set_slot(base, slot, 0xfeed);
        }
        mem.clear_words(base + 1, 2);
        assert_eq!(
            (0..4).map(|s| mem.slot(base, s)).collect::<Vec<_>>(),
            vec![0xfeed, 0, 0, 0xfeed]
        );
        assert_eq!(mem.read_words(base, 4), vec![0xfeed, 0, 0, 0xfeed]);
    }

    #[test]
    fn an_object_round_trips_its_header_and_payload() {
        let mut table = Table::new();
        let text = table.object("String", Shape::Str);
        let mut mem = Memory::new(16);
        let object = alloc(&mut mem, &table, text, 24);
        assert_eq!(object, STACK_WORDS);
        assert_eq!(mem.object_layout(object), text);
        assert_eq!(mem.object_len(object), 24);
        // The payload is zero before anything writes it.
        assert_eq!(mem.payload(object, 0), 0);
        mem.set_payload(object, 2, u64::MAX);
        assert_eq!(mem.payload(object, 2), u64::MAX);
        // A full-width layout id and length survive the packing.
        let word = header(LayoutId(u32::MAX), u32::MAX);
        assert_eq!(header_layout(word), LayoutId(u32::MAX));
        assert_eq!(header_len(word), u32::MAX);
    }

    #[test]
    fn allocation_stops_at_the_budget() {
        let mut mem = Memory::new(4);
        assert!(mem.alloc(LayoutId(1), 2, 2).is_some());
        // One word left, and the smallest object is two.
        assert_eq!(mem.alloc(LayoutId(1), 1, 1), None);
        assert_eq!(mem.alloc(LayoutId(1), 0, 0), Some(STACK_WORDS + 3));
        assert_eq!(mem.alloc(LayoutId(1), 0, 0), None);
        assert_eq!(mem.heap_words(), 4);
    }

    #[test]
    fn a_collection_frees_what_no_root_reaches() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(64);
        let kept = alloc(&mut mem, &table, array, 2);
        let lost = alloc(&mut mem, &table, array, 2);
        mem.set_payload(kept, 0, 41);
        mem.set_payload(lost, 0, 42);

        let done = mem.collect(table.layouts(), &Held(vec![kept]));
        assert_eq!(done.live_words, 3);
        assert_eq!(done.freed_words, 3);
        assert_eq!(done.collections, 1);
        // Non-moving: the survivor is where it was, with what it held.
        assert_eq!(mem.object_layout(kept), array);
        assert_eq!(mem.payload(kept, 0), 41);
        // The reclaimed run is a walkable free block.
        assert_eq!(mem.free_blocks(), vec![lost]);
        assert_eq!(mem.object_layout(lost), LayoutId::FREE);
        assert_eq!(mem.object_len(lost), 2);
    }

    #[test]
    fn a_freed_block_is_handed_out_again() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(3);
        let first = alloc(&mut mem, &table, array, 2);
        assert_eq!(mem.alloc(array, 2, 2), None);

        mem.set_payload(first, 1, 0xfeed);
        mem.collect(table.layouts(), &Held(vec![]));

        // The retry the `None` above is an invitation to. The block comes back
        // zeroed, because a `Ref` field of the new object must read as null.
        let second = alloc(&mut mem, &table, array, 2);
        assert_eq!(second, first);
        assert_eq!(mem.payload(second, 1), 0);
        assert!(mem.free_blocks().is_empty());
    }

    #[test]
    fn a_split_leaves_a_free_block_behind() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(8);
        let big = alloc(&mut mem, &table, array, 4);
        mem.collect(table.layouts(), &Held(vec![]));

        let small = alloc(&mut mem, &table, array, 1);
        assert_eq!(small, big);
        // Three words remain, described by a header of their own so the sweeper
        // can still walk over them.
        assert_eq!(mem.free_blocks(), vec![big + 2]);
        assert_eq!(mem.object_layout(big + 2), LayoutId::FREE);
        assert_eq!(mem.object_len(big + 2), 2);
    }

    #[test]
    fn adjacent_free_blocks_coalesce() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(64);
        let a = alloc(&mut mem, &table, array, 1);
        let _b = alloc(&mut mem, &table, array, 2);
        let _c = alloc(&mut mem, &table, array, 1);
        let kept = alloc(&mut mem, &table, array, 1);
        mem.collect(table.layouts(), &Held(vec![kept]));

        // Three dead objects, one block: 2 + 3 + 2 words, and a request no one
        // of them could have satisfied now fits.
        assert_eq!(mem.free_blocks(), vec![a]);
        assert_eq!(mem.block_words(a), 7);
        assert_eq!(alloc(&mut mem, &table, array, 6), a);
    }

    /// The simplification the run-of-words model bought the collector.
    ///
    /// An enum's payload words have one static map, because the lowering
    /// assigns case offsets so that every case using a word agrees on its
    /// `Repr`. So nothing reads the discriminant: `None` and `Some` are
    /// traced by the same map, and what keeps a `None` from retaining what a
    /// `Some` left behind is that constructing a case zeroes the region it
    /// does not fill — a fact about the data rather than a question at
    /// collection time.
    #[test]
    fn an_enums_payload_is_traced_by_a_static_map() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        // `enum E { A(Int, String), B(Float) }`: `[disc, Int, Ref, Float]`.
        let e = table.enumeration("E", &[("A", vec![INT, REF]), ("B", vec![FLOAT])]);
        assert_eq!(
            table.layouts()[e.index()].words,
            vec![Repr::Int, Repr::Int, Repr::Ref, Repr::Float]
        );

        // A boxed `E`, so that there is an object to trace: word 2 of the
        // payload is the reference whichever case the value is in.
        for (case, live) in [(0u64, true), (1, false)] {
            let mut mem = Memory::new(64);
            let target = alloc(&mut mem, &table, array, 1);
            let held = alloc(&mut mem, &table, e, 0);
            mem.set_payload(held, 0, case);
            if live {
                mem.set_payload(held, 2, target);
            }

            let done = mem.collect(table.layouts(), &Held(vec![held]));
            assert_eq!(mem.object_layout(target) != LayoutId::FREE, live);
            assert_eq!(done.freed_words, if live { 0 } else { 2 });
        }
    }

    /// An `Array<Point>` is a run of two-word elements, not a run of
    /// addresses, and a `Set` of them is the same run kept sorted. The
    /// collector walks each element's own map at the element's stride.
    #[test]
    fn a_run_of_multiword_elements_is_walked_at_its_stride() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        // `struct Tagged { name: String, count: Int }`: `[Ref, Int]`.
        let tagged = table.structure("Tagged", &[("name", REF), ("count", INT)]);
        let of_tagged = table.object(
            "Array",
            Shape::Elements {
                elem: tagged,
                growable: false,
            },
        );
        assert_eq!(table.payload_words(of_tagged, 3), 6);

        let mut mem = Memory::new(64);
        let first = alloc(&mut mem, &table, array, 1);
        let second = alloc(&mut mem, &table, array, 1);
        let lost = alloc(&mut mem, &table, array, 1);
        let items = alloc(&mut mem, &table, of_tagged, 3);
        // Element 0's name, element 2's name, and the counts in between —
        // which are the words a stride of one would have followed.
        mem.set_payload(items, 0, first);
        mem.set_payload(items, 1, lost);
        mem.set_payload(items, 4, second);
        mem.set_payload(items, 5, lost);

        mem.collect(table.layouts(), &Held(vec![items]));
        assert_ne!(mem.object_layout(first), LayoutId::FREE);
        assert_ne!(mem.object_layout(second), LayoutId::FREE);
        assert_eq!(
            mem.object_layout(lost),
            LayoutId::FREE,
            "a count is an `Int` however plausible an address it holds"
        );
    }

    /// A box holds a `LayoutId` and then the value's words, so a boxed
    /// multiword value is that value inline rather than a reference to
    /// somewhere else again.
    #[test]
    fn a_box_is_traced_by_the_layout_it_names() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let tagged = table.structure("Tagged", &[("name", REF), ("count", INT)]);
        let boxed = table.object("Any", Shape::Boxed);
        assert_eq!(table.payload_words(boxed, 2), 3);

        for held in [tagged, INT] {
            let mut mem = Memory::new(64);
            let target = alloc(&mut mem, &table, array, 1);
            let width = table.layouts()[held.index()].width();
            let object = alloc(&mut mem, &table, boxed, width);
            mem.set_payload(object, 0, held.0 as u64);
            mem.set_payload(object, 1, target);

            mem.collect(table.layouts(), &Held(vec![object]));
            // The same bits under two layouts. An `Int` that happens to equal
            // an address is not one, and the box is the only thing that knows.
            assert_eq!(mem.object_layout(target) != LayoutId::FREE, held == tagged);
        }
    }

    /// A closure's captures are inline in its environment, each at its own
    /// width, which is what "a struct inside a closure environment is laid
    /// out the way a struct in a frame is" means when the collector reads it.
    #[test]
    fn a_closures_captures_are_walked_inline() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let tagged = table.structure("Tagged", &[("name", REF), ("count", INT)]);
        let closure = table.object(
            "closure",
            Shape::Closure {
                function: cove_lir::FunctionId(0),
                captures: vec![INT, tagged, REF],
            },
        );
        assert_eq!(table.payload_words(closure, 0), 5);

        let mut mem = Memory::new(64);
        let name = alloc(&mut mem, &table, array, 1);
        let last = alloc(&mut mem, &table, array, 1);
        let lost = alloc(&mut mem, &table, array, 1);
        let object = alloc(&mut mem, &table, closure, 0);
        // Payload: [callee, Int, Ref, Int, Ref].
        mem.set_payload(object, 1, lost);
        mem.set_payload(object, 2, name);
        mem.set_payload(object, 3, lost);
        mem.set_payload(object, 4, last);

        mem.collect(table.layouts(), &Held(vec![object]));
        assert_ne!(mem.object_layout(name), LayoutId::FREE);
        assert_ne!(mem.object_layout(last), LayoutId::FREE);
        assert_eq!(mem.object_layout(lost), LayoutId::FREE);
    }

    /// A `Map<String, Point>` is a run of three-word entries and only the
    /// first word of each is a root.
    #[test]
    fn a_maps_entries_are_walked_key_then_value() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let point = table.structure("Point", &[("x", INT), ("y", INT)]);
        let map = table.object(
            "Map",
            Shape::Entries {
                key: REF,
                value: point,
            },
        );
        assert_eq!(table.payload_words(map, 2), 6);

        let mut mem = Memory::new(64);
        let key = alloc(&mut mem, &table, array, 1);
        let lost = alloc(&mut mem, &table, array, 1);
        let entries = alloc(&mut mem, &table, map, 2);
        // Entry 0 is `[key, x, y]` and entry 1 is `[key, x, y]`, so words 1
        // and 2 are a `Point`'s and nothing follows them however plausible an
        // address they hold.
        mem.set_payload(entries, 0, key);
        mem.set_payload(entries, 1, lost);
        mem.set_payload(entries, 2, lost);
        mem.set_payload(entries, 4, lost);

        mem.collect(table.layouts(), &Held(vec![entries]));
        assert_ne!(mem.object_layout(key), LayoutId::FREE);
        assert_eq!(mem.object_layout(lost), LayoutId::FREE);
    }

    #[test]
    fn an_interior_address_survives_a_collection() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let holder = table.structure("Holder", &[("it", REF)]);
        let mut mem = Memory::new(64);
        let target = alloc(&mut mem, &table, array, 3);
        let base = alloc(&mut mem, &table, holder, 0);
        let _garbage = alloc(&mut mem, &table, array, 3);
        mem.set_payload(base, 0, target);

        // What a `var` argument naming an element carries: the address of one
        // word inside an object. It is not a root — the object is held by the
        // `Ref` field of `base` — and it is not rewritten, because nothing
        // moves.
        let element = mem.payload_addr(target, 1);
        mem.write(element, 0xc0ffee);

        let done = mem.collect(table.layouts(), &Held(vec![base]));
        assert_eq!(done.freed_words, 4);
        assert_eq!(mem.read(element), 0xc0ffee);
        assert_eq!(mem.payload(target, 1), 0xc0ffee);
        assert_eq!(mem.object_layout(target), array);
    }

    #[test]
    fn a_deep_graph_does_not_recurse() {
        // A linked list is an ordinary value. Marking it must cost heap, not
        // Rust stack.
        let mut table = Table::new();
        let holder = table.structure("Node", &[("next", REF)]);
        let mut mem = Memory::new(1 << 20);
        let head = alloc(&mut mem, &table, holder, 0);
        let mut tail = head;
        for _ in 0..200_000 {
            let next = alloc(&mut mem, &table, holder, 0);
            mem.set_payload(tail, 0, next);
            tail = next;
        }

        let done = mem.collect(table.layouts(), &Held(vec![head]));
        assert_eq!(done.live_words, 200_001 * 2);
        assert_eq!(done.freed_words, 0);
    }

    #[test]
    fn a_cycle_is_marked_once() {
        let mut table = Table::new();
        let holder = table.structure("Node", &[("next", REF)]);
        let mut mem = Memory::new(64);
        let a = alloc(&mut mem, &table, holder, 0);
        let b = alloc(&mut mem, &table, holder, 0);
        mem.set_payload(a, 0, b);
        mem.set_payload(b, 0, a);

        // Reachable, and the mark bit is what stops the walk. Then unreachable,
        // which is the whole reason a tracing collector is here rather than a
        // reference count.
        assert_eq!(mem.collect(table.layouts(), &Held(vec![a])).live_words, 4);
        assert_eq!(mem.collect(table.layouts(), &Held(vec![])).freed_words, 4);
    }

    #[test]
    fn a_null_root_is_ordinary() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut mem = Memory::new(64);
        let kept = alloc(&mut mem, &table, array, 1);
        // A slot that has not been written yet, and one that `Clear` emptied.
        let done = mem.collect(table.layouts(), &Held(vec![0, kept, 0]));
        assert_eq!(done.live_words, 2);
        assert_eq!(mem.object_layout(kept), array);
    }

    #[test]
    fn a_leaf_is_not_read_at_all() {
        // `may_hold_refs` is what a string costs a collection: a table lookup
        // and no word reads. The payload here is a plausible address, and it is
        // never followed.
        let mut table = Table::new();
        let array = leaf(&mut table);
        let text = table.object("String", Shape::Str);
        let mut mem = Memory::new(64);
        let target = alloc(&mut mem, &table, array, 1);
        let held = alloc(&mut mem, &table, text, 8);
        mem.set_payload(held, 0, target);

        mem.collect(table.layouts(), &Held(vec![held]));
        assert_eq!(mem.object_layout(target), LayoutId::FREE);
    }

    // --- two tasks over one heap ------------------------------------------

    /// A task's memory can be given to the thread that will run it.
    ///
    /// The one thing `spawn` needs of this module and cannot work around: a
    /// `Memory` is `Send`, so the run's heap goes with the task rather than
    /// being copied to it.
    #[test]
    fn a_task_memory_crosses_to_its_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Memory>();

        let first = Memory::new(1 << 16);
        let mut second = first.for_task().unwrap();
        let done = std::thread::spawn(move || {
            let base = second.push_frame(2).unwrap();
            second.set_slot(base, 0, 7);
            second.alloc(LayoutId(1), 0, 1).unwrap()
        })
        .join()
        .unwrap();
        // The object the other thread allocated is in this run's heap, at an
        // address this task can read: one heap, one address space.
        assert_eq!(done, STACK_WORDS);
        assert_eq!(first.object_layout(done), LayoutId(1));
    }

    /// Two threads allocating at once are given disjoint runs of words.
    ///
    /// The whole of what the allocator's lock is for. Each thread writes its
    /// own mark into every payload word of everything it allocates, and every
    /// object still reads back as one thread's: if two allocations had
    /// overlapped, one of them would be holding the other's mark.
    #[test]
    fn two_threads_allocate_disjoint_words() {
        const EACH: u64 = 2000;
        let mut table = Table::new();
        let array = leaf(&mut table);
        let words = table.payload_words(array, 3);
        let mut first = Memory::new(1 << 20);
        let second = first.for_task().unwrap();

        let start = Barrier::new(2);
        let mine: Vec<u64> = std::thread::scope(|scope| {
            let theirs = scope.spawn(|| {
                let mut mem = second;
                start.wait();
                let mut held = Vec::new();
                for _ in 0..EACH {
                    let addr = mem.alloc(array, 3, words).expect("the fixture has room");
                    for at in 0..words {
                        mem.set_payload(addr, at, 2);
                    }
                    held.push(addr);
                }
                held
            });
            start.wait();
            let mut held = Vec::new();
            for _ in 0..EACH {
                let addr = first.alloc(array, 3, words).expect("the fixture has room");
                for at in 0..words {
                    first.set_payload(addr, at, 1);
                }
                held.push(addr);
            }
            let mut theirs = theirs.join().unwrap();
            theirs.extend_from_slice(&held);
            theirs
        });

        assert_eq!(mine.len() as u64, 2 * EACH);
        let mut seen: Vec<u64> = mine.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), mine.len(), "no two objects share an address");
        for addr in mine {
            let mark = first.payload(addr, 0);
            assert!(mark == 1 || mark == 2);
            for at in 0..words {
                assert_eq!(
                    first.payload(addr, at),
                    mark,
                    "an object holds one thread's writes and not the other's"
                );
            }
        }
    }

    /// A collection waits for every other task and reads the roots it
    /// published.
    ///
    /// The second thread never allocates and never collects. It runs a loop
    /// that polls at what a dispatch loop would call a safepoint, holding one
    /// object that no other task can reach — and it survives, which it could
    /// only do if the collector waited for the publication and read it.
    #[test]
    fn a_collection_reads_the_roots_another_task_published() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut first = Memory::new(1 << 14);
        let mut second = first.for_task().unwrap();

        let theirs = alloc(&mut second, &table, array, 4);
        let ready = Barrier::new(2);
        let stop = AtomicBool::new(false);
        let polls = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                ready.wait();
                while !stop.load(Ordering::Relaxed) {
                    second.poll(&Held(vec![theirs]));
                    polls.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                // One last one, so a collection that began after the flag was
                // read is not left waiting for a task that has stopped asking.
                second.poll(&Held(vec![theirs]));
            });
            ready.wait();

            let mine = alloc(&mut first, &table, array, 4);
            let lost = alloc(&mut first, &table, array, 4);
            assert_ne!(mine, theirs);
            let done = first.collect(table.layouts(), &Held(vec![mine]));
            stop.store(true, Ordering::Relaxed);

            // The other task's object survived and this task's garbage did not.
            assert_eq!(first.object_layout(theirs), array);
            assert_eq!(first.object_layout(mine), array);
            assert_eq!(first.object_layout(lost), LayoutId::FREE);
            assert_eq!(done.freed_words, 5);
        });
        assert!(polls.load(Ordering::Relaxed) > 0);
    }

    /// A task that is blocked is already at a safepoint, so a collection does
    /// not wait for it — and does not free what it is holding.
    ///
    /// This is what keeps a collection from deadlocking behind a task waiting
    /// on a `Shared` cell: the waiter published its roots when it began
    /// waiting and will not poll again until it is woken, and the task that
    /// would wake it is the one trying to collect.
    #[test]
    fn a_collection_does_not_wait_for_a_blocked_task() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut first = Memory::new(1 << 14);
        let mut second = first.for_task().unwrap();

        let theirs = alloc(&mut second, &table, array, 4);
        let gate = alloc(&mut first, &table, array, 0);
        let word = first.payload_addr(gate, 0);
        let waiting = Barrier::new(2);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                waiting.wait();
                // Blocks until the other task writes the word, publishing its
                // roots for the whole wait.
                second.wait(word, 0, &Held(vec![theirs]));
            });
            waiting.wait();
            // Give the other thread a chance to be inside the wait. Whether it
            // is or not, the collection below must terminate: a task that has
            // not arrived yet is one this task waits for, and a task that is
            // blocked is one it does not.
            std::thread::yield_now();

            let lost = alloc(&mut first, &table, array, 4);
            first.collect(table.layouts(), &Held(vec![gate]));
            assert_eq!(first.object_layout(lost), LayoutId::FREE);

            first.release_word(word, 1);
            first.wake(word);
        });
        // Whatever the interleaving, the blocked task's object was published
        // and survived.
        assert_eq!(first.object_layout(theirs), array);
    }

    /// A task inside a host call is at a safepoint, and stays there until the
    /// call returns.
    ///
    /// The same mechanism [`Memory::wait`] uses, taken directly, because a
    /// host call is the other place a task stops running without reaching a
    /// safepoint of its own. The collection below has to finish while the
    /// other thread is still in the call.
    #[test]
    fn a_collection_does_not_wait_for_a_task_inside_a_host_call() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut first = Memory::new(1 << 14);
        let mut second = first.for_task().unwrap();
        let theirs = alloc(&mut second, &table, array, 4);

        let inside = Barrier::new(2);
        let done = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let held = second.blocking(&Held(vec![theirs]));
                inside.wait();
                // The "call": it returns when the other task says so, which is
                // after the collection.
                done.wait();
                drop(held);
            });
            inside.wait();
            let lost = alloc(&mut first, &table, array, 4);
            first.collect(table.layouts(), &Held(vec![]));
            assert_eq!(first.object_layout(lost), LayoutId::FREE);
            assert_eq!(
                first.object_layout(theirs),
                array,
                "what the waiting task published is a root"
            );
            done.wait();
        });
    }

    /// Two tasks asking to collect at once run one collection, not two.
    #[test]
    fn a_second_collector_waits_out_the_first() {
        let mut table = Table::new();
        let array = leaf(&mut table);
        let mut first = Memory::new(1 << 14);
        let mut second = first.for_task().unwrap();
        let mine = alloc(&mut first, &table, array, 1);
        let theirs = alloc(&mut second, &table, array, 1);

        let both = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                both.wait();
                second.collect(table.layouts(), &Held(vec![theirs]));
            });
            both.wait();
            first.collect(table.layouts(), &Held(vec![mine]));
        });

        // Both objects are alive, and the two requests produced at most two
        // collections however they interleaved — the point being that neither
        // deadlocked and neither freed the other's root.
        assert_eq!(first.object_layout(mine), array);
        assert_eq!(first.object_layout(theirs), array);
        assert!(first.collections() >= 1);
    }

    /// A run has as many stack segments as it has tasks, and no more.
    #[test]
    fn a_run_runs_out_of_segments_rather_than_overlapping_them() {
        let first = Memory::new(16);
        let mut held = Vec::new();
        for _ in 1..SEGMENTS {
            held.push(first.for_task().expect("a segment is free"));
        }
        assert_eq!(first.tasks(), SEGMENTS as usize);
        assert_eq!(first.for_task().err(), Some(NoSegment));

        // Every one of them is somewhere else.
        let mut origins: Vec<u64> = held.iter().map(|mem| segment_origin(mem.at)).collect();
        origins.push(0);
        origins.sort_unstable();
        origins.dedup();
        assert_eq!(origins.len(), SEGMENTS as usize);
    }
}
