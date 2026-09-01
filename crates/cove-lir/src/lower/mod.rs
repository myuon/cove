//! Turning a checked package into the executable IR.
//!
//! This reads the answers `cove-sema` already settled and writes them down
//! as slots and instructions. It re-derives nothing: an expression's type
//! comes from [`Facts::ty`](cove_sema::Facts::ty), a declaration's boundary
//! from [`Facts::signature`](cove_sema::Facts::signature), and where the two
//! could disagree the checker is right by construction because this asked it
//! rather than working it out again.
//!
//! # There is no refusal
//!
//! A valid checked program lowers. That is the whole contract, and it is
//! what [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! replaces the predecessor's admission predicate with. The two ways this
//! can answer `Err` are the two `gap` module builds — an unsettled type and
//! a construct this crate has not been taught yet — and neither is a
//! judgement about the program.
//!
//! # The shape of a lowered body
//!
//! Control flow is flat. There are no basic blocks and no block arguments:
//! an `if` is a [`Inst::BranchFalse`] over a run of instructions, a `while`
//! is a backward [`Inst::Jump`], and both are emitted with an unpatchable
//! target that is filled in once the destination is known. A loop keeps the
//! jumps its `break`s left behind and patches them when it learns where its
//! end is.
//!
//! Every function ends in [`Inst::Return`], and it is emitted
//! unconditionally — even after a body that already returned on every path.
//! That is one dead word, and what it buys is that every patched target
//! lands on an instruction: a branch whose destination is "after everything"
//! has something to be after. Tracking reachability well enough to drop it
//! would mean tracking which pending patches point past the end, which is
//! more machinery than the word is worth.

mod expr;
mod frame;
mod gap;
mod stmt;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use cove_diag::{Diagnostic, Span};
use cove_sema::resolve::{Program as Checked, ResolvedModule};
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Expr, FnDecl};

use crate::inst::{Inst, Pc, Slot};
use crate::layout::{Layout, LayoutId, Shape};
use crate::program::{ArgsId, Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

use frame::{Frame, Val};

/// The target of a jump that has been emitted but whose destination is not
/// known yet.
///
/// `u32::MAX` rather than `0`, so that a patch this lowering forgot is a
/// verifier fault naming the instruction rather than a silent jump to the
/// top of the function.
const PENDING: Pc = Pc::MAX;

/// Lowers a checked package.
///
/// The result either runs or names what stopped it. Nothing in between: a
/// lowered [`Program`] has been through [`crate::verify()`], so a caller that
/// holds one holds a program whose slots, jumps and calls are all in range
/// and whose reference map is the one its reprs imply.
pub fn lower(checked: &Checked) -> Result<Program, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let plan = Plan::build(checked, &mut errors);
    let mut args = Args::default();
    let mut functions = Vec::new();
    for id in 0..plan.decls.len() {
        functions.push(lower_function(checked, &plan, id, &mut args, &mut errors));
    }

    let program = Program {
        functions,
        // Two layouts every program declares whether or not it uses them:
        // `LayoutId(0)` is what the sweeper writes into a reclaimed run of
        // words, and the string layout is what the machine allocates a
        // host's answer as. A scalar-only program names neither.
        layouts: vec![
            Layout::free(),
            Layout {
                name: Arc::from("String"),
                shape: Shape::Str,
            },
        ],
        str_layout: LayoutId(1),
        strings: Vec::new(),
        args: args.lists,
        tables: Vec::new(),
        host_ops: Vec::new(),
        builtins: Vec::new(),
        by_name: plan.by_name,
    };

    if !errors.is_empty() {
        return Err(only_once(errors));
    }

    // A fault here is a bug in this module, not a fault in the user's
    // program: everything the verifier checks is something this lowering
    // decided. Reporting it as a diagnostic would put it in front of the
    // person least able to act on it, so it fails loudly and with the whole
    // list, because one lowering bug usually shows up in several places and
    // seeing all of them is what says which one is the cause.
    if let Err(faults) = crate::verify(&program) {
        let listing: Vec<String> = faults.iter().map(ToString::to_string).collect();
        panic!(
            "the lowering produced a program the verifier rejects:\n  {}",
            listing.join("\n  ")
        );
    }

    Ok(program)
}

/// Keeps the first diagnostic about each place.
///
/// One unsettled type is read once per operand it feeds, so the same
/// expression can be reported several times over. What a reader needs is
/// the place, once.
fn only_once(errors: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = HashSet::new();
    errors
        .into_iter()
        .filter(|item| {
            let at = item.primary.map(|span| (span.file.0, span.start, span.end));
            seen.insert((item.code.clone(), at))
        })
        .collect()
}

// ---------------------------------------------------------------- the plan

/// What one declaration becomes: its identity, and the frame boundary a call
/// to it has to match.
struct Decl<'a> {
    module: Arc<str>,
    name: Arc<str>,
    decl: &'a FnDecl,
    /// `None` for a declaration outside this lowering's scope. A gap has
    /// already been reported for it, so the stub that takes its place is
    /// never seen: `lower` answers `Err` before the program leaves.
    boundary: Option<Boundary>,
}

/// A declaration's parameters and answer, as words.
struct Boundary {
    params: Vec<Repr>,
    returns: Repr,
}

/// Every declaration the package will have a [`Function`] for, numbered.
///
/// The order is module then name, both from a `BTreeMap`, so a package
/// lowers to the same function ids every time it is lowered. A test that
/// pins a listing is pinning something stable rather than a hash order.
struct Plan<'a> {
    decls: Vec<Decl<'a>>,
    by_name: BTreeMap<(Arc<str>, Arc<str>), FunctionId>,
    lookup: HashMap<(String, String), FunctionId>,
}

impl<'a> Plan<'a> {
    fn build(checked: &'a Checked, errors: &mut Vec<Diagnostic>) -> Plan<'a> {
        let mut plan = Plan {
            decls: Vec::new(),
            by_name: BTreeMap::new(),
            lookup: HashMap::new(),
        };
        for (name, resolved) in &checked.modules {
            plan.declare_gaps(resolved, errors);
            for (fn_name, entry) in &resolved.functions {
                let id = FunctionId(plan.decls.len() as u32);
                let module: Arc<str> = Arc::from(name.as_str());
                let fn_name_arc: Arc<str> = Arc::from(fn_name.as_str());
                plan.by_name
                    .insert((module.clone(), fn_name_arc.clone()), id);
                plan.lookup.insert((name.clone(), fn_name.clone()), id);
                plan.decls.push(Decl {
                    module,
                    name: fn_name_arc,
                    decl: entry.decl.as_ref(),
                    boundary: boundary_of(checked, &entry.decl, errors),
                });
            }
        }
        plan
    }

    /// Reports the declarations that are not functions and so have no code
    /// here yet. A type this lowering cannot represent is a gap at every use
    /// of it as well, but naming the declaration once is what says where the
    /// work is.
    fn declare_gaps(&self, resolved: &ResolvedModule, errors: &mut Vec<Diagnostic>) {
        for entry in resolved.structs.values() {
            errors.push(gap::gap("a `struct` declaration", entry.decl.span));
        }
        for entry in resolved.enums.values() {
            errors.push(gap::gap("an `enum` declaration", entry.decl.span));
        }
        for entry in resolved.traits.values() {
            errors.push(gap::gap("a `trait` declaration", entry.decl.span));
        }
        for entry in resolved.methods.values() {
            errors.push(gap::gap("a method or associated function", entry.decl.span));
        }
    }

    /// The declaration `name` denotes where the module `from` can see it.
    ///
    /// A module's own declaration wins over an imported one, which is the
    /// order resolution already established: a `use` that would shadow a
    /// local declaration was refused there.
    fn resolve(&self, checked: &Checked, from: &str, name: &str) -> Option<FunctionId> {
        if let Some(id) = self.lookup.get(&(from.to_string(), name.to_string())) {
            return Some(*id);
        }
        let owner = checked.modules.get(from)?.imports.get(name)?;
        self.lookup.get(&(owner.clone(), name.to_string())).copied()
    }

    /// How many words a call to `id` passes and what kind of word it
    /// answers, as owned values: a call site reads this while it is still
    /// holding the body it is lowering.
    fn boundary(&self, id: FunctionId) -> Option<(usize, Repr)> {
        self.decls[id.index()]
            .boundary
            .as_ref()
            .map(|boundary| (boundary.params.len(), boundary.returns))
    }
}

/// What a call to this declaration passes and answers, read off the
/// checker's signature rather than off the source annotations.
///
/// The annotations are names; the signature is what those names resolved to
/// in the module they were written in, which is the only reading of a
/// `-> other.Thing` that means the same in both modules.
fn boundary_of(checked: &Checked, decl: &FnDecl, errors: &mut Vec<Diagnostic>) -> Option<Boundary> {
    let mut ok = true;
    if !decl.generics.is_empty() {
        errors.push(gap::gap("a generic function", decl.span));
        ok = false;
    }
    if decl.is_async {
        errors.push(gap::gap("an `async fn`", decl.span));
        ok = false;
    }
    for param in &decl.params {
        let what = if param.is_var {
            "a `var` parameter"
        } else if param.variadic {
            "a variadic parameter"
        } else if param.default.is_some() {
            "a parameter with a default"
        } else {
            continue;
        };
        errors.push(gap::gap(what, param.span));
        ok = false;
    }

    let Some(signature) = checked.facts.signature(decl.span.file, decl.span) else {
        errors.push(gap::gap(
            "a declaration the checker recorded no signature for",
            decl.span,
        ));
        return None;
    };

    let mut params = Vec::new();
    for (param, ty) in decl.params.iter().zip(&signature.params) {
        match word_of(ty) {
            Some(repr) => params.push(repr),
            None => {
                errors.push(describe(ty, param.span));
                ok = false;
            }
        }
    }
    let returns = match word_of(&signature.ret) {
        Some(repr) => repr,
        None => {
            let span = decl.return_type.as_ref().map_or(decl.span, |ty| ty.span);
            errors.push(describe(&signature.ret, span));
            ok = false;
            Repr::Unit
        }
    };

    ok.then_some(Boundary { params, returns })
}

/// The one word a value of this type occupies, for the types this task
/// lowers.
///
/// [`Ty::Never`] answers a word too, and it is `Unit`. A value of that type
/// is never produced — the expression left the frame or the loop before it
/// could be — so the slot exists to keep the numbering uniform and nothing
/// ever writes it.
fn word_of(ty: &Ty) -> Option<Repr> {
    match ty {
        Ty::Unit | Ty::Never => Some(Repr::Unit),
        Ty::Bool => Some(Repr::Bool),
        Ty::Int => Some(Repr::Int),
        Ty::Float => Some(Repr::Float),
        Ty::Duration => Some(Repr::Duration),
        _ => None,
    }
}

/// Why a type has no word here: the checker settled nothing, or it settled
/// something this task has not reached.
fn describe(ty: &Ty, span: Span) -> Diagnostic {
    match ty {
        Ty::Unknown(_) => gap::unknown(ty, span),
        _ => gap::gap(&format!("a value of type `{ty}`"), span),
    }
}

// ------------------------------------------------------------ interning

/// The program-wide pool of argument lists.
///
/// A call's arguments are a static list of source slots, and a repeated call
/// shape is one list rather than one per site — which is the whole reason
/// [`Inst::Call`] names an [`ArgsId`] instead of carrying the list inline.
#[derive(Default)]
struct Args {
    lists: Vec<Vec<Slot>>,
    index: HashMap<Vec<Slot>, ArgsId>,
}

impl Args {
    fn intern(&mut self, slots: Vec<Slot>) -> ArgsId {
        if let Some(id) = self.index.get(&slots) {
            return *id;
        }
        let id = ArgsId(self.lists.len() as u32);
        self.index.insert(slots.clone(), id);
        self.lists.push(slots);
        id
    }
}

// --------------------------------------------------------------- one body

/// A loop being lowered, and what it owes its `break`s.
struct Loop {
    /// Where `continue` goes: the condition, so the next turn is decided
    /// again rather than assumed.
    head: Pc,
    /// How many scopes were open outside the body, so an early exit knows
    /// which ones it is leaving.
    depth: usize,
    /// Jumps emitted by `break` with nowhere to go yet.
    breaks: Vec<Pc>,
}

/// The state of lowering one function body.
struct Body<'a> {
    checked: &'a Checked,
    plan: &'a Plan<'a>,
    args: &'a mut Args,
    errors: &'a mut Vec<Diagnostic>,
    /// The module the body is written in, which is what an unqualified name
    /// in it is resolved against.
    module: &'a str,
    frame: Frame,
    code: Vec<Inst>,
    spans: Vec<Span>,
    loops: Vec<Loop>,
    /// The slot the body's answer is assembled in, and the one the trailing
    /// [`Inst::Return`] names.
    answer: Slot,
}

fn lower_function(
    checked: &Checked,
    plan: &Plan,
    id: usize,
    args: &mut Args,
    errors: &mut Vec<Diagnostic>,
) -> Function {
    let decl = &plan.decls[id];
    let Some(boundary) = &decl.boundary else {
        return stub(decl);
    };

    let mut frame = Frame::new();
    for repr in &boundary.params {
        frame.param(*repr);
    }
    // The answer word is taken before any temporary, so it is live for the
    // whole body and never handed to something else.
    let answer = frame.alloc(boundary.returns);

    let mut body = Body {
        checked,
        plan,
        args,
        errors,
        module: &decl.module,
        frame,
        code: Vec::new(),
        spans: Vec::new(),
        loops: Vec::new(),
        answer,
    };

    body.frame.push_scope();
    for (index, param) in decl.decl.params.iter().enumerate() {
        body.frame.bind(&param.name.node, index as Slot);
    }
    body.block(&decl.decl.body, Some(answer));
    let clears = body.frame.pop_scope();
    body.clear(&clears, decl.decl.body.span);
    body.emit(Inst::Return { src: answer }, decl.decl.body.span);

    let reprs = body.frame.reprs().to_vec();
    Function {
        module: decl.module.clone(),
        name: decl.name.clone(),
        arity: boundary.params.len() as u32,
        refs: RefMap::of(&reprs),
        reprs,
        returns: boundary.returns,
        captures: Vec::new(),
        code: body.code,
        spans: body.spans,
        span: decl.decl.span,
        is_async: decl.decl.is_async,
    }
}

/// What stands in for a declaration this lowering reported a gap about.
///
/// It exists so that function ids stay dense and a call site that names one
/// still has something to name. Nothing ever runs it: a gap is an error, and
/// `lower` answers `Err` rather than handing the program back.
fn stub(decl: &Decl) -> Function {
    let reprs = vec![Repr::Unit];
    Function {
        module: decl.module.clone(),
        name: decl.name.clone(),
        arity: 0,
        refs: RefMap::of(&reprs),
        reprs,
        returns: Repr::Unit,
        captures: Vec::new(),
        code: vec![Inst::Return { src: 0 }],
        spans: vec![decl.decl.span],
        span: decl.decl.span,
        is_async: false,
    }
}

impl Body<'_> {
    // ---- emitting -------------------------------------------------------

    /// Appends one instruction and the span it came from, answering where it
    /// landed so a jump can be patched later.
    fn emit(&mut self, inst: Inst, span: Span) -> Pc {
        let at = self.here();
        self.code.push(inst);
        self.spans.push(span);
        at
    }

    /// Where the next instruction will land, which is what a forward jump is
    /// patched to.
    fn here(&self) -> Pc {
        self.code.len() as Pc
    }

    fn patch(&mut self, at: Pc, to: Pc) {
        match &mut self.code[at as usize] {
            Inst::Jump { to: target } | Inst::BranchFalse { to: target, .. } => *target = to,
            other => unreachable!("patched a {other:?}, which is not a jump"),
        }
    }

    /// Ends the live range of the reference slots a scope owned.
    ///
    /// Empty today, and deliberately still here: no slot in a scalar-only
    /// body is `Ref` or `Addr`, so [`Frame::pop_scope`] answers an empty
    /// list and nothing is emitted. What the mechanism is for is the moment
    /// the heap arrives, when the alternative — a static reference map with
    /// no way to say a value died — retains every object a frame ever
    /// touched until the frame returns.
    fn clear(&mut self, slots: &[Slot], span: Span) {
        for slot in slots {
            self.emit(Inst::Clear { slot: *slot }, span);
        }
    }

    // ---- reading the checker's answers -----------------------------------

    /// The type the checker settled for `expr`.
    fn ty(&self, expr: &Expr) -> Option<&Ty> {
        self.checked.facts.ty(expr.span.file, expr.id)
    }

    /// The word `expr`'s value occupies, reporting the reason there is none.
    ///
    /// A reported failure answers `Unit` so that lowering can carry on and
    /// find the rest of what is wrong in the same run. The answer is never
    /// acted on: `lower` has an error and will not hand the program back.
    fn word(&mut self, expr: &Expr) -> Repr {
        let Some(ty) = self.ty(expr).cloned() else {
            self.errors.push(gap::gap(
                "an expression the checker recorded no type for",
                expr.span,
            ));
            return Repr::Unit;
        };
        match word_of(&ty) {
            Some(repr) => repr,
            None => {
                self.errors.push(describe(&ty, expr.span));
                Repr::Unit
            }
        }
    }

    /// Whether this expression leaves rather than answering: a `return`, a
    /// `break`, a `continue`, or a form built out of them.
    ///
    /// What it decides is whether the surrounding form copies the answer.
    /// Nothing is ever written to a diverging expression's slot, so copying
    /// from it would move a word that was never produced — and where the
    /// surrounding form wants a different `Repr`, it would not even be well
    /// typed.
    fn diverges(&self, expr: &Expr) -> bool {
        matches!(self.ty(expr), Some(Ty::Never))
    }

    /// Copies an expression's answer into the slot the surrounding form is
    /// assembling its own in.
    fn store(&mut self, dst: Slot, value: &Val, from: &Expr) {
        if self.diverges(from) || value.slot == dst {
            return;
        }
        self.emit(
            Inst::Move {
                dst,
                src: value.slot,
            },
            from.span,
        );
    }

    /// A slot for a value nothing will produce, so that a diverging
    /// expression still answers something the caller can hold.
    fn dead(&mut self, expr: &Expr) -> Val {
        let repr = self.word(expr);
        Val::temp(self.frame.alloc(repr))
    }

    /// Reports a construct this lowering has not been taught, answering a
    /// slot of the right kind so the walk can continue and report the rest.
    fn gap(&mut self, what: &str, expr: &Expr) -> Val {
        self.errors.push(gap::gap(what, expr.span));
        self.dead(expr)
    }
}
