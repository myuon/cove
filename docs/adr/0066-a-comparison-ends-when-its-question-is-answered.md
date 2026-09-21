# ADR 0066: A comparison ends when its question is answered

- Status: Accepted
- Date: 2026-09-21
- Decides: that the hand-written x86-64 template compiler is the **only**
  native code generator; that the Cranelift arm, its Cargo feature, its four
  pinned dependencies, its half of the shared test suite, the two-arm
  agreement test and the harness that raced the two are removed; and — the
  half a reader will want most — **where the coverage that only the two-arm
  test held goes, case by case, and which classes of divergence get a test of
  their own instead.** Recorded for
  [issue #452](https://github.com/myuon/cove/issues/452)
- Supersedes:
  [ADR 0056](0056-the-first-code-generator-is-the-one-that-was-cheaper-everywhere.md)'s
  **"the Cranelift arm may be deleted, and the comparison and its harness are
  kept"**, and nothing else it decided. ADR 0056's choice of the template
  compiler as the first code generator stands exactly as written, and so does
  every measurement that justified it: the two tables in its Context are the
  record of a race that happened, and this ADR neither re-runs them nor
  disagrees with them. What is withdrawn is the *retention* — the standing
  obligation to keep the losing arm compiling, lowering and agreeing

## Context

ADR 0055 named Cranelift for the native tier's first code generator without a
comparison. ADR 0056 made the comparison: two code generators, built against
the same lowered IR, the same slot ABI and **one shared subset predicate**, so
that a difference between them was a fact about code generation rather than
about what each had chosen to compile. It raced them on `benches/arith`'s
scalar loop and on two of covefmt's functions with real inputs, and the
template arm won on compile latency by 69×, on the stripped bundle by 140× and
on the dependency graph by 38 crates, while execution came out within a few per
cent in both directions.

Keeping the loser was the right call **at the time**, and for a reason that had
nothing to do with hedging. A race is only a race if both runners are running
the same course: ADR 0056's numbers are readable as a statement about code
generators precisely because `tests/agree.rs` asserted, on the same IR, that
the two arms answered the same words, made the same hand-overs in the same
order, left the same work unpaid and **refused the same programs**. A
comparison whose arms computed different things would have been a number about
nothing. So the second arm was not kept as insurance; it was kept as the
instrument that made the first arm's measurements mean something. ADR 0056 said
so plainly — "the harness that would prove it is kept for exactly that" — and
named one condition under which Cranelift would come back: a workload where a
register allocator earns its 4.7 MB.

That instrument has now read its measurement. The tier is chosen, `cove run
--backend native` compiles with the template arm and nothing selects the other,
and no distributed build has ever contained Cranelift.

What has changed since is the **other** side of the ledger. ADR 0064's Phase 1
began moving `Intrinsic` variants out of the runtime and into the IR, and every
one of those migrations adds an instruction that both code generators must
lower. Phase 1 added three:

| instruction | lines added to the Cranelift lowering | pull request |
| --- | ---: | --- |
| `Inst::RunFind` | 14 | [#449](https://github.com/myuon/cove/pull/449) |
| `Inst::FloatAbs` | 26 | [#451](https://github.com/myuon/cove/pull/451) |
| `Inst::FloatMinMax` | 44 | [#453](https://github.com/myuon/cove/pull/453) |

Eighty-four lines of `crates/cove-native/src/compile.rs`, written to be correct
against a contract and tested against a second implementation of it, in a
lowering that no `cove` binary anywhere contains. The third of them was not
cheap work either: `Inst::FloatMinMax` absorbs a NaN where Cranelift's `fmin`
propagates one, so the Cranelift arm could not use the obvious instruction and
is three `select`s over `fcmp` instead — a sequence reasoned out, written and
reviewed for an arm that is never entered.

And Phase 1 is the small part.
[Issue #454](https://github.com/myuon/cove/issues/454) plans the rest in six
steps, from 23 `Intrinsic` variants down, at an estimate of a dozen or so pull
requests. It names this decision as its own precondition, and it names the two
shapes the alternative takes: either each migration writes a second lowering
nobody runs, or it leaves a refusal in an arm nothing selects — and a refusal
there silently narrows the shared subset predicate, which is to say it narrows
what the *template* arm compiles, because the predicate is one predicate. The
second shape is the worse of the two: it would make the retained arm a brake on
the shipped one.

So the cost is no longer the 4.7 MB ADR 0056 weighed and declined to pay. It is
a per-instruction tax on every remaining migration, and it is paid in the
currency [PHILOSOPHY](../PHILOSOPHY.md)'s "earn complexity through use" is
written about. The condition ADR 0056 named for bringing Cranelift back — a
workload wanting a register allocator — has not arrived in three ADRs of
measurement; the condition for the comparison ending has.

## Decision

**The hand-written x86-64 template compiler is the only native code generator.**

- `crates/cove-native` has one code generator. `src/compile.rs`, the `cranelift`
  feature, and the four pinned `cranelift-*` dependencies are removed;
  `template` stays a feature that is off by default, because ADR 0055's
  adoption gate asks that "a build without the native feature has no
  executable-memory dependency" and that is a fact about `cargo tree` which is
  unchanged.
- The `cranelift` feature is removed from `cove-runtime`, `cove-cli` and
  `cove-bench`, which only forwarded it.
- `crates/cove-native/tests/scalar.rs`, which bound the Cranelift arm to the
  shared suite, and `crates/cove-native/tests/agree.rs`, which compared the two
  arms, are removed. `tests/suite/mod.rs` stays whole and is now the template
  arm's suite; `tests/template.rs` runs it.
- `crates/cove-bench/src/bin/native_compare.rs` is removed with them. Its own
  first sentence is "which native code generator, measured rather than argued",
  and that is the question this ADR closes. Its VM-oracle discipline — every
  run of every arm checked against the VM's answer for that iteration — is not
  lost: it is what `crates/cove-runtime/tests/native_tier.rs` and CI's
  `covefmtBench` step already do on whole programs rather than on one extracted
  loop.
- **A new IR instruction is lowered once.** A migration under
  [issue #454](https://github.com/myuon/cove/issues/454) writes one lowering,
  in `crates/cove-native/src/template.rs`, and its agreement gate is AST oracle
  against VM against template JIT.

**No Cove semantics may survive only in the deleted arm.** That is the
obligation this decision is really about, and the next section is how it was
discharged.

## Where the agreement tests' coverage goes

`tests/agree.rs` held five cases. It is worth being exact about what each one
was *for*, because the file's name invites the assumption that all of it was
cross-implementation checking, and most of it was not.

The suite the two arms shared, `tests/suite/mod.rs`, holds each arm to **the
encoded tier's behaviour written out as literals** — the answer, the raise, the
pending work, the hand-overs, the words left in the heap. `agree.rs` asked a
different and *weaker* question: not "is this one right" but "are these two the
same run". Wherever the suite already has an oracle, the two-arm comparison was
a second opinion on a question already answered from first principles, and
deleting it loses nothing. Wherever `agree.rs` ran a shape the suite has no
literal for, the coverage was real and had to move.

Case by case:

- **`both_arms_order_strings_alike`** — already covered, and by something
  stronger. `suite::a_string_order_is_the_runtimes_leaf` runs the same
  every-pair matrix of `ordered_strings` — equal, a prefix, a difference in the
  ninth byte, a two-byte character, the empty string, a string straddling a
  chunk, and the null reference that orders as empty — against a `std::cmp`
  oracle computed in the test, and asserts the hand-overs and the absence of a
  safepoint besides. `native_tier.rs`'s
  `a_string_order_from_compiled_code_agrees_with_the_vm` is the same claim
  against the real VM. Deleted.
- **`both_arms_poll_at_the_same_turns_under_a_threshold`** — **moved**, and it
  is the only one that had to be. ADR 0060's backedge threshold is the one
  behaviour with no VM oracle at all: the ADR moved the test out of the
  safepoint helper and into the code generators, so the *only* thing that ever
  held the boundary rule was the two arms agreeing on it. The shared suite pins
  thresholds of 0 and 16; `agree.rs` compared 1, 5, 7, 12 and 1024 and asserted
  literals for none of them. Thresholds of 7 and 5 are exactly the first
  backedge's work and a later one's, which is to say they are the rows where an
  arm that wrote `>` for `>=` differs and nowhere else differs. That table is
  now `suite::a_backedge_polls_at_the_turn_its_threshold_names`, with the poll
  turns, the work carried to each and the work left pending written out as
  literals for every one of the seven thresholds — an oracle the code generator
  does not supply, rather than a second copy of it.
- **`both_arms_answer_the_same_thing`** — already covered, in every one of its
  rows, and this was checked row by row rather than assumed. The loop, the trap,
  the eleven arithmetic rows, negation including `i64::MIN`, the `ABSOLUTES` and
  `EXTREMA` tables in all their aliasing shapes, the literal table, `load-elem`,
  `store-elem`, `byte-at`, the ADR 0062 windows and their every cold path, the
  ADR 0058 run copies and slices, allocation in all three `Len` forms and its
  refusal, the address family on both sides of the stack/heap boundary, the
  overlapping `memmove`, the clear, the intrinsic protocol in each effect class
  and the fused comparisons — each has a suite function holding one arm to
  literal VM behaviour, and in several places the suite's version is strictly
  the stronger: its `load-elem` object straddles a chunk boundary and
  `agree.rs`'s does not, and its literal table is read, re-read through a
  *different* table and followed to an object's length where `agree.rs` only
  compared two frames. Deleted.
- **`both_arms_refuse_the_same_programs`** — already covered, and vacuous by
  construction once there is one arm. Its own doc comment says why it existed:
  "the subset is one predicate, shared, so this cannot drift — the sharing is
  the claim". With one arm there is nothing to share it with.
  `suite::anything_outside_the_slice_refuses_the_whole_function` refuses a
  superset of its three programs — all three appear there verbatim, beside six
  more. Deleted.
- **`both_arms_count_the_same_intrinsic_sites`** — half already covered, half
  gone with the thing it was about. ADR 0064's Decision 7 divides its
  per-variant report into sites, which are a fact about the IR, and bytes,
  which only the template arm can attribute; this case asserted the two arms'
  site tables were equal and that the bytes went one way and not the other.
  `suite::every_intrinsic_call_site_is_counted` and
  `suite::an_intrinsic_calls_machine_code_is_charged_to_its_variant` hold the
  template arm's tables to literals, and `Arm::ATTRIBUTES_INTRINSIC_CALLS`
  keeps the `Some`/`None` claim per arm. What is genuinely gone is the sentence
  "a site is a fact about `cove_ir` rather than about a code generator", which
  was *demonstrated* by two independent counts agreeing and is now only
  asserted. That is a real loss and it is recorded rather than papered over; it
  is also, with one code generator, a distinction without a difference.

### The four classes issue #452 names

Issue #452 asks for NaN, signed zero, overflow and boundary values to have
explicit tests rather than inherited ones. They do, and at two levels, because
the two levels can see different things:

- **What a Cove program can observe** is pinned against the VM, in
  `crates/cove-runtime/tests/native_tier.rs`, where every case runs the same
  entry twice — once on `Vm::new`, once on `Vm::with_native` — and asserts the
  answers are equal *and* that the native crossing was taken.
  `a_float_extremum_runs_as_machine_code` carries the NaN rows in both argument
  orders and the signed-zero rows in both, because `min(-0.0, +0.0)` is `+0.0`
  and the reverse is `-0.0` and Cove renders the sign.
  `a_float_absolute_runs_as_machine_code` carries `-0.0` and `f64::MIN`.
  `negation_and_its_overflow_are_the_vm_s` carries `i64::MIN`. These are the
  cross-implementation check now: VM against template, on real Cove source,
  rather than template against Cranelift on hand-built IR.
- **What a Cove program cannot observe** is pinned in bits, in
  `crates/cove-native/tests/suite/mod.rs`, which `tests/template.rs` runs.
  `ABSOLUTES` and `EXTREMA` were always shared tables rather than `agree.rs`'s
  own, and they survive unchanged as the template arm's tables: a NaN's
  payload, its sign and its **quiet bit**, on signalling operands, where an
  implementation that reached the answer through arithmetic rather than by
  selecting a whole word would set bit 51 and be caught. `EXTREMA` was computed
  by a standalone `rustc` program calling `f64::min` and `f64::max` behind a
  `black_box`, so it is the machine's answers and not a transcription of an
  argument about them — which is exactly the property that lets one arm be held
  to it.

This is the divergence the two-arm test was built to catch, and it is worth
naming because it did catch one. `Inst::FloatMinMax`'s contract is **not** the
IEEE operation of either name: it absorbs a NaN where IEEE 754-2019's `minimum`
propagates one, and on operands that compare equal it answers the second, so
`min(-0.0, +0.0)` is `+0.0`. Cranelift's `fmin` is the 2019 operation and
propagates; x86-64's `minsd` has the right tie rule but answers its second
operand on a NaN. **Both obvious machine instructions are wrong, in different
ways.** Lowered naively the two arms would have disagreed with the VM and with
each other, and the reason they did not is that the contract was written down
first. That contract is on `Inst::FloatMinMax` in `crates/cove-ir/src/inst.rs`,
four lines of pseudocode with the three traps spelled out in prose beneath it,
and it is what survives the arm: a second implementation is one way to hold a
lowering to a contract, and a written contract with a bit-exact table and a
VM-differential case is another. The second one is the one that scales to a
dozen more migrations.

## What this does not decide

Restated from issue #452's non-goals, as this ADR's own, because a retirement
is an inviting moment to change other things at the same time:

- **The template JIT's instruction set is not widened here.** What it compiles
  and what it refuses are exactly what they were before this change. An
  instruction the Cranelift arm lowered and the template arm did not would have
  been a subset difference and the shared predicate forbade one; there is no
  such instruction to absorb.
- **No new code generator is introduced**, and none is planned. ADR 0056's
  recorded condition for reconsidering — a workload where a register allocator
  earns its cost — is unchanged and still open. What this ADR removes is the
  standing arm, not the possibility; a future one would be a new ADR with a new
  measurement, and ADR 0056's tables would be its starting point exactly as they
  are written.
- **Nothing about the IR or the calling convention moves.** No instruction is
  added, removed or re-specified, ADR 0057's return-into-the-destination is
  untouched, and the slot ABI is what ADR 0055 decided.
- **No performance change is made or claimed.** Removing a code generator that
  nothing selects cannot change what runs, and that is asserted rather than
  assumed: `cove run covefmtBench --backend native` and cq's 20,000-record run
  answer byte-identical `--stats --boundary` reports before and after — the same
  emitted IR, the same encoded instruction and dispatch counts, the same
  crossings, the same helper calls, the same fuel. Any movement in those would
  have meant the change was not what it says it is.

## Consequences

- One lowering per IR instruction. A migration under issue #454 is a smaller
  change than a Phase 1 migration was, by the 14 to 44 lines the table above
  records, and by rather more than that in the reasoning those lines needed.
- `cove-native` has one feature, `template`, and CI's native step is one arm
  rather than three passes. The third pass existed only to compile
  `agree.rs`, which `#![cfg(all(feature = "cranelift", feature = "template"))]`
  hid from either single-feature pass.
- `crates/cove-runtime/tests/native_tier.rs` is unchanged and still
  `#![cfg(feature = "template")]`, so a default build still compiles it to a
  binary with no tests in it. It runs in CI's "the native tier's runtime and
  CLI" step, which now runs once with `--features template` instead of looping
  over three feature sets. It is the file that matters most here: with the
  second arm gone it is the only place a cross-implementation check on machine
  code still happens, VM against template, on real Cove programs.
- The dependency graph loses four pinned `cranelift-*` crates and everything
  under them. ADR 0056 measured that at 38 crates and 4.7 MB of stripped binary
  for a build that turned the feature on; no default build ever had them, so
  what a user gets does not change.
- `docs/adr/0056`'s numbers remain the repository's record of why the template
  compiler was chosen, and remain checkable in the sense that matters: they say
  what was measured, on what, when. They are not reproducible from this
  commit, and that is what it means for a comparison to end.
