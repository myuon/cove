# ADR 0017: An embedder's Host API schemas are available to the checker

- Status: Accepted
- Date: 2026-08-26
- Supersedes: [ADR 0001](0001-mvp-language-design.md)'s account of which host
  modules a compiler can see, which held that the boundary checks a call
  "again for the host modules a compiler cannot see, which is every module an
  embedding registers"
- Implemented by: PR #77
- Implementation status: complete — `HostApi::module_schema`,
  `cove_schema::HostSchemas`, and `cove_sema::Compiler` all exist, and
  `crates/cove-runtime/tests/embedding.rs` checks a program against a module
  no shipped table describes before running it

## Context

ADR 0001 decided that the Host API schema is one table both ends read, and
said what followed from it:

> Both ends read it and both ends enforce it — `cove check` checks a call's
> arguments where they are written, and the boundary checks them again for the
> host modules a compiler cannot see, which is every module an embedding
> registers.

The first half of that is a decision. The second half was a description of the
plumbing at the time, written as though it were one. Nothing about the design
made an embedder's module unreadable to the compiler. `cove-schema` sits below
both `cove-sema` and `cove-runtime` precisely so that neither owns the
description; an embedder could already write a `ModuleSchema` as precise as
`CONSOLE` or `HTTP`; and `hosts::SHIPPED` was privileged in exactly one way —
it was the only table anybody had thought to hand the checker. What was
missing was somewhere to hand one over.

The cost of leaving it missing fell on the profile ADR 0001 names second.
Embedded is one of the four MVP execution profiles, and an embedding's own
modules got none of what a shipped module's calls get: no arity check, no
argument type check at the call site, no result type — so every call into one
produced an unknown type, and the unknown spread through whatever was written
with it — no field check on the types it declares, and no capability in `cove
outline`. The Host API boundary was the first thing in the toolchain to look
at the call at all, which meant a misspelled operation or a wrongly typed
argument in an embedder's module was a run-time failure by construction. For a
language whose case for embedding is that the host stays in control, that is a
gap rather than a design.

There was a second cost, quieter, and it is why this ADR changes `HostApi` as
well as `Compiler`. A module described itself five times — `name()`,
`capability()`, `schema()`, `types()`, `resources()` — and "shared is literal"
held for the shipped modules only because all eight happened to answer out of
one table. Nothing made them. A host that answered `name()` with one string
and `schema()` out of another table was a host describing itself two ways, and
the moment a checker started reading one of those descriptions, the two could
disagree about a module while both remained internally consistent. That is the
drift `cove-schema` exists to prevent, and the trait was the one place it was
still possible.

## Decision

An embedder hands the checker the same `ModuleSchema` value its module
registers with, and the checker reads it exactly as it reads a shipped one.

### One value, and only one way to ask for it

`HostApi` declares a single required method:

```rust
fn module_schema(&self) -> ModuleSchema;
```

That is the whole of what a module says about itself — its name, its
capability, its operations, its types, and the kinds of resource it opens. The
five accessors are gone rather than defaulted. A defaulted accessor is still
an overridable one, and an overridable accessor is a second description
waiting to be written: registry dispatch would read the accessors while the
checker read `module_schema()`, so an implementation that overrode one of them
would describe a different module to each end, silently, and the compile error
that made the migration visible would be the last warning anyone got. Having
one method means there is nothing to keep in agreement.

The checker takes the same value:

```rust
let program = Compiler::new()
    .with_host_schemas(hosts.module_schemas())
    .compile(&package)?;
```

`HostRegistry::module_schemas()` answers with what every registered module
declared, so registering a host and checking a program written against it are
two readings of one table rather than two tables that have to be kept the
same. `Compiler::with_host_schema` takes a single `ModuleSchema` for an
embedding that would rather name its modules one at a time.

`module_schemas()` reports the first module registered under a given name,
because that is the one every dispatch path in the registry finds. A checker
that reported the last would be checking a module the run will not reach.

### A supplied module is a host module in every sense

Not a lesser kind. A module the checker was given may be named by a `use`, may
not be shadowed by a package module, has its calls checked at the call site
for arity and argument types, produces the result type its operation declares,
has its declared types written and initialized with labels like any struct,
answers on its resource handles only the operations its `ResourceSchema`
declares, and contributes its capability to every function that reaches it.

The capability a call requires comes from the operation, falling back to the
module when the schema declares no such operation — which is the rule the
boundary already enforced. The two must agree, or the checker would report a
run as grantable that the boundary then refuses. Every shipped module names
its capability after itself, so nothing shipped is affected; an embedder that
gates one module's operations differently now gets a checker that says so.

### A module no schema describes keeps the boundary-only fallback

Registering a module the compiler was never told about stays legal, because a
host may register whatever it likes and the boundary checks every call
regardless. It is no longer silent: `cove check` warns
`cove::resolve::unchecked_host` at the `use` that named the module, and points
at `Compiler::with_host_schema` as the way to make it checked. The warning is
raised once per module per package rather than once per `use`, so a package
where five modules import one unknown host reports one unknown host.

### The set of modules may be closed

`HostSchemas::only(..)` builds a set that answers for the modules it was given
and no others. An embedding that registers a registry of its own — not the
shipped hosts plus additions — needs that: a set that still fell back to
`SHIPPED` would tell such a program that `files.write` is a checked call, and
the run it is about to make has no `files` module to dispatch it to.

### Nothing is serialized

The in-process embedding API is the case that exists. A format for describing
a host module to something outside the process should be invented when
something outside a process needs to read one, and not before.

## Consequences

ADR 0001's "shared by the compiler, runtime, and CLI" becomes literal for
every host module rather than for the eight the toolchain ships. Its sentence
about the modules a compiler cannot see is now true only of the modules an
embedder chose not to describe.

**A `HostApi` implemented outside this workspace must be migrated, and the
compiler will say so.** `module_schema` is required and the five accessors no
longer exist, so an existing implementation fails to compile with
`not all trait items implemented` rather than quietly losing a description.
The migration is to delete `name`, `capability`, `schema`, `types` and
`resources` and write one method in their place:

```rust
impl HostApi for Company {
    fn module_schema(&self) -> ModuleSchema {
        COMPANY
    }
    // call / call_with / call_resource unchanged
}
```

where `COMPANY` is a `ModuleSchema` holding what the five methods used to
return separately — the string `name()` answered, the string behind the
`Capability` `capability()` answered, and the three slices. Deleting only some
of them is not an option, which is the point: there is no form of the trait in
which two descriptions of one module can both exist.

**A schema assembled at run time has to leak.** `ModuleSchema` is `Copy` and
every field of it is `&'static`, which is what makes the shipped tables
`const` and what lets a caller hold a schema while it goes on reading. The
trait used to hand back borrows of `self`, so a host could keep a `String` and
a `Vec<OperationSchema>` and return references into itself; returning the
table by value takes that away. A module whose name comes from configuration
or whose operations come from a manifest now builds its schema once, leaks it,
and hands out the same copy — several leaks for one module, once per process.
That is acceptable for an in-process embedding registered at startup and it is
not free, so it is recorded on `ModuleSchema` with the pattern spelled out,
and [issue #86](https://github.com/myuon/cove/issues/86) carries the question
of whether `ModuleSchema` should take a lifetime parameter instead. A lifetime
would reach every crate that names the type; `Cow` would not work at all,
because `Box::new` is not const-constructible and the shipped tables would
stop being `const`.

**The boundary still checks everything it checked before.** A schema is a
claim, and an embedder that supplies one its implementation does not honour
has told the checker something untrue — so the boundary keeps checking
arguments on the way in and the host's answer on the way out, for supplied
modules exactly as for shipped ones. This ADR moves a failure earlier; it
removes no check.
