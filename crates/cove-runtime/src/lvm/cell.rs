//! `Shared`: one ordinary object in the run's one heap, and the lock that is
//! one of its words.
//!
//! [ADR 0008](../../../../docs/adr/0008-concurrent-task-execution.md) makes
//! `Shared<T>` the one handle that crosses a task boundary by *sharing*
//! instead of by copying, and issue #240's Q1 says where it lives: **an
//! ordinary Cove-owned object in the run's heap**, not a store of its own and
//! not a per-task anything. This module is the mechanism that makes that
//! sentence work — what a cell's words are, what synchronises them, and what
//! keeps a task waiting for one from stopping the collector.
//!
//! # A cell is a lock word and a value
//!
//! ~~~text
//! +0  header:  [ layout: Shared | len: 0 ]
//! +1  state:   0, or the tag of the task inside `lock`
//! +2  the wrapped value's words, inline under its own layout
//! ...
//! ~~~
//!
//! The value is **inline**, not behind another reference, for the same reason
//! a struct's fields are inline: a value's words are where the value is. What
//! that buys here is that `lock` hands its closure a `Repr::Addr` naming
//! [`value`] — the ordinary `var` alias the language already describes — and
//! *nothing is copied at all*, in or out. The tree-walking oracle converts the
//! cell's contents to a `Value` on the way in and back to a `Transfer` on the
//! way out, once per `lock`, because two threads cannot both address an
//! `Rc`-based value. Two tasks can both address a word.
//!
//! The collector needs no new idea for it: the payload is a run of words with
//! a static per-word map, exactly like a closure environment's, and the state
//! word is an `Int` in that map so nothing traces it.
//!
//! # What synchronises the cell, and what synchronises the heap
//!
//! They are two things and they are kept apart.
//!
//! The **heap** synchronises handing out words and stopping the world to
//! collect ([`crate::lvm::mem::Space`]). It does not synchronise a *value*,
//! and paying for that on every access would be paying, in every program, for
//! something the task-safety rule already forbids.
//!
//! The **cell** synchronises one value, and it is the only place a value is
//! synchronised. [`lock`] takes the state word with `Acquire` and [`unlock`]
//! releases it with `Release`, so everything the previous holder wrote — the
//! cell's own words, and any object it allocated and stored into them — is
//! visible to whoever takes the cell next. Every other word of this memory is
//! `Relaxed`, and is allowed to be, because a release/acquire pair on one
//! location orders everything either side of it. The lock word *is* the
//! publication.
//!
//! That the lock lives in the cell rather than in a side table is what keeps
//! a cell an ordinary object: there is no table to key by address, nothing to
//! reclaim when a cell dies, and no second lifetime running beside the
//! collector's. A cell is freed by the sweep like anything else.
//!
//! # Waiting for a cell does not stop a collection
//!
//! A task waiting for a cell another task holds cannot reach a safepoint of
//! its own, so a collector that waited for it would be waiting for a task
//! that is waiting for a task that is waiting for the collector. So a waiter
//! *publishes its roots and stays published for the whole wait*
//! ([`crate::lvm::mem::Memory::wait`]): it is not running, its frames do not
//! change, and the snapshot it left is true until it is woken. The collector
//! counts it as arrived and goes ahead.
//!
//! # A cycle through a cell is an ordinary cycle
//!
//! [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
//! decides it, and this module is most of the argument for why it could be
//! decided: a cell is an object in the traced heap, the values reachable
//! through it are objects in the same heap, and the collector ADR 0011's
//! amendment deferred is the collector that is running. So a cell that comes
//! to hold a handle to itself is collected when it becomes unreachable, and
//! nothing on the `lock` path inspects what a closure left.
//!
//! Reentrant locking is the other question and is unchanged: [`Reentrant`] is
//! a live lock state, and no collector can answer one.
//!
//! # The layout is `cove-lir`'s
//!
//! [`Shape::Shared`](cove_lir::Shape::Shared) is the variant, and
//! [`cove_lir::SHARED_STATE`] and [`cove_lir::SHARED_VALUE`] are the two
//! offsets — read from there rather than written again, because the lowering
//! forms the address of the value word and this reads the lock word, and the
//! two are the same object.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::lvm::mem::{Memory, Roots};

/// The payload word holding a cell's lock.
pub(crate) const STATE: u32 = cove_lir::SHARED_STATE;

/// The payload word the wrapped value begins at.
pub(crate) const VALUE: u32 = cove_lir::SHARED_VALUE;

/// What the state word holds when no task is inside `lock`.
const UNLOCKED: u64 = 0;

/// A task asked for a cell it is already inside.
///
/// `lock` refuses rather than waits, because waiting would be waiting for
/// itself. The message and the span belong to the instruction arm that reports
/// it; what is decided here is only that the answer is a refusal and that it
/// is reached without touching the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reentrant;

/// The next tag to hand a thread.
///
/// Starts at one, so that zero can mean "no task holds this cell" in the same
/// word. That is the same reason a `Repr::Host` word is one past its index:
/// a zeroed word has to mean nothing rather than mean the first of something.
static NEXT_TAG: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's tag, taken on first use.
    static TAG: Cell<u64> = const { Cell::new(0) };
}

/// This task's identity in a cell's state word.
///
/// A tag rather than a thread id because the word has to be comparable to
/// zero and to another task's tag and to nothing else; what it is beyond that
/// is not a question anything asks. It is taken lazily, so a run that never
/// reaches a `Shared` never takes one.
pub(crate) fn tag() -> u64 {
    TAG.with(|held| {
        let mut tag = held.get();
        if tag == 0 {
            tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
            held.set(tag);
        }
        tag
    })
}

/// The address of `cell`'s state word.
pub(crate) fn state(mem: &Memory, cell: u64) -> u64 {
    mem.payload_addr(cell, STATE)
}

/// The address of the first word of `cell`'s value.
///
/// What `lock` hands its closure: an ordinary place, of the ordinary width
/// its layout says, aliased rather than copied.
pub(crate) fn value(mem: &Memory, cell: u64) -> u64 {
    mem.payload_addr(cell, VALUE)
}

/// Which task is inside `cell`, or zero.
pub(crate) fn holder(mem: &Memory, cell: u64) -> u64 {
    mem.read(state(mem, cell))
}

/// Takes `cell`, blocking until it is free.
///
/// `roots` is what this task is holding, published for as long as it waits —
/// see the module docs. It is asked for only on the path that blocks, so an
/// uncontended `lock` is one compare-and-swap and nothing else.
///
/// Answers [`Reentrant`] for a task that already holds this cell. That is
/// ADR 0008's rule about `lock` being the whole of the access, read the only
/// way a word can be asked it: the state word already names the holder.
pub(crate) fn lock(mem: &Memory, cell: u64, roots: &dyn Roots) -> Result<(), Reentrant> {
    let word = state(mem, cell);
    let mine = tag();
    loop {
        match mem.acquire_word(word, UNLOCKED, mine) {
            Ok(()) => return Ok(()),
            Err(held) if held == mine => return Err(Reentrant),
            // Waits until the word is no longer the tag that was read, which
            // is either the holder leaving or another waiter taking it first.
            // Either way the next turn of the loop is the one that asks.
            Err(held) => mem.wait(word, held, roots),
        }
    }
}

/// Gives `cell` back, publishing every write made while it was held.
///
/// The caller must be the holder. Both ways out of a lock body reach here —
/// the one that finished and the one that failed — because a cell a failing
/// task never gave back is a cell no task can ever take, and a Cove error is
/// an ordinary answer rather than an abandonment.
pub(crate) fn unlock(mem: &Memory, cell: u64) {
    let word = state(mem, cell);
    debug_assert_eq!(
        mem.read(word),
        tag(),
        "a cell is given back by the task that took it"
    );
    mem.release_word(word, UNLOCKED);
    // Unconditional, because whether anyone is waiting is not a question this
    // word can answer. What it costs is an uncontended mutex on a path that
    // has just run a closure; a contended-bit in the state word would save it
    // and is exactly the kind of thing Q1 says to leave until a measurement
    // asks for it.
    mem.wake(word);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::mem::NoRoots;
    use cove_lir::{Layout, LayoutId, Repr, Shape};
    use std::sync::Barrier;

    const FREE: LayoutId = LayoutId(0);
    const INT: LayoutId = LayoutId(1);
    const CELL: LayoutId = LayoutId(2);
    const REF: LayoutId = LayoutId(3);
    /// A cell wrapping a reference, which is what a cycle needs: the value
    /// word is a `Repr::Ref`, so the collector traces it.
    const RING: LayoutId = LayoutId(4);

    /// A layout table holding two cell families.
    ///
    /// `CELL` wraps an `Int` and is a leaf; `RING` wraps a one-word reference
    /// and is what a cycle among cells is built out of.
    fn table() -> Vec<Layout> {
        vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::object("Shared", Shape::Shared { value: INT }),
            Layout::word("<ref>", Repr::Ref),
            Layout::object("Shared", Shape::Shared { value: REF }),
        ]
    }

    /// The objects a task says it is holding, for a collection that has to be
    /// told rather than able to read a frame.
    struct Held(Vec<u64>);

    impl Roots for Held {
        fn each_root(&self, f: &mut dyn FnMut(u64)) {
            for &addr in &self.0 {
                f(addr);
            }
        }
    }

    /// A cell holding one `Int`, set to `start`.
    fn cell(mem: &mut Memory, start: u64) -> u64 {
        let addr = mem.alloc(CELL, 0, 2).expect("the fixture has room");
        mem.set_payload(addr, VALUE, start);
        addr
    }

    #[test]
    fn a_cell_is_taken_and_given_back() {
        let mut mem = Memory::new(64);
        let it = cell(&mut mem, 7);
        assert_eq!(holder(&mem, it), 0);

        lock(&mem, it, &NoRoots).unwrap();
        assert_eq!(holder(&mem, it), tag());
        // The value is aliased rather than copied: the closure would be
        // handed this address and write through it.
        let place = value(&mem, it);
        mem.write(place, 9);
        unlock(&mem, it);

        assert_eq!(holder(&mem, it), 0);
        assert_eq!(mem.payload(it, VALUE), 9);
    }

    /// `lock` inside `lock` on the same cell is refused, not waited for.
    #[test]
    fn a_task_cannot_take_a_cell_it_is_already_inside() {
        let mut mem = Memory::new(64);
        let it = cell(&mut mem, 0);
        lock(&mem, it, &NoRoots).unwrap();
        assert_eq!(lock(&mem, it, &NoRoots), Err(Reentrant));
        unlock(&mem, it);
        // And the refusal did not take the cell or leave it taken.
        assert_eq!(holder(&mem, it), 0);
    }

    /// Two cells nest, which is why the refusal above is per cell rather than
    /// per task.
    #[test]
    fn two_cells_nest() {
        let mut mem = Memory::new(64);
        let outer = cell(&mut mem, 1);
        let inner = cell(&mut mem, 2);
        lock(&mem, outer, &NoRoots).unwrap();
        lock(&mem, inner, &NoRoots).unwrap();
        assert_eq!(holder(&mem, outer), tag());
        assert_eq!(holder(&mem, inner), tag());
        unlock(&mem, inner);
        unlock(&mem, outer);
    }

    /// The whole of what `Shared` is for: two tasks, one cell, one heap.
    ///
    /// Each turn is a read and a write of the same word through the same
    /// address, which is a race in every arrangement but this one. The total
    /// is exact, which it can only be if every turn saw the previous one — so
    /// this is the mutual exclusion *and* the publication, measured together.
    #[test]
    fn two_tasks_take_turns_over_one_cell() {
        const EACH: u64 = 20_000;
        let mut first = Memory::new(1 << 12);
        let second = first.for_task().unwrap();
        let it = cell(&mut first, 0);

        let start = Barrier::new(2);
        let gate = &start;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut mem = second;
                gate.wait();
                for _ in 0..EACH {
                    lock(&mem, it, &NoRoots).unwrap();
                    let place = value(&mem, it);
                    let was = mem.read(place);
                    mem.write(place, was + 1);
                    unlock(&mem, it);
                }
            });
            start.wait();
            for _ in 0..EACH {
                lock(&first, it, &NoRoots).unwrap();
                let place = value(&first, it);
                let was = first.read(place);
                first.write(place, was + 1);
                unlock(&first, it);
            }
        });

        assert_eq!(first.payload(it, VALUE), 2 * EACH);
    }

    /// A collection runs while another task is waiting for a cell, and frees
    /// nothing the waiter is holding.
    ///
    /// The deadlock this rules out is the one the module docs name: the
    /// collector waits for every task to arrive, and a task waiting for a
    /// cell can only arrive if waiting *is* arriving. The holder here is the
    /// task that collects, so the wait cannot end until the collection does.
    #[test]
    fn a_collection_runs_while_a_task_waits_for_a_cell() {
        let layouts = table();
        let mut first = Memory::new(1 << 12);
        let mut second = first.for_task().unwrap();
        let it = cell(&mut first, 0);
        let theirs = cell(&mut second, 5);

        let taken = Barrier::new(2);
        let gate = &taken;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut mem = second;
                gate.wait();
                lock(&mem, it, &Held(vec![theirs, it])).unwrap();
                let place = value(&mem, it);
                mem.write(place, 1);
                unlock(&mem, it);
            });

            lock(&first, it, &NoRoots).unwrap();
            taken.wait();
            // The other task is on its way into a wait it cannot leave until
            // this one gives the cell back. A collection here has to finish
            // without it running an instruction.
            std::thread::yield_now();
            let lost = cell(&mut first, 0);
            first.collect(&layouts, &Held(vec![it]));
            assert_eq!(first.object_layout(lost), FREE);
            unlock(&first, it);
        });

        // The waiter's own cell was published as a root for the whole wait and
        // is still there, and its write landed.
        assert_eq!(first.object_layout(theirs), CELL);
        assert_eq!(first.payload(theirs, VALUE), 5);
        assert_eq!(first.payload(it, VALUE), 1);
    }

    /// A cell that holds a handle to itself is collected once nothing else
    /// names it.
    ///
    /// This is the case
    /// [ADR 0011](../../../../docs/adr/0011-garbage-collection.md)'s amendment
    /// refused and
    /// [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
    /// made ordinary, and the test does not depend on when a collection runs
    /// because it runs one: what is asserted is that the cycle is *reclaimable*,
    /// which is exactly what the amendment said no collector could answer.
    #[test]
    fn a_cell_holding_itself_is_collected() {
        let layouts = table();
        let mut mem = Memory::new(1 << 12);
        let it = mem.alloc(RING, 0, 2).expect("the fixture has room");
        // What a `lock` whose closure stored the cell back into itself leaves:
        // the value word naming the cell it is a word of.
        mem.set_payload(it, VALUE, it);

        // Still named, so still there — and the collector followed the cycle
        // without being asked to stop, which is the other half of the claim.
        mem.collect(&layouts, &Held(vec![it]));
        assert_eq!(mem.object_layout(it), RING);
        assert_eq!(mem.payload(it, VALUE), it);

        mem.collect(&layouts, &NoRoots);
        assert_eq!(mem.object_layout(it), FREE);
    }

    /// So is a cycle through two of them, which the amendment left as an
    /// accepted, documented leak.
    ///
    /// Nothing about the collector knows there are cells in it. That is the
    /// substance of ADR 0037: the leak is gone rather than merely undetected,
    /// because the question was never a `Shared` question.
    #[test]
    fn a_cycle_through_two_cells_is_collected() {
        let layouts = table();
        let mut mem = Memory::new(1 << 12);
        let one = mem.alloc(RING, 0, 2).expect("the fixture has room");
        let other = mem.alloc(RING, 0, 2).expect("the fixture has room");
        mem.set_payload(one, VALUE, other);
        mem.set_payload(other, VALUE, one);
        let words = 2 * (1 + 2);

        // What the sweep reports rather than what each header says, because
        // adjacent free objects are written back as *one* free block: only
        // the first of a run keeps a header a reader can ask, and the run is
        // the whole heap here.
        let done = mem.collect(&layouts, &NoRoots);
        assert_eq!(done.freed_words, words);
        assert_eq!(done.live_words, 0);
        assert_eq!(mem.object_layout(one), FREE);
    }

    /// What a cell publishes is not only its own words.
    ///
    /// The holder allocates an object, writes into it, and stores its address
    /// in the cell. Every one of those writes is `Relaxed`; what makes them
    /// visible to the next holder is the release/acquire pair on the state
    /// word alone.
    #[test]
    fn a_cell_publishes_what_the_holder_wrote_through_it() {
        const TURNS: u64 = 5_000;
        let mut first = Memory::new(1 << 16);
        let second = first.for_task().unwrap();
        let it = cell(&mut first, 0);

        let start = Barrier::new(2);
        let gate = &start;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut mem = second;
                gate.wait();
                for turn in 1..=TURNS {
                    lock(&mem, it, &NoRoots).unwrap();
                    // A fresh object whose words say which turn wrote them,
                    // then its address into the cell.
                    let object = mem.alloc(INT, 0, 4).expect("the fixture has room");
                    for at in 0..4 {
                        mem.set_payload(object, at, turn);
                    }
                    let place = value(&mem, it);
                    mem.write(place, object);
                    unlock(&mem, it);
                }
            });
            start.wait();
            let mut seen = 0;
            while seen < TURNS {
                lock(&first, it, &NoRoots).unwrap();
                let object = first.read(value(&first, it));
                if object != 0 {
                    let turn = first.payload(object, 0);
                    seen = seen.max(turn);
                    for at in 0..4 {
                        assert_eq!(
                            first.payload(object, at),
                            turn,
                            "every word the holder wrote before it released is visible"
                        );
                    }
                }
                unlock(&first, it);
            }
        });
    }
}
