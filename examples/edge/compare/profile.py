#!/usr/bin/env python3
"""Summarises an xctrace Time Profiler recording of the edge server.

    xcrun xctrace export --input X.trace \
        --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' > X.xml
    python3 examples/edge/compare/profile.py X.xml [--requests N] [--leaves 25]

Time Profiler samples a thread only while it is running (1 ms per sample),
so a sample is CPU time. Every sample of the process is assigned to its
thread's role (a worker, the idle poller, the parking lot, ...) and, on a
worker, to the innermost *phase* of the request path its stack is inside
(PHASES below, matched against the mangled Rust symbol, innermost first),
and to its leaf (the syscall wrapper, when it is in the kernel). With
`--requests N` each count is also printed as microseconds per request.
"""

import argparse
import collections
import re
import sys
import xml.etree.ElementTree as ET

# (label, substring of a mangled frame name). Checked from the leaf up: the
# first frame that matches any phase names the sample's phase. Order here
# breaks ties within one frame only.
PHASES = [
    ("http: read(2) the request", "12read_request"),
    ("http: write(2) the response", "8Response4send"),
    ("idle: hand the connection to the idle thread", "4Idle4park"),
    ("isolate: OwnedVm::new (Deployed::isolate)", "8Deployed7isolate"),
    ("boundary: request_value (build edge.Request)", "13request_value"),
    ("boundary: response_of (read edge.Response)", "11response_of"),
    ("run: invoke_within_parkable", "invoke_within_parkable"),
    ("stats: Stats::answered", "5Stats8answered"),
    ("serve: setsockopt (read timeout, nodelay)", "set_read_timeout"),
    ("serve: setsockopt (read timeout, nodelay)", "set_nodelay"),
    ("runq: take a job (wait, steal)", "8RunQueue4take"),
    ("runq: push", "8RunQueue"),
    ("settle: drop the isolate", "drop_in_place"),
    ("serve: route, Flight", "5serve"),
    ("settle", "6settle"),
    ("start", "5start"),
]


def role(thread):
    name = thread.split(" (")[0]
    if name.startswith("edge-worker"):
        return "worker"
    return name


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("xml")
    ap.add_argument("--requests", type=int, default=0)
    ap.add_argument("--leaves", type=int, default=25)
    ap.add_argument(
        "--within",
        help="also break the samples whose stack has a frame containing this down by "
        "the frame that frame called, DEPTH levels in (--depth)",
    )
    ap.add_argument("--depth", type=int, default=1)
    args = ap.parse_args()
    within = collections.Counter()

    frames = {}  # id -> name
    backtraces = {}  # id -> [names], leaf first
    threads = {}
    weights = {}
    by_role = collections.Counter()
    by_phase = collections.Counter()
    by_phase_leaf = collections.Counter()
    by_leaf = collections.Counter()
    total = 0

    for _, row in ET.iterparse(args.xml, events=("end",)):
        if row.tag != "row":
            continue
        thread_el = row.find("thread")
        if thread_el.get("ref"):
            thread = threads[thread_el.get("ref")]
        else:
            thread = thread_el.get("fmt")
            threads[thread_el.get("id")] = thread
        w_el = row.find("weight")
        if w_el.get("ref"):
            w = weights[w_el.get("ref")]
        else:
            w = int(w_el.text)
            weights[w_el.get("id")] = w
        bt_el = row.find("tagged-backtrace")
        names = []
        if bt_el is not None:
            if bt_el.get("ref"):
                names = backtraces[bt_el.get("ref")]
            else:
                b = bt_el.find("backtrace")
                if b.get("ref"):
                    names = backtraces[b.get("ref")]
                else:
                    for f in b.findall("frame"):
                        if f.get("ref"):
                            names.append(frames[f.get("ref")])
                        else:
                            frames[f.get("id")] = f.get("name", "?")
                            names.append(f.get("name", "?"))
                    backtraces[b.get("id")] = names
                backtraces[bt_el.get("id")] = names
        ms = w / 1e6
        total += ms
        r = role(thread)
        by_role[r] += ms
        leaf = names[0] if names else "?"
        if r == "worker":
            phase = "other"
            for name in names:
                hit = next((label for label, sub in PHASES if sub in name), None)
                if hit:
                    phase = hit
                    break
            by_phase[phase] += ms
            by_phase_leaf[(phase, short(leaf))] += ms
        by_leaf[(r, short(leaf))] += ms
        if args.within:
            # Outermost match: the frame nearest the root.
            at = max((i for i, n in enumerate(names) if args.within in n), default=None)
            if at is not None:
                inner = names[max(at - args.depth, 0) : at]
                within[" < ".join(short(n) for n in inner) or "(self)"] += ms
        row.clear()

    per = (lambda ms: f"{1000 * ms / args.requests:8.2f}") if args.requests else (lambda ms: "")
    unit = "µs/req" if args.requests else ""
    print(f"# {total:.0f} ms of samples")
    print(f"\n{'thread role':<52} {'ms':>8} {'share':>6} {unit:>8}")
    for r, ms in by_role.most_common():
        print(f"{r:<52} {ms:8.0f} {100 * ms / total:5.1f}% {per(ms)}")
    print(f"\n{'worker phase (innermost)':<52} {'ms':>8} {'share':>6} {unit:>8}")
    for p, ms in by_phase.most_common():
        print(f"{p:<52} {ms:8.0f} {100 * ms / total:5.1f}% {per(ms)}")
    print(f"\n{'worker phase / leaf':<90} {'ms':>8} {unit:>8}")
    for (p, l), ms in by_phase_leaf.most_common(args.leaves):
        print(f"{(p + ' / ' + l)[:90]:<90} {ms:8.0f} {per(ms)}")
    print(f"\n{'role / leaf':<90} {'ms':>8} {unit:>8}")
    for (r, l), ms in by_leaf.most_common(args.leaves):
        if r != "worker":
            print(f"{(r + ' / ' + l)[:90]:<90} {ms:8.0f} {per(ms)}")
    if args.within:
        print(f"\n{'inside ' + args.within:<110} {'ms':>8} {unit:>8}")
        for k, ms in within.most_common(args.leaves):
            print(f"{k[:110]:<110} {ms:8.0f} {per(ms)}")


def short(name):
    """A readable fragment of a v0-mangled Rust symbol: its last few
    identifiers, read by their length prefixes. Crude — it does not decode
    v0's back-references or generics — but it names the function."""
    if not name.startswith("_R"):
        return name
    i, ids = 0, []
    while i < len(name):
        if name[i].isdigit():
            j = i
            while j < len(name) and name[j].isdigit():
                j += 1
            n = int(name[i:j])
            ident = name[j : j + n]
            if n and re.match(r"[A-Za-z_]", ident or "-"):
                ids.append(ident)
                i = j + n
                continue
            i = j
        else:
            i += 1
    ids = [x for x in ids if not x.startswith("Cs") and len(x) > 1]
    return "::".join(ids[-4:]) if ids else name[:60]


if __name__ == "__main__":
    main()
