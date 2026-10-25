# ADR 0122: With no `--backend`, a command runs on the fastest tier this host has

- Status: Accepted
- Date: 2026-10-25
- Decides: that a command that runs a Cove program and is given no
  `--backend` runs it on the fastest tier this build and this host have — the
  native tier of ADR 0055 where there is one (a default build on Unix x86-64,
  since ADR 0076), the encoded VM otherwise; that `cove test` accepts
  `--backend native` and reports one aggregate tier mixture for the suite; that
  a native run the default chose reports its mixture in one line and a run that
  named `--backend native` in full; that `cove replay` replays a trace recorded
  on the native tier on the native tier; and that `cove build` and
  `cove run --profile` keep the VM, for reasons of their own. Recorded for
  [issue #711](https://github.com/myuon/cove/issues/711)
- Supersedes: [ADR 0022](0022-the-vm-is-the-default-backend.md)'s decision
  that "the VM is what runs a Cove program" — that `cove run`, `cove generate`
  and `cove test` reach the VM without being asked — for every command that
  runs a program, `cove build` excepted. Everything else in ADR 0022 stands:
  the interpreter stays selectable, `--backend vm` still selects the VM, one
  function holds the default, and `cove test` still lowers once per test
- Supersedes: [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  "Initially native execution is explicit. The VM remains the default until
  the adoption gate below passes." This ADR is the follow-up ADR that
  ADR 0055's "Making the native tier the default … require[s] the evidence
  above and a follow-up ADR" asks for; how the gate stands is set out below,
  item by item, including the item that does not hold. Everything else in
  ADR 0055 stands, and in particular its no-fallback rule: `--backend native`
  on a host without the tier is still a capability diagnostic
- Supersedes: [ADR 0076](0076-the-native-tier-is-built-by-default.md)'s "The
  default *backend* does not change. `cove run` is the encoded VM unless
  `--backend native` is given". The rest of ADR 0076 — what is built, and that
  the libraries keep the feature off — is untouched and is what this ADR
  stands on
- Refers to, without superseding: [ADR 0077](0077-cove-fmt-is-covefmt.md),
  whose "`cove fmt` runs covefmt on the native tier on a host that has it and
  on the encoded VM otherwise" is the precedent this ADR generalises;
  [ADR 0026](0026-a-trace-names-the-backend-that-recorded-it.md), whose
  default replay — the backend the trace names — this ADR keeps and now
  serves for a native trace; and
  [ADR 0121](0121-a-test-fn-may-take-parameters.md), whose properties run on
  the native tier with one compilation per property

## Context

ADR 0022 made the VM the default in 2026-08 because it was faster than the
interpreter and agreed with it. ADR 0055 added a native tier under the VM —
compiled machine code per function, the encoded VM for every function the
code generator refuses — and kept it explicit until an adoption gate passed.
ADR 0076 built it into the default `cove` binary, and ADR 0077 made `cove fmt`
run on it where the host has it, which is the first command whose default was
the native tier.

Three things moved since.

- **The interpreter is to be retired** ([issue #701](https://github.com/myuon/cove/issues/701)).
  What a program runs on in practice is the VM and the native tier, and the
  project tests against the default: CI's dogfood steps, the end-to-end
  golden files and `cove test` in `examples/` and `tools/covefmt/` all run
  whatever a command does when it is given no flag. A default that is not
  what ships on the main platform makes that baseline a tier the project does
  not ship.
- **`cove test` could not run native at all.** It refused `--backend native`,
  because ADR 0055 forbids presenting a mixed run as native and a suite had no
  report that aggregated its tests' tiers: a hundred and forty per-test tables
  are not one answer. So the one command whose job is to say whether a
  program works could not say it about the tier `cove fmt` already runs on.
- **Property tests** (ADR 0121) run one lowered test a hundred times, which is
  the shape a compiled tier is for, provided it is compiled once per property
  and not once per case.

The project owner decided in issue #711, and widened the decision from
`cove test` to every command: with no `--backend`, run on the fastest tier
this build and host have.

## Decision

### 1. One default, resolved per host

`Backend::default_for_a_run` answers `native` where
`cove_runtime::native_available()` answers `Ok` — this build has the code
generator, the host is x86-64 Unix, and it can map executable memory — and
`vm` everywhere else. The probe compiles the empty program, which can fail
only for a reason of the host's, and is asked once per process.

That is not the fallback ADR 0055 forbids. The VM is chosen on a host without
the tier because nobody asked for the tier; `--backend native` named on such
a host is still ADR 0055's capability diagnostic, from every command that
takes the flag. And a native run is a mixture by design — every function the
code generator refuses runs on the encoded VM, a complete execution path below
the same lowering — which ADR 0055 allows on the condition that the run says
so. Every native run below says so.

### 2. Each command

- **`cove run` and `cove generate`** take the default as it stands. A native
  run the default chose prints one line to stderr: the functions compiled and
  refused, and the share of Cove calls that entered compiled code. A run that
  named `--backend native` prints ADR 0055's whole report, as it always did.
  One line rather than the table, because a report a run did not ask for is
  there to say which tier ran, not to be read as a benchmark — and a twenty-
  line table after every `cove run` is noise in every program's stderr.
- **`cove run --profile`** with no `--backend` runs the VM. ADR 0055 allows a
  profile on the native tier to be "refused or explicitly run the VM"; named,
  it is still refused, and unnamed, the fastest tier that can answer the
  question asked is the VM.
- **`cove test`** takes the default, and accepts `--backend native`. Each test
  is lowered once, as ADR 0022 has it, and compiled once, and every case of a
  property runs over that one table. After the last test it prints one
  aggregate report to stderr: tests compiled, functions compiled and refused
  summed over the tests' lowerings, compile time, the runs and how their
  calls crossed between the tiers, and the refusals **by reason**, each
  refused function counted once and charged every dynamic call it kept in the
  VM over the suite — `cove run`'s ranking, by the work a refusal prevented. A
  suite on the VM or the interpreter prints no report, as `cove run` on the VM
  prints none.
- **`cove replay`** keeps ADR 0026: with no flag it replays on the backend the
  trace names. Since `cove run` records on the native tier by default, a trace
  that names `native` is now the ordinary case, and it is replayed on the
  native tier rather than refused. On a host without the tier it is the
  capability diagnostic, naming `--backend vm` as the cross-backend replay that
  says so; replaying it on the VM unasked would be the silent substitution.
- **`cove build`** keeps the VM. What it builds embeds a backend in an
  executable (`EmbeddedBackend`), and the native tier cannot be embedded yet —
  ADR 0055 leaves "whether `cove build` embeds JIT code, links an object, or
  keeps the VM" open. Its default is the fastest backend it can embed, which
  is the VM, and `--backend native` stays refused there with that reason.
- **`cove fmt`** is unchanged: it already did this (ADR 0077).
- **`cove debug`** is unchanged: a debugger is a feature of the encoded
  machine and takes no `--backend`.

### 3. The tests hold both tiers to one answer

The end-to-end golden files are the default's answer, so on a native host
they are the native tier's and in CI's `--no-default-features` run they are
the VM's — one set of files, so the two must agree. The report a native run
prints is taken out of stderr before anything is compared. Where the default
is native, every case the interpreter is compared on is also run with
`--backend vm` and held to the same answer; the opt-in `native` marker the
cases used to carry is gone, because every case is now run on the tier.
`cove test`'s own unit tests run each suite on the interpreter, the VM and,
where the tier runs, the native tier, and require one answer.

CI's dogfood runs `cove test` and `cove generate --check` on the default,
which is native on its Linux x86-64 runner, and keeps `--backend vm` steps so
the VM is still run by name; the `--backend ast` steps are left for issue
#701 to decide.

## How ADR 0055's adoption gate stands

ADR 0055 asked for seven things before native execution became the default.

1. *Every native-capable corpus entry agrees with the VM and the oracle.* The
   end-to-end corpus now runs every case on the native tier, the VM and the
   interpreter and requires one answer (Decision 3). `cove test` in
   `examples/` and `tools/covefmt/` agrees on all three.
2. *Tests exercise the crossings, recursion, errors, cancellation and GC at a
   compiled safepoint.* `cove-runtime`'s `tests/native_tier.rs` does.
3. *`responsiveness.rs` states and measures the native tier's stop bounds.*
   **Not met as written**: `responsiveness.rs` measures the VM.
   `native_tier.rs` tests deadlines and cancellation at compiled safepoints
   (ADR 0060, ADR 0078), which bounds the stop but does not measure it the way
   `responsiveness.rs` does for the VM. The default moves without it, on the
   owner's decision; a program that needs the VM's measured bound names
   `--backend vm`.
4. *Compile time is reported apart from execution.* Every native run reports
   it; `cove test` reports the suite's.
5. and 6. *`arith` and `covefmt`.* Measured when ADR 0055 and ADR 0056 were
   written, and `covefmtBench` holds the two tiers byte-equal in CI.
7. *A build without the feature retains the VM corpus.* CI's
   `--no-default-features` end-to-end run, which this change keeps passing.

## What this gives up

- **Short runs pay a compile.** The tier compiles every reachable function
  before the first instruction. A program that runs for a few milliseconds can
  be slower on the native tier than on the VM, and a test suite of short
  tests is that shape: measured 2026-10-25 on an x86-64 Mac, `cove test` in
  `examples/` (140 tests) took 1.84 s on the default against 1.49 s on
  `--backend vm`, and in `tools/covefmt/` (82 tests) 4.54 s against 3.35 s, of
  which the native compile was 0.31 s and 1.17 s. "Fastest tier" is the tier
  with the fastest execution, not a promise about every program's wall time.
  `--backend vm` is one flag away, and a compile cache is a separate question.
- **Every `cove run` on a native host prints a line to stderr.** That is the
  price of ADR 0055's rule that a mixed run never presents as native, paid in
  its smallest form.
- **The default differs by host.** A program's answer does not — that is what
  Decision 3 tests — but its timing, its `--stats` and its trace header do. A
  trace now names the tier that recorded it, which ADR 0026 already made the
  header's job.

## Alternatives considered

**Keep the VM default and let `cove test` take `--backend native`.** The first
shape of issue #711. Rejected by the owner when the issue was widened: the
default is what the project tests against, and a VM default keeps the baseline
on a tier `cove fmt` has already left.

**Print the full report on every native run.** ADR 0055's report is a table of
twenty lines or more for a real program. On a run nobody asked to be measured,
the line that says which tier ran is what the rule needs.

**Make `cove build` embed the native tier.** That is ADR 0055's open question
about AOT and persisted code, and nothing here answers it.

## Consequences

- `Backend::default_for_a_run` answers per host; `RunFlags` records whether
  the backend was named.
- `cove_runtime::native_available` exists; `Vm::with_prepared` installs the
  machine code a `PreparedProgram::with_native` preparation carries, as
  `OwnedVm` already did; `TestBackend::Native` and
  `TestRun::run_prepared_counting` let any test runner run on the tier and
  report its mixture.
- `cove test` prints an aggregate `native:` report on stderr after a native
  suite.
- `cove replay` runs a native trace on the native tier.
- The end-to-end suite runs every compared case on the native tier and the VM
  where the tier runs; `tests/e2e/*/native` markers are removed.
