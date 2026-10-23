# Conflict resolution for MCP-only agents

Status: note for WI-2312 (step 1: repro and options). This note doesn't
change [design.md](../design.md) or [decisions.md](../decisions.md). The
recommendation below needs an operator decision and its own design PR.

## Question

Can an agent that uses only the server's `/mcp` endpoint, with no working
copy, finish a land that conflicted? In other words, can it resolve a
conflict by hand through MCP and the API?

## Short answer

**Not in place.** The conflicting draft cannot be resolved or landed through
MCP. The one route that works is to give up the stack: close the workspace
without keeping its drafts, reopen it on the new tip, commit the merged file
contents inline, and land. The agent has to build the merge itself from three
`graft_read` calls. That route loses the link to the original draft, and a
stack of several drafts has to be rebuilt by hand.

## Observed behaviour

This is pinned by
`crates/graft-server/tests/mcp_conflict.rs`,
`mcp_only_agent_cannot_resolve_in_place_and_must_abandon_and_recommit`. It
runs the real `graft-server` binary with `main` set to `direct` explicitly,
and drives `/mcp` over HTTP with two agent tokens (`write` and `land:main`).
Agents A and B open workspaces `wa` and `wb` on `main`. Each commits inline a
different edit to line 2 of `f.txt`. A lands first.

| Step | MCP call (as B) | Result today |
| --- | --- | --- |
| 1 | `graft_land` (merge `line`, then `file`) | `{kind: "conflict", onto: <A's landed id>, paths: ["f.txt"], retryable: false}`. Those are the only keys. The engine's `ConflictPath` holds `base`, `ours` and `theirs` ids, but `render.rs` drops them. There's no content and there are no markers |
| 2 | `graft_sync` (default and `onto`) | The same `conflict`. Nothing moves: the workspace base stays on the old tip and the head stays on B's draft |
| 3 | `graft_read` at the base, the draft and `onto`/`main`; `graft_diff` base→onto | Works. The agent can fetch all three sides, one call each, and run its own three-way merge |
| 4 | `graft_commit` with the resolved content (default parent) | Succeeds, but stacks the fix on top of the conflicting draft. `graft_land` and `graft_sync` then conflict on `f.txt` again, because the bottom draft rebases first. `graft_land` with `head = fix` gives the same result |
| 4b | `graft_land` with `squash: true` | `rejected`, `policy: "squash"`: landing a stack as one changeset isn't built yet |
| 5 | `graft_commit` with `parent = onto` (or `"main"`) | `ref_conflict`, `name: "wb"`, `actual: <head>`. `CreateChangeset` with a workspace CASes the head from `parents[0]` (§7.4), so a draft can't start a new stack on `onto` in the same workspace |
| 6 | `graft_revert` | Not registered on `/mcp`. `tools/call` returns JSON-RPC error `-32602`. It is listed in design §10.1 and in `graft_mcp::TOOLS`, but `McpServer` has no handler for it. `tools/list` returns 11 tools, and none of them drops, replaces or reorders drafts (`Stack`, `SetHead`) |
| 6b | `graft_workspace open` with `at = <draft>` | `invalid_change` (`base-not-on-line`). A workspace can only open on the line |
| 7 | `graft_workspace close` (`keep_drafts: false`), `open` the same name on `main`, `graft_commit` the resolved content, `graft_land` | **Lands** (`rebased: false`). `main` now holds the resolved file. The landed changeset's parent is `onto`, and its metadata is empty: there's no `graft.rebased-from` link to B's draft, which is now `abandoned` |

Step 7 is what the CLI already does. `graft sync` on a conflict
(`WorkingCopy::sync_writing_conflicts`) closes the workspace without its
drafts, reopens it on the tip, and writes merge3 markers into the working
copy. The stack becomes one draft when the user commits. So the CLI and the
MCP workaround produce the same result. The difference is that the CLI
computes the markers locally and keeps the user's edits as uncommitted
changes, while the MCP agent has to do both itself, with nothing on the
server to help.

What the MCP-only agent is missing:

- **Which ids conflict.** The base, ours and theirs ids are computed but not
  rendered. The agent has to work out the base (the workspace `base`) and
  ours (its own draft, or whichever draft in a stack failed, which is not
  reported).
- **Rendered markers.** `graft_model::merge3` exists and the server runs it
  for D16, but no tool returns its output.
- **A way to put a resolution on `onto` in the same workspace,** keeping the
  draft's identity. Today the only option is to discard the workspace.
- **Stack edits** (`Stack drop/squash`, `SetHead`): the API has them, MCP
  doesn't expose them.

Costs of the workaround, in addition to the extra round trips:

- **Lineage.** The result has no `graft.rebased-from`, so reviewers and the
  `attested` policy can't tie it back to the draft. Under `attested`, the new
  changeset needs fresh attestations. Under D6, an attested resolution of an
  attested draft would be re-tested from scratch.
- **Stacks.** Each draft above and below the conflict must be re-created by
  hand, in order, with inline content capped at 1 MiB and 256 files per
  commit.
- **Window.** Between `close` and `open` the agent holds no drafts. A
  crashed agent leaves only abandoned drafts, which are recoverable for the
  retention window but which nothing will re-adopt automatically.

## Option (a): `graft_conflicts` and `graft_resolve`

Two new tools, backed by two new API operations.

**`graft_conflicts {repo, workspace, onto?}`** (read-only). It runs the same
planner as `Sync`, in memory only, against `onto` (default: the line tip).
It returns:

- `onto`, and the draft whose rebase failed (`draft`);
- per path: `path`, `base`, `ours` and `theirs` (ids and entry kinds, any of
  them absent for an add or delete);
- for text files that pass the D16 limits (all three sides text and under
  1 MiB): the `merge3` output with diff3 markers, capped by the result budget
  (64 KiB per result, with a per-path cap and `truncated`), and paged by path
  with `next`;
- for binary, oversized, symlink or kind-changing paths: the ids only, with a
  `reason` (`binary`, `too-large`, `kind`, `clash`).

The agent reads larger sides with `graft_read` by id. Nothing here can grow
without bound, so rule 7 (bounded responses) holds.

**`graft_resolve {repo, workspace, onto, expected_head, resolutions: [{path,
content | delete | take: ours|theirs|base}]}`** (write). In one transaction
it rebases the whole stack onto `onto` using the caller's resolutions:

- It plans the stack bottom-up as `Sync` does. At the first draft that
  conflicts, each conflicting path takes the caller's resolution in place of
  the failed merge. Paths the caller didn't resolve make the whole call fail
  with `Conflict` (same shape as today), and nothing is written.
- A resolution for a path that doesn't conflict is refused
  (`invalid_argument`), so a stale resolution can't overwrite a clean merge.
- Resolved blobs go in as inline blobs (same caps as `graft_commit`).
- The rewritten draft keeps author, time, message and metadata. It adds
  `graft.rebased-from` as any rebase does, plus `graft.resolved-paths` (a
  sorted JSON array, alongside `graft.merged-paths`).
- The workspace moves `base → onto` and `head → new head` by CAS on
  `expected_head` (rule 4: one CAS, on the workspace; the line is untouched
  until `graft_land`).
- **Deterministic id.** The changeset id is content-only. The same draft,
  `onto` and resolutions give the same id, so a retry finds the stored
  changeset (rule 5: no server time or randomness).

The agent then calls `graft_land`. The stack now sits on `onto`, so if the
tip hasn't moved, the land is a fast-forward. If it has moved, the land
rebases as usual and, with D16, usually merges cleanly.

Changes:

| Crate | Change |
| --- | --- |
| `graft-proto` | `Conflicts` request and response; `Resolve` request with `Resolution { path, entry }`; one `WorkspaceApi` method each. Minor protocol bump. `pb` messages and conversions |
| `graft-engine` | `rebase.rs`: expose the planner with an override map `path → entry` and a dry-run mode that returns the per-path sides and `merge3` output. `workspace.rs`: `do_resolve`, which is `Sync` with overrides plus the new metadata key |
| `graft-model` | none (`merge3` already renders diff3 markers). One new metadata key name |
| `graft-server` | gRPC service wiring; scope checks as for `Sync` (`write` on each resolved path) |
| `graft-mcp` | two tools, their argument structs and bounded rendering. Also render `base/ours/theirs` in the existing `conflict` error (additive) |
| `graft-client`, `graft-cli` | optional: `graft sync` could use `Resolve`. Not required |

Idempotency: `graft_conflicts` is read-only and deterministic for a given
`(stack, onto)`. `graft_resolve` is a CAS on the workspace head, with a
deterministic output id. A repeat after success sees `head == new` and
returns success, as `SetHead` does (§7.9).

**D16.** `graft_resolve` runs the same line merge as `Sync` for every path the
caller didn't resolve, so the caller only handles the hunks that truly
conflict. The markers come from the same `merge3` the engine would have
applied, so what the agent sees is exactly why the rebase failed. Paths
merged cleanly go in `graft.merged-paths` as they do today.

**Phase 2 (`attest_merged`).** A resolved draft is a new snapshot nobody has
tested. It should count as merged at least: the attestation lookup should
stop at `graft.resolved-paths` as it does at `graft.merged-paths`, so CI
attests the resolved head again. The details belong to phase 2.

## Option (b): expose stack edits plus an `onto`-parented commit

Expose what the API already has, and loosen one rule:

- `graft_stack {repo, workspace, op: drop|squash|reorder|split, ...}` →
  `Stack` (§8.5).
- `graft_set_head {repo, workspace, expected, new}` → `SetHead`.
- `graft_commit` gains `rebase_base: true` (or a new `graft_workspace` action
  `rebase {onto}`) to commit a draft whose parent is `onto` (the line tip),
  moving the workspace base to `onto` and the head to the new draft in one
  CAS. Today `check_extends` refuses this with `ref_conflict`.

The agent's flow: `graft_read` the three sides, merge them itself, call
`graft_commit` with the resolved full contents of every path the stack
changed, with parent `onto` and `rebase_base`, then drop the old drafts with
`graft_stack drop` (or have the commit replace the stack), then `graft_land`.

Changes:

| Crate | Change |
| --- | --- |
| `graft-proto` | `CreateChangeset` (or `SetHead`) needs a way to move `base` too: a new field (`rebase_workspace: bool`) or a new `Rebase` mode on `SetHead`. Minor protocol bump |
| `graft-engine` | `write.rs` `check_extends`: allow `parent == onto` when asked, and move the base in the same transaction. Decide what happens to the old drafts: superseded by the new one, or abandoned |
| `graft-mcp` | `graft_stack`, `graft_set_head`, the new commit flag; register `graft_revert` while there |
| `graft-server` | wiring only |

Idempotency: the commit id is content-only, and the workspace move is a CAS,
so a retry is safe. But dropping old drafts is a separate call. Without a
"replace" flag the flow is two transactions, which breaks rule 3 (one
transaction per logical write), and a crash between them leaves both stacks.

**D16.** None of the line-merge machinery is used. The agent merges by
itself, with nothing on the server to show the conflicting hunks, and the
server can't tell a resolution from an ordinary edit. Nothing records which
paths were resolved.

**Phase 2 (`attest_merged`).** The new draft is just a new draft. It needs
fresh attestations under `attested`, whether or not the rebase was trivial.
That's safe, but it's more CI than (a) needs for paths that merged cleanly.

## Comparison

| | (a) `graft_conflicts` + `graft_resolve` | (b) stack edits + `onto` commit |
| --- | --- | --- |
| Agent work | Read markers, send resolved files | Fetch three sides, merge everything itself, re-send every changed path |
| Lineage | Kept (`rebased-from`, `resolved-paths`) | Lost unless (b) adds a "supersedes" rule |
| Transactions | One per resolve | Two (commit, then drop), unless a "replace" flag is added |
| Stacks | Whole stack rebased; only conflicting paths in the first failing draft need input | Agent rebuilds the stack, or squashes it to one draft |
| D16 | Reused: same merge, same markers | Not used |
| New engine surface | Planner overrides and a dry-run (moderate) | Relaxed `check_extends` and a base move (small) |
| Also useful for | `graft sync` in the CLI (server-rendered markers) | General stack editing over MCP |

## Recommendation

**Option (a).** It's the one that matches the design. Conflicts come back as
ids plus bounded, rendered content. Resolution is one CAS transaction with a
deterministic id. The D16 merge and its metadata are reused instead of
bypassed. It also keeps the costs-follow-the-change rule: the planner reads
only the changed paths of the stack, and the agent sends only the paths that
conflict.

Two small, independent fixes are worth doing first, whatever is chosen:

1. Render `base`, `ours` and `theirs` (ids) and the failing draft in the
   existing `conflict` error. This is additive, and the engine already
   computes them.
2. Either register `graft_revert` on `/mcp` or remove it from
   `graft_mcp::TOOLS` and §10.1. They disagree today.

Until (a) ships, the workaround in step 7 is the documented route for
MCP-only agents. It's also worth saying so in the `/mcp` server instructions,
so agents don't loop on `graft_sync`.
