# graft bench: 2026-10-23-live-awareness

Started 2026-10-23T15:59:30+00:00, 39 s. Seed 2294, 8 concurrent writers x 5 rounds, 5 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | vogt-prod, Linux 7.0.0-38-generic |
| CPU | 13th Gen Intel(R) Core(TM) i7-13700 (24 CPUs), 62.6 GiB |
| graft | `562b075fc2eb`, graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Scale 10

11 commits (276 file operations), final tree 255 files, 0.3 MiB. Stream sha256 `6b3f58da5a0ad7ca`, git head `7450d5abd003`. Result: **pass**.

### History

| Step | graft | git |
| --- | ---: | ---: |
| replay throughput (commits/s, commit + land or push) | 14.72 | 40.49 |
| commit p50 / p95 (ms) | 36.2 / 83.1 | 8.3 / 10.3 |
| land or push p50 / p95 / p99 (ms) | 22.2 / 26.3 / 26.4 | 13.9 / 17.0 / 18.2 |
| import whole history (s) | 1.078 (graft-mirror, 10.21 commits/s) | 0.041 (fast-import + generation) |

### Operations (p50 ms)

| Operation | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| clone, full | 48.7 | 33.2 | 1.46 |
| clone, sparse (`src/synthapp/audit_1237`) | 20.4 | 33.5 | 0.61 |
| status (11 changed files) | 2.5 | 3.2 | 0.79 |
| diff, working copy | 2.4 | 2.3 | 1.07 |
| diff, 10 changesets | 5.2 | 2.1 | 2.48 |
| log, 50 | 3.8 | 1.9 | 2.02 |
| log --path `registry.py`, 50 | 3.3 | 2.0 | 1.6 |
| blame `registry.py` | 3.8 | 1.7 | 2.18 |
| sync, 5 behind | 20.8 | 23.9 | 0.87 |

### Concurrent writers (8 writers x 5 rounds)

| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| clean | graft | 40/40 | 108.5 | 245.8 | 302.9 | 33.517 | 35 | 40 | 0 | 0 |  |
| clean | git | 40/40 | 120.8 | 234.0 | 286.6 | 34.392 | 35 | 180 |  | 0 |  |
| merge3 | graft | 40/40 | 93.4 | 227.1 | 283.8 | 39.732 | 35 | 40 | 0 | 0 |  |
| merge3 | git | 40/40 | 145.3 | 287.5 | 337.1 | 27.083 | 35 | 180 |  | 0 |  |
| conflict | graft | 40/40 | 372.3 | 822.6 | 940.5 | 11.902 | 35 | 215 | 35 | 140 | 64.3 |
| conflict | git | 40/40 | 234.5 | 521.8 | 563.7 | 16.503 | 35 | 180 |  | 140 | 47.0 |

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
| line never moved backwards (replay order, 84 tips polled during concurrent lands) | yes |

### Server

| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |
| --- | ---: | ---: | ---: | ---: |
| start | 0.0 | 11.9 | 11.9 | 0.2 |
| after_import | 0.1 | 18.9 | 19.1 | 1.6 |
| after_graft_replay | 0.3 | 21.4 | 23.1 | 4.9 |
| after_ops | 0.88 | 22.2 | 23.8 | 4.9 |
| after_concurrent | 5.1 | 26.2 | 28.2 | 7.3 |
| end | 5.17 | 26.8 | 28.2 | 7.3 |

git replay repository: 0.2 MiB (loose objects, no gc).

## Live awareness

Deterministic mode: seed 2294, 4 writers x 6 steps, one client call per tick (no races), interleaving round-robin with a seeded jitter. Counts are gated (two runs must match, and graft must match the `git merge-file` oracle); timings are reported only. Result: **pass**.

| Overlap | System | Policy | conflicts at land | caught early | conflict lines resolved | rework (regions rewritten) | land attempts | land refusals (named fewer paths) | probe calls | probe p50 / p95 (ms) | wall (s) | same counts twice | oracle | final tree |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| mid | graft | late | 2 | 0 | 15 | 3 | 8 | 2 (1) | 0 |  /  | 1.633 | yes | yes | yes |
| mid | graft | sync-early | 1 | 2 | 20 | 4 | 6 | 1 (0) | 24 | 17.4 / 42.3 | 1.879 | yes | yes | yes |
| mid | graft | preview | skipped: graft status --help has no --upstream | | | | | | | | | | | |
| mid | graft | drafts | skipped: graft status --help has no --drafts | | | | | | | | | | | |
| mid | git | late | 2 | 0 | 20 | 4 | 7 |  | 0 |  /  | 0.331 | yes |  | yes |
| mid | git | fetch-rebase-early | 1 | 2 | 20 | 4 | 6 |  | 24 | 10.0 / 29.4 | 0.575 | yes |  | yes |

| Overlap | Script | Steps own / region / hot | graft final tree = git | draft overlap (oracle: ticks, pairs) |
| --- | --- | --- | --- | --- |
| mid | `14db314f0529` | 9 / 9 / 6 | yes | 42 of 52, 275 |

### MCP conflict scenario

Two writers change one key of `live/shared_0.py`; the second land conflicts, is resolved by rule and lands, once over `/mcp` and once through the CLI. Resolution over MCP: fallback: graft_read the three sides, merge and resolve locally, land from a new workspace. Calls: graft_land, graft_land, graft_read, graft_workspace, graft_commit, graft_land.

| Path | conflict reported | landed | file = expected |
| --- | --- | --- | --- |
| CLI | live/shared_0.py | yes | yes |
| MCP | live/shared_0.py | yes | yes |

MCP result equals the CLI result: yes.

