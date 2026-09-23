//! The instructions.
//!
//! Every instruction names its operands and its destination by **slot
//! number**. There is no operand stack: no push, no pop, no stack-effect
//! table, no discipline to get wrong.
//!
//! That is [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md)'s
//! *"parameters, locals, temporaries and captures share the one slot
//! numbering"* taken literally. If a temporary is a slot, then an
//! instruction that consumes a temporary names a slot, and the thing an
//! operand stack exists to provide is already there.
//!
//! Two things fall out of that, and they are why it is worth choosing:
//!
//! - **A frame's roots are a static fact.** A stack machine's set of live
//!   references changes as operands are pushed and popped, so its reference
//!   map has to be indexed by program counter. Here the map does not change
//!   between a function's first instruction and its last, and
//!   [`crate::RefMap`] is one bit per slot.
//! - **A call needs no argument buffer.** The callee's frame begins where
//!   the caller's ends, so [`Inst::Call`] copies the words of argument *i*
//!   into the run parameter *i* occupies and transfers control. Nothing is
//!   pushed, permuted, or copied back.
//!
//! # The instruction set describes families, not cases
//!
//! There is one `LoadField`, not one per value kind that has fields; one
//! `Arith`, not one per numeric type; one `Alloc`, not one per collection.
//! A field of an *inline* value needs no instruction at all — it is a slot
//! offset the lowering computes.
//! What an object *is* is a question the object answers at run time, from
//! its own header. Nothing here grows a case because a corpus program was
//! refused, because nothing here refuses anything.
//!
//! The two instructions that carry an immediate — [`Inst::ArithImm`] and
//! [`Inst::CmpImm`] — are the same rule applied to an operand rather than to
//! a type. They are not `add.int.imm`, `sub.int.imm`, `lt.int.imm` and eight
//! more: the operator is a field, as it already is on [`Inst::Arith`] and
//! [`Inst::Cmp`], so the family stays two however many operators the language
//! grows. What they say that no other instruction can is that an operand is a
//! constant, which is a fact the source stated and every other representation
//! of it throws away.

use crate::layout::LayoutId;
use crate::{ArgsId, CaseId, FunctionId, HostOpId, SiteId, StrId, TableId};

/// A slot in the current frame: `memory[frame_base + slot]`.
pub type Slot = u32;

/// An index into a function's instructions.
pub type Pc = u32;

/// Which numeric interpretation an arithmetic or comparison instruction
/// gives its operand words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Num {
    /// Two's-complement `i64`. Also what a `Duration` is arithmetic on:
    /// nanoseconds add like integers, and only the boundary cares that the
    /// answer is called a `Duration`.
    Int,
    /// An IEEE-754 double, bit-cast out of the word.
    Float,
}

/// What a comparison compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    Int,
    Float,
    Bool,
    /// The bytes of two [`crate::Shape::Str`] objects.
    Str,
    /// Two words, as words.
    ///
    /// This is `is`: the identity comparison the language reserves for
    /// shared storage, and it is the one comparison that is allowed to look
    /// at a reference as bits, because that is what it is asking about.
    Identity,
    /// Two case indices, as the integers they are.
    ///
    /// [`Inst::Tag`] says a tag is refused by arithmetic, ordering and
    /// integer comparison, because none of those accepts that `Repr` — and
    /// that refusal is what keeps a case index from being confused with a
    /// number. This does not weaken it: it accepts a `Tag` and nothing else,
    /// so the pairing a tag can take part in is still only with another tag.
    ///
    /// What it is for is `Kind.Space == Kind.Word`, which is `1 == 2`. An
    /// enum with no payload is one word wide and that word is the
    /// discriminant, so two of them are equal exactly when the two words are.
    /// Walked instead by `Any.equals`, the same question measured 342 ms
    /// against 210 for 2,000,000 comparisons — 66 ns of builtin call apiece,
    /// and 8% of a native profile of a formatter that reads a token's kind in
    /// every loop it has.
    Tag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// The three-way comparison: an `Int`, `-1` when `a` sorts before `b`,
    /// `0` when they are equal and `1` when it sorts after.
    ///
    /// [ADR 0059](../../../docs/adr/0059-a-keyed-collection-is-searched-by-order-not-hashed.md)
    /// moves a `Map`'s and a `Set`'s binary search into the standard library
    /// over a value order, and a search step asks one question with three
    /// answers. Two ordered comparisons would ask it twice — two dispatches
    /// and, for a `String`, two walks of the same bytes — where this asks it
    /// once (#378, Q4.4).
    ///
    /// It is only an [`Inst::Cmp`]. The answer is not a `Bool`, so nothing
    /// branches on it directly: [`Inst::CmpBranch`], [`Inst::CmpImm`] and
    /// [`Inst::CmpImmBranch`] refuse it, and the bytecode gives it a family
    /// of its own rather than a member of the cross products those mirror.
    /// And it is only an order the language keeps for a key: over
    /// [`Compare::Int`] (which a `Duration` reads as), [`Compare::Bool`]
    /// (`false` first), [`Compare::Str`] (by bytes) and [`Compare::Tag`] (by
    /// case index, which a lowering asks for only where the index order is
    /// the case-name order a key sorts by). A `Float` has no total order and
    /// an identity has none a program may see, so the verifier refuses both.
    Order,
}

/// A conversion between two scalar representations.
///
/// The two `Duration` members are relabels: a `Duration` is signed
/// nanoseconds in one word, so reading its count out and building one from a
/// count move the word unchanged and only the slot's `Repr` differs. They are
/// instructions rather than [`Inst::Copy`]s because a copy is between two
/// locations of one layout, and they are here rather than intrinsics because
/// ADR 0058's Phase 5 (#378, P5-2) keeps a runtime call for work, not for a
/// word that does not change.
///
/// **Every member here is total, and that is now a property of the family
/// rather than an accident of it.** There was a `FloatToInt` until issue
/// #454's Step 2, and it was the one conversion with inputs it had no answer
/// for: a NaN, an infinity, and every magnitude at or past `2^63`. Its arm in
/// the VM was Rust's `x as i64`, which answers `0` for a NaN and clamps at
/// each end rather than refusing, where `Float.toInt` answers
/// `Result<Int, Error>` and names which of the three stopped it. ADR 0064's
/// Decision 6 refuses that pair in as many words — "its bare `as` cast
/// disagrees with the intrinsic's checked `Result`. Phase 3 either gives it
/// the checked semantics or deletes it; leaving a second, wrong answer in the
/// IR is not an option" — and this is the second of those two. No lowering
/// ever emitted it, so nothing written in Cove could reach either answer.
///
/// Giving it the checked semantics instead was the other branch and is not
/// available to a one-word conversion: a checked conversion has **two**
/// answers, the value and which of the three refusals applies, and this
/// family's whole shape is one word in and one word out. The instruction that
/// eventually carries `Float.toInt` will not be a member here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Convert {
    /// `Int` to `Float`, as `as`-style widening: `Int.toFloat()`.
    ///
    /// Total, and lossy only by rounding: every `i64` has a nearest `f64` and
    /// `i64 as f64` is that one. It is the only `as` cast left in this family
    /// and it is not the kind Decision 6 refused — it has no input it
    /// declines and no answer a caller has to check.
    IntToFloat,
    /// A `Duration`'s count of nanoseconds, as an `Int`: `d.nanos()`.
    DurationToInt,
    /// A count of nanoseconds, as a `Duration`: `Duration.nanos(n)`.
    IntToDuration,
}

/// Which end of the pair an [`Inst::FloatMinMax`] answers.
///
/// A flag rather than two instructions, which is the opposite of what
/// [`Inst::FloatAbs`] decided one migration earlier, and the difference is
/// that these two exist. `abs` declined a `FloatUnary { op }` because it would
/// have had one member and two guesses in it; `min` and `max` arrive together,
/// share a nine-instruction sequence in the native lowering, and differ in a
/// single opcode byte and a single condition code. A flag writes that sequence
/// once per tier; two
/// instructions write it twice per tier and give a reader two places to keep
/// in agreement. The bytecode is unaffected either way — [`Op::FloatMinMax`]
/// is two opcodes exactly as two instructions would be, the way
/// [`Op::Convert`] is three.
///
/// [`Op::FloatMinMax`]: crate::bytecode::Op::FloatMinMax
/// [`Op::Convert`]: crate::bytecode::Op::Convert
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinMax {
    /// `Float.min`.
    Min,
    /// `Float.max`.
    Max,
}

/// How many elements an [`Inst::Alloc`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Len {
    /// A shape whose size the layout already fixes: a struct, an enum, a
    /// closure, a box.
    Fixed,
    /// A count the lowering knew: a literal array's element count, a string
    /// literal's byte count.
    Count(u32),
    /// A count in a slot, as an `Int`.
    Slot(Slot),
}

/// What the units of a run are: the storage descriptor of
/// [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
/// `FixedRun(storage, length)`.
///
/// A run is a heap object whose header length counts *units*, and this says
/// what one unit is. It is a static fact of the instruction that names it,
/// never a tag a backend reads at run time: ADR 0058 requires that
/// "`PackedBytes` and `Words(LayoutId)` are statically distinguished", so an
/// encoding may split an operation by storage — [`Inst::RunCopy`] is two
/// opcodes — without the IR growing a second instruction for it.
///
/// Offsets, counts and bounds are always in units. What turns a unit into the
/// memory it occupies is this descriptor and nothing else, so a caller that
/// holds a logical length never multiplies by a stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// Bytes, eight to a word, least-significant first — a `String`'s payload
    /// and a [`crate::Shape::Bytes`] run's. One unit is one byte, and a run of
    /// them holds no references.
    PackedBytes,
    /// Whole elements of this layout, laid end to end. One unit is one
    /// element, and its **stride** is the layout's width in words — so an
    /// `Array<Point>` is a run of two-word units, not a run of words.
    ///
    /// The layout is the *element's* layout, not the object's: the object is a
    /// [`crate::Shape::Elements`] of it, and its reference map is what the
    /// collector traces every unit by. That is why a copy between two runs is
    /// held to one element layout — a unit written into a run of another
    /// family would be traced by the wrong map.
    Words(LayoutId),
}

/// What a [`Inst::RunFinish`] checks about a live prefix before it hands it
/// over: [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
/// `validation = None | Utf8`.
///
/// It is a static fact of the instruction, for [`Storage`]'s reason: whether a
/// finish walks its bytes is not something a backend should learn by reading
/// a tag. And it is a separate fact from the storage, because the ADR lets the
/// optimizer eliminate a validation it can prove — every byte from a valid
/// `String` range at UTF-8 boundaries — without the run becoming any other
/// kind of run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validation {
    /// Nothing is checked: `Vector.freeze()`'s finish, where an element store
    /// already is the array it becomes.
    None,
    /// The live prefix must be valid UTF-8: a byte builder's finish, whose
    /// answer is a `String` and whose bytes were never checked on the way in.
    Utf8,
}

/// One instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    // ---- constants and moves ------------------------------------------
    /// `dst = ()`
    Unit { dst: Slot },
    /// `dst = value`
    Bool { dst: Slot, value: bool },
    /// `dst = value`, also how a `Duration` literal reaches a slot.
    Int { dst: Slot, value: i64 },
    /// `dst = <callee's dense id>`, as a word.
    ///
    /// What a lowered closure's environment is given for its callee field,
    /// and the only place that value is produced. It is not [`Inst::Int`],
    /// though the word it writes is the same [`FunctionId`] an `Int` of that
    /// value would be: a closure's [`crate::layout::Shape::Closure`] already
    /// carries `function: FunctionId` as a typed fact, and writing the same
    /// id again through the untyped integer path made a second,
    /// uninspectable copy of it — one no verifier could tell from an
    /// ordinary integer, and one that renumbered every golden lowering with
    /// a closure in it whenever an unrelated function was added or moved,
    /// because the two facts were spelled a plain number in the listing
    /// rather than the name that number happened to hold that day.
    ///
    /// [`mod@crate::verify`] bounds `callee` against
    /// [`crate::program::Program::functions`] the way [`Inst::Call`]'s is
    /// bounded, and, where the destination is then stored into a
    /// statically-known closure object, checks that `callee` agrees with
    /// what the object's own layout says — the comparison the two copies
    /// never had. [`crate::print`] renders it symbolically, by the callee's
    /// name and not its number, which is what stops the churn: an unrelated
    /// declaration changing `callee`'s numeric value no longer changes a
    /// single character of the listing.
    FuncRef { dst: Slot, callee: FunctionId },
    /// `dst = <the index of `case` in `layout`>`, as an enum's discriminant.
    ///
    /// The one way a discriminant is written. It is not [`Inst::Int`], though
    /// the word it writes is the number an `Int` of that value would write,
    /// and the reason is [`Inst::FuncRef`]'s: a case index reached its slot
    /// through the untyped integer path, where no verifier could tell it from
    /// an ordinary number and nothing bounded it against the enum it was
    /// supposed to name.
    ///
    /// Its destination is a [`Repr::Tag`](crate::Repr::Tag) word, which is
    /// what makes the two facts separable at all: a tag is one non-reference
    /// word, physically an integer, and is refused by arithmetic, ordering
    /// and integer comparison because none of those accepts that `Repr`.
    /// [`Inst::Switch`] accepts it, and so do copying and clearing, which
    /// read a layout rather than a `Repr`.
    ///
    /// [`mod@crate::verify`] bounds `case` against `layout`'s own case list
    /// and refuses a `layout` that is not an enum. [`crate::print`] renders
    /// it by the case's name, so an unrelated case added before it changes no
    /// character of a listing that does not mention it.
    Tag {
        dst: Slot,
        layout: LayoutId,
        case: CaseId,
    },
    /// `dst = f64::from_bits(bits)`
    ///
    /// The bits rather than the `f64` so that [`Inst`] can be `Eq` and
    /// `Hash`ed, and so that a NaN in the source survives the IR unchanged.
    Float { dst: Slot, bits: u64 },
    /// `dst = <the address of the string object for `text`>`
    ///
    /// The object already exists.
    /// [ADR 0045](../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md)
    /// places every program literal in the heap before the run's first
    /// instruction executes, so this is a load of a precomputed address —
    /// no branch, no allocation, no copy, whether this is the first turn of
    /// a loop or the millionth.
    Str { dst: Slot, text: StrId },
    /// `dst = src`, for the words `layout` describes.
    ///
    /// This is ADR 0001's field-wise shallow copy, and it is one operation
    /// because a value's words are where the value is. Copying a
    /// `Wrapper { p: Point, v: Vector }` copies three words: the `Point`
    /// becomes independent because its words were copied, and the `Vector`
    /// stays shared because what was copied is its address. Both answers
    /// fall out of the same copy and neither needs a policy.
    ///
    /// There is no sharing bit, no copy-on-write and no unsharing of a write
    /// path. Those were needed only while every struct was one address, and
    /// they existed to conceal an alias the representation had created.
    ///
    /// `let` and `var` lower to the same thing: ADR 0001 says they do not
    /// change expression semantics, and Cove has no move semantics. A
    /// lowering may elide a copy whose source is a fresh temporary, but that
    /// is an optimisation — correctness never depends on proving uniqueness,
    /// and a lowering that cannot tell emits the copy.
    Copy {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// Zeroes the words `layout` describes at `slot`.
    ///
    /// A slot whose value is dead. The lowering emits one at the end of the
    /// scope a binding belonged to, and at a temporary's last use, for every
    /// slot whose [`Repr`](crate::Repr) is [`Ref`](crate::Repr::Ref) or
    /// [`Addr`](crate::Repr::Addr).
    ///
    /// This is what keeps a static reference map from turning into a leak.
    /// The map says which slots the collector *reads*; it cannot say when
    /// the value in one stopped being needed, because that is a fact about a
    /// program point and the map is a fact about a function. Clearing the
    /// slot moves the answer into the data: a dead reference slot holds
    /// null, the collector reads null, and the object is unreachable at the
    /// next collection rather than at the next return.
    ///
    /// It costs one store on a path that was going to leave the value behind
    /// anyway, and it is emitted only where the slot would otherwise retain
    /// something — never for a scalar, and never where the slot is about to
    /// be overwritten.
    Clear { slot: Slot, layout: LayoutId },

    // ---- scalar operations --------------------------------------------
    /// `dst = -a`
    Neg { num: Num, dst: Slot, a: Slot },
    /// `dst = a op b`
    Arith {
        num: Num,
        op: ArithOp,
        dst: Slot,
        a: Slot,
        b: Slot,
    },
    /// `dst = a op b`, answering a `Bool` — or, for [`CmpOp::Order`], the
    /// `Int` `-1`, `0` or `1`.
    Cmp {
        on: Compare,
        op: CmpOp,
        dst: Slot,
        a: Slot,
        b: Slot,
    },
    /// `dst = a op value`, on `Int` words, where `value` was written in the
    /// source.
    ///
    /// The same arithmetic [`Inst::Arith`] does — the same overflow, the same
    /// division and remainder by zero, the same `Duration` naming — with the
    /// right operand in the instruction instead of in a slot. It exists
    /// because the alternative is worse than a wasted word: the literals of a
    /// loop condition are materialised by instructions the back edge jumps
    /// over, so `while i < 2000000` executed an [`Inst::Int`] two million
    /// times to write a constant into a temporary that nothing else ever
    /// read.
    ///
    /// **Two variants and not sixteen.** `op` is a field here exactly as it
    /// is on [`Inst::Arith`], so an operator added to the language costs no
    /// instruction, and this pair covers the eleven that exist.
    ///
    /// **The immediate is on the right, and only on the right.** `a - 1` and
    /// `1 - a` are different questions and `a % 7` and `7 % a` more so, so a
    /// left-hand immediate would be a second family rather than a mirror of
    /// this one; a commutative operator's lowering puts the literal on the
    /// right instead. There is no float immediate for the same reason there
    /// is no left one — a second family, for a form no benchmark asked for.
    ///
    /// `Num` is absent because there is only [`Num::Int`] to name: `value` is
    /// an `i64`, and a `Duration`'s word is nanoseconds, which is an `i64`.
    ArithImm {
        op: ArithOp,
        dst: Slot,
        a: Slot,
        value: i64,
    },
    /// `dst = a op value`, comparing `Int` words, answering a `Bool`.
    ///
    /// [`Inst::ArithImm`]'s other half, and `Compare` is absent for the
    /// reason `Num` is absent there: the operand is an `Int` word, so the
    /// comparison is [`Compare::Int`].
    CmpImm {
        op: CmpOp,
        dst: Slot,
        a: Slot,
        value: i64,
    },
    /// `dst = !a`
    Not { dst: Slot, a: Slot },
    /// `dst = <a, converted>`
    Convert { to: Convert, dst: Slot, a: Slot },
    /// `dst = |a|`, on an IEEE-754 double: **bit 63 cleared, and no other bit
    /// touched.**
    ///
    /// That is the contract and not a description of one implementation of it.
    /// IEEE 754 makes `abs` a *sign-bit* operation rather than an arithmetic
    /// one, so it never rounds, never signals, and — the part that is easy to
    /// lose — never **quiets** a signalling NaN. A route through arithmetic
    /// reaches the same magnitude for every ordinary operand and sets bit 51
    /// on a signalling one: measured at run time, `0.0 - x`, `-1.0 * x` and
    /// `x + 0.0` each turn `0xfff0_0000_dead_beef` into
    /// `0xfff8_0000_dead_beef` where this answers
    /// `0x7ff0_0000_dead_beef`. Both tiers are held to the same table of bit
    /// patterns, signalling rows included: the encoded VM in `vm::exec`'s
    /// `a_float_absolute_clears_the_sign_bit_and_nothing_else`, and the native
    /// lowering in `cove-native`'s `tests/suite`'s `ABSOLUTES`. They are run
    /// against *each other* by `cove-runtime`'s `native_tier.rs`, on real Cove
    /// source rather than on hand-built IR.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
    /// Decision 2 admits "a typed scalar operation that maps to a CPU or
    /// backend operation (`sqrt`, `abs`, `round`, a checked typed
    /// conversion)" below the standard library, and this is that: it is total,
    /// it allocates nothing, it touches no heap, and the only failure it could
    /// have is one no program wrote down — so there is none. `Float.abs` was
    /// an `Inst::IntrinsicCall` naming a method until this replaced it.
    ///
    /// **It is not a family, and that is the whole of the decision.**
    /// `Float.round` and `Float.sqrt` were the census's other two typed
    /// scalar operations; a `FloatUnary { op }` added here
    /// would have been a one-member family guessing at both of them, and the
    /// guess would have bought nothing — the bytecode gives a member of such a family one opcode
    /// each exactly as three instructions would, and each of the other two has
    /// a question of its own still to settle (`round`'s tie rule is on
    /// Decision 6's list of things pinned before the arm that implements them
    /// is touched; `sqrt` is the one *irrational* operation IEEE 754 still
    /// requires to be correctly rounded, so its bits are the same on every
    /// conforming machine and a Newton iteration in Cove would not be).
    /// Three near-identical instructions later is a
    /// mechanical refactor into one family; a family now is a decision taken
    /// for two operations nobody has measured. `PHILOSOPHY.md`'s "earn
    /// complexity through use", and one caller is one caller.
    ///
    /// **Both have since arrived and the guess would have been wrong**, which
    /// is worth leaving on the record beside the reasoning:
    /// [`Inst::FloatRound`] and [`Inst::FloatSqrt`] are separate instructions
    /// and not members here, because the three lowerings have nothing in
    /// common at all — five bytes of `btr`, eighty-seven bytes of conversion
    /// and select, and one `sqrtsd` — so a shared `op` field would have
    /// selected between three sequences with no shared shape to factor.
    ///
    /// **There is no `Num` on it**, where [`Inst::Neg`] has one. An `Int`'s
    /// absolute value is not this operation: `Int.abs` raises at `Int.MIN`,
    /// where two's complement has no answer, and it is `std.int.abs` in Cove
    /// for that reason. A `Num::Int` arm here would be the second, wrong
    /// answer in the IR that Decision 6 refuses — the refusal that issue
    /// #454's Step 2 acted on when it deleted [`Convert`]'s float-to-int
    /// member, whose `x as i64` disagreed with `Float.toInt`'s checked
    /// `Result` in every direction it could.
    ///
    /// The native tier lowers it: `btr` clears bit 63 on an integer register,
    /// five bytes, the word never reaching an SSE register at all.
    /// `cove-native`'s `tests/suite`'s `ABSOLUTES` holds it to the bit — a
    /// NaN's payload and its quiet bit included, which no Cove program can
    /// see — and `cove-runtime`'s `native_tier.rs` holds it against the VM for
    /// what one can. That it is lowered rather than refused is ADR 0065's
    /// Decision 5 applied again, and is why this route is available at all: a
    /// refusal would take every caller back to the VM.
    FloatAbs { dst: Slot, a: Slot },
    /// `dst = min(a, b)` or `dst = max(a, b)`, on two IEEE-754 doubles — and
    /// **neither of them is the IEEE 754 operation of that name.**
    ///
    /// The contract, which is what the native lowering is written against and
    /// what three tiers of tests hold it to:
    ///
    /// ```text
    /// min(a, b) = b is NaN -> a | a is NaN -> b | a < b -> a | otherwise b
    /// max(a, b) = b is NaN -> a | a is NaN -> b | a > b -> a | otherwise b
    /// ```
    ///
    /// Three things in that are worth saying in words, because all three are
    /// places an implementation goes wrong while passing every ordinary case.
    ///
    /// **It absorbs a NaN, it does not propagate one.** `min(NaN, x)` is `x`
    /// and `min(x, NaN)` is `x`, which is IEEE 754-2008's `minNum` rather than
    /// IEEE 754-2019's `minimum`. That is not a subtlety, and it is why this
    /// contract is written out here rather than delegated to whatever a
    /// machine or a back end calls `fmin`: the retired Cranelift arm's `fmin`
    /// *was* the 2019 operation, so that arm could not use the instruction
    /// named after the operation and was three `select`s over `fcmp` instead.
    ///
    /// **On operands that compare equal the answer is the second one.**
    /// `-0.0 == 0.0` is true, so `min(-0.0, +0.0)` is `+0.0` and
    /// `min(+0.0, -0.0)` is `-0.0` — and `max` answers those same two, which
    /// is *not* what IEEE 754's `maxNum` gives. Rust's documentation declines
    /// to decide this ("either input may be returned non-deterministically"),
    /// so what makes it a contract is that a Cove program can see it: a zero's
    /// sign is observable three ways, and `tests/e2e/values_float_min_max`
    /// pins all three in both argument orders. x86-64's `minsd` has exactly
    /// this tie rule, which is not a coincidence — it is where the behaviour
    /// comes from.
    ///
    /// **The answer is bit-identical to one of the two operands, always.**
    /// Nothing here rounds, and nothing here **quiets** a signalling NaN:
    /// every tier selects a whole word rather than computing one. A route
    /// through arithmetic — a subtraction to compare with, a multiply by one
    /// to move with — would set bit 51 on a signalling operand, and the
    /// signalling rows of `cove-native`'s `tests/suite`'s `EXTREMA` are what
    /// catch it. Both tiers are held to that one table: the encoded VM in
    /// `vm::exec`'s `a_float_extremum_answers_one_of_its_operands` and the
    /// native lowering in `cove-native`'s `tests/template.rs`. They are run
    /// against *each other* by `cove-runtime`'s `native_tier.rs`, on real Cove
    /// source rather than on hand-built IR.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
    /// Decision 2 admits "a typed scalar operation that maps to a CPU or
    /// backend operation" below the standard library and refuses one named
    /// after a method, and this is that. `Float.min` and `Float.max` were
    /// `Inst::IntrinsicCall`s carrying the methods' own names until this
    /// replaced them; ADR 0064's census proposed "Cove over typed compare"
    /// instead, and that route is **blocked** — the body is four instructions
    /// of float comparison and `crates/cove-native/src/subset.rs` admits
    /// none of them, so a caller of it is refused and taken back to the VM.
    /// Checked rather than assumed: a caller whose own float work is nothing
    /// at all is refused with `CmpBranch(Float, Ne) at pc 4, also blocked by:
    /// CmpBranch(Float, Lt) x1`, while the same caller over the operation
    /// below it compiles, in the same program in the same run.
    ///
    /// **There is no `Num` on it**, for [`Inst::FloatAbs`]'s reason: `Int.min`
    /// and `Int.max` are `std.int.min` and `std.int.max`, Cove over `<`, and
    /// they need no tie rule and no NaN rule because an `Int` has neither.
    FloatMinMax {
        op: MinMax,
        dst: Slot,
        a: Slot,
        b: Slot,
    },
    /// `dst = round(a)`, on one IEEE-754 double: the nearest integer, with a
    /// half going **away from zero**.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
    /// Decision 2 names `round` in its list of typed scalar operations, and
    /// this is it. `Float.round` was an [`Inst::IntrinsicCall`] carrying the
    /// method's own name until this replaced it; the Cove-body route the
    /// census proposes instead is **blocked**, for the reason it is blocked
    /// for [`Inst::FloatMinMax`]: `crates/cove-native/src/subset.rs` admits no
    /// float constant, comparison or arithmetic, so a body written over any of
    /// them would take every caller of `Float.round` back to the VM. Checked
    /// rather than assumed — see the measurement in `benches/floatround`.
    ///
    /// # The contract
    ///
    /// The four things a reimplementation has to get right, in the order they
    /// are easy to get wrong:
    ///
    /// **A half goes away from zero, not to even.** `0.5` is `1.0`, `2.5` is
    /// `3.0`, `4.5` is `5.0`, and the negatives are the mirror of those.
    /// Ties-to-even — which is what a machine's default rounding mode is, what
    /// x86-64's `roundsd` offers and what `f64::round_ties_even` is — answers
    /// `0.0`, `2.0` and `4.0` there. It does **not** disagree on every half:
    /// `1.5` and `3.5` are `2.0` and `4.0` under both rules, so a table whose
    /// halves all have an odd floor would pass an implementation with the tie
    /// backwards. `tests/e2e/values_float_round` and `cove-native`'s
    /// `tests/suite`'s `ROUNDINGS` both carry halves of each parity for that
    /// reason.
    ///
    /// **The sign of the operand is the sign of the answer, zeros included.**
    /// `(-0.4).round()` is `-0.0` and not `+0.0`, and `-0.0` is
    /// distinguishable from `0.0` in Cove three ways — interpolation,
    /// `format`, and `1.0 / x` — so this is a contract rather than an
    /// observation. Every subnormal answers a zero of its own sign too.
    ///
    /// **A value with no fractional part is handed back unchanged, to the
    /// bit.** Every double at or past `2^52` is already an integer, and so are
    /// both infinities; the largest magnitude with a fraction at all is
    /// `2^52 - 0.5`, whose answer is `2^52`.
    ///
    /// **A NaN keeps its sign and its payload and is *quieted*.** That last
    /// part is where this differs from [`Inst::FloatAbs`] and
    /// [`Inst::FloatMinMax`], both of which hand an operand back whole: this
    /// operation's answer is computed rather than selected, the computation is
    /// an addition, and an addition sets bit 51. No Cove program can see any
    /// of it — there is no bit access to a `Float` and every NaN renders as
    /// `NaN` — so it is pinned in bits, in `cove-native`'s `tests/suite`'s
    /// `ROUNDINGS`, whose signalling rows are the ones that say so.
    ///
    /// # Why the runtime still calls `f64::round`
    ///
    /// [`crate::MinMax`]'s contract is spelled out in `cove-runtime`'s
    /// `float::extremum` rather than delegated to `f64::min`, because
    /// `f64::min`'s own documentation declines to decide its answer on
    /// operands that compare equal — so a tier that delegated would be a tier
    /// whose answer is a fact about which `rustc` built it. **`f64::round`
    /// declines nothing that a Cove program can see.** Its first sentence
    /// fixes the tie ("If a value is half-way between two integers, round away
    /// from `0.0`") and the sentence after it refuses any latitude at all
    /// ("This function always returns the precise result"); IEEE 754's
    /// `roundToIntegralTiesToAway` fixes the sign of a zero answer, on the
    /// same footing that makes `f64::abs` a sign-bit operation and `f64::sqrt`
    /// correctly rounded — which is the footing `cove-runtime`'s `float`
    /// module already names as keeping both of those *out* of it. So a
    /// `float::round` beside `float::extremum` would be a **copy** of
    /// `f64::round` rather than a departure from it, and the only oracle a
    /// copy has is the thing it copied. Both evaluators call `f64::round`;
    /// what is written down instead of a fourth implementation is this
    /// contract, and the tables that hold the native lowering to it.
    ///
    /// # The native lowering, and why it is seventeen instructions
    ///
    /// **x86-64 has no instruction for this.** `roundsd`'s four modes are
    /// nearest-even, floor, ceiling and truncate; half-away-from-zero is not
    /// among them, and `roundsd` is SSE4.1 where this code generator is SSE2.
    /// What `rustc -O` emits for `f64::round` on this target — read rather
    /// than guessed — is `trunc(x + copysign(0.49999999999999994, x))`, and
    /// the constant is `nextdown(0.5)` rather than `0.5` because
    /// `0.49999999999999994 + 0.5` rounds *up* to `1.0` and would answer `1`
    /// where the answer is `0`. The template arm is that identity over `|x|`,
    /// with the truncation done by `cvttsd2si`/`cvtsi2sd` instead of the
    /// `roundsd` it has not got, a magnitude test for the range where that
    /// pair has no answer, and the operand's sign bit `or`ed back on at the
    /// end. See the arm itself for the sequence.
    ///
    /// **There is no `Num` on it**, for [`Inst::FloatAbs`]' reason: an `Int`
    /// is already an integer and has nothing to round.
    FloatRound { dst: Slot, a: Slot },
    /// `dst = sqrt(a)`, on one IEEE-754 double: the **correctly rounded**
    /// square root.
    ///
    /// [ADR 0064](../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
    /// Decision 2 names `sqrt` first in its list of typed scalar operations
    /// and then names this instruction in as many words — "`Float.sqrt` need
    /// not be a Newton iteration in Cove. A typed scalar IR operation that
    /// maps to `sqrtsd` passes Decision 2: it names machine semantics and
    /// would not be renamed if `Float.sqrt` were." This is it, and it is the
    /// last of the four. `Float.sqrt` was an [`Inst::IntrinsicCall`] carrying
    /// the method's own name until this replaced it; the Cove-body route the
    /// census proposes instead is **blocked**, for the reason it is blocked
    /// for [`Inst::FloatAbs`], [`Inst::FloatMinMax`] and [`Inst::FloatRound`]:
    /// `crates/cove-native/src/subset.rs` admits no float constant,
    /// comparison or arithmetic, and a Newton iteration needs all three. That
    /// was checked by lowering one rather than assumed — see the measurement
    /// in `benches/floatsqrt`.
    ///
    /// # The contract
    ///
    /// **The answer is correctly rounded, and that is the whole of the
    /// numeric contract.** IEEE 754 §5.4.1 makes `squareRoot` one of the
    /// operations that must be computed as if to infinite precision and then
    /// rounded once, so there is exactly one answer for every operand whose
    /// answer is a number and it is the same on every conforming machine.
    /// It is the only one of the four typed scalar operations for which that
    /// is true — `abs` is a sign bit, [`crate::MinMax`]'s tie is Cove's own
    /// choice, and `round`'s tie rule is a choice between two defensible
    /// rules — and it is why the tables below carry roots that are *not*
    /// representable as well as ones that are: an implementation accurate to
    /// within an ulp passes every exact root and fails those.
    ///
    /// **A zero keeps the sign of its operand.** IEEE 754 §5.4.1 states
    /// `squareRoot(-0)` is `-0`, so `-0.0` is the one negative operand whose
    /// answer is not a NaN, and `-0.0` is distinguishable from `0.0` in Cove
    /// three ways — interpolation, `format`, and `1.0 / x`. A lowering that
    /// refused a negative operand by testing the sign *bit* would get exactly
    /// this one wrong.
    ///
    /// **`+inf` answers `+inf`, and is the only infinity whose answer is a
    /// number.** Every negative operand but `-0.0`, and `-inf` with them, is
    /// IEEE 754's invalid operation and answers a quiet NaN.
    ///
    /// **Which quiet NaN is not IEEE 754's to say, and it is not the same one
    /// for the two ways of getting one.** For a *negative* operand the
    /// standard requires a quiet NaN and leaves its sign and payload to the
    /// implementation (§6.2); x86-64 answers the "QNaN floating-point
    /// indefinite", `0xfff8_0000_0000_0000` — sign bit **set**, and the same
    /// bits whatever the operand was. For a *NaN* operand the answer is that
    /// operand with its sign and its payload kept and its quiet bit set. No
    /// Cove program can see any of it — there is no bit access to a `Float`
    /// and every NaN renders as `NaN` — so it is pinned in bits, in
    /// `cove-native`'s `tests/suite`'s `SQUARE_ROOTS`, whose host is x86-64
    /// by construction. `cove-runtime`'s own table pins the rest and asserts
    /// only *quietness* for an invalid operation, because that crate is not
    /// x86-64 and the bits would be a fact about the host rather than about
    /// the operation.
    ///
    /// # Why the runtime still calls `f64::sqrt`
    ///
    /// [`crate::MinMax`]'s contract is spelled out in `cove-runtime`'s
    /// `float::extremum` rather than delegated to `f64::min`, because
    /// `f64::min`'s own documentation declines to decide its answer on
    /// operands that compare equal. **`f64::sqrt` declines nothing, and here
    /// the reason is the standard rather than the documentation**: a
    /// correctly rounded operation has one answer, so a `float::sqrt` beside
    /// `float::extremum` could only be a slower way of computing the same
    /// bits, with no oracle but the thing it replaced. Both evaluators call
    /// `f64::sqrt`; what is written down instead of a third implementation is
    /// this contract, and the tables that hold the native lowering to it.
    ///
    /// # The native lowering, and why it is two instructions
    ///
    /// **x86-64 has the instruction.** `sqrtsd` is SSE2, is correctly rounded
    /// by the same clause of the same standard, and takes a memory operand —
    /// so the arm is `sqrtsd xmm0, [frame+a]` and `movsd [frame+dst], xmm0`,
    /// two instructions and eighteen bytes, against three and nineteen for
    /// [`Inst::FloatAbs`] and seventeen and eighty-seven for
    /// [`Inst::FloatRound`].
    ///
    /// **It is the shortest of the four in instructions and the dearest in
    /// time, which is the finding rather than an aside.** `benches/floatsqrt`
    /// measures the compiled operation at 1.30 ns for an operand whose root is
    /// exact and 2.37 ns for one whose root is not, where `FloatRound`'s
    /// seventeen instructions measure 0.91 ns and `FloatAbs`' three measure
    /// 0.097 ns on the same machine. So instruction count does not order these
    /// four by cost: the square-root unit is not the ALU, and this is the
    /// first of these migrations where what is left after the crossing is
    /// removed is the operation itself rather than the dispatch around it. A
    /// **subnormal** operand is dearer again by a factor of fifteen, in both
    /// arms and on both tiers, which puts it in the hardware and not in
    /// anything Cove does.
    ///
    /// **There is no `Num` on it**, for [`Inst::FloatAbs`]' reason, and more
    /// strongly: there is no integer square root in the language to be the
    /// other member of a family.
    FloatSqrt { dst: Slot, a: Slot },

    // ---- control flow --------------------------------------------------
    /// Continue at `to`.
    Jump { to: Pc },
    /// Continue at `to` when `cond` is false; otherwise fall through.
    ///
    /// One conditional branch rather than two: `&&`, `||`, `if` and `while`
    /// all lower through it, and the lowering inverts the condition rather
    /// than the instruction set carrying both polarities.
    BranchFalse { cond: Slot, to: Pc },
    /// [`Inst::Cmp`] and then [`Inst::BranchFalse`] on what it wrote, as one
    /// instruction.
    ///
    /// `dst = a op b`, answering a `Bool`, and then continue at `target` when
    /// that `Bool` is false. It is **exactly the two instructions it
    /// replaces, in that order, with no condition attached** — the `Bool` is
    /// still written to `dst`, and anything that reads `dst` afterwards reads
    /// what the unfused pair would have left there.
    ///
    /// That is the whole of its correctness argument, and
    /// [ADR 0054](../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md)
    /// chooses it over the alternative deliberately. An instruction that
    /// skipped the write would be sound only where nothing reads `dst`, which
    /// is a liveness question, and what the fusion saves is the *dispatch*
    /// rather than the store. Not asking the question costs one slot write on
    /// the paths where the answer would have been "nothing reads it" and buys
    /// a rule with no side conditions.
    ///
    /// Nothing lowers to this. [`mod@crate::lower`]'s peephole recognises the
    /// pair in finished code — a comparison, the `branch-false` beside it
    /// reading the slot it wrote, and no jump, branch or switch in the
    /// function naming that `branch-false` — so a form the peephole cannot
    /// see is a missed fusion rather than a wrong one.
    CmpBranch {
        on: Compare,
        op: CmpOp,
        dst: Slot,
        a: Slot,
        b: Slot,
        target: Pc,
    },
    /// [`Inst::CmpImm`] and then [`Inst::BranchFalse`] on what it wrote, as
    /// one instruction.
    ///
    /// [`Inst::CmpBranch`]'s other half, with the same semantics and the same
    /// argument: the `Bool` is written to `dst` and then `target` is taken
    /// when it is false.
    ///
    /// **The immediate is an `i32` here and an `i64` on [`Inst::CmpImm`].**
    /// The sixteen-byte instruction spends two slots on `dst` and `a` and
    /// then has one payload word left for two numbers, so the immediate and
    /// the target are its two halves. A comparison against a wider immediate
    /// is left as the unfused pair, which is the existing instructions doing
    /// what they already do — and the narrowing is *checked* rather than
    /// assumed, because a fusion that dropped the high bits would be a wrong
    /// answer and not a slow one.
    CmpImmBranch {
        op: CmpOp,
        dst: Slot,
        a: Slot,
        value: i32,
        target: Pc,
    },
    /// Continue at the entry of `table` selected by the `Int` in `on`.
    ///
    /// This is how a `match` over an enum's cases dispatches: `on` is the
    /// case index read out of the object, and the table has one target per
    /// case plus a default.
    Switch { on: Slot, table: TableId },
    /// Leave the function, answering the value at `src`.
    ///
    /// `src` is the *first* slot of that value location, and how many words
    /// follow it is [`crate::Function::returns`] — which is why a listing
    /// writes that layout *on* the slot, and the whole run with it:
    /// `return s0..s2:Result`.
    Return { src: Slot },

    // ---- calls ----------------------------------------------------------
    /// `dst = callee(args...)`
    ///
    /// The machine writes `args[i]` into the callee's slot `i` and gives it
    /// a frame beginning at the end of this one. Nothing else happens: the
    /// argument list is static, the destination is declared, and there is no
    /// buffer between the two frames.
    Call {
        dst: Slot,
        callee: FunctionId,
        args: ArgsId,
    },
    /// `dst = closure(args...)`, where `closure` holds a reference to a
    /// [`crate::Shape::Closure`] object.
    ///
    /// The callee is the function id in the object's first payload word, and
    /// its captures are copied into the slots after the parameters.
    ///
    /// `result` is the layout of what the call answers, and it is the one
    /// operand here that is not read off the object at run time. Which body
    /// this enters is a run-time fact; how wide its answer is, is not. The
    /// checker settles a call through a value against the callee's *function
    /// type*, so the answer's type — and with it the run of words the
    /// destination has to be — is as static as any other call's.
    ///
    /// It is carried rather than looked up because there is nowhere to look:
    /// every other call names a callee the program declares — a
    /// [`FunctionId`], a [`crate::HostOpId`], a [`crate::SiteId`] — and
    /// the answer's layout is read from that declaration. A closure call
    /// names a word in a slot. Without this field the destination's width
    /// was known to the checker, thrown away by the lowering, and then
    /// unavailable to everything downstream: `crate::verify` could ask only
    /// that `dst` was a slot at all, the encoded verifier the same, and a
    /// listing had to print the head word's `Repr` where every other call
    /// prints the run. A two-word answer written into the last slot of a
    /// frame was checked by nothing.
    CallClosure {
        dst: Slot,
        closure: Slot,
        args: ArgsId,
        result: LayoutId,
    },
    /// `dst = <host op>(args...)`
    ///
    /// This is a boundary: the arguments are materialised into public
    /// public `Value`s, the host answers one, and the answer
    /// is written back into a word. It is the only place in ordinary
    /// execution where a `Value` exists.
    CallHost {
        dst: Slot,
        op: HostOpId,
        args: ArgsId,
    },
    /// `dst = <host op>(*receiver, args...)`, addressed to the resource
    /// the [`Repr::Host`](crate::Repr::Host) word in `receiver` names.
    ///
    /// The same boundary [`Inst::CallHost`] is, reached the other way a
    /// callee can be found. `Call` and `CallClosure` are already that pair
    /// on this side of the boundary — a callee named statically, and a
    /// callee in a slot — and a host resource's operations are the same
    /// distinction one boundary further out: ADR 0013 gives the *host* the
    /// table of what is open, so `files.Writer.writeLine` is dispatched on
    /// the handle and not on the module the source wrote in front of it.
    ///
    /// The receiver is an operand of its own rather than `args[0]`, and that
    /// is the difference that decides there are two instructions here rather
    /// than a flag on one. An [`crate::Arg`] is a value location the
    /// boundary *materialises*, and the registry does not take the handle as
    /// an argument — `HostRegistry::call_resource` takes it as the thing
    /// being addressed and hands the host only what follows. So putting it
    /// in the list would mean materialising a name into a `Value` in order
    /// to take it apart again, and the argument list would no longer be the
    /// arguments.
    CallResource {
        dst: Slot,
        receiver: Slot,
        op: HostOpId,
        args: ArgsId,
    },
    /// `dst = <builtin>(args...)`
    ///
    /// A builtin operates on words and heap objects directly. It is not a
    /// boundary and it does not materialise anything.
    IntrinsicCall {
        dst: Slot,
        site: SiteId,
        args: ArgsId,
    },

    // ---- the heap --------------------------------------------------------
    /// `dst = <a new object of `layout`>`
    ///
    /// The payload is zeroed, so a reference field of a half-built object is
    /// null rather than garbage if a collection happens before it is
    /// filled in.
    Alloc {
        dst: Slot,
        layout: LayoutId,
        len: Len,
    },
    /// `dst = <the value at payload word `at` of `obj`>`
    ///
    /// One instruction for every fixed-position read there is: a struct
    /// field, an enum's case index (`at == 0`) or payload word, a closure's
    /// capture. The lowering computes `at` from the layout it knows
    /// statically; the machine bounds-checks it against the layout the
    /// object names, because a reference slot carries no layout of its own.
    LoadField {
        dst: Slot,
        obj: Slot,
        at: u32,
        layout: LayoutId,
    },
    /// `<payload word `at` of `obj`> = src`
    StoreField {
        obj: Slot,
        at: u32,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = obj[index]`, for an object whose elements are `layout` wide.
    ///
    /// The stride is the element layout's width, so an `Array<Point>` is a
    /// run of two-word elements rather than a run of addresses.
    LoadElem {
        dst: Slot,
        obj: Slot,
        index: Slot,
        layout: LayoutId,
    },
    /// `obj[index] = src`
    StoreElem {
        obj: Slot,
        index: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = run[index]`, one unit of a run in `storage`: ADR 0058's
    /// `run-load dst, run, index, storage`.
    ///
    /// For [`Storage::PackedBytes`] the unit is a byte, and `dst` is an `Int`
    /// in `0..=255`; `run` is a `String` and `index` a byte offset into it.
    /// That is the only storage admitted today — `crate::verify` refuses
    /// [`Storage::Words`], whose element reads are still [`Inst::LoadElem`] —
    /// so what follows is the byte form's account.
    ///
    /// It began as `byte-at`, and is that instruction with the unit named
    /// rather than assumed, for [`Inst::RunCopy`]'s reason: a byte read is
    /// one member of a family of run operations, and the optimizer should see
    /// the family rather than `String.byteAt`.
    ///
    /// The one instruction that reaches *inside* a word. Everything else here
    /// addresses a value location or a payload word, because a word is what a
    /// frame and a heap object are made of — but a `String`'s payload is
    /// bytes, eight to a word, and the only shape that reads one is this.
    ///
    /// It is an instruction and not a builtin, and the difference is the
    /// whole reason it exists. `String.byteAt` as an `intrinsic-call` measured
    /// 58 ns of which 48 ns was *being a builtin call* — the operands copied
    /// into a buffer, the operand array built, the dispatch by two strings,
    /// the answer written back — for work that is one payload word, a shift
    /// and a mask. `benches/builtincall` is where those two numbers are.
    ///
    /// `index` is bounds-checked against the receiver's byte length, and an
    /// offset outside it stops the run. That is `String.sliceBytes`'s rule
    /// and not `Array.get`'s: a byte offset out of range is one this type
    /// never handed out, where an index out of range is arithmetic a caller
    /// did about a sequence it can count. Answering an `Option` here would
    /// also be answering it eight times per word of a lexer's inner loop,
    /// and the wrapper was measured at more than the read.
    RunLoad {
        dst: Slot,
        run: Slot,
        index: Slot,
        storage: Storage,
    },
    /// A bulk range copy between two runs: `dst[dst_at .. dst_at+count] =
    /// src[src_at .. src_at+count]`, in units of `storage`.
    ///
    /// [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
    /// `run-copy dst, dstOffset, src, srcOffset, count, storage`: the one
    /// copy beneath string slicing and appending, `Vector.toArray`,
    /// `Array.toVector`, vector growth, removal and bulk construction. It
    /// began as [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
    /// `copy-bytes`, and is that instruction with the unit named rather than
    /// assumed: for [`Storage::PackedBytes`] a unit is a byte, and for
    /// [`Storage::Words`] a unit is a whole element of the layout, `stride`
    /// words wide. It is not a Cove loop over [`Inst::RunStore`] or
    /// [`Inst::StoreElem`], because that would turn one bulk operation into a
    /// dispatch and a safepoint per unit — which ADR 0051's "why a byte loop in
    /// IR is not enough" and ADR 0058 both reject.
    ///
    /// # What it means
    ///
    /// **`memmove`.** The source is read as it was before the copy, so the two
    /// ranges may overlap — a run shifting its own units along is the obvious
    /// use, and a vector's `remove` is exactly that.
    ///
    /// **Bounds in units, before any write.** `dst_at`, `src_at` and `count`
    /// are checked against each object's header length, which is its logical
    /// length in units, and a failure stops the run with nothing written. They
    /// are never multiplied into words until after that check.
    ///
    /// **One family on both sides.** For [`Storage::PackedBytes`], `src` may be
    /// a `String` **or** a [`crate::Shape::Bytes`] run — a fused slice copies
    /// straight out of the run that produced it — and `dst` must be a
    /// [`crate::Shape::Bytes`] run: writing into a `String` is refused,
    /// because a `String`'s bytes are an invariant a finish establishes and
    /// nothing reopens. For [`Storage::Words`], both must be
    /// [`crate::Shape::Elements`] of exactly that element layout, fixed or
    /// growable, because the collector traces each by its own layout's
    /// reference map and a unit of another family would be traced wrongly.
    ///
    /// # What it costs, and what it is charged
    ///
    /// One dispatch and one unit of work per payload word moved, which is
    /// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
    /// "charged proportionally to the bytes or words examined". A word is the
    /// unit of *work* whatever the unit of the run is, because a word is what
    /// the memory moves: a byte run is charged by the words its bytes touch
    /// and an element run by `count * stride`.
    ///
    /// The charge is not folded into the count of instructions dispatched.
    /// That number is a public observable — the debugger, the trace, the
    /// profile and `cove-bench` all report it, and `crate::vm::profile`'s own
    /// test asserts its per-opcode totals sum to it — so weighted work has a
    /// coordinate of its own.
    ///
    /// The copy is made in bounded chunks with a safepoint between them, and
    /// that is not a refinement of the charge but the thing that makes it
    /// sound. A charge taken only *after* an arbitrarily large copy would let
    /// one instruction run arbitrarily far past a fuel or cancellation bound
    /// before anything looked, which
    /// [ADR 0040](../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
    /// `S + T` forbids. One chunk is one stride of work in either storage — a
    /// byte chunk and a word chunk move the same number of words — so a
    /// stopped run gets no further than a stride past the bound whatever
    /// length it was given. A word chunk is a whole number of elements.
    ///
    /// A collection may therefore happen with the destination part written.
    /// The collector is non-moving, so neither address changes across it, and
    /// it needs no barrier: it is a stop-the-world mark from the roots, not a
    /// generational or incremental one with a remembered set to keep. What it
    /// does need is both objects rooted and the destination walkable, and both
    /// hold: the caller has `sync`ed and both objects are named by frame slots
    /// this instruction read them out of; a fresh destination's payload is
    /// zeroed by allocation, and every word already copied is a whole word of
    /// a unit whose layout the destination's reference map agrees with.
    ///
    /// [`Inst::GrowableAlloc`] and [`Inst::RunFinish`] are **not** charged
    /// this way and not chunked. Their bulk work is inside the allocator's
    /// zeroing and inside one `from_utf8` over a copy of the run, neither of
    /// which this could interrupt, and charging an operation that cannot be
    /// interrupted only makes its overshoot visible rather than bounded. They
    /// remain one unit each, which is what an ordinary [`Inst::Alloc`] of a
    /// large `Array` has always been.
    ///
    /// # Why five operands live behind an [`ArgsId`]
    ///
    /// An encoded instruction has room for three slot-sized operands and a
    /// payload, and this needs five: `dst`, `dst_at`, `src`, `src_at` and
    /// `count`. Rather than spend a second instruction pair to carry the
    /// overflow, this reuses the machinery a call's argument list already is —
    /// [`ArgsId`] names a row of [`crate::Program::args`], and a call already
    /// demonstrates that an arity larger than three operands is a solved
    /// problem in this format. The row holds exactly five [`crate::Arg`]s, in
    /// the order `dst`, `dst_at`, `src`, `src_at`, `count`, and carries each
    /// one's layout the same way a call's arguments do, so the verifier checks
    /// them by the same rule rather than by a new one. The storage is not in
    /// the row: it is the instruction's own, and the encoding carries a
    /// [`Storage::Words`] layout in the payload half the row leaves free.
    RunCopy { args: ArgsId, storage: Storage },
    /// `dst = <a fresh fixed run holding src[from .. from+count]>`, in units
    /// of `storage`.
    ///
    /// # In ADR 0058's families
    ///
    /// `run-alloc dst, storage, count` → `run-copy dst, 0, src, from, count`,
    /// as one instruction: #378's Q3, an exact construction that never exposes
    /// an unfinished fixed run to Cove. Between the two halves the fresh run is
    /// allocated and not yet filled, and a split form would hand a Cove program
    /// a run whose units are zeroes rather than elements; as one instruction
    /// the run is written into `dst` only once the machine holds it, and the
    /// copy that fills it is [`Inst::RunCopy`]'s, chunks, polls and charge
    /// included.
    ///
    /// It is what `Array.slice`, `Vector.slice` and `Vector.toArray` lower to
    /// beneath their Cove bodies — `core.arraySlice` and `core.vectorSlice` —
    /// what the lowering's own copies (a `for` over a vector, `sorted`'s
    /// working copy) emit directly, and, over bytes, what `String.sliceBytes`
    /// lowers to beneath its own — `core.stringSlice`.
    ///
    /// # What it means
    ///
    /// **Bounds in units, before anything is allocated.** `from` and `count`
    /// are checked against `src`'s header length, which for a vector's store is
    /// its capacity and not the vector's length: the standard-library body that
    /// calls it has already clamped the range into the logical length, which is
    /// the policy this instruction has none of. A range outside the source, a
    /// negative count and a null source stop the run with nothing allocated —
    /// each is a broken invariant of the lowering, never a program's mistake.
    ///
    /// **A fresh object.** The answer is allocated at exactly `count` units, so
    /// nothing else holds it and a later `freeze` or `toVector` of it is free
    /// to reason as if the copy were the only one — it is.
    ///
    /// **One family on both sides.** For [`Storage::Words`], `src` must be a
    /// [`crate::Shape::Elements`] of the element, fixed or growable — an
    /// `Array` or a `Vector`'s store — and the answer is the *fixed*
    /// [`crate::Shape::Elements`] of it, an `Array<T>`.
    ///
    /// For [`Storage::PackedBytes`], `src` must be a `String` and the answer is
    /// a `String` — `crate::verify` holds `dst`'s layout to
    /// [`crate::Program::str_layout`]. **Nothing about the bytes is validated**,
    /// and not because validation was forgotten: a range of a valid `String`
    /// whose two ends are character boundaries is valid UTF-8, and
    /// `std.string.sliceBytes` refuses every other range before it asks (#378,
    /// Q3). That is a precondition this instruction trusts and does not check —
    /// the boundary test is `String` policy, written once, in Cove — so the only
    /// producer is that body, through a core intrinsic no program can call. A
    /// run under construction is not admitted as a source: nothing slices one,
    /// and its bytes are not text until a finish says so.
    ///
    /// # Why the destination is in the row
    ///
    /// Four operands — `dst`, `src`, `from`, `count` — and two layouts: the
    /// element the storage names and the `Array` the answer is allocated as. A
    /// sixteen-byte instruction has three slot operands and a payload of two
    /// ids, so the operands live behind an [`ArgsId`] as
    /// [`Inst::RunCopy`]'s do, the element is the payload's other half as
    /// `RunCopyWords`' is, and the answer's layout is the one the row already
    /// carries for `dst`: the row holds exactly four [`crate::Arg`]s in the
    /// order `dst`, `src`, `from`, `count`, and `dst`'s layout is the
    /// allocation's. Unlike [`Inst::RunCopy`]'s `dst`, this one is **written**:
    /// it is a frame slot that receives the fresh run's address, which every
    /// pass asking what an instruction writes reads out of the row.
    RunSlice { args: ArgsId, storage: Storage },
    /// `dst = <the first unit offset at or after `from` where `needle` occurs
    /// in `haystack`, or -1>`, in units of `storage`.
    ///
    /// # In ADR 0058's families
    ///
    /// The sixth member of the run family, beside [`Inst::RunLoad`],
    /// [`Inst::RunCopy`], [`Inst::RunSlice`], [`Inst::GrowableAlloc`] and
    /// [`Inst::RunFinish`], added by
    /// [ADR 0065](../../../docs/adr/0065-a-run-search-is-the-one-loop-that-stays-below.md).
    /// Its argument is the family's own rather than a new one: the work is
    /// proportional to the length of a run the caller has not bounded, and
    /// each unit of it is a load and a comparison, so a Cove loop would pay a
    /// VM dispatch per unit where this pays one for the whole. That is the
    /// same purchase [`Inst::RunCopy`] makes for a copy — "one dispatch and one
    /// unit of work per payload word moved" — and it is *not* an argument
    /// about complexity classes: a Cove KMP would be `O(n + m)` too, and would
    /// still turn its loop once a byte.
    ///
    /// **[`Storage::PackedBytes`] only.** `crate::verify` refuses
    /// [`Storage::Words`]. ADR 0058 moved `Array.contains` and
    /// `Array.indexOf` to Cove loops over `==` and they stay there; a sequence
    /// search that wants this instruction gets its own decision, its own
    /// measurement, and those loops as the thing to beat. Stated as a
    /// restriction rather than left implicit because "a bounded run search
    /// over a named storage" sounds general, and defining it over one storage
    /// while calling it general would name a primitive after a capability it
    /// does not have.
    ///
    /// # It searches bytes and knows nothing else
    ///
    /// The comparison is bitwise over units of [`Storage::PackedBytes`]. This
    /// instruction has **no notion of a character, an encoding, a boundary or
    /// a `String`**; it neither validates nor interprets what it reads, and it
    /// would answer the same for a run of arbitrary bytes that never came from
    /// text. Whether a byte match is also a character match is
    /// `std.string.contains`' question and is argued in `std.string`, the way
    /// `std.string.endsWith` argues it for its own offset. Putting that
    /// argument here would be writing `String` policy into an instruction.
    ///
    /// # What it means
    ///
    /// - **The answer is an absolute unit offset into the haystack run**, not
    ///   one relative to `from`. Relative would be a footgun at every call
    ///   site, and `indexOf`, `split` and `replace` all want a position in the
    ///   receiver.
    /// - **Not found is -1.** Not an `Option`: an instruction answers a word,
    ///   and the standard library builds what the public API needs —
    ///   `std.string.contains` a `Bool`, `std.string.indexOf` an `Option<Int>`
    ///   in *character* positions after a walk of its own — which it does, as
    ///   of ADR 0064's fifth Phase 1 migration, so both of those sentences are
    ///   now about code rather than about a plan.
    /// - **An empty needle answers `from`.** The empty needle occurs at every
    ///   position including the end, which is what `str::find("")` answers.
    /// - **A needle longer than `haystack_len - from` answers -1**, and is not
    ///   an error. So does any needle that does not occur.
    /// - **`from` must be in `0 ..= haystack_len`.** `from == haystack_len` is
    ///   legal and answers -1, or `from` for an empty needle. A `from` outside
    ///   that range **stops the run**, as a range outside the source does in
    ///   [`Inst::RunSlice`]: it is a broken invariant of the lowering, never a
    ///   program's mistake, and the standard-library body above it is what
    ///   holds a program's index to the range.
    /// - **A null run stops the run**, as it does in [`Inst::RunSlice`].
    /// - **The two runs may alias.** `s.contains(s)`, a needle that is the
    ///   haystack, and two overlapping runs of one object are all defined: the
    ///   instruction only reads, so there is no order in which a write could
    ///   be seen. Stated because [`Inst::RunCopy`]'s aliasing rules are not
    ///   this one's and a reader will ask.
    /// - **Both operands are fixed runs.** A run under construction is not
    ///   admitted, for the reason [`Inst::RunSlice`] gives.
    ///
    /// # What it is charged, and where it polls
    ///
    /// [`Inst::RunCopy`]'s convention, held to in **both** phases of the
    /// search — which is the part an implementer would not think of unless it
    /// were written down, because the preparation of a needle is `O(m)` work
    /// before the first unit of haystack is read.
    ///
    /// Two answers are reached before any of it and charge no bulk work,
    /// because neither examines a unit: an empty needle, and a needle longer
    /// than `haystack_len - from`. The instruction's own single unit of fuel is
    /// unchanged, so a fast path is one fuel and nothing else — the accounting
    /// `std.string.endsWith`'s length refusal already has.
    ///
    /// Otherwise the needle is prepared and the haystack consumed in steps of
    /// at most `SAFEPOINT_STRIDE` units each, charged as they are consumed and
    /// followed by a safepoint, so **the uninterruptible span is
    /// `SAFEPOINT_STRIDE` units whatever `n` and `m` are and whichever phase
    /// the instruction is in**. That is what
    /// [ADR 0040](../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
    /// `S + T` asks for. A window of `max(SAFEPOINT_STRIDE, m)` would not be
    /// it: a needle of ten million bytes would make one window ten million
    /// units of uninterruptible work.
    ///
    /// Preparing an `m`-unit needle is `m` units of work and is paid for, and
    /// a search that reaches the end of the haystack is `n - from` more, so a
    /// whole scan is charged exactly `m + (n - from)`: preparation is charged
    /// and no unit is charged twice.
    ///
    /// The implementation is therefore a **resumable** matcher, which rules
    /// out calling `str::find` once — it reports neither where it stopped nor
    /// anything to resume from — and it may not be a quadratic scan, because
    /// if the work below the boundary is what a Cove loop would have done
    /// anyway then the family's argument supports nothing.
    ///
    /// # Why four operands live behind an [`ArgsId`]
    ///
    /// [`Inst::RunSlice`]'s reason: a sixteen-byte instruction has three slot
    /// operands and a payload of two ids, and this needs four — `dst`,
    /// `haystack`, `needle`, `from`, in that order, each carrying its layout
    /// the way a call's arguments do. Both runs' lengths come from their
    /// headers, as `RunSlice`'s `src` length does; neither an offset nor a
    /// count is passed for either run. Unlike [`Inst::RunCopy`]'s `dst` and
    /// like [`Inst::RunSlice`]'s, this one is **written** — a frame slot that
    /// receives the answer, which every pass asking what an instruction writes
    /// reads out of the row — but unlike either it is an `Int` and not a
    /// reference, because the answer is an offset and not a run.
    RunFind { args: ArgsId, storage: Storage },
    /// `dst = <a new, empty growable run in `storage` whose store has room for
    /// `capacity` units>`.
    ///
    /// # In ADR 0058's families
    ///
    /// Owner allocation is not one of the six. This is `run-alloc store,
    /// storage, max(capacity, floor)`, then the allocation of a two-word owner
    /// naming that store at logical length zero, as one instruction. It stays
    /// one because between the two allocations the store is reachable from
    /// nothing a collector walks: the runtime holds it with a temporary root for
    /// exactly that window, and the IR gives this instruction one destination,
    /// so there is no slot a split form could keep it in (#378, Q5).
    ///
    /// # For [`Storage::PackedBytes`]
    ///
    /// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
    /// byte buffer, the first of the four instructions that build a byte run
    /// wherever the final length is not known before the writes. A fixed run
    /// is enough when it *is* known; it is not enough for `examples/covefmt`,
    /// whose three hot joins are filled by data-dependent loops and whose
    /// largest is a `var out` parameter passed through recursive calls.
    ///
    /// The owner is [`crate::Program::buffer_layout`], two payload words holding
    /// a logical length and a reference; the store is
    /// [`crate::Program::bytes_layout`], whose *header* length is the capacity.
    /// Neither layout is named here, for [`Inst::Str`]'s reason: both are
    /// program-wide constants, and a call site that always means the same one
    /// should not have to say so.
    ///
    /// `capacity` is a **hint** and not a bound. Exceeding it grows the store
    /// rather than failing, so a tuning estimate cannot change what a program
    /// answers — which is the whole of ADR 0052's "capacity is not an Array
    /// length". A capacity below the runtime's own byte floor is raised to it,
    /// and a negative or oversized one fails through the same "this run has no
    /// memory left" refusal every other allocation does.
    ///
    /// # For [`Storage::Words`]
    ///
    /// `core.vectorWithCapacity`: an empty `Vector<T>` over the storage's
    /// element, which is what every keyed update in `std.map` and `std.set`
    /// builds its answer in — an `inserted`, a `removed`, a `keys` or a
    /// `values`.
    ///
    /// Neither layout is named here either, and for a better reason than the
    /// byte member's: the element *determines* both — the owner is the
    /// program's [`crate::Shape::Vector`] of it, and the store the growable
    /// [`crate::Shape::Elements`] of it — so a second copy of a derivable fact
    /// in the row would be one more thing an encoder, a verifier and two code
    /// generators could come to disagree about, which is the redundancy
    /// [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
    /// spent instructions removing. An allocation is the **only** growable
    /// operation that needs that map read backwards: every other one is handed
    /// an object whose own header already names its layout. The runtime keeps
    /// the reverse direction as a table built once per program and shared by
    /// every task of a run, beside the one it keeps of every layout's width —
    /// it is not a scan of the layout table per allocation.
    ///
    /// `capacity` is a hint here as it is for bytes, and it is raised to the
    /// same floor: four elements, ADR 0052's small floor in the word unit.
    /// Before this instruction admitted words the lowering allocated the store
    /// at exactly the capacity asked for, so a keyed update of one entry
    /// allocated a store of one element and grew it on the second. Capacity is
    /// not readable from Cove — no API answers it — so nothing a program
    /// answers changed; what changed is the allocated-word count of a run full
    /// of small constructions, which rose, and that is the price of a vector
    /// built through the growable path having the growth policy of one.
    GrowableAlloc {
        dst: Slot,
        capacity: Slot,
        storage: Storage,
    },
    /// `growable-ensure owner, additional`: room for `additional` more units in
    /// the store `owner` names, growing it if they would not fit.
    ///
    /// # The first half of a buffer window
    ///
    /// [ADR 0062](../../../docs/adr/0062-an-append-is-ensure-store-commit.md)
    /// split the composite `growable-push` and `growable-extend` into the three
    /// instructions [ADR 0058] always said they were: this, a write into the
    /// room it made — an [`Inst::StoreElem`], an [`Inst::RunStore`] or an
    /// [`Inst::RunCopy`] into the store at the logical length — and an
    /// [`Inst::GrowableCommit`] that publishes the written units. The writes and
    /// the commit are separate instructions so that one protocol serves a push,
    /// a byte, an append and a keyed extend, and so that a policy which is not
    /// the machine's — a range check, a clamp — can be Cove between the ensure
    /// and the write rather than a clause of a composite instruction.
    ///
    /// Soundness is not the machine's to assume. `crate::verify`'s reservation
    /// rule is what makes a window of these safe to run: the write is at the
    /// length read before the ensure, into the store read after it, with no
    /// call, allocation or branch target in between, and a written window is
    /// committed in the same block. See `Check::check_reservations`.
    ///
    /// # What it means
    ///
    /// **Nothing a program can see changes.** The logical length is not
    /// touched, and neither is any unit below it: a growth copies the live
    /// prefix into a larger store and replaces the owner's store word, so a
    /// store read *before* this instruction may name the old one. That is why
    /// the reservation rule reads the store after it.
    ///
    /// **Refusals leave the owner as it was.** A null owner, an owner that is
    /// not a growable run of `storage`, a consumed owner and a negative
    /// `additional` are refused before anything is allocated — a negative room
    /// is a broken invariant of the body that computed it, never a small one. A
    /// room nothing could hold fails through the allocator's "this run has no
    /// memory left".
    ///
    /// **What it costs.** One unit of work, as [`Inst::GrowableAlloc`] is: a
    /// growth is an allocation and the copy of the live prefix that
    /// `runs::growable_ensure` has never charged (issue #378, Q13).
    ///
    /// It writes no frame slot. `owner` is a `Vector<T>` whose element is the
    /// storage's for [`Storage::Words`] and a byte buffer for
    /// [`Storage::PackedBytes`], and `additional` an `Int`.
    ///
    /// [ADR 0058]: ../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    GrowableEnsure {
        owner: Slot,
        additional: Slot,
        storage: Storage,
    },
    /// `growable-commit owner, count`: the logical length advanced over `count`
    /// units the window already wrote.
    ///
    /// The second half of [`Inst::GrowableEnsure`]'s window, and — since ADR
    /// 0062 deleted the composite `growable-push` and `growable-extend` — the
    /// only instruction that raises a length: [ADR 0052]'s rule that a
    /// unit becomes value only once written is what the reservation rule in
    /// `crate::verify` enforces about the instructions before it.
    ///
    /// # A bound the machine still checks
    ///
    /// `0 <= count` and `len + count <= capacity` are checked at run time and a
    /// commit outside them is refused with the length unchanged. The static
    /// rule proves them for a lowered program, but the loader-side bytecode
    /// verifier has no dataflow and cannot, and a length past the capacity is
    /// the one corruption every later read of the owner would believe. What is
    /// *not* checked is that the units were written — that is the static rule's
    /// alone, and the reason a commit is never emitted outside a window.
    ///
    /// It writes no frame slot, and charges one unit of work. `owner` is
    /// [`Inst::GrowableEnsure`]'s, and `count` an `Int`.
    ///
    /// [ADR 0052]: ../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
    GrowableCommit {
        owner: Slot,
        count: Slot,
        storage: Storage,
    },
    /// `run[index] = src`, one unit of a run in `storage`: ADR 0058's
    /// `run-store run, index, src, storage`, and [`Inst::RunLoad`] backwards.
    ///
    /// For [`Storage::PackedBytes`] the unit is a byte: `run` is a
    /// [`crate::Shape::Bytes`] run under construction — a byte buffer's store,
    /// never a `String`, whose bytes are an invariant a finish establishes and
    /// nothing reopens — `index` a byte offset bounded by the run's header
    /// length, which for a store is its capacity, and `src` an `Int` that must
    /// be `0..=255`. Every one of those is checked at run time, because the
    /// value is a number a program computed and a byte outside the range would
    /// be bits of the neighbouring bytes. The byte is blended into its word and
    /// the other seven are left as they were.
    ///
    /// That is the only storage admitted, for [`Inst::RunLoad`]'s reason:
    /// `crate::verify` refuses [`Storage::Words`], whose unit writes are
    /// [`Inst::StoreElem`].
    ///
    /// It is the byte write of [ADR 0062]'s window — `appendByte` and an
    /// interpolation's one-byte literal — and nothing else produces it: a byte
    /// written at an offset below the length would change text a program
    /// already holds a length for, which the reservation rule rules out.
    ///
    /// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    RunStore {
        run: Slot,
        index: Slot,
        src: Slot,
        storage: Storage,
    },
    /// `owner.truncate(len)`: the logical length lowered to `len`, and the
    /// units it vacates cleared.
    ///
    /// # In ADR 0058's families
    ///
    /// Not one of the six: #378's Q5. It is `growable-commit`'s inverse — a
    /// commit raises the length over units the caller has written, and this
    /// lowers it over units the caller has read — and it is what `Vector.pop`
    /// and `Vector.remove` need beneath their Cove bodies, which read the
    /// element with a load, move the tail down with a [`Inst::RunCopy`] of the
    /// store into itself, and then give the last unit back.
    ///
    /// **It is not a negative [`Inst::GrowableCommit`]**, and issue #423 forbids
    /// modelling it as one. The two are inverses in what they do to the length
    /// word and in nothing else. A commit *publishes* storage the window
    /// immediately before it has just initialized, so the units it names are
    /// written by the time it runs and there is nothing for it to clean; a
    /// truncate *un-publishes* storage that was initialized arbitrarily long
    /// ago, and the units it names hold live references until it clears them.
    /// A commit of `-1` would therefore be a length write with no clear, which
    /// is the one outcome this instruction exists to rule out — and it would
    /// have to be refused by the very rule that makes a commit sound, the
    /// reservation rule, which reasons about the window written *above* the
    /// length and has nothing to say about the units below it.
    ///
    /// # What it means
    ///
    /// **The vacated units are zeroed before the length is written.** A store's
    /// shape says its whole capacity is elements, and the collector traces it
    /// that way, so a unit above the length that still held a reference would
    /// keep what it names alive — a vector used as a work queue would retain
    /// everything it had ever held. The store is not replaced and not shrunk:
    /// the room stays for the next push.
    ///
    /// **It only lowers.** A `len` above the current length, or below zero, is
    /// refused as a broken invariant: nothing between the length and the
    /// capacity is a written unit, so raising the length here would expose
    /// zeroes as elements, which is what a window's write and its commit
    /// exist to rule out. Every producer computes `len` from the length it has
    /// just read.
    ///
    /// **What it is charged.** One unit, as [`Inst::GrowableAlloc`] and
    /// [`Inst::RunFinish`] are: the zeroing is one clear of the vacated words,
    /// which nothing could interrupt, and its only producers vacate one
    /// element.
    ///
    /// # For [`Storage::Words`]
    ///
    /// `owner` names a `Vector<T>` whose element layout is the storage's, and
    /// `len` an `Int`. It writes no frame slot. Only [`Storage::Words`] is
    /// admitted: a byte builder has no operation that takes bytes back out.
    ///
    /// # Why this is one instruction and not two
    ///
    /// Issue #423 asked whether the clear and the length write should be two
    /// verified instructions, the way ADR 0062 split an append into an ensure,
    /// a store and a commit. They should not, and the argument is written out
    /// here rather than asserted, because "a composite instruction" is the kind
    /// of thing that reads like something nobody got round to.
    ///
    /// **The interval between the two halves is exactly the unsound state.**
    /// The zeroing happens before the length is written, and neither order
    /// survives being taken apart. A length published first exposes the vacated
    /// words as elements: a collection between the two halves traces a store
    /// whose header says its whole capacity is elements, so it would follow
    /// words the program had just been told were gone, and a `Vector<T>` read
    /// through an alias would answer them. A clear after the publish is a write
    /// *above* the logical length — spare room, which the next
    /// [`Inst::GrowableEnsure`] may hand to a window that has already written
    /// into it and is about to commit. So the only sound arrangement is the one
    /// the single instruction has, and a split form's whole contribution would
    /// be to make the unsound interval nameable: a pc a debugger can stop on, a
    /// safepoint a collection can happen at, a place a later optimizer can put
    /// an instruction between. That is the opposite of what the issue asks for
    /// — "do not expose unfinished buffers", "no uninitialized suffix becomes
    /// visible" — and it is what ADR 0062 says of [`Inst::GrowableAlloc`] and
    /// [`Inst::RunFinish`] too.
    ///
    /// **Nothing would read the pieces.** A split needs a typed clear
    /// instruction over `Storage::Words(elem)` and a second verifier relation
    /// tying that clear to the length write — the reservation rule's mirror
    /// image, and a rule is the most expensive thing this IR can grow, because
    /// every backend has to be held to it. Against that: there is exactly one
    /// producer, `Body::core_vector_truncate` from `core.vectorTruncate`, whose
    /// only callers are `Vector.pop` and `Vector.remove`; the code generator
    /// emits no fast path for it and hands it to the runtime whole; and ADR
    /// 0062's window optimizer has no window here, so there would be nothing
    /// for the new relation to prove anything about. Two instructions and a
    /// rule, read by nobody, is not a simplification.
    ///
    /// **It is not hot, and a split would not be measured as an improvement.**
    /// Measured on fixed inputs for issue #423: covefmt executes it 14,008
    /// times and calls the native helper for it 14,083 times over a run of some
    /// 775 million instructions, and cq executes it **zero** times. There is no
    /// dispatch to save, because the composite form is already one dispatch and
    /// the split form would be two.
    ///
    /// So issue #423's outcome 2 — one composite `Truncate{Storage}` primitive,
    /// justified — rather than outcome 1. Outcome 1's substance is already
    /// true: `runs::growable_truncate` clears the removed word range and
    /// publishes the smaller logical length as one step nothing can observe
    /// between. What outcome 1 would have added is only the form.
    GrowableTruncate {
        owner: Slot,
        len: Slot,
        storage: Storage,
    },
    /// `dst = <owner's live prefix, validated and relabelled to `target`>`,
    /// consuming the owner.
    ///
    /// # In ADR 0058's families
    ///
    /// `run-finish dst, owner, targetLayout, validation` itself, over a
    /// growable owner. How an exact construction — a fixed run with no owner —
    /// becomes a `String` is #378's Q4, and is decided with its first producer.
    ///
    /// What is validated and what is answered is the **live prefix**
    /// `[0, length)`. A store is as long as the last growth made it, and ADR
    /// 0052's "finishing reuses the store" is what happens to the rest: the
    /// store is relabelled to `target` with the *logical* length, and the words
    /// between the two lengths become a free block the next sweep folds back
    /// in. Nothing is copied, which is the same O(1) transition
    /// `Vector.freeze()` already makes for elements.
    ///
    /// The owner is then emptied — length zero, store null — exactly as
    /// `Vector.freeze()` empties a vector, because finishing *consumes*. That
    /// the consumed owner has no second live holder is
    /// [`cove_sema`](../../../crates/cove-sema/src/unique.rs)'s conservative
    /// local uniqueness proof and not something this machine can answer; what
    /// the machine keeps is the liveness check, so an owner used after a finish
    /// is refused rather than read as an empty one.
    ///
    /// # For [`Storage::PackedBytes`]
    ///
    /// ADR 0052's finish for a byte buffer: `crate::verify` requires
    /// [`Validation::Utf8`] and a `target` of [`crate::Program::str_layout`].
    /// The bytes are checked as UTF-8 exactly once, because a run assembled by
    /// [`Inst::RunStore`] may hold anything a byte can hold, and invalid
    /// UTF-8 fails with the same error a source-level string operation already
    /// raises for it. `target` is carried although it is a program-wide
    /// constant, because a word run's finish names an `Array` layout of its
    /// element and the family does not change shape when it does.
    ///
    /// # For [`Storage::Words`]
    ///
    /// `Vector.freeze()`: `std.vector.freeze` is `core.vectorFinish(items)`, and
    /// this is what that lowers to. `crate::verify` requires
    /// [`Validation::None`] — a run of whole elements has nothing to validate —
    /// and a `target` that is the non-growable [`crate::Shape::Elements`] of the
    /// storage's element, because the relabelled store is traced by the target's
    /// reference map from then on.
    ///
    /// Whether the owner had a second holder is not asked here, in either
    /// storage. ADR 0001's "conservative, local uniqueness checking for this
    /// explicit transition" is `cove_sema::unique`'s, at the program's own
    /// `.freeze()` call site, and it is authoritative (#240, #378's Q9).
    ///
    /// [`crate::Shape::Bytes`] cannot cross a Cove call and neither can the
    /// owner cross the Host boundary, but the owner *can* cross a call, which
    /// is the whole reason it is a value rather than a raw run: the formatter's
    /// `fn emit(node: Tree, var out: StringBuilder)` needs to pass a partly
    /// built string down a recursion.
    RunFinish {
        dst: Slot,
        owner: Slot,
        target: LayoutId,
        validation: Validation,
        storage: Storage,
    },
    /// `dst = <obj's header length>`: an element count, or a string's bytes.
    Len { dst: Slot, obj: Slot },
    /// `dst = <the [`LayoutId`] in obj's header>`, as an `Int`.
    ///
    /// The other half of the header word [`Inst::Len`] reads, and it is here
    /// for the same reason: *what an object is* is a question the object
    /// answers at run time, from its own header, and a `Ref` slot carries no
    /// layout of its own.
    ///
    /// It exists because a dispatch has to ask it. A `dyn Trait` value's
    /// implementation is decided by the type behind it, and nothing static
    /// says which that is; the object's header does. Reading it into a slot
    /// turns "which implementation" into an ordinary [`Inst::Switch`] over a
    /// table the lowering builds from the trait's declared conformances,
    /// which is why there is no dispatch instruction — one general question
    /// about an object, answered with the control flow that is already here.
    LayoutOf { dst: Slot, obj: Slot },

    // ---- places ----------------------------------------------------------
    /// `dst = &frame[slot]`
    ///
    /// A place is one word. There is no place object, no place stack and no
    /// table of places; a `var` parameter is an ordinary slot whose
    /// [`Repr`](crate::Repr) is [`Addr`](crate::Repr::Addr).
    AddrOfSlot { dst: Slot, slot: Slot },
    /// `dst = &<payload word `at` of `obj`>`
    ///
    /// The lowering keeps `obj` in a live reference slot for exactly the
    /// address's live range, and clears that slot with [`Inst::Clear`] when
    /// the address dies — not unconditionally for the rest of the frame,
    /// which would retain the object across everything a long-running body
    /// does afterwards. The collector therefore needs no interior-pointer
    /// logic, and the heap does not move, so the address stays correct
    /// across a collection for as long as it is live and no longer.
    AddrOfField { dst: Slot, obj: Slot, at: u32 },
    /// `dst = &obj[index]`, at a stride of `layout`'s width.
    AddrOfElem {
        dst: Slot,
        obj: Slot,
        index: Slot,
        layout: LayoutId,
    },
    /// `dst = addr + at`, a static word offset into the value at `addr`.
    ///
    /// The one place instruction whose operand is itself a place, and what
    /// makes a place composable. A place is the address of the *first* word
    /// of a value location, so without this a `var` parameter could only name
    /// the whole of what it was given: `p.y = 1` through a `var p: Point` had
    /// to load both words, write one and store both back — observationally
    /// the same on one thread, but not what the address was for — and
    /// `f(var p.y)` could not be lowered at all, because there was no way to
    /// form the address to pass.
    ///
    /// `at` is a word offset within the value the address names, computed by
    /// the lowering from the layout the checker settled. It is the same
    /// arithmetic a field of an inline struct is, done to an address instead
    /// of to a slot number, and the answer is again the address of the first
    /// word of a value location — so it goes back through [`Inst::Load`],
    /// [`Inst::Store`] or another of these with no second rule about what an
    /// address is.
    ///
    /// Nothing checks `at` against the value's extent, because a frame does
    /// not record one: what an address names is a fact about the instruction
    /// that formed it, and [`mod@crate::verify`] says the same of
    /// [`Inst::Switch`]'s operand for the same reason.
    AddrOfPart { dst: Slot, addr: Slot, at: u32 },
    /// `dst = *addr`, for the words `layout` describes.
    Load {
        dst: Slot,
        addr: Slot,
        layout: LayoutId,
    },
    /// `*addr = src`, for the words `layout` describes.
    ///
    /// A nested write through a `var` parameter updates the destination words
    /// in place. There is nothing between the address and the words, which is
    /// what a place being an address of the *first word* of a value location
    /// buys.
    Store {
        addr: Slot,
        src: Slot,
        layout: LayoutId,
    },

    // ---- erasure ----------------------------------------------------------
    /// `dst = <a box holding the words of `src`, tagged `layout`>`
    ///
    /// What a value becomes when its static type is not known: `dyn Trait`,
    /// a Host result a schema declared `Any`, an expression the checker
    /// declined to type. One word in the slot either way.
    Box {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },
    /// `dst = <the value inside the box in `src`>`, trapping if its tag is
    /// not `layout`.
    Unbox {
        dst: Slot,
        src: Slot,
        layout: LayoutId,
    },

    // ---- tasks -------------------------------------------------------------
    /// `dst = <a new task scope, open>`
    ///
    /// `scope name { ... }` binds one of these, and everything the Language
    /// Card says about a scope is a fact about the two instructions that
    /// leave it rather than about this one: *concurrent work belongs to a
    /// task scope, and leaving the scope waits for or cancels its child
    /// tasks.*
    ///
    /// `name` is what the source bound it to. It is carried because a
    /// diagnostic quotes it — *task 2 of scope `requests`* — and by the time
    /// a scope is a word there is nothing else left that knows.
    ScopeEnter { dst: Slot, name: StrId },
    /// Leave the scope in `scope` the way the body reached its end: wait for
    /// every child, and say whether one of them failed in a way the
    /// enclosing function has to pass on.
    ///
    /// `failed` is a `Bool`. When it is true, `error` holds the `Err`
    /// payload of the first child whose value was one, at `layout` — and the
    /// lowering wraps it in the enclosing function's own `Err` and returns
    /// it, which is exactly what `?` would have done. A child that *raised*
    /// is not that: a runtime error is not a value, so this instruction
    /// fails with it and the two ways a child can end stay two things.
    ///
    /// A discriminated outcome rather than an instruction carrying control
    /// flow, because where the failure goes is a fact about the function the
    /// scope was written in — which `Err` to build, and what to return — and
    /// the lowering is what holds those.
    ScopeLeave {
        scope: Slot,
        failed: Slot,
        error: Slot,
        layout: LayoutId,
    },
    /// Cancel every child of the scope in `scope` and wait for it to stop.
    ///
    /// What an *early* exit from a scope's body reaches: a `return`, a `?`,
    /// a `break` or a `continue` that leaves it. Leaving a scope waits for
    /// or cancels its children whichever way it is left, so this is an
    /// obligation on every exit path exactly as [`Inst::Clear`] is, and the
    /// lowering emits one per open scope the jump leaves.
    ///
    /// It answers nothing. A scope being left early is already leaving with
    /// something to say, and a child's failure discovered on the way out
    /// would replace it with an unrelated one.
    ScopeCancel { scope: Slot },
    /// `dst = scope.spawn(closure)`, on a thread of its own.
    ///
    /// `answer` is the layout of the value the body produces, and it is here
    /// because the answer needs somewhere to be *before* the thread exists:
    /// the machine allocates an object of that width and records its address
    /// in the scope's table, so the answer is an object in the run's one heap
    /// and a root of this task from the moment it can hold anything. Handing
    /// the words back through the thread instead would leave them in no
    /// store the collector walks for as long as the join took.
    ///
    /// This returns once the thread exists and orders nothing else. ADR
    /// 0008's amendment is explicit that whether the child has run an
    /// instruction by the time the next one here does is the operating
    /// system's answer.
    Spawn {
        dst: Slot,
        scope: Slot,
        closure: Slot,
        answer: LayoutId,
    },
    /// `dst = await task`, for the words `answer` describes.
    ///
    /// Waits for the task's thread and answers the value its body produced.
    /// A body runs at most once and is waited for at most once, so awaiting
    /// the same handle twice answers the same value and repeats no effect.
    Await {
        dst: Slot,
        task: Slot,
        answer: LayoutId,
    },
    /// `task.cancel()`: ask the task to stop at its next safepoint.
    ///
    /// Asking is all it does. Whether the task stopped or had already
    /// finished is known only where something waits for it, which is why
    /// `TaskCancelled` is traced at the join and not here.
    Cancel { task: Slot },
    /// `dst = <a task already settled with the words at `src`>`.
    ///
    /// What a **call to an `async fn`** answers. The body ran at the call
    /// site, on this task's stack, as [`Inst::Call`]; this is the handle the
    /// call hands back, and there is no thread anywhere in it.
    ///
    /// That is the oracle's reading rather than an invention here.
    /// `Interpreter::call_target` runs the body and wraps what it produced in
    /// `crate::task::Task::settled`, and `crate::task::Task::settled`'s own
    /// documentation says why: ADR 0008 gives a thread to `spawn`, which is
    /// where the language says concurrency begins, so nothing may depend on
    /// *when* an `async fn` body ran — only on the value an `await` produces.
    /// A call that is never awaited has still run.
    ///
    /// So this task belongs to no scope and nothing joins it. It is
    /// `position` zero of `crate::task::describe`, which is the case that
    /// spelling exists for: *this task*, with no place in a spawn order to
    /// name. The words are copied into an object of the same shape a
    /// spawned task's answer goes into, because an `await` reads the two the
    /// same way and a second arrangement would be a second thing to get
    /// right.
    Settled {
        dst: Slot,
        src: Slot,
        answer: LayoutId,
    },

    // ---- cells ---------------------------------------------------------------
    /// Take the [`crate::Shape::Shared`] cell in `cell`, waiting for whoever
    /// holds it.
    ///
    /// ADR 0008 makes `lock` the whole of a `Shared`'s access: there is no
    /// `get` and no `set`, so a read-modify-write cannot be written as two
    /// operations that race. What that means here is an ordinary
    /// [`Inst::CallClosure`] between this and [`Inst::SharedUnlock`], with the
    /// address of the cell's value as the closure's argument — the same shape
    /// `map` is lowered to, and for the same reason `docs/LINEAR_VM.md` gives:
    /// **a builtin never calls back into Cove**. A builtin that ran the
    /// closure itself would put a Rust frame under every Cove frame it made.
    ///
    /// So `lock` is *two* instructions rather than one that calls, and what
    /// the second one costs is an obligation: **the release belongs to every
    /// exit path**, exactly as [`Inst::Clear`] and [`Inst::ScopeCancel`] do.
    /// The lowering emits it on the path that finished, and a runtime error —
    /// which is not a jump the lowering can emit — is the machine's to answer,
    /// once, for every cell the task was holding.
    ///
    /// A task that asks for a cell it already holds is refused rather than
    /// made to wait, and that rule is untouched by
    /// [ADR 0037](../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md):
    /// waiting would be waiting for itself, and no collector can answer a live
    /// lock state. What the ADR did remove is the *other* refusal — a closure
    /// that leaves the cell holding a handle to itself is an ordinary
    /// object-graph cycle now, collected when it becomes unreachable, so
    /// nothing here inspects what the closure left.
    SharedLock { cell: Slot },
    /// Give the cell in `cell` back, publishing everything written while it
    /// was held.
    ///
    /// The lock word *is* the publication: it is taken with `Acquire` and
    /// released with `Release`, and every other word of the machine's memory
    /// is relaxed and is allowed to be. Acquiring a cell therefore makes
    /// visible not only its own words but every object the previous holder
    /// allocated and stored into them.
    SharedUnlock { cell: Slot },

    // ---- failure ----------------------------------------------------------
    /// Fail the run with `message`, `rule`, and `help`.
    ///
    /// This is what an exhausted `match` and a failed `Unbox` reach. It is
    /// not a refusal to run the program: the program ran, and this is what
    /// it did.
    ///
    /// A `RuntimeError` is three printed sentences, not one — `message`,
    /// `rule`, and `help` — and each slot holds the address of a heap
    /// `String` (a [`Repr::Ref`](crate::Repr::Ref)), exactly as
    /// [`AssertFailed`](Inst::AssertFailed)'s `message` does. **An empty
    /// string means the sentence is absent**: the runtime prints no `rule:`
    /// or `help:` line for a slot that holds one.
    ///
    /// The three are slots rather than [`StrId`]s for the reason
    /// [`AssertFailed`](Inst::AssertFailed)'s doc gives for its one: a
    /// standard-library body builds the refusal it raises, and the words
    /// quote a value the run computed rather than one the lowering could
    /// have written down as a constant. A `Trap` synthesized by the lowering
    /// itself — whose sentence is a constant — still pays for a slot, by
    /// [`Inst::Str`] loading a precomputed address rather than allocating
    /// one; see [ADR 0045](../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md).
    Trap {
        /// The address of the leading sentence, a `String`. Never empty —
        /// this is the one line a trap always prints.
        message: Slot,
        /// The address of the `rule:` sentence, a `String`, or the address
        /// of an empty `String` when the trap has none.
        rule: Slot,
        /// The address of the `help:` sentence, a `String`, or the address
        /// of an empty `String` when the trap has none.
        help: Slot,
    },

    /// Record that an assertion failed here, carrying the `String` in
    /// `message`.
    ///
    /// The one instruction that writes nothing a program can read. An
    /// assertion is lowered rather than performed — see this crate's
    /// `lower::assertions` — so by the time the failing arm runs, the
    /// `Err(Error("assertion failed: ..."))` is an ordinary value and the
    /// only thing left that the machine knows and the value does not is
    /// *where it was written*. A test runner points at the assertion the way
    /// every other error points at source, and this is how it is told.
    ///
    /// The span is the instruction's own, which is the assertion call's, so
    /// nothing has to be threaded through the program to carry it. The
    /// message is a slot rather than a [`StrId`] because `assertEqual`
    /// renders the two values it compared and that string is built at run
    /// time; a runner compares it against the `Err` it is holding, so that a
    /// later unrelated failure is not reported at this assertion.
    AssertFailed { message: Slot },
}
