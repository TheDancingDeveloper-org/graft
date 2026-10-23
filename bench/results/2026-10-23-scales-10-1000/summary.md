# graft bench: scales 10 and 1,000 (local server)

Started 2026-10-23T08:35:49+00:00, 368 s. Seed 2294, 8 concurrent writers x 5 rounds, 5 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | vogt-prod, Linux 7.0.0-38-generic |
| CPU | 13th Gen Intel(R) Core(TM) i7-13700 (24 CPUs), 62.6 GiB |
| graft | `35c308759e8f`, graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Scale 10

11 commits (276 file operations), final tree 255 files, 0.3 MiB. Stream sha256 `6b3f58da5a0ad7ca`, git head `7450d5abd003`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 15.03 | 35.69 |
| commit p50 / p95 (ms) | 36.2 / 82.1 | 12.7 / 15.5 |
| land or push p50 / p95 / p99 (ms) | 20.3 / 32.8 / 33.0 | 13.8 / 15.0 / 15.2 |
| import whole history (s) | 1.191 (graft-mirror, 9.23 commits/s) | 0.045 (fast-import + generation) |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 52.0 | 33.0 | 1.58 |
| clone, sparse (`src/synthapp/audit_1237`) | 19.7 | 33.2 | 0.59 |
| status (11 changed files) | 2.6 | 2.2 | 1.16 |
| diff, working copy | 2.5 | 2.3 | 1.1 |
| diff, 10 changesets | 4.3 | 1.9 | 2.21 |
| log, 50 | 3.2 | 1.5 | 2.15 |
| log --path `registry.py`, 50 | 2.7 | 1.4 | 1.9 |
| blame `registry.py` | 2.7 | 1.5 | 1.73 |
| sync, 5 behind | 17.9 | 21.4 | 0.84 |

### Concurrent writers (8 writers x 5 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 40/40 | 137.8 | 342.9 | 381.5 | 27.246 | 35 | 40 | 0 | 0 |  |
| clean | git | 40/40 | 170.6 | 307.1 | 359.2 | 24.954 | 35 | 180 |  | 0 |  |
| merge3 | graft | 40/40 | 207.9 | 347.5 | 503.5 | 20.116 | 35 | 180 | 140 | 0 |  |
| merge3 | git | 40/40 | 140.5 | 286.5 | 288.8 | 28.519 | 35 | 180 |  | 0 |  |
| conflict | graft | 40/40 | 480.8 | 5,317.2 | 5,599.3 | 4.446 | 35 | 215 | 35 | 140 | 67.6 |
| conflict | git | 40/40 | 219.3 | 424.6 | 553.2 | 17.978 | 35 | 180 |  | 140 | 48.7 |

### Correctness

| Check | Result |
| --- | --- |
| git replay head = fast-import head (byte-identical history) | yes |
| graft tree after replay = model replay (255 files) | yes |
| git tree after replay = model replay | yes |
| graft log = landed sequence (11 changesets) | yes |
| graft-mirror import tree = model | yes |
| final graft tree = expected, byte for byte | yes |
| final git tree = expected, byte for byte | yes |
| final graft tree = final git tree | yes |
| no lost or duplicated lands (157 lands, log 157) | yes |
| blame of `registry.py` intact (54 lines; agrees with git on 54) | yes |
| blame of `CHANGELOG.md` intact (5 lines; agrees with git on 5) | yes |
| line never moved backwards (replay order, 99 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 11.3 | 11.3 | 0.2 |
| after_import | 0.08 | 18.4 | 18.4 | 1.6 |
| after_graft_replay | 0.25 | 20.9 | 21.8 | 4.9 |
| after_ops | 0.74 | 21.8 | 24.0 | 5.0 |
| after_concurrent | 5.79 | 24.1 | 27.7 | 7.3 |
| end | 5.82 | 24.7 | 27.7 | 7.3 |

git replay repository: 0.2 MiB (loose objects, no gc).

## Scale 1,000

1,001 commits (2,312 file operations), final tree 585 files, 2.1 MiB. Stream sha256 `4fcc9e7008322209`, git head `c174cd5e50a8`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 6.4 | 19.59 |
| commit p50 / p95 (ms) | 86.9 / 185.5 | 12.4 / 55.0 |
| land or push p50 / p95 / p99 (ms) | 47.4 / 95.5 / 236.4 | 16.0 / 76.8 / 131.8 |
| import whole history (s) | 38.211 (graft-mirror, 26.2 commits/s) | 0.839 (fast-import + generation) |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 276.2 | 574.2 | 0.48 |
| clone, sparse (`src/synthapp/audit_1237`) | 78.6 | 331.6 | 0.24 |
| status (11 changed files) | 13.6 | 9.5 | 1.43 |
| diff, working copy | 9.9 | 9.0 | 1.1 |
| diff, 20 changesets | 32.3 | 18.8 | 1.72 |
| log, 50 | 15.6 | 6.8 | 2.29 |
| log --path `registry.py`, 50 | 12.2 | 21.9 | 0.56 |
| blame `registry.py` | 14.3 | 26.8 | 0.53 |
| sync, 5 behind | 76.6 | 98.9 | 0.77 |

### Concurrent writers (8 writers x 5 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 40/40 | 445.3 | 1,109.7 | 1,179.1 | 8.725 | 35 | 40 | 0 | 0 |  |
| clean | git | 40/40 | 202.7 | 401.0 | 406.8 | 21.063 | 35 | 180 |  | 0 |  |
| merge3 | graft | 40/40 | 883.5 | 1,509.9 | 1,764.4 | 5.379 | 35 | 180 | 140 | 0 |  |
| merge3 | git | 40/40 | 193.9 | 357.6 | 381.3 | 22.366 | 35 | 180 |  | 0 |  |
| conflict | graft | 40/40 | 1,540.5 | 2,936.2 | 3,207.7 | 3.021 | 35 | 215 | 35 | 140 | 320.2 |
| conflict | git | 40/40 | 279.9 | 1,338.2 | 1,424.2 | 12.065 | 35 | 180 |  | 140 | 44.9 |

### Correctness

| Check | Result |
| --- | --- |
| git replay head = fast-import head (byte-identical history) | yes |
| graft tree after replay = model replay (585 files) | yes |
| git tree after replay = model replay | yes |
| graft log = landed sequence (1,001 changesets) | yes |
| graft-mirror import tree = model | yes |
| final graft tree = expected, byte for byte | yes |
| final git tree = expected, byte for byte | yes |
| final graft tree = final git tree | yes |
| no lost or duplicated lands (1,147 lands, log 1,147) | yes |
| blame of `registry.py` intact (103 lines; agrees with git on 103) | yes |
| blame of `CHANGELOG.md` intact (33 lines; agrees with git on 33) | yes |
| line never moved backwards (replay order, 121 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 5.82 | 24.7 | 27.7 | 7.3 |
| after_import | 8.24 | 33.8 | 33.8 | 18.1 |
| after_graft_replay | 19.94 | 39.4 | 39.4 | 31.2 |
| after_ops | 21.53 | 48.6 | 49.3 | 31.3 |
| after_concurrent | 29.83 | 51.6 | 51.6 | 33.8 |
| end | 30.03 | 54.8 | 54.8 | 33.8 |

git replay repository: 2.5 MiB (loose objects, no gc).

