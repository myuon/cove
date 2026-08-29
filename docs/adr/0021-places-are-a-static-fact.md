# ADR 0021: Places are a static fact, and `cove check` decides them

- Status: Accepted
- Date: 2026-08-29
- Supersedes: [ADR 0004](0004-static-type-checking.md)'s Consequences
  decision that "The interpreter's dynamic checks stay", for the checks that
  state a language rule rather than a runtime invariant. ADR 0004's
  reasoning — that they "cover what the checker deliberately cannot see" —
  is what stops holding once the checker can see them, and its prescription
  that such a check "becomes a broken-invariant guard" is right only where
  deleting it would leave an evaluator guessing. Everything else in ADR 0004
  stands, including that a clean check guarantees what it says it guarantees
  and that the boundary rules the runtime alone can see are the runtime's
- Implemented by: [PR #135](https://github.com/myuon/cove/pull/135), closing
  [issue #125](https://github.com/myuon/cove/issues/125) and with it
  [#112](https://github.com/myuon/cove/issues/112) and
  [#113](https://github.com/myuon/cove/issues/113)
- Implementation status: complete for the six rows of the table below. One
  member of the same class is deliberately left where it was, and is named
  under "What is not decided here"

## Context

Five constructs were accepted by `cove check`, refused by the interpreter at
run time, and refused by `cove_ir::lower` before the VM was handed anything:

| construct | what the oracle says, when the statement runs |
| --- | --- |
| assignment to a read-only place | ``cannot assign to `x`, which is a read-only place`` |
| a `var` argument that is a read-only place | ``\`total\` is a read-only place, so it cannot be passed as \`var\`` |
| a `var` argument that is not a place | ``this expression is not a place, so it cannot be assigned or aliased`` |
| a mutating receiver that is a read-only place | ``\`push\` takes a \`var self\` receiver, but \`fixed\` is a read-only place`` |
| a mutating receiver that is not a place | ``\`push\` takes a \`var self\` receiver, but \`this expression\` is not a place`` |
| a labelled argument out of declaration order | ``\`two\` was given the label \`a\` out of declaration order`` |

Six rows, and the sixth is the same fact about a call's shape rather than
about a place, so the set is really five place questions and one order
question. The list had grown three times in a week, as the lowering learned
more about places, and it was going to keep growing.

**That `cove_ir::lower` settles every one of them is the proof they are
static facts.** The lowering has no runtime information at all. It reads the
binding a name reaches, whether that binding was written `var`, the shape of
the receiver expression, and the order of the parameter list — and it
answers. A fact a lowering can settle from the source is a fact a checker can
settle from the source.

What was wrong with the arrangement is not that the diagnostics were bad.
They are good, and this ADR keeps them word for word. What was wrong is
*when* they arrive:

- a program that can never be right is discovered by running it, and only if
  the run reaches the line. An assignment in a branch nothing takes was never
  reported at all;
- the two backends refused at different times, visibly.
  `tests/e2e/fail_assign_let` printed `limit 10` on the interpreter and
  nothing at all on the VM, because ADR 0019's rule — a VM run either
  finishes on the VM or fails before any side effect — was working exactly as
  intended on a case it was not written for. Both refusing is right; refusing
  at different times is a divergence, and
  [issue #111](https://github.com/myuon/cove/issues/111) cannot make the VM
  the default while it stands.

The obstacle was stated in the lowering's own doc comment, and it is the
question this ADR exists to answer:

> `cove-sema` catches neither backend's case today — **mutability is not a
> type, so `cove check` does not enforce it** — and whoever moves it there
> deletes this and the interpreter's own refusal both.

That sentence is the whole difficulty. `cove check` was built by
[ADR 0004](0004-static-type-checking.md) as a *type* checker, and mutability
is not a type: `var x: Int` and `let x: Int` have the same type, and
`Interpreter::coerce` has nothing to convert between them. So a rule about
mutability had no home, and the two backends each grew one.

## Decision

**`cove check` is a check over a resolved program, not a type checker.** It
may decide any fact the source settles through the structure it already
walks; it must not decide a fact that needs the run.

The two halves of that are what make it a rule rather than a licence:

- *the structure it already walks* is the scope stack and the declaration a
  call reaches. This pass knows every binding's name, its type, and now
  whether it was written `var`, because it is the pass that brought the name
  into scope. Reading one more field off a binding it declared itself is not
  a new analysis; it is not doing arithmetic on the one it has;
- *a fact that needs the run* stays out, and the boundary rules are the
  example: whether a capability was granted, whether a budget was exhausted,
  whether a value crossing a task boundary is safe at the moment it crosses.
  ADR 0004's account of those is untouched, and so is the reason — the
  checker cannot see them, not that it declines to look.

Mutability is the first kind. `let` creates a read-only place and `var` a
mutable one — the Language Card's first bullet — and which of the two a
given expression names is answered by walking to the expression's root and
asking the binding. There is no fixpoint, no constraint solving, no aliasing
analysis, and no ownership model. It is one field on a binding and a walk
down `ExprKind::Field`.

### A place is defined once, in the checker

`Checker::place_mutability` in `crates/cove-sema/src/typeck.rs` is the
definition:

> An expression is a place exactly when it is a name this body bound, or a
> field of a place. It is writable exactly when the binding at its root was
> written `var` and belongs to the function value being checked.

`Interpreter::resolve_place_opt` and `cove_ir::lower`'s
`Body::place_mutability` were two readings of that rule, one asked of an
environment at run time and one asked of a slot table while lowering. The
first is now a question about placehood alone, and the second is
`Body::is_a_place`, which asks the same. `cove_runtime::interp::Place` no
longer carries a `mutable` flag and `cove_ir`'s `Binding` no longer carries a
`writable` one, because a second statement of a rule is free to drift from
the one that decides.

The second clause of the definition is not decoration. A closure holds a
*copy* of what it captured — `Env::declare_capture` builds a read-only
binding — so a captured `var` is a read-only place inside the closure however
it was declared outside. The checker walks a lambda's body with the scopes
around it still standing, so it has to know where the function value it is
checking begins. It does, and that is the whole of what the second clause is
for.

### The wording is the interpreter's

Every diagnostic this adds says what the interpreter said, in the same words,
with the same rule line and the same help. Nothing is re-invented. Users have
seen these sentences for the whole life of the language, and a change that
moved *when* an error arrives and also *what it says* would be two changes to
review as one.

Three codes carry them:

- `cove::type::read_only_place` — the `let`/`var` rule, wherever it is
  broken: an assignment, a `var` argument, a `var self` receiver. One code
  for one rule and three messages, because what the program was doing differs
  and the fact reported does not, and a reader searching for the rule wants
  all three;
- `cove::type::not_a_place` — already existed for an assignment target, and
  now covers everything written where a place is required;
- `cove::type::label_order` — labels appear in declaration order.

They live under `cove::type::` with the rest of this pass's codes. That is a
naming decision and it is the honest one: the prefix names the pass that
reports, not the kind of fact reported, and inventing `cove::place::` would
say that this pass has two halves when the argument above is that it has one.

### A backend may refuse more than the oracle, and states its own invariants

Deleting a language rule from the oracle and leaving it in a backend would
make the backend more permissive than the oracle, which
[ADR 0019](0019-executable-ir-and-vm.md) forbids. So the two go together, and
they did. But two refusals survive on *both* sides, and they are not language
rules:

- a `var self` receiver, or a `var` argument, that is no place at all.
  Deleting these would not leave either evaluator doing something defined:
  a `var self` method binds the caller's place, and with no place there is
  nothing to bind. `Interpreter::var_self_needs_place` and the lowering's
  `Body::place` are what is left, and both refuse, so neither backend is more
  permissive than the other;
- a labelled argument out of declaration order. The VM's calling convention
  is built on the property that a call whose labels stand in declaration
  order fills its parameters in increasing order, which is what makes pushing
  the arguments left to right the same as pushing them in declaration order.
  `arguments_in_order` is where that property is relied on, so it is where it
  is stated; the interpreter's `assign_labels` keeps the matching one, so the
  two backends stay in step. Neither is reachable by a checked program, and a
  unit test drives the lowering's directly, because that is what an invariant
  is worth stating for.

ADR 0019 is not superseded by any of this. It said a backend may refuse what
the oracle runs and may never run what the oracle refuses, and that is still
true in both directions.

## Consequences

**This is a breaking language change, and the break is the point.** A program
whose bad assignment sits in a branch nothing takes ran to completion
yesterday and does not compile today. A program that printed four lines and
then failed on the fifth now prints nothing, because it never starts. Both
are programs that could never be right, and reporting them before the run is
what the change is for — but a program that ran and now does not is a break,
and calling it anything else would be dishonest.

Three end-to-end cases show exactly that, and each has been rewritten as a
case that does not check:

- `tests/e2e/fail_assign_let` printed `limit 10` and then failed. Its
  `expected.out` is empty now;
- `tests/e2e/fn_labels` printed the four call forms it accepts and then
  failed on the fifth. It still names all five, and now none of them runs;
- `tests/e2e/type_struct` printed five lines of struct initialization and
  field reads and then failed on `Point(y: 20, x: 10)`. `type_struct_copy` is
  the case that still *runs* those forms.

Each moved into a package of its own, which is how this suite already keeps a
case that does not check from stopping the seventy that do.

A fourth case moved for a different reason.
`tests/e2e/backend_unsupported` pins ADR 0019's no-silent-fallback rule, and
it pinned it *using* one of these constructs — so it had to be rewritten a
fourth time, because a program `cove check` refuses never reaches a backend
and can no longer say anything about what a backend does. It names a function
declared inside a function body now: a construct the interpreter runs, the
lowering has no instruction for, and nothing about which is wrong. That is
what "unsupported" is supposed to mean, and the case is better for having to
find one.

The differential corpus is unchanged in size and changed in shape: 118 cases,
89 lowered and agreeing on both backends as before, 4 refused becoming 1, and
25 not checking becoming 28. Zero disagreements before and after. The
refusals went because the checker catches them, not because the VM learned
anything, and `LOWERED_FLOOR` says so where it did not move.

What the runtime lost is smaller than it looks. Five refusals were deleted
from `crates/cove-runtime/src/interp.rs` and four from
`crates/cove-ir/src/lower.rs`, and every one of them was a sentence about a
program being wrong rather than a guard on an invariant. What is left in the
interpreter is what it needs to answer at all.

### What is not decided here

One member of the same class stays at run time, and naming it is part of
being honest about the rest. A `var` marking must agree at both ends: `f(var
x)` needs `f` to declare that parameter `var`, and `f(x)` needs it not to.
The interpreter refuses both directions, the lowering refuses both, and
`cove check` accepts both — so it is in the class by the same test everything
above is.

It is not moved here because the checker cannot yet say it everywhere it
would have to. A function *type* carries no marking, so a call through a
value has nothing to check against; and the parameter lists this pass builds
for a builtin, a host operation and a struct's synthesized initializer are
not written with a marking at all, so a `var` argument to one of them has no
established answer to check against either. Deciding it means deciding what
`Point(var x)` should mean, which is a language question and not this ADR's
to answer. The rule this ADR sets is what will decide it when it is asked:
the marking a call site writes and the marking a declaration writes are both
in the source, so wherever the checker can reach the declaration, the answer
is the checker's.

Until then the runtime check for it stays, on both backends, and the
`docs/LANGUAGE_REFERENCE.md` section "What the run time decides" names it
rather than leaving a reader to find it.

## Alternatives considered

**Reproduce the run-time failure in the VM.** Lower each bad construct to an
instruction that raises the oracle's own error at the same point, sharing the
error constructor the way `interp::enum_case` and `interp::host_enum_case`
are already shared. This is complete agreement between the backends with no
behaviour change at all, and it unblocks issue #111 without breaking a single
program.

It was refused because of what it builds. The IR would grow instructions
whose only purpose is to fail, for constructs the lowering *detected
statically* — a compiler emitting code to discover at run time something it
already knows. The strangeness of that is the argument: if the lowering knows,
something before the lowering should have said so.

**Keep the interpreter's refusals as broken-invariant guards**, which is what
ADR 0004 prescribes for a check the checker makes unreachable. This was
tempting because it costs nothing and deletes nothing. It was refused because
a guard that can never fire is a rule stated twice, and the whole reason
these five were hard to find in the first place is that the rule was stated
in three places and no two of them could see each other. Where deleting a
check would leave an evaluator with nothing defined to do, the guard stays —
and this ADR names those, above, rather than keeping all of them and calling
it caution.

**Point at the declaration.** A checker has the `let` in hand and could show
it as a second label, which the interpreter never could; issue #113 raises
this and leaves it open. It is not done here, because "keep the interpreter's
wording" and "add a label the interpreter never had" pull against each other,
and this change is already asking a reader to review a break in what
`cove check` accepts. The label is a change to a diagnostic and can be made
on its own, against these tests, with nothing else moving.
