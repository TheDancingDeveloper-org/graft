# graft bench: k1-main-3

Started 2026-10-24T08:51:47+00:00, 138 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 11.0 | 34.63 |
| commit p50 / p95 (ms) | 60.0 / 74.8 | 9.8 / 20.9 |
| land or push p50 / p95 / p99 (ms) | 31.9 / 37.5 / 42.0 | 15.4 / 19.3 / 30.6 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 158.8 | 189.4 | 0.84 |
| clone, sparse (`src/synthapp/audit_1237`) | 57.1 | 115.8 | 0.49 |
| status (11 changed files) | 4.4 | 3.2 | 1.34 |
| diff, working copy | 4.1 | 3.1 | 1.3 |
| diff, 20 changesets | 11.8 | 8.7 | 1.36 |
| log, 50 | 3.5 | 2.0 | 1.73 |
| log --path `registry.py`, 50 | 3.4 | 8.9 | 0.38 |
| blame `registry.py` | 5.3 | 9.8 | 0.54 |
| sync, 5 behind | 50.5 | 24.4 | 2.06 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 59.6 | 81.2 | 83.1 | 23.844 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 35.6 | 53.4 | 55.0 | 35.95 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 60.1 | 80.3 | 82.1 | 24.128 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 34.3 | 52.2 | 53.7 | 36.764 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 124.4 | 198.7 | 205.3 | 9.649 | 1 | 4 | 1 | 1 | 109.2 |
| conflict | git | 2/2 | 70.6 | 120.8 | 125.3 | 15.797 | 1 | 3 |  | 1 | 94.7 |

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
| start | 0.0 | 12.9 | 12.9 | 0.2 |
| after_graft_replay | 12.54 | 25.6 | 25.6 | 16.6 |
| after_ops | 13.89 | 29.4 | 32.7 | 16.8 |
| after_concurrent | 14.13 | 30.5 | 32.7 | 16.9 |
| end | 14.2 | 31.2 | 32.7 | 16.9 |

git replay repository: 2.5 MiB (loose objects, no gc).

