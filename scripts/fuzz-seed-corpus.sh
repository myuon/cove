#!/usr/bin/env bash
# Builds fuzz/corpus/<target>/ from the repository's own programs and the
# committed seeds. The corpus is not committed (fuzz/.gitignore); this script
# is how it is made, on a laptop and in CI alike. See docs/FUZZING.md.
#
#   scripts/fuzz-seed-corpus.sh                 # every target
#   scripts/fuzz-seed-corpus.sh run structured  # just these
#
# - parse, compile, run: every .cove file under tests/e2e, examples, benches
#   and tools/covefmt, and the standard library's own sources;
# - image: the image body each of those lowers to, written by the stable
#   `cove-fuzz image-seed` command (so asking for `image` builds `cove-fuzz`);
# - structured: nothing from the tree, since its input is a stream of choices
#   rather than a program; the committed seeds start it.
#
# Every target also gets fuzz/seeds/<target>/ and fuzz/regressions/<target>/,
# so a fresh corpus already covers each input that once found a bug.
#
# It adds to a corpus that is already there and removes nothing, so running
# it over a corpus a long session grew keeps what that session found.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
corpus=fuzz/corpus

if [ $# -eq 0 ]; then
  set -- parse compile run structured image
fi

sources=()
while IFS= read -r -d '' file; do
  sources+=("$file")
done < <(find tests/e2e examples benches tools/covefmt crates/cove-sema/std \
  -name '*.cove' -not -path '*/target/*' -print0 | sort -z)

for target in "$@"; do
  mkdir -p "$corpus/$target"
  case "$target" in
    parse | compile | run)
      for file in "${sources[@]}"; do
        # One flat directory: the path, with separators made safe, is the name.
        cp "$file" "$corpus/$target/$(echo "$file" | tr '/.' '__')"
      done
      ;;
    image)
      cargo build --profile checked -p cove-fuzz --quiet
      ./target/checked/cove-fuzz image-seed "$corpus/image" "${sources[@]}"
      ;;
    structured) ;;
    *)
      echo "no target \`$target\`: parse, compile, run, structured, image" >&2
      exit 2
      ;;
  esac
  for dir in "fuzz/seeds/$target" "fuzz/regressions/$target"; do
    if [ -d "$dir" ]; then
      # Not known/: those still crash, and libFuzzer stops on a crash in its
      # starting corpus before it has fuzzed anything.
      find "$dir" -type f -not -name '*.md' -not -path '*/known/*' \
        -exec cp {} "$corpus/$target/" \;
    fi
  done
  printf '%-10s %5d inputs\n' "$target" "$(find "$corpus/$target" -type f | wc -l)"
done
