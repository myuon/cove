#!/usr/bin/env python3
"""Branch coverage by component, from `llvm-cov export -format=text` on stdin.

    llvm-cov export ... | scripts/fuzz-coverage-components.py <repo root> > out.json

Used by scripts/fuzz-coverage-report.sh (docs/FUZZ_EVALUATION.md, section 11).
A branch is one outcome of a condition, as llvm-cov counts them: a condition
the instrumentation saw is two branches, its true and its false side, and
each is reached if it was taken at least once. The component of a branch is
decided by its file (COMPONENTS), except in the two files where the collector
shares a file with the machine (GC_FUNCTIONS), where it is decided by the
name of the function the branch is in.

Writes, per component: total and reached branches, and the functions with the
most unreached branches, each with its file, line, and name.
"""

import functools
import json
import os
import re
import sys

# Component, and the path prefixes (relative to the repository) whose files
# belong to it. The first matching rule wins; a file matching none is `other`.
COMPONENTS = [
    ("parser", ["crates/cove-syntax/src/lexer.rs", "crates/cove-syntax/src/parser.rs",
                "crates/cove-syntax/src/number.rs", "crates/cove-syntax/src/token.rs",
                "crates/cove-syntax/src/ast.rs", "crates/cove-syntax/src/lib.rs"]),
    ("checker", ["crates/cove-sema/src/"]),
    ("lowering", ["crates/cove-ir/src/lower/", "crates/cove-ir/src/legalize.rs"]),
    ("verifier", ["crates/cove-ir/src/verify.rs", "crates/cove-ir/src/verify/",
                  "crates/cove-ir/src/bytecode/verify.rs"]),
    ("native", ["crates/cove-native/src/", "crates/cove-runtime/src/vm/exec/native.rs",
                "crates/cove-runtime/src/native.rs"]),
    ("gc", ["crates/cove-runtime/src/heap.rs"]),
    # Debugging and profiling views: on no replay path.
    ("vm tooling", ["crates/cove-runtime/src/vm/debug.rs", "crates/cove-runtime/src/vm/render.rs",
                    "crates/cove-runtime/src/vm/profile.rs"]),
    ("vm", ["crates/cove-runtime/src/vm/", "crates/cove-ir/src/bytecode/"]),
    ("interpreter", ["crates/cove-runtime/src/interp.rs"]),
    # Outside the seven components, reported so that nothing is dropped.
    ("runtime, shared", ["crates/cove-runtime/src/"]),
    ("ir, shared", ["crates/cove-ir/src/"]),
    ("fuzz harness", ["crates/cove-fuzz/src/"]),
]

# Files the collector shares with the machine, and the function names that
# are the collector's in them: marking, tracing, sweeping, the roots a frame
# gives up, and the free-list search a sweep feeds.
GC_FUNCTIONS = {
    "crates/cove-runtime/src/vm/mem.rs": re.compile(
        r"\b(collect\w*|mark\w*|trace\w*|sweep\w*|roots?\w*|scan_free|reclaim\w*)\b"),
    "crates/cove-runtime/src/vm/exec.rs": re.compile(r"\b(roots?\w*|collect\w*|\w*_roots?)\b"),
    "crates/cove-runtime/src/vm/exec/frames.rs": re.compile(r"\b(roots?\w*|\w*_roots?)\b"),
}

FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def component(rel):
    for name, prefixes in COMPONENTS:
        if any(rel == p or rel.startswith(p) for p in prefixes):
            return name
    return "other"


def main():
    root = os.path.realpath(sys.argv[1])
    data = json.load(sys.stdin)["data"][0]
    lines_of = {}

    def source(path):
        if path not in lines_of:
            try:
                with open(path, encoding="utf-8", errors="replace") as f:
                    lines_of[path] = f.read().split("\n")
            except OSError:
                lines_of[path] = []
        return lines_of[path]

    # Function extents per file: (start line, end line, mangled name). A
    # branch belongs to the innermost extent around it — a closure's, if it is
    # in one — and a function is named by the `fn` on or above its first line.
    extents = {}
    # Code regions by location, executed if any instantiation executed them.
    regions_at = {}
    for f in data["functions"]:
        files = f.get("filenames") or []
        for r in f.get("regions", []):
            if files and r[5] == 0 and r[7] == 0:
                key = (files[0], r[0], r[1], r[2], r[3])
                regions_at[key] = max(regions_at.get(key, 0), r[4])
    for f in data["functions"]:
        files = f.get("filenames") or []
        regions = [r for r in f.get("regions", []) if r[5] == 0]  # file id 0: its own file
        if not files or not regions:
            continue
        path = files[0]
        start = min(r[0] for r in regions)
        end = max(r[2] for r in regions)
        extents.setdefault(path, set()).add((start, end))

    @functools.lru_cache(maxsize=None)
    def name_at(path, line):
        text = source(path)
        for at in range(min(line, len(text)), 0, -1):
            m = FN.search(text[at - 1])
            if m:
                return m.group(1), at
        return "?", line

    totals = {}
    places = {}
    for f in data["files"]:
        path = f["filename"]
        if not path.startswith(root + "/"):
            continue
        rel = path[len(root) + 1:]
        if "/tests/" in rel or rel.startswith("tests/"):
            continue
        base = component(rel)
        spans = sorted(extents.get(path, ()), key=lambda e: (e[0], -e[1]))
        # Several instantiations of one generic function report the same
        # branch: merge by location, taken if any instantiation took it.
        merged = {}
        for b in f.get("branches", []):
            key = (b[0], b[1], b[2], b[3])
            t, fl = merged.get(key, (0, 0))
            merged[key] = (t + b[4], fl + b[5])
        for (line, col, _, _), (taken, not_taken) in merged.items():
            inner = None
            for s, e in spans:
                if s <= line <= e and (inner is None or (s >= inner[0] and e <= inner[1])):
                    inner = (s, e)
            fname, fline = name_at(path, inner[0] if inner else line)
            comp = base
            gc = GC_FUNCTIONS.get(rel)
            if gc and gc.search(fname):
                comp = "gc"
            reached = (taken > 0) + (not_taken > 0)
            t = totals.setdefault(comp, [0, 0])
            t[0] += 2
            t[1] += reached
            p = places.setdefault((comp, rel, fline, fname), [0, 0])
            p[0] += 2
            p[1] += reached

    region_totals = {}
    by_file = {}
    for (path, line, col, _, _), count in regions_at.items():
        by_file.setdefault(path, []).append((line, count))
    for path, items in by_file.items():
        if not path.startswith(root + "/"):
            continue
        rel = path[len(root) + 1:]
        if "/tests/" in rel or rel.startswith("tests/"):
            continue
        base = component(rel)
        spans = sorted(extents.get(path, ()), key=lambda e: (e[0], -e[1]))
        gc = GC_FUNCTIONS.get(rel)
        for line, count in items:
            comp = base
            if gc:
                inner = None
                for s_, e in spans:
                    if s_ <= line <= e and (inner is None or (s_ >= inner[0] and e <= inner[1])):
                        inner = (s_, e)
                if gc.search(name_at(path, inner[0] if inner else line)[0]):
                    comp = "gc"
            t = region_totals.setdefault(comp, [0, 0])
            t[0] += 1
            t[1] += count > 0

    out = {"components": {}}
    for comp in sorted(set(totals) | set(region_totals)):
        total, reached = totals.get(comp, (0, 0))
        unreached = sorted(
            ((v[0] - v[1], v[0], k) for k, v in places.items() if k[0] == comp and v[0] > v[1]),
            key=lambda x: (-x[0], x[2]))
        never = sum(1 for k, v in places.items() if k[0] == comp and v[1] == 0)
        out["components"][comp] = {
            "branches": total,
            "reached": reached,
            "percent": round(100.0 * reached / total, 1) if total else None,
            "regions": region_totals.get(comp, [0, 0])[0],
            "regions_reached": region_totals.get(comp, [0, 0])[1],
            "functions_with_branches": sum(1 for k in places if k[0] == comp),
            "functions_no_branch_reached": never,
            "top_unreached": [
                {"file": k[1], "line": k[2], "function": k[3], "unreached": u, "branches": n}
                for u, n, k in unreached[:15]
            ],
        }
    json.dump(out, sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
