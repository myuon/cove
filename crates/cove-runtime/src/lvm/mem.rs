//! One run's linear memory: a reserved stack region, a heap region above it,
//! and one kind of address that names a word in either.
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
//! # Two allocations, one address space
//!
//! A linear address is a word index. `[0, STACK_WORDS)` is the stack region;
//! everything at or above [`STACK_WORDS`] is the heap region. [`is_stack`] is
//! the whole of the decoder, and it is the only thing anywhere that knows the
//! two regions are currently two separate `Vec<u64>`s.
//!
//! ADR 0034 permits that split as a temporary implementation state on one
//! condition: no address encoding, lowered layout, GC map or public API may
//! expose it. Nothing does, because **no address changes value when the two
//! are later placed in one block** — a heap object is at `STACK_WORDS + its
//! offset within the heap` under either arrangement, and a stack word is at
//! its own index under both. Moving to one block is then a change to two
//! indexing expressions, not a representation migration.
//!
//! Addresses are indices rather than pointers, which is what lets a region
//! reallocate as it grows while every live address stays correct. A growable
//! stack and a non-moving heap coexist with no fixup pass over anything.
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

use cove_lir::{Layout, LayoutId, Repr, Shape};

/// The words reserved for the stack region, `[0, STACK_WORDS)`.
///
/// One mebiword, eight mebibytes. The number is an implementation choice and
/// deliberately not a language fact: the tree-walking oracle and this machine
/// represent a frame differently and will run out at different depths, so
/// requiring them to agree on a depth would be requiring one of them to
/// represent a frame the other's way. What they must agree on is the *way*
/// they fail — a stack-overflow runtime error, with a span, deterministically,
/// inside the run's memory budget.
///
/// Reserved is not committed. The backing store grows on demand and a program
/// that never nests deeply never pays for the region; what the constant buys
/// is that no heap object is ever placed below it, so address `0` is a stack
/// word, can never name an object, and is free to mean null for `Repr::Ref`.
pub(crate) const STACK_WORDS: u64 = 1 << 20;

/// The most words the heap region may hold.
///
/// A reclaimed run of words describes its own length in its header's 32-bit
/// `len` field, so a heap the sweeper can walk is one whose largest possible
/// free run fits there. Thirty-two gibibytes of Cove objects is far past the
/// point where a bump allocator and a stop-the-world mark and sweep were the
/// right answer, so the cap costs nothing that this allocator could have
/// delivered anyway.
const MAX_HEAP_WORDS: u64 = u32::MAX as u64;

/// Whether a linear address names a word of the stack region.
///
/// This one comparison is the entire region decoder, and the entire knowledge
/// that the regions live in two allocations. See the module docs.
#[inline]
pub(crate) fn is_stack(addr: u64) -> bool {
    addr < STACK_WORDS
}

/// The header word of an object of `layout` whose length field is `len`.
///
/// `len` means whatever the layout says it means — a byte count for a string,
/// an element count for an array, nothing at all for a struct. The header
/// carries it rather than the payload word count because
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

/// One run's linear memory.
pub(crate) struct Memory {
    /// The stack region, `[0, stack.len())`. Grown on demand and never past
    /// [`STACK_WORDS`].
    stack: Vec<u64>,
    /// The heap region, addressed from [`STACK_WORDS`]. Its length is the bump
    /// pointer.
    heap: Vec<u64>,
    /// The first address the heap may not reach.
    limit: u64,
    /// Free blocks, by address. Rebuilt by every sweep, consumed by [`Memory::alloc`].
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

impl Memory {
    /// An empty memory whose heap may grow to `heap_words_budget` words.
    ///
    /// The budget is a count of words rather than of objects: what exhausts a
    /// heap is the space its objects take, and a `Vector` of a million
    /// elements is one object.
    pub(crate) fn new(heap_words_budget: usize) -> Memory {
        let budget = (heap_words_budget as u64).min(MAX_HEAP_WORDS);
        Memory {
            stack: Vec::new(),
            heap: Vec::new(),
            limit: STACK_WORDS + budget,
            free: Vec::new(),
            marks: Vec::new(),
            allocated_words: 0,
            collections: 0,
        }
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
            self.stack[addr as usize]
        } else {
            self.heap[(addr - STACK_WORDS) as usize]
        }
    }

    /// Writes `word` at `addr`, in whichever region it names.
    #[inline]
    pub(crate) fn write(&mut self, addr: u64, word: u64) {
        if is_stack(addr) {
            self.stack[addr as usize] = word;
        } else {
            self.heap[(addr - STACK_WORDS) as usize] = word;
        }
    }

    // --- the stack region ---------------------------------------------------

    /// Reserves `size` zeroed words on top of the stack and answers their base.
    ///
    /// Zeroed, because a `Repr::Ref` slot that has not been written yet must
    /// read as null rather than as whatever the returned frame left in that
    /// word. The collector reads a frame's reference slots by a static map, so
    /// a slot the program has not reached yet is still walked, and a stale
    /// address there would retain an object — or, worse, name a word that is no
    /// longer an object header.
    pub(crate) fn push_frame(&mut self, size: u32) -> Result<u64, Overflow> {
        let base = self.stack.len() as u64;
        if base + size as u64 >= STACK_WORDS {
            return Err(Overflow);
        }
        self.stack.resize(self.stack.len() + size as usize, 0);
        Ok(base)
    }

    /// Drops every frame at or above `base`.
    ///
    /// Truncation does not clear the words it releases; [`Memory::push_frame`]
    /// zeroes them on the way back up. Doing it once, on the path that is about
    /// to write them anyway, is one pass rather than two.
    pub(crate) fn pop_frame(&mut self, base: u64) {
        self.stack.truncate(base as usize);
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

    /// How many words of the stack region are committed.
    pub(crate) fn stack_words(&self) -> u64 {
        self.stack.len() as u64
    }

    // --- the heap region ----------------------------------------------------

    /// The first heap address no object occupies.
    #[inline]
    fn bump(&self) -> u64 {
        STACK_WORDS + self.heap.len() as u64
    }

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
    pub(crate) fn alloc(&mut self, layout: LayoutId, len: u32, payload_words: u32) -> Option<u64> {
        let words = 1 + payload_words as u64;
        let addr = match self.take_free(words) {
            Some(addr) => {
                // A reclaimed block still holds the dead object's words, and a
                // `Ref` field of the new object must read as null until it is
                // written. The bump path below needs no such pass: its words
                // have never been used.
                let at = (addr - STACK_WORDS) as usize;
                self.heap[at..at + words as usize].fill(0);
                addr
            }
            None => {
                let addr = self.bump();
                if addr + words > self.limit {
                    return None;
                }
                self.heap.resize(self.heap.len() + words as usize, 0);
                addr
            }
        };
        self.write(addr, header(layout, len));
        self.allocated_words += words;
        Some(addr)
    }

    /// The first free block of at least `words` words, split to size.
    ///
    /// First fit over a list the sweeper leaves in address order. It is the
    /// simplest thing that makes "collect and retry" mean something, and ADR
    /// 0034 leaves the final allocator undecided, so nothing is committed by
    /// choosing it. A remainder always becomes a free block of its own, however
    /// small: the smallest one is a header and no payload, which is one word.
    fn take_free(&mut self, words: u64) -> Option<u64> {
        let mut at = 0;
        while at < self.free.len() {
            let addr = self.free[at];
            let have = self.block_words(addr);
            if have >= words {
                if have == words {
                    // `remove` rather than `swap_remove`: the list is in
                    // address order and first fit over an address-ordered
                    // list is what keeps small survivors from stranding the
                    // low end of the heap. A swap would trade that for a
                    // shift over a list the next sweep rebuilds anyway.
                    self.free.remove(at);
                } else {
                    let rest = addr + words;
                    self.write(rest, header(LayoutId::FREE, (have - words - 1) as u32));
                    self.free[at] = rest;
                }
                return Some(addr);
            }
            at += 1;
        }
        None
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

    /// How many words the free block at `addr` occupies, header included.
    fn block_words(&self, addr: u64) -> u64 {
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
        1 + layouts[layout.index()].payload_words(len) as u64
    }

    /// Words the heap region currently occupies, free blocks included.
    pub(crate) fn heap_words(&self) -> u64 {
        self.heap.len() as u64
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub(crate) fn allocated_words(&self) -> u64 {
        self.allocated_words
    }

    /// How many collections have run.
    pub(crate) fn collections(&self) -> u64 {
        self.collections
    }

    // --- mark and sweep -----------------------------------------------------

    /// Marks everything `roots` reaches and reclaims the rest.
    ///
    /// Non-moving, so every address a program holds — including one that points
    /// into an object — is still correct when this returns.
    pub(crate) fn collect(&mut self, layouts: &[Layout], roots: &dyn Roots) -> Collected {
        self.marks.clear();
        self.marks.resize(self.heap.len().div_ceil(64), 0);

        // An explicit worklist rather than recursion. A linked list a million
        // long is an ordinary Cove value and a legal object graph; a collector
        // that recursed over it would overflow the Rust stack, and the one
        // moment a runtime cannot afford to abort is the one where it is
        // reclaiming memory because there is none left.
        let mut work: Vec<u64> = Vec::new();
        let marks = &mut self.marks;
        let bump = STACK_WORDS + self.heap.len() as u64;
        roots.each_root(&mut |addr| {
            if reachable(addr, bump) && set_mark(marks, addr) {
                work.push(addr);
            }
        });

        while let Some(addr) = work.pop() {
            self.trace(layouts, addr, &mut work);
        }

        let (freed_words, live_words) = self.sweep(layouts);
        self.collections += 1;
        Collected {
            freed_words,
            live_words,
            collections: self.collections,
        }
    }

    /// Marks and enqueues every object the object at `addr` refers to.
    ///
    /// What an object refers to is answered by its layout *and* by its own
    /// words, and both are needed. An enum is sized for its widest case, so
    /// which of its payload words are references depends on the case it is in —
    /// a fact about this object, at this moment, which only the object can
    /// answer. A boxed value is the same question one level down: the tag it
    /// carries is what says whether the word beside it is a reference. Tracing
    /// by layout alone would retain whatever a payload-less case happened to
    /// leave behind, and treating a boxed `Int` as an address would be worse
    /// than a leak.
    fn trace(&mut self, layouts: &[Layout], addr: u64, work: &mut Vec<u64>) {
        let layout = &layouts[self.object_layout(addr).index()];
        // The one question that can be answered without reading the object at
        // all. A string, an `Array<Int>` and a scalar struct leave here having
        // cost a table lookup.
        if !layout.may_hold_refs() {
            return;
        }
        match &layout.shape {
            Shape::Free | Shape::Str => {}
            Shape::Struct { fields, .. } => {
                for (at, field) in fields.iter().enumerate() {
                    if field.repr.is_ref() {
                        self.enqueue(self.payload(addr, at as u32), work);
                    }
                }
            }
            Shape::Enum { cases } => {
                let case = self.payload(addr, 0);
                // A case index the table does not have is a lowering bug, and a
                // collection is the worst place to discover one by unwinding.
                // Tracing nothing is the safe reading: it can only fail by
                // freeing something, which the differential corpus catches, and
                // it cannot corrupt the heap the way marking an arbitrary word
                // would.
                if let Some(case) = cases.get(case as usize) {
                    for (at, repr) in case.payload.iter().enumerate() {
                        if repr.is_ref() {
                            self.enqueue(self.payload(addr, 1 + at as u32), work);
                        }
                    }
                }
            }
            Shape::Elements { elem, .. } => {
                if elem.is_ref() {
                    for at in 0..self.object_len(addr) {
                        self.enqueue(self.payload(addr, at), work);
                    }
                }
            }
            // Word 0 is the length and word 1 is the store, whose own layout
            // says what its elements are. A vector's header is a leaf apart
            // from the one reference that makes it growable.
            Shape::Vector { .. } => self.enqueue(self.payload(addr, 1), work),
            Shape::Closure { captures, .. } => {
                for (at, repr) in captures.iter().enumerate() {
                    if repr.is_ref() {
                        self.enqueue(self.payload(addr, 1 + at as u32), work);
                    }
                }
            }
            Shape::Boxed => {
                if Repr::from_tag(self.payload(addr, 0)) == Some(Repr::Ref) {
                    self.enqueue(self.payload(addr, 1), work);
                }
            }
        }
    }

    /// Marks `addr` and enqueues it if this is the first time it was seen.
    ///
    /// Null is the ordinary case, not an error: a frame is zeroed on entry, a
    /// slot is cleared at its last use, and a `Ref` field of a half-built object
    /// has not been written yet.
    #[inline]
    fn enqueue(&mut self, addr: u64, work: &mut Vec<u64>) {
        let bump = self.bump();
        debug_assert!(
            addr == 0 || reachable(addr, bump),
            "a Ref word named {addr}, which is not a heap object"
        );
        if reachable(addr, bump) && set_mark(&mut self.marks, addr) {
            work.push(addr);
        }
    }

    /// Whether the object at `addr` was marked by this collection.
    fn is_marked(&self, addr: u64) -> bool {
        let bit = (addr - STACK_WORDS) as usize;
        self.marks[bit / 64] & (1 << (bit % 64)) != 0
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
    fn sweep(&mut self, layouts: &[Layout]) -> (u64, u64) {
        self.free.clear();
        let mut freed = 0;
        let mut live = 0;
        let mut run: Option<u64> = None;
        let mut addr = STACK_WORDS;
        let end = self.bump();
        while addr < end {
            let words = self.object_words(layouts, addr);
            if self.is_marked(addr) {
                live += words;
                if let Some(start) = run.take() {
                    self.close_free_run(start, addr);
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
            self.close_free_run(start, end);
        }
        (freed, live)
    }

    /// Writes `[start, end)` as one free block and records it.
    fn close_free_run(&mut self, start: u64, end: u64) {
        self.write(start, header(LayoutId::FREE, (end - start - 1) as u32));
        self.free.push(start);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The roots of a test: a list, walked in order.
    struct Held(Vec<u64>);

    impl Roots for Held {
        fn each_root(&self, f: &mut dyn FnMut(u64)) {
            for &addr in &self.0 {
                f(addr);
            }
        }
    }

    fn layout(name: &str, shape: Shape) -> Layout {
        Layout {
            name: Arc::from(name),
            shape,
        }
    }

    fn field(name: &str, repr: Repr) -> cove_lir::Field {
        cove_lir::Field {
            name: Arc::from(name),
            repr,
        }
    }

    /// A table whose index 0 is the reserved free layout, as a program's is.
    fn table(shapes: Vec<Shape>) -> Vec<Layout> {
        let mut layouts = vec![Layout::free()];
        layouts.extend(shapes.into_iter().map(|shape| layout("test", shape)));
        layouts
    }

    /// `LayoutId` of the `n`th shape passed to [`table`].
    fn id(n: u32) -> LayoutId {
        LayoutId(n + 1)
    }

    /// A leaf: two words of nothing anybody points at.
    fn leaf() -> Shape {
        Shape::Elements {
            elem: Repr::Int,
            growable: false,
        }
    }

    /// A one-word box holding one reference.
    fn holder() -> Shape {
        Shape::Struct {
            fields: vec![field("it", Repr::Ref)],
            opaque: false,
        }
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
    fn the_stack_region_is_the_limit() {
        let mut mem = Memory::new(16);
        // One frame just short of the region, then one word too many.
        let base = mem.push_frame(STACK_WORDS as u32 - 2).unwrap();
        assert_eq!(base, 0);
        assert_eq!(mem.push_frame(2), Err(Overflow));
        assert_eq!(mem.push_frame(1).unwrap(), STACK_WORDS - 2);
    }

    #[test]
    fn an_address_reads_the_same_way_in_either_region() {
        let mut mem = Memory::new(16);
        let base = mem.push_frame(2).unwrap();
        let object = mem.alloc(id(0), 2, 2).unwrap();

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

    #[test]
    fn an_object_round_trips_its_header_and_payload() {
        let mut mem = Memory::new(16);
        let object = mem.alloc(id(2), 13, 3).unwrap();
        assert_eq!(object, STACK_WORDS);
        assert_eq!(mem.object_layout(object), id(2));
        assert_eq!(mem.object_len(object), 13);
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
        assert!(mem.alloc(id(0), 2, 2).is_some());
        // One word left, and the smallest object is two.
        assert_eq!(mem.alloc(id(0), 1, 1), None);
        assert_eq!(mem.alloc(id(0), 0, 0), Some(STACK_WORDS + 3));
        assert_eq!(mem.alloc(id(0), 0, 0), None);
        assert_eq!(mem.heap_words(), 4);
    }

    #[test]
    fn a_collection_frees_what_no_root_reaches() {
        let layouts = table(vec![leaf()]);
        let mut mem = Memory::new(64);
        let kept = mem.alloc(id(0), 2, 2).unwrap();
        let lost = mem.alloc(id(0), 2, 2).unwrap();
        mem.set_payload(kept, 0, 41);
        mem.set_payload(lost, 0, 42);

        let done = mem.collect(&layouts, &Held(vec![kept]));
        assert_eq!(done.live_words, 3);
        assert_eq!(done.freed_words, 3);
        assert_eq!(done.collections, 1);
        // Non-moving: the survivor is where it was, with what it held.
        assert_eq!(mem.object_layout(kept), id(0));
        assert_eq!(mem.payload(kept, 0), 41);
        // The reclaimed run is a walkable free block.
        assert_eq!(mem.free, vec![lost]);
        assert_eq!(mem.object_layout(lost), LayoutId::FREE);
        assert_eq!(mem.object_len(lost), 2);
    }

    #[test]
    fn a_freed_block_is_handed_out_again() {
        let layouts = table(vec![leaf()]);
        let mut mem = Memory::new(3);
        let first = mem.alloc(id(0), 2, 2).unwrap();
        assert_eq!(mem.alloc(id(0), 2, 2), None);

        mem.set_payload(first, 1, 0xfeed);
        mem.collect(&layouts, &Held(vec![]));

        // The retry the `None` above is an invitation to. The block comes back
        // zeroed, because a `Ref` field of the new object must read as null.
        let second = mem.alloc(id(0), 2, 2).unwrap();
        assert_eq!(second, first);
        assert_eq!(mem.payload(second, 1), 0);
        assert!(mem.free.is_empty());
    }

    #[test]
    fn a_split_leaves_a_free_block_behind() {
        let layouts = table(vec![leaf()]);
        let mut mem = Memory::new(8);
        let big = mem.alloc(id(0), 4, 4).unwrap();
        mem.collect(&layouts, &Held(vec![]));

        let small = mem.alloc(id(0), 1, 1).unwrap();
        assert_eq!(small, big);
        // Three words remain, described by a header of their own so the sweeper
        // can still walk over them.
        assert_eq!(mem.free, vec![big + 2]);
        assert_eq!(mem.object_layout(big + 2), LayoutId::FREE);
        assert_eq!(mem.object_len(big + 2), 2);
    }

    #[test]
    fn adjacent_free_blocks_coalesce() {
        let layouts = table(vec![leaf()]);
        let mut mem = Memory::new(64);
        let a = mem.alloc(id(0), 1, 1).unwrap();
        let _b = mem.alloc(id(0), 2, 2).unwrap();
        let _c = mem.alloc(id(0), 1, 1).unwrap();
        let kept = mem.alloc(id(0), 1, 1).unwrap();

        mem.collect(&layouts, &Held(vec![kept]));

        // Three dead objects, one block: 2 + 3 + 2 words, and a request no one
        // of them could have satisfied now fits.
        assert_eq!(mem.free, vec![a]);
        assert_eq!(mem.block_words(a), 7);
        assert_eq!(mem.alloc(id(0), 6, 6).unwrap(), a);
    }

    #[test]
    fn an_enum_is_traced_by_the_case_it_is_in() {
        // `Option<T>`: two payload words, one for the case index and one sized
        // for `Some`, which `None` leaves unused.
        let option = Shape::Enum {
            cases: vec![
                cove_lir::Case {
                    name: Arc::from("None"),
                    payload: vec![],
                },
                cove_lir::Case {
                    name: Arc::from("Some"),
                    payload: vec![Repr::Ref],
                },
            ],
        };
        let layouts = table(vec![option, leaf()]);

        for (case, live) in [(0, false), (1, true)] {
            let mut mem = Memory::new(64);
            let target = mem.alloc(id(1), 1, 1).unwrap();
            let opt = mem.alloc(id(0), 0, 2).unwrap();
            mem.set_payload(opt, 0, case);
            // The word `Some` would use, left behind in both runs. In `None` it
            // is not a reference — the case says so — and the object it names
            // must not be retained by it.
            mem.set_payload(opt, 1, target);

            let done = mem.collect(&layouts, &Held(vec![opt]));
            assert_eq!(mem.object_layout(target) != LayoutId::FREE, live);
            assert_eq!(done.freed_words, if live { 0 } else { 2 });
        }
    }

    #[test]
    fn a_boxed_word_is_a_reference_only_when_its_tag_says_so() {
        let layouts = table(vec![Shape::Boxed, leaf()]);

        for repr in [Repr::Int, Repr::Ref] {
            let mut mem = Memory::new(64);
            let target = mem.alloc(id(1), 1, 1).unwrap();
            let boxed = mem.alloc(id(0), 0, 2).unwrap();
            mem.set_payload(boxed, 0, repr.tag());
            mem.set_payload(boxed, 1, target);

            mem.collect(&layouts, &Held(vec![boxed]));
            // The same bits under two tags. An `Int` that happens to equal an
            // address is not one, and the box is the only thing that knows.
            assert_eq!(
                mem.object_layout(target) != LayoutId::FREE,
                repr == Repr::Ref
            );
        }
    }

    #[test]
    fn an_interior_address_survives_a_collection() {
        let layouts = table(vec![holder(), leaf()]);
        let mut mem = Memory::new(64);
        let target = mem.alloc(id(1), 3, 3).unwrap();
        let base = mem.alloc(id(0), 0, 1).unwrap();
        let _garbage = mem.alloc(id(1), 3, 3).unwrap();
        mem.set_payload(base, 0, target);

        // What a `var` argument naming an element carries: the address of one
        // word inside an object. It is not a root — the object is held by the
        // `Ref` field of `base` — and it is not rewritten, because nothing
        // moves.
        let element = mem.payload_addr(target, 1);
        mem.write(element, 0xc0ffee);

        let done = mem.collect(&layouts, &Held(vec![base]));
        assert_eq!(done.freed_words, 4);
        assert_eq!(mem.read(element), 0xc0ffee);
        assert_eq!(mem.payload(target, 1), 0xc0ffee);
        assert_eq!(mem.object_layout(target), id(1));
    }

    #[test]
    fn a_deep_graph_does_not_recurse() {
        // A linked list is an ordinary value. Marking it must cost heap, not
        // Rust stack.
        let layouts = table(vec![holder()]);
        let mut mem = Memory::new(1 << 20);
        let head = mem.alloc(id(0), 0, 1).unwrap();
        let mut tail = head;
        for _ in 0..200_000 {
            let next = mem.alloc(id(0), 0, 1).unwrap();
            mem.set_payload(tail, 0, next);
            tail = next;
        }

        let done = mem.collect(&layouts, &Held(vec![head]));
        assert_eq!(done.live_words, 200_001 * 2);
        assert_eq!(done.freed_words, 0);
    }

    #[test]
    fn a_cycle_is_marked_once() {
        let layouts = table(vec![holder()]);
        let mut mem = Memory::new(64);
        let a = mem.alloc(id(0), 0, 1).unwrap();
        let b = mem.alloc(id(0), 0, 1).unwrap();
        mem.set_payload(a, 0, b);
        mem.set_payload(b, 0, a);

        // Reachable, and the mark bit is what stops the walk. Then unreachable,
        // which is the whole reason a tracing collector is here rather than a
        // reference count.
        assert_eq!(mem.collect(&layouts, &Held(vec![a])).live_words, 4);
        assert_eq!(mem.collect(&layouts, &Held(vec![])).freed_words, 4);
    }

    #[test]
    fn a_null_root_is_ordinary() {
        let layouts = table(vec![leaf()]);
        let mut mem = Memory::new(64);
        let kept = mem.alloc(id(0), 1, 1).unwrap();
        // A slot that has not been written yet, and one that `Clear` emptied.
        let done = mem.collect(&layouts, &Held(vec![0, kept, 0]));
        assert_eq!(done.live_words, 2);
        assert_eq!(mem.object_layout(kept), id(0));
    }

    #[test]
    fn a_leaf_is_not_read_at_all() {
        // `may_hold_refs` is what a string costs a collection: a table lookup
        // and no word reads. The payload here is a plausible address, and it is
        // never followed.
        let layouts = table(vec![Shape::Str, leaf()]);
        let mut mem = Memory::new(64);
        let target = mem.alloc(id(1), 1, 1).unwrap();
        let text = mem.alloc(id(0), 8, 1).unwrap();
        mem.set_payload(text, 0, target);

        mem.collect(&layouts, &Held(vec![text]));
        assert_eq!(mem.object_layout(target), LayoutId::FREE);
    }
}
