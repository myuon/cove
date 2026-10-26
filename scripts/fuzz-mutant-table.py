#!/usr/bin/env python3
"""Tabulates what scripts/fuzz-mutant-campaign.sh wrote, as Markdown.

    scripts/fuzz-mutant-table.py DIR

One row a (mutant, target): whether a dedicated test found the mutant, how
many committed inputs of that target found it on replay, and how many of the
exploration sessions found it and when — the median and quartiles of the
time to the first finding, a session that found nothing being censored at
its budget (written `>S`), and a session stopped by a finding the clean build
shares being censored at the moment it stopped. Then a mutation score per
target and per category: the share of mutants each route detected. A score
here is over these mutants; it is not an estimate of how many unknown bugs
the harness would find.
"""
import os
import re
import sys

CATEGORY = [
    ('native-', 'native code generation'),
    ('gc-', 'collector roots and tracing'),
    ('vm-', 'VM bounds'),
    ('verify-', 'verifier'),
    ('reset-', 'run-to-run state'),
    ('grant-', 'capability grants'),
    ('lower-', 'lowering'),
]


def category(name):
    for key, label in CATEGORY:
        if key in name:
            return label
    return 'other'


def quantile(times, q, budget):
    """The q-quantile of `times` (None = censored at the budget)."""
    xs = sorted(t if t is not None else float('inf') for t in times)
    if not xs:
        return '—'
    v = xs[min(len(xs) - 1, int(q * (len(xs) - 1) + 0.5))]
    return '>%ds' % budget if v == float('inf') else '%ds' % v


def parse(path):
    r = {'dedicated': {}, 'replay': {}, 'explore': {}, 'budget': None}
    last = None
    for line in open(path):
        line = line.rstrip('\n')
        m = re.match(r'dedicated (\S+): (DETECTED|passed)', line)
        if m:
            r['dedicated'][m.group(1)] = m.group(2) == 'DETECTED'
            continue
        m = re.match(r'replay (\S+) (\S+): (\d+) of (\d+) inputs', line)
        if m:
            t = r['replay'].setdefault(m.group(1), [0, 0])
            t[0] += int(m.group(3))
            t[1] += int(m.group(4))
            continue
        m = re.match(r'explore (\S+) seed (\d+): (DETECTED after (\d+)s|not detected in (\d+)s)', line)
        if m:
            t = m.group(1)
            if m.group(4) is not None:
                last = [int(m.group(4)), 'found']
            else:
                r['budget'] = int(m.group(5))
                last = [None, 'missed']
            r['explore'].setdefault(t, []).append(last)
            continue
        if 'the clean build ALSO fails' in line and last is not None:
            # A toolchain finding: the session stopped without finding the
            # mutant, so its time is censored there.
            last[1] = 'stopped'
    return r


def main():
    d = sys.argv[1]
    names = sorted(n for n in os.listdir(d) if os.path.isfile(os.path.join(d, n, 'report.txt')))
    reports = {n: parse(os.path.join(d, n, 'report.txt')) for n in names}
    targets = []
    for r in reports.values():
        for t in r['explore']:
            if t not in targets:
                targets.append(t)
    budget = next((r['budget'] for r in reports.values() if r['budget']), 0)
    if not budget:
        meta = open(os.path.join(d, 'meta.txt')).read() if os.path.exists(os.path.join(d, 'meta.txt')) else ''
        m = re.search(r'x (\d+)s', meta)
        budget = int(m.group(1)) if m else 0

    print('| mutant | dedicated tests | committed inputs of the targets not explored that found it | target | replay: committed inputs that found it | exploration: sessions that found it | time to find: Q1 / median / Q3 | stopped by another finding |')
    print('|---|---|---|---|---|---|---|---|')
    for n in names:
        r = reports[n]
        ded = [k for k, v in r['dedicated'].items() if v]
        ded = ', '.join(ded) if ded else 'none'
        others = ', '.join('`%s` %d of %d' % (t, v[0], v[1]) for t, v in sorted(r['replay'].items())
                           if t not in targets)
        for i, t in enumerate(targets):
            runs = r['explore'].get(t, [])
            found = [x for x in runs if x[1] == 'found']
            times = [x[0] if x[1] == 'found' else None for x in runs]
            stopped = sum(1 for x in runs if x[1] == 'stopped')
            rep = r['replay'].get(t, [0, 0])
            print('| %s | %s | %s | `%s` | %d of %d | %d of %d | %s / %s / %s | %s |' % (
                '`%s`' % n if i == 0 else '', ded if i == 0 else '', others if i == 0 else '',
                t, rep[0], rep[1],
                len(found), len(runs),
                quantile(times, 0.25, budget), quantile(times, 0.5, budget), quantile(times, 0.75, budget),
                stopped or ''))

    print()
    print('| mutation score (of %d mutants) | dedicated tests | committed-input replay | exploration, any session | exploration, share of sessions | any route |' % len(names))
    print('|---|---|---|---|---|---|')
    any_ded = sum(1 for r in reports.values() if any(r['dedicated'].values()))
    for t in targets:
        rep = sum(1 for r in reports.values() if r['replay'].get(t, [0])[0] > 0)
        exp = sum(1 for r in reports.values() if any(x[1] == 'found' for x in r['explore'].get(t, [])))
        sess = [x for r in reports.values() for x in r['explore'].get(t, [])]
        share = sum(1 for x in sess if x[1] == 'found')
        anyr = sum(1 for r in reports.values()
                   if any(r['dedicated'].values()) or r['replay'].get(t, [0])[0] > 0
                   or any(x[1] == 'found' for x in r['explore'].get(t, [])))
        print('| `%s` | %d | %d | %d | %d of %d | %d |' % (t, any_ded, rep, exp, share, len(sess), anyr))
    allr = sum(1 for r in reports.values()
               if any(r['dedicated'].values()) or any(v[0] > 0 for v in r['replay'].values())
               or any(x[1] == 'found' for v in r['explore'].values() for x in v))
    fuzz = sum(1 for r in reports.values()
               if any(v[0] > 0 for v in r['replay'].values())
               or any(x[1] == 'found' for v in r['explore'].values() for x in v))
    print('| every target | %d | | | | %d (fuzzing alone: %d) |' % (any_ded, allr, fuzz))

    print()
    print('| category | mutants | dedicated tests | fuzzing (replay or exploration, any target) | any route |')
    print('|---|---|---|---|---|')
    cats = {}
    for n, r in reports.items():
        c = cats.setdefault(category(n), [0, 0, 0, 0])
        c[0] += 1
        ded = any(r['dedicated'].values())
        fz = any(v[0] > 0 for v in r['replay'].values()) or any(x[1] == 'found' for v in r['explore'].values() for x in v)
        c[1] += ded
        c[2] += fz
        c[3] += ded or fz
    for c, v in cats.items():
        print('| %s | %d | %d | %d | %d |' % (c, *v))


if __name__ == '__main__':
    main()
