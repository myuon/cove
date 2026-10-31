#!/usr/bin/env bash
# The pull request's starting corpus: small, chosen for what it reaches per
# second of replay, and kept apart from the corpus the nightly run grows
# (docs/FUZZING.md, "The pull request's corpus").
#
#   scripts/fuzz-pr-corpus.sh select [--budget S] [--pool T=DIR]... [--only "T..."]
#   scripts/fuzz-pr-corpus.sh materialise <target> <dest>
#
# `select` writes fuzz/corpus-pr/<family>.list (what to start from) and
# <family>.selection.txt (why: each input's replay time and the features it
# added, and what the choice leaves to the nightly corpus). Three families:
#
#   differential_source  the repository's programs, measured by
#                        `differential_source`; the list also starts `run`
#                        and `parse` (`compile` starts from its committed
#                        inputs alone)
#   image                the same programs as image bodies, measured by
#                        `image`; the list also starts `ir_mutate`
#   ir, differential, structured
#                        inputs from a grown corpus (`--pool T=DIR`, e.g. the
#                        nightly one), copied into fuzz/corpus-pr/<target>/
#                        because a generator's input is not a program in the
#                        tree
#
# Every target's committed seeds and regressions are kept whatever they cost
# (`fuzz/regressions/*/known/` never: those still fail, and are the known
# findings a run compares against). `--budget` is seconds of single-process
# replay a target's start may cost, mandatory inputs included (default 6).
#
# `materialise` writes a target's starting corpus into <dest>: its seeds and
# regressions, and its family's list. scripts/fuzz-budget.sh calls it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/fuzz/corpus-pr"
bin="$root/target/checked/cove-fuzz"
command="${1:-}"
shift || true

committed() {
  for d in "$root/fuzz/seeds/$1" "$root/fuzz/regressions/$1"; do
    [ -d "$d" ] && find "$d" -type f -not -name '*.md' -not -path '*/known/*'
  done | sort
}

family() {
  case "$1" in
    # `compile` starts from its committed inputs alone: under ASan on the
    # CI's runner the list's 36 programs took 12.9 s of its 24 s
    # (docs/FUZZ_EVALUATION.md, section 12).
    differential_source | run | parse) echo differential_source ;;
    image | ir_mutate) echo image ;;
    *) echo "$1" ;;
  esac
}

sources() {
  (cd "$root" && find tests/e2e examples benches tools/covefmt crates/cove-sema/std \
    -name '*.cove' -not -path '*/target/*' | sort)
}

case "$command" in
  select)
    budget=6 pools=() only="differential_source image ir differential structured"
    while [ $# -gt 0 ]; do
      case "$1" in
        --budget) budget="$2"; shift 2 ;;
        --pool) pools+=("$2"); shift 2 ;;
        --only) only="$2"; shift 2 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
      esac
    done
    (cd "$root" && cargo build --profile checked -p cove-fuzz --features template --quiet)
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    mkdir -p "$dir"
    for t in $only; do
      mandatory=()
      while IFS= read -r f; do mandatory+=("$f"); done < <(committed "$t")
      "$bin" coverage-each "$t" ${mandatory[@]+"${mandatory[@]}"} > "$work/$t.mandatory.jsonl"
      case "$t" in
        differential_source)
          (cd "$root" && "$bin" coverage-each "$t" $(sources)) > "$work/$t.pool.jsonl"
          ;;
        image)
          mkdir -p "$work/bodies"
          (cd "$root" && "$bin" image-seed "$work/bodies" $(sources) 2> /dev/null)
          "$bin" coverage-each "$t" "$work/bodies" > "$work/$t.pool.jsonl"
          ;;
        *)
          pool=""
          for p in ${pools[@]+"${pools[@]}"}; do
            [ "${p%%=*}" = "$t" ] && pool="${p#*=}"
          done
          [ -n "$pool" ] || { echo "$t: no --pool $t=DIR given" >&2; exit 2; }
          "$bin" coverage-each "$t" "$pool" > "$work/$t.pool.jsonl"
          ;;
      esac
      python3 "$root/scripts/fuzz-pr-corpus.py" --budget "$budget" \
        --mandatory "$work/$t.mandatory.jsonl" --pool "$work/$t.pool.jsonl" > "$work/$t.chosen"
      {
        echo "# scripts/fuzz-pr-corpus.sh select --budget $budget, $(date -u +%Y-%m-%d), at $(git -C "$root" rev-parse --short HEAD), $(uname -sm)"
        echo "# kind	input	replay ms	features it added	the first of them"
        sed "s|$root/||g; s|$work/bodies/||g" "$work/$t.chosen"
      } > "$dir/$t.selection.txt"
      : > "$dir/$t.list"
      case "$t" in
        differential_source)
          awk -F'\t' '$1 == "chosen" {print "repo\t" $2}' "$work/$t.chosen" | sed "s|$root/||" >> "$dir/$t.list"
          ;;
        image)
          # A body is named for its source's path, separators made `_`.
          while IFS= read -r name; do
            for s in $(sources); do
              if [ "$(echo "$s" | tr '/.' '__')" = "$name" ]; then
                printf 'repo\t%s\n' "$s" >> "$dir/$t.list"
              fi
            done
          done < <(awk -F'\t' '$1 == "chosen" {print $2}' "$work/$t.chosen" | xargs -n1 basename)
          ;;
        *)
          rm -rf "${dir:?}/$t"
          mkdir -p "$dir/$t"
          while IFS= read -r f; do
            name="$(shasum "$f" | cut -c1-16)"
            cp "$f" "$dir/$t/$name"
            printf 'file\t%s\n' "$name" >> "$dir/$t.list"
          done < <(awk -F'\t' '$1 == "chosen" {print $2}' "$work/$t.chosen")
          ;;
      esac
      echo "$t: $(grep -c . "$dir/$t.list" || true) chosen; $(grep '^# budget' "$dir/$t.selection.txt")"
    done
    ;;
  materialise)
    t="${1:?usage: scripts/fuzz-pr-corpus.sh materialise <target> <dest>}"
    dest="${2:?usage: scripts/fuzz-pr-corpus.sh materialise <target> <dest>}"
    mkdir -p "$dest"
    while IFS= read -r f; do cp "$f" "$dest/"; done < <(committed "$t")
    list="$dir/$(family "$t").list"
    if [ -f "$list" ]; then
      repo=()
      while IFS=$'\t' read -r kind name; do
        case "$kind" in
          repo) repo+=("$name") ;;
          file) cp "$dir/$t/$name" "$dest/" ;;
        esac
      done < "$list"
      if [ ${#repo[@]} -gt 0 ]; then
        case "$t" in
          image) (cd "$root" && "$bin" image-seed "$dest" "${repo[@]}" 2> /dev/null) ;;
          ir_mutate) (cd "$root" && "$bin" ir-mutate-seed "$dest" "${repo[@]}" 2> /dev/null) ;;
          *) for s in "${repo[@]}"; do cp "$root/$s" "$dest/$(echo "$s" | tr '/.' '__')"; done ;;
        esac
      fi
    fi
    echo "$t: $(find "$dest" -type f | wc -l | tr -d ' ') inputs in $dest"
    ;;
  *)
    sed -n '2,30p' "$0" >&2
    exit 2
    ;;
esac
