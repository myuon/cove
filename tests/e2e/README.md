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
  cove.toml                 one [run.<name>] table per case
  documents/                documents the `documents` host may read
  <case>/main.cove          the program
  <case>/expected.out       exact expected stdout
  <case>/expected.err       present only when the case must fail
  <case>/args               optional: one program argument per line
  <case>/env                optional: KEY=VALUE per line, or a bare KEY to
                            remove that variable from the child process
```

Every directory containing a `main.cove` is discovered automatically and run
in sorted order as `cove run <case> [arguments]`, with the working directory
set to this one. A case with no matching `[run.<case>]` table fails the suite
instead of being skipped silently, and a `[run.<name>]` table with no case
directory fails it too.

## Rules the harness enforces

- The exit status is zero when there is no `expected.err`, and non-zero when
  there is one.
- stdout equals `expected.out` byte for byte.
- stderr equals `expected.err` when it exists, and is empty when it does not.

Diagnostics contain absolute paths, so stderr is normalised before it is
compared: the absolute path of this directory becomes the literal `<e2e>`.
Nothing else is normalised, so line and column numbers stay pinned and a
diagnostic that moves is a visible change.

Because the whole package is loaded and resolved on every run, a parse or
resolve error in one case breaks every case. Failure cases therefore pin
runtime failures, not compile-time ones.

## Adding a case

1. Create `<case>/main.cove`. Give every exported declaration a `///` doc
   comment, and print results with `console.println` so the behaviour is
   observable.
2. Add a `[run.<case>]` table to `cove.toml` granting exactly the capabilities
   the program needs.
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
