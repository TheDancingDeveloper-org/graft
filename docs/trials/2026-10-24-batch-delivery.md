# Batch delivery: bench scenario and one live batch (2026-10-24)

WI-2352. This tests the fixes for WI-2336 gap 2, where a batch PR lost
review, authorship and history:

- WI-2337: identity;
- WI-2343 and D26: one commit per land;
- WI-2340: drop a land, and batch size and age caps;
- WI-2351 and D28: oversight review and a `land-pr` merge.

The method was a bench scenario, then one live batch. Everything was built
and run on the graft-dev VM, with the binaries built at `292dbf3`; graft-dev's
server image is `3dbd296`. Under D22, graft runs no checks: the PR's CI is
GitHub's own.

**Result:** authorship, history and blast radius are met in the bench and on
GitHub. The bench meets latency. Live latency to GitHub `main` waits on
oversight's review and merge of
[graft #143](https://github.com/TheDancingDeveloper-org/graft/pull/143).

## 1. Bench scenario (`python3 -m graftbench --batch`)

`bench/graftbench/batch.py` sets up:

- a local graft-server;
- a bare repository standing in for GitHub, with a stdlib fake of the REST
  and GraphQL calls that `pr-batch` makes;
- N agents, each with its own agent token (minted with `--session` and
  `--work-item`) and its own working copy, plus `VOGT_SESSION_ID`,
  `VOGT_WORK_ITEM` and an author.

Every round, each agent lands at the same moment (a barrier) on line `int`.
The pre-land check (D27) is on by default. Each land edits two places:

- the agent's own module;
- the agent's own region of the shared `src/shared.py`, so the lands merge
  by line (D16).

Agent 1's last land plants a syntax error. The "build" is
`python3 -m compileall -f` over `src/`, standing in for the forge's CI.

The run goes through these steps:

1. Cut batch 1 with `pr-batch`.
2. Check each exported commit against its land.
3. `git bisect run` the build over the batch branch.
4. `git blame` every good land's line.
5. Drop the bad land with `pr-batch --exclude`.
6. Merge batch 2: a simulated `land-pr` rebase-merge (a fast-forward, since
   the base doesn't move).
7. Settle with one more `pr-batch` run.

| Run | Lands | Authorship | One commit per land | bisect | blame | Commits on `main` | Good lands held | Drop and recut | Land to `main` p50 / p90 |
|---|---|---|---|---|---|---|---|---|---|
| 5 agents x 2, run 1 | 10 | 10/10 | yes (10) | bad land | 9/9 | 11 (10 lands + 1 revert) | 9 | 0.37 s | 2.5 s / 2.8 s |
| 5 x 2, run 2 | 10 | 10/10 | yes | bad land | 9/9 | 11 | 9 | 0.26 s | 2.0 s / 2.3 s |
| 5 x 2, run 3 | 10 | 10/10 | yes | bad land | 9/9 | 11 | 9 | 0.17 s | 1.4 s / 1.8 s |
| 8 x 3 | 24 | 24/24 | yes (24) | bad land | 23/23 | 25 | 23 | 0.22 s | 2.0 s / 2.7 s |

What the columns mean:

- **Authorship:** each commit's author name and email, plus its
  `Graft-Session`, `Graft-Work-Item` and `Graft-Changeset` trailers, match
  the land. graft's changeset records the author kind as `agent`.
- **Land p50 / p90:** 233 to 345 ms / 381 to 495 ms. No land needed
  `--sync`, and none was retried.
- **Cut:** opening batch 1 took 0.36 to 0.46 s.
- **Drop:** PR 1 closed with a comment, and batch 2 passed CI.

The raw results are in `bench/results/2026-10-24-batch/`.

In the first bench run, the planted land was agent 1's *first* land. Its
second land rewrote the same lines, and `pr-batch --exclude` stopped with a
sync conflict (`src/agent_1.py`, `src/shared.py`) and moved nothing. That is
correct: a land that later lands build on can't be reverted cleanly, so it
is fixed forward with a new land (D28). The bench now plants the bad land
last, so the drop is independent.

## 2. Live batch (graft on GitHub)

| Item | Value |
|---|---|
| Server | graft-dev, repository `graft-wi2352`, followed from GitHub `TheDancingDeveloper-org/graft` `main` by a dedicated mirror DB |
| Line | `batch-2352`, forked at `main` (`6c16a69`), policy `direct` |
| Clients | 5 scripted client sessions (`bench/graftbench/batchlive.py`), each with its own agent token (`--session ses_wi2352liveN --work-item WI-2352`) and working copy |
| Changes | agents 1, 2 and 3 each edit `trial/agent-guide.md` (the same file, in different sections), bringing it up to date with D28, D27 and WI-2337; agent 4 adds `trial/README.md`; agent 5 is the planted bad land (`trial/DROP-ME.md`) |

The clients weren't Vogt worker sessions. Driving 5 LLM sessions would have
added cost and noise without changing what graft does. The session ids are
synthetic, and the work item is real.

### What happened

1. All 5 lands went through at the same moment, first try, with no sync. The
   pre-land check ran on every land and found no conflicts. Land latency was
   95 to 316 ms, and all 5 lands fell within 0.22 s.
2. `pr-batch` opened
   [graft #142](https://github.com/TheDancingDeveloper-org/graft/pull/142)
   12.5 s after the last land (including following GitHub `main`). It had
   5 commits, one per land, in land order.
3. `pr-batch --exclude <agent 5's land>` ran 58 s after the last land and
   took 8.0 s. It landed a revert on the line, closed #142 with a comment
   ("superseded: land `ca6b439eafa20` dropped ... recut as a new batch"),
   and opened
   [graft #143](https://github.com/TheDancingDeveloper-org/graft/pull/143):
   6 commits (the 5 lands and the revert), mergeable, with a net diff of 2
   files (`trial/README.md`, `trial/agent-guide.md`).
4. Oversight reviews #143 and merges it with `land-pr` (D28). This trial
   stops at the open PR.

### Measures

| Measure | Target | Live result |
|---|---|---|
| One commit per land | 1 | 5 lands, 5 commits on #142; on #143, 5 land commits plus 1 revert commit for the drop |
| Authorship on GitHub | 100% | 5/5 land commits have the right author, `Graft-Session` and `Graft-Work-Item` (the "Vogt-Work-Item" the item named is spelled `Graft-Work-Item`). The revert commit is authored `wi2352-mirror <unknown>`, see gaps |
| Blockers pinned on one land | yes | the bad land is its own commit (`7ad3aee`), and the drop named it by changeset |
| Blast radius of one bad land | — | it held 4 good lands, in the same open PR, until the drop. Drop and recut: 8.0 s on GitHub (0.2 to 0.4 s in the bench) |
| Land to PR open | — | 12.5 s (batch 1); recut 66 s after the last land, almost all of it the operator's 58 s before running the drop |
| Land to GitHub `main` | below the per-PR baseline | pending #143's review and `land-pr`. Baseline (docs/notes/graft-fit-assessment.md §3): median merge 0.2 h for graft and 3.0 h for rdpapp; rustTorrent batch #214 took 17.3 h |

## Gaps and follow-ups

- **Revert commit identity.** A drop's revert reaches GitHub as
  `<mirror token subject> <unknown>`. It should carry the operator's or the
  mirror's identity with a real email. Follow-up item raised.
- **Stale batch detail.** `pr-batch` still prints and records "waiting for
  the operator to merge" for an open batch. Under D28, oversight reviews the
  batch and `land-pr` merges it. Follow-up item raised.
- **Live latency.** Land to GitHub `main` is measured once #143 merges: the
  merge time minus the land times in
  `bench/results/2026-10-24-batch/live/state.json`. Tracked on WI-2352.
- **A drop that later lands depend on conflicts.** This is expected and
  documented above. The fix is a forward land.
- **Pre-land draft overlap** was skipped live, because the agent tokens
  lacked the `drafts` scope. Only the conflict preview ran.
