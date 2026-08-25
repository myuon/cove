# End-to-end tests

This directory is one Cove package whose modules are executable test cases.
Every case is a real program, run through the real `cove` binary, whose
observable behaviour is pinned by golden files.

```console
$ cargo test -p cove-cli --test e2e
```

The harness lives in `crates/cove-cli/tests/e2e.rs`.

## Layout

```text
tests/e2e/
  cove.toml                 one [run.<name>] table per shared-package case
  documents/                documents the `documents` host may read
  files/                    the root the `files` host is confined to; it is
                            created by the first case that writes, and
                            `host_files` removes what it wrote, so the
                            directory is left empty
  <case>/main.cove          the program, for a shared-package case
  <case>/<module>/*.cove    a further module of the same package, which the
                            case may `use`; it is not a case of its own,
                            since it holds no main.cove
  <case>/cove.toml          present only when <case> is its own package
  <case>/main/main.cove     the program, for an own-package case
  <case>/expected.out       exact expected stdout
  <case>/expected.err       present only when the case must fail
  <case>/args               optional: one program argument per line
  <case>/env                optional: KEY=VALUE per line, or a bare KEY to
                            remove that variable from the child process
```

Every directory containing a `main.cove`, directly or through its own
`cove.toml` (see below), is discovered automatically and run in sorted order
as `cove run <case> [arguments]`. A case with no matching `[run.<case>]` table
fails the suite instead of being skipped silently, and a `[run.<name>]` table
with no case directory fails it too.

## Rules the harness enforces

- The exit status is zero when there is no `expected.err`, and non-zero when
  there is one.
- stdout equals `expected.out` byte for byte.
- stderr equals `expected.err` when it exists, and is empty when it does not.

Diagnostics contain absolute paths, so stderr is normalised before it is
compared: the absolute path of this directory becomes the literal `<e2e>`.
Nothing else is normalised, so line and column numbers stay pinned and a
diagnostic that moves is a visible change.

## Shared cases vs. a case that is its own package

Most cases share this directory's package: their working directory is
`tests/e2e/`, their `[run.<case>]` table lives in `tests/e2e/cove.toml`, and
`cove run <case>` resolves every module below this directory before running
anything. That means a parse or resolve error in any one shared case fails
every shared case's run, so a shared case can only pin a *runtime* failure —
by the time the program is running, the whole package has already resolved
cleanly.

A case directory that holds its own `cove.toml` is exempt from this: the
harness runs `cove` with its working directory set to that case's own
directory instead, and looks up `[run.<case>]` there. `cove` then resolves
only that case's own package, so its check-time errors (a parse error, an
unresolved `use`, a duplicate declaration, and so on) cannot affect any other
case. A `.cove` file may not live directly in a package root, so an
own-package case nests its program one level down, conventionally as
`<case>/main/main.cove` with `entry = "main.main"`.

Give a case its own package when it needs to pin a **check-time** diagnostic,
or any other program that would fail to resolve or type-check — a type error,
a non-exhaustive `match`, an unknown `use` path, a duplicate declaration, and
the like. `cove run` type-checks the whole package before it runs anything, so
a case that used to fail at run time moves here as soon as the checker can see
the mistake: `fail_mixed_arithmetic`, `fail_mixed_equality`, and
`fail_count_removed` all did. Leave a case in the shared package otherwise: it is
less to set up, and keeps the shared package's module count as the signal
that most of the suite still lives together.

## Adding a case

1. Decide whether the case needs its own package (see above). For a shared
   case, create `<case>/main.cove`; for an own-package case, create
   `<case>/cove.toml` and `<case>/main/main.cove`. Give every exported
   declaration a `///` doc comment, and print results with `console.println`
   so the behaviour is observable.
2. Add a `[run.<case>]` table — to `cove.toml` for a shared case, or to
   `<case>/cove.toml` for an own-package case — granting exactly the
   capabilities the program needs.
3. Generate the golden files, then read the diff before committing it.

## Updating the golden files

```console
$ UPDATE_EXPECT=1 cargo test -p cove-cli --test e2e
```

This rewrites every golden file from the actual output. `expected.out` is
always written, `expected.err` is created when a case newly fails and deleted
when a case newly succeeds, so a regression can never hide behind a stale
file. Deleting a golden file and regenerating restores it, and regenerating an
already-passing suite changes nothing.

A golden file is the specification of the current behaviour, so always read
`git diff` afterwards and make sure every change is one you meant.

## A note on `values_string`

`values_string/expected.out` contains the raw bytes of the escapes the lexer
supports, including a carriage return and a NUL. Git treats it as binary, and
that is deliberate: the case exists to pin those bytes exactly.
