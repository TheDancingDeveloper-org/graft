# Proposal: mirrors as repository settings, one mirror service

Status: draft for operator review, 2026-11-03. This is the design PR for
moving mirror definitions out of deploy files and into graft. On sign-off it
becomes decision D40 and amends `design.md` §11; nothing here is built before
that.

## 1. Problem

`graft-mirror follow --config <file>` follows one repository. graft-dev
therefore runs one `graft-mirror-<repo>` container per followed repository,
thirteen today, all in one pod. Each has its own config, graft token, git
cache, SQLite file, metrics port and volume subPath, rendered from
`deploy/mirrors.tsv` and `deploy/mirror-<repo>.toml` by the ops repo's
`gen.py`.

What that costs:

- every deploy restart-storms the pod while the server comes up;
- thirteen copies of the runtime, the poll loop and the git cache;
- onboarding a repository is an edit to graft's deploy files, an ops PR, a
  render, an Infisical secret, an ESO mapping and a redeploy. A hand-placed
  secret is overwritten by ESO, which is what happened on the last
  onboarding;
- `pr-batch` is an operator `kubectl exec` into the right container.

None of this is required by the design. `design.md` §11 says the mirror is a
separate service and the only place git exists. It does not say one process
per repository. The per-repo shape is how the first trial mirror was copied
for each new repository.

**Operator position (2026-11-03):** graft is a standalone stack. Users and
agents create and manage repositories, and their mirrors, live through the
API and MCP. Mirror definitions are not infrastructure-as-code.

## 2. Target

- **Repositories and mirrors are API objects.** A mirror is a setting on a
  repository, stored in the server's meta DB, managed by `graft repo mirror`
  and matching MCP tools, readable as status from the same place.
- **One mirror service.** One process, one GitHub App key, one volume. It
  learns which repositories to mirror from the server and reconciles its set
  of follow and export tasks, so a mirror added through the API starts
  within one poll interval, with no deploy.
- **Nothing per repository in deployment.** The Deployment, the server's
  signing key, the GitHub App key and the mirror service's token are the
  whole deploy-time surface.
- **Batching is an API call.** Cutting, excluding from and settling a batch
  PR are requests recorded on the server and executed by the mirror service.
  Nobody execs into a container.

## 3. What changes in graft

### 3.1 A global scope for repository administration

Scopes today are all `repo:<name>:…` (`graft-auth` `scope.rs`). There is no
scope that covers repositories that do not exist yet, so repository creation
is an admin CLI call rather than an API anyone can be granted.

Add two forms to the scope grammar:

| Scope | Grants |
| --- | --- |
| `repos:admin` | `CreateRepo`, `DeleteRepo`, `ListRepos`; the holder gets `repo:<name>:admin` on a repository it creates |
| `repo:*:<perm>[:<arg>]` | the named permission on every repository, now and later |

`repo:*:admin` is for service principals only (the mirror service, an
orchestrator). The token kind `mirror` names the mirror service's token so it
is auditable and revocable as one thing (`token_denylist`). The grammar
change is text-only; no encoding moves, so no golden vector moves.

Repository creation becomes `graft repo create <name>` and the MCP tool
`repo_create`, under `repos:admin`. The existing admin CLI keeps working.

### 3.2 The mirror setting

A new meta table, `repo_mirrors`, one row per `(repo, upstream)`:

| Column | Meaning |
| --- | --- |
| `repo` | the graft repository |
| `upstream` | upstream URL, `https://github.com/OWNER/REPO.git` or any git URL |
| `lines` | list of `{branch, line, direction}`; `direction` is `follow`, `export` or `both` |
| `interval_s` | poll interval, default 15 (D9) |
| `enabled` | bool |
| `credential` | nullable: an encrypted per-mirror credential (§3.4) |
| `export` | author mapping and default author, as today's `[export]` |
| `status` | written by the mirror service (§3.3) |
| `created_by`, `updated_by`, `updated_at` | audit |

`graft-proto` gets the typed `Mirror`, `MirrorLine` and `MirrorStatus`
messages, and `RepoService` gets `SetMirror`, `GetMirror`, `ListMirrors`,
`RemoveMirror`, all under `repo:<name>:admin`. `ListMirrors` across
repositories is under `repo:*:read` or `repos:admin` and is paginated.

The CLI and MCP surface:

```
graft repo mirror add    <repo> <upstream> --follow main=main [--export …] [--interval 15]
graft repo mirror set    <repo> <upstream> [--enable|--disable] [--follow …]
graft repo mirror remove <repo> <upstream>
graft repo mirror status [<repo>]
```

MCP tools: `mirror_add`, `mirror_set`, `mirror_remove`, `mirror_status`,
with the same bounds (paginated status, no credential in any response).

Branch names and an upstream URL are stored as strings. The server does not
parse them, fetch them or hold any git type; this keeps rule 6 (git stays in
`graft-git` and `graft-mirror`). The design PR states this as the boundary.

### 3.3 The mirror service

`graft-mirror serve` replaces `graft-mirror follow --config`. Its config is
the server address, its token file, its data directory, the GitHub App key
file and installation id, metrics address, and limits (§4). No repositories.

Reconcile loop, every `interval_s` (default 15 s) and on start:

1. `ListMirrors` with `enabled = true` (paginated).
2. For each mirror not yet running, create its data directory
   (`<data>/<repo>/mirror.git`, `<data>/<repo>/mirror.sqlite`) and spawn a
   follow task. The per-repository SQLite and git cache keep today's schema
   and layout, so a repository's state is one directory.
3. For each running task whose row changed, restart it with the new lines.
4. For each running task whose row is gone or disabled, stop it and leave its
   directory (removal of data is a separate, explicit operation).
5. After each poll a task writes `status` back with `SetMirrorStatus`: last
   poll time, last imported sha and changeset, lag in changesets and
   seconds, last error, `diverged`.

Each repository's follow task is today's `poll_once` unchanged: `ls-remote`,
fetch, import, admin `UpdateRef` CAS on the line, with the followed-line
marker `graft.followed/<line>` and the authority claim as they are. The
authority switch (design §11.3) stays per line and per repository.

Metrics: one `/metrics` endpoint, every mirror series gains a `repo` label.
The alert rules in `deploy/monitoring/` change from thirteen jobs to one
with a label.

Why a separate service and not the server: the server would otherwise have
to run git, hold a GitHub credential and push to a forge. Design §11 keeps
all of that out of the server, and D38's web UI and D39's independence are
both easier to hold when the server stays a store plus an engine.

### 3.4 Credentials

- **GitHub App (D8).** One App installed on the organisation, key and
  installation id in the mirror service's secret. The service mints
  installation tokens per call. A repository in that organisation needs no
  credential of its own, so adding it is one API call with no secret.
- **Other upstreams** (another organisation, another host) need a credential
  on the mirror row. `SetMirrorCredential` is write-only: the server encrypts
  the value at rest with a key derived from its signing key and a per-row
  nonce, stores the ciphertext, and only `GetMirrorCredential`, under the
  `mirror` token kind, returns it, to the mirror service, over the normal
  authenticated channel. No other API returns it, it is never logged, and
  the CLI reads it from a file or stdin, never argv (rule 8).
- Rotation: the App key is rotated by replacing one secret and restarting the
  service. A per-mirror credential is rotated by `SetMirrorCredential`.
- Rejected: storing Infisical references. That ties graft to the estate's
  secret store, against D39.

### 3.5 Batching as an API

Batch state today lives in the mirror's SQLite (`batches*` tables) and the
command runs inside the container. The state stays where it is; the control
moves to the server as a request queue:

| Call | Effect |
| --- | --- |
| `CutBatch(repo, line, base, excludes)` | records a request; the mirror service runs today's `pr-batch` and records the result (PR URL, commits, or the failure) |
| `GetBatch(repo, line)` | the open batch, its PR, its lands, its state |
| `ListBatches(repo)` | paginated history |

CLI `graft repo batch cut|status|list`, MCP `batch_cut`, `batch_status`,
`batch_list`. Scope `repo:<name>:admin`. The request row is the audit record
for who cut what. Oversight and agents use this instead of `kubectl exec`;
`land-pr` is unchanged, since the PR itself is what it merges.

## 4. Isolation

Segregation is in the data and the authorisation, not in processes.

- **Data:** every mirror row is keyed by repository, and every server call
  the service makes is authorised by scope and moved by CAS. The service's
  own state is one directory per repository, so a corrupt cache or SQLite
  file breaks one repository.
- **Authorisation:** the service holds `repo:*:admin`. Its blast radius is
  every repository, the same as today's pod, which holds thirteen admin
  tokens in one secret. The gain is that it is one token kind to rotate and
  denylist, and its use is one audit identity.
- **Runtime:** limits are service config, not per-repository processes:
  - a timeout per poll and per fetch;
  - a cap on concurrent fetches, and a per-upstream-host limit so one forge's
    rate budget is shared fairly;
  - exponential backoff for a repository whose polls keep failing, with the
    failure in its `status`, so one broken upstream does not slow the rest;
  - a bound on the import work per poll (today's `TAGS_PER_POLL` and the
    equivalent for commits), so one large push cannot starve the loop.
- **What a buggy or compromised repository job can still affect:** shared
  memory, disk and the GitHub rate budget of the service, and, through the
  wildcard scope, other repositories' refs. The first three are the trade
  for one container and match Forgejo and Gitea. The last is unchanged from
  today and is bounded by the followed-line guard: only the mirror and
  admins move a followed line, and every move is a CAS the journal records.

## 5. Scale, HA and failure modes

- One replica. A line has one authority, and the authority claim is CAS in
  the mirror's own DB, so a second replica would need a shared DB. Thirteen
  repositories polled every 15 s is far below one process's capacity; the
  limits in §4 are what matter when that grows.
- A service crash loses nothing: every write to graft was a CAS the server
  committed, and the next start reconciles from `ListMirrors` and the per-
  repository state.
- Divergence and force-push stop one line, as today, with the `repo` label
  on the alert and the reason in `status`. Resolution is unchanged
  (`reconcile`, and the deferred full rewind, WI-3142).
- The server being down stops every mirror, as it does today.

## 6. Migration and rollback

1. Land the scope, table, API and `serve` subcommand behind nothing; the old
   `follow --config` stays until step 4.
2. `graft-mirror import-config <toml>…` reads the thirteen configs and
   creates the mirror rows. It is idempotent.
3. Deploy one `graft-mirror serve` container with the thirteen per-repository
   directories moved from the subPaths into its data volume. Both mirrors
   must not run at once; the old Deployment is scaled to zero first. A
   mistake is safe: the followed-line guard and the authority claim make a
   double-running mirror refuse rather than fork a line.
4. Remove the per-repository containers, `mirrors.tsv`, `mirror-*.toml`,
   `onboard-repo.sh`, `render-onboarded-compose.sh` and the ops `gen.py`
   mirror rendering. `deploy/README.md` documents `graft repo mirror add` as
   the onboarding step.
5. Rollback at any point before step 4 is the previous Deployment manifest:
   the rows are additive and the per-repository data layout is unchanged.

## 7. What the guard and rewind become

Both are server-side, per-line concepts and are unchanged. The followed-line
guard (`graft.followed/<line>`, WI-3140) belongs to the server regardless of
how many mirror processes exist. The full rewind of a followed line
(WI-3142) becomes a mirror-service operation on one repository, requested
through the same API as a batch (`RewindFollowed(repo, line)`), rather than
a command run in a container.

## 8. Out of scope

- Webhooks instead of polling (D9 already allows either; the reconcile loop
  does not depend on which).
- Moving mirror state (the per-repository SQLite) into the server.
- Multi-replica mirror services.
- Export-only and graft-primary mode changes (roadmap, v0.3 onward). The
  `direction` field is reserved for them.

## 9. Decisions requested

1. Adopt §2 as the target, amending `design.md` §11 to say the mirror
   service is one process driven by repository settings.
2. Add `repos:admin` and `repo:*:<perm>` to the scope grammar (§3.1), and the
   `mirror` token kind.
3. Store per-mirror credentials encrypted in the server (§3.4), not as
   secret-store references.
4. Batching and the followed-line rewind are server API requests executed by
   the mirror service (§3.5, §7).
