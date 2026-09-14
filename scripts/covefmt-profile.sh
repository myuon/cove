#!/bin/bash
#
# Where a covefmt run's *machine* time goes, by component, for each tier.
#
# [Issue #369](https://github.com/myuon/cove/issues/369) item 6 asks for the
# native run's time attributed to generated code, encoded dispatch, the
# open/close/call helpers, safepoints, frame growth and zeroing, argument and
# return copies, runtime builtins, allocation and collection, and tier lookup —
# and says in as many words not to infer any of it from dispatched-instruction
# percentages, because #367 and #368 showed a call instruction was worth far
# more wall time than its opcode share.
#
# So this is a sampling profile of the real process, and the instrument is
# **`/usr/bin/sample`, which ships with macOS**. Nothing is installed: the
# repository's ablation harnesses (`cove-native-compare`, `scripts/ablate`)
# decompose the *call path* at nanosecond resolution and cannot see a whole run,
# and `--profile` is refused beside `--backend native` for the reason ADR 0055
# gives — it counts dispatched opcodes and the native tier dispatches none.
#
# # What the method costs, and what it cannot do
#
# **It is about 10% slow.** Measured on this workload the bench's own `whole`
# is 668 ms under the sampler against 608 ms without it, and the overhead is
# the same on both arms (668 against 669), so it does not favour either. Read
# the *shares*, never the times.
#
# **The noise floor is the spread between runs, not the square root of the
# sample count.** At five runs an arm, a bucket's share moves by ±0.3
# percentage points between runs and the 50% bucket by ±1.0 — several times the
# Poisson floor. A difference smaller than that is not a difference.
#
# **The unwinder cannot walk out of generated code.** A sample taken inside a
# JIT page has no symbol and no caller, so it appears as a root of its own.
# Self time is unaffected and is what this reports; an *inclusive* attribution
# above a generated frame is not available at all.
#
# **An inlined callee is charged to its inliner.** `--profile checked` carries
# no debug info, so `Machine::safepoint` and `Memory::push_frame` are partly
# inside `encoded::dispatch` and the safepoint and frame-zeroing buckets are
# **lower bounds**. That is the one place this profile and the per-call
# ablation tables of #365 and #368 disagree, and the ablation is right there.
#
# The two arms are run alternately for the reason `covefmt-tiers.sh` interleaves
# them, and the reading is the **difference between the two columns**: what the
# native tier took away, and what it added to take it.
#
# Usage, from anywhere in the repository:
#
#     cargo build --profile checked -p cove-cli --features template
#     scripts/covefmt-profile.sh [rounds]
set -euo pipefail

rounds=${1:-5}
root=$(cd "$(dirname "$0")/.." && pwd)
binary=$root/target/checked/cove

if [[ ! -x $binary ]]; then
  echo "covefmt-profile.sh: no binary at $binary" >&2
  echo "build one: cargo build --profile checked -p cove-cli --features template" >&2
  exit 1
fi
if [[ ! -x /usr/bin/sample ]]; then
  echo "covefmt-profile.sh: needs /usr/bin/sample, which is macOS's own sampler" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# `target/checked/cove` is not reliably a feature build: cargo replaces it with
# whichever feature set was last asked for, so an ordinary `cargo t` between two
# runs of this script puts the default build back. See the same check, and the
# longer reason, in `covefmt-tiers.sh`.
if (cd "$root/examples" && "$binary" run covefmtBench --files-root "$root" \
  --backend native --fuel 1 >/dev/null 2>"$work/probe"); then
  echo "covefmt-profile.sh: the native tier answered a run it should have run out of fuel on" >&2
  exit 1
fi
if grep -q "native execution is unavailable" "$work/probe"; then
  sed 's/^/  /' "$work/probe" >&2
  echo "covefmt-profile.sh: build the binary this measurement needs:" >&2
  echo "  cargo build --profile checked -p cove-cli --features template" >&2
  exit 1
fi

for n in $(seq "$rounds"); do
  for arm in native vm; do
    (
      cd "$root/examples"
      "$binary" run covefmtBench --files-root "$root" --backend "$arm" \
        >"$work/out-$arm-$n" 2>/dev/null &
      pid=$!
      # Long enough to outlast the run, and `sample` stops when the process does.
      /usr/bin/sample "$pid" 30 1 -file "$work/$arm-$n.txt" >/dev/null 2>&1 || true
      wait $pid
    )
    printf '%s %s\n' "$arm" \
      "$(awk '/^whole/ {print $2}' "$work/out-$arm-$n")" >>"$work/whole"
  done
  printf '  round %d of %d\n' "$n" "$rounds"
done

echo
echo "the bench's own \`whole\`, in ms, under the sampler -- the method's own cost"
sort "$work/whole" | awk '{a[$1]=a[$1]" "$2} END {for (k in a) print "  " k ":" a[k]}'

for arm in vm native; do
  echo
  echo "======== $arm ========"
  python3 "$root/scripts/covefmt-profile.py" "$work/$arm"-*.txt
done
