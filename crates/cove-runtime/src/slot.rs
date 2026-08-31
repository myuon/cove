//! The vertical slice [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
//! decision 8 requires: a VM-owned heap object named by a one-word handle, and
//! the rooting mechanism that keeps such an object alive when the handle is
//! temporarily outside the VM's slots.
//!
//! Nothing in this module is wired into [`crate::vm::Vm`], and the reason is
//! not that the wiring was left for later. It is that the mechanism below
//! **cannot** be wired into the live collector until slots are eight bytes.
//! "Why this cannot be added to the collector as it stands", below, is the
//! argument, and it is the finding this slice exists to establish.
//!
//! # What the gate is
//!
//! [`crate::heap`] finds a value the backend is holding in a Rust local by
//! arithmetic rather than by scanning: it counts the references it can see
//! and compares them with `Rc::strong_count`, and a shortfall is a reference
//! held somewhere it cannot read. That rule is what makes "collect at any
//! safepoint" true on both backends today.
//!
//! ADR 0028 decision 8 says plainly that the rule is not assumed to survive a
//! VM-owned handle:
//!
//! > An index or offset copied into a Rust local does not change
//! > `Rc::strong_count`, so **the ADR does not claim that the current
//! > shortfall collector survives such a handle untouched**. The vertical
//! > slice must include a safepoint with a heap handle temporarily outside
//! > the VM stack and prove that the object remains live.
//!
//! [`Handle`] is `Copy` and eight bytes wide and owns nothing. Copying one
//! into a Rust local changes no count anywhere, so an accounting rule that
//! reads counts is blind to it. The object it names is owned by
//! [`HandleHeap`] and by nothing else, so if the collector does not see the
//! handle the object is swept and the local names a free slot. That is a
//! use-after-free, and `a_bare_handle_in_a_rust_local_is_not_a_root` in this
//! module's tests is that use-after-free, committed on purpose.
//!
//! # The mechanism, and the three that were not chosen
//!
//! Decision 8 lists four coherent mechanisms and says the prototype must
//! choose and test one. This slice takes the second: **every Rust-local
//! handle that can survive to a safepoint is registered in an explicit
//! temporary-root stack.** [`TempRoots`] is that stack and
//! [`Machine::with_root`] is how a dispatch loop uses it.
//!
//! - *Handles participate in reference counting.* A one-word handle can be an
//!   `Rc` pointer rather than an index, and then the shortfall rule survives
//!   untouched. It was refused because the count has to be adjusted every
//!   time a slot is stored, overwritten or dropped, and a frame's teardown
//!   becomes a walk of its reference map running destructors. That is exactly
//!   the cost ADR 0027 created `Vm::scalars` to avoid — "eight bytes each,
//!   with no tag and **no destructor**" — and decision 1 generalises the
//!   arrangement to every slot. It also takes the heap's ownership away from
//!   the VM and gives it back to `Rc`, which contradicts decision 2's
//!   VM-owned layout, size, reference map and movement guarantee: an object
//!   whose lifetime `Rc` decides has no header the VM is free to define. And
//!   it does not actually remove the discipline — a bitwise `Copy` of an
//!   `Rc`-backed handle into a local still changes no count, so *taking a
//!   count* on the way into a local is the same act of remembering as
//!   *pushing a shadow root*, at strictly greater cost.
//! - *The dispatch discipline already guarantees it.* It does not, and this
//!   was checked rather than assumed. See "What the dispatch discipline
//!   actually holds" below.
//! - *Another mechanism with the same invariant.* A stack map over the Rust
//!   frames is the usual fourth answer and ADR 0011 rules it out in advance:
//!   "the roots are the interpreter's own structures rather than a machine
//!   stack, so there are no stack maps here." Nothing in ADR 0028 reopens
//!   that.
//!
//! # What the dispatch discipline actually holds
//!
//! `Vm::collect_if_due` carries an enumeration of every point at which this
//! backend may collect, and issue #209 re-confirmed it exhaustive. That list
//! is exhaustive about **where** a collection can happen. It is not a claim
//! that no value is in a Rust local when one does, and its own text says so
//! at four of its entries:
//!
//! - `Inst::Try` — "Here the failure *is* in a local: it was popped, opened,
//!   and found to be an `Err` before the safepoint."
//! - `Inst::CallHost` and `Inst::CallResource` — `Vm::take` and
//!   `Vm::borrow_args` drain the arguments off the operand stack into a
//!   `Vec<Value>` before the call is charged, and a host that re-enters runs
//!   Cove code, and therefore reaches safepoints, while they are out there.
//! - `Inst::Lock` — "the closure value itself is a local".
//! - `Inst::LeaveScope` and `Inst::CancelScope` — "The scope is popped out of
//!   `Vm::scopes` before its children are waited for, so during that wait it
//!   is a local rather than a walked root".
//!
//! Every one of those is followed by "rooted by its own reference", which is
//! the shortfall rule and nothing else. `Vm::arg_vectors` says the same in
//! its own words, and `Vm::retire_heap` adds a fifth case that is outside the
//! dispatch loop entirely: a finished task's answer is a Rust local of the
//! caller.
//!
//! So the discipline mechanism is not nearly true; it is false at five named
//! places, and every one of them is currently *load-bearing* on the rule the
//! ADR says may not survive. Making it true would mean reordering `Try` so
//! the failure stays an operand, keeping host-call arguments on the operand
//! stack across an arbitrarily deep re-entrant run, keeping a closing scope
//! in `Vm::scopes` while its children are waited for, and giving a returning
//! task somewhere on a stack to leave its answer. Each is possible; together
//! they are a global invariant over the whole dispatch loop, re-proved by
//! reading at every future edit, with no mechanical check and no local
//! failure. A shadow root is the opposite of that: it is local, it is
//! visible at the site that needs it, and forgetting it fails at that site.
//!
//! # Why this cannot be added to the collector as it stands
//!
//! A shadow-root stack over `Value` would be unsound today, and that is worth
//! stating because it is the tempting half-step. [`crate::heap`]'s soundness
//! rests on each reference being yielded **once**: a reference counted twice
//! makes a live allocation's count add up when it does not, and the sweep
//! then empties something a temporary can still reach. A `Value` in a Rust
//! local is already accounted for — by its own `Rc` count — so registering it
//! in a second root list would yield it twice and conceal exactly the
//! shortfall that roots it. That is the failure PR #192 kept
//! `Vm::arg_vectors` out of the root set for, and the one ADR 0027 kept
//! `Vm::places` out for.
//!
//! A **handle** is not a counted reference, which is why the same stack is
//! sound over handles: [`HandleHeap`] traces rather than counts, so a handle
//! reached from two root locations is marked once and there is no arithmetic
//! to spoil. The two heaps are therefore kept disjoint. Nothing in this
//! module mentions [`crate::value::Value`], and no object here can hold one:
//! an object's payload is [`Slot`] words, and a word is either scalar bits or
//! a [`Handle`]. The shadow-root stack cannot become a second path to
//! anything `crate::heap` already yields, because it cannot name anything
//! `crate::heap` manages.
//!
//! # The three multiplicities
//!
//! Decision 8 distinguishes three and says they must not be conflated. Here
//! is what each means once the collector traces handles:
//!
//! 1. **Root storage locations are yielded once.** [`Safepoint::walk`] yields
//!    each mapped handle slot of the frame once and each entry of the
//!    temporary-root stack once. It does not attempt to de-duplicate the
//!    *handles*: a handle standing in a slot and also registered as a
//!    temporary root is two storage locations and both are yielded.
//!    [`HandleCollection::roots_yielded`] is that count, and
//!    `a_handle_in_a_slot_and_in_the_shadow_stack_is_two_locations_and_one_object`
//!    pins it.
//! 2. **Real graph edges are counted once each.** This requirement exists for
//!    the comparison against `Rc::strong_count` and there is no such
//!    comparison here, so it does not arise — which is the whole reason a
//!    shadow root is safe over a handle and not over a `Value`. What replaces
//!    it is the disjointness above: the handle heap must never become a
//!    second path into the counted heap's accounting.
//! 3. **Objects are expanded once during marking.**
//!    [`HandleCollection::expansions`] counts how many times the mark phase
//!    read an object's reference map, and it equals the number of live
//!    objects whatever the shape of the graph.
//!    `a_shared_object_reached_by_many_edges_is_expanded_once` and
//!    `a_cycle_of_handles_is_expanded_once_and_reclaimed` are the two shapes
//!    that would break it.
//!
//! # What this is not
//!
//! Not a migration. `Value` is unchanged, the public API is unchanged, and
//! `Vm::stack`, `Vm::scalars` and `Vm::places` are unchanged. [`Frame`] here
//! is a stand-in for decision 1's single logical frame — eight-byte untagged
//! slots plus the reference map that says which of them hold handles — sized
//! to what a rooting proof needs and nothing more. There is no instruction
//! set, no layout for an enum, no `Dynamic`, and no measurement: ADR 0028
//! makes no performance claim it has not measured and neither does this.
//!
//! # What the migration still owes
//!
//! What this slice settles is the rooting invariant and nothing else, which
//! is what decision 8 says has to be settled before "the collector migration
//! can be called specified". Named so that the next reader does not have to
//! infer them from what is absent:
//!
//! - **The stack map is not derived.** [`Frame::refs`] is a bit per slot,
//!   maintained by whoever pushes. A real frame's map comes from
//!   `cove_ir::Function`'s per-slot layout, which is lowering work, and
//!   decision 1's "every physical offset derives from the one frame layout"
//!   is the invariant that arrangement owes.
//! - **A handle slot is never reused.** The sweep sets an object's slot to
//!   `None` and leaves it, so a stale handle names a dead object rather than
//!   a live one. A heap that runs for longer than a test needs a free list
//!   and a generation counter, and a handle then stops being a bare index.
//! - **No layout is chosen for an enum, and there is no `Dynamic`.**
//!   Decision 2 leaves enum layout to be selected per lowered type and
//!   requires only that the layout completely determine how to find every
//!   reference; decision 3's two-slot `Dynamic` needs a witness the reference
//!   map can read. Neither is here, and both change what a reference map has
//!   to say.
//! - **Nothing materialises.** Decision 5's boundary is where a handle
//!   becomes the `Value` a host is handed, and whatever does that holds a
//!   handle across a call that can collect — which is the first real caller
//!   of [`Machine::with_root`] and the first place the discipline will be
//!   load-bearing rather than demonstrated.
//! - **Nothing is measured.** #197's prototype phase ends at a measurement
//!   gate, and this slice deliberately does not approach it.

// Nothing outside this module names anything in it, and that is the point:
// the slice is the prototype #197's measurement gate stands in front of, and
// wiring it into the live VM before the migration would mean paying for two
// heaps to run one. The `allow` is scoped to this file for that reason, and
// every item in this file is prototype code, so it hides nothing else.
#![allow(dead_code)]

use std::collections::HashSet;

/// One eight-byte untagged VM slot.
///
/// ADR 0028 decision 1: the bits are not self-describing and never become so.
/// A slot holds a full `Int`, a full IEEE-754 `Float` bit pattern, a canonical
/// `Bool`, or a [`Handle`], and which of those it is comes from the frame's
/// layout — here [`Frame::refs`] — rather than from the bits.
pub(crate) type Slot = u64;

/// A one-word VM-owned name for a heap object.
///
/// ADR 0028 decision 2: a slot referring to heap-backed data holds one word,
/// and what it names carries its layout, size and reference map in VM-owned
/// metadata. This is that word. It is an index into [`HandleHeap`]'s object
/// table rather than a pointer, which is the shape the ADR's own sentence is
/// about — "an index or offset copied into a Rust local does not change
/// `Rc::strong_count`".
///
/// `Copy`, and deliberately so: a handle is data. Copying one is a `mov`, it
/// runs no destructor, and it tells nobody. Everything in this module's
/// documentation follows from that one property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Handle(u32);

impl Handle {
    /// The absence of an object, for a reference word that names none.
    ///
    /// A real layout would more likely give a nullable reference a niche of
    /// its own; this is the smallest thing that lets a test build a cycle in
    /// two steps, which needs an object to exist before the handle to it does.
    pub(crate) const NONE: Handle = Handle(u32::MAX);

    /// Whether this names no object.
    pub(crate) fn is_none(self) -> bool {
        self == Handle::NONE
    }

    /// The handle as the eight bytes a slot holds.
    pub(crate) fn to_slot(self) -> Slot {
        self.0 as Slot
    }

    /// The handle a slot's eight bytes are, read because the layout says the
    /// slot is a reference and for no other reason.
    pub(crate) fn from_slot(bits: Slot) -> Handle {
        Handle(bits as u32)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

/// Which layout an object has, as the index of one in [`HandleHeap`]'s table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayoutId(u32);

/// What the VM owns about one shape of heap object.
///
/// ADR 0028 decision 2 names five things a header or the VM's metadata must
/// carry. Three of them are here — a layout id, the object's size, and its
/// reference map — and the other two are deliberately absent. There is no
/// payload layout beyond "`words` words", because this slice has no aggregate
/// worth describing; and there is no movement guarantee, because ADR 0011 and
/// the Language Card make collection non-moving and decision 2 records that
/// as an allocator invariant rather than a mandatory word in every header.
/// [`HandleHeap`] never moves an object.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    name: &'static str,
    words: usize,
    /// Which of the object's words are handles.
    ///
    /// This is the reference map, and it is the only thing that decides.
    /// A word this does not name is scalar bits and is never read as a
    /// handle, however much it may look like one — which is decision 1's
    /// invariant that "a slot the layout calls scalar must never be reachable
    /// by a walk that treats it as a reference", stated for an object's
    /// interior rather than for a frame.
    refs: Vec<usize>,
}

impl Layout {
    /// A layout of `words` eight-byte words, of which those at `refs` are
    /// handles.
    pub(crate) fn new(name: &'static str, words: usize, refs: Vec<usize>) -> Layout {
        for &at in &refs {
            assert!(
                at < words,
                "{name}'s reference map names word {at} of {words}"
            );
        }
        Layout { name, words, refs }
    }

    /// How many eight-byte words the object holds.
    pub(crate) fn words(&self) -> usize {
        self.words
    }

    /// The layout's name, for a panic message.
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }
}

/// One VM-owned heap object: a layout and its words.
#[derive(Clone, Debug)]
struct Object {
    layout: LayoutId,
    words: Vec<Slot>,
}

/// One task's roots, as something that can drive a walk of the handles it
/// holds.
///
/// The handle counterpart of [`crate::heap::Roots`], and it owes less than
/// that trait does. A `Roots` walk is asked for twice and must yield each
/// reference exactly once, because the collector compares what it saw with
/// `Rc::strong_count`. This walk is asked for once and nothing is counted, so
/// what it owes is only that it reach every root storage location. Yielding
/// one handle from two locations is not a fault here; missing a location is.
pub(crate) trait HandleRoots {
    /// Calls `visit` once for every root storage location holding a handle.
    fn walk(&self, visit: &mut dyn FnMut(Handle));
}

/// One logical frame of eight-byte untagged slots, and the map that says
/// which of them are references.
///
/// ADR 0028 decision 1 asks for one contiguous region, one numbering and one
/// base. This is that, at the size a rooting proof needs: no frame stack, no
/// call convention, no operand/local distinction. What it does carry is the
/// part decision 8 depends on — **a handle slot is a root according to the
/// frame reference map**, and a scalar slot contains no reference by
/// construction.
///
/// The map is a bit per slot rather than a stack map derived from
/// `cove_ir::Function`, because deriving one is lowering work and this slice
/// is not lowering anything. What it demonstrates is the property either
/// arrangement has to have: the walk consults the map and never the bits.
#[derive(Debug, Default)]
pub(crate) struct Frame {
    slots: Vec<Slot>,
    /// Whether the slot at each index holds a handle.
    refs: Vec<bool>,
}

impl Frame {
    /// An empty frame.
    pub(crate) fn new() -> Frame {
        Frame::default()
    }

    /// How many slots stand in the frame.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the frame holds no slot at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Appends a slot the layout calls scalar, holding `bits`.
    pub(crate) fn push_scalar(&mut self, bits: Slot) -> usize {
        self.slots.push(bits);
        self.refs.push(false);
        self.slots.len() - 1
    }

    /// Appends a slot the layout calls a reference, holding `handle`.
    pub(crate) fn push_handle(&mut self, handle: Handle) -> usize {
        self.slots.push(handle.to_slot());
        self.refs.push(true);
        self.slots.len() - 1
    }

    /// Drops every slot above `len`, which is what returning from a call does.
    pub(crate) fn truncate(&mut self, len: usize) {
        self.slots.truncate(len);
        self.refs.truncate(len);
    }

    /// The handle standing in slot `at`.
    ///
    /// # Panics
    ///
    /// If the layout calls that slot scalar. Reading a scalar slot as a
    /// reference is the one thing decision 1's invariant forbids, so it is a
    /// programming error here rather than a value this can answer.
    pub(crate) fn handle_at(&self, at: usize) -> Handle {
        assert!(self.refs[at], "slot {at} is scalar, not a reference");
        Handle::from_slot(self.slots[at])
    }

    /// The bits standing in slot `at`, whatever the layout calls it.
    pub(crate) fn bits_at(&self, at: usize) -> Slot {
        self.slots[at]
    }

    /// Takes the handle out of slot `at` and leaves the slot scalar.
    ///
    /// This is the move ADR 0028 decision 8 says must be proved safe: after
    /// it, the object is named by the returned `Handle` and by nothing the
    /// frame's reference map describes, so a collection at the next safepoint
    /// will not find it from the frame. The caller either puts it back with
    /// [`Frame::put_handle`] before the next safepoint or registers it with
    /// [`Machine::with_root`], and those two are the whole of the discipline.
    ///
    /// The slot is left holding the handle's bits and marked scalar rather
    /// than cleared, which is on purpose: a stale word that still looks like
    /// a live handle is exactly what a walk that read bits instead of the map
    /// would trip over.
    pub(crate) fn take_handle(&mut self, at: usize) -> Handle {
        let handle = self.handle_at(at);
        self.refs[at] = false;
        handle
    }

    /// Puts a handle back into slot `at`, making the slot a reference again.
    pub(crate) fn put_handle(&mut self, at: usize, handle: Handle) {
        self.slots[at] = handle.to_slot();
        self.refs[at] = true;
    }
}

/// The shadow-root stack: every Rust-local handle that can survive to a
/// safepoint.
///
/// This is the mechanism ADR 0028 decision 8's second bullet names, and its
/// discipline is [`crate::heap::SlotRoots`]'s: push on the way in, truncate
/// back to the recorded depth on the way out. The tree-walking interpreter
/// already keeps its root list in step with its environment chain that way,
/// so this is a shape the codebase has rather than a new one.
///
/// Nothing here de-duplicates. A handle may stand in the stack twice, or in
/// the stack and in a frame slot at once, and both are yielded; marking is a
/// set union and does not care. See this module's "The three multiplicities"
/// for why that is sound here and would not be over a `Value`.
#[derive(Debug, Default)]
pub(crate) struct TempRoots {
    handles: Vec<Handle>,
}

impl TempRoots {
    /// An empty stack.
    pub(crate) fn new() -> TempRoots {
        TempRoots::default()
    }

    /// How many handles are registered. A caller records this before rooting
    /// anything and hands it back to [`TempRoots::truncate`] afterwards.
    pub(crate) fn depth(&self) -> usize {
        self.handles.len()
    }

    /// Registers one Rust-local handle.
    pub(crate) fn push(&mut self, handle: Handle) {
        self.handles.push(handle);
    }

    /// Unregisters everything pushed after `depth`.
    pub(crate) fn truncate(&mut self, depth: usize) {
        self.handles.truncate(depth);
    }
}

impl HandleRoots for TempRoots {
    fn walk(&self, visit: &mut dyn FnMut(Handle)) {
        for &handle in &self.handles {
            visit(handle);
        }
    }
}

/// One safepoint's roots: the frame's mapped handle slots, then the shadow
/// stack.
///
/// The counterpart of `vm::StackRoots`, and the list of what is *not* here is
/// the same kind of list that one carries. A scalar slot is not a root,
/// because the reference map says it holds no reference. The object table is
/// not a root: it is what is being collected, and treating it as a root would
/// mark everything.
pub(crate) struct Safepoint<'v> {
    pub(crate) frame: &'v Frame,
    pub(crate) temps: &'v TempRoots,
}

impl HandleRoots for Safepoint<'_> {
    /// Yields every mapped handle slot of the frame, then every registered
    /// temporary root.
    ///
    /// Each *storage location* is yielded once, which is ADR 0028 decision
    /// 8's first multiplicity. A handle reached from two locations is yielded
    /// twice and that is not a fault: see this module's "The three
    /// multiplicities".
    fn walk(&self, visit: &mut dyn FnMut(Handle)) {
        let slots = self.frame.slots.iter();
        for (&bits, &is_reference) in slots.zip(&self.frame.refs) {
            if is_reference {
                visit(Handle::from_slot(bits));
            }
        }
        self.temps.walk(visit);
    }
}

/// What one handle collection did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HandleCollection {
    /// Objects allocated since the previous collection.
    pub(crate) allocated: u64,
    /// Objects the sweep reclaimed.
    pub(crate) freed_objects: u64,
    /// Objects still live after the sweep.
    pub(crate) live_objects: u64,
    /// Bytes the live set holds: every live object's words.
    pub(crate) live_bytes: u64,
    /// How many root storage locations the walk yielded — ADR 0028 decision
    /// 8's first multiplicity, measured.
    pub(crate) roots_yielded: u64,
    /// How many times the mark phase read an object's reference map — decision
    /// 8's third multiplicity, measured. Equal to `live_objects` whatever the
    /// shape of the graph, because an object is expanded once.
    pub(crate) expansions: u64,
}

/// The fewest objects the slice's heap allocates between two collections,
/// matching [`crate::heap`]'s own floor so the two heaps pace alike.
const MIN_ALLOCATIONS_BETWEEN_COLLECTIONS: u64 = 64;

/// How much the object count may grow past the live set before the next
/// collection, matching [`crate::heap`].
const GROWTH_FACTOR: u64 = 2;

/// A VM-owned object heap, addressed by [`Handle`].
///
/// The difference from [`crate::heap::Heap`] that the whole slice turns on:
/// this heap **owns** its objects. `Heap` holds a `Weak` and lets `Rc` decide
/// every lifetime except a cycle's; here there is no `Rc` and no count, so
/// the collector decides every lifetime and an object survives exactly when
/// the mark phase reaches it. That is what makes a missing root a
/// use-after-free rather than a leak, and it is why the proof this module
/// owes has to be a liveness proof and not an accounting one.
#[derive(Debug)]
pub(crate) struct HandleHeap {
    layouts: Vec<Layout>,
    /// One entry per handle ever issued; `None` once the sweep has taken it.
    ///
    /// A swept slot is never reused, which keeps a stale handle naming a dead
    /// object rather than a live one and so makes the negative test observe
    /// the failure instead of a coincidence. A real heap would reuse with a
    /// generation counter, and that is a migration concern rather than a
    /// rooting one.
    objects: Vec<Option<Object>>,
    allocations_since_collection: u64,
    next_collection_at: u64,
    collections: u64,
}

impl HandleHeap {
    /// An empty heap.
    pub(crate) fn new() -> HandleHeap {
        HandleHeap {
            layouts: Vec::new(),
            objects: Vec::new(),
            allocations_since_collection: 0,
            next_collection_at: MIN_ALLOCATIONS_BETWEEN_COLLECTIONS,
            collections: 0,
        }
    }

    /// Records a layout and answers the id objects name it by.
    pub(crate) fn register(&mut self, layout: Layout) -> LayoutId {
        self.layouts.push(layout);
        LayoutId(self.layouts.len() as u32 - 1)
    }

    /// The layout `id` names.
    pub(crate) fn layout(&self, id: LayoutId) -> &Layout {
        &self.layouts[id.0 as usize]
    }

    /// Allocates an object of `layout` holding `words`.
    ///
    /// # Panics
    ///
    /// If `words` is not the width the layout declares. An object whose size
    /// disagrees with its layout is one whose reference map cannot be
    /// trusted, and ADR 0028 decision 2's required invariant is that the
    /// layout completely determines how to find every reference.
    pub(crate) fn allocate(&mut self, layout: LayoutId, words: Vec<Slot>) -> Handle {
        let declared = self.layout(layout);
        assert_eq!(
            words.len(),
            declared.words,
            "{} declares {} words",
            declared.name,
            declared.words
        );
        self.objects.push(Some(Object { layout, words }));
        self.allocations_since_collection += 1;
        Handle(self.objects.len() as u32 - 1)
    }

    /// Whether the object `handle` names still exists.
    pub(crate) fn is_live(&self, handle: Handle) -> bool {
        !handle.is_none()
            && self
                .objects
                .get(handle.index())
                .is_some_and(|slot| slot.is_some())
    }

    /// The word at `at` of the object `handle` names.
    ///
    /// # Panics
    ///
    /// If the object has been swept. That is the use-after-free the negative
    /// test provokes, and it panics rather than reading whatever is there
    /// because a prototype that returns garbage proves nothing.
    pub(crate) fn word(&self, handle: Handle, at: usize) -> Slot {
        self.object(handle).words[at]
    }

    /// Writes the word at `at` of the object `handle` names.
    pub(crate) fn set_word(&mut self, handle: Handle, at: usize, bits: Slot) {
        let object = self.objects[handle.index()]
            .as_mut()
            .unwrap_or_else(|| panic!("handle {handle:?} names a swept object"));
        object.words[at] = bits;
    }

    /// How many objects the heap holds.
    pub(crate) fn live_objects(&self) -> u64 {
        self.objects.iter().filter(|slot| slot.is_some()).count() as u64
    }

    /// How many collections this heap has run.
    pub(crate) fn collections(&self) -> u64 {
        self.collections
    }

    /// Whether enough has been allocated since the last collection to be
    /// worth another one — [`crate::heap::Heap::should_collect`]'s rule.
    pub(crate) fn should_collect(&self) -> bool {
        self.allocations_since_collection >= self.next_collection_at
    }

    fn object(&self, handle: Handle) -> &Object {
        self.objects
            .get(handle.index())
            .and_then(|slot| slot.as_ref())
            .unwrap_or_else(|| panic!("handle {handle:?} names a swept object"))
    }

    /// Marks from `roots` and sweeps what is not marked.
    pub(crate) fn collect(&mut self, roots: &dyn HandleRoots) -> HandleCollection {
        let mut marked: HashSet<u32> = HashSet::new();
        let mut work: Vec<Handle> = Vec::new();
        let mut roots_yielded = 0u64;

        roots.walk(&mut |handle| {
            roots_yielded += 1;
            if self.is_live(handle) && marked.insert(handle.0) {
                work.push(handle);
            }
        });

        // A worklist rather than recursion, for [`crate::heap::Marker`]'s
        // reason: a chain of objects is as long as the program made it, and
        // recursion over one would be bounded by the native stack.
        let mut expansions = 0u64;
        let mut live_bytes = 0u64;
        while let Some(handle) = work.pop() {
            expansions += 1;
            let object = self.object(handle);
            live_bytes += (object.words.len() * std::mem::size_of::<Slot>()) as u64;
            // The reference map, and nothing else, decides which words are
            // followed. A word it does not name is scalar bits, whatever
            // those bits happen to look like.
            for &at in &self.layouts[object.layout.0 as usize].refs {
                let child = Handle::from_slot(object.words[at]);
                if self.is_live(child) && marked.insert(child.0) {
                    work.push(child);
                }
            }
        }

        let mut freed_objects = 0u64;
        for (at, slot) in self.objects.iter_mut().enumerate() {
            if slot.is_some() && !marked.contains(&(at as u32)) {
                *slot = None;
                freed_objects += 1;
            }
        }

        let collection = HandleCollection {
            allocated: self.allocations_since_collection,
            freed_objects,
            live_objects: marked.len() as u64,
            live_bytes,
            roots_yielded,
            expansions,
        };
        self.allocations_since_collection = 0;
        self.next_collection_at =
            (collection.live_objects * GROWTH_FACTOR).max(MIN_ALLOCATIONS_BETWEEN_COLLECTIONS);
        self.collections += 1;
        collection
    }
}

/// A dispatch loop's worth of the slice: a frame, a shadow stack, and a heap
/// with a safepoint between them.
///
/// The point of having this rather than calling [`HandleHeap::collect`] from a
/// test is that a *safepoint* is not the same thing as a collection. A
/// safepoint is a point at which a collection is permitted to happen, and
/// whether one does is the heap's decision. The liveness proof this module
/// owes is that an object survives a collection **the machine chose to run**
/// while its only handle was in a Rust local, so the tests reach a real
/// safepoint and assert that a collection actually happened there.
#[derive(Debug)]
pub(crate) struct Machine {
    pub(crate) heap: HandleHeap,
    pub(crate) frame: Frame,
    temps: TempRoots,
}

impl Machine {
    /// A machine with an empty heap, frame and shadow stack.
    pub(crate) fn new() -> Machine {
        Machine {
            heap: HandleHeap::new(),
            frame: Frame::new(),
            temps: TempRoots::new(),
        }
    }

    /// Allocates, without collecting. Allocation is not a safepoint here, as
    /// it is not in `Vm`.
    pub(crate) fn allocate(&mut self, layout: LayoutId, words: Vec<Slot>) -> Handle {
        self.heap.allocate(layout, words)
    }

    /// A safepoint: collects if the heap says one is due, and answers what it
    /// did.
    ///
    /// `Vm::safepoint` calls `Vm::collect_if_due` after charging fuel; this is
    /// that call with the budget taken out, which is the only part rooting
    /// depends on.
    pub(crate) fn safepoint(&mut self) -> Option<HandleCollection> {
        if !self.heap.should_collect() {
            return None;
        }
        let roots = Safepoint {
            frame: &self.frame,
            temps: &self.temps,
        };
        Some(self.heap.collect(&roots))
    }

    /// Collects now, whatever the heap's pacing says — for a test that wants
    /// one collection at a known point rather than a safepoint's decision.
    pub(crate) fn collect_now(&mut self) -> HandleCollection {
        let roots = Safepoint {
            frame: &self.frame,
            temps: &self.temps,
        };
        self.heap.collect(&roots)
    }

    /// Runs `body` with `handle` registered as a temporary root.
    ///
    /// **This is the mechanism.** A dispatch loop that takes a handle out of
    /// a slot and can reach a safepoint before putting it back wraps the
    /// stretch between the two in this, and the handle is a root for exactly
    /// that stretch. The push and the truncate are paired by the scope rather
    /// than by anyone remembering, which is the property a shadow-root stack
    /// has and the dispatch-discipline mechanism does not: forgetting it is
    /// visible at the one site that needed it, instead of being a global
    /// invariant re-proved by reading.
    ///
    /// Truncating to the recorded depth rather than popping one is
    /// [`crate::heap::SlotRoots`]'s discipline, and it makes nesting and
    /// early exit correct without either being a special case.
    pub(crate) fn with_root<R>(
        &mut self,
        handle: Handle,
        body: impl FnOnce(&mut Machine) -> R,
    ) -> R {
        let depth = self.temps.depth();
        self.temps.push(handle);
        let answer = body(self);
        self.temps.truncate(depth);
        answer
    }

    /// How many handles the shadow stack holds, so a test can check that
    /// [`Machine::with_root`] leaves nothing behind.
    pub(crate) fn rooted(&self) -> usize {
        self.temps.depth()
    }
}

impl Default for Machine {
    fn default() -> Machine {
        Machine::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layout with one scalar word and one reference word: the smallest
    /// object that can point at another.
    fn node(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::new("test.Node", 2, vec![1]))
    }

    /// A layout of two words the map calls scalar, so that neither is ever
    /// read as a handle.
    fn pair(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::new("test.Pair", 2, Vec::new()))
    }

    /// Allocates `count` objects nothing points at, which is how a test makes
    /// the heap decide a collection is due.
    fn churn(machine: &mut Machine, layout: LayoutId, count: usize) {
        for n in 0..count {
            machine.allocate(layout, vec![n as Slot, Handle::NONE.to_slot()]);
        }
    }

    // ------------------------------------------------------- the gate

    /// The failure ADR 0028 decision 8 says the ADR does not claim survives:
    /// a handle copied into a Rust local is not a root, and the object it
    /// names is swept out from under it.
    ///
    /// This is the negative direction of the liveness proof, and it is why
    /// the positive one is not vacuous. The two tests are the same program
    /// with one difference — [`Machine::with_root`] — so the mechanism is
    /// exactly what the difference in outcome measures.
    #[test]
    fn a_bare_handle_in_a_rust_local_is_not_a_root() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let held = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        let at = machine.frame.push_handle(held);

        // Out of the slot and into a Rust local. Nothing observed this: no
        // count moved, because there is no count.
        let local = machine.frame.take_handle(at);
        assert_eq!(local, held);

        let collected = machine.collect_now();
        assert_eq!(
            collected.freed_objects, 1,
            "an unrooted handle's object should have been swept: {collected:?}"
        );
        assert!(
            !machine.heap.is_live(local),
            "the local now names a free slot, which is the use-after-free the \
             mechanism has to prevent"
        );
    }

    /// The proof decision 8 asks for: a safepoint with a heap handle
    /// temporarily outside the frame, a collection that actually runs there,
    /// and the object still live afterwards.
    #[test]
    fn a_handle_outside_the_frame_survives_a_safepoint_that_collects() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let held = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        let at = machine.frame.push_handle(held);

        let local = machine.frame.take_handle(at);
        let collections = machine.with_root(local, |machine| {
            // The handle is in a Rust local and in the shadow stack, and in
            // no slot the frame's reference map describes. Allocate past the
            // heap's threshold and reach a safepoint, exactly as a dispatch
            // loop does.
            let mut collections = 0;
            for _ in 0..(MIN_ALLOCATIONS_BETWEEN_COLLECTIONS * 2) {
                machine.allocate(layout, vec![0, Handle::NONE.to_slot()]);
                if let Some(collection) = machine.safepoint() {
                    collections += 1;
                    assert_eq!(
                        collection.live_objects, 1,
                        "the rooted object, and nothing else: {collection:?}"
                    );
                }
            }
            collections
        });

        assert!(
            collections > 0,
            "the safepoint has to have collected, or this test proves nothing"
        );
        assert!(machine.heap.is_live(local), "the rooted object survived");
        assert_eq!(machine.heap.word(local, 0), 7, "and its contents did too");
        assert_eq!(machine.rooted(), 0, "the shadow stack was left as found");

        // And the shadow root is what did it. The scope has ended, so the
        // handle is registered nowhere and no slot's reference map describes
        // it; the next collection takes the object, and the Rust local that
        // still holds its bits names a free slot.
        machine.collect_now();
        assert!(
            !machine.heap.is_live(local),
            "with the root gone the object goes, which is what makes the \
             survival above the mechanism's doing and not the heap's"
        );
    }

    /// The same, one level deeper: a rooted object's own references are
    /// reached through its layout's reference map, so a graph hanging off a
    /// Rust-local handle survives with it.
    #[test]
    fn what_a_rooted_handle_reaches_survives_with_it() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let inner = machine.allocate(layout, vec![11, Handle::NONE.to_slot()]);
        let outer = machine.allocate(layout, vec![22, inner.to_slot()]);

        let survived = machine.with_root(outer, |machine| {
            let collected = machine.collect_now();
            assert_eq!(collected.live_objects, 2, "{collected:?}");
            machine.heap.is_live(inner)
        });
        assert!(survived);
        assert_eq!(machine.heap.word(inner, 0), 11);
    }

    /// Putting the handle back is the other half of the discipline, and it is
    /// enough on its own: a stretch with no safepoint in it needs no shadow
    /// root.
    #[test]
    fn a_handle_returned_to_a_mapped_slot_needs_no_shadow_root() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let held = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        let at = machine.frame.push_handle(held);

        let local = machine.frame.take_handle(at);
        machine.frame.put_handle(at, local);

        let collected = machine.collect_now();
        assert_eq!(collected.freed_objects, 0);
        assert!(machine.heap.is_live(held));
    }

    // ------------------------------------- the frame's reference map

    /// Decision 1's invariant, from the frame's side: a slot the layout calls
    /// scalar is never reachable by a walk that treats it as a reference —
    /// however much its bits look like a live handle.
    ///
    /// The scalar here holds the exact index of a real object, which is what
    /// a conservative scan would root and a precise one must not.
    #[test]
    fn a_scalar_slot_holding_a_live_handles_bits_is_not_a_root() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let object = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        machine.frame.push_scalar(object.to_slot());

        let collected = machine.collect_now();
        assert_eq!(collected.roots_yielded, 0, "a scalar slot yields nothing");
        assert_eq!(collected.freed_objects, 1, "{collected:?}");
        assert!(!machine.heap.is_live(object));
    }

    /// The same invariant inside an object: a word the layout's reference map
    /// does not name is scalar bits, whatever they look like.
    #[test]
    fn a_scalar_word_holding_a_live_handles_bits_is_not_an_edge() {
        let mut machine = Machine::new();
        let nodes = node(&mut machine.heap);
        let pairs = pair(&mut machine.heap);
        let hidden = machine.allocate(nodes, vec![7, Handle::NONE.to_slot()]);
        let holder = machine.allocate(pairs, vec![hidden.to_slot(), hidden.to_slot()]);
        machine.frame.push_handle(holder);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 1, "{collected:?}");
        assert!(machine.heap.is_live(holder));
        assert!(!machine.heap.is_live(hidden));
    }

    // ------------------------------------------- the three multiplicities

    /// Decision 8's first multiplicity beside its third. A handle standing in
    /// a frame slot *and* registered as a temporary root is two root storage
    /// locations, and both are yielded; it is one object, and it is expanded
    /// once.
    ///
    /// This is the distinction that makes the shadow stack safe here and
    /// unsafe over a `Value`. `crate::heap` would have to yield the second
    /// location not at all, because a reference counted twice conceals the
    /// shortfall that roots it.
    #[test]
    fn a_handle_in_a_slot_and_in_the_shadow_stack_is_two_locations_and_one_object() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let held = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        machine.frame.push_handle(held);

        let collected = machine.with_root(held, |machine| machine.collect_now());
        assert_eq!(collected.roots_yielded, 2, "the slot and the shadow root");
        assert_eq!(collected.live_objects, 1);
        assert_eq!(collected.expansions, 1, "expanded once, not twice");
        assert_eq!(collected.freed_objects, 0);
    }

    /// Decision 8's third multiplicity against a shape that would break a
    /// walk without a marked set: one object reached by four edges from two
    /// others, plus two root locations.
    #[test]
    fn a_shared_object_reached_by_many_edges_is_expanded_once() {
        let mut machine = Machine::new();
        let nodes = node(&mut machine.heap);
        let shared = machine.allocate(nodes, vec![9, Handle::NONE.to_slot()]);
        let one = machine.allocate(nodes, vec![1, shared.to_slot()]);
        let two = machine.allocate(nodes, vec![2, shared.to_slot()]);
        machine.frame.push_handle(one);
        machine.frame.push_handle(two);
        machine.frame.push_handle(shared);

        let collected = machine.collect_now();
        assert_eq!(collected.roots_yielded, 3);
        assert_eq!(collected.live_objects, 3);
        assert_eq!(
            collected.expansions, 3,
            "three objects, three expansions, however many edges: {collected:?}"
        );
    }

    /// The shape that would not terminate at all without it, and the one the
    /// tracing collector exists for: a cycle. `Rc` alone never frees one,
    /// which is `crate::heap`'s whole reason for being; a handle heap frees
    /// it for the same reason it frees anything, because nothing reached it.
    #[test]
    fn a_cycle_of_handles_is_expanded_once_and_reclaimed() {
        let mut machine = Machine::new();
        let nodes = node(&mut machine.heap);
        let a = machine.allocate(nodes, vec![1, Handle::NONE.to_slot()]);
        let b = machine.allocate(nodes, vec![2, a.to_slot()]);
        machine.heap.set_word(a, 1, b.to_slot());

        machine.frame.push_handle(a);
        let reached = machine.collect_now();
        assert_eq!(reached.live_objects, 2);
        assert_eq!(reached.expansions, 2, "a cycle is expanded once round");

        machine.frame.truncate(0);
        let dropped = machine.collect_now();
        assert_eq!(dropped.freed_objects, 2);
        assert_eq!(dropped.expansions, 0);
        assert!(!machine.heap.is_live(a));
        assert!(!machine.heap.is_live(b));
    }

    // ------------------------------------------------ the discipline itself

    /// Truncate-to-depth, not pop: nested roots come off in the right order
    /// and each scope leaves the stack as it found it.
    #[test]
    fn nested_roots_unwind_to_the_depth_each_scope_recorded() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let outer = machine.allocate(layout, vec![1, Handle::NONE.to_slot()]);
        let inner = machine.allocate(layout, vec![2, Handle::NONE.to_slot()]);

        let seen = machine.with_root(outer, |machine| {
            assert_eq!(machine.rooted(), 1);
            let inside = machine.with_root(inner, |machine| {
                assert_eq!(machine.rooted(), 2);
                machine.collect_now()
            });
            assert_eq!(machine.rooted(), 1, "the inner scope came off");
            let after = machine.collect_now();
            (inside, after)
        });
        assert_eq!(machine.rooted(), 0, "and so did the outer");

        assert_eq!(seen.0.live_objects, 2, "both were rooted: {:?}", seen.0);
        assert_eq!(
            seen.1.live_objects, 1,
            "only the outer was, once the inner scope ended: {:?}",
            seen.1
        );
        assert!(machine.heap.is_live(outer));
        assert!(!machine.heap.is_live(inner));
    }

    /// A root registered by a scope that has ended is not a root, which is
    /// what makes the stack a stack rather than a leak. The object rooted in
    /// the first scope is swept by a collection in the second.
    #[test]
    fn a_root_does_not_outlive_the_scope_that_pushed_it() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let first = machine.allocate(layout, vec![1, Handle::NONE.to_slot()]);
        let second = machine.allocate(layout, vec![2, Handle::NONE.to_slot()]);

        machine.with_root(first, |_| ());
        let collected = machine.with_root(second, |machine| machine.collect_now());

        assert_eq!(collected.roots_yielded, 1);
        assert!(!machine.heap.is_live(first));
        assert!(machine.heap.is_live(second));
    }

    /// The heap paces itself the way `crate::heap::Heap` does, so a safepoint
    /// in a program that allocates nothing collects nothing — which is what
    /// makes the liveness test's "a collection actually ran" assertion mean
    /// something.
    #[test]
    fn a_safepoint_is_not_a_collection() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        assert!(machine.safepoint().is_none(), "an empty heap is not due");

        churn(
            &mut machine,
            layout,
            MIN_ALLOCATIONS_BETWEEN_COLLECTIONS as usize - 1,
        );
        assert!(machine.safepoint().is_none(), "one short of the floor");

        churn(&mut machine, layout, 1);
        assert!(machine.safepoint().is_some(), "at the floor");
        assert_eq!(machine.heap.collections(), 1);
    }

    /// The reference map decides what an object's interior costs to walk, and
    /// the live figure is the words of the objects the mark phase reached.
    #[test]
    fn live_bytes_are_the_words_the_mark_phase_reached() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        assert_eq!(machine.heap.layout(layout).words(), 2);
        let held = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        machine.allocate(layout, vec![8, Handle::NONE.to_slot()]);
        machine.frame.push_handle(held);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 1);
        assert_eq!(
            collected.live_bytes,
            2 * std::mem::size_of::<Slot>() as u64,
            "one object of two eight-byte words: {collected:?}"
        );
    }

    /// A slot is eight bytes, which is the number ADR 0028 decision 1 fixes
    /// and the reason the whole design exists. A handle fits in one with room
    /// to spare, and a full `Int` or IEEE-754 `Float` bit pattern fills one
    /// exactly.
    #[test]
    fn a_slot_is_eight_bytes_and_a_handle_fits_in_one() {
        assert_eq!(std::mem::size_of::<Slot>(), 8);
        assert!(std::mem::size_of::<Handle>() <= std::mem::size_of::<Slot>());
        // Round-tripping the extremes of the domains decision 1 promises a
        // typed slot preserves, which a tagged 8-byte value could not.
        for bits in [0, i64::MAX as u64, i64::MIN as u64, f64::NAN.to_bits()] {
            let mut frame = Frame::new();
            let at = frame.push_scalar(bits);
            assert_eq!(frame.bits_at(at), bits);
        }
    }

    /// The reference map is consulted, not the bits, and a layout that lies
    /// about its own width is refused at the allocation rather than at the
    /// walk.
    #[test]
    #[should_panic(expected = "test.Node declares 2 words")]
    fn an_object_whose_size_disagrees_with_its_layout_is_refused() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        machine.allocate(layout, vec![1]);
    }

    /// Reading a scalar slot as a reference is a programming error, not an
    /// answer.
    #[test]
    #[should_panic(expected = "slot 0 is scalar")]
    fn a_scalar_slot_cannot_be_read_as_a_handle() {
        let mut frame = Frame::new();
        frame.push_scalar(0);
        frame.handle_at(0);
    }
}
