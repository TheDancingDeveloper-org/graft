# graft bench: k10-base-2

Started 2026-10-24T10:25:34+00:00, 1880 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 7.62 | 19.6 |
| commit p50 / p95 (ms) | 75.5 / 148.8 | 17.1 / 63.3 |
| land or push p50 / p95 / p99 (ms) | 37.5 / 73.5 / 109.3 | 18.2 / 43.2 / 54.5 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 525.0 | 1,466.1 | 0.36 |
| clone, sparse (`src/synthapp/action_7687`) | 83.0 | 715.6 | 0.12 |
| status (11 changed files) | 14.6 | 7.1 | 2.06 |
| diff, working copy | 14.7 | 5.1 | 2.87 |
| diff, 20 changesets | 11.7 | 7.0 | 1.68 |
| log, 50 | 8.3 | 2.1 | 3.99 |
| log --path `registry.py`, 50 | 8.0 | 9.3 | 0.86 |
| blame `registry.py` | 27.8 | 113.1 | 0.25 |
| sync, 5 behind | 58.8 | 31.6 | 1.86 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 76.0 | 88.6 | 89.8 | 22.099 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 52.1 | 79.2 | 81.6 | 24.212 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 85.9 | 104.4 | 106.0 | 18.749 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 46.1 | 70.3 | 72.5 | 27.276 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 153.0 | 240.8 | 248.6 | 7.973 | 1 | 4 | 1 | 1 | 138.3 |
| conflict | git | 2/2 | 58.5 | 95.7 | 99.0 | 20.001 | 1 | 3 |  | 1 | 53.7 |

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
| start | 0.0 | 12.2 | 12.2 | 0.2 |
| after_graft_replay | 133.52 | 26.7 | 27.3 | 132.3 |
| after_ops | 136.54 | 37.4 | 38.6 | 132.4 |
| after_concurrent | 136.99 | 39.8 | 39.8 | 132.5 |
| end | 137.65 | 40.1 | 40.1 | 132.5 |

git replay repository: 23.0 MiB (loose objects, no gc).

