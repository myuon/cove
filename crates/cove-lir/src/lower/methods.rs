//! The methods of the types the language ships.
//!
//! A method call is written on a value — `text.split(",")` — or on a type's
//! own name — `Int.parse(text)` — and which type it is written on decides
//! everything. The checker settled that already, so this reads
//! [`Facts::ty`](cove_sema::Facts::ty) of the receiver rather than resolving
//! the name a second time.
//!
//! # The machine's table is the specification
//!
//! `cove_runtime::lvm::builtins` dispatches on the pair
//! [`Builtin`] names — a receiver and an operation — and what it implements
//! is what may be emitted here. Every one of them takes its operands in one
//! shape: the receiver first where there is one, then the arguments in
//! source order, and the answer is the word the checker settled for the
//! call. An operation the machine does not have is a gap naming it,
//! `` `Array.map` `` rather than "a method call", because the message is
//! what says where the next piece of work is.
//!
//! That is why [`MACHINE_METHODS`] is a list rather than a fall-through:
//! what this lowering emits is a contract the machine is written against,
//! and a name that reached the machine by accident would be a runtime
//! refusal where a gap should have named the work.
//!
//! # `Option` and `Result` are not in that table, and are not added to it
//!
//! An `Option` is an enum object, and `isSome()` is the question a `match`
//! already asks of one: word 0 is the case index, so the answer is a
//! [`Inst::GetWord`] and a comparison. `unwrapOr(fallback)` is that question
//! and a branch. Both are lowered here, directly, because a builtin for
//! either would be a call into the runtime to read one word the instruction
//! set reads on its own — and the receiver would have to be held across it.
//! `mapError` is lowered here too, and it is the one of the three that takes
//! a closure. There is no loop in it to lower to — a `Result` is one value
//! rather than a sequence — but the rule `docs/LINEAR_VM.md` states for a
//! sequence method holds for the same reason: a builtin never calls back into
//! Cove, so what runs the callback is an ordinary [`Inst::CallClosure`] frame
//! and not a re-entry into the dispatch loop from inside a Rust function. So
//! it is a branch and one call. `cove_lir::lower::walks` is where the four
//! that *are* walks live.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes;
use super::{Body, PENDING};
use crate::inst::{CmpOp, Compare, Inst, Slot};
use crate::layout::LayoutId;
use crate::program::Builtin;

impl Body<'_> {
    /// A method call on a value of a builtin type.
    ///
    /// A receiver whose methods this lowering has not been taught is a gap
    /// naming the method, because that is the sentence that says where the
    /// next piece of work is.
    pub(super) fn call_builtin_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        name: &str,
        args: &[Arg],
    ) -> Val {
        let Some(ty) = self.settled_ty(base) else {
            return self.dead(expr);
        };
        if let Some(bad) = self.plain_arguments(args) {
            return self.gap(bad, expr);
        }
        // Meeting a value of the type is what declares its layout, and a
        // method call is meeting one. It matters for a vector: the machine
        // finds the store to grow into by looking the family up in this
        // table, and a program that only ever receives a vector from
        // somewhere else would otherwise declare no store for it. A `Set` and
        // a `Map` are the same case — `Set.inserted` answers a new object of
        // the receiver's own family, and the machine reads that family out of
        // the receiver's header rather than out of the table, but a program
        // that only ever receives one still has to have declared it.
        if matches!(ty, Ty::Array(_) | Ty::Vector(_) | Ty::Set(_) | Ty::Map(..))
            && self.layout(&ty, base.span).is_none()
        {
            return self.dead(expr);
        }
        match &ty {
            Ty::Array(elem) => {
                let elem = (**elem).clone();
                self.array_method(expr, base, &elem, name, args)
            }
            Ty::Vector(elem) => {
                let elem = (**elem).clone();
                self.vector_method(expr, base, &elem, name, args)
            }
            Ty::Set(_) => self.set_method(expr, base, name, args),
            Ty::Map(..) => self.map_method(expr, base, name, args),
            Ty::Option(_) | Ty::Result(..) => self.answer_method(expr, base, &ty, name, args),
            // A host resource's operations belong to the host that issued
            // the handle, and the handle is what routes them:
            // `HostRegistry::call_resource` reads the module and the resource
            // kind off it rather than off the call site. There is no
            // instruction here that addresses one — [`Inst::CallHost`] names
            // a module and an operation, and this names a *handle* — so the
            // work is an instruction, its verifier arm, and the boundary that
            // routes it, and naming the operation is what says so.
            //
            // A host type that is plain *data* never arrives: it has fields
            // rather than operations, and the checker has already refused a
            // method call on one.
            Ty::Host(qualified) => self.gap(
                &format!("`{qualified}.{name}`, an operation of a host resource"),
                expr,
            ),
            _ => {
                let Some(receiver) = receiver_name(&ty) else {
                    // A declared type's methods do not come here: the
                    // checker recorded which declaration such a call
                    // resolved to, and `Body::call_through` reads that
                    // before anything else. What is left is a receiver
                    // whose type this lowering has no methods for at all —
                    // a type parameter, a function value — and naming the
                    // call is the most that can be said about it.
                    return self.gap("a method call", expr);
                };
                if !MACHINE_METHODS.contains(&(receiver, name)) {
                    return self.gap(&format!("`{receiver}.{name}`"), expr);
                }
                self.machine_call(expr, Some(base), receiver, name, args)
            }
        }
    }

    /// `Int.parse(text)`, `Duration.millis(n)`: an operation of a builtin
    /// type written through the type's own name.
    ///
    /// It has no receiver, so its operands are its arguments alone. That is
    /// also what tells a `Duration` builder from a `Duration` reader — see
    /// [`Body::machine_call`].
    pub(super) fn call_associated(
        &mut self,
        expr: &Expr,
        receiver: &str,
        operation: &str,
        args: &[Arg],
    ) -> Val {
        if let Some(bad) = self.plain_arguments(args) {
            return self.gap(bad, expr);
        }
        self.machine_call(expr, None, receiver, operation, args)
    }

    /// An operation the machine performs, over the operands the call site
    /// gives it.
    ///
    /// The receiver is the first operand where there is one and the
    /// arguments follow it in source order, which is the one shape every
    /// operation in the table has. The result is the layout the checker
    /// settled for the call.
    ///
    /// # The `Repr` of operand 0 is part of what is emitted
    ///
    /// `Duration.seconds(1)` builds a duration and `d.seconds()` reads one
    /// back out, and the language spells them the same. The machine tells
    /// the two apart by the `Repr` of operand 0 — `Repr::Duration` is the
    /// receiver of a reader, and anything else is the count of a builder —
    /// and that is a static fact about the slot chosen here: a reader's
    /// first operand is its receiver, whose type the checker settled as
    /// `Duration`, and a builder has no receiver and passes an `Int` count.
    /// Nothing is inferred from a word on either side.
    pub(super) fn machine_call(
        &mut self,
        expr: &Expr,
        base: Option<&Expr>,
        receiver: &str,
        operation: &str,
        args: &[Arg],
    ) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Some(result) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        if !self.answer_layouts(&ty, expr.span) {
            return self.dead(expr);
        }

        let held_receiver = base.map(|base| self.expr(base));
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let mut passed = Vec::with_capacity(args.len() + 1);
        passed.extend(held_receiver.iter().map(Val::arg));
        passed.extend(held.iter().map(Val::arg));

        let dst = self.temp(result);
        self.emit_builtin(dst.slot, receiver, operation, &passed, result, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        // The receiver dies with the call: nothing after the answer is
        // written reads it.
        if let Some(value) = held_receiver {
            self.release(value, expr.span);
        }
        dst
    }

    /// Interns the families the machine will look for while it builds this
    /// call's answer.
    ///
    /// `cove_runtime::lvm::builtins` finds a family by searching the
    /// program's layout table, so a family the program never otherwise
    /// mentions is a refusal at run time rather than a missing instruction.
    /// The answer's own layout is interned by the caller; what is left is
    /// the builtin `Error`, when the answer is a `Result`. The machine
    /// builds the `Error` carrying a failure's message *itself*, and the
    /// `Result` layout describes its `Err` words without saying what
    /// declared them — so interning the `Result` alone would leave
    /// `Int.parse("x")` with nowhere to put the message.
    fn answer_layouts(&mut self, ty: &Ty, span: Span) -> bool {
        if let Ty::Result(_, error) = ty {
            if matches!(**error, Ty::Error) && self.layout(error, span).is_none() {
                return false;
            }
        }
        true
    }

    /// One [`Inst::CallBuiltin`], and the [`Builtin`] it names.
    ///
    /// The pool interns, so a program that splits a string in twenty places
    /// names one builtin and one argument list per distinct operand shape.
    pub(super) fn emit_builtin(
        &mut self,
        dst: Slot,
        receiver: &str,
        operation: &str,
        args: &[crate::program::Arg],
        result: LayoutId,
        span: Span,
    ) {
        let builtin = self.pool.builtin(Builtin {
            receiver: receiver.into(),
            operation: operation.into(),
            result,
        });
        let args = self.pool.args.intern(args.to_vec());
        self.emit(Inst::CallBuiltin { dst, builtin, args }, span);
    }

    /// The argument shapes a builtin method has no place for, named as the
    /// source writes them.
    ///
    /// # A label is not a permutation here either
    ///
    /// `items.sorted(by: fn(a, b) { a < b })` is written with the parameter
    /// name the schema declares, and nothing here reorders anything. The
    /// checker has already refused a label out of declaration order, a
    /// positional argument after a labelled one, and a repeated label; and no
    /// builtin method declares a parameter with a default, so a list that
    /// arity-checks lines up with the parameters one for one. That is
    /// [`Body::operands`]'s reasoning, said of the table
    /// `cove_schema::builtins` writes rather than of a declaration.
    ///
    /// What is left is the two an operand list has no room for. A `var`
    /// argument is an address and a builtin takes values; a spread expands
    /// into a variadic, and the two builtins that declare one — `Set.of` and
    /// `Map.of` — collect their operands from the call site, so the expansion
    /// would have to happen here and has not been written.
    pub(super) fn plain_arguments(&self, args: &[Arg]) -> Option<&'static str> {
        args.iter().find_map(|arg| {
            if arg.is_var {
                Some("a `var` argument to a builtin method")
            } else if arg.spread {
                Some("a spread argument to a builtin method")
            } else {
                None
            }
        })
    }

    // ---- `Option` and `Result` ---------------------------------------------

    /// A method of the two enums the language answers a failure with.
    ///
    /// Neither is in the machine's table and neither is added to it: both
    /// questions are about the value's discriminant, which is word 0 and is
    /// already in the frame. See the module docs.
    fn answer_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        ty: &Ty,
        name: &str,
        args: &[Arg],
    ) -> Val {
        let receiver = if matches!(ty, Ty::Option(_)) {
            "Option"
        } else {
            "Result"
        };
        match (receiver, name, args.len()) {
            ("Option", "isSome", 0) => self.case_test(expr, base, ty, "Some"),
            ("Option", "isNone", 0) => self.case_test(expr, base, ty, "None"),
            ("Result", "isOk", 0) => self.case_test(expr, base, ty, "Ok"),
            // `isError`, not `isErr`: the case is called `Err` and the
            // question is called `isError`, and both names are the
            // language's — `cove_schema::builtins` writes them.
            ("Result", "isError", 0) => self.case_test(expr, base, ty, "Err"),
            ("Option", "unwrapOr", 1) => self.unwrap_or(expr, base, ty, "Some", &args[0].value),
            ("Result", "unwrapOr", 1) => self.unwrap_or(expr, base, ty, "Ok", &args[0].value),
            ("Result", "mapError", 1) => self.map_error(expr, base, ty, &args[0].value),
            _ => self.gap(&format!("`{receiver}.{name}`"), expr),
        }
    }

    /// Whether the value is in the case `case`.
    ///
    /// The discriminant is word 0 of the value, so the comparison names the
    /// value's own location and nothing is read out of anything.
    fn case_test(&mut self, expr: &Expr, base: &Expr, ty: &Ty, case: &str) -> Val {
        let Some((index, _)) = shapes::case_at(self.checked, self.module, ty, case) else {
            self.report(ty, expr.span);
            return self.dead(expr);
        };
        let obj = self.expr(base);
        let wanted = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: wanted.slot,
                value: index as i64,
            },
            expr.span,
        );
        let dst = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: dst.slot,
                a: obj.slot,
                b: wanted.slot,
            },
            expr.span,
        );
        self.give_back(wanted.slot, wanted.layout);
        self.release(obj, expr.span);
        dst
    }

    /// `value.unwrapOr(fallback)`: the payload of the carrying case, or the
    /// fallback.
    ///
    /// The fallback is evaluated before the branch and whichever way the
    /// branch goes, because it is an ordinary argument: the language
    /// evaluates a call's arguments before the call, and one of them may do
    /// something. Making it lazy here would be this lowering deciding
    /// something the language did not — the oracle's `unwrapOr` receives it
    /// already evaluated.
    fn unwrap_or(
        &mut self,
        expr: &Expr,
        base: &Expr,
        ty: &Ty,
        carrier: &str,
        fallback: &Expr,
    ) -> Val {
        let Some((index, _)) = shapes::case_at(self.checked, self.module, ty, carrier) else {
            self.report(ty, expr.span);
            return self.dead(expr);
        };
        let layout = self.layout_of(expr);
        let dst = self.temp(layout);
        let obj = self.expr(base);
        let other = self.expr(fallback);
        let Some((parts, _)) = self.case_of(obj.layout, index) else {
            self.release(other, expr.span);
            self.release(obj, expr.span);
            return self.gap("`unwrapOr` on a value that is not an enum here", expr);
        };

        let wanted = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: wanted.slot,
                value: index as i64,
            },
            expr.span,
        );
        let carries = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: carries.slot,
                a: obj.slot,
                b: wanted.slot,
            },
            expr.span,
        );
        self.give_back(wanted.slot, wanted.layout);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: carries.slot,
                to: PENDING,
            },
            expr.span,
        );
        self.give_back(carries.slot, carries.layout);

        match parts.first() {
            Some(part) => self.copy(dst.slot, obj.slot + 1 + part.at, part.layout, expr.span),
            None => {
                self.emit(Inst::Unit { dst: dst.slot }, expr.span);
            }
        }
        let carry_on = self.emit(Inst::Jump { to: PENDING }, expr.span);
        let otherwise = self.here();
        self.patch(branch, otherwise);
        self.copy(dst.slot, other.slot, layout, expr.span);
        let end = self.here();
        self.patch(carry_on, end);

        self.release(other, expr.span);
        self.release(obj, expr.span);
        dst
    }

    /// `result.mapError { ... }`: the `Ok` carried through, the failure
    /// replaced by what the callback answers.
    ///
    /// This is the one the module docs above named as owed, and it is what
    /// they said it would be: a branch and one [`Inst::CallClosure`]. A
    /// `Result` is one value rather than a sequence, so there is no loop to
    /// build — but the rule `docs/LINEAR_VM.md` states for `map` holds here
    /// for the same reason, and the callback runs as an ordinary frame
    /// rather than from inside a builtin that re-entered the dispatch loop.
    ///
    /// **The two `Result`s are two layouts.** `Int.parse(text)` answers a
    /// `Result<Int, Error>` and `.mapError { ConfigError.InvalidPort(text) }`
    /// answers a `Result<Int, ConfigError>`, so the `Ok` that is "carried
    /// through" is copied rather than passed along: the oracle answers the
    /// receiver itself because its values carry their own shape, and here a
    /// location's width is its layout's.
    ///
    /// The callback is evaluated **before** the branch and whichever way the
    /// branch goes, exactly as [`Body::unwrap_or`]'s fallback is and for the
    /// same reason: it is an ordinary argument, and the language evaluates a
    /// call's arguments before the call.
    ///
    /// Whether it is handed the error it replaces is read off the function
    /// type the checker settled rather than off the syntax. The oracle asks
    /// `Host::arity`, and `Checker::map_error` accepts both a callback that
    /// takes the error and one that ignores it — so the settled type is the
    /// one place both spellings have already agreed.
    fn map_error(&mut self, expr: &Expr, base: &Expr, ty: &Ty, callback: &Expr) -> Val {
        let (Some((ok_at, _)), Some((err_at, _))) = (
            shapes::case_at(self.checked, self.module, ty, "Ok"),
            shapes::case_at(self.checked, self.module, ty, "Err"),
        ) else {
            self.report(ty, expr.span);
            return self.dead(expr);
        };
        let Some(func) = self.callback(callback) else {
            return self.dead(expr);
        };
        let Some(replaced) = self.layout(&func.ret, callback.span) else {
            return self.dead(expr);
        };

        let layout = self.layout_of(expr);
        let dst = self.temp(layout);
        let obj = self.expr(base);
        let closure = self.expr(callback);
        // Taken before the branch although only one arm writes it: a run
        // allocated inside an arm would be handed back to the next
        // temporary while the other arm still had a jump into it.
        let answer = self.temp(replaced);

        let carried = self.case_of(obj.layout, ok_at);
        let failed = self.case_of(obj.layout, err_at);
        let (Some((carried, _)), Some((failed, _))) = (carried, failed) else {
            self.release(answer, expr.span);
            self.release(closure, expr.span);
            self.release(obj, expr.span);
            return self.gap("`mapError` on a value that is not an enum here", expr);
        };

        let wanted = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: wanted.slot,
                value: ok_at as i64,
            },
            expr.span,
        );
        let succeeded = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: succeeded.slot,
                a: obj.slot,
                b: wanted.slot,
            },
            expr.span,
        );
        self.give_back(wanted.slot, wanted.layout);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: succeeded.slot,
                to: PENDING,
            },
            expr.span,
        );
        self.give_back(succeeded.slot, succeeded.layout);

        let held: Vec<Val> = carried
            .iter()
            .map(|part| Val::borrowed(obj.slot + 1 + part.at, part.layout))
            .collect();
        self.write_case(dst.slot, layout, ok_at, &held, expr.span);
        let carry_on = self.emit(Inst::Jump { to: PENDING }, expr.span);

        let otherwise = self.here();
        self.patch(branch, otherwise);
        // A callback written to ignore the error takes no operand, which is
        // what `Host::arity` answers zero for on the other side.
        let operands = match (func.params.is_empty(), failed.first()) {
            (false, Some(part)) => {
                vec![Val::borrowed(obj.slot + 1 + part.at, part.layout).arg()]
            }
            _ => Vec::new(),
        };
        self.call_closure(answer.slot, closure.slot, operands, expr.span);
        let fitted = self.fit(
            Val::borrowed(answer.slot, replaced),
            self.case_layout(layout, err_at),
            expr.span,
        );
        self.write_case(
            dst.slot,
            layout,
            err_at,
            std::slice::from_ref(&fitted),
            expr.span,
        );
        self.release(fitted, expr.span);
        let end = self.here();
        self.patch(carry_on, end);

        self.release(answer, expr.span);
        self.release(closure, expr.span);
        self.release(obj, expr.span);
        dst
    }

    /// The layout of the one thing case `index` of an enum-shaped layout
    /// carries, and the layout of `()` for one that carries nothing.
    fn case_layout(&self, layout: LayoutId, index: u32) -> LayoutId {
        match self.case_of(layout, index) {
            Some((parts, _)) => parts.first().map_or(shapes::UNIT, |part| part.layout),
            None => shapes::UNIT,
        }
    }
}

/// The methods the machine performs, by the receiver and operation
/// [`Builtin`] names them with.
///
/// Every one of them is an operation of a value that is one word or is text,
/// and none of them is something an instruction expresses: a `String`'s
/// length is in characters rather than bytes, `Int.abs` at `Int.MIN` stops
/// the run, `Float.toInt` answers a `Result`. The sequence operations are
/// not here — `cove_lir::lower::collections` has its own list, because for a
/// sequence some of them *are* instructions and the split is the interesting
/// part.
///
/// The six `Duration` names are each both a reader and a builder;
/// [`ASSOCIATED`] holds the builders and this holds the readers, and the
/// machine tells them apart by the `Repr` of operand 0.
const MACHINE_METHODS: &[(&str, &str)] = &[
    ("String", "length"),
    ("String", "isEmpty"),
    ("String", "words"),
    ("String", "chars"),
    ("String", "split"),
    ("String", "join"),
    ("String", "slice"),
    ("String", "trim"),
    ("String", "contains"),
    ("String", "startsWith"),
    ("String", "endsWith"),
    ("String", "indexOf"),
    ("String", "replace"),
    ("String", "toUpper"),
    ("String", "toLower"),
    ("Int", "toFloat"),
    ("Int", "abs"),
    ("Int", "min"),
    ("Int", "max"),
    ("Float", "toInt"),
    ("Float", "round"),
    ("Float", "abs"),
    ("Float", "min"),
    ("Float", "max"),
    ("Float", "format"),
    ("Duration", "nanos"),
    ("Duration", "micros"),
    ("Duration", "millis"),
    ("Duration", "seconds"),
    ("Duration", "minutes"),
    ("Duration", "hours"),
];

/// The operations the machine performs that are written on a type's name
/// rather than on a value.
///
/// `Vector.of` is not here: it allocates two objects whose layouts the
/// lowering knows, so it is [`Inst::Alloc`]s rather than a call — see
/// [`Body::vector_of`].
const ASSOCIATED: &[(&str, &str)] = &[
    ("String", "fromCodePoint"),
    ("Int", "parse"),
    ("Int", "parseRadix"),
    ("Float", "parse"),
    ("Duration", "nanos"),
    ("Duration", "micros"),
    ("Duration", "millis"),
    ("Duration", "seconds"),
    ("Duration", "minutes"),
    ("Duration", "hours"),
];

/// Whether `head.name(...)` is one of the machine's associated functions.
///
/// The name in front of the `.` is a namespace rather than a value here, and
/// a module or an enum can be written the same way — so the type the checker
/// settled for the call is asked as well as the name. `Duration.seconds(1)`
/// answers a `Duration`, and each of the three parsers answers the `Result`
/// of the type it is named for; nothing else in the language answers those
/// under those names.
pub(super) fn associated(head: &str, name: &str, ty: &Ty) -> bool {
    if !ASSOCIATED.contains(&(head, name)) {
        return false;
    }
    match head {
        "Duration" => matches!(ty, Ty::Duration),
        "Int" => answers(ty, &Ty::Int),
        "Float" => answers(ty, &Ty::Float),
        "String" => answers(ty, &Ty::Str),
        _ => false,
    }
}

/// Whether `ty` is the `Result<ok, Error>` a builtin parser answers.
fn answers(ty: &Ty, ok: &Ty) -> bool {
    matches!(ty, Ty::Result(value, error) if **value == *ok && matches!(**error, Ty::Error))
}

/// What the language calls the type a method was written on.
///
/// It is the name [`Builtin::receiver`] carries and the name a gap names the
/// work with, and those are one name for one reason: the set of operations
/// is the language reference's, and the reference writes `String.split` and
/// `Array.map`.
///
/// A declared `struct` or `enum` answers `None` rather than its own name.
/// Its methods are not the machine's and never will be — they are lowered
/// functions of the package, reached through
/// [`Facts::target`](cove_sema::Facts::target) — so naming one here would
/// point at the wrong work.
fn receiver_name(ty: &Ty) -> Option<&'static str> {
    Some(match ty {
        Ty::Unit => "Unit",
        Ty::Str => "String",
        Ty::Bool => "Bool",
        Ty::Int => "Int",
        Ty::Float => "Float",
        Ty::Duration => "Duration",
        Ty::Error => "Error",
        Ty::Range => "Range",
        Ty::Array(_) => "Array",
        Ty::Vector(_) => "Vector",
        Ty::Set(_) => "Set",
        Ty::Map(..) => "Map",
        Ty::MapEntry(..) => "MapEntry",
        Ty::Option(_) => "Option",
        Ty::Result(..) => "Result",
        Ty::Task(_) => "Task",
        Ty::Shared(_) => "Shared",
        Ty::Scope => "Scope",
        _ => return None,
    })
}
