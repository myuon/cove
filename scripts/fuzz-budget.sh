#!/usr/bin/env bash
# Runs a fuzzing plan — several workers, each a sequence of targets — under
# one wall-clock budget, and records what every part of it cost
# (docs/FUZZING.md, "A budget of workers"; docs/FUZZ_EVALUATION.md, section 12).
#
#   scripts/fuzz-budget.sh <plan> [options]
#
#   <plan>          a file, or a name in fuzz/plans/ (`pr`, `nightly`, ...)
#   --wall S        the whole run's wall clock, start corpus to summary
#                   (default: the plan's `wall` line)
#   --out DIR       where everything goes (default fuzz/measure/budget-<plan>-<date>)
#   --seed N        libFuzzer seeds N*100 + slot (default 1)
#   --corpus DIR    the starting corpora, DIR/<target> (default: what
#                   scripts/fuzz-pr-corpus.sh materialises, which is the PR
#                   corpus; `--corpus committed` takes the committed seeds and
#                   regressions alone)
#   --keep-corpus DIR  grow DIR/<target> in place instead of a copy (the
#                   nightly run's accumulated corpus)
#   --no-build      the fuzz binaries are built already
#   --clean DIR     a clean tree whose `cove-fuzz` replays each finding as
#                   well (a mutant experiment: a finding the clean build shares
#                   is not the mutant's)
#
# A plan is lines of `<worker> <target> <weight> <sanitizer>`, plus `wall S`
# and `reserve S`. Each worker runs its lines one after another; a line gets
# the share of the worker's remaining time its weight is of the worker's
# remaining weights, so a slot that ends early hands its time on. Every
# libFuzzer process is `-fork=1` with crashes, timeouts and out-of-memory
# stops ignored, so one finding does not end the exploration; and each is
# also killed from outside at its share's end, because libFuzzer runs its
# starting corpus before it looks at `-max_total_time` and a large corpus
# runs past any budget.
#
# The run stops exploring `reserve` seconds before `wall`, then sorts every
# finding (scripts/fuzz-budget-summary.py): new, or like a known reproducer
# under fuzz/regressions/*/known/, grouped by a signature of its message — a
# likeness, not a proof of one cause — and crash, timeout, out-of-memory,
# leak or the run itself failing. Exit status: 0 if nothing new and nothing
# failed to run, 1 if a new finding, 2 if a worker could not run. A known-like
# finding is reported and does not fail the run.
#
# What it writes, under DIR: meta.txt; build.txt (build seconds, kept apart
# from the budget); slots/<w>-<k>-<target>/ (fuzz.log, result.txt,
# artifacts/, session/ counts); corpus/<target>/ (what the run ends with);
# summary.json and summary.md, written on success, failure and timeout alike.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
plan="${1:?usage: scripts/fuzz-budget.sh <plan> [options]}"
shift
[ -f "$plan" ] || plan="$root/fuzz/plans/$plan.plan"
[ -f "$plan" ] || { echo "no plan $plan" >&2; exit 2; }
plan="$(cd "$(dirname "$plan")" && pwd)/$(basename "$plan")"
wall="" out="" seed=1 corpus="" keep="" build=1 clean=""
while [ $# -gt 0 ]; do
  case "$1" in
    --wall) wall="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --seed) seed="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    --keep-corpus) keep="$2"; shift 2 ;;
    --no-build) build=0; shift ;;
    --clean) clean="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

name="$(basename "$plan" .plan)"
plan_wall=$(awk '$1 == "wall" {print $2}' "$plan")
reserve=$(awk '$1 == "reserve" {print $2}' "$plan")
wall="${wall:-${plan_wall:-120}}"
reserve="${reserve:-15}"
out="${out:-$root/fuzz/measure/budget-$name-$(date +%Y%m%dT%H%M%S)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
# Lines `worker target weight sanitizer`, comments and settings left out.
slots="$(awk '$1 ~ /^[0-9]+$/ {print $1, $2, $3, $4}' "$plan")"
targets="$(echo "$slots" | awk '{print $2}' | sort -u | tr '\n' ' ')"
now() { perl -MTime::HiRes=time -e 'printf "%.2f\n", time'; }

triple="$(rustc +nightly -vV | sed -n 's/^host: //p')"
bin_of() { echo "$root/fuzz/target/san-$2/$triple/release/$1"; }

# --- build, timed apart from the budget ------------------------------------
{
  echo "plan: $name ($plan)"
  sed 's/^/  /' "$plan"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "revision: $(git -C "$root" rev-parse HEAD)"
  echo "tree: $(git -C "$root" status --porcelain --untracked-files=no | wc -l | tr -d ' ') modified tracked files"
  echo "rustc: $(rustc +nightly --version)"
  echo "cargo-fuzz: $(cargo fuzz --version 2>/dev/null || echo none)"
  echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) logical cores"
  echo "wall: ${wall}s; reserve: ${reserve}s; seed base: $seed"
  echo "load before: $(uptime | sed 's/.*load averages*: //')"
} > "$out/meta.txt"

: > "$out/build.txt"
if [ "$build" = 1 ]; then
  t0=$(now)
  (cd "$root" && cargo build --profile checked -p cove-fuzz --features template --quiet)
  echo "cove-fuzz (checked, stable): $(echo "$(now) - $t0" | bc)s" >> "$out/build.txt"
  for san in $(echo "$slots" | awk '{print $4}' | sort -u); do
    want="$(echo "$slots" | awk -v s="$san" '$4 == s {print $2}' | sort -u | tr '\n' ' ')"
    t0=$(now)
    # shellcheck disable=SC2086
    (cd "$root/fuzz" && cargo +nightly fuzz build -s "$san" --target-dir "target/san-$san" $want 2>&1 | tail -1)
    echo "fuzz targets, sanitizer $san ($want): $(echo "$(now) - $t0" | bc)s" >> "$out/build.txt"
  done
fi
for line in $(echo "$slots" | awk '{print $2 ":" $4}' | sort -u); do
  b="$(bin_of "${line%%:*}" "${line##*:}")"
  [ -x "$b" ] || { echo "no binary $b (build it, or drop --no-build)" >&2; exit 2; }
done

# --- starting corpora -------------------------------------------------------
t_start=$(now)
mkdir -p "$out/corpus"
for t in $targets; do
  dest="$out/corpus/$t"
  if [ -n "$keep" ]; then
    mkdir -p "$keep/$t"
    dest="$(cd "$keep/$t" && pwd)"
    ln -sfn "$dest" "$out/corpus/$t"
  else
    mkdir -p "$dest"
  fi
  if [ "$corpus" = committed ]; then
    for d in "$root/fuzz/seeds/$t" "$root/fuzz/regressions/$t"; do
      [ -d "$d" ] && find "$d" -type f -not -name '*.md' -not -path '*/known/*' -exec cp {} "$dest/" \;
    done
  elif [ -n "$corpus" ]; then
    [ -d "$corpus/$t" ] && cp -R "$corpus/$t/." "$dest/"
  else
    "$root/scripts/fuzz-pr-corpus.sh" materialise "$t" "$dest" > /dev/null
  fi
  echo "starting corpus $t: $(find "$dest" -type f | wc -l | tr -d ' ') inputs" >> "$out/meta.txt"
done
t_corpus=$(now)

# --- explore ----------------------------------------------------------------
deadline=$(echo "$t_start + $wall - $reserve" | bc)
echo "explore from $t_corpus to $deadline" >> "$out/meta.txt"

# One slot: libFuzzer in a process group of its own, killed at `$4` if it is
# still running then.
slot() {
  local w="$1" k="$2" t="$3" until="$4" san="$5"
  local dir="$out/slots/$w-$k-$t"
  mkdir -p "$dir/artifacts" "$dir/session"
  local max_len extra=()
  case "$t" in
    parse | compile | run | differential_source) max_len=65536 extra=(-dict="$root/fuzz/dictionaries/cove.dict") ;;
    structured | differential | ir) max_len=4096 extra=(-len_control=0) ;;
    image | ir_mutate) max_len=262144 ;;
  esac
  local begin; begin=$(now)
  local secs; secs=$(echo "$until - $begin" | bc | cut -d. -f1)
  [ "${secs:-0}" -ge 1 ] || secs=1
  local flags=(
    -fork=1 -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1
    -seed=$((seed * 100 + w * 10 + k))
    -max_total_time="$secs"
    -timeout=10 -rss_limit_mb=4096 -malloc_limit_mb=1024
    -max_len="$max_len" -print_final_stats=1
    -artifact_prefix="$dir/artifacts/"
    ${extra[@]+"${extra[@]}"}
  )
  echo "$(bin_of "$t" "$san") $out/corpus/$t ${flags[*]}" > "$dir/command.txt"
  local status=0 killed=0
  COVE_FUZZ_STATS_DIR="$dir/session" perl -e 'setpgrp(0, 0); exec @ARGV or die "exec: $!"' -- \
    "$(bin_of "$t" "$san")" "$out/corpus/$t" "${flags[@]}" > "$dir/fuzz.log" 2>&1 &
  local pid=$!
  # libFuzzer says when it has run its starting corpus and not how long that
  # took; this watches for the line, to half a second.
  local replayed=""
  while kill -0 "$pid" 2> /dev/null; do
    if [ -z "$replayed" ] && grep -q 'starting to fuzz in\|INITED' "$dir/fuzz.log" 2> /dev/null; then
      replayed=$(now)
    fi
    if [ "$(echo "$(now) >= $until" | bc)" = 1 ]; then
      killed=1
      kill -INT -- "-$pid" 2> /dev/null || true
      sleep 1
      kill -KILL -- "-$pid" 2> /dev/null || true
      break
    fi
    sleep 0.5
  done
  wait "$pid" 2> /dev/null || status=$?
  local end; end=$(now)
  {
    echo "worker: $w"
    echo "slot: $k"
    echo "target: $t"
    echo "sanitizer: $san"
    echo "began: $begin"
    echo "ended: $end"
    echo "given seconds: $secs"
    echo "status: $status"
    echo "killed at the deadline: $killed"
    if [ -n "$replayed" ]; then echo "replayed: $replayed"; fi
  } > "$dir/result.txt"
}

worker() {
  local w="$1" lines k=0 left weight t san until
  lines="$(echo "$slots" | awk -v w="$w" '$1 == w')"
  left=$(echo "$lines" | awk '{s += $3} END {print s}')
  while read -r _ t weight san; do
    k=$((k + 1))
    until=$(echo "$(now) + ($deadline - $(now)) * $weight / $left" | bc -l)
    slot "$w" "$k" "$t" "$until" "$san"
    left=$((left - weight))
  done <<< "$lines"
}

pids=()
for w in $(echo "$slots" | awk '{print $1}' | sort -un); do
  worker "$w" &
  pids+=($!)
done
for p in "${pids[@]}"; do wait "$p" || true; done
t_explored=$(now)

# --- sort the findings, count, summarise -----------------------------------
known=()
for d in "$root"/fuzz/regressions/*/known; do [ -d "$d" ] && known+=("$d"); done
status=0
python3 "$root/scripts/fuzz-budget-summary.py" "$out" \
  --cove-fuzz "$root/target/checked/cove-fuzz" \
  ${clean:+--clean-cove-fuzz "$clean/target/checked/cove-fuzz"} \
  --times "$t_start" "$t_corpus" "$t_explored" \
  --wall "$wall" \
  ${known[@]+--known "${known[@]}"} || status=$?
echo "load after: $(uptime | sed 's/.*load averages*: //')" >> "$out/meta.txt"
echo "wrote $out"
exit "$status"
