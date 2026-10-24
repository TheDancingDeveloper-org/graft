# Read paths against the phase-1 bar (WI-2341)

Phase-1 bar (operator decision, 2026-10-24): `log`, `status` and `sync` are
not slower than git. 2× faster than git is a later-phase target.

graft-dev, medians of per-run p50 ms, graft / git, origin/main f5ac0c4.
All 12 runs: 1k and 10k, main and base, ×3 each. Raw data: `bench/results/2026-10-24-read-path/`.

| | 1k | 10k | Bar met |
| --- | ---: | ---: | --- |
| log -50 | 3.3 / 2.0 | 3.4 / 1.9 | no (1.7×) |
| status | 4.1 / 3.2 | 13.3 / 6.5 | no (1.3× / 2.0×) |
| sync, 5 behind | 54.6 / 23.4 | 70.6 / 29.1 | no (2.3× / 2.4×) |

`log --path` (3.3 / 8.8 at 10k) stays faster than git. main is faster than
the pre-#138 base on `log` (8.2 → 3.4 at 10k). All three read paths are still
slower than git, so WI-2341 stays open for parity work. sync samples are
bimodal on both builds; the probe of `.graft/index` / `.graft/state` fsync
timing and host IO is still to do.
