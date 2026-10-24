# graft bench: k10-main-3

Started 2026-10-24T10:59:07+00:00, 1687 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 7.76 | 29.27 |
| commit p50 / p95 (ms) | 77.7 / 140.8 | 14.6 / 26.0 |
| land or push p50 / p95 / p99 (ms) | 35.0 / 69.9 / 125.4 | 16.9 / 22.2 / 33.8 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 500.1 | 1,396.4 | 0.36 |
| clone, sparse (`src/synthapp/action_7687`) | 70.6 | 616.5 | 0.11 |
| status (11 changed files) | 13.3 | 6.6 | 2.02 |
| diff, working copy | 13.3 | 4.8 | 2.75 |
| diff, 20 changesets | 8.4 | 6.6 | 1.27 |
| log, 50 | 3.4 | 1.9 | 1.76 |
| log --path `registry.py`, 50 | 3.3 | 8.5 | 0.39 |
| blame `registry.py` | 22.1 | 106.9 | 0.21 |
| sync, 5 behind | 70.6 | 29.1 | 2.43 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 107.4 | 137.3 | 140.0 | 14.184 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 46.8 | 72.7 | 75.0 | 26.381 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 103.9 | 127.8 | 130.0 | 15.281 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 46.0 | 71.4 | 73.6 | 26.823 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 208.6 | 327.7 | 338.3 | 5.862 | 1 | 4 | 1 | 1 | 172.5 |
| conflict | git | 2/2 | 57.6 | 93.0 | 96.1 | 20.564 | 1 | 3 |  | 1 | 54.3 |

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
| line never moved backwards (replay order, 7 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.8 | 12.8 | 0.2 |
| after_graft_replay | 138.0 | 27.6 | 27.9 | 132.1 |
| after_ops | 140.85 | 51.1 | 51.1 | 132.3 |
| after_concurrent | 141.29 | 51.2 | 51.2 | 132.4 |
| end | 141.54 | 51.2 | 51.2 | 132.4 |

git replay repository: 23.0 MiB (loose objects, no gc).

