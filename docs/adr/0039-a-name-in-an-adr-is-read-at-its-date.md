# ADR 0039: A name in an ADR is read at the ADR's date

- Status: Accepted
- Date: 2026-09-03
- Supersedes: nothing, and that is the substance of it.
  [ADR 0019](0019-executable-ir-and-vm.md) decided that an executable IR and
  a dedicated VM exist and that the tree walk stays the semantic oracle;
  [ADR 0022](0022-the-vm-is-the-default-backend.md) decided that the VM is
  what a command reaches when nobody names a backend. Both decisions
  *survived* the replacement of the machine they were about, which is why
  neither is superseded here. [ADR 0034](0034-one-physical-word-stack.md)
  ordered the deletion of named artefacts, they were deleted, and the names
  were then free. No existing ADR is edited by this one, including its header
- Decides: how a reader resolves a crate, module, type, path or flag name
  written in an accepted ADR
- Implementation status: nothing to implement. `feat: delete the predecessor
  backend` (b094d82) is ADR 0034's completion condition 8, and `refactor: the
  replacement takes the predecessor's names` (6e90085) is the commit after
  it. That commit's own message says a superseding ADR is the only repair the
  convention allows and that it is the next change; this is that change, and
  it adds no code and moves no file

## Context

ADR 0034 decided that reaching one linear memory is a clean replacement of
the executable IR, the lowering and the VM rather than a renovation of them,
and made completion conditional on ten things. The eighth is a deletion
(`0034-one-physical-word-stack.md:236-238`):

> the replacement becomes the production path and the predecessor executable
> IR, Vm, FrameVm, admits mechanism, duplicate heap and migration machinery
> are deleted;

A replacement built beside the thing it replaces needs a second spelling for
the length of the build, because two backends in one tree cannot both be
called the VM. The replacement had one throughout: crate `cove-lir`, module
`cove_runtime::lvm`, type `Lvm`, flag `--backend lvm`, harness
`crates/cove-cli/tests/lvm_coverage.rs`.

Commit b094d82 deleted the predecessor — `cove-ir`, `vm.rs`, `frame.rs`,
`slot.rs`, the `admits` mechanism and its coverage ratchet, the duplicate
heap, and the `Backend::{Vm, Frame}` bench rows. Commit 6e90085, the next
one, took the freed names: `cove-lir` became `cove-ir`, `cove_runtime::lvm`
became `cove_runtime::vm`, `Lvm` became `Vm`, `--backend lvm` became
`--backend vm`. The reason is the one `docs/LINEAR_VM.md` had recorded in
advance: "linear" describes the memory model and not the IR, which is a
register IR, and it is not a name worth keeping once there is nothing left to
distinguish it from. No behaviour changed; the diff over the coverage harness
was seventeen name substitutions.

**The rename created a hazard the deletion did not.** A stale name that no
longer resolves is obviously stale. A stale name that resolves to something
else is not. In the window between b094d82 and 6e90085, ADR 0019's
`crates/cove-ir` was a dangling path: a reader who followed it found nothing
and learned, in one step, that they were reading about a machine that is
gone. After 6e90085 the path exists. It contains a *different* machine —
a different instruction set, a different value model, and a different
coverage number — and the ADR that names it was not edited, because an
accepted ADR is immutable. The record changed meaning without changing a
character.

In code the same substitution had the same problem and could be repaired:
6e90085 found nine sites that said `Vm` or `cove-ir` meaning the predecessor
— a provenance note that after substitution read "replaced them with
themselves", a test comment denying its own subject, doc paths into
`src/vm/tests/` belonging to a deleted backend — and reworded each to name
the predecessor as the predecessor. An ADR cannot be reworded. The convention
in `CLAUDE.md` forbids it, and it forbids it for exactly the reason that
makes this ADR necessary: an ADR is a record of what was believed when it was
written, and editing one destroys the thing it exists to preserve.

### What resolves now, and to what

Ten accepted ADRs name the predecessor in words that today resolve to the
replacement. Line numbers are at this ADR's commit.

- **`0019-executable-ir-and-vm.md:18-19`** — "`crates/cove-ir` lowers and
  validates; `cove_runtime::vm` executes; `cove run --backend vm` selects
  it". All three resolve, to `crates/cove-ir/`, to
  `crates/cove-runtime/src/vm/mod.rs`, and to `Backend::Vm` in
  `crates/cove-cli/src/main.rs:1428`. The figures on the two lines after them
  are the predecessor's: "Of 119 cases, 43 lower and agree on both, 51 are
  refused by name, and 25 do not check" (`:21-22`, restated at `:199-200`).
  The replacement's survey is `crates/cove-cli/tests/vm_coverage.rs`: 116 of
  the 149 programs the repository keeps (`:135-136`) lower, run and agree
  (`AGREEING_FLOOR`, `:316`), nothing that lowers disagrees
  (`KNOWN_DISAGREEMENTS` is empty, `:344-352`), and nothing fails to lower.
- **`0022-the-vm-is-the-default-backend.md`** — its *title* is now a sentence
  about today's backend, and it happens to still be true, which is the trap
  in miniature: nothing about the reuse tells a reader whether a surviving
  sentence survived or was merely reread. Inside it, `:46` enumerates "every
  `Vm` field" of a struct whose fields were `stack` and `scalars`; `:137`
  names `cove_ir::lower::lower_entry`, which resolves, to
  `crates/cove-ir/src/lower/mod.rs:264`; `:204` names `--backend vm`.
- **`0021-places-are-a-static-fact.md:25, 41, 112`** — `cove_ir::lower`,
  which resolves (`crates/cove-ir/src/lib.rs:46`). The ADR's claim is about
  what *that* lowering settled before the VM was handed anything.
- **`0020-a-diagnostic-stream-for-console.md:131`** —
  `cove_ir::Inst::CallHost { module, op, argc }`. The path resolves as far as
  the variant (`crates/cove-ir/src/inst.rs:247`) and the destructuring is
  wrong: the fields are `dst`, `op: HostOpId`, `args: ArgsId`. The ADR's
  decision is untouched — a `HostOp` still carries the module and the
  operation name (`crates/cove-ir/src/program.rs:97`), so a new operation on
  an existing module is still invisible to the IR — but the code it quotes as
  evidence is a different instruction.
- **`0023-a-replay-chooses-its-backend.md:114, 178`** and
  **`0026-a-trace-names-the-backend-that-recorded-it.md:176`** —
  `--backend vm`, which is accepted and runs the replacement.
- **`0028-five-representations-and-one-is-public.md:229, 372, 400, 454,
  811`** — `cove_ir::Function` (resolves,
  `crates/cove-ir/src/program.rs:174`), `cove_ir::lower`, `Vm::invoke`
  (resolves,
  `crates/cove-runtime/src/vm/mod.rs:197`, and does the same job for a
  different machine), and `cove-ir` as a crate — once among those that must
  go through constructors and readers, and once in a count of sixty-six
  `Value` mentions to be changed in it and two others.
- **`0031-a-host-handle-is-not-a-vm-handle.md:135, 247`** — `cove-ir`, once
  as the crate that "names slots publicly and always has", and once as the
  crate that had zero code sites in a Costs-section estimate. The second is a
  count about a deleted crate that now reads as a count about a live one.

### The two sharpest cases

**`0030-a-host-call-asks-the-fuel-limit.md:85, 118, 122`** is the sharpest
single example, because the receiver is real and the message is not. It names
`Vm::charge_at_host_boundary` twice, `Vm::safepoint`, and `Vm::collect`.
`Vm` resolves — `crates/cove-runtime/src/vm/mod.rs:136` — and has none of
the three. `charge_at_host_boundary` and `collect` belong to the private
`Machine` (`crates/cove-runtime/src/vm/exec.rs:798` and `:1747`) and
`safepoint` belongs to the budget (`crates/cove-runtime/src/budget.rs:268`).
A reader who checks the type and finds it will conclude the ADR is wrong
about the runtime rather than right about a different one. ADR 0030's
*decision* is in better shape than its evidence: b094d82 found the
replacement charging fuel only every 1024 instructions, fixed it to charge at
every host boundary before dispatch, and measured the replacement bounding
Host effects by fuel more tightly than either backend ADR 0030 was written
against.

**`0034-one-physical-word-stack.md:14, 43, 46, 110, 237`** is the case a
reader is most likely to hit, and it needs saying plainly. `Vm` and `cove_ir`
appear in ADR 0034's *own* deletion list and in the Context that argues for
it: `:14` calls "the current executable IR, Vm and FrameVm" predecessor
implementations, `:43` says the calling convention is "now [a fact] in
cove_ir", `:46` names "the production Vm's value/scalar/place stores", `:110`
says slot numbering is "already carried by cove_ir::Function", and `:237`
orders that "the predecessor executable IR, Vm, FrameVm, admits mechanism,
duplicate heap and migration machinery are deleted". Read at today's date,
condition 8 orders the deletion of the survivor ADR 0034 itself mandated, and
the Context cites the replacement as evidence for replacing it. Read at ADR
0034's date, 2026-09-01, which is before either commit, every one of those
five names the predecessor, the condition was met by b094d82, and nothing in
ADR 0034 is either wrong or outstanding.

### The names that do not resolve, and are safe for that reason

The contrast is the argument. `0023-a-replay-chooses-its-backend.md:117`
names `cove_ir::Unsupported::to_diagnostic`; there is no `Unsupported` in
`cove-ir` and `crates/cove-ir/src/lib.rs:31` says so in as many words.
`0021-places-are-a-static-fact.md:117` says "`cove_ir`'s `Binding` no longer
carries a `writable` one"; there is no `Binding`. `:223` and
`0022-the-vm-is-the-default-backend.md:217` count refusals in
`crates/cove-ir/src/lower.rs`, a file that does not exist — the lowering is
`src/lower/mod.rs` and a directory beside it.
`0028-five-representations-and-one-is-public.md:40, 56, 177` name `Vm::stack`
as a `Vec<Value>`, `Vm::scalars` as a `Vec<i64>`, and `Vm::arg_vectors`, and
today's `Vm` has five fields and none of those. Each of these tells a reader
in one step what none of the previous section's names tell them at all.

### Two transitional names left behind in an ADR

`0038-a-type-nothing-settles-is-not-a-program.md:92` names `cove_lir`'s
`Shapes`, and `:167` names `cove_lir::lower::shapes`'s `host_ty`. Both items
exist — `crates/cove-ir/src/lower/shapes.rs`, and `host_ty` at `:914` — and
the crate is spelled `cove_ir`. ADR 0038 was accepted the day of the rename
and cannot be edited, so this is the sentence that records the substitution:
read `cove_lir` as `cove_ir` throughout ADR 0038.

## Decision

**A name in an accepted ADR is read at that ADR's date.** When a name is
reused, what the older ADR meant by it is the thing that existed when the ADR
was written, not the thing that answers to the name now.

Three things follow, and the second is the one that does the work.

**A name may be reused once the thing it named is deleted.** Nothing in this
project reserves a spelling against its former occupant. The names above were
freed by ADR 0034's condition 8 and taken by the commit after the deletion,
and that was the right order: the predecessor was gone before anything else
answered to its name, so no build, no test and no command was ever ambiguous.

**Resolution is not evidence.** That a path, module, type or flag written in
an ADR exists today says nothing about whether the ADR was talking about it.
A reader checking an ADR's claim against the tree must first ask whether the
artefact under that name is the one the ADR meant, and the date in the
header, not the tree, is what answers. This is the rule that makes ADR 0034's
deletion list readable and ADR 0030's `Vm::` prefix harmless.

**A figure keeps the provenance of the run that produced it.** ADR 0019's
"of 119 cases, 43 lower and agree" is a count of the deleted backend and
stays one; the replacement's number is 116 of 149 with an empty disagreement
set, and it is a different measurement of a different machine, not a later
value of the same series. The same holds for every benchmark row attributed
to a backend, under [ADR 0029](0029-a-benchmark-number-is-evidence-within-one-build.md)'s
rule that a number is evidence within one build.

## Consequences

- Ten ADRs — 0019, 0020, 0021, 0022, 0023, 0026, 0028, 0030, 0031 and
  0034 — contain names that resolve to something they are not about, and stay
  as written. This ADR is the index of them, and it is the only place a
  reader is told.
- A future reuse of a freed name costs another ADR like this one, and the
  cost is now known: one document, one index of sites, no edits. It is the
  price of not carrying a migration's vocabulary permanently, and it is paid
  once by the reader of the record rather than continuously by the reader of
  the code.
- The two commits are deliberately separate and stay so in the history.
  b094d82 deletes and 6e90085 renames, so `git show 6e90085` is a pure
  substitution — which is what makes "the names were free when they were
  taken" checkable rather than asserted.
- `examples/life/README.md` shows `cove run life --backend vm --stats` with
  figures the predecessor produced and no provenance note. The command works
  again and the numbers are a deleted backend's; this ADR names it as an
  instance of the hazard and does not annotate it, because annotating a
  measurement is a judgement about the measurement rather than a rename.

## What is not decided here

- **`docs/VM_ARCHITECTURE.md` is not repaired here.** It is not an ADR, it is
  not immutable, and a separate change is fixing its pointers. This ADR
  governs how to read the immutable record and says nothing about a document
  that can simply be corrected.
- **What the affected ADRs' figures are *worth* is not decided.** This ADR
  fixes their provenance and stops there: a coverage count or a benchmark row
  attributed to the old backend is still that backend's, and ADR 0029 already
  governs how far any benchmark number travels.
- **Whether ADR immutability should gain an exception for a mechanical
  rename** is not reopened. The convention in `CLAUDE.md` is unchanged, and
  this ADR is what it prescribes.
- **Whether a future transitional name should be chosen so as never to need
  a rename** — a name the replacement could keep from the start — is a
  question for the next migration, not a rule adopted here.

## Alternatives considered

**Keep the transitional names forever.** `cove-lir`, `cove_runtime::lvm`,
`Lvm` and `--backend lvm` would have stayed, and the whole hazard would not
exist: every name in every one of the ten ADRs would be unambiguously dead,
a reader following `crates/cove-ir` would find nothing, and no ADR like this
one would be needed. That is a real benefit and it is the strongest case
against what was done — a reader who mistakes a live name for a dead one is
worse off than a reader who finds a dangling path, because the first is
misled and the second is merely stopped.

It lost on where the cost falls. `lir` and `lvm` mean "the second one, built
beside the first". Keeping them makes the permanent vocabulary of the code a
permanent record of a migration that is over: every reader of
`crates/cove-lir` forever asks what the `l` distinguishes it from, and the
answer is a crate that was deleted. `docs/LINEAR_VM.md` had already recorded
that "linear" describes the memory model and not the IR, which is a register
IR, so the name was not even accurate about the survivor. Against that, the
cost of the rename is one document read once by whoever reads ten old ADRs.
Every reader of the code pays the first forever so that a reader of the
record pays the second once.

**Ship a compatibility alias for `--backend lvm`.** Rejected, with no
deprecation path and no second spelling, and the reasoning is in the code
rather than only here: `crates/cove-cli/src/main.rs:1423-1427` says the flag
"ran under the transitional spelling `lvm` between ADR 0034's cutover and the
rename that followed, and nothing was built to keep that spelling alive — no
alias, no deprecation path, no second spelling", and
`crates/cove-runtime/src/trace.rs:142-152` says the same of a trace header
and gives the measurement: the window is measured in *commits*, the trace
format version had already moved to 4 in the same cutover, and a trace is a
recording of a run that can be taken again. An alias would make a name that
is scheduled to mean nothing readable forever, which is the same permanent
cost as the previous alternative for a smaller benefit — and it would have
given `--backend` two names for one backend, in a toolchain where five
commands share one list of accepted names
(`Backend::NAMES`, `crates/cove-cli/src/main.rs:1460`) precisely so the set
cannot drift.

**Edit the ten ADRs.** The mechanical fix — substitute `cove-ir` for the
predecessor's name in ADR 0019, add a note to ADR 0034's condition 8 — is
forbidden by `CLAUDE.md` and would be wrong even if it were not. ADR 0034's
deletion list is the clearest case: rewriting it to say which `Vm` it meant
would erase the fact that when ADR 0034 was written there was only one, which
is the whole reason the list reads the way it does. The pair — an untouched
record and a later ADR saying how to read it — keeps both what was believed
and what was learned. That is the same argument the convention makes for
supersession, applied to a change that supersedes nothing.

**Say nothing, and let readers work it out.** The rename is discoverable from
`git log`, and someone who suspects a name has moved can find the commit that
moved it. This lost on the shape of the failure: the reader who most needs
the warning is the one who does *not* suspect, because the name resolved and
nothing looked wrong. A hazard whose symptom is the absence of a symptom is
exactly the kind that has to be written down in the place a reader is already
looking.
