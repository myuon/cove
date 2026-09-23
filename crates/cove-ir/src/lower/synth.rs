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
//! # `Value.admitKey`'s boundary is not that boundary, and the reason is the
//! sentence
//!
//! The third user does not fit that paragraph and it is worth saying why
//! rather than filing it under "nearly". The other two *answer* something —
//! a `Bool`, an `Int` — and a walk that has composed the answer is done. An
//! admission answers `()` **or raises**, and what it raises is
//! `` `{method}` cannot use a `{type}` inside `{path}` as a {role} `` with a
//! `rule:` and a `help:` beside it: three sentences, and [`Inst::Trap`] now
//! takes a slot for each, so the instruction is no longer the wall.
//!
//! Every hole in that sentence is something the lowering knows, except one.
//! The two names are literals at all nine of `std.map`'s and `std.set`'s call
//! sites; the type is the layout's; the path is composed of field and case
//! names a *synthesized* walk knows statically per arm — except where it
//! reaches through a run or a map, where it quotes an index or a rendered
//! key computed at run time, which is issue #461's kind of hole and not one
//! this module's walks build.
//!
//! So [`Operation::Admission`] does not refuse. It **decides**: a `Bool`,
//! `true` where the runtime is to be asked, and `super::core` runs the
//! [`Intrinsic::ValueAdmitKey`] it would have run anyway, at the site it
//! would have run it at, over the key it would have run it over. That keeps
//! the diagnostic byte for byte — including the frame it is blamed on, which
//! a fallback raised from inside a walk would have added one to (ADR 0058
//! reads the blame off the live frames). What the walk buys is the admitting
//! path, which is every path a program that works ever takes: a key the
//! layout settles reaches no intrinsic at all.
//!
//! [`admission`] is the question that decides, asked by the call site and by
//! the walk, of one table — `ordered_by`'s arrangement, and `Body::always_admitted`
//! before this module had an arm for the operation.
//!
//! A box is decided the same way since [ADR
//! 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! Phase 3: `std.dynamic.refusesKey`, a Cove walk over a view of the box,
//! answers the bit — called by `super::core` for a key that is a box, and by
//! [`Synth::admit`] where a walk reaches a boxed part — and the intrinsic under
//! the branch still words the refusal. So a boxed layout is
//! [`Admission::Decided`] like any other layout a walk can answer for, and
//! `cove-cli`'s `tests/boxed.rs` holds every call of the function to an erased
//! operand (Decision 5).
//!
//! # Three of the four walks allocate nothing, and the fourth allocates by
//! nature
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
//! What is true of all four, and is what the old sentence was reaching for:
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
//!   a clear *from*.
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
//! `Value.order`'s two are not it, and `Value.admitKey`'s — which carry a
//! rule and a help — are not it either, for a reason next to it rather than
//! in it.
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

use std::sync::Arc;

use cove_diag::Span;
use cove_schema::builtins::{ERROR, MESSAGE_FIELD, RANGE};

use crate::inst::{ArithOp, CmpOp, Compare, Inst, Pc, Slot};
use crate::intrinsic::Intrinsic;
use crate::layout::{Case, Field, Layout, LayoutId, Shape};
use crate::program::{Arg, Function, FunctionId, IntrinsicSite, Table, TableId};
use crate::repr::{RefMap, Repr};

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
    /// write. Where the bit is set, `super::core` runs the
    /// [`Intrinsic::ValueAdmitKey`] it would have run anyway, at the site it
    /// would have run it at, and the runtime writes the sentence.
    ///
    /// It has to be that way round, and the reason is in the sentence rather
    /// than in the walk. A refusal here is
    /// `` `{method}` cannot use a `{type}` inside `{path}` as a {role} ``
    /// with a **`rule:` and a `help:` beside it**, and a `path` that reaches
    /// through a run or a map quotes an index or a rendered key computed at
    /// run time — so `super::pattern`'s trick of choosing the string at the
    /// lowering is not enough here, whatever is known about the layout. See
    /// this module's header.
    ///
    /// So the answer is a `Bool` and the sense of it is **`true` when the
    /// runtime is to be asked**: a `branch-false` over the intrinsic is one
    /// instruction where a `branch-true` would be two, since the instruction
    /// set has only the one.
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
    /// a literal and of `std.int`'s `renderInto` for a number. Those are the
    /// same three bodies `super::interpolate` already appends a piece
    /// through, reached the same way, so no lowering writes [ADR 0062]'s
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
        }
    }

    /// What it takes, in order.
    ///
    /// Two values of the layout for a comparison, one for the admission,
    /// which needs neither of `core.admitKey`'s two names — they word a
    /// refusal, and this walk does not word one — and for the rendering the
    /// value and the buffer its text is appended to, in that order, which is
    /// `Value.renderInto`'s own.
    fn params(self, layout: LayoutId) -> Vec<LayoutId> {
        match self {
            Operation::Equality | Operation::Order => vec![layout, layout],
            Operation::Admission => vec![layout],
            Operation::Rendering => vec![layout, shapes::BYTE_BUFFER],
            // The path's innermost pair, as the address of the frame words it
            // is in, and how many pairs deep the path goes.
            Operation::Tracked => vec![layout, layout, shapes::ADDR, shapes::INT],
        }
    }
}

/// What the language can say, from a layout alone, about every value of it as
/// a map key or a set element.
///
/// ADR 0001 admits a key built only from immutable parts and refuses a
/// `Float`, whose `NaN` is not equal to itself and so has no total order, and
/// a `Vector` and everything holding one, because a key's equality must not
/// change while a collection holds it. Which of the three this answers is
/// what decides, at the call site, between emitting nothing, a call to a
/// walk, and the intrinsic.
///
/// The order of the variants is the order of the lattice: a composite is the
/// **greatest** of its parts, so one part nobody can settle statically makes
/// the whole value one the runtime is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Admission {
    /// No value of this layout is ever refused, so asking is nothing at all.
    ///
    /// `Body::always_admitted` before this module had an arm for the
    /// operation, moved here unchanged and for [`ordered_by`]'s reason: the
    /// call site and the walk ask one question, in one place, of one table.
    /// **It is load-bearing and it is not an optimisation.** Every key
    /// `covefmt` and `cq` use is a `String` or an `Int`, so this is why
    /// neither program reaches `Value.admitKey` at a single site.
    Always,
    /// Some values of this layout are refused and a walk can tell which.
    ///
    /// A `Float` field, a `Vector` field, an enum with one case that holds
    /// one — the walk reads whatever decides and asks only where the answer
    /// is that the key is refused, which is the one path that does not
    /// continue.
    Decided,
    /// The runtime's own walk is what answers.
    ///
    /// Two things reach it: a layout that holds itself, whose values nest as
    /// deep as they like where a walk composed here is finite; and a layout
    /// nested past [`NESTING`], which is the same question asked about code
    /// size.
    ///
    /// A third reached it until [ADR
    /// 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
    /// Phase 3, and it was the one ADR 0064's Decision 4 named: a
    /// [`Shape::Boxed`], whose family is a [`LayoutId`] in its own payload word
    /// 0 and is genuinely unknown until the box is opened. It is
    /// [`Admission::Decided`] now — `std.dynamic.refusesKey` decides it, in
    /// Cove over a view of the box — so a composite holding one is decided by
    /// a walk that calls that function at the box.
    Dynamic,
}

/// How deep a key's layout may nest before the admission stops composing and
/// asks.
///
/// `Body::always_admitted`'s `ADMITTED_DEPTH`, moved here with it. It was
/// chosen well inside the runtime's bound on how deep a key is walked (128
/// steps, of which a level of nesting takes at most two), so that a layout
/// this shallow had no value the runtime's walk would stop for its depth. That
/// bound is gone since ADR 0068's Phase 3 — the runtime's walk is a loop over
/// a stack, and words a refusal at any depth — so this is a bound on code size
/// and nothing else.
const NESTING: usize = 48;

/// Which of the three [`layout`] is.
///
/// Over a slice of layouts rather than over a [`shapes::Shapes`] because
/// [`crate::verify`] asks it too, of a finished [`crate::Program`], and the
/// rule it checks is this one: a question asked in one place of one table.
pub(crate) fn admission(layouts: &[Layout], layout: LayoutId) -> Admission {
    fn walk(layouts: &[Layout], layout: LayoutId, path: &mut Vec<LayoutId>) -> Admission {
        if layout.index() >= layouts.len() {
            return Admission::Dynamic;
        }
        if path.contains(&layout) || path.len() >= NESTING {
            return Admission::Dynamic;
        }
        path.push(layout);
        // A composite is the greatest of its parts, walked one at a time
        // because each of them may push onto the same path.
        fn parts(layouts: &[Layout], held: &[LayoutId], path: &mut Vec<LayoutId>) -> Admission {
            let mut answer = Admission::Always;
            for one in held {
                answer = answer.max(walk(layouts, *one, path));
            }
            answer
        }
        let answer = match &layouts[layout.index()].shape {
            // The scalars a key may be, and the string.
            Shape::Word(Repr::Unit | Repr::Bool | Repr::Int | Repr::Duration) | Shape::Str => {
                Admission::Always
            }
            // A set's members are keys by construction, so nesting one never
            // fails and nothing inside it is asked about.
            Shape::Members { .. } => Admission::Always,
            // An array is its element, and a map is its *value*: a map's keys
            // are keys by construction too, so only its values need asking.
            Shape::Elements {
                elem,
                growable: false,
            } => walk(layouts, *elem, path),
            Shape::Entries { value, .. } => walk(layouts, *value, path),
            Shape::Struct { fields, .. } => {
                let held: Vec<LayoutId> = fields.iter().map(|field| field.layout).collect();
                parts(layouts, &held, path)
            }
            Shape::Enum { cases, .. } => {
                let held: Vec<LayoutId> = cases
                    .iter()
                    .flat_map(|case| case.parts.iter().map(|part| part.layout))
                    .collect();
                parts(layouts, &held, path)
            }
            // A box is decided by `std.dynamic.refusesKey` over a view of it,
            // so it is a part a walk can answer for like any other: some of
            // its values are refused, and a call tells which.
            Shape::Boxed => Admission::Decided,
            // A `Float`, a `Vector` and the growable run beneath it, a byte
            // run and a byte buffer, a closure, a `Shared` cell, a host
            // handle, a task and a task scope. Every one of them is refused,
            // whatever value it holds, and a walk that met one knows so.
            _ => Admission::Decided,
        };
        path.pop();
        answer
    }
    walk(layouts, layout, &mut Vec::new())
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
    /// A walk [`Synth::rendering`] writes.
    Walk,
    /// The runtime's own rendering walk.
    ///
    /// ADR 0064's Decision 4, and **this is the operation whose fallback is
    /// reached from more than a [`Shape::Boxed`]** — which is a real
    /// widening of that decision rather than a reading of it, and is written
    /// down here because [`crate::verify`] checks exactly this list.
    ///
    /// Four shapes reach it, in two kinds:
    ///
    /// - a **layout that does not say what the value is**. A [`Shape::Boxed`]
    ///   keeps its [`LayoutId`] in payload word 0, and a bare `Repr::Ref`
    ///   word is an address whose object's family is read off the object.
    ///   There is nothing here for a walk to be directed by. This is
    ///   Decision 4 exactly as the other three operations have it.
    /// - a **scalar whose text no Cove body can write**. A `Float` renders
    ///   as the shortest decimal that reads back as itself, which is Rust's
    ///   `{}` and is a dragon-4 class algorithm; `Float.format` is not it,
    ///   and ADR 0064's own census puts that variant's migration (row 24) at
    ///   a phase this one does not wait for. A `Duration` renders in the
    ///   largest of six units that divides it exactly, which *is* ordinary
    ///   Cove — a table and a remainder — but it is `std.duration`'s policy
    ///   to write rather than a walk's to inline, and no `std.duration`
    ///   function renders one today.
    ///
    /// So a `Float` field of a struct reaches the intrinsic at that field
    /// and the struct around it is still a walk. That is the whole of what
    /// the widening buys, and it is why it is a *leaf* rule rather than a
    /// whole-layout one: a `Point { x: Float, y: Float }` that fell back
    /// whole would take its name, its field labels and its punctuation back
    /// below with it.
    Dynamic,
}

/// Which of the four [`Rendered`] a value of this shape is.
///
/// Asked of a [`Shape`] and not of a [`LayoutId`] because every answer is
/// decided by the family alone — which is what lets [`crate::verify`] ask it
/// of a finished program's layout table without reaching into this module's
/// [`Pool`].
pub(crate) fn rendered(shape: &Shape) -> Rendered {
    match shape {
        Shape::Str => Rendered::Text,
        Shape::Word(Repr::Int) => Rendered::Digits,
        // See [`Rendered::Dynamic`]. `Shape::Free` is here for the reason a
        // reclaimed run is not a value: there is nothing to walk, and the
        // runtime's own arm is the one that says so.
        Shape::Boxed | Shape::Free | Shape::Word(Repr::Ref | Repr::Float | Repr::Duration) => {
            Rendered::Dynamic
        }
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
        // looked at.
        Operation::Admission => matches!(
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
        // every family has text and only four of them have text a walk
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
        Operation::Rendering => matches!(rendered(shape), Rendered::Walk),
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
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![layout];
    while let Some(at) = pending.pop() {
        if !seen.insert(at) {
            continue;
        }
        let shape = &shapes.layout(at).shape;
        if matches!(shape, Shape::Boxed) {
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
    let mut seen = std::collections::HashSet::new();
    let mut pending = from.to_vec();
    while let Some(at) = pending.pop() {
        if seen.insert(at) {
            pending.extend(parts_of(shapes, at));
        }
    }
    seen
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
    if let Some(id) = pool.synthesized.get(&(op, layout)) {
        return *id;
    }
    let at = pool.appended.len();
    let id = FunctionId((decls + at) as u32);
    pool.appended.push(None);
    pool.synthesized.insert((op, layout), id);

    // A layout's name is not unique — every `Array<T>` is called `Array` —
    // so the id goes in the name as well. It is read by a listing, a profile
    // row and a backtrace, and each of those wants to know *which* `Array`.
    let name: Arc<str> = Arc::from(format!(
        "{}<{}#{}>",
        op.verb(),
        pool.shapes.layout(layout).name,
        layout.0
    ));

    // The parameters are laid out from slot 0 in order, each as wide as its
    // layout, and the answer sits after the last of them.
    let params = op.params(layout);
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
    };
    if op == Operation::Equality && tracked(synth.pool, layout) {
        synth.start_path(layout, &taken);
    } else {
        synth.body(layout, &taken);
    }
    let end = synth.here();
    for at in std::mem::take(&mut synth.leaves) {
        synth.patch(at, end);
    }
    synth.emit(Inst::Return { src: answer });

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
    /// `None` in every walk but an [`Operation::Tracked`] one, which is what
    /// keeps every other walk what it was — [`Synth::compare`] reads this
    /// before it asks [`tracked`], and a walk with no path calls
    /// `equals<L>` where one with a path calls `tracks<L>`.
    path: Option<Path>,
}

/// The two words a tracked walk forwards to the tracked walks it calls.
#[derive(Clone, Copy, Debug)]
struct Path {
    /// The address of the innermost vector pair on the path: word 0 of a
    /// `tracks<Vector<…>>` frame, whose first three words are the pair's left
    /// vector, its right vector and the address of the pair before it.
    at: Slot,
    /// How many pairs the path holds. Nought at the root, where `at` is an
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

    /// The whole of an admission walk: whether this value is one the runtime
    /// is to be asked to refuse.
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
    /// is *nothing* and a call around nothing is not a saving. [`admission`]
    /// is what makes the expansion finite: a layout that holds itself, or one
    /// nested past [`NESTING`], is [`Admission::Dynamic`] and never reaches a
    /// walk at all, so what is expanded here is a finite tree of inline
    /// containment.
    ///
    /// **It never raises**, and the `false` it falls through with is the
    /// whole of its good path. See [`Synth::refuse`].
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
        match admission(self.pool.shapes.all(), layout) {
            // Nothing at all. This is the arm that makes the walk small:
            // every scalar, every string, every set, and every composite
            // built only out of those.
            Admission::Always => return,
            // Unreachable from a walk, because [`admission`] is the greatest
            // of a composite's parts and a call site only makes a function
            // for a layout that is not this — a box is not this either since
            // ADR 0068's Phase 3, and [`Synth::boxed`] decides it. Written out
            // because "should never" is not "cannot", and asking is always
            // correct.
            Admission::Dynamic => {
                self.refuse();
                return;
            }
            Admission::Decided => {}
        }
        if path.contains(&layout) || path.len() >= NESTING {
            self.refuse();
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
    /// reason every other arm's is: the runtime's own walk answers a
    /// discriminant no case names with a sentence of its own, and handing the
    /// question over is what keeps that sentence the one a reader sees. The
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

    /// This value is one the runtime is to be asked about: `true`, and leave.
    ///
    /// **The admission walk never raises, and this is why.** Its refusal is
    /// `` `{method}` cannot use a `{type}` inside `{path}` as a {role} ``
    /// with a `rule:` and a `help:` beside it. Three of those four holes are
    /// something the lowering knows outright — the two names are literals at
    /// all nine standard-library call sites, the type is the layout's, and
    /// the rule and the help are one of two constant pairs. The path is not:
    /// it is composed of field and case names a *synthesized* walk knows
    /// statically only until it reaches through a run or a map, where it
    /// quotes an index or a rendered key computed at run time — issue #461's
    /// kind of hole, and not one [`Inst::Trap`]'s three slots close by
    /// themselves.
    ///
    /// So the walk answers the bit and `super::core` runs the intrinsic,
    /// **at the call site, in the caller's frame, over the caller's key**.
    /// That is not tidiness either: the sentence names the path from the key
    /// to the part that is wrong, the blame chain is read off the live frames
    /// (ADR 0058), and a fallback raised from inside a walk would have added
    /// a frame of its own and a second `in the standard library` label
    /// pointing at the line the first one already pointed at. Decision 8 asks
    /// that the diagnostic not change, and this is what that costs.
    ///
    /// Every point this is reached from is one the runtime really does
    /// refuse — a `Float`, a `Vector`, a handle, a discriminant no case names
    /// — so the intrinsic that follows the `true` raises rather than
    /// answering, and the extra walk it does is on the one path that ends the
    /// run. The `Admission::Dynamic` arms above are the exception and they
    /// are unreachable from a walk, because a composite holding one is
    /// `Dynamic` itself and never gets a walk.
    fn refuse(&mut self) {
        self.constant(true);
        self.leave();
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
            Shape::Closure { .. } => self.literal(buffer, "<fn>"),
            // An address is a place, a handle is the host's, and a task or a
            // scope is the scheduler's; interpolating one would be putting
            // this run's bookkeeping into a string a program prints. A tag is
            // not a value either, for a reason of its own: it is word 0 of an
            // enum, and an enum renders whole through its layout.
            //
            // Everything else — a `String`, an `Int`, a `Float`, a
            // `Duration`, a box — is not a walk at all, so [`walks`] made no
            // function for it and it cannot arrive here.
            _ => self.no_text(),
        }
    }

    /// One part of a rendering: the append where the layout is one, a call
    /// where it is a walk of its own, and the fallback where it is a layout
    /// that does not say what the value is or a scalar no Cove body spells.
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
                // `std.int.renderInto` takes the value *first* and the
                // buffer second, where the two appends take the buffer
                // first. Each is its own declaration's order.
                let args = self.pool.args.intern(vec![
                    Arg {
                        slot: at,
                        layout: shapes::INT,
                    },
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
            Rendered::Walk => {
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
    /// repeated. Not reachable from a checked program: the families it
    /// answers for are word 0 of an enum, an address, a host handle, a task
    /// and a task scope, and none of them is a type a program can
    /// interpolate.
    fn no_text(&mut self) {
        self.trap("this value has no text of its own");
    }

    /// ADR 0064's Decision 4 for the rendering, widened by two scalars.
    ///
    /// See [`Rendered::Dynamic`], which is the list, and `crate::verify`,
    /// which is where the list is enforced rather than promised.
    fn below(&mut self, layout: LayoutId, at: Slot, buffer: Slot) {
        let site = self.pool.intrinsic_site(IntrinsicSite {
            intrinsic: Intrinsic::ValueRenderInto,
            result: shapes::UNIT,
        });
        let args = self.pool.args.intern(vec![
            Arg { slot: at, layout },
            Arg {
                slot: buffer,
                layout: shapes::BYTE_BUFFER,
            },
        ]);
        self.emit(Inst::IntrinsicCall {
            dst: self.answer,
            site,
            args,
        });
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
