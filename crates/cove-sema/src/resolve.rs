//! Name resolution across the units of a module.
//!
//! Resolution produces the flat program the runtime executes and the derived
//! facts (`export` visibility, required capabilities) that tooling reports.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use cove_diag::{Diagnostic, Span};
use cove_syntax::ast::{
    Block, EnumDecl, Expr, ExprKind, FnDecl, Item, ItemKind, MatchArm, Pattern, PatternKind, Stmt,
    StmtKind, StrPart, StructDecl, TypeAlias,
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

    // Pass 5: `match` exhaustiveness and case-name checks, now that every
    // enum in the module is known. This walks every body a second time with
    // the same walker as passes 2 and 3, just with `enums` filled in.
    check_module_matches(&resolved, errors, warnings);

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
    };
    walk_block(body, &mut walk);
    (walk.capabilities, walk.calls)
}

/// Checks every `match` expression in every function and method body of
/// `resolved` for the exhaustiveness and case-name facts derivable without a
/// type checker, now that every enum the module declares is known.
///
/// This reuses [`walk_block`] rather than a second traversal: the only
/// difference from the walk [`analyze_body`] already did is that `enums` is
/// filled in this time, so [`check_match_arms`] actually runs.
fn check_module_matches(
    resolved: &ResolvedModule,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    for entry in resolved.functions.values() {
        check_body_matches(&entry.decl.body, resolved, errors, warnings);
    }
    for entry in resolved.methods.values() {
        check_body_matches(&entry.decl.body, resolved, errors, warnings);
    }
}

fn check_body_matches(
    body: &Block,
    resolved: &ResolvedModule,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    let mut walk = BodyWalk {
        host_uses: &resolved.host_uses,
        host_items: &resolved.host_items,
        enums: Some(&resolved.enums),
        capabilities: BTreeSet::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
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
    enums: Option<&'a BTreeMap<String, EnumEntry>>,
    capabilities: BTreeSet<Capability>,
    calls: Vec<CallShape>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
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
            walk_block(body, walk);
        }
        ExprKind::While { condition, body } => {
            walk_expr(condition, walk);
            walk_block(body, walk);
        }
        ExprKind::Return(inner) => {
            if let Some(inner) = inner {
                walk_expr(inner, walk);
            }
        }
        ExprKind::Lambda { body, .. } => walk_block(body, walk),
        ExprKind::Scope { body, .. } => walk_block(body, walk),
        ExprKind::Range { start, end, .. } => {
            walk_expr(start, walk);
            walk_expr(end, walk);
        }
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
/// which enum they name, a bare case name matches no declared enum or more
/// than one, or the settled-on enum is not declared in this module (which
/// also excludes any enum imported from elsewhere; there is no
/// module-to-module import resolution yet).
fn resolve_target_enum<'a>(
    arms: &[MatchArm],
    enums: &'a BTreeMap<String, EnumEntry>,
) -> Option<TargetEnum<'a>> {
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
        other => enums.get(other).map(TargetEnum::Declared),
    }
}

/// The enum a bare case name such as `Debug` names: `Option` or `Result` for
/// their builtin case names, or the one module enum whose cases include it.
/// `None` when no enum declares that case, or more than one does.
fn bare_case_enum(case_name: &str, enums: &BTreeMap<String, EnumEntry>) -> Option<String> {
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
        .map(|(name, _)| name.clone());
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
}
