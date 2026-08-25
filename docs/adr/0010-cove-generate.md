# ADR 0010: `cove generate`

- Status: Accepted
- Date: 2026-08-25
- Implemented by: PR #25
- Implementation status: complete

## Context

The Language Card's tooling contract lists `cove generate` — "run explicit,
capability-controlled code generation". ADR 0001 says more: "The normal build
never executes arbitrary project code. Build scripts are excluded from the MVP.
Code generation is an explicit `cove generate` workflow whose generator runs as
an ordinary capability-controlled Cove entry and whose output is inspectable
source."

The command does not exist. What it must be, though, is already almost entirely
decided by those two sentences, and by what the toolchain now has: entries,
grants, budgets, and a formatter.

## Decision

`cove generate <run-name>` runs an ordinary Cove entry under the capabilities
its `[run.<name>]` table grants, and writes the source it returns into the
package.

```toml
[run.routes]
entry = "codegen.routes"
allow = ["files"]
generates = "server/routes.cove"
```

### The generator is an ordinary program

Not a plugin, not a macro, not a build script. It is a Cove entry, it is
checked and type-checked like any other, and it can only reach what its
capabilities allow. That is the whole reason this is a separate command rather
than part of `cove build`: generation is the one time the toolchain runs
project code, and it should be a thing a person asks for.

### The output is source, and it is formatted and checked

A generator returns `String`. `cove generate` writes it to the path
`generates` names, runs `cove fmt` over it, and then checks the package. A
generator that produces source that does not parse fails at the moment it did
so, not later.

Generated files carry a header marking them generated and naming the run that
made them. `cove generate --check` regenerates into memory and fails if the
result differs from what is on disk, which is what CI runs.

### Generation never runs implicitly

`cove build`, `cove run`, `cove check`, and `cove test` do not generate. A
stale generated file is a real state the toolchain can be in, and `--check` is
how a project refuses it. Making generation implicit would make every other
command able to execute project code, which is exactly what ADR 0001 forbids.

### Scope

One output file per run. No templating language, no partial regeneration, no
dependency tracking between generators. A generator that wants structure builds
a string, which is what makes its output inspectable.

## Consequences

The card's `cove generate` line becomes true, and ADR 0001's "the normal build
never executes arbitrary project code" stays true because generation is never
part of a build.

`generates` is a new `[run.<name>]` key, so a run table now describes either an
execution or a generation. A run with `generates` may still be executed by
`cove run`; it just also names where its output belongs.
