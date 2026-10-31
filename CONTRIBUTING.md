# Contributing to graft

graft is early: the design is in [docs/design.md](docs/design.md) and the
work is cut into items in [docs/plan.md](docs/plan.md). Pick an item, or open
an issue to discuss a change to the design first.

## How changes land

graft lands through its own mirror, batched. Workers `g land` on graft line
`backlog-1`. Oversight cuts one batch PR from the line (`graft-mirror
pr-batch`) and merges it with `land-pr --method rebase`, so each land keeps
its own commit (D28). When a backlog of work is under way, all of it goes into
a single batch PR. Batch rules: workspace `.claude/rules/delivery-process.md`
("Landing").

- Graft line: `backlog-1`; mirror: `graft-dev-mirror-graft`
- Required checks: `fmt`, `clippy`, `test` (`.github/workflows/ci.yml`)
- Review: every batch gets a review from a different model than the author
  where an agent wrote it. Blockers go back to the author.
- Every change adds a changelog fragment (see [Rules for every change](#rules-for-every-change)).
- Hand-made PRs (rare, operator-agreed) still squash through the merge queue.

**Required checks** (`.github/workflows/ci.yml`, on `pull_request` and
`merge_group`):

| Check | Runs |
| --- | --- |
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| `test` | `cargo nextest run --workspace --all-features`, plus doctests |

A pull request or merge-queue group whose diff touches only `docs/**`,
`changes/**`, `bench/results/**` or `*.md` files skips the Rust steps of
these jobs (and of `bench-smoke`); the checks still run and report success
(`.github/scripts/docs-only.sh`).

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
