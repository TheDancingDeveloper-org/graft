# WI-2341 read-path re-run (partial)

graft-dev (4 CPUs, quiet), origin/main f5ac0c4 ("main") interleaved with the
pre-#138 binaries ("base"). `--no-import --writers 2 --rounds 1 --iterations 9`.
`run.sh` runs the set; `agg.py` writes `medians.txt` (median of per-run p50, ms).

Done at hand-off: 1k x3 main, 1k x3 base, 10k x1 main. The remaining 10k runs
(main x2, base x3) were still running on graft-dev (user unit `wi2341r`,
output in `~/scratch/wi2341r/out`).

| p50 ms, graft / git | 1k main | 1k base | 10k main |
| --- | ---: | ---: | ---: |
| log -50 | 3.3 / 2.0 | 5.6 / 1.9 | 2.9 / 1.8 |
| status | 4.1 / 3.2 | 4.3 / 3.3 | 13.3 / 6.3 |
| sync, 5 behind | 54.6 / 23.4 | 60.4 / 23.6 | 75.1 / 29.0 |

The 2x-faster-than-git target is not met: all three are slower than git.

Sync: at 1k both builds now measure 50-65 ms, against 6-10 ms in the #138
runs. The harness is unchanged for sync, and base slowed as much as main, so
this is not a code regression between base and main. The samples are bimodal
(p50 58, p95 186 ms; git steady). Suspect host or fsync timing (sync fsyncs
`.graft/index` and `.graft/state`); not yet probed.
