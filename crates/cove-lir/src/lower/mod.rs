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
//! # A generic is one function per instantiation
//!
//! `docs/LINEAR_VM.md` says why there was no choice: a slot's `Repr` is fixed
//! for the whole function, that is what makes one static reference map
//! correct at every program counter, and a generic value's width is a fact
//! about the type argument — `Cell<Int>` is one word and `Cell<Point>` is
//! two. Carrying layouts at run time would make widths dynamic and take the
//! map with them; boxing every generic value would allocate on `f(1)` and
//! make a type parameter mean what `dyn Trait` already means. So `f<Int>` and
//! `f<Point>` are two functions and two frames, and a generic `struct` at two
//! instantiations is two layouts.
//!
//! It costs one substitution and no second walk. The checker walked the
//! generic body once with its type parameters rigid, so every fact in it is
//! recorded in terms of them, and `Body::ty` completes one as it is read.
//! Which arguments a call site asks for is read off the facts the checker
//! settled there — `Body::instantiation` is that reading, and it is also why
//! an explicit type argument needs no path of its own: the checker applied
//! it before this crate saw anything.
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

mod assertions;
mod cells;
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
mod tasks;
mod walks;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use cove_diag::{Diagnostic, SourceMap, Span};
use cove_schema::HostSchemas;
use cove_sema::facts::MethodTarget;
use cove_sema::resolve::{FnKey, Node, Program as Checked};
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

/// Lowers a checked package: every declaration it has, whether or not
/// anything reaches it.
///
/// The result either runs or names what stopped it. Nothing in between: a
/// lowered [`Program`] has been through [`crate::verify()`], so a caller that
/// holds one holds a program whose locations, jumps and calls are all in
/// range and whose reference map is the one its reprs imply.
///
/// `sources` is the package's own text, and it is here for one reason:
/// `assert` and `assertEqual` quote the code that failed, so the lowering
/// has to read the bytes an argument's span covers. See this module's
/// `assertions` submodule for why they are lowered rather than performed.
///
/// `schemas` is the set of host modules this compilation was given, and it
/// must be the set the *checker* was given. A type a host module declares has
/// a layout — a `files.Reader` is one [`Repr::Host`] word and an
/// `http.Response` is its fields inline — and the schema is the only thing
/// that says which of the two a name is. Reading `cove_schema::hosts` here
/// instead would describe the shipped modules and no others, so a program
/// that names a type an embedding registered would lower for fewer types than
/// it checked against: a backend refusing a program the language admits,
/// rather than a gap somebody can build. `HostApi` is a trait, and an
/// embedder's module is not a lesser kind of host.
///
/// [`lower_roots`] is the same lowering over what a named set of roots
/// reaches, and it is what a command that runs a program should use — a
/// command names the roots it is about to run and this crate works out the
/// slice. This one is what a whole-package listing means — everything the
/// package declares is part of it — and it is what the lowering's own tests
/// and the corpus survey ask for.
pub fn lower(
    checked: &Checked,
    sources: &SourceMap,
    schemas: &HostSchemas,
) -> Result<Program, Vec<Diagnostic>> {
    let mut plan = Plan::index(checked);
    let everything: HashSet<FunctionId> = (0..plan.decls.len())
        .map(|at| FunctionId(at as u32))
        .collect();
    let Lowering {
        program, errors, ..
    } = emit(checked, sources, schemas, &mut plan, &everything);
    finish(program, errors)
}

/// Lowers only the declarations `roots` can reach.
///
/// A package holds programs that have nothing to do with each other —
/// `benches/` is nine of them and `tests/e2e/` is a hundred — and a gap in
/// one of them is not a reason to refuse the others. So a command lowers
/// what it is about to run and leaves the rest as a stub nothing names.
///
/// A root is a `(module, name)` pair naming a declaration the way the
/// checker's own tables do, and the slice is what *any* of them reaches.
/// That is the whole of the API a command needs: it selects roots — the
/// entry it was asked for, the test it is about to run, the entries a
/// package configures — and reachability stays here. A caller that walked
/// the call graph itself would be a second answer to a question this
/// module already has to answer, and the two would drift.
///
/// A root that names nothing this package declares contributes nothing. It
/// is not an error here, because what a name denotes is the checker's
/// question and every caller has already asked it: `run_entry` answers
/// "this package does not declare `m.f`" better than a lowering could, and
/// it answers it about the program that was actually going to run.
///
/// # One answer for the whole set
///
/// The gaps this returns are the gaps of everything `roots` reaches,
/// together, and there is no telling which root a gap came from. So a
/// caller that needs one root's failure to be one root's failure passes
/// one root — which is what [`lower_entry`] is, and why `cove test` lowers
/// each test rather than the suite.
///
/// # What "reachable" is, and how this is sure of it
///
/// The seed is [`Program::call_graph`](cove_sema::resolve::Program::call_graph),
/// which the checker already derived: for each declaration, every declaration
/// it may call. Both precisions are followed, because
/// [`CallPrecision::Approximate`](cove_sema::resolve::CallPrecision) is a
/// superset of what may run and a slice has to hold everything that might.
///
/// The seed is not the answer, though, and the graph itself says why: a
/// callee that is a *value* — `xs.map(double)`, a conformance a `dyn`
/// dispatch picks, a `Snapshot` implementation nothing writes a call to —
/// contributes no edge, because there is no call site naming it. A
/// declaration missing from the slice is not a gap the person holding the
/// source can act on; it is a stub that answers `()` where a call was meant
/// to go.
///
/// So the slice is closed against the lowering rather than against the
/// graph. Every place that turns a name into a [`FunctionId`] asks whether
/// this pass lowered it first, and a body that names a declaration this
/// slice left out records it rather than emitting a call to a stub. What was
/// recorded is added to the slice and the package is lowered again,
/// until a round wants nothing — which is a fixed point over *what this
/// lowering emits references to*, not over what a second reachability
/// analysis believes.
///
/// The call graph is what makes that one round rather than one per level of
/// the call tree: seeded with nothing, each round could only discover the
/// callees of what the round before it lowered.
pub fn lower_roots(
    checked: &Checked,
    sources: &SourceMap,
    schemas: &HostSchemas,
    roots: &[(&str, &str)],
) -> Result<Program, Vec<Diagnostic>> {
    let mut plan = Plan::index(checked);
    let mut reach = plan.reachable_from(checked, roots);
    loop {
        let Lowering {
            program,
            errors,
            wanted,
        } = emit(checked, sources, schemas, &mut plan, &reach);
        if wanted.is_empty() {
            return finish(program, errors);
        }
        reach.extend(wanted);
    }
}

/// Lowers only the declarations `module.name` can reach.
///
/// The one-root case of [`lower_roots`], which is where everything this
/// does is written down. It has a name of its own because one root is what
/// almost every caller has — `cove run` has the entry it was asked for,
/// `cove replay` has the entry the tape was recorded from, `cove test` has
/// the test it is about to run — and because a set of one is the only set
/// whose gaps belong to a single root.
pub fn lower_entry(
    checked: &Checked,
    sources: &SourceMap,
    schemas: &HostSchemas,
    module: &str,
    name: &str,
) -> Result<Program, Vec<Diagnostic>> {
    lower_roots(checked, sources, schemas, &[(module, name)])
}

/// One pass of the lowering over one set of declarations.
struct Lowering {
    program: Program,
    errors: Vec<Diagnostic>,
    /// The declarations a body named that this pass had left out.
    ///
    /// Empty for a whole-package lowering, because nothing is left out.
    /// For a sliced one it is the correction: see [`lower_entry`].
    wanted: HashSet<FunctionId>,
}

/// Lowers `reach` and stubs the rest.
fn emit<'a>(
    checked: &'a Checked,
    sources: &SourceMap,
    schemas: &HostSchemas,
    plan: &mut Plan<'a>,
    reach: &HashSet<FunctionId>,
) -> Lowering {
    let mut errors = Vec::new();
    let mut wanted = HashSet::new();
    let mut pool = Pool::new(schemas.clone());
    plan.boundaries(checked, reach, &mut pool, &mut errors);
    let mut functions = Vec::new();
    for id in 0..plan.decls.len() {
        functions.push(if reach.contains(&FunctionId(id as u32)) {
            lower_function(
                checked,
                sources,
                plan,
                id,
                &mut pool,
                &mut errors,
                &mut wanted,
            )
        } else {
            stub(&plan.decls[id])
        });
    }
    // A lambda and an instantiation are `Function`s of their own, numbered
    // after every declaration and discovered while a body is being walked
    // rather than by the plan. Either may be appended while the body that
    // asked for it is still being lowered, so this list is complete only once
    // the loop above has finished — which is why it is drained here and not
    // built beside `plan.decls`.
    functions.extend(
        pool.appended
            .drain(..)
            .map(|held| held.expect("every reserved function was lowered into its own slot")),
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
        // Only what this pass lowered is nameable. A stub answers `()`, so
        // an entry point that resolved to one would run and say nothing
        // rather than saying it was not there — and `run_entry` already has
        // a good answer for a name a program does not carry.
        //
        // A generic declaration is a stub for the same reason and is left out
        // for the same reason. What a program carries is its
        // instantiations — `f<Int>` — and there is no entry point among them:
        // a command names a declaration, and which instantiation of a generic
        // one it meant is not a question a command line can answer.
        by_name: plan
            .by_name
            .iter()
            .filter(|(_, id)| reach.contains(id))
            .filter(|(_, id)| plan.decls[id.index()].decl.generics.is_empty())
            .map(|(key, id)| (key.clone(), *id))
            .collect(),
    };
    Lowering {
        program,
        errors,
        wanted,
    }
}

/// The lowered program, or what stopped it — and the verifier's word that
/// the first of the two is well formed.
fn finish(program: Program, errors: Vec<Diagnostic>) -> Result<Program, Vec<Diagnostic>> {
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
    /// `None` for a declaration outside this lowering's scope, and for one
    /// this pass never asked about. A gap has already been reported for the
    /// first, so the stub that takes its place is never seen: `lower`
    /// answers `Err` before the program leaves. The second is a declaration
    /// the slice left out, and [`Plan::reached`] is what tells the two
    /// apart.
    boundary: Option<Boundary>,
    /// The trait whose default body this method is, when it is one.
    ///
    /// What it decides is which substitution the body is lowered under. The
    /// checker walks a default body **once**, with `self` typed as a rigid
    /// `Ty::Param("Self")` bounded by the trait, so every fact recorded
    /// inside it is written in terms of `Self` — which is the same situation
    /// a generic declaration is in, one parameter at a time. See
    /// [`Decl::substitution`].
    from_trait_default: Option<String>,
    /// The type this is a method of, as the receiver's own type.
    ///
    /// `None` for a free function, and for a method of a *generic* type,
    /// whose receiver's width depends on a type argument this does not have —
    /// see [`Decl::on_generic_type`].
    ///
    /// It is written in the package's own vocabulary — `m.Booking` rather
    /// than `Booking` — because a conformance may be declared in the module
    /// that declares the *trait* (ADR 0006's orphan rule), and then the
    /// type's bare name means nothing where the body is lowered.
    receiver_ty: Option<Ty>,
    /// The generic type this is a method of, when it is one.
    ///
    /// A method of `Cell<T>` has no boundary of its own for the same reason a
    /// generic function has none — its receiver's width depends on `T` — and
    /// it needs one thing more than a generic function does: the parameters
    /// are the *type*'s rather than the declaration's, so which arguments a
    /// call settles is read off the receiver rather than off the signature.
    /// That is not built, and this is what names it as the work rather than
    /// letting it fail as "a value of type `Cell<T>`", which says where the
    /// trouble is and not what is owed.
    on_generic_type: Option<String>,
}

/// The one type parameter a trait's default body is written in terms of.
const SELF: &str = "Self";

impl Decl<'_> {
    /// The type parameters this declaration's facts are recorded in terms
    /// of, and what this lowering puts in their place.
    ///
    /// Empty for almost everything, and then the substitution is the
    /// identity. A **trait method's default body** is the one declaration
    /// that is neither generic nor ordinary: `resolve::conform` synthesises
    /// one `FnDecl` per conforming type, all of them carrying the *trait
    /// method's* span, and `Checker::check_trait_defaults` records one
    /// `Signature` at that span with the receiver typed `Ty::Param("Self")`.
    ///
    /// So a default body is a generic declaration with one parameter, and
    /// this is what says which. Nothing else is needed: the boundary is
    /// [`boundary_of`] under this substitution, the body is [`lower_body`]
    /// under it, and a call the body makes on `self` finds the conforming
    /// type's own implementation through [`Body::conformance`] — which is
    /// the path a bounded generic already takes, for the same reason. One
    /// recorded walk serves every conformance.
    ///
    /// A default body on a *generic* type would need `Self` to stand for an
    /// instantiation rather than for a declaration, so it is left to
    /// [`Plan::boundaries`]'s earlier arm.
    fn substitution(&self) -> (Vec<Arc<str>>, Vec<Ty>) {
        match (&self.from_trait_default, &self.receiver_ty) {
            (Some(_), Some(ty)) => (vec![Arc::from(SELF)], vec![ty.clone()]),
            _ => (Vec::new(), Vec::new()),
        }
    }
}

/// A declaration's parameters and answer, as layouts.
///
/// A generic declaration has one of these per instantiation rather than one
/// of its own, so it is cloned into [`Instance`] and read from there at a
/// call site — see [`Body::shape`].
#[derive(Clone)]
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
    /// Whether the last parameter collects the arguments the ones before it
    /// did not take.
    ///
    /// One flag rather than a position, because the checker has already
    /// refused a variadic parameter anywhere but last — `cove::type::
    /// variadic_position` — and refused one written with a default.
    variadic: bool,
    /// Whether the declaration was written `async fn`.
    ///
    /// It changes nothing about the function and everything about the call.
    /// `returns` above is the layout of `T` and not of `Task<T>`, because
    /// the checker's `Signature::ret` is `T` and because the oracle's
    /// `Interpreter::invoke_body` produces a `T` — the task is made *around*
    /// the answer, by the caller, after the body has already run. So the
    /// declaration is lowered as an ordinary function and every Cove call
    /// site follows its [`Inst::Call`] with an [`Inst::Settled`].
    ///
    /// The three places that reach a body from outside Cove — an entry, a
    /// host `invoke`, and a host calling a Cove callback — all *await* what
    /// they get in the oracle, and awaiting a settled task is the value.
    /// Here they get the value, because no task was made. That is the same
    /// answer arrived at by not building the thing that would be undone.
    is_async: bool,
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
    /// The declarations this pass is lowering rather than stubbing.
    ///
    /// It is the whole numbering for [`lower`] and one entry's reachable set
    /// for [`lower_entry`]. [`Plan::reached`] is what reads it, and every
    /// place that names a declaration asks before it emits a call.
    lowered: HashSet<FunctionId>,
}

impl<'a> Plan<'a> {
    /// Numbers every declaration the package has.
    ///
    /// The numbering is over the whole package whether or not a slice will
    /// use all of it, and that is what makes a [`FunctionId`] mean the same
    /// thing in every pass [`lower_entry`] runs: a set of ids gathered by one
    /// round names the same declarations in the next.
    ///
    /// Nothing here reads a signature or builds a layout. What a call to a
    /// declaration passes is [`Plan::boundaries`], which is asked only about
    /// the declarations a pass is actually lowering — so a slice pays for the
    /// types it reaches and not for the package's.
    fn index(checked: &'a Checked) -> Plan<'a> {
        let mut plan = Plan {
            decls: Vec::new(),
            by_name: BTreeMap::new(),
            lookup: HashMap::new(),
            methods: HashMap::new(),
            lowered: HashSet::new(),
        };
        for (name, resolved) in &checked.modules {
            for (fn_name, entry) in &resolved.functions {
                let module: Arc<str> = Arc::from(name.as_str());
                let id = plan.declare(module, Arc::from(fn_name.as_str()), entry.decl.as_ref());
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
                let id = plan.declare(module, lowered, entry.decl.as_ref());
                plan.decls[id.index()].from_trait_default = entry.from_trait_default.clone();
                let owner = resolved.owner_of(type_name).unwrap_or(name.as_str());
                if is_generic_type(checked, owner, type_name) {
                    plan.decls[id.index()].on_generic_type = Some(type_name.clone());
                } else {
                    plan.decls[id.index()].receiver_ty = receiver_ty(checked, owner, type_name);
                }
                plan.methods
                    .insert((owner.to_string(), type_name.clone(), method.clone()), id);
            }
        }
        plan
    }

    /// Reads the boundary of every declaration in `reach`, and reports what
    /// stopped one.
    ///
    /// This is where a pass's errors about *declarations* come from, and
    /// restricting it to the slice is most of what slicing is worth: a
    /// generic function or an `async fn` in a module the entry never enters
    /// is not work this entry is waiting on.
    fn boundaries(
        &mut self,
        checked: &'a Checked,
        reach: &HashSet<FunctionId>,
        pool: &mut Pool,
        errors: &mut Vec<Diagnostic>,
    ) {
        self.lowered = reach.clone();
        for at in 0..self.decls.len() {
            let id = FunctionId(at as u32);
            if !reach.contains(&id) {
                continue;
            }
            let decl = &self.decls[at];
            let module = decl.module.to_string();
            let (generics, args) = decl.substitution();
            let boundary = if let Some(type_name) = &decl.on_generic_type {
                let named = format!("`{type_name}.{}`", short_name(&decl.name));
                errors.push(gap::gap(
                    &format!("{named}, a method of a generic type"),
                    decl.decl.span,
                ));
                None
            } else if decl.from_trait_default.is_some() && generics.is_empty() {
                // A default body whose conforming type this lowering could
                // not name. Nothing in the corpus reaches it — a conformance
                // is declared for a type of the package — and it is named
                // rather than left out because the alternative is a stub that
                // answers `()`.
                let named = format!("`{}`", decl.name);
                errors.push(gap::gap(
                    &format!("{named}, a trait method's default body on a type with no layout"),
                    decl.decl.span,
                ));
                None
            } else if !decl.decl.generics.is_empty() {
                // A generic declaration has no one boundary. Its parameters'
                // widths depend on what its type parameters stand for —
                // `Cell<Int>` is one word and `Cell<Point>` is two — so the
                // boundary belongs to an instantiation and
                // [`Body::instantiate`] reads one per set of arguments. What
                // stands here is a stub nothing names.
                None
            } else {
                boundary_of(checked, &module, decl.decl, &generics, &args, pool, errors)
            };
            self.decls[at].boundary = boundary;
        }
    }

    /// Numbers one declaration and records the name it answers to.
    fn declare(&mut self, module: Arc<str>, name: Arc<str>, decl: &'a FnDecl) -> FunctionId {
        let id = FunctionId(self.decls.len() as u32);
        self.by_name.insert((module.clone(), name.clone()), id);
        self.decls.push(Decl {
            module,
            name,
            decl,
            boundary: None,
            from_trait_default: None,
            receiver_ty: None,
            on_generic_type: None,
        });
        id
    }

    /// The declarations any of `roots` can reach, as the checker's call
    /// graph answers it.
    ///
    /// A seed rather than a verdict: see [`lower_roots`] for what the graph
    /// cannot see and what closes the gap.
    ///
    /// One walk over every root rather than one walk each, because the
    /// answer is a union and `seen` is what makes a shared callee cost the
    /// walk once no matter how many roots reach it.
    fn reachable_from(&self, checked: &'a Checked, roots: &[(&str, &str)]) -> HashSet<FunctionId> {
        let mut seen: BTreeSet<Node> = BTreeSet::new();
        let mut stack: Vec<Node> = roots
            .iter()
            .map(|(module, name)| (module.to_string(), FnKey::Fn(name.to_string())))
            .collect();
        while let Some(node) = stack.pop() {
            if !seen.insert(node.clone()) {
                continue;
            }
            let Some(edges) = checked.call_graph.get(&node) else {
                continue;
            };
            stack.extend(edges.keys().cloned());
        }
        seen.iter().filter_map(|node| self.id_of(node)).collect()
    }

    /// The declaration a call-graph node names.
    fn id_of(&self, node: &Node) -> Option<FunctionId> {
        let (module, key) = node;
        match key {
            FnKey::Fn(name) => self.lookup.get(&(module.clone(), name.clone())).copied(),
            FnKey::Method(type_name, method) => self
                .by_name
                .get(&(
                    Arc::from(module.as_str()),
                    Arc::from(format!("{type_name}.{method}")),
                ))
                .copied(),
        }
    }

    /// Whether this pass lowered `id` rather than stubbing it.
    ///
    /// Every place that turns a name into a [`FunctionId`] asks this before
    /// it emits anything naming one, because a stub answers `()` and a call
    /// to it would be a wrong answer rather than a refusal. What a caller
    /// does with a `false` is [`Body::reached`]: record the declaration and
    /// let the next pass lower it.
    fn reached(&self, id: FunctionId) -> bool {
        self.lowered.contains(&id)
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
    ///
    /// `None` for an id past the declarations — a lambda or an instantiation
    /// — which [`Body::shape`] answers instead.
    fn shape(&self, id: FunctionId) -> Option<CallShape> {
        Some(self.decls.get(id.index())?.boundary.as_ref()?.shape())
    }
}

impl Boundary {
    /// What one call site has to match, as owned values.
    fn shape(&self) -> CallShape {
        CallShape {
            params: self.params.clone(),
            types: self.types.clone(),
            returns: self.returns,
            receiver: self.receiver,
            variadic: self.variadic,
            is_async: self.is_async,
        }
    }
}

/// What one call site has to match, held apart from the [`Plan`] so that a
/// body can read it while it is writing into its own frame.
struct CallShape {
    params: Vec<LayoutId>,
    types: Vec<Ty>,
    returns: LayoutId,
    receiver: bool,
    variadic: bool,
    is_async: bool,
}

impl CallShape {
    /// How many parameters the call site writes, which is every one but the
    /// receiver.
    ///
    /// It is no longer the number of *arguments* a call passes: a variadic
    /// parameter takes any number and a defaulted one takes none, so what
    /// lines a call up with a frame is [`Body::assign`] rather than a count.
    fn written(&self) -> usize {
        self.params.len() - usize::from(self.receiver)
    }

    /// The layout of written parameter `at`, past the receiver.
    fn param(&self, at: usize) -> LayoutId {
        self.params[usize::from(self.receiver) + at]
    }

    /// The type of written parameter `at`, past the receiver.
    ///
    /// For a variadic one this is the *element* type, because that is what
    /// each collected argument is.
    fn ty(&self, at: usize) -> &Ty {
        &self.types[usize::from(self.receiver) + at]
    }
}

/// The type a method of `owner`'s declaration of `name` receives, written
/// the way the package names it.
///
/// `m.Booking` rather than `Booking`, because a conformance may be written
/// in the module that declares the *trait* — ADR 0006's orphan rule — and
/// the type's bare name means nothing there. It is the same reason
/// [`shapes::qualified`] exists, and `shapes::declaring` reads a qualified
/// name back apart.
///
/// A method's receiver is otherwise read off the checker's `Signature`, and
/// this is not a second answer to that: it is asked only where a signature
/// records `Ty::Param("Self")` and something has to say what `Self` is.
///
/// A declaration this cannot place — a conformance on a type the package
/// does not declare — answers `None`, and a default body on one is a gap.
fn receiver_ty(checked: &Checked, owner: &str, name: &str) -> Option<Ty> {
    let resolved = checked.modules.get(owner)?;
    let qualified: Arc<str> = Arc::from(format!("{owner}.{name}"));
    if resolved.structs.contains_key(name) {
        return Some(Ty::Struct(qualified, Vec::new()));
    }
    if resolved.enums.contains_key(name) {
        return Some(Ty::Enum(qualified, Vec::new()));
    }
    None
}

/// Whether `owner`'s declaration of `name` binds type parameters.
fn is_generic_type(checked: &Checked, owner: &str, name: &str) -> bool {
    let Some(resolved) = checked.modules.get(owner) else {
        return false;
    };
    let generic = |generics: &[cove_syntax::ast::GenericParam]| !generics.is_empty();
    resolved
        .structs
        .get(name)
        .is_some_and(|entry| generic(&entry.decl.generics))
        || resolved
            .enums
            .get(name)
            .is_some_and(|entry| generic(&entry.decl.generics))
}

/// The method half of a `Type.method` declaration name.
fn short_name(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, method)| method)
}

/// What a call to this declaration passes and answers, read off the
/// checker's signature rather than off the source annotations.
///
/// The annotations are names; the signature is what those names resolved to
/// in the module they were written in, which is the only reading of a
/// `-> other.Thing` that means the same in both modules.
///
/// `generics` and `args` are the declaration's type parameters and what this
/// instantiation puts in their place, and they are empty for a declaration
/// that binds none. A generic declaration has a boundary *per instantiation*
/// and no boundary of its own: `fn f<T>(x: T)` says how many parameters there
/// are and nothing about how wide one is, and a width is what a frame is made
/// of.
#[allow(clippy::too_many_arguments)]
fn boundary_of(
    checked: &Checked,
    module: &str,
    decl: &FnDecl,
    generics: &[Arc<str>],
    args: &[Ty],
    pool: &mut Pool,
    errors: &mut Vec<Diagnostic>,
) -> Option<Boundary> {
    let mut ok = true;
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
        let receiver = &receiver.instantiate(generics, args);
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
        let ty = &ty.instantiate(generics, args);
        // A variadic parameter is an immutable `Array<T>` inside the body
        // whatever stands in front of it, and the signature records the
        // element type `T` rather than the array — so the location the callee
        // reads is one layout wider out than the one a written argument fits
        // into. `Boundary::types` keeps the element type, because that is
        // what each collected argument is erased to.
        let held = if param.variadic {
            Ty::Array(Box::new(ty.clone()))
        } else {
            ty.clone()
        };
        match pool.shapes.of(checked, module, &held) {
            // A `var` parameter is an ordinary slot whose `Repr` is `Addr`:
            // it names the caller's storage, so the word is the address of
            // it rather than a copy of what is in it. The type is still
            // read, because a type with no layout is a gap whichever side of
            // the alias it is on. A variadic one is not an alias whatever the
            // source wrote, which is the `is_var && !variadic` the checker's
            // own `ParamSig` records.
            Some(_) if param.is_var && !param.variadic => params.push(shapes::ADDR),
            Some(layout) => params.push(layout),
            None => {
                errors.push(describe(&pool.shapes, &held, param.span));
                ok = false;
            }
        }
        types.push(ty.clone());
    }
    let ret = signature.ret.instantiate(generics, args);
    let returns = match pool.shapes.of(checked, module, &ret) {
        Some(layout) => layout,
        None => {
            let span = decl.return_type.as_ref().map_or(decl.span, |ty| ty.span);
            errors.push(describe(&pool.shapes, &ret, span));
            ok = false;
            shapes::UNIT
        }
    };

    ok.then_some(Boundary {
        receiver: signature.receiver.is_some(),
        variadic: decl.params.last().is_some_and(|param| param.variadic),
        // The signature's `ret` is `T` for an `async fn`, so `returns` above
        // is the layout of the value the body produces and the handle is the
        // call site's to make. See `Boundary::is_async`.
        is_async: decl.is_async,
        params,
        types,
        returns,
        ret,
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
    /// The functions numbered after every declaration: the body a lambda
    /// lowered to, and the body one instantiation of a generic declaration
    /// lowered to.
    ///
    /// A slot is reserved — `None` — before the body is lowered, because
    /// that body may close over a lambda or ask for an instantiation of its
    /// own and the inner one has to be numbered after the outer. So the
    /// entry is filled in on the way back out, and a `None` left at the end
    /// would be a lowering that reserved a number and never used it.
    ///
    /// The two kinds share one list because they share the one thing that
    /// matters about it — a number past the declarations, taken before the
    /// body is known — and two lists would have to agree on which of them a
    /// given number was from.
    appended: Vec<Option<Function>>,
    /// What each instantiation is, keyed by the id it was given.
    ///
    /// A call site reads a boundary and a declaration off this the way it
    /// reads them off [`Plan`] for an ordinary declaration.
    instances: HashMap<FunctionId, Instance>,
    /// The id one declaration at one set of type arguments was given.
    ///
    /// This is what makes a generic instantiated twice at one type cost one
    /// function, and it is also what makes a *recursive* generic terminate:
    /// the id is recorded before the body is lowered, so a call the body
    /// makes to itself finds the number rather than starting again.
    instance_ids: HashMap<(FunctionId, String), FunctionId>,
    /// The instantiations being lowered right now, outermost first.
    ///
    /// A chain rather than a count, because what a program that exceeds the
    /// bound needs told is which instantiation asked for which — see
    /// [`Body::instantiate`].
    open: Vec<String>,
}

/// One monomorphisation: which declaration it lowers, and for what.
///
/// A generic declaration is not a function here. `fn f<T>(x: T)` says how
/// many parameters there are and nothing about how wide one is, and a width
/// is what a frame is made of — so what is lowered is one `Function` per set
/// of type arguments, and this is what says which one.
struct Instance {
    /// The generic declaration in [`Plan::decls`], which is where the labels,
    /// the defaults and the syntax of the body are read from.
    decl: FunctionId,
    /// The type parameters the declaration binds, in declaration order.
    generics: Vec<Arc<str>>,
    /// What this instantiation puts in their place, in the same order.
    args: Vec<Ty>,
    /// The boundary those arguments settle.
    boundary: Boundary,
}

impl Pool {
    fn new(schemas: HostSchemas) -> Pool {
        Pool {
            args: Args::default(),
            strings: Vec::new(),
            tables: Vec::new(),
            host_ops: Vec::new(),
            builtins: Vec::new(),
            shapes: Shapes::new(schemas),
            appended: Vec::new(),
            instances: HashMap::new(),
            instance_ids: HashMap::new(),
            open: Vec::new(),
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
    /// How many temporaries were live outside the loop, so an early exit
    /// knows which ones it made and which ones it merely found.
    ///
    /// A `break` clears the temporaries above this mark and none below it.
    /// The ones below are the loop's own machinery and the enclosing
    /// expression's — the array a `for` is walking is read again after the
    /// `break` lands, and an enclosing loop's is read for the rest of *its*
    /// run.
    held: usize,
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

/// A task scope being lowered, and what leaving it early owes it.
struct OpenScope {
    /// The `Repr::Scope` slot the [`Inst::ScopeEnter`] wrote.
    slot: Slot,
    /// How many loops were open outside this scope.
    ///
    /// What it decides is which of the two directions a jump is going. A
    /// `break` leaves every scope opened *inside* its own loop and none
    /// opened outside it, and that is the same question `Loop::depth` asks
    /// about lexical scopes, asked the other way round.
    loops: usize,
    /// Whether any child spawned into this scope answers a `Result`.
    ///
    /// What it decides is whether leaving the scope can produce a failure at
    /// all. `wait_for_children` returns a child's `Err` from the function the
    /// scope was written in, so that function must be able to carry one — but
    /// only if a child can *make* one, and a scope over `Task<Verdict>`
    /// children cannot. Asking every scope for a failure layout refused two
    /// corpus programs the checker had already cleared, for the same reason
    /// and by the same predicate `Checker::spawned` uses.
    can_fail: bool,
}

/// The state of lowering one function body.
struct Body<'a> {
    checked: &'a Checked,
    /// The package's own text, which `assert` and `assertEqual` quote.
    ///
    /// Nothing else in this crate reads it: a lowering answers what the
    /// checker settled, and source text is not one of those answers. The two
    /// assertions are the exception the language itself makes — their failure
    /// message names the condition in the words the test was written in, and
    /// only the compiler has them.
    sources: &'a SourceMap,
    plan: &'a Plan<'a>,
    pool: &'a mut Pool,
    errors: &'a mut Vec<Diagnostic>,
    /// The declarations this body named that the pass had left out of its
    /// slice. See [`lower_entry`].
    wanted: &'a mut HashSet<FunctionId>,
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
    /// The task scopes this body has open, outermost first.
    ///
    /// A scope is left by waiting for or cancelling its children, and there
    /// are two ways out of one: the body reaches its end, which is
    /// [`Inst::ScopeLeave`], or control leaves through a `return`, a `?`, a
    /// `break` or a `continue`, which is [`Inst::ScopeCancel`]. Only the
    /// first is written where the `scope` is. The second is an obligation on
    /// every exit path, exactly as [`Inst::Clear`] is, and this is the list
    /// that says which scopes a given jump is leaving.
    scopes: Vec<OpenScope>,
    /// The temporaries this body is holding that a collection would trace,
    /// innermost last.
    ///
    /// A scope answers which *bindings* it owns, and that is what
    /// [`Frame::pop_scope`] and [`Frame::refs_within`] are for. A temporary
    /// belongs to no scope: it is the expression's own, and the expression
    /// that made it ends its live range with [`Body::release`]. That works
    /// for every path that reaches the release — and an early exit does not.
    ///
    /// `f(a, if c { b } else { break })` evaluates `a` into a temporary, and
    /// the `break` leaves before the call that would have consumed it. The
    /// scopes hold nothing about `a`, so `leave_turn` cleared the bindings
    /// and the loop's element and left the temporary holding a reference for
    /// the rest of the frame. This is the list that answers it.
    held: Vec<(Slot, LayoutId)>,
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
    /// The type parameters the declaration this body lowers binds, in
    /// declaration order. Empty for a declaration that binds none.
    generics: Vec<Arc<str>>,
    /// What this instantiation puts in their place, in the same order.
    ///
    /// The checker walked a generic body **once**, with its parameters rigid,
    /// so every fact recorded inside it is written in terms of `generics`.
    /// This is what completes one: [`Body::ty`] answers `m.Article` where the
    /// fact says `T`, and everything downstream — a layout, a width, which
    /// conformance a bounded call reaches — falls out of that one
    /// substitution rather than out of a rule per construct.
    args: Vec<Ty>,
    /// The layout of the value behind each [`shapes::ADDR`] slot: what a
    /// `var` parameter, or a `var self`, names in the caller's frame.
    ///
    /// A slot's [`Repr`] is all a frame records, and `Addr` says only that
    /// the word is an address. Almost nothing needs more, because every
    /// *read* of a `var` parameter is at the layout the checker settled for
    /// the expression doing the reading — [`Body::name`] and
    /// [`Body::place_of`] both take it from there.
    ///
    /// A capture is the one place with no such expression: `Body::captured_by`
    /// holds a name and a slot and nothing the checker recorded a type for.
    /// So the parameter's own layout is written down where the boundary was
    /// read, which is the only place it is known without asking a second
    /// time.
    aliases: HashMap<Slot, LayoutId>,
}

#[allow(clippy::too_many_arguments)]
fn lower_function(
    checked: &Checked,
    sources: &SourceMap,
    plan: &Plan,
    id: usize,
    pool: &mut Pool,
    errors: &mut Vec<Diagnostic>,
    wanted: &mut HashSet<FunctionId>,
) -> Function {
    let decl = &plan.decls[id];
    let Some(boundary) = &decl.boundary else {
        return stub(decl);
    };
    let name = decl.name.clone();
    // The same substitution the boundary was read under, so that a fact
    // recorded in terms of `Self` completes to the same type in the frame
    // and in the body.
    let (generics, args) = decl.substitution();
    lower_body(
        checked, sources, plan, decl, boundary, name, &generics, &args, pool, errors, wanted,
    )
}

/// Lowers one declaration's body, at one instantiation of its type
/// parameters.
///
/// `generics` and `args` are empty for an ordinary declaration, and then the
/// substitution is the identity and this is exactly what it always was. For a
/// generic declaration they are what the call site asked for, and they are
/// carried on the [`Body`] rather than applied to the syntax: the checker
/// walked the body once, so the facts are recorded once in terms of the
/// parameters, and completing a fact as it is read is what makes one recorded
/// walk serve every instantiation.
#[allow(clippy::too_many_arguments)]
fn lower_body(
    checked: &Checked,
    sources: &SourceMap,
    plan: &Plan,
    decl: &Decl,
    boundary: &Boundary,
    name: Arc<str>,
    generics: &[Arc<str>],
    args: &[Ty],
    pool: &mut Pool,
    errors: &mut Vec<Diagnostic>,
    wanted: &mut HashSet<FunctionId>,
) -> Function {
    let mut frame = Frame::new();
    let mut param_slots = Vec::with_capacity(boundary.params.len());
    for layout in &boundary.params {
        param_slots.push(frame.param(pool.shapes.words(*layout)));
    }
    // What each `var` parameter names, at the width the parameter was
    // declared. `boundary_of` resolved that type to a layout before it chose
    // `ADDR` for the slot, so this asks the interned table for an answer it
    // already holds.
    let mut aliases = HashMap::new();
    for (at, layout) in boundary.params.iter().enumerate() {
        if *layout != shapes::ADDR {
            continue;
        }
        if let Some(held) = pool.shapes.of(checked, &decl.module, &boundary.types[at]) {
            aliases.insert(param_slots[at], held);
        }
    }
    // The answer is taken before any temporary, so it is live for the whole
    // body and never handed to something else.
    let answer = Dest {
        slot: frame.alloc(pool.shapes.words(boundary.returns)),
        layout: boundary.returns,
    };

    let mut body = Body {
        checked,
        sources,
        plan,
        pool,
        errors,
        wanted,
        module: &decl.module,
        name: name.clone(),
        lambdas: 0,
        frame,
        code: Vec::new(),
        spans: Vec::new(),
        loops: Vec::new(),
        scopes: Vec::new(),
        held: Vec::new(),
        answer,
        returns: boundary.ret.clone(),
        generics: generics.to_vec(),
        args: args.to_vec(),
        aliases,
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
        name,
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

/// What stands in for a declaration this pass did not lower.
///
/// Three kinds reach it, and they end differently.
///
/// One is a declaration this lowering reported a gap about, and nothing ever
/// runs that: a gap is an error and the program is not handed back.
///
/// The second is a declaration [`lower_entry`]'s slice left out, and that one
/// is in a program that *does* run. Nothing can name it: no call was emitted
/// to it — the fixed point is what makes that true — and it is left out of
/// [`Program::by_name`], so it is not an entry point either. It exists so
/// that function ids stay dense, which is what lets a set of ids gathered by
/// one pass mean the same thing in the next.
///
/// The third is a **generic declaration**, and it is in a program that runs
/// and is not an error. It is not a stand-in for anything: a generic
/// declaration is not one function, so there is nothing here for it to be.
/// What the program carries instead is its instantiations, each a function
/// of its own numbered past the declarations. This holds its number for the
/// same reason the second kind does, and is left out of
/// [`Program::by_name`] for the same reason too.
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
    ///
    /// A temporary that holds a reference is recorded in [`Body::held`] for
    /// the length of its live range, so that an early exit from a loop can
    /// clear the ones a scope knows nothing about.
    fn temp(&mut self, layout: LayoutId) -> Val {
        let slot = self.alloc(layout);
        if self.holds_ref(layout) {
            self.hold(slot, layout);
        }
        Val::temp(slot, layout)
    }

    /// Records that a temporary at `slot` is live.
    ///
    /// Any earlier entry for the same slot is dropped first. A run holds one
    /// value at a time — the frame only hands one out again after it has been
    /// freed — so a second entry for a slot supersedes the first rather than
    /// standing beside it, and a stale one would be a `Clear` of a location
    /// something else is now using.
    fn hold(&mut self, slot: Slot, layout: LayoutId) {
        self.forget(slot);
        self.held.push((slot, layout));
    }

    /// Ends the record of the temporary at `slot`, whether or not there was
    /// one.
    fn forget(&mut self, slot: Slot) {
        self.held.retain(|(held, _)| *held != slot);
    }

    /// Gives a location's run back without ending anything's live range.
    ///
    /// Every consumer of a *value* calls [`Body::release`] instead; this is
    /// for a run the lowering allocated and knows holds nothing.
    fn give_back(&mut self, slot: Slot, layout: LayoutId) {
        let width = self.width(layout);
        self.forget(slot);
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
    /// only difference this bridges is a box — and a box has two directions.
    /// A concrete value on its way into a `dyn` or an `Any` location is
    /// boxed; an erased value on its way into a location whose type is
    /// written is opened, which is [`Body::unbox`].
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
        if self.is_boxed(value.layout) {
            return self.unbox(value, want, span);
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

    /// Opens an erased value at the layout the place using it names.
    ///
    /// Where `want` comes from is the whole of what this depends on, and it
    /// is never invented here: it is a type the source *wrote* at the place
    /// the value is being used — a declared parameter, a declared return
    /// type, a field's declared type — or, for an operator, the layout of
    /// the operand beside it. A use where nothing says is a gap raised by
    /// the caller rather than a layout guessed here.
    ///
    /// The trap is [`Inst::Unbox`]'s: a box carries the [`LayoutId`] of what
    /// was put in it, and reading it as something else fails the run rather
    /// than reinterpreting the words. That is what makes erasure safe
    /// without the checker having proved anything about it.
    fn unbox(&mut self, value: Val, want: LayoutId, span: Span) -> Val {
        let dst = self.temp(want);
        self.emit(
            Inst::Unbox {
                dst: dst.slot,
                src: value.slot,
                layout: want,
            },
            span,
        );
        self.release(value, span);
        dst
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

    /// The layout of one element of a run-of-elements family.
    ///
    /// `None` for everything else, because everything else is not a run: the
    /// question is asked where a lowering holds a sequence's own layout and
    /// needs the stride, which is the element layout's width.
    fn element_layout(&self, layout: LayoutId) -> Option<LayoutId> {
        match self.pool.shapes.layout(layout).shape {
            crate::layout::Shape::Elements { elem, .. } => Some(elem),
            _ => None,
        }
    }

    /// Whether a value of this layout is one word of scalar bits, which is
    /// what an instruction rather than a walk can compare.
    fn is_scalar(&self, layout: LayoutId) -> bool {
        matches!(
            self.pool.shapes.layout(layout).shape,
            crate::layout::Shape::Word(_)
        )
    }

    /// Whether `layout` is the string family.
    ///
    /// A `String` is the one heap value the language orders: `a < b` on two
    /// of them compares their bytes. Every other heap value the checker
    /// admits an operator on admits only `==` and `!=`.
    fn is_text(&self, layout: LayoutId) -> bool {
        matches!(
            self.pool.shapes.layout(layout).shape,
            crate::layout::Shape::Str
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

    /// The type the checker settled for `expr`, in the terms the declaration
    /// was written in.
    ///
    /// Inside a generic body that is a `Ty::Param`, because the checker
    /// walked the body once with its type parameters rigid. Almost nothing
    /// wants that: [`Body::ty`] is what a lowering asks, and this is for the
    /// two questions that are about the *declaration* rather than about the
    /// value — whether a receiver is a bounded type parameter, and whether an
    /// expression diverges.
    fn raw_ty(&self, expr: &Expr) -> Option<&Ty> {
        self.checked.facts.ty(expr.span.file, expr.id)
    }

    /// The type the checker settled for `expr`, as this instantiation
    /// settles it.
    ///
    /// Owned rather than borrowed because a body of a generic declaration is
    /// lowered once per set of type arguments and the answer is built from
    /// the fact rather than being the fact. For a declaration that binds no
    /// type parameters the substitution is the identity and this is a clone.
    fn ty(&self, expr: &Expr) -> Option<Ty> {
        self.raw_ty(expr).map(|ty| self.complete(ty))
    }

    /// A type written in the declaration's own terms, as this instantiation
    /// settles it.
    fn complete(&self, ty: &Ty) -> Ty {
        ty.instantiate(&self.generics, &self.args)
    }

    /// The same as [`Body::ty`], reporting an expression the checker recorded
    /// nothing for.
    fn settled_ty(&mut self, expr: &Expr) -> Option<Ty> {
        match self.ty(expr) {
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
        let Some(ty) = self.ty(expr) else {
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
    ///
    /// The fact is read as it was recorded. `Ty::Never` holds no type
    /// parameter, so no instantiation can turn one into it or it into
    /// anything else, and this is asked once per stored value.
    fn diverges(&self, expr: &Expr) -> bool {
        matches!(self.raw_ty(expr), Some(Ty::Never))
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

    /// Whether `id` is a declaration this pass lowered, recording it for the
    /// next one when it is not.
    ///
    /// A `false` is not a gap and is deliberately silent: it says the slice
    /// [`lower_entry`] took was too small, which is this crate's mistake to
    /// correct rather than the program's to answer for. The caller emits
    /// nothing naming `id`, the errors of this pass are thrown away, and the
    /// pass after it has the declaration.
    ///
    /// A whole-package lowering never sees one, because nothing is left out.
    fn reached(&mut self, id: FunctionId) -> bool {
        if self.plan.reached(id) {
            return true;
        }
        self.wanted.insert(id);
        false
    }

    /// The source text a span covers, which is what an assertion quotes.
    ///
    /// `?` for a span the map does not hold, exactly as the oracle's own
    /// reader answers, so a message worded here and one worded there cannot
    /// differ even in the case neither expects.
    fn source_text(&self, span: Span) -> &str {
        self.sources
            .files()
            .find(|file| file.id == span.file)
            .and_then(|file| file.text.get(span.start as usize..span.end as usize))
            .unwrap_or("?")
    }

    /// Reports a construct this lowering has not been taught, answering a
    /// location of the right shape so the walk can continue and report the
    /// rest.
    fn gap(&mut self, what: &str, expr: &Expr) -> Val {
        self.errors.push(gap::gap(what, expr.span));
        self.dead(expr)
    }
}
