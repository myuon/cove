//! The conformance suite for `docs/LANGUAGE_REFERENCE.md`.
//!
//! The reference states, for every expression and pattern form, what the
//! form resolves to, what type it has, what it evaluates to, and which
//! errors it can produce. A sentence in a document cannot hold the checker
//! and the interpreter to the same answer; this file can, because it asks
//! both of them the same question about the same program and fails when
//! their answers differ.
//!
//! Three tables do that, one per kind of claim the reference makes:
//!
//! - [`RULES`] pins a form's *type* and its *value* together. A rule's body
//!   is compiled once at the type the reference gives it, which must be
//!   accepted, and once at a foreign type, which must be refused with
//!   `cove::type::mismatch` — that second compile is what makes the first
//!   one a claim about the type rather than about some type that happens to
//!   fit. The same body is then run, and what comes back must be the value
//!   the reference names.
//! - [`REJECTIONS`] pins the programs the reference does not admit, each
//!   against the diagnostic code that refuses it.
//! - [`TRAPS`] pins the failures the reference leaves to run time: a program
//!   the checker admits and the interpreter stops.
//!
//! A fourth test reads the coverage claim off the AST itself: the list of
//! forms is generated from `ExprKind` and `PatternKind` by the `forms!`
//! macro below, so a variant added to either stops this file compiling, and
//! the programs it is matched against are parsed and walked rather than
//! searched as text — and only the ones this suite *accepts* count.
//!
//! A future backend is held to the reference by the same three tables:
//! whatever produces a value for [`RULES`] and a failure for [`TRAPS`] is a
//! Cove implementation, and whatever does not, is not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::{Diagnostic, Severity, SourceMap};
use cove_runtime::interp::Interpreter;
use cove_runtime::{Grants, HostRegistry, Runtime, Value};
use cove_sema::resolve::{resolve, Program};
use cove_sema::{Config, Module, Package, Unit};
use cove_syntax::ast::{
    Block, Expr, ExprKind, Item, ItemKind, Param, Pattern, PatternKind, StmtKind, StrPart,
};

/// The module every fixture below is written in.
const MODULE: &str = "conformance";

/// The function every fixture below is entered through.
const ENTRY: &str = "probe";

/// What a rule's body means, in both of the two accounts a Cove
/// implementation gives of it.
struct Rule {
    /// The heading in `docs/LANGUAGE_REFERENCE.md` this pins.
    section: &'static str,
    /// Declarations the body needs, written above it.
    decls: &'static str,
    /// A function body: statements, then the tail expression under test.
    body: &'static str,
    /// The type the reference gives that tail, written as Cove source.
    ty: &'static str,
    /// What the interpreter produces for it, as Cove renders a value.
    value: &'static str,
    /// A type the reference does *not* give the body, at which the checker
    /// must refuse it. That refusal is what makes [`Rule::ty`] the type the
    /// body has rather than one it merely fits.
    foreign: &'static str,
}

/// A program the reference does not admit, and the diagnostic that says so.
struct Rejection {
    section: &'static str,
    /// A whole module, since some rejections are about declarations.
    source: &'static str,
    /// The `code` of the diagnostic that must refuse it.
    code: &'static str,
}

/// A program the reference admits and the interpreter stops, because the
/// rule it breaks is not one a type can state.
struct Trap {
    section: &'static str,
    decls: &'static str,
    body: &'static str,
    /// The declared result of the body, which never arrives.
    ty: &'static str,
    /// Text the runtime error must contain.
    message: &'static str,
}

// ------------------------------------------------------------------ values

/// Every expression and pattern form the reference gives a type and a value.
static RULES: &[Rule] = &[
    Rule {
        section: "Literals",
        decls: "",
        body: "7",
        ty: "Int",
        value: "7",
        foreign: "Bool",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "1.5",
        ty: "Float",
        value: "1.5",
        foreign: "Int",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "true",
        ty: "Bool",
        value: "true",
        foreign: "Int",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "500ms",
        ty: "Duration",
        value: "500ms",
        foreign: "Int",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "\"a{1 + 1}b\"",
        ty: "String",
        value: "a2b",
        foreign: "Int",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "()",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "Literals",
        decls: "",
        body: "[1, 2]",
        ty: "Array<Int>",
        value: "[1, 2]",
        foreign: "Array<String>",
    },
    Rule {
        section: "Names",
        decls: "",
        body: "let value = 7\nvalue",
        ty: "Int",
        value: "7",
        foreign: "Bool",
    },
    Rule {
        section: "Names",
        decls: "",
        body: "let outer = 1\nlet shadowed = {\n  let outer = 2\n  outer\n}\nouter + shadowed",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    Rule {
        section: "Field access",
        decls: "struct Point {\n  x: Int\n}\n\n",
        body: "let point = Point(x: 3)\npoint.x",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    Rule {
        section: "Calls",
        decls: "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n",
        body: "twice(n: 4)",
        ty: "Int",
        value: "8",
        foreign: "Bool",
    },
    Rule {
        section: "Calls",
        decls: "fn tagged(name: String = \"none\") -> String {\n  name\n}\n\n",
        body: "tagged()",
        ty: "String",
        value: "none",
        foreign: "Int",
    },
    Rule {
        section: "Calls",
        decls: "fn total(values: Int...) -> Int {\n  var sum = 0\n  for value in values {\n    sum += value\n  }\n  sum\n}\n\n",
        body: "total(1, 2, 3)",
        ty: "Int",
        value: "6",
        foreign: "Bool",
    },
    Rule {
        section: "Calls",
        decls: "struct Wrapper<T> {\n  value: T\n}\n\nfn unwrap<T>(wrapper: Wrapper<T>) -> T {\n  wrapper.value\n}\n\n",
        body: "unwrap(Wrapper(value: 5))",
        ty: "Int",
        value: "5",
        foreign: "String",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "-7",
        ty: "Int",
        value: "-7",
        foreign: "Bool",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "!true",
        ty: "Bool",
        value: "false",
        foreign: "Int",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "1 + 2 * 3",
        ty: "Int",
        value: "7",
        foreign: "Bool",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "1 == 1",
        ty: "Bool",
        value: "true",
        foreign: "Int",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "let zero = 0\nlet short = false && 1 / zero == 0\nlet long = true || 1 / zero == 0\n\"{short} {long}\"",
        ty: "String",
        value: "false true",
        foreign: "Int",
    },
    Rule {
        section: "Operators",
        decls: "",
        body: "let first = Vector.of(1)\nlet second = first\nlet other = Vector.of(1)\n\"{first is second} {first is other} {first == other}\"",
        ty: "String",
        value: "true false true",
        foreign: "Int",
    },
    Rule {
        section: "Assignment",
        decls: "",
        body: "var total = 1\ntotal += 2\ntotal",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    Rule {
        section: "Assignment",
        decls: "",
        body: "var total = 1\ntotal = 2",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "`?`",
        decls: "fn parse(ok: Bool) -> Result<Int, Error> {\n  match ok {\n    true => Ok(1)\n    false => Err(Error(\"no\"))\n  }\n}\n\n",
        body: "let value = parse(true)?\nOk(value + 1)",
        ty: "Result<Int, Error>",
        value: "Ok(2)",
        foreign: "Result<String, Error>",
    },
    Rule {
        section: "`?`",
        decls: "fn parse(ok: Bool) -> Result<Int, Error> {\n  match ok {\n    true => Ok(1)\n    false => Err(Error(\"no\"))\n  }\n}\n\n",
        body: "let value = parse(false)?\nOk(value + 1)",
        ty: "Result<Int, Error>",
        value: "Err(no)",
        foreign: "Result<String, Error>",
    },
    Rule {
        section: "`?`",
        decls: "fn first(values: Array<Int>) -> Option<Int> {\n  let head = values.get(0)?\n  Some(head + 1)\n}\n\n",
        body: "first([4])",
        ty: "Option<Int>",
        value: "Some(5)",
        foreign: "Option<String>",
    },
    Rule {
        section: "`await`",
        decls: "async fn work() -> Int {\n  7\n}\n\n",
        body: "await work()",
        ty: "Int",
        value: "7",
        foreign: "Bool",
    },
    Rule {
        section: "Blocks",
        decls: "",
        body: "{\n  let value = 1\n  value + 1\n}",
        ty: "Int",
        value: "2",
        foreign: "Bool",
    },
    Rule {
        section: "Blocks",
        decls: "",
        body: "{\n  let value = 1\n}",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "Blocks",
        decls: "",
        body: "1 + 1\n\"discarded\"",
        ty: "String",
        value: "discarded",
        foreign: "Int",
    },
    Rule {
        section: "`if`",
        decls: "",
        body: "if 1 < 2 {\n  \"then\"\n} else {\n  \"else\"\n}",
        ty: "String",
        value: "then",
        foreign: "Int",
    },
    // The rule this issue exists to settle: the branch runs, and its value
    // is still not the `if`'s.
    Rule {
        section: "`if`",
        decls: "",
        body: "var ran = false\nlet value = if true {\n  ran = true\n  1\n}\n\"{ran} {value}\"",
        ty: "String",
        value: "true ()",
        foreign: "Int",
    },
    Rule {
        section: "`match`",
        decls: "",
        body: "match 2 {\n  1 => \"one\"\n  2 => \"two\"\n  _ => \"other\"\n}",
        ty: "String",
        value: "two",
        foreign: "Int",
    },
    Rule {
        section: "Patterns",
        decls: "",
        body: "match 5 {\n  bound => bound + 1\n}",
        ty: "Int",
        value: "6",
        foreign: "Bool",
    },
    // A `-` pattern holds an ordinary expression, evaluated in the arm's
    // enclosing scope each time the pattern is tried, so it can name a
    // binding. The reference records that it is wider than "a literal".
    Rule {
        section: "Patterns",
        decls: "",
        body: "let bound = 1\nmatch -1 {\n  -bound => \"negated\"\n  _ => \"other\"\n}",
        ty: "String",
        value: "negated",
        foreign: "Int",
    },
    Rule {
        section: "Patterns",
        decls: "enum Status {\n  Pending\n  Active(Int)\n}\n\n",
        body: "match Status.Active(3) {\n  Status.Pending => 0\n  Status.Active(since) => since\n}",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    Rule {
        section: "Patterns",
        decls: "",
        body: "match Some(4) {\n  Some(value) => value\n  None => 0\n}",
        ty: "Int",
        value: "4",
        foreign: "Bool",
    },
    Rule {
        section: "Loops",
        decls: "",
        body: "var seen = 0\nfor value in [1, 2, 3] {\n  seen += value\n}\nseen",
        ty: "Int",
        value: "6",
        foreign: "Bool",
    },
    Rule {
        section: "Loops",
        decls: "",
        body: "for value in [1, 2] {\n  value\n}",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "Loops",
        decls: "",
        body: "var seen = 0\nwhile seen < 3 {\n  seen += 1\n}\nseen",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    // Every loop is `()`, `while true` included, and a `break` operand is
    // evaluated for its effects and discarded. Whether a loop should ever
    // carry a value is issue #87.
    Rule {
        section: "Loops",
        decls: "",
        body: "var attempts = 0\nwhile true {\n  attempts += 1\n  if attempts == 3 {\n    break attempts\n  }\n}",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "Loops",
        decls: "",
        body: "var seen = 0\nfor value in [1, 2] {\n  seen = value\n  break value\n}",
        ty: "()",
        value: "()",
        foreign: "Int",
    },
    Rule {
        section: "Loops",
        decls: "",
        body: "var odd = 0\nfor value in [1, 2, 3, 4] {\n  if value % 2 == 0 {\n    continue\n  }\n  odd += value\n}\nodd",
        ty: "Int",
        value: "4",
        foreign: "Bool",
    },
    Rule {
        section: "`return`",
        decls: "",
        body: "return 3",
        ty: "Int",
        value: "3",
        foreign: "Bool",
    },
    Rule {
        section: "Lambdas",
        decls: "",
        body: "let add: fn(Int) -> Int = fn(n) {\n  n + 1\n}\nadd(1)",
        ty: "Int",
        value: "2",
        foreign: "Bool",
    },
    // A lambda's `return` returns from the lambda, and the checker holds it
    // to the lambda's own result type.
    Rule {
        section: "Lambdas",
        decls: "",
        body: "let classify: fn(Int) -> String = fn(n) {\n  if n > 0 {\n    return \"positive\"\n  }\n  \"other\"\n}\n\"{classify(1)} {classify(-1)}\"",
        ty: "String",
        value: "positive other",
        foreign: "Int",
    },
    // Captures are read when the closure is made, not when it is called.
    Rule {
        section: "Lambdas",
        decls: "",
        body: "var counter = 1\nlet read: fn() -> Int = fn() {\n  counter\n}\ncounter = 2\n\"{read()} {counter}\"",
        ty: "String",
        value: "1 2",
        foreign: "Int",
    },
    Rule {
        section: "`scope`",
        decls: "",
        body: "scope tasks {\n  let task = tasks.spawn {\n    7\n  }\n  await task\n}",
        ty: "Int",
        value: "7",
        foreign: "Bool",
    },
    Rule {
        section: "Ranges",
        decls: "",
        body: "0..<3",
        ty: "Range",
        value: "0..<3",
        foreign: "Int",
    },
    Rule {
        section: "Copies and aliases",
        decls: "struct Counter {\n  hits: Int\n}\n\n",
        body: "var first = Counter(hits: 1)\nvar second = first\nsecond.hits = 2\n\"{first.hits} {second.hits}\"",
        ty: "String",
        value: "1 2",
        foreign: "Int",
    },
    Rule {
        section: "Copies and aliases",
        decls: "",
        body: "var first = Vector.of(1, 2)\nvar second = first\nsecond.push(3)\n\"{first.length()} {second.length()}\"",
        ty: "String",
        value: "3 3",
        foreign: "Int",
    },
    Rule {
        section: "`dyn Trait`",
        decls: "trait Named {\n  fn name(self) -> String\n}\n\nstruct Room {\n  id: Int\n}\n\nimpl Named for Room {\n  fn name(self) -> String {\n    \"room {self.id}\"\n  }\n}\n\n",
        body: "let shown: dyn Named = Room(id: 2)\nshown.name()",
        ty: "String",
        value: "room 2",
        foreign: "Int",
    },
    // Where the checker abstains it abstains about a sub-expression, not
    // about the whole body: these two name a method on something it has no
    // type for, and still owe the reference the type and value below.
    Rule {
        section: "Where the checker abstains",
        decls: "",
        body: "let sizes = Vector.of(1)\nsizes.length()",
        ty: "Int",
        value: "1",
        foreign: "Bool",
    },
    Rule {
        section: "Where the checker abstains",
        decls: "",
        body: "let empty = []\nempty.length()",
        ty: "Int",
        value: "0",
        foreign: "Bool",
    },
    // A written `dyn Trait` is wrapped where it is written and a lambda's
    // inferred one is not, so nothing a program can ask may tell the two
    // apart: both have the same type, so both answer the same way.
    Rule {
        section: "`dyn Trait`",
        decls: "trait Named {\n  fn name(self) -> String\n}\n\nstruct Room {\n  id: Int\n}\n\nimpl Named for Room {\n  fn name(self) -> String {\n    \"room {self.id}\"\n  }\n}\n\n",
        body: "let direct: dyn Named = Room(id: 1)\nlet make: fn(Int) -> dyn Named = fn(n) {\n  Room(id: n)\n}\n\"{direct == make(1)} {make(1).name()}\"",
        ty: "String",
        value: "true room 1",
        foreign: "Int",
    },
];

// -------------------------------------------------------------- rejections

/// Every program the reference does not admit.
static REJECTIONS: &[Rejection] = &[
    Rejection {
        section: "`if`",
        source: "/// Asks an `if` with no `else` for a value.\nexport fn probe() -> Int {\n  if true {\n    1\n  }\n}\n",
        code: "cove::type::mismatch",
    },
    Rejection {
        section: "`if`",
        source: "/// Gives an `if` a condition that is not a `Bool`.\nexport fn probe() -> () {\n  if 1 {\n  }\n}\n",
        code: "cove::type::condition",
    },
    Rejection {
        section: "`if`",
        source: "/// Gives an `if`'s branches different types.\nexport fn probe() -> () {\n  let described = if true {\n    \"one\"\n  } else {\n    1\n  }\n}\n",
        code: "cove::type::branches",
    },
    Rejection {
        section: "Loops",
        source: "/// Asks a loop for a value its `break` cannot supply.\nexport fn probe() -> () {\n  let found: Int = while true {\n    break 1\n  }\n}\n",
        code: "cove::type::mismatch",
    },
    Rejection {
        section: "Loops",
        source: "/// Breaks with no loop to leave.\nexport fn probe() -> () {\n  break\n}\n",
        code: "cove::resolve::break_outside_loop",
    },
    Rejection {
        section: "Loops",
        source: "/// Continues with no loop to skip.\nexport fn probe() -> () {\n  continue\n}\n",
        code: "cove::resolve::continue_outside_loop",
    },
    Rejection {
        section: "Loops",
        source: "/// Breaks out of a loop from inside a closure.\nexport fn probe() -> () {\n  for value in [1] {\n    let escape = fn() {\n      break\n    }\n  }\n}\n",
        code: "cove::resolve::break_outside_loop",
    },
    Rejection {
        section: "Loops",
        source: "/// Iterates something that is not a sequence.\nexport fn probe() -> () {\n  for value in 1 {\n  }\n}\n",
        code: "cove::type::iterable",
    },
    Rejection {
        section: "Operators",
        source: "/// Adds an `Int` to a `String`.\nexport fn probe() -> Int {\n  1 + \"one\"\n}\n",
        code: "cove::type::operator",
    },
    Rejection {
        section: "Assignment",
        source: "/// Assigns to a place `let` made read-only.\nexport fn probe() -> Int {\n  let fixed = 1\n  fixed = 2\n  fixed\n}\n",
        code: "cove::type::read_only_place",
    },
    Rejection {
        section: "Calls",
        source: "/// Names two labels in the other order than the declaration writes them.\nfn between(low: Int, high: Int) -> Int {\n  high - low\n}\n\n/// Calls it out of order.\nexport fn probe() -> Int {\n  between(high: 2, low: 1)\n}\n",
        code: "cove::type::label_order",
    },
    Rejection {
        section: "Calls",
        source: "/// Pushes onto a place `let` made read-only.\nexport fn probe() -> Int {\n  let items = Vector.of(1)\n  items.push(2)\n  items.length()\n}\n",
        code: "cove::type::read_only_place",
    },
    Rejection {
        section: "Calls",
        source: "/// Pushes onto a temporary, which is no place at all.\nexport fn probe() -> () {\n  Vector.of(1).push(2)\n}\n",
        code: "cove::type::not_a_place",
    },
    Rejection {
        section: "Operators",
        source: "/// Compares the identity of a value type.\nexport fn probe() -> Bool {\n  1 is 1\n}\n",
        code: "cove::type::operator",
    },
    Rejection {
        section: "Assignment",
        source: "/// Assigns to something that is not a place.\nexport fn probe() -> () {\n  1 = 2\n}\n",
        code: "cove::parse::invalid_assignment_target",
    },
    Rejection {
        section: "`?`",
        source: "/// Applies `?` to something that is not a `Result` or an `Option`.\nexport fn probe() -> Result<Int, Error> {\n  let value = 1?\n  Ok(value)\n}\n",
        code: "cove::type::try_operand",
    },
    Rejection {
        section: "`?`",
        source: "/// Applies `?` in a function that returns no `Result`.\nfn parse() -> Result<Int, Error> {\n  Ok(1)\n}\n\n/// Propagates out of a function with nowhere to propagate to.\nexport fn probe() -> Int {\n  parse()?\n}\n",
        code: "cove::type::try_return",
    },
    Rejection {
        section: "`await`",
        source: "/// Awaits something that is not a task.\nexport fn probe() -> Int {\n  await 1\n}\n",
        code: "cove::type::await_operand",
    },
    Rejection {
        section: "Names",
        source: "/// Names something nothing in scope explains.\nexport fn probe() -> Int {\n  missing\n}\n",
        code: "cove::type::unknown_name",
    },
    Rejection {
        section: "Field access",
        source: "struct Point {\n  x: Int\n}\n\n/// Reads a field a struct does not have.\nexport fn probe() -> Int {\n  Point(x: 1).y\n}\n",
        code: "cove::type::unknown_field",
    },
    Rejection {
        section: "Calls",
        source: "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n/// Passes an argument the parameter does not admit.\nexport fn probe() -> Int {\n  twice(n: \"four\")\n}\n",
        code: "cove::type::mismatch",
    },
    Rejection {
        section: "Calls",
        source: "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n/// Passes more arguments than there are parameters.\nexport fn probe() -> Int {\n  twice(1, 2)\n}\n",
        code: "cove::type::arity",
    },
    Rejection {
        section: "Calls",
        source: "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n/// Leaves a required parameter unfilled.\nexport fn probe() -> Int {\n  twice()\n}\n",
        code: "cove::type::missing_argument",
    },
    Rejection {
        section: "Calls",
        source: "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n/// Labels an argument with a name no parameter has.\nexport fn probe() -> Int {\n  twice(count: 1)\n}\n",
        code: "cove::type::unknown_label",
    },
    Rejection {
        section: "Calls",
        source: "/// Calls something that is not a function.\nexport fn probe() -> Int {\n  let value = 1\n  value(2)\n}\n",
        code: "cove::type::not_callable",
    },
    Rejection {
        section: "Declarations",
        source: "/// Declares a parameter with no type.\nexport fn probe(n) -> Int {\n  1\n}\n",
        code: "cove::type::missing_parameter_type",
    },
    Rejection {
        section: "`match`",
        source: "enum Status {\n  Pending\n  Active\n}\n\n/// Leaves a case of an enum uncovered.\nexport fn probe() -> Int {\n  match Status.Pending {\n    Status.Pending => 1\n  }\n}\n",
        code: "cove::resolve::non_exhaustive_match",
    },
    Rejection {
        section: "Patterns",
        source: "enum Status {\n  Pending\n  Active(Int)\n}\n\n/// Binds the wrong number of payload values.\nexport fn probe() -> Int {\n  match Status.Pending {\n    Status.Pending => 0\n    Status.Active(since, extra) => since\n  }\n}\n",
        code: "cove::type::payload_arity",
    },
    Rejection {
        section: "Patterns",
        source: "/// Matches an `Int` against a `String` literal.\nexport fn probe() -> Int {\n  match 1 {\n    \"one\" => 1\n    _ => 0\n  }\n}\n",
        code: "cove::type::pattern",
    },
    Rejection {
        section: "`dyn Trait`",
        source: "trait Named {\n  fn name(self) -> String\n}\n\nstruct Room {\n  id: Int\n}\n\n/// Converts a type that declares no conformance.\nexport fn probe() -> String {\n  let shown: dyn Named = Room(id: 1)\n  \"shown\"\n}\n",
        code: "cove::type::mismatch",
    },
    Rejection {
        section: "`dyn Trait`",
        source: "trait Named {\n  fn name(self) -> String\n}\n\nstruct Room {\n  id: Int\n}\n\nimpl Named for Room {\n  fn name(self) -> String {\n    \"room\"\n  }\n}\n\nfn show<T: Named>(value: T) -> String {\n  value.name()\n}\n\n/// Gives a `dyn Trait` value to a bounded type parameter.\nexport fn probe() -> String {\n  let shown: dyn Named = Room(id: 1)\n  show(shown)\n}\n",
        code: "cove::type::unsatisfied_bound",
    },
    Rejection {
        section: "`dyn Trait`",
        source: "trait Named {\n  fn name(self) -> String\n}\n\nstruct Room {\n  id: Int\n}\n\n/// Calls a method on a parameter with no bound to resolve it.\nexport fn probe<T>(value: T) -> String {\n  value.name()\n}\n",
        code: "cove::type::unbounded_parameter",
    },
    Rejection {
        section: "Tasks",
        source: "/// Wraps mutable storage a `Shared` may not hold.\nexport fn probe() -> () {\n  let cell = Shared(Vector.of(1))\n}\n",
        code: "cove::type::task_safety",
    },
];

// ------------------------------------------------------------------- traps

/// Every failure the reference leaves to run time.
static TRAPS: &[Trap] = &[
    Trap {
        section: "Operators",
        decls: "",
        body: "let big = 9223372036854775807\nbig + 1",
        ty: "Int",
        message: "overflow",
    },
    Trap {
        section: "Operators",
        decls: "",
        body: "let zero = 0\n1 / zero",
        ty: "Int",
        message: "by zero",
    },
    // Two enums declare a `Red`, so resolution cannot tell which enum a
    // bare `Red` pattern belongs to and abstains about exhaustiveness.
    Trap {
        section: "`match`",
        decls: "enum Signal {\n  Red\n  Green\n}\n\nenum Paint {\n  Red\n  Blue\n}\n\n",
        body: "match Signal.Green {\n  Red => 1\n}",
        ty: "Int",
        message: "no `match` arm covers",
    },
    Trap {
        section: "Tasks",
        decls: "",
        body: "let held = Vector.of(1)\nscope tasks {\n  let task = tasks.spawn {\n    held.length()\n  }\n  await task\n}",
        ty: "Int",
        message: "cannot capture",
    },
];

// ----------------------------------------------------------------- harness

/// Writes one rule's body out as the module the suite compiles and runs.
fn probe_source(decls: &str, body: &str, ty: &str) -> String {
    let body = body
        .lines()
        .map(|line| match line.is_empty() {
            true => String::new(),
            false => format!("  {line}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{decls}/// Produces the value the conformance suite pins.\nexport fn {ENTRY}() -> {ty} {{\n{body}\n}}\n")
}

/// Everything `cove check` reports for one inline module, and the resolved
/// program when nothing stops it.
fn check(source: &str) -> (Arc<SourceMap>, Vec<Diagnostic>, Option<Arc<Program>>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from(format!("{MODULE}/main.cove"));
    let file = sources.add(path.clone(), source.to_string());
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(errors) => return (Arc::new(sources), errors, None),
    };
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules: BTreeMap::from([(
            MODULE.to_string(),
            Module {
                name: MODULE.to_string(),
                dir: PathBuf::from(MODULE),
                units: vec![Unit { file, path, ast }],
            },
        )]),
    };
    let program = match resolve(&package) {
        Ok(program) => program,
        Err(errors) => return (Arc::new(sources), errors, None),
    };
    let diagnostics = cove_sema::typeck::check(&package, &program);
    (Arc::new(sources), diagnostics, Some(Arc::new(program)))
}

/// The errors alone, since a warning stops nothing.
fn errors(source: &str) -> Vec<Diagnostic> {
    check(source)
        .1
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect()
}

/// Runs a probe that checked cleanly, with no capability granted: a rule of
/// the language is answerable to the language alone.
fn run(source: &str) -> Result<Value, String> {
    let (sources, diagnostics, program) = check(source);
    let refused: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "the checker refused it: {}",
            summarize(&diagnostics)
        ));
    }
    let hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
    let runtime = Runtime::new(
        program.expect("a module that checks cleanly resolved"),
        sources,
        Arc::new(hosts),
    );
    Interpreter::new(&runtime)
        .run_entry(MODULE, ENTRY, Vec::new())
        .map_err(|error| error.message)
}

/// The codes of a set of diagnostics, for a failure message that says what
/// was reported instead of only that something was.
fn summarize(diagnostics: &[Diagnostic]) -> String {
    match diagnostics.is_empty() {
        true => "nothing".to_string(),
        false => diagnostics
            .iter()
            .map(|diagnostic| format!("{} ({})", diagnostic.code, diagnostic.message))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[test]
fn every_rule_has_the_type_and_the_value_the_reference_gives_it() {
    let mut failures = Vec::new();
    for rule in RULES {
        let source = probe_source(rule.decls, rule.body, rule.ty);
        let reported = errors(&source);
        if !reported.is_empty() {
            failures.push(format!(
                "{}: `{}` should have type `{}`, but the checker reported {}",
                rule.section,
                rule.body,
                rule.ty,
                summarize(&reported)
            ));
            continue;
        }
        match run(&source) {
            Ok(value) if value.to_string() == rule.value => {}
            Ok(value) => failures.push(format!(
                "{}: `{}` should evaluate to `{}`, but produced `{value}`",
                rule.section, rule.body, rule.value
            )),
            Err(error) => failures.push(format!(
                "{}: `{}` should evaluate to `{}`, but failed: {error}",
                rule.section, rule.body, rule.value
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_rule_says_which_type_it_has_and_not_merely_one_that_fits() {
    let mut failures = Vec::new();
    for rule in RULES {
        let reported = errors(&probe_source(rule.decls, rule.body, rule.foreign));
        // The refusal has to be a *type* mismatch, and not merely some
        // error. A foreign type that is malformed rather than foreign --
        // `dyn Int`, say -- would be refused for saying nothing about the
        // body, and the claim this test makes would be satisfied without a
        // type ever having been compared.
        if !reported
            .iter()
            .any(|diagnostic| diagnostic.code == cove_sema::typeck::MISMATCH)
        {
            failures.push(format!(
                "{}: `{}` is not a `{}`, but the checker did not say so: {}",
                rule.section,
                rule.body,
                rule.foreign,
                summarize(&reported)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_rejected_program_is_refused_by_the_diagnostic_the_reference_names() {
    let mut failures = Vec::new();
    for rejection in REJECTIONS {
        let reported = errors(rejection.source);
        if !reported
            .iter()
            .any(|diagnostic| diagnostic.code == rejection.code)
        {
            failures.push(format!(
                "{}: expected `{}`, but the checker reported {}\n{}",
                rejection.section,
                rejection.code,
                summarize(&reported),
                rejection.source
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_trap_checks_cleanly_and_stops_the_run() {
    let mut failures = Vec::new();
    for trap in TRAPS {
        let source = probe_source(trap.decls, trap.body, trap.ty);
        let reported = errors(&source);
        if !reported.is_empty() {
            failures.push(format!(
                "{}: `{}` is a run-time failure, but the checker refused it: {}",
                trap.section,
                trap.body,
                summarize(&reported)
            ));
            continue;
        }
        match run(&source) {
            Err(message) if message.contains(trap.message) => {}
            Err(message) => failures.push(format!(
                "{}: `{}` should fail with `{}`, but failed with `{message}`",
                trap.section, trap.body, trap.message
            )),
            Ok(value) => failures.push(format!(
                "{}: `{}` should fail with `{}`, but produced `{value}`",
                trap.section, trap.body, trap.message
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------- coverage

/// Declares a set of AST forms once and expands it twice: into the list of
/// names the coverage test must account for, and into an exhaustive `match`
/// that names the form a node is.
///
/// The `match` is the half that does the work. A variant added to the AST
/// makes this file stop compiling, and there is no way to make it compile
/// again without saying what the new form is called — which is what puts it
/// in the list the test then insists on finding.
macro_rules! forms {
    ($ty:ty, $all:ident, $name_of:ident, $( $pat:pat => $name:literal ),* $(,)?) => {
        const $all: &[&str] = &[$($name),*];

        fn $name_of(form: &$ty) -> &'static str {
            match form { $($pat => $name),* }
        }
    };
}

forms! {
    ExprKind, EXPRESSION_FORMS, expression_form,
    ExprKind::Int(..) => "Int",
    ExprKind::Float(..) => "Float",
    ExprKind::Bool(..) => "Bool",
    ExprKind::Duration(..) => "Duration",
    ExprKind::Str(..) => "Str",
    ExprKind::Unit => "Unit",
    ExprKind::Ident(..) => "Ident",
    ExprKind::ArrayLit(..) => "ArrayLit",
    ExprKind::Field { .. } => "Field",
    ExprKind::Call { .. } => "Call",
    ExprKind::Unary { .. } => "Unary",
    ExprKind::Binary { .. } => "Binary",
    ExprKind::Assign { .. } => "Assign",
    ExprKind::Try(..) => "Try",
    ExprKind::Await(..) => "Await",
    ExprKind::Block(..) => "Block",
    ExprKind::If { .. } => "If",
    ExprKind::Match { .. } => "Match",
    ExprKind::For { .. } => "For",
    ExprKind::While { .. } => "While",
    ExprKind::Return(..) => "Return",
    ExprKind::Break(..) => "Break",
    ExprKind::Continue => "Continue",
    ExprKind::Lambda { .. } => "Lambda",
    ExprKind::Scope { .. } => "Scope",
    ExprKind::Range { .. } => "Range",
}

forms! {
    PatternKind, PATTERN_FORMS, pattern_form,
    PatternKind::Wildcard => "Wildcard",
    PatternKind::Binding(..) => "Binding",
    PatternKind::Literal(..) => "Literal",
    PatternKind::Variant { .. } => "Variant",
}

/// The forms a program reaches, read off its tree.
///
/// Reading the tree rather than searching the text is what makes the count
/// mean anything: `()` occurs in every empty parameter list and `7` occurs
/// inside `17`, so a substring can be found in a program that does not hold
/// the form at all. Every walk below descends into every sub-expression,
/// because a sub-tree quietly skipped here would be a form the suite reports
/// as covered and never looks at.
#[derive(Default)]
struct Coverage {
    expressions: BTreeSet<&'static str>,
    patterns: BTreeSet<&'static str>,
}

impl Coverage {
    fn item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                self.params(&decl.params);
                self.block(&decl.body);
            }
            ItemKind::Trait(decl) => {
                for method in &decl.methods {
                    self.params(&method.params);
                    if let Some(default) = &method.default {
                        self.block(default);
                    }
                }
            }
            ItemKind::Impl(block) => {
                for item in &block.items {
                    self.item(item);
                }
            }
            // A struct, an enum and an alias are written in types alone, and
            // a type holds no expression.
            ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
        }
    }

    /// A default is an expression written in a signature, so it is source
    /// the program reaches like any other.
    fn params(&mut self, params: &[Param]) {
        for param in params {
            if let Some(default) = &param.default {
                self.expression(default);
            }
        }
    }

    fn block(&mut self, block: &Block) {
        for statement in &block.statements {
            match &statement.kind {
                StmtKind::Let { value, .. } => self.expression(value),
                StmtKind::Expr(expr) => self.expression(expr),
                StmtKind::Item(item) => self.item(item),
            }
        }
        if let Some(tail) = &block.tail {
            self.expression(tail);
        }
    }

    fn expression(&mut self, expr: &Expr) {
        self.expressions.insert(expression_form(&expr.kind));
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            // An interpolation is an expression the literal encloses, and
            // more than one rule above hides a form inside one.
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(expr) = part {
                        self.expression(expr);
                    }
                }
            }
            ExprKind::ArrayLit(elements) => {
                for element in elements {
                    self.expression(element);
                }
            }
            ExprKind::Field { base, .. } => self.expression(base),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                self.expression(callee);
                for arg in args {
                    self.expression(&arg.value);
                }
                if let Some(trailing) = trailing {
                    self.expression(trailing);
                }
            }
            ExprKind::Unary { operand, .. } => self.expression(operand),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expression(lhs);
                self.expression(rhs);
            }
            ExprKind::Assign { target, value, .. } => {
                self.expression(target);
                self.expression(value);
            }
            ExprKind::Try(operand) | ExprKind::Await(operand) => self.expression(operand),
            ExprKind::Block(block) => self.block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expression(condition);
                self.block(then_branch);
                if let Some(else_branch) = else_branch {
                    self.expression(else_branch);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expression(scrutinee);
                for arm in arms {
                    self.pattern(&arm.pattern);
                    self.expression(&arm.body);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                self.expression(iterable);
                self.block(body);
            }
            ExprKind::While { condition, body } => {
                self.expression(condition);
                self.block(body);
            }
            ExprKind::Return(operand) | ExprKind::Break(operand) => {
                if let Some(operand) = operand {
                    self.expression(operand);
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                self.params(params);
                self.block(body);
            }
            ExprKind::Scope { body, .. } => self.block(body),
            ExprKind::Range { start, end, .. } => {
                self.expression(start);
                self.expression(end);
            }
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        self.patterns.insert(pattern_form(&pattern.kind));
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => {}
            // A literal pattern holds an ordinary expression, `-bound` and
            // `1` alike, so it is walked like one.
            PatternKind::Literal(expr) => self.expression(expr),
            PatternKind::Variant { payload, .. } => {
                for pattern in payload {
                    self.pattern(pattern);
                }
            }
        }
    }
}

/// Every form the AST admits appears in a program this suite *accepts*, so a
/// form cannot be added to the language and left out of the reference
/// unnoticed.
///
/// Only [`RULES`] and [`TRAPS`] count. A [`REJECTIONS`] entry is a program
/// the checker refuses, and a form that occurs nowhere else has been given
/// neither a type nor a value by anything here — counting it would let the
/// suite claim coverage of a form no Cove implementation ever runs.
#[test]
fn every_expression_and_pattern_form_appears_in_an_accepted_program() {
    let accepted = RULES
        .iter()
        .map(|rule| (rule.section, probe_source(rule.decls, rule.body, rule.ty)))
        .chain(
            TRAPS
                .iter()
                .map(|trap| (trap.section, probe_source(trap.decls, trap.body, trap.ty))),
        );
    let mut coverage = Coverage::default();
    for (section, source) in accepted {
        let mut sources = SourceMap::new();
        let file = sources.add(PathBuf::from(format!("{MODULE}/main.cove")), source.clone());
        // A source that does not parse has no tree to read, and a suite that
        // skipped it would go on reporting the coverage it used to have.
        let unit = match cove_syntax::parse_file(&sources, file) {
            Ok(unit) => unit,
            Err(reported) => panic!(
                "{section}: a program this suite accepts did not parse: {}\n{source}",
                summarize(&reported)
            ),
        };
        for item in &unit.items {
            coverage.item(item);
        }
    }
    let missing: Vec<String> = EXPRESSION_FORMS
        .iter()
        .filter(|form| !coverage.expressions.contains(*form))
        .map(|form| format!("ExprKind::{form}"))
        .chain(
            PATTERN_FORMS
                .iter()
                .filter(|form| !coverage.patterns.contains(*form))
                .map(|form| format!("PatternKind::{form}")),
        )
        .collect();
    assert!(
        missing.is_empty(),
        "no rule and no trap exercises: {}",
        missing.join(", ")
    );
}
