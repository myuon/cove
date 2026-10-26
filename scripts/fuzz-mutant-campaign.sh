#!/usr/bin/env bash
# Every mutant's experiment under one budget: the detection-power campaign
# of docs/FUZZ_EVALUATION.md, section 10 (issue 708).
#
#   scripts/fuzz-mutant-campaign.sh --out DIR [options] [MUTANT...]
#
#   --out DIR        where everything goes (one directory per mutant)
#   --corpus DIR     starting corpora, DIR/<target> (default: made by
#                    scripts/fuzz-seed-corpus.sh into DIR/start)
#   --targets "..."  the targets explored (default: differential
#                    differential_source ir)
#   --seconds S      each exploration session's length (default 45)
#   --runs N         sessions per (mutant, target), libFuzzer seeds 1..N
#                    (default 10)
#   --fork F         -fork=F, the core budget (default 4)
#
# With no MUTANT, every one `scripts/fuzz-mutant.sh list` names. Each runs
# scripts/fuzz-mutant-experiment.sh with the committed seeds and regressions
# replayed, the dedicated tests for what it breaks (the default four, and the
# ones `extra_tests` names below), and an exploration of every target.
# scripts/fuzz-mutant-table.py DIR tabulates the reports.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="" corpus="" targets="differential differential_source ir" seconds=45 runs=10 fork=4
mutants=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    --targets) targets="$2"; shift 2 ;;
    --seconds) seconds="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    --fork) fork="$2"; shift 2 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) mutants+=("$1"); shift ;;
  esac
done
[ -n "$out" ] || { echo "--out DIR is required" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
if [ ${#mutants[@]} -eq 0 ]; then
  while IFS= read -r m; do mutants+=("$m"); done < <("$root/scripts/fuzz-mutant.sh" list)
fi
if [ -z "$corpus" ]; then
  corpus="$out/start"
  for t in $targets; do
    [ -d "$corpus/$t" ] || COVE_FUZZ_CORPUS_DIR="$corpus" "$root/scripts/fuzz-seed-corpus.sh" "$t" > /dev/null
  done
fi
corpus="$(cd "$corpus" && pwd)"

# The suites that exist for what a mutant breaks, beyond the experiment's
# default four (gc_roots with interior_roots and regressions, template,
# native_tier), by the mutant's category.
extra_tests() {
  local p="cargo test --profile checked"
  case "$1" in
    *-vm-byte-at-*) echo "encoded=$p -p cove-runtime --features template --test encoded --test conformance" ;;
    *-verify-*) echo "verify=$p -p cove-ir --lib verify" ;;
    *-reset-*) echo "lifecycle=$p -p cove-runtime --features template --test embedding --test lifecycle" ;;
    *-grant-*) echo "authority=$p -p cove-runtime --features template --test authority --test embedding" ;;
    *-lower-*) echo "lower=$p -p cove-ir --lib lower"
               echo "differential=$p -p cove-cli --test differential" ;;
  esac
}

{
  echo "revision: $(git -C "$root" rev-parse HEAD)"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) logical cores"
  echo "targets: $targets; sessions: $runs x ${seconds}s, -fork=$fork"
  for t in $targets; do
    echo "starting corpus $t: $(find "$corpus/$t" -type f | wc -l | tr -d ' ') inputs"
  done
  echo "mutants: ${mutants[*]}"
} > "$out/meta.txt"

for m in "${mutants[@]}"; do
  args=(--out "$out/$m" --seconds "$seconds" --runs "$runs" --fork "$fork")
  for t in $targets; do args+=(--explore "$t=$corpus/$t"); done
  while IFS= read -r extra; do
    [ -n "$extra" ] && args+=(--dedicated "$extra")
  done < <(extra_tests "$m")
  start=$(date +%s)
  "$root/scripts/fuzz-mutant-experiment.sh" "$m" "${args[@]}" > "$out/$m.log" 2>&1 || true
  echo "$m: $(( $(date +%s) - start ))s" | tee -a "$out/meta.txt"
done
echo "wrote $out"
