# ADR 0032: A closure's parameter list is fixed

- Status: Accepted
- Date: 2026-08-31
- Records: [issue #168](https://github.com/myuon/cove/issues/168), which found
  the divergence and deliberately left the language question open
- Implemented by: [PR #217](https://github.com/myuon/cove/pull/217), which put
  `cove::type::variadic_lambda` in `Checker::lambda` and deleted the VM
  lowering's refusal of the same shape.
  [PR #167](https://github.com/myuon/cove/pull/167) is the stopgap it replaced
- Supersedes: **nothing.** This ADR applies
  [ADR 0016](0016-four-kinds-of-unknown.md) to a second construct rather than
  replacing any part of it, and it takes up a choice
  [ADR 0019](0019-executable-ir-and-vm.md) leaves to whoever decides a
  construct rather than closing one. "What this supersedes, and why it is
  nothing" goes through both

## Context

### A live divergence, and the first genuine one

```cove
let g = fn(items: Int...) { "{items}" }
println("{g(1)}")?
```

On `main` before [PR #167](https://github.com/myuon/cove/pull/167), `cove
check` reported nothing and the two backends printed different things:
`[1]` on the interpreter, `1` on the VM.

Each was right by its own lights. The checker typed `items` as its **element**
type and silently dropped the `...`, which is why `g(1)` checked at all and
why the VM bound `1`. `Interpreter::bind_params` wrapped the argument in an
`Array` as it does for any variadic slot — one function serves a declaration
and a lambda alike — which is why the interpreter bound `[1]`, and which
matches what `docs/LANGUAGE_REFERENCE.md` says a variadic parameter binds
inside its callee. Nothing in either backend was a bug against a rule this
project had written down. The rule had not been written down.

### Nothing compared it, because nothing wrote it

The differential harness never saw this, and that is worth recording on its
own. No corpus case writes a variadic lambda parameter, and a construct
nothing writes is a construct nothing compares. The harness's headline number
— 97 of 129 cases lowering and agreeing — is evidence about the constructs the
corpus contains and about nothing else. Issue #168 found this by writing the
program by hand.

### #167 refused it in one backend, which made it unreachable

[PR #167](https://github.com/myuon/cove/pull/167) refused a closure's variadic
parameter in `Lowering::lambda_function`, beside the `var` parameter and the
default it already refused there. That is strictly narrowing the VM, which
[ADR 0019](0019-executable-ir-and-vm.md) permits — a construct the IR does not
cover is named as unsupported at lowering time, and a backend may be less
capable and never more permissive. It was the right thing to do with a
disagreement nobody had decided, and it decided nothing: the two backends
still held different answers, and one of them had merely stopped being asked.

## Decision

**A closure's parameter list is fixed.** A variadic parameter is written on a
declaration. It cannot be written on a function value, at any position, and
`cove check` refuses one with `cove::type::variadic_lambda`:

```
error[cove::type::variadic_lambda]: parameter `items` is variadic, so it cannot be written on a function value
  rule: A variadic parameter is written on a declaration: a function value has exactly the parameters its function type names, and a function type names a fixed list of them.
  help: remove the `...` and give `items` an `Array` type, passing one at the call; or declare an `fn`, which a call reaches by name and can gather arguments for
```

The reasoning is [ADR 0016](0016-four-kinds-of-unknown.md)'s. A function type
in Cove names a fixed list of parameters, and a function value has exactly the
parameters its type names — which is the rule ADR 0016 already states from the
other side, where a *variadic host operation* used as a value cannot be given
an `fn` type because there is none to write, and gets
`cove::type::variadic_as_value`. A `...` on a lambda asks for the same thing
from the same direction: how many arguments a variadic parameter gathers is
decided at the call, and a call through a value has no declaration in reach to
gather against. The parameter list is a run-time fact, and a function type
cannot name one.

**It is decided in the checker, not in a backend.** The refusal reaches both
backends and produces one diagnostic rather than one backend's silence, so the
lowering's refusal became a rule stated twice and is deleted. A declared `fn`
is untouched: its variadic parameter still gathers, and is still an immutable
`Array<T>` inside the body.

## Why this is an ADR and not a comment at the refusal

[ADR 0021](0021-places-are-a-static-fact.md) licenses the checker to decide
any fact the source settles through the structure the pass already walks, and
two of this project's variadic diagnostics sit squarely inside that licence: a
non-last variadic parameter and a variadic parameter with a default had no
meaning at all, so refusing them **recorded** a fact rather than removing one.

This is not that. The interpreter — which
[ADR 0012](0012-performance-gate-and-native-backend.md) ranks above any
backend — gave this shape an answer, `[1]`, consistent with what the language
reference says a variadic parameter binds. Refusing it takes a program the
oracle ran and makes it an error. That is **narrowing the language**, not
settling something the language already implied, and issue #168 says why it
wants an ADR: the argument through ADR 0016 is a good one, "but that is an
argument rather than a decision, and it is a language change that wants
stating rather than inferring."

## What this supersedes, and why it is nothing

**ADR 0016 is applied, not replaced.** Its decision "A host operation is a
value" says a variadic host operation is the one operation that cannot become
one, "because Cove has no variadic `fn` type to write", and calls that the
language's own gap. This ADR reaches the same conclusion about the same gap
from the lambda side. Nothing ADR 0016 decided is contradicted: the note it
put on a variadic host operation used as a value is unchanged, in code and in
kind, and the four kinds of unknown are untouched. A lambda whose variadic
parameter is refused binds a `Recovery` unknown, which is that ADR's mechanism
working exactly as it decided.

**ADR 0019 is not narrowed either.** It permits a backend to refuse a
construct; it does not require a refusal to stay in a backend once the
language has decided the construct, and its own account of the refusal list as
"the roadmap" assumes refusals leave it. #167's stopgap was legitimate under
ADR 0019 and its deletion is legitimate under ADR 0019. Neither is a decision
this ADR replaces.

So neither header gains a `Superseded in part by` line, and both are referred
to from here one way.

## The cost

**This removes programs the oracle ran.** `let g = fn(items: Int...) { ... }`
executed on the interpreter, and the answer it gave was defensible. Anything
that wrote one now fails `cove check`, and no migration is automatic.

What such a program writes instead is what the diagnostic's own help says: an
`Array` parameter with the array built at the call, or a declared `fn`, which
a call reaches by name and can therefore gather for. Both are available today
and neither is a workaround for something the language means to allow later.

In this repository the cost was zero programs, because none existed — which is
the same fact as the harness never having compared the construct, seen from
the other end. That is not evidence that nobody outside it wrote one; it is
evidence that this project has no example of wanting to.

## What is not decided

**Whether a function type could ever carry variadicity.** The other reading is
still open: teach a function type to name a variadic tail, so that a call
through a value gathers. That needs ADR 0016's account of what a function type
names revisited, and it would supersede this ADR's decision as well. Nothing
here obstructs it — no representation is fixed, no syntax is spent, and the
refusal is one branch in `Checker::lambda`. This ADR chooses the smaller of
the two languages because it is what the rest of the language already says,
and it does not claim the larger one was refused.

**What the interpreter's variadic binding does for a declaration.**
`Interpreter::bind_params`'s variadic branch is deliberately unchanged. It is
a declaration's branch; it served a lambda only because one function serves
both, and deleting it to reflect that no lambda can reach it would delete the
rule the oracle exists to state.

## Consequences

- `cove::type::variadic_lambda` is a check-time error on both backends, and
  `cove check` is the only place that says so.
- `Lowering::lambda_function` no longer refuses the shape; its doc records what
  went and why, and `REGISTERED_REFUSALS` loses nothing because the corpus
  never contained a case that reached it. `LOWERED_FLOOR` stays at 97.
- The differential harness gains no case that lowers.
  `tests/e2e/fail_variadic_lambda` joins the cases that do not check, which
  is the right group for a program that is now an error, and the conformance
  suite's `REJECTIONS` pins the code under `Lambdas`.
- The harness's own number is a claim about its corpus. This ADR does not fix
  that, and issue #168's observation stands as the record of it: the next
  divergence of this kind will also be found by writing the program, not by
  running the corpus.
