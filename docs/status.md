# graft status: what's delivered (2026-10-24)

- **As of:** `main` at `3fe907b` after #170 (the 2026-10-23 run, #82 to
  #119, and the 2026-10-24 run, #120 to #170), under operator decision
  [D22](decisions.md#d22-phase-1-is-a-git-alternative), with D26 to D29.
- **Review:** Vogt WI-2297. Its children are the backlog below.
- **Numbers:** bench WI-2294, scale 1,000, local server
  ([results](../bench/results/2026-10-23-scales-10-1000/summary.md)).

graft works end to end as source control today:
- Import a git repository.
- Clone it whole or sparse; many working copies per host.
- Create a line, commit drafts in a leased workspace, then sync and land
  them, with server-side rebase, line merge (the default) and CAS.
- Preview conflicts with no side effects; get conflicts back in the files,
  or resolve them over MCP or the API with no working copy.
- Read log, diff, blame and show.
- Merge one line into another with recorded merge points, cherry-pick a
  changeset onto another line, and tag a changeset.
- Export to git, or deliver a batch to GitHub as one PR with one commit
  per land (D26), reviewed and rebase-merged by `land-pr` (D28).

A live trial landed agent work on rustTorrent this way (WI-2270, batch PR
rustTorrent #214).

## Delivered

| Area | What works | Evidence |
| --- | --- | --- |
| Model, formats | ids, trees, changesets, frames, tokens, frozen by golden vectors; three-way text merge | G1.1, G1.3, G3.4 (#16) |
| Storage | filesystem object store, SQLite metadata (schema v7: v6 draft paths, v7 line merges), conformance and crash suites | G1.2, G1.5, G1.6 |
| History reads | log (with `--path`), diff, blame, show, cat, ls; derived line index; a log page reads each changeset once | G1.4, G1.10 (#17), G1.13 (#21), WI-2303 (#92) |
| Lines | `graft line create/list`, API and MCP `graft_line_create`; a new line records one fork-point row instead of indexing its history | WI-2301 (#98), WI-2316 (#107) |
| Line merge (D29) | `graft line merge` (gRPC, CLI, MCP `graft_merge`): merge points between lines (schema v7), bounded and memoized merge-base search, `--sync` writes a conflicted merge into the files, `land --merged-from` stamps the result and export writes a `Graft-Merged-From` trailer | WI-2363 (#152), WI-2368 (#157, #158, #159, #164, #166), WI-2372 (#168) |
| Pick and tag | `graft pick` and MCP `graft_pick`: server-side cherry-pick; `graft tag create/list` and MCP `graft_tag`: immutable tags | WI-2364 (#156), WI-2365 (#153) |
| Path merge rule | per-path `merge = line \| none`, so lockfiles conflict instead of line-merging | WI-2339 (#135) |
| Server | gRPC, JSON facade, `/metrics`, health, admin (repo create, token mint and revoke, fsck, backup, gc), per-line policy config; lines default to `direct` | G1.12 (#23), G2.4 (#33), WI-2288 (#76), WI-2298 (#85) |
| Auth | signed bearer tokens with repo and line scopes, revocation; `/mcp` applies the owner-or-admin check on workspace operations | G1.9, G2.4, WI-2315 (#101) |
| Pre-land check (D27) | `graft land` and MCP preview conflicts and draft overlap before landing, with `Rebase` `dry_run` (no writes); no build or test runs | WI-2349 steps 0 and 7 (#125, #132) |
| Attribution | the CLI records the Vogt session, work item and author on drafts and lands; the engine refuses metadata that does not match the caller's token | WI-2350 (#123), WI-2349 step 6 (#131) |
| Client and CLI | clone (full and sparse), status (`--upstream` conflict preview), diff, commit and amend, workspace, sync, land (line merge by default, `--squash`), stack ops, revert, `--json` with schemas, exit codes | G1.11 (#20), G2.5 (#34), G3.5 (#58), WI-2296 (#83), WI-2309 (#99) |
| Workspaces and worktrees | leased workspaces, draft states, holders, GC protection records; many working copies per host, coexisting with git | G2.1 (#27), G3.11 (#59), WI-2313 (#102) |
| Rebase and land | file-level and line-merge rebase (D16), lock-free CAS land, idempotent retry, contention back-off | G2.7 (#31), G3.1 (#51), G3.7 (#64) |
| Conflicts | `get_conflicts` and `resolve` in the engine, gRPC and MCP (`graft_conflicts`, `graft_resolve`) | WI-2312 (#86, #88, #90, #93) |
| Git interop | lossless import with a fidelity check (concurrent blob upload, batched engine writes), git-primary follow, deterministic and full export, graft-primary export with lease pushes, authority switch | G1.8, G1.14 (#22), G2.2 (#28), G3.3 (#52), G3.9 (#63), WI-2308 (#104), WI-2318 (#110) |
| GitHub delivery | `graft-mirror pr-batch`: one PR per batch (D21) with one commit per land (D26); drop a land, cut batches by size or age; lands with no author email get the mirror identity; batch PRs get the standard review and are rebase-merged by `land-pr` (D28). A wrong or empty mirror DB reports `Unmapped`. The per-land `bridge` is removed | WI-2284 (#74), WI-2306 (#105), WI-2302 (#106), WI-2320 (#108), WI-2349 step 5 (#134), WI-2340 (#126), WI-2351 (#129), WI-2361 (#148), WI-2354, WI-2355 (#162) |
| Tags and odd git input | annotated and signed tag objects are kept through import, follow and export; GitHub tags follow as fixed `tags/<name>` refs; tags naming a tree or blob are skipped and reported; oversize blobs are refused in the pre-check, before any write; git metadata path names (`.git`) are refused; export keeps subtrees under a recorded raw tree | WI-2357 (#160), WI-2370 (#167), WI-2342 (#141), WI-2358 (#165), WI-2360 (#163), WI-2359 (#149), WI-2356 (#150) |
| MCP | read and write tools at the server's `/mcp`, and `graft mcp` over stdio | G1.15 (#12), G2.8 (#35), WI-2300 (#87) |
| LSP | hover blame, code lenses, diagnostics, commands, virtual documents | G3.6 (#53) |
| Node daemon | per-host cache daemon forwarding graft methods | G2.3 (#30) |
| GC | dry-run report; the sweep in the server (`admin gc`, scheduler, metrics), off by default | G2.9 (#37), G3.2 (#66), WI-2274 (#103) |
| Dev server | graft-dev VM, compose stack in `deploy/`; pre-flight config check, health gate and rollback; scheduled backups and restore runbook | WI-2251 (#50, #55), WI-2291 (#96), WI-2307 (#97) |
| Adversarial harnesses | `graft-harness`: racing lands under injected faults checked against a model, planted-bug hooks, line merge against `git merge-file`, hostile git repos through import and export, hostile API clients (scopes, tokens, limits, leaks) | WI-2345 (#133, #136, #161), WI-2346 (#144), WI-2347 (#146), WI-2348 (#154) |
| Measurement | pilot harness and crash suite (Rust); the bench against git (`bench/`), its live-awareness mode, and a scale-10 correctness smoke in the merge queue | G1.16 (#26), WI-2294 (#81), WI-2311 (#100), WI-2305 (#94) |
| Bench, 2026-10-24 | real-PR replay (115 estate PRs, graft and git matched on all); branching strategies (trunk, release branches, git flow); batch delivery mode | WI-2338 (#130), WI-2362 (#151), WI-2372 (#168), WI-2352 (#145) |
| Docs | D22 follow-through; D23 (fork-point index, bridge removal); group-commit note; D25 (phase 2), D26, D27, D28, D29; fit assessment; trial-1 write-up | WI-2295 (#89), WI-2321 (#109), WI-2323 (#111), WI-2349 (#122), WI-2353 (#127), WI-2351 (#129), WI-2363 (#152), WI-2336 (#121), WI-2270 (#128) |

## Built but switched off (phase 2, D22)

- The `attested` and `queued` policies.
- Attest.
- The trial attester (WI-2276, WI-2292).
- The CI token exchange (G3.8, #65, #67, #68, #71).
- Post-land checks with auto-revert.
- D25 (post-land checks, bisect and auto-revert on batch lines, design
  §8.7) and §11.4 steps 1 to 4: designed, not started. Phase 2 starts with
  the graft-primary authority switch.

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
| import of the history, graft-mirror end to end (s) | 56 | 1 (fast-import) | |
| import, engine `ImportChangesets` step only, after #110 (s) | 6.2 (was 18.9) | | |

Correctness held at both scales (10 and 1,000):
- The final graft tree, the final git tree and a model replay are equal,
  byte for byte.
- No land was lost or duplicated.
- The line never moved backwards.
- Blame is intact.
- Every real conflict reached the client.

The merge3 row predates WI-2296 (#83): plain `land` was refused 140 times
before `land --sync` merged. `land` now line-merges by default; re-measure
with WI-2304. Import: `graft-mirror import` went 22.5 → 18.5 s with
concurrent blob upload (#104), and the engine's `ImportChangesets` 18.9 → 6.2
s per 1k with batched writes (#110). The 6.2 s is the engine step alone,
so neither it nor the 56 s end-to-end figure is comparable with fast-import's
1 s, which runs locally with no server. The rest is SQLite commits (WI-2323).
Creating a line now writes one fork row, not a copy of its history (#107).

### Read paths against the kill criterion (WI-2341)

The phase-1 kill criterion asks for read paths (`log`, `status`, `sync`)
that are not slower than git; 2x faster than git is a later-phase target
(operator decision, 2026-10-24). On graft-dev (p50 ms, graft / git) after WI-2341:

| | 1k | 10k |
| --- | ---: | ---: |
| log -50 | 3.0 / 1.8 (was 5.1) | 3.0 / 1.8 (was 7.6) |
| status | 3.9 / 3.1 | 13.2 / 7.1 |
| sync, 5 behind | 6.3 / 22.2 (was 9.9) | 74.9 / 27.1 (was 46.3, re-run pending) |
| log --path, 50 | | 2.9 / 9.1 |
| clone, sparse | 6.3 / 83.6 | 70.4 / 585.7 |

log no longer grows with the slice. It is still about 1.6x git: process
start plus a gRPC connect and one RPC is the floor for a fresh process. A
warm node daemon is used when its socket exists. status is a per-file
lstat floor plus index parsing, about 2x git at 10k. sync at 1k is now
3.5x faster than git, but the 10k run needs repeating: see
`changes/WI-2341.md`. Its floor is two fsynced state files.

Against the phase-1 bar, `log -50` (about 1.6x) and `status` at 10k
(about 1.9x) are still slower than git; that gap is WI-2400. The 10k
`log` and import baseline is WI-2304 (#116); 100k is deferred.

### Git flow (branching bench, seed 2362, after WI-2372)

With merge points and `line merge --sync`, graft reports the same
conflicts as git across trunk, release-branch and git flow runs, merges
`release/1.1 → main` clean, and needs at most 3 round trips per merge
or pick, against git's 7 to 22.

## Open (phase 1)

Vogt is the source of truth.

| Item | What |
| --- | --- |
| WI-2341 | read-path re-run on graft-dev (the 10k `sync` figure); results pending |
| WI-2400 | read-path parity: `log` and `status` not slower than git |
| WI-2314 | CI `test` job OOM flake: quarantined (#91); lift the quarantine once `oom_kill` stays at 0 for a day after the Node B memory fix |
| WI-2329 | review leftovers, p4 |

Blocked on the operator:
- WI-2307: the alerts patch (backups and runbook merged in #97).
- WI-2317: tailnet SSH.

Deferred:
- WI-2240, estate token front for Vogt sessions (D10), p4.
- WI-2366, embedded single-binary mode, a future phase (p4).
- The 100k bench.

Parked:
- WI-2214, phase-4 breadth (p4).
- WI-2293, attester, phase 2 (p4).
- D25 and §11.4 steps 1 to 4, phase 2.

Closed as not planned:
- WI-2213, the rustTorrent cutover (D21, D22).
- WI-2193, the Komodo deployment, superseded by the VM.
- WI-2369, export raw-tree cost (not a bug).
