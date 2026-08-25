//! `cove impact`: what a change to one declaration can affect.
//!
//! Resolution derives the package's call graph and takes the capability
//! fixed point over it. That same graph answers the other question a
//! reviewer has before a change lands: who calls this, which modules do they
//! live in, which `[run.<name>]` entries reach them, and does this
//! declaration need authority the entry does not grant.
//!
//! # Approximation
//!
//! The graph is sound, not exact. A call on a receiver whose type is not
//! written at the call site cannot be narrowed without a static type
//! checker, so resolution records an edge to *every* same-named method
//! reachable through imports. That never misses a real caller and can invent
//! one that does not exist. An edge derived that way is marked
//! [`CallPrecision::Approximate`], and every caller this report can only
//! reach through one is labelled rather than presented as a fact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cove_diag::SourceMap;
use cove_sema::package::Package;
use cove_sema::resolve::{CallPrecision, FnEntry, FnKey, Node, Program};

use crate::{fn_signature, load, location_line, CliError};

/// Runs `cove impact [path] <name>`.
pub(crate) fn cmd_impact(args: &[String]) -> Result<(), CliError> {
    let mut path: Option<&Path> = None;
    let mut query: Option<&str> = None;
    for arg in args {
        if let Some(flag) = arg.strip_prefix("--") {
            return Err(CliError::Message(format!(
                "unknown flag `--{flag}`; `cove impact` takes `[path] <name>`"
            )));
        }
        // The name is the last positional argument, so `cove impact greet`
        // and `cove impact examples greet` both read naturally.
        if let Some(previous) = query.replace(arg.as_str()) {
            path = Some(Path::new(previous));
        }
    }
    let Some(query) = query else {
        return Err(CliError::Message(
            "`cove impact` needs the name of a declaration, such as `greet` or `hello.greet`"
                .into(),
        ));
    };

    let (sources, package, program) = load(path)?;
    let target = resolve_target(&program, query)?;
    print!("{}", render_impact(&sources, &package, &program, &target));
    Ok(())
}

// ------------------------------------------------------------------ the target

/// Finds the one declaration `query` names.
///
/// A node answers to its bare name, to its name qualified by the module that
/// declares it, and — for a method — to its name qualified by its type. The
/// candidates are matched by name rather than by splitting `query` on `.`,
/// because a module name is dotted too: `booking.create.validate` could name
/// either a function of module `booking.create` or the method `create` of a
/// type `booking` and neither reading is more correct than the other.
fn resolve_target(program: &Program, query: &str) -> Result<Node, CliError> {
    let mut matched: Vec<Node> = Vec::new();
    for (module, resolved) in &program.modules {
        for name in resolved.functions.keys() {
            let node = (module.clone(), FnKey::Fn(name.clone()));
            if query == name || query == format!("{module}.{name}") {
                matched.push(node);
            }
        }
        for (type_name, name) in resolved.methods.keys() {
            let node = (
                module.clone(),
                FnKey::Method(type_name.clone(), name.clone()),
            );
            if query == name
                || query == format!("{type_name}.{name}")
                || query == format!("{module}.{type_name}.{name}")
            {
                matched.push(node);
            }
        }
    }

    match matched.len() {
        1 => Ok(matched.pop().expect("one match")),
        0 => Err(CliError::Message(format!(
            "this package declares no function or method `{query}`\n  \
             `cove impact` reports what calls a declaration, so it takes a function or a \
             method, written as `name`, `module.name`, or `module.Type.method`"
        ))),
        _ => {
            let names: Vec<String> = matched.iter().map(qualified).collect();
            Err(CliError::Message(format!(
                "`{query}` names {} declarations: {}\n  qualify it with the module that \
                 declares the one you mean",
                matched.len(),
                names.join(", ")
            )))
        }
    }
}

/// A node's full name: `hello.main`, or `traits.Booking.summarize`.
fn qualified(node: &Node) -> String {
    let (module, key) = node;
    match key {
        FnKey::Fn(name) => format!("{module}.{name}"),
        FnKey::Method(type_name, name) => format!("{module}.{type_name}.{name}"),
    }
}

/// The resolved entry a node stands for.
fn entry_of<'a>(program: &'a Program, node: &Node) -> Option<&'a FnEntry> {
    let resolved = program.modules.get(&node.0)?;
    match &node.1 {
        FnKey::Fn(name) => resolved.functions.get(name),
        FnKey::Method(type_name, name) => resolved.methods.get(&(type_name.clone(), name.clone())),
    }
}

// ------------------------------------------------------------------ reachability

/// How a caller reaches the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reach {
    /// Every call on the path is one the compiler resolved from the call
    /// site.
    Exact,
    /// The only paths cross a call whose receiver type is unknown, so the
    /// edge may not exist at run time.
    Approximate,
}

/// Every declaration that can reach `target` through the call graph, and how
/// certainly.
///
/// A caller is `Exact` when some path to the target uses only exact edges,
/// even if another path to it does not: one call the compiler resolved is
/// enough to make the dependency real.
fn callers<'a>(program: &'a Program, target: &'a Node) -> BTreeMap<&'a Node, Reach> {
    let exact = walk_back(program, target, true);
    let mut reached: BTreeMap<&Node, Reach> = walk_back(program, target, false)
        .into_iter()
        .map(|node| {
            let reach = if exact.contains(node) {
                Reach::Exact
            } else {
                Reach::Approximate
            };
            (node, reach)
        })
        .collect();
    // A recursive declaration reaches itself; it is the subject of the
    // report, not one of the things it affects.
    reached.remove(target);
    reached
}

/// Every node from which `target` is reachable, following call edges
/// backwards, optionally over exact edges only.
fn walk_back<'a>(program: &'a Program, target: &'a Node, exact_only: bool) -> BTreeSet<&'a Node> {
    let mut callers_of: BTreeMap<&Node, Vec<&Node>> = BTreeMap::new();
    for (caller, callees) in &program.call_graph {
        for (callee, precision) in callees {
            if exact_only && *precision != CallPrecision::Exact {
                continue;
            }
            callers_of.entry(callee).or_default().push(caller);
        }
    }

    let mut reached: BTreeSet<&Node> = BTreeSet::new();
    let mut pending: Vec<&Node> = vec![target];
    while let Some(node) = pending.pop() {
        let Some(callers) = callers_of.get(node) else {
            continue;
        };
        for caller in callers {
            if reached.insert(caller) {
                pending.push(caller);
            }
        }
    }
    reached
}

// ------------------------------------------------------------------ rendering

/// Renders what a change to `target` can affect: its callers, their modules,
/// the entries that reach them, and what those entries must grant.
fn render_impact(
    sources: &SourceMap,
    package: &Package,
    program: &Program,
    target: &Node,
) -> String {
    let reached = callers(program, target);
    let entry = entry_of(program, target);
    let required: Vec<String> = entry
        .map(|entry| {
            entry
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut out = format!("impact of `{}`\n", qualified(target));
    if let Some(entry) = entry {
        out.push_str(&format!("  {}\n", fn_signature(entry)));
        out.push_str(&location_line(
            sources,
            &package.root,
            entry.decl.name.span,
            2,
        ));
        out.push_str(&format!("  requires {}\n", capability_list(&required)));
    }

    // Listed by the name the report prints, which is not the order the
    // graph keys them in: a reader scans names, not `(module, kind)` pairs.
    let mut listed: Vec<(String, Reach)> = reached
        .iter()
        .map(|(node, reach)| (qualified(node), *reach))
        .collect();
    listed.sort();

    out.push_str(&format!("\ncallers ({}):\n", listed.len()));
    if listed.is_empty() {
        out.push_str("  (none — nothing in this package calls it)\n");
    }
    for (name, reach) in &listed {
        out.push_str(&format!("  {name}{}\n", label(*reach)));
    }

    let modules: BTreeSet<&str> = reached.keys().map(|(module, _)| module.as_str()).collect();
    out.push_str(&format!("\nmodules ({}):\n", modules.len()));
    if modules.is_empty() {
        out.push_str("  (none)\n");
    }
    for module in &modules {
        out.push_str(&format!("  {module}\n"));
    }

    out.push_str(&render_entries(package, target, &reached, &required));

    if reached.values().any(|reach| *reach == Reach::Approximate) {
        out.push_str(APPROXIMATION_NOTE);
    }
    out
}

/// The `[run.<name>]` entries that reach the target, and whether each grants
/// what it requires.
fn render_entries(
    package: &Package,
    target: &Node,
    reached: &BTreeMap<&Node, Reach>,
    required: &[String],
) -> String {
    let mut affected: Vec<(&str, &str, Reach, &Vec<String>)> = Vec::new();
    for (name, run) in &package.config.runs {
        let Some((module, function)) = run.entry_parts() else {
            continue;
        };
        let node = (module.to_string(), FnKey::Fn(function.to_string()));
        let reach = if node == *target {
            Some(Reach::Exact)
        } else {
            reached.get(&node).copied()
        };

        if let Some(reach) = reach {
            affected.push((name.as_str(), run.entry.as_str(), reach, &run.allow));
        }
    }

    let mut out = format!("\nentries ({}):\n", affected.len());
    if affected.is_empty() {
        out.push_str("  (none — no `[run.<name>]` entry in cove.toml reaches it)\n");
    }
    for (name, entry, reach, allow) in affected {
        out.push_str(&format!("  [run.{name}] {entry}{}\n", label(reach)));
        out.push_str(&format!("    allow = {}\n", capability_list(allow)));
        if required.is_empty() {
            out.push_str("    it requires no capability, so this entry's grants do not change\n");
            continue;
        }
        for capability in required {
            if allow.iter().any(|granted| granted == capability) {
                out.push_str(&format!("    requires {capability} — granted\n"));
            } else {
                out.push_str(&format!(
                    "    requires {capability} — not granted; the runtime would reject the call\n"
                ));
            }
        }
    }
    out
}

/// `console, env`, or `nothing` when the list is empty.
fn capability_list(capabilities: &[String]) -> String {
    if capabilities.is_empty() {
        "nothing".to_string()
    } else {
        capabilities.join(", ")
    }
}

/// The marker an approximated reach carries in the report.
fn label(reach: Reach) -> &'static str {
    match reach {
        Reach::Exact => "",
        Reach::Approximate => "  (approximate)",
    }
}

/// What `(approximate)` means, printed once when the report contains one.
const APPROXIMATION_NOTE: &str = "\n\
(approximate) marks a caller reached only through a call whose receiver type
the compiler cannot narrow without a static type checker. Such a call is
resolved to every same-named method reachable through imports, so the edge is
a possibility rather than a fact: it never hides a real caller, and it can
name one that does not exist.
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{examples_root, load_fixture, write, TempDir};

    /// The impact report for `query` over the package at `root`.
    fn report(root: &Path, query: &str) -> String {
        let (sources, package, program) = load_fixture(root);
        let target = match resolve_target(&program, query) {
            Ok(target) => target,
            Err(CliError::Message(message)) => panic!("{message}"),
            Err(_) => unreachable!(),
        };
        render_impact(&sources, &package, &program, &target)
    }

    /// Two modules: `util` exports a helper, `app` calls it directly and
    /// through one more hop, and `[run.app]` enters at `app.main`.
    fn write_two_modules(root: &Path) {
        write(
            root,
            "cove.toml",
            "[run.app]\nentry = \"app.main\"\nallow = [\"console\"]\n",
        );
        write(
            root,
            "util/text.cove",
            "\
use console.println

/// Prints a line.
export fn emit(line: String) {
  console.println(line)
}

/// Formats a name.
export fn label(name: String) -> String {
  \"[{name}]\"
}
",
        );
        write(
            root,
            "app/main.cove",
            "\
use util.emit
use util.label

/// Wraps `emit` one hop away from `main`.
fn announce(name: String) {
  emit(label(name))
}

/// Runs the program.
export fn main() -> Result<Unit, Error> {
  announce(\"cove\")
  Ok(())
}
",
        );
    }

    #[test]
    fn a_direct_call_appears_as_a_caller() {
        let dir = TempDir::new("impact-direct");
        write_two_modules(dir.path());
        let out = report(dir.path(), "util.label");
        assert!(
            out.contains("\ncallers (2):\n  app.announce\n  app.main\n"),
            "{out}"
        );
    }

    #[test]
    fn a_transitive_call_appears_as_a_caller() {
        let dir = TempDir::new("impact-transitive");
        write_two_modules(dir.path());
        let out = report(dir.path(), "util.emit");
        // `main` calls `announce` calls `emit`: two hops, still reported.
        assert!(out.contains("  app.main\n"), "{out}");
    }

    #[test]
    fn callers_across_a_module_boundary_name_their_own_module() {
        let dir = TempDir::new("impact-modules");
        write_two_modules(dir.path());
        let out = report(dir.path(), "util.emit");
        assert!(out.contains("\nmodules (1):\n  app\n"), "{out}");
    }

    #[test]
    fn an_entry_that_reaches_the_target_is_reported_with_its_grants() {
        let dir = TempDir::new("impact-entry");
        write_two_modules(dir.path());
        let out = report(dir.path(), "util.emit");
        assert!(
            out.contains(
                "\nentries (1):\n  [run.app] app.main\n    allow = console\n    requires console — granted\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn an_entry_that_does_not_grant_what_the_target_requires_says_so() {
        let dir = TempDir::new("impact-ungranted");
        write_two_modules(dir.path());
        write(
            dir.path(),
            "cove.toml",
            "[run.app]\nentry = \"app.main\"\nallow = []\n",
        );
        let out = report(dir.path(), "util.emit");
        assert!(
            out.contains("    requires console — not granted; the runtime would reject the call\n"),
            "{out}"
        );
    }

    #[test]
    fn a_declaration_nothing_calls_says_so() {
        let dir = TempDir::new("impact-unreached");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "app/main.cove",
            "/// Unused.\nexport fn orphan() -> Int {\n  1\n}\n",
        );
        let out = report(dir.path(), "orphan");
        assert!(
            out.contains("\ncallers (0):\n  (none — nothing in this package calls it)\n"),
            "{out}"
        );
        assert!(
            out.contains("(none — no `[run.<name>]` entry in cove.toml reaches it)"),
            "{out}"
        );
    }

    #[test]
    fn an_edge_the_compiler_could_only_approximate_is_labelled() {
        let dir = TempDir::new("impact-approximate");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "app/main.cove",
            "\
/// A widget.
export struct Widget {
  id: Int
}

impl Widget {
  /// Names the widget.
  export fn describe(self) -> String {
    \"{self.id}\"
  }
}

/// Calls `describe` on a parameter, whose type no checker has narrowed yet.
export fn show(thing: Widget) -> String {
  thing.describe()
}
",
        );
        let out = report(dir.path(), "app.Widget.describe");
        assert!(
            out.contains("  app.show  (approximate)\n"),
            "an approximated caller must be labelled:\n{out}"
        );
        assert!(
            out.contains("(approximate) marks a caller reached only through a call whose receiver"),
            "the label must be explained:\n{out}"
        );
    }

    #[test]
    fn a_call_the_compiler_resolved_exactly_carries_no_label() {
        let dir = TempDir::new("impact-exact");
        write_two_modules(dir.path());
        let out = report(dir.path(), "util.emit");
        assert!(!out.contains("(approximate)"), "{out}");
    }

    #[test]
    fn an_unknown_name_explains_the_forms_it_accepts() {
        let dir = TempDir::new("impact-unknown");
        write_two_modules(dir.path());
        let (_, _, program) = load_fixture(dir.path());
        let Err(CliError::Message(message)) = resolve_target(&program, "nope") else {
            panic!("an unknown name must be an error");
        };
        assert!(
            message.contains("declares no function or method `nope`")
                && message.contains("module.Type.method"),
            "{message}"
        );
    }

    #[test]
    fn an_ambiguous_bare_name_lists_the_candidates() {
        let dir = TempDir::new("impact-ambiguous");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "one/main.cove",
            "/// One.\nexport fn shared() -> Int {\n  1\n}\n",
        );
        write(
            dir.path(),
            "two/main.cove",
            "/// Two.\nexport fn shared() -> Int {\n  2\n}\n",
        );
        let (_, _, program) = load_fixture(dir.path());
        let Err(CliError::Message(message)) = resolve_target(&program, "shared") else {
            panic!("an ambiguous name must be an error");
        };
        assert!(
            message.contains("names 2 declarations: one.shared, two.shared"),
            "{message}"
        );
        assert!(resolve_target(&program, "one.shared").is_ok());
    }

    #[test]
    fn the_real_examples_package_reports_an_entry_and_its_grant() {
        let out = report(&examples_root(), "hello.greeting");
        assert!(out.contains("\ncallers (1):\n  hello.main\n"), "{out}");
        assert!(out.contains("\nmodules (1):\n  hello\n"), "{out}");
        assert!(
            out.contains("\nentries (1):\n  [run.hello] hello.main\n"),
            "{out}"
        );
    }
}
