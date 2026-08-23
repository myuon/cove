# Cove Language Card

> Draft 0.1 — a one-page map of the intended language, not yet a specification.

Cove should feel familiar if you know TypeScript, Go, Swift, or Rust. This card
records the parts you should not have to guess.

## Program shape

```cove
/// Prints a greeting for a command-line user.
module greeting {
  provides { greet }
  uses { console.println }
  allow { console }
  entrypoints { main }
}

struct Person {
  name: String
}

fn greet(person: Person) -> String {
  "Hello, {person.name}!"
}

/// Greets the user passed on the command line.
fn main(args: List<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr("world")
  console.println(greet(Person { name }))?
  Ok(())
}
```

## Familiar core

- `let` creates an immutable binding; `var` creates a mutable one.
- Functions use `fn name(arg: Type) -> ReturnType`.
- Blocks and control-flow forms are expressions.
- Structs are product types; enums are tagged unions.
- `match` must cover every enum case.
- Generics use angle brackets: `List<T>`, `Result<T, E>`.
- The last expression in a block is its value; `return` exits early.
- Comments use `//` and `/* ... */`.

## Values and errors

- There is no implicit `null`.
- Missing values use `Option<T>`: `Some(value)` or `None`.
- Expected failure uses `Result<T, E>`: `Ok(value)` or `Err(error)`.
- `expr?` returns the error from the current function.
- Panics are reserved for broken invariants, not ordinary errors.
- `==` means value equality. Identity, when available, is explicit.

## Evaluation

- Evaluation order is left to right.
- Integer overflow behavior is defined and consistent across backends.
- Collection iteration order is defined by each collection type.
- There are no implicit numeric, string, or boolean conversions.
- Imports do not execute initialization code.
- Native and Wasm targets must preserve source-level semantics.

## Modules describe their boundary first

```cove
/// Validates a booking request and creates a confirmed booking.
module booking.creation {
  provides { createBooking }
  uses { inventory.reserve payment.authorize }
  owns { BookingDraft }
  allow { database network clock }
  entrypoints { http.createBooking }
}
```

- `provides`: public declarations exported by the module.
- `uses`: dependencies visible from this module.
- `owns`: data or concepts for which the module is responsible.
- `allow`: coarse Host API capabilities the module may use.
- `entrypoints`: declarations invoked from outside the module.

`///` doc comments attach ordinary prose to the following declaration. The
compiler preserves them for `outline`, documentation, and inspection tools.
Projects may lint for missing documentation without turning prose into
language semantics.

## Authority comes from the host

Cove code has no ambient I/O authority when embedded. File, network, clock,
process, database, and similar operations are typed Host APIs. A host can
provide real, fake, filtered, remote, or denied implementations.

`allow` declares a maximum authority; it does not create authority. The host
must still grant and implement the capability.

## Tasks and resource control

Concurrent work belongs to a task scope. Leaving the scope waits for or cancels
its child tasks; work does not silently outlive its owner.

CPU, memory, time, concurrency, and host-call limits are runtime controls, not
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
cove trace     run with source-level execution tracing
cove test      run tests
```

Compiler errors should state the Cove rule, point to the relevant source, and
show a textual correction when one is unambiguous.
