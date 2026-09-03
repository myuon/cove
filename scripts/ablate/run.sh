#!/bin/bash
#
# The ablation builds issue #123's calibration was measured with.
#
# **None of these patches applies any more.** Every one of them is written
# against `crates/cove-runtime/src/vm.rs`, and ADR 0034's cutover deleted that
# file along with the rest of the backend it belonged to. Every `Vm` and every
# `src/vm` path named below is that backend's; the replacement took the name
# in the commit after the deletion and has no `vm.rs`, no `Vm::charge` and no
# ablation of its own. They are kept
# because `docs/VM_ARCHITECTURE.md` is kept, and for the same reason: that
# document records what each mechanism was measured to cost, six accepted ADRs
# cite its sections, and a table whose method has been thrown away is a table
# nobody can check. Recalibrating the linear-memory backend means writing new
# patches against its dispatch loop and a new table beside the old one, not
# editing these.
#
# Each `.patch` beside this script removes one thing the VM's dispatch path
# carries, so that what it costs can be read off the difference. Several of
# them are unsound and none of them is a proposal: an ablation says what a
# mechanism costs *in the arrangement it was measured in*, which is why
# `docs/VM_ARCHITECTURE.md` keeps the negative results beside the positive
# ones. They live here as patches rather than as a feature flag in the
# runtime because a flag would have to be carried by every production run to
# make one measurement possible, and #123 measured what that would cost: a
# branch on a `bool` in `Vm::charge` recovers none of what removing the
# counter recovers, because the branch costs what the increment costs.
#
# A patch is expected to rot. It is written against the `Vm::charge`,
# `Vm::safepoint`, `Vm::back_edge` and `Vm::enter` of the commit that added
# it, and `git apply` refuses loudly when one of those moves, which is the
# behaviour wanted: an ablation that applied to code it was not written for
# would measure something nobody named.
#
#     scripts/ablate/run.sh              # build every variant into /tmp
#     scripts/ablate/run.sh nocount      # build one
#
# Then measure each against the unmodified build, which this also produces
# as `/tmp/cove-production`:
#
#     scripts/vm-time.sh arith 15 /tmp/cove-production
#     scripts/vm-time.sh arith 15 /tmp/cove-nocount
#
# Measure the unmodified build on both sides of the variants rather than
# once. Two brackets that disagree mean the machine moved under the study
# and nothing between them is readable.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

if ! git diff --quiet; then
  echo "ablate: the working tree is dirty; these patches are applied to it and reverted" >&2
  exit 1
fi

wanted=${1:-}
patches=$(ls "$root/scripts/ablate"/*.patch)

cargo build --release -p cove-cli -p cove-bench
cp target/release/cove /tmp/cove-production
cp target/release/cove-bench /tmp/cove-bench-production

for patch in $patches; do
  name=$(basename "$patch" .patch)
  if [ -n "$wanted" ] && [ "$name" != "$wanted" ]; then
    continue
  fi
  echo "=== $name"
  git apply "$patch"
  cargo build --release -p cove-cli -p cove-bench
  cp target/release/cove "/tmp/cove-$name"
  cp target/release/cove-bench "/tmp/cove-bench-$name"
  git apply -R "$patch"
done

# The tree is left as it was found, and the unmodified build is left in
# `target/` so that a measurement made straight after this reads the
# production binary rather than the last variant.
cargo build --release -p cove-cli -p cove-bench
