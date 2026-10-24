# graft bench: k1-main-2

Started 2026-10-24T08:45:34+00:00, 134 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | graft-dev, Linux 7.0.0-38-generic |
| CPU | AMD Ryzen 7 7735HS with Radeon Graphics (4 CPUs), 7.7 GiB |
| graft | ``, graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Scale 1,000

1,001 commits (2,312 file operations), final tree 585 files, 2.1 MiB. Stream sha256 `4fcc9e7008322209`, git head `c174cd5e50a8`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 11.15 | 37.11 |
| commit p50 / p95 (ms) | 60.1 / 74.6 | 9.1 / 14.5 |
| land or push p50 / p95 / p99 (ms) | 31.2 / 36.6 / 40.8 | 14.3 / 18.0 / 31.3 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 150.6 | 185.8 | 0.81 |
| clone, sparse (`src/synthapp/audit_1237`) | 56.0 | 125.9 | 0.44 |
| status (11 changed files) | 4.0 | 3.1 | 1.32 |
| diff, working copy | 3.9 | 2.8 | 1.37 |
| diff, 20 changesets | 11.6 | 8.1 | 1.43 |
| log, 50 | 3.0 | 2.0 | 1.5 |
| log --path `registry.py`, 50 | 3.1 | 8.5 | 0.36 |
| blame `registry.py` | 4.9 | 9.3 | 0.53 |
| sync, 5 behind | 54.6 | 23.4 | 2.34 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 60.9 | 81.3 | 83.1 | 23.837 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 32.8 | 48.4 | 49.8 | 39.572 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 58.2 | 80.8 | 82.8 | 23.887 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 31.6 | 47.6 | 49.1 | 40.237 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 127.5 | 207.3 | 214.4 | 9.241 | 1 | 4 | 1 | 1 | 114.9 |
| conflict | git | 2/2 | 39.8 | 62.1 | 64.1 | 30.861 | 1 | 3 |  | 1 | 37.0 |

### Correctness

| Check | Result |
| --- | --- |
| git replay head = fast-import head (byte-identical history) | yes |
| graft tree after replay = model replay (585 files) | yes |
| git tree after replay = model replay | yes |
| graft log = landed sequence (1,001 changesets) | yes |
| final graft tree = expected, byte for byte | yes |
| final git tree = expected, byte for byte | yes |
| final graft tree = final git tree | yes |
| no lost or duplicated lands (1,053 lands, log 1,053) | yes |
| blame of `registry.py` intact (102 lines; agrees with git on 102) | yes |
| blame of `CHANGELOG.md` intact (33 lines; agrees with git on 33) | yes |
| line never moved backwards (replay order, 5 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.6 | 12.6 | 0.2 |
| after_graft_replay | 11.81 | 25.2 | 25.4 | 16.7 |
| after_ops | 13.09 | 28.2 | 31.3 | 16.9 |
| after_concurrent | 13.32 | 28.9 | 31.3 | 16.9 |
| end | 13.38 | 30.6 | 31.3 | 16.9 |

git replay repository: 2.5 MiB (loose objects, no gc).

