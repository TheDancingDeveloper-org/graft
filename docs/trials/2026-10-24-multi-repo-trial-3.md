# Trial 3: three repos, six Codex sessions on Clawbay (2026-10-24/25)

WI-2418. Codex worker sessions (gpt-5.6-sol through LiteLLM to Clawbay) delivered real backlog work from three repositories through graft at the same time: rustTorrent and fluent2-egui on `main`, and the vogt Rust port on its long-lived `rust-core` branch. graft-dev followed all three. Each repo had a `trial-3` line and one batch PR (D26: one commit per land), reviewed by Sonnet and landed by `land-pr` through the repo's rebase merge queue (D28).

D22 applied throughout: graft ran no build, test or check. Midway the operator also removed builds from the workers (static checks only: `cargo fmt`, `cargo check`, tsc and lint), so each repo's CI on the batch PR was the only build and test gate.

**Result:** 20 of 22 items landed through graft between 22:10Z and 01:10Z, with 4 fix-forwards. The other 2 were already on `rust-core`. Line merge took every same-file overlap on rustTorrent and fluent2-egui. On vogt, two real conflicts stopped the land (exit 3) and the workers resolved them. Every land passed the D27 pre-land check, and nothing was lost or mis-attributed. rustTorrent and fluent2-egui batches merged with per-land commits. The vogt batch is open and parked: it fails vogt's own CI, which is outside this trial (see Scope).

## Scope

graft is a git replacement, so the trial judges source control only: lines, sync, land, conflicts, merge, the pre-land check, batch PRs and attribution. Defects in a target repo's code, tests or CI are context for the batch outcomes, not graft findings. graft stays language and project agnostic.

## Setup

| Item | Value |
| --- | --- |
| Server | graft-dev at graft main 21685fa (#182), with mirrors for rusttorrent (`main`), fluent2-egui (`main`) and vogt (graft line `rust-core`) (#181). All three at lag 0 before the first land |
| Lines | `trial-3` per repo, forked from the followed base (#182 scripts: `GRAFT_TRIAL_REPO`, `GRAFT_TRIAL_BASE`) |
| Workers | Codex, gpt-5.6-sol through LiteLLM to Clawbay, up to 6 at once; 4 fix-forwards on Claude Opus. Tokens session-bound, with the drafts scope so the full D27 pre-land check runs (WI-2417, #182) |
| CLI | built from main with `.gitignore` support (WI-2411), the stale-conflict fix (WI-2413) and attribution fixes (WI-2412, WI-2414) |
| GitHub | D28 rulesets on all three repos (pull_request rule plus admin bypass); fluent2's merge queue switched from SQUASH to REBASE so per-land commits survive |
| Items | 22 approved: rustTorrent 8, fluent2-egui 6 (3 raised for the trial), vogt port 8 |

## Results per repo

### rustTorrent

- 8 item lands between 22:10 and 22:48Z, plus 1 fix-forward. 0 conflicts, 0 pre-land check stops, 0 retries. Line merge took every same-file pair: the router (#189, #190, #191, #192), Footer.tsx (#193, #207) and DetailPane (#208 with #209).
- Batch #217: Sonnet found 2 blockers, both in the #189 land. They were feature-gated compile errors (a private module path and a misplaced utoipa attribute under `swagger`) that static checks without features can't see. DeepSeek: clean.
- #217 closed unmerged; fix-forward cfd89d4 (Claude Opus); recut as #218, with the 8 commits patch-identical. Re-review clean.
- #218 opened 23:02:23Z and merged 23:16:18Z (14 min): 9 commits on main (945a82d), each with Graft-Session and Graft-Work-Item trailers.
- Land to main: 28 to 66 min, p50 about 37 min.

### fluent2-egui

- 6 item lands between 22:24 and 23:19Z, plus 1 fix-forward. 0 conflicts, 0 pre-land stops. The same-file group (#103, #155, #156 on the syntaxes, upstream.toml and code_view tests) line-merged cleanly.
- Batch #164: Sonnet clean. CI failed. The #103 grammar refresh added a variable-length lookbehind (`await\s+using`) that fancy-regex can't compile, so syntect silently dropped the TypeScript grammar. Review didn't catch it; CI did.
- #164 closed unmerged; fix-forward ce5906b (`await\susing`); recut as #165.
- #165 opened 00:03:25Z and merged 00:18:48Z (15 min): 7 commits on main (3a414da).
- Land to main: about 60 to 115 min, because of the CI failure and recut.

### vogt Rust port (`rust-core`)

- 6 item lands, plus 2 fix-forwards (c63a12e for the review, cd946ad for CI). 2 conflicts (exit 3), both resolved by the worker and landed on the next try: WI-2026 against WI-2022 in the session reads, and WI-2142 against the operation registry (it also renumbered its migration). 0 pre-land stops.
- 2 items (WI-1778, WI-1779) were already on `rust-core`; the review confirmed it and they were closed.
- One session hit its 90-minute budget, hibernated, and resumed with its context.
- Batch #1190 (base `rust-core`, 6 commits): review and CI both rejected it. Closed; fix-forward landed; recut as #1191 (7 commits, the first 6 patch-identical). Review clean.
- #1191 still fails vogt's CI. Python and Rust config generators disagree, a migration freeze list wasn't updated, and Rust CLI tests fail. Live Playwright fails on the `rust-core` base too. A second fix-forward (cd946ad) landed on the line after the recut, so #1191 doesn't carry it. By operator decision the PR is parked open for the port's owners rather than fixed inside the trial. graft's part (lines, lands, conflicts, batch PR against a non-main base) worked.

## Measurements

| | rustTorrent | fluent2-egui | vogt | all |
| --- | ---: | ---: | ---: | ---: |
| Item lands | 8 | 6 | 6 | 20 |
| Fix-forwards | 1 | 1 | 2 | 4 |
| Conflicts (exit 3) | 0 | 0 | 2 | 2 |
| Pre-land check stops | 0 | 0 | 0 | 0 |
| Batch PRs (rejected, merged) | 2 (1, 1) | 2 (1, 1) | 2 (1, 0; 1 parked) | 6 (3, 2; 1 parked) |
| Batch open to merge | 14 min | 15 min | parked | |

- **D27 pre-land check:** ran on every land with session-bound tokens holding the drafts scope. It stopped nothing, because line merge resolved every overlap. No false positives were observed.
- **Attribution:** every batch commit carries Graft-Session and Graft-Work-Item trailers (WI-2414 fixed). The author is the mirror identity, because sessions set no author email (WI-2412 design).
- **Review against CI:** one batch was rejected by review (rT: feature-gated compile errors) and one by CI (fluent2: a runtime grammar regression). Each defect was pinned to a single land, the fix forward was one commit, and the recut kept the other commits patch-identical. This is the cost of "static checks only" for workers: defects surface one stage later, at the batch, and cost a recut instead of a local retry.
- **Codex/Clawbay quality:** of 22 items, 20 were delivered and 2 were correctly reported as already done. Every batch needed at least one fix: a feature-gated compile error (rT), a grammar regression (fluent2), and on vogt several parity defects against the Python contract. These are code-quality findings about the agents, not about graft.

## Incidents (not graft)

- **LiteLLM key budget:** the session key's $4 max budget was hit at about 22:55 and stalled every Codex worker until about 23:07 (operator removed the cap).
- **Per-provider key split:** the old session key was invalidated, and every Codex worker returned 401 until relaunched with `codex resume` at 23:15. Context was kept.
- **Clawbay GPT degradation:** after a 23:5x LiteLLM restart, gpt-5.6-sol timed out, luna fell back to grok and gpt-6-astra returned 503; grok-4.7 was healthy. Workers kept going slowly.
- **Worker behaviour:** one Codex session looped on empty autopilot turns until re-tasked. One resumed session reported another item's land. Two vogt items (WI-1778, WI-1779) were already present on `rust-core`.
- **Operator rule change mid-trial:** workers stopped running builds and tests (static checks only), so the 2-builder cap no longer limited concurrency.

## Comparison

| | per-PR baseline (fit assessment §3) | trial 1 | trial 2 | trial 3 |
| --- | --- | --- | --- | --- |
| Repos | 1 per PR | rustTorrent | rustTorrent | 3 (one on a non-main base) |
| Agents | per PR | 4 Opus, sequential | up to 4 Opus | up to 6 Codex (gpt-5.6-sol) plus 4 Opus fixes |
| Lands | | 4 | 7 plus 2 fixes | 20 plus 4 fixes |
| Conflicts | queue conflicts 4 of 55 (rT) | 0 | 2 | 2 |
| Batch merge (open to merge) | rT PR p50 1.6 h | 17.3 h | 15.4 min | 14–15 min |
| Rejected batches | | 0 | 1 (CI) | 3 (review or CI), plus 1 parked |
| History on GitHub | one commit per PR | one squashed commit | 9 per-land commits | per-land commits with session trailers |

## Gaps raised

Source-control gaps found and fixed during the trial:

- WI-2411: the client ignored `.gitignore`, so build output was committed (fixed).
- WI-2412: session lands carried a placeholder author (fixed; design note above).
- WI-2413: `graft status` still listed a resolved file as conflicted (fixed).
- WI-2414: batch commits had no Graft-Session trailer (fixed).
- WI-2417: the pre-land check was skipped without the drafts scope (fixed).

Still open:

- WI-2415 and WI-2434 (D30): a review fix-forward can't join the open batch, so every fix costs a close and recut. This happened 3 times.
- WI-2416: tell clients when lands touch their paths (measure first).
- WI-2419: the first deploy of a newly followed repo races its import (deploy script).

Not graft: WI-2436 (Komodo env edit) and WI-2437 (LiteLLM key routing) were raised in their own projects.

## See also

- [trial 2](2026-10-24-rusttorrent-trial-2.md), [trial 1](2026-10-rusttorrent-trial-1.md), [fit assessment](../notes/graft-fit-assessment.md), [D21, D22, D26, D27, D28](../decisions.md).
