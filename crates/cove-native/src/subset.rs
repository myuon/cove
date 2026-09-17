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
    ArithOp, CmpOp, Compare, Convert, Function, Inst, LayoutId, Len, Num, Program, Repr, Shape,
    Slot, Storage, StrId,
};

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
/// ADR 0059's three-way [`CmpOp::Order`] is in the slice over the same three,
/// because `encoded.rs`'s `ORDER_INT | ORDER_BOOL | ORDER_TAG` arm is one
/// signed comparison of the two words for all of them. A `String`'s order is
/// in the slice too, and only its order: `ORDER_STR` walks two objects' bytes,
/// which both arms hand to [`OrderStrFn`](crate::abi::OrderStrFn), a leaf helper
/// that cannot allocate, raise or move anything — so a standard-library search
/// over `String` keys compiles (#378, Q4.14). `Str` equality and the ordered
/// comparisons `cmp_str!` answers copy both strings out and stay outside.
///
/// Everything else — [`Compare::Float`], `Str` but for its order,
/// [`Identity`](Compare::Identity) — is outside the slice. `Identity` would be
/// one integer comparison and is left out because nothing the raced slice does
/// asks it, which is the rule this predicate is widened by.
fn comparison_supported(on: Compare, op: CmpOp) -> bool {
    match on {
        Compare::Int => true,
        Compare::Bool | Compare::Tag => matches!(op, CmpOp::Eq | CmpOp::Ne | CmpOp::Order),
        Compare::Str => op == CmpOp::Order,
        Compare::Float | Compare::Identity => false,
    }
}

/// An [`Inst::GrowablePush`] over [`Storage::Words`] — `Vector.push` — with the
/// static facts its fast path is emitted from.
///
/// [ADR 0058] moved `Vector.push` into the standard library as
/// `core.vectorPush(items, value)`, which lowers to this instruction, so this is
/// no longer a builtin this crate recognises by name: it is a run instruction,
/// decoded here once for both arms.
///
/// `Machine::push_words` is three steps: read the owner, ensure room for one
/// more element, and write the element's words and the new length. **Only the
/// second of those has a cold half, and that is the whole shape of this.** What
/// is emitted is the push into spare capacity; a push that has to grow calls
/// [`GrowableFn`](crate::abi::GrowableFn) with
/// [`GrowableOp::PushWords`](crate::abi::GrowableOp::PushWords) and the VM does
/// the whole push.
///
/// Growth is where the *allocation* is, so it is not that it was hard — [ADR
/// 0055]'s allocation helper is right there, and `Inst::Alloc` goes through it.
/// It is that growth is also a payload *copy of unbounded length*, which is a
/// loop over `len * stride` heap words, and `capacity` doubles: the copy is
/// amortised over the pushes that filled the store, so emitting it would be code
/// proportional to the run in exchange for a share of the work that falls as the
/// vector grows. The fast path is what the census counted.
///
/// Two other preconditions go the same way, and neither is a lowering bug a
/// reader may dismiss:
///
/// - **the owner's object is the vector the element layout implies.**
///   `Machine::vector_run` reads `Shape::Vector { elem }` out of the object's own
///   header and holds it to the instruction's element layout — because the
///   stride, and therefore where the element goes, is derived from that layout.
///   The layout table interns one `Shape::Vector` per element layout, so the
///   header is compared with [`WordPush::vector`], found by searching the table
///   the way `make::elements` searches it for an `Array`;
/// - **`freeze()` has not consumed it.** `Machine::vector_run` refuses a store
///   word of nought, in a sentence this crate cannot build.
///
/// Both refusals are the runtime's to word, so both go to the helper. The null
/// owner is the one refusal that is emitted, because [`Raise::NullObject`]
/// already names it.
///
/// There is no destination: the `()` a push answers is a separate
/// [`Inst::Unit`] the lowering writes after it.
///
/// [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
/// [ADR 0058]: ../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WordPush {
    /// The owner's slot: one `Repr::Ref` word naming the `Vector` header.
    pub(crate) owner: Slot,
    /// The `Shape::Vector` layout over the instruction's element layout, which
    /// the object's own header is compared against.
    pub(crate) vector: LayoutId,
    /// The head of the element's run in this frame, `stride` words wide.
    pub(crate) src: Slot,
    /// The element layout's width, which is `Growable::stride`.
    pub(crate) stride: u32,
}

/// The [`WordPush`] a word `growable-push` of `elem` is, or `None` for a program
/// whose layout table has no vector of `elem` — which a lowering that pushed
/// onto one always declared, so `None` is a bound and not a family.
pub(crate) fn word_push(
    program: &Program,
    owner: Slot,
    src: Slot,
    elem: LayoutId,
) -> Option<WordPush> {
    if elem.index() >= program.layouts.len() {
        return None;
    }
    let vector = program
        .layouts
        .iter()
        .position(|layout| matches!(layout.shape, Shape::Vector { elem: e } if e == elem))
        .map(|index| LayoutId(index as u32))?;
    Some(WordPush {
        owner,
        vector,
        src,
        stride: program.layout(elem).width(),
    })
}

/// An [`Inst::GrowablePush`] over [`Storage::PackedBytes`] — `appendByte`, and
/// the one-byte literal run of an interpolation — with the static facts its fast
/// path is emitted from.
///
/// `Machine::append_byte` is [`WordPush`]'s three steps over a packed run: read
/// the owner, ensure room for one more byte, and write the byte and the new
/// length. What is emitted is the push into spare capacity, and everything else
/// is [`GrowableFn`](crate::abi::GrowableFn) with
/// [`GrowableOp::Push`](crate::abi::GrowableOp::Push), which performs the whole
/// push again from the start.
///
/// The helper's documentation gives ADR 0052's reasons why none of its four
/// operations had an emitted half: an allocation's second root, an extend's
/// chunked safepoint, and a finish's UTF-8 walk. **A push into spare capacity
/// meets none of them.** It allocates nothing, so there is no store a collector
/// could miss and nothing to collect; it moves one byte, so there is no bulk work
/// to charge in chunks; and it validates nothing, because a run is validated
/// when it is finished. So the fast path takes no safepoint and publishes no
/// work, exactly as [`WordPush`]'s does: the accumulator is charged at the next
/// safepoint, and the cold half publishes it as every hand-over does.
///
/// The preconditions that go to the helper are `Machine::buffer`'s and
/// `append_byte`'s own, and each has a sentence the runtime builds:
///
/// - **the owner's object is a byte buffer.** Its header's layout is compared
///   with [`BytePush::buffer`], the program's one `Shape::ByteBuffer` layout;
/// - **`finish()` has not consumed it**, which leaves a store word of nought;
/// - **the length is below the capacity.** The owner's payload word 0 is the
///   length in bytes, and the store's header length is its capacity in bytes —
///   eight to a payload word, least significant first, which is how
///   `RUN_LOAD_BYTES` reads the same run. The comparison is of the whole length
///   word, unsigned, so a length past the capacity is cold and the runtime
///   refuses it in its own words;
/// - **the value is a byte**, `0` to `255`, compared unsigned so that a negative
///   `Int` is cold too, where `appendByte`'s refusal is worded.
///
/// The null owner is the one refusal that is emitted, as [`Raise::NullObject`],
/// which is what `Machine::buffer` answers for it.
///
/// [ADR 0052]: ../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BytePush {
    /// The owner's slot: one `Repr::Ref` word naming the `ByteBuffer` header.
    pub(crate) owner: Slot,
    /// The `Shape::ByteBuffer` layout the object's own header is compared against.
    pub(crate) buffer: LayoutId,
    /// The slot holding the byte, one `Int` word.
    pub(crate) src: Slot,
}

/// The [`BytePush`] a byte `growable-push` is, or `None` for a program whose
/// `buffer_layout` is not a `Shape::ByteBuffer` — which a lowering that pushed a
/// byte always declared, so `None` is a bound and not a family.
pub(crate) fn byte_push(program: &Program, owner: Slot, src: Slot) -> Option<BytePush> {
    let buffer = program.buffer_layout;
    matches!(
        program.layouts.get(buffer.index())?.shape,
        Shape::ByteBuffer
    )
    .then_some(BytePush { owner, buffer, src })
}

/// An [`Inst::GrowableEnsure`] or an [`Inst::GrowableCommit`] — [ADR 0062]'s
/// window — with the static facts its emitted test is made from.
///
/// Both read the owner the way [`WordPush`] and [`BytePush`] do: the header is
/// compared with the one owner layout the storage implies — the program's
/// `Shape::Vector` of the element, or its `Shape::ByteBuffer` — the store word
/// with nought, and the length word with the store's capacity. What differs is
/// the question asked of the count:
///
/// - **an ensure** asks whether `count <= capacity - length`, unsigned, and does
///   nothing else when it holds: there is room, so there is nothing to grow. A
///   negative count read unsigned is past any room, so it is cold, and the
///   runtime's refusal is the one that words it. So is a length past the
///   capacity, which is tested first so that the subtraction cannot wrap;
/// - **a commit** asks the same question and, when it holds, writes
///   `length + count` into the owner. When it does not, the commit is refused by
///   the runtime, whose check the loader-side verifier's lack of dataflow is
///   the reason for.
///
/// Every cold path is [`GrowableFn`](crate::abi::GrowableFn) with the
/// operation's [`GrowableOp`](crate::abi::GrowableOp), which runs the whole
/// instruction again from the start and rejoins. The null owner is emitted, as
/// [`Raise::NullObject`].
///
/// Neither takes a safepoint on its fast path, for [`BytePush`]'s reason: it
/// allocates nothing and moves nothing.
///
/// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reserve {
    /// The owner's slot: one `Repr::Ref` word.
    pub(crate) owner: Slot,
    /// The count's slot: one `Int` word.
    pub(crate) count: Slot,
    /// The owner layout the object's own header is compared against.
    pub(crate) layout: LayoutId,
    /// Which storage, so that each arm picks the cold operation.
    pub(crate) words: bool,
}

/// The [`Reserve`] an ensure or a commit of `storage` is, or `None` for a
/// program with no owner layout of that storage — which a lowering that reserved
/// room in one always declared, so `None` is a bound and not a family.
pub(crate) fn reserve(
    program: &Program,
    owner: Slot,
    count: Slot,
    storage: Storage,
) -> Option<Reserve> {
    let (layout, words) = match storage {
        Storage::PackedBytes => (byte_push(program, owner, count)?.buffer, false),
        Storage::Words(elem) => (word_push(program, owner, count, elem)?.vector, true),
    };
    Some(Reserve {
        owner,
        count,
        layout,
        words,
    })
}

/// An [`Inst::RunStore`] over [`Storage::PackedBytes`] — [ADR 0062]'s byte
/// write — with the static facts its emitted blend is made from.
///
/// `RUN_STORE_BYTES` is `RUN_LOAD_BYTES` backwards, with two more questions: the
/// object is a byte run under construction, whose header is compared with the
/// program's one `Shape::Bytes` layout, and the value is a byte, compared
/// unsigned so a negative `Int` is past `255`. The offset is compared unsigned
/// with the header length, which is `RUN_LOAD_BYTES`' comparison. Each refusal's
/// sentence is the runtime's, so each goes to
/// [`GrowableFn`](crate::abi::GrowableFn) as
/// [`GrowableOp::StoreBytes`](crate::abi::GrowableOp::StoreBytes), which runs the
/// whole store again; the null run is emitted, as [`Raise::NullObject`].
///
/// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ByteStore {
    pub(crate) run: Slot,
    pub(crate) index: Slot,
    pub(crate) src: Slot,
    /// The `Shape::Bytes` layout the object's own header is compared against.
    pub(crate) bytes: LayoutId,
}

/// The [`ByteStore`] a byte `run-store` is, or `None` for a program whose
/// `bytes_layout` is not a `Shape::Bytes`.
pub(crate) fn byte_store(
    program: &Program,
    run: Slot,
    index: Slot,
    src: Slot,
) -> Option<ByteStore> {
    let bytes = program.bytes_layout;
    matches!(program.layouts.get(bytes.index())?.shape, Shape::Bytes).then_some(ByteStore {
        run,
        index,
        src,
        bytes,
    })
}

/// A push or an append window — [ADR 0062]'s ensure, write and commit, as
/// `cove_ir::legalize` recognises it — with the static facts both arms emit it
/// from as **one fast path**.
///
/// The rows of a window are each admitted on their own, and each has an emitted
/// test of its own: [`Reserve`] twice, and [`ByteStore`] or a `store-elem`. Run
/// row by row, a push reads the owner's header three times and asks the room
/// question twice. What [`windows`] decodes is the one fast path the composite
/// push had — [`WordPush`]'s and [`BytePush`]'s, whose questions these are — so
/// that both arms can emit a window the way they emitted `GrowablePush`, and
/// make the frame writes the rows would have made besides. **The window is not a
/// second definition of the shape**: [`cove_ir::legalize::recognize`] is asked,
/// over this crate's own block partition, and nothing here matches a row.
///
/// Admission is the rows'. [`supported`] never looks at a window, so a function
/// whose rows form one compiles exactly when the same rows unrecognised would,
/// and `None` from [`windows`] for a head is not a refusal: the rows are emitted
/// one at a time, as they would have been.
///
/// # The cold path is the rows
///
/// What is emitted in front of the write is the composite push's precondition
/// list, and nothing past it: the owner is not null — refused at the head, as its
/// `load-field` refuses it — its header is [`BufferWindow::layout`], its store is
/// live, and there is room for the count. The frame writes the rows before the
/// ensure make are made first, and every failure goes to a cold half that is the
/// rows from where it failed:
///
/// - **a header of another family** is the head's `load-field` handed to the
///   runtime's field helper whole — which answers any object exactly — then any
///   constant before the ensure, and then the cold ensure below. The ensure is
///   what refuses such an owner, in its own words at its own span;
/// - **no room, or a consumed store**, is the ensure handed to
///   [`GrowableFn`](crate::abi::GrowableFn) as `EnsureBytes` or `EnsureWords`,
///   which grows, refuses or stops. When it returns, the owner and the length are
///   read again and the store read and the room question are **asked again**:
///   the rejoin is at the store row, not past the room test. A returned ensure
///   leaves room, so the second question is answered yes; the test is there so
///   that nothing written after it depends on the runtime having said so. Every
///   turn through it is a call of a helper that takes a safepoint;
/// - **a byte push's store of another family, or a value that is not a byte**,
///   is the store written to its slot and the `run-store` handed over as
///   [`GrowableOp::StoreBytes`](crate::abi::GrowableOp::StoreBytes), which
///   refuses it at the write's span — after the ensure has grown, as the rows
///   grow first.
///
/// So every refusal is a row's refusal, raised at that row's pc with that row's
/// operands, and no helper ever does a whole push.
///
/// An append's write is [`RunCopyFn`](crate::abi::RunCopyFn) whole, as a
/// `run-copy` row's is, and its commit is the commit row — the copy is a
/// safepoint, so nothing the room test knew is known after it.
///
/// # The work is the rows'
///
/// Nothing here charges. A window never spans a block boundary — a row after the
/// head that some branch lands on is not a window — so the block it is in charges
/// its static instruction count at entry, rows included, as it always did.
///
/// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BufferWindow {
    /// The rows, their operands and the frame writes they make.
    pub(crate) window: cove_ir::legalize::Window,
    /// The owner layout the object's own header is compared against:
    /// [`Reserve::layout`].
    pub(crate) layout: LayoutId,
    /// For a byte push, the `Shape::Bytes` layout the store's header is
    /// compared against — [`ByteStore::bytes`]. Unread otherwise.
    pub(crate) bytes: LayoutId,
    /// Whether the storage is words, so that each arm picks the cold ensure and
    /// masks the length word as [`WordPush`] does.
    pub(crate) words: bool,
    /// An append's `run-copy` argument row. Unread for a push.
    pub(crate) args: u32,
}

/// Every window of `function` that both arms emit as one fast path, indexed by
/// its head's program counter.
///
/// `lengths` is [`leaders`] of the same function: a window is recognised over
/// this crate's own blocks, which are the partition work is charged by. The scan
/// is `cove_ir::legalize::windows`' own, from the top, so no pc is inside two.
pub(crate) fn windows(
    program: &Program,
    function: &Function,
    lengths: &[Option<u32>],
) -> Vec<Option<BufferWindow>> {
    let code = &function.code;
    let starts: Vec<bool> = lengths.iter().map(Option::is_some).collect();
    let mut found = vec![None; code.len()];
    let mut pc = 0;
    while pc < code.len() {
        match cove_ir::legalize::recognize(program, code, &starts, pc) {
            Some(window) => {
                found[pc] = buffer_window(program, code, window);
                pc += window.rows;
            }
            None => pc += 1,
        }
    }
    found
}

/// The facts a recognised window is emitted from, or `None` where a row's own
/// decoder has none — which [`supported`] has already refused, so `None` is a
/// bound and not a family.
fn buffer_window(
    program: &Program,
    code: &[Inst],
    window: cove_ir::legalize::Window,
) -> Option<BufferWindow> {
    use cove_ir::legalize::Pattern;
    let reserve = reserve(program, window.owner, window.count, window.storage)?;
    let bytes = match window.pattern {
        Pattern::PushByte => byte_store(program, window.store, window.at, window.src)?.bytes,
        _ => program.bytes_layout,
    };
    let args = match code.get(window.write)? {
        Inst::RunCopy { args, .. } => args.0,
        _ => 0,
    };
    Some(BufferWindow {
        window,
        layout: reserve.layout,
        bytes,
        words: reserve.words,
        args,
    })
}

/// An [`Inst::RunFinish`] over [`Storage::Words`] — `Vector.freeze()` — with
/// the static facts its relabel is emitted from.
///
/// `std.vector.freeze` is `core.vectorFinish(items)`, which lowers to this
/// instruction. `Machine::finish_words` is the owner's checks and then
/// `growable_finish`'s three writes: `Memory::relabel` turns the store in place
/// into the `Array` it already holds — a header write, and a free block for the
/// capacity it gives up — and the two payload words of the `Vector` header are
/// zeroed, which is the consumed mark. `Memory::relabel` is documented as "two
/// heap word writes and nothing else, no free-list surgery", so all of it is
/// emitted; there is no cold half the way [`WordPush`]'s growth is.
///
/// The `Array<T>` layout `relabel` writes is the instruction's own `target`,
/// which `cove_ir::verify` holds to the fixed `Elements` of the element — it is
/// no longer searched for, as a builtin's answer had to be.
///
/// Two preconditions go to [`GrowableFn`](crate::abi::GrowableFn) as
/// [`GrowableOp::FinishWords`](crate::abi::GrowableOp::FinishWords), for
/// [`WordPush`]'s reasons exactly: the owner's object is not the vector the
/// element layout implies, and the store word is already nought. The null owner
/// is emitted as [`Raise::NullObject`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WordFinish {
    /// Where the answer goes: the store's own linear address, which `relabel`
    /// leaves it at.
    pub(crate) dst: Slot,
    /// The owner's slot: one `Repr::Ref` word naming the `Vector` header.
    pub(crate) owner: Slot,
    /// The `Shape::Vector` layout over the element, which the object's own
    /// header is compared against.
    pub(crate) vector: LayoutId,
    /// The element layout's width, `Growable::stride` — needed to turn the
    /// element count `relabel` is given into the payload words it releases.
    pub(crate) stride: u32,
    /// The `Array<T>` layout `relabel` writes into the store's header.
    pub(crate) array: LayoutId,
}

/// The [`WordFinish`] a word `run-finish` of `elem` into `target` is, or `None`
/// for a program whose table has no vector of `elem` or whose `target` is not
/// the fixed run of it — neither of which a verified lowering produces, so
/// `None` is a bound and not a family.
pub(crate) fn word_finish(
    program: &Program,
    dst: Slot,
    owner: Slot,
    target: LayoutId,
    elem: LayoutId,
) -> Option<WordFinish> {
    let push = word_push(program, owner, 0, elem)?;
    // The fixed run of the element, or — a keyed finish, #378 P4-5 — the `Set`
    // of it or the `Map` whose entry it is. The emitted relabel is the same
    // header write and free block for all three: each is `len` units of
    // `stride` words under the same reference map. The sorted-and-distinct
    // assertion `Machine::finish_words` makes in the runtime's own tests is not
    // emitted.
    let fixed = program.layouts.get(target.index()).is_some_and(|layout| {
        matches!(layout.shape, Shape::Elements { elem: e, growable: false } if e == elem)
            || cove_ir::finishes_as_keyed_run_of(&program.layouts, &layout.shape, elem)
    });
    fixed.then_some(WordFinish {
        dst,
        owner,
        vector: push.vector,
        stride: push.stride,
        array: target,
    })
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
/// function. A function refused at its first `IntrinsicCall` may be refused at
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
        // `encoded.rs`'s `INT_TO_FLOAT`, `DURATION_TO_INT` and `INT_TO_DURATION`
        // arms: one word in, one word out, nothing that can fail. `Int.toFloat`
        // and both halves of `Duration.nanos` lower to these since ADR 0058's
        // Phase 5 (#378, P5-2) took them off the intrinsic boundary, and a
        // function this admits would otherwise have been refused at the call.
        //
        // `FloatToInt` is not here and falls to `Reason::Instruction`: no
        // lowering emits it, and its saturating truncation is not the one
        // `cvttsd2si` answers out of range.
        Inst::Convert {
            to: Convert::IntToFloat | Convert::DurationToInt | Convert::IntToDuration,
            dst,
            a,
        } => slot(*dst) && slot(*a),
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
        } => {
            *op != CmpOp::Order
                && comparison_supported(*on, *op)
                && slot(*dst)
                && slot(*a)
                && slot(*b)
                && *target < end
        }
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
        // A byte `run-load`, and only that: the word member has no opcode and
        // no arm, and `cove_ir::verify` refuses it before it could get here.
        Inst::RunLoad {
            dst,
            run,
            index,
            storage: Storage::PackedBytes,
        } => slot(*dst) && slot(*run) && slot(*index),
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
        // Three of the four handed to [`GrowableFn`](crate::abi::GrowableFn) whole,
        // and that helper's own documentation is where the decision for each of
        // them is written down. The fourth, a byte push, is an emitted push into
        // spare capacity with that helper as its cold half: see [`BytePush`] for
        // why none of the helper's reasons reaches it.
        //
        // Each is admitted over `Storage::PackedBytes`, and the helper is the byte
        // buffer's. The one word member that exists, a push, is admitted in an arm
        // of its own below, because it is an emitted fast path and not this
        // helper whole; the rest fall to the `_` with every other unlowered
        // instruction, which `cove_ir::verify` refuses before they get here.
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
        Inst::GrowableAlloc {
            dst,
            capacity,
            storage: Storage::PackedBytes,
        } => slot(*dst) && slot(*capacity),
        // A byte push is an emitted fast path with this helper as its cold half —
        // see [`BytePush`] — so it is admitted where that can be decoded. The
        // buffer's layout id has to fit an `i32`, for the word push's reason
        // below: the template arm compares a header's high half with an `imm32`.
        Inst::GrowablePush {
            owner,
            src,
            storage: Storage::PackedBytes,
        } => byte_push(program, *owner, *src).is_some_and(|push| {
            i32::try_from(push.buffer.0).is_ok() && slot(push.owner) && slot(push.src)
        }),
        // `Vector.push`, which is not the byte buffer's helper whole but an
        // emitted fast path with [`WordPush`]'s cold half. The element is a run of
        // `stride` words of this frame, so it is bounded the way an
        // `Inst::Copy`'s source is — and by `MAX_RUN_WORDS` too, because the
        // emitted write is one store per word of it.
        //
        // The vector's layout id has to fit an `i32`, and that is the *template*
        // arm's bound rather than a bound on the language: it tests the header's
        // high half with `cmp r64, imm32`, whose immediate is sign-extended, so an
        // id above `i32::MAX` would be compared against a negative number. No
        // program has two billion layouts, so this refuses nothing real — but an
        // arm that was silently wrong above a threshold is worse than one that
        // refuses at it.
        Inst::GrowablePush {
            owner,
            src,
            storage: Storage::Words(elem),
        } => word_push(program, *owner, *src, *elem).is_some_and(|push| {
            i32::try_from(push.vector.0).is_ok()
                && push.stride <= MAX_RUN_WORDS
                && slot(push.owner)
                && run(push.src, push.stride)
        }),
        // Four operands behind an `ArgsId` — `owner`, `src`, `from`, `to` — which
        // is the row `cove_ir::verify` already holds to that shape and width. Each
        // is one word, so each is bounded as a slot rather than as a run.
        Inst::GrowableExtend {
            args,
            storage: Storage::PackedBytes,
        } => {
            let list = program.arg_list(*args);
            list.len() == 4
                && list
                    .iter()
                    .all(|arg| program.layout(arg.layout).width() == 1 && slot(arg.slot))
        }
        Inst::RunFinish {
            dst,
            owner,
            storage: Storage::PackedBytes,
            ..
        } => slot(*dst) && slot(*owner),
        // `Vector.freeze()`: an emitted relabel with [`WordFinish`]'s cold half,
        // not the byte buffer's helper whole. No `run` bound the way a word
        // push's is: the relabel is one header write and (at most) one
        // free-block write, whatever the stride — there is no per-element loop
        // for a width to bound. Both layout ids have to fit an `i32`, for the
        // word push's reason: the *template* arm tests each against the header's
        // high half with `cmp r64, imm32`.
        Inst::RunFinish {
            dst,
            owner,
            target,
            storage: Storage::Words(elem),
            ..
        } => word_finish(program, *dst, *owner, *target, *elem).is_some_and(|finish| {
            i32::try_from(finish.vector.0).is_ok()
                && i32::try_from(finish.array.0).is_ok()
                && slot(finish.dst)
                && slot(finish.owner)
        }),
        // `Vector.pop` and `Vector.remove`'s truncate, handed to the growable
        // helper whole: two slots it reads, and an element layout it reads off
        // the instruction, bounded against the table. There is no emitted fast
        // path — a truncate clears a stride of words and writes the length, and
        // the helper is one call per pop.
        Inst::GrowableTruncate {
            owner,
            len,
            storage: Storage::Words(elem),
        } => elem.index() < program.layouts.len() && slot(*owner) && slot(*len),
        // [ADR 0062]'s window, each member admitted on its own so that admitting
        // the protocol refuses no function that was compiled before it: an ensure
        // and a commit over either storage, and a byte store. Each is an emitted
        // test with the growable helper as its cold half — see [`Reserve`] and
        // [`ByteStore`] — and each layout id has to fit an `i32`, for the word
        // push's reason.
        //
        // [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
        Inst::GrowableEnsure {
            owner,
            additional: count,
            storage,
        }
        | Inst::GrowableCommit {
            owner,
            count,
            storage,
        } => reserve(program, *owner, *count, *storage).is_some_and(|reserve| {
            i32::try_from(reserve.layout.0).is_ok() && slot(reserve.owner) && slot(reserve.count)
        }),
        Inst::RunStore {
            run,
            index,
            src,
            storage: Storage::PackedBytes,
        } => byte_store(program, *run, *index, *src).is_some_and(|store| {
            i32::try_from(store.bytes.0).is_ok()
                && slot(store.run)
                && slot(store.index)
                && slot(store.src)
        }),
        // `encoded.rs`'s `INTRINSIC_CALL` arm, which is `Machine::call_intrinsic`
        // whole, handed over through the one helper with the protocol its
        // effects ask for — see [`IntrinsicFn`](crate::abi::IntrinsicFn). Bounded
        // like any other operand: a site and an argument list the program has,
        // and a destination slot.
        //
        // **Every intrinsic is admitted, and that is a measurement rather than a
        // default** (#378, Q5.5): a helper call is a native-to-runtime crossing, so
        // admission was to be kept only where the function around the call is
        // faster for it. Admitting none against admitting all, one binary, twelve
        // interleaved rounds: covefmt `whole` -7.3%, cq revenue-summary -10.9%,
        // `benches/keyed` -14.3% (`keyed_of5` -56%), `benches/seqsearch` -6.4%
        // (the wide `Any.equals` rows -8% to -22%), and no row of those or of
        // `benches/builtincall` and `benches/bytescan` slower beyond the spread.
        // Native-to-VM crossings fell on every workload that had them (covefmt
        // 302,791 -> 144,736, cq 3,100,001 -> 1,300,000, keyed 100,016 -> 0), so no
        // narrower rule — only intrinsics that cannot collect, or a list — had a
        // loss to remove. A future intrinsic that measures slower compiled is
        // refused here, by name.
        Inst::IntrinsicCall { dst, site, args } => {
            site.index() < program.intrinsic_sites.len()
                && args.index() < program.args.len()
                && slot(*dst)
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
        // [ADR 0058]'s run copy, over either storage, handed to
        // [`RunCopyFn`](crate::abi::RunCopyFn) whole — whose documentation is
        // where the decision is written down: memmove in bounded chunks with a poll
        // between them, and refusals whose sentences the runtime formats. It is
        // the instruction `Vector.toArray` lowers to, and the census named it as
        // the only blocker of several parser functions.
        //
        // What is bounded is [`Inst::GrowableExtend`]'s: what the helper will read
        // out of this frame. Five operands behind an `ArgsId` — `dst`, `dst_at`,
        // `src`, `src_at`, `count` — each one word, each bounded as a slot. A word
        // copy's element layout is bounded against the program's table, because
        // the helper reads its width, and against an `i32`, for [`WordPush`]'s
        // reason: the template arm materialises it as a 32-bit immediate. Neither
        // is reachable for a verified program, and each is a read past a table
        // rather than a wrong answer if it were.
        //
        // [ADR 0058]: ../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
        Inst::RunCopy { args, storage } => {
            let list = program.arg_list(*args);
            let elem = match storage {
                Storage::PackedBytes => true,
                Storage::Words(elem) => {
                    elem.index() < program.layouts.len() && i32::try_from(elem.0).is_ok()
                }
            };
            elem && list.len() == 5
                && list
                    .iter()
                    .all(|arg| program.layout(arg.layout).width() == 1 && slot(arg.slot))
        }
        // [ADR 0058]'s run slice, the same helper with [`RunOp::SliceWords`] or
        // [`RunOp::SliceBytes`]: an allocation and the run copy that fills it.
        // Bounded as a `run-copy` is — four one-word operands the frame has, and
        // for words an element layout the table has that fits the template arm's
        // immediate.
        //
        // [`RunOp::SliceWords`]: crate::abi::RunOp::SliceWords
        // [`RunOp::SliceBytes`]: crate::abi::RunOp::SliceBytes
        // [ADR 0058]: ../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
        Inst::RunSlice { args, storage } => {
            let list = program.arg_list(*args);
            let elem = match storage {
                Storage::PackedBytes => true,
                Storage::Words(elem) => {
                    elem.index() < program.layouts.len() && i32::try_from(elem.0).is_ok()
                }
            };
            elem && list.len() == 4
                && list
                    .iter()
                    .all(|arg| program.layout(arg.layout).width() == 1 && slot(arg.slot))
        }
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
    use cove_ir::{Layout, RefMap};
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

    // --- `word_push` and `word_finish` -------------------------------------------

    const RUN_INT: LayoutId = LayoutId(1);
    const RUN_VECTOR: LayoutId = LayoutId(2);
    const RUN_ARRAY: LayoutId = LayoutId(3);

    /// A table with an `Int` element, a `Vector<Int>` at [`RUN_VECTOR`] only
    /// when `with_vector` says so, and an `Array<Int>` at [`RUN_ARRAY`].
    fn program_with_runs(with_vector: bool) -> Program {
        let vector = if with_vector {
            Layout::object("Vector", Shape::Vector { elem: RUN_INT })
        } else {
            Layout::word("Unit", Repr::Unit)
        };
        Program {
            layouts: vec![
                Layout::free(),
                Layout::word("Int", Repr::Int),
                vector,
                Layout::object(
                    "Array",
                    Shape::Elements {
                        elem: RUN_INT,
                        growable: false,
                    },
                ),
            ],
            ..Program::default()
        }
    }

    /// The vector a word run's owner must be is found in the table by its
    /// element, as `make::elements` finds an `Array`, and a finish's array is
    /// the instruction's own target.
    #[test]
    fn a_word_run_finds_its_vector_in_the_table() {
        let program = program_with_runs(true);
        assert_eq!(
            word_push(&program, 0, 1, RUN_INT),
            Some(WordPush {
                owner: 0,
                vector: RUN_VECTOR,
                src: 1,
                stride: 1,
            })
        );
        assert_eq!(
            word_finish(&program, 2, 0, RUN_ARRAY, RUN_INT),
            Some(WordFinish {
                dst: 2,
                owner: 0,
                vector: RUN_VECTOR,
                stride: 1,
                array: RUN_ARRAY,
            })
        );
    }

    /// A table with no vector of the element, an element past the table, and a
    /// finish into something that is not the fixed run of the element are each
    /// refused rather than assumed — none is reachable from a verified lowering.
    #[test]
    fn a_word_run_the_table_cannot_describe_is_refused() {
        let program = program_with_runs(false);
        assert_eq!(word_push(&program, 0, 1, RUN_INT), None);
        assert_eq!(word_finish(&program, 2, 0, RUN_ARRAY, RUN_INT), None);
        let program = program_with_runs(true);
        assert_eq!(word_push(&program, 0, 1, LayoutId(99)), None);
        assert_eq!(word_finish(&program, 2, 0, RUN_VECTOR, RUN_INT), None);
        assert_eq!(word_finish(&program, 2, 0, LayoutId(99), RUN_INT), None);
    }
}
