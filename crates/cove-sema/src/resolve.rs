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
use std::rc::Rc;

use cove_diag::{Diagnostic, Span};
use cove_syntax::ast::{
    Block, EnumDecl, Expr, ExprKind, FnDecl, Item, ItemKind, MatchArm, Pattern, PatternKind, Stmt,
    StmtKind, StrPart, StructDecl, TraitDecl, TraitMethod, TypeAlias,
};

use crate::capability::Capability;
use crate::package::Package;

/// A declaration that belongs to a module, with the facts derived from it.
#[derive(Debug)]
pub struct FnEntry {
    pub decl: Rc<FnDecl>,
    pub exported: bool,
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
    pub required_capabilities: BTreeSet<Capability>,
}

#[derive(Debug)]
pub struct StructEntry {
    pub decl: Rc<StructDecl>,
    pub exported: bool,
    pub doc: Option<String>,
}

#[derive(Debug)]
pub struct EnumEntry {
    pub decl: Rc<EnumDecl>,
    pub exported: bool,
    pub doc: Option<String>,
}

/// A trait a module declares.
#[derive(Debug)]
pub struct TraitEntry {
    pub decl: Rc<TraitDecl>,
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
    pub decl: Rc<TypeAlias>,
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
    /// Non-fatal diagnostics, such as missing doc comments on exported
    /// declarations.
    pub warnings: Vec<Diagnostic>,
}

impl Program {
    /// Looks up a fully qualified entry such as `hello.main`.
    pub fn lookup_fn(&self, module: &str, name: &str) -> Option<&FnEntry> {
        self.modules.get(module)?.functions.get(name)
    }
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
/// 5. required capabilities are derived as a fixed point over the
///    *package's* call graph, so a function reaching a Host API through an
///    imported helper reports it;
/// 6. every body is checked against everything now known, including enums
///    reached through an import.
pub fn resolve(package: &Package) -> Result<Program, Vec<Diagnostic>> {
    let mut program = Program::default();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let surfaces: BTreeMap<&str, Surface> = package
        .modules
        .iter()
        .map(|(name, module)| (name.as_str(), Surface::of(module)))
        .collect();

    let mut call_sites: BTreeMap<Node, Vec<CallShape>> = BTreeMap::new();
    let mut edges: Vec<ImportEdge> = Vec::new();
    for (name, module) in &package.modules {
        let uses = resolve_uses(name, module, &surfaces, &mut errors);
        edges.extend(uses.edges.iter().cloned());
        let (resolved, calls) =
            resolve_module(name, module, uses, &surfaces, &mut errors, &mut warnings);
        for (key, shapes) in calls {
            call_sites.insert((name.clone(), key), shapes);
        }
        program.modules.insert(name.clone(), resolved);
    }

    check_import_cycles(&edges, &mut errors);
    check_method_collisions(&program, &mut errors);
    let call_graph = package_call_graph(&program, &call_sites);
    propagate_capabilities(&mut program, &call_graph);
    check_bodies(&program, &mut errors, &mut warnings);

    if errors.is_empty() {
        program.warnings = warnings;
        Ok(program)
    } else {
        errors.extend(warnings);
        Err(errors)
    }
}

fn resolve_module(
    name: &str,
    module: &crate::package::Module,
    uses: ModuleUses,
    surfaces: &BTreeMap<&str, Surface>,
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
                    let (capabilities, calls) =
                        analyze_body(&decl.body, &resolved.host_uses, &resolved.host_items);
                    call_sites.insert(FnKey::Fn(decl.name.node.clone()), calls);
                    resolved.functions.insert(
                        decl.name.node.clone(),
                        FnEntry {
                            decl: Rc::new(decl.clone()),
                            exported: item.exported,
                            doc: item.doc.clone(),
                            receiver_type: None,
                            from_trait_default: None,
                            direct_capabilities: capabilities,
                            required_capabilities: BTreeSet::new(),
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
                            decl: Rc::new(decl.clone()),
                            exported: item.exported,
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
                            decl: Rc::new(decl.clone()),
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
                            decl: Rc::new(decl.clone()),
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
                            decl: Rc::new(decl.clone()),
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
            if trait_module.is_none() {
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
            let trait_module =
                declaring_module_of(surfaces, name, &uses, &trait_ident.node, DeclKind::Trait)
                    .expect("checked above")
                    .to_string();
            let trait_decl = surfaces[trait_module.as_str()].traits[&trait_ident.node].clone();
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
                    let (capabilities, calls) =
                        analyze_body(&decl.body, &resolved.host_uses, &resolved.host_items);
                    call_sites.insert(
                        FnKey::Method(type_name.clone(), decl.name.node.clone()),
                        calls,
                    );
                    resolved.methods.insert(
                        key,
                        FnEntry {
                            decl: Rc::new(decl.clone()),
                            exported: inner.exported,
                            doc: inner.doc.clone(),
                            receiver_type: Some(type_name.clone()),
                            from_trait_default: None,
                            direct_capabilities: capabilities,
                            required_capabilities: BTreeSet::new(),
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
/// This mirrors the modules `cove_runtime::host` registers, which the
/// compiler cannot ask directly because it does not depend on the runtime —
/// the same arrangement `typeck`'s builtin tables already use. It is only
/// consulted to refuse a package module that would shadow a host module: a
/// `use` naming an unknown host module is still accepted, since a host may
/// register any module it likes.
const HOST_MODULES: [&str; 4] = ["clock", "console", "documents", "env"];

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
    traits: BTreeMap<String, Rc<TraitDecl>>,
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
        let mut traits: BTreeMap<String, Rc<TraitDecl>> = BTreeMap::new();
        for unit in &module.units {
            for item in &unit.ast.items {
                if let ItemKind::Trait(decl) = &item.kind {
                    traits
                        .entry(decl.name.node.clone())
                        .or_insert_with(|| Rc::new(decl.clone()));
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
    errors: &mut Vec<Diagnostic>,
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
                if let Some(diagnostic) = shadowed_host(&path, span) {
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
                    if let Some(diagnostic) = shadowed_host(&owner, span) {
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
                    uses.host_uses.insert(path);
                }
                2 => {
                    let host = segments[0].to_string();
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

/// Refuses a module that shares its name with a host module.
///
/// Modules resolve first, so such a module would silently make the host
/// module unreachable for the whole package.
fn shadowed_host(module: &str, span: Span) -> Option<Diagnostic> {
    if !HOST_MODULES.contains(&module) {
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
    trait_decl: Rc<TraitDecl>,
    method_spans: &mut BTreeMap<(String, String), Span>,
    call_sites: &mut BTreeMap<FnKey, Vec<CallShape>>,
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
            Rc::new(decl.clone()),
            trait_exported,
            declared.doc.clone().or_else(|| inner.doc.clone()),
            None,
            method_spans,
            call_sites,
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
        let decl = Rc::new(FnDecl {
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
    decl: Rc<FnDecl>,
    exported: bool,
    doc: Option<String>,
    from_trait_default: Option<String>,
    method_spans: &mut BTreeMap<(String, String), Span>,
    call_sites: &mut BTreeMap<FnKey, Vec<CallShape>>,
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
    let (capabilities, calls) = analyze_body(&decl.body, &resolved.host_uses, &resolved.host_items);
    call_sites.insert(
        FnKey::Method(type_name.to_string(), decl.name.node.clone()),
        calls,
    );
    resolved.methods.insert(
        key,
        FnEntry {
            decl,
            exported,
            doc,
            receiver_type: Some(type_name.to_string()),
            from_trait_default,
            direct_capabilities: capabilities,
            required_capabilities: BTreeSet::new(),
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

/// Derives the Host API capabilities a function body calls directly, plus
/// the raw call sites found in it (used to build the module's call graph in
/// a later pass).
///
/// This only looks at calls textually inside `body` (including nested
/// blocks, lambdas, match arms, and loops). `enums` is `None` here: a
/// module's enums are not all known yet at this point (see [`BodyWalk`]), so
/// `match` exhaustiveness is not checked during this walk.
fn analyze_body(
    body: &Block,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
) -> (BTreeSet<Capability>, Vec<CallShape>) {
    let mut walk = BodyWalk {
        host_uses,
        host_items,
        enums: None,
        capabilities: BTreeSet::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        loop_depth: 0,
    };
    walk_block(body, &mut walk);
    (walk.capabilities, walk.calls)
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
fn check_bodies(program: &Program, errors: &mut Vec<Diagnostic>, warnings: &mut Vec<Diagnostic>) {
    for resolved in program.modules.values() {
        let enums = enums_in_scope(program, resolved);
        for entry in resolved.functions.values() {
            check_body(&entry.decl.body, resolved, &enums, errors, warnings);
        }
        for entry in resolved.methods.values() {
            // A default body belongs to the trait that declares it, so it is
            // walked once below rather than once per conformance — and in
            // the module that declares the trait, whose enums are the ones
            // its arms can name.
            if entry.from_trait_default.is_none() {
                check_body(&entry.decl.body, resolved, &enums, errors, warnings);
            }
        }
        for entry in resolved.traits.values() {
            for method in &entry.decl.methods {
                if let Some(body) = &method.default {
                    check_body(body, resolved, &enums, errors, warnings);
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
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    let mut walk = BodyWalk {
        host_uses: &resolved.host_uses,
        host_items: &resolved.host_items,
        enums: Some(enums),
        capabilities: BTreeSet::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        loop_depth: 0,
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
}

/// A node in a module's call graph: a free function or an `impl` method.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FnKey {
    Fn(String),
    Method(String, String),
}

fn walk_block(block: &Block, walk: &mut BodyWalk) {
    for stmt in &block.statements {
        walk_stmt(stmt, walk);
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, walk);
    }
}

fn walk_stmt(stmt: &Stmt, walk: &mut BodyWalk) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr(value, walk),
        StmtKind::Expr(expr) => walk_expr(expr, walk),
        // A nested declaration (such as a local `fn`) is its own scope; it is
        // resolved and walked on its own, not as part of the enclosing body.
        StmtKind::Item(_) => {}
    }
}

fn walk_expr(expr: &Expr, walk: &mut BodyWalk) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit
        | ExprKind::Ident(_) => {}
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
            if let Some(capability) = call_capability(callee, walk.host_uses, walk.host_items) {
                walk.capabilities.insert(capability);
            }
            if let Some(shape) = call_shape(callee) {
                walk.calls.push(shape);
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
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, walk);
            walk.loop_depth += 1;
            walk_block(body, walk);
            walk.loop_depth -= 1;
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
        ExprKind::Lambda { body, .. } => {
            let outer_depth = std::mem::replace(&mut walk.loop_depth, 0);
            walk_block(body, walk);
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
    Option,
    Result,
}

impl TargetEnum<'_> {
    fn display_name(&self) -> &str {
        match self {
            TargetEnum::Declared(entry) => &entry.decl.name.node,
            TargetEnum::Option => "Option",
            TargetEnum::Result => "Result",
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
            TargetEnum::Option => vec!["Some".to_string(), "None".to_string()],
            TargetEnum::Result => vec!["Ok".to_string(), "Err".to_string()],
        }
    }

    /// How a missing case should read in a diagnostic: qualified for a
    /// module enum (`LogLevel.Warn`), bare for a builtin, since arms write
    /// `Some(x)` and `None`, never `Option.Some(x)`.
    fn qualified(&self, case: &str) -> String {
        match self {
            TargetEnum::Declared(entry) => format!("{}.{case}", entry.decl.name.node),
            TargetEnum::Option | TargetEnum::Result => case.to_string(),
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

    match candidate?.as_str() {
        "Option" => Some(TargetEnum::Option),
        "Result" => Some(TargetEnum::Result),
        other => enums.get(other).copied().map(TargetEnum::Declared),
    }
}

/// The enum a bare case name such as `Debug` names: `Option` or `Result` for
/// their builtin case names, or the one enum in scope whose cases include it.
/// `None` when no enum declares that case, or more than one does.
fn bare_case_enum(case_name: &str, enums: &EnumsInScope) -> Option<String> {
    if case_name == "Some" || case_name == "None" {
        return Some("Option".to_string());
    }
    if case_name == "Ok" || case_name == "Err" {
        return Some("Result".to_string());
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
/// immediately-called lambda, contributes no call-graph edge.
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
type Node = (String, FnKey);

/// Resolves every call site recorded while the modules were resolved to the
/// declarations it may reach, anywhere in the package.
fn package_call_graph(
    program: &Program,
    call_sites: &BTreeMap<Node, Vec<CallShape>>,
) -> BTreeMap<Node, BTreeSet<Node>> {
    let reachable: BTreeMap<&str, BTreeSet<&str>> = program
        .modules
        .keys()
        .map(|name| (name.as_str(), reachable_modules(program, name)))
        .collect();
    call_sites
        .iter()
        .map(|((module, key), calls)| {
            (
                (module.clone(), key.clone()),
                resolve_calls(program, module, calls, &reachable[module.as_str()]),
            )
        })
        .collect()
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
fn resolve_calls(
    program: &Program,
    module: &str,
    calls: &[CallShape],
    reachable: &BTreeSet<&str>,
) -> BTreeSet<Node> {
    let Some(resolved) = program.modules.get(module) else {
        return BTreeSet::new();
    };
    let mut targets = BTreeSet::new();
    for call in calls {
        match call {
            CallShape::Ident(name) => {
                if resolved.functions.contains_key(name) {
                    targets.insert((module.to_string(), FnKey::Fn(name.clone())));
                } else if let Some(owner) = declaring_module(program, resolved, name, |owner| {
                    owner.functions.contains_key(name)
                }) {
                    targets.insert((owner, FnKey::Fn(name.clone())));
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
                    targets.extend(type_methods(program, &owner, &type_name, method));
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
                            targets.insert((target.clone(), FnKey::Fn(method.clone())));
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
                            targets.insert((
                                (*name).to_string(),
                                FnKey::Method(type_name.clone(), method_name.clone()),
                            ));
                        }
                    }
                }
            }
        }
    }
    targets
}

/// The declarations `type_module.type_name`'s method `method` may reach.
///
/// A type's methods usually live in the module that declares the type. A
/// conformance is the exception: ADR 0006 allows `impl Trait for Type` in the
/// module that declares the *trait*, so a method of this type can live in any
/// module that conforms it to a trait of its own.
fn type_methods(
    program: &Program,
    type_module: &str,
    type_name: &str,
    method: &str,
) -> BTreeSet<Node> {
    let key = (type_name.to_string(), method.to_string());
    let mut found = BTreeSet::new();
    if let Some(owner) = program.modules.get(type_module) {
        if owner.methods.contains_key(&key) {
            found.insert((
                type_module.to_string(),
                FnKey::Method(key.0.clone(), key.1.clone()),
            ));
        }
    }
    for (name, resolved) in &program.modules {
        let conforms = resolved.conformances.values().any(|conformance| {
            conformance.type_module == type_module
                && conformance.type_name == type_name
                && conformance.methods.contains(method)
        });
        if conforms && resolved.methods.contains_key(&key) {
            found.insert((name.clone(), FnKey::Method(key.0.clone(), key.1.clone())));
        }
    }
    found
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
/// A fixed point rather than a recursive walk is required because the call
/// graph can be cyclic: direct and mutual recursion must not recurse forever.
/// Module imports may not form a cycle, but calls within a module still may.
/// Each round only ever adds capabilities to a finite set, so the loop is
/// guaranteed to terminate.
fn propagate_capabilities(program: &mut Program, call_graph: &BTreeMap<Node, BTreeSet<Node>>) {
    let mut required: BTreeMap<Node, BTreeSet<Capability>> = BTreeMap::new();
    for (module, resolved) in &program.modules {
        for (name, entry) in &resolved.functions {
            required.insert(
                (module.clone(), FnKey::Fn(name.clone())),
                entry.direct_capabilities.clone(),
            );
        }
        for ((type_name, method_name), entry) in &resolved.methods {
            required.insert(
                (
                    module.clone(),
                    FnKey::Method(type_name.clone(), method_name.clone()),
                ),
                entry.direct_capabilities.clone(),
            );
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
            for callee in callees {
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
        }
        if !changed {
            break;
        }
    }

    for (module, resolved) in program.modules.iter_mut() {
        for (name, entry) in resolved.functions.iter_mut() {
            entry.required_capabilities = required
                .remove(&(module.clone(), FnKey::Fn(name.clone())))
                .unwrap_or_default();
        }
        for ((type_name, method_name), entry) in resolved.methods.iter_mut() {
            entry.required_capabilities = required
                .remove(&(
                    module.clone(),
                    FnKey::Method(type_name.clone(), method_name.clone()),
                ))
                .unwrap_or_default();
        }
    }
}

/// If `callee` is a call to a host module (`console.println(...)`) or an
/// unqualified host item (`println(...)`), the capability it requires.
fn call_capability(
    callee: &Expr,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
) -> Option<Capability> {
    match &callee.kind {
        ExprKind::Field { base, .. } => {
            if let ExprKind::Ident(name) = &base.kind {
                if host_uses.contains(name.as_str()) {
                    return Some(Capability::new(name.clone()));
                }
            }
            None
        }
        ExprKind::Ident(name) => host_items
            .get(name)
            .map(|host| Capability::new(host.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::package::{Module, Unit};
    use cove_diag::SourceMap;
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

    #[test]
    fn warns_on_an_exported_trait_and_its_methods_without_docs() {
        let package = package_of(module_from_sources(
            "render",
            &["export trait Display {\n  fn describe(self) -> String\n}\n"],
        ));
        let program = resolve(&package).expect("resolves");
        let names: Vec<&str> = program
            .warnings
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
        assert!(program.warnings.is_empty());
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
            .warnings
            .iter()
            .any(|d| d.code == "cove::resolve::missing_doc"));
    }

    #[test]
    fn private_declaration_without_doc_does_not_warn() {
        let module = module_from_sources("private", &["fn helper() {\n}\n"]);
        let package = package_of(module);
        let program = resolve(&package).expect("resolves");
        assert!(program.warnings.is_empty());
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
        assert!(program.warnings.is_empty());
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
            .warnings
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
            .warnings
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
            .warnings
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
            .warnings
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
            .warnings
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
            .warnings
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
