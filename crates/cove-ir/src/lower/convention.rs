//! Where a lowered function keeps what it is given, and where it leaves its
//! answer.
//!
//! This is the callee's half of the calling convention: which stack each
//! parameter arrives on, which stack the answer travels on, and what a
//! specialisation's prologue computes for the parameters no call site
//! supplied. [`Function::params`] and [`Function::returns`] are that
//! convention written down.
//!
//! The caller's half is `super::call`, and the two agree by construction
//! rather than by agreement: both read the same [`Signature`] through the
//! same [`slot_kind_of`], which is one rule and not two that could drift
//! apart. `super::validate` is where the pair is made to say so out loud.
//!
//! [`scalar_of_ty`] is that rule. A binding's slot, an operand's stack, a
//! parameter's slot and where a call leaves its answer are four questions
//! with one answer, so they ask one function — and an abstention is not a
//! settled type in any of the four.
//!
//! [`Signature`]: cove_sema::Signature

use std::sync::Arc;

use cove_sema::typeck::Ty;
use cove_syntax::ast::Param;

use crate::{Function, Inst, Scalar, SlotKind, Unsupported};

use super::fuel::block_fuel;
use super::index::{reject_dyn, Key, Lowering};
use super::scan::var_argument_roots;
use super::{Body, Position};

impl<'a> Lowering<'a> {
    /// Lowers a lambda: its parameters, then the captures the body that
    /// wrote it handed over, then its own body.
    ///
    /// Everything about the convention is fixed rather than read off a
    /// signature. Every parameter is a value slot and the answer comes back
    /// on the value stack, because [`Inst::CallValue`] is emitted where
    /// nothing knows which function it will reach — see that instruction.
    /// The captures then take the value slots straight after the value
    /// parameters, which is exactly where the call puts them, and that
    /// arrangement is only possible *because* the parameters are all in one
    /// stack: a scalar parameter would leave a hole between the two.
    ///
    /// One lambda is not called through a value, and it is the exception the
    /// whole of [`Function::capture_base`] exists for. The closure a `lock`
    /// is given may write its first parameter `var`, which means it names the
    /// cell's contents rather than receiving a copy of them — so that
    /// parameter is a place, [`Inst::Lock`] hands one over, and the captures
    /// begin one value slot earlier than `arity` would say.
    ///
    /// The captures are declared before the parameters although they are
    /// numbered after them, and both halves of that matter.
    /// `Env::declare_capture` puts a capture in a list searched *after* this
    /// call's own bindings, so a parameter shadows a capture of the same
    /// name; and `Env::captures` walks that list *before* the frame, so a
    /// nested lambda's own captures come out in the same order. One `live`
    /// list in this order answers both.
    pub(super) fn lambda_function(&mut self, index: usize) -> Result<Function, Unsupported> {
        let site = &self.lambdas[index];
        let module = site.module;
        let span = site.span;
        let decl_params = site.params;
        let decl_body = site.body;
        let captures: Vec<Arc<str>> = site.captures.iter().map(|name| Arc::from(*name)).collect();
        let capture_names: Vec<&'a str> = site.captures.clone();
        let aliases = site.aliases_first_param;
        let is_async = site.is_async;

        let mut body = Body::new(self, module);
        body.returns = SlotKind::Value;
        body.rooted = var_argument_roots(decl_body);

        let mut params: Vec<SlotKind> = Vec::with_capacity(decl_params.len());
        let mut slots: Vec<u32> = Vec::with_capacity(decl_params.len());
        for (at, param) in decl_params.iter().enumerate() {
            reject_parameter(param, at + 1 == decl_params.len())?;
            if param.is_var {
                // A `var` parameter names the caller's storage, and a call
                // through a value has no way to hand one over: every
                // argument of `Inst::CallValue` travels on the value stack.
                // `shared.lock(fn(var value) { ... })` is the one place one
                // is written, and `Inst::Lock` is the one call that does not
                // go through `Inst::CallValue` — so that lambda, and only
                // that lambda, takes its first parameter as a place.
                if !(aliases && at == 0) {
                    return Err(Unsupported::new(
                        format!("a closure's `var` parameter `{}`", param.name.node),
                        param.span,
                    ));
                }
                params.push(SlotKind::Place);
                slots.push(body.allocate(SlotKind::Place));
                continue;
            }
            if param.default.is_some() {
                // `bind_params` would evaluate it in the callee, exactly as
                // it does for a declared function — but a call through a
                // value supplies a count and nothing else, so there is no
                // supplied-set for a specialisation to be keyed by. Nothing
                // writes one; refusing says so.
                return Err(Unsupported::new(
                    format!("a closure's default for `{}`", param.name.node),
                    param.span,
                ));
            }
            params.push(SlotKind::Value);
            slots.push(body.allocate(SlotKind::Value));
        }
        // Numbered after the parameters, because that is where the call
        // copies them out of the closure.
        let capture_slots: Vec<u32> = capture_names
            .iter()
            .map(|_| body.allocate(SlotKind::Value))
            .collect();
        for (index, name) in capture_names.iter().enumerate() {
            body.declare_capture_at(name, index as u32, capture_slots[index]);
        }
        for (at, param) in decl_params.iter().enumerate() {
            body.declare_at(Some(param.name.node.as_str()), params[at], slots[at]);
            // A lambda's parameters are bound by the same `bind_params`, so
            // one written `dyn Trait` receives a trait object exactly as a
            // declaration's does. A lambda has no written return type, so
            // there is no second conversion here: `Interpreter::invoke`
            // reads one off `Closure::decl`, and a lambda's is `None`.
            body.coerce_param(module, param, params[at], slots[at], true);
        }

        body.block_at(decl_body, Position::Value)?;
        body.emit_final_return(decl_body.span);
        let finished = body.finish();
        let capture_base = value_params(&params);

        Ok(Function {
            module: module.into(),
            // Stable, and unique within the program, because the index is
            // the order lambdas were reached in and that order is the
            // worklist's. A listing reads it, and nothing else does.
            name: format!("<closure {index}>").into(),
            value_frame_size: finished.value_frame_size,
            scalar_frame_size: finished.scalar_frame_size,
            place_frame_size: finished.place_frame_size,
            arity: params.len() as u32,
            params,
            returns: SlotKind::Value,
            has_receiver: false,
            // An `async` lambda answers a settled task exactly as an `async
            // fn` does, and for the same reason: `Interpreter::invoke` reads
            // `is_async` off the closure it was handed and wraps what the
            // body produced.
            answers_a_task: is_async,
            captures,
            capture_base,
            param_names: param_names(decl_params),
            block_fuel: block_fuel(&finished.code),
            code: finished.code,
            spans: finished.spans,
            arg_spans: finished.arg_spans,
            span,
        })
    }

    /// Lowers one declared function into its instructions.
    pub(super) fn declared_function(
        &mut self,
        key: Key,
        supplied: &[bool],
        as_value: bool,
    ) -> Result<Function, Unsupported> {
        let declared = self.declaration(key);
        let module = declared.module;
        let name: Arc<str> = declared.name.as_str().into();
        let from_trait_default = declared.from_trait_default;
        let decl = declared.decl;

        if let Some(ty) = &decl.return_type {
            reject_dyn(ty, "a `dyn` return type")?;
        }

        // The convention this function is called under, read from what the
        // checker resolved for this declaration rather than derived from its
        // annotations again — the rule the whole pass follows.
        //
        // A declaration the checker recorded nothing for is not a checked
        // program, and the lowering does not guess about one: every
        // parameter and the answer keep the representation every slot had
        // before it, which is the same thing an abstention about a binding
        // gets.
        let signature = self.signature(key);
        let returns = match as_value || decl.is_async {
            // A closure answers on the value stack whatever the declaration
            // says, because `Inst::CallValue` reads exactly that one and has
            // no callee to have asked. An `async fn` answers there too,
            // whatever it declared, because what a call to one answers is a
            // task and a task is a value: `async fn f() -> Int` hands back a
            // `Task<Int>`, and only `await` produces the `Int`.
            true => SlotKind::Value,
            false => signature.map_or(SlotKind::Value, |signature| slot_kind_of(&signature.ret)),
        };
        if as_value {
            // The three shapes a closure has no way to express, and each is
            // refused rather than approximated. A `var` parameter names the
            // caller's storage, and every argument of a call through a value
            // travels on the value stack; a variadic parameter collects
            // leftovers, and the call supplies a count with nothing to say
            // which of them were leftovers; and a default is used by a call
            // that omits an argument, which is what numbers a specialisation
            // — but a call through a value supplies `arity` arguments and
            // there is no supplied-set for one to be keyed by.
            if let Some(param) = decl.params.iter().find(|param| param.is_var) {
                return Err(Unsupported::new(
                    format!(
                        "`{}` used as a value, whose parameter `{}` is `var`",
                        declared.name, param.name.node
                    ),
                    param.span,
                ));
            }
            if let Some(param) = decl.params.iter().find(|param| param.variadic) {
                return Err(Unsupported::new(
                    format!(
                        "`{}` used as a value, whose parameter `{}` is variadic",
                        declared.name, param.name.node
                    ),
                    param.span,
                ));
            }
            if let Some(param) = decl.params.iter().find(|param| param.default.is_some()) {
                return Err(Unsupported::new(
                    format!(
                        "`{}` used as a value, whose parameter `{}` has a default",
                        declared.name, param.name.node
                    ),
                    param.span,
                ));
            }
        }

        // In the order a call supplies them, which is what makes an argument
        // become a slot without moving: the receiver first, then the
        // parameters as declared.
        let mut params: Vec<SlotKind> = Vec::new();
        // Read before the body borrows the lowering, because a name has to
        // be interned to carry it and interning is the lowering's.
        let dyn_return = match (returns, &decl.return_type) {
            (SlotKind::Value, Some(ty)) => match self.dyn_conversion(module, ty) {
                Some((trait_name, depth)) => {
                    let trait_name = self.name(&trait_name);
                    Some(Inst::MakeDyn { trait_name, depth })
                }
                None => None,
            },
            _ => None,
        };
        let mut body = Body::new(self, module);
        body.returns = returns;
        body.dyn_return = dyn_return;
        body.generics = &decl.generics;
        body.self_bound = from_trait_default;
        body.rooted = var_argument_roots(&decl.body);
        if let Some(receiver) = decl.receiver {
            // `var self` is a place slot and nothing else is. Which stack an
            // ordinary receiver lives in is derived rather than assumed — a
            // receiver is a value in every program that can be written
            // today, because a method is declared on a struct or an enum,
            // but that is the signature's answer and not this pass's guess.
            //
            // An ordinary receiver is read-only in the body and a `var self`
            // one is not, which is the same `writable` a `let` and a `var`
            // binding get and is what a write through it is checked against.
            let kind = if receiver.is_var {
                SlotKind::Place
            } else if as_value {
                // Under the value-stack convention a receiver is an argument
                // like any other, and the caller has no callee to have read
                // the signature's answer off. Nothing a method is declared
                // on today is scalar — a trait is implemented for a struct
                // or an enum — so this states the convention rather than
                // changing where a receiver goes.
                SlotKind::Value
            } else {
                signature
                    .and_then(|signature| signature.receiver.as_ref())
                    .map_or(SlotKind::Value, slot_kind_of)
            };
            params.push(kind);
            body.declare(Some("self"), kind);
        }
        let mut kinds: Vec<SlotKind> = Vec::with_capacity(decl.params.len());
        for (at, param) in decl.params.iter().enumerate() {
            reject_parameter(param, at + 1 == decl.params.len())?;
            // A variadic parameter is one ordinary value slot holding the
            // `Array<T>` the call site collected, which is what
            // `bind_params` declares one as — `env.declare(name,
            // Place::binding(Value::Array(items.into()), false))`, immutable
            // and holding an array. It is not asked of the signature,
            // because `record_signature` deliberately stores what the
            // parameter was *written* as rather than the array the body
            // sees: `items: Int...` would answer `Int` there, and a scalar
            // slot is exactly what this must not be.
            kinds.push(if param.is_var && supplied[at] {
                // A `var` parameter does not have a type's slot at all: it
                // names the caller's storage, and what type that storage
                // holds says nothing about where the *name* lives. Left to
                // its default it is not one: `bind_params` reaches
                // `Place::binding` there like any other default, and
                // `Body::call_declared` refuses a call that leaves a `var`
                // parameter out rather than lowering a place that names
                // nothing.
                SlotKind::Place
            } else if param.variadic || as_value {
                SlotKind::Value
            } else {
                signature
                    .and_then(|signature| signature.params.get(at))
                    .map_or(SlotKind::Value, slot_kind_of)
            });
        }

        // The supplied parameters take the first slot numbers of whichever
        // stack each lives in, because that is what the calling convention
        // means: an argument is pushed onto its own stack and *becomes* the
        // callee's slot there without moving. A parameter left to its
        // default is not pushed by anyone, so it is numbered after all of
        // them and the convention does not notice it exists.
        let mut slots: Vec<u32> = vec![0; decl.params.len()];
        for (at, kind) in kinds.iter().enumerate() {
            if supplied[at] {
                params.push(*kind);
                slots[at] = body.allocate(*kind);
            }
        }
        for (at, kind) in kinds.iter().enumerate() {
            if !supplied[at] {
                slots[at] = body.allocate(*kind);
            }
        }

        // Now the names, in declaration order, with each default evaluated
        // when its own parameter's turn comes. That order is the whole of
        // what makes this the interpreter's semantics rather than an
        // approximation of it: `bind_params` walks the parameters in order
        // and declares each into an environment holding the ones before it,
        // so a default may read an earlier parameter and cannot read a later
        // one. Naming a parameter only when its turn comes is how a default
        // that reads a later one refuses here instead of quietly reading a
        // slot nothing has written.
        for (at, param) in decl.params.iter().enumerate() {
            if !supplied[at] {
                let default = param.default.as_ref().unwrap_or_else(|| {
                    unreachable!("a parameter left unsupplied was reached through its default")
                });
                match kinds[at] {
                    SlotKind::Scalar(_) => body.expr_scalar(default)?,
                    SlotKind::Value => body.expr(default)?,
                    SlotKind::Place => unreachable!("a default does not produce a place"),
                }
                body.coerce_param(module, param, kinds[at], slots[at], false);
                body.emit(store_slot(kinds[at], slots[at]), default.span);
            } else {
                body.coerce_param(module, param, kinds[at], slots[at], true);
            }
            body.declare_at(Some(param.name.node.as_str()), kinds[at], slots[at]);
        }

        // The body's value is the function's answer, so it is lowered into
        // the stack the answer travels on rather than into the value stack
        // and moved across afterwards.
        body.block_at(&decl.body, position_of(returns))?;
        body.emit_final_return(decl.body.span);
        let finished = body.finish();
        let capture_base = value_params(&params);

        Ok(Function {
            module: module.into(),
            name,
            value_frame_size: finished.value_frame_size,
            scalar_frame_size: finished.scalar_frame_size,
            place_frame_size: finished.place_frame_size,
            arity: params.len() as u32,
            params,
            returns,
            has_receiver: decl.receiver.is_some(),
            answers_a_task: decl.is_async,
            // A declared function used as a value is a closure over nothing:
            // `Interpreter::eval_ident` builds one with `captures:
            // Vec::new()`, because a declaration reads no environment.
            captures: Vec::new(),
            capture_base,
            // Only a function that can become a closure value is ever called
            // with a count of the caller's choosing, so only that one can
            // reach the diagnostic these names are for.
            param_names: match as_value {
                true => param_names(&decl.params),
                false => Vec::new(),
            },
            block_fuel: block_fuel(&finished.code),
            code: finished.code,
            spans: finished.spans,
            arg_spans: finished.arg_spans,
            span: decl.span,
        })
    }
}

/// How many of a function's parameters arrive on the value stack, which is
/// where its captures begin.
///
/// The same as `params.len()` for every closure but the one `Shared::lock` is
/// given a `var` parameter; see [`Function::capture_base`].
fn value_params(params: &[SlotKind]) -> u32 {
    params
        .iter()
        .filter(|kind| matches!(kind, SlotKind::Value))
        .count() as u32
}

/// What a scalar stack would hold a value of this type as, or `None` for a
/// type it cannot hold.
///
/// The one rule, and the only one. A binding's slot, an operand's stack, a
/// parameter's slot, and where a call leaves its answer are four questions
/// with one answer, so they ask one function: two rules that could disagree
/// about what the scalar stack holds is exactly the drift reading the
/// checker's answers is supposed to make impossible.
///
/// `Ty::Unknown` is the checker saying it did not prove this and is not a
/// settled type, so it answers `None` like everything else the stack has no
/// word for.
pub(super) fn scalar_of_ty(ty: &Ty) -> Option<Scalar> {
    match ty {
        Ty::Int => Some(Scalar::Int),
        Ty::Bool => Some(Scalar::Bool),
        _ => None,
    }
}

/// Where a slot of this type lives, which is [`scalar_of_ty`] read as a
/// place rather than as a representation.
pub(super) fn slot_kind_of(ty: &Ty) -> SlotKind {
    match scalar_of_ty(ty) {
        Some(what) => SlotKind::Scalar(what),
        None => SlotKind::Value,
    }
}

/// The position an expression is lowered in to leave its value where a slot
/// of this kind wants it.
fn position_of(kind: SlotKind) -> Position {
    match kind {
        SlotKind::Value => Position::Value,
        SlotKind::Scalar(_) => Position::Scalar,
        // Only a function's `returns` is asked this, and `slot_kind_of`
        // never answers `Place` about a return type: a place is what a
        // parameter can be, not what a value can be.
        SlotKind::Place => unreachable!("no expression is written in place position"),
    }
}

/// The instruction that writes a slot, which is decided by where the slot is.
pub(super) fn store_slot(kind: SlotKind, slot: u32) -> Inst {
    match kind {
        SlotKind::Value => Inst::StoreLocal(slot),
        SlotKind::Scalar(_) => Inst::StoreScalar(slot),
        // A place slot is filled by the calling convention and never
        // written: a `var` parameter is the one thing that has one, and
        // assigning to a `var` parameter writes through the place rather
        // than replacing it. Every caller of this has already sent a place
        // binding down `Body::assign_through_place`.
        SlotKind::Place => unreachable!("a place slot is never stored into"),
    }
}

/// The names `params` were written with, which is all of a written parameter
/// list a lowered [`Function`] keeps.
///
/// [`Function::param_names`] says what became of the rest. They are copied
/// into the program's own string type rather than borrowed, for the reason
/// every other name here is: a [`Program`] outlives the syntax it was lowered
/// from, and every thread of a run reads it.
///
/// [`Program`]: crate::Program
fn param_names(params: &[Param]) -> Vec<Arc<str>> {
    params
        .iter()
        .map(|param| Arc::from(param.name.node.as_str()))
        .collect()
}

/// Refuses a parameter the IR has no shape for.
///
/// A variadic parameter has one, and it is an ordinary value slot holding
/// the `Array<T>` the call site collected — see [`Body::call_declared`]. The
/// two shapes it can be written in that nothing has decided a meaning for
/// are refused here instead.
///
/// **Not the last parameter.** `Interpreter::assign_labels` gathers the
/// left-over arguments into `rest` only when the *last* parameter is
/// variadic, while `bind_params` wraps *any* variadic parameter's one slot
/// in an `Array`. So a variadic parameter written anywhere else is an array
/// of at most one element, which is a shape nobody meant and which the
/// parser and the checker both let through. Refusing says so rather than
/// picking one of the two readings.
///
/// **Written with a default.** `bind_params` tests `param.variadic` before
/// it looks at `param.default` and then `continue`s, and the checker's
/// `match_arguments` does the same, so a default on a variadic parameter is
/// dead code that neither of them can ever reach. `parse_param` accepts
/// `items: T... = x` all the same. Lowering it would mean lowering a
/// construct whose meaning is an accident of the order two `if`s are
/// written in.
pub(super) fn reject_parameter(param: &Param, is_last: bool) -> Result<(), Unsupported> {
    if param.is_var && param.variadic {
        // A variadic parameter is bound to an `Array` the call site
        // collected, which is storage the caller never named, so there is
        // nothing for a `var` to alias. `bind_params` binds one immutably
        // and never reads `is_var` for it, so the two markings written
        // together mean nothing rather than something this declines.
        return Err(Unsupported::new("a `var` variadic parameter", param.span));
    }
    if param.variadic {
        if !is_last {
            return Err(Unsupported::new(
                "a variadic parameter that is not the last one",
                param.span,
            ));
        }
        if param.default.is_some() {
            return Err(Unsupported::new(
                "a variadic parameter written with a default",
                param.span,
            ));
        }
    }
    if let Some(ty) = &param.ty {
        reject_dyn(ty, "a `dyn` parameter")?;
    }
    Ok(())
}
