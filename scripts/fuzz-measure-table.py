#!/usr/bin/env python3
"""Tabulates what scripts/fuzz-measure.sh wrote: one row a metric, one column
a session, then the minimum, the median and the maximum, as Markdown.

    scripts/fuzz-measure-table.py fuzz/measure/<dir> [session|corpus]

`session` (the default) reads each run's in-session counts, `corpus` the
replay of its grown corpus. Shares are of the row the metric is a part of,
and are computed per session before the spread is taken.
"""
import json
import os
import statistics
import sys


def get(tree, *path):
    for key in path:
        if not isinstance(tree, dict) or key not in tree:
            return 0
        tree = tree[key]
    return tree if isinstance(tree, int) else 0


def total(tree, *path):
    for key in path:
        if not isinstance(tree, dict) or key not in tree:
            return 0
        tree = tree[key]
    if isinstance(tree, int):
        return tree
    return sum(total(v) for v in tree.values())


def share(part, whole):
    return None if whole == 0 else 100.0 * part / whole


PAIRS = ["ast|vm", "vm|native", "vm|image", "vm|vm-gc", "vm-gc|native-gc", "vm|vm-again"]


def rows(s, target):
    c = lambda k: get(s, "comparisons", k)
    ran = get(s, "stages", "ran")
    out = [
        ("inputs (executions)", get(s, "inputs")),
        ("ran (every mode run and compared)", ran),
        ("started on `vm`", get(s, "stages", "started")),
        ("full", c("full")),
        ("full, every run success", c("full_success")),
        ("full, every run success, native ran", c("full_success_with_native")),
        ("% of ran: full, every run success", share(c("full_success"), ran)),
        ("prefix-only", c("prefix")),
        ("prefix-only, empty", c("empty_prefix")),
        ("% of prefix-only: empty", share(c("empty_prefix"), c("prefix"))),
        ("native ran (programs)", get(s, "programs", "native_ran")),
        ("collection under a compiled frame (programs)", get(s, "programs", "gc_while_compiled")),
        ("mismatches", get(s, "mismatches")),
    ]
    if target in ("ir", "ir_mutate", "image"):
        out.insert(1, ("built", get(s, "stages", "built")))
        out.insert(2, ("verified", get(s, "stages", "verified")))
    for pair in PAIRS:
        name = pair.replace("|", " vs ")
        full = total(s, "pairs", pair, "full")
        if full == 0 and total(s, "pairs", pair) == 0:
            continue
        out += [
            (f"`{name}` full", full),
            (f"`{name}` full: success / error / trap / limit", "{} / {} / {} / {}".format(
                *(get(s, "pairs", pair, "full", k) for k in ("success", "error", "trap", "limit")))),
            (f"`{name}` full success with native", get(s, "pairs", pair, "full_success_with_native")),
            (f"`{name}` prefix nonempty / empty", "{} / {}".format(
                get(s, "pairs", pair, "prefix", "nonempty"), get(s, "pairs", pair, "prefix", "empty"))),
            (f"`{name}` skipped", total(s, "pairs", pair, "skipped")),
        ]
    return out


def fmt(v):
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:.1f}%"
    if isinstance(v, int):
        return f"{v:,}"
    return str(v)


def main():
    root = sys.argv[1]
    which = sys.argv[2] if len(sys.argv) > 2 else "session"
    meta = open(os.path.join(root, "meta.txt")).read()
    target = meta.split("\n")[0].split(": ")[1]
    runs = sorted(d for d in os.listdir(root) if d.startswith("run-"))
    tables = []
    for run in runs:
        path = os.path.join(root, run, f"{which}.json")
        tables.append(rows(json.load(open(path)), target) if os.path.exists(path) else [])
    names = [r[0] for r in tables[0]]
    print(f"| {target} ({which}) | " + " | ".join(runs) + " | min | median | max |")
    print("|---|" + "---|" * (len(runs) + 3))
    for i, name in enumerate(names):
        values = [t[i][1] if i < len(t) else None for t in tables]
        nums = [v for v in values if isinstance(v, (int, float))]
        spread = [min(nums), statistics.median(nums), max(nums)] if nums else ["", "", ""]
        if nums and all(isinstance(v, int) for v in nums):
            spread[1] = round(spread[1])
        print(f"| {name} | " + " | ".join(fmt(v) for v in values) + " | "
              + " | ".join(fmt(v) if v != "" else "" for v in spread) + " |")


if __name__ == "__main__":
    main()
