#!/usr/bin/env bash
# What the fuzzing corpus reaches: Rust branch coverage by component, IR
# instruction kinds executed, and the type x boundary matrix
# (docs/FUZZ_EVALUATION.md, section 11).
#
#   scripts/fuzz-coverage-report.sh [options] [phase...]
#
#   phases (default: all of them, in this order)
#     corpus   one fresh session per target from its committed seeds and
#              regressions (scripts/fuzz-measure.sh --runs 1, libFuzzer seed 1)
#     source   an instrumented `cove-fuzz` (nightly, -Z coverage-options=branch)
#              replays each target's corpus through its harness; per-target and
#              union profiles, exported as llvm-cov JSON and summarised per
#              component by scripts/fuzz-coverage-components.py
#     ir       `cove-fuzz coverage` over each running target's corpus: IR
#              instruction kinds and the boundary matrix
#     report   OUT/report.md and OUT/report.json from the above
#
#   --out DIR       where everything goes (default fuzz/measure/coverage-<date>)
#   --seconds S     a session's length (default 300)
#   --fork F        libFuzzer -fork=F (default 4)
#   --corpus DIR    take DIR/<target>/ as the corpora instead of the corpus phase
#
# What it writes, under OUT:
#
#   meta.txt                      revision, toolchains, host, budgets
#   sessions/<target>/            fuzz-measure.sh's output; run-1/corpus is the corpus
#   corpus/<target>               a symlink to the corpus each later phase reads
#   source/<target>.profdata      the target's merged profile
#   source/<target>.json          its components (scripts/fuzz-coverage-components.py)
#   source/union.json             the same over every target's profile together
#   ir/<target>.json              `cove-fuzz coverage`'s counts
#   report.md, report.json
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
targets=(parse compile run structured differential differential_source ir ir_mutate image)
running=(run differential differential_source ir ir_mutate image)
out="" seconds=300 fork=4 corpus=""
phases=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --seconds) seconds="$2"; shift 2 ;;
    --fork) fork="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    corpus | source | ir | report) phases+=("$1"); shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
if [ ${#phases[@]} -eq 0 ]; then
  phases=(corpus source ir report)
  if [ -n "$corpus" ]; then phases=(source ir report); fi
fi
out="${out:-$root/fuzz/measure/coverage-$(date +%Y%m%dT%H%M%S)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
cd "$root"

has() { case " ${phases[*]} " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if [ ! -f "$out/meta.txt" ]; then
  {
    echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "revision: $(git rev-parse HEAD)"
    echo "tree: $(git status --porcelain --untracked-files=no | wc -l | tr -d ' ') modified tracked files"
    echo "rustc (nightly): $(rustc +nightly --version)"
    echo "cargo-fuzz: $(cargo fuzz --version)"
    echo "cargo-llvm-cov: $(cargo llvm-cov --version 2>/dev/null || echo none)"
    echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) logical cores"
    echo "session: ${seconds}s, -fork=$fork, libFuzzer seed 1, no sanitizer"
  } > "$out/meta.txt"
fi

# --- corpus ---------------------------------------------------------------
if has corpus; then
  for t in "${targets[@]}"; do
    start="$out/start/$t"
    rm -rf "$start"
    mkdir -p "$start"
    for dir in "fuzz/seeds/$t" "fuzz/regressions/$t"; do
      if [ -d "$dir" ]; then
        find "$dir" -type f -not -name '*.md' -not -path '*/known/*' -exec cp {} "$start/" \;
      fi
    done
    echo "start $t: $(find "$start" -type f | wc -l | tr -d ' ') inputs" | tee -a "$out/meta.txt"
    scripts/fuzz-measure.sh "$t" "$seconds" --runs 1 --fork "$fork" \
      --corpus "$start" --out "$out/sessions/$t"
    mkdir -p "$out/corpus"
    ln -sfn "$out/sessions/$t/run-1/corpus" "$out/corpus/$t"
    echo "corpus $t: $(find "$out/corpus/$t/" -type f | wc -l | tr -d ' ') inputs; $(tr '\n' ';' < "$out/sessions/$t/run-1/result.txt")" \
      | tee -a "$out/meta.txt"
  done
elif [ -n "$corpus" ]; then
  mkdir -p "$out/corpus"
  for t in "${targets[@]}"; do
    if [ -d "$corpus/$t" ]; then ln -sfn "$(cd "$corpus/$t" && pwd)" "$out/corpus/$t"; fi
  done
fi
