#!/bin/bash
#
# The measurement `examples/cq/README.md` publishes.
#
# Run it from anywhere in the repository, after
# `cargo build --release -p cove-cli` and after generating the input:
#
#     cd examples
#     ../target/release/cove run cqSample --files-root cq/data -- 100000 bookings-large.jsonl
#
# It writes one section per workload: three runs each of the two
# 100,000-record transformations, then the same run traced and untraced. Wall
# time, fuel, and the managed heap come from `cove run --stats`; resident
# memory is `/usr/bin/time -l`'s, and is the whole process rather than the
# collector's heap. The README says why both are reported.
#
# macOS only: `/usr/bin/time -l` is what reports resident memory here, and the
# published numbers were taken on macOS. The README records the machine.
#
# Wall-clock numbers are not gated anywhere: ADR 0012 says why, and this is a
# local exercise for the same reason `cove-bench` is.
#
# It fails rather than reporting a partial measurement. `set -e` stops it if a
# run exits non-zero, and `pipefail` stops it if the `grep` that reads a run's
# statistics finds none — which is what a run that died early looks like.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)/examples"

COVE=../target/release/cove
ROOT=cq/data
OUT=${1:-/tmp/perf-cq.txt}

if [ ! -x "$COVE" ]; then
  echo "perf-cq: $COVE is not there; run \`cargo build --release -p cove-cli\`" >&2
  exit 1
fi
if [ ! -f "$ROOT/bookings-large.jsonl" ]; then
  echo "perf-cq: $ROOT/bookings-large.jsonl is not there; the header comment says how to make it" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT")"
: > "$OUT"

# One scratch directory for this run's stderr and its trace, removed however
# the script ends, so a failed run leaves nothing behind to be mistaken for a
# measurement.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
err="$scratch/stderr"

say() { echo "$@" >> "$OUT"; }

say "commit:  $(git rev-parse HEAD)"
say "rustc:   $(rustc --version)"
say "cpu:     $(sysctl -n machdep.cpu.brand_string)"
say "os:      macOS $(sw_vers -productVersion) ($(uname -m))"
say ""

# Runs `cq` once and appends what it reported. `grep` failing here means the
# run produced no statistics, which `pipefail` turns into the script's own
# failure rather than a silently short section of the report.
run() {
  local label=$1
  shift
  /usr/bin/time -l "$COVE" run cq --files-root "$ROOT" --stats "$@" >/dev/null 2>"$err"
  say "-- $label"
  grep -E "^stats:|^heap:" "$err" >> "$OUT"
  awk '/maximum resident set size/ {printf "rss:   max_resident_bytes=%s\n", $1}' "$err" >> "$OUT"
}

say "== revenue-summary, 100,000 records, three runs =="
for i in 1 2 3; do
  run "run $i" -- bookings-large.jsonl --program revenue-summary --output summary-large.csv
done

say ""
say "== confirmed-bookings, 100,000 records, three runs =="
for i in 1 2 3; do
  run "run $i" -- bookings-large.jsonl --program confirmed-bookings --output confirmed-large.jsonl
done

say ""
say "== trace overhead, revenue-summary, 20,000 records =="
run "untraced" --limit 20000 -- bookings-large.jsonl --program revenue-summary --output summary-large.csv
run "traced" --trace "$scratch/trace.jsonl" --limit 20000 -- \
  bookings-large.jsonl --program revenue-summary --output summary-large.csv

echo "perf-cq: wrote $OUT" >&2
