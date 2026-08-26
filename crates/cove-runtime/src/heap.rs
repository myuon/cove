//! The per-task mark-and-sweep collector.
//!
//! The Language Card says memory is managed by a precise, non-moving
//! mark-and-sweep collector, and ADR 0011 narrows that to a heap per task over
//! the values a task owns. This module is that heap.
//!
//! ADR 0008 gives each spawned task a thread and an [`crate::interp::Interpreter`]
//! of its own, so a heap belongs to one interpreter and is reached only from
//! the thread running it. That is what makes "per task" more than a
//! convention: a task's objects are unreachable from any other thread, so a
//! collection needs no safepoint from anyone else and takes no lock.
//!
//! # What the heap owns
//!
//! `Rc` reclaims a value the moment nothing points at it, which is correct for
//! every Cove value that is built once and never altered — a string, an array,
//! a map, a set, a closure, a struct, an enum case. None of those can be made
//! to point back at something that points at them, because each is built from
//! values that already exist. The one exception is
//! [`crate::value::VectorStorage`]: a vector's elements are behind a
//! `RefCell`, so `v.push(v)` is a cycle, and `Rc` alone will never free it.
//!
//! The heap therefore tracks exactly the objects that can form a cycle. `Rc`
//! remains the allocation handle and still reclaims everything acyclic on its
//! own; the collector exists for what `Rc` cannot do.
//!
//! The heap holds a [`Weak`] handle to each object rather than a strong one.
//! That is not an optimisation: `freeze()` consumes *uniquely owned* vector
//! storage and asks `Rc::strong_count` whether the caller holds the only
//! handle, so a heap holding a strong reference would make `freeze()` fail on
//! every vector. A `Weak` keeps the collector out of the language's own
//! uniqueness rule, and it costs nothing — a cycle keeps itself alive, so a
//! `Weak` to a member of one always upgrades.
//!
//! # Roots, and why reference counts are part of them
//!
//! ADR 0011 is explicit that the roots are the interpreter's own structures
//! rather than a machine stack, so there are no stack maps here. Every binding
//! the interpreter creates is a [`crate::interp`] `Place`, whose slot is
//! registered in a [`Roots`] list with the same push-and-truncate discipline
//! the environment chain already has.
//!
//! That covers named bindings. It does not cover a value the evaluator is
//! holding in a Rust local — the left operand of a `+` whose right operand is
//! still being evaluated, an argument evaluated before the call it belongs to.
//! Those are the values ADR 0011 calls "values being evaluated," and a tree
//! walker has no list of them.
//!
//! The collector finds them exactly, without scanning anything: it counts the
//! references it *can* see. For every shared allocation it walks — a vector, an
//! array, a map, a closure, a trait object, a task, a task scope — it sums the
//! references reachable from the registered roots and from the objects it
//! manages, and compares that with `Rc::strong_count`. A shortfall is a
//! reference held somewhere the collector cannot read — an evaluator temporary
//! — so that allocation, and everything it holds, is a root.
//!
//! Counting the containers as well as the objects is what makes this sound
//! rather than merely plausible. An array can hold the only reference to a
//! vector while being held itself by a garbage cycle *and* by a temporary; if
//! only the vector were counted, every reference to it would look accounted
//! for — by the garbage — and the sweep would empty something the program can
//! still reach.
//!
//! This is precise in the sense ADR 0001 asks for: no word is guessed to be a
//! pointer, and no integer is ever mistaken for one. It is also the invariant
//! that makes every other awkward case safe. A slot the interpreter has
//! mutably borrowed cannot be read, so its references go unseen, so whatever
//! it holds is treated as a root.
//!
//! # What it does not do
//!
//! No finalizers, no compaction, no generations, no concurrent or incremental
//! collection, no weak references in the language. Each is out of scope in ADR
//! 0001 and remains so in ADR 0011.
//!
//! # `Shared`
//!
//! ADR 0011 says a `Shared<T>` cell owns its contents rather than any task's
//! heap, and collects them with the cell. That is what happens, and it needs
//! no collector at all.
//!
//! A [`crate::shared::SharedCell`] holds a [`crate::task::Transfer`], not a
//! [`Value`], and `Transfer::of` refuses a `Vector`. A cell therefore cannot
//! hold a collectable object, so there is nothing in one for a heap to own and
//! no way for a cycle among a task's objects to run through one. The `Arc`
//! frees the contents with the cell, which is exactly what the ADR asks for. Each `lock` materialises
//! a fresh `Value` for the locking task, and *that* copy is an ordinary value
//! in that task's heap, collected there like any other.
//!
//! So the collector treats a `Shared` as a leaf: it never takes the cell's
//! lock. That is not only unnecessary, it is required. `lock` holds the mutex
//! for the whole of the closure it is given, that closure runs Cove code, and
//! Cove code reaches safepoints — so a collector that locked a cell would
//! sooner or later wait for a lock the collecting thread already holds.
//!
//! One thing this does not reach: a cell may hold *another* cell, including
//! itself, and that is an `Arc` cycle no heap here can see. Cells are
//! reachable from every task that was given one and outlive all of them, so
//! collecting cycles among them would need a collector that stops every
//! thread — which ADR 0011 rules out under "no concurrent collection". It is
//! a real leak, and the ADR now says so under "What this leaves uncollected".

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::task::{Task, TaskScope};
use crate::value::{MapKey, Value, VectorStorage};

/// The fewest objects a task may allocate between two collections.
///
/// A collection costs a walk of the live set, so collecting after every
/// allocation would make the collector the program. This floor is what a small
/// program pays: it allocates fewer than this many vectors and is never
/// collected at all.
const MIN_ALLOCATIONS_BETWEEN_COLLECTIONS: u64 = 64;

/// How much the object count may grow past the live set before the next
/// collection.
///
/// Doubling makes the total collection work over a run proportional to total
/// allocation rather than to allocation times live size, which is the standard
/// reason to size the next collection from the last one's survivors.
const GROWTH_FACTOR: u64 = 2;

/// A slot the collector starts from: one binding's storage.
///
/// This is [`crate::interp`]'s `Place` slot, registered here so a collection
/// can read it. The interpreter is the only writer, and it keeps the list in
/// step with the environment chain: a binding pushes, and leaving a block or a
/// call truncates back to the length it recorded on entry.
type Slot = Rc<RefCell<Value>>;

/// Every binding one interpreter currently holds, innermost last.
///
/// The list is shared by every environment on one thread, and its
/// push-and-truncate discipline mirrors that thread's environment chain
/// exactly. There is one list per interpreter and one interpreter per task, so
/// this *is* a task's roots — nothing has to be sliced out of a larger set.
#[derive(Default)]
pub struct Roots {
    slots: Vec<Slot>,
}

impl Roots {
    /// An empty root set.
    pub fn new() -> Roots {
        Roots::default()
    }

    /// How many slots are registered. A caller records this before entering a
    /// scope and hands it back to [`Roots::truncate`] on the way out.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no binding is registered at all.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Registers one binding's slot.
    pub fn push(&mut self, slot: Slot) {
        self.slots.push(slot);
    }

    /// Drops every slot registered after `len`, which is what leaving a block
    /// or a call does.
    pub fn truncate(&mut self, len: usize) {
        self.slots.truncate(len);
    }

    /// Every registered slot, which is this task's bindings and no others'.
    fn slots(&self) -> &[Slot] {
        &self.slots
    }
}

/// What one collection did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collection {
    /// Objects allocated since the previous collection.
    pub allocated: u64,
    /// Objects the sweep reclaimed.
    pub freed_objects: u64,
    /// Bytes held by what the sweep reclaimed, counting each shared
    /// allocation once and not following an edge into an object that
    /// survived. A value the reclaimed objects shared with a survivor is
    /// counted here even though it was not released, so this is an upper
    /// bound; [`Collection::live_bytes`] is the measured figure.
    pub freed_bytes: u64,
    /// Objects still live after the sweep.
    pub live_objects: u64,
    /// Bytes the live set holds, by the accounting [`Heap::live_bytes`]
    /// describes.
    pub live_bytes: u64,
    /// How long the program was stopped for this collection.
    pub pause: Duration,
}

/// What a run's heaps did in total.
///
/// `cove run --stats` prints this, and the trace's `heap_summary` event
/// carries it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Collectable objects allocated over the whole run.
    pub allocated_objects: u64,
    /// Bytes those allocations asked for, at the size each object was born.
    pub allocated_bytes: u64,
    /// How many collections ran.
    pub collections: u64,
    /// Objects those collections reclaimed.
    pub freed_objects: u64,
    /// Bytes live at the most recent collection.
    pub live_bytes: u64,
    /// Objects live at the most recent collection.
    pub live_objects: u64,
    /// The largest live set any collection measured.
    pub peak_bytes: u64,
    /// Total time the program was stopped for collection.
    pub pause: Duration,
}

impl HeapStats {
    /// Folds one heap's totals into these, which is what happens when a task's
    /// thread ends and its heap is retired into the run's.
    ///
    /// The live figures are not merged: a live set is a present fact about one
    /// heap rather than a total over a run. At the end of a run every task's
    /// heap has been swept and gone with its thread, so what is live is the
    /// entry's own and [`crate::interp::Interpreter::heap_stats`] reads it
    /// from there.
    pub fn merge(&mut self, other: &HeapStats) {
        self.allocated_objects += other.allocated_objects;
        self.allocated_bytes += other.allocated_bytes;
        self.collections += other.collections;
        self.freed_objects += other.freed_objects;
        self.peak_bytes = self.peak_bytes.max(other.peak_bytes);
        self.pause += other.pause;
    }
}

/// One task's heap.
///
/// A task owns the objects it allocates. The Language Card's task-safety rule
/// is what makes that a language rule rather than an approximation: a vector
/// may not cross a task boundary, so no two tasks ever hold the same one, and
/// a task can collect without waiting for any other task to reach a safepoint.
pub struct Heap {
    /// Every object this task has allocated and not yet lost, keyed by the
    /// address the object was allocated at. The address is stable while the
    /// `Weak` lives, so it identifies the object even after `Rc` has already
    /// reclaimed it.
    objects: HashMap<usize, Weak<VectorStorage>>,
    allocations_since_collection: u64,
    next_collection_at: u64,
    stats: HeapStats,
}

impl Heap {
    /// An empty heap.
    pub fn new() -> Heap {
        Heap {
            objects: HashMap::new(),
            allocations_since_collection: 0,
            next_collection_at: MIN_ALLOCATIONS_BETWEEN_COLLECTIONS,
            stats: HeapStats::default(),
        }
    }

    /// This heap's totals so far.
    pub fn stats(&self) -> HeapStats {
        self.stats
    }

    /// Whether the heap is tracking no object at all, which is what a program
    /// that never made a vector looks like.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// This heap's totals so far, resetting the cumulative ones.
    ///
    /// A task's heap ends with its thread, and what it did is folded into the
    /// run's totals by [`crate::runtime::Runtime::retire_heap`]. Taking the
    /// counters rather than reading them keeps a second fold from counting the
    /// same allocation twice. The live figures describe the heap right now and
    /// are not counters, so they are left alone.
    pub fn take_stats(&mut self) -> HeapStats {
        let taken = self.stats;
        self.stats = HeapStats {
            live_bytes: self.stats.live_bytes,
            live_objects: self.stats.live_objects,
            peak_bytes: self.stats.peak_bytes,
            ..HeapStats::default()
        };
        taken
    }

    /// Allocates growable vector storage owned by this heap.
    ///
    /// The returned `Rc` is the program's handle; the heap keeps only a
    /// `Weak`, so the value's lifetime is still `Rc`'s to decide until a cycle
    /// takes that decision away from it.
    pub fn allocate(&mut self, elements: Vec<Value>) -> Rc<VectorStorage> {
        let storage = VectorStorage::new(elements);
        self.stats.allocated_objects += 1;
        self.stats.allocated_bytes += object_bytes(&storage);
        self.allocations_since_collection += 1;
        self.objects
            .insert(Rc::as_ptr(&storage) as usize, Rc::downgrade(&storage));
        storage
    }

    /// Whether enough has been allocated since the last collection to be worth
    /// another one.
    pub fn should_collect(&self) -> bool {
        self.allocations_since_collection >= self.next_collection_at
    }

    /// Bytes live as of the most recent collection.
    ///
    /// This counts the whole live set, not only the objects the heap manages:
    /// every string, array, map, set, struct, enum case, and closure the live
    /// objects and the roots reach, with each shared allocation counted once.
    /// It is the storage the runtime holds for the program's values, and the
    /// figure the memory budget is checked against.
    pub fn live_bytes(&self) -> u64 {
        self.stats.live_bytes
    }

    /// Marks from the roots and sweeps what is not marked.
    pub fn collect(&mut self, roots: &Roots) -> Collection {
        let started = Instant::now();

        // An object `Rc` already reclaimed leaves a dead `Weak` behind. Drop
        // those first so every count below is over objects that still exist.
        self.objects.retain(|_, weak| weak.strong_count() > 0);

        // Every strong count is read before anything here upgrades a handle,
        // so the numbers describe the program's references and not the
        // collector's.
        let strong: HashMap<usize, usize> = self
            .objects
            .iter()
            .map(|(&at, weak)| (at, weak.strong_count()))
            .collect();

        let scan = self.count_visible_references(roots);
        let live = self.mark(roots, &scan, &strong);
        // The scan holds a handle to everything it saw. Releasing them before
        // the sweep keeps the collector out of the reference counts it is
        // about to act on.
        drop(scan);
        let (freed_objects, freed_bytes) = self.sweep(&live.marked);

        let collection = Collection {
            allocated: self.allocations_since_collection,
            freed_objects,
            freed_bytes,
            live_objects: self.objects.len() as u64,
            live_bytes: live.bytes,
            pause: started.elapsed(),
        };

        self.allocations_since_collection = 0;
        self.next_collection_at =
            (collection.live_objects * GROWTH_FACTOR).max(MIN_ALLOCATIONS_BETWEEN_COLLECTIONS);
        self.stats.collections += 1;
        self.stats.freed_objects += freed_objects;
        self.stats.live_bytes = collection.live_bytes;
        self.stats.live_objects = collection.live_objects;
        self.stats.peak_bytes = self.stats.peak_bytes.max(collection.live_bytes);
        self.stats.pause += collection.pause;
        collection
    }

    /// Counts, for every shared allocation it can reach, how many references
    /// to it the collector can see.
    ///
    /// It is not enough to do this for the objects the heap manages. A `Rc`
    /// container the collector does not manage — an array, a map, a closure,
    /// a trait object, a task — can hold the only reference to an object while
    /// itself being held by nothing but an evaluator temporary. Counting the
    /// container too is what catches that: its own references do not add up
    /// either, so everything it holds is a root.
    ///
    /// Each allocation's contents are walked exactly once, so each physical
    /// reference is counted exactly once and a shortfall is exactly the set of
    /// references the collector cannot read.
    fn count_visible_references(&self, roots: &Roots) -> Scan {
        let mut scan = Scan {
            seen: HashMap::new(),
            slots: HashSet::new(),
        };
        for slot in roots.slots() {
            if !scan.slots.insert(Rc::as_ptr(slot) as usize) {
                continue;
            }
            if let Ok(value) = slot.try_borrow() {
                scan.count(&value);
            };
        }
        for weak in self.objects.values() {
            let Some(object) = weak.upgrade() else {
                continue;
            };
            // The heap's own table is not a reference, so this registers the
            // object without sighting one.
            scan.observe(&object);
            if let Ok(elements) = object.elements.try_borrow() {
                for element in elements.iter() {
                    scan.count(element);
                }
            };
        }
        scan
    }

    /// Marks everything reachable from this task's bindings and from every
    /// object some evaluator temporary still holds.
    fn mark(&self, roots: &Roots, scan: &Scan, strong: &HashMap<usize, usize>) -> LiveSet {
        let mut marker = Marker {
            managed: &self.objects,
            excluded: None,
            marked: HashSet::new(),
            walked: HashSet::new(),
            bytes: 0,
            work: Vec::new(),
        };
        for slot in roots.slots() {
            if !marker.walked.insert(Rc::as_ptr(slot) as usize) {
                continue;
            }
            // A slot that cannot be read is one the interpreter is writing
            // through. Its references went uncounted above, so whatever it
            // holds is already a root by the rule below.
            if let Ok(value) = slot.try_borrow() {
                marker.visit(&value);
            }
        }
        // Anything whose references do not add up is held from somewhere the
        // collector cannot read — an evaluator temporary — so it is a root.
        for (at, sighting) in &scan.seen {
            // A managed object's count is the one snapshotted before this
            // collection upgraded any handle; every other allocation's was
            // read the first time it was seen, before the scan took a handle
            // of its own.
            let held = strong.get(at).copied().unwrap_or(sighting.strong);
            if held > sighting.sighted {
                marker.visit(&sighting.held);
            }
        }
        // An object whose elements are borrowed right now cannot be read, and
        // so cannot be swept either: clearing it is exactly what the borrow
        // would forbid.
        for (at, weak) in &self.objects {
            if marker.marked.contains(at) {
                continue;
            }
            if let Some(object) = weak.upgrade() {
                if object.elements.try_borrow().is_err() {
                    marker.enqueue(object);
                }
            };
        }
        marker.drain();
        LiveSet {
            marked: marker.marked,
            bytes: marker.bytes,
        }
    }

    /// Reclaims every object the mark phase did not reach.
    ///
    /// The two phases matter. Clearing every doomed object's elements while
    /// the heap still holds a strong handle to all of them breaks the cycles
    /// without any object's `Drop` running inside another's, so a long chain
    /// is torn down iteratively rather than by a recursive drop that would
    /// exhaust the native stack. Dropping the handles afterwards is what
    /// actually frees the storage.
    fn sweep(&mut self, marked: &HashSet<usize>) -> (u64, u64) {
        let mut doomed: Vec<Rc<VectorStorage>> = Vec::new();
        for (at, weak) in &self.objects {
            if marked.contains(at) {
                continue;
            }
            if let Some(object) = weak.upgrade() {
                doomed.push(object);
            }
        }
        if doomed.is_empty() {
            return (0, 0);
        }

        let freed_bytes = {
            let mut accounting = Marker {
                managed: &self.objects,
                excluded: Some(marked),
                marked: HashSet::new(),
                walked: HashSet::new(),
                bytes: 0,
                work: Vec::new(),
            };
            for object in &doomed {
                accounting.enqueue(object.clone());
            }
            accounting.drain();
            accounting.bytes
        };

        self.objects.retain(|at, _| marked.contains(at));
        for object in &doomed {
            if let Ok(mut elements) = object.elements.try_borrow_mut() {
                elements.clear();
            }
        }
        let freed_objects = doomed.len() as u64;
        drop(doomed);
        (freed_objects, freed_bytes)
    }
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}

/// What one mark phase found: which objects are live, and how much storage the
/// values it reached hold.
struct LiveSet {
    marked: HashSet<usize>,
    bytes: u64,
}

/// One shared allocation the collector saw, and how completely it saw it.
struct Sighting {
    /// A handle to walk it again in the mark phase, taken the first time it
    /// was seen.
    held: Value,
    /// References to it the collector could read.
    sighted: usize,
    /// References to it that exist, read before the handle above was taken.
    strong: usize,
}

/// Counts the references to every shared allocation the collector can reach.
struct Scan {
    seen: HashMap<usize, Sighting>,
    /// Root slots already read, since one `Place` can be registered by more
    /// than one environment: a `var` parameter binds the caller's slot.
    slots: HashSet<usize>,
}

impl Scan {
    /// Records one reference to the allocation at `at`, and reports whether
    /// this was the first time the collector saw it — which is when its
    /// contents still need walking.
    fn sight(&mut self, at: usize, strong: usize, held: impl FnOnce() -> Value) -> bool {
        match self.seen.get_mut(&at) {
            Some(sighting) => {
                sighting.sighted += 1;
                false
            }
            None => {
                self.seen.insert(
                    at,
                    Sighting {
                        held: held(),
                        sighted: 1,
                        strong,
                    },
                );
                true
            }
        }
    }

    /// Registers a managed object without counting a reference to it: the
    /// heap's own table is a `Weak`, not a reference the program holds.
    fn observe(&mut self, object: &Rc<VectorStorage>) {
        let at = Rc::as_ptr(object) as usize;
        if let std::collections::hash_map::Entry::Vacant(slot) = self.seen.entry(at) {
            slot.insert(Sighting {
                held: Value::Vector(object.clone()),
                sighted: 0,
                // A managed object's real count is snapshotted by
                // `Heap::collect` before anything upgrades a handle; this
                // one is never used for a managed object.
                strong: 0,
            });
        }
    }

    /// Counts one reference for every shared allocation `value` names, and
    /// walks the contents of each the first time it is seen.
    ///
    /// A managed object's contents are not walked here: the heap's table
    /// enumerates every object, so walking from a reference as well would
    /// count its outgoing references twice.
    fn count(&mut self, value: &Value) {
        match value {
            Value::Vector(storage) => {
                self.sight(
                    Rc::as_ptr(storage) as usize,
                    Rc::strong_count(storage),
                    || value.clone(),
                );
            }
            Value::Array(items) => {
                if self.sight(array_addr(items), Rc::strong_count(items), || value.clone()) {
                    for item in items.iter() {
                        self.count(item);
                    }
                }
            }
            Value::Map(entries) => {
                if self.sight(
                    Rc::as_ptr(entries) as usize,
                    Rc::strong_count(entries),
                    || value.clone(),
                ) {
                    for entry in entries.values() {
                        self.count(entry);
                    }
                }
            }
            Value::Closure(closure) => {
                if self.sight(
                    Rc::as_ptr(closure) as usize,
                    Rc::strong_count(closure),
                    || value.clone(),
                ) {
                    for (_, captured) in &closure.captures {
                        self.count(captured);
                    }
                }
            }
            Value::Dyn(wrapped) => {
                if self.sight(
                    Rc::as_ptr(wrapped) as usize,
                    Rc::strong_count(wrapped),
                    || value.clone(),
                ) {
                    self.count(&wrapped.value);
                }
            }
            Value::Task(task) => self.count_task(task),
            Value::TaskScope(scope) => {
                if self.sight(Rc::as_ptr(scope) as usize, Rc::strong_count(scope), || {
                    value.clone()
                }) {
                    if let Ok(tasks) = scope.tasks.try_borrow() {
                        for task in tasks.iter() {
                            self.count_task(task);
                        }
                    };
                }
            }
            // A `Struct` and an `Enum` are `Box`ed, so each is owned by
            // exactly one value and no two paths reach the same one.
            Value::Struct(structure) => {
                for (_, field) in &structure.fields {
                    self.count(field);
                }
            }
            Value::Enum(enumeration) => {
                for item in &enumeration.payload {
                    self.count(item);
                }
            }
            // A `Shared` is a leaf. Its cell holds a `Transfer`, which no
            // `Vector` can be part of, so no reference to a managed object
            // hides in one — and reading it would mean taking a lock the
            // collecting thread may already hold.
            Value::Shared(_) => {}
            // A `Set`'s elements are `MapKey`s, which no mutable handle can
            // be, so no reference hides in one. A resource handle is a name
            // the host resolves, so it owns no Cove object either. Every
            // remaining case is a scalar, a string, or a range.
            _ => {}
        }
    }

    fn count_task(&mut self, task: &Rc<Task>) {
        let first = self.sight(Rc::as_ptr(task) as usize, Rc::strong_count(task), || {
            Value::Task(Rc::clone(task))
        });
        if !first {
            return;
        }
        // A task's body went to its own thread as a `Transfer` and is not
        // reachable from the handle, so the value it settled with is all a
        // handle holds.
        if let Ok(state) = task.state.try_borrow() {
            if let crate::task::TaskState::Settled(value) = &*state {
                self.count(value);
            }
        };
    }
}

/// Marks a live set and measures it.
struct Marker<'h> {
    /// The objects this heap manages. A vector allocated elsewhere — by a
    /// host, or by a test building a value directly — is not this heap's to
    /// mark or to free.
    managed: &'h HashMap<usize, Weak<VectorStorage>>,
    /// Objects to stop at, used when measuring what a sweep released: the
    /// survivors are not part of what was freed.
    excluded: Option<&'h HashSet<usize>>,
    marked: HashSet<usize>,
    walked: HashSet<usize>,
    bytes: u64,
    work: Vec<Rc<VectorStorage>>,
}

impl Marker<'_> {
    /// Marks `object` and queues its contents.
    fn enqueue(&mut self, object: Rc<VectorStorage>) {
        let at = Rc::as_ptr(&object) as usize;
        if self.excluded.is_some_and(|set| set.contains(&at)) {
            return;
        }
        if self.marked.insert(at) {
            self.work.push(object);
        }
    }

    /// Walks the queue until nothing is left.
    ///
    /// Managed objects go through this queue rather than through recursion,
    /// because a chain of vectors is as long as the program made it and
    /// recursion over one would be bounded by the native stack.
    fn drain(&mut self) {
        while let Some(object) = self.work.pop() {
            self.bytes += object_bytes(&object);
            // An object whose elements are borrowed right now cannot be read,
            // and does not need to be: its references went uncounted, so
            // everything it holds is already a root.
            if let Ok(elements) = object.elements.try_borrow() {
                for element in elements.iter() {
                    self.visit(element);
                }
            }
        }
    }

    /// Marks every managed object `value` reaches, and adds the storage
    /// `value` itself holds to the live total.
    fn visit(&mut self, value: &Value) {
        match value {
            // A vector this heap does not manage — another task's, or one
            // built outside the interpreter — is neither its to mark nor its
            // to measure.
            Value::Vector(storage)
                if self.managed.contains_key(&(Rc::as_ptr(storage) as usize)) =>
            {
                self.enqueue(storage.clone());
            }
            Value::Str(text) => {
                if self.walked.insert(text.as_ptr() as usize) {
                    self.bytes += text.len() as u64;
                }
            }
            Value::Array(items) => {
                if self.walked.insert(array_addr(items)) {
                    self.bytes += (items.len() * size_of::<Value>()) as u64;
                    for item in items.iter() {
                        self.visit(item);
                    }
                }
            }
            Value::Map(entries) => {
                if self.walked.insert(Rc::as_ptr(entries) as usize) {
                    for (key, entry) in entries.iter() {
                        self.bytes += key_bytes(key) + size_of::<Value>() as u64;
                        self.visit(entry);
                    }
                }
            }
            Value::Set(items) => {
                if self.walked.insert(Rc::as_ptr(items) as usize) {
                    for item in items.iter() {
                        self.bytes += key_bytes(item);
                    }
                }
            }
            Value::Struct(structure) => {
                self.bytes += size_of::<crate::value::StructValue>() as u64;
                for (name, field) in &structure.fields {
                    self.bytes += (name.len() + size_of::<Value>()) as u64;
                    self.visit(field);
                }
            }
            Value::Enum(enumeration) => {
                self.bytes += size_of::<crate::value::EnumValue>() as u64;
                for item in &enumeration.payload {
                    self.bytes += size_of::<Value>() as u64;
                    self.visit(item);
                }
            }
            Value::Closure(closure) => {
                if self.walked.insert(Rc::as_ptr(closure) as usize) {
                    self.bytes += size_of::<crate::value::Closure>() as u64;
                    for (name, captured) in &closure.captures {
                        self.bytes += (name.len() + size_of::<Value>()) as u64;
                        self.visit(captured);
                    }
                }
            }
            Value::Dyn(wrapped) => {
                if self.walked.insert(Rc::as_ptr(wrapped) as usize) {
                    self.bytes += size_of::<crate::value::DynValue>() as u64;
                    self.visit(&wrapped.value);
                }
            }
            Value::Task(task) => self.visit_task(task),
            // A `Shared`'s contents belong to the cell, not to this task, so
            // they are neither marked nor measured here; see this module's
            // documentation for why the lock is never taken.
            Value::Shared(_) => {}
            Value::TaskScope(scope) if self.walked.insert(Rc::as_ptr(scope) as usize) => {
                self.bytes += size_of::<TaskScope>() as u64;
                // A scope this thread is mid-way through mutating cannot be
                // read, so its tasks go unsighted and the shortfall rule
                // roots them.
                if let Ok(tasks) = scope.tasks.try_borrow() {
                    for task in tasks.iter() {
                        self.visit_task(task);
                    }
                }
            }
            _ => {}
        }
    }

    fn visit_task(&mut self, task: &Rc<Task>) {
        if !self.walked.insert(Rc::as_ptr(task) as usize) {
            return;
        }
        self.bytes += size_of::<Task>() as u64;
        if let Ok(state) = task.state.try_borrow() {
            if let crate::task::TaskState::Settled(value) = &*state {
                self.visit(value);
            }
        }
    }
}

/// The address of an array's shared storage.
///
/// `Rc<[Value]>` is a fat pointer, so it is narrowed to the address of its
/// first element, which identifies the allocation just as well.
fn array_addr(items: &Rc<[Value]>) -> usize {
    Rc::as_ptr(items) as *const Value as usize
}

/// The storage one object holds for itself: its header and its element slots.
///
/// What each element points at is counted separately, once, however many
/// elements point at it.
fn object_bytes(storage: &VectorStorage) -> u64 {
    let elements = storage
        .elements
        .try_borrow()
        .map(|elements| elements.len())
        .unwrap_or(0);
    (size_of::<VectorStorage>() + elements * size_of::<Value>()) as u64
}

/// The storage a map key or set element holds.
fn key_bytes(key: &MapKey) -> u64 {
    let own = size_of::<MapKey>() as u64;
    own + match key {
        MapKey::Str(text) => text.len() as u64,
        MapKey::EnumCase(type_name, case, payload) => {
            (type_name.len() + case.len()) as u64 + payload.iter().map(key_bytes).sum::<u64>()
        }
        MapKey::Struct(type_name, fields, _) => {
            type_name.len() as u64
                + fields
                    .iter()
                    .map(|(name, field)| name.len() as u64 + key_bytes(field))
                    .sum::<u64>()
        }
        MapKey::Array(items) => items.iter().map(key_bytes).sum(),
        MapKey::Set(items) => items.iter().map(key_bytes).sum(),
        MapKey::Map(entries) => entries
            .iter()
            .map(|(key, value)| key_bytes(key) + key_bytes(value))
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::StructValue;

    /// Registers `value` as a binding and returns the slot, so a test can drop
    /// the root later.
    fn root(roots: &mut Roots, value: Value) -> Slot {
        let slot = Rc::new(RefCell::new(value));
        roots.push(slot.clone());
        slot
    }

    #[test]
    fn an_unreachable_object_is_reclaimed() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let storage = heap.allocate(vec![Value::Int(1)]);
        drop(storage);
        let collected = heap.collect(&roots);
        // `Rc` already freed it, so there was nothing left for the sweep to
        // free; the heap still stops tracking it.
        assert_eq!(collected.live_objects, 0);
    }

    #[test]
    fn a_reachable_object_survives() {
        let mut roots = Roots::new();
        let mut heap = Heap::new();
        let storage = heap.allocate(vec![Value::Int(1)]);
        let _slot = root(&mut roots, Value::Vector(storage));
        let collected = heap.collect(&roots);
        assert_eq!(collected.live_objects, 1);
        assert_eq!(collected.freed_objects, 0);
    }

    /// The whole reason for the collector: two objects that point at each
    /// other keep each other's reference count above zero forever.
    #[test]
    fn a_cycle_is_reclaimed() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let a = heap.allocate(Vec::new());
        let b = heap.allocate(Vec::new());
        a.elements.borrow_mut().push(Value::Vector(b.clone()));
        b.elements.borrow_mut().push(Value::Vector(a.clone()));
        let weak = Rc::downgrade(&a);
        drop(a);
        drop(b);

        assert!(weak.upgrade().is_some(), "`Rc` cannot free a cycle");
        let collected = heap.collect(&roots);
        assert_eq!(collected.freed_objects, 2);
        assert_eq!(collected.live_objects, 0);
        assert!(weak.upgrade().is_none(), "the cycle was not freed");
    }

    #[test]
    fn a_reachable_cycle_survives() {
        let mut roots = Roots::new();
        let mut heap = Heap::new();
        let a = heap.allocate(Vec::new());
        a.elements.borrow_mut().push(Value::Vector(a.clone()));
        let _slot = root(&mut roots, Value::Vector(a.clone()));
        drop(a);
        let collected = heap.collect(&roots);
        assert_eq!(collected.freed_objects, 0);
        assert_eq!(collected.live_objects, 1);
    }

    /// A cycle whose back edge runs through a struct field is still a cycle.
    #[test]
    fn a_cycle_through_a_struct_field_is_reclaimed() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let object = heap.allocate(Vec::new());
        object
            .elements
            .borrow_mut()
            .push(Value::Struct(Box::new(StructValue {
                type_name: "test.Node".into(),
                fields: vec![("next".into(), Value::Vector(object.clone()))],
                opaque: false,
            })));
        let weak = Rc::downgrade(&object);
        drop(object);
        assert!(weak.upgrade().is_some());
        assert_eq!(heap.collect(&roots).freed_objects, 1);
        assert!(weak.upgrade().is_none());
    }

    /// An object held only by a value the collector cannot read — an
    /// evaluator temporary, here modelled by a plain Rust local — is a root,
    /// found by comparing the references the collector can see with the
    /// reference count.
    #[test]
    fn an_object_held_only_by_a_temporary_is_a_root() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let held = heap.allocate(vec![Value::Int(1)]);
        let collected = heap.collect(&roots);
        assert_eq!(collected.freed_objects, 0);
        assert_eq!(collected.live_objects, 1);
        assert_eq!(held.elements.borrow().len(), 1, "its contents survived");
    }

    /// The same rule, one level deeper: the temporary holds a container, and
    /// the object is inside it.
    #[test]
    fn an_object_inside_a_temporary_container_is_a_root() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let inner = heap.allocate(vec![Value::Int(7)]);
        let weak = Rc::downgrade(&inner);
        let temporary = Value::Array(vec![Value::Vector(inner)].into());
        let collected = heap.collect(&roots);
        assert_eq!(collected.freed_objects, 0);
        assert!(weak.upgrade().is_some());
        drop(temporary);
    }

    /// The subtle case, and the reason the scan counts references to shared
    /// containers and not only to managed objects. `shared` is held by a
    /// garbage cycle *and* by a temporary the collector cannot read. Counting
    /// only the object would find every reference to it accounted for — by
    /// the garbage — and free the one thing the temporary can still reach.
    #[test]
    fn an_object_reached_through_a_container_a_temporary_shares_with_garbage_survives() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let inner = heap.allocate(vec![Value::Int(7)]);
        let alive = Rc::downgrade(&inner);
        let shared: Rc<[Value]> = vec![Value::Vector(inner)].into();

        let a = heap.allocate(Vec::new());
        let b = heap.allocate(Vec::new());
        a.elements.borrow_mut().push(Value::Vector(b.clone()));
        a.elements
            .borrow_mut()
            .push(Value::Array(Rc::clone(&shared)));
        b.elements.borrow_mut().push(Value::Vector(a.clone()));
        let cycle = Rc::downgrade(&a);
        drop(a);
        drop(b);

        let collected = heap.collect(&roots);
        assert!(
            cycle.upgrade().is_none(),
            "the garbage cycle should have gone"
        );
        let survivor = alive
            .upgrade()
            .expect("the temporary's array still holds it");
        // Sweeping clears an object's elements, so this is what a wrong answer
        // looks like: the handle is still there and its contents are gone.
        assert!(
            survivor
                .elements
                .borrow()
                .first()
                .is_some_and(|element| element.eq_value(&Value::Int(7))),
            "the sweep emptied a vector something still holds: {collected:?}"
        );
        assert_eq!(shared.len(), 1);
    }

    #[test]
    fn a_binding_dropped_from_the_roots_is_reclaimed() {
        let mut roots = Roots::new();
        let mut heap = Heap::new();
        let object = heap.allocate(Vec::new());
        let cycle = Value::Vector(object.clone());
        object.elements.borrow_mut().push(cycle);
        drop(object);
        let base = roots.len();
        let slot = Rc::new(RefCell::new(Value::Unit));
        roots.push(slot.clone());
        drop(slot);
        roots.truncate(base);
        assert_eq!(heap.collect(&roots).freed_objects, 1);
    }

    #[test]
    fn live_bytes_falls_when_a_cycle_is_reclaimed() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let a = heap.allocate(vec![Value::Str("a fairly long string".into())]);
        let b = heap.allocate(Vec::new());
        a.elements.borrow_mut().push(Value::Vector(b.clone()));
        b.elements.borrow_mut().push(Value::Vector(a.clone()));

        let before = {
            // With both handles held, the cycle is rooted by the temporaries.
            heap.collect(&roots).live_bytes
        };
        drop(a);
        drop(b);
        let after = heap.collect(&roots).live_bytes;
        assert!(before > 0);
        assert_eq!(after, 0, "live bytes should fall to nothing: {before}");
    }

    #[test]
    fn collections_are_spaced_by_allocation() {
        let mut heap = Heap::new();
        assert!(!heap.should_collect());
        for _ in 0..MIN_ALLOCATIONS_BETWEEN_COLLECTIONS {
            let _ = heap.allocate(Vec::new());
        }
        assert!(heap.should_collect());
    }

    #[test]
    fn stats_accumulate_over_collections() {
        let roots = Roots::new();
        let mut heap = Heap::new();
        let object = heap.allocate(Vec::new());
        let cycle = Value::Vector(object.clone());
        object.elements.borrow_mut().push(cycle);
        drop(object);
        heap.collect(&roots);
        heap.collect(&roots);
        let stats = heap.stats();
        assert_eq!(stats.allocated_objects, 1);
        assert_eq!(stats.collections, 2);
        assert_eq!(stats.freed_objects, 1);
    }
}
