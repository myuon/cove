# ADR 0005: Module-to-module imports

- Status: Accepted
- Date: 2026-08-25

## Context

The Language Card leads its module section with the rule that "an `export`
declaration is public; other declarations are module-private". That rule is
decorative today. No module can name another module's declaration at all, so
nothing is ever refused for being private, and `export` only affects what
`cove outline` prints.

`use` currently means one thing: a host module, optionally with one operation
named for unqualified use. A path with three or more segments is a hard error
that says module-to-module imports are not supported yet.

The cost compounds. Capability derivation and match exhaustiveness are
intra-module because a call cannot leave a module. `cove api diff` and
`cove impact` describe relationships between modules that cannot exist. A
package is currently a set of islands.

## Decision

A module may name another module's exported declarations, with the same `use`
syntax it already has.

### One syntax, disambiguated by what exists

```cove
use booking.create           // the module `booking`, item `create`
use console.println          // the host module `console`, operation `println`
use src.booking.createBooking
```

`use` takes a dotted path. The compiler resolves it against the package's
modules first and the host registry second. A path that matches neither is an
error naming both things it looked for. A path that matches both is an error;
the fix is to rename the module, since the host namespace is not the package's
to change.

Resolving modules first matters: a package's own structure should not change
meaning because a host gained an operation.

The alternative — a distinct keyword for host imports — was rejected. The card
says the same source runs as a CLI, a server, or an embedded guest, and the
host changes the available authority, not the language's meaning. A syntax
that marks host imports differently would make the boundary a source-level
concern rather than an execution-time one.

### Only exports are visible

A `use` naming a declaration that exists but is not exported is an error that
says so and points at the declaration. This is the rule the card already
states; it simply becomes enforceable.

### No cycles

A module may not import, directly or transitively, a module that imports it.
Cycles are reported with the path that forms them.

ADR 0001 lists "how are dependency cycles represented and diagnosed" as an
open question. This answers it for the MVP by forbidding them: a package whose
modules form a cycle has a structure its author can see and fix, and
supporting cycles costs ordering guarantees the compiler currently gets for
free.

### Imports do not execute anything

Unchanged from the card, and now worth restating because it becomes possible
to violate: importing a module runs none of its code. There are no module
initializers, so import order is not observable.

## Consequences

Capability derivation becomes cross-module, and its call graph becomes the
package's rather than a module's. Match exhaustiveness can see an enum
declared elsewhere. The type checker gains an import environment. Each of
those is a narrowing of an existing approximation, not a new mechanism.

`cove outline` and API snapshots become meaningful: a module's exported
interface is now something another module depends on, so changing it can
break a caller within the same package.

Diamond imports are ordinary and need no special treatment, since a module has
no state and importing it does nothing.
