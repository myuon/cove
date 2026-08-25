# ADR 0004: Static type checking

- Status: Accepted
- Date: 2026-08-25
- Amended by: [ADR 0005](0005-module-to-module-imports.md), which gave the
  checker the import environment "Checking is per-module" said it would need,
  and [ADR 0006](0006-traits-and-dispatch.md), which turned "parametric and
  unbounded" into parametric with bounds
- Implemented by: PR #12
- Implementation status: partial — see "What is not checked yet" below

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
`Task<T>`, function types, and the type a `scope` binds, which is where
`spawn` lives.

Calling an `async fn` yields `Task<T>`, matching what the interpreter does;
`await` settles it to `T`.

An `if` without an `else` has type `Unit` and its branch's value is discarded.
The interpreter returns the taken branch's value, so this rule is stricter
than execution rather than in conflict with it.

Builtin method signatures should live in one table that the checker and the
interpreter's builtins both read, so a method cannot exist at run time without
a type. They cannot literally share one today: `cove-sema` does not depend on
`cove-runtime` and must not, so the checker mirrors the table and says so.
Unifying them needs a crate both can depend on.

### Two types no program can write

`Unknown` is what the checker does not know: a Host API call, or a capitalized
name no module declares. It is equal to every type and every operation on it
yields `Unknown`, so an unknown never produces a cascade of errors about
itself. A *lowercase* unresolved name is an error rather than an unknown,
because locals, parameters, module functions, and `use`d host items exhaust
the ways one could be in scope.

`Never` is what `return`, `break`, and `continue` have. It is equal to
everything, so a control-flow arm never disagrees with a value arm.

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

## What is not checked yet (2026-08-25)

The checker described above exists and `cove check` runs it, so the Context's
"none of that is true today" is history. Two of its promises are not kept, and
naming them here is cheaper for a reader than deriving them from the code.

The Host API schema is still unread. ADR 0001 asks for one description of each
operation's argument, result, and error types shared by the compiler, runtime,
and CLI, and `OperationSchema` holds all three — but nothing in `cove-sema`
looks at it, so a host call's result is `Unknown` and its arguments are
unchecked. The checker does not hide this: it warns (`HOST_TYPE`) wherever a
host type is named, which is why `cove check` reports warnings on
`examples/server` and `examples/callbacks`. The runtime does not check the
other end either, which is [issue #38](https://github.com/myuon/cove/issues/38).

"Annotations are mandatory at boundaries" is not enforced for a declaration's
parameters. A parameter written without a type becomes `Unknown` rather than
an error, and `Unknown` is equal to everything, so a call passing the wrong
thing checks clean. The cause is structural — a declaration's parameter and a
lambda's parameter are the same node, and ADR 0004 wants the second to infer —
and the fix is a decision rather than a line, so it is
[issue #41](https://github.com/myuon/cove/issues/41).

Two smaller things are worth stating because they read as gaps and are not. A
bound written on a `struct`, `enum`, or `type` parameter is rejected outright,
because a bound is checked where its parameter is instantiated and only a call
site instantiates one. And the builtin method table is still mirrored between
`cove-sema` and `cove-runtime` rather than shared, exactly as this ADR said it
would have to be until a crate both can depend on exists.
