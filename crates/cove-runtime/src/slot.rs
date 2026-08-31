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
//! [`crate::frame`]'s Phase B is that sentence's other half. Its slots *are*
//! eight bytes, so [`HandleHeap`], [`Handle`], [`Layout`] and [`TempRoots`] are
//! wired into it and it collects at the safepoints `Vm` collects at, over a
//! bitmap rather than over this module's [`Frame`]. What stays here is the
//! boundary — [`Machine`] and its materialiser — which Phase B does not reach,
//! because an aggregate does not yet cross out of it.
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
//! to spoil. The two heaps are therefore kept disjoint. No object here can
//! hold a [`Value`]: an object's payload is [`Slot`] words, and a word is
//! either scalar bits or a [`Handle`]. The shadow-root stack cannot become a
//! second path to anything `crate::heap` already yields, because it cannot
//! name anything `crate::heap` manages.
//!
//! # The boundary, and where the handover happens
//!
//! [`Value`] is named in exactly one place in this module — [`Machine`]'s
//! materialiser — and that is decision 5's boundary. It is the one place the
//! two heaps meet, so it is where the paragraph above stops being a claim and
//! becomes something a test can fail.
//!
//! The direction of travel is the whole of the argument. A [`Handle`] goes in
//! and an owned `Value` comes out. Nothing goes the other way: there is no
//! constructor here that puts a `Value` into an object, and the `Value` that
//! comes out holds no handle, no layout id and no index into
//! [`HandleHeap`]. Decision 5 is what makes that true rather than a
//! convention — a `Value` "is not a window onto a slot, a heap object or a
//! dynamic value; it is a separate object with a representation of its own".
//!
//! **The handover is per part, and it is a copy.** [`Machine::materialise`]
//! reads one word of one object, builds the `Value` that word means, and from
//! the instant that `Value` exists the part is owned by `crate::heap`'s
//! counted world and by nothing in this one. The reverse holds at the same
//! instant: nothing in the handle heap changed, so no object here became
//! reachable from a `Value`.
//!
//! **Nothing is double-counted across it**, and there are two directions to
//! say that in:
//!
//! - `crate::heap`'s walk and its `Rc::strong_count` comparison can never see
//!   a handle, because no `Value` stores one. A materialised `Value` is an
//!   ordinary `Value` in a Rust local of whoever asked for it, rooted by its
//!   own count exactly as `Vm::take`'s argument vector is, and #192's rule
//!   about `Vm::arg_vectors` applies to it unchanged. The handle heap adds no
//!   reference for `crate::heap` to count and takes none away.
//! - [`Safepoint::walk`] and [`TempRoots`] can never yield a `Value`, because
//!   nothing here holds one past the expression that builds it. A root
//!   location in this module is a [`Slot`] the reference map calls a handle,
//!   or an entry of the shadow stack, and both are [`Handle`]s.
//!
//! So the two root sets are over disjoint universes, and "yielded twice" is a
//! question that cannot be asked across the seam. What *can* go wrong at the
//! seam is the failure this module already exists for: the source handle is
//! in a Rust local for the whole of the materialisation, reading a part is VM
//! work, and VM work reaches safepoints. That is why
//! [`Machine::materialise`] is [`Machine::with_root`]'s first real caller and
//! `the_source_is_swept_mid_materialisation_without_the_root` is what it costs
//! to forget.
//!
//! # A variable-length tail, and what a reference map can say about one
//!
//! Decision 2 requires an object's header to carry its "payload layout,
//! including a variable-length tail where it has one". Everything else in this
//! module is a fixed set of words, so the half of that sentence after the
//! comma was specified and unexercised, and so was the reference map's ability
//! to describe a run whose length is not known until the allocation.
//!
//! A tail is split between the two places an object's description lives, and
//! the split is forced rather than chosen:
//!
//! - the [`Layout`] — one per lowered type, written before any object of it
//!   exists — carries the fixed part, a per-word reference map for *that*, and
//!   **one** [`Part`] for the whole tail;
//! - the object's own header carries how many tail words there are, because
//!   that is settled by the allocation and by nothing earlier.
//!
//! So the reference map is two rules rather than a bitmap: a set of indices
//! for the fixed part, and a single bit for the tail. That is not an economy,
//! it is what a variable length permits. A per-word map of a tail cannot be
//! written down at lowering time, when the length is unknown — and it need not
//! be, because the collector's question about a word is a yes-or-no and every
//! word of a tail answers it the same way.
//!
//! Both answers are exercised, and the second is the one that matters.
//! [`Shape::Array`] with [`Part::Nested`] is a run of handles the mark phase
//! walks, which is the thing a reference map exists for; the same shape with
//! [`Part::Int`] is a run of scalars the mark phase must not follow one word
//! of, however much those words look like live handles, which is decision 1's
//! invariant — "a slot the layout calls scalar must never be reachable by a
//! walk that treats it as a reference" — stated for a tail, where a walk that
//! guessed from the bits would guess in bulk. [`Shape::Str`] is the third
//! case: a tail whose word count is not its element count, over a fixed part
//! that is load-bearing. The map is indifferent to the packing, and that
//! indifference is the point.
//!
//! ## A tail of handles is a run of siblings
//!
//! [`Machine::materialise_args`] is where a second root is load-bearing rather
//! than redundant: a nested object is reachable from the rooted parent that
//! names it, but a call's arguments are *siblings*, and no one of them roots
//! another. A tail of handles is that case at a scale the program rather than
//! the frame chooses, and [`Machine::materialise_tail_args`] is it — a spread
//! call whose whole argument list is one array.
//!
//! The array is the crossing's argument vector, so nothing roots it, and it is
//! swept at the first safepoint inside the crossing while every one of its
//! former elements survives. That is the assertion and not an accident: it is
//! what says the tail's reference map is not what keeps the elements alive
//! there. The shadow stack is. Rooting them one at a time sweeps the rest,
//! which is `rooting_one_tail_element_at_a_time_sweeps_the_siblings`.
//!
//! Nothing about the mechanism had to change for the scale. Truncate-to-depth
//! covers eight roots the way it covers two, so a tail turns out to be a large
//! instance of the sibling case rather than a new one.
//!
//! ## Decision 7's `Elements` guard, and where a tail meets it
//!
//! [`Elements`](crate::value::Elements) is decision 7's opaque public guard
//! over a `Vector`'s `RefCell`, there because an alias may write the elements
//! and so nothing can hand out a plain `&[Value]` of them. A tail is the
//! handle-heap analogue of the same problem — a run of words the VM owns and
//! a host wants to read — so whether the guard's shape fits one is a question
//! this slice owes an answer to. The answer is in two halves.
//!
//! **For an array the guard fits and is not needed.** A tail materialises as
//! `Value::array`, a copy, and the copy is read with `Value::items`, which
//! answers a plain slice because the materialised array's storage does sit
//! still. Handing out a guard there would be answering a question nobody
//! asked.
//!
//! **A guard onto a live tail is a different object, and it is refused rather
//! than missing.** `Elements` borrows from a `Value`; a guard whose lifetime
//! came from [`HandleHeap`] would be a window onto VM storage, and ADR 0028's
//! "The tension #195 left" refuses exactly that in its second reason — "a lazy
//! window keeps a `Value` alive against VM storage, which means a host holding
//! one constrains when a collection may run", where materialisation is what
//! keeps the safepoint assumption true. So `Elements`'s shape is the one a
//! tail must *not* be handed out with, and the fact that it fits an array so
//! easily is because the guard is over a materialisation and not over a heap.
//!
//! What is left over is not about the guard's shape at all, and it is the
//! finding this slice ends on. Decision 7 also says that "the values whose
//! identity is observable — `Vector`, `Shared`, `Task`, `TaskScope`,
//! `Resource` — are materialized as handles rather than as copies", and that
//! `Vector` keeps having no copying constructor because `is` is defined for
//! it. A Cove `Vector` living in this heap would be a tail. Materialising it
//! as a copy is what decision 7 refuses; materialising it "as a handle" cannot
//! mean a [`Handle`], because a `Value` holding one would join the two heaps
//! whose disjointness is what makes everything above sound. A tail therefore
//! reaches every aggregate whose identity is *not* observable — array, string,
//! and any composition of them — and stops at the five that is. Which way that
//! stops is a decision ADR 0028 does not take: either those types stay in
//! `crate::heap`'s counted world and never become tails, or a `Value` gains a
//! way to name VM storage and the seam stops being one-way. This slice picks
//! neither, because picking one is not a rooting question.
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
//!    pins it. A tail does not change the rule and makes it easier to break:
//!    `a_tail_naming_one_object_twice_is_two_locations_and_one_expansion` is
//!    two tail slots naming one object, which is two locations to root and one
//!    object to expand.
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
//!    `a_shared_object_reached_by_many_edges_is_expanded_once`,
//!    `a_cycle_of_handles_is_expanded_once_and_reclaimed` and
//!    `a_shared_object_in_many_tail_slots_is_expanded_once` are the three
//!    shapes that would break it.
//!
//! # What this is not
//!
//! Not a migration. [`Value`] is unchanged, the public API is unchanged, and
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
//! - ~~**A handle slot is never reused.**~~ It is now, because a benchmark is
//!   a run that lasts: `benches/field` allocates one object a turn for two
//!   million turns, and an object table that only grows makes every sweep walk
//!   everything the run ever allocated. The sweep returns an entry to a free
//!   list and [`Handle`] carries a generation, which is what keeps a stale
//!   handle naming a dead object *after* its index has been handed out again.
//!   What is still owed is the tail of that: an entry keeps the word buffer of
//!   the object the sweep took, so a dead object's memory is held until the
//!   entry is reused rather than returned.
//! - **No enum layout is *selected*, and there is no `Dynamic`.** [`Shape`]
//!   gives an enum one layout per case with the case in its header, which is
//!   the form decision 2 says the prototype may use for implementation
//!   economy and explicitly must not turn into a default without measuring
//!   it against an immediate discriminant, typed payload slots or a niche.
//!   Decision 3's two-slot `Dynamic` needs a witness the reference map can
//!   read, and it is not here; adding one changes what a reference map has to
//!   say.
//!
//!   The tail is worth one finding for whichever round selects that layout,
//!   because it moves a line the niche form would have to cross. A reference
//!   map is now a function of the layout *and one number in the object's
//!   header*: how long the tail is. That number is written at the allocation
//!   and is never a value the program can see. A niche layout would make the
//!   map a function of the object's *payload* instead — the same word being
//!   both a value and the thing that says how to read a value — and those two
//!   are different in kind rather than in degree. Decision 2 already says a
//!   niche is "more complex because the reference map may have to interpret
//!   the word according to the enum layout"; a tail is evidence for how much,
//!   since it is the weakest possible version of that dependency and it still
//!   required the header to exist. Neither of the other two forms is affected:
//!   an immediate discriminant and typed payload slots are both fixed maps.
//! - **No aggregate whose identity is observable.** A tail materialises as an
//!   array or a string, which are values a copy is right for. `Vector`,
//!   `Shared`, `Task`, `TaskScope` and `Resource` are the five decision 7 says
//!   are materialised as handles because `is` can tell them apart, and
//!   "Decision 7's `Elements` guard, and where a tail meets it" above is why a
//!   tail cannot be one of them without either a copying constructor decision
//!   7 refuses or a `Value` that stores a [`Handle`]. So `vector_elements` has
//!   nothing here to materialise from and will not until that is decided.
//!   `Map` and `Set` are unbuilt for a smaller reason: their tails would be
//!   `MapKey`s in key order, and a [`Part`] has no way to say "a key".
//! - **A tail is copied at the boundary and never windowed**, which is
//!   deliberate and is also the reason the two heaps stay disjoint. Whether
//!   the copy is what a large array should cost is a measurement question,
//!   and it is the one decision 5's "the boundary can only get more expensive"
//!   most obviously points at now that the thing crossing can be as long as
//!   the program likes.
//! - **The boundary is one-way.** Decision 5 says a `Value` is *built* on the
//!   way out and *consumed* on the way in, and only the first half is here.
//!   Consuming one — a host's answer becoming slots and objects — is the
//!   direction that would have to allocate in the handle heap while a
//!   half-built object is in a Rust local, which is a rooting question of its
//!   own and a different one.
//! - **Nothing is measured.** #197's prototype phase ends at a measurement
//!   gate, and this slice deliberately does not approach it. Decision 5's
//!   own cost — "the boundary can only get more expensive" — is the number
//!   this materialiser most obviously owes, and it is not taken here.

// `crate::frame` names the heap, the handle, the layout and the shadow stack
// now; it did not when this module was written, and the paragraph that used to
// stand here said so. What is still true is the half that mattered: nothing in
// the **live** `Vm` names any of it, because wiring it there before the
// migration would mean paying for two heaps to run one.
//
// The `allow` stays because the boundary half of the slice — `Machine`, the
// materialiser, `Shape`, the tail — is still reached only by this module's own
// tests, and it is scoped to this file so it hides nothing else.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::Arc;

use crate::value::Value;

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
/// # An index and a generation, and why the generation is here
///
/// A handle was a bare index while the heap never reused an entry, which was
/// enough for a rooting proof and is not enough for a heap a benchmark runs
/// against: `benches/field` allocates one object a turn for two million
/// turns, and an object table that only grows makes every sweep walk two
/// million entries. So the sweep now returns an entry to a free list, and the
/// generation is what keeps the negative tests honest across that reuse — a
/// handle to a swept object does not become a handle to whatever was
/// allocated in its place, because the entry's generation moved on and the
/// handle's did not.
///
/// **Generation zero is never issued.** An entry starts at 1, so eight zero
/// bytes are never a live handle and a frame slot that has been given no
/// object yet is safe for the walk to visit without being filled in first.
/// That is what lets a call open a frame with one `Vec::resize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Handle {
    index: u32,
    generation: u32,
}

impl Handle {
    /// The absence of an object, for a reference word that names none.
    ///
    /// A real layout would more likely give a nullable reference a niche of
    /// its own; this is the smallest thing that lets a test build a cycle in
    /// two steps, which needs an object to exist before the handle to it does.
    pub(crate) const NONE: Handle = Handle {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// Whether this names no object.
    pub(crate) fn is_none(self) -> bool {
        self == Handle::NONE
    }

    /// The handle as the eight bytes a slot holds: the index low, the
    /// generation high.
    pub(crate) fn to_slot(self) -> Slot {
        (self.index as Slot) | ((self.generation as Slot) << 32)
    }

    /// The handle a slot's eight bytes are, read because the layout says the
    /// slot is a reference and for no other reason.
    pub(crate) fn from_slot(bits: Slot) -> Handle {
        Handle {
            index: bits as u32,
            generation: (bits >> 32) as u32,
        }
    }

    fn index(self) -> usize {
        self.index as usize
    }
}

/// Which layout an object has, as the index of one in [`HandleHeap`]'s table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayoutId(u32);

/// What one word of an object means at decision 5's boundary.
///
/// This is decision 2's **payload layout**, at the width the slice needs. It
/// is not a tag: no object carries one of these, the layout does, and the
/// layout is reached through the object's [`LayoutId`] — which is decision 4's
/// rule that "reflection reads metadata, never bits" applied to the boundary
/// rather than to a `typeOf`. The bits are read as an `Int` because the layout
/// says the word is an `Int`, and for no other reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// The full signed sixty-four bits.
    Int,
    /// The full IEEE-754 bit pattern, every pattern including every NaN.
    Float,
    /// Canonical 0 or 1.
    Bool,
    /// A [`Handle`], whose object is materialised in turn. This is the only
    /// `Part` the reference map names.
    Nested,
}

impl Part {
    /// Whether a word of this part is a reference the collector follows.
    fn is_reference(self) -> bool {
        matches!(self, Part::Nested)
    }
}

/// What a host is handed when an object of this layout crosses decision 5's
/// boundary.
///
/// One variant per Cove value kind the slice materialises, and
/// [`Shape::Opaque`] for the layouts that never cross — which is most of the
/// heap, and is what an object the VM keeps for itself looks like.
///
/// Nothing about a shape is public and nothing about it reaches a host. What
/// reaches a host is a [`Value`], and decision 5's phrasing is why the two are
/// listed separately at all: the promise `Value` keeps is that "each part a
/// reader answers with is stored as the thing it answers with", and a `Shape`
/// is the VM-side description that a materialisation *satisfies*, not a second
/// place the promise lives.
#[derive(Clone, Debug)]
pub(crate) enum Shape {
    /// Not a boundary type. An object of this layout is the VM's own and
    /// materialising one is a programming error.
    Opaque,
    /// One word, materialised as the [`Part`] names.
    ///
    /// Decision 1 puts an unboxed `Int` in a *slot*, so this is what a boxed
    /// or erased scalar looks like rather than what an ordinary one does. It
    /// is here because it is the smallest thing the boundary can be asked for,
    /// and a boundary that cannot do the smallest thing is not one.
    Scalar(Part),
    /// A declared struct: one named word per field, in declaration order.
    Struct {
        type_name: &'static str,
        fields: Vec<(&'static str, Part)>,
    },
    /// A declared array: no fixed words at all, and one tail word per
    /// element.
    ///
    /// This is decision 2's variable-length tail at its smallest — the whole
    /// payload is the tail, and how long it is is the *object's* business and
    /// not the layout's. `element` is what every tail word is, and one answer
    /// for the whole run is what lets a reference map describe a length
    /// nobody knows until the allocation: the map says "the tail is handles"
    /// or "the tail is scalars" once, and it is as true of a tail of nought
    /// as of a tail of a thousand.
    Array { element: Part },
    /// A declared string: one fixed word holding the length in bytes, and a
    /// tail of words packing eight UTF-8 bytes each.
    ///
    /// Here for the two cases [`Shape::Array`] does not reach — a tail whose
    /// word count is not its element count, and a fixed part that is
    /// load-bearing rather than empty. The packing is a layout question and
    /// the reference map is indifferent to it: what the map has to say about
    /// this tail is that it is scalar, and it would say the same if the
    /// layout spent a whole word on every byte.
    ///
    /// The fixed word is [`Part::Int`] because the only thing anything asks
    /// of it is that it is not a handle. Nothing materialises it on its own:
    /// a byte count is not a Cove value.
    Str,
    /// One case of a declared enum, with its payload words in the order the
    /// case declares them.
    ///
    /// The case is in the layout rather than in a word, which is decision 2's
    /// "a heap object with the case in its header": here the header *is* the
    /// [`LayoutId`], so a two-case enum is two layouts. Decision 2 permits
    /// that for the prototype and forbids making it the default without
    /// measurement, and no measurement is taken here.
    Enum {
        type_name: &'static str,
        case: &'static str,
        payload: Vec<Part>,
    },
}

impl Shape {
    /// The parts of this shape's **fixed** part, in word order.
    fn fixed_parts(&self) -> Vec<Part> {
        match self {
            Shape::Opaque | Shape::Array { .. } => Vec::new(),
            Shape::Scalar(part) => vec![*part],
            Shape::Str => vec![Part::Int],
            Shape::Struct { fields, .. } => fields.iter().map(|(_, part)| *part).collect(),
            Shape::Enum { payload, .. } => payload.clone(),
        }
    }

    /// What every word of this shape's tail is, where it has one.
    fn tail(&self) -> Option<Part> {
        match self {
            Shape::Array { element } => Some(*element),
            Shape::Str => Some(Part::Int),
            Shape::Opaque | Shape::Scalar(_) | Shape::Struct { .. } | Shape::Enum { .. } => None,
        }
    }
}

/// What the VM owns about one shape of heap object.
///
/// ADR 0028 decision 2 names five things a header or the VM's metadata must
/// carry. Four of them are here — a layout id, the object's size, its
/// reference map, and its payload layout including the variable-length tail
/// where it has one — and the fifth is deliberately absent: there is no
/// movement guarantee, because ADR 0011 and the Language Card make collection
/// non-moving and decision 2 records that as an allocator invariant rather
/// than a mandatory word in every header. [`HandleHeap`] never moves an
/// object.
///
/// The payload layout is here as [`Layout::shape`], because decision 5's
/// boundary needs one: reading a word as an `Int` rather than as a `Float` or
/// a handle is a question only the layout can answer. A layout built from a
/// [`Shape`] derives its reference map from that payload layout rather than
/// being told it twice, which is decision 2's required invariant — "the
/// lowered layout completely determines how to find every reference; runtime
/// code must not guess" — made true by there being nothing else to consult.
///
/// # What a layout says about a tail, and what it cannot
///
/// A layout is one description shared by every object that has it, and a tail
/// is the one part of an object whose *size* the layout does not know: how
/// many words follow the fixed part is settled by the allocation and recorded
/// in the object's own header, as [`Object::tail`]. So the layout says two
/// things and the object says the third:
///
/// - `words` — how many fixed words come first, and [`Layout::refs`] which of
///   *those* are handles, word by word;
/// - `tail` — what every word after them is, as one [`Part`] for the whole
///   run, or `None` for a layout with no tail at all;
/// - and the object's own `tail` count is how far that run goes.
///
/// The reference map is therefore two rules rather than a bitmap: an explicit
/// set for the fixed part, and one bit for the tail. That is not an economy,
/// it is what a variable length forces — a per-word map for the tail could not
/// be written down before the allocation that decides how many words there
/// are, so the only map that can describe a tail is one that says the same
/// thing about all of it.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    name: Arc<str>,
    words: usize,
    /// Which of the object's **fixed** words are handles.
    ///
    /// This is half the reference map, and within the fixed part it is the
    /// only thing that decides. A word this does not name is scalar bits and
    /// is never read as a handle, however much it may look like one — which is
    /// decision 1's invariant that "a slot the layout calls scalar must never
    /// be reachable by a walk that treats it as a reference", stated for an
    /// object's interior rather than for a frame.
    refs: Vec<usize>,
    /// What each word of the variable-length tail is, or `None` where the
    /// layout has no tail.
    ///
    /// The other half of the reference map, and it is one answer rather than a
    /// set because a tail's length is not known until the object exists.
    /// `Some(Part::Nested)` is a run of handles the collector walks;
    /// `Some(`anything else`)` is a run of scalars it must not.
    tail: Option<Part>,
    /// What an object of this layout materialises as, and what each of its
    /// words means on the way.
    shape: Shape,
}

impl Layout {
    /// A layout of `words` eight-byte words, of which those at `refs` are
    /// handles, and which never crosses decision 5's boundary.
    pub(crate) fn new(name: impl Into<Arc<str>>, words: usize, refs: Vec<usize>) -> Layout {
        Layout::opaque(name, words, refs, None)
    }

    /// The same, with a variable-length tail every word of which is `tail`.
    ///
    /// An object of such a layout is `words` fixed words and then as many tail
    /// words as its allocation asked for, which may be none.
    pub(crate) fn with_tail(
        name: impl Into<Arc<str>>,
        words: usize,
        refs: Vec<usize>,
        tail: Part,
    ) -> Layout {
        Layout::opaque(name, words, refs, Some(tail))
    }

    fn opaque(
        name: impl Into<Arc<str>>,
        words: usize,
        refs: Vec<usize>,
        tail: Option<Part>,
    ) -> Layout {
        let name = name.into();
        for &at in &refs {
            assert!(
                at < words,
                "{name}'s reference map names word {at} of {words}"
            );
        }
        Layout {
            name,
            words,
            refs,
            tail,
            shape: Shape::Opaque,
        }
    }

    /// A layout whose objects a host may be handed: the fixed width its
    /// `shape` implies, the tail it implies, and the reference map that shape
    /// derives for both.
    ///
    /// There is no second argument for the reference map, and that is the
    /// point. Decision 2 requires that "the lowered layout completely
    /// determines how to find every reference"; here it does so because there
    /// is nothing else to consult and no way to say something different. The
    /// tail is the case where that matters most: nothing can be told that a
    /// tail is scalar and then be handed a tail of handles, because the same
    /// [`Shape`] answers both questions.
    pub(crate) fn boundary(name: impl Into<Arc<str>>, shape: Shape) -> Layout {
        let name = name.into();
        let parts = shape.fixed_parts();
        let refs = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.is_reference())
            .map(|(at, _)| at)
            .collect();
        Layout {
            name,
            words: parts.len(),
            refs,
            tail: shape.tail(),
            shape,
        }
    }

    /// How many eight-byte words the object's **fixed** part holds. An
    /// object's tail stands after these, and how long it is is the object's
    /// own header and not this.
    pub(crate) fn words(&self) -> usize {
        self.words
    }

    /// What every word of the tail is, or `None` for a layout without one.
    pub(crate) fn tail(&self) -> Option<Part> {
        self.tail
    }

    /// Whether the fixed word at `at` is a handle, straight off the reference
    /// map.
    pub(crate) fn is_reference(&self, at: usize) -> bool {
        self.refs.contains(&at)
    }

    /// The layout's name, for a panic message.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// What an object of this layout materialises as.
    pub(crate) fn shape(&self) -> &Shape {
        &self.shape
    }
}

/// One entry of the object table: an object, whether it is live, and which
/// generation of the entry it is.
///
/// A swept entry keeps its `Object` rather than dropping it, and the next
/// allocation to take the entry reuses that object's word buffer. That is why
/// a steady-state loop allocates nothing: the buffer is the only heap
/// allocation an object has, and the free list hands it back. What a reader
/// must not conclude from the object still standing there is that it is
/// reachable — `live` is what says so, and the mark phase never looks inside
/// a dead entry.
#[derive(Clone, Debug)]
struct Entry {
    /// Which generation of this entry the object is. Starts at 1 and moves on
    /// at every reuse, so a handle to a swept object never becomes a handle to
    /// its successor.
    generation: u32,
    /// Whether the object is reachable-as-of-the-last-sweep, which is what
    /// "exists" means in a traced heap.
    live: bool,
    object: Object,
}

/// One VM-owned heap object: a layout, how long its tail is, and its words.
#[derive(Clone, Debug)]
struct Object {
    layout: LayoutId,
    /// How many of `words` are the tail — decision 2's "and then N more".
    ///
    /// This is the one piece of an object's layout the [`Layout`] cannot
    /// carry, because it is settled by the allocation rather than by the
    /// lowering, and it is why a header exists at all rather than every word
    /// of the description living in the layout table. Always zero for a layout
    /// whose `tail` is `None`.
    tail: usize,
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
    /// One entry per handle index the heap has ever issued.
    ///
    /// An entry the sweep took is not removed: it is marked dead, its index
    /// goes on [`HandleHeap::free`], and its generation moves on when the
    /// index is handed out again. A stale handle therefore still names a dead
    /// object rather than a live one, which is what makes the negative tests
    /// observe the failure instead of a coincidence, and it keeps naming one
    /// **after the index is reused** — which a bare index could not do and is
    /// the reason [`Handle`] carries a generation.
    objects: Vec<Entry>,
    /// The indices of the entries the sweep took, waiting to be handed out
    /// again. Reusing an index is what keeps the object table the size of the
    /// live set rather than the size of everything the run ever allocated,
    /// and a sweep is a walk of that table.
    free: Vec<u32>,
    /// Where [`HandleHeap::copy_replacing`] reads an object's words, kept
    /// between calls so that writing a struct field allocates nothing.
    scratch: Vec<Slot>,
    /// The mark phase's set and worklist, kept between collections for the
    /// same reason: a collection that allocates is a collection whose cost is
    /// partly the allocator's.
    marked: HashSet<u32>,
    work: Vec<Handle>,
    allocations_since_collection: u64,
    next_collection_at: u64,
    collections: u64,
    /// Whether every safepoint collects, whatever the pacing says.
    ///
    /// The standard way to make a rooting bug deterministic instead of lucky,
    /// and this slice needs it for a reason the rest of the module did not: a
    /// materialisation reaches a safepoint per part, and which part the heap
    /// happens to be due at is an accident of what the program allocated
    /// before the boundary. With this on, *every* part is read after a real
    /// collection, so a test that survives has survived all of them.
    stress: bool,
}

impl HandleHeap {
    /// An empty heap.
    pub(crate) fn new() -> HandleHeap {
        HandleHeap {
            layouts: Vec::new(),
            objects: Vec::new(),
            free: Vec::new(),
            scratch: Vec::new(),
            marked: HashSet::new(),
            work: Vec::new(),
            allocations_since_collection: 0,
            next_collection_at: MIN_ALLOCATIONS_BETWEEN_COLLECTIONS,
            collections: 0,
            stress: false,
        }
    }

    /// Turns collection at every safepoint on or off.
    pub(crate) fn stress(&mut self, on: bool) {
        self.stress = on;
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
    /// Where the layout has a tail, everything past its fixed part is the
    /// tail, and how many words that is goes in the object's header. This is
    /// the only place a tail's length is decided, which is decision 2's point:
    /// it is not known until here.
    ///
    /// # Panics
    ///
    /// If `words` is not the width the layout declares — exactly, for a layout
    /// with no tail; at least its fixed part, for a layout with one. An object
    /// whose size disagrees with its layout is one whose reference map cannot
    /// be trusted, and ADR 0028 decision 2's required invariant is that the
    /// layout completely determines how to find every reference.
    pub(crate) fn allocate(&mut self, layout: LayoutId, words: Vec<Slot>) -> Handle {
        self.allocate_from(layout, &words)
    }

    /// The same, from a slice the caller already has — which is what a frame
    /// hands over, because the words an object is built from are the operands
    /// standing on the one stack.
    ///
    /// This is the allocation that does **no** heap allocation once the free
    /// list is warm: a reused entry keeps the word buffer the previous object
    /// had, and this refills it. `crates/cove-runtime/tests/frame_allocation.rs`
    /// is what says so with a global allocator rather than with this sentence.
    pub(crate) fn allocate_from(&mut self, layout: LayoutId, words: &[Slot]) -> Handle {
        let declared = self.layout(layout);
        let tail = if declared.tail.is_some() {
            assert!(
                words.len() >= declared.words,
                "{} declares {} words before its tail",
                declared.name,
                declared.words
            );
            words.len() - declared.words
        } else {
            assert_eq!(
                words.len(),
                declared.words,
                "{} declares {} words",
                declared.name,
                declared.words
            );
            0
        };
        self.allocations_since_collection += 1;
        match self.free.pop() {
            Some(index) => {
                let entry = &mut self.objects[index as usize];
                // Generation zero is never issued, so eight zero bytes are
                // never a live handle. See [`Handle`].
                entry.generation = match entry.generation.wrapping_add(1) {
                    0 => 1,
                    next => next,
                };
                entry.live = true;
                entry.object.layout = layout;
                entry.object.tail = tail;
                entry.object.words.clear();
                entry.object.words.extend_from_slice(words);
                Handle {
                    index,
                    generation: entry.generation,
                }
            }
            None => {
                self.objects.push(Entry {
                    generation: 1,
                    live: true,
                    object: Object {
                        layout,
                        tail,
                        words: words.to_vec(),
                    },
                });
                Handle {
                    index: self.objects.len() as u32 - 1,
                    generation: 1,
                }
            }
        }
    }

    /// Whether the object `handle` names still exists.
    ///
    /// Three ways to answer no, and the third is the one the generation is
    /// here for: no such index, an entry the sweep took, or an entry that has
    /// been handed out again since this handle was made.
    pub(crate) fn is_live(&self, handle: Handle) -> bool {
        !handle.is_none()
            && self
                .objects
                .get(handle.index())
                .is_some_and(|entry| entry.live && entry.generation == handle.generation)
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

    /// Whether word `at` of the object `handle` names is a reference.
    ///
    /// **The reference map, and nothing else, answers this.** It is asked by a
    /// field read, which has to say what kind of word it just pushed onto a
    /// frame — decision 2's "which of its words are handles, so a collector
    /// scans those and not the scalars beside them", asked one word at a time
    /// rather than once per collection.
    pub(crate) fn word_is_reference(&self, handle: Handle, at: usize) -> bool {
        let object = self.object(handle);
        let layout = &self.layouts[object.layout.0 as usize];
        if at < layout.words {
            layout.refs.contains(&at)
        } else {
            layout.tail.is_some_and(Part::is_reference)
        }
    }

    /// Allocates a copy of the object `source` names with word `at` replaced.
    ///
    /// This is what writing a field of a Cove struct is, and the copy is not
    /// caution. A struct is a value: `Vm` reaches the same point holding an
    /// `Rc` and calls `Rc::make_mut`, which copies when another holder exists
    /// and mutates in place when none does. A traced heap keeps no count, so
    /// it cannot tell those apart — and always copying is the answer that is
    /// right in both cases, because the copy is what a value type's assignment
    /// means and the original is left for the collector.
    ///
    /// The scratch buffer is why this allocates nothing: the words are read
    /// into it, the free list hands back an entry with a buffer of its own,
    /// and neither is a `Vec` this had to make.
    pub(crate) fn copy_replacing(&mut self, source: Handle, at: usize, bits: Slot) -> Handle {
        let mut scratch = std::mem::take(&mut self.scratch);
        {
            let object = self.object(source);
            scratch.clear();
            scratch.extend_from_slice(&object.words);
        }
        scratch[at] = bits;
        let layout = self.object(source).layout;
        let handle = self.allocate_from(layout, &scratch);
        self.scratch = scratch;
        handle
    }

    /// Writes the word at `at` of the object `handle` names.
    pub(crate) fn set_word(&mut self, handle: Handle, at: usize, bits: Slot) {
        let entry = self
            .objects
            .get_mut(handle.index())
            .filter(|entry| entry.live && entry.generation == handle.generation)
            .unwrap_or_else(|| panic!("handle {handle:?} names a swept object"));
        entry.object.words[at] = bits;
    }

    /// How many objects the heap holds.
    pub(crate) fn live_objects(&self) -> u64 {
        self.objects.iter().filter(|entry| entry.live).count() as u64
    }

    /// How many collections this heap has run.
    pub(crate) fn collections(&self) -> u64 {
        self.collections
    }

    /// Whether enough has been allocated since the last collection to be
    /// worth another one — [`crate::heap::Heap::should_collect`]'s rule.
    pub(crate) fn should_collect(&self) -> bool {
        self.stress || self.allocations_since_collection >= self.next_collection_at
    }

    /// What an object of `handle`'s layout materialises as.
    ///
    /// # Panics
    ///
    /// If the object has been swept — which is the whole of the negative
    /// proof: a materialiser that lost its root asks this question first.
    pub(crate) fn shape_of(&self, handle: Handle) -> &Shape {
        let object = self.object(handle);
        self.layouts[object.layout.0 as usize].shape()
    }

    /// The word indices of the tail of the object `handle` names: decision 2's
    /// "and then N more", which is the object's header and not its layout.
    ///
    /// Empty for an object of a layout with no tail, and empty for one whose
    /// allocation asked for no tail words — which are different facts about
    /// the heap and the same answer here, because nothing that walks a tail
    /// has any reason to tell them apart.
    pub(crate) fn tail_range(&self, handle: Handle) -> std::ops::Range<usize> {
        let object = self.object(handle);
        let fixed = self.layouts[object.layout.0 as usize].words;
        fixed..fixed + object.tail
    }

    /// Whether the tail of the object `handle` names is a run of handles.
    ///
    /// The reference map's answer about the tail, and the only thing that
    /// decides whether a tail word is followed.
    pub(crate) fn tail_is_reference(&self, handle: Handle) -> bool {
        let object = self.object(handle);
        self.layouts[object.layout.0 as usize]
            .tail
            .is_some_and(Part::is_reference)
    }

    /// How many words the object `handle` names holds, fixed part and tail
    /// together.
    pub(crate) fn layout_words(&self, handle: Handle) -> usize {
        self.object(handle).words.len()
    }

    /// The name of `handle`'s layout, for a panic message.
    pub(crate) fn layout_name(&self, handle: Handle) -> &str {
        let object = self.object(handle);
        self.layouts[object.layout.0 as usize].name()
    }

    fn object(&self, handle: Handle) -> &Object {
        self.objects
            .get(handle.index())
            .filter(|entry| entry.live && entry.generation == handle.generation)
            .map(|entry| &entry.object)
            .unwrap_or_else(|| panic!("handle {handle:?} names a swept object"))
    }

    /// Marks from `roots` and sweeps what is not marked.
    pub(crate) fn collect(&mut self, roots: &dyn HandleRoots) -> HandleCollection {
        // Taken out and put back rather than made here. A collection that
        // allocates is a collection whose cost is the allocator's, and this
        // heap now runs inside a benchmark: `frame_allocation.rs` counts the
        // allocator's calls over ten thousand extra field writes and expects
        // the difference to be zero, which two fresh containers per collection
        // would spend.
        let mut marked = std::mem::take(&mut self.marked);
        let mut work = std::mem::take(&mut self.work);
        marked.clear();
        work.clear();
        let mut roots_yielded = 0u64;

        roots.walk(&mut |handle| {
            roots_yielded += 1;
            if self.is_live(handle) && marked.insert(handle.index) {
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
            let layout = &self.layouts[object.layout.0 as usize];
            live_bytes += (object.words.len() * std::mem::size_of::<Slot>()) as u64;
            // The tail's share of the reference map: one bit for the whole
            // run, and the object's own header for how far it goes. A tail of
            // scalars contributes no indices at all, which is the same
            // statement as a fixed word the map does not name.
            let tail = if layout.tail.is_some_and(Part::is_reference) {
                self.tail_range(handle)
            } else {
                0..0
            };
            // The reference map, and nothing else, decides which words are
            // followed. A word it does not name is scalar bits, whatever
            // those bits happen to look like.
            for at in layout.refs.iter().copied().chain(tail) {
                let child = Handle::from_slot(object.words[at]);
                if self.is_live(child) && marked.insert(child.index) {
                    work.push(child);
                }
            }
        }

        let mut freed_objects = 0u64;
        for (at, entry) in self.objects.iter_mut().enumerate() {
            if entry.live && !marked.contains(&(at as u32)) {
                entry.live = false;
                self.free.push(at as u32);
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
        self.marked = marked;
        self.work = work;
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
    /// Every collection this machine has run, in order.
    ///
    /// A collection that happens *inside* [`Machine::materialise`] is
    /// otherwise unobservable from outside it: the materialiser answers a
    /// [`Value`] and says nothing about what the heap did on the way. This is
    /// how a test pins the root depth at the moment a collection ran, which is
    /// the only way to tell a materialisation that was rooted throughout from
    /// one that was merely lucky.
    collections: Vec<HandleCollection>,
}

impl Machine {
    /// A machine with an empty heap, frame and shadow stack.
    pub(crate) fn new() -> Machine {
        Machine {
            heap: HandleHeap::new(),
            frame: Frame::new(),
            temps: TempRoots::new(),
            collections: Vec::new(),
        }
    }

    /// Every collection this machine has run, in order.
    pub(crate) fn collections(&self) -> &[HandleCollection] {
        &self.collections
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
        Some(self.collect_now())
    }

    /// Collects now, whatever the heap's pacing says — for a test that wants
    /// one collection at a known point rather than a safepoint's decision.
    pub(crate) fn collect_now(&mut self) -> HandleCollection {
        let roots = Safepoint {
            frame: &self.frame,
            temps: &self.temps,
        };
        let collection = self.heap.collect(&roots);
        self.collections.push(collection);
        collection
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

    /// Runs `body` with every one of `handles` registered as a temporary root.
    ///
    /// [`Machine::with_root`] for the case a boundary crossing actually has.
    /// `Inst::CallHost` takes *all* of a call's arguments off the operand
    /// stack before the call is charged, and they are siblings rather than a
    /// chain, so no one of them roots the others. Truncate-to-depth is what
    /// makes one scope cover any number of them without a second mechanism.
    pub(crate) fn with_roots<R>(
        &mut self,
        handles: &[Handle],
        body: impl FnOnce(&mut Machine) -> R,
    ) -> R {
        let depth = self.temps.depth();
        for &handle in handles {
            self.temps.push(handle);
        }
        let answer = body(self);
        self.temps.truncate(depth);
        answer
    }

    /// How many handles the shadow stack holds, so a test can check that
    /// [`Machine::with_root`] leaves nothing behind.
    pub(crate) fn rooted(&self) -> usize {
        self.temps.depth()
    }

    // ------------------------------------------------- decision 5's boundary

    /// Materialises the object `handle` names as the [`Value`] a host is
    /// handed.
    ///
    /// **This is [`Machine::with_root`]'s first real caller**, and the reason
    /// it is the first is that it is the first thing that has to hold a handle
    /// across work that can collect. `handle` is a Rust local for the whole of
    /// the materialisation — nothing in the frame's reference map names it,
    /// because a boundary is reached with the value already taken off the
    /// stack — and reading a part is VM work, and VM work reaches safepoints.
    ///
    /// What comes back is an owned `Value` in decision 5's sense: "not a
    /// window onto a slot, a heap object or a dynamic value" but "a separate
    /// object with a representation of its own, whose parts are stored as the
    /// things the readers answer with". It shares no storage with
    /// [`HandleHeap`]. Once it exists, the object it was made from may be
    /// swept without the `Value` noticing, and
    /// `a_materialised_value_outlives_the_object_it_was_made_from` is that,
    /// asserted.
    ///
    /// # Panics
    ///
    /// If `handle` names a swept object, which is what forgetting the root
    /// costs; or if its layout is [`Shape::Opaque`], which is a VM object
    /// nobody outside the VM has any business being handed.
    pub(crate) fn materialise(&mut self, handle: Handle) -> Value {
        self.with_root(handle, |machine| machine.materialise_rooted(handle))
    }

    /// Decision 5's "Host calls — arguments out": the handles standing in
    /// `count` frame slots from `at`, taken off the stack the way `Vm::take`
    /// takes them, and materialised.
    ///
    /// Every argument is rooted for the whole of the crossing rather than one
    /// at a time, and that is not caution. Materialising argument *i* reaches
    /// safepoints, and arguments *i+1* onwards are in a Rust local vector by
    /// then and in no slot the reference map describes;
    /// `rooting_one_argument_at_a_time_sweeps_the_others` is what happens if
    /// the scope is drawn round each in turn instead.
    pub(crate) fn materialise_args(&mut self, at: usize, count: usize) -> Vec<Value> {
        let handles: Vec<Handle> = (at..at + count)
            .map(|slot| self.frame.take_handle(slot))
            .collect();
        self.with_roots(&handles, |machine| {
            handles
                .iter()
                .map(|&handle| machine.materialise_rooted(handle))
                .collect()
        })
    }

    /// The same crossing, where the arguments arrive as the tail of one heap
    /// array rather than as a run of frame slots.
    ///
    /// This is [`Machine::materialise_args`] at a scale the frame does not
    /// choose: a spread call's argument list is as long as the array is, and
    /// the array is a value the program built. Decision 5 lists "the arguments
    /// a host passes to a Cove closure" beside "Host calls — arguments out",
    /// and both are this shape once an argument list can be a heap object.
    ///
    /// **The array is consumed and is not rooted, and that is the whole of
    /// what this proves.** The tail's handles are read out into a Rust local
    /// vector in one go, the way `Vm::take` drains a call's arguments off the
    /// operand stack — and from that instant the array itself is what #192
    /// kept `Vm::arg_vectors` out of the root set for: a container the
    /// crossing has finished with, whose elements are now siblings in a Rust
    /// local with nothing but the shadow stack holding them. The array is
    /// swept at the first safepoint inside the crossing and every element
    /// survives it, which is only true because each element is a root of its
    /// own. `rooting_one_tail_element_at_a_time_sweeps_the_siblings` is what
    /// it costs to draw the scope round each in turn instead.
    ///
    /// # Panics
    ///
    /// If the layout's tail is not a run of handles. Reading a scalar tail as
    /// references is the one thing decision 1's invariant forbids, stated for
    /// a tail.
    pub(crate) fn materialise_tail_args(&mut self, source: Handle) -> Vec<Value> {
        let handles = self.tail_handles(source);
        self.with_roots(&handles, |machine| {
            handles
                .iter()
                .map(|&handle| machine.materialise_rooted(handle))
                .collect()
        })
    }

    /// The handles standing in the tail of the object `source` names.
    ///
    /// No safepoint: this is one read, the way `Vm::take` is one drain, and
    /// what happens after it is the caller's rooting problem rather than
    /// this one's.
    fn tail_handles(&self, source: Handle) -> Vec<Handle> {
        assert!(
            self.heap.tail_is_reference(source),
            "{}'s tail is scalar and holds no references",
            self.heap.layout_name(source)
        );
        self.heap
            .tail_range(source)
            .map(|at| Handle::from_slot(self.heap.word(source, at)))
            .collect()
    }

    /// The body of [`Machine::materialise`], which assumes `handle` is already
    /// a root.
    ///
    /// Separate from the rooting so that the negative test can run *this*
    /// program rather than a paraphrase of it: the only difference between
    /// `the_source_survives_a_collection_in_the_middle_of_materialising_it`
    /// and `the_source_is_swept_mid_materialisation_without_the_root` is the
    /// [`Machine::with_root`] one of them goes through.
    fn materialise_rooted(&mut self, handle: Handle) -> Value {
        // Cloning the shape rather than borrowing it is a prototype artefact:
        // reading a part needs `&mut self` for the safepoint, and the layout
        // table lives in the heap. A real boundary would hold the layout table
        // apart from the object table, which is what decision 2's "VM-owned
        // metadata" already suggests, and borrow it for the whole crossing.
        let shape = self.heap.shape_of(handle).clone();
        match shape {
            Shape::Opaque => panic!(
                "{} is the VM's own object and does not cross the boundary",
                self.heap.layout_name(handle)
            ),
            Shape::Scalar(part) => self.part(handle, 0, part),
            Shape::Struct { type_name, fields } => {
                let mut materialised = Vec::with_capacity(fields.len());
                for (at, (name, part)) in fields.into_iter().enumerate() {
                    materialised.push((name, self.part(handle, at, part)));
                }
                Value::structure(type_name, materialised)
            }
            Shape::Enum {
                type_name,
                case,
                payload,
            } => {
                let mut materialised = Vec::with_capacity(payload.len());
                for (at, part) in payload.into_iter().enumerate() {
                    materialised.push(self.part(handle, at, part));
                }
                Value::enumeration(type_name, case, materialised)
            }
            // The tail, and it is read exactly as the fixed part is: one word
            // at a time, as the [`Part`] the layout gives the whole run, with
            // a safepoint before each. How many there are is the object's
            // header rather than the layout's — the one number that arrives at
            // the boundary from the allocation instead of from the lowering.
            Shape::Array { element } => {
                let tail = self.heap.tail_range(handle);
                let mut items = Vec::with_capacity(tail.len());
                for at in tail {
                    items.push(self.part(handle, at, element));
                }
                Value::array(items)
            }
            // A tail whose words are not one element each. The layout's fixed
            // word says how many bytes of the last word are the string's, and
            // reading it is the same act as reading any other word: the
            // boundary knows it is a length because the layout says so.
            Shape::Str => {
                let length = self.word(handle, 0) as usize;
                let tail = self.heap.tail_range(handle);
                let mut bytes = Vec::with_capacity(tail.len() * size_of::<Slot>());
                for at in tail {
                    bytes.extend_from_slice(&self.word(handle, at).to_le_bytes());
                }
                assert!(
                    length <= bytes.len(),
                    "a String's length word runs past its tail"
                );
                bytes.truncate(length);
                let text = String::from_utf8(bytes).expect("a String object's tail is UTF-8");
                Value::string(text)
            }
        }
    }

    /// Materialises word `at` of the object `source` names, reading it as
    /// `part` says to.
    ///
    /// The safepoint is the point of the whole exercise. `Vm` charges fuel and
    /// calls `Vm::collect_if_due` at `Inst::CallHost` with the arguments
    /// already drained into a Rust local; this is that, once per part, because
    /// a boundary is not one instruction but a stretch of VM work whose length
    /// is the size of what crosses. Whether a collection actually happens
    /// there is the heap's decision and not the boundary's, which is why
    /// [`HandleHeap::stress`] exists: so that a test does not have to hope.
    ///
    /// Nothing is read out of the bits that the layout did not already say.
    /// That is decision 4's rule at the boundary — the type comes from the
    /// metadata, never from the value.
    fn part(&mut self, source: Handle, at: usize, part: Part) -> Value {
        let bits = self.word(source, at);
        match part {
            Part::Int => Value::int(bits as i64),
            Part::Float => Value::float(f64::from_bits(bits)),
            Part::Bool => Value::bool(bits != 0),
            // The handover, recursively. The child is a Rust local of this
            // frame from here until its own `Value` exists, so it gets a root
            // of its own. It is *also* reachable from `source`, which is
            // rooted, so that root is redundant for liveness in this shape —
            // and it is pushed anyway, because "something else happens to
            // reach it" is precisely the global argument a shadow root exists
            // to replace with a local one.
            Part::Nested => self.materialise(Handle::from_slot(bits)),
        }
    }

    /// Reads one word of the object `source` names, at a safepoint.
    ///
    /// The safepoint is the point of the whole exercise, and it is per *word*
    /// rather than per crossing for the reason above: a boundary is a stretch
    /// of VM work whose length is the size of what crosses, and a tail is what
    /// makes that length something the program chose. An array of a thousand
    /// handles is a thousand safepoints inside one crossing.
    fn word(&mut self, source: Handle, at: usize) -> Slot {
        // Deliberately discarded: what the collection did is on
        // `Machine::collections` for a test to read, and the materialiser has
        // no decision to make about it.
        let _ = self.safepoint();
        self.heap.word(source, at)
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

    /// The smallest thing the boundary can be asked for: one word, read as a
    /// full signed sixty-four bits.
    fn boxed_int(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary("test.BoxedInt", Shape::Scalar(Part::Int)))
    }

    /// One word read three ways, so that a test can show the layout deciding
    /// and the bits not.
    fn three_ways(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary(
            "test.ThreeWays",
            Shape::Struct {
                type_name: "ThreeWays",
                fields: vec![("n", Part::Int), ("x", Part::Float), ("flag", Part::Bool)],
            },
        ))
    }

    /// A declared struct of two scalar fields: the smallest aggregate with a
    /// payload.
    fn point(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary(
            "test.Point",
            Shape::Struct {
                type_name: "Point",
                fields: vec![("x", Part::Int), ("y", Part::Int)],
            },
        ))
    }

    /// A declared struct of two *references*: the smallest nested aggregate,
    /// and the one materialising which holds more than one handle at once.
    fn segment(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary(
            "test.Segment",
            Shape::Struct {
                type_name: "Segment",
                fields: vec![("from", Part::Nested), ("to", Part::Nested)],
            },
        ))
    }

    /// One case of an enum, with the case in the layout — decision 2's "a heap
    /// object with the case in its header", where the header is the layout id.
    fn case_of(heap: &mut HandleHeap, case: &'static str, payload: Vec<Part>) -> LayoutId {
        heap.register(Layout::boundary(
            "test.Option",
            Shape::Enum {
                type_name: "Option",
                case,
                payload,
            },
        ))
    }

    /// A `Point` at `x`, `y`, and the handle naming it.
    fn a_point(machine: &mut Machine, layout: LayoutId, x: i64, y: i64) -> Handle {
        machine.allocate(layout, vec![x as Slot, y as Slot])
    }

    /// An array whose tail is a run of handles, and which has no fixed part at
    /// all: the shape the reference map exists for.
    fn handle_array(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary(
            "test.Array",
            Shape::Array {
                element: Part::Nested,
            },
        ))
    }

    /// An array whose tail is a run of scalars, and which the collector must
    /// therefore not follow one word of.
    fn int_array(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary(
            "test.IntArray",
            Shape::Array { element: Part::Int },
        ))
    }

    /// A string: one fixed word of byte length, and a tail packing eight bytes
    /// a word.
    fn strings(heap: &mut HandleHeap) -> LayoutId {
        heap.register(Layout::boundary("test.Str", Shape::Str))
    }

    /// An array object whose tail names `elements`.
    fn an_array(machine: &mut Machine, layout: LayoutId, elements: &[Handle]) -> Handle {
        machine.allocate(
            layout,
            elements.iter().map(|handle| handle.to_slot()).collect(),
        )
    }

    /// A string object holding `text`, packed the way [`Shape::Str`] says.
    fn a_string(machine: &mut Machine, layout: LayoutId, text: &str) -> Handle {
        let bytes = text.as_bytes();
        let mut words = vec![bytes.len() as Slot];
        words.extend(bytes.chunks(size_of::<Slot>()).map(|chunk| {
            let mut word = [0u8; size_of::<Slot>()];
            word[..chunk.len()].copy_from_slice(chunk);
            Slot::from_le_bytes(word)
        }));
        machine.allocate(layout, words)
    }

    /// `count` `Point`s, which is what a tail of handles is filled with.
    fn some_points(machine: &mut Machine, layout: LayoutId, count: i64) -> Vec<Handle> {
        (0..count)
            .map(|n| a_point(machine, layout, n, n * 10))
            .collect()
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

    // ------------------------------------------- reuse, and what survives it

    /// A sweep returns an entry to the free list and the next allocation takes
    /// it, so a loop that allocates one object a turn does not grow the object
    /// table. That is what makes a sweep cost the live set rather than the
    /// history.
    #[test]
    fn a_swept_entry_is_handed_out_again_and_the_table_does_not_grow() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let kept = machine.allocate(layout, vec![1, Handle::NONE.to_slot()]);
        machine.frame.push_handle(kept);
        // One object a turn, none of them rooted, collected every turn.
        machine.heap.stress(true);
        for turn in 0..64 {
            machine.allocate(layout, vec![turn as Slot, Handle::NONE.to_slot()]);
            machine
                .safepoint()
                .expect("stress collects at every safepoint");
        }
        assert!(
            machine.heap.is_live(kept),
            "the rooted object is the one thing that survived every turn"
        );
        assert_eq!(
            machine.heap.live_objects(),
            1,
            "one object is live, and the other sixty-four turns' worth are not"
        );
        assert_eq!(
            machine.heap.objects.len(),
            2,
            "sixty-four allocations reused one entry: the table is the live set \
             and one free entry, not the history"
        );
    }

    /// The property the generation exists for, and the reason reuse did not
    /// make the negative tests vacuous.
    ///
    /// A handle to a swept object does not become a handle to whatever is
    /// allocated in its place. Without the generation, `stale` and `fresh`
    /// would be the same eight bytes and the use-after-free would read as a
    /// success.
    #[test]
    fn a_handle_to_a_swept_object_does_not_name_its_successor() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let stale = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        let freed = machine.collect_now();
        assert_eq!(freed.freed_objects, 1, "nothing rooted it: {freed:?}");

        let fresh = machine.allocate(layout, vec![9, Handle::NONE.to_slot()]);
        assert_eq!(
            Handle::from_slot(fresh.to_slot()),
            fresh,
            "a handle survives the round trip through the eight bytes a slot holds"
        );
        assert_ne!(
            stale.to_slot(),
            fresh.to_slot(),
            "the reused entry is a different handle, because its generation moved on"
        );
        assert!(machine.heap.is_live(fresh));
        assert!(
            !machine.heap.is_live(stale),
            "the stale handle names the swept object and not the one that took its entry"
        );
    }

    /// The mutation of the test above: reading through the stale handle is the
    /// use-after-free, and it is reported as one rather than answering the
    /// successor's word.
    #[test]
    #[should_panic(expected = "names a swept object")]
    fn reading_through_a_handle_to_a_swept_object_is_refused_after_reuse() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let stale = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        machine.collect_now();
        machine.allocate(layout, vec![9, Handle::NONE.to_slot()]);
        machine.heap.word(stale, 0);
    }

    /// Eight zero bytes are never a live handle, which is what lets a call open
    /// a frame with one `Vec::resize` and leave the reference words to be
    /// written by the body.
    #[test]
    fn a_zero_word_is_never_a_live_handle() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let first = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        assert_ne!(
            first.to_slot(),
            0,
            "generation zero is never issued, so the first handle is not the zero word"
        );
        assert!(
            !machine.heap.is_live(Handle::from_slot(0)),
            "a slot that has been given no object yet holds no object"
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

    // -------------------------------------------- decision 5's boundary

    /// The smallest crossing there is: one word, one [`Value`], and the full
    /// `Int` domain decision 1 promises a typed slot preserves surviving it.
    #[test]
    fn a_scalar_object_materialises_as_the_value_its_layout_names() {
        let mut machine = Machine::new();
        let layout = boxed_int(&mut machine.heap);
        for n in [0, 1, -1, i64::MAX, i64::MIN] {
            let handle = machine.allocate(layout, vec![n as Slot]);
            let value = machine.materialise(handle);
            assert_eq!(value.as_int(), Some(n), "materialising {n}");
        }
        assert_eq!(machine.rooted(), 0);
    }

    /// Decision 4 at the boundary: the type comes from the metadata and never
    /// from the value. One bit pattern in three words of one object, read as
    /// an `Int`, a `Float` and a `Bool` because that is what the layout says
    /// each word is.
    #[test]
    fn the_boundary_reads_the_layout_and_not_the_bits() {
        let mut machine = Machine::new();
        let layout = three_ways(&mut machine.heap);
        let bits = 2.0f64.to_bits();
        let handle = machine.allocate(layout, vec![bits, bits, bits]);

        let value = machine.materialise(handle);
        assert_eq!(value.declared_type(), Some("ThreeWays"));
        assert_eq!(
            value.field("n").and_then(Value::as_int),
            Some(4_611_686_018_427_387_904),
            "the same word as a full signed sixty-four bits"
        );
        assert_eq!(
            value.field("x").and_then(Value::as_float),
            Some(2.0),
            "and as an IEEE-754 double"
        );
        assert_eq!(value.field("flag").and_then(Value::as_bool), Some(true));
    }

    /// The promise the ADR names by name. #195's
    /// `payload() -> Option<&[Value]>` "is specifically a promise that a
    /// payload stays contiguous", and after decision 5 that promise binds the
    /// materialisation rather than the VM's object — which here is one word of
    /// one heap object, with the case in its layout and no `[Value]` anywhere.
    #[test]
    fn an_enum_payload_materialises_as_the_contiguous_slice_the_reader_promises() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let somes = case_of(&mut machine.heap, "Some", vec![Part::Nested]);
        let nones = case_of(&mut machine.heap, "None", Vec::new());
        assert_eq!(
            machine.heap.layout(nones).words(),
            0,
            "a case with no payload"
        );

        let inner = a_point(&mut machine, points, 1, 2);
        let some = machine.allocate(somes, vec![inner.to_slot()]);
        let value = machine.materialise(some);
        assert_eq!(value.case(), Some("Some"));
        let payload = value.payload().expect("an enum has a payload");
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0].field("y").and_then(Value::as_int), Some(2));

        let none = machine.allocate(nones, Vec::new());
        let value = machine.materialise(none);
        assert_eq!(value.case(), Some("None"));
        assert_eq!(value.payload().map(<[Value]>::len), Some(0));
    }

    /// A nested aggregate: two levels, four scalar leaves, and the reference
    /// map deciding which words are followed at both.
    #[test]
    fn a_nested_aggregate_materialises_through_the_reference_map() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let segments = segment(&mut machine.heap);
        let from = a_point(&mut machine, points, 1, 2);
        let to = a_point(&mut machine, points, 3, 4);
        let handle = machine.allocate(segments, vec![from.to_slot(), to.to_slot()]);

        let value = machine.materialise(handle);
        assert_eq!(value.declared_type(), Some("Segment"));
        let leaf = |field: &str, part: &str| {
            value
                .field(field)
                .and_then(|point| point.field(part))
                .and_then(Value::as_int)
        };
        assert_eq!((leaf("from", "x"), leaf("from", "y")), (Some(1), Some(2)));
        assert_eq!((leaf("to", "x"), leaf("to", "y")), (Some(3), Some(4)));
    }

    // ------------------------------- the boundary as `with_root`'s caller

    /// The positive direction. The source handle is out of the frame and in a
    /// Rust local for the whole crossing, a collection runs before *every*
    /// part is read, and the object and its contents are still there.
    #[test]
    fn the_source_survives_a_collection_in_the_middle_of_materialising_it() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let handle = a_point(&mut machine, points, 6, 7);
        let at = machine.frame.push_handle(handle);

        // Out of the slot and into a Rust local, which is how a boundary is
        // reached: the value has already come off the stack.
        let local = machine.frame.take_handle(at);
        machine.heap.stress(true);

        let value = machine.materialise(local);
        assert_eq!(value.field("x").and_then(Value::as_int), Some(6));
        assert_eq!(value.field("y").and_then(Value::as_int), Some(7));
        assert_eq!(
            machine.collections().len(),
            2,
            "one collection per part read: {:?}",
            machine.collections()
        );
        assert!(
            machine
                .collections()
                .iter()
                .all(|collection| collection.roots_yielded == 1
                    && collection.live_objects == 1
                    && collection.freed_objects == 0),
            "the shadow root, and nothing else, kept it: {:?}",
            machine.collections()
        );
        assert_eq!(machine.rooted(), 0, "the shadow stack was left as found");
    }

    /// The negative direction, and it is the same program: the only difference
    /// from the test above is that this one calls the materialiser's body
    /// without [`Machine::with_root`] round it. The first safepoint inside the
    /// crossing sweeps the object out from under the materialiser, and the
    /// next word read is the use-after-free.
    #[test]
    #[should_panic(expected = "names a swept object")]
    fn the_source_is_swept_mid_materialisation_without_the_root() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let handle = a_point(&mut machine, points, 6, 7);
        let at = machine.frame.push_handle(handle);

        let local = machine.frame.take_handle(at);
        machine.heap.stress(true);

        machine.materialise_rooted(local);
    }

    /// The same, without the stress mode: a collection the *heap* chose,
    /// landing inside a crossing because of what the program allocated before
    /// it. This is the shape `Inst::CallHost` actually has, where the pacing
    /// has nothing to do with the boundary.
    #[test]
    fn a_collection_the_heap_paced_lands_inside_a_materialisation() {
        let mut machine = Machine::new();
        let nodes = node(&mut machine.heap);
        let points = point(&mut machine.heap);
        let handle = a_point(&mut machine, points, 6, 7);
        let at = machine.frame.push_handle(handle);
        churn(
            &mut machine,
            nodes,
            MIN_ALLOCATIONS_BETWEEN_COLLECTIONS as usize,
        );

        let local = machine.frame.take_handle(at);
        assert!(
            machine.collections().is_empty(),
            "nothing has collected yet"
        );

        let value = machine.materialise(local);
        assert_eq!(value.field("x").and_then(Value::as_int), Some(6));
        assert_eq!(
            machine.collections().len(),
            1,
            "the boundary's own safepoint is where the heap came due: {:?}",
            machine.collections()
        );
        assert_eq!(
            machine.collections()[0].freed_objects,
            MIN_ALLOCATIONS_BETWEEN_COLLECTIONS,
            "the churn went and the source did not"
        );
        assert!(machine.heap.is_live(local));
    }

    /// Nesting, and it falls out of truncate-to-depth rather than needing a
    /// case of its own: the depth of the shadow stack at each collection is
    /// the depth of the materialisation that reached it.
    ///
    /// The sequence is the whole assertion. One root while the segment's own
    /// words are read, two while a point's are, and back to one between the
    /// two points — which is [`TempRoots`] unwinding to the depth each scope
    /// recorded, with nothing in [`Machine::materialise`] arranging it.
    #[test]
    fn nesting_roots_every_handle_the_crossing_holds_and_unwinds_to_depth() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let segments = segment(&mut machine.heap);
        let from = a_point(&mut machine, points, 1, 2);
        let to = a_point(&mut machine, points, 3, 4);
        let handle = machine.allocate(segments, vec![from.to_slot(), to.to_slot()]);

        machine.heap.stress(true);
        let value = machine.materialise(handle);

        let depths: Vec<u64> = machine
            .collections()
            .iter()
            .map(|collection| collection.roots_yielded)
            .collect();
        assert_eq!(
            depths,
            vec![1, 2, 2, 1, 2, 2],
            "the segment's two words at depth one, each point's two at depth two"
        );
        assert!(
            machine
                .collections()
                .iter()
                .all(|collection| collection.live_objects == 3),
            "all three survived every one of them: {:?}",
            machine.collections()
        );
        assert_eq!(machine.rooted(), 0);
        assert_eq!(
            value
                .field("to")
                .and_then(|point| point.field("y"))
                .and_then(Value::as_int),
            Some(4)
        );
    }

    /// Decision 8's three multiplicities, at the seam. A nested aggregate
    /// whose two fields name one object is **two root storage locations and
    /// one expansion** while it is being materialised: the crossing must not
    /// become a second path to something already yielded.
    #[test]
    fn a_materialisation_is_not_a_second_path_to_what_it_already_yielded() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let segments = segment(&mut machine.heap);
        let shared = a_point(&mut machine, points, 1, 2);
        let handle = machine.allocate(segments, vec![shared.to_slot(), shared.to_slot()]);

        machine.heap.stress(true);
        let value = machine.materialise(handle);

        for collection in machine.collections() {
            assert_eq!(
                collection.expansions, collection.live_objects,
                "an object is expanded once however many roots reach it: {collection:?}"
            );
            assert_eq!(collection.live_objects, 2, "{collection:?}");
        }
        let depths: Vec<u64> = machine
            .collections()
            .iter()
            .map(|collection| collection.roots_yielded)
            .collect();
        assert_eq!(
            depths,
            vec![1, 2, 2, 1, 2, 2],
            "the shared object is rooted twice over, and yielded once per location"
        );

        // And it is materialised twice, into two `Value`s: the shared identity
        // is the VM's, and a `Point` is a copy on the way out. Decision 7 is
        // where the values whose identity *is* observable are named, and none
        // of them is here.
        let leaf = |field: &str| {
            value
                .field(field)
                .and_then(|point| point.field("x"))
                .and_then(Value::as_int)
        };
        assert_eq!((leaf("from"), leaf("to")), (Some(1), Some(1)));
    }

    /// The handover, asserted. Once the crossing is done the `Value` owes the
    /// handle heap nothing: every object it was made from is swept and the
    /// `Value` reads exactly as before, because its parts are stored as the
    /// things the readers answer with and none of them is a [`Handle`].
    #[test]
    fn a_materialised_value_outlives_the_object_it_was_made_from() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let segments = segment(&mut machine.heap);
        let from = a_point(&mut machine, points, 1, 2);
        let to = a_point(&mut machine, points, 3, 4);
        let handle = machine.allocate(segments, vec![from.to_slot(), to.to_slot()]);
        machine.frame.push_handle(handle);

        let value = machine.materialise(handle);

        machine.frame.truncate(0);
        let swept = machine.collect_now();
        assert_eq!(swept.roots_yielded, 0, "nothing roots the source now");
        assert_eq!(swept.freed_objects, 3, "the segment and both points");
        assert!(!machine.heap.is_live(handle));

        assert_eq!(
            value
                .field("from")
                .and_then(|point| point.field("x"))
                .and_then(Value::as_int),
            Some(1),
            "the `Value` shares no storage with the heap it came from"
        );
    }

    /// Decision 5's "Host calls — arguments out", which is where more than one
    /// handle is in a Rust local at once without any of them rooting the
    /// others.
    #[test]
    fn every_argument_is_rooted_for_the_whole_crossing() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let first = a_point(&mut machine, points, 1, 2);
        let second = a_point(&mut machine, points, 3, 4);
        machine.frame.push_handle(first);
        machine.frame.push_handle(second);

        machine.heap.stress(true);
        let args = machine.materialise_args(0, 2);

        assert_eq!(args.len(), 2);
        assert_eq!(args[0].field("x").and_then(Value::as_int), Some(1));
        assert_eq!(args[1].field("x").and_then(Value::as_int), Some(3));
        assert!(
            machine
                .collections()
                .iter()
                .all(|collection| collection.roots_yielded == 2 && collection.live_objects == 2),
            "both arguments were roots at every one of them: {:?}",
            machine.collections()
        );
        assert_eq!(machine.rooted(), 0);
    }

    /// The mutation of the test above, and the bug it is a control for:
    /// rooting each argument for its own materialisation is not the same as
    /// rooting all of them for the crossing. The second argument is off the
    /// stack and in no root while the first crosses, so a safepoint inside the
    /// first takes it.
    #[test]
    #[should_panic(expected = "names a swept object")]
    fn rooting_one_argument_at_a_time_sweeps_the_others() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let first = a_point(&mut machine, points, 1, 2);
        let second = a_point(&mut machine, points, 3, 4);
        machine.frame.push_handle(first);
        machine.frame.push_handle(second);

        machine.heap.stress(true);
        let a = machine.frame.take_handle(0);
        let b = machine.frame.take_handle(1);
        machine.materialise(a);
        machine.materialise(b);
    }

    // --------------------------- what the layout decides, on both sides

    /// One word, two readers, one layout. The collector does not follow the
    /// `Int` field even though its bits name a live object, and the boundary
    /// reads that same word as the `Int` the layout says it is — and neither
    /// of them consulted the bits to decide.
    #[test]
    fn a_boundary_layouts_reference_map_comes_from_its_payload_layout() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let mixed = machine.heap.register(Layout::boundary(
            "test.Mixed",
            Shape::Struct {
                type_name: "Mixed",
                fields: vec![("n", Part::Int), ("child", Part::Nested)],
            },
        ));
        let hidden = a_point(&mut machine, points, 8, 9);
        let child = a_point(&mut machine, points, 1, 2);
        let handle = machine.allocate(mixed, vec![hidden.to_slot(), child.to_slot()]);
        machine.frame.push_handle(handle);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 2, "{collected:?}");
        assert!(machine.heap.is_live(child));
        assert!(
            !machine.heap.is_live(hidden),
            "a scalar word is not an edge"
        );

        let value = machine.materialise(handle);
        assert_eq!(
            value.field("n").and_then(Value::as_int),
            Some(hidden.to_slot() as i64),
            "and the boundary read the same word as the Int the layout calls it"
        );
        assert_eq!(
            value
                .field("child")
                .and_then(|point| point.field("y"))
                .and_then(Value::as_int),
            Some(2)
        );
    }

    /// Most of the heap never crosses. An object whose layout is
    /// [`Shape::Opaque`] is the VM's own, and asking the boundary for one is a
    /// programming error rather than a value it can answer.
    #[test]
    #[should_panic(expected = "test.Node is the VM's own object")]
    fn an_opaque_object_does_not_cross_the_boundary() {
        let mut machine = Machine::new();
        let layout = node(&mut machine.heap);
        let handle = machine.allocate(layout, vec![7, Handle::NONE.to_slot()]);
        machine.materialise(handle);
    }

    // ------------------------------- decision 2's variable-length tail

    /// The thing a reference map exists for: a run of handles whose length
    /// nobody knew until the allocation, walked because the layout says the
    /// tail is references and for no other reason.
    #[test]
    fn a_tail_of_handles_is_walked_by_the_reference_map() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 8);
        let array = an_array(&mut machine, arrays, &elements);
        machine.frame.push_handle(array);

        let collected = machine.collect_now();
        assert_eq!(
            collected.roots_yielded, 1,
            "one slot names the array and nothing names its elements: {collected:?}"
        );
        assert_eq!(collected.live_objects, 9, "the array and all eight");
        assert_eq!(
            collected.expansions, collected.live_objects,
            "each expanded once: {collected:?}"
        );
        assert!(elements.iter().all(|&handle| machine.heap.is_live(handle)));
    }

    /// And the other half of the same statement. A tail the layout calls
    /// scalar is not walked however much its words look like handles — the
    /// tail's analogue of `a_scalar_word_holding_a_live_handles_bits_is_not_an_edge`,
    /// and the one that matters more, because a tail is where a walk that
    /// guessed from the bits would guess in bulk.
    #[test]
    fn a_tail_of_scalars_is_not_walked_however_its_words_look() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let ints = int_array(&mut machine.heap);
        let hidden = some_points(&mut machine, points, 4);
        let array = an_array(&mut machine, ints, &hidden);
        machine.frame.push_handle(array);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 1, "the array alone: {collected:?}");
        assert_eq!(collected.freed_objects, 4);
        assert!(
            hidden.iter().all(|&handle| !machine.heap.is_live(handle)),
            "every word of the tail holds a live object's handle, and the map \
             says the tail is scalar, so none of them is an edge"
        );
    }

    /// One layout, two objects, two lengths. The layout carries the fixed part
    /// and what the tail's words are; how many of them there are is the
    /// object's own header, and the walk and the byte count both take it from
    /// there.
    #[test]
    fn a_tails_length_is_the_objects_own_and_not_its_layouts() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        assert_eq!(
            machine.heap.layout(arrays).words(),
            0,
            "an array is all tail"
        );
        assert_eq!(machine.heap.layout(arrays).tail(), Some(Part::Nested));

        let elements = some_points(&mut machine, points, 3);
        let empty = an_array(&mut machine, arrays, &[]);
        let three = an_array(&mut machine, arrays, &elements);
        machine.frame.push_handle(empty);
        machine.frame.push_handle(three);

        assert_eq!(machine.heap.tail_range(empty).len(), 0);
        assert_eq!(machine.heap.tail_range(three).len(), 3);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 5, "both arrays and three points");
        assert_eq!(
            collected.live_bytes,
            (3 + 3 * 2) * size_of::<Slot>() as u64,
            "the empty array's nought words, the other's three, and six in \
             the points: {collected:?}"
        );
    }

    /// Decision 8's second and third multiplicities, in a tail. Five tail
    /// words naming one object are five edges and one expansion, and the
    /// object is neither expanded five times nor freed because it was reached
    /// more than once.
    #[test]
    fn a_shared_object_in_many_tail_slots_is_expanded_once() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let shared = a_point(&mut machine, points, 1, 2);
        let array = an_array(&mut machine, arrays, &[shared; 5]);
        machine.frame.push_handle(array);

        let collected = machine.collect_now();
        assert_eq!(collected.live_objects, 2, "{collected:?}");
        assert_eq!(
            collected.expansions, collected.live_objects,
            "reached five times and expanded once: {collected:?}"
        );
        assert!(machine.heap.is_live(shared));
    }

    /// A fixed part and a tail in one object, with a reference in each and a
    /// scalar beside them. The map is two rules and the walk applies both.
    #[test]
    fn a_fixed_part_and_a_tail_are_both_walked() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let chunks =
            machine
                .heap
                .register(Layout::with_tail("test.Chunk", 2, vec![1], Part::Nested));
        let hidden = a_point(&mut machine, points, 8, 9);
        let fixed = a_point(&mut machine, points, 1, 2);
        let tail = some_points(&mut machine, points, 3);
        let mut words = vec![hidden.to_slot(), fixed.to_slot()];
        words.extend(tail.iter().map(|handle| handle.to_slot()));
        let chunk = machine.allocate(chunks, words);
        machine.frame.push_handle(chunk);

        let collected = machine.collect_now();
        assert_eq!(
            collected.live_objects, 5,
            "the chunk, its one fixed reference and its three tail words: {collected:?}"
        );
        assert!(machine.heap.is_live(fixed));
        assert!(tail.iter().all(|&handle| machine.heap.is_live(handle)));
        assert!(
            !machine.heap.is_live(hidden),
            "the fixed word the map does not name is not an edge, and having a \
             tail did not make it one"
        );
    }

    /// An object shorter than its layout's fixed part has no tail; it has a
    /// missing field. A tail makes the width a range and not a free-for-all.
    #[test]
    #[should_panic(expected = "test.Chunk declares 2 words before its tail")]
    fn an_object_shorter_than_its_fixed_part_is_refused() {
        let mut machine = Machine::new();
        let chunks =
            machine
                .heap
                .register(Layout::with_tail("test.Chunk", 2, vec![1], Part::Nested));
        machine.allocate(chunks, vec![0]);
    }

    // --------------------------------------- a tail at decision 5's boundary

    /// A tail of scalars across the boundary, at three lengths from one
    /// layout, and `Value::items` is the reader that answers.
    #[test]
    fn an_array_of_scalars_materialises_from_a_tail() {
        let mut machine = Machine::new();
        let ints = int_array(&mut machine.heap);
        for length in [0usize, 1, 5] {
            let words: Vec<Slot> = (0..length).map(|n| n as Slot).collect();
            let handle = machine.allocate(ints, words);
            let value = machine.materialise(handle);
            let items = value.items().expect("an array's items");
            assert_eq!(items.len(), length);
            let read: Vec<Option<i64>> = items.iter().map(Value::as_int).collect();
            assert_eq!(
                read,
                (0..length as i64).map(Some).collect::<Vec<_>>(),
                "a tail of {length}"
            );
        }
        assert_eq!(machine.rooted(), 0);
    }

    /// A tail of handles across the boundary: the reference map says the run
    /// is references, so every word of it is materialised in turn.
    #[test]
    fn an_array_of_handles_materialises_through_the_tails_reference_map() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 3);
        let array = an_array(&mut machine, arrays, &elements);

        let value = machine.materialise(array);
        let items = value.items().expect("an array's items");
        assert_eq!(items.len(), 3);
        let read: Vec<Option<i64>> = items
            .iter()
            .map(|item| item.field("y").and_then(Value::as_int))
            .collect();
        assert_eq!(read, vec![Some(0), Some(10), Some(20)]);
    }

    /// The tail whose word count is not its element count. The fixed word says
    /// how many bytes are the string's, the tail packs eight to a word, and
    /// the reference map's only interest in any of it is that none of it is a
    /// handle.
    #[test]
    fn a_string_materialises_from_a_packed_scalar_tail() {
        let mut machine = Machine::new();
        let layout = strings(&mut machine.heap);
        for text in ["", "hi", "eight!!!", "a string that needs three words"] {
            let handle = a_string(&mut machine, layout, text);
            assert_eq!(
                machine.heap.tail_range(handle).len(),
                text.len().div_ceil(size_of::<Slot>()),
                "eight bytes to a word, for {text:?}"
            );
            let value = machine.materialise(handle);
            assert_eq!(value.as_str(), Some(text));
        }
    }

    /// The positive rooting direction, for a tail. The array is out of the
    /// frame and in a Rust local for the whole crossing, a collection runs
    /// before every word of the tail is read, and nothing in it is swept.
    #[test]
    fn the_tail_of_a_source_survives_a_collection_in_the_middle_of_materialising_it() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 4);
        let array = an_array(&mut machine, arrays, &elements);
        let at = machine.frame.push_handle(array);

        let local = machine.frame.take_handle(at);
        machine.heap.stress(true);
        let value = machine.materialise(local);

        assert_eq!(
            value
                .items()
                .map(|items| items.len())
                .expect("an array's items"),
            4
        );
        assert!(
            machine
                .collections()
                .iter()
                .all(|collection| collection.live_objects == 5 && collection.freed_objects == 0),
            "the array and its four elements survived every one: {:?}",
            machine.collections()
        );
        let depths: Vec<u64> = machine
            .collections()
            .iter()
            .map(|collection| collection.roots_yielded)
            .collect();
        assert_eq!(
            depths,
            vec![1, 2, 2, 1, 2, 2, 1, 2, 2, 1, 2, 2],
            "one root while a tail word is read, two while the point it names \
             is, four times over"
        );
        assert_eq!(machine.rooted(), 0);
    }

    /// The negative direction, same program without the root: the first
    /// safepoint inside the crossing sweeps the array, and the next tail word
    /// read is the use-after-free.
    #[test]
    #[should_panic(expected = "names a swept object")]
    fn the_source_of_a_tail_is_swept_mid_materialisation_without_the_root() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 4);
        let array = an_array(&mut machine, arrays, &elements);
        let at = machine.frame.push_handle(array);

        let local = machine.frame.take_handle(at);
        machine.heap.stress(true);
        machine.materialise_rooted(local);
    }

    // ------------------------------------------- a tail as a run of siblings

    /// The sibling case at the scale a tail gives it. A spread call's
    /// arguments are the tail of an array; the array is consumed by the
    /// crossing and rooted by nothing, so the elements' only root is the
    /// shadow stack — eight of them at once, none of which roots any other.
    ///
    /// The array being swept at the first safepoint is the assertion, not an
    /// accident: it is what says the reference map is not what is keeping the
    /// elements alive here.
    #[test]
    fn every_element_of_a_tail_is_rooted_for_the_whole_crossing() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 8);
        let array = an_array(&mut machine, arrays, &elements);

        machine.heap.stress(true);
        let args = machine.materialise_tail_args(array);

        assert_eq!(args.len(), 8);
        let read: Vec<Option<i64>> = args
            .iter()
            .map(|arg| arg.field("y").and_then(Value::as_int))
            .collect();
        assert_eq!(read, (0..8).map(|n| Some(n * 10)).collect::<Vec<_>>());
        assert!(
            !machine.heap.is_live(array),
            "the array is the crossing's argument vector and nothing roots it"
        );
        assert_eq!(
            machine.collections()[0].freed_objects,
            1,
            "the array went at the first safepoint and nothing else did: {:?}",
            machine.collections()
        );
        assert!(
            machine
                .collections()
                .iter()
                .all(|collection| collection.roots_yielded == 8 && collection.live_objects == 8),
            "all eight were roots at every one of them: {:?}",
            machine.collections()
        );
        assert!(elements.iter().all(|&handle| machine.heap.is_live(handle)));
        assert_eq!(machine.rooted(), 0);
    }

    /// The mutation of the test above, and the reason its rooting is not
    /// decoration. Rooting each element for its own materialisation leaves the
    /// other seven in a Rust local and in no root at all, and the first
    /// safepoint inside the first crossing takes them.
    #[test]
    #[should_panic(expected = "names a swept object")]
    fn rooting_one_tail_element_at_a_time_sweeps_the_siblings() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let elements = some_points(&mut machine, points, 8);
        let array = an_array(&mut machine, arrays, &elements);

        machine.heap.stress(true);
        let handles = machine.tail_handles(array);
        for handle in handles {
            machine.materialise(handle);
        }
    }

    /// Decision 8's three multiplicities at the seam, in a tail. Two tail
    /// words naming one object are **two root storage locations and one
    /// expansion**: the crossing roots each location it took a handle out of,
    /// and marking expands the object they share once.
    #[test]
    fn a_tail_naming_one_object_twice_is_two_locations_and_one_expansion() {
        let mut machine = Machine::new();
        let points = point(&mut machine.heap);
        let arrays = handle_array(&mut machine.heap);
        let shared = a_point(&mut machine, points, 1, 2);
        let array = an_array(&mut machine, arrays, &[shared, shared]);

        machine.heap.stress(true);
        let args = machine.materialise_tail_args(array);

        for collection in machine.collections() {
            assert_eq!(
                collection.roots_yielded, 2,
                "two tail slots are two locations: {collection:?}"
            );
            assert_eq!(collection.live_objects, 1, "{collection:?}");
            assert_eq!(
                collection.expansions, collection.live_objects,
                "and one expansion however many locations reach it: {collection:?}"
            );
        }
        let read: Vec<Option<i64>> = args
            .iter()
            .map(|arg| arg.field("x").and_then(Value::as_int))
            .collect();
        assert_eq!(read, vec![Some(1), Some(1)], "two `Value`s, one object");
        assert_eq!(machine.rooted(), 0);
    }

    /// A scalar tail holds no references, so nothing may read one out of it —
    /// decision 1's invariant, stated for a tail rather than for a slot.
    #[test]
    #[should_panic(expected = "test.IntArray's tail is scalar")]
    fn a_scalar_tail_cannot_be_read_as_references() {
        let mut machine = Machine::new();
        let ints = int_array(&mut machine.heap);
        let array = machine.allocate(ints, vec![0, 1, 2]);
        machine.materialise_tail_args(array);
    }
}
