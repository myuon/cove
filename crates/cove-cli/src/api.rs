//! `cove api snapshot` and `cove api diff`: the derived public interface,
//! recorded and compared.
//!
//! Exported declarations are the single source of truth, so a snapshot is
//! derived entirely from source and duplicates nothing by hand. It records
//! what a caller can depend on — the exported declarations of every module,
//! their types, the traits their types conform to, and the capabilities they
//! require — and nothing that only describes where the source happens to sit
//! today. Definition locations, declaration order within a file, and
//! module-private declarations are all left out: none of them can break a
//! caller, and every one of them would put noise in the diff a reviewer
//! reads.
//!
//! # The interface hash
//!
//! The `hash` line covers the snapshot body with its `///` doc lines
//! removed, and nothing else. Concretely it covers every module name, every
//! exported declaration's header (its kind, name, generic parameters,
//! parameter labels and types, return type, and `async`), every struct
//! field, enum case, and trait requirement in declaration order, every alias
//! target, every trait conformance, and every required capability. It does
//! not cover doc comments, definition locations, declaration order in
//! source, module-private declarations, or any function body. A doc change
//! therefore leaves the hash alone, and a capability change does not.
//!
//! The digest is FNV-1a/64, which is a change detector rather than a
//! security boundary: it says "this interface is not the one that was
//! recorded", and `cove api diff` says what changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cove_sema::resolve::{FnEntry, Program};

use crate::{fn_signature, generics_suffix, load, CliError};

/// The file `cove api snapshot` writes and `cove api diff` reads unless
/// `--out` or `--against` names another.
pub(crate) const DEFAULT_SNAPSHOT: &str = "cove-api.txt";

/// The format marker every snapshot starts with, so a later format can be
/// recognised rather than misread.
const FORMAT: &str = "cove-api 1";

/// The comment block a written snapshot opens with, telling a reader what
/// the file is for and that it is not edited by hand.
const PREAMBLE: &str = "\
# The public interface of this package, derived from source.
#
# Written by `cove api snapshot`. `cove api diff` compares this recording
# against the interface the current source derives, so check it in and let a
# reviewer read the difference. Do not edit it by hand.
";

// ------------------------------------------------------------------ command

/// Runs `cove api <subcommand>`.
pub(crate) fn cmd_api(args: &[String]) -> Result<(), CliError> {
    let Some(subcommand) = args.first() else {
        return Err(CliError::Message(
            "`cove api` needs a subcommand: `snapshot` or `diff`".into(),
        ));
    };
    match subcommand.as_str() {
        "snapshot" => cmd_snapshot(&args[1..]),
        "diff" => cmd_diff(&args[1..]),
        other => Err(CliError::Message(format!(
            "unknown `cove api` subcommand `{other}`; the subcommands are `snapshot` and `diff`"
        ))),
    }
}

/// Writes the package's derived public interface to a file.
fn cmd_snapshot(args: &[String]) -> Result<(), CliError> {
    let (path, out) = parse_args(args, "--out")?;
    let (_, package, program) = load(path.as_deref())?;
    let interface = derive(&program);
    let text = interface.to_snapshot();

    let target = out.unwrap_or_else(|| package.root.join(DEFAULT_SNAPSHOT));
    std::fs::write(&target, &text)
        .map_err(|e| CliError::Message(format!("cannot write `{}`: {e}", target.display())))?;
    println!("{}", snapshot_summary(&target, &interface));
    Ok(())
}

/// The one-line summary `cove api snapshot` prints to stdout.
fn snapshot_summary(target: &Path, interface: &Interface) -> String {
    let declarations: usize = interface
        .modules
        .values()
        .map(|decls| decls.iter().map(Decl::count).sum::<usize>())
        .sum();
    format!(
        "wrote {}: {} module(s), {declarations} declaration(s), hash {}",
        target.display(),
        interface.modules.len(),
        interface.hash()
    )
}

/// Compares the current source's derived interface against a recording.
fn cmd_diff(args: &[String]) -> Result<(), CliError> {
    let (path, against) = parse_args(args, "--against")?;
    let (_, package, program) = load(path.as_deref())?;
    let recorded_path = against.unwrap_or_else(|| package.root.join(DEFAULT_SNAPSHOT));

    let Ok(text) = std::fs::read_to_string(&recorded_path) else {
        return Err(CliError::Message(format!(
            "no API snapshot at `{}`\n  record one with `cove api snapshot`, and check it in so \
             `cove api diff` has something to compare against",
            recorded_path.display()
        )));
    };
    let recorded = parse(&text)
        .map_err(|e| CliError::Message(format!("`{}`: {e}", recorded_path.display())))?;

    let current = derive(&program);
    let changes = diff(&recorded, &current);
    print!("{}", render_diff(&recorded, &current, &changes));

    if changes.iter().any(|c| c.severity == Severity::Breaking) {
        return Err(CliError::BreakingChange);
    }
    Ok(())
}

/// Splits `[path] [<flag> <file>]` into the package path and the file the
/// flag names.
fn parse_args(args: &[String], flag: &str) -> Result<(Option<PathBuf>, Option<PathBuf>), CliError> {
    let mut path = None;
    let mut file = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let value = args
                .get(i + 1)
                .ok_or_else(|| CliError::Message(format!("`{flag}` needs a value")))?;
            file = Some(PathBuf::from(value));
            i += 2;
            continue;
        }
        if let Some(other) = args[i].strip_prefix("--") {
            return Err(CliError::Message(format!(
                "unknown flag `--{other}`; this command takes `{flag} <file>`"
            )));
        }
        path = Some(PathBuf::from(&args[i]));
        i += 1;
    }
    Ok((path, file))
}

// ------------------------------------------------------------------ the model

/// The public interface of a package: every module with at least one
/// exported declaration, and what it exports.
///
/// A module that exports nothing has no interface, so it does not appear: a
/// package can add or remove one without changing what any caller may
/// depend on.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Interface {
    modules: BTreeMap<String, Vec<Decl>>,
}

/// One exported declaration, or one exported method of an exported type.
#[derive(Clone, Debug, PartialEq)]
struct Decl {
    /// The `export ...` header, written in source form.
    header: String,
    /// The declaration's `///` doc, one entry per line.
    doc: Vec<String>,
    /// The capabilities this declaration requires, in name order. Only a
    /// function or method has any.
    requires: Vec<String>,
    /// Fields, enum cases, and trait requirements, in declaration order:
    /// their order is part of the contract, because it decides a synthesized
    /// initializer's positional arguments and an enum payload's positions.
    members: Vec<String>,
    /// `module.Trait` for every trait this type conforms to, in name order.
    conforms: Vec<String>,
    /// The type's exported methods, in name order.
    methods: Vec<Decl>,
}

impl Decl {
    /// The name this declaration is known by inside its module.
    fn name(&self) -> &str {
        header_name(&self.header)
    }

    /// This declaration and every method under it.
    fn count(&self) -> usize {
        1 + self.methods.len()
    }
}

/// The name a header declares: `export async fn greet(name: String)` names
/// `greet`, and `export struct Widget<T>` names `Widget`.
fn header_name(header: &str) -> &str {
    let mut rest = header;
    for prefix in ["export ", "async "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let rest = rest.split_once(' ').map_or(rest, |(_, tail)| tail);
    rest.split(['(', '<', ' ', '='])
        .next()
        .unwrap_or_default()
        .trim()
}

// ------------------------------------------------------------------ deriving

/// Derives the package's public interface from the facts resolution already
/// produced.
pub(crate) fn derive(program: &Program) -> Interface {
    let mut interface = Interface::default();
    for (name, resolved) in &program.modules {
        let mut decls: Vec<Decl> = Vec::new();

        for entry in resolved.functions.values().filter(|e| e.exported) {
            decls.push(function_decl(entry));
        }
        for (type_name, entry) in resolved.structs.iter().filter(|(_, e)| e.exported) {
            let members = entry
                .decl
                .fields
                .iter()
                .map(|field| format!("field {}: {}", field.name.node, field.ty))
                .collect();
            decls.push(type_decl(
                program,
                name,
                type_name,
                format!(
                    "export struct {type_name}{}",
                    generics_suffix(&entry.decl.generics)
                ),
                entry.doc.clone(),
                members,
            ));
        }
        for (type_name, entry) in resolved.enums.iter().filter(|(_, e)| e.exported) {
            let members = entry
                .decl
                .cases
                .iter()
                .map(|case| {
                    if case.payload.is_empty() {
                        format!("case {}", case.name.node)
                    } else {
                        let payload: Vec<String> =
                            case.payload.iter().map(ToString::to_string).collect();
                        format!("case {}({})", case.name.node, payload.join(", "))
                    }
                })
                .collect();
            decls.push(type_decl(
                program,
                name,
                type_name,
                format!(
                    "export enum {type_name}{}",
                    generics_suffix(&entry.decl.generics)
                ),
                entry.doc.clone(),
                members,
            ));
        }
        for (trait_name, entry) in resolved.traits.iter().filter(|(_, e)| e.exported) {
            decls.push(Decl {
                header: format!("export trait {trait_name}"),
                doc: doc_lines(&entry.doc),
                requires: Vec::new(),
                members: entry.decl.methods.iter().map(requirement_line).collect(),
                conforms: Vec::new(),
                methods: Vec::new(),
            });
        }
        for (alias_name, entry) in resolved.aliases.iter().filter(|(_, e)| e.exported) {
            decls.push(Decl {
                header: format!(
                    "export type {alias_name}{} = {}",
                    generics_suffix(&entry.decl.generics),
                    entry.decl.ty
                ),
                doc: doc_lines(&entry.doc),
                requires: Vec::new(),
                members: Vec::new(),
                conforms: Vec::new(),
                methods: Vec::new(),
            });
        }

        if decls.is_empty() {
            continue;
        }
        // Name order rather than source order: moving a declaration within
        // a file does not change the interface, so it should not change the
        // snapshot. The header breaks a tie, since a module may declare a
        // type and a function of one name.
        decls.sort_by(|a, b| (a.name(), &a.header).cmp(&(b.name(), &b.header)));
        interface.modules.insert(name.clone(), decls);
    }
    interface
}

/// A free function or method's recorded form.
fn function_decl(entry: &FnEntry) -> Decl {
    Decl {
        header: fn_signature(entry),
        doc: doc_lines(&entry.doc),
        requires: entry
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        members: Vec::new(),
        conforms: Vec::new(),
        methods: Vec::new(),
    }
}

/// A struct or enum's recorded form, with the traits it conforms to and the
/// methods it exports.
fn type_decl(
    program: &Program,
    module: &str,
    type_name: &str,
    header: String,
    doc: Option<String>,
    members: Vec<String>,
) -> Decl {
    Decl {
        header,
        doc: doc_lines(&doc),
        requires: Vec::new(),
        members,
        conforms: exported_conformances(program, module, type_name),
        methods: exported_methods(program, module, type_name)
            .into_iter()
            .map(function_decl)
            .collect(),
    }
}

/// One trait requirement, with `(default)` when the trait supplies a body a
/// conformance may leave alone.
fn requirement_line(method: &cove_syntax::ast::TraitMethod) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(receiver) = method.receiver {
        params.push(if receiver.is_var { "var self" } else { "self" }.to_string());
    }
    params.extend(method.params.iter().map(ToString::to_string));
    let ret = match &method.return_type {
        Some(ty) => format!(" -> {ty}"),
        None => String::new(),
    };
    let default = if method.default.is_some() {
        " (default)"
    } else {
        ""
    };
    format!(
        "{}fn {}({}){ret}{default}",
        if method.is_async { "async " } else { "" },
        method.name.node,
        params.join(", ")
    )
}

/// Every trait the type `module.type_name` conforms to, qualified by the
/// module that declares the trait.
///
/// [`Program::conformances_of`] answers where the conformance was written,
/// which need not be either party's own module. A conformance to a
/// module-private trait is dropped: no other module can name the trait, and
/// the conformance supplies no exported method, so it is not part of any
/// interface.
fn exported_conformances(program: &Program, module: &str, type_name: &str) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for (_, conformance) in program.conformances_of(module, type_name) {
        let exported = program
            .modules
            .get(&conformance.trait_module)
            .and_then(|owner| owner.traits.get(&conformance.trait_name))
            .is_some_and(|entry| entry.exported);
        if exported {
            names.insert(format!(
                "{}.{}",
                conformance.trait_module, conformance.trait_name
            ));
        }
    }
    names.into_iter().collect()
}

/// Every exported method of the type `module.type_name`, in name order.
fn exported_methods<'a>(program: &'a Program, module: &str, type_name: &str) -> Vec<&'a FnEntry> {
    program
        .methods_of(module, type_name)
        .into_iter()
        .filter(|declared| declared.entry.exported)
        .map(|declared| declared.entry)
        .collect()
}

/// Splits a doc comment into its lines, or an empty list when there is none.
fn doc_lines(doc: &Option<String>) -> Vec<String> {
    doc.as_deref()
        .map(|doc| doc.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ rendering

impl Interface {
    /// The complete snapshot file: preamble, format marker, interface hash,
    /// and body.
    pub(crate) fn to_snapshot(&self) -> String {
        format!(
            "{PREAMBLE}{FORMAT}\nhash {}\n\n{}",
            self.hash(),
            self.body(true)
        )
    }

    /// The interface hash: FNV-1a/64 over the body with its doc lines
    /// removed, so a doc change leaves it alone and everything else does
    /// not.
    pub(crate) fn hash(&self) -> String {
        format!("fnv1a64:{:016x}", fnv1a64(self.body(false).as_bytes()))
    }

    /// The module blocks, with or without their doc lines.
    fn body(&self, docs: bool) -> String {
        let mut out = String::new();
        for (i, (name, decls)) in self.modules.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("module {name}\n"));
            for decl in decls {
                render_decl(decl, 2, docs, &mut out);
            }
        }
        out
    }
}

/// Renders one declaration: doc, header, required capabilities, members,
/// conformances, and nested methods, each indented under the last.
fn render_decl(decl: &Decl, indent: usize, docs: bool, out: &mut String) {
    if docs {
        for line in &decl.doc {
            // A blank doc line writes `///` alone: a recorded file should
            // not carry trailing whitespace into a reviewer's diff.
            let text = if line.is_empty() {
                String::new()
            } else {
                format!(" {line}")
            };
            out.push_str(&format!("{:indent$}///{text}\n", ""));
        }
    }
    out.push_str(&format!("{:indent$}{}\n", "", decl.header));
    let inner = indent + 2;
    if !decl.requires.is_empty() {
        out.push_str(&format!(
            "{:inner$}requires {}\n",
            "",
            decl.requires.join(", ")
        ));
    }
    for member in &decl.members {
        out.push_str(&format!("{:inner$}{member}\n", ""));
    }
    for conformance in &decl.conforms {
        out.push_str(&format!("{:inner$}conforms {conformance}\n", ""));
    }
    for method in &decl.methods {
        render_decl(method, inner, docs, out);
    }
}

/// FNV-1a/64, a small deterministic change detector with no dependency.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ------------------------------------------------------------------ parsing

/// Reads a recorded snapshot back into the interface it recorded.
pub(crate) fn parse(text: &str) -> Result<Interface, String> {
    let mut lines = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .skip_while(|line| line.is_empty());

    match lines.next() {
        Some(FORMAT) => {}
        Some(other) => {
            return Err(format!(
                "is not a `{FORMAT}` snapshot; its first line is `{other}`"
            ))
        }
        None => return Err("is empty".to_string()),
    }
    match lines.next() {
        Some(line) if line.starts_with("hash ") => {}
        _ => return Err("has no `hash` line".to_string()),
    }

    let mut interface = Interface::default();
    let mut module: Option<String> = None;
    let mut decls: Vec<Decl> = Vec::new();
    let mut doc: Vec<String> = Vec::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let content = line.trim_start();

        if indent == 0 {
            let name = content
                .strip_prefix("module ")
                .ok_or_else(|| format!("expected a `module` line, found `{content}`"))?;
            if let Some(previous) = module.replace(name.to_string()) {
                interface
                    .modules
                    .insert(previous, std::mem::take(&mut decls));
            }
            continue;
        }
        if module.is_none() {
            return Err(format!("`{content}` appears before any `module` line"));
        }
        if let Some(text) = content.strip_prefix("///") {
            doc.push(text.strip_prefix(' ').unwrap_or(text).to_string());
            continue;
        }
        if content.starts_with("export ") {
            let decl = Decl {
                header: content.to_string(),
                doc: std::mem::take(&mut doc),
                requires: Vec::new(),
                members: Vec::new(),
                conforms: Vec::new(),
                methods: Vec::new(),
            };
            // Indent says whether this is a declaration of the module or a
            // method of the declaration above it.
            if indent == 2 {
                decls.push(decl);
            } else {
                decls
                    .last_mut()
                    .ok_or_else(|| format!("`{content}` has no declaration to belong to"))?
                    .methods
                    .push(decl);
            }
            continue;
        }

        let owner = last_decl(&mut decls, indent)
            .ok_or_else(|| format!("`{content}` has no declaration to belong to"))?;
        if let Some(caps) = content.strip_prefix("requires ") {
            owner.requires = caps.split(", ").map(str::to_string).collect();
        } else if let Some(name) = content.strip_prefix("conforms ") {
            owner.conforms.push(name.to_string());
        } else {
            owner.members.push(content.to_string());
        }
    }

    if let Some(name) = module {
        interface.modules.insert(name, decls);
    }
    Ok(interface)
}

/// The declaration a line at `indent` belongs to: the last declaration of
/// the module at indent 4, and its last method deeper than that.
fn last_decl(decls: &mut [Decl], indent: usize) -> Option<&mut Decl> {
    let decl = decls.last_mut()?;
    if indent <= 4 {
        return Some(decl);
    }
    decl.methods.last_mut()
}

// ------------------------------------------------------------------ diffing

/// Whether a change can break a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    /// A caller written against the recorded interface may stop working.
    Breaking,
    /// The interface grew or its prose changed; every caller still works.
    Compatible,
}

/// One difference between a recorded interface and the one the source
/// derives now.
#[derive(Debug, PartialEq)]
pub(crate) struct Change {
    /// Whether a caller can survive this change.
    pub(crate) severity: Severity,
    /// The declaration this is about, as `module.name` or
    /// `module.Type.method`.
    pub(crate) path: String,
    /// What changed, in one line, plus any continuation lines.
    pub(crate) detail: Vec<String>,
}

impl Change {
    fn new(severity: Severity, path: &str, detail: impl Into<String>) -> Change {
        Change {
            severity,
            path: path.to_string(),
            detail: vec![detail.into()],
        }
    }
}

/// Everything the diff compares about one declaration.
struct Facts<'a> {
    header: &'a str,
    doc: &'a [String],
    requires: BTreeSet<&'a str>,
    members: &'a [String],
    conforms: BTreeSet<&'a str>,
}

/// Every declaration of `interface`, keyed by the path a reader names it by.
fn flatten(interface: &Interface) -> BTreeMap<String, Facts<'_>> {
    let mut flat = BTreeMap::new();
    for (module, decls) in &interface.modules {
        for decl in decls {
            let path = format!("{module}.{}", decl.name());
            for method in &decl.methods {
                flat.insert(format!("{path}.{}", method.name()), facts(method));
            }
            flat.insert(path, facts(decl));
        }
    }
    flat
}

fn facts(decl: &Decl) -> Facts<'_> {
    Facts {
        header: &decl.header,
        doc: &decl.doc,
        requires: decl.requires.iter().map(String::as_str).collect(),
        members: &decl.members,
        conforms: decl.conforms.iter().map(String::as_str).collect(),
    }
}

/// Classifies every difference between the recorded interface and the
/// current one.
///
/// Breaking is what a caller cannot survive: an export that is gone (a
/// rename is a removal and an addition), a changed signature, a struct
/// field, enum case, or trait requirement that was removed, changed,
/// reordered, or added — a new field is required by the synthesized
/// initializer, a new enum case breaks an exhaustive `match`, and a new
/// trait requirement without a default breaks every conformance — a removed
/// conformance, and a newly required capability, since a caller's host may
/// not grant it.
///
/// A new field is breaking unconditionally, with no exception to check:
/// `cove_syntax::ast::Field` has no default, so a field a caller may leave
/// out of the synthesized initializer is not expressible today. Were field
/// defaults added, adding a defaulted field would become compatible and this
/// classification would need the same `(default)` test the trait
/// requirements already get.
///
/// Compatible is the rest: a new export, a new conformance, a new trait
/// requirement that has a default body, a capability no longer required, and
/// a doc change.
pub(crate) fn diff(recorded: &Interface, current: &Interface) -> Vec<Change> {
    let old = flatten(recorded);
    let new = flatten(current);
    let mut changes = Vec::new();

    for (path, was) in &old {
        let Some(now) = new.get(path) else {
            changes.push(Change::new(
                Severity::Breaking,
                path,
                format!("removed export `{}`", was.header),
            ));
            continue;
        };
        compare(path, was, now, &mut changes);
    }
    for (path, now) in &new {
        if !old.contains_key(path) {
            changes.push(Change::new(
                Severity::Compatible,
                path,
                format!("new export `{}`", now.header),
            ));
        }
    }

    changes.sort_by(|a, b| (a.severity, &a.path, &a.detail).cmp(&(b.severity, &b.path, &b.detail)));
    changes
}

/// Compares one declaration that both interfaces have.
fn compare(path: &str, was: &Facts, now: &Facts, changes: &mut Vec<Change>) {
    if was.header != now.header {
        changes.push(Change {
            severity: Severity::Breaking,
            path: path.to_string(),
            detail: vec![
                "changed signature".to_string(),
                format!("was `{}`", was.header),
                format!("now `{}`", now.header),
            ],
        });
    }

    let old_members: BTreeSet<&str> = was.members.iter().map(String::as_str).collect();
    let new_members: BTreeSet<&str> = now.members.iter().map(String::as_str).collect();
    for member in old_members.difference(&new_members) {
        changes.push(Change::new(
            Severity::Breaking,
            path,
            format!("removed `{member}`"),
        ));
    }
    for member in new_members.difference(&old_members) {
        // A trait requirement with a default body is the one addition a
        // conformance does not have to answer.
        let severity = if member.ends_with(" (default)") {
            Severity::Compatible
        } else {
            Severity::Breaking
        };
        changes.push(Change::new(severity, path, format!("added `{member}`")));
    }
    if old_members == new_members && was.members != now.members {
        changes.push(Change::new(
            Severity::Breaking,
            path,
            "reordered fields or cases, which moves their positions",
        ));
    }

    for name in was.conforms.difference(&now.conforms) {
        changes.push(Change::new(
            Severity::Breaking,
            path,
            format!("no longer conforms to `{name}`"),
        ));
    }
    for name in now.conforms.difference(&was.conforms) {
        changes.push(Change::new(
            Severity::Compatible,
            path,
            format!("now conforms to `{name}`"),
        ));
    }

    for capability in now.requires.difference(&was.requires) {
        changes.push(Change::new(
            Severity::Breaking,
            path,
            format!("now requires `{capability}`, which a caller's host may not grant"),
        ));
    }
    for capability in was.requires.difference(&now.requires) {
        changes.push(Change::new(
            Severity::Compatible,
            path,
            format!("no longer requires `{capability}`"),
        ));
    }

    if was.doc != now.doc {
        changes.push(Change::new(Severity::Compatible, path, "doc changed"));
    }
}

/// Renders the hash comparison, the classified changes, and the summary.
pub(crate) fn render_diff(recorded: &Interface, current: &Interface, changes: &[Change]) -> String {
    let mut out = String::new();
    let (was, now) = (recorded.hash(), current.hash());
    if was == now {
        out.push_str(&format!("interface hash {was}, unchanged\n"));
    } else {
        out.push_str(&format!("interface hash {was} -> {now}\n"));
    }

    for severity in [Severity::Breaking, Severity::Compatible] {
        let group: Vec<&Change> = changes.iter().filter(|c| c.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(match severity {
            Severity::Breaking => "\nbreaking:\n",
            Severity::Compatible => "\ncompatible:\n",
        });
        for change in group {
            out.push_str(&format!("  {}: {}\n", change.path, change.detail[0]));
            for line in &change.detail[1..] {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }

    out.push('\n');
    out.push_str(&diff_summary(changes));
    out.push('\n');
    out
}

/// The one-line summary `cove api diff` ends with.
pub(crate) fn diff_summary(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "no interface change".to_string();
    }
    let breaking = changes
        .iter()
        .filter(|c| c.severity == Severity::Breaking)
        .count();
    format!(
        "{breaking} breaking change(s), {} compatible change(s)",
        changes.len() - breaking
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{examples_root, load_fixture, write, TempDir};

    /// The snapshot text a fixture package derives.
    fn snapshot_of(root: &Path) -> String {
        let (_, _, program) = load_fixture(root);
        derive(&program).to_snapshot()
    }

    fn interface_of(root: &Path) -> Interface {
        let (_, _, program) = load_fixture(root);
        derive(&program)
    }

    /// A package with one module, written from `source`.
    fn one_module(dir: &TempDir, source: &str) -> PathBuf {
        write(dir.path(), "cove.toml", "");
        write(dir.path(), "app/main.cove", source);
        dir.path().to_path_buf()
    }

    const GREETER: &str = "\
use console.println

/// Returns a greeting.
export fn greeting(name: String) -> String {
  \"Hello, {name}!\"
}

/// Prints a greeting.
export fn main() -> Result<Unit, Error> {
  console.println(greeting(\"world\"))?
  Ok(())
}
";

    #[test]
    fn a_snapshot_is_the_same_bytes_every_time_it_is_derived() {
        let dir = TempDir::new("api-deterministic");
        let root = one_module(&dir, GREETER);

        let first = snapshot_of(&root);
        let second = snapshot_of(&root);
        assert_eq!(first, second);

        // A second package with the same source derives the same bytes,
        // including the hash: nothing machine-specific reaches the file.
        let other = TempDir::new("api-deterministic-2");
        let other_root = one_module(&other, GREETER);
        assert_eq!(first, snapshot_of(&other_root));
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_through_its_own_text() {
        let dir = TempDir::new("api-roundtrip");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "app/main.cove",
            "\
use console.println

/// A value that can summarise itself.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String

  /// Returns a report line.
  fn line(self) -> String {
    \"- {self.summarize()}\"
  }
}

/// A booking.
export struct Booking {
  id: Int
  guests: Int
}

impl Summary for Booking {
  fn summarize(self) -> String {
    \"booking {self.id}\"
  }
}

/// Levels.
export enum Level {
  Debug
  Error(String)
}

/// A callback.
export type Callback = fn(value: Int) -> Unit

/// Prints one.
export fn main() -> Result<Unit, Error> {
  console.println(Booking(id: 1, guests: 2).line())?
  Ok(())
}
",
        );
        let derived = interface_of(dir.path());
        let text = derived.to_snapshot();
        let parsed = parse(&text).expect("a written snapshot parses");
        assert_eq!(parsed, derived, "round trip changed the interface:\n{text}");
        assert!(diff(&parsed, &derived).is_empty());
    }

    #[test]
    fn the_hash_ignores_a_doc_change_and_not_a_capability_change() {
        let dir = TempDir::new("api-hash");
        let root = one_module(&dir, GREETER);
        let original = interface_of(&root).hash();

        write(
            dir.path(),
            "app/main.cove",
            &GREETER.replace(
                "/// Returns a greeting.",
                "/// Returns a friendly greeting.",
            ),
        );
        assert_eq!(
            interface_of(&root).hash(),
            original,
            "a doc change must leave the interface hash alone"
        );

        write(
            dir.path(),
            "app/main.cove",
            &GREETER
                .replace("use console.println", "use console.println\nuse env.get")
                .replace("\"Hello, {name}!\"", "\"Hello, {env.get(name)}!\""),
        );
        assert_ne!(
            interface_of(&root).hash(),
            original,
            "a newly required capability must change the interface hash"
        );
    }

    #[test]
    fn the_hash_ignores_a_private_declaration_and_a_reordering() {
        let dir = TempDir::new("api-hash-private");
        let root = one_module(&dir, GREETER);
        let original = interface_of(&root).hash();

        write(
            dir.path(),
            "app/main.cove",
            &format!("{GREETER}\n/// Private helper.\nfn helper() -> Int {{\n  1\n}}\n"),
        );
        assert_eq!(interface_of(&root).hash(), original);

        let reordered = "\
use console.println

/// Prints a greeting.
export fn main() -> Result<Unit, Error> {
  console.println(greeting(\"world\"))?
  Ok(())
}

/// Returns a greeting.
export fn greeting(name: String) -> String {
  \"Hello, {name}!\"
}
";
        write(dir.path(), "app/main.cove", reordered);
        assert_eq!(interface_of(&root).hash(), original);
    }

    /// Derives the interface of `source`, against the interface of
    /// [`GREETER`].
    fn changes_against_greeter(name: &str, source: &str) -> Vec<Change> {
        let before = TempDir::new(&format!("api-before-{name}"));
        let recorded = interface_of(&one_module(&before, GREETER));
        let after = TempDir::new(&format!("api-after-{name}"));
        let current = interface_of(&one_module(&after, source));
        diff(&recorded, &current)
    }

    fn only(changes: &[Change]) -> &Change {
        assert_eq!(changes.len(), 1, "expected one change, got {changes:?}");
        &changes[0]
    }

    #[test]
    fn removing_an_export_is_breaking() {
        let changes = changes_against_greeter(
            "removed",
            "\
use console.println

/// Prints a greeting.
export fn main() -> Result<Unit, Error> {
  console.println(\"hi\")?
  Ok(())
}
",
        );
        let change = only(&changes);
        assert_eq!(change.severity, Severity::Breaking);
        assert_eq!(change.path, "app.greeting");
        assert_eq!(
            change.detail[0],
            "removed export `export fn greeting(name: String) -> String`"
        );
    }

    #[test]
    fn renaming_an_export_is_a_removal_and_an_addition() {
        let changes = changes_against_greeter("renamed", &GREETER.replace("greeting(", "greet("));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].severity, Severity::Breaking);
        assert_eq!(changes[0].path, "app.greeting");
        assert_eq!(changes[1].severity, Severity::Compatible);
        assert_eq!(changes[1].path, "app.greet");
    }

    #[test]
    fn changing_a_signature_is_breaking() {
        let changes = changes_against_greeter(
            "signature",
            &GREETER.replace(
                "greeting(name: String)",
                "greeting(name: String, loud: Bool)",
            ),
        );
        let change = only(&changes);
        assert_eq!(change.severity, Severity::Breaking);
        assert_eq!(change.path, "app.greeting");
        assert_eq!(
            change.detail,
            vec![
                "changed signature".to_string(),
                "was `export fn greeting(name: String) -> String`".to_string(),
                "now `export fn greeting(name: String, loud: Bool) -> String`".to_string(),
            ]
        );
    }

    #[test]
    fn requiring_a_new_capability_is_breaking() {
        let changes = changes_against_greeter(
            "capability",
            &GREETER
                .replace("use console.println", "use console.println\nuse env.get")
                .replace("\"Hello, {name}!\"", "\"Hello, {env.get(name)}!\""),
        );
        // `greeting` gains `env` directly, and `main` gains it through the
        // call, so both are breaking.
        assert_eq!(changes.len(), 2, "{changes:?}");
        for change in &changes {
            assert_eq!(change.severity, Severity::Breaking);
            assert_eq!(
                change.detail[0],
                "now requires `env`, which a caller's host may not grant"
            );
        }
        assert_eq!(changes[0].path, "app.greeting");
        assert_eq!(changes[1].path, "app.main");
    }

    #[test]
    fn removing_a_trait_conformance_is_breaking_and_adding_one_is_compatible() {
        let with = "\
/// Summarises.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String
}

/// A booking.
export struct Booking {
  id: Int
}

impl Summary for Booking {
  fn summarize(self) -> String {
    \"{self.id}\"
  }
}
";
        let without = "\
/// Summarises.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String
}

/// A booking.
export struct Booking {
  id: Int
}
";
        let a = TempDir::new("api-conformance-with");
        let with_interface = interface_of(&one_module(&a, with));
        let b = TempDir::new("api-conformance-without");
        let without_interface = interface_of(&one_module(&b, without));

        let removed = diff(&with_interface, &without_interface);
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert_eq!(removed[0].severity, Severity::Breaking);
        assert_eq!(removed[0].path, "app.Booking");
        assert_eq!(removed[0].detail[0], "no longer conforms to `app.Summary`");
        assert_eq!(removed[1].severity, Severity::Breaking);
        assert_eq!(removed[1].path, "app.Booking.summarize");
        assert_eq!(
            removed[1].detail[0],
            "removed export `export fn summarize(self) -> String`"
        );

        let added = diff(&without_interface, &with_interface);
        assert!(
            added
                .iter()
                .all(|change| change.severity == Severity::Compatible),
            "{added:?}"
        );
        assert_eq!(added[0].detail[0], "now conforms to `app.Summary`");
    }

    #[test]
    fn adding_an_export_is_compatible() {
        let changes = changes_against_greeter(
            "added",
            &format!(
                "{GREETER}\n/// Says goodbye.\nexport fn farewell() -> String {{\n  \"bye\"\n}}\n"
            ),
        );
        let change = only(&changes);
        assert_eq!(change.severity, Severity::Compatible);
        assert_eq!(change.path, "app.farewell");
        assert_eq!(
            change.detail[0],
            "new export `export fn farewell() -> String`"
        );
    }

    #[test]
    fn changing_a_doc_is_compatible() {
        let changes = changes_against_greeter(
            "doc",
            &GREETER.replace(
                "/// Returns a greeting.",
                "/// Returns a friendly greeting.",
            ),
        );
        let change = only(&changes);
        assert_eq!(change.severity, Severity::Compatible);
        assert_eq!(change.path, "app.greeting");
        assert_eq!(change.detail[0], "doc changed");
    }

    #[test]
    fn adding_a_struct_field_or_an_enum_case_is_breaking() {
        let before = "\
/// A booking.
export struct Booking {
  id: Int
}

/// Levels.
export enum Level {
  Debug
}
";
        let after = "\
/// A booking.
export struct Booking {
  id: Int
  guests: Int
}

/// Levels.
export enum Level {
  Debug
  Info
}
";
        let a = TempDir::new("api-members-before");
        let recorded = interface_of(&one_module(&a, before));
        let b = TempDir::new("api-members-after");
        let current = interface_of(&one_module(&b, after));

        let changes = diff(&recorded, &current);
        assert_eq!(changes.len(), 2, "{changes:?}");
        for change in &changes {
            assert_eq!(change.severity, Severity::Breaking);
        }
        assert_eq!(changes[0].path, "app.Booking");
        assert_eq!(changes[0].detail[0], "added `field guests: Int`");
        assert_eq!(changes[1].path, "app.Level");
        assert_eq!(changes[1].detail[0], "added `case Info`");
    }

    #[test]
    fn a_new_trait_requirement_is_breaking_unless_it_has_a_default() {
        let before = "\
/// Summarises.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String
}
";
        let required = "\
/// Summarises.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String

  /// Returns a title.
  fn title(self) -> String
}
";
        let defaulted = "\
/// Summarises.
export trait Summary {
  /// Returns the summary.
  fn summarize(self) -> String

  /// Returns a title.
  fn title(self) -> String {
    \"untitled\"
  }
}
";
        let a = TempDir::new("api-trait-before");
        let recorded = interface_of(&one_module(&a, before));
        let b = TempDir::new("api-trait-required");
        let c = TempDir::new("api-trait-defaulted");

        let breaking = diff(&recorded, &interface_of(&one_module(&b, required)));
        assert_eq!(only(&breaking).severity, Severity::Breaking);
        assert_eq!(
            only(&breaking).detail[0],
            "added `fn title(self) -> String`"
        );

        let compatible = diff(&recorded, &interface_of(&one_module(&c, defaulted)));
        assert_eq!(only(&compatible).severity, Severity::Compatible);
        assert_eq!(
            only(&compatible).detail[0],
            "added `fn title(self) -> String (default)`"
        );
    }

    /// The same fact `cove outline` renders: a conformance written in the
    /// trait's module belongs to the type's interface. Both commands read it
    /// from `Program::conformances_of` and `Program::methods_of`, so they
    /// cannot answer this differently.
    #[test]
    fn a_conformance_declared_in_the_trait_s_module_is_recorded_under_the_type() {
        let dir = TempDir::new("api-cross-module-conformance");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "shapes/widget.cove",
            "/// A widget.\nexport struct Widget {\n  id: Int\n}\n",
        );
        write(
            dir.path(),
            "report/summary.cove",
            "\
use shapes.Widget

/// A value that can summarise itself.
export trait Summary {
  /// Returns the one-line summary.
  fn summarize(self) -> String
}

impl Summary for Widget {
  fn summarize(self) -> String {
    \"widget {self.id}\"
  }
}
",
        );
        let text = snapshot_of(dir.path());
        assert!(
            text.contains(
                "\
module shapes
  /// A widget.
  export struct Widget
    field id: Int
    conforms report.Summary
    /// Returns the one-line summary.
    export fn summarize(self) -> String
"
            ),
            "the snapshot must record the conformance and its method:\n{text}"
        );

        // Dropping the conformance is breaking, which is the whole reason
        // the snapshot has to see it in the first place.
        write(
            dir.path(),
            "report/summary.cove",
            "\
/// A value that can summarise itself.
export trait Summary {
  /// Returns the one-line summary.
  fn summarize(self) -> String
}
",
        );
        let recorded = parse(&text).expect("the snapshot parses");
        let changes = diff(&recorded, &interface_of(dir.path()));
        assert!(
            changes
                .iter()
                .any(|change| change.severity == Severity::Breaking
                    && change.path == "shapes.Widget"
                    && change.detail[0] == "no longer conforms to `report.Summary`"),
            "{changes:?}"
        );
    }

    #[test]
    fn diff_without_a_snapshot_says_how_to_make_one() {
        let dir = TempDir::new("api-no-snapshot");
        one_module(&dir, GREETER);
        let Err(CliError::Message(message)) = cmd_diff(&[dir.path().display().to_string()]) else {
            panic!("a missing snapshot must be an error");
        };
        assert!(
            message.contains("no API snapshot at")
                && message.contains(DEFAULT_SNAPSHOT)
                && message.contains("cove api snapshot"),
            "unhelpful message: {message}"
        );
    }

    #[test]
    fn snapshot_writes_the_file_and_diff_then_reports_no_change() {
        let dir = TempDir::new("api-roundtrip-cli");
        one_module(&dir, GREETER);
        let path = dir.path().display().to_string();

        assert!(cmd_snapshot(std::slice::from_ref(&path)).is_ok());
        let written = dir.path().join(DEFAULT_SNAPSHOT);
        assert!(written.is_file(), "the snapshot was not written");

        assert!(cmd_diff(std::slice::from_ref(&path)).is_ok());

        // A breaking change fails, so CI can use the exit status.
        write(
            dir.path(),
            "app/main.cove",
            &GREETER.replace("export fn greeting", "fn greeting"),
        );
        assert!(matches!(cmd_diff(&[path]), Err(CliError::BreakingChange)));
    }

    #[test]
    fn out_and_against_name_another_file() {
        let dir = TempDir::new("api-out");
        one_module(&dir, GREETER);
        let path = dir.path().display().to_string();
        let out = dir.path().join("recorded.txt");

        assert!(cmd_snapshot(&[path.clone(), "--out".into(), out.display().to_string()]).is_ok());
        assert!(out.is_file());
        assert!(cmd_diff(&[path, "--against".into(), out.display().to_string()]).is_ok());
    }

    #[test]
    fn diff_summary_counts_each_classification() {
        assert_eq!(diff_summary(&[]), "no interface change");
        let changes = vec![
            Change::new(Severity::Breaking, "app.a", "removed export `x`"),
            Change::new(Severity::Compatible, "app.b", "doc changed"),
            Change::new(Severity::Compatible, "app.c", "doc changed"),
        ];
        assert_eq!(
            diff_summary(&changes),
            "1 breaking change(s), 2 compatible change(s)"
        );
    }

    #[test]
    fn header_name_reads_every_declaration_form() {
        assert_eq!(
            header_name("export fn greet(name: String) -> String"),
            "greet"
        );
        assert_eq!(header_name("export async fn load() -> Unit"), "load");
        assert_eq!(
            header_name("export fn identity<T>(value: T) -> T"),
            "identity"
        );
        assert_eq!(header_name("export struct Widget"), "Widget");
        assert_eq!(header_name("export struct Pair<A, B>"), "Pair");
        assert_eq!(header_name("export enum Level"), "Level");
        assert_eq!(header_name("export trait Summary"), "Summary");
        assert_eq!(
            header_name("export type Callback = async fn(value: Int) -> Unit"),
            "Callback"
        );
    }

    /// The checked-in snapshot of the real `examples/` package.
    ///
    /// This applies the feature to itself: an accidental change to the
    /// examples' public interface shows up as a failing test and a diff a
    /// reviewer reads, which is exactly what the command exists to do for a
    /// package that uses it.
    #[test]
    fn the_examples_snapshot_is_reproduced_byte_for_byte() {
        let recorded_path = examples_root().join(DEFAULT_SNAPSHOT);
        let recorded =
            std::fs::read_to_string(&recorded_path).expect("examples/cove-api.txt is checked in");
        let derived = snapshot_of(&examples_root());
        assert_eq!(
            derived,
            recorded,
            "the examples' derived interface no longer matches `{}`; \
             review the change and re-record it with `cove api snapshot examples`",
            recorded_path.display()
        );
    }

    #[test]
    fn the_examples_snapshot_diffs_clean() {
        let (_, _, program) = load_fixture(&examples_root());
        let text = std::fs::read_to_string(examples_root().join(DEFAULT_SNAPSHOT)).unwrap();
        let recorded = parse(&text).expect("the checked-in snapshot parses");
        let current = derive(&program);
        assert_eq!(diff(&recorded, &current), Vec::new());
    }
}
