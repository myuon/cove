#!/usr/bin/env bash
# Runs one fuzz target under AddressSanitizer with the limits docs/FUZZING.md
# describes, and leaves a crash input in fuzz/artifacts/<target>/.
#
#   scripts/fuzz.sh <parse|compile|run|structured|image> [seconds] [-- libFuzzer flags...]
#
#   scripts/fuzz.sh run 300           # five minutes of `run`
#   scripts/fuzz.sh structured 60 -- -jobs=4 -workers=4
#
# Needs nightly and cargo-fuzz (`cargo install cargo-fuzz`). Builds the
# corpus first if fuzz/corpus/<target>/ does not exist yet.
#
# The limits, and why each is what it is:
#
# - `-max_total_time`: the session's length, the second argument (default 60).
# - `-timeout=10`: seconds one input may take before libFuzzer calls it a hang.
#   The harness's own deadline is 150 ms a run and there are at most two runs
#   (VM, native) after a front end of a few tens of milliseconds, so ten
#   seconds is reached only by a loop the runtime's deadline cannot stop —
#   which is a finding, not a slow input. (Under ASan everything is ~2-3x
#   slower; the margin covers it.)
# - `-rss_limit_mb=4096`: the process's memory. A run's heap is capped at two
#   mebibytes by the harness, so this is reached only by memory the runtime
#   allocates outside its heap — a host-side `Vec` sized by a guest number —
#   which is a finding too. ASan's shadow and quarantine are most of it.
# - `-malloc_limit_mb=1024`: one allocation larger than this is reported at
#   the allocation, with its stack, rather than as an RSS overrun later.
# - `-max_len`: 64 KiB of source (the largest program in the tree is under
#   that), 256 KiB of image body, 4 KiB of generator choices.
# - `-dict`: Cove's keywords and operators, for the source targets.
# - `-detect_leaks=0`: LeakSanitizer (Linux only) would report memory a
#   process-lifetime cache keeps; leaks across runs are issue 681's
#   priority 3, not a memory-safety finding.
#
# The process has no network and no file system to reach through the
# harness: a run is granted `console` alone, writing into a sink.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${1:?usage: scripts/fuzz.sh <target> [seconds] [-- libFuzzer flags]}"
seconds="${2:-60}"
shift $(( $# >= 2 ? 2 : 1 ))
if [ "${1:-}" = "--" ]; then
  shift
fi

case "$target" in
  parse | compile | run) max_len=65536 extra=(-dict=dictionaries/cove.dict) ;;
  # A generator reads its choices from the front, so a short input is a
  # short program; `-len_control=0` lets libFuzzer use the whole length at
  # once rather than growing towards it over the first hours.
  structured) max_len=4096 extra=(-len_control=0) ;;
  image) max_len=262144 extra=() ;;
  *)
    echo "no target \`$target\`: parse, compile, run, structured, image" >&2
    exit 2
    ;;
esac

if [ ! -d "$root/fuzz/corpus/$target" ]; then
  "$root/scripts/fuzz-seed-corpus.sh" "$target"
fi

# cargo-fuzz itself points `-artifact_prefix` at fuzz/artifacts/<target>/.
cd "$root/fuzz"
mkdir -p "artifacts/$target" logs
cargo +nightly fuzz run "$target" "corpus/$target" -- \
  -max_total_time="$seconds" \
  -timeout=10 \
  -rss_limit_mb=4096 \
  -malloc_limit_mb=1024 \
  -max_len="$max_len" \
  -detect_leaks=0 \
  -print_final_stats=1 \
  ${extra[@]+"${extra[@]}"} \
  "$@" 2>&1 | tee "logs/$target.log"
