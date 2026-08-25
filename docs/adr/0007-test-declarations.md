# ADR 0007: Test declarations and `cove test`

- Status: Accepted
- Date: 2026-08-25
- Implemented by: PR #19
- Implementation status: complete

## Context

The Language Card's tooling contract lists `cove test` — "run tests". There is
no such command and, more fundamentally, no way to write a test: the language
has no test declaration, and nothing in `cove.toml` names one.

Cove's own test suite is written in Rust. That is fine for the compiler, and
useless for anyone writing a Cove program.

## Decision

A test is an ordinary exported-shaped declaration marked `test`, and
`cove test` runs every one in the package.

```cove
/// A greeting names the person it greets.
test fn greetsByName() -> Result<Unit, Error> {
  assert(greet("Ada") == "Hello, Ada!")?
  Ok(())
}
```

### `test` is a declaration modifier, not a decorator

The card reserves decorator syntax for behaviour with specified compiler or
runtime semantics and defines no decorators. A test is exactly such a
behaviour, so it could have been one. It is a modifier instead because
`export` already occupies that position and means a comparable thing: who may
call this. A test is a declaration the toolchain calls and nothing else can.

A `test fn` may not be `export`ed. Its whole contract is that the test runner
is its only caller.

### A test returns `Result<Unit, Error>`

Not a boolean, and not nothing. A test that fails does so the way every other
Cove function reports expected failure, so `?` works inside it and a failure
carries a message. This is the reason not to invent a test-only failure
mechanism.

### `assert` is a builtin

`assert(condition) -> Result<Unit, Error>` and
`assertEqual(actual, expected) -> Result<Unit, Error>`, the latter reporting
both values on failure. They are builtins rather than a library because the
failure message wants the source text of the condition, which only the
compiler has.

Panics remain reserved for broken invariants. A failing assertion is an
expected failure, so it is an `Err`.

### Capabilities

A test declares no capabilities of its own; `cove test` grants what the test's
call graph requires, taking each capability's fake implementation by default.
A test that wants the real one says so:

```toml
[test]
allow_real = ["clock"]
```

Defaulting to fakes is what makes a test suite deterministic and safe to run
anywhere. The card's host boundary already provides a fake for every
capability, so this costs nothing to honour.

### Scope

No fixtures, no setup and teardown, no parameterised tests, no test ordering
control, no parallelism. Each is a decision that should be forced by a
representative test suite rather than added because other languages have it.

## Consequences

`test fn` is new surface on the Language Card, which the card must state.

Tests are part of a module and can see its private declarations, which is what
makes them useful and is why they are not a separate package.

`cove test` needs to report per-test results, a summary, and a non-zero exit
on failure. It should reuse the diagnostic renderer so a failing assertion
points at source like every other error.
