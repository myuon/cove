#!/usr/bin/env bash
# Artificial defects for measuring what the fuzzing harness detects
# (issue 681; docs/FUZZ_EVALUATION.md is the record of what was measured).
#
#   scripts/fuzz-mutant.sh list                  the mutants, one a line
#   scripts/fuzz-mutant.sh check                 every patch applies to this tree, and no
#                                                mutant is applied in it (CI runs this)
#   scripts/fuzz-mutant.sh apply  <name> [DIR]   a scratch worktree of HEAD at DIR
#                                                (default fuzz/measure/mutants/<name>), with
#                                                the mutant applied there
#   scripts/fuzz-mutant.sh verify <name> [DIR]   that DIR holds exactly that mutant, and
#                                                this tree holds none
#   scripts/fuzz-mutant.sh run    <name> [DIR] -- COMMAND...
#                                                COMMAND in that worktree, which builds
#                                                into a target directory of its own
#   scripts/fuzz-mutant.sh revert <name> [DIR]   remove the worktree, and with it the mutant
#
# A mutant is a patch, fuzz/mutants/<name>.patch, that adds one marked defect
# (`COVE-MUTANT <name>`) to the runtime or the code generator. **It is never
# applied to a tree anybody builds from**: `apply` makes a detached worktree
# of the current commit under fuzz/measure/ (which git ignores) and applies it
# there, `revert` deletes that worktree, and `check` fails if the marker is
# anywhere in this tree outside fuzz/mutants/ and this script. A patch is a
# file and not a feature or a cfg because a cfg is code that ships in every
# build and is one wrong flag from being on, where a patch has to be applied
# by hand, to a copy, to exist at all.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
mutants="$root/fuzz/mutants"
marker="COVE-MUTANT"

usage() {
  sed -n '4,17p' "$0" >&2
  exit 2
}

patch_of() {
  local p="$mutants/$1.patch"
  [ -f "$p" ] || { echo "no mutant \`$1\` (scripts/fuzz-mutant.sh list)" >&2; exit 2; }
  echo "$p"
}

# The marker anywhere in `$1`'s tracked or untracked sources, outside the
# mutants and this script.
markers_in() {
  (cd "$1" && git grep -n --untracked "$marker" -- . \
    ':!fuzz/mutants' ':!scripts/fuzz-mutant.sh' ':!docs/FUZZ_EVALUATION.md' ':!fuzz/measure' || true)
}

command="${1:-}"
[ -n "$command" ] || usage
shift
case "$command" in
  list)
    for p in "$mutants"/*.patch; do
      basename "$p" .patch
    done
    ;;
  check)
    status=0
    for p in "$mutants"/*.patch; do
      if git -C "$root" apply --check "$p" 2> /dev/null; then
        echo "applies: $(basename "$p" .patch)"
      else
        echo "DOES NOT APPLY: $(basename "$p" .patch)" >&2
        status=1
      fi
    done
    found="$(markers_in "$root")"
    if [ -n "$found" ]; then
      echo "a mutant is applied in this tree:" >&2
      echo "$found" >&2
      status=1
    else
      echo "no mutant is applied in this tree"
    fi
    exit "$status"
    ;;
  apply | verify | run | revert)
    name="${1:?usage: scripts/fuzz-mutant.sh $command <name> [DIR]}"
    shift
    patch="$(patch_of "$name")"
    dir="$root/fuzz/measure/mutants/$name"
    if [ $# -gt 0 ] && [ "$1" != "--" ]; then
      dir="$1"
      shift
    fi
    ;;
  *) usage ;;
esac

case "$command" in
  apply)
    if [ -e "$dir" ]; then
      echo "$dir exists: revert it first" >&2
      exit 1
    fi
    mkdir -p "$(dirname "$dir")"
    git -C "$root" worktree add --detach --quiet "$dir" HEAD
    git -C "$dir" apply "$patch"
    echo "applied $name in $dir (at $(git -C "$dir" rev-parse --short HEAD))"
    ;;
  verify)
    found="$(markers_in "$root")"
    if [ -n "$found" ]; then
      echo "a mutant is applied in $root itself:" >&2
      echo "$found" >&2
      exit 1
    fi
    [ -d "$dir" ] || { echo "$dir: no worktree (apply first)" >&2; exit 1; }
    applied="$(cd "$dir" && git grep -l "$marker $name" -- crates || true)"
    others="$(cd "$dir" && git grep -n "$marker" -- crates | grep -v "$marker $name" || true)"
    if [ -z "$applied" ] || [ -n "$others" ]; then
      echo "$dir does not hold exactly $name" >&2
      exit 1
    fi
    echo "$dir holds $name ($applied), and $root holds no mutant"
    ;;
  run)
    [ "${1:-}" = "--" ] && shift
    [ $# -gt 0 ] || usage
    [ -d "$dir" ] || { echo "$dir: no worktree (apply first)" >&2; exit 1; }
    cd "$dir"
    "$@"
    ;;
  revert)
    if [ -d "$dir" ]; then
      git -C "$root" worktree remove --force "$dir"
      echo "removed $dir"
    fi
    git -C "$root" worktree prune
    ;;
esac
