# graft bench: k1-base-3

Started 2026-10-24T08:54:46+00:00, 160 s. Seed 2294, 2 concurrent writers x 1 rounds, 9 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

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
| replay throughput (commits/s, commit + land or push) | 10.77 | 20.57 |
| commit p50 / p95 (ms) | 60.4 / 75.2 | 14.5 / 43.6 |
| land or push p50 / p95 / p99 (ms) | 34.0 / 39.4 / 42.5 | 19.5 / 53.6 / 98.6 |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 158.9 | 189.3 | 0.84 |
| clone, sparse (`src/synthapp/audit_1237`) | 55.0 | 127.9 | 0.43 |
| status (11 changed files) | 4.4 | 3.3 | 1.34 |
| diff, working copy | 4.3 | 3.1 | 1.38 |
| diff, 20 changesets | 12.0 | 8.3 | 1.46 |
| log, 50 | 5.6 | 1.9 | 3.01 |
| log --path `registry.py`, 50 | 5.6 | 8.5 | 0.66 |
| blame `registry.py` | 5.7 | 9.5 | 0.6 |
| sync, 5 behind | 55.3 | 23.6 | 2.34 |

### Concurrent writers (2 writers x 1 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 2/2 | 66.5 | 85.2 | 86.9 | 22.783 | 1 | 2 | 0 | 0 |  |
| clean | git | 2/2 | 33.6 | 50.1 | 51.6 | 38.282 | 1 | 3 |  | 0 |  |
| merge3 | graft | 2/2 | 67.6 | 89.0 | 90.9 | 21.819 | 1 | 2 | 0 | 0 |  |
| merge3 | git | 2/2 | 33.4 | 50.1 | 51.6 | 38.324 | 1 | 3 |  | 0 |  |
| conflict | graft | 2/2 | 130.1 | 212.6 | 219.9 | 9.007 | 1 | 4 | 1 | 1 | 119.4 |
| conflict | git | 2/2 | 40.4 | 63.7 | 65.8 | 30.088 | 1 | 3 |  | 1 | 35.7 |

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
| start | 0.0 | 12.1 | 12.1 | 0.2 |
| after_graft_replay | 12.04 | 24.2 | 24.5 | 16.6 |
| after_ops | 13.4 | 28.6 | 30.0 | 16.8 |
| after_concurrent | 13.62 | 28.2 | 30.0 | 16.8 |
| end | 13.73 | 28.5 | 30.0 | 16.8 |

git replay repository: 2.5 MiB (loose objects, no gc).

