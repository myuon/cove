#!/usr/bin/env python3
"""What the idle thread's `poll(2)` costs as kept-alive connections grow.

    python3 examples/edge/compare/idle_cost.py [--reps 2] [--rate 10000] [--conns 64 256 ...]

The same 10,000 `hello` requests a second as the `connections` sweep in
README.md, spread over N kept-alive connections. Before and after each run
it reads the server's `/_stats` `idle_poller` counters (wakes, pollfds
handed to `poll`, wall time inside `poll` and around it), so each row has
the idle thread's wakes per second, `pollfd`s per wake, and microseconds
per wake inside and around `poll`, beside the server's CPU per request.
Appends to results/idle-cost.jsonl.
"""

import argparse
import json
import os
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sweep  # noqa: E402


def poller():
    body = urllib.request.urlopen(f"http://127.0.0.1:{sweep.PORT['cove']}/_stats", timeout=5).read()
    return json.loads(body)["idle_poller"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=2)
    ap.add_argument("--rate", type=int, default=10000)
    ap.add_argument("--seconds", type=float, default=4)
    ap.add_argument("--conns", type=int, nargs="+", default=[64, 256, 1000, 4000, 10000])
    ap.add_argument("--port", type=int, default=8797, help="not 8787: sweep.py's, which another run may hold")
    ap.add_argument("--edge", default=sweep.EDGE)
    args = ap.parse_args()
    sweep.PORT["cove"] = args.port
    sweep.EDGE = os.path.abspath(args.edge)
    for rep in range(args.reps):
        for n in args.conns:
            proc = sweep.start("cove")
            sweep.warm("cove")
            before = poller()
            requests = int(args.rate * args.seconds)
            summary = sweep.run_load(
                "cove",
                ["--mix", "hello=1", "--rate", str(args.rate), "--from-intended",
                 "--requests", str(requests), "--concurrency", str(n), "--keep-alive"],
                sample_rss_of=proc,
            )
            after = poller()
            sweep.stop(proc)
            wakes = after["wakes"] - before["wakes"]
            fds = after["pollfds"] - before["pollfds"]
            in_poll = after["in_poll_ms"] - before["in_poll_ms"]
            around = after["around_poll_ms"] - before["around_poll_ms"]
            wall = summary["wall_s"]
            row = {
                "rep": rep,
                "connections": n,
                "rate": args.rate,
                "answered": summary["answered"],
                "wall_s": wall,
                "p50_ms": summary["p50_ms"],
                "p99_ms": summary["p99_ms"],
                "server_cpu_s": summary["server_cpu_s"],
                "cpu_us_per_req": 1e6 * summary["server_cpu_s"] / summary["answered"],
                "idle_wakes": wakes,
                "idle_wakes_per_s": wakes / wall,
                "pollfds_per_wake": fds / max(wakes, 1),
                "in_poll_us_per_wake": 1e3 * in_poll / max(wakes, 1),
                "around_poll_us_per_wake": 1e3 * around / max(wakes, 1),
                "wakes_per_req": wakes / summary["answered"],
                "around_poll_us_per_req": 1e3 * around / summary["answered"],
                "load_before": summary["load_before"],
                "binary": sweep.EDGE,
            }
            sweep.append("idle-cost.jsonl", row)
            print(
                f"rep {rep} conns {n:>6}: {row['cpu_us_per_req']:6.1f} µs CPU/req, p50 {row['p50_ms']:.2f} "
                f"p99 {row['p99_ms']:.2f} ms; idle {row['idle_wakes_per_s']:8.0f} wakes/s, "
                f"{row['pollfds_per_wake']:7.0f} fds/wake, {row['wakes_per_req']:.2f} wakes/req, "
                f"in poll {row['in_poll_us_per_wake']:7.1f} µs/wake, around {row['around_poll_us_per_wake']:7.1f} µs/wake "
                f"({row['around_poll_us_per_req']:.1f} µs/req); load {row['load_before']:.2f}",
                flush=True,
            )


if __name__ == "__main__":
    main()
