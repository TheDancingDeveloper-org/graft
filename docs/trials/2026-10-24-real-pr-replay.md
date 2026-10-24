# Trial: real overlapping PRs replayed through graft and git (WI-2338)

Date 2026-10-24. Release build of origin/main 0e6dbfd. Harness
`bench/graftbench/realprs.py`; raw results in
`bench/results/2026-10-24-real-prs/` (`results.json`, `summary.md`); case
plan in `bench/realprs/cases.json`.

## Question

The fit assessment (docs/notes/graft-fit-assessment.md, gap 4) says graft's
core claim, that line merge and server-side rebase remove the estate's churn,
has only synthetic evidence. Does it hold on the PRs that actually conflicted?

## Method

115 real PRs from four repositories, each landed from its real fork point
with `graft land` (and `land --sync` when the server's rebase refuses) and
with git (`push`; on a rejection `fetch`, `merge-tree`, `rebase`, `push`).
A PR's change is the head the agent had before it first integrated: the head
before its first force push (the original fork is otherwise lost), or before
its first merge of `main` back in. Modes are described in `bench/README.md`.
Oracle: `cargo check --workspace --all-targets` on every merged tree. A PR
stops at its first conflict; nothing is resolved.

## Results

| case | PRs | clean graft / git | conflicts graft / git | clean but broken graft / git | round trips graft / git |
| --- | ---: | ---: | ---: | ---: | ---: |
| rdpapp `main.rs` split (open PRs onto today's main) | 9 | 1 / 1 | 8 / 8 | 0 / 0 | 17 / 19 |
| rdpapp 11-PR stack onto today's main | 11 | 0 / 0 | 1 / 1 (#28, `plan.md`) | 0 / 0 | 2 / 2 |
| fluent2-egui registration lists | 34 | 18 / 18 | 16 / 16 | 0 / 0 | 50 / 64 |
| rustTorrent qbit #138, #139, #141, #150 | 4 | 0 / 0 | 4 / 4 | 0 / 0 | 8 / 8 |
| graft PRs that merged `main` back in | 57 | 39 / 39 | 18 / 18 | 1 / 1 (#35) | 75 / 153 |
| **total** | **115** | **58 / 58** | **47 / 47** | **1 / 1** | **152 / 246** |

- graft and git agree on every PR: the same PRs merge, the same conflict,
  and all 58 clean merges produce byte-identical trees.
- Conflicted paths are the ones the assessment named: `CHANGELOG.md` (16 of
  fluent2's conflicts), `crates/gallery/src/pages/mod.rs` (11), the gateway
  `main.rs` (8 of 8), `qbit_compat.rs`, `graft-testkit/src/lib.rs`,
  `Cargo.toml` and `Cargo.lock`.
- Clean but broken: one, graft #35 (`graft-server/src/mcp.rs`, E0308), and
  both tools produce it. The other 46 merged trees pass `cargo check`; the 11 fast-forwards are the PR's own tree and are not checked.
- Round trips: graft needs 1 per clean land against git's 3 (push, fetch,
  push), so 38% fewer overall; on conflicts both take 2. Wall time per land
  is tens of milliseconds for both (p50 graft 46–196 ms, git 58–224 ms) and
  is not a differentiator at this scale.

## Verdict

The claim is **refuted** for conflicts and **confirmed** only for round trips:

- Line merge removes no conflict that git's merge-ort leaves: 47 of 115 real
  PRs conflict under both, and 39 of 57 graft merge-backs would still have
  merged cleanly under git `rebase`. The churn came from textual conflicts on
  append points and move-heavy refactors, and from the forge's "branch out of
  date" rule, not from git's merge.
- graft's win is the protocol: one call per land instead of fetch, rebase
  and push, and conflict feedback at `land` time.
- Under `direct`, graft lands the same clean-but-broken merge git would
  (#35). That is gap 1.

## Limits

- The stack case stops at #28, which conflicts with today's `main` on
  `plan.md`. graft's one-call stack land is not exercised past it; that needs
  conflict resolution, which this harness does not do.
- PRs land one at a time in a fixed order, not with threads; contention is
  the real `main` between each PR's fork and its integration point.
- `cargo check` only; tests are not run.
- One fluent2 PR (#68) and one graft PR (#55) were skipped: their
  integration point is not on `main`.
