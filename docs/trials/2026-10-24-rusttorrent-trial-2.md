# rustTorrent trial 2: seven agent lands with same-file overlap (2026-10-24)

WI-2403. Opus worker sessions delivered real rustTorrent backlog items
through graft on the integration line `trial-2`, with no git branches or PR
chains. Three pairs of items were chosen to edit the same files. graft-mirror
`pr-batch` sent the line to GitHub as a batch PR with one commit per land
(D26), which got a Sonnet review, a DeepSeek second review and `land-pr`
(D28).

Scope follows D22: graft ran no build, test or check. Workers ran
rustTorrent's checks themselves, and rustTorrent's own CI ran on the batch
PR.

**Result:** all 7 items and 2 fix-forwards merged on rustTorrent `main` as 9 commits, one per land (#216, 89b3c59). The first batch PR (#215) was closed unmerged after the merge queue's gui-e2e found 4 regressions, all from one land (#199). graft held up: 2 file conflicts in 7 lands, each resolved in one extra round trip, with line merge handling every other same-file overlap. The gaps are in attribution, client ergonomics and tooling, not in the core.

## Setup

| Item | Value |
| --- | --- |
| Server | graft-dev VM. Wave 1 (lands 1–3, 11:15–11:17Z) ran on the #140 image. #175 (graft `main` 3e8ba5d, image `sha256:9ab165f2…`) deployed at about 11:21Z, and every later land ran on it. Mirror lag was 0 after the redeploy |
| Repository | `rusttorrent`, followed from GitHub `TheDancingDeveloper-org/rustTorrent` |
| Line | `trial-2`, forked at GitHub `main` 7e112d7 (trial 1's batch), policy `direct` (the default line policy) |
| Agents | 10 Opus 5.5 Vogt worker sessions (8 item workers, 2 fix-forwards), at most 4 at once, 4 agent tokens (`trial/mint-agent-tokens.sh`, `GRAFT_TRIAL_LINE=trial-2`, #174), graft CLI built from 3e8ba5d |
| GitHub | ruleset 25363247 gets D28's setup: a `pull_request` rule allowing rebase, plus an OrganizationAdmin bypass (operator-approved). The merge queue rebases |
| Export | `graft-mirror pr-batch trial-2 --base main`, a one-off container with a temporary GitHub token file |

## Items

The operator approved WI-2153, WI-2155 and WI-2157 (logging), #195 and #196
(sidebar filters), and #198 and #199 (table columns). The logging items had
already been delivered by trial 1 (#214) but their Vogt items were never
closed. The workers found this on the line. WI-2153 and WI-2157 were closed,
WI-2155 got one real follow-up land, and the operator swapped in #202 and
#203 (settings tabs).

| # | Land | Item | Overlaps with | Syncs | Lands | Conflicts | Pre-land check stops |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: |
| 1 | c9c385f | WI-2155 follow-up: drop removed torrents from the phase map | — | 1 | 1 | 0 | 0 |
| 2 | ceb6e8b | #199 default columns (WI-2406) | #198 | 1 | 1 | 0 | 0 |
| 3 | c5508d3 | #198 limit columns (WI-2405) | #199 | 1 | 1 | 0 (line merge of 4 files on sync) | 0 |
| 4 | c129eb5 | #196 tracker error/warning groups (WI-2408) | #195 | 1 | 1 | 0 | 0 |
| 5 | cbb52e8 | #195 13 status filters (WI-2407) | #196 | 2 | 2 | 1 (`torrentFilters.ts` imports; first land refused by the server) | 0 (stale CLI) |
| 6 | c351a78 | #202 Behaviour tab (WI-2409) | #203 | 1 | 1 | 0 | 0 |
| 7 | c2ee920 | #203 Downloads tab (WI-2410) | #202 | 1 | 1 | 1 (`FileSelectionModal.tsx`; 3 other shared files line-merged) | 0 |
| 8 | cc14243 | fix-forward: #196 review blocker | — | 1 | 1 | 0 | 0 |
| 9 | c70989a | fix-forward: gui-e2e regressions caused by #199 | — | 1 | 1 | 0 | 0 (skipped: token scope, WI-2417) |


## Measurements

### Conflicts and round trips

- 7 item lands: 2 hit a file conflict (29%). Both were resolved by the agent
  in one extra round trip. No land was contended (exit 4) or unavailable
  (exit 7).
- Line merge resolved every other same-file overlap with no agent action: #198
  on top of #199 (4 shared files), and 3 of #203's 4 files shared with #202.
- Round trips per land: 1.0 for 5 lands, 2.0 for #195 and #203. Mean 1.29.
  The git per-PR equivalent is at least 2 (push, then rebase when `main`
  moves) for any PR that waits behind another (bench WI-2372).

### Pre-land check (D27)

- **Not measured for the item lands.** The 7 item lands ran a stale pre-D27
  CLI (WI-2414), so no client-side check ran. #195's first land was refused by
  the server's own conflict check (exit 3), with the same cost: one extra round
  trip, nothing written. The e2e fix (c70989a) ran the correct CLI, but the
  check was skipped because the trial token lacks the drafts scope (WI-2417).
  Trial 3 must measure D27 with fixed tokens.
- The worker logs for #195, #199 and #202 say "pre-land check". The CLI they
  ran had no pre-land check: its `graft land --help` has no `--no-check` flag,
  checked at 20:13Z. So those lines report the server's land outcome (refused
  on conflict, or rebased cleanly), not a D27 result. This note counts them
  that way.

### Land-to-main time

Land time (from the worker logs) to the merge of #216 at 20:32:38Z, 9 lands:

| | p50 | p90 | max |
| --- | ---: | ---: | ---: |
| all 9 lands | 0.69 h | 9.29 h | 9.29 h |
| second wave (6 lands from 19:50Z) | 0.66 h | 0.71 h | 0.71 h |

The first 3 lands (11:15–11:17Z) waited until the second wave started at 19:48Z, after the operator chose replacements for the already-delivered logging items. That wait was not graft or review time. Batch timing: #215 opened 19:56Z and was closed 20:07:59Z after gui-e2e failed. The recut #216 opened 20:17:13Z and merged 20:32:38Z (15.4 min).

### Batch PR review

| PR | Reviewer | Verdict | Findings | Pinned to one land? |
| --- | --- | --- | --- | --- |
| #215 | Sonnet | clean | tracker-error filter not reactive (nit), unbounded per-torrent polling (#196 30 s, #198 10 s), persisted filter can show an empty list | yes: #196, #198. The filter note came from #195 and #196 together |
| #215 | DeepSeek (second) | 1 blocker | tracker error/warning filters don't recompute on health updates | yes: #196 (c129eb5) |
| #215 | merge queue gui-e2e | 4 failures | Name column squeezed to 0 px by the new default columns: context menu, recheck, settings and sort tests | yes: all 4 from #199 (ceb6e8b) |
| #216 | Sonnet | clean | 7 original commits patch-identical to #215 (`git patch-id`); both fixes confirmed; nits only | yes |

Every finding but one was pinned to a single land by its commit and Graft-Changeset trailer; Sonnet's empty-filter note on #215 spans #195 and #196. One-commit-per-land (D26) made that possible; trial 1's squashed commit could not.

### Authorship and history on GitHub

- rustTorrent `main` gained 9 commits from #216, one per land, in land order, rebased by the merge queue (the ruleset's REBASE method). The D28 ruleset change (pull_request rule plus admin bypass) was applied but not needed: land-pr went through the queue.
- Every commit has `Graft-Changeset`, `Graft-Work-Item` and `Graft-Batch` trailers.
- Only c70989a has `Graft-Session`, and every commit is authored `graft <graft@localhost>`. Root cause (WI-2414): `trial/install-cli.sh` honoured an inherited `CARGO_TARGET_DIR` and installed a stale pre-WI-2350 CLI under a fresh stamp, so the 8 earlier lands carried no session and the default author fell back to `graft@localhost` (`$USER` is unset in Vogt sessions, so the name defaulted to `graft`). Fixed in graft #176 (installer target dir; author is env-only, and an empty email gives the mirror identity, WI-2412). The correct CLI was installed at 20:13Z, before the last land.

### Fix-forward and revert

- 2 fix-forwards, 0 reverts. cc14243 fixed the DeepSeek blocker on #196 (session started 19:59:23Z, landed by 20:00:44Z). c70989a fixed the 4 gui-e2e failures from #199 (session started 20:08:01Z, landed by 20:16:56Z).
- Neither fix could join the open batch: pr-batch keeps one open batch per line (WI-2415). #215 was closed unmerged (graft marked batch 2 rejected), and the next run recut everything as batch 3.

## Comparison

| | per-PR baseline (fit assessment §3, rustTorrent) | trial 1 (WI-2270) | trial 2 |
| --- | --- | --- | --- |
| Units | 108 PRs over the window | 4 lands, 1 batch PR | 7 item lands + 2 fixes, 2 (1 rejected, 1 merged) batch PRs |
| Merge time p50 / p90 | 1.6 h / 16.2 h | 17.3 h (one batch, no reviewer) | 0.69 h / 9.29 h (second wave 0.66 h / 0.71 h); recut PR open to merge 15.4 min |
| Queue conflicts or failures | 8 of 55 entries | 0 | 1 of 2 batch PRs (gui-e2e, 4 tests, one land) |
| Contention tested | yes (140 co-editing pairs) | no | 3 same-file pairs, 2 conflicts, 1 land refused by the server |
| History on GitHub | one squash or rebase per PR | one squashed commit | 9 commits, one per land, with trailers |
| Attribution | author per PR | every land `graft` | `graft@localhost` (stale CLI, fixed in #176) |

## Gaps raised

| Item | Gap | State |
| --- | --- | --- |
| WI-2411 | the client ignores `.gitignore`, so every web worker moved node_modules and dist out by hand | pushed (aae394a), PR next |
| WI-2412 | the default author was `<user>@localhost`, with no env-only fallback | done (#176) |
| WI-2413 | `graft status` still lists a conflict after the markers are removed | open, next |
| WI-2414 | lands carried no `vogt.session`, because the installer installed a stale CLI | done (#176) |
| WI-2415 | a fix-forward can't join the open batch, so the batch has to close and recut | open, needs decision |
| WI-2416 | live path awareness: push "lands touched your paths" (operator idea) | open, measure first |
| WI-2417 | the pre-land check is skipped when the token lacks the drafts scope, and trial tokens never have it | open |

## See also

- [trial 1](2026-10-rusttorrent-trial-1.md), [fit assessment](../notes/graft-fit-assessment.md), [D21, D22, D26, D27, D28](../decisions.md).
