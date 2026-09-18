//! One growable run: the core `ByteBuffer` and `Vector<T>` both grow through.
//!
//! [ADR 0052](../../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)
//! gives the two one discipline — a stable owner, a replaceable store whose
//! header length is its capacity, and a live prefix `[0, len)` inside it — and
//! [ADR 0058](../../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! names the three operations on it, whatever a unit is:
//!
//! - [`growable_ensure`] makes room for `additional` more units, growing the
//!   store if they would not fit;
//! - [`growable_commit`] advances the logical length over units the caller
//!   has already written;
//! - [`growable_finish`] consumes the owner and relabels its store into the
//!   fixed run it already is.
//!
//! Before this module each family carried its own copy of the three: a
//! `reserve_bytes` and the body of `finish_buffer` for bytes, and `seq.rs`'s
//! `grow` and the body of `vector_freeze` for elements, with two views, two
//! sets of word offsets and two floors that answered the same question in
//! different units. The
//! differences that were real — a byte copy blends partial words where an
//! element copy moves whole ones, a byte finish validates UTF-8 where an
//! element finish does not — are a match on [`Storage`] and a [`Validation`]
//! here, and nothing else about the two is allowed to differ.
//!
//! What stays with each family is how it *reads* its owner. The refusals a
//! consumed or corrupted owner answers are worded for the family and ordered
//! as its callers always saw them, so `Machine::buffer` and `seq.rs`'s
//! `vector` each check their own and then build a [`Growable`].
//!
//! # What this does not charge
//!
//! A growth copies the live prefix and is not charged fuel for it, which is
//! what both families did before they shared this. ADR 0052 says a growth copy
//! is proportional work; charging it changes the `fuel_spent` a program
//! observes, and that is a decision of its own (issue #378, Q13) rather than a
//! side effect of moving the code.

use std::sync::Arc;

use cove_ir::{LayoutId, Shape, Storage, Validation};

use super::Machine;
use crate::error::RuntimeError;

/// Payload word 0 of a growable owner — a `Shape::Vector` or a
/// `Shape::ByteBuffer`: how many of its store's units are value.
///
/// The same word in both, because the two are one ownership discipline. The
/// IR's lowering states the same offsets as `VECTOR_LEN` and `BUFFER_LEN`.
pub(crate) const GROWABLE_LEN: u32 = 0;

/// Payload word 1 of a growable owner: the store holding its units, whose own
/// header length is the capacity. Null once a finish consumed the owner.
pub(crate) const GROWABLE_STORE: u32 = 1;

/// The smallest byte store a buffer is allocated with, and the floor a growth
/// doubles up from.
///
/// ADR 0052 says "twice the capacity from a small floor" and leaves the floor
/// to the storage unit. [`MIN_GROWABLE_ELEMENTS`] is four *elements*; sixteen
/// is the byte-sized answer to the same question, and the reason it is not
/// four is that four bytes is less than one word. A byte store packs eight
/// bytes to a word and costs a header word whatever it holds, so a capacity
/// below eight buys nothing at all and a capacity of eight buys one growth's
/// worth of nothing: sixteen is two payload words, which covers the
/// punctuation-sized appends a formatter makes between the ones that are worth
/// reallocating for.
pub(crate) const MIN_GROWABLE_BYTES: u64 = 16;

/// The smallest element store a growth asks for.
///
/// A `Vector` built from known elements is allocated to exactly those, so this
/// is the floor of the first growth rather than of the first allocation.
pub(crate) const MIN_GROWABLE_ELEMENTS: u64 = 4;

/// The two layouts a growable run of one element is built out of: the owner's
/// and the store's.
///
/// An allocation is the only growable operation that has to read ADR 0052's
/// ownership pair *backwards*, from the element to the objects: every other one
/// is handed an owner whose own header names its layout.
/// `Inst::GrowableAlloc` over [`Storage::Words`] carries the element and
/// nothing else, so this is what the element is turned into, and
/// [`word_runs`] is where the turning is done once per program rather than once
/// per allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WordRun {
    /// The `Shape::Vector` of the element: the two-word owner, a logical length
    /// and a store reference.
    pub(crate) owner: LayoutId,
    /// The growable `Shape::Elements` of the element: the run the elements live
    /// in, whose header length is the capacity.
    pub(crate) store: LayoutId,
}

/// Every element's [`WordRun`], indexed by the *element's* `LayoutId`.
///
/// One pass over the layout table rather than one scan per element, which is
/// what makes this a table worth building: a program with `n` layouts has at
/// most `n` vectors, and asking each of them which element it is over is `n`
/// questions, not `n` squared.
///
/// Where a program declares two layouts of the same shape over one element —
/// which interning makes unlikely and does not forbid — the **first** is kept,
/// which is the answer `cove_native::subset`'s `word_owner` gives from its own
/// scan. The two agree because a run compiled natively and the same run
/// interpreted must allocate the same object.
///
/// `None` is a bound and not a family: a lowering that grew a run of `elem`
/// declared both of these, so an element with no entry is a program that never
/// builds one, and the machine refuses the allocation rather than guessing.
pub(crate) fn word_runs(program: &cove_ir::Program) -> Arc<[Option<WordRun>]> {
    let mut owners: Vec<Option<LayoutId>> = vec![None; program.layouts.len()];
    let mut stores: Vec<Option<LayoutId>> = vec![None; program.layouts.len()];
    for (index, layout) in program.layouts.iter().enumerate() {
        let id = LayoutId(index as u32);
        let (held, elem) = match layout.shape {
            Shape::Vector { elem } => (&mut owners, elem),
            Shape::Elements {
                elem,
                growable: true,
            } => (&mut stores, elem),
            _ => continue,
        };
        if let Some(slot) = held.get_mut(elem.index()) {
            slot.get_or_insert(id);
        }
    }
    owners
        .into_iter()
        .zip(stores)
        .map(|(owner, store)| {
            Some(WordRun {
                owner: owner?,
                store: store?,
            })
        })
        .collect()
}

/// A live growable run: its owner, its store, and how much of the store is
/// value rather than spare room.
///
/// `len` and `capacity` are both units — bytes for
/// [`Storage::PackedBytes`], elements for [`Storage::Words`] — as the owner's
/// length word and the store's own header state them. It is a *view*: the
/// operations below keep it in step with the words it read, so a caller that
/// ensured and committed through one can read `store` and `len` from it
/// afterwards rather than out of the owner again.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Growable {
    pub(crate) owner: u64,
    pub(crate) store: u64,
    pub(crate) len: u32,
    pub(crate) capacity: u32,
    pub(crate) storage: Storage,
}

impl Growable {
    /// The floor a growth doubles up from, in this run's units.
    fn floor(&self) -> u64 {
        match self.storage {
            Storage::PackedBytes => MIN_GROWABLE_BYTES,
            Storage::Words(_) => MIN_GROWABLE_ELEMENTS,
        }
    }
}

/// Room for `additional` more units in `run`, growing its store if they would
/// not fit.
///
/// If `len + additional <= capacity` this does nothing, and that comparison is
/// the whole of the common case. Otherwise the store is replaced by one of
/// `max(needed, capacity * 2, floor)` units — ADR 0052's doubling from a small
/// floor, with the one addition a bulk append needs: a doubling that still
/// would not hold the range is not two growths, it is one growth to the length
/// that fits. For a single unit `needed` is `capacity + 1`, which never exceeds
/// the other two, so a push grows exactly as it did before this was shared.
///
/// # Nothing is mutated on a refusal
///
/// The sum is checked before anything else, and a capacity nothing could hold
/// is refused by the allocator: the doubling saturates, and
/// `Machine::allocate` answers "this run has no memory left" for a length too
/// large for a header or a payload *before* the owner's store word or its
/// length word is touched. So a refused growth leaves the owner exactly as it
/// was.
///
/// # Why nothing is lost to a collection
///
/// The allocation may collect. The old store is reachable from the owner, and
/// every caller read the owner out of a frame slot (or a payload word of an
/// object that is in one), so the collection cannot free it. The new store is
/// unrooted from the moment it is allocated until the owner's store word names
/// it — and nothing in that window allocates: the copy is a run of loads and
/// stores, so no collection can happen inside it.
///
/// The copy is the live prefix and nothing else. That is what keeps a byte
/// store's spare tail zero: a fresh store is zeroed, and
/// `Machine::copy_string_bytes` blends masked bytes rather than whole words, so
/// the bytes of the last partial word above `len` are left as the allocator
/// left them.
#[inline]
pub(crate) fn growable_ensure(
    machine: &mut Machine<'_>,
    run: &mut Growable,
    additional: u64,
) -> Result<(), RuntimeError> {
    let Some(needed) = u64::from(run.len).checked_add(additional) else {
        return Err(RuntimeError::new("this run has no memory left"));
    };
    if needed <= u64::from(run.capacity) {
        return Ok(());
    }
    grow(machine, run, needed)
}

/// [`growable_ensure`]'s slow path: a larger store, the live prefix copied
/// into it, and the owner's store word replaced.
///
/// The new store has the old store's layout. A growth replaces a store with a
/// larger one of the same family, and the old store is where that family is
/// already written down — reading it is one header load, where searching the
/// program's layout table for it would be a scan per growth.
#[inline(never)]
fn grow(machine: &mut Machine<'_>, run: &mut Growable, needed: u64) -> Result<(), RuntimeError> {
    let want = needed
        .max(u64::from(run.capacity).saturating_mul(2))
        .max(run.floor());
    let layout = machine.mem.object_layout(run.store);
    let store = machine.allocate(layout, i64::try_from(want).unwrap_or(i64::MAX))?;
    match run.storage {
        Storage::PackedBytes => {
            machine.copy_string_bytes(store, 0, run.store, 0, run.len as usize);
        }
        Storage::Words(elem) => {
            // The old store holds `capacity * stride` payload words, so the
            // live prefix's product is inside a `u32` already.
            let words = run.len * machine.width(elem);
            let dst = machine.mem.payload_addr(store, 0);
            let src = machine.mem.payload_addr(run.store, 0);
            machine.mem.copy_words(dst, src, words);
        }
    }
    machine.mem.set_payload(run.owner, GROWABLE_STORE, store);
    run.store = store;
    run.capacity = machine.mem.object_len(store);
    // The one place a store is replaced, so the one place a growth can be
    // counted as a growth: a window that finished the ordinary way may not have
    // grown anything, and an ensure outside every window may have. See
    // `BoundaryReport::growths`. A run that asked for no counts pays one
    // `Option` test per reallocation, on a path that has just allocated and
    // copied the live prefix.
    if machine.counting.is_some() {
        machine.count_growth(run.storage);
    }
    Ok(())
}

/// Advances `run`'s logical length by `count` units the caller has written.
///
/// Every caller ensured room for them first, so the new length is inside the
/// capacity. The length word is written here and nowhere else in an append,
/// and it is written after the units: a run stopped part way through a bulk
/// append leaves what it copied above the logical length, where it is spare
/// room rather than value.
#[inline]
pub(crate) fn growable_commit(machine: &mut Machine<'_>, run: &mut Growable, count: u64) {
    let len = u64::from(run.len) + count;
    debug_assert!(
        len <= u64::from(run.capacity),
        "a commit of {count} unit(s) onto {} leaves a capacity of {}",
        run.len,
        run.capacity
    );
    machine.mem.set_payload(run.owner, GROWABLE_LEN, len);
    run.len = len as u32;
}

/// Lowers `run`'s logical length to `len` and clears the units it vacates:
/// [`growable_commit`]'s inverse, beneath `Vector.pop` and `Vector.remove`.
///
/// The units in `[len, run.len)` are zeroed **before** the length word is
/// written. A store's shape says its whole capacity is elements and the
/// collector traces it that way, so a vacated unit that still held a reference
/// would be a root for whatever it named. The store is kept, and so is its
/// capacity.
///
/// Neither half can allocate — `Memory::clear_words` fills words that already
/// exist, and `Memory::set_payload` writes one — so there is no safepoint
/// between them and no collection can happen inside this function. That is what
/// makes the order sufficient rather than merely correct, and it is the reason
/// [`cove_ir::Inst::GrowableTruncate`] is one instruction and not two; the whole
/// argument, including why it is not a negative commit, is written out there.
///
/// Only a word run has a truncate: `crate::verify` admits no byte member.
/// `len` is at most the length, which the one caller checks and refuses.
pub(crate) fn growable_truncate(machine: &mut Machine<'_>, run: &mut Growable, len: u32) {
    debug_assert!(
        len <= run.len,
        "a truncate of {} to {len} raises it",
        run.len
    );
    let Storage::Words(elem) = run.storage else {
        unreachable!("a byte run has no truncate, and `cove_ir::verify` refuses one")
    };
    let stride = machine.width(elem);
    let vacated = (run.len - len) * stride;
    let from = machine.mem.payload_addr(run.store, len * stride);
    machine.mem.clear_words(from, vacated);
    machine
        .mem
        .set_payload(run.owner, GROWABLE_LEN, u64::from(len));
    run.len = len;
}

/// Consumes `run`'s owner and answers its store, relabelled to `target` at the
/// logical length.
///
/// ADR 0052's "finishing reuses the store": the store *is* the answer. A
/// `Shape::Bytes` run and a `Shape::Str` object of the same byte length occupy
/// the same words, and a growable element store and an `Array` of the same
/// element count do too, so nothing is copied — the header is rewritten down
/// from the capacity to the length, and the words in between are released as
/// a free block the next sweep folds back in. `spare` is measured in *payload
/// words*, because that is what a free block is measured in.
///
/// # Validation
///
/// [`Validation::Utf8`] checks the live prefix `[0, len)` and nothing above
/// it: the bytes past the logical length are spare room the program never
/// appended and must not be asked to account for. It refuses before anything
/// is relabelled, so a refused finish leaves the owner as it was.
///
/// The tail of a byte store's last partial word is zero, which is what makes a
/// finished `String` equal word-for-word to the same text written by
/// `Machine::new_string`: allocation zeroes, every append writes only the live
/// prefix, and a growth copies only the live prefix into another zeroed store.
/// `eq.str` compares payload words, so a dirty tail would be a string unequal
/// to itself written another way.
///
/// # Consuming
///
/// The owner is emptied — length zero, store null — because a finish
/// *consumes*. That is sound only where the caller holds the only handle, and
/// the uniqueness proof is `cove_sema`'s rather than this machine's; each
/// family's reader refuses an owner whose store word is null, so a proof that
/// let one through reports rather than reading a null store as an empty run.
pub(crate) fn growable_finish(
    machine: &mut Machine<'_>,
    run: &Growable,
    target: LayoutId,
    validation: Validation,
) -> Result<u64, RuntimeError> {
    if validation == Validation::Utf8 && !live_prefix_is_utf8(machine, run) {
        return Err(RuntimeError::new("this string's bytes are not valid UTF-8"));
    }
    let spare = match run.storage {
        Storage::PackedBytes => run.capacity.div_ceil(8) - run.len.div_ceil(8),
        Storage::Words(elem) => (run.capacity - run.len) * machine.width(elem),
    };
    machine.relabel(run.store, target, run.len, spare);
    machine.mem.set_payload(run.owner, GROWABLE_LEN, 0);
    machine.mem.set_payload(run.owner, GROWABLE_STORE, 0);
    Ok(run.store)
}

/// Whether the live prefix of a byte run is valid UTF-8.
///
/// Read a word at a time first: a prefix none of whose bytes has its high bit
/// set is ASCII, and ASCII is UTF-8, so the common case copies nothing and
/// decodes nothing. Only a prefix that holds a byte of `0x80` or above is
/// copied out and handed to the decoder. The bytes above the length in the
/// last word are masked off rather than trusted to be zero: they are spare
/// room, and spare room is not the value.
fn live_prefix_is_utf8(machine: &Machine<'_>, run: &Growable) -> bool {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let len = run.len as usize;
    let whole = len / 8;
    let ascii = (0..whole).all(|at| machine.mem.payload(run.store, at as u32) & HIGH == 0)
        && match len % 8 {
            0 => true,
            tail => {
                let mask = (1u64 << (tail * 8)) - 1;
                machine.mem.payload(run.store, whole as u32) & mask & HIGH == 0
            }
        };
    ascii || std::str::from_utf8(&live_bytes(machine, run)).is_ok()
}

/// The live prefix of a byte run, as bytes.
///
/// `Machine::string_bytes`' read bounded by the *owner's* length rather than
/// the store's, which is the whole difference between a builder and a string:
/// the store's header length is its capacity.
fn live_bytes(machine: &Machine<'_>, run: &Growable) -> Vec<u8> {
    debug_assert_eq!(
        run.storage,
        Storage::PackedBytes,
        "only a byte run has bytes to validate"
    );
    let len = run.len as usize;
    let mut out = Vec::with_capacity(len);
    for at in 0..len.div_ceil(8) {
        let word = machine.mem.payload(run.store, at as u32);
        for byte in 0..8 {
            if out.len() == len {
                break;
            }
            out.push((word >> (byte * 8)) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use cove_ir::Program;

    use super::super::tests::Build;
    use super::*;

    /// A program with the layouts both storages grow through: a byte buffer
    /// and its store, and a `Vector<String>` whose store holds references —
    /// so a copy the collector could not see would be a string freed.
    struct Layouts {
        program: Program,
        text: LayoutId,
        vector: LayoutId,
        store: LayoutId,
        array: LayoutId,
    }

    fn layouts() -> Layouts {
        let mut build = Build::default();
        let text = build.string_layout();
        build.bytes_layout();
        build.buffer_layout();
        let vector = build.layout("Vector<String>", Shape::Vector { elem: text });
        let store = build.layout(
            "store<String>",
            Shape::Elements {
                elem: text,
                growable: true,
            },
        );
        let array = build.layout(
            "Array<String>",
            Shape::Elements {
                elem: text,
                growable: false,
            },
        );
        Layouts {
            program: build.done(),
            text,
            vector,
            store,
            array,
        }
    }

    /// An empty byte buffer over a store of `capacity` bytes, rooted.
    fn bytes_run(machine: &mut Machine<'_>, capacity: u32) -> Growable {
        let program = machine.program;
        let store = machine.new_object(program.bytes_layout, capacity).unwrap();
        machine.push_temp(store);
        let owner = machine.new_object(program.buffer_layout, 0).unwrap();
        machine.push_temp(owner);
        machine.set_payload(owner, GROWABLE_STORE, store);
        Growable {
            owner,
            store,
            len: 0,
            capacity,
            storage: Storage::PackedBytes,
        }
    }

    /// An empty `Vector<String>` over a store of `capacity` elements, rooted.
    fn words_run(machine: &mut Machine<'_>, f: &Layouts, capacity: u32) -> Growable {
        let store = machine.new_object(f.store, capacity).unwrap();
        machine.push_temp(store);
        let owner = machine.new_object(f.vector, 0).unwrap();
        machine.push_temp(owner);
        machine.set_payload(owner, GROWABLE_STORE, store);
        Growable {
            owner,
            store,
            len: 0,
            capacity,
            storage: Storage::Words(f.text),
        }
    }

    /// Appends `bytes` one at a time, through ensure and commit.
    fn push_bytes(machine: &mut Machine<'_>, run: &mut Growable, bytes: &[u8]) {
        for byte in bytes {
            growable_ensure(machine, run, 1).unwrap();
            machine.put_bytes(run.store, run.len as usize, 1, u64::from(*byte));
            growable_commit(machine, run, 1);
        }
    }

    /// The run the owner's words describe now, which is what the view must
    /// agree with.
    fn reread(machine: &Machine<'_>, run: &Growable) -> (u64, u64, u32) {
        let store = machine.payload(run.owner, GROWABLE_STORE);
        (
            machine.payload(run.owner, GROWABLE_LEN),
            store,
            machine.object_len(store),
        )
    }

    #[test]
    fn an_empty_run_grows_to_its_storage_floor() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);

        let mut bytes = bytes_run(&mut machine, 0);
        growable_ensure(&mut machine, &mut bytes, 1).unwrap();
        assert_eq!(u64::from(bytes.capacity), MIN_GROWABLE_BYTES);
        assert_eq!(reread(&machine, &bytes), (0, bytes.store, bytes.capacity));

        let mut words = words_run(&mut machine, &f, 0);
        growable_ensure(&mut machine, &mut words, 1).unwrap();
        assert_eq!(u64::from(words.capacity), MIN_GROWABLE_ELEMENTS);
        assert_eq!(machine.object_layout(words.store), f.store);
        assert_eq!(reread(&machine, &words), (0, words.store, words.capacity));
    }

    #[test]
    fn a_run_that_fits_is_left_alone() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut words = words_run(&mut machine, &f, 3);
        let before = machine.mem.allocations();
        growable_ensure(&mut machine, &mut words, 3).unwrap();
        assert_eq!(machine.mem.allocations(), before, "nothing was allocated");
        assert_eq!(words.capacity, 3);
    }

    /// A full run doubles, and a growth copies the live prefix at the unit's
    /// own width — a byte blended into a partial word, an element as its
    /// whole stride.
    #[test]
    fn a_full_run_doubles_and_keeps_its_prefix() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);

        let mut bytes = bytes_run(&mut machine, 0);
        push_bytes(&mut machine, &mut bytes, b"0123456789abcdefg");
        assert_eq!(bytes.capacity, 32, "sixteen doubled once");
        assert_eq!(bytes.len, 17);
        assert_eq!(reread(&machine, &bytes), (17, bytes.store, 32));
        assert_eq!(live_bytes(&machine, &bytes), b"0123456789abcdefg".to_vec());
        assert_eq!(
            machine.payload(bytes.store, 2) >> 8,
            0,
            "the partial word's spare tail is still zero"
        );

        let mut words = words_run(&mut machine, &f, 0);
        let mut texts = Vec::new();
        for at in 0..5 {
            let text = machine.new_string(&format!("item {at}")).unwrap();
            machine.push_temp(text);
            texts.push(text);
            growable_ensure(&mut machine, &mut words, 1).unwrap();
            machine.set_payload(words.store, words.len, text);
            growable_commit(&mut machine, &mut words, 1);
        }
        assert_eq!(words.capacity, 8, "four doubled once");
        assert_eq!(reread(&machine, &words), (5, words.store, 8));
        assert_eq!(machine.payload_run(words.store, 0, 5), texts);
        assert_eq!(machine.payload(words.store, 5), 0, "spare room is zero");
    }

    /// A bulk extend that a doubling would not hold grows once, to the length
    /// that fits.
    #[test]
    fn an_extend_past_twice_the_capacity_grows_to_what_it_needs() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut bytes = bytes_run(&mut machine, 16);
        push_bytes(&mut machine, &mut bytes, b"abc");
        let before = machine.mem.allocations();
        growable_ensure(&mut machine, &mut bytes, 100).unwrap();
        assert_eq!(machine.mem.allocations(), before + 1, "one growth");
        assert_eq!(bytes.capacity, 103);
        assert_eq!(live_bytes(&machine, &bytes), b"abc".to_vec());
    }

    /// An ensure whose arithmetic overflows, or whose capacity nothing could
    /// allocate, refuses in the allocator's words and leaves the owner as it
    /// was.
    #[test]
    fn a_refused_growth_leaves_the_owner_untouched() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut bytes = bytes_run(&mut machine, 16);
        push_bytes(&mut machine, &mut bytes, b"abc");
        let mut words = words_run(&mut machine, &f, 2);
        for run in [&mut bytes, &mut words] {
            let before = *run;
            let owner = reread(&machine, run);
            for additional in [u64::MAX, u64::from(u32::MAX)] {
                let error = growable_ensure(&mut machine, run, additional).unwrap_err();
                assert_eq!(error.message, "this run has no memory left");
                assert_eq!(reread(&machine, run), owner, "the owner is untouched");
                assert_eq!(
                    (run.store, run.len, run.capacity),
                    (before.store, before.len, before.capacity)
                );
            }
        }
    }

    /// The heap is sized so the growth's own allocation *must* collect, with
    /// the old store's references reachable only through the owner — which is
    /// the one collection a growth can meet, since nothing between the new
    /// store's allocation and the owner write allocates. A second collection
    /// after the owner write finds the new store through the owner.
    #[test]
    fn a_growth_that_collects_keeps_both_stores_honest() {
        let f = layouts();
        // Small enough that the garbage below fills it.
        let mut machine = Machine::new(&f.program, 400);
        let mut words = words_run(&mut machine, &f, 4);
        for at in 0..4 {
            // Rooted only through the store, which is rooted only through the
            // owner once the fixture's temporary for the store is dropped.
            let text = machine.new_string(&format!("kept {at}")).unwrap();
            machine.set_payload(words.store, at, text);
            growable_commit(&mut machine, &mut words, 1);
        }
        // Only the owner stays a temporary root: the store and its strings
        // must survive through it.
        let mark = machine.temps();
        machine.release_temps(mark - 2);
        machine.push_temp(words.owner);
        // Unrooted garbage until not even a one-element object fits, so the
        // growth's allocation cannot be served without a collection.
        for (len, words) in [(16, 16), (1, 1)] {
            while machine.mem.alloc(f.array, len, words).is_some() {}
        }

        let before = machine.collected().collections;
        let old = words.store;
        growable_ensure(&mut machine, &mut words, 1).unwrap();
        assert!(
            machine.collected().collections > before,
            "this fixture exists to collect inside the growth's allocation"
        );
        assert_ne!(words.store, old);
        machine.collect();
        for at in 0..4 {
            let text = machine.payload(words.store, at);
            assert_eq!(
                machine.object_layout(text),
                f.text,
                "element {at} is a string"
            );
            assert_eq!(
                machine.string_bytes(text),
                format!("kept {at}").into_bytes()
            );
        }
        assert_eq!(reread(&machine, &words), (4, words.store, 8));
    }

    /// The word-at-a-time ASCII check reads the live prefix and nothing past
    /// it: a high byte in the spare room of the last word does not refuse a
    /// finish, and one inside the prefix — at a word boundary as well as
    /// inside a word — still reaches the decoder and does.
    /// **A byte run has no truncate, and the machine refuses to guess rather
    /// than reading a byte length as a word length.**
    ///
    /// The third of the three layers that refuse one, and the only one that is
    /// an assertion instead of a diagnostic: `cove_ir::verify` faults the
    /// instruction over `Storage::PackedBytes` and the bytecode encoder has no
    /// opcode to put it in, so nothing a program can express arrives here. That
    /// is exactly why this is a panic and not a `Result` — the two static
    /// refusals are the proof, and this says the proof is load-bearing. A byte
    /// run that got through would clear `(run.len - len)` *words* starting at a
    /// byte offset, over memory belonging to bytes the program still holds.
    #[test]
    #[should_panic(expected = "a byte run has no truncate")]
    fn a_byte_run_has_no_truncate_and_the_machine_refuses_to_guess() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut bytes = bytes_run(&mut machine, 8);
        push_bytes(&mut machine, &mut bytes, b"abc");
        growable_truncate(&mut machine, &mut bytes, 1);
    }

    #[test]
    fn a_byte_finish_reads_only_the_live_prefix_a_word_at_a_time() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut bytes = bytes_run(&mut machine, 0);
        push_bytes(&mut machine, &mut bytes, b"eleven byte");
        // Spare room, in the live prefix's last word and in the next one.
        machine.put_bytes(bytes.store, 11, 1, 0xC3);
        machine.put_bytes(bytes.store, 13, 1, 0xFF);
        let text =
            growable_finish(&mut machine, &bytes, f.program.str_layout, Validation::Utf8).unwrap();
        assert_eq!(machine.string_bytes(text), b"eleven byte".to_vec());

        for (prefix, broken) in [(16, 0xFF), (13, 0xC3)] {
            let mut bytes = bytes_run(&mut machine, 0);
            push_bytes(&mut machine, &mut bytes, &vec![b'a'; prefix]);
            push_bytes(&mut machine, &mut bytes, &[broken]);
            let error =
                growable_finish(&mut machine, &bytes, f.program.str_layout, Validation::Utf8)
                    .unwrap_err();
            assert_eq!(error.message, "this string's bytes are not valid UTF-8");
        }
    }

    #[test]
    fn a_byte_finish_validates_and_relabels_to_a_string() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut bytes = bytes_run(&mut machine, 0);
        push_bytes(&mut machine, &mut bytes, "héllo".as_bytes());
        let text =
            growable_finish(&mut machine, &bytes, f.program.str_layout, Validation::Utf8).unwrap();
        assert_eq!(text, bytes.store);
        assert_eq!(machine.object_layout(text), f.program.str_layout);
        assert_eq!(machine.string_bytes(text), "héllo".as_bytes().to_vec());
        assert_eq!(reread_empty(&machine, bytes.owner), (0, 0));

        let mut broken = bytes_run(&mut machine, 0);
        push_bytes(&mut machine, &mut broken, &[b'a', 0xC3]);
        let error = growable_finish(
            &mut machine,
            &broken,
            f.program.str_layout,
            Validation::Utf8,
        )
        .unwrap_err();
        assert_eq!(error.message, "this string's bytes are not valid UTF-8");
        assert_eq!(
            reread(&machine, &broken),
            (2, broken.store, 16),
            "a refused finish leaves the owner as it was"
        );
        assert_eq!(machine.object_layout(broken.store), f.program.bytes_layout);
    }

    #[test]
    fn a_word_finish_relabels_to_an_array_without_validation() {
        let f = layouts();
        let mut machine = Machine::new(&f.program, 1 << 14);
        let mut words = words_run(&mut machine, &f, 4);
        let text = machine.new_string("only").unwrap();
        machine.set_payload(words.store, 0, text);
        growable_commit(&mut machine, &mut words, 1);
        let array = growable_finish(&mut machine, &words, f.array, Validation::None).unwrap();
        assert_eq!(array, words.store);
        assert_eq!(machine.object_layout(array), f.array);
        assert_eq!(machine.object_len(array), 1);
        assert_eq!(machine.payload(array, 0), text);
        // The three elements given back are a free block right after it.
        assert_eq!(machine.object_layout(array + 2), LayoutId::FREE);
        assert_eq!(machine.object_len(array + 2), 2);
        assert_eq!(reread_empty(&machine, words.owner), (0, 0));
    }

    fn reread_empty(machine: &Machine<'_>, owner: u64) -> (u64, u64) {
        (
            machine.payload(owner, GROWABLE_LEN),
            machine.payload(owner, GROWABLE_STORE),
        )
    }
}
