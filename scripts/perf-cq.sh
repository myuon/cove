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
# Wall-clock numbers are not gated anywhere: ADR 0012 says why, and this is a
# local exercise for the same reason `cove-bench` is.
set -u
cd "$(git rev-parse --show-toplevel)/examples" || exit 1
COVE=../target/release/cove
ROOT=cq/data
OUT=${1:-/tmp/perf/out.txt}
ERR=/tmp/perf/.err
: > "$OUT"
say() { echo "$@" >> "$OUT"; }

say "commit:  $(git rev-parse HEAD)"
say "rustc:   $(rustc --version)"
say "cpu:     $(sysctl -n machdep.cpu.brand_string)"
say "os:      macOS $(sw_vers -productVersion) ($(uname -m))"
say ""

run() { # label, extra args...
  local label=$1; shift
  /usr/bin/time -l $COVE run cq --files-root $ROOT --stats "$@" >/dev/null 2>"$ERR"
  say "-- $label"
  grep -E "^stats:|^heap:" "$ERR" >> "$OUT"
  awk '/maximum resident set size/ {printf "rss:   max_resident_bytes=%s\n", $1}' "$ERR" >> "$OUT"
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
/usr/bin/time -l $COVE run cq --files-root $ROOT --stats --trace /tmp/perf/trace.jsonl --limit 20000 -- \
    bookings-large.jsonl --program revenue-summary --output summary-large.csv >/dev/null 2>"$ERR"
say "-- traced"
grep -E "^stats:|^heap:" "$ERR" >> "$OUT"
awk '/maximum resident set size/ {printf "rss:   max_resident_bytes=%s\n", $1}' "$ERR" >> "$OUT"
