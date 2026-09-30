//! Reusing a loaded field until something may have replaced it.
//!
//! A `load-field d <- o +k` reads one payload word of the object in `o`. The
//! lowering emits one wherever the source asks for the word, into a scratch
//! slot it clears again after the use, so a body that reads the same word of
//! the same object ten times on a path reads it ten times. The commonest case
//! is a vector's store word, read afresh by every `core.vectorLoad` and
//! `core.vectorStore` because a push may have replaced it (issue #514's VM
//! residue (b)); but nothing here knows that. It is a question about any
//! owner slot and any field offset, asked of the finished code, and nothing
//! here names a function, a module, a layout's name or a shape's purpose.
//!
//! # The rule
//!
//! A *key* is an owner slot, a field offset and the one-word layout the field
//! is read at. Each key that is worth it gets a **cache slot** of its own,
//! appended to the frame. A load of the key writes the cache slot, and a load
//! of it where the cache slot already holds the field's current value on
//! **every** path is dropped. The cache slot holds the current value from a
//! load until the first instruction that *may* change it — a *kill*:
//!
//! - **a write of the owner slot**, by anything, at any width the wide answer
//!   of [`Flow::writes`] gives;
//! - **a `store-field` at the same offset** into an owner that may be the
//!   same object;
//! - **a growable instruction** — `growable-ensure`, `growable-commit`,
//!   `growable-truncate`, `run-finish` — on an owner that may be the same
//!   object, whatever the offset;
//! - **an element write** — `store-elem`, `run-store`, `run-copy`, the
//!   clearing half of a truncate or a finish, and an identity set's entry and
//!   leaving, which write the set's table — into an object that may be the
//!   owner;
//! - **anything else that writes the heap, calls, parks or is not
//!   classified**: every kind of call — plain, closure, Host, resource,
//!   intrinsic — a store through an address, a scope, a task, a lock, an
//!   assertion's report. That list is written as the complement of a list of
//!   instructions that are known not to, so an instruction added to the IR
//!   kills every key until somebody has asked what it writes.
//!
//! A `return` and a trap end the frame's run and have no way on, so nothing
//! is carried across either; at either, a cache slot holds the field's
//! current value, which the owner slot beside it holds too.
//!
//! *May be the same object* is decided by the layout the owner slot's writers
//! agree on, over the whole function, the way `crate::verify`'s slot facts
//! are: every writer of the slot — a parameter, a copy, a load, a call's
//! answer, an allocation — names one layout, or the slot's layout is
//! unknown and it may be anything. Values are shared by reference, so two
//! slots of one vector layout may hold one vector and a push through either
//! kills both; a vector of another element type, or an object of another
//! fixed layout, cannot be the same object and is not killed. A struct or an
//! enum is inline, so a slot whose layout is one holds a field of it, not an
//! object of it, and is unknown.
//!
//! # Partial redundancy
//!
//! A load that is available on some paths and not on others is made available
//! on all of them where that is cheap, rather than left as it was: the load
//! is inserted on each edge into the region where it is missing, and the
//! loads in the region then all go. The region is the set of program counters
//! where the key is *partially available* — some path from a load reaches it
//! without a kill — and *partially anticipated* — some path from it reaches a
//! load of the key without a kill.
//!
//! The rule is **safe, demand-justified insertion**: every inserted load is
//! safe at its insertion point, at least one successor path uses it before
//! the next kill, and no reload is inserted across a call barrier. It is not
//! "no speculative reloads": a reload may land on a path that is killed again
//! before it reads, which requiring *every* path to use it would forbid — but
//! that stricter rule fails at every insertion point of a loop with one rare
//! pushing arm or an exit, and so forfeits the whole gain. What is never done
//! is inserting a load on a way that cannot reach a use of it, or where no
//! path brought the value in the first place.
//!
//! What it does insert in a loop is the reload after a rare kill — a push onto a vector of the same layout, whose own
//! window read the new store — so that the hot turns that did not push read
//! nothing. What that costs is a reload on a turn that kills again before it
//! reads, which is the price of not having a profile: the program's own
//! loads are where it read, and the insertion moves them to where it lost
//! the value.
//!
//! Which is why a **call** is a barrier to it inside a loop. A key a call
//! took away on the way round a loop that calls is not loaded again on an
//! edge of that loop: the reload would be paid on every turn that calls, and
//! a loop whose turns call is one whose next turn may as well call again
//! before it reads, which nothing here can count. The load stays where the
//! program made it. The way *into* such a loop is taken once, and a load is
//! placed there as anywhere else.
//!
//! An inserted load goes once at the join where every way in needs it, on a
//! fall-through edge where it is one, before the jump that is the edge where
//! that is one, and otherwise in a block of its own at the end of the
//! function, which jumps on to where the edge went.
//!
//! A load is inserted only where it cannot fail and cannot write a word the
//! collector would misread: the owner's layout is a growable owner whose
//! payload is a length and a store reference, the key reads the word at the
//! `Repr` that layout puts there, and the owner slot is non-null on every
//! path to the edge — written by a parameter, an allocation, a call's answer
//! or a load of a value of that layout, and not cleared since.
//!
//! A load of an owner slot that a reservation in the same block is opened or
//! committed on is *pinned*: never dropped, and not counted as a use that
//! would pull a load towards it. ADR 0062's reservation rule reads the store
//! after the ensure and the length before it, of that slot, in that block,
//! and a load it needs is kept exactly where it is.
//!
//! # What a load's old destination becomes
//!
//! The lowering's scratch slot for a load is shared between owners, so the
//! load's destination and every read of it are renamed to the key's cache
//! slot — and only where that is exactly a renaming: every read the load
//! reaches is reached by loads of the same key alone, is one of the
//! instructions whose read operands can be named apart from their writes,
//! and is reached on every path by a load into the old slot after the last
//! kill, so the cache slot holds there what the old slot held; the old slot
//! was null on every way into the load, so leaving it unwritten keeps
//! nothing alive; and no name a debugger prints is bound to it between the
//! load and its last read.
//!
//! # The cache slot is a root, and it is cleared
//!
//! A reference key's cache slot is a reference word the collector reads, and
//! the value in it is the field's *current* value for as long as it is read:
//! the object in the owner slot holds it too, so for that long the cache
//! slot keeps nothing alive that the frame did not already. What would be
//! new is a cache slot still naming a store that a push has replaced — the
//! old store, garbage to everyone else. So a reference cache slot is cleared
//! where it stops being read on a way that can reach a kill before it is
//! loaded again: on the edge where it goes from live to dead, which is always
//! before the kill, because a kill is never between a load and a read of the
//! value it loaded. It is therefore null at every kill, and **a store that
//! growth replaced is never named by a cache slot at a safepoint**, except
//! where [`super::redefined`] later drops one of these clears because a
//! reload of the same slot post-dominates it across a window that neither
//! allocates nor calls — the conservative retention issue #514's step
//! (a)(ii) documents, and no more than it. An element write that reads the
//! cache slot and may kill its own key is followed by a clear of it, since
//! there the value was live into the kill. A clear on an edge into a join
//! runs on every way in, where that is cheaper than a block of its own: the
//! slot is dead there on all of them.
//!
//! # Where it runs
//!
//! After the expansion of small leaves, which is what puts a vector's reads
//! in the loops they are hot in, and before the passes that drop clears:
//! the clears this pass places, and the old scratch slot's clears it leaves
//! behind over a null word, are theirs to drop by their own rules.

use std::collections::HashMap;

use crate::inst::{Inst, Slot, Storage};
use crate::layout::{LayoutId, Shape};
use crate::program::{Function, Program, Table};
use crate::repr::{RefMap, Repr};

use super::frees::Flow;

/// Reuses loaded fields across every function of `program`.
pub(super) fn reuse_loaded_fields(program: &mut Program) {
    for index in 0..program.functions.len() {
        let Some(plan) = plan(&program.functions[index], program) else {
            continue;
        };
        let caches = plan.caches.clone();
        {
            let Program {
                functions, tables, ..
            } = &mut *program;
            let function = &mut functions[index];
            for (_, repr) in &plan.slots {
                function.reprs.push(*repr);
            }
            function.refs = RefMap::of(&function.reprs);
            plan.edit.apply(function, tables);
        }
        let Some(edit) = clears(&program.functions[index], program, &caches) else {
            continue;
        };
        let Program {
            functions, tables, ..
        } = &mut *program;
        edit.apply(&mut functions[index], tables);
    }
}

/// One owner slot, one field offset, one layout it is read at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    owner: Slot,
    at: u32,
    layout: LayoutId,
}

/// A cache slot and the key it holds.
#[derive(Clone, Copy, Debug)]
struct Cache {
    slot: Slot,
    key: Key,
}

/// What one function's rewrite is: the slots it appends, and the edit.
struct Plan {
    slots: Vec<(Slot, Repr)>,
    caches: Vec<Cache>,
    edit: Edit,
}

// ---------------------------------------------------------------------------
// A small bit set, one bit per key.

#[derive(Clone, PartialEq, Eq)]
struct Bits(Vec<u64>);

impl Bits {
    fn empty(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    fn full(n: usize) -> Bits {
        let mut bits = Bits(vec![u64::MAX; n.div_ceil(64)]);
        if !n.is_multiple_of(64) {
            if let Some(last) = bits.0.last_mut() {
                *last = (1u64 << (n % 64)) - 1;
            }
        }
        bits
    }

    fn get(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }

    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    fn unset(&mut self, i: usize) {
        self.0[i / 64] &= !(1 << (i % 64));
    }

    fn or(&mut self, other: &Bits) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a |= b;
        }
    }

    fn and(&mut self, other: &Bits) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a &= b;
        }
    }

    fn minus(&mut self, other: &Bits) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a &= !b;
        }
    }

    fn any(&self) -> bool {
        self.0.iter().any(|word| *word != 0)
    }

    fn assign(&mut self, other: &Bits) {
        self.0.copy_from_slice(&other.0);
    }

    fn fill(&mut self, value: bool) {
        self.0.fill(if value { u64::MAX } else { 0 });
    }

    fn trim(&mut self, n: usize) {
        if !n.is_multiple_of(64) {
            if let Some(last) = self.0.last_mut() {
                *last &= (1u64 << (n % 64)) - 1;
            }
        }
    }

    fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(at, word)| {
            (0..64)
                .filter(move |bit| word & (1 << bit) != 0)
                .map(move |bit| at * 64 + bit)
        })
    }
}

// ---------------------------------------------------------------------------
// The graph.

/// Successors and predecessors of every program counter. `ENTRY` is the
/// function's own entry, a predecessor of counter 0.
struct Graph {
    succs: Vec<Vec<usize>>,
    preds: Vec<Vec<usize>>,
}

const ENTRY: usize = usize::MAX;

impl Graph {
    fn of(flow: &Flow<'_>, len: usize) -> Graph {
        let mut succs = vec![Vec::new(); len];
        let mut preds = vec![Vec::new(); len];
        for (pc, out) in succs.iter_mut().enumerate() {
            flow.successors(pc, &mut |to| {
                if to < len && !out.contains(&to) {
                    out.push(to);
                }
            });
        }
        for (pc, out) in succs.iter().enumerate() {
            for &to in out {
                preds[to].push(pc);
            }
        }
        if len > 0 {
            preds[0].push(ENTRY);
        }
        Graph { succs, preds }
    }
}

/// The frame words each instruction reads and writes, asked of [`Flow`] once.
struct Access {
    reads: Vec<Vec<(Slot, u32)>>,
    /// The wide answer: every word a write may reach.
    writes: Vec<Vec<(Slot, u32)>>,
    /// The narrow answer: the words a write certainly reaches.
    certain: Vec<Vec<(Slot, u32)>>,
}

impl Access {
    fn of(code: &[Inst], flow: &Flow<'_>) -> Access {
        let mut access = Access {
            reads: Vec::with_capacity(code.len()),
            writes: Vec::with_capacity(code.len()),
            certain: Vec::with_capacity(code.len()),
        };
        for inst in code {
            let mut reads = Vec::new();
            flow.reads(inst, &mut |slot, width| reads.push((slot, width)));
            let mut writes = Vec::new();
            flow.writes(inst, true, &mut |slot, width| writes.push((slot, width)));
            let mut certain = Vec::new();
            flow.writes(inst, false, &mut |slot, width| certain.push((slot, width)));
            access.reads.push(reads);
            access.writes.push(writes);
            access.certain.push(certain);
        }
        access
    }
}

fn touches(runs: &[(Slot, u32)], slot: Slot) -> bool {
    runs.iter()
        .any(|&(base, width)| base <= slot && u64::from(slot) < u64::from(base) + u64::from(width))
}

// ---------------------------------------------------------------------------
// What an instruction may change.

/// What an instruction may do to the heap, as this pass classifies it.
enum Effect {
    /// Writes no heap word that existed before it, and calls nothing.
    Quiet,
    /// Writes elements of a run: an object whose payload is a sequence.
    Elements,
    /// Writes payload word `at` of the object in `obj`.
    Field { obj: Slot, at: u32 },
    /// May replace or rewrite the words of a growable owner of `storage` —
    /// which one, the storage is what narrows; `elements` if it also writes
    /// the elements of its store.
    Growable { storage: Storage, elements: bool },
    /// Anything else: a call, a park, a write through an address, or an
    /// instruction nobody has classified.
    Everything,
}

fn effect(inst: &Inst) -> Effect {
    match *inst {
        Inst::Unit { .. }
        | Inst::Bool { .. }
        | Inst::Int { .. }
        | Inst::FuncRef { .. }
        | Inst::Tag { .. }
        | Inst::Float { .. }
        | Inst::Str { .. }
        | Inst::Copy { .. }
        | Inst::Clear { .. }
        | Inst::Neg { .. }
        | Inst::Arith { .. }
        | Inst::Cmp { .. }
        | Inst::ArithImm { .. }
        | Inst::CmpImm { .. }
        | Inst::Not { .. }
        | Inst::Convert { .. }
        | Inst::FloatAbs { .. }
        | Inst::FloatMinMax { .. }
        | Inst::FloatRound { .. }
        | Inst::FloatSqrt { .. }
        | Inst::FloatTruncate { .. }
        | Inst::Bits { .. }
        | Inst::BitNot { .. }
        | Inst::Shift { .. }
        | Inst::Jump { .. }
        | Inst::BranchFalse { .. }
        | Inst::CmpBranch { .. }
        | Inst::CmpImmBranch { .. }
        | Inst::Switch { .. }
        | Inst::Alloc { .. }
        | Inst::LoadField { .. }
        | Inst::LoadElem { .. }
        | Inst::RunLoad { .. }
        | Inst::RunSlice { .. }
        | Inst::RunFind { .. }
        | Inst::GrowableAlloc { .. }
        | Inst::Len { .. }
        | Inst::AddrOfSlot { .. }
        | Inst::AddrOfField { .. }
        | Inst::AddrOfPart { .. }
        | Inst::Load { .. }
        | Inst::Box { .. }
        | Inst::Unbox { .. }
        | Inst::DynOpen { .. }
        | Inst::DynKind { .. }
        | Inst::DynSameType { .. }
        | Inst::DynSameObject { .. }
        | Inst::DynIdentitySet { .. }
        | Inst::DynNameOrder { .. }
        | Inst::DynRead { .. }
        | Inst::DynCase { .. }
        | Inst::DynCount { .. }
        | Inst::DynChild { .. }
        | Inst::HandleText { .. }
        | Inst::DynTypeName { .. }
        | Inst::DynFieldName { .. }
        | Inst::DynCaseName { .. }
        | Inst::DynOpaque { .. }
        | Inst::DynHandleText { .. }
        | Inst::DynOnPath { .. } => Effect::Quiet,
        // The end of the frame's run. Nothing after either reads a cache
        // slot, and at either the slot holds the field's current value,
        // which the owner slot beside it holds too.
        Inst::Return { .. } | Inst::Trap { .. } => Effect::Quiet,
        // An identity set's entry and leaving write the set's own frame
        // words, which the owner test sees, and its table — an object of its
        // own, whose words are a run — and may allocate a larger one. No
        // other object's words change.
        Inst::StoreElem { .. }
        | Inst::RunStore { .. }
        | Inst::RunCopy { .. }
        | Inst::DynIdentityEnter { .. }
        | Inst::DynIdentityLeave { .. } => Effect::Elements,
        Inst::StoreField { obj, at, .. } => Effect::Field { obj, at },
        Inst::GrowableEnsure { storage, .. } | Inst::GrowableCommit { storage, .. } => {
            Effect::Growable {
                storage,
                elements: false,
            }
        }
        Inst::GrowableTruncate { storage, .. } | Inst::RunFinish { storage, .. } => {
            Effect::Growable {
                storage,
                elements: true,
            }
        }
        _ => Effect::Everything,
    }
}

/// What the object in a slot may be, from the layout its writers agree on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Nothing is known: it may be any object.
    Unknown,
    /// A growable owner of words of this element layout.
    Words(LayoutId),
    /// A growable owner of bytes.
    Bytes,
    /// A run: an object whose payload is a sequence.
    Run,
    /// An object of this fixed layout and no other.
    Fixed(LayoutId),
}

fn class_of(program: &Program, layout: Option<LayoutId>) -> Class {
    let Some(layout) = layout else {
        return Class::Unknown;
    };
    let Some(held) = program.layouts.get(layout.index()) else {
        return Class::Unknown;
    };
    match held.shape {
        Shape::Vector { elem } => Class::Words(elem),
        Shape::ByteBuffer => Class::Bytes,
        Shape::Elements { .. }
        | Shape::Bytes
        | Shape::Str
        | Shape::Members { .. }
        | Shape::Entries { .. }
        | Shape::IdentityTable => Class::Run,
        Shape::Closure { .. } | Shape::Shared { .. } | Shape::Boxed => Class::Fixed(layout),
        // Inline families, and a bare reference word: the slot holds a
        // reference to something this layout does not describe.
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } | Shape::Free => Class::Unknown,
    }
}

impl Class {
    fn may_be_run(self) -> bool {
        matches!(self, Class::Unknown | Class::Run)
    }

    fn may_be_growable(self, storage: Storage) -> bool {
        match (self, storage) {
            (Class::Unknown, _) => true,
            (Class::Words(elem), Storage::Words(of)) => elem == of,
            (Class::Bytes, Storage::PackedBytes) => true,
            _ => false,
        }
    }

    fn may_alias(self, other: Class) -> bool {
        match (self, other) {
            (Class::Unknown, _) | (_, Class::Unknown) => true,
            (Class::Words(a), Class::Words(b)) => a == b,
            (Class::Bytes, Class::Bytes) | (Class::Run, Class::Run) => true,
            (Class::Fixed(a), Class::Fixed(b)) => a == b,
            _ => false,
        }
    }

    /// The `Repr` of payload word `at` of every object of this class, where
    /// that is a fact about the class: what makes a load of it safe to place
    /// where the program did not.
    fn word(self, at: u32) -> Option<Repr> {
        match (self, at) {
            (Class::Words(_) | Class::Bytes, 0) => Some(Repr::Int),
            (Class::Words(_) | Class::Bytes, 1) => Some(Repr::Ref),
            _ => None,
        }
    }
}

/// The layout each frame word's writers agree on, over the whole function.
fn slot_layouts(function: &Function, program: &Program, flow: &Flow<'_>) -> Vec<Option<LayoutId>> {
    // `None` not yet written, `Some(None)` written by disagreeing or unnamed
    // writers, `Some(Some(l))` written only with `l`.
    let size = flow.size;
    let mut seen: Vec<Option<Option<LayoutId>>> = vec![None; size];
    let agree =
        |seen: &mut Vec<Option<Option<LayoutId>>>, slot: usize, layout: Option<LayoutId>| {
            if slot >= size {
                return;
            }
            seen[slot] = match seen[slot] {
                None => Some(layout),
                Some(Some(was)) if Some(was) == layout => Some(Some(was)),
                Some(_) => Some(None),
            };
        };
    let one = |id: LayoutId| flow.width(id) == 1;
    let mut word = 0usize;
    for &param in &function.params {
        let width = flow.width(param) as usize;
        for at in 0..width {
            agree(
                &mut seen,
                word + at,
                if width == 1 { Some(param) } else { None },
            );
        }
        word += width;
    }
    for capture in &function.captures {
        let width = flow.width(capture.layout) as usize;
        for at in 0..width {
            let layout = if width == 1 {
                Some(capture.layout)
            } else {
                None
            };
            agree(&mut seen, capture.slot as usize + at, layout);
        }
    }
    let vector_of = |elem: LayoutId| {
        let mut found = program
            .layouts
            .iter()
            .enumerate()
            .filter(|(_, held)| held.shape == Shape::Vector { elem })
            .map(|(at, _)| LayoutId(at as u32));
        match (found.next(), found.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    };
    for inst in &function.code {
        let named: Option<(Slot, Option<LayoutId>)> = match *inst {
            // A clear writes null, which no load is asked about.
            Inst::Clear { .. } => continue,
            Inst::Copy { dst, layout, .. }
            | Inst::Load { dst, layout, .. }
            | Inst::LoadField { dst, layout, .. }
            | Inst::LoadElem { dst, layout, .. }
            | Inst::Unbox { dst, layout, .. }
            | Inst::Alloc { dst, layout, .. }
                if one(layout) =>
            {
                Some((dst, Some(layout)))
            }
            Inst::Call { dst, callee, .. } => program
                .functions
                .get(callee.index())
                .filter(|target| one(target.returns))
                .map(|target| (dst, Some(target.returns))),
            Inst::GrowableAlloc {
                dst,
                storage: Storage::Words(elem),
                ..
            } => Some((dst, vector_of(elem))),
            Inst::GrowableAlloc {
                dst,
                storage: Storage::PackedBytes,
                ..
            } => Some((dst, Some(program.buffer_layout))),
            Inst::Str { dst, .. } => Some((dst, Some(program.str_layout))),
            Inst::Box { dst, .. } => Some((dst, Some(program.boxed_layout))),
            _ => None,
        };
        match named {
            Some((dst, layout)) => agree(&mut seen, dst as usize, layout),
            None => flow.writes(inst, true, &mut |slot, width| {
                for at in slot as usize..slot as usize + width as usize {
                    agree(&mut seen, at, None);
                }
            }),
        }
    }
    seen.into_iter().map(|held| held.flatten()).collect()
}

// ---------------------------------------------------------------------------
// Planning.

/// The kills of every key at every counter, over `code`.
fn kills(code: &[Inst], access: &Access, keys: &[Key], classes: &[Class]) -> Vec<Bits> {
    let n = keys.len();
    code.iter()
        .enumerate()
        .map(|(pc, inst)| {
            let mut killed = Bits::empty(n);
            let written = &access.writes[pc];
            let effect = effect(inst);
            for (at, key) in keys.iter().enumerate() {
                let owner = key.owner as usize;
                let rewritten = touches(written, key.owner);
                let class = classes[owner];
                let kill = rewritten
                    || match effect {
                        Effect::Quiet => false,
                        Effect::Elements => class.may_be_run(),
                        Effect::Field { obj, at } => {
                            let other =
                                classes.get(obj as usize).copied().unwrap_or(Class::Unknown);
                            at == key.at && class.may_alias(other)
                        }
                        Effect::Growable {
                            storage, elements, ..
                        } => class.may_be_growable(storage) || (elements && class.may_be_run()),
                        Effect::Everything => true,
                    };
                if kill {
                    killed.set(at);
                }
            }
            killed
        })
        .collect()
}

/// Whether a local of `function`, or of a body expanded into it, names
/// `slot` at any counter in `lo..=hi`: where a debugger stopped would print
/// the word.
fn named_over(function: &Function, flow: &Flow<'_>, slot: Slot, lo: usize, hi: usize) -> bool {
    function
        .locals
        .iter()
        .chain(function.inlined.iter().flat_map(|held| &held.locals))
        .any(|local| {
            let words = local.slot..local.slot + flow.width(local.layout);
            words.contains(&slot) && (local.from as usize) <= hi && lo < local.to as usize
        })
}

/// `inst` with every read of `from` renamed to `to`, where every such read
/// is in an operand this pass can rename without touching a write.
fn renamed_reads(inst: &Inst, from: Slot, to: Slot) -> Option<Inst> {
    let swap = |slot: Slot| if slot == from { to } else { slot };
    let mut inst = inst.clone();
    match &mut inst {
        Inst::LoadField { dst, obj, .. } | Inst::Len { dst, obj } => {
            if *dst == from {
                return None;
            }
            *obj = swap(*obj);
        }
        Inst::LoadElem {
            dst, obj, index, ..
        } => {
            if *dst == from {
                return None;
            }
            *obj = swap(*obj);
            *index = swap(*index);
        }
        Inst::StoreElem {
            obj, index, src, ..
        } => {
            // A source is a value of the element's layout, which may be wider
            // than a word: it is not renamed, and a read of `from` there is
            // refused below.
            if *src == from {
                return None;
            }
            *obj = swap(*obj);
            *index = swap(*index);
        }
        Inst::RunLoad {
            dst, run, index, ..
        } => {
            if *dst == from {
                return None;
            }
            *run = swap(*run);
            *index = swap(*index);
        }
        Inst::RunStore {
            run, index, src, ..
        } => {
            *run = swap(*run);
            *index = swap(*index);
            *src = swap(*src);
        }
        Inst::Arith { dst, a, b, .. }
        | Inst::Cmp { dst, a, b, .. }
        | Inst::CmpBranch { dst, a, b, .. } => {
            if *dst == from {
                return None;
            }
            *a = swap(*a);
            *b = swap(*b);
        }
        Inst::ArithImm { dst, a, .. }
        | Inst::CmpImm { dst, a, .. }
        | Inst::CmpImmBranch { dst, a, .. }
        | Inst::Neg { dst, a, .. }
        | Inst::Not { dst, a } => {
            if *dst == from {
                return None;
            }
            *a = swap(*a);
        }
        _ => return None,
    }
    Some(inst)
}

/// Where the loads into each of `slots` reach, as one set on the way into
/// each counter. Slot `i`'s loads `loads[i]` are bits `base[i]..` in order,
/// and the bit after them stands for every other writer of the slot and for
/// the value it held on entry: what matters about those is only that they
/// reach. One analysis for every slot, rather than one a slot.
fn reaching(
    graph: &Graph,
    access: &Access,
    slots: &[Slot],
    loads: &[Vec<usize>],
) -> (Vec<Bits>, Vec<usize>) {
    let n = graph.succs.len();
    let mut base = Vec::with_capacity(slots.len());
    let mut count = 0;
    for held in loads {
        base.push(count);
        count += held.len() + 1;
    }
    // What each counter does: for each slot it writes, the bits of that slot
    // to clear if the write is certain, and the bit to set.
    let mut own: Vec<Vec<(usize, usize, usize, bool)>> = vec![Vec::new(); n];
    for (pc, writes) in own.iter_mut().enumerate() {
        for (at, &slot) in slots.iter().enumerate() {
            if touches(&access.writes[pc], slot) {
                let width = loads[at].len() + 1;
                let other = base[at] + loads[at].len();
                let certain = touches(&access.certain[pc], slot);
                let bit = loads[at]
                    .iter()
                    .position(|&load| load == pc)
                    .map_or(other, |index| base[at] + index);
                writes.push((base[at], width, bit, certain || bit != other));
            }
        }
    }
    let mut entry = Bits::empty(count);
    for (at, held) in loads.iter().enumerate() {
        entry.set(base[at] + held.len());
    }
    let mut into = vec![Bits::empty(count); n];
    let mut out = vec![Bits::empty(count); n];
    let mut arriving = Bits::empty(count);
    let mut leaving = Bits::empty(count);
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            arriving.fill(false);
            for &from in &graph.preds[pc] {
                if from == ENTRY {
                    arriving.or(&entry);
                } else {
                    arriving.or(&out[from]);
                }
            }
            leaving.assign(&arriving);
            for &(from, width, bit, certain) in &own[pc] {
                if certain {
                    for at in from..from + width {
                        leaving.unset(at);
                    }
                }
                leaving.set(bit);
            }
            if arriving != into[pc] || leaving != out[pc] {
                into[pc].assign(&arriving);
                out[pc].assign(&leaving);
                changed = true;
            }
        }
    }
    (into, base)
}

/// Whether each of `slots` is null on every path into each counter, as one
/// set a counter. None of them is written on entry.
fn null_into(code: &[Inst], graph: &Graph, access: &Access, slots: &[Slot]) -> Vec<Bits> {
    let n = code.len();
    let count = slots.len();
    // What each counter leaves each slot it writes as: null, or not known.
    let mut nulls = vec![Bits::empty(count); n];
    let mut unknowns = vec![Bits::empty(count); n];
    for pc in 0..n {
        let clear = matches!(code[pc], Inst::Clear { .. } | Inst::Unit { .. });
        for (at, &slot) in slots.iter().enumerate() {
            if touches(&access.writes[pc], slot) {
                if clear {
                    nulls[pc].set(at);
                } else {
                    unknowns[pc].set(at);
                }
            }
        }
    }
    let full = Bits::full(count);
    let mut into = vec![full.clone(); n];
    let mut out = vec![full.clone(); n];
    let mut arriving = Bits::empty(count);
    let mut leaving = Bits::empty(count);
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            arriving.assign(&full);
            for &from in &graph.preds[pc] {
                if from != ENTRY {
                    arriving.and(&out[from]);
                }
            }
            leaving.assign(&arriving);
            leaving.minus(&unknowns[pc]);
            leaving.or(&nulls[pc]);
            if arriving != into[pc] || leaving != out[pc] {
                into[pc].assign(&arriving);
                out[pc].assign(&leaving);
                changed = true;
            }
        }
    }
    into
}

/// Must-availability on the way into and out of each counter, with `gen`
/// and `kill`, where the keys in `assumed` are taken as available on the way
/// into a counter.
fn available(
    graph: &Graph,
    gen: &[Bits],
    kill: &[Bits],
    assumed: &[Bits],
    n_keys: usize,
) -> (Vec<Bits>, Vec<Bits>) {
    let n = gen.len();
    let mut into = vec![Bits::full(n_keys); n];
    let mut out = vec![Bits::full(n_keys); n];
    let mut arriving = Bits::empty(n_keys);
    let mut leaving = Bits::empty(n_keys);
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            arriving.fill(true);
            arriving.trim(n_keys);
            for &from in &graph.preds[pc] {
                if from == ENTRY {
                    arriving.fill(false);
                } else {
                    arriving.and(&out[from]);
                }
            }
            arriving.or(&assumed[pc]);
            leaving.assign(&arriving);
            leaving.minus(&kill[pc]);
            leaving.or(&gen[pc]);
            if arriving != into[pc] || leaving != out[pc] {
                into[pc].assign(&arriving);
                out[pc].assign(&leaving);
                changed = true;
            }
        }
    }
    (into, out)
}

/// The strongly connected component each counter is in, as an index: two
/// counters share one exactly when each can reach the other. A counter on no
/// cycle is a component of its own.
fn cycles(graph: &Graph) -> Vec<usize> {
    // Tarjan's algorithm, without recursion.
    let n = graph.succs.len();
    let unseen = usize::MAX;
    let mut index = vec![unseen; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut component = vec![unseen; n];
    let mut next_index = 0;
    let mut next_component = 0;
    for root in 0..n {
        if index[root] != unseen {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&mut (node, ref mut child)) = work.last_mut() {
            if *child == 0 && index[node] == unseen {
                index[node] = next_index;
                low[node] = next_index;
                next_index += 1;
                stack.push(node);
                on_stack[node] = true;
            }
            if let Some(&to) = graph.succs[node].get(*child) {
                *child += 1;
                if index[to] == unseen {
                    work.push((to, 0));
                } else if on_stack[to] {
                    low[node] = low[node].min(index[to]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = next_component;
                    if member == node {
                        break;
                    }
                }
                next_component += 1;
            }
        }
    }
    component
}

/// Partial availability on the way into each counter: some path from a load
/// reaches it without a kill.
fn partially_available(graph: &Graph, gen: &[Bits], kill: &[Bits], n_keys: usize) -> Vec<Bits> {
    let n = gen.len();
    let mut out = vec![Bits::empty(n_keys); n];
    let mut into = vec![Bits::empty(n_keys); n];
    let mut arriving = Bits::empty(n_keys);
    let mut leaving = Bits::empty(n_keys);
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            arriving.fill(false);
            for &from in &graph.preds[pc] {
                if from != ENTRY {
                    arriving.or(&out[from]);
                }
            }
            leaving.assign(&arriving);
            leaving.minus(&kill[pc]);
            leaving.or(&gen[pc]);
            if arriving != into[pc] || leaving != out[pc] {
                into[pc].assign(&arriving);
                out[pc].assign(&leaving);
                changed = true;
            }
        }
    }
    into
}

/// Where the owner slot of each key is non-null on every path out of each
/// counter, and on entry.
fn owners_non_null(
    function: &Function,
    program: &Program,
    code: &[Inst],
    graph: &Graph,
    flow: &Flow<'_>,
    access: &Access,
    owners: &[Slot],
) -> (Vec<Vec<bool>>, Vec<bool>) {
    let n = code.len();
    let params = function.param_words(&program.layouts) as usize;
    let entry: Vec<bool> = owners
        .iter()
        .map(|&owner| {
            let owner = owner as usize;
            owner < params
                || function.captures.iter().any(|capture| {
                    let from = capture.slot as usize;
                    from <= owner && owner < from + flow.width(capture.layout) as usize
                })
        })
        .collect();
    let mut out: Vec<Vec<bool>> = vec![vec![true; owners.len()]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut held: Vec<bool> = vec![true; owners.len()];
            for &from in &graph.preds[pc] {
                let edge = if from == ENTRY { &entry } else { &out[from] };
                for (at, value) in held.iter_mut().enumerate() {
                    *value &= edge[at];
                }
            }
            let fresh = matches!(
                code[pc],
                Inst::Alloc { .. }
                    | Inst::GrowableAlloc { .. }
                    | Inst::Call { .. }
                    | Inst::Copy { .. }
                    | Inst::Load { .. }
                    | Inst::LoadField { .. }
                    | Inst::LoadElem { .. }
                    | Inst::Unbox { .. }
            );
            for (at, &owner) in owners.iter().enumerate() {
                for &(base, width) in &access.writes[pc] {
                    if base <= owner && u64::from(owner) < u64::from(base) + u64::from(width) {
                        held[at] = fresh && width == 1;
                    }
                }
            }
            if held != out[pc] {
                out[pc] = held;
                changed = true;
            }
        }
    }
    (out, entry)
}

fn plan(function: &Function, program: &Program) -> Option<Plan> {
    let flow = Flow::of(function, program)?;
    let code = &function.code;
    let n = code.len();
    let size = flow.size;
    let params = function.param_words(&program.layouts) as usize;
    let entry_written = |slot: usize| {
        slot < params
            || function.captures.iter().any(|capture| {
                let from = capture.slot as usize;
                from <= slot && slot < from + flow.width(capture.layout) as usize
            })
    };

    // The loads, and the keys they read.
    let mut keys: Vec<Key> = Vec::new();
    let mut key_of: HashMap<Key, usize> = HashMap::new();
    let mut load: Vec<Option<(Slot, usize)>> = vec![None; n];
    let mut destinations: Vec<bool> = vec![false; size];
    for (pc, inst) in code.iter().enumerate() {
        let Inst::LoadField {
            dst,
            obj,
            at,
            layout,
        } = *inst
        else {
            continue;
        };
        let (d, o) = (dst as usize, obj as usize);
        if flow.width(layout) != 1
            || d == o
            || d >= size
            || o >= size
            || flow.addressed[d]
            || flow.addressed[o]
            || entry_written(d)
        {
            continue;
        }
        let key = Key {
            owner: obj,
            at,
            layout,
        };
        let index = *key_of.entry(key).or_insert_with(|| {
            keys.push(key);
            keys.len() - 1
        });
        load[pc] = Some((dst, index));
        destinations[d] = true;
    }
    // A key whose owner is itself some load's destination would be renamed
    // under itself.
    for held in load.iter_mut() {
        if held.is_some_and(|(_, key)| destinations[keys[key].owner as usize]) {
            *held = None;
        }
    }
    if load.iter().all(Option::is_none) {
        return None;
    }
    let graph = Graph::of(&flow, n);
    let access = Access::of(code, &flow);
    let layouts = slot_layouts(function, program, &flow);
    let classes: Vec<Class> = layouts
        .iter()
        .map(|held| class_of(program, *held))
        .collect();
    let n_keys = keys.len();
    let kill = kills(code, &access, &keys, &classes);

    // Nothing can go unless some load is reached by another of its key with
    // no kill between: asked first, and cheaply, of every load.
    let every: Vec<Bits> = (0..n)
        .map(|pc| {
            let mut bits = Bits::empty(n_keys);
            if let Some((_, key)) = load[pc] {
                bits.set(key);
            }
            bits
        })
        .collect();
    let reached = partially_available(&graph, &every, &kill, n_keys);
    let mut hot = Bits::empty(n_keys);
    for pc in 0..n {
        if let Some((_, key)) = load[pc] {
            if reached[pc].get(key) {
                hot.set(key);
            }
        }
    }
    if !hot.any() {
        return None;
    }
    // A key none of whose loads another reaches keeps its code: its loads
    // are not candidates, and cost the analysis below nothing.
    for held in load.iter_mut() {
        if held.is_some_and(|(_, key)| !hot.get(key)) {
            *held = None;
        }
    }

    // Which loads can become writes of their key's cache slot: the webs of
    // each destination.
    let mut renamed: Vec<bool> = vec![false; n];
    // For each counter, the reads to rename: (old slot, key).
    let mut reads: Vec<Vec<(Slot, usize)>> = vec![Vec::new(); n];
    let mut dsts: Vec<Slot> = load.iter().flatten().map(|(dst, _)| *dst).collect();
    dsts.sort_unstable();
    dsts.dedup();
    // The loads into each slot, as the definitions its webs are made of.
    let loads_into: Vec<Vec<usize>> = dsts
        .iter()
        .map(|&slot| {
            (0..n)
                .filter(|&pc| load[pc].is_some_and(|(dst, _)| dst == slot))
                .collect()
        })
        .collect();
    let (reached_by, base) = reaching(&graph, &access, &dsts, &loads_into);
    let nulls = null_into(code, &graph, &access, &dsts);
    for (index, &slot) in dsts.iter().enumerate() {
        let defs = &loads_into[index];
        let other = defs.len();
        let from = base[index];
        let into: Vec<Bits> = reached_by
            .iter()
            .map(|all| {
                let mut bits = Bits::empty(other + 1);
                for at in 0..=other {
                    if all.get(from + at) {
                        bits.set(at);
                    }
                }
                bits
            })
            .collect();
        let is_ref = function
            .reprs
            .get(slot as usize)
            .is_some_and(|repr| repr.is_ref());
        let null: Vec<bool> = nulls.iter().map(|held| held.get(index)).collect();
        let mut parent: Vec<usize> = (0..defs.len()).collect();
        fn find(parent: &mut [usize], at: usize) -> usize {
            let mut at = at;
            while parent[at] != at {
                parent[at] = parent[parent[at]];
                at = parent[at];
            }
            at
        }
        // A load into a reference slot that may still hold something would
        // leave that something rooted if it stopped writing the slot.
        let mut bad: Vec<bool> = defs.iter().map(|&pc| is_ref && !null[pc]).collect();
        let mut uses: Vec<(usize, usize)> = Vec::new();
        for pc in 0..n {
            if !touches(&access.reads[pc], slot) {
                continue;
            }
            let arriving: Vec<usize> = into[pc].ones().collect();
            let clean =
                !arriving.contains(&other) && renamed_reads(&code[pc], slot, slot).is_some();
            for &def in arriving.iter().filter(|&&def| def < other) {
                if !clean {
                    bad[def] = true;
                }
            }
            if let Some(&first) = arriving.first().filter(|&&def| def < other) {
                uses.push((pc, first));
            }
            for pair in arriving.windows(2).filter(|pair| pair[1] < other) {
                let (a, b) = (find(&mut parent, pair[0]), find(&mut parent, pair[1]));
                parent[a] = b;
            }
        }
        // A web is as bad as its worst definition, and as mixed as its keys;
        // and it is bad where a name is bound to the slot anywhere between its
        // first counter and its last — or anywhere at all, where a read comes
        // before a definition and the web goes round a loop.
        let mut span: HashMap<usize, (usize, usize, bool)> = HashMap::new();
        for (def, &pc) in defs.iter().enumerate() {
            let root = find(&mut parent, def);
            let held = span.entry(root).or_insert((pc, pc, false));
            held.0 = held.0.min(pc);
            held.1 = held.1.max(pc);
        }
        for &(pc, first) in &uses {
            let root = find(&mut parent, first);
            let def = defs[first];
            if let Some(held) = span.get_mut(&root) {
                held.0 = held.0.min(pc);
                held.1 = held.1.max(pc);
                held.2 |= pc <= def;
            }
        }
        let mut root_bad: HashMap<usize, bool> = HashMap::new();
        for (&root, &(lo, hi, round)) in &span {
            let (lo, hi) = if round { (0, n) } else { (lo, hi) };
            if named_over(function, &flow, slot, lo, hi) {
                root_bad.insert(root, true);
            }
        }
        let mut root_key: HashMap<usize, Option<usize>> = HashMap::new();
        for def in 0..defs.len() {
            let root = find(&mut parent, def);
            let key = load[defs[def]].map(|(_, key)| key);
            *root_bad.entry(root).or_insert(false) |= bad[def];
            let entry = root_key.entry(root).or_insert(key);
            if *entry != key {
                *entry = None;
            }
        }
        for def in 0..defs.len() {
            let root = find(&mut parent, def);
            if !root_bad[&root] && root_key[&root].is_some() {
                renamed[defs[def]] = true;
            }
        }
        for &(pc, first) in &uses {
            if renamed[defs[first]] {
                let key = load[defs[first]].map(|(_, key)| key).expect("a load");
                reads[pc].push((slot, key));
            }
        }
    }

    // Pinned loads: of an owner slot a reservation in the same block is
    // opened or committed on. The reservation rule's facts are about that
    // slot, in that block.
    let leaders = crate::flow::leaders_in(program, code);
    let mut pinned = vec![false; n];
    let mut start = 0;
    for pc in 0..=n {
        if pc == n || (pc > start && leaders[pc]) {
            let owners: Vec<Slot> = code[start..pc]
                .iter()
                .filter_map(|inst| match *inst {
                    Inst::GrowableEnsure { owner, .. } | Inst::GrowableCommit { owner, .. } => {
                        Some(owner)
                    }
                    _ => None,
                })
                .collect();
            for at in start..pc {
                if let Inst::LoadField { obj, .. } = code[at] {
                    pinned[at] = owners.contains(&obj);
                }
            }
            start = pc;
        }
    }

    // Every renamed read must find its key available on every path in —
    // loaded *into its own old slot* after the last kill — or the cache slot
    // could hold there a newer value than the old slot did. A web that fails
    // is not renamed: to a fixpoint, one pair of old slot and key at a time.
    loop {
        let mut pairs: Vec<(Slot, usize)> = (0..n)
            .filter(|&pc| renamed[pc])
            .filter_map(|pc| load[pc])
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        if pairs.is_empty() {
            break;
        }
        let pair_of: HashMap<(Slot, usize), usize> = pairs
            .iter()
            .enumerate()
            .map(|(at, pair)| (*pair, at))
            .collect();
        let count = pairs.len();
        let gen: Vec<Bits> = (0..n)
            .map(|pc| {
                let mut bits = Bits::empty(count);
                if renamed[pc] {
                    if let Some(pair) = load[pc] {
                        bits.set(pair_of[&pair]);
                    }
                }
                bits
            })
            .collect();
        let pair_kill: Vec<Bits> = (0..n)
            .map(|pc| {
                let mut bits = Bits::empty(count);
                for (at, &(_, key)) in pairs.iter().enumerate() {
                    if kill[pc].get(key) {
                        bits.set(at);
                    }
                }
                bits
            })
            .collect();
        let none = vec![Bits::empty(count); n];
        let (into, _) = available(&graph, &gen, &pair_kill, &none, count);
        let mut failed: Vec<(Slot, usize)> = Vec::new();
        for pc in 0..n {
            for read in &reads[pc] {
                match pair_of.get(read) {
                    Some(&at) if into[pc].get(at) => {}
                    _ => failed.push(*read),
                }
            }
        }
        if failed.is_empty() {
            break;
        }
        failed.sort_unstable();
        failed.dedup();
        for pair in failed {
            for pc in 0..n {
                if load[pc] == Some(pair) {
                    renamed[pc] = false;
                }
                reads[pc].retain(|&read| read != pair);
            }
        }
    }

    // The analysis over the renamed loads.
    let gen: Vec<Bits> = (0..n)
        .map(|pc| {
            let mut bits = Bits::empty(n_keys);
            if renamed[pc] {
                if let Some((_, key)) = load[pc] {
                    bits.set(key);
                }
            }
            bits
        })
        .collect();
    let used: Vec<Bits> = (0..n)
        .map(|pc| {
            let mut bits = gen[pc].clone();
            if pinned[pc] {
                bits = Bits::empty(n_keys);
            }
            bits
        })
        .collect();
    let pav_in = partially_available(&graph, &gen, &kill, n_keys);
    // Partial anticipation, backward.
    let mut pant_in = vec![Bits::empty(n_keys); n];
    let mut changed = true;
    while changed {
        changed = false;
        for pc in (0..n).rev() {
            let mut leaving = Bits::empty(n_keys);
            for &to in &graph.succs[pc] {
                leaving.or(&pant_in[to]);
            }
            leaving.minus(&kill[pc]);
            leaving.or(&used[pc]);
            if leaving != pant_in[pc] {
                pant_in[pc] = leaving;
                changed = true;
            }
        }
    }
    let mut region: Vec<Bits> = (0..n)
        .map(|pc| {
            let mut bits = pav_in[pc].clone();
            bits.and(&pant_in[pc]);
            bits
        })
        .collect();

    // A call, and anything else that may do anything, is a barrier for the
    // insertion inside a loop that makes one: a key a call took away on the
    // way round is not loaded again on an edge of that loop. The reload would
    // be paid on every turn that calls, and a loop that calls is one whose
    // next turn may as well call again before it reads — which nothing here
    // can count — so the load stays where the program made it. On the way
    // into such a loop, the edge is taken once, and the load is placed.
    let barrier: Vec<Bits> = code
        .iter()
        .map(|inst| {
            if matches!(effect(inst), Effect::Everything) {
                Bits::full(n_keys)
            } else {
                Bits::empty(n_keys)
            }
        })
        .collect();
    let after_call = partially_available(&graph, &barrier, &gen, n_keys);
    let cycle = cycles(&graph);
    // Which components hold a barrier. A component's index is below `n`.
    let mut calling = vec![false; n];
    for (pc, inst) in code.iter().enumerate() {
        if matches!(effect(inst), Effect::Everything) {
            calling[cycle[pc]] = true;
        }
    }
    let lost_to_a_call = |key: usize, from: usize, to: usize| {
        if from == ENTRY || cycle[from] != cycle[to] || !calling[cycle[from]] {
            return false;
        }
        barrier[from].get(key) || (after_call[from].get(key) && !gen[from].get(key))
    };

    let owners: Vec<Slot> = keys.iter().map(|key| key.owner).collect();
    let (non_null, entry_non_null) =
        owners_non_null(function, program, code, &graph, &flow, &access, &owners);
    let safe = |key: usize, from: usize| {
        let owner = keys[key].owner as usize;
        let word = classes[owner].word(keys[key].at);
        let repr = program
            .layouts
            .get(keys[key].layout.index())
            .and_then(|held| held.words.first().copied());
        let present = if from == ENTRY {
            entry_non_null[key]
        } else {
            non_null[from][key]
        };
        word.is_some() && word == repr && present
    };

    // Insert on every edge into the region that does not carry the key, and
    // take out of the region what cannot be inserted safely: to a fixpoint.
    let (inserted, into) = loop {
        let (into, out) = available(&graph, &gen, &kill, &region, n_keys);
        let mut inserted: HashMap<(usize, usize), Bits> = HashMap::new();
        let mut unsafe_at: Vec<(usize, usize)> = Vec::new();
        for (pc, wanted) in region.iter().enumerate() {
            if !wanted.any() {
                continue;
            }
            for &from in &graph.preds[pc] {
                let mut missing = wanted.clone();
                if from != ENTRY {
                    missing.minus(&out[from]);
                }
                for key in missing.ones().collect::<Vec<_>>() {
                    if safe(key, from) && !lost_to_a_call(key, from, pc) {
                        inserted
                            .entry((from, pc))
                            .or_insert_with(|| Bits::empty(n_keys))
                            .set(key);
                    } else {
                        unsafe_at.push((pc, key));
                    }
                }
            }
        }
        if unsafe_at.is_empty() {
            break (inserted, into);
        }
        for (pc, key) in unsafe_at {
            region[pc].unset(key);
        }
    };

    // The loads that go, and the keys that are worth a cache slot: the ones
    // that lose a load. A key that loses none keeps the code it had.
    let mut dropped = vec![false; n];
    let mut worth = Bits::empty(n_keys);
    for pc in 0..n {
        if let (true, false, Some((_, key))) = (renamed[pc], pinned[pc], load[pc]) {
            if into[pc].get(key) {
                dropped[pc] = true;
                worth.set(key);
            }
        }
    }
    if !worth.any() {
        return None;
    }
    let mut slots: Vec<(Slot, Repr)> = Vec::new();
    let mut caches: Vec<Cache> = Vec::new();
    let mut cache_of: HashMap<usize, Slot> = HashMap::new();
    for key in worth.ones() {
        let slot = (size + slots.len()) as Slot;
        let repr = program
            .layouts
            .get(keys[key].layout.index())
            .and_then(|held| held.words.first().copied())?;
        slots.push((slot, repr));
        caches.push(Cache {
            slot,
            key: keys[key],
        });
        cache_of.insert(key, slot);
    }
    if size + slots.len() > crate::MAX_FRAME_WORDS {
        return None;
    }

    // The renamed code.
    let mut working = code.clone();
    for pc in 0..n {
        if let (true, Some((_, key))) = (renamed[pc], load[pc]) {
            if let Some(&cache) = cache_of.get(&key) {
                if let Inst::LoadField { dst, .. } = &mut working[pc] {
                    *dst = cache;
                }
            } else {
                dropped[pc] = false;
            }
        } else {
            dropped[pc] = false;
        }
        for &(slot, key) in &reads[pc] {
            if let Some(&cache) = cache_of.get(&key) {
                working[pc] = renamed_reads(&working[pc], slot, cache).expect("checked renameable");
            }
        }
    }
    let mut edit = Edit::new(working);
    edit.dropped = dropped;
    // In a fixed order, so that a listing is the same from one run to the
    // next; and once at the join, rather than once an edge, where every way
    // into the join needs the same load.
    let mut edges: Vec<((usize, usize), Bits)> = inserted.into_iter().collect();
    edges.sort_by_key(|((from, to), _)| (*to, *from));
    let mut whole: HashMap<usize, Bits> = HashMap::new();
    for pc in 0..n {
        let mut every = Bits::full(n_keys);
        let mut any = false;
        for &from in &graph.preds[pc] {
            any = true;
            match edges.binary_search_by_key(&(pc, from), |((f, t), _)| (*t, *f)) {
                Ok(at) => every.and(&edges[at].1),
                Err(_) => every = Bits::empty(n_keys),
            }
        }
        if any && graph.preds[pc].len() > 1 && every.any() {
            whole.insert(pc, every);
        }
    }
    let load_of = |key: usize, cache: Slot| {
        let Key { owner, at, layout } = keys[key];
        Inst::LoadField {
            dst: cache,
            obj: owner,
            at,
            layout,
        }
    };
    for pc in 0..n {
        if let Some(every) = whole.get(&pc) {
            for key in every.ones() {
                if let Some(&cache) = cache_of.get(&key) {
                    edit.before[pc].push(load_of(key, cache));
                }
            }
        }
    }
    for ((from, to), keys_on) in edges {
        for key in keys_on.ones() {
            if whole.get(&to).is_some_and(|every| every.get(key)) {
                continue;
            }
            let Some(&cache) = cache_of.get(&key) else {
                continue;
            };
            edit.on_edge(&graph, from, to, load_of(key, cache), false);
        }
    }
    Some(Plan {
        slots,
        caches,
        edit,
    })
}

// ---------------------------------------------------------------------------
// Clearing a reference cache slot where it goes dead.

fn clears(function: &Function, program: &Program, caches: &[Cache]) -> Option<Edit> {
    let flow = Flow::of(function, program)?;
    let code = &function.code;
    let n = code.len();
    let graph = Graph::of(&flow, n);
    let layouts = slot_layouts(function, program, &flow);
    let classes: Vec<Class> = layouts
        .iter()
        .map(|held| class_of(program, *held))
        .collect();
    let refs: Vec<Cache> = caches
        .iter()
        .copied()
        .filter(|cache| {
            function
                .reprs
                .get(cache.slot as usize)
                .is_some_and(|repr| repr.is_ref())
        })
        .collect();
    if refs.is_empty() {
        return None;
    }
    let keys: Vec<Key> = refs.iter().map(|cache| cache.key).collect();
    let access = Access::of(code, &flow);
    let kill = kills(code, &access, &keys, &classes);
    let mut edit = Edit::new(code.clone());
    for (at, cache) in refs.iter().enumerate() {
        let slot = cache.slot;
        let touches = |inst: &Inst, reads: bool| {
            let mut hit = false;
            let mut probe = |base: Slot, width: u32| hit |= base <= slot && slot < base + width;
            if reads {
                flow.reads(inst, &mut probe);
            } else {
                // The narrow answer: a closure call's guessed width may reach
                // past the frame the cache slot was appended to, and what it
                // really writes never does.
                flow.writes(inst, false, &mut probe);
            }
            hit
        };
        let read: Vec<bool> = code.iter().map(|inst| touches(inst, true)).collect();
        let written: Vec<bool> = code.iter().map(|inst| touches(inst, false)).collect();
        let loaded: Vec<bool> = code
            .iter()
            .map(|inst| matches!(*inst, Inst::LoadField { dst, .. } if dst == slot))
            .collect();
        // Live: a read ahead before a write.
        let mut live = vec![false; n];
        // Needed: a kill ahead before a write.
        let mut needed = vec![false; n];
        let mut changed = true;
        while changed {
            changed = false;
            for pc in (0..n).rev() {
                let ahead_live = graph.succs[pc].iter().any(|&to| live[to]);
                let ahead_needed = graph.succs[pc].iter().any(|&to| needed[to]);
                let l = read[pc] || (ahead_live && !written[pc]);
                let k = kill[pc].get(at) || (ahead_needed && !written[pc]);
                if l != live[pc] || k != needed[pc] {
                    live[pc] = l;
                    needed[pc] = k;
                    changed = true;
                }
            }
        }
        // Edges that carry a clear, grown until no non-null dead value can
        // reach a kill.
        let mut cleared: Vec<(usize, usize)> = Vec::new();
        // An instruction that reads the slot and may kill its key leaves the
        // value it read stale: cleared straight after.
        for pc in 0..n {
            if read[pc] && kill[pc].get(at) {
                for &to in &graph.succs[pc] {
                    cleared.push((pc, to));
                }
            }
        }
        loop {
            // May be non-null, forward.
            let mut out = vec![false; n];
            let mut changed = true;
            while changed {
                changed = false;
                for pc in 0..n {
                    let arriving = graph.preds[pc]
                        .iter()
                        .any(|&from| from != ENTRY && out[from] && !cleared.contains(&(from, pc)));
                    let leaving = if written[pc] { loaded[pc] } else { arriving };
                    if leaving != out[pc] {
                        out[pc] = leaving;
                        changed = true;
                    }
                }
            }
            let mut wanted: Vec<(usize, usize)> = Vec::new();
            let mut leftover: Vec<(usize, usize)> = Vec::new();
            for pc in 0..n {
                if !out[pc] {
                    continue;
                }
                for &to in &graph.succs[pc] {
                    if live[to] || !needed[to] || cleared.contains(&(pc, to)) {
                        continue;
                    }
                    if live[pc] || loaded[pc] {
                        wanted.push((pc, to));
                    } else {
                        leftover.push((pc, to));
                    }
                }
            }
            if wanted.is_empty() {
                wanted = leftover;
            }
            if wanted.is_empty() {
                break;
            }
            cleared.extend(wanted);
        }
        let layout = cache.key.layout;
        for (from, to) in cleared {
            edit.on_edge(&graph, from, to, Inst::Clear { slot, layout }, true);
        }
    }
    Some(edit)
}

// ---------------------------------------------------------------------------
// Editing a function: instructions dropped, and instructions placed on edges.

struct Edit {
    code: Vec<Inst>,
    dropped: Vec<bool>,
    /// Run by every way into the counter, before it.
    before: Vec<Vec<Inst>>,
    /// Run only by the fall from the counter before, ahead of `before`.
    fall: Vec<Vec<Inst>>,
    /// A block of its own at the end, reached from `from` in place of `to`.
    trampolines: Vec<(usize, usize, Vec<Inst>)>,
}

impl Edit {
    fn new(code: Vec<Inst>) -> Edit {
        let n = code.len();
        Edit {
            code,
            dropped: vec![false; n],
            before: vec![Vec::new(); n],
            fall: vec![Vec::new(); n],
            trampolines: Vec::new(),
        }
    }

    /// Places `inst` on the edge `from -> to`. `anywhere` says it may run on
    /// every way into `to` as well, which is cheaper than a block of its own
    /// where the edge is a branch's into a join.
    fn on_edge(&mut self, graph: &Graph, from: usize, to: usize, inst: Inst, anywhere: bool) {
        let only = graph.preds[to].len() == 1;
        if from == ENTRY {
            if only {
                self.before[to].push(inst);
            } else {
                self.fall[to].push(inst);
            }
            return;
        }
        if only {
            self.before[to].push(inst);
            return;
        }
        let jumps = matches!(self.code[from], Inst::Jump { .. });
        if jumps {
            self.before[from].push(inst);
            return;
        }
        let mut taken = false;
        match self.code[from] {
            Inst::BranchFalse { to: target, .. }
            | Inst::CmpBranch { target, .. }
            | Inst::CmpImmBranch { target, .. } => taken = target as usize == to,
            Inst::Switch { .. } => taken = true,
            _ => {}
        }
        if !taken && to == from + 1 {
            self.fall[to].push(inst);
            return;
        }
        if anywhere {
            // Another edge into the join may have placed it already.
            if !self.before[to].contains(&inst) {
                self.before[to].push(inst);
            }
            return;
        }
        if taken && to == from + 1 {
            // Both ways out of `from` land here: whoever else arrives, the
            // instruction is for all of this branch's ways.
            self.fall[to].push(inst.clone());
        }
        if let Some((_, _, held)) = self
            .trampolines
            .iter_mut()
            .find(|(at, into, _)| *at == from && *into == to)
        {
            held.push(inst);
        } else {
            self.trampolines.push((from, to, vec![inst]));
        }
    }

    /// Writes the edit into `function`, moving every counter that named an
    /// instruction.
    fn apply(self, function: &mut Function, tables: &mut [Table]) {
        let n = self.code.len();
        let old_spans = function.spans.clone();
        // Where each counter's own instructions begin, and where the fall
        // into it begins.
        let mut code: Vec<Inst> = Vec::new();
        let mut spans = Vec::new();
        let mut first = vec![0usize; n + 1];
        let mut fall_first = vec![0usize; n + 1];
        let mut origin: Vec<Option<usize>> = Vec::new();
        for pc in 0..n {
            fall_first[pc] = code.len();
            for inst in &self.fall[pc] {
                code.push(inst.clone());
                spans.push(old_spans[pc]);
                origin.push(None);
            }
            first[pc] = code.len();
            for inst in &self.before[pc] {
                code.push(inst.clone());
                spans.push(old_spans[pc]);
                origin.push(None);
            }
            if !self.dropped[pc] {
                code.push(self.code[pc].clone());
                spans.push(old_spans[pc]);
                origin.push(Some(pc));
            }
        }
        let end = code.len();
        fall_first[n] = end;
        first[n] = end;
        // A target that named a dropped instruction with nothing placed
        // before it lands where the fall out of it goes.
        let mut entry = vec![end; n + 1];
        for pc in 0..n {
            let own = !self.before[pc].is_empty() || !self.dropped[pc];
            entry[pc] = if own { first[pc] } else { fall_first[pc + 1] };
        }
        let mut tramp_at: HashMap<(usize, usize), usize> = HashMap::new();
        let mut tail: Vec<Inst> = Vec::new();
        let mut tail_spans = Vec::new();
        for (from, to, held) in &self.trampolines {
            tramp_at.insert((*from, *to), end + tail.len());
            for inst in held {
                tail.push(inst.clone());
                tail_spans.push(old_spans[*to]);
            }
            tail.push(Inst::Jump {
                to: entry[*to] as u32,
            });
            tail_spans.push(old_spans[*to]);
        }
        let target = |from: Option<usize>, to: u32| -> u32 {
            if let Some(from) = from {
                if let Some(&at) = tramp_at.get(&(from, to as usize)) {
                    return at as u32;
                }
            }
            entry[to as usize] as u32
        };
        for (at, inst) in code.iter_mut().enumerate() {
            let from = origin[at];
            match inst {
                Inst::Jump { to } | Inst::BranchFalse { to, .. } => *to = target(from, *to),
                Inst::CmpBranch { target: to, .. } | Inst::CmpImmBranch { target: to, .. } => {
                    *to = target(from, *to)
                }
                Inst::Switch { table, .. } => {
                    let table = &mut tables[table.index()];
                    for to in &mut table.targets {
                        *to = target(from, *to);
                    }
                    table.default = target(from, table.default);
                }
                _ => {}
            }
        }
        code.extend(tail);
        spans.extend(tail_spans);
        let moved = |pc: u32| -> u32 {
            let pc = pc as usize;
            if pc >= n {
                end as u32
            } else {
                entry[pc] as u32
            }
        };
        function.code = code;
        function.spans = spans;
        for local in &mut function.locals {
            local.from = moved(local.from);
            local.to = moved(local.to);
        }
        for held in &mut function.inlined {
            held.from = moved(held.from);
            held.to = moved(held.to);
            for local in &mut held.locals {
                local.from = moved(local.from);
                local.to = moved(local.to);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::layout::Layout;
    use crate::repr::RefMap;
    use crate::FunctionId;
    use cove_diag::Span;

    /// An `Int`, a bare reference, a `Vector<Int>`, a `String` and a
    /// `Vector<String>`.
    const INT: LayoutId = LayoutId(0);
    const REF: LayoutId = LayoutId(1);
    const INTS: LayoutId = LayoutId(2);
    const STR: LayoutId = LayoutId(3);
    const STRS: LayoutId = LayoutId(4);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::word("<ref>", Repr::Ref),
            Layout::object("Vector<Int>", Shape::Vector { elem: INT }),
            Layout::object("String", Shape::Str),
            Layout::object("Vector<String>", Shape::Vector { elem: STR }),
        ]
    }

    fn span() -> Span {
        Span::new(cove_diag::FileId(0), 0, 0)
    }

    /// Slots 0 and 1 two `Vector<Int>` parameters, which may be one vector;
    /// slot 2 a `Vector<String>` parameter; slot 3 the scratch word a store
    /// is read into; 4 an index; 5 an element; 6 a condition; 7 a count.
    fn function(code: Vec<Inst>) -> Function {
        let reprs = vec![
            Repr::Ref,
            Repr::Ref,
            Repr::Ref,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Bool,
            Repr::Int,
        ];
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: vec![INTS, INTS, STRS],
            spans: (0..code.len()).map(|_| span()).collect(),
            refs: RefMap::of(&reprs),
            reprs,
            returns: INT,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program(code: Vec<Inst>) -> Program {
        Program {
            functions: vec![function(code)],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            args: vec![Vec::new()],
            str_layout: STR,
            ..Program::default()
        }
    }

    /// What the pass makes of `code`, alone, and the frame it leaves.
    fn ran(code: Vec<Inst>) -> (Vec<Inst>, usize) {
        let mut program = program(code);
        reuse_loaded_fields(&mut program);
        let function = program.function(FunctionId(0));
        (function.code.clone(), function.reprs.len())
    }

    /// The first cache slot the pass appends to [`function`]'s frame.
    const C: Slot = 8;

    /// `dst = owner`'s store word.
    fn store(dst: Slot, owner: Slot) -> Inst {
        Inst::LoadField {
            dst,
            obj: owner,
            at: 1,
            layout: REF,
        }
    }

    /// `s5 = store[s4]`.
    fn elem(store: Slot) -> Inst {
        Inst::LoadElem {
            dst: 5,
            obj: store,
            index: 4,
            layout: INT,
        }
    }

    fn clear(slot: Slot) -> Inst {
        Inst::Clear { slot, layout: REF }
    }

    fn ensure(owner: Slot, storage: Storage) -> Inst {
        Inst::GrowableEnsure {
            owner,
            additional: 7,
            storage,
        }
    }

    fn done() -> Inst {
        Inst::Return { src: 5 }
    }

    fn branch(to: u32) -> Inst {
        Inst::BranchFalse { cond: 6, to }
    }

    /// A read the way the lowering writes one: the store into the scratch
    /// word, the element out of it, the scratch word cleared.
    fn read(owner: Slot) -> [Inst; 3] {
        [store(3, owner), elem(3), clear(3)]
    }

    /// A load both arms of a branch leave available is not made again at
    /// the join, nor in either arm: the first load's value is read by every
    /// later use. The scratch word's clears stay — they clear a word that is
    /// null now, which `nulls` drops later.
    #[test]
    fn a_load_both_arms_keep_available_is_reused_at_the_join() {
        let mut code = Vec::new();
        code.extend(read(0));
        code.push(branch(8));
        code.extend(read(0));
        code.push(Inst::Jump { to: 11 });
        code.extend(read(0));
        code.extend(read(0));
        code.push(done());
        let (code, frame) = ran(code);
        assert_eq!(frame, 9, "one cache slot");
        assert_eq!(
            code,
            [
                store(C, 0),
                elem(C),
                clear(3),
                branch(7),
                elem(C),
                clear(3),
                Inst::Jump { to: 9 },
                elem(C),
                clear(3),
                elem(C),
                clear(3),
                done(),
            ]
        );
    }

    /// A load available on one arm only is inserted on the other, and the
    /// one at the join goes: the join is then available on both.
    #[test]
    fn a_load_available_on_one_arm_is_inserted_on_the_other() {
        let mut code = vec![branch(4)];
        code.extend(read(0));
        code.extend(read(0));
        code.push(done());
        let (code, _) = ran(code);
        assert_eq!(
            code,
            [
                branch(7),
                store(C, 0),
                elem(C),
                clear(3),
                elem(C),
                clear(3),
                done(),
                // The branch's own way into the join, which the arm that
                // loaded does not share.
                store(C, 0),
                Inst::Jump { to: 4 },
            ]
        );
    }

    /// A loop that reads a store nothing in it can replace reads it once,
    /// before the loop, on the way in and not on the back edge.
    #[test]
    fn a_load_a_loop_cannot_change_is_made_once_before_it() {
        let mut code = vec![Inst::Int { dst: 4, value: 0 }, branch(6)];
        code.extend(read(0));
        code.push(Inst::Jump { to: 1 });
        code.push(done());
        let (code, _) = ran(code);
        assert_eq!(
            code,
            [
                Inst::Int { dst: 4, value: 0 },
                store(C, 0),
                branch(6),
                elem(C),
                clear(3),
                Inst::Jump { to: 2 },
                done(),
            ]
        );
    }

    /// A kill inside the loop — an ensure that may replace the store — is
    /// followed by a reload where the loop's two ways meet again, so the
    /// turns that did not grow read nothing. The cache slot is cleared on the
    /// way into the arm that grows, where it stops being read: it names no
    /// store the ensure replaced at any point the ensure can collect or
    /// after.
    #[test]
    fn a_kill_inside_a_loop_is_followed_by_a_reload() {
        let mut code = vec![branch(8)];
        code.extend(read(0));
        code.push(branch(7));
        code.push(Inst::Int { dst: 7, value: 1 });
        code.push(ensure(0, Storage::Words(INT)));
        code.push(Inst::Jump { to: 0 });
        code.push(done());
        let (code, _) = ran(code);
        assert_eq!(
            code,
            [
                store(C, 0),
                branch(10),
                elem(C),
                clear(3),
                branch(9),
                clear(C),
                Inst::Int { dst: 7, value: 1 },
                ensure(0, Storage::Words(INT)),
                store(C, 0),
                Inst::Jump { to: 1 },
                done(),
            ]
        );
    }

    /// Two slots of one vector layout may hold one vector, so an ensure
    /// through the second kills what was loaded through the first, and the
    /// first is read again. An ensure on a vector of another element type
    /// cannot be the same object, and kills nothing.
    #[test]
    fn a_growth_through_an_alias_of_the_same_layout_reloads() {
        let mut code = Vec::new();
        code.extend(read(0));
        code.push(ensure(1, Storage::Words(INT)));
        code.extend(read(0));
        code.push(done());
        let (aliased, _) = ran(code.clone());
        assert_eq!(
            aliased, code,
            "nothing is reused across the ensure, so nothing changes"
        );
        let mut code = Vec::new();
        code.extend(read(0));
        code.push(ensure(2, Storage::Words(STR)));
        code.extend(read(0));
        code.push(done());
        let (apart, _) = ran(code);
        assert_eq!(
            apart,
            [
                store(C, 0),
                elem(C),
                clear(3),
                ensure(2, Storage::Words(STR)),
                elem(C),
                clear(3),
                done(),
            ]
        );
    }

    /// A call may do anything, so it kills every key, and the cache slot is
    /// cleared before it rather than holding a store across it.
    #[test]
    fn a_call_kills_and_the_cache_slot_is_null_across_it() {
        let call = Inst::Call {
            dst: 7,
            callee: FunctionId(0),
            args: crate::ArgsId(0),
        };
        let mut code = Vec::new();
        code.extend(read(0));
        code.extend(read(0));
        code.push(call.clone());
        code.extend(read(0));
        code.push(done());
        let (code, _) = ran(code);
        assert_eq!(
            code,
            [
                store(C, 0),
                elem(C),
                clear(3),
                elem(C),
                clear(C),
                clear(3),
                call,
                store(C, 0),
                elem(C),
                clear(3),
                done(),
            ]
        );
    }

    /// A trap ends the frame's run: nothing carries across it, because
    /// there is no way on from it. The load after the branch around a trap
    /// finds the value the way that did not trap brought it.
    #[test]
    fn nothing_carries_across_a_trap() {
        let trap = Inst::Trap {
            message: 3,
            rule: 3,
            help: 3,
        };
        let mut code = Vec::new();
        code.extend(read(0));
        code.push(branch(5));
        code.push(trap.clone());
        code.extend(read(0));
        code.push(done());
        let (code, _) = ran(code);
        assert_eq!(
            code,
            [
                store(C, 0),
                elem(C),
                clear(3),
                branch(5),
                trap,
                elem(C),
                clear(3),
                done(),
            ]
        );
        assert!(
            matches!(
                effect(&Inst::Trap {
                    message: 0,
                    rule: 0,
                    help: 0
                }),
                Effect::Quiet
            ),
            "a trap is a terminator: it has no successor to carry anything to"
        );
    }

    /// A loop whose turns may call is not given a reload after the call: the
    /// next turn may call again before it reads, and nothing here can count
    /// which. The loop's own load stays where the program made it.
    #[test]
    fn a_call_inside_a_loop_is_a_barrier_to_the_reload() {
        let call = Inst::Call {
            dst: 7,
            callee: FunctionId(0),
            args: crate::ArgsId(0),
        };
        let mut code = vec![branch(7)];
        code.extend(read(0));
        code.push(branch(6));
        code.push(call);
        code.push(Inst::Jump { to: 0 });
        code.push(done());
        let (after, frame) = ran(code.clone());
        assert_eq!(frame, 8, "nothing reused, no slot");
        assert_eq!(after, code);
    }

    /// A call before a loop is not inside it: the way into the loop is taken
    /// once, and the load the loop makes on every turn is made there instead.
    #[test]
    fn a_call_before_a_loop_does_not_keep_the_load_in_it() {
        let call = Inst::Call {
            dst: 7,
            callee: FunctionId(0),
            args: crate::ArgsId(0),
        };
        let mut code = vec![call.clone(), Inst::Int { dst: 4, value: 0 }, branch(7)];
        code.extend(read(0));
        code.push(Inst::Jump { to: 2 });
        code.push(done());
        let (after, _) = ran(code);
        assert_eq!(
            after,
            [
                call,
                Inst::Int { dst: 4, value: 0 },
                store(C, 0),
                branch(7),
                elem(C),
                clear(3),
                Inst::Jump { to: 3 },
                done(),
            ]
        );
    }

    /// Every call, and anything not classified, kills every key.
    #[test]
    fn every_kind_of_call_is_classified_as_a_kill_of_everything() {
        let calls = [
            Inst::Call {
                dst: 0,
                callee: FunctionId(0),
                args: crate::ArgsId(0),
            },
            Inst::CallClosure {
                dst: 0,
                closure: 1,
                args: crate::ArgsId(0),
                result: INT,
            },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: INT,
            },
            Inst::AssertFailed { message: 0 },
        ];
        for call in calls {
            assert!(matches!(effect(&call), Effect::Everything), "{call:?}");
        }
    }

    /// A load whose owner slot is written between two reads is a different
    /// object's field, and is loaded again.
    #[test]
    fn a_write_of_the_owner_slot_kills() {
        let mut code = Vec::new();
        code.extend(read(0));
        code.push(Inst::Copy {
            dst: 0,
            src: 1,
            layout: INTS,
        });
        code.extend(read(0));
        code.push(done());
        let (after, frame) = ran(code.clone());
        assert_eq!(frame, 8, "nothing to reuse, no slot");
        assert_eq!(after, code);
    }

    /// The pass reads the code and nothing that names it: no function, no
    /// module, no string a program wrote. Checked on its own source, up to
    /// these tests.
    #[test]
    fn the_pass_names_no_function_or_module() {
        let source = include_str!("loads.rs");
        let body = &source[..source.find("#[cfg(test)]").expect("a test module")];
        let code: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for named in [
            ".name",
            ".module",
            "qualified",
            "function_named",
            "\"std",
            "Arc<str>",
            "strings",
        ] {
            assert!(!code.contains(named), "the pass reads `{named}`");
        }
    }
}
