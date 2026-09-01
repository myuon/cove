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
//! `mapError` is not, because it takes a closure: `docs/LINEAR_VM.md` says a
//! closure-taking method lowers to a loop in the IR rather than to a builtin
//! that calls back, and a call through a function value is a gap of its own.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, word_of};
use super::{Body, PENDING};
use crate::inst::{CmpOp, Compare, Inst, Slot};
use crate::program::Builtin;
use crate::repr::Repr;

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
        let Some(ty) = self.owned_ty(base) else {
            return self.dead(expr);
        };
        if let Some(bad) = self.plain_arguments(args) {
            return self.gap(bad, expr);
        }
        // Meeting a value of the type is what declares its layout, and a
        // method call is meeting one. It matters for a vector: the machine
        // finds the store to grow into by looking the family up in this
        // table, and a program that only ever receives a vector from
        // somewhere else would otherwise declare no store for it.
        if matches!(ty, Ty::Array(_) | Ty::Vector(_)) && self.layout(&ty, base.span).is_none() {
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
            Ty::Option(_) | Ty::Result(..) => self.answer_method(expr, base, &ty, name, args),
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
    /// operation in the table has. The result is the word the checker
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
        let Some(ty) = self.owned_ty(expr) else {
            return self.dead(expr);
        };
        let Some(result) = word_of(&ty) else {
            self.errors.push(super::describe(&ty, expr.span));
            return self.dead(expr);
        };
        if !self.answer_layouts(&ty, result, expr.span) {
            return self.dead(expr);
        }

        let held_receiver = base.map(|base| self.expr(base));
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let mut slots = Vec::with_capacity(args.len() + 1);
        slots.extend(held_receiver.iter().map(|value| value.slot));
        slots.extend(held.iter().map(|value| value.slot));

        let dst = self.frame.alloc(result);
        self.emit_builtin(dst, receiver, operation, &slots, result, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        // The receiver is a reference wherever the type is an object, and it
        // dies with the call: nothing after the answer is written reads it.
        if let Some(value) = held_receiver {
            self.release(value, expr.span);
        }
        Val::temp(dst)
    }

    /// Interns the families the machine will look for while it builds this
    /// call's answer.
    ///
    /// `cove_runtime::lvm::builtins::make` finds a family by searching the
    /// program's layout table, so a family the program never otherwise
    /// mentions is a refusal at run time rather than a missing instruction.
    /// Two of them are needed here and only one is obvious:
    ///
    /// - the answer's own family — an `Option<Int>` for `indexOf`, an
    ///   `Array<String>` for `split`; and
    /// - the builtin `Error`, when the answer is a `Result`. The machine
    ///   builds the `Error` carrying a failure's message *itself*, and the
    ///   `Result` layout describes its `Err` word as a reference without
    ///   saying what is behind it — so interning the `Result` alone would
    ///   leave `Int.parse("x")` with nowhere to put the message.
    fn answer_layouts(&mut self, ty: &Ty, result: Repr, span: Span) -> bool {
        if result == Repr::Ref && self.layout(ty, span).is_none() {
            return false;
        }
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
        args: &[Slot],
        result: Repr,
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
    pub(super) fn plain_arguments(&self, args: &[Arg]) -> Option<&'static str> {
        args.iter().find_map(|arg| {
            if arg.label.is_some() {
                Some("a labelled argument to a builtin method")
            } else if arg.is_var {
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
    /// questions are about the object's case index, which is word 0, and the
    /// instruction set already reads it. See the module docs.
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
            _ => self.gap(&format!("`{receiver}.{name}`"), expr),
        }
    }

    /// Whether the object is in the case `case`.
    ///
    /// The receiver dies at the [`Inst::GetWord`] that reads its case index:
    /// the answer is an `Int` from that instruction onwards, and holding the
    /// object past it would retain whatever its payload names.
    fn case_test(&mut self, expr: &Expr, base: &Expr, ty: &Ty, case: &str) -> Val {
        let Some((index, _)) = shapes::case_at(self.checked, self.module, ty, case) else {
            self.errors.push(super::describe(ty, expr.span));
            return self.dead(expr);
        };
        let obj = self.expr(base);
        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: tag,
                obj: obj.slot,
                at: 0,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        let wanted = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: wanted,
                value: index as i64,
            },
            expr.span,
        );
        let dst = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst,
                a: tag,
                b: wanted,
            },
            expr.span,
        );
        self.frame.free(wanted);
        self.frame.free(tag);
        Val::temp(dst)
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
            self.errors.push(super::describe(ty, expr.span));
            return self.dead(expr);
        };
        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        let obj = self.expr(base);
        let other = self.expr(fallback);

        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: tag,
                obj: obj.slot,
                at: 0,
            },
            expr.span,
        );
        let wanted = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: wanted,
                value: index as i64,
            },
            expr.span,
        );
        let carries = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: carries,
                a: tag,
                b: wanted,
            },
            expr.span,
        );
        self.frame.free(wanted);
        self.frame.free(tag);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: carries,
                to: PENDING,
            },
            expr.span,
        );
        self.frame.free(carries);

        self.emit(
            Inst::GetWord {
                dst,
                obj: obj.slot,
                at: 1,
            },
            expr.span,
        );
        let carry_on = self.emit(Inst::Jump { to: PENDING }, expr.span);
        let otherwise = self.here();
        self.patch(branch, otherwise);
        self.emit(
            Inst::Move {
                dst,
                src: other.slot,
            },
            expr.span,
        );
        let end = self.here();
        self.patch(carry_on, end);

        self.release(other, expr.span);
        self.release(obj, expr.span);
        Val::temp(dst)
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
