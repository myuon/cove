# Representative programs

These programs are executable design tests for Cove. The syntax is still
provisional.

`hello`, `config`, `values`, `restricted`, and `traits` run today. `server`,
`tasks`, and `callbacks` parse, resolve, and report their required
capabilities, but stop at execution because asynchronous functions are not
implemented yet.

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
| `text/` | Not a program: the module `restricted/` imports, for `export` and capabilities across a boundary, and the package's own `test fn` declarations |
| `cove.toml` | Host-selected entry functions and granted capabilities |

Each directory is a module; declarations marked `export` form its public API.
A module may name another module's exported declarations with `use`, so
`text/` is not a program of its own: `restricted/` imports it, and reaches
`console` only through `text.report`. That is why `cove outline` reports
`restricted.main` as requiring `console` although it names no host module —
required capabilities are derived from the package's call graph, not one
module's.
The first implementation milestone, making `hello/` run, is done. The MVP is
not complete until all eight programs have defined behavior in both diagnostics
and execution.

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

A closure may outlive the call that created it, but it can cross a task boundary
only when every capture is task-safe. Host resources such as the booking
repository declare that property in their Host API schema.
