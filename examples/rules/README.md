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
is that compiling is worth about 162 invocations, that reusing one VM instead
of building one per request saves 168 allocations against the 237 an invocation
costs, and that how the pull request gets in is worth 40 allocations and 135
instructions.

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

`host/` is a workspace member, so `cargo test --workspace` runs its eighteen
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
| `host/` | the Rust application that embeds all of the above |

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

**Lower the entry, once.** `cove_ir::lower::lower_entry` lowers what one entry
can reach, so an application that invokes two entries lowers twice and holds
both.

**Build one backend and invoke it many times.** `RulePackage::serve` builds a
`Runtime` and a `Vm` and hands them to a closure; every invocation the closure
makes is served by that one VM. It takes a closure rather than answering with a
session because a `Vm` borrows the `Runtime` and the lowered program, and
because nothing Cove-shaped could leave it in any case: a `Value` is `Rc`-based
and is not `Send`.

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
emits `get-field-at 4` where the source wrote `pr.changedLines`, so a struct a
host built with nine fields would have the VM read past the end of one and a
struct with ten in another order would answer the wrong field with no sign of
it. `crates/cove-runtime/src/invoke.rs` is where that is refused, and
`an_argument_the_rules_do_not_declare_is_refused_before_anything_runs` is what
holds it to the three ways a host can get it wrong.

A capability is not part of that check, and an invocation grants nothing: what
`evaluate` may reach is what the `HostRegistry` was granted, exactly as for a
run `cove run` started. `evaluate` reaches nothing, which is why the case above
grants it nothing.

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

This is worth leaving visible rather than hiding, because it is the shape of a
real problem. The person who writes rules is not the person who wrote the
embedder, and the rule author's toolchain is `cove check`, `cove fmt` and
`cove test`, none of which can be handed a schema. Today they can format their
rules and can only half-check them. That is
[issue #151](https://github.com/myuon/cove/issues/151).

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
nothing has to be told that.

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
cargo run --release -p cove-rules --bin cove-rules-measure -- 500
```

**The counts below were re-taken for this change and the times were not.**
Allocations, bytes and instructions come from a counting `GlobalAlloc`
installed in the measurement binary and from `Vm::instructions`, not from a
sampler, so they are exact and every row is comparable with every other. The
wall times are the ones this document has carried since it was written:

```text
Intel(R) Core(TM) i7-10700K CPU @ 3.80GHz, 32 GiB, macOS 26.5.2
rustc 1.93.1, --release, medians of five runs, 3,000 invocations a row
```

They were taken before `evaluate` existed, on another machine, and the machine
this change was made on was busy with other work. **Every wall time in this
document needs re-taking, in one session, on a quiet machine**, and the two new
rows have none at all. Nothing below argues from a time where a count would do,
and where a time is quoted it is marked.

Re-taking the counts turned up something worth recording, because this document
had claimed the counts were what a different machine could check. At the parent
commit, on the machine this change was made on, `decideSample` costs 210
allocations where the table above said 218, and `decideRequest` 277 where it
said 285 — a constant eight on every row that runs the rule catalog, on the
same source at the same commit. So an allocation count is *nearly*
machine-independent and not exactly: something in the standard library between
rustc 1.93.0 and 1.93.1 allocates eight fewer times over this program. The
counts below are one session's, and the base-versus-branch comparison behind
them was made in that session too.

The binary stays in the repository rather than being a scratch instrument that
is deleted. It is not a benchmark — nothing runs it on a push and it is not in
`cove-bench` — but it is the only thing that can say whether a change to the
embedding API made an embedding cheaper or more expensive, and a number nobody
can reproduce is a number nobody can argue with. It earned that keep here: the
rows below are what said `evaluate` costs 40 allocations less than the boundary
route rather than "less".

### What is paid once

Over ten `.cove` files in six modules.

| | ns *(stale)* | allocations | bytes |
| --- | ---: | ---: | ---: |
| load: read, parse, resolve, check | **5,676,618** | 32,764 | 4,427,704 |
|   of which: read from disk | 582,767 | | |
|   of which: parse | 1,434,899 | | |
|   of which: resolve and check | 3,554,366 | | |
| lower `rules.decideSample` (34 fns) | 275,637 | 2,014 | 195,691 |
| lower `rules.embedded.decideRequest` (32 fns) | 264,219 | 1,917 | 176,395 |
| lower `rules.embedded.evaluate` (27 fns) | — | 1,684 | 151,278 |
| lower `rules.embedded.pullOnly` (2 fns) | 54,961 | 623 | 50,646 |
| lower `rules.floor` (1 fn) | 47,491 | 564 | 43,845 |

Checking is 63% of loading and parsing is 25%, which is the ratio ADR 0022
predicted when it made a VM run check as well as resolve: the lowering reads
the checker's answers, so the check is not a second opinion but the thing that
produces them.

**Checking now records two more kinds of answer, and it costs 42 allocations.**
`invoke` holds a host's argument to a declared struct's fields and to a
declared enum case's payload, and the only account of either is the checker's,
so `cove_sema` records a signature for each — the synthesized initializer
`PullRequest(id: ..)` calls. Over this package that is about two dozen
declarations and 42 allocations, or 0.13% of loading. The remaining difference
between 32,558 at the parent commit and 32,764 here is the source: `evaluate`
and the doc comments that explain it are 164 allocations of text.

Lowering is measured over twenty turns because the first one a process performs
is cold; loading is measured once, because loading once is what it is for.
`evaluate` reaches 27 functions where `decideRequest` reaches 32, and the five
are `rules.embedded.pullRequest` and what the Host API call it makes drags in.

**Against a 35.1 µs invocation *(stale)*, loading is worth 162 invocations and
lowering the entry is worth 7.5.** An application that decides more than a
couple of hundred pull requests over its life spends more of its own time
deciding than it spent compiling. Compiling per request would not be.

### What is paid per invocation

One VM serving all of them, which is the shape an embedding is for.

| | ns *(stale)* | allocations | bytes | instructions |
| --- | ---: | ---: | ---: | ---: |
| `rules.floor` — an entry that does nothing | 923 | 11 | 240 | 4 |
| `rules.decideSample` — the catalog over its own fixture | 26,432 | 210 | 11,275 | 645 |
| `rules.embedded.evaluate` — the catalog over the host's | — | **237** | 12,037 | **600** |
| `rules.embedded.pullOnly` — one host call, no rules | 4,083 | 48 | 2,082 | 34 |
| `rules.embedded.decideRequest` — the catalog, across the boundary | 35,071 | **277** | 13,788 | **735** |
| `evaluate`, with a trace sink installed | — | 237 | 12,037 | 600 |
| `pullOnly`, with a trace sink installed | 7,796 | 101 | 4,080 | 34 |
| `decideRequest`, with a trace sink installed | 40,971 | 342 | 16,372 | 735 |

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
`docs/VM_ARCHITECTURE.md` records the same effect from the other end, at 16.8%
of `benches/hostheavy`.

### What reuse is worth

| | ns *(stale)* | allocations | bytes |
| --- | ---: | ---: | ---: |
| `decideRequest`, one VM for every invocation | **35,071** | 277 | 13,788 |
| `decideRequest`, a new `Runtime` and `Vm` each time | 50,319 | 445 | 31,841 |
| `decideRequest`, on the interpreter | 86,411 | 822 | 44,461 |

**Building a `Runtime` and a `Vm` costs 168 allocations**, against the 277 an
invocation costs. A `Vm::new` reads the lowered program's struct shapes, its
enum shapes and its constants and builds a table of each, so that cost is
proportional to the program rather than to the request, and paying it per
request is paying it for nothing: 38% of everything such an application did
would be rebuilding tables it had already built.

That is the number this example was asked for, and counting the compile as
well, an application that rebuilt everything per request would pay the whole of
the load row on top of it.

**The VM allocates 2.97× less than the interpreter here**, on a program written
for a domain rather than for a benchmark, with a Host API call in it.
`docs/VM_ARCHITECTURE.md`'s suite is mechanism benchmarks; this is a second
kind of evidence for the same claim, and it agrees.

### The Rust side of the conversion

| | ns *(stale)* | allocations | bytes |
| --- | ---: | ---: | ---: |
| `PullRequest::to_cove` | 1,211 | 21 | 1,032 |
| `PullRequest::to_policy` | — | 21 | 1,032 |
| `Decision::from_cove` | 440 | 5 | 360 |

The first two are the same ten fields under two type names, and the table says
so rather than assuming it: `reviews.PullRequest` is what the boundary carries
and `rules.policy.PullRequest` is what an invocation carries, and building
either costs 21 allocations. Nothing about passing a value directly makes the
value cheaper to build. What it removes is everything that happened to it
afterwards.

All three are written the obvious way — build the vector of fields, match the
enum, clone the string — rather than the fast way, because what an embedder
writes is what an embedder pays. Inbound costs 4.2× outbound in allocations,
and the reason is the shape rather than the direction: ten fields and two
arrays go in, and a policy and a short list of findings come out.

## Gaps

Each of these is something this example wanted and the toolchain does not have.
They are issues rather than workarounds in the program.

- **An embedder still builds its arguments out of the runtime's own
  `Value`.** `Value::structure`, `Value::array` and `Value::enumeration` mean a
  host no longer names `Rc<StructValue>` or the `opaque` flag to build one, but
  a `Value` is still what crosses in both directions and
  `Decision::from_cove` still matches on its variants.
  [Issue #109](https://github.com/myuon/cove/issues/109) asks for the internal
  representation to become less exposed than that, and gates the work on VM
  profiles it also asks this example to produce.
- **A rule package written against an embedder's module cannot be checked by
  `cove check`.** There is nowhere to hand the CLI a `ModuleSchema`: not a
  flag, not a `cove.toml` key. The author of a rule package therefore cannot
  run the checker the embedder runs, and `cove test` cannot run a test that
  touches the embedder's module at all.
  [#151](https://github.com/myuon/cove/issues/151)
- **A budget cannot be given to one invocation.** A `Budget` lives on the
  `HostRegistry`, `set_budget` needs `&mut`, and a backend holds the registry by
  shared reference for as long as it exists. So fuel, the deadline and the
  host-call limit are spent over the whole session rather than reset per
  invocation, and an embedder that wants to bound one request has to build a
  registry, a `Runtime` and a `Vm` for it — the 168 allocations above.
  `embedding.fuel_is_spent_over_the_session_and_not_over_one_invocation` pins
  the behaviour as it is. [#152](https://github.com/myuon/cove/issues/152)
- **A Host API schema cannot declare a `Map` or a `Set`.** `HostType` has
  `Array`, `Option` and `Result` and nothing else compound, so `labels` crosses
  as an `Array<String>` and `rules.policy.PullRequest.labelSet` converts it
  where a membership question is asked.
  [#153](https://github.com/myuon/cove/issues/153)
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
  with nine of ten fields would have had the VM read past its end. `invoke`
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
- **A failed invocation does not damage the session.** A host that fails, a
  request that does not exist, a schema the host broke, and a capability that
  was not granted are four different failures and all four leave the VM able to
  serve the next request. That is what makes holding one worthwhile.
- **The trace links itself.** Both `reviews` operations take the request
  identifier as their first argument, so every `HostCall` event carries it and
  a sink can group a run's calls by application request without the runtime
  knowing what an application request is.

## Tests

`cove test` runs 27 `test fn` declarations across `rules.policy`,
`rules.catalog` and `rules.engine`: each rule in isolation, both dispatch forms
against each other, the six fixtures against the six arms of the policy, and
`policyFor` over findings assembled by hand so that a combination rule can be
asserted without arranging a pull request that produces it.

`cargo test -p cove-rules` runs eighteen more, in `host/tests/embedding.rs`:
six pull requests evaluated directly, the two ways in reaching the same
decision, both backends answering a direct invocation the same way, an argument
the declaration does not admit, one compiled package deciding six requests
across the boundary, both backends agreeing on the embedded entry, the Rust
fixtures and the Cove fixtures agreeing, an additive schema change, a breaking
one, an argument and a result the boundary refused, a capability that was not
granted, a host that failed, a session that kept serving, fuel, a host-call
limit, the request identifier in the trace, and the conversion following the
schema's field order.

The fixtures are written twice — once in `rules.fixtures` and once in
`cove_rules::samples` — because in a real embedding they arrive from the
application, and two copies of anything drift.
`the_host_s_fixtures_and_the_package_s_agree` is what holds them together: the
decision reached over the host's copy, through the boundary, must equal the
decision reached over the package's own, which crosses nothing.
