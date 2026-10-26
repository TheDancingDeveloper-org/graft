# graft

[![CI](https://github.com/TheDancingDeveloper-org/graft/actions/workflows/ci.yml/badge.svg)](https://github.com/TheDancingDeveloper-org/graft/actions/workflows/ci.yml)
[![Licence: AGPL-3.0-only](https://img.shields.io/badge/licence-AGPL--3.0--only-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/TheDancingDeveloper-org/graft?display_name=tag)](https://github.com/TheDancingDeveloper-org/graft/releases)

graft is a server-centric source control system: many clients share one
history, and the server does the commit, history, diff, rebase and land.
It is for agent fleets — many short sessions on one codebase — that need
git-like commands without a branch or a pull request per change.

**Status: pre-v0.1.0, phase 1 (a git alternative).** Import, clone, commit,
sync, line-merge land and git export work today through the CLI, MCP and LSP.
Local mode and release binaries are not shipped yet. Named server login,
config and clone-by-URL are available in the CLI. What is delivered is in
[docs/status.md](docs/status.md).

## Install

> **lands in v0.1.0.** There is no GitHub release or container image yet.
> The binary and container commands below use the locations that release will
> publish. Until then, build from a checkout:
> `cargo run -p graft-cli -- --help`.

Release binaries (Linux x86_64 and aarch64; macOS Apple Silicon):

Release CI builds and checksum-verifies all three target archives. There is no
self-hosted Intel macOS runner, so v0.1.0 does not publish an
`x86_64-apple-darwin` archive. The Rust
test suite runs separately on the repository's self-hosted CI runner; the
release matrix does not run target-specific tests from the packaged archives.

```sh
curl -fsSL -o graft.tar.gz https://github.com/TheDancingDeveloper-org/graft/releases/download/v0.1.0/graft-x86_64-unknown-linux-musl.tar.gz
tar -xzf graft.tar.gz
./graft version
```

From crates.io:

`cargo install graft-cli` is not available for v0.1.0. The CLI still depends
on unpublished workspace crates through path dependencies, so this release
ships binaries and the container image only. Publishing the dependency chain
to crates.io is follow-up work.

Container image:

```sh
docker run --rm ghcr.io/thedancingdeveloper-org/graft:v0.1.0 version
```

Today the workspace is four binaries (`graft`, `graft-server`, `graft-node`,
`graft-mirror`). One binary that also serves, caches and mirrors is part of
the same release.

## Quick start: local

> Local mode is in. `graft init` creates a repository the CLI opens
> in-process: no server, no daemon, no token. The commands below are the
> ones that mode accepts.

```sh
graft init demo && cd demo
printf 'one\n' > file.txt
graft commit -m "start"
graft log
graft clone . ../demo-2          # a second working copy of the same repository
printf 'one\ntwo\n' > file.txt
graft commit -m "add a line"
graft land                       # moves the line by compare-and-swap
```

From the second copy, edit a different region of `file.txt`, commit and land.
The server-side line merge (merge3) keeps both edits. Overlapping edits exit
3 with conflict markers in the file.

## Quick start: server

> **lands in v0.1.0** for `graft serve --dev`. Clone by URL works today:
> `graft clone graft://HOST:PORT/REPO`. The token comes from a matching
> `graft login`, never from the URL. A bare repository name still uses the
> default server ([docs/agents.md](docs/agents.md)).

```sh
graft login dev --endpoint http://127.0.0.1:17888 < ~/.config/graft/token
graft clone demo
cd demo
printf 'hello\n' > readme.txt
graft commit -m "hello"
graft land
```

The future `--dev` mode will bind loopback, generate a signing key, write an
admin token into the user config (never on stdout or the command line) and
print the clone command. Today `graft login` reads the token from stdin (or
`--token-file`), verifies the server, and stores a private token-file reference.

## Using it with agents

The agent loop is clone, edit, commit, sync, land. A conflict comes back in
the files (exit 3), not as a pull request. The short guide, including the
exit codes and what never to do, is [docs/agents.md](docs/agents.md).

MCP is `graft mcp` on stdio today. The HTTP `/mcp` endpoint and the tool
list are in [docs/agents.md](docs/agents.md) (checkout-less agents).

## Migrating from git

Git stays in the mirror. Nothing else in graft speaks git.

| Step | Command | What it does |
| --- | --- | --- |
| Import | `graft-mirror import` | Lossless import of a git repository, including raw tag objects |
| Follow | `graft-mirror follow` | Keep a git remote and graft in step |
| Batch export | `graft-mirror pr-batch` | Deliver lands to GitHub as one pull request, one commit per land |
| Full export | `graft-mirror export` | Write the history back out to git. This is the exit path |

> **lands in v0.1.0.** `graft import <git-path>` into a local repository,
> and `graft mirror …` as subcommands of the one binary. Until then the
> commands are the `graft-mirror` binary.

## How it compares

| | graft | git | jj | Sapling | OpenScion |
| --- | --- | --- | --- | --- | --- |
| Unit of history | changeset on a shared line | commit on a branch | change | commit, with a server | changeset (Scion model) |
| Concurrent work | many working copies land on one line | branch and pull request per change | local changes, no server | server-backed stacks | designed for agent fleets |
| Conflict | returned in the files (merge3) | merge or rebase, client-side | client-side | client-side | returned to the client |
| Where the work runs | the server | the client | the client | client plus server | a server |
| Agent surface | CLI, MCP, LSP | CLI | CLI | CLI | not published |
| Status | pre-v0.1.0, phase 1 | mature | mature | mature | a design, not a product |

graft does not run builds, tests or checkers. It does not try to be
git-compatible. Command names follow git where the meaning is the same.

## Benchmarks

![graft against git](docs/guide/bench.svg)

One chart from the committed result files: clone, sparse clone, log,
`log --path`, blame, status, sync and concurrent lands at 1,000 and
10,000 commits, plus a real-PR replay and the branching-strategy runs.
How it was measured, and what is not on the chart, is in
[docs/guide/benchmarks.md](docs/guide/benchmarks.md).

## Documents

| Document | What it covers |
| --- | --- |
| [docs/design.md](docs/design.md) | goals, data model, storage, API, landing, git interop |
| [docs/status.md](docs/status.md) | what is delivered, measured numbers, and what is still open |
| [docs/plan.md](docs/plan.md) | phases, backlog, and the build waves as history |
| [docs/decisions.md](docs/decisions.md) | operator decisions, including phase 1 (D22) and the phase-1 bar (D31) |
| [docs/architecture.md](docs/architecture.md) | crate map, the idea in six lines, runtime and deploy diagrams |
| [docs/agents.md](docs/agents.md) | the agent loop, exit codes, MCP |
| [CONTRIBUTING.md](CONTRIBUTING.md) | how changes land |
| [AGENTS.md](AGENTS.md) | rules for agent sessions working in this repository |

## Contributing

graft is early. The design is [docs/design.md](docs/design.md); work lands
on short-lived branches through the merge queue. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

[AGPL-3.0-only](LICENSE).
