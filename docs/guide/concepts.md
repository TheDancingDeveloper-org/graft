# Graft concepts for git users

Graft is a server-centric source control system. A working copy contains the
files you edit, while the server owns history, workspaces, drafts, lines, and
the operations that move work between them.

## Glossary

### Repository

A repository is a named namespace on a server. It contains the repository's
objects, changesets, refs, workspaces, and journal.

### Object

An object is an immutable, content-addressed value. A blob holds file bytes;
a tree describes one directory.

### Changeset

A changeset is the unit of work. It names its parents, records file changes
against its first parent, and identifies the resulting root tree.

### Draft

A draft is a changeset that no line has reached yet. `graft commit` creates a
draft from inline content or the working copy, and a workspace holds it while
it is active.

### Landed

A changeset is landed when it is on a line's first-parent history. Landed
changesets remain part of that line's history.

### Line

A line is a protected ref, such as `main` or `release/1.4`, that moves forward
through landed changesets. `graft line create` forks a new line at a changeset
and never overwrites an existing line.

### Ref

A ref is a named pointer to a changeset. Lines are movable refs; tags are
immutable refs.

### Tag

A tag is an immutable name for a changeset. `graft tag create` creates it once
and never moves it.

### Workspace

A workspace is a named, owned, leased server record with a base and a head. It
is the server-side twin of a working copy; `graft workspace` can show, list,
open, renew, or close workspaces.

### Stack

A stack is the drafts between a workspace's base and head, oldest first.
`graft stack` lists, squashes, reorders, splits, or drops those drafts.

### Journal

The journal is the repository's append-only, sequence-numbered record of
writes. Watchers and mirrors follow it without scanning repository history.

### Base

The base is the landed changeset on a line that a workspace starts from.
`graft sync` moves the workspace base to the line tip and rebases its stack.

### Head

The head is the newest draft in a workspace, or the base when the stack is
empty. Working-copy status is measured against the workspace head.

### Land

To land is to put the workspace stack on a line. `graft land` rebases the
stack on the server and moves the line only if its expected version still
matches.

### Sync

To sync is to move the workspace base to the line tip. The server rebases the
stack, and the client updates the working-copy files.

### Line merge

A line merge brings one line into another and records a merge point between
them. `graft line merge` uses a three-way merge and writes unresolved
conflicts into the working copy for resolution.

### Conflict

A conflict is a path that cannot be merged automatically. Text conflicts use
markers in the file; other conflicts use a `.graft-conflict` sidecar until the
workspace records a resolution.

## Git to graft

Graft keeps familiar command names where their meaning is close, but its
server-owned workspaces and drafts replace git's local-history workflow.

| Git term | Graft term | What differs |
| --- | --- | --- |
| Repository | Repository | A remote repository is a named namespace on one server, not a collection of peer clones and remotes. |
| Commit | Changeset; `graft commit` creates a draft | The changeset is created on the server and is visible immediately; there is no later push of a private local commit. |
| Branch | Line for shared history; workspace stack for work in progress | Lines are few protected refs. Ongoing work stays as leased drafts rather than short-lived branches. |
| Tag | Tag | A graft tag is an immutable ref to a changeset and never moves. |
| Working tree | Working copy | The files are the client-side view of a server workspace, often limited to selected path prefixes. |
| Index / staging area | None | `graft commit` records the working copy's changes, or only the paths named by the caller. |
| Push | `graft land` | Land rebases the workspace stack on the server and advances the line with a compare-and-swap. |
| Pull / fetch | `graft sync`; object fetch | Sync moves the workspace base to the line tip and updates files. Object fetch retrieves named content rather than negotiating histories. |
| Merge | Land, or line merge | Ordinary work lands by server-side rebase. A line merge integrates one line into another and records a merge point. |
| Rebase | Server-side rebase during sync or land | The server rebases the workspace stack; the client resolves only paths that cannot merge automatically. |
| Cherry-pick | `graft pick` | Pick creates a draft that re-applies a landed changeset; conflicts are written into the working-copy files. |
| Revert | `graft revert` | Revert creates a draft that inverts a changeset, ready to land like any other draft. |
| Pull request | Review of a workspace stack before land | Graft itself has no pull-request object or branch-per-change requirement; review can use changesets, diffs, and workspace metadata directly. |
