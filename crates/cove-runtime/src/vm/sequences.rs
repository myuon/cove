//! `Array` and `Vector`, and the keyed runs a `Set` and a `Map` finish into.
//!
//! The two are one shape apart. An `Array` holds its elements in the object,
//! one indirection nearer than a `Vector`, and cannot grow. A `Vector` is a
//! two-word header — `[len, store]` — over a growable store, and it pays that
//! indirection for the one thing an `Array` does not need: its identity is
//! observable, so growing must not move the object a program is holding.
//!
//! # An element is a run of words, and the length still counts elements
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) states the rule this
//! module turns on:
//!
//! > One slot is one eight-byte word. One value may occupy one or more
//! > consecutive slots.
//!
//! So an `Array<Point>` is a run of *two-word* elements laid end to end, not
//! a run of addresses that each name two words somewhere else. An element's
//! **stride** is its element layout's width, and every offset into a payload
//! below is an element position multiplied by it.
//!
//! What is *not* multiplied is anything a program can see. A header's `len`
//! is elements, a bound handed to `slice` is elements, and the capacity a
//! `push` compares against is elements. Keeping the two apart is the whole of
//! the arithmetic here: lengths and positions in elements, offsets in words.
//!
//! # An operand is an element, at whatever width one is
//!
//! An argument names a value location and carries its layout, so a whole
//! element arrives as a whole element. `push` and `set` therefore work at any
//! stride, as the operations that only *read* elements always did — those
//! read them out of the receiver, where the width was never in doubt.
//!
//! Until an argument carried a layout they refused, and so did `contains`
//! and `indexOf`. A call said where an operand began and never how wide it
//! was, so a `Vector<Point>.push(p)` would have written `p.x` into the store
//! and called it a `Point`, and refusing was the honest answer. None of those
//! operations is a builtin any more: each is `std.vector` over run
//! instructions whose element layout the lowering states.
//!
//! # The receiver is the vector, not a place that holds one
//!
//! `push`, `set`, `pop`, `remove` and `freeze` declare `var self` in the
//! schema, and a `var` parameter is ordinarily a
//! [`Repr::Addr`](cove_ir::Repr::Addr). None of them is passed one here: the
//! lowering hands over the vector itself, as a
//! [`Repr::Ref`](cove_ir::Repr::Ref). That is not a shortcut, it is what the
//! language says a `Vector` is — a copy of one is an alias, mutation through one copy is
//! visible through every other, and every one of them names the same two
//! words. Writing through the header is therefore already visible everywhere
//! the value went, and there is nothing to write back to the receiver's own
//! slot.
//!
//! # `freeze()` is the one that consumes
//!
//! It takes the store away from the header and hands it back as an `Array`,
//! in place and in O(1). That is only sound where the caller holds the only
//! handle, and uniqueness is not a question this backend can answer — a
//! handle is a word and words are not counted. It does not have to: the
//! checker proves it before the program runs. Since ADR 0058 it is not this
//! module's either: `freeze()` is `std.vector.freeze` over a word
//! `Inst::RunFinish`, which is `Machine::finish_words`.
//!
//! # Growth
//!
//! A store is allocated to exactly the elements it is built from, because
//! `Vector.of(1, 2, 3)` and `Array.toVector()` know the count and spare room
//! nobody asked for is room a program pays for. A `push` onto a full store
//! allocates one of **twice the capacity, from a floor of four**, copies the
//! elements across, and writes the new store into word 1 — the header does
//! not move, so no reference to it anywhere goes stale. Appending is
//! therefore amortised O(1). That growth, and `freeze()`'s relabel, are
//! [`crate::vm::exec::runs`]' — the one growable-run core a byte buffer grows
//! through too.
//!
//! A store never shrinks. `pop` and `remove` leave the room they vacate, so
//! that a program that fills and empties one does not reallocate on every
//! turn — but they **zero the words they vacate**, because a store's shape
//! says its whole capacity is elements and the collector reads it that way. A
//! dead element left in the spare room would be a root, and a vector used as
//! a work queue would retain everything it had ever held.

// `Array.contains`, `Array.indexOf`, `Vector.contains` and `Vector.indexOf`
// are not here: each is `std.array` or `std.vector`, a Cove loop over `==`
// (ADR 0058, #378). They were the last operations this module dispatched, when
// it was `vm::intrinsics::seq`, so what is left is the documentation above and
// the cases below, which exercise `Machine::ensure_growable`,
// `Machine::commit_growable`, `Machine::truncate_words`,
// `Machine::finish_words` and `Machine::keyed_run_in_order` over sequences and
// keyed runs. ADR 0073 deleted the rest of `vm::intrinsics` with the
// `IntrinsicCall` mechanism and moved these here.

#[cfg(test)]
mod tests {
    use cove_ir::{Layout, LayoutId, Program, Repr, Shape};

    use crate::error::RuntimeError;
    use crate::vm::exec::tests::Build;
    use crate::vm::exec::Machine;

    /// The program every case below is run against.
    ///
    /// One fixture with every family a run operation here reaches for, so that
    /// a test that declared only the families it thought it needed is not
    /// testing its own fixture. A hand-written program is the only kind any of
    /// them uses, for the reason [`crate::vm::exec::tests::Build`] gives.
    ///
    /// It was the fixture of every intrinsic arm's cases too, and declared the
    /// `Error` and `Result` families those arms built answers into, until ADR
    /// 0073 deleted the arms with the `IntrinsicCall` mechanism.
    ///
    /// A family is named by a `LayoutId` rather than by a `Repr` now, so the
    /// scalars are declared first and everything else is built out of them —
    /// which is also what makes an `Array<Point>` expressible here at all.
    fn world() -> Program {
        let mut build = Build::default();
        let _unit = build.word("Unit", Repr::Unit);
        let boolean = build.word("Bool", Repr::Bool);
        let int = build.word("Int", Repr::Int);
        let _float = build.word("Float", Repr::Float);
        let _duration = build.word("Duration", Repr::Duration);
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;

        let point = build.structure("Point", &[("x", int), ("y", int)]);

        for elem in [string, int, point] {
            build.layout(
                "Array",
                Shape::Elements {
                    elem,
                    growable: false,
                },
            );
            build.layout(
                "Vector",
                Shape::Elements {
                    elem,
                    growable: true,
                },
            );
            build.layout("Vector", Shape::Vector { elem });
            build.enumeration("Option", &[("None", vec![]), ("Some", vec![elem])]);
        }
        build.layout("Boxed", Shape::Boxed);
        // A `Range` is a struct with the three fields the design fixes, and
        // it is in here because a key sorts after every other family when it
        // is one.
        build.structure(
            "Range",
            &[("start", int), ("end", int), ("inclusive", boolean)],
        );
        for elem in [int, string] {
            build.layout("Set", Shape::Members { elem });
        }
        build.layout(
            "Map",
            Shape::Entries {
                key: int,
                value: int,
            },
        );
        build.layout(
            "Map",
            Shape::Entries {
                key: string,
                value: int,
            },
        );
        build.structure("MapEntry", &[("key", int), ("value", int)]);

        // A two-word element whose reference is **not** its first word.
        //
        // `Point` is two words of `Int` and `String` is one word that is a
        // reference; neither can tell a walk at the element's stride from a
        // walk at a stride of one, because the two coincide. A `Note` can: a
        // clear at the wrong stride leaves word 3 — the second note's text —
        // standing, and a clear at the wrong offset takes out word 1, which is
        // the first note's. `Vector<Note>` is what
        // `a_truncate_of_a_two_word_element_clears_the_reference_in_its_second_word`
        // is built over.
        let note = build.structure("Note", &[("at", int), ("text", string)]);
        build.layout(
            "Array",
            Shape::Elements {
                elem: note,
                growable: false,
            },
        );
        build.layout(
            "Vector",
            Shape::Elements {
                elem: note,
                growable: true,
            },
        );
        build.layout("Vector", Shape::Vector { elem: note });
        build.done()
    }

    /// The text of the string object at `addr`.
    fn read(machine: &Machine, addr: u64) -> String {
        String::from_utf8(machine.string_bytes(addr)).expect("a string holds valid UTF-8")
    }

    /// The first layout `wanted` accepts.
    fn find(program: &Program, wanted: impl Fn(&Layout) -> bool) -> LayoutId {
        program
            .layouts
            .iter()
            .position(wanted)
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every family")
    }

    /// The layout of a run of `elem` elements, growable or not.
    fn elements(program: &Program, elem: LayoutId, growable: bool) -> LayoutId {
        find(
            program,
            |layout| matches!(layout.shape, Shape::Elements { elem: e, growable: g } if e == elem && g == growable),
        )
    }

    /// The layout of a `Vector` header over `elem` elements.
    fn vector(program: &Program, elem: LayoutId) -> LayoutId {
        find(
            program,
            |layout| matches!(layout.shape, Shape::Vector { elem: e } if e == elem),
        )
    }

    /// The one-word layout of `repr`.
    fn scalar(program: &Program, repr: Repr) -> LayoutId {
        find(program, |layout| layout.shape == Shape::Word(repr))
    }

    /// The first layout the fixture declares under `name`.
    fn named(program: &Program, name: &str) -> LayoutId {
        find(program, |layout| &*layout.name == name)
    }

    /// The element words of a run-shaped object at `addr`.
    fn words_of(machine: &Machine, addr: u64) -> Vec<u64> {
        let layout = machine.program().layout(machine.object_layout(addr));
        let stride = match layout.shape {
            Shape::Elements { elem, .. } | Shape::Members { elem } => machine.words_of(elem),
            _ => 1,
        };
        machine.payload_run(addr, 0, machine.object_len(addr) * stride)
    }

    /// A `Vector` of `elem` holding `words`, which the caller holds rooted.
    ///
    /// The store is allocated to exactly the elements it was given. See
    /// [`crate::vm::exec::runs::growable_ensure`] for what happens when it
    /// fills. No run builds a vector in Rust since ADR 0058 moved
    /// `Array.toVector` into instructions, so only these cases do.
    fn vector_of(
        machine: &mut Machine,
        elem: LayoutId,
        words: &[u64],
    ) -> Result<u64, RuntimeError> {
        let store_layout = elements(machine.program(), elem, true);
        let header_layout = vector(machine.program(), elem);
        let stride = machine.words_of(elem).max(1) as usize;
        let len = words.len() / stride;
        let store = machine.new_object(store_layout, len as u32)?;
        // The store exists and nothing walks it, and allocating the header can
        // collect. It is released the moment the header exists, because the two
        // writes below cannot allocate and word 1 is what holds it afterwards.
        let mark = machine.temps();
        machine.push_temp(store);
        let header = machine.new_object(header_layout, 0);
        machine.release_temps(mark);
        let header = header?;
        machine.set_payload(header, 0, len as u64);
        machine.set_payload(header, 1, store);
        machine.set_payload_run(store, 0, words);
        Ok(header)
    }

    /// A `Vector<Int>` holding `values`, with a store of exactly that many.
    fn growable(machine: &mut Machine, values: &[i64]) -> u64 {
        let int = scalar(machine.program(), Repr::Int);
        let words: Vec<u64> = values.iter().map(|value| *value as u64).collect();
        vector_of(machine, int, &words).expect("the fixture declares every family")
    }

    /// `Vector.push(value)`, which since ADR 0062 is what `std.vector.push`
    /// lowers to: `growable-ensure` of one, the element's words into the store
    /// at the logical length, and `growable-commit` of one.
    ///
    /// The ensure comes first because it may allocate and so replace the store,
    /// which is why the store is read *after* it — the same order the window's
    /// rows are in, and the reason the reservation rule puts the store read
    /// where it does.
    fn push(
        machine: &mut Machine,
        items: u64,
        elem: LayoutId,
        words: &[u64],
    ) -> Result<(), RuntimeError> {
        let storage = cove_ir::Storage::Words(elem);
        machine.ensure_growable(items, storage, 1)?;
        let stride = machine.program().layout(elem).width();
        let at = machine.payload(items, 0) as u32 * stride;
        let store = machine.payload(items, 1);
        machine.set_payload_run(store, at, words);
        machine.commit_growable(items, storage, 1)
    }

    /// An `Array<Point>` is a run of two-word elements. Everything that walks
    /// one counts in elements and offsets in words, and this is where that
    /// distinction is load-bearing: a length of three is three `Point`s and
    /// six words, and a `pop` answers a pair.
    #[test]
    fn an_array_of_points_is_walked_at_a_two_word_stride() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let layout = elements(&program, point, false);
        let items = machine.new_object(layout, 3).unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4, 5, 6]);

        // The header's length is elements.
        assert_eq!(machine.object_len(items), 3);

        // And a `Vector` over the same elements keeps both: three in the
        // header's count, six in the store.
        let grown = vector_of(&mut machine, point, &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(machine.payload(grown, 0), 3);
        let store = machine.payload(grown, 1);
        assert_eq!(machine.object_len(store), 3);
        assert_eq!(words_of(&machine, store), vec![1, 2, 3, 4, 5, 6]);

        // A truncate — `pop`'s and `remove`'s last step — takes whole elements
        // off and zeroes every word of each.
        machine.truncate_words(grown, point, 2).unwrap();
        assert_eq!(machine.payload(grown, 0), 2);
        assert_eq!(words_of(&machine, store), vec![1, 2, 3, 4, 0, 0]);
        machine.truncate_words(grown, point, 0).unwrap();
        assert_eq!(words_of(&machine, store), vec![0, 0, 0, 0, 0, 0]);
    }

    /// The other side of the stride: an element of any width is written whole.
    ///
    /// A push refused until an argument carried its layout — reading an
    /// element was never in doubt, because the receiver says how wide one is,
    /// and storing one meant being handed a whole value that a base slot could
    /// not describe. `contains` and `indexOf`, which compared against one, are
    /// `std.array` and `std.vector` loops over `==` now.
    #[test]
    fn an_element_wider_than_a_word_arrives_whole() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let grown = vector_of(&mut machine, point, &[1, 2, 3, 4]).unwrap();
        let vectors = machine.object_layout(grown);

        // A `push` writes both words at the element's own stride.
        push(&mut machine, grown, point, &[5, 6]).unwrap();
        // The store grew, so its spare room is zeroed room past the length —
        // the three elements are the first six words of it.
        let store = machine.payload(grown, 1);
        assert_eq!(machine.payload_run(store, 0, 6), vec![1u64, 2, 3, 4, 5, 6]);
        assert_eq!(machine.object_layout(grown), vectors);
    }

    /// A store is traced by its element layout's reference map and searched
    /// at its width, so a value of another family put into one would be both
    /// a collection following the wrong words and a search comparing them.
    /// That is the one thing an operand's layout still has to be held to.
    #[test]
    fn an_element_of_another_family_is_refused_rather_than_stored() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let int = scalar(&program, Repr::Int);
        let layout = elements(&program, point, true);
        let store = machine.new_object(layout, 1).unwrap();
        let grown = machine.new_object(vector(&program, point), 0).unwrap();
        machine.set_payload(grown, 1, store);
        // A word window is given its element layout by the lowering rather than
        // by an operand, so what it holds to the store's family is the owner: a
        // vector of another element is refused before a word moves.
        let error = push(&mut machine, grown, int, &[1]).unwrap_err();
        assert_eq!(
            error.message,
            "a growable run of `Int` was expected here, and this object is not one"
        );
    }

    /// The whole point of the indirection: the header a program is holding
    /// keeps its address while the store beneath it is replaced.
    #[test]
    fn a_vector_grows_without_moving() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);
        assert_eq!(machine.object_len(store), 2);

        let int = scalar(&program, Repr::Int);
        push(&mut machine, items, int, &[3]).unwrap();
        let grown = machine.payload(items, 1);
        assert_ne!(grown, store, "a full store is replaced");
        // Twice the capacity, from a floor of four.
        assert_eq!(machine.object_len(grown), 4);
        assert_eq!(machine.payload(items, 0), 3);
        assert_eq!(words_of(&machine, grown), vec![1, 2, 3, 0]);

        // And the next push fits without replacing anything.
        push(&mut machine, items, int, &[4]).unwrap();
        assert_eq!(machine.payload(items, 1), grown);
        assert_eq!(machine.payload(items, 0), 4);
    }

    /// An empty vector's store starts at nothing, so the first push is the
    /// one that takes the floor.
    #[test]
    fn an_empty_vector_grows_to_the_floor() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[]);
        push(&mut machine, items, scalar(&program, Repr::Int), &[7]).unwrap();
        assert_eq!(
            u64::from(machine.object_len(machine.payload(items, 1))),
            crate::vm::exec::runs::MIN_GROWABLE_ELEMENTS
        );
        assert_eq!(machine.payload(items, 0), 1);
    }

    /// A copy of a `Vector` is an alias, and every one of them names the same
    /// two words — so a growth through one is a growth every other sees.
    #[test]
    fn a_growth_is_visible_through_every_copy() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let alias = items;

        push(&mut machine, items, scalar(&program, Repr::Int), &[3]).unwrap();
        assert_eq!(machine.payload(alias, 0), 3);
        let store = machine.payload(alias, 1);
        assert_eq!(machine.payload_run(store, 0, 3), vec![1u64, 2, 3]);
    }

    /// The store keeps its room and loses its dead elements: the words a
    /// truncate vacates are zeroed, because a store's whole capacity is
    /// elements as far as the collector is concerned — and a truncate only
    /// lowers, so a length above the current one is refused with nothing
    /// written.
    ///
    /// Every length a truncate can be given is here: a partial one, the length
    /// it already has (a no-op, which must still not clear anything), nought
    /// (the whole live prefix), nought again on a vector that is already empty,
    /// and the two that are not lengths at all — above the current one and
    /// below zero. Both of those are refused **with the length unchanged and
    /// nothing cleared**, which is the half a length assertion alone would miss:
    /// a truncate that zeroed first and refused afterwards would pass on the
    /// length and have destroyed the elements.
    #[test]
    fn a_truncate_shortens_the_vector_and_clears_the_words_it_vacates() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let items = growable(&mut machine, &[1, 2, 3]);
        let store = machine.payload(items, 1);

        machine.truncate_words(items, int, 1).unwrap();
        assert_eq!(machine.payload(items, 0), 1);
        assert_eq!(
            machine.payload(items, 1),
            store,
            "the store is not replaced"
        );
        assert_eq!(words_of(&machine, store), vec![1, 0, 0]);
        // A truncate to the length it already has writes nothing.
        machine.truncate_words(items, int, 1).unwrap();
        assert_eq!(words_of(&machine, store), vec![1, 0, 0]);

        for len in [2, -1] {
            let error = machine.truncate_words(items, int, len).unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "`growableTruncate` would take a length of 1 to {len}, and a truncate only \
                     lowers a length"
                )
            );
            assert_eq!(machine.payload(items, 0), 1);
            assert_eq!(
                words_of(&machine, store),
                vec![1, 0, 0],
                "a refused truncate clears nothing either"
            );
        }

        // Down to nought: the whole live prefix, and the store is still the
        // store — a truncate gives back no memory, only length.
        machine.truncate_words(items, int, 0).unwrap();
        assert_eq!(machine.payload(items, 0), 0);
        assert_eq!(machine.payload(items, 1), store);
        assert_eq!(words_of(&machine, store), vec![0, 0, 0]);

        // And a vector that is already empty: a truncate to the length it has
        // is the same no-op whether that length is its first or its last, and
        // one to a length it does not have is refused in the same words.
        let empty = growable(&mut machine, &[]);
        machine.truncate_words(empty, int, 0).unwrap();
        assert_eq!(machine.payload(empty, 0), 0);
        let error = machine.truncate_words(empty, int, 1).unwrap_err();
        assert_eq!(
            error.message,
            "`growableTruncate` would take a length of 0 to 1, and a truncate only lowers a \
             length"
        );
    }

    /// **A truncate clears at the element's own stride, so a reference in a
    /// word other than the element's first is cleared too.**
    ///
    /// `a_string_a_truncate_takes_out_is_collected` below is a one-word
    /// element, where the stride and the word are the same number, and
    /// `an_array_of_points_is_walked_at_a_two_word_stride` is two words that
    /// hold no reference at all: between them they leave the case that matters
    /// uncovered. A `Note` is `{at: Int, text: String}` — two words, with the
    /// reference in word 1. A clear at a stride of one leaves the taken note's
    /// text standing, which is a root the collector follows to a string the
    /// program can no longer reach; a clear from the wrong offset takes out
    /// word 1, which is the *kept* note's text and a string it still holds.
    /// Only a walk that knows both the stride and the offset passes.
    #[test]
    fn a_truncate_of_a_two_word_element_clears_the_reference_in_its_second_word() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let note = named(&program, "Note");
        let text = program.str_layout;
        let store = machine
            .new_object(elements(&program, note, true), 2)
            .unwrap();
        machine.push_temp(store);
        let items = machine.new_object(vector(&program, note), 0).unwrap();
        machine.push_temp(items);
        // Each note is written whole before the next string is allocated, so
        // what is already in the store is traced through it by the allocation
        // that follows — the store is the root, and word 1 of each element is
        // where its reference map says a reference is.
        let kept = machine.new_string("kept").unwrap();
        machine.set_payload(store, 0, 11);
        machine.set_payload(store, 1, kept);
        let taken = machine.new_string("taken").unwrap();
        machine.set_payload(store, 2, 22);
        machine.set_payload(store, 3, taken);
        machine.set_payload(items, 0, 2);
        machine.set_payload(items, 1, store);

        machine.truncate_words(items, note, 1).unwrap();
        assert_eq!(
            words_of(&machine, store),
            vec![11, kept, 0, 0],
            "both words of the vacated note, and neither word of the kept one"
        );
        machine.collect();
        assert_eq!(
            machine.object_layout(taken),
            LayoutId::FREE,
            "the taken note's text was swept"
        );
        assert_eq!(machine.object_layout(kept), text, "the kept one's was not");
        assert_eq!(read(&machine, kept), "kept");
    }

    /// **A collection immediately before a truncate and immediately after it
    /// each see a consistent vector — and one *during* a truncate cannot
    /// happen, which is the property asserted here rather than the property
    /// tested.**
    ///
    /// "During" is the case that would matter, and it is the case that cannot
    /// be constructed: `runs::growable_truncate` is a clear of words that
    /// already exist followed by one payload write, and neither allocates, so
    /// there is no safepoint between the clear and the length write for a
    /// collection to be scheduled at. Writing a test that tried to schedule one
    /// there would be writing a test that can never fail, so what is asserted
    /// instead is the fact that makes it impossible — the machine's allocation
    /// counter does not move across a truncate — and then the two moments that
    /// *are* observable are checked on either side of it.
    ///
    /// That is also the argument for the instruction being one instruction:
    /// the interval this test says does not exist is exactly the interval a
    /// split form would name. See `cove_ir::Inst::GrowableTruncate`.
    #[test]
    fn a_collection_before_a_truncate_and_after_it_agree_and_none_happens_inside() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let text = program.str_layout;
        let store = machine
            .new_object(elements(&program, text, true), 3)
            .unwrap();
        machine.push_temp(store);
        let items = machine.new_object(vector(&program, text), 0).unwrap();
        machine.push_temp(items);
        for (at, word) in ["a", "b", "c"].iter().enumerate() {
            let held = machine.new_string(word).unwrap();
            machine.set_payload(store, at as u32, held);
        }
        machine.set_payload(items, 0, 3);
        machine.set_payload(items, 1, store);
        let held = machine.payload_run(store, 0, 3);

        // Before: at the full length every element is a root, and a collection
        // keeps all three.
        machine.collect();
        for addr in &held {
            assert_eq!(machine.object_layout(*addr), text, "{addr} survived");
        }

        // During: there is no during. Nothing here allocates, so the clear and
        // the length write are one step as far as the collector is concerned.
        let allocations = machine.allocations();
        machine.truncate_words(items, text, 2).unwrap();
        assert_eq!(
            machine.allocations(),
            allocations,
            "a truncate allocates nothing, so no collection can run between the clear and the \
             length write"
        );

        // After: the vacated word was already cleared when the collection
        // began, so what the vector no longer holds is swept and what it still
        // holds is untouched. One element is taken rather than two, because two
        // adjacent free objects are coalesced into one block and only the first
        // of them keeps a header to read.
        machine.collect();
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(machine.object_layout(held[0]), text);
        assert_eq!(machine.object_layout(held[1]), text);
        assert_eq!(read(&machine, held[0]), "a");
        assert_eq!(read(&machine, held[1]), "b");
        assert_eq!(
            machine.object_layout(held[2]),
            LayoutId::FREE,
            "the element the truncate vacated is no longer a root"
        );
    }

    /// `freeze()` hands back the store it was already holding, and empties
    /// the vector.
    ///
    /// The array's address *is* the store's, which is the whole of what makes
    /// this O(1): nothing was copied, the header was rewritten from the
    /// growable family to the fixed one, and every element stayed where it
    /// was.
    #[test]
    fn freeze_takes_the_store_and_hands_it_back_as_an_array() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1, 2]);
        let store = machine.payload(items, 1);
        let int = scalar(&program, Repr::Int);

        let frozen = machine
            .finish_words(items, elements(&program, int, false), int)
            .unwrap();
        assert_eq!(frozen, store);
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);
        assert!(matches!(
            machine
                .program()
                .layout(machine.object_layout(frozen))
                .shape,
            Shape::Elements {
                growable: false,
                ..
            }
        ));

        // Consumed: the header stays where it is and answers that it has no
        // storage, which is the state a checked program cannot reach, and a
        // second finish is refused.
        assert_eq!(machine.payload(items, 1), 0);
        let error = machine
            .finish_words(items, elements(&program, int, false), int)
            .unwrap_err();
        assert_eq!(error.message, crate::builtins::CONSUMED_VECTOR);
    }

    /// A `push` grows the store past the length, and what `freeze()` answers
    /// is the length rather than the capacity.
    ///
    /// The words in between are given back as a free block, which the sweep
    /// has to be able to walk over: an object whose header says two elements
    /// sitting in room for four would otherwise leave two words that belong
    /// to nothing.
    #[test]
    fn freeze_gives_back_the_room_the_vector_grew_into() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let items = growable(&mut machine, &[1]);
        push(&mut machine, items, scalar(&program, Repr::Int), &[2]).unwrap();
        let store = machine.payload(items, 1);
        assert_eq!(
            u64::from(machine.object_len(store)),
            crate::vm::exec::runs::MIN_GROWABLE_ELEMENTS
        );

        let int = scalar(&program, Repr::Int);
        let frozen = machine
            .finish_words(items, elements(&program, int, false), int)
            .unwrap();
        assert_eq!(frozen, store);
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);

        // The heap is still walkable, which a collection is the way to ask:
        // the sweep walks every object from the first to the bump pointer,
        // and a released run that is not a free block of its own is where
        // that walk would leave the sequence.
        machine.push_temp(frozen);
        while machine.collected().collections == 0 {
            growable(&mut machine, &[3, 4, 5, 6, 7, 8, 9, 10]);
        }
        assert_eq!(words_of(&machine, frozen), vec![1, 2]);
    }

    /// A header whose store word is null is still a state the machine can be
    /// handed, and every run instruction over it refuses it.
    ///
    /// A checked program never produces one — `cove_sema::unique` proves a
    /// vector is not read after its `freeze()` — so it is built by hand here,
    /// because the reading of it is what is under test. No `Vector` method is
    /// dispatched by name any more; a run instruction over the vector is below
    /// every method, and answers the one internal-invariant sentence.
    #[test]
    fn a_vector_with_no_storage_refuses_every_run_instruction() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let header = machine.new_object(vector(&program, int), 0).unwrap();

        let pushed = push(&mut machine, header, int, &[1]).unwrap_err();
        let finished = machine
            .finish_words(header, elements(&program, int, false), int)
            .unwrap_err();
        let truncated = machine.truncate_words(header, int, 0).unwrap_err();
        for error in [pushed, finished, truncated] {
            assert_eq!(error.message, crate::builtins::CONSUMED_VECTOR);
            assert_eq!(error.rule, None);
        }
    }

    /// **A string a truncate takes out of a `Vector<String>` is garbage at the
    /// next collection, and the strings it leaves are not.**
    ///
    /// What `pop` and `remove` rely on the cleared words for: the store is still
    /// reachable through the vector, and its spare room is traced as elements,
    /// so a vacated word left holding the address would keep the string alive.
    #[test]
    fn a_string_a_truncate_takes_out_is_collected() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let text = program.str_layout;
        let store = machine
            .new_object(elements(&program, text, true), 2)
            .unwrap();
        machine.push_temp(store);
        let items = machine.new_object(vector(&program, text), 0).unwrap();
        machine.push_temp(items);
        let kept = machine.new_string("kept").unwrap();
        machine.set_payload(store, 0, kept);
        let taken = machine.new_string("taken").unwrap();
        machine.set_payload(store, 1, taken);
        machine.set_payload(items, 0, 2);
        machine.set_payload(items, 1, store);
        let mark = machine.temps();
        machine.release_temps(mark - 1);

        machine.truncate_words(items, text, 1).unwrap();
        machine.collect();
        assert_eq!(
            machine.object_layout(taken),
            LayoutId::FREE,
            "the taken string was swept"
        );
        assert_eq!(machine.object_layout(kept), text, "the kept one was not");
        assert_eq!(read(&machine, kept), "kept");
    }

    /// The one window a builtin has to get rooting wrong: a growth allocates a
    /// larger store while the elements it is about to copy are reachable only
    /// through the header.
    ///
    /// The heap is small and full of dead objects, so that allocation
    /// collects. The header is pushed as a temporary root by hand because
    /// there is no frame here to hold it — which is what a real call has, and
    /// what the operand words rely on everywhere else. That is the whole
    /// invariant: the old store is traced *through* the header, so it and its
    /// elements survive the allocation that replaces it.
    #[test]
    fn a_growth_holds_the_store_it_copies_from_across_the_allocation() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let text = program.str_layout;
        let store_layout = elements(&program, text, true);
        let header_layout = vector(&program, text);

        let store = machine.new_object(store_layout, 1).unwrap();
        machine.push_temp(store);
        let items = machine.new_object(header_layout, 0).unwrap();
        machine.push_temp(items);
        let kept = machine.new_string("the one that must survive").unwrap();
        machine.push_temp(kept);
        machine.set_payload(store, 0, kept);
        machine.set_payload(items, 0, 1);
        machine.set_payload(items, 1, store);
        // Dead strings, two words each, until the heap is exactly full — so
        // that the larger store below cannot fit and has to collect.
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        push(&mut machine, items, text, &[kept]).unwrap();
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        let grown = machine.payload(items, 1);
        assert_ne!(grown, store, "a full store is replaced");
        assert_eq!(machine.payload(items, 0), 2);
        assert_eq!(
            read(&machine, machine.payload(grown, 0)),
            "the one that must survive"
        );
        assert_eq!(
            read(&machine, machine.payload(grown, 1)),
            "the one that must survive"
        );
    }

    /// A store of `keys` of `elem`, one after another, as a keyed finish hands
    /// [`Machine::keyed_run_in_order`] the run it is about to relabel.
    fn keyed_store(machine: &mut Machine, elem: LayoutId, keys: &[&[u64]]) -> u64 {
        let layout = elements(machine.program(), elem, false);
        let store = machine
            .new_object(layout, keys.len() as u32)
            .expect("the fixture's heap is large enough");
        let words: Vec<u64> = keys.iter().flat_map(|key| key.iter().copied()).collect();
        machine.set_payload_run(store, 0, &words);
        store
    }

    /// The keyed-finish check answers from the oracle's `MapKey` order, so it
    /// has to be seen refusing: a check that could not fail would pass every
    /// `Set` and `Map` a test builds whatever order `std.set`, `std.map` and
    /// `std.dynamic.order` left them in.
    ///
    /// `Point` is the family that matters. A scalar or a `String` key is one
    /// the old check compared directly, and every other family went through
    /// the machine's own Rust order walk, which ADR 0068's Phase 5 deleted; a
    /// struct is compared by its name and then field by field, which is
    /// `MapKey::Struct`'s derived order.
    #[test]
    fn a_keyed_finish_is_checked_against_the_oracles_order_and_can_fail() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let point = named(&program, "Point");
        let string = named(&program, "String");
        let width = machine.words_of(point);

        let ascending = keyed_store(&mut machine, point, &[&[1, 2], &[1, 3], &[2, 0]]);
        assert!(machine.keyed_run_in_order(point, ascending, width, width, 3));

        // The second field decides once the first is equal: `(1, 3)` before
        // `(1, 2)` is out of order.
        let swapped = keyed_store(&mut machine, point, &[&[1, 3], &[1, 2]]);
        assert!(!machine.keyed_run_in_order(point, swapped, width, width, 2));

        // The first field decides before the second: `(2, 0)` before `(1, 9)`.
        let first = keyed_store(&mut machine, point, &[&[2, 0], &[1, 9]]);
        assert!(!machine.keyed_run_in_order(point, first, width, width, 2));

        // Distinct as well as ascending: a key twice is refused.
        let twice = keyed_store(&mut machine, point, &[&[1, 2], &[1, 2]]);
        assert!(!machine.keyed_run_in_order(point, twice, width, width, 2));

        // A `String` key by its bytes, through the same conversion.
        let (a, b) = (
            machine.new_string("a").unwrap(),
            machine.new_string("b").unwrap(),
        );
        let words = keyed_store(&mut machine, string, &[&[b], &[a]]);
        assert!(!machine.keyed_run_in_order(string, words, 1, 1, 2));
        let words = keyed_store(&mut machine, string, &[&[a], &[b]]);
        assert!(machine.keyed_run_in_order(string, words, 1, 1, 2));
    }
}
