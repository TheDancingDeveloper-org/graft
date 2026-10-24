#!/bin/bash
# WI-2341 re-run: origin/main ("main") vs pre-#138 ("base"), interleaved.
set -u
R=~/scratch/wi2341r
B=$R/src/bench
MAIN=$R/src/target/release
BASE=~/scratch/wi2341/base
run() { # name bindir scale
  local out=$R/out/$1; mkdir -p $R/out
  while [ "$(cut -d. -f1 /proc/loadavg)" -ge 1 ]; do sleep 10; done
  cd $B && python3 -m graftbench --scales $3 --no-import --writers 2 --rounds 1 --iterations 9 \
    --graft-bin $2/graft --server-bin $2/graft-server --mirror-bin $2/graft-mirror \
    --work $R/work-$1 --label $1 --out $out > $R/out/$1.log 2>&1
  echo "$1 rc=$? $(date +%T)" >> $R/progress
  rm -rf $R/work-$1
}
for i in 1 2 3; do run k1-main-$i $MAIN 1000; run k1-base-$i $BASE 1000; done
for i in 1 2 3; do run k10-main-$i $MAIN 10000; run k10-base-$i $BASE 10000; done
echo ALLDONE >> $R/progress
