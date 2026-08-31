#!/bin/sh
# Six processes of one binary, each a whole suite, run one at a time.
set -e
mkdir -p /tmp/phase-e-bench
for i in 1 2 3 4 5 6; do
  ./target/release/cove-bench --iterations 15 > /tmp/phase-e-bench/suite-$i.json 2>/tmp/phase-e-bench/suite-$i.err
  echo "suite $i done"
done
echo "all done"
