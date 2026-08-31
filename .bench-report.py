import json, glob, statistics, sys

rows = ["pure", "arith", "call", "field", "method", "sortedargs", "mixedargs"]
per_suite = {}
for path in sorted(glob.glob("/tmp/phase-e-bench/suite-*.json")):
    vm, frame = {}, {}
    for line in open(path):
        line = line.strip()
        if not line.startswith("{"):
            continue
        d = json.loads(line)
        if d.get("kind") == "vm":
            vm[d["benchmark"]] = d["wall_ns"]["median"]
        if d.get("kind") == "frame":
            frame[d["benchmark"]] = d["wall_ns"]["median"]
    per_suite[path] = (vm, frame)

print(f"{len(per_suite)} suite(s)")
print()
print(f"{'row':<12}{'VM ms':>10}{'frame ms':>11}{'ratio':>9}{'band pt':>9}")
for row in rows:
    vms = [v[row] for v, f in per_suite.values() if row in v]
    frs = [f[row] for v, f in per_suite.values() if row in f]
    ratios = [f[row] / v[row] for v, f in per_suite.values() if row in v and row in f]
    if not ratios:
        continue
    band = (max(ratios) - min(ratios)) * 100
    print(
        f"{row:<12}{statistics.median(vms)/1e6:>10.2f}{statistics.median(frs)/1e6:>11.2f}"
        f"{statistics.median(ratios):>9.3f}{band:>9.1f}"
    )
    print(f"             ratios: {' '.join(f'{r:.3f}' for r in sorted(ratios))}")

print()
# The per-call figures, each pair two rows of one run.
def per_call(a, b, calls, backend):
    out = []
    for v, f in per_suite.values():
        d = v if backend == "vm" else f
        if a in d and b in d:
            out.append((d[a] - d[b]) / calls)
    return statistics.median(out) if out else float("nan")

print("per-call, each figure two rows of one run (ns):")
print(f"  scalar frame   call - arith      VM {per_call('call','arith',2_000_000,'vm'):6.1f}  frame {per_call('call','arith',2_000_000,'frame'):6.1f}")
print(f"  with reference method - field    VM {per_call('method','field',4_000_000,'vm'):6.1f}  frame {per_call('method','field',4_000_000,'frame'):6.1f}")
print(f"  the permutation mixedargs - sortedargs, 4,000,000 calls:")
print(f"      VM    {per_call('mixedargs','sortedargs',4_000_000,'vm'):+7.3f}")
print(f"      frame {per_call('mixedargs','sortedargs',4_000_000,'frame'):+7.3f}")
diffs = []
for v, f in per_suite.values():
    if "mixedargs" in f and "sortedargs" in f:
        diffs.append(f["mixedargs"] / f["sortedargs"])
if diffs:
    print(f"  mixedargs / sortedargs on the frame, per suite: {' '.join(f'{d:.4f}' for d in sorted(diffs))}")
vdiffs = []
for v, f in per_suite.values():
    if "mixedargs" in v and "sortedargs" in v:
        vdiffs.append(v["mixedargs"] / v["sortedargs"])
if vdiffs:
    print(f"  mixedargs / sortedargs on the VM,    per suite: {' '.join(f'{d:.4f}' for d in sorted(vdiffs))}")
