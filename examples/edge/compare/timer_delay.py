#!/usr/bin/env python3
"""Where a simulated upstream answer's delay goes, from an edge timeline.

    python3 examples/edge/compare/timer_delay.py TIMELINE.json [--label X] [--jsonl OUT]
    python3 examples/edge/compare/timer_delay.py --summarise results/timer.jsonl

For every `upstream.get` a run parked on, the timeline (`cove-edge
--timeline`) has the park, with the latency the call was given — so the
instant its answer was *due* — then `answer_ready` when the parking lot
woke, made the answer and queued the resume, then `resume` when a worker
took it up. This prints the distribution of

    lot late    answer_ready - due     the timer's wake, and the lot's own work
    to worker   resume - answer_ready  the run queue, and a worker picking it up
    total       resume - due

in milliseconds, for the parks the timer answered (not a deadline, not a
fetch). `answer_ready` is noted after the lot has woken and built the
answer, just before it pushes the batch onto the run queue, so "lot late"
includes the lot's handling of the earlier entries of the same batch.
"""

import argparse
import json


def pct(xs, p):
    if not xs:
        return float("nan")
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def summarise(path):
    """`--summarise results/timer.jsonl`: per binary and rate, the median over
    rounds of each stage's percentiles, and the range of the p50s."""
    groups = {}
    with open(path) as f:
        for line in f:
            row = json.loads(line)
            words = dict(w.split("=", 1) for w in row["label"].split()[1:])
            key = (row["label"].split()[0], int(words["rate"]))
            groups.setdefault(key, []).append((row, float(words["load"])))
    print("| binary | aggregate req/s | rounds | load | lot late p50 / p99 / max | to worker p50 / p99 / max | total p50 / p99 / max |")
    print("| --- | ---: | ---: | --- | ---: | ---: | ---: |")
    med = lambda xs: sorted(xs)[len(xs) // 2]
    for (name, rate), rows in sorted(groups.items(), key=lambda kv: (kv[0][1], kv[0][0])):
        loads = [l for _, l in rows]
        cells = []
        for stage in ["lot_late", "to_worker", "total"]:
            p50 = med([r[stage]["p50"] for r, _ in rows])
            p99 = med([r[stage]["p99"] for r, _ in rows])
            mx = med([r[stage]["max"] for r, _ in rows])
            cells.append(f"{p50:.3f} / {p99:.3f} / {mx:.1f}")
        print(f"| {name} | {rate:,} | {len(rows)} | {min(loads):.1f}–{max(loads):.1f} | " + " | ".join(cells) + " |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("timeline", nargs="?")
    ap.add_argument("--summarise", metavar="JSONL")
    ap.add_argument("--label", default="")
    ap.add_argument("--jsonl", help="append the summary as one JSON row")
    args = ap.parse_args()
    if args.summarise:
        summarise(args.summarise)
        return
    with open(args.timeline) as f:
        events = json.load(f)["events"]
    # A request parks several times in turn, so pair each park with the
    # answer_ready and resume that follow it for the same request.
    pending = {}
    rows = []
    for e in sorted(events, key=lambda e: e["t"]):
        req = e["req"]
        if e["ev"] == "park" and e.get("op") == "upstream.get" and "due_ms" in e:
            pending[req] = {"due": e["t"] / 1e3 + e["due_ms"]}
        elif e["ev"] == "answer_ready" and req in pending:
            if e["by"] == "timer":
                pending[req]["ready"] = e["t"] / 1e3
            else:
                pending.pop(req)
        elif e["ev"] == "resume" and req in pending and "ready" in pending[req]:
            p = pending.pop(req)
            rows.append((p["ready"] - p["due"], e["t"] / 1e3 - p["ready"], e["t"] / 1e3 - p["due"]))
    names = ["lot late", "to worker", "total"]
    print(f"# {args.label} {len(rows)} timer-answered parks; ms")
    print(f"{'stage':<10} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>8} {'mean':>8}")
    summary = {"label": args.label, "parks": len(rows)}
    for i, name in enumerate(names):
        xs = [r[i] for r in rows]
        mean = sum(xs) / len(xs) if xs else float("nan")
        print(f"{name:<10} {pct(xs, 50):8.3f} {pct(xs, 90):8.3f} {pct(xs, 99):8.3f} {max(xs) if xs else 0:8.3f} {mean:8.3f}")
        summary[name.replace(" ", "_")] = {
            "p50": pct(xs, 50),
            "p90": pct(xs, 90),
            "p99": pct(xs, 99),
            "max": max(xs) if xs else None,
            "mean": mean,
        }
    if args.jsonl:
        with open(args.jsonl, "a") as f:
            f.write(json.dumps(summary, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
