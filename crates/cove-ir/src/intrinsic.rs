//! The core operations a builtin call names statically, and what generating
//! code for one has to be ready for.
//!
//! [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! decides that a runtime call which stays in Rust "is not dispatched by
//! receiver and method strings. Lowering resolves it to an intrinsic
//! identifier with a fixed operand and result shape" — and that "the VM may
//! use Rust dispatch on the numeric identifier. Native code binds a direct
//! helper address or a compact helper table entry." [`Intrinsic`] is that
//! identifier: a closed, numbered enum with one variant per operation
//! `crate::vm::intrinsics::call` (in `cove-runtime`) has been taught, rather
//! than the `(receiver, operation)` pair of strings [`crate::IntrinsicSite`] used
//! to carry.
//!
//! # Effects are read off the implementation, not guessed
//!
//! The ADR also asks for "compiler-visible effects": `may_allocate`,
//! `may_collect`, `may_raise`, `may_block`, `bulk_work`, `reads_memory` and
//! `writes_memory`. [`Effects`] is that bitset, and [`Intrinsic::effects`]
//! assigns one to every variant by reading what its VM arm actually does —
//! not by a rule of thumb applied uniformly to every operation of a family.
//! Where a path is ambiguous, the flag is set: a superset here costs
//! generated code a check it did not need; a missing flag costs it a bug a
//! collection or an unwound error would not survive.

use std::fmt;

/// One core operation an `IntrinsicCall` may name.
///
/// A variant is named `ReceiverOperation` in upper camel case — `Float`'s
/// `parse` is [`Intrinsic::FloatParse`] — because that pair is
/// the language reference's own naming of it: [`Intrinsic::receiver`] and
/// [`Intrinsic::operation`] answer the two halves back apart, and
/// [`Display`](fmt::Display) prints them the way `cove-ir`'s printer and
/// `cove-runtime`'s error messages always have, `Receiver.operation`.
///
/// The set is closed and numbered rather than named because that is the
/// whole of what this ADR changes: the VM matches on the variant instead of
/// on a pair of strings, and native code can bind a helper address to it
/// directly instead of reconstructing a name to dispatch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    // `StringRefuseByteRange` stood here, the refusal of a byte range
    // `std.stringbuilder`'s `appendRange` had found wrong, until issue #432
    // made its five sentences `std.stringbuilder.byteRangeRefusalMessage` and
    // its raise ADR 0067's `core.refuse`.
    // `FloatToInt` stood here, `f64::trunc` and three refusals worded in
    // Rust, until issue #432 made it `std.float.toInt`: a Cove body over
    // `core.floatTruncate`, which is `Inst::FloatTruncate` — ADR 0064's
    // checked typed conversion, answering an integer and whether there is
    // one — and the three sentences in Cove on the path that refuses
    // (ADR 0071).
    //
    // `FloatParse` is the last, and **no program emits it**: issue #432 made
    // `Float.parse` `std.float.parse`, a Cove body with no instruction of its
    // own (ADR 0072), so the lowering never names this variant. It stays, with
    // its VM arm and its native protocol, only because the mechanism does:
    // the cases that hold the mechanism to account build the call by hand, and
    // the change that deletes `Intrinsic` and `Inst::IntrinsicCall` deletes it.
    FloatParse,
}

/// Every [`Intrinsic`], in declaration order.
///
/// What [`Intrinsic::from_names`] searches and what this module's own tests
/// walk to check the table has no gap and no duplicate — the two ways a hand-
/// written list like this one goes wrong.
pub const ALL: &[Intrinsic] = &[Intrinsic::FloatParse];

/// How many variants there are, as the width of a per-variant table.
///
/// [`Intrinsic::index`] numbers every variant below this, so a `[T; COUNT]` has
/// exactly one row per variant and no row that is not one. Taken from [`ALL`]
/// rather than written as a numeral for the reason [`Intrinsic::index`] is a
/// search of it: ADR 0064's Decision 1 says the set only shrinks, so the number
/// is going to change, and a numeral somewhere else is a second thing to
/// remember to change with it.
pub const COUNT: usize = ALL.len();

impl Intrinsic {
    /// The type the operation belongs to: `Array`, `String`, `Map`, `Int`.
    ///
    /// (`Value` was the receiver of the four rules over any value's layout
    /// that [ADR
    /// 0068](../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)
    /// moved into Cove: `Value.order` in its Phase 3, `Value.renderInto` in
    /// Phase 4b-ii, and `Value.admitKey`, which a keyed collection's
    /// standard-library body reached through `core.admitKey`, in Phase 4c,
    /// when its refusal was worded in `std.dynamic` and in the walks the
    /// lowering composes. `Any` was the receiver of `Any.equals`, `==` on two
    /// erased values, until the ADR's Phase 2 made that
    /// `std.dynamic.equals`.)
    pub const fn receiver(self) -> &'static str {
        match self {
            Intrinsic::FloatParse => "Float",
        }
    }

    /// The operation's own name: `split`, `join`, `parse`.
    pub const fn operation(self) -> &'static str {
        match self {
            Intrinsic::FloatParse => "parse",
        }
    }

    /// The intrinsic named `receiver.operation`, if there is one.
    ///
    /// A linear search of [`ALL`], which is the whole set this backend has
    /// been taught and small enough that a search of it costs nothing next
    /// to lowering the call it names. Nothing after lowering calls this:
    /// [`crate::IntrinsicSite`] carries the variant itself once one is found, for
    /// the reason this ADR exists — dispatch is never again a string
    /// comparison.
    pub fn from_names(receiver: &str, operation: &str) -> Option<Intrinsic> {
        ALL.iter().copied().find(|intrinsic| {
            intrinsic.receiver() == receiver && intrinsic.operation() == operation
        })
    }

    /// Where this variant is in [`ALL`], for a table with one row per variant.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
    /// Decision 7 asks for allocations, allocated words, proportional work and
    /// machine-code bytes reported **per variant**, and a report that wants one
    /// row each needs somewhere to put the row. `cove_native::IntrinsicCode` is
    /// the first caller; it indexes a `[u64; COUNT]` by this.
    ///
    /// It is a search of [`ALL`] rather than a `match` arm per variant, and
    /// that is Decision 1's doing rather than an economy. The variant set "may
    /// lose variants and may never gain one", so every migration deletes a
    /// line from the enum and from [`ALL`] — and a hand-written `match`
    /// returning 0, 1, 2… would have to be renumbered from the deletion
    /// downwards each time, which is thirty chances to write the wrong number
    /// in exchange for nothing. Deriving the index from [`ALL`] means the one
    /// list that already has to be edited is the only list that has to be
    /// edited. The search costs what [`Intrinsic::from_names`]'s does, on a
    /// path that runs once per emitted call site at compile time.
    pub fn index(self) -> usize {
        ALL.iter()
            .position(|each| *each == self)
            .expect("`ALL` names every variant; `all_names_every_variant_once` is what holds it")
    }

    /// What the intrinsic is about, which is what the verifier holds its
    /// operands to.
    ///
    /// See [`Category`]. There is no collection category, and that is the
    /// point: ADR 0058 moved every collection operation into run instructions
    /// and the standard library, and a new one written as an intrinsic has no
    /// category to be.
    pub const fn category(self) -> Category {
        match self {
            Intrinsic::FloatParse => Category::Scalar,
            // `String.refuseByteRange` was the last `Category::Text`, until
            // issue #432 made it Cove.
            // `Value.admitKey` was the last `Category::Value`, a rule over
            // whatever layout the key has, until ADR 0068's Phase 4c.
        }
    }

    /// The operands the intrinsic takes and the answer it writes, as the
    /// verifier checks every `IntrinsicCall` against them.
    ///
    /// ADR 0058: "Lowering resolves it to an intrinsic identifier with a fixed
    /// operand and result shape." This is that shape, written down once, so
    /// that `crate::verify` refuses a call whose argument count, argument
    /// layouts or answer layout disagree with it — and so that the machine's
    /// arms no longer re-check any of the three on every call (#378, P5-3).
    pub const fn signature(self) -> Signature {
        use Carried as K;
        use Class as C;
        const fn fixed(operands: &'static [Class], result: Class) -> Signature {
            Signature { operands, result }
        }
        match self {
            Intrinsic::FloatParse => fixed(&[C::Str], C::ResultOf(K::Float)),
        }
    }

    /// The effects generated code has to be ready for when it calls this
    /// intrinsic.
    ///
    /// See [`Effects`]'s fields for what each one asks of generated code.
    /// Assigned by reading the VM arm each intrinsic dispatches to in
    /// `cove-runtime`'s `vm::intrinsics`, not by a rule applied to every
    /// member of a family — two operations of the same receiver may answer
    /// differently, the way `Float.toInt` read nothing past its one word
    /// while it was here and [`Intrinsic::FloatParse`] reads a whole
    /// `String`.
    pub const fn effects(self) -> Effects {
        use Effects as E;
        // `MAY_RAISE` is language-level failure only (#378, Q5.3). An arm no
        // longer re-checks its operand count or types — the verifier refused
        // any call that disagrees with [`Intrinsic::signature`] — so the
        // `Err` those checks answered is not a path any verified program has,
        // and the flag says what a program can actually be stopped by: a
        // refusal the language defines (a byte range
        // outside its string, a key it does not admit), a value nested past what a walk
        // of it may reach, and an exhausted heap — which is why every
        // intrinsic that allocates carries it.
        //
        // Nothing below reaches the scheduler or a Host boundary, which is a
        // fact about the whole family: it is why there is no flag for it.
        let raise = E::MAY_RAISE;
        let allocate = E::MAY_ALLOCATE.union(E::MAY_COLLECT).union(raise);
        match self {
            // `Value.renderInto` stood here, the rendering of a value whose
            // layout did not say what it was, until ADR 0068's Phase 4b-ii
            // made it `std.dynamic.renderInto`: a Cove walk over a view of
            // the box, which charges its work an instruction at a time.

            // `split` and `replace` stood here, the last two readers of a
            // `String` that allocated what they answered, and they left
            // together at the end of issue #454's Step 3: `std.string` bodies
            // over `core.stringFind`, raising on an empty needle through ADR
            // 0067's `core.refuse`.
            // No predicate or search of a `String` is here any more, and
            // that is the whole of ADR 0046's four. `startsWith` and
            // `endsWith` left first, for ADR 0064's reason: a bounded byte
            // comparison is a Cove loop over `byteAt`, the same reading and
            // the same proportionality, charged an instruction at a time
            // instead of declared a flag at a time. `contains` left next and
            // differently — ADR 0065 gave it `Inst::RunFind` to stand on,
            // because its work is proportional to a haystack the caller did
            // not size and a dispatch per byte of one is what an instruction
            // is for. `indexOf` followed it onto the same instruction, and the
            // byte offset that instruction answers becomes a character
            // position in `std.string.indexOf`, a walk of the prefix's lead
            // bytes. With it went the last arm here that read without
            // allocating.
            // `codePointAtByte` is not here: it is `std.string`, a decode in
            // Cove over one run load a byte, and neither is `fromCodePoint`,
            // which is that decode run backwards over a `std.stringbuilder`
            // run. It was the last `String` arm here whose operand was a
            // *word*: it read nothing off the heap and what it did was
            // allocate, which was `Float.toInt`'s and `Float.format`'s shape
            // and not any other `String` operation's. Every `String` arm left
            // below reads a receiver.
            // The byte-range refusal stood here, the one arm that raised and
            // allocated nothing. Issue #432 made its sentence
            // `std.stringbuilder.byteRangeRefusalMessage`, a `String` a Cove
            // body builds, and its raise ADR 0067's `core.refuse`.

            // No `Array` or `Vector` operation is here. `contains` and
            // `indexOf` are `std.array` and `std.vector` loops over `==`;
            // `slice`, `toVector`, `push`, `set`, `pop`, `remove`, `freeze` and
            // `toArray` are each Cove over run instructions.

            // No `Set` or `Map` operation is here: the literals, the
            // membership tests, `get`, `inserted`, `removed`, `toArray`, `keys`
            // and `values` are `std.set` and `std.map` over the three `Value`
            // intrinsics at the end, run copies and slices, and a keyed finish
            // (ADR 0059).

            // `Int.toFloat` and `Duration.nanos` are not here: each is an
            // `Inst::Convert` (#378, P5-2).

            // **No arm here is `E::NONE` any more, and that is the whole of
            // what issue #454's Step 2 did to this function.** There used to
            // be a scalar family of its own words with nothing on the heap to
            // read and nothing that can fail — `Float.abs`, `Float.min`,
            // `Float.max`, `Float.round`, `Float.sqrt`, the operations IEEE
            // 754 answers for every input — and every one of them is an
            // instruction now. What is left below allocates, or refuses, or
            // both. See `every_intrinsic_left_can_be_refused` for the form
            // that takes as a test and for what it costs `cove-native`.
            //
            // The parser left reads a `String` receiver's bytes and allocates
            // the message an `Err` carries. It is not proportional to anything
            // past the one receiver or the one answer, which is short enough
            // that this backend does not charge it as bulk work.
            //
            // There were three parsers until issue #454's Step 4. `Int.parse`
            // left first, as `std.int.parse`: a `core.byteLength` and one
            // `byteAt` a byte with an accumulator that runs negative so that
            // neither end of `Int` needs a magnitude `Int` has not got.
            // `Int.parseRadix` followed as `std.int.parseRadix` once ADR 0067
            // gave a Cove body `core.refuse` to stop the run with, which is
            // what it does for a radix outside `2..=36`.
            Intrinsic::FloatParse => allocate.union(E::READS_MEMORY),
            // `Float.format` stood beside this, the last `Float` intrinsic that
            // built a `String`; it is `std.float.format` now, exact fixed-point
            // decimal over base-`10^9` limbs, refusing a digit count outside
            // `0..=17` through ADR 0067's `core.refuse`. `Float.toInt` stood
            // beside it, allocating the message of each of its three
            // refusals, until issue #432 made it `std.float.toInt` over
            // `Inst::FloatTruncate` (ADR 0071).
            // `==` on two erased values stood here, a walk of both operands
            // together that allocated nothing and stopped the run past a
            // depth of 128. ADR 0068's Phase 2 made it `std.dynamic.equals`,
            // a Cove loop over a view of each box with no depth bound at all
            // (issue #480), and deleted the variant.

            // ADR 0059's three keyed intrinsics stood here, and none is left.
            // The duplicate refusal is `std.set.of`'s and `std.map.of`'s own
            // Cove since `core.refuse`, ADR 0067; the order of two erased keys
            // is `std.dynamic.order` since ADR 0068's Phase 3; and the
            // admission, which walked a key and raised one the language
            // refuses in the method's words, is a walk the lowering composes
            // or `std.dynamic.refusesKey` to decide, and `describes<L>` or
            // `std.dynamic.refuseKey` to word, since the ADR's Phase 4c.
        }
    }
}

/// What an [`Intrinsic`] is about.
///
/// One, and not a collection: ADR 0058's Phase 5 makes "a new collection
/// `IntrinsicCall` a verification failure", and this is the half of that rule
/// a verifier can read. An intrinsic whose operand is a collection is refused
/// by `crate::verify`, and there is no longer an exception: the `Array<String>` `Class::Strings` named was `String.join`'s
/// operand, read as the input of a bulk text operation (#378, Q18) rather than
/// as a collection it managed, and issue #454's Step 3 made that join Cove. No
/// operand of any variant left is a collection at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    // `Text` stood here, for what read or built text — Unicode, searching,
    // splitting, case mapping — until issue #432 made its last member,
    // `String.refuseByteRange`, Cove.
    /// An `Int` or a `Float`, and the text one is parsed from or formatted to.
    Scalar,
    // `Value` stood here, a rule over any value directed by its layout, until
    // ADR 0068's Phase 4c deleted `Value.admitKey`, its last member; issue
    // #536 deleted the category nothing produced.
}

/// What an [`Intrinsic`] takes and answers, as [`Intrinsic::signature`]
/// writes it down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The operands every call passes, in order: a method's receiver first.
    ///
    /// Exactly these and no more. There was one variadic intrinsic,
    /// `String.interpolate`, and an interpolation is now assembled by run
    /// instructions and one append per piece (#403), so nothing takes a list.
    pub operands: &'static [Class],
    /// What the answer written into the destination is.
    pub result: Class,
}

/// The layout an operand or an answer of an [`Intrinsic`] has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A `String`: one reference to a string object.
    Str,
    /// The `Result` whose `Ok` carries one of these, and whose `Err` carries
    /// the `Error` the machine builds.
    ResultOf(Carried),
    // `Bool`, `Strings` (an `Array<String>`), `OptionOf`, `Value` (any
    // layout) and `Buffer` (a `ByteBuffer`) stood here, each the class of an
    // intrinsic that has since moved into Cove or an instruction. Issue #536
    // deleted them once no signature named any of them: a class nothing
    // declares is a verifier arm nothing reaches. `Unit` and `Int` went for
    // the same reason with issue #432: `String.refuseByteRange`'s answer of
    // nothing and its two offsets were the last of each. `Float` went with
    // the same issue's next migration: `Float.toInt`'s operand was the last
    // one, and it is `std.float.toInt` now.
}

/// What a [`Class::ResultOf`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carried {
    // `Int` stood here, what `Float.toInt`'s `Ok` carried, until issue #432
    // made that operation `std.float.toInt`.
    Float,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let carried = |carried: &Carried| match carried {
            Carried::Float => "Float",
        };
        match self {
            Class::Str => write!(f, "String"),
            Class::ResultOf(inner) => write!(f, "Result<{}, Error>", carried(inner)),
        }
    }
}

impl fmt::Display for Intrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.receiver(), self.operation())
    }
}

/// What generated code has to be ready for when it calls an [`Intrinsic`].
///
/// [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
/// asks for seven facts because they "decide whether generated code
/// must publish roots, synchronize the program counter, take a safepoint and
/// reload stack or heap pointers. A non-allocating field bound check does
/// not pay the allocation protocol. A grow operation does." Four are left:
/// issue #536 deleted `MAY_BLOCK`, `BULK_WORK` and `WRITES_MEMORY`, which no
/// intrinsic declared — nothing below the standard library reaches the
/// scheduler, walks an operand's length or writes through a handle it was
/// given any more. A `u8` newtype rather than a crate dependency: the flags
/// fit in one byte, and this crate answers to nothing before the IR does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects(u8);

impl Effects {
    /// No effect at all.
    pub const NONE: Effects = Effects(0);

    /// May allocate a new heap object.
    ///
    /// Generated code that calls an intrinsic with this flag set must
    /// publish every root the collector would need to trace if the call
    /// collects — an object built so far but not yet reachable from a slot,
    /// most of all.
    pub const MAY_ALLOCATE: Effects = Effects(1 << 0);

    /// May run the collector.
    ///
    /// Set on exactly the intrinsics [`Effects::MAY_ALLOCATE`] is: this
    /// backend collects at allocation and nowhere else, so the two facts
    /// have one cause. Kept as its own flag because the reason to ask it is
    /// different — generated code that holds a raw heap address across the
    /// call must reload it afterwards, since a collection may have moved or
    /// relabeled what it pointed at, and this is the flag that says whether
    /// the call could have run one.
    pub const MAY_COLLECT: Effects = Effects(1 << 1);

    /// May answer with a `RuntimeError` rather than a value.
    ///
    /// Generated code must synchronize the program counter before a call
    /// that carries this flag, so that an error it raises reports the
    /// source span that was live rather than one left over from whatever
    /// ran before it.
    pub const MAY_RAISE: Effects = Effects(1 << 2);

    /// Reads words out of a heap object rather than only out of the operand
    /// words it was handed.
    ///
    /// Generated code must have a valid heap pointer for any object the
    /// call reads, which after a call that also carries
    /// [`Effects::MAY_COLLECT`] means reloading it rather than reusing one
    /// computed before the call.
    pub const READS_MEMORY: Effects = Effects(1 << 5);

    /// `self` with every flag `other` sets also set.
    pub const fn union(self, other: Effects) -> Effects {
        Effects(self.0 | other.0)
    }

    /// Whether every flag `other` sets is also set in `self`.
    pub const fn contains(self, other: Effects) -> bool {
        self.0 & other.0 == other.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set of intrinsics may lose members and may never gain one.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)
    /// decides that `IntrinsicCall` "is a migration mechanism, and its
    /// population only falls": every variant below is a public method's
    /// algorithm in Rust, [issue #432](https://github.com/myuon/cove/issues/432)
    /// moves each one into Cove or into a primitive that names a machine
    /// instead, and the enum is deleted when the list is empty.
    ///
    /// **It is compared as a set rather than as a count**, for the reason
    /// `crates/cove-cli/tests/vm_coverage.rs` compares its known
    /// disagreements as one: a count cannot tell a variant that left from one
    /// that arrived, so a change that migrates `String.split` and adds
    /// `Text.replace` leaves the number falling and the architecture exactly
    /// where it was. Only a set says which happened.
    ///
    /// Deleting a line is the whole of what a migration owes this test.
    /// Adding one is the design error ADR 0064 exists to make loud: an
    /// operation that wants a runtime arm of its own has to pass Decision 2
    /// first — its only failures are ones the program did not write — and an
    /// operation named after a method never does.
    #[test]
    fn the_intrinsic_set_only_shrinks() {
        // `Float.parse` is migrated too (issue #432, ADR 0072) and the method
        // reaches no intrinsic; its variant is here only until the mechanism
        // is deleted, which deletes this whole test with it.
        const MIGRATED_BUT_STILL_HERE: &[&str] = &["Float.parse"];

        let here: Vec<String> = ALL.iter().map(|one| one.to_string()).collect();
        let allowed: Vec<&str> = MIGRATED_BUT_STILL_HERE.to_vec();

        let added: Vec<&String> = here
            .iter()
            .filter(|one| !allowed.contains(&one.as_str()))
            .collect();
        assert!(
            added.is_empty(),
            "`Intrinsic` gained {added:?}. ADR 0064: the set only shrinks, and a \
             new operation below the standard library names a machine — a typed \
             scalar operation, a run load or store, a bounded run search, \
             compare, slice or copy, a typed run's allocation or finish, or a \
             dynamic value's layout — not a method. If this really is one of \
             those, name it for the capability and give it an instruction; \
             `IntrinsicCall` is not where it goes"
        );

        let left: Vec<&&str> = allowed
            .iter()
            .filter(|one| !here.iter().any(|had| had == *one))
            .collect();
        if !left.is_empty() {
            // The ratchet: what a migration deletes, it deletes here too, so
            // that the list is always what the enum is and the next reader
            // can see how far #432 got by reading one of them.
            panic!(
                "`Intrinsic` no longer has {left:?} — good. Delete those lines \
                 from `MIGRATED_BUT_STILL_HERE` in the same change, so the list \
                 stays the census #432 is measured against"
            );
        }
    }

    /// [`ALL`] names every variant exactly once.
    ///
    /// A hand-written list like [`ALL`] goes wrong in exactly two ways: a
    /// variant left out, or one written down twice. Counting catches the
    /// first without repeating the ninety-line list a second time, and a
    /// duplicate is caught by comparing every pair — `Intrinsic` has no
    /// `Ord`, so a `BTreeSet` is not the cheap way to ask, and fifty-some
    /// items is nowhere near where that would matter.
    #[test]
    fn all_names_every_variant_once() {
        // Exhaustiveness: a variant added to the enum and not to `ALL`
        // fails to compile here rather than passing silently, because this
        // match has to name every one of them.
        fn count(intrinsic: Intrinsic) -> usize {
            match intrinsic {
                Intrinsic::FloatParse => 1,
            }
        }
        let variants: usize = ALL.iter().map(|intrinsic| count(*intrinsic)).sum();
        assert_eq!(variants, ALL.len(), "`count` names every variant once");

        for (at, left) in ALL.iter().enumerate() {
            for right in &ALL[at + 1..] {
                assert_ne!(left, right, "`ALL` names {left} twice");
            }
        }
    }

    /// [`Intrinsic::index`] numbers every variant once, and numbers none of
    /// them past [`COUNT`].
    ///
    /// Both halves are the contract a per-variant table depends on, and neither
    /// implies the other. A table indexed by a value at or past its own width
    /// panics, which is loud; a table two of whose variants share an index adds
    /// their rows together and reports a plausible wrong number, which is not.
    /// The second is the one worth a test.
    #[test]
    fn index_numbers_every_variant_once() {
        let mut seen = [false; COUNT];
        for intrinsic in ALL {
            let at = intrinsic.index();
            assert!(at < COUNT, "`{intrinsic}` is numbered {at} of {COUNT}");
            assert!(!seen[at], "`{intrinsic}` shares index {at} with another");
            seen[at] = true;
        }
        assert!(
            seen.iter().all(|had| *had),
            "`COUNT` is wider than the variants `index` numbers"
        );
    }

    /// Every intrinsic is found again by the pair it prints as.
    #[test]
    fn from_names_inverts_receiver_and_operation() {
        for intrinsic in ALL {
            assert_eq!(
                Intrinsic::from_names(intrinsic.receiver(), intrinsic.operation()),
                Some(*intrinsic),
                "`{intrinsic}` was not found by its own receiver and operation"
            );
        }
    }

    #[test]
    fn from_names_refuses_a_pair_nothing_names() {
        assert_eq!(Intrinsic::from_names("String", "reverse"), None);
        assert_eq!(Intrinsic::from_names("Nothing", "at_all"), None);
    }

    #[test]
    fn display_prints_receiver_dot_operation() {
        assert_eq!(Intrinsic::FloatParse.to_string(), "Float.parse");
    }

    #[test]
    fn effects_union_and_contains_agree() {
        let both = Effects::MAY_ALLOCATE.union(Effects::MAY_COLLECT);
        assert!(both.contains(Effects::MAY_ALLOCATE));
        assert!(both.contains(Effects::MAY_COLLECT));
        assert!(!both.contains(Effects::MAY_RAISE));
        assert!(Effects::NONE.contains(Effects::NONE));
        assert!(!Effects::NONE.contains(Effects::MAY_RAISE));
    }

    /// A collecting intrinsic always allocates, because this backend
    /// collects only at an allocation.
    #[test]
    fn collecting_implies_allocating() {
        for intrinsic in ALL {
            let effects = intrinsic.effects();
            if effects.contains(Effects::MAY_COLLECT) {
                assert!(
                    effects.contains(Effects::MAY_ALLOCATE),
                    "`{intrinsic}` may collect without a flag saying it may allocate"
                );
            }
        }
    }

    /// An intrinsic that allocates may raise, because an allocation can find
    /// the heap exhausted.
    #[test]
    fn allocating_implies_raising() {
        for intrinsic in ALL {
            let effects = intrinsic.effects();
            if effects.contains(Effects::MAY_ALLOCATE) {
                assert!(
                    effects.contains(Effects::MAY_RAISE),
                    "`{intrinsic}` may allocate without a flag saying it may raise"
                );
            }
        }
    }

    /// **Every intrinsic left can be refused**, and that is a statement about
    /// what this enum has become rather than a flag being checked.
    ///
    /// `MAY_RAISE` is language-level failure only (#378, Q5.3), so this used
    /// to be a *list*: the operations no program can be stopped by, which
    /// were the `Float` functions IEEE 754 answers for every input and
    /// nothing else at all. A character count headed it once; a suffix test,
    /// a prefix test, a whole-haystack search and a search that answers a
    /// position sat under it; then `Float.abs`, then `Float.min` and
    /// `Float.max`, then `Float.round`, and with `Float.sqrt` the list is
    /// empty. Each of those left the same way — ADR 0064's Decision 2, a
    /// typed scalar operation that names a machine — and none of them left by
    /// having a flag changed.
    ///
    /// **So the assertion is inverted, and it is not the same assertion
    /// spelled differently.** `assert_eq!(never, vec![])` would be a test
    /// whose name no longer describes it and whose expected value is
    /// satisfied by an `ALL` with nothing in it; what is asserted instead is
    /// the fact — every surviving variant declares `MAY_RAISE` — with the
    /// non-vacuity guard that makes it worth asserting. Deleting it and
    /// moving the reasoning was the other option and is the wrong one: this
    /// is a *ratchet in the other direction* from
    /// [`the_intrinsic_set_only_shrinks`], and it is the thing that would
    /// notice an intrinsic being added back below Decision 2's bar.
    ///
    /// **What it costs, which is real and is recorded here because nothing
    /// else would record it.** `cove_native::IntrinsicProtocol` reads these
    /// effects into two facts, and an intrinsic with neither — no safepoint
    /// and no raise — is a *plain call*: no publish, no program counter, no
    /// outcome test, and the frame pointer kept live across it. That class
    /// had exactly one member left, `Float.sqrt`, and now has none, so the
    /// code generator's plain-call path is reachable only by an intrinsic
    /// nobody has written. `cove-native`'s `INTRINSIC_CLASSES` is down from
    /// three classes to two, and `cove-runtime`'s `native_tier.rs` lost the
    /// case that drove that path from a real program. The path itself stays:
    /// it is what the effects *mean*, and the alternative is deleting a
    /// lowering because the census happens to be empty this week. (Issue #432
    /// emptied the raise-only class the same way, and `INTRINSIC_CLASSES` is
    /// down to one.)
    ///
    /// It is also why `vm::exec`'s `unraisable` has no end-to-end case and
    /// can now have none at all: that panic needs an arm that can answer an
    /// `Err` while its variant declares no `MAY_RAISE`, and there is no such
    /// variant left to build a program out of.
    #[test]
    fn every_intrinsic_left_can_be_refused() {
        assert!(
            !ALL.is_empty(),
            "the assertion below is about the variants there are, so there \
             have to be some; when this enum is finally empty, delete this \
             test with the enum rather than leaving it passing vacuously"
        );
        let never: Vec<Intrinsic> = ALL
            .iter()
            .copied()
            .filter(|intrinsic| !intrinsic.effects().contains(Effects::MAY_RAISE))
            .collect();
        assert!(
            never.is_empty(),
            "{never:?} declare no `MAY_RAISE`. Every intrinsic left allocates \
             or refuses; an operation IEEE 754 or the language answers for \
             every input is a typed scalar instruction (ADR 0064, Decision 2) \
             and not a runtime call. If one really belongs here, say in \
             `IntrinsicProtocol`'s doc that the plain-call class has a member \
             again, and put `native_tier.rs`' case back"
        );
    }

    /// No intrinsic is a collection operation: ADR 0058 moved every one into
    /// run instructions and the standard library, and Phase 5 makes a new one
    /// a verification failure. No receiver is a collection, and no category
    /// is one — [`Category`] has none to be.
    ///
    /// Operands were checked here too. The `ByteBuffer` a rendering appended
    /// to was an allowed one, as text work's output, until ADR 0068's Phase
    /// 4b-ii made the rendering Cove; `String.join`'s `Array<String>`, text
    /// work's *input*, was another until issue #454's Step 3. Issue #536
    /// deleted `Class::Strings`, `Class::Buffer` and `Class::Value` once no
    /// signature named any of them, so an operand that is a collection cannot
    /// be written in a [`Signature`] at all, and `crate::verify` refuses a
    /// collection handed to any intrinsic whatever its signature says.
    #[test]
    fn no_intrinsic_is_a_collection_operation() {
        const COLLECTIONS: &[&str] = &[
            "Array",
            "Vector",
            "Set",
            "Map",
            "ByteBuffer",
            "StringBuilder",
        ];
        for intrinsic in ALL {
            assert!(
                !COLLECTIONS.contains(&intrinsic.receiver()),
                "`{intrinsic}` is an operation of a collection"
            );
        }
    }

    /// Every intrinsic's operand list is short enough to be read without
    /// collecting it.
    #[test]
    fn every_operand_list_is_short() {
        for intrinsic in ALL {
            assert!(intrinsic.signature().operands.len() <= 3, "`{intrinsic}`");
        }
    }
}
