# WI-2349 step 8a: phase-1 rollout and bench on graft-dev (2026-10-24)

Design §11.4 step 8a. Steps 0, 5, 6 and 7 (#125, #134, #131, #132) rolled out
to graft-dev and measured. No build, test or check ran anywhere (D22): the
harness only drives `graft`, `graft-mirror` and git.

## Rollout

- Image `ghcr.io/thedancingdeveloper-org/graft@sha256:48e9b703…` (image run
  40064190204, built from `3dbd296`), pinned in `deploy/image.env`.
- Deployed with `deploy/up.sh` from this branch on the VM. Its pre-flight
  (WI-2291) loaded `server.toml` and `mirror.toml` with the new image before
  the swap; the server and mirror came up healthy, and up.sh recorded the
  image as last good. `/readyz` 200, `rusttorrent` serves its refs.
- When this branch lands, `deploy-dev.yml` re-runs up.sh with the same pin,
  which changes nothing.

## Verification on the deployed server

`python3 -m graftbench.phase1 --container graft-dev-server --line verify-8a`
against graft-dev itself, in throwaway repositories (`wi2349-8a`, `wi2349-8b`,
`wi2349-8bb-5`, `wi2349-8c`) on the test line `verify-8a`. `rusttorrent`,
`graft-dev` and their users were not touched. graft-dev's `main` is
git-primary, so agent lands go to the test line (the first attempt, on
`main`, was refused with exit 5 and left `wi2349-8a` with only its seed).

| Check | Result |
| --- | --- |
| identity: token bound to session + work item, matching claim | lands |
| identity: other `VOGT_SESSION_ID` | refused at commit, `session-metadata` |
| identity: other `VOGT_WORK_ITEM` | refused at commit, `work-item-metadata` |
| pre-land check, real conflicts (23 cases) | 23 stopped, nothing sent (line tip unchanged) |
| `--no-check`, same conflicts | 23 refused by the server's land |
| test batch of 5 lands → `pr-batch` | 5 commits, every one with `Graft-Changeset`, `Graft-Session`, `Graft-Work-Item`, `Graft-Batch`; `@` kept; upstream change kept |

The batch PR went to a fake GitHub on loopback and a local bare upstream, not
to GitHub.

## Bench (local server on the VM, same build)

`python3 -m graftbench.phase1 --k 20 --batches 10,50,200,500,501`, release
build of `main` at `292dbf3` (3dbd296 plus the test-only #136) built on the
VM, a fresh graft-server in a temp dir. Raw:
[bench/results/2026-10-24-phase1-8a](../../bench/results/2026-10-24-phase1-8a/).
Another session's 10k core bench was running on the VM at the same time
(load average about 2 on 4 vCPUs), so read the latencies as an upper bound.

### Pre-land check latency added to `graft land`

| Case (k=20) | check p50 / p95 | `--no-check` p50 / p95 | added at p50 |
| --- | ---: | ---: | ---: |
| up to date | 9.2 / 11.0 ms | 6.9 / 8.5 ms | +2.3 ms |
| one land behind (other file) | 14.2 / 16.5 ms | 10.1 / 12.5 ms | +4.0 ms |
| real conflict | 7.4 / 9.7 ms (stopped client-side) | 5.0 / 6.0 ms (server refuses) | +2.4 ms |

On the deployed server (k=20, over the compose network's published port):
up to date 31.6 against 21.1 ms (+10.5), one behind 63.8 against 66.3 ms
(none), conflict 10.9 against 12.4 ms. A k=3 run before it showed +110 ms;
that was warm-up, gone at k=20.

The check is one dry-run rebase plus a draft-overlap read: a few ms, well
under the land itself. It stopped 20/20 local and 23/23 deployed conflicts
before anything was sent.

### Batch of N lands → N commits

One agent token (session + work item), N lands on line `batch`, then one
upstream commit on another line of a file the lands touched, then one
`pr-batch` run (follow, sync, per-land replay, push, open PR).

| N | commits | squash fallback | trailers on every commit | upstream change kept | `pr-batch` time | per land |
| ---: | ---: | --- | --- | --- | ---: | ---: |
| 10 | 10 | no | yes | yes | 0.15 s | 15.0 ms |
| 50 | 50 | no | yes | yes | 0.47 s | 9.3 ms |
| 200 | 200 | no | yes | yes | 1.63 s | 8.2 ms |
| 500 | 500 | no | yes | yes | 3.91 s | 7.8 ms |
| 501 | 500 (+1 waits) | no | yes | no, see below | 2.60 s | 5.2 ms |

- **Squash-fallback rate: 0 of 6 batches** (including the deployed one).
- Export time is linear in N, about 8 ms per land, with a fixed ~0.1 s.
  That is costs following the change (rule 2).
- **501 lands:** the 500 cap holds; one land waits for the next batch. The
  cut is at land 500, before the line's sync onto the moved base, so this
  batch is exported against the base it held there and does not carry the
  later upstream commit, as §11.4 / WI-2340 describes ("exported against
  the base the line held there"). The branch is therefore based on the older
  `main`. Worth a look by oversight: the batch.rs module comment says the
  chain is built "on GitHub's current `main`".
- Server after the whole run: 27 MiB peak RSS, 15.4 CPU s, 15 MiB on disk.

## Not measured

- Squash fallback under real conflicts between lands (the bench's lands
  never conflict with each other); the unit tests cover the path.
- MCP `graft_land` (it shares `graft_mcp::preflight` with the CLI).

## Scratch

On the VM: `~/scratch/p1` (harness copy, logs) and `~/scratch/graft-8a` (the
deploy checkout). The temp dirs were removed. The throwaway repositories
above stay on graft-dev; they hold only generated files.
