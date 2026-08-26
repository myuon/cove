# ADR 0014: Capability analysis for higher-order calls

- Status: Accepted
- Date: 2026-08-26
- Amends: [ADR 0001](0001-mvp-language-design.md) and the Language Card, whose
  "the compiler reports which capabilities each function requires from its
  call graph" gains the qualifier it always needed: what it reports is a lower
  bound, and a function whose call graph cannot be followed to the end says so
- Implemented by: the change that closed
  [issue #73](https://github.com/myuon/cove/issues/73)
- Implementation status: complete for the indirect forms the language can
  write today; "What this deliberately leaves out" names the one case that is
  still handled by the pre-existing receiver over-approximation rather than by
  a marker

## Context

Cove code has no ambient authority. A Host API call needs a capability, and
the host grants capabilities at the execution boundary, so the question "what
will this function ask the host for?" has an answer worth deriving. Resolution
derives it: it walks each body for direct Host API calls and takes a fixed
point over the package's call graph, and `cove outline`, `cove api`,
`cove test`, and `cove impact` all report the result.

That derivation is exact for a direct call and says nothing at all for an
indirect one:

```cove
fn run(work: fn() -> Unit) {
  work()
}
```

`run` requires whatever `work` requires, and nothing in `run`'s signature or
body says what that is. Function types deliberately carry no latent capability
set in the MVP, so no amount of call-graph work recovers it.

This is not an edge case. Closures, `dyn Trait` receivers, bounded generic
parameters, callbacks stored in data, callbacks the host will invoke, and
higher-order functions crossing a module boundary are all the same shape, and
`examples/callbacks` and `examples/traits` are both built out of them.

Runtime enforcement was never in doubt: `HostRegistry` checks a grant before
every call and refuses one it was not given, whatever the compiler decided.
What was in doubt is what the *reports* mean. A `requires console, http` line
with nothing beside it reads as the whole list, and for a function containing
an indirect call it is not.

Three answers were available. Report a lower bound of the statically visible
requirements; report a conservative upper bound; or report an exact set under
restrictions that make the indirect forms unwritable. The second needs a
whole-program closure over every function value in the package, and would name
capabilities most call sites cannot reach. The third would remove language
features the representative programs already use.

## Decision

**A derived capability set is a lower bound, and a declaration that reaches a
call the compiler cannot follow is marked capability-open.**

No effect system, no latent capability set in a function type, and no
capability summary attached to a closure value. Those remain future work, to
be forced by a representative program rather than added in advance.

### The guarantee

For a declaration that is **not** capability-open: the derived set is every
capability that calling it can reach. Nothing it does will ask the host for
anything the set does not name.

For a declaration that **is** capability-open: the derived set is a floor.
Every capability in it is genuinely reachable, and calling it may reach
capabilities the set does not name, through a call whose target the compiler
could not identify.

In neither case does the derived set decide anything. The runtime's grant
check is the only thing that does, and it refuses a call the run was not
granted whether or not the compiler saw it coming.

### Where a capability is charged

A lambda's body is analysed as part of the body that *writes* it. A closure
that prints is charged `console` to the function containing the `fn(...) {
... }`, not to whatever eventually calls it.

That single rule is why the lower bound is useful rather than empty. In a
program that builds a callback and passes it down, the entry that runs the
callback already reaches the function that built it, so the fixed point
carries the capability up to the entry anyway. The floor only falls short when
a value arrives from somewhere the entry's call graph does not lead.

For the same reason, a bare name read as a value — `handler: health` in a
route table the host will later invoke — records a call-graph edge to the
declaration it names. The call happens somewhere Cove cannot see, but the
callback was named here, so what it requires is reported here.

### What each indirect form records

| Form | What analysis records |
| --- | --- |
| `work()`, where `work` is a parameter, a local, or an element taken out of a collection | no edge; `FunctionValue` |
| `handlers.get(0)()` — a callee that is an expression rather than a name | no edge; `FunctionValue` |
| `fn(...) { ... }` written inline | the lambda's body is analysed as part of the enclosing body; no openness |
| a named function read as a value (`handler: health`) | an exact edge to that declaration; no openness |
| `entry.summarize()`, where `entry` is `dyn Trait` | every same-named method reachable through imports; `DynamicDispatch` |
| `entry.summarize()`, where `entry` is a bounded generic parameter | the same; `DynamicDispatch` |
| a Host API call that will call a Cove closure back (`clock.every(60s, fn() { ... })`) | the host module's capability, plus whatever the closure's own body needs, charged here |
| a call to a capability-open declaration | `ReachedOpenCall` |
| `receiver.method()`, receiver type not written at the call site | every same-named method reachable through imports (pre-existing over-approximation); no openness by itself |

A value is treated as `dyn`- or generic-typed when this declaration's own
signature says so, at any depth — `entries: Array<dyn Summary>` counts — and
that follows through the `let`, `var`, and `for` bindings taken from it. The
tracking is a mention rather than a type, because there is no type information
in resolution to ask.

Openness travels the same edges the capabilities do, in the same fixed point:
a declaration that calls a capability-open one is capability-open too, since
the requirement its callee could not see is one it cannot see either. Both
facts have to move together, or a report could show a complete-looking set
assembled out of an incomplete one.

### What the tools show

One derived fact, one vocabulary, everywhere a capability set is printed.

- `cove outline` prints `capability-open: <reasons>` under the `requires`
  line, so the two are never read as one.
- `cove api snapshot` records a bare `capability-open` line, covered by the
  interface hash. `cove api diff` calls gaining it **breaking** — a caller's
  host may no longer grant enough — and losing it compatible, exactly as it
  treats gaining and losing a capability.
- `cove impact` marks a capability-open target and explains, once, that its
  `requires` line is a floor and an entry above it may still be refused.
- `cove test` grants what the call graph derived and does not widen it. When
  the boundary refuses a call in a capability-open test, the failure says the
  derived set was a floor and which indirect form was in the way.
- `cove run` and `cove generate` add the same note to a refusal at the Host
  boundary when the entry is capability-open.

## Consequences

A `requires` line now means one of two things and says which, which is the
whole point: an incomplete list can no longer be mistaken for a complete one.

Four of the twenty exported functions and methods in `examples/` are
capability-open, and every one of them genuinely is: `callbacks.main` runs
handlers through a router, `traits.report` and `traits.headline` dispatch
through `dyn Summary` and through a bound, and `traits.main` calls both. The marker is a signal rather than noise, because
ordinary calls — a declaration, a struct initializer, a host item, a free
builtin such as `Ok` or `assert`, a builtin type used as a namespace — are
all ruled out by name before a bare call is called indirect.

Recording an edge for a named function read as a value strengthens the lower
bound and widens `cove impact`'s notion of a caller slightly: a declaration
that stores `health` in a route table now shows up as reaching it. That is the
honest relationship, since storing it is what makes it run.

A capability-open test can fail at the boundary for a capability the runner
had no way to derive. That is a real cost, and the alternative — granting a
capability-open test everything the registry provides — would make the
runner's grants stop meaning anything. The failure explains itself instead.

## What this deliberately leaves out

**No latent capability sets in function types.** `fn() -> Unit` says nothing
about authority, and adding `fn() -> Unit requires console` would put an
effect system into every signature that has no use for one. Issue #73 is
explicit that this waits for a representative program that demands it.

**No capability summary on a closure value.** Carrying a set at runtime beside
each closure would make some of this exact, and it would also make a closure's
representation depend on what its body reaches. It is future work.

**A `dyn` value whose type this declaration never writes** — one returned by
a call it makes, say, and then dispatched on — is not marked. It falls back to
the receiver over-approximation resolution already applies to any receiver
whose type is not written at the call site: every same-named method reachable
through imports. That misses an implementor only in a module the calling
module cannot reach, which requires the conformance to live downstream of the
call. Closing it properly needs the static type checker's results in
resolution, which is a larger change than this decision is worth.

**No static grant checking.** Nothing here refuses to run a program because
its derived set exceeds `[run.<name>] allow`. A lower bound is the wrong thing
to build a refusal on, and the boundary already refuses the call itself, at
the moment it happens, naming the capability and the run.
