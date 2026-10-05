import re, sys, statistics as st
from collections import defaultdict
text = open(sys.argv[1]).read().splitlines()
cur = None
data = defaultdict(list)   # (bench, side) -> [value]
counts = defaultdict(set)
loads = defaultdict(list)
for line in text:
    m = re.match(r"round=(\d+) side=(\w+) bench=(\S+) load=\{ ([\d.]+)", line)
    if m:
        cur = (m.group(3), m.group(2)); loads[m.group(2)].append(float(m.group(4))); continue
    m = re.match(r"backend: (\w+) .*execute=([\d.]+)(m?s) instructions=(\d+)", line)
    if m and cur:
        v = float(m.group(2)) * (1 if m.group(3) == 's' else 1e-3)
        data[(cur[0], cur[1])].append(v * 1000); counts[(cur[0], cur[1])].add(m.group(4)); continue
    m = re.match(r"^(edge-n\+y|edge-n|edge\+y|edge|vm|native)\s+(crunch n=\d+|hello name=Cove)\s+(\d+)", line)
    if m and cur:
        data[(m.group(1) + " " + m.group(2), cur[1])].append(int(m.group(3)) / 1e3)
keys = sorted({k for k, _ in data})
print("| row | base | head | Δ | base rounds | head rounds |")
print("|---|---:|---:|---:|---|---|")
for k in keys:
    b, h = data.get((k, 'base'), []), data.get((k, 'head'), [])
    if not b or not h:
        print(f"| {k} | – | {st.median(h):.2f} | new | | {', '.join(f'{x:.1f}' for x in h)} |"); continue
    mb, mh = st.median(b), st.median(h)
    print(f"| {k} | {mb:.2f} | {mh:.2f} | {100*(mh-mb)/mb:+.1f}% | {', '.join(f'{x:.1f}' for x in b)} | {', '.join(f'{x:.1f}' for x in h)} |")
for k, v in counts.items():
    print(k, v)
print("load", {s: (min(v), max(v)) for s, v in loads.items()})
