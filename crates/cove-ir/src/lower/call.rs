//! A call at the call site: where each argument goes, and which function a
//! name reaches.
//!
//! This is the caller's half of the calling convention. Each argument is
//! lowered into the stack its own parameter's slot kind names and nothing
//! is moved afterwards, which is only sound because the arguments already
//! stand in declaration order — [`arguments_in_order`] is that invariant,
//! stated here because here is where it is relied on. `cove-sema` holds a
//! program to the same rule as a *language* rule (ADR 0021); this states it
//! as a property of a convention, which is why there are two statements of
//! it and not one.
//!
//! The callee's half is `super::convention`, and both halves read the same
//! [`Signature`] through the same `slot_kind_of`. `super::validate` is
//! where a call and its callee are made to agree out loud.
//!
//! Everything a call can lower to other than [`Inst::Call`] answers on the
//! value stack, because everything else is the interpreter's own code and
//! the interpreter speaks `Value`.
//!
//! [`Signature`]: cove_sema::Signature

use cove_diag::Span;
use cove_schema::builtins;
use cove_schema::hosts;
use cove_schema::TypeSchema;
use cove_syntax::ast::{Arg, Expr, ExprId, ExprKind, StructDecl};

use crate::{Inst, Scalar, SlotKind, Unsupported};

use super::body::Body;
use super::convention::{reject_parameter, scalar_of_ty, slot_kind_of};
use super::index::{reject_dyn, Instance, Key};

impl<'a, 'l> Body<'a, 'l> {
    /// Lowers a call, answering where it left its result.
    ///
    /// `Some` means the scalar stack, which is what a call to a function
    /// whose return type the checker settled as `Int` or `Bool` leaves it
    /// on; `None` means the value stack, which is what every other call
    /// leaves it on. The answer is threaded up rather than asked about
    /// afterwards because only the path that resolved the callee knows it —
    /// a builtin, a host operation, a constructor, and a declared function
    /// are four different answers reached through four different lookups.
    pub(super) fn call(
        &mut self,
        id: ExprId,
        callee: &'a Expr,
        written: &'a [Arg],
        trailing: Option<&'a Expr>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        let args = Args::new(written, trailing);
        match &callee.kind {
            ExprKind::Ident(name) => self.call_named(name, args, span),
            ExprKind::Field { base, name } => self.call_qualified(id, base, &name.node, args, span),
            _ => Err(Unsupported::new("a call through a value", callee.span)),
        }
    }

    /// `f(...)`, where `f` is a bare name.
    ///
    /// The order is the interpreter's: a local first — which is what makes a
    /// binding shadow a declaration — then a declared function, a struct
    /// initializer, an imported host operation, and a free builtin.
    fn call_named(
        &mut self,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        if self.lookup(name).is_some() {
            return self.call_through_value(name, args, span);
        }
        if let Some(key) = self.outer.function_of(self.module, name) {
            return self.call_declared(key, None, args, span);
        }
        if let Some((owner, decl)) = self.outer.struct_of(self.module, name) {
            return on_the_value_stack(self.make_struct(owner, decl, args, span));
        }
        if self.outer.declares_enum(self.module, name) {
            return Err(Unsupported::new(
                format!("`{name}`, which names an enum"),
                span,
            ));
        }
        if let Some(module) = self.outer.host_item(self.module, name) {
            return on_the_value_stack(self.call_host(module, name, args, span));
        }
        if name == builtins::MAP_ENTRY.name {
            return on_the_value_stack(self.make_map_entry(args, span));
        }
        if let Some(schema) = builtins::free_builtin(name) {
            return on_the_value_stack(self.make_builtin(schema.name, args, span));
        }
        Err(Unsupported::new(
            format!("a call to `{name}`, which the lowering cannot resolve"),
            span,
        ))
    }

    /// `f(...)`, where `f` is a local holding a callable value.
    ///
    /// The arguments go on the value stack left to right and the callee on
    /// top of them, which is the one place an operand is not in source
    /// order — see [`Inst::CallValue`] for why that is unobservable and what
    /// it buys.
    ///
    /// Nothing here knows what `f` holds, so nothing here can put an
    /// argument anywhere but the value stack, and nothing can put a label on
    /// one either: `bind_params` matches a label against the callee's own
    /// parameter names, which are a run-time fact about the closure. A
    /// labelled argument is therefore refused rather than lowered as a
    /// positional one, which is the direction a second backend is allowed to
    /// be wrong in.
    fn call_through_value(
        &mut self,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        plain_arguments(args, name)?;
        if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
            return Err(Unsupported::new(
                format!("a labelled argument to `{name}`, which is called through a value"),
                arg.span,
            ));
        }
        if args.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{name}` with more than 65534 arguments"),
                span,
            ));
        }
        let argc = args.len() as u16;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        self.ident(name, span)?;
        self.emit(Inst::CallValue { argc }, span);
        // On the value stack, because a call through a value has no callee
        // to read a convention off.
        Ok(None)
    }

    /// `head.name(...)`, where `head` may be a host module, an enum, a
    /// struct, or a module imported whole — and is a receiver when it is
    /// none of those.
    fn call_qualified(
        &mut self,
        id: ExprId,
        base: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if self.outer.is_host_module(self.module, head) {
                    return on_the_value_stack(self.call_host(head, name, args, span));
                }
                if let Some((owner, decl)) = self.outer.enum_of(self.module, head) {
                    // A case wins over an associated function of the same
                    // name, so naming a case never changes meaning when an
                    // `impl` block is added — which is the order
                    // `Interpreter::eval_call` asks in.
                    let is_case = decl.cases.iter().any(|case| case.name.node == name);
                    if !is_case {
                        if let Some(key) = self.outer.method_of(owner, head, name) {
                            return self.call_declared(key, None, args, span);
                        }
                    }
                    return on_the_value_stack(self.make_enum(owner, head, name, args, span));
                }
                if let Some((owner, _)) = self.outer.struct_of(self.module, head) {
                    if let Some(key) = self.outer.method_of(owner, head, name) {
                        return self.call_declared(key, None, args, span);
                    }
                }
                if let Some(owner) = self.outer.imported_module(self.module, head) {
                    if let Some(key) = self.outer.exported_function(owner, name) {
                        return self.call_declared(key, None, args, span);
                    }
                    if let Some(decl) = self.outer.exported_struct(owner, name) {
                        return on_the_value_stack(self.make_struct(owner, decl, args, span));
                    }
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, which module `{owner}` does not export"),
                        span,
                    ));
                }
                if builtins::is_builtin_type(head) {
                    return on_the_value_stack(self.call_builtin_assoc(head, name, args, span));
                }
            }
        }
        self.method_call(id, base, name, args, span)
    }

    /// A call to a function this package declares, with the receiver a
    /// method needs pushed first.
    ///
    /// Each argument is lowered into the stack its own parameter's slot kind
    /// names, and nothing is moved afterwards: the arguments already stand in
    /// declaration order — `arguments_in_order` refuses a call whose
    /// arguments do not — so within each stack they land in exactly the
    /// order that stack's slots are numbered in, and *become* those slots.
    ///
    /// Answers where the call left its result, which is the callee's
    /// `returns` read from the same signature the callee's own lowering
    /// reads. Both sides of a call therefore agree by construction rather
    /// than by convention, and `validate` says so out loud.
    pub(super) fn call_declared(
        &mut self,
        key: Key,
        receiver: Option<&'a Expr>,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        let declared = self.outer.declaration(key);
        let decl = declared.decl;
        let what = declared.name.clone();

        for param in &decl.params {
            reject_parameter(param)?;
        }
        let names: Vec<&str> = decl
            .params
            .iter()
            .map(|param| param.name.node.as_str())
            .collect();
        // `cove::type::variadic_position` has already refused a variadic
        // parameter that is not the last one, so the last one is the only
        // one there can be.
        let variadic = decl.params.last().is_some_and(|param| param.variadic);
        let assigned = arguments_in_order(&names, args, &what, variadic, span)?;

        // Which parameters this call site hands an argument, which is what
        // decides *which* function it calls: a parameter left out is one the
        // callee computes, so a callee that computes one is a different
        // function from a callee that is given it. A variadic parameter is
        // always supplied, because the leftovers are collected here into the
        // one `Array` it receives and an empty one is an argument like any
        // other.
        let mut supplied: Vec<bool> = assigned.slots.iter().map(Option::is_some).collect();
        if variadic {
            supplied[names.len() - 1] = true;
        }
        for (at, param) in decl.params.iter().enumerate() {
            if supplied[at] {
                continue;
            }
            if param.default.is_none() {
                return Err(Unsupported::new(
                    format!(
                        "a call to `{what}` that does not supply one argument for every parameter"
                    ),
                    span,
                ));
            }
            if param.is_var {
                // `bind_params` would bind the default's *value* here rather
                // than an alias, so the parameter the body writes through
                // would name storage no caller owns. Nothing writes one, and
                // refusing says so rather than lowering a place that names
                // nothing.
                return Err(Unsupported::new(
                    format!(
                        "a call to `{what}` that leaves the `var` parameter `{}` to a default",
                        param.name.node
                    ),
                    span,
                ));
            }
        }

        // The same signature the callee's own lowering reads, so the two
        // cannot disagree about where an argument goes; a declaration the
        // checker recorded nothing about falls back to the convention every
        // function had before, on both sides at once.
        let signature = self.outer.signature(key);
        // `Inst::Call` holds each count in a `u16` — see its doc comment for
        // what that buys — so a declaration with more parameters than that
        // is refused here rather than counted into a number that cannot
        // hold it. Nothing writes one; the check is what makes the width a
        // fact rather than an assumption.
        if decl.params.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{what}`, which has more than 65534 parameters"),
                span,
            ));
        }
        let mut value_argc: u16 = 0;
        let mut scalar_argc: u16 = 0;
        let mut place_argc: u16 = 0;
        let mut into = |kind: SlotKind| match kind {
            SlotKind::Value => value_argc += 1,
            SlotKind::Scalar(_) => scalar_argc += 1,
            SlotKind::Place => place_argc += 1,
        };

        match (decl.receiver, receiver) {
            (Some(declared), Some(expr)) => {
                if declared.is_var {
                    // A `var self` receiver is a place, and it is the one
                    // the method writes through. That it *is* a writable
                    // place is `cove-sema`'s to say and it has said it — see
                    // ADR 0021 — so this builds the place and nothing else.
                    into(SlotKind::Place);
                    self.place(expr)?;
                } else {
                    let kind = signature
                        .and_then(|signature| signature.receiver.as_ref())
                        .map_or(SlotKind::Value, slot_kind_of);
                    into(kind);
                    match kind {
                        SlotKind::Scalar(_) => self.expr_scalar(expr)?,
                        SlotKind::Value => self.expr(expr)?,
                        SlotKind::Place => {
                            unreachable!("only a `var self` receiver is a place")
                        }
                    }
                }
            }
            (Some(_), None) => {
                return Err(Unsupported::new(
                    format!("a call to the method `{what}` with no receiver"),
                    span,
                ))
            }
            (None, Some(_)) => {
                return Err(Unsupported::new(
                    format!("a call to `{what}`, which takes no receiver"),
                    span,
                ))
            }
            (None, None) => {}
        }
        // Every parameter but a variadic one takes at most one argument, and
        // `arguments_in_order` has already refused a call whose arguments do
        // not fill the parameters in increasing order — so pushing them in
        // the order the parameters are declared is pushing them in the order
        // they are written, and the one a parameter was left to its default
        // is simply not there.
        let fixed = names.len() - usize::from(variadic);
        for at in 0..fixed {
            let Some(position) = assigned.slots[at] else {
                continue;
            };
            let arg = args.at(position);
            // A `...` here fills one parameter's slot, and `bind_params`
            // reads that slot through `value_of` without looking at
            // `spread` — the whole array becomes the argument. Refused
            // rather than reproduced: see `no_spread_here`.
            if arg.spread {
                return Err(no_spread_here(&what, arg.span));
            }
            // The marking is at both ends and has to agree at both, which is
            // what `bind_params` checks at run time and what this checks
            // before the run.
            let declared_var = decl.params[at].is_var;
            if declared_var != arg.is_var {
                return Err(var_marking_disagrees(
                    &what,
                    &decl.params[at].name.node,
                    declared_var,
                    arg.span,
                ));
            }
            if declared_var {
                // A `var` argument names the caller's own place, and that it
                // is one, and a writable one, is `cove-sema`'s to say — see
                // ADR 0021.
                into(SlotKind::Place);
                self.place(arg.value)?;
                continue;
            }
            let kind = signature
                .and_then(|signature| signature.params.get(at))
                .map_or(SlotKind::Value, slot_kind_of);
            into(kind);
            match kind {
                SlotKind::Scalar(_) => self.expr_scalar(arg.value)?,
                SlotKind::Value => self.expr(arg.value)?,
                SlotKind::Place => unreachable!("only a `var` parameter is a place"),
            }
        }
        if variadic {
            // The arguments left over are the elements of the one `Array`
            // the callee receives, so they are pushed left to right and
            // collected here rather than passed as arguments of their own.
            // That is the whole of the change at a call site: the callee
            // still gets one argument per parameter and the calling
            // convention does not move.
            //
            // They go onto the value stack whatever the checker settled
            // about each of them, because an `Array` holds `Value`s and
            // `Inst::MakeArray` reads that stack. Zero of them is an empty
            // `Array`, which is what `bind_params` builds when
            // `assign_labels` left it nothing.
            //
            // A label written in the variadic parameter's own place is one
            // element rather than a pile of leftovers, which is what
            // `bind_params` makes of `slots[index]`; a call that writes both
            // was refused above. `bind_params` reads that one through
            // `value_of` and never looks at its `spread`, so a `...` written
            // there is a marking nothing acts on and is refused rather than
            // spread.
            if let Some(position) = assigned.slots[names.len() - 1] {
                if args.at(position).spread {
                    return Err(no_spread_here(&what, args.at(position).span));
                }
            }
            let elements: Vec<CallArg<'a>> = assigned.slots[names.len() - 1]
                .into_iter()
                .chain(assigned.rest.iter().copied())
                .map(|position| args.at(position))
                .collect();
            if let Some(arg) = elements.iter().find(|arg| arg.is_var) {
                return Err(Unsupported::new(
                    format!("a `var` argument to `{what}`, which takes values"),
                    arg.span,
                ));
            }
            self.variadic_array(&elements, span)?;
            into(SlotKind::Value);
        }
        // An `async fn` answers a settled task whatever its return type
        // says, and a task is a value: `async fn f() -> Int` leaves a
        // `Task<Int>` on the value stack, and only `await` produces the
        // `Int`. `declared_function` settles `returns` the same way, and
        // `validate` reconciles the two.
        let answer = match decl.is_async {
            true => None,
            false => signature.and_then(|signature| scalar_of_ty(&signature.ret)),
        };
        // This is the whole of the reachability rule: the call being emitted
        // is what makes the target part of the program, so the target is
        // numbered here and nowhere else.
        let function = self.outer.number(Instance::Declared {
            key,
            supplied,
            as_value: false,
        });
        self.emit(
            Inst::Call {
                function,
                value_argc,
                scalar_argc,
                place_argc,
                returns_scalar: answer.is_some(),
            },
            span,
        );
        Ok(answer)
    }

    /// The one `Array` a variadic parameter receives, built out of the
    /// arguments that were left over.
    ///
    /// Without a `...` this is `Inst::MakeArray` over the elements as
    /// written, which is what it has always been. A `...` passes an existing
    /// sequence where those elements would go, so the array is built in runs
    /// instead: `MakeArray` for each run of ordinary arguments, the spread's
    /// own value for each `...`, and `Inst::SpreadArgument` to join each
    /// piece to what came before. The empty array it starts from is what
    /// `bind_params` starts from too, and a call with no leftovers at all
    /// still lowers to the single `MakeArray` it did before.
    ///
    /// The pieces are appended as each is produced rather than after all of
    /// them are, which is one instruction's worth of difference from
    /// `eval_args`: the interpreter evaluates every argument and then reads
    /// them in `bind_params`, so a spread of something that is neither an
    /// `Array` nor a `Vector` is reported after the arguments to its right
    /// have run. The checker reports that spread before either backend sees
    /// it — ``` `...` spreads an `Array` or a `Vector`, but found `Int` ```
    /// is a check-time diagnostic — so the order is unobservable in a
    /// checked program, and stating it is cheaper than an instruction that
    /// would have to carry which of its operands were spreads.
    fn variadic_array(&mut self, elements: &[CallArg<'a>], span: Span) -> Result<(), Unsupported> {
        if !elements.iter().any(|arg| arg.spread) {
            for arg in elements {
                self.expr(arg.value)?;
            }
            self.emit(Inst::MakeArray(elements.len() as u32), span);
            return Ok(());
        }
        self.emit(Inst::MakeArray(0), span);
        let mut at = 0;
        while at < elements.len() {
            if elements[at].spread {
                self.expr(elements[at].value)?;
                // The argument's own span, because this is the instruction
                // that reports a spread of something that is neither
                // sequence and `bind_params` reports it at `arg.span`.
                self.emit(Inst::SpreadArgument, elements[at].span);
                at += 1;
                continue;
            }
            let from = at;
            while at < elements.len() && !elements[at].spread {
                at += 1;
            }
            for arg in &elements[from..at] {
                self.expr(arg.value)?;
            }
            self.emit(Inst::MakeArray((at - from) as u32), span);
            // Appending an `Array` this instruction just built, which cannot
            // be the failure the span above is for.
            self.emit(Inst::SpreadArgument, span);
        }
        Ok(())
    }

    /// `console.println(...)` and `clock.now()`.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Some(declared) = hosts::module(module).and_then(|schema| schema.declared_type(op)) {
            return self.make_host_type(module, declared, args, span);
        }
        plain_arguments(args, op)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let module = self.outer.name(module);
        let op = self.outer.name(op);
        self.emit(
            Inst::CallHost {
                module,
                op,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `http.Route(method: ..., path: ..., handler: ...)`: one value of a
    /// type a host module declares.
    ///
    /// This crosses no boundary. `Interpreter::init_host_type` is
    /// `init_struct` with the field names read from a `TypeSchema` instead of
    /// from a declaration — the same `assign_labels`, the same one value per
    /// field, and an ordinary `Value::Struct` whose `type_name` is
    /// `{module}.{Name}` and whose `opaque` is false — so it lowers to the
    /// instruction that builds an ordinary struct, with the qualified name
    /// the schema spells and the fields in the order the schema declares
    /// them. `is_opaque` answers false for it in the VM for the reason it
    /// answers false in the interpreter: no module of this package declares
    /// `Route`.
    ///
    /// An enum a host declares is not written this way — `http.Method.Get`
    /// is a case, and `init_host_type` reports the call as an error — so a
    /// call that names one is refused rather than built.
    ///
    /// Which types a module declares is read from the schema this crate can
    /// see, where the interpreter reads it from the `HostRegistry` the run
    /// was given. The two answer differently only for a registry that left a
    /// module out, which no runner builds: `cove run`, `cove test`, and
    /// `embed` each register every host and let the grants decide what a
    /// *call* may do. `Vm`'s own test harness registers every host for that
    /// reason and no other.
    fn make_host_type(
        &mut self,
        module: &str,
        declared: &'static TypeSchema,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        if declared.is_enum() {
            return Err(Unsupported::new(
                format!(
                    "`{module}.{}`, which is a host enum and not a function",
                    declared.name
                ),
                span,
            ));
        }
        let names: Vec<&str> = declared.fields.iter().map(|field| field.name).collect();
        every_argument_supplied(&names, args, declared.name, span)?;
        plain_arguments(args, declared.name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(&format!("{module}.{}", declared.name));
        let fields = self.outer.name(&names.join(","));
        self.emit(Inst::MakeStruct { ty, fields }, span);
        Ok(())
    }

    /// `Ok(...)`, `Err(...)`, `Some(...)`, `Error(...)`, `Shared(...)`,
    /// `assert(...)`, and `assertEqual(...)`, which is every free builtin
    /// there is.
    ///
    /// The two assertions carry their arguments' spans as well as their own.
    /// A failing `assert` quotes the source text of its condition — that is
    /// what makes it a builtin rather than a library function — and the
    /// instruction's own span covers the whole call, so the argument's span
    /// is recorded beside it in [`crate::Function::arg_spans`]. The
    /// interpreter reads exactly these spans, out of the same `SourceMap`.
    fn make_builtin(&mut self, name: &str, args: Args<'a>, span: Span) -> Result<(), Unsupported> {
        // `Shared` is here rather than beside the three `Result`/`Option`
        // constructors because it is the one that can refuse its payload:
        // what a cell wraps must be task-safe, since a `Shared` is reachable
        // from every task it was given to. `builtins::call_constructor` makes
        // that check, so both backends refuse the same payloads in the same
        // words.
        if !matches!(
            name,
            "Ok" | "Err" | "Some" | "Error" | "Shared" | "assert" | "assertEqual"
        ) {
            return Err(Unsupported::new(format!("`{name}`"), span));
        }
        let quotes_its_arguments = matches!(name, "assert" | "assertEqual");
        plain_arguments(args, name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let name = self.outer.name(name);
        let pc = self.code.len();
        self.emit(
            Inst::MakeBuiltin {
                name,
                argc: args.len() as u32,
            },
            span,
        );
        // `emit` keeps nothing where control cannot arrive, so the spans are
        // recorded against the instruction that was actually written.
        if quotes_its_arguments && self.code.len() > pc {
            self.arg_spans.insert(
                pc as u32,
                args.iter().map(|arg| arg.value.span).collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// `Cursor(at: 0, step: 1)`: a synthesized labelled call, whose values
    /// are pushed in the order the fields were declared.
    fn make_struct(
        &mut self,
        owner: &str,
        decl: &'a StructDecl,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        for field in &decl.fields {
            reject_dyn(&field.ty, "a `dyn` field")?;
        }
        let names: Vec<&str> = decl
            .fields
            .iter()
            .map(|field| field.name.node.as_str())
            .collect();
        every_argument_supplied(&names, args, &decl.name.node, span)?;
        plain_arguments(args, &decl.name.node)?;
        for (at, arg) in args.iter().enumerate() {
            self.expr(arg.value)?;
            // Each field's value is converted against the type the field was
            // written with, in the module that declares the struct rather
            // than the one initializing it — which is what
            // `Interpreter::init_struct` passes to `coerce`. The `at`th
            // argument fills the `at`th field because
            // `every_argument_supplied` accepted this call: every field has
            // an argument and the arguments fill them in increasing order.
            self.coerce_to(owner, &decl.fields[at].ty, arg.span);
        }
        let ty = self.outer.name(&format!("{owner}.{}", decl.name.node));
        let fields = self.outer.name(&names.join(","));
        self.emit(Inst::MakeStruct { ty, fields }, span);
        Ok(())
    }

    /// `Status.Confirmed` and `Json.Text(t)`: one case of a declared enum.
    ///
    /// The instruction carries the *qualified* type name, because that is
    /// what a case value holds — two modules may each declare a `Status`, and
    /// `Interpreter::enum_case` writes `{module}.{Enum}` into the value so
    /// that they stay two types.
    ///
    /// Whether the enum declares this case, and whether the payload is the
    /// length the case carries, are not asked here. `enum_case` asks them
    /// when the value is built and reports each in its own words, and the VM
    /// calls that same function; asking twice would be a second place for the
    /// answer to be written down.
    pub(super) fn make_enum(
        &mut self,
        owner: &str,
        enum_name: &str,
        case: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        plain_arguments(args, case)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(&format!("{owner}.{enum_name}"));
        let case = self.outer.name(case);
        self.emit(
            Inst::MakeEnum {
                ty,
                case,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `Vector.of(...)`, `Int.parse(text)`, and the rest of
    /// `builtins::call_associated`.
    ///
    /// The arguments are pushed in the order they are written and nothing
    /// else is checked: the interpreter reaches these through `plain_values`,
    /// which reads an argument's value and never its label, so a variadic
    /// like `Vector.of` and a fixed one like `Int.parse` are the same shape
    /// here and their arity is the callee's to complain about.
    ///
    /// A name the type has no associated function for is emitted too, for the
    /// reason a missing enum case is: the failure belongs to the call, and
    /// the one function both backends dispatch through is where it is worded.
    fn call_builtin_assoc(
        &mut self,
        ty: &str,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        plain_arguments(args, name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(ty);
        let name = self.outer.name(name);
        self.emit(
            Inst::CallBuiltinAssoc {
                ty,
                name,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `MapEntry(key: k, value: v)`, the one pair a `Map` is built from.
    ///
    /// It is a builtin *struct* rather than an associated function — nothing
    /// is called on the name, and `init_map_entry` builds a `StructValue`
    /// exactly as a declared struct's synthesized initializer does — so it
    /// lowers to the builtin that builds one, with its two fields pushed in
    /// declaration order. `assign_labels` is what the interpreter puts them
    /// in that order with, and [`arguments_in_order`] is the same rule read
    /// at lowering time.
    fn make_map_entry(&mut self, args: Args<'a>, span: Span) -> Result<(), Unsupported> {
        let names: Vec<&str> = builtins::MAP_ENTRY
            .fields
            .iter()
            .map(|field| field.name)
            .collect();
        every_argument_supplied(&names, args, builtins::MAP_ENTRY.name, span)?;
        plain_arguments(args, builtins::MAP_ENTRY.name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let name = self.outer.name(builtins::MAP_ENTRY.name);
        self.emit(
            Inst::MakeBuiltin {
                name,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }
}

/// Which parameter each argument fills, refusing every shape whose answer
/// the lowering would have to rearrange.
///
/// This is `assign_labels` in the interpreter, asked before the run instead
/// of during it. That function matches a positional argument to the next
/// parameter not yet filled, matches a label to the parameter of that name,
/// refuses a label whose parameter stands before one already filled, and
/// refuses a positional argument after a labelled one. What survives is a
/// call whose arguments fill parameters in strictly increasing order — which
/// is what makes pushing them left to right the same as pushing them in
/// declaration order.
///
/// A parameter no argument fills is left to its default, and a default is
/// evaluated by the callee, so it is not this function's business beyond
/// saying which ones they are. `Body::call_declared` reads that from
/// [`Arguments::slots`] and specialises the callee; a parameter with no
/// default to fall back on is what is reported here, in the words
/// `bind_params` reports it in.
///
/// `variadic` says the last parameter takes every argument left over, which
/// changes two of the three questions and neither of the others. There is no
/// longer a most: `assign_labels` puts a positional argument past the last
/// parameter into `rest` rather than reporting one too many. And a variadic
/// parameter is never missing, since one given nothing is an empty `Array`.
///
/// The out-of-order case is the checker's, not this pass's. `cove-sema`
/// reports `cove::type::label_order` for a label whose parameter stands
/// before one an earlier argument already filled, so no checked program
/// reaches here with one — and this still refuses it, because
/// [`Arguments::slots`] is read by call sites that push arguments left to
/// right and the property they rely on is worth stating where it is relied
/// on rather than assumed from somewhere else. ADR 0021 is why the two are
/// not the same kind of statement: the checker's is a language rule and this
/// is an invariant of a calling convention.
///
/// The surprising case is the one refused by name. `assign_labels` will
/// accept `f(1, 2, items: 3)` and bind `items` to `[3, 2]` — the labelled
/// argument first and the ones that fell into `rest` after it, which is also
/// what the checker's `match_arguments` does — and a lowering that pushed
/// those left to right would have them the other way round. Rather than
/// rearrange them, a variadic parameter that is written with a label *and*
/// collects leftovers is reported.
pub(super) fn arguments_in_order(
    names: &[&str],
    args: Args<'_>,
    what: &str,
    variadic: bool,
    span: Span,
) -> Result<Arguments, Unsupported> {
    let mut slots: Vec<Option<usize>> = vec![None; names.len()];
    let mut rest: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut labelled = false;
    for (position, arg) in args.iter().enumerate() {
        match &arg.label {
            Some(label) => {
                labelled = true;
                let Some(index) = names.iter().position(|name| *name == label.node) else {
                    return Err(Unsupported::new(
                        format!("`{what}`, which has no parameter labelled `{}`", label.node),
                        arg.span,
                    ));
                };
                if index < next {
                    return Err(Unsupported::new(
                        format!(
                            "a call to `{what}` whose arguments do not stand in declaration order"
                        ),
                        arg.span,
                    ));
                }
                slots[index] = Some(position);
                next = index + 1;
            }
            None => {
                if labelled {
                    return Err(Unsupported::new(
                        format!(
                            "a call to `{what}` with a positional argument after a labelled one"
                        ),
                        arg.span,
                    ));
                }
                if variadic && next + 1 >= names.len() {
                    rest.push(position);
                } else if next < names.len() {
                    slots[next] = Some(position);
                    next += 1;
                } else {
                    return Err(Unsupported::new(
                        format!("a call to `{what}` with more arguments than it has parameters"),
                        arg.span,
                    ));
                }
            }
        }
    }
    if variadic && !rest.is_empty() && slots[names.len() - 1].is_some() {
        return Err(Unsupported::new(
            format!("a call to `{what}` that labels its variadic parameter and passes more"),
            span,
        ));
    }
    Ok(Arguments { slots, rest })
}

/// A call's arguments: the ones written inside the parentheses, and the
/// trailing closure written after them.
///
/// `f(x) { ... }` is sugar and nothing more. `Interpreter::eval_args`
/// evaluates the written arguments left to right and then pushes the
/// trailing one on the end as an unlabelled, non-`var`, non-spread argument
/// — so a trailing closure *is* the last positional argument, and the whole
/// of what this type does is let every path that reads a call's arguments
/// say so once instead of each of them taking a second parameter it would
/// have to remember to use.
///
/// The parser has already built the block as an `ExprKind::Lambda` with no
/// parameters, so the value is an ordinary expression here and lowers
/// through the ordinary lambda path.
#[derive(Clone, Copy)]
pub(super) struct Args<'a> {
    written: &'a [Arg],
    trailing: Option<&'a Expr>,
}

/// One argument of a call, whichever side of the parentheses it was written
/// on.
///
/// A written one is its [`Arg`] read field by field; a trailing one is the
/// expression with the four answers `eval_args` gives it — no label, not
/// `var`, not a spread, and its own span.
#[derive(Clone, Copy)]
pub(super) struct CallArg<'a> {
    pub(super) label: Option<&'a cove_syntax::ast::Ident>,
    pub(super) is_var: bool,
    pub(super) spread: bool,
    pub(super) value: &'a Expr,
    pub(super) span: Span,
}

impl<'a> Args<'a> {
    /// The arguments written inside the parentheses, and the trailing
    /// closure when one was written.
    pub(super) fn new(written: &'a [Arg], trailing: Option<&'a Expr>) -> Args<'a> {
        Args { written, trailing }
    }

    pub(super) fn len(self) -> usize {
        self.written.len() + usize::from(self.trailing.is_some())
    }

    pub(super) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The argument at `position`, where the trailing closure is the one
    /// past the written ones.
    pub(super) fn at(self, position: usize) -> CallArg<'a> {
        match self.written.get(position) {
            Some(arg) => CallArg {
                label: arg.label.as_ref(),
                is_var: arg.is_var,
                spread: arg.spread,
                value: &arg.value,
                span: arg.span,
            },
            None => {
                let trailing = self
                    .trailing
                    .expect("a position past the written arguments is the trailing closure");
                CallArg {
                    label: None,
                    is_var: false,
                    spread: false,
                    value: trailing,
                    span: trailing.span,
                }
            }
        }
    }

    pub(super) fn iter(self) -> impl Iterator<Item = CallArg<'a>> {
        (0..self.len()).map(move |position| self.at(position))
    }
}

/// Which argument fills each parameter, and which arguments a variadic
/// parameter collects.
pub(super) struct Arguments {
    /// For each parameter, the position of the argument that fills it, or
    /// `None` where the call left it to its default.
    pub(super) slots: Vec<Option<usize>>,
    /// The positions of the arguments that fell past the last parameter, in
    /// the order they are written, which a variadic parameter collects.
    pub(super) rest: Vec<usize>,
}

/// The rule [`arguments_in_order`] states, for a call that admits no default
/// and no variadic parameter.
///
/// A struct's synthesized initializer, `MapEntry`, and a type a host module
/// declares are all of that shape: every field is written or the call is
/// wrong, and the interpreter says so with its own words rather than with a
/// default it does not have.
fn every_argument_supplied(
    names: &[&str],
    args: Args<'_>,
    what: &str,
    span: Span,
) -> Result<(), Unsupported> {
    let assigned = arguments_in_order(names, args, what, false, span)?;
    if assigned.slots.iter().any(Option::is_none) {
        return Err(Unsupported::new(
            format!("a call to `{what}` that does not supply one argument for every parameter"),
            span,
        ));
    }
    Ok(())
}

/// A `var` argument written where the parameter is not declared `var`, or a
/// parameter declared `var` given an argument that is not.
///
/// The interpreter refuses both at run time, in `bind_params`, and it
/// refuses them because the marking is deliberately at both ends: "A `var`
/// parameter is a non-escaping inout alias, marked at both the declaration
/// and the call site." A checked program should not reach either message,
/// and a backend that quietly accepted one would be more permissive than the
/// oracle.
fn var_marking_disagrees(what: &str, param: &str, declared_var: bool, span: Span) -> Unsupported {
    Unsupported::new(
        match declared_var {
            true => format!("a call to `{what}`, whose parameter `{param}` is declared `var` and whose argument is not written `var`"),
            false => format!("a call to `{what}`, whose parameter `{param}` is not declared `var` and whose argument is written `var`"),
        },
        span,
    )
}

/// Neither marking a call site can write, at a call this backend does not
/// route through a declared function's parameters.
///
/// A struct initializer, a host operation, an enum case, a builtin, and a
/// builtin's associated function all take values. None of them declares a
/// `var` parameter, so `var` written at one is a program the interpreter
/// refuses too; and none of them collects a variadic parameter's elements,
/// so a `...` written at one is a marking the interpreter *ignores* — which
/// is refused here instead. See [`no_spread_here`].
pub(super) fn plain_arguments(args: Args<'_>, what: &str) -> Result<(), Unsupported> {
    if let Some(arg) = args.iter().find(|arg| arg.is_var) {
        return Err(Unsupported::new(
            format!("a `var` argument to `{what}`, which takes values"),
            arg.span,
        ));
    }
    if let Some(arg) = args.iter().find(|arg| arg.spread) {
        return Err(no_spread_here(what, arg.span));
    }
    Ok(())
}

/// A `...` written where nothing collects the elements it would spread.
///
/// Only a variadic parameter of a declared function does. Everywhere else
/// the interpreter reads the argument's *value* and never its `spread` flag:
/// `println(...["a"])` hands `console.println` one `Array` and fails against
/// the schema, and `k(...[1, 2, 3])` binds the whole array to `k`'s one
/// parameter. Refusing rather than reproducing that is the direction a
/// second backend is allowed to be wrong in, and it keeps the flag from
/// being carried through paths that would have to ignore it.
fn no_spread_here(what: &str, span: Span) -> Unsupported {
    Unsupported::new(
        format!("a `...` spread argument to `{what}`, which collects nothing"),
        span,
    )
}

/// A call that answers on the value stack, whatever it produced.
///
/// Everything a call can lower to other than [`Inst::Call`] hands back a
/// `Value`: a builtin method, a host operation, a struct initializer, an
/// enum case, an assertion, and a builtin type's associated function are all
/// the interpreter's own code, and the interpreter speaks `Value`. Saying so
/// through one function keeps `Body::call_declared` the only place where a
/// call's answer can be anything else.
fn on_the_value_stack(lowered: Result<(), Unsupported>) -> Result<Option<Scalar>, Unsupported> {
    lowered.map(|()| None)
}
