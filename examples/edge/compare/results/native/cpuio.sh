#!/bin/bash
# The #588 cpu-io mix, on the VM and on the native backend, sliced and not.
# Usage: cpuio.sh REPS OUT
ROOT=${ROOT:-$(cd "$(dirname "$0")/../../../../.." && pwd)}
EDGE=$ROOT/target/release/cove-edge
LOAD=$ROOT/target/release/cove-edge-load
OUT=$2
PORT=${PORT:-8813}
run() { # label backend slice rate requests extra...
  label=$1; backend=$2; slice=$3; rate=$4; requests=$5; shift 5
  # Refuse rather than measure somebody else's server.
  if curl -s -o /dev/null --max-time 1 http://127.0.0.1:$PORT/; then echo "port $PORT already answers" >&2; exit 1; fi
  $EDGE --quiet --port $PORT --workers 4 --scheduler steal --slice $slice --backend $backend > /dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 100); do curl -s -o /dev/null http://127.0.0.1:$PORT/_stats && break; sleep 0.1; done
  for p in "/hello/" "/crunch/?n=2000" "/aggregate/"; do curl -s -o /dev/null "http://127.0.0.1:$PORT$p"; done
  echo "=== $label backend=$backend slice=$slice rate=$rate $* load=$(sysctl -n vm.loadavg)" >> $OUT
  $LOAD --addr 127.0.0.1:$PORT --mix cpu-io --requests $requests --concurrency 200 --keep-alive --rate $rate "$@" >> $OUT 2>&1
  curl -s http://127.0.0.1:$PORT/_stats | grep -E '"(yields|yield_requests|errors)"' >> $OUT
  kill $pid; wait $pid 2>/dev/null; sleep 1
}
for rep in $(seq 1 $1); do
  for cfg in "vm 2" "native 2" "native 0" "vm 0"; do
    set -- $cfg
    run "rep$rep-588" $1 $2 330 500
    run "rep$rep-588-intended" $1 $2 330 500 --from-intended
  done
  for cfg in "native 2" "native 0"; do
    set -- $cfg
    run "rep$rep-x3" $1 $2 990 1500 --from-intended
  done
done
echo ALLDONE >> $OUT
