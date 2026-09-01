//! What one eight-byte word means.
//!
//! [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md) keeps the
//! word untagged and puts its meaning in static metadata. This is that
//! metadata at its smallest unit: one `Repr` per slot, per field, per array
//! element.
//!
//! **A `Repr` describes one word, not one value.** A value may occupy several
//! consecutive slots, and what says how many is a
//! [`Layout`](crate::Layout) — a run of these. The two are separate for the
//! reason the collector is: it asks exactly one question, of one word at a
//! time, and a reference map that is one bit per slot answers it without a
//! range table.

/// The interpretation of one word.
///
/// The collector consults exactly one thing about a `Repr`: whether it is
/// [`Repr::Ref`]. Everything else is for the boundary, the verifier and the
/// printer, all of which run outside the dispatch loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Repr {
    /// Nothing. The word is zero.
    ///
    /// `Unit` is a value in Cove — `fn f() {}` answers one — so it takes a
    /// slot rather than being absent, which keeps the slot numbering and the
    /// calling convention free of a special case.
    Unit,
    /// `0` or `1`.
    Bool,
    /// A two's-complement `i64`.
    Int,
    /// An IEEE-754 double, bit-cast into the word.
    ///
    /// Bit-cast rather than converted: `f64::to_bits` round-trips every
    /// value including the NaN payloads, and the word is never read as an
    /// integer by anything that did not write it as one.
    Float,
    /// Nanoseconds, as an `i64`.
    ///
    /// A separate `Repr` from [`Repr::Int`] although the bits are the same,
    /// because the boundary has to know which one to materialise and asking
    /// the slot is cheaper than carrying a second table.
    Duration,
    /// The linear address of a heap object's header, or `0` for none.
    ///
    /// This is the only `Repr` the collector treats as a root. Heap
    /// addresses start at `STACK_WORDS`, so `0` can never name an object and
    /// is free to mean null — which is what a frame full of zeroes gives a
    /// `Ref` slot that has not been written yet.
    Ref,
    /// The linear address of one mutable word: a place.
    ///
    /// Not a root. The object an interior address points into is kept alive
    /// by the `Ref` slot the lowering holds it in, and the heap does not
    /// move, so the address stays correct across a collection without the
    /// collector knowing it exists.
    Addr,
    /// An index into the run's host resource table.
    ///
    /// A host resource is owned by the host, not by Cove, so it is not an
    /// object in the heap and not a root. The word names it; the host owns
    /// its lifetime.
    Host,
}

impl Repr {
    /// Whether a word of this `Repr` is a garbage-collection root.
    ///
    /// This is the whole of what the collector asks the static side.
    pub fn is_ref(self) -> bool {
        matches!(self, Repr::Ref)
    }

    /// The name this `Repr` prints under in a disassembly.
    pub fn name(self) -> &'static str {
        match self {
            Repr::Unit => "unit",
            Repr::Bool => "bool",
            Repr::Int => "int",
            Repr::Float => "float",
            Repr::Duration => "duration",
            Repr::Ref => "ref",
            Repr::Addr => "addr",
            Repr::Host => "host",
        }
    }
}

impl std::fmt::Display for Repr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Which slots of a frame are [`Repr::Ref`], as one bit each.
///
/// A frame's roots are a static fact here rather than a program-counter
/// dependent one, and that is the point of the register machine: a stack
/// machine's live-reference set changes as operands are pushed and popped,
/// so its map has to be indexed by pc. Here the answer does not change
/// between the first instruction of a function and the last.
///
/// The lowering guarantees the one fact this relies on: **a slot's `Repr` is
/// fixed for the whole function.** A slot may be reused by a later value of
/// the *same* `Repr` — that is what keeps a frame from growing with every
/// temporary a long body mentions — but never by one of a different `Repr`,
/// because then no single bit would be right at every program counter.
///
/// A static map says which slots the collector reads. It cannot say when the
/// value in one stopped being needed, because that is a fact about a program
/// point. The lowering answers that in the data instead: it emits
/// [`Clear`](crate::Inst::Clear) at a reference's last use, so a dead slot
/// holds null and the collector traces nothing from it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefMap {
    words: Vec<u64>,
    slots: u32,
}

impl RefMap {
    /// The map for a frame of `slots` words, with `reprs[i]` at slot `i`.
    pub fn of(reprs: &[Repr]) -> RefMap {
        let slots = reprs.len() as u32;
        let mut map = RefMap {
            words: vec![0; reprs.len().div_ceil(64)],
            slots,
        };
        for (slot, repr) in reprs.iter().enumerate() {
            if repr.is_ref() {
                map.words[slot / 64] |= 1 << (slot % 64);
            }
        }
        map
    }

    /// How many slots the map covers.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// Whether slot `slot` holds a reference.
    ///
    /// Out of range answers `false` rather than panicking: the collector
    /// walks a frame whose size it took from the same [`crate::Function`] as
    /// this map, so a disagreement is a bug in the lowering, and a
    /// collection is the worst place to discover one by unwinding.
    pub fn is_ref(&self, slot: u32) -> bool {
        let slot = slot as usize;
        match self.words.get(slot / 64) {
            Some(word) => word & (1 << (slot % 64)) != 0,
            None => false,
        }
    }

    /// Every reference slot, ascending.
    ///
    /// Iterating the set bits rather than every slot is what makes a frame
    /// of mostly scalars cheap to scan: a frame with no references at all
    /// costs one word read per 64 slots.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words
            .iter()
            .copied()
            .enumerate()
            .flat_map(|(i, word)| {
                let mut rest = word;
                std::iter::from_fn(move || {
                    if rest == 0 {
                        return None;
                    }
                    let bit = rest.trailing_zeros();
                    rest &= rest - 1;
                    Some(i as u32 * 64 + bit)
                })
            })
    }

    /// Whether the frame holds no references at all.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scalar_frame_has_no_roots() {
        let map = RefMap::of(&[Repr::Int, Repr::Bool, Repr::Float]);
        assert!(map.is_empty());
        assert_eq!(map.iter().collect::<Vec<_>>(), Vec::<u32>::new());
    }

    #[test]
    fn the_map_names_exactly_the_ref_slots() {
        let map = RefMap::of(&[Repr::Int, Repr::Ref, Repr::Addr, Repr::Ref]);
        assert_eq!(map.iter().collect::<Vec<_>>(), vec![1, 3]);
        assert!(!map.is_ref(0));
        assert!(map.is_ref(1));
        assert!(!map.is_ref(2));
        assert!(map.is_ref(3));
    }

    #[test]
    fn a_place_is_not_a_root() {
        // ADR 0034: an address is not itself a root. What it points into is
        // kept alive by the `Ref` slot holding the base object.
        assert!(!Repr::Addr.is_ref());
        assert!(!Repr::Host.is_ref());
        assert!(Repr::Ref.is_ref());
    }

    #[test]
    fn the_map_spans_more_than_one_word() {
        let mut reprs = vec![Repr::Int; 130];
        reprs[0] = Repr::Ref;
        reprs[64] = Repr::Ref;
        reprs[129] = Repr::Ref;
        let map = RefMap::of(&reprs);
        assert_eq!(map.slots(), 130);
        assert_eq!(map.iter().collect::<Vec<_>>(), vec![0, 64, 129]);
    }

    #[test]
    fn out_of_range_is_not_a_root() {
        let map = RefMap::of(&[Repr::Ref]);
        assert!(map.is_ref(0));
        assert!(!map.is_ref(9999));
    }
}
