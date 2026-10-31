#!/usr/bin/env python3
"""Chooses a small starting corpus from `cove-fuzz coverage-each` lines.

    fuzz-pr-corpus.py --budget SECONDS --mandatory M.jsonl --pool P.jsonl > chosen.tsv

Every mandatory input (the committed seeds and regressions) is kept,
whatever it costs, and what it reaches counts as covered. Then, greedily,
the pool input that reaches the most features nothing chosen reaches yet per
second of replay, until the budget is spent or nothing adds a feature. A
feature is a semantic one — an instruction kind executed (and in compiled
code), a trigger state, a type x boundary cell, a kind of program the
comparison saw — not a branch, so a boundary that code coverage would call
redundant is kept for its own sake.

Writes, tab-separated: `kind file ms new-features first-new-features`
(`kind` is `mandatory` or `chosen`), then `#` lines: the budget spent, the
mandatory inputs' cost on their own, and the features the pool reaches that
the choice does not — what is left to the nightly corpus.
"""

import argparse
import json
import sys

OVERHEAD_MS = 2.0


def read(path):
    out = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                out.append(json.loads(line))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--budget", type=float, required=True, help="seconds of replay, mandatory included")
    ap.add_argument("--mandatory", required=True)
    ap.add_argument("--pool", required=True)
    args = ap.parse_args()
    mandatory = read(args.mandatory)
    pool = [p for p in read(args.pool) if not p["finding"]]
    covered = set()
    spent = 0.0
    rows = []
    for m in mandatory:
        new = sorted(set(m["features"]) - covered)
        covered |= set(m["features"])
        spent += m["ms"]
        rows.append(("mandatory", m["file"], m["ms"], new))
    mandatory_ms = spent
    budget_ms = args.budget * 1000.0
    left = list(pool)
    while left and spent < budget_ms:
        best, best_score = None, 0.0
        for p in left:
            if spent + p["ms"] > budget_ms:
                continue
            gain = len(set(p["features"]) - covered)
            score = gain / (p["ms"] + OVERHEAD_MS)
            if gain and score > best_score:
                best, best_score = p, score
        if best is None:
            break
        new = sorted(set(best["features"]) - covered)
        covered |= set(best["features"])
        spent += best["ms"]
        rows.append(("chosen", best["file"], best["ms"], new))
        left.remove(best)
    reachable = set()
    for p in pool + mandatory:
        reachable |= set(p["features"])
    lost = sorted(reachable - covered)
    for kind, file, ms, new in rows:
        print("%s\t%s\t%.1f\t%d\t%s" % (kind, file, ms, len(new), " ".join(new[:6])))
    print("# budget %.1fs; spent %.2fs, of which mandatory %.2fs (%d inputs); chosen %d of %d pool inputs"
          % (args.budget, spent / 1000, mandatory_ms / 1000, len(mandatory),
             sum(1 for r in rows if r[0] == "chosen"), len(pool)))
    print("# features: %d covered of %d the pool and the mandatory inputs reach; %d left to the nightly corpus"
          % (len(covered), len(reachable), len(lost)))
    for f in lost:
        print("# lost\t%s" % f)
    if mandatory_ms > budget_ms:
        print("# WARNING: the mandatory inputs alone take %.2fs, over the %.1fs budget" % (mandatory_ms / 1000, args.budget))
    return 0


if __name__ == "__main__":
    sys.exit(main())
