# Core bench at 10,000 commits on the graft-dev VM (2026-10-23)

WI-2304. `python3 -m graftbench --scales 10000 --keep`, release build of
`main` at `2c32be2` built on the VM, a fresh local `graft-server` with its own
data dir (the deployed graft-dev server was not touched). Raw results:
[bench/results/2026-10-23-scale-10000](../../bench/results/2026-10-23-scale-10000/).

Host: graft-dev (Proxmox VM, AMD Ryzen 7 7735HS, 4 vCPU, 7.8 GiB). The 10 and
1,000 run ([core-bench-10-1000](2026-10-23-core-bench-10-1000.md)) ran on
vogt-prod (24 CPUs, shared), at `35c3087`, before WI-2296; cross-run numbers
below compare across hosts and builds, so read them as shape, not speed.

**Result: pass.** Every correctness check holds at 10,001 commits: byte-identical
trees for graft, git and the model; graft log equals the landed sequence
(10,147 lands, none lost or duplicated); the line never moved backwards; blame
of the shared files is intact and agrees with `git blame` on every line. Run
time 1,669 s (28 min).

## Headline numbers

| | graft | git |
| --- | ---: | ---: |
| replay throughput (commit + land) | 8.99 commits/s | 30.06 commits/s |
| land p50 / p95 / p99 (replay) | 33.9 / 61.9 / 100.0 ms | 15.8 / 26.3 / 44.3 ms |
| import, whole history | 133.6 s, 74.9 commits/s (13.4 s per 1k) | |
| clone full / sparse | 384 / 43 ms | 1,347 / 560 ms |
| status, 11 changed | 13.8 ms | 6.8 ms |
| log 50 / log --path 50 | 7.8 / 7.8 ms | 1.9 / 8.4 ms |
| blame `registry.py` (583 lines) | 29.1 ms | 105.9 ms |
| sync, 5 behind | 47.3 ms | 28.4 ms |
| concurrent clean / merge3 land p50 | 238 / 229 ms | 365 / 316 ms |
| concurrent conflict land p50 | 1,142 ms | 491 ms |
| `graft line create` at the tip | ~10 ms wall (CLI process included), flat | |

**merge3 is fixed.** With WI-2296 on main, the merge3 scenario lands 40/40
in 40 attempts with **0 file-level refusals** (it was 140 refusals and 180
attempts at 1k). graft's merge3 p50 now equals its clean p50.

**`graft line create`** at the tip of the 10k histories (both the replayed
repo and the imported one), 10 runs each: 9.6 to 17.5 ms wall, median about
10.5 ms, including CLI start-up. That is one round trip, the same order as
`log -50`; nothing in it scales with history (WI-2316 check passes).

**Import row.** 13.4 s per 1k commits on graft-dev, against about 6 s per 1k
expected on Node B after #110. #110 pre-writes trees concurrently, so the
import is CPU-bound and graft-dev has 4 vCPUs to Node B's many; this is the
host, not history. The per-commit import cost is not rising: 1k imported
18 MiB, 10k 130 MiB (13 KiB per commit both).

## Growth with history (design rule 2)

No rule-2 suspects. What was watched, 1k (vogt-prod) against 10k (graft-dev):

| Measure | 1k | 10k | Verdict |
| --- | ---: | ---: | --- |
| land p50 (replay) | 47.4 ms | 33.9 ms | flat |
| replay throughput, within the 10k run | 10.8/s at 1k | 9.0/s at 10k | -17%, but git fell by the same share (36.3 to 30/s): the generated commits get heavier, not graft's history |
| status | 13.6 ms | 13.8 ms | flat |
| log -50 | 15.6 ms | 7.8 ms | flat |
| sync, 5 behind | 76.6 ms | 47.3 ms | flat |
| diff, 20 changesets | 32.3 ms | 10.8 ms | flat |
| storage per replayed commit | 13.1 KiB | 12.8 KiB | flat |
| server RSS after replay | 39 MiB | 58 MiB (peak 74) | grows far slower than history (10x); the tree is 5.6x larger |
| blame `registry.py` | 14.3 ms | 29.1 ms | follows the file (103 to 583 lines), not the history |
| full clone | 276 ms | 384 ms | follows the tree (585 to 3,264 files) |

## Notes

- The conflict scenario is still graft's weak spot: p50 1.1 s, 3.9 lands/s
  against git's 0.49 s and 10.1/s. It is a client round-trip cost (refusal,
  `land --sync`, resolve, land), not history growth.
- `--keep` scratch is on the VM at `~/wi2304/work` (server data, token file
  0600), to be deleted when no longer needed.
- 100k remains deferred (operator decision, 2026-10-23).
