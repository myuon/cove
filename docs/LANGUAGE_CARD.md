# Cove Language Card

> Draft 0.1 — a one-page map of the intended language, not yet a specification.

Cove should feel familiar if you know TypeScript, Go, Swift, or Rust. This card
records the parts you should not have to guess.

## Program shape

```cove
use console.println

/// Returns a greeting for `name`.
export fn greet(name: String) -> String {
  "Hello, {name}!"
}

/// Runs the command-line program.
export fn main(args: List<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr("world")
  console.println(greet(name))?
  Ok(())
}
```

## Familiar core

- `let` creates a read-only place; `var` creates a mutable place.
- Functions use `fn name(arg: Type) -> ReturnType`.
- Blocks and control-flow forms are expressions.
- Structs are product types; enums are tagged unions.
- `match` must cover every enum case.
- Generics use angle brackets: `List<T>`, `Result<T, E>`.
- Traits are nominal and explicitly implemented; dynamic dispatch is distinct
  from generic static dispatch.
- The last expression in a block is its value; `return` exits early.
- Comments use `//` and `/* ... */`.

## Values and errors

- There is no implicit `null`.
- Missing values use `Option<T>`: `Some(value)` or `None`.
- Expected failure uses `Result<T, E>`: `Ok(value)` or `Err(error)`.
- `expr?` returns the error from the current function.
- Panics are reserved for broken invariants, not ordinary errors.
- `==` means value equality. Identity, when available, is explicit.

## Values, places, and references

Values themselves are not mutable. A storage-backed value in a `let` place may
be shallow-shared in O(1). A `var` place may be updated in place but cannot be
implicitly aliased.

```cove
var values = [1, 2, 3]

let snapshot = values.copy() // independent outer storage
let alias = values.ref()     // reference to the same mutable place
```

`.ref()` leaves the original variable usable; mutations through either name
are visible through the other. It is task-local. Cross-task mutable sharing
requires a synchronized type such as `Shared<T>`. Trivially copyable values do
not require an explicit choice.

A method declares whether it requires a mutable receiver:

```cove
fn length(self) -> Int
fn push(var self, value: T)
```

A `self` method is callable through `let` or `var`. A `var self` method
requires a mutable place or explicit reference. Calls use ordinary method
syntax; Cove has no borrow or lifetime syntax.

## Evaluation

- Evaluation order is left to right.
- Integer overflow behavior is defined and consistent across backends.
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

The host chooses the entry function and grants authority at the execution
boundary:

```toml
[run.server]
entry = "server.main"
allow = ["network", "clock"]
```

A host may provide real, fake, filtered, remote, or denied implementations.
The runtime rejects Host API calls that were not granted.

## Tasks and resource control

Concurrent work belongs to a task scope. Leaving the scope waits for or cancels
its child tasks; work does not silently outlive its owner. Immutable values may
cross task boundaries. Mutable places and task-local references cannot.
Cross-task mutation requires an explicit synchronized type; unsafe captures are
compile errors.

Memory is managed by a precise, non-moving mark-and-sweep collector. CPU, memory, time, concurrency, and host-call limits are runtime controls, not
termination proofs. Exceeding a limit cancels execution with a structured
runtime error.

## Annotations

```cove
/// Reserves inventory and then authorizes payment.
@hot
fn createBooking(request: BookingRequest) -> Result<Booking, BookingError> {
  // ...
}
```

Syntax is reserved for enforceable semantics; prose belongs in doc comments.
Annotations are explicit metadata that changes checking, compilation, or
runtime behavior. Unknown annotations are errors; they never silently change
behavior.

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
