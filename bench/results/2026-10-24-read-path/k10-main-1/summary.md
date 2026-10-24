# graft bench: k10-main-1

Started 2026-10-24T08:58:36+00:00, 1446 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 9.63 | 28.42 |
| commit p50 / p95 (ms) | 61.3 / 96.5 | 14.9 / 28.2 |
| land or push p50 / p95 / p99 (ms) | 33.4 / 46.2 / 87.7 | 17.1 / 28.0 / 45.6 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 500.5 | 1,378.4 | 0.36 |
| clone, sparse (`src/synthapp/action_7687`) | 72.8 | 638.2 | 0.11 |
| status (11 changed files) | 13.3 | 6.3 | 2.11 |
| diff, working copy | 13.2 | 4.6 | 2.86 |
| diff, 20 changesets | 8.1 | 6.4 | 1.26 |
| log, 50 | 2.9 | 1.8 | 1.57 |
| log --path `registry.py`, 50 | 2.9 | 8.8 | 0.32 |
| blame `registry.py` | 23.2 | 107.3 | 0.22 |
| sync, 5 behind | 75.1 | 29.0 | 2.59 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 55.6 | 71.7 | 73.2 | 27.069 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 46.3 | 71.1 | 73.3 | 27.006 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 55.0 | 70.8 | 72.2 | 27.443 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 43.8 | 66.9 | 69.0 | 28.684 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 120.7 | 190.8 | 197.0 | 10.054 | 1 | 4 | 1 | 1 | 105.9 |
| conflict | git | 2/2 | 55.8 | 90.0 | 93.0 | 21.283 | 1 | 3 |  | 1 | 53.0 |

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
| line never moved backwards (replay order, 5 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.8 | 12.8 | 0.2 |
| after_graft_replay | 133.55 | 26.5 | 27.8 | 132.1 |
| after_ops | 136.34 | 50.8 | 50.8 | 132.3 |
| after_concurrent | 136.76 | 50.9 | 50.9 | 132.3 |
| end | 136.99 | 50.9 | 50.9 | 132.3 |

git replay repository: 23.0 MiB (loose objects, no gc).

