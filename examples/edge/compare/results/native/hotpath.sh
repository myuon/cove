#!/bin/bash
# Interleaved base/head no-yield measurements. Usage: hotpath.sh ROUNDS OUT
# HEAD is this tree; BASE is fcf64a9 extracted elsewhere and built with
# `cargo build --profile checked -p cove-cli -p cove-edge --features cove-edge/native`.
HEAD=${HEAD:-$(cd "$(dirname "$0")/../../../../.." && pwd)}
BASE=${BASE:-/tmp/nyield/base}
OUT=$2
for round in $(seq 1 $1); do
  for side in base head; do
    if [ $side = base ]; then T=$BASE; else T=$HEAD; fi
    for backend in native vm; do
      echo "round=$round side=$side bench=covefmt-$backend load=$(sysctl -n vm.loadavg)" >> $OUT
      (cd $T/tools/covefmt && $T/target/checked/cove run covefmtBench --files-root $BASE --backend $backend --stats 2>&1 | grep -E "^backend:|^stats:|^memory:") >> $OUT
    done
    echo "round=$round side=$side bench=compare load=$(sysctl -n vm.loadavg)" >> $OUT
    $T/target/checked/cove-edge-compare --batches 5 --min-ms 200 >> $OUT 2>&1
  done
done
echo ALLDONE >> $OUT
