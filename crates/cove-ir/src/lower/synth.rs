//! Synthesizing a layout-directed operation as ordinary function IR.
//!
//! [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 3: for the five operations that are *walks of a layout* rather
//! than algorithms over a representation, **lowering synthesizes one private
//! function per `(operation, layout)` actually reached, composed
//! structurally.** This module is that producer. `Any.equals` was its first
//! user, `Value.order` its second and `Value.admitKey` its third: what
//! differs between them is the shape of the control flow and not the table
//! being walked, so the memo is keyed by the *pair* and each operation is a
//! variant of [`Operation`] and an arm of [`Synth::body`].
//!
//! # Why a producer and not a Cove declaration
//!
//! Cove has no structural access. A field is reached by a name the source
//! wrote, `match` names a case, and a `dyn Trait` gives polymorphism over
//! behaviour rather than over structure — so `fn equal<T>(a: T, b: T) -> Bool`
//! cannot be *written* for an arbitrary `T` today, however much the answer is
//! decided by `T` alone. What the language does have is
//! [`Body::instantiation`](super::dispatch), which already produces one
//! function per `(declaration, type arguments)` pair; this is the same idea
//! with a [`Shape`] walk in place of the substitution into a written body.
//!
//! # It is ordinary IR, and nothing below lowering may know its name
//!
//! What this emits is a [`Function`]: parameters, a frame, comparisons,
//! branches, a loop, a [`Inst::Switch`], calls to other synthesized functions,
//! and a [`Inst::Return`]. The printer prints it, [`crate::verify`] checks it,
//! `super::inline` expands it where it is small enough, `super::sweep` stands
//! it down where the expansion left nothing naming it, and both code
//! generators see a function. **No backend may recognize it by name**, which
//! ADR 0064 states as a rule and
//! `crates/cove-ir/src/lower/tests/synthesis.rs` asserts as a fact about the
//! source of the two backend crates: a backend that matched on a
//! standard-library function name would have reinvented the string dispatch
//! ADR 0058 deleted.
//!
//! # The one thing it does not walk
//!
//! [`Shape::Boxed`] — `dyn Trait`, and a Host schema's `Any` — keeps its
//! [`LayoutId`] in payload word 0 and is genuinely unknown until the box is
//! opened. Decision 4 admits exactly one dynamic-layout fallback *per
//! operation* and the first two are reached from there and from nowhere else.
//!
//! Neither fallback is an intrinsic any more. [ADR
//! 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! Phase 2 made equality's `std.dynamic.equals`, a Cove walk over a view of
//! each box, and deleted `Intrinsic::AnyEquals`; its Phase 3 made the order's
//! `std.dynamic.order` and deleted `Intrinsic::ValueOrder`. Each has **two**
//! callers, not one: [`Synth::fallback`] and [`Synth::dynamic`], where a walk
//! reaches a boxed part, and `Body::compare_values` and `Body::core_order`,
//! where `==` or `core.order` itself meets two boxes. Both call the one
//! function, and `cove-cli`'s `tests/boxed.rs` holds every call of either to
//! erased operands (Decision 5).
//!
//! # The admission is two walks, and the reason is the sentence
//!
//! The third user does not fit that paragraph and it is worth saying why
//! rather than filing it under "nearly". The other two *answer* something —
//! a `Bool`, an `Int` — and a walk that has composed the answer is done. An
//! admission answers `()` **or raises**, and what it raises is
//! `` `{method}` cannot use a `{type}` inside `{path}` as a {role} `` with a
//! `rule:` and a `help:` beside it: three sentences, and [`Inst::Trap`] takes a
//! slot for each (ADR 0067).
//!
//! Every hole in that sentence is something the lowering knows, but for the
//! path. The two names are literals at all nine of `std.map`'s and `std.set`'s
//! call sites; the type, the rule and the help are the refused part's layout's;
//! and the path is composed of field and case names a walk knows statically per
//! arm — except where it reaches through a run or a map, where it quotes an
//! index or a map's key as it renders, which is text made at run time.
//!
//! So the admission is **two** walks. [`Operation::Admission`] **decides**: a
//! `Bool`, `true` where the key is refused, reading only what decides and
//! allocating nothing — which is all the path that admits, every path a
//! program that works takes, ever runs. [`Operation::Description`] **words**:
//! `super::core` calls it under the branch on that answer, at the site, in the
//! caller's frame, and it walks the same parts again, writes the path, and
//! ends in one [`Inst::Trap`]. The runtime's `Value.admitKey` stood under that
//! branch and worded the refusal in Rust until [ADR
//! 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! Phase 4c. The diagnostic is what it was byte for byte, blamed where it was:
//! a wording walk is support code ([`Function::is_support`]), so its trap is
//! blamed on the `core.admitKey` line that called it.
//!
//! [`admission`] is the question that decides whether either is made, asked
//! by the call site and by the walks, of one table — `ordered_by`'s
//! arrangement, and `Body::always_admitted` before this module had an arm for
//! the operation.
//!
//! A box is decided the same way since the ADR's Phase 3 —
//! `std.dynamic.refusesKey`, a Cove walk over a view of the box, answers the
//! bit, called by `super::core` for a key that is a box and by [`Synth::admit`]
//! where a walk reaches a boxed part — and worded the same way since its Phase
//! 4c, by `std.dynamic.refuseKey`, called by `super::core` and by
//! [`Synth::describing`] where the part it names is a box. So a boxed layout is
//! [`Admission::Decided`] like any other layout a walk can answer for, and
//! `cove-cli`'s `tests/boxed.rs` holds every call of either function to an
//! erased operand (Decision 5).
//!
//! # Three of the walks allocate nothing, and the rest allocate by nature
//!
//! This paragraph used to say *nothing here allocates*, as a fact about every
//! instruction the module emits, and [`Operation::Rendering`] is the arm that
//! made it false. A rendering's whole business is to put bytes in a buffer:
//! it calls `std.stringbuilder`'s `appendText` and `appendByteInto` and
//! `std.int`'s `renderInto`, each of which is [ADR 0062]'s ensure, write and
//! commit, and an *ensure* that does not fit grows the buffer's store. So a
//! collection can happen part way through a rendering walk, and a reader who
//! believed the old sentence would have reasoned wrongly about exactly the
//! arm that needs the reasoning.
//!
//! What is true of each, and is what the old sentence was reaching for:
//!
//! - [`Operation::Equality`], [`Operation::Order`] and
//!   [`Operation::Admission`] emit only loads, comparisons, branches,
//!   [`Inst::Trap`]s and calls to walks of their own kind, and no intrinsic
//!   any of them reaches declares `MAY_ALLOCATE`. Nothing can collect inside
//!   one — except inside the one call each makes where it reaches a box, to
//!   `std.dynamic.equals`, `std.dynamic.order` or `std.dynamic.refusesKey`,
//!   which allocate a stack of views once a boxed value nests; and that is an
//!   ordinary call, whose frame and whose caller's are traced like every
//!   other. [`Operation::Tracked`] is equality's walk and adds only frame
//!   addresses and loads through them — the path of vector pairs it carries
//!   lives in the frames of the walks that are inside those pairs — so it
//!   allocates nothing either.
//! - [`Operation::Rendering`] can collect at every append. It still owes no
//!   [`Inst::Clear`], and the reason is what a clear is *for*: a clear ends
//!   the **retention** a static [`crate::repr::RefMap`] would otherwise give
//!   a dead slot, and it is never a safety obligation — an address a live
//!   frame's map still names is traced, so it is never stale and never
//!   dangling (`super::tails` says the same thing from the other side).
//!   What a rendering walk retains is one element of the run it is walking,
//!   in a slot the next turn overwrites, until its own frame is popped a few
//!   instructions later. That is the position `super::tails` exists to drop
//!   a clear *from*. [`Operation::RenderTracked`] is the rendering's walk and
//!   adds what [`Operation::Tracked`] adds, frame addresses and loads through
//!   them, so what it allocates is what the appends under it allocate and not
//!   one word more.
//! - [`Operation::Description`] and [`Operation::DescribeAt`] allocate on
//!   the one path they have, which ends the run: the buffer the path is
//!   written into, the appends into it and the renderings of the keys it
//!   quotes, and the sentence the path is finished into. They owe no
//!   [`Inst::Clear`] for the rendering's reason and a stronger one: nothing
//!   after the trap they end in reads a frame at all.
//!
//! So an arm added later that allocates owes the clears that its own shape
//! earns, and this one earns none. An arm that held a reference across an
//! unbounded amount of work — a run of appends whose length is the *value's*
//! rather than the layout's — would be a different answer, and the sentence
//! to check it against is the one above rather than the one this replaced.
//!
//! [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
//!
//! What [`Operation::Order`] adds is a walk that can *fail*. A `Float` and a
//! mutable handle are not keys, so where equality falls through to `false`
//! the order raises — and it raises the sentence `std.dynamic.order` raises,
//! and the Rust walk did before it, which is a **constant**. Its [`crate::StrId`] is chosen by the lowering that
//! emits it and loaded into [`Inst::Trap`]'s slot with [`Inst::Str`] — a
//! precomputed address, not an allocation — exactly as `super::pattern`'s
//! uncovered `match` and `super::dispatch`'s undispatchable call choose
//! theirs, so a refusal whose wording does not quote a value computed at run
//! time is one synthesis can write. Issue #461 is about the other kind.
//! `Value.order`'s two are not it. The admission's refusal is — its path
//! quotes an index or a map's key as it renders — and since ADR 0068's Phase
//! 4c [`Operation::Description`] builds that text at run time into a buffer,
//! as an interpolation does, and hands the trap the `String` it made.
//!
//! # The fifth operation is not here, and it never needed to be
//!
//! Decision 3 names five, and four of them are [`Operation`]'s variants. The
//! fifth — `Value.refuseDuplicate`, which `std.set.of` and `std.map.of` reach
//! when a literal holds one element twice — **was never a walk this module
//! had to compose**, and since ADR 0067 it is not an intrinsic either. It is
//! written down here rather than left as a gap, because a gap reads as work
//! outstanding and this is a finding.
//!
//! **There is no decision left to make.** The other four answer something a
//! call site asked: a `Bool`, an `Int`, a decision-`Bool`, an appended `()`.
//! A duplicate's refusal answers nothing — it *always raises* — and it is
//! reached only from inside `if at >= 0` in `std.set.of` and `std.map.of`,
//! where `seekPlaced` has already found the duplicate **in Cove**. So the
//! split [`Operation::Admission`] uses does not apply: a `refuses<L>(L) ->
//! Bool` for this operation would be `return true`, no layout read and no
//! call avoided.
//!
//! **The one thing in it that is layout-directed is the sentence**, and the
//! sentence is ordinary Cove. The refusal is
//! `` `{method}` was given the {role} `{key}` more than once ``, and `{key}`
//! is the key **as it renders** — which is an interpolation, and an
//! interpolation of a layout the lowering knows is [`Operation::Rendering`]'s
//! walk already. What was missing was never the rendering; it was a way to
//! raise three sentences a frame holds, and `core.refuse` over
//! [`Inst::Trap`]'s three slots is that. So `std.set` and `std.map` word the
//! refusal themselves and nothing here grew a variant for it.
//!
//! `tests/e2e/values_value_refuse_duplicate` is the corpus that held the
//! migration to the bytes the two Rust copies had written, blame included.
//!
//! # A value that contains itself
//!
//! A struct pushed into a vector it holds has no end, and a walk of its layout
//! recursed until the machine's stack segment stopped it — or, on the native
//! tier through a map, the host's. Since issue #493 `==` refuses such a value,
//! and it does so with the path the comparison is walking and nothing more:
//! the pairs of vectors, one from each side, it is inside. [`tracked`] decides
//! which layouts can hold themselves at all — only through a `Vector`, since
//! everything else a walk descends into is immutable once built — and only
//! those get [`Operation::Tracked`]'s walks, which carry the path as two
//! parameters and look a vector pair up on it before going inside. Every other
//! layout's walk is what it was, instruction for instruction. The refusal is
//! [`crate::dynamic::CONTAINS_ITSELF`], the sentence `std.dynamic.equals` and
//! the oracle raise, and it is blamed on the `==` that reached the walk: the
//! machine does not count a walk's frames as callers (see
//! [`Function::is_support`]).
//!
//! The rendering meets the same values and answers them differently: issue
//! #499's decisions 1 and 2 have a vector the rendering is already inside
//! render as `[…]`, so `struct S { v: Vector<S> }` with `s` pushed into its own
//! `v` is `S(v: [S(v: […])])`. [`render_tracked`] decides which layouts carry
//! that path — the same cycles, and for a reason of the rendering's own every
//! layout whose walk reaches a box — and only those get
//! [`Operation::RenderTracked`]'s walks, which carry it exactly as
//! [`Operation::Tracked`]'s do: in the frames of the vectors being rendered,
//! with no allocation. Every other layout's rendering is what it was,
//! instruction for instruction.

use std::sync::Arc;

use cove_diag::Span;
use cove_schema::builtins::{ERROR, MESSAGE_FIELD, RANGE};

use crate::inst::{ArithOp, CmpOp, Compare, Inst, Pc, Slot, Storage, Validation};
use crate::layout::{Case, Field, Layout, LayoutId, Shape};
use crate::program::{Arg, Function, FunctionId, Table, TableId};
use crate::repr::{RefMap, Repr};

use super::named::NamedId;
use super::shapes;
use super::{Pool, PENDING};

/// The module a synthesized function says it is in.
///
/// Not a module any source can name — `<` is not a name character — so a
/// declaration cannot collide with one and `cove run` cannot reach one. It is
/// here for a listing, a profile row and a debugger's frame name, and for
/// [`Function::is_support`], which is what blames a refusal raised inside a
/// walk on the `==` that reached it.
pub(super) const MODULE: &str = crate::program::SYNTHESIZED_MODULE;

/// One layout-directed operation ADR 0064's Decision 3 names.
///
/// **Four of the five, and the fifth is not pending.** Decision 3 names
/// `ValueRefuseDuplicate` too, and this module's header says why it is not a
/// variant here: it decides nothing — `std.set.of` and `std.map.of` have
/// already found the duplicate in Cove — and its sentence is an ordinary
/// interpolation those two bodies raise through `core.refuse`.
///
/// The key of the memo is still the *pair* rather than the layout, so that
/// adding an operation is a variant here and an arm in [`Synth::body`] rather
/// than a second memo.
///
/// [`Operation::Tracked`] is a variant for that reason and not a sixth
/// operation: it is `==` again, at the layouts where a value can contain
/// itself, and a second key is what lets `equals<L>` and `tracks<L>` be two
/// functions of one layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Operation {
    /// `a == b`: whether two values of one layout are the same value.
    Equality,
    /// `core.order(a, b)`: where one value of a layout sorts relative to
    /// another in the order a `Map` keeps its keys and a `Set` its elements.
    ///
    /// Three-valued where equality is two-valued, and that is the whole of
    /// what makes it a different shape of walk rather than a different set of
    /// comparisons. The answer is an `Int` — `-1`, `0`, `1` — every arm
    /// writes it, and the walk goes on only while it is `0`. So the early
    /// exit is [`Synth::leave_unless_settled`] where equality's is a
    /// `branch-false`, and the *last* part of a struct or a case still needs
    /// no branch after it, because its answer is the whole answer exactly as
    /// equality's is.
    Order,
    /// `core.admitKey(key, method, role)`: whether this value is one the
    /// language will not have as a map key or a set element.
    ///
    /// The walk **decides** rather than refuses, and that is the whole of
    /// what makes it a third shape. The other two answer the question their
    /// call site asked; this one answers a question the call site did not
    /// ask — the call site asked for a *refusal*, which is a sentence, and
    /// the walk hands back the one bit that says whether there is one to
    /// write. Where the bit is set, `super::core` runs
    /// [`Operation::Description`]'s walk, at the site, and that walk writes
    /// the sentence.
    ///
    /// It has to be that way round, and the reason is in the sentence rather
    /// than in the walk. A refusal here is
    /// `` `{method}` cannot use a `{type}` inside `{path}` as a {role} ``
    /// with a **`rule:` and a `help:` beside it**, and a `path` that reaches
    /// through a run or a map quotes an index or a rendered key computed at
    /// run time: text, which the path that admits has no business making. See
    /// this module's header.
    ///
    /// So the answer is a `Bool` and the sense of it is **`true` when the key
    /// is refused**: a `branch-false` over the wording is one instruction
    /// where a `branch-true` would be two, since the instruction set has only
    /// the one.
    Admission,
    /// `core.renderInto(value, buffer)`: the text of one value of a layout,
    /// appended to the byte buffer an interpolation is being assembled in.
    ///
    /// Two things make this a fourth shape of walk, and neither of them is
    /// the table being walked — which is the same [`Shape`] table the other
    /// three read.
    ///
    /// **It is a composition of calls rather than of comparisons.** The
    /// other three end at an [`Inst::Cmp`]; every leaf of this one is a
    /// `call`, of `std.stringbuilder`'s `appendText` or `appendByteInto` for
    /// a literal and of `std.int`'s, `std.float`'s or `std.duration`'s
    /// `renderInto` for a number. Those are the same bodies
    /// `super::interpolate` already appends a piece through, reached the same
    /// way, so no lowering writes [ADR 0062]'s
    /// ensure-store-commit window a second time. [`Leaves`] is how a walk
    /// gets at them, because resolving a name is `Plan` and `Body`'s and a
    /// [`Pool`] has neither.
    ///
    /// **It has a written second operand.** The buffer is a parameter, not
    /// an answer: a byte buffer is a handle (ADR 0052), so a callee's
    /// appends are its caller's and a level of nesting costs no `String` at
    /// all. What the function answers is `()`, which is why nothing here
    /// uses [`Synth::leaves`](Synth::leave) — a rendering never stops early,
    /// every part of a value is shown, and the joins it does need are local
    /// to the `switch` or the branch that opened them.
    ///
    /// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    Rendering,
    /// `a == b` at a layout that can reach itself through a `Vector`, carrying
    /// the pairs of vectors the comparison is inside so that a value holding
    /// itself is refused rather than walked for ever (issue #493).
    ///
    /// [`Operation::Equality`]'s walk, part for part and in the same order,
    /// with two more parameters: the address of the innermost vector pair on
    /// the path and how many pairs the path holds. Only a layout [`tracked`]
    /// answers `true` for has one, and only a function of this operation
    /// calls one — `equals<L>` for such a layout is a wrapper that starts the
    /// path empty. So a layout that cannot reach itself through a vector is
    /// walked exactly as it was before this variant existed, instruction for
    /// instruction. See [`Synth::tracking`].
    Tracked,
    /// `core.renderInto(value, buffer)` at a layout whose rendering carries
    /// the vectors it is inside, so that a value holding itself renders its
    /// repeat as `[…]` rather than being walked for ever (issue #493, and
    /// issue #499's decisions 2 and 3).
    ///
    /// [`Operation::Tracked`]'s arrangement for the rendering: the
    /// [`Operation::Rendering`] walk, part for part and append for append,
    /// with two more parameters — the address of the innermost vector on the
    /// path and how many vectors the path holds. Only a layout
    /// [`render_tracked`] answers `true` for has one, and only a function of
    /// this operation calls one: `renders<L>` for such a layout is a wrapper
    /// that starts the path empty. So a layout that is on no cycle and
    /// reaches no box is rendered exactly as it was before this variant
    /// existed, instruction for instruction. See [`Synth::render_tracking`].
    RenderTracked,
    /// `core.admitKey(key, method, role)` where [`Operation::Admission`]'s
    /// walk answered that the key is refused: the refusal, in `method`'s words
    /// and naming the key by `role`, of the part of it that is refused. ADR
    /// 0068's Phase 4c; see [`Synth::describing`].
    ///
    /// The refusal-path twin of the admission walk, and a function of its own
    /// rather than an arm of it for the reason the admission walk decides
    /// without wording: everything a refusal quotes — the path from the key to
    /// the part, an index, a map's key as it renders — is text, and text is
    /// work the path that admits never does. So `refuses<L>` still answers one
    /// bit and allocates nothing, and `describes<L>` runs only once it has
    /// answered `true`, at the site, in the caller's frame.
    Description,
    /// [`Operation::Description`] of a part of a key rather than of the key,
    /// with the path down to the part on a trail the caller passes: what a
    /// wording walk calls where it reaches a layout that holds itself, or one
    /// nested past [`NESTING`], rather than expanding it in place.
    /// [`Operation::Tracked`]'s arrangement — `describes<L>` starts the trail,
    /// and every walk below it that is a call is one of these. It returns
    /// where the part is admitted, which is how its caller learns so: see
    /// [`Synth::phrase`].
    DescribeAt,
}

impl Operation {
    /// What the synthesized function is called, before its layout.
    fn verb(self) -> &'static str {
        match self {
            Operation::Equality => "equals",
            Operation::Order => "order",
            Operation::Admission => "refuses",
            Operation::Rendering => "renders",
            Operation::Tracked => "tracks",
            Operation::RenderTracked => "rendersTracked",
            Operation::Description => "describes",
            Operation::DescribeAt => "describesAt",
        }
    }

    /// The layout of what it answers.
    fn answers(self) -> LayoutId {
        match self {
            Operation::Equality => shapes::BOOL,
            Operation::Order => shapes::INT,
            Operation::Admission => shapes::BOOL,
            Operation::Rendering => shapes::UNIT,
            Operation::Tracked => shapes::BOOL,
            Operation::RenderTracked => shapes::UNIT,
            Operation::Description | Operation::DescribeAt => shapes::UNIT,
        }
    }

    /// What it takes, in order.
    ///
    /// Two values of the layout for a comparison, one for the admission,
    /// which needs neither of `core.admitKey`'s two names — they word a
    /// refusal, and this walk does not word one — and for the rendering the
    /// value and the buffer its text is appended to, in that order, which is
    /// `std.dynamic.renderInto`'s own, and was `Value.renderInto`'s.
    ///
    /// `trail` is the layout of the trail a wording walk of a part is handed,
    /// a `Vector<String>` — which only the program's own layout table can
    /// name — and is read by [`Operation::DescribeAt`] alone.
    fn params(self, layout: LayoutId, trail: Option<LayoutId>) -> Vec<LayoutId> {
        match self {
            Operation::Equality | Operation::Order => vec![layout, layout],
            Operation::Admission => vec![layout],
            Operation::Rendering => vec![layout, shapes::BYTE_BUFFER],
            // The path's innermost pair, as the address of the frame words it
            // is in, and how many pairs deep the path goes.
            Operation::Tracked => vec![layout, layout, shapes::ADDR, shapes::INT],
            // The same two words after the rendering's own pair: the path's
            // innermost vector, as the address of the frame word it is in,
            // and how many vectors deep the path goes.
            Operation::RenderTracked => {
                vec![layout, shapes::BYTE_BUFFER, shapes::ADDR, shapes::INT]
            }
            // The key and `core.admitKey`'s two names, which is the site's
            // call; and for a part, the trail of the path down to it after
            // them.
            Operation::Description => vec![layout, shapes::STR, shapes::STR],
            Operation::DescribeAt => vec![
                layout,
                shapes::STR,
                shapes::STR,
                trail.expect(
                    "a wording walk of a part is asked for only after its call site resolved the \
                     trail it is handed",
                ),
            ],
        }
    }
}

/// What the language can say, from a layout alone, about every value of it as
/// a map key or a set element.
///
/// ADR 0001 admits a key built only from immutable parts and refuses a
/// `Float`, whose `NaN` is not equal to itself and so has no total order, and
/// a `Vector` and everything holding one, because a key's equality must not
/// change while a collection holds it. Which of the two this answers is what
/// decides, at the call site, between emitting nothing and asking.
///
/// The order of the variants is the order of the lattice: a composite is the
/// **greatest** of its parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Admission {
    /// No value of this layout is ever refused, so asking is nothing at all.
    ///
    /// `Body::always_admitted` before this module had an arm for the
    /// operation, moved here unchanged and for [`ordered_by`]'s reason: the
    /// call site and the walk ask one question, in one place, of one table.
    /// **It is load-bearing and it is not an optimisation.** Every key
    /// `covefmt` and `cq` use is a `String` or an `Int`, so this is why
    /// neither program asks an admission anything at a single site.
    Always,
    /// Some values of this layout are refused and a walk can tell which.
    ///
    /// A `Float` field, a `Vector` field, an enum with one case that holds
    /// one — the walk reads whatever decides and asks only where the answer
    /// is that the key is refused, which is the one path that does not
    /// continue.
    Decided,
    // `Dynamic` stood here until ADR 0068's Phase 4c: the runtime's own walk
    // answered a layout that holds itself, and one nested past [`NESTING`].
    // Both are walks of calls now — see [`Synth::admit`] — and a box, the
    // third it once held, has been `Decided` since the ADR's Phase 3.
}

/// How deep a key's layout may nest before a walk stops expanding a part in
/// place and calls the walk of the part's layout instead.
///
/// `Body::always_admitted`'s `ADMITTED_DEPTH`, moved here with it. It was a
/// bound on how deep the runtime's walk went before that bound was taken away
/// in ADR 0068's Phase 3, and it is a bound on code size and nothing else: a
/// layout nested past it is walked by calls, one function a level, exactly as
/// a layout that holds itself is.
const NESTING: usize = 48;

/// Which of the two [`layout`] is.
///
/// Over a slice of layouts rather than over a [`shapes::Shapes`] because
/// [`crate::verify`] asks it too, of a finished [`crate::Program`], and the
/// rule it checks is this one: a question asked in one place of one table.
///
/// A composite is the greatest of the parts it can reach, which is every
/// layout a walk of it descends into — so the answer is a question about the
/// set of them rather than about any one route there, and a layout that holds
/// itself asks it of a finite set. `struct Node { tag: Int, kids: Array<Node>
/// }` reaches `Node`, `Array<Node>` and `Int`, and every one of them is a key,
/// so however deep a `Node` nests it is [`Admission::Always`] — which is what
/// the runtime's walk found about each value of it, one node at a time, until
/// ADR 0068's Phase 4c asked the layout instead.
pub(crate) fn admission(layouts: &[Layout], layout: LayoutId) -> Admission {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![layout];
    while let Some(at) = pending.pop() {
        if !seen.insert(at) {
            continue;
        }
        let Some(described) = layouts.get(at.index()) else {
            return Admission::Decided;
        };
        match &described.shape {
            // The scalars a key may be, and the string.
            Shape::Word(Repr::Unit | Repr::Bool | Repr::Int | Repr::Duration) | Shape::Str => {}
            // A set's members are keys by construction, so nesting one never
            // fails and nothing inside it is asked about.
            Shape::Members { .. } => {}
            // An array is its element, and a map is its *value*: a map's keys
            // are keys by construction too, so only its values need asking.
            Shape::Elements {
                elem,
                growable: false,
            } => pending.push(*elem),
            Shape::Entries { value, .. } => pending.push(*value),
            Shape::Struct { fields, .. } => {
                pending.extend(fields.iter().map(|field| field.layout));
            }
            Shape::Enum { cases, .. } => pending.extend(
                cases
                    .iter()
                    .flat_map(|case| case.parts.iter().map(|part| part.layout)),
            ),
            // A box is decided by `std.dynamic.refusesKey` over a view of it,
            // so it is a part a walk can answer for like any other: some of
            // its values are refused, and a call tells which. And a `Float`, a
            // `Vector` and the growable run beneath it, a byte run and a byte
            // buffer, a closure, a `Shared` cell, a host handle, a task and a
            // task scope: every one of them is refused, whatever value it
            // holds, and a walk that met one knows so.
            _ => return Admission::Decided,
        }
    }
    Admission::Always
}

/// Whether every value of `layout` is refused as a key, whatever it holds: a
/// part the walks name as the refused part the moment they meet it, and a key
/// the call site refuses in a literal sentence.
///
/// Every family [`admission`] answers [`Admission::Decided`] for by itself —
/// not through a part — except a box, whose value decides.
pub(crate) fn refused_whole(shape: &Shape) -> bool {
    !matches!(
        shape,
        Shape::Word(Repr::Unit | Repr::Bool | Repr::Int | Repr::Duration)
            | Shape::Str
            | Shape::Members { .. }
            | Shape::Elements {
                growable: false,
                ..
            }
            | Shape::Entries { .. }
            | Shape::Struct { .. }
            | Shape::Enum { .. }
            | Shape::Boxed
    )
}

/// What a refusal calls a value of `layout`, which [`refused_whole`] answers
/// `true` for: its type, in the words the oracle's `MapKey::convert` names it
/// by and `std.dynamic.refuseKey` names a boxed one by too — `Float`,
/// `Vector`, `fn`, `Shared`, `Task`, `TaskScope` (issue #506).
///
/// A Host resource is named by `resource`, its qualified type — `http.Server`
/// — which the layout cannot say, because every resource shares one: the
/// caller has it from the key's type, through [`super::named`], and it is
/// `None` only where no resource can be. It is never the handle's number, and
/// nothing about it is read at run time.
pub(crate) fn refused_word<'a>(shape: &Shape, resource: Option<&'a str>) -> &'a str {
    match shape {
        Shape::Word(Repr::Float) => "Float",
        Shape::Word(Repr::Host) => resource.unwrap_or("nothing"),
        Shape::Word(Repr::Task) => "Task",
        Shape::Word(Repr::Scope) => "TaskScope",
        Shape::Word(Repr::Addr) => "a place",
        Shape::Word(Repr::Tag) => "an enum case",
        // A bare reference word is a function value's location, whose object
        // is a closure.
        Shape::Word(_) | Shape::Closure { .. } => "fn",
        Shape::Vector { .. } | Shape::Elements { .. } => "Vector",
        Shape::Shared { .. } => "Shared",
        Shape::Bytes => "<byte run>",
        Shape::ByteBuffer => "<byte buffer>",
        _ => "nothing",
    }
}

/// The map keys a refusal of `layout` can quote: the key layout of every map
/// an admission walk of it reaches, whose values it asks about and whose
/// entries it names by the key as that key renders — `[7]`.
///
/// Asked by the call site before it asks for the wording walk, because
/// rendering a key is a walk of the key's own and the call site is what
/// resolves the functions one calls ([`reaches_a_scalar`]'s reason).
pub(super) fn quoted_keys(shapes: &shapes::Shapes, layout: LayoutId) -> Vec<LayoutId> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![layout];
    let mut keys = Vec::new();
    while let Some(at) = pending.pop() {
        if !seen.insert(at) {
            continue;
        }
        match &shapes.layout(at).shape {
            Shape::Elements {
                elem,
                growable: false,
            } => pending.push(*elem),
            Shape::Entries { key, value } => {
                keys.push(*key);
                pending.push(*value);
            }
            Shape::Struct { fields, .. } => {
                pending.extend(fields.iter().map(|field| field.layout));
            }
            Shape::Enum { cases, .. } => pending.extend(
                cases
                    .iter()
                    .flat_map(|case| case.parts.iter().map(|part| part.layout)),
            ),
            _ => {}
        }
    }
    keys
}

/// The three standard-library appends a rendering walk is composed out of.
///
/// A synthesized walk emits ordinary [`Inst::Call`]s and a call needs a
/// [`FunctionId`]. Turning `std.stringbuilder.appendText` into one is
/// `Plan::resolve`'s and `Body::reached`'s, and a [`Pool`] has neither — so
/// the call site that first asks for a rendering resolves all three, and
/// every walk that ask reaches reads them from here.
///
/// They are the same three bodies [`super::interpolate`] appends a piece
/// through. That is the point rather than a convenience: each is
/// [ADR 0062](../../../../docs/adr/0062-an-append-is-ensure-store-commit.md)'s
/// ensure, write and commit written once in Cove, expanded at every site by
/// `super::inline` and recognised by [`crate::legalize`], and a lowering
/// that emitted the window itself would be the second copy that module's
/// header says there is not.
#[derive(Clone, Copy, Debug)]
pub(super) struct Leaves {
    /// `std.stringbuilder.appendText(buffer, text)`: a whole `String`.
    pub(super) text: FunctionId,
    /// `std.stringbuilder.appendByteInto(buffer, value)`: one byte, which is
    /// what a one-character separator or bracket is.
    pub(super) byte: FunctionId,
    /// `std.int.renderInto(value, buffer)`: the decimal text of an `Int`,
    /// one digit at a time.
    pub(super) digits: FunctionId,
    /// `std.float.renderInto(value, buffer)`: the text of a `Float`, once a
    /// call site has asked for a walk that reaches one.
    ///
    /// Not resolved with the three above, because it is not needed by every
    /// walk: a program that renders a struct of `Int`s would otherwise have
    /// `std.float`'s writer, and `format` under it, in its slice and its
    /// literals in its heap. `Body::render_leaves_for` fills it in when
    /// [`reaches_a_scalar`] says the walk it is about to ask for meets a
    /// `Float`.
    pub(super) float: Option<FunctionId>,
    /// `std.duration.renderInto(value, buffer)`: the text of a `Duration`,
    /// in [`Leaves::float`]'s arrangement.
    pub(super) duration: Option<FunctionId>,
}

/// The trail a wording walk hands the wording walks it calls: the layout of a
/// `Vector<String>` and the five `std.dynamic` functions that make, lengthen,
/// measure, cut and read one. See [`Synth::phrase`].
///
/// [`Leaves`]' arrangement: resolving a name is `Plan`'s and `Body`'s, so the
/// call site that asks for a wording walk that can call another resolves
/// these first — [`needs_trail`] says whether it can — and the walk reads
/// them from the [`Pool`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Trail {
    /// `Vector<String>` in this program's layout table.
    pub(super) layout: LayoutId,
    /// `std.dynamic.newTrail() -> Vector<String>`.
    pub(super) new: FunctionId,
    /// `std.dynamic.trailPush(trail, piece)`.
    pub(super) push: FunctionId,
    /// `std.dynamic.trailLength(trail) -> Int`.
    pub(super) length: FunctionId,
    /// `std.dynamic.trailCut(trail, depth)`.
    pub(super) cut: FunctionId,
    /// `std.dynamic.trailText(trail) -> String`.
    pub(super) text: FunctionId,
}

/// Whether a wording walk of `layout` calls another rather than expanding
/// everything in place: whether its walk meets a layout it is already inside
/// — one that holds itself — or nests past [`NESTING`]. [`Synth::phrase`]'s
/// own question, asked of the layout table, so that the call site resolves a
/// [`Trail`] exactly where a walk will hand one on.
pub(super) fn needs_trail(shapes: &shapes::Shapes, layout: LayoutId) -> bool {
    fn parts(shapes: &shapes::Shapes, layout: LayoutId) -> Vec<LayoutId> {
        match &shapes.layout(layout).shape {
            Shape::Struct { fields, .. } => fields.iter().map(|field| field.layout).collect(),
            Shape::Enum { cases, .. } => cases
                .iter()
                .flat_map(|case| case.parts.iter().map(|part| part.layout))
                .collect(),
            Shape::Elements {
                elem,
                growable: false,
            } => vec![*elem],
            Shape::Entries { value, .. } => vec![*value],
            _ => Vec::new(),
        }
    }
    // The walk descends only into what is not admitted outright, and never
    // into a part it can settle from the layout; so it is the walkable layouts
    // it can reach that decide, with the longest chain of them below each.
    fn deepest(
        shapes: &shapes::Shapes,
        layout: LayoutId,
        on: &mut Vec<LayoutId>,
        memo: &mut std::collections::HashMap<LayoutId, Option<usize>>,
    ) -> Option<usize> {
        if let Some(known) = memo.get(&layout) {
            return *known;
        }
        if on.contains(&layout) {
            return None;
        }
        if admission(shapes.all(), layout) == Admission::Always
            || !walks(Operation::Admission, &shapes.layout(layout).shape)
        {
            return Some(0);
        }
        on.push(layout);
        let mut below = Some(0);
        for part in parts(shapes, layout) {
            below = match (below, deepest(shapes, part, on, memo)) {
                (Some(most), Some(this)) => Some(most.max(this)),
                _ => None,
            };
        }
        on.pop();
        let answer = below.map(|depth| depth + 1);
        memo.insert(layout, answer);
        answer
    }
    deepest(
        shapes,
        layout,
        &mut Vec::new(),
        &mut std::collections::HashMap::new(),
    )
    .is_none_or(|depth| depth > NESTING)
}

/// How a value of one layout becomes text.
///
/// [`ordered_by`]'s arrangement for the rendering: **one table, asked by the
/// call site and by the walk**, so that the two cannot disagree about which
/// of them writes a part. [`walks`]'s `Rendering` arm is this function and
/// nothing else, and so is `super::interpolate`'s choice of append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rendered {
    /// The bytes themselves, appended whole by [`Leaves::text`].
    ///
    /// A `String` renders as its bytes — **unquoted and unescaped**, which
    /// is what makes `"{name}"` interpolation rather than a debug form — so
    /// the whole of rendering one is the append `super::interpolate`'s
    /// `Ty::Str` arm already makes for a piece.
    Text,
    /// One digit at a time, by [`Leaves::digits`].
    Digits,
    /// `std.float.renderInto(value, buffer)`: a `Float`'s text, written in
    /// Cove since ADR 0068's Phase 4a.
    ///
    /// The shortest decimal that reads back as the value, and `.0` after a
    /// whole number, which `crates/cove-sema/std/float.cove` says in full.
    /// A walk reaches it through [`Leaves::float`], which the call site that
    /// asks for the walk fills in when [`reaches_a_scalar`] says it needs to.
    Float,
    /// `std.duration.renderInto(value, buffer)`: a `Duration`'s count in the
    /// largest unit that divides it exactly, and the unit's suffix.
    /// [`Rendered::Float`]'s arrangement.
    Duration,
    /// A walk [`Synth::rendering`] writes.
    Walk,
    /// `std.dynamic.renderInto`, the Cove walk over a view of a box.
    ///
    /// ADR 0064's Decision 4: a **layout that does not say what the value
    /// is**. A [`Shape::Boxed`] keeps its [`LayoutId`] in payload word 0, and
    /// there is nothing here for a walk to be directed by — so ADR 0068's
    /// Phase 4b-ii opens it and walks the view, in Cove, handing over the
    /// path of vectors the rendering that reached it is inside. It was the
    /// runtime's `Value.renderInto` until then, and before Phase 4a it was
    /// reached from a `Float` and a `Duration` too; a bare `Repr::Ref` word
    /// and a reclaimed run were on this list beside the box, and are walks
    /// now, since the one bare reference a value's layout can be is a
    /// function value's. `cove-cli`'s `tests/boxed.rs` holds every call of
    /// the function to an erased operand.
    Dynamic,
}

/// Which of the six [`Rendered`] a value of this shape is.
///
/// Asked of a [`Shape`] and not of a [`LayoutId`] because every answer is
/// decided by the family alone — which is what lets [`crate::verify`] ask it
/// of a finished program's layout table without reaching into this module's
/// [`Pool`].
pub(crate) fn rendered(shape: &Shape) -> Rendered {
    match shape {
        Shape::Str => Rendered::Text,
        Shape::Word(Repr::Int) => Rendered::Digits,
        Shape::Word(Repr::Float) => Rendered::Float,
        Shape::Word(Repr::Duration) => Rendered::Duration,
        // See [`Rendered::Dynamic`].
        Shape::Boxed => Rendered::Dynamic,
        // Everything else is a walk, a bare reference word included: the
        // one a value's layout is is a function value's, whose text is
        // `<fn>` whichever function it names — see [`Synth::rendering`] — and
        // a reclaimed run is a walk that refuses.
        _ => Rendered::Walk,
    }
}

/// The declared name of a layout, without the type arguments the
/// instantiation is identified by.
///
/// `crates/cove-runtime/src/vm/boundary.rs`'s `declared`, which is what the
/// rendering intrinsic reads a struct's name through, written here so that a
/// walk composed at lowering time spells the name the same way. The first
/// `<` after the first character, so that the layout table's own bracketed
/// names — `<free>`, `<synth>` — are left whole rather than reduced to
/// nothing.
fn declared(name: &str) -> &str {
    match name.char_indices().find(|(at, ch)| *at > 0 && *ch == '<') {
        Some((at, _)) => &name[..at],
        None => name,
    }
}

/// The declared name without its module, which is what a rendering shows.
///
/// `boundary::short`. It is applied *after* [`declared`] for the reason that
/// module gives: a layout's name carries the instantiation's type arguments,
/// and cutting at the last `.` of `m.Cell<m.Point>` would cut inside the
/// brackets (#407).
fn short(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Whether this struct layout is the builtin `Error`, which renders as the
/// message it carries rather than as the struct it happens to be.
///
/// The intrinsic's own test, by name and by the first field's name. Sound
/// because the name is the checker's and `Error` is a builtin a module
/// cannot redeclare — and a program that prints an error is printing what
/// went wrong, where `Error(message: x)` says the same thing twice.
fn is_error(name: &str, fields: &[Field]) -> bool {
    name == ERROR.name && fields.first().map(|field| &*field.name) == Some(MESSAGE_FIELD.name)
}

/// Whether this struct layout is the builtin `Range`, which renders as the
/// operator it was written with.
///
/// `boundary::is_range`, whole rather than by name alone: `1..3` and `1..<4`
/// cover the same values and are two renderings because they are two values.
fn is_range(shapes: &shapes::Shapes, layout: LayoutId, name: &str, fields: &[Field]) -> bool {
    let word = |at: usize, called: &str, repr: Repr| {
        fields
            .get(at)
            .is_some_and(|field| &*field.name == called && shapes.words(field.layout) == [repr])
    };
    name == RANGE.name
        && fields.len() == 3
        && shapes.words(layout) == [Repr::Int, Repr::Int, Repr::Bool]
        && word(0, "start", Repr::Int)
        && word(1, "end", Repr::Int)
        && word(2, "inclusive", Repr::Bool)
}

/// The comparison that orders a value of `layout` exactly as a key is
/// ordered, where one instruction can — and nothing at all where a walk is
/// needed.
///
/// `key::order` ranks an `Int` and a `Duration` by their signed words, a
/// `Bool` `false` first, and a `String` by its bytes — which are
/// [`Compare::Int`], [`Compare::Bool`] and [`Compare::Str`]. It ranks an
/// enum's cases by their *names*, and a payload-free enum's word is its case
/// *index*, so [`Compare::Tag`] is the order only where the cases were
/// declared in ascending name order; any other enum is a walk. A `Unit` has
/// one value and no comparison instruction admits it, so it is a walk too,
/// and so is everything wider than a word.
///
/// **This is the short circuit, and it is load-bearing.** `core.order` asks
/// it before anything else and emits the single [`Inst::Cmp`] where it
/// answers, which is why `cq` — a program whose every map key is a `String` —
/// calls no order walk at any site and on any turn. A synthesis that
/// displaced that instruction with a call to a function would be a
/// regression on the one program in this repository that exercises the path.
/// It lives here rather than beside its caller so that the walk and the call
/// site ask one question, in one place, of the same table: an arm this
/// answers is an arm [`Synth::order`] must not also make a function for.
pub(super) fn ordered_by(shapes: &shapes::Shapes, layout: LayoutId) -> Option<Compare> {
    match &shapes.layout(layout).shape {
        Shape::Word(Repr::Int | Repr::Duration) => Some(Compare::Int),
        Shape::Word(Repr::Bool) => Some(Compare::Bool),
        Shape::Str => Some(Compare::Str),
        Shape::Enum { cases, payload } if payload.is_empty() => cases
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name)
            .then_some(Compare::Tag),
        _ => None,
    }
}

/// Where each case of `cases` sits in the order the *names* put them in.
///
/// `key::order` compares an enum's cases by name and not by index, which is
/// the one place a statically known layout does not simply read the
/// discriminant: `Result` is declared `Ok` then `Err` and orders `Err`
/// first. So the walk needs the permutation, and where it is the identity —
/// which is the common case, and `Option`'s — the discriminants *are* the
/// ranks and one [`Compare::Tag`] order is the whole comparison.
fn ranks_of(cases: &[Case]) -> Vec<i64> {
    let mut sorted: Vec<usize> = (0..cases.len()).collect();
    sorted.sort_by(|one, other| (*cases[*one].name).cmp(&cases[*other].name));
    let mut ranks = vec![0; cases.len()];
    for (rank, at) in sorted.into_iter().enumerate() {
        ranks[at] = rank as i64;
    }
    ranks
}

/// Whether `layout` is one this module synthesizes a *function* for, rather
/// than one a call site compares in place.
///
/// The composite families and nothing else: a scalar, a string and a box are
/// one instruction or one intrinsic wherever they are met, so a function for
/// one would be a call around a single instruction. Everything here is a walk
/// of more than one part, which is what a function is for.
///
/// The two operations do not agree on the list, and the disagreement is the
/// language's rather than this module's. A `Vector` is a value `==` compares
/// element by element and it is **not a key**: ADR 0001 refuses a mutable
/// handle as one, because a key's equality must not change while a
/// collection holds it. A growable run is a `Vector`'s store and is not a
/// value either. So the order walks neither, and reaches them at
/// [`Synth::not_a_key`] instead — where the intrinsic reaches its own
/// refusal, in the same words.
pub(crate) fn walks(op: Operation, shape: &Shape) -> bool {
    match op {
        Operation::Equality | Operation::Tracked => matches!(
            shape,
            Shape::Struct { .. }
                | Shape::Enum { .. }
                | Shape::Elements { .. }
                | Shape::Vector { .. }
                | Shape::Members { .. }
                | Shape::Entries { .. }
        ),
        Operation::Order => matches!(
            shape,
            Shape::Struct { .. }
                | Shape::Enum { .. }
                | Shape::Elements {
                    growable: false,
                    ..
                }
                | Shape::Members { .. }
                | Shape::Entries { .. }
        ),
        // The admission's list leaves out the `Set`, and for a third reason
        // again: a set's members are keys by construction, so [`admission`]
        // answers [`Admission::Always`] for one and a call site never gets
        // this far. What is left is the four families whose parts have to be
        // looked at. The wording walk descends where the deciding one does.
        Operation::Admission | Operation::Description | Operation::DescribeAt => matches!(
            shape,
            Shape::Struct { .. }
                | Shape::Enum { .. }
                | Shape::Elements {
                    growable: false,
                    ..
                }
                | Shape::Entries { .. }
        ),
        // The rendering's list is a *complement* and not a list, because
        // every family has text and only a handful of them have text a walk
        // cannot compose. It is [`rendered`] and nothing else, for that
        // function's reason: the call site asks the same question of the
        // same table.
        //
        // So a `Bool` gets a function of six instructions and a `Unit` one
        // of two, where the other three operations would have compared them
        // in place. That is deliberate — `super::inline` expands a leaf that
        // small at every site and `super::sweep` stands the function down,
        // so what it costs is nothing, and what it buys is that the call
        // site has one arm instead of fifteen.
        Operation::Rendering | Operation::RenderTracked => {
            matches!(rendered(shape), Rendered::Walk)
        }
    }
}

/// Whether a walk of `op` over `layout` reaches a [`Shape::Boxed`] part — and
/// so whether it calls `std.dynamic.equals` from [`Synth::fallback`], for
/// [`Operation::Equality`], `std.dynamic.order` from [`Synth::dynamic`], for
/// [`Operation::Order`], or `std.dynamic.refusesKey` from [`Synth::admit`], for
/// [`Operation::Admission`].
///
/// Asked by the call site before it asks [`function_for`], because the walk
/// cannot resolve a name and the call site can: a layout this answers `true`
/// for has the function resolved onto the [`Pool`] first, and one it answers
/// `false` for asks for nothing — so a program whose comparisons and keys never
/// meet a box does not have the module in its slice at all. It follows exactly
/// the parts the walk descends into — a family [`walks`] answers `true` for,
/// under `op` — with a set of the layouts already seen so that one holding
/// itself terminates. The two operations do not descend into the same
/// families, and that is [`walks`]' disagreement again: an order never goes
/// inside a `Vector`, which is not a key, so a box inside one is not a box an
/// order reaches; and an admission never goes inside a `Set`, or a map's keys,
/// which are keys by construction.
pub(super) fn reaches_a_box(shapes: &shapes::Shapes, op: Operation, layout: LayoutId) -> bool {
    reaches(shapes, op, layout, |shape| matches!(shape, Shape::Boxed))
}

/// Whether an admission walk of `layout` reaches a Host resource handle outside
/// a box: whether the wording walk after it has a resource to name, which it
/// names by the key's type and not by the layout (see [`super::named`]).
pub(super) fn reaches_a_resource(shapes: &shapes::Shapes, layout: LayoutId) -> bool {
    reaches(shapes, Operation::Admission, layout, |shape| {
        *shape == Shape::Word(Repr::Host)
    })
}

/// Whether a rendering walk of `layout` reaches a part that is one word of
/// `repr` — and so whether it calls `std.float.renderInto`, for a
/// `Repr::Float`, or `std.duration.renderInto`, for a `Repr::Duration`.
///
/// [`reaches_a_box`]'s question, asked for [`Rendered::Float`] and
/// [`Rendered::Duration`] and for its reason: the walk cannot resolve a name,
/// so the call site resolves the callee first, and a program whose renderings
/// never meet the scalar does not have the function in its slice. The layout
/// itself counts, so a piece that *is* a `Float` answers `true`.
pub(super) fn reaches_a_scalar(shapes: &shapes::Shapes, layout: LayoutId, repr: Repr) -> bool {
    reaches(shapes, Operation::Rendering, layout, |shape| {
        *shape == Shape::Word(repr)
    })
}

/// Whether a walk of `op` over `layout` reaches a part `found` answers `true`
/// for, following exactly the parts the walk descends into.
fn reaches(
    shapes: &shapes::Shapes,
    op: Operation,
    layout: LayoutId,
    found: impl Fn(&Shape) -> bool,
) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![layout];
    while let Some(at) = pending.pop() {
        if !seen.insert(at) {
            continue;
        }
        let shape = &shapes.layout(at).shape;
        if found(shape) {
            return true;
        }
        if !walks(op, shape) {
            continue;
        }
        match shape {
            Shape::Struct { fields, .. } => pending.extend(fields.iter().map(|field| field.layout)),
            Shape::Enum { cases, .. } => pending.extend(
                cases
                    .iter()
                    .flat_map(|case| case.parts.iter().map(|part| part.layout)),
            ),
            Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
                pending.push(*elem)
            }
            // An admission asks about a map's values and never its keys, which
            // are keys by construction, so a box among the keys is not a box
            // it reaches.
            Shape::Entries { value, .. } if op == Operation::Admission => pending.push(*value),
            Shape::Entries { key, value } => pending.extend([*key, *value]),
            _ => {}
        }
    }
    false
}

/// Whether an equality walk of `layout` has to carry a path — whether a value
/// of it can contain itself.
///
/// # Only a `Vector` can close a cycle
///
/// Every other family an equality walk descends into is immutable once built —
/// a struct and an enum are values, and an `Array`, a `Set` and a `Map` are
/// never written after they are made — so a value can only come to hold itself
/// by being pushed into a vector it already holds. A function value, a
/// `Shared` cell, a task and every other opaque family is a leaf of the walk
/// (it is compared as a constant), so a cycle through one is not a cycle the
/// walk follows.
///
/// So a layout needs a path exactly when it lies on a cycle of the walk that
/// passes through a `Vector`: when it reaches some vector layout that reaches
/// it back. That is decided here from the layout table, over the parts
/// [`Synth::compare`] descends into — a struct's fields, every case's parts,
/// the element of a run, a set and a vector, and a map's entry (or, before the
/// entry's layout exists, its key and its value, which is what the entry
/// reaches) — and it is **exact** rather than cautious in both directions:
///
/// - A layout that answers `false` cannot hold itself, so its walk ends on
///   every value, and it is emitted exactly as it was before issue #493.
/// - A layout that answers `true` is on a cycle with every other layout on it,
///   so every function the cycle passes through carries the path, and the
///   path is never dropped between two vectors of one cycle. A layout that
///   merely *reaches* a cycle — `Array<Node>` over a recursive `Node` — is not
///   on it, and starts the path empty where it calls in: no vector pair above
///   it can be met again below it, because nothing below it reaches it.
///
/// **A box is a leaf here too**, and that is not caution either. A boxed part
/// is compared by `std.dynamic.equals`, which follows the box's contents with a
/// path of its own and never calls back into a walk composed here, so a cycle
/// that passes through a box is a cycle that walk finds. A static walk that
/// reaches a box has already handed everything below it over.
pub(super) fn tracked(pool: &mut Pool, layout: LayoutId) -> bool {
    if let Some(known) = pool.tracked.get(&layout) {
        return *known;
    }
    let shapes = &pool.shapes;
    let answer = walks(Operation::Equality, &shapes.layout(layout).shape)
        && reached(shapes, &parts_of(shapes, layout))
            .into_iter()
            .chain([layout])
            .any(|vector| {
                matches!(shapes.layout(vector).shape, Shape::Vector { .. })
                    && reached(shapes, &parts_of(shapes, vector)).contains(&layout)
            });
    pool.tracked.insert(layout, answer);
    answer
}

/// The layouts an equality walk of `layout` calls a walk of, or compares in
/// place: [`reaches_a_box`]'s parts, with a map's entry where it has one.
///
/// A map's walk compares its entries, so the entry layout is a node of the walk
/// in its own right. It is looked up rather than made — making it here would
/// intern a layout the program might not otherwise have had at this point, and
/// renumber the layouts after it — and where it does not exist yet, its key and
/// value stand in for it, which reach exactly what it would.
fn parts_of(shapes: &shapes::Shapes, layout: LayoutId) -> Vec<LayoutId> {
    match &shapes.layout(layout).shape {
        Shape::Struct { fields, .. } => fields.iter().map(|field| field.layout).collect(),
        Shape::Enum { cases, .. } => cases
            .iter()
            .flat_map(|case| case.parts.iter().map(|part| part.layout))
            .collect(),
        Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
            vec![*elem]
        }
        Shape::Entries { key, value } => match shapes.existing_entry(*key, *value) {
            Some(entry) => vec![entry],
            None => vec![*key, *value],
        },
        _ => Vec::new(),
    }
}

/// Every layout reachable from `from` through [`parts_of`], `from` included.
fn reached(shapes: &shapes::Shapes, from: &[LayoutId]) -> std::collections::HashSet<LayoutId> {
    reached_by(shapes, from, parts_of)
}

/// Every layout reachable from `from` through `parts`, `from` included.
fn reached_by(
    shapes: &shapes::Shapes,
    from: &[LayoutId],
    parts: fn(&shapes::Shapes, LayoutId) -> Vec<LayoutId>,
) -> std::collections::HashSet<LayoutId> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = from.to_vec();
    while let Some(at) = pending.pop() {
        if seen.insert(at) {
            pending.extend(parts(shapes, at));
        }
    }
    seen
}

/// Whether a rendering walk of `layout` has to carry the path of vectors it
/// is inside — [`tracked`]'s question, asked for the rendering.
///
/// # The rule
///
/// A layout's rendering is tracked when **either**
///
/// - it lies on a cycle of the rendering walk that passes through a
///   `Vector` — it reaches some vector layout that reaches it back, which is
///   [`tracked`]'s rule over the parts [`Synth::rendering`] descends into
///   ([`shown_parts`]); **or**
/// - its walk **reaches a box**.
///
/// So a `Vector` is tracked exactly when its element walk can reach the same
/// vector layout again, or reaches a box; and every layout between such a
/// vector and the place the path is read — the vector again, or the box — is
/// tracked too, so that the path is handed on and never dropped on the way.
///
/// # Why a box counts as reaching anything
///
/// For equality a box is a leaf, because `std.dynamic.equals` follows the
/// box's contents with a path of its own. The rendering does **not** restart
/// the path at a box — issue #499's decision 3: the path the static walk is
/// carrying continues into the dynamic renderer — and a box can hold a value
/// of any layout, including the vector a walk above it is inside. So a walk
/// that reaches a box is, as far as the layout table can tell, on a cycle
/// through whatever vector the box holds, and it carries the path there.
/// The rule asks only whether the box is reached, and not whether some vector
/// that reaches the box exists: that would be a question about layouts that
/// may not have been interned yet, and the answer here, like [`tracked`]'s, is
/// a fact about the layout and its parts alone, so it never changes.
///
/// **Since ADR 0068's Phase 4b-ii the box reads the path.** A box is
/// rendered by `std.dynamic.renderInto`, which is handed the path the static
/// walk is carrying — see [`Synth::below`] — and looks a vector up on it as
/// well as on its own. So a cycle that closes through a box renders its `[…]`
/// where the oracle's does, which walks the whole value as one path. In Phase
/// 4b-i the box started a path of its own and rendered the repeat one vector
/// later.
///
/// Every other layout — one on no cycle through a vector and reaching no box —
/// answers `false`, and its walk is emitted exactly as it was before issue
/// #493 reached the rendering.
pub(super) fn render_tracked(pool: &mut Pool, layout: LayoutId) -> bool {
    if let Some(known) = pool.render_tracked.get(&layout) {
        return *known;
    }
    let shapes = &pool.shapes;
    let answer = walks(Operation::Rendering, &shapes.layout(layout).shape) && {
        let below = reached_by(shapes, &shown_parts(shapes, layout), shown_parts);
        below
            .iter()
            .any(|at| matches!(shapes.layout(*at).shape, Shape::Boxed))
            || below.iter().copied().chain([layout]).any(|vector| {
                matches!(shapes.layout(vector).shape, Shape::Vector { .. })
                    && reached_by(shapes, &shown_parts(shapes, vector), shown_parts)
                        .contains(&layout)
            })
    };
    pool.render_tracked.insert(layout, answer);
    answer
}

/// The layouts a rendering walk of `layout` renders a part of — the parts
/// [`Synth::rendering`] hands to [`Synth::render`], and nothing else.
///
/// Not [`parts_of`], because the rendering does not descend where equality
/// does: an opaque struct renders as its name and nothing inside it, the
/// builtin `Error` as its message alone, a `Range` as two numbers; and a map
/// renders its key and its value one at a time rather than comparing whole
/// entries. A layout that is not a walk of its own — a scalar, a string, a
/// box — has no parts here, which is what makes a box a node the walk
/// reaches and never goes inside.
fn shown_parts(shapes: &shapes::Shapes, layout: LayoutId) -> Vec<LayoutId> {
    let described = shapes.layout(layout);
    if !walks(Operation::Rendering, &described.shape) {
        return Vec::new();
    }
    match &described.shape {
        Shape::Struct { opaque: true, .. } => Vec::new(),
        Shape::Struct { fields, .. } if is_error(&described.name, fields) => {
            vec![fields[0].layout]
        }
        Shape::Struct { fields, .. } if is_range(shapes, layout, &described.name, fields) => {
            Vec::new()
        }
        Shape::Struct { fields, .. } => fields.iter().map(|field| field.layout).collect(),
        Shape::Enum { cases, .. } => cases
            .iter()
            .flat_map(|case| case.parts.iter().map(|part| part.layout))
            .collect(),
        Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
            vec![*elem]
        }
        Shape::Entries { key, value } => vec![*key, *value],
        _ => Vec::new(),
    }
}

/// The function `op` lowers to at `layout`, synthesizing it if nothing has
/// asked for it before.
///
/// The number is taken and recorded **before** the body is walked, which is
/// what makes a recursive layout terminate: `struct Node { kids: Array<Node> }`
/// asks for `equals<Node>` from inside `equals<Node>`'s own walk and finds the
/// number rather than starting again. [`Body::instantiate`](super::dispatch)
/// terminates for the same reason and this is deliberately the same shape.
///
/// It needs no bound of its own beside that. The memo is keyed by a
/// [`LayoutId`], the layout table is finite and interned before any of this
/// runs, and a walk reaches only layouts that are already in it — so the
/// recursion is over a finite graph with every node visited once, where
/// `instantiate`'s is over types a body can *grow* and needs
/// [`gap::MAX_DEPTH`](super::gap) to stop.
pub(super) fn function_for(
    op: Operation,
    layout: LayoutId,
    pool: &mut Pool,
    decls: usize,
    span: Span,
) -> FunctionId {
    function_named_for(op, layout, None, pool, decls, span)
}

/// [`function_for`] for a wording walk of a key whose type holds a Host
/// resource: `named` is the node [`super::named`] made for the type, which
/// the walk names each resource in it by, and so part of what the function
/// is. `None` is the walk [`function_for`] answers.
///
/// Two keys of one layout whose types hold different resources — an
/// `Array<http.Server>` and an `Array<files.Reader>` — are two walks, because
/// the one word that tells them apart is a literal each walk emits.
pub(super) fn function_named_for(
    op: Operation,
    layout: LayoutId,
    named: Option<NamedId>,
    pool: &mut Pool,
    decls: usize,
    span: Span,
) -> FunctionId {
    if let Some(id) = pool.synthesized.get(&(op, layout, named)) {
        return *id;
    }
    let at = pool.appended.len();
    let id = FunctionId((decls + at) as u32);
    pool.appended.push(None);
    pool.synthesized.insert((op, layout, named), id);

    // A layout's name is not unique — every `Array<T>` is called `Array` —
    // so the id goes in the name as well. It is read by a listing, a profile
    // row and a backtrace, and each of those wants to know *which* `Array`.
    //
    // A walk that names resources carries its node's number too, for the same
    // reason: two such walks of one layout are two functions.
    let named_as = named.map_or(String::new(), |named| format!("@{}", named.index()));
    let name: Arc<str> = Arc::from(format!(
        "{}<{}#{}{named_as}>",
        op.verb(),
        pool.shapes.layout(layout).name,
        layout.0
    ));

    // The parameters are laid out from slot 0 in order, each as wide as its
    // layout, and the answer sits after the last of them.
    let params = op.params(layout, pool.trail.map(|trail| trail.layout));
    let mut reprs: Vec<Repr> = Vec::new();
    let mut taken: Vec<Slot> = Vec::with_capacity(params.len());
    for param in &params {
        taken.push(reprs.len() as Slot);
        reprs.extend_from_slice(pool.shapes.words(*param));
    }
    let answer = reprs.len() as Slot;
    reprs.extend_from_slice(pool.shapes.words(op.answers()));
    let mut synth = Synth {
        pool,
        op,
        decls,
        span,
        reprs,
        code: Vec::new(),
        leaves: Vec::new(),
        answer,
        settled: None,
        literal: None,
        punctuation: None,
        path: None,
        words: None,
        named,
    };
    if op == Operation::Equality && tracked(synth.pool, layout) {
        synth.start_path(layout, &taken);
    } else if op == Operation::Rendering && render_tracked(synth.pool, layout) {
        synth.start_render_path(layout, &taken);
    } else {
        synth.body(layout, &taken);
    }
    let end = synth.here();
    for at in std::mem::take(&mut synth.leaves) {
        synth.patch(at, end);
    }
    synth.emit(Inst::Return { src: answer });
    // A wording walk's refusal is written once, after the return, and every
    // refused part it can name jumps to it: see [`Synth::refuse_with`].
    synth.worded();

    let function = Function {
        module: Arc::from(MODULE),
        name,
        params,
        refs: RefMap::of(&synth.reprs),
        reprs: synth.reprs,
        returns: op.answers(),
        captures: Vec::new(),
        spans: vec![span; synth.code.len()],
        code: synth.code,
        locals: Vec::new(),
        inlined: Vec::new(),
        span,
        is_async: false,
        stub: false,
    };
    pool.appended[at] = Some(function);
    id
}

/// A loop over the positions two runs both have, and the two lengths that
/// decide the order if every one of them agreed.
///
/// Carried between [`Synth::shorter`] and [`Synth::lengths_decide`] because
/// the element comparison is written between them and the lengths are
/// compared *after* it.
struct Both {
    /// The length of the left run.
    left: Slot,
    /// The length of the right run.
    right: Slot,
    /// The position being compared.
    index: Slot,
    /// Where the next turn begins.
    head: Pc,
    /// The two `branch-false`s that leave the loop, one per run.
    exits: [Pc; 2],
}

/// A loop over the units of one run, carried between [`Synth::over`] and
/// [`Synth::around`] because the unit is read and asked about between them.
struct Each {
    /// The position being looked at.
    index: Slot,
    /// Where the next turn begins.
    head: Pc,
    /// The `branch-false` that leaves the loop.
    done: Pc,
}

/// One synthesized function being written.
struct Synth<'p> {
    pool: &'p mut Pool,
    /// Which layout-directed operation is being composed.
    ///
    /// Read by every part of the walk rather than by [`Synth::body`] alone:
    /// a part that is itself a walk asks for a function of the *same*
    /// operation, so this is what makes `order<Array<Row>>` call
    /// `order<Row>` and not `equals<Row>`.
    op: Operation,
    /// How many declarations the program has, which is where the appended
    /// functions are numbered from.
    decls: usize,
    /// The site that first asked for this function.
    ///
    /// A synthesized function is shared by every site that compares the same
    /// layout, so there is no one place it came from; this is the first, and
    /// it is what a runtime error inside the walk is blamed on.
    span: Span,
    reprs: Vec<Repr>,
    code: Vec<Inst>,
    /// Where every jump that leaves with the answer already written sits.
    ///
    /// The walk is a conjunction, and the whole of its control flow is "stop
    /// as soon as something is not equal". Each of these is patched to the
    /// [`Inst::Return`] once the body is finished and its program counter is
    /// known.
    leaves: Vec<Pc>,
    /// The one slot the answer is written into, by every arm.
    answer: Slot,
    /// The one `String` slot a rendering walk loads each of its literals
    /// into, made on first use and reused by every literal after it.
    ///
    /// [`Synth::settled`]'s arrangement, for the same reason and with one
    /// more: nothing reads the slot but the `call` on the very next
    /// instruction, so a walk of ten literals costs one word rather than
    /// ten — and the word is a `Repr::Ref`, so ten of them would be ten
    /// entries in the frame's [`crate::repr::RefMap`] for the collector to
    /// read at every safepoint of a walk that can collect.
    literal: Option<Slot>,
    /// The one `Int` slot a rendering walk loads each of its one-byte
    /// literals into. [`Synth::literal`]'s counterpart for a bracket, a
    /// space or a comma, which `appendByteInto` takes as a number.
    punctuation: Option<Slot>,
    /// The one `Bool` a three-valued walk tests its answer with, made on
    /// first use and reused by every test after it.
    ///
    /// [`Operation::Order`]'s early exit is "leave unless the answer is
    /// `0`", which is a comparison against an immediate and a branch on what
    /// it wrote. Nothing reads that `Bool` but the branch on the very next
    /// instruction — `super::peephole` fuses the pair for exactly that
    /// reason — so one slot serves the whole function, and a walk of ten
    /// fields costs one word rather than nine.
    settled: Option<Slot>,
    /// What a tracked walk hands the tracked walks it calls: see [`Path`].
    ///
    /// `None` in every walk but an [`Operation::Tracked`] or an
    /// [`Operation::RenderTracked`] one, which is what keeps every other walk
    /// what it was — [`Synth::compare`] reads this before it asks [`tracked`],
    /// and a walk with no path calls `equals<L>` where one with a path calls
    /// `tracks<L>`; [`Synth::render`] reads it before it asks
    /// [`render_tracked`], and calls `renders<L>` or `rendersTracked<L>`.
    path: Option<Path>,
    /// What a wording walk words its refusal with: see [`Words`]. `None` in
    /// every walk but an [`Operation::Description`] or an
    /// [`Operation::DescribeAt`] one.
    words: Option<Words>,
    /// The node of the key's type a wording walk names the Host resources in
    /// its value by: see [`function_named_for`]. `None` in every other walk,
    /// and in a wording walk of a value that holds no resource.
    named: Option<NamedId>,
}

/// The slots a wording walk writes its refusal out of, and the jumps to the
/// refusal it writes once at its end.
///
/// A refusal is `` `{method}` cannot use a `{type}` inside `{path}` as a
/// {role} `` and a rule and a help, and all but the path are known the moment
/// the walk meets the refused part: the two names are the walk's parameters,
/// and the type, the rule and the help are the part's layout's. So each part
/// the walk can name sets three strings and jumps, and the sentence is
/// assembled at one place rather than at each of them.
#[derive(Clone, Debug)]
struct Words {
    /// `core.admitKey`'s method, a parameter.
    method: Slot,
    /// And its role, a parameter.
    role: Slot,
    /// The byte buffer the path to the refused part is spelled into: made by
    /// `describes<L>` before it walks, and by `describesAt<L>` at the one part
    /// it names.
    path: Slot,
    /// The trail a `describesAt<L>` was handed — the path down to its value —
    /// and `None` in `describes<L>`, whose value is the key. See
    /// [`Synth::phrase`].
    trail: Option<Slot>,
    /// What stands between the method and the path, type included:
    /// `` ` cannot use a `Float` inside ` ``.
    lead: Slot,
    /// The rule the part is refused under.
    rule: Slot,
    /// And the help.
    help: Slot,
    /// Every jump to the refusal, patched once it is written.
    refusals: Vec<Pc>,
}

/// One step of the path a wording walk names a refused part by, carried down
/// the walk and written only at the part: see [`Synth::describing`].
#[derive(Clone, Debug)]
enum Piece {
    /// Text the layout says: a struct's name at the root, `.field`, `(0)`.
    Text(String),
    /// An element of an array, `[i]`, by the slot its index is in.
    Index(Slot),
    /// A value of a map, `[{key}]`, by the layout of the map's key and the
    /// slot the entry — key first — is in.
    Key(LayoutId, Slot),
}

/// The two words a tracked walk forwards to the tracked walks it calls.
#[derive(Clone, Copy, Debug)]
struct Path {
    /// The address of the innermost entry on the path.
    ///
    /// For equality, word 0 of a `tracks<Vector<…>>` frame, whose first three
    /// words are the pair's left vector, its right vector and the address of
    /// the pair before it. For the rendering, word 0 of a
    /// `rendersTracked<Vector<…>>` frame, whose first three words are the
    /// vector, the buffer and the address of the entry before it. Either way
    /// word 2 is the link, which is what one walk down the path follows.
    at: Slot,
    /// How many entries the path holds. Nought at the root, where `at` is an
    /// address nothing reads.
    depth: Slot,
}

impl Synth<'_> {
    // ---- emitting -------------------------------------------------------

    fn here(&self) -> Pc {
        self.code.len() as Pc
    }

    fn emit(&mut self, inst: Inst) -> Pc {
        let at = self.here();
        self.code.push(inst);
        at
    }

    /// A run of slots for a value of `layout`, taken from the end of the
    /// frame and never given back.
    ///
    /// There is no free list here, and that is a decision rather than an
    /// omission: a synthesized body is a few dozen instructions with a
    /// handful of temporaries, and [`super::frame::Frame`]'s reuse exists for
    /// a body as long as the source that wrote it. What a frame costs is
    /// words, and these frames are the narrowest in the program.
    fn alloc(&mut self, layout: LayoutId) -> Slot {
        let at = self.reprs.len() as Slot;
        let words = self.pool.shapes.words(layout).to_vec();
        self.reprs.extend_from_slice(&words);
        at
    }

    /// Leaves the walk where the answer so far is `false`.
    fn leave_unless(&mut self) {
        let at = self.emit(Inst::BranchFalse {
            cond: self.answer,
            to: PENDING,
        });
        self.leaves.push(at);
    }

    /// Leaves the walk whatever the answer is.
    fn leave(&mut self) {
        let at = self.emit(Inst::Jump { to: PENDING });
        self.leaves.push(at);
    }

    fn patch(&mut self, at: Pc, to: Pc) {
        match &mut self.code[at as usize] {
            Inst::Jump { to: target }
            | Inst::BranchFalse { to: target, .. }
            | Inst::CmpBranch { target, .. }
            | Inst::CmpImmBranch { target, .. } => *target = to,
            other => panic!("a synthesized walk patched {other:?}, which is not a jump"),
        }
    }

    // ---- the composition ------------------------------------------------

    /// The whole of one synthesized function's body: the layout's parts, in
    /// the order the language compares them.
    ///
    /// One arm per operation, and each of them a walk of the same [`Shape`]
    /// table. They are written out separately rather than parameterised
    /// because what differs is not a comparison but the *shape of the
    /// control flow*: equality is a conjunction that stops at the first
    /// difference and answers a `Bool`, and an order is a chain that stops at
    /// the first part that is not equal and answers the sign that part gave.
    fn body(&mut self, layout: LayoutId, taken: &[Slot]) {
        match self.op {
            Operation::Equality => self.equality(layout, taken[0], taken[1]),
            Operation::Order => self.ordering(layout, taken[0], taken[1]),
            Operation::Admission => self.admission(layout, taken[0]),
            Operation::Rendering => self.rendering(layout, taken[0], taken[1]),
            Operation::Tracked => self.tracking(layout, taken),
            Operation::RenderTracked => self.render_tracking(layout, taken),
            Operation::Description => self.describing(layout, taken, None),
            Operation::DescribeAt => self.describing(layout, taken, Some(taken[3])),
        }
    }

    // ---- `a == b` -------------------------------------------------------

    /// Two values of one layout, part for part, stopping at the first that
    /// differs.
    fn equality(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let shape = self.pool.shapes.layout(layout).shape.clone();
        match shape {
            // Fields in declaration order, each where it sits. A field of an
            // inline value is a static word offset, so a nested struct costs
            // no load at all — the walk descends by slot arithmetic.
            Shape::Struct { fields, .. } => {
                let parts: Vec<(LayoutId, u32)> = fields
                    .iter()
                    .map(|field| (field.layout, field.at))
                    .collect();
                self.parts(&parts, a, b);
            }
            // The case, and then the payload the case names.
            Shape::Enum { cases, .. } => self.enumeration(&cases, a, b),
            // A run of elements is its header length and then the elements.
            // An `Array` and a `Set` are both the run itself; the two differ
            // in what their construction promised about the order, which is
            // part of the value and so is compared by comparing positions.
            Shape::Elements { elem, .. } | Shape::Members { elem } => {
                let length = self.alloc(shapes::INT);
                self.emit(Inst::Len {
                    dst: length,
                    obj: a,
                });
                let counterpart = self.alloc(shapes::INT);
                self.emit(Inst::Len {
                    dst: counterpart,
                    obj: b,
                });
                self.lengths(length, counterpart);
                self.walk(elem, a, b, length);
            }
            // A map is a run of entries, each the key's words then the
            // value's — which is `MapEntry`'s own layout, and the reason a
            // `for` over a map is one `LoadElem` at that width.
            Shape::Entries { key, value } => {
                let entry = self.pool.shapes.entry_of(key, value);
                let length = self.alloc(shapes::INT);
                self.emit(Inst::Len {
                    dst: length,
                    obj: a,
                });
                let counterpart = self.alloc(shapes::INT);
                self.emit(Inst::Len {
                    dst: counterpart,
                    obj: b,
                });
                self.lengths(length, counterpart);
                self.walk(entry, a, b, length);
            }
            // A vector's length is its own word 0 and not its store's header,
            // which is the capacity: ADR 0052's "capacity is not an `Array`
            // length" holding here exactly as it holds in `length()`.
            Shape::Vector { elem } => {
                let store = self.pool.shapes.store_of(elem);
                let length = self.alloc(shapes::INT);
                self.emit(Inst::LoadField {
                    dst: length,
                    obj: a,
                    at: shapes::VECTOR_LEN,
                    layout: shapes::INT,
                });
                let counterpart = self.alloc(shapes::INT);
                self.emit(Inst::LoadField {
                    dst: counterpart,
                    obj: b,
                    at: shapes::VECTOR_LEN,
                    layout: shapes::INT,
                });
                self.lengths(length, counterpart);
                let held = self.alloc(store);
                self.emit(Inst::LoadField {
                    dst: held,
                    obj: a,
                    at: shapes::VECTOR_STORE,
                    layout: store,
                });
                let counterheld = self.alloc(store);
                self.emit(Inst::LoadField {
                    dst: counterheld,
                    obj: b,
                    at: shapes::VECTOR_STORE,
                    layout: store,
                });
                self.walk(elem, held, counterheld, length);
            }
            // Nothing else is a walk — see [`walks`], which is what decides
            // that a function is made at all — so this is the one-instruction
            // answer, written here so the match is total and an arm added to
            // [`Shape`] later has to be thought about in one place.
            _ => self.compare(layout, a, b),
        }
    }

    /// Two lengths, and a leave where they differ.
    fn lengths(&mut self, a: Slot, b: Slot) {
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Eq,
            dst: self.answer,
            a,
            b,
        });
        self.leave_unless();
    }

    /// Each part at its offset, stopping at the first that differs.
    ///
    /// The answer of the *last* part is the answer of the whole, so it needs
    /// no branch after it: a walk of four fields is four comparisons and
    /// three branches. A layout with no parts at all is `true`, which is what
    /// `()` and a case with no payload are.
    fn parts(&mut self, parts: &[(LayoutId, u32)], a: Slot, b: Slot) {
        let Some((last, rest)) = parts.split_last() else {
            self.emit(Inst::Bool {
                dst: self.answer,
                value: true,
            });
            return;
        };
        for (layout, at) in rest {
            self.compare(*layout, a + *at as Slot, b + *at as Slot);
            self.leave_unless();
        }
        self.compare(last.0, a + last.1 as Slot, b + last.1 as Slot);
    }

    /// The case, and then the parts that case names.
    ///
    /// Word 0 of an enum is its discriminant, and two values of *one* layout
    /// are two values of one declared enum — so the case is compared as the
    /// word it is, where the runtime walk compares case *names* because its
    /// two operands may be two instantiations of one declaration. That is the
    /// whole of what a statically known layout buys here, and it is
    /// [`ordered_by`]'s [`Compare::Tag`] again — though the *order* pays more
    /// for it, because the case names decide there and a declaration written
    /// out of name order needs the permutation [`ranks_of`] builds.
    ///
    /// An enum whose every case is payload-free needs nothing more: the
    /// discriminant *is* the value, which is why `super::expr`'s
    /// `is_case_index` already sends one to an instruction and never here.
    fn enumeration(&mut self, cases: &[Case], a: Slot, b: Slot) {
        self.emit(Inst::Cmp {
            on: Compare::Tag,
            op: CmpOp::Eq,
            dst: self.answer,
            a,
            b,
        });
        if cases.iter().all(|case| case.parts.is_empty()) {
            return;
        }
        self.leave_unless();
        let switch = self.emit(Inst::Switch {
            on: a,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(cases.len());
        for case in cases {
            targets.push(self.here());
            // A part's offset is within the payload region, which begins
            // after the discriminant.
            let parts: Vec<(LayoutId, u32)> = case
                .parts
                .iter()
                .map(|part| (part.layout, 1 + part.at))
                .collect();
            if parts.is_empty() {
                // The two discriminants already agreed and there is nothing
                // else to this case, so the answer standing is the answer.
                self.leave();
                continue;
            }
            self.parts(&parts, a, b);
            self.leave();
        }
        // A discriminant no case names is not a value this function compares
        // equal to anything. Nothing a valid program holds reaches it — the
        // machine bounds-checks what it reads out of an object rather than
        // taking the lowering's word for it, which is the same reason a
        // `match` the checker proved exhaustive still carries a default.
        let default = self.emit(Inst::Bool {
            dst: self.answer,
            value: false,
        });
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted three lines above");
        };
        *held = table;
    }

    /// `len` elements of `elem`, at the same index in both runs.
    ///
    /// The loop is ordinary IR, so the work it does is charged the way every
    /// other loop's is — one unit per instruction, at a safepoint every
    /// stride — where the intrinsic charged `Effects::BULK_WORK` and reported
    /// what it had examined afterwards. ADR 0040's bounds apply to it because
    /// they apply to every back edge, and not because this asked for them.
    fn walk(&mut self, elem: LayoutId, a: Slot, b: Slot, len: Slot) {
        let index = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: index,
            value: 0,
        });
        let head = self.here();
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: index,
            b: len,
        });
        let done = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        let held = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: a,
            index,
            layout: elem,
        });
        let counterpart = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: counterpart,
            obj: b,
            index,
            layout: elem,
        });
        self.compare(elem, held, counterpart);
        self.leave_unless();
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: index,
            a: index,
            value: 1,
        });
        self.emit(Inst::Jump { to: head });
        // The answer standing when the loop runs out is the last element's,
        // or — for two empty runs — the lengths', and both are `true`.
        let at = self.here();
        self.patch(done, at);
    }

    /// One part of a walk: an instruction where the layout is one, a call
    /// where it is a walk of its own, and the one fallback where it is a box.
    fn compare(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let shape = self.pool.shapes.layout(layout).shape.clone();
        if walks(self.op, &shape) {
            if let Some(path) = self.path {
                if tracked(self.pool, layout) {
                    let callee =
                        function_for(Operation::Tracked, layout, self.pool, self.decls, self.span);
                    let args = self.pool.args.intern(vec![
                        Arg { slot: a, layout },
                        Arg { slot: b, layout },
                        Arg {
                            slot: path.at,
                            layout: shapes::ADDR,
                        },
                        Arg {
                            slot: path.depth,
                            layout: shapes::INT,
                        },
                    ]);
                    self.emit(Inst::Call {
                        dst: self.answer,
                        callee,
                        args,
                    });
                    return;
                }
            }
            let callee = function_for(
                Operation::Equality,
                layout,
                self.pool,
                self.decls,
                self.span,
            );
            let args = self
                .pool
                .args
                .intern(vec![Arg { slot: a, layout }, Arg { slot: b, layout }]);
            self.emit(Inst::Call {
                dst: self.answer,
                callee,
                args,
            });
            return;
        }
        match shape {
            // A typed comparison per scalar, which is the point of the
            // exercise: `Compare::Float` is IEEE-754 equality, so a `NaN` is
            // equal to nothing and `-0.0` is equal to `0.0`, where comparing
            // the two words as words gets both of them wrong.
            Shape::Word(Repr::Bool) => self.instruction(Compare::Bool, a, b),
            Shape::Word(Repr::Int | Repr::Duration) => self.instruction(Compare::Int, a, b),
            Shape::Word(Repr::Float) => self.instruction(Compare::Float, a, b),
            Shape::Word(Repr::Tag) => self.instruction(Compare::Tag, a, b),
            Shape::Str => self.instruction(Compare::Str, a, b),
            // `()` has one value.
            Shape::Word(Repr::Unit) => self.constant(true),
            // The erased case, and the only fallback there is.
            Shape::Boxed => self.fallback(layout, a, b),
            // Everything left is a family the language gives no equality:
            // a function value and the closure environment behind it, a
            // `Shared` cell, a host resource handle, a task, a task scope, an
            // address, and the two byte-run shapes no source expression
            // produces. `equal.rs`'s walk falls through to `false` for every
            // one of them, and so does `Value::eq_value` beside it — a
            // closure "is not equal to anything, itself included".
            _ => self.constant(false),
        }
    }

    fn instruction(&mut self, on: Compare, a: Slot, b: Slot) {
        self.emit(Inst::Cmp {
            on,
            op: CmpOp::Eq,
            dst: self.answer,
            a,
            b,
        });
    }

    fn constant(&mut self, value: bool) {
        self.emit(Inst::Bool {
            dst: self.answer,
            value,
        });
    }

    // ---- `core.order(a, b)` ---------------------------------------------

    /// Two values of one layout, part for part, stopping at the first part
    /// that is not equal — and answering the sign *that* part gave.
    ///
    /// The families are the same as equality's and three of them are walked
    /// differently, each because `key::order` walks them differently:
    ///
    /// - an **enum** orders by its case *name* and not by its index, so the
    ///   discriminant is a rank rather than a number (see [`ranks_of`]);
    /// - a **run** compares its elements over the length both have and its
    ///   *lengths last*, where equality compares lengths first and stops;
    ///   `[1]` sorts before `[1, 0]`, and both before `[2]`;
    /// - a **map** compares key before value within each entry, which is one
    ///   `load-elem` at the entry layout's width and two comparisons at two
    ///   offsets inside it.
    ///
    /// A struct is fields in declaration order, exactly as equality's is: the
    /// runtime walk compares field *names* first, and two values of one
    /// layout are two values of one declaration, so every name comparison it
    /// makes is equal and the fields themselves decide. That is the same
    /// thing a statically known layout buys at the enum's discriminant, in
    /// the one place it does not need a permutation to spend it.
    fn ordering(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let described = self.pool.shapes.layout(layout);
        let name = described.name.clone();
        let shape = described.shape.clone();
        match shape {
            Shape::Struct { fields, .. } => {
                let parts: Vec<(LayoutId, u32)> = fields
                    .iter()
                    .map(|field| (field.layout, field.at))
                    .collect();
                self.ranked(&parts, a, b);
            }
            Shape::Enum { cases, .. } => self.ranking(&cases, &name, a, b),
            Shape::Elements {
                elem,
                growable: false,
            }
            | Shape::Members { elem } => self.sequence(elem, a, b),
            Shape::Entries { key, value } => self.entries(key, value, a, b),
            // Nothing else is a walk — see [`walks`] — so this is the
            // one-instruction answer, the constant, the refusal or the box,
            // written in one place for the reason [`Synth::equality`]'s
            // fall-through is.
            _ => self.order(layout, a, b),
        }
    }

    /// Each part at its offset, stopping at the first that is not equal.
    ///
    /// [`Synth::parts`] with a three-valued answer, and the *same* shape: the
    /// last part's answer is the answer of the whole, so it needs no branch
    /// after it. A layout with no parts at all is equal, which is what `()`
    /// and a case with no payload are.
    fn ranked(&mut self, parts: &[(LayoutId, u32)], a: Slot, b: Slot) {
        let Some((last, rest)) = parts.split_last() else {
            self.equal();
            return;
        };
        for (layout, at) in rest {
            self.order(*layout, a + *at as Slot, b + *at as Slot);
            self.leave_unless_settled();
        }
        self.order(last.0, a + last.1 as Slot, b + last.1 as Slot);
    }

    /// The case's rank, and then the parts that case names.
    ///
    /// Where equality compares the discriminant as the word it is, an order
    /// compares the case *name* — so the word is a rank, and the two agree
    /// only where the cases were declared in ascending name order.
    /// [`ordered_by`] already answered the layouts where they agree *and*
    /// there is no payload, with one `Compare::Tag` order and no function at
    /// all; what is left here is an enum with a payload, whose discriminants
    /// may still be its ranks, and an enum whose declaration is out of name
    /// order, whose are not. The first is one instruction and the second is
    /// two `switch`es into a constant apiece, which is the permutation
    /// written as the only structural read the instruction set has.
    fn ranking(&mut self, cases: &[Case], name: &str, a: Slot, b: Slot) {
        let ranks = ranks_of(cases);
        if ranks
            .iter()
            .enumerate()
            .all(|(at, rank)| at as i64 == *rank)
        {
            self.emit(Inst::Cmp {
                on: Compare::Tag,
                op: CmpOp::Order,
                dst: self.answer,
                a,
                b,
            });
        } else {
            let left = self.rank(&ranks, name, a);
            let right = self.rank(&ranks, name, b);
            self.emit(Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Order,
                dst: self.answer,
                a: left,
                b: right,
            });
        }
        if cases.iter().all(|case| case.parts.is_empty()) {
            return;
        }
        self.leave_unless_settled();
        let switch = self.emit(Inst::Switch {
            on: a,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(cases.len());
        for case in cases {
            targets.push(self.here());
            let parts: Vec<(LayoutId, u32)> = case
                .parts
                .iter()
                .map(|part| (part.layout, 1 + part.at))
                .collect();
            if parts.is_empty() {
                // The two ranks already agreed and there is nothing else to
                // this case, so the answer standing — equal — is the answer.
                self.leave();
                continue;
            }
            self.ranked(&parts, a, b);
            self.leave();
        }
        let default = self.here();
        self.wrong_case(name);
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted a few lines above");
        };
        *held = table;
    }

    /// The rank of the case the value at `at` is in, as an `Int` slot.
    ///
    /// One `switch` into one [`Inst::Int`] per case. A discriminant no case
    /// names is [`Synth::wrong_case`]'s, which is what the runtime walk
    /// answers for the same word and in the same sentence — and it is the
    /// same reading of a `switch` default that [`Synth::enumeration`]'s
    /// takes: the machine bounds-checks what it reads out of an object
    /// rather than taking the lowering's word for it.
    fn rank(&mut self, ranks: &[i64], name: &str, at: Slot) -> Slot {
        let dst = self.alloc(shapes::INT);
        let switch = self.emit(Inst::Switch {
            on: at,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(ranks.len());
        let mut ends = Vec::with_capacity(ranks.len());
        for rank in ranks {
            targets.push(self.here());
            self.emit(Inst::Int { dst, value: *rank });
            ends.push(self.emit(Inst::Jump { to: PENDING }));
        }
        let default = self.here();
        self.wrong_case(name);
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted a few lines above");
        };
        *held = table;
        let join = self.here();
        for end in ends {
            self.patch(end, join);
        }
        dst
    }

    /// An `Array`'s elements or a `Set`'s members, position by position over
    /// the length both runs have, and then the two lengths.
    ///
    /// A `Set`'s members are already ascending, so the two families are one
    /// walk here exactly as they are one walk in `key::order` — what a `Set`
    /// promised about its order is part of the value, and comparing
    /// positions is how that promise is read.
    fn sequence(&mut self, elem: LayoutId, a: Slot, b: Slot) {
        let both = self.shorter(a, b);
        let held = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: a,
            index: both.index,
            layout: elem,
        });
        let counterpart = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: counterpart,
            obj: b,
            index: both.index,
            layout: elem,
        });
        self.order(elem, held, counterpart);
        self.leave_unless_settled();
        self.lengths_decide(both);
    }

    /// A `Map`'s entries, key before value, and then the two lengths.
    ///
    /// The entry is `MapEntry`'s own inline layout — the key's words then the
    /// value's — so one `load-elem` at that width reads both halves and the
    /// value's offset inside it is the key's width. That is the same
    /// `Shapes::entry_of` equality's walk reads a map with, at the one place
    /// the two differ: the halves are compared in turn rather than as one
    /// part.
    fn entries(&mut self, key: LayoutId, value: LayoutId, a: Slot, b: Slot) {
        let entry = self.pool.shapes.entry_of(key, value);
        let keys = self.pool.shapes.words(key).len() as Slot;
        let both = self.shorter(a, b);
        let held = self.alloc(entry);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: a,
            index: both.index,
            layout: entry,
        });
        let counterpart = self.alloc(entry);
        self.emit(Inst::LoadElem {
            dst: counterpart,
            obj: b,
            index: both.index,
            layout: entry,
        });
        self.order(key, held, counterpart);
        self.leave_unless_settled();
        self.order(value, held + keys, counterpart + keys);
        self.leave_unless_settled();
        self.lengths_decide(both);
    }

    /// The head of a loop over the positions two runs both have.
    ///
    /// Two lengths, an index, and two `branch-false`s rather than a computed
    /// minimum: a minimum would be a comparison, a branch and a copy, and
    /// this is two comparisons and two branches with no slot to hold the
    /// answer in. The exits are left for [`Synth::lengths_decide`], which is
    /// where the lengths are finally compared — **after** the elements,
    /// which is the whole difference between an order over a run and an
    /// equality over one.
    fn shorter(&mut self, a: Slot, b: Slot) -> Both {
        let left = self.alloc(shapes::INT);
        self.emit(Inst::Len { dst: left, obj: a });
        let right = self.alloc(shapes::INT);
        self.emit(Inst::Len { dst: right, obj: b });
        let index = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: index,
            value: 0,
        });
        let head = self.here();
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: index,
            b: left,
        });
        let one = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: index,
            b: right,
        });
        let other = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        Both {
            left,
            right,
            index,
            head,
            exits: [one, other],
        }
    }

    /// The end of that loop's turn, and what decides a run whose compared
    /// positions all agreed: which of the two is longer.
    ///
    /// A prefix sorts before its extension, and two runs of one length that
    /// agreed everywhere are equal — both of which are `Vec`'s own `Ord`,
    /// which is what every `MapKey` variant holding a run is ordered by.
    fn lengths_decide(&mut self, both: Both) {
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: both.index,
            a: both.index,
            value: 1,
        });
        self.emit(Inst::Jump { to: both.head });
        let at = self.here();
        for exit in both.exits {
            self.patch(exit, at);
        }
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Order,
            dst: self.answer,
            a: both.left,
            b: both.right,
        });
    }

    /// One part of an order: an instruction where the layout is ordered by
    /// one, a call where it is a walk of its own, the constant where it has
    /// one value, the one fallback where it is a box, and a refusal where it
    /// is not a key at all.
    fn order(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        if let Some(on) = ordered_by(&self.pool.shapes, layout) {
            self.emit(Inst::Cmp {
                on,
                op: CmpOp::Order,
                dst: self.answer,
                a,
                b,
            });
            return;
        }
        let shape = self.pool.shapes.layout(layout).shape.clone();
        if walks(Operation::Order, &shape) {
            let callee = function_for(Operation::Order, layout, self.pool, self.decls, self.span);
            let args = self
                .pool
                .args
                .intern(vec![Arg { slot: a, layout }, Arg { slot: b, layout }]);
            self.emit(Inst::Call {
                dst: self.answer,
                callee,
                args,
            });
            return;
        }
        match shape {
            // `()` has one value, and no comparison instruction takes a
            // `Repr::Unit` — which is why `ordered_by` answers nothing for it
            // and this is where it is settled.
            Shape::Word(Repr::Unit) => self.equal(),
            // The erased case, and the only fallback there is.
            Shape::Boxed => self.dynamic(layout, a, b),
            // Everything left is a value the language will not keep in a
            // keyed collection: a `Float`, whose `NaN` is not equal to itself
            // and so has no total order; a `Vector` and the growable run
            // beneath it; a byte run and a byte buffer; a closure, a `Shared`
            // cell, a host handle, a task and a task scope. `key::order`
            // refuses every one of them in one sentence, and this is that
            // sentence.
            _ => self.not_a_key(),
        }
    }

    /// Leaves the walk where the answer so far is not equal.
    ///
    /// [`Synth::leave_unless`] over three values: an `Int` answer that is not
    /// `0` is the answer of the whole walk, and the parts after it are not
    /// looked at. The pair is a `cmp-imm` and the `branch-false` beside it,
    /// which is the form `super::peephole` fuses.
    fn leave_unless_settled(&mut self) {
        let settled = match self.settled {
            Some(slot) => slot,
            None => {
                let slot = self.alloc(shapes::BOOL);
                self.settled = Some(slot);
                slot
            }
        };
        self.emit(Inst::CmpImm {
            op: CmpOp::Eq,
            dst: settled,
            a: self.answer,
            value: 0,
        });
        let at = self.emit(Inst::BranchFalse {
            cond: settled,
            to: PENDING,
        });
        self.leaves.push(at);
    }

    /// The answer of two values that sort the same: `0`.
    fn equal(&mut self) {
        self.emit(Inst::Int {
            dst: self.answer,
            value: 0,
        });
    }

    /// ADR 0064's Decision 4 again, for the order, which since [ADR
    /// 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
    /// Phase 3 is a call to `std.dynamic.order`.
    ///
    /// [`Synth::fallback`]'s argument word for word: a box's family is a
    /// [`LayoutId`] in its own payload word 0, so there is no layout here to
    /// direct a walk with, and what answers is a Cove walk over a view of each
    /// box. It is reached from [`Shape::Boxed`] and from nowhere else. The
    /// callee was resolved onto the [`Pool`] by the call site that asked for
    /// this walk, which asked [`reaches_a_box`] of [`Operation::Order`] first,
    /// and `cove-cli`'s `tests/boxed.rs` holds every call of it to erased
    /// operands.
    fn dynamic(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let callee = self.pool.dynamic_order.expect(
            "an order walk that reaches a box is asked for only after its call site resolved \
             `std.dynamic.order`",
        );
        let args = self
            .pool
            .args
            .intern(vec![Arg { slot: a, layout }, Arg { slot: b, layout }]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    /// Raises `message`, with no `rule:` and no `help:` line.
    ///
    /// [`Synth::not_a_key`], [`Synth::wrong_case`] and [`Synth::no_text`]
    /// each raise a sentence that is the lowering's own, chosen here and
    /// quoting nothing computed at run time, so it needs no operand the walk
    /// would have to build: it costs one [`Inst::Str`] per slot rather than
    /// a composed body — a load of a precomputed address, not an
    /// allocation, by
    /// [ADR 0045](../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md).
    /// `rule` and `help` share one slot, holding the empty `String`
    /// [`Inst::Trap`] takes to mean "this sentence is absent".
    fn trap(&mut self, message: &str) {
        let text = self.pool.string(message);
        let message = self.alloc(shapes::STR);
        self.emit(Inst::Str { dst: message, text });
        let empty = self.pool.string("");
        let rule = self.alloc(shapes::STR);
        self.emit(Inst::Str {
            dst: rule,
            text: empty,
        });
        self.emit(Inst::Trap {
            message,
            rule,
            help: rule,
        });
    }

    /// A value that is not a key, refused in `key::not_a_key`'s words.
    ///
    /// The sentence is the lowering's own and quotes nothing computed at run
    /// time — see [`Synth::trap`]. It is not reachable from a checked
    /// program — `core.admitKey` refuses such a key before a single
    /// comparison is made, which is `Map.get`'s and `Set.of`'s first line —
    /// and it is written out for the reason the runtime's own arm is:
    /// "should never" is not "cannot", and a silent wrong answer from a
    /// comparison costs more than the arm that reports one.
    fn not_a_key(&mut self) {
        self.trap("this value cannot be a map key or a set element");
    }

    /// A value in a case its layout does not have, in `key::wrong_case`'s
    /// words.
    ///
    /// The lowering's own sentence again: the name is the layout's, known
    /// statically, and nothing else is quoted.
    fn wrong_case(&mut self, name: &str) {
        self.trap(&format!("this `{name}` is in a case it does not have"));
    }

    // ---- `core.admitKey(key, method, role)` -----------------------------

    /// The whole of an admission walk: whether this value is one the language
    /// refuses as a key.
    ///
    /// Three things make this a different shape of walk from the other two,
    /// and every one of them follows from the call site having asked for a
    /// *sentence* rather than for a value.
    ///
    /// **The parts it settles emit nothing at all.** Equality stops at the
    /// first part that differs and the order at the first part that decides,
    /// and both write an answer at every part; here a part no value of which
    /// is ever refused is not looked at. A
    /// `Holder { id: Int, mark: Mark, label: String }` is two fields of
    /// nothing and one `switch`, and the walk for a layout with no such part
    /// is not made at all.
    ///
    /// **It is expanded in place rather than composed out of calls.** The
    /// other two reach a nested layout through [`function_for`], which is
    /// what makes `equals<Array<Row>>` call `equals<Row>`; here the nested
    /// part is written into this same function, because a part that settles
    /// is *nothing* and a call around nothing is not a saving. What keeps the
    /// expansion finite is [`Synth::admit`]'s one exception: a layout already
    /// being expanded — one that holds itself — or one nested past
    /// [`NESTING`] is a call of its own walk instead.
    ///
    /// **It never raises**, and the `false` it falls through with is the
    /// whole of its good path. The refusal is [`Operation::Description`]'s,
    /// which the call site runs where this answers `true`.
    fn admission(&mut self, layout: LayoutId, at: Slot) {
        let mut path = Vec::new();
        self.admit(layout, at, &mut path);
        // Nothing refused: every part the walk looked at is a key, and every
        // part it did not look at is one whatever it holds.
        self.constant(false);
    }

    /// One value of `layout`, at `at`: nothing where every value of it is a
    /// key, and otherwise whatever reading decides.
    fn admit(&mut self, layout: LayoutId, at: Slot, path: &mut Vec<LayoutId>) {
        if admission(self.pool.shapes.all(), layout) == Admission::Always {
            // Nothing at all. This is the arm that makes the walk small:
            // every scalar, every string, every set, and every composite
            // built only out of those.
            return;
        }
        if path.contains(&layout) || path.len() >= NESTING {
            // A layout that holds itself is a walk of calls: `refuses<Node>`
            // calls `refuses<Node>` for each `Node` a `Node` holds. What bounds
            // the recursion is the machine's stack segment, as it bounds
            // `equals<Node>`'s — the language sets no bound of its own (issue
            // #480's decision B), and a key deeper than the stack is an error
            // the run reports. A layout nested past [`NESTING`] is the same
            // call for the sake of the code's size.
            self.called(layout, at);
            return;
        }
        let shape = self.pool.shapes.layout(layout).shape.clone();
        path.push(layout);
        match shape {
            // Fields in declaration order, each at its static word offset,
            // and the ones that are already keys cost nothing.
            Shape::Struct { fields, .. } => {
                for field in &fields {
                    self.admit(field.layout, at + field.at as Slot, path);
                }
            }
            // The one family where the *value* decides and the type does not:
            // `Mark.Count(3)` is a key and `Mark.Weight(1.5)` is not.
            Shape::Enum { cases, .. } => self.cases(&cases, at, path),
            // An array is its elements, over the length the header carries. A
            // `Set`'s members never reach here — [`admission`] answers
            // `Always` for one — which is why this arm is an array's alone.
            Shape::Elements {
                elem,
                growable: false,
            } => self.each(elem, at, path),
            // A map is its *values*: the keys are keys by construction, since
            // the map could not have been built otherwise.
            Shape::Entries { key, value } => self.values(key, value, at, path),
            // A box: no layout says what it holds, so `std.dynamic.refusesKey`
            // decides it over a view, and the walk leaves where it answers
            // that the key is refused.
            Shape::Boxed => self.boxed(layout, at),
            // A `Float`, a `Vector` and the growable run beneath it, a byte
            // run and a byte buffer, a closure, a `Shared` cell, a host
            // handle, a task and a task scope: none of them is a key,
            // whatever it holds, so there is nothing to read and the answer
            // is settled.
            _ => self.refuse(),
        }
        path.pop();
    }

    /// The parts of whichever case this value is in.
    ///
    /// One [`Inst::Switch`] into one arm per case, and an arm is only as long
    /// as the parts of it that are not already keys — which for an enum with
    /// one refused case is a single [`Synth::refuse`] under one of the arms and
    /// a bare [`Inst::Jump`] under the rest.
    ///
    /// The default is [`Synth::refuse`] and not a [`Inst::Trap`], for the
    /// reason every other arm's is: this walk decides and never raises, and
    /// the wording walk the call site runs next meets the same discriminant at
    /// the same `switch` and refuses it in [`Synth::wrong_case`]'s words. The
    /// machine bounds-checks what it reads out of an object rather than
    /// taking the lowering's word for it, which is the same reason a `match`
    /// the checker proved exhaustive still carries a default.
    fn cases(&mut self, cases: &[Case], at: Slot, path: &mut Vec<LayoutId>) {
        let switch = self.emit(Inst::Switch {
            on: at,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(cases.len());
        let mut ends = Vec::with_capacity(cases.len());
        for case in cases {
            targets.push(self.here());
            for part in &case.parts {
                // A part's offset is within the payload region, which begins
                // after the discriminant.
                self.admit(part.layout, at + 1 + part.at as Slot, path);
            }
            // An arm that has already left needs no jump to the join, and the
            // one it would get would be unreachable: [`Synth::refuse`] leaves
            // with the answer written, and this is how that is noticed. The
            // leave has to be the unconditional one: [`Synth::boxed`] leaves
            // under a branch, and an arm that ends there still falls through.
            let last = self.here() - 1;
            let left = self.leaves.last() == Some(&last)
                && matches!(self.code[last as usize], Inst::Jump { .. });
            if !left {
                ends.push(self.emit(Inst::Jump { to: PENDING }));
            }
        }
        let default = self.here();
        self.refuse();
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted a few lines above");
        };
        *held = table;
        let join = self.here();
        for end in ends {
            self.patch(end, join);
        }
    }

    /// A boxed part, at `at`: one call of `std.dynamic.refusesKey` over it,
    /// and the walk leaves where the answer is `true`.
    ///
    /// ADR 0064's Decision 4 for the admission, which since [ADR
    /// 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
    /// Phase 3 is a Cove walk over a view of the box rather than the whole key
    /// handed to the runtime. The answer is written straight into the walk's
    /// own, so where it is `true` the walk leaves with it — under a branch on
    /// its negation, since the instruction set has only a `branch-false` —
    /// and where it is `false` the walk goes on to the next part as it would
    /// after any part that is a key. The callee was resolved onto the
    /// [`Pool`] by the call site that asked for this walk, which asked
    /// [`reaches_a_box`] of [`Operation::Admission`] first, and `cove-cli`'s
    /// `tests/boxed.rs` holds every call of it to an erased operand.
    fn boxed(&mut self, layout: LayoutId, at: Slot) {
        let callee = self.pool.dynamic_refuses_key.expect(
            "an admission walk that reaches a box is asked for only after its call site \
             resolved `std.dynamic.refusesKey`",
        );
        let args = self.pool.args.intern(vec![Arg { slot: at, layout }]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
        let admitted = self.alloc(shapes::BOOL);
        self.emit(Inst::Not {
            dst: admitted,
            a: self.answer,
        });
        let at = self.emit(Inst::BranchFalse {
            cond: admitted,
            to: PENDING,
        });
        self.leaves.push(at);
    }

    /// A part of `layout` at `at` decided by a call of `layout`'s own walk,
    /// `refuses<layout>`, rather than expanded in place: [`Synth::boxed`]'s
    /// shape, over a walk this module composes.
    ///
    /// [`Synth::admit`] asks for it where the layout is already being
    /// expanded — it holds itself — or is nested past [`NESTING`].
    /// [`function_for`] records a function's number before it walks the body,
    /// so a walk that calls itself finds the number rather than starting again.
    fn called(&mut self, layout: LayoutId, at: Slot) {
        let callee = function_for(
            Operation::Admission,
            layout,
            self.pool,
            self.decls,
            self.span,
        );
        let args = self.pool.args.intern(vec![Arg { slot: at, layout }]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
        let admitted = self.alloc(shapes::BOOL);
        self.emit(Inst::Not {
            dst: admitted,
            a: self.answer,
        });
        let at = self.emit(Inst::BranchFalse {
            cond: admitted,
            to: PENDING,
        });
        self.leaves.push(at);
    }

    /// Every element of a run, at the layout its elements have.
    ///
    /// An ordinary loop, so ADR 0040's bounds apply to it because they apply
    /// to every back edge. An empty run runs it no times and is a key, which
    /// is what makes an empty `Array<Float>` one.
    fn each(&mut self, elem: LayoutId, at: Slot, path: &mut Vec<LayoutId>) {
        let each = self.over(at);
        let held = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: at,
            index: each.index,
            layout: elem,
        });
        self.admit(elem, held, path);
        self.around(each);
    }

    /// Every *value* of a map's entries, read at the `MapEntry` layout's
    /// width.
    ///
    /// The same [`shapes::Shapes::entry_of`] the other two walks read a map
    /// with: the entry is the key's words then the value's, so the value's
    /// offset inside it is the key's width.
    fn values(&mut self, key: LayoutId, value: LayoutId, at: Slot, path: &mut Vec<LayoutId>) {
        let entry = self.pool.shapes.entry_of(key, value);
        let keys = self.pool.shapes.words(key).len() as Slot;
        let each = self.over(at);
        let held = self.alloc(entry);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: at,
            index: each.index,
            layout: entry,
        });
        self.admit(value, held + keys, path);
        self.around(each);
    }

    /// The head of a loop over the units of the run at `at`.
    ///
    /// Carried to [`Synth::around`] because the unit is read and asked about
    /// between them, exactly as [`Both`] is carried across a run comparison.
    fn over(&mut self, at: Slot) -> Each {
        let len = self.alloc(shapes::INT);
        self.emit(Inst::Len { dst: len, obj: at });
        let index = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: index,
            value: 0,
        });
        let head = self.here();
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: index,
            b: len,
        });
        let done = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        Each { index, head, done }
    }

    /// The tail of that loop: the step, the back edge, and the landing the
    /// exit was left pending for.
    fn around(&mut self, each: Each) {
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: each.index,
            a: each.index,
            value: 1,
        });
        self.emit(Inst::Jump { to: each.head });
        let end = self.here();
        self.patch(each.done, end);
    }

    /// This value is refused: `true`, and leave.
    ///
    /// **The admission walk never raises, and this is why.** Its refusal is
    /// `` `{method}` cannot use a `{type}` inside `{path}` as a {role} ``
    /// with a `rule:` and a `help:` beside it, and the path quotes an index or
    /// a map's key as it renders wherever the walk reaches through a run or a
    /// map: text, which the path that admits — every path a program that works
    /// takes — has no business building. So this walk answers the bit, and the
    /// call site runs [`Operation::Description`]'s walk where it is `true`,
    /// **at the call site, in the caller's frame, over the caller's key**, and
    /// that walk words the refusal.
    ///
    /// Every point this is reached from is one the language really does
    /// refuse — a `Float`, a `Vector`, a handle, a discriminant no case names —
    /// so the wording walk that follows the `true` raises rather than
    /// answering, and the walk it does again is on the one path that ends the
    /// run.
    fn refuse(&mut self) {
        self.constant(true);
        self.leave();
    }

    // ---- the refusal `core.admitKey` words ---------------------------------

    /// The whole of a wording walk: the refusal of a key an admission walk of
    /// `layout` refused, in the words of the parameters `taken[1]` and
    /// `taken[2]` — `core.admitKey`'s method and role. ADR 0068's Phase 4c.
    ///
    /// **It walks the parts [`Synth::admission`] walks, in its order, and
    /// names the first it meets that is refused.** That is the order the
    /// oracle's `MapKey::convert` visits in — depth first, a struct's fields
    /// and a case's parts in declaration order, an array's elements and a
    /// map's values in order — and a part every value of which is a key is not
    /// looked at. It is expanded in place for the admission walk's reason, and
    /// it calls a walk exactly where that walk calls one: at a layout that
    /// holds itself or is nested past [`NESTING`], which is
    /// [`Operation::DescribeAt`], and at a box, which is
    /// `std.dynamic.refuseKey`, Cove over a view of it.
    ///
    /// **The path is written only at the part it names.** Every piece of it is
    /// known where the walk is — a struct's name at the root, a field's name, a
    /// case's part, the slot an index or a map's entry is in — so the walk
    /// carries the pieces as [`Piece`]s, and a refused part spells the whole
    /// of them into a buffer and jumps to the one sentence [`Synth::worded`]
    /// writes after the function's return. An admitted part costs no text.
    ///
    /// A walk it calls cannot see its caller's pieces, so at a call they are
    /// written onto a trail the callee is handed (see [`Synth::phrase`]), and
    /// `trail` is that parameter of a [`Operation::DescribeAt`] walk: the path
    /// down to its value, which its refused part's path begins with. A
    /// [`Operation::Description`] walk is the one whose value is the key: a
    /// struct and an enum there begin the path with their own name,
    /// `Reading.weight`, `Mark.Weight(0)`, and every other family with nothing,
    /// `[0]`, `[7]`.
    ///
    /// **It is blamed on the site.** The walk is support code
    /// ([`Function::is_support`]), so the refusal its [`Inst::Trap`] raises
    /// is blamed on the `core.admitKey` line that called it, with that line's
    /// `in the standard library` block — where the runtime's `Value.admitKey`
    /// was blamed until this walk replaced it.
    fn describing(&mut self, layout: LayoutId, taken: &[Slot], trail: Option<Slot>) {
        self.emit(Inst::Unit { dst: self.answer });
        let root = trail.is_none();
        // The buffer a refused part spells its path into. A walk of the key
        // makes it now; a walk of a part makes it only at the part it names,
        // which most calls of one — a part that is admitted — never reach.
        let path = self.alloc(shapes::BYTE_BUFFER);
        if root {
            self.bytes(path, 32);
        }
        let lead = self.alloc(shapes::STR);
        let rule = self.alloc(shapes::STR);
        let help = self.alloc(shapes::STR);
        self.words = Some(Words {
            method: taken[1],
            role: taken[2],
            path,
            trail,
            lead,
            rule,
            help,
            refusals: Vec::new(),
        });
        let mut pieces = Vec::new();
        let mut nest = Vec::new();
        let named = self.named;
        self.describe(layout, taken[0], root, named, &mut pieces, &mut nest);
        // Every part was admitted. That is the answer a walk of a part gives
        // its caller, which goes on to the next; a walk of the key its
        // admission walk refused cannot get here, and returns rather than
        // stopping the run with a sentence about nothing.
    }

    /// A new byte buffer in `slot`, with room for `room` bytes.
    fn bytes(&mut self, slot: Slot, room: i64) {
        let capacity = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: capacity,
            value: room,
        });
        self.emit(Inst::GrowableAlloc {
            dst: slot,
            capacity,
            storage: Storage::PackedBytes,
        });
    }

    /// One value of `layout` at `at`, whose parts are walked: a struct, an
    /// enum, an array or a map. `pieces` is the path to it, and `root` whether
    /// it is the key itself.
    ///
    /// `named` is the node of its type that names the resources in it, and
    /// each part is walked with the node of its own: see [`super::named`].
    fn describe(
        &mut self,
        layout: LayoutId,
        at: Slot,
        root: bool,
        named: Option<NamedId>,
        pieces: &mut Vec<Piece>,
        nest: &mut Vec<LayoutId>,
    ) {
        let described = self.pool.shapes.layout(layout);
        let name = described.name.clone();
        let shape = described.shape.clone();
        nest.push(layout);
        match shape {
            Shape::Struct { fields, .. } => {
                if root {
                    pieces.push(Piece::Text(short(declared(&name)).to_string()));
                }
                for (nth, field) in fields.iter().enumerate() {
                    let step = Piece::Text(format!(".{}", field.name));
                    let part = self.pool.naming.part(named, nth);
                    self.phrase(
                        field.layout,
                        at + field.at as Slot,
                        step,
                        part,
                        pieces,
                        nest,
                    );
                }
                if root {
                    pieces.pop();
                }
            }
            Shape::Enum { cases, .. } => self.phrases(&cases, &name, at, root, named, pieces, nest),
            Shape::Elements {
                elem,
                growable: false,
            } => {
                let each = self.over(at);
                let held = self.alloc(elem);
                self.emit(Inst::LoadElem {
                    dst: held,
                    obj: at,
                    index: each.index,
                    layout: elem,
                });
                let part = self.pool.naming.part(named, 0);
                self.phrase(elem, held, Piece::Index(each.index), part, pieces, nest);
                self.around(each);
            }
            Shape::Entries { key, value } => {
                let entry = self.pool.shapes.entry_of(key, value);
                let keys = self.pool.shapes.words(key).len() as Slot;
                let each = self.over(at);
                let held = self.alloc(entry);
                self.emit(Inst::LoadElem {
                    dst: held,
                    obj: at,
                    index: each.index,
                    layout: entry,
                });
                let part = self.pool.naming.part(named, 0);
                self.phrase(
                    value,
                    held + keys,
                    Piece::Key(key, held),
                    part,
                    pieces,
                    nest,
                );
                self.around(each);
            }
            // [`walks`] is what sends a layout here, and it sends only these.
            _ => self.trap("this value's refusal has no path to name"),
        }
        nest.pop();
    }

    /// The parts of whichever case this value is in: [`Synth::cases`]' one
    /// `switch`, and at the root the enum's name and the case's before the
    /// part, `Mark.Weight(0)`.
    ///
    /// The default is [`Synth::wrong_case`]'s trap, which is where the
    /// admission walk's default sends a discriminant no case names.
    #[allow(clippy::too_many_arguments)]
    fn phrases(
        &mut self,
        cases: &[Case],
        name: &str,
        at: Slot,
        root: bool,
        named: Option<NamedId>,
        pieces: &mut Vec<Piece>,
        nest: &mut Vec<LayoutId>,
    ) {
        // The node's parts are every case's parts, one case after another.
        let mut counted = 0;
        let switch = self.emit(Inst::Switch {
            on: at,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(cases.len());
        let mut ends = Vec::with_capacity(cases.len());
        for case in cases {
            targets.push(self.here());
            if root {
                pieces.push(Piece::Text(format!(
                    "{}.{}",
                    short(declared(name)),
                    case.name
                )));
            }
            for (nth, part) in case.parts.iter().enumerate() {
                // A part's offset is within the payload region, which begins
                // after the discriminant.
                let step = Piece::Text(format!("({nth})"));
                let each = self.pool.naming.part(named, counted);
                counted += 1;
                self.phrase(
                    part.layout,
                    at + 1 + part.at as Slot,
                    step,
                    each,
                    pieces,
                    nest,
                );
            }
            if root {
                pieces.pop();
            }
            ends.push(self.emit(Inst::Jump { to: PENDING }));
        }
        let default = self.here();
        self.wrong_case(name);
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted a few lines above");
        };
        *held = table;
        let join = self.here();
        for end in ends {
            self.patch(end, join);
        }
    }

    /// One part of a value, of `layout` at `at`, reached by `step`: nothing
    /// where every value of it is a key; the refusal where every value of it
    /// is refused; and otherwise what the admission walk did there.
    ///
    /// - **In place**, where the admission walk expanded the part in place.
    /// - **A call of `describesAt<L>`**, where it called `refuses<L>`: a layout
    ///   the walk is already inside, or one nested past [`NESTING`]. The
    ///   pieces down to the part are spelled into one `String` and pushed onto
    ///   the trail — made here if this is the key's walk — the call is made,
    ///   and the trail is cut back when it comes back, because a call that
    ///   comes back found nothing refused. **The part is not decided first**:
    ///   the call is the decision and the description in one visit, so a key
    ///   whose layout holds itself is walked once however deep the refused part
    ///   is, where asking `refuses<L>` of every part on the way down would walk
    ///   what is below each of them again.
    /// - **A call of `std.dynamic.refuseKey`**, at a box, under the branch on
    ///   what `std.dynamic.refusesKey` decided of it: Cove words it, over a
    ///   view of the box, handed the path down to the box as a `String`.
    fn phrase(
        &mut self,
        layout: LayoutId,
        at: Slot,
        step: Piece,
        named: Option<NamedId>,
        pieces: &mut Vec<Piece>,
        nest: &mut Vec<LayoutId>,
    ) {
        if admission(self.pool.shapes.all(), layout) == Admission::Always {
            return;
        }
        let shape = self.pool.shapes.layout(layout).shape.clone();
        pieces.push(step);
        if refused_whole(&shape) {
            self.spell_path(pieces);
            self.refuse_with(&shape, named);
        } else if matches!(shape, Shape::Boxed) {
            let decide = self.pool.dynamic_refuses_key.expect(
                "a wording walk that reaches a box is asked for only after its call site resolved \
                 `std.dynamic.refusesKey`",
            );
            let skip = self.asked(decide, layout, at);
            let anchor = self.spelled(pieces);
            self.erased(layout, at, anchor);
            self.leave();
            let past = self.here();
            self.patch(skip, past);
        } else if nest.contains(&layout) || nest.len() >= NESTING {
            self.hand_on(layout, at, named, pieces);
        } else {
            self.describe(layout, at, false, named, pieces, nest);
        }
        pieces.pop();
    }

    /// A part of `layout` at `at` handed to `describesAt<layout>` with the
    /// trail down to it: see [`Synth::phrase`].
    fn hand_on(&mut self, layout: LayoutId, at: Slot, named: Option<NamedId>, pieces: &[Piece]) {
        let trail = self.trail();
        let words = self.words.clone().expect("a wording walk has its words");
        let depth = self.alloc(shapes::INT);
        let held = match words.trail {
            // A walk of a part lengthens the trail it was handed, and cuts it
            // back to this length after the call.
            Some(handed) => {
                self.called_with(trail.length, depth, &[(handed, trail.layout)]);
                handed
            }
            // The key's walk starts one, which nothing after the call reads.
            None => {
                let made = self.alloc(trail.layout);
                self.called_with(trail.new, made, &[]);
                made
            }
        };
        let piece = self.alloc(shapes::BYTE_BUFFER);
        self.bytes(piece, 16);
        self.spell_into(piece, pieces);
        let text = self.alloc(shapes::STR);
        self.finished(text, piece);
        let unit = self.answer;
        self.called_with(
            trail.push,
            unit,
            &[(held, trail.layout), (text, shapes::STR)],
        );
        let callee = function_named_for(
            Operation::DescribeAt,
            layout,
            named,
            self.pool,
            self.decls,
            self.span,
        );
        self.called_with(
            callee,
            unit,
            &[
                (at, layout),
                (words.method, shapes::STR),
                (words.role, shapes::STR),
                (held, trail.layout),
            ],
        );
        if words.trail.is_some() {
            self.called_with(
                trail.cut,
                unit,
                &[(held, trail.layout), (depth, shapes::INT)],
            );
        }
    }

    /// One call of `callee` over `args`, answering into `dst`.
    fn called_with(&mut self, callee: FunctionId, dst: Slot, args: &[(Slot, LayoutId)]) {
        let args = self.pool.args.intern(
            args.iter()
                .map(|(slot, layout)| Arg {
                    slot: *slot,
                    layout: *layout,
                })
                .collect(),
        );
        self.emit(Inst::Call { dst, callee, args });
    }

    /// The `String` the bytes of `buffer` are, into `dst`.
    fn finished(&mut self, dst: Slot, buffer: Slot) {
        self.emit(Inst::RunFinish {
            dst,
            owner: buffer,
            target: shapes::STR,
            validation: Validation::Utf8,
            storage: Storage::PackedBytes,
        });
    }

    /// The [`Trail`] functions, which the call site resolved.
    fn trail(&self) -> Trail {
        self.pool.trail.expect(
            "a wording walk that calls another is asked for only after its call site resolved \
             the trail it hands on",
        )
    }

    /// A call of the deciding walk `decide` over the part at `at`, and the
    /// branch past what follows it where the part is admitted: that branch's
    /// program counter, to be patched.
    fn asked(&mut self, decide: FunctionId, layout: LayoutId, at: Slot) -> Pc {
        let refused = self.alloc(shapes::BOOL);
        let args = self.pool.args.intern(vec![Arg { slot: at, layout }]);
        self.emit(Inst::Call {
            dst: refused,
            callee: decide,
            args,
        });
        self.emit(Inst::BranchFalse {
            cond: refused,
            to: PENDING,
        })
    }

    /// Spells the whole path to the part `pieces` ends at into the walk's
    /// buffer: the trail the walk was handed first, if it was handed one —
    /// whose buffer is made here, at the one part it names — and then
    /// `pieces`.
    fn spell_path(&mut self, pieces: &[Piece]) {
        let words = self.words.clone().expect("a wording walk has its words");
        if let Some(handed) = words.trail {
            self.bytes(words.path, 32);
            let trail = self.trail();
            let before = self.alloc(shapes::STR);
            self.called_with(trail.text, before, &[(handed, trail.layout)]);
            let text = self.leaves().text;
            self.append(text, words.path, before, shapes::STR);
        }
        self.spell_into(words.path, pieces);
    }

    /// The whole path to the part `pieces` ends at, as a `String`: what
    /// [`Synth::spell_path`] spells, finished.
    fn spelled(&mut self, pieces: &[Piece]) -> Slot {
        let words = self.words.clone().expect("a wording walk has its words");
        self.spell_path(pieces);
        let anchor = self.alloc(shapes::STR);
        self.finished(anchor, words.path);
        anchor
    }

    /// Appends the path `pieces` spell to `buffer`: the static text of them
    /// one literal at a time between the ones that are read at run time — an
    /// index, through `std.int.renderInto`, and a map's key, as it renders.
    fn spell_into(&mut self, buffer: Slot, pieces: &[Piece]) {
        let mut text = String::new();
        for piece in pieces {
            match piece {
                Piece::Text(more) => text.push_str(more),
                Piece::Index(index) => {
                    text.push('[');
                    self.literal(buffer, &text);
                    text.clear();
                    let digits = self.leaves().digits;
                    self.number(digits, *index, shapes::INT, buffer);
                    text.push(']');
                }
                Piece::Key(key, entry) => {
                    text.push('[');
                    self.literal(buffer, &text);
                    text.clear();
                    self.render(*key, *entry, buffer);
                    text.push(']');
                }
            }
        }
        self.literal(buffer, &text);
    }

    /// The refusal of a part every value of which is refused, of `shape`:
    /// what it is called, and the rule and the help, set for
    /// [`Synth::worded`]'s sentence, and the jump to it.
    ///
    /// A `Float` is refused under a rule of its own — `NaN` is not equal to
    /// itself, which breaks the total order every key needs — and everything
    /// else because its equality could change while a collection holds it;
    /// the two are [`crate::dynamic`]'s, and the oracle's and
    /// `std.dynamic.refuseKey`'s word for word.
    ///
    /// `named` is the part's node, which names it where it is a Host
    /// resource.
    fn refuse_with(&mut self, shape: &Shape, named: Option<NamedId>) {
        let words = self.words.clone().expect("a wording walk has its words");
        let resource = self.pool.naming.resource(named).map(str::to_string);
        let word = refused_word(shape, resource.as_deref());
        let lead = self
            .pool
            .string(&format!("` cannot use a `{word}` inside `"));
        let (rule, help) = crate::dynamic::refused_key(word);
        let rule = self.pool.string(rule);
        let help = self.pool.string(help);
        self.emit(Inst::Str {
            dst: words.lead,
            text: lead,
        });
        self.emit(Inst::Str {
            dst: words.rule,
            text: rule,
        });
        self.emit(Inst::Str {
            dst: words.help,
            text: help,
        });
        let jump = self.emit(Inst::Jump { to: PENDING });
        if let Some(words) = self.words.as_mut() {
            words.refusals.push(jump);
        }
    }

    /// A boxed part, at `at`, that `std.dynamic.refusesKey` refused:
    /// `std.dynamic.refuseKey` handed the box, the two names and `anchor`, the
    /// path down to the box, which it continues through a view of the box and
    /// words. It never answers.
    ///
    /// Its last operand is the path of vectors a map key it quotes renders
    /// under, which is empty: a key holds no vector. It is made as an
    /// interpolation of a box makes one (see `Body::render_erased`).
    fn erased(&mut self, layout: LayoutId, at: Slot, anchor: Slot) {
        let callee = self.pool.dynamic_refuse_key.expect(
            "a wording walk that reaches a box is asked for only after its call site resolved \
             `std.dynamic.refuseKey`",
        );
        let words = self.words.clone().expect("a wording walk has its words");
        let quoted = self.alloc(shapes::RENDER_PATH);
        self.emit(Inst::AddrOfSlot {
            dst: quoted,
            slot: quoted,
        });
        self.emit(Inst::Int {
            dst: quoted + crate::dynamic::PATH_DEPTH as Slot,
            value: 0,
        });
        let unit = self.answer;
        self.called_with(
            callee,
            unit,
            &[
                (at, layout),
                (words.method, shapes::STR),
                (words.role, shapes::STR),
                (anchor, shapes::STR),
                (quoted, shapes::RENDER_PATH),
            ],
        );
    }

    /// The sentence a wording walk's refused parts jump to, written once after
    /// the function's return, and the [`Inst::Trap`] that raises it.
    ///
    /// `` `{method}` cannot use a `{type}` inside `{path}` as a {role} ``:
    /// the path the buffer holds, finished into a `String`, and the rest
    /// appended around it into a buffer of its own — the method and the role
    /// as the site passed them, and the type in the lead the part set. The
    /// path is never empty here, because the part a walk names is always
    /// inside the value it was handed; a key refused whole is the call site's
    /// literal sentence, and never a walk.
    ///
    /// Nothing for a walk that is not a wording walk, or one with no part it
    /// can name.
    fn worded(&mut self) {
        let Some(words) = self.words.take() else {
            return;
        };
        if words.refusals.is_empty() {
            return;
        }
        let here = self.here();
        for at in &words.refusals {
            self.patch(*at, here);
        }
        let path = self.alloc(shapes::STR);
        self.finished(path, words.path);
        let message = self.alloc(shapes::BYTE_BUFFER);
        self.bytes(message, 64);
        let text = self.leaves().text;
        self.literal(message, "`");
        self.append(text, message, words.method, shapes::STR);
        self.append(text, message, words.lead, shapes::STR);
        self.append(text, message, path, shapes::STR);
        self.literal(message, "` as a ");
        self.append(text, message, words.role, shapes::STR);
        let sentence = self.alloc(shapes::STR);
        self.finished(sentence, message);
        self.emit(Inst::Trap {
            message: sentence,
            rule: words.rule,
            help: words.help,
        });
    }

    // ---- a value that contains itself ------------------------------------

    /// `equals<L>` for a layout [`tracked`] answers `true` for: the path
    /// started empty, and the tracked walk called with it.
    ///
    /// A wrapper rather than the walk itself, so that every caller outside the
    /// cycle — the `==` at a call site, a walk that merely reaches the cycle —
    /// calls the `equals<L>` it always did with the two operands it always
    /// passed. The address is the wrapper's own first word, which a path of
    /// depth nought never reads: it is there because the parameter is an
    /// address and a word has to be one.
    ///
    /// The wrapper is a call, so `super::inline` never expands it, and the
    /// address it forms stays in its own frame.
    fn start_path(&mut self, layout: LayoutId, taken: &[Slot]) {
        let at = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfSlot { dst: at, slot: 0 });
        let depth = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: depth,
            value: 0,
        });
        let callee = function_for(Operation::Tracked, layout, self.pool, self.decls, self.span);
        let args = self.pool.args.intern(vec![
            Arg {
                slot: taken[0],
                layout,
            },
            Arg {
                slot: taken[1],
                layout,
            },
            Arg {
                slot: at,
                layout: shapes::ADDR,
            },
            Arg {
                slot: depth,
                layout: shapes::INT,
            },
        ]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    /// `tracks<L>`: [`Synth::equality`] with the path.
    ///
    /// A layout that is not a vector forwards the path it was given to the
    /// tracked walks of its parts and adds nothing to it. A vector **is** a
    /// pair on the path — the only kind there is — and its walk does two
    /// things first:
    ///
    /// - **It looks for its own pair on the path**, and a pair already there
    ///   is the refusal: `a` and `b` are the two vectors a walk further up is
    ///   still inside. The pair is compared by `Compare::Identity`, which is
    ///   what `is` lowers to, on both sides together — `a == a` has every left
    ///   equal to its right all the way down, and it is only a cycle when the
    ///   same two come round again. The look is a loop over `depth` frames and
    ///   nothing else: three loads a pair, no allocation.
    /// - **It becomes the path's innermost pair** for everything below it. Its
    ///   own first three words already are the pair — the two vectors, which
    ///   are one word each, and the address of the pair before — so the path
    ///   it hands on is the address of its word 0 and a depth one more. The
    ///   frame is live for exactly as long as anything below it is being
    ///   compared, and a frame does not move, so the address is good for as
    ///   long as anything can read it; and the words are its parameters, which
    ///   nothing in the walk writes.
    ///
    /// The lengths are compared before the pair is looked for, and a pair of
    /// empty vectors is answered there and never looked for: nothing is under
    /// it, so it cannot lead back, and a pair on the path is never empty.
    /// Either order would answer the same — a pair on the path is one whose
    /// lengths were found equal on the way down, and nothing has changed them
    /// since — and this one keeps the look off every leaf.
    fn tracking(&mut self, layout: LayoutId, taken: &[Slot]) {
        let (a, b, at, depth) = (taken[0], taken[1], taken[2], taken[3]);
        if !matches!(self.pool.shapes.layout(layout).shape, Shape::Vector { .. }) {
            self.path = Some(Path { at, depth });
            self.equality(layout, a, b);
            return;
        }
        debug_assert_eq!(
            (a, b, at),
            (0, 1, 2),
            "a vector's pair is the first three words of its frame"
        );
        let Shape::Vector { elem } = self.pool.shapes.layout(layout).shape else {
            unreachable!("asked only of a vector");
        };
        // The lengths first, as `Synth::equality`'s vector arm has them — and
        // then an empty pair is answered, because nothing is under it to lead
        // back: a leaf of a tree is a vector with no elements, and most of a
        // tree is leaves, so this is what keeps the look off most of the walk.
        let length = self.alloc(shapes::INT);
        self.emit(Inst::LoadField {
            dst: length,
            obj: a,
            at: shapes::VECTOR_LEN,
            layout: shapes::INT,
        });
        let counterpart = self.alloc(shapes::INT);
        self.emit(Inst::LoadField {
            dst: counterpart,
            obj: b,
            at: shapes::VECTOR_LEN,
            layout: shapes::INT,
        });
        self.lengths(length, counterpart);
        // The walk down the path: `pair` from the innermost, `seen` of `depth`.
        let seen = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: seen,
            value: 0,
        });
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: seen,
            b: length,
        });
        let empty = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        // The answer standing is the lengths', which is `true`.
        self.leaves.push(empty);
        let pair = self.alloc(shapes::ADDR);
        self.emit(Inst::Copy {
            dst: pair,
            src: at,
            layout: shapes::ADDR,
        });
        let head = self.here();
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: seen,
            b: depth,
        });
        let done = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        let held = self.alloc(layout);
        self.emit(Inst::Load {
            dst: held,
            addr: pair,
            layout,
        });
        let same = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Identity,
            op: CmpOp::Eq,
            dst: same,
            a: held,
            b: a,
        });
        let left = self.emit(Inst::BranchFalse {
            cond: same,
            to: PENDING,
        });
        let word = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfPart {
            dst: word,
            addr: pair,
            at: 1,
        });
        self.emit(Inst::Load {
            dst: held,
            addr: word,
            layout,
        });
        self.emit(Inst::Cmp {
            on: Compare::Identity,
            op: CmpOp::Eq,
            dst: same,
            a: held,
            b,
        });
        let right = self.emit(Inst::BranchFalse {
            cond: same,
            to: PENDING,
        });
        self.contains_itself();
        let next = self.here();
        self.patch(left, next);
        self.patch(right, next);
        self.emit(Inst::AddrOfPart {
            dst: word,
            addr: pair,
            at: 2,
        });
        self.emit(Inst::Load {
            dst: pair,
            addr: word,
            layout: shapes::ADDR,
        });
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: seen,
            a: seen,
            value: 1,
        });
        self.emit(Inst::Jump { to: head });
        let end = self.here();
        self.patch(done, end);
        // Not on the path: this pair is the path's innermost from here down.
        let inner = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfSlot {
            dst: inner,
            slot: a,
        });
        let deeper = self.alloc(shapes::INT);
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: deeper,
            a: depth,
            value: 1,
        });
        self.path = Some(Path {
            at: inner,
            depth: deeper,
        });
        // The rest of `Synth::equality`'s vector arm: the two stores, and the
        // elements in them.
        let store = self.pool.shapes.store_of(elem);
        let held = self.alloc(store);
        self.emit(Inst::LoadField {
            dst: held,
            obj: a,
            at: shapes::VECTOR_STORE,
            layout: store,
        });
        let counterheld = self.alloc(store);
        self.emit(Inst::LoadField {
            dst: counterheld,
            obj: b,
            at: shapes::VECTOR_STORE,
            layout: store,
        });
        self.walk(elem, held, counterheld, length);
    }

    /// The refusal of a value that contains itself, in
    /// [`crate::dynamic::CONTAINS_ITSELF`]'s three sentences — the ones
    /// `std.dynamic.equals` and the oracle raise.
    fn contains_itself(&mut self) {
        let message = self.alloc(shapes::STR);
        let text = self.pool.string(crate::dynamic::CONTAINS_ITSELF);
        self.emit(Inst::Str { dst: message, text });
        let rule = self.alloc(shapes::STR);
        let text = self.pool.string(crate::dynamic::CONTAINS_ITSELF_RULE);
        self.emit(Inst::Str { dst: rule, text });
        let help = self.alloc(shapes::STR);
        let text = self.pool.string(crate::dynamic::CONTAINS_ITSELF_HELP);
        self.emit(Inst::Str { dst: help, text });
        self.emit(Inst::Trap {
            message,
            rule,
            help,
        });
    }

    /// ADR 0064's Decision 4: the one dynamic-layout boundary, which since
    /// [ADR 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
    /// Phase 2 is a call to `std.dynamic.equals`.
    ///
    /// A box's family is a [`LayoutId`] in its own payload word 0, so there
    /// is no layout here to direct a walk with, and what answers is a Cove
    /// walk over a view of each box. It is reached from [`Shape::Boxed`] and
    /// from nowhere else. The callee was resolved onto the [`Pool`] by the call
    /// site that asked for this walk, which asked [`reaches_a_box`] first, and
    /// `cove-cli`'s `tests/boxed.rs` holds every call of it to erased operands.
    fn fallback(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let callee = self.pool.dynamic_equals.expect(
            "a walk that reaches a box is asked for only after its call site resolved \
             `std.dynamic.equals`",
        );
        let args = self
            .pool
            .args
            .intern(vec![Arg { slot: a, layout }, Arg { slot: b, layout }]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    // ---- `core.renderInto(value, buffer)` -------------------------------

    /// The whole of a rendering walk: the text of one value of `layout`,
    /// appended to the buffer at `buffer`.
    ///
    /// The `()` is written first and never again. Every append this walk
    /// makes is a call to a body that answers `()`, and each of them writes
    /// its answer into this same slot — so the instruction below is what
    /// covers the two arms that append nothing at all, and the rest is the
    /// same word rewritten with the same value.
    ///
    /// There is no early exit anywhere in here. Equality stops at the first
    /// difference, the order at the first part that decides and the
    /// admission at the first part that is refused; a rendering shows every
    /// part of every value, so [`Synth::leaves`] stays empty and each join —
    /// a `switch`'s, a `Bool`'s, a loop's — is patched where it was opened.
    fn rendering(&mut self, layout: LayoutId, at: Slot, buffer: Slot) {
        self.emit(Inst::Unit { dst: self.answer });
        let described = self.pool.shapes.layout(layout);
        let name = described.name.clone();
        let shape = described.shape.clone();
        match shape {
            // `()` is two characters and a `Bool` is one branch and two
            // literals. Both are here rather than at the call site because
            // [`walks`] sends them here; see its `Rendering` arm.
            Shape::Word(Repr::Unit) => self.literal(buffer, "()"),
            Shape::Word(Repr::Bool) => self.boolean(buffer, at),
            Shape::Struct { fields, opaque } => {
                self.record(layout, &name, &fields, opaque, at, buffer)
            }
            Shape::Enum { cases, .. } => self.variant(&cases, &name, at, buffer),
            // An `Array`, and the run a `Vector` keeps its elements in.
            Shape::Elements { elem, .. } => self.run(elem, at, buffer, "[", "]"),
            // A set and a map both render inside braces, which is how the
            // language writes them and why they are ordered families rather
            // than hashed ones: the order is part of what a program sees.
            Shape::Members { elem } => self.run(elem, at, buffer, "{", "}"),
            Shape::Entries { key, value } => self.mapping(key, value, at, buffer),
            Shape::Vector { elem } => self.vector(elem, at, buffer),
            // Not Cove values, so nothing renders one deliberately — a
            // debugger inspecting a run under construction is the only
            // reader. The bytes of a buffer may not be valid UTF-8 besides.
            Shape::Bytes => self.literal(buffer, "<byte run>"),
            Shape::ByteBuffer => self.literal(buffer, "<byte buffer>"),
            // A cell shows as the handle it is rather than as what it holds:
            // its contents are reachable only under a `lock`, and rendering
            // one would be reading it without taking it.
            Shape::Shared { .. } => self.literal(buffer, "<shared>"),
            // A function value is one reference word to its environment,
            // whose own `Shape::Closure` header says which function it is —
            // and a rendering says `<fn>` whichever it is. So the layout of
            // the *location*, a bare `Repr::Ref` word, is the whole of what
            // the text needs: the one such layout a value can have is
            // `Shapes::function_value`'s. It reached the runtime's
            // `Value.renderInto` until ADR 0068's Phase 4b-ii, the four
            // `known` sites `tests/boxed.rs` counted.
            Shape::Closure { .. } | Shape::Word(Repr::Ref) => self.literal(buffer, "<fn>"),
            // A reclaimed run is not a value, and the runtime's rendering
            // refused one in these words.
            Shape::Free => self.trap("this value was read after it was reclaimed"),
            // A task shows as the handle it is and never as the value it
            // will produce, which is observable only through `await` or the
            // scope settling it — `Display for Value`'s `<task>`, and all of
            // it is in the layout.
            Shape::Word(Repr::Task) => self.literal(buffer, "<task>"),
            // A host resource and a task scope show *which* one they are —
            // `<{module}.{Type}#{n}>`, `<task scope {name}>` — and that is in
            // the run's resource table and the scheduler's scope table rather
            // than in the layout: every resource is the one `<host>` layout and
            // every scope the one `TaskScope`. So neither can be placed as a
            // literal, and until ADR 0068's Phase 4b-ii both refused here (issue
            // #499). [`Inst::HandleText`] is the one question that answers it,
            // asked of the word and of nothing else; ADR 0068's Decision 5 keeps
            // this known layout off reflection, which a rendering of an erased
            // value would have been.
            Shape::Word(Repr::Host | Repr::Scope) => self.handle(at, buffer),
            // An address is a place and not a value; interpolating one would
            // be putting this run's bookkeeping into a string a program
            // prints. A tag is not a value either, for a reason of its own: it
            // is word 0 of an enum, and an enum renders whole through its
            // layout.
            //
            // Everything else — a `String`, an `Int`, a `Float`, a
            // `Duration`, a box — is not a walk at all, so [`walks`] made no
            // function for it and it cannot arrive here.
            _ => self.no_text(),
        }
    }

    /// One part of a rendering: the append where the layout is one, a call
    /// where it is a walk of its own or a scalar a standard-library body
    /// spells, and the fallback where it is a layout that does not say what
    /// the value is.
    ///
    /// The one place [`rendered`] is read at run-time-emitting time, and the
    /// same question `super::interpolate` asks of a whole piece.
    fn render(&mut self, layout: LayoutId, at: Slot, buffer: Slot) {
        let shape = self.pool.shapes.layout(layout).shape.clone();
        match rendered(&shape) {
            Rendered::Text => {
                let callee = self.leaves().text;
                self.append(callee, buffer, at, shapes::STR);
            }
            Rendered::Digits => {
                let callee = self.leaves().digits;
                self.number(callee, at, shapes::INT, buffer);
            }
            Rendered::Float => {
                let callee = self.leaves().float.expect(
                    "a walk that reaches a `Float` is asked for only after its call site \
                     resolved `std.float.renderInto`",
                );
                self.number(callee, at, shapes::FLOAT, buffer);
            }
            Rendered::Duration => {
                let callee = self.leaves().duration.expect(
                    "a walk that reaches a `Duration` is asked for only after its call site \
                     resolved `std.duration.renderInto`",
                );
                self.number(callee, at, shapes::DURATION, buffer);
            }
            Rendered::Walk => {
                // A tracked walk hands its path to the tracked walk of a part
                // that carries one; see [`render_tracked`]. Only an
                // [`Operation::RenderTracked`] walk has a path to hand on.
                if let Some(path) = self.path {
                    if render_tracked(self.pool, layout) {
                        let callee = function_for(
                            Operation::RenderTracked,
                            layout,
                            self.pool,
                            self.decls,
                            self.span,
                        );
                        let args = self.pool.args.intern(vec![
                            Arg { slot: at, layout },
                            Arg {
                                slot: buffer,
                                layout: shapes::BYTE_BUFFER,
                            },
                            Arg {
                                slot: path.at,
                                layout: shapes::ADDR,
                            },
                            Arg {
                                slot: path.depth,
                                layout: shapes::INT,
                            },
                        ]);
                        self.emit(Inst::Call {
                            dst: self.answer,
                            callee,
                            args,
                        });
                        return;
                    }
                }
                let callee = function_for(
                    Operation::Rendering,
                    layout,
                    self.pool,
                    self.decls,
                    self.span,
                );
                let args = self.pool.args.intern(vec![
                    Arg { slot: at, layout },
                    Arg {
                        slot: buffer,
                        layout: shapes::BYTE_BUFFER,
                    },
                ]);
                self.emit(Inst::Call {
                    dst: self.answer,
                    callee,
                    args,
                });
            }
            Rendered::Dynamic => self.below(layout, at, buffer),
        }
    }

    /// A call of one of the three standard-library functions that write a
    /// scalar's text — `std.int`'s, `std.float`'s or `std.duration`'s
    /// `renderInto` — over the word at `at`.
    ///
    /// Each takes the value *first* and the buffer second, where the two
    /// appends take the buffer first. Each is its own declaration's order.
    fn number(&mut self, callee: FunctionId, at: Slot, layout: LayoutId, buffer: Slot) {
        let args = self.pool.args.intern(vec![
            Arg { slot: at, layout },
            Arg {
                slot: buffer,
                layout: shapes::BYTE_BUFFER,
            },
        ]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    /// A struct: the builtin `Error` and `Range`, an opaque type's bare
    /// name, and otherwise the declared name and the fields in declaration
    /// order.
    fn record(
        &mut self,
        layout: LayoutId,
        name: &str,
        fields: &[Field],
        opaque: bool,
        at: Slot,
        buffer: Slot,
    ) {
        // An opaque type renders as its name and nothing else. Its fields
        // are the declaring module's own business, and a rendering is read
        // by whoever the string reaches, so showing them here would publish
        // through `println` what the checker refuses to let a caller name.
        if opaque {
            let text = short(declared(name)).to_string();
            self.literal(buffer, &text);
            return;
        }
        if is_error(name, fields) {
            let message = fields[0].clone();
            self.render(message.layout, at + message.at as Slot, buffer);
            return;
        }
        if is_range(&self.pool.shapes, layout, name, fields) {
            self.extent(buffer, at);
            return;
        }
        // One literal per gap rather than one per punctuation mark: the
        // declared name and the first label are one string, and every label
        // after it carries the `, ` in front of it. A four-field struct is
        // five appends and four renderings, where a mark at a time would be
        // nine appends.
        let mut lead = format!("{}(", short(declared(name)));
        if fields.is_empty() {
            lead.push(')');
            self.literal(buffer, &lead);
            return;
        }
        for (nth, field) in fields.iter().enumerate() {
            if nth > 0 {
                lead.push_str(", ");
            }
            lead.push_str(&field.name);
            lead.push_str(": ");
            self.literal(buffer, &lead);
            lead.clear();
            self.render(field.layout, at + field.at as Slot, buffer);
        }
        self.literal(buffer, ")");
    }

    /// A `Range`, as the operator it was written with.
    ///
    /// `1..3` and `1..<4` cover the same values and are two renderings,
    /// because they are two values: `==` on ranges compares the bounds a
    /// program wrote and not the set they describe.
    fn extent(&mut self, buffer: Slot, at: Slot) {
        self.render(shapes::INT, at + shapes::RANGE_START as Slot, buffer);
        let exclusive = self.emit(Inst::BranchFalse {
            cond: at + shapes::RANGE_INCLUSIVE as Slot,
            to: PENDING,
        });
        self.literal(buffer, "..");
        let done = self.emit(Inst::Jump { to: PENDING });
        let other = self.here();
        self.patch(exclusive, other);
        self.literal(buffer, "..<");
        let join = self.here();
        self.patch(done, join);
        self.render(shapes::INT, at + shapes::RANGE_END as Slot, buffer);
    }

    /// The case this value is in, and the parts that case names.
    ///
    /// One [`Inst::Switch`] into one arm per case, each arm a literal, its
    /// parts and a jump to the join. The case name and its opening bracket
    /// are one literal for [`Synth::record`]'s reason.
    ///
    /// The default is an [`Inst::Trap`]. The runtime's own arm words the
    /// same refusal with the discriminant in it — `` is in case {index} `` —
    /// and rendering that discriminant into text is work this walk does not
    /// do, so this one says [`Synth::wrong_case`]'s sentence instead: the
    /// words `key::wrong_case`
    /// already uses, which [`Synth::ranking`] already emits for the same
    /// reading of the same `switch`. Nothing a checked program holds reaches
    /// either — the machine bounds-checks what it reads out of an object
    /// rather than taking the lowering's word for it, which is the same
    /// reason a `match` the checker proved exhaustive still carries a
    /// default.
    fn variant(&mut self, cases: &[Case], name: &str, at: Slot, buffer: Slot) {
        let switch = self.emit(Inst::Switch {
            on: at,
            table: TableId(0),
        });
        let mut targets = Vec::with_capacity(cases.len());
        let mut ends = Vec::with_capacity(cases.len());
        for case in cases {
            targets.push(self.here());
            if case.parts.is_empty() {
                self.literal(buffer, &case.name);
            } else {
                let mut lead = format!("{}(", case.name);
                for (nth, part) in case.parts.iter().enumerate() {
                    if nth > 0 {
                        lead.push_str(", ");
                    }
                    self.literal(buffer, &lead);
                    lead.clear();
                    // A part's offset is within the payload region, which
                    // begins after the discriminant.
                    self.render(part.layout, at + 1 + part.at as Slot, buffer);
                }
                self.literal(buffer, ")");
            }
            ends.push(self.emit(Inst::Jump { to: PENDING }));
        }
        let default = self.here();
        self.wrong_case(name);
        let table = self.pool.table(Table { targets, default });
        let Inst::Switch { table: held, .. } = &mut self.code[switch as usize] else {
            unreachable!("the switch was emitted a few lines above");
        };
        *held = table;
        let join = self.here();
        for end in ends {
            self.patch(end, join);
        }
    }

    /// A run of elements between two brackets, `, ` between each pair.
    fn run(&mut self, elem: LayoutId, at: Slot, buffer: Slot, open: &str, close: &str) {
        let len = self.alloc(shapes::INT);
        self.emit(Inst::Len { dst: len, obj: at });
        self.joined(elem, at, len, buffer, open, close);
    }

    /// A vector's elements, which are its store's, over the length the
    /// *vector* carries.
    ///
    /// A vector renders like an array, because the indirection is what lets
    /// it grow without moving and is not a fact about the value. The length
    /// comes from the vector and not from the store, which is the whole
    /// reason the two are separate: a store is as long as the last growth
    /// made it, and the elements past the length are the spare room.
    fn vector(&mut self, elem: LayoutId, at: Slot, buffer: Slot) {
        let store = self.pool.shapes.store_of(elem);
        let len = self.alloc(shapes::INT);
        self.emit(Inst::LoadField {
            dst: len,
            obj: at,
            at: shapes::VECTOR_LEN,
            layout: shapes::INT,
        });
        let held = self.alloc(store);
        self.emit(Inst::LoadField {
            dst: held,
            obj: at,
            at: shapes::VECTOR_STORE,
            layout: store,
        });
        self.joined(elem, held, len, buffer, "[", "]");
    }

    /// A map's entries, `key: value` apiece, between braces.
    ///
    /// The entry is `MapEntry`'s own inline layout — the key's words then
    /// the value's — so one `load-elem` at that width reads both halves and
    /// the value's offset inside it is the key's width. That is the same
    /// [`shapes::Shapes::entry_of`] the other three walks read a map with.
    fn mapping(&mut self, key: LayoutId, value: LayoutId, at: Slot, buffer: Slot) {
        let entry = self.pool.shapes.entry_of(key, value);
        let keys = self.pool.shapes.words(key).len() as Slot;
        let len = self.alloc(shapes::INT);
        self.emit(Inst::Len { dst: len, obj: at });
        self.literal(buffer, "{");
        let each = self.through(len, buffer);
        let held = self.alloc(entry);
        self.emit(Inst::LoadElem {
            dst: held,
            obj: at,
            index: each.index,
            layout: entry,
        });
        self.render(key, held, buffer);
        self.literal(buffer, ": ");
        self.render(value, held + keys, buffer);
        self.around(each);
        self.literal(buffer, "}");
    }

    /// `len` units of `obj`, rendered and joined between two brackets.
    fn joined(
        &mut self,
        elem: LayoutId,
        obj: Slot,
        len: Slot,
        buffer: Slot,
        open: &str,
        close: &str,
    ) {
        self.literal(buffer, open);
        let each = self.through(len, buffer);
        let held = self.alloc(elem);
        self.emit(Inst::LoadElem {
            dst: held,
            obj,
            index: each.index,
            layout: elem,
        });
        self.render(elem, held, buffer);
        self.around(each);
        self.literal(buffer, close);
    }

    /// The head of a loop over `len` positions, with the separator every
    /// position but the first is preceded by already appended.
    ///
    /// [`Synth::over`] with the `, ` in it, and the separator is inside the
    /// loop rather than after each element for the reason a joining ever
    /// puts it there: what is wanted is `n - 1` separators, and a loop that
    /// appended one after every element would have to take the last one back
    /// out. The test is `index != 0` and a `branch-false` over the append,
    /// which is one comparison and one branch a turn — the instruction set
    /// has no `branch-true`, so writing it the other way round would be two.
    fn through(&mut self, len: Slot, buffer: Slot) -> Each {
        let index = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: index,
            value: 0,
        });
        let head = self.here();
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: index,
            b: len,
        });
        let done = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        self.emit(Inst::CmpImm {
            op: CmpOp::Ne,
            dst: more,
            a: index,
            value: 0,
        });
        let first = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        self.literal(buffer, ", ");
        let body = self.here();
        self.patch(first, body);
        Each { index, head, done }
    }

    /// One literal run of text, appended to the buffer at `buffer`.
    ///
    /// One byte is [`Leaves::byte`] of a constant, which needs no string
    /// loaded and no length; anything longer is a pooled string and
    /// [`Leaves::text`]. That is `super::interpolate::Body::append_literal`'s
    /// own choice, made here for the same reason and over the same two
    /// bodies.
    fn literal(&mut self, buffer: Slot, text: &str) {
        match text.as_bytes() {
            [] => {}
            [byte] => {
                let value = i64::from(*byte);
                let slot = match self.punctuation {
                    Some(slot) => slot,
                    None => {
                        let slot = self.alloc(shapes::INT);
                        self.punctuation = Some(slot);
                        slot
                    }
                };
                self.emit(Inst::Int { dst: slot, value });
                let callee = self.leaves().byte;
                self.append(callee, buffer, slot, shapes::INT);
            }
            _ => {
                let id = self.pool.string(text);
                let slot = match self.literal {
                    Some(slot) => slot,
                    None => {
                        let slot = self.alloc(shapes::STR);
                        self.literal = Some(slot);
                        slot
                    }
                };
                self.emit(Inst::Str {
                    dst: slot,
                    text: id,
                });
                let callee = self.leaves().text;
                self.append(callee, buffer, slot, shapes::STR);
            }
        }
    }

    /// The text of the resource or scope in `at`, appended to `buffer`: one
    /// [`Inst::HandleText`] into a `String` of the walk's own, and the append
    /// of it.
    ///
    /// The one arm of a rendering walk that allocates a string itself. It owes
    /// no [`Inst::Clear`], for the module's reason: the string is retained for
    /// one append, until this function's frame is popped a few instructions
    /// later.
    fn handle(&mut self, at: Slot, buffer: Slot) {
        let text = self.alloc(shapes::STR);
        self.emit(Inst::HandleText { dst: text, src: at });
        let callee = self.leaves().text;
        self.append(callee, buffer, text, shapes::STR);
    }

    /// One call of an append: the buffer first, then what is appended.
    ///
    /// Its answer is written into [`Synth::answer`], which already holds the
    /// `()` this function returns and is about to hold it again. A
    /// destination of its own would be a word per append in a frame that has
    /// one word of answer.
    fn append(&mut self, callee: FunctionId, buffer: Slot, slot: Slot, layout: LayoutId) {
        let args = self.pool.args.intern(vec![
            Arg {
                slot: buffer,
                layout: shapes::BYTE_BUFFER,
            },
            Arg { slot, layout },
        ]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    /// `true` or `false`, the two words the language writes a `Bool` as.
    fn boolean(&mut self, buffer: Slot, at: Slot) {
        let otherwise = self.emit(Inst::BranchFalse {
            cond: at,
            to: PENDING,
        });
        self.literal(buffer, "true");
        let done = self.emit(Inst::Jump { to: PENDING });
        let no = self.here();
        self.patch(otherwise, no);
        self.literal(buffer, "false");
        let join = self.here();
        self.patch(done, join);
    }

    /// A value with no text of its own, refused in the runtime's own words.
    ///
    /// The lowering's own sentence again, [`Synth::not_a_key`]'s argument
    /// repeated. The families it answers for are word 0 of an enum and an
    /// address, which no checked program can interpolate. A task was here
    /// too, and renders as `<task>` now; and a host handle and a task scope,
    /// which issue #499 found a program can interpolate, render through
    /// [`Synth::handle`] since ADR 0068's Phase 4b-ii.
    fn no_text(&mut self) {
        self.trap("this value has no text of its own");
    }

    /// ADR 0064's Decision 4 for the rendering: a box, whose layout does not
    /// say what the value is, rendered by `std.dynamic.renderInto`.
    ///
    /// See [`Rendered::Dynamic`]. **The path is handed on** (issue #499's
    /// decision 3): a tracked walk passes the two words it is carrying as one
    /// [`shapes::RENDER_PATH`] argument, and the dynamic renderer looks a
    /// vector up on it with `core.dynamicOnPath` before its own path — so a
    /// cycle that closes through a box renders `[…]` exactly where the
    /// oracle's does. Every walk that reaches a box is tracked
    /// ([`render_tracked`]), so there is always a path here; the two words
    /// are adjacent, because every place a path is made allocates its address
    /// and then its depth, and the parameters of a tracked walk are laid out
    /// the same way.
    ///
    /// The other direction needs nothing: the dynamic renderer walks
    /// everything below the box through views and never calls a walk
    /// composed here, so no path has to come back out of it.
    fn below(&mut self, layout: LayoutId, at: Slot, buffer: Slot) {
        let callee = self.pool.dynamic_render.expect(
            "a walk that reaches a box is asked for only after its call site resolved \
             `std.dynamic.renderInto`",
        );
        let path = match self.path {
            Some(path) => path,
            None => {
                // No walk reaches here without one; an empty path, as an
                // interpolation of a box hands over, is the answer if one did.
                let at = self.alloc(shapes::RENDER_PATH);
                self.emit(Inst::AddrOfSlot { dst: at, slot: at });
                self.emit(Inst::Int {
                    dst: at + 1,
                    value: 0,
                });
                Path { at, depth: at + 1 }
            }
        };
        assert_eq!(
            path.depth,
            path.at + 1,
            "a path's depth is the word after its address"
        );
        let args = self.pool.args.intern(vec![
            Arg { slot: at, layout },
            Arg {
                slot: buffer,
                layout: shapes::BYTE_BUFFER,
            },
            Arg {
                slot: path.at,
                layout: shapes::RENDER_PATH,
            },
        ]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    // ---- a rendering that contains itself --------------------------------

    /// `renders<L>` for a layout [`render_tracked`] answers `true` for: the
    /// path started empty, and the tracked walk called with it.
    ///
    /// [`Synth::start_path`] for the rendering, and a wrapper for its reason:
    /// every caller outside the cycle — an interpolation, a walk that merely
    /// reaches the cycle — calls the `renders<L>` it always did with the two
    /// operands it always passed. The address is the wrapper's own first
    /// word, which a path of depth nought never reads.
    fn start_render_path(&mut self, layout: LayoutId, taken: &[Slot]) {
        let at = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfSlot { dst: at, slot: 0 });
        let depth = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: depth,
            value: 0,
        });
        let callee = function_for(
            Operation::RenderTracked,
            layout,
            self.pool,
            self.decls,
            self.span,
        );
        let args = self.pool.args.intern(vec![
            Arg {
                slot: taken[0],
                layout,
            },
            Arg {
                slot: taken[1],
                layout: shapes::BYTE_BUFFER,
            },
            Arg {
                slot: at,
                layout: shapes::ADDR,
            },
            Arg {
                slot: depth,
                layout: shapes::INT,
            },
        ]);
        self.emit(Inst::Call {
            dst: self.answer,
            callee,
            args,
        });
    }

    /// `rendersTracked<L>`: [`Synth::rendering`] with the path.
    ///
    /// A layout that is not a vector forwards the path it was given to the
    /// tracked walks of its parts and adds nothing to it. A vector **is** an
    /// entry of the path — the only kind there is, because a `Vector` is the
    /// only node a value can meet again: everything else a rendering descends
    /// into is immutable once built — and its walk does two things before it
    /// renders its elements:
    ///
    /// - **It looks for itself on the path**, and finding itself is the
    ///   repeat: this vector is one a rendering further up is still inside,
    ///   so it renders as `[…]` and nothing is walked below it (issue #499's
    ///   decision 2). The look is by `Compare::Identity`, which is what `is`
    ///   lowers to, one load and one comparison an entry, and a loop over
    ///   `depth` frames and nothing else — no allocation.
    /// - **It becomes the path's innermost entry** for everything below it.
    ///   Its own first three words already are the entry — the vector, the
    ///   buffer, and the address of the entry before — so the path it hands
    ///   on is the address of its word 0 and a depth one more. The frame is
    ///   live for exactly as long as anything below it is being rendered, and
    ///   that is exactly the span decision 1 of issue #499 gives an entry:
    ///   pushed on descent, popped when the vector's rendering completes. So
    ///   a vector met twice by two routes — a shared DAG — is on the path only
    ///   while one of them is being rendered, and renders in full both times.
    ///
    /// **An empty vector is never looked for.** Its length is read first, and
    /// one of nought renders `[]` without the look: nothing is under it, so
    /// it cannot lead back, and a vector on the path is never empty. The two
    /// words that would push it are written and read by nothing, because
    /// there is nothing below it to read them. The leaf of a tree is a vector
    /// with no elements and most of a tree is leaves, so this is what keeps
    /// the look off most of the walk.
    fn render_tracking(&mut self, layout: LayoutId, taken: &[Slot]) {
        let (at, buffer, path, depth) = (taken[0], taken[1], taken[2], taken[3]);
        let Shape::Vector { elem } = self.pool.shapes.layout(layout).shape else {
            self.path = Some(Path { at: path, depth });
            self.rendering(layout, at, buffer);
            return;
        };
        debug_assert_eq!(
            (at, buffer, path),
            (0, 1, 2),
            "a vector's path entry is the first three words of its frame"
        );
        self.emit(Inst::Unit { dst: self.answer });
        let len = self.alloc(shapes::INT);
        self.emit(Inst::LoadField {
            dst: len,
            obj: at,
            at: shapes::VECTOR_LEN,
            layout: shapes::INT,
        });
        // The walk down the path: `entry` from the innermost, `seen` of
        // `depth` — and before it, the empty vector's way past it.
        let seen = self.alloc(shapes::INT);
        self.emit(Inst::Int {
            dst: seen,
            value: 0,
        });
        let more = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: seen,
            b: len,
        });
        let empty = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        let entry = self.alloc(shapes::ADDR);
        self.emit(Inst::Copy {
            dst: entry,
            src: path,
            layout: shapes::ADDR,
        });
        let head = self.here();
        self.emit(Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst: more,
            a: seen,
            b: depth,
        });
        let absent = self.emit(Inst::BranchFalse {
            cond: more,
            to: PENDING,
        });
        let held = self.alloc(layout);
        self.emit(Inst::Load {
            dst: held,
            addr: entry,
            layout,
        });
        let same = self.alloc(shapes::BOOL);
        self.emit(Inst::Cmp {
            on: Compare::Identity,
            op: CmpOp::Eq,
            dst: same,
            a: held,
            b: at,
        });
        let other = self.emit(Inst::BranchFalse {
            cond: same,
            to: PENDING,
        });
        // On the path: the repeat, and nothing below it.
        self.literal(buffer, "[…]");
        let repeat = self.emit(Inst::Jump { to: PENDING });
        let next = self.here();
        self.patch(other, next);
        let word = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfPart {
            dst: word,
            addr: entry,
            at: 2,
        });
        self.emit(Inst::Load {
            dst: entry,
            addr: word,
            layout: shapes::ADDR,
        });
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: seen,
            a: seen,
            value: 1,
        });
        self.emit(Inst::Jump { to: head });
        let walk = self.here();
        self.patch(absent, walk);
        self.patch(empty, walk);
        // Not on the path: this vector is the path's innermost from here down.
        let inner = self.alloc(shapes::ADDR);
        self.emit(Inst::AddrOfSlot {
            dst: inner,
            slot: at,
        });
        let deeper = self.alloc(shapes::INT);
        self.emit(Inst::ArithImm {
            op: ArithOp::Add,
            dst: deeper,
            a: depth,
            value: 1,
        });
        self.path = Some(Path {
            at: inner,
            depth: deeper,
        });
        // The rest of [`Synth::vector`]: the store, and the elements in it
        // over the length the vector carries.
        let store = self.pool.shapes.store_of(elem);
        let elements = self.alloc(store);
        self.emit(Inst::LoadField {
            dst: elements,
            obj: at,
            at: shapes::VECTOR_STORE,
            layout: store,
        });
        self.joined(elem, elements, len, buffer, "[", "]");
        let end = self.here();
        self.patch(repeat, end);
    }

    /// The three appends this walk is composed out of.
    ///
    /// Recorded on the [`Pool`] by the call site that first asked for a
    /// rendering, because resolving a standard-library name is `Plan`'s and
    /// `Body::reached`'s and neither is reachable from here. A walk is asked
    /// for only through that call site, which is what makes this total.
    fn leaves(&self) -> Leaves {
        self.pool
            .leaves
            .expect("a rendering walk is asked for only after its call site has found the appends")
    }
}
