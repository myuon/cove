# Representative programs

These programs are executable design tests for Cove. The compiler does not
exist yet, so the syntax is provisional.

Together they test the core product hypotheses:

| Program | What it validates |
| --- | --- |
| `hello/` | Familiar syntax and ordinary CLI ergonomics |
| `config/` | `Option`, `Result`, typed configuration, and explicit errors |
| `server/` | A useful HTTP service without framework ceremony |
| `restricted/` | Host-provided capabilities and denied ambient authority |
| `tasks/` | Structured concurrency, cancellation, and trace boundaries |
| `values/` | Struct value copies, shared collection handles, and explicit snapshots |
| `callbacks/` | Routers, middleware, events, timers, retries, and task-safe captures |
| `cove.toml` | Host-selected entry functions and granted capabilities |

Each directory is a module; declarations marked `export` form its public API.
The first implementation milestone should make `hello/` run. The MVP is
not complete until all seven programs have defined behavior in both diagnostics
and execution.

## Value-and-handle diagnostics

A read-only place cannot call a mutating method:

```cove
let booking = Booking(...)
booking.confirm()
// error: `Booking.confirm` requires a mutable receiver
```

Struct assignment is a field-wise shallow copy. Value fields become independent;
List, Map, Set, closure, and Host-resource fields remain shared handles. Assignment and argument passing use this same rule everywhere.

```cove
var copy = booking
copy.status = Confirmed      // changes only copy.status
copy.guests.push("Alice")    // visible through both List handles
```

An independent transitive snapshot is explicit:

```cove
var snapshot = booking.copy()
snapshot.guests.push("Bob")  // not visible through booking
```

Collection mutation during iteration is rejected. Mutable handles and structs
containing them are not map keys without a stable key representation.

## Callback-model findings

Callbacks are ordinary handle values. Routers, event buses, timers, structs,
and closures store them through ordinary O(1) shallow copying.

A closure may freely outlive the call that created it, but mutable state cannot
cross a task boundary unless it uses a synchronized type such as `Shared<T>`.
The callback example therefore uses `Shared<Metrics>` for request counters
while ordinary application and repository handles are shallow-copied into
closures.
