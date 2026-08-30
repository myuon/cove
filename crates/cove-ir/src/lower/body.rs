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
//! The slot rules are one set of rules over three independently numbered
//! stacks: a scope is a [`Mark`] taken before it and restored after it, a
//! shadow is a new slot rather than an overwrite, and each frame size is a
//! high-water mark rather than a count.
//!
//! And the questions put to the checker are asked here, once each, so that
//! the answer one construct acts on and the answer another acts on are the
//! same answer. [`Body::scalar_of`] and [`Body::slot_kind`] are
//! `super::convention`'s `scalar_of_ty` asked about an expression, and
//! [`Body::binary_inst`] and [`Body::field_inst`] are the two places a
//! settled type becomes a different instruction.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use cove_diag::Span;
use cove_schema::hosts;
use cove_sema::typeck::Ty;
use cove_sema::MethodTarget;
use cove_syntax::ast::{
    BinaryOp as SourceBinary, Expr, ExprId, ExprKind, GenericParam, Param, Type,
};

use crate::{BinaryOp, Const, Inst, IntOp, Scalar, SlotKind, Unsupported};

use super::convention::scalar_of_ty;
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
    /// Which of this function's captures this binding is, for the bindings
    /// that are one.
    ///
    /// A capture is an ordinary value slot, so this changes nothing about
    /// how it is reached; what it changes is which instruction says so.
    /// [`Inst::LoadCapture`] carries the index into
    /// [`Function::captures`] rather than the slot number the index works
    /// out to, because the layout is a fact about the closure and the
    /// capture list is what states it.
    ///
    /// [`Function::captures`]: crate::Function::captures
    pub(super) capture: Option<u32>,
    /// Which stack the slot lives in, decided when it was declared and never
    /// revisited: a binding's type does not change, so neither does where it
    /// is kept.
    pub(super) kind: SlotKind,
}

/// Where a scope begins: [`Body::scope`] takes one and [`Body::release`]
/// restores it, which is what ends the scope.
///
/// The three slot counters are numbered separately, so ending a scope has to
/// roll all of them back, not just how many bindings are live.
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
    /// The high-water mark of value slots handed out: `self` if there is a
    /// receiver, then parameters, then every `Value` local and temporary.
    pub(super) value_frame_size: u32,
    /// The high-water mark of scalar slots handed out: every `Int` or `Bool`
    /// local and temporary.
    pub(super) scalar_frame_size: u32,
    /// The high-water mark of place slots handed out, which is every `var`
    /// parameter and a `var self` receiver: nothing a body declares takes
    /// one.
    pub(super) place_frame_size: u32,
    /// The next value slot number to hand out, restored when a scope ends.
    pub(super) next_value: u32,
    /// The next scalar slot number to hand out, restored when a scope ends.
    pub(super) next_scalar: u32,
    /// The next place slot number to hand out, restored when a scope ends.
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
    /// Every name this body uses as the root of a place — see
    /// [`var_argument_roots`], which collects them before a single
    /// instruction is emitted.
    ///
    /// A binding of one of these names is kept on the value stack even where
    /// the checker settled it as `Int`, because a place is an index into the
    /// value stack and cannot address the scalar one. It is a set of names
    /// rather than of bindings, so it over-approximates across shadowing:
    /// `bump(var total)` written anywhere in a body puts *every* `total` the
    /// body declares on the value stack, including ones no place ever names.
    /// That costs a slot its representation and can cost nothing else,
    /// because both representations hold the same value.
    ///
    /// [`var_argument_roots`]: crate::lower::scan::var_argument_roots
    pub(super) rooted: BTreeSet<&'a str>,
}

impl<'a, 'l> Body<'a, 'l> {
    pub(super) fn new(outer: &'l mut Lowering<'a>, module: &'a str) -> Body<'a, 'l> {
        Body {
            outer,
            module,
            returns: SlotKind::Value,
            dyn_return: None,
            generics: &[],
            self_bound: None,
            code: Vec::new(),
            spans: Vec::new(),
            depth: Some(Depth::EMPTY),
            live: Vec::new(),
            value_frame_size: 0,
            scalar_frame_size: 0,
            place_frame_size: 0,
            next_value: 0,
            next_scalar: 0,
            next_place: 0,
            labels: Vec::new(),
            patches: Vec::new(),
            loops: Vec::new(),
            open_scopes: 0,
            arg_spans: BTreeMap::new(),
            rooted: BTreeSet::new(),
        }
    }

    /// The finished instructions, with every jump pointing at a real one.
    pub(super) fn finish(mut self) -> Finished {
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
        Finished {
            code: self.code,
            spans: self.spans,
            arg_spans: self.arg_spans,
            value_frame_size: self.value_frame_size,
            scalar_frame_size: self.scalar_frame_size,
            place_frame_size: self.place_frame_size,
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
        self.depth = Some(depth.after(stack_shape(&self.outer.constants, inst)));
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
            SlotKind::Value => (
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
        if param.variadic || !matches!(kind, SlotKind::Value) {
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
        let arrival = depth.after(stack_shape(&self.outer.constants, inst(0)));
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
    /// The value stack and the scalar stack are numbered separately, so
    /// `kind` picks which counter this draws from. A number is dense within
    /// its own stack — nothing to skip, because the other stack's numbers
    /// are not in this space at all.
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
    pub(super) fn allocate(&mut self, kind: SlotKind) -> u32 {
        match kind {
            SlotKind::Value => {
                let slot = self.next_value;
                self.next_value += 1;
                self.value_frame_size = self.value_frame_size.max(self.next_value);
                slot
            }
            SlotKind::Scalar(_) => {
                let slot = self.next_scalar;
                self.next_scalar += 1;
                self.scalar_frame_size = self.scalar_frame_size.max(self.next_scalar);
                slot
            }
            SlotKind::Place => {
                let slot = self.next_place;
                self.next_place += 1;
                self.place_frame_size = self.place_frame_size.max(self.next_place);
                slot
            }
        }
    }

    /// Lets `name` reach a slot [`Body::allocate`] already reserved.
    pub(super) fn declare_at(&mut self, name: Option<&'a str>, kind: SlotKind, slot: u32) {
        self.live.push(Binding {
            name,
            slot,
            capture: None,
            kind,
        });
    }

    /// Lets `name` reach the slot holding capture `index`.
    pub(super) fn declare_capture_at(&mut self, name: &'a str, index: u32, slot: u32) {
        self.live.push(Binding {
            name: Some(name),
            slot,
            capture: Some(index),
            kind: SlotKind::Value,
        });
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
    /// than recomputed from what remains live: a scope's declarations are on
    /// three independent stacks now, and the mark is what was true of all of
    /// them before any grew.
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
            SlotKind::Value | SlotKind::Place => None,
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

    /// Where a binding of `name` declared from something of `kind` actually
    /// lives.
    ///
    /// The one thing that overrides the checker's answer, and it overrides it
    /// in one direction only: a name a place is rooted at keeps its value
    /// slot even where the checker settled it as `Int`, because a place is
    /// an index into the value stack and there is nothing in the scalar
    /// stack for one to address. See [`Body::rooted`] for what the set is
    /// and why it over-approximates.
    pub(super) fn rooted_kind(&self, name: &str, kind: SlotKind) -> SlotKind {
        match kind.is_scalar() && self.rooted.contains(name) {
            true => SlotKind::Value,
            false => kind,
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
                    SlotKind::Value => self.emit(Inst::PlaceLocal(slot), expr.span),
                    SlotKind::Place => self.emit(Inst::LoadPlace(slot), expr.span),
                    // The pre-pass puts every name a `var` argument is
                    // rooted at on the value stack, so this is a root it did
                    // not see rather than one it declined. Refusing says so
                    // instead of addressing eight bytes of the wrong stack.
                    SlotKind::Scalar(_) => {
                        return Err(Unsupported::new(
                            format!("`{name}` as a place, which is a scalar slot"),
                            expr.span,
                        ))
                    }
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
    /// what every slot used to.
    pub(super) fn slot_kind(&self, expr: &Expr) -> SlotKind {
        match self.scalar_of(expr) {
            Some(what) => SlotKind::Scalar(what),
            None => SlotKind::Value,
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
        match self.field_position(receiver, name) {
            Some(index) => Inst::GetFieldAt(index),
            None => Inst::GetField(self.outer.name(name)),
        }
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
        let Some(Ty::Struct(named, _)) = self.settled(receiver) else {
            return None;
        };
        let decl = match named.split_once('.') {
            Some((module, type_name)) => self
                .outer
                .checked
                .modules
                .get(module)?
                .structs
                .get(type_name)?
                .decl
                .as_ref(),
            None => self.outer.struct_of(self.module, named)?.1,
        };
        let index = decl
            .fields
            .iter()
            .position(|field| field.name.node == name)?;
        Some(index as u32)
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
