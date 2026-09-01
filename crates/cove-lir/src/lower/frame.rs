//! Where a value lives while a function runs.
//!
//! There is one numbering, and everything a body needs words for is in it:
//! the parameters first, in declaration order, then the answer, then every
//! local and every temporary. That is ADR 0034's single slot space taken as
//! the only place a value can be.
//!
//! # A value location is a base slot and a layout
//!
//! One slot is one word, and one value may occupy several consecutive ones.
//! So the frame allocates a *run*: [`Frame::alloc`] is handed the words a
//! layout describes and answers the slot the first of them is at.
//!
//! # A run is reused, and only by one with the same words
//!
//! A long body mentions far more temporaries than it holds at once. If each
//! took slots of its own the frame would grow with the source rather than
//! with what is live, and a frame is what a call costs. So a run is given
//! back when it dies and handed to the next value that asks — but only to
//! one whose words are the *same, in the same order*, because
//! [`crate::RefMap`] is one bit per slot for the whole function and a slot
//! that changed kind would make no single bit right at every program
//! counter.
//!
//! The free list is therefore keyed by the run's words and the invariant is
//! structural: a run is only ever taken from the list its own shape is on,
//! so no bookkeeping mistake can put a reference where the map says there is
//! an integer. Two locations of the same width whose words differ — a
//! `[Int, Ref]` and a `[Ref, Int]` — never share a run.
//!
//! # Ownership answers when a location dies
//!
//! An expression's answer is either a temporary it made — which its consumer
//! gives back — or a location that belongs to something else, such as a
//! parameter, a local, or a field of one. [`Val`] carries which, because
//! freeing a binding's run at the end of the expression that read it would
//! let a later temporary overwrite a variable that is still in scope.

use std::collections::HashMap;

use crate::layout::LayoutId;
use crate::{Repr, Slot};

/// Where an expression left its answer: a base slot and the layout that says
/// how many words follow it and what each of them holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Val {
    pub slot: Slot,
    pub layout: LayoutId,
    /// Whether the consumer of this value should give the run back.
    ///
    /// A temporary is the expression's own and dies with its last use. A
    /// borrowed location — a parameter, a local, a field of one, the answer
    /// — outlives the expression that read it, and giving one back would
    /// hand a live binding to the next temporary that asked.
    pub temp: bool,
}

impl Val {
    /// A temporary this expression allocated.
    pub fn temp(slot: Slot, layout: LayoutId) -> Val {
        Val {
            slot,
            layout,
            temp: true,
        }
    }

    /// A location that belongs to something longer-lived.
    pub fn borrowed(slot: Slot, layout: LayoutId) -> Val {
        Val {
            slot,
            layout,
            temp: false,
        }
    }
}

/// One lexical scope: the names it introduced and the runs it must give back
/// when it ends.
#[derive(Default)]
struct Scope {
    /// Pushed in declaration order and searched backwards, so a shadowing
    /// declaration wins without the earlier one having to be removed — and
    /// the earlier one's run stays owned by this scope, which is what a
    /// shadowed binding still needs.
    names: Vec<(String, Slot, LayoutId)>,
    /// The runs the scope gives back when it ends, with the width each one
    /// occupies. The width is carried rather than looked up, because ending
    /// a scope must not need the layout table the caller is holding.
    owned: Vec<(Slot, LayoutId, u32)>,
}

pub(crate) struct Frame {
    reprs: Vec<Repr>,
    free: HashMap<Vec<Repr>, Vec<Slot>>,
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

    /// Whether the run at `slot` holds anything a collection traces, or an
    /// address whose live range the lowering ends.
    pub fn holds_ref(&self, slot: Slot, width: u32) -> bool {
        (slot..slot + width).any(|at| matches!(self.reprs[at as usize], Repr::Ref | Repr::Addr))
    }

    /// A run that is never given back: a parameter.
    ///
    /// Parameters occupy the frame from slot 0 in declaration order and the
    /// caller writes into them, so they are taken before anything else asks
    /// and never returned to a free list.
    pub fn param(&mut self, words: &[Repr]) -> Slot {
        self.push(words)
    }

    /// A run of `words`, reusing a dead one of exactly that shape when there
    /// is one.
    pub fn alloc(&mut self, words: &[Repr]) -> Slot {
        match self.free.get_mut(words).and_then(Vec::pop) {
            Some(slot) => slot,
            None => self.push(words),
        }
    }

    fn push(&mut self, words: &[Repr]) -> Slot {
        let at = self.reprs.len() as Slot;
        self.reprs.extend_from_slice(words);
        at
    }

    /// Gives a run back to the list its shape draws from.
    pub fn free(&mut self, slot: Slot, width: u32) {
        let words = self.reprs[slot as usize..(slot + width) as usize].to_vec();
        self.free.entry(words).or_default().push(slot);
    }

    /// How many scopes are open. A loop records this so `break` knows how
    /// many it is leaving.
    pub fn depth(&self) -> usize {
        self.scopes.len()
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    /// Ends the innermost scope, answering the locations it owned that hold
    /// a reference.
    ///
    /// The runs go back on the free lists here; the answer is what the
    /// caller must emit [`crate::Inst::Clear`] for, because a static
    /// reference map cannot say when a value stopped being needed.
    pub fn pop_scope(&mut self) -> Vec<(Slot, LayoutId)> {
        let scope = self.scopes.pop().expect("a scope is open");
        let mut clears = Vec::new();
        for (slot, layout, width) in scope.owned {
            if self.holds_ref(slot, width) {
                clears.push((slot, layout));
            }
            self.free(slot, width);
        }
        clears
    }

    /// The locations the scopes inside `depth` own that hold a reference.
    ///
    /// This is what a `break` or a `continue` has to clear: it leaves those
    /// scopes without ending them, and the loop it jumps to or out of goes
    /// on running, so a reference left behind would be retained for the rest
    /// of the frame rather than for the rest of the turn.
    pub fn refs_within(&self, depth: usize) -> Vec<(Slot, LayoutId)> {
        self.scopes[depth..]
            .iter()
            .flat_map(|scope| scope.owned.iter().copied())
            .filter(|(slot, _, width)| self.holds_ref(*slot, *width))
            .map(|(slot, layout, _)| (slot, layout))
            .collect()
    }

    /// Names a location in the innermost scope.
    pub fn bind(&mut self, name: &str, slot: Slot, layout: LayoutId) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .names
            .push((name.to_string(), slot, layout));
    }

    /// Makes the innermost scope responsible for giving a location back.
    pub fn own(&mut self, slot: Slot, layout: LayoutId, width: u32) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .owned
            .push((slot, layout, width));
    }

    /// The location `name` denotes, searching inwards out.
    pub fn lookup(&self, name: &str) -> Option<(Slot, LayoutId)> {
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .names
                .iter()
                .rev()
                .find(|(bound, _, _)| bound == name)
                .map(|(_, slot, layout)| (*slot, *layout))
        })
    }
}
