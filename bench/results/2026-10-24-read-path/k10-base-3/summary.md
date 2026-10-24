# graft bench: k10-base-3

Started 2026-10-24T11:28:07+00:00, 1609 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 8.19 | 30.22 |
| commit p50 / p95 (ms) | 67.6 / 123.9 | 14.4 / 23.3 |
| land or push p50 / p95 / p99 (ms) | 37.2 / 66.8 / 100.0 | 16.8 / 21.7 / 31.7 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 468.4 | 1,399.3 | 0.33 |
| clone, sparse (`src/synthapp/action_7687`) | 68.6 | 600.9 | 0.11 |
| status (11 changed files) | 14.1 | 6.7 | 2.1 |
| diff, working copy | 13.9 | 4.9 | 2.85 |
| diff, 20 changesets | 11.5 | 6.8 | 1.68 |
| log, 50 | 8.2 | 1.9 | 4.4 |
| log --path `registry.py`, 50 | 7.8 | 8.8 | 0.89 |
| blame `registry.py` | 25.3 | 108.2 | 0.23 |
| sync, 5 behind | 75.5 | 28.9 | 2.61 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 120.3 | 145.0 | 147.2 | 13.502 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 47.0 | 71.8 | 74.0 | 26.748 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 119.6 | 146.8 | 149.2 | 13.319 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 42.7 | 66.2 | 68.3 | 28.992 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 217.6 | 348.5 | 360.2 | 5.503 | 1 | 4 | 1 | 1 | 194.2 |
| conflict | git | 2/2 | 57.5 | 93.9 | 97.2 | 20.38 | 1 | 3 |  | 1 | 55.3 |

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
| start | 0.0 | 12.1 | 12.1 | 0.2 |
| after_graft_replay | 129.85 | 26.5 | 27.1 | 132.2 |
| after_ops | 132.73 | 35.6 | 37.6 | 132.4 |
| after_concurrent | 133.2 | 36.0 | 37.6 | 132.4 |
| end | 133.77 | 36.5 | 37.6 | 132.4 |

git replay repository: 23.0 MiB (loose objects, no gc).

