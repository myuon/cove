#!/usr/bin/env python3
"""What budget runs reached, and when: a measurement apart from the run.

    fuzz-budget-reach.py --cove-fuzz BIN --at 30 60 120 CONFIG=RUN_DIR... > reach.json

For every run directory scripts/fuzz-budget.sh wrote, each target's final
corpus is replayed once, an input at a time, by `cove-fuzz coverage-each`
(the IR instruction keys executed anywhere and in compiled code, trigger
states, and the type x boundary matrix's cells); what the run had reached at
`T` seconds is the union over the inputs whose file existed by then
(libFuzzer writes an input when it adds it, so its modification time is when
it was found; the starting corpus is written before the clock starts).
`parse` and `compile` run no program and have no such features.

The replay is the expensive part and is timed: `replay_seconds` per run.
Per configuration, over its runs: the union and the per-run counts at each
time, and for each target the features no other target of the same run
reached — what dropping that target would have lost.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

RUNS_PROGRAMS = {"run", "structured", "differential", "differential_source", "ir", "ir_mutate", "image"}


def classes(features):
    out = {"keys": 0, "keys_compiled": 0, "states": 0, "states_compiled": 0, "cells": 0, "cells_compiled": 0, "programs": 0}
    names = {"x": "keys", "c": "keys_compiled", "t": "states", "tc": "states_compiled",
             "m": "cells", "mc": "cells_compiled", "s": "programs"}
    for f in features:
        out[names[f.split(":", 1)[0]]] += 1
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cove-fuzz", required=True)
    ap.add_argument("--at", nargs="+", type=float, default=[30, 60, 120])
    ap.add_argument("runs", nargs="+")
    args = ap.parse_args()
    configs = {}
    for spec in args.runs:
        config, _, d = spec.partition("=")
        d = Path(d)
        summary = json.loads((d / "summary.json").read_text())
        meta = (d / "meta.txt").read_text()
        explore_from = None
        for line in meta.splitlines():
            if line.startswith("explore from "):
                explore_from = float(line.split()[2])
        began = time.time()
        per_target = {}
        for t_dir in sorted((d / "corpus").iterdir()):
            t = t_dir.name
            if t not in RUNS_PROGRAMS:
                continue
            res = subprocess.run([args.cove_fuzz, "coverage-each", t, str(t_dir)],
                                 capture_output=True, text=True)
            rows = [json.loads(l) for l in res.stdout.splitlines() if l.strip()]
            per_target[t] = []
            for r in rows:
                try:
                    mtime = os.stat(r["file"]).st_mtime
                except OSError:
                    continue
                per_target[t].append((mtime - explore_from, set(r["features"])))
        replay_seconds = time.time() - began
        at = {}
        for T in args.at:
            union, by_target = set(), {}
            for t, rows in per_target.items():
                got = set()
                for when, feats in rows:
                    if when <= T:
                        got |= feats
                by_target[t] = got
                union |= got
            at[str(int(T))] = {"union": classes(union), "union_set": sorted(union),
                               "targets": {t: classes(s) for t, s in by_target.items()}}
        final = at[str(int(max(args.at)))]
        sets = {t: set() for t in per_target}
        for t, rows in per_target.items():
            for _, feats in rows:
                sets[t] |= feats
        unique = {}
        for t, s in sets.items():
            others = set().union(*(o for u, o in sets.items() if u != t)) if len(sets) > 1 else set()
            only = sorted(s - others)
            unique[t] = {"count": len(only), "classes": classes(only), "features": only}
        configs.setdefault(config, []).append({
            "run": str(d), "replay_seconds": round(replay_seconds, 1), "at": at,
            "unique": unique,
            "worker_seconds": summary["worker_seconds"],
            "total_seconds": summary["phases"]["total_seconds"],
        })
    out = {}
    for config, runs in configs.items():
        union_all = set()
        for r in runs:
            union_all |= set(r["at"][str(int(max(args.at)))]["union_set"])
        out[config] = {
            "runs": runs,
            "union_over_runs": classes(union_all),
            "union_over_runs_set": sorted(union_all),
        }
    for config, c in out.items():
        for r in c["runs"]:
            for T in r["at"].values():
                T.pop("union_set", None)
    json.dump(out, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    sys.exit(main())
