# ADR 0016: Four kinds of unknown

- Status: Accepted
- Superseded in part by [ADR 0038](0038-a-type-nothing-settles-is-not-a-program.md),
  which makes an unconstrained unknown an error rather than a warning, moves
  a schema's `HostType::Any` out of the unknowns into a type of its own, and
  closes the second of the two silences named under "What a clean check
  guarantees". The four kinds, the rule that every kind compares equal to
  every type, the silence of a dynamic boundary, and `Any` being a note and
  never a warning all stand
- Date: 2026-08-26
- Supersedes: [ADR 0004](0004-static-type-checking.md)'s "Two types no program
  can write", which decided that `Unknown` is one thing — "a Host API call, or
  a capitalized name no module declares" — and that a capitalized unresolved
  name is one of them. Neither half survives: `Unknown` carries which of four
  kinds of not-knowing it is, and a capitalized name nothing declares is an
  error like a lowercase one. The rest of ADR 0004 stands, including `Never`
  and the rule that an unknown compares equal to every type.
- Implemented by: PR #82
- Implementation status: complete for the classification and for what each
  kind costs a reader of `cove check`. Two silences remain and are stated
  below rather than claimed away; one of them is [issue
  #74](https://github.com/myuon/cove/issues/74)'s to close.

## Context

[ADR 0004](0004-static-type-checking.md) gave the checker one `Unknown` and
one job for it: be equal to every type, so that one mistake does not print as
ten. That job is real and this ADR keeps it. What ADR 0004 did not decide is
what an unknown *means* to someone reading a successful `cove check`, and by
the time the checker had grown a Host API schema, host resource handles, and
lambdas that take their parameter types from the place holding them, the one
variant was standing for at least four unrelated things at fifteen
construction sites.

Because all of them compare equal to every type, a `cove check` that reported
nothing could not be read. It might mean the package was proved. It might mean
an unknown had spread far enough to validate whatever was written after it —
`needsString(Tagged(n: 1))` where `needsString` wants a `Tagged<String>`
checked clean, and `attempt().mapError { return 42 }` checked clean and then
failed at run time with `` `Int` has no method `length` ``.
[Issue #76](https://github.com/myuon/cove/issues/76) asks for the inventory
and the split.

## Decision

**An unknown carries which kind of not-knowing it is.** `Ty::Unknown` takes an
`Unknown` payload rather than being a bare variant, so the classification is a
value the pass can read rather than a convention about which constructor was
called. Every kind still compares equal to every type: knowing the kind
decides what a reader is told and never what type-checks.

| kind | what it stands for | `cove check` |
|------|--------------------|--------------|
| `Recovery` | an error already reported, here or upstream | silent |
| `DynamicBoundary` | a host module this build ships no schema for | silent here; answered at the `use` |
| `Unconstrained` | nothing that has been read states this type | note, warning, or silent |
| `Placeholder` | a position no reachable program observes | must not escape |

A *language gap* — information the checker should have been given and was not
— has no kind at all, which is the point: it is reported rather than produced.

**Recovery is defined by its silence,** and its reach is one step wider than
the branch that builds it. The arguments of a call that was just rejected are
walked against a recovery expectation, so an empty array or an unannotated
lambda parameter written inside one is not reported as a second mistake. One
mistake is one diagnostic however far its unknown travels.

**A dynamic boundary is not reported at the call.** An embedding registers its
host modules at run time and names them in no table a compiler could read, so
a call into one is unchecked — but that is a fact about the `use` that named
the module and about the build that could not see it. No edit to
`sensors.read` fixes it; the remedy is to hand the module's `ModuleSchema` to
the compiler, which is one thing to say however many calls a program makes.
Issue #74 puts a warning at the `use`, where the remedy is, and this ADR
deliberately does not put a second one at every call site.

What such a call does owe its arguments is the abstention itself, as their
expected type. A callback registered with a host this build has no schema for
is the shape an embedding is written in, and it must not be asked to state a
type nothing on this side could have stated: an early `return` inside one is
not a language gap, because there is no language gap to fill.

**`HostType::Any` is a promise, so it is a note.** The two ends of a signature
are not symmetric. In a *parameter*, `Any` says every value is accepted: no
argument is a mistake, neither the compiler nor the boundary rejects one, and
nothing is given up because there was no constraint to check. In a *result*,
or in a *field*, it says the value may be of any type — and from there onwards
the program holds something no schema described, so a field read off it, a
call made on it, or a place it is stored into is checked at run time and by
nothing before it. Those are noted. A note and not a warning: a schema
declaring `Any` is a design decision, not a fault in the program that calls
it, so no strictness setting should be able to fail on one. `cove-diag` gains
`Severity::Note` for exactly this, `cove check` counts notes apart from
warnings, and `--deny-warnings` acts on warnings only.

**A language gap is reported.** Each of these used to pass silently:

- a name nothing in scope explains is an error, whatever its case. ADR 0004
  made a capitalized one an `Unknown` on the theory that it might come from a
  host; but a host reaches a module through `use` like everything else, so
  that theory named no real way for the name to arrive and only let an unknown
  through;
- a type or a module written where a value belongs is an error
  (`cove::type::not_a_value`). `Vector` in `Vector.of(1, 2)` is understood as
  part of the call; a bare `Vector`, `console`, or `Counter` is not a form
  with a type in this system, and never was;
- an early `return` in a function value nothing expects is an error
  (`cove::type::lambda_return`). Such a lambda takes its result from its
  body's value, so a `return` produces one where the body's value is not, and
  nothing written says what the two must agree on. "Nothing expects it" is
  asked of the expected *result* type specifically: an expectation this pass
  abstained about answers for it, and an expected function type whose own
  result the pass left open does not;
- an unannotated lambda parameter, an empty array literal, a bare `None`, and
  a struct's type parameter that no field mentions, each where nothing in
  particular is expected, warn (`cove::type::unconstrained`). Warnings rather
  than errors, because the value is still usable, the operations that do not
  depend on the missing type are still checked, and writing the type is always
  available. "Nothing in particular is expected" excludes a place this pass
  already abstained about, and a sibling or a branch that settles the type
  counts as something written: `[[], [1]]` and `if c { None } else { Some(1) }`
  are proved, and are silent.

**A host operation is a value.** The interpreter has always bound one and
called it later, so `let log = console.println` is a form the language has.
Reading the schema turns what used to be an unknown into the operation's own
function type, which checks a call made through the value exactly as a direct
call is checked. The one operation this cannot be done for is a *variadic*
one, because Cove has no variadic `fn` type to write: that is the language's
own gap rather than the program's, and it is said out loud as a note
(`cove::type::variadic_as_value`) rather than hidden or refused.

**A placeholder must not escape, and the pass asserts it.** A placeholder is
not a fourth kind of not-knowing; it marks the internal positions the
surrounding form settles before reading them and the branches no reachable
program takes. `Checker::expr` and `Checker::declare` assert in debug builds
that one never reaches the type of an expression or of a binding, so what each
construction site claims about itself is something the test suite holds it to.
Two sites used to break the claim, and each let a program check clean and then
be wrong at run time; both are now `Unconstrained`, which is what they always
were.

## What a clean check guarantees

`cove check` reporting nothing means every type the package wrote down was
checked: every struct field, declared parameter, call to a declared or
imported function, and call into a Host API module this build ships a schema
for was checked against a written or schema-declared type.

Two silences are not covered by that, and are named here rather than left to
be discovered:

- **a host module this build ships no schema for.** Nothing about a call into
  one is proved, and nothing is said about it in the type checker either,
  because the fact belongs to the `use`. Issue #74 is what puts a warning
  there; until it lands, a package that reaches such a host has one unproved
  boundary per `use` and a clean check does not say so.
- **a builtin constructor's type parameter that nothing settles.** `Ok(1)` in
  a place expecting no `Result` is a `Result<Int, _>`, and the `_` is carried
  rather than reported. Closing this means deciding what `Ok(1)` alone should
  mean, which is a language question and not the checker's to answer.

A check whose only output is *notes* means the same, except at the places the
notes name. A check with *warnings* means the package left something to infer
that nothing written settles, and `cove check --deny-warnings` is exactly the
request that it did not.

## Consequences

Programs that used to check and no longer do are the ones leaning on a gap. In
this repository there were four, and all four now say what they meant: a
handler array in `examples/callbacks`, and an empty array, an `Option`, and a
closure parameter in `tests/e2e`. None of their fixtures changed, because none
of their behaviour did.

The cost of the payload is that `Ty::Unknown` is no longer a unit variant, so
every pattern that matched it names the kind or ignores it. That is twenty
lines in one file, and it is what buys the assertion: a classification nothing
can read is a comment, and a comment cannot be wrong in a way a test catches.

`Checker::host_schema` is the single seam an embedder-supplied schema has to
reach. It takes `&self` and answers with an owned `ModuleSchema` rather than a
`&'static` one, because a table an embedder registers is owned by the
compilation and has no `'static` borrow to hand back. `ModuleSchema` is `Copy`
and its contents are `'static`, so answering by value costs nothing and
everything reached *through* the answer is still `&'static`.

What this ADR does not decide is anything the runtime keeps: mutability,
argument order, a host resource's task-safety, and the rest of what ADR 0004
lists as the interpreter's own. An unknown of any kind is still equal to every
type, so the boundary still checks what the checker abstained about.
