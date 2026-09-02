# ADR 0038: A type nothing settles is not a program

- Status: Accepted
- Date: 2026-09-03
- Supersedes: [ADR 0016](0016-four-kinds-of-unknown.md)'s decision that an
  unconstrained unknown is a *warning* — "warnings rather than errors,
  because the value is still usable" — and its placement of a schema's
  `HostType::Any` among the unknowns. It also closes the second of the two
  silences ADR 0016 names under "What a clean check guarantees": what `Ok(1)`
  alone means. The rest of ADR 0016 stands, including the four kinds, the
  rule that every kind compares equal to every type, the silence of a dynamic
  boundary, and that a schema's `Any` is a *note* and never a warning
- Supersedes in part: [ADR 0036](0036-an-inference-variable-is-not-a-kind-of-unknown.md)'s
  property 2, which says a variable nothing settles is reported "as
  `Unconstrained` is reported — a warning naming the binding". It is still
  reported as `Unconstrained` is reported; `Unconstrained` is an error now.
  Everything else in ADR 0036 stands, and its central claim — that an
  inference variable never escapes the body that minted it — is what this
  ADR generalises
- Decides: what a consumer of `cove_sema::Facts` is allowed to be handed

## Context

ADR 0016 classified the checker's silences and decided what each one costs a
reader of `cove check`. It did not decide what they cost a *backend*, because
at the time there was one backend and it was a tree walker: an unknown
reached the interpreter as a value that already knew what it was, so a type
the checker never settled cost nothing at run time. That is the premise
behind "the value is still usable, the operations that do not depend on the
missing type are still checked".

The linear-memory backend
([ADR 0034](0034-one-physical-word-stack.md)) removes that premise. A value
is placed in a slot of a width its type decides, so a `Vector<_>` is not a
value with one unchecked operation on it — it is a value with no location. A
backend handed one has exactly two moves: refuse the program, or carry a
run-time type with every value and dispatch on it. The second is a different
language, and it is the one this ADR exists to make unavailable.

Reading the corpus is what showed how much was actually being carried. Over
every package in the repository, sliced by module, a walk of `Facts::ty`
found eighty-four expressions in packages that checked without error whose
type held an unknown. Three of them were the families anyone would have
guessed. The rest were not:

- a schema's `Any` and a type parameter nobody settled were the *same value*,
  so the lowering could not tell "the host promised nothing depends on this
  type" from "nobody said". It asked the schema again at every position a
  value is produced, because the type it had been handed could not answer;
- `found.push(item)` settled what `found` holds, and
  `found.freeze().sorted(by: fn(a, b) { ... })` still bound `a` and `b` to a
  hole — the inference variable was opaque for the whole body even after a
  use had said what it was — so a field read off `a` was a *recovery* unknown
  in a package with no error in it;
- a field read off a value from a host module no schema describes came back
  as a recovery unknown rather than as the dynamic boundary it was, so a
  reader downstream was told "an error was reported about this" when none
  had been.

## Decision

**An expression a checked package records a type for holds no `Ty::Unknown`,
excepting a dynamic boundary.** `crates/cove-sema/tests/settled.rs` is the
statement of it, asked of every package in the repository and of fixtures for
each shape. A backend reading `Facts::ty` therefore never has to decide what
to do about a type the checker did not know, and is never in the position
where a dynamic dispatch would be the answer.

The exception is `Unknown::DynamicBoundary`, and it is the only one. It
stands for a host module *this build* ships no schema for, which is a fact
about the build and not about the program: no edit to `sensors.read` fixes
it, and the remedy is `Compiler::with_host_schema`. ADR 0016 puts the one
diagnostic at the `use` for that reason and this changes nothing about it. A
program that reaches such a host cannot be lowered, and that is honest — what
is missing is a description this compilation was never given.

Four decisions follow, and each of them is what makes the invariant true
somewhere it was not.

**A schema's `Any` is a type.** `Ty::Any`, not `Ty::Unknown(Unconstrained)`.
It is a *promise* — "a value of some type, and nothing here depends on which"
— where an unknown is an absence, and a reader that cannot tell them apart
cannot act on either. It compares equal to every type and every operation on
it abstains, which is exactly what the unconstrained unknown standing there
used to do, so no diagnostic moves: the notes ADR 0016 put on an `Any` result
and an `Any` field are the same notes, and a lambda given to an `Any`
parameter is asked to state nothing, as before.

It is not a `dyn Trait`, and the reason is worth writing down because
`docs/LINEAR_VM.md` names one erased *representation* for both and the
lowering already spells a schema's `Any` as `Ty::Dyn("Any")` on its own side.
A representation shared is `cove_lir`'s `Shapes` to decide; that is what
`Shapes` is. As types the two are opposites: a `dyn Display` accepts only a
conforming value and answers only that trait's methods, an `Any` accepts
every value and answers everything at run time. Writing one as the other
would put an exception on every `Ty::Dyn` in the checker and a reserved trait
name in the language, to save one line in a table whose whole job is to map
several types onto one representation.

**An unconstrained unknown is an error.** `cove::type::unconstrained` was a
warning on the reading quoted above, and the reading was the interpreter's.
An empty array literal, a bare `None`, an unannotated lambda parameter
nothing expects a type of, a struct's type parameter no field mentions, and a
binding whose uses settle nothing are one fact and are all refused. Writing
the type is still always available, which is what every one of those `help`s
says; what changed is that leaving it unwritten is no longer a program.

Reporting it stays where ADR 0016 and ADR 0036 put it, and gains one place: a
variable that no binding took — the result of a call written in the middle of
an expression — is asked at the call, because there is no name to ask about.
And a body that already has an error in it is asked for no annotations at
all: everything this reports is "the program did not say", and after a
mistake the checker cannot tell that from "the mistake stopped it being
read". That is ADR 0016's own recovery rule, one mistake and one diagnostic,
applied to the end of a body.

**`Ok(1)` alone means nothing until something says what the failure type is.**
This is ADR 0016's open question, and the answer is a refusal rather than a
default. `Result` is generic in both parameters and this repository writes
`Result<Int, ParseError>`, so defaulting the failure type to `Error` would be
a guess dressed as a rule. What the checker gains instead is the ability to
*settle* it far more often than it could: a free builtin's type parameters
are inference variables now rather than unknowns minted on the spot, so a
later use fills them; `?` says the failure type is the enclosing function's;
and leaving a `scope` says the same about a child nothing awaited. `Ok(5)?`
inside `-> Result<Int, String>` is settled by the `?`. What is left after all
of that is a program that genuinely stated nothing, and it is asked to.

**A type that contains itself is `cove::type::recursive_type`.** `v.push(v)`
asks for `μX. Vector<X>`: regular, finitely representable, and with no
surface syntax, because recursion in Cove is nominal and this inference is
structural. The constraint is detected exactly — there is no occurs check and
none is needed, since a variable is never bound to a type holding one — and
then it cannot be kept. ADR 0036's `TyVar::spoken_for` carried it silently,
which made it the one remaining way a check that reported nothing could hand
a backend a `Vector<_>`. The diagnostic points at the use and names the
declaration that writes the type instead.

**The abstention keeps its name as it spreads.** A field read off a value
from an unschema'd host, a `?` applied to one, a call made through one: each
is the *same* unproved boundary, so each produces a `DynamicBoundary` and not
a `Recovery`. Without this the one diagnostic at the `use` is not enough,
because one step downstream the value claims an error was reported about it.

## Consequences

Programs that used to check and no longer do are the ones leaning on a gap,
and in this repository there were six. Five are `v.push(v)` in
`tests/e2e/gc_{capture,cycles,frames,place,reentry}`, each of which now writes
the cycle through a declared type — the form `gc_cycles` already shipped. The
sixth is `let ok = Ok(1)` in `tests/e2e/type_result`, which now writes
`Result<Int, Error>`.

**One shape of value became unwritable, and it is worth naming.** A single
object that reaches itself — a vector holding the handle that names it — is
allowed by the value model and has no type that can be written. `gc_cycles`
existed to build a cycle of every shape the value model allows, and it now
builds every shape the model allows *and the language can write*: through a
struct field, and through a closure capture. A collector that reclaimed those
two and not a one-object self-reference is a collector that case no longer
catches, and the shape is still reachable from a host that builds a value
directly.

Every corpus program that leans on `clock.timeout` or `clock.every` now
carries `Ty::Any` where it carried an unknown, which is the largest single
thing the invariant needed and the one that removes the reason
`cove_lir::lower::shapes`'s `host_ty` is written twice. `Shapes::of` has to
learn `Ty::Any`; until it does, such a value is a lowering gap exactly as an
unknown was.

The checker now resolves an inference variable at every place a name is
*bound* from a type — a lambda parameter, a `for` binding, a pattern binding
— and deliberately not where a found type is *compared* with an expected one,
because that comparison is where a use says what a variable is and where two
uses disagreeing is `cove::type::inference_conflict` rather than an ordinary
mismatch.

## What this does not decide

- whether the diagnostic belongs at the `use` for a host no schema describes,
  which is ADR 0016's and unchanged;
- whether an inference variable should be what an empty array literal, a bare
  `None`, an unannotated lambda parameter or a struct parameter no field
  mentions defers to, which ADR 0036 left open and which this does not touch:
  all four still report where they are written;
- whether variable-to-variable unification is ever added, which would be what
  a self-referential type would need before it could be *represented*, let
  alone written.
