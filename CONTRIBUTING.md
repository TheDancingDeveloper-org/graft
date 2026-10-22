# Contributing to graft

graft is early: the design is in [docs/design.md](docs/design.md) and the
work is cut into items in [docs/plan.md](docs/plan.md). Pick an item, or open
an issue to discuss a change to the design first.

## How changes land

graft follows trunk-based development, with a merge queue.

1. **Branch** off `main`: one short-lived branch per work item or change,
   named `g<phase>-<n>-<slug>` for plan items. Keep PRs small, and merge them
   within hours, not days.
2. **Push** the branch and open a PR against `main`. Agent workers push and
   stop. Oversight opens the PR.
3. **Review.** Every PR gets a review, from a different model than the author
   where an agent wrote it. Blockers go back to the author.
4. **Merge queue.** A clean review is added to the merge queue, which
   **squashes**: each PR lands as one commit on `main`. The queue runs the
   required checks on the latest `main`, then merges. Nobody pushes to `main`
   directly.
5. **Behind or conflicting with `main`?** Run `git merge origin/main` on your
   branch, resolve, and push normally. Never rebase a pushed branch or
   force-push. Resolve the root `Cargo.toml` by keeping every dependency line,
   sorted. Resolve `Cargo.lock` by taking `main`'s copy and running
   `cargo update -w`.

**Required checks** (`.github/workflows/ci.yml`, on `pull_request` and
`merge_group`):

| Check | Runs |
| --- | --- |
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| `test` | `cargo nextest run --workspace --all-features`, plus doctests |

Pushes to `main` don't re-run CI, because the queue already ran it. Expensive
suites (the crash suite, the pilot harness, fuzzing) run nightly or in the
merge queue, never as required PR checks.

**Releases** are `v*` tags on `main`. Versions are bumped only in the release
PR, never in feature PRs.

## Rules for every change

- **Stay inside your crates.** Plan items list the crates they own, and
  parallel work depends on that. A change to another crate's public API is a
  separate, small interface PR that lands first.
- **One dependency per line, sorted**, in the root
  `[workspace.dependencies]`. Resolve `Cargo.lock` conflicts by regenerating
  it, never by hand.
- **Changelog fragments:** add `changes/<item-or-pr>.md` (one or two lines).
  Don't edit a shared changelog.
- **Tests come with the change.** Each plan item has a test strategy; durable
  encodings need golden vectors, and stores must pass the conformance suite.
- **Design changes** (anything in `docs/design.md`, or a durable format) are
  their own PR and need the maintainer's sign-off.
- **No secrets** in code, tests, fixtures or logs.

## Local checks

```sh
export CARGO_BUILD_JOBS=4      # on shared hosts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Licence

By contributing, you agree that your contributions are licensed under the
[GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).
