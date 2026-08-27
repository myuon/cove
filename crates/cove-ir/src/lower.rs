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
//! Two things are deliberately *not* scalar. A parameter's slot is a value
//! slot, because an argument arrives on the caller's value stack and becomes
//! the callee's slot without moving, and this does not change the calling
//! convention. A struct's field is not a slot at all.
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
//!   block reuses the same numbers and `frame_size` is a high-water mark
//!   rather than a count of declarations.
//! - **A `for` binding lives in the scope its body sees**, and the iterable
//!   is evaluated in the enclosing one.
//! - **Evaluation is left to right everywhere**: arguments, operands, array
//!   elements, and struct fields.
//! - **A struct's fields are pushed in declaration order.** `assign_labels`
//!   in the interpreter refuses a label written out of declaration order, so
//!   a call it accepts already stands in that order; `arguments_in_order`
//!   below is that rule, and a call it cannot put in order is reported rather
//!   than rearranged.
//! - **A `match` arm is a scope, and the first that matches is the only one
//!   that runs.** `match_pattern` tests and binds as it walks, and the arm
//!   that does not match releases what it bound — so an arm's slots behave
//!   the way a block's do, and a subject no arm covers stops the run.
//!
//! # What is not lowered
//!
//! Closures and trailing closures, `scope`/`spawn`/`await`, `var`
//! parameters, traits and `dyn`, `Shared`, `snapshot`, a range used as a
//! value, assignment to a field of anything but a local, and any call whose
//! target cannot be named at lowering time. Each is reported in the words a
//! Cove programmer writes it in.
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
use std::rc::Rc;

use cove_diag::{FileId, Span};
use cove_schema::builtins;
use cove_schema::hosts;
use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;
use cove_sema::MethodTarget;
use cove_syntax::ast::{
    Arg, BinaryOp as SourceBinary, Block, EnumDecl, Expr, ExprId, ExprKind, FnDecl, ItemKind,
    MatchArm, Pattern, PatternKind, Stmt, StmtKind, StrPart, StructDecl, Type, TypeKind,
    UnaryOp as SourceUnary,
};

use crate::{
    BinaryOp, Const, ConstId, Function, FunctionId, Inst, IntOp, Program, Scalar, SlotKind,
    UnaryOp, Unsupported,
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
    let entry = lowering.number(key);
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
        lowering.number(Key(index));
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

/// The whole-program state one lowering carries: what the package declares,
/// which of it has been reached, and the constants the reached share.
struct Lowering<'a> {
    checked: &'a Checked,
    /// Every function the package declares, in the checker's order, whether
    /// or not this lowering will emit any of them.
    catalog: Vec<Declared<'a>>,
    /// The id each catalog entry was given, once something reached it.
    numbered: Vec<Option<FunctionId>>,
    /// The declaration each id names, in the order the ids were handed out.
    ///
    /// This is the worklist as much as the table: a declaration is appended
    /// when it is first reached, and the lowering walks the vector from the
    /// front until it stops growing.
    reached: Vec<Key>,
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
    constants: Vec<Const>,
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
            numbered: Vec::new(),
            reached: Vec::new(),
            functions: BTreeMap::new(),
            methods: BTreeMap::new(),
            by_name: BTreeMap::new(),
            visible: visibility(checked),
            constants: Vec::new(),
        };
        for (module, resolved) in &checked.modules {
            for (name, entry) in &resolved.functions {
                let key = lowering.catalogue(Declared {
                    module,
                    name: name.clone(),
                    type_name: None,
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
        self.numbered.push(None);
        Key(self.catalog.len() - 1)
    }

    /// The id `key` has, handing one out and queuing the declaration when
    /// this is the first thing to reach it.
    ///
    /// Numbering once is what ends the walk: a function that calls itself,
    /// and a cycle of functions that call each other, are each already
    /// numbered by the time the call that closes the loop is emitted.
    fn number(&mut self, key: Key) -> FunctionId {
        if let Some(id) = self.numbered[key.0] {
            return id;
        }
        let id = FunctionId(self.reached.len() as u32);
        self.numbered[key.0] = Some(id);
        self.reached.push(key);
        id
    }

    /// What `key` names.
    fn declaration(&self, key: Key) -> &Declared<'a> {
        &self.catalog[key.0]
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

    /// Lowers one function into its instructions.
    fn function(&mut self, id: FunctionId) -> Result<Function, Unsupported> {
        let declared = self.declaration(self.reached[id.0 as usize]);
        let module = declared.module;
        let name: Rc<str> = declared.name.as_str().into();
        let decl = declared.decl;

        if decl.is_async {
            return Err(Unsupported::new("an `async fn`", decl.span));
        }
        if let Some(ty) = &decl.return_type {
            reject_dyn(ty, "a `dyn` return type")?;
        }

        let mut body = Body::new(self, module);
        if let Some(receiver) = decl.receiver {
            if receiver.is_var {
                return Err(Unsupported::new("a `var self` receiver", receiver.span));
            }
            // A receiver is read-only in the body: a `var self` receiver is
            // refused above, so nothing writes through this one.
            body.declare(Some("self"), false, SlotKind::Value);
        }
        for param in &decl.params {
            reject_parameter(param.is_var, param.variadic, param.ty.as_ref(), param.span)?;
            // An ordinary parameter receives a shallow copy and is a
            // read-only place inside the body, exactly as the interpreter
            // declares one; a `var` parameter is refused above.
            body.declare(Some(param.name.node.as_str()), false, SlotKind::Value);
        }

        body.block(&decl.body)?;
        body.emit_final_return(decl.body.span);
        let finished = body.finish();

        Ok(Function {
            module: module.into(),
            name,
            frame_size: finished.frame_size,
            slots: finished.slots,
            arity: decl.params.len() as u32 + u32::from(decl.receiver.is_some()),
            has_receiver: decl.receiver.is_some(),
            captures: Vec::new(),
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
/// `writable` is `is_var` carried through the lowering: a `var` binds a
/// mutable place and everything else — a `let`, a parameter, a receiver, a
/// `for` binding — binds a read-only one, which is the interpreter's
/// `Place::binding` read at lowering time rather than at run time.
struct Binding<'a> {
    name: Option<&'a str>,
    slot: u32,
    writable: bool,
    /// Which stack the slot lives in, decided when it was declared and never
    /// revisited: a binding's type does not change, so neither does where it
    /// is kept.
    kind: SlotKind,
}

/// A jump target, resolved once the instruction it points at exists.
struct Label {
    at: Option<u32>,
    /// The operand-stack depths control arrives here with, taken from the
    /// first reachable jump that names it.
    depth: Option<Depth>,
}

/// How much stands on each of the two operand stacks.
///
/// Two numbers rather than one because there are two stacks. Every join point
/// has to be arrived at with the same amount on both, and `validate` simulates
/// both, so tracking one and guessing the other would be tracking neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Depth {
    values: u32,
    scalars: u32,
}

impl Depth {
    /// Both stacks empty, which is where a body and a loop's operands start.
    const EMPTY: Depth = Depth {
        values: 0,
        scalars: 0,
    };

    /// The depths after one instruction of this shape has run.
    fn after(self, shape: Shape) -> Depth {
        Depth {
            values: self.values.saturating_sub(shape.values.0) + shape.values.1,
            scalars: self.scalars.saturating_sub(shape.scalars.0) + shape.scalars.1,
        }
    }
}

/// The loop a `break` or a `continue` leaves.
struct LoopFrame {
    break_to: usize,
    continue_to: usize,
    /// The operand-stack depths the loop runs at, which is what a `break`
    /// written inside a half-evaluated expression has to get back down to —
    /// on both stacks, because a half-evaluated `a + b` can have left
    /// something on either.
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    /// Something reads what this leaves.
    Value,
    /// Nothing does.
    Effect,
}

/// What lowering one body produced, on its way into a [`Function`].
struct Finished {
    code: Vec<Inst>,
    spans: Vec<Span>,
    arg_spans: BTreeMap<u32, Vec<Span>>,
    frame_size: u32,
    /// Where each slot lives, one entry per slot, which is the frame
    /// metadata the VM addresses a slot through and a root set is read from.
    slots: Vec<SlotKind>,
}

/// Everything one function's instructions are built from.
struct Body<'a, 'l> {
    outer: &'l mut Lowering<'a>,
    module: &'a str,
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
    frame_size: u32,
    /// Where each slot number lives, by slot number.
    ///
    /// It only grows: a slot released at the end of a block keeps its kind,
    /// because a later sibling block that reuses the number reuses the
    /// storage, and one number naming two storages is exactly the mistake
    /// `Body::declare` skips past.
    kinds: Vec<SlotKind>,
    /// The next slot number to hand out, restored when a scope ends.
    ///
    /// This was `live.len()` while every slot was alike. It is its own number
    /// now because `declare` may skip a released slot whose kind does not
    /// match.
    next: u32,
    labels: Vec<Label>,
    patches: Vec<(usize, usize)>,
    loops: Vec<LoopFrame>,
    /// The argument spans of the instructions whose diagnostic quotes source,
    /// which today is the two assertions and nothing else.
    arg_spans: BTreeMap<u32, Vec<Span>>,
}

impl<'a, 'l> Body<'a, 'l> {
    fn new(outer: &'l mut Lowering<'a>, module: &'a str) -> Body<'a, 'l> {
        Body {
            outer,
            module,
            code: Vec::new(),
            spans: Vec::new(),
            depth: Some(Depth::EMPTY),
            live: Vec::new(),
            frame_size: 0,
            kinds: Vec::new(),
            next: 0,
            labels: Vec::new(),
            patches: Vec::new(),
            loops: Vec::new(),
            arg_spans: BTreeMap::new(),
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
                | Inst::JumpIfFalseScalar(to) => *to = target,
                other => unreachable!("a patch points at a jump, not {other:?}"),
            }
        }
        Finished {
            code: self.code,
            spans: self.spans,
            arg_spans: self.arg_spans,
            frame_size: self.frame_size,
            slots: self.kinds,
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
        if matches!(inst, Inst::Return | Inst::Jump(_) | Inst::NoMatch) {
            self.depth = None;
        }
        self.code.push(inst);
        self.spans.push(span);
    }

    /// The `Return` a function ends in, emitted even where control cannot
    /// fall into it.
    ///
    /// A body whose last expression is itself a `return` leaves nothing to
    /// fall through, and a function still has to end in the instruction that
    /// ends a function: [`validate`] asks for one, and a VM that ran off the
    /// end would have nowhere to go.
    fn emit_final_return(&mut self, span: Span) {
        if self.depth.is_none() {
            if matches!(self.code.last(), Some(Inst::Return)) {
                return;
            }
            self.depth = Some(Depth {
                values: 1,
                scalars: 0,
            });
        }
        self.emit(Inst::Return, span);
    }

    fn constant(&mut self, value: Const, span: Span) {
        let id = self.outer.constant(value);
        self.emit(Inst::Const(id), span);
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
    /// does, so `let x = 1; let x = 2` is two slots. `writable` is what a
    /// write to the binding is checked against, and only a `var` is.
    ///
    /// # Why a released number is not always reused
    ///
    /// A block's slots are released at its end and a later sibling block
    /// reuses the numbers, which is what makes `frame_size` a high-water
    /// mark. A number now names storage in one of two stacks, so a number
    /// reused by a binding of the other kind would name two — and neither
    /// the VM nor `validate` could say which. So a number whose kind does
    /// not match is skipped, and the reuse that keeps a frame small still
    /// happens between the bindings that can share.
    fn declare(&mut self, name: Option<&'a str>, writable: bool, kind: SlotKind) -> u32 {
        let mut slot = self.next;
        while self
            .kinds
            .get(slot as usize)
            .is_some_and(|settled| *settled != kind)
        {
            slot += 1;
        }
        if slot as usize == self.kinds.len() {
            self.kinds.push(kind);
        }
        self.next = slot + 1;
        self.live.push(Binding {
            name,
            slot,
            writable,
            kind,
        });
        self.frame_size = self.frame_size.max(slot + 1);
        slot
    }

    /// Where a scope begins, to be handed back to [`Body::release`] when it
    /// ends.
    fn scope(&self) -> usize {
        self.live.len()
    }

    /// Releases every binding declared since `mark`, which is what ends a
    /// scope.
    ///
    /// The next slot number goes back with them: a scope's declarations are a
    /// stack, so one past the highest still-live slot is where the block that
    /// contained them was.
    fn release(&mut self, mark: usize) {
        self.live.truncate(mark);
        self.next = self.live.last().map_or(0, |binding| binding.slot + 1);
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
            SlotKind::Value => None,
        }
    }

    /// Whether source may write the binding `name` reaches.
    fn is_writable(&self, name: &str) -> bool {
        self.binding(name).is_some_and(|binding| binding.writable)
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
    fn scalar_of(&self, expr: &Expr) -> Option<Scalar> {
        match self.settled(expr) {
            Some(Ty::Int) => Some(Scalar::Int),
            Some(Ty::Bool) => Some(Scalar::Bool),
            _ => None,
        }
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
            ExprKind::Binary { op, lhs, rhs } => binary_op(*op)
                .is_some_and(|op| matches!(self.binary_inst(op, lhs, rhs), Inst::IntBinary(_))),
            _ => false,
        }
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

    /// Lowers a block, leaving its value on the stack.
    ///
    /// The slots the block declared are released at its end, so a later
    /// sibling block reuses the numbers and `frame_size` stays a high-water
    /// mark rather than a total.
    fn block(&mut self, block: &'a Block) -> Result<(), Unsupported> {
        self.block_at(block, Position::Value)
    }

    /// The same block, lowered for its value or for its effect.
    ///
    /// A block's value is its tail's, so a block lowered for effect lowers
    /// its tail for effect too and a block with no tail builds no `Unit` at
    /// all. Its statements are unaffected: they were already lowered for
    /// their effect, whichever position the block itself is in.
    fn block_at(&mut self, block: &'a Block, position: Position) -> Result<(), Unsupported> {
        let mark = self.scope();
        for statement in &block.statements {
            self.statement(statement)?;
        }
        match (&block.tail, position) {
            (Some(tail), _) => self.expr_at(tail, position)?,
            (None, Position::Value) => self.constant(Const::Unit, block.span),
            (None, Position::Effect) => {}
        }
        self.release(mark);
        Ok(())
    }

    fn statement(&mut self, statement: &'a Stmt) -> Result<(), Unsupported> {
        match &statement.kind {
            StmtKind::Let {
                is_var,
                name,
                ty,
                value,
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
                let kind = self.slot_kind(value);
                match kind {
                    SlotKind::Scalar(_) => self.expr_scalar(value)?,
                    SlotKind::Value => self.expr(value)?,
                }
                let slot = self.declare(Some(name.node.as_str()), *is_var, kind);
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
            } => self.call(expr.id, callee, args, trailing.is_some(), span)?,
            ExprKind::Unary { op, operand } => {
                self.expr(operand)?;
                let op = match op {
                    SourceUnary::Not => UnaryOp::Not,
                    SourceUnary::Neg => UnaryOp::Neg,
                };
                self.emit(Inst::Unary(op), span);
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, span)?,
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
            ExprKind::Return(value) => {
                match value {
                    Some(value) => self.expr(value)?,
                    None => self.constant(Const::Unit, span),
                }
                self.emit(Inst::Return, span);
            }
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
            ExprKind::Range { .. } => {
                return Err(Unsupported::new(
                    "a range used as a value rather than as a `for` header",
                    span,
                ))
            }
            ExprKind::Lambda { .. } => return Err(Unsupported::new("a closure", span)),
            ExprKind::Match { scrutinee, arms } => {
                return self.match_expr(scrutinee, arms, position, span)
            }
            ExprKind::Scope { .. } => return Err(Unsupported::new("a task scope", span)),
            ExprKind::Await(_) => return Err(Unsupported::new("an `await`", span)),
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
        if let Some(slot) = self.lookup(name) {
            self.emit(Inst::LoadLocal(slot), span);
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
        if self.outer.function_of(self.module, name).is_some() {
            return Err(Unsupported::new(
                format!("`{name}`, a function used as a value"),
                span,
            ));
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
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if let Some((owner, _)) = self.outer.enum_of(self.module, head) {
                    // `Status.Confirmed`: a case written without a call, so
                    // its payload is empty. Whether the enum declares such a
                    // case is settled where the interpreter settles it — in
                    // `enum_case`, at run time — because a case that does not
                    // exist is a failure with a message rather than a shape
                    // the lowering could produce something else for.
                    return self.make_enum(owner, head, name, &[], span);
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
        op: SourceBinary,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
    ) -> Result<(), Unsupported> {
        match op {
            SourceBinary::And | SourceBinary::Or => {
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
        if !self.is_writable(name) {
            return Err(read_only_place(name, span));
        }
        match op {
            None => match kind {
                SlotKind::Scalar(_) => self.expr_scalar(value)?,
                SlotKind::Value => self.expr(value)?,
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
                }
            }
        }
        self.emit(store_slot(kind, slot), span);
        if position == Position::Value {
            self.constant(Const::Unit, span);
        }
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
        if !self.is_writable(name) {
            // A field of a read-only binding is a read-only place too, which
            // is what `Place::field` carries down from its base.
            return Err(read_only_place(&format!("{name}.{field}"), span));
        }
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
        if position == Position::Value {
            self.constant(Const::Unit, span);
        }
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
                if position == Position::Value {
                    self.constant(Const::Unit, span);
                }
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
            depth: base,
        });
        let lowered = self.block_at(body, Position::Effect);
        self.loops.pop();
        lowered?;
        self.jump(Inst::Jump, top, span);
        self.bind(end);
        if position == Position::Value {
            self.constant(Const::Unit, span);
        }
        Ok(())
    }

    /// `for`, over a range written in the header or over a sequence.
    ///
    /// The iterable is evaluated once, in the enclosing scope, and the
    /// binding is declared in the scope the body sees — the two halves of
    /// what the interpreter does around `iterable_items`.
    ///
    /// A range header never builds a range value: the IR has no instruction
    /// that makes one, and `for` is the only place the language reads one,
    /// so the bounds go into two hidden slots and the loop counts between
    /// them. Anything else is asked once, by `iter-items`, for the items a
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
                let cursor = self.declare(None, false, SlotKind::Value);
                let limit = self.declare(None, false, SlotKind::Value);
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
                let sequence = self.declare(None, false, SlotKind::Value);
                let length = self.declare(None, false, SlotKind::Value);
                let cursor = self.declare(None, false, SlotKind::Value);
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
        let element = self.declare(Some(binding), false, SlotKind::Value);

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
        if position == Position::Value {
            self.constant(Const::Unit, span);
        }
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
        // Whatever the half-evaluated expression around this left on either
        // stack goes with it, so the loop's exit is reached at the depths the
        // loop runs at.
        if let Some(depth) = self.depth {
            for _ in base.values..depth.values {
                self.emit(Inst::Pop, span);
            }
            for _ in base.scalars..depth.scalars {
                self.emit(Inst::ScalarPop, span);
            }
        }
        self.jump(Inst::Jump, target, span);
        Ok(())
    }

    // --------------------------------------------------------------- calls

    fn call(
        &mut self,
        id: ExprId,
        callee: &'a Expr,
        args: &'a [Arg],
        trailing: bool,
        span: Span,
    ) -> Result<(), Unsupported> {
        if trailing {
            return Err(Unsupported::new("a trailing closure", span));
        }
        for arg in args {
            if arg.is_var {
                return Err(Unsupported::new("a `var` argument", arg.span));
            }
            if arg.spread {
                return Err(Unsupported::new("a `...` spread argument", arg.span));
            }
        }
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
    fn call_named(&mut self, name: &str, args: &'a [Arg], span: Span) -> Result<(), Unsupported> {
        if self.lookup(name).is_some() {
            return Err(Unsupported::new(
                format!("a call through the local `{name}`"),
                span,
            ));
        }
        if let Some(key) = self.outer.function_of(self.module, name) {
            return self.call_declared(key, None, args, span);
        }
        if let Some((owner, decl)) = self.outer.struct_of(self.module, name) {
            return self.make_struct(owner, decl, args, span);
        }
        if self.outer.declares_enum(self.module, name) {
            return Err(Unsupported::new(
                format!("`{name}`, which names an enum"),
                span,
            ));
        }
        if let Some(module) = self.outer.host_item(self.module, name) {
            return self.call_host(module, name, args, span);
        }
        if name == builtins::MAP_ENTRY.name {
            return self.make_map_entry(args, span);
        }
        if let Some(schema) = builtins::free_builtin(name) {
            return self.make_builtin(schema.name, args, span);
        }
        Err(Unsupported::new(
            format!("a call to `{name}`, which the lowering cannot resolve"),
            span,
        ))
    }

    /// `head.name(...)`, where `head` may be a host module, an enum, a
    /// struct, or a module imported whole — and is a receiver when it is
    /// none of those.
    fn call_qualified(
        &mut self,
        id: ExprId,
        base: &'a Expr,
        name: &str,
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if self.outer.is_host_module(self.module, head) {
                    return self.call_host(head, name, args, span);
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
                    return self.make_enum(owner, head, name, args, span);
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
                        return self.make_struct(owner, decl, args, span);
                    }
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, which module `{owner}` does not export"),
                        span,
                    ));
                }
                if builtins::is_builtin_type(head) {
                    return self.call_builtin_assoc(head, name, args, span);
                }
            }
        }
        self.method_call(id, base, name, args, span)
    }

    /// A call to a function this package declares, with the receiver a
    /// method needs pushed below its arguments.
    fn call_declared(
        &mut self,
        key: Key,
        receiver: Option<&'a Expr>,
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        let declared = self.outer.declaration(key);
        let decl = declared.decl;
        let what = declared.name.clone();

        if decl.is_async {
            return Err(Unsupported::new(
                format!("a call to the `async fn` `{what}`"),
                span,
            ));
        }
        for param in &decl.params {
            reject_parameter(param.is_var, param.variadic, param.ty.as_ref(), param.span)?;
        }
        let names: Vec<&str> = decl
            .params
            .iter()
            .map(|param| param.name.node.as_str())
            .collect();
        arguments_in_order(&names, args, &what, span)?;

        match (decl.receiver, receiver) {
            (Some(declared), Some(expr)) => {
                if declared.is_var {
                    return Err(Unsupported::new(
                        format!("a call to `{what}`, which takes a `var self` receiver"),
                        span,
                    ));
                }
                self.expr(expr)?;
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
        for arg in args {
            self.expr(&arg.value)?;
        }
        let argc = args.len() as u32 + u32::from(decl.receiver.is_some());
        // This is the whole of the reachability rule: the call being emitted
        // is what makes the target part of the program, so the target is
        // numbered here and nowhere else.
        let function = self.outer.number(key);
        self.emit(Inst::Call { function, argc }, span);
        Ok(())
    }

    /// `console.println(...)` and `clock.now()`.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        if hosts::module(module).is_some_and(|schema| schema.declared_type(op).is_some()) {
            return Err(Unsupported::new(
                format!("`{module}.{op}`, which initializes a type a host declares"),
                span,
            ));
        }
        for arg in args {
            self.expr(&arg.value)?;
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

    /// `Ok(...)`, `Err(...)`, `Some(...)`, `Error(...)`, `assert(...)`, and
    /// `assertEqual(...)`.
    ///
    /// `Shared(...)` is the free builtin that is not here: it makes a value
    /// with storage shared across tasks, which nothing in this IR expresses.
    ///
    /// The two assertions carry their arguments' spans as well as their own.
    /// A failing `assert` quotes the source text of its condition — that is
    /// what makes it a builtin rather than a library function — and the
    /// instruction's own span covers the whole call, so the argument's span
    /// is recorded beside it in [`crate::Function::arg_spans`]. The
    /// interpreter reads exactly these spans, out of the same `SourceMap`.
    fn make_builtin(&mut self, name: &str, args: &'a [Arg], span: Span) -> Result<(), Unsupported> {
        if !matches!(
            name,
            "Ok" | "Err" | "Some" | "Error" | "assert" | "assertEqual"
        ) {
            return Err(Unsupported::new(format!("`{name}`"), span));
        }
        let quotes_its_arguments = matches!(name, "assert" | "assertEqual");
        for arg in args {
            self.expr(&arg.value)?;
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
        args: &'a [Arg],
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
        arguments_in_order(&names, args, &decl.name.node, span)?;
        for arg in args {
            self.expr(&arg.value)?;
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
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        for arg in args {
            self.expr(&arg.value)?;
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
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        for arg in args {
            self.expr(&arg.value)?;
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
    fn make_map_entry(&mut self, args: &'a [Arg], span: Span) -> Result<(), Unsupported> {
        let names: Vec<&str> = builtins::MAP_ENTRY
            .fields
            .iter()
            .map(|field| field.name)
            .collect();
        arguments_in_order(&names, args, builtins::MAP_ENTRY.name, span)?;
        for arg in args {
            self.expr(&arg.value)?;
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
            let slot = self.declare(Some(name), false, SlotKind::Value);
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
        let slot = self.declare(Some(name), false, SlotKind::Value);
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
    fn method_call(
        &mut self,
        id: ExprId,
        receiver: &'a Expr,
        name: &str,
        args: &'a [Arg],
        span: Span,
    ) -> Result<(), Unsupported> {
        // Before anything is asked about the name, because a recorded target
        // makes every one of those questions moot: which types declare this
        // name, whether a builtin shares it, and whether the builtin that
        // shares it writes through its receiver are all questions about
        // *which* declaration is meant, and the checker has answered that.
        let recorded = self.target(id, span);
        if let Some(key) = recorded.and_then(|target| self.declared_by(target)) {
            return self.call_declared(key, Some(receiver), args, span);
        }
        if name == "await" {
            return Err(Unsupported::new("an `await`", span));
        }
        if name == "snapshot" {
            return Err(Unsupported::new("`snapshot`", span));
        }
        if builtins::is_mutating_method(name) {
            return Err(Unsupported::new(
                format!("`{name}`, which writes through its receiver"),
                span,
            ));
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
            self.expr(receiver)?;
            for arg in args {
                self.expr(&arg.value)?;
            }
            let name = self.outer.name(name);
            self.emit(
                Inst::CallBuiltin {
                    name,
                    argc: args.len() as u32,
                },
                span,
            );
            return Ok(());
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

/// Whether the arguments already stand in declaration order, one for every
/// parameter.
///
/// `assign_labels` in the interpreter matches positional arguments to names
/// in order, refuses a label written out of declaration order, and refuses a
/// positional argument after a labelled one. What survives all three is a
/// call whose arguments are its parameters in order — which is what makes
/// pushing them left to right the same as pushing them in declaration order.
/// Anything else is reported rather than rearranged: a parameter left to its
/// default would need the callee to evaluate an expression the IR does not
/// carry, and a reordering would put the pushes in an order the receiver
/// does not expect.
fn arguments_in_order(
    names: &[&str],
    args: &[Arg],
    what: &str,
    span: Span,
) -> Result<(), Unsupported> {
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
                if index != position {
                    return Err(Unsupported::new(
                        format!(
                            "a call to `{what}` whose arguments do not stand in declaration order"
                        ),
                        arg.span,
                    ));
                }
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
                if position >= names.len() {
                    return Err(Unsupported::new(
                        format!("a call to `{what}` with more arguments than it has parameters"),
                        arg.span,
                    ));
                }
            }
        }
    }
    if args.len() != names.len() {
        return Err(Unsupported::new(
            format!("a call to `{what}` that does not supply one argument for every parameter"),
            span,
        ));
    }
    Ok(())
}

/// A write to a place the program is not allowed to write.
///
/// This refuses at lowering time what the interpreter refuses at run time —
/// ``cannot assign to `x`, which is a read-only place`` — because a backend
/// that performed the write would be more permissive than the oracle ADR
/// 0012 ranks above it, and being wrong in the other direction is the only
/// direction a second backend may be wrong in.
///
/// The wording says the program is wrong rather than that the VM is
/// incapable, because it is. The right home for the check is the checker,
/// where a read-only place is a static fact rather than a runtime one;
/// `cove-sema` catches neither backend's case today — mutability is not a
/// type, so `cove check` does not enforce it — and whoever moves it there
/// deletes this and the interpreter's own refusal both.
fn read_only_place(place: &str, span: Span) -> Unsupported {
    Unsupported::new(
        format!("assignment to `{place}`, which is a read-only place"),
        span,
    )
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

/// Refuses a `dyn` written anywhere in a type.
///
/// A `dyn` value is the language's one implicit conversion, made where a
/// type is *written*, and the IR has no instruction that makes one.
fn reject_dyn(ty: &Type, what: &str) -> Result<(), Unsupported> {
    if mentions_dyn(ty) {
        return Err(Unsupported::new(what, ty.span));
    }
    Ok(())
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

/// Refuses a parameter the IR has no shape for.
fn reject_parameter(
    is_var: bool,
    variadic: bool,
    ty: Option<&Type>,
    span: Span,
) -> Result<(), Unsupported> {
    if is_var {
        return Err(Unsupported::new("a `var` parameter", span));
    }
    if variadic {
        return Err(Unsupported::new("a variadic parameter", span));
    }
    if let Some(ty) = ty {
        reject_dyn(ty, "a `dyn` parameter")?;
    }
    Ok(())
}

// ---------------------------------------------------------- the invariants

/// Checks the invariants a lowered function must hold before the VM runs it.
///
/// The VM trusts its input completely — that is most of what makes it worth
/// having — so this is where the trust is earned. Every jump lands on an
/// instruction, every slot is inside the frame, every id names something,
/// every recorded argument span belongs to an instruction that exists, every
/// function ends in a `Return`, and both operand stacks have one depth at
/// every instruction control can reach: a join point arrived at with two
/// different depths is a bug in the lowering, and finding it here is the
/// difference between a failed test and a VM reading a value that is not
/// there.
///
/// A slot is addressed as what it is, too. A slot number names storage in
/// one of the two stacks, and which one is settled at lowering, so a scalar
/// instruction reaching a value slot — or the other way round — is caught
/// here rather than read as whichever eight bytes happened to stand there.
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
    if function.arity > function.frame_size {
        return Err(format!(
            "takes {} arguments but has a frame of {}",
            function.arity, function.frame_size
        ));
    }
    if function.slots.len() != function.frame_size as usize {
        return Err(format!(
            "describes {} slots of a frame of {}",
            function.slots.len(),
            function.frame_size
        ));
    }
    // An argument arrives on the caller's value stack and becomes one of the
    // callee's first slots without moving, which is the calling convention
    // and is why a parameter's slot is a value slot. A scalar parameter would
    // need the convention changed, and this slice does not change it.
    for slot in 0..function.arity as usize {
        if function.slots[slot].is_scalar() {
            return Err(format!(
                "takes argument {slot} into a scalar slot, which no call fills"
            ));
        }
    }
    if !matches!(function.code.last(), Some(Inst::Return)) {
        return Err("does not end in a `return`".to_string());
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
                if slot >= function.frame_size {
                    return Err(at(format!(
                        "reaches slot {slot} of a frame of {}",
                        function.frame_size
                    )));
                }
                if function.slots[slot as usize].is_scalar() {
                    return Err(at(format!(
                        "reaches slot {slot} as a value, and it is a scalar slot"
                    )));
                }
            }
            Inst::LoadScalar(slot) | Inst::StoreScalar(slot) => {
                if slot >= function.frame_size {
                    return Err(at(format!(
                        "reaches slot {slot} of a frame of {}",
                        function.frame_size
                    )));
                }
                if !function.slots[slot as usize].is_scalar() {
                    return Err(at(format!(
                        "reaches slot {slot} as a scalar, and it is a value slot"
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
            Inst::Jump(to)
            | Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to) => {
                if to as usize >= length {
                    return Err(at(format!("jumps to {to}, past the {length} instructions")));
                }
            }
            Inst::Call {
                function: target,
                argc,
            } => {
                let Some(target) = program.functions.get(target.0 as usize) else {
                    return Err(at(format!(
                        "calls function {}, which does not exist",
                        target.0
                    )));
                };
                if target.arity != argc {
                    return Err(at(format!(
                        "calls `{}.{}` with {argc} arguments, which takes {}",
                        target.module, target.name, target.arity
                    )));
                }
            }
            Inst::CallHost { module, op, .. } => {
                constant(module, "the host module")?;
                constant(op, "the host operation")?;
            }
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
    let mut depths: Vec<Option<(i64, i64)>> = vec![None; length];
    let mut pending = vec![(0usize, (0i64, 0i64))];
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
                    "{pc}: reached with {} values and {} scalars on the stack and with {} and {}",
                    depth.0, depth.1, seen.0, seen.1
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
        let after = (
            depth.0 - i64::from(shape.values.0) + i64::from(shape.values.1),
            depth.1 - i64::from(shape.scalars.0) + i64::from(shape.scalars.1),
        );
        match inst {
            // Neither continues: a return leaves the frame, and a `match`
            // that covered nothing stops the run.
            Inst::Return | Inst::NoMatch => {}
            Inst::Jump(to) => pending.push((to as usize, after)),
            Inst::JumpIfFalse(to) | Inst::JumpIfTrue(to) | Inst::JumpIfFalseScalar(to) => {
                pending.push((to as usize, after));
                pending.push((pc + 1, after));
            }
            _ => pending.push((pc + 1, after)),
        }
    }
    Ok(())
}

/// How many operands an instruction takes off each stack and puts back.
///
/// Two pairs rather than one, because there are two stacks and an
/// instruction may read one and write the other: that is what a boundary
/// instruction *is*.
#[derive(Clone, Copy)]
struct Shape {
    /// Taken off, and put back on, the value stack.
    values: (u32, u32),
    /// Taken off, and put back on, the scalar stack.
    scalars: (u32, u32),
}

impl Shape {
    /// An instruction that touches only the value stack.
    const fn on_values(taken: u32, left: u32) -> Shape {
        Shape {
            values: (taken, left),
            scalars: (0, 0),
        }
    }

    /// An instruction that touches only the scalar stack.
    const fn on_scalars(taken: u32, left: u32) -> Shape {
        Shape {
            values: (0, 0),
            scalars: (taken, left),
        }
    }
}

/// How many operands an instruction takes off each stack and puts back.
///
/// One description, read by the lowering as it emits and by [`validate`] as
/// it simulates, so the two cannot disagree about what an instruction does.
fn stack_shape(constants: &[Const], inst: Inst) -> Shape {
    match inst {
        Inst::Const(_) | Inst::LoadLocal(_) | Inst::LoadCapture(_) => Shape::on_values(0, 1),
        Inst::StoreLocal(_) | Inst::Pop => Shape::on_values(1, 0),
        Inst::Dup => Shape::on_values(1, 2),
        Inst::Unary(_) | Inst::GetField(_) | Inst::GetFieldAt(_) | Inst::Try => {
            Shape::on_values(1, 1)
        }
        Inst::Binary(_) | Inst::SetField(_) => Shape::on_values(2, 1),
        // The typed operator is the scalar stack's: two `i64` in, one out.
        Inst::IntBinary(_) => Shape::on_scalars(2, 1),
        Inst::ScalarConst(_) | Inst::LoadScalar(_) => Shape::on_scalars(0, 1),
        Inst::StoreScalar(_) | Inst::ScalarPop | Inst::JumpIfFalseScalar(_) => {
            Shape::on_scalars(1, 0)
        }
        // The two boundary instructions, and the only ones that move
        // anything between the stacks.
        Inst::ScalarToValue(_) => Shape {
            values: (0, 1),
            scalars: (1, 0),
        },
        Inst::ValueToScalar => Shape {
            values: (1, 0),
            scalars: (0, 1),
        },
        Inst::Jump(_) => Shape::on_values(0, 0),
        Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) | Inst::Return => Shape::on_values(1, 0),
        Inst::Call { argc, .. } | Inst::CallHost { argc, .. } | Inst::MakeBuiltin { argc, .. } => {
            Shape::on_values(argc, 1)
        }
        // The receiver sits below the arguments.
        Inst::CallBuiltin { argc, .. } => Shape::on_values(argc + 1, 1),
        Inst::MakeArray(len) => Shape::on_values(len, 1),
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

    #[test]
    fn every_literal_loads_one_constant() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  let a = 1\n  let b = 1.5\n  let c = true\n  let d = 250ms\n  let e = ()\n  let g = \"hi\"\n  a\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=6 scalars=[0:Int, 2:Bool]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Float(1.5)\n\
             \x20  3  store 1\n\
             \x20  4  scalar-const 1\n\
             \x20  5  store-scalar 2\n\
             \x20  6  const Duration(250000000)\n\
             \x20  7  store 3\n\
             \x20  8  const Unit\n\
             \x20  9  store 4\n\
             \x20 10  const Str(\"hi\")\n\
             \x20 11  store 5\n\
             \x20 12  load-scalar 0\n\
             \x20 13  scalar-to-value Int\n\
             \x20 14  return\n"
        );
    }

    #[test]
    fn an_interpolated_string_renders_its_parts_left_to_right() {
        assert_eq!(
            listing("fn f(n: Int) -> String {\n  \"tick {n}!\"\n}\n", "f"),
            "fn m.f arity=1 frame=1\n\
             \x20  0  const Str(\"tick \")\n\
             \x20  1  load 0\n\
             \x20  2  const Str(\"!\")\n\
             \x20  3  concat 3\n\
             \x20  4  return\n"
        );
    }

    #[test]
    fn a_string_with_nothing_interpolated_is_one_constant() {
        assert_eq!(
            listing("fn f() -> String {\n  \"tick\"\n}\n", "f"),
            "fn m.f arity=0 frame=0\n\
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
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 0\n\
             \x20  4  load-scalar 0\n\
             \x20  5  scalar-const 3\n\
             \x20  6  int Add\n\
             \x20  7  store-scalar 0\n\
             \x20  8  load-scalar 0\n\
             \x20  9  scalar-to-value Int\n\
             \x20 10  return\n"
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
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
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
            "fn m.f arity=2 frame=2\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  load 1\n\
             \x20  3  value-to-scalar\n\
             \x20  4  int Mul\n\
             \x20  5  load 0\n\
             \x20  6  value-to-scalar\n\
             \x20  7  int Div\n\
             \x20  8  load 1\n\
             \x20  9  value-to-scalar\n\
             \x20 10  int Rem\n\
             \x20 11  load 0\n\
             \x20 12  value-to-scalar\n\
             \x20 13  int Sub\n\
             \x20 14  load 1\n\
             \x20 15  value-to-scalar\n\
             \x20 16  int Add\n\
             \x20 17  load 0\n\
             \x20 18  value-to-scalar\n\
             \x20 19  int Ne\n\
             \x20 20  scalar-to-value Bool\n\
             \x20 21  return\n"
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
            "fn m.f arity=6 frame=8 scalars=[6:Int]\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  load 1\n\
             \x20  3  value-to-scalar\n\
             \x20  4  int Add\n\
             \x20  5  store-scalar 6\n\
             \x20  6  load 2\n\
             \x20  7  load 3\n\
             \x20  8  binary Add\n\
             \x20  9  store 7\n\
             \x20 10  load 4\n\
             \x20 11  load 5\n\
             \x20 12  binary Add\n\
             \x20 13  return\n"
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
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at 0\n\
             \x20  2  value-to-scalar\n\
             \x20  3  load 0\n\
             \x20  4  get-field-at 1\n\
             \x20  5  value-to-scalar\n\
             \x20  6  int Add\n\
             \x20  7  scalar-to-value Int\n\
             \x20  8  return\n"
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
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  call m.Box.length argc=1\n\
             \x20  2  value-to-scalar\n\
             \x20  3  const Int(1)\n\
             \x20  4  const Int(2)\n\
             \x20  5  const Int(3)\n\
             \x20  6  make-array 3\n\
             \x20  7  call-builtin length argc=0\n\
             \x20  8  value-to-scalar\n\
             \x20  9  int Add\n\
             \x20 10  scalar-to-value Int\n\
             \x20 11  return\n"
        );
    }

    #[test]
    fn a_unary_operator_applies_to_what_was_pushed() {
        assert_eq!(
            listing(
                "fn f(b: Bool) -> Int {\n  if !b {\n    return -1\n  }\n  0\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  unary Not\n\
             \x20  2  jump-if-false 6\n\
             \x20  3  const Int(1)\n\
             \x20  4  unary Neg\n\
             \x20  5  return\n\
             \x20  6  const Int(0)\n\
             \x20  7  return\n"
        );
    }

    #[test]
    fn and_and_or_short_circuit_through_jumps() {
        assert_eq!(
            listing("fn f(a: Bool, b: Bool) -> Bool {\n  a && b || a\n}\n", "f"),
            "fn m.f arity=2 frame=2\n\
             \x20  0  load 0\n\
             \x20  1  jump-if-false 4\n\
             \x20  2  load 1\n\
             \x20  3  jump 5\n\
             \x20  4  const Bool(false)\n\
             \x20  5  jump-if-true 8\n\
             \x20  6  load 0\n\
             \x20  7  jump 9\n\
             \x20  8  const Bool(true)\n\
             \x20  9  return\n"
        );
    }

    #[test]
    fn a_block_with_no_tail_is_unit() {
        assert_eq!(
            listing("fn f() -> Unit {\n  let a = 1\n}\n", "f"),
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
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
            "fn m.f arity=1 frame=2 scalars=[1:Int]\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 2\n\
             \x20  3  int Lt\n\
             \x20  4  jump-if-false-scalar 9\n\
             \x20  5  scalar-const 1\n\
             \x20  6  store-scalar 1\n\
             \x20  7  const Unit\n\
             \x20  8  jump 12\n\
             \x20  9  scalar-const 2\n\
             \x20 10  store-scalar 1\n\
             \x20 11  const Unit\n\
             \x20 12  return\n"
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
            "fn m.f arity=1 frame=2 scalars=[1:Int]\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 2\n\
             \x20  3  int Lt\n\
             \x20  4  jump-if-false-scalar 7\n\
             \x20  5  const Int(1)\n\
             \x20  6  jump 8\n\
             \x20  7  const Int(2)\n\
             \x20  8  value-to-scalar\n\
             \x20  9  store-scalar 1\n\
             \x20 10  load-scalar 1\n\
             \x20 11  scalar-to-value Int\n\
             \x20 12  return\n"
        );
    }

    #[test]
    fn an_if_with_an_else_joins_both_branches() {
        assert_eq!(
            listing(
                "fn f(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    n - 1\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 2\n\
             \x20  3  int Lt\n\
             \x20  4  jump-if-false-scalar 7\n\
             \x20  5  load 0\n\
             \x20  6  jump 12\n\
             \x20  7  load 0\n\
             \x20  8  value-to-scalar\n\
             \x20  9  scalar-const 1\n\
             \x20 10  int Sub\n\
             \x20 11  scalar-to-value Int\n\
             \x20 12  return\n"
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
            "fn m.f arity=1 frame=2 scalars=[1:Int]\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load 0\n\
             \x20  3  value-to-scalar\n\
             \x20  4  scalar-const 2\n\
             \x20  5  int Lt\n\
             \x20  6  jump-if-false-scalar 9\n\
             \x20  7  scalar-const 1\n\
             \x20  8  store-scalar 1\n\
             \x20  9  load-scalar 1\n\
             \x20 10  scalar-to-value Int\n\
             \x20 11  return\n"
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
            "fn m.f arity=1 frame=2 scalars=[1:Int]\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load 0\n\
             \x20  3  value-to-scalar\n\
             \x20  4  scalar-const 2\n\
             \x20  5  int Lt\n\
             \x20  6  jump-if-false-scalar 9\n\
             \x20  7  scalar-const 1\n\
             \x20  8  store-scalar 1\n\
             \x20  9  const Unit\n\
             \x20 10  return\n"
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
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
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
             \x20 12  scalar-to-value Int\n\
             \x20 13  return\n"
        );
    }

    /// A range header never asks `iter-items` for anything.
    ///
    /// It builds no value at all: the bounds go into two hidden slots and the
    /// loop counts between them, which is faster than materialising every
    /// element and answers exactly what walking the range's items would.
    #[test]
    fn a_for_over_a_range_counts_between_two_hidden_slots() {
        let listed = listing(
            "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
            "f",
        );
        assert!(!listed.contains("iter-items"), "{listed}");
        assert_eq!(
            listing(
                "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=4 scalars=[0:Int]\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Int(0)\n\
             \x20  3  store 1\n\
             \x20  4  const Int(3)\n\
             \x20  5  store 2\n\
             \x20  6  load 1\n\
             \x20  7  load 2\n\
             \x20  8  binary Lt\n\
             \x20  9  jump-if-false 22\n\
             \x20 10  load 1\n\
             \x20 11  store 3\n\
             \x20 12  load-scalar 0\n\
             \x20 13  load 3\n\
             \x20 14  value-to-scalar\n\
             \x20 15  int Add\n\
             \x20 16  store-scalar 0\n\
             \x20 17  load 1\n\
             \x20 18  const Int(1)\n\
             \x20 19  binary Add\n\
             \x20 20  store 1\n\
             \x20 21  jump 6\n\
             \x20 22  load-scalar 0\n\
             \x20 23  scalar-to-value Int\n\
             \x20 24  return\n"
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
            "fn m.f arity=1 frame=6 scalars=[1:Int]\n\
             \x20  0  scalar-const 0\n\
             \x20  1  store-scalar 1\n\
             \x20  2  load 0\n\
             \x20  3  iter-items\n\
             \x20  4  store 2\n\
             \x20  5  load 2\n\
             \x20  6  call-builtin length argc=0\n\
             \x20  7  store 3\n\
             \x20  8  const Int(0)\n\
             \x20  9  store 4\n\
             \x20 10  load 4\n\
             \x20 11  load 3\n\
             \x20 12  binary Lt\n\
             \x20 13  jump-if-false 29\n\
             \x20 14  load 2\n\
             \x20 15  load 4\n\
             \x20 16  call-builtin get argc=1\n\
             \x20 17  try\n\
             \x20 18  store 5\n\
             \x20 19  load-scalar 1\n\
             \x20 20  load 5\n\
             \x20 21  value-to-scalar\n\
             \x20 22  int Add\n\
             \x20 23  store-scalar 1\n\
             \x20 24  load 4\n\
             \x20 25  const Int(1)\n\
             \x20 26  binary Add\n\
             \x20 27  store 4\n\
             \x20 28  jump 10\n\
             \x20 29  load-scalar 1\n\
             \x20 30  scalar-to-value Int\n\
             \x20 31  return\n"
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
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
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
             \x20 22  scalar-to-value Int\n\
             \x20 23  return\n"
        );
    }

    #[test]
    fn a_call_reaches_a_declaration_and_a_function_reaches_itself() {
        assert_eq!(
            listing(
                "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n",
                "fib"
            ),
            "fn m.fib arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 2\n\
             \x20  3  int Lt\n\
             \x20  4  jump-if-false-scalar 7\n\
             \x20  5  load 0\n\
             \x20  6  jump 23\n\
             \x20  7  load 0\n\
             \x20  8  value-to-scalar\n\
             \x20  9  scalar-const 1\n\
             \x20 10  int Sub\n\
             \x20 11  scalar-to-value Int\n\
             \x20 12  call m.fib argc=1\n\
             \x20 13  value-to-scalar\n\
             \x20 14  load 0\n\
             \x20 15  value-to-scalar\n\
             \x20 16  scalar-const 2\n\
             \x20 17  int Sub\n\
             \x20 18  scalar-to-value Int\n\
             \x20 19  call m.fib argc=1\n\
             \x20 20  value-to-scalar\n\
             \x20 21  int Add\n\
             \x20 22  scalar-to-value Int\n\
             \x20 23  return\n"
        );
    }

    #[test]
    fn arguments_are_pushed_left_to_right() {
        assert_eq!(
            listing(
                "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1, 2)\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  call m.g argc=2\n\
             \x20  3  return\n"
        );
    }

    const STRUCT_AND_METHOD: &str = "struct P {\n  x: Int\n  y: Int\n}\n\nimpl P {\n  fn sum(self) -> Int {\n    self.x + self.y\n  }\n}\n\nfn f() -> Int {\n  let p = P(x: 1, y: 2)\n  p.sum() + p.x\n}\n";

    #[test]
    fn a_struct_is_built_in_declaration_order_and_read_by_field() {
        assert_eq!(
            listing(STRUCT_AND_METHOD, "f"),
            "fn m.f arity=0 frame=1\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  make-struct m.P fields=x,y\n\
             \x20  3  store 0\n\
             \x20  4  load 0\n\
             \x20  5  call m.P.sum argc=1\n\
             \x20  6  value-to-scalar\n\
             \x20  7  load 0\n\
             \x20  8  get-field-at 0\n\
             \x20  9  value-to-scalar\n\
             \x20 10  int Add\n\
             \x20 11  scalar-to-value Int\n\
             \x20 12  return\n"
        );
    }

    #[test]
    fn a_method_takes_its_receiver_in_slot_zero() {
        assert_eq!(
            listing(STRUCT_AND_METHOD, "P.sum"),
            "fn m.P.sum arity=1 frame=1 receiver\n\
             \x20  0  load 0\n\
             \x20  1  get-field-at 0\n\
             \x20  2  value-to-scalar\n\
             \x20  3  load 0\n\
             \x20  4  get-field-at 1\n\
             \x20  5  value-to-scalar\n\
             \x20  6  int Add\n\
             \x20  7  scalar-to-value Int\n\
             \x20  8  return\n"
        );
    }

    #[test]
    fn a_host_operation_is_called_through_its_module() {
        assert_eq!(
            listing(
                "use console.println\nuse clock\n\nfn f() -> Result<Unit, Error> {\n  let at = clock.now()\n  println(\"at {at}\")?\n  Ok(())\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=1\n\
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
    fn a_builtin_method_takes_its_receiver_below_its_arguments() {
        assert_eq!(
            listing(
                "fn f(items: Array<Int>) -> Int {\n  items.get(0).unwrapOr(7)\n}\n",
                "f"
            ),
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  const Int(0)\n\
             \x20  2  call-builtin get argc=1\n\
             \x20  3  const Int(7)\n\
             \x20  4  call-builtin unwrapOr argc=1\n\
             \x20  5  return\n"
        );
    }

    #[test]
    fn a_free_builtin_is_built_from_its_arguments() {
        assert_eq!(
            listing(
                "fn f() -> Result<Unit, Error> {\n  assertEqual(1, 1)?\n  Ok(())\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=0\n\
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
            "fn m.f arity=0 frame=0\n\
             \x20  0  make-builtin None argc=0\n\
             \x20  1  return\n"
        );
    }

    #[test]
    fn an_array_literal_collects_its_elements_left_to_right() {
        assert_eq!(
            listing("fn f() -> Array<Int> {\n  [1, 2, 3]\n}\n", "f"),
            "fn m.f arity=0 frame=0\n\
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
            "fn m.f arity=1 frame=1\n\
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
            "fn m.f arity=1 frame=1\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  scalar-const 0\n\
             \x20  3  int Lt\n\
             \x20  4  jump-if-false-scalar 7\n\
             \x20  5  const Int(0)\n\
             \x20  6  return\n\
             \x20  7  load 0\n\
             \x20  8  return\n"
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
            "fn m.f arity=0 frame=2 scalars=[0:Int, 1:Int]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-const 1\n\
             \x20  4  int Add\n\
             \x20  5  store-scalar 1\n\
             \x20  6  load-scalar 1\n\
             \x20  7  scalar-to-value Int\n\
             \x20  8  return\n"
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
            "fn m.f arity=0 frame=1 scalars=[0:Int]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  load-scalar 0\n\
             \x20  3  scalar-to-value Int\n\
             \x20  4  pop\n\
             \x20  5  scalar-const 2\n\
             \x20  6  store-scalar 0\n\
             \x20  7  load-scalar 0\n\
             \x20  8  scalar-to-value Int\n\
             \x20  9  return\n"
        );
    }

    /// `frame_size` is the high-water mark: three bindings are live at once
    /// inside the nested block, and one of them is the outer body's.
    #[test]
    fn the_frame_is_as_big_as_the_most_that_was_ever_live() {
        assert_eq!(
            listing(
                "fn f() -> Int {\n  let a = 1\n  {\n    let b = 2\n    let c = 3\n    b + c\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=3 scalars=[0:Int, 1:Int, 2:Int]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  scalar-const 2\n\
             \x20  3  store-scalar 1\n\
             \x20  4  scalar-const 3\n\
             \x20  5  store-scalar 2\n\
             \x20  6  load-scalar 1\n\
             \x20  7  load-scalar 2\n\
             \x20  8  int Add\n\
             \x20  9  scalar-to-value Int\n\
             \x20 10  return\n"
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
            "fn m.f arity=1 frame=2 scalars=[1:Int]\n\
             \x20  0  load 0\n\
             \x20  1  value-to-scalar\n\
             \x20  2  store-scalar 1\n\
             \x20  3  load-scalar 1\n\
             \x20  4  scalar-to-value Int\n\
             \x20  5  return\n"
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
            "fn m.f arity=0 frame=0\n\
             \x20  0  const Int(1)\n\
             \x20  1  make-enum m.E.B argc=1\n\
             \x20  2  return\n"
        );
    }

    /// A case that carries nothing is written without a call, and lowers to
    /// the same instruction over no payload.
    #[test]
    fn a_case_that_carries_nothing_is_built_from_nothing() {
        assert_eq!(
            listing(&format!("{ENUM}fn f() -> E {{\n  E.A\n}}\n"), "f"),
            "fn m.f arity=0 frame=0\n\
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
            "fn m.f arity=1 frame=2\n\
             \x20  0  load 0\n\
             \x20  1  test-case E.A\n\
             \x20  2  jump-if-false 6\n\
             \x20  3  pop\n\
             \x20  4  const Int(1)\n\
             \x20  5  jump 16\n\
             \x20  6  test-case E.B\n\
             \x20  7  jump-if-false 15\n\
             \x20  8  get-payload 0\n\
             \x20  9  dup\n\
             \x20 10  store 1\n\
             \x20 11  pop\n\
             \x20 12  pop\n\
             \x20 13  load 1\n\
             \x20 14  jump 16\n\
             \x20 15  no-match\n\
             \x20 16  return\n"
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
            "fn m.f arity=1 frame=2\n\
             \x20  0  load 0\n\
             \x20  1  test-case Pair.L\n\
             \x20  2  jump-if-false 10\n\
             \x20  3  get-payload 0\n\
             \x20  4  dup\n\
             \x20  5  store 1\n\
             \x20  6  pop\n\
             \x20  7  pop\n\
             \x20  8  load 1\n\
             \x20  9  jump 20\n\
             \x20 10  test-case Pair.R\n\
             \x20 11  jump-if-false 19\n\
             \x20 12  get-payload 0\n\
             \x20 13  dup\n\
             \x20 14  store 1\n\
             \x20 15  pop\n\
             \x20 16  pop\n\
             \x20 17  load 1\n\
             \x20 18  jump 20\n\
             \x20 19  no-match\n\
             \x20 20  return\n"
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
            "fn m.f arity=1 frame=2\n\
             \x20  0  load 0\n\
             \x20  1  test-case Ok\n\
             \x20  2  jump-if-false 16\n\
             \x20  3  get-payload 0\n\
             \x20  4  test-case Some\n\
             \x20  5  jump-if-true 8\n\
             \x20  6  pop\n\
             \x20  7  jump 16\n\
             \x20  8  get-payload 0\n\
             \x20  9  dup\n\
             \x20 10  store 1\n\
             \x20 11  pop\n\
             \x20 12  pop\n\
             \x20 13  pop\n\
             \x20 14  load 1\n\
             \x20 15  jump 19\n\
             \x20 16  pop\n\
             \x20 17  const Int(0)\n\
             \x20 18  jump 19\n\
             \x20 19  return\n"
        );
    }

    /// An associated function of a builtin type reads its arguments and
    /// nothing else, because there is no receiver to stand below them.
    #[test]
    fn an_associated_function_reads_its_arguments_alone() {
        assert_eq!(
            listing("fn f() -> Int {\n  Vector.of(1, 2).length()\n}\n", "f"),
            "fn m.f arity=0 frame=0\n\
             \x20  0  const Int(1)\n\
             \x20  1  const Int(2)\n\
             \x20  2  call-assoc Vector.of argc=2\n\
             \x20  3  call-builtin length argc=0\n\
             \x20  4  return\n"
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
            "fn m.f arity=0 frame=0\n\
             \x20  0  const Str(\"a\")\n\
             \x20  1  const Int(1)\n\
             \x20  2  make-builtin MapEntry argc=2\n\
             \x20  3  get-field key\n\
             \x20  4  return\n"
        );
    }

    // -------------------------------------------------------- unsupported

    #[test]
    fn every_unsupported_construct_is_named() {
        let cases: Vec<(&str, &str)> = vec![
            (
                "a closure",
                "fn f() -> Result<Int, Error> {\n  Err(Error(message: \"a\")).mapError(fn(e) {\n    Error(message: \"b\")\n  })\n}\n",
            ),
            (
                "a trailing closure",
                "fn f() -> Result<Int, Error> {\n  Err(Error(message: \"a\")).mapError {\n    Error(message: \"b\")\n  }\n}\n",
            ),
            (
                "a task scope",
                "fn f() -> Int {\n  scope tasks {\n    1\n  }\n}\n",
            ),
            (
                "an `await`",
                "async fn g() -> Int {\n  1\n}\n\nfn f() -> Int {\n  await g()\n}\n",
            ),
            (
                "a `var` parameter",
                "fn g(var x: Int) {\n  x = 1\n}\n",
            ),
            (
                "a `dyn` parameter",
                "trait Show {\n  fn show(self) -> String\n}\n\nstruct A {\n  n: Int\n}\n\nimpl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\nfn f(v: dyn Show) -> String {\n  v.show()\n}\n",
            ),
            (
                "`Shared`",
                "fn f() -> Int {\n  let s = Shared(1)\n  1\n}\n",
            ),
            (
                "`snapshot`",
                "fn f(a: Array<Int>) -> Array<Int> {\n  a.snapshot()\n}\n",
            ),
            (
                "a range used as a value rather than as a `for` header",
                "fn f() -> Int {\n  let r = 0..<3\n  r.length()\n}\n",
            ),
            (
                "assignment to a field of anything but a local",
                "struct Q {\n  x: Int\n}\n\nstruct P {\n  q: Q\n}\n\nfn f() -> Int {\n  var p = P(q: Q(x: 1))\n  p.q.x = 2\n  p.q.x\n}\n",
            ),
            (
                "a call through the local `g`",
                "fn f(g: fn(Int) -> Int) -> Int {\n  g(1)\n}\n",
            ),
            (
                "a function declared inside a function body",
                "fn f() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n",
            ),
            (
                "`g`, a function used as a value",
                "fn g() -> Int {\n  1\n}\n\nfn f() -> Int {\n  let h = g\n  1\n}\n",
            ),
            (
                "a variadic parameter",
                "fn g(items: Int...) -> Int {\n  1\n}\n\nfn f() -> Int {\n  g(1, 2)\n}\n",
            ),
            (
                "a call to `g` whose arguments do not stand in declaration order",
                "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(b: 2, a: 1)\n}\n",
            ),
            (
                "a call to `g` that does not supply one argument for every parameter",
                "fn g(a: Int, b: Int = 2) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
            ),
            (
                "assignment to `x`, which is a read-only place",
                "fn f() -> Int {\n  let x = 1\n  x = 2\n  x\n}\n",
            ),
            (
                "assignment to `n`, which is a read-only place",
                "fn f(n: Int) -> Int {\n  n = 2\n  n\n}\n",
            ),
            (
                "assignment to `p.x`, which is a read-only place",
                "struct P {\n  x: Int\n}\n\nfn f() -> Int {\n  let p = P(x: 1)\n  p.x = 2\n  p.x\n}\n",
            ),
        ];
        for (what, source) in cases {
            assert_eq!(refused(source), what, "for:\n{source}");
        }
    }

    /// The `Display` a diagnostic shows says which backend refused, so a
    /// person reading it knows the program is fine and the VM is not ready.
    #[test]
    fn an_unsupported_construct_reads_as_a_sentence() {
        let why = lower(&checked(
            "fn f() -> Int {\n  scope tasks {\n    1\n  }\n}\n",
        ))
        .expect_err("a task scope is refused");
        assert_eq!(why.to_string(), "the VM cannot run a task scope yet");
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
            "fn arith.main arity=0 frame=2 scalars=[0:Int, 1:Int]\n\
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
            "fn m.f arity=0 frame=1\n\
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
             \x20 13  get-field-at 0\n\
             \x20 14  return\n"
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
            "fn startup.main arity=0 frame=0\n\
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

    /// Issue #115 itself: `hello` is three lines and `callbacks/` holds a
    /// closure, and the two share a package and nothing else.
    #[test]
    fn an_entry_lowers_past_a_construct_it_cannot_reach() {
        let checked = examples();
        let refused = lower(&checked).expect_err("`examples/` holds a program that does not lower");
        assert_eq!(refused.what, "a closure");

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
        let source = "fn helper() -> Result<Int, Error> {\n  \
                      Err(Error(message: \"a\")).mapError(fn(e) {\n    \
                      Error(message: \"b\")\n  })\n}\n\n\
                      fn main() -> Result<Int, Error> {\n  helper()\n}\n";
        let checked = checked(source);
        let whole = lower(&checked).expect_err("the package does not lower");
        let entry = lower_entry(&checked, "m", "main").expect_err("nor does the entry");
        assert_eq!(entry.what, "a closure");
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

    /// A scalar instruction may only reach a scalar slot, and a value
    /// instruction only a value slot.
    ///
    /// A slot number names storage in one of two stacks, and nothing about
    /// the number says which. Read as the wrong one it would answer with
    /// whichever eight bytes stood there, which is the class of mistake this
    /// check exists to make impossible rather than merely unlikely.
    #[test]
    fn validate_refuses_a_slot_addressed_as_the_wrong_kind() {
        let source = "fn f() -> Int {\n  let a = 1\n  a\n}\n";
        let mut program = lower(&checked(source)).expect("it lowers");
        program.functions[0].code[1] = Inst::StoreLocal(0);
        assert_eq!(
            validate(&program).expect_err("a scalar slot written as a value is refused"),
            "m.f: 1: reaches slot 0 as a value, and it is a scalar slot"
        );

        let mut program =
            lower(&checked("fn f(s: String) -> String {\n  s\n}\n")).expect("it lowers");
        program.functions[0].code[0] = Inst::LoadScalar(0);
        assert_eq!(
            validate(&program).expect_err("a value slot read as a scalar is refused"),
            "m.f: 0: reaches slot 0 as a scalar, and it is a value slot"
        );
    }

    /// Two sibling blocks share a slot number only when they agree about
    /// where it lives.
    ///
    /// The first block's `Int` takes slot 0 in the scalar stack. The second
    /// block's `String` cannot have slot 0 as well, because one number would
    /// then name two storages, so it takes the next number instead and the
    /// frame is one bigger than the deepest block. Reuse still happens
    /// between bindings that can share it, which the third block shows.
    #[test]
    fn sibling_blocks_share_a_slot_number_only_when_the_kinds_agree() {
        assert_eq!(
            listing(
                "fn f() -> Unit {\n  {\n    let a = 1\n  }\n  {\n    let b = \"two\"\n  }\n  {\n    let c = 3\n  }\n}\n",
                "f"
            ),
            "fn m.f arity=0 frame=2 scalars=[0:Int]\n\
             \x20  0  scalar-const 1\n\
             \x20  1  store-scalar 0\n\
             \x20  2  const Str(\"two\")\n\
             \x20  3  store 1\n\
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
        let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
        program.functions[0].code.pop();
        program.functions[0].spans.pop();
        assert_eq!(
            validate(&program).expect_err("a missing return is refused"),
            "m.f: does not end in a `return`"
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
            if let Inst::Call { argc, .. } = inst {
                *argc = 2;
            }
        }
        assert!(validate(&program)
            .expect_err("a mismatched call is refused")
            .contains("with 2 arguments, which takes 1"),);
    }
}
