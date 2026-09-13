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
use crate::{ArgsId, BuiltinId, CaseId, FunctionId, HostOpId, StrId, TableId};

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
}

/// A conversion between two scalar representations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Convert {
    /// `Int` to `Float`, as `as`-style widening.
    IntToFloat,
    /// `Float` to `Int`, truncating toward zero.
    FloatToInt,
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
    /// `dst = a op b`, answering a `Bool`.
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
    /// [`FunctionId`], a [`crate::HostOpId`], a [`crate::BuiltinId`] — and
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
    CallBuiltin {
        dst: Slot,
        builtin: BuiltinId,
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
    /// `dst = <byte `at` of the string `obj`>`, as an `Int` in `0..=255`.
    ///
    /// The one instruction that reaches *inside* a word. Everything else here
    /// addresses a value location or a payload word, because a word is what a
    /// frame and a heap object are made of — but a `String`'s payload is
    /// bytes, eight to a word, and the only shape that reads one is this.
    ///
    /// It is an instruction and not a builtin, and the difference is the
    /// whole reason it exists. `String.byteAt` as a `call-builtin` measured
    /// 58 ns of which 48 ns was *being a builtin call* — the operands copied
    /// into a buffer, the operand array built, the dispatch by two strings,
    /// the answer written back — for work that is one payload word, a shift
    /// and a mask. `benches/builtincall` is where those two numbers are.
    ///
    /// `at` is bounds-checked against the receiver's byte length, and an
    /// offset outside it stops the run. That is `String.sliceBytes`'s rule
    /// and not `Array.get`'s: a byte offset out of range is one this type
    /// never handed out, where an index out of range is arithmetic a caller
    /// did about a sequence it can count. Answering an `Option` here would
    /// also be answering it eight times per word of a lexer's inner loop,
    /// and the wrapper was measured at more than the read.
    ByteAt { dst: Slot, obj: Slot, at: Slot },
    /// `dst = <a new, zeroed byte run of `len` bytes>`.
    ///
    /// [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
    /// allocation. It always allocates [`crate::Program::bytes_layout`] —
    /// the one shape every run under construction shares — so unlike
    /// [`Inst::Alloc`] it carries no [`LayoutId`] of its own, for
    /// [`Inst::Str`]'s reason: a program-wide constant should not have to be
    /// named at every call site that always means the same one.
    ///
    /// The payload is zeroed exactly as [`Inst::Alloc`]'s is, so a run that
    /// is collected before it is filled walks safely — not because a
    /// half-written byte is meaningful, but because [`crate::Shape::Bytes`] holds no
    /// references for the collector to chase either way.
    ///
    /// `len` is a byte count and a run-time value, because the whole point
    /// of ADR 0051's construction is a length computed by summing the pieces
    /// a `join` was given — a fixed length would have made this
    /// [`Inst::Alloc`] with a [`Len::Count`] instead. A negative or oversized
    /// `len` fails through the same "this run has no memory left" refusal
    /// every other allocation does.
    AllocBytes { dst: Slot, len: Slot },
    /// `bytes[at] = value`, one checked byte of a run under construction.
    ///
    /// The scalar half of [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
    /// two write primitives, and deliberately the smaller one: it exists for
    /// a delimiter or an encoded scalar a lowering writes one at a time, not
    /// as how a `join` is expected to move text. Copying more than a
    /// handful of bytes through this would replace one native copy with as
    /// many dispatches as there are bytes, which is exactly the shape
    /// [`Inst::CopyBytes`] exists to avoid.
    ///
    /// `bytes` must name a live [`crate::Shape::Bytes`] object — writing into a
    /// `String` is refused, because a `String`'s bytes are the invariant
    /// [`Inst::FinishString`] exists to establish and never to reopen.
    /// `at` is bounds-checked against the run's declared length the same way
    /// [`Inst::ByteAt`]'s is, and `value` must be a byte, `0..=255`: neither
    /// bound is optional here the way it would be reading back a value this
    /// run already produced, because this is the instruction that puts an
    /// arbitrary integer into memory another instruction will one day read
    /// back and trust.
    WriteByte { bytes: Slot, at: Slot, value: Slot },
    /// A bulk range copy into a run under construction: `dst[dst_at
    /// .. dst_at+len] = src[src_at .. src_at+len]`.
    ///
    /// This is [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)'s
    /// principal instruction — the one a `join` or a fused `sliceBytes`
    /// lowers to instead of `sliceBytes -> Vector.push -> join`'s hidden
    /// allocations — and the reason it exists at all is that a byte loop
    /// over [`Inst::WriteByte`] would multiply dispatch by the number of
    /// bytes moved, which ADR 0051's "why a byte loop in IR is not enough"
    /// rejects. One instruction, one native run copy.
    ///
    /// # What it costs, and what it is charged
    ///
    /// One dispatch and one unit of work per payload word moved, which is
    /// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
    /// "charged proportionally to the bytes or words examined". A word is the
    /// unit because a word is what the memory moves.
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
    /// `S + T` forbids. One chunk is one stride of work, so a stopped run gets
    /// no further than a stride past the bound whatever length it was given.
    ///
    /// A collection may therefore happen with the destination half written.
    /// That is safe for the reason ADR 0051 gave for the run's payload holding
    /// no references, and rooted for a second one: the caller has already
    /// `sync`ed, and both objects are named by frame slots this instruction
    /// read them out of, so the walk finds them where it finds every other
    /// live reference.
    ///
    /// [`Inst::AllocBytes`] and [`Inst::FinishString`] are **not** charged
    /// this way and not chunked. Their bulk work is inside the allocator's
    /// zeroing and inside one `from_utf8` over a copy of the run, neither of
    /// which this could interrupt, and charging an operation that cannot be
    /// interrupted only makes its overshoot visible rather than bounded. They
    /// remain one unit each, which is what an ordinary [`Inst::Alloc`] of a
    /// large `Array` has always been.
    ///
    /// `src` may be a `String` **or** another [`crate::Shape::Bytes`] run — a fused
    /// slice copies straight out of the run that produced it, without
    /// finishing it as a `String` first — but `dst` must always be a
    /// [`crate::Shape::Bytes`] run under construction: writing into a `String` is
    /// refused for [`Inst::WriteByte`]'s reason. Bounds are checked against
    /// both objects' declared lengths rather than left to whatever the
    /// native copy routine happens to do with an out-of-range range.
    ///
    /// # Why five operands live behind an [`ArgsId`]
    ///
    /// An encoded instruction has room for three slot-sized operands and a
    /// payload, and this needs five: `dst`, `dst_at`, `src`, `src_at` and
    /// `len`. Rather than spend a fifth [`Inst`] variant or a second
    /// instruction pair to carry the overflow, this reuses the machinery a
    /// call's argument list already is — [`ArgsId`] names a row of
    /// [`crate::Program::args`], and a call already demonstrates that an
    /// arity larger than three operands is a solved problem in this format.
    /// The row holds exactly five [`crate::Arg`]s, in the order `dst`,
    /// `dst_at`, `src`, `src_at`, `len`, and carries each one's layout the
    /// same way a call's arguments do, so the verifier checks them by the
    /// same rule rather than by a new one.
    CopyBytes { args: ArgsId },
    /// `dst = <the run at `bytes`, validated and turned into an immutable
    /// String, in place>`.
    ///
    /// The instruction [ADR 0051](../../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)
    /// closes construction with. `bytes` must name a live [`crate::Shape::Bytes`]
    /// run; its packed payload is read and checked as UTF-8 exactly once,
    /// because a run assembled from [`Inst::WriteByte`] and [`Inst::CopyBytes`]
    /// may hold anything a byte can hold, and ADR 0051 refuses to skip that
    /// check for an arbitrary run. Invalid UTF-8 fails with the same error a
    /// source-level string operation already raises for it.
    ///
    /// On success the run becomes the answer **without copying its
    /// payload**: a [`crate::Shape::Bytes`] object and a [`crate::Shape::Str`] object of
    /// the same byte length occupy the same number of words, so finishing is
    /// a re-label of the object's header — its layout changes from
    /// [`crate::Program::bytes_layout`] to [`crate::Program::str_layout`] and
    /// its `len` does not change at all — rather than an allocation and a
    /// copy. Not copying the payload is the whole performance argument this
    /// ADR makes: every byte a `join` moves is moved once, by
    /// [`Inst::CopyBytes`], and finishing moves none of them again.
    FinishString { dst: Slot, bytes: Slot },
    /// `dst = <a new, empty byte buffer whose store has room for `capacity`
    /// bytes>`.
    ///
    /// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
    /// allocation, and the first of the four instructions that replace
    /// [`Inst::AllocBytes`] wherever the final length is not known before the
    /// writes. ADR 0051's fixed run is enough when it *is* known; it is not
    /// enough for `examples/covefmt`, whose three hot joins are filled by
    /// data-dependent loops and whose largest is a `var out` parameter passed
    /// through recursive calls.
    ///
    /// Two objects are allocated, because that is what a stable owner is: the
    /// owner is [`crate::Program::buffer_layout`], two payload words holding a
    /// logical length and a reference; the store is
    /// [`crate::Program::bytes_layout`], the same packed run ADR 0051 already
    /// has, whose *header* length is the capacity. Neither layout is named
    /// here, for [`Inst::AllocBytes`]'s reason: both are program-wide
    /// constants, and a call site that always means the same one should not
    /// have to say so.
    ///
    /// `capacity` is a **hint** and not a bound. Exceeding it grows the store
    /// rather than failing, so a tuning estimate cannot change what a program
    /// answers — which is the whole of ADR 0052's "capacity is not an Array
    /// length". A capacity below the runtime's own floor is raised to it, and a
    /// negative or oversized one fails through the same "this run has no memory
    /// left" refusal every other allocation does.
    AllocBuffer { dst: Slot, capacity: Slot },
    /// `buffer.append(value)`, one checked byte onto the end of a buffer.
    ///
    /// The scalar half of ADR 0052's append pair, and [`Inst::WriteByte`]'s
    /// counterpart for a growable run — with the one difference that makes a
    /// buffer a buffer: there is no offset. A write goes at the logical length
    /// and the logical length becomes one more, so a caller never names a
    /// position and can never leave a hole below one.
    ///
    /// It exists for a delimiter or an encoded scalar a lowering emits one at a
    /// time, not as how text is expected to move: copying a run of bytes
    /// through this would be as many dispatches as there are bytes, which is
    /// what [`Inst::AppendBytes`] is for.
    ///
    /// `buffer` must name a live owner and `value` must be a byte, `0..=255`,
    /// for [`Inst::WriteByte`]'s reason — this is an instruction that puts an
    /// arbitrary integer into memory that [`Inst::FinishBuffer`] will later
    /// read back and validate. Nothing is bounds-checked against the capacity,
    /// because there is no bound to check: a full store grows.
    AppendByte { buffer: Slot, value: Slot },
    /// A bulk range append: `buffer.append(src[from .. to])`.
    ///
    /// ADR 0052's principal instruction, and [`Inst::CopyBytes`]'s growable
    /// counterpart. One dispatch moves the whole range, for the reason ADR
    /// 0051 gave when it refused a byte loop in IR: a loop of
    /// [`Inst::AppendByte`] would multiply dispatch by the number of bytes.
    ///
    /// `src` may be a `String` **or** a [`crate::Shape::Bytes`] run, which is
    /// what lets a fused slice copy straight out of the run or the string that
    /// produced it. The ADR's own example is the optimisation this enables:
    /// `sliceBytes(source, from, to) -> append` becomes one checked append
    /// from that source range and never materialises the slice.
    ///
    /// Where `src` is a `String`, `from` and `to` are checked to be character
    /// boundaries and not merely in range — the same check, in the same words,
    /// that `String.sliceBytes` makes. ADR 0052 requires it: "`appendSlice`
    /// checks the same bounds and UTF-8 boundaries as `String.sliceBytes`".
    /// Without it a program could assemble a run of valid pieces that is not
    /// valid UTF-8, and discover it only at [`Inst::FinishBuffer`], where the
    /// offset that did it is long gone. A [`crate::Shape::Bytes`] source is
    /// held to no such rule, because a run under construction is not claiming
    /// to be text.
    ///
    /// # What it costs, and what it is charged
    ///
    /// [`Inst::CopyBytes`]'s answer, unchanged: one unit of work per payload
    /// word moved, in bounded chunks with a safepoint between them, so a
    /// stopped run gets no further than a stride past the bound whatever length
    /// it was given. Growth is charged as the allocation it is.
    ///
    /// The store is grown **once, up front**, for the whole range rather than
    /// per chunk. That is not only cheaper: a growth part way through would
    /// have to copy a prefix that the chunks before it had already written, and
    /// the one allocation before the first chunk is what keeps the copy a copy.
    ///
    /// # Why four operands live behind an [`ArgsId`]
    ///
    /// [`Inst::CopyBytes`]'s reason at one fewer operand: an encoded
    /// instruction has room for three slot-sized operands and this needs four —
    /// `buffer`, `src`, `from` and `to`. Rather than spend a second instruction
    /// to carry the overflow, this reuses the machinery a call's argument list
    /// already is. The row holds exactly four [`crate::Arg`]s in the order
    /// `buffer`, `src`, `from`, `to`, and carries each one's layout the way a
    /// call's arguments do, so the verifier checks them by the same rule.
    AppendBytes { args: ArgsId },
    /// `dst = <the buffer at `buffer`, consumed, its store validated and
    /// relabelled into an immutable String>`.
    ///
    /// ADR 0052's finish, and [`Inst::FinishString`]'s counterpart for a
    /// growable run. The bytes are read and checked as UTF-8 exactly once,
    /// because a run assembled from [`Inst::AppendByte`] may hold anything a
    /// byte can hold, and invalid UTF-8 fails with the same error a
    /// source-level string operation already raises for it.
    ///
    /// What is validated and what is answered is the **live prefix**
    /// `[0, length)`. A store is as long as the last growth made it, and ADR
    /// 0052's "finishing reuses the store" is what happens to the rest: the
    /// store is relabelled from [`crate::Program::bytes_layout`] to
    /// [`crate::Program::str_layout`] with the *logical* length, and the words
    /// between the two lengths become a free block the next sweep folds back
    /// in. Nothing is copied, which is the same O(1) transition
    /// `Vector.freeze()` already makes for elements.
    ///
    /// The owner is then emptied — length zero, store null — exactly as
    /// `Vector.freeze()` empties a vector, because finishing *consumes*. That
    /// the consumed buffer has no second live holder is
    /// [`cove_sema`](../../../crates/cove-sema/src/unique.rs)'s conservative
    /// local uniqueness proof and not something this machine can answer; what
    /// the machine keeps is the liveness check, so a buffer used after a finish
    /// is refused rather than read as an empty one.
    ///
    /// [`crate::Shape::Bytes`] cannot cross a Cove call and neither can the
    /// owner cross the Host boundary, but the owner *can* cross a call, which
    /// is the whole reason it is a value rather than a raw run: the formatter's
    /// `fn emit(node: Tree, var out: StringBuilder)` needs to pass a partly
    /// built string down a recursion.
    FinishBuffer { dst: Slot, buffer: Slot },
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
    /// Fail the run with `message`.
    ///
    /// This is what an exhausted `match` and a failed `Unbox` reach. It is
    /// not a refusal to run the program: the program ran, and this is what
    /// it did.
    Trap { message: StrId },

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
