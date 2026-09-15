//! What both code generators compile, decided once.
//!
//! Two arms compile Cove IR in this crate — Cranelift under the `cranelift`
//! feature and a hand-written x86-64 template compiler under `template` — and
//! the
//! whole point of having two is to measure one against the other. A
//! measurement over two different subsets of the IR would not be that
//! measurement, so the subset is not written twice: [`supported`] is the one
//! predicate both arms ask, and `leaders` is the one block partition both
//! arms charge work over.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use cove_ir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Function, Inst, Intrinsic, LayoutId, Len, Num,
    Program, Repr, Shape, Slot, StrId,
};
use cove_schema::builtins::{NONE_CASE, SOME_CASE};

use crate::abi::Raise;

/// The widest run of words this slice moves in one instruction.
///
/// An [`Inst::Copy`], an [`Inst::LoadElem`]'s element, an [`Inst::Call`]'s
/// answer, an [`Inst::Load`], an [`Inst::Store`] and an [`Inst::Clear`] are each
/// emitted as a run of loads and a run of stores — see each arm's `copy` for why
/// a copy is in that order — so the code they produce is linear in the width and
/// there is no memmove helper to fall back to yet. A bound is therefore worth
/// having, and it is deliberately generous: sixteen words is a wider inline value
/// than anything the corpus lowers, and a `covefmt.Token` is three.
const MAX_RUN_WORDS: u32 = 16;

/// Whether a slot of this `Repr` is one this slice will touch.
///
/// The scalars, and [`Repr::Ref`] — see [`crate::abi`]'s "References are live
/// here, and the frame is why that is safe". A reference is admitted because
/// the frame is the canonical home of every value at every instruction
/// boundary, so a `Repr::Ref` slot is a root the existing walk already finds,
/// and because the covefmt slice takes a `String` and an `Array` as its
/// parameters: refusing a reference would refuse the measurement.
///
/// [`Repr::Addr`] is admitted, and it is the one entry here that was once a
/// refusal. A `var` parameter arrives as one word holding a linear word index,
/// and the address family that forms and follows one — [`Inst::AddrOfSlot`],
/// [`Inst::AddrOfPart`], [`Inst::Load`], [`Inst::Store`] — is emitted code
/// rather than a runtime call. It is *not* a root, which is why admitting it
/// costs the collector nothing: `Function::refs` names [`Repr::Ref`] and ADR
/// 0034 says why, and see [`crate::abi`]'s "An address names either region".
///
/// [`Host`](Repr::Host), [`Task`](Repr::Task) and [`Scope`](Repr::Scope) stay
/// excluded, and for a reason that has nothing to do with the collector: they
/// are not roots either, but every operation that produces or consumes one is a
/// runtime call this slice does not lower, so a frame holding one is a frame
/// whose function will be refused anyway.
///
/// [`Repr::Float`] is admitted although no float *operation* is lowered. A
/// float slot that is only copied is a run of bits like any other, and
/// refusing the whole function because one of its frame slots is a `Float`
/// would refuse it for a reason that is not true.
fn is_lowered(repr: Repr) -> bool {
    match repr {
        Repr::Unit
        | Repr::Bool
        | Repr::Int
        | Repr::Float
        | Repr::Duration
        | Repr::Tag
        | Repr::Ref
        | Repr::Addr => true,
        Repr::Host | Repr::Task | Repr::Scope => false,
    }
}

/// The byte offset of a slot from the frame's first word, if it fits the
/// `i32` displacement both arms address a frame with.
///
/// A frame is bounded by `cove_ir::MAX_FRAME_WORDS`, which is far inside
/// this, so the `None` is unreachable in practice. It is checked rather than
/// asserted because "unreachable in practice" is a claim about today's
/// constant.
pub(crate) fn slot_offset(slot: Slot) -> Option<i32> {
    i32::try_from(i64::from(slot) * 8).ok()
}

/// The byte offset of a literal's address from the first word of
/// [`NativeCtx::literals`](crate::abi::NativeCtx::literals), if it fits the `i32`
/// displacement both arms address the table with.
///
/// [`slot_offset`] one table over, and the `None` is unreachable for the same
/// kind of reason: a program with 2^28 string literals is one no source file
/// produced. It is checked rather than asserted because "unreachable in practice"
/// is a claim about today's corpus, and an arm that was silently wrong above a
/// threshold is worse than one that refuses at it.
pub(crate) fn literal_offset(text: StrId) -> Option<i32> {
    i64::try_from(text.index())
        .ok()
        .and_then(|at| at.checked_mul(8))
        .and_then(|at| i32::try_from(at).ok())
}

/// Whether a comparison is one this slice lowers.
///
/// [`Compare::Int`] takes all six operators, as
/// `encoded.rs`'s `cmp_int!` does. [`Compare::Bool`] takes equality only,
/// which is the same division `encoded.rs` makes at its `EQ_BOOL`/`NE_BOOL`
/// arms against the `LT_BOOL | LE_BOOL | GT_BOOL | GE_BOOL => not_ordered!()`
/// arm beside them — an ordered comparison of `Bool` is a runtime error, and
/// emitting one would be lowering a refusal. Refusing the function instead
/// leaves it to the tier that already has the message.
///
/// [`Compare::Tag`] takes equality only, and for exactly the same reason: it
/// shares `encoded.rs`'s `EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true)` arm and
/// its `LT_TAG | LE_TAG | GT_TAG | GE_TAG => not_ordered!()` neighbour. It is
/// here because `token.kind != Kind.Punct` is what a formatter asks in every
/// loop it has — see [`cove_ir::Compare::Tag`]'s own note on what walking it
/// instead cost.
///
/// Everything else — [`Compare::Float`], [`Str`](Compare::Str),
/// [`Identity`](Compare::Identity) — is outside the slice. `Identity` would be
/// one integer comparison and is left out because nothing the raced slice does
/// asks it, which is the rule this predicate is widened by.
fn comparison_supported(on: Compare, op: CmpOp) -> bool {
    match on {
        Compare::Int => true,
        Compare::Bool | Compare::Tag => matches!(op, CmpOp::Eq | CmpOp::Ne),
        Compare::Float | Compare::Str | Compare::Identity => false,
    }
}

/// A [`Inst::CallBuiltin`] both arms emit code for, with its operands decoded.
///
/// A builtin is *named* rather than numbered — see [`cove_ir::Builtin`] — so the
/// question "is this one the tier lowers" is a pair of string comparisons over
/// the program's own table, and it is asked **once**, here, rather than in each
/// arm. That is [`supported`]'s rule taken one level down: a family admitted by
/// the subset and not emitted by an arm is a panic, and the only way to keep the
/// two from drifting is for the decision and the operands to come out of the same
/// function.
///
/// The operand checks that belong to the *shape* are here and the ones that
/// belong to the *frame* are in [`inst_refused`], which is the same division
/// every other instruction makes: a receiver that is not one `Repr::Ref` word is
/// not this builtin at all, and a receiver at a slot the frame does not have is
/// this builtin outside a bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// `String.byteLength() -> Int`.
    ///
    /// `vm::builtins::text::byte_length` is `receiver_addr` and then
    /// `machine.object_len(addr)`, which is what [`Inst::Len`] already is: a null
    /// refusal and the header's low half. So this lowers to the *same emitter*
    /// `Inst::Len` uses in both arms rather than to a family of its own — see
    /// each arm's `len_of`.
    ///
    /// One check of `receiver_addr`'s three is **not** emitted, and it is worth
    /// saying which. `receiver_addr` asks `super::is_string(machine, addr)` after
    /// the null test and answers `no_method` for an object that is not a
    /// `Shape::Str`. That question cannot have a different answer here: a
    /// `call-builtin` of `String.byteLength` is emitted by
    /// `cove_ir::lower::methods` only where the checker settled the receiver's
    /// type as `Ty::Str`, so the receiver word is a reference to a `Shape::Str`
    /// object or it is null. It is the same class of guard as the one
    /// [`Inst::AddrOfField`] is refused for — a *lowering bug*, not something a
    /// program can reach — and it is left out for the same reason: naming it
    /// would be a [`Raise`] carrying the message `no_method` builds out of an
    /// operand's rendered value, which is a whole `Value` this crate cannot see.
    /// The null refusal is a program's to reach and is emitted.
    ByteLength { dst: Slot, obj: Slot },
    /// `Vector.push(value)`, the path where the store has room.
    ///
    /// `vm::builtins::seq::vector_push` is four steps: read the receiver, copy the
    /// element's words out, pick a store — the one it has, or a larger one — and
    /// write the element and the new length. **Only the third of those has a cold
    /// half, and that is the whole shape of this.** What is emitted is the push
    /// into spare capacity; a push that has to grow calls
    /// [`BuiltinFn`](crate::abi::BuiltinFn) and the VM does the whole push.
    ///
    /// Growth is where the *allocation* is, so it is not that it was hard — [ADR
    /// 0055]'s allocation helper is right there, and `Inst::Alloc` goes through it.
    /// It is that growth is also a payload *copy of unbounded length*, which is a
    /// loop over `len * stride` heap words, and `capacity` doubles: the copy is
    /// amortised over the pushes that filled the store, so emitting it would be
    /// code proportional to the run in exchange for a share of the work that falls
    /// as the vector grows. The fast path is what the census counted.
    ///
    /// Two other preconditions go the same way, and neither is a lowering bug a
    /// reader may dismiss:
    ///
    /// - **the receiver's object is the layout the call site names.** `vector()`
    ///   reads `Shape::Vector { elem }` out of the object's own header, and this
    ///   compares that header against the layout the argument list declares —
    ///   because everything else here is derived from the declared one: the
    ///   element's layout, its stride, and therefore where the element goes. A
    ///   mismatch is what `operand::no_method` reports, whose message renders the
    ///   receiver as a `Value`;
    /// - **`freeze()` has not consumed it.** `vector()` refuses a store word of
    ///   nought with `operand::frozen`, which a *program* reaches by pushing to a
    ///   frozen vector, and whose message names the method.
    ///
    /// Both messages are the runtime's to build and this crate can build neither,
    /// so both are tested and both go to the helper. The null receiver is the one
    /// refusal that is emitted, because [`Raise::NullObject`] already names it.
    ///
    /// [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
    Push {
        /// Where the `Unit` answer goes: `vector_push` answers `Ok(0)`, so this is
        /// one word of nought and not nothing.
        dst: Slot,
        /// The receiver's slot: one `Repr::Ref` word naming the `Vector` header.
        recv: Slot,
        /// The layout the call site declares the receiver to be, whose shape is a
        /// [`Shape::Vector`] and which the object's own header is compared against.
        vector: LayoutId,
        /// The element's slot in this frame, `stride` words wide.
        value: Slot,
        /// The element layout's width, which is `Growable::stride`.
        stride: u32,
        /// The builtin and its argument list, for the cold path.
        builtin: u32,
        args: u32,
    },
    /// `Vector.set(index, value) -> Option<T>`, the path where the index is
    /// in range.
    ///
    /// `vm::builtins::seq::vector_set` reads the receiver, reads the index,
    /// answers `None` for one outside `[0, len)`, and otherwise reads the
    /// element that was there — **before** overwriting it, because `set`
    /// answers what `get` would have — and hands that back as `Some`. Every
    /// one of those is a static fact of the call site or a comparison this
    /// tier can already do, so unlike [`Method::Push`] there is no cold half
    /// to a range that fits: an in-range `set` and an out-of-range one are
    /// both emitted, and only the receiver's own preconditions go to
    /// [`BuiltinFn`](crate::abi::BuiltinFn).
    ///
    /// Those preconditions are [`Method::Push`]'s two, for the same reasons:
    ///
    /// - **the receiver's object is the layout the call site names**, read out
    ///   of the header the way `vector()` does, because the element's layout,
    ///   its stride and therefore where it sits in the store are all derived
    ///   from the declared one;
    /// - **`freeze()` has not consumed it**, which `vector()` refuses with
    ///   `operand::frozen` — a message this crate cannot build any more than
    ///   `Push`'s could.
    ///
    /// The null receiver is emitted as [`Raise::NullObject`] for the same
    /// reason `Push`'s is: it is the one refusal whose message names no
    /// operand.
    ///
    /// # Building the `Option`
    ///
    /// The answer is not a builtin's ordinary one-word or `stride`-word run:
    /// it is a case of an enum, which is a tag word and a payload region wide
    /// enough for whichever case is written — [`crate::abi`] and
    /// `vm::builtins::make`'s `case_words` agree that constructing a case
    /// *zeroes* the payload words it does not fill, so a stale word from a
    /// wider case never reads through a narrower one. `some_at` and `width`
    /// are read out of the call site's declared `Option<T>` layout by
    /// [`method_of`] rather than assumed to be zero and `1 + stride`: an enum
    /// lays a case's parts into the lowest free run, and a reader that
    /// guessed would be trusting a fact this crate can simply look up.
    #[allow(clippy::too_many_arguments)]
    Set {
        /// Where the `Option<T>` answer goes: a tag word and the payload
        /// region, `width` words wide in all.
        dst: Slot,
        /// The receiver's slot: one `Repr::Ref` word naming the `Vector` header.
        recv: Slot,
        /// The layout the call site declares the receiver to be, whose shape is
        /// a [`Shape::Vector`] and which the object's own header is compared
        /// against.
        vector: LayoutId,
        /// The index argument's slot: one `Repr::Int` word.
        index: Slot,
        /// The value argument's slot, `stride` words wide.
        value: Slot,
        /// The element layout's width, which is `Growable::stride`.
        stride: u32,
        /// The `Option<T>` layout's total width: the tag word plus its whole
        /// payload region, which bounds every word this method writes.
        width: u32,
        /// The tag value a `Some` answer holds.
        some_case: u32,
        /// The word offset of `Some`'s one part within the payload region —
        /// after the tag word, so its `stride` words sit at
        /// `dst + 1 + some_at`.
        some_at: u32,
        /// The tag value a `None` answer holds.
        none_case: u32,
        /// The builtin and its argument list, for the cold path.
        builtin: u32,
        args: u32,
    },
    /// `Vector.freeze() -> Array<T>`.
    ///
    /// `vm::builtins::seq::vector_freeze` is `vector()` and then three writes:
    /// `Memory::relabel` turns the store in place into the `Array` it already
    /// holds — a header write, and a free block for the capacity it gives up —
    /// and the two payload words of the `Vector` header are zeroed, which is
    /// `freeze()`'s mark. `Memory::relabel` is documented as "two heap word
    /// writes and nothing else, no free-list surgery", so all of it is
    /// emitted; there is no cold half the way [`Method::Push`]'s growth is.
    ///
    /// The `Array<T>` layout `relabel` writes is a **compile-time constant**:
    /// `make::elements(program, elem, false)` only searches the program's own
    /// layout table for a [`Shape::Elements`] of `elem` that is not growable,
    /// and that search is exactly as available here as it is to the runtime —
    /// see [`method_of`]'s `Intrinsic::VectorFreeze` arm. A program whose checker
    /// admitted `.freeze()` at all has that layout, so the search failing is
    /// the same class of impossibility [`Inst::AddrOfField`]'s bound is;
    /// refusing the call site rather than asserting keeps that claim untested
    /// by a program this crate cannot see the whole of.
    ///
    /// Two preconditions go to [`BuiltinFn`](crate::abi::BuiltinFn), for
    /// [`Method::Push`]'s reasons exactly: the receiver's object is not the
    /// declared `Shape::Vector` layout, and the store word is already nought —
    /// a *second* `freeze()`, whose message names the method. The null
    /// receiver is emitted as [`Raise::NullObject`].
    Freeze {
        /// Where the answer goes: the store's own linear address, which
        /// `relabel` leaves it at.
        dst: Slot,
        /// The receiver's slot: one `Repr::Ref` word naming the `Vector` header.
        recv: Slot,
        /// The layout the call site declares the receiver to be.
        vector: LayoutId,
        /// The element layout's width, `Growable::stride` — needed to turn the
        /// element count `relabel` is given into the payload words it releases.
        stride: u32,
        /// The `Array<T>` layout `relabel` writes into the store's header.
        array: LayoutId,
        /// The builtin and its argument list, for the cold path.
        builtin: u32,
        args: u32,
    },
}

/// Which [`Method`] a `call-builtin` is, or `None` for one no arm lowers.
///
/// `None` is [`Reason::Instruction`] and not [`Reason::Operands`], which is this
/// module's own division read through one more level: a builtin nothing lowers is
/// a family to write, and the *name* is what says which family it is.
pub(crate) fn method_of(
    program: &Program,
    dst: Slot,
    builtin: BuiltinId,
    args: ArgsId,
) -> Option<Method> {
    let named = program.builtin(builtin);
    let list = program.arg_list(args);
    // The receiver is operand zero, which is `vm::builtins::operand::method`'s
    // own split. One `Repr::Ref` word, because that is what `operand::as_word`
    // requires of it and what an object address is.
    let reference = |at: usize| -> Option<Slot> {
        let arg = list.get(at)?;
        (program.layout(arg.layout).words.as_slice() == [Repr::Ref]).then_some(arg.slot)
    };
    match named.intrinsic {
        Intrinsic::StringByteLength => {
            // The answer is one `Int` word written at `dst`, which is what
            // `Machine::call_builtin` copies out of the builtin's `out` buffer.
            if list.len() != 1 || program.layout(named.result).width() != 1 {
                return None;
            }
            Some(Method::ByteLength {
                dst,
                obj: reference(0)?,
            })
        }
        // `vm::builtins::seq::vector_push`. Everything the fast path needs is a
        // static fact of the call site, and each one is read here rather than in
        // an arm: the receiver's declared layout, whose shape says what the
        // elements are; the element's own layout, which has to be that same one
        // or `operand::run_of` would refuse the call; and the stride, which is
        // that layout's width.
        Intrinsic::VectorPush => {
            // The receiver and one argument, which is `operand::method`'s split
            // and the arity its refusal names.
            if list.len() != 2 || program.layout(named.result).width() != 1 {
                return None;
            }
            let recv = reference(0)?;
            let vector = list[0].layout;
            let Shape::Vector { elem } = program.layout(vector).shape else {
                return None;
            };
            // `operand::run_of(machine, .., items.elem, args[0])` requires the
            // argument's layout to *be* the element's, so a call site where the
            // two differ is one the VM refuses. Refusing to compile it leaves the
            // refusal where its message is.
            if list[1].layout != elem {
                return None;
            }
            Some(Method::Push {
                dst,
                recv,
                vector,
                value: list[1].slot,
                stride: program.layout(elem).width(),
                builtin: builtin.0,
                args: args.0,
            })
        }
        // `vm::builtins::seq::vector_set`. The receiver's declared layout and
        // the element's own layout are read exactly as `Push`'s are, and for
        // the same reason; what is new here is the *answer*'s layout, an
        // `Option<T>` the call site also declares — and its `Some`/`None` tag
        // values and `Some`'s payload offset are read out of it rather than
        // assumed, because [`cove_ir::layout::enum_layout`] places a case's
        // parts wherever they first fit rather than always at word zero.
        Intrinsic::VectorSet => {
            // The receiver, the index and the value — `operand::method`'s
            // split and the arity its refusal names.
            if list.len() != 3 {
                return None;
            }
            let recv = reference(0)?;
            let vector = list[0].layout;
            let Shape::Vector { elem } = program.layout(vector).shape else {
                return None;
            };
            // `operand::int` requires the index to be one `Repr::Int` word.
            if program.layout(list[1].layout).words.as_slice() != [Repr::Int] {
                return None;
            }
            // `operand::run_of(machine, .., items.elem, args[1])` requires the
            // value argument's layout to *be* the element's, so a call site
            // where the two differ is one the VM refuses. Refusing to compile
            // it leaves the refusal where its message is.
            if list[2].layout != elem {
                return None;
            }
            let option = program.layout(named.result);
            let Shape::Enum { cases, .. } = &option.shape else {
                return None;
            };
            let some_case = option.case(SOME_CASE.name)?;
            let none_case = option.case(NONE_CASE.name)?;
            let some_at = cases[some_case as usize].parts.first()?.at;
            Some(Method::Set {
                dst,
                recv,
                vector,
                index: list[1].slot,
                value: list[2].slot,
                stride: program.layout(elem).width(),
                width: option.width(),
                some_case,
                some_at,
                none_case,
                builtin: builtin.0,
                args: args.0,
            })
        }
        // `vm::builtins::seq::vector_freeze`. The receiver's declared layout is
        // read exactly as `Push`'s and `Set`'s is; what is new is the *answer's*
        // layout, which is not declared anywhere on the call site the way
        // `Set`'s `Option<T>` is; it is `make::elements(program, elem, false)`'s
        // own search over the program's layout table, repeated here rather than
        // called, because this crate does not depend on `cove-runtime`.
        Intrinsic::VectorFreeze => {
            // The receiver alone — `operand::method`'s split and the arity its
            // refusal names.
            if list.len() != 1 || program.layout(named.result).width() != 1 {
                return None;
            }
            let recv = reference(0)?;
            let vector = list[0].layout;
            let Shape::Vector { elem } = program.layout(vector).shape else {
                return None;
            };
            let array = program
                .layouts
                .iter()
                .position(|layout| {
                    matches!(layout.shape, Shape::Elements { elem: e, growable } if e == elem && !growable)
                })
                .map(|index| LayoutId(index as u32))?;
            Some(Method::Freeze {
                dst,
                recv,
                vector,
                stride: program.layout(elem).width(),
                array,
                builtin: builtin.0,
                args: args.0,
            })
        }
        _ => None,
    }
}

/// Why a function has no machine code, as one stable reason.
///
/// [ADR 0055] asks a native run to report "one stable refusal reason per refused
/// function", and *stable* is the load-bearing word: the reason is what a reader
/// sorts a table by and decides what to build next from, so it names a **family**
/// rather than an instruction. Two functions refused for `LoadField` and for
/// `AllocFixed` share [`Reason::Instruction`] and are told apart by the
/// instruction [`Refusal::at`] names; two refused because a slot holds a
/// `Repr::Host` share [`Reason::SlotRepr`] and there is no instruction to name.
///
/// The division that matters is between the last two. [`Reason::Instruction`] is
/// an operation nothing lowers — a family to write — and [`Reason::Operands`] is
/// an operation that *is* lowered, refused because a bound it names was
/// exceeded. Those point at different work, and a single "unsupported" would
/// have hidden the difference.
///
/// [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `lower::stub` left a stand-in where a body would be.
    ///
    /// There is nothing to compile, and compiling it would present a run as
    /// more native than it is.
    Stub,
    /// A frame slot holds a representation this tier does not keep in a slot.
    ///
    /// [`Host`](Repr::Host), [`Task`](Repr::Task) and [`Scope`](Repr::Scope);
    /// see this module's `is_lowered`.
    SlotRepr(Repr),
    /// The body does not end in a terminator, so its last block falls off the
    /// end.
    NoTerminator,
    /// An instruction no arm emits code for.
    Instruction,
    /// An instruction both arms lower, whose operands are outside a bound one
    /// of them needs.
    ///
    /// A value wider than this module's `MAX_RUN_WORDS`, a slot past the end of
    /// the frame,
    /// a slot offset an `i32` displacement cannot name, a comparison that is a
    /// runtime error rather than an answer, or a jump table with more cases
    /// than an `i32` immediate can hold.
    Operands,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Stub => write!(f, "the body is a stub"),
            Reason::SlotRepr(repr) => write!(f, "a frame slot holds a `{repr:?}`"),
            Reason::NoTerminator => write!(f, "the body does not end in a terminator"),
            Reason::Instruction => write!(f, "an instruction is not lowered"),
            Reason::Operands => write!(f, "an operand is outside a bound"),
        }
    }
}

/// One refused function's reason, and where the reason was found.
///
/// `at` is the **first** unsupported instruction, which is the other half of
/// what ADR 0055's report asks for. First rather than every one, because a
/// function is refused whole: the second refusal in a body is not work anybody
/// can do next, and a list of them would sort a long function above a hot one.
/// It is `None` when the refusal is not about an instruction at all — a stub, a
/// slot's representation, a missing terminator — because there is no pc to name
/// and a zero would read as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Which family of refusal this is.
    pub reason: Reason,
    /// The pc of the first instruction that could not be lowered.
    pub at: Option<u32>,
}

/// Whether every part of `function` is inside this slice.
///
/// Called before lowering begins, which is what makes lowering infallible.
/// The instruction match here and each arm's `inst` are two halves of one
/// decision and have to agree: a form admitted here and not lowered there is
/// a panic, which is why that arm is `unreachable!` and says so.
///
/// It is [`refusal`] answering `None`, and it stays as the predicate both arms
/// ask because a `bool` is what a code generator needs: *why* a function was
/// refused is a question for the report and not for the emitter.
pub fn supported(program: &Program, function: &Function) -> bool {
    refusal(program, function).is_none()
}

/// The reason a function is refused whole, before any instruction is looked
/// at, or `None` if none of those coarse reasons apply.
///
/// [`refusal`] and [`blockers`] both start here, and have to: the order these
/// three are checked in is the order [`refusal`]'s own documentation promises
/// — stub, then slot representation, then terminator — and a function that
/// fails one of them has no instruction census worth taking, because no
/// single instruction is what would make it compile.
fn whole_function_refusal(function: &Function) -> Option<Refusal> {
    let of = |reason: Reason| Some(Refusal { reason, at: None });
    // A stub is a stand-in for a body the lowering did not lower, so there is
    // nothing to compile: `lower::stub` leaves a `return` of a cleared slot.
    // Compiling it would answer the same thing the encoded tier answers, and
    // it would also present a run as more native than it is.
    if function.stub {
        return of(Reason::Stub);
    }
    if let Some(repr) = function.reprs.iter().copied().find(|r| !is_lowered(*r)) {
        return of(Reason::SlotRepr(repr));
    }
    // The verifier requires it, and the lowering depends on it: a function
    // whose last instruction is not a terminator would fall off the end of
    // its last basic block, and there is nowhere for it to fall to.
    if !matches!(
        function.code.last(),
        Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Trap { .. } | Inst::Switch { .. })
    ) {
        return of(Reason::NoTerminator);
    }
    None
}

/// Why `function` is outside this slice, or `None` if it is inside it.
///
/// The order the checks are made in is the order the reasons are reported in,
/// and it is deliberate: a stub is refused before its slots are read and its
/// slots before its instructions, so the reason a reader is given is the
/// *coarsest* true one. A stub whose slots also hold a `Repr::Host` is reported
/// as a stub, because writing the missing lowering is not what would make it
/// compile.
pub fn refusal(program: &Program, function: &Function) -> Option<Refusal> {
    if let Some(whole) = whole_function_refusal(function) {
        return Some(whole);
    }
    function.code.iter().enumerate().find_map(|(pc, inst)| {
        inst_refused(program, function, inst).map(|reason| Refusal {
            reason,
            at: Some(pc as u32),
        })
    })
}

/// Every instruction-level refusal in `function`, in pc order.
///
/// [`refusal`] answers the question a reader asks first — is this function
/// refused, and where — with the first blocker, because that is what marks a
/// function refused at all and it is cheap to find. It does not answer the
/// question a reader asks next: what would it take to *compile* this
/// function. A function refused at its first `CallBuiltin` may be refused at
/// nine more after it, and lowering the one family that stopped `refusal`
/// would still leave it on the encoded tier — a lowering built from the first
/// blocker alone is a lowering built for a function that still will not
/// compile. `blockers` is what answers that question: one [`Refusal`] per
/// refused instruction, so the caller can see the whole set a function is
/// waiting on and group it.
///
/// For the whole-function reasons — [`Reason::Stub`], [`Reason::SlotRepr`],
/// [`Reason::NoTerminator`] — there is no instruction census to take, because
/// none of them are about one instruction: this answers the single
/// [`Refusal`] [`refusal`] would have, and nothing more. A supported function
/// answers an empty vector.
pub fn blockers(program: &Program, function: &Function) -> Vec<Refusal> {
    if let Some(whole) = whole_function_refusal(function) {
        return vec![whole];
    }
    function
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, inst)| {
            inst_refused(program, function, inst).map(|reason| Refusal {
                reason,
                at: Some(pc as u32),
            })
        })
        .collect()
}

/// Why one instruction is outside the slice, or `None` if it is inside it.
///
/// Every arm answers [`Reason::Operands`] and the fallback answers
/// [`Reason::Instruction`], which is the whole of the division: an arm exists
/// because both code generators emit that form, so reaching one and failing it
/// is a bound and never a missing family.
fn inst_refused(program: &Program, function: &Function, inst: &Inst) -> Option<Reason> {
    let slots = function.reprs.len();
    let end = function.code.len() as u32;
    let slot = |at: Slot| (at as usize) < slots && slot_offset(at).is_some();
    let run = |at: Slot, width: u32| {
        at.checked_add(width)
            .is_some_and(|last| (last as usize) <= slots)
            && slot_offset(at.saturating_add(width)).is_some()
    };
    // `true` is "this instruction is inside the slice", so that each arm below
    // reads the way it read while it was a predicate.
    let inside = match inst {
        // `encoded.rs`'s `CONST_UNIT` arm, which is `set_word_at(base + dst, 0)`
        // and nothing else — one store of a zero word, the same shape
        // `Inst::Bool` is with the constant already chosen.
        //
        // It was outside the slice until [ADR 0052]'s four came in, and it was
        // outside for a defensible reason: the adoption gate's list does not name
        // it and nothing the raced corpus ran reached one. Lowering the buffer
        // family is what made it matter, and made it matter *a lot* — the first
        // blocker of `std.stringbuilder.StringBuilder.append` and
        // `appendSlice` became this, because both of them answer `Unit`, and
        // that is 443,126 dynamic calls of the covefmt corpus behind a single
        // word write. The philosophy's "earn complexity through use" is what
        // admits it: a representative program showed the friction, twice.
        //
        // [ADR 0052]: ../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
        Inst::Unit { dst } => slot(*dst),
        Inst::Bool { dst, .. } | Inst::Int { dst, .. } => slot(*dst),
        // A case index is one word and the word is a compile-time constant, so
        // this is `encoded.rs`'s `FUNC_REF | CONST_TAG` arm: the same store
        // `CONST_INT` makes, of a number the layout already fixed. The layout
        // and the case are bounded by `cove_ir::verify` before this is reached.
        Inst::Tag { dst, .. } => slot(*dst),
        // `encoded.rs`'s `STR` arm: `set_slot(base, dst, literal_addr(text))`, a
        // read of the table [ADR 0045] placed before the run's first instruction.
        // There is no branch, no allocation and nothing that can fail — a
        // placement failure is refused before a frame exists — so both arms emit
        // the table read and the store and nothing else.
        //
        // The `StrId` is bounded against the program's own table as well as
        // against the `i32` displacement, because `Refusal` is the honest answer
        // for an id no program has: `cove_ir::verify` already refuses one, and an
        // arm that read past the table would be reading whatever followed it.
        //
        // [ADR 0045]: ../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md
        Inst::Str { dst, text } => {
            text.index() < program.strings.len() && literal_offset(*text).is_some() && slot(*dst)
        }
        Inst::Copy { dst, src, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*dst, layout.width())
                && run(*src, layout.width())
        }
        // `encoded.rs`'s `CLEAR` arm, which is `clear_words(base + slot, width)`
        // — a run of zero words over a value location that is always a frame
        // slot, so there is no region to decide. It is bounded like a copy
        // because it is one: `MAX_RUN_WORDS` is a bound on the code a single
        // instruction expands to, and a clear expands to one store per word.
        Inst::Clear { slot: at, layout } => {
            let width = program.layout(*layout).width();
            width <= MAX_RUN_WORDS && run(*at, width)
        }
        Inst::Not { dst, a } => slot(*dst) && slot(*a),
        // ---- places ---------------------------------------------------------
        //
        // Six of the eight, and the two that are missing are missing on purpose.
        //
        // [`Inst::LoadField`] and [`Inst::StoreField`] refuse through
        // `Machine::checked`, whose bound is dynamic — `Layout::payload_words`
        // reads the object's own runtime header — but `Layout::fixed_payload_words`
        // answers that bound at compile time for every shape the census reaches:
        // `NativeCtx::fixed_payload_words` is a table of it, one `u32` per
        // `LayoutId` with `0` standing in for "ask the runtime". Both arms read the
        // object's layout out of its header, look the bound up in one load, and
        // take the fast path if the field fits; a `0` entry — a variable-payload
        // shape such as `Any` — always fails that comparison and falls to
        // [`crate::abi::FieldLoadFn`]/[`FieldStoreFn`], which perform the whole
        // access through `Machine::checked` itself. So the bound is emitted without
        // a new `Raise` ever naming a `LayoutId`, and the runtime's own message is
        // what a program would see if the lowering were ever wrong about `at`.
        //
        // [`Inst::AddrOfField`] is a different question — it does not read a field,
        // it forms the *address* of one, and that address has to be sound whatever
        // it is later used with. Emitting the same table lookup for it would refuse
        // whole and cheaply, but nothing in the corpus this slice is widened by
        // forms one, so it stays out. See the philosophy's "Earn complexity through
        // use".
        //
        // [`Inst::AddrOfElem`] would be cheap for the same reason `LoadElem`'s own
        // bound is — its refusal is `Raise::IndexOutOfRange`, which `load_elem`
        // already emits — and it is still left out because neither it nor
        // `AddrOfField` occurs once in that corpus.
        Inst::AddrOfSlot { dst, slot: at } => slot(*dst) && slot(*at),
        // `at` has to fit an `i32`, and that is the *template* arm's bound rather
        // than a bound on the language: it adds the offset with `add r64, imm32`.
        // A word offset within one value location cannot approach it — a layout is
        // bounded by the frame it fits in — so this refuses nothing real, and an
        // arm that was silently wrong above a threshold is worse than one that
        // refuses at it.
        Inst::AddrOfPart { dst, addr, at } => {
            i32::try_from(*at).is_ok() && slot(*dst) && slot(*addr)
        }
        Inst::Load { dst, addr, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*dst, layout.width())
                && slot(*addr)
        }
        Inst::Store { addr, src, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*src, layout.width())
                && slot(*addr)
        }
        // `encoded.rs`'s `LOAD_FIELD` arm. `layout` here is the *field's own*
        // value-type layout — used only for its width, exactly as `Load`'s and
        // `Copy`'s are — and is not the object's; the object's own layout is a
        // run-time fact `NativeCtx::fixed_payload_words` or
        // `crate::abi::FieldLoadFn` reads out of its header, and neither is
        // bounded here because neither can be wrong: the table's sentinel `0`
        // sends every layout it does not answer for to the helper. `at` has to
        // fit an `i32` once `width` is added to it, for `Inst::AddrOfPart`'s
        // reason — the *template* arm forms each payload word's address with
        // `add r64, imm32`.
        Inst::LoadField {
            dst,
            obj,
            at,
            layout,
        } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && at
                    .checked_add(layout.width())
                    .and_then(|last| i32::try_from(last).ok())
                    .is_some()
                && run(*dst, layout.width())
                && slot(*obj)
        }
        // `encoded.rs`'s `STORE_FIELD` arm: [`Inst::LoadField`] backwards, bounded
        // the same way and for the same reason.
        Inst::StoreField {
            obj,
            at,
            src,
            layout,
        } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && at
                    .checked_add(layout.width())
                    .and_then(|last| i32::try_from(last).ok())
                    .is_some()
                && run(*src, layout.width())
                && slot(*obj)
        }
        // `encoded.rs`'s `NEG_INT` arm, which is `checked_neg` and nothing else.
        //
        // `Num::Float` is not here and falls to `Reason::Instruction`, the same
        // division `Inst::Arith` above makes: no float *operation* is lowered, and
        // `NEG_FLOAT` cannot raise at all, so the two arms are not one arm with a
        // flag.
        Inst::Neg {
            num: Num::Int,
            dst,
            a,
        } => slot(*dst) && slot(*a),
        Inst::Arith {
            num: Num::Int,
            dst,
            a,
            b,
            ..
        } => slot(*dst) && slot(*a) && slot(*b),
        Inst::ArithImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::Cmp { on, op, dst, a, b } => {
            comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b)
        }
        Inst::CmpImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::CmpBranch {
            on,
            op,
            dst,
            a,
            b,
            target,
        } => comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b) && *target < end,
        Inst::CmpImmBranch { dst, a, target, .. } => slot(*dst) && slot(*a) && *target < end,
        Inst::Jump { to } => *to < end,
        Inst::BranchFalse { cond, to } => slot(*cond) && *to < end,
        // Every target and the default, because the machine does not take the
        // lowering's word for the index it reads out of a slot: `encoded.rs`
        // takes `targets.get(index).unwrap_or(&default)`, so the default is as
        // reachable as any target and is bounded with them.
        Inst::Switch { on, table } => {
            let table = program.table(*table);
            // The case count has to fit an `i32`, and that is a bound the
            // *template* arm needs rather than a bound on the language: its
            // compare chain tests `cmp r64, imm32`, whose immediate is
            // sign-extended, so a case index above `i32::MAX` would be compared
            // against a negative number. No enum has two billion cases and
            // `cove_ir::lower` could not build one, so this refuses nothing real —
            // but an arm that was silently wrong above a threshold is worse than
            // one that refuses at it, and the two arms share this predicate so
            // neither may admit what the other cannot lower.
            i32::try_from(table.targets.len()).is_ok()
                && slot(*on)
                && table.default < end
                && table.targets.iter().all(|target| *target < end)
        }
        Inst::Len { dst, obj } => slot(*dst) && slot(*obj),
        // The stride is the element layout's width and the destination is that
        // many words wide, so an `Array<Token>` writes three slots and an
        // `Array<Int>` one. The element's own words have to be ones this slice
        // can hold in a frame, which is `Inst::Copy`'s rule for the same
        // reason: what arrives is a run of words and they land in slots.
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*dst, layout.width())
                && slot(*obj)
                && slot(*index)
        }
        // `encoded.rs`'s `STORE_ELEM` arm, which is [`Inst::LoadElem`] backwards:
        // the same `Machine::element` — the same null refusal and the same
        // `Raise::IndexOutOfRange` — and then a run of words the other way. It is
        // bounded exactly as `LoadElem` is, because it expands to the same code.
        //
        // It is here because **it is what makes an allocation reachable**: an array
        // literal is an `Inst::Alloc` followed by one of these per element, so a
        // function that built one was refused for this however well the allocation
        // itself lowered. `Inst::StoreField` is the same story for a struct and is
        // *not* here — see this module's note on `Inst::AddrOfField`, whose
        // `Machine::checked` refusal names a layout by name and payload width.
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_RUN_WORDS
                && layout.words.iter().copied().all(is_lowered)
                && run(*src, layout.width())
                && slot(*obj)
                && slot(*index)
        }
        Inst::ByteAt { dst, obj, at } => slot(*dst) && slot(*obj) && slot(*at),
        // A call is admitted whatever the callee is: it is handed to
        // `NativeHelpers::call`, which opens the frame with the runtime's own
        // `open_frame` and runs the callee on whichever tier it is on. So the
        // callee's *body* is not this function's business and is not examined —
        // only that the callee exists, that the arguments name slots this frame
        // has, and that the answer fits where it is going.
        Inst::Call { dst, callee, args } => {
            let answer = program.layout(program.function(*callee).returns).width();
            callee.index() < program.functions.len()
                && answer <= MAX_RUN_WORDS
                && run(*dst, answer)
                && program.arg_list(*args).iter().all(|arg| {
                    let width = program.layout(arg.layout).width();
                    width <= MAX_RUN_WORDS && run(arg.slot, width)
                })
        }
        Inst::Return { src } => run(*src, program.layout(function.returns).width()),
        Inst::Trap { .. } => true,
        // ---- [ADR 0052]'s growable buffer -----------------------------------
        //
        // All four, handed to [`BufferFn`](crate::abi::BufferFn) whole, and that
        // helper's own documentation is where the decision for each of them is
        // written down — including why none of them has an emitted fast path and
        // why `AppendByte` is here although the census never named it.
        //
        // They are admitted together. Three of them without the fourth would be a
        // subset that could allocate a builder and not finish it, and the first
        // function that appended one byte would be refused with no work behind the
        // refusal.
        //
        // What is bounded here is what the helper will *read out of this frame*,
        // which is [`Inst::Call`]'s rule: the helper resolves its operands through
        // `Memory::slot`, so a slot this frame does not have is a read past the
        // end of it. Nothing about the capacity, the byte value or the range is
        // bounded, because every one of those is a number a running program
        // computed and each has a refusal of its own whose sentence the runtime
        // builds — a second rejection here would be a second message for one rule.
        //
        // [ADR 0052]: ../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
        Inst::AllocBuffer { dst, capacity } => slot(*dst) && slot(*capacity),
        Inst::AppendByte { buffer, value } => slot(*buffer) && slot(*value),
        // Four operands behind an `ArgsId` — `buffer`, `src`, `from`, `to` — which
        // is the row `cove_ir::verify` already holds to that shape and width. Each
        // is one word, so each is bounded as a slot rather than as a run.
        Inst::AppendBytes { args } => {
            let list = program.arg_list(*args);
            list.len() == 4
                && list
                    .iter()
                    .all(|arg| program.layout(arg.layout).width() == 1 && slot(arg.slot))
        }
        Inst::FinishBuffer { dst, buffer } => slot(*dst) && slot(*buffer),
        // A builtin is decoded by [`method_of`] and by nothing here, so that the
        // name this tier lowers is written down once. `None` is a family nothing
        // emits and falls to `Reason::Instruction` with every other unlowered
        // instruction; a family that *is* emitted is bounded like any other.
        Inst::CallBuiltin { dst, builtin, args } => {
            match method_of(program, *dst, *builtin, *args) {
                Some(Method::ByteLength { dst, obj }) => slot(dst) && slot(obj),
                // The element is a run of `stride` words of this frame, so it is
                // bounded the way an `Inst::Copy`'s source is — and by
                // `MAX_RUN_WORDS` too, because the emitted write is one store per
                // word of it.
                Some(Method::Push {
                    dst,
                    recv,
                    vector,
                    value,
                    stride,
                    ..
                }) => {
                    // The layout id has to fit an `i32`, and that is the
                    // *template* arm's bound rather than a bound on the language:
                    // it tests the header's high half with `cmp r64, imm32`, whose
                    // immediate is sign-extended, so an id above `i32::MAX` would
                    // be compared against a negative number. No program has two
                    // billion layouts, so this refuses nothing real — but an arm
                    // that was silently wrong above a threshold is worse than one
                    // that refuses at it.
                    i32::try_from(vector.0).is_ok()
                        && stride <= MAX_RUN_WORDS
                        && slot(dst)
                        && slot(recv)
                        && run(value, stride)
                }
                // The answer is a tag word and a `width - 1`-word payload
                // region, so `dst` is bounded as a run of `width` words the
                // way `Push`'s `value` is bounded as a run of `stride` — and
                // `width` is checked directly rather than derived from
                // `stride`, because the two agree for `Option<T>` but nothing
                // here should assume it.
                Some(Method::Set {
                    dst,
                    recv,
                    vector,
                    index,
                    value,
                    stride,
                    width,
                    ..
                }) => {
                    // See `Push`'s note on the same bound: the *template* arm
                    // tests the header's high half with `cmp r64, imm32`.
                    i32::try_from(vector.0).is_ok()
                        && stride <= MAX_RUN_WORDS
                        && width <= MAX_RUN_WORDS
                        && run(dst, width)
                        && slot(recv)
                        && slot(index)
                        && run(value, stride)
                }
                // No `run` bound the way `Push`'s and `Set`'s is: the relabel
                // this emits is one header write and (at most) one free-block
                // write, whatever `stride` is — there is no per-element loop
                // for a width to bound.
                Some(Method::Freeze {
                    dst,
                    recv,
                    vector,
                    array,
                    ..
                }) => {
                    // See `Push`'s note on the same bound for `vector`, and the
                    // same reason again for `array`: the *template* arm tests
                    // each against the header's high half with `cmp r64, imm32`.
                    i32::try_from(vector.0).is_ok()
                        && i32::try_from(array.0).is_ok()
                        && slot(dst)
                        && slot(recv)
                }
                None => return Some(Reason::Instruction),
            }
        }
        // `encoded.rs`'s `ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT` arm, which is
        // `Machine::allocate` and a store of the address it answered. The helper
        // is handed the layout and the length whole — see
        // [`AllocFn`](crate::abi::AllocFn) — so there is nothing about the
        // *layout* to bound here: a length no header could hold and a payload no
        // `u32` could count are that function's to refuse, through the one
        // "this run has no memory left" every other allocation fails with.
        Inst::Alloc { dst, len, .. } => match len {
            Len::Fixed | Len::Count(_) => slot(*dst),
            Len::Slot(at) => slot(*dst) && slot(*at),
        },
        _ => return Some(Reason::Instruction),
    };
    (!inside).then_some(Reason::Operands)
}

/// Where a basic block begins, and how many instructions it holds.
///
/// A block is ADR 0055's unit of work accounting and safepoint placement, so
/// this is not only a code-generation convenience: the static instruction
/// count of a block is the charge added to the work accumulator when the
/// block is entered.
///
/// A leader is the first instruction, any branch or jump target, and the
/// instruction after any terminator. That last clause is what makes a
/// conditional branch's fall-through a block of its own, which Cranelift
/// needs because its `brif` names both successors explicitly.
pub(crate) fn leaders(program: &Program, function: &Function) -> Vec<Option<u32>> {
    let end = function.code.len();
    let mut leader = vec![false; end];
    if end > 0 {
        leader[0] = true;
    }
    fn mark(leader: &mut [bool], at: usize) {
        if at < leader.len() {
            leader[at] = true;
        }
    }
    for (pc, inst) in function.code.iter().enumerate() {
        match inst {
            Inst::Jump { to } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::BranchFalse { to, .. } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                mark(&mut leader, *target as usize);
                mark(&mut leader, pc + 1);
            }
            // Every case and the default, and then the fall-through — which a
            // `switch` has none of, but marking `pc + 1` is what makes the
            // instruction after a terminator a block whether anything jumps to
            // it or not, and a `switch` is a terminator.
            Inst::Switch { table, .. } => {
                let table = program.table(*table);
                for target in table.targets.iter().chain(std::iter::once(&table.default)) {
                    mark(&mut leader, *target as usize);
                }
                mark(&mut leader, pc + 1);
            }
            Inst::Return { .. } | Inst::Trap { .. } => mark(&mut leader, pc + 1),
            _ => {}
        }
    }
    // Rewritten as "how long is the block starting here", counting forward to
    // the next leader, so the work charge is one lookup at block entry.
    let mut lengths = vec![None; end];
    let mut at = end;
    for pc in (0..end).rev() {
        if leader[pc] {
            lengths[pc] = Some((at - pc) as u32);
            at = pc;
        }
    }
    lengths
}

/// Which overflow `int_arith` would name for `op` writing to `dst`.
///
/// `int_arith`'s `named` closure answers "duration arithmetic" instead of the
/// operation's own name when the destination is a [`Repr::Duration`] — the
/// question `encoded.rs` asks as `machine.repr(id, a!()) ==
/// Some(Repr::Duration)`. Here the destination's `Repr` is a static fact, so
/// the question is asked once, at compile time, and the answer is a constant in
/// the generated code.
///
/// It is asked only of addition, subtraction and multiplication, because those
/// are the three arms of `int_arith` that call `named`. Division and remainder
/// name themselves whatever the destination is.
///
/// Shared by both arms for the same reason [`supported`] is: this is a rule of
/// the *language*, and two code generators disagreeing about it would be two
/// different languages.
pub(crate) fn overflow_of(function: &Function, op: ArithOp, dst: Slot) -> Raise {
    let duration = function.reprs.get(dst as usize) == Some(&Repr::Duration);
    match op {
        ArithOp::Add if duration => Raise::DurationOverflowed,
        ArithOp::Sub if duration => Raise::DurationOverflowed,
        ArithOp::Mul if duration => Raise::DurationOverflowed,
        ArithOp::Add => Raise::AddOverflowed,
        ArithOp::Sub => Raise::SubOverflowed,
        ArithOp::Mul => Raise::MulOverflowed,
        ArithOp::Div => Raise::DivOverflowed,
        ArithOp::Rem => Raise::RemOverflowed,
    }
}

/// Which "divided by zero" a division-shaped operation names.
///
/// `int_arith`'s `Div` and `Rem` arms test the divisor before they divide, and
/// they name the operation they are: `divided_by_zero("division")` and
/// `divided_by_zero("remainder")`.
pub(crate) fn by_zero_of(op: ArithOp) -> Raise {
    match op {
        ArithOp::Rem => Raise::RemainderByZero,
        _ => Raise::DividedByZero,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_ir::{enum_layout, Arg, Builtin, Layout, RefMap};
    use std::sync::Arc;

    fn span() -> cove_diag::Span {
        cove_diag::Span::new(cove_diag::FileId(0), 0, 0)
    }

    fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span(); code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program(function: Function) -> Program {
        Program {
            functions: vec![function],
            layouts: vec![
                Layout::free(),
                Layout::word("Unit", Repr::Unit),
                Layout::word("Ref", Repr::Ref),
            ],
            ..Program::default()
        }
    }

    /// **`blockers` answers every refused instruction; `refusal` answers only
    /// the first.**
    ///
    /// `AddrOfField` and `AddrOfElem` are both left outside the slice on
    /// purpose — see this module's note on `Inst::AddrOfField` — so a body
    /// that reaches one of each and then a second `AddrOfField` is refused at
    /// three separate pcs. `refusal` is the first of them, because that is
    /// what marks the function refused at all; `blockers` is all three,
    /// because that is what says whether lowering one family would be enough.
    #[test]
    fn blockers_answers_every_refusal_and_refusal_answers_the_first() {
        let reprs = vec![Repr::Ref, Repr::Unit];
        let code = vec![
            Inst::AddrOfField {
                dst: 0,
                obj: 0,
                at: 0,
            },
            Inst::AddrOfElem {
                dst: 0,
                obj: 0,
                index: 0,
                layout: LayoutId(2),
            },
            Inst::AddrOfField {
                dst: 0,
                obj: 0,
                at: 1,
            },
            Inst::Return { src: 1 },
        ];
        let function = function(reprs, LayoutId(1), code);
        let program = program(function);
        let function = program.function(cove_ir::FunctionId(0));

        assert_eq!(
            refusal(&program, function),
            Some(Refusal {
                reason: Reason::Instruction,
                at: Some(0),
            })
        );
        assert_eq!(
            blockers(&program, function),
            vec![
                Refusal {
                    reason: Reason::Instruction,
                    at: Some(0),
                },
                Refusal {
                    reason: Reason::Instruction,
                    at: Some(1),
                },
                Refusal {
                    reason: Reason::Instruction,
                    at: Some(2),
                },
            ]
        );
    }

    /// A stub refuses the whole function, so there is no instruction census
    /// to take: both `refusal` and `blockers` answer the one reason, and
    /// `blockers` does not walk a body that was never lowered.
    #[test]
    fn a_stub_gives_exactly_one_entry_from_both() {
        let mut function = function(vec![], LayoutId(1), Vec::new());
        function.stub = true;
        let program = program(function);
        let function = program.function(cove_ir::FunctionId(0));

        let expected = Refusal {
            reason: Reason::Stub,
            at: None,
        };
        assert_eq!(refusal(&program, function), Some(expected));
        assert_eq!(blockers(&program, function), vec![expected]);
    }

    // --- `method_of`'s `Intrinsic::VectorSet` arm ------------------------------

    const SET_INT: LayoutId = LayoutId(1);
    const SET_REF: LayoutId = LayoutId(2);
    const SET_VECTOR: LayoutId = LayoutId(3);
    const SET_OPTION: LayoutId = LayoutId(4);

    /// A layout table wide enough for `Vector.set(Int, Int) -> Option<Int>`:
    /// `Int`, `Ref`, `Vector<Int>` and `Option<Int>` — with one `Builtin` at
    /// `BuiltinId(0)` naming `Vector.set` and `args` at `ArgsId(0)` and
    /// `ArgsId(1)` for the empty list and the one this module's tests build.
    ///
    /// `Option<Int>`'s `Some`/`None` case order is not the checker's — this
    /// table is built by hand and puts `Some` first — and that is deliberate:
    /// it is what proves `method_of` reads the tag values and the payload
    /// offset out of the layout rather than assuming `Some` is index 0 or
    /// that its one part sits at word 0, the way `cove_ir::enum_layout`
    /// itself does not promise either.
    fn program_with_set(function: Function, args: Vec<Arg>) -> Program {
        let mut layouts = vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::word("Ref", Repr::Ref),
            Layout::object("Vector", Shape::Vector { elem: SET_INT }),
        ];
        let (cases, payload) = enum_layout(
            &[
                (Arc::from("Some"), vec![SET_INT]),
                (Arc::from("None"), vec![]),
            ],
            &layouts,
        );
        let mut words = vec![Repr::Tag];
        words.extend_from_slice(&payload);
        layouts.push(Layout::inline(
            "Option",
            Shape::Enum { cases, payload },
            words,
        ));
        Program {
            functions: vec![function],
            layouts,
            args: vec![Vec::new(), args],
            builtins: vec![Builtin {
                intrinsic: Intrinsic::VectorSet,
                result: SET_OPTION,
            }],
            ..Program::default()
        }
    }

    /// The receiver, the index and a value of the element's own layout —
    /// `method_of`'s admitted shape — answers a [`Method::Set`] whose case
    /// tags and payload offset are the ones [`program_with_set`] built,
    /// rather than the `0`/`1 + 0` a reader might have assumed.
    #[test]
    fn vector_set_is_admitted_with_the_right_arity_and_layouts() {
        let function = function(vec![Repr::Ref, Repr::Int, Repr::Int], SET_OPTION, vec![]);
        let program = program_with_set(
            function,
            vec![
                Arg {
                    slot: 0,
                    layout: SET_VECTOR,
                },
                Arg {
                    slot: 1,
                    layout: SET_INT,
                },
                Arg {
                    slot: 2,
                    layout: SET_INT,
                },
            ],
        );
        let method = method_of(&program, 3, BuiltinId(0), ArgsId(1));
        assert_eq!(
            method,
            Some(Method::Set {
                dst: 3,
                recv: 0,
                vector: SET_VECTOR,
                index: 1,
                value: 2,
                stride: 1,
                width: 2,
                some_case: 0,
                some_at: 0,
                none_case: 1,
                builtin: 0,
                args: 1,
            })
        );
    }

    /// `operand::method`'s split asks for the receiver and exactly two
    /// arguments; a call site with only one refuses, because `method_of`'s own
    /// note is that a call site the VM would refuse for arity is one this
    /// tier leaves refused rather than compiling into a read of a slot that
    /// is not there.
    #[test]
    fn vector_set_refuses_the_wrong_arity() {
        let function = function(vec![Repr::Ref, Repr::Int], SET_OPTION, vec![]);
        let program = program_with_set(
            function,
            vec![
                Arg {
                    slot: 0,
                    layout: SET_VECTOR,
                },
                Arg {
                    slot: 1,
                    layout: SET_INT,
                },
            ],
        );
        assert_eq!(method_of(&program, 2, BuiltinId(0), ArgsId(1)), None);
    }

    /// `operand::run_of(machine, .., items.elem, args[1])` requires the value
    /// argument's layout to *be* the element's, so a call site where they
    /// differ is one the VM refuses — `Vector<Int>.set(_, someRef)`, here.
    /// Refusing to compile it leaves the refusal where its message is, rather
    /// than emitting a write `operand::run_of` would never have allowed.
    #[test]
    fn vector_set_refuses_a_value_layout_that_is_not_the_element() {
        let function = function(vec![Repr::Ref, Repr::Int, Repr::Ref], SET_OPTION, vec![]);
        let program = program_with_set(
            function,
            vec![
                Arg {
                    slot: 0,
                    layout: SET_VECTOR,
                },
                Arg {
                    slot: 1,
                    layout: SET_INT,
                },
                Arg {
                    slot: 2,
                    layout: SET_REF,
                },
            ],
        );
        assert_eq!(method_of(&program, 3, BuiltinId(0), ArgsId(1)), None);
    }

    // --- `method_of`'s `Intrinsic::VectorFreeze` arm ---------------------------

    const FREEZE_INT: LayoutId = LayoutId(1);
    const FREEZE_REF: LayoutId = LayoutId(2);
    const FREEZE_VECTOR: LayoutId = LayoutId(3);
    const FREEZE_ARRAY: LayoutId = LayoutId(4);

    /// A layout table for `Vector<Int>.freeze() -> Ref`, with `Array<Int>` at
    /// [`FREEZE_ARRAY`] only when `with_array` says so — the one precondition
    /// this arm cannot read off the call site itself and has to find by
    /// searching the program's own table, exactly as `make::elements` does.
    fn program_with_freeze(function: Function, args: Vec<Arg>, with_array: bool) -> Program {
        let mut layouts = vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::word("Ref", Repr::Ref),
            Layout::object("Vector", Shape::Vector { elem: FREEZE_INT }),
        ];
        if with_array {
            layouts.push(Layout::object(
                "Array",
                Shape::Elements {
                    elem: FREEZE_INT,
                    growable: false,
                },
            ));
        }
        Program {
            functions: vec![function],
            layouts,
            args: vec![Vec::new(), args],
            builtins: vec![Builtin {
                intrinsic: Intrinsic::VectorFreeze,
                result: FREEZE_REF,
            }],
            ..Program::default()
        }
    }

    /// The receiver alone — `method_of`'s admitted shape — answers a
    /// [`Method::Freeze`] whose `array` is [`FREEZE_ARRAY`], found by the
    /// search and not assumed to be the next id after the vector's.
    #[test]
    fn vector_freeze_is_admitted_with_the_layout_the_table_has() {
        let function = function(vec![Repr::Ref, Repr::Ref], FREEZE_REF, vec![]);
        let program = program_with_freeze(
            function,
            vec![Arg {
                slot: 0,
                layout: FREEZE_VECTOR,
            }],
            true,
        );
        let method = method_of(&program, 1, BuiltinId(0), ArgsId(1));
        assert_eq!(
            method,
            Some(Method::Freeze {
                dst: 1,
                recv: 0,
                vector: FREEZE_VECTOR,
                stride: 1,
                array: FREEZE_ARRAY,
                builtin: 0,
                args: 1,
            })
        );
    }

    /// `operand::method`'s split asks for the receiver and no argument; a call
    /// site with one more refuses, for [`vector_set_refuses_the_wrong_arity`]'s
    /// reason.
    #[test]
    fn vector_freeze_refuses_the_wrong_arity() {
        let function = function(vec![Repr::Ref, Repr::Ref, Repr::Ref], FREEZE_REF, vec![]);
        let program = program_with_freeze(
            function,
            vec![
                Arg {
                    slot: 0,
                    layout: FREEZE_VECTOR,
                },
                Arg {
                    slot: 1,
                    layout: FREEZE_VECTOR,
                },
            ],
            true,
        );
        assert_eq!(method_of(&program, 2, BuiltinId(0), ArgsId(1)), None);
    }

    /// `make::elements(program, elem, false)`'s own search, repeated here
    /// rather than called: a program whose layout table has no non-growable
    /// `Elements` of the vector's element is refused rather than assumed to
    /// have one. A program the checker built for a real `.freeze()` always
    /// has it; this is the same defensive `?` every other lookup in this arm
    /// already is.
    #[test]
    fn vector_freeze_refuses_a_program_with_no_array_layout() {
        let function = function(vec![Repr::Ref, Repr::Ref], FREEZE_REF, vec![]);
        let program = program_with_freeze(
            function,
            vec![Arg {
                slot: 0,
                layout: FREEZE_VECTOR,
            }],
            false,
        );
        assert_eq!(method_of(&program, 1, BuiltinId(0), ArgsId(1)), None);
    }
}
