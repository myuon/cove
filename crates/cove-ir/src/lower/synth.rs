//! Synthesizing a layout-directed operation as ordinary function IR.
//!
//! [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 3: for the five operations that are *walks of a layout* rather
//! than algorithms over a representation, **lowering synthesizes one private
//! function per `(operation, layout)` actually reached, composed
//! structurally.** This module is that producer. `Any.equals` was its first
//! user and `Value.order` is its second, which is also the first pair to
//! share it: what differs between the two is the shape of the control flow
//! and not the table being walked, so the memo is keyed by the *pair* and
//! each operation is a variant of [`Operation`] and an arm of [`Synth::body`].
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
//! operation* and each is reached from there and from nowhere else:
//! [`Synth::fallback`] is the only place in this crate that emits an
//! [`Intrinsic::AnyEquals`] and [`Synth::dynamic`] the only place that emits
//! an [`Intrinsic::ValueOrder`], and [`crate::verify`] refuses either one
//! whose operands are not boxed. So the two intrinsics survive this
//! migration, and each survives it in one arm rather than in thirty.
//!
//! # Nothing here allocates, and one walk may refuse
//!
//! Every instruction this emits is a load, a comparison, a branch, an
//! [`Inst::Trap`] or a call into another function of the same kind, and
//! neither intrinsic it can reach declares `MAY_ALLOCATE`. So no collection
//! can happen inside a synthesized walk, and a reference temporary this
//! leaves live across a loop turn cannot be holding a stale address when a
//! collector reads it. That is why there is no [`Inst::Clear`] here, and it
//! is a fact about the walk rather than a convention: an arm added later that
//! allocates owes the clears that go with it.
//!
//! What [`Operation::Order`] adds is a walk that can *fail*. A `Float` and a
//! mutable handle are not keys, so where equality falls through to `false`
//! the order raises — and it raises the sentence the intrinsic raises, which
//! is a **constant**. `Inst::Trap`'s [`crate::StrId`] is chosen by the
//! lowering that emits it, exactly as `super::pattern`'s uncovered `match`
//! and `super::dispatch`'s undispatchable call choose theirs, so a refusal
//! whose wording does not quote a value computed at run time is one synthesis
//! can write. Issue #461 is about the other kind, and none of these are it.

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
/// Three of the five are still to come — `ValueAdmitKey` and
/// `ValueRenderInto` are walks of the same shape over the same table, and
/// `ValueRefuseDuplicate` is an error construction blocked on issue #461 —
/// and the key of the memo is the *pair* rather than the layout so that
/// adding one is a variant here and an arm in [`Synth::body`] rather than a
/// second memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Operation {
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
}

impl Operation {
    /// What the synthesized function is called, before its layout.
    fn verb(self) -> &'static str {
        match self {
            Operation::Equality => "equals",
            Operation::Order => "order",
        }
    }

    /// The layout of what it answers.
    fn answers(self) -> LayoutId {
        match self {
            Operation::Equality => shapes::BOOL,
            Operation::Order => shapes::INT,
        }
    }
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
/// executes `Value.order` at no sites and on no turn. A synthesis that
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
pub(super) fn walks(op: Operation, shape: &Shape) -> bool {
    match op {
        Operation::Equality => matches!(
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
    fn body(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        match self.op {
            Operation::Equality => self.equality(layout, a, b),
            Operation::Order => self.ordering(layout, a, b),
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

    /// ADR 0064's Decision 4 again, for the order.
    ///
    /// [`Synth::fallback`]'s argument word for word: a box's family is a
    /// [`LayoutId`] in its own payload word 0, so there is no layout here to
    /// direct a walk with and the runtime's own walk is what answers. It is
    /// reached from [`Shape::Boxed`] and from nowhere else, and
    /// [`crate::verify`] is where that is enforced rather than promised.
    fn dynamic(&mut self, layout: LayoutId, a: Slot, b: Slot) {
        let site = self.pool.intrinsic_site(IntrinsicSite {
            intrinsic: Intrinsic::ValueOrder,
            result: shapes::INT,
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

    /// A value that is not a key, refused in `key::not_a_key`'s words.
    ///
    /// The sentence is a constant, which is why synthesis may raise it:
    /// [`Inst::Trap`]'s string is chosen by the lowering that emits it, and
    /// this one quotes nothing computed at run time. It is not reachable from
    /// a checked program — `core.admitKey` refuses such a key before a single
    /// comparison is made, which is `Map.get`'s and `Set.of`'s first line —
    /// and it is written out for the reason the runtime's own arm is: "should
    /// never" is not "cannot", and a silent wrong answer from a comparison
    /// costs more than the arm that reports one.
    fn not_a_key(&mut self) {
        let message = self
            .pool
            .string("this value cannot be a map key or a set element");
        self.emit(Inst::Trap { message });
    }

    /// A value in a case its layout does not have, in `key::wrong_case`'s
    /// words.
    ///
    /// Constant too: the name is the layout's, which is known to the
    /// lowering, and nothing else is quoted.
    fn wrong_case(&mut self, name: &str) {
        let message = self
            .pool
            .string(&format!("this `{name}` is in a case it does not have"));
        self.emit(Inst::Trap { message });
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
