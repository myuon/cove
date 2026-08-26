//! Name resolution across the units of a module, and across the modules of a
//! package.
//!
//! Resolution produces the flat program the runtime executes and the derived
//! facts (`export` visibility, required capabilities, trait conformances)
//! that tooling reports.
//!
//! ADR 0005 makes a module able to name another module's exported
//! declarations, so resolution is a package-wide pass rather than a
//! per-module one: a `use` is checked against another module's declarations,
//! the module dependency graph must be acyclic, and required capabilities are
//! derived from the whole package's call graph.
//!
//! # What a derived capability set promises
//!
//! ADR 0015: a derived set is a *lower bound*. Function types carry no latent
//! capability set, so a call through a function value, a `dyn Trait`
//! receiver, or a generic parameter's bound is one the call graph cannot
//! follow to a declaration. Rather than pretend otherwise, resolution records
//! *why* it could not — [`FnEntry::open_calls`] — and propagates that along
//! the same edges the capabilities travel. A function that carries no open
//! call has a complete set; one that does has a floor and says so, and the
//! runtime's grant check remains the only thing that decides what a call may
//! actually do.
//!
//! # Conformances
//!
//! An `impl Trait for Type` block is checked here and then flattened: every
//! method it supplies, and every method the trait defaults that it does not
//! override, is recorded as an ordinary method of the type. Dispatch never
//! has to ask where a method came from, and a trait method that collides with
//! an inherent method of the same name is caught by the same duplicate check
//! that catches two inherent methods.
//!
//! The conformance itself is recorded separately, because the set of a
//! trait's implementors is a fact the type checker needs (to check a bound)
//! and tooling needs (to show a type's interface).
//!
//! Either party to a conformance may be imported (ADR 0006's orphan rule
//! names the module that declares the trait or the module that declares the
//! type; ADR 0005 lets a third module name both), so the rule is checked
//! against what the module *declares*, not against what it can see. The two
//! rules together also make a conformance unique without a further check:
//! for both parties' modules to declare the same one, each would have to
//! import the other, which is the cycle ADR 0005 forbids.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cove_diag::{Diagnostic, Span, Spanned};
use cove_schema::HostSchemas;
use cove_syntax::ast::{
    Block, EnumDecl, Expr, ExprKind, FnDecl, Item, ItemKind, MatchArm, Pattern, PatternKind,
    Receiver, Stmt, StmtKind, StrPart, StructDecl, TraitDecl, TraitMethod, TypeAlias,
};

use crate::capability::{Capability, OpenCall};
use crate::package::Package;

/// A declaration that belongs to a module, with the facts derived from it.
#[derive(Debug)]
pub struct FnEntry {
    pub decl: Arc<FnDecl>,
    pub exported: bool,
    /// `test fn`: a declaration only the test runner calls.
    ///
    /// A test is module-private by construction — `export` and `test` cannot
    /// both apply — so it sees its module's private declarations, and its
    /// required capabilities are derived from its call graph exactly as any
    /// other function's are.
    pub is_test: bool,
    pub doc: Option<String>,
    /// The type this function is a method of, when it came from an `impl`.
    pub receiver_type: Option<String>,
    /// The trait whose default body this method runs, when the conformance
    /// did not supply one of its own.
    ///
    /// Such a method's body belongs to the trait, not to this type, so it is
    /// checked once where the trait declares it rather than once per
    /// conformance.
    pub from_trait_default: Option<String>,
    /// Capabilities used directly in this function's body.
    pub direct_capabilities: BTreeSet<Capability>,
    /// Capabilities this function requires, including those reached through
    /// calls to other declarations of the package — its own module's, and
    /// any module it imports from.
    ///
    /// A call through a field access (`receiver.method(...)`) whose receiver
    /// is not a bare reference to a struct or enum visible where the call is
    /// written is resolved to *every* method sharing that name in this
    /// module and in every module it imports from. There is no static type
    /// checker yet to narrow the receiver's actual type, so this is a
    /// deliberate over-approximation: it can report a capability a function
    /// does not really need, but never omits one it does. Static type
    /// checking would let this be exact.
    ///
    /// Module imports made no part of that precise. They widened it: an
    /// unknown receiver used to be able to reach only the module's own
    /// methods, and can now reach the methods of every module reachable
    /// through its imports, because that is where a value it did not declare
    /// can come from.
    /// What they did narrow is the *other* direction — a call that leaves
    /// the module is now followed rather than lost, so a capability reached
    /// through an imported helper is reported instead of missed.
    ///
    /// This set is a *lower bound* when [`FnEntry::open_calls`] is not empty;
    /// see ADR 0015.
    pub required_capabilities: BTreeSet<Capability>,
    /// The indirect calls written in this function's own body: calls the
    /// call graph cannot follow, so whatever they reach is not in
    /// `direct_capabilities`.
    pub direct_open_calls: BTreeSet<OpenCall>,
    /// Why `required_capabilities` is a lower bound rather than the whole of
    /// what calling this function can reach — empty when it is the whole of
    /// it.
    ///
    /// This is `direct_open_calls` propagated over the same call graph the
    /// capabilities travel: a caller of a capability-open declaration is
    /// capability-open too, because the requirement it cannot see is one its
    /// own callers cannot see either.
    pub open_calls: BTreeSet<OpenCall>,
}

impl FnEntry {
    /// Whether [`FnEntry::required_capabilities`] is a lower bound: this
    /// function, or something it calls, makes a call the call graph cannot
    /// follow.
    ///
    /// Every report that shows a derived capability set asks this, so that
    /// an incomplete set is never shown as though it were complete.
    pub fn is_capability_open(&self) -> bool {
        !self.open_calls.is_empty()
    }
}

#[derive(Debug)]
pub struct StructEntry {
    pub decl: Arc<StructDecl>,
    pub exported: bool,
    /// `export opaque struct`: the export carries the type's name and its
    /// exported methods, and no way to build or read one.
    ///
    /// The flag is recorded here rather than derived at each use because
    /// every consumer asks the same question of it — the type checker, which
    /// refuses a cross-module construction or field access, and `cove
    /// outline` and `cove api snapshot`, which leave the representation out
    /// of what they publish.
    pub opaque: bool,
    pub doc: Option<String>,
}

#[derive(Debug)]
pub struct EnumEntry {
    pub decl: Arc<EnumDecl>,
    pub exported: bool,
    pub doc: Option<String>,
}

/// A trait a module declares.
#[derive(Debug)]
pub struct TraitEntry {
    pub decl: Arc<TraitDecl>,
    pub exported: bool,
    pub doc: Option<String>,
}

impl TraitEntry {
    /// The method of this trait named `name`, if it declares one.
    pub fn method(&self, name: &str) -> Option<&TraitMethod> {
        self.decl.methods.iter().find(|m| m.name.node == name)
    }
}

/// One `impl Trait for Type` block: the fact that `type_name` conforms to
/// `trait_name`, and how.
///
/// Conformance is explicit, so this is the complete set of implementors a
/// trait has, which is what makes a bound checkable and a `dyn Trait` value's
/// implementation findable.
#[derive(Clone, Debug)]
pub struct Conformance {
    pub trait_name: String,
    pub type_name: String,
    /// The module that declares the trait, and the module that declares the
    /// type. Either may be this module or one it imports from — the orphan
    /// rule only requires that one of them *is* this module — so a
    /// conformance names both parties by the module they belong to.
    pub trait_module: String,
    pub type_module: String,
    /// Every method the conformance supplies, whether written in the block or
    /// inherited from the trait's default body.
    pub methods: BTreeSet<String>,
    /// The `impl Trait for Type` header, for a diagnostic to point at.
    pub span: Span,
}

#[derive(Debug)]
pub struct AliasEntry {
    pub decl: Arc<TypeAlias>,
    pub exported: bool,
    pub doc: Option<String>,
}

/// Everything one module declares.
#[derive(Debug, Default)]
pub struct ResolvedModule {
    pub name: String,
    /// Free functions, keyed by name.
    pub functions: BTreeMap<String, FnEntry>,
    /// Methods and associated functions, keyed by `(type name, function name)`.
    pub methods: BTreeMap<(String, String), FnEntry>,
    pub structs: BTreeMap<String, StructEntry>,
    pub enums: BTreeMap<String, EnumEntry>,
    pub traits: BTreeMap<String, TraitEntry>,
    /// Every declared conformance, keyed by `(trait name, type name)`.
    pub conformances: BTreeMap<(String, String), Conformance>,
    pub aliases: BTreeMap<String, AliasEntry>,
    /// Host modules named by `use`, such as `console` from `use console.println`.
    pub host_uses: BTreeSet<String>,
    /// Names imported unqualified by `use`, such as `println` -> `console`.
    pub host_items: BTreeMap<String, String>,
    /// Declarations imported from another module of the package, mapping the
    /// name they are visible under — the `use` path's last segment, which is
    /// the declaration's own name — to the module that declares them.
    ///
    /// Only exported declarations ever appear here; a `use` naming a
    /// module-private one is rejected.
    pub imports: BTreeMap<String, String>,
    /// Modules imported whole, mapping the name they are visible under — the
    /// `use` path's last segment — to the full module name, so their exports
    /// can be reached qualified as `booking.createBooking`.
    pub module_imports: BTreeMap<String, String>,
}

impl ResolvedModule {
    /// The module that declares `name` as this module sees it: itself when it
    /// declares `name`, and the module `name` was imported from otherwise.
    ///
    /// A local declaration wins, which is only reachable when a conflicting
    /// import was already reported: [`resolve`] refuses a `use` that binds a
    /// name this module also declares.
    pub fn owner_of<'a>(&'a self, name: &str) -> Option<&'a str> {
        if self.functions.contains_key(name)
            || self.structs.contains_key(name)
            || self.enums.contains_key(name)
            || self.traits.contains_key(name)
            || self.aliases.contains_key(name)
        {
            return Some(&self.name);
        }
        self.imports.get(name).map(String::as_str)
    }

    /// Whether this module's declaration of `name` is exported, or `None`
    /// when it declares no `name`.
    ///
    /// A method is not a declaration in this sense: it is reached through
    /// its type, so the type's visibility is what governs it.
    pub fn exported(&self, name: &str) -> Option<bool> {
        if let Some(entry) = self.functions.get(name) {
            return Some(entry.exported);
        }
        if let Some(entry) = self.structs.get(name) {
            return Some(entry.exported);
        }
        if let Some(entry) = self.enums.get(name) {
            return Some(entry.exported);
        }
        if let Some(entry) = self.traits.get(name) {
            return Some(entry.exported);
        }
        self.aliases.get(name).map(|entry| entry.exported)
    }

    /// Every name this module exports, in name order.
    pub fn exports(&self) -> Vec<String> {
        let functions = self
            .functions
            .iter()
            .filter(|(_, entry)| entry.exported)
            .map(|(name, _)| name);
        let structs = self
            .structs
            .iter()
            .filter(|(_, entry)| entry.exported)
            .map(|(name, _)| name);
        let enums = self
            .enums
            .iter()
            .filter(|(_, entry)| entry.exported)
            .map(|(name, _)| name);
        let traits = self
            .traits
            .iter()
            .filter(|(_, entry)| entry.exported)
            .map(|(name, _)| name);
        let aliases = self
            .aliases
            .iter()
            .filter(|(_, entry)| entry.exported)
            .map(|(name, _)| name);
        let mut names: Vec<String> = functions
            .chain(structs)
            .chain(enums)
            .chain(traits)
            .chain(aliases)
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Every module this one imports from, whether by declaration or whole.
    pub fn dependencies(&self) -> BTreeSet<&str> {
        self.imports
            .values()
            .chain(self.module_imports.values())
            .map(String::as_str)
            .collect()
    }
}

/// A resolved package, ready to run or inspect.
#[derive(Debug, Default)]
pub struct Program {
    pub modules: BTreeMap<String, ResolvedModule>,
    /// Every diagnostic that does not stop the package: the resolver's own
    /// warnings, such as a missing doc comment on an exported declaration,
    /// and the type checker's warnings *and notes*.
    ///
    /// It is called `notices` rather than `warnings` because it holds two
    /// severities and they ask for different things. A warning is a doubt,
    /// and `cove check --deny-warnings` refuses a package that has one. A
    /// note is not: it is the compiler naming something it deliberately did
    /// not prove — a Host API result a schema declared `Any`, a variadic
    /// operation used as a value — which no strictness setting can turn into
    /// a proof. Every consumer therefore filters on the exact
    /// [`cove_diag::Severity`] rather than on this field's length.
    ///
    /// It never holds an [`cove_diag::Severity::Error`]: an error is
    /// returned as `Err` and the package does not resolve.
    pub notices: Vec<Diagnostic>,
    /// The package's call graph: for each declaration, every declaration it
    /// may call, and how precisely the call site named it.
    ///
    /// This is the graph [`FnEntry::required_capabilities`] is the fixed
    /// point over. It is kept rather than discarded because reachability
    /// between declarations is a derived fact in its own right: it is what
    /// answers which declarations a change can affect.
    pub call_graph: BTreeMap<Node, BTreeMap<Node, CallPrecision>>,
}

impl Program {
    /// Looks up a fully qualified entry such as `hello.main`.
    pub fn lookup_fn(&self, module: &str, name: &str) -> Option<&FnEntry> {
        self.modules.get(module)?.functions.get(name)
    }

    /// Every `test fn` the package declares, in module then name order.
    ///
    /// This is what `cove test` runs. A test is an ordinary declaration of
    /// its module, so its required capabilities are already derived and its
    /// body already checked; the runner only has to find it.
    pub fn tests(&self) -> Vec<DeclaredTest<'_>> {
        let mut found = Vec::new();
        for (module, resolved) in &self.modules {
            for (name, entry) in &resolved.functions {
                if entry.is_test {
                    found.push(DeclaredTest {
                        module: module.as_str(),
                        name: name.as_str(),
                        entry,
                    });
                }
            }
        }
        found
    }

    /// Every conformance declared for the type `type_module.type_name`,
    /// paired with the module whose source declares it, in trait order.
    ///
    /// The declaring module is not always the type's own. ADR 0006's orphan
    /// rule only requires that the module declaring an `impl Trait for Type`
    /// block declares one of the two, so a conformance may be written where
    /// the *trait* is. A type's conformances are therefore a fact about the
    /// package, and asking one module for them under-reports the type's
    /// interface.
    pub fn conformances_of(&self, type_module: &str, type_name: &str) -> Vec<(&str, &Conformance)> {
        let mut found: Vec<(&str, &Conformance)> = Vec::new();
        for (module, resolved) in &self.modules {
            for conformance in resolved.conformances.values() {
                if conformance.type_module == type_module && conformance.type_name == type_name {
                    found.push((module.as_str(), conformance));
                }
            }
        }
        found.sort_by(|(_, a), (_, b)| {
            (&a.trait_module, &a.trait_name).cmp(&(&b.trait_module, &b.trait_name))
        });
        found
    }

    /// Every method of the type `type_module.type_name`, wherever it is
    /// declared, in method-name order.
    ///
    /// A type's methods usually live in the module that declares the type. A
    /// conformance is the exception, for the reason [`Self::conformances_of`]
    /// gives: a method of this type can be declared by any module that
    /// conforms it to a trait of its own.
    ///
    /// One name answers to one method, whichever module declares it:
    /// `check_method_collisions` rejects a package where two modules
    /// declare a method of one name for one type.
    pub fn methods_of(&self, type_module: &str, type_name: &str) -> Vec<DeclaredMethod<'_>> {
        let mut found: BTreeMap<&str, DeclaredMethod<'_>> = BTreeMap::new();
        if let Some(owner) = self.modules.get(type_module) {
            for ((owner_type, method), entry) in &owner.methods {
                if owner_type == type_name {
                    found.insert(
                        method.as_str(),
                        DeclaredMethod {
                            module: owner.name.as_str(),
                            name: method.as_str(),
                            entry,
                        },
                    );
                }
            }
        }
        for (module, conformance) in self.conformances_of(type_module, type_name) {
            let Some(owner) = self.modules.get(module) else {
                continue;
            };
            for method in &conformance.methods {
                let key = (type_name.to_string(), method.clone());
                let Some(entry) = owner.methods.get(&key) else {
                    continue;
                };
                found.insert(
                    method.as_str(),
                    DeclaredMethod {
                        module: owner.name.as_str(),
                        name: method.as_str(),
                        entry,
                    },
                );
            }
        }
        found.into_values().collect()
    }
}

/// One `test fn`, and the module that declares it.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredTest<'a> {
    /// The module the test belongs to, whose private declarations it sees.
    pub module: &'a str,
    /// The test's own name.
    pub name: &'a str,
    /// The declaration itself, with the capabilities its call graph requires.
    pub entry: &'a FnEntry,
}

impl DeclaredTest<'_> {
    /// The name the runner reports and `--filter` matches, such as
    /// `text.countsWords`.
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.module, self.name)
    }
}

/// One method of a type, and the module whose source declares it.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredMethod<'a> {
    /// The module that declares this method: the type's own, or one that
    /// conforms the type to a trait of its own.
    pub module: &'a str,
    /// The method's name.
    pub name: &'a str,
    /// The method itself, with the facts derived from it.
    pub entry: &'a FnEntry,
}

/// Resolves every module of `package` into the flat program the runtime
/// executes.
///
/// Because a module may name another module's exported declarations, this is
/// a package-wide pass rather than a per-module one:
///
/// 1. each module's *surface* — what it declares, and whether each
///    declaration is exported — is collected, since a `use` in one module is
///    answered by another module's declarations;
/// 2. every `use` is resolved against the package's modules first and the
///    host registry second (ADR 0005);
/// 3. the module dependency graph is checked for cycles, which ADR 0005
///    forbids;
/// 4. each module's own declarations are merged across its units;
/// 5. required capabilities, and the reasons they are only a lower bound,
///    are derived as a fixed point over the *package's* call graph, so a
///    function reaching a Host API through an imported helper reports it and
///    a function reaching an indirect call through one says so;
/// 6. every body is checked against everything now known, including enums
///    reached through an import.
pub fn resolve(package: &Package) -> Result<Program, Vec<Diagnostic>> {
    resolve_with(package, &HostSchemas::new())
}

/// Resolves `package` against `schemas`, the host modules this compilation
/// may name.
///
/// This is [`resolve`] with the one thing an embedder can change: the set of
/// Host API descriptions the resolver reads. A module in `schemas` is a host
/// module here in every sense a shipped one is -- it may not be shadowed by
/// a package module, a `use` of it is not warned about, and a call into it
/// requires the capability its own table declares.
pub fn resolve_with(package: &Package, schemas: &HostSchemas) -> Result<Program, Vec<Diagnostic>> {
    let mut program = Program::default();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let surfaces: BTreeMap<&str, Surface> = package
        .modules
        .iter()
        .map(|(name, module)| (name.as_str(), Surface::of(module)))
        .collect();

    let opaque_fields = OpaqueFields::of(package);

    let mut call_sites: BTreeMap<Node, Vec<CallShape>> = BTreeMap::new();
    let mut edges: Vec<ImportEdge> = Vec::new();
    // Modules a warning has already been issued for, so a package where
    // several `use`s (in one module or several) name the same undescribed
    // host module is told once rather than once per `use`. `package.modules`
    // is a `BTreeMap`, so this loop visits modules in name order and the
    // warning always lands on the first `use` of the alphabetically first
    // module that names it, which is what makes the outcome deterministic
    // enough to test.
    let mut warned_hosts: BTreeSet<String> = BTreeSet::new();
    for (name, module) in &package.modules {
        let uses = resolve_uses(
            name,
            module,
            &surfaces,
            schemas,
            &mut errors,
            &mut warnings,
            &mut warned_hosts,
        );
        edges.extend(uses.edges.iter().cloned());
        let (resolved, calls) = resolve_module(
            name,
            module,
            uses,
            &surfaces,
            &opaque_fields,
            schemas,
            &mut errors,
            &mut warnings,
        );
        for (key, shapes) in calls {
            call_sites.insert((name.clone(), key), shapes);
        }
        program.modules.insert(name.clone(), resolved);
    }

    check_import_cycles(&edges, &mut errors);
    check_method_collisions(&program, &mut errors);
    let (call_graph, unresolved) = package_call_graph(&program, &call_sites);
    merge_open_calls(&mut program, &unresolved);
    propagate_capabilities(&mut program, &call_graph);
    program.call_graph = call_graph;
    check_bodies(&program, schemas, &mut errors, &mut warnings);

    if errors.is_empty() {
        program.notices = warnings;
        Ok(program)
    } else {
        errors.extend(warnings);
        Err(errors)
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_module(
    name: &str,
    module: &crate::package::Module,
    uses: ModuleUses,
    surfaces: &BTreeMap<&str, Surface>,
    opaque_fields: &OpaqueFields,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) -> (ResolvedModule, BTreeMap<FnKey, Vec<CallShape>>) {
    let mut resolved = ResolvedModule {
        name: name.to_string(),
        host_uses: uses.host_uses.clone(),
        host_items: uses.host_items.clone(),
        imports: uses.imports.clone(),
        module_imports: uses.module_imports.clone(),
        ..ResolvedModule::default()
    };

    // Pass 2: top-level declarations, merged across every unit of the module.
    let mut fn_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut struct_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut enum_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut alias_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut trait_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut pending_impls: Vec<(&cove_syntax::ast::ImplBlock, Span)> = Vec::new();
    // Raw call sites found in each declaration's body, resolved to call-graph
    // edges once every declaration in the module is known (pass 4).
    let mut call_sites: BTreeMap<FnKey, Vec<CallShape>> = BTreeMap::new();

    for unit in &module.units {
        for item in &unit.ast.items {
            match &item.kind {
                ItemKind::Fn(decl) => {
                    if let Some(existing) =
                        duplicate(&mut fn_spans, &decl.name.node, decl.name.span)
                    {
                        errors.push(duplicate_declaration(
                            name,
                            &decl.name.node,
                            decl.name.span,
                            existing,
                        ));
                        continue;
                    }
                    missing_doc(warnings, item, &decl.name.node, decl.name.span);
                    let (capabilities, calls, open) = analyze_body(
                        decl,
                        &resolved.host_uses,
                        &resolved.host_items,
                        opaque_fields,
                        schemas,
                    );
                    call_sites.insert(FnKey::Fn(decl.name.node.clone()), calls);
                    resolved.functions.insert(
                        decl.name.node.clone(),
                        FnEntry {
                            decl: Arc::new(decl.clone()),
                            exported: item.exported,
                            is_test: item.is_test,
                            doc: item.doc.clone(),
                            receiver_type: None,
                            from_trait_default: None,
                            direct_capabilities: capabilities,
                            required_capabilities: BTreeSet::new(),
                            direct_open_calls: open,
                            open_calls: BTreeSet::new(),
                        },
                    );
                }
                ItemKind::Struct(decl) => {
                    if let Some(existing) =
                        duplicate(&mut struct_spans, &decl.name.node, decl.name.span)
                    {
                        errors.push(duplicate_declaration(
                            name,
                            &decl.name.node,
                            decl.name.span,
                            existing,
                        ));
                        continue;
                    }
                    missing_doc(warnings, item, &decl.name.node, decl.name.span);
                    resolved.structs.insert(
                        decl.name.node.clone(),
                        StructEntry {
                            decl: Arc::new(decl.clone()),
                            exported: item.exported,
                            opaque: item.is_opaque,
                            doc: item.doc.clone(),
                        },
                    );
                }
                ItemKind::Enum(decl) => {
                    if let Some(existing) =
                        duplicate(&mut enum_spans, &decl.name.node, decl.name.span)
                    {
                        errors.push(duplicate_declaration(
                            name,
                            &decl.name.node,
                            decl.name.span,
                            existing,
                        ));
                        continue;
                    }
                    missing_doc(warnings, item, &decl.name.node, decl.name.span);
                    resolved.enums.insert(
                        decl.name.node.clone(),
                        EnumEntry {
                            decl: Arc::new(decl.clone()),
                            exported: item.exported,
                            doc: item.doc.clone(),
                        },
                    );
                }
                ItemKind::Trait(decl) => {
                    if let Some(existing) =
                        duplicate(&mut trait_spans, &decl.name.node, decl.name.span)
                    {
                        errors.push(duplicate_declaration(
                            name,
                            &decl.name.node,
                            decl.name.span,
                            existing,
                        ));
                        continue;
                    }
                    missing_doc(warnings, item, &decl.name.node, decl.name.span);
                    // A trait's methods are part of the interface the trait
                    // publishes, so an exported trait documents each of them.
                    if item.exported {
                        for method in &decl.methods {
                            if method.doc.is_none() {
                                warnings.push(undocumented(
                                    &format!("{}.{}", decl.name.node, method.name.node),
                                    method.name.span,
                                ));
                            }
                        }
                    }
                    resolved.traits.insert(
                        decl.name.node.clone(),
                        TraitEntry {
                            decl: Arc::new(decl.clone()),
                            exported: item.exported,
                            doc: item.doc.clone(),
                        },
                    );
                }
                ItemKind::TypeAlias(decl) => {
                    if let Some(existing) =
                        duplicate(&mut alias_spans, &decl.name.node, decl.name.span)
                    {
                        errors.push(duplicate_declaration(
                            name,
                            &decl.name.node,
                            decl.name.span,
                            existing,
                        ));
                        continue;
                    }
                    missing_doc(warnings, item, &decl.name.node, decl.name.span);
                    resolved.aliases.insert(
                        decl.name.node.clone(),
                        AliasEntry {
                            decl: Arc::new(decl.clone()),
                            exported: item.exported,
                            doc: item.doc.clone(),
                        },
                    );
                }
                ItemKind::Impl(impl_block) => {
                    pending_impls.push((impl_block, item.span));
                }
            }
        }
    }

    // Pass 3: `impl` blocks, once every struct, enum, and trait in the module
    // is known.
    let mut method_spans: BTreeMap<(String, String), Span> = BTreeMap::new();
    for (impl_block, _impl_span) in pending_impls {
        let type_name = impl_block.type_name.node.clone();
        let declares_type =
            resolved.structs.contains_key(&type_name) || resolved.enums.contains_key(&type_name);
        // A conformance may name an imported type, so the type an `impl`
        // extends is looked up the way every other name is: this module
        // first, then what it imported.
        let type_module = declaring_module_of(surfaces, name, &uses, &type_name, DeclKind::Type);

        // The orphan rule: a conformance may only be declared where one of
        // its two parties *is declared*. Imports widen which names an `impl`
        // can spell, but not this: a third module that imports both a trait
        // and a type still may not make one conform to the other, which is
        // the whole point of the rule.
        if let Some(trait_ident) = &impl_block.trait_name {
            let trait_name = trait_ident.node.clone();
            let declares_trait = resolved.traits.contains_key(&trait_name);
            let trait_module =
                declaring_module_of(surfaces, name, &uses, &trait_name, DeclKind::Trait);
            if !declares_trait && !declares_type {
                errors.push(orphan_conformance(
                    name,
                    &trait_name,
                    &type_name,
                    trait_ident.span.to(impl_block.type_name.span),
                ));
                continue;
            }
            // `Snapshot` is a builtin trait (see `builtin_snapshot_trait`): it
            // belongs to no module, so it never has a `trait_module`, and the
            // check below would otherwise reject it as unknown.
            if trait_module.is_none() && trait_name != BUILTIN_SNAPSHOT_TRAIT {
                errors.push(
                    Diagnostic::error(
                        "cove::resolve::unknown_trait",
                        format!("`{trait_name}` names a trait module `{name}` can see"),
                    )
                    .at(trait_ident.span)
                    .rule("A conformance names a trait the module declares or imports.")
                    .help(format!(
                        "Declare `trait {trait_name}` in this module, `use <module>.{trait_name}` to import it, or fix the name."
                    )),
                );
                continue;
            }
        }

        if type_module.is_none() {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::unknown_impl_type",
                    format!("`impl {type_name}` names a type module `{name}` can see"),
                )
                .at(impl_block.type_name.span)
                .rule("An `impl` block extends a struct or enum the module declares, or one it imports as part of a conformance.")
                .help(format!(
                    "Declare `struct {type_name}` or `enum {type_name}` in this module, `use <module>.{type_name}` to import it, or fix the name."
                )),
            );
            continue;
        }
        let type_module = type_module.expect("checked just above").to_string();

        // An inherent `impl` extends only a type this module declares: it is
        // not a conformance, so the orphan rule has nothing to say about it,
        // and adding methods to another module's type from outside would be
        // exactly what that rule forbids.
        if impl_block.trait_name.is_none() && !declares_type {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::foreign_inherent_impl",
                    format!(
                        "`impl {type_name}` adds methods to a type module `{type_module}` declares"
                    ),
                )
                .at(impl_block.type_name.span)
                .rule("An `impl` block with no trait extends a type its own module declares; a method for another module's type belongs to a trait, so that the conformance is a fact both modules can see.")
                .help(format!(
                    "move this block to module `{type_module}`, or declare a trait here and write `impl <Trait> for {type_name}`"
                )),
            );
            continue;
        }

        if let Some(trait_ident) = &impl_block.trait_name {
            let header = trait_ident.span.to(impl_block.type_name.span);
            let key = (trait_ident.node.clone(), type_name.clone());
            if let Some(existing) = resolved.conformances.get(&key) {
                errors.push(
                    Diagnostic::error(
                        "cove::resolve::duplicate_conformance",
                        format!("`{type_name}` already conforms to `{}`", trait_ident.node),
                    )
                    .at(header)
                    .label(existing.span, "the first conformance is declared here")
                    .rule("A type conforms to a trait exactly once; conformance is explicit, so two `impl Trait for Type` blocks would leave no way to choose.")
                    .help("Merge the two blocks into one."),
                );
                continue;
            }
            let (trait_module, trait_decl) = match declaring_module_of(
                surfaces,
                name,
                &uses,
                &trait_ident.node,
                DeclKind::Trait,
            ) {
                Some(module) => (
                    module.to_string(),
                    surfaces[module].traits[&trait_ident.node].clone(),
                ),
                // Only `Snapshot` reaches here: every other trait name was
                // already rejected above. It belongs to no module, so it
                // conforms wherever the type itself does.
                None => (type_module.clone(), builtin_snapshot_trait(header)),
            };
            check_conformance(
                &mut resolved,
                name,
                impl_block,
                Conformance {
                    trait_name: trait_ident.node.clone(),
                    type_name: type_name.clone(),
                    trait_module,
                    type_module,
                    methods: BTreeSet::new(),
                    span: header,
                },
                trait_decl,
                &mut method_spans,
                &mut call_sites,
                opaque_fields,
                schemas,
                errors,
            );
            continue;
        }

        for inner in &impl_block.items {
            match &inner.kind {
                ItemKind::Fn(decl) => {
                    let key = (type_name.clone(), decl.name.node.clone());
                    if let Some(existing_span) = method_spans.get(&key) {
                        errors.push(
                            Diagnostic::error(
                                "cove::resolve::duplicate_declaration",
                                format!(
                                    "`{type_name}.{}` is declared twice in module `{name}`",
                                    decl.name.node
                                ),
                            )
                            .at(decl.name.span)
                            .label(
                                *existing_span,
                                format!("`{}` first declared here", decl.name.node),
                            )
                            .rule(
                                "Each method name may be declared once per type across a module's implementation units.",
                            ),
                        );
                        continue;
                    }
                    method_spans.insert(key.clone(), decl.name.span);
                    missing_doc(warnings, inner, &decl.name.node, decl.name.span);
                    let (capabilities, calls, open) = analyze_body(
                        decl,
                        &resolved.host_uses,
                        &resolved.host_items,
                        opaque_fields,
                        schemas,
                    );
                    call_sites.insert(
                        FnKey::Method(type_name.clone(), decl.name.node.clone()),
                        calls,
                    );
                    resolved.methods.insert(
                        key,
                        FnEntry {
                            decl: Arc::new(decl.clone()),
                            exported: inner.exported,
                            // A method is reached through its type, never
                            // through the test runner; the parser rejects a
                            // `test fn` written in an `impl` block.
                            is_test: false,
                            doc: inner.doc.clone(),
                            receiver_type: Some(type_name.clone()),
                            from_trait_default: None,
                            direct_capabilities: capabilities,
                            required_capabilities: BTreeSet::new(),
                            direct_open_calls: open,
                            open_calls: BTreeSet::new(),
                        },
                    );
                }
                _ => {
                    errors.push(
                        Diagnostic::error(
                            "cove::resolve::invalid_impl_item",
                            "only `fn` declarations are allowed inside an `impl` block",
                        )
                        .at(inner.span)
                        .rule("An `impl` block may only contain method declarations."),
                    );
                }
            }
        }
    }

    // The call sites found in passes 2 and 3 are resolved to call-graph
    // edges by [`package_call_graph`], once every module is resolved: a call
    // may reach an imported declaration, so the graph is the package's.
    (resolved, call_sites)
}

// ------------------------------------------------------------------ imports

/// The host modules a package may name without declaring them.
///
/// This is [`HostSchemas`], the one description of the host modules this
/// compilation can see, rather than a list kept here in step with it: the
/// compiler cannot ask the runtime, which depends on it, but it can read the
/// schema both of them do. It used to be a hand-written array with a
/// cross-crate test to catch the drift, written after `http` had already
/// drifted out of it and let a package module shadow the host module in
/// silence.
///
/// It is only consulted to refuse a package module that would shadow a host
/// module. A `use` naming a module that is not here is still accepted, since
/// a host may register any module it likes. Such a `use` warns instead: a
/// module no schema describes is checked by nothing until the run reaches the
/// boundary.
pub fn host_modules(schemas: &HostSchemas) -> impl Iterator<Item = &'static str> + '_ {
    schemas.names()
}

/// What one module offers a `use` in another: every top-level declaration it
/// makes, what kind it is, whether it is exported, and where it was written.
///
/// This is collected before any module is resolved, because a `use` in one
/// module is answered by another module's declarations, and because an
/// `impl Trait for Type` may name an imported trait whose declaration has to
/// be read while the importing module is still being resolved.
///
/// Methods do not appear: a method is reached through its type, so importing
/// the type is what makes it visible.
#[derive(Debug, Default)]
struct Surface {
    declarations: BTreeMap<String, Declared>,
    /// The traits this module declares, whose method lists an
    /// `impl Trait for Type` in another module has to read.
    traits: BTreeMap<String, Arc<TraitDecl>>,
}

#[derive(Debug)]
struct Declared {
    kind: DeclKind,
    exported: bool,
    span: Span,
}

/// What a declaration is, to the extent a `use` and an `impl` need to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeclKind {
    Function,
    /// A struct or an enum: the two kinds an `impl` block may extend.
    Type,
    Trait,
    Alias,
}

impl Surface {
    fn of(module: &crate::package::Module) -> Surface {
        let mut declarations: BTreeMap<String, Declared> = BTreeMap::new();
        let mut traits: BTreeMap<String, Arc<TraitDecl>> = BTreeMap::new();
        for unit in &module.units {
            for item in &unit.ast.items {
                if let ItemKind::Trait(decl) = &item.kind {
                    traits
                        .entry(decl.name.node.clone())
                        .or_insert_with(|| Arc::new(decl.clone()));
                }
                let (name, kind) = match &item.kind {
                    ItemKind::Fn(decl) => (&decl.name, DeclKind::Function),
                    ItemKind::Struct(decl) => (&decl.name, DeclKind::Type),
                    ItemKind::Enum(decl) => (&decl.name, DeclKind::Type),
                    ItemKind::Trait(decl) => (&decl.name, DeclKind::Trait),
                    ItemKind::TypeAlias(decl) => (&decl.name, DeclKind::Alias),
                    ItemKind::Impl(_) => continue,
                };
                declarations.entry(name.node.clone()).or_insert(Declared {
                    kind,
                    exported: item.exported,
                    span: name.span,
                });
            }
        }
        Surface {
            declarations,
            traits,
        }
    }

    /// Whether this module declares `name` as `kind`.
    fn declares(&self, name: &str, kind: DeclKind) -> bool {
        self.declarations
            .get(name)
            .is_some_and(|declared| declared.kind == kind)
    }
}

/// The declaration a name in module `module` refers to, when some module of
/// the package declares it as `kind`: the module that declares it, and the
/// name itself.
///
/// A module's own declaration answers first, and an import answers second —
/// the same order every other lookup uses. A `use` cannot bind a name the
/// module declares, so at most one of the two ever applies.
fn declaring_module_of<'a>(
    surfaces: &'a BTreeMap<&'a str, Surface>,
    module: &'a str,
    uses: &'a ModuleUses,
    name: &str,
    kind: DeclKind,
) -> Option<&'a str> {
    if surfaces
        .get(module)
        .is_some_and(|surface| surface.declares(name, kind))
    {
        return Some(module);
    }
    let owner = uses.imports.get(name)?.as_str();
    surfaces
        .get(owner)
        .is_some_and(|surface| surface.declares(name, kind))
        .then_some(owner)
}

/// One module's resolved `use` declarations.
#[derive(Debug, Default)]
struct ModuleUses {
    imports: BTreeMap<String, String>,
    module_imports: BTreeMap<String, String>,
    host_uses: BTreeSet<String>,
    host_items: BTreeMap<String, String>,
    /// One edge per `use` that names a module of this package, for the
    /// cycle check.
    edges: Vec<ImportEdge>,
}

/// A module dependency, with the `use` that created it so a cycle can point
/// at the line that closes it.
#[derive(Clone, Debug)]
struct ImportEdge {
    from: String,
    to: String,
    span: Span,
}

/// What one `use` binds in the module that writes it, for conflict reports.
#[derive(Clone, Debug)]
enum Bound {
    /// `use console.println` binds `println` to a host module.
    HostItem(String),
    /// `use booking.create` binds `create` to a module's declaration.
    Item(String),
    /// `use booking` binds `booking` to a whole module.
    Module(String),
}

impl Bound {
    fn describe(&self) -> String {
        match self {
            Bound::HostItem(host) => format!("the host module `{host}`"),
            Bound::Item(module) => format!("module `{module}`"),
            Bound::Module(module) => format!("the module `{module}`"),
        }
    }
}

/// Resolves every `use` of one module.
///
/// ADR 0005: a dotted path resolves against the package's modules first and
/// the host registry second, so a package's own structure does not change
/// meaning because a host gained an operation. Concretely, for a path `p`:
///
/// 1. when `p` names a module, it imports that module, whose exports are
///    then reachable qualified;
/// 2. otherwise, when `p` without its last segment names a module, it
///    imports that module's declaration of the last segment, which must be
///    exported;
/// 3. otherwise it is a host path: one segment names a host module, two name
///    a host module and one operation, and anything longer matches nothing
///    and is reported.
///
/// A module that shares a name with a host module is refused rather than
/// preferred, since the host namespace is not the package's to change.
fn resolve_uses(
    name: &str,
    module: &crate::package::Module,
    surfaces: &BTreeMap<&str, Surface>,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
    warned_hosts: &mut BTreeSet<String>,
) -> ModuleUses {
    let mut uses = ModuleUses::default();
    let mut bound: BTreeMap<String, (Bound, Span)> = BTreeMap::new();
    let own = surfaces.get(name);

    for unit in &module.units {
        for use_decl in &unit.ast.uses {
            let segments: Vec<&str> = use_decl.path.iter().map(|i| i.node.as_str()).collect();
            let path = segments.join(".");
            let span = use_decl.span;
            let last = segments.last().expect("a `use` path is never empty");

            if surfaces.contains_key(path.as_str()) {
                if let Some(diagnostic) = shadowed_host(&path, schemas, span) {
                    errors.push(diagnostic);
                    continue;
                }
                if let Some(diagnostic) = ambiguous_module_path(&path, &segments, surfaces, span) {
                    errors.push(diagnostic);
                    continue;
                }
                bind(
                    &mut bound,
                    name,
                    own,
                    last,
                    Bound::Module(path.clone()),
                    span,
                    errors,
                );
                uses.module_imports.insert(last.to_string(), path.clone());
                uses.edges.push(ImportEdge {
                    from: name.to_string(),
                    to: path,
                    span,
                });
                continue;
            }

            if segments.len() >= 2 {
                let owner = segments[..segments.len() - 1].join(".");
                if let Some(surface) = surfaces.get(owner.as_str()) {
                    if let Some(diagnostic) = shadowed_host(&owner, schemas, span) {
                        errors.push(diagnostic);
                        continue;
                    }
                    match surface.declarations.get(*last) {
                        Some(declared) if declared.exported => {
                            bind(
                                &mut bound,
                                name,
                                own,
                                last,
                                Bound::Item(owner.clone()),
                                span,
                                errors,
                            );
                            uses.imports.insert(last.to_string(), owner.clone());
                            uses.edges.push(ImportEdge {
                                from: name.to_string(),
                                to: owner,
                                span,
                            });
                        }
                        Some(declared) => {
                            errors.push(private_declaration(&owner, last, span, declared.span))
                        }
                        None => errors.push(no_such_declaration(&owner, last, surface, span)),
                    }
                    continue;
                }
            }

            match segments.len() {
                1 => {
                    warn_unchecked_host_once(&path, schemas, span, warned_hosts, warnings);
                    uses.host_uses.insert(path);
                }
                2 => {
                    let host = segments[0].to_string();
                    warn_unchecked_host_once(&host, schemas, span, warned_hosts, warnings);
                    uses.host_uses.insert(host.clone());
                    bind(
                        &mut bound,
                        name,
                        own,
                        last,
                        Bound::HostItem(host.clone()),
                        span,
                        errors,
                    );
                    uses.host_items.insert(last.to_string(), host);
                }
                _ => errors.push(unknown_use(&path, &segments, surfaces, span)),
            }
        }
    }

    uses
}

/// Records that `use` binds `name` in the importing module, reporting a name
/// it already binds and a name the importing module declares itself.
///
/// A repeated `use` of the same thing is not a conflict: writing
/// `use booking.create` in two units of one module means the same import
/// twice.
fn bind(
    bound: &mut BTreeMap<String, (Bound, Span)>,
    module: &str,
    own: Option<&Surface>,
    name: &str,
    what: Bound,
    span: Span,
    errors: &mut Vec<Diagnostic>,
) {
    if let Some(declared) = own.and_then(|surface| surface.declarations.get(name)) {
        errors.push(
            Diagnostic::error(
                "cove::resolve::import_conflict",
                format!("`{name}` is imported, but module `{module}` also declares it"),
            )
            .at(span)
            .label(declared.span, format!("`{name}` is declared here"))
            .rule("An imported name and a declared name cannot both mean `name` in one module.")
            .help("rename one of them, or drop the `use` and name the import qualified"),
        );
        return;
    }
    match bound.get(name) {
        Some((existing, existing_span)) if !same_origin(existing, &what) => {
            // Two host modules disagreeing about one unqualified name is the
            // case that predates module imports, and keeps its own code.
            let code = match (existing, &what) {
                (Bound::HostItem(_), Bound::HostItem(_)) => "cove::resolve::ambiguous_use",
                _ => "cove::resolve::import_conflict",
            };
            errors.push(
                Diagnostic::error(
                    code,
                    format!(
                        "`{name}` is imported from both {} and {}",
                        existing.describe(),
                        what.describe()
                    ),
                )
                .at(span)
                .label(
                    *existing_span,
                    format!("first imported from {} here", existing.describe()),
                )
                .rule("A `use` name must resolve to exactly one declaration or host module.")
                .help(format!(
                    "drop one of the two `use` declarations, and name `{name}` qualified where the other meaning is wanted"
                )),
            );
        }
        Some(_) => {}
        None => {
            bound.insert(name.to_string(), (what, span));
        }
    }
}

fn same_origin(a: &Bound, b: &Bound) -> bool {
    match (a, b) {
        (Bound::HostItem(a), Bound::HostItem(b))
        | (Bound::Item(a), Bound::Item(b))
        | (Bound::Module(a), Bound::Module(b)) => a == b,
        _ => false,
    }
}

/// Warns about a `use` of a host module no schema describes.
///
/// Such a module is the one case the checker genuinely cannot answer for: a
/// host may register anything, and until this compilation is handed its
/// table, every call into it is unknown here and checked for the first time
/// at the boundary. That fallback is deliberate and stays, but a program
/// resting on it should say so rather than read like one that checked.
fn unchecked_host_module(module: &str, schemas: &HostSchemas, span: Span) -> Option<Diagnostic> {
    if schemas.module(module).is_some() {
        return None;
    }
    Some(
        Diagnostic::warning(
            "cove::resolve::unchecked_host",
            format!("no Host API schema describes the host module `{module}`, so calls into it are unchecked"),
        )
        .at(span)
        .rule(
            "A Host API call is checked against its module's schema; the checker reads the shipped schemas and any an embedder supplies.",
        )
        .help(format!(
            "if `{module}` is an embedder's module, hand its `ModuleSchema` to the compiler with `Compiler::new().with_host_schema(...)`; otherwise check the spelling"
        )),
    )
}

/// Warns about `module` once per package, on the first `use` that named it.
///
/// A `use` of an undescribed host module says the same thing every time it
/// is written, whether that is twice in one module (`use company` and
/// `use company.employee`) or once in each of several modules, so repeating
/// the warning would only inflate a `cove check --deny-warnings` count
/// without telling the reader anything new. `warned_hosts` is the memory
/// that makes it fire once: shared across every module of the package by the
/// caller, and updated here exactly when a warning is actually produced.
fn warn_unchecked_host_once(
    module: &str,
    schemas: &HostSchemas,
    span: Span,
    warned_hosts: &mut BTreeSet<String>,
    warnings: &mut Vec<Diagnostic>,
) {
    if warned_hosts.contains(module) {
        return;
    }
    if let Some(warning) = unchecked_host_module(module, schemas, span) {
        warned_hosts.insert(module.to_string());
        warnings.push(warning);
    }
}

/// Refuses a module that shares its name with a host module.
///
/// Modules resolve first, so such a module would silently make the host
/// module unreachable for the whole package.
fn shadowed_host(module: &str, schemas: &HostSchemas, span: Span) -> Option<Diagnostic> {
    if !host_modules(schemas).any(|host| host == module) {
        return None;
    }
    Some(
        Diagnostic::error(
            "cove::resolve::module_shadows_host",
            format!("module `{module}` has the same name as the host module `{module}`"),
        )
        .at(span)
        .rule(
            "`use` resolves against the package's modules first, so a module named after a host module hides it.",
        )
        .help(format!(
            "rename the `{module}` module; the host namespace is not this package's to change"
        )),
    )
}

/// Refuses a path that names a module *and* an exported declaration of the
/// module one segment shorter, which ADR 0005 does not settle.
fn ambiguous_module_path(
    path: &str,
    segments: &[&str],
    surfaces: &BTreeMap<&str, Surface>,
    span: Span,
) -> Option<Diagnostic> {
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[..segments.len() - 1].join(".");
    let last = segments[segments.len() - 1];
    let declared = surfaces
        .get(owner.as_str())?
        .declarations
        .get(last)
        .filter(|declared| declared.exported)?;
    Some(
        Diagnostic::error(
            "cove::resolve::ambiguous_use",
            format!("`use {path}` names both the module `{path}` and `{last}`, exported by module `{owner}`"),
        )
        .at(span)
        .label(declared.span, format!("`{last}` is declared here"))
        .rule("A `use` path must have exactly one meaning.")
        .help(format!(
            "rename the `{path}` module or `{owner}.{last}`, so the path names one of them"
        )),
    )
}

fn private_declaration(module: &str, name: &str, span: Span, declared: Span) -> Diagnostic {
    Diagnostic::error(
        "cove::resolve::private_declaration",
        format!("`{name}` is declared by module `{module}`, but is not exported"),
    )
    .at(span)
    .label(
        declared,
        format!("`{name}` is declared here, without `export`"),
    )
    .rule("An `export` declaration is public; other declarations are module-private.")
    .help(format!(
        "write `export` on `{name}` in module `{module}`, or import something else"
    ))
}

fn no_such_declaration(module: &str, name: &str, surface: &Surface, span: Span) -> Diagnostic {
    let exported: Vec<String> = surface
        .declarations
        .iter()
        .filter(|(_, declared)| declared.exported)
        .map(|(name, _)| name.clone())
        .collect();
    Diagnostic::error(
        "cove::resolve::unknown_use",
        format!("module `{module}` declares no `{name}`, and `{module}` is not a host module"),
    )
    .at(span)
    .rule("`use` names a module of this package, one of its exported declarations, a host module, or one host operation.")
    .help(if exported.is_empty() {
        format!("module `{module}` exports nothing; write `export` on the declaration to import")
    } else {
        format!("module `{module}` exports {}", list_backticked(&exported))
    })
}

fn unknown_use(
    path: &str,
    segments: &[&str],
    surfaces: &BTreeMap<&str, Surface>,
    span: Span,
) -> Diagnostic {
    let owner = segments[..segments.len() - 1].join(".");
    let modules: Vec<String> = surfaces.keys().map(|name| name.to_string()).collect();
    Diagnostic::error(
        "cove::resolve::unknown_use",
        format!("`use {path}` names neither a module of this package nor a host module"),
    )
    .at(span)
    .rule("`use` resolves against the package's modules first and the host registry second.")
    .help(format!(
        "there is no module `{path}` or `{owner}`; this package declares {}, and a host path names a module (`use console`) or one operation (`use console.println`)",
        list_backticked(&modules)
    ))
}

/// Rejects one type having two methods of one name, when they were declared
/// in different modules.
///
/// A conformance may be declared where the trait is, for a type declared
/// elsewhere, so a type's methods no longer all come from one module — and
/// the per-module duplicate check cannot see the other one. Two candidates
/// with no rule to choose between them is the same mistake wherever the two
/// are written: `impl Trait for Type` in the trait's module and an inherent
/// method of that name in the type's own would leave the checker and the
/// interpreter free to pick differently.
fn check_method_collisions(program: &Program, errors: &mut Vec<Diagnostic>) {
    /// One method of one type: the module that declares the type, the type,
    /// and the method name.
    type MethodOf<'a> = (&'a str, &'a str, &'a str);
    /// Where a method was written: the module, and the name's span.
    type Site<'a> = (&'a str, Span);

    let mut declared: BTreeMap<MethodOf, Vec<Site>> = BTreeMap::new();
    for (module, resolved) in &program.modules {
        for ((type_name, method), entry) in &resolved.methods {
            let Some(owner) = resolved.owner_of(type_name) else {
                continue;
            };
            declared
                .entry((owner, type_name.as_str(), method.as_str()))
                .or_default()
                .push((module.as_str(), entry.decl.name.span));
        }
    }

    for ((type_module, type_name, method), sites) in declared {
        let [(first_module, first), rest @ ..] = sites.as_slice() else {
            continue;
        };
        for (module, span) in rest {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::duplicate_declaration",
                    format!(
                        "`{type_name}.{method}` is declared in module `{module}` and in module `{first_module}`"
                    ),
                )
                .at(*span)
                .label(*first, format!("`{method}` first declared here"))
                .rule(
                    "Each method name may be declared once per type, across every module: a conformance declared where its trait is must not collide with a method of the type's own module.",
                )
                .help(format!(
                    "rename one of them, or move both into module `{type_module}`, which declares `{type_name}`"
                )),
            );
        }
    }
}

/// Rejects a module that imports, directly or transitively, a module that
/// imports it.
///
/// ADR 0001 left "how are dependency cycles represented and diagnosed" open;
/// ADR 0005 answers it by forbidding them, so a package whose modules form a
/// cycle has a structure its author can see and fix.
fn check_import_cycles(edges: &[ImportEdge], errors: &mut Vec<Diagnostic>) {
    let mut graph: BTreeMap<&str, Vec<&ImportEdge>> = BTreeMap::new();
    for edge in edges {
        graph.entry(edge.from.as_str()).or_default().push(edge);
    }

    let mut settled: BTreeSet<&str> = BTreeSet::new();
    let mut reported: BTreeSet<Vec<&str>> = BTreeSet::new();
    let roots: Vec<&str> = graph.keys().copied().collect();
    for root in roots {
        let mut stack: Vec<&ImportEdge> = Vec::new();
        walk_imports(
            root,
            &graph,
            &mut Vec::new(),
            &mut stack,
            &mut settled,
            &mut reported,
            errors,
        );
    }
}

/// Depth-first walk over the module dependency graph, reporting the first
/// time each cycle is closed.
fn walk_imports<'a>(
    module: &'a str,
    graph: &BTreeMap<&'a str, Vec<&'a ImportEdge>>,
    path: &mut Vec<&'a str>,
    stack: &mut Vec<&'a ImportEdge>,
    settled: &mut BTreeSet<&'a str>,
    reported: &mut BTreeSet<Vec<&'a str>>,
    errors: &mut Vec<Diagnostic>,
) {
    if settled.contains(module) {
        return;
    }
    if let Some(start) = path.iter().position(|name| *name == module) {
        let mut cycle: Vec<&str> = path[start..].to_vec();
        cycle.push(module);
        let closing = *stack.last().expect("a cycle is closed by an edge");
        // One cycle is reachable from every module on it; report it once,
        // keyed by its members rather than by where the walk entered it.
        let mut members: Vec<&str> = cycle.clone();
        members.sort();
        members.dedup();
        if reported.insert(members) {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::import_cycle",
                    format!(
                        "module `{module}` imports itself through {}",
                        cycle.join(" -> ")
                    ),
                )
                .at(closing.span)
                .rule(
                    "A module may not import, directly or transitively, a module that imports it.",
                )
                .help("move what both modules need into a third module they can each import"),
            );
        }
        return;
    }

    path.push(module);
    for edge in graph.get(module).into_iter().flatten() {
        stack.push(edge);
        walk_imports(&edge.to, graph, path, stack, settled, reported, errors);
        stack.pop();
    }
    path.pop();
    settled.insert(module);
}

/// The name of Cove's builtin `Snapshot` trait.
///
/// Unlike every other trait, `Snapshot` is not written anywhere in Cove
/// source: it belongs to no module, so an `impl Snapshot for Type` conforms
/// wherever `Type` itself is declared, without a `trait Snapshot { ... }`
/// declaration or a `use` to reach one. This is a deliberate, narrow
/// departure from ADR 0006's "conformance is explicit... there is no
/// blanket implementation": the alternative was extending the trait grammar
/// with a `Self` return type solely so the compiler's own prelude could
/// spell one trait, which the MVP's "no associated types" already rules out
/// for user-written traits.
const BUILTIN_SNAPSHOT_TRAIT: &str = "Snapshot";

/// Synthesizes `Snapshot`'s one method, `fn snapshot(self) -> Self`, at the
/// header span of the `impl Snapshot for Type` block that needs it.
///
/// `Self` is not a type the language can otherwise write — the MVP has no
/// associated types — so this is built directly rather than parsed. Nothing
/// downstream needs it to be: `check_conformance` only checks method names,
/// not their types, and each conformance declares its own concrete return
/// type exactly like any other trait method.
fn builtin_snapshot_trait(span: Span) -> Arc<TraitDecl> {
    Arc::new(TraitDecl {
        name: Spanned::new(BUILTIN_SNAPSHOT_TRAIT.to_string(), span),
        methods: vec![TraitMethod {
            doc: Some(
                "Returns an independent, mutable copy of this value's own graph, preserving \
                 cycles and internal sharing where it has any."
                    .to_string(),
            ),
            name: Spanned::new("snapshot".to_string(), span),
            is_async: false,
            receiver: Some(Receiver {
                is_var: false,
                span,
            }),
            params: Vec::new(),
            return_type: None,
            default: None,
            span,
        }],
        span,
    })
}

/// Checks one `impl Trait for Type` block and records the conformance it
/// declares.
///
/// A conformance must supply every method the trait declares without a
/// default body, and may supply no method the trait does not declare. A
/// method the trait defaults and the block does not override is recorded as
/// the type's own method, running the trait's body, so that dispatch never
/// has to ask where a method came from.
#[allow(clippy::too_many_arguments)]
fn check_conformance(
    resolved: &mut ResolvedModule,
    module: &str,
    impl_block: &cove_syntax::ast::ImplBlock,
    conformance: Conformance,
    trait_decl: Arc<TraitDecl>,
    method_spans: &mut BTreeMap<(String, String), Span>,
    call_sites: &mut BTreeMap<FnKey, Vec<CallShape>>,
    opaque_fields: &OpaqueFields,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
) {
    let Conformance {
        trait_name,
        type_name,
        span: header,
        ..
    } = conformance.clone();
    // A conformance's methods are as public as the pair it joins: a trait
    // declared elsewhere is one this module imported, which it could only do
    // if the trait is exported.
    let trait_exported = resolved
        .traits
        .get(&trait_name)
        .map(|entry| entry.exported)
        .unwrap_or(true);
    let mut supplied: BTreeSet<String> = BTreeSet::new();

    for inner in &impl_block.items {
        let ItemKind::Fn(decl) = &inner.kind else {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::invalid_impl_item",
                    "only `fn` declarations are allowed inside an `impl` block",
                )
                .at(inner.span)
                .rule("An `impl` block may only contain method declarations."),
            );
            continue;
        };
        let method_name = decl.name.node.clone();
        let Some(declared) = trait_decl
            .methods
            .iter()
            .find(|m| m.name.node == method_name)
        else {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::unknown_trait_method",
                    format!("`{trait_name}` declares no method `{method_name}`"),
                )
                .at(decl.name.span)
                .label(trait_decl.name.span, format!("`{trait_name}` is declared here"))
                .rule("An `impl Trait for Type` block supplies exactly the methods the trait declares; anything else belongs in the type's own `impl` block.")
                .help(format!(
                    "declare `{method_name}` in `trait {trait_name}`, or move it to `impl {type_name}`"
                )),
            );
            continue;
        };
        supplied.insert(method_name);
        record_method(
            resolved,
            module,
            &type_name,
            Arc::new(decl.clone()),
            trait_exported,
            declared.doc.clone().or_else(|| inner.doc.clone()),
            None,
            method_spans,
            call_sites,
            opaque_fields,
            schemas,
            errors,
        );
    }

    let missing: Vec<String> = trait_decl
        .methods
        .iter()
        .filter(|m| m.default.is_none() && !supplied.contains(&m.name.node))
        .map(|m| m.name.node.clone())
        .collect();
    if !missing.is_empty() {
        errors.push(
            Diagnostic::error(
                "cove::resolve::missing_trait_method",
                format!(
                    "`{type_name}` does not conform to `{trait_name}`: missing {}",
                    list_backticked(&missing)
                ),
            )
            .at(header)
            .label(
                trait_decl.name.span,
                format!("`{trait_name}` declares {}", list_backticked(&missing)),
            )
            .rule("A conformance supplies every method its trait declares without a default body.")
            .help(format!(
                "add {} to this block",
                missing
                    .iter()
                    .map(|m| format!("`fn {m}(...)`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
    }

    // A defaulted method the block did not override becomes the type's own
    // method, with the trait's body.
    let mut methods = supplied.clone();
    for method in &trait_decl.methods {
        if supplied.contains(&method.name.node) {
            continue;
        }
        let Some(body) = &method.default else {
            continue;
        };
        methods.insert(method.name.node.clone());
        let decl = Arc::new(FnDecl {
            name: method.name.clone(),
            is_async: method.is_async,
            generics: Vec::new(),
            receiver: method.receiver,
            params: method.params.clone(),
            return_type: method.return_type.clone(),
            body: body.clone(),
            span: method.span,
        });
        record_method(
            resolved,
            module,
            &type_name,
            decl,
            trait_exported,
            method.doc.clone(),
            Some(trait_name.clone()),
            method_spans,
            call_sites,
            opaque_fields,
            schemas,
            errors,
        );
    }

    resolved.conformances.insert(
        (trait_name, type_name),
        Conformance {
            methods,
            ..conformance
        },
    );
}

/// Records one method of `type_name`, rejecting a second declaration of the
/// same name whatever `impl` block it came from: a trait method and an
/// inherent method of the same name would leave a call site with two
/// candidates and no rule to choose between them.
#[allow(clippy::too_many_arguments)]
fn record_method(
    resolved: &mut ResolvedModule,
    module: &str,
    type_name: &str,
    decl: Arc<FnDecl>,
    exported: bool,
    doc: Option<String>,
    from_trait_default: Option<String>,
    method_spans: &mut BTreeMap<(String, String), Span>,
    call_sites: &mut BTreeMap<FnKey, Vec<CallShape>>,
    opaque_fields: &OpaqueFields,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
) {
    let key = (type_name.to_string(), decl.name.node.clone());
    if let Some(existing_span) = method_spans.get(&key) {
        errors.push(
            Diagnostic::error(
                "cove::resolve::duplicate_declaration",
                format!(
                    "`{type_name}.{}` is declared twice in module `{module}`",
                    decl.name.node
                ),
            )
            .at(decl.name.span)
            .label(
                *existing_span,
                format!("`{}` first declared here", decl.name.node),
            )
            .rule(
                "Each method name may be declared once per type across a module's implementation units.",
            ),
        );
        return;
    }
    method_spans.insert(key.clone(), decl.name.span);
    let (capabilities, calls, open) = analyze_body(
        &decl,
        &resolved.host_uses,
        &resolved.host_items,
        opaque_fields,
        schemas,
    );
    call_sites.insert(
        FnKey::Method(type_name.to_string(), decl.name.node.clone()),
        calls,
    );
    resolved.methods.insert(
        key,
        FnEntry {
            decl,
            exported,
            // A method is never a test; see the `is_test` field.
            is_test: false,
            doc,
            receiver_type: Some(type_name.to_string()),
            from_trait_default,
            direct_capabilities: capabilities,
            required_capabilities: BTreeSet::new(),
            direct_open_calls: open,
            open_calls: BTreeSet::new(),
        },
    );
}

/// The orphan rule from ADR 0006, stated where it is broken.
fn orphan_conformance(module: &str, trait_name: &str, type_name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        "cove::resolve::orphan_conformance",
        format!(
            "module `{module}` declares neither `{trait_name}` nor `{type_name}`, so it cannot make one conform to the other"
        ),
    )
    .at(span)
    .rule("An `impl Trait for Type` is allowed only in the module that declares the trait or the module that declares the type, so that a conformance cannot appear from a module neither party knows about.")
    .help(format!(
        "move this block to the module that declares `{trait_name}` or the one that declares `{type_name}`"
    ))
}

/// Records `name` at `span` in `spans`, returning the previous span if `name`
/// was already declared.
fn duplicate(spans: &mut BTreeMap<String, Span>, name: &str, span: Span) -> Option<Span> {
    if let Some(existing) = spans.get(name) {
        return Some(*existing);
    }
    spans.insert(name.to_string(), span);
    None
}

fn duplicate_declaration(module: &str, name: &str, span: Span, first: Span) -> Diagnostic {
    Diagnostic::error(
        "cove::resolve::duplicate_declaration",
        format!("`{name}` is declared twice in module `{module}`"),
    )
    .at(span)
    .label(first, format!("`{name}` first declared here"))
    .rule("Each name may be declared once per module across its implementation units.")
}

/// The Language Card warns on an exported declaration with no doc comment.
fn missing_doc(warnings: &mut Vec<Diagnostic>, item: &Item, name: &str, span: Span) {
    if item.exported && item.doc.is_none() {
        warnings.push(undocumented(name, span));
    }
}

/// The `missing_doc` warning itself, for the declarations that are not
/// [`Item`]s of their own — a trait's methods.
fn undocumented(name: &str, span: Span) -> Diagnostic {
    Diagnostic::warning(
        "cove::resolve::missing_doc",
        format!("exported `{name}` has no doc comment"),
    )
    .at(span)
    .rule("Public declarations without doc comments warn by default.")
    .help(format!("Add a `///` doc comment above `{name}`."))
}

/// Every field name in the package declared with a type whose implementation
/// its producer chose, split by whether reading the field *is* holding such a
/// value or merely holding a container of them.
///
/// A body reaches a `dyn Trait` through a field far more often than it names
/// one: `struct Box { item: dyn Summary }` is written once, and every method
/// of `Box` then dispatches through `self.item` without the type appearing
/// again. Seeding the walk from parameters alone missed that shape entirely,
/// and a missed shape is the lower-bound-presented-as-complete failure
/// ADR 0015 exists to rule out.
///
/// The set is keyed by field *name*, across the whole package, because
/// resolution has no type checker to ask what `holder.item` is a field of.
/// That over-approximates in one direction only: an unrelated struct's field
/// of the same name reads as opaque too, so a method called on it is reported
/// as dispatching dynamically when it does not. Naming a capability-open
/// declaration that is not one costs a reader a second look; missing one
/// costs them the guarantee.
#[derive(Debug, Default)]
struct OpaqueFields {
    /// Fields declared `dyn Trait`, or as one of the declaring struct's own
    /// generic parameters: reading one holds the opaque value itself.
    direct: BTreeSet<String>,
    /// Fields whose type only mentions such a value at depth, such as
    /// `entries: Array<dyn Summary>`: reading one holds an ordinary
    /// container, and what comes out of it is opaque.
    containers: BTreeSet<String>,
}

impl OpaqueFields {
    /// Collects them from every struct the package declares.
    ///
    /// A struct's own generic parameters are what make `item: T` opaque, so
    /// each declaration is read against its own, not against the ones of
    /// whatever function later touches the field.
    fn of(package: &Package) -> Self {
        let mut fields = OpaqueFields::default();
        for module in package.modules.values() {
            for unit in &module.units {
                for item in &unit.ast.items {
                    let ItemKind::Struct(decl) = &item.kind else {
                        continue;
                    };
                    let generics: BTreeSet<String> = decl
                        .generics
                        .iter()
                        .map(|param| param.name.node.clone())
                        .collect();
                    for field in &decl.fields {
                        match type_opacity(&field.ty, &generics) {
                            Opacity::Direct => {
                                fields.direct.insert(field.name.node.clone());
                            }
                            Opacity::Container => {
                                fields.containers.insert(field.name.node.clone());
                            }
                            Opacity::None => {}
                        }
                    }
                }
            }
        }
        fields
    }
}

/// How far out of this declaration's reach the implementation behind a value
/// was chosen.
///
/// The distinction the two non-`None` cases draw is the whole point: a
/// container of `dyn Trait` is itself an ordinary `Array`, so `items.length()`
/// is an ordinary call, while `items.get(0).summarize()` is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Opacity {
    /// An ordinary value: its type names no `dyn Trait` and no generic
    /// parameter.
    None,
    /// A container of opaque values. A method called on the container is an
    /// ordinary call; anything taken out of it is [`Opacity::Direct`].
    Container,
    /// The value itself is one whose implementation its producer chose: a
    /// `dyn Trait`, or a generic parameter of the declaration being walked.
    /// A method called on it runs a conformance picked where the value was
    /// made.
    Direct,
}

/// Derives the Host API capabilities a function body calls directly, the raw
/// call sites found in it (used to build the module's call graph in a later
/// pass), and the indirect calls that make what it derived a lower bound.
///
/// This only looks at calls textually inside `decl`'s body (including nested
/// blocks, lambdas, match arms, loops, and local `fn` declarations). A
/// closure written here is walked here, which is the whole reason the lower
/// bound is worth having: a callback that prints is charged to the function
/// that *wrote* it, whatever later invokes it. `enums` is `None` here: a
/// module's enums are not all known yet at this point (see [`BodyWalk`]), so
/// `match` exhaustiveness is not checked during this walk.
///
/// The generic parameters this reads are `decl`'s own. An `impl` block's
/// generics are not consulted, because this is handed an [`FnDecl`] and
/// nothing else; that is not a hole today, since the parser rejects
/// `impl<T: Summary> Cell<T>`, but it becomes one the moment bounds on
/// `impl` blocks land.
fn analyze_body(
    decl: &FnDecl,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
    opaque_fields: &OpaqueFields,
    schemas: &HostSchemas,
) -> (BTreeSet<Capability>, Vec<CallShape>, BTreeSet<OpenCall>) {
    let generics: BTreeSet<String> = decl
        .generics
        .iter()
        .map(|param| param.name.node.clone())
        .collect();
    let mut walk = BodyWalk {
        host_uses,
        host_items,
        opaque_fields,
        schemas,
        enums: None,
        capabilities: BTreeSet::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        loop_depth: 0,
        generics,
        scopes: vec![Scope::default()],
        opaque: BTreeSet::new(),
        containers: BTreeSet::new(),
        open: BTreeSet::new(),
    };
    // `self` is a value the caller supplied, not a declaration this module
    // can be called through, so binding it keeps a body that merely reads it
    // from recording an edge to a same-named function.
    walk.bind_value("self");
    walk.bind_params(&decl.params);
    walk_block(&decl.body, &mut walk);
    (walk.capabilities, walk.calls, walk.open)
}

/// Which [`Opacity`] a value of type `ty` has, for a declaration binding
/// `generics`.
fn type_opacity(ty: &cove_syntax::ast::Type, generics: &BTreeSet<String>) -> Opacity {
    if is_opaque_type(ty, generics) {
        Opacity::Direct
    } else if mentions_opaque_type(ty, generics) {
        Opacity::Container
    } else {
        Opacity::None
    }
}

/// Whether a value of type `ty` is *itself* one whose implementation its
/// producer chose: a `dyn Trait`, or one of `generics` written bare.
///
/// This deliberately does not look inside a type's arguments.
/// `items: Array<T>` is an `Array` and nothing else, and `items.length()` is
/// `Array.length`, a builtin with no conformance to pick; only what comes out
/// of `items` is opaque. [`mentions_opaque_type`] is the question with the
/// depth in it.
fn is_opaque_type(ty: &cove_syntax::ast::Type, generics: &BTreeSet<String>) -> bool {
    use cove_syntax::ast::TypeKind;
    match &ty.kind {
        TypeKind::Dyn(_) => true,
        TypeKind::Named { path, .. } => path.len() == 1 && generics.contains(path[0].node.as_str()),
        TypeKind::Unit | TypeKind::Fn { .. } => false,
    }
}

/// Whether `ty` names, at any depth, a value whose implementation its
/// producer chose rather than this declaration.
///
/// Depth matters because a container hands out its elements:
/// `entries: Array<dyn Summary>` is how a body comes to hold a `dyn Summary`
/// without ever writing the type again.
fn mentions_opaque_type(ty: &cove_syntax::ast::Type, generics: &BTreeSet<String>) -> bool {
    use cove_syntax::ast::TypeKind;
    match &ty.kind {
        TypeKind::Dyn(_) => true,
        TypeKind::Unit => false,
        TypeKind::Named { path, args } => {
            (path.len() == 1 && generics.contains(path[0].node.as_str()))
                || args.iter().any(|arg| mentions_opaque_type(arg, generics))
        }
        TypeKind::Fn {
            params,
            return_type,
            ..
        } => {
            params.iter().any(|param| {
                param
                    .ty
                    .as_ref()
                    .is_some_and(|ty| mentions_opaque_type(ty, generics))
            }) || return_type
                .as_ref()
                .is_some_and(|ty| mentions_opaque_type(ty, generics))
        }
    }
}

/// Whether `expr`'s own value is one whose implementation its producer chose,
/// so that a method called on it runs a conformance picked somewhere this
/// call graph does not reach.
///
/// A bare name and a field read are values this walk can classify exactly:
/// `items: Array<T>` binds a container, so `items.length()` is an ordinary
/// call and `holder.entries.length()` is too, while `self.item.summarize()`
/// is not. Anything else — a call, an element taken out of a collection, a
/// chain — is a value with no name here, and [`mentions_opaque`] is what
/// decides it: without a type checker the walk cannot say what
/// `entries.get(0)` *is*, only that it came out of `entries`, which is enough
/// to know that a method called on it dispatches where this call graph does
/// not lead.
fn value_is_opaque(expr: &Expr, walk: &BodyWalk) -> bool {
    match &expr.kind {
        ExprKind::Ident(name) => walk.is_opaque(name),
        ExprKind::Field { base, name } => {
            walk.opaque_fields.direct.contains(name.node.as_str()) || value_is_opaque(base, walk)
        }
        _ => mentions_opaque(expr, walk),
    }
}

/// Whether `expr` reads a name or a field bound to a *container* of opaque
/// values, so that a binding taken straight from it is a container too.
fn holds_opaque_container(expr: &Expr, walk: &BodyWalk) -> bool {
    match &expr.kind {
        ExprKind::Ident(name) => walk.is_container(name),
        ExprKind::Field { name, .. } => walk.opaque_fields.containers.contains(name.node.as_str()),
        _ => false,
    }
}

/// The [`Opacity`] a `let` or `var` binding takes on.
///
/// A written type answers it outright. Without one this falls back to what
/// the initialiser *reads*, which is a mention rather than a type: a bare
/// name or a field read passes its own class along, and anything else that
/// touches an opaque value — `entries.get(0)`, `self.item.next()` — produces
/// the opaque value rather than a container of them.
///
/// What this cannot see is an initialiser whose opacity lives only in its
/// type. `let entry = makeDyn()` binds a `dyn Trait` that nothing in this
/// body writes, and resolution has no type checker to ask what `makeDyn`
/// returns, so the binding is not tracked and the dispatch on it falls back
/// to the receiver over-approximation. ADR 0015 names that gap.
fn binding_opacity(ty: Option<&cove_syntax::ast::Type>, value: &Expr, walk: &BodyWalk) -> Opacity {
    let written = ty.map_or(Opacity::None, |ty| type_opacity(ty, &walk.generics));
    let read = if value_is_opaque(value, walk) {
        Opacity::Direct
    } else if holds_opaque_container(value, walk) {
        Opacity::Container
    } else if mentions_opaque(value, walk) {
        Opacity::Direct
    } else {
        Opacity::None
    };
    written.max(read)
}

/// Whether `expr` reads anything opaque, so that what it produces may be one
/// of those values or something taken out of one.
///
/// This is deliberately a mention rather than a type: without a type checker
/// the walk cannot say what `entries.get(0)` *is*, only that it came from
/// `entries`.
fn mentions_opaque(expr: &Expr, walk: &BodyWalk) -> bool {
    let any = |exprs: &[Expr]| exprs.iter().any(|e| mentions_opaque(e, walk));
    match &expr.kind {
        ExprKind::Ident(name) => walk.is_opaque(name) || walk.is_container(name),
        ExprKind::Field { base, name } => {
            walk.opaque_fields.direct.contains(name.node.as_str())
                || walk.opaque_fields.containers.contains(name.node.as_str())
                || mentions_opaque(base, walk)
        }
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            mentions_opaque(callee, walk)
                || args.iter().any(|arg| mentions_opaque(&arg.value, walk))
                || trailing
                    .as_ref()
                    .is_some_and(|tail| mentions_opaque(tail, walk))
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) | ExprKind::Unary { operand: inner, .. } => {
            mentions_opaque(inner, walk)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            mentions_opaque(lhs, walk) || mentions_opaque(rhs, walk)
        }
        ExprKind::ArrayLit(items) => any(items),
        ExprKind::Str(parts) => parts.iter().any(|part| match part {
            StrPart::Interpolation(inner) => mentions_opaque(inner, walk),
            StrPart::Text(_) => false,
        }),
        ExprKind::Block(block) | ExprKind::Scope { body: block, .. } => {
            block_mentions_opaque(block, walk)
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_mentions_opaque(then_branch, walk)
                || else_branch
                    .as_ref()
                    .is_some_and(|branch| mentions_opaque(branch, walk))
        }
        ExprKind::Match { arms, .. } => arms.iter().any(|arm| mentions_opaque(&arm.body, walk)),
        _ => false,
    }
}

/// Whether a block's value can be an opaque one: its tail is what it
/// produces, and that is the only way a value leaves it.
///
/// A `break` inside the block is not a second way out. It belongs to an
/// enclosing loop rather than to this block, and a loop is not a value in
/// Cove anyway — `typeck` gives both `while` and `for` the type `Unit`
/// whatever a `break` inside them carries — so there is nothing here for a
/// `break` operand to become.
fn block_mentions_opaque(block: &Block, walk: &BodyWalk) -> bool {
    block
        .tail
        .as_ref()
        .is_some_and(|tail| mentions_opaque(tail, walk))
}

/// The enums a module can name: the ones it declares, plus the ones it
/// imported, under the single name each is visible by.
type EnumsInScope<'a> = BTreeMap<&'a str, &'a EnumEntry>;

/// Checks every `match` expression in every body of `program` for the
/// exhaustiveness and case-name facts derivable without a type checker, now
/// that every enum a module can name is known — including an imported one.
/// This walk also reports `break` and `continue` outside a loop, which does
/// not depend on `enums` and so already ran once (harmlessly, since
/// [`analyze_body`] discards its walk's errors) while the module's
/// declarations were being collected.
///
/// This reuses [`walk_block`] rather than a second traversal: the only
/// difference from the walk [`analyze_body`] already did is that `enums` is
/// filled in this time, so [`check_match_arms`] actually runs.
fn check_bodies(
    program: &Program,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    for resolved in program.modules.values() {
        let enums = enums_in_scope(program, resolved);
        for entry in resolved.functions.values() {
            check_body(
                &entry.decl.body,
                resolved,
                &enums,
                schemas,
                errors,
                warnings,
            );
        }
        for entry in resolved.methods.values() {
            // A default body belongs to the trait that declares it, so it is
            // walked once below rather than once per conformance — and in
            // the module that declares the trait, whose enums are the ones
            // its arms can name.
            if entry.from_trait_default.is_none() {
                check_body(
                    &entry.decl.body,
                    resolved,
                    &enums,
                    schemas,
                    errors,
                    warnings,
                );
            }
        }
        for entry in resolved.traits.values() {
            for method in &entry.decl.methods {
                if let Some(body) = &method.default {
                    check_body(body, resolved, &enums, schemas, errors, warnings);
                }
            }
        }
    }
}

/// Every enum `resolved` can name, whether it declares it or imported it.
///
/// A `use` cannot bind a name the importing module declares, so the two
/// sources never disagree about a name.
fn enums_in_scope<'a>(program: &'a Program, resolved: &'a ResolvedModule) -> EnumsInScope<'a> {
    let mut enums: EnumsInScope<'a> = resolved
        .enums
        .iter()
        .map(|(name, entry)| (name.as_str(), entry))
        .collect();
    for (name, owner) in &resolved.imports {
        let Some(entry) = program
            .modules
            .get(owner)
            .and_then(|owner| owner.enums.get(name))
        else {
            continue;
        };
        enums.insert(name.as_str(), entry);
    }
    enums
}

fn check_body(
    body: &Block,
    resolved: &ResolvedModule,
    enums: &EnumsInScope,
    schemas: &HostSchemas,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    let no_opaque_fields = OpaqueFields::default();
    let mut walk = BodyWalk {
        host_uses: &resolved.host_uses,
        host_items: &resolved.host_items,
        schemas,
        enums: Some(enums),
        capabilities: BTreeSet::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        loop_depth: 0,
        // This walk answers `match` questions; the capability facts it would
        // re-derive were already recorded by [`analyze_body`], so it starts
        // from no declaration and discards what it finds.
        generics: BTreeSet::new(),
        opaque_fields: &no_opaque_fields,
        scopes: Vec::new(),
        opaque: BTreeSet::new(),
        containers: BTreeSet::new(),
        open: BTreeSet::new(),
    };
    walk_block(body, &mut walk);
    errors.extend(walk.errors);
    warnings.extend(walk.warnings);
}

/// Everything the body walker threads through a traversal, and what it
/// collects along the way.
///
/// The walker runs twice per body. The first run, from [`analyze_body`],
/// happens while a module's declarations are still being collected, so
/// `enums` is `None` and only `capabilities` and `calls` are derived. The
/// second run, from [`check_body_matches`], happens once every enum is
/// known; `enums` is filled in, and [`check_match_arms`] records what it
/// finds in `errors` and `warnings` instead.
struct BodyWalk<'a> {
    host_uses: &'a BTreeSet<String>,
    host_items: &'a BTreeMap<String, String>,
    /// The host modules this compilation can see, which is what turns a call
    /// into the capability it requires.
    schemas: &'a HostSchemas,
    enums: Option<&'a EnumsInScope<'a>>,
    capabilities: BTreeSet<Capability>,
    calls: Vec<CallShape>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
    /// How many enclosing `for`/`while` loops the walk is currently inside,
    /// reset to `0` while walking a lambda body. `break` and `continue` only
    /// make sense inside a loop of the same function or closure; they cannot
    /// reach a loop outside a closure boundary, matching how `return` already
    /// only unwinds to the nearest enclosing call.
    loop_depth: u32,
    /// The generic parameters the declaration being walked binds, which is
    /// what makes `entry: T` a value whose implementation its caller chose
    /// rather than a value of a type named `T`.
    generics: BTreeSet<String>,
    /// Every field name in the package that holds, or contains, a value whose
    /// implementation its producer chose; see [`OpaqueFields`].
    opaque_fields: &'a OpaqueFields,
    /// The names this body has bound, innermost scope last.
    ///
    /// This exists so that a name the body binds is never mistaken for the
    /// module-level declaration it shadows: `fn label(report: String)` reads
    /// `report` as its own parameter, not as a call-graph edge to whatever
    /// `fn report` the module happens to declare.
    scopes: Vec<Scope>,
    /// The names bound to a value whose implementation its producer chose: a
    /// parameter or lambda parameter whose type is a `dyn Trait` or a generic
    /// parameter, and anything later bound from one by `let`, `var`, or
    /// `for`.
    ///
    /// Unlike [`BodyWalk::scopes`] this is flat and only grows. A name that
    /// has gone out of scope can then still read as opaque, which costs
    /// precision in one direction only — an extra `DynamicDispatch`, never a
    /// missing one — and in exchange a value that leaves a block through its
    /// tail keeps the class it was given inside.
    opaque: BTreeSet<String>,
    /// The same, for names bound to a *container* of such values, which is a
    /// different fact: `items.length()` is an ordinary call on an ordinary
    /// `Array`, and only what comes out of `items` is opaque.
    containers: BTreeSet<String>,
    /// Why what this walk derived is a lower bound; see [`OpenCall`].
    open: BTreeSet<OpenCall>,
}

/// The names one lexical scope of a body binds.
#[derive(Debug, Default)]
struct Scope {
    /// Names bound to a value: parameters, `self`, `let` and `var` bindings,
    /// a `for` binding, a lambda's parameters. Calling one is a call to a
    /// value, and reading one is not a reference to a declaration.
    values: BTreeSet<String>,
    /// Local `fn` declarations. Their bodies are walked where they are
    /// written, exactly as a lambda's is, so calling one needs no call-graph
    /// edge and hides nothing.
    functions: BTreeSet<String>,
}

impl BodyWalk<'_> {
    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Records `name` as bound to a value in the innermost scope.
    fn bind_value(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.values.insert(name.to_string());
        }
    }

    /// Records `name` as a local `fn` in the innermost scope.
    fn bind_local_fn(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.functions.insert(name.to_string());
        }
    }

    /// Binds a declaration's or a lambda's parameters, with the opacity each
    /// one's written type gives it.
    ///
    /// A variadic parameter is an `Array` of what it was written as, so
    /// `items: T...` binds a container rather than an opaque value itself.
    fn bind_params(&mut self, params: &[cove_syntax::ast::Param]) {
        for param in params {
            let mut opacity = param
                .ty
                .as_ref()
                .map_or(Opacity::None, |ty| type_opacity(ty, &self.generics));
            if param.variadic {
                opacity = opacity.min(Opacity::Container);
            }
            self.bind_value(&param.name.node);
            self.mark(&param.name.node, opacity);
        }
    }

    /// Records what `name` is bound to, when it is not an ordinary value.
    fn mark(&mut self, name: &str, opacity: Opacity) {
        match opacity {
            Opacity::None => {}
            Opacity::Container => {
                self.containers.insert(name.to_string());
            }
            Opacity::Direct => {
                self.opaque.insert(name.to_string());
            }
        }
    }

    fn binds_value(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.values.contains(name))
    }

    fn binds_local_fn(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.functions.contains(name))
    }

    fn is_opaque(&self, name: &str) -> bool {
        self.opaque.contains(name)
    }

    fn is_container(&self, name: &str) -> bool {
        self.containers.contains(name)
    }
}

/// The shape of one call site's callee, kept just precise enough to resolve
/// which declarations of the module it may reach. Resolution happens later,
/// once every declaration in the module is known; see [`resolve_calls`].
#[derive(Clone, Debug)]
enum CallShape {
    /// `f(...)`.
    Ident(String),
    /// `receiver.method(...)`. `receiver_ident` is the receiver's name when
    /// it is a bare identifier (such as `self` or a struct/enum name used as
    /// a namespace), and `None` for any other receiver expression.
    Field {
        receiver_ident: Option<String>,
        method: String,
    },
    /// A bare name read as a value rather than called: `handler: health`.
    ///
    /// A named function handed to a host, stored in a route table, or
    /// wrapped in a closure is called somewhere this body cannot see, so
    /// naming it is what makes it reachable at all. The edge is the same
    /// edge a call would make, which is what keeps a callback the host
    /// invokes from dropping out of the derived set.
    Reference(String),
}

/// A node in a module's call graph: a free function or an `impl` method.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FnKey {
    /// A free function, by its name.
    Fn(String),
    /// A method or associated function, by `(type name, function name)`.
    Method(String, String),
}

fn walk_block(block: &Block, walk: &mut BodyWalk) {
    walk.push_scope();
    for stmt in &block.statements {
        walk_stmt(stmt, walk);
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, walk);
    }
    walk.pop_scope();
}

fn walk_stmt(stmt: &Stmt, walk: &mut BodyWalk) {
    match &stmt.kind {
        StmtKind::Let {
            name, ty, value, ..
        } => {
            // The value is walked first, so `let x = x` still reads the
            // outer binding rather than the one it is about to make.
            walk_expr(value, walk);
            let opacity = binding_opacity(ty.as_ref(), value, walk);
            walk.bind_value(&name.node);
            walk.mark(&name.node, opacity);
        }
        StmtKind::Expr(expr) => walk_expr(expr, walk),
        // A local `fn` is an ordinary closure the enclosing body writes —
        // `typeck` and the interpreter both treat it as one — so it is
        // charged the same way an inline lambda is: its body is analysed
        // here, and calling it by name is an ordinary call rather than a
        // call to a value whose target nothing can name. Any other nested
        // declaration contributes nothing to this body.
        StmtKind::Item(item) => {
            if let ItemKind::Fn(decl) = &item.kind {
                walk.bind_local_fn(&decl.name.node);
                walk.push_scope();
                walk.bind_params(&decl.params);
                // A local `fn` is a closure boundary, so `break` and
                // `continue` inside it cannot reach a loop outside it, just
                // as they cannot out of a lambda.
                let outer_depth = std::mem::replace(&mut walk.loop_depth, 0);
                walk_block(&decl.body, walk);
                walk.loop_depth = outer_depth;
                walk.pop_scope();
            }
        }
    }
}

fn walk_expr(expr: &Expr, walk: &mut BodyWalk) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit => {}
        // A name read as a value may be a function being handed somewhere
        // else to be called; see [`CallShape::Reference`]. A name this body
        // binds is not that function however it is spelled, so it records
        // nothing — otherwise a parameter or local shadowing a module-level
        // function would draw an exact edge to a declaration it cannot
        // reach. A name that turns out to be a type resolves to no
        // declaration and so records nothing either.
        ExprKind::Ident(name) => {
            if !walk.binds_value(name) && !walk.binds_local_fn(name) {
                walk.calls.push(CallShape::Reference(name.clone()));
            }
        }
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    walk_expr(inner, walk);
                }
            }
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                walk_expr(item, walk);
            }
        }
        ExprKind::Field { base, .. } => walk_expr(base, walk),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            if let Some(capability) =
                call_capability(callee, walk.host_uses, walk.host_items, walk.schemas)
            {
                walk.capabilities.insert(capability);
            }
            match call_shape(callee) {
                // A local `fn` is a closure this walk already analysed where
                // it was written, so calling it needs no edge and hides
                // nothing from the derived set.
                Some(CallShape::Ident(name)) if walk.binds_local_fn(&name) => {}
                // A name this body bound to a value is the higher-order
                // case whatever a module declares under the same name.
                Some(CallShape::Ident(name)) if walk.binds_value(&name) => {
                    walk.open.insert(OpenCall::FunctionValue);
                }
                Some(shape) => {
                    // A method call on a value whose implementation the
                    // caller chose runs a conformance picked where that
                    // value was made, which is not somewhere resolution can
                    // follow from here.
                    if let (CallShape::Field { .. }, ExprKind::Field { base: receiver, .. }) =
                        (&shape, &callee.kind)
                    {
                        if value_is_opaque(receiver, walk) {
                            walk.open.insert(OpenCall::DynamicDispatch);
                        }
                    }
                    walk.calls.push(shape);
                }
                // `handlers.get(0)()` and its neighbours: the callee is a
                // value with no name, so no edge leads to what it runs.
                None => {
                    walk.open.insert(OpenCall::FunctionValue);
                }
            }
            walk_expr(callee, walk);
            for arg in args {
                walk_expr(&arg.value, walk);
            }
            if let Some(trailing) = trailing {
                walk_expr(trailing, walk);
            }
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, walk),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, walk);
            walk_expr(rhs, walk);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_expr(target, walk);
            walk_expr(value, walk);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => walk_expr(inner, walk),
        ExprKind::Block(block) => walk_block(block, walk),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, walk);
            walk_block(then_branch, walk);
            if let Some(else_branch) = else_branch {
                walk_expr(else_branch, walk);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, walk);
            check_match_arms(expr, arms, walk);
            for MatchArm { body, .. } in arms {
                walk_expr(body, walk);
            }
        }
        ExprKind::For {
            binding,
            iterable,
            body,
        } => {
            walk_expr(iterable, walk);
            // Iterating a container of `dyn Trait` hands out one per turn,
            // so the binding is the opaque value the container held rather
            // than another container of them.
            let opaque = mentions_opaque(iterable, walk);
            walk.push_scope();
            walk.bind_value(&binding.node);
            if opaque {
                walk.mark(&binding.node, Opacity::Direct);
            }
            walk.loop_depth += 1;
            walk_block(body, walk);
            walk.loop_depth -= 1;
            walk.pop_scope();
        }
        ExprKind::While { condition, body } => {
            walk_expr(condition, walk);
            walk.loop_depth += 1;
            walk_block(body, walk);
            walk.loop_depth -= 1;
        }
        ExprKind::Return(inner) => {
            if let Some(inner) = inner {
                walk_expr(inner, walk);
            }
        }
        ExprKind::Break(inner) => {
            if let Some(inner) = inner {
                walk_expr(inner, walk);
            }
            check_in_loop(expr, "break", walk);
        }
        ExprKind::Continue => check_in_loop(expr, "continue", walk),
        // A lambda is a separate closure boundary: `break` and `continue`
        // cannot reach a loop outside it, exactly as `return` inside a
        // lambda returns from the lambda, not the enclosing function.
        ExprKind::Lambda { params, body, .. } => {
            let outer_depth = std::mem::replace(&mut walk.loop_depth, 0);
            walk.push_scope();
            walk.bind_params(params);
            walk_block(body, walk);
            walk.pop_scope();
            walk.loop_depth = outer_depth;
        }
        ExprKind::Scope { body, .. } => walk_block(body, walk),
        ExprKind::Range { start, end, .. } => {
            walk_expr(start, walk);
            walk_expr(end, walk);
        }
    }
}

/// Reports `keyword` (`break` or `continue`) used outside any loop this walk
/// has entered. A lambda body resets [`BodyWalk::loop_depth`] to `0`, so this
/// also rejects reaching for a loop across a closure boundary.
fn check_in_loop(expr: &Expr, keyword: &str, walk: &mut BodyWalk) {
    if walk.loop_depth == 0 {
        walk.errors.push(
            Diagnostic::error(
                format!("cove::resolve::{keyword}_outside_loop"),
                format!("`{keyword}` outside a loop"),
            )
            .at(expr.span)
            .rule(format!(
                "`{keyword}` only makes sense inside a `for` or `while` loop, and cannot reach one outside a closure."
            ))
            .help(format!("move this `{keyword}` inside an enclosing loop, or remove it")),
        );
    }
}

/// Checks one `match` expression for the exhaustiveness and case-name facts
/// derivable without a type checker. A no-op until every enum in the module
/// is known (`walk.enums.is_some()`); see [`BodyWalk`].
///
/// The scrutinee's enum is determined from the arms' `Variant` patterns
/// alone (see [`resolve_target_enum`]); when it cannot be determined, this
/// silently reports nothing rather than guess. `Wildcard` and `Binding` arms
/// make a match exhaustive by construction, since there is no static type to
/// check them against. Arms after the first such catch-all arm can never
/// run, which is checked independently of whether the enum could be
/// determined at all.
fn check_match_arms(match_expr: &Expr, arms: &[MatchArm], walk: &mut BodyWalk) {
    let Some(enums) = walk.enums else {
        return;
    };

    let catch_all_index = arms.iter().position(|arm| is_catch_all(&arm.pattern));
    if let Some(catch_all_index) = catch_all_index {
        let catch_all_span = arms[catch_all_index].span;
        for arm in &arms[catch_all_index + 1..] {
            walk.warnings.push(
                Diagnostic::warning(
                    "cove::resolve::unreachable_match_arm",
                    "this `match` arm can never run",
                )
                .at(arm.span)
                .label(
                    catch_all_span,
                    "unreachable because this earlier arm matches everything",
                )
                .rule("An arm after a `_` or binding arm can never run."),
            );
        }
    }
    let has_catch_all = catch_all_index.is_some();

    if let Some(target) = resolve_target_enum(arms, enums) {
        let valid_cases = target.case_names();
        let mut seen: BTreeMap<&str, Span> = BTreeMap::new();
        for arm in arms {
            let PatternKind::Variant { path, .. } = &arm.pattern.kind else {
                continue;
            };
            let case_name = path.last().expect("a variant path is never empty");
            if !valid_cases.iter().any(|case| case == &case_name.node) {
                walk.errors.push(
                    Diagnostic::error(
                        "cove::resolve::unknown_enum_case",
                        format!(
                            "`{}` is not a case of `{}`",
                            case_name.node,
                            target.display_name()
                        ),
                    )
                    .at(arm.pattern.span)
                    .rule("Every `match` arm must name a case its enum declares.")
                    .help(format!(
                        "`{}` declares {}",
                        target.display_name(),
                        list_backticked(&valid_cases)
                    )),
                );
                continue;
            }
            if let Some(first_span) = seen.get(case_name.node.as_str()) {
                walk.errors.push(
                    Diagnostic::error(
                        "cove::resolve::duplicate_match_arm",
                        format!("`{}` is already covered by an earlier arm", case_name.node),
                    )
                    .at(arm.pattern.span)
                    .label(
                        *first_span,
                        format!("`{}` first matched here", case_name.node),
                    )
                    .rule("Each enum case may be matched by at most one arm."),
                );
            } else {
                seen.insert(case_name.node.as_str(), arm.pattern.span);
            }
        }

        if !has_catch_all {
            let missing: Vec<String> = valid_cases
                .iter()
                .filter(|case| !seen.contains_key(case.as_str()))
                .map(|case| target.qualified(case))
                .collect();
            if !missing.is_empty() {
                walk.errors.push(non_exhaustive_enum_match(
                    match_expr.span,
                    &target,
                    &missing,
                ));
            }
        }
        return;
    }

    check_literal_arms(match_expr, arms, catch_all_index, has_catch_all, walk);
}

/// Checks a `match`'s literal-pattern arms once no enum could be determined
/// for the scrutinee (see [`resolve_target_enum`]).
///
/// `Bool` is exhaustible in a way `Int` and `String` are not: its domain is
/// exactly two values, `true` and `false`, so a `match` whose literal arms
/// are all `Bool` can be proven exhaustive once both are covered, with no
/// catch-all required. This is a property of the type, not a special case
/// carved out for `match` — `Int` and `String` have effectively unbounded
/// domains, so a literal `match` over either can never be proven exhaustive
/// without a catch-all. A match mixing a `Bool` literal with a non-`Bool`
/// literal (which cannot happen once there is a type checker, but nothing
/// here rules it out yet) is treated as the non-`Bool` case, since it still
/// needs a catch-all.
fn check_literal_arms(
    match_expr: &Expr,
    arms: &[MatchArm],
    catch_all_index: Option<usize>,
    has_catch_all: bool,
    walk: &mut BodyWalk,
) {
    let literal_indices: Vec<usize> = arms
        .iter()
        .enumerate()
        .filter(|(_, arm)| is_literal(&arm.pattern))
        .map(|(index, _)| index)
        .collect();
    if literal_indices.is_empty() {
        return;
    }

    let all_bool = literal_indices
        .iter()
        .all(|&index| literal_bool_value(&arms[index].pattern).is_some());

    if !all_bool {
        if !has_catch_all {
            walk.errors.push(
                Diagnostic::error(
                    "cove::resolve::non_exhaustive_match",
                    "`match` over literal patterns needs a `_` or binding arm",
                )
                .at(match_expr.span)
                .rule("`match` must cover every enum case.")
                .help(
                    "add a `_` arm, or a binding arm, to cover every value the literal arms do not",
                ),
            );
        }
        return;
    }

    let mut seen: BTreeMap<bool, Span> = BTreeMap::new();
    let mut covered_at: Option<usize> = None;
    for &index in &literal_indices {
        let value = literal_bool_value(&arms[index].pattern).expect("checked all_bool above");
        if let Some(first_span) = seen.get(&value) {
            walk.errors.push(
                Diagnostic::error(
                    "cove::resolve::duplicate_match_arm",
                    format!("`{value}` is already covered by an earlier arm"),
                )
                .at(arms[index].pattern.span)
                .label(*first_span, format!("`{value}` first matched here"))
                .rule("Each value of `Bool` may be matched by at most one arm."),
            );
            continue;
        }
        seen.insert(value, arms[index].pattern.span);
        if seen.len() == 2 && covered_at.is_none() {
            covered_at = Some(index);
        }
    }

    if let Some(catch_all_index) = catch_all_index {
        if let Some(covered_at) = covered_at {
            if covered_at < catch_all_index {
                walk.warnings.push(
                    Diagnostic::warning(
                        "cove::resolve::unreachable_match_arm",
                        "this `match` arm can never run",
                    )
                    .at(arms[catch_all_index].span)
                    .label(
                        arms[covered_at].span,
                        "unreachable because `true` and `false` are already covered here",
                    )
                    .rule("A `match` over `Bool` covering both `true` and `false` leaves no value for a later `_` or binding arm."),
                );
            }
        }
        return;
    }

    if seen.len() < 2 {
        let missing = if seen.contains_key(&true) {
            "false"
        } else {
            "true"
        };
        walk.errors.push(
            Diagnostic::error(
                "cove::resolve::non_exhaustive_match",
                format!("this `match` does not cover `{missing}`"),
            )
            .at(match_expr.span)
            .rule("A `match` over `Bool` must cover both `true` and `false`.")
            .help(format!("add a `{missing} => ...` arm, or add a `_` arm")),
        );
    }
}

/// The `Bool` value a literal pattern matches, or `None` when the pattern is
/// not a literal or its literal is not a `Bool`.
fn literal_bool_value(pattern: &Pattern) -> Option<bool> {
    let PatternKind::Literal(expr) = &pattern.kind else {
        return None;
    };
    match expr.kind {
        ExprKind::Bool(value) => Some(value),
        _ => None,
    }
}

fn is_catch_all(pattern: &Pattern) -> bool {
    matches!(
        pattern.kind,
        PatternKind::Wildcard | PatternKind::Binding(_)
    )
}

fn is_literal(pattern: &Pattern) -> bool {
    matches!(pattern.kind, PatternKind::Literal(_))
}

/// The enum a `match`'s `Variant` arms name, when this analysis can
/// determine it. `Option` and `Result` are builtins rather than module
/// declarations, so they are represented separately from a module `enum`.
enum TargetEnum<'a> {
    Declared(&'a EnumEntry),
    /// One of the language's own enums, held as the schema entry that
    /// declares it. What its cases are is a question this crate asks rather
    /// than answers: `cove_schema::builtins` says that an `Option` is `Some`
    /// and `None`, and the runtime builds those values out of the same
    /// entry.
    Builtin(&'static cove_schema::builtins::BuiltinSchema),
}

impl TargetEnum<'_> {
    fn display_name(&self) -> &str {
        match self {
            TargetEnum::Declared(entry) => &entry.decl.name.node,
            TargetEnum::Builtin(schema) => schema.name,
        }
    }

    /// Case names in declaration order.
    fn case_names(&self) -> Vec<String> {
        match self {
            TargetEnum::Declared(entry) => entry
                .decl
                .cases
                .iter()
                .map(|case| case.name.node.clone())
                .collect(),
            TargetEnum::Builtin(schema) => schema
                .cases
                .iter()
                .map(|case| case.name.to_string())
                .collect(),
        }
    }

    /// How a missing case should read in a diagnostic: qualified for a
    /// module enum (`LogLevel.Warn`), bare for a builtin, since arms write
    /// `Some(x)` and `None`, never `Option.Some(x)`.
    fn qualified(&self, case: &str) -> String {
        match self {
            TargetEnum::Declared(entry) => format!("{}.{case}", entry.decl.name.node),
            TargetEnum::Builtin(_) => case.to_string(),
        }
    }
}

/// Determines the enum a `match`'s `Variant` arms name, or `None` when this
/// analysis cannot be sure: no arm is a `Variant`, the arms disagree about
/// which enum they name, a bare case name matches no enum in scope or more
/// than one, or the settled-on enum is one this module can neither declare
/// nor name through an import.
///
/// A path of three or more segments, such as the `booking.Status.Confirmed`
/// an imported *module* makes writable, still abstains: the arms carry no
/// type, so the enum a qualified path names is left to the type checker.
fn resolve_target_enum<'a>(arms: &[MatchArm], enums: &EnumsInScope<'a>) -> Option<TargetEnum<'a>> {
    let mut candidate: Option<String> = None;
    for arm in arms {
        let PatternKind::Variant { path, .. } = &arm.pattern.kind else {
            continue;
        };
        let this_enum = match path.as_slice() {
            [case] => bare_case_enum(&case.node, enums)?,
            [enum_name, _case] => enum_name.node.clone(),
            _ => return None,
        };
        match &candidate {
            None => candidate = Some(this_enum),
            Some(existing) if *existing != this_enum => return None,
            _ => {}
        }
    }

    let candidate = candidate?;
    match cove_schema::builtin(&candidate) {
        Some(schema) if schema.is_enum() => Some(TargetEnum::Builtin(schema)),
        _ => enums
            .get(candidate.as_str())
            .copied()
            .map(TargetEnum::Declared),
    }
}

/// The enum a bare case name such as `Debug` names: the builtin enum that
/// declares it, or the one enum in scope whose cases include it. `None` when
/// no enum declares that case, or more than one does.
fn bare_case_enum(case_name: &str, enums: &EnumsInScope) -> Option<String> {
    if let Some(schema) = cove_schema::builtins::enum_declaring(case_name) {
        return Some(schema.name.to_string());
    }
    let mut matches = enums
        .iter()
        .filter(|(_, entry)| {
            entry
                .decl
                .cases
                .iter()
                .any(|case| case.name.node == case_name)
        })
        .map(|(name, _)| (*name).to_string());
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

fn list_backticked(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the `non_exhaustive_match` diagnostic for a `match` over `target`
/// missing the cases in `missing`, in declaration order.
fn non_exhaustive_enum_match(span: Span, target: &TargetEnum, missing: &[String]) -> Diagnostic {
    let list = list_backticked(missing);
    let help = if missing.len() == 1 {
        format!("add an arm for {list}, or add a `_` arm")
    } else {
        format!("add arms for {list}, or add a `_` arm")
    };
    Diagnostic::error(
        "cove::resolve::non_exhaustive_match",
        format!(
            "`match` does not cover every case of `{}`: missing {list}",
            target.display_name()
        ),
    )
    .at(span)
    .rule("`match` must cover every enum case.")
    .help(help)
}

/// The call-graph shape of `callee`, when it is a form the call graph can
/// resolve (a bare name or a field access). Any other callee, such as an
/// immediately-called lambda or an element taken out of a collection,
/// contributes no call-graph edge; its caller records
/// [`OpenCall::FunctionValue`] instead, so the gap is reported rather than
/// dropped.
///
/// A field that holds a function value is a hole waiting to open. `h.cb()`
/// and `(h.cb)()` on a `fn`-typed field are rejected by the type checker
/// today, so there is nothing to miss — but the parser normalises both into
/// a `Field` callee, which this reads as a method call, resolves to no
/// method, and marks nothing. The day a function-typed field becomes callable
/// this has to distinguish the two.
fn call_shape(callee: &Expr) -> Option<CallShape> {
    match &callee.kind {
        ExprKind::Ident(name) => Some(CallShape::Ident(name.clone())),
        ExprKind::Field { base, name } => {
            let receiver_ident = match &base.kind {
                ExprKind::Ident(base_name) => Some(base_name.clone()),
                _ => None,
            };
            Some(CallShape::Field {
                receiver_ident,
                method: name.node.clone(),
            })
        }
        _ => None,
    }
}

/// One node of the package's call graph: a declaration, and the module that
/// declares it.
pub type Node = (String, FnKey);

/// How precisely a call site named the callee an edge leads to.
///
/// Both kinds of edge are sound for the capability fixed point, which only
/// ever unions more in. They differ for anything that reports an edge to a
/// person: an approximate edge may not exist in any real execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallPrecision {
    /// The call site names the callee: a free function, a module-qualified
    /// export, or a method of a receiver whose type is written at the call
    /// site.
    Exact,
    /// The receiver's type is not known without a type checker, so the call
    /// site was resolved to every same-named method reachable through
    /// imports. See [`FnEntry::required_capabilities`].
    Approximate,
}

/// Resolves every call site recorded while the modules were resolved to the
/// declarations it may reach, anywhere in the package.
///
/// The second map is what resolution could *not* do: the call sites whose
/// callee is a value rather than a declaration, keyed by the declaration
/// that wrote them. It is merged into each entry's `direct_open_calls`
/// before the fixed point runs, so a lower bound and the reason it is one
/// travel together.
#[allow(clippy::type_complexity)]
fn package_call_graph(
    program: &Program,
    call_sites: &BTreeMap<Node, Vec<CallShape>>,
) -> (
    BTreeMap<Node, BTreeMap<Node, CallPrecision>>,
    BTreeMap<Node, BTreeSet<OpenCall>>,
) {
    let reachable: BTreeMap<&str, BTreeSet<&str>> = program
        .modules
        .keys()
        .map(|name| (name.as_str(), reachable_modules(program, name)))
        .collect();
    let mut graph = BTreeMap::new();
    let mut open = BTreeMap::new();
    for ((module, key), calls) in call_sites {
        let (targets, unresolved) =
            resolve_calls(program, module, calls, &reachable[module.as_str()]);
        let node = (module.clone(), key.clone());
        if !unresolved.is_empty() {
            open.insert(node.clone(), unresolved);
        }
        graph.insert(node, targets);
    }
    (graph, open)
}

/// Records what resolution could not follow on the declarations that wrote
/// it, beside what each already found in its own body.
fn merge_open_calls(program: &mut Program, unresolved: &BTreeMap<Node, BTreeSet<OpenCall>>) {
    for (module, resolved) in program.modules.iter_mut() {
        for (name, entry) in resolved.functions.iter_mut() {
            if let Some(open) = unresolved.get(&(module.clone(), FnKey::Fn(name.clone()))) {
                entry.direct_open_calls.extend(open.iter().copied());
            }
        }
        for ((type_name, method_name), entry) in resolved.methods.iter_mut() {
            let key = FnKey::Method(type_name.clone(), method_name.clone());
            if let Some(open) = unresolved.get(&(module.clone(), key)) {
                entry.direct_open_calls.extend(open.iter().copied());
            }
        }
    }
}

/// Every module `module` can reach through imports, directly or through the
/// modules it imports, including itself.
///
/// This is the set a value's type can be declared by where `module` runs: a
/// declaration this module never mentions still arrives here through
/// something it does import. The walk terminates because it visits each
/// module once, so a cycle that resolution is about to reject cannot spin it.
fn reachable_modules<'a>(program: &'a Program, module: &'a str) -> BTreeSet<&'a str> {
    let mut reached: BTreeSet<&str> = BTreeSet::new();
    let mut pending: Vec<&str> = vec![module];
    while let Some(name) = pending.pop() {
        if !reached.insert(name) {
            continue;
        }
        if let Some(resolved) = program.modules.get(name) {
            pending.extend(resolved.dependencies());
        }
    }
    reached
}

/// Resolves the raw call sites found in one declaration's body to the
/// declarations they may call, in `module` or in any module `module` imports
/// from.
///
/// A bare-name call resolves to the free function of that name the calling
/// module declares, or, failing that, to the one it imported under that
/// name. A call qualified by an imported module (`booking.create(...)`)
/// resolves to that module's exported function. A field-access call whose
/// receiver is a bare identifier naming a struct or enum in scope — declared
/// here or imported — resolves precisely to that type's method, in the
/// module that declares the type.
///
/// Every other field-access call — a receiver that is `self`, a local
/// variable, or any other expression whose type is unknown without a type
/// checker — resolves to *every* method sharing that name in `reachable`,
/// the modules this one can reach through imports. That is a deliberate
/// over-approximation: it can name a capability a call site does not really
/// reach, but never misses one. `reachable` is transitive rather than
/// direct because a value can be declared by a module this one never
/// mentions and still arrive here, as the result of something it does
/// import.
///
/// A bare name that resolves to no declaration at all is the higher-order
/// case: `work()` where `work` is a parameter or a local. There is nothing
/// to draw an edge to, so the call site is reported as
/// [`OpenCall::FunctionValue`] in the second return value instead of
/// vanishing.
fn resolve_calls(
    program: &Program,
    module: &str,
    calls: &[CallShape],
    reachable: &BTreeSet<&str>,
) -> (BTreeMap<Node, CallPrecision>, BTreeSet<OpenCall>) {
    let Some(resolved) = program.modules.get(module) else {
        return (BTreeMap::new(), BTreeSet::new());
    };
    let mut targets: BTreeMap<Node, CallPrecision> = BTreeMap::new();
    let mut open: BTreeSet<OpenCall> = BTreeSet::new();
    for call in calls {
        match call {
            CallShape::Reference(name) => {
                if resolved.functions.contains_key(name) {
                    exact(&mut targets, (module.to_string(), FnKey::Fn(name.clone())));
                } else if let Some(owner) = declaring_module(program, resolved, name, |owner| {
                    owner.functions.contains_key(name)
                }) {
                    exact(&mut targets, (owner, FnKey::Fn(name.clone())));
                }
            }
            CallShape::Ident(name) => {
                if resolved.functions.contains_key(name) {
                    exact(&mut targets, (module.to_string(), FnKey::Fn(name.clone())));
                } else if let Some(owner) = declaring_module(program, resolved, name, |owner| {
                    owner.functions.contains_key(name)
                }) {
                    exact(&mut targets, (owner, FnKey::Fn(name.clone())));
                } else if calls_a_value(program, resolved, name) {
                    open.insert(OpenCall::FunctionValue);
                }
            }
            CallShape::Field {
                receiver_ident,
                method,
            } => {
                let owner = receiver_ident.as_ref().and_then(|head| {
                    declaring_module(program, resolved, head, |owner| {
                        owner.structs.contains_key(head) || owner.enums.contains_key(head)
                    })
                    .map(|owner| (owner, head.clone()))
                });
                if let Some((owner, type_name)) = owner {
                    for node in type_methods(program, &owner, &type_name, method) {
                        exact(&mut targets, node);
                    }
                    continue;
                }
                if let Some(target) = receiver_ident
                    .as_ref()
                    .and_then(|head| resolved.module_imports.get(head))
                {
                    if let Some(owner) = program.modules.get(target) {
                        if owner
                            .functions
                            .get(method)
                            .is_some_and(|entry| entry.exported)
                        {
                            exact(&mut targets, (target.clone(), FnKey::Fn(method.clone())));
                            continue;
                        }
                    }
                }
                for name in reachable {
                    let Some(candidate) = program.modules.get(*name) else {
                        continue;
                    };
                    for (type_name, method_name) in candidate.methods.keys() {
                        if method_name == method {
                            targets
                                .entry((
                                    (*name).to_string(),
                                    FnKey::Method(type_name.clone(), method_name.clone()),
                                ))
                                .or_insert(CallPrecision::Approximate);
                        }
                    }
                }
            }
        }
    }
    (targets, open)
}

/// Whether a bare `name(...)` that resolved to no function is a call to a
/// value a parameter or local holds.
///
/// A bare call is one of a short list of things — a declared function, a
/// struct initializer, a host item a `use console.println` brought into
/// scope, a builtin written without a receiver such as `Ok` or `assert`, a
/// builtin type used as a namespace, or a value. Only the last is indirect,
/// so everything else is ruled out by name before the call site is called
/// open. An enum or an unknown name written this way is a type error the
/// checker reports, and reporting it here as well would say the wrong thing
/// about it.
fn calls_a_value(program: &Program, resolved: &ResolvedModule, name: &str) -> bool {
    let names_a_type = |owner: &ResolvedModule| {
        owner.structs.contains_key(name)
            || owner.enums.contains_key(name)
            || owner.aliases.contains_key(name)
    };
    !(declaring_module(program, resolved, name, names_a_type).is_some()
        || resolved.host_items.contains_key(name)
        || cove_schema::builtins::builtin(name).is_some()
        || cove_schema::builtins::free_builtin(name).is_some()
        || name == cove_schema::builtins::NONE_CASE.name)
}

/// Records an edge whose callee the call site named, replacing an
/// approximate edge to the same callee: one call site naming it is enough to
/// make the edge real, whatever another site in the same body guessed.
fn exact(targets: &mut BTreeMap<Node, CallPrecision>, node: Node) {
    targets.insert(node, CallPrecision::Exact);
}

/// The declarations `type_module.type_name`'s method `method` may reach.
///
/// [`Program::methods_of`] knows where a type's methods live, including the
/// ones a conformance declared in another module supplies, so this is a
/// filter over it rather than a second search.
fn type_methods(
    program: &Program,
    type_module: &str,
    type_name: &str,
    method: &str,
) -> BTreeSet<Node> {
    program
        .methods_of(type_module, type_name)
        .into_iter()
        .filter(|declared| declared.name == method)
        .map(|declared| {
            (
                declared.module.to_string(),
                FnKey::Method(type_name.to_string(), method.to_string()),
            )
        })
        .collect()
}

/// The module that declares `name` as `resolved` sees it, when the
/// declaration it finds there satisfies `is_kind`.
fn declaring_module(
    program: &Program,
    resolved: &ResolvedModule,
    name: &str,
    is_kind: impl Fn(&ResolvedModule) -> bool,
) -> Option<String> {
    if is_kind(resolved) {
        return Some(resolved.name.clone());
    }
    let owner_name = resolved.imports.get(name)?;
    let owner = program.modules.get(owner_name)?;
    is_kind(owner).then(|| owner_name.clone())
}

/// Fills in `required_capabilities` on every function and method of the
/// package as the least fixed point of "start from what a declaration calls
/// directly, then union in whatever every declaration it (transitively)
/// calls requires."
///
/// The graph is the package's, not one module's: a function that reaches
/// `console.println` only through an imported helper requires `console`.
///
/// The same round carries [`FnEntry::open_calls`] outward: a declaration
/// that calls a capability-open one is capability-open too, since the
/// requirement its callee could not see is one it cannot see either. Both
/// facts have to travel together, or a report could show a complete-looking
/// set that was assembled out of an incomplete one.
///
/// A fixed point rather than a recursive walk is required because the call
/// graph can be cyclic: direct and mutual recursion must not recurse forever.
/// Module imports may not form a cycle, but calls within a module still may.
/// Each round only ever adds to two finite sets, so the loop is guaranteed to
/// terminate.
fn propagate_capabilities(
    program: &mut Program,
    call_graph: &BTreeMap<Node, BTreeMap<Node, CallPrecision>>,
) {
    let mut required: BTreeMap<Node, BTreeSet<Capability>> = BTreeMap::new();
    let mut open: BTreeMap<Node, BTreeSet<OpenCall>> = BTreeMap::new();
    for (module, resolved) in &program.modules {
        for (name, entry) in &resolved.functions {
            let node = (module.clone(), FnKey::Fn(name.clone()));
            required.insert(node.clone(), entry.direct_capabilities.clone());
            open.insert(node, entry.direct_open_calls.clone());
        }
        for ((type_name, method_name), entry) in &resolved.methods {
            let node = (
                module.clone(),
                FnKey::Method(type_name.clone(), method_name.clone()),
            );
            required.insert(node.clone(), entry.direct_capabilities.clone());
            open.insert(node, entry.direct_open_calls.clone());
        }
    }

    let keys: Vec<Node> = required.keys().cloned().collect();
    loop {
        let mut changed = false;
        for key in &keys {
            let Some(callees) = call_graph.get(key) else {
                continue;
            };
            let mut additions = BTreeSet::new();
            // One reason is enough for a caller: it says the set below it is
            // a floor, and the declaration that could not be followed says
            // which form it was.
            let mut reached_open = false;
            for callee in callees.keys() {
                if let Some(callee_open) = open.get(callee) {
                    reached_open |= !callee_open.is_empty();
                }
                let Some(callee_caps) = required.get(callee) else {
                    continue;
                };
                for cap in callee_caps {
                    if !required[key].contains(cap) {
                        additions.insert(cap.clone());
                    }
                }
            }
            if !additions.is_empty() {
                required.get_mut(key).unwrap().extend(additions);
                changed = true;
            }
            if reached_open && open.get_mut(key).unwrap().insert(OpenCall::ReachedOpenCall) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for (module, resolved) in program.modules.iter_mut() {
        for (name, entry) in resolved.functions.iter_mut() {
            let node = (module.clone(), FnKey::Fn(name.clone()));
            entry.required_capabilities = required.remove(&node).unwrap_or_default();
            entry.open_calls = open.remove(&node).unwrap_or_default();
        }
        for ((type_name, method_name), entry) in resolved.methods.iter_mut() {
            let node = (
                module.clone(),
                FnKey::Method(type_name.clone(), method_name.clone()),
            );
            entry.required_capabilities = required.remove(&node).unwrap_or_default();
            entry.open_calls = open.remove(&node).unwrap_or_default();
        }
    }
}

/// If `callee` is a call to a host module (`console.println(...)`) or an
/// unqualified host item (`println(...)`), the capability it requires.
///
/// The capability is the one the *operation's* schema declares, not the
/// module's, because that is the rule `HostRegistry::call_with` and
/// `call_resource` enforce at the boundary: they read
/// `OperationSchema::capability` and fall back to the module's only when the
/// operation itself is not in the schema. Deriving the module's capability
/// here instead would let the checker under-report what a call costs — an
/// embedder may gate one operation of a module more tightly than the module
/// as a whole, such as a `company` module whose `directory` operations need
/// only `directory` while `payroll` needs `payroll` — and a program that
/// requested only what the checker asked for would then be refused at run
/// time. A module no schema describes falls back to its name, which is the
/// only thing about it this compilation knows.
fn call_capability(
    callee: &Expr,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
    schemas: &HostSchemas,
) -> Option<Capability> {
    let (module, operation) = match &callee.kind {
        ExprKind::Field { base, name } => match &base.kind {
            ExprKind::Ident(module_name) if host_uses.contains(module_name.as_str()) => {
                (module_name.clone(), name.node.clone())
            }
            _ => return None,
        },
        ExprKind::Ident(name) => (host_items.get(name)?.clone(), name.clone()),
        _ => return None,
    };
    Some(operation_capability(&module, &operation, schemas))
}

/// The capability a call to `module`'s `operation` requires, matching the
/// rule the Host API boundary enforces: the operation's own capability when
/// its schema declares one, the module's otherwise, and the module's name
/// when no schema describes the module at all.
fn operation_capability(module: &str, operation: &str, schemas: &HostSchemas) -> Capability {
    match schemas.module(module) {
        Some(schema) => match schema.operation(operation) {
            Some(op) => Capability::new(op.capability),
            None => Capability::new(schema.capability),
        },
        None => Capability::new(module),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::package::{Module, Unit};
    use cove_diag::SourceMap;
    use cove_schema::{Effect, HostType, ModuleSchema, OperationSchema};
    use std::path::PathBuf;

    /// Builds a single module out of inline source texts, one per unit,
    /// without touching the filesystem.
    fn module_from_sources(name: &str, sources_text: &[&str]) -> Module {
        let mut sources = SourceMap::new();
        let mut units = Vec::new();
        for (i, text) in sources_text.iter().enumerate() {
            let path = PathBuf::from(format!("{name}{i}.cove"));
            let file = sources.add(path.clone(), *text);
            let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
            units.push(Unit { file, path, ast });
        }
        Module {
            name: name.to_string(),
            dir: PathBuf::from(name),
            units,
        }
    }

    fn package_of(module: Module) -> Package {
        package_of_modules(vec![module])
    }

    /// Builds a package out of several inline modules, so a `use` in one can
    /// be answered by another.
    fn package_of_modules(modules: Vec<Module>) -> Package {
        let mut map = BTreeMap::new();
        for module in modules {
            map.insert(module.name.clone(), module);
        }
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: map,
        }
    }

    /// Resolves a package of inline modules, one source per module.
    fn resolve_modules(modules: &[(&str, &str)]) -> Result<Program, Vec<Diagnostic>> {
        let package = package_of_modules(
            modules
                .iter()
                .map(|(name, source)| module_from_sources(name, &[source]))
                .collect(),
        );
        resolve(&package)
    }

    #[track_caller]
    fn resolve_ok(modules: &[(&str, &str)]) -> Program {
        match resolve_modules(modules) {
            Ok(program) => program,
            Err(errors) => panic!(
                "expected the package to resolve, found: {}",
                errors
                    .iter()
                    .map(|d| format!("{}: {}", d.code, d.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }

    /// Resolves a package expected to fail, and returns the one diagnostic
    /// with `code`.
    #[track_caller]
    fn resolve_err(modules: &[(&str, &str)], code: &str) -> Diagnostic {
        let errors = resolve_modules(modules).expect_err("expected the package to be rejected");
        errors
            .into_iter()
            .find(|d| d.code == code)
            .unwrap_or_else(|| panic!("expected a `{code}` diagnostic"))
    }

    /// Resolves a package of inline modules against a Host API schema an
    /// embedder supplied, rather than the shipped set alone.
    fn resolve_modules_with(
        modules: &[(&str, &str)],
        schemas: &HostSchemas,
    ) -> Result<Program, Vec<Diagnostic>> {
        let package = package_of_modules(
            modules
                .iter()
                .map(|(name, source)| module_from_sources(name, &[source]))
                .collect(),
        );
        resolve_with(&package, schemas)
    }

    #[track_caller]
    fn resolve_ok_with(modules: &[(&str, &str)], schemas: &HostSchemas) -> Program {
        match resolve_modules_with(modules, schemas) {
            Ok(program) => program,
            Err(errors) => panic!(
                "expected the package to resolve, found: {}",
                errors
                    .iter()
                    .map(|d| format!("{}: {}", d.code, d.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }

    #[test]
    fn records_a_test_and_leaves_it_module_private() {
        let program = resolve_ok(&[(
            "text",
            "fn wordCount(text: String) -> Int {\n  text.words().length()\n}\n\n             test fn countsWords() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        )]);
        let entry = &program.modules["text"].functions["countsWords"];
        assert!(entry.is_test);
        assert!(!entry.exported);
        assert!(!program.modules["text"]
            .exports()
            .contains(&"countsWords".to_string()));

        let tests = program.tests();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].qualified_name(), "text.countsWords");
    }

    #[test]
    fn lists_every_test_of_the_package_in_module_then_name_order() {
        let program = resolve_ok(&[
            (
                "second",
                "test fn b() -> Result<Unit, Error> {\n  Ok(())\n}\n\n                 test fn a() -> Result<Unit, Error> {\n  Ok(())\n}\n",
            ),
            (
                "first",
                "test fn c() -> Result<Unit, Error> {\n  Ok(())\n}\n",
            ),
        ]);
        let names: Vec<String> = program
            .tests()
            .iter()
            .map(DeclaredTest::qualified_name)
            .collect();
        assert_eq!(names, ["first.c", "second.a", "second.b"]);
    }

    #[test]
    fn a_test_requires_the_capabilities_its_call_graph_reaches() {
        let program = resolve_ok(&[(
            "text",
            "use console.println\n\n             fn report(text: String) -> Result<Unit, Error> {\n  println(text)\n}\n\n             test fn reports() -> Result<Unit, Error> {\n  report(\"a\")?\n  Ok(())\n}\n\n             test fn countsNothing() -> Result<Unit, Error> {\n  Ok(())\n}\n",
        )]);
        let required = |name: &str| -> Vec<String> {
            program.modules["text"].functions[name]
                .required_capabilities
                .iter()
                .map(Capability::to_string)
                .collect()
        };
        // Derived from the call graph exactly as any other function's are:
        // the test names no host module itself.
        assert_eq!(required("reports"), ["console".to_string()]);
        assert!(required("countsNothing").is_empty());
    }

    #[test]
    fn a_test_may_call_its_modules_private_declarations() {
        let program = resolve_ok(&[(
            "text",
            "fn secret() -> Int {\n  7\n}\n\n             test fn seesSecret() -> Result<Unit, Error> {\n  secret()\n  Ok(())\n}\n",
        )]);
        let edges = &program.call_graph[&("text".to_string(), FnKey::Fn("seesSecret".to_string()))];
        assert!(edges.contains_key(&("text".to_string(), FnKey::Fn("secret".to_string()))));
    }

    #[test]
    fn merges_two_units_of_the_same_module() {
        let module = module_from_sources(
            "greet",
            &[
                "/// Greets by name.\nexport fn greet(name: String) -> String {\n  name\n}\n",
                "/// Says goodbye.\nexport fn farewell(name: String) -> String {\n  name\n}\n",
            ],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let resolved = &program.modules["greet"];
        assert!(resolved.functions.contains_key("greet"));
        assert!(resolved.functions.contains_key("farewell"));
    }

    #[test]
    fn reports_duplicate_declaration_across_units() {
        let module = module_from_sources(
            "dup",
            &[
                "/// First.\nexport fn greet(name: String) -> String {\n  name\n}\n",
                "/// Second.\nexport fn greet(name: String) -> String {\n  name\n}\n",
            ],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::duplicate_declaration"));
    }

    #[test]
    fn resolves_impl_methods() {
        let module = module_from_sources(
            "booking",
            &[
                "/// A booking.\nexport struct Booking {\n  id: String\n}\n\nimpl Booking {\n  /// Returns the id.\n  fn id(self) -> String {\n    self.id\n  }\n}\n",
            ],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let resolved = &program.modules["booking"];
        let method = resolved
            .methods
            .get(&("Booking".to_string(), "id".to_string()))
            .expect("method resolved");
        assert_eq!(method.receiver_type.as_deref(), Some("Booking"));
    }

    /// The trait and the two types every conformance test below builds on.
    const TRAIT_SOURCE: &str = "\
/// Renders itself.
export trait Display {
  /// The full form.
  fn describe(self) -> String

  /// A short form, defaulting to the full one.
  fn label(self) -> String { self.describe() }
}

/// A booking.
export struct Booking(id: Int)

/// A receipt.
export struct Receipt(total: Int)
";

    fn resolved_of(name: &str, sources: &[&str]) -> ResolvedModule {
        let package = package_of(module_from_sources(name, sources));
        let mut program = resolve(&package).expect("resolves");
        program.modules.remove(name).expect("the module resolves")
    }

    fn resolve_errors(name: &str, sources: &[&str]) -> Vec<Diagnostic> {
        let package = package_of(module_from_sources(name, sources));
        resolve(&package).expect_err("expected resolution to fail")
    }

    fn has_code(diagnostics: &[Diagnostic], code: &str) -> bool {
        diagnostics.iter().any(|d| d.code == code)
    }

    #[test]
    fn records_a_conformance_and_the_methods_it_supplies() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Booking {{\n  fn describe(self) -> String {{ \"b\" }}\n  fn label(self) -> String {{ \"#\" }}\n}}\n"
        );
        let resolved = resolved_of("render", &[&source]);
        let conformance = resolved
            .conformances
            .get(&("Display".to_string(), "Booking".to_string()))
            .expect("the conformance is recorded");
        assert_eq!(
            conformance.methods.iter().cloned().collect::<Vec<_>>(),
            ["describe", "label"]
        );
        // A conformance's methods are ordinary methods of the type, which is
        // what lets dispatch find them without asking where they came from.
        assert!(resolved
            .methods
            .contains_key(&("Booking".to_string(), "describe".to_string())));
    }

    #[test]
    fn a_defaulted_method_becomes_the_type_s_own_method() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Receipt {{\n  fn describe(self) -> String {{ \"r\" }}\n}}\n"
        );
        let resolved = resolved_of("render", &[&source]);
        let label = resolved
            .methods
            .get(&("Receipt".to_string(), "label".to_string()))
            .expect("the default body is recorded as a method");
        assert_eq!(label.receiver_type.as_deref(), Some("Receipt"));
        assert_eq!(
            label.doc.as_deref(),
            Some("A short form, defaulting to the full one.")
        );
    }

    #[test]
    fn rejects_a_conformance_missing_a_required_method() {
        let source = format!("{TRAIT_SOURCE}\nimpl Display for Booking {{\n}}\n");
        let errors = resolve_errors("render", &[&source]);
        assert!(has_code(&errors, "cove::resolve::missing_trait_method"));
        assert!(errors[0].message.contains("`describe`"));
        // `label` has a default, so it is not missing.
        assert!(!errors[0].message.contains("`label`"));
    }

    #[test]
    fn rejects_a_method_the_trait_does_not_declare() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Booking {{\n  fn describe(self) -> String {{ \"b\" }}\n  fn extra(self) -> Int {{ 1 }}\n}}\n"
        );
        let errors = resolve_errors("render", &[&source]);
        assert!(has_code(&errors, "cove::resolve::unknown_trait_method"));
    }

    #[test]
    fn rejects_the_same_conformance_twice() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Booking {{\n  fn describe(self) -> String {{ \"b\" }}\n}}\n\nimpl Display for Booking {{\n  fn describe(self) -> String {{ \"c\" }}\n}}\n"
        );
        let errors = resolve_errors("render", &[&source]);
        assert!(has_code(&errors, "cove::resolve::duplicate_conformance"));
    }

    #[test]
    fn rejects_a_trait_method_that_collides_with_an_inherent_method() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Booking {{\n  fn describe(self) -> String {{ \"b\" }}\n}}\n\nimpl Booking {{\n  /// Also describes.\n  fn describe(self) -> String {{ \"c\" }}\n}}\n"
        );
        let errors = resolve_errors("render", &[&source]);
        assert!(has_code(&errors, "cove::resolve::duplicate_declaration"));
    }

    #[test]
    fn the_orphan_rule_allows_a_local_trait_or_a_local_type() {
        // The trait is local, the type is not declared here at all: the
        // orphan rule is satisfied, and what fails is the separate rule that
        // an `impl` extends a struct or enum of this module.
        let source = format!("{TRAIT_SOURCE}\nimpl Display for Int {{\n  fn describe(self) -> String {{ \"i\" }}\n}}\n");
        let errors = resolve_errors("render", &[&source]);
        assert!(has_code(&errors, "cove::resolve::unknown_impl_type"));
        assert!(!has_code(&errors, "cove::resolve::orphan_conformance"));
    }

    #[test]
    fn rejects_a_conformance_between_two_types_the_module_does_not_declare() {
        let errors = resolve_errors(
            "elsewhere",
            &["impl Display for Int {\n  fn describe(self) -> String { \"i\" }\n}\n"],
        );
        assert!(has_code(&errors, "cove::resolve::orphan_conformance"));
    }

    // ------------------------------------------------- the builtin `Snapshot`

    #[test]
    fn impl_snapshot_records_a_conformance_with_no_trait_declaration_in_source() {
        let source = "\
/// A booking.
export struct Booking(id: Int)

impl Snapshot for Booking {
  /// Returns a copy of this booking.
  fn snapshot(self) -> Booking { self }
}
";
        let resolved = resolved_of("booking", &[source]);
        let conformance = resolved
            .conformances
            .get(&("Snapshot".to_string(), "Booking".to_string()))
            .expect("the conformance is recorded even though no `trait Snapshot` was written");
        assert_eq!(
            conformance.methods.iter().cloned().collect::<Vec<_>>(),
            ["snapshot"]
        );
        assert!(resolved
            .methods
            .contains_key(&("Booking".to_string(), "snapshot".to_string())));
        // `Snapshot` itself is not a declaration this module makes: it
        // belongs to no module.
        assert!(!resolved.traits.contains_key("Snapshot"));
    }

    #[test]
    fn impl_snapshot_still_requires_the_snapshot_method() {
        let source = "\
/// A booking.
export struct Booking(id: Int)

impl Snapshot for Booking {
}
";
        let errors = resolve_errors("booking", &[source]);
        assert!(has_code(&errors, "cove::resolve::missing_trait_method"));
        assert!(errors[0].message.contains("`snapshot`"));
    }

    #[test]
    fn a_third_module_may_not_conform_an_imported_type_to_snapshot() {
        // `Snapshot` belongs to no module, so the orphan rule's only way to
        // pass is the type's own module declaring it; a module that merely
        // imports `Booking` cannot conform it.
        let error = resolve_err(
            &[
                (
                    "booking",
                    "/// A booking.\nexport struct Booking(id: Int)\n",
                ),
                (
                    "other",
                    "use booking.Booking\n\nimpl Snapshot for Booking {\n  fn snapshot(self) -> Booking { self }\n}\n",
                ),
            ],
            "cove::resolve::orphan_conformance",
        );
        assert!(error.message.contains("Snapshot"));
        assert!(error.message.contains("Booking"));
    }

    #[test]
    fn warns_on_an_exported_trait_and_its_methods_without_docs() {
        let package = package_of(module_from_sources(
            "render",
            &["export trait Display {\n  fn describe(self) -> String\n}\n"],
        ));
        let program = resolve(&package).expect("resolves");
        let names: Vec<&str> = program
            .notices
            .iter()
            .filter(|d| d.code == "cove::resolve::missing_doc")
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "exported `Display` has no doc comment",
                "exported `Display.describe` has no doc comment"
            ]
        );
    }

    #[test]
    fn a_defaulted_method_is_marked_as_coming_from_its_trait() {
        let source = format!(
            "{TRAIT_SOURCE}\nimpl Display for Receipt {{\n  fn describe(self) -> String {{ \"r\" }}\n}}\n"
        );
        let resolved = resolved_of("render", &[&source]);
        let methods = &resolved.methods;
        assert_eq!(
            methods[&("Receipt".to_string(), "label".to_string())]
                .from_trait_default
                .as_deref(),
            Some("Display")
        );
        assert!(methods[&("Receipt".to_string(), "describe".to_string())]
            .from_trait_default
            .is_none());
    }

    #[test]
    fn a_default_body_s_match_is_checked_once_however_many_types_conform() {
        let source = "\
/// A signal.
enum Signal {
  Red
  Green
}

/// Shows itself.
trait Show {
  /// The signal.
  fn signal(self) -> Signal

  /// A name, from a `match` that misses a case.
  fn name(self) -> String {
    match self.signal() {
      Signal.Red => \"red\"
    }
  }
}

/// One.
struct A(x: Int)

/// Two.
struct B(x: Int)

impl Show for A {
  fn signal(self) -> Signal { Signal.Red }
}

impl Show for B {
  fn signal(self) -> Signal { Signal.Green }
}
";
        let errors = resolve_errors("show", &[source]);
        assert_eq!(
            errors
                .iter()
                .filter(|d| d.code == "cove::resolve::non_exhaustive_match")
                .count(),
            1
        );
    }

    #[test]
    fn a_conformance_method_propagates_its_capabilities() {
        let source = format!(
            "use console.println\n\n{TRAIT_SOURCE}\nimpl Display for Booking {{\n  fn describe(self) -> String {{\n    console.println(\"b\")\n    \"b\"\n  }}\n}}\n"
        );
        let resolved = resolved_of("render", &[&source]);
        let describe = &resolved.methods[&("Booking".to_string(), "describe".to_string())];
        assert!(describe
            .required_capabilities
            .iter()
            .any(|c| c.to_string() == "console"));
    }

    #[test]
    fn rejects_impl_for_unknown_type() {
        let module = module_from_sources("orphan", &["impl Nothing {\n  fn go(self) {\n  }\n}\n"]);
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::unknown_impl_type"));
    }

    #[test]
    fn rejects_non_fn_impl_items() {
        let module = module_from_sources(
            "badimpl",
            &[
                "export struct Thing {\n  x: Int\n}\n\nimpl Thing {\n  struct Nested {\n    y: Int\n  }\n}\n",
            ],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::invalid_impl_item"));
    }

    #[test]
    fn one_segment_use_records_a_host_use() {
        let module = module_from_sources("hostuse", &["use http\n\nexport fn main() {\n}\n"]);
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(program.modules["hostuse"].host_uses.contains("http"));
    }

    #[test]
    fn two_segment_use_records_use_and_item() {
        let module = module_from_sources(
            "hostitem",
            &["use console.println\n\nexport fn main() {\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let resolved = &program.modules["hostitem"];
        assert!(resolved.host_uses.contains("console"));
        assert_eq!(
            resolved.host_items.get("println").map(String::as_str),
            Some("console")
        );
    }

    #[test]
    fn a_use_matching_no_module_and_no_host_path_is_rejected() {
        let diagnostic = resolve_err(&[("toolong", "use a.b.c\n")], "cove::resolve::unknown_use");
        assert!(diagnostic.message.contains("a.b.c"));
        // The message names both things the compiler looked for.
        assert!(diagnostic.message.contains("module"));
        assert!(diagnostic.message.contains("host module"));
    }

    // ------------------------------------------------------- module imports

    #[test]
    fn a_use_imports_an_exported_declaration() {
        let program = resolve_ok(&[
            (
                "greet",
                "/// Greets by name.\nexport fn greeting(name: String) -> String {\n  name\n}\n",
            ),
            (
                "hello",
                "use greet.greeting\n\n/// Entry point.\nexport fn main() -> String {\n  greeting(\"world\")\n}\n",
            ),
        ]);
        assert_eq!(
            program.modules["hello"].imports.get("greeting"),
            Some(&"greet".to_string())
        );
        assert!(program.modules["hello"].module_imports.is_empty());
        // A module of this package is not a host module, so nothing was
        // recorded as one.
        assert!(program.modules["hello"].host_uses.is_empty());
    }

    #[test]
    fn a_use_of_a_module_alone_imports_the_module() {
        let program = resolve_ok(&[
            (
                "greet",
                "/// Greets by name.\nexport fn greeting(name: String) -> String {\n  name\n}\n",
            ),
            (
                "hello",
                "use greet\n\n/// Entry point.\nexport fn main() -> String {\n  greet.greeting(\"world\")\n}\n",
            ),
        ]);
        assert_eq!(
            program.modules["hello"].module_imports.get("greet"),
            Some(&"greet".to_string())
        );
        assert!(program.modules["hello"].imports.is_empty());
        assert!(program.modules["hello"].host_uses.is_empty());
    }

    #[test]
    fn a_nested_module_is_imported_by_its_full_path() {
        let program = resolve_ok(&[
            (
                "src.booking",
                "/// Creates a booking.\nexport fn createBooking() -> String {\n  \"b\"\n}\n",
            ),
            (
                "app",
                "use src.booking.createBooking\n\n/// Entry point.\nexport fn main() -> String {\n  createBooking()\n}\n",
            ),
        ]);
        assert_eq!(
            program.modules["app"].imports.get("createBooking"),
            Some(&"src.booking".to_string())
        );
    }

    #[test]
    fn a_use_of_a_private_declaration_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "greet",
                    "fn greeting(name: String) -> String {\n  name\n}\n",
                ),
                ("hello", "use greet.greeting\n"),
            ],
            "cove::resolve::private_declaration",
        );
        assert!(diagnostic.message.contains("not exported"));
        // The declaration itself is labelled, and `export` is the fix.
        assert_eq!(diagnostic.labels.len(), 1);
        assert!(diagnostic.help.as_deref().unwrap().contains("export"));
    }

    #[test]
    fn a_use_naming_a_module_that_declares_no_such_name_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "greet",
                    "/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n",
                ),
                ("hello", "use greet.farewell\n"),
            ],
            "cove::resolve::unknown_use",
        );
        assert!(diagnostic.message.contains("declares no `farewell`"));
        assert!(diagnostic.message.contains("not a host module"));
        assert!(diagnostic.help.as_deref().unwrap().contains("greeting"));
    }

    #[test]
    fn a_module_named_after_a_host_module_is_rejected_rather_than_preferred() {
        let diagnostic = resolve_err(
            &[
                (
                    "console",
                    "/// Prints.\nexport fn println(line: String) {\n}\n",
                ),
                ("app", "use console.println\n"),
            ],
            "cove::resolve::module_shadows_host",
        );
        assert!(diagnostic.help.as_deref().unwrap().contains("rename"));
    }

    /// Every host module the shipped schema describes is refused as a
    /// package module, or a package module of that name shadows it silently
    /// -- modules resolve first.
    ///
    /// The loop reads [`host_modules`] rather than repeating it, because a
    /// second copy of the list is a second place for a host to go missing:
    /// `http` was absent from both for as long as this test spelled its own
    /// names out.
    #[test]
    fn every_shipped_host_module_is_refused_as_a_package_module() {
        for host in host_modules(&HostSchemas::new()) {
            let diagnostic = resolve_err(
                &[
                    (host, "/// Does something.\nexport fn thing() {\n}\n"),
                    ("app", &format!("use {host}.thing\n")),
                ],
                "cove::resolve::module_shadows_host",
            );
            assert!(
                diagnostic.message.contains(host),
                "`{host}` should be refused as a package module"
            );
        }
    }

    #[test]
    fn a_use_naming_both_a_module_and_a_declaration_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "booking",
                    "/// Creates a booking.\nexport fn create() -> String {\n  \"b\"\n}\n",
                ),
                (
                    "booking.create",
                    "/// Validates a booking.\nexport fn validate() -> Bool {\n  true\n}\n",
                ),
                ("app", "use booking.create\n"),
            ],
            "cove::resolve::ambiguous_use",
        );
        assert!(diagnostic.message.contains("both"));
    }

    #[test]
    fn an_import_colliding_with_a_declaration_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "greet",
                    "/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n",
                ),
                (
                    "hello",
                    "use greet.greeting\n\nfn greeting() -> String {\n  \"other\"\n}\n",
                ),
            ],
            "cove::resolve::import_conflict",
        );
        assert!(diagnostic.message.contains("also declares it"));
    }

    #[test]
    fn two_imports_of_one_name_from_different_modules_are_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "left",
                    "/// Greets.\nexport fn greeting() -> String {\n  \"l\"\n}\n",
                ),
                (
                    "right",
                    "/// Greets.\nexport fn greeting() -> String {\n  \"r\"\n}\n",
                ),
                ("hello", "use left.greeting\nuse right.greeting\n"),
            ],
            "cove::resolve::import_conflict",
        );
        assert!(diagnostic.message.contains("both"));
    }

    #[test]
    fn importing_the_same_declaration_twice_is_not_a_conflict() {
        let package = package_of_modules(vec![
            module_from_sources(
                "greet",
                &["/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n"],
            ),
            module_from_sources("hello", &["use greet.greeting\n", "use greet.greeting\n"]),
        ]);
        resolve(&package).expect("resolves");
    }

    #[test]
    fn an_unknown_two_segment_use_is_still_a_host_path() {
        let program = resolve_ok(&[("app", "use other.println\n")]);
        assert!(program.modules["app"].host_uses.contains("other"));
        assert_eq!(
            program.modules["app"].host_items.get("println"),
            Some(&"other".to_string())
        );
    }

    /// A package where the same undescribed host module is named by two
    /// `use`s in one module (unqualified, then qualified to an item) *and*
    /// by a `use` in a second module gets one `unchecked_host` warning, not
    /// three: repeating the warning per `use` would inflate a
    /// `cove check --deny-warnings` count for one unknown module.
    ///
    /// The single warning is pinned to the first `use` of the
    /// alphabetically first module that names the module, because
    /// `package.modules` is a `BTreeMap`: module `a` sorts before `b`, and
    /// within `a` its first unit's `use company` sorts before its second
    /// unit's `use company.employee`.
    #[test]
    fn unchecked_host_warns_once_per_module_not_once_per_use() {
        let module_a = module_from_sources(
            "a",
            &[
                "use company\n\n/// Calls into `company`.\nexport fn f() {\n  company.employee()\n}\n",
                "use company.employee\n\n/// Calls the unqualified import.\nexport fn g() {\n  employee()\n}\n",
            ],
        );
        let first_use_file = module_a.units[0].file;
        let module_b = module_from_sources(
            "b",
            &["use company\n\n/// Also calls into `company`.\nexport fn h() {\n  company.employee()\n}\n"],
        );
        let package = package_of_modules(vec![module_a, module_b]);
        let program = resolve(&package).expect("resolves despite the unchecked host warning");
        let warnings: Vec<_> = program
            .notices
            .iter()
            .filter(|d| d.code == "cove::resolve::unchecked_host")
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one unchecked_host warning, found {warnings:?}"
        );
        assert_eq!(
            warnings[0].primary.expect("warning has a span").file,
            first_use_file,
            "the warning should point at module `a`'s first `use company`"
        );
    }

    #[test]
    fn unchecked_host_warns_once_per_distinct_module() {
        let program = resolve_ok(&[
            (
                "a",
                "use company\n\n/// Calls into `company`.\nexport fn f() {\n  company.employee()\n}\n",
            ),
            (
                "b",
                "use vendor\n\n/// Calls into `vendor`.\nexport fn g() {\n  vendor.order()\n}\n",
            ),
        ]);
        let modules_warned: BTreeSet<&str> = program
            .notices
            .iter()
            .filter(|d| d.code == "cove::resolve::unchecked_host")
            .map(|d| {
                if d.message.contains("`company`") {
                    "company"
                } else if d.message.contains("`vendor`") {
                    "vendor"
                } else {
                    panic!("unexpected unchecked_host warning: {}", d.message)
                }
            })
            .collect();
        assert_eq!(
            modules_warned,
            BTreeSet::from(["company", "vendor"]),
            "two distinct undescribed host modules should each warn once"
        );
    }

    // -------------------------------------------------------------- cycles

    #[test]
    fn a_direct_import_cycle_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "a",
                    "use b.fromB\n\n/// Exported.\nexport fn fromA() -> Int {\n  1\n}\n",
                ),
                (
                    "b",
                    "use a.fromA\n\n/// Exported.\nexport fn fromB() -> Int {\n  2\n}\n",
                ),
            ],
            "cove::resolve::import_cycle",
        );
        assert!(
            diagnostic.message.contains("a -> b -> a")
                || diagnostic.message.contains("b -> a -> b")
        );
    }

    #[test]
    fn a_transitive_import_cycle_is_rejected() {
        let diagnostic = resolve_err(
            &[
                (
                    "a",
                    "use b.fromB\n\n/// Exported.\nexport fn fromA() -> Int {\n  1\n}\n",
                ),
                (
                    "b",
                    "use c.fromC\n\n/// Exported.\nexport fn fromB() -> Int {\n  2\n}\n",
                ),
                (
                    "c",
                    "use a.fromA\n\n/// Exported.\nexport fn fromC() -> Int {\n  3\n}\n",
                ),
            ],
            "cove::resolve::import_cycle",
        );
        assert!(diagnostic.message.contains(" -> "));
        assert_eq!(
            resolve_modules(&[
                (
                    "a",
                    "use b.fromB\n\n/// Exported.\nexport fn fromA() -> Int {\n  1\n}\n",
                ),
                (
                    "b",
                    "use c.fromC\n\n/// Exported.\nexport fn fromB() -> Int {\n  2\n}\n",
                ),
                (
                    "c",
                    "use a.fromA\n\n/// Exported.\nexport fn fromC() -> Int {\n  3\n}\n",
                ),
            ])
            .unwrap_err()
            .iter()
            .filter(|d| d.code == "cove::resolve::import_cycle")
            .count(),
            1,
            "one cycle is reported once, however many modules it runs through"
        );
    }

    #[test]
    fn a_module_importing_itself_is_a_cycle() {
        resolve_err(
            &[(
                "a",
                "use a.fromA\n\n/// Exported.\nexport fn fromA() -> Int {\n  1\n}\n",
            )],
            "cove::resolve::import_cycle",
        );
    }

    /// A diamond is ordinary: importing a module runs none of its code, so
    /// two modules may import a third without any ordering question.
    #[test]
    fn a_diamond_import_is_accepted() {
        let program = resolve_ok(&[
            (
                "base",
                "/// The shared helper.\nexport fn base() -> Int {\n  1\n}\n",
            ),
            (
                "left",
                "use base.base\n\n/// Exported.\nexport fn left() -> Int {\n  base()\n}\n",
            ),
            (
                "right",
                "use base.base\n\n/// Exported.\nexport fn right() -> Int {\n  base()\n}\n",
            ),
            (
                "top",
                "use left.left\nuse right.right\n\n/// Exported.\nexport fn top() -> Int {\n  left() + right()\n}\n",
            ),
        ]);
        assert_eq!(program.modules.len(), 4);
    }

    // ------------------------------------------- capabilities across modules

    #[test]
    fn required_capabilities_cross_a_module_boundary() {
        let program = resolve_ok(&[
            (
                "log",
                "use console.println\n\n/// Logs a message.\nexport fn log(msg: String) {\n  console.println(msg)\n}\n",
            ),
            (
                "app",
                "use log.log\n\n/// Entry point; never names a host module.\nexport fn main() {\n  log(\"hi\")\n}\n",
            ),
        ]);
        let main = &program.modules["app"].functions["main"];
        assert!(main.direct_capabilities.is_empty());
        assert!(main
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn required_capabilities_cross_a_qualified_module_call() {
        let program = resolve_ok(&[
            (
                "log",
                "use console.println\n\n/// Logs a message.\nexport fn log(msg: String) {\n  console.println(msg)\n}\n",
            ),
            (
                "app",
                "use log\n\n/// Entry point.\nexport fn main() {\n  log.log(\"hi\")\n}\n",
            ),
        ]);
        assert!(program.modules["app"].functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn required_capabilities_cross_two_module_boundaries() {
        let program = resolve_ok(&[
            (
                "bottom",
                "use console.println\n\n/// Logs.\nexport fn log(msg: String) {\n  console.println(msg)\n}\n",
            ),
            (
                "middle",
                "use bottom.log\n\n/// Logs twice.\nexport fn twice(msg: String) {\n  log(msg)\n  log(msg)\n}\n",
            ),
            (
                "top",
                "use middle.twice\n\n/// Entry point.\nexport fn main() {\n  twice(\"hi\")\n}\n",
            ),
        ]);
        assert!(program.modules["top"].functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn required_capabilities_cross_an_imported_type_s_method() {
        let program = resolve_ok(&[
            (
                "thing",
                "use console.println\n\n/// A thing.\nexport struct Thing {\n  id: String\n}\n\n\
                 impl Thing {\n  /// Prints the id.\n  fn touch(self) {\n    console.println(self.id)\n  }\n}\n",
            ),
            (
                "app",
                "use thing.Thing\n\n/// Entry point.\nexport fn main() {\n  Thing.touch()\n}\n",
            ),
        ]);
        assert!(program.modules["app"].functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    /// A receiver whose type the resolver cannot know reaches every method
    /// of that name in this module *and* in the modules it imports, which is
    /// what keeps the over-approximation sound across a boundary.
    #[test]
    fn an_unknown_receiver_reaches_an_imported_type_s_method() {
        let program = resolve_ok(&[
            (
                "thing",
                "use console.println\n\n/// A thing.\nexport struct Thing {\n  id: String\n}\n\n\
                 impl Thing {\n  /// Prints the id.\n  fn touch(self) {\n    console.println(self.id)\n  }\n}\n",
            ),
            (
                "app",
                "use thing.Thing\n\n/// Entry point.\nexport fn main(value: Thing) {\n  value.touch()\n}\n",
            ),
        ]);
        assert!(program.modules["app"].functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn a_module_that_imports_nothing_requires_nothing_from_its_neighbours() {
        let program = resolve_ok(&[
            (
                "log",
                "use console.println\n\n/// Logs a message.\nexport fn log(msg: String) {\n  console.println(msg)\n}\n",
            ),
            (
                "pure",
                "/// Adds.\nexport fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n",
            ),
        ]);
        assert!(program.modules["pure"].functions["add"]
            .required_capabilities
            .is_empty());
    }

    // --------------------------------------------- imported enums in `match`

    #[test]
    fn match_over_an_imported_enum_is_checked_for_exhaustiveness() {
        let diagnostic = resolve_err(
            &[
                (
                    "levels",
                    "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n  Warn\n}\n",
                ),
                (
                    "app",
                    "use levels.LogLevel\n\n/// Describes a level.\nexport fn describe(level: LogLevel) -> String {\n  \
                     match level {\n    LogLevel.Debug => \"debug\"\n    LogLevel.Info => \"info\"\n  }\n}\n",
                ),
            ],
            "cove::resolve::non_exhaustive_match",
        );
        assert!(diagnostic.message.contains("LogLevel.Warn"));
    }

    #[test]
    fn match_covering_every_case_of_an_imported_enum_passes() {
        let program = resolve_ok(&[
            (
                "levels",
                "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n}\n",
            ),
            (
                "app",
                "use levels.LogLevel\n\n/// Describes a level.\nexport fn describe(level: LogLevel) -> String {\n  \
                 match level {\n    LogLevel.Debug => \"debug\"\n    LogLevel.Info => \"info\"\n  }\n}\n",
            ),
        ]);
        assert!(program.notices.is_empty());
    }

    #[test]
    fn an_unknown_case_of_an_imported_enum_is_reported() {
        let diagnostic = resolve_err(
            &[
                (
                    "levels",
                    "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n}\n",
                ),
                (
                    "app",
                    "use levels.LogLevel\n\n/// Describes a level.\nexport fn describe(level: LogLevel) -> String {\n  \
                     match level {\n    LogLevel.Debug => \"debug\"\n    LogLevel.Bogus => \"bogus\"\n    LogLevel.Info => \"info\"\n  }\n}\n",
                ),
            ],
            "cove::resolve::unknown_enum_case",
        );
        assert!(diagnostic.message.contains("Bogus"));
    }

    // ------------------------------------------ conformances across modules

    /// The trait, and the type, every cross-module conformance test builds
    /// on: one exported trait with a required and a defaulted method, and one
    /// exported struct, each in a module of its own.
    const DISPLAY: &str = "\
/// Renders itself.
export trait Display {
  /// The full form.
  fn describe(self) -> String

  /// A short form, defaulting to the full one.
  fn label(self) -> String { self.describe() }
}
";

    const BOOKING: &str = "\
/// A booking.
export struct Booking {
  id: Int
}
";

    /// ADR 0006 allows a conformance in the module that declares the type,
    /// which with imports means the trait may be an imported one.
    #[test]
    fn a_module_may_conform_its_own_type_to_an_imported_trait() {
        let program = resolve_ok(&[
            ("display", DISPLAY),
            (
                "booking",
                &format!(
                    "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
                     /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
                ),
            ),
        ]);
        let conformance = program.modules["booking"]
            .conformances
            .get(&("Display".to_string(), "Booking".to_string()))
            .expect("the conformance is recorded where the type is declared");
        assert_eq!(conformance.trait_module, "display");
        assert_eq!(conformance.type_module, "booking");
        // The defaulted method comes along, so dispatch finds both.
        assert_eq!(
            conformance.methods.iter().cloned().collect::<Vec<_>>(),
            ["describe", "label"]
        );
    }

    /// And the reverse: a conformance in the module that declares the trait,
    /// for a type it imported.
    #[test]
    fn a_module_may_conform_an_imported_type_to_its_own_trait() {
        let program = resolve_ok(&[
            ("booking", BOOKING),
            (
                "display",
                &format!(
                    "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
                     /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
                ),
            ),
        ]);
        let conformance = program.modules["display"]
            .conformances
            .get(&("Display".to_string(), "Booking".to_string()))
            .expect("the conformance is recorded where the trait is declared");
        assert_eq!(conformance.trait_module, "display");
        assert_eq!(conformance.type_module, "booking");
        // The methods live with the conformance, not with the type.
        assert!(program.modules["display"]
            .methods
            .contains_key(&("Booking".to_string(), "describe".to_string())));
        assert!(program.modules["booking"].methods.is_empty());
    }

    /// The orphan rule is what imports must *not* widen: a module that can
    /// see both parties still may not join them.
    #[test]
    fn a_third_module_may_not_conform_an_imported_type_to_an_imported_trait() {
        let diagnostic = resolve_err(
            &[
                ("display", DISPLAY),
                ("booking", BOOKING),
                (
                    "app",
                    "use display.Display\nuse booking.Booking\n\n\
                     impl Display for Booking {\n  /// The full form.\n  fn describe(self) -> String {\n    \"b\"\n  }\n}\n",
                ),
            ],
            "cove::resolve::orphan_conformance",
        );
        assert!(diagnostic.message.contains("declares neither"));
    }

    /// An inherent `impl` is not a conformance, so it may not reach across a
    /// module boundary at all: there would be no fact for the type's own
    /// module to see.
    #[test]
    fn an_inherent_impl_may_not_extend_an_imported_type() {
        let diagnostic = resolve_err(
            &[
                ("booking", BOOKING),
                (
                    "app",
                    "use booking.Booking\n\nimpl Booking {\n  /// The id.\n  fn id(self) -> Int {\n    self.id\n  }\n}\n",
                ),
            ],
            "cove::resolve::foreign_inherent_impl",
        );
        assert!(diagnostic.help.as_deref().unwrap().contains("booking"));
    }

    /// A conformance declared where the trait is may not collide with a
    /// method the type's own module declares: the checker would resolve one
    /// and the interpreter the other.
    #[test]
    fn a_conformance_may_not_collide_with_the_type_s_own_method() {
        let diagnostic = resolve_err(
            &[
                (
                    "booking",
                    &format!(
                        "{BOOKING}\nimpl Booking {{\n  /// Describes.\n  export fn describe(self) -> String {{\n    \"inherent\"\n  }}\n}}\n"
                    ),
                ),
                (
                    "display",
                    &format!(
                        "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
                         /// The full form.\n  fn describe(self) -> String {{\n    \"conformance\"\n  }}\n}}\n"
                    ),
                ),
            ],
            "cove::resolve::duplicate_declaration",
        );
        assert!(diagnostic.message.contains("Booking.describe"));
        assert!(diagnostic.message.contains("display"));
        assert!(diagnostic.message.contains("booking"));
    }

    /// The same collision between two conformances in two modules, which the
    /// per-module duplicate check cannot see either.
    #[test]
    fn two_modules_may_not_give_one_type_the_same_method_name() {
        let diagnostic = resolve_err(
            &[
                ("booking", BOOKING),
                (
                    "display",
                    &format!(
                        "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
                         /// The full form.\n  fn describe(self) -> String {{\n    \"d\"\n  }}\n}}\n"
                    ),
                ),
                (
                    "audit",
                    "use booking.Booking\n\n\
                     /// Audits itself.\nexport trait Audit {\n  /// The full form.\n  fn describe(self) -> String\n}\n\n\
                     impl Audit for Booking {\n  /// The full form.\n  fn describe(self) -> String {\n    \"a\"\n  }\n}\n",
                ),
            ],
            "cove::resolve::duplicate_declaration",
        );
        assert!(diagnostic.message.contains("Booking.describe"));
    }

    /// Two modules cannot both declare the same conformance, and the import
    /// rules are what guarantee it: each would have to import the other's
    /// party, which is a cycle. No separate check is needed, so this pins
    /// the shape that would need one if cycles were ever allowed.
    #[test]
    fn one_conformance_cannot_be_declared_in_both_parties_modules() {
        let diagnostic = resolve_err(
            &[
                (
                    "display",
                    &format!(
                        "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
                         /// The full form.\n  fn describe(self) -> String {{\n    \"d\"\n  }}\n}}\n"
                    ),
                ),
                (
                    "booking",
                    &format!(
                        "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
                         /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
                    ),
                ),
            ],
            "cove::resolve::import_cycle",
        );
        assert!(diagnostic.message.contains(" -> "));
    }

    #[test]
    fn an_import_colliding_with_a_declared_trait_is_rejected() {
        let diagnostic = resolve_err(
            &[
                ("display", DISPLAY),
                (
                    "app",
                    "use display.Display\n\ntrait Display {\n  /// The full form.\n  fn describe(self) -> String\n}\n",
                ),
            ],
            "cove::resolve::import_conflict",
        );
        assert!(diagnostic.message.contains("also declares it"));
    }

    #[test]
    fn a_use_of_a_private_trait_is_rejected() {
        resolve_err(
            &[
                (
                    "display",
                    "trait Display {\n  /// The full form.\n  fn describe(self) -> String\n}\n",
                ),
                ("app", "use display.Display\n"),
            ],
            "cove::resolve::private_declaration",
        );
    }

    #[test]
    fn a_conformance_naming_a_trait_no_module_declares_is_rejected() {
        let diagnostic = resolve_err(
            &[(
                "booking",
                &format!("{BOOKING}\nimpl Display for Booking {{\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"),
            )],
            "cove::resolve::unknown_trait",
        );
        assert!(diagnostic.help.as_deref().unwrap().contains("use"));
    }

    /// A conformance method is a method of the type wherever it was written,
    /// so a capability it needs reaches every caller of that method.
    #[test]
    fn required_capabilities_cross_a_conformance_in_another_module() {
        let program = resolve_ok(&[
            ("booking", BOOKING),
            (
                "display",
                &format!(
                    "use console.println\nuse booking.Booking\n\n{DISPLAY}\n\
                     impl Display for Booking {{\n  /// The full form.\n  fn describe(self) -> String {{\n    \
                     console.println(\"tracing\")\n    \"b\"\n  }}\n}}\n"
                ),
            ),
            (
                "app",
                "use booking.Booking\nuse display.Display\n\n\
                 /// Entry point.\nexport fn main(value: Booking) -> String {\n  value.describe()\n}\n",
            ),
        ]);
        assert!(program.modules["app"].functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    /// A bare case name resolves against the enums in scope, which now
    /// includes an imported one.
    #[test]
    fn a_bare_case_of_an_imported_enum_resolves() {
        let diagnostic = resolve_err(
            &[
                (
                    "levels",
                    "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n}\n",
                ),
                (
                    "app",
                    "use levels.LogLevel\n\n/// Describes a level.\nexport fn describe(level: LogLevel) -> String {\n  \
                     match level {\n    Debug => \"debug\"\n  }\n}\n",
                ),
            ],
            "cove::resolve::non_exhaustive_match",
        );
        assert!(diagnostic.message.contains("LogLevel.Info"));
    }

    #[test]
    fn ambiguous_unqualified_use_is_rejected() {
        let module =
            module_from_sources("ambiguous", &["use console.println\nuse other.println\n"]);
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::ambiguous_use"));
    }

    #[test]
    fn derives_capability_from_qualified_call() {
        let module = module_from_sources(
            "cap",
            &[
                "use console.println\n\n/// Prints.\nexport fn main() {\n  console.println(\"hi\")\n}\n",
            ],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let entry = &program.modules["cap"].functions["main"];
        assert!(entry
            .direct_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn derives_capability_from_unqualified_call() {
        let module = module_from_sources(
            "cap2",
            &["use console.println\n\n/// Prints.\nexport fn main() {\n  println(\"hi\")\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let entry = &program.modules["cap2"].functions["main"];
        assert!(entry
            .direct_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn finds_a_host_call_inside_a_closure() {
        let module = module_from_sources(
            "cap3",
            &[
                "use console.println\n\n/// Builds a callback.\nexport fn build() {\n  let cb = fn() {\n    console.println(\"hi\")\n  }\n}\n",
            ],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let entry = &program.modules["cap3"].functions["build"];
        assert!(entry
            .direct_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn warns_on_missing_doc_for_exported_declaration() {
        let module = module_from_sources("nodoc", &["export fn main() {\n}\n"]);
        let package = package_of(module);
        let program = resolve(&package).expect("resolves even with a warning");
        assert!(program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::missing_doc"));
    }

    #[test]
    fn private_declaration_without_doc_does_not_warn() {
        let module = module_from_sources("private", &["fn helper() {\n}\n"]);
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(program.notices.is_empty());
    }

    #[test]
    fn loads_and_resolves_the_real_examples_package() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut sources = SourceMap::new();
        let package = crate::package::load(&root, &mut sources).expect("examples package loads");
        let program = resolve(&package);
        assert!(program.is_ok(), "examples package should resolve cleanly");
    }

    #[test]
    fn required_capabilities_reach_through_a_helper_chain() {
        let module = module_from_sources(
            "chain",
            &["use console.println\n\n\
                 /// Logs a message.\n\
                 fn log(msg: String) {\n  console.println(msg)\n}\n\n\
                 /// Entry point; never calls a Host API directly.\n\
                 export fn main() {\n  log(\"hi\")\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let main = &program.modules["chain"].functions["main"];
        assert!(main.direct_capabilities.is_empty());
        assert!(main
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn required_capabilities_reach_through_a_method_call() {
        let module = module_from_sources(
            "methodprop",
            &["use console.println\n\n\
                 /// A thing with an id.\n\
                 export struct Thing {\n  id: String\n}\n\n\
                 impl Thing {\n  \
                 /// Prints the id.\n  \
                 fn touch(self) {\n    console.println(self.id)\n  }\n}\n\n\
                 /// Entry point that reaches the Host API only through `Thing.touch`.\n\
                 export fn main() {\n  Thing.touch()\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let touch =
            &program.modules["methodprop"].methods[&("Thing".to_string(), "touch".to_string())];
        assert!(touch
            .direct_capabilities
            .contains(&Capability::new("console")));
        let main = &program.modules["methodprop"].functions["main"];
        assert!(main.direct_capabilities.is_empty());
        assert!(main
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    #[test]
    fn required_capabilities_propagate_through_mutual_recursion() {
        let module = module_from_sources(
            "mutual",
            &["use console.println\n\n\
                 /// True when `n` is even; recurses through `isOdd`.\n\
                 fn isEven(n: Int) -> Bool {\n  \
                 if n == 0 {\n    true\n  } else {\n    isOdd(n - 1)\n  }\n}\n\n\
                 /// True when `n` is odd; logs, then recurses through `isEven`.\n\
                 fn isOdd(n: Int) -> Bool {\n  \
                 console.println(\"checking\")\n  \
                 if n == 0 {\n    false\n  } else {\n    isEven(n - 1)\n  }\n}\n\n\
                 /// Entry point.\n\
                 export fn main() -> Bool {\n  isEven(4)\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let resolved = &program.modules["mutual"];

        assert!(resolved.functions["isOdd"]
            .direct_capabilities
            .contains(&Capability::new("console")));
        assert!(resolved.functions["isEven"].direct_capabilities.is_empty());

        // Neither function calls the other's host capability directly, but
        // the fixpoint must reach it through the recursive cycle without
        // looping forever.
        assert!(resolved.functions["isEven"]
            .required_capabilities
            .contains(&Capability::new("console")));
        assert!(resolved.functions["main"]
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    /// An embedder's schema for a module named after neither of its
    /// operations: the module's own capability is `directory`, but the
    /// `payroll` operation is gated on `payroll` instead. This is the shape
    /// the boundary (`HostRegistry::call_with`) actually enforces, and the
    /// checker must derive the same capability or an embedder could grant
    /// exactly what the checker asked for and still be refused at run time.
    const COMPANY: ModuleSchema = ModuleSchema {
        name: "company",
        capability: "directory",
        operations: &[
            OperationSchema {
                name: "employee",
                params: &[],
                variadic: false,
                result: HostType::Unit,
                capability: "directory",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
            OperationSchema {
                name: "payroll",
                params: &[],
                variadic: false,
                result: HostType::Unit,
                capability: "payroll",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
        ],
        types: &[],
        resources: &[],
    };

    #[test]
    fn required_capabilities_use_the_operation_s_capability_not_the_module_s() {
        let schemas = HostSchemas::new().with(COMPANY);
        let program = resolve_ok_with(
            &[(
                "app",
                "use company\n\n/// Entry point.\nexport fn main() {\n  company.payroll()\n}\n",
            )],
            &schemas,
        );
        let main = &program.modules["app"].functions["main"];
        assert!(main
            .required_capabilities
            .contains(&Capability::new("payroll")));
        assert!(!main
            .required_capabilities
            .contains(&Capability::new("directory")));
    }

    #[test]
    fn required_capabilities_fall_back_to_the_module_s_capability_for_an_undeclared_operation() {
        let schemas = HostSchemas::new().with(COMPANY);
        let program = resolve_ok_with(
            &[(
                "app",
                "use company\n\n/// Entry point; calls an operation the schema does not declare.\nexport fn main() {\n  company.other()\n}\n",
            )],
            &schemas,
        );
        let main = &program.modules["app"].functions["main"];
        assert!(main
            .required_capabilities
            .contains(&Capability::new("directory")));
        assert!(!main
            .required_capabilities
            .contains(&Capability::new("payroll")));
    }

    #[test]
    fn a_function_requiring_nothing_stays_empty() {
        let module = module_from_sources(
            "pure",
            &[
                "/// Adds two numbers.\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n\
                 /// Entry point; calls only a pure helper.\n\
                 export fn main() -> Int {\n  add(1, 2)\n}\n",
            ],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        let resolved = &program.modules["pure"];
        assert!(resolved.functions["add"].required_capabilities.is_empty());
        assert!(resolved.functions["main"].required_capabilities.is_empty());
    }

    #[test]
    fn derives_required_capabilities_for_the_real_examples_package() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut sources = SourceMap::new();
        let package = crate::package::load(&root, &mut sources).expect("examples package loads");
        let program = resolve(&package).expect("examples package resolves");

        let hello_main = &program.modules["hello"].functions["main"];
        assert!(hello_main
            .required_capabilities
            .contains(&Capability::new("console")));

        let hello_greeting = &program.modules["hello"].functions["greeting"];
        assert!(hello_greeting.required_capabilities.is_empty());

        let restricted_main = &program.modules["restricted"].functions["main"];
        assert!(restricted_main
            .required_capabilities
            .contains(&Capability::new("documents")));
        assert!(restricted_main
            .required_capabilities
            .contains(&Capability::new("console")));

        let config_load_config = &program.modules["config"].functions["loadConfig"];
        assert!(config_load_config
            .required_capabilities
            .contains(&Capability::new("env")));
    }

    // ------------------------------------------------ capability-openness

    /// The declaration `module.name` of a resolved package.
    #[track_caller]
    fn function<'a>(program: &'a Program, module: &str, name: &str) -> &'a FnEntry {
        program
            .lookup_fn(module, name)
            .unwrap_or_else(|| panic!("`{module}.{name}` is declared"))
    }

    #[test]
    fn calling_a_function_typed_parameter_is_capability_open() {
        let program = resolve_ok(&[(
            "higher",
            "/// Runs whatever it was handed.\n\
             export fn run(work: fn() -> Unit) {\n  work()\n}\n",
        )]);
        let run = function(&program, "higher", "run");
        assert!(run.required_capabilities.is_empty());
        assert_eq!(
            run.open_calls,
            BTreeSet::from([OpenCall::FunctionValue]),
            "a call to a value the call graph cannot name is the higher-order case"
        );
    }

    /// The model this whole decision rests on: a lambda is charged to the
    /// function that *writes* it, so a closure invoked through a parameter
    /// does not lose its capability on the way -- while the function that
    /// invokes it is honest about not knowing what it will run.
    #[test]
    fn a_closure_that_calls_a_host_charges_the_function_that_wrote_it() {
        let program = resolve_ok(&[(
            "callback",
            "use console.println\n\n\
             /// Runs whatever it was handed.\n\
             fn run(work: fn() -> Unit) {\n  work()\n}\n\n\
             /// Hands `run` a closure that prints.\n\
             export fn main() {\n  run(fn() {\n    console.println(\"hi\")\n  })\n}\n",
        )]);

        let main = function(&program, "callback", "main");
        assert!(
            main.direct_capabilities
                .contains(&Capability::new("console")),
            "the closure's body is part of the body that wrote it"
        );

        let run = function(&program, "callback", "run");
        assert!(run.required_capabilities.is_empty());
        assert!(run.is_capability_open());
        assert_eq!(
            main.open_calls,
            BTreeSet::from([OpenCall::ReachedOpenCall]),
            "calling a capability-open declaration makes its caller one too"
        );
    }

    #[test]
    fn calling_a_method_on_a_dyn_parameter_is_capability_open() {
        let program = resolve_ok(&[(
            "dynamic",
            "/// Something that describes itself.\n\
             export trait Summary {\n  \
             /// One line about this value.\n  \
             fn summarize(self) -> String\n}\n\n\
             /// Renders entries whose types may differ.\n\
             export fn report(entries: Array<dyn Summary>) -> String {\n  \
             var text = \"\"\n  \
             for entry in entries {\n    text = entry.summarize()\n  }\n  text\n}\n",
        )]);
        assert_eq!(
            function(&program, "dynamic", "report").open_calls,
            BTreeSet::from([OpenCall::DynamicDispatch]),
            "a `dyn` value taken out of a container still dispatches by its own type"
        );
    }

    #[test]
    fn calling_a_method_on_a_bounded_generic_is_capability_open() {
        let program = resolve_ok(&[(
            "generic",
            "/// Something that describes itself.\n\
             export trait Summary {\n  \
             /// One line about this value.\n  \
             fn summarize(self) -> String\n}\n\n\
             /// Headlines one entry.\n\
             export fn headline<T: Summary>(entry: T) -> String {\n  entry.summarize()\n}\n",
        )]);
        assert_eq!(
            function(&program, "generic", "headline").open_calls,
            BTreeSet::from([OpenCall::DynamicDispatch]),
            "the caller instantiates `T`, so it also picks the conformance that runs"
        );
    }

    #[test]
    fn calling_a_callback_stored_in_data_is_capability_open() {
        let program = resolve_ok(&[(
            "stored",
            "/// Runs every handler in turn.\n\
             export fn dispatch(handlers: Array<fn() -> Unit>) {\n  \
             for handler in handlers {\n    handler()\n  }\n}\n",
        )]);
        assert_eq!(
            function(&program, "stored", "dispatch").open_calls,
            BTreeSet::from([OpenCall::FunctionValue])
        );
    }

    /// Everything a bare call can be other than a value: a declaration, a
    /// struct initializer, a host item, a free builtin, and a builtin type
    /// used as a namespace. None of them is indirect, and a report that
    /// called them open would be crying wolf on ordinary code.
    #[test]
    fn ordinary_calls_leave_a_function_capability_closed() {
        let program = resolve_ok(&[(
            "closed",
            "use console.println\n\n\
             /// A thing with an id.\n\
             export struct Thing {\n  id: String\n}\n\n\
             /// Makes one.\n\
             fn make() -> Thing {\n  Thing(id: \"a\")\n}\n\n\
             /// Entry point.\n\
             export fn main() -> Result<Unit, Error> {\n  \
             let thing = make()\n  \
             let items = Vector.of(thing.id)\n  \
             println(\"{items.length()}\")?\n  \
             assert(true)?\n  \
             Ok(())\n}\n",
        )]);
        let main = function(&program, "closed", "main");
        assert!(!main.is_capability_open(), "found {:?}", main.open_calls);
        assert!(main
            .required_capabilities
            .contains(&Capability::new("console")));
    }

    /// A named function handed somewhere else to be called -- a route table,
    /// a host that will invoke it -- is still named, so the edge is real and
    /// the capability it needs reaches the function that named it.
    #[test]
    fn naming_a_function_as_a_value_reaches_what_it_requires() {
        let program = resolve_ok(&[(
            "reentry",
            "use http\n\
             use console.println\n\n\
             /// Answers one request, and says so on the console.\n\
             fn health(request: http.Request) -> http.Response {\n  \
             console.println(\"served\")\n  \
             http.json(200, \"ok\")\n}\n\n\
             /// Registers the handler the host will call back.\n\
             export fn routes() -> Array<http.Route> {\n  \
             [http.Route(method: http.Method.Get, path: \"/health\", handler: health)]\n}\n",
        )]);
        let routes = function(&program, "reentry", "routes");
        assert!(
            routes
                .required_capabilities
                .contains(&Capability::new("console")),
            "a callback the host will invoke is reached through the name that stored it"
        );
        assert!(
            !routes.is_capability_open(),
            "nothing here is a call the graph could not follow"
        );
    }

    /// The shape that falsified the guarantee before the field types were
    /// read: a `dyn Trait` reached through a struct field rather than through
    /// a parameter. `lib` writes the type once, on the field, and never
    /// again; the conformance lives in `plugin`, which `lib` cannot reach, so
    /// the receiver over-approximation finds nothing either. Without the
    /// marker `app.main` reports an empty, complete-looking set and the run
    /// is refused at the boundary.
    #[test]
    fn dispatching_through_a_dyn_struct_field_is_capability_open() {
        let program = resolve_ok(&[
            (
                "lib",
                "/// Something that describes itself.\n\
                 export trait Summary {\n  \
                 /// One line about this value.\n  \
                 fn summarize(self) -> String\n}\n\n\
                 /// Holds one of them.\n\
                 export struct Box {\n  item: dyn Summary\n}\n\n\
                 impl Box {\n  \
                 /// Shows what it holds.\n  \
                 export fn show(self) -> String {\n    self.item.summarize()\n  }\n}\n",
            ),
            (
                "plugin",
                "use console.println\n\
                 use lib.Summary\n\n\
                 /// Says so out loud.\n\
                 export struct Noisy {\n  n: Int\n}\n\n\
                 impl Summary for Noisy {\n  \
                 /// One line about this value.\n  \
                 fn summarize(self) -> String {\n    \
                 let ignored = println(\"side effect\")\n    \"noisy\"\n  }\n}\n",
            ),
            (
                "app",
                "use lib\nuse plugin\n\n\
                 /// Entry point.\n\
                 export fn main() -> Result<Unit, Error> {\n  \
                 let held = lib.Box(item: plugin.Noisy(n: 1))\n  \
                 let text = held.show()\n  \
                 Ok(())\n}\n",
            ),
        ]);
        let show = &program.modules["lib"].methods[&("Box".to_string(), "show".to_string())];
        assert_eq!(
            show.open_calls,
            BTreeSet::from([OpenCall::DynamicDispatch]),
            "a `dyn` field is a value whose implementation its producer chose"
        );
        assert!(
            function(&program, "app", "main").is_capability_open(),
            "openness has to reach the entry, or its empty set reads as complete"
        );
    }

    /// The container is not the thing it contains. `Array.length` is a
    /// builtin with no conformance to pick, so reading the element type at
    /// depth must not make the receiver itself opaque.
    #[test]
    fn a_method_on_a_container_of_generics_is_not_dynamic_dispatch() {
        let program = resolve_ok(&[(
            "counting",
            "/// How many entries there are.\n\
             export fn count<T>(items: Array<T>) -> Int {\n  items.length()\n}\n",
        )]);
        let count = function(&program, "counting", "count");
        assert!(!count.is_capability_open(), "found {:?}", count.open_calls);
    }

    /// ...while what comes *out* of that container still is.
    #[test]
    fn a_method_on_an_element_of_a_dyn_container_is_dynamic_dispatch() {
        let program = resolve_ok(&[(
            "element",
            "/// Something that describes itself.\n\
             export trait Summary {\n  \
             /// One line about this value.\n  \
             fn summarize(self) -> String\n}\n\n\
             /// The first entry's line, or nothing.\n\
             export fn first(entries: Array<dyn Summary>) -> String {\n  \
             entries.get(0).map(fn(entry) {\n    entry.summarize()\n  }).unwrapOr(\"\")\n}\n",
        )]);
        assert_eq!(
            function(&program, "element", "first").open_calls,
            BTreeSet::from([OpenCall::DynamicDispatch]),
            "an element taken out of a `dyn` container dispatches by its own type"
        );
    }

    /// A name the body binds is that name, whatever the module declares
    /// under it. Reading it recorded an exact call-graph edge before, which
    /// charged a pure function a capability it cannot reach.
    #[test]
    fn a_parameter_shadowing_a_function_records_no_edge() {
        let program = resolve_ok(&[(
            "shadow",
            "use console.println\n\n\
             /// Prints one line.\n\
             fn report(text: String) -> Result<Unit, Error> {\n  println(text)\n}\n\n\
             /// Returns what it was given.\n\
             export fn label(report: String) -> String {\n  report\n}\n",
        )]);
        let label = function(&program, "shadow", "label");
        assert!(
            label.required_capabilities.is_empty(),
            "found {:?}",
            label.required_capabilities
        );
        assert!(!label.is_capability_open(), "found {:?}", label.open_calls);
    }

    /// A local `fn` is an ordinary closure, so it is charged where it is
    /// written -- which both restores the capability its body needs and
    /// removes the `FunctionValue` marker calling it used to earn.
    #[test]
    fn a_local_fn_is_charged_to_the_body_that_wrote_it() {
        let program = resolve_ok(&[(
            "local",
            "use console.println\n\n\
             /// Entry point.\n\
             export fn main() -> Result<Unit, Error> {\n  \
             /// Prints once.\n  \
             fn helper() -> Result<Unit, Error> {\n    println(\"hi\")\n  }\n  \
             helper()?\n  Ok(())\n}\n",
        )]);
        let main = function(&program, "local", "main");
        assert!(main
            .required_capabilities
            .contains(&Capability::new("console")));
        assert!(!main.is_capability_open(), "found {:?}", main.open_calls);
    }

    #[test]
    fn openness_crosses_a_module_boundary() {
        let program = resolve_ok(&[
            (
                "runner",
                "/// Runs whatever it was handed.\n\
                 export fn run(work: fn() -> Unit) {\n  work()\n}\n",
            ),
            (
                "app",
                "use runner.run\n\n\
                 /// Entry point.\n\
                 export fn main() {\n  run(fn() {\n  })\n}\n",
            ),
        ]);
        assert!(function(&program, "app", "main").is_capability_open());
    }

    #[test]
    fn match_covering_every_enum_case_passes() {
        let module = module_from_sources(
            "exhaustive",
            &["enum LogLevel {\n  Debug\n  Info\n}\n\n\
               fn describe(level: LogLevel) -> String {\n  \
               match level {\n    \
               LogLevel.Debug => \"debug\"\n    \
               LogLevel.Info => \"info\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(program.notices.is_empty());
    }

    #[test]
    fn missing_case_is_reported_by_name() {
        let module = module_from_sources(
            "missing",
            &["enum LogLevel {\n  Debug\n  Info\n  Warn\n  Error\n}\n\n\
               fn describe(level: LogLevel) -> String {\n  \
               match level {\n    \
               LogLevel.Debug => \"debug\"\n    \
               LogLevel.Info => \"info\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("LogLevel.Warn"));
        assert!(diag.message.contains("LogLevel.Error"));
        assert!(diag.help.as_deref().unwrap().contains("LogLevel.Warn"));
    }

    #[test]
    fn a_wildcard_arm_makes_a_partial_match_exhaustive() {
        let module = module_from_sources(
            "wildcard_ok",
            &["enum LogLevel {\n  Debug\n  Info\n  Warn\n  Error\n}\n\n\
               fn describe(level: LogLevel) -> String {\n  \
               match level {\n    \
               LogLevel.Debug => \"debug\"\n    \
               _ => \"other\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(!program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::non_exhaustive_match"));
    }

    #[test]
    fn option_match_covering_both_cases_passes() {
        let module = module_from_sources(
            "option_ok",
            &["fn describe(value: Option<Int>) -> Int {\n  \
               match value {\n    \
               Some(x) => x\n    \
               None => 0\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        resolve(&package).expect("resolves");
    }

    #[test]
    fn option_match_missing_none_is_reported() {
        let module = module_from_sources(
            "option_missing",
            &["fn describe(value: Option<Int>) -> Int {\n  \
               match value {\n    \
               Some(x) => x\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("None"));
    }

    #[test]
    fn result_match_covering_both_cases_passes() {
        let module = module_from_sources(
            "result_ok",
            &["fn describe(value: Result<Int, Error>) -> Int {\n  \
               match value {\n    \
               Ok(x) => x\n    \
               Err(e) => 0\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        resolve(&package).expect("resolves");
    }

    #[test]
    fn result_match_missing_err_is_reported() {
        let module = module_from_sources(
            "result_missing",
            &["fn describe(value: Result<Int, Error>) -> Int {\n  \
               match value {\n    \
               Ok(x) => x\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("Err"));
    }

    #[test]
    fn unknown_enum_case_is_reported() {
        let module = module_from_sources(
            "unknown_case",
            &["enum LogLevel {\n  Debug\n  Info\n}\n\n\
               fn describe(level: LogLevel) -> String {\n  \
               match level {\n    \
               LogLevel.Debug => \"debug\"\n    \
               LogLevel.Bogus => \"bogus\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::unknown_enum_case"));
    }

    #[test]
    fn duplicate_match_arm_is_reported() {
        let module = module_from_sources(
            "dup_arm",
            &["enum LogLevel {\n  Debug\n  Info\n}\n\n\
               fn describe(level: LogLevel) -> String {\n  \
               match level {\n    \
               LogLevel.Debug => \"first\"\n    \
               LogLevel.Debug => \"second\"\n    \
               LogLevel.Info => \"info\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::duplicate_match_arm"));
    }

    #[test]
    fn arm_after_a_wildcard_is_an_unreachable_warning() {
        let module = module_from_sources(
            "unreachable_arm",
            &["fn tag(n: Int) -> String {\n  \
               match n {\n    \
               _ => \"any\"\n    \
               1 => \"one\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves; only a warning");
        assert!(program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::unreachable_match_arm"));
    }

    #[test]
    fn literal_match_over_non_bool_without_a_catch_all_arm_is_reported() {
        let module = module_from_sources(
            "literal_missing",
            &["fn tag(n: Int) -> String {\n  \
               match n {\n    \
               1 => \"one\"\n    \
               2 => \"two\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("literal"));
    }

    /// `Bool`'s domain is exactly `{true, false}`, so covering both makes the
    /// match exhaustive without a catch-all — unlike `Int` or `String`.
    #[test]
    fn bool_match_covering_both_values_passes_without_a_catch_all() {
        let module = module_from_sources(
            "bool_ok",
            &["fn flag(on: Bool) -> String {\n  \
               match on {\n    \
               true => \"yes\"\n    \
               false => \"no\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(!program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::non_exhaustive_match"));
    }

    #[test]
    fn bool_match_missing_false_is_reported_by_name() {
        let module = module_from_sources(
            "bool_missing_false",
            &["fn flag(on: Bool) -> String {\n  \
               match on {\n    \
               true => \"yes\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("`false`"));
        assert!(diag.help.as_deref().unwrap().contains("false"));
    }

    #[test]
    fn bool_match_missing_true_is_reported_by_name() {
        let module = module_from_sources(
            "bool_missing_true",
            &["fn flag(on: Bool) -> String {\n  \
               match on {\n    \
               false => \"no\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("`true`"));
        assert!(diag.help.as_deref().unwrap().contains("true"));
    }

    #[test]
    fn bool_match_with_both_values_and_a_wildcard_warns_the_wildcard_is_unreachable() {
        let module = module_from_sources(
            "bool_wildcard_unreachable",
            &["fn flag(on: Bool) -> String {\n  \
               match on {\n    \
               true => \"yes\"\n    \
               false => \"no\"\n    \
               _ => \"other\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves; only a warning");
        assert!(program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::unreachable_match_arm"));
    }

    #[test]
    fn duplicate_bool_match_arm_is_reported() {
        let module = module_from_sources(
            "bool_dup_arm",
            &["fn flag(on: Bool) -> String {\n  \
               match on {\n    \
               true => \"yes\"\n    \
               true => \"also yes\"\n    \
               false => \"no\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::duplicate_match_arm"));
    }

    /// A literal `match` mixing a `Bool` arm with a non-`Bool` literal is not
    /// exhaustible by the `Bool`-domain rule, so it keeps needing a
    /// catch-all like any other literal match.
    #[test]
    fn mixed_bool_and_int_literal_match_still_needs_a_catch_all() {
        let module = module_from_sources(
            "mixed_literal",
            &["fn describe(n: Int) -> String {\n  \
               match n {\n    \
               true => \"true?\"\n    \
               1 => \"one\"\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        let diag = errs
            .iter()
            .find(|d| d.code == "cove::resolve::non_exhaustive_match")
            .expect("reports non_exhaustive_match");
        assert!(diag.message.contains("literal"));
    }

    /// Pins the shape used by `examples/config/load.cove`: string literals
    /// with a final binding arm that catches everything else.
    #[test]
    fn literal_match_with_a_binding_arm_passes() {
        let module = module_from_sources(
            "literal_ok",
            &["fn parseLevel(raw: String) -> String {\n  \
               match raw {\n    \
               \"debug\" => \"Debug\"\n    \
               \"info\" => \"Info\"\n    \
               other => other\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(!program
            .notices
            .iter()
            .any(|d| d.code == "cove::resolve::non_exhaustive_match"));
    }

    #[test]
    fn a_match_whose_enum_is_ambiguous_stays_silent() {
        let module = module_from_sources(
            "ambiguous_enum",
            &["enum Left {\n  A\n  B\n}\n\n\
               enum Right {\n  A\n  C\n}\n\n\
               fn pick(x: Int) -> Int {\n  \
               match x {\n    \
               A => 1\n    \
               B => 2\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let program = resolve(&package).expect("resolves; the enum cannot be determined");
        assert!(!program
            .notices
            .iter()
            .any(|d| d.code.starts_with("cove::resolve::") && d.code.contains("match")));
    }

    #[test]
    fn break_and_continue_inside_a_loop_resolve_cleanly() {
        let module = module_from_sources(
            "loop_ok",
            &["fn firstEven(items: Int...) -> Int {\n  \
               for item in items {\n    \
               if item % 2 != 0 {\n      continue\n    }\n    \
               break item\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        resolve(&package).expect("resolves");
    }

    #[test]
    fn break_outside_a_loop_is_rejected() {
        let module = module_from_sources("break_bare", &["fn go() {\n  break\n}\n"]);
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::break_outside_loop"));
    }

    #[test]
    fn continue_outside_a_loop_is_rejected() {
        let module = module_from_sources("continue_bare", &["fn go() {\n  continue\n}\n"]);
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::continue_outside_loop"));
    }

    #[test]
    fn break_inside_a_lambda_cannot_reach_an_outer_loop() {
        let module = module_from_sources(
            "break_in_lambda",
            &["fn go() {\n  for item in [1, 2] {\n    \
               let f = fn() {\n      break\n    }\n  \
               }\n}\n"],
        );
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::break_outside_loop"));
    }
}

#[cfg(test)]
mod send_sync {
    use super::Program;

    /// A task thread runs the same resolved program as the thread that
    /// spawned it, reached by reference, so the program must be shareable
    /// across threads (ADR 0008). Nothing in a resolved program is mutable,
    /// so this holds as long as no reference-counted handle in it is `Rc`.
    #[test]
    fn a_resolved_program_is_shareable_across_task_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Program>();
    }
}
