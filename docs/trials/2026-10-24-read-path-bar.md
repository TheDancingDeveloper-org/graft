# Read paths against the phase-1 bar (WI-2341), interim

Phase-1 bar (operator decision, 2026-10-24): `log`, `status` and `sync` are
not slower than git. 2× faster than git is a later-phase target.

graft-dev, medians of per-run p50 ms, graft / git, origin/main f5ac0c4.
Interim: 8 of 11 runs (1k ×3, 10k ×2); 10k main ×1 and base ×2 were still
running. Raw data: `bench/results/2026-10-24-read-path/`.

| | 1k | 10k | Bar met |
| --- | ---: | ---: | --- |
| log -50 | 3.3 / 2.0 | 3.1 / 1.9 | no (1.6×) |
| status | 4.1 / 3.2 | 13.3 / 6.4 | no (1.3× / 2.1×) |
| sync, 5 behind | 54.6 / 23.4 | 68.6 / 29.5 | no (2.3×) |

`log --path` (3.1 / 8.9 at 10k) stays faster than git. main is faster than
the pre-#138 base on `log` (8.2 → 3.1 at 10k). All three read paths are still
slower than git, so WI-2341 stays open for parity work. sync samples are
bimodal on both builds; the probe of `.graft/index` / `.graft/state` fsync
timing and host IO is still to do.
