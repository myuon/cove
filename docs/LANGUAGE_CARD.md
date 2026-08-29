# Cove Language Card

> Draft 0.1 — a one-page map of the intended language, not a specification.

Cove should feel familiar if you know TypeScript, Go, Swift, or Rust. This card
records the parts you should not have to guess.

[LANGUAGE_REFERENCE.md](LANGUAGE_REFERENCE.md) is where a rule is stated once
and in full: for every expression and pattern form, what it resolves to, how it
is typed, how it evaluates, and which errors it can produce. This card stays a
map and sends you there rather than growing into the specification itself.

## Program shape

```cove
use console.println

/// Returns a greeting for `name`.
export fn greet(name: String) -> String {
  "Hello, {name}!"
}

/// Runs the command-line program.
export fn main(args: Array<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr("world")
  console.println(greet(name))?
  Ok(())
}
```

## Familiar core

- `let` creates a read-only place; `var` creates a mutable place.
- Functions use `fn name(arg: Type) -> ReturnType`.
- `test fn name() -> Result<Unit, Error>` declares a test. `test` sits where
  `export` sits and excludes it: `cove test` is a test's only caller. The
  builtin `assert(condition)` and `assertEqual(actual, expected)` report a
  failure as an `Err` that quotes the condition's source text.
- Calls support static argument labels: `request(url: endpoint, timeout: 5s)`.
  Labels are parameter names, so they appear in declaration order.
- Struct initialization uses synthesized labeled calls: `User(name: "A")`.
- A variadic `items: T...` is an immutable `Array<T>` inside the function.
- Blocks and control-flow forms are expressions.
- Structs are product types; enums are tagged unions. Both may have methods
  and associated functions in an `impl` block.
- `match` must cover every enum case.
- `a..b` includes `b`; `a..<b` excludes it. A range is an ordinary value.
- Every sequence reports its element count as `length()`.
- Generics use angle brackets: `Array<T>`, `Result<T, E>`.
- Traits are nominal and explicitly implemented; dynamic dispatch is distinct
  from generic static dispatch.
- The last expression in a block is its value; `return` exits early.
- An `if` with no `else` is `Unit`: the branch runs, and its value is
  discarded, because there is no second branch to give the other case a value.
- A `for` or `while` loop is an expression. It evaluates to `Unit`, because it
  can reach its end without breaking, so a `break expr` operand is evaluated
  for its effects and discarded. `continue` skips to the next iteration.
- Comments use `//` and `/* ... */`.

## Statements end at the end of a line

A line break ends a statement when the line could have ended there. There is
no `;`.

```cove
let total = subtotal + tax
console.println("{total}")?
```

A line break does not end anything when the statement is visibly incomplete or
the next line visibly continues it:

- inside `(`, `[`, or `<`, so multi-line argument lists and array literals read
  normally;
- when the line ends with an operator, so `a +` continues onto the next line;
- when the next line begins with `.`, so method chains split across lines;
- before `else` and before a `match` arm's `=>`.

An operator that can only continue an expression cannot start a line. Cove
reports that rather than guessing which reading was meant.

## Values and errors

- There is no implicit `null`.
- Missing values use `Option<T>`: `Some(value)` or `None`.
- Expected failure uses `Result<T, E>`: `Ok(value)` or `Err(error)`.
- `unwrapOr(fallback)` takes the value out of either: what a `Some` or an `Ok`
  carries, and `fallback` for a `None` or an `Err`. It says nothing about the
  error, which is what `result.mapError { ... }` is for.
- `Error` is the builtin error type. `Error("...")` builds one and `.message`
  reads what it carries.
- `expr?` returns the error from the current function.
- `await` binds tighter than `?`, so `await task()?` awaits and then propagates.
- Panics are reserved for broken invariants, not ordinary errors.
- `==` means value equality. Identity, when available, is explicit: `is`
  compares shared-storage identity, at `==`'s precedence, and is defined only
  for identity-capable handles such as `Vector`.

## Values, collections, and mutation

Assignment and ordinary argument passing perform field-wise shallow copies.

- Primitive values, strings, enums, and structs have value semantics.
- `Array<T>` is fixed-length and immutable; `[1, 2]` is an array.
- `Vector<T>` is growable and mutable; `Vector.of(1, 2)` constructs one.
- `Array`, `Vector`, `String`, and ranges all answer `length()`.
- A character is a `String` of length 1: `text.chars()` takes a string apart
  into them, and `String.fromCodePoint(codePoint)` builds one from the number
  that names it, or an `Err` for a number that names none — one out of range,
  or a surrogate half. There is no `Character` type.
- `Int.parse(text)` reads a decimal number and `Int.parseRadix(text, radix)`
  reads one in a radix from 2 to 36. Text that is not a number answers `Err`;
  a radix outside 2 to 36 stops the run, because it is the call that is wrong
  and not the text.
- Vector assignment is O(1), and aliases share elements and length.
- `Map` and `Set` are immutable in the MVP.
- Cove never performs an implicit deep copy.

```cove
let fixed = [1, 2]

var first = Vector.of(1, 2)
var second = first
second.push(3)
// both vectors observe [1, 2, 3]
```

A `let Vector<T>` is a valid read-only view but may observe mutation through
another alias. Mutating receivers say `var self`. Ordinary parameters are
shallow copies; a `var` parameter is a non-escaping inout alias and is marked
at both declaration and call site.

```cove
fn fill(var output: Vector<Int>)
fill(var output)
```

A closure captures a snapshot of each binding it reads, taken where the
closure is written, so assigning to that binding afterwards does not change
what the closure sees. A captured `Vector` or `Shared` still shares its
storage, because copying either copies the handle.

`vector.freeze()` consumes a locally unique vector and returns an immutable
array in O(1). `vector.toArray()` is the O(n) fallback when uniqueness cannot
be proved. Other independent graph copies require an explicit
`impl Snapshot for Type { fn snapshot(self) -> Type { ... } }`; closures,
tasks, and Host resources do not conform by default.

## Evaluation

- Evaluation order is left to right.
- Integer overflow is a broken invariant, not a wrapped result. `Int` division
  and remainder by zero are too. `Float` is IEEE 754 and stops at nothing:
  `1.0 / 0.0` is `inf`.
- Collection iteration order is defined by each collection type.
- There are no implicit numeric, string, or boolean conversions.
- Imports do not execute initialization code; fallible or asynchronous setup is
  an ordinary function called explicitly.
- Native and Wasm targets must preserve source-level semantics.

## Modules describe their boundary first

```text
src/
  booking/
    create.cove
    validate.cove
```

Each directory is one module, and its name is derived from its path. All Cove
files in that directory are implementation units of the same module. An
`export` declaration is public; other declarations are module-private.

`export opaque struct User { ... }` exports `User`'s name and its exported
methods and associated functions only; its fields and its synthesized
labeled constructor `User(...)` stay module-private. The declaring module is
unaffected — inside it `User` is an ordinary struct, constructed and
inspected like any other. Exporting an enum always exports its cases, so
there is no opaque enum; wrap the variant in a struct instead.

`cove outline` derives the typed public interface, definition locations, and
required capabilities directly from source. `cove api snapshot` records that
derived interface for compatibility checks without duplicating it by hand.

`///` doc comments attach ordinary prose to the following declaration. The
compiler preserves them for `outline`, documentation, and inspection tools.
Missing doc comments on public modules and declarations produce a warning by
default. Projects can deny warnings in CI. Private declarations need comments
only when their intent is not clear from code.

## Authority comes from the host

Cove code has no ambient I/O authority when embedded. File, network, clock,
process, database, and similar operations are typed Host APIs. The compiler
reports which capabilities each function requires from its call graph.

That report is a **lower bound**. It is the whole list only for a function
whose calls the compiler can all follow, and even then it can still name a
capability no particular run reaches. A function that calls a value — a
`fn`-typed parameter, a closure taken out of a collection — or dispatches
through a `dyn Trait` or a bounded generic parameter is reported as
**capability-open**, because what runs is chosen by its caller. `cove outline`
and `cove api` mark such a function, and mark anything that calls one.

A lambda is charged to the function that *writes* it, so a callback that
prints already requires `console` wherever it was built, whatever later
invokes it. Function types carry no latent capability set, and no static
result decides anything: the runtime refuses a Host API call the run was not
granted, and that check is the only authority.

The host chooses the entry function and grants authority at the execution
boundary:

```toml
[run.server]
entry = "server.main"
allow = ["http", "clock"]
```

A capability belongs to an *operation*, not to a module. An operation usually
requires the capability its module is named for, and one that requires a
narrower one says so in the schema: `console.println` requires `console` and
`console.eprintln`, which writes to the program's diagnostic stream rather
than to its output, requires `console.error`. So a host may grant a program
the right to print its records and refuse it the right to comment on them, or
the other way about, and a grant written before an operation existed never
comes to cover it. A capability's name says which module it opens.

A host may provide real, fake, filtered, remote, or denied implementations.
The runtime rejects Host API calls that were not granted. An operation's
argument, result, and error types come from its schema, and both ends check
a call against it: `cove check` checks its arguments and arity at the call
site and gives it the schema's result type, and the boundary checks the same
arguments again before the host is reached and the host's answer after. A
value that is not one the declared type admits, followed all the way down
through `Array`, `Option`, `Result`, and a declared type's name, stops the
run rather than travelling on. `Any` admits everything, and a declared
type's fields are not checked by the boundary. An embedding registers host
modules of its own and hands their schemas to the compiler, which checks calls
into them exactly as it checks calls into the shipped ones. A host module no
schema describes is one the compiler cannot see: a call into it is checked at
the boundary alone, and `cove check` warns that it is.

`cove test` is such a host: it grants each test the capabilities its call
graph requires, taking every implementation's fake form so a suite is
deterministic, and `[test] allow_real = [...]` names the exceptions. It grants
a capability-open test that same lower bound rather than widening it, and says
so when the boundary then refuses a call.

## Tasks and resource control

Concurrent work belongs to a task scope. Leaving the scope waits for or cancels
its child tasks. Immutable task-safe values such as arrays may cross task
boundaries. A vector cannot cross, even through `let`; finish it as an array
or wrap mutable state in `Shared`, which is the MVP's only synchronized type.
Closures are task-safe only when every capture is. Host resources declare
task-safety in their Host API schema.

`Shared(value)` wraps task-safe mutable state, and `lock` is its only
operation: it holds the value for the whole of the closure it is given and
produces that closure's value, so a read-modify-write is one operation rather
than two that can race. A `Shared` crosses a task boundary by sharing rather
than by copying, which is the one exception to the copy rule.

```cove
let metrics = Shared(Metrics(requests: 0, failures: 0))
metrics.lock(fn(var value) { value.record(failed) })
```

`Shared` ownership must stay acyclic. A cell may not come to hold a handle to
itself; `lock` rejects a closure that would leave the cell reachable from its
own new value. A cycle through two or more cells is not detected and leaks.

Memory is managed by a precise, non-moving mark-and-sweep collector, whose
allocation, live-heap, peak-heap, and pause-time numbers are observable but not
enforced: strict memory isolation is a process, container, or microVM
boundary's job, not this runtime's. CPU, time, concurrency, and Host-call
limits are runtime controls, not termination proofs, and every one of them is
imposed today. The concurrency limit bounds the tasks a run holds at once: a
`spawn` past it stops the run, refused before its thread exists rather than
made to wait for a sibling to finish.

## Annotations

The MVP defines no annotations. Decorator syntax is reserved for behavior with
specified compiler or runtime semantics; unknown annotations are errors.

## Tooling contract

```text
cove fmt       format source deterministically
cove check     parse, resolve, and type-check
cove run       run a program
cove build     produce a native executable
cove outline   show modules and architectural boundaries
cove test      run tests
cove api diff  compare source and operational interfaces
cove impact    explain what a proposed change can affect
cove trace     record and inspect source-level execution
cove replay    reproduce recorded Host API interactions
cove generate  run explicit, capability-controlled code generation
```

Compiler errors should state the Cove rule, point to the relevant source, and
show a textual correction when one is unambiguous.

A diagnostic is an error, a warning, or a note. An error is a program the
toolchain refuses. A warning is one it accepts and doubts, which
`cove check --deny-warnings` refuses instead. A note is one it accepts and
does not doubt: it names something the compiler deliberately did not prove —
a Host API result or field whose schema declares `Any`, above all — so no
strictness setting turns one into a failure. A `cove check` that reports
nothing has checked every type the package wrote down, and the two things it
still cannot prove — a host module no schema describes, shipped or supplied
by an embedder, and a builtin constructor's type parameter nothing settles —
are named in `cove_sema::typeck` rather than left to be found.
