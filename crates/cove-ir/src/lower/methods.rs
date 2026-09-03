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
//! `cove_runtime::vm::builtins` dispatches on the pair
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
//! it is a branch and one call. `cove_ir::lower::walks` is where the four
//! that *are* walks live.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, RANGE_END, RANGE_INCLUSIVE, RANGE_START};
use super::{Body, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Num, Slot};
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
        // `snapshot()` is the one method of the builtin `Snapshot` trait, and
        // it is asked of *every* type rather than of one — so it is answered
        // before the receiver's own table is consulted, exactly as
        // `Interpreter::call_method` answers it after a declared conformance
        // and before anything a builtin receiver has. A struct or an enum
        // that wrote `impl Snapshot for Type` never arrives here at all:
        // `Facts::target` named its declaration and `Body::call_through` read
        // that first.
        if name == "snapshot" && args.is_empty() {
            return self.snapshot(expr, base, &ty);
        }
        match &ty {
            Ty::Range => self.range_method(expr, base, name, args),
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
            // A scope and a task handle are the two values whose operations
            // are the scheduler's rather than the heap's, so they are
            // instructions rather than builtins: `cove_ir::lower::tasks` is
            // where the Language Card's sentence about a scope is taken
            // apart.
            Ty::Scope => self.scope_method(expr, base, name, args),
            Ty::Task(_) => self.task_method(expr, base, name, args),
            // A `Shared` is the third: `lock` is two instructions and a call
            // between them rather than a builtin, because a builtin never
            // calls back into Cove. `cove_ir::lower::cells` is where it and
            // the cell's constructor live.
            Ty::Shared(_) => self.shared_method(expr, base, name, args),
            // A host resource's operations belong to the host that issued
            // the handle, and the handle is what routes them:
            // `HostRegistry::call_resource` reads the module and the resource
            // kind off it rather than off the call site. So this is not a
            // builtin at all — it is the boundary, addressed the other way —
            // and `Body::call_resource` is where it is lowered, beside the
            // host call it shares a boundary with.
            Ty::Host(qualified) => {
                let qualified = qualified.to_string();
                self.call_resource(expr, base, &qualified, name, args)
            }
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
    /// `cove_runtime::vm::builtins` finds a family by searching the
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

    // ---- `Snapshot` --------------------------------------------------------

    /// `value.snapshot()`: the independent copy the builtin `Snapshot` trait
    /// makes.
    ///
    /// `crates/cove-runtime/src/builtins.rs`'s `snapshot` is the rule, and it
    /// has exactly two answers for a value no declared conformance speaks
    /// for. **An immutable value answers itself**, because a copy of it is
    /// not observable — and that is every scalar, a `String`, a `Range`, and
    /// an `Array`, a `Map` and a `Set` *whatever they hold*, which the oracle
    /// says in as many words: "each is immutable, so an element that shares
    /// storage with something else went on sharing it before this was called
    /// and there is nothing for a copy to separate". **A `Vector` answers a
    /// new vector**, because its storage is the one storage a copy is
    /// observable of.
    ///
    /// So the first answer is [`Body::copy`] — ADR 0001's field-wise shallow
    /// copy is what `value.clone()` is here — into a location of this
    /// expression's own, rather than the receiver's location handed back. A
    /// borrowed location would be an alias of the binding, and
    /// `f(a.snapshot(), g())` would then hand the call whatever `g` left in
    /// `a`.
    ///
    /// There is no `("Any", "snapshot")` arm in
    /// `cove_runtime::vm::builtins` and this does not want one. A copy is
    /// instructions the machine already has, and the recursion the second
    /// answer needs is a walk that may call a conformance — which
    /// `docs/LINEAR_VM.md` puts in the lowering rather than in a builtin,
    /// because a builtin never calls back into Cove.
    fn snapshot(&mut self, expr: &Expr, base: &Expr, ty: &Ty) -> Val {
        match ty {
            _ if snapshots_itself(ty) => {
                let value = self.expr(base);
                let dst = self.temp(value.layout);
                self.copy(dst.slot, value.slot, value.layout, expr.span);
                self.release(value, expr.span);
                dst
            }
            // A vector of values that each answer themselves is a vector of
            // words, so the independent copy is the words in a store of their
            // own: `Vector.toArray` clones them out and `Array.toVector`
            // allocates the new vector around them. That is the oracle's
            // `allocate_vector(snapshotted)` for the case where snapshotting
            // an element is the identity, and it costs one intermediate
            // object rather than an instruction the machine does not have.
            Ty::Vector(elem) if snapshots_itself(elem) => {
                let elem = (**elem).clone();
                let value = self.expr(base);
                let Some(array) = self.vector_snapshot(&value, &elem, expr.span) else {
                    self.release(value, expr.span);
                    return self.dead(expr);
                };
                self.release(value, expr.span);
                let layout = self.layout_of(expr);
                let dst = self.temp(layout);
                self.emit_builtin(
                    dst.slot,
                    "Array",
                    "toVector",
                    &[array.arg()],
                    layout,
                    expr.span,
                );
                self.release(array, expr.span);
                dst
            }
            // An element that has a graph of its own has to be snapshotted
            // one at a time, and one of the ways it answers is a call to the
            // conformance its type declared. That is a walk in this lowering
            // rather than a builtin, for the reason above; nothing in the
            // corpus writes one yet, so the work is named rather than
            // guessed at.
            Ty::Vector(elem) => self.gap(
                &format!(
                    "`Vector.snapshot` of a `{elem}`, whose elements each need a snapshot of \
                     their own"
                ),
                expr,
            ),
            // Everything the oracle refuses at run time — a closure, a task,
            // a scope, a `Shared`, a Host resource — and everything it
            // answers by dispatching to a conformance that is not there. A
            // gap rather than a `Trap`, because what a program that reaches
            // one earns is the oracle's own refusal and this lowering has not
            // been taught to raise it.
            _ => self.gap(&format!("`snapshot` on a value of type `{ty}`"), expr),
        }
    }

    // ---- `Range` -----------------------------------------------------------

    /// A method of a `Range`, which is arithmetic rather than a builtin.
    ///
    /// A `Range` is three inline words — `start`, `end`, and whether the end
    /// is one the range yields — so every question about one is a comparison
    /// of words already in the frame. `cove_runtime::vm::builtins` has no
    /// `Range` arm and does not need one, for the reason `Option` and
    /// `Result` have none: what a builtin would be handed is what the
    /// instruction set already reads.
    ///
    /// `RangeBounds` in `cove_runtime::value` is the oracle, and it
    /// normalises `..` into `..<` by adding one to the end. That addition is
    /// **not** made here, exactly as `Body::for_range` does not make it: the
    /// end is `i64::MAX` in a program that writes `0..Int.MAX`, the oracle
    /// widens to `i128` before adding, and this machine's `Int` addition
    /// stops the run on an overflow. So each of the three asks the question
    /// the written end already answers, and the `inclusive` word is what
    /// chooses the comparison.
    fn range_method(&mut self, expr: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Val {
        match (name, args.len()) {
            ("isEmpty", 0) => {
                let range = self.expr(base);
                let dst = self.temp(shapes::BOOL);
                self.range_is_empty(range.slot, dst.slot, expr.span);
                self.release(range, expr.span);
                dst
            }
            ("length", 0) => self.range_length(expr, base),
            ("contains", 1) => self.range_contains(expr, base, &args[0].value),
            _ => self.gap(&format!("`Range.{name}`"), expr),
        }
    }

    /// `dst = range.isEmpty()`, over a range already in the frame.
    ///
    /// `RangeBounds::is_empty` is `end <= start` once the end has been
    /// normalised, and the normalisation is the `+ 1` an inclusive range
    /// earns. Asking it of the written end instead is one comparison either
    /// way: an inclusive range yields nothing when its end is *below* its
    /// start, and an exclusive one when the two are equal as well.
    fn range_is_empty(&mut self, range: Slot, dst: Slot, span: Span) {
        let exclusive = self.emit(
            Inst::BranchFalse {
                cond: range + RANGE_INCLUSIVE,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst,
                a: range + RANGE_END,
                b: range + RANGE_START,
            },
            span,
        );
        let done = self.emit(Inst::Jump { to: PENDING }, span);
        let otherwise = self.here();
        self.patch(exclusive, otherwise);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst,
                a: range + RANGE_END,
                b: range + RANGE_START,
            },
            span,
        );
        let end = self.here();
        self.patch(done, end);
    }

    /// `range.length()`: how many values the range yields.
    ///
    /// `RangeBounds::len` is `(end - start).max(0)`, and the emptiness test
    /// is what stands in for the `max`: a range that yields nothing answers
    /// zero without subtracting anything, and one that yields something has
    /// an end at or past its start, so the subtraction is of two words the
    /// larger of which is first.
    fn range_length(&mut self, expr: &Expr, base: &Expr) -> Val {
        let span = expr.span;
        let range = self.expr(base);
        let dst = self.temp(shapes::INT);
        let empty = self.temp(shapes::BOOL);
        self.range_is_empty(range.slot, empty.slot, span);
        let counted = self.emit(
            Inst::BranchFalse {
                cond: empty.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Int {
                dst: dst.slot,
                value: 0,
            },
            span,
        );
        let done = self.emit(Inst::Jump { to: PENDING }, span);
        let otherwise = self.here();
        self.patch(counted, otherwise);
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Sub,
                dst: dst.slot,
                a: range.slot + RANGE_END,
                b: range.slot + RANGE_START,
            },
            span,
        );
        // The end of an inclusive range is a value the range yields, so it
        // is one more than the difference says.
        let exclusive = self.emit(
            Inst::BranchFalse {
                cond: range.slot + RANGE_INCLUSIVE,
                to: PENDING,
            },
            span,
        );
        let one = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: one.slot,
                value: 1,
            },
            span,
        );
        self.add(dst.slot, dst.slot, one.slot, span);
        self.give_back(one.slot, one.layout);
        let end = self.here();
        self.patch(done, end);
        self.patch(exclusive, end);
        self.give_back(empty.slot, empty.layout);
        self.release(range, span);
        dst
    }

    /// `range.contains(value)`: whether the range yields it.
    ///
    /// `RangeBounds::contains` is `start <= value && value < end`, with the
    /// end normalised. Un-normalised it is `start <= value` and then the
    /// comparison the `inclusive` word chooses, and the first of the two is
    /// what a false answer leaves in the destination — a range starting past
    /// the value never reaches the second question.
    ///
    /// The argument is evaluated after the receiver and before either
    /// comparison, because it is an ordinary argument: the language
    /// evaluates a call's arguments before the call, and the oracle's
    /// `expect_args` receives it already evaluated.
    fn range_contains(&mut self, expr: &Expr, base: &Expr, value: &Expr) -> Val {
        let span = expr.span;
        let range = self.expr(base);
        let held = self.expr(value);
        let dst = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst: dst.slot,
                a: range.slot + RANGE_START,
                b: held.slot,
            },
            span,
        );
        let below = self.emit(
            Inst::BranchFalse {
                cond: dst.slot,
                to: PENDING,
            },
            span,
        );
        let exclusive = self.emit(
            Inst::BranchFalse {
                cond: range.slot + RANGE_INCLUSIVE,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst: dst.slot,
                a: held.slot,
                b: range.slot + RANGE_END,
            },
            span,
        );
        let done = self.emit(Inst::Jump { to: PENDING }, span);
        let otherwise = self.here();
        self.patch(exclusive, otherwise);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: dst.slot,
                a: held.slot,
                b: range.slot + RANGE_END,
            },
            span,
        );
        let end = self.here();
        self.patch(below, end);
        self.patch(done, end);
        self.release(held, span);
        self.release(range, span);
        dst
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

/// Whether `snapshot()` on a value of this type answers the value itself.
///
/// The list is `cove_runtime::builtins::snapshot`'s own first arm, and it is
/// a list rather than "everything that is not a `Vector`" for the reason that
/// function gives: what decides it is whether the value has a mutable graph
/// of its own, and an `Array`, a `Map` and a `Set` do not have one *whatever
/// they hold* — an element of one that shares storage with something else was
/// sharing it before the call and there is nothing for a copy to separate.
///
/// A struct, an enum and a `dyn` are not here and are not "false" either:
/// each answers through a conformance, which is a call rather than a copy,
/// and a call is not what this question is asked in order to emit.
fn snapshots_itself(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Unit
            | Ty::Bool
            | Ty::Int
            | Ty::Float
            | Ty::Duration
            | Ty::Str
            | Ty::Range
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Set(_)
    )
}

/// The methods the machine performs, by the receiver and operation
/// [`Builtin`] names them with.
///
/// Every one of them is an operation of a value that is one word or is text,
/// and none of them is something an instruction expresses: a `String`'s
/// length is in characters rather than bytes, `Int.abs` at `Int.MIN` stops
/// the run, `Float.toInt` answers a `Result`. The sequence operations are
/// not here — `cove_ir::lower::collections` has its own list, because for a
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
