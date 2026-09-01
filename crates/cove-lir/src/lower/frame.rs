//! Where a value lives while a function runs.
//!
//! There is one numbering, and everything a body needs a word for is in it:
//! the parameters first, in declaration order, then the return word, then
//! every local and every temporary. That is ADR 0034's single slot space
//! taken as the only place a value can be, which is what removes the
//! question a predecessor with several stacks had to answer at every step —
//! *which* store is this value in.
//!
//! # A slot is reused, and only by its own kind
//!
//! A long body mentions far more temporaries than it holds at once. If each
//! took a slot of its own the frame would grow with the source rather than
//! with what is live, and a frame is what a call costs. So a temporary is
//! given back when it dies and handed to the next value that asks — but only
//! to one of the same [`Repr`], because [`crate::RefMap`] is one bit per slot
//! for the whole function and a slot that changed kind would make no single
//! bit right at every program counter.
//!
//! The free list is therefore per `Repr` and the invariant is structural: a
//! slot is only ever taken from the list its own kind is on, so no
//! bookkeeping mistake can put a reference where the map says there is an
//! integer.
//!
//! # Ownership answers when a slot dies
//!
//! An expression's answer is either a temporary it made — which its consumer
//! gives back — or a slot that belongs to something else, such as a
//! parameter or a local. [`Val`] carries which, because freeing a binding's
//! slot at the end of the expression that read it would let a later
//! temporary overwrite a variable that is still in scope.
//!
//! A local's slot is owned by the scope that declared it, and released when
//! that scope ends. That is the same event the heap task needs for
//! [`crate::Inst::Clear`], which is why the scope answers a list of slots
//! rather than quietly dropping them.

use std::collections::HashMap;

use crate::{Repr, Slot};

/// Where an expression left its answer.
pub(crate) struct Val {
    pub slot: Slot,
    /// Whether the consumer of this value should give the slot back.
    ///
    /// A temporary is the expression's own and dies with its last use. A
    /// borrowed slot — a parameter, a local, the return word — outlives the
    /// expression that read it, and giving one back would hand a live
    /// binding to the next temporary that asked.
    pub temp: bool,
}

impl Val {
    /// A temporary this expression allocated.
    pub fn temp(slot: Slot) -> Val {
        Val { slot, temp: true }
    }

    /// A slot that belongs to something longer-lived.
    pub fn borrowed(slot: Slot) -> Val {
        Val { slot, temp: false }
    }
}

/// One lexical scope: the names it introduced and the slots it must give
/// back when it ends.
#[derive(Default)]
struct Scope {
    /// Pushed in declaration order and searched backwards, so a shadowing
    /// declaration wins without the earlier one having to be removed — and
    /// the earlier one's slot stays owned by this scope, which is what a
    /// shadowed binding still needs.
    names: Vec<(String, Slot)>,
    owned: Vec<Slot>,
}

pub(crate) struct Frame {
    reprs: Vec<Repr>,
    free: HashMap<Repr, Vec<Slot>>,
    scopes: Vec<Scope>,
}

impl Frame {
    pub fn new() -> Frame {
        Frame {
            reprs: Vec::new(),
            free: HashMap::new(),
            scopes: Vec::new(),
        }
    }

    /// What each slot of the frame holds, which is what
    /// [`crate::Function::reprs`] is.
    pub fn reprs(&self) -> &[Repr] {
        &self.reprs
    }

    pub fn repr(&self, slot: Slot) -> Repr {
        self.reprs[slot as usize]
    }

    /// A slot that is never given back: a parameter.
    ///
    /// Parameters are slots `0..arity` and the caller writes into them, so
    /// they are taken before anything else asks and never returned to a free
    /// list.
    pub fn param(&mut self, repr: Repr) -> Slot {
        self.push(repr)
    }

    /// A slot holding one value of `repr`, reusing a dead one when there is
    /// one of the same kind.
    pub fn alloc(&mut self, repr: Repr) -> Slot {
        match self.free.get_mut(&repr).and_then(Vec::pop) {
            Some(slot) => slot,
            None => self.push(repr),
        }
    }

    fn push(&mut self, repr: Repr) -> Slot {
        self.reprs.push(repr);
        (self.reprs.len() - 1) as Slot
    }

    /// Gives a slot back to the list its kind draws from.
    pub fn free(&mut self, slot: Slot) {
        let repr = self.repr(slot);
        self.free.entry(repr).or_default().push(slot);
    }

    /// Gives back a value's slot if it was the expression's own.
    pub fn release(&mut self, val: Val) {
        if val.temp {
            self.free(val.slot);
        }
    }

    /// How many scopes are open. A loop records this so `break` knows how
    /// many it is leaving.
    pub fn depth(&self) -> usize {
        self.scopes.len()
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    /// Ends the innermost scope, answering the reference slots it owned.
    ///
    /// The slots go back on the free lists here; the answer is what the
    /// caller must emit [`crate::Inst::Clear`] for, because a static
    /// reference map cannot say when a value stopped being needed.
    pub fn pop_scope(&mut self) -> Vec<Slot> {
        let scope = self.scopes.pop().expect("a scope is open");
        let mut clears = Vec::new();
        for slot in scope.owned {
            if matches!(self.repr(slot), Repr::Ref | Repr::Addr) {
                clears.push(slot);
            }
            self.free(slot);
        }
        clears
    }

    /// The reference slots the scopes inside `depth` own.
    ///
    /// This is what a `break` or a `continue` has to clear: it leaves those
    /// scopes without ending them, and the loop it jumps to or out of goes on
    /// running, so a reference left behind would be retained for the rest of
    /// the frame rather than for the rest of the turn.
    pub fn refs_within(&self, depth: usize) -> Vec<Slot> {
        self.scopes[depth..]
            .iter()
            .flat_map(|scope| scope.owned.iter().copied())
            .filter(|slot| matches!(self.repr(*slot), Repr::Ref | Repr::Addr))
            .collect()
    }

    /// Names `slot` in the innermost scope.
    pub fn bind(&mut self, name: &str, slot: Slot) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .names
            .push((name.to_string(), slot));
    }

    /// Makes the innermost scope responsible for giving `slot` back.
    pub fn own(&mut self, slot: Slot) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .owned
            .push(slot);
    }

    /// The slot `name` denotes, searching inwards out.
    pub fn lookup(&self, name: &str) -> Option<Slot> {
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .names
                .iter()
                .rev()
                .find(|(bound, _)| bound == name)
                .map(|(_, slot)| *slot)
        })
    }
}
