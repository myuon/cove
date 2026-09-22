//! Synthesizing a layout-directed operation as ordinary function IR.
//!
//! [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 3: for the five operations that are *walks of a layout* rather
//! than algorithms over a representation, **lowering synthesizes one private
//! function per `(operation, layout)` actually reached, composed
//! structurally.** This module is that producer, and `Any.equals` is its
//! first user.
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
//! opened. Decision 4 admits exactly one dynamic-layout fallback and it is
//! reached from there and from nowhere else: [`Synth::fallback`] is the only
//! place in this crate that emits an [`Intrinsic::AnyEquals`], and
//! [`crate::verify`] refuses one whose operands are not boxed. So the
//! intrinsic survives this migration, and it survives it in one arm rather
//! than in thirty.
//!
//! # Nothing here allocates
//!
//! Every instruction this emits is a load, a comparison, a branch or a call
//! into another function of the same kind, and the one intrinsic it can reach
//! declares no `MAY_ALLOCATE`. So no collection can happen inside a
//! synthesized walk, and a reference temporary this leaves live across a loop
//! turn cannot be holding a stale address when a collector reads it. That is
//! why there is no [`Inst::Clear`] here, and it is a fact about the walk
//! rather than a convention: an arm added later that allocates owes the
//! clears that go with it.

use std::sync::Arc;

use cove_diag::Span;

use crate::inst::{ArithOp, CmpOp, Compare, Inst, Pc, Slot};
use crate::intrinsic::Intrinsic;
use crate::layout::{Case, LayoutId, Shape};
use crate::program::{Arg, Function, FunctionId, IntrinsicSite, Table, TableId};
use crate::repr::{RefMap, Repr};

use super::shapes;
use super::{Pool, PENDING};

/// The module a synthesized function says it is in.
///
/// Not a module any source can name — `<` is not a name character — so a
/// declaration cannot collide with one and `cove run` cannot reach one. It is
/// here for a listing, a profile row and a debugger's frame name, which are
/// the three readers a synthesized function has.
pub(super) const MODULE: &str = "<synth>";

/// One layout-directed operation ADR 0064's Decision 3 names.
///
/// Four of the five are still to come — `ValueOrder`, `ValueAdmitKey` and
/// `ValueRenderInto` are walks of the same shape over the same table, and
/// `ValueRefuseDuplicate` is an error construction blocked on issue #461 —
/// and the key of the memo is the *pair* rather than the layout so that
/// adding one is a variant here and an arm in [`Synth::body`] rather than a
/// second memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Operation {
    /// `a == b`: whether two values of one layout are the same value.
    Equality,
}

impl Operation {
    /// What the synthesized function is called, before its layout.
    fn verb(self) -> &'static str {
        match self {
            Operation::Equality => "equals",
        }
    }

    /// The layout of what it answers.
    fn answers(self) -> LayoutId {
        match self {
            Operation::Equality => shapes::BOOL,
        }
    }
}

/// Whether `layout` is one this module synthesizes a *function* for, rather
/// than one a call site compares in place.
///
/// The composite families and nothing else: a scalar, a string and a box are
/// one instruction or one intrinsic wherever they are met, so a function for
/// one would be a call around a single instruction. Everything here is a walk
/// of more than one part, which is what a function is for.
pub(super) fn walks(shape: &Shape) -> bool {
    matches!(
        shape,
        Shape::Struct { .. }
            | Shape::Enum { .. }
            | Shape::Elements { .. }
            | Shape::Vector { .. }
            | Shape::Members { .. }
            | Shape::Entries { .. }
    )
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

    let words = pool.shapes.words(layout).to_vec();
    let mut reprs = words.clone();
    reprs.extend_from_slice(&words);
    let left = 0;
    let right = words.len() as Slot;
    let answer = (words.len() * 2) as Slot;
    reprs.push(Repr::Bool);

    let mut synth = Synth {
        pool,
        decls,
        span,
        reprs,
        code: Vec::new(),
        leaves: Vec::new(),
        answer,
    };
    synth.body(layout, left, right);
    let end = synth.here();
    for at in std::mem::take(&mut synth.leaves) {
        synth.patch(at, end);
    }
    synth.emit(Inst::Return { src: answer });

    let function = Function {
        module: Arc::from(MODULE),
        name,
        params: vec![layout, layout],
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

/// One synthesized function being written.
struct Synth<'p> {
    pool: &'p mut Pool,
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
    fn body(&mut self, layout: LayoutId, a: Slot, b: Slot) {
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
    /// [`Body::ordered_by`](super::core)'s [`Compare::Tag`] again.
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
        if walks(&shape) {
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

    /// ADR 0064's Decision 4: the one dynamic-layout boundary.
    ///
    /// A box's family is a [`LayoutId`] in its own payload word 0, so there
    /// is no layout here to direct a walk with and the runtime's own walk is
    /// what answers. It is reached from [`Shape::Boxed`] and from nowhere
    /// else, and [`crate::verify`] is where that is enforced rather than
    /// promised.
    fn fallback(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let site = self.pool.intrinsic_site(IntrinsicSite {
            intrinsic: Intrinsic::AnyEquals,
            result: shapes::BOOL,
        });
        let args = self
            .pool
            .args
            .intern(vec![Arg { slot: a, layout }, Arg { slot: b, layout }]);
        self.emit(Inst::IntrinsicCall {
            dst: self.answer,
            site,
            args,
        });
    }
}
