# graft bench: k10-base-1

Started 2026-10-24T09:23:38+00:00, 1790 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | graft-dev, Linux 7.0.0-38-generic |
| CPU | AMD Ryzen 7 7735HS with Radeon Graphics (4 CPUs), 7.7 GiB |
| graft | ``, graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Scale 10,000

10,001 commits (22,872 file operations), final tree 3,264 files, 2.5 MiB. Stream sha256 `9edbac0db919b739`, git head `e415aa2a1dce`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 7.65 | 23.41 |
| commit p50 / p95 (ms) | 67.9 / 174.4 | 16.6 / 38.9 |
| land or push p50 / p95 / p99 (ms) | 37.4 / 87.5 / 128.8 | 18.2 / 42.8 / 53.0 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 461.6 | 1,362.3 | 0.34 |
| clone, sparse (`src/synthapp/action_7687`) | 38.5 | 565.0 | 0.07 |
| status (11 changed files) | 14.3 | 6.7 | 2.13 |
| diff, working copy | 13.8 | 5.0 | 2.74 |
| diff, 20 changesets | 11.2 | 6.8 | 1.64 |
| log, 50 | 8.2 | 1.9 | 4.37 |
| log --path `registry.py`, 50 | 7.8 | 8.9 | 0.88 |
| blame `registry.py` | 25.6 | 107.9 | 0.24 |
| sync, 5 behind | 74.3 | 28.7 | 2.58 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 109.6 | 131.2 | 133.1 | 14.91 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 45.6 | 68.9 | 71.0 | 27.847 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 108.5 | 130.5 | 132.5 | 15.014 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 43.6 | 66.9 | 68.9 | 28.638 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 233.5 | 369.2 | 381.2 | 5.201 | 1 | 4 | 1 | 1 | 194.1 |
| conflict | git | 2/2 | 55.2 | 89.3 | 92.3 | 21.415 | 1 | 3 |  | 1 | 51.0 |

### Correctness

| Check | Result |
| --- | --- |
| git replay head = fast-import head (byte-identical history) | yes |
| graft tree after replay = model replay (3,264 files) | yes |
| git tree after replay = model replay | yes |
| graft log = landed sequence (10,001 changesets) | yes |
| final graft tree = expected, byte for byte | yes |
| final git tree = expected, byte for byte | yes |
| final graft tree = final git tree | yes |
| no lost or duplicated lands (10,053 lands, log 10,053) | yes |
| blame of `registry.py` intact (583 lines; agrees with git on 583) | yes |
| blame of `CHANGELOG.md` intact (326 lines; agrees with git on 326) | yes |
| line never moved backwards (replay order, 4 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.0 | 12.0 | 0.2 |
| after_graft_replay | 138.38 | 26.6 | 27.0 | 132.2 |
| after_ops | 141.23 | 37.4 | 38.5 | 132.4 |
| after_concurrent | 141.66 | 39.9 | 39.9 | 132.4 |
| end | 142.25 | 38.4 | 39.9 | 132.4 |

git replay repository: 23.0 MiB (loose objects, no gc).

