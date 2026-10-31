#!/usr/bin/env bash
# Does a budget plan detect an artificial defect, and by which route?
# (docs/FUZZ_EVALUATION.md, section 12; the mutants are fuzz/mutants/).
#
#   scripts/fuzz-budget-mutants.sh --out DIR [options] MUTANT...
#
#   --plans "P..."   plans to try (default "pr-a pr-b")
#   --seeds "N..."   scripts/fuzz-budget.sh seeds, one trial each (default "1 2 3")
#   --wall S         each trial's wall clock (default: the plan's)
#   --corpus DIR     starting corpora DIR/<target> (default: the PR corpus,
#                    materialised by this tree's scripts/fuzz-pr-corpus.sh)
#
# One scratch worktree, fuzz/measure/mutants/budget, holds one mutant at a
# time: the next is swapped in by reversing the last patch and applying the
# next, and `scripts/fuzz-mutant.sh verify` checks that it holds exactly
# that one and this tree none. Its target directories persist between
# mutants, so each rebuild is incremental. Per mutant:
#
#   1. corpus replay: every starting input through the mutant's `cove-fuzz`;
#      an input that fails there and not on this tree's build detected it;
#   2. new exploration: each plan x seed from the starting corpus with those
#      inputs taken out, so a known reproducer is never found again; a
#      finding counts only if it reproduces on the mutant's build and not on
#      the clean one.
#
# DIR/results.tsv has a row a trial; DIR/<mutant>/ everything else. The
# worktree is removed at the end.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="" plans="pr-a pr-b" seeds="1 2 3" wall="" corpus=""
mutants=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --plans) plans="$2"; shift 2 ;;
    --seeds) seeds="$2"; shift 2 ;;
    --wall) wall="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) mutants+=("$1"); shift ;;
  esac
done
[ -n "$out" ] && [ ${#mutants[@]} -gt 0 ] || { sed -n '2,12p' "$0" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
m="$root/scripts/fuzz-mutant.sh"
wt="$root/fuzz/measure/mutants/budget"
now() { perl -MTime::HiRes=time -e 'printf "%.2f\n", time'; }
targets=""
for p in $plans; do
  targets="$targets $(awk '$1 ~ /^[0-9]+$/ {print $2}' "$root/fuzz/plans/$p.plan")"
done
targets="$(echo "$targets" | tr ' ' '\n' | grep . | sort -u | tr '\n' ' ')"

"$m" check > /dev/null
(cd "$root" && cargo build --profile checked -p cove-fuzz --features template --quiet)
clean="$root/target/checked/cove-fuzz"
if [ -z "$corpus" ]; then
  corpus="$out/start"
  for t in $targets; do
    [ -d "$corpus/$t" ] || "$root/scripts/fuzz-pr-corpus.sh" materialise "$t" "$corpus/$t" > /dev/null
  done
fi
corpus="$(cd "$corpus" && pwd)"
{
  echo "revision: $(git -C "$root" rev-parse HEAD)"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) logical cores"
  echo "plans: $plans; seeds: $seeds; wall: ${wall:-as the plan says}"
  for t in $targets; do echo "starting corpus $t: $(find "$corpus/$t" -type f | wc -l | tr -d ' ') inputs"; done
} > "$out/meta.txt"
[ -f "$out/results.tsv" ] || printf 'mutant\tplan\tseed\tverdict\tfirst_after\twall\tdetail\n' > "$out/results.tsv"

# Whether `$2` fails when `$3` replays it through target `$1`.
fails() {
  case "$1" in
    differential | differential_source | ir | ir_mutate | image)
      ! "$3" stats "$1" "$2" > /dev/null 2>&1 ;;
    *) ! "$3" "$1" "$2" > /dev/null 2>&1 ;;
  esac
}

# The targets the plans run with sanitizer `$1`.
wanted() {
  local p
  for p in $plans; do
    awk -v s="$1" '$1 ~ /^[0-9]+$/ && $4 == s {print $2}' "$root/fuzz/plans/$p.plan"
  done | sort -u | tr '\n' ' '
}

previous=""
for mutant in "${mutants[@]}"; do
  mo="$out/$mutant"
  mkdir -p "$mo"
  if [ -z "$previous" ]; then
    "$m" revert budget "$wt" > /dev/null 2>&1 || true
    if [ -d "$wt" ]; then git -C "$root" worktree remove --force "$wt"; fi
    "$m" apply "$mutant" "$wt" > /dev/null
  else
    git -C "$wt" apply -R "$root/fuzz/mutants/$previous.patch"
    git -C "$wt" apply "$root/fuzz/mutants/$mutant.patch"
  fi
  "$m" verify "$mutant" "$wt" > "$mo/verify.txt"
  previous="$mutant"

  t0=$(now)
  status=0
  (
    cd "$wt"
    cargo build --profile checked -p cove-fuzz --features template --quiet
    cd fuzz
    for san in none address; do
      want="$(wanted "$san")"
      # shellcheck disable=SC2086
      if [ -n "$want" ]; then
        cargo +nightly fuzz build -s "$san" --target-dir "target/san-$san" $want 2>&1 | tail -1
      fi
    done
  ) > "$mo/build.log" 2>&1 || status=$?
  echo "build: $(echo "$(now) - $t0" | bc)s, status $status" > "$mo/build.txt"
  if [ "$status" != 0 ]; then
    for p in $plans; do for s in $seeds; do
      printf '%s\t%s\t%s\tbuild failed\t\t\t%s\n' "$mutant" "$p" "$s" "$(tail -1 "$mo/build.log")" >> "$out/results.tsv"
    done; done
    continue
  fi
  mutated="$wt/target/checked/cove-fuzz"

  # 1. Corpus replay, and the exploration's start without what it found.
  rm -rf "$mo/explore-start"
  : > "$mo/replay.txt"
  for t in $targets; do
    mkdir -p "$mo/explore-start/$t"
    cp -R "$corpus/$t/." "$mo/explore-start/$t/"
    found=0 toolchain=0 n=0
    for f in "$mo/explore-start/$t"/*; do
      [ -f "$f" ] || continue
      n=$((n + 1))
      if fails "$t" "$f" "$mutated"; then
        if fails "$t" "$f" "$clean"; then
          toolchain=$((toolchain + 1))
        else
          found=$((found + 1))
          echo "$t $(basename "$f")" >> "$mo/replay-found.txt"
        fi
        rm "$f"
      fi
    done
    echo "replay $t: $found of $n starting inputs detected it; $toolchain fail on the clean build too" >> "$mo/replay.txt"
  done

  # 2. New exploration.
  for p in $plans; do
    for s in $seeds; do
      run="$mo/$p-seed$s"
      rm -rf "$run"
      st=0
      "$m" run "$mutant" "$wt" -- scripts/fuzz-budget.sh "$p" --no-build --seed "$s" \
        --corpus "$mo/explore-start" --clean "$root" --out "$run" ${wall:+--wall "$wall"} \
        > "$run.log" 2>&1 || st=$?
      verdict="not detected" first="" detail=""
      if [ ! -f "$run/summary.json" ]; then
        verdict="run failed"; detail="$(tail -1 "$run.log")"
      else
        read -r verdict first detail < <(python3 "$root/scripts/fuzz-budget-summary.py" --verdict "$run/summary.json")
      fi
      w="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['phases']['total_seconds'])" "$run/summary.json" 2>/dev/null || echo "")"
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$mutant" "$p" "$s" "$verdict" "$first" "$w" "$detail" >> "$out/results.tsv"
      echo "$mutant $p seed $s: $verdict $first"
    done
  done
done
if [ -n "$previous" ]; then
  git -C "$root" worktree remove --force "$wt"
  git -C "$root" worktree prune
fi
"$m" check > /dev/null && echo "this tree holds no mutant"
echo "wrote $out"
