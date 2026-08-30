//! Lowering a checked program to the executable IR, and the validation that
//! stands between the two.
//!
//! What this lowers is decided by [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md):
//! everything it covers becomes instructions, and everything it does not is
//! named as [`Unsupported`] rather than approximated. A VM that quietly
//! finished a run somewhere else would be a VM whose measurements are about a
//! mixture, so a construct with no lowering stops the lowering and says what
//! it was.
//!
//! # The unit that is lowered is the unit that is run
//!
//! [`lower_entry`] lowers what one entry can reach and nothing else, because
//! an entry is what a run is. Reachability is not derived separately: a body
//! reaches exactly the functions it emits a `Call` to, so numbering a call's
//! target when the call is emitted *is* the closure, and the worklist is
//! empty when nothing new was named.
//!
//! [`lower`] is the same loop seeded with every declaration instead of one,
//! so a whole-package listing and an entry's program are two seeds of one
//! lowering rather than two lowerings that could drift.
//!
//! # An expression is lowered for its value or for its effect
//!
//! `Position` below is the distinction. A statement's value is read by nothing,
//! and `()` is a value here — an assignment, a loop, and an `if` with no
//! `else` all answer one — so lowering every expression the same way builds
//! a `Unit` for a `Pop` to take away again. That was six of the twenty-five
//! instructions `benches/arith` ran per iteration. Lowering for effect emits
//! neither, and reaches inside a block, an `if`/`else`, and a `match` so that
//! the saving is taken where the value would have been built.
//!
//! It changes nothing about what a program means: the value of a block, of an
//! `if` used as an expression, and of a `match` used as an expression are
//! what they were, and only a value nobody reads stops being built.
//! [`validate`]'s depth simulation is what catches a mistake in it.
//!
//! # A settled type is an instruction, and an abstention is not
//!
//! `cove-sema` publishes what it worked out about every expression, and this
//! pass reads it rather than guessing from the shape of the source. Three
//! things follow from it, and nothing else does:
//!
//! - An operator over two operands the checker settled as `Int` lowers to
//!   [`Inst::IntBinary`], which needs no look at what it was handed.
//! - A field of a receiver whose type the checker settled lowers to
//!   [`Inst::GetFieldAt`], which is an index rather than a name to scan for.
//! - A method call the checker recorded a declaration for calls it, so a
//!   name a builtin type and a declared type both answer to is no longer a
//!   refusal.
//!
//! The rule the first two share is that a type must be *settled*.
//! `Ty::Unknown` is the checker saying it did not prove this and no fact at
//! all is the expression never having been walked; neither is `Int`, and
//! both lower to the untyped instruction. Specialising on either would be
//! this pass deciding something the checker declined to, which is the one
//! thing ADR 0019 says a lowering does not do.
//!
//! # A settled type is also where the value is kept
//!
//! The same rule, asked of a binding rather than of an operator, decides
//! which of the VM's two stacks its slot lives in. A local declared from
//! something the checker settled as `Int` or `Bool` is an `i64` in the
//! scalar stack — [`SlotKind::Scalar`] — and everything else is the `Value`
//! it always was. It is one rule and not two: `Body::scalar_of` is
//! `Body::is_int` asked about both scalar types, and an abstention answers
//! both the same way.
//!
//! [`Inst::IntBinary`] reads and writes that stack, because two `i64` in and
//! one out is the whole of what it does, and [`Inst::ScalarConst`],
//! [`Inst::LoadScalar`], [`Inst::StoreScalar`] and
//! [`Inst::JumpIfFalseScalar`] are what let a loop over integers stay in it.
//! [`Inst::ScalarToValue`] and [`Inst::ValueToScalar`] are the boundary, and
//! the lowering spends one only where an expression really does cross:
//! `Body::on_scalar_stack` is what keeps a condition the value stack
//! computed from being moved across just to be tested.
//!
//! # A signature is where the value is kept too
//!
//! The same rule again, asked of a declaration's boundary rather than of a
//! binding, decides the calling convention. A parameter the checker settled
//! as `Int` or `Bool` is a scalar slot, so its argument is pushed onto the
//! scalar stack and *becomes* that slot without moving, exactly as a value
//! argument becomes a value slot; and a function whose return type the
//! checker settled leaves its answer on the scalar stack and ends in
//! [`Inst::ReturnScalar`]. [`Function::params`] and [`Function::returns`]
//! are that convention written down, and `validate` is where a call and its
//! callee are made to agree about it.
//!
//! It is read from `Facts::signature` rather than derived from the
//! annotations here, for the reason everything else is: two readings that
//! could disagree is what `Facts` exists to prevent. A declaration the
//! checker recorded nothing for keeps the convention every function had
//! before — every argument on the value stack, the answer on the value
//! stack — because an abstention is not a settled type here either.
//!
//! What is still deliberately not scalar is a struct's field, which is not a
//! slot at all.
//!
//! # What the interpreter decides and this reproduces
//!
//! `crates/cove-runtime/src/interp.rs` is the oracle, and seven of its rules
//! are most of the difficulty here:
//!
//! - **A name resolves in declaration order.** A reference written before a
//!   `let` in the same block does not see it, so a `let`'s value is lowered
//!   *before* its name is declared and `let x = x` reads the outer `x`.
//! - **Shadowing makes a new slot.** `Env::declare` pushes; it never
//!   overwrites. Two `let x`s are two slots, and a reference reaches the
//!   later one because a lookup scans from the top.
//! - **A block's slots are released when the block ends**, so a later sibling
//!   block reuses the same numbers and each of `value_frame_size` and
//!   `scalar_frame_size` is a high-water mark rather than a count of
//!   declarations.
//! - **A `for` binding lives in the scope its body sees**, and the iterable
//!   is evaluated in the enclosing one.
//! - **Evaluation is left to right everywhere**: arguments, operands, array
//!   elements, and struct fields.
//! - **A struct's fields are pushed in declaration order.** A call whose
//!   labels stand in declaration order fills the parameters in increasing
//!   order, which is what makes pushing the arguments left to right the same
//!   as pushing them in declaration order. `cove-sema` is what holds a
//!   program to that (ADR 0021); `arguments_in_order` below states the same
//!   rule as this pass's own invariant, because it is what the calling
//!   convention is built on and a lowering that assumed it silently would be
//!   assuming it.
//! - **A default argument is evaluated by the callee**, in an environment
//!   holding the parameters declared before it. `bind_params` walks the
//!   parameters in order and reaches `None => match &param.default` inside
//!   the frame it is filling, so a default may read an earlier parameter and
//!   cannot read a later one. A call that leaves a parameter out therefore
//!   reaches a *specialisation*: an ordinary function whose arity is what
//!   that call site supplies and whose prologue computes the rest, which is
//!   what `Instance` below is the key of.
//! - **A `match` arm is a scope, and the first that matches is the only one
//!   that runs.** `match_pattern` tests and binds as it walks, and the arm
//!   that does not match releases what it bound — so an arm's slots behave
//!   the way a block's do, and a subject no arm covers stops the run.
//!
//! # What is not lowered
//!
//! A `snapshot` a declared conformance would have to answer from inside a
//! container, a task scope in a function that answers on the scalar stack, a
//! `lock` whose closure is not written at the call, assignment to a field of
//! anything but a local, and any call whose callee is neither a name nor a
//! field of one. Each is reported in the words a Cove programmer writes it
//! in.
//!
//! # What is refused because the program is wrong
//!
//! Two of the refusals are not about this pass being unfinished. A write to
//! a `let` binding, and a method call by a name whose answer nothing has
//! settled, are reported because the alternative is a backend that accepts
//! what the oracle refuses or that guesses which of two targets was meant.
//! ADR 0012 ranks the oracle above a backend, so refusing to lower is the
//! answer and approximating is not.
//!
//! The second of those two is now narrow. A call the checker recorded a
//! declaration for is that declaration's, so a name two types share stops
//! being ambiguous the moment the receiver's type is known; what is left is
//! a call the checker recorded nothing for, where a name is still all there
//! is.

mod fuel;
mod scan;
mod validate;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use cove_diag::FileId;
use cove_diag::Span;
use cove_schema::builtins;
use cove_schema::hosts;
use cove_schema::TypeSchema;
use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;
use cove_sema::MethodTarget;
use cove_sema::Signature;
use cove_syntax::ast::{
    Arg, BinaryOp as SourceBinary, Block, EnumDecl, Expr, ExprId, ExprKind, FnDecl, GenericParam,
    ItemKind, MatchArm, Param, Pattern, PatternKind, Stmt, StmtKind, StrPart, StructDecl, Type,
    TypeKind, UnaryOp as SourceUnary,
};

use crate::{
    BinaryOp, Const, ConstId, Dispatch, DispatchId, Function, FunctionId, Inst, IntOp, Program,
    Scalar, SlotKind, UnaryOp, Unsupported,
};

use scan::{mentioned_names, var_argument_roots};
use validate::{stack_shape, Shape};

pub use fuel::block_fuel;
pub use validate::validate;

/// A lowered program and the function to start it at.
///
/// The id is here because the lowering already knows it — the entry is the
/// first function it numbers — and a caller that looked it up again by name
/// would be asking a question this pass has already answered.
#[derive(Debug)]
pub struct Lowered {
    pub program: Program,
    pub entry: FunctionId,
}

/// Lowers what the entry `module.name` can reach, and nothing else.
///
/// The unit being run is an entry, so the unit being lowered is an entry.
/// A construct the lowering does not cover refuses this program only if the
/// entry can reach it: a closure in a module this entry neither imports nor
/// calls is not part of the program this entry is, and refusing for it would
/// be refusing for a run that cannot happen.
///
/// What it *can* reach is what the lowering emits. A body reaches exactly
/// the functions it emits a [`Inst::Call`] to, so the closure needs no
/// separate pass: the entry is numbered, its body is lowered, every call
/// numbers a target that was not numbered yet, and the work ends when a body
/// names nothing new. Recursion and a cycle of mutual recursion end there
/// too, because a declaration is numbered once.
///
/// A name this package does not declare is reported rather than panicked on,
/// since the caller that chose it — a `[run.<name>]` table — is a file a
/// person edits.
pub fn lower_entry(checked: &Checked, module: &str, name: &str) -> Result<Lowered, Unsupported> {
    let mut lowering = Lowering::index(checked);
    let Some(key) = lowering.entry_point(module, name) else {
        return Err(Unsupported::new(
            format!("`{module}.{name}`, which this package does not declare"),
            // A name that was looked for and not found has no declaration to
            // underline, and inventing one would point a reader at source
            // that has nothing to do with it.
            Span::new(FileId(0), 0, 0),
        ));
    };
    let entry = lowering.number(Instance::whole(
        key,
        lowering.declaration(key).decl.params.len(),
    ));
    Ok(Lowered {
        program: lowering.reachable()?,
        entry,
    })
}

/// Lowers every function of a checked program.
///
/// This is [`lower_entry`]'s loop seeded with every declaration rather than
/// with one, so there is a single lowering and a whole-package listing is
/// what it produces when nothing is left out. Seeding numbers everything
/// before any body is lowered, so a call reaches a declaration written later
/// in the package and a function reaches itself. The order is the checker's
/// own — modules by name, then free functions by name, then methods by type
/// and name — which is what makes a listing stable enough for a golden test.
///
/// One unsupported construct anywhere fails the whole program, which is what
/// a whole-package listing means: everything the package declares is part of
/// it, whether or not an entry reaches it.
pub fn lower(program: &Checked) -> Result<Program, Unsupported> {
    let mut lowering = Lowering::index(program);
    for index in 0..lowering.catalog.len() {
        let key = Key(index);
        lowering.number(Instance::whole(
            key,
            lowering.declaration(key).decl.params.len(),
        ));
    }
    lowering.reachable()
}

/// Which modules each module of the package can reach, itself included.
///
/// A `use` is the only way one module's declarations become another's, so
/// the transitive closure of `use` is the whole of what a module can name,
/// and the whole of what can be handed to it by anything it names.
fn visibility(checked: &Checked) -> BTreeMap<String, BTreeSet<String>> {
    let mut visible = BTreeMap::new();
    for module in checked.modules.keys() {
        let mut reached = BTreeSet::from([module.clone()]);
        let mut pending = vec![module.clone()];
        while let Some(next) = pending.pop() {
            let Some(resolved) = checked.modules.get(&next) else {
                continue;
            };
            for owner in resolved
                .imports
                .values()
                .chain(resolved.module_imports.values())
            {
                if reached.insert(owner.clone()) {
                    pending.push(owner.clone());
                }
            }
        }
        visible.insert(module.clone(), reached);
    }
    visible
}

/// One function the package declares, and what the lowering emits it from.
struct Declared<'a> {
    /// The module whose body runs it. A method belongs to the module that
    /// declares its `impl` block, which ADR 0006 lets differ from the module
    /// that declares the type.
    module: &'a str,
    /// The name a listing shows: `Type.method` for a method, so that a
    /// method and a free function of one name stay two functions.
    name: String,
    /// The type a method is declared on, and nothing for a free function.
    ///
    /// Kept apart from `name` because ADR 0006 lets a conformance put a
    /// method in the module that declares the *trait*, so the module a
    /// method belongs to and the module its receiver's type belongs to are
    /// two different questions.
    type_name: Option<&'a str>,
    /// The trait whose default body this method runs, for a method a
    /// conformance did not write.
    ///
    /// `check_conformance` materialises a trait's defaulted method as the
    /// type's own, with the trait's body — so the declaration is an ordinary
    /// one, and the only thing that distinguishes it is that its `self` is
    /// the rigid `Self` the checker bounded by the trait rather than the
    /// concrete type. That bound is not written anywhere in the declaration,
    /// so it is carried here; [`Body::bound_of`] is what reads it.
    from_trait_default: Option<&'a str>,
    decl: &'a FnDecl,
}

/// Addresses a declaration of the package, reached or not.
///
/// A lookup answers with one of these rather than with a [`FunctionId`],
/// because finding a declaration and lowering it are two different events:
/// an id is what a lowered function is addressed by, and only a call that is
/// actually emitted earns its target one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key(usize);

/// What one lowered function is lowered from.
///
/// Two things can be, and they are numbered in one space because a
/// [`FunctionId`] names one of them and a caller does not care which:
/// a declaration together with the parameters a call site supplied for it,
/// and a lambda together with the captures the body that wrote it handed
/// over.
///
/// # Why a declaration is not enough by itself
///
/// A default argument is evaluated by the *callee* — `bind_params` reaches
/// `None => match &param.default` inside the frame it is filling — so a call
/// that leaves one out is not a call with fewer arguments to the same
/// function. It is a call to a function whose prologue computes the rest.
///
/// The supplied arguments are not a prefix either: `measure(3, prefix: "d")`
/// skips the middle parameter, so a count would not say which of them
/// arrived. That is why this is the thing that gets numbered rather than
/// [`Key`]: each distinct supplied-set becomes an ordinary [`Function`] whose
/// arity is what that call site passes, and the calling convention is
/// untouched, because a specialisation numbers the supplied parameters' slots
/// first and its defaulted ones after them.
///
/// Two call sites that supply the same parameters share one specialisation,
/// which is what keeps the worklist finite: a package declares finitely many
/// parameters, so it has finitely many supplied-sets.
///
/// # Why a lambda is numbered by where it was written
///
/// A lambda has no declaration to catalogue, so [`Lowering::lambdas`] is its
/// catalogue and this holds an index into it. One entry per *written*
/// lambda, keyed by the expression that wrote it, because two
/// specialisations of the enclosing function reach the same lambda with the
/// same names live and therefore hand it the same captures.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Instance {
    Declared {
        key: Key,
        /// One entry per declared parameter, in declaration order: whether
        /// the call site handed this parameter an argument.
        ///
        /// A variadic parameter is always supplied, because a call site
        /// collects whatever is left over into the one `Array` it receives
        /// and an empty `Array` is an argument like any other.
        supplied: Vec<bool>,
        /// Whether this is the specialisation reached under the
        /// value-stack convention.
        ///
        /// A declared function used as a value is called through
        /// [`Inst::CallValue`], which knows nothing about its target, so
        /// every argument arrives on the value stack and the answer comes
        /// back on it. That is a different convention from the one the
        /// declaration's own signature names, and a convention is what a
        /// slot number means — so it is a different function, numbered
        /// beside the one a direct call reaches rather than replacing it.
        ///
        /// A trait method reached through [`Inst::CallDyn`] is the second
        /// road to the same place, and it is the same flag rather than one
        /// of its own because it is the same convention and the same
        /// reason for it: the call site cannot know which implementation it
        /// will enter, so it cannot have placed its arguments by any one of
        /// their signatures. A method a closure and a dynamic dispatch both
        /// reach is therefore one function.
        ///
        /// A body under this convention is lowered the way a body whose
        /// bindings the checker abstained about is: an `Int` parameter is a
        /// value slot, and `Body::expr_scalar` moves it across where
        /// arithmetic wants it. The same thing `Body::rooted` already does
        /// to a binding a place is rooted at, for the same reason — both
        /// representations hold the same value.
        as_value: bool,
    },
    /// An index into [`Lowering::lambdas`].
    Lambda(usize),
}

impl Instance {
    /// The instance every parameter is handed an argument, which is what a
    /// whole-package lowering seeds and what a call that omits nothing
    /// reaches.
    fn whole(key: Key, params: usize) -> Instance {
        Instance::Declared {
            key,
            supplied: vec![true; params],
            as_value: false,
        }
    }

    /// The instance a dynamic dispatch reaches: every parameter supplied,
    /// under the value-stack convention.
    ///
    /// The same convention a closure is called under, for the same reason
    /// and reached by a second road. Nothing at a `Inst::CallDyn` knows
    /// which implementation it will enter, so the call cannot have placed
    /// its arguments by any one candidate's signature; every argument
    /// travels on the value stack and the answer comes back on it. A
    /// declaration both roads reach is one function, because this is the
    /// same key.
    fn dynamic(key: Key, params: usize) -> Instance {
        Instance::Declared {
            key,
            supplied: vec![true; params],
            as_value: true,
        }
    }
}

/// One lambda the lowering has reached, and what the body that wrote it
/// handed over.
///
/// The captures are settled *here*, by the enclosing body, before this
/// lambda's own instructions exist. That is the whole difference from the
/// interpreter: `Env::captures` builds the list as the closure is created,
/// so a capture's position is a run-time fact there, and it is a fact about
/// the lowering here. The set is the same set — the names the body mentions,
/// intersected with what was live, one entry per name, outermost first — and
/// [`mentioned_names`] is the interpreter's `mention_block` read at lowering
/// time.
struct LambdaSite<'a> {
    /// The module the lambda's body resolves names in, which is the module
    /// the lambda is written in.
    module: &'a str,
    params: &'a [Param],
    body: &'a Block,
    span: Span,
    /// The names this lambda captures, in the order their values are pushed
    /// before [`Inst::MakeClosure`] and in the order its
    /// [`Inst::LoadCapture`] indices address.
    captures: Vec<&'a str>,
    /// Whether this lambda was written `async`, and so answers a settled
    /// task rather than the value its body produced.
    is_async: bool,
    /// Whether this lambda's first parameter, if it is written `var`, names
    /// storage the caller holds rather than receiving a copy of it.
    ///
    /// True for the one closure that is written that way and called that
    /// way: the one `Shared::lock` is given. A `var` parameter is otherwise
    /// refused on a lambda, because every argument of an
    /// [`Inst::CallValue`] travels on the value stack and a place cannot —
    /// and [`Inst::Lock`] is the instruction that does not go through one.
    aliases_first_param: bool,
}

/// The whole-program state one lowering carries: what the package declares,
/// which of it has been reached, and the constants the reached share.
struct Lowering<'a> {
    checked: &'a Checked,
    /// Every function the package declares, in the checker's order, whether
    /// or not this lowering will emit any of them.
    catalog: Vec<Declared<'a>>,
    /// The id each specialisation was given, once something reached it.
    numbered: BTreeMap<Instance, FunctionId>,
    /// The specialisation each id names, in the order the ids were handed
    /// out.
    ///
    /// This is the worklist as much as the table: a specialisation is
    /// appended when it is first reached, and the lowering walks the vector
    /// from the front until it stops growing.
    reached: Vec<Instance>,
    /// Free functions, by the module that declares them and their name.
    functions: BTreeMap<(String, String), Key>,
    /// Methods, by the module that declares the `impl` block, the type, and
    /// the method name.
    methods: BTreeMap<(String, String, String), Key>,
    /// Every method a name answers to, for a receiver whose type the
    /// lowering has no way to name.
    ///
    /// Every one the package declares. Which of them a given call site could
    /// actually reach is [`Lowering::could_dispatch`]'s question, asked
    /// against the module the call is written in.
    by_name: BTreeMap<String, Vec<Key>>,
    /// The modules each module can reach through `use`, transitively, and
    /// itself.
    ///
    /// A type travels only along `use` edges — a value of it is obtained by
    /// naming something that produces one — so this bounds which types a
    /// value written in a module can have.
    visible: BTreeMap<String, BTreeSet<String>>,
    /// Every lambda some body has reached, in the order they were reached,
    /// which is what [`Instance::Lambda`] indexes.
    ///
    /// A lambda has no declaration, so there is nothing to catalogue it with
    /// ahead of time the way [`Lowering::catalog`] catalogues declarations:
    /// a lambda becomes part of the program at the moment a body lowers the
    /// expression that writes it, and that is the moment its captures are
    /// known.
    lambdas: Vec<LambdaSite<'a>>,
    /// The entry each written lambda was catalogued as, so that one written
    /// lambda is one function however many times a body is lowered.
    ///
    /// Keyed by file and expression id, because an [`ExprId`] is unique
    /// within the file it was parsed from and a package has many files.
    lambda_of: BTreeMap<(FileId, ExprId), usize>,
    constants: Vec<Const>,
    /// Every dynamic dispatch site some body has reached, in the order they
    /// were reached, which is what [`DispatchId`] indexes.
    ///
    /// One entry per `(trait, method)` pair rather than per call site: the
    /// implementations a call can reach are a fact about the pair, and two
    /// calls to `label()` on a `dyn Display` reach the same set.
    dispatches: Vec<Dispatch>,
}

impl<'a> Lowering<'a> {
    /// Catalogues every declared function without numbering or lowering any
    /// of them.
    ///
    /// Cataloguing is what makes a name answerable; numbering is what makes
    /// a function part of the program being lowered, and [`Lowering::number`]
    /// is the only thing that does it.
    fn index(checked: &'a Checked) -> Lowering<'a> {
        let mut lowering = Lowering {
            checked,
            catalog: Vec::new(),
            numbered: BTreeMap::new(),
            reached: Vec::new(),
            functions: BTreeMap::new(),
            methods: BTreeMap::new(),
            by_name: BTreeMap::new(),
            visible: visibility(checked),
            lambdas: Vec::new(),
            lambda_of: BTreeMap::new(),
            constants: Vec::new(),
            dispatches: Vec::new(),
        };
        for (module, resolved) in &checked.modules {
            for (name, entry) in &resolved.functions {
                let key = lowering.catalogue(Declared {
                    module,
                    name: name.clone(),
                    type_name: None,
                    from_trait_default: None,
                    decl: &entry.decl,
                });
                lowering
                    .functions
                    .insert((module.clone(), name.clone()), key);
            }
            for ((type_name, method), entry) in &resolved.methods {
                let key = lowering.catalogue(Declared {
                    module,
                    name: format!("{type_name}.{method}"),
                    type_name: Some(type_name.as_str()),
                    from_trait_default: entry.from_trait_default.as_deref(),
                    decl: &entry.decl,
                });
                lowering
                    .methods
                    .insert((module.clone(), type_name.clone(), method.clone()), key);
                lowering
                    .by_name
                    .entry(method.clone())
                    .or_default()
                    .push(key);
            }
        }
        lowering
    }

    fn catalogue(&mut self, declared: Declared<'a>) -> Key {
        self.catalog.push(declared);
        Key(self.catalog.len() - 1)
    }

    /// The id `instance` has, handing one out and queuing the
    /// specialisation when this is the first thing to reach it.
    ///
    /// Numbering once is what ends the walk: a function that calls itself,
    /// and a cycle of functions that call each other, are each already
    /// numbered by the time the call that closes the loop is emitted. A
    /// recursive call that supplies a different set of parameters numbers a
    /// second specialisation, and that walk ends for the same reason — there
    /// are only so many sets.
    fn number(&mut self, instance: Instance) -> FunctionId {
        if let Some(id) = self.numbered.get(&instance) {
            return *id;
        }
        let id = FunctionId(self.reached.len() as u32);
        self.numbered.insert(instance.clone(), id);
        self.reached.push(instance);
        id
    }

    /// What `key` names.
    fn declaration(&self, key: Key) -> &Declared<'a> {
        &self.catalog[key.0]
    }

    /// The boundary the checker resolved for `key`, keyed by the
    /// declaration's own span.
    ///
    /// `None` is the checker having recorded nothing about this
    /// declaration, which a checked program does not produce. The lowering
    /// does not guess when it happens: see [`Lowering::function`], where the
    /// fallback is written down.
    fn signature(&self, key: Key) -> Option<&'a Signature> {
        let decl = self.declaration(key).decl;
        self.checked.facts.signature(decl.span.file, decl.span)
    }

    /// Whether a method call written in `from` could reach the method `key`.
    ///
    /// A receiver is a value, and a value's type came from `from` or from
    /// somewhere `from` reaches through `use`, so a method of a type no
    /// chain of imports brings here is not an answer this call site has.
    /// Asking is what keeps a method a package declares far away — in
    /// another program of the same package — from making every call of that
    /// name ambiguous.
    ///
    /// Either module counts: the one that declares the `impl` block, and the
    /// one that declares the type, which ADR 0006's orphan rule lets differ.
    /// A conformance written beside the trait is still reached through a
    /// receiver whose type came from wherever the type is declared.
    fn could_dispatch(&self, from: &str, key: Key) -> bool {
        let Some(visible) = self.visible.get(from) else {
            // A module the checker does not know is not a module this
            // lowering can bound, so nothing is ruled out.
            return true;
        };
        let declared = self.declaration(key);
        if visible.contains(declared.module) {
            return true;
        }
        let Some(type_name) = declared.type_name else {
            return false;
        };
        visible.iter().any(|module| {
            self.checked.modules.get(module).is_some_and(|resolved| {
                resolved.structs.contains_key(type_name) || resolved.enums.contains_key(type_name)
            })
        })
    }

    /// The declaration a `[run.<name>]` table's `module.name` selects.
    ///
    /// An entry is a free function of a named module and nothing else, so
    /// this asks the one table that holds those rather than going through
    /// the import-aware lookups a *body* uses: the entry is not written
    /// inside any module, so there is no module whose `use` declarations it
    /// could be read against.
    fn entry_point(&self, module: &str, name: &str) -> Option<Key> {
        self.functions
            .get(&(module.to_string(), name.to_string()))
            .copied()
    }

    /// Lowers everything numbered, and everything that lowering numbers.
    ///
    /// The ids are handed out in the order the declarations were reached, so
    /// walking them in order is walking the worklist in the order it grew,
    /// and the loop ends when a pass over the last body added nothing.
    fn reachable(mut self) -> Result<Program, Unsupported> {
        let mut functions = Vec::with_capacity(self.reached.len());
        while functions.len() < self.reached.len() {
            functions.push(self.function(FunctionId(functions.len() as u32))?);
        }
        Ok(Program {
            functions,
            constants: self.constants,
            dispatches: self.dispatches,
        })
    }

    /// Interns a constant, so that one value is one [`ConstId`] however many
    /// instructions load it.
    fn constant(&mut self, value: Const) -> ConstId {
        match self.constants.iter().position(|held| *held == value) {
            Some(index) => ConstId(index as u32),
            None => {
                self.constants.push(value);
                ConstId(self.constants.len() as u32 - 1)
            }
        }
    }

    /// Interns a name an instruction carries: a field, a host module, a host
    /// operation, a builtin, or a type.
    fn name(&mut self, text: &str) -> ConstId {
        self.constant(Const::Name(text.into()))
    }

    /// The function `module` reaches by the bare name `name`: its own
    /// declaration first, and the one a `use` imported under that name
    /// second, exactly as `Interpreter::find_function` does.
    fn function_of(&self, module: &str, name: &str) -> Option<Key> {
        if let Some(key) = self.functions.get(&(module.to_string(), name.to_string())) {
            return Some(*key);
        }
        let owner = self.checked.modules.get(module)?.imports.get(name)?;
        self.functions
            .get(&(owner.clone(), name.to_string()))
            .copied()
    }

    /// The struct `module` reaches by the bare name `name`, and the module
    /// that declares it.
    fn struct_of(&self, module: &str, name: &str) -> Option<(&'a str, &'a StructDecl)> {
        let (module, resolved) = self.checked.modules.get_key_value(module)?;
        if let Some(entry) = resolved.structs.get(name) {
            return Some((module.as_str(), &entry.decl));
        }
        let owner = resolved.imports.get(name)?;
        let (owner, resolved) = self.checked.modules.get_key_value(owner)?;
        Some((owner.as_str(), &resolved.structs.get(name)?.decl))
    }

    /// The enum `module` reaches by the bare name `name`, and the module
    /// that declares it.
    ///
    /// The declaring module is half the answer: a case carries the qualified
    /// type name of the enum it belongs to, and two modules may each declare
    /// a `Status`, so a value has to say which one it is.
    fn enum_of(&self, module: &str, name: &str) -> Option<(&'a str, &'a EnumDecl)> {
        let (module, resolved) = self.checked.modules.get_key_value(module)?;
        if let Some(entry) = resolved.enums.get(name) {
            return Some((module.as_str(), &entry.decl));
        }
        let owner = resolved.imports.get(name)?;
        let (owner, resolved) = self.checked.modules.get_key_value(owner)?;
        Some((owner.as_str(), &resolved.enums.get(name)?.decl))
    }

    /// Whether `module` reaches an enum by the bare name `name`.
    fn declares_enum(&self, module: &str, name: &str) -> bool {
        self.enum_of(module, name).is_some()
    }

    /// The method of `type_module.type_name` named `name`.
    ///
    /// A type's methods usually live with the type; ADR 0006's orphan rule
    /// lets a conformance put one in the module that declares the trait
    /// instead, so the conformances are searched second.
    fn method_of(&self, type_module: &str, type_name: &str, name: &str) -> Option<Key> {
        let declared = (
            type_module.to_string(),
            type_name.to_string(),
            name.to_string(),
        );
        if let Some(key) = self.methods.get(&declared) {
            return Some(*key);
        }
        self.checked.modules.iter().find_map(|(module, resolved)| {
            let conforms = resolved.conformances.values().any(|conformance| {
                conformance.type_module == type_module
                    && conformance.type_name == type_name
                    && conformance.methods.contains(name)
            });
            if !conforms {
                return None;
            }
            self.methods
                .get(&(module.clone(), type_name.to_string(), name.to_string()))
                .copied()
        })
    }

    /// The name a `dyn` value built in `module` carries.
    ///
    /// `Interpreter::declaring_module` asked before the run instead of
    /// during it. A trait belongs to the module that declares it, which may
    /// be one this module imported the trait from, and a `dyn` value built
    /// here must carry the same name a value built there does, or the two
    /// would not compare equal. A name no module declares as a trait is left
    /// bare, which is what that function's `None` leaves too.
    fn trait_named(&self, module: &str, name: &str) -> Arc<str> {
        let Some(resolved) = self.checked.modules.get(module) else {
            return name.into();
        };
        if resolved.traits.contains_key(name) {
            return format!("{module}.{name}").into();
        }
        match resolved.imports.get(name) {
            Some(owner)
                if self
                    .checked
                    .modules
                    .get(owner)
                    .is_some_and(|owner| owner.traits.contains_key(name)) =>
            {
                format!("{owner}.{name}").into()
            }
            _ => name.into(),
        }
    }

    /// The conversion a type written in `module` asks for: the qualified
    /// trait a `dyn` inside it names, and how many `Array` or `Option`
    /// layers stand between the value and that `dyn`.
    ///
    /// This is the walk `Interpreter::coerce` makes over the written type,
    /// made once here instead of once per conversion there. It reaches into
    /// `Array<T>` and `Option<T>` and nothing else, for the reason that
    /// function gives: those are the forms whose elements are written as
    /// `dyn` too, and a `Vector` is a shared handle whose elements cannot be
    /// rewritten behind its other aliases.
    ///
    /// `None` is two different things, and the caller tells them apart with
    /// [`mentions_dyn`]: a type with no `dyn` in it at all needs no
    /// conversion, and a type that mentions one somewhere this walk does not
    /// reach — `Map<String, dyn Display>`, a written function type — is
    /// refused, because converting it would be this pass deciding something
    /// the oracle does not do.
    fn dyn_conversion(&self, module: &str, ty: &Type) -> Option<(Arc<str>, u16)> {
        let (name, depth) = dyn_shape(ty)?;
        Some((self.trait_named(module, name), depth))
    }

    /// The dispatch site a call to `method` through `dyn trait_name`
    /// reaches, numbering one the first time such a call is lowered.
    ///
    /// The candidates are every conformance to that trait the *package*
    /// declares, and deliberately not the ones the calling module can see:
    /// see [`Dispatch`] for why a bound would leave out the case dynamic
    /// dispatch exists for. Each is numbered as a specialisation under the
    /// value-stack convention, exactly as a declared function used as a
    /// value is, because nothing at the call site knows which of them it
    /// will reach.
    ///
    /// One site per `(trait, method)` pair, so two calls to `label()` on a
    /// `dyn Display` share it: the implementations are a fact about the
    /// pair, and rebuilding the list per call site would be the same answer
    /// written twice.
    fn dispatch_site(&mut self, trait_name: &str, method: &str) -> DispatchId {
        if let Some(index) = self
            .dispatches
            .iter()
            .position(|site| &*site.trait_name == trait_name && &*site.method == method)
        {
            return DispatchId(index as u32);
        }
        // Numbered before the candidates are, so that a trait method that
        // dispatches on its own receiver — a default body calling another of
        // the trait's methods — finds this site already there rather than
        // numbering a second one.
        let id = DispatchId(self.dispatches.len() as u32);
        self.dispatches.push(Dispatch {
            trait_name: trait_name.into(),
            method: method.into(),
            cases: Vec::new(),
        });
        let mut implementors: Vec<(String, String)> = Vec::new();
        for resolved in self.checked.modules.values() {
            for conformance in resolved.conformances.values() {
                let qualified = format!("{}.{}", conformance.trait_module, conformance.trait_name);
                if qualified == trait_name && conformance.methods.contains(method) {
                    implementors.push((
                        conformance.type_module.clone(),
                        conformance.type_name.clone(),
                    ));
                }
            }
        }
        // A type conforms to a trait once, so this is a sort for
        // determinism rather than a deduplication: the listing a golden test
        // reads has to be the same list every run, and the modules were
        // walked in name order but the types inside them were not.
        implementors.sort();
        implementors.dedup();
        let mut cases: Vec<(Arc<str>, FunctionId)> = Vec::new();
        for (type_module, type_name) in implementors {
            // `method_of` is `Interpreter::find_method`: the type's own
            // module first, and the module that declares the conformance
            // second. A conformance whose method this pass cannot find is
            // one the run could not have called either.
            let Some(key) = self.method_of(&type_module, &type_name, method) else {
                continue;
            };
            let params = self.declaration(key).decl.params.len();
            let function = self.number(Instance::dynamic(key, params));
            cases.push((format!("{type_module}.{type_name}").into(), function));
        }
        self.dispatches[id.0 as usize].cases = cases;
        id
    }

    /// Whether `name` is a host module `module` may address.
    ///
    /// A `use` makes one addressable by name, and a shipped module is
    /// addressable anyway, which is what `Interpreter::is_host_module` asks
    /// the registry.
    fn is_host_module(&self, module: &str, name: &str) -> bool {
        self.checked
            .modules
            .get(module)
            .is_some_and(|resolved| resolved.host_uses.contains(name))
            || hosts::module(name).is_some()
    }

    /// The host module an unqualified `use console.println` binds `name` to.
    fn host_item(&self, module: &str, name: &str) -> Option<&'a str> {
        Some(
            self.checked
                .modules
                .get(module)?
                .host_items
                .get(name)?
                .as_str(),
        )
    }

    /// The module `head` names in `module`, when a `use` imported it whole.
    fn imported_module(&self, module: &str, head: &str) -> Option<&'a str> {
        Some(
            self.checked
                .modules
                .get(module)?
                .module_imports
                .get(head)?
                .as_str(),
        )
    }

    /// The exported function `owner.name`, when `owner` exports one.
    fn exported_function(&self, owner: &str, name: &str) -> Option<Key> {
        if self.checked.modules.get(owner)?.exported(name) != Some(true) {
            return None;
        }
        self.functions
            .get(&(owner.to_string(), name.to_string()))
            .copied()
    }

    /// The exported struct `owner.name`, when `owner` exports one.
    fn exported_struct(&self, owner: &str, name: &str) -> Option<&'a StructDecl> {
        let resolved = self.checked.modules.get(owner)?;
        if resolved.exported(name) != Some(true) {
            return None;
        }
        Some(&resolved.structs.get(name)?.decl)
    }

    /// The id the lambda `expr` writes, catalogued with `captures` the first
    /// time something reaches it.
    ///
    /// One written lambda is one function. Two specialisations of the
    /// enclosing declaration reach it with the same names live — a parameter
    /// left to a default is still declared, only computed by the prologue —
    /// so the capture list is the same list, and numbering it twice would be
    /// two functions with one meaning.
    fn number_lambda(&mut self, site: LambdaSite<'a>, at: (FileId, ExprId)) -> FunctionId {
        if let Some(index) = self.lambda_of.get(&at) {
            return self.number(Instance::Lambda(*index));
        }
        let index = self.lambdas.len();
        self.lambdas.push(site);
        self.lambda_of.insert(at, index);
        self.number(Instance::Lambda(index))
    }

    /// Lowers one function into its instructions.
    fn function(&mut self, id: FunctionId) -> Result<Function, Unsupported> {
        match self.reached[id.0 as usize].clone() {
            Instance::Declared {
                key,
                supplied,
                as_value,
            } => self.declared_function(key, &supplied, as_value),
            Instance::Lambda(index) => self.lambda_function(index),
        }
    }

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
    fn lambda_function(&mut self, index: usize) -> Result<Function, Unsupported> {
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
    fn declared_function(
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
struct Binding<'a> {
    name: Option<&'a str>,
    slot: u32,
    /// Which of this function's captures this binding is, for the bindings
    /// that are one.
    ///
    /// A capture is an ordinary value slot, so this changes nothing about
    /// how it is reached; what it changes is which instruction says so.
    /// [`Inst::LoadCapture`] carries the index into
    /// [`Function::captures`] rather than the slot number the index works
    /// out to, because the layout is a fact about the closure and the
    /// capture list is what states it.
    capture: Option<u32>,
    /// Which stack the slot lives in, decided when it was declared and never
    /// revisited: a binding's type does not change, so neither does where it
    /// is kept.
    kind: SlotKind,
}

/// Where a scope begins: [`Body::scope`] takes one and [`Body::release`]
/// restores it, which is what ends the scope.
///
/// The three slot counters are numbered separately, so ending a scope has to
/// roll all of them back, not just how many bindings are live.
#[derive(Clone, Copy)]
struct Mark {
    live: usize,
    next_value: u32,
    next_scalar: u32,
    next_place: u32,
}

/// A jump target, resolved once the instruction it points at exists.
struct Label {
    at: Option<u32>,
    /// The operand-stack depths control arrives here with, taken from the
    /// first reachable jump that names it.
    depth: Option<Depth>,
}

/// How much stands on each of the three operand stacks.
///
/// Three numbers rather than one because there are three stacks. Every join
/// point has to be arrived at with the same amount on all of them, and
/// `validate` simulates all of them, so tracking one and guessing the rest
/// would be tracking none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Depth {
    values: u32,
    scalars: u32,
    places: u32,
}

impl Depth {
    /// Every stack empty, which is where a body and a loop's operands start.
    const EMPTY: Depth = Depth {
        values: 0,
        scalars: 0,
        places: 0,
    };

    /// The depths after one instruction of this shape has run.
    fn after(self, shape: Shape) -> Depth {
        Depth {
            values: self.values.saturating_sub(shape.values.0) + shape.values.1,
            scalars: self.scalars.saturating_sub(shape.scalars.0) + shape.scalars.1,
            places: self.places.saturating_sub(shape.places.0) + shape.places.1,
        }
    }
}

/// The loop a `break` or a `continue` leaves.
struct LoopFrame {
    break_to: usize,
    continue_to: usize,
    /// How many task scopes were open when the loop began, so that a `break`
    /// or a `continue` written inside one knows how many it is leaving.
    scopes: usize,
    /// The operand-stack depths the loop runs at, which is what a `break`
    /// written inside a half-evaluated expression has to get back down to —
    /// on every stack, because a half-evaluated `a + b` can have left
    /// something on any of them.
    depth: Depth,
}

/// Which kind of `for` header a loop is walking.
#[derive(Clone, Copy)]
enum Header {
    /// `a..b` and `a..<b`: the cursor is the value the binding takes, and
    /// `limit` is the bound it is tested against.
    Range { limit: u32, inclusive: bool },
    /// Anything else: the cursor is an index into `sequence`, whose length
    /// was read once into `length`.
    Sequence { sequence: u32, length: u32 },
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
enum Position {
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
struct Finished {
    code: Vec<Inst>,
    spans: Vec<Span>,
    arg_spans: BTreeMap<u32, Vec<Span>>,
    value_frame_size: u32,
    scalar_frame_size: u32,
    place_frame_size: u32,
}

/// Everything one function's instructions are built from.
struct Body<'a, 'l> {
    outer: &'l mut Lowering<'a>,
    module: &'a str,
    /// Which stack this function's answer travels on, which decides both
    /// which return instruction it ends in and where every `return` inside
    /// it leaves its operand. Read from the declaration's signature once, in
    /// [`Lowering::function`].
    returns: SlotKind,
    /// The conversion this function's written return type asks for, emitted
    /// before every one of its returns.
    ///
    /// `Interpreter::invoke` converts what a body answered against the type
    /// the declaration *wrote*, so the conversion belongs to the callee and
    /// not to the call: a declaration with a `dyn Trait` return type answers
    /// a trait object whichever call site asked. Kept here rather than
    /// re-derived at each `return`, because a body has one return type and a
    /// `return` written inside a `match` arm has no declaration in reach.
    dyn_return: Option<Inst>,
    /// The type parameters this declaration writes, with the traits each is
    /// bounded by.
    ///
    /// A method call on a value whose type is one of them resolves through
    /// its bounds — that is what a bound is written for — so this is what
    /// [`Body::bound_of`] searches. Empty for a lambda, whose body has no
    /// declaration of its own to have written any.
    generics: &'a [GenericParam],
    /// The trait `Self` is bounded by, inside a trait's default body.
    ///
    /// `check_trait_defaults` checks a default body once with `self` typed as
    /// a rigid `Self` bounded by that trait, so a call on `self` there is a
    /// call through a bound like any other — but the parameter is not
    /// written in the declaration, because the declaration is one
    /// `check_conformance` synthesized. This is the bound it would have
    /// written.
    self_bound: Option<&'a str>,
    code: Vec<Inst>,
    spans: Vec<Span>,
    /// The operand-stack depths, or `None` where control cannot arrive.
    ///
    /// `return`, `break`, and `continue` are expressions, so the
    /// instructions written after one are unreachable and have no depth to
    /// speak of. Tracking that rather than guessing is what keeps a later
    /// join point honest.
    depth: Option<Depth>,
    live: Vec<Binding<'a>>,
    /// The high-water mark of value slots handed out: `self` if there is a
    /// receiver, then parameters, then every `Value` local and temporary.
    value_frame_size: u32,
    /// The high-water mark of scalar slots handed out: every `Int` or `Bool`
    /// local and temporary.
    scalar_frame_size: u32,
    /// The high-water mark of place slots handed out, which is every `var`
    /// parameter and a `var self` receiver: nothing a body declares takes
    /// one.
    place_frame_size: u32,
    /// The next value slot number to hand out, restored when a scope ends.
    next_value: u32,
    /// The next scalar slot number to hand out, restored when a scope ends.
    next_scalar: u32,
    /// The next place slot number to hand out, restored when a scope ends.
    next_place: u32,
    labels: Vec<Label>,
    patches: Vec<(usize, usize)>,
    loops: Vec<LoopFrame>,
    /// How many task scopes are open around the instruction being emitted.
    ///
    /// A `break` or a `continue` leaves every scope written between it and
    /// its loop without reaching the `Inst::LeaveScope` below each of them,
    /// so it emits one `Inst::CancelScope` per scope it leaves — which is
    /// this count against the one [`LoopFrame`] recorded. Every other early
    /// exit leaves the frame as well, and the VM cancels what a popped frame
    /// had open for itself; see [`Inst::EnterScope`].
    open_scopes: usize,
    /// The argument spans of the instructions whose diagnostic quotes source,
    /// which today is the two assertions and nothing else.
    arg_spans: BTreeMap<u32, Vec<Span>>,
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
    rooted: BTreeSet<&'a str>,
}

impl<'a, 'l> Body<'a, 'l> {
    fn new(outer: &'l mut Lowering<'a>, module: &'a str) -> Body<'a, 'l> {
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
    fn finish(mut self) -> Finished {
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
    fn emit(&mut self, inst: Inst, span: Span) {
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
    fn emit_final_return(&mut self, span: Span) {
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
    /// `Interpreter::invoke` converts the one value a call answered and does
    /// not ask which `return` produced it.
    fn emit_dyn_return(&mut self, span: Span) {
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
    fn coerce_to(&mut self, module: &str, ty: &Type, span: Span) {
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
    fn coerce_param(
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

    fn constant(&mut self, value: Const, span: Span) {
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
    fn unit_at(&mut self, position: Position, span: Span) {
        match position {
            Position::Effect => {}
            Position::Value => self.constant(Const::Unit, span),
            Position::Scalar => {
                self.constant(Const::Unit, span);
                self.emit(Inst::ValueToScalar, span);
            }
        }
    }

    fn label(&mut self) -> usize {
        self.labels.push(Label {
            at: None,
            depth: None,
        });
        self.labels.len() - 1
    }

    /// Emits a jump to `label`, recording the depth control leaves with.
    fn jump(&mut self, inst: fn(u32) -> Inst, label: usize, span: Span) {
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
    fn bind(&mut self, label: usize) {
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
    fn declare(&mut self, name: Option<&'a str>, kind: SlotKind) -> u32 {
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
    fn allocate(&mut self, kind: SlotKind) -> u32 {
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
    fn declare_at(&mut self, name: Option<&'a str>, kind: SlotKind, slot: u32) {
        self.live.push(Binding {
            name,
            slot,
            capture: None,
            kind,
        });
    }

    /// Lets `name` reach the slot holding capture `index`.
    fn declare_capture_at(&mut self, name: &'a str, index: u32, slot: u32) {
        self.live.push(Binding {
            name: Some(name),
            slot,
            capture: Some(index),
            kind: SlotKind::Value,
        });
    }

    /// Where a scope begins, to be handed back to [`Body::release`] when it
    /// ends.
    fn scope(&self) -> Mark {
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
    fn release(&mut self, mark: Mark) {
        self.live.truncate(mark.live);
        self.next_value = mark.next_value;
        self.next_scalar = mark.next_scalar;
        self.next_place = mark.next_place;
    }

    /// The binding `name` reaches: the most recent declaration of it, because
    /// a lookup scans from the top and a shadow was declared later.
    fn binding(&self, name: &str) -> Option<&Binding<'a>> {
        self.live
            .iter()
            .rev()
            .find(|binding| binding.name == Some(name))
    }

    /// The slot `name` reaches.
    fn lookup(&self, name: &str) -> Option<u32> {
        self.binding(name).map(|binding| binding.slot)
    }

    /// The slot `name` reaches and what it holds, where it is a scalar one.
    ///
    /// `None` for a name that is not a local and for a local kept as a
    /// `Value`, which are the two cases that lower the way they always did.
    fn scalar_binding(&self, name: &str) -> Option<(u32, Scalar)> {
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
    fn is_a_place(&self, expr: &'a Expr) -> bool {
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
    fn rooted_kind(&self, name: &str, kind: SlotKind) -> SlotKind {
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
    fn place(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
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
    fn written_through_a_place(&self, target: &'a Expr) -> bool {
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
    fn settled(&self, expr: &Expr) -> Option<&'a Ty> {
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
    fn resource_op(&self, receiver: &Expr, name: &str) -> bool {
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
    fn is_int(&self, expr: &Expr) -> bool {
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
    fn scalar_of(&self, expr: &Expr) -> Option<Scalar> {
        self.settled(expr).and_then(scalar_of_ty)
    }

    /// Where a binding declared from `expr` lives.
    ///
    /// The same question again, because a binding's storage and an operand's
    /// storage are settled by the same fact: a slot the checker proved holds
    /// an `Int` holds an integer word, and a slot it said nothing about holds
    /// what every slot used to.
    fn slot_kind(&self, expr: &Expr) -> SlotKind {
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
    fn on_scalar_stack(&self, expr: &'a Expr) -> bool {
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
    fn callee_returns(&self, id: ExprId, callee: &'a Expr) -> Option<Scalar> {
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
    fn binary_inst(&self, op: BinaryOp, lhs: &'a Expr, rhs: &'a Expr) -> Inst {
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
    fn field_inst(&mut self, receiver: &'a Expr, name: &str) -> Inst {
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
    fn field_position(&self, receiver: &'a Expr, name: &str) -> Option<u32> {
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
    fn scalar_field(&self, field: &Expr, receiver: &'a Expr, name: &str) -> Option<u32> {
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
    fn target(&self, id: ExprId, span: Span) -> Option<&'a MethodTarget> {
        self.outer.checked.facts.target(span.file, id)
    }

    /// The declaration `target` names, or `None` where this package has none
    /// of that name.
    ///
    /// `None` is not a failure to report. It leaves the call to the
    /// name-based path below, which is where a call the checker said nothing
    /// about goes anyway.
    fn declared_by(&self, target: &MethodTarget) -> Option<Key> {
        self.outer
            .method_of(&target.module, &target.type_name, &target.method)
    }

    // ---------------------------------------------------------- statements

    /// A block, lowered in the position it was written in.
    ///
    /// A block's value is its tail's, so the position is handed to the tail:
    /// lowered for effect a block builds no `Unit` at all, and lowered in
    /// scalar position its tail leaves its value on the scalar stack. Its
    /// statements are unaffected — they were already lowered for their
    /// effect, whichever position the block itself is in.
    ///
    /// The slots the block declared are released at its end, so a later
    /// sibling block reuses the numbers and each frame size stays a
    /// high-water mark rather than a total.
    fn block_at(&mut self, block: &'a Block, position: Position) -> Result<(), Unsupported> {
        let mark = self.scope();
        for statement in &block.statements {
            self.statement(statement)?;
        }
        match &block.tail {
            Some(tail) => self.expr_at(tail, position)?,
            None => self.unit_at(position, block.span),
        }
        self.release(mark);
        Ok(())
    }

    fn statement(&mut self, statement: &'a Stmt) -> Result<(), Unsupported> {
        match &statement.kind {
            StmtKind::Let {
                name, ty, value, ..
            } => {
                if let Some(ty) = ty {
                    reject_dyn(ty, "a `dyn` binding")?;
                }
                // The value is lowered before the name exists, which is what
                // makes `let x = x` read the outer `x`.
                //
                // Where the binding lives is settled by the same fact every
                // typed instruction is settled by: the type the checker gave
                // what it is declared from. An abstention keeps the slot a
                // `Value`, and the whole function then reads as it always did.
                //
                // An annotation that converts settles it instead: what the
                // binding holds is the trait object the conversion makes,
                // which is a `Value` whatever the value it was declared from
                // was. No annotation the checker settles as `Int` or `Bool`
                // can convert, so this only ever moves an answer the way
                // `rooted_kind` does.
                let converts = ty.as_ref().is_some_and(|ty| dyn_shape(ty).is_some());
                let kind = match converts {
                    true => SlotKind::Value,
                    false => self.rooted_kind(name.node.as_str(), self.slot_kind(value)),
                };
                match kind {
                    SlotKind::Scalar(_) => self.expr_scalar(value)?,
                    SlotKind::Value => self.expr(value)?,
                    // `slot_kind` answers about a type and never says
                    // `Place`, and `rooted_kind` only ever moves an answer
                    // towards the value stack.
                    SlotKind::Place => unreachable!("a `let` does not declare a place"),
                }
                if let Some(ty) = ty {
                    // `eval_block_body` converts the value against the
                    // annotation before it declares the name, so this stands
                    // between the value and the store the same way.
                    let module = self.module;
                    self.coerce_to(module, ty, statement.span);
                }
                let slot = self.declare(Some(name.node.as_str()), kind);
                self.emit(store_slot(kind, slot), statement.span);
                Ok(())
            }
            StmtKind::Expr(expr) => {
                // A statement is the one place a value is definitely
                // unwanted, so it is where lowering for effect starts.
                self.effect(expr)?;
                Ok(())
            }
            StmtKind::Item(item) => Err(Unsupported::new(
                match item.kind {
                    ItemKind::Fn(_) => "a function declared inside a function body",
                    _ => "a type declared inside a function body",
                },
                statement.span,
            )),
        }
    }

    // --------------------------------------------------------- expressions

    /// Lowers one expression, which leaves exactly one value on the stack.
    fn expr(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr_at(expr, Position::Value)
    }

    /// Lowers one expression whose value nobody reads, which leaves nothing
    /// on the stack.
    ///
    /// Everything the expression does still happens; only its value goes
    /// missing. See `Position` for why that is worth a second entry point.
    fn effect(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr_at(expr, Position::Effect)
    }

    /// Lowers one expression so that what it computed is on the scalar
    /// stack.
    ///
    /// Called only where [`Body::scalar_of`] settled a type, so what arrives
    /// is what the instruction reading it was promised. An expression the
    /// scalar stack has no instructions for is lowered exactly as it always
    /// was and moved across by one [`Inst::ValueToScalar`] — a boundary
    /// rather than a second lowering of the language.
    ///
    /// The three constructs with an inside are not moved across: a block, an
    /// `if`/`else`, and a `match` hand [`Position::Scalar`] to their tails,
    /// branches, and arms, so that an integer is left where an integer was
    /// wanted rather than built as a `Value` in each branch and unwrapped
    /// again afterwards. That is the same reasoning [`Position::Effect`]
    /// reaches inside them for.
    fn expr_scalar(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        let span = expr.span;
        match &expr.kind {
            ExprKind::Int(value) if self.scalar_of(expr) == Some(Scalar::Int) => {
                self.emit(Inst::ScalarConst(*value), span);
            }
            ExprKind::Bool(value) if self.scalar_of(expr) == Some(Scalar::Bool) => {
                self.emit(Inst::ScalarConst(i64::from(*value)), span);
            }
            ExprKind::Ident(name) => match self.scalar_binding(name) {
                Some((slot, _)) => self.emit(Inst::LoadScalar(slot), span),
                None => return self.moved_to_scalar(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => {
                // `&&`/`||` wanted as a scalar: the scalar form costs
                // `2 - k` boundaries where `k` operands are already on the
                // scalar stack, the value form costs `k + 1` (one per
                // already-scalar operand, plus one to move the answer
                // across), so the scalar form wins as soon as `k >= 1`.
                if matches!(op, SourceBinary::And | SourceBinary::Or)
                    && self.scalar_of(expr) == Some(Scalar::Bool)
                    && (self.on_scalar_stack(lhs) || self.on_scalar_stack(rhs))
                {
                    return self.and_or_scalar(*op, lhs, rhs, span);
                }
                let inst = binary_op(*op).map(|op| self.binary_inst(op, lhs, rhs));
                let Some(inst @ Inst::IntBinary(_)) = inst else {
                    return self.moved_to_scalar(expr);
                };
                // `binary_inst` answered `IntBinary` only because the checker
                // settled both operands as `Int`, so both hold this
                // function's precondition and neither needs asking again.
                self.expr_scalar(lhs)?;
                self.expr_scalar(rhs)?;
                self.emit(inst, span);
            }
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => {
                // Deliberately not through `moved_to_scalar`: a call to a
                // function whose answer already arrives on the scalar stack
                // would be moved off it and straight back on again, which is
                // the pair of instructions this whole convention exists to
                // stop emitting. Only a call that landed on the value stack
                // crosses.
                if self
                    .call(expr.id, callee, args, trailing.as_deref(), span)?
                    .is_none()
                {
                    self.emit(Inst::ValueToScalar, span);
                }
            }
            ExprKind::Block(_) | ExprKind::Match { .. } => {
                return self.expr_at(expr, Position::Scalar)
            }
            // An `if` with no `else` answers `()`, which the scalar stack
            // does not hold, so only the two-branch form takes the position.
            ExprKind::If { else_branch, .. } if else_branch.is_some() => {
                return self.expr_at(expr, Position::Scalar)
            }
            // `Inst::GetFieldAtScalar` where the receiver's position and the
            // field's own type are both settled — see `Body::scalar_field`.
            // Anything else falls to `moved_to_scalar`, exactly where
            // `Inst::GetFieldAt` is not emitted either.
            ExprKind::Field { base, name } => match self.scalar_field(expr, base, &name.node) {
                Some(index) => {
                    self.expr(base)?;
                    self.emit(Inst::GetFieldAtScalar(index), span);
                }
                None => return self.moved_to_scalar(expr),
            },
            _ => return self.moved_to_scalar(expr),
        }
        Ok(())
    }

    /// Lowers one expression the way it has always been lowered, and moves
    /// what it produced onto the scalar stack.
    fn moved_to_scalar(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr(expr)?;
        self.emit(Inst::ValueToScalar, expr.span);
        Ok(())
    }

    /// Lowers a condition and answers whether it left its `Bool` on the
    /// scalar stack.
    fn condition(&mut self, condition: &'a Expr) -> Result<bool, Unsupported> {
        if self.scalar_of(condition) == Some(Scalar::Bool) && self.on_scalar_stack(condition) {
            self.expr_scalar(condition)?;
            return Ok(true);
        }
        self.expr(condition)?;
        Ok(false)
    }

    /// Lowers one expression in the position it was written in.
    ///
    /// Six constructs take the position themselves, because each of them
    /// either builds its `Unit` here — an assignment, a `while`, a `for`, an
    /// `if` with no `else` — or has an inside the position should reach: an
    /// `if`/`else`, a `Block`, and a `Match` hand it to each branch, tail,
    /// and arm. Everything else answers a value it computed, and the only
    /// honest way to want nothing from it is to take that value off again,
    /// which is the `Pop` below.
    fn expr_at(&mut self, expr: &'a Expr, position: Position) -> Result<(), Unsupported> {
        let span = expr.span;
        // The scalar position reaches only the three constructs with an
        // inside; everything else is a leaf, and a leaf's scalar lowering is
        // [`Body::expr_scalar`]'s rather than a second copy of it here.
        if position == Position::Scalar
            && !matches!(
                expr.kind,
                ExprKind::Block(_)
                    | ExprKind::Match { .. }
                    | ExprKind::If {
                        else_branch: Some(_),
                        ..
                    }
            )
        {
            return self.expr_scalar(expr);
        }
        match &expr.kind {
            ExprKind::Int(value) => self.constant(Const::Int(*value), span),
            ExprKind::Float(value) => self.constant(Const::Float(*value), span),
            ExprKind::Bool(value) => self.constant(Const::Bool(*value), span),
            ExprKind::Duration(value) => self.constant(Const::Duration(*value), span),
            ExprKind::Unit => self.constant(Const::Unit, span),
            ExprKind::Str(parts) => self.string(parts, span)?,
            ExprKind::Ident(name) => self.ident(name, span)?,
            ExprKind::ArrayLit(items) => {
                for item in items {
                    self.expr(item)?;
                }
                self.emit(Inst::MakeArray(items.len() as u32), span);
            }
            ExprKind::Field { base, name } => self.field(base, &name.node, span)?,
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => {
                // A call to a function whose return type the checker settled
                // leaves its answer on the scalar stack, so what a reader of
                // this position needs is on the other one: one boundary
                // instruction where a value is wanted, and the scalar
                // stack's own discard where nothing is.
                if let Some(what) = self.call(expr.id, callee, args, trailing.as_deref(), span)? {
                    if position == Position::Effect {
                        self.emit(Inst::ScalarPop, span);
                        return Ok(());
                    }
                    self.emit(Inst::ScalarToValue(what), span);
                }
            }
            ExprKind::Unary { op, operand } => {
                self.expr(operand)?;
                let op = match op {
                    SourceUnary::Not => UnaryOp::Not,
                    SourceUnary::Neg => UnaryOp::Neg,
                };
                self.emit(Inst::Unary(op), span);
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(expr, *op, lhs, rhs, span)?,
            ExprKind::Assign { op, target, value } => {
                return self.assign(*op, target, value, position, span)
            }
            ExprKind::Try(inner) => {
                self.expr(inner)?;
                self.emit(Inst::Try, span);
            }
            ExprKind::Block(block) => return self.block_at(block, position),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                return self.conditional(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    position,
                    span,
                )
            }
            ExprKind::While { condition, body } => {
                return self.while_loop(condition, body, position, span)
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => return self.for_loop(binding.node.as_str(), iterable, body, position, span),
            ExprKind::Return(value) => match (self.returns, value) {
                // Every return of a function leaves on the stack that
                // function's `returns` names, because a caller reads exactly
                // that one and nothing tells it which of two a given return
                // used.
                (SlotKind::Scalar(_), Some(value)) => {
                    self.expr_scalar(value)?;
                    self.emit(Inst::ReturnScalar, span);
                }
                // `return` with no value answers `()`, and no scalar stack
                // holds one. The checker compares a `return`'s operand
                // against the declared type, so a checked program whose
                // return type is `Int` or `Bool` has no such `return`;
                // lowering it as the untyped one rather than inventing a
                // scalar is what makes `validate` refuse the pair and say so
                // instead of the VM reading a word that was never written.
                (SlotKind::Scalar(_), None) | (SlotKind::Value, None) => {
                    self.constant(Const::Unit, span);
                    self.emit_dyn_return(span);
                    self.emit(Inst::Return, span);
                }
                (SlotKind::Value, Some(value)) => {
                    self.expr(value)?;
                    self.emit_dyn_return(span);
                    self.emit(Inst::Return, span);
                }
                // `slot_kind_of` never answers `Place` about a return type,
                // so no function's `returns` is one.
                (SlotKind::Place, _) => {
                    unreachable!("a function does not answer a place")
                }
            },
            ExprKind::Break(value) => {
                // The operand is evaluated for its effects and discarded: a
                // loop is `()` however it leaves, so there is nowhere for a
                // value to go.
                if let Some(value) = value {
                    self.effect(value)?;
                }
                self.leave_loop(true, span)?;
            }
            ExprKind::Continue => self.leave_loop(false, span)?,
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => self.range(start, end, *inclusive_end, span)?,
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => self.lambda(expr, *is_async, params, body, span, false)?,
            ExprKind::Match { scrutinee, arms } => {
                return self.match_expr(scrutinee, arms, position, span)
            }
            ExprKind::Scope { name, body } => return self.scope_expr(name, body, position, span),
            ExprKind::Await(inner) => {
                self.expr(inner)?;
                self.emit(Inst::Await, span);
            }
        }
        if position == Position::Effect {
            // A value was computed and nothing reads it. Where control cannot
            // reach here — after a `return`, a `break`, or a `continue` —
            // `emit` writes nothing, so a diverging expression costs no `Pop`
            // either.
            self.emit(Inst::Pop, span);
        }
        Ok(())
    }

    /// A string literal, and the interpolations written inside it.
    ///
    /// A literal with nothing interpolated is one `Const::Str`: there is no
    /// rendering to do, so there is nothing for a `Concat` to do either.
    fn string(&mut self, parts: &'a [StrPart], span: Span) -> Result<(), Unsupported> {
        let interpolated = parts
            .iter()
            .any(|part| matches!(part, StrPart::Interpolation(_)));
        if !interpolated {
            let mut text = String::new();
            for part in parts {
                if let StrPart::Text(literal) = part {
                    text.push_str(literal);
                }
            }
            self.constant(Const::Str(text.into()), span);
            return Ok(());
        }
        for part in parts {
            match part {
                StrPart::Text(literal) => self.constant(Const::Str(literal.as_str().into()), span),
                StrPart::Interpolation(expr) => self.expr(expr)?,
            }
        }
        self.emit(Inst::Concat(parts.len() as u32), span);
        Ok(())
    }

    /// `a..b` and `a..<b`, built as the value it is.
    ///
    /// A range is an ordinary Cove value — `Interpreter::eval`'s
    /// `ExprKind::Range` arm evaluates one like any other expression, and
    /// says so — so it can be bound, passed, compared, rendered, and used as
    /// a `Map` key. [`Body::for_loop`] is the one place that never builds
    /// one: a `for` over a range walks between two bounds it keeps in hidden
    /// slots, so there is no `Range` in a loop at all, and that stays true.
    ///
    /// The bounds go onto the scalar stack, which is where the checker's own
    /// answer puts them: it checks each against `Ty::Int`, so
    /// [`Body::scalar_of`] settles both, and a settled `Int` operand belongs
    /// on that stack the way every other one does. Where it settled
    /// something else — which a checked program has no way to write, since
    /// the expectation is what makes a non-`Int` bound a diagnostic — this
    /// refuses rather than moving a `Value` across a boundary that promised
    /// an `Int` and was handed something else.
    fn range(
        &mut self,
        start: &'a Expr,
        end: &'a Expr,
        inclusive_end: bool,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.scalar_of(start) != Some(Scalar::Int) || self.scalar_of(end) != Some(Scalar::Int) {
            return Err(Unsupported::new(
                "a range whose bounds the checker did not settle as `Int`",
                span,
            ));
        }
        self.expr_scalar(start)?;
        self.expr_scalar(end)?;
        self.emit(Inst::MakeRange { inclusive_end }, span);
        Ok(())
    }

    /// A bare name.
    ///
    /// A local wins over everything else, which is what lets a `let http`
    /// shadow the host module of that name — and what leaves an `http.fetch`
    /// written above the `let` still reaching the host.
    fn ident(&mut self, name: &str, span: Span) -> Result<(), Unsupported> {
        if let Some((slot, what)) = self.scalar_binding(name) {
            // A scalar slot read where a `Value` is wanted is the boundary in
            // the outward direction, and the instruction carries the tag the
            // word itself does not.
            self.emit(Inst::LoadScalar(slot), span);
            self.emit(Inst::ScalarToValue(what), span);
            return Ok(());
        }
        if let Some(binding) = self.binding(name) {
            let (slot, kind, capture) = (binding.slot, binding.kind, binding.capture);
            match (kind, capture) {
                // A `var` parameter's slot holds a place, not the value, so
                // reading the parameter is loading the place and reading
                // through it — which is where the caller's own storage is.
                (SlotKind::Place, _) => {
                    self.emit(Inst::LoadPlace(slot), span);
                    self.emit(Inst::PlaceRead, span);
                }
                (_, Some(index)) => self.emit(Inst::LoadCapture(index), span),
                _ => self.emit(Inst::LoadLocal(slot), span),
            }
            return Ok(());
        }
        if name == builtins::NONE_CASE.name {
            // `None` is the one builtin case written as a bare name rather
            // than as a call, so it is built here rather than at a call.
            let none = self.outer.name(name);
            self.emit(
                Inst::MakeBuiltin {
                    name: none,
                    argc: 0,
                },
                span,
            );
            return Ok(());
        }
        if let Some(key) = self.outer.function_of(self.module, name) {
            // A closure over nothing. `Interpreter::eval_ident` builds one
            // with `captures: Vec::new()`, because a declaration reads no
            // environment — the whole of what makes a function a value is
            // that it can be called through one.
            //
            // The specialisation it names is not the one a direct call
            // reaches. A call through a value puts every argument on the
            // value stack and reads the answer off it, and a convention is
            // what a slot number means, so the body is lowered a second time
            // under that convention rather than called under a convention
            // nothing at the call site could have known.
            let params = self.outer.declaration(key).decl.params.len();
            let function = self.outer.number(Instance::Declared {
                key,
                supplied: vec![true; params],
                as_value: true,
            });
            self.emit(
                Inst::MakeClosure {
                    function,
                    captures: 0,
                },
                span,
            );
            return Ok(());
        }
        if self.outer.struct_of(self.module, name).is_some()
            || self.outer.declares_enum(self.module, name)
            || builtins::is_builtin_type(name)
        {
            return Err(Unsupported::new(
                format!("`{name}`, a type used as a value"),
                span,
            ));
        }
        if self.outer.imported_module(self.module, name).is_some()
            || self.outer.is_host_module(self.module, name)
            || self.outer.host_item(self.module, name).is_some()
        {
            return Err(Unsupported::new(
                format!("`{name}`, a module or a host operation used as a value"),
                span,
            ));
        }
        Err(Unsupported::new(
            format!("`{name}`, a name the lowering cannot resolve"),
            span,
        ))
    }

    /// `fn(x) { ... }`: a function of its own, and the values it is handed.
    ///
    /// Two things happen here and the order is the whole of the semantics.
    /// The captures are worked out first, from what this body has live and
    /// what the lambda's own body mentions, which is `Env::captures` asked
    /// at lowering time; then each of them is *read* onto the value stack,
    /// left to right, and [`Inst::MakeClosure`] pairs them with their names.
    ///
    /// Reading is the point. The oracle captures by value at creation time —
    /// a closure over a `var` binding still answers what the binding held
    /// when the lambda was written, after the binding has been assigned to —
    /// so a capture whose binding is a scalar slot crosses to the value
    /// stack here, and a capture whose binding is a `var` parameter is the
    /// value the place names rather than the place. See
    /// [`Inst::PlaceLocal`], where that second one is what keeps a place
    /// from outliving the frame that built it.
    ///
    /// A name the lambda mentions that this body has no binding for is not a
    /// capture at all: a declaration, a type, and a host module resolve in
    /// the module rather than in the environment, exactly as they do in the
    /// interpreter, where `Env::captures` only walks bindings.
    fn lambda(
        &mut self,
        expr: &'a Expr,
        is_async: bool,
        params: &'a [Param],
        body: &'a Block,
        span: Span,
        aliases_first_param: bool,
    ) -> Result<(), Unsupported> {
        let mentioned = mentioned_names(body);
        // Outermost first, one entry per name, and the *latest* binding of a
        // name that is declared twice — which is `Env::captures`'s walk,
        // where a repeated name overwrites the value it recorded and keeps
        // the position it recorded it at.
        let mut captured: Vec<(&'a str, u32)> = Vec::new();
        for (at, binding) in self.live.iter().enumerate() {
            let Some(name) = binding.name else { continue };
            if !mentioned.contains(name) {
                continue;
            }
            match captured.iter_mut().find(|(held, _)| *held == name) {
                Some(slot) => slot.1 = at as u32,
                None => captured.push((name, at as u32)),
            }
        }
        if captured.len() >= u16::MAX as usize {
            // `Inst::MakeClosure` holds the count in a `u16` for the reason
            // `Inst::Call` holds its counts in one. Nothing writes a lambda
            // with this many free names; the check is what makes the width a
            // fact.
            return Err(Unsupported::new(
                "a closure with more than 65534 captures",
                span,
            ));
        }
        let names: Vec<&'a str> = captured.iter().map(|(name, _)| *name).collect();
        for (_, at) in &captured {
            let binding = &self.live[*at as usize];
            let (slot, kind, capture) = (binding.slot, binding.kind, binding.capture);
            match (kind, capture) {
                // The value the place names, not the place: `Env::captures`
                // calls `place.read`.
                (SlotKind::Place, _) => {
                    self.emit(Inst::LoadPlace(slot), span);
                    self.emit(Inst::PlaceRead, span);
                }
                (SlotKind::Scalar(what), _) => {
                    self.emit(Inst::LoadScalar(slot), span);
                    self.emit(Inst::ScalarToValue(what), span);
                }
                (SlotKind::Value, Some(index)) => self.emit(Inst::LoadCapture(index), span),
                (SlotKind::Value, None) => self.emit(Inst::LoadLocal(slot), span),
            }
        }
        let count = names.len() as u16;
        let module = self.module;
        let function = self.outer.number_lambda(
            LambdaSite {
                module,
                params,
                body,
                span,
                captures: names,
                is_async,
                aliases_first_param,
            },
            (span.file, expr.id),
        );
        self.emit(
            Inst::MakeClosure {
                function,
                captures: count,
            },
            span,
        );
        Ok(())
    }

    /// `base.name` written where a value is wanted.
    ///
    /// A head that is not a local may be a *name* rather than a value, and
    /// `Interpreter::eval_field` answers those before it evaluates anything:
    /// `Status.Confirmed` is a case of an enum, `console.println` is a host
    /// operation, and `booking.Status` is a declaration reached through the
    /// module that exports it. Only the first of the three has a lowering,
    /// and the other two are named rather than read as a field of a value
    /// they are not.
    fn field(&mut self, base: &'a Expr, name: &str, span: Span) -> Result<(), Unsupported> {
        // `http.Method.Get`: a case of an enum a host declares, reached
        // through the module that declares it. The head is a `Field` rather
        // than an `Ident` here, because two names stand between the module
        // and the case, and neither of them is a value: the interpreter
        // answers `http.Method` as a `Value::Type` and then reads the case
        // off it, so nothing this builds was ever a field of anything.
        if let ExprKind::Field {
            base: inner,
            name: type_name,
        } = &base.kind
        {
            if let ExprKind::Ident(head) = &inner.kind {
                if self.lookup(head).is_none() && self.outer.is_host_module(self.module, head) {
                    if let Some(declared) = cove_schema::hosts::module(head)
                        .and_then(|schema| schema.declared_type(&type_name.node))
                    {
                        // A schema's `cases` is empty for a struct, so this
                        // asks whether the type is an enum at all. Whether it
                        // declares *this* case is settled where the
                        // interpreter settles it, at run time, for the reason
                        // `Body::make_enum` gives.
                        if !declared.cases.is_empty() {
                            let ty = self.outer.name(&format!("{head}.{}", type_name.node));
                            let case = self.outer.name(name);
                            self.emit(Inst::MakeHostEnum { ty, case }, span);
                            return Ok(());
                        }
                    }
                }
            }
        }
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if let Some((owner, _)) = self.outer.enum_of(self.module, head) {
                    // `Status.Confirmed`: a case written without a call, so
                    // its payload is empty. Whether the enum declares such a
                    // case is settled where the interpreter settles it — in
                    // `enum_case`, at run time — because a case that does not
                    // exist is a failure with a message rather than a shape
                    // the lowering could produce something else for.
                    return self.make_enum(owner, head, name, Args::new(&[], None), span);
                }
                if self.outer.is_host_module(self.module, head) {
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, a host operation used as a value"),
                        span,
                    ));
                }
                if self.outer.imported_module(self.module, head).is_some() {
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, a declaration named through its module"),
                        span,
                    ));
                }
            }
        }
        let inst = self.field_inst(base, name);
        self.expr(base)?;
        self.emit(inst, span);
        Ok(())
    }

    /// `&&` and `||` short-circuit, so they lower to a jump: there is no
    /// instruction for them, and an operator that evaluated both sides would
    /// be a different language.
    fn binary(
        &mut self,
        expr: &'a Expr,
        op: SourceBinary,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
    ) -> Result<(), Unsupported> {
        match op {
            SourceBinary::And | SourceBinary::Or => {
                // `&&`/`||` wanted as a value: the scalar form costs
                // `(2 - k) + 1` boundaries where `k` operands are already on
                // the scalar stack (both operands moved across, plus the
                // answer moved back), the value form costs `k`, so the
                // scalar form only wins where `k == 2` — both operands
                // already scalar, nothing but the answer crosses.
                if self.scalar_of(expr) == Some(Scalar::Bool)
                    && self.on_scalar_stack(lhs)
                    && self.on_scalar_stack(rhs)
                {
                    self.and_or_scalar(op, lhs, rhs, span)?;
                    self.emit(Inst::ScalarToValue(Scalar::Bool), span);
                    return Ok(());
                }
                let short = self.label();
                let end = self.label();
                self.expr(lhs)?;
                if op == SourceBinary::And {
                    self.jump(Inst::JumpIfFalse, short, span);
                } else {
                    self.jump(Inst::JumpIfTrue, short, span);
                }
                self.expr(rhs)?;
                self.jump(Inst::Jump, end, span);
                self.bind(short);
                // The side that short-circuited is the answer: `&&` that
                // stopped is `false` and `||` that stopped is `true`.
                self.constant(Const::Bool(op == SourceBinary::Or), span);
                self.bind(end);
                Ok(())
            }
            _ => {
                let op = binary_op(op).expect("`&&` and `||` are the two handled above");
                let inst = self.binary_inst(op, lhs, rhs);
                if let Inst::IntBinary(typed) = inst {
                    // The typed operator lives on the scalar stack, so its
                    // operands are lowered onto it and its answer is moved
                    // back only because a value is what was asked for here.
                    // Where a scalar is what was asked for, `expr_scalar`
                    // emits the same three instructions and no fourth.
                    self.expr_scalar(lhs)?;
                    self.expr_scalar(rhs)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                    return Ok(());
                }
                self.expr(lhs)?;
                self.expr(rhs)?;
                self.emit(inst, span);
                Ok(())
            }
        }
    }

    /// `&&`/`||` lowered entirely on the scalar stack.
    ///
    /// The same shape as `binary` above with every instruction replaced by
    /// its scalar counterpart: the jump pops the scalar stack instead of the
    /// value stack, and the side that short-circuited is answered as a
    /// scalar rather than a `Const`. The short-circuiting side is still the
    /// answer for the same reason it always was — `&&` that stopped is
    /// `false` and `||` that stopped is `true` — this only changes which
    /// stack that answer is written to.
    fn and_or_scalar(
        &mut self,
        op: SourceBinary,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
    ) -> Result<(), Unsupported> {
        let short = self.label();
        let end = self.label();
        self.expr_scalar(lhs)?;
        if op == SourceBinary::And {
            self.jump(Inst::JumpIfFalseScalar, short, span);
        } else {
            self.jump(Inst::JumpIfTrueScalar, short, span);
        }
        self.expr_scalar(rhs)?;
        self.jump(Inst::Jump, end, span);
        self.bind(short);
        // The side that short-circuited is the answer: `&&` that stopped is
        // `false` and `||` that stopped is `true`.
        self.emit(Inst::ScalarConst(i64::from(op == SourceBinary::Or)), span);
        self.bind(end);
        Ok(())
    }

    /// `place = value` and `place += value`, which produce `()`.
    ///
    /// A compound assignment reads the place, then evaluates the right-hand
    /// side, then combines them — the order the interpreter reads them in.
    ///
    /// The store is the whole of what an assignment does, so lowered for
    /// effect it ends there and the `()` it would have answered is not built.
    fn assign(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.written_through_a_place(target) {
            return self.assign_through_place(op, target, value, position, span);
        }
        if matches!(target.kind, ExprKind::Field { .. }) {
            return self.assign_field(op, target, value, position, span);
        }
        let ExprKind::Ident(name) = &target.kind else {
            return Err(Unsupported::new("assignment to this place", span));
        };
        let Some(binding) = self.binding(name) else {
            return Err(Unsupported::new(
                format!("assignment to `{name}`, which is not a local"),
                span,
            ));
        };
        let (slot, kind) = (binding.slot, binding.kind);
        match op {
            None => match kind {
                SlotKind::Scalar(_) => self.expr_scalar(value)?,
                SlotKind::Value => self.expr(value)?,
                SlotKind::Place => {
                    unreachable!("a place binding is written by `assign_through_place`")
                }
            },
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                // The place read is the left operand, so the type the checker
                // settled for it is what says whether this is integer
                // arithmetic — the same question `a + b` asks, asked of the
                // two expressions this form writes as one.
                let inst = self.binary_inst(op, target, value);
                match (kind, inst) {
                    // Read, combine, and write again without ever leaving the
                    // scalar stack. This is `i += 1` inside a loop, which is
                    // the case the whole arrangement exists for.
                    (SlotKind::Scalar(_), Inst::IntBinary(_)) => {
                        self.emit(Inst::LoadScalar(slot), target.span);
                        self.expr_scalar(value)?;
                        self.emit(inst, span);
                    }
                    (SlotKind::Scalar(what), _) => {
                        self.emit(Inst::LoadScalar(slot), target.span);
                        self.emit(Inst::ScalarToValue(what), target.span);
                        self.expr(value)?;
                        self.emit(inst, span);
                        self.emit(Inst::ValueToScalar, span);
                    }
                    (SlotKind::Value, Inst::IntBinary(typed)) => {
                        self.emit(Inst::LoadLocal(slot), target.span);
                        self.emit(Inst::ValueToScalar, target.span);
                        self.expr_scalar(value)?;
                        self.emit(inst, span);
                        self.emit(Inst::ScalarToValue(int_result(typed)), span);
                    }
                    (SlotKind::Value, _) => {
                        self.emit(Inst::LoadLocal(slot), target.span);
                        self.expr(value)?;
                        self.emit(inst, span);
                    }
                    (SlotKind::Place, _) => {
                        unreachable!("a place binding is written by `assign_through_place`")
                    }
                }
            }
        }
        self.emit(store_slot(kind, slot), span);
        self.unit_at(position, span);
        Ok(())
    }

    /// An assignment written through a place: `n = 1` where `n` is a `var`
    /// parameter, and `a.b.c = 1` wherever the path is longer than one step.
    ///
    /// The place is built twice for a compound assignment, once for the read
    /// and once for the write, rather than duplicated on the place stack.
    /// Building it is a slot load and a run of `place-field`s, none of which
    /// can have an effect and none of which can fail, so doing it twice is
    /// the same program — and the alternative would be an instruction whose
    /// only reader is this one lowering.
    ///
    /// The read happens before the right-hand side is evaluated, which is
    /// the order `Interpreter::eval`'s `ExprKind::Assign` arm reads in:
    /// `place.read(span)?` and then `self.eval(env, value)?`.
    fn assign_through_place(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if !self.is_a_place(target) {
            return Err(Unsupported::new("assignment to this place", span));
        }
        match op {
            None => {
                self.place(target)?;
                self.expr(value)?;
            }
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                // The place read is the left operand, so the type the checker
                // settled for it is what says whether this is integer
                // arithmetic — the same question `a + b` asks.
                let inst = self.binary_inst(op, target, value);
                // The place the write will consume, built below the one the
                // read consumes: `place-read` takes the top of the place
                // stack and `place-write` takes what was under it.
                self.place(target)?;
                self.place(target)?;
                self.emit(Inst::PlaceRead, target.span);
                if let Inst::IntBinary(typed) = inst {
                    // A place is read and written as a `Value` whatever it
                    // holds, so this is the boundary in both directions
                    // around one typed operator — the same shape a written
                    // struct field has, and for the same reason.
                    self.emit(Inst::ValueToScalar, target.span);
                    self.expr_scalar(value)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                } else {
                    self.expr(value)?;
                    self.emit(inst, span);
                }
            }
        }
        self.emit(Inst::PlaceWrite, span);
        self.unit_at(position, span);
        Ok(())
    }

    /// `place.field = value`, and the compound forms.
    ///
    /// The base must be a local. A struct is a value and a local is the only
    /// holder of its own, so writing a field is reading the struct, replacing
    /// the field, and storing the struct back — which is what
    /// [`crate::Inst::SetField`] does and why it is a whole-value update
    /// rather than a mutation through a place. A deeper path than one field is
    /// refused rather than rebuilt: it would need the intermediate struct put
    /// back too, and nothing in the subset produces one.
    ///
    /// `target` is the whole `place.field`, because that is what the
    /// instructions reading the struct point at: a diagnostic about the read
    /// is about the place, not about the name below it.
    fn assign_field(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let ExprKind::Field { base, name: field } = &target.kind else {
            unreachable!("`assign` dispatches here only for a field")
        };
        let field = field.node.as_str();
        let place = target.span;
        let ExprKind::Ident(name) = &base.kind else {
            return Err(Unsupported::new(
                "assignment to a field of anything but a local",
                span,
            ));
        };
        let Some(slot) = self.lookup(name) else {
            return Err(Unsupported::new(
                format!("assignment to a field of `{name}`, which is not a local"),
                span,
            ));
        };
        // The write goes by name whatever the checker settled: `SetField`
        // puts a value back where a name stands, and only the read has a
        // position to take instead.
        let named = self.outer.name(field);
        self.emit(Inst::LoadLocal(slot), place);
        match op {
            None => self.expr(value)?,
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                let read = self.field_inst(base, field);
                let inst = self.binary_inst(op, target, value);
                self.emit(Inst::Dup, place);
                self.emit(read, place);
                if let Inst::IntBinary(typed) = inst {
                    // A field is a `Value` wherever it is read from, so this
                    // is the boundary in both directions around one typed
                    // operator. A struct's fields are not slots and this
                    // slice does not make them one.
                    self.emit(Inst::ValueToScalar, place);
                    self.expr_scalar(value)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                } else {
                    self.expr(value)?;
                    self.emit(inst, span);
                }
            }
        }
        self.emit(Inst::SetField(named), span);
        self.emit(Inst::StoreLocal(slot), span);
        self.unit_at(position, span);
        Ok(())
    }

    /// `if` and `else`.
    ///
    /// An `if` with no `else` is `()` however it goes, including when the
    /// branch that ran produced something: there is no second branch to give
    /// the missing case a value, so the branch that ran does not get to
    /// supply one either. Its branch is therefore lowered for effect in both
    /// positions, and only the `()` at the join depends on which one this is.
    ///
    /// An `if` with an `else` is worth something, so the position reaches
    /// inside it: both branches are lowered in the position the `if` is in,
    /// and lowering for effect saves whatever each branch would have built.
    fn conditional(
        &mut self,
        condition: &'a Expr,
        then_branch: &'a Block,
        else_branch: Option<&'a Expr>,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let branch = branch_on(self.condition(condition)?);
        match else_branch {
            Some(else_branch) => {
                let otherwise = self.label();
                let end = self.label();
                self.jump(branch, otherwise, condition.span);
                self.block_at(then_branch, position)?;
                self.jump(Inst::Jump, end, span);
                self.bind(otherwise);
                self.expr_at(else_branch, position)?;
                self.bind(end);
            }
            None => {
                let end = self.label();
                self.jump(branch, end, condition.span);
                self.block_at(then_branch, Position::Effect)?;
                self.bind(end);
                self.unit_at(position, span);
            }
        }
        Ok(())
    }

    /// `while`, which is `()` however it leaves — so its body's value is
    /// never wanted, and lowered for effect the loop builds nothing at all.
    fn while_loop(
        &mut self,
        condition: &'a Expr,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let base = self.depth.unwrap_or(Depth::EMPTY);
        let top = self.label();
        let end = self.label();
        self.bind(top);
        let branch = branch_on(self.condition(condition)?);
        self.jump(branch, end, condition.span);
        self.loops.push(LoopFrame {
            break_to: end,
            continue_to: top,
            scopes: self.open_scopes,
            depth: base,
        });
        let lowered = self.block_at(body, Position::Effect);
        self.loops.pop();
        lowered?;
        self.jump(Inst::Jump, top, span);
        self.bind(end);
        self.unit_at(position, span);
        Ok(())
    }

    /// `for`, over a range written in the header or over a sequence.
    ///
    /// The iterable is evaluated once, in the enclosing scope, and the
    /// binding is declared in the scope the body sees — the two halves of
    /// what the interpreter does around `iterable_items`.
    ///
    /// A range header never builds a range value. [`Inst::MakeRange`] makes
    /// one, and a range written anywhere else is lowered through it, but a
    /// `for` has nothing to do with the value: it wants the integers between
    /// two bounds, so the bounds go into two hidden slots and the loop counts
    /// between them. Building a `Range` here and taking it apart again would
    /// be a value made for one instruction to discard, which is what
    /// `a_for_over_a_range_counts_between_two_hidden_slots` pins.
    ///
    /// Anything else is asked once, by `iter-items`, for the items a
    /// `for` walks it as — the elements of a sequence, the `MapEntry` of each
    /// pair of a `Map`, a `Set`'s elements in ascending order — and what
    /// comes back is always an `Array`, so the loop walks it by index with
    /// its length read once. Asking once is what makes iterating a `Vector`
    /// the body appends walk the same elements the interpreter's snapshot
    /// holds.
    fn for_loop(
        &mut self,
        binding: &'a str,
        iterable: &'a Expr,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let base = self.depth.unwrap_or(Depth::EMPTY);
        let mark = self.scope();

        let (cursor, header) = match &iterable.kind {
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => {
                let cursor = self.declare(None, SlotKind::Value);
                let limit = self.declare(None, SlotKind::Value);
                self.expr(start)?;
                self.emit(Inst::StoreLocal(cursor), start.span);
                self.expr(end)?;
                self.emit(Inst::StoreLocal(limit), end.span);
                (
                    cursor,
                    Header::Range {
                        limit,
                        inclusive: *inclusive_end,
                    },
                )
            }
            _ => {
                let sequence = self.declare(None, SlotKind::Value);
                let length = self.declare(None, SlotKind::Value);
                let cursor = self.declare(None, SlotKind::Value);
                self.expr(iterable)?;
                self.emit(Inst::IterItems, iterable.span);
                self.emit(Inst::StoreLocal(sequence), iterable.span);
                self.emit(Inst::LoadLocal(sequence), iterable.span);
                let name = self.outer.name("length");
                self.emit(Inst::CallBuiltin { name, argc: 0 }, iterable.span);
                self.emit(Inst::StoreLocal(length), iterable.span);
                self.constant(Const::Int(0), iterable.span);
                self.emit(Inst::StoreLocal(cursor), iterable.span);
                (cursor, Header::Sequence { sequence, length })
            }
        };

        // The binding belongs to the scope the body sees, and the body's own
        // block opens a scope inside this one.
        // A `for` binding is read-only, which is what the interpreter
        // declares one as.
        let element = self.declare(Some(binding), SlotKind::Value);

        let top = self.label();
        let next = self.label();
        let end = self.label();
        self.bind(top);
        self.emit(Inst::LoadLocal(cursor), span);
        match header {
            Header::Range { limit, inclusive } => {
                self.emit(Inst::LoadLocal(limit), span);
                // `a..b` yields `b`, and `a..<b` stops before it. Comparing
                // rather than adding one to the bound is what keeps a range
                // ending at the largest `Int` from overflowing.
                self.emit(
                    Inst::Binary(if inclusive {
                        BinaryOp::Le
                    } else {
                        BinaryOp::Lt
                    }),
                    span,
                );
            }
            Header::Sequence { length, .. } => {
                self.emit(Inst::LoadLocal(length), span);
                self.emit(Inst::Binary(BinaryOp::Lt), span);
            }
        }
        self.jump(Inst::JumpIfFalse, end, span);
        match header {
            Header::Range { .. } => self.emit(Inst::LoadLocal(cursor), span),
            Header::Sequence { sequence, .. } => {
                self.emit(Inst::LoadLocal(sequence), span);
                self.emit(Inst::LoadLocal(cursor), span);
                let get = self.outer.name("get");
                self.emit(Inst::CallBuiltin { name: get, argc: 1 }, span);
                // An indexed read answers an `Option`, and the test above
                // has already put the cursor below the length, so what comes
                // back is a `Some`. `Try` is the instruction that opens one,
                // and it is used here rather than `unwrapOr` because there is
                // no element value the lowering could invent as a fallback.
                self.emit(Inst::Try, span);
            }
        }
        self.emit(Inst::StoreLocal(element), span);

        self.loops.push(LoopFrame {
            break_to: end,
            continue_to: next,
            scopes: self.open_scopes,
            depth: base,
        });
        let lowered = self.block_at(body, Position::Effect);
        self.loops.pop();
        lowered?;

        // `continue` lands here, so that skipping the rest of a body still
        // advances the cursor.
        self.bind(next);
        self.emit(Inst::LoadLocal(cursor), span);
        self.constant(Const::Int(1), span);
        self.emit(Inst::Binary(BinaryOp::Add), span);
        self.emit(Inst::StoreLocal(cursor), span);
        self.jump(Inst::Jump, top, span);

        self.bind(end);
        self.release(mark);
        self.unit_at(position, span);
        Ok(())
    }

    /// Leaves the nearest enclosing loop.
    fn leave_loop(&mut self, breaking: bool, span: Span) -> Result<(), Unsupported> {
        let Some(frame) = self.loops.last() else {
            return Err(Unsupported::new(
                if breaking {
                    "a `break` outside a loop"
                } else {
                    "a `continue` outside a loop"
                },
                span,
            ));
        };
        let target = if breaking {
            frame.break_to
        } else {
            frame.continue_to
        };
        let base = frame.depth;
        // Every task scope written between here and the loop is left without
        // reaching the `leave-scope` below its body, and leaving a scope waits
        // for or cancels its children whichever way it is left. The oracle
        // reaches the same place through `Interpreter::leave_scope`, whose
        // early branch cancels for a `Break` exactly as it does for a `return`
        // or an error.
        let scopes = self.open_scopes - frame.scopes;
        for _ in 0..scopes {
            self.emit(Inst::CancelScope, span);
        }
        // Whatever the half-evaluated expression around this left on any of
        // the stacks goes with it, so the loop's exit is reached at the
        // depths the loop runs at. A place can be standing for the reason a
        // value can: `f(var x, if c { break } else { 1 })` has pushed one
        // before it evaluates the argument the `break` is written in.
        if let Some(depth) = self.depth {
            for _ in base.values..depth.values {
                self.emit(Inst::Pop, span);
            }
            for _ in base.scalars..depth.scalars {
                self.emit(Inst::ScalarPop, span);
            }
            for _ in base.places..depth.places {
                self.emit(Inst::PlacePop, span);
            }
        }
        self.jump(Inst::Jump, target, span);
        Ok(())
    }

    /// Lowers a call, answering where it left its result.
    ///
    /// `Some` means the scalar stack, which is what a call to a function
    /// whose return type the checker settled as `Int` or `Bool` leaves it
    /// on; `None` means the value stack, which is what every other call
    /// leaves it on. The answer is threaded up rather than asked about
    /// afterwards because only the path that resolved the callee knows it —
    /// a builtin, a host operation, a constructor, and a declared function
    /// are four different answers reached through four different lookups.
    fn call(
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
    fn call_declared(
        &mut self,
        key: Key,
        receiver: Option<&'a Expr>,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        let declared = self.outer.declaration(key);
        let decl = declared.decl;
        let what = declared.name.clone();

        for (at, param) in decl.params.iter().enumerate() {
            reject_parameter(param, at + 1 == decl.params.len())?;
        }
        let names: Vec<&str> = decl
            .params
            .iter()
            .map(|param| param.name.node.as_str())
            .collect();
        // `reject_parameter` has already refused a variadic parameter that
        // is not the last one, so the last one is the only one there can be.
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
    fn make_enum(
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

    // ------------------------------------------------------------- `match`

    /// `match subject { pattern => body ... }`.
    ///
    /// The subject is evaluated once and stays on the stack while the arms
    /// are tried, because [`Inst::TestCase`] and [`Inst::GetPayload`] peek:
    /// an arm that does not match has to leave the value for the next one.
    /// The arm that does match pops it before its body runs, and the value
    /// no arm covered is what [`Inst::NoMatch`] reports.
    ///
    /// Arms are tried in the order they are written and the first that
    /// matches is the only one that runs, which is what `ExprKind::Match`
    /// does; an arm's binders live in a scope of its own, released when the
    /// arm ends, exactly as a block's slots are.
    ///
    /// A `match`'s value is the value of the arm that ran, so the position it
    /// is lowered in is every arm's position: a `match` written as a
    /// statement builds nothing in any of them, and one written as an
    /// expression is unchanged.
    fn match_expr(
        &mut self,
        scrutinee: &'a Expr,
        arms: &'a [MatchArm],
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        self.expr(scrutinee)?;
        // The depth the subject alone stands at. Every failed test gets back
        // down to it before it jumps, so the next arm begins where this one
        // began and `validate`'s simulation sees one depth per instruction.
        let subject = self.depth.map_or(0, |depth| depth.values);
        let end = self.label();
        for arm in arms {
            let mark = self.scope();
            let next = self.label();
            self.pattern(&arm.pattern, next, subject)?;
            self.emit(Inst::Pop, arm.span);
            self.expr_at(&arm.body, position)?;
            self.release(mark);
            self.jump(Inst::Jump, end, arm.span);
            self.bind(next);
        }
        // Exhaustiveness is the checker's to prove and it does not prove it
        // yet, so a subject no arm covered stops the run rather than
        // answering. Where an arm matches everything, no jump reaches here
        // and `emit` writes nothing.
        self.emit(Inst::NoMatch, span);
        self.bind(end);
        Ok(())
    }

    /// One pattern, against the value on top of the stack.
    ///
    /// The value stays where it is: a test peeks and a binder copies, so what
    /// this leaves behind is what it was given, plus the payloads a nested
    /// pattern is still standing on. A test that fails discards those and
    /// jumps to `next`, so the arm after this one starts at `subject` — the
    /// depth the whole `match` runs its arms at.
    ///
    /// The rules are `Interpreter::match_pattern`'s, one for one, with one
    /// exception it names: a pattern that binds a different number of values
    /// than its case carries is a run-time error there, and here it is a
    /// `get-payload` past the end of the payload. `cove-sema` refuses such a
    /// pattern — `cove::type::payload_arity` — so no checked program reaches
    /// either, and reproducing the message would be reproducing it for a
    /// program that cannot exist.
    fn pattern(
        &mut self,
        pattern: &'a Pattern,
        next: usize,
        subject: u32,
    ) -> Result<(), Unsupported> {
        let span = pattern.span;
        match &pattern.kind {
            // Matches anything and binds nothing, so there is nothing to
            // emit: falling through is the match.
            PatternKind::Wildcard => Ok(()),
            PatternKind::Binding(name) => self.binder(name, next, subject, span),
            PatternKind::Literal(expr) => {
                // The same equality `==` is, because it is the same
                // comparison: `match_pattern` asks `eq_value`, which is what
                // `binary` answers `==` with once both sides are one type —
                // and the checker refuses a literal pattern of another type
                // before either backend sees it.
                self.emit(Inst::Dup, span);
                self.expr(expr)?;
                self.emit(Inst::Binary(BinaryOp::Eq), span);
                self.test(next, subject, span);
                Ok(())
            }
            PatternKind::Variant { path, payload } => {
                let case = self.outer.name(&case_tested(path));
                self.emit(Inst::TestCase(case), span);
                self.test(next, subject, span);
                // Each payload is matched against its own pattern, on top of
                // the value it came out of, which is how `Ok(Some(x))` reads
                // two levels down. The payload is dropped once its pattern is
                // done with it, leaving the enum it belongs to on top.
                for (index, sub) in payload.iter().enumerate() {
                    self.emit(Inst::GetPayload(index as u32), span);
                    self.pattern(sub, next, subject)?;
                    self.emit(Inst::Pop, span);
                }
                Ok(())
            }
        }
    }

    /// A binder: `other` binds the value, and `None` does not.
    ///
    /// `match_pattern` reads a binder named exactly `None` as a case test
    /// whenever the value it is given is an `Option`, and as a name
    /// otherwise. Which of the two it is therefore depends on the value
    /// rather than on the pattern, so both are lowered and the run picks:
    /// `Option` declares `Some` and `None` and nothing else, so a value that
    /// is neither is not an `Option`, and the name binds.
    ///
    /// Today's parser reaches none of this — a pattern whose name begins with
    /// an uppercase letter is a variant, and `None` does — so what is lowered
    /// here is the oracle's rule rather than a program's shape. It is
    /// reproduced anyway because the oracle is what a backend is answerable
    /// to, and a rule a backend quietly did not have is the kind of
    /// difference the differential tests exist to make impossible.
    ///
    /// The two tests name the type by its short name, which is what a pattern
    /// writes and what `match_pattern` compares a *variant* against; the
    /// binder rule compares the whole type name instead, so a declared enum
    /// that a module named `Option` and gave a case called `None` would be
    /// read as the builtin here and as a name there. That program cannot be
    /// written: the pattern it would need is one the parser makes a variant.
    fn binder(
        &mut self,
        name: &'a str,
        next: usize,
        subject: u32,
        span: Span,
    ) -> Result<(), Unsupported> {
        if name != builtins::NONE_CASE.name {
            self.emit(Inst::Dup, span);
            let slot = self.declare(Some(name), SlotKind::Value);
            self.emit(Inst::StoreLocal(slot), span);
            return Ok(());
        }
        let matched = self.label();
        let none = self.outer.name(&qualified_case(
            builtins::OPTION.name,
            builtins::NONE_CASE.name,
        ));
        self.emit(Inst::TestCase(none), span);
        self.jump(Inst::JumpIfTrue, matched, span);
        let some = self.outer.name(&qualified_case(
            builtins::OPTION.name,
            builtins::SOME_CASE.name,
        ));
        self.emit(Inst::TestCase(some), span);
        let bind_it = self.label();
        self.jump(Inst::JumpIfFalse, bind_it, span);
        self.fail_arm(next, subject, span);
        self.bind(bind_it);
        self.emit(Inst::Dup, span);
        let slot = self.declare(Some(name), SlotKind::Value);
        self.emit(Inst::StoreLocal(slot), span);
        self.bind(matched);
        Ok(())
    }

    /// Consumes the `Bool` a test pushed and leaves for the next arm when it
    /// is false.
    ///
    /// A test written at the top of a pattern can jump straight there,
    /// because the subject is all that stands on the stack. One written
    /// inside a payload cannot: the payloads it is standing on have to come
    /// off first, and a conditional jump has nowhere to put them.
    fn test(&mut self, next: usize, subject: u32, span: Span) {
        if self.depth.map(|depth| depth.values) == Some(subject + 1) {
            self.jump(Inst::JumpIfFalse, next, span);
            return;
        }
        let matched = self.label();
        self.jump(Inst::JumpIfTrue, matched, span);
        self.fail_arm(next, subject, span);
        self.bind(matched);
    }

    /// Leaves a half-matched pattern for the arm after it.
    ///
    /// Whatever the pattern was standing on goes with it, so the next arm is
    /// reached at the depth the arms run at — the same thing
    /// [`Body::leave_loop`] does for a `break` written inside a half-
    /// evaluated expression.
    fn fail_arm(&mut self, next: usize, subject: u32, span: Span) {
        if let Some(depth) = self.depth {
            for _ in subject..depth.values {
                self.emit(Inst::Pop, span);
            }
        }
        self.jump(Inst::Jump, next, span);
    }

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

    /// Lowers `scope name { ... }`.
    ///
    /// The Language Card's rule is the whole of it: leaving the scope waits
    /// for or cancels its child tasks. The scope's value is the value of its
    /// block, so a scope is an expression like any other block, and the name
    /// is an ordinary value slot for the length of it — `scope.spawn` reads
    /// its receiver the way every other method call does.
    ///
    /// The `try` written after the `leave-scope` is the whole of what a
    /// failed child does. `Interpreter::leave_scope` answers
    /// `Control::Return(Value::err(error))` for a child whose value was
    /// `Err`, which is what `?` already means here, so the instruction
    /// answers a `Result` and the `try` beside it turns one into the other.
    /// A child that *raised* never reaches the `try`: an error is not a
    /// value, and it propagates as itself.
    ///
    /// A function that answers on the scalar stack is refused rather than
    /// approximated. Every one of its returns is a `return-scalar`, and the
    /// value a failed child returns is a `Value`, so there is no stack for
    /// the failure to travel on. The oracle answers such a program — it
    /// returns an `Err` from a function declared `-> Int` — and this is one
    /// of the few places a backend is allowed to refuse what the oracle
    /// answers rather than reproduce it.
    fn scope_expr(
        &mut self,
        name: &'a cove_syntax::ast::Ident,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.returns.is_scalar() {
            return Err(Unsupported::new(
                "a task scope in a function that answers an `Int` or a `Bool`",
                span,
            ));
        }
        let mark = self.scope();
        let named = self.outer.name(name.node.as_str());
        self.emit(Inst::EnterScope(named), span);
        let slot = self.declare(Some(name.node.as_str()), SlotKind::Value);
        self.emit(Inst::StoreLocal(slot), span);
        self.open_scopes += 1;
        let lowered = self.block_at(body, Position::Value);
        self.open_scopes -= 1;
        lowered?;
        self.emit(Inst::LeaveScope, span);
        self.emit(Inst::Try, span);
        self.release(mark);
        if position == Position::Effect {
            self.emit(Inst::Pop, span);
        }
        Ok(())
    }

    /// `scope.spawn { ... }`: the scope, then the work to run in it.
    ///
    /// The receiver first and then the argument, which is the order
    /// `Interpreter::eval_method_call` evaluates them in.
    fn spawn(
        &mut self,
        receiver: &'a Expr,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, "spawn", 1, span)?;
        self.expr(receiver)?;
        self.expr(args.at(0).value)?;
        self.emit(Inst::Spawn, span);
        Ok(None)
    }

    /// `shared.lock(fn(var value) { ... })`: the cell, then the closure to
    /// run under its lock.
    ///
    /// The closure has to be written at the call, which is narrower than the
    /// oracle: `Interpreter::call_shared_method` takes whatever closure value
    /// it is handed. A `var` parameter names the cell's contents rather than
    /// receiving a copy of them, so it arrives on the place stack — and a
    /// lambda that is lowered as an ordinary value cannot have one, because
    /// every argument of an `Inst::CallValue` travels on the value stack.
    /// Lowering the lambda *here*, as the closure of this `lock`, is what
    /// makes the exception a property of one written site rather than of
    /// every closure a program could hand over.
    fn lock(
        &mut self,
        receiver: &'a Expr,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, "lock", 1, span)?;
        let written = args.at(0).value;
        let ExprKind::Lambda {
            is_async,
            params,
            body,
        } = &written.kind
        else {
            return Err(Unsupported::new(
                "a `lock` whose closure is not written at the call",
                written.span,
            ));
        };
        if params.len() != 1 {
            return Err(Unsupported::new(
                format!(
                    "a `lock` whose closure takes {} parameter(s) rather than one",
                    params.len()
                ),
                written.span,
            ));
        }
        self.expr(receiver)?;
        self.lambda(written, *is_async, params, body, written.span, true)?;
        self.emit(Inst::Lock, span);
        Ok(None)
    }

    /// `task.await()` and `task.cancel()`, which take nothing.
    fn task_op(
        &mut self,
        receiver: &'a Expr,
        inst: Inst,
        what: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, what, 0, span)?;
        self.expr(receiver)?;
        self.emit(inst, span);
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
    fn method_call(
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

/// The name a [`Inst::TestCase`] carries for one pattern path.
///
/// `match_pattern` tests the case name, and — when the path has two or more
/// segments — the enum's own short type name as well, so that
/// `Status.Confirmed` does not match another enum's `Confirmed`. One
/// instruction carries one name, so the two are written as one: a case alone
/// where the pattern named one, and `Type.Case` where it named both. Neither
/// a case name nor a type's short name can contain a `.`, so the pair reads
/// back unambiguously.
///
/// The segments before the last two are not tested, for the reason the
/// interpreter does not test them: `booking.Status.Confirmed` says which
/// module the enum was reached through, and a value carries the module that
/// *declares* it, which are two different questions.
fn case_tested(path: &[cove_syntax::ast::Ident]) -> String {
    let Some(case) = path.last() else {
        // A path with no segments cannot be written, and a test that names
        // nothing matches nothing, which is what `match_pattern` answers for
        // one.
        return String::new();
    };
    if path.len() < 2 {
        return case.node.clone();
    }
    qualified_case(&path[path.len() - 2].node, &case.node)
}

/// A case name written with the short name of the type that declares it,
/// which is the pair [`Inst::TestCase`] tests both halves of.
fn qualified_case(type_name: &str, case: &str) -> String {
    format!("{type_name}.{case}")
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
fn arguments_in_order(
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
struct Args<'a> {
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
struct CallArg<'a> {
    label: Option<&'a cove_syntax::ast::Ident>,
    is_var: bool,
    spread: bool,
    value: &'a Expr,
    span: Span,
}

impl<'a> Args<'a> {
    /// The arguments written inside the parentheses, and the trailing
    /// closure when one was written.
    fn new(written: &'a [Arg], trailing: Option<&'a Expr>) -> Args<'a> {
        Args { written, trailing }
    }

    fn len(self) -> usize {
        self.written.len() + usize::from(self.trailing.is_some())
    }

    fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The argument at `position`, where the trailing closure is the one
    /// past the written ones.
    fn at(self, position: usize) -> CallArg<'a> {
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

    fn iter(self) -> impl Iterator<Item = CallArg<'a>> {
        (0..self.len()).map(move |position| self.at(position))
    }
}

/// Which argument fills each parameter, and which arguments a variadic
/// parameter collects.
struct Arguments {
    /// For each parameter, the position of the argument that fills it, or
    /// `None` where the call left it to its default.
    slots: Vec<Option<usize>>,
    /// The positions of the arguments that fell past the last parameter, in
    /// the order they are written, which a variadic parameter collects.
    rest: Vec<usize>,
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

/// The arguments of a task operation, which takes a fixed number of plain
/// ones and nothing else.
///
/// `spawn`, `await`, `cancel` and `lock` are dispatched by the receiver's
/// kind rather than resolved against a declaration, so there is no signature
/// for a label to name and the interpreter reads one and ignores it. Refusing
/// is the direction a second backend is allowed to be wrong in.
fn task_arguments(args: Args<'_>, what: &str, takes: usize, span: Span) -> Result<(), Unsupported> {
    plain_arguments(args, what)?;
    if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
        return Err(Unsupported::new(
            format!("a labelled argument to `{what}`, which takes none"),
            arg.span,
        ));
    }
    if args.len() != takes {
        return Err(Unsupported::new(
            format!(
                "a `{what}` given {} argument(s) where it takes {takes}",
                args.len()
            ),
            // The first argument where there is one, and the call itself
            // where there is none: a `spawn` given nothing has no argument
            // to point at, and `Args::at` would index past the end.
            args.iter().next().map_or(span, |arg| arg.span),
        ));
    }
    Ok(())
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
fn plain_arguments(args: Args<'_>, what: &str) -> Result<(), Unsupported> {
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
fn scalar_of_ty(ty: &Ty) -> Option<Scalar> {
    match ty {
        Ty::Int => Some(Scalar::Int),
        Ty::Bool => Some(Scalar::Bool),
        _ => None,
    }
}

/// Where a slot of this type lives, which is [`scalar_of_ty`] read as a
/// place rather than as a representation.
fn slot_kind_of(ty: &Ty) -> SlotKind {
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

/// The source binary operator as the IR carries it, or `None` for the two
/// that short-circuit and so are not operators here at all.
fn binary_op(op: SourceBinary) -> Option<BinaryOp> {
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

/// The instruction that writes a slot, which is decided by where the slot is.
fn store_slot(kind: SlotKind, slot: u32) -> Inst {
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

/// What [`Inst::IntBinary`] leaves on the scalar stack.
///
/// Arithmetic answers an `Int` and a comparison answers a `Bool`. The scalar
/// stack carries no tag, so this is where a boundary instruction learns which
/// of the two it is being handed.
fn int_result(op: IntOp) -> Scalar {
    match op {
        IntOp::Add | IntOp::Sub | IntOp::Mul | IntOp::Div | IntOp::Rem => Scalar::Int,
        IntOp::Eq | IntOp::Ne | IntOp::Lt | IntOp::Le | IntOp::Gt | IntOp::Ge => Scalar::Bool,
    }
}

/// The conditional jump that reads the stack a condition was left on.
fn branch_on(scalar: bool) -> fn(u32) -> Inst {
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

/// Refuses a `dyn` written where this pass has no conversion to make.
///
/// A `dyn` value is the language's one implicit conversion, made where a
/// type is *written*, and [`Inst::MakeDyn`] is what makes one. What is left
/// to refuse is a type that mentions `dyn` somewhere the conversion does not
/// reach — a `Map`'s value type, a written function type's parameter — which
/// is exactly where `Interpreter::coerce` leaves the value alone. Lowering
/// those as a conversion would convert something the oracle does not, and
/// lowering them as nothing at all would leave a value unconverted with no
/// record that it was; so they are named instead.
fn reject_dyn(ty: &Type, what: &str) -> Result<(), Unsupported> {
    if mentions_dyn(ty) && dyn_shape(ty).is_none() {
        return Err(Unsupported::new(what, ty.span));
    }
    Ok(())
}

/// Where the `dyn` inside a written type is: the trait it names, and how
/// many `Array` or `Option` layers stand above it.
///
/// The pure half of [`Lowering::dyn_conversion`], which is the half a
/// refusal asks about — whether a conversion exists at all is a question
/// about the shape of the type and not about which module wrote it.
fn dyn_shape(ty: &Type) -> Option<(&str, u16)> {
    match &ty.kind {
        TypeKind::Dyn(name) => Some((name.node.as_str(), 0)),
        TypeKind::Named { path, args } if args.len() == 1 => {
            let head = path.last()?;
            if !matches!(head.node.as_str(), "Array" | "Option") {
                return None;
            }
            let (name, depth) = dyn_shape(&args[0])?;
            Some((name, depth + 1))
        }
        _ => None,
    }
}

/// Whether a type mentions `dyn` anywhere inside it.
fn mentions_dyn(ty: &Type) -> bool {
    match &ty.kind {
        TypeKind::Dyn(_) => true,
        TypeKind::Named { args, .. } => args.iter().any(mentions_dyn),
        TypeKind::Fn {
            params,
            return_type,
            ..
        } => {
            params
                .iter()
                .any(|param| param.ty.as_ref().is_some_and(mentions_dyn))
                || return_type.as_deref().is_some_and(mentions_dyn)
        }
        TypeKind::Unit => false,
    }
}

/// The names `params` were written with, which is all of a written parameter
/// list a lowered [`Function`] keeps.
///
/// [`Function::param_names`] says what became of the rest. They are copied
/// into the program's own string type rather than borrowed, for the reason
/// every other name here is: a [`Program`] outlives the syntax it was lowered
/// from, and every thread of a run reads it.
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
fn reject_parameter(param: &Param, is_last: bool) -> Result<(), Unsupported> {
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
