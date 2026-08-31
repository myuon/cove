//! What the package declares, which of it has been reached, and the constants
//! the reached share.
//!
//! Cataloguing is what makes a name answerable and numbering is what makes a
//! function part of the program being lowered, and they are two events here
//! for a reason: a declaration this program cannot reach is a declaration
//! nothing in it refuses for. [`Lowering::number`] is the only thing that
//! hands out a [`FunctionId`], and the vector it appends to is the worklist
//! as much as it is the table, so walking it in order is walking it in the
//! order it grew.
//!
//! Every lookup here is one `crates/cove-runtime/src/interp.rs` makes at run
//! time, asked once before the run against the tables `cove-sema` resolved:
//! `find_function`, `find_method`, `is_host_module`, `declaring_module`.
//! That is what each of them is answerable to, and each says which function
//! of the interpreter it is — because a lookup that answered differently
//! would be a call reaching a different declaration, which is the one
//! mistake a second backend must not make.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use cove_diag::FileId;
use cove_diag::Span;
use cove_schema::hosts;
use cove_sema::resolve::Program as Checked;
use cove_sema::Signature;
use cove_syntax::ast::{Block, EnumDecl, ExprId, FnDecl, Param, StructDecl, Type, TypeKind};

use super::convention::slot_kind_of;
use crate::{
    Const, ConstId, Dispatch, DispatchId, Function, FunctionId, Program, SlotKind, StructField,
    StructId, StructType, Unsupported,
};

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
pub(super) struct Declared<'a> {
    /// The module whose body runs it. A method belongs to the module that
    /// declares its `impl` block, which ADR 0006 lets differ from the module
    /// that declares the type.
    pub(super) module: &'a str,
    /// The name a listing shows: `Type.method` for a method, so that a
    /// method and a free function of one name stay two functions.
    pub(super) name: String,
    /// The type a method is declared on, and nothing for a free function.
    ///
    /// Kept apart from `name` because ADR 0006 lets a conformance put a
    /// method in the module that declares the *trait*, so the module a
    /// method belongs to and the module its receiver's type belongs to are
    /// two different questions.
    pub(super) type_name: Option<&'a str>,
    /// The trait whose default body this method runs, for a method a
    /// conformance did not write.
    ///
    /// `check_conformance` materialises a trait's defaulted method as the
    /// type's own, with the trait's body — so the declaration is an ordinary
    /// one, and the only thing that distinguishes it is that its `self` is
    /// the rigid `Self` the checker bounded by the trait rather than the
    /// concrete type. That bound is not written anywhere in the declaration,
    /// so it is carried here; [`Body::bound_of`] is what reads it.
    ///
    /// [`Body::bound_of`]: crate::lower::body::Body::bound_of
    pub(super) from_trait_default: Option<&'a str>,
    pub(super) decl: &'a FnDecl,
}

/// Addresses a declaration of the package, reached or not.
///
/// A lookup answers with one of these rather than with a [`FunctionId`],
/// because finding a declaration and lowering it are two different events:
/// an id is what a lowered function is addressed by, and only a call that is
/// actually emitted earns its target one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key(pub(super) usize);

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
pub(super) enum Instance {
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
        /// arithmetic wants it — both representations hold the same value.
        ///
        /// [`Inst::CallDyn`]: crate::Inst::CallDyn
        /// [`Inst::CallValue`]: crate::Inst::CallValue
        as_value: bool,
    },
    /// An index into [`Lowering::lambdas`].
    Lambda(usize),
}

impl Instance {
    /// The instance every parameter is handed an argument, which is what a
    /// whole-package lowering seeds and what a call that omits nothing
    /// reaches.
    pub(super) fn whole(key: Key, params: usize) -> Instance {
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
    pub(super) fn dynamic(key: Key, params: usize) -> Instance {
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
///
/// [`mentioned_names`]: crate::lower::scan::mentioned_names
pub(super) struct LambdaSite<'a> {
    /// The module the lambda's body resolves names in, which is the module
    /// the lambda is written in.
    pub(super) module: &'a str,
    pub(super) params: &'a [Param],
    pub(super) body: &'a Block,
    pub(super) span: Span,
    /// The names this lambda captures, in the order their values are pushed
    /// before [`Inst::MakeClosure`] and in the order `Function::captures` gives
    /// them their slots.
    ///
    /// [`Inst::MakeClosure`]: crate::Inst::MakeClosure
    pub(super) captures: Vec<&'a str>,
    /// Which stack each of those captures takes its slot in, in the same
    /// order — the enclosing binding's own kind, with a `var` parameter's
    /// place reading as the value it names.
    ///
    /// Read from the *first* site to reach this lambda, which is the site
    /// `crate::lower::index::Lowering::number_lambda` recorded, and that is
    /// sound although a second reach could disagree about it. A capture
    /// travels as a `Value` whatever this says; what this decides is where
    /// the call puts it, and the checker's type is the same on every road to
    /// the same lambda. So a disagreement costs a conversion and cannot cost
    /// an answer.
    pub(super) capture_kinds: Vec<SlotKind>,
    /// Whether this lambda was written `async`, and so answers a settled
    /// task rather than the value its body produced.
    pub(super) is_async: bool,
    /// Whether this lambda's first parameter, if it is written `var`, names
    /// storage the caller holds rather than receiving a copy of it.
    ///
    /// True for the one closure that is written that way and called that
    /// way: the one `Shared::lock` is given. A `var` parameter is otherwise
    /// refused on a lambda, because every argument of an
    /// [`Inst::CallValue`] travels on the value stack and a place cannot —
    /// and [`Inst::Lock`] is the instruction that does not go through one.
    ///
    /// [`Inst::CallValue`]: crate::Inst::CallValue
    /// [`Inst::Lock`]: crate::Inst::Lock
    pub(super) aliases_first_param: bool,
}

/// The whole-program state one lowering carries: what the package declares,
/// which of it has been reached, and the constants the reached share.
pub(super) struct Lowering<'a> {
    pub(super) checked: &'a Checked,
    /// Every function the package declares, in the checker's order, whether
    /// or not this lowering will emit any of them.
    pub(super) catalog: Vec<Declared<'a>>,
    /// The id each specialisation was given, once something reached it.
    pub(super) numbered: BTreeMap<Instance, FunctionId>,
    /// The specialisation each id names, in the order the ids were handed
    /// out.
    ///
    /// This is the worklist as much as the table: a specialisation is
    /// appended when it is first reached, and the lowering walks the vector
    /// from the front until it stops growing.
    pub(super) reached: Vec<Instance>,
    /// Free functions, by the module that declares them and their name.
    pub(super) functions: BTreeMap<(String, String), Key>,
    /// Methods, by the module that declares the `impl` block, the type, and
    /// the method name.
    pub(super) methods: BTreeMap<(String, String, String), Key>,
    /// Every method a name answers to, for a receiver whose type the
    /// lowering has no way to name.
    ///
    /// Every one the package declares. Which of them a given call site could
    /// actually reach is [`Lowering::could_dispatch`]'s question, asked
    /// against the module the call is written in.
    pub(super) by_name: BTreeMap<String, Vec<Key>>,
    /// The modules each module can reach through `use`, transitively, and
    /// itself.
    ///
    /// A type travels only along `use` edges — a value of it is obtained by
    /// naming something that produces one — so this bounds which types a
    /// value written in a module can have.
    pub(super) visible: BTreeMap<String, BTreeSet<String>>,
    /// Every lambda some body has reached, in the order they were reached,
    /// which is what [`Instance::Lambda`] indexes.
    ///
    /// A lambda has no declaration, so there is nothing to catalogue it with
    /// ahead of time the way [`Lowering::catalog`] catalogues declarations:
    /// a lambda becomes part of the program at the moment a body lowers the
    /// expression that writes it, and that is the moment its captures are
    /// known.
    pub(super) lambdas: Vec<LambdaSite<'a>>,
    /// The entry each written lambda was catalogued as, so that one written
    /// lambda is one function however many times a body is lowered.
    ///
    /// Keyed by file and expression id, because an [`ExprId`] is unique
    /// within the file it was parsed from and a package has many files.
    pub(super) lambda_of: BTreeMap<(FileId, ExprId), usize>,
    pub(super) constants: Vec<Const>,
    /// Every struct type some body has built or read a field of, in the order
    /// they were reached, which is what [`StructId`] indexes.
    pub(super) structs: Vec<StructType>,
    /// The id each qualified type name was interned as, so that one
    /// declaration is one [`StructId`] however many sites name it.
    ///
    /// The qualified name is the key because it is what identifies the
    /// *declaration*: two modules may each declare a `Cursor`, and a value of
    /// either carries `module.Cursor`. One name, one layout — which is the
    /// property that makes a construction unable to disagree with another
    /// construction of the same type.
    pub(super) struct_ids: BTreeMap<String, StructId>,
    /// Every dynamic dispatch site some body has reached, in the order they
    /// were reached, which is what [`DispatchId`] indexes.
    ///
    /// One entry per `(trait, method)` pair rather than per call site: the
    /// implementations a call can reach are a fact about the pair, and two
    /// calls to `label()` on a `dyn Display` reach the same set.
    pub(super) dispatches: Vec<Dispatch>,
}

impl<'a> Lowering<'a> {
    /// Catalogues every declared function without numbering or lowering any
    /// of them.
    ///
    /// Cataloguing is what makes a name answerable; numbering is what makes
    /// a function part of the program being lowered, and [`Lowering::number`]
    /// is the only thing that does it.
    pub(super) fn index(checked: &'a Checked) -> Lowering<'a> {
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
            structs: Vec::new(),
            struct_ids: BTreeMap::new(),
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
    pub(super) fn number(&mut self, instance: Instance) -> FunctionId {
        if let Some(id) = self.numbered.get(&instance) {
            return *id;
        }
        let id = FunctionId(self.reached.len() as u32);
        self.numbered.insert(instance.clone(), id);
        self.reached.push(instance);
        id
    }

    /// What `key` names.
    pub(super) fn declaration(&self, key: Key) -> &Declared<'a> {
        &self.catalog[key.0]
    }

    /// The boundary the checker resolved for `key`, keyed by the
    /// declaration's own span.
    ///
    /// `None` is the checker having recorded nothing about this
    /// declaration, which a checked program does not produce. The lowering
    /// does not guess when it happens: see [`Lowering::function`], where the
    /// fallback is written down.
    pub(super) fn signature(&self, key: Key) -> Option<&'a Signature> {
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
    pub(super) fn could_dispatch(&self, from: &str, key: Key) -> bool {
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
    pub(super) fn entry_point(&self, module: &str, name: &str) -> Option<Key> {
        self.functions
            .get(&(module.to_string(), name.to_string()))
            .copied()
    }

    /// Lowers everything numbered, and everything that lowering numbers.
    ///
    /// The ids are handed out in the order the declarations were reached, so
    /// walking them in order is walking the worklist in the order it grew,
    /// and the loop ends when a pass over the last body added nothing.
    pub(super) fn reachable(mut self) -> Result<Program, Unsupported> {
        let mut functions = Vec::with_capacity(self.reached.len());
        while functions.len() < self.reached.len() {
            functions.push(self.function(FunctionId(functions.len() as u32))?);
        }
        Ok(Program {
            functions,
            constants: self.constants,
            dispatches: self.dispatches,
            structs: self.structs,
        })
    }

    /// The [`StructId`] of the struct `decl` declares in `owner`, interning it
    /// on the first site that names it.
    ///
    /// **The layout is read off the declaration and never off a construction.**
    /// A field's [`SlotKind`] is [`slot_kind_of`] over the type the checker
    /// resolved for it, which is the same rule and the same function that
    /// decides a parameter's slot, a local's and a return's — ADR 0027's
    /// "only the checker's answer about its type" asked about a field.
    ///
    /// The checker records a struct's field types as the `params` of the
    /// signature it synthesizes for the initializer `Cursor(at: 0)`, in
    /// declaration order, which `cove_sema::Signature` says in as many words.
    /// So this is the checker's answer read once rather than a second
    /// resolution of the same annotations.
    ///
    /// A declaration the checker recorded nothing about keeps every field on
    /// the value stack. That is the abstention rule the rest of the lowering
    /// follows — an unsettled type is not a scalar — and it is what stops a
    /// missing fact from becoming a wrong one.
    pub(super) fn struct_type(&mut self, owner: &str, decl: &'a StructDecl) -> StructId {
        let qualified = format!("{owner}.{}", decl.name.node);
        if let Some(id) = self.struct_ids.get(&qualified) {
            return *id;
        }
        let settled = self.checked.facts.signature(decl.span.file, decl.span);
        let fields = decl
            .fields
            .iter()
            .enumerate()
            .map(|(at, field)| StructField {
                name: field.name.node.as_str().into(),
                kind: settled
                    .and_then(|signature| signature.params.get(at))
                    .map_or(SlotKind::Value, slot_kind_of),
            })
            .collect();
        let id = StructId(self.structs.len() as u32);
        self.structs.push(StructType {
            name: qualified.as_str().into(),
            fields,
        });
        self.struct_ids.insert(qualified, id);
        id
    }

    /// The [`StructId`] of a type a *host* module declares, such as
    /// `http.Route`.
    ///
    /// Every field is a value slot, and that is not caution: a host's fields
    /// are described by a `cove_schema::TypeSchema` rather than by a
    /// declaration the checker walked, so there is no `Ty` to read and the
    /// abstention rule applies exactly as it does to a declaration the checker
    /// recorded nothing about.
    pub(super) fn host_struct_type(
        &mut self,
        module: &str,
        name: &str,
        fields: &[&str],
    ) -> StructId {
        let qualified = format!("{module}.{name}");
        if let Some(id) = self.struct_ids.get(&qualified) {
            return *id;
        }
        let id = StructId(self.structs.len() as u32);
        self.structs.push(StructType {
            name: qualified.as_str().into(),
            fields: fields
                .iter()
                .map(|name| StructField {
                    name: (*name).into(),
                    kind: SlotKind::Value,
                })
                .collect(),
        });
        self.struct_ids.insert(qualified, id);
        id
    }

    /// Interns a constant, so that one value is one [`ConstId`] however many
    /// instructions load it.
    pub(super) fn constant(&mut self, value: Const) -> ConstId {
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
    pub(super) fn name(&mut self, text: &str) -> ConstId {
        self.constant(Const::Name(text.into()))
    }

    /// The function `module` reaches by the bare name `name`: its own
    /// declaration first, and the one a `use` imported under that name
    /// second, exactly as `Interpreter::find_function` does.
    pub(super) fn function_of(&self, module: &str, name: &str) -> Option<Key> {
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
    pub(super) fn struct_of(&self, module: &str, name: &str) -> Option<(&'a str, &'a StructDecl)> {
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
    pub(super) fn enum_of(&self, module: &str, name: &str) -> Option<(&'a str, &'a EnumDecl)> {
        let (module, resolved) = self.checked.modules.get_key_value(module)?;
        if let Some(entry) = resolved.enums.get(name) {
            return Some((module.as_str(), &entry.decl));
        }
        let owner = resolved.imports.get(name)?;
        let (owner, resolved) = self.checked.modules.get_key_value(owner)?;
        Some((owner.as_str(), &resolved.enums.get(name)?.decl))
    }

    /// Whether `module` reaches an enum by the bare name `name`.
    pub(super) fn declares_enum(&self, module: &str, name: &str) -> bool {
        self.enum_of(module, name).is_some()
    }

    /// The method of `type_module.type_name` named `name`.
    ///
    /// A type's methods usually live with the type; ADR 0006's orphan rule
    /// lets a conformance put one in the module that declares the trait
    /// instead, so the conformances are searched second.
    pub(super) fn method_of(&self, type_module: &str, type_name: &str, name: &str) -> Option<Key> {
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
    pub(super) fn trait_named(&self, module: &str, name: &str) -> Arc<str> {
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
    pub(super) fn dyn_conversion(&self, module: &str, ty: &Type) -> Option<(Arc<str>, u16)> {
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
    pub(super) fn dispatch_site(&mut self, trait_name: &str, method: &str) -> DispatchId {
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
    pub(super) fn is_host_module(&self, module: &str, name: &str) -> bool {
        self.checked
            .modules
            .get(module)
            .is_some_and(|resolved| resolved.host_uses.contains(name))
            || hosts::module(name).is_some()
    }

    /// The host module an unqualified `use console.println` binds `name` to.
    pub(super) fn host_item(&self, module: &str, name: &str) -> Option<&'a str> {
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
    pub(super) fn imported_module(&self, module: &str, head: &str) -> Option<&'a str> {
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
    pub(super) fn exported_function(&self, owner: &str, name: &str) -> Option<Key> {
        if self.checked.modules.get(owner)?.exported(name) != Some(true) {
            return None;
        }
        self.functions
            .get(&(owner.to_string(), name.to_string()))
            .copied()
    }

    /// The exported struct `owner.name`, when `owner` exports one.
    pub(super) fn exported_struct(&self, owner: &str, name: &str) -> Option<&'a StructDecl> {
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
    pub(super) fn number_lambda(
        &mut self,
        site: LambdaSite<'a>,
        at: (FileId, ExprId),
    ) -> FunctionId {
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
///
/// [`Inst::MakeDyn`]: crate::Inst::MakeDyn
pub(super) fn reject_dyn(ty: &Type, what: &str) -> Result<(), Unsupported> {
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
pub(super) fn dyn_shape(ty: &Type) -> Option<(&str, u16)> {
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
