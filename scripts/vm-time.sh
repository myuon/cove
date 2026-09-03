#!/bin/bash
#
# The median of a benchmark's `execute=` time, over repeated runs.
#
# `docs/VM_ARCHITECTURE.md` measures small backend changes this way rather
# than with `cove-bench`: fifteen runs of one benchmark through the `cove`
# binary reproduce to under a percent in seconds, where the whole suite takes
# minutes. Every ablation table in that document was taken with this loop, so
# it is written down here instead of being retyped each time.
#
# The tables it took were the predecessor backend's, and that backend was
# deleted at ADR 0034's cutover. The loop is not: what it measures is the
# `execute=` figure `--stats` prints, which the backend that replaced it
# prints in the same words. It names no backend below, because there is now
# one that a `cove` command runs a program on and naming it would be naming
# the default.
#
# Usage, from anywhere in the repository:
#
#     scripts/vm-time.sh arith [iterations] [binary]
#
# It prints one line: the benchmark, the minimum, the median, the maximum,
# the spread as a percentage of the median, and the instructions the run
# executed. A distribution rather than a number, because a single run of any
# of these says very little -- issue #123 is the one that asks for that in
# so many words.
#
# The binary defaults to `target/release/cove`, so a comparison between two
# builds is made by copying each one somewhere and naming it here. Nothing
# else may be running: these are wall-time measurements on an idle machine.
set -euo pipefail

bench=${1:?usage: vm-time.sh <bench> [iterations] [binary]}
iterations=${2:-15}
root=$(cd "$(dirname "$0")/.." && pwd)
binary=${3:-$root/target/release/cove}

cd "$root/benches"

for _ in $(seq "$iterations"); do
  "$binary" run "$bench" --stats 2>&1 >/dev/null | grep '^backend:'
done | BENCH="$bench" python3 -c '
import os, re, statistics, sys

# `execute=` is printed by `Duration`\''s own Debug formatting, so it carries a
# unit and the unit changes with the size of the number.
scale = {"ns": 1e-6, "µs": 1e-3, "ms": 1.0, "s": 1e3}
samples, instructions = [], None
for line in sys.stdin:
    text = re.search(r"execute=([0-9.]+)(\D+?)\s", line).groups()
    samples.append(float(text[0]) * scale[text[1]])
    instructions = re.search(r"instructions=(\S+)", line).group(1)

samples.sort()
median = statistics.median(samples)
print("%s min=%.2fms median=%.2fms max=%.2fms spread=%.1f%% instructions=%s"
      % (os.environ["BENCH"], samples[0], median, samples[-1],
         100 * (samples[-1] - samples[0]) / median, instructions))
'
