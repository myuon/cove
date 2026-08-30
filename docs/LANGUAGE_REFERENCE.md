# Cove Language Reference

> The implemented core, one rule per form. The
> [Language Card](LANGUAGE_CARD.md) is the map; this is the account you can
> hold an implementation to.

Cove has three descriptions of what a program means: the checker in
`crates/cove-sema`, the interpreter in `crates/cove-runtime`, and prose. Prose
is the one that cannot be run, so it is the one that drifts. This document
exists so that there is exactly one place a rule is written, and
`crates/cove-runtime/tests/conformance.rs` exists so that the other two are
held to it.

For each form below:

- **Resolves to** — what a name or path in the form denotes.
- **Types as** — what `cove check` gives it.
- **Evaluates to** — what the reference interpreter produces.
- **Errors** — the diagnostic codes it can produce, and the run-time failures
  it can raise.

The reference interpreter is the semantic oracle for what a program *does*.
It is not the oracle for what a program *means*: where the two passes
disagreed, this document decided, and both were changed to obey it. Those
decisions are listed first, because they are the only rules here that were
chosen rather than recorded.

## Decisions this document settles

**An `if` with no `else` produces `()`.** The value of the branch that runs
is discarded. There is no second branch to give the missing case a value, so
the branch that does run does not supply one either; a form whose type
depended on which way its condition went would be a form the checker could
not type. `if c { 1 }` is therefore `()` whether `c` holds or not, and
`let n: Int = if c { 1 }` is a `cove::type::mismatch`.

**Every loop produces `()`, and a `break` operand is discarded.** A `for`
runs out of items and a `while` runs out of condition, so a loop can reach its
end without breaking, and there is nothing at that end to produce but `()`. A
loop is therefore `()` however it leaves. `break expr` is accepted, its
operand is checked and evaluated for its effects, and its value is discarded —
the same answer this document gives an `if` with no `else`, for the same
reason. `while true` is an ordinary `while`: nothing about the condition makes
it a different form in either pass.

Whether Cove should have a loop that carries a value at all — a `loop`
keyword, an `Option<T>`-valued loop, a `for`/`else` clause, or none of them —
is [issue #87](https://github.com/myuon/cove/issues/87), and is deliberately
left open here. This document records only that today the two passes agree.

**A `dyn Trait` value answers as the value it holds.** The conversion happens
where a `dyn Trait` type is *written* — a `let` annotation, a field, a
parameter, a declared return type — and a lambda has no written return type,
so its result is not wrapped even though the checker gives it the same
`dyn Trait` type. Nothing a program can ask may tell those two apart, so
equality, rendering, and keying all look through the wrapper: two values `==`
calls equal are usable in the same places. Two trait objects holding different
concrete types are unequal, not incomparable — but a trait object compared
with anything that is not a trait object's contents is still incomparable,
because the wrapper explains that one mismatch and no other.

## Program shape

A `.cove` file is a `SourceUnit`: `use` declarations, then items. Every file
in a directory is an implementation unit of one module, named after its path.
An item is a `fn`, `struct`, `enum`, `trait`, `impl`, or `type` alias, and is
module-private unless `export`ed. `opaque` is a modifier on an exported
struct: `export opaque struct` narrows what the export publishes to the
type's name and its exported methods and associated functions, withholding
the fields and the labeled constructor they synthesize (see Opaque structs,
below). `test fn` sits where `export` sits and excludes it.
`impl Trait for Type` is the only way a conformance is declared.

Declaration-level errors: `cove::resolve::duplicate_declaration`,
`cove::resolve::unknown_trait`, `cove::resolve::unknown_impl_type`,
`cove::resolve::foreign_inherent_impl`, `cove::resolve::duplicate_conformance`,
`cove::resolve::orphan_conformance`, `cove::resolve::unknown_trait_method`,
`cove::resolve::missing_trait_method`, `cove::resolve::invalid_impl_item`,
`cove::resolve::import_conflict`, `cove::resolve::ambiguous_use`,
`cove::resolve::unknown_use`, `cove::resolve::private_declaration`,
`cove::resolve::import_cycle`, `cove::resolve::module_shadows_host`,
`cove::parse::opaque_not_exported`, `cove::parse::opaque_not_a_struct`,
`cove::type::missing_parameter_type`, `cove::type::conformance_signature`,
`cove::type::test`, `cove::type::entry`, and the
`cove::resolve::missing_doc` warning.

## Statements

A block is a sequence of statements and an optional tail expression. A
statement is one of:

- **`let name[: T] = value` / `var name[: T] = value`.** Declares a binding in
  the enclosing block's scope, shadowing anything of that name outside it. A
  written `T` is what `value` is checked against; without one the binding
  takes `value`'s type, except that an initializer that never produces a value
  has no type to give and the binding becomes unknown. `let` makes a
  read-only place and `var` a mutable one, which `cove check` enforces: a
  write to a read-only place is `cove::type::read_only_place`.
- **An expression.** Evaluated for its effects; its value is discarded and its
  type is unconstrained. A non-`()` expression in statement position is not an
  error.
- **A nested item.** Only a local `fn`, which is checked as an independent
  function: its own return type, its own type parameters, and its own loop
  and `return` context, nested inside the enclosing scope for name lookup.

A **block** is an expression. Its type and value are its tail's; a block with
no tail is `()`. It pushes a scope on entry and pops it on exit, whichever way
it leaves.

## Expressions

### Literals

| Form | Types as | Evaluates to |
| --- | --- | --- |
| `1` | `Int` | a 64-bit signed integer |
| `1.5` | `Float` | a 64-bit IEEE 754 double |
| `true` | `Bool` | a boolean |
| `500ms` | `Duration` | nanoseconds, rendered in the largest unit that divides it exactly |
| `"a{x}b"` | `String` | the parts concatenated left to right |
| `()` | `()` | the unit value |
| `[1, 2]` | `Array<T>` | a fresh immutable array |

A literal's written form fixes its type: there is no untyped numeric literal,
so an `Int` literal never becomes a `Float` where one is expected.

An interpolation `{expr}` renders any value and constrains none, so it is
checked but not required to be a `String`.

An array literal takes its element type from the expectation when there is
one, and otherwise from its elements, each checked against what the ones
before it inferred; an empty literal's element type is unknown. Elements are
evaluated left to right.

Errors: `cove::type::mismatch` on an element that disagrees.

### Names

`name`

- **Resolves to**, in order: a binding in scope (innermost first); the builtin
  `None`; a function of this module or one it imported, as a value; a struct,
  enum, builtin type, host module, imported module, or `use`d host item.
- **Types as** the binding's type, or the function's function type. A bare
  `None` takes its argument from the expectation and is `Option<_>` without
  one. A type or module used as a value is not a form this type system has, so
  the checker abstains.
- **Evaluates to** the binding's value, a closure, a `Type`, a host module or
  host function handle.
- **Errors**: `cove::type::unknown_name` for a lowercase name nothing in scope
  explains; the `cove::type::unresolved_name` warning for a capitalized one,
  which is assumed to come from the host. At run time, `cannot find <name> in
  this scope`, or `<name> is a module, not a value`.

### Field access

`base.name`

- **Resolves to** a struct field, an enum case (`Status.Pending`), a host
  module's member or type, or an imported module's declaration — the
  qualified readings are tried before `base` is evaluated as a value.
- **Types as** the field's type with the struct's type arguments substituted,
  the case's enum type, or the host schema's field type. A trait declares
  methods and not fields, so a `dyn Trait` and a type parameter have none.
- **Evaluates to** a copy of the field's value.
- **Errors**: `cove::type::unknown_field`, `cove::type::unknown_case`,
  `cove::type::unknown_member`, `cove::type::opaque_field`, the
  `cove::type::host_type` warning. At run time,
  `<type> has no field <name>`.

### Calls

`f(a, label: b)`, `Type(field: value)`, `receiver.method(...)`,
`f { trailing }`

- **Resolves to** a module function, a struct initializer, an enum case
  constructor, a method on the receiver's type, a trait method through a
  bound or through `dyn Trait`, a builtin, a host operation, or the value the
  callee expression produces.
- **Types as** the callee's result. Calling an `async fn` gives `Task<T>`. Type
  arguments come from `<...>` when written and otherwise from the arguments,
  matched one at a time so that each argument is checked against the
  signature as far as it is known — which is how a lambda argument gets its
  parameter types. A type parameter no argument constrains stays unknown
  rather than being reported.
- **Evaluates to** the callee's result. Arguments are evaluated left to right
  in source order, and a trailing closure last. A default is used for a
  parameter no argument fills. A variadic parameter is an immutable
  `Array<T>` inside the callee.
- **Errors**: `cove::type::arity`, `cove::type::missing_argument`,
  `cove::type::unknown_label`, `cove::type::mismatch`,
  `cove::type::not_callable`, `cove::type::unknown_method`,
  `cove::type::unknown_associated_function`,
  `cove::type::opaque_construction`, `cove::type::payload_arity`,
  `cove::type::receiver`, `cove::type::unsatisfied_bound`,
  `cove::type::unbounded_parameter`, `cove::type::dyn_associated_function`,
  `cove::type::dyn_mutating_method`, `cove::type::unknown_host_operation`,
  `cove::type::task_safety`, `cove::type::label_order`,
  `cove::type::read_only_place`, `cove::type::not_a_place`. At run time,
  `<value> is not callable`, `<type> has no method <name>`, a `var` argument
  on a parameter that is not `var`, a refused capability, and every failure a
  host operation or a builtin can raise.

  Labels appear in declaration order, and a call whose labels do not is
  `cove::type::label_order`. A `var` argument and a `var self` receiver name
  the caller's own place, so each must *be* a place — `cove::type::not_a_place`
  otherwise — and a writable one — `cove::type::read_only_place` otherwise.

### Operators

`-x`, `!x`, `a + b`, `a == b`, `a is b`, `a && b`

- **Types as**: `!` needs `Bool` and gives `Bool`; `-` needs `Int`, `Float`,
  or `Duration` and gives it back. `+ - * / %` need both operands to be the
  same type, and are defined for `Int` and `Float`; `Duration` has `+` and `-`
  only; `String` has none, because there are no implicit string conversions.
  `== !=` need the two types to agree and give `Bool`. `< <= > >=` need the
  two types to agree and to be `Int`, `Float`, `Duration`, or `String`.
  `&& ||` need `Bool` operands. `is` needs the two types to agree and to be
  `Vector`.
- **Evaluates to**: operands left to right, except that `&&` and `||`
  short-circuit and do not evaluate the right operand when the left settles
  the answer. `==` is structural: `Vector` compares its current elements,
  a resource compares the resource it names, and a value with no contents to
  compare — a closure, a task, a `Shared` — is equal to nothing, itself
  included. `is` compares shared storage, which only a `Vector` has.
- **Errors**: `cove::type::operator`. At run time, `` `Int` addition
  overflowed`` and its siblings, `` `Int` division by zero``, and
  `identity is not available for <type>`.

`Int` arithmetic traps rather than wrapping, and traps on division or
remainder by zero. `Float` arithmetic is IEEE 754 and traps on nothing:
`1.0 / 0.0` is `inf`, `0.0 / 0.0` is `NaN`, and a comparison against `NaN` is
`false`. `Duration` arithmetic traps on overflow.

### Assignment

`place = value`, `place += value`

- **Resolves to** a place: a name, or a field of a place. The parser refuses
  everything else (`cove::parse::invalid_assignment_target`), and a package
  that does not parse never reaches the checker, so the checker's
  `cove::type::not_a_place` is a second net that `cove check` does not reach
  through this door.
- **Types as** `()`. The value is checked against the place's type; a compound
  assignment computes what the operator would produce and checks *that*
  against the place's type.
- **Evaluates to** `()`. A compound assignment reads the place, then evaluates
  the right operand, then applies the operator.
- **Errors**: `cove::type::not_a_place`, `cove::type::mismatch`,
  `cove::type::operator`, `cove::type::read_only_place`. Mutability is not a
  type, but which binding a place is rooted at and how that binding was
  declared are settled by the same scope the checker already walks, so
  `cove check` enforces it: writing a place `let` made read-only is
  `cannot assign to <name>, which is a read-only place`, before the run.

### `?`

`expr?`

- **Types as** the value inside the operand: `T` from a `Result<T, E>`, `T`
  from an `Option<T>`. The enclosing function must return a `Result` whose
  error type is `E`, or an `Option` respectively.
- **Evaluates to** the value inside, and otherwise returns the whole `Err(e)`
  or `None` from the enclosing function — the enclosing *call*, so `?` inside
  a lambda returns from the lambda.
- **Errors**: `cove::type::try_operand`, `cove::type::try_return`. At run time,
  `` `?` needs a `Result` or an `Option` ``; a `Task` operand says to settle
  the task first.

### `await`

`await expr`

- **Types as** `T` for a `Task<T>` operand.
- **Evaluates to** the task's value, waiting for it. A task's body runs at
  most once and is joined at most once, so awaiting the same handle again
  produces the same value without repeating its effects. A task that failed
  raises its error here; a cancelled task has no value to await.
- **Errors**: `cove::type::await_operand`. At run time, `` `await` needs a
  task``, the task's own failure, and cancellation.

`await` binds tighter than `?`, so `await task()?` awaits and then propagates.
An `async fn` called outside a `spawn` runs its body at the call site and
hands back an already-settled task: concurrency comes from `spawn`, not from
`async`.

### `if`

```cove
if condition {
  ...
} else {
  ...
}
```

- **Types as**: the condition must be `Bool`. With an `else`, both branches
  are checked against the surrounding expectation when there is one and
  against each other when there is not, and the `if` is their common type.
  **With no `else`, the `if` is `()`** and the branch is checked against
  nothing.
- **Evaluates to** the value of the branch that ran — **and `()` when there is
  no `else`**, whichever way the condition went. The branch still runs and its
  effects still happen; only its value is discarded.
- **Errors**: `cove::type::condition`, `cove::type::branches`,
  `cove::type::mismatch`. At run time, `an `if` condition must be a `Bool``.

`else if` is an `else` whose expression is another `if`, so a chain that ends
without a final `else` ends in an `if` worth `()`, and the chain's branches
must therefore all be `()` too.

### `match`

```cove
match scrutinee {
  pattern => body
}
```

- **Types as**: the scrutinee is checked on its own; each arm's pattern is
  checked against the scrutinee's type and binds into that arm's body; the
  arms are checked against the surrounding expectation when there is one and
  against each other when there is not. A `match` with no arms is `Never`.
- **Evaluates to** the body of the first arm whose pattern matches. The
  scrutinee is evaluated once.
- **Errors**: `cove::resolve::non_exhaustive_match`,
  `cove::resolve::duplicate_match_arm`, `cove::resolve::unknown_enum_case`,
  the `cove::resolve::unreachable_match_arm` warning, `cove::type::branches`,
  `cove::type::pattern`, `cove::type::payload_arity`. At run time,
  ``no `match` arm covers <value>``.

Exhaustiveness is proved over enum cases and over `Bool`. A `match` over `Int`
or `String` literals can only be exhaustive through a `_` or a binding arm.
Where resolution cannot tell which enum a bare case name belongs to — two
enums in scope declaring the same case — it abstains, and a `match` with no
arm for the value is a run-time failure instead.

An arm is a duplicate when an earlier arm matches every value it would, which
is a question about the whole pattern and not about the case it names: a
sub-pattern that binds or is `_` covers its case, and one that does not covers
only what it matches. `Some(other)` after `Some(value)` is a duplicate;
`Some(other)` after `Some(Json.Text(value))` is not, and neither is
`Some(Json.Text(a))` after `Some(Json.Number(b))`. Coverage is decided one arm
against one earlier arm, so arms that only together leave a later one nothing
are not reported — that is exhaustiveness over a payload, which nothing
proves.

### Loops

```cove
for binding in iterable { ... }
while condition { ... }
```

- **Types as**: a `for` iterates an `Array<T>`, `Vector<T>`, `Set<T>`,
  `Range`, or `Map<K, V>`, binding `T`, `Int`, or `MapEntry<K, V>`
  respectively. A `while` condition must be `Bool`. Every loop is `()`,
  `while true` included.
- **Evaluates to** `()`, however it leaves. A `break` operand is evaluated for
  its effects and its value discarded. A `for` materializes its iterable once,
  before the first iteration, and iterates that snapshot in the collection's
  own order — ascending key order for a `Map`, sorted order for a `Set`. The
  loop back edge is a safepoint, which is where a cancelled or over-budget run
  stops.
- **Errors**: `cove::type::iterable`, `cove::type::condition`,
  `cove::type::mismatch`. At run time, the condition not being a `Bool`, and
  the run's own limits.

`break [expr]` leaves the nearest enclosing loop, and `continue` skips to its
next iteration. Neither produces a value of its own, so both are `Never`.
Neither may cross a closure boundary, exactly as a `return` inside a lambda
returns from the lambda: `cove::resolve::break_outside_loop` and
`cove::resolve::continue_outside_loop`.

### `return`

`return`, `return expr`

- **Types as** `Never`. The operand is checked against the enclosing
  function's declared return type; `return` with no operand is checked as
  `()`. Inside a lambda, the enclosing function is the lambda, and a lambda
  with no expected type has no signature to check the operand against.
- **Evaluates to** nothing: it unwinds to the nearest enclosing call, which
  is the lambda's own call when it is written in one.
- **Errors**: `cove::type::mismatch`.

### Lambdas

`fn(x) { ... }`, `async fn(x) { ... }`

- **Types as** a function type. A parameter with no written type takes it from
  the expected function type at the call or binding that gives the lambda one;
  a parameter that writes its own uses that. The result is the expected
  function type's result when there is one, and the body's value otherwise.
- **Evaluates to** a closure holding a **snapshot** of every captured
  binding's value, read where the lambda is written. Assigning to the outer
  binding afterwards does not change what the closure sees. A captured
  `Vector` or `Shared` still shares its storage, because copying either is
  copying the handle.
- **Errors**: `cove::type::arity`, `cove::type::mismatch`.

### `scope`

```cove
scope tasks {
  let task = tasks.spawn { ... }
  await task
}
```

- **Resolves to** a binding of the scope's name, shadowing anything outside
  it, whose only operation is `spawn`.
- **Types as** its body's type, and the scope name is a type the language
  gives no name to.
- **Evaluates to** its body's value. Leaving the scope normally waits for
  every child task in the order they were spawned, and a child's failure
  becomes the enclosing function's; leaving it early cancels them.
- **Errors**: at run time, `spawn` without a trailing closure, a capture that
  cannot cross a task boundary, the concurrency limit, and a child's own
  failure.

### Ranges

`0..<n`, `0..n`

- **Types as** `Range`; both bounds must be `Int`.
- **Evaluates to** a range value. `a..b` includes `b` and `a..<b` excludes it.
- **Errors**: `cove::type::mismatch`. At run time, a bound that is not an
  `Int`.

## Patterns

| Form | Binds | Types as | Matches |
| --- | --- | --- | --- |
| `_` | nothing | anything | always |
| `other` | `other` | the scrutinee's type | always |
| `1`, `"a"`, `true`, `-n` | nothing | must match the scrutinee's type | equal values |
| `Ok(v)`, `Status.Active(n)` | its sub-patterns | the case's payload types, opened with the scrutinee's type arguments | the same case |

A pattern's bindings are immutable and scoped to its arm. A binding pattern
and `_` are both catch-alls, so an arm after either is unreachable. A variant
pattern's path may be bare (`Ok`), qualified by its enum (`Status.Active`), or
module-qualified; a bare case name that two enums in scope both declare is one
resolution abstains about.

A "literal" pattern is a literal token — or a `-` followed by anything `-`
applies to, which is more than the name suggests. That second form is an
ordinary expression, checked like one and evaluated in the arm's enclosing
scope every time the pattern is tried, so `-n` matches the negation of
whatever `n` currently holds and `-f()` calls `f`. This is recorded here
because it is what both passes do, not because it is a form the language set
out to have; narrowing it to a negated numeric literal would be a change to
the language rather than a fix to either pass.

Errors: `cove::type::pattern`, `cove::type::payload_arity`,
`cove::resolve::unknown_enum_case`. At run time, a payload arity the checker
did not see.

## Types

Types are nominal, and there is no subtyping and no variance: two types are
equal when they name the same declaration and their arguments are equal, so
`Array<Booking>` is not an `Array<dyn Display>`. Generic arguments are
invariant everywhere.

Two things are not types a program can write. *Unknown* is the checker
declining to answer; it matches every type so that one gap does not become a
cascade of wrong errors. *Never* is the type of an expression that produces no
value — `return`, `break`, `continue` — and it matches every type so that an
arm which never produces a value never disagrees with one that does.

### The one implicit conversion

A concrete value is accepted where a `dyn Trait` is expected, exactly when it
declares a conformance to that trait. That is the only implicit conversion in
the language, and:

- it runs one way only — a `dyn Trait` value never becomes a concrete type,
  and never converts to another `dyn Trait`;
- it never reaches inside a generic argument;
- it satisfies no bound, not even its own trait's.

Only the trait's plain `self`-taking methods can be called through it: an
associated function has no receiver, and a `var self` method needs the
caller's own place.

At run time the conversion wraps the value, and the wrapper is a
representation rather than something the program put there: rendering,
equality, and use as a `Map` key or `Set` element all look through it, so a
written `dyn Trait` and a lambda's inferred one behave identically. A trait
object is a valid key exactly when the value it holds is one, and it keys as
that value — which is what `==` already says about the two of them.

### Opaque structs

`export opaque struct Name { ... }` exports the type's name and its exported
methods and associated functions, and withholds its fields and the labeled
constructor they synthesize — a struct's fields are otherwise as public as
its name, so this is a modifier that narrows an export rather than a second
kind of one. [ADR 0014](adr/0014-opaque-exported-types.md) is the record of
why.

Inside the module that declares it, an opaque struct is an ordinary struct:
`name.field` reads and `name.field = value` writes it, and the synthesized
labeled constructor `Name(field: value, ...)` is callable, exactly as for a
struct without `opaque` — because `opaque` describes a boundary between
modules, and the declaring module is not on the far side of its own
boundary. From any other module, only the name and the exported methods and
associated functions resolve; naming a field or calling the constructor
there is refused rather than typed:

- reading or assigning a field is `cove::type::opaque_field`;
- calling the labeled constructor is `cove::type::opaque_construction`.

A refusal ends the diagnosis: the checker does not go on to match the call
against fields the caller may not name, so a refused construction carries no
"known labels" list and no `cove::type::missing_argument` for a field it
withheld, and neither diagnostic quotes source from the declaring module. An
argument written inside a refused construction is still checked on its own,
since a mistake inside one is a mistake either way. The refused form's own
type is a recovery *unknown* — an error was already reported, so nothing
further is said about it.

An opaque value renders as its bare type name and nothing else —
`"{user}"` is `User` — unconditionally, including inside the declaring
module, because a rendered string carries no module with it once it exists.
The same holds wherever a value is rebuilt from its parts rather than
handled directly: use as a `Map` key or `Set` element, and a crossing of a
task boundary. Opacity composes with the `dyn Trait` wrapper rather than
being undone by it: the wrapper is looked through first, so a `dyn Trait`
holding an opaque value keys and renders as the value it holds — which is
still opaque, and so still keys and renders as its bare type name. A module
that wants its type to have a readable form exports a method returning one,
which is the same answer as for a field: what the module publishes is what
it wrote down.

There is no opaque enum: exporting an enum always exports its cases, because
a `match` a caller cannot write is not a smaller enum but a worse struct. A
variant whose representation should be hidden is wrapped in a struct and
exported as opaque instead.

## Copies, aliases, and identity

Assignment and ordinary argument passing are field-wise shallow copies.
Primitives, strings, enums, and structs have value semantics; `Array`, `Map`,
and `Set` share their storage, which is unobservable because none of them can
be mutated. `Vector` and `Shared` share storage that *can* be mutated, so a
copy of either is an alias: mutation through one is visible through every
other. `is` asks which two handles are the same storage, and `Vector` is the
one type it is defined for today. Cove performs no
implicit deep copy; `vector.freeze()` and `vector.toArray()` produce an
independent array, and any other independent copy is an explicit
`impl Snapshot for Type`.

A `var` parameter is a non-escaping inout alias, marked at the declaration and
at the call site. A mutating receiver is `var self`.

## Tasks

Concurrent work belongs to a task scope. A value crosses into a task by copy
when it is task-safe, which primitives, `String`, `Range`, `Array`, `Set`,
`Map`, structs, enums, trait objects, and closures are when everything they
hold is; a `Shared` crosses by sharing, which is the one exception to the copy
rule. A `Vector`, a task, a scope, and a host resource that declares itself
unsafe never cross. The same check runs on the value a task produces.

`cove check` states one part of this — `Shared<T>` needs a task-safe `T`
(`cove::type::task_safety`) — and leaves the rest to the boundary the value
crosses, because a resource's declared task-safety is not part of its type.

## Where the checker abstains

The checker produces *unknown* deliberately, never as a shrug, and only here:

- a host module no schema describes — the toolchain ships schemas for its own
  modules and an embedding hands over its own, and what neither names is left
  to the boundary — and a host operation whose schema declares `Any`;
- a capitalized name nothing in scope explains, which is assumed to come from
  the host (`cove::type::unresolved_name`, `cove::type::unknown_type`);
- a type or a module used as a value: `Vector` in `Vector.of(1, 2)` is
  understood as part of the call, but a bare `Vector` is not a form with a
  type;
- a type parameter no argument at the call site constrains;
- an empty array literal's element type, and a bare `None` with no expectation
  to take an argument from;
- a lambda's `return` when the lambda has no expected type, because there is
  no written signature to check it against.

An unknown matches every expectation, so a form in this list is accepted
wherever it is written.

The conformance suite does not pin this list. One entry was pinned — a bare
`Vector`, compiled at a foreign type and required to be *accepted* — and that
rule is gone, because [PR #82](https://github.com/myuon/cove/pull/82) makes a
type used as a value an error (`cove::type::not_a_value`). What is left is a
description of the checker as it stands, kept by hand.

It is about to change, and knowing how is worth more than pretending it will
not. PR #82 replaces "abstains" with four named kinds — recovery, a dynamic
boundary, an unconstrained API, and a language gap — and moves four of the six
bullets above out of the silent case: a name nothing in scope explains and a
lambda `return` with no expected type become errors
(`cove::type::unresolved_name`, `cove::type::unknown_name`,
`cove::type::unknown_type`, `cove::type::lambda_return`), a type or module
used as a value becomes `cove::type::not_a_value`, and an empty array literal
or a bare `None` with nothing to take an argument from warns
(`cove::type::unconstrained`). This section is rewritten against that model
when PR #82 lands, rather than amended twice into something that is neither.

One more type is not unknown but has no name either: the value a `scope`
binds. Its only operation is `spawn`, which is typed, and there is nothing
else to write it as.

## What the run time decides

One family of rule is the interpreter's alone, because deciding it needs the
run: every capability, budget, and task-safety decision at the host and task
boundaries. A program can pass `cove check` and fail on it.

Two families used to stand here beside it — which places source may write,
and whether labeled arguments stand in declaration order. Neither is about a
type, and both are decided by the scope the checker already walks, so ADR
0021 moved them to `cove check`. A program that breaks either does not
compile.

One rule about a call still waits for the run, and it is named where it is
enforced rather than here: a `var` marking that disagrees between the
declaration and the call site. It is decidable from the source too, and the
checker does not decide it yet, because a function *type* carries no marking
for a call through a value to be checked against.

The capability rule is why a derived capability requirement is a lower bound
rather than an exact set. `cove check` follows the calls it can see; a
declaration that reaches a call it cannot follow — a call to a function value,
or a method dispatched through a `dyn Trait` or a bounded generic parameter,
including one read out of a struct field whose declared type is or contains a
`dyn Trait` — is marked **capability-open**, because what it will run is
chosen by its caller. The marker is what the derived set does not cover, not a
capability of its own: it names the calls a program may make beyond those the
compiler counted, and the boundary is what decides either way. `cove outline`,
`cove api`, `cove test`, and `cove impact` all report it, and gaining it is a
breaking change in `cove api diff`
([ADR 0015](adr/0015-capability-analysis-for-higher-order-calls.md)).

Two more are the language's own limits rather than gaps: exhaustiveness over
`Int` and `String` needs a catch-all arm, and a bare case name two enums share
is one resolution cannot place.

## Conformance

`crates/cove-runtime/tests/conformance.rs` holds this document to the
implementation:

- every rule's body is compiled at the type stated here, which must be
  accepted, and at a foreign type, which must be refused as a
  `cove::type::mismatch` — so the type above is the type the form has, and not
  merely one it fits;
- the same body is run, and the value it produces must be the one stated here;
- every rejected program is refused by the diagnostic named here;
- every run-time failure named here checks cleanly and stops the run;
- every expression and pattern form the AST declares appears in an *accepted*
  program in the suite. The list of forms is generated from `ExprKind` and
  `PatternKind` themselves and the programs are parsed and walked, so adding a
  form to the language without covering it here does not compile.

`tests/e2e/` pins the same rules as whole programs run through the real
binary, and `docs/adr/` records why the rules are what they are.

A future backend is a Cove implementation exactly when it passes this suite.
