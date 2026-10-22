# graft

graft is a server-centric source control system for workloads made mostly of
short-lived AI agent sessions and CI jobs on shared hosts. It takes the ideas
Anthropic described for Scion in
[*Redesigning source control*](https://www.anthropic.com/engineering/redesigning-source-control)
and starts from a clean slate, in Rust.

> **Status: design and skeleton.** Nothing works yet. The workspace builds,
> and every API body is `todo!()`. The build plan is in
> [docs/plan.md](docs/plan.md).

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
| [docs/plan.md](docs/plan.md) | phased build plan; work items sized for one worker session each, in waves of three |
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
  graft-testkit       test fixtures and harnesses (dev only)
```

## Building

```sh
CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets
cargo run -p graft-cli -- version
```

## Licence

[AGPL-3.0-only](LICENSE).
