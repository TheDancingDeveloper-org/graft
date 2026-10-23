# graft bench: scale 10,000 on graft-dev (WI-2304)

Started 2026-10-23T23:19:48+00:00, 1669 s. Seed 2294, 8 concurrent writers x 5 rounds, 5 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | graft-dev, Linux 7.0.0-30-generic |
| CPU | AMD Ryzen 7 7735HS with Radeon Graphics (4 CPUs), 7.8 GiB |
| graft | `2c32be2d39a3`, graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Scale 10,000

10,001 commits (22,872 file operations), final tree 3,264 files, 2.5 MiB. Stream sha256 `9edbac0db919b739`, git head `e415aa2a1dce`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 8.99 | 30.06 |
| commit p50 / p95 (ms) | 63.8 / 130.1 | 14.2 / 28.5 |
| land or push p50 / p95 / p99 (ms) | 33.9 / 61.9 / 100.0 | 15.8 / 26.3 / 44.3 |
| import whole history (s) | 133.579 (graft-mirror, 74.87 commits/s) | 6.295 (fast-import + generation) |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 383.7 | 1,347.0 | 0.28 |
| clone, sparse (`src/synthapp/action_7687`) | 42.8 | 560.2 | 0.08 |
| status (11 changed files) | 13.8 | 6.8 | 2.04 |
| diff, working copy | 13.3 | 4.5 | 2.95 |
| diff, 20 changesets | 10.8 | 6.3 | 1.71 |
| log, 50 | 7.8 | 1.9 | 4.07 |
| log --path `registry.py`, 50 | 7.8 | 8.4 | 0.93 |
| blame `registry.py` | 29.1 | 105.9 | 0.27 |
| sync, 5 behind | 47.3 | 28.4 | 1.66 |

### Concurrent writers (8 writers x 5 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 40/40 | 238.4 | 382.0 | 423.8 | 20.582 | 35 | 40 | 0 | 0 |  |
| clean | git | 40/40 | 365.1 | 735.9 | 1,187.0 | 10.505 | 35 | 180 |  | 0 |  |
| merge3 | graft | 40/40 | 228.5 | 384.3 | 415.2 | 21.596 | 35 | 40 | 0 | 0 |  |
| merge3 | git | 40/40 | 316.1 | 1,466.2 | 1,564.7 | 11.005 | 35 | 180 |  | 0 |  |
| conflict | graft | 40/40 | 1,141.8 | 2,111.7 | 2,307.3 | 3.901 | 35 | 215 | 35 | 140 | 234.1 |
| conflict | git | 40/40 | 490.8 | 788.8 | 797.7 | 10.14 | 35 | 180 |  | 140 | 111.0 |

### Correctness

| Check | Result |
| --- | --- |
| git replay head = fast-import head (byte-identical history) | yes |
| graft tree after replay = model replay (3,264 files) | yes |
| git tree after replay = model replay | yes |
| graft log = landed sequence (10,001 changesets) | yes |
| graft-mirror import tree = model | yes |
| final graft tree = expected, byte for byte | yes |
| final git tree = expected, byte for byte | yes |
| final graft tree = final git tree | yes |
| no lost or duplicated lands (10,147 lands, log 10,147) | yes |
| blame of `registry.py` intact (583 lines; agrees with git on 583) | yes |
| blame of `CHANGELOG.md` intact (326 lines; agrees with git on 326) | yes |
| line never moved backwards (replay order, 100 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 13.4 | 13.4 | 0.2 |
| after_import | 30.45 | 56.2 | 73.7 | 129.6 |
| after_graft_replay | 162.09 | 57.6 | 73.7 | 257.1 |
| after_ops | 163.99 | 63.1 | 73.7 | 257.2 |
| after_concurrent | 169.81 | 63.1 | 73.7 | 259.7 |
| end | 170.47 | 63.1 | 73.7 | 259.7 |

git replay repository: 23.0 MiB (loose objects, no gc).

