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
#     branches an instrumented `cove-fuzz` (nightly, -Z coverage-options=branch)
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
#   --only "T..."   only these targets (the union is still made of every profile there is)
#
# What it writes, under OUT:
#
#   meta.txt                      revision, toolchains, host, budgets
#   sessions/<target>/            fuzz-measure.sh's output; run-1/corpus is the corpus
#   corpus/<target>               a symlink to the corpus each later phase reads
#   branches/<target>.profdata      the target's merged profile
#   branches/<target>.json          its components (scripts/fuzz-coverage-components.py)
#   branches/union.json             the same over every target's profile together
#   ir/<target>.json              `cove-fuzz coverage`'s counts
#   report.md, report.json
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
all=(parse compile run structured differential differential_source ir ir_mutate image)
targets=("${all[@]}")
running=(run structured differential differential_source ir ir_mutate image)
out="" seconds=300 fork=4 corpus=""
phases=()
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --seconds) seconds="$2"; shift 2 ;;
    --fork) fork="$2"; shift 2 ;;
    --corpus) corpus="$2"; shift 2 ;;
    --only) read -r -a targets <<< "$2"; running=("${targets[@]}"); shift 2 ;;
    corpus | branches | ir | report) phases+=("$1"); shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
if [ ${#phases[@]} -eq 0 ]; then
  phases=(corpus branches ir report)
  if [ -n "$corpus" ]; then phases=(branches ir report); fi
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

# --- source -----------------------------------------------------------------
# An instrumented `cove-fuzz`, in a target directory of its own so that it
# never replaces the ordinary build. Branch coverage is nightly-only
# (-Z coverage-options=branch); every crate is instrumented, and the
# component script keeps only the workspace's own sources.
sysroot="$(rustc +nightly --print sysroot)"
host="$(rustc +nightly -vV | sed -n 's/^host: //p')"
llvm="$sysroot/lib/rustlib/$host/bin"
covbin="$root/target/coverage/checked/cove-fuzz"
if has branches; then
  CARGO_TARGET_DIR="$root/target/coverage" \
    RUSTFLAGS="-C instrument-coverage -Z coverage-options=branch" \
    cargo +nightly build --profile checked -p cove-fuzz --features template --bin cove-fuzz --quiet
  mkdir -p "$out/branches"
  for t in "${targets[@]}"; do
    [ -d "$out/corpus/$t/" ] || continue
    raw="$out/branches/raw-$t"
    rm -rf "$raw" "$out/branches/$t.failed"
    mkdir -p "$raw"
    list="$out/branches/$t.files"
    find "$out/corpus/$t/" -type f | sort > "$list"
    start=$(date +%s)
    # Batches of 50 inputs a process. A batch whose process stops — a
    # finding panics the harness — is replayed one input a process, so an
    # input after the finding is still counted; the profile of a process that
    # panicked is still written, because a panic unwinds out of `main`.
    failed=0
    split -l 50 "$list" "$raw/batch-"
    for batch in "$raw"/batch-*; do
      if ! LLVM_PROFILE_FILE="$raw/%p-%8m.profraw" xargs "$covbin" "$t" < "$batch" > /dev/null 2>&1; then
        while IFS= read -r input; do
          LLVM_PROFILE_FILE="$raw/%p-%8m.profraw" "$covbin" "$t" "$input" > /dev/null 2>&1 \
            || { failed=$((failed + 1)); echo "$input" >> "$out/branches/$t.failed"; }
        done < "$batch"
      fi
      rm -f "$batch"
    done
    "$llvm/llvm-profdata" merge -sparse "$raw"/*.profraw -o "$out/branches/$t.profdata"
    rm -rf "$raw"
    echo "branches $t: $(wc -l < "$list" | tr -d ' ') inputs replayed in $(( $(date +%s) - start ))s, $failed stopped the harness" \
      | tee -a "$out/meta.txt"
  done
  profiles=()
  for t in "${all[@]}"; do
    if [ -f "$out/branches/$t.profdata" ]; then profiles+=("$out/branches/$t.profdata"); fi
  done
  "$llvm/llvm-profdata" merge -sparse "${profiles[@]}" -o "$out/branches/union.profdata"
  for name in "${targets[@]}" union; do
    p="$out/branches/$name.profdata"
    [ -f "$p" ] || continue
    "$llvm/llvm-cov" export -format=text -instr-profile "$p" "$covbin" \
      -ignore-filename-regex='/(\.cargo|\.rustup|rustc)/' \
      | python3 "$root/scripts/fuzz-coverage-components.py" "$root" > "$out/branches/$name.json"
  done
fi

# --- ir ---------------------------------------------------------------------
# The ordinary (uninstrumented) build: `cove-fuzz coverage` watches every
# instruction, and instrumentation would only slow it further.
if has ir; then
  cargo build --profile checked -p cove-fuzz --features template --quiet
  mkdir -p "$out/ir"
  "$root/target/checked/cove-fuzz" coverage-defined > "$out/ir/defined.json"
  for t in "${running[@]}"; do
    case "$t" in parse | compile) continue ;; esac
    [ -d "$out/corpus/$t/" ] || continue
    start=$(date +%s)
    "$root/target/checked/cove-fuzz" coverage "$t" "$out/corpus/$t/" \
      > "$out/ir/$t.json" 2> "$out/ir/$t.txt" || true
    echo "ir $t: $(head -1 "$out/ir/$t.txt") in $(( $(date +%s) - start ))s" | tee -a "$out/meta.txt"
  done
fi

# --- report -----------------------------------------------------------------
if has report; then
  python3 "$root/scripts/fuzz-coverage-table.py" "$out" > "$out/report.md"
  echo "wrote $out/report.md and $out/report.json"
fi
