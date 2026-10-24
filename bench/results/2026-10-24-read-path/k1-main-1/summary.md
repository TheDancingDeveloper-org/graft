# graft bench: k1-main-1

Started 2026-10-24T08:39:00+00:00, 145 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 10.57 | 38.4 |
| commit p50 / p95 (ms) | 60.9 / 80.3 | 9.3 / 15.9 |
| land or push p50 / p95 / p99 (ms) | 31.5 / 39.5 / 73.9 | 14.2 / 18.1 / 30.7 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 312.7 | 243.5 | 1.28 |
| clone, sparse (`src/synthapp/audit_1237`) | 157.6 | 201.2 | 0.78 |
| status (11 changed files) | 4.1 | 3.2 | 1.27 |
| diff, working copy | 4.0 | 2.9 | 1.36 |
| diff, 20 changesets | 11.3 | 7.9 | 1.44 |
| log, 50 | 3.3 | 1.8 | 1.83 |
| log --path `registry.py`, 50 | 3.0 | 8.4 | 0.36 |
| blame `registry.py` | 4.8 | 9.2 | 0.52 |
| sync, 5 behind | 58.3 | 23.4 | 2.49 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 59.0 | 80.7 | 82.6 | 23.967 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 33.8 | 50.5 | 52.0 | 38.029 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 61.5 | 83.1 | 85.0 | 23.29 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 34.0 | 51.0 | 52.5 | 37.61 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 117.6 | 193.2 | 199.9 | 9.913 | 1 | 4 | 1 | 1 | 108.6 |
| conflict | git | 2/2 | 41.1 | 64.5 | 66.6 | 29.67 | 1 | 3 |  | 1 | 36.8 |

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
| line never moved backwards (replay order, 4 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.8 | 12.8 | 0.2 |
| after_graft_replay | 11.87 | 25.8 | 26.0 | 16.6 |
| after_ops | 13.16 | 32.1 | 32.1 | 16.8 |
| after_concurrent | 13.39 | 30.6 | 32.3 | 16.8 |
| end | 13.46 | 30.9 | 32.3 | 16.8 |

git replay repository: 2.5 MiB (loose objects, no gc).

