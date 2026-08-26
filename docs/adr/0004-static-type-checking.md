# ADR 0004: Static type checking

- Status: Accepted
- Date: 2026-08-25
- Amended by: [ADR 0005](0005-module-to-module-imports.md), which gave the
  checker the import environment "Checking is per-module" said it would need,
  [ADR 0006](0006-traits-and-dispatch.md), which turned "parametric and
  unbounded" into parametric with bounds, and this ADR's own four amendments
  below — "Amendment (2026-08-25): one builtin table", which unmirrors the
  builtin method table "What is not checked yet" recorded,
  "Amendment (2026-08-25): the builtins that are called on nothing", which
  unmirrors the three lists the first one left,
  "Amendment (2026-08-25): what a builtin type is made of", which unmirrors
  the builtin enums' cases and declares the builtin structs' fields, closing
  that sequence, and "Amendment (2026-08-26): four kinds of unknown", which
  splits `Ty::Unknown`'s two jobs into four and decides what each one costs a
  reader of `cove check`
- Amends: [ADR 0001](0001-mvp-language-design.md)'s account of the builtin
  `Error`, which the third amendment settles as a struct carrying a `message`
  a program may read
- Implemented by: PR #12; the declaration-parameter rule by PR #43; the Host
  API schema by PR #48; the first amendment by PR #51; the second by PR #54;
  the third by PR #55; the fourth by PR #82
- Implementation status: partial — see "What is not checked yet" below, which
  is now shorter than it was: the checker reads the Host API schema (#44), the
  builtin table it used to mirror (#49), the constructors, assertions, and
  sequence names it mirrored after that (#50), and the builtin enums' cases
  and structs' fields (#53). What remains of it is classified rather than
  merely listed, by the fourth amendment (#76)

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
`Array<T>`, `Vector<T>`, `Map<K, V>`, `MapEntry<K, V>`, `Set<T>`,
`Option<T>`, `Result<T, E>`, `Task<T>`, function types, and the type a `scope`
binds, which is where `spawn` lives.

Two of them are structs rather than opaque: an `Error` carries a `message` and
a `MapEntry` carries a `key` and a `value`, and a program reads all three by
field. The third amendment below says where those fields are declared.

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
to check yet. The fourth amendment revisits the first two of those three: they
are not the same kind of not-knowing, and saying so is what makes them
readable in `cove check`.

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
three amendments below say where it went, and where the cases, the
constructors, and the fields went after it.

`Error` used to be checked as an opaque type with no fields, which is no
longer true: it is a builtin struct carrying a `message`, exactly as the
runtime has always built it. The third amendment says why.

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

What was left mirrored, and stated so a reader did not have to derive it:
`assert` and `assertEqual`'s arities, the constructor names `Ok`, `Err`,
`Some`, `Error`, and `Shared`, and which receivers are told that `count()` is
spelled `length()`. None of those is a method or an associated function, so
none was in this table. [Issue #50](https://github.com/myuon/cove/issues/50)
tracked them, and the amendment below closes it.

## Amendment (2026-08-25): the builtins that are called on nothing

The amendment above moved what a builtin *type* declares and left three
smaller lists behind, for a stated reason: `BuiltinSchema` is keyed by a
receiver, and a constructor and an assertion have none. `Ok(1)` and
`assert(true)` are written bare, the way a call to a declared function is.
Reasoning from the shape of the existing table is what kept them mirrored, and
the third of them had already drifted — the runtime taught `map.count()` the
`length()` spelling and `cove check` did not — which is the usual sign that
two lists cannot see each other.

**They get a table of their own rather than a receiver they do not have.**
`cove_schema::builtins::FREE_BUILTINS` is seven entries — five constructors
and two assertions — each a name, which of the two kinds it is, the type
parameters it binds, the parameters it takes, and what it produces. The kind
is in the table because both ends ask it before anything else: the interpreter
dispatches an assertion through the one path that carries its arguments'
source text, and the checker gives an assertion's wrong argument count a
different sentence than a constructor's.

**The signature is in it, and not only the arity, because the arity alone
would have left the five names written out twice anyway.** A constructor's
result is generic and its payload is read off it — `Ok(value: T) ->
Result<T, E>` — so the declaration that says what `Ok` produces is also the
one that says what it carries, and the checker opens both against the type the
call site expects rather than restating either. That is what makes `Ok`'s
error type come from the place the value is going, which is the only thing
that can know it. `Shared`'s task-safety rule is deliberately *not* in the
table: it is about what a type is, not what a call takes, so the checker
enforces it on a type and the runtime on a value, as they always did.

**`count()` is derived rather than declared.** The receivers told that the
element count is spelled `length()` are the builtin types that declare
`length` — six of them — read off the same table at both ends. That closes the
drift by construction rather than by agreement, and it changes one diagnostic:
`cove check` now teaches the spelling on a `Map` and a `Set`, which the
runtime already did.

`crates/cove-runtime/tests/builtin_schema.rs` covers the new table the way it
covers the old one, with one program per entry driven through resolve, check,
and a real run.

## Amendment (2026-08-25): what a builtin type is made of

The two amendments above moved what a builtin type *answers* and what a
builtin called on nothing *takes*. One list about the builtins was still
written out in both crates, and it was out of scope for both:
[issue #53](https://github.com/myuon/cove/issues/53), the case names of the
builtin enums. `cove-sema` knew that an `Option` is `Some` and `None` and a
`Result` is `Ok` and `Err` — twice, in fact, once for `match` exhaustiveness
and once to give a pattern's binding a type — and `cove-runtime` knew the same
four strings where it built the values.

That list had not drifted and is hard to drift: a case name the two sides
disagreed about fails immediately and loudly, because every program that calls
a function returning a `Result` builds one value and matches on it. `Ok` is
not a name either side can quietly forget. So the argument for closing it is
consistency — the rule the two previous amendments settled was one list, not
two that happen to agree.

**A `BuiltinSchema` says what a type is made of, not only what it answers.**
An entry now declares its `cases` if it is an enum and its `fields` if it is a
struct, in the same shape a host type's are: a case has a name and a payload,
a field has a name and a type. A case's payload is written in the parameters
the receiver binds — `Some` carries a `T`, `Err` carries an `E` — so a pattern
reads its binding's type off the scrutinee exactly as a method reads its
result off its receiver, through the same substitution. `Option` and `Result`
are the two enums; `Error` and `MapEntry` are the two structs, and `MapEntry`
joins the table rather than staying beside it, so that the labels its
initializer takes and the fields a program reads are one declaration.

**`Error` carries a `message`, and saying so closed a gap rather than a
duplication.** The runtime has always built an `Error` as a struct value with
a `message` field and has always served a read of it; the checker treated
`Error` as opaque and answered "`Error` has no field `message`", suggesting a
method instead — and `Error` has no methods at all, so the suggestion pointed
nowhere. Nothing in the Language Card or ADR 0001 ever said an `Error` is
opaque, and this table's own entry for `Error` already said the opposite: "the
message is read as a field". The checker was the half that was missing, so
`Error`'s field is declared here and `error.message` now type-checks as a
`String`. That is the one behavioural change these three amendments make to a
program that runs.

**What the runtime reads, and how far.** `Value::ok`, `Value::err`,
`Value::some`, `Value::none`, and `Value::error` build their values out of
this table, and the readers beside them — `is_ok`, `err_payload`, and their
neighbours — are how everything else in the workspace asks which case a value
is. The four strings are written once. The constructor `match` in
`cove_runtime::builtins` stays a `match`, on the rule the first amendment set:
a body reaches into a `Value`, and
`crates/cove-runtime/tests/builtin_schema.rs` is what holds it to the table. A
case is exercised there by a program that builds it *and* matches it *and*
answers a number only that arm produces, because a `match` that took the wrong
arm still runs.

**What is left mirrored, and why it is not a list.** One "static half" comment
survives in `cove-sema`, on `task_safe_argument`, and the second amendment's
reading of it holds: the `Shared` task-safety rule has a half in each crate
because each sees something the other cannot — the checker sees the type
arguments a program writes, the runtime sees a struct whose *field* holds a
vector — so that pair is two enforcements of one rule rather than two copies
of one list. The name-to-`Ty` translation in `Checker::builtin_type` also
stays, because `Ty` is `cove-sema`'s own representation and no other crate can
name it; what *was* a second list there, how many type parameters each builtin
binds, is read off `BuiltinSchema::parameters` now.

Nothing else about the builtins is stated in two crates. That is the end of
the sequence [#48](https://github.com/myuon/cove/issues/48),
[#49](https://github.com/myuon/cove/issues/49),
[#50](https://github.com/myuon/cove/issues/50), and
[#53](https://github.com/myuon/cove/issues/53) was working toward.

## Amendment (2026-08-26): four kinds of unknown

`Ty::Unknown` was one variant doing two jobs. It suppressed cascades after a
reported error, and it stood for everything the checker deliberately did not
know: a host module with no schema, a schema's `HostType::Any`, a capitalized
name nothing declares, a type written where a value belongs, a lambda
parameter with no expected type, a lambda's early `return`. Because all of
them compared equal to every type, a successful `cove check` could not be read:
it might mean the package was proved, or it might mean an unknown had spread
far enough to validate whatever was written after it.
[Issue #76](https://github.com/myuon/cove/issues/76) asks for the inventory
and the split. This is it.

**Four kinds, and the kind decides what the reader is told.** Every place the
pass produces an unknown now names its kind by which constructor it calls:
`Ty::recovery`, `Ty::dynamic_boundary`, `Ty::unconstrained`, or — for the few
internal positions the surrounding form settles before anything reads them —
`Ty::placeholder`. A *language gap* has no constructor at all, which is the
point: it is reported, not produced.

| kind | what it means | `cove check` |
|------|---------------|--------------|
| recovery | everything there was to say was said, here or upstream | silent |
| dynamic boundary | a host this build ships no schema for | warning |
| unconstrained API | a shipped schema's `HostType::Any` | note |
| language gap | information the checker should have been given | warning or error |

**Recovery is defined by its silence.** It covers both "the diagnostic is a
few lines above" and "the unknown being propagated was classified where it
arose", because the rule it enforces is the same one in both cases: add
nothing. One mistake is one diagnostic however far its unknown travels, which
`a_recovery_unknown_is_reported_once_however_far_it_spreads` pins.

**A dynamic boundary is a fact about the build, so it warns.** A call into an
unshipped host module used to be silent; it warns now
(`cove::type::unchecked_host_call`), as a type from one already did
(`cove::type::host_type`). Every such abstention goes through one function,
`host_schema`, which answers from `cove_schema`'s shipped table alone. That is
deliberately the only seam: giving the checker schemas an embedder registered
— [issue #74](https://github.com/myuon/cove/issues/74) — is a matter of
answering from that table too, and every warning here becomes an ordinary
check without another line of this pass changing.

**`HostType::Any` is a promise, so it is a note.** The promise is worth
stating exactly, because the two ends of a signature are not symmetric. In a
*parameter*, `Any` says every value is accepted: no argument is a mistake,
neither the compiler nor the boundary rejects one, and nothing is given up
because there was no constraint to check. In a *result*, it says the operation
may answer with a value of any type — and from that call onwards the program
holds a value no schema described, so a field read off it, a call made on it,
or a place it is stored into is checked at run time and by nothing before it.
The call is noted (`cove::type::unconstrained_result`), naming the schema's own
signature. A note and not a warning: a schema declaring `Any` is a design
decision, not a fault in the program calling it, so no strictness setting
should be able to fail on one. `cove-diag` gains `Severity::Note` for exactly
this, and `cove check` counts notes apart from warnings; `--deny-warnings`
acts on warnings only.

**A language gap is reported.** Each of these used to pass silently:

- a name nothing in scope explains is an error, whatever its case. A
  capitalized one used to be assumed to come from a host and warn — but a host
  reaches a module through `use` like everything else, so the assumption named
  no real way for the name to arrive and only let an unknown through;
- a type, a module, or a host operation written where a value belongs is an
  error (`cove::type::not_a_value`). `Vector` in `Vector.of(1, 2)` is
  understood as part of the call; a bare `Vector`, `console`, or `Counter` is
  not a form with a type in this system, and never was;
- an early `return` in a function value nothing expects is an error
  (`cove::type::lambda_return`). Such a lambda takes its result from its
  body's value, so a `return` produces one where the body's value is not, and
  nothing written says what the two must agree on. A function value the place
  holding it types is unaffected, which is the correction the diagnostic
  offers. A lambda the checker has *already* abstained about — the block
  `clock.timeout` bounds, whose type the schema declared `Any` — is not
  reported twice;
- an unannotated lambda parameter, an empty array literal, and a bare `None`,
  each where nothing in particular is expected, warn
  (`cove::type::unconstrained`). Warnings rather than errors, because the
  value is still usable, the operations that do not depend on the missing type
  are still checked, and writing the type is always available.

**What a clean check now guarantees**, which is the sentence the whole change
exists to make true: `cove check` reporting nothing means every type in the
package was proved. A run whose only output is notes means the same except at
the calls the notes name. A run with warnings means the package either reaches
a host this build cannot see or left something to infer that nothing written
settles — and `--deny-warnings` is exactly the request that neither happened.

Four of the repository's own programs turned out to be leaning on a gap and
now say what they meant: a handler array in `examples/callbacks`, and an empty
array, an `Option`, and a closure parameter in `tests/e2e`. That is the
amendment paying for itself on the day it landed.
