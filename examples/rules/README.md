# rules — a review policy, compiled once and invoked many times

A pull-request review policy: six rules behind a trait, an engine that weighs
their findings, and one `decide` that turns a `PullRequest` into a
`ReviewPolicy`. That is the Cove half, and it runs on its own —
`cove run reviewPolicy` prints what the policy makes of six sample pull
requests.

The other half is `host/`, a Rust crate that embeds it. It is the reason this
example exists. Every other program in `examples/` is run by `cove run`,
which compiles a package and runs one entry once; a rule engine is not that
shape. It is compiled once when the application starts and invoked once per
request for as long as the application lives, and nothing in this repository
had measured what that costs.
[Issue #109](https://github.com/myuon/cove/issues/109)'s gate names it —
"#90 measures compile-once/invoke-many embedding and Host conversion on the
VM" — and [Measurements](#measurements) is that measurement.

So the interesting question here is not "can Cove express a rule engine",
which it can and which [Shape](#shape) is the answer to. It is: what does an
application pay to hold one, and where does the payment go? The short answer
is that compiling is worth about 168 invocations, that reusing one backend
instance instead of building one per request saves 167 allocations against the
237 an invocation costs, and that how the pull request gets in is worth 40
allocations and 135 instructions. Those figures are measured on the backend
[ADR 0034](../../docs/adr/0034-one-physical-word-stack.md) has since deleted,
whose names the replacement now carries — see the provenance note at the top
of [Measurements](#measurements) for what that does and does not mean about
the numbers below.

That last number used to be a different number, and how it changed is the
better half of this document. When this example was written, `run_entry` took a
list of strings on both backends and there was no other way in, so a pull
request had to arrive through a Host API call into a host module the embedder
declared for no other reason. What the measurement found and called "the Host
API boundary" was, in large part, the cost of carrying an argument through a
mechanism meant for reaching outside the process. `Vm::invoke` closed that
([issue #150](https://github.com/myuon/cove/issues/150)),
`rules.embedded.evaluate` takes the pull request as an argument, and
[What the way in costs](#what-the-way-in-costs) measures the two ways against
each other and splits the old number in two. The old path is still here and
still measured, because it is the control that makes the split possible.

`host/` is a workspace member, so `cargo test --workspace` runs its twenty-two
cases and `cargo clippy` sees it. An example of an API that is not compiled
against that API is an example that has already rotted.

## Running it

```console
$ cd examples && cove run reviewPolicy
catalog
  size: over 1000 changed lines wants 2 reviewers
  guarded-path: 3 guarded prefixes, waived by `security-reviewed`
  tested: no description
  draft: no description
  branch: no description
  label: 3 weighted labels
decisions
  pr-1001 normal reviewers=0 because=none trail=clean
  pr-1002 require reviewers=2 because=large_change trail=size:required
  pr-1003 block reviewers=0 because=guarded_path:auth/ trail=guarded-path:blocking
  pr-1004 require reviewers=2 because=guarded_path_waived:auth/ trail=guarded-path:required
  pr-1005 normal reviewers=0 because=none trail=draft:advisory
  pr-1006 require reviewers=3 because=label:breaking-change trail=branch:required,label:required
```

The first three lines of the catalog describe themselves and the last three do
not, because `Rule.describes` has a default body and only three of the six
override it. That is the trait's default method doing its job in output rather
than in a doc comment.

### The rules

| rule | asks | answers |
| --- | --- | --- |
| `size` | more than 1000 changed lines? | required, 2 reviewers |
| `guarded-path` | does it touch `infra/`, `auth/` or `billing/`? | blocking, or required with the `security-reviewed` waiver |
| `tested` | no tests, and over 50 lines? | required, 1 reviewer |
| `draft` | still a draft? | advisory |
| `branch` | aimed at `release`? | required, 2 reviewers |
| `label` | what is the heaviest label worth? | required, that many reviewers |

Two decisions about how findings combine are worth stating, because they are
the whole of what `rules.engine` does and neither is obvious.

**A block is not a large number of reviewers.** A blocking finding decides on
its own, whatever else was found, and asks for no reviewers at all. The
alternative — a severity that is a number, so that three advisories add up to
a block — was rejected in `Severity`'s own doc comment: it makes the type
invite arithmetic that means nothing.

**Two requests for reviewers do not add.** The most any one finding asked for
is what the policy asks for, and the reason recorded is that finding's. `pr-1006`
above is the case: the `branch` rule asks for two and the `label` rule asks for
three, and the answer is three `because=label:breaking-change` rather than five
because of a sentence assembled out of both. A reason nobody reads is a reason
that is not there.

## Shape

| module | what it is |
| --- | --- |
| `rules.policy` | `PullRequest`, `Severity`, `Finding`, `Requirement`, `ReviewPolicy`, `Decision` — the types, and the methods that read them |
| `rules.catalog` | the `Rule` trait, the six rules, and `standard()` |
| `rules.engine` | `decide`, `appraise`, `findings`, `policyFor` |
| `rules.fixtures` | six pull requests, one per arm of the policy |
| `rules.embedded` | the adapter the Rust host invokes: `evaluate`, and the boundary controls `decideRequest` and `pullOnly` |
| `rules` | `main`, and the two measurement controls `decideSample` and `floor` |
| `host/` | the Rust application that embeds all of the above, and the two binaries it ships: `cove-rules-measure` and `cove-rules-check` |

The one signature everything else exists to serve:

```cove
export fn decide(pr: PullRequest) -> Decision
```

and the trait it dispatches through:

```cove
export trait Rule {
  fn name(self) -> String
  fn appraise(self, pr: PullRequest) -> Option<Finding>
  fn describes(self) -> String {
    "{self.name()}: no description"
  }
}
```

`standard()` answers an `Array<dyn Rule>` holding six different struct types,
and `rules.catalog.ask<R: Rule>` is the same call written against a bounded
type parameter instead. Both are here on purpose: they are the language's two
dispatch forms, they are separately lowered, and
`catalog_test.bothDispatchFormsReachTheSameRule` asserts they answer the same
thing rather than assuming it.

## The embedding

Four steps, and the whole of what an application writes. All four are in
`host/src/lib.rs`.

**Declare the module the application is.** `REVIEWS` is one
`cove_schema::ModuleSchema`: two operations, one type of ten fields, and the
capability they are gated on. It is a `const` because this application knows
what it serves; a host that learns its shape from a manifest at run time
assembles one and leaks it once, which
`crates/cove-runtime/tests/embedding.rs` demonstrates and
[issue #86](https://github.com/myuon/cove/issues/86) settled.

**Compile the rule package against it, once.**

```rust
let program = Compiler::new().with_host_schema(REVIEWS).compile(&package)?;
```

That one line is what makes `reviews.pull("req-2")` a checked call rather than
a discovery at the boundary. The same value goes to `HostApi::module_schema`,
so the description the checker read and the description the boundary enforces
are one value and cannot drift.

**Lower the entry, once.** `cove_ir::lower_entry` lowers what one entry can
reach, so an application that invokes two entries lowers twice and holds both.

**Build one backend and invoke it many times.** `RulePackage::serve` builds a
`Runtime` and a `Vm` and hands them to a closure; every invocation the
closure makes is served by that one `Vm`. It takes a closure rather than
answering with a session because a `Vm` borrows the `Runtime` and the
lowered program, and because nothing Cove-shaped could leave it in any case: a
`Value` is `Rc`-based and is not `Send`.

```rust
session.invoke("rules.embedded", "evaluate", vec![pr.to_policy()])
```

That is the whole of a request. `Session::evaluate` is that line plus
`Decision::of` on what came back.

### The two ways in

**`evaluate(pr: PullRequest) -> Decision`**, invoked with a value. The host
builds a `rules.policy.PullRequest` with `PullRequest::to_policy` and hands it
to `Session::invoke`, and reads a `rules.policy.Decision` out of the answer
with `Decision::of`. Nothing crosses the Host API boundary: no host module is
reached, no capability is asked for, and a trace sink watching the boundary
sees nothing at all. This is what an application should write.

**`decideRequest(args: Array<String>) -> Result<Decision, Error>`**, run with a
process argument. The request identifier goes in as the entry's one argument;
the pull request comes back out through `reviews.pull`, as a
`reviews.PullRequest` the Rust side built with `PullRequest::to_cove`; the
decision leaves twice, as the value the entry returned and through
`reviews.record` under the same identifier that started it.

The second was once the only one, because `run_entry` takes a `Vec<Rc<str>>` on
both backends and a host could say nothing else to a program. It stays for two
reasons. It is the control that makes the Host API boundary measurable — the
two reach the same decision over the same pull request, so what separates them
is the boundary and nothing else, which is what
[issue #109](https://github.com/myuon/cove/issues/109) needed. And it is what a
host module is genuinely for: both `reviews` operations take the request
identifier first, so every `HostCall` event a trace sink sees carries it and a
run's calls group by application request without the runtime knowing what an
application request is.

### What holds an invocation to the rules

`invoke` is checked against the signature the checker resolved, before the
first instruction. `rules.embedded.evaluate` declares one parameter, so an
invocation supplies one value; that value must be a `rules.policy.PullRequest`
carrying the ten fields the declaration lists, in that order, each of the
declared type.

The field order is not pedantry. The lowering spends the checker's answer and
emits `load-field ... +4 ...` where the source wrote `pr.changedLines`, so a
struct a host built with nine fields would have `Vm` read past the end of one
and a struct with ten in another order would answer the wrong field with no
sign of it. `crates/cove-runtime/src/invoke.rs` is where that is refused, and
`an_argument_the_rules_do_not_declare_is_refused_before_anything_runs` is what
holds it to the three ways a host can get it wrong.

A capability is not part of that check, and an invocation grants nothing: what
`evaluate` may reach is what the `HostRegistry` was granted, exactly as for a
run `cove run` started. `evaluate` reaches nothing, which is why the case above
grants it nothing.

### What bounds one request

```rust
session.evaluate_within(Limits { fuel: Some(1_200), ..Limits::default() },
                        "rules.embedded", "evaluate", pr)
```

A rule package is somebody else's code. It can loop, and an application that
runs one would rather be told which request went wrong than stop serving. So
the ordinary thing to want is a limit on a *request*, and until
[issue #152](https://github.com/myuon/cove/issues/152) an embedding could not
ask for one: a `Budget` belonged to the `HostRegistry`, `set_budget` needs
`&mut`, and a backend holds the registry by shared reference for as long as it
exists. Every limit was therefore spent over the whole session — the fuel for
the first decision came out of the same pot as the fuel for the ten-thousandth
— and the only way to get a per-request bound was to build a registry, a
`Runtime` and a backend per request, which is 167 allocations of table
rebuilding against a request's own 237 and is the thing compiling once was for
not doing.

**A budget belongs to an invocation.** `invoke_within` and `run_entry_within`
take one, install it as the call is entered, and leave it behind holding what
that invocation spent. It still lives on the registry, because ADR 0008 draws
a spawned task's fuel from the run's budget and a task thread reaches it
through the `Runtime` it carries; a task's charges are still its request's.
The deadline runs from the moment the invocation starts rather than from
wherever the `Limits` were written, which is what makes a per-request deadline
mean the request.

`embedding(reviews, grants, limits)` still installs a session budget, and that
is still right for the limits that are about the process. The two are
different questions and the example asks both:
`a_budget_on_the_registry_is_spent_over_every_invocation` is the session, and
`fuel_handed_to_one_invocation_bounds_that_invocation_alone` is the request —
the same three decisions, differing in one call, where the session runs out on
the second and the requests each answer.

Which limit to reach for is ADR 0024's and ADR 0030's, not this example's.
**`max_host_calls` is the control that bounds effects exactly**; fuel bounds
work, and how many effects a fuel limit admits still depends on what the
program does between them. ADR 0030 settles only the far end of that — no Host
call begins once the fuel a run has been charged has reached its limit — and
leaves the near end where ADR 0024 put it: a fuel limit is not portable
between the two backends. So `decide_within` is the case written against
`max_host_calls` — one decision makes two calls, `pull` and `record` — and it
is the one asserted on both backends, because a call is a call on either and a
unit of fuel is not.

There is no way to install a budget that does not take `&mut self` on a
backend. ADR 0024 states each stop as a bound that holds over a run, and a
budget swappable while the run it bounds was executing would make each of
those bounds a claim about something that had changed underneath it; a backend
running an invocation is mutably borrowed for its whole duration, so the shape
forbids it.

### Who checks this program

`cove check` in `examples/` reports one warning, and it is correct:

```console
warning[cove::resolve::unchecked_host]: no Host API schema describes the host module `reviews`, so calls into it are unchecked
 --> examples/rules/embedded/embedded.cove:1:1
  |
1 | use reviews
  | ^^^^^^^^^^^
  help: if `reviews` is an embedder's module, hand its `ModuleSchema` to the compiler with `Compiler::new().with_host_schema(...)`
```

`reviews` is the Rust crate's, and no `cove` command has heard of it. The
warning's own help says what to do about it, and `RulePackage::load` does
exactly that: the same source compiled with the schema in hand produces no
notices at all, which
`embedding.one_compiled_package_decides_every_open_request` asserts.

[Issue #151](https://github.com/myuon/cove/issues/151) asked for the missing
key — a `--schema` flag, or a `[hosts]` table in `cove.toml`. **The answer is
that there should not be one**, and this document used to be the argument for
filing it, so it is the right place to record why. The argument is made in
full where an embedder meets it, in `cove_sema::compile`'s module doc; the
short of it is two things.

A serialized description is a *second* description. The only thing that makes
`REVIEWS` worth anything is that the checker and the boundary read one value;
a table in `cove.toml` is another one, written by hand in another vocabulary,
and a checker reading it while the run enforces the `const` reports exactly
the failure [ADR 0017](../../docs/adr/0017-embedder-host-api-schemas.md)
exists to prevent — with the authority of having checked. Generating the file
from the `const` removes the drift and leaves the staleness.

And `cove test` settles it for any format at all. A schema lets the checker
*check* a call into `reviews`; it lets nothing *run* one, because what answers
a call is `Reviews`, an implementation, and no description carries one. A
`cove` handed a schema would check a rule package it still could not test —
which is the command a rule author uses most.

So the toolchain is the embedder's to provide, and the checking half of it is
one line:

```console
$ cargo run -p cove-rules --bin cove-rules-check
`rules` checks against `reviews`: 10 files, 6 modules, no notices
```

`host/src/bin/check.rs` is the whole of it. It is not a fork of `cove check`:
it reads the same package off disk, runs the same `cove_sema::Compiler`,
renders with the same `cove_diag::render`, and differs by the schemas it was
handed — the same `REVIEWS` the registry answers with, so the two cannot
drift. The test runner is more than one line because it needs `Reviews` as
well, which is the same conclusion arriving from the other end.

The warning stays visible. A `cove check` that was handed no description has
not checked those calls, and saying so is better than a silence that reads
like a proof.

The rest of what the embedding checks is checked properly, and the tests in
`host/tests/embedding.rs` say so one case at a time: an argument the schema
does not admit is refused at the boundary; a *result* the host's own schema
does not admit is refused too, before it reaches the program; an ungranted
capability is refused before the host's implementation is reached at all; a
host that fails stops the invocation and leaves the session serving the next
one.

### Schema evolution

Two cases, and they are the two an interface has.

An **additive** change — `REVIEWS_NEXT` adds an operation and a field — leaves
a package written against the older schema checking, lowering and deciding
exactly as before. Nothing calls the operation and nothing reads the field, and
the package does not have to be told that.

The host does. `Vm` materialises a Host API result into the physical layout
its schema declares the moment a call returns — one fixed-width struct, every
field's word written before the value is a value at all — rather than reading
a field lazily out of a tagged one the way the interpreter's oracle value
does. So a host that registers `REVIEWS_NEXT` has to answer all eleven fields
of a `reviews.PullRequest`, `openedAt` included, even though nothing this
package lowers reads it; a host that goes on answering `REVIEWS`'s ten is
answering a value its own declared schema does not admit, and is refused at
the boundary rather than let through with a hole in it. `Reviews::answer`
(`host/src/lib.rs`) is where this crate does that: it is the same ten fields
`PullRequest::to_cove` always built, plus a zero `openedAt` when `self.schema`
is the one that declares it. This was not a rule an embedder had to know
before: the predecessor backend's boundary held a host's argument to a
schema's fields as they were read, one at a time, so an unread field of a
richer schema was never asked for and a host answering an older, narrower
struct was never caught not having it.

A **breaking** change — `REVIEWS_RENAMED` renames `changedLines` — is reported
by the checker, at the line that reads the field, before anything runs:

```text
`reviews.PullRequest` has no field `changedLines`
```

Without `with_host_schema` that rename would have been discovered by the
boundary, on the first invocation, in production, in whichever host call
happened to come first. This is the whole argument for
[ADR 0017](../../docs/adr/0017-embedder-host-api-schemas.md) in one diff.

## Measurements

```text
cargo run --release -p cove-rules --bin cove-rules-measure -- 3000
```

Every number below was taken in one sitting on a machine doing nothing else,
at `732b238`, and the wall times are medians of five runs of that command:

```text
Intel(R) Core(TM) i7-10700K CPU @ 3.80GHz, 32 GiB, macOS 26.5.2 (x86_64)
rustc 1.93.1 (01f6ddf75 2026-02-11), --release
medians of five runs, 3,000 invocations a row
```

That matters more than a provenance note usually does, because the wall times
in this document had never been re-taken since it was written and were carried
from another machine, and because the counts had been marked stale on a
prediction that turned out to be wrong.

**Every number in this section is about the predecessor backend.**
`732b238` predates
[ADR 0034](../../docs/adr/0034-one-physical-word-stack.md)'s cutover, which
deleted `cove-ir` and `cove_runtime::vm::Vm` and replaced them with a
clean-room backend that has since taken those same names — a replacement, not
a renaming, so nothing here says the counts below are today's `Vm`'s. They
have not been retaken on it, and this section is left as the record of what
the predecessor cost rather than silently reattributed to its replacement.
Where the rest of this document describes what the embedding does today, the
`Vm` it names is the one running now; where it reports a number, that number
is the predecessor's until somebody reruns `cove-rules-measure` and replaces
this whole section, table by table.

**The prediction was that making `labels` a `Set` would move the invocation
counts, and it moved none of them.** The reasoning was that the `Set`
removes a loop from every decision, so instructions and allocations both had
to fall. Measured at `732b238` and at `69ce074` — the change's parent — in the
same session on this machine, every per-invocation allocation count and every
per-invocation instruction count is *identical*, to the unit:

| | parent | now | moved |
| --- | ---: | ---: | ---: |
| `decideSample` | 210 allocations, 645 instructions | 210, 645 | nothing |
| `evaluate` | 237, 600 | 237, 600 | nothing |
| `pullOnly` | 48, 34 | 48, 34 | nothing |
| `decideRequest` | 277, 735 | 277, 735 | nothing |

The loop that went away is `PullRequest.labelSet`, and the only caller it ever
had is `labelled`, which only `GuardedPathRule` asks — and only for a pull
request that touches a guarded prefix. Every pull request these rows decide is
`large()`, which touches `src/scheduler.rs` and carries no labels at all. So
the removed loop was never on the measured path, and "removes a loop from
every decision" was true of the source and false of these rows. `LabelRule`
walks `pr.labels` directly and always did; a `Set` of nothing and an `Array` of
nothing are the same zero turns.

What did move is worth recording, because it is the shape of the change rather
than the size of it.

**Every invocation moved by exactly 24 bytes, at the same allocation count.**
210 allocations of 11,275 bytes became 210 of 11,299, and the same +24 appears
on every row below that carries a pull request — seven of the eight, traced and
untraced alike, all but the `floor` that carries none. One allocation is 24
bytes wider than it was: the `Set` value where the `Array` was. A change of
shape at a fixed number of allocations is what a representation change looks
like when it is not also an allocation change.

**Lowering lost a function and the allocations that went with it.**
`decideSample` reaches 33 functions where it reached 34, `evaluate` 26 where it
reached 27, `decideRequest` 31 where it reached 32 — the missing one is
`labelSet` — and each lowering costs 9 to 41 fewer allocations. Loading gained
76 allocations and 9.6 KB, which is the source text of the doc comments that
explain the new shape. So the change is visible in what is *compiled* and
invisible in what is *run*, which is the opposite of what the stale note
predicted, and it is the reason the note is now retired rather than corrected.

**Every ratio this table is quoted for is unchanged**, and for a better reason
than the stale note gave. The note argued they survived because the change did
not touch the mechanisms behind them; the measurement says the rows themselves
did not move, so there was nothing for a ratio to survive. The boundary route
still costs 40 more allocations and 135 more instructions than the argument
route, a trace sink still costs 65 allocations on `decideRequest` and none on
`evaluate`, and one crossing still costs 37 allocations and 30 instructions.
The two figures that did move are the two with a wall time or a `Vm::new` in
them: compiling is worth 168 invocations rather than 162, and reuse saves 167
allocations rather than 168.

Allocations, bytes and instructions come from a counting `GlobalAlloc`
installed in the measurement binary and from `Vm::instructions`, not from a
sampler, so they are exact and every row is comparable with every other one
taken in the same session. Each was identical across all five runs.

An allocation count is *nearly* machine-independent and not exactly, which this
document found the hard way and keeps recording. An earlier session read
`decideSample` at 210 allocations where a table taken elsewhere said 218, and
`decideRequest` at 277 where it said 285 — a constant eight on every row that
runs the rule catalog, on the same source at the same commit, from something in
the standard library between rustc 1.93.0 and 1.93.1. This session reads the
same 210 and 277 at both commits. So a difference of a handful from the table
above is a fact about the machine that read it, and only a difference measured
at two commits *in one session* is a fact about the program.

The binary stays in the repository rather than being a scratch instrument that
is deleted. It is not a benchmark — nothing runs it on a push and it is not in
`cove-bench` — but it is the only thing that can say whether a change to the
embedding API made an embedding cheaper or more expensive, and a number nobody
can reproduce is a number nobody can argue with. It earned that keep here: the
rows below are what said `evaluate` costs 40 allocations less than the boundary
route rather than "less".

### What is paid once

Over ten `.cove` files in six modules.

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| load: read, parse, resolve, check | **5,788,073** | 32,850 | 4,437,621 |
|   of which: read from disk | 604,595 | | |
|   of which: parse | 1,433,721 | | |
|   of which: resolve and check | 3,583,954 | | |
| lower `rules.decideSample` (33 fns) | 268,799 | 1,985 | 190,574 |
| lower `rules.embedded.decideRequest` (31 fns) | 256,775 | 1,876 | 170,990 |
| lower `rules.embedded.evaluate` (26 fns) | 208,398 | 1,643 | 145,873 |
| lower `rules.embedded.pullOnly` (2 fns) | 56,183 | 614 | 49,338 |
| lower `rules.floor` (1 fn) | 47,217 | 555 | 42,537 |

Checking is 62% of loading and parsing is 25%, which is the ratio ADR 0022
predicted when it made a VM run check as well as resolve: the lowering reads
the checker's answers, so the check is not a second opinion but the thing that
produces them.

**Checking records two kinds of answer it did not used to, and it costs 42
allocations.** `invoke` holds a host's argument to a declared struct's fields
and to a declared enum case's payload, and the only account of either is the
checker's, so `cove_sema` records a signature for each — the synthesized
initializer `PullRequest(id: ..)` calls. Over this package that is about two
dozen declarations and 42 allocations, or 0.13% of loading. The rest of what
loading has gained since is source text: `evaluate` and the doc comments that
explain it were 164 allocations, and the `Set` change's own comments 76 more.

This is the one table the `Set` change moved. Every lowering costs less than it
did — `decideSample` 1,985 allocations where it cost 2,014, `evaluate` 1,643
where it cost 1,684 — because `PullRequest.labelSet` is gone and each entry
that reaches the rule catalog reaches one fewer function for it. Loading costs
more, because the change is longer to explain than it is to make.

Lowering is measured over twenty turns because the first one a process performs
is cold; loading is measured once, because loading once is what it is for.
`evaluate` reaches 26 functions where `decideRequest` reaches 31, and the five
are `rules.embedded.pullRequest` and what the Host API call it makes drags in.

**Against a 34.5 µs invocation, loading is worth 168 invocations and lowering
the entry is worth 7.4.** An application that decides more than a couple of
hundred pull requests over its life spends more of its own time deciding than
it spent compiling. Compiling per request would not be.

### What is paid per invocation

One VM serving all of them, which is the shape an embedding is for.

| | ns | allocations | bytes | instructions |
| --- | ---: | ---: | ---: | ---: |
| `rules.floor` — an entry that does nothing | 925 | 11 | 240 | 4 |
| `rules.decideSample` — the catalog over its own fixture | 25,866 | 210 | 11,299 | 645 |
| `rules.embedded.evaluate` — the catalog over the host's | 27,461 | **237** | 12,061 | **600** |
| `rules.embedded.pullOnly` — one host call, no rules | 4,113 | 48 | 2,106 | 34 |
| `rules.embedded.decideRequest` — the catalog, across the boundary | 34,486 | **277** | 13,867 | **735** |
| `evaluate`, with a trace sink installed | 27,548 | 237 | 12,061 | 600 |
| `pullOnly`, with a trace sink installed | 8,104 | 101 | 4,154 | 34 |
| `decideRequest`, with a trace sink installed | 40,664 | 342 | 16,551 | 735 |

Each row is a control on another, and the differences are the answer this
example was written to produce.

**An invocation costs 11 allocations and four instructions before the program
does anything.** Finding the entry by name, entering the frame, and answering.
That is the floor every other row stands on, and it is small enough that
nothing below is about it.

#### What the way in costs

`evaluate` and `decideRequest` reach the same decision over the same pull
request — `the_two_ways_in_reach_the_same_decision` asserts it — and differ in
how the pull request got there. One takes it as an argument. The other takes a
request identifier as a process argument, fetches the pull request through
`reviews.pull`, rebuilds it field by field into the package's own struct, and
reports the decision back through `reviews.record`.

**The boundary route costs 40 more allocations and 135 more instructions**:
277 against 237, and 735 against 600. That is 14% of its allocations and 18% of
its instructions spent on carrying an argument in and an answer out through a
mechanism meant for reaching outside the process.

The third control decomposes it. `decideSample` runs the same catalog over a
pull request the package builds in Cove and makes no host call at all, at 210
allocations and 645 instructions. So:

- `evaluate` against `decideSample` — **+27 allocations, −45 instructions**.
  The 27 are what it costs for the value to have come from Rust at all, and 21
  of them are `PullRequest::to_policy` on the Rust side; six are the runtime
  placing the argument. The 45 fewer instructions are the Cove that
  `decideSample` runs to build its own fixture and `evaluate` does not.
- `decideRequest` against `evaluate` — **+40 allocations, +135 instructions**.
  That is the boundary, isolated: two crossings, the schema check on each, and
  the ten-field rebuild `rules.embedded.pullRequest` performs because a Host
  API schema cannot name a type a Cove package owns.

Read against the older reading of the same table, this splits a number the
document used to quote as one. `decideRequest` against `decideSample` is 67
allocations, and this example used to call all 67 "the Host API boundary".
Twenty-seven of them are not: they are what *any* host-supplied input costs,
and an application pays them whichever way in it uses. Forty are the boundary.

**One crossing carrying a ten-field struct costs 37 allocations and 30
instructions.** That is `pullOnly` against `floor`: one `reviews.pull`, the
struct value the Rust side built for it, and the field-by-field rebuild.
Twenty-one of the 37 are `PullRequest::to_cove`, so the runtime's half of one
crossing is **16 allocations**.

For scale, with a caveat.
[Issue #123](https://github.com/myuon/cove/issues/123)'s calling-convention
matrix measured a Host callback and the reentry that runs it at 14 allocations,
on an operation carrying one `Int`. That is not the same mechanism: it
re-enters the VM to run a Cove closure, which `reviews.pull` does not, so it
does strictly more per call, and it still allocates 2.6× less. What differs is
what crossed — one `Int` against a ten-field struct with two arrays — so the
boundary's cost is dominated by the value rather than by the call, which is the
thing a fixed per-call figure could not have said. An argument passed to
`invoke` is that observation taken to its conclusion: the cheapest way to carry
a value across a boundary is not to.

#### What a trace costs, and when it costs nothing

**A trace sink costs 65 allocations on `decideRequest` and nothing at all on
`evaluate`.** 342 against 277, and 237 against 237 — the same number, not a
number close to it. A registry with a sink installed describes every argument
and every result a *host call* carried, as a `RecordedValue`, which is a deep
copy; a registry with the default `NullSink` answers `is_recording()` with
`false` and skips it. An invocation that makes no host call has nothing to
describe, so a sink watching it observes the entry and the run ending and
copies no value.

How the 65 split is the same finding from the other side. One `pull` traced
costs 53 allocations more than one `pull` untraced and carries a ten-field
struct; the `record` beside it carries four scalars and costs the other 12. So
the description is a per-value charge and not a per-call one, and a host
operation that hands a large value across is the one a trace makes expensive.
The predecessor backend's mechanism suite recorded the same effect from the
other end, at 16.8% of `benches/hostheavy`; that suite was deleted with the
backend it measured and is in git history at commit `6e90085`.

### What reuse is worth

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| `decideRequest`, one VM for every invocation | **34,486** | 277 | 13,867 |
| `decideRequest`, a new `Runtime` and `Vm` each time | 49,264 | 444 | 31,799 |
| `decideRequest`, on the interpreter | 87,683 | 821 | 44,523 |

**Building a `Runtime` and a `Vm` costs 167 allocations**, against the 277 an
invocation costs. A `Vm::new` reads the lowered program's struct shapes, its
enum shapes and its constants and builds a table of each, so that cost is
proportional to the program rather than to the request, and paying it per
request is paying it for nothing: 38% of everything such an application did
would be rebuilding tables it had already built.

That is the number this example was asked for, and counting the compile as
well, an application that rebuilt everything per request would pay the whole of
the load row on top of it.

**The VM allocates 2.96× less than the interpreter here**, on a program written
for a domain rather than for a benchmark, with a Host API call in it.
The suite this is being weighed against was mechanism benchmarks; this is a
second kind of evidence for the same claim, and it agrees.

### The Rust side of the conversion

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| `PullRequest::to_cove` | 1,268 | 21 | 1,056 |
| `PullRequest::to_policy` | 1,269 | 21 | 1,056 |
| `Decision::from_cove` | 409 | 5 | 360 |

The first two are the same ten fields under two type names, and the table says
so rather than assuming it: `reviews.PullRequest` is what the boundary carries
and `rules.policy.PullRequest` is what an invocation carries, and building
either costs 21 allocations. Nothing about passing a value directly makes the
value cheaper to build. What it removes is everything that happened to it
afterwards.

All three are written the obvious way — build the vector of fields, ask the
enum for its case, clone the string — rather than the fast way, because what an
embedder writes is what an embedder pays. Inbound costs 4.2× outbound in allocations,
and the reason is the shape rather than the direction: ten fields and two
arrays go in, and a policy and a short list of findings come out.

## Gaps

Each of these is something this example wanted and the toolchain does not have.
They are issues rather than workarounds in the program.

- **An embedder still builds its arguments out of the runtime's own
  `Value`,** though it no longer has to know what one is made of. This bullet
  used to say that `Decision::from_cove` matched on `Value`'s variants while
  `Value::structure` and its neighbours meant nothing had to *build* one that
  way, and [issue #186](https://github.com/myuon/cove/issues/186) closed that
  half: `Value::field`, `Value::fields`, `Value::case`, `Value::payload`,
  `Value::items`, `Value::elements`, `Value::entries`, `Value::declared_type`
  and the scalar `as_*` readers are how this crate reads an answer now, and it
  names no variant, no `Rc` and no `Box` in either direction. What is left is
  that a `Value` is still the currency: the variants are still `pub`, so
  nothing *stops* a host matching on one, and a host that does is the one that
  a change to the representation still breaks.
  [Issue #109](https://github.com/myuon/cove/issues/109) asked for less
  exposure than that, and sealing the variants is the part of it that remains.
- **A rule package written against an embedder's module still cannot be
  checked by `cove check`, and that is now a decision rather than a gap.**
  [#151](https://github.com/myuon/cove/issues/151) asked for a flag or a
  `cove.toml` key and the answer is no, for the two reasons
  [Who checks this program](#who-checks-this-program) gives: a serialized
  schema is a second description of a module whose first one is Rust, and a
  schema would let `cove check` check a package that `cove test` still could
  not run. The toolchain is the embedder's, and `cove-rules-check` is it.
- **A package directory must be a Cove identifier**, so this example is
  `examples/rules/` and not `examples/cove-rules/` as issue #90 names it: a
  directory holding `.cove` files becomes a module and `cove-rules` is not an
  identifier. `cove_sema::package` reports that clearly, so it is recorded here
  rather than filed.

## What worked

Worth recording beside the gaps, because the list is longer.

- **The natural signature is callable.** `export fn evaluate(pr: PullRequest)
  -> Decision` is what issue #90 wrote down, and an application invokes it with
  a value it built and reads a typed decision back. What it took was one seam
  on each backend and one check against the signature the checker had already
  resolved.
- **A wrong argument is the checker's diagnostic and not a crash.** The
  lowering reads a declared struct's field by index, so a host that built one
  with nine of ten fields would have had `Vm` read past its end. `invoke`
  holds the argument to the declaration before the first instruction, so what
  a host gets back is a sentence naming the argument, the field, and the type
  the declaration says belongs there.
- **Nothing in the program had to be written around the lowering.** Six rules
  behind a `dyn` with a defaulted method, the same call again through a bounded
  type parameter, `Set` and `Map` as a rule's state, `sorted(by:)` over a
  comparison of an enum's rank, `fold` and `filter` over closures, `match` on an
  enum case carrying a struct — all of it lowered on the day it was written.
  `examples:reviewPolicy` is in the differential corpus and both backends agree
  on it, which is what `LOWERED_FLOOR` moving from 94 to 95 records.
- **One value describes the module to both ends.** `REVIEWS` goes to
  `Compiler::with_host_schema` and comes back out of `HostApi::module_schema`,
  so a rename is a check-time error rather than a boundary surprise, and there
  is no second copy to keep in step.
- **A field crosses as the shape it is.** `labels` is a `Set<String>` on both
  sides. It was an `Array<String>` with a `labelSet` in `rules.policy` that
  walked it into a set wherever a rule asked a membership question, and that
  loop was in the module about review policy for a limitation of the schema
  vocabulary rather than for anything about reviews.
  [Issue #153](https://github.com/myuon/cove/issues/153) gave `HostType` a
  `Set` and a `Map`, so the host builds the set and the rules ask it. The one
  thing a schema can now say and no value satisfy — a `Set` element or a `Map`
  key Cove's `MapKey` restriction does not admit — is refused where the schema
  is read, and `the_schema_declares_only_types_a_value_could_have` is the one
  assertion an embedder writes over its own table to get that. What it bought
  is clarity and one function fewer in every lowering, and *not* a cheaper
  decision: [Measurements](#measurements) says why the invocation counts did
  not move a unit, and it is the kind of thing only a measurement says.
- **A limit can belong to a request.** `evaluate_within` and `decide_within`
  hand one invocation its own `Budget` on a `Vm` that was built once, which
  is [What bounds one request](#what-bounds-one-request) and was
  [issue #152](https://github.com/myuon/cove/issues/152).
- **A failed invocation does not damage the session.** A host that fails, a
  request that does not exist, a schema the host broke, and a capability that
  was not granted are four different failures and all four leave `Vm` able to
  serve the next request. That is what makes holding one worthwhile.
- **The trace links itself.** Both `reviews` operations take the request
  identifier as their first argument, so every `HostCall` event carries it and
  a sink can group a run's calls by application request without the runtime
  knowing what an application request is.

## Tests

`cove test` runs 28 `test fn` declarations across `rules.policy`,
`rules.catalog` and `rules.engine`: each rule in isolation, both dispatch forms
against each other, the six fixtures against the six arms of the policy, and
`policyFor` over findings assembled by hand so that a combination rule can be
asserted without arranging a pull request that produces it.

`cargo test -p cove-rules` runs twenty-two more, in `host/tests/embedding.rs`:
six pull requests evaluated directly, the two ways in reaching the same
decision, both backends answering a direct invocation the same way, an argument
the declaration does not admit, one compiled package deciding six requests
across the boundary, both backends agreeing on the embedded entry, the Rust
fixtures and the Cove fixtures agreeing, an additive schema change, a breaking
one, an argument and a result the boundary refused, a capability that was not
granted, a host that failed, a session that kept serving, a budget spent over
the session and a budget spent by one request, a host-call limit per request
on both backends, the request identifier in the trace, the conversion
following the schema's field order, `labels` crossing as the set it is, and
the schema itself declaring only types some value could have.

The fixtures are written twice — once in `rules.fixtures` and once in
`cove_rules::samples` — because in a real embedding they arrive from the
application, and two copies of anything drift.
`the_host_s_fixtures_and_the_package_s_agree` is what holds them together: the
decision reached over the host's copy, through the boundary, must equal the
decision reached over the package's own, which crosses nothing.
