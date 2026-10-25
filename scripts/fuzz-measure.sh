#!/usr/bin/env bash
# Measures a fuzz target: N sessions of a fixed length and core budget, each
# from a fresh copy of the same starting corpus and with its own libFuzzer
# seed, counted by `cove-fuzz stats` (docs/FUZZING.md, "Measuring a target").
#
#   scripts/fuzz-measure.sh <target> <seconds> [options] [-- libFuzzer flags...]
#
#   --runs N         sessions, seeds 1..N (default 3)
#   --fork F         libFuzzer -fork=F; the core budget (default 4)
#   --out DIR        where everything goes (default fuzz/measure/<target>-<date>)
#   --corpus DIR     the starting corpus, copied afresh for every session
#                    (default: what scripts/fuzz-seed-corpus.sh makes, built
#                    into DIR/start/<target>)
#   --asan           build with AddressSanitizer (default: none — a count and a
#                    disagreement are not memory-safety questions, and a build
#                    without it is several times faster)
#   --until-finding  stop a session at its first finding and record when, for
#                    a time-to-detection measurement (default: -ignore_crashes=1
#                    and run the whole length)
#   --replay         also replay each session's grown corpus with `cove-fuzz
#                    stats`, once an input
#
# What it writes, under DIR:
#
#   meta.txt                   revision, tree state, toolchains, host, load,
#                              the exact command line of every session
#   run-<seed>/session.json    the session's in-process counts, merged
#   run-<seed>/session.txt     their summary
#   run-<seed>/corpus.json     (--replay) the grown corpus, counted once each
#   run-<seed>/result.txt      exit status, wall seconds, executions, findings
#   run-<seed>/fuzz.log        libFuzzer's output
#   run-<seed>/artifacts/      any finding
#
# The session's corpus is a copy, so a measurement never grows
# fuzz/corpus/<target>/ and two measurements from one starting corpus start
# from the same inputs.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${1:?usage: scripts/fuzz-measure.sh <target> <seconds> [options] [-- libFuzzer flags]}"
seconds="${2:?usage: scripts/fuzz-measure.sh <target> <seconds> [options]}"
shift 2
runs=3 fork=4 out="" corpus="" sanitizer=none until=0 replay=0
while [ $# -gt 0 ]; do
  case "$1" in
    --runs) runs="$2"; shift 2 ;;
    --fork) fork="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    --asan) sanitizer=address; shift ;;
    --until-finding) until=1; shift ;;
    --replay) replay=1; shift ;;
    --) shift; break ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
extra_flags=("$@")

case "$target" in
  parse | compile | run | differential_source) max_len=65536 extra=(-dict=dictionaries/cove.dict) ;;
  structured | differential | ir) max_len=4096 extra=(-len_control=0) ;;
  image | ir_mutate) max_len=262144 extra=() ;;
  *) echo "no target \`$target\`" >&2; exit 2 ;;
esac

out="${out:-$root/fuzz/measure/$target-$(date +%Y%m%dT%H%M%S)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

if [ -z "$corpus" ]; then
  corpus="$out/start/$target"
  if [ ! -d "$corpus" ]; then
    COVE_FUZZ_CORPUS_DIR="$out/start" "$root/scripts/fuzz-seed-corpus.sh" "$target" > /dev/null
  fi
fi
corpus="$(cd "$corpus" && pwd)"

cd "$root"
cargo build --profile checked -p cove-fuzz --features template --quiet
cd "$root/fuzz"
cargo +nightly fuzz build -s "$sanitizer" "$target" 2>&1 | tail -1

{
  echo "target: $target"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "revision: $(git -C "$root" rev-parse HEAD)"
  echo "tree: $(git -C "$root" status --porcelain --untracked-files=no | wc -l | tr -d ' ') modified tracked files"
  git -C "$root" diff --stat | tail -1 | sed 's/^/diff: /'
  echo "rustc: $(rustc +nightly --version)"
  echo "cargo-fuzz: $(cargo fuzz --version)"
  echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) logical cores"
  echo "sanitizer: $sanitizer"
  echo "seconds: $seconds; sessions: $runs; fork: $fork; until-finding: $until"
  echo "starting corpus: $corpus ($(find "$corpus" -type f | wc -l | tr -d ' ') inputs)"
} > "$out/meta.txt"

for seed in $(seq 1 "$runs"); do
  run="$out/run-$seed"
  rm -rf "$run"
  mkdir -p "$run/corpus" "$run/artifacts" "$run/session"
  cp -R "$corpus/." "$run/corpus/"
  flags=(
    -seed="$seed"
    -max_total_time="$seconds"
    -fork="$fork"
    -timeout=10
    -rss_limit_mb=4096
    -malloc_limit_mb=1024
    -max_len="$max_len"
    -print_final_stats=1
    -artifact_prefix="$run/artifacts/"
    ${extra[@]+"${extra[@]}"}
  )
  if [ "$until" = 0 ]; then
    flags+=(-ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1)
  fi
  flags+=(${extra_flags[@]+"${extra_flags[@]}"})
  echo "run-$seed: load $(uptime | sed 's/.*load averages*: //'); cargo +nightly fuzz run -s $sanitizer $target <corpus> -- ${flags[*]}" >> "$out/meta.txt"
  start=$(date +%s)
  status=0
  COVE_FUZZ_STATS_DIR="$run/session" \
    cargo +nightly fuzz run -s "$sanitizer" "$target" "$run/corpus" -- "${flags[@]}" \
    > "$run/fuzz.log" 2>&1 || status=$?
  end=$(date +%s)
  findings=$(find "$run/artifacts" -type f | wc -l | tr -d ' ')
  first=""
  if [ "$findings" -gt 0 ]; then
    # BSD stat, then GNU stat: the modification time of each finding.
    first=$( (find "$run/artifacts" -type f -exec stat -f '%m' {} \; 2>/dev/null \
      || find "$run/artifacts" -type f -exec stat -c '%Y' {} \;) \
      | sort -n | head -1 | awk -v s="$start" '{print $1 - s}')
  fi
  execs=$(grep -Eo 'stat::number_of_executed_units: *[0-9]+' "$run/fuzz.log" | awk '{s += $2} END {print s + 0}')
  {
    echo "status: $status"
    echo "wall seconds: $((end - start))"
    echo "executions (libFuzzer's final stats, summed over processes): $execs"
    echo "findings: $findings"
    if [ -n "$first" ]; then echo "first finding written after: ${first}s"; fi
  } > "$run/result.txt"
  "$root/target/checked/cove-fuzz" stats merge "$run/session" \
    > "$run/session.json" 2> "$run/session.txt" || true
  if [ "$replay" = 1 ]; then
    "$root/target/checked/cove-fuzz" stats "$target" "$run/corpus" \
      > "$run/corpus.json" 2> "$run/corpus.txt" || true
  fi
  echo "run-$seed: $(tr '\n' ';' < "$run/result.txt")"
  head -1 "$run/session.txt" || true
done
echo "wrote $out"
