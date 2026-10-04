#!/usr/bin/env python3
"""Draws the comparison's charts from results/ as SVG, std only.

    python3 examples/edge/compare/charts.py

Reads results/stage1-cove.txt, results/stage1-go.txt, results/sweep.jsonl,
results/capacity.jsonl and results/waiting.jsonl; writes stage1.svg,
sweep-p99.svg, sweep-p50.svg and waiting.svg beside this file.

One axis per chart, Cove #2a78d6 and Go #eb6834 always, a legend and a
direct label at each line's end, text in neutral ink.
"""

import json
import math
import os
import re
import statistics

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")

COVE = "#2a78d6"
GO = "#eb6834"
INK = "#0b0b0b"
INK2 = "#52514e"
GRID = "#e6e5e1"
AXIS = "#b9b8b3"
SURFACE = "#fcfcfb"
FONT = "font-family=\"-apple-system, 'Helvetica Neue', Arial, sans-serif\""
COLOUR = {"cove": COVE, "go": GO}
NAME = {"cove": "Cove (edge)", "go": "Go (net/http)"}


def esc(text):
    return str(text).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Svg:
    def __init__(self, width, height):
        self.width, self.height = width, height
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" {FONT}>',
            f'<rect width="{width}" height="{height}" fill="{SURFACE}"/>',
        ]

    def text(self, x, y, text, size=12, fill=INK, anchor="start", weight="normal"):
        self.parts.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" text-anchor="{anchor}" font-weight="{weight}">{esc(text)}</text>'
        )

    def line(self, x1, y1, x2, y2, stroke, width=1, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.parts.append(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{stroke}" stroke-width="{width}"{d}/>'
        )

    def polyline(self, points, stroke, width=2):
        pts = " ".join(f"{x:.1f},{y:.1f}" for x, y in points)
        self.parts.append(
            f'<polyline points="{pts}" fill="none" stroke="{stroke}" stroke-width="{width}" stroke-linejoin="round" stroke-linecap="round"/>'
        )

    def circle(self, x, y, r, fill, stroke=SURFACE, width=2, title=None):
        t = f"<title>{esc(title)}</title>" if title else ""
        self.parts.append(
            f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{fill}" stroke="{stroke}" stroke-width="{width}">{t}</circle>'
        )

    def rect(self, x, y, w, h, fill, title=None, rx=0):
        t = f"<title>{esc(title)}</title>" if title else ""
        self.parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{max(0, w):.1f}" height="{max(0, h):.1f}" rx="{rx}" fill="{fill}">{t}</rect>'
        )

    def save(self, name):
        self.parts.append("</svg>")
        with open(os.path.join(HERE, name), "w") as f:
            f.write("\n".join(self.parts) + "\n")
        print(f"wrote {name}")


class Scale:
    def __init__(self, lo, hi, a, b, log=False):
        self.lo, self.hi, self.a, self.b, self.log = lo, hi, a, b, log

    def __call__(self, v):
        if self.log:
            t = (math.log10(v) - math.log10(self.lo)) / (math.log10(self.hi) - math.log10(self.lo))
        else:
            t = (v - self.lo) / (self.hi - self.lo)
        return self.a + t * (self.b - self.a)


def log_ticks(lo, hi):
    ticks = []
    e = math.floor(math.log10(lo))
    while 10**e <= hi * 1.0001:
        for m in (1, 2, 5):
            v = m * 10**e
            if lo * 0.9999 <= v <= hi * 1.0001:
                ticks.append(v)
        e += 1
    return ticks


def fmt(v):
    if v >= 1000:
        return f"{v / 1000:g}k"
    if v >= 1:
        return f"{v:g}"
    return f"{v:g}"


def legend(svg, x, y, entries):
    """entries: (label, colour, hollow)"""
    for label, colour, hollow in entries:
        svg.line(x, y - 4, x + 18, y - 4, colour, 2)
        svg.circle(x + 9, y - 4, 4.5, SURFACE if hollow else colour, colour if hollow else SURFACE, 2)
        svg.text(x + 24, y, label, 12, INK)
        x += 34 + 7.0 * len(label)


# ------------------------------------------------------------------ data


def rows(name):
    path = os.path.join(RESULTS, name)
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def metric(row, which, tenant=None):
    if tenant:
        t = row.get("tenants", {}).get(tenant)
        return t and t[f"{which}_ms"]
    return row[f"{which}_ms"]


# What each sweep panel plots: the scenario's own latency, except under the
# mix, where the interactive tenant's is the one a scheduler is judged by.
PANELS = [
    ("hello", None, "hello alone"),
    ("crunch", None, "crunch n=20000 alone"),
    ("aggregate", None, "aggregate alone (3 x 20–100 ms upstream)"),
    ("cpu-io", "hello", "cpu-io mix: hello's latency"),
]


def sweep_series(scenario, server, which, tenant):
    by_rate = {}
    for row in rows("sweep.jsonl"):
        if row["scenario"] == scenario and row["server"] == server:
            v = metric(row, which, tenant)
            if v:
                by_rate.setdefault(row["offered"], []).append(v)
    return [(rate, statistics.median(vs), min(vs), max(vs)) for rate, vs in sorted(by_rate.items())]


def capacity_of(scenario, server):
    # The best concurrency's median, as sweep.py bounds its sweep by.
    by = {}
    for r in rows("capacity.jsonl"):
        if r["scenario"] == scenario and r["server"] == server:
            by.setdefault(r["concurrency"], []).append(r["throughput"])
    return max((statistics.median(v) for v in by.values()), default=None)


def sweep_chart(which, name):
    pw, ph = 330, 250
    left, top = 64, 96
    gap = 56
    W = left + 4 * pw + 3 * gap + 40
    H = top + ph + 70
    svg = Svg(W, H)
    svg.text(24, 28, f"{which} latency against offered rate — Cove edge server vs Go net/http, 4 workers / GOMAXPROCS=4", 16, INK, weight="600")
    svg.text(24, 48, "Open loop: latency from each request's intended start (i / rate). Median of 3 runs; whiskers min..max. Both axes log. Dashed line: the server's unthrottled capacity.", 12, INK2)
    svg.text(24, 64, "The load generator shares the machine and polls every 200 µs: below about 2 ms (p50) and 3 ms (p99) it measures itself, not the servers.", 12, INK2)
    legend(svg, W - 300, 28, [(NAME["cove"], COVE, False), (NAME["go"], GO, False)])
    for i, (scenario, tenant, title) in enumerate(PANELS):
        x0 = left + i * (pw + gap)
        series = {s: sweep_series(scenario, s, which, tenant) for s in ("cove", "go")}
        allv = [p for s in series.values() for p in s]
        if not allv:
            continue
        rates = [p[0] for p in allv]
        xlo, xhi = min(rates) / 1.3, max(rates) * 1.3
        ylo = min(p[2] for p in allv) / 1.5
        yhi = max(p[3] for p in allv) * 1.5
        ylo = 10 ** math.floor(math.log10(ylo))
        yhi = 10 ** math.ceil(math.log10(yhi))
        sx = Scale(xlo, xhi, x0, x0 + pw, log=True)
        sy = Scale(ylo, yhi, top + ph, top, log=True)
        svg.text(x0, top - 12, title, 13, INK, weight="600")
        for t in log_ticks(ylo, yhi):
            y = sy(t)
            svg.line(x0, y, x0 + pw, y, GRID)
            if i == 0 or True:
                svg.text(x0 - 6, y + 4, f"{fmt(t)}", 10, INK2, "end")
        for t in log_ticks(xlo, xhi):
            x = sx(t)
            svg.line(x, top, x, top + ph, GRID)
            svg.text(x, top + ph + 16, fmt(t), 10, INK2, "middle")
        svg.line(x0, top + ph, x0 + pw, top + ph, AXIS)
        svg.text(x0 + pw / 2, top + ph + 34, "offered rate (req/s, log)", 11, INK2, "middle")
        if i == 0:
            svg.text(18, top + ph / 2, f"{which} ms (log)", 11, INK2, "middle")
            svg.parts[-1] = svg.parts[-1].replace("<text ", f'<text transform="rotate(-90 18 {top + ph / 2})" ', 1)
        for server in ("cove", "go"):
            cap = capacity_of(scenario, server)
            if cap and xlo < cap < xhi:
                x = sx(cap)
                svg.line(x, top, x, top + ph, COLOUR[server], 1, "4 3")
        ends = {}
        for server in ("cove", "go"):
            pts = series[server]
            if not pts:
                continue
            c = COLOUR[server]
            for rate, med, lo, hi in pts:
                svg.line(sx(rate), sy(lo), sx(rate), sy(hi), c, 1)
            svg.polyline([(sx(r), sy(m)) for r, m, _, _ in pts], c, 2)
            for rate, med, lo, hi in pts:
                svg.circle(sx(rate), sy(med), 4.5, c, title=f"{NAME[server]} {rate} req/s: {which} {med:.2f} ms ({lo:.2f}..{hi:.2f})")
            r, m, _, _ = pts[-1]
            ends[server] = [min(sx(r) + 8, x0 + pw + 8), sy(m) + 4]
        # Direct labels at the line ends, pushed apart when they would touch.
        if len(ends) == 2 and abs(ends["cove"][1] - ends["go"][1]) < 14 and abs(ends["cove"][0] - ends["go"][0]) < 40:
            upper, lower = sorted(ends, key=lambda k: ends[k][1])
            mid = (ends[upper][1] + ends[lower][1]) / 2
            ends[upper][1], ends[lower][1] = mid - 7, mid + 7
        for server, (lx, ly) in ends.items():
            svg.text(lx, ly, "Cove" if server == "cove" else "Go", 11, INK, weight="600")
    svg.save(name)


# ---------------------------------------------------------------- stage 1


def stage1():
    times = {}
    for name in ("stage1-cove.txt", "stage1-go.txt"):
        path = os.path.join(RESULTS, name)
        if not os.path.exists(path):
            return
        for line in open(path):
            m = re.match(r"^(\S+)\s+(crunch n=\d+|hello name=Cove)\s+(\d+)\s", line)
            if m:
                times.setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
    cases = ["crunch n=2000", "crunch n=20000", "crunch n=150000", "hello name=Cove"]
    W, H = 920, 120 + 56 * len(cases)
    left, right, top = 150, 720, 92
    svg = Svg(W, H)
    svg.text(24, 28, "Stage 1: time per call of the same handler, in-process (median of 7 batches)", 16, INK, weight="600")
    svg.text(24, 48, "Log scale. Cove VM = a fresh isolate per call, as the server runs it; Cove native = the native tier on a reused Vm.", 12, INK2)
    legend(svg, 24, 72, [("Cove VM (edge path)", COVE, False), ("Cove native tier", COVE, True), ("Go", GO, False)])
    allv = [v for c in cases for v in times.get(c, {}).values()]
    lo = 10 ** math.floor(math.log10(min(allv)))
    hi = 10 ** math.ceil(math.log10(max(allv)))
    sx = Scale(lo, hi, left, right, log=True)
    bottom = top + 56 * len(cases)
    for t in log_ticks(lo, hi):
        if str(t)[0] != "1":
            continue
        x = sx(t)
        svg.line(x, top - 6, x, bottom - 20, GRID)
        label = f"{t / 1e6:g} ms" if t >= 1e6 else (f"{t / 1e3:g} µs" if t >= 1e3 else f"{t:g} ns")
        svg.text(x, bottom - 4, label, 10, INK2, "middle")
    svg.text(right + 12, top - 10, "Cove / Go", 11, INK2)
    for i, case in enumerate(cases):
        y = top + 18 + 56 * i
        t = times.get(case, {})
        svg.text(left - 12, y + 4, case, 12, INK, "end")
        svg.line(left, y, right, y, GRID)
        go = t.get("go")
        for mode, colour, hollow in (("edge", COVE, False), ("native", COVE, True), ("go", GO, False)):
            if mode in t:
                svg.circle(sx(t[mode]), y, 5, SURFACE if hollow else colour, colour if hollow else SURFACE, 2,
                           title=f"{case} {mode}: {t[mode]:.0f} ns")
        if go:
            vm = t.get("edge", 0) / go
            nat = t.get("native", 0) / go
            svg.text(right + 12, y + 4, f"VM {vm:.1f}x · native {nat:.1f}x", 12, INK)
    svg.save("stage1.svg")


# ----------------------------------------------------------------- waiting


def waiting():
    data = rows("waiting.jsonl")
    if not data:
        return
    ns = sorted({r["inflight"] for r in data})
    measures = [
        ("RSS growth per waiting request (KiB)", lambda r: (r["rss_peak_kib"] - r["rss_before_kib"]) / r["inflight"]),
        ("server CPU per request (µs, user+sys)", lambda r: 1e6 * r["server_cpu_s"] / r["inflight"]),
    ]
    pw, ph, left, top, gap = 380, 230, 70, 112, 90
    W = left + 2 * pw + gap + 30
    H = top + ph + 60
    svg = Svg(W, H)
    svg.text(24, 28, "Waiting requests: aggregate with a 1 s upstream (3 s per request), all in flight at once", 16, INK, weight="600")
    svg.text(24, 48, "Median of 3 runs; whiskers min..max. RSS sampled every 20 ms with ps; growth = peak − RSS after warm-up.", 12, INK2)
    legend(svg, 24, 72, [(NAME["cove"], COVE, False), (NAME["go"], GO, False)])
    for p, (title, f) in enumerate(measures):
        x0 = left + p * (pw + gap)
        vals = {}
        for r in data:
            vals.setdefault((r["server"], r["inflight"]), []).append(f(r))
        hi = max(max(v) for v in vals.values()) * 1.15
        base = 10 ** math.floor(math.log10(hi))
        step = base
        for m in (0.1, 0.2, 0.5, 1, 2, 5):
            if hi / (m * base) <= 7:
                step = m * base
                break
        sy = Scale(0, hi, top + ph, top)
        svg.text(x0, top - 10, title, 13, INK, weight="600")
        t = 0
        while t <= hi:
            svg.line(x0, sy(t), x0 + pw, sy(t), GRID)
            svg.text(x0 - 6, sy(t) + 4, f"{t:g}", 10, INK2, "end")
            t += step
        svg.line(x0, top + ph, x0 + pw, top + ph, AXIS)
        group = pw / len(ns)
        bw = 34
        for gi, n in enumerate(ns):
            cx = x0 + group * (gi + 0.5)
            svg.text(cx, top + ph + 18, f"{n:,} in flight", 11, INK2, "middle")
            for si, server in enumerate(("cove", "go")):
                v = vals.get((server, n))
                if not v:
                    continue
                med = statistics.median(v)
                x = cx + (si - 1) * (bw + 2) + 1
                svg.rect(x, sy(med), bw, sy(0) - sy(med), COLOUR[server], f"{NAME[server]} {n}: {med:.1f}")
                svg.line(x + bw / 2, sy(min(v)), x + bw / 2, sy(max(v)), INK2, 1)
                svg.text(x + bw / 2, sy(max(v)) - 5, f"{med:.1f}" if p == 0 else f"{med:.0f}", 11, INK, "middle")
    svg.save("waiting.svg")


def med(vs):
    return statistics.median(vs) if vs else float("nan")


def spread(vs, f="{:.1f}"):
    if not vs:
        return "–"
    return f.format(med(vs)) + (f" ({f.format(min(vs))}–{f.format(max(vs))})" if len(vs) > 1 else "")


def tables():
    """The README's tables, as Markdown, from the same results."""
    out = []
    cap = rows("capacity.jsonl")
    out.append("### capacity\n\n| scenario | in flight | Cove req/s | Go req/s | Go / Cove | Cove CPU µs/req | Go CPU µs/req | Cove p50 / p99 ms | Go p50 / p99 ms |")
    out.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    keys = sorted({(r["scenario"], r["concurrency"]) for r in cap}, key=lambda k: (list(SCEN).index(k[0]), k[1]))
    for scenario, c in keys:
        sel = {s: [r for r in cap if r["scenario"] == scenario and r["concurrency"] == c and r["server"] == s] for s in ("cove", "go")}
        tp = {s: [r["throughput"] for r in sel[s]] for s in sel}
        cpu = {s: [1e6 * r["server_cpu_s"] / r["answered"] for r in sel[s]] for s in sel}
        lat = {s: f"{med([r['p50_ms'] for r in sel[s]]):.1f} / {med([r['p99_ms'] for r in sel[s]]):.1f}" for s in sel}
        out.append(f"| {scenario} | {c:,} | {spread(tp['cove'], '{:,.0f}')} | {spread(tp['go'], '{:,.0f}')} | "
                   f"{med(tp['go']) / med(tp['cove']):.2f} | {med(cpu['cove']):.0f} | {med(cpu['go']):.0f} | {lat['cove']} | {lat['go']} |")
    sw = rows("sweep.jsonl")
    for scenario, tenant, title in PANELS:
        out.append(f"\n### sweep: {title}\n")
        out.append("| offered req/s | Cove p50 | Cove p99 | Go p50 | Go p99 | p99 Cove / Go | Cove p99 from send | Go p99 from send | Cove CPU µs/req | Go CPU µs/req |")
        out.append("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
        for rate in sorted({r["offered"] for r in sw if r["scenario"] == scenario}):
            cells = []
            vals = {}
            for s in ("cove", "go"):
                sel = [r for r in sw if r["scenario"] == scenario and r["server"] == s and r["offered"] == rate]
                src = [(r.get("tenants", {}).get(tenant) if tenant else r) for r in sel]
                src = [x for x in src if x]
                vals[s] = dict(
                    p50=[x["p50_ms"] for x in src], p99=[x["p99_ms"] for x in src],
                    p99s=[x["p99_sent_ms"] for x in src],
                    cpu=[1e6 * r["server_cpu_s"] / max(1, r["answered"]) for r in sel],
                )
            def cell(s, k, f="{:.1f}"):
                return spread(vals[s][k], f) if vals[s][k] else "–"
            ratio = (f"{med(vals['cove']['p99']) / med(vals['go']['p99']):.1f}x"
                     if vals["cove"]["p99"] and vals["go"]["p99"] else "–")
            out.append(f"| {rate:,} | {cell('cove','p50')} | {cell('cove','p99')} | {cell('go','p50')} | {cell('go','p99')} | {ratio} | "
                       f"{cell('cove','p99s')} | {cell('go','p99s')} | {cell('cove','cpu','{:.0f}')} | {cell('go','cpu','{:.0f}')} |")
    mix = [r for r in sw if r["scenario"] == "cpu-io"]
    if mix:
        out.append("\n### sweep: cpu-io mix, every tenant (p50 / p99 ms, median of 3)\n")
        tenants = sorted({t for r in mix for t in r.get("tenants", {})})
        out.append("| offered | server | " + " | ".join(tenants) + " |")
        out.append("| ---: | --- | " + " | ".join("---:" for _ in tenants) + " |")
        for rate in sorted({r["offered"] for r in mix}):
            for s in ("cove", "go"):
                sel = [r for r in mix if r["server"] == s and r["offered"] == rate]
                if not sel:
                    continue
                cells = []
                for t in tenants:
                    xs = [r["tenants"][t] for r in sel if t in r.get("tenants", {})]
                    cells.append(f"{med([x['p50_ms'] for x in xs]):.1f} / {med([x['p99_ms'] for x in xs]):.1f}" if xs else "–")
                out.append(f"| {rate:,} | {s} | " + " | ".join(cells) + " |")
    con = rows("connections.jsonl")
    if con:
        out.append("\n### connections: hello at a fixed rate over N kept-alive connections\n")
        out.append("| connections | Cove p50 / p99 ms | Go p50 / p99 ms | Cove CPU µs/req | Go CPU µs/req |")
        out.append("| ---: | ---: | ---: | ---: | ---: |")
        for pool in sorted({r["pool"] for r in con}):
            c = {s: [r for r in con if r["pool"] == pool and r["server"] == s] for s in ("cove", "go")}
            f = lambda s: f"{med([r['p50_ms'] for r in c[s]]):.2f} / {med([r['p99_ms'] for r in c[s]]):.2f}"
            g = lambda s: f"{med([1e6 * r['server_cpu_s'] / r['answered'] for r in c[s]]):.0f}"
            out.append(f"| {pool:,} | {f('cove')} | {f('go')} | {g('cove')} | {g('go')} |")
    wt = rows("waiting.jsonl")
    if wt:
        out.append("\n### waiting: aggregate, 1 s upstream, N in flight at once\n")
        out.append("| in flight | server | answered 200 | RSS after warm-up MiB | peak RSS MiB | KiB per waiting request | CPU µs per request | p50 ms |")
        out.append("| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
        for n in sorted({r["inflight"] for r in wt}):
            for s in ("cove", "go"):
                sel = [r for r in wt if r["inflight"] == n and r["server"] == s]
                if not sel:
                    continue
                out.append(f"| {n:,} | {s} | {med([r['ok'] for r in sel]):.0f} | {med([r['rss_before_kib'] / 1024 for r in sel]):.1f} | "
                           f"{med([r['rss_peak_kib'] / 1024 for r in sel]):.1f} | "
                           f"{spread([(r['rss_peak_kib'] - r['rss_before_kib']) / n for r in sel])} | "
                           f"{spread([1e6 * r['server_cpu_s'] / n for r in sel], '{:.0f}')} | {med([r['p50_ms'] for r in sel]):.0f} |")
    print("\n".join(out))


SCEN = ["hello", "crunch", "aggregate", "cpu-io"]


if __name__ == "__main__":
    import sys
    if "--tables" in sys.argv:
        tables()
        raise SystemExit
    stage1()
    sweep_chart("p99", "sweep-p99.svg")
    sweep_chart("p50", "sweep-p50.svg")
    waiting()
    if "--png" in sys.argv:
        # PNGs for attaching to a pull request, rendered by headless Chrome
        # at 2x: `charts.py --png DIR`.
        import subprocess
        out = sys.argv[sys.argv.index("--png") + 1]
        os.makedirs(out, exist_ok=True)
        chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        for name in ("stage1", "sweep-p99", "sweep-p50", "waiting"):
            svg = os.path.join(HERE, f"{name}.svg")
            m = re.search(r'width="(\d+)" height="(\d+)"', open(svg).read())
            subprocess.run([chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars",
                            "--force-device-scale-factor=2", f"--window-size={m.group(1)},{m.group(2)}",
                            f"--screenshot={os.path.join(out, name + '.png')}", f"file://{svg}"],
                           check=True, capture_output=True)
            print(f"rendered {out}/{name}.png")
