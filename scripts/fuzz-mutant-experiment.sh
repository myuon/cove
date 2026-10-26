#!/usr/bin/env bash
# One mutation experiment (docs/FUZZ_EVALUATION.md, section 4): does the
# harness detect the mutant, and by which of three routes?
#
#   scripts/fuzz-mutant-experiment.sh <mutant> [options]
#
#   --out DIR          results (default fuzz/measure/mutation/<mutant>)
#   --replay DIR...    corpora to replay, each `<target>=<dir>` (repeatable);
#                      default: every target's committed seeds and regressions
#   --explore T=DIR    a target and the starting corpus a new session explores
#                      from (repeatable); inputs the replays found are removed
#                      from it first, so a known reproducer is never "found"
#   --seconds S        each exploration session's length (default 300)
#   --runs N           exploration sessions per target, seeds 1..N (default 3)
#   --fork F           -fork=F (default 4)
#   --keep             leave the mutant's worktree in place afterwards
#   --no-dedicated     skip the dedicated tests (a second exploration of a
#                      mutant whose tests were already run)
#   --dedicated N=CMD  one more dedicated test, named N, run as CMD in the
#                      mutant's worktree (repeatable): the suites that exist
#                      for what the mutant breaks, beyond the default four
#
# The three routes, reported apart:
#
#   1. dedicated tests: the test files that exist for what the mutant breaks
#      (`gc_roots`, `interior_roots`, `regressions`, `cove-native`'s
#      `template`, `cove-runtime`'s `native_tier`), run in the mutant's
#      worktree;
#   2. corpus replay: `cove-fuzz stats <target> <corpus>` with the mutant's
#      build, over corpora that existed before the experiment;
#   3. new exploration: scripts/fuzz-measure.sh --until-finding from a
#      starting corpus with the replays' findings taken out.
#
# The mutant is applied only in a scratch worktree (scripts/fuzz-mutant.sh);
# this tree is checked to hold none before and after.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
mutant="${1:?usage: scripts/fuzz-mutant-experiment.sh <mutant> [options]}"
shift
out="" seconds=300 runs=3 fork=4 keep=0 no_dedicated=0
replays=() explores=() extras=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --replay) replays+=("$2"); shift 2 ;;
    --explore) explores+=("$2"); shift 2 ;;
    --seconds) seconds="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    --fork) fork="$2"; shift 2 ;;
    --keep) keep=1; shift ;;
    --dedicated) extras+=("$2"); shift 2 ;;
    --no-dedicated) no_dedicated=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
if [ ${#replays[@]} -eq 0 ]; then
  for t in differential differential_source ir ir_mutate image; do
    replays+=("$t=$root/fuzz/seeds/$t" "$t=$root/fuzz/regressions/$t")
  done
fi
out="${out:-$root/fuzz/measure/mutation/$mutant}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
wt="$root/fuzz/measure/mutants/$mutant"
m="$root/scripts/fuzz-mutant.sh"
report="$out/report.txt"
: > "$report"
say() { echo "$*" | tee -a "$report"; }

"$m" check > /dev/null
"$m" revert "$mutant" > /dev/null 2>&1 || true
"$m" apply "$mutant"
"$m" verify "$mutant" | tee -a "$report"
say "revision: $(git -C "$wt" rev-parse HEAD) + $mutant"
say "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# 1. Dedicated tests.
dedicated() {
  local name="$1"; shift
  local start; start=$(date +%s)
  local status=0
  "$m" run "$mutant" -- "$@" > "$out/test-$name.log" 2>&1 || status=$?
  local failed
  failed=$(grep -E '^test .* FAILED$|^---- ' "$out/test-$name.log" | sed 's/^---- //; s/ stdout ----$//' | sort -u | tr '\n' ' ' || true)
  say "dedicated $name: $([ $status = 0 ] && echo passed || echo DETECTED) in $(( $(date +%s) - start ))s ${failed:+(failed: $failed)}"
}
if [ "$no_dedicated" = 0 ]; then
dedicated gc_roots cargo test --profile checked -p cove-fuzz --features template --test gc_roots --test interior_roots --test regressions
dedicated template cargo test --profile checked -p cove-native --features template --test template
dedicated native_tier cargo test --profile checked -p cove-runtime --features template --test native_tier
for spec in ${extras[@]+"${extras[@]}"}; do
  dedicated "${spec%%=*}" bash -c "${spec#*=}"
done
fi

# Replays `$2` through target `$1` with the mutant's build: the JSON to
# `$3.json`, the findings to `$3.txt`. A replay the process does not survive
# — an abort, a fault, which the catch-all of `cove-fuzz stats` cannot count —
# is made again one input at a time, and an input that kills its process is a
# finding too.
replay_dir() {
  local t="$1" dir="$2" to="$3" status=0 s f
  # `known/` holds inputs that fail on a clean build: they are not the
  # mutant's to find.
  if [ -d "$dir/known" ]; then
    rm -rf "$out/without-known"
    mkdir -p "$out/without-known"
    find "$dir" -type f -not -path '*/known/*' -not -name '*.md' -exec cp {} "$out/without-known/" \;
    dir="$out/without-known"
  fi
  "$wt/target/checked/cove-fuzz" stats "$t" "$dir" > "$to.json" 2> "$to.txt" || status=$?
  if [ "$status" -gt 1 ]; then
    : > "$to.txt"
    while IFS= read -r -d '' f; do
      s=0
      "$wt/target/checked/cove-fuzz" stats "$t" "$f" > /dev/null 2> "$out/one.txt" || s=$?
      if [ "$s" -gt 1 ]; then
        echo "finding: $f: the process died (status $s): $(grep -m1 -E 'panicked|Abort|fault|error' "$out/one.txt" | cut -c1-160)" >> "$to.txt"
      else
        grep '^finding: ' "$out/one.txt" >> "$to.txt" || true
      fi
    done < <(find "$dir" -type f -not -name '*.md' -not -name '.*' -print0 | sort -z)
  fi
}

# 2. Corpus replay, with the mutant's build — and this tree's own build, which
# an exploration's finding is replayed on to tell the mutant's findings from
# the toolchain's.
(cd "$root" && cargo build --profile checked -p cove-fuzz --features template --quiet)
"$m" run "$mutant" -- cargo build --profile checked -p cove-fuzz --features template --quiet
found="$out/found.txt"
: > "$found"
for spec in "${replays[@]}"; do
  t="${spec%%=*}" dir="${spec#*=}"
  [ -d "$dir" ] || continue
  tag="$(echo "$dir" | sed "s|$root/||; s|/|_|g")"
  start=$(date +%s)
  replay_dir "$t" "$dir" "$out/replay-$t-$tag"
  n=$(grep -c '^finding: ' "$out/replay-$t-$tag.txt" || true)
  inputs=$(find "$dir" -type f -not -name '*.md' -not -name '.*' -not -path '*/known/*' | wc -l | tr -d ' ')
  grep '^finding: ' "$out/replay-$t-$tag.txt" | sed 's/^finding: //; s/: .*//' >> "$found" || true
  first=$(grep -m1 '^finding: ' "$out/replay-$t-$tag.txt" | cut -c1-220 || true)
  say "replay $t ${dir#$root/}: $n of $inputs inputs found it, $(( $(date +%s) - start ))s ${first:+— $first}"
done

# 3. New exploration, from corpora with every replayed finding taken out.
for spec in ${explores[@]+"${explores[@]}"}; do
  t="${spec%%=*}" dir="${spec#*=}"
  start_corpus="$out/explore-start-$t"
  rm -rf "$start_corpus"
  mkdir -p "$start_corpus"
  cp -R "$dir/." "$start_corpus/"
  removed=0
  while IFS= read -r f; do
    base="$(basename "$f")"
    if [ -f "$start_corpus/$base" ] && cmp -s "$f" "$start_corpus/$base"; then
      rm "$start_corpus/$base"
      removed=$((removed + 1))
    fi
  done < "$found"
  # And anything in the starting corpus that finds it on its own.
  replay_dir "$t" "$start_corpus" "$out/explore-start-$t"
  while IFS= read -r f; do
    rm -f "$f"
    removed=$((removed + 1))
  done < <(grep '^finding: ' "$out/explore-start-$t.txt" | sed 's/^finding: //; s/: .*//')
  say "explore $t: starting corpus ${dir#$root/}, $(find "$start_corpus" -type f | wc -l | tr -d ' ') inputs after removing $removed that found it"
  "$m" run "$mutant" -- scripts/fuzz-measure.sh "$t" "$seconds" --runs "$runs" --fork "$fork" \
    --until-finding --corpus "$start_corpus" --out "$out/explore-$t" > "$out/explore-$t.log" 2>&1 || true
  for r in $(seq 1 "$runs"); do
    res="$out/explore-$t/run-$r/result.txt"
    if [ -f "$res" ]; then
      first=$(grep 'first finding' "$res" | sed 's/.*after: //' || true)
      verdict="not detected in ${seconds}s"
      if [ -n "$first" ]; then verdict="DETECTED after $first"; fi
      say "explore $t seed $r: $verdict ($(grep '^wall' "$res"), $(grep '^executions' "$res" | sed 's/ (.*)//'))"
      a=$(find "$out/explore-$t/run-$r/artifacts" -type f | head -1)
      if [ -n "$a" ]; then
        "$wt/target/checked/cove-fuzz" stats "$t" "$a" 2>&1 >/dev/null | grep '^finding' | cut -c1-300 >> "$report" || true
        if "$root/target/checked/cove-fuzz" stats "$t" "$a" > /dev/null 2>&1; then
          say "  the clean build agrees on it: the mutant's finding"
        else
          say "  the clean build ALSO fails on it: the toolchain's finding, not the mutant's"
        fi
      fi
    fi
  done
done

if [ "$keep" = 0 ]; then
  "$m" revert "$mutant" > /dev/null
fi
"$m" check > /dev/null && say "this tree holds no mutant"
