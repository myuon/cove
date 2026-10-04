#!/usr/bin/env python3
"""Interleaved A/B of edge-server binaries under one load.

    python3 examples/edge/compare/ab.py --rounds 7 --out results/ab-X.jsonl \
        NAME=PATH/TO/cove-edge NAME=PATH/TO/cove-edge ... -- LOAD ARGS...

Each round starts every binary in turn (in a rotated order), alone, with
`--workers 4`, warms it, runs `cove-edge-load` with LOAD ARGS against it,
records throughput, latency and the server's CPU per request, and stops it.
A row per run is appended to `--out`, with the load average before it.
The summary printed at the end is the median over rounds per binary, and
each binary's ratio to the first one's median.

The binaries are built by hand, one per variant, and copied aside, so that
the variants differ in nothing but the change: see README.md, "Verifying
the diagnosis".
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sweep  # noqa: E402  (start/stop/run_load, with EDGE swapped per binary)


def start(extra):
    """`sweep.start("cove")`, with `extra` flags on the server's command line."""
    port = sweep.PORT["cove"]
    cmd = [sweep.EDGE, "--quiet", "--port", str(port), "--workers", "4"] + extra
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{port}/_stats", timeout=0.5).read()
        raise SystemExit(f"port {port} is already answered by another process")
    except OSError:
        pass
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/_stats", timeout=1).read()
            if proc.poll() is not None:
                # Ours exited (the port was taken) and something else answered.
                raise SystemExit(f"port {port} is answered by another process")
            return proc
        except Exception:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"{cmd} did not start")


def main():
    argv = sys.argv[1:]
    if "--" not in argv:
        raise SystemExit(__doc__)
    split = argv.index("--")
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--port", type=int, default=8797, help="not 8787: sweep.py's, which another run may hold")
    ap.add_argument("--out", required=True)
    ap.add_argument("--extra", default="", help="extra server flags, space-separated")
    ap.add_argument("variants", nargs="+")
    args = ap.parse_args(argv[:split])
    sweep.PORT["cove"] = args.port
    load_args = argv[split + 1 :]
    variants = [v.split("=", 1) for v in args.variants]
    rows = {name: [] for name, _ in variants}
    for r in range(args.rounds):
        order = variants[r % len(variants) :] + variants[: r % len(variants)]
        for name, path in order:
            sweep.EDGE = os.path.abspath(path)
            proc = start(args.extra.split())
            sweep.warm("cove")
            summary = sweep.run_load("cove", load_args, sample_rss_of=proc)
            sweep.stop(proc)
            answered = summary["answered"]
            row = {
                "variant": name,
                "binary": path,
                "round": r,
                "load_args": load_args,
                "throughput": summary["throughput"],
                "p50_ms": summary["p50_ms"],
                "p99_ms": summary["p99_ms"],
                "server_cpu_s": summary["server_cpu_s"],
                "cpu_us_per_req": 1e6 * summary["server_cpu_s"] / answered,
                "load_before": summary["load_before"],
                "load_after": summary["load_after"],
                "time": time.strftime("%Y-%m-%dT%H:%M:%S"),
            }
            rows[name].append(row)
            with open(args.out, "a") as f:
                f.write(json.dumps(row, sort_keys=True) + "\n")
            print(
                f"round {r} {name:<12} {row['throughput']:9.0f} req/s  "
                f"{row['cpu_us_per_req']:6.1f} µs CPU/req  p50 {row['p50_ms']:.2f} p99 {row['p99_ms']:.2f}  "
                f"load {row['load_before']:.2f}",
                flush=True,
            )
    first = None
    print(f"\n{'variant':<12} {'req/s median':>13} {'range':>21} {'CPU µs/req':>11} {'range':>15} {'vs first':>9}")
    for name, _ in variants:
        tp = [x["throughput"] for x in rows[name]]
        cpu = [x["cpu_us_per_req"] for x in rows[name]]
        m, c = statistics.median(tp), statistics.median(cpu)
        if first is None:
            first = (m, c)
        print(
            f"{name:<12} {m:13.0f} {min(tp):10.0f}–{max(tp):<10.0f} {c:11.1f} {min(cpu):7.1f}–{max(cpu):<7.1f} "
            f"{m / first[0]:8.3f}x tput, {c / first[1]:.3f}x CPU"
        )


if __name__ == "__main__":
    main()
