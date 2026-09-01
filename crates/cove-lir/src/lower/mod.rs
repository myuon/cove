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
//! # A value is a run of words, and the lowering is what knows how many
//!
//! `docs/LINEAR_VM.md` puts the fields of a struct where the value is, so
//! `l.from.x` is a slot number this module computes and not an instruction
//! the machine runs. A field access on an inline value is *arithmetic on a
//! slot*, and only a field of a heap object is a load. That is why
//! `Val` is a base slot and a layout rather than a slot and a `Repr`: the
//! layout is what a copy's width, a location's reference words and a field's
//! offset are all read off.
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

mod closures;
mod collections;
mod dispatch;
mod expr;
mod frame;
mod gap;
mod methods;
mod pattern;
mod shapes;
mod stmt;
mod walks;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use cove_diag::{Diagnostic, Span};
use cove_sema::facts::MethodTarget;
use cove_sema::resolve::{Program as Checked, ResolvedModule};
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Expr, FnDecl};

use crate::inst::{Inst, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::{
    Arg, ArgsId, Builtin, BuiltinId, Function, FunctionId, HostOp, HostOpId, Program, StrId, Table,
    TableId,
};
use crate::repr::{RefMap, Repr};

use frame::{Frame, Val};
use shapes::Shapes;

/// The target of a jump that has been emitted but whose destination is not
/// known yet.
///
/// `u32::MAX` rather than `0`, so that a patch this lowering forgot is a
/// verifier fault naming the instruction rather than a silent jump to the
/// top of the function.
const PENDING: Pc = Pc::MAX;

/// Where a form assembles its answer: a base slot and the layout that says
/// how wide it is.
///
/// A destination is a *location*, not a slot, because a value may be several
/// words and both a branch join and a block tail have to write all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dest {
    pub slot: Slot,
    pub layout: LayoutId,
}

impl Dest {
    fn of(val: &Val) -> Dest {
        Dest {
            slot: val.slot,
            layout: val.layout,
        }
    }
}

/// Lowers a checked package.
///
/// The result either runs or names what stopped it. Nothing in between: a
/// lowered [`Program`] has been through [`crate::verify()`], so a caller that
/// holds one holds a program whose locations, jumps and calls are all in
/// range and whose reference map is the one its reprs imply.
pub fn lower(checked: &Checked) -> Result<Program, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let mut pool = Pool::new();
    let plan = Plan::build(checked, &mut pool, &mut errors);
    let mut functions = Vec::new();
    for id in 0..plan.decls.len() {
        functions.push(lower_function(checked, &plan, id, &mut pool, &mut errors));
    }
    // A lambda is a `Function` of its own, numbered after every declaration
    // and discovered while a body is being walked rather than by the plan. A
    // nested lambda is appended while its enclosing one is being lowered, so
    // this list is complete only once the loop above has finished — which is
    // why it is drained here and not built beside `plan.decls`.
    functions.extend(
        pool.lambdas
            .drain(..)
            .map(|held| held.expect("every reserved lambda was lowered into its own slot")),
    );

    let program = Program {
        functions,
        layouts: pool.shapes.into_table(),
        str_layout: shapes::STR,
        boxed_layout: shapes::BOXED,
        strings: pool.strings,
        args: pool.args.lists,
        tables: pool.tables,
        host_ops: pool.host_ops,
        builtins: pool.builtins,
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

/// A declaration's parameters and answer, as layouts.
struct Boundary {
    /// The layout of each parameter, **receiver first** where the
    /// declaration has one. There are no type groups and nothing is
    /// permuted; a method's receiver is first because it is the first thing
    /// a call supplies. A `var` parameter's layout is
    /// [`shapes::ADDR`]: it names the caller's storage rather than holding a
    /// value.
    params: Vec<LayoutId>,
    /// The types those locations hold, in the same order.
    ///
    /// The layout alone is not enough at a call site. A parameter written
    /// `dyn Trait` and one written `String` are both one [`Repr::Ref`] word,
    /// and only the first is a place where a concrete value is erased — so
    /// the call site has to read the type the checker settled, not the
    /// layout this lowering derived from it.
    types: Vec<Ty>,
    returns: LayoutId,
    /// What the declaration answers, as the checker settled it.
    ///
    /// The layout is not enough for `?`, which has to build the enclosing
    /// function's own `Err` or `None` and therefore needs to know which of
    /// the two the answer is.
    ret: Ty,
    /// Whether the first parameter is the receiver: `self`, or the address
    /// `var self` names.
    receiver: bool,
}

/// Every declaration the package will have a [`Function`] for, numbered.
///
/// The order is module then name, both from a `BTreeMap`, so a package
/// lowers to the same function ids every time it is lowered. A test that
/// pins a listing is pinning something stable rather than a hash order.
/// Within a module the free functions come first and the methods follow, so
/// adding a method to a type does not renumber a package's functions.
struct Plan<'a> {
    decls: Vec<Decl<'a>>,
    by_name: BTreeMap<(Arc<str>, Arc<str>), FunctionId>,
    lookup: HashMap<(String, String), FunctionId>,
    /// A method, keyed the way [`MethodTarget`] names one: the module that
    /// declares the *type*, the type, and the method.
    ///
    /// That is not always the module the `impl` block is written in — ADR
    /// 0006's orphan rule lets a conformance be written where the trait is —
    /// so this is keyed by what a call site holds rather than by where the
    /// code ended up.
    methods: HashMap<(String, String, String), FunctionId>,
}

impl<'a> Plan<'a> {
    fn build(checked: &'a Checked, pool: &mut Pool, errors: &mut Vec<Diagnostic>) -> Plan<'a> {
        let mut plan = Plan {
            decls: Vec::new(),
            by_name: BTreeMap::new(),
            lookup: HashMap::new(),
            methods: HashMap::new(),
        };
        for (name, resolved) in &checked.modules {
            plan.declare_gaps(resolved, errors);
            for (fn_name, entry) in &resolved.functions {
                let module: Arc<str> = Arc::from(name.as_str());
                let boundary = boundary_of(checked, name, &entry.decl, pool, errors);
                let id = plan.declare(
                    module,
                    Arc::from(fn_name.as_str()),
                    entry.decl.as_ref(),
                    boundary,
                );
                plan.lookup.insert((name.clone(), fn_name.clone()), id);
            }
            for ((type_name, method), entry) in &resolved.methods {
                let module: Arc<str> = Arc::from(name.as_str());
                // A method is named `Type.method` in the module whose `impl`
                // block writes it. A type and a free function of one name
                // cannot both be declared in a module, and a `.` is not a
                // name character, so the two namings cannot collide and
                // `m.Point.scaled` reads in a diagnostic as it is written.
                let lowered: Arc<str> = Arc::from(format!("{type_name}.{method}"));
                let boundary = match &entry.from_trait_default {
                    // A default body belongs to the trait, and the checker
                    // checks it once there rather than once per conformance
                    // — so there is no per-type declaration to read a
                    // boundary off, and the trait method has none recorded
                    // either. See `Plan::declare_gaps`.
                    Some(trait_name) => {
                        errors.push(gap::gap(
                            &format!("`{trait_name}.{method}`, a trait method's default body"),
                            entry.decl.span,
                        ));
                        None
                    }
                    None => boundary_of(checked, name, &entry.decl, pool, errors),
                };
                let id = plan.declare(module, lowered, entry.decl.as_ref(), boundary);
                let owner = resolved.owner_of(type_name).unwrap_or(name.as_str());
                plan.methods
                    .insert((owner.to_string(), type_name.clone(), method.clone()), id);
            }
        }
        plan
    }

    /// Numbers one declaration and records the name it answers to.
    fn declare(
        &mut self,
        module: Arc<str>,
        name: Arc<str>,
        decl: &'a FnDecl,
        boundary: Option<Boundary>,
    ) -> FunctionId {
        let id = FunctionId(self.decls.len() as u32);
        self.by_name.insert((module.clone(), name.clone()), id);
        self.decls.push(Decl {
            module,
            name,
            decl,
            boundary,
        });
        id
    }

    /// Reports the declarations that have no code here yet.
    ///
    /// A `struct` and an `enum` are not among them: they declare a
    /// [`crate::Layout`] rather than a function, and the layout is built
    /// where a value of the type is met. Neither is a `trait` or an `impl`
    /// block: a trait declares an interface, and a method is an ordinary
    /// lowered function whose first parameter is the receiver.
    ///
    /// What is still reported is the declaration whose *layout* this
    /// lowering cannot build at all — a generic one, whose fields are type
    /// parameters and so have no words. A type this lowering cannot
    /// represent is a gap at every use of it as well, but naming the
    /// declaration once is what says where the work is.
    fn declare_gaps(&self, resolved: &ResolvedModule, errors: &mut Vec<Diagnostic>) {
        for entry in resolved.structs.values() {
            if !entry.decl.generics.is_empty() {
                errors.push(gap::gap("a generic `struct` declaration", entry.decl.span));
            }
        }
        for entry in resolved.enums.values() {
            if !entry.decl.generics.is_empty() {
                errors.push(gap::gap("a generic `enum` declaration", entry.decl.span));
            }
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

    /// The function a [`MethodTarget`] names.
    fn method(&self, target: &MethodTarget) -> Option<FunctionId> {
        self.methods
            .get(&(
                target.module.clone(),
                target.type_name.clone(),
                target.method.clone(),
            ))
            .copied()
    }

    /// The function `type_module.type_name` answers `method` with.
    fn method_of(&self, type_module: &str, type_name: &str, method: &str) -> Option<FunctionId> {
        self.methods
            .get(&(
                type_module.to_string(),
                type_name.to_string(),
                method.to_string(),
            ))
            .copied()
    }

    /// What a call to `id` passes and answers, as owned values: a call site
    /// reads this while it is still holding the body it is lowering.
    fn shape(&self, id: FunctionId) -> Option<CallShape> {
        let boundary = self.decls[id.index()].boundary.as_ref()?;
        Some(CallShape {
            params: boundary.params.clone(),
            types: boundary.types.clone(),
            returns: boundary.returns,
            receiver: boundary.receiver,
        })
    }
}

/// What one call site has to match, held apart from the [`Plan`] so that a
/// body can read it while it is writing into its own frame.
struct CallShape {
    params: Vec<LayoutId>,
    types: Vec<Ty>,
    returns: LayoutId,
    receiver: bool,
}

impl CallShape {
    /// How many arguments the call site writes, which is every parameter but
    /// the receiver.
    fn arity(&self) -> usize {
        self.params.len() - usize::from(self.receiver)
    }
}

/// What a call to this declaration passes and answers, read off the
/// checker's signature rather than off the source annotations.
///
/// The annotations are names; the signature is what those names resolved to
/// in the module they were written in, which is the only reading of a
/// `-> other.Thing` that means the same in both modules.
fn boundary_of(
    checked: &Checked,
    module: &str,
    decl: &FnDecl,
    pool: &mut Pool,
    errors: &mut Vec<Diagnostic>,
) -> Option<Boundary> {
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
        let what = if param.variadic {
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
    let mut types = Vec::new();
    // The receiver comes first because that is the order a call supplies it
    // in, and `Signature` records it apart from the parameters so that this
    // does not have to be inferred from a count.
    if let Some(receiver) = &signature.receiver {
        let span = decl.receiver.map_or(decl.span, |it| it.span);
        match pool.shapes.of(checked, module, receiver) {
            // `var self` is a `var` parameter written in the receiver
            // position: the method names the caller's storage, so the first
            // parameter holds its address and a write to a field of `self`
            // reaches the caller's own words with no copy back.
            Some(_) if decl.receiver.is_some_and(|it| it.is_var) => params.push(shapes::ADDR),
            Some(layout) => params.push(layout),
            None => {
                errors.push(describe(&pool.shapes, receiver, span));
                ok = false;
            }
        }
        types.push(receiver.clone());
    }
    for (param, ty) in decl.params.iter().zip(&signature.params) {
        match pool.shapes.of(checked, module, ty) {
            // A `var` parameter is an ordinary slot whose `Repr` is `Addr`:
            // it names the caller's storage, so the word is the address of
            // it rather than a copy of what is in it. The type is still
            // read, because a type with no layout is a gap whichever side of
            // the alias it is on.
            Some(_) if param.is_var => params.push(shapes::ADDR),
            Some(layout) => params.push(layout),
            None => {
                errors.push(describe(&pool.shapes, ty, param.span));
                ok = false;
            }
        }
        types.push(ty.clone());
    }
    let returns = match pool.shapes.of(checked, module, &signature.ret) {
        Some(layout) => layout,
        None => {
            let span = decl.return_type.as_ref().map_or(decl.span, |ty| ty.span);
            errors.push(describe(&pool.shapes, &signature.ret, span));
            ok = false;
            shapes::UNIT
        }
    };

    ok.then_some(Boundary {
        receiver: signature.receiver.is_some(),
        params,
        types,
        returns,
        ret: signature.ret.clone(),
    })
}

/// Why a type has no layout here: the checker settled nothing, it settled a
/// declaration whose layout contains itself, or it settled something this
/// task has not reached.
///
/// The middle one is separated because it is not the same kind of thing.
/// [ADR 0035](../../../../docs/adr/0035-a-value-type-may-not-contain-itself.md)
/// makes an implicitly recursive value layout a *checker* error, so this is a
/// program that will stop being a program — and saying only "a value of type
/// `Node`" would read as a piece of work someone here still owes.
fn describe(shapes: &Shapes, ty: &Ty, span: Span) -> Diagnostic {
    match ty {
        Ty::Unknown(_) => gap::unknown(ty, span),
        Ty::Struct(name, _) | Ty::Enum(name, _) if shapes.contains_itself(name) => {
            gap::gap(&format!("`{name}`, whose layout contains itself"), span)
        }
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
    lists: Vec<Vec<Arg>>,
    index: HashMap<Vec<Arg>, ArgsId>,
}

impl Args {
    fn intern(&mut self, args: Vec<Arg>) -> ArgsId {
        if let Some(id) = self.index.get(&args) {
            return *id;
        }
        let id = ArgsId(self.lists.len() as u32);
        self.index.insert(args.clone(), id);
        self.lists.push(args);
        id
    }
}

/// Everything a [`Program`] holds once for the whole package, being built.
///
/// It is one struct rather than a parameter each because every one of them
/// is written from inside a body and read only when the program is
/// assembled: a body that meets a string literal, a `match`, a host call or
/// a struct is adding to a table that outlives the function it is in.
///
/// Each table interns. Two call sites of the same shape share one argument
/// list, two `"{n}"`s share one string, and two `Option<Int>`s share one
/// layout — which is what keeps these tables as long as the shapes a program
/// has rather than as long as the expressions that mention them.
struct Pool {
    args: Args,
    strings: Vec<Arc<str>>,
    tables: Vec<Table>,
    host_ops: Vec<HostOp>,
    builtins: Vec<Builtin>,
    shapes: Shapes,
    /// The functions the lambdas lowered to, numbered after every
    /// declaration.
    ///
    /// A slot is reserved — `None` — before the lambda's body is lowered,
    /// because that body may make a closure of its own and the inner one has
    /// to be numbered after the outer. So the entry is filled in on the way
    /// back out, and a `None` left at the end would be a lowering that
    /// reserved a number and never used it.
    lambdas: Vec<Option<Function>>,
}

impl Pool {
    fn new() -> Pool {
        Pool {
            args: Args::default(),
            strings: Vec::new(),
            tables: Vec::new(),
            host_ops: Vec::new(),
            builtins: Vec::new(),
            shapes: Shapes::new(),
            lambdas: Vec::new(),
        }
    }

    fn string(&mut self, text: &str) -> StrId {
        match self.strings.iter().position(|held| &**held == text) {
            Some(at) => StrId(at as u32),
            None => {
                self.strings.push(Arc::from(text));
                StrId((self.strings.len() - 1) as u32)
            }
        }
    }

    /// A jump table. Not interned: a `match`'s targets are program counters
    /// of the function it is in, so two tables that happen to agree agree by
    /// accident.
    fn table(&mut self, table: Table) -> TableId {
        self.tables.push(table);
        TableId((self.tables.len() - 1) as u32)
    }

    fn host_op(&mut self, op: HostOp) -> HostOpId {
        match self.host_ops.iter().position(|held| *held == op) {
            Some(at) => HostOpId(at as u32),
            None => {
                self.host_ops.push(op);
                HostOpId((self.host_ops.len() - 1) as u32)
            }
        }
    }

    fn builtin(&mut self, builtin: Builtin) -> BuiltinId {
        match self.builtins.iter().position(|held| *held == builtin) {
            Some(at) => BuiltinId(at as u32),
            None => {
                self.builtins.push(builtin);
                BuiltinId((self.builtins.len() - 1) as u32)
            }
        }
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
    /// The location a `for` binds each turn, when it holds a reference.
    ///
    /// The loop owns it rather than the per-turn scope, because the scope
    /// gives its slots back when it ends and the next turn writes this one
    /// again. That leaves nobody to clear it on the one path that does not
    /// reach the end of a turn, which is what this is: a `break` clears the
    /// element it was holding on its way out.
    element: Option<Dest>,
}

/// The state of lowering one function body.
struct Body<'a> {
    checked: &'a Checked,
    plan: &'a Plan<'a>,
    pool: &'a mut Pool,
    errors: &'a mut Vec<Diagnostic>,
    /// The module the body is written in, which is what an unqualified name
    /// in it is resolved against.
    module: &'a str,
    /// What this body is called, which is what a lambda written inside it is
    /// named after: `f#0`, and `f#0#0` for one nested in that.
    name: Arc<str>,
    /// How many lambdas this body has already made, which is the number the
    /// next one is given.
    lambdas: u32,
    frame: Frame,
    code: Vec<Inst>,
    spans: Vec<Span>,
    loops: Vec<Loop>,
    /// The location the body's answer is assembled in, and the one the
    /// trailing [`Inst::Return`] names.
    answer: Dest,
    /// What this function answers, as a type rather than as a layout.
    ///
    /// `?` needs it: the value it leaves through is the enclosing
    /// function's own `Err` or `None`, built here, and building one needs
    /// to know which of the two *this* function answers rather than what the
    /// `?` was applied to.
    returns: Ty,
}

fn lower_function(
    checked: &Checked,
    plan: &Plan,
    id: usize,
    pool: &mut Pool,
    errors: &mut Vec<Diagnostic>,
) -> Function {
    let decl = &plan.decls[id];
    let Some(boundary) = &decl.boundary else {
        return stub(decl);
    };

    let mut frame = Frame::new();
    let mut param_slots = Vec::with_capacity(boundary.params.len());
    for layout in &boundary.params {
        param_slots.push(frame.param(pool.shapes.words(*layout)));
    }
    // The answer is taken before any temporary, so it is live for the whole
    // body and never handed to something else.
    let answer = Dest {
        slot: frame.alloc(pool.shapes.words(boundary.returns)),
        layout: boundary.returns,
    };

    let mut body = Body {
        checked,
        plan,
        pool,
        errors,
        module: &decl.module,
        name: decl.name.clone(),
        lambdas: 0,
        frame,
        code: Vec::new(),
        spans: Vec::new(),
        loops: Vec::new(),
        answer,
        returns: boundary.ret.clone(),
    };

    body.frame.push_scope();
    // The receiver is the first parameter where there is one, so a written
    // parameter's location is its position shifted past it. Nothing else
    // about a method differs: the body reads `self` the way it reads any
    // binding, and where the receiver is an `Addr` — `var self` — reading it
    // is a `Load` through the word, which is the same rule a `var` parameter
    // already follows.
    let mut at = 0;
    if boundary.receiver {
        body.frame.bind("self", param_slots[0], boundary.params[0]);
        at = 1;
    }
    for (index, param) in decl.decl.params.iter().enumerate() {
        body.frame.bind(
            &param.name.node,
            param_slots[at + index],
            boundary.params[at + index],
        );
    }
    body.block(&decl.decl.body, Some(answer));
    let clears = body.frame.pop_scope();
    body.clear(&clears, decl.decl.body.span);
    body.emit(Inst::Return { src: answer.slot }, decl.decl.body.span);

    let reprs = body.frame.reprs().to_vec();
    Function {
        module: decl.module.clone(),
        name: decl.name.clone(),
        params: boundary.params.clone(),
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
        params: Vec::new(),
        refs: RefMap::of(&reprs),
        reprs,
        returns: shapes::UNIT,
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

    // ---- locations -------------------------------------------------------

    /// The words a value of `layout` occupies.
    fn words(&self, layout: LayoutId) -> Vec<Repr> {
        self.pool.shapes.words(layout).to_vec()
    }

    fn width(&self, layout: LayoutId) -> u32 {
        self.pool.shapes.width(layout)
    }

    /// Whether a location of this layout holds anything a collection traces,
    /// or an address whose live range this lowering ends.
    fn holds_ref(&self, layout: LayoutId) -> bool {
        self.pool.shapes.holds_ref(layout)
    }

    /// A run of the frame wide enough for a value of `layout`.
    fn alloc(&mut self, layout: LayoutId) -> Slot {
        let words = self.words(layout);
        self.frame.alloc(&words)
    }

    /// A temporary location of `layout`.
    fn temp(&mut self, layout: LayoutId) -> Val {
        Val::temp(self.alloc(layout), layout)
    }

    /// Gives a location's run back without ending anything's live range.
    ///
    /// Every consumer of a *value* calls [`Body::release`] instead; this is
    /// for a run the lowering allocated and knows holds nothing.
    fn give_back(&mut self, slot: Slot, layout: LayoutId) {
        let width = self.width(layout);
        self.frame.free(slot, width);
    }

    /// One [`Inst::Copy`]: ADR 0001's field-wise shallow copy, as many words
    /// as the layout says.
    fn copy(&mut self, dst: Slot, src: Slot, layout: LayoutId, span: Span) {
        if dst == src {
            return;
        }
        self.emit(Inst::Copy { dst, src, layout }, span);
    }

    /// Zeroes a location's words.
    fn zero(&mut self, slot: Slot, layout: LayoutId, span: Span) {
        self.emit(Inst::Clear { slot, layout }, span);
    }

    /// Puts a value in a form a location of `want` can hold.
    ///
    /// There is one conversion in the language and it is erasure, so the
    /// only difference this bridges is a box: a concrete value on its way
    /// into a `dyn` location.
    ///
    /// Anything else is a copy of the wrong width. It is reported as a gap
    /// rather than emitted, because `lower` answers `Err` before the
    /// verifier ever sees it — which is the difference between a construct
    /// this lowering has not been taught and a program in the heap with the
    /// wrong number of words in it.
    fn fit(&mut self, value: Val, want: LayoutId, span: Span) -> Val {
        if value.layout == want {
            return value;
        }
        if self.is_boxed(want) {
            let dst = self.temp(want);
            self.emit(
                Inst::Box {
                    dst: dst.slot,
                    src: value.slot,
                    layout: value.layout,
                },
                span,
            );
            self.release(value, span);
            return dst;
        }
        let held = self.pool.shapes.layout(value.layout).name.clone();
        let wanted = self.pool.shapes.layout(want).name.clone();
        self.errors.push(gap::gap(
            &format!("a `{held}` where a `{wanted}` goes, which this lowering cannot convert"),
            span,
        ));
        self.release(value, span);
        self.temp(want)
    }

    // ---- reading a layout ------------------------------------------------

    /// The field `name` of a struct-shaped layout: its word offset within
    /// the value and its own layout.
    ///
    /// This is where a field access stops being an instruction. `l.from.x`
    /// is `base + Field::at` twice over, computed here and added to a slot
    /// number, because the fields of an inline value are *where the value
    /// is*.
    fn field_of(&self, layout: LayoutId, name: &str) -> Option<crate::layout::Field> {
        self.pool.shapes.layout(layout).field(name).cloned()
    }

    /// The fields of a struct-shaped layout, in declaration order.
    fn fields_of(&self, layout: LayoutId) -> Option<Vec<crate::layout::Field>> {
        match &self.pool.shapes.layout(layout).shape {
            crate::layout::Shape::Struct { fields, .. } => Some(fields.clone()),
            _ => None,
        }
    }

    /// The parts of case `index` of an enum-shaped layout, and the payload
    /// region's own words.
    ///
    /// A part's `at` is an offset within the payload region, which begins
    /// *after* the discriminant, so a part of the value is at
    /// `base + 1 + at`.
    fn case_of(
        &self,
        layout: LayoutId,
        index: u32,
    ) -> Option<(Vec<crate::layout::Part>, Vec<Repr>)> {
        match &self.pool.shapes.layout(layout).shape {
            crate::layout::Shape::Enum { cases, payload } => cases
                .get(index as usize)
                .map(|case| (case.parts.clone(), payload.clone())),
            _ => None,
        }
    }

    /// Whether a value of this layout is one heap address naming a box.
    ///
    /// A `dyn Trait` is the one this lowering builds: erasure is where a
    /// value stops having a static width, and a heap object is where a value
    /// without a static width lives.
    fn is_boxed(&self, layout: LayoutId) -> bool {
        matches!(
            self.pool.shapes.layout(layout).shape,
            crate::layout::Shape::Boxed
        )
    }

    /// Whether a value of this layout is one word of scalar bits, which is
    /// what an instruction rather than a walk can compare.
    fn is_scalar(&self, layout: LayoutId) -> bool {
        matches!(
            self.pool.shapes.layout(layout).shape,
            crate::layout::Shape::Word(_)
        )
    }

    /// Ends the live range of the reference locations a scope owned.
    ///
    /// A scalar body emits nothing here, because [`Frame::pop_scope`]
    /// answers an empty list. A body that holds an object emits one clear
    /// per binding, and that clear is what keeps a static reference map from
    /// being a leak: the map says which slots a collection *reads*, and only
    /// the data can say when the value in one stopped being needed.
    fn clear(&mut self, locations: &[(Slot, LayoutId)], span: Span) {
        for (slot, layout) in locations {
            self.zero(*slot, *layout, span);
        }
    }

    /// Ends a temporary's live range, clearing the location when it held
    /// something the collector would otherwise trace.
    ///
    /// Every consumer of a value calls this rather than freeing the run
    /// behind it, so a reference a body stopped needing is null from that
    /// instruction onwards rather than until the frame returns. It is
    /// unconditional for a location holding a `Ref` or an `Addr`: the run
    /// goes back on a free list here, and whether some later value of the
    /// same shape happens to overwrite it is a fact about the rest of the
    /// body, which this cannot see and must not assume.
    ///
    /// A borrowed location is not cleared, because it is not this
    /// expression's to end: a parameter, a local, or the answer outlives the
    /// expression that read it, and the scope that owns it clears it.
    fn release(&mut self, value: Val, span: Span) {
        if !value.temp {
            return;
        }
        if self.holds_ref(value.layout) {
            self.zero(value.slot, value.layout, span);
        }
        self.give_back(value.slot, value.layout);
    }

    /// A string of the program's pool, added only if it is not already in
    /// it.
    fn string(&mut self, text: &str) -> StrId {
        self.pool.string(text)
    }

    /// The layout of a value of `ty`, reporting the type this lowering
    /// cannot build one for.
    fn layout(&mut self, ty: &Ty, span: Span) -> Option<LayoutId> {
        match self.pool.shapes.of(self.checked, self.module, ty) {
            Some(id) => Some(id),
            None => {
                self.report(ty, span);
                None
            }
        }
    }

    /// Says why a type has no layout here, in the words [`describe`] chooses.
    fn report(&mut self, ty: &Ty, span: Span) {
        let item = describe(&self.pool.shapes, ty, span);
        self.errors.push(item);
    }

    // ---- reading the checker's answers -----------------------------------

    /// The type the checker settled for `expr`.
    fn ty(&self, expr: &Expr) -> Option<&Ty> {
        self.checked.facts.ty(expr.span.file, expr.id)
    }

    /// The same, owned, for a caller that goes on to write into the frame
    /// while it holds the answer.
    fn owned_ty(&mut self, expr: &Expr) -> Option<Ty> {
        match self.ty(expr).cloned() {
            Some(ty) => Some(ty),
            None => {
                self.errors.push(gap::gap(
                    "an expression the checker recorded no type for",
                    expr.span,
                ));
                None
            }
        }
    }

    /// The layout of `expr`'s value, reporting the reason there is none.
    ///
    /// A reported failure answers the one-word `Unit` layout so that
    /// lowering can carry on and find the rest of what is wrong in the same
    /// run. The answer is never acted on: `lower` has an error and will not
    /// hand the program back.
    fn layout_of(&mut self, expr: &Expr) -> LayoutId {
        let Some(ty) = self.ty(expr).cloned() else {
            self.errors.push(gap::gap(
                "an expression the checker recorded no type for",
                expr.span,
            ));
            return shapes::UNIT;
        };
        self.layout(&ty, expr.span).unwrap_or(shapes::UNIT)
    }

    /// Whether this expression leaves rather than answering: a `return`, a
    /// `break`, a `continue`, or a form built out of them.
    ///
    /// What it decides is whether the surrounding form copies the answer.
    /// Nothing is ever written to a diverging expression's location, so
    /// copying from it would move words that were never produced — and where
    /// the surrounding form wants a different layout, it would not even be
    /// well formed.
    fn diverges(&self, expr: &Expr) -> bool {
        matches!(self.ty(expr), Some(Ty::Never))
    }

    /// Copies an expression's answer into the location the surrounding form
    /// is assembling its own in.
    ///
    /// The one thing this is not is a copy in every case. A body whose
    /// declared return type is `dyn Trait` erases its tail on the way into
    /// the answer, because a declared return type is a written type and that
    /// is where the language's one implicit conversion happens. The answer
    /// is taken before any temporary and is never handed to anything else,
    /// so `dst == self.answer` names exactly the function's own tail and no
    /// nested form's destination.
    fn store(&mut self, dst: Dest, value: &Val, from: &Expr) {
        if self.diverges(from) || value.slot == dst.slot {
            return;
        }
        // Where the two disagree the value is erased on the way in, and
        // that is the language's one implicit conversion. `Body::erase`
        // covers the positions where a `dyn` type is *written*; this covers
        // the ones where the checker settled `dyn` for an expression whose
        // value was never put through one — the tail of a body declared
        // `-> dyn Trait`, an arm of an `if` in a `dyn` position.
        //
        // The source is borrowed here whatever it was: whoever called this
        // still owns it and will end its live range itself.
        if value.layout != dst.layout {
            let held = self.fit(
                Val::borrowed(value.slot, value.layout),
                dst.layout,
                from.span,
            );
            self.copy(dst.slot, held.slot, dst.layout, from.span);
            self.release(held, from.span);
            return;
        }
        self.copy(dst.slot, value.slot, dst.layout, from.span);
    }

    /// A location for a value nothing will produce, so that a diverging
    /// expression still answers something the caller can hold.
    fn dead(&mut self, expr: &Expr) -> Val {
        let layout = self.layout_of(expr);
        self.temp(layout)
    }

    /// Reports a construct this lowering has not been taught, answering a
    /// location of the right shape so the walk can continue and report the
    /// rest.
    fn gap(&mut self, what: &str, expr: &Expr) -> Val {
        self.errors.push(gap::gap(what, expr.span));
        self.dead(expr)
    }
}
