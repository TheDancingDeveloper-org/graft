# rustTorrent trial-1: four agent lands through graft (2026-10-23/24)

WI-2270. Four Vogt worker sessions delivered real rustTorrent backlog items
through graft on one integration line, `trial-1`, with no branches or PR
chains. graft-mirror then shipped the line to GitHub as one batch PR.

Scope follows D22: phase 1 is source control only. The line ran the
`direct` policy, with no build, test or attestation in the land path. The
attester was disabled (`GRAFT_ATTESTER=0`). GitHub's own CI ran once, on the
batch PR.

**Result:** all four lands succeeded with no conflicts, and the batch PR
merged. Delivery to GitHub was the slow part, not graft.

## Setup

| Item | Value |
|---|---|
| Server | graft-dev on the dedicated VM |
| Repository | `rusttorrent`, followed from GitHub `TheDancingDeveloper-org/rustTorrent` |
| Line | `trial-1`, forked at GitHub `main`, policy `direct` (hot-fixed from `attested` under D22) |
| Agents | 4 Vogt worker sessions on vogt-prod, each with its own token (read, write, `land:trial-1`) and working copy, graft CLI only, following `trial/agent-guide.md` |
| Export | `graft-mirror pr-batch trial-1 --base main`, run once as a one-off container |

## What the trial did

The four lands went onto `trial-1` one after another, on top of `ce14110`:

| # | Changeset | Work item |
|---|---|---|
| 1 | `ce20700` | WI-2155 |
| 2 | `cc35dd4` | WI-2158 |
| 3 | `c635648` | WI-2153 |
| 4 | `cd50344` | WI-2157 |

`pr-batch` exported the line's net change as batch 1: one squashed commit on
GitHub `main` at `ae7fb6e`, pushed to `graft/batch-1` and opened as
[rustTorrent #214](https://github.com/TheDancingDeveloper-org/rustTorrent/pull/214).
The PR had 1 commit and 6 files (+136/-21), and was mergeable.

## Timeline

1. 2026-10-23: operator decision D22. `trial-1` was switched to `direct`
   and the attester was stopped, both on the VM.
2. 2026-10-23: the four agents landed in sequence, with no retries.
3. 2026-10-23: the first `pr-batch` attempt mounted the wrong volume (see
   below). The second opened #214.
4. #214 waited 17.3 h with no review.
5. 2026-10-24: #214 merged as `7e112d7a3947`.

## What worked

- Every `graft land` succeeded first time. There were no conflicts and no
  contention.
- The final tree matched what was expected, and GitHub's CI passed on it.
- `pr-batch` turned four lands into one mergeable PR in a single run.
- Agents needed only the graft CLI and the agent guide. No agent created a
  branch or a PR.

## What didn't

- **The batch waited 17.3 h with no review.** The repository's median merge
  time is 1.6 h. Batching moves all review onto one PR, and nobody owned
  that PR until the operator merged it.
- **Every land was attributed to `graft`.** The batch PR body listed
  author `graft` and no Vogt session or work item for each land. Review
  could not tell who did what.
- **One squashed commit.** Per-land history, and the option to revert one
  land, were lost on GitHub.
- **No contention was tested.** The lands ran one after another, so the
  trial says nothing about concurrent same-file work.
- **Ops: an unclear error on an empty mirror DB.** The first `pr-batch`
  mounted `graft-dev_graft-dev-mirror` instead of `graft-dev-mirror`. With an
  empty mirror DB, `follow` reported line `main` as `graft-moved`. The
  message doesn't point at the empty DB and should.
- `deploy/server.toml` in the repository still had `trial-1` as `attested`;
  the switch to `direct` was a hot-fix on the VM (WI-2298 covers it).

## Fixes since

- **WI-2350** (landed, #123): the graft CLI records the Vogt session, the
  work item and the author (`GRAFT_AUTHOR_*`, then git config, then
  `$USER`) on drafts and lands. Batch PRs no longer say `graft`.
- **D26** (decided 2026-10-24, revising D21): a batch PR carries one commit
  per land, keeping its author, session and work item. Squash is only a
  fallback. graft keeps no review state; review stays on the GitHub PR.
- **WI-2340** (pushed on `wi-2340-batch-drop-cap`, not landed at the time
  of writing): `pr-batch` can drop one land from an open batch without
  holding back the rest, and can cut batches by size or age, so a batch
  does not grow while it waits.

The 17.3 h wait is a review-ownership question, not a graft defect. None of
the fixes above removes it.

## See also

- [graft fit assessment](../notes/graft-fit-assessment.md): §1 uses this
  trial as evidence of the batching cost, and §5 (rustTorrent) gives the
  repository's PR and conflict profile.
- [D21, D22, D26](../decisions.md).
