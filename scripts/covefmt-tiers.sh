#!/bin/bash
#
# The three formatters over one corpus: Rust, the encoded VM, and the native
# tier — interleaved, repeated, and checked.
#
# `examples/covefmt/README.md` publishes what this prints, and
# [issue #369](https://github.com/myuon/cove/issues/369) is what asks for it:
# "measure full covefmt ... use repeated interleaved runs, separate cold from
# warm samples, and report median/min/max". This is that measurement written
# down rather than retyped, for the same reason `scripts/vm-time.sh` and
# `scripts/perf-cq.sh` are.
#
# Usage, from anywhere in the repository:
#
#     cargo build --profile checked -p cove-cli --features template
#     cargo build --profile checked -p cove-bench
#     scripts/covefmt-tiers.sh [warm runs] [cove binary] [cove-fmt-phases binary]
#
# Three things about the shape are load-bearing.
#
# **The arms are interleaved, not batched.** One iteration runs Rust, then the
# VM, then the native tier, and the next iteration does the same. Batched —
# ten of one arm and then ten of the next — a thermal drift or a background
# process that starts halfway through is charged entirely to whichever arm was
# running, and there is no way to tell from the numbers that it happened.
# Interleaved, it is charged to all three, which is a smaller error and a
# visible one: it widens every spread at once.
#
# **The first iteration is reported apart.** It reads 248 files off a cold
# page cache and pages in an 8 MB binary, and it is a different measurement
# from the nine after it rather than a noisy sample of the same one. Averaging
# it in would put a first-run cost into a steady-state median; throwing it away
# would hide a cost a user pays every time.
#
# **Every run is checked, and a check that fails stops the script.** Both Cove
# arms print the corpus size, the five oracle scores and the formatter's own
# verdict; this diffs the native arm's output against the VM's byte for byte on
# every iteration and asserts the corpus facts on every one. A wall-time table
# over a run that formatted something else is not a measurement, and a partial
# script that leaves a plausible table behind is the failure this repository
# has already had once.
#
# What it prints, per arm: total process wall time; the three pipeline phases
# the Cove bench times by difference; the whole pipeline, which is the figure
# comparable with `cove fmt --check`; the backend's own `execute=`; the
# instructions the encoded tier dispatched; the allocations, the words they
# took and the collections; and for the native arm, the compilation time, the
# machine-code bytes and the four tier transition counts.
#
# One heavy command at a time and nothing else running: these are wall-time
# measurements, and this repository has tests that assert timing maxima.
set -euo pipefail

warm=${1:-9}
root=$(cd "$(dirname "$0")/.." && pwd)
binary=${2:-$root/target/checked/cove}
# The Rust arm's phase table. Optional: without it the Rust column holds only
# the process wall time, which is the one figure `cove fmt --check` reports on
# its own. Build it with `cargo build --profile checked -p cove-bench`.
phases=${3:-$root/target/checked/cove-fmt-phases}

if [[ ! -x $binary ]]; then
  echo "covefmt-tiers.sh: no binary at $binary" >&2
  echo "build one: cargo build --profile checked -p cove-cli --features template" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The native arm needs a binary built with the code generator's feature, and
# `target/checked/cove` is **not** reliably that binary: cargo replaces it with
# whichever feature set was last asked for, so an ordinary `cargo t` or
# `cargo clippy --workspace` between two runs of this script puts the default
# build back, and this measurement then dies fourteen rounds in with "exited 1".
# One cheap run with a fuel limit of one asks the question before anything is
# timed: an unavailable tier is a diagnostic on stderr, and a `native` that
# works reaches its fuel limit instead.
#
# `--fuel 1` and not `--help`, because what has to be established is that this
# build's `--backend native` can *compile and enter* the tier on this host, which
# is three separate ways to be unavailable (no feature, not x86-64, no executable
# mapping) and none of them is visible in a usage message.
if (cd "$root/examples" && "$binary" run covefmtBench --files-root "$root" \
  --backend native --fuel 1 >/dev/null 2>"$work/probe"); then
  echo "covefmt-tiers.sh: the native tier answered a run it should have run out of fuel on" >&2
  exit 1
fi
if grep -q "native execution is unavailable" "$work/probe"; then
  sed 's/^/  /' "$work/probe" >&2
  echo "covefmt-tiers.sh: build the binary this measurement needs:" >&2
  echo "  cargo build --profile checked -p cove-cli --features template" >&2
  exit 1
fi

# One command, timed, with the clock inside the process that spawns it: macOS
# ships bash 3.2, which has no `EPOCHREALTIME`, and `/usr/bin/time` reports
# hundredths of a second — which is a 17% quantisation on the Rust arm's 60 ms.
# It prints the milliseconds and nothing else, and a non-zero exit is an error
# here rather than a slow run.
cat >"$work/time.py" <<'PYTHON'
import subprocess
import sys
import time

started = time.monotonic()
done = subprocess.run(sys.argv[3:], stdout=open(sys.argv[1], "w"), stderr=open(sys.argv[2], "w"))
print(round((time.monotonic() - started) * 1000))
if done.returncode != 0:
    sys.exit(f"covefmt-tiers.sh: `{' '.join(sys.argv[3:])}` exited {done.returncode}")
PYTHON

cat >"$work/phases.py" <<'PYTHON'
"""`cove-fmt-phases`'s four numbers, as the Rust arm's samples."""
import os
import re
import sys

phase = os.environ["PHASE"]
text = open(sys.argv[1]).read()
for name in ("lex", "parse", "print", "whole"):
    found = re.search(r"^" + name + r"\s+([0-9.]+) ms", text, re.M)
    if found is None:
        sys.exit(f"covefmt-tiers.sh: no {name} in cove-fmt-phases output")
    print(f"{phase} rust {name} {found.group(1)}")
PYTHON

# The bench prints its timings on stdout beside its verdicts, and the timings
# are the one part that differs between two correct runs. Stripping them is
# what makes "byte for byte" a check of the formatting rather than of the clock.
strip_timings() { grep -v '^lex\|^parse\|^print\|^whole'; }

# One iteration of all three arms, interleaved. `$1` is the label the samples
# are filed under: `cold` for the first, `warm` for the rest.
one_round() {
  local phase=$1 n=$2

  # Rust. `cove fmt --check` at the repository root is lex, parse, format
  # *and* compare, over the same files the Cove walk reaches — both skip
  # `target` and any directory whose name holds a dot.
  local wall arm
  wall=$(cd "$root" && python3 "$work/time.py" /dev/null /dev/null \
    "$binary" fmt --check)
  echo "$phase rust wall $wall" >>"$work/samples"

  # The Rust arm's three phases, which `cove fmt --check` does not separate.
  # One iteration here rather than nine inside the bin, so that the Rust
  # phases are interleaved with the Cove arms exactly as the wall times are.
  if [[ -x $phases ]]; then
    "$phases" "$root" 1 >"$work/phases.out"
    PHASE=$phase python3 "$work/phases.py" "$work/phases.out" >>"$work/samples"
  fi

  for arm in vm native; do
    wall=$(cd "$root/examples" && python3 "$work/time.py" \
      "$work/$arm.out" "$work/$arm.err" \
      "$binary" run covefmtBench --files-root "$root" --backend "$arm" --stats)
    echo "$phase $arm wall $wall" >>"$work/samples"
    PHASE=$phase ARM=$arm python3 "$work/read.py" \
      "$work/$arm.out" "$work/$arm.err" >>"$work/samples"
  done

  # The two Cove arms formatted the same corpus the same way, or this stops.
  strip_timings <"$work/vm.out" >"$work/vm.checked"
  strip_timings <"$work/native.out" >"$work/native.checked"
  if ! diff -q "$work/vm.checked" "$work/native.checked" >/dev/null; then
    echo "covefmt-tiers.sh: run $n ($phase): the native tier did not print what the VM printed" >&2
    diff "$work/vm.checked" "$work/native.checked" >&2 || true
    exit 1
  fi
  printf '  %s run %d: ok, and the two Cove arms agree byte for byte\n' "$phase" "$n"
}

cat >"$work/read.py" <<'PYTHON'
"""One run's numbers, as `<phase> <arm> <name> <value>` lines."""
import os
import re
import sys

out, err = (open(path).read() for path in sys.argv[1:3])
phase, arm = os.environ["PHASE"], os.environ["ARM"]
say = lambda name, value: print(f"{phase} {arm} {name} {value}")


def one(text, pattern, name, cast=int):
    found = re.search(pattern, text, re.M)
    if found is None:
        sys.exit(f"covefmt-tiers.sh: no {name} in the {arm} arm's output")
    say(name, cast(found.group(1)))


# The corpus facts, asserted rather than read: a table over 247 files is not
# this table. Issue #369's own figures were a corpus out of date by two files
# and 14,017 bytes, which is how this check came to be here.
#
# The file count is exact and the byte count is a band, and the asymmetry is
# deliberate. **The corpus is this repository**, so editing a doc comment
# anywhere in it moves the byte count — correcting the stale figures this check
# exists to catch moved it by 950 bytes on its own. An exact byte assertion
# would be a tripwire on every prose change in the tree, which is a gate nobody
# would keep. A 2% band is loose enough for prose and tight enough that a
# corpus which grew or shrank enough to invalidate a timing cannot pass.
FILES, BYTES, BAND = 248, 698481, 0.02
files, bytes_ = re.search(r"^(\d+) file\(s\), (\d+) byte\(s\)$", out, re.M).groups()
files, bytes_ = int(files), int(bytes_)
if files != FILES or abs(bytes_ - BYTES) > BAND * BYTES:
    sys.exit(
        f"covefmt-tiers.sh: the corpus is {files} files / {bytes_} bytes, and this "
        f"measurement is published against {FILES} files / about {BYTES}. Update the "
        "recorded facts in examples/covefmt/README.md and this script together, or "
        "explain the difference — do not average over two corpora."
    )
scores = re.findall(r"^(\d+) of (\d+) file\(s\) ", out, re.M)
if len(scores) != 5 or any(a != "248" or b != "248" for a, b in scores):
    sys.exit(f"covefmt-tiers.sh: the {arm} arm's five oracle scores are {scores}")

one(out, r"^lex\s+(\d+) ms", "lex")
one(out, r"^parse\s+(\d+) ms", "parse")
one(out, r"^print\s+(\d+) ms", "print")
one(out, r"^whole\s+(\d+) ms", "whole")
one(out, r"^lex\s+\d+ ms \((\d+) tokens\)", "tokens")
one(out, r"^print\s+\d+ ms \((\d+) bytes\)", "printed_bytes")

# `execute=` is `Duration`'s own Debug formatting, so it carries a unit and the
# unit changes with the size of the number.
scale = {"ns": 1e-6, "µs": 1e-3, "ms": 1.0, "s": 1e3}
found = re.search(r"execute=([0-9.]+)(\D+?)\s", err)
say("execute", round(float(found.group(1)) * scale[found.group(2)], 3))
one(err, r"instructions=(\d+)", "dispatched")
one(err, r"allocations=(\d+)", "allocations")
one(err, r"allocated_words=(\d+)", "allocated_words")
one(err, r"collections=(\d+)", "collections")
one(err, r"heap_words=(\d+)", "heap_words")

if arm == "native":
    one(err, r"compiled \((\d+\.\d)%\)", "compiled_percent", float)
    one(err, r"(\d+) reachable function", "reachable")
    one(err, r"compiled \((?:\d+\.\d)%\), (\d+) refused", "refused")
    one(err, r"(\d+) byte\(s\) of machine code", "code_bytes")
    one(err, r"compilation ([0-9.]+) ms", "compile", float)
    for name, label in (
        ("vm_to_vm", "VM -> VM"),
        ("vm_to_native", r"VM -> native"),
        ("native_to_vm", r"native -> VM"),
        ("native_to_native_direct", r"native -> native direct"),
        ("total_calls", "total Cove calls"),
    ):
        one(err, re.escape(label) + r"\s+([\d,]+)", name, lambda s: int(s.replace(",", "")))
    one(err, r"\(([0-9.]+)% of Cove calls used native code\)", "native_call_share", float)
PYTHON

printf 'covefmt over 248 files, three arms, interleaved: 1 cold run and %d warm\n' "$warm"
one_round cold 1
for n in $(seq "$warm"); do
  one_round warm "$n"
done

WARM=$warm python3 - "$work/samples" <<'PYTHON'
import collections
import os
import statistics
import sys

samples = collections.defaultdict(list)
for line in open(sys.argv[1]):
    phase, arm, name, value = line.split()
    samples[(phase, arm, name)].append(float(value))

arms = ("rust", "vm", "native")
rows = [
    ("total process wall", "wall", "ms", 0),
    ("lex", "lex", "ms", 0),
    ("parse", "parse", "ms", 0),
    ("print", "print", "ms", 0),
    ("whole pipeline", "whole", "ms", 0),
    ("backend execute=", "execute", "ms", 1),
    ("native compilation", "compile", "ms", 1),
    ("dispatched instructions", "dispatched", "", 0),
    ("allocations", "allocations", "", 0),
    ("allocated words", "allocated_words", "", 0),
    ("collections", "collections", "", 0),
    ("machine-code bytes", "code_bytes", "", 0),
    ("VM -> VM", "vm_to_vm", "", 0),
    ("VM -> native", "vm_to_native", "", 0),
    ("native -> VM", "native_to_vm", "", 0),
    ("native -> native direct", "native_to_native_direct", "", 0),
    ("total Cove calls", "total_calls", "", 0),
    ("native call share", "native_call_share", "%", 1),
    ("output bytes", "printed_bytes", "", 0),
    ("tokens", "tokens", "", 0),
]


def cell(phase, arm, name, unit, places):
    held = samples.get((phase, arm, name))
    if not held:
        return "-"
    if len(set(held)) == 1:
        return f"{held[0]:,.{places}f}{unit}"
    return (
        f"{statistics.median(held):,.{places}f}{unit} "
        f"[{min(held):,.{places}f}..{max(held):,.{places}f}]"
    )


for phase, caption in (
    ("warm", f"warm, {os.environ['WARM']} interleaved runs, median [min..max]"),
    ("cold", "cold, the first run of the session"),
):
    print(f"\n{caption}")
    print(f"  {'measurement':<26} {'rust':>30} {'vm':>30} {'native':>30}")
    for label, name, unit, places in rows:
        cells = [cell(phase, arm, name, unit, places) for arm in arms]
        if all(c == "-" for c in cells):
            continue
        print(f"  {label:<26} " + " ".join(f"{c:>30}" for c in cells))

# The two Cove arms ran in the same round, on the same machine, seconds apart,
# so each round is a *pair* and the difference within a pair is a far tighter
# measurement than the difference between two medians. A drift that moves both
# arms cancels here and does not cancel above, which is the whole reason the
# script interleaves; printing only the two columns would throw that away.
#
# The sign is the reading. A median difference smaller than the median
# *absolute* difference is a coin toss dressed as a result, and a table that
# says "1.005x" over samples like that has invented a number -- so this prints
# how many of the rounds went each way and lets a reader see it.
print("\nthe two Cove arms, paired within each warm round: native - vm")
print(f"  {'measurement':<26} {'median':>12} {'min':>12} {'max':>12} {'native slower in':>18}")
for label, name in (("whole pipeline", "whole"), ("backend execute=", "execute"),
                    ("total process wall", "wall"), ("lex", "lex"),
                    ("parse", "parse"), ("print", "print")):
    vm = samples.get(("warm", "vm", name), [])
    native = samples.get(("warm", "native", name), [])
    if len(vm) != len(native) or not vm:
        continue
    deltas = [n - v for v, n in zip(vm, native)]
    slower = sum(1 for d in deltas if d > 0)
    print(
        f"  {label:<26} {statistics.median(deltas):>+11.1f}ms {min(deltas):>+11.1f}ms "
        f"{max(deltas):>+11.1f}ms {f'{slower}/{len(deltas)} rounds':>18}"
    )
PYTHON
