# graft bench: k10-main-2

Started 2026-10-24T09:54:26+00:00, 1771 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 7.23 | 30.12 |
| commit p50 / p95 (ms) | 74.2 / 178.8 | 14.6 / 23.4 |
| land or push p50 / p95 / p99 (ms) | 36.5 / 93.9 / 134.5 | 16.9 / 21.9 / 31.5 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 464.1 | 1,383.7 | 0.34 |
| clone, sparse (`src/synthapp/action_7687`) | 69.5 | 587.7 | 0.12 |
| status (11 changed files) | 13.3 | 6.5 | 2.03 |
| diff, working copy | 13.1 | 5.1 | 2.59 |
| diff, 20 changesets | 8.4 | 6.9 | 1.22 |
| log, 50 | 3.4 | 2.0 | 1.7 |
| log --path `registry.py`, 50 | 3.3 | 9.0 | 0.36 |
| blame `registry.py` | 22.7 | 106.2 | 0.21 |
| sync, 5 behind | 62.0 | 30.0 | 2.07 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 103.4 | 128.4 | 130.6 | 15.207 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 46.1 | 69.9 | 72.0 | 27.493 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 109.7 | 133.1 | 135.1 | 14.716 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 42.9 | 65.9 | 67.9 | 29.136 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 208.3 | 324.7 | 335.1 | 5.918 | 1 | 4 | 1 | 1 | 187.5 |
| conflict | git | 2/2 | 56.5 | 91.2 | 94.3 | 20.989 | 1 | 3 |  | 1 | 52.6 |

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
| line never moved backwards (replay order, 6 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 13.0 | 13.0 | 0.2 |
| after_graft_replay | 145.13 | 28.5 | 29.1 | 132.1 |
| after_ops | 147.94 | 51.2 | 51.2 | 132.4 |
| after_concurrent | 148.37 | 51.2 | 51.2 | 132.4 |
| end | 148.62 | 51.2 | 51.2 | 132.4 |

git replay repository: 23.0 MiB (loose objects, no gc).

