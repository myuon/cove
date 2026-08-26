# ADR 0015: Capability analysis for higher-order calls

- Status: Accepted
- Date: 2026-08-26
- Supersedes: [ADR 0001](0001-mvp-language-design.md)'s account of derived
  capability requirements. ADR 0001 has the compiler report "which
  capabilities each function requires from its call graph", which reads as the
  whole list; this ADR decides instead that what it reports is a lower bound,
  and that a function whose call graph cannot be followed to the end says so.
  The Language Card's wording follows this ADR rather than that one.
- Implemented by: the change that closed
  [issue #73](https://github.com/myuon/cove/issues/73)
- Implementation status: complete for the indirect forms the language can
  write today; "What this deliberately leaves out" names the cases still
  handled by the pre-existing receiver over-approximation rather than by a
  marker

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

For a declaration that is **not** capability-open: the derived set is a sound
upper bound. It is complete — nothing the declaration does can ask the host
for a capability the set does not name — but it need not be minimal. Three
things this decision records can put a capability in the set that a given
call never reaches: a receiver whose type is not written at the call site
resolves to every same-named method reachable through imports, and that
pre-existing over-approximation can produce an edge that exists in no real
execution; a lambda is charged to the body that *writes* it rather than the
body that *runs* it, so a closure built but never called still contributes
its capabilities to the function that wrote it; and a bare name read as a
value records an edge to the declaration it names whether or not the value is
ever called through. The set can therefore be wider than any one execution
needs, even though it is never narrower than what the code as written could
ask for.

For a declaration that **is** capability-open: neither bound is tight. The
set is not complete, because the call the compiler could not follow — the
reason for the marker — may reach a capability named nowhere in it. And it is
not minimal either, for the same three reasons above: an approximate edge, a
closure charged at its write site, or a named function read as a value can
all still be sitting in the set without anything ever running them. Only the
openness marker itself is load-bearing: it says a call escaped analysis, not
how the set beyond it relates to what actually runs.

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
| a local `fn` declared inside a body | its body is analysed as part of the enclosing body, exactly as a lambda's is; calling it by name is an ordinary call, and no openness |
| a name the body itself binds — a parameter, a local, a `for` binding — read or called | nothing, whatever a module happens to declare under the same name |
| `entry.summarize()`, where `entry` is `dyn Trait` | every same-named method reachable through imports; `DynamicDispatch` |
| `entry.summarize()`, where `entry` is a bounded generic parameter | the same; `DynamicDispatch` |
| `self.item.summarize()`, where `item` is a field declared `dyn Trait` | the same; `DynamicDispatch` |
| `items.length()`, where `items` is `Array<T>` or `Array<dyn Trait>` | an ordinary call on an ordinary `Array`; no openness |
| a Host API call that will call a Cove closure back (`clock.every(60s, fn() { ... })`) | the host module's capability, plus whatever the closure's own body needs, charged here |
| a call to a capability-open declaration | `ReachedOpenCall` |
| `receiver.method()`, receiver type not written at the call site | every same-named method reachable through imports (pre-existing over-approximation); no openness by itself |

A value is treated as one whose implementation its producer chose when a
written type says so: a parameter or a lambda parameter of this declaration,
or a *field* of any struct the package declares. The field case is not a
refinement but the common one — `struct Box { item: dyn Summary }` writes the
type once, and every method of `Box` then dispatches through `self.item`
without naming it again — and leaving it out was the difference between a
lower bound and a wrong one.

Depth is read, but it is not the same fact at every depth. `entries:
Array<dyn Summary>` binds an *`Array`*, so `entries.length()` is
`Array.length`, a builtin with no conformance to pick; what comes out of
`entries` is the `dyn Summary`. The two classes are tracked apart, which is
why iterating `entries` gives a `dyn` binding while calling a method on
`entries` itself does not mark anything.

That classification follows through a `let`, `var`, or `for` binding whose
initialiser *mentions* an already-classified name, or that carries a written
type of its own. It is a mention rather than a type, because there is no type
information in resolution to ask — which also fixes its limit: an
initialiser whose opacity lives only in its type, `let entry = makeDyn()`,
binds a `dyn Trait` this body never writes, and is not tracked. "What this
deliberately leaves out" says what happens instead.

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

Four of the twenty-two exported functions and methods in `examples/` are
capability-open, and every one of them genuinely is: `callbacks.main` runs
handlers through a router, `traits.report` and `traits.headline` dispatch
through `dyn Summary` and through a bound, and `traits.main` calls both. The
marker is a signal rather than noise, and staying that way is work: ordinary
calls — a
declaration, a struct initializer, a host item, a free builtin such as `Ok` or
`assert`, a builtin type used as a namespace — are ruled out by name before a
bare call is called indirect; a name the body itself binds is ruled out before
it can shadow one; a local `fn` is analysed rather than guessed at; and a
container of opaque values is distinguished from the values themselves, so
`items.length()` stays an ordinary call.

`traits.headline<T: Summary>` is the marker's honest cost. Its bound is
checked at the call site, which is also where `T` is chosen, so `summarize`
resolves to exactly one implementation per call — and `traits.main` passes a
concrete `Booking` and a concrete `Receipt`. The analysis marks it open
anyway, and poisons `traits.main` through `ReachedOpenCall`, so three of the
four markers come from dispatch a monomorphising compiler could follow.
Following it needs the type checker's instantiation of `T` inside resolution,
which is the same missing ingredient as everything under "What this
deliberately leaves out"; until that exists, marking is the answer that cannot
be wrong in the dangerous direction.

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

**A `dyn` value whose type nothing written down gives away.** Resolution
classifies a value from a written type — a parameter's, a lambda parameter's,
a struct field's — or from a binding taken from one of those. A value whose
opacity is only in a type resolution never reads is not classified, and a
method called on it is not marked: `makeDyn().summarize()`, and
`let entry = makeDyn()` followed by `entry.summarize()`, are both unmarked,
because answering what `makeDyn` returns means having the type checker's
results inside resolution. Such a call falls back to the receiver
over-approximation resolution already applies to any receiver whose type is
not written at the call site: every same-named method reachable through
imports. That misses an implementor only in a module the calling module cannot
reach — which does happen: it is exactly the shape of a plugin whose
conformance lives downstream of the code that dispatches. Closing it properly
is a larger change than this decision is worth, but it is a real gap and not a
theoretical one.

**A field's type is matched by name, not by the struct that declares it.**
Because resolution cannot say what `holder.item` is a field *of*, the set of
opaque field names is collected across the whole package. An unrelated
struct's field of the same name therefore reads as opaque too, and a method
called on it is reported as dispatching dynamically when it does not. That
direction was chosen deliberately: naming a capability-open declaration that
is not one costs a reader a second look, and missing one costs them the
guarantee.

**Bounds on `impl` blocks.** The walk is handed a function declaration and
reads that declaration's own generic parameters. An `impl` block's generics
are never consulted. The parser rejects `impl<T: Summary> Cell<T>` today, so
there is nothing to miss; the moment bounds on `impl` blocks land, there is.

**No static grant checking.** Nothing here refuses to run a program because
its derived set exceeds `[run.<name>] allow`. A lower bound is the wrong thing
to build a refusal on, and the boundary already refuses the call itself, at
the moment it happens, naming the capability and the run.
