# Agent guidance: graft

This repository is a new source control system, written in Rust. Read
[docs/design.md](docs/design.md) before changing behaviour, and
[docs/plan.md](docs/plan.md) to find your work item. Workspace-wide rules
(the delivery process, safe handling, Vogt and Cadastre) still apply.

## Before you edit

- Work on one plan item (`G<phase>.<n>`) per session, on branch
  `g<phase>-<n>-<slug>` off `main`. The item's Vogt work item is in the brief.
- **Edit only the crates your item lists.** Three workers run at once on
  disjoint crates. If you need to change another crate's public API, stop.
  Raise a small interface PR for that crate (types and signatures only, with
  stubbed bodies), and say so in your hand-off.
- Program against fakes (`graft-store` memory store with the `testkit`
  feature, and `graft-testkit`) when a dependency's bodies are not built yet.
- Don't edit `docs/design.md` or `docs/decisions.md` in a feature PR. If the
  design is wrong, say so in the hand-off. Design changes are their own PR.

## Rules of the design you must not break

These are the constraints reviewers will say "no" to:

1. **Durable formats are frozen by golden vectors.** Never change an
   encoding (ids, tree, changeset, frame, token) without a migration and an
   operator decision.
2. **Costs follow the change.** No operation may walk the whole history or
   materialize a whole snapshot. Use the line index, the skip lists, and
   id-pruned tree diffs. Scanning the journal or every changeset to answer a
   query is a bug, even when it passes the tests.
3. **One transaction per logical write**, including its journal entry.
   Objects are durable before metadata references them.
4. **Refs move only by compare-and-swap.** Nothing moves two refs at once.
5. **No server time or randomness in an id.**
6. **Git stays in `graft-git` and `graft-mirror`.** No git types or concepts
   in any other crate.
7. **Every response is bounded.** Lists are paginated, patches are capped.
8. **Errors are typed** (`graft_proto::ApiError`) and never carry file
   contents or tokens. Tokens never appear in logs, `Debug` output or
   process arguments.
9. **Validate at the boundary, then trust.** Don't add defensive re-checks
   deep inside the engine for cases the boundary already rejects.
10. **Tests assert outcomes, not timing.** No pass condition may depend on a
   fixed sleep, a minimum count of timed events, or machine speed; CI runners
   are slower than your host. Crash and concurrency tests record what was
   acknowledged, inject the fault, then check exactly that afterwards
   (presence and integrity). A test that can pass without exercising the
   fault is a defect.

## Checks (targeted; CI runs the rest)

```sh
export CARGO_BUILD_JOBS=4          # shared hosts
cargo fmt --all -- --check
cargo clippy -p <crate> --all-targets --all-features -- -D warnings
cargo nextest run -p <crate> --all-features   # or: cargo test -p <crate>
```

## Finishing

- Add a changelog fragment `changes/<item>.md`.
- Land with `g land` on the graft line (`backlog-1`): commit, `g sync`, then
  `g land`. Once landed, **stop**. Don't open a PR, don't poll CI.
- Batch PRs are the normal path. Oversight cuts one batch PR from the line
  (`graft/batch-<n>`, made by `pr-batch`) and `land-pr` rebase-merges it with
  `--admin` once the required checks pass, so each land's commit survives
  (D28). Hand-made PRs squash through the merge queue.
- Commit with the identity in your brief.
- Leave a Vogt hand-off comment on your work item: branch, head sha, and
  which checks you ran. Then list **every** Deliver and Accept item from the
  work item, each with the file and test that meets it, or `NOT DONE: <why>`.
  Never describe partial work as done; reviewers check this list line by line.
- Print `AUTOPILOT: DONE`.
