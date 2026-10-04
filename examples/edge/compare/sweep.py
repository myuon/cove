#!/usr/bin/env python3
"""Stage 2 of the edge-vs-Go comparison: drives both servers with the same
load generator and appends what it measured to results/*.jsonl.

    python3 examples/edge/compare/sweep.py capacity [--reps 3]
    python3 examples/edge/compare/sweep.py sweep [--reps 3] [--scenario hello ...]
    python3 examples/edge/compare/sweep.py connections [--reps 3] [--rate 10000] [--pools 64 ...]
    python3 examples/edge/compare/sweep.py waiting [--reps 3]

Expects `cargo build --release -p cove-edge` and `go build -o edge-go .` in
compare/go to have been run (README.md, "Reproducing"). Each run starts the
server it measures, alone, with 4 workers / GOMAXPROCS=4, and stops it
after; the edge server and the Go server alternate, so that a change in
the machine's load lands on both. std only.
"""

import argparse
import atexit
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LOAD = os.path.join(ROOT, "target", "release", "cove-edge-load")
EDGE = os.path.join(ROOT, "target", "release", "cove-edge")
GO = os.path.join(HERE, "go", "edge-go")
RESULTS = os.path.join(HERE, "results")
PORT = {"cove": 8787, "go": 8788}

# What each scenario asks for: a `cove-edge-load` target (a path, or a mix).
SCENARIOS = {
    "hello": ["--mix", "hello=1"],
    "crunch": ["--path", "/crunch/?n=20000"],
    "aggregate": ["--path", "/aggregate/"],
    "cpu-io": ["--mix", "cpu-io"],
}

# The offered rates of the sweep, requests per second: one absolute grid per
# scenario, covering both servers, each server measured up to a little past
# its own capacity (CAPACITY below, from `capacity`) and no further, because
# an open-loop run past saturation only measures how long it was.
GRID = {
    "hello": [2500, 10000, 25000, 50000, 75000, 100000, 125000, 150000, 175000],
    "crunch": [100, 250, 500, 750, 900, 1000, 1500, 2500, 3000, 3500, 4000],
    "aggregate": [1000, 2500, 5000, 10000, 20000, 30000, 35000, 40000, 45000],
    "cpu-io": [50, 100, 200, 250, 300, 350, 400, 600, 1000, 1250, 1500],
}

# Seconds of arrivals the connection pool holds, per scenario: about twice
# the scenario's typical latency on the slower server.
POOL_SECONDS = {"hello": 0.004, "crunch": 0.05, "aggregate": 0.5, "cpu-io": 0.6}

# Each server's capacity per scenario (req/s, unthrottled, the median of
# `capacity`'s runs), which bounds its sweep at 1.15x. Filled in from
# results/capacity.jsonl when it exists.
STOP_PAST = 1.15


def capacities():
    found = {}
    path = os.path.join(RESULTS, "capacity.jsonl")
    if not os.path.exists(path):
        return found
    rows = {}
    with open(path) as f:
        for line in f:
            row = json.loads(line)
            key = (row["server"], row["scenario"], row["concurrency"])
            rows.setdefault(key, []).append(row["throughput"])
    # The best concurrency's median: a closed loop with too few in flight
    # measures the concurrency, not the server.
    for (server, scenario, _), values in rows.items():
        values.sort()
        median = values[len(values) // 2]
        found[(server, scenario)] = max(found.get((server, scenario), 0), median)
    return found


def start(server, latency=None):
    port = PORT[server]
    if server == "cove":
        cmd = [EDGE, "--quiet", "--port", str(port), "--workers", "4"]
        if latency:
            cmd += ["--latency", latency]
        env = None
    else:
        cmd = [GO, "-port", str(port), "-quiet"]
        if latency:
            cmd += ["-latency", latency]
        env = dict(os.environ, GOMAXPROCS="4")
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    atexit.register(lambda: proc.poll() is None and proc.kill())
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/_stats", timeout=1).read()
            return proc
        except Exception:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"{server} did not start")


def stop(proc):
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    time.sleep(0.5)


def ps(pid, field):
    out = subprocess.run(["ps", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return out


def cpu_seconds(pid):
    """User + system CPU of `pid`, from `ps -o time=` ([[dd-]hh:]mm:ss.cc)."""
    text = ps(pid, "time")
    total = 0.0
    for part in text.replace("-", ":").split(":"):
        total = total * 60 + float(part)
    return total


def load_average():
    return os.getloadavg()[0]


def run_load(server, args, sample_rss_of=None):
    """One `cove-edge-load` run; its summary, plus the server's CPU over it
    and, if asked, its peak RSS sampled every 20 ms."""
    out = "/tmp/edge-compare-summary.json"
    if os.path.exists(out):
        os.remove(out)
    cmd = [LOAD, "--addr", f"127.0.0.1:{PORT[server]}", "--summary-out", out] + args
    load_before = load_average()
    pid = sample_rss_of.pid if sample_rss_of else None
    proc = sample_rss_of
    cpu_before = cpu_seconds(proc.pid) if proc else None
    rss_before = int(ps(pid, "rss") or 0) if pid else None
    child = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    peak = rss_before or 0
    if pid:
        while child.poll() is None:
            try:
                peak = max(peak, int(ps(pid, "rss") or 0))
            except ValueError:
                pass
            time.sleep(0.02)
    text, _ = child.communicate()
    if child.returncode != 0 or not os.path.exists(out):
        print(text, file=sys.stderr)
        raise SystemExit(f"cove-edge-load failed: {' '.join(cmd)}")
    with open(out) as f:
        summary = json.load(f)
    if proc:
        summary["server_cpu_s"] = cpu_seconds(proc.pid) - cpu_before
        summary["rss_before_kib"] = rss_before
        summary["rss_peak_kib"] = peak
    summary["load_before"] = load_before
    summary["load_after"] = load_average()
    return summary


def append(name, row):
    os.makedirs(RESULTS, exist_ok=True)
    with open(os.path.join(RESULTS, name), "a") as f:
        f.write(json.dumps(row, sort_keys=True) + "\n")


def warm(server):
    # Every tenant once, so neither server's first request is in a run.
    port = PORT[server]
    for path in ["/hello/", "/crunch/?n=2000", "/aggregate/", f"/proxy/?url=http://127.0.0.1:{port}/hello/"]:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=10).read()
        except Exception:
            pass


def capacity(args):
    plans = {
        # (concurrency, requests): closed loop, enough in flight to keep four
        # workers busy without the queue being the whole latency.
        "hello": [(64, 200000), (256, 200000)],
        "crunch": [(16, 4000), (64, 4000)],
        "aggregate": [(2000, 40000), (5000, 60000), (10000, 120000)],
        "cpu-io": [(64, 2000), (256, 3000), (1024, 8000)],
    }
    for rep in range(args.reps):
        for scenario in args.scenario:
            for concurrency, requests in plans[scenario]:
                if args.concurrency and concurrency not in args.concurrency:
                    continue
                for server in ["cove", "go"]:
                    proc = start(server)
                    warm(server)
                    load = ["--keep-alive", "--concurrency", str(concurrency), "--requests", str(requests)]
                    s = run_load(server, SCENARIOS[scenario] + load, sample_rss_of=proc)
                    stop(proc)
                    row = dict(s, server=server, scenario=scenario, rep=rep)
                    append("capacity.jsonl", row)
                    print(f"capacity {scenario:<9} {server:<4} c={concurrency:<5} {s['throughput']:>9.0f} req/s  "
                          f"p50 {s['p50_ms']:.1f} p99 {s['p99_ms']:.1f} ms  cpu {row['server_cpu_s']:.2f}s  "
                          f"errors {s['errors']}  load {s['load_before']:.1f}", flush=True)


def sweep(args):
    caps = capacities()
    for rep in range(args.reps):
        for scenario in args.scenario:
            for server in ["cove", "go"]:
                cap = caps.get((server, scenario))
                proc = start(server)
                warm(server)
                for rate in GRID[scenario]:
                    if cap and rate > cap * STOP_PAST:
                        break
                    seconds = args.seconds
                    requests = max(1000, int(rate * seconds))
                    # Enough connections for twice the arrivals of one
                    # typical latency, at least 64, and not more: an idle
                    # kept-alive connection is not free on the edge server
                    # (its idle thread polls every one per wake-up; see
                    # `connections`), so a pool sized for the worst case
                    # would measure that instead. Past saturation the cap
                    # binds, the generator reports the send lag, and the
                    # latency still counts the wait from the intended start.
                    concurrency = min(max(64, int(rate * POOL_SECONDS[scenario])), 12000)
                    load = ["--keep-alive", "--rate", str(rate), "--from-intended",
                            "--concurrency", str(concurrency), "--requests", str(requests)]
                    s = run_load(server, SCENARIOS[scenario] + load, sample_rss_of=proc)
                    row = dict(s, server=server, scenario=scenario, rep=rep, offered=rate)
                    append("sweep.jsonl", row)
                    print(f"sweep {scenario:<9} {server:<4} rate {rate:>6} -> {s['throughput']:>8.0f} req/s  "
                          f"p50 {s['p50_ms']:>8.2f} p99 {s['p99_ms']:>9.2f} ms  lag p99 {s['lag_p99_ms']:.2f} "
                          f"late {s['late']}  err {s['errors']}  cpu/req {1e6 * row['server_cpu_s'] / max(1, s['answered']):.0f} us  "
                          f"load {s['load_before']:.1f}", flush=True)
                    time.sleep(1)
                stop(proc)


def connections(args):
    """hello at a fixed rate over more and more kept-alive connections:
    the same arrivals, spread thinner."""
    for rep in range(args.reps):
        for pool in args.pools:
            for server in ["cove", "go"]:
                proc = start(server)
                warm(server)
                rate = args.rate
                load = ["--keep-alive", "--rate", str(rate), "--from-intended",
                        "--concurrency", str(pool), "--requests", str(int(rate * args.seconds))]
                s = run_load(server, SCENARIOS["hello"] + load, sample_rss_of=proc)
                stop(proc)
                row = dict(s, server=server, scenario="hello-connections", rep=rep, offered=rate, pool=pool)
                append("connections.jsonl", row)
                print(f"connections {server:<4} pool {pool:>6} rate {rate} -> p50 {s['p50_ms']:.2f} p99 {s['p99_ms']:.2f} ms  "
                      f"cpu/req {1e6 * row['server_cpu_s'] / max(1, s['answered']):.0f} us  opened {s['connections']}  "
                      f"err {s['errors']}  load {s['load_before']:.1f}", flush=True)
                time.sleep(1)


def waiting(args):
    for rep in range(args.reps):
        for n in args.inflight:
            for server in ["cove", "go"]:
                proc = start(server, latency="1000..1000")
                warm(server)
                time.sleep(1)
                load = ["--concurrency", str(n), "--requests", str(n)]
                s = run_load(server, ["--path", "/aggregate/"] + load, sample_rss_of=proc)
                stop(proc)
                row = dict(s, server=server, scenario="aggregate-1s", rep=rep, inflight=n)
                append("waiting.jsonl", row)
                growth = (row["rss_peak_kib"] - row["rss_before_kib"]) / n
                print(f"waiting {server:<4} n={n:<6} ok {s['ok']}/{n}  rss {row['rss_before_kib']} -> {row['rss_peak_kib']} KiB "
                      f"({growth:.1f} KiB per waiting request)  cpu {row['server_cpu_s']:.2f}s "
                      f"({1e6 * row['server_cpu_s'] / n:.0f} us/request)  p50 {s['p50_ms']:.0f} ms", flush=True)
                time.sleep(2)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ["capacity", "sweep", "connections", "waiting"]:
        p = sub.add_parser(name)
        p.add_argument("--reps", type=int, default=3)
        p.add_argument("--scenario", nargs="*", default=list(SCENARIOS))
        p.add_argument("--seconds", type=float, default=4.0)
        p.add_argument("--concurrency", type=int, nargs="*", default=None)
        p.add_argument("--pools", type=int, nargs="*", default=[64, 256, 1000, 4000, 10000])
        p.add_argument("--rate", type=int, default=10000)
        p.add_argument("--inflight", type=int, nargs="*", default=[1000, 10000])
    args = parser.parse_args()
    for binary in [LOAD, EDGE, GO]:
        if not os.path.exists(binary):
            raise SystemExit(f"missing {binary}; see README.md, Reproducing")
    {"capacity": capacity, "sweep": sweep, "connections": connections, "waiting": waiting}[args.command](args)


if __name__ == "__main__":
    main()
