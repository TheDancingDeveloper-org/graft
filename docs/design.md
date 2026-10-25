# graft design

- **Status:** Draft 1 (2026-10-22)
- **Working name:** graft (the operator may rename it; see
  [decisions.md](decisions.md#d1-name))
- **Companions:** [plan.md](plan.md) (phased build), [decisions.md](decisions.md)
  (open questions for the operator)

graft is a server-centric source control system for a workload made mostly of
short-lived AI agent sessions and CI jobs on shared hosts. It takes the ideas
Anthropic described for Scion in
[*Redesigning source control*](https://www.anthropic.com/engineering/redesigning-source-control)
and starts from a clean slate. It is written in Rust. It is not built on
OpenScion, and it is not git with a new transport. Git is a migration bridge
at the edge of the system. Git concepts are not part of the core model.

## Contents

1. [Why build this](#1-why-build-this)
2. [Goals and non-goals](#2-goals-and-non-goals)
3. [Concepts and vocabulary](#3-concepts-and-vocabulary)
4. [Data model](#4-data-model)
5. [Architecture: tiers and surfaces](#5-architecture-tiers-and-surfaces)
6. [Storage](#6-storage)
7. [Server API](#7-server-api)
8. [Landing semantics](#8-landing-semantics)
9. [Client and working copy](#9-client-and-working-copy)
10. [MCP and LSP](#10-mcp-and-lsp)
11. [Git interop](#11-git-interop)
12. [Auth](#12-auth)
13. [Observability](#13-observability)
14. [Failure modes](#14-failure-modes)
15. [Performance budgets](#15-performance-budgets)
16. [Departures from Scion and OpenScion](#16-departures-from-scion-and-openscion)

---

## 1. Why build this

### Our workload

Today the estate runs git, GitHub, and GitHub merge queues, with Vogt starting
and tracking agent sessions. The work is shaped like this:

- Many short-lived agent sessions and CI jobs share a few hosts (Node B,
  node-e, the Windows/WSL runner). Each one clones, edits a small slice, and
  lands.
- Each session wants a small part of the tree and a few answers: what changed,
  who changed this line, is my base stale. It does not want history.
- Landing goes through a merge queue that serializes PRs and re-runs CI. That
  costs minutes per change and causes rebase churn when many workers touch the
  same repository.
- Orchestration (Vogt), review, and CI ask questions about repositories that
  git can only answer from a checkout.

### What git costs us that graft removes

| Pain today | graft answer |
| --- | --- |
| Every session clones, then fetches, then pushes; every shared host holds N copies | A node cache per host shared by every session; sparse checkouts fetch only named objects |
| A lost push race means `pull --rebase` locally, then push again | `land` rebases on the server and retries the compare-and-swap there; the client makes one call |
| Branches as work in progress, which pile up stale on the forge | Drafts that are leased to a workspace and collected when the lease lapses |
| Questions (log, blame, diff, merge base) need a checkout | Every question is an RPC. CI, Vogt and bots need no working copy |
| Packfiles, `gc`, repacking, shallow and partial-clone edge cases | No packfiles. Objects are stored one per id and never rewritten |
| Agents parse porcelain text | A typed API, with `--json` on every command, an MCP server, and an LSP server |

### Honest scale note

Our repositories are small. rustTorrent is 20 MiB and 673 files, and git
serves it in a quarter of a second (see the OpenScion trial,
`openscion/docs/trials/rusttorrent-2026-10-22.md`). graft does not win here on
raw transfer size. It wins on these:

1. landing without a serialized queue;
2. one shared cache per host;
3. API answers with no checkout;
4. an agent-native interface.

Phase 1 has explicit **kill criteria** ([plan.md](plan.md#phase-1-exit-and-kill-criteria)).
If the pilot does not beat git on the paths agents use, we stop.

## 2. Goals and non-goals

### Goals

1. **Cost follows the change.** A commit, rebase, or land costs
   O(changed paths × path depth), never O(repository) or O(history).
2. **Cost follows the read.** A checkout or query fetches only what it names.
   Sparse checkout is the normal case, not a special mode.
3. **Source control is an API.** Every capability is an RPC first. The CLI,
   MCP server, and LSP server are thin clients of the same API. If the CLI can
   do something, the API can too (a parity test enforces this).
4. **Durable from day one.** Every acknowledged write survives a crash or
   restart. There is no in-memory-only mode outside tests.
5. **Lock-free landing.** Refs move only by compare-and-swap on one record.
   There is no global merge lock. The server rebases and retries, and the
   client never strands.
6. **Content addressing everywhere.** Every immutable value is named by its
   hash, so any tier can cache it without coordination, and any reader can
   verify it.
7. **Few refs.** Only protected *lines* (`main`, `release/*`) and immutable
   tags are global. Work in progress is drafts in workspaces, not branches.
8. **Agent-native.** Typed errors, stable exit codes, idempotent operations,
   bounded and paginated responses, JSON everywhere, and Vogt session
   identity carried end to end.
9. **Small to deploy.** One server process plus one node daemon per host,
   with SQLite and a local filesystem. Bigger backends sit behind ports and
   are not required.
10. **A safe way in and out.** Lossless import from git, a two-way mirror to
    GitHub during migration, and a deterministic full export. graft must never
    be a trap.

### Non-goals

- **Being git-compatible.** graft does not speak the git wire protocol, does
  not store git objects, and does not reproduce git behaviour. Command names
  follow git where the meaning is the same, as a familiar starting point. They
  are not a target. See [what we drop](#what-graft-drops-from-git).
- **Offline-first use of a remote repository.** A working copy of a remote
  server keeps a bounded cache, not a replica. Reading cached objects offline
  works; writing does not. A *local repository* is the exception
  ([§5.4](#54-local-repositories), [D32](decisions.md#d32-local-repositories)):
  the engine runs in the `graft` process, so there is no server to be offline
  from. That is single-user and on one machine. It is not a replica of a
  remote repository, and it does not write while disconnected from one.
- **Branch-heavy workflows.** There are no long-lived feature branches. A
  stack of drafts covers what feature branches are used for.
- **Full-DAG history and blame.** Log and blame follow the first parent of a
  line, which is what trunk-based development needs.
- **Code review, code search, and CI.** These stay separate systems. graft
  provides the hooks and queries they need.
- **Anthropic scale in early phases.** No design choice rules it out (storage
  and transport are ports, and the tiers are uniform). Our targets are in
  [§15](#15-performance-budgets).
- **Replacing language servers.** graft's LSP adds source-control features
  next to rust-analyzer and friends. It does not compete with them.

### What graft drops from git

| git concept | graft |
| --- | --- |
| Staging area / index | None. `commit` records the working copy's changes in the slice, or the paths you name |
| Local history, local commits | None on a remote repository: a commit is created on the server and is visible at once. A local repository ([§5.4](#54-local-repositories)) is the whole repository in a directory, not a private history beside a remote |
| Branches for work in progress | Drafts, held by a workspace with a lease |
| push / pull / fetch negotiation | `land` (server rebase plus CAS) and `sync` (move the base); fetch names objects |
| Remotes | One server endpoint per repository (or the node daemon in front of it) |
| Packfiles, `gc`, `repack` | Objects stored individually. Collection is incremental and runs on the server |
| Merge commits for integrating work | Land rebases onto the line. Multi-parent changesets exist only for imported history |
| Stash | A draft in a scratch workspace |
| Detached HEAD, reflog | A workspace's base and head are explicit. The journal records every ref move |
| SHA-1, trees that embed commit objects | BLAKE3 ids. Changesets list changes and commit to a root tree |
| Interactive rebase | `stack` operations on drafts (reorder, squash, split), all on the server |

## 3. Concepts and vocabulary

| Term | Meaning |
| --- | --- |
| **Repository** | A named namespace on a server: its objects, changesets, refs, workspaces and journal |
| **Object** | An immutable, content-addressed value: a *blob* (file bytes) or a *tree* (one directory) |
| **Changeset** | The unit of work. It names its parents and lists file changes against its first parent, and commits to its root tree |
| **Draft** | A changeset no line has reached yet. It is held by a workspace and collectable when nothing holds it |
| **Landed** | A changeset on a line's first-parent chain. It is never collected |
| **Line** | A protected ref (`main`, `release/1.4`) that moves only forward along first parents. Lines are indexed for history |
| **Ref** | A named pointer to a changeset with a version, moved only by compare-and-swap. Lines and tags are refs |
| **Workspace** | A named, owned, leased record: `base` (a changeset on a line), `head` (top draft or none), and the session that holds it. It is the server-side twin of a working copy |
| **Stack** | The drafts between a workspace's base and head, oldest first |
| **Journal** | A per-repository, append-only, sequence-numbered record of every write. It is the source for watchers, mirrors and audit |
| **Derived data** | Values computed from changesets (generation, skip lists, line index, blame). They are versioned, recomputable and invisible to clients |
| **Tier** | server (authority), node (per-host cache daemon), client (library, CLI, MCP, LSP) |

### Verbs

The CLI keeps git's verbs where the meaning is the same, so that people and
agents start from what they know. Where a git verb would mislead, graft uses
its own.

| Verb | Same as git? | What it does in graft |
| --- | --- | --- |
| `clone` | close | Opens a workspace on the server and lays out a sparse working copy from a line's tip |
| `status` | close | Reports working-copy changes against the workspace head. It is stat-based and has no index |
| `diff` | close | Diffs the working copy or any two changesets. Server-side when no working copy is involved |
| `log`, `show`, `blame` | close | Server queries, with no local history |
| `commit` | **different** | Uploads changed blobs and creates a draft on the server. It is visible at once and nothing is pushed |
| `land` | new | Rebases the stack onto the line on the server and moves the ref by CAS, retrying there |
| `sync` | new | Moves the workspace base to the line tip (the server rebases the stack) and updates the files |
| `stack` | new | Lists, reorders, squashes, splits, or drops drafts |
| `revert` | close | Creates a draft that inverts a changeset, rebased onto the tip |
| `workspace` | new | Lists, opens, renews, or closes workspaces |
| `sparse` | close | Adds or removes path prefixes from the slice |
| `api` | new | Calls any RPC raw, with JSON in and out |
| `mcp`, `lsp` | new | Runs the MCP or LSP server over stdio |

## 4. Data model

The encodings below are durable formats. Once data is written in them,
changing them is a migration, so every one carries a format version byte and
is pinned by golden test vectors.

### 4.1 Object ids

Every blob, tree, and changeset is named by a 32-byte **BLAKE3** hash of its
canonical encoding, with the kind separated by BLAKE3's key-derivation mode:

```text
id(kind, payload) = BLAKE3::derive_key(context = "graft 1 " ‖ kind, payload)
kind ∈ { "blob", "tree", "changeset" }
```

- Domain separation by kind means a blob can never collide with a tree that
  has the same bytes.
- The id is computed over the **uncompressed** payload, so the storage and
  transport codec can change without changing any id.
- Text form: `<kind-letter><64 lowercase hex>`, for example `c3fa…` for a
  changeset, `t…` for a tree, `b…` for a blob. Short prefixes (12 hex) are
  accepted where they are unique, as in git, but are never stored.
- Why BLAKE3: it is fast on large inputs and on many cores, and its tree mode
  allows verified streaming of chunked large blobs later. SHA-256 is the
  alternative ([D7](decisions.md#d7-hash-function)).

### 4.2 Paths

A `RepoPath` is UTF-8 with `/` separators. It has no leading or trailing `/`,
no empty, `.` or `..` components, and no NUL. Each component is at most 255
bytes and a whole path at most 4,096 bytes. Paths are case-sensitive and sort
by bytes. The root is not a path. APIs take `Option<RepoPath>` where absent
means the root. A path that breaks a rule is rejected with `InvalidPath` before
any other work.

Paths that a common filesystem cannot represent are rejected when written,
with `InvalidPath { reason: "portability" }`. That covers names that differ
only in case within one directory, and Windows-reserved names (`CON`, `aux.rs`
and so on) when the repository enables `portable_paths`. That way an agent on
the WSL runner never receives a tree it cannot lay out.

### 4.3 Blobs

A blob is file bytes, with no name and no mode. The default maximum is 64 MiB,
configurable per repository. Larger files are rejected with `LimitExceeded`
until chunked blobs arrive (a later phase: a `chunked` blob whose payload lists
chunk ids, so fetch resumes and dedupes inside a file).

### 4.4 Trees

A tree is one directory's entries, sorted by name bytes, with names unique:

| Field | Values |
| --- | --- |
| `name` | one valid path component |
| `kind` | `file`, `exec`, `symlink` (the blob holds the target), `tree`, `submodule` (opaque external pointer; see below) |
| `id` | blob id, tree id, or for `submodule` a 32-byte opaque reference |

Canonical payload: `u8 version(1)`, then for each entry
`kind-byte ‖ u16 len(name) ‖ name ‖ id(32)`. Kind bytes are `f x l d s`.

- A tree is never empty, except the root of an empty repository. Removing the
  last file in a directory removes the directory.
- Clients never write trees. The server derives them.
- `submodule` exists so that git import is lossless for repositories that use
  submodules. graft itself gives it no behaviour beyond storing it and showing
  it.

### 4.5 Changesets

| Field | In the id? | Meaning |
| --- | --- | --- |
| `parents` | yes | Ordered changeset ids. Empty only for a root. Imports may have several roots |
| `root` | yes | The root tree id of the resulting snapshot |
| `changes` | yes | Changes against `parents[0]`, sorted by path |
| `author` | yes | `{ name, email, kind: human \| agent \| bot \| ci }` |
| `authored_at` | yes | Client-supplied time: ms since epoch and UTC offset in minutes |
| `message` | yes | Free text |
| `metadata` | yes | Sorted string map. Keys under `graft.` are reserved (`graft.rebased-from`, `graft.reverts`, `graft.merged-paths`, `graft.merged-from`). Tool keys are namespaced (`vogt.session`, `vogt.work_item`, `review.url`) |

A change is one of:

- `Put { path, kind ∈ {file, exec, symlink, submodule}, id }`: create or
  replace the entry at `path`;
- `Delete { path }`: remove the entry at `path`.

Canonical payload: `u8 version(1)`, `u32 n ‖ parents`, `root(32)`,
`u32 n ‖ changes` (`'p' ‖ str(path) ‖ kind-byte ‖ id` or `'d' ‖ str(path)`),
`author` (`str name ‖ str email ‖ u8 kind`), `i64 ms ‖ i16 offset`,
`str message`, `u32 n ‖ (str key ‖ str value)…`. Integers are big-endian and
`str` is `u32` length ‖ UTF-8.

**Two deliberate choices:**

1. **The id commits to `root`.** Anyone holding a changeset id can check the
   snapshot it names, so a cache or a buggy server cannot serve a wrong tree
   for a correct changeset. The server computes `root` while it validates
   (it has to derive it anyway), so this costs nothing extra.
2. **No server-assigned field is in the id.** `authored_at` comes from the
   client. Retrying a `CreateChangeset` after a timeout yields the same id, so
   creation is idempotent and agents can retry blindly. When the server
   received it, the log position, and the state live in a separate *record*
   (§4.7), not in the content.

**Validity rules.** The server rejects a request that breaks one, with
`InvalidChange { path, rule }`:

1. every parent exists in this repository;
2. a parentless changeset is allowed only for import (an admin scope) or an
   empty repository;
3. each path appears at most once and passes the path rules;
4. every referenced blob exists (or is supplied inline in the same request);
5. `Delete` names an existing entry in `parents[0]`'s snapshot (to delete a
   directory, delete every file under it; the API has a `DeletePrefix`
   convenience that expands on the server);
6. a `Put` is not a no-op;
7. the result is a well-formed tree, with no path that is both a file and a
   directory;
8. the change count is within the limit (100,000 by default);
9. a changeset with no changes is allowed only with more than one parent,
   or — for a git import only ([D33](decisions.md#d33-empty-imported-changesets)) —
   with fewer than two parents (a single parent or none) when metadata carries
   `mirror.git-commit` ([D19](decisions.md#d19-import-identity)). The native
   write boundary rejects any `mirror.*` key, so a native client cannot set
   `mirror.git-commit` and a native empty changeset is still rejected. The
   encoding is unchanged, and the golden vectors for native changesets are
   unchanged.

### 4.6 Refs and lines

A ref is `{ name, target, version: u64, kind }`, stored as **one record**.

- It moves only by `CompareAndSwap { name, expected: Option<(target, version)>, new }`.
  A stale expectation returns `RefConflict { actual }` and changes nothing.
- `kind = line`: the move must be a first-parent fast-forward (`new` has the
  current target on its first-parent chain). That makes the *line index*
  valid forever. Only `Land` and admin `UpdateRef` move a line.
- `kind = tag`: created once (`expected = None`), then immutable. Deletion is
  an admin action and is journalled.
- `kind = tool`: unrestricted moves, for a small number of tool-owned pointers.
  These are not indexed.
- No operation moves two refs atomically. Nothing needs to.
- Expected population: a handful per repository. Workspaces are **not**
  refs. They are per-owner records and never need global agreement.

### 4.7 Changeset records and draft lifecycle

Alongside the immutable content, the server keeps a mutable **record** per
changeset: `{ lsn, received_at, state, holder, superseded_by, expires_at, pins }`.
The last two serve collection (§8.6).

```text
                 land / import onto a line
    draft ─────────────────────────────────────────► landed   (terminal, never collected)
      │  rebased by land/sync (new draft replaces it)
      ├─────────────────────────────► superseded ─┐
      │  workspace closed or lease lapsed         │ after the retention window
      └─────────────────────────────► abandoned ──┴──► collected (record kept as a tombstone)
```

- **draft:** held by a workspace (as head or inside its stack) or by an
  in-flight land attempt (a short internal lease).
- **landed:** reached by a line. The record notes the line and position.
- **superseded:** replaced by its rebased successor (`superseded_by`). It is
  kept for the retention window so that "what did my draft become?" always
  has an answer.
- **abandoned:** no holder, because its workspace closed or the lease lapsed.
- **collected:** the GC has removed its content and any objects that only it
  referenced. A tombstone `{ id, collected_at, reason }` remains, so a late
  reader gets `Collected`, not a confusing `NotFound`.

### 4.8 The journal

Each repository has one append-only journal with strictly increasing **LSNs**:

```text
ChangesetCreated { id, state, workspace? }   RefMoved { name, old, new, version, actor }
WorkspaceOpened/Renewed/Closed { name }      LandAttempt { draft, ref, outcome }
Collected { ids, objects, bytes }            AdminAction { what, actor }
LineMerged { src, dst, src_changeset, src_pos, dst_pos }   (§8.9)
```

- An entry is written in the same metadata transaction as the change it
  records, so the journal can never miss a ref move.
- A changeset's parents always have lower LSNs, so the journal is a
  topological order.
- `Watch { from_lsn, filter }` streams the journal. The mirror, the node
  prefetcher, Vogt, and CI triggers all consume it and resume from their last
  LSN.
- Retention: ref moves, admin actions and landed-changeset entries are kept
  forever. Draft-only entries are compacted after the retention window.

### 4.9 Derived data

Everything that is not stored content is a derived kind:

```text
derive(kind@v, c) = f( c, [derive(kind@v, p) for p in needed_parents(kind, c)] )
```

Each kind declares its parents (first or all), a version, and a mode: eager
(computed before the write is acknowledged) or lazy (computed on first use,
walking back only to the nearest computed ancestor). Values are keyed by
`(kind, version, changeset[, path])`. Bumping a version recomputes lazily. No
client ever sees how a value is computed.

| Kind | Parents | Mode | Value | Serves |
| --- | --- | --- | --- | --- |
| root tree | first | eager (in the id) | snapshot root | fetch, reads, validity |
| generation | all | eager | `1 + max(parents)` | ancestry pruning |
| skip list | first | eager | first-parent ancestors at distance 2^k | `Log` ranges, first-parent ancestry in O(log n) |
| line position | first | eager on land | `(line, pos)` for landed changesets | O(1) ancestry on a line; history index key |
| line path index | first | eager on land | rows `(line, path-or-dir-prefix, pos)` for each changed path and its ancestor directories | `Log --path`, file history, directory history |
| blame | first | lazy, cached | per `(changeset, path)`: line → origin changeset | `Blame` |
| git ids | first | lazy, owned by the mirror | graft id ↔ git sha | interop only, kept outside the core ([§11](#11-git-interop)) |
| later: symbols, copies, content search | first | lazy | tree-sitter tags, rename detection, trigram index | LSP workspace symbols, MCP search |

The **line path index** is the main departure from a git-shaped history walk.
Because a line moves only forward, a landed changeset's position never
changes. History for `path` on `main` is therefore a single index range scan.
It costs O(results), not O(history). For a draft (not on a line), the server
walks first parents to the line (at most the stack depth), then switches to
the index.

**Fork points** ([D23](decisions.md#d23-line-fork-points)). A line created on
existing history does not copy its parent's rows. Creating it indexes only the
unlanded stretch it lands, and writes one `line_forks(line, parent, pos)` row
pointing at a line that holds the first landed ancestor in its own rows. Reads
below the fork fall through to the parent, bounded at `pos`: `line_pos` and
the path scan try the line's own rows first, then follow the fork one hop at a
time. Positions are first-parent depth, so a changeset has the same position
on every line that holds it, and no renumbering is needed. Creating a line
costs the unlanded stretch plus one indexed read; queries cost their results
plus one fork read per hop. A forked line's `line_pos` and `line_paths` rows
are only its own lands.

### 4.10 Invariants

1. **Immutability:** the bytes behind an id never change. Only GC deletes
   content, and only content that is not landed and not held.
2. **Verifiability:** every component hashes what it receives and rejects a
   mismatch with `CorruptObject`.
3. **Proportional writes:** creating a changeset, including eager derived
   data, is O(changes × depth).
4. **Topological journal:** parents precede children.
5. **Lines only move forward**, along first parents.
6. **Refs are separate:** creating a changeset never moves a ref, and moving
   a ref never creates content.
7. **Determinism:** the same inputs give the same ids and derived values on
   every machine. No server clock or randomness feeds an id.
8. **No full materialization:** no operation walks or holds a whole snapshot.
   Recursive listings and diffs are paginated and pruned by subtree id.
9. **Landed is forever:** nothing reachable from a line is ever collected
   (except by an explicit, journalled `Obliterate` of one blob; see
   [§14](#14-failure-modes)).

## 5. Architecture: tiers and surfaces

```text
  agents · CI jobs · humans · Vogt · editors
     │            │              │            │
   graft CLI   graft mcp      graft lsp    any RPC client (Vogt, bots)
     └────────────┴───── graft-client (lib) ──┘
                    │  same RPCs over a Unix socket
              graft-node (one per host)   ← disk object cache shared by every session
                    │  same RPCs over the network (gzip)
              graft-server (authority)    ← engine, index, GC, auth, journal, /mcp
                    │
         MetaStore port          ObjectStore port
      (SQLite → Postgres)     (filesystem → S3-compatible)

              graft-mirror (separate service) ⇄ GitHub   ← the only place git exists
```

### 5.1 Tiers

**graft-server: the authority.** It owns changesets, refs, workspaces, the
journal, derived data, and GC. It is the only component that creates
changesets or moves refs. It is stateless over its stores. With SQLite it runs
as one process per host. With the Postgres port it can run several replicas,
because refs are single-record CAS and creation needs no coordination.

**graft-node: one daemon per host.** It serves every session on the host over
`/run/graft/node.sock`.

- It keeps a bounded on-disk object cache (LRU by bytes, default 20 GiB) in
  the same frame format the server stores and the wire carries, so it streams
  objects without transcoding.
- It coalesces identical in-flight requests (single-flight). Thirty sessions
  cloning the same slice cause one upstream fetch.
- It forwards every repository (mutable) RPC to the server unchanged, with
  the caller's token. It never caches mutable answers.
- It optionally follows the journal (`Watch`) for configured lines and
  prefetches each new tip's trees and changed blobs. Fresh checkouts then hit
  close to 100%.
- Its cache is just a cache. It survives restarts, and if it is corrupt it is
  wiped and refilled. Every object is verified on read.
- Strict tiering: it calls only the server (or a future regional tier). It
  bounds upstream concurrency and sheds load with `Unavailable` rather than
  queueing without limit.

**Client: the `graft-client` library.** It holds connection and token
handling, the fetch protocol, a local object cache (used only when there is no
node daemon), the working copy, sparse patterns, and status. The CLI, MCP
server and LSP server all link it. A job that needs only answers uses the API
and never makes a working copy.

**Uniform contract.** Object RPCs return the same results at every tier. A
client finds the nearest endpoint (`GRAFT_ENDPOINT`, else the node socket if it
exists, else the configured server) and never needs to know the topology.

### 5.2 Surfaces: one API, four faces

`graft-proto` defines the API as Rust traits (`ObjectApi`, `RepoApi`,
`WorkspaceApi`, `AdminApi`) plus their request, response and error types.
Everything implements or consumes those traits:

- the engine implements them in-process (tests, the server, `/mcp` on the
  server);
- the remote client implements them over the wire;
- the CLI, MCP server and LSP server are written against the traits, so each
  works in-process or remote with no change.

A **parity test** lists every API operation and asserts that it has a CLI
command and an MCP tool, or an explicit, documented exclusion.

### 5.3 Crate map

The crate boundaries are drawn so that three workers can build in parallel
without touching the same files ([plan.md](plan.md#crate-ownership-and-parallelism)).

| Crate | Kind | Owns | Depends on |
| --- | --- | --- | --- |
| `graft-model` | lib | ids, paths, blobs, trees, changesets, canonical encodings, pure tree algorithms (apply changes, diff trees) over an `ObjectSource` trait | — |
| `graft-proto` | lib | API traits, wire types, typed errors, limits, protocol version, codec frames | model |
| `graft-store` | lib | `ObjectStore` and `MetaStore` ports, the in-memory test store, the port conformance suite | model |
| `graft-store-fs` | lib | filesystem `ObjectStore` (server and node) | store, model |
| `graft-store-sqlite` | lib | SQLite `MetaStore`, schema and migrations | store, model |
| `graft-index` | lib | derived-data framework and kinds; `Log`, ancestry, `Blame` | store, model |
| `graft-engine` | lib | `RepoApi`/`WorkspaceApi` implementation: create, reads, diff, rebase, land, workspaces, journal, ACL checks | index, store, model, proto, auth |
| `graft-gc` | lib | draft expiry, object holder accounting, sweep | store, model |
| `graft-auth` | lib | principals, tokens, scopes, path rules | model, proto |
| `graft-telemetry` | lib | tracing and metrics setup, the metric name registry | — |
| `graft-server` | bin | transport, wiring, config, admin subcommands, server-hosted MCP | engine, gc, auth, telemetry, proto, store-fs, store-sqlite, mcp |
| `graft-node` | bin | node cache daemon | proto, store-fs, auth, telemetry |
| `graft-client` | lib | remote API client, fetch, working copy, sparse, status | proto, model, store-fs |
| `graft-mcp` | lib | MCP server over the API traits | proto |
| `graft-lsp` | lib | LSP server over the API traits and the working copy | proto, client |
| `graft-cli` | bin `graft` | commands; `graft mcp` and `graft lsp` | client, mcp, lsp, proto, telemetry |
| `graft-git` | lib | git import, export, and id mapping. The only crate that knows git | model, proto |
| `graft-mirror` | bin | follow, batch PR export (D21), export mirror, authority switch (the per-land PR bridge was removed under D21/D22) | git, proto, client, telemetry |
| `graft-testkit` | lib (dev only) | fake clock, synthetic repos, crash harness, pilot harness | model, store, proto |

### 5.4 Local repositories

A local repository is a repository whose storage is a directory on the
machine that runs `graft`, opened by the same process. Decision
[D32](decisions.md#d32-local-repositories). It exists so one binary can be
tried with no server, no node daemon, no mirror and no token. It is not a
second engine and not a cache of a remote.

**Where it lives.** `graft init [DIR]` creates two things:

- the repository directory, at `DIR/.graft/repo`: SQLite metadata
  (`meta.sqlite`, the same schema and migrations as the server) and the
  filesystem object tree (`objects/`, the same frames as
  `graft-store-fs`). One repository, one directory. `DIR` defaults to the
  current directory. The repository name is `--repo`, defaulting to the
  directory's name. There is no `--bare` form.
- the working copy in `DIR`, whose configured server is the literal
  `local:<path-to-repo>`. A second working copy is `graft clone
  <path-to-repo>` (or `graft clone <working-copy>`, which reads that
  config). Clone of a local path never contacts the network.

**How the CLI opens it.** When the working copy's server is `local:…`, the
CLI constructs `graft-engine`'s in-process `RepoApi` over `graft-store-sqlite`
and `graft-store-fs`, the same wiring the server uses, and passes that trait
object to the commands. No HTTP, no socket, no token. The process takes an
exclusive lock file (`repo.lock`, `flock`) for the life of the command; a
second process exits with a typed busy error instead of opening the store.
That includes a long-running `graft mcp` stdio session: while it holds
the lock, every other command on this repository exits busy.
One process at a time is the whole concurrency story: there are no leases to
renew across processes, and the single local principal is implicit (no auth
crate on this path).

**What is unchanged.** CAS lands, one transaction per logical write, the
journal, objects durable before metadata names them, id-pruned diffs, GC off
by default, typed errors, bounded responses. A conflicting land still exits
3 and is resolved by the client. `kill -9` during a land loses no
acknowledged write, because durability is the store's, not the transport's.
Commit, sync, land, log, diff, blame, stack, revert, pick, tag and line all
go through that in-process API. `graft import <git-path>` and `graft mirror
export` use the same API, so a local repository imports and exports without
a server.

**Attaching to a server later.** The working copy's server value is the only
switch. Changing it from `local:…` to an endpoint makes the next command use
the remote client, which still needs a server, a token and a clone of that
remote. Nothing in this design copies a local repository onto a server.
`graft push-to-server` (promote a local repository to a remote, history
intact) is a follow-up, not part of this item.

**Out of scope locally.**

- No node daemon and no local object cache in front of the repo: the process
  is the server.
- No HTTP server and no `/mcp` HTTP. `graft mcp` over stdio does work: it is
  the same trait object the CLI holds.
- No tokens, no ACL, no multi-user, no two processes in one repository.
- No offline writes against a *remote* repository. The offline-first
  non-goal stands for those.
- No GC on by default, same as the server.

## 6. Storage

### 6.1 Ports

The engine never names a backend. Ports are Rust traits in `graft-store`.

**`ObjectStore`** holds content-addressed frames:

| Method | Contract |
| --- | --- |
| `put(repo, id, frame)` | put-if-absent; durable when it returns; idempotent |
| `get(repo, id)` | the frame or `None`; the caller verifies |
| `has(repo, ids)` | the subset present |
| `delete(repo, id)` | GC only; idempotent |
| `stat()` | bytes, objects, free space, for metrics and admission |

**`MetaStore`** holds everything else. It is deliberately narrow: single-key
reads, single-record CAS, and a transaction scoped to *one write plus its
journal entry*. That scope is what lets it move later to a sharded store.

| Area | Operations |
| --- | --- |
| changesets | insert content and record (idempotent on id); get; update record state |
| refs | get, list(prefix), `cas(name, expected, new)` with a journal entry in the same transaction |
| workspaces | open, get, list(owner), cas head/base, renew lease, close |
| journal | append (inside the write transactions above), `read(from_lsn, limit)`, `tail` notifications |
| derived | get/put by `(kind, version, key)` |
| line index | append rows on land (same transaction as the ref CAS); one fork row on line create; range scans that fall through forks |
| holders | add/remove `(object, holder)` rows, read by object or by holder; parent pins; uploads and claims with expiry; doomed and deleting marks (§8.6) |
| lands | idempotency records `(draft, ref) → landed` |

Every port ships with the **conformance suite** in `graft-store`. Each
implementation must pass it, including crash tests through the testkit's
process harness.

### 6.2 Implementations

| Port | Phase 1 | Later |
| --- | --- | --- |
| `ObjectStore` | filesystem (`graft-store-fs`) | S3-compatible (MinIO, R2), if a second host or off-box durability needs it |
| `MetaStore` | SQLite (`graft-store-sqlite`) | Postgres, only for multi-replica servers ([D3](decisions.md#d3-storage-backend)) |
| both | in-memory, **tests only**, behind a `testkit` feature that the server binary does not enable | — |

### 6.3 Object frames: one format at rest and on the wire

Transport compression is **gzip** (operator, 2026-10-22). To avoid transcoding
at any tier, an object is stored in exactly the frame it travels in:

```text
frame = magic "GRF1" ‖ u8 kind ‖ u8 codec (0 = raw, 1 = gzip) ‖ u64 uncompressed_len ‖ body
```

- Bodies of 512 bytes or more are gzip-compressed (level 6) when that saves at
  least 10%. Otherwise they are stored raw (codec 0). That avoids gzip
  overhead on tiny files and on content that is already compressed.
- The id is over the uncompressed payload, so the codec can change later
  ([D5](decisions.md#d5-at-rest-codec)) without changing ids.
- The server, node, and client stream frames byte for byte. Only the final
  consumer decompresses and verifies.
- Non-object RPC messages use transport-level gzip (gRPC `grpc-encoding: gzip`,
  or HTTP `Content-Encoding: gzip`), negotiated, for messages over 1 KiB.

### 6.4 Filesystem object store

- Layout: `<root>/<repo>/objects/<2 hex>/<62 hex>`. The kind is in the frame
  header.
- Write path: write a temp file in the target directory → `fsync` → `rename`
  (no replace; if the target exists, discard the temp file) → `fsync` the
  directory. A crash leaves at most stray `*.tmp` files, which startup removes.
- Read path: read the frame, then verify the hash when decompressing. The
  server verifies on every read. The node verifies when it fills and when it
  serves.
- No packing, no rewriting, no maintenance. Backups are incremental for free,
  because files are immutable.

### 6.5 SQLite metadata store

- WAL mode, `synchronous=FULL`, `foreign_keys=ON`. One writer connection on a
  dedicated thread, fed by a bounded queue, plus a read connection pool.
- The schema sketch (owned by `graft-store-sqlite`, migrated by version):
  `repos`, `changesets(repo, id, content, lsn, received_at, state, holder, superseded_by, expires_at, pins)`,
  `refs(repo, name, target, version, kind)`, `journal(repo, lsn, kind, body)`,
  `workspaces(repo, name, owner, session, base, head, lease_until)`,
  `derived(repo, kind, ver, key, value)`, `line_pos(repo, line, changeset, pos)`,
  `line_paths(repo, line, path, pos)`, `line_forks(repo, line, parent, pos)`
  (schema version 5, one row per forked line),
  `line_merges(repo, dst, dst_pos, src, src_changeset, src_pos)` (schema
  version 7, one row per merge landing, §8.9), `holders(repo, object, holder)`,
  `uploads(repo, object, owner, expires)`, `doomed(repo, object, since, deleting)`,
  `lands(repo, draft, ref, landed, at)`, `tombstones(repo, id, at, reason)`.
- Capacity: one writer handles thousands of small transactions a second on
  NVMe. Our peak (hundreds of lands per hour per repository) is far below that.

### 6.6 Write ordering and crash consistency

**The rule: objects are durable before any metadata references them, and the
metadata transaction is the commit point.**

1. Blobs are uploaded (`PutObjects`) and are durable when acknowledged. Each is
   recorded as an *upload* with an expiry and the uploader.
2. `CreateChangeset` derives trees, writes them to the object store, claims
   any referenced object that is doomed (§8.6), and then runs one metadata
   transaction: content, record, holders, eager derived
   data, journal entry.
3. `Land` writes its rebased candidate as in step 2, then runs one transaction:
   ref CAS, line index rows, record states, land record, journal entry.

A crash before a commit point leaves only unreferenced objects. These are
uploads whose expiry the GC honours. A crash after it is fully recovered by
SQLite's WAL. **Every acknowledged operation survives restart. This is
tested by `kill -9` during load, from the first phase.**

### 6.7 Backup and restore

- Metadata: SQLite online backup to a file every hour, plus a continuous WAL
  shipper (Litestream or equivalent, [D3](decisions.md#d3-storage-backend)).
- Objects: incremental file sync (restic) of the immutable object tree.
- Restore order: take the metadata snapshot *first*, then the objects
  (objects are a superset). `graft-server fsck` verifies after a restore that
  every referenced object exists and hashes correctly.
- During migration, GitHub (through the mirror) is a second, independent
  copy of every landed changeset.

## 7. Server API

### 7.1 Conventions

- One request struct and one response struct per operation, both
  `#[non_exhaustive]`, so fields can be added compatibly.
- Every request names the `repo`, and every request carries a
  client-generated `request_id` (for logs and idempotency).
- Every list is paginated (`limit`, `page_token`). Every response has a
  documented size limit. Nothing returns an unbounded result.
- Protocol version `(major, minor)`. A client binds when majors match and the
  server minor is at least the client's. Adding a field or an operation is a
  minor bump. Anything else is a major bump.
- Errors form a closed, typed set ([§7.10](#710-errors)). An error never
  carries file contents or tokens.
- The wire protocol is gRPC over HTTP/2, with gzip ([D4](decisions.md#d4-wire-protocol)),
  over TCP or a Unix socket. A read-only JSON facade (`/v1/...`) serves simple
  tools and scripts.

### 7.2 Operations

**Object API (every tier answers it):**

| Op | Request → Response | Notes |
| --- | --- | --- |
| `GetObjects` | `ids` → stream of `frame`, then `missing` | Partial results are normal; a tier forwards exactly its misses. At most 1,024 ids and 64 MiB per call; the remainder is reported as `deferred` |
| `HasObjects` | `ids` → `missing` | Clients call it before uploading |
| `PutObjects` | stream of blob frames → `ids` | Blobs only (trees are derived). Idempotent. Records an upload lease. A tier forwards it |

**Repo API (server; the node forwards it):**

| Op | Request → Response |
| --- | --- |
| `CreateChangeset` ("commit") | `parents, changes, author, authored_at, message, metadata, inline_blobs?, workspace?` → `{ id, root, lsn, state }` |
| `GetChangesets` | `ids` → changesets plus records, `missing`, `collected` |
| `ResolveRef` / `ListRefs` | `name` / `prefix` → `{ target, version, kind }` |
| `UpdateRef` | admin or tool refs: `name, expected, new` → `{ version }` or `RefConflict` |
| `Stat` / `ListTree` / `ReadFile` | read a path in a changeset's snapshot without fetching trees; `ListTree` is paginated, and recursive listings are capped per page |
| `Log` | `from, until?, paths?, limit, page` → changeset summaries, newest first, along first parents |
| `IsAncestor` / `MergeBase` | ancestry over all parents, pruned by generation and line positions |
| `Diff` | `from, to, paths?, patch?: { context, max_bytes }` → file changes, plus optional unified patches for text files |
| `Blame` | `at, path, lines?` → line ranges with origin changeset, author and authored time |
| `Rebase` | `changeset, onto, merge?: file \| line` → `{ id }` or `Conflict` |
| `Revert` | `changeset, onto, workspace?` → `{ id }` or `Conflict` |
| `Land` | `head, line, policy?, deadline` → `Landed`, `Conflict`, `Contended`, or `Rejected` ([§8](#8-landing-semantics)) |
| `Watch` | `from_lsn, filter` → stream of journal entries |

**Workspace API:**

| Op | Request → Response |
| --- | --- |
| `OpenWorkspace` | `name?, line, at?, session?, lease` → workspace (`base` = line tip by default) |
| `GetWorkspace` / `ListWorkspaces` | by name / owner |
| `RenewLease` | `name, lease` → `lease_until` (the client heartbeats it; Vogt can renew it for a session) |
| `SetHead` | `name, expected_head, new_head` (CAS) — used by `commit` and `stack` ops |
| `Sync` | `name, onto?` → new `base`, rebased `head`, the list of changed paths, or `Conflict` |
| `Stack` | `name, op: reorder \| squash \| split \| drop` → new head |
| `CloseWorkspace` | `name, keep_drafts?` → its drafts become abandoned unless kept |
| `OverlappingDrafts` | `line, paths, include_own?, limit, page?` → `(path, workspace)` hits in index order, plus owner, session, work item, draft age and lease end per workspace. Needs `repo:<r>:drafts`; live leased workspaces only; never contents or head ids ([D24](decisions.md#d24-draft-overlap-visibility)) |

**Admin API:** `CreateRepo`, `ImportChangesets` (bulk, preserves parents and
allows roots), `Gc { dry_run }`, `Fsck`, `Obliterate { blob, reason }`,
`MintToken`, `RevokeToken`, `SetPolicy`.

### 7.3 Fetch

graft has no negotiation. The client names what it wants:

1. Resolve the target: `ResolveRef`, or `GetWorkspace` for its base, or a
   changeset id the client already holds. Its `root` is in the changeset.
2. If the local store (node or client cache) lacks the root tree, request it.
3. For each tree entry under the sparse prefixes, collect the missing subtree
   ids and request them in batches, level by level (breadth-first, pipelined:
   the next level's requests go out as frames arrive).
4. Collect the missing blob ids under the prefixes and request them in
   batches.
5. Verify every frame against its id before it is stored or written out.

Every request is for immutable objects, so it caches at every tier and resumes
by asking again for what is still missing. Updating a working copy from base A
to base B is `Diff(A, B, paths = slice)` followed by fetching the blobs it
names. The cost tracks the change, not the slice.

### 7.4 Commit

`CreateChangeset` validates the rules in §4.5 and derives the root by path
copying. Only the trees on the changed paths are rewritten, and untouched
subtrees keep their ids. It writes the new trees, commits the metadata
transaction, and returns. With `workspace` set, it also compare-and-swaps the
workspace head from `parents[0]` to the new id in the same transaction, so a
draft never exists without a holder.

API-only commits, for bots, revert pipelines and MCP agents with no checkout,
send small contents inline (`inline_blobs`, 4 MiB total), so a whole commit is
one RPC.

### 7.5 Log

`Log(from, until?, paths?)` walks the first-parent chain newest first:

- `from` on a line: a range scan of `line_paths` (with paths) or `line_pos`
  (without), from `pos(from)` down to `pos(until)`. Cost is O(results).
- `from` is a draft: walk first parents to the line (bounded by stack depth),
  filter that stretch by path, then continue on the index.
- A directory path matches everything beneath it, because the index stores
  directory prefixes.

### 7.6 Diff

`Diff(from, to)` diffs two root trees and recurses only into entries whose ids
differ. Cost is proportional to the differing subtrees. With `patch`, the
server produces unified diffs for text blobs (histogram diff), capped by
`max_bytes` per file and per response. `Diff` against `MergeBase(head, line)`
is how CI selects targets without a checkout.

### 7.7 Blame

`Blame(at, path)` uses the line path index to list the versions of `path` (the
changesets that touched it), oldest first from the nearest cached blame. It
diffs successive versions (histogram diff) to carry line origins forward. It
caches the result per `(changeset, path)` as derived data. Cost is the number
of versions of that file since the nearest cached value, times its size. The
length of history does not matter. Whitespace-only and move detection are
options for a later version of the kind.

### 7.8 Rebase

`Rebase(c, onto)` re-applies `c`'s changes, relative to `base = c.parents[0]`,
onto `onto`. For each path `p` that `c` changes:

| `base[p]` vs `onto[p]` | `c[p]` vs `onto[p]` | Result |
| --- | --- | --- |
| equal | — | apply `c[p]` |
| differ | equal | drop: the change is already there |
| differ | differ, and both are text and `merge = line` | three-way line merge (diff3 over the histogram diff). If clean, put the merged blob and record the path in `graft.merged-paths`. Otherwise it is a conflict |
| differ | differ, otherwise | conflict |

**Files and directories.** Changes are file-level, so the table compares
file entries: an entry is its kind and id, and a directory at `p` counts as
no entry. That gives these results:

- Both sides make the same change, including the same file-to-directory
  change: dropped, because it is already there.
- We deleted file `a`, and `onto` turned `a` into a directory: our deletion
  is dropped, because it is already done. The result keeps `onto`'s
  directory.
- We edit file `a`, and `onto` turned `a` into a directory (or the reverse):
  the entries at `a` differ, so it is a conflict by the table.
- Both sides turned `a` into a directory: each file inside is its own path,
  compared by the table.

A *clash* is a result that would need one path to be both a file and a
directory, for example we add `a/c` where `onto` has file `a`. A clash is
always a conflict. It is found when the kept changes are applied to `onto`
(validity rule 7), and reported at that path.

Any conflict fails the whole rebase with
`Conflict { paths: [{ path, base, ours, theirs }] }` (ids, not contents) and
creates nothing. On success the result keeps `c`'s author, `authored_at`,
message and metadata, and adds `graft.rebased-from = c`. It is deterministic,
so a retried rebase returns the same id. A stack rebases bottom-up, and a
conflict reports the first draft that failed.

Cost: O(changes × depth) plus the size of any line-merged files. It never
depends on the distance between `base` and `onto`.

### 7.9 Idempotency

| Op | How retries are safe |
| --- | --- |
| `PutObjects` | put-if-absent by id |
| `CreateChangeset` | the id is content-only (no server fields), so a repeat returns the existing id |
| `Rebase` / `Revert` | deterministic output id |
| `Land` | land record `(draft, line) → landed`. A repeat returns the earlier `Landed` |
| `SetHead`, `UpdateRef` | CAS: a repeat after success sees `actual == new` and reports success |

### 7.10 Errors

`NotFound{what}` · `Collected{id}` · `InvalidPath{path, reason}` ·
`InvalidChange{path, rule}` · `UnknownParent{id}` · `MissingObjects{ids}` ·
`RefConflict{name, actual}` · `Conflict{paths}` · `Contended{line, tip}` ·
`Rejected{policy, detail}` · `PermissionDenied{scope, path?}` ·
`LeaseExpired{workspace}` · `LimitExceeded{limit, max}` · `CorruptObject{id}` ·
`Unavailable{retry_after}` · `Unauthenticated` · `IncompatibleProtocol{server}` ·
`Obliterated{id}`.

## 8. Landing semantics

### 8.1 Server-side land

`Land(head, line, policy, deadline)` lands a workspace's stack (or any draft
chain whose bottom has a landed first parent) onto a line:

```text
if lands[(head, line)] exists: return Landed(that)                  # idempotent
check scopes: land:<line>, and write on every changed path in the stack
evaluate policy pre-checks (§8.3); on failure return Rejected
loop until deadline or the attempt budget (default 16) runs out:
    tip ← ResolveRef(line)
    candidate ← head if stack.bottom.parent == tip else Rebase(stack, onto = tip)
        on Conflict: journal LandAttempt(conflict); return Conflict{paths, tip}
    write candidate content (holder: land-attempt lease, 10 min)
    txn: CAS(line, expected = tip, new = candidate)
         + line index rows + record states (landed / superseded) + lands row + journal
    on success: return Landed{ landed: candidate, line_version, pos, rebased: bool, merged_paths }
    on RefConflict: back off (full jitter, 2 ms base, 200 ms cap) and loop
return Contended{ line, tip }
```

- A lost race costs one rebase on the server, with no client round trip. Two
  lands on disjoint files both succeed, and the line ends with both changes.
- The loser's rejected candidates are drafts held only by a land-attempt
  lease. Each gets `expires_at` when written, so they are collected minutes
  later even if the server crashes (§8.6). The CAS transaction clears
  `expires_at` and sets `pins` to 0 on the candidates that land.
- **Per-line combining (phase 4):** under heavy contention, each line gets a
  single in-process sequencer per server, which batches the queued lands. It
  rebases each onto the previous one and does one CAS for the batch, moving the
  line through the chain. That turns N racing retries into N rebases plus one
  CAS. Correctness still rests only on the CAS, so the combiner is purely an
  optimization and can be switched off.

### 8.2 What the client does

`graft land` makes one call and acts on the result:

| Result | Exit | Client action |
| --- | --- | --- |
| `Landed` | 0 | Moves the workspace base to the landed id and clears the stack. If it was rebased, applies `Diff(old_base, landed)` within the slice (others' changes in the slice), with no re-clone. Prints the landed id |
| `Conflict` | 3 | Leaves the working copy and the draft untouched. With `--sync`, or when the user runs `graft sync`, it rebases to the tip and writes conflict markers for text paths (`.graft-conflict` sidecars for binaries), then exits 3. The agent edits, then runs `graft commit && graft land` again |
| `Contended` | 4 | Retryable. The CLI retries with backoff for up to `--wait` (default 60 s), then exits 4 |
| `Rejected` | 5 | A policy said no (for example a missing check attestation). The detail names the policy |
| `PermissionDenied` / `Unauthenticated` | 6 | Not retryable |
| `Unavailable` / transport error | 7 | Retryable. Land is idempotent, so a blind retry is safe |

The working copy is **never stranded**. Whatever happened, `graft status`
shows the base, head, and state, and `graft sync` always moves forward. That
is the failure the OpenScion trial hit: a lost CAS left the client on a stale
base with no way to move.

### 8.3 Land policies (per line)

**Phase 1 default ([D22](decisions.md#d22-phase-1-is-a-git-alternative)):
`direct` on every line, `main` included.** A land is rebase, line merge and
CAS, and no build, test or check runs in the land path. `attested` and
`queued` are phase 2. Their code exists, but it is optional and switched off,
and the gate on `main` that [D6](decisions.md#d6-landing-gate-on-main)
chose (`attested`, with post-land CI and auto-revert) is the phase 2 design.
graft supports all of these:

| Policy | Meaning |
| --- | --- |
| `direct` | Scion's model, and the phase 1 default (D22). Land at once. GitHub CI runs on the batch PR (D21). In phase 2, post-land checks and an auto-revert mark each land on a batch integration line good or bad (§8.7, D25) |
| `attested` | The stack's head (pre-rebase) must carry passing attestations for the named checks (`ci/build`, `ci/test`), posted by CI through `Attest { changeset, check, status, url }`. Rebase-only changes onto a newer tip do not need re-attesting. The line-merge result can be required to re-attest (`attest_merged = true`) |
| `queued` | The merge-queue equivalent. The candidate is tested after rebase and before the CAS, by a CI hook, in batches. This is the slowest option, and it exists for lines that need it (release lines) |

Path rules can add requirements (`require_human_review` on `deploy/**`).
Policies are server config per line in phase 2. A versioned in-repo policy
file, landed under admin scope, can come later.

### 8.4 Sync

`Sync(workspace, onto = tip)` rebases the stack onto the new tip and moves the
workspace base and head by CAS. It returns the paths the client must update,
or the conflicts. It is the same rebase code as land, without the ref move.
A draft whose changes are all on the new tip is dropped from the stack. After
a land whose ack was lost, a sync therefore empties the stack instead of
re-landing it; the land's record still answers a retry of the old head with
the first `Landed` (§7.9).

### 8.5 Stacks

`commit` adds a draft on top of the head. `commit --amend` replaces the head
(the old head is superseded). `stack squash|reorder|split|drop` produce new
drafts on the server and move the head by CAS. `land` lands the whole stack
by default, each draft as its own landed changeset in order. `--squash` lands
it as one.

### 8.6 Orphans and collection

An *orphan* is any non-landed changeset or object that nothing holds. graft
never lets them build up silently (the trial found 246 orphans that nothing
reported).

**Changeset holders.** Every draft has exactly one holder: a workspace, or a
land-attempt lease. A changeset that is abandoned or superseded has none and
waits out its retention window.

**Expiry.** A record gets `expires_at` when it stops being a draft. Drafts
held by a workspace and landed changesets have none. A draft held by a
land-attempt lease is the exception: it gets `expires_at` when it is
written, because no other step would ever end its lease.

| What | `expires_at` |
| --- | --- |
| land-attempt candidates | set when written: the end of the land-attempt lease (10 minutes). The land transaction clears it for the candidates that land |
| drafts in a closed workspace, or one whose lease lapsed | the close time, or `lease_until` for a lapse, plus the draft retention window (7 days by default) |
| superseded drafts | the supersede time plus the same window |
| uploads never claimed by a changeset | 1 hour after upload (on the upload row) |

A candidate whose server crashed before the outcome is never written again.
Its `expires_at` from creation still passes, so it is collected like any
loser. The lease is longer than any land deadline, so a live land never
races it. The land transaction also preconditions that the candidate is not
collected, so a stalled server fails its CAS (`Contended`) instead of landing
a collected record. The G2.9 dry run never treats a `land:` holder as gone;
the sweep reads `expires_at` and does not have that gap.

Adopting an abandoned changeset (`SetHead`) makes it a draft again and
clears `expires_at`. Workspace leases are short (default 2 hours, renewed by
the client's heartbeat or by Vogt while the session lives,
[D10](decisions.md#d10-agent-token-issuance)). A crashed session's work is
still there for the full retention window. A retention change applies to
records that end after it.

**Protection records.**

Collection must never delete an object that a live changeset can reach. It
must also never walk a snapshot or the history to find out. graft keeps two
kinds of *protection record*, both written in the same transaction as the
changeset:

- **Object rows** `holders(object, holder)`. A changeset holds a row for
  every object on the paths it changes: each blob it puts, and each tree on
  the path from the root to each change in its own snapshot (the trees it
  derives, including the root). An upload lease holds a row for each object
  it uploaded until it expires. Rows are a set, indexed both by object and by
  holder.
- **Parent pins** `pins` on the changeset record: on a non-landed record,
  the number of non-collected changesets that name it as a parent. A
  changeset with `pins > 0` is never collected. Only non-landed parents are
  pinned. A landed changeset is never collected, so a pin on it would only
  add a hot row at the line tip. When a pinned record lands, the land
  transaction sets its `pins` to 0, and collection takes no pin off a parent
  that has landed. It decides by the parent's state inside its own
  transaction. So `pins` is 0 on every landed record. A landed changeset's
  parents are all landed (§8.1 lands a whole chain whose bottom's first
  parent is the tip), so every pin comes from a non-landed child.

**The invariant.** Every object in a non-collected changeset's snapshot has
a row whose holder is not collected. The argument is short. An object in
`C`'s snapshot is either on a path `C` changed, so `C` holds a row, or it is
in the snapshot of `C`'s first parent (changes are relative to
`parents[0]`). `C` pins its parents, so they are not collected while `C` is
not, and the same holds for them. The chain ends at a landed changeset,
whose rows are never removed, or at a root changeset, whose changes cover its
whole snapshot. So a live changeset protects everything it references,
although it writes rows only for its own changes.

**Write paths that add protection:**

| Path | Adds |
| --- | --- |
| `CreateChangeset`, and everything that writes through it (`Rebase`, `Sync`, land candidates, `Revert`, `commit --amend`, stack operations, import) | rows for its changed paths; one pin on each non-landed parent |
| `PutObjects` | an upload row with its expiry |
| a write that references or re-puts a doomed object | a claim row with its expiry, and clears the doomed mark, before its commit (revival) |
| the commit of a write that references a doomed object | clears the doomed mark (revival) |

**Paths that remove protection:** only the sweep, when it collects a record
(its rows, and one pin on each non-landed parent) or an expired upload or
claim (its row). A state change (draft to landed, superseded or abandoned)
touches no row. Landing zeroes the record's own `pins`, which then mean
nothing (above). A landed changeset's rows stay forever.

**Successors keep their objects.** When a superseded record `A` expires and
its successor `S = A.superseded_by` is not collected, every row of `A` for an
object that `S`'s snapshot still has at the same position moves to `S`. The
sweep finds them by reading `A`'s changed paths in both roots, O(changes ×
depth). A successor written by the current write path already holds these
objects itself, so this rule is a second line of defence against a missing
row. It is cheap, because it reads only `A`'s own paths. It replaces the
G2.9 dry run's rule that a candidate is protected while its successor chain
reaches a non-candidate. If `S` is already collected, nothing moves: a live
changeset never depends on a collected one.

**Sweep (incremental, never a full walk).**

The sweep runs per repository, in batches, rate-limited, under a per-repo
lease so that only one sweeper does the work. Each numbered step is one
metadata transaction with its journal entry.

0. **Lapse.** Workspaces whose lease ended are found by a range scan on
   `lease_until`. Each is closed: its drafts become abandoned with
   `expires_at = lease_until + retention`, and `WorkspaceClosed` is
   journalled. The drafts are found from the workspace's head, by walking
   first parents while the parent is a draft held by that workspace, so the
   cost is the stack's length. The workspace CAS guards against a concurrent
   renewal. Land-attempt drafts need no lapse step: their `expires_at` was
   set when they were written.
1. **Collect.** Records with `expires_at ≤ now` and `pins = 0` are found by a
   range scan on an index of exactly those records. For each record `R`:
   1. read its content (parents and changes) and its rows by the holder
      index;
   2. move rows to the successor, as above;
   3. in one transaction, with the precondition that `R`'s record is
      unchanged and `pins = 0`: set `R` collected and write its tombstone,
      delete its remaining rows, take one pin off each non-landed parent, mark
      `doomed(since = now)` every object that now has no row, and journal
      `Collected`.

   A parent that reaches `pins = 0` and is already expired enters the scan
   range, and the same pass collects it. A long stack unwinds from the top.
2. **Uploads and claims.** Expired upload and claim rows are found by a
   range scan on `expires` and removed, with the same doom rule.
3. **Delete.** Doomed objects older than the grace period (longer than the
   longest request deadline, by default 15 minutes) are found by a range scan
   on `since`. For each: one transaction marks it `deleting`, with the
   precondition that it is still doomed and has no row; the frame is deleted
   from the object store; a second transaction removes the doomed row.
4. **Revival.** A put of a frame that already exists does not touch the
   marks, so revival cannot wait for the commit alone. A tree derived and
   put before the commit can be doomed long before the write starts, and be
   deleted between the put and the commit. Revival has two parts.
   1. **Claim, before the commit.** A write reads the doomed and `deleting`
      marks of every object it references or puts: the blobs it names by id
      and the trees it derives. This read is O(change). If one is doomed, a
      transaction clears the mark and adds a *claim row*
      `claim:<expires>:<request_id>` that expires after the request
      deadline. Its precondition is that the object is still doomed and not
      `deleting`. If one is `deleting`, `CreateChangeset` returns
      `MissingObjects` for a blob named by id. For a tree it derives, it
      waits for the delete to finish and puts the frame again. `PutObjects`
      returns `Unavailable { retry_after }` until the delete is done, and
      then stores the frame again. Only after the claim does the write check
      that every frame it references exists.
   2. **Commit.** The commit transaction clears the doomed mark of any object
      it references, for objects doomed after the claim read.

   This holds because an object is deleted only after it has been doomed for
   the grace period, which is longer than any request deadline. An object
   doomed when the write read it has a claim row, so step 3's precondition
   fails. An object doomed after that read cannot reach its delete before the
   write commits, and the commit clears the mark. An object deleted before
   the read has no frame, and the existence check catches it. So nothing that
   is held is ever deleted, and nothing acknowledged points at a deleted
   frame. A claim row left by a write that never commits expires, and step 2
   dooms the object again.

**Cost.** Collecting a record costs O(changes × depth) tree reads (the
successor check), one row write per row it held, and one pin write per
parent. Each object costs O(1) per doom, revival or delete. Every scan is an
index range over what is collectable now. Writes gain one pin per non-landed
parent, and one mark read per object they reference or put. They write a
claim only for an object that is doomed. Nothing reads the
journal, walks ancestry, or opens a snapshot, so the cost follows the change
being collected, not the size of the history.

**Crash and concurrency safety.**

- **A child racing its parent's collection.** `CreateChangeset` preconditions
  each parent's record (not collected, unchanged) and adds the pin in the
  same transaction. Collection preconditions `pins = 0` and an unchanged
  record. One of them commits. A losing create retries and returns
  `Collected`. A losing collection skips the record.
- **Adoption racing collection.** `SetHead` changes the record (draft,
  `expires_at` cleared), so the collection's precondition fails.
- **Revival racing a delete.** The `deleting` mark is set in a transaction
  that checks the object is doomed and has no row. A claim clears the doomed
  mark and adds a row in a transaction that checks the object is not
  `deleting`. One of them commits first. If the claim wins, the delete skips
  the object. If the delete wins, the write waits for it and puts the frame
  again, or returns `MissingObjects`. A tree put before the claim read cannot
  slip through: the claim reads the marks after the put, and the frame check
  comes after the claim.
- **Crash mid-sweep.** Every step is one transaction, so a crash leaves a
  consistent state. A `deleting` row found at start-up is finished: the
  frame delete is idempotent, then the row is removed. Objects are durable
  before metadata references them (§6.6), so a crash before a write's commit
  leaves only unheld frames, which are uploads or never referenced.
- **Two sweepers.** The per-repo lease keeps it to one. If the lease is lost,
  the preconditions still make a second sweeper safe, only wasteful.

**Migration of existing repositories.**

Repositories written before protection records have object rows (G2.1 writes
rows for put blobs and derived trees, which are exactly the changed paths)
but no pins and no `expires_at`. The schema migration, at start-up and
before the server serves:

1. adds `pins` (default 0) and `expires_at` to records; the expiry index
   (`expires_at` where `pins = 0`); the holder index on rows; the `deleting`
   mark on doomed rows; and the `expires` index on uploads;
2. reads only non-landed records, through the state index, never landed
   history. For each draft, abandoned or superseded record it adds one pin to
   each non-landed parent. Landed records keep `pins = 0`. For each abandoned
   or superseded record it sets `expires_at` to the migration time plus the
   retention window, because the end time was never stored. For each draft
   held by a land-attempt lease it sets `expires_at` to the migration time
   plus that lease;
3. recomputes the objects on each non-landed record's changed paths and adds
   any missing row. This should add none, and the count is reported;
4. sets the repository's `gc_protection` marker in the last batch's
   transaction.

Its cost is O(non-landed records × changes × depth), once. The sweep refuses
to run (`Rejected { policy: "gc" }`) on a repository without the marker. The
dry run works either way.

**Failure modes.**

| Failure | Effect | Handling |
| --- | --- | --- |
| A write path misses a row or a pin (a bug) | A held object could be doomed and deleted | The grace period and revival catch a live reference. Deletion is off per repository until the operator turns it on. `graft admin gc --dry-run --verify` checks the plan against the G2.9 ancestry walk and refuses to sweep on a mismatch. The G3.2 property test checks the invariant over random interleavings |
| Pin leak (a count too high) | An expired record is never collected | `graft_orphans{state="pinned"}` shows it. `fsck` recomputes each non-landed record's pins from the non-landed records that name it as a parent, sets landed records to 0, and repairs any difference. It reads no landed history, because only non-landed records pin |
| A land server crashes after writing candidates | Its drafts have no live holder | They carry `expires_at` from creation and are collected like any loser |
| A write crashes after claiming | The claim row keeps the object | The claim expires after the request deadline, and step 2 dooms the object again |
| Successor already collected | No rows move | Safe: a live changeset never depends on a collected one |
| Server clock jumps forward | Records expire early | Bounded by the retention window. Only the server's clock is used, never a client's |
| A long stack expires at once | Many collections in one pass | The batch limit stops the pass. The next pass continues from the index |
| The sweep falls behind | Orphans grow | `graft_orphans{state}` and an alert on growth |
| A frame is already gone at delete | — | `delete` is idempotent |
| A late reader asks for a collected changeset | — | The tombstone answers `Collected`, not `NotFound` |

`graft admin gc --dry-run` and the `graft_orphans{state}` gauges report what
is held, lapsed and collectable at any time. The dry run reads the
protection records once a repository has them, and keeps the ancestry walk
as its test oracle and `--verify` mode. GC is journalled.

### 8.7 Post-land checks on the batch line

**Phase 2.** This section is the phase-2 design. It starts only with the
graft-primary authority switch; until then D22 stands and graft runs no
build, test, check or checker anywhere.

[D25](decisions.md#d25-post-land-checks-on-the-batch-line) designs phase 2's
post-land checks for batch integration lines (§11.4) only. The land path is
unchanged: rebase, line merge, CAS. Checks run after the ref moves, on the
landed snapshot, and mark each land good or bad before `pr-batch` will
include it. `direct` lines without `checks` behave as in §8.3.

**Configuration.** Per line in `server.toml`:

```toml
[[lines]]
name = "integration"
policy = "direct"
checks = "post-land"
required = ["ci/fmt", "ci/clippy", "ci/test"]
retries = 1          # a failed check is run once more before the land is bad
```

`required` names the checks GitHub requires on the target repository's
`main`, so a batch that is good in graft is the batch GitHub CI expects to
pass. The commands live with the checker.

**Flow.**

```mermaid
sequenceDiagram
  participant A as client (CLI, MCP, API)
  participant S as graft server
  participant C as post-land checker (graft-attester)
  participant M as graft-mirror pr-batch
  A->>S: Land (rebase, line merge, CAS)
  S-->>A: Landed (latency unchanged)
  S->>S: journal RefMoved; land_checks row = pending
  C->>S: Watch RefMoved on the line
  C->>S: Fetch landed snapshot (sparse, by id)
  C->>C: run required checks (sandboxed, timed out)
  C->>S: Attest {land, check, passed, url}
  S->>S: same txn: Attested entry + land_checks mark
  alt a required check failed (after retries)
    C->>S: Land Revert(bad land), metadata graft.reverts
  end
  M->>S: GetLandChecks(line, from batch start)
  M->>M: cut batch at newest land with an all-settled prefix
```

1. **Pending.** A land on a checked line gets `pending` in a `land_checks`
   index (`repo, line, pos, changeset, state`) in the land's own
   transaction (rule 3). No journal scan finds it later.
2. **Checked.** The checker watches `RefMoved` on its line, fetches the
   landed snapshot by id, and runs each required check in a cleared
   environment with a timeout, as the trial attester does for drafts. When
   it is behind, it checks the newest unchecked land first (skip ahead, as
   a merge queue batches): if that passes, every land up to it is good.
3. **Marked.** `Attest` on a landed changeset of a checked line is
   accepted from an attest-only token for that changeset. It writes the
   `Attested` journal entry and updates `land_checks` in one transaction:
   `good` when every required check has passed, `bad` when one failed after
   `retries`, `pending` otherwise. A check that errors (timeout, checkout
   failure) is retried and then counts as failed, with its log.
4. **Bisected.** When a skipped-ahead check fails, the checker checks the
   lands between the last good one and the failure, halving each time, so
   the first bad land is found in O(log n) runs. Lands before it are good.
5. **Reverted.** The checker lands `Revert` of the first bad land on the
   line, with metadata `graft.reverts = <land>` and `graft.revert-check =
   <check url>`, through the ordinary land path. The revert is a land like
   any other and is checked too. The journal carries `LandReverted { land,
   revert, check }`, so the author's session (from the land's metadata)
   can be told. Lands after the bad one stay; their next check runs on top
   of the revert. A revert that conflicts is not forced: the line is marked
   `blocked` and the operator decides (`graft line unblock`).
6. **Batched.** `pr-batch` reads `land_checks` from the batch start by
   position (paged, O(lands in the batch)). It cuts the batch at the
   newest land such that every land up to it is `good`, or `bad` with a
   good revert after it inside the cut. Pending lands wait for the next
   batch; a run with nothing settled waits. A bad land never reaches a
   batch PR without its revert.

**Reuse of the phase-2 code.**

| Piece | From | Change |
| --- | --- | --- |
| Attest-only token, CI OIDC exchange | G3.8 / G3.8b (WI-2211, WI-2265, WI-2271) | Mint for a landed changeset of a checked line as well as a draft |
| `Attest` RPC and `Attested` journal entry | G3.1 | Accept landed changesets on checked lines; write `land_checks` in the same transaction |
| Checker | the trial attester (WI-2276, WI-2292) | A `post-land` mode: watch `RefMoved` instead of drafts, skip ahead, bisect, revert |
| Journal read robustness | WI-2293 | Fixed first: one failed `GetWorkspace` must not stall the read |
| GitHub Actions path | `ci/github/graft-pre-land.yml` | Optional: export a land as `graft/land-<id>` and attest through the OIDC exchange. The self-hosted checker is the default (no branch per land on GitHub) |
| Auto-revert bot (plan phase 4 table) | not started | Built here, inside the checker, for checked lines only |

The `attested` and `queued` policies stay off.

**Bounds.** The checker's queue is bounded and the journal is its backlog.
A check's log is capped. `GetLandChecks` is paginated. A flaky check
retried and passing is counted in `graft_post_land_flakes_total{check}`.

### 8.8 Pre-land check in `graft land`

[D27](decisions.md#d27-pre-land-check-in-graft-land): before it sends
`Land`, `graft land` runs two read-only checks by default, on every line:

1. **Conflict preview**, the same read as `graft status --upstream`
   (WI-2309): the files the stack and the line tip both changed since the
   stack's base. For each such file the client asks the server for a
   dry-run line merge (`Rebase { dry_run: true }`) under the land's
   `--merge` mode. A file that merges cleanly is reported and does not
   stop the land. A file that does not (a **real conflict**) stops it:
   `graft land` exits 3 with the paths, as a conflicted land does, and
   sends nothing. `--sync` then writes the conflicts into the files as it
   does today.
2. **Draft overlap**, the same read as `graft status --drafts` (D24): the
   other live workspaces with open drafts on the stack's paths. It is
   reported (workspace, owner, session, work item, age) and never stops
   the land. Without the `repo:<r>:drafts` scope it is skipped with a one-
   line note.

`--no-check` skips both. JSON output carries both results under
`preflight`, so any client (the CLI, MCP, an API caller) reads the same
shape. The MCP `land` tool runs the same checks.

- **Dry run writes nothing.** A plain `Rebase` creates the rebased
  changesets and pins their parents (§8.6), so a preview on every land would
  leave orphans behind. `Rebase { dry_run: true }` runs the same line merge
  and returns only the per-path result (merged, conflicted, with conflict
  paths); it writes no changeset, object, pin or journal entry, and needs
  only `read`.
- **Advisory, not a gate.** The server's land stays the authority: the tip
  can move between the check and the CAS, and a conflict then comes back
  from `Land` as before. No server state is added and no land policy
  changes.
- **Bounded.** Both reads are paginated and capped (the `--upstream` and
  `--drafts` limits); a capped read says so and does not stop the land on
  what it did not see.
- **Cost.** One upstream diff page, one dry-run rebase over the both-changed
  files, one overlap page: O(the change), never the history.

### 8.9 Merges between lines

([D29](decisions.md#d29-merge-points-between-lines)) A merge brings one
line's work into another, such as git flow's release → main or a hotfix back
into develop. History stays first-parent: a merge lands as an ordinary
one-parent changeset on the target line. What makes it a merge is a **merge
point**, recorded beside the line index, which `MergeBase` reads.

**Storage.** `line_merges(repo, dst, dst_pos, src, src_changeset, src_pos)`
(SQLite schema version 7; version 6 is the draft path index), one row per
merge landing, keyed by `(repo, dst, dst_pos)` with an index on `(repo, dst, src, dst_pos)`. The
latest row for a `(src, dst)` pair is that pair's merge point. Rows are
append-only; a line position is never re-landed, so a row is never wrong. The
changeset also carries `graft.merged-from = <src>@<src_changeset>` in its
metadata, for readers and export, but nothing on the hot path reads it.

**Containment.** A line `L` *contains* a point `(M, p)` when one of these
holds, with at most `H` hops (default 8) and `R` row reads (default 64) per
query:

1. `M = L` and `p <= tip(L)`.
2. `L` has a fork row `(M', q)` (D23) and `M'` contains `(M, p)` at position
   `q`.
3. `L` has a merge row from `M'` at `src_pos = s`, and `M'` contains `(M, p)`
   at position `s`.
4. `(M, p)` is itself a merge landing from `S` at `s`, and `L` contains both
   `(M, p - 1)` and `(S, s)`. Its content is exactly those two, so a line
   that holds both holds it.

Each rule reads only indexed rows "at or below a position", newest first.
The `MetaStore` reads are `line_fork` (a line's fork row, D23) and the
`line_merges` rows into a line at or below a position.

**`MergeBase`.** For two changesets on lines, `MergeBase` first tries the
merge points: walking `dst`'s merge rows newest first (rule 3 and 4 limited to
the budget), it returns `dst`'s changeset at the highest position that `src`
contains. If none is found inside the budget, it falls back to the existing
ancestry walk (§7.2), which is what it does today. The answer says which it
used (`via: merge_point | fork | ancestry`), so a capped search is visible.

**`LineMerge`.** `LineMerge { src, dst, src_at?, merge: line, deadline,
message? }`:

```text
check scopes: read on src, land:<dst>, write on every changed path
s  ← src_at or ResolveRef(src); require s on src's line index (pos sp)
loop until deadline or attempt budget (as Land, §8.1):
    tip  ← ResolveRef(dst)
    base ← MergeBase(s, tip)                       # merge point first
    if base == s or s contained by dst: return UpToDate
    cand ← three-way tree merge of (base → s) onto tip, line merge per D16
        on Conflict: journal LandAttempt(conflict); return Conflict{paths, base, tip, s}
    cand: parents [tip], metadata graft.merged-from, graft.merged-paths
    txn: CAS(dst, expected = tip, new = cand) + line index rows + record
         + line_merges row (dst, pos(cand), src, s, sp)
         + journal RefMoved + LineMerged { src, dst, src_changeset, src_pos, dst_pos }
    on RefConflict: back off and loop
return Contended{ dst, tip }
```

- Only `dst` moves, by CAS; `src` is only read (§4.6). The merge row and its
  journal entry are in the same transaction as the ref move (§4.8), so a
  merge point can never exist without its landing, or the reverse.
- If `src` moves during the merge, the row records the `s` that was merged,
  which is exactly what the target contains.
- Idempotent on `(s, dst, tip)`: a retry after a lost reply returns the same
  `Landed` (§7.9).

**Conflicts.** A `Conflict` carries `base`, `tip` and `s` (ids, no
contents), so the client can write conflict markers in one fetch. The client
resolves in a workspace on `dst` and lands with
`Land { ..., merged_from: { line: src, changeset: s } }`. The land checks that
`s` is on `src`'s line index and writes the same `line_merges` row and
`LineMerged` entry in its CAS transaction. A clean merge is 1 round trip; a
conflicted one is 3 (merge, fetch, land).

**Surfaces.**

- CLI: `graft line merge <src> [--into <dst>] [--at <changeset>] [-m MSG]`.
  `--into` defaults to the working copy's line. Exits as `land` (§8.2):
  0 landed or up to date, 3 conflict (with `--sync`, markers are written),
  4 contended. `graft land --merged-from <src>@<id>` lands a resolution.
- MCP: `graft_merge { src, into?, at?, message? }`, same result shape.
- `graft log` shows `merged <src>@<id>` on a merge landing;
  `graft line merges <dst> [--from <src>]` lists a line's merge points,
  paginated.

**Blame and export.** Blame follows first parents, so lines brought in by a
merge are attributed to the merge landing, as with a squash; the
`graft.merged-from` metadata lets a reader follow them to the source line.
Export (§11.2) writes a one-parent git commit with a
`Graft-Merged-From: <src>@<changeset>` trailer. No git merge commits are made.

**Bounds and cost.** A merge costs one bounded `MergeBase` (at most `H` hops
and `R` indexed reads), one tree merge over the paths that differ from
`base` (id-pruned, O(the change)), and one CAS. Nothing scans the journal,
the history, or a line's changesets. `line merges` is paginated (limit 100,
max 1,000).

## 9. Client and working copy

### 9.1 Layout

```text
<wc>/.graft/
  config        server endpoint, repo, workspace name
  state         base id, head id, line, last synced LSN (atomic rename on update)
  sparse        path prefixes, one per line (multiple prefixes; root files optional)
  index         path → (kind, blob id, size, mtime_ns, ctime_ns, ino, mode)
  cache/        local object frames (only when no node daemon is reachable)
```

### 9.2 Status without a staging area

`status` compares each file in the slice against the index entry by
(size, mtime_ns, ctime_ns, ino, mode). It re-hashes only on a mismatch, or
when the entry is *racily clean* (its mtime is not strictly older than the
time the index was written). A filesystem watcher (inotify) is an optional
accelerator in a later phase. It is never required for correctness. Files
outside the slice are never read and never changed, and a commit lists only
the paths that changed.

### 9.3 Commands (phase targets)

| Phase | Commands |
| --- | --- |
| 1 | `clone [--path P]… [--at REF\|ID]`, `status`, `diff`, `log`, `show`, `blame`, `cat`, `ls`, `sparse`, `api`, `mcp` (read tools), `version` |
| 2 | `commit [--amend] [-m]`, `workspace`, `stack list`, `land`, `sync` (the GitHub PR bridge mode was removed under D21/D22) |
| 3 | `land` (native), `revert`, `stack squash\|reorder\|split\|drop`, `lsp`, `attest` (for CI) |

Every command has `--json`, with a stable schema per command, and no
interactive prompts unless it runs on a TTY. Exit codes are as in §8.2.

## 10. MCP and LSP

### 10.1 MCP

Most of our clients are agents, so MCP is a first-class surface and not an
afterthought. `graft-mcp` implements an MCP server over the API traits. It
runs in two places:

- **`graft mcp` (stdio)**, inside a working copy: it knows the workspace, so
  it offers working-copy tools as well as repository tools.
- **The server's `/mcp` endpoint** (streamable HTTP, bearer token): for agents
  with no checkout at all, such as review bots, Vogt oversight, and bisect.

| Tool | Read-only | Maps to |
| --- | --- | --- |
| `graft_log` | yes | `Log` (paths, range, limit) |
| `graft_show` | yes | `GetChangesets` plus `Diff` patch |
| `graft_diff` | yes | `Diff` (with patch, capped) |
| `graft_blame` | yes | `Blame` (line range) |
| `graft_read` / `graft_ls` | yes | `ReadFile` / `ListTree` (paged) |
| `graft_status` | yes | working-copy status (stdio only) |
| `graft_commit` | no | `CreateChangeset` with inline content, or from the working copy |
| `graft_land` / `graft_sync` | no | `Land` / `Sync` |
| `graft_revert` | no | `Revert` |
| `graft_workspace` | no | open, renew, or close |

Every tool result is bounded and paginated, which keeps agent token use down.
Read tools carry `readOnlyHint`. Repositories and changesets are also exposed
as MCP resources (`graft://<repo>/<changeset>/<path>`).

### 10.2 LSP

`graft lsp` is a stdio LSP server that an editor (or an agent harness that
speaks LSP) runs *next to* its language server. It gives source-control
intelligence on the files being edited:

| LSP feature | graft behaviour |
| --- | --- |
| `textDocument/hover` | blame for the line: changeset, author (and whether human or agent), message, Vogt work item |
| `textDocument/codeLens` | "changed N times, last by X" per file or item; "base is K changesets behind main" |
| `publishDiagnostics` | conflict regions after a failed sync; edits outside the sparse slice; files changed upstream since the base |
| `workspace/executeCommand` | `graft.commit`, `graft.land`, `graft.sync`, `graft.diffBase`, `graft.blameFile` |
| virtual documents | `graft:` URIs open any file at any changeset without a checkout (custom `graft/readFile` request) |
| later: `workspace/symbol`, `textDocument/definition` beyond the slice | backed by a server-side symbol index (derived kind `symbols`, tree-sitter tags), so a sparse checkout can still navigate the whole repository |

The LSP server is built on the same `graft-client` working copy and API
traits as the CLI. It adds no state of its own.

## 11. Git interop

Git lives only in `graft-git` (a library) and `graft-mirror` (a service). The
core model has no git fields, and the server does not know git exists. Mirror
state, including the graft id ↔ git sha map and the raw headers of imported
commits, lives in the mirror's own SQLite database. That database is backed up
like the server's and can be rebuilt from GitHub.

### 11.1 Import

`graft-mirror import <git-repo> --repo <name>`:

1. Read the git repository with `gix`. The transport is `git fetch` into a
   local bare mirror, because it is battle-tested for GitHub auth and is
   limited to the mirror's host.
2. Walk every commit reachable from the configured refs (`main`, release
   branches, tags) in topological order, oldest first. Multiple roots and
   merges are supported, and side-branch commits reached through merges are
   imported as changesets too, so the DAG is complete.
3. For each commit, diff its tree against its first parent's tree. Only
   changed subtrees are walked. Upload the new blobs, then
   `ImportChangesets` in batches (admin scope, which allows roots and
   preserves parents and `authored_at` from the author date).
4. Map modes: `100644 → file`, `100755 → exec`, `120000 → symlink`,
   `160000 → submodule`, `040000 → tree`. Non-canonical legacy modes (`100664`)
   map to `file`, and the raw git tree is recorded in the mirror DB, so export
   stays exact.
5. Record `(changeset, git sha, raw commit headers: author, committer, dates,
   encoding, gpgsig, extra headers)` in the mirror DB.
   Every imported changeset also carries the metadata key
   `mirror.git-commit = <40-hex sha>` ([D19](decisions.md#d19-import-identity)).
   Git commits that differ only in fields graft does not model, such as the
   committer line of a rebased copy, therefore stay distinct changesets, and the
   mapping is always 1:1. The key is graft metadata set only by `graft-git`, and
   export never writes it into the git commit.
6. Set the line refs and import tags as `tag` refs.

**The fidelity invariant:** for every imported commit `c`,
`export(import(c)).sha == c.sha`. The import verifies this per commit, as it
goes, and refuses to finish if any commit fails, unless `--allow-lossy` names
the exceptions. Paths that are not UTF-8 or break the portability rules stop
the import with a list of offenders ([D11](decisions.md#d11-path-rules-on-import)).

### 11.2 Export

`export(changeset)` builds a git commit deterministically:

- **git blob sha:** `sha1("blob <len>\0" ‖ bytes)`, cached in the mirror DB.
- **git tree sha:** computed by path copying from the parent's git tree. Only
  trees on the changed paths are recomputed, which is O(change), using the same
  algorithm as the root-tree derivation, over a git-shaped encoder.
- **commit:** for imported changesets, the recorded raw headers, which
  reproduce the exact sha. For native changesets: `author` from the changeset;
  `committer = graft <graft@…>` with the line position's landed time (stored
  in the land record, so it is deterministic after the fact); the message plus
  trailers `Graft-Changeset: <id>` and, when present, `Graft-Session: <ses_…>`
  and `Vogt-Work-Item: WI-…`. The parents are the git shas of the parents.

`graft-mirror export --to <bare repo>` writes a complete git repository
(every line, tag and landed changeset). This is the **exit path**. It is run as
a weekly drill and compared against GitHub.

### 11.3 The two-way mirror during migration

Each line has exactly one **authority** at a time, recorded in the mirror DB
and in graft's line policy. Two systems never accept writes for the same line
at once. That is how the mirror avoids split-brain.

**Mode A: `git-primary` (phases 1 and 2).** GitHub `main` is authoritative.

- **Follow:** the mirror learns of new commits by GitHub webhook (`push`)
  where ingress exists, and by `git ls-remote` polling every 15 s otherwise
  ([D9](decisions.md#d9-github-change-notification)). It imports the new
  commits and moves graft `main` to match by admin CAS. Lag is a metric with
  an alert.
- **Write path (phase 2), the PR bridge (removed under D21/D22; GitHub
  delivery is the batch PR, D21. Kept here as history):** `graft land` on a
  git-primary line does not move graft `main`. The server hands the stack to
  the mirror, which exports it as commits on the current GitHub tip and pushes
  branch `graft/<workspace>/<short-id>`. It opens a PR with the work item and
  session in the body and enqueues it in the GitHub merge queue. The CLI
  returns `Queued { pr_url }` (exit 0 with `--no-wait`), or waits on the
  journal for the PR's outcome. When the merge queue lands it, the commit
  comes back through follow. The trailer `Graft-Changeset` links it to the
  draft, which becomes `landed` (mapped to the squashed or rebased commit
  GitHub made). A PR that fails CI or is closed comes back as `Rejected`, with
  the check URL.

**Mode B: `graft-primary` (phase 3 onward).** graft `main` is authoritative.

- Land is native (§8). The mirror tails the journal and, for each `RefMoved`
  on the line, exports the new changesets and pushes to GitHub with a
  **lease**: `git push --force-with-lease=main:<last exported sha>`. That is a
  CAS on GitHub's side, and it is never a blind force.
- A GitHub ruleset makes `main` writable only by the mirror's GitHub App
  ([D8](decisions.md#d8-github-mirror-identity)). A rejected lease means
  someone wrote to GitHub out of band. The mirror stops exporting that line,
  raises a `mirror_divergence` alert, and does nothing destructive. An
  operator resolves it with `graft-mirror reconcile`, which imports the stray
  commits as a draft and lands them, or discards them.
- GitHub PRs opened by people or external bots (Dependabot) are imported as
  drafts in a `github-pr/<n>` workspace by the mirror. They land through graft
  `Land` when approved, so the PR closes as merged by the export.
- GitHub Actions keeps running on pushes to the GitHub mirror (post-land CI),
  and on `graft/*` branches when a line's policy is `attested` (pre-land
  checks, reported back through `Attest`).

**Authority switch (A → B, and B → A for rollback):**

1. Freeze: set GitHub `main` read-only (ruleset) and drain the merge queue.
2. Catch up: follow imports every commit, and graft `main` equals GitHub `main`
   by sha mapping.
3. Verify: `graft-mirror verify` re-exports the last N changesets and compares
   shas, and compares trees at the tip.
4. Flip: set the line's authority in graft and the mirror, switch land to
   native, and point the ruleset bypass at the mirror's App only.
5. Unfreeze. The rollback (B → A) is the same steps in reverse. Graft's
   history is already on GitHub, so nothing is lost.

### 11.4 Batch PR export

Under [D21](decisions.md#d21-batch-pr-export) clients land on a graft
integration line forked from the followed `main`, and `graft-mirror pr-batch
<line> --base main` delivers it to GitHub as one PR at a time, which the
operator merges. [D26](decisions.md#d26-one-commit-per-land-in-a-batch-pr)
changes the PR's commits from one squash to one commit per land, and
[D25](decisions.md#d25-post-land-checks-on-the-batch-line) (§8.7, phase 2) will
decide where a batch is cut; in phase 1 a batch is cut at the tip.

**Per-land commit shape.** `graft/batch-<n>` is a chain on GitHub's current
`main` sha `B`:

```
B ── c1 ── c2 ── … ── ck        (k ≤ 500; sync changesets skipped)

ck:  tree      = export(replayed snapshot k); at k = last, = export(integration tip)
     parent    = c(k-1), or B
     author    = the land's author (name, email and kind, as the client recorded them), land time
     committer = the mirror identity, land time
     message   = the land's message, then:
       Graft-Changeset: <land id>
       Graft-Session: <Vogt session or ->
       Graft-Work-Item: <work item or ->
       Graft-Checks: ci/fmt=passed ci/clippy=passed ci/test=passed
       Graft-Batch: <n>
```

- **Replay.** The lands' trees on the integration line sit on whichever
  base the line had absorbed then. pr-batch replays each land's own diff
  (land against its first parent), in land order, onto the base tip with
  graft `Rebase` (line merge, D16). The replayed changesets are held by a
  mirror workspace `batch-<n>` that is closed when the batch settles, so GC
  collects them (§8.6). Cost: one diff and one rebase per land, never a
  snapshot walk.
- **Check.** The last replayed snapshot must have the integration tip's
  root id. If it does not, or a replay conflicts, the batch is exported
  squashed as before (one commit, the D21 message), and the PR body says
  why.
- **Reverts (phase 2).** A bad land and its revert (§8.7) are both commits.
- **Determinism.** Every field comes from recorded data and land times, so
  the chain's shas are a function of the batch. The chain is written to the
  local mirror and recorded in the batch record (`commits`, the list of
  `(land, sha)`) before the push; a retry pushes the same shas.
- **PR body.** As D21: the land table (now with each land's commit sha),
  the caps (60,000 characters, at most 8 checks per land) and the
  `Graft-Batch` trailers.
- **Review.** graft keeps no review state: no review verb, approval or
  review metadata. Review happens on the GitHub batch PR, per commit, and
  the operator's merge is the approval.
- **Cut.** A batch ends at the tip in phase 1. In phase 2, on a checked
  line, it ends at the cut point of §8.7 step 6. A batch holds 500 lands
  at most; the rest wait for the next batch.
- **Settle.** Follow imports the PR's merge. A rebase-merge produces new
  shas for the same trees; follow maps each back by its `Graft-Changeset`
  trailer. A squash-merge maps the batch tip as D21 does. Rejected batches
  leave the line as it is.

**Implementation steps (WI-2349 follow-ups, for §8.7, §8.8 and this
section).** Each is one plan item on disjoint crates, after step 0. Steps 1–4
and 8b are **phase 2**: they wait for the graft-primary authority switch. Step
0's D25 types are phase 2 too; its `dry_run` part is phase 1.

0. Interface PR (`graft-proto`, types and signatures only): a `dry_run`
   flag on `Rebase` and its result shape, plus the D25 types
   (`GetLandChecks`, `LandReverted`, line `checks` config). Bodies stubbed;
   lands before steps 2 and 7.
1. *(Phase 2.)* WI-2293: the attester's journal read survives a failed
   `GetWorkspace` (`graft-attester`).
2. *(Phase 2.)* Checked lines: `checks`, `required`, `retries` in line config;
   the `land_checks` index and its schema migration; `Attest` and the token
   mint accept landed changesets on checked lines; `GetLandChecks` (paginated)
   and `LandReverted` (`graft-engine`, `graft-proto`, `graft-store-sqlite`,
   `graft-auth`). An interface PR with the proto types first.
3. *(Phase 2.)* The checker's `post-land` mode: watch `RefMoved`, skip
   ahead, bisect, retry, revert (`graft-attester`).
4. *(Phase 2.)* pr-batch cuts at the settled prefix on a checked line
   (`graft-mirror`).
5. Per-land commits: replay, root check, squash fallback, the batch record's
   commit list, follow's per-commit mapping (`graft-mirror`, `graft-git`
   for the commit builder). Golden test for the chain's shas.
6. CLI identity (fit assessment gap 5, its own item, `graft-cli`): needed
   for D26's authors, and lands before step 5 ships.
7. The pre-land check in `graft land` and the MCP `land` tool
   (`graft-cli`, `graft-mcp`; §8.8, D27), with the engine body of
   `Rebase { dry_run }` (`graft-engine`, from step 0).
8. graft-dev rollout, in two parts:
   - **8a (phase 1).** Roll out per-land commits (step 5), CLI identity
     (step 6) and the pre-land check (step 7) on graft-dev, and measure them
     on the bench harness: batch export time, the squash-fallback rate, and
     conflicts the pre-land check stops.
   - **8b (phase 2).** Switch the trial integration line to
     `checks = "post-land"`, run the attester profile, and measure on the
     bench harness: bad-land detection time, revert time, and batches cut
     per day.

### 11.5 Batch PR review and merge

[D28](decisions.md#d28-batch-prs-are-reviewed-and-landed-like-any-pr) amends
D21 and D26: a batch PR is reviewed and merged like any other PR, not held for
the operator. §11.4's "which the operator merges" and "the operator's merge is
the approval" read as this section.

- **Review.** When `pr-batch` opens a batch PR, oversight starts its standard
  Claude review (a read-only reviewer) on the PR as a whole. graft calls no
  LLM and stores no review data (D26).
- **Blockers name the land.** A finding names the land's commit (its
  `Graft-Changeset` trailer). It is fixed forward by a new land on the
  integration line, which joins the open batch as one more commit (§11.6,
  D30), or by dropping the land. graft does not rewrite an open batch.
- **Merge.** A clean review goes to oversight's `land-pr`, which merges once
  GitHub's required checks on the PR pass, with rebase-merge to keep the
  per-land commits (D26). graft still never merges or queues the PR: the
  merge is done outside graft.
- **Checks.** The required checks are GitHub's, run by the forge on the PR.
  graft runs none of them, so D22's "no build, test or CI in the land path"
  holds.
- **PR body.** It ends with a line saying how the PR is merged: reviewed by
  oversight, then merged by `land-pr` once the required checks pass.
- **Settle.** Unchanged: follow imports the merge and records the batch as
  merged (§11.4).

### 11.6 Appending to an open batch

[D30](decisions.md#d30-a-land-after-pr-batch-joins-the-open-batch-pr) lets a
land that arrives while the line's batch PR is open join that PR. §11.5's
"graft does not rewrite an open batch" still holds: an append adds commits
and never changes one.

```
open batch n:   B ── c1 ── … ── ck            cut at line position p
line:           … ── Lp ── L(p+1) ── … ── Lm  (lands only after p)
append:         B ── c1 ── … ── ck ── c(k+1) ── … ── c(k+m-p)
```

- **Allowed** when all hold: the batch is open and not sealed; every
  changeset after the cut is a land (a sync changeset means the base moved,
  so the lands wait for the next batch); the batch stays within `max_lands`
  and `MAX_COMMITS`. `max_age_ms` does not apply to an append.
- **Sealed** means the merge has started: auto-merge on, in a merge queue,
  or GitHub reports it merging. A sealed batch takes no appends, and new
  lands wait. If an append races a `land-pr` merge, the merge's
  `--match-head-commit` fails and oversight reviews the new head (D28).
- **Replay.** Each new land's own diff is rebased with line merge (D16) onto
  the replay of `ck`, held by the same mirror workspace `batch-<n>`. The
  last replay must have the new tip's root id. Cost: one diff and one
  rebase per new land, never the earlier ones again.
- **Record, then push.** The new `(land, sha)` pairs and the new cut are
  added to the batch record, then `graft/batch-<n>` is pushed as a
  fast-forward with a lease on `ck`. A lost lease (someone else pushed) stops
  the run with nothing changed in graft's record beyond the recorded chain,
  which the next run re-checks. Never a force push.
- **What the reviewer sees.** The same PR with more commits; the earlier
  shas are unchanged, so their review comments stay in place. The PR body's
  land table lists every commit, marks the appended ones, and is rewritten
  in place. Stale approvals are dropped by GitHub's push rule, and oversight
  re-reviews the head.
- **Drop.** `--exclude` lands a revert, which is appended. The open PR is no
  longer superseded for a drop (WI-2340's close and recut applies only when
  the batch is sealed or cannot be appended to).
- **Failure modes.** Replay conflict or root mismatch: nothing pushed,
  lands wait, the run reports the land. Lease lost: stop, report, retry on
  the next run. PR merged or closed between read and push: the push still
  only adds commits to a branch; follow settles the batch from what merged,
  and lands not in the merge go in the next batch. Crash after record, before
  push: the retry pushes the same shas (D26 determinism).
- **Agnostic.** The rule reads only graft's line, the batch record and the
  PR's state. It runs no build, test or check (D22) and knows nothing of the
  project's language or layout; the forge's own required checks run on the
  new head as on any push.

## 12. Auth

### 12.1 Principals and tokens

| Principal | How it gets a token |
| --- | --- |
| operator (human) | `graft-server admin token mint` on the server host, stored in Infisical |
| agent session | Vogt mints per session through `MintToken`, using Vogt's own service token, scoped to the session's repos and work, with a TTL ≤ 12 h. It is delivered to the session like other brokered credentials (see [D10](decisions.md#d10-agent-token-issuance)) |
| CI job | phase 2: a per-repo CI token in Infisical. Phase 3: an exchange of a GitHub Actions OIDC token, or a Woodpecker job token, for a short graft token |
| services (node, mirror) | long-lived service tokens with narrow scopes, rotated through Infisical |

Tokens are signed by the server (Ed25519, PASETO v4.public or an equivalent
compact format) and carry: `sub`, principal `kind`, `session` (Vogt `ses_…`),
`work_item`, `scopes`, `exp`, `jti`. The scopes are:

- `repo:<name>:read`
- `repo:<name>:write[:<path-prefix>]`
- `repo:<name>:land:<line>`
- `repo:<name>:attest` (post attestations only)
- `repo:<name>:drafts` (the draft-overlap query; granted only by `admin`, see
  [D24](decisions.md#d24-draft-overlap-visibility))
- `repo:<name>:admin`
- `objects:read`

Verification is local and needs no store lookup. Revocation is by short
expiry, plus a `jti` denylist in the MetaStore checked on writes.

### 12.2 Enforcement

- **Server:** every RPC checks its scope. Writes (`CreateChangeset`, `Rebase`,
  `Land`, `Revert`) check `write` against every changed path and fail with
  `PermissionDenied { path }`. Lines check `land:<line>`.
- **Node:** it verifies the caller's token locally (it has the server's public
  key) before serving cached objects for a repository. It forwards the token
  on repository RPCs. Unix-socket peer credentials (`SO_PEERCRED`) are logged
  as well, for audit.
- **Reads by path** (`ReadFile`, `ListTree`, `Diff`, `Log`, `Blame`) respect
  read path restrictions if a repository configures them. Content-addressed
  `GetObjects` cannot be restricted by path, so per-path read secrecy waits for
  a later design (capabilities issued by path-addressed reads). Until then a
  repository is the read boundary.
- **Transport:** the server binds to the tailnet address only. TLS (rustls) is
  supported and recommended once a non-tailnet client exists. Tokens never
  appear in logs, errors, or process arguments; the CLI reads them from a file
  or the environment.

## 13. Observability

- **Logs:** `tracing` with JSON output, shipped to the estate's Loki. Every
  span carries `repo`, `op`, `request_id`, `principal`, `session` (Vogt),
  `work_item`, and `tier`. File contents and tokens are never logged.
- **Metrics:** Prometheus at `/metrics` on the server, node, and mirror, scraped
  by the estate's Prometheus. The `graft-telemetry` registry names:

| Metric | Why |
| --- | --- |
| `graft_rpc_duration_seconds{op,tier,outcome}` | latency per operation per tier |
| `graft_land_attempts{line}` (histogram), `graft_land_outcome_total{line,outcome}` | contention and conflicts |
| `graft_land_to_visible_seconds` | land-to-visible latency, the headline number |
| `graft_rebase_merged_paths_total` | how often line-level merge saves a conflict |
| `graft_cache_hit_ratio{tier}`, `graft_singleflight_shared_total` | tier effectiveness |
| `graft_fetch_bytes_total{tier,codec}`, `graft_fetch_objects_total` | transfer and compression |
| `graft_store_op_seconds{port,op}`, `graft_sqlite_writer_queue_depth` | storage health |
| `graft_orphans{state}`, `graft_gc_reclaimed_bytes_total` | orphan build-up and reclamation |
| `graft_mirror_lag_seconds{line}`, `graft_mirror_lag_changesets{line}`, `graft_mirror_divergence{line}` | migration safety |
| `graft_journal_lsn{repo}`, `graft_watch_subscribers` | progress and fan-out |
| `graft_workspaces{state}`, `graft_lease_expired_total` | session hygiene |

- **Alerts** (Alertmanager to Telegram, as for other estate services):
  mirror divergence (page), mirror lag over 5 minutes, land error rate over 5%
  for 10 minutes, any `CorruptObject`, store write errors, free disk under 15%,
  SQLite writer queue saturated.
- **Traces:** OTLP export to Tempo, optional, with spans across
  client → node → server.
- **Audit:** the journal is the audit log for writes. `graft admin journal`
  and `Watch` expose it, and Vogt can link landed changesets to work items
  through `vogt.work_item` metadata.
- **Superfan sessions:** in Scion's spirit, a Vogt watcher session per
  component can read these logs and metrics and flag anomalies the static
  alerts miss. This is optional and comes after the pilot.

## 14. Failure modes

| Failure | Effect | Handling |
| --- | --- | --- |
| Server crash or `kill -9` mid-write | Partial objects; a transaction not committed | Objects are written before the commit point, and stray temp files are cleaned at boot. The SQLite WAL recovers committed transactions. Uncommitted work is not acknowledged, so the client retries, which is idempotent |
| Crash between the ref CAS and the journal | — | Impossible by construction: one transaction |
| Disk full | Writes fail | Admission control refuses writes with `Unavailable` below a free-space floor. Reads continue. Alert at 15% |
| Corrupt object on disk | A read fails its hash check | `CorruptObject`. The node discards the object and refetches. The server marks it and alerts; recover from backup or the GitHub mirror (export the commit and re-import the blob). `fsck` finds all of them |
| SQLite writer saturated | Write latency rises | The bounded queue gives `Unavailable{retry_after}` (backpressure), with a metric and an alert. Remedy: tune, or the Postgres port |
| Lost land race | — | The server retries (§8.1). `Contended` only after the budget is spent |
| Land conflict | — | `Conflict` with paths. The client never strands (§8.2) |
| Client or session dies mid-work | The draft is left with a lapsing lease | Retained for 7 days, then collected. Vogt can re-open the workspace for a new session |
| Token expires mid-land | The retry is refused | Land is idempotent. Re-mint and retry; the result is the same |
| Node daemon down | Clients cannot reach the socket | The client falls back to the server directly, with its local cache. Logged |
| Node cache corrupt | Hash mismatches | The node wipes the bad object or its whole cache. It holds no authoritative state |
| Server down | Writes and queries fail | `Unavailable`. Clients can still read cached objects for a known base; writes wait. During migration, git and GitHub remain usable for anything urgent (git-primary) |
| Network partition, node ↔ server | As above | Strict tiering: the node sheds load with `Unavailable` and never stampedes the server when it returns (bounded concurrency, jittered retries) |
| Thundering herd on `main` | Many retries | Backoff plus jitter. In phase 4 the per-line combiner batches lands, turning N racing retries into N rebases and one CAS |
| GitHub outage (git-primary) | Follow and batch PR export stall | Lands on graft lines continue. Follow and the next batch catch up. No data at risk |
| GitHub outage (graft-primary) | Export lags | Landing is unaffected. Export resumes from the last exported sha (pushes are idempotent, shas are deterministic) |
| Someone writes to GitHub `main` in graft-primary | The lease push fails | Export stops for that line, the divergence alert fires, and `reconcile` resolves it by hand. There is never an automatic force |
| Mirror DB lost | Exact shas of imported commits cannot be reproduced | Rebuild from GitHub by re-import matching (trees are equal by construction). The DB is backed up with the server |
| Secret committed and landed | The content is now forever (invariant 9) | `Obliterate { blob }`, admin only and journalled, replaces the frame with a tombstone. Ids are unchanged and reads return `Obliterated`. Rotate the secret: obliterating does not un-leak it, and the GitHub mirror needs its own purge |
| Clock skew | — | No server time feeds an id. `authored_at` is client data. Leases use only the server's clock |
| Old client against a newer server | — | Protocol version binding gives `IncompatibleProtocol` with the server version, and a clear CLI message |
| Huge file or huge change | Memory pressure | Blob limit (64 MiB), change-count limit, streaming frames, bounded `Diff` patches |
| Hostile or buggy client sends bad hashes | — | The server hashes every upload itself and assigns the ids |

## 15. Performance budgets

Server time for our pilot shape (Node B; rustTorrent: 673 files, about 300
commits), checked by the testkit pilot harness:

| Operation | Budget (p50 / p99) | git baseline on the trial host |
| --- | --- | --- |
| `CreateChangeset`, ≤ 10 files | 5 ms / 25 ms | `commit` + `push` ≈ 69 ms |
| `Land`, uncontended | 15 ms / 60 ms | merge queue: minutes |
| 8 concurrent lands, disjoint files, all landed | 60 ms total | ≈ 480 ms |
| `Log --path` | 5 ms / 20 ms | 7.8 ms |
| `Diff main~20..main` (names) | 5 ms / 20 ms | 4.9 ms |
| `Blame` hot file, cold / cached | 50 ms / 5 ms | 15.7 ms |
| Sparse clone of 300 files, node warm | 40 ms end to end | ≈ 125–155 ms |
| Full clone of 673 files, node warm | 120 ms end to end | ≈ 230 ms (depth 1) |
| Restart to serving | < 2 s | — |

Scale targets for the design (with no re-architecture): 50 repositories of up
to 5 GB history each, 300 concurrent sessions, 10,000 lands a day per
repository at peak, on one server with SQLite. Past that, the Postgres and S3
ports and server replicas take over.

## 16. Departures from Scion and OpenScion

| Topic | Scion (article) | OpenScion | graft |
| --- | --- | --- | --- |
| Changeset id | not specified | includes server `created_at`, so not idempotent | commits to `root`; content-only, so idempotent |
| Work in progress | changesets | unreferenced changesets, never reported | drafts held by leased workspaces, collected, with tombstones |
| Landing | server rebase, CAS, no global lock | client CAS only, so a lost race strands the client | server `Land` with retries, idempotent, policies, never strands |
| History | derived path history | client walk, one round trip per changeset (54 ms) | line path index, a range scan |
| Storage | Spanner + blob store | in-memory only (data lost on restart) | SQLite + filesystem, durable from day one, behind ports |
| Fetch | named objects, tiered | `GetObjects` fails the whole batch on one miss; no `HasObjects` | partial results, `HasObjects`, gzip frames identical at rest and on the wire |
| Tiers | daemon, regional cache, server | server only | node daemon + server; a regional tier fits later |
| Git | ingestion only | none | lossless import, a two-way mirror with one authority per line, full export |
| Interfaces | API + CLI | HTTP JSON + CLI | one API trait set: gRPC, CLI, MCP, LSP, with a parity test |
| Merge queue | none; post-merge testing | — | per-line policy: `direct`, `attested`, or `queued` |
