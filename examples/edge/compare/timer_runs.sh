#!/bin/sh
# Timer delay of the parking lot, before and after, at a low and a high rate:
# each binary in turn records a timeline of `aggregate` (three simulated
# upstream calls of 20-100 ms) and timer_delay.py splits each answer's delay.
#
#   sh examples/edge/compare/timer_runs.sh ROUNDS NAME=BINARY NAME=BINARY ...
#
# Appends to examples/edge/compare/results/timer.jsonl.
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
load="$root/target/release/cove-edge-load"
out="$here/results/timer.jsonl"
tmp=$(mktemp -d)
rounds=$1; shift
r=1
while [ "$r" -le "$rounds" ]; do
  for spec in "$@"; do
    name=${spec%%=*}; bin=${spec#*=}
    for plan in 50:500:64 10000:30000:4000; do
      rate=${plan%%:*}; rest=${plan#*:}; requests=${rest%%:*}; conc=${rest#*:}
      if curl -s -o /dev/null http://127.0.0.1:8799/_stats; then
        echo "timer_runs.sh: port 8799 is already answered by another process" >&2
        exit 1
      fi
      "$bin" --quiet --port 8799 --workers 4 --timeline "$tmp/t.json" > /dev/null 2>&1 &
      pid=$!
      until curl -s -o /dev/null http://127.0.0.1:8799/_stats; do
        kill -0 "$pid" 2>/dev/null || { echo "timer_runs.sh: $bin exited" >&2; exit 1; }
        sleep 0.1
      done
      curl -s -o /dev/null http://127.0.0.1:8799/aggregate/
      curl -s -o /dev/null "http://127.0.0.1:8799/_timeline?reset"
      before=$(sysctl -n vm.loadavg | awk '{print $2}')
      "$load" --addr 127.0.0.1:8799 --path /aggregate/ --rate "$rate" --from-intended \
        --requests "$requests" --concurrency "$conc" --keep-alive \
        --timeline-out "$tmp/t.json" --summary-out "$tmp/s.json" > /dev/null
      kill "$pid"; wait "$pid" 2>/dev/null || true
      python3 "$here/timer_delay.py" "$tmp/t.json" --label "$name rate=$rate round=$r load=$before at=$(date +%Y-%m-%dT%H:%M:%S)" --jsonl "$out"
      sleep 0.5
    done
  done
  r=$((r + 1))
done
rm -rf "$tmp"
