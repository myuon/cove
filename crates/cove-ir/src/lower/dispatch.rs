//! Which declaration a method call reaches.
//!
//! The receiver decides, and the receiver's type is the one thing this pass
//! cannot work out for itself — so the order here is the order
//! `Interpreter::eval_method_call` asks in, read off what the checker
//! settled rather than off what a value turned out to be. A recorded target
//! first, because it makes every question about the name moot; then a trait
//! object, a bounded type parameter, and the rigid `Self` of a trait's
//! default body, which are three static types and one call; then a host
//! resource, a task, a builtin, and last of all a name.
//!
//! [`Body::call_dyn`] is the one call in the language whose target is not
//! knowable before the run, and [`Inst::CallDyn`] is an instruction of its
//! own so that it says so rather than hiding behind one that looks
//! static.

use std::sync::Arc;

use cove_diag::Span;
use cove_schema::builtins;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Expr, ExprId};

use crate::{Inst, Scalar, Unsupported};

use super::body::Body;
use super::call::{plain_arguments, Args};
use super::index::Key;

impl<'a, 'l> Body<'a, 'l> {
    /// The trait a call to `method` on a value of the type parameter
    /// `param` goes through, qualified, and nothing when no bound declares
    /// one.
    ///
    /// `Interpreter::eval_method_call` draws no distinction between a
    /// receiver whose static type is a trait object, a bounded type
    /// parameter, or the rigid `Self` of a trait's default body: it reads
    /// the concrete value's own type name and looks the method up from
    /// there. So this pass draws none either, and all it needs from the
    /// static type is which trait the call goes through.
    ///
    /// The first bound that declares the name is the one, which is the
    /// choice `cove_sema`'s `bound_method` makes over the same list in the
    /// same order.
    ///
    /// A parameter neither the declaration nor a trait default put in scope
    /// answers `None` — a type parameter of an `impl` block or of the struct
    /// it extends, and every parameter in scope around a lambda, which has
    /// no declaration of its own to have written one. The caller then falls
    /// through to the refusal it had, which is the honest answer: a receiver
    /// this pass cannot name a trait for is one it cannot collect the
    /// candidates of.
    fn bound_of(&self, param: &str, method: &str) -> Option<Arc<str>> {
        let written: Vec<&str> = match param {
            "Self" => self.self_bound.into_iter().collect(),
            _ => self
                .generics
                .iter()
                .find(|generic| generic.name.node == param)
                .map(|generic| generic.bounds.iter().map(|b| b.node.as_str()).collect())
                .unwrap_or_default(),
        };
        for bound in written {
            let qualified = self.outer.trait_named(self.module, bound);
            let declares = qualified.rsplit_once('.').is_some_and(|(module, short)| {
                self.outer
                    .checked
                    .modules
                    .get(module)
                    .and_then(|resolved| resolved.traits.get(short))
                    .is_some_and(|entry| entry.method(method).is_some())
            });
            if declares {
                return Some(qualified);
            }
        }
        None
    }

    /// `value.label()` where the receiver's static type says which trait the
    /// method comes from and nothing about which implementation: the one
    /// call in the language whose target is not knowable before the run.
    ///
    /// Three static types say that and are therefore one call here, as they
    /// are one call in `Interpreter::eval_method_call`: a `dyn Trait`, a
    /// type parameter bounded by the trait, and the rigid `Self` of that
    /// trait's own default body. What finds the implementation in each is
    /// the concrete value the receiver turns out to be — a run-time fact,
    /// and therefore a run-time lookup.
    /// [`Inst::CallDyn`] is that lookup. It is an instruction of its own
    /// rather than an [`Inst::Call`] with a target guessed at, which is what
    /// [issue #116](https://github.com/myuon/cove/issues/116) asks for: an
    /// operation whose target is not statically known says so, instead of
    /// hiding behind one that looks static.
    ///
    /// The arity is the *trait's*, not any one implementation's. A
    /// conformance's method must match the signature its trait declares —
    /// `cove_sema`'s `signature_difference` compares the receiver, the
    /// parameter names, the parameter types and the return type — so the
    /// trait's own declaration is the one thing every candidate agrees about
    /// and is what a call site can place its arguments by.
    fn call_dyn(
        &mut self,
        trait_name: &str,
        receiver: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        // `Ty::Dyn` carries the trait qualified by the module that declares
        // it, which is the same name `Interpreter::coerce` builds and the
        // same one `Dispatch` is keyed by.
        let declared = trait_name.rsplit_once('.').and_then(|(module, short)| {
            let resolved = self.outer.checked.modules.get(module)?;
            resolved.traits.get(short)?.method(name)
        });
        let Some(declared) = declared else {
            return Err(Unsupported::new(
                format!("a call to `{name}`, which `{trait_name}` does not declare here"),
                span,
            ));
        };
        // A call through a `dyn` supplies a count and nothing else, exactly
        // as a call through a value does: there is no supplied-set for a
        // specialisation to be keyed by, and no callee in reach for a label
        // to be matched against. Both shapes are refused rather than
        // rearranged.
        plain_arguments(args, name)?;
        if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
            return Err(Unsupported::new(
                format!("a labelled argument to `{name}`, which is called through a `dyn`"),
                arg.span,
            ));
        }
        if args.len() != declared.params.len() {
            return Err(Unsupported::new(
                format!(
                    "a call to `{name}` through `dyn {trait_name}` that supplies {} of its {} argument(s)",
                    args.len(),
                    declared.params.len()
                ),
                span,
            ));
        }
        if args.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{name}` with more than 65534 arguments"),
                span,
            ));
        }
        let site = self.outer.dispatch_site(trait_name, name);
        // The receiver first and then the arguments, left to right:
        // `Interpreter::eval_method_call` resolves the receiver before
        // `eval_args`, and the order two effects happen in is observable.
        self.expr(receiver)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        self.emit(
            Inst::CallDyn {
                site,
                argc: args.len() as u16 + 1,
            },
            span,
        );
        // On the value stack, because no candidate's own signature is one
        // this call could have read a convention off.
        Ok(None)
    }

    /// `receiver.name(...)`, where the receiver is a value.
    ///
    /// The interpreter tries a declared method of the receiver's *runtime*
    /// type first and falls back to the builtin table, so which of the two
    /// applies is a fact about the receiver — and the receiver's type is
    /// what the checker settled. Two answers follow from it, and the second
    /// is as much of the point as the first:
    ///
    /// - Where the checker recorded the declaration this call reaches, that
    ///   is the declaration, and nothing about the name is asked.
    /// - Where it settled the receiver's type and recorded no declaration,
    ///   this call reaches none: it is a builtin method, and a declared type
    ///   answering to the same name somewhere in the package is not what it
    ///   could have meant.
    ///
    /// Together those are why `impl Box { fn length(self) }` and
    /// `[1, 2, 3].length()` can now be written in one program. Both used to
    /// refuse — the first because a builtin shares the name, the second
    /// because a declared type does — and a name was all there was to tell
    /// them apart, which is not enough.
    ///
    /// A receiver the checker abstained about, or one it never walked, is
    /// still resolved by name and still refuses what a name cannot settle.
    /// Guessing there is the one mistake a second backend must not make:
    /// `[1, 2, 3].length()` is the builtin's `3` on the oracle, and a `Call`
    /// to a declared `Box.length` is a different program.
    pub(super) fn method_call(
        &mut self,
        id: ExprId,
        receiver: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        // Before anything is asked about the name, because a recorded target
        // makes every one of those questions moot: which types declare this
        // name, whether a builtin shares it, and whether the builtin that
        // shares it writes through its receiver are all questions about
        // *which* declaration is meant, and the checker has answered that.
        let recorded = self.target(id, span);
        if let Some(key) = recorded.and_then(|target| self.declared_by(target)) {
            return self.call_declared(key, Some(receiver), args, span);
        }
        // A `dyn Trait` receiver dispatches from the value it carries rather
        // than from its static type, which is the whole of what makes the
        // dispatch dynamic. Asked before every question below for the reason
        // `Interpreter::eval_method_call` unwraps its receiver before it asks
        // any of its own: none of them is a question about a trait object,
        // and the checker records no target for one — a call through a trait
        // reaches a declaration the call site cannot name.
        if let Some(Ty::Dyn(trait_name)) = self.settled(receiver) {
            // `Facts::ty` holds the type as the checker held it while it
            // walked this body, and there a trait the module declares is
            // named bare while an imported one already carries the module it
            // came from — `cove_sema`'s `qualified_name`, which is what
            // `Signature` publishes after applying it and what a `dyn` value
            // carries. `trait_named` applies the same rule from the same
            // tables, and leaves a name that already carries a module alone.
            let qualified = self.outer.trait_named(self.module, trait_name);
            return self.call_dyn(&qualified, receiver, name, args, span);
        }
        // A bounded type parameter is the same call. The checker resolves
        // the *signature* through the bound and the run resolves the
        // *implementation* through the value, exactly as it does for a
        // trait object, and `Interpreter::eval_method_call` runs one code
        // path for both. `Self` inside a trait's default body is the same
        // again, which is why a dispatch through a `dyn` to a method the
        // conformance did not write reaches this.
        if let Some(Ty::Param(param)) = self.settled(receiver) {
            if let Some(qualified) = self.bound_of(param, name) {
                return self.call_dyn(&qualified, receiver, name, args, span);
            }
        }
        // A resource handle's methods belong to the host that issued it, so
        // they are dispatched through the boundary rather than looked up in
        // the package — the rule `Interpreter::call_builtin_method` states
        // and dispatches by, asked here of what the checker settled instead
        // of what a value turned out to be. It is asked before any of the
        // questions below for the reason the interpreter asks it before its
        // own: none of them is about a handle, and a name a host answers is
        // not a name this package or the builtins have to share.
        if self.resource_op(receiver, name) {
            plain_arguments(args, name)?;
            // The receiver first and then the arguments, left to right:
            // `Interpreter::eval_method_call` evaluates the receiver before
            // `eval_args`, and the order two effects happen in is observable.
            self.expr(receiver)?;
            for arg in args.iter() {
                self.expr(arg.value)?;
            }
            let op = self.outer.name(name);
            self.emit(
                Inst::CallResource {
                    op,
                    argc: args.len() as u32,
                },
                span,
            );
            // On the value stack, whatever the schema says the operation
            // answers, exactly as `Inst::CallHost` leaves a host call's
            // answer.
            return Ok(None);
        }
        // The operations of a task scope and of a task handle, dispatched by
        // the type the checker settled for the receiver where
        // `Interpreter::call_task_method` dispatches by the value's own kind.
        // Asked before the builtins for the reason the interpreter asks them
        // before its own: a scope and a handle are runtime values that no
        // declaration and no builtin answers for, and `spawn`, `await` and
        // `cancel` are not names a builtin shares.
        match self.settled(receiver) {
            Some(Ty::Scope) if name == "spawn" => return self.spawn(receiver, args, span),
            Some(Ty::Task(_)) if name == "await" => {
                return self.task_op(receiver, Inst::Await, "await", args, span)
            }
            Some(Ty::Task(_)) if name == "cancel" => {
                return self.task_op(receiver, Inst::Cancel, "cancel", args, span)
            }
            Some(Ty::Shared(_)) if name == "lock" => return self.lock(receiver, args, span),
            _ => {}
        }
        if name == "await" {
            return Err(Unsupported::new("an `await`", span));
        }
        if name == "snapshot" {
            // A struct or an enum with an `impl Snapshot for Type` never
            // reaches here: the checker recorded which declaration that call
            // means, and the recorded target above took it. What is left is
            // the half of the trait no conformance answers for, and it is
            // emitted only where the checker settled a type that cannot
            // reach one — see `snapshot_without_a_conformance`, which is
            // where the receiver decides and not the name.
            let Some(ty) = self.settled(receiver) else {
                return Err(Unsupported::new(
                    "`snapshot` on a receiver whose type nothing settled",
                    span,
                ));
            };
            if !snapshot_without_a_conformance(ty) {
                return Err(Unsupported::new(
                    format!("`snapshot` on a `{}`, which a conformance answers", ty),
                    span,
                ));
            }
            if !args.is_empty() {
                // `snapshot` takes none, and `Interpreter::eval_method_call`
                // says so before it reads the receiver; refusing keeps the
                // instruction's shape a fact rather than something a call
                // site could vary.
                return Err(Unsupported::new("`snapshot` given arguments", span));
            }
            self.expr(receiver)?;
            self.emit(Inst::Snapshot, span);
            return Ok(None);
        }
        // `freeze` is the one builtin that needs the place rather than a read
        // of it. `builtins::freeze` counts the handles to the storage and
        // refuses when the count is not one, and a read of the receiver would
        // be the second handle — which is why
        // `Interpreter::call_builtin_method` runs it inside `place.with_mut`
        // and why `Inst::Freeze` takes a place.
        //
        // A receiver that is not a place at all falls through to the ordinary
        // builtin lowering below, exactly as it does in the interpreter:
        // `Vector.of(1).freeze()` has no place, and `builtins::call_method`'s
        // own `freeze` arm answers it from the temporary — which holds the
        // only handle there is. `push` falls through as well, whatever its
        // receiver: it mutates through the handle a `Vector` is, so there is
        // nothing to write back to the receiver's slot. That the receiver of
        // a mutating method is a place, and a writable one, is `cove-sema`'s
        // to say and it has said it (ADR 0021).
        if name == "freeze" && self.is_a_place(receiver) {
            if !args.is_empty() {
                // `freeze()` takes none, and the checker says so before this
                // does; refusing keeps the instruction's shape a fact rather
                // than something a call site could vary.
                return Err(Unsupported::new("`freeze` given arguments", span));
            }
            self.place(receiver)?;
            self.emit(Inst::Freeze, span);
            return Ok(None);
        }
        // Which types declare a method of this name is a question for the
        // shared table rather than for a list written here, so a builtin
        // that gains a method gains this refusal with it.
        let builtin_method = builtins::builtins()
            .iter()
            .any(|schema| schema.method(name).is_some());
        // Only the methods this module could be handed a receiver for, and
        // only where a name is still all there is to go on. A receiver whose
        // type the checker settled and recorded no target for has already
        // been decided about — the target above would have named a
        // declaration if the call reached one — so there is no candidate
        // here and a name two types share stops being ambiguous.
        //
        // Three cases are not that. `Unknown` is the checker saying it did
        // not prove this and `Never` is a receiver that produces no value,
        // so neither settles which method a call reaches; a receiver the
        // checker never walked settles nothing either. And a target it
        // *did* record that this pass could not find a declaration for is
        // an answer nobody here can act on, which leaves the name.
        let by_name_is_all_there_is = recorded.is_some()
            || matches!(
                self.settled(receiver),
                None | Some(Ty::Unknown(_)) | Some(Ty::Never)
            );
        let candidates: Vec<Key> = if by_name_is_all_there_is {
            self.outer
                .by_name
                .get(name)
                .map(|all| {
                    all.iter()
                        .copied()
                        .filter(|key| self.outer.could_dispatch(self.module, *key))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !candidates.is_empty() {
            if candidates.len() > 1 {
                return Err(Unsupported::new(
                    format!("a call to `{name}`, which more than one type declares"),
                    span,
                ));
            }
            if builtin_method {
                return Err(Unsupported::new(
                    format!(
                        "a call to `{name}`, which a builtin type and a declared type both have"
                    ),
                    span,
                ));
            }
            let key = candidates[0];
            return self.call_declared(key, Some(receiver), args, span);
        }
        if builtin_method {
            plain_arguments(args, name)?;
            self.expr(receiver)?;
            for arg in args.iter() {
                self.expr(arg.value)?;
            }
            let name = self.outer.name(name);
            self.emit(
                Inst::CallBuiltin {
                    name,
                    argc: args.len() as u32,
                },
                span,
            );
            // A builtin method answers on the value stack, whatever its type:
            // `call_method` is the interpreter's and hands back a `Value`.
            return Ok(None);
        }
        Err(Unsupported::new(
            format!("a call to `{name}`, which no declared type and no builtin has"),
            span,
        ))
    }
}

/// Whether `Interpreter::snapshot` answers about a value of this type
/// without reaching a declared conformance.
///
/// The interpreter dispatches a `Value::Struct`, a `Value::Enum` and a
/// `Value::Dyn` to an `impl Snapshot for Type`, and answers everything else
/// itself. An instruction cannot run a whole Cove function in the middle of
/// itself, so the VM covers the second half and this is the question that
/// decides which half a receiver is in.
///
/// An `Array`, a `Map` and a `Set` are cloned rather than walked — each is
/// immutable, so there is nothing inside one for a copy to separate — and
/// that is why their element types are not asked about. A `Vector` *is*
/// walked, one element at a time, so a `Vector<T>` is only in this half when
/// `T` is: a `Vector<Booking>` dispatches once per element and is refused.
///
/// An abstention is not an answer, and neither is [`Ty::Unknown`]. Both are
/// refused by the caller, which asks for a settled type before asking this.
fn snapshot_without_a_conformance(ty: &Ty) -> bool {
    match ty {
        Ty::Unit
        | Ty::Bool
        | Ty::Int
        | Ty::Float
        | Ty::Str
        | Ty::Duration
        | Ty::Range
        | Ty::Array(_)
        | Ty::Map(_, _)
        | Ty::Set(_) => true,
        Ty::Vector(inner) => snapshot_without_a_conformance(inner),
        _ => false,
    }
}
