# graft bench: k1-base-1

Started 2026-10-24T08:42:26+00:00, 137 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 10.76 | 37.81 |
| commit p50 / p95 (ms) | 60.6 / 75.1 | 9.3 / 15.1 |
| land or push p50 / p95 / p99 (ms) | 33.9 / 39.1 / 43.0 | 14.3 / 18.4 / 31.1 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 155.5 | 181.7 | 0.86 |
| clone, sparse (`src/synthapp/audit_1237`) | 55.8 | 110.1 | 0.51 |
| status (11 changed files) | 4.2 | 3.2 | 1.29 |
| diff, working copy | 4.2 | 3.1 | 1.35 |
| diff, 20 changesets | 12.1 | 7.6 | 1.58 |
| log, 50 | 5.5 | 1.8 | 3.12 |
| log --path `registry.py`, 50 | 5.3 | 8.2 | 0.65 |
| blame `registry.py` | 5.8 | 9.2 | 0.63 |
| sync, 5 behind | 60.4 | 22.4 | 2.69 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 66.2 | 87.4 | 89.3 | 22.193 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 33.0 | 50.0 | 51.5 | 38.359 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 64.8 | 86.1 | 88.0 | 22.523 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 33.9 | 52.2 | 53.8 | 36.762 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 126.5 | 206.1 | 213.1 | 9.295 | 1 | 4 | 1 | 1 | 117.6 |
| conflict | git | 2/2 | 39.7 | 62.3 | 64.3 | 30.707 | 1 | 3 |  | 1 | 35.9 |

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
| start | 0.0 | 11.5 | 11.5 | 0.2 |
| after_graft_replay | 12.08 | 23.9 | 24.5 | 16.6 |
| after_ops | 13.42 | 28.5 | 29.8 | 16.8 |
| after_concurrent | 13.64 | 28.4 | 29.8 | 16.8 |
| end | 13.75 | 28.3 | 29.8 | 16.8 |

git replay repository: 2.5 MiB (loose objects, no gc).

