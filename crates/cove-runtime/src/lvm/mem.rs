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
    /// A copy within one region is a `memmove`, so a run may overlap itself —
    /// which a copy between two slots of one frame can, and which a lowering
    /// is free to emit rather than having to prove it does not.
    pub(crate) fn copy_words(&mut self, dst: u64, src: u64, words: u32) {
        if words == 0 || dst == src {
            return;
        }
        debug_assert!(
            self.holds(dst, words) && self.holds(src, words),
            "a {words}-word copy between {src} and {dst} leaves the words that exist"
        );
        let n = words as usize;
        match (is_stack(dst), is_stack(src)) {
            (true, true) => {
                let (d, s) = (dst as usize, src as usize);
                self.stack.copy_within(s..s + n, d);
            }
            (false, false) => {
                let (d, s) = ((dst - STACK_WORDS) as usize, (src - STACK_WORDS) as usize);
                self.heap.copy_within(s..s + n, d);
            }
            (true, false) => {
                let (d, s) = (dst as usize, (src - STACK_WORDS) as usize);
                self.stack[d..d + n].copy_from_slice(&self.heap[s..s + n]);
            }
            (false, true) => {
                let (d, s) = ((dst - STACK_WORDS) as usize, src as usize);
                self.heap[d..d + n].copy_from_slice(&self.stack[s..s + n]);
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
        let n = words as usize;
        if is_stack(addr) {
            let at = addr as usize;
            self.stack[at..at + n].fill(0);
        } else {
            let at = (addr - STACK_WORDS) as usize;
            self.heap[at..at + n].fill(0);
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
            let at = addr as usize;
            self.stack[at..at + n].to_vec()
        } else {
            let at = (addr - STACK_WORDS) as usize;
            self.heap[at..at + n].to_vec()
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
    fn holds(&self, addr: u64, words: u32) -> bool {
        let end = addr + words as u64;
        if is_stack(addr) {
            end <= self.stack.len() as u64
        } else {
            end <= STACK_WORDS + self.heap.len() as u64
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
    /// The released block does not join [`Memory::free`]: the next sweep
    /// walks the heap and rebuilds that list, and until then the words are
    /// neither reachable nor handed out.
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
        1 + layouts[layout.index()].payload_words(len, layouts) as u64
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
    /// What an object refers to is a question its *layout* answers, and — for
    /// one shape — one word of the object itself. That is new, and it is the
    /// simplification the run-of-words model bought: an enum's payload region
    /// has a static per-word reference map, because the lowering assigns case
    /// offsets so that every case using a word agrees on its `Repr`. Nothing
    /// here reads a discriminant to decide what to trace, and a payload-less
    /// case cannot retain what another case left in a word it does not use,
    /// because construction zeroes the region it does not fill.
    ///
    /// So a payload is walked by flattened per-word `Repr`s, and every shape
    /// says where its runs of them are. The one object that still has to be
    /// read is a box: erasure is where a value stops having a static width,
    /// so the box carries the layout of what it holds in its first payload
    /// word, and that is the word this asks for.
    fn trace(&mut self, layouts: &[Layout], addr: u64, work: &mut Vec<u64>) {
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
                self.trace_run(addr, 0, &layout.words, work)
            }
            // The stride is the element's width, so an `Array<Point>` is a
            // run of two-word elements rather than a run of addresses, and a
            // `Set` of them is the same run kept sorted.
            Shape::Elements { elem, .. } | Shape::Members { elem } => {
                let elem = &layouts[elem.index()];
                let stride = elem.width();
                for at in 0..len {
                    self.trace_run(addr, at * stride, &elem.words, work);
                }
            }
            // Word 0 is the length and word 1 is the store, whose own layout
            // says what its elements are. A vector's header is a leaf apart
            // from the one reference that makes it growable.
            Shape::Vector { .. } => self.enqueue(self.payload(addr, 1), work),
            // Key then value, each at its own width: a `Map<String, Point>`
            // is a run of three-word entries and only the first is traced.
            Shape::Entries { key, value } => {
                let (key, value) = (&layouts[key.index()], &layouts[value.index()]);
                let stride = key.width() + value.width();
                for at in 0..len {
                    self.trace_run(addr, at * stride, &key.words, work);
                    self.trace_run(addr, at * stride + key.width(), &value.words, work);
                }
            }
            // Word 0 is the callee. The captures follow it, each inline under
            // its own layout, which is how a captured struct is stored where
            // the capture is rather than behind another address.
            Shape::Closure { captures, .. } => {
                let mut at = 1;
                for id in captures {
                    let capture = &layouts[id.index()];
                    self.trace_run(addr, at, &capture.words, work);
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
                    self.trace_run(addr, 1, &held.words, work);
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
    fn trace_run(&mut self, addr: u64, at: u32, words: &[Repr], work: &mut Vec<u64>) {
        for (offset, repr) in words.iter().enumerate() {
            if repr.is_ref() {
                self.enqueue(self.payload(addr, at + offset as u32), work);
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

    /// A copy within one region is a `memmove`, so a lowering may emit one
    /// whose source and destination overlap rather than having to prove they
    /// do not.
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
        assert_eq!(mem.free, vec![lost]);
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
        assert!(mem.free.is_empty());
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
        assert_eq!(mem.free, vec![big + 2]);
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
        assert_eq!(mem.free, vec![a]);
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
}
