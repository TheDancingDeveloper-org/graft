# MCP guide

Graft's MCP server exposes bounded source-control reads and writes to agents.
Every result is capped at 64 KiB. List and patch tools may return less than the
requested limit to stay under that cap.

## Register the server

From a Graft working copy, print the registration for your agent client:

```sh
graft mcp --print-config claude
graft mcp --print-config codex
graft mcp --print-config cursor
```

The generated registration starts `graft mcp` over stdio. It never prints a
token. The token is read from Graft's configured server, a configured token
file, or `GRAFT_TOKEN`; it is never a tool argument.

An agent without a checkout can connect by streamable HTTP to the Graft
server's `/mcp` endpoint. Configure that URL and its bearer token in the MCP
client rather than passing a token to any tool. HTTP provides repository and
workspace operations, while working-copy-only behavior is available only from
`graft mcp` over stdio inside a working copy.

## Results, paging, and errors

- Results are structured JSON. Tool arguments reject unknown fields.
- When a result has `next`, call the same tool again with that value as
  `cursor`. Continue until `next` is `null`; a short page is not the last page.
- `graft_read` uses `next_offset`: pass it back as `offset` until it is `null`.
- Patches are capped. Each text patch is at most 4 KiB and reports
  `truncated: true` when clipped; binary changes have a null patch.
- Errors are structured and include `kind` and `message`. API errors also
  report `retryable`. Common kinds include `invalid_argument`, `not_found`,
  `unauthenticated`, `forbidden`, `stale_base`, `conflict`, `contended`,
  `unavailable`, and `needs_working_copy`. A `conflict` result identifies the
  target and paths to resolve; `stale_base` means the line moved and the
  workspace must be synchronized before retrying.

## Read tools

### `graft_log`

Reads first-parent history, optionally restricted to paths.

- Arguments: `repo` and `from` are required; `until`, `paths`, `limit` (1-100,
  default 50), and `cursor` are optional.
- Result: `changesets` contains bounded changeset summaries and `next` is the
  resume token. Keep paging with `cursor` while `next` is set.
- Errors: invalid refs or paths use `invalid_argument` or `not_found`.

### `graft_show`

Reads one changeset and its first-parent patch.

- Arguments: required `repo` and `changeset`; optional `limit` (1-100, default
  20) and `cursor`.
- Result: `changeset`, `files`, and `next`. File entries include `path`, `op`,
  `kind`/`id` for puts, `patch`, and `truncated`. Keep paging with `cursor`.
- Errors: an unknown changeset is `not_found`.

### `graft_diff`

Diffs two snapshots, optionally for selected paths.

- Arguments: required `repo`, `from`, and `to`; optional `paths`, `patch`
  (default true), `limit` (1-100, default 20), and `cursor`.
- Result: `files` and `next`; file entries use the same capped patch shape as
  `graft_show`. Keep paging with `cursor`.
- Errors: invalid snapshots or paths are typed errors.

### `graft_blame`

Reads line origins for a bounded window of one file.

- Arguments: required `repo`, `at`, and `path`; optional zero-based
  `start_line` and exclusive `end_line` (at most 1,000 lines by default).
- Result: file identity and a `lines` array with line ranges and originating
  changesets. Request another range to continue.
- Errors: missing or non-text files return a typed error.

### `graft_read`

Reads a bounded byte range from one file.

- Arguments: required `repo`, `at`, and `path`; optional `offset` (default 0)
  and `limit` (1-65,536 bytes, default 16,384).
- Result: `path`, `kind`, `encoding`, `data`, `offset`, and `next_offset`.
  Continue by passing `next_offset` as `offset` until it is null.
- Errors: missing paths, invalid offsets, and directories are typed errors.

### `graft_ls`

Lists a directory at a snapshot.

- Arguments: required `repo` and `at`; optional `path` (root by default),
  `recursive` (default false), `limit` (1-500, default 100), and `cursor`.
- Result: `entries` with path, kind, and object id, plus `next`. Keep paging
  with `cursor`.
- Errors: a missing directory or invalid path is typed.

### `graft_status`

Reads the current working copy's repository, workspace, and changed paths.

- Arguments: required `repo`.
- Result: working-copy repository/workspace identity and a capped changed-file
  summary.
- Errors: `needs_working_copy` outside stdio `graft mcp`, or when the requested
  repository is not the current working copy.

### `graft_upstream`

Shows how far a workspace is behind its line and predicts whether its work
merges cleanly onto the tip.

- Arguments: required `repo`; optional `workspace` (defaults to the current
  working copy), `limit` (1-100, default 50), and `cursor`.
- Result: `line`, `workspace`, `base`, `head`, `tip`, `up_to_date`, `behind`,
  `changed`, `overlaps`, `stack_conflict`, `clean`, and `next`. Keep paging the
  `changed` list with `cursor`.
- Errors: `workspace` is required outside a working copy; missing workspaces
  and refs are typed errors.

### `graft_draft_overlap`

Finds other live workspaces whose open drafts touch files or directory
prefixes you plan to change. It never returns file contents.

- Arguments: required `repo` and `paths` (1-64); optional `line` (default
  `main`), `include_own` (default false), `limit` (1-200, default 50), and
  `cursor`.
- Result: `line`, grouped `overlaps` with workspace, owner, session, work item,
  draft age, lease and paths, plus `next`. Keep paging with `cursor`.
- Errors: requires the repository drafts scope; otherwise returns `forbidden`.

### `graft_review_queue`

Lists landed changesets on a line that still need a passing review.

- Arguments: required `repo` and `line`; optional `since_lsn`, `path_prefix`,
  `max_lines`, and `limit` (1-1,000, default 50).
- Result: bounded `items` with changeset, position, journal LSN, author, touched
  paths, and changed-line count, plus `next_lsn` for the next page.
- Errors: invalid lines, paths, limits, or cursors are typed.

### `graft_get_attestations`

Lists attestations for one changeset or a bounded line-position range.

- Arguments: required `repo`, plus either `changeset` or `line`, `from_pos`, and
  `to_pos`; optional `kinds`, `limit` (1-1,000, default 50), and `cursor`.
- Result: bounded `attestations` and `next`. Keep paging with `cursor`.
- Errors: invalid selectors, kinds, limits, and cursors are typed.

### `graft_get_land_checks`

Lists the effective check state of landed changesets on a line.

- Arguments: required `repo` and `line`; optional `from_pos`, `limit` (1-1,000,
  default 50), and `cursor`.
- Result: bounded `rows` with position, changeset, state, and optional revert,
  plus `next`. Keep paging with `cursor`.
- Errors: invalid lines, positions, limits, and cursors are typed.

### `graft_attest`

Records a check, review, or approval outcome for a changeset.

- Arguments: required `repo`, `changeset`, `kind`, `check`, and `status`;
  optional display name `by` and `url`.
- Result: `{"changeset": "<id>", "recorded": true}`. The recorded attestation is not returned.
- Errors: invalid changesets, kinds, statuses, or oversized fields are typed.

## Workspace and draft tools

### `graft_workspace`

Opens, reads, renews, or closes a server-side workspace.

- Arguments: required `repo` and `action` (`open`, `get`, `renew`, `close`).
  `name` is required except when opening; `line`, `at`, and `lease_ms` apply to
  open/renew, and `keep_drafts` applies to close.
- Result: the workspace view (`name`, `line`, `base`, `head`, owner,
  attribution, and lease), or close confirmation.
- Errors: invalid action-specific arguments, missing workspaces, or contention
  are typed errors.

### `graft_commit`

Creates a draft on a workspace from inline file changes or current
working-copy changes.

- Arguments: required `repo`, `workspace`, and `message`; `files` contains up
  to 256 `{path, op, content, encoding, kind}` entries and at most 1 MiB of
  content. `from_working_copy` selects current local changes. Optional `parent`
  and `authored_at_ms` make retries deterministic.
- Result: draft changeset id, `parent`, and `authored_at_ms`.
- Errors: invalid content/path data, `needs_working_copy`, `stale_base`, or
  workspace errors are typed.

### `graft_sync`

Rebases a workspace stack onto its line tip or a named target.

- Arguments: required `repo` and `workspace`; optional `onto`.
- Result: updated workspace/base/head and any rebased draft ids.
- Errors: `conflict` reports the target and conflicting paths; contention and
  missing refs are typed.

### `graft_land`

Lands a workspace stack by compare-and-swap onto one line.

- Arguments: required `repo` and `workspace`; optional `head`, `line`,
  `squash`, merge mode (`line` default or `file`), `check` (default true), and
  `merged_from` for a resolved line merge.
- Result: landed changeset id, line, previous tip, and pre-land summary.
- Errors: `stale_base` means synchronize and retry; `conflict` supplies paths;
  `contended` is retryable. The pre-land check never runs a build or test.

### `graft_conflicts`

Previews a workspace stack's conflicts against a target without moving refs.

- Arguments: required `repo` and `workspace`; optional `onto`, `limit` (1-64,
  default 16), and `cursor`.
- Result: target, expected workspace head, `conflicts` with bounded markers,
  and `next`. Keep paging with `cursor`.
- Errors: invalid targets and workspace state are typed.

### `graft_resolve`

Rebases a conflicted stack using caller-provided resolutions.

- Arguments: required `repo`, `workspace`, `onto`, and `resolutions` using the
  same file-change shape as `graft_commit`; optional `expected_head` protects
  the update with compare-and-swap.
- Result: updated workspace and rebased draft ids.
- Errors: incomplete or stale resolutions return `conflict` or `stale_base`.

### `graft_revert`

Creates a draft that inverses a changeset.

- Arguments: required `repo` and `changeset`; optional `onto` and `workspace`.
- Result: new draft changeset id and workspace information.
- Errors: overlapping inverse changes return `conflict`; unknown changesets
  and invalid workspace state are typed.

### `graft_pick`

Creates a draft that reapplies a landed changeset.

- Arguments: required `repo` and `changeset`; optional `onto`, `workspace`,
  and merge mode (`line` default or `file`).
- Result: new draft changeset id and workspace information.
- Errors: overlapping changes return `conflict`; unknown changesets and
  invalid workspace state are typed.

## Ref tools

### `graft_line_create`

Creates a new line at a changeset without overwriting an existing line.

- Arguments: required `repo`, `name`, and `from`.
- Result: line name and target changeset.
- Errors: an existing line is `already_exists`; invalid names or refs are
  typed.

### `graft_merge`

Merges one line into another while recording a merge point.

- Arguments: required `repo` and source line `src`; optional target `into`
  (default `main`), source `at`, `message`, and merge mode (`line` default or
  `file`).
- Result: landed changeset, source and target line information, and merge
  point.
- Errors: conflicts return `conflict`; stale or contended target updates are
  typed and never move two refs at once.

### `graft_tag`

Creates an immutable tag at a changeset.

- Arguments: required `repo`, `name`, and `at`; `tags/` is added when absent.
- Result: tag name and target changeset.
- Errors: an existing tag is `already_exists`; invalid names or refs are
  typed. Tags never move.
