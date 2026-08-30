# Representative programs

These programs are executable design tests for Cove. The syntax is still
provisional.

All ten run today; asynchronous execution is no longer what blocks any of
them -- tasks run on threads.

What a run of the last three does depends on the hosts it was given, and
`cove run` gives it the real ones. `cove run server` binds port 8080 and
serves until it is interrupted. `cove run callbacks` gets no further than its
first line: `cove run` installs the denied `database`, so `database.connect`
refuses and says so, because there is still no real one. `cove run tasks`
fetches `http://127.0.0.1:8080/bookings` and `/prices`, so on its own it
reports a connection error; run `server` first and it has something to reach,
though `server` routes only `/health`, so what comes back is a pair of 404s
rather than bookings and prices. The two are not written to compose.

`crates/cove-cli/tests/examples.rs` runs every one of the ten against
deterministic fake hosts instead -- a listener with a scripted queue of
requests, a `fetch` with recorded answers, a clock that moves only when
something moves it, and a database of canned rows -- which is where their
behavior is actually pinned.

There is one assertion those tests cannot make. `callbacks` spawns a repeating
report timer and cancels it once the server's listener runs dry, and how many
times it fires is decided by whether the operating system starts the timer's
thread before the cancellation reaches it, because `clock.every` reads its
task's cancellation flag before doing anything else. That is settled rather
than open. [ADR 0008](../docs/adr/0008-concurrent-task-execution.md)'s
amendment decides that a `spawn` starts a task and orders nothing else, and
names the three ways the count could have been made decidable -- a rendezvous
at `spawn`, a clock a test steps, a `clock.every` that reports its rounds --
with the reason each was refused.

So the test asserts what the program decides. The fake clock offers at most one
round, so at most one report line may appear; a line that does appear is one of
the three the program could print, since the timer sees zero, one, or two
requests recorded by then and neither of these routes fails; and no count is
asserted. `clock.every`'s own behavior is pinned exactly by the unit tests in
`crates/cove-runtime/src/clock.rs`, which drive it with no second thread to
race.

`cq/` is the odd one out and is meant to be. The others are each a few dozen
lines aimed at one hypothesis; `cq/` is a program of a size somebody might
actually write, and its purpose is to find out whether writing one is
comfortable. `examples/cq/README.md` is where that answer lives, along with
wall-clock, heap, allocation, and records-per-second numbers over a
100,000-record input -- the first measurement this repository has of Cove doing
a real job rather than a benchmark. Reading it is the point of it.

`life/` is the second of that kind and asks a different question. `cq/` moves
data through a program; `life/` keeps state and changes it, tick by tick,
which is where what a copy means stops being a curiosity and starts deciding
whether the program is right. Its world is a struct of immutable arrays, so
`let earlier = world` is a snapshot and nothing had to be written to make it
one; its resolution loop hands a `Vector` to a helper and relies on the copy
being an alias. `examples/life/README.md` records both, along with what ten
thousand ticks cost and what they leave on the heap, and the two collection
gaps the program hit on the way ([#154](https://github.com/myuon/cove/issues/154),
[#155](https://github.com/myuon/cove/issues/155)).

Together they test the core product hypotheses:

| Program | What it validates |
| --- | --- |
| `hello/` | Familiar syntax and ordinary CLI ergonomics |
| `config/` | `Option`, `Result`, typed configuration, and explicit errors |
| `server/` | A useful HTTP service without framework ceremony |
| `restricted/` | Host-provided capabilities and denied ambient authority |
| `tasks/` | Structured concurrency, cancellation, and trace boundaries |
| `values/` | Struct copies, Vector aliases, immutable Arrays, and freeze |
| `traits/` | Nominal traits, generic bounds, and both dispatch forms |
| `callbacks/` | Routers, middleware, events, timers, retries, and task-safe captures |
| `cq/` | A whole practical program: streaming JSON Lines and CSV transformation, typed records, actionable diagnostics, and measured throughput. [Its own README](cq/README.md) records what it found |
| `life/` | A deterministic ecosystem simulation: a seeded generator written in Cove, a world that is a value because it holds no Vector, a resolution loop that works because a Vector is a handle, and three species that are modules. [Its own README](life/README.md) records what it found |
| `text/` | Not a program: the module `restricted/` imports, for `export` and capabilities across a boundary, and the package's own `test fn` declarations |
| `codegen/` | `cove generate`: a capability-controlled generator that reads `files/status_codes.txt` |
| `httpstatus/` | Not written by hand: `codegen.statusCodes`'s output, checked in and kept honest by `cove generate --check` |
| `cove.toml` | Host-selected entry functions and granted capabilities |

Each directory is a module; declarations marked `export` form its public API.
A module may name another module's exported declarations with `use`, so
`text/` is not a program of its own: `restricted/` imports it, and reaches
`console` only through `text.report`. That is why `cove outline` reports
`restricted.main` as requiring `console` although it names no host module —
required capabilities are derived from the package's call graph, not one
module's.
The first implementation milestone, making `hello/` run, is done, and so is
the one that follows it: all ten programs now have defined behavior in
both diagnostics and execution, whether that behavior is a clean run, like
`hello`'s, or a documented refusal, like `callbacks`' immediate stop when
`database.connect` finds no real database behind `cove run` to connect to.

## Collection lifecycle

Array literals produce immutable fixed-length data. A Vector is the explicit
growable form and may be bound through either `let` or `var`.

```cove
let finished = [1, 2]
var building = Vector.of(1, 2)
building.push(3)
```

Vector aliases share elements and length. `freeze()` consumes locally unique
Vector storage into an Array in O(1); `toArray()` is the O(n) fallback when
uniqueness cannot be proved. Vectors cannot cross task boundaries. Arrays can
when their elements are task-safe.

Ordinary parameters receive shallow copies. A `var` parameter is a
non-escaping inout alias and is marked at both declaration and call site.

Calls and struct initialization use static argument labels. A homogeneous
variadic parameter `items: T...` is an immutable Array inside the function, so
`Vector.of(1, 2)` is an ordinary user-definable associated function rather
than a special literal.
Independent mutable graph copies exist only for types implementing
`Snapshot`.

## Callback-model findings

Callbacks are ordinary handle values. The example builds routes in a local
Vector, freezes them into an Array, and only then starts request tasks. Event
subscriptions use an immutable Map of Arrays. Mutable request metrics use
`Shared<Metrics>`.

A closure may outlive the call that created it, but it can cross a task
boundary only when every capture is task-safe. `callbacks` captures a
`database.Connection` handle -- bundled inside `App` -- in the closure
`services.spawn` runs on a new task to drive the report timer's
`clock.every` body, and that crossing is allowed only because `database`'s
`ResourceSchema` declares `Connection` task-safe: what a handle names lives
behind the host's own lock, so two tasks holding the same handle take
turns rather than racing.
