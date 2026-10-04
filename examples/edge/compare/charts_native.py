#!/usr/bin/env python3
"""Draws the native-backend charts (ADR 0085) from results/native/, std only.

    python3 examples/edge/compare/charts_native.py

Reads results/native/capacity.jsonl, sweep.jsonl and cpuio.txt; writes
native-capacity.svg, native-crunch-p99.svg and native-cpu-io.svg beside this
file. The same marks and colours as charts.py, with Cove's native backend as a
third series (#1baf7a), direct-labelled because its contrast is below 3:1.
"""

import json
import os
import re
import statistics

from charts import AXIS, COVE, GO, GRID, INK, INK2, SURFACE, Scale, Svg, fmt, legend, log_ticks

HERE = os.path.dirname(os.path.abspath(__file__))
NATIVE = os.path.join(HERE, "results", "native")
CN = "#1baf7a"
COLOUR = {"cove": COVE, "cove-native": CN, "go": GO}
NAME = {"cove": "Cove, VM", "cove-native": "Cove, native", "go": "Go"}
ORDER = ("cove", "cove-native", "go")


def rows(name):
    with open(os.path.join(NATIVE, name)) as f:
        return [json.loads(line) for line in f if line.strip()]


def capacity():
    cap = rows("capacity.jsonl")
    groups = [("crunch", 16), ("crunch", 64), ("cpu-io", 64), ("cpu-io", 256)]
    W, H = 760, 380
    left, right, top, bottom = 70, 20, 70, 60
    svg = Svg(W, H)
    svg.text(24, 30, "Capacity, closed loop: requests per second (median of 3)", 15, INK, weight="600")
    legend(svg, 24, 54, [(NAME[s], COLOUR[s], False) for s in ORDER])
    top_v = 4000
    y = Scale(0, top_v, H - bottom, top)
    for v in range(0, top_v + 1, 1000):
        svg.line(left, y(v), W - right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, f"{v:,}", 11, INK2, anchor="end")
    svg.line(left, y(0), W - right, y(0), AXIS)
    gw = (W - left - right) / len(groups)
    bw = 34
    for gi, (scenario, c) in enumerate(groups):
        x0 = left + gi * gw + (gw - 3 * bw - 2 * 2) / 2
        for si, server in enumerate(ORDER):
            vals = [r["throughput"] for r in cap if r["scenario"] == scenario and r["concurrency"] == c and r["server"] == server]
            m = statistics.median(vals)
            x = x0 + si * (bw + 2)
            svg.rect(x, y(m), bw, y(0) - y(m), COLOUR[server], title=f"{NAME[server]}, {scenario} c={c}: {m:,.0f} req/s", rx=4)
            svg.text(x + bw / 2, y(m) - 6, f"{m:,.0f}", 10, INK, anchor="middle")
        svg.text(left + gi * gw + gw / 2, H - bottom + 20, f"{scenario}, {c} in flight", 12, INK, anchor="middle")
    svg.save("native-capacity.svg")


def crunch_p99():
    sw = [r for r in rows("sweep.jsonl") if r["scenario"] == "crunch"]
    W, H = 760, 420
    left, right, top, bottom = 70, 110, 70, 50
    svg = Svg(W, H)
    svg.text(24, 30, "crunch (n = 20,000) at a fixed rate: p99 latency from the intended start", 15, INK, weight="600")
    legend(svg, 24, 54, [(NAME[s], COLOUR[s], False) for s in ORDER])
    x = Scale(0, 4000, left, W - right)
    lo, hi = 2, 2000
    y = Scale(lo, hi, H - bottom, top, log=True)
    for v in log_ticks(lo, hi):
        svg.line(left, y(v), W - right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, f"{fmt(v)} ms", 11, INK2, anchor="end")
    for v in range(0, 4001, 500):
        svg.text(x(v), H - bottom + 18, f"{v:,}", 11, INK2, anchor="middle")
    svg.text((left + W - right) / 2, H - 10, "offered requests per second", 11, INK2, anchor="middle")
    svg.line(left, H - bottom, W - right, H - bottom, AXIS)
    for server in ORDER:
        rates = sorted({r["offered"] for r in sw if r["server"] == server})
        pts = []
        for rate in rates:
            vals = [r["p99_ms"] for r in sw if r["server"] == server and r["offered"] == rate]
            pts.append((rate, max(lo, min(hi, statistics.median(vals)))))
        svg.polyline([(x(a), y(b)) for a, b in pts], COLOUR[server])
        for a, b in pts:
            svg.circle(x(a), y(b), 4, COLOUR[server], title=f"{NAME[server]} at {a:,} req/s: p99 {b:.1f} ms")
        a, b = pts[-1]
        svg.text(x(a) + 8, y(b) + 4, NAME[server], 11, INK, weight="600")
    svg.save("native-crunch-p99.svg")


def cpu_io():
    runs = {}
    cur = None
    with open(os.path.join(NATIVE, "cpuio.txt")) as f:
        for line in f:
            m = re.match(r"=== rep\d+-(\S+) backend=(\w+) slice=(\d+) rate=(\d+)", line)
            if m:
                cur = m.groups()
                continue
            m = re.match(r"\s+hello\s.*p99\s+([\d.]+) ms", line)
            if m and cur:
                runs.setdefault(cur, []).append(float(m.group(1)))
    bars = [
        ("588-intended", "vm", "0", "330", "VM, no slice", COVE),
        ("588-intended", "vm", "2", "330", "VM, 2 ms slice", COVE),
        ("588-intended", "native", "0", "330", "native, no slice", CN),
        ("588-intended", "native", "2", "330", "native, 2 ms slice", CN),
        ("x3", "native", "0", "990", "native, no slice", CN),
        ("x3", "native", "2", "990", "native, 2 ms slice", CN),
    ]
    W, H = 760, 400
    left, right, top, bottom = 70, 20, 70, 76
    svg = Svg(W, H)
    svg.text(24, 30, "hello's p99 under the cpu-io mix, four workers (median of 3 runs)", 15, INK, weight="600")
    legend(svg, 24, 54, [(NAME["cove"], COVE, False), (NAME["cove-native"], CN, False)])
    top_v = 120
    y = Scale(0, top_v, H - bottom, top)
    for v in range(0, top_v + 1, 20):
        svg.line(left, y(v), W - right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, f"{v} ms", 11, INK2, anchor="end")
    svg.line(left, y(0), W - right, y(0), AXIS)
    bw = (W - left - right) / len(bars)
    for i, (run, backend, slice_, rate, label, colour) in enumerate(bars):
        vals = runs[(run, backend, slice_, rate)]
        m = statistics.median(vals)
        bx = left + i * bw + 14
        svg.rect(bx, y(m), bw - 28, y(0) - y(m), colour, title=f"{label} at {rate} req/s: hello p99 {m:.1f} ms ({vals})", rx=4)
        svg.text(bx + (bw - 28) / 2, y(m) - 6, f"{m:.1f} ms", 11, INK, anchor="middle")
        svg.text(left + i * bw + bw / 2, H - bottom + 18, label, 11, INK, anchor="middle")
    for rate, first in (("330 req/s (the #588 rate)", 0), ("990 req/s", 4)):
        width = 4 if first == 0 else 2
        svg.text(left + first * bw + width * bw / 2, H - bottom + 40, rate, 11, INK2, anchor="middle")
        svg.line(left + first * bw + 10, H - bottom + 26, left + (first + width) * bw - 10, H - bottom + 26, AXIS)
    svg.save("native-cpu-io.svg")


if __name__ == "__main__":
    capacity()
    crunch_p99()
    cpu_io()
