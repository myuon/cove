#!/usr/bin/env python3
"""What a scripts/fuzz-budget.sh run cost and found: summary.json and summary.md.

    fuzz-budget-summary.py OUT --cove-fuzz BIN [--clean-cove-fuzz BIN]
                           --times START CORPUS EXPLORED --wall S [--known DIR...]

Each finding libFuzzer wrote is replayed once through `cove-fuzz` (the run's
own build, without a sanitizer) to read its message, and sorted:

- kind, from libFuzzer's file name: crash, timeout, oom, leak (and slow-unit,
  which is not a finding);
- a *signature*: the target, the kind, and the message's first line with
  every number made `#` — two findings with one signature are *alike*, which
  says they stopped at the same place with the same words, not that one
  defect caused both;
- known-like, if a reproducer under fuzz/regressions/<target>/known/ has the
  same signature when replayed the same way; new otherwise;
- with --clean-cove-fuzz, whether the clean build fails on it too (a mutant
  experiment's question: then it is not the mutant's).

A finding that does not fail when replayed outside libFuzzer (a timeout, an
out-of-memory stop, or a crash that needed ASan) keeps its kind and is
reported as not reproduced, never dropped.

Exit status: 1 if any finding is new, 2 if a slot failed to run, else 0.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

STATS_TARGETS = {"differential", "differential_source", "ir", "ir_mutate", "image"}
KINDS = ("crash", "timeout", "oom", "leak", "slow-unit")


def message(cove_fuzz, target, path, limit=60):
    """The first line of what replaying `path` through `target` fails with,
    or None if it does not fail."""
    if target in STATS_TARGETS:
        cmd = [cove_fuzz, "stats", target, str(path)]
    else:
        cmd = [cove_fuzz, target, str(path)]
    try:
        run = subprocess.run(cmd, capture_output=True, text=True, timeout=limit, errors="replace")
    except subprocess.TimeoutExpired:
        return "replay ran past %ds" % limit
    err = run.stderr
    for line in err.splitlines():
        if line.startswith("finding: "):
            # `finding: PATH: AT: MESSAGE`
            rest = line[len("finding: ") + len(str(path)) + 2:]
            return rest.strip()
    if run.returncode == 0:
        return None
    lines = err.splitlines()
    for i, line in enumerate(lines):
        m = re.search(r"panicked at ([^:]+:\d+):\d+:", line)
        if m:
            what = lines[i + 1] if i + 1 < len(lines) else ""
            return "%s: %s" % (m.group(1), what.strip())
    for line in lines:
        if re.search(r"fatal|Abort|SIGSEGV|stack overflow|error", line, re.I):
            return line.strip()[:200]
    return "exit status %d" % run.returncode


def signature(target, kind, text):
    if text is None:
        return "%s %s (not reproduced)" % (target, kind)
    at, sep, what = text.partition(": ")
    if not sep or not re.match(r"^[\w/.-]+\.rs:\d+$", at):
        at, what = "", text
    what = re.sub(r"\d+", "#", what)
    what = re.sub(r"\s+", " ", what).strip()[:160]
    return " ".join(x for x in (target, kind, at, what) if x)


def kind_of(name):
    for k in KINDS:
        if name.startswith(k + "-"):
            return k
    return "other"


def log_facts(text):
    """Times and counts libFuzzer's log says, for one slot."""
    facts = {}
    m = re.search(r"INFO: -fork=\d+: (\d+) seed inputs, starting to fuzz", text)
    if m:
        facts["seed_inputs"] = int(m.group(1))
    m = re.search(r"INFO: seed corpus: files: (\d+)", text)
    if m:
        facts.setdefault("seed_inputs", int(m.group(1)))
    status = re.findall(r"^#(\d+):? .*?cov: (\d+).*?(?:time: (\d+)s)?$", text, re.M)
    if status:
        facts["executions"] = int(status[-1][0])
        facts["cov"] = int(status[-1][1])
    m = re.findall(r"oom/timeout/crash: (\d+)/(\d+)/(\d+)", text)
    if m:
        facts["oom_timeout_crash"] = [int(x) for x in m[-1]]
    m = re.search(r"INFO: exiting: (-?\d+) time: (\d+)s", text)
    if m:
        facts["exit"] = int(m.group(1))
        facts["libfuzzer_seconds"] = int(m.group(2))
    facts["replayed"] = bool(re.search(r"starting to fuzz in|INITED", text))
    return facts


def read_result(path):
    out = {}
    for line in path.read_text().splitlines():
        k, _, v = line.partition(": ")
        out[k] = v
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--cove-fuzz", required=True)
    ap.add_argument("--clean-cove-fuzz")
    ap.add_argument("--times", nargs=3, type=float, required=True)
    ap.add_argument("--wall", type=float, required=True)
    ap.add_argument("--known", nargs="*", default=[])
    args = ap.parse_args()
    began = time.time()
    out = Path(args.out)
    t_start, t_corpus, t_explored = args.times

    known = {}
    for d in args.known:
        target = Path(d).parent.name
        for f in sorted(Path(d).iterdir()):
            if f.is_file() and not f.name.endswith(".md"):
                sig = signature(target, "crash", message(args.cove_fuzz, target, f))
                known.setdefault(sig, []).append(str(f))

    slots, findings, failures = [], [], []
    for d in sorted((out / "slots").iterdir()):
        r = read_result(d / "result.txt")
        log = (d / "fuzz.log").read_text(errors="replace") if (d / "fuzz.log").exists() else ""
        facts = log_facts(log)
        began_s, ended_s = float(r["began"]), float(r["ended"])
        slot = {
            "slot": d.name,
            "worker": int(r["worker"]),
            "target": r["target"],
            "sanitizer": r["sanitizer"],
            "given_seconds": int(r["given seconds"]),
            "wall_seconds": round(ended_s - began_s, 1),
            "began": round(began_s - t_start, 1),
            "ended": round(ended_s - t_start, 1),
            "status": int(r["status"]),
            "killed": r["killed at the deadline"] == "1",
            **facts,
        }
        if "replayed" in r:
            slot["replay_seconds"] = round(float(r["replayed"]) - began_s, 1)
            slot["replayed"] = True
        if "replay_seconds" in slot:
            slot["explore_seconds"] = round(max(0.0, slot["wall_seconds"] - slot["replay_seconds"]), 1)
        stats = d / "session"
        if any(stats.glob("*.json")):
            merged = subprocess.run(
                [args.cove_fuzz, "stats", "merge", str(stats)],
                capture_output=True, text=True,
            )
            if merged.returncode == 0:
                (d / "session.json").write_text(merged.stdout)
                (d / "session.txt").write_text(merged.stderr)
                try:
                    s = json.loads(merged.stdout)
                    slot["counted"] = {
                        "inputs": s.get("inputs", 0),
                        "full_success": s.get("comparisons", {}).get("full_success", 0),
                        "full_success_with_native": s.get("comparisons", {}).get("full_success_with_native", 0),
                        "native_ran": s.get("programs", {}).get("native_ran", 0),
                        "gc_while_compiled": s.get("programs", {}).get("gc_while_compiled", 0),
                        "vm_gc_collected": s.get("programs", {}).get("vm_gc_collected", 0),
                        "mismatches": s.get("mismatches", 0),
                    }
                except (ValueError, AttributeError):
                    pass
        # A slot that ended with no finding to show for a failing status, and
        # was not stopped by us, did not run: the harness, the binary, or the
        # machine.
        artifacts = sorted((d / "artifacts").iterdir()) if (d / "artifacts").exists() else []
        if slot["status"] not in (0,) and not slot["killed"] and not artifacts:
            slot["failed_to_run"] = True
            failures.append({"slot": d.name, "status": slot["status"],
                             "tail": log.strip().splitlines()[-3:] if log.strip() else []})
        if not slot["replayed"]:
            slot["replay_unfinished"] = True
        for a in artifacts:
            kind = kind_of(a.name)
            if kind == "slow-unit":
                continue
            text = message(args.cove_fuzz, r["target"], a) if kind in ("crash", "leak", "other") else None
            sig = signature(r["target"], kind, text)
            f = {
                "slot": d.name,
                "target": r["target"],
                "kind": kind,
                "file": str(a),
                "found_after": round(a.stat().st_mtime - began_s, 1),
                "message": text,
                "signature": sig,
                "known_like": known.get(sig, []),
            }
            if args.clean_cove_fuzz and kind in ("crash", "leak", "other"):
                f["clean_fails_too"] = message(args.clean_cove_fuzz, r["target"], a) is not None
            findings.append(f)
        slots.append(slot)

    groups = {}
    for f in findings:
        g = groups.setdefault(f["signature"], {
            "signature": f["signature"], "kind": f["kind"], "target": f["target"],
            "known_like": f["known_like"], "count": 0, "files": [],
            "first_after": None, "clean_fails_too": f.get("clean_fails_too"),
        })
        g["count"] += 1
        g["files"].append(f["file"])
        when = f["found_after"] + next(s["began"] for s in slots if s["slot"] == f["slot"])
        g["first_after"] = when if g["first_after"] is None else min(g["first_after"], when)

    ended = time.time()
    worker_seconds = sum(s["wall_seconds"] for s in slots)
    explore_wall = t_explored - t_corpus
    summary = {
        "wall_budget": args.wall,
        "phases": {
            "corpus_seconds": round(t_corpus - t_start, 1),
            "explore_wall_seconds": round(explore_wall, 1),
            "summary_seconds": round(ended - began, 1),
            "total_seconds": round(ended - t_start, 1),
        },
        "worker_seconds": round(worker_seconds, 1),
        "parallelism": round(worker_seconds / explore_wall, 2) if explore_wall > 0 else None,
        "replay_seconds_total": sum(s.get("replay_seconds", 0) for s in slots),
        "slots": slots,
        "groups": sorted(groups.values(), key=lambda g: g["first_after"] or 0),
        "failures": failures,
    }
    new = [g for g in groups.values() if not g["known_like"]]
    summary["verdict"] = (
        "failed to run" if failures else "new finding" if new else
        "known-like findings only" if groups else "nothing found")
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

    md = ["# fuzz budget: %s" % out.name, ""]
    p = summary["phases"]
    md.append("**%s.** Wall %.0fs of a %.0fs budget: corpus %.1fs, exploring %.1fs (every slot's replay included), summary %.1fs. "
              "Worker time %.0fs, parallelism %s." % (
                  summary["verdict"], p["total_seconds"], args.wall, p["corpus_seconds"],
                  p["explore_wall_seconds"], p["summary_seconds"], worker_seconds, summary["parallelism"]))
    md.append("")
    md.append("| slot | sanitizer | given | wall | start-corpus replay | explored | executions | stopped by us | findings |")
    md.append("|---|---|---:|---:|---:|---:|---:|---|---:|")
    for s in slots:
        n = sum(1 for f in findings if f["slot"] == s["slot"])
        replay = "%ss of %s inputs" % (s["replay_seconds"], s.get("seed_inputs", "?")) if "replay_seconds" in s else "unfinished"
        md.append("| %s | %s | %ds | %.0fs | %s | %ss | %s | %s | %d |" % (
            s["slot"], s["sanitizer"], s["given_seconds"], s["wall_seconds"], replay,
            s.get("explore_seconds", 0), s.get("executions", "?"), "yes" if s["killed"] else "no", n))
    if groups:
        md += ["", "| finding (signature: alike, not proven one cause) | kind | count | first after | known-like | clean build fails too |", "|---|---|---:|---:|---|---|"]
        for g in summary["groups"]:
            md.append("| `%s` | %s | %d | %.0fs | %s | %s |" % (
                g["signature"].replace("|", "\\|")[:200], g["kind"], g["count"], g["first_after"] or 0,
                ", ".join(os.path.basename(k) for k in g["known_like"]) or "**new**",
                "" if g["clean_fails_too"] is None else ("yes" if g["clean_fails_too"] else "no")))
    for fl in failures:
        md.append("")
        md.append("**%s failed to run** (status %d): %s" % (fl["slot"], fl["status"], " / ".join(fl["tail"])))
    (out / "summary.md").write_text("\n".join(md) + "\n")
    print("\n".join(md))
    if failures:
        return 2
    if new:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
