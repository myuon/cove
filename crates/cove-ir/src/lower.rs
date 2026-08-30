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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cove_diag::{FileId, Span};
use cove_schema::builtins;
use cove_schema::hosts;
use cove_schema::TypeSchema;
use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;
use cove_sema::{MethodTarget, Signature};
use cove_syntax::ast::{
    Arg, BinaryOp as SourceBinary, Block, EnumDecl, Expr, ExprId, ExprKind, FnDecl, GenericParam,
    ItemKind, MatchArm, Param, Pattern, PatternKind, Stmt, StmtKind, StrPart, StructDecl, Type,
    TypeKind, UnaryOp as SourceUnary,
};

use crate::{
    BinaryOp, Const, ConstId, Dispatch, DispatchId, Function, FunctionId, Inst, IntOp, Program,
    Scalar, SlotKind, UnaryOp, Unsupported,
};

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

// -------------------------------------------------------------- the index

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

// --------------------------------------------------------------- one body

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

    // --------------------------------------------------------------- calls

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

    // ---------------------------------------------------------------- tasks

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

/// Every name a body uses as the root of a place, collected before anything
/// is lowered.
///
/// A place is an index into the *value* stack, so a binding a place can be
/// rooted at has to live there — which is a fact about the whole body and
/// not about the statement that declares the binding, since
/// `bump(var total)` is written after `var total = 0` and decides where
/// `total` lives. So the body is walked once first, and
/// [`Body::rooted`] is what the walk found.
///
/// Two forms root a place: a `var` argument, whose root is the name at the
/// bottom of the `a.b.c` it is written as, and the receiver of `freeze`,
/// which is the one builtin that needs the storage handle where it lies
/// rather than a read of it. A `var self` receiver roots one too and is not
/// collected here, because a method that declares one is declared on a
/// struct or an enum and a binding of such a type is a value slot already;
/// `Body::place` refuses rather than guessing if that ever stops being true.
///
/// The answer is a set of *names*, which over-approximates across shadowing
/// on purpose — see [`Body::rooted`] for why that is free.
/// Every name a lambda's body can read from the environment around it.
///
/// This is `crate::interp`'s `mention_block`, and it has to stay that
/// function: what a closure captures is what its body mentions intersected
/// with what is live, and a set that differed from the interpreter's would
/// be a closure holding a different list. The set over-approximates on
/// purpose — a name the body binds for itself is listed too — because
/// capturing a name the body never reads costs one value and missing one
/// leaves the body unable to resolve it. Over-approximating here is *also*
/// harmless in a second way the interpreter does not need: a mentioned name
/// that no binding answers is simply not a capture, since only bindings are
/// walked.
///
/// The borrows are the source's, so the set is `&str` where the
/// interpreter's is `String`. Nothing else differs.
fn mentioned_names(block: &Block) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    mention_block(block, &mut found);
    found
}

fn mention_block<'a>(block: &'a Block, out: &mut BTreeSet<&'a str>) {
    for stmt in &block.statements {
        match &stmt.kind {
            StmtKind::Let { value, .. } => mention_expr(value, out),
            StmtKind::Expr(expr) => mention_expr(expr, out),
            StmtKind::Item(item) => match &item.kind {
                ItemKind::Fn(decl) => mention_fn(decl, out),
                ItemKind::Impl(block) => {
                    for item in &block.items {
                        if let ItemKind::Fn(decl) = &item.kind {
                            mention_fn(decl, out);
                        }
                    }
                }
                // A trait's default bodies are reached through the
                // conformances resolution recorded them under, not through
                // this closure's environment.
                ItemKind::Struct(_)
                | ItemKind::Enum(_)
                | ItemKind::Trait(_)
                | ItemKind::TypeAlias(_) => {}
            },
        }
    }
    if let Some(tail) = &block.tail {
        mention_expr(tail, out);
    }
}

fn mention_fn<'a>(decl: &'a FnDecl, out: &mut BTreeSet<&'a str>) {
    mention_params(&decl.params, out);
    mention_block(&decl.body, out);
}

/// A default argument is evaluated by the callee, so the names it reads
/// belong to the body.
fn mention_params<'a>(params: &'a [Param], out: &mut BTreeSet<&'a str>) {
    for param in params {
        if let Some(default) = &param.default {
            mention_expr(default, out);
        }
    }
}

fn mention_expr<'a>(expr: &'a Expr, out: &mut BTreeSet<&'a str>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit => {}
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    mention_expr(inner, out);
                }
            }
        }
        ExprKind::Ident(name) => {
            out.insert(name.as_str());
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                mention_expr(item, out);
            }
        }
        // A field name is not a binding; only the base can read one.
        ExprKind::Field { base, .. } => mention_expr(base, out),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            mention_expr(callee, out);
            for arg in args {
                mention_expr(&arg.value, out);
            }
            if let Some(trailing) = trailing {
                mention_expr(trailing, out);
            }
        }
        ExprKind::Unary { operand, .. } => mention_expr(operand, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            mention_expr(lhs, out);
            mention_expr(rhs, out);
        }
        ExprKind::Assign { target, value, .. } => {
            mention_expr(target, out);
            mention_expr(value, out);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => mention_expr(inner, out),
        ExprKind::Block(block) => mention_block(block, out),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            mention_expr(condition, out);
            mention_block(then_branch, out);
            if let Some(branch) = else_branch {
                mention_expr(branch, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            mention_expr(scrutinee, out);
            for arm in arms {
                mention_pattern(&arm.pattern, out);
                mention_expr(&arm.body, out);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            mention_expr(iterable, out);
            mention_block(body, out);
        }
        ExprKind::While { condition, body } => {
            mention_expr(condition, out);
            mention_block(body, out);
        }
        ExprKind::Return(inner) | ExprKind::Break(inner) => {
            if let Some(inner) = inner {
                mention_expr(inner, out);
            }
        }
        ExprKind::Continue => {}
        ExprKind::Lambda { params, body, .. } => {
            mention_params(params, out);
            mention_block(body, out);
        }
        // The scope name is bound by the `scope`, so it shadows anything the
        // surrounding environment holds under that name.
        ExprKind::Scope { body, .. } => mention_block(body, out),
        ExprKind::Range { start, end, .. } => {
            mention_expr(start, out);
            mention_expr(end, out);
        }
    }
}

/// Pattern bindings are binders, so only a literal pattern reads a name.
fn mention_pattern<'a>(pattern: &'a Pattern, out: &mut BTreeSet<&'a str>) {
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Binding(_) => {}
        PatternKind::Literal(expr) => mention_expr(expr, out),
        PatternKind::Variant { payload, .. } => {
            for sub in payload {
                mention_pattern(sub, out);
            }
        }
    }
}

fn var_argument_roots(body: &Block) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    walk_block(body, &mut found);
    found
}

/// The name at the bottom of a place expression, or `None` where the
/// expression does not name one.
///
/// `a`, `a.b`, and `a.b.c` all root at `a`, which is `Place::field` carrying
/// its base's identity down unchanged, read here as a question about syntax.
fn place_root(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(name) => Some(name),
        ExprKind::Field { base, .. } => place_root(base),
        _ => None,
    }
}

fn walk_block<'a>(block: &'a Block, found: &mut BTreeSet<&'a str>) {
    for statement in &block.statements {
        match &statement.kind {
            StmtKind::Let { value, .. } => walk_expr(value, found),
            StmtKind::Expr(expr) => walk_expr(expr, found),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, found);
    }
}

/// Walks every expression a body holds, recording the roots as it goes.
///
/// Exhaustive over [`ExprKind`] rather than defaulting, because a form this
/// forgot to walk would be a `var` argument the pre-pass did not see and a
/// binding left on the scalar stack that a place then could not address.
/// `Body::place` would refuse such a program rather than mislower it, but a
/// refusal for a construct the corpus writes is a coverage loss, so the
/// match is written out.
fn walk_expr<'a>(expr: &'a Expr, found: &mut BTreeSet<&'a str>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::Continue => {}
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    walk_expr(inner, found);
                }
            }
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                walk_expr(item, found);
            }
        }
        ExprKind::Field { base, .. } => walk_expr(base, found),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            // `x.freeze()` is a call whose callee is `x.freeze`, so the
            // receiver is inside the callee rather than beside it.
            if let ExprKind::Field { base, name } = &callee.kind {
                if name.node == "freeze" {
                    if let Some(root) = place_root(base) {
                        found.insert(root);
                    }
                }
            }
            walk_expr(callee, found);
            for arg in args {
                if arg.is_var {
                    if let Some(root) = place_root(&arg.value) {
                        found.insert(root);
                    }
                }
                walk_expr(&arg.value, found);
            }
            if let Some(trailing) = trailing {
                walk_expr(trailing, found);
            }
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, found),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, found);
            walk_expr(rhs, found);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_expr(target, found);
            walk_expr(value, found);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => walk_expr(inner, found),
        ExprKind::Block(block) => walk_block(block, found),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, found);
            walk_block(then_branch, found);
            if let Some(branch) = else_branch {
                walk_expr(branch, found);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, found);
            for arm in arms {
                walk_expr(&arm.body, found);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, found);
            walk_block(body, found);
        }
        ExprKind::While { condition, body } => {
            walk_expr(condition, found);
            walk_block(body, found);
        }
        ExprKind::Return(value) | ExprKind::Break(value) => {
            if let Some(value) = value {
                walk_expr(value, found);
            }
        }
        ExprKind::Lambda { body, .. } | ExprKind::Scope { body, .. } => walk_block(body, found),
        ExprKind::Range { start, end, .. } => {
            walk_expr(start, found);
            walk_expr(end, found);
        }
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

/// Neither marking a call site can write, at a call this backend does not
/// route through a declared function's parameters.
///
/// A struct initializer, a host operation, an enum case, a builtin, and a
/// builtin's associated function all take values. None of them declares a
/// `var` parameter, so `var` written at one is a program the interpreter
/// refuses too; and none of them collects a variadic parameter's elements,
/// so a `...` written at one is a marking the interpreter *ignores* — which
/// is refused here instead. See [`no_spread_here`].
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

// -------------------------------------------------------------- the blocks

/// Whether an instruction is the last one of a straight line: after it,
/// control is somewhere the next index does not name.
///
/// The five jumps go elsewhere or fall through, [`Inst::Call`],
/// [`Inst::CallValue`] and [`Inst::CallDyn`] run a whole callee in between,
/// [`Inst::Try`] may leave the frame instead of continuing, and
/// [`Inst::Return`], [`Inst::ReturnScalar`] and [`Inst::NoMatch`] do not
/// continue at all.
fn ends_a_block(inst: Inst) -> bool {
    matches!(
        inst,
        Inst::Jump(_)
            | Inst::JumpIfFalse(_)
            | Inst::JumpIfTrue(_)
            | Inst::JumpIfFalseScalar(_)
            | Inst::JumpIfTrueScalar(_)
            | Inst::Call { .. }
            | Inst::CallValue { .. }
            | Inst::CallDyn { .. }
            | Inst::Try
            | Inst::Return
            | Inst::ReturnScalar
            | Inst::NoMatch
    )
}

/// How many instructions run from each index control can *arrive* at before
/// it can go somewhere else, and 0 at every index it cannot arrive at.
///
/// This is [`Function::block_fuel`], and it is a pass over finished code
/// rather than something the lowering threads through itself: a block's
/// boundaries are readable from the instructions alone, so deriving them here
/// keeps every emitter of a jump from having to know that the VM charges by
/// the block.
///
/// # Arrival, not partition
///
/// The obvious reading — cut the code at every head and let the pieces tile
/// it — is wrong, and wrong in a way that silently loses instructions. An
/// `if` with no `else` inside a loop lowers to a body that *falls* into the
/// join its own conditional jump also targets. The join is a head, because a
/// jump lands on it; but control also reaches it by walking off the end of
/// the block above, and nothing about that walk announces itself. A VM that
/// charged a head only where it jumped to one would never charge that join,
/// and the instructions after it would run for free.
///
/// So a count here is an *extent* and the counts overlap: `block_fuel[h]` is
/// how far the straight line beginning at `h` runs — to the first instruction
/// at or after `h` that ends a block, inclusive. Falling from one head
/// into another is then already paid for, because the extent of the first
/// reaches past the second and out to the same terminator. Jumping to the
/// second pays for the second alone. Both are exact, which is the whole
/// requirement: the instructions charged for a path are the instructions that
/// ran on it.
///
/// A head is the entry, every jump target, and the index after every
/// instruction that ends a block — including after a return, which control
/// never reaches, so that every index has an answer rather than a hole.
///
/// A jump target outside the code, or a straight line that runs off the end,
/// is answered rather than reported. Both are [`validate`]'s to refuse, and
/// this has to answer something for the malformed function it is asked about
/// first.
pub fn block_fuel(code: &[Inst]) -> Vec<u32> {
    if code.is_empty() {
        return Vec::new();
    }
    let mut head = vec![false; code.len()];
    head[0] = true;
    for (pc, inst) in code.iter().enumerate() {
        match *inst {
            Inst::Jump(to)
            | Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                if let Some(target) = head.get_mut(to as usize) {
                    *target = true;
                }
            }
            _ => {}
        }
        if ends_a_block(*inst) {
            if let Some(next) = head.get_mut(pc + 1) {
                *next = true;
            }
        }
    }
    let mut fuel = vec![0u32; code.len()];
    for (at, slot) in fuel.iter_mut().enumerate() {
        if !head[at] {
            continue;
        }
        let mut end = at;
        while end + 1 < code.len() && !ends_a_block(code[end]) {
            end += 1;
        }
        *slot = (end - at + 1) as u32;
    }
    fuel
}

// ---------------------------------------------------------- the invariants

/// Checks the invariants a lowered function must hold before the VM runs it.
///
/// The VM trusts its input completely — that is most of what makes it worth
/// having — so this is where the trust is earned. Every jump lands on an
/// instruction, every slot is inside the frame, every id names something,
/// every recorded argument span belongs to an instruction that exists, every
/// function ends in the return its convention names, and both operand stacks
/// have one depth at every instruction control can reach: a join point
/// arrived at with two different depths is a bug in the lowering, and finding
/// it here is the difference between a failed test and a VM reading a value
/// that is not there.
///
/// A slot is addressed as what it is, too. A slot number names storage in
/// one of the two stacks, and which one is settled at lowering, so a scalar
/// instruction reaching a value slot — or the other way round — is caught
/// here rather than read as whichever eight bytes happened to stand there.
///
/// The calling convention is checked from both ends, which is what makes it
/// an invariant rather than a convention. A function's `params` has one
/// entry per argument and each stack has room for the parameters that live
/// in it; a function ends in the return its `returns` names and holds no
/// instance of the other one; and every `Call` supplies the counts its
/// callee's `params` describe and expects its answer on the stack its
/// callee's `returns` leaves it on.
pub fn validate(program: &Program) -> Result<(), String> {
    for (index, function) in program.functions.iter().enumerate() {
        let id = FunctionId(index as u32);
        validate_function(program, id)
            .map_err(|why| format!("{}.{}: {why}", function.module, function.name))?;
    }
    Ok(())
}

fn validate_function(program: &Program, id: FunctionId) -> Result<(), String> {
    let function = program.function(id);
    if function.code.is_empty() {
        return Err("has no instructions".to_string());
    }
    if function.spans.len() != function.code.len() {
        return Err(format!(
            "carries {} spans for {} instructions",
            function.spans.len(),
            function.code.len()
        ));
    }
    if function.params.len() != function.arity as usize {
        return Err(format!(
            "takes {} arguments but says where {} of them arrive",
            function.arity,
            function.params.len()
        ));
    }
    let value_params = function
        .params
        .iter()
        .filter(|k| matches!(k, SlotKind::Value))
        .count() as u32;
    let scalar_params = function.params.iter().filter(|k| k.is_scalar()).count() as u32;
    let place_params = function.params.iter().filter(|k| k.is_place()).count() as u32;
    if value_params > function.value_frame_size {
        return Err(format!(
            "takes {value_params} value arguments but has a value frame of {}",
            function.value_frame_size
        ));
    }
    if scalar_params > function.scalar_frame_size {
        return Err(format!(
            "takes {scalar_params} scalar arguments but has a scalar frame of {}",
            function.scalar_frame_size
        ));
    }
    if place_params > function.place_frame_size {
        return Err(format!(
            "takes {place_params} place arguments but has a place frame of {}",
            function.place_frame_size
        ));
    }
    // One return instruction per function, decided by where the answer
    // travels: a caller reads exactly the stack `returns` names, and nothing
    // tells it which of the two a given return happened to use. There is no
    // third: a place is what a parameter can be and not what a call can
    // answer, so a function that claimed to answer one is refused here
    // rather than left for a return instruction that does not exist.
    let (ends_in, other, stack) = match function.returns {
        SlotKind::Value => (Inst::Return, Inst::ReturnScalar, "value"),
        SlotKind::Scalar(_) => (Inst::ReturnScalar, Inst::Return, "scalar"),
        SlotKind::Place => return Err("answers a place, which no call reads".to_string()),
    };
    if function.code.last() != Some(&ends_in) {
        return Err(format!("does not end in a `{}`", render_return(ends_in)));
    }
    if function.code.contains(&other) {
        return Err(format!(
            "answers on the {stack} stack and holds a `{}`",
            render_return(other)
        ));
    }
    // The captures stand in the value slots straight after the parameters
    // that arrived on the value stack, put there by the call out of the
    // closure it went through, so `capture_base` has to be that number and
    // the frame has to have room for both. `Inst::LoadCapture` addresses them
    // by index into `captures` rather than by slot, so this is the one place
    // the two are reconciled.
    if function.capture_base != value_params {
        return Err(format!(
            "begins its captures at value slot {} and takes {value_params} value argument(s)",
            function.capture_base
        ));
    }
    if !function.captures.is_empty() {
        // Every argument but a `lock` closure's first one arrives on the
        // value stack, which is what makes a capture's slot a static number:
        // see `Function::capture_base`.
        if scalar_params > 0 || place_params > 1 {
            return Err(format!(
                "holds {} capture(s) and takes {} of its {} arguments off another stack",
                function.captures.len(),
                function.arity - value_params,
                function.arity
            ));
        }
        let window = function.capture_base as usize + function.captures.len();
        if window > function.value_frame_size as usize {
            return Err(format!(
                "holds {} capture(s) after {} value argument(s) in a value frame of {}",
                function.captures.len(),
                function.capture_base,
                function.value_frame_size
            ));
        }
    }
    for at in function.arg_spans.keys() {
        if *at as usize >= function.code.len() {
            return Err(format!(
                "carries argument spans for instruction {at} of {}",
                function.code.len()
            ));
        }
    }

    let length = function.code.len();
    for (pc, inst) in function.code.iter().enumerate() {
        let at = |why: String| format!("{pc}: {why}");
        let constant = |which: ConstId, what: &str| -> Result<(), String> {
            match program.constants.get(which.0 as usize) {
                Some(Const::Name(_)) => Ok(()),
                Some(other) => Err(at(format!("{what} names {other:?} rather than a name"))),
                None => Err(at(format!("{what} names constant {} of none", which.0))),
            }
        };
        match *inst {
            Inst::Const(which) => {
                if program.constants.get(which.0 as usize).is_none() {
                    return Err(at(format!(
                        "loads constant {}, which does not exist",
                        which.0
                    )));
                }
            }
            Inst::LoadLocal(slot) | Inst::StoreLocal(slot) => {
                if slot >= function.value_frame_size {
                    return Err(at(format!(
                        "reaches slot {slot} of a frame of {}",
                        function.value_frame_size
                    )));
                }
            }
            Inst::LoadScalar(slot) | Inst::StoreScalar(slot) => {
                if slot >= function.scalar_frame_size {
                    return Err(at(format!(
                        "reaches slot {slot} of a frame of {}",
                        function.scalar_frame_size
                    )));
                }
            }
            Inst::LoadPlace(slot) => {
                if slot >= function.place_frame_size {
                    return Err(at(format!(
                        "reaches slot {slot} of a frame of {}",
                        function.place_frame_size
                    )));
                }
            }
            // A place names a slot of the *value* stack, which is the whole
            // of why `crate::lower` keeps a binding a place is rooted at
            // there. Checking it here is what makes that a fact rather than
            // an intention.
            Inst::PlaceLocal(slot) => {
                if slot >= function.value_frame_size {
                    return Err(at(format!(
                        "names value slot {slot} of a frame of {}",
                        function.value_frame_size
                    )));
                }
            }
            Inst::LoadCapture(index) => {
                if index as usize >= function.captures.len() {
                    return Err(at(format!(
                        "reaches capture {index} of {}",
                        function.captures.len()
                    )));
                }
            }
            Inst::MakeClosure {
                function: target,
                captures,
            } => {
                let Some(target) = program.functions.get(target.0 as usize) else {
                    return Err(at(format!(
                        "makes a closure of function {}, which does not exist",
                        target.0
                    )));
                };
                if target.captures.len() != usize::from(captures) {
                    return Err(at(format!(
                        "makes a closure of `{}.{}` over {captures} captures, which takes {}",
                        target.module,
                        target.name,
                        target.captures.len()
                    )));
                }
                // The convention `Inst::CallValue` calls under, checked
                // where the closure is *made*, because that is the last
                // point at which the target is known: the call itself
                // reaches whatever value stands there. So a closure can only
                // ever be made of a function that takes its arguments on the
                // value stack and answers on it.
                //
                // With one exception, and it is the one call that does not go
                // through `Inst::CallValue`. The closure `Inst::Lock` is
                // given may take its *first* parameter as a place, because
                // that instruction hands one over itself; every parameter
                // after it is an argument like any other. What keeps such a
                // closure away from a `CallValue` is that `crate::lower`
                // builds one only as the direct argument of a `lock`, where
                // the very next instruction consumes it, so it never becomes
                // a value the program can name.
                if target
                    .params
                    .iter()
                    .skip(1)
                    .any(|kind| !matches!(kind, SlotKind::Value))
                    || target.params.first().is_some_and(|kind| kind.is_scalar())
                {
                    return Err(at(format!(
                        "makes a closure of `{}.{}`, which takes an argument off another stack",
                        target.module, target.name
                    )));
                }
                if target.returns.is_scalar() {
                    return Err(at(format!(
                        "makes a closure of `{}.{}`, which answers on the scalar stack",
                        target.module, target.name
                    )));
                }
            }
            Inst::MakeDyn { trait_name, .. } => constant(trait_name, "the trait")?,
            // The same convention `Inst::MakeClosure` is checked against,
            // and checked here for the same reason: this is the last point
            // at which the targets are known, because the call itself
            // reaches whichever of them the receiver turns out to carry. So
            // every candidate has to take every argument on the value stack
            // and answer on it, and they all have to take the same number.
            Inst::CallDyn { site, argc } => {
                let Some(dispatch) = program.dispatches.get(site.0 as usize) else {
                    return Err(at(format!(
                        "dispatches through site {}, which does not exist",
                        site.0
                    )));
                };
                for (type_name, id) in &dispatch.cases {
                    let Some(target) = program.functions.get(id.0 as usize) else {
                        return Err(at(format!(
                            "dispatches to function {}, which does not exist",
                            id.0
                        )));
                    };
                    if target.arity != u32::from(argc) {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}` with {argc} arguments, which takes {}",
                            dispatch.method, target.arity
                        )));
                    }
                    if target
                        .params
                        .iter()
                        .any(|kind| !matches!(kind, SlotKind::Value))
                    {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which takes an argument off another stack",
                            dispatch.method
                        )));
                    }
                    if target.returns.is_scalar() {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which answers on the scalar stack",
                            dispatch.method
                        )));
                    }
                    if !target.captures.is_empty() {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which is entered through the closure \
                             that holds its {} capture(s)",
                            dispatch.method,
                            target.captures.len()
                        )));
                    }
                }
            }
            Inst::Jump(to)
            | Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                if to as usize >= length {
                    return Err(at(format!("jumps to {to}, past the {length} instructions")));
                }
            }
            Inst::Call {
                function: target,
                value_argc,
                scalar_argc,
                place_argc,
                returns_scalar,
            } => {
                let Some(target) = program.functions.get(target.0 as usize) else {
                    return Err(at(format!(
                        "calls function {}, which does not exist",
                        target.0
                    )));
                };
                let value_argc = u32::from(value_argc);
                let scalar_argc = u32::from(scalar_argc);
                let place_argc = u32::from(place_argc);
                if target.arity != value_argc + scalar_argc + place_argc {
                    return Err(at(format!(
                        "calls `{}.{}` with {} arguments, which takes {}",
                        target.module,
                        target.name,
                        value_argc + scalar_argc + place_argc,
                        target.arity
                    )));
                }
                let values = target
                    .params
                    .iter()
                    .filter(|k| matches!(k, SlotKind::Value))
                    .count() as u32;
                let scalars = target.params.iter().filter(|k| k.is_scalar()).count() as u32;
                let places = target.params.iter().filter(|k| k.is_place()).count() as u32;
                if values != value_argc || scalars != scalar_argc || places != place_argc {
                    return Err(at(format!(
                        "calls `{}.{}` with {value_argc} value, {scalar_argc} scalar and {place_argc} place arguments, which takes {values}, {scalars} and {places}",
                        target.module, target.name
                    )));
                }
                if !target.captures.is_empty() {
                    return Err(at(format!(
                        "calls `{}.{}` directly, which is entered through the closure that \
                         holds its {} capture(s)",
                        target.module,
                        target.name,
                        target.captures.len()
                    )));
                }
                if target.returns.is_scalar() != returns_scalar {
                    return Err(at(format!(
                        "calls `{}.{}` for an answer on the {} stack, which answers on the {}",
                        target.module,
                        target.name,
                        if returns_scalar { "scalar" } else { "value" },
                        if target.returns.is_scalar() {
                            "scalar"
                        } else {
                            "value"
                        }
                    )));
                }
            }
            Inst::CallHost { module, op, .. } => {
                constant(module, "the host module")?;
                constant(op, "the host operation")?;
            }
            Inst::CallResource { op, .. } => constant(op, "the resource operation")?,
            Inst::CallBuiltin { name, .. } => constant(name, "the builtin method")?,
            Inst::MakeBuiltin { name, .. } => constant(name, "the builtin")?,
            Inst::MakeEnum { ty, case, .. } => {
                constant(ty, "the enum")?;
                constant(case, "the case")?;
            }
            Inst::CallBuiltinAssoc { ty, name, .. } => {
                constant(ty, "the builtin type")?;
                constant(name, "the associated function")?;
            }
            Inst::TestCase(case) => constant(case, "the case")?,
            Inst::GetField(name) | Inst::SetField(name) => constant(name, "the field")?,
            Inst::MakeStruct { ty, fields } => {
                constant(ty, "the type")?;
                constant(fields, "the fields")?;
            }
            _ => {}
        }
    }

    // Both operand stacks, simulated over every path control can take. Code
    // no path reaches is not simulated: it cannot be run, so its depths are
    // not a fact about anything.
    let mut depths: Vec<Option<(i64, i64, i64)>> = vec![None; length];
    let mut pending = vec![(0usize, (0i64, 0i64, 0i64))];
    while let Some((pc, depth)) = pending.pop() {
        if pc >= length {
            return Err(format!(
                "{}: control runs past the last instruction",
                pc - 1
            ));
        }
        if let Some(seen) = depths[pc] {
            if seen != depth {
                return Err(format!(
                    "{pc}: reached with {} values, {} scalars and {} places on the stack and with {}, {} and {}",
                    depth.0, depth.1, depth.2, seen.0, seen.1, seen.2
                ));
            }
            continue;
        }
        depths[pc] = Some(depth);
        let inst = function.code[pc];
        let shape = stack_shape(&program.constants, inst);
        if depth.0 < i64::from(shape.values.0) {
            return Err(format!(
                "{pc}: takes {} values off a stack of {}",
                shape.values.0, depth.0
            ));
        }
        if depth.1 < i64::from(shape.scalars.0) {
            return Err(format!(
                "{pc}: takes {} scalars off a stack of {}",
                shape.scalars.0, depth.1
            ));
        }
        if depth.2 < i64::from(shape.places.0) {
            return Err(format!(
                "{pc}: takes {} places off a stack of {}",
                shape.places.0, depth.2
            ));
        }
        let after = (
            depth.0 - i64::from(shape.values.0) + i64::from(shape.values.1),
            depth.1 - i64::from(shape.scalars.0) + i64::from(shape.scalars.1),
            depth.2 - i64::from(shape.places.0) + i64::from(shape.places.1),
        );
        match inst {
            // None continues: a return leaves the frame, whichever stack it
            // reads, and a `match` that covered nothing stops the run.
            Inst::Return | Inst::ReturnScalar | Inst::NoMatch => {}
            Inst::Jump(to) => pending.push((to as usize, after)),
            Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                pending.push((to as usize, after));
                pending.push((pc + 1, after));
            }
            _ => pending.push((pc + 1, after)),
        }
    }

    // The block table, which the VM charges fuel from without looking at it
    // twice. A count is an extent: how far the straight line from that head
    // runs. So each one has to end on an instruction that ends a block and
    // run through no earlier one, and the heads have to be exactly the
    // indices the code names — a head the code does not name is one the VM
    // never arrives at, and a head the table is missing is an arrival that
    // charges nothing.
    if function.block_fuel.len() != length {
        return Err(format!(
            "carries {} block lengths for {length} instructions",
            function.block_fuel.len()
        ));
    }
    for (pc, count) in function.block_fuel.iter().enumerate() {
        let count = *count as usize;
        if count == 0 {
            continue;
        }
        if pc + count > length {
            return Err(format!(
                "{pc}: begins a block of {count}, which runs past the {length} instructions"
            ));
        }
        if let Some(inside) = (pc..pc + count - 1).find(|at| ends_a_block(function.code[*at])) {
            return Err(format!(
                "{pc}: begins a block of {count}, which runs through the one that ends at {inside}"
            ));
        }
        if !ends_a_block(function.code[pc + count - 1]) {
            return Err(format!(
                "{pc}: begins a block of {count}, which ends where control does not"
            ));
        }
    }
    let expected = block_fuel(&function.code);
    if let Some((pc, (held, want))) = function
        .block_fuel
        .iter()
        .zip(&expected)
        .enumerate()
        .find(|(_, (held, want))| held != want)
    {
        return Err(match (held, want) {
            (0, _) => format!("{pc}: begins a block of {want}, and the table begins none there"),
            (_, 0) => format!("{pc}: begins no block, and the table begins one of {held} there"),
            _ => format!("{pc}: begins a block of {want}, and the table says {held}"),
        });
    }
    Ok(())
}

/// How many operands an instruction takes off each stack and puts back.
///
/// Three pairs rather than one, because there are three stacks and an
/// instruction may read one and write another: that is what a boundary
/// instruction *is*, and reading a place is the boundary between the place
/// stack and the value stack.
#[derive(Clone, Copy)]
struct Shape {
    /// Taken off, and put back on, the value stack.
    values: (u32, u32),
    /// Taken off, and put back on, the scalar stack.
    scalars: (u32, u32),
    /// Taken off, and put back on, the place stack.
    places: (u32, u32),
}

impl Shape {
    /// An instruction that touches only the value stack.
    const fn on_values(taken: u32, left: u32) -> Shape {
        Shape {
            values: (taken, left),
            scalars: (0, 0),
            places: (0, 0),
        }
    }

    /// An instruction that touches only the scalar stack.
    const fn on_scalars(taken: u32, left: u32) -> Shape {
        Shape {
            values: (0, 0),
            scalars: (taken, left),
            places: (0, 0),
        }
    }

    /// An instruction that touches only the place stack.
    const fn on_places(taken: u32, left: u32) -> Shape {
        Shape {
            values: (0, 0),
            scalars: (0, 0),
            places: (taken, left),
        }
    }
}

/// How many operands an instruction takes off each stack and puts back.
///
/// One description, read by the lowering as it emits and by [`validate`] as
/// it simulates, so the two cannot disagree about what an instruction does.
fn stack_shape(constants: &[Const], inst: Inst) -> Shape {
    match inst {
        Inst::Const(_) | Inst::LoadLocal(_) | Inst::LoadCapture(_) | Inst::MakeHostEnum { .. } => {
            Shape::on_values(0, 1)
        }
        Inst::StoreLocal(_) | Inst::Pop => Shape::on_values(1, 0),
        Inst::SpreadArgument => Shape::on_values(2, 1),
        Inst::Dup => Shape::on_values(1, 2),
        Inst::Unary(_) | Inst::GetField(_) | Inst::GetFieldAt(_) | Inst::Try | Inst::Snapshot => {
            Shape::on_values(1, 1)
        }
        // The fusion of `Inst::GetFieldAt` with `Inst::ValueToScalar`: the
        // struct it reads is the same one value in, and the field it reads
        // out lands on the other stack.
        Inst::GetFieldAtScalar(_) => Shape {
            values: (1, 0),
            scalars: (0, 1),
            places: (0, 0),
        },
        Inst::Binary(_) | Inst::SetField(_) => Shape::on_values(2, 1),
        // The typed operator is the scalar stack's: two `i64` in, one out.
        Inst::IntBinary(_) => Shape::on_scalars(2, 1),
        Inst::ScalarConst(_) | Inst::LoadScalar(_) => Shape::on_scalars(0, 1),
        Inst::StoreScalar(_)
        | Inst::ScalarPop
        | Inst::JumpIfFalseScalar(_)
        | Inst::JumpIfTrueScalar(_) => Shape::on_scalars(1, 0),
        // The two boundary instructions, and the only ones that move
        // anything between the stacks.
        Inst::ScalarToValue(_) => Shape {
            values: (0, 1),
            scalars: (1, 0),
            places: (0, 0),
        },
        Inst::ValueToScalar => Shape {
            values: (1, 0),
            scalars: (0, 1),
            places: (0, 0),
        },
        // The place stack's own three, and then the two that cross out of
        // it. A place is built and refined where it stands; reading one puts
        // a `Value` on the value stack and writing one takes a `Value` off
        // it, which is the same kind of boundary the two above are.
        Inst::PlaceLocal(_) | Inst::LoadPlace(_) => Shape::on_places(0, 1),
        Inst::PlaceField(_) => Shape::on_places(1, 1),
        Inst::PlacePop => Shape::on_places(1, 0),
        Inst::PlaceRead | Inst::Freeze => Shape {
            values: (0, 1),
            scalars: (0, 0),
            places: (1, 0),
        },
        Inst::PlaceWrite => Shape {
            values: (1, 0),
            scalars: (0, 0),
            places: (1, 0),
        },
        Inst::Jump(_) => Shape::on_values(0, 0),
        Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) | Inst::Return => Shape::on_values(1, 0),
        Inst::ReturnScalar => Shape::on_scalars(1, 0),
        // A call reads each stack for the arguments that arrived on it and
        // leaves its answer on the one its callee's return type named.
        Inst::Call {
            value_argc,
            scalar_argc,
            place_argc,
            returns_scalar,
            ..
        } => Shape {
            values: (u32::from(value_argc), u32::from(!returns_scalar)),
            scalars: (u32::from(scalar_argc), u32::from(returns_scalar)),
            places: (u32::from(place_argc), 0),
        },
        Inst::CallHost { argc, .. } | Inst::MakeBuiltin { argc, .. } => Shape::on_values(argc, 1),
        // The captured values, in the order `Function::captures` names them.
        Inst::MakeClosure { captures, .. } => Shape::on_values(u32::from(captures), 1),
        // The arguments and the callee above them, and one answer back.
        // Every one of them is on the value stack: nothing at a call through
        // a value knows which function it will reach.
        Inst::CallValue { argc } => Shape::on_values(u32::from(argc) + 1, 1),
        // The receiver is the first of the arguments rather than a fourth
        // operand: it is `self`, and it becomes the callee's slot 0.
        Inst::CallDyn { argc, .. } => Shape::on_values(u32::from(argc), 1),
        // One value in and the trait object made of it out, whatever the
        // conversion turned out to reach inside.
        Inst::MakeDyn { .. } => Shape::on_values(1, 1),
        // The receiver sits below the arguments, and for a resource call the
        // receiver is the handle the call is routed by.
        Inst::CallBuiltin { argc, .. } | Inst::CallResource { argc, .. } => {
            Shape::on_values(argc + 1, 1)
        }
        Inst::MakeArray(len) => Shape::on_values(len, 1),
        // Two settled `Int` bounds off the scalar stack, and the one `Range`
        // they make onto the value stack.
        Inst::MakeRange { .. } => Shape {
            values: (0, 1),
            scalars: (2, 0),
            places: (0, 0),
        },
        Inst::Concat(parts) => Shape::on_values(parts, 1),
        Inst::MakeStruct { fields, .. } => Shape::on_values(field_count(constants, fields), 1),
        // A case's payload is what it is built from, and an associated
        // function has no receiver, so both read exactly their arguments.
        Inst::MakeEnum { argc, .. } | Inst::CallBuiltinAssoc { argc, .. } => {
            Shape::on_values(argc, 1)
        }
        // Both peek: a pattern tests the subject and then binds out of it,
        // and the arm after this one needs the subject still there.
        Inst::TestCase(_) | Inst::GetPayload(_) => Shape::on_values(0, 1),
        // The iterable is read and the `Array` of what a `for` walks it as
        // stands where it stood.
        Inst::IterItems => Shape::on_values(1, 1),
        // The value no arm covered is what the message names, so it is read;
        // nothing is put back, because control does not continue.
        Inst::NoMatch => Shape::on_values(1, 0),
        // A scope is a value, so opening one leaves it standing for the
        // `store` that binds it.
        Inst::EnterScope(_) => Shape::on_values(0, 1),
        // The scope's own value in, and the `Result` the `try` below it
        // reads out.
        Inst::LeaveScope => Shape::on_values(1, 1),
        // Nothing at all: what a `break` needs is the waiting, and the scope
        // it waits for is the frame's rather than an operand.
        Inst::CancelScope => Shape::on_values(0, 0),
        // The scope and the closure to run in it, and the handle back.
        Inst::Spawn => Shape::on_values(2, 1),
        // The cell and the closure to run under its lock, and what the
        // closure answered back. The contents the closure is given do not
        // stand on the stack when the instruction begins: `Inst::Lock` puts
        // them there itself, and takes them back before it ends.
        Inst::Lock => Shape::on_values(2, 1),
        // The handle in and the value it settled to out; and the handle in
        // and the `()` a `cancel` answers out.
        Inst::Await | Inst::Cancel => Shape::on_values(1, 1),
    }
}

/// A return instruction as a diagnostic names it.
fn render_return(inst: Inst) -> &'static str {
    match inst {
        Inst::ReturnScalar => "return-scalar",
        _ => "return",
    }
}

/// How many fields a `MakeStruct` takes, read out of the name it carries.
///
/// The names are one comma-separated constant rather than one constant each,
/// because an instruction carries one id and the whole list is what says
/// which pushed value is which field.
fn field_count(constants: &[Const], fields: ConstId) -> u32 {
    match constants.get(fields.0 as usize) {
        Some(Const::Name(names)) if names.is_empty() => 0,
        Some(Const::Name(names)) => names.split(',').count() as u32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use cove_diag::SourceMap;
    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};

    /// Checks one module of source the way `cove run` checks a package:
    /// parse, resolve, and type-check.
    ///
    /// Both halves, because the second is what settles a type and the
    /// lowering reads those: a program that was only resolved carries no
    /// facts, so every listing taken from one would show the untyped
    /// instruction and would say nothing about the rule that picks between
    /// them.
    ///
    /// The module is called `m`, so a listing reads `fn m.something` and a
    /// test asserts on the whole of it.
    fn checked(source: &str) -> Checked {
        let mut sources = SourceMap::new();
        let file = sources.add("m/main.cove", source.to_string());
        let ast = match cove_syntax::parse_file(&sources, file) {
            Ok(ast) => ast,
            Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
        };
        let package = Package {
            root: PathBuf::from("."),
            config: Config::default(),
            modules: BTreeMap::from([(
                "m".to_string(),
                Module {
                    name: "m".to_string(),
                    dir: PathBuf::from("m"),
                    units: vec![Unit {
                        file,
                        path: PathBuf::from("m/main.cove"),
                        ast,
                    }],
                },
            )]),
        };
        match cove_sema::Compiler::new().compile(&package) {
            Ok(program) => program,
            Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
        }
    }

    fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
        items
            .iter()
            .map(|item| cove_diag::render(sources, item))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rendered instructions of one lowered function, with the whole
    /// program validated first.
    ///
    /// Every listing test asserts the whole listing rather than a line of
    /// it, so a change that moves an instruction is a test that fails rather
    /// than a test that still passes for a reason nobody meant.
    fn listing(source: &str, name: &str) -> String {
        let program = lower(&checked(source)).expect("the program lowers");
        validate(&program).expect("the lowering holds the VM's invariants");
        let id = program
            .function_named("m", name)
            .unwrap_or_else(|| panic!("`{name}` was lowered"));
        crate::render(&program, id)
    }

    /// The listing of the specialisation of `name` that takes `arity`
    /// arguments.
    ///
    /// A call that leaves a parameter to its default reaches a function of
    /// its own, so a name is no longer one function and [`listing`] — which
    /// asks for the first one of that name — would always answer the whole
    /// one. The arity is what tells the specialisations apart, because each
    /// takes exactly what its call site supplies.
    fn specialisation(source: &str, name: &str, arity: u32) -> String {
        let program = lower(&checked(source)).expect("the program lowers");
        validate(&program).expect("the lowering holds the VM's invariants");
        let id = program
            .functions
            .iter()
            .position(|function| &*function.name == name && function.arity == arity)
            .map(|index| FunctionId(index as u32))
            .unwrap_or_else(|| panic!("`{name}` was lowered taking {arity} argument(s)"));
        crate::render(&program, id)
    }

    /// What stopped the lowering, in the words it reported.
    fn refused(source: &str) -> String {
        match lower(&checked(source)) {
            Ok(_) => panic!("the program lowered, and was expected not to"),
            Err(why) => why.what,
        }
    }

    /// The whole `examples/` package, checked.
    ///
    /// The evidence that a package is not a program: it holds eleven
    /// `[run.<name>]` entries, and `callbacks/` holds a closure the lowering
    /// refuses. Nothing about `hello` changes because of that.
    fn examples() -> Checked {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut sources = SourceMap::new();
        let package = match cove_sema::package::load(&root, &mut sources) {
            Ok(package) => package,
            Err(items) => panic!(
                "the examples package loads:\n{}",
                rendered(&sources, &items)
            ),
        };
        match cove_sema::Compiler::new().compile(&package) {
            Ok(program) => program,
            Err(items) => panic!(
                "the examples package checks:\n{}",
                rendered(&sources, &items)
            ),
        }
    }

    /// The `benches/` package with only the module `name` kept.
    ///
    /// [`lower`] is all-or-nothing over a package, so keeping one module at a
    /// time is what lets a test say which entry lowers rather than only that
    /// some entry did not.
    fn bench(name: &str) -> Checked {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches");
        let mut sources = SourceMap::new();
        let mut package = match cove_sema::package::load(&root, &mut sources) {
            Ok(package) => package,
            Err(items) => panic!("the benches package loads:\n{}", rendered(&sources, &items)),
        };
        let module = package
            .modules
            .remove(name)
            .unwrap_or_else(|| panic!("`benches/{name}` is a module of the package"));
        package.modules = BTreeMap::from([(name.to_string(), module)]);
        match cove_sema::Compiler::new().compile(&package) {
            Ok(program) => program,
            Err(items) => panic!(
                "the benches package checks:\n{}",
                rendered(&sources, &items)
            ),
        }
    }

    // ------------------------------------------------ one construct each

    /// A lambda is a [`Function`] like any other, and what the environment
    /// around it handed over is the list beside it.
    ///
    /// The order of the three is the whole story. `adder` reads `by` — off
    /// the scalar stack, because that is where its own parameter lives, and
    /// across to the value stack, because a capture is a value — and then
    /// `make-closure` pairs it with the name. `main` pushes the argument,
    /// then the callee above it, and `call-value` takes both. And the
    /// closure's own body reaches `by` by index rather than by slot,
    /// although the slot is `arity + 0` and a `load 1` would have found it.
    #[test]
    fn a_lambda_is_a_function_over_the_values_it_captured() {
        let source = "fn adder(by: Int) -> fn(Int) -> Int {\n  \
                      fn(n: Int) {\n    n + by\n  }\n}\n\n\
                      fn main() -> Int {\n  let add = adder(3)\n  add(4)\n}\n";
        assert_eq!(
            listing(source, "adder"),
            "fn m.adder arity=1 frame=0/1 params=[Int] -> value\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-to-value Int\n\
             \x20  2  make-closure m.<closure 0> captures=1\n\
             \x20  3  return\n"
        );
        assert_eq!(
            listing(source, "main"),
            "fn m.main arity=0 frame=1/0 -> Int\n\
             \x20  0  scalar-const 3\n\
             \x20  1  call m.adder argc=0/1\n\
             \x20  2  store 0\n\
             \x20  3  const Int(4)\n\
             \x20  4  load 0\n\
             \x20  5  call-value argc=1\n\
             \x20  6  value-to-scalar\n\
             \x20  7  return-scalar\n"
        );
        assert_eq!(
            listing(source, "<closure 0>"),
            "fn m.<closure 0> arity=1 frame=2/0 params=[value] captures=[by] -> value\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  capture 0\n\
             \x20  3  value-to-scalar\n\
             \x20  4  int Add\n\
             \x20  5  scalar-to-value Int\n\
             \x20  6  return\n"
        );
    }

    /// A declared function used as a value is a closure over nothing, and it
    /// is not the function a direct call reaches.
    ///
    /// `twice` is called both ways here. The direct call takes its argument
    /// on the scalar stack and answers there, because the checker settled
    /// `Int` at both ends; the specialisation a closure is made of takes it
    /// on the value stack and answers there, because `call-value` reads
    /// exactly those and has no callee to have asked. The body is the same
    /// body, lowered twice, with the boundary instructions in the second one
    /// where the first needed none — which is what a binding the checker
    /// abstained about already gets.
    #[test]
    fn a_function_used_as_a_value_is_a_closure_over_nothing() {
        let source = "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n\
                      fn f() -> Int {\n  let g = twice\n  twice(1) + g(2)\n}\n";
        assert_eq!(
            specialisation(source, "twice", 1),
            "fn m.twice arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Mul\n\
             \x20  3  return-scalar\n"
        );
        let program = lower(&checked(source)).expect("the program lowers");
        validate(&program).expect("the lowering holds the VM's invariants");
        let boxed = program
            .functions
            .iter()
            .position(|function| {
                &*function.name == "twice" && matches!(function.returns, SlotKind::Value)
            })
            .expect("`twice` is lowered a second time, as a value");
        assert_eq!(
            crate::render(&program, FunctionId(boxed as u32)),
            "fn m.twice arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 2\n\
             \x20  3  int Mul\n\
             \x20  4  scalar-to-value Int\n\
             \x20  5  return\n"
        );
    }

    /// `f(x) { ... }` is sugar: the trailing closure is the last positional
    /// argument and is lowered exactly where a written one would be.
    ///
    /// The two listings are the same listing but for the constant, which is
    /// the assertion: `Interpreter::eval_args` pushes the trailing one onto
    /// the end of the evaluated arguments with no label, no `var` and no
    /// spread, and that is all this reproduces.
    #[test]
    fn a_trailing_closure_lands_where_a_written_argument_would() {
        let apply = "fn apply(v: Int, t: fn() -> Int) -> Int {\n  v + t()\n}\n\n";
        let trailing = listing(
            &format!("{apply}fn f() -> Int {{\n  apply(5) {{ 3 }}\n}}\n"),
            "f",
        );
        let written = listing(
            &format!("{apply}fn f() -> Int {{\n  apply(5, fn() {{ 3 }})\n}}\n"),
            "f",
        );
        assert_eq!(trailing, written);
        assert_eq!(
            trailing,
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  scalar-const 5\n\
             \x20  1  make-closure m.<closure 0> captures=0\n\
             \x20  2  call m.apply argc=1/1 -> scalar\n\
             \x20  3  return-scalar\n"
        );
    }

    /// The oracle captures by value at creation time, and a `var` binding is
    /// no exception: `place-read` is what `Env::captures` calls, and it is
    /// what keeps a place from leaving the frame that built it.
    #[test]
    fn a_var_parameter_is_captured_as_the_value_its_place_names() {
        let source = "fn g(var total: Int) -> fn() -> Int {\n  \
                      fn() {\n    total\n  }\n}\n";
        assert_eq!(
            listing(source, "g"),
            "fn m.g arity=1 frame=0/0/1 params=[place] -> value\n\
             \x20  0  load-place 0\n\
             \x20  1  place-read\n\
             \x20  2  make-closure m.<closure 0> captures=1\n\
             \x20  3  return\n"
        );
    }

    /// A name the module answers is not a capture. `Env::captures` walks
    /// bindings, so a declaration, a type, and a host module resolve where
    /// they always did — and a lambda that mentions one holds nothing for
    /// it.
    #[test]
    fn only_a_binding_is_captured() {
        let source = "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n\
                      fn f() -> fn(Int) -> Int {\n  fn(n: Int) {\n    twice(n)\n  }\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  make-closure m.<closure 0> captures=0\n\
             \x20  1  return\n"
        );
    }

    /// A lambda inside a lambda captures out of the outer one's captures as
    /// well as out of its parameters, and in the order `Env::captures`
    /// produces them: the captures the enclosing body was handed first, its
    /// own bindings after.
    #[test]
    fn a_nested_lambda_captures_out_of_the_captures_it_stands_in() {
        let source = "fn f(a: Int) -> fn(Int) -> fn() -> Int {\n  \
                      fn(b: Int) {\n    fn() {\n      a + b\n    }\n  }\n}\n";
        assert_eq!(
            listing(source, "<closure 0>"),
            "fn m.<closure 0> arity=1 frame=2/0 params=[value] captures=[a] -> value\n\
             \x20  0  capture 0\n\
             \x20  1  load 0\n\
             \x20  2  make-closure m.<closure 1> captures=2\n\
             \x20  3  return\n"
        );
        assert_eq!(
            listing(source, "<closure 1>"),
            "fn m.<closure 1> arity=0 frame=2/0 captures=[a, b] -> value\n\
             \x20  0  capture 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  capture 1\n\
             \x20  3  value-to-scalar\n\
             \x20  4  int Add\n\
             \x20  5  scalar-to-value Int\n\
             \x20  6  return\n"
        );
    }

    /// A name declared twice is captured once, holding what the innermost
    /// declaration holds — which is `Env::captures` overwriting the value it
    /// recorded and keeping the position it recorded it at.
    #[test]
    fn a_shadowed_name_is_captured_once_and_at_its_latest_binding() {
        let source = "fn f() -> fn() -> Int {\n  \
                      let a = 1\n  let b = 2\n  let a = 3\n  \
                      fn() {\n    a + b\n  }\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/3 -> value\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 1\n\
             \x20  4  scalar-const 3\n\
             \x20  5  store-scalar 2\n\
             \x20  6  load-scalar 2\n\
             \x20  7  scalar-to-value Int\n\
             \x20  8  load-scalar 1\n\
             \x20  9  scalar-to-value Int\n\
             \x20 10  make-closure m.<closure 0> captures=2\n\
             \x20 11  return\n"
        );
    }

    #[test]
    fn every_literal_loads_one_constant() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  let a = 1\n  let b = 1.5\n  let c = true\n  let d = 250ms\n  let e = ()\n  let g = \"hi\"\n  a\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=4/2 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Float(1.5)\n\
             \x20  3  store 0\n\
             \x20  4  scalar-const 1\n\
             \x20  5  store-scalar 1\n\
             \x20  6  const Duration(250000000)\n\
             \x20  7  store 1\n\
             \x20  8  const Unit\n\
             \x20  9  store 2\n\
             \x20 10  const Str(\"hi\")\n\
             \x20 11  store 3\n\
             \x20 12  load-scalar 0\n\
             \x20 13  return-scalar\n"
        );
    }

    #[test]
    fn an_interpolated_string_renders_its_parts_left_to_right() {
        assert_eq!(
            listing("fn f(n: Int) -> String {\n  \"tick {n}!\"\n}\n", "f"),
            "fn m.f arity=1 frame=0/1 params=[Int] -> value\n\
             \x20  0  const Str(\"tick \")\n\
             \x20  1  load-scalar 0\n\
             \x20  2  scalar-to-value Int\n\
             \x20  3  const Str(\"!\")\n\
             \x20  4  concat 3\n\
             \x20  5  return\n"
        );
    }

    #[test]
    fn a_string_with_nothing_interpolated_is_one_constant() {
        assert_eq!(
            listing("fn f() -> String {\n  \"tick\"\n}\n", "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  const Str(\"tick\")\n\
             \x20  1  return\n"
        );
    }

    /// An assignment written as a statement stores, and stops there.
    ///
    /// The store is the whole of what an assignment does, so the `()` it
    /// would answer is not built and there is nothing for a `Pop` to take
    /// away. `x += 3` still reads the slot, adds, and writes it back, because
    /// lowering for effect removes a value and never an operation.
    #[test]
    fn an_assignment_written_as_a_statement_builds_no_value() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var x = 1\n  x = 2\n  x += 3\n  x\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/1 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 0\n\
             \x20  4  load-scalar 0\n\
             \x20  5  scalar-const 3\n\
             \x20  6  int Add\n\
             \x20  7  store-scalar 0\n\
             \x20  8  load-scalar 0\n\
             \x20  9  return-scalar\n"
        );
    }

    /// An assignment whose value is read still answers `()`.
    ///
    /// A block's tail is its value, so this one is lowered for value and the
    /// `()` an assignment means is built exactly as it was. Both halves of
    /// the rule are golden, because the saving is only correct if this is
    /// unchanged.
    #[test]
    fn an_assignment_whose_value_is_read_still_answers_unit() {
        assert_eq!(
            listing("fn f() -> Unit {\n  var x = 1\n  x = 2\n}\n", "f"),
            "fn m.f arity=0 frame=0/1 -> value\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 0\n\
             \x20  4  const Unit\n\
             \x20  5  return\n"
        );
    }

    #[test]
    fn operands_are_evaluated_left_to_right() {
        assert_eq!(
            listing(
                "fn f(a: Int, b: Int) -> Bool {\n  a * b / a % b - a + b != a\n}\n",
                "f"
            ),
            "fn m.f arity=2 frame=0/2 params=[Int, Int] -> Bool\n\
             \x20  0  load-scalar 0\n\
             \x20  1  load-scalar 1\n\
             \x20  2  int Mul\n\
             \x20  3  load-scalar 0\n\
             \x20  4  int Div\n\
             \x20  5  load-scalar 1\n\
             \x20  6  int Rem\n\
             \x20  7  load-scalar 0\n\
             \x20  8  int Sub\n\
             \x20  9  load-scalar 1\n\
             \x20 10  int Add\n\
             \x20 11  load-scalar 0\n\
             \x20 12  int Ne\n\
             \x20 13  return-scalar\n"
        );
    }

    /// The operator carries a type only where the checker settled one.
    ///
    /// Three additions in one listing: two `Int`, two `Float`, and two
    /// `Duration`. Only the first is integer arithmetic, so only the first
    /// is `int Add`; the other two keep the operator that looks at what it
    /// was handed, because `Float` and `Duration` are not `Int` and the rule
    /// is not "a number". Reading all three from one function is what makes
    /// the rule visible rather than three tests that each happen to agree.
    ///
    /// An operand the checker *abstained* about is not written here because
    /// it cannot be: `Ty::Unknown` accompanies a diagnostic, and a program
    /// with one does not reach the lowering. The rule that it is not `Int`
    /// is stated where it is read, in `Body::is_int`.
    #[test]
    fn an_addition_is_typed_only_where_the_checker_settled_int() {
        assert_eq!(
            listing(
                "fn f(a: Int, b: Int, c: Float, d: Float, e: Duration, g: Duration) -> Duration {\n  let n = a + b\n  let x = c + d\n  e + g\n}\n",
                "f"
            ),
            "fn m.f arity=6 frame=5/3 params=[Int, Int, value, value, value, value] -> value\n\
             \x20  0  load-scalar 0\n\
             \x20  1  load-scalar 1\n\
             \x20  2  int Add\n\
             \x20  3  store-scalar 2\n\
             \x20  4  load 0\n\
             \x20  5  load 1\n\
             \x20  6  binary Add\n\
             \x20  7  store 4\n\
             \x20  8  load 2\n\
             \x20  9  load 3\n\
             \x20 10  binary Add\n\
             \x20 11  return\n"
        );
    }

    /// A field of a receiver the checker settled is read by position.
    ///
    /// Both fields, so that the position is read as a position rather than
    /// as a zero that happens to be right.
    #[test]
    fn a_field_of_a_settled_struct_is_read_by_position() {
        assert_eq!(
            listing(
                "struct P {\n  x: Int\n  y: Int\n}\n\nfn f(p: P) -> Int {\n  p.x + p.y\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at-scalar 0\n\
             \x20  2  load 0\n\
             \x20  3  get-field-at-scalar 1\n\
             \x20  4  int Add\n\
             \x20  5  return-scalar\n"
        );
    }

    /// A `Bool` field is branched on directly where its receiver's position
    /// and its own type are both settled: `Inst::GetFieldAtScalar` puts it on
    /// the scalar stack and `Inst::JumpIfFalseScalar` reads it there, with no
    /// `Value` built for a condition that is never wanted as one.
    #[test]
    fn a_bool_field_as_a_condition_never_builds_a_value() {
        assert_eq!(
            listing(
                "struct P {\n  ready: Bool\n}\n\nfn f(p: P) -> Int {\n  if p.ready {\n    1\n  } else {\n    0\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at-scalar 0\n\
             \x20  2  jump-if-false-scalar 5\n\
             \x20  3  scalar-const 1\n\
             \x20  4  jump 6\n\
             \x20  5  scalar-const 0\n\
             \x20  6  return-scalar\n"
        );
    }

    /// `MapEntry` is a builtin, not a struct this package declares, so its
    /// fields have a settled type but no knowable position — the same reason
    /// [`Inst::GetFieldAt`] declines it. The fusion answers only where both
    /// halves are settled, so an `Int` field still lowers to
    /// [`Inst::GetField`] rather than guessing a position for it.
    #[test]
    fn a_field_of_an_unsettled_position_still_lowers_by_name() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  MapEntry(key: \"a\", value: 1).value + 1\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"a\")\n\
             \x20  1  const Int(1)\n\
             \x20  2  make-builtin MapEntry argc=2\n\
             \x20  3  get-field value\n\
             \x20  4  value-to-scalar\n\
             \x20  5  scalar-const 1\n\
             \x20  6  int Add\n\
             \x20  7  return-scalar\n"
        );
    }

    /// A name a builtin type and a declared type both answer to reaches the
    /// one the receiver's type names, and both are written here.
    ///
    /// This used to refuse the whole program: a name reached two answers and
    /// the lowering had no way to choose, so declaring `Box.length` anywhere
    /// in a package stopped `[1, 2, 3].length()` lowering everywhere in it.
    /// The checker settles the receiver's type, which is the only thing that
    /// ever decided it.
    #[test]
    fn a_name_a_builtin_and_a_declared_type_share_reaches_what_the_receiver_names() {
        assert_eq!(
            listing(
                "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn length(self) -> Int {\n    self.n\n  }\n}\n\nfn f(b: Box) -> Int {\n  b.length() + [1, 2, 3].length()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  call m.Box.length argc=1/0 -> scalar\n\
             \x20  2  const Int(1)\n\
             \x20  3  const Int(2)\n\
             \x20  4  const Int(3)\n\
             \x20  5  make-array 3\n\
             \x20  6  call-builtin length argc=0\n\
             \x20  7  value-to-scalar\n\
             \x20  8  int Add\n\
             \x20  9  return-scalar\n"
        );
    }

    #[test]
    fn a_unary_operator_applies_to_what_was_pushed() {
        assert_eq!(
            listing(
                "fn f(b: Bool) -> Int {\n  if !b {\n    return -1\n  }\n  0\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/1 params=[Bool] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-to-value Bool\n\
             \x20  2  unary Not\n\
             \x20  3  jump-if-false 8\n\
             \x20  4  const Int(1)\n\
             \x20  5  unary Neg\n\
             \x20  6  value-to-scalar\n\
             \x20  7  return-scalar\n\
             \x20  8  scalar-const 0\n\
             \x20  9  return-scalar\n"
        );
    }

    #[test]
    fn and_and_or_short_circuit_through_jumps() {
        assert_eq!(
            listing("fn f(a: Bool, b: Bool) -> Bool {\n  a && b || a\n}\n", "f"),
            "fn m.f arity=2 frame=0/2 params=[Bool, Bool] -> Bool\n\
             \x20  0  load-scalar 0\n\
             \x20  1  jump-if-false-scalar 4\n\
             \x20  2  load-scalar 1\n\
             \x20  3  jump 5\n\
             \x20  4  scalar-const 0\n\
             \x20  5  jump-if-true-scalar 8\n\
             \x20  6  load-scalar 0\n\
             \x20  7  jump 9\n\
             \x20  8  scalar-const 1\n\
             \x20  9  return-scalar\n"
        );
    }

    /// The scalar form of `&&`/`||` is declined where neither operand is
    /// already on the scalar stack: `MapEntry`'s fields are settled types but
    /// not positions — [`Inst::GetFieldAt`] is for a struct this package
    /// declares, and a builtin one still reads by name — so both operands
    /// here cost a `ValueToScalar` to reach the scalar stack, which is
    /// exactly what the value form's own single `ValueToScalar` on the
    /// answer is cheaper than.
    #[test]
    fn and_over_two_values_still_lowers_through_jumps() {
        assert_eq!(
            listing(
                "fn f(s: MapEntry<String, Bool>, t: MapEntry<String, Bool>) -> Bool {\n  s.value && t.value\n}\n",
                "f"
            ),
            "fn m.f arity=2 frame=2/0 params=[value, value] -> Bool\n\
             \x20  0  load 0\n\
             \x20  1  get-field value\n\
             \x20  2  jump-if-false 6\n\
             \x20  3  load 1\n\
             \x20  4  get-field value\n\
             \x20  5  jump 7\n\
             \x20  6  const Bool(false)\n\
             \x20  7  value-to-scalar\n\
             \x20  8  return-scalar\n"
        );
    }

    /// A `&&` over scalar `Bool` parameters used as an `if` condition is
    /// branched on directly, with no `Value` built for it at all.
    #[test]
    fn and_of_scalar_bools_as_a_condition_never_builds_a_value() {
        assert_eq!(
            listing(
                "fn f(a: Bool, b: Bool) -> Int {\n  if a && b {\n    1\n  } else {\n    0\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=2 frame=0/2 params=[Bool, Bool] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  jump-if-false-scalar 4\n\
             \x20  2  load-scalar 1\n\
             \x20  3  jump 5\n\
             \x20  4  scalar-const 0\n\
             \x20  5  jump-if-false-scalar 8\n\
             \x20  6  scalar-const 1\n\
             \x20  7  jump 9\n\
             \x20  8  scalar-const 0\n\
             \x20  9  return-scalar\n"
        );
    }

    #[test]
    fn a_block_with_no_tail_is_unit() {
        assert_eq!(
            listing("fn f() -> Unit {\n  let a = 1\n}\n", "f"),
            "fn m.f arity=0 frame=0/1 -> value\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Unit\n\
             \x20  3  return\n"
        );
    }

    /// A block whose tail is an `if`/`else` keeps what both branches build.
    ///
    /// This is the other half of the rule. The block is the function's body
    /// and its value is what the function returns, so the `if` is lowered for
    /// value, and so is each of its branches — both of which are blocks with
    /// no tail, which is what a `const Unit` in a listing means.
    #[test]
    fn a_block_whose_tail_is_an_if_else_still_builds_both_values() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Unit {\n  {\n    if n < 2 {\n      let a = 1\n    } else {\n      let b = 2\n    }\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/2 params=[Int] -> value\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Lt\n\
             \x20  3  jump-if-false-scalar 8\n\
             \x20  4  scalar-const 1\n\
             \x20  5  store-scalar 1\n\
             \x20  6  const Unit\n\
             \x20  7  jump 11\n\
             \x20  8  scalar-const 2\n\
             \x20  9  store-scalar 1\n\
             \x20 10  const Unit\n\
             \x20 11  return\n"
        );
    }

    /// `let x = if c { 1 } else { 2 }` reads the `if`, so both branches
    /// answer and the store takes whichever one ran.
    #[test]
    fn a_let_of_an_if_else_stores_the_branch_that_ran() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  let x = if n < 2 {\n    1\n  } else {\n    2\n  }\n  x\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/2 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Lt\n\
             \x20  3  jump-if-false-scalar 6\n\
             \x20  4  scalar-const 1\n\
             \x20  5  jump 7\n\
             \x20  6  scalar-const 2\n\
             \x20  7  store-scalar 1\n\
             \x20  8  load-scalar 1\n\
             \x20  9  return-scalar\n"
        );
    }

    #[test]
    fn an_if_with_an_else_joins_both_branches() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    n - 1\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Lt\n\
             \x20  3  jump-if-false-scalar 6\n\
             \x20  4  load-scalar 0\n\
             \x20  5  jump 9\n\
             \x20  6  load-scalar 0\n\
             \x20  7  scalar-const 1\n\
             \x20  8  int Sub\n\
             \x20  9  return-scalar\n"
        );
    }

    /// An `if` with no `else` written as a statement builds nothing.
    ///
    /// It is `()` however it goes — there is no second branch to give the
    /// missing case a value — so as a statement there is no value to build in
    /// either direction, and its branch is lowered for effect too.
    #[test]
    fn an_if_with_no_else_written_as_a_statement_builds_no_value() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  var t = 0\n  if n < 2 {\n    t = 1\n  }\n  t\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/2 params=[Int] -> Int\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 2\n\
             \x20  4  int Lt\n\
             \x20  5  jump-if-false-scalar 8\n\
             \x20  6  scalar-const 1\n\
             \x20  7  store-scalar 1\n\
             \x20  8  load-scalar 1\n\
             \x20  9  return-scalar\n"
        );
    }

    /// The same `if` whose value is read is still `()` however it goes.
    #[test]
    fn an_if_with_no_else_whose_value_is_read_is_still_unit() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Unit {\n  var t = 0\n  if n < 2 {\n    t = 1\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/2 params=[Int] -> value\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 2\n\
             \x20  4  int Lt\n\
             \x20  5  jump-if-false-scalar 8\n\
             \x20  6  scalar-const 1\n\
             \x20  7  store-scalar 1\n\
             \x20  8  const Unit\n\
             \x20  9  return\n"
        );
    }

    /// A `while` written as a statement builds nothing, in the body or at the
    /// end.
    ///
    /// A loop is `()` however it leaves, so its body's value is never wanted
    /// and neither is its own here: four instructions of test, four of body,
    /// and the jump back, with no `Unit` anywhere in it.
    #[test]
    fn a_while_loop_tests_at_the_top_and_jumps_back() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var i = 0\n  while i < 3 {\n    i += 1\n  }\n  i\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/1 -> Int\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 3\n\
             \x20  4  int Lt\n\
             \x20  5  jump-if-false-scalar 11\n\
             \x20  6  load-scalar 0\n\
             \x20  7  scalar-const 1\n\
             \x20  8  int Add\n\
             \x20  9  store-scalar 0\n\
             \x20 10  jump 2\n\
             \x20 11  load-scalar 0\n\
             \x20 12  return-scalar\n"
        );
    }

    /// The source every variadic test below is a call written into.
    const VARIADIC: &str = "fn join(sep: String, items: String...) -> Int {\n  items.length()\n}\n";

    /// One `join(...)` call, lowered as the body of `f`.
    fn variadic_call(call: &str) -> String {
        listing(
            &format!("{VARIADIC}\nfn f() -> Int {{\n  {call}\n}}\n"),
            "f",
        )
    }

    /// A variadic parameter is one value slot, and the arguments that fill
    /// it are collected into it at the call site.
    ///
    /// This is the whole of the change: the callee still receives exactly
    /// one argument per parameter — `argc=2/0` for two parameters — so the
    /// calling convention does not move at all. `make-array` is where three
    /// arguments become the two the callee is called with.
    #[test]
    fn a_variadic_call_collects_its_arguments_into_one() {
        assert_eq!(
            variadic_call("join(\"-\", \"a\", \"b\")"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  const Str(\"a\")\n\
             \x20  2  const Str(\"b\")\n\
             \x20  3  make-array 2\n\
             \x20  4  call m.join argc=2/0 -> scalar\n\
             \x20  5  return-scalar\n"
        );
    }

    /// A variadic parameter given nothing is an empty `Array`, not a missing
    /// argument.
    ///
    /// `Interpreter::assign_labels` leaves its slot empty and `rest` empty,
    /// and `bind_params` builds `Value::Array` out of the two, so the callee
    /// is still called with one argument for every parameter.
    #[test]
    fn a_variadic_parameter_given_nothing_is_an_empty_array() {
        assert_eq!(
            variadic_call("join(\"-\")"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  make-array 0\n\
             \x20  2  call m.join argc=2/0 -> scalar\n\
             \x20  3  return-scalar\n"
        );
        assert_eq!(
            listing(
                "fn count(items: Int...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  count()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  make-array 0\n\
             \x20  1  call m.count argc=1/0 -> scalar\n\
             \x20  2  return-scalar\n"
        );
    }

    /// A variadic parameter is a value slot even where its element type is
    /// one the scalar stack holds.
    ///
    /// `items: Int...` is an `Array<Int>` inside the body, and `params=[value]`
    /// is what says the lowering read that rather than the `Int` the checker
    /// recorded: `record_signature` stores a variadic parameter as what it
    /// was *written* as, which is the element type, so asking the signature
    /// here would have numbered the slot in the scalar stack and the callee
    /// would have loaded a word where an array was pushed.
    #[test]
    fn a_variadic_parameter_of_ints_is_still_a_value_slot() {
        assert_eq!(
            listing(
                "fn count(items: Int...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  count(1, 2)\n}\n",
                "count"
            ),
            "fn m.count arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  call-builtin length argc=0\n\
             \x20  2  value-to-scalar\n\
             \x20  3  return-scalar\n"
        );
    }

    /// A label on the variadic parameter, written in its own place, is one
    /// element.
    ///
    /// `assign_labels` puts a labelled argument in that parameter's slot and
    /// `bind_params` makes it the array's first element; no positional
    /// argument may follow a label, so there is nothing else for the array
    /// to hold. The call `join("-", "a", items: "b")` is the one where that
    /// stops being true: the interpreter answers `["b", "a"]`, the labelled
    /// argument before the one that fell past it, and pushing them as
    /// written would have them the other way round. It is refused by name.
    #[test]
    fn a_labelled_variadic_argument_is_one_element() {
        assert_eq!(
            variadic_call("join(\"-\", items: \"a\")"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  const Str(\"a\")\n\
             \x20  2  make-array 1\n\
             \x20  3  call m.join argc=2/0 -> scalar\n\
             \x20  4  return-scalar\n"
        );
        assert_eq!(
            refused(&format!(
                "{VARIADIC}\nfn f() -> Int {{\n  join(\"-\", \"a\", items: \"b\")\n}}\n"
            )),
            "a call to `join` that labels its variadic parameter and passes more"
        );
    }

    /// A `...` passes an existing sequence where a variadic parameter's
    /// elements would go.
    ///
    /// `bind_params` reads an `Array`'s elements or a `Vector`'s and extends
    /// the array it is building with them, so the callee still receives one
    /// `Array` and the calling convention does not move. The call with no
    /// spread in it still lowers to the single `make-array` it always did.
    #[test]
    fn a_spread_argument_extends_the_array_a_variadic_parameter_receives() {
        assert_eq!(
            variadic_call("join(\"-\", ...[\"a\", \"b\"])"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  make-array 0\n\
             \x20  2  const Str(\"a\")\n\
             \x20  3  const Str(\"b\")\n\
             \x20  4  make-array 2\n\
             \x20  5  spread-argument\n\
             \x20  6  call m.join argc=2/0 -> scalar\n\
             \x20  7  return-scalar\n"
        );
    }

    /// A spread mixed with ordinary arguments builds the array in runs.
    ///
    /// Each run of ordinary arguments is one `make-array`, each spread is
    /// its own value, and `spread-argument` joins each piece to what came
    /// before — which is the order `bind_params` walks `rest` in.
    #[test]
    fn a_spread_mixed_with_ordinary_arguments_builds_the_array_in_runs() {
        assert_eq!(
            variadic_call("join(\"-\", \"w\", ...[\"a\"], \"z\")"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  make-array 0\n\
             \x20  2  const Str(\"w\")\n\
             \x20  3  make-array 1\n\
             \x20  4  spread-argument\n\
             \x20  5  const Str(\"a\")\n\
             \x20  6  make-array 1\n\
             \x20  7  spread-argument\n\
             \x20  8  const Str(\"z\")\n\
             \x20  9  make-array 1\n\
             \x20 10  spread-argument\n\
             \x20 11  call m.join argc=2/0 -> scalar\n\
             \x20 12  return-scalar\n"
        );
    }

    /// Everywhere a variadic parameter is not, a `...` is a marking the
    /// interpreter ignores, and this refuses rather than reproducing it.
    ///
    /// `println(...["a"])` hands `console.println` one `Array` and fails
    /// against the schema; `k(...[1, 2, 3])` binds the whole array to `k`'s
    /// one parameter; and `join("-", items: ...["a"])` makes the array one
    /// element long, because `bind_params` reads a labelled variadic slot
    /// through `value_of` and never looks at `spread`. None of the three is
    /// a spread doing anything, so none of them lowers.
    #[test]
    fn a_spread_that_collects_nothing_is_refused() {
        assert_eq!(
            refused(
                "use console.println\n\nfn f() -> Result<Unit, Error> {\n  println(...[\"a\"])\n}\n"
            ),
            "a `...` spread argument to `println`, which collects nothing"
        );
        assert_eq!(
            refused(
                "fn k(a: Array<Int>) -> Int {\n  a.length()\n}\n\nfn f() -> Int {\n  k(...[1, 2, 3])\n}\n"
            ),
            "a `...` spread argument to `k`, which collects nothing"
        );
        assert_eq!(
            refused(&format!(
                "{VARIADIC}\nfn f() -> Int {{\n  join(\"-\", items: ...[\"a\"])\n}}\n"
            )),
            "a `...` spread argument to `join`, which collects nothing"
        );
    }

    /// The two shapes a variadic parameter can be written in that nothing
    /// has decided a meaning for.
    ///
    /// Both parse and both check. A variadic parameter that is not the last
    /// one is an array of at most one element, because `assign_labels`
    /// gathers `rest` only for the last parameter while `bind_params` wraps
    /// any variadic one; and a default written on a variadic parameter is
    /// unreachable, because `bind_params` tests `variadic` first and
    /// `continue`s.
    #[test]
    fn the_variadic_shapes_nothing_decided_a_meaning_for_are_refused() {
        assert_eq!(
            refused(
                "fn f(items: Int..., last: Int) -> Int {\n  last\n}\n\nfn g() -> Int {\n  f(1, last: 2)\n}\n"
            ),
            "a variadic parameter that is not the last one"
        );
        assert_eq!(
            refused(
                "fn f(items: Int... = 1) -> Int {\n  items.length()\n}\n\nfn g() -> Int {\n  f()\n}\n"
            ),
            "a variadic parameter written with a default"
        );
    }

    /// A call that leaves a parameter to its default reaches a function
    /// whose prologue computes it.
    ///
    /// The default is evaluated by the callee — `bind_params` reaches
    /// `None => match &param.default` inside the frame it is filling — so
    /// the caller pushes only what it wrote and the callee's first
    /// instructions are the ones the interpreter runs at the same moment.
    /// The supplied parameter is slot 0 and the defaulted one slot 1,
    /// because a specialisation numbers the supplied parameters first and
    /// that is the whole of what keeps the calling convention where it was.
    #[test]
    fn a_parameter_left_to_its_default_is_computed_by_the_callee() {
        let source =
            "fn g(a: Int, b: Int = 2) -> Int {\n  a + b\n}\n\nfn f() -> Int {\n  g(1)\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  call m.g argc=0/1 -> scalar\n\
             \x20  2  return-scalar\n"
        );
        assert_eq!(
            specialisation(source, "g", 1),
            "fn m.g arity=1 frame=0/2 params=[Int] -> Int\n\
             \x20  0  scalar-const 2\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load-scalar 0\n\
             \x20  3  load-scalar 1\n\
             \x20  4  int Add\n\
             \x20  5  return-scalar\n"
        );
    }

    /// Two call sites that supply different parameters are two functions,
    /// and two that supply the same ones are one.
    #[test]
    fn a_supplied_set_is_what_a_call_numbers() {
        let source = "fn g(a: Int, b: Int = 2) -> Int {\n  a + b\n}\n\nfn f() -> Int {\n  g(1) + g(1, 3) + g(4)\n}\n";
        let program = lower(&checked(source)).expect("the program lowers");
        validate(&program).expect("the lowering holds the VM's invariants");
        assert_eq!(
            program
                .functions
                .iter()
                .filter(|function| &*function.name == "g")
                .map(|function| function.arity)
                .collect::<Vec<_>>(),
            [2, 1]
        );
    }

    /// A label may skip a parameter, because the one it skips has a default.
    ///
    /// `assign_labels` matches a label to the parameter of that name and
    /// only refuses one whose parameter stands before a parameter already
    /// filled, so `measure(3, prefix: "d")` fills the first and the third.
    /// The arguments still reach the callee in declaration order, which is
    /// what lets them be pushed as written.
    #[test]
    fn a_label_may_skip_a_parameter_that_has_a_default() {
        let source = "fn measure(value: Int, unit: String = \"m\", prefix: String = \"length\") -> String {\n  \"{prefix}: {value}{unit}\"\n}\n\nfn f() -> String {\n  measure(3, prefix: \"d\")\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  scalar-const 3\n\
             \x20  1  const Str(\"d\")\n\
             \x20  2  call m.measure argc=1/1\n\
             \x20  3  return\n"
        );
        assert_eq!(
            specialisation(source, "measure", 2),
            "fn m.measure arity=2 frame=2/1 params=[Int, value] -> value\n\
             \x20  0  const Str(\"m\")\n\
             \x20  1  store 1\n\
             \x20  2  load 0\n\
             \x20  3  const Str(\": \")\n\
             \x20  4  load-scalar 0\n\
             \x20  5  scalar-to-value Int\n\
             \x20  6  load 1\n\
             \x20  7  concat 4\n\
             \x20  8  return\n"
        );
    }

    /// A default may read a parameter declared before it, and refuses on one
    /// declared after it.
    ///
    /// `bind_params` declares each parameter as its own turn comes, so the
    /// environment a default is evaluated in holds the parameters before it
    /// and not the ones after. `fn f(a: Int = b, b: Int = 1)` is therefore
    /// ``cannot find `b` in this scope`` on the interpreter, whatever the
    /// call site supplies. A specialisation names its parameters in the same
    /// order for the same reason, so the name is not one this body can
    /// resolve and the lowering says so rather than reading a slot nothing
    /// has written.
    #[test]
    fn a_default_reads_the_parameters_before_it_and_no_others() {
        assert_eq!(
            specialisation(
                "fn g(a: Int, b: Int = a * 2) -> Int {\n  b\n}\n\nfn f() -> Int {\n  g(3)\n}\n",
                "g",
                1
            ),
            "fn m.g arity=1 frame=0/2 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Mul\n\
             \x20  3  store-scalar 1\n\
             \x20  4  load-scalar 1\n\
             \x20  5  return-scalar\n"
        );
        assert_eq!(
            refused("fn g(a: Int = b, b: Int = 1) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g()\n}\n"),
            "`b`, a name the lowering cannot resolve"
        );
    }

    /// A default before a variadic parameter is evaluated by the callee like
    /// any other.
    ///
    /// The two rules meet without either giving way: the call supplies the
    /// variadic parameter, because an empty `Array` is an argument, and it
    /// does not supply `sep`, so the specialisation it reaches takes one
    /// argument and computes the other.
    #[test]
    fn a_default_before_a_variadic_parameter_is_computed_by_the_callee() {
        let source = "fn join(sep: String = \"-\", items: String...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  join()\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  make-array 0\n\
             \x20  1  call m.join argc=1/0 -> scalar\n\
             \x20  2  return-scalar\n"
        );
        assert_eq!(
            specialisation(source, "join", 1),
            "fn m.join arity=1 frame=2/0 params=[value] -> Int\n\
             \x20  0  const Str(\"-\")\n\
             \x20  1  store 1\n\
             \x20  2  load 0\n\
             \x20  3  call-builtin length argc=0\n\
             \x20  4  value-to-scalar\n\
             \x20  5  return-scalar\n"
        );
    }

    /// A range used as a value builds one, from two bounds on the scalar
    /// stack.
    ///
    /// The bounds are the checker's own answer about them — `a range runs
    /// between two `Int`s` is what it checks each against — so they are
    /// pushed the way every other settled operand is, and `make-range` is
    /// where the two words become the one `Value` a `Range` is.
    #[test]
    fn a_range_used_as_a_value_is_built_from_two_scalar_bounds() {
        assert_eq!(
            listing("fn f() -> Range {\n  0..<3\n}\n", "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  scalar-const 0\n\
             \x20  1  scalar-const 3\n\
             \x20  2  make-range ..<\n\
             \x20  3  return\n"
        );
    }

    /// `..` and `..<` are one instruction apart, and the difference is the
    /// flag rather than the bounds.
    ///
    /// It is not normalised away, because it is observable: `Value::eq_value`
    /// compares it, `Display` writes the operator back out, and `0..<3` and
    /// `0..2` are two values that yield the same integers.
    #[test]
    fn an_inclusive_range_value_differs_only_in_the_flag() {
        let inclusive = listing("fn f() -> Range {\n  0..3\n}\n", "f");
        let exclusive = listing("fn f() -> Range {\n  0..<3\n}\n", "f");
        assert!(inclusive.contains("   2  make-range ..\n"), "{inclusive}");
        assert_eq!(
            inclusive.replace("make-range ..", "make-range ..<"),
            exclusive
        );
    }

    /// A `Range` bound to a name, and asked one of its builtin methods.
    ///
    /// The bounds need not be constants: a parameter the checker settled as
    /// `Int` is already on the scalar stack, so it is loaded from there and
    /// nothing crosses a boundary on the way in. `length()` is
    /// `cove_schema::builtins::RANGE`'s own method and reaches
    /// `builtins::call_method`, which is the interpreter's, so the two
    /// backends compute it with one piece of code.
    #[test]
    fn a_range_can_be_bound_and_asked_its_methods() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  let r = 1..n\n  r.length()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/1 params=[Int] -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  load-scalar 0\n\
             \x20  2  make-range ..\n\
             \x20  3  store 0\n\
             \x20  4  load 0\n\
             \x20  5  call-builtin length argc=0\n\
             \x20  6  value-to-scalar\n\
             \x20  7  return-scalar\n"
        );
    }

    /// A range header never asks `iter-items` for anything.
    ///
    /// It builds no value at all — not even the `Range` `make-range` exists
    /// to build: the bounds go into two hidden slots and the loop counts
    /// between them, which is faster than materialising every element and
    /// answers exactly what walking the range's items would.
    #[test]
    fn a_for_over_a_range_counts_between_two_hidden_slots() {
        let listed = listing(
            "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
            "f",
        );
        assert!(!listed.contains("iter-items"), "{listed}");
        assert!(!listed.contains("make-range"), "{listed}");
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=3/1 -> Int\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Int(0)\n\
             \x20  3  store 0\n\
             \x20  4  const Int(3)\n\
             \x20  5  store 1\n\
             \x20  6  load 0\n\
             \x20  7  load 1\n\
             \x20  8  binary Lt\n\
             \x20  9  jump-if-false 22\n\
             \x20 10  load 0\n\
             \x20 11  store 2\n\
             \x20 12  load-scalar 0\n\
             \x20 13  load 2\n\
             \x20 14  value-to-scalar\n\
             \x20 15  int Add\n\
             \x20 16  store-scalar 0\n\
             \x20 17  load 0\n\
             \x20 18  const Int(1)\n\
             \x20 19  binary Add\n\
             \x20 20  store 0\n\
             \x20 21  jump 6\n\
             \x20 22  load-scalar 0\n\
             \x20 23  return-scalar\n"
        );
    }

    /// `a..b` yields `b` and `a..<b` stops before it, which is the one
    /// difference between the two headers.
    #[test]
    fn an_inclusive_range_tests_with_le() {
        let inclusive = listing(
            "fn f() -> Int {\n  var t = 0\n  for i in 0..3 {\n    t += i\n  }\n  t\n}\n",
            "f",
        );
        let exclusive = listing(
            "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
            "f",
        );
        assert!(inclusive.contains("   8  binary Le\n"), "{inclusive}");
        assert_eq!(inclusive.replace("binary Le", "binary Lt"), exclusive);
    }

    /// A `for` over a sequence asks `iter-items` what it walks it as, once,
    /// and walks the `Array` that comes back by index.
    ///
    /// The instruction is what makes the loop right for a `Map` and a `Set`
    /// as well: they answer neither `length()` nor `get(i)`, and the walk
    /// never asks them to, because what it walks is the `Array` of their
    /// items rather than the collection itself.
    #[test]
    fn a_for_over_a_sequence_asks_for_its_items_and_walks_them_by_index() {
        assert_eq!(
            listing(
                "fn f(items: Array<Int>) -> Int {\n  var t = 0\n  for item in items {\n    t += item\n  }\n  t\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=5/1 params=[value] -> Int\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load 0\n\
             \x20  3  iter-items\n\
             \x20  4  store 1\n\
             \x20  5  load 1\n\
             \x20  6  call-builtin length argc=0\n\
             \x20  7  store 2\n\
             \x20  8  const Int(0)\n\
             \x20  9  store 3\n\
             \x20 10  load 3\n\
             \x20 11  load 2\n\
             \x20 12  binary Lt\n\
             \x20 13  jump-if-false 29\n\
             \x20 14  load 1\n\
             \x20 15  load 3\n\
             \x20 16  call-builtin get argc=1\n\
             \x20 17  try\n\
             \x20 18  store 4\n\
             \x20 19  load-scalar 0\n\
             \x20 20  load 4\n\
             \x20 21  value-to-scalar\n\
             \x20 22  int Add\n\
             \x20 23  store-scalar 0\n\
             \x20 24  load 3\n\
             \x20 25  const Int(1)\n\
             \x20 26  binary Add\n\
             \x20 27  store 3\n\
             \x20 28  jump 10\n\
             \x20 29  load-scalar 0\n\
             \x20 30  return-scalar\n"
        );
    }

    /// `break` leaves the loop and `continue` lands where the cursor is
    /// advanced, so skipping the rest of a body still makes progress.
    #[test]
    fn break_leaves_the_loop_and_continue_reaches_the_next_iteration() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i += 1\n    if i == 2 {\n      continue\n    }\n    if i == 5 {\n      break\n    }\n  }\n  i\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/1 -> Int\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 10\n\
             \x20  4  int Lt\n\
             \x20  5  jump-if-false-scalar 21\n\
             \x20  6  load-scalar 0\n\
             \x20  7  scalar-const 1\n\
             \x20  8  int Add\n\
             \x20  9  store-scalar 0\n\
             \x20 10  load-scalar 0\n\
             \x20 11  scalar-const 2\n\
             \x20 12  int Eq\n\
             \x20 13  jump-if-false-scalar 15\n\
             \x20 14  jump 2\n\
             \x20 15  load-scalar 0\n\
             \x20 16  scalar-const 5\n\
             \x20 17  int Eq\n\
             \x20 18  jump-if-false-scalar 20\n\
             \x20 19  jump 21\n\
             \x20 20  jump 2\n\
             \x20 21  load-scalar 0\n\
             \x20 22  return-scalar\n"
        );
    }

    #[test]
    fn a_call_reaches_a_declaration_and_a_function_reaches_itself() {
        assert_eq!(
            listing(
                "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n",
                "fib"
            ),
            "fn m.fib arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  int Lt\n\
             \x20  3  jump-if-false-scalar 6\n\
             \x20  4  load-scalar 0\n\
             \x20  5  jump 15\n\
             \x20  6  load-scalar 0\n\
             \x20  7  scalar-const 1\n\
             \x20  8  int Sub\n\
             \x20  9  call m.fib argc=0/1 -> scalar\n\
             \x20 10  load-scalar 0\n\
             \x20 11  scalar-const 2\n\
             \x20 12  int Sub\n\
             \x20 13  call m.fib argc=0/1 -> scalar\n\
             \x20 14  int Add\n\
             \x20 15  return-scalar\n"
        );
    }

    #[test]
    fn arguments_are_pushed_left_to_right() {
        assert_eq!(
            listing(
                "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1, 2)\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  scalar-const 2\n\
             \x20  2  call m.g argc=0/2 -> scalar\n\
             \x20  3  return-scalar\n"
        );
    }

    /// The convention itself, in the smallest program that states it: a
    /// parameter the checker settled travels on the scalar stack and becomes
    /// the callee's scalar slot, and the answer comes back the same way.
    /// Nothing crosses between the stacks, on either side of the call.
    #[test]
    fn a_settled_parameter_and_a_settled_answer_travel_on_the_scalar_stack() {
        let source =
            "fn identity(value: Int) -> Int {\n  value\n}\n\nfn f() -> Int {\n  identity(1)\n}\n";
        assert_eq!(
            listing(source, "identity"),
            "fn m.identity arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  return-scalar\n"
        );
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  call m.identity argc=0/1 -> scalar\n\
             \x20  2  return-scalar\n"
        );
    }

    /// Each argument goes to the stack its own parameter names, and within
    /// each stack they land in the order that stack's slots are numbered in
    /// — which is why nothing has to be moved once they are pushed.
    ///
    /// `g`'s frame is one value slot and one scalar slot, and `tag` is value
    /// slot 0 while `n` is scalar slot 0: the numbering is dense inside each
    /// stack and says nothing about the other.
    #[test]
    fn an_argument_travels_on_the_stack_its_own_type_names() {
        let source = "fn g(n: Int, tag: String, k: Int) -> String {\n  tag\n}\n\nfn f() -> String {\n  g(1, \"a\", 2)\n}\n";
        assert_eq!(
            listing(source, "g"),
            "fn m.g arity=3 frame=1/2 params=[Int, value, Int] -> value\n\
             \x20  0  load 0\n\
             \x20  1  return\n"
        );
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  scalar-const 1\n\
             \x20  1  const Str(\"a\")\n\
             \x20  2  scalar-const 2\n\
             \x20  3  call m.g argc=1/2\n\
             \x20  4  return\n"
        );
    }

    /// A receiver is pushed first because it is the first thing `params`
    /// names, and it goes to its own stack like any other argument — which
    /// is the value stack, because a method is declared on a struct or an
    /// enum.
    #[test]
    fn a_receiver_is_the_first_argument_and_travels_on_its_own_stack() {
        let source = "struct P {\n  x: Int\n}\n\nimpl P {\n  fn plus(self, by: Int) -> Int {\n    self.x + by\n  }\n}\n\nfn f(p: P) -> Int {\n  p.plus(by: 2)\n}\n";
        assert_eq!(
            listing(source, "P.plus"),
            "fn m.P.plus arity=2 frame=1/1 params=[value, Int] receiver -> Int\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  int Add\n\
             \x20  4  return-scalar\n"
        );
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  scalar-const 2\n\
             \x20  2  call m.P.plus argc=1/1 -> scalar\n\
             \x20  3  return-scalar\n"
        );
    }

    /// A scalar answer crosses only where something on the other stack reads
    /// it: one boundary instruction where a value is wanted, the scalar
    /// stack's own discard where nothing is, and neither where a scalar was
    /// wanted anyway.
    #[test]
    fn a_scalar_answer_crosses_only_where_a_value_reads_it() {
        let source = "fn g() -> Int {\n  1\n}\n\nfn f() -> String {\n  g()\n  let n = g() + 1\n  \"{g()}\"\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=0/1 -> value\n\
             \x20  0  call m.g argc=0/0 -> scalar\n\
             \x20  1  scalar-pop\n\
             \x20  2  call m.g argc=0/0 -> scalar\n\
             \x20  3  scalar-const 1\n\
             \x20  4  int Add\n\
             \x20  5  store-scalar 0\n\
             \x20  6  call m.g argc=0/0 -> scalar\n\
             \x20  7  scalar-to-value Int\n\
             \x20  8  concat 1\n\
             \x20  9  return\n"
        );
    }

    /// A `Bool` a call left on the scalar stack is branched on where it
    /// stands, rather than moved across to be tested.
    #[test]
    fn a_bool_a_call_answered_is_tested_where_it_stands() {
        let source = "fn big(n: Int) -> Bool {\n  n > 2\n}\n\nfn f(n: Int) -> Int {\n  if big(n) {\n    1\n  } else {\n    0\n  }\n}\n";
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  call m.big argc=0/1 -> scalar\n\
             \x20  2  jump-if-false-scalar 5\n\
             \x20  3  scalar-const 1\n\
             \x20  4  jump 6\n\
             \x20  5  scalar-const 0\n\
             \x20  6  return-scalar\n"
        );
    }

    const STRUCT_AND_METHOD: &str = "struct P {\n  x: Int\n  y: Int\n}\n\nimpl P {\n  fn sum(self) -> Int {\n    self.x + self.y\n  }\n}\n\nfn f() -> Int {\n  let p = P(x: 1, y: 2)\n  p.sum() + p.x\n}\n";

    #[test]
    fn a_struct_is_built_in_declaration_order_and_read_by_field() {
        assert_eq!(
            listing(STRUCT_AND_METHOD, "f"),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  make-struct m.P fields=x,y\n\
             \x20  3  store 0\n\
             \x20  4  load 0\n\
             \x20  5  call m.P.sum argc=1/0 -> scalar\n\
             \x20  6  load 0\n\
             \x20  7  get-field-at-scalar 0\n\
             \x20  8  int Add\n\
             \x20  9  return-scalar\n"
        );
    }

    #[test]
    fn a_method_takes_its_receiver_in_slot_zero() {
        assert_eq!(
            listing(STRUCT_AND_METHOD, "P.sum"),
            "fn m.P.sum arity=1 frame=1/0 params=[value] receiver -> Int\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at-scalar 0\n\
             \x20  2  load 0\n\
             \x20  3  get-field-at-scalar 1\n\
             \x20  4  int Add\n\
             \x20  5  return-scalar\n"
        );
    }

    /// `snapshot` splits in two by the receiver's type, not by its name.
    ///
    /// A struct with an `impl Snapshot for Type` is an ordinary method call:
    /// the checker recorded which declaration it reaches, so it lowers to a
    /// `Call` before the name is asked about at all. Everything a
    /// conformance is not consulted about is the `snapshot` instruction.
    #[test]
    fn snapshot_is_a_call_where_a_conformance_answers_and_an_instruction_where_none_does() {
        assert_eq!(
            listing(
                "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(b: B) -> B {\n  b.snapshot()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  call m.B.snapshot argc=1/0\n\
             \x20  2  return\n"
        );
        assert_eq!(
            listing(
                "fn f(v: Vector<Int>) -> Vector<Int> {\n  v.snapshot()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  snapshot\n\
             \x20  2  return\n"
        );
    }

    /// A `Vector` whose elements dispatch is refused, and so is a receiver
    /// the checker settled nothing about.
    ///
    /// `Interpreter::snapshot` walks a `Vector` one element at a time and
    /// sends each struct to its own conformance. An instruction cannot run a
    /// whole Cove function in the middle of itself, so the refusal is here,
    /// before the run, rather than a failure during one.
    #[test]
    fn snapshot_refuses_where_it_would_have_to_reach_a_conformance() {
        assert_eq!(
            refused(
                "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(v: Vector<B>) -> Vector<B> {\n  v.snapshot()\n}\n"
            ),
            "`snapshot` on a `Vector<B>`, which a conformance answers"
        );
    }

    /// A type a host module declares is built here rather than asked for
    /// across the boundary.
    ///
    /// `Interpreter::init_host_type` is `init_struct` with the field names
    /// read from a `TypeSchema`: the same `assign_labels`, one value per
    /// field, and an ordinary `Value::Struct` named `{module}.{Name}`. So it
    /// lowers to `make-struct`, the same instruction a declared struct's
    /// initializer lowers to, and no `call-host` is emitted for it.
    #[test]
    fn a_type_a_host_declares_is_built_like_any_other_struct() {
        assert_eq!(
            listing(
                "use http\n\nfn f() -> Int {\n  http.Response(status: 200, body: \"ok\").status\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Int(200)\n\
             \x20  1  const Str(\"ok\")\n\
             \x20  2  make-struct http.Response fields=status,body\n\
             \x20  3  get-field status\n\
             \x20  4  value-to-scalar\n\
             \x20  5  return-scalar\n"
        );
    }

    #[test]
    fn a_host_operation_is_called_through_its_module() {
        assert_eq!(
            listing(
                "use console.println\nuse clock\n\nfn f() -> Result<Unit, Error> {\n  let at = clock.now()\n  println(\"at {at}\")?\n  Ok(())\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> value\n\
             \x20  0  call-host clock.now argc=0\n\
             \x20  1  store 0\n\
             \x20  2  const Str(\"at \")\n\
             \x20  3  load 0\n\
             \x20  4  concat 2\n\
             \x20  5  call-host console.println argc=1\n\
             \x20  6  try\n\
             \x20  7  pop\n\
             \x20  8  const Unit\n\
             \x20  9  make-builtin Ok argc=1\n\
             \x20 10  return\n"
        );
    }

    #[test]
    fn a_resource_operation_is_called_through_the_handle_it_stands_on() {
        assert_eq!(
            listing(
                "use http\n\nfn f() -> Result<Unit, Error> {\n  let server = http.listen(0)?\n  server.close()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> value\n\
             \x20  0  const Int(0)\n\
             \x20  1  call-host http.listen argc=1\n\
             \x20  2  try\n\
             \x20  3  store 0\n\
             \x20  4  load 0\n\
             \x20  5  call-resource close argc=0\n\
             \x20  6  return\n"
        );
    }

    #[test]
    fn a_resource_operation_takes_its_handle_below_its_arguments() {
        assert_eq!(
            listing(
                "use files\n\nfn f(line: String) -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(line)\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=2/0 params=[value] -> value\n\
             \x20  0  const Str(\"notes.txt\")\n\
             \x20  1  call-host files.create argc=1\n\
             \x20  2  try\n\
             \x20  3  store 1\n\
             \x20  4  load 1\n\
             \x20  5  load 0\n\
             \x20  6  call-resource writeLine argc=1\n\
             \x20  7  return\n"
        );
    }

    #[test]
    fn a_builtin_method_takes_its_receiver_below_its_arguments() {
        assert_eq!(
            listing(
                "fn f(items: Array<Int>) -> Int {\n  items.get(0).unwrapOr(7)\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  const Int(0)\n\
             \x20  2  call-builtin get argc=1\n\
             \x20  3  const Int(7)\n\
             \x20  4  call-builtin unwrapOr argc=1\n\
             \x20  5  value-to-scalar\n\
             \x20  6  return-scalar\n"
        );
    }

    #[test]
    fn a_free_builtin_is_built_from_its_arguments() {
        assert_eq!(
            listing(
                "fn f() -> Result<Unit, Error> {\n  assertEqual(1, 1)?\n  Ok(())\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(1)\n\
             \x20  2  make-builtin assertEqual argc=2\n\
             \x20  3  try\n\
             \x20  4  pop\n\
             \x20  5  const Unit\n\
             \x20  6  make-builtin Ok argc=1\n\
             \x20  7  return\n"
        );
    }

    /// An assertion carries the spans of its arguments, and nothing else
    /// does.
    ///
    /// A failing `assert` quotes the source text of its condition — that is
    /// what makes it a builtin rather than a library function — and an
    /// instruction's own span covers the whole call, so the argument's span
    /// is recorded beside the instruction. A constructor quotes nothing, so
    /// it carries nothing: a span no diagnostic reads would be a cost with
    /// no reader.
    #[test]
    fn an_assertion_carries_the_spans_of_its_arguments() {
        let source = "fn f() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n";
        let program = lower(&checked(source)).expect("it lowers");
        validate(&program).expect("it holds the invariants");
        let function = program.function(program.function_named("m", "f").expect("`f` is lowered"));
        let made: Vec<usize> = function
            .code
            .iter()
            .enumerate()
            .filter(|(_, inst)| matches!(inst, Inst::MakeBuiltin { .. }))
            .map(|(pc, _)| pc)
            .collect();
        let [assertion, constructor] = made[..] else {
            panic!("the body builds the assertion and the `Ok`: {made:?}");
        };
        let quoted: Vec<&str> = function
            .arg_spans_at(assertion)
            .iter()
            .map(|span| &source[span.start as usize..span.end as usize])
            .collect();
        assert_eq!(quoted, ["1 + 1", "3"]);
        assert!(function.arg_spans_at(constructor).is_empty());
    }

    /// `None` is the one builtin case written as a bare name rather than as
    /// a call, so it is the one that builds from no arguments.
    #[test]
    fn none_is_built_from_nothing() {
        assert_eq!(
            listing("fn f() -> Option<Int> {\n  None\n}\n", "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  make-builtin None argc=0\n\
             \x20  1  return\n"
        );
    }

    #[test]
    fn an_array_literal_collects_its_elements_left_to_right() {
        assert_eq!(
            listing("fn f() -> Array<Int> {\n  [1, 2, 3]\n}\n", "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  const Int(3)\n\
             \x20  3  make-array 3\n\
             \x20  4  return\n"
        );
    }

    #[test]
    fn a_question_mark_opens_what_it_is_given() {
        assert_eq!(
            listing(
                "fn f(v: Option<Int>) -> Option<Int> {\n  Some(v? + 1)\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  try\n\
             \x20  2  value-to-scalar\n\
             \x20  3  scalar-const 1\n\
             \x20  4  int Add\n\
             \x20  5  scalar-to-value Int\n\
             \x20  6  make-builtin Some argc=1\n\
             \x20  7  return\n"
        );
    }

    #[test]
    fn a_return_ends_the_function_where_it_is_written() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  if n < 0 {\n    return 0\n  }\n  return n\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  scalar-const 0\n\
             \x20  2  int Lt\n\
             \x20  3  jump-if-false-scalar 6\n\
             \x20  4  scalar-const 0\n\
             \x20  5  return-scalar\n\
             \x20  6  load-scalar 0\n\
             \x20  7  return-scalar\n"
        );
    }

    // -------------------------------------------------------------- slots

    #[test]
    fn shadowing_declares_a_second_slot() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  let x = 1\n  let x = x + 1\n  x\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/2 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 1\n\
             \x20  4  int Add\n\
             \x20  5  store-scalar 1\n\
             \x20  6  load-scalar 1\n\
             \x20  7  return-scalar\n"
        );
    }

    /// A block's slots are released at its end, so the block after it takes
    /// the same numbers and the frame is as big as the deepest block rather
    /// than as big as the whole body.
    #[test]
    fn sibling_blocks_reuse_the_slots_the_first_released() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  {\n    let a = 1\n    a\n  }\n  {\n    let b = 2\n    b\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/1 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-to-value Int\n\
             \x20  4  pop\n\
             \x20  5  scalar-const 2\n\
             \x20  6  store-scalar 0\n\
             \x20  7  load-scalar 0\n\
             \x20  8  return-scalar\n"
        );
    }

    /// A frame size is the high-water mark: three bindings are live at once
    /// inside the nested block, and one of them is the outer body's.
    #[test]
    fn the_frame_is_as_big_as_the_most_that_was_ever_live() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  let a = 1\n  {\n    let b = 2\n    let c = 3\n    b + c\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/3 -> Int\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 1\n\
             \x20  4  scalar-const 3\n\
             \x20  5  store-scalar 2\n\
             \x20  6  load-scalar 1\n\
             \x20  7  load-scalar 2\n\
             \x20  8  int Add\n\
             \x20  9  return-scalar\n"
        );
    }

    /// A name resolves in declaration order, so the value of a `let` is read
    /// before the name it declares exists.
    #[test]
    fn let_x_equals_x_reads_the_outer_binding() {
        assert_eq!(
            listing(
                "fn f(x: Int) -> Int {\n  {\n    let x = x\n    x\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=0/2 params=[Int] -> Int\n\
             \x20  0  load-scalar 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load-scalar 1\n\
             \x20  3  return-scalar\n"
        );
    }

    // ------------------------------------------------------ enums and match

    const ENUM: &str = "enum E {\n  A\n  B(Int)\n}\n\n";

    /// A case carries the qualified name of the enum it belongs to, and its
    /// payload is pushed before it is built.
    #[test]
    fn an_enum_case_is_built_from_its_payload() {
        assert_eq!(
            listing(&format!("{ENUM}fn f() -> E {{\n  E.B(1)\n}}\n"), "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  const Int(1)\n\
             \x20  1  make-enum m.E.B argc=1\n\
             \x20  2  return\n"
        );
    }

    /// A host's enum is reached through the module that declares it, and it
    /// is not the same instruction: a host's case has a schema rather than a
    /// declaration behind it, and it never carries a payload.
    #[test]
    fn a_case_of_an_enum_a_host_declares_names_the_module_that_declares_it() {
        assert_eq!(
            listing(
                "use http.listen\n\nfn f() -> http.Method {\n  http.Method.Get\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  make-host-enum http.Method.Get\n\
             \x20  1  return\n"
        );
    }

    /// A case that carries nothing is written without a call, and lowers to
    /// the same instruction over no payload.
    #[test]
    fn a_case_that_carries_nothing_is_built_from_nothing() {
        assert_eq!(
            listing(&format!("{ENUM}fn f() -> E {{\n  E.A\n}}\n"), "f"),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  make-enum m.E.A argc=0\n\
             \x20  1  return\n"
        );
    }

    /// Two arms, tried in order over one subject that stays on the stack.
    #[test]
    fn a_match_tries_its_arms_in_order_over_one_subject() {
        assert_eq!(
            listing(
                &format!("{ENUM}fn f(e: E) -> Int {{\n  match e {{\n    E.A => 1\n    E.B(n) => n\n  }}\n}}\n"),
                "f"
            ),
            "fn m.f arity=1 frame=2/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  test-case E.A\n\
             \x20  2  jump-if-false 6\n\
             \x20  3  pop\n\
             \x20  4  scalar-const 1\n\
             \x20  5  jump 17\n\
             \x20  6  test-case E.B\n\
             \x20  7  jump-if-false 16\n\
             \x20  8  get-payload 0\n\
             \x20  9  dup\n\
             \x20 10  store 1\n\
             \x20 11  pop\n\
             \x20 12  pop\n\
             \x20 13  load 1\n\
             \x20 14  value-to-scalar\n\
             \x20 15  jump 17\n\
             \x20 16  no-match\n\
             \x20 17  return-scalar\n"
        );
    }

    /// An arm's binders are released when the arm ends, so a later arm reuses
    /// the slots and the frame is as big as one arm needs rather than as big
    /// as all of them.
    #[test]
    fn sibling_arms_reuse_the_slots_the_first_released() {
        assert_eq!(
            listing(
                "enum Pair {\n  L(Int)\n  R(Int)\n}\n\nfn f(p: Pair) -> Int {\n  match p {\n    Pair.L(x) => x\n    Pair.R(y) => y\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=2/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  test-case Pair.L\n\
             \x20  2  jump-if-false 11\n\
             \x20  3  get-payload 0\n\
             \x20  4  dup\n\
             \x20  5  store 1\n\
             \x20  6  pop\n\
             \x20  7  pop\n\
             \x20  8  load 1\n\
             \x20  9  value-to-scalar\n\
             \x20 10  jump 22\n\
             \x20 11  test-case Pair.R\n\
             \x20 12  jump-if-false 21\n\
             \x20 13  get-payload 0\n\
             \x20 14  dup\n\
             \x20 15  store 1\n\
             \x20 16  pop\n\
             \x20 17  pop\n\
             \x20 18  load 1\n\
             \x20 19  value-to-scalar\n\
             \x20 20  jump 22\n\
             \x20 21  no-match\n\
             \x20 22  return-scalar\n"
        );
    }

    /// A pattern nested two deep tests the payload it is standing on, and
    /// leaves that payload behind when it is done with it.
    #[test]
    fn a_nested_pattern_matches_the_payload_it_stands_on() {
        assert_eq!(
            listing(
                "fn f(r: Result<Option<Int>, Error>) -> Int {\n  match r {\n    Ok(Some(x)) => x\n    _ => 0\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=2/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  test-case Ok\n\
             \x20  2  jump-if-false 17\n\
             \x20  3  get-payload 0\n\
             \x20  4  test-case Some\n\
             \x20  5  jump-if-true 8\n\
             \x20  6  pop\n\
             \x20  7  jump 17\n\
             \x20  8  get-payload 0\n\
             \x20  9  dup\n\
             \x20 10  store 1\n\
             \x20 11  pop\n\
             \x20 12  pop\n\
             \x20 13  pop\n\
             \x20 14  load 1\n\
             \x20 15  value-to-scalar\n\
             \x20 16  jump 20\n\
             \x20 17  pop\n\
             \x20 18  scalar-const 0\n\
             \x20 19  jump 20\n\
             \x20 20  return-scalar\n"
        );
    }

    /// An associated function of a builtin type reads its arguments and
    /// nothing else, because there is no receiver to stand below them.
    #[test]
    fn an_associated_function_reads_its_arguments_alone() {
        assert_eq!(
            listing("fn f() -> Int {\n  Vector.of(1, 2).length()\n}\n", "f"),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  call-assoc Vector.of argc=2\n\
             \x20  3  call-builtin length argc=0\n\
             \x20  4  value-to-scalar\n\
             \x20  5  return-scalar\n"
        );
    }

    /// `MapEntry` is a builtin struct, so its two fields are pushed in
    /// declaration order and built by the builtin that builds one.
    #[test]
    fn a_map_entry_is_built_from_its_two_fields() {
        assert_eq!(
            listing(
                "fn f() -> String {\n  MapEntry(key: \"a\", value: 1).key\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> value\n\
             \x20  0  const Str(\"a\")\n\
             \x20  1  const Int(1)\n\
             \x20  2  make-builtin MapEntry argc=2\n\
             \x20  3  get-field key\n\
             \x20  4  return\n"
        );
    }

    // ---------------------------------------------------------------- dyn

    /// A parameter written `dyn Trait` is converted in the callee's
    /// prologue, which is where `bind_params` converts one, and a call on it
    /// dispatches from the value rather than from the type.
    #[test]
    fn a_dyn_parameter_is_converted_where_it_is_bound_and_dispatches_from_the_value() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 fn f(v: dyn Show) -> String {\n  v.show()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  make-dyn m.Show\n\
             \x20  2  store 0\n\
             \x20  3  load 0\n\
             \x20  4  call-dyn m.Show.show argc=1 [m.A]\n\
             \x20  5  return\n"
        );
    }

    /// A field written `dyn Trait` is converted where the struct is built,
    /// which is where `init_struct` converts one, and a dispatch through the
    /// field reaches every type the package conforms to the trait — here two
    /// of them, which is what makes the choice a run-time one.
    #[test]
    fn a_dyn_field_is_converted_where_the_struct_is_built() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 struct B {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 impl Show for B {\n  fn show(self) -> String {\n    \"b\"\n  }\n}\n\n\
                 struct Box {\n  item: dyn Show\n}\n\n\
                 fn f() -> String {\n  let held = Box(item: A(n: 1))\n  held.item.show()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> value\n\
             \x20  0  const Int(1)\n\
             \x20  1  make-struct m.A fields=n\n\
             \x20  2  make-dyn m.Show\n\
             \x20  3  make-struct m.Box fields=item\n\
             \x20  4  store 0\n\
             \x20  5  load 0\n\
             \x20  6  get-field-at 0\n\
             \x20  7  call-dyn m.Show.show argc=1 [m.A, m.B]\n\
             \x20  8  return\n"
        );
    }

    /// An `Array<dyn Trait>` converts each element, and the depth in the
    /// instruction is the walk `Interpreter::coerce` makes into the array.
    #[test]
    fn an_array_of_dyn_converts_each_element() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 fn f(v: Array<dyn Show>) -> Int {\n  v.length()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
             \x20  0  load 0\n\
             \x20  1  make-dyn m.Show inside 1\n\
             \x20  2  store 0\n\
             \x20  3  load 0\n\
             \x20  4  call-builtin length argc=0\n\
             \x20  5  value-to-scalar\n\
             \x20  6  return-scalar\n"
        );
    }

    /// A declared `dyn Trait` return type converts the answer before it
    /// leaves, which is where `Interpreter::invoke` converts one — so the
    /// conversion belongs to the callee and every `return` of it reaches
    /// one.
    #[test]
    fn a_dyn_return_type_converts_every_return() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 fn f(v: A, c: Bool) -> dyn Show {\n  if c {\n    return v\n  }\n  v\n}\n",
                "f"
            ),
            "fn m.f arity=2 frame=1/1 params=[value, Bool] -> value\n\
             \x20  0  load-scalar 0\n\
             \x20  1  jump-if-false-scalar 5\n\
             \x20  2  load 0\n\
             \x20  3  make-dyn m.Show\n\
             \x20  4  return\n\
             \x20  5  load 0\n\
             \x20  6  make-dyn m.Show\n\
             \x20  7  return\n"
        );
    }

    /// A call on a value whose type is a bounded type parameter is the same
    /// dispatch a trait object gets, and reaches the same candidates: the
    /// checker resolved the *signature* through the bound, and the run
    /// resolves the *implementation* through the value.
    #[test]
    fn a_call_through_a_trait_bound_dispatches_from_the_value() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 struct B {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 impl Show for B {\n  fn show(self) -> String {\n    \"b\"\n  }\n}\n\n\
                 fn f<T: Show>(v: T) -> String {\n  v.show()\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
             \x20  0  load 0\n\
             \x20  1  call-dyn m.Show.show argc=1 [m.A, m.B]\n\
             \x20  2  return\n"
        );
    }

    /// A trait's default body is checked once with `self` typed as the rigid
    /// `Self` bounded by that trait, so a call it makes on `self` is a call
    /// through a bound — and the body is lowered once per conformance that
    /// did not override it, under the name of the type it was materialised
    /// for.
    #[test]
    fn a_trait_default_body_dispatches_on_self() {
        assert_eq!(
            listing(
                "trait Show {\n  fn show(self) -> String\n\n\
                 \x20 fn loud(self) -> String {\n    \"!{self.show()}!\"\n  }\n}\n\n\
                 struct A {\n  n: Int\n}\n\n\
                 impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
                 fn f(v: dyn Show) -> String {\n  v.loud()\n}\n",
                "A.loud"
            ),
            "fn m.A.loud arity=1 frame=1/0 params=[value] receiver -> value\n\
             \x20  0  const Str(\"!\")\n\
             \x20  1  load 0\n\
             \x20  2  call-dyn m.Show.show argc=1 [m.A]\n\
             \x20  3  const Str(\"!\")\n\
             \x20  4  concat 3\n\
             \x20  5  return\n"
        );
    }

    // -------------------------------------------------------- unsupported

    #[test]
    fn every_unsupported_construct_is_named() {
        let cases: Vec<(&str, &str)> = vec![
            (
                // Every lambda but one: the closure a `lock` is given may
                // write its first parameter `var`, because `Inst::Lock` hands
                // it a place rather than a value. Nothing else can, because
                // every argument of an `Inst::CallValue` travels on the value
                // stack. An `async` lambda is no different, and an `async fn`
                // and a call to one both lower.
                "a closure's `var` parameter `n`",
                "fn f() -> Int {\n  let g = fn(var n: Int) {\n    n = 2\n  }\n  1\n}\n",
            ),
            (
                // A scope lowers; what does not is a scope in a function
                // whose answer travels on the scalar stack. A child that
                // settled `Err` returns that failure from the enclosing
                // call, and the oracle does exactly that whatever the
                // declared return type is — so a function that answers an
                // `Int` has no stack for the failure to come back on.
                "a task scope in a function that answers an `Int` or a `Bool`",
                "fn f() -> Int {\n  scope tasks {\n    1\n  }\n}\n",
            ),
            (
                "a `var` variadic parameter",
                "fn g(var x: Int...) -> Int {\n  1\n}\n",
            ),
            (
                // A `dyn` the conversion does not reach. A `Map`'s value
                // type is written inside a head with two arguments, which
                // is where `Interpreter::coerce` stops walking, so nothing
                // converts what stands there and this is refused rather
                // than converted anyway.
                "a `dyn` parameter",
                "trait Show {\n  fn show(self) -> String\n}\n\nstruct A {\n  n: Int\n}\n\nimpl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\nfn f(v: Map<String, dyn Show>) -> Int {\n  v.length()\n}\n",
            ),
            (
                // `Shared(value)` and `lock` both lower; what does not is a
                // `lock` whose closure is a value rather than written at the
                // call. A `var` parameter names the cell's contents, which
                // only `Inst::Lock` can hand over, so the closure has to be
                // the one this instruction is lowering.
                "a `lock` whose closure is not written at the call",
                "fn g(var n: Int) {\n  n = 2\n}\n\nfn f() -> Int {\n  let s = Shared(1)\n  let h = fn(v: Int) {\n    v\n  }\n  s.lock(h)\n}\n",
            ),
            (
                "`snapshot` on a `Vector<B>`, which a conformance answers",
                "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(v: Vector<B>) -> Vector<B> {\n  v.snapshot()\n}\n",
            ),
            (
                "a call to `g`, whose parameter `n` is declared `var` and whose argument is not written `var`",
                "fn g(var n: Int) {\n  n = 1\n}\n\nfn f() -> Int {\n  var x = 1\n  g(x)\n  x\n}\n",
            ),
            (
                "a call to `g`, whose parameter `n` is not declared `var` and whose argument is written `var`",
                "fn g(n: Int) -> Int {\n  n\n}\n\nfn f() -> Int {\n  var x = 1\n  g(var x)\n}\n",
            ),
            (
                "a function declared inside a function body",
                "fn f() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n",
            ),
            (
                "`g` used as a value, whose parameter `n` has a default",
                "fn g(n: Int = 1) -> Int {\n  n\n}\n\nfn f() -> Int {\n  let h = g\n  1\n}\n",
            ),
        ];
        for (what, source) in cases {
            assert_eq!(refused(source), what, "for:\n{source}");
        }
    }

    /// The one refusal in this pass no checked program can reach.
    ///
    /// [`arguments_in_order`] is the invariant the calling convention rests
    /// on: `Body::call_declared` pushes the arguments in the order the
    /// parameters are declared, and that is the order they were *written* in
    /// only because the labels stand in declaration order. `cove-sema`
    /// refuses a call whose labels do not (ADR 0021), so this is reached by
    /// driving the function rather than a program — which is what an
    /// invariant is worth stating for, and is why it stays.
    #[test]
    fn arguments_that_do_not_stand_in_declaration_order_are_refused() {
        let span = Span::new(cove_diag::FileId(0), 0, 0);
        let arg = |label: &str| Arg {
            label: Some(cove_diag::Spanned {
                node: label.to_string(),
                span,
            }),
            is_var: false,
            spread: false,
            value: Expr {
                id: cove_syntax::ast::ExprId(0),
                kind: ExprKind::Int(1),
                span,
            },
            span,
        };
        let written = vec![arg("b"), arg("a")];
        let why = match arguments_in_order(&["a", "b"], Args::new(&written, None), "g", false, span)
        {
            Ok(_) => panic!("labels out of declaration order are refused"),
            Err(why) => why,
        };
        assert_eq!(
            why.what,
            "a call to `g` whose arguments do not stand in declaration order"
        );

        let in_order = vec![arg("a"), arg("b")];
        let assigned =
            match arguments_in_order(&["a", "b"], Args::new(&in_order, None), "g", false, span) {
                Ok(assigned) => assigned,
                Err(why) => panic!("labels in declaration order are accepted, but {why}"),
            };
        assert_eq!(assigned.slots, vec![Some(0), Some(1)]);
    }

    /// The `Display` a diagnostic shows says which backend refused, so a
    /// person reading it knows the program is fine and the VM is not ready.
    #[test]
    fn an_unsupported_construct_reads_as_a_sentence() {
        let why = lower(&checked(
            "fn f() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n",
        ))
        .expect_err("a function declared inside a function body is refused");
        assert_eq!(
            why.to_string(),
            "the VM cannot yet run a function declared inside a function body"
        );
    }

    /// "yet" stands before the construct, so a construct named by a phrase
    /// that ends in a clause still reads as a sentence. It used to be
    /// appended, and several of the names below end in one.
    #[test]
    fn a_construct_named_by_a_clause_still_reads_as_a_sentence() {
        let why = lower(&checked(
            "fn g(n: Int = 1) -> Int {\n  n\n}\n\nfn f() -> Int {\n  let h = g\n  1\n}\n",
        ))
        .expect_err("a declaration with a default used as a value is refused");
        assert_eq!(
            why.to_string(),
            "the VM cannot yet run `g` used as a value, whose parameter `n` has a default"
        );
    }

    // ------------------------------------------------------------ benches

    /// ADR 0012's benchmark package is the target, and six of its eight
    /// entries lower.
    #[test]
    fn six_of_the_eight_bench_entries_lower_and_validate() {
        for name in ["pure", "hostheavy", "arith", "arrayget", "call", "chars"] {
            let program = match lower(&bench(name)) {
                Ok(program) => program,
                Err(why) => panic!("`benches/{name}` lowers, but stopped at {why}"),
            };
            assert!(
                program.function_named(name, "main").is_some(),
                "`benches/{name}` lowers its entry"
            );
            validate(&program)
                .unwrap_or_else(|why| panic!("`benches/{name}` holds the invariants: {why}"));
        }
    }

    /// `benches/arith`'s loop, which is what lowering for effect was measured
    /// on.
    ///
    /// Every statement in it is one of the three that build nothing now: two
    /// compound assignments and an `if` with no `else`. Nineteen instructions
    /// run on an iteration that takes the branch and fifteen on one that does
    /// not, where before it was twenty-five and nineteen — six of them a
    /// `const Unit` and the `pop` that took it away again.
    #[test]
    fn the_arith_bench_loop_builds_no_value_it_does_not_use() {
        let program = lower(&bench("arith")).expect("`benches/arith` lowers");
        validate(&program).expect("it holds the invariants");
        let id = program
            .function_named("arith", "main")
            .expect("its entry is lowered");
        assert_eq!(
            crate::render(&program, id),
            "fn arith.main arity=0 frame=0/2 -> value\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 0\n\
             \x20  3  store-scalar 1\n\
             \x20  4  load-scalar 1\n\
             \x20  5  scalar-const 2000000\n\
             \x20  6  int Lt\n\
             \x20  7  jump-if-false-scalar 23\n\
             \x20  8  load-scalar 1\n\
             \x20  9  scalar-const 7\n\
             \x20 10  int Rem\n\
             \x20 11  scalar-const 0\n\
             \x20 12  int Eq\n\
             \x20 13  jump-if-false-scalar 18\n\
             \x20 14  load-scalar 0\n\
             \x20 15  scalar-const 1\n\
             \x20 16  int Add\n\
             \x20 17  store-scalar 0\n\
             \x20 18  load-scalar 1\n\
             \x20 19  scalar-const 1\n\
             \x20 20  int Add\n\
             \x20 21  store-scalar 1\n\
             \x20 22  jump 4\n\
             \x20 23  load-scalar 0\n\
             \x20 24  scalar-to-value Int\n\
             \x20 25  const Int(285715)\n\
             \x20 26  make-builtin assertEqual argc=2\n\
             \x20 27  try\n\
             \x20 28  pop\n\
             \x20 29  const Unit\n\
             \x20 30  make-builtin Ok argc=1\n\
             \x20 31  return\n"
        );

        // The hot loop, from the test at its top to the jump back, holds no
        // instruction that reads or writes the value stack. The
        // `assertEqual` below it is the boundary, and it is outside the loop.
        let function = program.function(id);
        for inst in &function.code[4..=22] {
            let shape = stack_shape(&program.constants, *inst);
            assert_eq!(
                (shape.values.0, shape.values.1),
                (0, 0),
                "`arith`'s loop runs no general `Value` operation, and {inst:?} is one"
            );
        }
    }

    /// The other two lower through the instruction that writes a field, and
    /// it is one construct they share: `cursor.at += cursor.step`.
    ///
    /// [`crate::Inst::SetField`] is what they reach, so this asserts that
    /// they reach it rather than only that they lowered — a lowering that
    /// arrived at the same answer some other way would be a different
    /// program with the same result.
    #[test]
    fn field_and_method_lower_through_a_written_field() {
        for name in ["field", "method"] {
            let program = lower(&bench(name)).unwrap_or_else(|why| {
                panic!("`benches/{name}` lowers, but: {why}");
            });
            validate(&program).expect("it holds the invariants");
            let id = program
                .function_named(name, "main")
                .expect("its entry is lowered");
            let listing = crate::render(&program, id);
            assert!(
                listing.contains("set-field at"),
                "`benches/{name}` writes a field:\n{listing}"
            );
        }
    }

    /// A compound write reads the field, computes, and writes it back, and
    /// the struct it writes back to is the one it read from.
    #[test]
    fn a_compound_field_write_reads_the_field_it_writes() {
        let program = lower(&checked(
            "struct P {\n  x: Int\n}\n\nexport fn f() -> Int {\n  var p = P(x: 1)\n  p.x += 2\n  p.x\n}\n",
        ))
        .expect("it lowers");
        validate(&program).expect("it holds the invariants");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        assert_eq!(
            crate::render(&program, id),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  make-struct m.P fields=x\n\
             \x20  2  store 0\n\
             \x20  3  load 0\n\
             \x20  4  dup\n\
             \x20  5  get-field-at 0\n\
             \x20  6  value-to-scalar\n\
             \x20  7  scalar-const 2\n\
             \x20  8  int Add\n\
             \x20  9  scalar-to-value Int\n\
             \x20 10  set-field x\n\
             \x20 11  store 0\n\
             \x20 12  load 0\n\
             \x20 13  get-field-at-scalar 0\n\
             \x20 14  return-scalar\n"
        );
    }

    /// `push` needs no place: `Value::Vector` is a handle, so the receiver of
    /// a `var` binding is read like any other value's and handed to
    /// `Inst::CallBuiltin` exactly as a non-mutating method would be.
    #[test]
    fn push_on_a_var_binding_lowers_like_any_other_builtin_method() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var v = Vector.of()\n  v.push(1)\n  v.length()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  call-assoc Vector.of argc=0\n\
             \x20  1  store 0\n\
             \x20  2  load 0\n\
             \x20  3  const Int(1)\n\
             \x20  4  call-builtin push argc=1\n\
             \x20  5  pop\n\
             \x20  6  load 0\n\
             \x20  7  call-builtin length argc=0\n\
             \x20  8  value-to-scalar\n\
             \x20  9  return-scalar\n"
        );
    }

    /// A field path is still a place: `Place::field` in
    /// `crates/cove-runtime/src/interp.rs` steps from a place to a place, and
    /// `Body::is_a_place` mirrors it, so `s.items.push` reaches the same
    /// fall-through `v.push` above does.
    #[test]
    fn push_through_a_var_struct_field_lowers() {
        assert_eq!(
            listing(
                "struct S {\n  items: Vector<Int>\n}\n\nfn f() -> Int {\n  var s = S(items: Vector.of())\n  s.items.push(1)\n  s.items.length()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  call-assoc Vector.of argc=0\n\
             \x20  1  make-struct m.S fields=items\n\
             \x20  2  store 0\n\
             \x20  3  load 0\n\
             \x20  4  get-field-at 0\n\
             \x20  5  const Int(1)\n\
             \x20  6  call-builtin push argc=1\n\
             \x20  7  pop\n\
             \x20  8  load 0\n\
             \x20  9  get-field-at 0\n\
             \x20 10  call-builtin length argc=0\n\
             \x20 11  value-to-scalar\n\
             \x20 12  return-scalar\n"
        );
    }

    // `push` on a read-only place and `push` on a temporary were two tests
    // here. Both are `cove::type::` errors since ADR 0021, so neither
    // program reaches this pass and `cove-sema` is where they are pinned.

    /// `freeze` takes the place and not a read of it, which is the whole
    /// difference between it and `push`: the uniqueness check has to see the
    /// caller's own handle exactly once, and `place-read` would be a second
    /// one.
    #[test]
    fn freeze_takes_the_place_and_not_a_read_of_it() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var v = Vector.of()\n  let frozen = v.freeze()\n  frozen.length()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=2/0 -> Int\n\
             \x20  0  call-assoc Vector.of argc=0\n\
             \x20  1  store 0\n\
             \x20  2  place 0\n\
             \x20  3  freeze\n\
             \x20  4  store 1\n\
             \x20  5  load 1\n\
             \x20  6  call-builtin length argc=0\n\
             \x20  7  value-to-scalar\n\
             \x20  8  return-scalar\n"
        );
    }

    /// A receiver that is not a place at all keeps the ordinary builtin
    /// lowering, exactly as `Interpreter::call_builtin_method` falls through
    /// to `builtins::call_method` for one: the temporary holds the only
    /// handle there is, so the count is right without a place.
    #[test]
    fn freeze_on_a_temporary_reads_it_like_any_other_builtin() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  Vector.of(1).freeze().length()\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0/0 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  call-assoc Vector.of argc=1\n\
             \x20  2  call-builtin freeze argc=0\n\
             \x20  3  call-builtin length argc=0\n\
             \x20  4  value-to-scalar\n\
             \x20  5  return-scalar\n"
        );
    }

    // ------------------------------------------------------------- places

    /// `two(var x, var x)` is what a place has to be an alias for: both
    /// arguments are the same `place 0`, so the callee's two parameters name
    /// one slot of the caller's frame and the second write sees the first.
    #[test]
    fn two_var_arguments_naming_one_binding_push_the_same_place_twice() {
        assert_eq!(
            listing(
                "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nfn f() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  const Int(0)\n\
             \x20  1  store 0\n\
             \x20  2  place 0\n\
             \x20  3  place 0\n\
             \x20  4  call m.two argc=0/0/2\n\
             \x20  5  pop\n\
             \x20  6  load 0\n\
             \x20  7  value-to-scalar\n\
             \x20  8  return-scalar\n"
        );
        assert_eq!(
            listing(
                "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nfn f() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n",
                "two"
            ),
            "fn m.two arity=2 frame=0/0/2 params=[place, place] -> value\n\
             \x20  0  load-place 0\n\
             \x20  1  load-place 0\n\
             \x20  2  place-read\n\
             \x20  3  value-to-scalar\n\
             \x20  4  scalar-const 1\n\
             \x20  5  int Add\n\
             \x20  6  scalar-to-value Int\n\
             \x20  7  place-write\n\
             \x20  8  load-place 1\n\
             \x20  9  load-place 1\n\
             \x20 10  place-read\n\
             \x20 11  value-to-scalar\n\
             \x20 12  scalar-const 10\n\
             \x20 13  int Add\n\
             \x20 14  scalar-to-value Int\n\
             \x20 15  place-write\n\
             \x20 16  const Unit\n\
             \x20 17  return\n"
        );
    }

    /// A `var self` receiver is a place argument, and the body writes
    /// through it: `self.hits += 1` is the place built twice, read through
    /// once, and written through once.
    #[test]
    fn a_var_self_receiver_is_a_place_argument() {
        let source = "struct Counter {\n  hits: Int\n}\n\nimpl Counter {\n  fn hit(var self) {\n    self.hits += 1\n  }\n}\n\nfn f() -> Int {\n  var c = Counter(hits: 0)\n  c.hit()\n  c.hits\n}\n";
        assert_eq!(
            listing(source, "Counter.hit"),
            "fn m.Counter.hit arity=1 frame=0/0/1 params=[place] receiver -> value\n\
             \x20  0  load-place 0\n\
             \x20  1  place-field 0\n\
             \x20  2  load-place 0\n\
             \x20  3  place-field 0\n\
             \x20  4  place-read\n\
             \x20  5  value-to-scalar\n\
             \x20  6  scalar-const 1\n\
             \x20  7  int Add\n\
             \x20  8  scalar-to-value Int\n\
             \x20  9  place-write\n\
             \x20 10  const Unit\n\
             \x20 11  return\n"
        );
        assert_eq!(
            listing(source, "f"),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  const Int(0)\n\
             \x20  1  make-struct m.Counter fields=hits\n\
             \x20  2  store 0\n\
             \x20  3  place 0\n\
             \x20  4  call m.Counter.hit argc=0/0/1\n\
             \x20  5  pop\n\
             \x20  6  load 0\n\
             \x20  7  get-field-at-scalar 0\n\
             \x20  8  return-scalar\n"
        );
    }

    /// A `var` parameter passed on as a `var` argument is one `load-place`
    /// and nothing else, which is what makes the callee's callee alias the
    /// original binding rather than this frame's slot.
    #[test]
    fn a_forwarded_var_argument_is_the_place_the_parameter_holds() {
        assert_eq!(
            listing(
                "fn bump(var n: Int) {\n  n += 1\n}\n\nfn forward(var n: Int) {\n  bump(var n)\n}\n",
                "forward"
            ),
            "fn m.forward arity=1 frame=0/0/1 params=[place] -> value\n\
             \x20  0  load-place 0\n\
             \x20  1  call m.bump argc=0/0/1\n\
             \x20  2  return\n"
        );
    }

    /// A path deeper than one field is written through a place even where
    /// the root is an ordinary local: the whole-value update
    /// `Body::assign_field` performs replaces a local's struct, and it has
    /// nowhere to put the intermediate one back.
    #[test]
    fn a_field_two_deep_is_written_through_a_place() {
        assert_eq!(
            listing(
                "struct Inner {\n  n: Int\n}\n\nstruct Outer {\n  inner: Inner\n}\n\nfn f() -> Int {\n  var o = Outer(inner: Inner(n: 1))\n  o.inner.n = 2\n  o.inner.n\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/0 -> Int\n\
             \x20  0  const Int(1)\n\
             \x20  1  make-struct m.Inner fields=n\n\
             \x20  2  make-struct m.Outer fields=inner\n\
             \x20  3  store 0\n\
             \x20  4  place 0\n\
             \x20  5  place-field 0\n\
             \x20  6  place-field 0\n\
             \x20  7  const Int(2)\n\
             \x20  8  place-write\n\
             \x20  9  load 0\n\
             \x20 10  get-field-at 0\n\
             \x20 11  get-field-at-scalar 0\n\
             \x20 12  return-scalar\n"
        );
    }

    /// `startup` is not one of the eight, and it lowers too: it is the
    /// smallest function the package has, and it is what a frame of nothing
    /// looks like.
    #[test]
    fn the_smallest_entry_is_a_unit_and_a_return() {
        let program = lower(&bench("startup")).expect("`benches/startup` lowers");
        validate(&program).expect("it holds the invariants");
        let id = program
            .function_named("startup", "main")
            .expect("its entry is lowered");
        assert_eq!(
            crate::render(&program, id),
            "fn startup.main arity=0 frame=0/0 -> value\n\
             \x20  0  const Unit\n\
             \x20  1  return\n"
        );
    }

    // ---------------------------------------------------- from an entry

    /// The names of the functions a lowered program holds, in the order it
    /// numbered them.
    fn lowered_names(program: &Program) -> Vec<String> {
        program
            .functions
            .iter()
            .map(|function| format!("{}.{}", function.module, function.name))
            .collect()
    }

    /// Issue #115 itself: `hello` is three lines and `callbacks/` holds an
    /// `async` closure, and the two share a package and nothing else.
    #[test]
    fn an_entry_lowers_only_what_it_reaches_of_the_examples() {
        let checked = examples();
        // The whole package lowers now. It did not when this test was
        // written — it stopped at an `async` closure, and the point of the
        // test was that `hello` lowered anyway — so what is left is the half
        // that was always the point: an entry is what it reaches, whatever
        // else the package holds.
        lower(&checked).expect("`examples/` lowers whole");

        let lowered = lower_entry(&checked, "hello", "main").expect("`hello.main` lowers");
        validate(&lowered.program).expect("it holds the VM's invariants");
        assert_eq!(
            lowered_names(&lowered.program),
            ["hello.main", "hello.greeting"]
        );
        assert_eq!(lowered.entry, FunctionId(0));
        assert!(
            lowered
                .program
                .function_named("callbacks", "main")
                .is_none(),
            "nothing `hello` cannot reach comes with it"
        );
    }

    /// A program holds what its entry reaches and nothing beside it, so the
    /// count is the measurement and the absence is the point.
    #[test]
    fn a_lowered_entry_holds_only_what_it_reaches() {
        let source = "fn used() -> Int {\n  1\n}\n\nfn between() -> Int {\n  used()\n}\n\n\
                      fn unreached() -> Int {\n  used()\n}\n\nfn main() -> Int {\n  between()\n}\n";
        let checked = checked(source);

        let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
        validate(&lowered.program).expect("it holds the VM's invariants");
        // Numbered on discovery: the entry, then what its body called, then
        // what that body called.
        assert_eq!(
            lowered_names(&lowered.program),
            ["m.main", "m.between", "m.used"]
        );
        assert!(lowered.program.function_named("m", "unreached").is_none());

        // The same package, lowered whole, holds the one the entry cannot
        // reach as well.
        let whole = lower(&checked).expect("the package lowers");
        assert_eq!(
            lowered_names(&whole),
            ["m.between", "m.main", "m.unreached", "m.used"]
        );
    }

    /// A declaration is numbered once, so a call back to something already
    /// numbered adds nothing to walk and the worklist empties.
    #[test]
    fn recursion_and_mutual_recursion_terminate() {
        let checked = checked(
            "fn down(n: Int) -> Int {\n  if n == 0 {\n    0\n  } else {\n    up(n - 1)\n  }\n}\n\n\
             fn up(n: Int) -> Int {\n  down(n - 1)\n}\n\n\
             fn main() -> Int {\n  main2(3)\n}\n\n\
             fn main2(n: Int) -> Int {\n  down(n) + main2(0)\n}\n",
        );
        let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
        validate(&lowered.program).expect("it holds the VM's invariants");
        assert_eq!(
            lowered_names(&lowered.program),
            ["m.main", "m.main2", "m.down", "m.up"]
        );
    }

    /// A method is reached through a call like anything else, so a method
    /// only an unreached function calls is not part of the program.
    #[test]
    fn a_method_is_lowered_where_the_entry_reaches_it() {
        let checked = checked(
            "struct P {\n  x: Int\n}\n\n\
             impl P {\n  fn reached(self) -> Int {\n    self.x\n  }\n\n  \
             fn unreached(self) -> Int {\n    self.x + 1\n  }\n}\n\n\
             fn aside() -> Int {\n  P(x: 2).unreached()\n}\n\n\
             fn main() -> Int {\n  P(x: 1).reached()\n}\n",
        );
        let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
        validate(&lowered.program).expect("it holds the VM's invariants");
        assert_eq!(lowered_names(&lowered.program), ["m.main", "m.P.reached"]);
        assert!(lowered.program.function_named("m", "P.unreached").is_none());
        assert!(lowered.program.function_named("m", "aside").is_none());
    }

    /// Narrowing what is lowered narrows nothing about what is refused: a
    /// construct the entry reaches is reported in the words it always was.
    #[test]
    fn an_unsupported_construct_on_the_path_is_still_refused() {
        let source = "fn helper() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n\n\
                      fn main() -> Int {\n  helper()\n}\n";
        let checked = checked(source);
        let whole = lower(&checked).expect_err("the package does not lower");
        let entry = lower_entry(&checked, "m", "main").expect_err("nor does the entry");
        assert_eq!(entry.what, "a function declared inside a function body");
        assert_eq!(entry.what, whole.what);
        assert_eq!(entry.span, whole.span);
    }

    /// A `[run.<name>]` table is a file a person edits, so a name it gets
    /// wrong is reported rather than crashed on.
    #[test]
    fn a_missing_entry_is_reported() {
        let checked = checked("fn main() -> Int {\n  1\n}\n");
        let missing = lower_entry(&checked, "m", "notMain").expect_err("there is no `m.notMain`");
        assert_eq!(
            missing.what,
            "`m.notMain`, which this package does not declare"
        );
        let missing = lower_entry(&checked, "elsewhere", "main").expect_err("there is no module");
        assert_eq!(
            missing.what,
            "`elsewhere.main`, which this package does not declare"
        );
    }

    // ----------------------------------------------------------- validate

    // ------------------------------------------------------------- blocks

    /// A listing of the blocks, read the way `render` reads instructions: the
    /// head, how far it reaches, and the instruction it begins at.
    fn blocks(program: &Program, function: &str) -> String {
        let id = program
            .function_named("m", function)
            .expect("the function is lowered");
        let function = program.function(id);
        let mut out = String::new();
        for (pc, count) in function.block_fuel.iter().enumerate() {
            if *count != 0 {
                out.push_str(&format!(
                    "{pc}+{count} {}\n",
                    crate::render(program, id)
                        .lines()
                        .nth(pc + 1)
                        .and_then(|line| line.trim().split_once("  "))
                        .expect("the listing has a line per instruction")
                        .1
                ));
            }
        }
        out
    }

    /// Every head reaches the jump that ends its straight line, and the run-up
    /// to a loop reaches past the head the back edge lands on — which is what
    /// makes the extents overlap and what makes falling into a head already
    /// paid for.
    #[test]
    fn a_head_reaches_the_jump_that_ends_its_line() {
        let program = lower(&checked(
            "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
        ))
        .expect("it lowers");
        assert_eq!(
            blocks(&program, "f"),
            "0+6 scalar-const 0\n\
             2+4 load-scalar 0\n\
             6+5 load-scalar 0\n\
             11+2 load-scalar 0\n"
        );
    }

    /// The case a partition would lose: an `if` with no `else` falls into the
    /// join its own jump also targets, and nothing about that fall announces
    /// itself. The head above the join has to reach past it, or the
    /// instructions after the join run for free.
    #[test]
    fn a_head_reaches_past_a_join_it_falls_into() {
        let program = lower(&checked(
            "fn f(b: Bool) -> Int {\n  var i = 0\n  if b {\n    i = 1\n  }\n  i\n}\n",
        ))
        .expect("it lowers");
        let function = program.function(program.function_named("m", "f").expect("`f` is lowered"));
        let join = match function.code.iter().find_map(|inst| match inst {
            Inst::JumpIfFalse(to) | Inst::JumpIfFalseScalar(to) => Some(*to as usize),
            _ => None,
        }) {
            Some(join) => join,
            None => panic!("an `if` lowers to a conditional jump"),
        };
        assert_ne!(function.block_fuel[join], 0, "the join is a head");
        let above = (0..join)
            .rev()
            .find(|pc| function.block_fuel[*pc] != 0)
            .expect("some head stands above the join");
        assert!(
            above + function.block_fuel[above] as usize > join,
            "the head at {above} reaches {} and the join is at {join}",
            above + function.block_fuel[above] as usize
        );
    }

    /// A call ends a block, because the callee runs before the caller's next
    /// instruction does and the caller's fuel has not been charged yet.
    #[test]
    fn a_call_ends_the_block_it_stands_in() {
        let program = lower(&checked(
            "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1) + g(2)\n}\n",
        ))
        .expect("it lowers");
        assert_eq!(
            blocks(&program, "f"),
            "0+2 scalar-const 1\n\
             2+2 scalar-const 2\n\
             4+2 int Add\n"
        );
    }

    /// Whichever head control last arrived at, its extent reaches every
    /// instruction between that head and the next one it can leave from. That
    /// is the property the VM's instruction count rests on: an instruction
    /// outside every extent above it would run without being charged.
    #[test]
    fn every_instruction_is_inside_the_extent_of_the_head_above_it() {
        let program = lower(&checked(
            "fn g(a: Int) -> Int {\n  a\n}\n\n\
             fn f(b: Bool) -> Int {\n  \
               var total = 0\n  \
               for x in [1, 2, 3] {\n    \
                 if b && x > 1 {\n      total = total + g(x)\n    } else {\n      total = total - 1\n    }\n  \
               }\n  \
               total\n\
             }\n",
        ))
        .expect("it lowers");
        validate(&program).expect("it holds the invariants");
        for function in &program.functions {
            let mut reaches = 0usize;
            for pc in 0..function.code.len() {
                if function.block_fuel[pc] != 0 {
                    reaches = reaches.max(pc + function.block_fuel[pc] as usize);
                }
                assert!(
                    reaches > pc,
                    "{}: {pc} is inside no head's extent",
                    function.name
                );
            }
        }
    }

    /// A head the code does not name is a head the VM never arrives at, so
    /// the instructions its extent covers would be charged twice — once by
    /// it, and once by whichever head control really came from.
    #[test]
    fn validate_refuses_a_block_head_the_code_does_not_name() {
        let mut program = lower(&checked(
            "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
        ))
        .expect("it lowers");
        program.functions[0].block_fuel[3] = 3;
        assert_eq!(
            validate(&program).expect_err("a head nothing reaches is refused"),
            "m.f: 3: begins no block, and the table begins one of 3 there"
        );
    }

    /// And a head the code does name, missing from the table, is an arrival
    /// that charges nothing at all.
    #[test]
    fn validate_refuses_a_block_head_the_table_is_missing() {
        let mut program = lower(&checked(
            "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
        ))
        .expect("it lowers");
        program.functions[0].block_fuel[2] = 0;
        assert_eq!(
            validate(&program).expect_err("a head the table is missing is refused"),
            "m.f: 2: begins a block of 4, and the table begins none there"
        );
    }

    /// An extent that stops short of the instruction control leaves from is
    /// refused whatever it stops on, because the rest of that straight line
    /// would run uncharged.
    #[test]
    fn validate_refuses_a_block_that_ends_where_control_does_not() {
        let mut program = lower(&checked(
            "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
        ))
        .expect("it lowers");
        program.functions[0].block_fuel[2] = 3;
        assert_eq!(
            validate(&program).expect_err("a block that stops short is refused"),
            "m.f: 2: begins a block of 3, which ends where control does not"
        );
    }

    #[test]
    fn validate_refuses_a_jump_past_the_end() {
        let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
        let span = program.functions[0].span;
        program.functions[0].code.insert(0, Inst::Jump(99));
        program.functions[0].spans.insert(0, span);
        assert_eq!(
            validate(&program).expect_err("a jump past the end is refused"),
            "m.f: 0: jumps to 99, past the 3 instructions"
        );
    }

    #[test]
    fn validate_refuses_a_slot_outside_the_frame() {
        let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
        program.functions[0].code[0] = Inst::LoadLocal(4);
        assert_eq!(
            validate(&program).expect_err("a slot outside the frame is refused"),
            "m.f: 0: reaches slot 4 of a frame of 0"
        );
    }

    /// A slot number is bounded by its own stack's frame size, not by
    /// whatever the other stack's frame happens to be.
    ///
    /// The first program has no value locals at all — `value_frame_size` is
    /// 0 — so addressing value slot 0 is refused exactly as it would be in a
    /// frame with a hundred scalar slots and no value ones. The second is the
    /// mirror image: no scalar locals, so scalar slot 0 is refused however
    /// large the value frame is. Two independent numberings mean the mistake
    /// once caught by comparing a slot's declared kind is caught the same way
    /// an out-of-range slot always was.
    #[test]
    fn validate_refuses_a_slot_past_its_own_stacks_frame() {
        let source = "fn f() -> Int {\n  let a = 1\n  a\n}\n";
        let mut program = lower(&checked(source)).expect("it lowers");
        program.functions[0].code[1] = Inst::StoreLocal(0);
        assert_eq!(
            validate(&program).expect_err("a value slot past an empty value frame is refused"),
            "m.f: 1: reaches slot 0 of a frame of 0"
        );

        let mut program =
            lower(&checked("fn f(s: String) -> String {\n  s\n}\n")).expect("it lowers");
        program.functions[0].code[0] = Inst::LoadScalar(0);
        assert_eq!(
            validate(&program).expect_err("a scalar slot past an empty scalar frame is refused"),
            "m.f: 0: reaches slot 0 of a frame of 0"
        );
    }

    /// Two sibling blocks share a slot number even when they disagree about
    /// where it lives, because the value stack and the scalar stack are
    /// numbered separately now.
    ///
    /// The first block's `Int` takes scalar slot 0. The second block's
    /// `String` is free to take value slot 0 as well — a value number and a
    /// scalar number are not the same number space, so there is nothing to
    /// skip past — and the third block's `Int` reuses scalar slot 0 again.
    /// The frame is as big as either stack's deepest block, not as big as
    /// their sum.
    #[test]
    fn sibling_blocks_share_a_slot_number_regardless_of_kind() {
        assert_eq!(
            listing(
                "fn f() -> Unit {\n  {\n    let a = 1\n  }\n  {\n    let b = \"two\"\n  }\n  {\n    let c = 3\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1/1 -> value\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Str(\"two\")\n\
             \x20  3  store 0\n\
             \x20  4  scalar-const 3\n\
             \x20  5  store-scalar 0\n\
             \x20  6  const Unit\n\
             \x20  7  return\n"
        );
    }

    #[test]
    fn validate_refuses_a_join_reached_at_two_depths() {
        let mut program = lower(&checked(
            "fn f(b: Bool) -> Int {\n  if b {\n    1\n  } else {\n    2\n  }\n}\n",
        ))
        .expect("it lowers");
        // One more value on the branch that jumps to the join than on the
        // branch that falls into it.
        let unit = program.constants.len() as u32;
        program.constants.push(Const::Unit);
        let function = &mut program.functions[0];
        let span = function.span;
        function.code.insert(2, Inst::Const(ConstId(unit)));
        function.spans.insert(2, span);
        for inst in &mut function.code {
            match inst {
                Inst::Jump(to) | Inst::JumpIfFalse(to) | Inst::JumpIfTrue(to) => *to += 1,
                _ => {}
            }
        }
        assert!(
            validate(&program)
                .expect_err("a join at two depths is refused")
                .contains("on the stack"),
            "{:?}",
            validate(&program)
        );
    }

    #[test]
    fn validate_refuses_a_function_that_does_not_end_in_a_return() {
        let mut program = lower(&checked("fn f() -> String {\n  \"hi\"\n}\n")).expect("it lowers");
        program.functions[0].code.pop();
        program.functions[0].spans.pop();
        assert_eq!(
            validate(&program).expect_err("a missing return is refused"),
            "m.f: does not end in a `return`"
        );
    }

    /// The instruction a function must end in is the one its convention
    /// names, so a scalar-answering function is missing a different one.
    #[test]
    fn validate_refuses_a_scalar_function_that_does_not_end_in_a_return_scalar() {
        let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
        program.functions[0].code.pop();
        program.functions[0].spans.pop();
        assert_eq!(
            validate(&program).expect_err("a missing return is refused"),
            "m.f: does not end in a `return-scalar`"
        );
    }

    #[test]
    fn validate_refuses_argument_spans_for_an_instruction_that_does_not_exist() {
        let mut program = lower(&checked(
            "fn f() -> Result<Unit, Error> {\n  assert(1 > 2)?\n  Ok(())\n}\n",
        ))
        .expect("it lowers");
        let function = &mut program.functions[0];
        let span = function.span;
        function.arg_spans.insert(99, vec![span]);
        assert_eq!(
            validate(&program).expect_err("spans for no instruction are refused"),
            "m.f: carries argument spans for instruction 99 of 10"
        );
    }

    #[test]
    fn validate_refuses_a_call_with_the_wrong_number_of_arguments() {
        let mut program = lower(&checked(
            "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
        ))
        .expect("it lowers");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        let function = &mut program.functions[id.0 as usize];
        for inst in &mut function.code {
            if let Inst::Call { scalar_argc, .. } = inst {
                *scalar_argc = 2;
            }
        }
        assert!(validate(&program)
            .expect_err("a mismatched call is refused")
            .contains("with 2 arguments, which takes 1"),);
    }

    /// A function that holds captures is entered through the closure that
    /// holds them, so a direct `Call` to one would open a frame with the
    /// capture slots left holding `Unit`.
    #[test]
    fn validate_refuses_a_direct_call_to_a_function_that_holds_captures() {
        let mut program = lower(&checked(
            "fn f(a: Int) -> fn() -> Int {\n  fn() {\n    a\n  }\n}\n",
        ))
        .expect("it lowers");
        let closure = program
            .function_named("m", "<closure 0>")
            .expect("the lambda is lowered");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        program.functions[id.0 as usize].code[0] = Inst::Call {
            function: closure,
            value_argc: 0,
            scalar_argc: 0,
            place_argc: 0,
            returns_scalar: false,
        };
        assert!(validate(&program)
            .expect_err("a direct call to a closure body is refused")
            .contains("directly, which is entered through the closure"));
    }

    /// A closure is called under one convention, and the last point at which
    /// the target is known is where the closure is made — so that is where
    /// the convention is checked.
    #[test]
    fn validate_refuses_a_closure_made_of_a_function_that_answers_a_scalar() {
        let mut program = lower(&checked(
            "fn g() -> Int {\n  1\n}\n\nfn f() -> fn() -> Int {\n  fn() {\n    g()\n  }\n}\n",
        ))
        .expect("it lowers");
        let scalar = program.function_named("m", "g").expect("`g` is lowered");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        program.functions[id.0 as usize].code[0] = Inst::MakeClosure {
            function: scalar,
            captures: 0,
        };
        assert!(validate(&program)
            .expect_err("a closure over a scalar answer is refused")
            .contains("which answers on the scalar stack"));
    }

    /// The counts are per stack and not only in total, because a call that
    /// supplied the right number of arguments on the wrong stacks would read
    /// words nobody wrote.
    #[test]
    fn validate_refuses_a_call_that_puts_its_arguments_on_the_wrong_stack() {
        let mut program = lower(&checked(
            "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
        ))
        .expect("it lowers");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        let function = &mut program.functions[id.0 as usize];
        for inst in &mut function.code {
            if let Inst::Call {
                value_argc,
                scalar_argc,
                ..
            } = inst
            {
                *value_argc = 1;
                *scalar_argc = 0;
            }
        }
        assert!(validate(&program)
            .expect_err("a call on the wrong stacks is refused")
            .contains("with 1 value, 0 scalar and 0 place arguments, which takes 0, 1 and 0"),);
    }

    /// And the answer's stack likewise: a caller that read the wrong one
    /// would read whatever the callee happened to leave behind.
    #[test]
    fn validate_refuses_a_call_that_expects_its_answer_on_the_wrong_stack() {
        let mut program = lower(&checked(
            "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
        ))
        .expect("it lowers");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        let function = &mut program.functions[id.0 as usize];
        for inst in &mut function.code {
            if let Inst::Call { returns_scalar, .. } = inst {
                *returns_scalar = false;
            }
        }
        assert!(validate(&program)
            .expect_err("a call reading the wrong stack is refused")
            .contains("for an answer on the value stack, which answers on the scalar"),);
    }

    /// A function returns on one stack, so a body holding both instructions
    /// would leave its caller reading whichever one happened to run.
    #[test]
    fn validate_refuses_a_function_that_mixes_the_two_returns() {
        let mut program = lower(&checked("fn f(a: Int) -> Int {\n  a\n}\n")).expect("it lowers");
        let id = program.function_named("m", "f").expect("`f` is lowered");
        let function = &mut program.functions[id.0 as usize];
        function.code.insert(0, Inst::Return);
        function.spans.insert(0, function.span);
        assert!(validate(&program)
            .expect_err("a mixed return is refused")
            .contains("answers on the scalar stack and holds a `return`"),);
    }
}
