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
of building one per request is worth 43% of an invocation, and that the Host
API boundary is 25% of an invocation's time and 24% of its allocations before
anything is traced and 14% more when it is.

`host/` is a workspace member, so `cargo test --workspace` runs its fourteen
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
| `rules.embedded` | the adapter the Rust host invokes: `decideRequest` and `pullOnly` |
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

### What the boundary carries

An application request identifier goes in as the entry's one process argument.
The pull request comes back out through `reviews.pull`, as a
`reviews.PullRequest` the Rust side built with `PullRequest::to_cove`. The
decision leaves twice: as the value the entry returned, which
`Decision::from_cove` reads into a Rust enum, and through `reviews.record`,
which carries it back under the same request identifier that started it.

That the input arrives by a Host API call rather than as an argument is not a
design choice. `run_entry` takes a `Vec<Rc<str>>` on both backends and there is
no other way in; see [Gaps](#gaps). It does mean the example measures the
boundary rather than measuring around it, which is what
[issue #109](https://github.com/myuon/cove/issues/109) needed.

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
Intel(R) Core(TM) i7-10700K CPU @ 3.80GHz, 32 GiB, macOS 26.5.2
rustc 1.93.1, --release
cargo run --release -p cove-rules --bin cove-rules-measure -- 3000
```

Times are medians of five runs of the binary, 3,000 invocations a row, and the
spread between the fastest and slowest run is under 9% on every row below
except `rules.floor` (16.8%, on a 923 ns row). They were taken on a machine
doing other things and are worth the ratios between rows rather than the
absolute figures.

**The allocation and instruction counts are exact.** They come from a counting
`GlobalAlloc` installed in the measurement binary and from `Vm::instructions`,
not from a sampler, and they were identical in all five runs. That is
deliberate: [issue #109](https://github.com/myuon/cove/issues/109) asks what a
call across the boundary costs, and a count is an answer a different machine
can check.

The binary stays in the repository rather than being a scratch instrument that
is deleted. It is not a benchmark — nothing runs it on a push and it is not in
`cove-bench` — but it is the only thing that can say whether a change to the
Host API made an embedding cheaper or more expensive, and a number nobody can
reproduce is a number nobody can argue with.

### What is paid once

Over ten `.cove` files in six modules, 1,191 lines including tests and
doc comments.

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| load: read, parse, resolve, check | **5,676,618** | 32,558 | 4,393,400 |
|   of which: read from disk | 582,767 | | |
|   of which: parse | 1,434,899 | | |
|   of which: resolve and check | 3,554,366 | | |
| lower `rules.embedded.decideRequest` (32 fns) | 264,219 | 1,926 | 178,816 |
| lower `rules.decideSample` (34 fns) | 275,637 | 2,023 | 198,112 |
| lower `rules.embedded.pullOnly` (2 fns) | 54,961 | 620 | 50,616 |
| lower `rules.floor` (1 fn) | 47,491 | 561 | 43,815 |

Checking is 63% of loading and parsing is 25%, which is the ratio ADR 0022
predicted when it made a VM run check as well as resolve: the lowering reads
the checker's answers, so the check is not a second opinion but the thing that
produces them.

Lowering is measured over twenty turns because the first one a process performs
is cold; loading is measured once, because loading once is what it is for. The
disk read is whatever the page cache says — 0.58 ms warm here and 11.7 ms on
the first run after a build.

**Against a 35.1 µs invocation, loading is worth 162 invocations and lowering
the entry is worth 7.5.** An application that decides more than a couple of
hundred pull requests over its life spends more of its own time deciding than
it spent compiling, and 5.7 ms is not a startup cost anybody is going to
notice. Compiling per request would be.

### What is paid per invocation

One VM serving all of them, which is the shape an embedding is for.

| | ns | allocations | bytes | instructions |
| --- | ---: | ---: | ---: | ---: |
| `rules.floor` — an entry that does nothing | 923 | 11 | 240 | 4 |
| `rules.decideSample` — the whole catalog, no host call | 26,432 | 218 | 12,456 | 645 |
| `rules.embedded.pullOnly` — one host call, no rules | 4,083 | 48 | 2,082 | 34 |
| `rules.embedded.decideRequest` — both | **35,071** | **285** | 15,024 | 735 |
| `pullOnly`, with a trace sink installed | 7,796 | 101 | 4,130 | 34 |
| `decideRequest`, with a trace sink installed | 40,971 | 350 | 17,708 | 735 |

Each row is a control on the one above it, and the four differences are the
answer this example was written to produce.

**An invocation costs 923 ns before the program does anything.** Finding the
entry by name, building the `Array<String>` of arguments, entering the frame,
and answering: 11 allocations and four instructions. That is the floor every
other row stands on, and it is small enough that nothing below is about it.

**The Host API boundary is 25% of an invocation's time and 24% of its
allocations.** `decideRequest` against `decideSample` is the same rules over
the same pull request, differing in that one fetches it through `reviews.pull`
and reports the decision through `reviews.record` and the other builds it in
Cove and reports nothing: 35,071 ns against 26,432, and 285 allocations against
218. So two crossings cost **8,639 ns and 67 allocations**, against 90 more VM
instructions — which is the point. The boundary is not instructions. Fourteen
percent more instructions cost 33% more time.

**One crossing carrying a ten-field struct costs 3,160 ns, 37 allocations, and
30 instructions.** That is `pullOnly` against `floor`: one `reviews.pull`, the
struct value the Rust side built for it, and the field-by-field rebuild into
the package's own `PullRequest`. Twenty-one of the 37 allocations are Rust's
own `PullRequest::to_cove` — an `Rc<str>` per field name and per string, an
`Rc<[Value]>` per array, a `Vec` for the fields and an `Rc<StructValue>` around
them — so the runtime's half of one crossing is **16 allocations**.

For scale, with a caveat.
[Issue #123](https://github.com/myuon/cove/issues/123)'s calling-convention
matrix measured a Host callback and the reentry that runs it at 1,277 ns and 14
allocations, on an operation carrying one `Int`. That is not the same
mechanism: it re-enters the VM to run a Cove closure, which `reviews.pull` does
not, so it does strictly more per call. And it is still 2.5× cheaper in time
and 2.6× cheaper in allocations than this one. What differs is what crossed —
one `Int` against a ten-field struct with two arrays — so the boundary's cost
is dominated by the value rather than by the call, which is the thing a fixed
per-call figure could not have said.

**A trace sink costs 14% of a traced invocation and 65 allocations.** A
registry with a sink installed describes every argument and every result a call
carried, as a `RecordedValue`, which is a deep copy; a registry with the
default `NullSink` answers `is_recording()` with `false` and skips it. Two
calls traced cost 5,900 ns and 65 allocations more than two calls untraced.

The interesting part is how those 65 split. One `pull` traced costs 53
allocations more than one `pull` untraced, and it carries a ten-field struct;
the `record` beside it carries four scalars and costs the other 12. So the
description is not a per-call charge but a per-value one, and a host operation
that hands a large value across is the one a trace makes expensive.
`docs/VM_ARCHITECTURE.md` records the same effect from the other end, at 16.8%
of `benches/hostheavy`.

### What reuse is worth

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| `decideRequest`, one VM for every invocation | **35,071** | 285 | 15,024 |
| `decideRequest`, a new `Runtime` and `Vm` each time | 50,319 | 453 | 33,076 |
| `decideRequest`, on the interpreter | 86,411 | 821 | 44,491 |

**Building a `Runtime` and a `Vm` costs 15,248 ns and 168 allocations**, which
is 43% of an invocation on top of it. A `Vm::new` reads the lowered program's
struct shapes, its enum shapes and its constants and builds a table of each, so
that cost is proportional to the program rather than to the request, and paying
it per request is paying it for nothing. An application that built one per
invocation would be doing 43% more work per request, and 30% of everything it
did would be rebuilding tables it had already built.

That is the number this example was asked for and it is worth being plain about
what it is: **compile-once/invoke-many is worth about 30% on this workload,
before the compile is counted at all.** Counting the compile, an application
that rebuilt everything per request would pay 5.99 ms rather than 35.1 µs,
which is 171×.

**The VM is 2.46× the interpreter here and allocates 2.88× less**, on a program
written for a domain rather than for a benchmark, with a Host API call in it.
`docs/VM_ARCHITECTURE.md`'s suite is mechanism benchmarks; this is a second
kind of evidence for the same claim, and it agrees.

### The Rust side of the conversion

| | ns | allocations | bytes |
| --- | ---: | ---: | ---: |
| `PullRequest::to_cove` | 1,211 | 21 | 1,032 |
| `Decision::from_cove` | 440 | 5 | 360 |

Both are written the obvious way — build the `Vec` of fields, match the enum,
clone the string — rather than the fast way, because what an embedder writes is
what an embedder pays. Inbound costs 2.8× outbound, and the reason is the
shape rather than the direction: ten fields and two arrays go in, and a policy
and a short list of findings come out.

## Gaps

Each of these is something this example wanted and the toolchain does not have.
They are issues rather than workarounds in the program.

- **An exported function cannot be invoked with host-supplied arguments.**
  `Vm::run_entry` and `Interpreter::run_entry` take a `Vec<Rc<str>>` — the
  process arguments an entry may declare — so there is no way for a Rust host
  to call `decide(pr)` with a `Value` it built. The input has to arrive by a
  Host API call instead, which is why `rules.embedded` exists at all.
  [#150](https://github.com/myuon/cove/issues/150)
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
  registry, a `Runtime` and a `Vm` for it — the 43% above.
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

`cargo test -p cove-rules` runs fourteen more, in `host/tests/embedding.rs`:
one compiled package deciding six requests, both backends agreeing on the
embedded entry, the Rust fixtures and the Cove fixtures agreeing, an additive
schema change, a breaking one, an argument and a result the boundary refused, a
capability that was not granted, a host that failed, a session that kept
serving, fuel, a host-call limit, the request identifier in the trace, and the
conversion following the schema's field order.

The fixtures are written twice — once in `rules.fixtures` and once in
`cove_rules::samples` — because in a real embedding they arrive from the
application, and two copies of anything drift.
`the_host_s_fixtures_and_the_package_s_agree` is what holds them together: the
decision reached over the host's copy, through the boundary, must equal the
decision reached over the package's own, which crosses nothing.
