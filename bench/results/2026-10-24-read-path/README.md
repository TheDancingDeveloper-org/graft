# WI-2341 read-path re-run

graft-dev (4 CPUs, quiet), origin/main f5ac0c4 ("main") interleaved with the
pre-#138 binaries ("base"). `--no-import --writers 2 --rounds 1 --iterations 9`.
`run.sh` runs the set; `agg.py` writes `medians.txt` (median of per-run p50, ms).

All 12 runs completed and passed (1k and 10k, x3 each build, interleaved).

| p50 ms, graft / git | 1k main | 1k base | 10k main | 10k base |
| --- | ---: | ---: | ---: | ---: |
| log -50 | 3.3 / 2.0 | 5.6 / 1.9 | 3.4 / 1.9 | 8.2 / 1.9 |
| status | 4.1 / 3.2 | 4.3 / 3.3 | 13.3 / 6.5 | 14.3 / 6.7 |
| sync, 5 behind | 54.6 / 23.4 | 60.4 / 23.6 | 70.6 / 29.1 | 74.3 / 28.9 |

No regression: main is equal to or faster than base on every read path at
both sizes. The 10k sync 46.3 -> 74.9 figure from #138 was environment, not
code; base measures 59-76 ms today too.

The 2x-faster-than-git target is not met: all three are slower than git.

Sync: at 1k both builds now measure 50-65 ms, against 6-10 ms in the #138
runs. The harness is unchanged for sync, and base slowed as much as main, so
this is not a code regression between base and main. The samples are bimodal
(p50 58, p95 186 ms; git steady). Suspect host or fsync timing (sync fsyncs
`.graft/index` and `.graft/state`); not yet probed.

