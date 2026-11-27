# Project status

Last reviewed: 2026-10-24.

graft is usable today as source control. It covers the whole loop an agent or
developer needs: import a git repository, clone it whole or sparse, commit
drafts in a leased workspace, then sync and land them with server-side rebase,
line merge and compare-and-swap. History reads, line merges, cherry-picks,
tags and conflict handling are built, and a repository can be exported back to
git or delivered to GitHub as a batch pull request with one commit per land.

The current phase positions graft as a git alternative with low friction
([decisions](decisions.md#d22-phase-1-is-a-git-alternative)). Lands are
`direct`: nothing runs in the land path. Verification, attestation-based
policies and post-land checks are built in part but switched off; see
[Planned / not yet built](#planned--not-yet-built). For direction, see the
[roadmap](roadmap.md).

## Capabilities

| Area | What works |
| --- | --- |
| Model and formats | Ids, trees, changesets, frames and tokens, frozen by golden vectors; three-way text merge |
| Storage | Filesystem object store and SQLite metadata (schema v7); conformance and crash suites |
| History reads | `log` (with `--path`), `diff`, `blame`, `show`, `cat`, `ls`; a derived line index; a log page reads each changeset once |
| Lines | `graft line create` and `list`, also over the API and MCP (`graft_line_create`); a new line records one fork-point row instead of indexing its history |
| Line merge | `graft line merge` (gRPC, CLI, MCP `graft_merge`): merge points between lines, bounded and memoized merge-base search, `--sync` writes a conflicted merge into the files, `land --merged-from` stamps the result, and export writes a `Graft-Merged-From` trailer |
| Pick and tag | `graft pick` and MCP `graft_pick` (server-side cherry-pick); `graft tag create` and `list` and MCP `graft_tag` (immutable tags) |
| Path merge rule | Per-path `merge = line \| none`, so lockfiles conflict instead of line-merging |
| Rebase and land | File-level and line-merge rebase, lock-free compare-and-swap land, idempotent retry, contention back-off |
| Conflicts | Conflicts come back in the files; `get_conflicts` and `resolve` in the engine, gRPC and MCP (`graft_conflicts`, `graft_resolve`) let an agent resolve them with no working copy |
| Pre-land check | `graft land` and MCP preview conflicts and draft overlap before landing, using a `Rebase` `dry_run` that writes nothing; no build or test runs |
| Client and CLI | Clone (full and sparse), `status` (`--upstream` conflict preview), `diff`, `commit` and `amend`, workspace, `sync`, `land` (line merge by default), stack operations, `revert`, `--json` with schemas, documented exit codes. `land --squash` is not built; use `stack squash` then `land` |
| Workspaces | Leased workspaces, draft states, holders and GC protection records; many working copies per host, coexisting with git |
| Server | gRPC, JSON facade, `/metrics`, health, admin (repo create, token mint and revoke, fsck, backup, gc) and per-line policy config; lines default to `direct` |
| Auth | Signed bearer tokens with repo and line scopes, and revocation; `/mcp` applies an owner-or-admin check on workspace operations |
| Attribution | The CLI records the session, work item and author on drafts and lands, and keeps them through sync, conflict resolution and land rebase. The author defaults to `GRAFT_AUTHOR_*`, then `$USER` or `$LOGNAME` with no email. The server refuses metadata that does not match a session-bound token and accepts it from an unbound one |
| Git interop | Lossless import with a fidelity check, following a git remote, deterministic and full export, graft-primary export with lease pushes, and an authority switch |
| GitHub delivery | One pull request per batch with one commit per land, cut by a server request (`graft repo batch cut` or `CutBatch`) that the mirror service claims. Lands can be dropped, and batches cut by size or age. Lands with no author email get the mirror identity. A wrong or empty mirror database reports `Unmapped` |
| Tags and unusual git input | Annotated and signed tag objects are kept through import, follow and export; GitHub tags follow as fixed `tags/<name>` refs; tags naming a tree or blob are skipped and reported; oversize blobs are refused in the pre-check, before any write; git metadata path names (`.git`) are refused; export keeps subtrees under a recorded raw tree |
| MCP | Read and write tools at the server's `/mcp`, and `graft mcp` over stdio |
| LSP | Hover blame, code lenses, diagnostics, commands, virtual documents |
| Node daemon | A per-host cache daemon that forwards graft methods |
| Garbage collection | Dry-run report; a sweep in the server (`admin gc`, scheduler, metrics), off by default |
| Dev server | Compose stack in `deploy/`, with a pre-flight config check, health gate and rollback; scheduled backups and a restore runbook |
| Testing | `graft-harness` adversarial harnesses: racing lands under injected faults checked against a model, planted-bug hooks, line merge against `git merge-file`, hostile git repositories through import and export, and hostile API clients (scopes, tokens, limits, leaks). A pilot harness and crash suite; a scale-10 correctness smoke test in the merge queue |

A live trial landed agent work on a real Rust project through the batch
pull-request path; see the [trial write-ups](internal/trials/).

## Planned / not yet built

These are built in part or designed, and switched off or not started. They are
the subject of the [roadmap](roadmap.md).

- **Attestation-based policies.** The `attested` and `queued` land policies,
  attestations, and a trial attester are built but switched off.
- **CI token exchange.** A CI job can trade its identity token for a short
  attest-scoped graft token. Built, switched off.
- **Post-land checks with automatic revert.** Post-land checks, bisect and
  auto-revert on batch lines are designed
  ([decisions](decisions.md#d25-post-land-checks-on-the-batch-line)) but not
  started.
- **Graft-primary repositories.** Switching a repository's authority from its
  git host to graft is the first step of the next phase.
- **`land --squash`.** Not built; use `stack squash` then `land`.
- **Embedded single-binary mode.** Deferred to a later phase.
- **100k-commit benchmark.** Deferred.
- **Parity with git on read paths.** Closing the remaining gap to git is a
  later-phase goal; see [Performance](#performance).

## Performance

graft's benchmarks compare it against git on the same workload. The full
method, results and charts are in [benchmarks](guide/benchmarks.md); the
latest runs are summarised here.

### Benchmark summary (scale 1,000)

1,001 commits on a synthetic Python project, 8 concurrent writers, a local
server on a 24-CPU host. Each step is a fresh CLI process for both tools.
[Full results](../bench/results/2026-10-23-scales-10-1000/summary.md).

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
| import, engine `ImportChangesets` step only (s) | 6.2 (was 18.9) | | |

Correctness held at both scales (10 and 1,000):

- The final graft tree, the final git tree and a model replay are equal, byte
  for byte.
- No land was lost or duplicated.
- The line never moved backwards.
- Blame is intact.
- Every real conflict reached the client.

Notes on reading the table:

- The same-file (merge3) row predates the change that made `land` line-merge
  by default; before it, plain `land` was refused 140 times before
  `land --sync` merged. It has not been re-measured.
- Import figures are not comparable with fast-import. The 6.2 s is the engine
  step alone, and the 56 s end-to-end figure includes the server, while
  fast-import runs locally with no server. The remainder is mostly SQLite
  commits.
- Creating a line now writes one fork row rather than a copy of its history.

### Read-path floor

Read-path speed is a no-regression floor, not a current target
([decisions](decisions.md#d31-phase-1-measures-agent-friction-not-read-latency)).
No read path may be slower than the twelve-run medians below (p50 ms, graft /
git, recorded on 2026-10-24), and every operation stays proportional to the
size of the change. Being 2x faster than git is a later-phase target.

| | 1k | 10k |
| --- | ---: | ---: |
| log -50 | 3.3 / 2.0 | 3.4 / 1.9 |
| status | 4.1 / 3.2 | 13.3 / 6.5 |
| sync, 5 behind | 54.6 / 23.4 | 70.6 / 29.1 |

`log --path` and sparse clone are not part of the floor, since they already
beat git.

- `log` no longer grows with the slice. It is still slower than git: process
  start plus a gRPC connect and one RPC is the floor for a fresh process. A
  warm node daemon is used when its socket exists.
- `status` is bounded by a per-file `lstat` plus index parsing.
- `sync` is bounded by two fsynced state files (`.graft/index` and
  `.graft/state`).
- The remaining gap to git (about 1.7x on `log -50` and about 2.0x on `status`
  at 10k) is a later-phase goal.

### Git flow (branching benchmark)

With merge points and `line merge --sync`, graft reports the same conflicts as
git (18 of 18 blocks) and merges `release/1.1` into `main` cleanly. A clean
merge takes 1 round trip and a conflicted merge 3; git takes 2 per merge.
Picks still cost 7 round trips through the carry path in the tracked and
merge-base variants. The committed
`bench/results/2026-10-24-branching/summary.md` predates the line-merge work
and will be regenerated.
