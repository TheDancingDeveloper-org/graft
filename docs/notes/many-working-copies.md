# Many working copies on one host

Work item WI-2313. How to run many agents on one machine with graft, and what
graft does when working copies share a directory with git or with each other.
This note describes current behaviour. It does not change the design
(`docs/design.md` §8.6, §9).

## One clone per agent

graft has no worktree command, and doesn't need one. Each `graft clone` is a
working copy of its own. On its first write it opens a server workspace with a
server-picked name, and holds its own base, head and drafts. To run N agents,
make N clones in N directories. They can all use the same token.

Clones made by one token never share a workspace by accident. The server names
an unnamed workspace from the principal and the request id. The CLI, the LSP
and the MCP server put a random per-process nonce in every request id
(`graft_client::request_id`). Before WI-2313, ids were `cli-<pid>-<n>`, so
two containers whose CLI ran as the same pid, under one token, got the same
workspace.

## Two directories bound to one workspace

Two directories can end up bound to one workspace: through
`graft clone --workspace <name>` with the name of a live workspace, or by
copying a working copy (`cp -r`, including `.graft/`). graft allows the
binding and protects the head by compare-and-swap. It does not refuse the
binding up front.

- Every write (commit, sync, land, stack operations) first checks that the
  server's head is the head this working copy holds. Then it moves the head
  by compare-and-swap.
- The first directory to write wins. The other gets `Diverged` (the client's
  typed error: workspace, server head, local head). The CLI exits 3 (conflict) with kind `diverged`.
  Nothing is overwritten, and the loser's edits stay on its disk.
- After the winner lands, the loser is still refused. It is not silently
  rebased onto the landed change.

To recover the losing directory, clone afresh and copy its edits across. Or
close its binding with `graft workspace close` and let its next write open a
new workspace. One binding, one directory is the supported shape. This is the
same rule as git's "one worktree per branch", enforced at the first write
instead of at bind time. Tests: `crates/graft-client/tests/worktrees.rs`.

## Teardown

A deleted directory does not tell the server anything. Its workspace stays
leased until the lease lapses (2 hours by default). Then the periodic lease
sweep ends it, and its drafts become abandoned. Abandoned drafts can still be
adopted for the draft retention window (7 days), and become collectable after
that.

The documented teardown for an agent session is:

```sh
graft land                 # or keep the drafts: see below
graft workspace close      # releases the workspace now
rm -rf <clone>
```

`graft workspace close` ends the lease straight away. Its drafts are
abandoned at the close time, not at the lease end, so their retention window
starts sooner. `--keep-drafts` keeps them held instead. Agent harnesses (Vogt
session stop, WI-2240) should run `graft workspace close` in the clone before
they delete it. A harness that can't (a crash, a killed container) leaves
nothing worse than a workspace that lapses at its lease.

## Inside a git checkout, or beside git worktrees

Agent harnesses often make git worktrees: Claude Code `isolation: worktree`,
or Vogt's per-session directories. graft works next to them in either layout:

- **Beside them.** A graft clone in its own directory, as in the trial. There
  is nothing to configure.
- **Inside one.** A graft clone made in a directory that is also a git
  checkout. graft never lists, reads or commits top-level `.git` (a directory,
  or the pointer file of a linked git worktree), `.hg`, `.jj` or `.svn`. Each
  `.graft/` holds a `.gitignore` of `*`, so the surrounding git checkout
  ignores graft's state with no rule of its own. Nothing in graft reads
  `.git`; git interop lives only in `graft-git` and `graft-mirror`.

Paths the two tools share (the project's files) are checked out by both. Pick
one tool to commit them. Committing the same edit through both gives two
unrelated histories.

## Disk and clone time for N clones

`python3 -m graftbench --many-copies 1,8,32` (from `bench/`) clones N full and
N sparse copies at once. It compares them against one git clone plus N-1
`git worktree add`, and reports wall time, per-clone latency, and bytes split
into files, cache and other state. The scale-10 result is in
`bench/results/2026-10-23-many-copies-10/`.

What it shows: each graft clone keeps its own `.graft/cache`, so disk grows
linearly with N. git worktrees share one object store, so only the
checked-out files grow. The design's remedy is graft-node, the per-host cache
daemon. Two gaps remain, both left as follow-ups:

- The client writes `.graft/cache` even when its endpoint is a node socket.
  Design §9.1 says the cache is kept only when no node daemon is reachable.
- The bench can't start a node locally, because the node needs the server's
  public key and `graft-server admin` does not print it.
