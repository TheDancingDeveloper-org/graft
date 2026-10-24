# graft fit assessment: the estate's last four weeks against graft

Status: assessment for WI-2336 (2026-10-24). Read-only: no repository or
server was changed. It compares the git and GitHub flow that six estate
repositories actually used with what graft phase 1 would have done
([D21](../decisions.md#d21-batch-pr-export),
[D22](../decisions.md#d22-phase-1-is-a-git-alternative)). It ends with
graft's fundamental gaps, ranked.

## 1. Bottom line

- **Where the time went, it wasn't the ref race.** At the estate's peak, a
  repository merged 34 PRs in a day and 10 in an hour (graft itself on
  2026-10-23: 68 in a day). graft's lock-free CAS land is built for 10,000
  lands a day. The cost the estate paid was elsewhere:
  - rebase churn: 3–47% of PRs force-pushed, and 48% of graft's own PRs
    merged `main` back in;
  - stacked-PR chains that were collapsed into carrier PRs or left open for
    days;
  - merge-queue ejections;
  - CI volume (about 10,200 workflow runs over the six repositories in four
    weeks).
- **graft would have helped with**:
  - **stacks**: one `land` of a whole stack instead of 5–11 chained PRs;
  - **conflict feedback inside the agent's own session**: `land` returns
    `Conflict` at once, where today a PR sits `DIRTY` until someone notices;
  - **CI cost**: under D21, CI runs once per batch, not per PR;
  - **rdpapp**, which can't have a merge queue on the Team plan.
- **graft would not have removed the conflicts themselves.** The hot spots
  were:
  - append points and registration lists (`CHANGELOG.md` in 91 of
    fluent2's 117 PRs, `gallery/src/pages/mod.rs`, `plan.md`,
    `docs/ENGINE.md`);
  - move-heavy refactors (the rdpapp `main.rs` split, the rustTorrent qbit
    split);
  - lockfiles and generated files.

  graft's merge3 runs at xdiff's `EAGER` level, so adjacent edits conflict,
  as in git, and it has no move detection.
- **graft would have hindered in four ways.** Phase 1 lands with no checks
  (D22) and ships to GitHub as one squashed batch (D21). That turns today's
  per-PR review, per-PR CI and per-PR revert into one review, one CI run
  and one reject per batch. The rustTorrent trial shows the cost even at
  four lands: batch PR #214 had no review, author `graft` on every land, no
  session, and 17.3 h waiting for the operator (the repository's median
  merge time is 1.6 h). Vogt and the estate's tooling are also built
  around branches and PRs, and graft workspaces are invisible to them.
- **The evidence for graft's core claim is still synthetic.** The trial's
  four lands ran one after another with no contention. The bench uses a
  generated Python project. Neither has replayed the estate's real
  concurrent same-file work, which this note shows exists in quantity.

## 2. Sources and method

- **Window:** PRs created from 2026-09-26 to 2026-10-24 (4 weeks).
- **Repositories** (all `TheDancingDeveloper-org`):
  - `rustnzb` (the brief's "rustnzbd");
  - `rustTorrent`;
  - `vogt`, and its Rust port on the `rust-core` line and the unmerged
    port branches
    (`/home/sprooty/Working/notes/vogt-rust-port-gap-2026-10-24.md`,
    WI-1082);
  - `fluent2-egui` (the brief's "fluent2");
  - `rdpapp` (private);
  - `graft` itself, for comparison.
- **PR data:** GitHub GraphQL search, per PR: commits and their parent
  counts, reviews, comments, the first 100 files, and timeline events
  (force pushes, base changes, merge-queue add and remove with reason,
  auto-merge). The comments on closed PRs were classified by keyword:
  carried by a stack, superseded, or other.
- **CI data:** `actions/runs`, fetched in date slices, with job and step
  names for a sample of the failed runs.
- **Repository shape:** the git tree of `main` (file count, size, binaries,
  largest blobs), commit counts and releases.
- **Overlap:** two PRs overlap when they were open at the same time and
  touch the same file. `Cargo.lock` is left out, because it overlaps
  everything.
- **graft side:**
  - docs/design.md, decisions.md (D21–D24), status.md, plan.md;
  - the bench at 1k and 10k (`docs/trials/2026-10-23-*`);
  - WI-2270 and its comments, and rustTorrent PR #214;
  - the CLI and mirror code, where the docs were silent.

Caveats:

- A force push is usually a rebase onto `main`, but sometimes an amend after
  review.
- CI "duration" is run start to last update, so it includes job queueing.
- vogt's run count is a floor: two date slices hit the API's 1,000-run cap
  (4,902 fetched of about 5,400).
- The closed-PR classification is a keyword match, not a reading.
- An overlap is a co-edit, not a conflict. The conflict figures below come
  from merge-queue `merge_conflict` ejections, merge-from-`main` commits and
  PRs GitHub marks `CONFLICTING`.

## 3. The numbers

| | rustTorrent | rustnzb | fluent2-egui | rdpapp | vogt | graft |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| PRs (Dependabot) | 108 (28) | 145 (10) | 117 (0) | 65 (0) | 437 (69) | 120 (0) |
| merged / closed unmerged / open | 88 / 12 / 8 | 124 / 21 / 0 | 114 / 3 / 0 | 41 / 2 / 22 | 322 / 110 / 5 | 119 / 1 / 0 |
| merge time p50 / p90 (h) | 1.6 / 16.2 | 1.0 / 20.3 | 0.6 / 2.3 | 3.0 / 17.9 | 0.5 / 4.8 | 0.2 / 0.5 |
| max PRs open at once (non-bot) | 12 | 16 | 8 | 26 | 19 | 5 |
| PRs force-pushed (pushes) | 35 (45) | 35 (50) | 55 (88) | 28 (45) | 97 (130) | 4 (4) |
| PRs with merge-from-`main` commits | 0 | 4 | 5 | 0 | 6 | 58 (128 commits) |
| PRs not based on `main` | 13 | 2 | 2 | 11 | 132 | 0 |
| queue: entries / `merge_conflict` / failed or timed out | 55 / 4 / 4 | 84 / 0 / 13 | 64 / 4 / 0 | no queue | 36 / 2 / 7 | 130 / 2 / 9 |
| PR pairs co-editing a file while both open | 140 | 139 | 228 | 409 | 675 | 18 |
| GitHub reviews (PRs reviewed) | 2 (2) | 38 (19) | 0 | 3 (1) | 19 (16) | 0 |
| PR comments (PRs with any) | 37 (27) | 90 (60) | 46 (25) | 33 (29) | 362 (203) | 1 (1) |
| CI runs (failed / cancelled) | 1,542 (61 / 89) | 1,976 (276 / 72) | 483 (86 / 12) | 459 (35 / 49) | ≥4,902 (481 / 740) | 342 (32 / 8) |
| merge-group runs (failed) | 159 (5) | 405 (73) | 60 (0) | — | 120 (15) | 156 (9) |
| PR CI run p90 (min) | 50 | 28 | 6 | 172 | 27 | 6 |
| releases in window | 2 | 11 | 1 | 0 | 20 | 0 |
| tree: files / MB / largest blob | 673 / 17 / 0.7 MB | 841 / 25 / 1.2 MB | 835 / 7 / 0.4 MB | 368 / 7 / 0.8 MB | 1,125 / 17 / 0.5 MB | 671 / 5 / 0.1 MB |
| binaries (images, fonts, archives) | 102 | 81 | 94 | 2 | 102 | 0 |
| commits on `main` (all time / window) | 297 / 123 | 760 / 328 | 160 / 160 | 426 / 56 | 675 / 586 | 127 / 127 |

What the table says:

- **The repositories are small** (5–25 MB, at most 1,125 files, at most 760
  commits on `main`, no blob over 1.2 MB). graft's history-scaling
  machinery (line index, fork points, id-pruned diffs) has nothing to win
  here, and design §1's "honest scale note" holds. Nothing comes near the
  64 MiB blob limit.
- **Agents do work in parallel on the same files**: up to 26 PRs open at
  once, and 18 to 675 co-editing pairs per repository.
- **Formal GitHub reviews are rare.** Review happens in PR comments (the
  oversight Claude review) and in follow-up PRs: rustnzb #174 → #236, and
  the vogt "review fixes" PRs #1029 and #1046.
- **CI is the volume cost**, about 10,200 runs in total. In rustnzb, 18% of
  merge-group runs failed (73 of 405), and in vogt 15% of all runs were
  cancelled by superseding pushes (740).

## 4. Where the time went, and what graft would change

### 4.1 Rebase churn and merge-queue conflicts

- **Force pushes** (rebases onto `main`, and amends) touched 30–47% of PRs
  in rustTorrent, fluent2-egui and rdpapp, and 22–24% in rustnzb and vogt.
  graft's own repository avoids them by rule (AGENTS.md: merge, never
  rebase), and paid with **128 merge commits in 58 of 120 PRs**.
- **Merge-queue conflict ejections:** 12 in total.
  - rustTorrent: #138, #139, #141 and #150, re-landed 0.5–2.6 h later.
  - fluent2-egui: #58, #60, #64 and #87, re-landed 0.2–1.2 h later.
  - vogt: #1168 and #1171, never merged.
  - graft: #10.
- **graft:** `land` rebases on the server and line-merges non-adjacent
  hunks. A stale base costs no client round trip and no force push. That
  removes most of the force-push and merge-commit churn: the edits that
  touch the same file but not the same lines.
- **What it would not remove** is the 12 ejections, or most of them.
  rustTorrent's four are the qbit compat split (#150) racing feature PRs
  that add endpoints to the file being split. That is a move against an
  edit, and merge3 has no move detection. fluent2-egui's four are
  registration lines added at the same point (`CHANGELOG.md`, `mod.rs`
  lists), and `EAGER` makes adjacent inserts conflict. graft would hand
  these back as `Conflict` (exit 3) in the agent's session, which is
  quicker than an ejection, but they are still conflicts.

### 4.2 Several agents on the same files

The most co-edited files, with the number of PRs that touched them in the
window:

| Repository | Hot files (PRs touching) | Kind |
| --- | --- | --- |
| fluent2-egui | `CHANGELOG.md` (91 of 117), `crates/gallery/src/pages/mod.rs` (38), `controls/mod.rs` (29), `README.md` (28) | append point, registration lists |
| rdpapp | `plan.md` (34), `crates/rdpapp-gateway/src/main.rs` (32), `contracts/v1/openapi.json` and `web/src/generated/rdpapp-api.ts` (12 each) | plan file, monolith being split, generated files |
| vogt | `docs/ENGINE.md` (122), `src/vogt/application/models.py` (81), `services/sessions.py` (79), `engine/server/tests/integration.rs` (70) | docs append points, large shared modules |
| rustnzb | `crates/nzb-web/src/queue_manager.rs` (37), `apps/rustnzb/src/handlers.rs` (33), `nzb-core/src/db.rs` (21), `apps/rustnzb/frontend/package-lock.json` (36 co-edit pairs) | large shared modules, lockfile |
| rustTorrent | `.github/workflows/release.yml` (22), `crates/librtbit/Cargo.toml` (19), `qbit_compat.rs` (18), `qbit_parity_spec.json` (16) | CI files, manifests, a module being split |

- **Large shared modules** are graft's real win: rustnzb's
  `queue_manager.rs` and `handlers.rs`, vogt's `models.py` and
  `sessions.py`. Edits there are mostly non-adjacent hunks, which line
  merge lands without a rebase. Draft overlap (D24) could also tell an agent
  who else is in `queue_manager.rs` before it starts. Nothing consumes that
  answer yet (§6, gap 3).
- **Append points and registration lists** are not a graft win. The fix that
  worked is the delivery process's own rule: one entry per line, sorted, or
  per-PR fragment files (graft's `changes/`).
- **Generated files and lockfiles** are the dangerous case. They often
  merge cleanly as text and wrong as content: rdpapp's OpenAPI and its
  generated TypeScript, `package-lock.json`, `Cargo.lock`. Under `direct`,
  that wrong merge lands (gap 1).

### 4.3 Stacked PRs and long-lived lines

- **vogt collapsed three chains into carrier PRs:**
  - eleven PRs, #835 to #845 (`wi-912` → … → `wi-919`), were closed and
    "landed on main via #847 … which carried the whole stacked chain"
    (81 files);
  - the live-call stack M1–M5 (#865 to #869) went in through #870;
  - the `integrate/vogt-next` line was retired after v0.7.12, its release
    re-linearised as #977 (92 files, superseding #973).

  25 of vogt's 46 non-Dependabot closed PRs were closed this way.
- **rdpapp has an 11-PR stack** still open after 4–5 days: #28 → #51 → #52
  → … → #59, plus #60, which merged into #59.
- **rdpapp also has eight parallel PRs**, #38 to #50, each moving one handler
  group out of `rdpapp-gateway/src/main.rs`. Six of the eight are now
  `CONFLICTING` and three are `BLOCKED`.
- **graft:** a stack is drafts in one workspace, and `land` lands it in one
  call, with `stack squash|reorder|split` on the server. No base retargets,
  no carrier PR, no chain to close. This is graft's clearest win in the
  data. The parallel `main.rs` split would still conflict in graft: each PR
  deletes a region and adds `mod`/`use` lines at the same spot near the
  top. The difference is that each agent would find out at its own `land`
  and fix it then, instead of six PRs going stale together.
- **The vogt Rust port is the counter-case.** `rust-core` is 739 commits
  ahead of and 129 behind `main`. The port sits on two unmerged lines that
  diverged, A and B. PR #1155 (+26,309/−836, 242 files, 90 commits) has
  been open since 2026-10-20, with #1156 and #1157 stacked on it.
  Consolidation is estimated at 6–9 worker-days. graft's model would not
  have helped:
  - It has no long-lived branches (non-goal), so the port would be a line.
  - Multi-parent changesets exist only for imported history, so moving
    `main` into the port line is a rebase of the line's net change. D21's
    batch sync does that for the integration line only.
  - Workspace leases are 2 h and draft retention is 7 days, so a
    multi-week port can't live in a workspace.
  - The hard part is behavioural drift (new Python features to port, goldens
    to re-record), and no text merge solves that.

### 4.4 CI waits and flakes

- **Volume:** about 10,200 runs in four weeks. Per estate repository, PR-event
  runs were 220 (fluent2-egui) to 2,921 (vogt).
- **Waits:** PR CI p90 was 6 min (fluent2-egui, graft), 27–50 min (vogt,
  rustnzb, rustTorrent) and 172 min (rdpapp).
- **Failures:**
  - In a sample of 60 failed rustnzb runs, the jobs were `e2e` (35),
    `desktop` (14), `rust` (8) and `container-smoke` (6). These are the
    expensive suites, and they catch real combination breakage: 73
    merge-group failures *after* each PR's own CI passed.
  - Flake proxy (one sha and workflow both failed and passed): vogt 16,
    rustTorrent 3, rustnzb 3.
  - Reruns: vogt 54, rustnzb 19, rustTorrent 11.
- **graft phase 1:** no CI in the land path (D22). GitHub CI runs once per
  batch PR (D21). That cuts the PR-event volume to one run per batch, and
  land latency stops being CI latency. The price is detection granularity.
  The 73 rustnzb combinations that failed in the merge group would have
  landed on the line, and been found only on the batch PR. A red batch is
  rejected whole, and its lands carry into the next batch, so one bad land
  holds back every good one until someone finds and reverts it (gap 1).

### 4.5 Review loops

- **Today:** per PR. Oversight opens the PR and runs a Claude review.
  Blockers go back to the author on the same PR, and follow-ups become
  their own PRs (rustnzb #174 → #236, vogt #1029, #1046, #1057). PR comment
  threads carry the discussion: vogt has 362 comments on 203 PRs.
- **Under D21:** review happens once, on the squashed batch. A blocker can
  only reject the whole batch: there is no per-land eject, and no per-land
  thread. #214 is the only batch so far. It got 0 reviews, and its table
  shows author `graft` and session `-` for all four lands (gaps 2 and 5).

### 4.6 Large or binary files, monorepo size, history depth

- **Not a factor at today's scale.**
  - No file is over 10 MB, and the largest is 1.2 MB
    (`desktop/src-tauri/icons/icon.icns`).
  - There are 81 to 102 binaries per repository: logo PNGs, fonts and
    Playwright snapshots (vogt's `demo.spec.ts-snapshots`).
  - Histories are at most 760 commits.
- **graft:** the 64 MiB blob limit is fine. A binary conflict becomes a
  `.graft-conflict` sidecar, as git leaves a conflicted binary unmerged.
  Sparse clone wins in milliseconds: 384 ms against 1,347 ms at 10k
  commits, and repositories this size clone in well under a second with
  git.
- **Many copies per host:** this host has 598 per-work-item directories
  under `Active/`. 597 of them are `git worktree`s, which already share one
  object store per repository. So graft's per-host node cache saves less
  than design §1 assumes against "N clones". What fills the disk is
  `target/` and `node_modules`, and graft doesn't touch those.

### 4.7 Release tagging

- **Releases in the window:** vogt 20 (v0.7.2 to v0.7.22), rustnzb 11
  (betas of v1.5.0 to v1.7.0), rustTorrent 2, fluent2-egui 1, rdpapp 0.
  Every one is a tag on GitHub `main` that drives GitHub Actions release
  workflows, with a release dry run on the PRs that touch them.
- **Under D21,** GitHub `main` stays git-primary, so tagging doesn't change.
  But a release can only include work whose batch the operator has merged.
  graft's follow doesn't import tags (`tags: false` in `follow.rs`), so
  graft history can't name a release.

### 4.8 Forge features relied on

The estate relies on these forge features:

- PR discussion;
- required checks through rulesets;
- the merge queue, with `land-pr` waiting on it;
- Dependabot: 107 PRs in the window;
- GitHub Actions, including releases, container publication, Pages and the
  scheduled post-deploy smoke;
- CodeQL on release builds;
- Vogt's forge sync, which reads PRs, branches and checks.

graft has none of these and doesn't plan them (non-goals: code review,
code search, CI). Under D21 they all stay on GitHub, so in phase 1 graft
is an additional system in front of GitHub, not a replacement for it.

## 5. Per repository

### rustTorrent

- **Helped:**
  - The qbit cluster (#138, #139 and #141 against #150) would have hit
    `Conflict` at `land` instead of four ejections, though not fewer
    conflicts.
  - The Homebrew → Flatpak → winget packaging stack (#119 to #123) would
    have been one workspace stack.
  - Most of the 45 force pushes go away.
- **Hindered:**
  - The most-touched file is `.github/workflows/release.yml` (22 PRs). CI
    and release work can only be checked by GitHub Actions, so it would
    land unverified and be checked only in a batch.
  - The 28 Dependabot PRs stay on GitHub.
  - Batch #214 waited 17.3 h for the operator, against the repository's
    1.6 h median.
- **Net:** small. It is a good place for trials (it was one) and a poor one
  for proving value: its peak was 24 merges a day, with 12 PRs open at
  once.

### rustnzb

- **Helped:**
  - Heavy same-module editing: `queue_manager.rs` in 37 PRs and
    `handlers.rs` in 33, with up to 16 PRs open at once.
  - Line merge and draft overlap fit this best of the six.
  - The 1,117 PR-event CI runs would drop to one per batch.
- **Hindered:**
  - 18% of merge-group runs failed, mostly in `e2e` and `desktop`. Those
    are exactly the combination breaks that would land unchecked under
    `direct`, then sink a whole batch.
  - 11 beta releases in four weeks tie the release cadence to the
    operator's batch merges.
  - `package-lock.json` (36 co-edit pairs) is a merge3 hazard.
- **Net:** the strongest case for line merge, and the strongest case
  against phase 1's empty land path. It wants post-land CI per land
  (phase 2) before graft is a safe default here.

### fluent2-egui

- **Helped:**
  - 47% of PRs were force-pushed (88 pushes): the highest share of the six.
    Server-side rebase removes that churn where hunks don't touch.
- **Not helped:**
  - The conflicts are the registration kind: `CHANGELOG.md` in 91 of 117
    PRs, `gallery/src/pages/mod.rs` in 38, `controls/mod.rs` in 29. merge3
    conflicts on adjacent inserts, as git does.
  - All four queue ejections (#58, #60, #64, #87) are of this kind.
  - CI is already cheap: PR p90 is 6 min, and there were 0 merge-group
    failures.
- **Net:** modest. Fragment files (already the delivery-process rule) are
  the bigger lever.

### rdpapp (private; no merge queue on the Team plan)

- **Helped:** graft's strongest case.
  - The 11-PR stack open for 4–5 days becomes one workspace stack.
  - The eight `main.rs` split PRs, six of them now `CONFLICTING`, would get
    conflicts at land time, in the author's session.
  - Lock-free land gives rdpapp something GitHub won't sell it: concurrent
    landing without a merge queue.
  - Its PR CI is the slowest of the six (p90 172 min), so taking CI out of
    the land path saves the most here.
- **Hindered:**
  - #64 renumbered a migration to `0038` after a numbering collision. Two
    lands that each add migration 0038 merge cleanly as text and break
    as content.
  - The OpenAPI contract and its generated client (12 PRs each) have the
    same hazard.
  - As a private repository, graft-dev would hold private source. It is
    token-gated, but it is one more place to secure.
- **Net:** the best candidate for a real graft trial, provided some check
  runs after land (gap 1).

### vogt and the Rust port

- **Helped:**
  - Three carrier-PR collapses (#847, #870, #977) and 25 PRs closed
    unmerged because a stack carried them: the PR-chain cost graft exists
    to remove.
  - `models.py` and `sessions.py` co-editing.
- **Hindered:**
  - vogt *is* the estate's git consumer. Its collectors
    (`collectors/git_local.py`, `dep_refs.py`, `adapters/git/clone.py`),
    forge sync, branch-activity and open-PR scoring, and hand-offs (branch,
    head sha) all assume branches and PRs.
  - Its 20 releases ran through GitHub Actions.
  - The Rust port needed a long-lived divergent line, the shape graft
    rules out (§4.3).
  - About 5,400 CI runs, 740 cancelled: CI per batch would save the most
    here, and its `e2e` and `rust-core` failures (84 and 157) show what
    would land unchecked.
- **Net:** a big win on stacks, blocked by integration. Vogt has to see
  graft workspaces before agents can use them (gap 3).

### graft (for comparison)

- graft is built on git and GitHub, not on graft. 58 of its 120 PRs merged
  `main` back in, 128 merge commits in all, because three parallel workers
  share `Cargo.toml`, `graft-engine/src/lib.rs`, `write.rs` and
  `decisions.md`.
- That is the estate's most direct evidence of the churn graft removes,
  and its most obvious first real user.

## 6. Fundamental gaps and risks (ranked)

| # | Gap | Severity | Evidence | What a fix takes |
| --- | --- | --- | --- | --- |
| 1 | **Breakage lands unchecked, and one bad land sinks a batch.** `direct` with no checks (D22), plus one squashed batch PR (D21), means a clean text merge that is semantically wrong reaches the line and is found only on the batch PR. A red batch is rejected whole; nothing ejects or bisects one land | Critical | rustnzb: 73 of 405 merge-group runs failed after per-PR CI passed. vogt: 15 of 120. rdpapp #64 migration-number collision; generated OpenAPI/TS files; lockfiles. Only one batch can be open per line (`batch.rs` `observe`) | Start phase 2's post-land checks per land on the integration line, with automatic bisect and revert (the code exists: G3.1, G3.8, the attester), or at least a cheap per-land `check` hook for line-merged results (`attest_merged`). Add `pr-batch --exclude <changeset>`, or an automatic revert-and-recut when the batch goes red. An operator decision (phase 2), then M |
| 2 | **The batch PR flattens review, attribution and history.** Per-PR review, blocker loops, threads and per-change commits on GitHub become one squashed commit and one review. The operator becomes the merge bottleneck | High | #214: 0 reviews, 1 commit for 4 lands, open 17.3 h against a 1.6 h median. The estate's delivery process runs a review per PR (`.claude/rules/delivery-process.md`) | A per-land review record in graft: a review verb or `review.url` metadata, with Vogt's review session reading graft's diff API. Export the batch as per-land commits (rebase-merge) instead of one squash, so GitHub blame and bisect keep land granularity. Cut batches by size or age. M; the export shape is a D21 amendment |
| 3 | **Vogt and the estate tooling can't see graft work.** Workspaces and drafts aren't branches or PRs, so Vogt's drift, branch activity, open-PR scoring, hand-offs (branch and head sha), `land-pr` and `ci-log` all go blind. Draft overlap (D24) has no consumer | High | Vogt's collectors and forge sync read git and GitHub. Work items score `open_pr` and `branch_activity`. 597 git worktrees on this host are how sessions work today | A graft forge adapter in Vogt: a workspace as the branch analogue, a land as the merge, the draft-overlap feed, the batch as the PR. Vogt's session start opens a workspace instead of a worktree. L, cross-repository, needs a Vogt work item |
| 4 | **The claim is untested on real work.** The trial's four lands ran in sequence with no contention, and the bench is synthetic. No measurement shows what line merge saves on the estate's real overlaps | High | WI-2270: "landed one after another … with no conflicts". The bench's conflict scenario is graft's weak spot (p50 1,142 ms against git's 491 ms at 10k) | Replay real PR diffs concurrently through the harness: the rdpapp `main.rs` split (#38 to #50), the fluent2-egui registration PRs, the rustTorrent qbit cluster, the graft repository's own merge-commit PRs. Count clean line merges, conflicts and wrong-but-clean merges (build after merge), against `git merge-tree`. M; bench only |
| 5 | **Agent identity isn't carried.** The CLI sends `session: None` and kind `Human`. The author is `GRAFT_AUTHOR_NAME`, else `$USER`, else `graft`. `VOGT_SESSION_ID` and `VOGT_WORK_ITEM` are ignored (the work item only arrives as `-m` metadata) | Medium (breaks goal 8) | `graft-cli/src/conn.rs` `call_context`, `cmd/commit.rs` `author`. #214's table: author `graft` and session `-` on all four lands | Read `VOGT_SESSION_ID` and `VOGT_WORK_ITEM` into the call context and changeset metadata, default the author kind to `agent` when a session is set, and take the author from the token's subject. S |
| 6 | **merge3 doesn't address the estate's conflict hot spots.** Adjacent edits conflict (`EAGER`), there is no move detection, and lockfiles and generated files merge as plain text | Medium | The 12 queue `merge_conflict` ejections were moves against edits (the rustTorrent qbit split) or adjacent registration inserts (fluent2-egui). The hot files are `CHANGELOG.md` (91 PRs), `pages/mod.rs` (38), `plan.md` (34) and `docs/ENGINE.md` (122) | Per-path merge rules: append-union for declared append-only files, and refuse-and-regenerate for lockfiles and generated paths (`Cargo.lock`, `package-lock.json`, `web/src/generated/**`). Move-aware rebase is later work. Keep pushing the fragment and sorted-list rule. M for the rules |
| 7 | **No model for long-lived lines or multi-day stacks.** No line-to-line merge outside batch sync; leases of 2 h and retention of 7 days; non-goal "branch-heavy workflows" | Medium | vogt `rust-core`: 739 ahead and 129 behind, two divergent port lines, #1155 open 4 days. rdpapp's 11-PR stack open 4–5 days | A design note: `line sync <line> --from main`, recording a two-parent changeset on non-`main` lines, and lease policy for multi-day stacks (Vogt renews per session). Design PR, then M |
| 8 | **Read-path speed misses the kill criterion.** The criterion asks for 2× faster reads than git end to end. `log -50` and `status` are slower, and land and sync are slower in replay | Medium | 1k: `log 50` 2.51× git, `status` 1.34×. 10k: `sync` 47 ms against 28 ms, replay land p50 34 ms against 16 ms. Wins: clone, sparse clone, `log --path`, blame | Profile CLI start-up and the per-call round trip. Serve through the warm node daemon by default. Re-run the bench on the quiet VM. M |
| 9 | **Operating cost of a second system.** The VM, server, mirror, backups and follow, plus their failure modes, all sit in front of GitHub, which still does everything else | Medium | Trial: a wrong volume mount left the mirror DB empty and reported `graft-moved` (fixed as `Unmapped`, #108). Kill criterion: at most half a day a week of operator time | Track operator time per week against the kill criterion. Keep `deploy/up.sh` as the only deploy path. Alert on follow lag |
| 10 | **Forge features stay on GitHub, and follow ignores tags.** No PR discussion, checks, issues or Actions in graft, and release tags never reach graft history | Low (by design, D21) | 20 vogt releases and 11 rustnzb betas, all GitHub tags. `tags: false` in `follow.rs`. 107 Dependabot PRs | Import tags in follow as `tag` refs (design §11.1 already allows them). The rest is a non-goal: document that graft fronts GitHub, not replaces it. S |
| 11 | **Large files, history depth, sparse clone.** Not a gap at today's scale, and not a selling point either | Low | Largest blob 1.2 MB, at most 1,125 files, at most 760 commits. git worktrees already share objects per host | None now. Revisit with chunked blobs if a repository grows past the 64 MiB limit |

## 7. What to do next

1. **Measure on real overlaps** (gap 4) before more phase-1 features. The
   question is how many of the estate's co-edits line merge lands cleanly
   and correctly.
2. **Fix identity** (gap 5). It is small, and every later trial needs it.
3. **Ask the operator two questions:**
   - Does phase 2's post-land check start now, for the batch line only
     (gap 1)?
   - Should a batch export as per-land commits rather than one squash
     (gap 2)?

   Both change D21 and D22, so each needs its own design PR.
4. **Pick rdpapp or the graft repository as the next trial**, not
   rustTorrent: it has real stacks, real same-file parallel work and no
   merge queue. Vogt follows once it has a graft adapter (gap 3).
