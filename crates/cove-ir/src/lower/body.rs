//! The builder one function's instructions are emitted through.
//!
//! Everything an emitter needs and nothing about any one construct. The
//! constructs are `super::expr`, `super::call`, `super::dispatch` and
//! `super::task`, each of which is a second `impl Body`, so the whole of
//! what is here is `pub(super)`: there is no smaller boundary to draw
//! between a builder and the four modules that are its only callers.
//!
//! Three things are kept together here rather than beside whichever
//! construct first needed them.
//!
//! [`Body::emit`] is where the operand depths are tracked, and it tracks
//! them by asking `super::validate`'s `stack_shape` — the same description
//! `validate` simulates from, so an emitter and the check over what it
//! emitted cannot disagree about what an instruction does. An instruction
//! control cannot reach is not kept, which is what leaves a listing with
//! nothing in it the VM could never execute.
//!
//! The slot rules are one set of rules over the three regions of one frame
//! numbering: a scope is a [`Mark`] taken before it and restored after it, a
//! shadow is a new slot rather than an overwrite, and each frame size is a
//! high-water mark rather than a count. An emitter counts within its own
//! region as it allocates — [`Body::allocate`] draws from one of three
//! per-region counters — and [`Body::finish`] is what turns each
//! region-local count into the one number every instruction after lowering
//! carries.
//!
//! And the questions put to the checker are asked here, once each, so that
//! the answer one construct acts on and the answer another acts on are the
//! same answer. [`Body::scalar_of`] and [`Body::slot_kind`] are
//! `super::convention`'s `scalar_of_ty` asked about an expression, and
//! [`Body::binary_inst`] and [`Body::field_inst`] are the two places a
//! settled type becomes a different instruction.

use std::collections::BTreeMap;

use cove_diag::Span;
use cove_schema::hosts;
use cove_sema::typeck::Ty;
use cove_sema::MethodTarget;
use cove_syntax::ast::{
    BinaryOp as SourceBinary, EnumDecl, Expr, ExprId, ExprKind, GenericParam, Param, StructDecl,
    Type,
};

use crate::{BinaryOp, Const, EnumId, Inst, IntOp, Scalar, SlotKind, Unsupported, ValueKind};

use super::convention::{scalar_of_ty, slot_kind_of};
use super::index::{Key, Lowering};
use super::validate::{stack_shape, Shape};

/// One live binding: the slot it occupies, the name that reaches it, and
/// whether source may write it.
///
/// A hidden binding has no name. A `for` header needs somewhere to keep what
/// it walks, and those places are slots like any other — they simply cannot
/// be reached from source, because no Cove name resolves to them.
///
/// Whether source may *write* the binding is not here. It was, as `is_var`
/// carried through the lowering, and ADR 0021 moved the rule to `cove-sema`
/// — so the answer is one this pass would only be repeating, and repeating
/// it is how the two could come apart.
pub(super) struct Binding<'a> {
    pub(super) name: Option<&'a str>,
    pub(super) slot: u32,
    /// Which stack the slot lives in, decided when it was declared and never
    /// revisited: a binding's type does not change, so neither does where it
    /// is kept.
    pub(super) kind: SlotKind,
}

/// Where a scope begins: [`Body::scope`] takes one and [`Body::release`]
/// restores it, which is what ends the scope.
///
/// The three slot counters are counted separately, one per region of the one
/// frame numbering, so ending a scope has to roll all of them back, not just
/// how many bindings are live. Each is a region-local count while the body
/// is being emitted — [`Body::finish`] is where a count becomes a number in
/// the one numbering, not here.
#[derive(Clone, Copy)]
pub(super) struct Mark {
    pub(super) live: usize,
    pub(super) next_value: u32,
    pub(super) next_scalar: u32,
    pub(super) next_place: u32,
}

/// A jump target, resolved once the instruction it points at exists.
struct Label {
    at: Option<u32>,
    /// The operand-stack depths control arrives here with, taken from the
    /// first reachable jump that names it.
    pub(super) depth: Option<Depth>,
}

/// How much stands on each of the three operand stacks.
///
/// Three numbers rather than one because there are three stacks. Every join
/// point has to be arrived at with the same amount on all of them, and
/// `validate` simulates all of them, so tracking one and guessing the rest
/// would be tracking none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Depth {
    pub(super) values: u32,
    pub(super) scalars: u32,
    pub(super) places: u32,
}

impl Depth {
    /// Every stack empty, which is where a body and a loop's operands start.
    pub(super) const EMPTY: Depth = Depth {
        values: 0,
        scalars: 0,
        places: 0,
    };

    /// The depths after one instruction of this shape has run.
    pub(super) fn after(self, shape: Shape) -> Depth {
        Depth {
            values: self.values.saturating_sub(shape.values.0) + shape.values.1,
            scalars: self.scalars.saturating_sub(shape.scalars.0) + shape.scalars.1,
            places: self.places.saturating_sub(shape.places.0) + shape.places.1,
        }
    }
}

/// The loop a `break` or a `continue` leaves.
pub(super) struct LoopFrame {
    pub(super) break_to: usize,
    pub(super) continue_to: usize,
    /// How many task scopes were open when the loop began, so that a `break`
    /// or a `continue` written inside one knows how many it is leaving.
    pub(super) scopes: usize,
    /// The operand-stack depths the loop runs at, which is what a `break`
    /// written inside a half-evaluated expression has to get back down to —
    /// on every stack, because a half-evaluated `a + b` can have left
    /// something on any of them.
    pub(super) depth: Depth,
}

/// Whether an expression's value is wanted.
///
/// An expression lowered for its **value** leaves exactly one thing on the
/// operand stack. One lowered for its **effect** leaves nothing. Both do
/// everything the expression does — a call is still made, a store still
/// happens — and they differ only in whether a value nobody reads is built.
///
/// The distinction is worth having because `()` is a value here. An
/// assignment, a `while`, a `for`, and an `if` with no `else` all answer
/// `()`, and a statement discards whatever it is handed; lowered for value
/// each of them therefore pushes a `Unit` for a `Pop` to take away again.
/// That is six of the twenty-five instructions `benches/arith` runs per
/// iteration, and every one of them moves a `Value` and runs its drop glue.
///
/// [`Position::Effect`] reaches inside the constructs that have an inside: a
/// block lowers its tail for effect, an `if`/`else` lowers both branches for
/// effect, and a `match` lowers every arm. The saving is taken where the
/// value would have been built rather than where it would have been thrown
/// away, so it reaches a `Unit` built three blocks down.
///
/// What it does not do is decide that anything need not run. Which calls are
/// pure is not a question this pass asks, so an expression whose value is
/// unwanted is still lowered in full and only its result goes missing.
///
/// [`Position::Scalar`] is the value position on the other stack, and it
/// reaches inside the same three constructs for the same reason. An `if`
/// whose branches are integers should leave an integer, not build a `Value`
/// in each branch for a boundary instruction to unwrap again — and the
/// saving is only there if the position reaches the branch, because the
/// branch is where the value would have been built.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Position {
    /// Something reads what this leaves, on the value stack.
    Value,
    /// Nothing does.
    Effect,
    /// Something reads what this leaves, on the scalar stack.
    ///
    /// Entered only where the checker settled the expression's type as `Int`
    /// or `Bool`, so what arrives is what the instruction reading it was
    /// promised. `Body::expr_scalar` is the way in and the way every leaf is
    /// lowered; a construct with an inside hands this down and lets its
    /// branches, tails, and arms be the leaves.
    Scalar,
}

/// What lowering one body produced, on its way into a [`Function`].
///
/// [`Function`]: crate::Function
pub(super) struct Finished {
    pub(super) code: Vec<Inst>,
    pub(super) spans: Vec<Span>,
    pub(super) arg_spans: BTreeMap<u32, Vec<Span>>,
    pub(super) value_frame_size: u32,
    pub(super) scalar_frame_size: u32,
    pub(super) place_frame_size: u32,
    /// The kind of every slot this function's frame has, in the one
    /// numbering's final order — [`Function::slots`](crate::Function::slots)
    /// itself. `Body::finish` builds this directly rather than handing
    /// `super::convention` three region layouts to concatenate, because the
    /// final order is no longer three regions end to end: see `Body::finish`
    /// for where each of a body's region-local numbers lands.
    pub(super) slots: Vec<SlotKind>,
    /// Each slot's offset within its own region — how many slots of the same
    /// region come before it — in the same order as `slots`. This is
    /// [`Function::offset`](crate::Function::offset): what a backend that
    /// keeps the three regions in separate arrays adds to that region's base
    /// to find a slot physically, now that a slot's number in the one
    /// numbering is no longer that offset by construction.
    pub(super) offsets: Vec<u32>,
    /// The final slot number that scalar-region-local count `j` —
    /// [`Body::allocate`]'s return value for a scalar binding — was turned
    /// into, indexed by `j`.
    ///
    /// A capture's slot is reserved by `Body::allocate` before the body is
    /// finished, and unlike a load or a store it is not an instruction
    /// operand `Body::finish` can patch in place: `super::convention` reads
    /// its final number here instead, once, after the body is finished.
    pub(super) scalar_slot_of: Vec<u32>,
    /// The same translation for the value region.
    pub(super) value_slot_of: Vec<u32>,
}

/// Everything one function's instructions are built from.
pub(super) struct Body<'a, 'l> {
    pub(super) outer: &'l mut Lowering<'a>,
    pub(super) module: &'a str,
    /// Which stack this function's answer travels on, which decides both
    /// which return instruction it ends in and where every `return` inside
    /// it leaves its operand. Read from the declaration's signature once, in
    /// [`Lowering::function`].
    pub(super) returns: SlotKind,
    /// The conversion this function's written return type asks for, emitted
    /// before every one of its returns.
    ///
    /// `Interpreter::call_target` converts what a body answered against the type
    /// the declaration *wrote*, so the conversion belongs to the callee and
    /// not to the call: a declaration with a `dyn Trait` return type answers
    /// a trait object whichever call site asked. Kept here rather than
    /// re-derived at each `return`, because a body has one return type and a
    /// `return` written inside a `match` arm has no declaration in reach.
    pub(super) dyn_return: Option<Inst>,
    /// The type parameters this declaration writes, with the traits each is
    /// bounded by.
    ///
    /// A method call on a value whose type is one of them resolves through
    /// its bounds — that is what a bound is written for — so this is what
    /// [`Body::bound_of`] searches. Empty for a lambda, whose body has no
    /// declaration of its own to have written any.
    pub(super) generics: &'a [GenericParam],
    /// The trait `Self` is bounded by, inside a trait's default body.
    ///
    /// `check_trait_defaults` checks a default body once with `self` typed as
    /// a rigid `Self` bounded by that trait, so a call on `self` there is a
    /// call through a bound like any other — but the parameter is not
    /// written in the declaration, because the declaration is one
    /// `check_conformance` synthesized. This is the bound it would have
    /// written.
    pub(super) self_bound: Option<&'a str>,
    pub(super) code: Vec<Inst>,
    pub(super) spans: Vec<Span>,
    /// The operand-stack depths, or `None` where control cannot arrive.
    ///
    /// `return`, `break`, and `continue` are expressions, so the
    /// instructions written after one are unreachable and have no depth to
    /// speak of. Tracking that rather than guessing is what keeps a later
    /// join point honest.
    pub(super) depth: Option<Depth>,
    pub(super) live: Vec<Binding<'a>>,
    /// The kind of every value-region number handed out so far, in
    /// region-local order — `self` if there is a receiver, then parameters,
    /// then every `Value` local and temporary declare into this one push at
    /// a time; a scope reusing a number records nothing new, because the
    /// number is already here with the kind it will always have.
    ///
    /// This is region-local, not the one frame numbering: a value parameter
    /// and a value local can both be index 0 here, of a body whose scalar or
    /// place region also has slot 0. [`Body::finish`] is what turns each
    /// index into a number in the one numbering, by the permutation
    /// documented there.
    ///
    /// Its length is the high-water mark of value slots handed out, which
    /// [`Body::finish`] carries into `Finished::value_frame_size`. Kept as
    /// the layout itself and not as a separate count, because a count is a
    /// fact the layout already states and restating it is exactly the kind
    /// of second description that could drift from the first.
    pub(super) value_layout: Vec<SlotKind>,
    /// The kind of every scalar-region number handed out so far, in
    /// region-local order: every `Int` or `Bool` parameter, local, and
    /// temporary. Its length is the width of the scalar region — see
    /// `value_layout` for why this is a region-local index and not yet a
    /// number in the one frame numbering.
    ///
    /// This is the one layout of the three that can disagree with itself
    /// across two scopes that reuse the same number — see
    /// [`Body::allocate`] for why and what is done about it.
    pub(super) scalar_layout: Vec<SlotKind>,
    /// The kind of every place-region number handed out so far, in
    /// region-local order, which is every `var` parameter and a `var self`
    /// receiver: nothing a body declares takes one. Its length is the width
    /// of the place region.
    pub(super) place_layout: Vec<SlotKind>,
    /// The next value slot number to hand out, restored when a scope ends.
    ///
    /// Counted within the value region alone while the body is emitted:
    /// [`Body::finish`] permutes every number an instruction carries into
    /// the one frame numbering's, once, after the whole body is emitted.
    pub(super) next_value: u32,
    /// The next scalar slot number to hand out, restored when a scope ends.
    ///
    /// Counted within the scalar region alone, for the same reason
    /// `next_value` is — see [`Body::finish`]. It used to need no further
    /// adjustment there, because the scalar region began the one numbering
    /// and its region-local count already was its number in it; that stopped
    /// being true once a parameter of another kind could be numbered ahead
    /// of a body's own scalar slots.
    pub(super) next_scalar: u32,
    /// The next place slot number to hand out, restored when a scope ends.
    ///
    /// Counted within the place region alone while the body is emitted, for
    /// the reason `next_value` is.
    pub(super) next_place: u32,
    labels: Vec<Label>,
    patches: Vec<(usize, usize)>,
    pub(super) loops: Vec<LoopFrame>,
    /// How many task scopes are open around the instruction being emitted.
    ///
    /// A `break` or a `continue` leaves every scope written between it and
    /// its loop without reaching the `Inst::LeaveScope` below each of them,
    /// so it emits one `Inst::CancelScope` per scope it leaves — which is
    /// this count against the one [`LoopFrame`] recorded. Every other early
    /// exit leaves the frame as well, and the VM cancels what a popped frame
    /// had open for itself; see [`Inst::EnterScope`].
    pub(super) open_scopes: usize,
    /// The argument spans of the instructions whose diagnostic quotes source,
    /// which today is the two assertions and nothing else.
    pub(super) arg_spans: BTreeMap<u32, Vec<Span>>,
}

impl<'a, 'l> Body<'a, 'l> {
    pub(super) fn new(outer: &'l mut Lowering<'a>, module: &'a str) -> Body<'a, 'l> {
        Body {
            outer,
            module,
            returns: SlotKind::Value(ValueKind::Unknown),
            dyn_return: None,
            generics: &[],
            self_bound: None,
            code: Vec::new(),
            spans: Vec::new(),
            depth: Some(Depth::EMPTY),
            live: Vec::new(),
            value_layout: Vec::new(),
            scalar_layout: Vec::new(),
            place_layout: Vec::new(),
            next_value: 0,
            next_scalar: 0,
            next_place: 0,
            labels: Vec::new(),
            patches: Vec::new(),
            loops: Vec::new(),
            open_scopes: 0,
            arg_spans: BTreeMap::new(),
        }
    }

    /// The finished instructions, with every jump pointing at a real one and
    /// every slot named by its number in the one frame numbering.
    ///
    /// `params` is the calling convention this body was lowered under — the
    /// same `Vec<SlotKind>` `super::convention` passes a call site and packs
    /// into [`Function::params`](crate::Function::params) — read here for
    /// the one fact only the caller of `finish` still holds: which
    /// region-local numbers a parameter claimed, and in what declaration
    /// order.
    ///
    /// # Why the slot numbers are settled here and not as they were emitted
    ///
    /// A parameter's argument arrives on the stack its own kind names and
    /// becomes that stack's next slot without moving — that is what a
    /// calling convention *is* — so every parameter, whichever kind it is,
    /// occupies the first region-local number its own region ever hands out.
    /// The one numbering has to put every parameter in `0..arity`, in
    /// declaration order, so that a slot's number agrees with where its
    /// argument physically arrives; and it still has to group everything
    /// after `arity` by region, because that is what lets a backend keep the
    /// three regions in three separate arrays. Both are true together only
    /// because the numbering is a *permutation* of the region-local numbers
    /// an emitter handed out, not the running sum this used to be.
    ///
    /// For a region-local number `j` in a region that has `p` parameters of
    /// its own kind: if `j < p`, `j` is the region-local number of some
    /// parameter — parameters are allocated before anything else in each
    /// region, in declaration order, so `j` is the `j`-th parameter of that
    /// region's kind — and the answer is that parameter's declaration index,
    /// which is where it stands in `params` and therefore its slot in
    /// `0..arity`. Otherwise `j` is past every parameter this region has, and
    /// the answer is `arity` plus however many non-parameter slots the
    /// regions before this one in the numbering's order hold, plus `j - p`.
    ///
    /// Every one of those quantities is a **high-water mark**: a fact about
    /// the whole body that is not known until the whole body is emitted,
    /// because a scope hands its slot numbers back when it ends. So an
    /// emitter counts within its own region, where the counting rule is
    /// local, and the permutation above is computed once here, when every
    /// region's width is finally settled, and applied to every slot number
    /// an instruction carries and to the per-region layouts that become
    /// [`Finished::slots`] and [`Finished::offsets`]. Nothing downstream sees
    /// the intermediate, region-local form — [`Finished`] is what
    /// `super::convention` builds a [`Function`] out of — and
    /// `super::validate` checks the result against
    /// [`Function::region_of`](crate::Function::region_of) rather than
    /// against three sizes, so a slip in this function is caught by the pass
    /// that exists to catch it.
    pub(super) fn finish(mut self, params: &[SlotKind]) -> Finished {
        for (pc, label) in &self.patches {
            let target = self.labels[*label]
                .at
                .expect("every label a jump names is bound");
            match &mut self.code[*pc] {
                Inst::Jump(to)
                | Inst::JumpIfFalse(to)
                | Inst::JumpIfTrue(to)
                | Inst::JumpIfFalseScalar(to)
                | Inst::JumpIfTrueScalar(to) => *to = target,
                other => unreachable!("a patch points at a jump, not {other:?}"),
            }
        }
        let scalar_frame_size = self.scalar_layout.len() as u32;
        let value_frame_size = self.value_layout.len() as u32;
        let place_frame_size = self.place_layout.len() as u32;
        let arity = params.len() as u32;

        // The declaration index of each parameter of each kind, in
        // declaration order: where region-local numbers `0..p` of that
        // region land, because a parameter is always allocated before
        // anything else its own region ever hands out.
        let mut scalar_params = Vec::new();
        let mut value_params = Vec::new();
        let mut place_params = Vec::new();
        for (at, kind) in params.iter().enumerate() {
            match kind {
                SlotKind::Scalar(_) => scalar_params.push(at as u32),
                SlotKind::Value(_) => value_params.push(at as u32),
                SlotKind::Place => place_params.push(at as u32),
            }
        }
        let p_scalar = scalar_params.len() as u32;
        let p_value = value_params.len() as u32;
        let p_place = place_params.len() as u32;
        // Where each region's non-parameter tail begins, past the `arity`
        // block every parameter takes a slot in regardless of its own kind —
        // in the numbering's order, scalar tail first, then value, then
        // place.
        let base_value = scalar_frame_size - p_scalar;
        let base_place = base_value + value_frame_size - p_value;

        let permute_scalar = |j: u32| -> u32 {
            scalar_params
                .get(j as usize)
                .copied()
                .unwrap_or_else(|| arity + (j - p_scalar))
        };
        let permute_value = |j: u32| -> u32 {
            value_params
                .get(j as usize)
                .copied()
                .unwrap_or_else(|| arity + base_value + (j - p_value))
        };
        let permute_place = |j: u32| -> u32 {
            place_params
                .get(j as usize)
                .copied()
                .unwrap_or_else(|| arity + base_place + (j - p_place))
        };

        for inst in &mut self.code {
            match inst {
                Inst::LoadScalar(slot) | Inst::StoreScalar(slot) | Inst::PlaceScalar(slot, _) => {
                    *slot = permute_scalar(*slot);
                }
                Inst::LoadLocal(slot) | Inst::StoreLocal(slot) | Inst::PlaceLocal(slot) => {
                    *slot = permute_value(*slot);
                }
                Inst::LoadPlace(slot) => *slot = permute_place(*slot),
                _ => {}
            }
        }

        let total = (scalar_frame_size + value_frame_size + place_frame_size) as usize;
        let mut slots = vec![SlotKind::Value(ValueKind::Unknown); total];
        let mut offsets = vec![0u32; total];
        for (j, kind) in self.scalar_layout.iter().enumerate() {
            let slot = permute_scalar(j as u32) as usize;
            slots[slot] = *kind;
            offsets[slot] = j as u32;
        }
        for (j, kind) in self.value_layout.iter().enumerate() {
            let slot = permute_value(j as u32) as usize;
            slots[slot] = *kind;
            offsets[slot] = j as u32;
        }
        for (j, kind) in self.place_layout.iter().enumerate() {
            let slot = permute_place(j as u32) as usize;
            slots[slot] = *kind;
            offsets[slot] = j as u32;
        }
        let scalar_slot_of = (0..scalar_frame_size).map(permute_scalar).collect();
        let value_slot_of = (0..value_frame_size).map(permute_value).collect();

        Finished {
            code: self.code,
            spans: self.spans,
            arg_spans: self.arg_spans,
            value_frame_size,
            scalar_frame_size,
            place_frame_size,
            slots,
            offsets,
            scalar_slot_of,
            value_slot_of,
        }
    }

    // ------------------------------------------------------------ emitting

    /// Emits one instruction, unless control cannot reach it.
    ///
    /// The expressions after a `return`, a `break`, or a `continue` are
    /// lowered — an unsupported construct written there is still refused —
    /// but nothing they would emit can run, so nothing is kept. That is what
    /// leaves a listing with no instruction in it that the VM could never
    /// execute.
    pub(super) fn emit(&mut self, inst: Inst, span: Span) {
        let Some(depth) = self.depth else {
            return;
        };
        self.depth = Some(depth.after(stack_shape(&self.outer.structs, inst)));
        if matches!(
            inst,
            Inst::Return | Inst::ReturnScalar | Inst::Jump(_) | Inst::NoMatch
        ) {
            self.depth = None;
        }
        self.code.push(inst);
        self.spans.push(span);
    }

    /// The return a function ends in, emitted even where control cannot fall
    /// into it.
    ///
    /// A body whose last expression is itself a `return` leaves nothing to
    /// fall through, and a function still has to end in the instruction that
    /// ends a function: [`validate`] asks for one, and a VM that ran off the
    /// end would have nowhere to go.
    ///
    /// Which one it is, and which stack the body left its answer on, are the
    /// same question — the function's `returns` — so a body that already
    /// ends in either of the two is left alone.
    ///
    /// [`validate`]: crate::lower::validate
    pub(super) fn emit_final_return(&mut self, span: Span) {
        let (inst, arrival) = match self.returns {
            SlotKind::Value(_) => (
                Inst::Return,
                Depth {
                    values: 1,
                    scalars: 0,
                    places: 0,
                },
            ),
            SlotKind::Scalar(_) => (
                Inst::ReturnScalar,
                Depth {
                    values: 0,
                    scalars: 1,
                    places: 0,
                },
            ),
            // No function answers a place, so no function ends in a return
            // that reads one: `slot_kind_of` never says `Place`, and
            // `Lowering::function` reads `returns` from it alone.
            SlotKind::Place => unreachable!("a function does not answer a place"),
        };
        if self.depth.is_none() {
            if matches!(self.code.last(), Some(Inst::Return | Inst::ReturnScalar)) {
                return;
            }
            self.depth = Some(arrival);
        }
        if inst == Inst::Return {
            self.emit_dyn_return(span);
        }
        self.emit(inst, span);
    }

    /// The conversion a `dyn` return type asks for, before the return that
    /// carries the answer out.
    ///
    /// Every return of a function reaches this, because
    /// `Interpreter::call_target` converts the one value a call answered and does
    /// not ask which `return` produced it.
    pub(super) fn emit_dyn_return(&mut self, span: Span) {
        if let Some(inst) = self.dyn_return {
            self.emit(inst, span);
        }
    }

    /// Emits the conversion a type written in `module` asks for, and nothing
    /// where it asks for none.
    ///
    /// What is converted is the top of the value stack, which is where every
    /// site the interpreter coerces at has left its value: a parameter just
    /// read back out of its slot, a default just computed, an annotated
    /// `let`'s value, and a struct field's argument.
    ///
    /// `module` is the module the type was *written* in, which is not always
    /// the one this body belongs to: a struct's fields are written where the
    /// struct is declared, and `Interpreter::init_struct` passes that module
    /// to `coerce` for exactly this reason — a trait's qualified name is read
    /// against the module that wrote the annotation.
    pub(super) fn coerce_to(&mut self, module: &str, ty: &Type, span: Span) {
        let Some((trait_name, depth)) = self.outer.dyn_conversion(module, ty) else {
            return;
        };
        let trait_name = self.outer.name(&trait_name);
        self.emit(Inst::MakeDyn { trait_name, depth }, span);
    }

    /// The conversion a parameter's written type asks for, made where
    /// `bind_params` makes it.
    ///
    /// A parameter written `dyn Trait` receives a trait object, and the
    /// interpreter builds it as the parameter is *bound* — in declaration
    /// order, before the next parameter's default is evaluated, which is
    /// what lets that default read this one already converted. So it is
    /// emitted in the callee's prologue and not at the call site: a call
    /// knows nothing about the callee's annotations, and a call through a
    /// value or through a `dyn` knows nothing about the callee at all.
    ///
    /// `in_slot` says where the value is. A supplied parameter is already
    /// standing in its slot, so it is read out, converted, and written back;
    /// a parameter left to its default has just been computed onto the
    /// stack, and the store that follows is the caller's.
    ///
    /// Two shapes are left alone because `bind_params` leaves them alone: a
    /// `var` parameter, which names the caller's storage rather than
    /// receiving a copy of it, and a variadic one, which receives the
    /// `Array` the call site collected whatever its element type was
    /// written as.
    pub(super) fn coerce_param(
        &mut self,
        module: &str,
        param: &Param,
        kind: SlotKind,
        slot: u32,
        in_slot: bool,
    ) {
        if param.variadic || !matches!(kind, SlotKind::Value(_)) {
            return;
        }
        let Some(ty) = &param.ty else {
            return;
        };
        let Some((trait_name, depth)) = self.outer.dyn_conversion(module, ty) else {
            return;
        };
        let trait_name = self.outer.name(&trait_name);
        if in_slot {
            self.emit(Inst::LoadLocal(slot), param.span);
        }
        self.emit(Inst::MakeDyn { trait_name, depth }, param.span);
        if in_slot {
            self.emit(Inst::StoreLocal(slot), param.span);
        }
    }

    pub(super) fn constant(&mut self, value: Const, span: Span) {
        let id = self.outer.constant(value);
        self.emit(Inst::Const(id), span);
    }

    /// The `()` a construct that answers one leaves, in the position it was
    /// written in.
    ///
    /// An assignment, a `while`, a `for`, an `if` with no `else`, and a
    /// block with no tail all answer `()`. Lowered for effect none of them
    /// builds one, which is what [`Position::Effect`] is for.
    ///
    /// None of them can be written in scalar position at all: `()` is not a
    /// type the scalar stack holds, and the position is chosen from the type
    /// the checker settled. The boundary is emitted rather than skipped
    /// anyway, so that the depth stays a fact and a mistake shows up as the
    /// VM's own report of a `value-to-scalar` handed something that is not a
    /// scalar, rather than as a stack that is quietly one short.
    pub(super) fn unit_at(&mut self, position: Position, span: Span) {
        match position {
            Position::Effect => {}
            Position::Value => self.constant(Const::Unit, span),
            Position::Scalar => {
                self.constant(Const::Unit, span);
                self.emit(Inst::ValueToScalar, span);
            }
        }
    }

    pub(super) fn label(&mut self) -> usize {
        self.labels.push(Label {
            at: None,
            depth: None,
        });
        self.labels.len() - 1
    }

    /// Emits a jump to `label`, recording the depth control leaves with.
    pub(super) fn jump(&mut self, inst: fn(u32) -> Inst, label: usize, span: Span) {
        let Some(depth) = self.depth else {
            return;
        };
        let arrival = depth.after(stack_shape(&self.outer.structs, inst(0)));
        if self.labels[label].depth.is_none() {
            self.labels[label].depth = Some(arrival);
        }
        let pc = self.code.len();
        self.emit(inst(0), span);
        self.patches.push((pc, label));
    }

    /// Binds `label` to the next instruction.
    ///
    /// Where control could not fall through, the depth the jumps arrive with
    /// is what the code below runs at; that is how the instructions after a
    /// `return` in one arm of an `if` get a depth again.
    pub(super) fn bind(&mut self, label: usize) {
        self.labels[label].at = Some(self.code.len() as u32);
        if self.depth.is_none() {
            self.depth = self.labels[label].depth;
        }
    }

    // --------------------------------------------------------------- slots

    /// Declares a binding, which always takes a slot of its own.
    ///
    /// Shadowing declares rather than overwrites, exactly as `Env::declare`
    /// does, so `let x = 1; let x = 2` is two slots.
    ///
    /// Each region of the one frame numbering is counted on its own while
    /// the body is emitted, so `kind` picks which of the three counters this
    /// draws from — see `Body::allocate`. The number it returns is dense
    /// within that region's own count and is not yet a number in the one
    /// numbering: [`Body::finish`] permutes it, once, after the whole body is
    /// emitted and every region's width is settled.
    pub(super) fn declare(&mut self, name: Option<&'a str>, kind: SlotKind) -> u32 {
        let slot = self.allocate(kind);
        self.declare_at(name, kind, slot);
        slot
    }

    /// Reserves a slot without letting any name reach it yet.
    ///
    /// Split from [`Body::declare`] because a specialisation numbers its
    /// slots in one order and declares its names in another: the supplied
    /// parameters take the first slot numbers, because that is what the
    /// calling convention means, while a default is evaluated in the scope
    /// its own parameter's turn comes in, so a parameter's name must not be
    /// reachable before then. Reserving and naming are therefore two events
    /// here and one event everywhere else.
    ///
    /// The number returned is a count within `kind`'s own region, not yet a
    /// number in the one frame numbering — [`Body::finish`] is what permutes
    /// every slot number a finished body carries into one, once.
    ///
    /// # Why a reused number can be skipped forward
    ///
    /// A scope hands its numbers back when it ends — see [`Body::release`] —
    /// so one region-local number can be handed out more than once in one
    /// body, once per scope that reuses it. Two scopes can reuse it for
    /// bindings of different kinds: `{ let a: Int = 1 }` followed by a
    /// sibling `{ let b: Bool = true }` both start counting their scalars
    /// from the same number, and the first records that number a
    /// `Scalar(Int)` while the second would want it a `Scalar(Bool)`.
    ///
    /// The place region cannot disagree with itself this way — it never
    /// declares anything but `SlotKind::Place` — but the other two both can.
    /// The value region used to be the scalar region's example of a region
    /// that could not: every binding it held was `SlotKind::Value` and
    /// nothing else, so two scopes reusing a value-region number always
    /// agreed. [`ValueKind`](crate::ValueKind) is what ended that: `{ let a:
    /// String = "x" }` followed by a sibling `{ let b: Junk = j }` both start
    /// counting their value slots from the same number, and the first
    /// records that number `Value(Str)` while the second would want it
    /// `Value(Unknown)` — the scalar region's `Int`-then-`Bool` shape, over
    /// the value region's own two cases. So the rule below is written once,
    /// over all three regions, and it is no longer singled out for the one
    /// that needs it — both of the other two do now.
    ///
    /// [`Function::slots`](crate::Function::slots) names one [`SlotKind`]
    /// per number, so it has no way to record a number that meant two
    /// different things in two scopes. So: when the number this call is
    /// about to hand out is already recorded in this region's layout with a
    /// *different* kind than `kind`, that number is left to whichever
    /// binding already claimed it, and the search moves forward — to the
    /// first number that is either past the end of what this region has
    /// ever recorded, or already recorded with the *same* kind — and that
    /// is the number handed out instead. The counter then continues from
    /// there, so the skipped number is given up for good rather than
    /// renegotiated the next time this region's count passes it.
    ///
    /// This costs at most one extra slot per such mismatch, because a skip
    /// either lands on a number nothing has recorded yet — which grows the
    /// layout by exactly the one entry the skip needed — or on a number
    /// already recorded with a matching kind, which grows nothing at all.
    /// That is the price of the table naming an exact kind for every number
    /// rather than approximating: [`Function::slots`] never has to be read
    /// alongside the instruction that touches a slot to know what the slot
    /// holds.
    pub(super) fn allocate(&mut self, kind: SlotKind) -> u32 {
        match kind {
            SlotKind::Value(_) => {
                Self::allocate_in(&mut self.next_value, &mut self.value_layout, kind)
            }
            SlotKind::Scalar(_) => {
                Self::allocate_in(&mut self.next_scalar, &mut self.scalar_layout, kind)
            }
            SlotKind::Place => {
                Self::allocate_in(&mut self.next_place, &mut self.place_layout, kind)
            }
        }
    }

    /// One region's half of [`Body::allocate`]: advances `next` past any
    /// number `layout` already records with a different kind, records
    /// `kind` at the number it settles on if nothing has recorded one
    /// there yet, and returns that number.
    fn allocate_in(next: &mut u32, layout: &mut Vec<SlotKind>, kind: SlotKind) -> u32 {
        let mut slot = *next;
        while (slot as usize) < layout.len() && layout[slot as usize] != kind {
            slot += 1;
        }
        if slot as usize == layout.len() {
            layout.push(kind);
        }
        *next = slot + 1;
        slot
    }

    /// Lets `name` reach a slot [`Body::allocate`] already reserved.
    pub(super) fn declare_at(&mut self, name: Option<&'a str>, kind: SlotKind, slot: u32) {
        self.live.push(Binding { name, slot, kind });
    }

    /// Where a scope begins, to be handed back to [`Body::release`] when it
    /// ends.
    pub(super) fn scope(&self) -> Mark {
        Mark {
            live: self.live.len(),
            next_value: self.next_value,
            next_scalar: self.next_scalar,
            next_place: self.next_place,
        }
    }

    /// Releases every binding declared since `mark`, which is what ends a
    /// scope.
    ///
    /// Every slot counter goes back with them, restored from the mark rather
    /// than recomputed from what remains live: a scope's declarations are
    /// counted across three independent regions of the one frame numbering,
    /// and the mark is what was true of all three counters before any of
    /// them grew.
    pub(super) fn release(&mut self, mark: Mark) {
        self.live.truncate(mark.live);
        self.next_value = mark.next_value;
        self.next_scalar = mark.next_scalar;
        self.next_place = mark.next_place;
    }

    /// The binding `name` reaches: the most recent declaration of it, because
    /// a lookup scans from the top and a shadow was declared later.
    pub(super) fn binding(&self, name: &str) -> Option<&Binding<'a>> {
        self.live
            .iter()
            .rev()
            .find(|binding| binding.name == Some(name))
    }

    /// The slot `name` reaches.
    pub(super) fn lookup(&self, name: &str) -> Option<u32> {
        self.binding(name).map(|binding| binding.slot)
    }

    /// The slot `name` reaches and what it holds, where it is a scalar one.
    ///
    /// `None` for a name that is not a local and for a local kept as a
    /// `Value`, which are the two cases that lower the way they always did.
    pub(super) fn scalar_binding(&self, name: &str) -> Option<(u32, Scalar)> {
        let binding = self.binding(name)?;
        match binding.kind {
            SlotKind::Scalar(what) => Some((binding.slot, what)),
            SlotKind::Value(_) | SlotKind::Place => None,
        }
    }

    /// Whether `expr` is a place: something this pass can address, rather
    /// than something it can only read.
    ///
    /// Walk down through `ExprKind::Field { base, .. }` to the expression's
    /// root, which is a place only where it is an `ExprKind::Ident` naming a
    /// local. A field asks no question of its own; it is a step from a place
    /// to a place, exactly as `Place::field` is.
    ///
    /// Whether source may *write* that place is not asked here and is not
    /// this pass's to answer. `Checker::place_mutability` in `cove-sema` is
    /// the definition and the only statement of it; ADR 0021 says why there
    /// is one rather than three.
    pub(super) fn is_a_place(&self, expr: &'a Expr) -> bool {
        match &expr.kind {
            ExprKind::Ident(name) => self.binding(name).is_some(),
            ExprKind::Field { base, .. } => self.is_a_place(base),
            _ => false,
        }
    }

    /// Builds the place `expr` names, leaving it on the place stack.
    ///
    /// The two forms are the interpreter's two: a name, which is the root,
    /// and a field of one, which is `Place::field` — the base's place with
    /// one more step on the end. A root that is itself a `var` parameter is
    /// the place that parameter *holds* rather than a place naming its slot,
    /// which is what makes a `var` argument passed on as a `var` argument
    /// alias the original binding and not the parameter.
    ///
    /// Mutability is not asked here. Every caller has already asked
    /// [`Body::place_mutability`] and refused in words about what it was
    /// doing — assigning, or calling a method that writes through its
    /// receiver — because "read-only" is the same fact reported differently
    /// depending on who noticed it.
    pub(super) fn place(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        match &expr.kind {
            ExprKind::Ident(name) => {
                let Some(binding) = self.binding(name) else {
                    return Err(Unsupported::new(
                        format!("`{name}` as a place, which is not a local"),
                        expr.span,
                    ));
                };
                let (slot, kind) = (binding.slot, binding.kind);
                match kind {
                    SlotKind::Value(_) => self.emit(Inst::PlaceLocal(slot), expr.span),
                    SlotKind::Place => self.emit(Inst::LoadPlace(slot), expr.span),
                    // A place names a slot, and a slot is a region and a
                    // number in it. So a binding the checker settled as
                    // `Int` or `Bool` is rooted where it lives rather than
                    // moved somewhere a place can reach, which is what issue
                    // #162 asked for and what `Inst::PlaceScalar` is.
                    SlotKind::Scalar(what) => self.emit(Inst::PlaceScalar(slot, what), expr.span),
                }
                Ok(())
            }
            ExprKind::Field { base, name } => {
                // A step is a position, and a position needs the checker to
                // have settled the type it is a position in. A read can fall
                // back to the name — `Inst::GetField` scans a list that is
                // there — and a place cannot, because the same path is
                // walked to write as well and `Inst::PlaceWrite` descends by
                // index.
                let Some(index) = self.field_position(base, &name.node) else {
                    return Err(Unsupported::new(
                        format!(
                            "`{}` as a place, whose field position the checker did not settle",
                            place_text(expr)
                        ),
                        expr.span,
                    ));
                };
                self.place(base)?;
                self.emit(Inst::PlaceField(index), expr.span);
                Ok(())
            }
            _ => Err(Unsupported::new(
                "an expression that is not a place, written where one is needed",
                expr.span,
            )),
        }
    }

    /// Whether an assignment to `target` is a write through a place.
    ///
    /// Two shapes are: a target rooted at a `var` parameter, whose storage
    /// belongs to the caller and cannot be replaced by storing a slot; and a
    /// path of more than one field, which the whole-value update
    /// [`Body::assign_field`] performs has no way to put back — it replaces
    /// a local's struct, and a deeper path would need every struct between
    /// replaced too.
    ///
    /// A single field of a local is left where it was. It is the same write
    /// either way, and the existing lowering is what `benches/field` runs.
    pub(super) fn written_through_a_place(&self, target: &'a Expr) -> bool {
        match &target.kind {
            ExprKind::Ident(name) => self.binding(name).is_some_and(|b| b.kind.is_place()),
            ExprKind::Field { base, .. } => match &base.kind {
                ExprKind::Ident(name) => self.binding(name).is_some_and(|b| b.kind.is_place()),
                ExprKind::Field { .. } => self.is_a_place(target),
                _ => false,
            },
            _ => false,
        }
    }

    // ----------------------------------------------- what the checker knows

    /// The type the checker settled for `expr`, or `None` where it settled
    /// none.
    ///
    /// `None` means the expression was never walked — a tree built by hand,
    /// or a callee that names a declaration rather than producing a value.
    /// It does not mean the checker was unsure: an expression it walked and
    /// could say nothing about answers [`Ty::Unknown`], which is an answer
    /// and is not a type. Every caller here specialises on a settled type,
    /// so both of those fall through to the untyped instruction.
    pub(super) fn settled(&self, expr: &Expr) -> Option<&'a Ty> {
        self.outer.checked.facts.ty(expr.span.file, expr.id)
    }

    /// Whether `receiver` is a handle to a host resource that answers an
    /// operation called `name`.
    ///
    /// [`Ty::Host`] is written the same way whichever kind of type a host
    /// module declares — `http.Response`, which the host hands over, reads
    /// like `http.Server`, which it keeps — so the name alone does not say
    /// whether a value of it is a `Value::Resource`. The schema does:
    /// `declared_type` answers for the data a host gives away, and `resource`
    /// for the handle it keeps, and only the second is called through
    /// `HostRegistry::call_resource`. So both halves are asked, of the module
    /// the qualified name begins with.
    ///
    /// A receiver the checker did not settle answers `false` and keeps the
    /// refusal it had: which method such a call reaches is a question about a
    /// value at run time, and this backend decides it here or not at all.
    pub(super) fn resource_op(&self, receiver: &Expr, name: &str) -> bool {
        let Some(Ty::Host(qualified)) = self.settled(receiver) else {
            return false;
        };
        let Some((module, type_name)) = qualified.rsplit_once('.') else {
            return false;
        };
        hosts::module(module)
            .and_then(|schema| schema.resource(type_name))
            .is_some_and(|resource| resource.operation(name).is_some())
    }

    /// Whether the checker settled that this expression is an `Int`.
    ///
    /// Written as one question because it is asked of both operands of every
    /// operator, and because the two ways of not knowing — an abstention and
    /// an expression that was never walked — have to answer it the same way.
    pub(super) fn is_int(&self, expr: &Expr) -> bool {
        matches!(self.settled(expr), Some(Ty::Int))
    }

    /// What a scalar stack would hold this expression's value as, or `None`
    /// where the checker settled no type that stack can hold.
    ///
    /// The rule [`Body::is_int`] states, asked of both scalar types at once
    /// and for the same reason: an abstention and an expression that was
    /// never walked are not settled types, so neither becomes a scalar.
    ///
    /// The rule itself is [`scalar_of_ty`], so that an expression's storage
    /// and a parameter's storage are decided by one function rather than by
    /// two that could drift apart. Two such rules disagreeing is exactly
    /// what reading the checker's answers is supposed to make impossible.
    pub(super) fn scalar_of(&self, expr: &Expr) -> Option<Scalar> {
        self.settled(expr).and_then(scalar_of_ty)
    }

    /// Where a binding declared from `expr` lives.
    ///
    /// The same question again, because a binding's storage and an operand's
    /// storage are settled by the same fact: a slot the checker proved holds
    /// an `Int` holds an integer word, and a slot it said nothing about holds
    /// what every slot used to. [`slot_kind_of`] is the one rule that answers
    /// it — the same one a parameter's, a field's, and a case's payload go
    /// through — asked here of `expr`'s own settled type, so a `let s =
    /// "hello"` binding a `String` names [`ValueKind::Str`] exactly as a
    /// `String`-typed parameter does, and every other settlement, `None`
    /// included, keeps the binding on the value stack as [`ValueKind::Unknown`].
    pub(super) fn slot_kind(&self, expr: &Expr) -> SlotKind {
        match self.settled(expr) {
            Some(ty) => slot_kind_of(ty),
            None => SlotKind::Value(ValueKind::Unknown),
        }
    }

    /// Whether this expression is *computed* on the scalar stack, rather than
    /// computed on the value stack and moved across.
    ///
    /// It decides which stack a condition is tested on: a `Bool` the scalar
    /// stack already holds is one [`Inst::JumpIfFalseScalar`], and one the
    /// value stack holds would need a [`Inst::ValueToScalar`] first — an
    /// instruction spent to save none.
    pub(super) fn on_scalar_stack(&self, expr: &'a Expr) -> bool {
        match &expr.kind {
            ExprKind::Int(_) | ExprKind::Bool(_) => self.scalar_of(expr).is_some(),
            ExprKind::Ident(name) => self.scalar_binding(name).is_some(),
            // The same threshold `expr_scalar` lowers `&&`/`||` at: one
            // operand already on the scalar stack makes the scalar form
            // cheaper (see `and_or_scalar`'s callers). `condition` asks this
            // and then calls `expr_scalar`, so the two answering differently
            // would mean testing a condition on the stack it was not put on.
            ExprKind::Binary {
                op: SourceBinary::And | SourceBinary::Or,
                lhs,
                rhs,
            } => {
                self.scalar_of(expr) == Some(Scalar::Bool)
                    && (self.on_scalar_stack(lhs) || self.on_scalar_stack(rhs))
            }
            ExprKind::Binary { op, lhs, rhs } => binary_op(*op)
                .is_some_and(|op| matches!(self.binary_inst(op, lhs, rhs), Inst::IntBinary(_))),
            ExprKind::Call { callee, .. } => self.callee_returns(expr.id, callee).is_some(),
            ExprKind::Field { base, name } => self.scalar_field(expr, base, &name.node).is_some(),
            _ => false,
        }
    }

    /// What a call to a declared function leaves on the scalar stack, asked
    /// without lowering the call.
    ///
    /// Only the two callees a name settles on their own: a bare name that is
    /// not a local and reaches a declared function, and a method call the
    /// checker recorded a declaration for. Everything else answers `None`.
    ///
    /// That is allowed to be incomplete because nothing depends on it for
    /// correctness. It decides which stack a condition is *tested* on, and
    /// both answers are lowered correctly whichever this gives: a call that
    /// landed on the other stack crosses it with one boundary instruction.
    /// A wrong answer costs an instruction, so this answers only where a
    /// cheap question settles it.
    pub(super) fn callee_returns(&self, id: ExprId, callee: &'a Expr) -> Option<Scalar> {
        let key = match &callee.kind {
            ExprKind::Ident(name) if self.lookup(name).is_none() => {
                self.outer.function_of(self.module, name)?
            }
            ExprKind::Field { .. } => {
                let target = self.target(id, callee.span)?;
                self.declared_by(target)?
            }
            _ => return None,
        };
        scalar_of_ty(&self.outer.signature(key)?.ret)
    }

    /// The instruction `op` lowers to over these two operands.
    ///
    /// [`Inst::IntBinary`] where the checker settled *both* operands as `Int`
    /// and the operator is one `Int` answers, so that the VM neither examines
    /// the operands nor builds the interpreter's `Result<Value, RuntimeError>`
    /// to discover what it already knew. [`Inst::Binary`] everywhere else,
    /// which is every operand pair the checker did not settle and `is`, which
    /// asks about storage rather than about integers.
    pub(super) fn binary_inst(&self, op: BinaryOp, lhs: &'a Expr, rhs: &'a Expr) -> Inst {
        match int_op(op) {
            Some(op) if self.is_int(lhs) && self.is_int(rhs) => Inst::IntBinary(op),
            _ => Inst::Binary(op),
        }
    }

    /// The instruction a read of `receiver.name` lowers to.
    ///
    /// [`Inst::GetFieldAt`] where the checker settled the receiver's type and
    /// the declaration of that type gives `name` a position, because a
    /// position is an index and a name is a scan. [`Inst::GetField`] wherever
    /// the type was not settled, was settled as something other than a struct
    /// this package declares, or names a field the declaration does not have
    /// — the last of which is not this pass's failure to report, since a
    /// program the checker accepted has no such read.
    pub(super) fn field_inst(&mut self, receiver: &'a Expr, name: &str) -> Inst {
        match self.field_of(receiver, name) {
            Some((owner, decl, at)) => Inst::GetFieldAt {
                of: self.outer.struct_type(owner, decl),
                at,
            },
            None => Inst::GetField(self.outer.name(name)),
        }
    }

    /// The struct a read of `receiver.name` reads, and where `name` stands in
    /// it.
    ///
    /// [`Body::field_position`] is this with the type thrown away, and the two
    /// are one function so that the position and the type a backend reads the
    /// field's kind out of cannot come from two different declarations.
    pub(super) fn field_of(
        &self,
        receiver: &'a Expr,
        name: &str,
    ) -> Option<(&'a str, &'a StructDecl, u32)> {
        let Some(Ty::Struct(named, _)) = self.settled(receiver) else {
            return None;
        };
        let checked = self.outer.checked;
        let (owner, decl) = match named.split_once('.') {
            Some((module, type_name)) => {
                let (module, resolved) = checked.modules.get_key_value(module)?;
                (module.as_str(), &*resolved.structs.get(type_name)?.decl)
            }
            None => self.outer.struct_of(self.module, named)?,
        };
        let index = decl
            .fields
            .iter()
            .position(|field| field.name.node == name)?;
        Some((owner, decl, index as u32))
    }

    /// Where `name` stands among the fields of the struct `receiver` is.
    ///
    /// The order is the declaration's, which is the order a struct's fields
    /// are pushed in and therefore the order they are held in: `make_struct`
    /// pushes them that way and [`crate::Inst::SetField`] replaces one where
    /// it stands, so nothing a lowered program builds holds them otherwise.
    ///
    /// The checker names a type of the module it was checking — bare for a
    /// type that module declares and `module.Name` for one it met through an
    /// import — so a bare name is read against the module this body belongs
    /// to, exactly as source written there would read it.
    pub(super) fn field_position(&self, receiver: &'a Expr, name: &str) -> Option<u32> {
        self.field_of(receiver, name).map(|(_, _, at)| at)
    }

    /// Where `receiver.name` stands, asked only where the read is one
    /// [`Inst::GetFieldAtScalar`] can answer: the receiver's type settled a
    /// position, the same as for [`Inst::GetFieldAt`], *and* the field itself
    /// is a type the scalar stack holds.
    ///
    /// One predicate for the two places that need it — lowering the read
    /// itself and deciding which stack it leaves its answer on
    /// ([`Body::on_scalar_stack`]) — so that they cannot settle it
    /// differently.
    pub(super) fn scalar_field(&self, field: &Expr, receiver: &'a Expr, name: &str) -> Option<u32> {
        self.scalar_of(field)?;
        self.field_position(receiver, name)
    }

    /// The declared enum and the case within it that `subject`'s settled
    /// type names `case_name` as, if the checker settled `subject` as a
    /// type this package declares.
    ///
    /// Mirrors [`Body::field_of`] exactly, over [`Ty::Enum`] in place of
    /// [`Ty::Struct`]: a bare name is read against this body's own module
    /// and a qualified one against the module it names, the same rule a
    /// source-level `Status.Confirmed` reads by. `Result` and `Option` are
    /// not this package's declarations — a `subject` of [`Ty::Option`] or
    /// [`Ty::Result`] answers `None` here, which is what leaves
    /// [`Inst::GetPayload`]'s `of` unset for them; see that variant's own
    /// doc comment for why closing that gap would cost this backend the
    /// precision it already has for `Ok(1)`.
    pub(super) fn variant_case(
        &mut self,
        subject: &Ty,
        case_name: &str,
    ) -> Option<(EnumId, u32, &'a EnumDecl)> {
        let Ty::Enum(named, _) = subject else {
            return None;
        };
        let checked = self.outer.checked;
        let (owner, decl) = match named.split_once('.') {
            Some((module, type_name)) => {
                let (module, resolved) = checked.modules.get_key_value(module)?;
                (module.as_str(), &*resolved.enums.get(type_name)?.decl)
            }
            None => self.outer.enum_of(self.module, named)?,
        };
        let case_index = decl
            .cases
            .iter()
            .position(|case| case.name.node == case_name)?;
        let (of, _) = self.outer.enum_type(owner, decl);
        Some((of, case_index as u32, decl))
    }

    /// The checker's settlement of payload position `at` of case `case_index`
    /// of `decl`, as a full [`Ty`] rather than the [`SlotKind`] alone
    /// [`crate::lower::index::Lowering::enum_type`] keeps.
    ///
    /// Read off the same per-case `cove_sema::Signature` that function reads a
    /// [`SlotKind`] out of — case span keyed, for the reason its own doc
    /// comment gives — but kept whole here, because a *nested* pattern
    /// needs to know what the position is (an enum, and which one) to
    /// resolve its own [`Inst::GetPayload`], not merely whether it is a
    /// reference.
    pub(super) fn case_payload_ty(
        &self,
        decl: &'a EnumDecl,
        case_index: u32,
        at: usize,
    ) -> Option<Ty> {
        let case_span = decl.cases.get(case_index as usize)?.span;
        self.outer
            .checked
            .facts
            .signature(case_span.file, case_span)?
            .params
            .get(at)
            .cloned()
    }

    /// The declaration the checker recorded this call as reaching.
    ///
    /// A method call is written against a value and which declaration it
    /// reaches is decided by that value's type, which is the one thing this
    /// pass cannot work out for itself. Where the checker recorded an answer
    /// there is nothing left to guess at; where it recorded none — a builtin
    /// method, a host operation, a receiver it abstained about —
    /// [`Body::method_call`] asks by name and refuses what a name cannot
    /// settle.
    pub(super) fn target(&self, id: ExprId, span: Span) -> Option<&'a MethodTarget> {
        self.outer.checked.facts.target(span.file, id)
    }

    /// The declaration `target` names, or `None` where this package has none
    /// of that name.
    ///
    /// `None` is not a failure to report. It leaves the call to the
    /// name-based path below, which is where a call the checker said nothing
    /// about goes anyway.
    pub(super) fn declared_by(&self, target: &MethodTarget) -> Option<Key> {
        self.outer
            .method_of(&target.module, &target.type_name, &target.method)
    }
}

/// The dotted name a place is written with in source, for a diagnostic — the
/// same rendering `Interpreter::describe_place` in
/// `crates/cove-runtime/src/interp.rs` produces, since a receiver refused
/// here is a receiver that expression would have described there.
fn place_text(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Ident(name) => name.clone(),
        ExprKind::Field { base, name } => format!("{}.{}", place_text(base), name.node),
        _ => "this expression".to_string(),
    }
}

/// The source binary operator as the IR carries it, or `None` for the two
/// that short-circuit and so are not operators here at all.
pub(super) fn binary_op(op: SourceBinary) -> Option<BinaryOp> {
    Some(match op {
        SourceBinary::Add => BinaryOp::Add,
        SourceBinary::Sub => BinaryOp::Sub,
        SourceBinary::Mul => BinaryOp::Mul,
        SourceBinary::Div => BinaryOp::Div,
        SourceBinary::Rem => BinaryOp::Rem,
        SourceBinary::Eq => BinaryOp::Eq,
        SourceBinary::Ne => BinaryOp::Ne,
        SourceBinary::Lt => BinaryOp::Lt,
        SourceBinary::Le => BinaryOp::Le,
        SourceBinary::Gt => BinaryOp::Gt,
        SourceBinary::Ge => BinaryOp::Ge,
        SourceBinary::Is => BinaryOp::Is,
        SourceBinary::And | SourceBinary::Or => return None,
    })
}

/// What [`Inst::IntBinary`] leaves on the scalar stack.
///
/// Arithmetic answers an `Int` and a comparison answers a `Bool`. The scalar
/// stack carries no tag, so this is where a boundary instruction learns which
/// of the two it is being handed.
pub(super) fn int_result(op: IntOp) -> Scalar {
    match op {
        IntOp::Add | IntOp::Sub | IntOp::Mul | IntOp::Div | IntOp::Rem => Scalar::Int,
        IntOp::Eq | IntOp::Ne | IntOp::Lt | IntOp::Le | IntOp::Gt | IntOp::Ge => Scalar::Bool,
    }
}

/// The conditional jump that reads the stack a condition was left on.
pub(super) fn branch_on(scalar: bool) -> fn(u32) -> Inst {
    if scalar {
        Inst::JumpIfFalseScalar
    } else {
        Inst::JumpIfFalse
    }
}

/// The operator as [`Inst::IntBinary`] carries it, or `None` for one `Int`
/// does not answer.
///
/// `is` is that one. It compares storage rather than value, and an `Int` has
/// none to compare, so there is nothing for a typed instruction to do faster.
fn int_op(op: BinaryOp) -> Option<IntOp> {
    Some(match op {
        BinaryOp::Add => IntOp::Add,
        BinaryOp::Sub => IntOp::Sub,
        BinaryOp::Mul => IntOp::Mul,
        BinaryOp::Div => IntOp::Div,
        BinaryOp::Rem => IntOp::Rem,
        BinaryOp::Eq => IntOp::Eq,
        BinaryOp::Ne => IntOp::Ne,
        BinaryOp::Lt => IntOp::Lt,
        BinaryOp::Le => IntOp::Le,
        BinaryOp::Gt => IntOp::Gt,
        BinaryOp::Ge => IntOp::Ge,
        BinaryOp::Is => return None,
    })
}
