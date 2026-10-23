# graft status: what's delivered (baseline, 2026-10-23)

- **As of:** `main` at `35c3087` (87 commits), after operator decision
  [D22](decisions.md#d22-phase-1-is-a-git-alternative).
- **Review:** Vogt WI-2297. Its children are the backlog below.
- **Numbers:** bench WI-2294, scale 1,000, local server
  ([results](../bench/results/2026-10-23-scales-10-1000/summary.md); the
  harness lands with WI-2294).

graft works end to end as source control today:
- Import a git repository.
- Clone it whole or sparse.
- Commit drafts in a leased workspace, then sync and land them on a line,
  with server-side rebase, line merge and CAS.
- Get conflicts back in the files.
- Read log, diff, blame and show.
- Export to git, or deliver a batch to GitHub as one squashed PR.

A live trial landed agent work on rustTorrent this way (WI-2270, batch PR
rustTorrent #214).

## Delivered

| Area | What works | Evidence |
| --- | --- | --- |
| Model, formats | ids, trees, changesets, frames, tokens, frozen by golden vectors; three-way text merge | G1.1, G1.3, G3.4 (#16) |
| Storage | filesystem object store, SQLite metadata, conformance and crash suites | G1.2, G1.5, G1.6 |
| History reads | log (with `--path`), diff, blame, show, cat, ls; derived line index | G1.4, G1.10 (#17), G1.13 (#21) |
| Server | gRPC, JSON facade, `/metrics`, health, admin (repo create, token mint and revoke, fsck, backup), per-line policy config | G1.12 (#23), G2.4 (#33), WI-2288 (#76) |
| Auth | signed bearer tokens with repo and line scopes, revocation | G1.9, G2.4 |
| Client and CLI | clone (full and sparse), status, diff, commit and amend, workspace, sync, land (`--sync`, `--squash`), stack ops, revert, `--json` with schemas, exit codes | G1.11 (#20), G2.5 (#34), G3.5 (#58) |
| Workspaces and drafts | leased workspaces, draft states, holders, GC protection records | G2.1 (#27), G3.11 (#59) |
| Rebase and land | file-level and line-merge rebase (D16), lock-free CAS land, idempotent retry, contention back-off | G2.7 (#31), G3.1 (#51), G3.7 (#64) |
| Git interop | lossless import with a fidelity check, git-primary follow, deterministic export and full export, graft-primary export with lease pushes, authority switch (rehearsed A→B→A) | G1.8, G1.14 (#22), G2.2 (#28), G3.3 (#52), G3.9 (#63) |
| GitHub delivery | `graft-mirror pr-batch`: one squashed PR per batch (D21) | WI-2284 (#74), used live for rustTorrent #214 |
| MCP | read and write tools (commit, land, sync, workspace) at the server's `/mcp` | G1.15 (#12), G2.8 (#35) |
| LSP | hover blame, code lenses, diagnostics, commands, virtual documents | G3.6 (#53) |
| Node daemon | per-host cache daemon forwarding graft methods | G2.3 (#30) |
| GC | dry-run report; the incremental sweep as a library (off by default) | G2.9 (#37), G3.2 (#66) |
| Dev server | graft-dev VM, compose stack in `deploy/`, deployed by an in-VM runner | WI-2251 (#50, #55) |
| Measurement | pilot harness and crash suite (Rust); the core-VCS bench against git (Python, `bench/`) | G1.16 (#26), WI-2294 |

## Partly done or stubbed (phase 1)

| Gap | State | Item |
| --- | --- | --- |
| `graft land` doesn't line-merge | the client sends `MergeMode::File`, so a same-file land is refused unless `--sync` | WI-2296 (p1) |
| `graft mcp` over stdio | the CLI prints "not implemented yet"; the library has the transport | WI-2300 |
| Creating a line | no API or CLI; the trial used `graft-mirror import --line` | WI-2301 |
| GC sweep in the server | `admin gc` refuses, and the engine's `do_gc` refuses a real (non-dry-run) sweep; no scheduler | WI-2274 |
| `graft-mirror bridge` | a CLI stub for per-land PRs, which D21 rejected | WI-2302 (remove) |

## Built but switched off (phase 2, D22)

- The `attested` and `queued` policies.
- Attest.
- The trial attester (WI-2276, WI-2292).
- The CI token exchange (G3.8, #65, #67, #68, #71).
- Post-land checks with auto-revert.

None of it runs in phase 1, and none of it gets extended until phase 2
starts. The only open item is WI-2293 (p4).

## Measured (bench, scale 1,000)

1,001 commits on a synthetic Python project, 8 concurrent writers, local
server on a 24-CPU host. Each step is a fresh CLI process for both tools.

| | graft | git | graft ÷ git |
| --- | ---: | ---: | ---: |
| replay, commit + land / push (commits/s) | 14.6 | 35.6 | |
| land / push p50 (ms) | 20.4 | 14.5 | 1.4 |
| clone, full (ms) | 89 | 179 | 0.50 |
| clone, sparse (ms) | 18 | 112 | 0.16 |
| status (ms) | 5.6 | 4.2 | 1.34 |
| log, 50 (ms) | 5.7 | 2.3 | 2.51 |
| log --path, 50 (ms) | 4.7 | 9.8 | 0.48 |
| blame (ms) | 5.5 | 12.1 | 0.46 |
| sync, 5 behind (ms) | 21.0 | 22.5 | 0.93 |
| 8 concurrent clean lands, p50 (ms) | 110 | 139 | 0.79 |
| 8 concurrent same-file lands (merge3), p50 (ms) | 225 | 137 | 1.64 |
| import of the history (s) | 56 (graft-mirror) | 1 (fast-import) | |

Correctness held at both scales (10 and 1,000):
- The final graft tree, the final git tree and a model replay are equal,
  byte for byte.
- No land was lost or duplicated.
- The line never moved backwards.
- Blame is intact.
- Every real conflict reached the client.

The merge3 row is slow because of WI-2296: plain `land` was refused 140 times
before `land --sync` merged.

Still to measure: 10k and 100k commits on the quiet VM (WI-2304).

## Backlog (phase 1)

In priority order. Vogt is the source of truth.

| Item | P | What |
| --- | --- | --- |
| WI-2296 | p1 | `graft land` sends `MergeMode::Line` (D16) |
| WI-2299 | p1 | this docs alignment (D22, plan, status) |
| WI-2270 | p1 | finish the trial: write-up, trial-1 config in the repo, PR #214 |
| WI-2294 | p1 | the bench harness (in review) |
| WI-2300 | p2 | `graft mcp` over stdio |
| WI-2301 | p2 | create a line (API, CLI, MCP) |
| WI-2303 | p2 | plain `log` 2.5x slower than git |
| WI-2304 | p2 | bench at 10k and 100k on the VM |
| WI-2291 | p2 | deploy-dev: config can deploy ahead of its image |
| WI-2274 | p3 | wire the GC sweep into the server |
| WI-2305 | p3 | bench correctness smoke in the merge queue |
| WI-2306 | p3 | pr-batch: clear error for an empty mirror DB |
| WI-2307 | p3 | graft-dev backups, restore rehearsal, alerts |
| WI-2308 | p3 | graft-mirror import throughput |
| WI-2302 | p3 | remove the `bridge` stub |
| WI-2240 | p3 | estate token front for Vogt sessions (D10) |

Parked:
- WI-2214, phase-4 breadth (p4).
- WI-2293, attester, phase 2 (p4).

Closed as not planned:
- WI-2213, the rustTorrent cutover (D21, D22).
- WI-2193, the Komodo deployment, superseded by the VM.
