# graft

graft is a server-centric source control system for workloads made mostly of
short-lived AI agent sessions and CI jobs on shared hosts. It takes the ideas
Anthropic described for Scion in
[*Redesigning source control*](https://www.anthropic.com/engineering/redesigning-source-control)
and starts from a clean slate, in Rust.

> **Status: phase 1, a git alternative ([D22](docs/decisions.md#d22-phase-1-is-a-git-alternative)).**
> Import, sparse clone, many working copies per host, line creation, commit,
> sync, server-side rebase with line merge, lock-free land, conflict preview
> and resolution, line merge with merge points, pick, tags, log, diff,
> blame, git export, and batch delivery to GitHub (one commit per land)
> all work end to end, through the CLI, MCP (HTTP and stdio) and LSP. GC is
> wired in and off by default. CI
> checks in the land path are built but switched off (phase 2). What is
> delivered, and what is next, is in [docs/status.md](docs/status.md).

## Bench

![graft against git](docs/guide/bench.svg)

One chart from the committed result files: clone, sparse clone, log,
`log --path`, blame, status, sync and concurrent lands at 1,000 and
10,000 commits, plus the real-PR replay and the branching-strategy runs.
How it was measured, and what is not on the chart, is in
[docs/guide/benchmarks.md](docs/guide/benchmarks.md).


## The idea in six lines

- **The server computes.** Commits, history, diff, blame, rebase and landing
  run on the server. The client lays out files and keeps a bounded cache.
- **Changesets, not snapshots.** A changeset lists the files it changed
  against its first parent and commits to its root tree. Writes cost the size
  of the change.
- **Content-addressed everything** (BLAKE3). Any tier can cache any object,
  and any reader can verify it.
- **Lock-free landing.** Refs move only by compare-and-swap. `land` rebases
  on the server and retries there, so a client that loses a race is never
  stranded.
- **Drafts, not branches.** Work in progress lives in leased workspaces, and
  orphans are collected incrementally.
- **One API, four faces:** gRPC, the `graft` CLI, an MCP server for agents,
  and an LSP server for editors.

graft is **not** git-compatible and does not try to be. Command names follow
git where the meaning is the same. Git itself lives only in a migration bridge
(`graft-mirror`), which does lossless import, a two-way GitHub mirror, and a
full export.

## Documents

| Document | What it covers |
| --- | --- |
| [docs/design.md](docs/design.md) | goals, data model, tiers, storage, API, landing, git interop, auth, observability, failure modes |
| [docs/status.md](docs/status.md) | what is delivered, measured numbers, and the phase 1 backlog |
| [docs/plan.md](docs/plan.md) | phases (D22), backlog, and the build waves as history |
| [docs/decisions.md](docs/decisions.md) | open questions for the operator, with options and recommendations |
| [CONTRIBUTING.md](CONTRIBUTING.md) | how changes land |
| [AGENTS.md](AGENTS.md) | rules for agent sessions working in this repository |

## Layout

```text
crates/
  graft-model         ids, paths, blobs, trees, changesets, pure tree algorithms
  graft-proto         API traits, wire types, errors, object frames (gzip)
  graft-store         storage ports, in-memory test store, conformance suite
  graft-store-fs      filesystem ObjectStore
  graft-store-sqlite  SQLite MetaStore
  graft-index         derived data; log, ancestry, blame
  graft-engine        the repository engine (implements the API traits)
  graft-gc            draft expiry and object collection
  graft-auth          principals, tokens, scopes
  graft-telemetry     tracing, metrics, metric names
  graft-server        bin: the authoritative server
  graft-node          bin: the per-host cache daemon
  graft-client        client library: remote API, fetch, working copy
  graft-cli           bin `graft`
  graft-mcp           MCP server over the API traits
  graft-lsp           LSP server over the API traits
  graft-git           git import, export and mapping (the only git-aware crate)
  graft-mirror        bin: git and GitHub mirror
  graft-attester      bin: trial attester, builds and tests drafts, posts Attest (phase 2, off)
  graft-testkit       test fixtures and harnesses (dev only)
```

## Platforms

Release builds (`.github/workflows/release.yml`) produce static binaries for:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

CI (`.github/workflows/ci.yml`) builds and tests on the Linux self-hosted runners only. macOS binaries are built on release; they are not part of the required merge-queue checks. The binary list is `scripts/release/binaries.txt` (`graft`, `graft-server`, `graft-node`, `graft-mirror`).

`cargo install graft-cli` is not available yet: the workspace is `publish = false` and still version `0.0.0`. The release tag publishes `graft-*` crates only, never a crate named `graft`. A Homebrew tap is not part of this release.

## Building

```sh
CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets
cargo run -p graft-cli -- version
```

## Licence

[AGPL-3.0-only](LICENSE).
