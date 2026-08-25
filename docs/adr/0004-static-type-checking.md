# ADR 0004: Static type checking

- Status: Accepted
- Date: 2026-08-25
- Amended by: [ADR 0005](0005-module-to-module-imports.md), which gave the
  checker the import environment "Checking is per-module" said it would need,
  [ADR 0006](0006-traits-and-dispatch.md), which turned "parametric and
  unbounded" into parametric with bounds, and this ADR's own
  "Amendment (2026-08-25): one builtin table" below, which unmirrors the
  builtin method table "What is not checked yet" recorded
- Implemented by: PR #12; the declaration-parameter rule by PR #43; the Host
  API schema by PR #48; the amendment by PR #51
- Implementation status: partial — see "What is not checked yet" below, which
  is now shorter than it was: the checker reads the Host API schema (#44) and
  the builtin table it used to mirror (#49)

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

Function parameters, struct fields, and enum payloads are written; the
checker refuses a declaration whose parameter has none. A return type may be
omitted, and an omitted one means `Unit` — a default, not a hole, because a
function's result is always produced whether or not the signature names it,
and `benches/startup`'s `export fn main() {}` is the shortest program that
relies on that default staying implicit. Local `let` and `var` bindings infer
from their initializer, and lambda parameters infer from the expected type at
the call site, because a lambda has no signature of its own to read outside
the call that gives it one.

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
Unifying them needs a crate both can depend on. That crate now exists and the
mirroring is over; the amendment below says how, and what the shared table had
to be able to say that the Host API schema cannot.

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
"none of that is true today" is history. What it does not check, it says
here, because naming it is cheaper for a reader than deriving it from the
code.

The Host API schema is read. ADR 0001 asks for one description of each
operation's argument, result, and error types shared by the compiler, runtime,
and CLI; that description is now `cove-schema`, a crate below both this one
and the runtime, and the checker reads it. A call into a host module the
toolchain ships is checked at its call site — its arity, each argument against
the type declared for it, and its result is the type the schema declares
rather than `Unknown`. A host type is an ordinary nominal type: `http.Request`
in a signature is checked like any other, and so are its fields, its cases,
and the operations a resource handle answers.
[ADR 0013](0013-host-resource-handles.md)'s second amendment decides all of
it, and closes [issue #44](https://github.com/myuon/cove/issues/44).

Three things about a host call stay unchecked here, each for a stated reason.
A host module the toolchain does *not* ship is invisible to any compiler — an
embedding registers its modules at run time — so a call into one is `Unknown`
and warns (`HOST_TYPE`), and the boundary is what checks it. An operation that
declares `HostType::Any`, which is what `clock.timeout` says of the work it
bounds, becomes `Unknown` too, because a type carrying no constraint and *the
checker does not know* are the same thing at a call site. And a host
resource's declared task-safety is enforced by the runtime alone; no shipped
resource declares itself unsafe to cross a task boundary, so there is nothing
to check yet.

"Annotations are mandatory at boundaries" used to have a second exception,
worth recording now that it is closed: a declaration's parameter could omit
its type and become `Unknown`, which is equal to everything, so a call
passing the wrong thing checked clean. That is now refused
(`cove::type::missing_parameter_type`), closing
[issue #41](https://github.com/myuon/cove/issues/41).

One smaller thing is worth stating because it reads as a gap and is not: a
bound written on a `struct`, `enum`, or `type` parameter is rejected outright,
because a bound is checked where its parameter is instantiated and only a call
site instantiates one.

The builtin method table used to be listed here too, as still mirrored between
`cove-sema` and `cove-runtime` rather than shared. It is not any more; the
amendment below says where it went.

## Amendment (2026-08-25): one builtin table

This ADR wrote the builtin methods and associated functions out twice — once
in `cove-sema` with types attached and once in `cove-runtime` with bodies
attached — and said the duplication would stand "until a crate both can depend
on exists". [ADR 0013](0013-host-resource-handles.md)'s second amendment built
that crate for the Host API schema. The same argument moves the builtins into
it, and [issue #49](https://github.com/myuon/cove/issues/49) asks for exactly
that: two lists that must agree and cannot see each other drift silently, and
the builtin pair had no cross-crate test at all, so a method added to one side
and not the other was either an error `cove check` reported on a program that
ran or a program that checked and then failed at run time.

`cove_schema::builtins` is the table now, and `cove-sema` has none of its own:
`builtin_method`, `builtin_associated`, and `is_builtin_type` read it, and so
does the help line that lists what a receiver does have — which had quietly
become a third copy of the same list, and had drifted, never gaining
`mapError`, `cancel`, `lock`, or `spawn`.

**It needs a vocabulary of its own, and that is the part worth arguing.** The
Host API schema's `HostType` is deliberately monomorphic, because a boundary
has nothing to instantiate a type parameter with. A builtin is the opposite of
that: `Array<T>.get` answers in the element type of the receiver it was called
on, `snapshot` answers in the receiver's own type, `Shared<T>.lock` answers in
whatever its callback produces, and `Vector.of(items: T...)` binds a parameter
of its own. So `cove-schema` carries two vocabularies rather than one widened
one — `HostType` for what crosses the boundary, `BuiltinType` for what the
language says about itself — and they diverge exactly where the two kinds of
signature do. Widening `HostType` would have put generics into every host
signature that has no use for them, and `HostType::Any` — a boundary's way of
saying it does not look inside a value — means nothing for a method the
language itself defines.

**The runtime reads it where reading it is free, and no further.** The
`is_builtin_type` and `is_mutating_method` predicates are the two questions
the interpreter answers from a name alone, and both are the shared table's
now. Dispatch is not: a builtin's body reaches into a runtime `Value`, so it
stays beside the value model, and `call_method` is the hottest path in a
tree-walking interpreter. What holds the `match` to the table is
`crates/cove-runtime/tests/builtin_schema.rs`, which drives every entry
through resolve, check, and a real run. A signature the schema gains with no
body behind it fails a test; a body the schema does not declare is unreachable,
because `cove check` refuses to call a name the table does not have.

What is left mirrored, and stated so a reader does not have to derive it:
`assert` and `assertEqual`'s arities, the constructor names `Ok`, `Err`,
`Some`, `Error`, and `Shared`, and which receivers are told that `count()` is
spelled `length()`. None of those is a method or an associated function, so
none is in this table;
[issue #50](https://github.com/myuon/cove/issues/50) tracks them.
