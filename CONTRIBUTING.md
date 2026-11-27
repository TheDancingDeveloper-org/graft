# Contributing to graft

Thanks for your interest in graft. Bug reports, documentation fixes, tests and
code are all welcome.

## Before you start

- **Bugs and small fixes:** open an issue or go straight to a pull request.
- **New features or design changes:** open an issue first so we can agree the
  approach. The current design is in [docs/design.md](docs/design.md) and the
  reasoning behind past choices is in [docs/decisions.md](docs/decisions.md).
- **Security problems:** do not open a public issue. Follow
  [SECURITY.md](SECURITY.md).

## Sending a pull request

1. Fork the repository and branch from `main`.
2. Make a focused change: one concern per pull request.
3. Run the [local checks](#local-checks) and add tests with your change.
4. Add a changelog fragment as `changes/<short-name>.md` (one or two lines).
   Do not edit `CHANGELOG.md` directly.
5. Open the pull request against `main`. A maintainer reviews it and merges it
   through the merge queue once the required checks pass.

The required checks run on every pull request and in the merge queue:

| Check | Runs |
| --- | --- |
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| `test` | `cargo nextest run --workspace --all-features`, plus doctests |

A change that only touches documentation (`docs/**`, `changes/**`,
`bench/results/**` or `*.md`) skips the Rust steps; the checks still report.
Expensive suites (the crash suite, the pilot harness, fuzzing) run nightly or
in the merge queue, not on every push. Releases are `v*` tags on `main`, and
versions are bumped only in the release pull request.

Maintainers' own delivery process, which uses graft itself, is described in
[docs/internal/landing.md](docs/internal/landing.md).

## Engineering rules

- **Keep a change within the crates it needs.** A change to another crate's
  public API is best sent as a separate, small pull request that lands first.
- **One dependency per line, sorted**, in the root
  `[workspace.dependencies]`. Resolve `Cargo.lock` conflicts by regenerating
  it, never by hand.
- **Tests come with the change.** Durable encodings need golden vectors, and
  stores must pass the conformance suite.
- **Design changes** (anything in `docs/design.md`, or a durable format) are
  their own PR and need a maintainer's sign-off.
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
