# ADR 0004: Static type checking

- Status: Accepted
- Date: 2026-08-25

## Context

The Language Card asserts typed function signatures, generic types such as
`Array<T>` and `Result<T, E>`, no implicit numeric, string, or boolean
conversions, and a `cove outline` that derives "the typed public interface".
The tooling contract says `cove check` will "parse, resolve, and type-check".

None of that is true today. Types are parsed, recorded, and reprinted, and
then discarded. Every rule the card states about types is enforced — when it
is enforced at all — by the interpreter, at the moment the program runs.

The cost is not only honesty. Resolution documents in four places that its
match-exhaustiveness and capability analyses are approximations *because* the
scrutinee's or receiver's type is unknown. A method call resolves to every
method of that name in the module. Nothing else in the language pays back as
much.

## Decision

Add a type checker between resolution and execution. `cove check` runs it,
and `cove run` refuses to execute a package that does not check.

### Annotations are mandatory at boundaries, inferred inside

Function parameters, return types, struct fields, and enum payloads are
written. Local `let` and `var` bindings infer from their initializer, and
lambda parameters infer from the expected type at the call site.

This is the rule ADR 0001 already chose — "Function and public API boundaries
remain explicitly typed" with local inference — and it keeps the checker a
single pass over each body with no global constraint solving. A reader can
determine a function's type from its signature alone, which is also what
makes `outline` and API snapshots derivable.

### Nominal, no subtyping, invariant generics

Two types are equal when they name the same declaration and their arguments
are equal. There is no subtyping, no coercion, and no variance. `Array<Int>`
is not an `Array<Any>`, because there is no `Any`.

Subtyping is where a small type system stops being explainable on one page.
The card's budget is the reason to refuse it, not an implementation
convenience.

### Generics are parametric and unbounded in the MVP

A type parameter is checked by unification at the call site and substituted
into the signature. It carries no bounds, because bounds require traits, and
traits are not implemented. A generic function may therefore only do to its
type parameters what any type admits: move them, store them, compare them for
equality.

When traits land, bounds attach here without changing this decision.

### Checking is per-module

Cross-module references do not exist yet, so a module checks against its own
declarations plus the builtins. When module-to-module imports land, the
checker gains an import environment; nothing else changes.

### Builtin types

`Unit`, `Bool`, `Int`, `Float`, `String`, `Duration`, `Error`, `Range`,
`Array<T>`, `Vector<T>`, `Map<K, V>`, `Set<T>`, `Option<T>`, `Result<T, E>`,
`Task<T>`, and function types. Their method signatures live in one table that
the checker and the interpreter's builtins both read, so a method cannot exist
at run time without a type, or have a type it does not honour.

### Diagnostics carry the rule

A type error states the Cove rule it enforces and shows a correction when one
is unambiguous, like every other diagnostic. "Expected `Int`, found `String`"
is not enough on its own; the card's promise is that errors teach the
language.

## Consequences

Programs that ran before may now fail to check. That is the point, and the
end-to-end suite will show exactly which and why.

The interpreter's dynamic checks stay. They are the runtime's own invariants,
they cover what the checker deliberately cannot see, and removing them would
trade a clear error for undefined behaviour. Where the checker makes one
unreachable, the interpreter's version becomes a broken-invariant guard rather
than a user-facing diagnostic.

Match exhaustiveness and capability derivation can stop approximating once the
checker can tell them a scrutinee's or receiver's type. Neither is changed by
this ADR; both become able to change.

Higher-kinded types, implicit instance search, dependent types, and effect
polymorphism remain out of scope, as ADR 0001 decided.
