#!/usr/bin/env python3
"""The tables of docs/FUZZ_EVALUATION.md section 11, from what
scripts/fuzz-coverage-report.sh wrote under OUT.

    scripts/fuzz-coverage-table.py OUT > OUT/report.md   # and writes OUT/report.json
"""

import json
import os
import sys

COMPONENTS = ["parser", "checker", "lowering", "verifier", "vm", "native", "gc"]
EXTRA = ["interpreter", "runtime, shared", "ir, shared", "vm tooling", "fuzz harness", "other"]
TARGETS = ["parse", "compile", "run", "structured", "differential", "differential_source",
           "ir", "ir_mutate", "image"]


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def get(tree, *path):
    for key in path:
        if not isinstance(tree, dict) or key not in tree:
            return 0
        tree = tree[key]
    return tree if isinstance(tree, int) else 0


def merge(a, b):
    for k, v in b.items():
        if isinstance(v, dict):
            merge(a.setdefault(k, {}), v)
        else:
            a[k] = a.get(k, 0) + v


def pct(n, d):
    return f"{100.0 * n / d:.1f}%" if d else "—"


def main():
    out = sys.argv[1]
    report = {"meta": open(os.path.join(out, "meta.txt")).read() if os.path.exists(
        os.path.join(out, "meta.txt")) else ""}
    lines = ["# Fuzz corpus coverage", "", "```", report["meta"].rstrip(), "```", ""]

    # 1. Branches by component.
    branches = {t: load(os.path.join(out, "branches", f"{t}.json")) for t in TARGETS + ["union"]}
    report["branches"] = {t: v for t, v in branches.items() if v}
    union = branches.get("union")
    if union:
        lines += ["## Branches by component (union of every target)", "",
                  "| component | branches | reached | % | regions | reached | % |",
                  "|---|---:|---:|---:|---:|---:|---:|"]
        for c in COMPONENTS + EXTRA:
            v = union["components"].get(c)
            if not v:
                continue
            lines.append(
                f"| {c} | {v['branches']:,} | {v['reached']:,} | {pct(v['reached'], v['branches'])} "
                f"| {v['regions']:,} | {v['regions_reached']:,} | {pct(v['regions_reached'], v['regions'])} |")
        lines += ["", "## Branch % by component and target", "",
                  "| target | " + " | ".join(COMPONENTS) + " |",
                  "|---|" + "---:|" * len(COMPONENTS)]
        for t in TARGETS + ["union"]:
            d = branches.get(t)
            if not d:
                continue
            cells = []
            for c in COMPONENTS:
                v = d["components"].get(c, {"branches": 0, "reached": 0})
                cells.append(pct(v["reached"], v["branches"]))
            lines.append(f"| {t} | " + " | ".join(cells) + " |")
        lines += ["", "## Most unreached branches, by function (union)", ""]
        for c in COMPONENTS:
            v = union["components"].get(c)
            if not v:
                continue
            lines += [f"### {c}", "", "| function | file:line | unreached / branches |", "|---|---|---:|"]
            for p in v["top_unreached"][:10]:
                lines.append(f"| `{p['function']}` | {p['file']}:{p['line']} | {p['unreached']} / {p['branches']} |")
            lines.append("")

    # 2. Instruction kinds.
    defined = load(os.path.join(out, "ir", "defined.json")) or {"keys": [], "cells": []}
    ir = {}
    for t in TARGETS:
        d = load(os.path.join(out, "ir", f"{t}.json"))
        if d:
            ir[t] = d
    merged = {}
    for d in ir.values():
        merge(merged, d)
    if ir:
        ir_all = dict(ir, union=merged)
        report["ir"] = {}
        lines += ["## IR instruction kinds", "",
                  f"{len(defined['keys'])} keys defined (what the verifier's shape checks admit).", "",
                  "| target | programs measured | present | executed | executed in compiled code | % of defined executed |",
                  "|---|---:|---:|---:|---:|---:|"]
        for t, d in ir_all.items():
            cov = d.get("coverage", {})
            n = {col: sum(1 for k in defined["keys"] if get(cov, k, col) > 0)
                 for col in ["present", "executed", "executed_compiled"]}
            report["ir"][t] = dict(n, measured=get(d, "programs", "measured"),
                                   never_executed=[k for k in defined["keys"] if get(cov, k, "executed") == 0])
            lines.append(f"| {t} | {get(d, 'programs', 'measured'):,} | {n['present']} | {n['executed']} "
                         f"| {n['executed_compiled']} | {pct(n['executed'], len(defined['keys']))} |")
        cov = merged.get("coverage", {})
        never = [k for k in defined["keys"] if get(cov, k, "executed") == 0]
        held = [k for k in never if get(cov, k, "present") > 0]
        lines += ["", f"Never executed in the union ({len(never)}); of them present in some program "
                  f"({len(held)}): " + ", ".join(f"`{k}`" for k in held), "",
                  "Never present: " + ", ".join(f"`{k}`" for k in never if k not in held), "",
                  "Executed, but never in compiled code: " + ", ".join(
                      f"`{k}`" for k in defined["keys"]
                      if get(cov, k, "executed") > 0 and get(cov, k, "executed_compiled") == 0), ""]

        # 3. Matrix.
        cells = defined["cells"]
        report["matrix"] = {}
        families = []
        for c in cells:
            fam = ".".join(c.split(".")[:1])
            if fam not in families:
                families.append(fam)
        lines += ["## Type × boundary matrix", "",
                  f"{len(cells)} cells; each is counted on the encoded VM (anywhere) and in compiled code "
                  f"(a natively compiled function other than the entry): {2 * len(cells)} cell × tier.", "",
                  "| target | " + " | ".join(f"{f} (VM / compiled)" for f in families) + " | all (VM / compiled) |",
                  "|---|" + "---:|" * (len(families) + 1)]
        for t, d in ir_all.items():
            m = d.get("matrix", {})
            row = []
            tot = [0, 0, 0]
            for f in families:
                fc = [c for c in cells if c.startswith(f + ".")]
                vm = sum(1 for c in fc if get(m, c, "executed") > 0)
                nat = sum(1 for c in fc if get(m, c, "executed_compiled") > 0)
                row.append(f"{vm} / {nat} of {len(fc)}")
                tot[0] += vm
                tot[1] += nat
                tot[2] += len(fc)
            report["matrix"][t] = {c: [get(m, c, "executed"), get(m, c, "executed_compiled")] for c in cells}
            lines.append(f"| {t} | " + " | ".join(row) + f" | {tot[0]} / {tot[1]} of {tot[2]} |")
        m = merged.get("matrix", {})
        lines += ["", "Union, per cell (programs: VM / compiled):", ""]
        for f in families:
            fc = [c for c in cells if c.startswith(f + ".")]
            lines.append(f"- **{f}**: " + ", ".join(
                f"`{c[len(f) + 1:]}` {get(m, c, 'executed')}/{get(m, c, 'executed_compiled')}" for c in fc))
        lines += ["", "Never executed (union): " + ", ".join(
            f"`{c}`" for c in cells if get(m, c, "executed") == 0), "",
                  "Executed only outside compiled code (union): " + ", ".join(
            f"`{c}`" for c in cells if get(m, c, "executed") > 0 and get(m, c, "executed_compiled") == 0), ""]

    with open(os.path.join(out, "report.json"), "w") as f:
        json.dump(report, f, indent=1)
    print("\n".join(lines))


if __name__ == "__main__":
    main()
