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
/// A variant is named `ReceiverOperation` in upper camel case — `String`'s
/// `toUpper` is [`Intrinsic::StringToUpper`] — because that pair is
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
    ValueRenderInto,
    StringSplit,
    StringReplace,
    StringToUpper,
    StringToLower,
    StringRefuseByteRange,
    IntParseRadix,
    FloatToInt,
    FloatFormat,
    FloatParse,
    AnyEquals,
    ValueOrder,
    ValueAdmitKey,
    ValueRefuseDuplicate,
}

/// Every [`Intrinsic`], in declaration order.
///
/// What [`Intrinsic::from_names`] searches and what this module's own tests
/// walk to check the table has no gap and no duplicate — the two ways a hand-
/// written list like this one goes wrong.
pub const ALL: &[Intrinsic] = &[
    Intrinsic::ValueRenderInto,
    Intrinsic::StringSplit,
    Intrinsic::StringReplace,
    Intrinsic::StringToUpper,
    Intrinsic::StringToLower,
    Intrinsic::StringRefuseByteRange,
    Intrinsic::IntParseRadix,
    Intrinsic::FloatToInt,
    Intrinsic::FloatFormat,
    Intrinsic::FloatParse,
    Intrinsic::AnyEquals,
    Intrinsic::ValueOrder,
    Intrinsic::ValueAdmitKey,
    Intrinsic::ValueRefuseDuplicate,
];

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
    /// `Any` for [`Intrinsic::AnyEquals`], which is `==` on anything wider
    /// than a word rather than a method a type declares — see the doc
    /// comment where `cove-runtime` dispatches it. `Value` for the three a
    /// keyed collection's standard-library body reaches through `core.order`,
    /// `core.admitKey` and `core.refuseDuplicate`, which are rules over any
    /// key's layout rather than methods of a type either, and for
    /// [`Intrinsic::ValueRenderInto`], which is what `"{x}"` appends for a
    /// piece of any layout.
    pub const fn receiver(self) -> &'static str {
        match self {
            Intrinsic::ValueRenderInto => "Value",
            Intrinsic::StringSplit => "String",
            Intrinsic::StringReplace => "String",
            Intrinsic::StringToUpper => "String",
            Intrinsic::StringToLower => "String",
            Intrinsic::StringRefuseByteRange => "String",
            Intrinsic::IntParseRadix => "Int",
            Intrinsic::FloatToInt => "Float",
            Intrinsic::FloatFormat => "Float",
            Intrinsic::FloatParse => "Float",
            Intrinsic::AnyEquals => "Any",
            Intrinsic::ValueOrder => "Value",
            Intrinsic::ValueAdmitKey => "Value",
            Intrinsic::ValueRefuseDuplicate => "Value",
        }
    }

    /// The operation's own name: `split`, `join`, `parse`.
    pub const fn operation(self) -> &'static str {
        match self {
            Intrinsic::ValueRenderInto => "renderInto",
            Intrinsic::StringSplit => "split",
            Intrinsic::StringReplace => "replace",
            Intrinsic::StringToUpper => "toUpper",
            Intrinsic::StringToLower => "toLower",
            Intrinsic::StringRefuseByteRange => "refuseByteRange",
            Intrinsic::IntParseRadix => "parseRadix",
            Intrinsic::FloatToInt => "toInt",
            Intrinsic::FloatFormat => "format",
            Intrinsic::FloatParse => "parse",
            Intrinsic::AnyEquals => "equals",
            Intrinsic::ValueOrder => "order",
            Intrinsic::ValueAdmitKey => "admitKey",
            Intrinsic::ValueRefuseDuplicate => "refuseDuplicate",
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
            Intrinsic::StringSplit
            | Intrinsic::StringReplace
            | Intrinsic::StringToUpper
            | Intrinsic::StringToLower
            | Intrinsic::StringRefuseByteRange => Category::Text,
            Intrinsic::IntParseRadix
            | Intrinsic::FloatToInt
            | Intrinsic::FloatFormat
            | Intrinsic::FloatParse => Category::Scalar,
            // Rendering is a walk directed by whatever layout the piece has,
            // which is what makes it a value rule rather than a text one: a
            // `"{items}"` renders an `Array` through it.
            Intrinsic::ValueRenderInto
            | Intrinsic::AnyEquals
            | Intrinsic::ValueOrder
            | Intrinsic::ValueAdmitKey
            | Intrinsic::ValueRefuseDuplicate => Category::Value,
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
            // The piece first, then the buffer it is appended to: the value
            // is the receiver, as it is of every other operation here.
            Intrinsic::ValueRenderInto => fixed(&[C::Value, C::Buffer], C::Unit),
            Intrinsic::StringSplit => fixed(&[C::Str, C::Str], C::Strings),
            Intrinsic::StringToUpper | Intrinsic::StringToLower => fixed(&[C::Str], C::Str),
            Intrinsic::StringReplace => fixed(&[C::Str, C::Str, C::Str], C::Str),
            // The text and the two offsets a refusal is worded with, in the
            // order `String.sliceBytes` names them.
            Intrinsic::StringRefuseByteRange => fixed(&[C::Str, C::Int, C::Int], C::Unit),
            Intrinsic::IntParseRadix => fixed(&[C::Str, C::Int], C::ResultOf(K::Int)),
            Intrinsic::FloatToInt => fixed(&[C::Float], C::ResultOf(K::Int)),
            Intrinsic::FloatFormat => fixed(&[C::Float, C::Int], C::Str),
            Intrinsic::FloatParse => fixed(&[C::Str], C::ResultOf(K::Float)),
            Intrinsic::AnyEquals => fixed(&[C::Value, C::Value], C::Bool),
            Intrinsic::ValueOrder => fixed(&[C::Value, C::Value], C::Int),
            // The key, then the method and the role a refusal is worded with.
            Intrinsic::ValueAdmitKey | Intrinsic::ValueRefuseDuplicate => {
                fixed(&[C::Value, C::Str, C::Str], C::Unit)
            }
        }
    }

    /// The effects generated code has to be ready for when it calls this
    /// intrinsic.
    ///
    /// See [`Effects`]'s fields for what each one asks of generated code.
    /// Assigned by reading the VM arm each intrinsic dispatches to in
    /// `cove-runtime`'s `vm::intrinsics`, not by a rule applied to every
    /// member of a family — two operations of the same receiver may answer
    /// differently, the way [`Intrinsic::StringRefuseByteRange`] allocates
    /// nothing and [`Intrinsic::StringToUpper`] does.
    pub const fn effects(self) -> Effects {
        use Effects as E;
        // `MAY_RAISE` is language-level failure only (#378, Q5.3). An arm no
        // longer re-checks its operand count or types — the verifier refused
        // any call that disagrees with [`Intrinsic::signature`] — so the
        // `Err` those checks answered is not a path any verified program has,
        // and the flag says what a program can actually be stopped by: a
        // refusal the language defines (an empty separator, a radix outside
        // `2..=36`, a key it does not admit), a value nested past what a walk
        // of it may reach, and an exhausted heap — which is why every
        // intrinsic that allocates carries it.
        //
        // `MAY_BLOCK` is on none of them: nothing below reaches the scheduler
        // or a Host boundary, which is a fact about the whole family and not
        // one this match has to repeat per arm.
        let raise = E::MAY_RAISE;
        let allocate = E::MAY_ALLOCATE.union(E::MAY_COLLECT).union(raise);
        match self {
            // Rendering walks whatever value it was handed, which may be a
            // collection nested arbitrarily deep — past the depth a rendering
            // may reach, which stops the run — and appends the text to a byte
            // buffer the caller holds: a write through a handle, and a growth
            // of the buffer's store when the text does not fit.
            Intrinsic::ValueRenderInto => allocate
                .union(E::READS_MEMORY)
                .union(E::WRITES_MEMORY)
                .union(E::BULK_WORK),

            // The readers do one decode and then walk, split or map the
            // result, so every one of them is proportional to the receiver
            // and allocates the array or string it answers. `split` and
            // `replace` also refuse an empty needle.
            Intrinsic::StringSplit
            | Intrinsic::StringReplace
            | Intrinsic::StringToUpper
            | Intrinsic::StringToLower => allocate.union(E::READS_MEMORY).union(E::BULK_WORK),
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
            // allocate, which is `Float.toInt`'s and `Float.format`'s shape
            // and not any other `String` operation's. Every `String` arm left
            // below reads a receiver.
            // The byte-range refusal always raises and allocates nothing: it
            // reads the receiver's bytes to say which end is inside a
            // character, and the message is the machine's rather than an
            // object on the heap. It reads at most two bytes, so it is not
            // bulk work.
            Intrinsic::StringRefuseByteRange => raise.union(E::READS_MEMORY),

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
            // The two parsers left read a `String` receiver's bytes and
            // allocate the message an `Err` carries, and `parseRadix` refuses
            // a radix outside `2..=36`; `format` allocates the `String` it
            // always answers and refuses a digit count past 17. None of the
            // four is proportional to anything past the one receiver or the
            // one answer, which is short enough that this backend does not
            // charge it as bulk work.
            //
            // There were three parsers until issue #454's Step 4, and
            // `Int.parse` is the one that left: it is `std.int.parse` now, a
            // `core.byteLength` and one `byteAt` a byte with an accumulator
            // that runs negative so that neither end of `Int` needs a
            // magnitude `Int` has not got. `parseRadix` stays for the reason
            // this comment's other half names — it *refuses* — and `parse`
            // could go because every failure it has is an `Err` value it
            // builds. That is the line issue #461 draws through six of the
            // arms below, and the only reason the two are now apart.
            Intrinsic::IntParseRadix | Intrinsic::FloatParse => allocate.union(E::READS_MEMORY),
            Intrinsic::FloatToInt | Intrinsic::FloatFormat => allocate,

            // `==` on anything wider than a word walks both operands
            // together, as deep as they nest — past the depth a walk may
            // reach, which stops the run — and allocates nothing: the answer
            // is one `Bool` word.
            Intrinsic::AnyEquals => raise.union(E::READS_MEMORY).union(E::BULK_WORK),

            // ADR 0059's keyed intrinsics. The order and the admission each
            // walk a key as deep as it nests and allocate nothing: the order
            // answers one `Int` word, the admission nothing at all, and both
            // raise — a key too deep to walk, and for the admission a key the
            // language refuses, in the method's words. The duplicate refusal
            // always raises; it renders the key it names, which reads it, and
            // the message is the machine's rather than an object on the heap.
            Intrinsic::ValueOrder | Intrinsic::ValueAdmitKey => {
                raise.union(E::READS_MEMORY).union(E::BULK_WORK)
            }
            Intrinsic::ValueRefuseDuplicate => raise.union(E::READS_MEMORY),
        }
    }
}

/// What an [`Intrinsic`] is about.
///
/// Three, and not one of them a collection: ADR 0058's Phase 5 makes "a new
/// collection `IntrinsicCall` a verification failure", and this is the half of
/// that rule a verifier can read. A `Text` or `Scalar` intrinsic whose operand
/// is a collection is refused by `crate::verify`, and there is no longer an
/// exception: the `Array<String>` [`Class::Strings`] names was `String.join`'s
/// operand, read as the input of a bulk text operation (#378, Q18) rather than
/// as a collection it managed, and issue #454's Step 3 made that join Cove. No
/// operand of any variant left is a collection at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Reads or builds text: Unicode, searching, splitting, case mapping.
    Text,
    /// An `Int` or a `Float`, and the text one is parsed from or formatted to.
    Scalar,
    /// A rule over any value, directed by its layout: equality, key order and
    /// admission, and rendering.
    Value,
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
    /// `()`, the one word a refusal that returns answers.
    Unit,
    Bool,
    Int,
    Float,
    /// A `String`: one reference to a string object.
    Str,
    /// An `Array<String>`: what `words`, `chars` and `split` answer and what
    /// `join` reads.
    Strings,
    /// The `Option` whose `Some` carries one of these.
    OptionOf(Carried),
    /// The `Result` whose `Ok` carries one of these, and whose `Err` carries
    /// the `Error` the machine builds.
    ResultOf(Carried),
    /// A value of any layout, read as the layout says.
    Value,
    /// A `ByteBuffer`: ADR 0052's growable byte run, which a rendering appends
    /// its text to. The one collection an operand may be besides
    /// [`Class::Strings`], and for the same reason: it is where text work
    /// writes, not a collection the intrinsic manages.
    Buffer,
}

/// What an [`Class::OptionOf`] or a [`Class::ResultOf`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carried {
    Int,
    Float,
    Str,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let carried = |carried: &Carried| match carried {
            Carried::Int => "Int",
            Carried::Float => "Float",
            Carried::Str => "String",
        };
        match self {
            Class::Unit => write!(f, "Unit"),
            Class::Bool => write!(f, "Bool"),
            Class::Int => write!(f, "Int"),
            Class::Float => write!(f, "Float"),
            Class::Str => write!(f, "String"),
            Class::Strings => write!(f, "Array<String>"),
            Class::OptionOf(inner) => write!(f, "Option<{}>", carried(inner)),
            Class::ResultOf(inner) => write!(f, "Result<{}, Error>", carried(inner)),
            Class::Value => write!(f, "a value"),
            Class::Buffer => write!(f, "ByteBuffer"),
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
/// asks for these seven facts because they "decide whether generated code
/// must publish roots, synchronize the program counter, take a safepoint and
/// reload stack or heap pointers. A non-allocating field bound check does
/// not pay the allocation protocol. A grow operation does." A `u8` newtype
/// rather than a crate dependency: seven flags fit in one byte, and this
/// crate answers to nothing before the IR does.
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

    /// May park the running task at a scheduler or Host boundary.
    ///
    /// No [`Intrinsic`] carries this today — a core intrinsic is Rust that
    /// stays off the scheduler and the Host boundary by construction, which
    /// is the boundary [`crate::IntrinsicSite`]'s own doc comment draws. The flag
    /// exists because the ADR's effect list names it as one of the seven a
    /// future intrinsic could need, and generated code that saw it would
    /// have to take a safepoint before the call so the scheduler can move
    /// other work while this task is parked.
    pub const MAY_BLOCK: Effects = Effects(1 << 3);

    /// Does work proportional to an operand's length rather than bounded
    /// work: a loop over elements or bytes, a copy, a shift.
    ///
    /// Generated code that calls an intrinsic with this flag set must poll
    /// for cancellation in bounded chunks under [ADR
    /// 0040](../../../docs/adr/0040-long-operations-are-preemptible.md)
    /// rather than run the call to completion as one uninterruptible step.
    pub const BULK_WORK: Effects = Effects(1 << 4);

    /// Reads words out of a heap object rather than only out of the operand
    /// words it was handed.
    ///
    /// Generated code must have a valid heap pointer for any object the
    /// call reads, which after a call that also carries
    /// [`Effects::MAY_COLLECT`] means reloading it rather than reusing one
    /// computed before the call.
    pub const READS_MEMORY: Effects = Effects(1 << 5);

    /// Mutates a heap object the caller already held a handle to, rather
    /// than only writing into an object the call itself just allocated.
    ///
    /// Set on `Vector`'s in-place methods and on no `Set` or `Map`
    /// operation, because the latter are immutable and every update answers
    /// a new object instead of writing through the receiver. Generated code
    /// must treat every other alias of the mutated object as observing the
    /// write — there is no private copy to reason about instead.
    pub const WRITES_MEMORY: Effects = Effects(1 << 6);

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
        const MIGRATED_BUT_STILL_HERE: &[&str] = &[
            "Value.renderInto",
            "String.split",
            "String.replace",
            "String.toUpper",
            "String.toLower",
            "String.refuseByteRange",
            "Int.parseRadix",
            "Float.toInt",
            "Float.format",
            "Float.parse",
            "Any.equals",
            "Value.order",
            "Value.admitKey",
            "Value.refuseDuplicate",
        ];

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
                Intrinsic::ValueRenderInto
                | Intrinsic::StringSplit
                | Intrinsic::StringReplace
                | Intrinsic::StringToUpper
                | Intrinsic::StringToLower
                | Intrinsic::StringRefuseByteRange
                | Intrinsic::IntParseRadix
                | Intrinsic::FloatToInt
                | Intrinsic::FloatFormat
                | Intrinsic::FloatParse
                | Intrinsic::AnyEquals
                | Intrinsic::ValueOrder
                | Intrinsic::ValueAdmitKey
                | Intrinsic::ValueRefuseDuplicate => 1,
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
        assert_eq!(Intrinsic::StringSplit.to_string(), "String.split");
        assert_eq!(Intrinsic::AnyEquals.to_string(), "Any.equals");
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
    /// lowering because the census happens to be empty this week.
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
    /// a verification failure. No receiver is a collection, no category is
    /// one — [`Category`] has none to be — and the one collection an operand
    /// may still be is the `ByteBuffer` a rendering appends to, which is text
    /// work's output. It was two until issue #454's Step 3: `String.join` took
    /// an `Array<String>`, text work's *input*, and this test carried the
    /// exception that said so. `std.string.join` walks that array with a
    /// `for` loop now, so the exception is gone and the assertion below is
    /// unconditional.
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
            let signature = intrinsic.signature();
            for class in signature.operands {
                assert!(
                    *class != Class::Strings,
                    "`{intrinsic}` takes an `Array<String>`, and nothing here may: \
                     `String.join` was the one that did, and it is Cove now"
                );
                assert!(
                    *class != Class::Buffer || intrinsic.operation() == "renderInto",
                    "`{intrinsic}` takes a `ByteBuffer`, which only a rendering may"
                );
                assert!(
                    *class != Class::Value || intrinsic.category() == Category::Value,
                    "`{intrinsic}` is a {:?} intrinsic taking any value, which a collection is",
                    intrinsic.category()
                );
            }
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

    /// No intrinsic reaches the scheduler or a Host boundary.
    #[test]
    fn nothing_blocks() {
        for intrinsic in ALL {
            assert!(
                !intrinsic.effects().contains(Effects::MAY_BLOCK),
                "`{intrinsic}` carries `MAY_BLOCK`, which no core intrinsic should"
            );
        }
    }
}
