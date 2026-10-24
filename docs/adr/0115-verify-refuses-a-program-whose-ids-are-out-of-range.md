# ADR 0115: `verify` refuses a program whose ids are out of range

- Status: Accepted
- Date: 2026-10-24
- Decides: that `cove_ir::verify` answers for every `Program`, including one
  `serial::decode` returned, by checking every id against its table before it
  checks anything else, and reports an `Invalid` instead of panicking; that
  `serial::decode` stays as it is; that the `image` fuzz target is decode then
  verify; and that a verified decoded program is still not run. Recorded for
  [issue #683](https://github.com/myuon/cove/issues/683)
- Supersedes: [ADR 0112](0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md)'s
  decision (section 5) that a decoded image is not verified

## Context

ADR 0112 left `verify` out of the `image` target because the first fuzzing
session found `Function::param_words` indexing the layout table with a
parameter layout past its end. It judged `verify` to be written against
tables consistent by construction, and holding it to arbitrary tables to be
the same trust `decode` declines. The input was kept under `known/` as the
first thing to fix if images were ever accepted from elsewhere.

That leaves a verifier that panics on the one input a verifier exists for: a
program that is not what the lowering would have built. A check that can only
be trusted on well-formed input is not a check, and the cost of making it
total is small and local.

## Decision

`verify` begins with a pass (`verify/indices.rs`) that visits every id the
program holds: the program-wide layout ids, every layout's shape, `names`,
`resource_names`, `by_name`, argument lists, host operations, and in every
function its parameters, return layout, captures, locals, expansions and every
instruction. Each `LayoutId`, `FunctionId`, `StrId`, `ArgsId`, `TableId` and
`HostOpId` must be inside its table. Any that is not is reported as an
`Invalid`, and `verify` returns without running the rest, because the rest
indexes freely. The instruction walk has no wildcard arm, so a new instruction
cannot be added without deciding what ids it holds.

The check is in `verify`, not in `decode`. `decode`'s contract is about bytes,
it is also used where the program is trusted and large, and a second place
that knows what every id means would drift from the verifier. `decode`'s
documentation still says an image is trusted once its header matches; what
changes is that a caller who wants to stop trusting it has a total check to
call.

The `image` target is decode then `verify`. A decoded program that verifies
is still not run: `verify` checks well-formedness as the lowering establishes
it, and that is not a claim that the VM or the code generator is safe on every
program it accepts. That claim has not been made and is not made here.

## Consequences

- `fuzz/regressions/image/verify-param-layout-out-of-range` leaves `known/`
  and is replayed by `cargo t`.
- Further panics the `image` target finds in `verify` are fixed in `verify`
  and given a regression input.
- Ordinary lowered programs pay one extra linear walk in `verify`.
