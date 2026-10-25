# graft

[![CI](https://github.com/TheDancingDeveloper-org/graft/actions/workflows/ci.yml/badge.svg)](https://github.com/TheDancingDeveloper-org/graft/actions/workflows/ci.yml)
[![Licence: AGPL-3.0-only](https://img.shields.io/badge/licence-AGPL--3.0--only-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/TheDancingDeveloper-org/graft?display_name=tag)](https://github.com/TheDancingDeveloper-org/graft/releases)

graft is a server-centric source control system: many clients share one
history, and the server does the commit, history, diff, rebase and land.
It is for agent fleets — many short sessions on one codebase — that need
git-like commands without a branch or a pull request per change.

**Status: pre-v0.1.0, phase 1 (a git alternative).** Clone, commit,
sync, line-merge land, named-server login and git export work today through
the CLI, MCP and LSP. `graft init` (local mode) and `graft serve --dev` are
not in the binary yet. graft does not run builds, tests or checkers.
What is delivered is in [docs/status.md](docs/status.md).

## Install

There is no crates.io package and no published release yet. Build the CLI
from this repository:

```sh
cargo install --git https://github.com/TheDancingDeveloper-org/graft graft-cli
```

That installs the `graft` binary (clone, commit, sync, land, login, MCP).
`graft-server`, `graft-node` and `graft-mirror` are separate workspace
binaries; build them from a checkout with `cargo build -p graft-cli`.

Release binaries and the container image are produced by the release
workflow when a `v*` tag is pushed. They are not published yet. The
locations that tag will use:

```sh
curl -fsSL -o graft.tar.gz https://github.com/TheDancingDeveloper-org/graft/releases/download/vX.Y.Z/graft-x86_64-unknown-linux-musl.tar.gz
tar -xzf graft.tar.gz
./graft version

docker run --rm ghcr.io/thedancingdeveloper-org/graft:vX.Y.Z version
```

Archives are Linux x86_64, Linux aarch64 and macOS Apple Silicon. There is
no Intel macOS archive. Do not `cargo install graft` or `graft-cli` from
crates.io: those names are not published.

## Quick start: local

`graft init` is specified and not built yet. When it lands, a local
repository is a directory the CLI opens in-process: no server, no daemon,
no token. The commands below are the ones that mode will accept. Until
then, use the server quick start.

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

From the second copy, edit a different region of `file.txt`, commit and
land. Line merge (merge3) keeps both edits. A second land of an overlapping
edit exits 3 with conflict markers in the file.

## Quick start: server

`graft serve --dev` is not built yet. Clone by URL works today against a
server you already run: `graft clone graft://HOST:PORT/REPO`. The token
comes from `graft login`, never from the URL.

```sh
graft login dev
graft clone graft://HOST:PORT/REPO
cd REPO
printf 'hello\n' > readme.txt
graft commit -m "hello"
graft sync
graft land
```

`graft login` reads the token from stdin or `--token-file`, checks the
server, and stores a private token-file reference. It never takes the token
as an argument. When `graft serve --dev` lands it will bind loopback, write
that token into the user config (never on stdout or the command line) and
print the clone command. Running a server today is
[docs/dev-stack.md](docs/dev-stack.md).

## Using it with agents

The agent loop is clone, edit, commit, sync, land. A conflict comes back in
the files (exit 3), not as a pull request. The short guide, including the
exit codes and what never to do, is [docs/agents.md](docs/agents.md).

MCP is `graft mcp` on stdio. Inside a working copy it needs no flags:

```sh
claude mcp add graft -- graft mcp
```

Codex and Cursor snippets (`graft mcp --print-config codex` and
`graft mcp --print-config cursor`), the tool list, and the server `/mcp`
form are in [docs/agents.md](docs/agents.md).

## Migrating from git

Git stays in the mirror. Nothing else in graft speaks git.

| Step | Command | What it does |
| --- | --- | --- |
| Import | `graft-mirror import` | Lossless import of a git repository, including raw tag objects |
| Follow | `graft-mirror follow` | Keep a git remote and graft in step |
| Batch export | `graft-mirror pr-batch` | Deliver lands to GitHub as one pull request, one commit per land |
| Full export | `graft-mirror export` | Write the history back out to git. This is the exit path |

`graft import` and `graft mirror` as subcommands of the one binary are
not built yet. Until then the commands are the `graft-mirror` binary,
which is hidden behind `graft mirror` only when experimental commands are
enabled. Build it with `cargo build -p graft-mirror`.

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
