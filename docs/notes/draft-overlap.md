# Draft overlap: who else is editing these paths

Status: design note for WI-2310. This note doesn't change
[design.md](../design.md) or [decisions.md](../decisions.md). Two operator
decisions are already made (2026-10-23): results are gated by a new scope,
and they carry paths, ids and draft age, never contents. Accepting this note
needs a decisions.md entry (proposed D23, below) in its own PR.

## 1. Use case

The server already holds every agent's drafts. Git can't answer "who else is
changing `src/foo.rs` right now?", because its drafts sit on each machine.
graft can, before anyone lands. An agent about to edit a file asks; if
another live workspace has an open draft on it, the agent picks another file,
syncs early, or waits. Late conflicts at `Land` become early coordination.

Today `ListWorkspaces` returns only the caller's workspaces, and nothing
indexes open drafts by path, so the question costs a walk of every workspace
stack. This note adds that index and one query over it.

WI-2311's bench "live awareness" mode is the acceptance oracle: at every
probe the query must return exactly the oracle's set of
`(workspace, paths)`, and closed or expired workspaces must drop out.

## 2. Scope

**String:** `repo:<name>:drafts`. Grammar, extending §12.1:

```text
scope := "repo:" name ":" perm [ ":" arg ] | "objects:read"
perm  := "read" | "write" | "land" | "attest" | "admin" | "drafts"
```

(`attest` is on `main` already: `Scope::Attest` in
`crates/graft-auth/src/scope.rs`. §12.1 of design.md doesn't list it yet;
the D23 docs PR can fix that in passing.)

`drafts` takes no argument; `repo:<r>:drafts:<anything>` is `Malformed`.

**Composition** (in `Scope::grants`):

- `admin` grants `drafts` on its repository, as it grants everything.
- `read`, `write` and `land` do **not** grant `drafts`. This is the point of
  the operator decision: plain repository access doesn't reveal what other
  agents are doing.
- `drafts` does **not** grant `read`. It only answers the overlap query and
  its Watch filter. In practice a token holds both, since the caller needs
  `read` to name paths meaningfully, but the server requires only `drafts`
  for `OverlappingDrafts`.
- `drafts` is not prefix-restricted. A `write:<prefix>` agent that also holds
  `drafts` sees overlap anywhere in the repository. A prefix form
  (`drafts:<prefix>`) is a compatible later addition if anyone asks.

**Default grant:** none. `MintToken` adds it only when asked. Vogt's session
minting (D10) gets an opt-in per stack or per project; it isn't in the
default scope set. The operator's admin token has it through `admin`.

## 3. Data model

### The index

A new MetaStore table, **draft paths**, one row per changed path per live
workspace:

```text
draft_paths( repo, line, path, workspace ) PRIMARY KEY
  + workspace_paths( repo, workspace, path )   -- reverse, for removal
```

`path` is the full repository path of a file the stack changes (added,
modified, deleted or a rename's two ends) relative to the workspace `base`.
Directory prefixes aren't stored: a prefix query is a range scan on
`(repo, line, path >= prefix, path < prefix⁺)`, which is what the key order
gives. A workspace contributes the **union** of its stack's changed paths,
not a net diff (a path changed then reverted in a stack still counts; that is
rare and errs toward warning).

Owner, session, work item and draft age are not copied into the row. The
query joins to the workspace record (already keyed by `(repo, name)`) for
owner, session, `lease_until_ms` and `head`, and to the head's changeset
record for `received_at` (draft age). The work item comes from the
workspace's token claims, so `WorkspaceRecord` gains an optional `work_item`
field set at `OpenWorkspace` (an additive store change).

### Maintenance: same transaction (rule 3)

Every write that changes a workspace's stack updates the rows in the same
metadata transaction as the head CAS and its journal entry:

| Write | Row change | Cost |
| --- | --- | --- |
| `CreateChangeset` with a workspace (commit) | insert the new draft's changed paths | O(changes in that draft) |
| `commit --amend`, `Stack` ops, `SetHead` | replace the workspace's set: delete via `workspace_paths`, insert the union of the new stack's changes | O(stack depth × changes per draft) |
| `Sync` (rebase onto a new tip) | same as replace; the `base` moves too | as above |
| `Land` of the whole stack | delete the workspace's rows (stack empty, head on the line) | O(rows held) |
| `CloseWorkspace` | delete the workspace's rows, even with `keep_drafts` (kept drafts have no live holder) | O(rows held) |
| `OpenWorkspace` | nothing (no drafts yet) | O(1) |

Every cost is proportional to the change being written (rule 2). The draft
paths come from each changeset's `changes` list, which the write already has
in hand; no tree is walked and no snapshot materialised. The stack union is
read from the drafts between `base` and `head`, bounded by stack depth, the
same walk `Sync` already does.

### Lifetime

- **Lease lapse:** a lapse writes nothing at the moment it happens, so the
  query filters `lease_until_ms > now` on the joined workspace record at read
  time. Expired workspaces drop out immediately, with no sweeper on the read
  path; their rows still cost scan budget until GC deletes them, which is why
  a page may come back short (§4). Server
  time is used only as a filter, never in an id (rule 5).
- **Superseded drafts:** they have no holder, so they never appear; the
  replace step above removes their paths in the same transaction as the
  supersede.
- **GC:** when collection marks an expired workspace's drafts abandoned
  (§8.6), it deletes that workspace's rows in the same transaction. Until
  then the rows exist but the lease filter hides them. `Fsck` gains a check:
  every row's workspace exists and its paths match the stack (a rebuild
  repairs a mismatch).
- **Collected changesets** are never live drafts, so the index never points
  at a tombstone.

### Migration

**`WorkspaceRecord.work_item` is a stored-record change.** Checked against
`main`: workspaces are stored as plain SQL columns
(`crates/graft-store-sqlite/src/migrations.rs`, table `workspaces`) and as a
struct in the memory store. No golden vector covers them (the golden vectors
in `graft-model`, `graft-proto/src/frame.rs` and `graft-auth/src/token.rs`
pin ids, trees, changesets, frames and tokens only). So the change is a
nullable column added by a new numbered schema migration
(`ALTER TABLE workspaces ADD COLUMN work_item TEXT`), with old rows reading as
`None`. No durable encoding moves, so rule 1's operator decision is not
triggered; if review finds a vector that does cover workspaces, it would be,
and this field would wait for that decision. The same goes for the token's
`work_item` claim, which is read, not changed.

New tables only; existing workspaces are back-filled once at upgrade by
walking each live workspace's stack (bounded by live workspaces × stack
depth). No durable encoding changes, so no golden vector moves.

## 4. API

```text
OverlappingDrafts {
  repo, line,
  paths: [RepoPath],        // 1..=64 paths or directory prefixes, each non-empty
  include_own: bool = false,
  limit: u32 (1..=200),     // hits per page
  page_token?               // opaque cursor: (input index, path, workspace)
} → {
  hits: [ { path, workspace } ],          // in index order
  workspaces: [ DraftWorkspace {          // one per workspace named in `hits`
    workspace, owner, session?, work_item?,
    draft_age_ms,            // now - head.received_at
    lease_until_ms,
  } ],
  next_page_token?           // absent when every input prefix is exhausted
}
```

- **Scope:** `repo:<r>:drafts` (or `admin`). Without it,
  `ApiError::PermissionDenied { scope: "repo:<r>:drafts", path: None }`.
- **Excludes the caller's own workspaces** (owner = token `sub`) unless
  `include_own` is set.
- **Paging follows the index order.** The response is a page of
  `(path, workspace)` hits, not a page of workspaces. The input prefixes are
  sorted and de-duplicated; the cursor is `(input index, path, workspace)`,
  the last row examined. The next page resumes the range scan on
  `(repo, line, path, workspace) > (path, workspace)` within that input
  prefix, then moves to the next prefix. Nothing is gathered and re-sorted,
  so a broad prefix costs no more per page than a narrow one. Clients that
  want one entry per workspace group `hits` themselves; the CLI and MCP tool
  do.
- **No whole-repository prefix.** The empty path `""` and the root are
  rejected with `ApiError::InvalidPath { path, reason: "prefix-required" }`.
  A caller can still page through a broad top-level directory, but only one
  bounded page at a time.
- **Expired-but-not-collected rows** (lease lapsed, GC not yet run) are
  filtered at read time by joining the workspace record and dropping rows
  with `lease_until_ms <= now`, as are the caller's own rows. Filtered rows
  still count toward the page's scan budget, so **a page may be short, even
  empty, with a `next_page_token`**. Clients keep paging until the token is
  absent.
- **Cost per page** (rules 2 and 7): at most `S = 4 × limit` index rows
  scanned (`S ≤ 800`), plus one workspace-record point read per distinct
  workspace among them (memoised per page, ≤ `S`), plus one changeset-record
  read per distinct head for draft age. So O(limit) per page, independent of
  the number of workspaces, drafts, prefixes' total breadth or history.
  Response size is bounded by `limit` hits plus at most `limit`
  workspace entries.
- **Errors** (all `graft_proto::ApiError`, none carrying contents or
  tokens):
  - `PermissionDenied` — no `drafts` scope;
  - `InvalidPath` — a malformed path, or the empty/root prefix;
  - `LimitExceeded { limit: "paths", max: 64 }` — too many input paths;
    `LimitExceeded { limit: "limit", max: 200 }` — page size too large;
  - `InvalidChange { rule: "bad-page-token" }`-style rejection is avoided:
    a token that fails to decode, or was issued for different inputs, is
    `InvalidPath { path: "", reason: "bad-page-token" }`. (If a dedicated
    variant is preferred, it is an additive `ApiError` change in step a.)
  - `NotFound` for an unknown repository or line, as other reads do.

**Optional Watch event.** A journal entry `DraftPathsChanged { workspace,
line, added: [path], removed: [path] }` (paths capped as above, with a
`truncated` flag) written in the same transaction as the row change. A
client watches with `kinds = ["DraftPathsChanged"]` and intersects the paths
with its own. Delivery to watchers is gated by the `drafts` scope: a `Watch`
caller without it never receives this kind. The server doesn't compute
"overlaps mine" per watcher; that would be per-watcher state on the write
path. The entry is draft-only, so it is compacted after the retention window
(§4.8). This is a second step; the query alone meets the acceptance.

## 5. Surfaces

- **CLI:** `graft status --upstream --drafts` (built on WI-2309's
  `--upstream`). It sends the working copy's changed paths plus the stack's,
  and prints one line per overlapping workspace: owner, session or work item,
  age, and the shared paths. `--drafts` alone with no local changes takes
  paths from arguments (`graft status --drafts src/`). Exit status stays 0;
  `--json` gives the raw response.
- **MCP:** `graft_draft_overlap { line?, paths }`, returning the response as
  JSON. Agents call it before starting a file. It is listed only when the
  session token holds `drafts`.
- **LSP:** none now. A later diagnostic ("2 other drafts touch this file")
  can reuse the query.
- **JSON facade:** `GET /v1/repos/<r>/drafts/overlap?line=&path=…`.

## 6. Privacy and threat notes

A `drafts` token learns, for the repository:

- which live workspaces exist on a line, their owners, Vogt sessions and work
  items, when their head draft was written and when their lease ends;
- which paths their stacks change, intersected with paths the caller names.

It does **not** learn:

- file contents, changeset ids, messages, diffs or line ranges (no `head` id
  is returned, so a `drafts`-only token can't fetch the draft with
  `GetObjects` or `ReadFile`);
- anything about closed or expired workspaces, or other repositories;
- tokens or token ids (`jti`).

Residual risks:

- **Path enumeration.** A caller can ask for prefix `""` and page through
  every path every agent is touching. That is the intended capability; it is
  why `drafts` is a separate scope and isn't granted by `read`. Path names
  can themselves be sensitive (an unreleased feature's file names); a
  repository with path read restrictions (§12.2) should not hand out
  `drafts`, or a later `drafts:<prefix>` form must intersect with them.
- **Activity timing.** Draft age and lease end reveal when an agent is
  working. Accepted: Vogt already shows this to the operator.
- **Logs.** Requests log the repo, line and path count, not the path list,
  matching how other path-bearing RPCs log.

## 7. Alternatives considered

1. **Scan at query time** (walk every live workspace's stack per query). No
   new table, but the cost is O(workspaces × stack × changes) per query,
   which breaks rule 2 and gets worse with exactly the concurrency this
   feature exists for. Rejected.
2. **Gate by `read`.** Simpler tokens, but every reader sees every agent's
   activity. Overruled by the operator decision.
3. **Index whole drafts, not workspaces** (rows per draft changeset). Every
   supersede and abandon would need row updates for drafts that aren't
   anyone's head, and the query would have to dedupe by holder. The
   per-workspace union is smaller and matches what the caller asks.
4. **Derived kind (§4.9).** Derived data is keyed by changeset and
   immutable; draft overlap is mutable per workspace, so it doesn't fit.
5. **Server-side overlap push** (Watch computes "overlaps mine" per watcher).
   Precise, but per-watcher state on the write path. The client-side
   intersection over `DraftPathsChanged` is cheap and stateless. Deferred.
6. **Advisory locks** (claim a path before editing). Changes the model from
   awareness to coordination, and reintroduces the stale-lock problem graft
   avoids. Out of scope; an overlap query is the cheaper first step.

**Recommendation:** the per-workspace path index (§3), the query (§4) gated
by `repo:<r>:drafts`, and the CLI and MCP surfaces. The Watch event is a
follow-up step once the bench shows the query alone helps.

## 8. Crates touched and step split

| Step | Crates | Contents |
| --- | --- | --- |
| **a. Interface PR** | `graft-auth`, `graft-proto`, `graft-store` | `Scope::Drafts` with parse, display and `grants` (+ tests); `OverlappingDrafts`/`DraftOverlap` types and the `Api` method stubbed; MetaStore port methods `draft_paths_replace`, `draft_paths_delete`, `overlapping_drafts` and `WorkspaceRecord.work_item`, with stubbed bodies |
| **b. Store and engine** | `graft-store` (memory + conformance), `graft-store-sqlite`, `graft-engine`, `graft-gc` | Tables and back-fill migration; row maintenance in commit, amend, stack, sync, land and close; GC and `Fsck` handling; the query with the lease filter; conformance tests for the four acceptance cases (overlap seen, disjoint not, closed and expired drop out, no contents) |
| **c. Surfaces** | `graft-server`, `graft-cli`, `graft-mcp` | gRPC and JSON facade routes with the scope check; `graft status --upstream --drafts`; `graft_draft_overlap` MCP tool; an end-to-end test in `graft-server/tests` |
| **d. (optional) Watch** | `graft-store`, `graft-engine`, `graft-server` | `DraftPathsChanged` journal entry and its scope-gated delivery |

Steps b and c can overlap once a lands (c programs against the memory store).
The bench check (WI-2311, live-awareness mode) runs after c.

**Decision needed:** a decisions.md entry (proposed **D23: draft-overlap
visibility**) recording the `repo:<r>:drafts` scope, its non-composition with
`read`, the default of no grant, and the no-contents rule; plus a §7.2 and
§12.1 update in design.md. Both go in their own docs PR before step a lands.
