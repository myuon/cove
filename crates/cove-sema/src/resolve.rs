//! Name resolution across the units of a module.
//!
//! Resolution produces the flat program the runtime executes and the derived
//! facts (`export` visibility, required capabilities) that tooling reports.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use cove_diag::{Diagnostic, Span};
use cove_syntax::ast::{
    Block, EnumDecl, Expr, ExprKind, FnDecl, Item, ItemKind, MatchArm, Stmt, StmtKind, StrPart,
    StructDecl, TypeAlias,
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
    /// Capabilities used directly in this function's body.
    pub direct_capabilities: BTreeSet<Capability>,
    /// Capabilities this function requires, including those reached through
    /// calls to other declarations in the same module.
    ///
    /// A call through a field access (`receiver.method(...)`) whose receiver
    /// is not a bare reference to a struct or enum this module declares is
    /// resolved to *every* method in the module sharing that name. There is
    /// no static type checker yet to narrow the receiver's actual type, so
    /// this is a deliberate over-approximation: it can report a capability a
    /// function does not really need, but never omits one it does. Static
    /// type checking would let this be exact.
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
    pub aliases: BTreeMap<String, AliasEntry>,
    /// Host modules named by `use`, such as `console` from `use console.println`.
    pub host_uses: BTreeSet<String>,
    /// Names imported unqualified by `use`, such as `println` -> `console`.
    pub host_items: BTreeMap<String, String>,
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

/// Resolves every module of `package`, merging its implementation units and
/// deriving `use` bindings and direct Host API capabilities.
pub fn resolve(package: &Package) -> Result<Program, Vec<Diagnostic>> {
    let mut program = Program::default();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for (name, module) in &package.modules {
        let resolved = resolve_module(name, module, &mut errors, &mut warnings);
        program.modules.insert(name.clone(), resolved);
    }

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
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) -> ResolvedModule {
    let mut resolved = ResolvedModule {
        name: name.to_string(),
        ..ResolvedModule::default()
    };

    // Pass 1: `use` declarations. These must be known before we can derive
    // capabilities for any function body in pass 2.
    let mut host_item_origin: BTreeMap<String, (String, Span)> = BTreeMap::new();
    for unit in &module.units {
        for use_decl in &unit.ast.uses {
            let segments: Vec<&str> = use_decl.path.iter().map(|i| i.node.as_str()).collect();
            match segments.len() {
                1 => {
                    resolved.host_uses.insert(segments[0].to_string());
                }
                2 => {
                    let host = segments[0].to_string();
                    let item = segments[1].to_string();
                    resolved.host_uses.insert(host.clone());
                    match host_item_origin.get(&item) {
                        Some((existing_host, existing_span)) if existing_host != &host => {
                            errors.push(
                                Diagnostic::error(
                                    "cove::resolve::ambiguous_use",
                                    format!(
                                        "`{item}` is imported from both `{existing_host}` and `{host}`"
                                    ),
                                )
                                .at(use_decl.span)
                                .label(
                                    *existing_span,
                                    format!("first imported from `{existing_host}` here"),
                                )
                                .rule(
                                    "An unqualified `use` name must resolve to exactly one host module.",
                                )
                                .help(format!(
                                    "Use `{host}.{item}` explicitly wherever it should not mean `{existing_host}.{item}`."
                                )),
                            );
                        }
                        _ => {
                            host_item_origin.insert(item.clone(), (host.clone(), use_decl.span));
                            resolved.host_items.insert(item, host);
                        }
                    }
                }
                _ => {
                    errors.push(
                        Diagnostic::error(
                            "cove::resolve::unsupported_use",
                            format!("`use {}` names more than two segments", segments.join(".")),
                        )
                        .at(use_decl.span)
                        .rule(
                            "`use` names a host module or one host item; module-to-module imports are not supported yet.",
                        ),
                    );
                }
            }
        }
    }

    // Pass 2: top-level declarations, merged across every unit of the module.
    let mut fn_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut struct_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut enum_spans: BTreeMap<String, Span> = BTreeMap::new();
    let mut alias_spans: BTreeMap<String, Span> = BTreeMap::new();
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

    // Pass 3: `impl` blocks, once every struct and enum in the module is known.
    let mut method_spans: BTreeMap<(String, String), Span> = BTreeMap::new();
    for (impl_block, _impl_span) in pending_impls {
        let type_name = impl_block.type_name.node.clone();
        if !resolved.structs.contains_key(&type_name) && !resolved.enums.contains_key(&type_name) {
            errors.push(
                Diagnostic::error(
                    "cove::resolve::unknown_impl_type",
                    format!("`impl {type_name}` names a type module `{name}` does not declare"),
                )
                .at(impl_block.type_name.span)
                .rule("An `impl` block extends a struct or enum declared in the same module.")
                .help(format!(
                    "Declare `struct {type_name}` or `enum {type_name}` in this module, or fix the name."
                )),
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

    // Pass 4: derive `required_capabilities` as the least fixed point of the
    // module's call graph, now that every function, method, struct, and enum
    // is known.
    let call_graph: BTreeMap<FnKey, BTreeSet<FnKey>> = call_sites
        .into_iter()
        .map(|(key, calls)| (key, resolve_calls(&calls, &resolved)))
        .collect();
    propagate_capabilities(&mut resolved, &call_graph);

    resolved
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
        warnings.push(
            Diagnostic::warning(
                "cove::resolve::missing_doc",
                format!("exported `{name}` has no doc comment"),
            )
            .at(span)
            .rule("Public declarations without doc comments warn by default.")
            .help(format!("Add a `///` doc comment above `{name}`.")),
        );
    }
}

/// Derives the Host API capabilities a function body calls directly, plus
/// the raw call sites found in it (used to build the module's call graph in
/// a later pass).
///
/// This only looks at calls textually inside `body` (including nested
/// blocks, lambdas, match arms, and loops).
fn analyze_body(
    body: &Block,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
) -> (BTreeSet<Capability>, Vec<CallShape>) {
    let mut capabilities = BTreeSet::new();
    let mut calls = Vec::new();
    walk_block(body, host_uses, host_items, &mut capabilities, &mut calls);
    (capabilities, calls)
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

fn walk_block(
    block: &Block,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
    out: &mut BTreeSet<Capability>,
    out_calls: &mut Vec<CallShape>,
) {
    for stmt in &block.statements {
        walk_stmt(stmt, host_uses, host_items, out, out_calls);
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, host_uses, host_items, out, out_calls);
    }
}

fn walk_stmt(
    stmt: &Stmt,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
    out: &mut BTreeSet<Capability>,
    out_calls: &mut Vec<CallShape>,
) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr(value, host_uses, host_items, out, out_calls),
        StmtKind::Expr(expr) => walk_expr(expr, host_uses, host_items, out, out_calls),
        // A nested declaration (such as a local `fn`) is its own scope; it is
        // resolved and walked on its own, not as part of the enclosing body.
        StmtKind::Item(_) => {}
    }
}

fn walk_expr(
    expr: &Expr,
    host_uses: &BTreeSet<String>,
    host_items: &BTreeMap<String, String>,
    out: &mut BTreeSet<Capability>,
    out_calls: &mut Vec<CallShape>,
) {
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
                    walk_expr(inner, host_uses, host_items, out, out_calls);
                }
            }
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                walk_expr(item, host_uses, host_items, out, out_calls);
            }
        }
        ExprKind::Field { base, .. } => walk_expr(base, host_uses, host_items, out, out_calls),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            if let Some(capability) = call_capability(callee, host_uses, host_items) {
                out.insert(capability);
            }
            if let Some(shape) = call_shape(callee) {
                out_calls.push(shape);
            }
            walk_expr(callee, host_uses, host_items, out, out_calls);
            for arg in args {
                walk_expr(&arg.value, host_uses, host_items, out, out_calls);
            }
            if let Some(trailing) = trailing {
                walk_expr(trailing, host_uses, host_items, out, out_calls);
            }
        }
        ExprKind::Unary { operand, .. } => {
            walk_expr(operand, host_uses, host_items, out, out_calls)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, host_uses, host_items, out, out_calls);
            walk_expr(rhs, host_uses, host_items, out, out_calls);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_expr(target, host_uses, host_items, out, out_calls);
            walk_expr(value, host_uses, host_items, out, out_calls);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => {
            walk_expr(inner, host_uses, host_items, out, out_calls)
        }
        ExprKind::Block(block) => walk_block(block, host_uses, host_items, out, out_calls),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, host_uses, host_items, out, out_calls);
            walk_block(then_branch, host_uses, host_items, out, out_calls);
            if let Some(else_branch) = else_branch {
                walk_expr(else_branch, host_uses, host_items, out, out_calls);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, host_uses, host_items, out, out_calls);
            for MatchArm { body, .. } in arms {
                walk_expr(body, host_uses, host_items, out, out_calls);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, host_uses, host_items, out, out_calls);
            walk_block(body, host_uses, host_items, out, out_calls);
        }
        ExprKind::While { condition, body } => {
            walk_expr(condition, host_uses, host_items, out, out_calls);
            walk_block(body, host_uses, host_items, out, out_calls);
        }
        ExprKind::Return(inner) => {
            if let Some(inner) = inner {
                walk_expr(inner, host_uses, host_items, out, out_calls);
            }
        }
        ExprKind::Lambda { body, .. } => walk_block(body, host_uses, host_items, out, out_calls),
        ExprKind::Scope { body, .. } => walk_block(body, host_uses, host_items, out, out_calls),
        ExprKind::Range { start, end, .. } => {
            walk_expr(start, host_uses, host_items, out, out_calls);
            walk_expr(end, host_uses, host_items, out, out_calls);
        }
    }
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

/// Resolves the raw call sites found in one declaration's body to the
/// declarations of `resolved` they may call.
///
/// A bare-name call resolves to the module's free function of that name, if
/// any. A field-access call whose receiver is a bare identifier naming a
/// struct or enum this module declares resolves precisely to that type's
/// method. Every other field-access call — a receiver that is `self`, a
/// local variable, or any other expression whose type is unknown without a
/// type checker — resolves to *every* method in the module sharing that
/// name. That is a deliberate over-approximation: it can name a capability a
/// call site does not really reach, but never misses one.
fn resolve_calls(calls: &[CallShape], resolved: &ResolvedModule) -> BTreeSet<FnKey> {
    let mut targets = BTreeSet::new();
    for call in calls {
        match call {
            CallShape::Ident(name) => {
                if resolved.functions.contains_key(name) {
                    targets.insert(FnKey::Fn(name.clone()));
                }
            }
            CallShape::Field {
                receiver_ident,
                method,
            } => {
                let known_type = receiver_ident.as_ref().filter(|type_name| {
                    resolved.structs.contains_key(type_name.as_str())
                        || resolved.enums.contains_key(type_name.as_str())
                });
                if let Some(type_name) = known_type {
                    if resolved
                        .methods
                        .contains_key(&(type_name.clone(), method.clone()))
                    {
                        targets.insert(FnKey::Method(type_name.clone(), method.clone()));
                    }
                } else {
                    for (type_name, method_name) in resolved.methods.keys() {
                        if method_name == method {
                            targets.insert(FnKey::Method(type_name.clone(), method_name.clone()));
                        }
                    }
                }
            }
        }
    }
    targets
}

/// Fills in `required_capabilities` on every function and method of
/// `resolved` as the least fixed point of "start from what a declaration
/// calls directly, then union in whatever every declaration it (transitively)
/// calls requires."
///
/// A fixed point rather than a recursive walk is required because the call
/// graph can be cyclic: direct and mutual recursion must not recurse forever.
/// Each round only ever adds capabilities to a finite set, so the loop is
/// guaranteed to terminate.
fn propagate_capabilities(
    resolved: &mut ResolvedModule,
    call_graph: &BTreeMap<FnKey, BTreeSet<FnKey>>,
) {
    let mut required: BTreeMap<FnKey, BTreeSet<Capability>> = BTreeMap::new();
    for (name, entry) in &resolved.functions {
        required.insert(FnKey::Fn(name.clone()), entry.direct_capabilities.clone());
    }
    for ((type_name, method_name), entry) in &resolved.methods {
        required.insert(
            FnKey::Method(type_name.clone(), method_name.clone()),
            entry.direct_capabilities.clone(),
        );
    }

    let keys: Vec<FnKey> = required.keys().cloned().collect();
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

    for (name, entry) in resolved.functions.iter_mut() {
        entry.required_capabilities = required
            .remove(&FnKey::Fn(name.clone()))
            .unwrap_or_default();
    }
    for ((type_name, method_name), entry) in resolved.methods.iter_mut() {
        entry.required_capabilities = required
            .remove(&FnKey::Method(type_name.clone(), method_name.clone()))
            .unwrap_or_default();
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
        let mut modules = BTreeMap::new();
        modules.insert(module.name.clone(), module);
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        }
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
    fn three_segment_use_is_unsupported() {
        let module = module_from_sources("toolong", &["use a.b.c\n"]);
        let package = package_of(module);
        let errs = resolve(&package).unwrap_err();
        assert!(errs
            .iter()
            .any(|d| d.code == "cove::resolve::unsupported_use"));
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
}
