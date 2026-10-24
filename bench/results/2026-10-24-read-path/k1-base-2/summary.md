# graft bench: k1-base-2

Started 2026-10-24T08:48:29+00:00, 148 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 10.31 | 34.47 |
| commit p50 / p95 (ms) | 60.3 / 80.7 | 9.9 / 17.1 |
| land or push p50 / p95 / p99 (ms) | 33.8 / 41.6 / 77.8 | 15.2 / 19.4 / 31.1 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 157.2 | 192.4 | 0.82 |
| clone, sparse (`src/synthapp/audit_1237`) | 55.9 | 117.1 | 0.48 |
| status (11 changed files) | 4.3 | 3.4 | 1.26 |
| diff, working copy | 4.4 | 3.3 | 1.35 |
| diff, 20 changesets | 12.4 | 8.6 | 1.44 |
| log, 50 | 6.0 | 2.1 | 2.84 |
| log --path `registry.py`, 50 | 5.8 | 9.0 | 0.65 |
| blame `registry.py` | 6.7 | 10.3 | 0.65 |
| sync, 5 behind | 65.3 | 24.6 | 2.65 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 66.1 | 88.9 | 91.0 | 21.781 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 36.3 | 54.1 | 55.7 | 35.517 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 67.8 | 87.6 | 89.3 | 22.193 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 35.2 | 52.8 | 54.4 | 36.364 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 131.5 | 218.3 | 226.0 | 8.763 | 1 | 4 | 1 | 1 | 121.1 |
| conflict | git | 2/2 | 42.1 | 65.7 | 67.8 | 29.181 | 1 | 3 |  | 1 | 36.7 |

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
| line never moved backwards (replay order, 6 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 12.1 | 12.1 | 0.2 |
| after_graft_replay | 12.14 | 24.0 | 25.0 | 16.6 |
| after_ops | 13.57 | 27.9 | 29.1 | 16.8 |
| after_concurrent | 13.83 | 29.7 | 30.3 | 16.8 |
| end | 13.94 | 29.5 | 30.3 | 16.8 |

git replay repository: 2.5 MiB (loose objects, no gc).

