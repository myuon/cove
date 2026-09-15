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
//! `crate::vm::builtins::call` (in `cove-runtime`) has been taught, rather
//! than the `(receiver, operation)` pair of strings [`crate::Builtin`] used
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

/// One core operation a `CallBuiltin` may name.
///
/// A variant is named `ReceiverOperation` in upper camel case — `String`'s
/// `sliceBytes` is [`Intrinsic::StringSliceBytes`] — because that pair is
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
    StringInterpolate,
    StringLength,
    StringWords,
    StringChars,
    StringSplit,
    StringJoin,
    StringSlice,
    StringTrim,
    StringContains,
    StringStartsWith,
    StringEndsWith,
    StringIndexOf,
    StringReplace,
    StringToUpper,
    StringToLower,
    StringFromCodePoint,
    StringCodePointAtByte,
    StringSliceBytes,
    ArrayContains,
    ArrayIndexOf,
    VectorContains,
    VectorIndexOf,
    SetOf,
    SetContains,
    SetToArray,
    SetInserted,
    SetRemoved,
    MapOf,
    MapGet,
    MapContains,
    MapKeys,
    MapValues,
    MapInserted,
    MapRemoved,
    IntToFloat,
    IntParse,
    IntParseRadix,
    FloatToInt,
    FloatRound,
    FloatAbs,
    FloatSqrt,
    FloatMin,
    FloatMax,
    FloatFormat,
    FloatParse,
    DurationNanos,
    AnyEquals,
}

/// Every [`Intrinsic`], in declaration order.
///
/// What [`Intrinsic::from_names`] searches and what this module's own tests
/// walk to check the table has no gap and no duplicate — the two ways a hand-
/// written list like this one goes wrong.
pub const ALL: &[Intrinsic] = &[
    Intrinsic::StringInterpolate,
    Intrinsic::StringLength,
    Intrinsic::StringWords,
    Intrinsic::StringChars,
    Intrinsic::StringSplit,
    Intrinsic::StringJoin,
    Intrinsic::StringSlice,
    Intrinsic::StringTrim,
    Intrinsic::StringContains,
    Intrinsic::StringStartsWith,
    Intrinsic::StringEndsWith,
    Intrinsic::StringIndexOf,
    Intrinsic::StringReplace,
    Intrinsic::StringToUpper,
    Intrinsic::StringToLower,
    Intrinsic::StringFromCodePoint,
    Intrinsic::StringCodePointAtByte,
    Intrinsic::StringSliceBytes,
    Intrinsic::ArrayContains,
    Intrinsic::ArrayIndexOf,
    Intrinsic::VectorContains,
    Intrinsic::VectorIndexOf,
    Intrinsic::SetOf,
    Intrinsic::SetContains,
    Intrinsic::SetToArray,
    Intrinsic::SetInserted,
    Intrinsic::SetRemoved,
    Intrinsic::MapOf,
    Intrinsic::MapGet,
    Intrinsic::MapContains,
    Intrinsic::MapKeys,
    Intrinsic::MapValues,
    Intrinsic::MapInserted,
    Intrinsic::MapRemoved,
    Intrinsic::IntToFloat,
    Intrinsic::IntParse,
    Intrinsic::IntParseRadix,
    Intrinsic::FloatToInt,
    Intrinsic::FloatRound,
    Intrinsic::FloatAbs,
    Intrinsic::FloatSqrt,
    Intrinsic::FloatMin,
    Intrinsic::FloatMax,
    Intrinsic::FloatFormat,
    Intrinsic::FloatParse,
    Intrinsic::DurationNanos,
    Intrinsic::AnyEquals,
];

impl Intrinsic {
    /// The type the operation belongs to: `Array`, `String`, `Map`, `Int`.
    ///
    /// `Any` for [`Intrinsic::AnyEquals`], which is `==` on anything wider
    /// than a word rather than a method a type declares — see the doc
    /// comment where `cove-runtime` dispatches it.
    pub const fn receiver(self) -> &'static str {
        match self {
            Intrinsic::StringInterpolate => "String",
            Intrinsic::StringLength => "String",
            Intrinsic::StringWords => "String",
            Intrinsic::StringChars => "String",
            Intrinsic::StringSplit => "String",
            Intrinsic::StringJoin => "String",
            Intrinsic::StringSlice => "String",
            Intrinsic::StringTrim => "String",
            Intrinsic::StringContains => "String",
            Intrinsic::StringStartsWith => "String",
            Intrinsic::StringEndsWith => "String",
            Intrinsic::StringIndexOf => "String",
            Intrinsic::StringReplace => "String",
            Intrinsic::StringToUpper => "String",
            Intrinsic::StringToLower => "String",
            Intrinsic::StringFromCodePoint => "String",
            Intrinsic::StringCodePointAtByte => "String",
            Intrinsic::StringSliceBytes => "String",
            Intrinsic::ArrayContains => "Array",
            Intrinsic::ArrayIndexOf => "Array",
            Intrinsic::VectorContains => "Vector",
            Intrinsic::VectorIndexOf => "Vector",
            Intrinsic::SetOf => "Set",
            Intrinsic::SetContains => "Set",
            Intrinsic::SetToArray => "Set",
            Intrinsic::SetInserted => "Set",
            Intrinsic::SetRemoved => "Set",
            Intrinsic::MapOf => "Map",
            Intrinsic::MapGet => "Map",
            Intrinsic::MapContains => "Map",
            Intrinsic::MapKeys => "Map",
            Intrinsic::MapValues => "Map",
            Intrinsic::MapInserted => "Map",
            Intrinsic::MapRemoved => "Map",
            Intrinsic::IntToFloat => "Int",
            Intrinsic::IntParse => "Int",
            Intrinsic::IntParseRadix => "Int",
            Intrinsic::FloatToInt => "Float",
            Intrinsic::FloatRound => "Float",
            Intrinsic::FloatAbs => "Float",
            Intrinsic::FloatSqrt => "Float",
            Intrinsic::FloatMin => "Float",
            Intrinsic::FloatMax => "Float",
            Intrinsic::FloatFormat => "Float",
            Intrinsic::FloatParse => "Float",
            Intrinsic::DurationNanos => "Duration",
            Intrinsic::AnyEquals => "Any",
        }
    }

    /// The operation's own name: `split`, `push`, `toFloat`.
    pub const fn operation(self) -> &'static str {
        match self {
            Intrinsic::StringInterpolate => "interpolate",
            Intrinsic::StringLength => "length",
            Intrinsic::StringWords => "words",
            Intrinsic::StringChars => "chars",
            Intrinsic::StringSplit => "split",
            Intrinsic::StringJoin => "join",
            Intrinsic::StringSlice => "slice",
            Intrinsic::StringTrim => "trim",
            Intrinsic::StringContains => "contains",
            Intrinsic::StringStartsWith => "startsWith",
            Intrinsic::StringEndsWith => "endsWith",
            Intrinsic::StringIndexOf => "indexOf",
            Intrinsic::StringReplace => "replace",
            Intrinsic::StringToUpper => "toUpper",
            Intrinsic::StringToLower => "toLower",
            Intrinsic::StringFromCodePoint => "fromCodePoint",
            Intrinsic::StringCodePointAtByte => "codePointAtByte",
            Intrinsic::StringSliceBytes => "sliceBytes",
            Intrinsic::ArrayContains => "contains",
            Intrinsic::ArrayIndexOf => "indexOf",
            Intrinsic::VectorContains => "contains",
            Intrinsic::VectorIndexOf => "indexOf",
            Intrinsic::SetOf => "of",
            Intrinsic::SetContains => "contains",
            Intrinsic::SetToArray => "toArray",
            Intrinsic::SetInserted => "inserted",
            Intrinsic::SetRemoved => "removed",
            Intrinsic::MapOf => "of",
            Intrinsic::MapGet => "get",
            Intrinsic::MapContains => "contains",
            Intrinsic::MapKeys => "keys",
            Intrinsic::MapValues => "values",
            Intrinsic::MapInserted => "inserted",
            Intrinsic::MapRemoved => "removed",
            Intrinsic::IntToFloat => "toFloat",
            Intrinsic::IntParse => "parse",
            Intrinsic::IntParseRadix => "parseRadix",
            Intrinsic::FloatToInt => "toInt",
            Intrinsic::FloatRound => "round",
            Intrinsic::FloatAbs => "abs",
            Intrinsic::FloatSqrt => "sqrt",
            Intrinsic::FloatMin => "min",
            Intrinsic::FloatMax => "max",
            Intrinsic::FloatFormat => "format",
            Intrinsic::FloatParse => "parse",
            Intrinsic::DurationNanos => "nanos",
            Intrinsic::AnyEquals => "equals",
        }
    }

    /// The intrinsic named `receiver.operation`, if there is one.
    ///
    /// A linear search of [`ALL`], which is the whole set this backend has
    /// been taught and small enough that a search of it costs nothing next
    /// to lowering the call it names. Nothing after lowering calls this:
    /// [`crate::Builtin`] carries the variant itself once one is found, for
    /// the reason this ADR exists — dispatch is never again a string
    /// comparison.
    pub fn from_names(receiver: &str, operation: &str) -> Option<Intrinsic> {
        ALL.iter().copied().find(|intrinsic| {
            intrinsic.receiver() == receiver && intrinsic.operation() == operation
        })
    }

    /// The effects generated code has to be ready for when it calls this
    /// intrinsic.
    ///
    /// See [`Effects`]'s fields for what each one asks of generated code.
    /// Assigned by reading the VM arm each intrinsic dispatches to in
    /// `cove-runtime`'s `vm::builtins`, not by a rule applied to every
    /// member of a family — two operations of the same receiver may answer
    /// differently, the way [`Intrinsic::StringContains`] allocates nothing
    /// and [`Intrinsic::StringSlice`] does.
    pub const fn effects(self) -> Effects {
        use Effects as E;
        // Every arm below validates its own operand count and shape before
        // it does anything else — `vm::builtins::operand::method` and
        // `operand::free` both answer `Err` for a call whose arity or
        // operand types are wrong — so every intrinsic carries
        // `MAY_RAISE`, even the ones a checked program can never make fail:
        // the lowering that lets one through would be a compiler bug this
        // machine reports rather than reads past. `MAY_BLOCK` is on none of
        // them: nothing below reaches the scheduler or a Host boundary,
        // which is a fact about the whole family and not one this match
        // has to repeat per arm.
        let raise = E::MAY_RAISE;
        match self {
            // Rendering walks whatever value it was handed, which may be a
            // collection nested arbitrarily deep, and answers a freshly
            // allocated `String`.
            Intrinsic::StringInterpolate => raise
                .union(E::MAY_ALLOCATE)
                .union(E::MAY_COLLECT)
                .union(E::READS_MEMORY)
                .union(E::BULK_WORK),

            // `length()` decodes every byte to count characters; the other
            // readers below it do the same one decode and then walk, split
            // or map the result, so every one of them is proportional to
            // the receiver and allocates the array or string it answers.
            Intrinsic::StringLength => raise.union(E::READS_MEMORY).union(E::BULK_WORK),
            Intrinsic::StringWords
            | Intrinsic::StringChars
            | Intrinsic::StringSplit
            | Intrinsic::StringJoin
            | Intrinsic::StringSlice
            | Intrinsic::StringTrim
            | Intrinsic::StringReplace
            | Intrinsic::StringToUpper
            | Intrinsic::StringToLower => raise
                .union(E::MAY_ALLOCATE)
                .union(E::MAY_COLLECT)
                .union(E::READS_MEMORY)
                .union(E::BULK_WORK),
            // The three predicates and `indexOf` search the receiver
            // without allocating anything.
            Intrinsic::StringContains
            | Intrinsic::StringStartsWith
            | Intrinsic::StringEndsWith
            | Intrinsic::StringIndexOf => raise.union(E::READS_MEMORY).union(E::BULK_WORK),
            // `codePointAtByte` decodes one character at a fixed cost, not
            // proportional to the whole string.
            Intrinsic::StringCodePointAtByte => raise.union(E::READS_MEMORY),
            // `sliceBytes` copies the answer out of the receiver a word at a
            // time, proportional to the slice rather than to the whole
            // string.
            Intrinsic::StringSliceBytes => raise
                .union(E::MAY_ALLOCATE)
                .union(E::MAY_COLLECT)
                .union(E::READS_MEMORY)
                .union(E::BULK_WORK),
            // `fromCodePoint` reads no receiver — its one argument is an
            // `Int` word — and allocates the one-character `String` it
            // answers, or the message an out-of-range code point fails
            // with.
            Intrinsic::StringFromCodePoint => raise.union(E::MAY_ALLOCATE).union(E::MAY_COLLECT),

            // A linear search over the elements.
            Intrinsic::ArrayContains | Intrinsic::ArrayIndexOf => {
                raise.union(E::READS_MEMORY).union(E::BULK_WORK)
            }
            // `slice` and `toVector` are not here: each is `std.array` over a
            // run slice or a run copy now.
            Intrinsic::VectorContains | Intrinsic::VectorIndexOf => {
                raise.union(E::READS_MEMORY).union(E::BULK_WORK)
            }
            // `push`, `set`, `pop`, `remove`, `freeze`, `slice` and `toArray` are
            // not here: each is `std.vector` over run instructions now.

            // A `Set` or a `Map` is immutable, so every update below
            // allocates a new run rather than writing through the receiver
            // — none of this family ever carries `WRITES_MEMORY`. The
            // membership tests and `get` are a binary search, not
            // proportional to the collection; everything else opens or
            // copies a run proportional to it.
            Intrinsic::SetContains | Intrinsic::MapContains | Intrinsic::MapGet => {
                raise.union(E::READS_MEMORY)
            }
            Intrinsic::SetOf
            | Intrinsic::SetToArray
            | Intrinsic::SetInserted
            | Intrinsic::SetRemoved
            | Intrinsic::MapOf
            | Intrinsic::MapKeys
            | Intrinsic::MapValues
            | Intrinsic::MapInserted
            | Intrinsic::MapRemoved => raise
                .union(E::READS_MEMORY)
                .union(E::MAY_ALLOCATE)
                .union(E::MAY_COLLECT)
                .union(E::BULK_WORK),

            // A scalar reader or writer of its own word, with nothing on
            // the heap to read.
            Intrinsic::IntToFloat
            | Intrinsic::FloatRound
            | Intrinsic::FloatAbs
            | Intrinsic::FloatSqrt
            | Intrinsic::FloatMin
            | Intrinsic::FloatMax
            | Intrinsic::DurationNanos => raise,
            // The three parsers read a `String` receiver's bytes and
            // allocate the message an `Err` carries; `format` allocates the
            // `String` it always answers. None of the four is proportional
            // to anything past the one receiver or the one answer, which is
            // short enough that this backend does not charge it as bulk
            // work.
            Intrinsic::IntParse | Intrinsic::IntParseRadix | Intrinsic::FloatParse => raise
                .union(E::READS_MEMORY)
                .union(E::MAY_ALLOCATE)
                .union(E::MAY_COLLECT),
            Intrinsic::FloatToInt | Intrinsic::FloatFormat => {
                raise.union(E::MAY_ALLOCATE).union(E::MAY_COLLECT)
            }

            // `==` on anything wider than a word walks both operands
            // together, as deep as they nest, and allocates nothing: the
            // answer is one `Bool` word.
            Intrinsic::AnyEquals => raise.union(E::READS_MEMORY).union(E::BULK_WORK),
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
    /// is the boundary [`crate::Builtin`]'s own doc comment draws. The flag
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
                Intrinsic::StringInterpolate
                | Intrinsic::StringLength
                | Intrinsic::StringWords
                | Intrinsic::StringChars
                | Intrinsic::StringSplit
                | Intrinsic::StringJoin
                | Intrinsic::StringSlice
                | Intrinsic::StringTrim
                | Intrinsic::StringContains
                | Intrinsic::StringStartsWith
                | Intrinsic::StringEndsWith
                | Intrinsic::StringIndexOf
                | Intrinsic::StringReplace
                | Intrinsic::StringToUpper
                | Intrinsic::StringToLower
                | Intrinsic::StringFromCodePoint
                | Intrinsic::StringCodePointAtByte
                | Intrinsic::StringSliceBytes
                | Intrinsic::ArrayContains
                | Intrinsic::ArrayIndexOf
                | Intrinsic::VectorContains
                | Intrinsic::VectorIndexOf
                | Intrinsic::SetOf
                | Intrinsic::SetContains
                | Intrinsic::SetToArray
                | Intrinsic::SetInserted
                | Intrinsic::SetRemoved
                | Intrinsic::MapOf
                | Intrinsic::MapGet
                | Intrinsic::MapContains
                | Intrinsic::MapKeys
                | Intrinsic::MapValues
                | Intrinsic::MapInserted
                | Intrinsic::MapRemoved
                | Intrinsic::IntToFloat
                | Intrinsic::IntParse
                | Intrinsic::IntParseRadix
                | Intrinsic::FloatToInt
                | Intrinsic::FloatRound
                | Intrinsic::FloatAbs
                | Intrinsic::FloatSqrt
                | Intrinsic::FloatMin
                | Intrinsic::FloatMax
                | Intrinsic::FloatFormat
                | Intrinsic::FloatParse
                | Intrinsic::DurationNanos
                | Intrinsic::AnyEquals => 1,
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
        assert_eq!(Intrinsic::VectorContains.to_string(), "Vector.contains");
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

    /// Every intrinsic validates its own operand shape before it does
    /// anything else, so every one of them may raise — see
    /// [`Intrinsic::effects`]'s doc comment for why that is not a flag this
    /// backend can narrow per arm.
    #[test]
    fn every_intrinsic_may_raise() {
        for intrinsic in ALL {
            assert!(
                intrinsic.effects().contains(Effects::MAY_RAISE),
                "`{intrinsic}` does not carry `MAY_RAISE`"
            );
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
