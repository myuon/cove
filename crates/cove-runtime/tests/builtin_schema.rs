//! Every builtin the shared table declares has a body behind it, and the
//! compiler agrees about what that body is called.
//!
//! `cove_schema::builtins` is the one description of the language's own
//! methods and associated functions. `cove-sema` checks a call against it and
//! `cove_runtime::builtins` implements the call, and the second of those is a
//! Rust `match` rather than a table, so nothing in the type system holds the
//! two together. This file does, by driving every entry in the shared table
//! through the whole toolchain: each one is a program that is resolved,
//! type-checked, and run.
//!
//! That closes both directions. An entry added to the schema with no
//! implementation behind it has no exercise, which
//! [`every_builtin_the_schema_declares_is_exercised`] fails on; write the
//! exercise and [`every_builtin_the_schema_declares_checks_and_runs`] fails
//! until the runtime dispatches it. An implementation added with no entry in
//! the schema is unreachable, because `cove check` refuses to call a name the
//! table does not declare -- which is what makes the shared table the list
//! rather than one of two lists.
//!
//! The programs touch no host, so nothing here reads a clock, a file, or a
//! socket.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::{Grants, HostRegistry, Runtime};
use cove_sema::{Config, Module, Package, Unit};

/// One entry of the shared table, with a program that calls it.
struct Exercise {
    /// The builtin type the entry belongs to.
    ty: &'static str,
    /// The method or associated function the program calls.
    name: &'static str,
    /// The body of `export fn main() -> Int`, indented and ending in an
    /// `Int`.
    body: &'static str,
}

/// A call to every method and associated function
/// `cove_schema::builtins::BUILTINS` declares.
///
/// `Error` is the one builtin type with no entries: a program writes the name
/// to build one and reads the message as a field, so there is nothing to
/// call and nothing here to call it with.
static EXERCISES: &[Exercise] = &[
    Exercise {
        ty: "Array",
        name: "get",
        body: "  let items = [1, 2]\n  items.get(0).unwrapOr(0)",
    },
    Exercise {
        ty: "Array",
        name: "length",
        body: "  let items = [1, 2]\n  items.length()",
    },
    Exercise {
        ty: "Array",
        name: "isEmpty",
        body: "  let items = [1, 2]\n  let empty = items.isEmpty()\n  0",
    },
    Exercise {
        ty: "Array",
        name: "snapshot",
        body: "  let items = [1, 2]\n  items.snapshot().length()",
    },
    Exercise {
        ty: "Vector",
        name: "get",
        body: "  var items = Vector.of(1, 2)\n  items.get(1).unwrapOr(0)",
    },
    Exercise {
        ty: "Vector",
        name: "length",
        body: "  var items = Vector.of(1, 2)\n  items.length()",
    },
    Exercise {
        ty: "Vector",
        name: "isEmpty",
        body: "  var items = Vector.of(1)\n  let empty = items.isEmpty()\n  0",
    },
    Exercise {
        ty: "Vector",
        name: "push",
        body: "  var items = Vector.of(1)\n  items.push(2)\n  items.length()",
    },
    // `freeze` needs locally unique storage, so the vector it consumes is
    // one nothing else observes.
    Exercise {
        ty: "Vector",
        name: "freeze",
        body: "  var items = Vector.of(1, 2)\n  items.freeze().length()",
    },
    Exercise {
        ty: "Vector",
        name: "toArray",
        body: "  var items = Vector.of(1, 2)\n  items.toArray().length()",
    },
    Exercise {
        ty: "Vector",
        name: "snapshot",
        body: "  var items = Vector.of(1, 2)\n  var copy = items.snapshot()\n  copy.length()",
    },
    Exercise {
        ty: "Vector",
        name: "of",
        body: "  var items = Vector.of(1, 2, 3)\n  items.length()",
    },
    Exercise {
        ty: "Map",
        name: "get",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.get(\"a\").unwrapOr(0)",
    },
    Exercise {
        ty: "Map",
        name: "length",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.length()",
    },
    Exercise {
        ty: "Map",
        name: "isEmpty",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  let empty = ages.isEmpty()\n  0",
    },
    Exercise {
        ty: "Map",
        name: "contains",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  let has = ages.contains(\"a\")\n  0",
    },
    Exercise {
        ty: "Map",
        name: "keys",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.keys().length()",
    },
    Exercise {
        ty: "Map",
        name: "values",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.values().length()",
    },
    Exercise {
        ty: "Map",
        name: "inserted",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.inserted(\"b\", 2).length()",
    },
    Exercise {
        ty: "Map",
        name: "removed",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.removed(\"a\").length()",
    },
    Exercise {
        ty: "Map",
        name: "snapshot",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages.snapshot().length()",
    },
    Exercise {
        ty: "Map",
        name: "of",
        body: "  let ages = Map.of(MapEntry(key: \"a\", value: 1), MapEntry(key: \"b\", value: 2))\n  ages.length()",
    },
    Exercise {
        ty: "Set",
        name: "length",
        body: "  let tags = Set.of(1, 2)\n  tags.length()",
    },
    Exercise {
        ty: "Set",
        name: "isEmpty",
        body: "  let tags = Set.of(1)\n  let empty = tags.isEmpty()\n  0",
    },
    Exercise {
        ty: "Set",
        name: "toArray",
        body: "  let tags = Set.of(1, 2)\n  tags.toArray().length()",
    },
    Exercise {
        ty: "Set",
        name: "contains",
        body: "  let tags = Set.of(1)\n  let has = tags.contains(1)\n  0",
    },
    Exercise {
        ty: "Set",
        name: "inserted",
        body: "  let tags = Set.of(1)\n  tags.inserted(2).length()",
    },
    Exercise {
        ty: "Set",
        name: "removed",
        body: "  let tags = Set.of(1, 2)\n  tags.removed(1).length()",
    },
    Exercise {
        ty: "Set",
        name: "snapshot",
        body: "  let tags = Set.of(1, 2)\n  tags.snapshot().length()",
    },
    Exercise {
        ty: "Set",
        name: "of",
        body: "  let tags = Set.of(1, 2, 3)\n  tags.length()",
    },
    Exercise {
        ty: "String",
        name: "length",
        body: "  let text = \"hello\"\n  text.length()",
    },
    Exercise {
        ty: "String",
        name: "isEmpty",
        body: "  let text = \"hello\"\n  let empty = text.isEmpty()\n  0",
    },
    Exercise {
        ty: "String",
        name: "words",
        body: "  let text = \"one two\"\n  text.words().length()",
    },
    Exercise {
        ty: "String",
        name: "snapshot",
        body: "  let text = \"hello\"\n  text.snapshot().length()",
    },
    Exercise {
        ty: "Range",
        name: "length",
        body: "  let span = 0..<3\n  span.length()",
    },
    Exercise {
        ty: "Range",
        name: "isEmpty",
        body: "  let span = 0..<3\n  let empty = span.isEmpty()\n  0",
    },
    Exercise {
        ty: "Range",
        name: "contains",
        body: "  let span = 0..<3\n  let has = span.contains(1)\n  0",
    },
    Exercise {
        ty: "Range",
        name: "snapshot",
        body: "  let span = 0..<3\n  span.snapshot().length()",
    },
    Exercise {
        ty: "Option",
        name: "isSome",
        body: "  let found = Some(1)\n  let present = found.isSome()\n  0",
    },
    Exercise {
        ty: "Option",
        name: "isNone",
        body: "  let found = Some(1)\n  let absent = found.isNone()\n  0",
    },
    Exercise {
        ty: "Option",
        name: "unwrapOr",
        body: "  let found = Some(1)\n  found.unwrapOr(0)",
    },
    Exercise {
        ty: "Result",
        name: "isOk",
        body: "  let outcome = Int.parse(\"1\")\n  let fine = outcome.isOk()\n  0",
    },
    Exercise {
        ty: "Result",
        name: "isError",
        body: "  let outcome = Int.parse(\"x\")\n  let failed = outcome.isError()\n  0",
    },
    // The Language Card writes `mapError { ... }` with a trailing closure
    // that may ignore the error it replaces, which is the shape the schema
    // does not declare and both ends accept anyway.
    Exercise {
        ty: "Result",
        name: "mapError",
        body: "  let outcome = Int.parse(\"x\").mapError { Error(\"not a number\") }\n  let failed = outcome.isError()\n  0",
    },
    Exercise {
        ty: "Int",
        name: "snapshot",
        body: "  let count = 7\n  count.snapshot()",
    },
    Exercise {
        ty: "Int",
        name: "parse",
        body: "  let parsed = Int.parse(\"7\")\n  let fine = parsed.isOk()\n  0",
    },
    Exercise {
        ty: "Float",
        name: "snapshot",
        body: "  let ratio = 1.5\n  let copy = ratio.snapshot()\n  0",
    },
    Exercise {
        ty: "Bool",
        name: "snapshot",
        body: "  let ready = true\n  let copy = ready.snapshot()\n  0",
    },
    Exercise {
        ty: "Unit",
        name: "snapshot",
        body: "  let nothing = ()\n  let copy = nothing.snapshot()\n  0",
    },
    Exercise {
        ty: "Duration",
        name: "snapshot",
        body: "  let wait = 500ms\n  let copy = wait.snapshot()\n  0",
    },
    Exercise {
        ty: "Task",
        name: "await",
        body: "  scope tasks {\n    let job = tasks.spawn { 7 }\n    job.await()\n  }",
    },
    Exercise {
        ty: "Task",
        name: "cancel",
        body: "  scope tasks {\n    let job = tasks.spawn { 7 }\n    job.cancel()\n    0\n  }",
    },
    Exercise {
        ty: "Shared",
        name: "lock",
        body: "  let counts = Shared(7)\n  counts.lock(fn(value) { value })",
    },
    Exercise {
        ty: "Scope",
        name: "spawn",
        body: "  scope tasks {\n    let first = tasks.spawn { 3 }\n    let second = tasks.spawn { 4 }\n    first.await() + second.await()\n  }",
    },
];

/// One entry of the table of builtins that are called on nothing, with a
/// program that calls it.
struct FreeExercise {
    /// The constructor or assertion the program calls.
    name: &'static str,
    /// The body of `export fn main() -> Int`, as [`Exercise::body`].
    body: &'static str,
}

/// A call to every constructor and assertion
/// `cove_schema::builtins::FREE_BUILTINS` declares.
///
/// An assertion's result is bound rather than answered, because `main` here
/// answers an `Int`; what matters is that the call checks and runs, which is
/// the only thing the two ends can still disagree about.
static FREE_EXERCISES: &[FreeExercise] = &[
    FreeExercise {
        name: "Ok",
        body: "  let outcome = Ok(1)\n  0",
    },
    FreeExercise {
        name: "Err",
        body: "  let outcome = Err(Error(\"boom\"))\n  0",
    },
    FreeExercise {
        name: "Some",
        body: "  Some(3).unwrapOr(0)",
    },
    FreeExercise {
        name: "Error",
        body: "  let failure = Error(\"boom\")\n  0",
    },
    FreeExercise {
        name: "Shared",
        body: "  let counts = Shared(7)\n  counts.lock(fn(value) { value })",
    },
    FreeExercise {
        name: "assert",
        body: "  let checked = assert(1 == 1)\n  0",
    },
    FreeExercise {
        name: "assertEqual",
        body: "  let checked = assertEqual(1 + 1, 2)\n  0",
    },
];

/// The exercise for `type.name`, if this file has one.
fn exercise(ty: &str, name: &str) -> Option<&'static Exercise> {
    EXERCISES
        .iter()
        .find(|entry| entry.ty == ty && entry.name == name)
}

/// Every method and associated function the shared table declares, qualified.
fn declared() -> Vec<(&'static str, &'static str)> {
    cove_schema::builtins::builtins()
        .iter()
        .flat_map(|entry| {
            entry
                .methods
                .iter()
                .chain(entry.associated)
                .map(move |method| (entry.name, method.name))
        })
        .collect()
}

/// The one-module package an exercise's body makes.
fn package(body: &str) -> (Package, Arc<SourceMap>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let text = format!("export fn main() -> Int {{\n{body}\n}}\n");
    let file = sources.add(path.clone(), &text);
    let ast = cove_syntax::parse_file(&sources, file)
        .unwrap_or_else(|errors| panic!("the exercise parses:\n{text}\n{errors:?}"));
    let mut modules = BTreeMap::new();
    modules.insert(
        "app".to_string(),
        Module {
            name: "app".to_string(),
            dir: PathBuf::from("app"),
            units: vec![Unit { file, path, ast }],
        },
    );
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    (package, Arc::new(sources))
}

/// An entry the schema declares but nothing here calls is one nothing holds
/// the runtime to, so the table would be free to grow a signature with no
/// body behind it.
#[test]
fn every_builtin_the_schema_declares_is_exercised() {
    let missing: Vec<String> = declared()
        .into_iter()
        .filter(|(ty, name)| exercise(ty, name).is_none())
        .map(|(ty, name)| format!("`{ty}.{name}`"))
        .collect();
    assert!(
        missing.is_empty(),
        "the builtin schema declares {} that nothing in this file calls",
        missing.join(", ")
    );
}

/// The other direction: an exercise for something the schema does not
/// declare would be testing a name no program can write.
#[test]
fn every_exercise_names_something_the_schema_declares() {
    for entry in EXERCISES {
        let schema = cove_schema::builtin(entry.ty)
            .unwrap_or_else(|| panic!("`{}` is not a builtin type", entry.ty));
        assert!(
            schema.method(entry.name).is_some() || schema.associated_function(entry.name).is_some(),
            "`{}.{}` is not in the builtin schema",
            entry.ty,
            entry.name
        );
    }
}

/// The whole point: what the schema declares, the compiler accepts and the
/// runtime runs.
///
/// A signature added to the table with no `match` arm behind it fails here,
/// which is the only way the shared table and the dispatcher can still
/// disagree.
#[test]
fn every_builtin_the_schema_declares_checks_and_runs() {
    for entry in EXERCISES {
        check_and_run(&format!("{}.{}", entry.ty, entry.name), entry.body);
    }
}

/// The constructors and the assertions, held to the same thing: the arity the
/// table declares is the arity `cove check` reports on and the arity the
/// interpreter enforces.
#[test]
fn every_free_builtin_the_schema_declares_is_exercised() {
    let missing: Vec<String> = cove_schema::builtins::free_builtins()
        .iter()
        .filter(|entry| {
            !FREE_EXERCISES
                .iter()
                .any(|exercise| exercise.name == entry.name)
        })
        .map(|entry| format!("`{}`", entry.name))
        .collect();
    assert!(
        missing.is_empty(),
        "the builtin schema declares {} that nothing in this file calls",
        missing.join(", ")
    );
}

/// The other direction, as above: an exercise for a name the table does not
/// declare is a name no program can write.
#[test]
fn every_free_exercise_names_something_the_schema_declares() {
    for entry in FREE_EXERCISES {
        assert!(
            cove_schema::free_builtin(entry.name).is_some(),
            "`{}` is not in the builtin schema",
            entry.name
        );
    }
}

/// What the free table declares, the compiler accepts and the runtime runs.
#[test]
fn every_free_builtin_the_schema_declares_checks_and_runs() {
    for entry in FREE_EXERCISES {
        check_and_run(entry.name, entry.body);
    }
}

/// Resolves, checks, and runs one exercise, and says which one it was when
/// any of the three refuses it.
fn check_and_run(what: &str, body: &str) {
    let (package, sources) = package(body);
    let program = cove_sema::resolve::resolve(&package)
        .unwrap_or_else(|errors| panic!("`{what}` resolves: {errors:?}"));
    let diagnostics = cove_sema::typeck::check(&package, &program);
    assert!(
        diagnostics.is_empty(),
        "`{what}` is declared by the schema but the checker refused it: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>()
    );

    let hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
    let runtime = Runtime::new(Arc::new(program), sources, Arc::new(hosts));
    Interpreter::new(&runtime)
        .run_entry("app", "main", Vec::new())
        .unwrap_or_else(|error| {
            panic!(
                "`{what}` is declared by the schema but the runtime refused it: {}",
                error.message
            )
        });
}
