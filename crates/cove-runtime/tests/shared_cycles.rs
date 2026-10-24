//! A ring of `Shared` cells does not outlive the interpreter run that built
//! it: [issue #688](https://github.com/myuon/cove/issues/688) and ADR 0115.
//!
//! The per-task heap never looks inside a cell, so `a` holding `b` and `b`
//! holding `a` is an `Arc` cycle no collection on a task's thread can free.
//! LeakSanitizer found one at the exit of the `differential_source` fuzz
//! target, built by `tests/e2e/fail_key_known_shared` on the tree-walking
//! interpreter — on Linux only, since macOS has no LeakSanitizer. These cases
//! ask the run's cell table instead, which works on every host: after the
//! entry returns, a cell is alive only if something outside the cells still
//! reaches it.
//!
//! The shortest ring is one cell holding a handle to itself, which the
//! interpreter's `lock` refused until
//! [issue #695](https://github.com/myuon/cove/issues/695) (ADR 0118). It is
//! stored now, as on the linear-memory backend, and reclaimed the same way.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::{Grants, HostRegistry, Runtime};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

const SOURCE: &str = "\
/// A node whose `next` is a cell another node can be reached through.
export struct Node {
  next: Option<Shared<Node>>
  label: String
}

/// Two cells that hold each other, and a node outside them that holds the
/// first.
fn ring() -> Node {
  let first = Shared(Node(next: None, label: \"first\"))
  let second = Shared(Node(next: None, label: \"second\"))
  first.lock(fn(var value) {
    value = Node(next: Some(second), label: \"first\")
  })
  second.lock(fn(var value) {
    value = Node(next: Some(first), label: \"second\")
  })
  Node(next: Some(first), label: \"outer\")
}

/// The label two steps along from `node`.
export fn around(node: Node) -> String {
  match node.next {
    Some(cell) => cell.lock(fn(value) {
      match value.next {
        Some(next) => next.lock(fn(held) { held.label })
        None => \"nothing\"
      }
    })
    None => \"nothing\"
  }
}

/// Builds a ring, walks it, and drops it.
export fn dropped() -> String {
  around(ring())
}

/// Builds a ring and hands it to the embedder.
export fn kept() -> Node {
  ring()
}

/// A cell that holds a handle to itself, renamed through that handle and
/// read back through the cell.
fn alone() -> Shared<Node> {
  let cell = Shared(Node(next: None, label: \"alone\"))
  cell.lock(fn(var value) {
    value = Node(next: Some(cell), label: \"alone\")
  })
  let inner = cell.lock(fn(value) { value.next })
  match inner {
    Some(next) => next.lock(fn(var value) {
      value = Node(next: value.next, label: \"renamed\")
    })
    None => {}
  }
  cell
}

/// Builds a cell that holds itself, reads it, and drops it.
export fn itself() -> String {
  alone().lock(fn(value) { value.label })
}

/// Builds a cell that holds itself and hands it to the embedder inside a
/// node.
export fn itselfKept() -> Node {
  Node(next: Some(alone()), label: \"outer\")
}

/// Builds a ring on a spawned task's thread, so its cells are registered from
/// a thread other than the entry's.
export fn spawned() -> String {
  scope work {
    let task = work.spawn { around(ring()) }
    await task
  }
}
";

fn runtime() -> Runtime {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::from([(
        "m".to_string(),
        Module {
            name: "m".to_string(),
            dir: PathBuf::from("m"),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    let program = match Compiler::new().compile(&package) {
        Ok(program) => program,
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<String>()
        ),
    };
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    Runtime::new(Arc::new(program), Arc::new(sources), hosts)
}

#[test]
fn a_ring_the_run_dropped_is_freed_when_the_entry_returns() {
    let runtime = runtime();
    let answer = Interpreter::new(&runtime)
        .run_entry("m", "dropped", Vec::<Rc<str>>::new())
        .expect("the run succeeds");
    assert_eq!(answer.as_str(), Some("second"));
    assert_eq!(runtime.cells().live(), 0, "the ring outlived its run");
}

#[test]
fn a_ring_the_answer_holds_survives_until_the_answer_goes() {
    let runtime = runtime();
    let answer = Interpreter::new(&runtime)
        .run_entry("m", "kept", Vec::<Rc<str>>::new())
        .expect("the run succeeds");
    // The answer reaches the first cell, and the first the second: both are
    // the embedder's for as long as it keeps the answer.
    assert_eq!(
        runtime.cells().live(),
        2,
        "a cell the answer reaches was freed"
    );
    assert_eq!(runtime.cells().reclaim(), 0);
    assert_eq!(runtime.cells().live(), 2);

    // And still intact: a later run over the same runtime can walk it.
    let walked = Interpreter::new(&runtime)
        .invoke("m", "around", vec![answer.clone()])
        .expect("the ring is walkable");
    assert_eq!(walked.as_str(), Some("second"));

    drop(walked);
    drop(answer);
    assert_eq!(runtime.cells().live(), 2, "only a reclamation frees a ring");
    assert_eq!(runtime.cells().reclaim(), 2);
    assert_eq!(runtime.cells().live(), 0);
}

#[test]
fn a_ring_a_task_built_is_freed_when_the_entry_returns() {
    let runtime = runtime();
    let answer = cove_runtime::on_cove_stack(move || {
        let answer = Interpreter::new(&runtime)
            .run_entry("m", "spawned", Vec::<Rc<str>>::new())
            .expect("the run succeeds");
        (answer.as_str().map(str::to_string), runtime.cells().live())
    })
    .expect("the entry's thread starts");
    assert_eq!(answer, (Some("second".to_string()), 0));
}

#[test]
fn a_cell_holding_itself_is_stored_and_freed_when_the_entry_returns() {
    let runtime = runtime();
    let answer = Interpreter::new(&runtime)
        .run_entry("m", "itself", Vec::<Rc<str>>::new())
        .expect("the run succeeds");
    assert_eq!(answer.as_str(), Some("renamed"));
    assert_eq!(runtime.cells().live(), 0, "the cell outlived its run");
}

#[test]
fn a_cell_holding_itself_the_answer_keeps_survives_until_the_answer_goes() {
    let runtime = runtime();
    let answer = Interpreter::new(&runtime)
        .run_entry("m", "itselfKept", Vec::<Rc<str>>::new())
        .expect("the run succeeds");
    assert_eq!(
        runtime.cells().live(),
        1,
        "a cell the answer reaches was freed"
    );
    assert_eq!(runtime.cells().reclaim(), 0);
    drop(answer);
    assert_eq!(
        runtime.cells().live(),
        1,
        "only a reclamation frees a cycle"
    );
    assert_eq!(runtime.cells().reclaim(), 1);
    assert_eq!(runtime.cells().live(), 0);
}
