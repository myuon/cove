# Isolation testing: failure, clean-up, the next run, and the permission boundary

What a Cove run promises the run after it, what authority a run has, how both
are tested, and what is out of reach.
[ADR 0114](adr/0114-what-a-failed-run-leaves-behind.md) records the decisions;
[issue #681](https://github.com/myuon/cove/issues/681) is the plan this is
priority 3 and "権限境界の検査" of. [FUZZING.md](FUZZING.md) is priority 1.

**In-process isolation is not OS isolation.** Everything here tests that one
Cove run, inside one process, cannot see or reach another run's state or
authority through the runtime's own API, and cannot corrupt the process doing
so. That is a different guarantee from the one a process, container or
microVM boundary gives a host running tenants it does not trust: those also
contain a bug in the runtime itself, a process-terminating out-of-memory, a
stack overflow, a Spectre-class side channel, and anything the native tier's
generated code gets wrong. Nothing here establishes the second guarantee, and
nothing here argues a host should rely on the first instead of it.

## What exists

The survey this specification was written from, 2026-10-24.

| Piece | What it is | Where |
|---|---|---|
| `Runtime` | What every thread of one run shares: the checked program, the sources, the registry, the trace. Immutable once built. | `crates/cove-runtime/src/runtime.rs` |
| `HostRegistry`, `Grants`, `GrantSource` | Every host module a run may reach, and the one grant check (`HostRegistry::dispatch`) every host call passes. Grants are fixed at construction; the registry is shared by `Arc`. `GrantSource` says whether they came from `cove.toml` or were sealed into a built binary. | `src/host.rs` |
| `PreparedProgram` | One lowered program, encoded and verified once (and its native code, `with_native`), for any number of machines. Immutable, `Send + Sync`. | `src/vm/mod.rs` |
| `Vm`, `OwnedVm` | One machine: its heap arena, stack, frames, resource table and counters, and the run's budget. `OwnedVm` owns the three things a `Vm` borrows so it is `'static + Send` — the one piece of `unsafe` in the lifecycle (`src/vm/parked.rs`). A machine is reused: `Step::Answered` hands it back "for its next run". | `src/vm/mod.rs`, `src/vm/parked.rs` |
| `ParkedVm`, `YieldedVm`, `Step` | A run parked at a host call that answered `HostAnswer::Pending` (ADR 0080), or yielded at a safepoint (ADR 0084). Each is ended by `resume`, `cancel` or drop, all of which consume it. | `src/vm/parked.rs` |
| `Budget`, `Meter`, `Limits`, `Cancellation` | A run's accounting: deadline, host-call limit, call depth, task limit, and a flag another thread raises. There is no fuel (ADR 0091). | `src/budget.rs` |
| Tasks | `scope`/`spawn`/`await`: a thread per task, sharing the run's heap, budget and registry (ADR 0008). `Shared` cells and `lock`. | `src/vm/exec.rs`, `src/shared.rs` |
| `Interpreter` | The tree-walking oracle, with the same ways in and the same budget handling. | `src/interp.rs` |
| `examples/edge/host` | A multi-tenant server: one registry per tenant (its own `kv` store), a fixed pool of worker threads, `Isolates::PerRequest` (a fresh `OwnedVm` per request) or `Isolates::Pooled(n)` (resident machines reused, failed or not), a parking lot that answers or cancels parked runs by id, and a fetch pool that aborts a cancelled run's fetch. | `examples/edge/host/src/server.rs`, `deploy.rs` |
| `examples/rules/host` | One `Vm` or `Interpreter` per `Session`, invoked many times, mixing `invoke` and `invoke_within`. | `examples/rules/host/src/lib.rs` |

**There is no in-process messaging between isolates.** No channel, mailbox or
message type exists. A `Value` is `!Send` (it holds `Rc`s); a `Shared` cell is
refused at the boundary ("a `Shared` names one cell of this run's heap, so it
cannot be handed out as a value"); a closure handed out names one machine's
heap and another machine refuses it. What does cross between runs is
host-owned: a host's own state, a resource handle (a name the host minted,
ADR 0031), and a parked run's request and answer, which the embedder carries.
So issue #681's "メッセージ送受信" items are tested as those three: late
answers to parked runs, late cancellations, and values and handles carried
from one run to another.

## The specification

Written before the tests, and what they hold the runtime to.

### The lifecycle and how each stage fails

| Stage | Can fail by | How it fails | What is left |
|---|---|---|---|
| parse, check | a syntax or type error | `Vec<Diagnostic>` | nothing; the process's standard-library cache (ADR 0083) is unharmed |
| lower | a construct the lowering has not been taught, or an entry left out of a slice | `Vec<Diagnostic>`; or, at the run, a refusal naming the missing function | nothing |
| prepare | a program with no encoding | held as the refusal every run answers with | the preparation, reusable |
| construct (`OwnedVm::with_heap_words`) | a heap too small for the program's literals | infallible; every run answers "this run has no memory left" | the machine, useless; the preparation, reusable |
| run | a trap (division by zero, overflow, assertion), heap exhaustion, deadline, cancellation, call depth, host-call limit, task limit, a host's failure, a denied capability | `Err(RuntimeError)` with `outcome` | the machine, reusable |
| park | — | `Step::Parked(ParkedVm)` | the run, waiting |
| resume | the run's flag raised or deadline passed while parked; a host failure; an answer of the wrong type | `Step::Answered(vm, Err(..))` | the machine, reusable |
| cancel | — (it is the failure) | `(OwnedVm, RuntimeError)` | the machine, reusable |
| yield / resume a yield | as resume | as resume | as resume |
| drop | — | frees the run; a parked request is dropped with it | nothing |

Two failures are outside the table because they end the process: a Rust
allocation the process cannot satisfy, and a stack overflow. See "Two kinds of
out-of-memory" below.

### S1. A run after a failure cannot tell the failure happened

After a run fails at any stage above, the machine handed back runs the next
run exactly as a fresh machine of the same preparation and heap capacity
does: the same answer, the same console bytes, the same host calls, and the
same instructions, allocated words and host calls charged.

### S2. What is state

Per-run, and must not survive: frames, stack, tasks, held cells, the parked
request, the budget's accounting and cancellation flag, the yield request,
every heap word the run did not hand out, and the resource table's names
(ADR 0116; while the embedder holds an object of the heap, ADR 0119 keeps
the names that object reaches and no others).
Survives by design: the heap arena and the machine's cumulative counters; values the embedder kept (a closure
pins its environment, and only its own machine may call it); a host's own
state (per registry); everything immutable (preparation, grants, the
standard library cache, per-thread interned names).

### S3. Late answers and late cancellations

An answer cannot reach a run that ended: `ParkedVm` is the only handle and
ending it consumes it. A machine reused after a cancelled park parks again
with a new `ParkedVm`, and an answer given to it is held to the operation's
declared result type and no further — correlating answers with requests is
the embedder's. A `Cancellation` raised after its run ended reaches no later
run.

### S4. Authority

A run's authority is its registry's grant set, checked at every host call
from every frame — entry, closure, function value, standard-library
higher-order callback, host callback, spawned task, host operation bound to a
name — against the registry the running machine was built over.

- A callee or a task has exactly its caller's authority: both have the run's.
- A closure or a handle carries no capability: called or used in a run that
  lacks it, it is refused there.
- There is no delegation, attenuation or widening; grants are fixed when the
  registry is built. `GrantSource::Sealed` and `Config` refuse the same calls
  and differ only in the advice.
- The static capability analysis (ADR 0015) is advice; the grant check is the
  only thing that decides.
- A refusal is `RuntimeError { outcome: HostBoundary, denied_capability:
  Some(name), .. }`, raised before the host is reached: the operation has no
  effect.

### S5. Two kinds of out-of-memory

- **Heap capacity — recoverable.** An allocation that does not fit the
  machine's heap after a collection fails the run with "this run has no memory
  left". S1 holds for the machine.
- **Process memory — not recoverable.** A Rust allocation the process cannot
  satisfy aborts the process. A guest that can cause one by its own choice of
  sizes is a finding (ADR 0112 §3), as is a stack overflow.

### S6. Dropping is always safe

A machine, a parked run and a yielded run may be dropped in every state, on
any thread, and every request a host made is dropped exactly once. A value
the embedder kept may outlive its machine.

## What is tested

| Test | Spec | Issue #681 item |
|---|---|---|
| `crates/cove-runtime/tests/lifecycle.rs` `every_failure_leaves_a_machine_a_fresh_one_cannot_be_told_from_{encoded,native}` — twelve failures (undeclared function, wrong argument type, wrong entry shape, trap after half a line of output, deadline, cancellation from another thread with a double cancel, call depth, host-call limit, task limit, a task trapping beside one calling the host, a trap inside `lock`, heap exhaustion), each followed by six probes compared field by field with a fresh machine | S1, S2 | 初期化の各段階で失敗させ、安全な破棄・再実行; 同じワーカーの再利用 |
| `a_refused_program_leaves_the_process_fit_to_compile_the_next`, `a_function_the_lowering_left_out_is_refused_and_the_machine_serves_on` | S1 | 初期化の各段階 (compile, lower) |
| `a_machine_too_small_for_its_literals_refuses_and_the_preparation_survives` — heaps of 0 to 512 words | S1, S5 | 初期化の各段階 (construct) |
| `a_heap_swept_across_the_program_s_need_fails_cleanly_on_either_side_{encoded,native}` — 1 Ki to 64 Ki words in 25% steps across a program's need, then a second run on each machine | S5 | OOM・fault injection |
| `every_exit_from_a_park_leaves_a_machine_fit_for_the_next_run_{encoded,native}` — cancel, cancel with the request taken, a host failure, a wrong-typed answer, the flag raised while parked, the deadline passed while parked, resumed on another thread and cancelled at its second call; requests made = requests dropped | S1, S3, S6 | 例外・キャンセル・終了後の二重解放、解放後アクセス、参照の取り残し |
| `every_exit_from_a_yield_leaves_a_machine_fit_for_the_next_run_{encoded,native}` | S1, S6 | 同上 |
| `a_machine_may_be_dropped_in_every_state_{encoded,native}` — never run, answered, failed, parked with and without its request, yielded, and with a closure it made outliving it | S6 | 解放後アクセス、共有資源の寿命 |
| `a_late_cancel_of_a_finished_run_does_not_reach_the_next_one` — the previous run's flag raised twice while the next run spins; its `on_cancel` callback runs once | S3 | isolate終了後の遅延メッセージ |
| `a_stale_answer_is_held_to_the_declared_type_and_no_further` | S3 | 遅延メッセージ |
| `a_closure_handed_out_is_callable_by_its_own_machine_alone` — kept across a collecting run, called back, refused elsewhere | S2, S6 | 共有資源の寿命 |
| `a_storm_of_lifecycles_leaves_every_machine_as_a_fresh_one_{encoded,native}` — 400 fixed-seed steps over a pool of four machines interleaving create, run, trap, deadline, park, resume, fail a resume, cancel, yield, cancel a yield, resume a cancelled yield and drop; every answer checked against a fresh machine's; `a_long_storm_of_lifecycles` (ignored) is 4 seeds × 5,000 steps on both tiers | S1, S6 | isolateの起動、失敗、終了、再生成を組み合わせる |
| `crates/cove-runtime/tests/owned_lifetimes.rs` — `OwnedVm`'s self-referential borrows through park, move, resume, reuse and drop on other threads; small enough for Miri | S6 | Miri |
| `crates/cove-runtime/tests/authority.rs` `every_route_to_an_ungranted_capability_is_refused_before_the_host_{encoded,native,ast}` — all eight shipped capabilities (console, env, documents, clock, files, process, database, http) by eight routes (direct; a closure made in one function and called from another; a declared function passed to a helper; `Array.map`; `Array.filter`; a spawned task; a host operation bound to a name, interpreter only; a callback `clock.timeout` runs). Each route is first run granted and must reach the host; then denied, it must be refused with `denied_capability`, every host is wrapped in a spy whose log must stay empty for it, and the in-memory files, the process log and the console must be untouched | S4 | allow-* の拒否テスト; 禁止された操作が実際に実行されていないことを観測可能な副作用で確認 |
| `a_closure_kept_from_an_earlier_run_is_checked_when_it_is_called` — through a capability-open entry, which no static check could refuse | S4 | クロージャ・ハンドルの受渡し; 権限の継承・委譲 |
| `a_closure_does_not_carry_its_maker_s_grants_to_another_machine` | S4 | 同上 |
| `a_handle_handed_to_a_run_without_the_grant_is_refused_at_its_first_use` — a `files.Reader` from a granting registry, on the VM and the interpreter | S4 | ハンドルの受渡し |
| `a_sealed_grant_set_refuses_exactly_what_a_configured_one_does`, `a_registry_s_grants_are_the_same_for_every_run_over_it` | S4 | 意図しない権限拡大がないこと |
| `examples/edge/host/tests/isolation.rs` — one worker, `Isolates::Pooled(1)`: a trap, a heap exhaustion, a deadline while running, a deadline while parked at a fetch whose answer arrives late; after each, the same tenant and another answer exactly as a fresh server; two tenants running the same code keep separate `kv` state; each tenant's registry grants exactly its own table | S1, S3, S4 | 同じワーカー・プロセスの再利用; 遅延メッセージ |
| `an_unbudgeted_invocation_does_not_inherit_the_last_run_s_budget_{encoded,native}` — each of `OwnedVm`'s four ways in with no budget (`invoke`, `run_entry`, `invoke_parkable`, `run_entry_parkable`) after a cancelled run, a run past its deadline and a run that spent its host-call limit, compared with a fresh machine's answer and charged on a meter of its own; `an_unbudgeted_interpretation_does_not_inherit_the_last_run_s_budget`, the interpreter's two | S1, S2 | 同じワーカーの再利用 |
| `an_unbudgeted_invocation_charges_the_registry_s_budget_as_a_fresh_machine_does` — a budget installed in the registry is charged by every unbudgeted run, reused machine or fresh, and once cancelled stops both and the interpreter | S1 | 同上 |
| `examples/rules/host/tests/session_reuse.rs` — `Session::evaluate` after an `evaluate_within` whose deadline passed, on the VM and the interpreter: the first finding below, through the rules session's public surface | S1 | 同じワーカーの再利用 |

Nothing here asserts a duration. The deadlines that stop runs are tens of
milliseconds and only ever have to pass.

### Cost

`cargo t` gains five test binaries; their tests take about two seconds of
wall clock together on an x86-64 Mac (lifecycle 0.4s, authority 0.05s,
owned_lifetimes 0.05s, the edge isolation case 1.1s), and the build adds
their link time. `cargo ratchet` gains `a_long_storm_of_lifecycles`, about
eleven seconds, most of it waiting for yields.

## Findings

What the tests and the survey found. A finding not yet fixed is reproduced by
an `#[ignore]`d case gated on `COVE_KNOWN_FINDINGS` where it can be tested, so
that `cargo t` and `cargo ratchet` stay green until it is fixed; a fix deletes
the gate and the `#[ignore]`. None is gated now.

1. **An unbudgeted invocation inherited the last run's budget** (S1) —
   **fixed**, [issue #684](https://github.com/myuon/cove/issues/684).
   `Vm::invoke`, `Vm::run_entry`, `OwnedVm::invoke_parkable` and
   `run_entry_parkable`, and `Interpreter::invoke` and `run_entry`, took no
   budget of their own and rebound none, so they ran under the `Meter` the last
   `invoke_within` or `run_entry_within` left on the machine: after a run whose
   cancellation was raised the next `invoke` started cancelled, after one whose
   deadline passed it was past it, after one that spent its host-call limit it
   was over it. `session_reuse.rs` met it through
   `cove_rules::Session::evaluate` after `evaluate_within`, on the interpreter
   (the VM case passed there only because the request finished before its
   first safepoint read the clock).

   Every way in with no budget of its own — those six, `Vm::invoke_lowered`
   and `Vm::native_session` — now takes the registry's budget afresh as it
   begins, which is what `Vm::new` and `Interpreter::new` take. So it runs
   exactly as a fresh machine's first run over the same registry runs, which
   is S1 read literally, and that settles the issue's question of whether the
   fallback is the registry's budget or a fresh unlimited one: it is the
   registry's, and for a registry with none installed that *is* a fresh
   unlimited meter, so `meter()` reads this run's counts alone. A budget
   installed in the registry with `HostRegistry::set_budget` is not a run's: it
   bounds every run over the registry for the registry's life (`cove run`'s
   `[run.<name>]` limits), so a spent or cancelled one stays spent for a reused
   machine and a fresh one alike. Resuming a parked or yielded run does not
   rebind — it is the same run, and keeps its budget (ADR 0082).

   It costs one uncontended lock per invocation, and for a registry with no
   budget one small allocation; nothing per instruction. On
   `cove-rules-measure 20000` (`--profile checked`, two runs each side, an
   x86-64 Mac) the floor row, an entry that does nothing, was 4.71 and 3.91 µs
   before and 4.14 and 5.00 µs after — inside the run-to-run spread — with the
   same 21 allocations a turn, the rules embedding's registry having a budget
   installed.
2. **A machine's resource table was never trimmed** — **fixed**,
   [issue #685](https://github.com/myuon/cove/issues/685),
   [ADR 0116](adr/0116-a-machine-s-resource-table-is-emptied-between-runs.md).
   `Machine::resource_word` interned every handle a run was handed into a
   table that lived as long as the machine and that nothing removed from
   (`src/vm/exec.rs`), and every resource operation scanned it linearly, so a
   resident machine handed a new handle per request grew by one entry a
   request and a handle operation got slower with the machine's age: a run
   minting and redeeming one ticket went from 21 µs to 420 µs a run over
   20,000 runs of one machine. Not an authority leak — the grant check is per
   call. Found by reading the code.

   The table is now per-run state: emptied at both edges of every entry,
   unless an object of the heap is pinned for a value the embedder holds — a
   kept closure may have a handle's word in its environment, and emptying the
   table under it makes that word name nothing or the wrong resource — and
   found by a hash lookup on the id. The same run costs 8 µs on its 20,000th
   run as on its first. A handle kept as a value is the name itself and
   survives an emptied table. Tested by
   `lifecycle.rs`'s `a_reused_machine_keeps_no_resource_from_its_earlier_runs_{encoded,native}`
   (1,000 runs, a failed one and a parkable one, the table empty after each,
   read through the `#[doc(hidden)]` `resources_held`) and
   `a_kept_handle_still_names_its_resource_{encoded,native}` (a handle kept as
   a value and a closure kept with a handle inside it, each used after runs
   that were handed other resources; the table emptied at the first edge after
   the closure is dropped).

   The exception was a residue: while the embedder held a closure the table
   was kept whole, so a machine whose embedder kept one for its whole life
   still grew by a name a run — 20,001 names after 20,000 runs. Since
   [issue #694](https://github.com/myuon/cove/issues/694) and
   [ADR 0119](adr/0119-a-held-closure-keeps-only-the-resource-names-it-reaches.md)
   the pinned objects are walked for their `Repr::Host` words at an edge and
   only the names they reach are kept, each at its own word; the walk is
   paced by the names it can drop (at least 32, and a sixteenth of what the
   last walk read), so the same 20,000 runs end with 2 names and cost what
   they did (3.8 to 4.0 µs a run against 3.8 to 4.2). Tested by
   `lifecycle.rs`'s `a_held_closure_keeps_only_the_names_it_reaches_{encoded,native}`
   (a thousand runs bounded, a run handed a hundred resources leaving exactly
   the closure's name, a closure over a vector of a hundred handles keeping
   all of them, each handle still naming its own resource, the table emptied
   once the closure goes) and
   `a_name_is_kept_while_any_held_value_reaches_it_{encoded,native}` (a
   closure in a closure, a closure in a struct and a cell a closure rewrites,
   each name dropped once nothing held reaches it), and the walk itself by
   `mem.rs`'s `the_resource_words_a_pinned_object_reaches_are_found_and_no_others`.
3. **A handle carries no registry identity** (S4, ADR 0114's alternatives).
   Handed by an embedder to a run over another registry that grants its
   capability, it names whatever that registry's host has under the same id.
   The embedder's responsibility by ADR 0114; recorded so that a host that
   starts passing handles between tenants knows.

Not findings, but limits on what the tests reach:

- **A host operation as a value does not lower** on the linear-memory backend
  (`let operation = files.write` is "not yet lowered: a name reached through a
  module"), so that route is tested on the interpreter only.

## What is not tested, and why

- **Process-terminating OOM and stack overflow.** There is nothing to recover
  and nothing to check after; they end the test binary. They are fuzzing
  findings (FUZZING.md).
- **Leaks.** AddressSanitizer on macOS has no LeakSanitizer, so the ASan runs
  below find use-after-free, double free and overflow, not memory never freed.
  What the tests check instead is that every host request is dropped exactly
  once, and that the next run allocates exactly what a fresh one does. The
  fuzz smoke runs on Linux with LeakSanitizer on (FUZZING.md), which is
  where issue #688's ring of `Shared` cells was found;
  `crates/cove-runtime/tests/shared_cycles.rs` holds the fix on every host by
  counting the cells still alive after a run.
- **The interpreter's reuse.** S1 is tested on the encoded and native tiers;
  the interpreter (`Interpreter::invoke` many times) is covered by the
  authority tests and by finding 1's cases only. It is the oracle, not a tier an
  embedder pools.
- **The `cove-wasm` playground**, whose runs are one per call from JavaScript.
- **The native tier's generated code under ASan.** The machine code the tier
  emits is not instrumented; ASan sees only what it calls into. FUZZING.md has
  the same note.
- **Concurrency between machines** beyond the edge server's pool and the
  storm's one thread: two machines over one registry at once is
  `parking.rs`'s `concurrent_runs_over_one_registry_are_charged_to_their_own_budgets`.

## How to run

`cargo t` runs everything above but the long storm.

Under AddressSanitizer (nightly, macOS or Linux x86-64; no `-Zbuild-std`
needed, the standard library is left uninstrumented, as `cargo fuzz` does):

```console
$ RUSTFLAGS=-Zsanitizer=address CARGO_TARGET_DIR=target/asan \
    cargo +nightly test --profile checked --target x86_64-apple-darwin \
    -p cove-runtime --features template \
    --test lifecycle --test authority --test owned_lifetimes -- --include-ignored
$ RUSTFLAGS=-Zsanitizer=address CARGO_TARGET_DIR=target/asan \
    cargo +nightly test --profile checked --target x86_64-apple-darwin \
    -p cove-edge --test isolation
```

`--target` is what keeps the sanitizer off proc macros. Run 2026-10-24: every
case passed, no report.

Under Miri (`rustup +nightly component add miri`):

```console
$ MIRIFLAGS=-Zmiri-disable-isolation CARGO_TARGET_DIR=target/miri \
    cargo +nightly miri test -p cove-runtime --test owned_lifetimes
```

`-Zmiri-disable-isolation` because a budget reads the clock. It builds and
starts, but the front end — checking the whole standard library to get the
program the cases run — runs under Miri too: on 2026-10-24 it was still inside
the first case after 46 minutes of CPU on an x86-64 Mac, with nothing
reported, and was stopped. So `OwnedVm`'s `unsafe` has **not** been checked
by Miri yet. What would make it practical is a program that does not go
through the checker — a lowered image read back with `cove_ir::serial`, as
`cove fmt` reads covefmt's, run with `Vm::invoke_lowered` — which this file
leaves for the next step.
