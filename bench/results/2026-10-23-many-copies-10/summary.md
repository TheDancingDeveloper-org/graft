# graft bench: many copies, scale 10

Started 2026-10-23T16:12:03+00:00, 6 s. Seed 2294, 8 concurrent writers x 5 rounds, 5 iterations per operation. Server: local. All lines `direct` (phase 1, D22): no checks in the land path.

| Machine | |
| --- | --- |
| Host | vogt-prod, Linux 7.0.0-38-generic |
| CPU | 13th Gen Intel(R) Core(TM) i7-13700 (24 CPUs), 62.6 GiB |
| graft | `867431da098f` (dirty), graft 0.0.0 (protocol 0.1) |
| git | git version 2.53.0 |
| Python | 3.14.4 |

## Many working copies, scale 10: pass

Repository: 255 files, 0.3 MiB; sparse slice `src/synthapp/audit_1237`: 2 files, 0.00 MiB. The N graft clones start at once; git is one clone plus N-1 `git worktree add`, one at a time, sharing one object store.

| System | Shape | N | Copies ok | Wall s | Clone p50 ms | Clone p95 ms | Files MiB | Cache MiB | Other state MiB | Total MiB | Per copy MiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| graft | full | 1 | yes | 0.08 | 80.6 | 80.6 | 0.3 | 0.1 | 0.0 | 0.5 | 0.5 |
| git | full | 1 | yes | 0.04 | 40.4 | 40.4 | 0.3 | 0.0 | 0.2 | 0.5 | 0.5 |
| graft | full | 8 | yes | 0.14 | 122.7 | 131.9 | 2.3 | 1.2 | 0.3 | 3.8 | 0.5 |
| git | full | 8 | yes | 0.13 | 13.4 | 28.1 | 2.3 | 0.0 | 0.4 | 2.7 | 0.3 |
| graft | full | 32 | yes | 0.59 | 548.8 | 577.6 | 9.2 | 4.8 | 1.3 | 15.3 | 0.5 |
| git | full | 32 | yes | 0.43 | 11.6 | 16.2 | 9.2 | 0.0 | 1.0 | 10.2 | 0.3 |
| graft | sparse | 1 | yes | 0.03 | 28.5 | 28.5 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| git | sparse | 1 | yes | 0.03 | 29.3 | 29.3 | 0.0 | 0.0 | 0.1 | 0.1 | 0.1 |
| graft | sparse | 8 | yes | 0.03 | 29.9 | 30.4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| git | sparse | 8 | yes | 0.08 | 6.7 | 22.8 | 0.0 | 0.0 | 0.3 | 0.3 | 0.0 |
| graft | sparse | 32 | yes | 0.05 | 33.5 | 37.4 | 0.1 | 0.1 | 0.0 | 0.1 | 0.0 |
| git | sparse | 32 | yes | 0.27 | 7.3 | 9.0 | 0.2 | 0.0 | 0.9 | 1.1 | 0.0 |

graft's cache is per clone (`.graft/cache`), so it grows with N; git's object store is shared, counted once under other state. Server storage growth over the run: 0 bytes. graft-node: not run: graft-node needs the server's public key, which graft-server admin does not print.

