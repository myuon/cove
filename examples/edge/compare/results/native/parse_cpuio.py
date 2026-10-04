import re, sys, statistics as st
from collections import defaultdict
rows = defaultdict(lambda: defaultdict(list))
cur = None
for line in open(sys.argv[1]):
    m = re.match(r"=== rep\d+-(\S+) backend=(\w+) slice=(\d+) rate=(\d+)", line)
    if m:
        cur = (m.group(1), m.group(2), m.group(3), m.group(4)); continue
    m = re.match(r"\s+(hello|crunch|aggregate|proxy|impatient)\s.*p50\s+([\d.]+) ms\s+p99\s+([\d.]+) ms", line)
    if m and cur:
        rows[cur][m.group(1) + " p50"].append(float(m.group(2)))
        rows[cur][m.group(1) + " p99"].append(float(m.group(3))); continue
    m = re.match(r'\s+"yields": (\d+)', line)
    if m and cur and len(rows[cur]["yields"]) < len(rows[cur]["hello p99"]):
        rows[cur]["yields"].append(int(m.group(1)))
    m = re.match(r"\s+answered .* ([\d.]+) req/s", line)
    if m and cur:
        rows[cur]["req/s"].append(float(m.group(1)))
def cell(v):
    return f"{st.median(v):.1f} ({', '.join(f'{x:g}' for x in v)})" if v else "–"
print("| run | backend | slice | rate | hello p50 | **hello p99** | crunch p50 | crunch p99 | aggregate p50 | yields | req/s |")
print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
for k, v in rows.items():
    print(f"| {k[0]} | {k[1]} | {k[2]} ms | {k[3]} | {cell(v['hello p50'])} | {cell(v['hello p99'])} | {cell(v['crunch p50'])} | {cell(v['crunch p99'])} | {cell(v['aggregate p50'])} | {cell(v['yields'])} | {cell(v['req/s'])} |")
