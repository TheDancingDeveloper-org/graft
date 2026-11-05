# Proposal: mirrors as repository settings, one mirror service

Status: revision 2 for operator sign-off, 2026-11-05. This is the design PR
for moving mirror definitions out of deploy files and into graft. On
sign-off it becomes decision D40 and amends `design.md` §11. Nothing here is
built before that.

Revision 2 folds in the Opus review of f55bc56 (WI-3215) and the operator's
answers of 2026-11-05:

1. The GitHub App does not exist yet. Migration seeds per-row PAT
   credentials, and the App comes later.
2. Only the operator holds the upstream-binding grant `mirror:bind`. Agents
   can't add or repoint mirrors.
3. A server-side authority lease per repository is accepted. This amends §8.
4. Export needs its own grant, separate from follow.

Appendix A lists the changes from revision 1. Appendix B lists the rewrites
needed to WI-3207–3213.

## 1. Problem

`graft-mirror follow --config <file>` follows one repository, so graft-dev
runs one `graft-mirror-<repo>` container per followed repository. There are
thirteen today, all in one pod. Each has its own config, graft token, git
cache, SQLite file, metrics port and volume subPath, rendered from
`deploy/mirrors.tsv` and `deploy/mirror-<repo>.toml` by the ops repo's
`gen.py`.

What that costs:

- every deploy restart-storms the pod while the server comes up;
- thirteen copies of the runtime, the poll loop and the git cache;
- onboarding a repository takes an edit to graft's deploy files, an ops PR,
  a render, an Infisical secret, an ESO mapping and a redeploy. ESO
  overwrites a hand-placed secret, which is what happened on the last
  onboarding;
- `pr-batch` is an operator `kubectl exec` into the right container.

The design doesn't require any of this. `design.md` §11 says the mirror is a
separate service and the only place git exists. It doesn't say one process
per repository. The per-repo shape came from copying the first trial mirror
for each new repository.

**Operator position (2026-11-03):** graft is a standalone stack. Users and
agents create and manage repositories live through the API and MCP. Mirror
definitions are not infrastructure-as-code. Binding a mirror to an upstream
is an operator act (2026-11-05, §3.1).

## 2. Target

- **Repositories and mirrors are API objects.** A mirror is a setting on a
  repository. It is stored in the server's meta DB, managed by
  `graft repo mirror` and the matching MCP tools, and its status is read
  from the same place.
- **One mirror service.** One process, one volume, one narrowly scoped
  token. It learns which repositories to mirror from the server and
  reconciles its set of follow and export tasks. A mirror bound through the
  API starts within one reconcile interval, with no deploy.
- **Nothing per repository in deployment.** The deploy-time surface is the
  Deployment, the server's signing key, the server's credential key (§3.4),
  the mirror service's token and, later, the GitHub App key.
- **Batching is an API call.** Cutting, excluding from and settling a batch
  PR are requests recorded on the server and executed by the mirror service.
  Nobody execs into a container.

## 3. What changes in graft

### 3.1 Scopes, grants and the mirror token

Scopes today are all `repo:<name>:…` (`graft-auth` `scope.rs`). No scope
covers repositories that don't exist yet, so creating a repository is an
admin CLI call rather than an API anyone can be granted.

Add these forms to the scope grammar:

| Scope | Grants | Held by |
| --- | --- | --- |
| `repos:admin` | `CreateRepo`, `DeleteRepo`, `ListRepos`. The holder gets `repo:<name>:admin` on a repository it creates. | operator; may be delegated |
| `mirror:bind` | binding a repository to an upstream: `SetMirror` when it creates a row or changes `upstream`, `SetMirrorCredential`, `RemoveMirror`, `PurgeMirror` | **operator only** |
| `mirror:export` | setting `direction` to `export` or `both` on any line, and changing the export author mapping | **operator only** |
| `repo:*:mirror` | the mirror permission set below, on every repository now and later | the `mirror` token kind only |
| `repo:*:<perm>[:<arg>]` | the named permission on every repository, now and later | service principals |

`mirror:bind` and `mirror:export` are separate grants because they protect
different things. Bind decides what the service *fetches*, which is the
exfiltration risk. Export decides what it *pushes*, which is the write risk
on an upstream the holder may have no rights on. Neither is implied by
`repo:<name>:admin`, and neither is implied by `repos:admin`. The server
refuses to mint either grant into an agent or session token kind; only the
operator token kind may hold them. Agents can create repositories (if given
`repos:admin`), read mirror status and request batches. They can't choose
an upstream, point a credential at one, or turn on export.

A repository admin (`repo:<name>:admin`) on a bound mirror may still:
enable or disable it, change `interval_s` (subject to the minimum, §4), and
add, remove or remap `follow` lines on the bound upstream. None of those
changes what is fetched from or pushed to.

**The mirror permission set** (`repo:*:mirror`) is exactly:

- `ListMirrors` and `GetMirror` (without credential);
- `GetMirrorCredential` (§3.4);
- `SetMirrorStatus` (§3.3);
- `UpdateRef`, only on a line that carries the followed marker
  `graft.followed/<line>` for that mirror row, and only by CAS;
- importing objects and changesets into such a line;
- acquiring, renewing and releasing the mirror authority lease (§3.6);
- claiming, completing and failing batch and rewind requests (§3.5).

It does not include `DeleteRepo`, token administration, `SetMirror`, moving
an unfollowed line, or any `repos:*` call. The `mirror` token kind may carry
only this set; the server rejects any other scope on that kind at mint time.
Every use is one audit identity and the token is revocable as one thing
(`token_denylist`).

The followed-line guard (WI-3140) admits the `mirror` token kind's
principal on a followed line instead of the thirteen per-repo tokens. The
guard change ships before cutover (§6 step 1). A test asserts that a
`mirror`-kind token can move a followed line and can't move an unfollowed
one.

The grammar change is text-only. No encoding moves, so no golden vector
moves.

Repository creation becomes `graft repo create <name>` and the MCP tool
`repo_create`, under `repos:admin`. The existing admin CLI keeps working.

### 3.2 The mirror setting

A new meta table, `repo_mirrors`, one row per `(repo, upstream)`:

| Column | Meaning |
| --- | --- |
| `repo` | the graft repository |
| `upstream` | upstream URL, validated against the allowlist below |
| `lines` | list of `{branch, line, direction}`; `direction` is `follow`, `export` or `both` |
| `interval_s` | poll interval, default 15 (D9), minimum enforced (§4) |
| `enabled` | bool |
| `credential` | nullable: an encrypted per-mirror credential (§3.4) |
| `export` | author mapping and default author, as today's `[export]` |
| `version` | monotonic counter, bumped by every `SetMirror`, `SetMirrorCredential` and enable/disable; also the etag on `SetMirror` (CAS) |
| `status` | written by the mirror service (§3.3), not part of `version` |
| `created_by`, `updated_by`, `updated_at` | audit |

`graft-proto` gets the typed `Mirror`, `MirrorLine` and `MirrorStatus`
messages. `RepoService` gets `SetMirror`, `GetMirror`, `ListMirrors`,
`RemoveMirror` and `PurgeMirror`, under the grants in §3.1. `SetMirror`
takes the expected `version`, and a mismatch is a typed conflict.
`ListMirrors` across repositories is under `repo:*:read`, `repo:*:mirror`
or `repos:admin`, is paginated, and can filter on `enabled`.

**Upstream URL allowlist.** The server validates `upstream` at the
boundary (rule 9) as a string, without parsing git:

- the scheme is `https` or `ssh` (including the `git@host:path` form). It
  is never `file`, `git`, `http`, `ext` or a transport helper (`<x>::…`);
- the host is a DNS name or a public address. Loopback, link-local,
  RFC 1918, CGNAT, unique-local IPv6, cluster service names
  (`*.svc`, `*.cluster.local`) and the server's own names are rejected;
- the host must be in the server's configured `mirror.allowed_hosts`
  (default `github.com`), which only the operator changes.

The mirror service checks the same rules again before every fetch, after
DNS resolution, and refuses to connect to a resolved address in a blocked
range. This closes a DNS-rebinding route round the server check. It isn't a
re-check of trusted data: the resolution happens in the service.

The CLI and MCP surface:

```
graft repo mirror add    <repo> <upstream> --follow main=main [--export …] [--interval 15]   # mirror:bind (+ mirror:export)
graft repo mirror set    <repo> <upstream> [--enable|--disable] [--follow …] [--interval …]  # repo admin, unless it changes upstream or export
graft repo mirror credential <repo> <upstream> --from-file <f> | --stdin                     # mirror:bind
graft repo mirror remove <repo> <upstream> [--clear-followed]                                # mirror:bind
graft repo mirror purge  <repo> <upstream>                                                   # mirror:bind
graft repo mirror status [<repo>]
```

MCP tools: `mirror_set`, `mirror_status` and `mirror_list` for agents.
`mirror_add`, `mirror_remove` and `mirror_purge` exist too, but they need
`mirror:bind`, which agent tokens can't hold, so an agent calling them gets
a typed permission error. Every response is bounded (status is paginated)
and no response carries a credential.

Branch names and an upstream URL are stored as strings. The server doesn't
fetch them or hold any git type, which keeps rule 6 (git stays in
`graft-git` and `graft-mirror`). The design PR states this as the boundary.

**Mirror removal.** `RemoveMirror` deletes the row. The service stops the
task at its next reconcile (§3.3). By default the `graft.followed/<line>`
markers stay, so the lines stay closed to normal lands. That keeps a
mistaken removal from opening a line that a later re-add would then find
diverged. `--clear-followed` clears the markers in the same transaction, so
the lines become ordinary graft lines. Each cleared marker gets its own
journal entry (rule 3: the removal is one logical write).

**Data removal.** `PurgeMirror(repo, upstream)` needs `mirror:bind`. It
applies only to a row that is already removed or disabled. It records a
purge request, and the service deletes `<data>/<repo>/` (git cache and
mirror SQLite, batch history included) and releases the repository's lease
(§3.6). The graft repository, its lines and its history are untouched.

### 3.3 The mirror service

`graft-mirror serve` replaces `graft-mirror follow --config`. Its config is
the server address, its token file, its data directory, its metrics address
and the limits (§4). With the App (later), it also takes the App key file
and installation id. The config lists no repositories.

The reconcile loop runs on start and every `reconcile_s` (default 15 s,
independent of any row's `interval_s`):

1. `ListMirrors` with `enabled = true` (paginated). Compare each row's
   `version` with the version its running task was started from. Detecting
   a change is that one integer comparison, never a field-by-field diff.
2. For each row with no running task: acquire the repository's authority
   lease (§3.6). Create the data directory (`<data>/<repo>/mirror.git`,
   `<data>/<repo>/mirror.sqlite`) and spawn a task. The per-repository
   SQLite and git cache keep today's schema and layout, so a repository's
   state is one directory.
3. For each running task whose `version` changed, mark it for restart. The
   task finishes the poll it is in, then exits, and the loop respawns it
   with the new row. A restart never cancels a task mid-import or
   mid-export.
4. For each running task whose row is gone or disabled, do the same, but
   don't respawn. Release the lease and leave the directory (§3.2, purge).
5. Claim any pending batch, rewind or purge requests (§3.5) for repositories
   it holds the lease on.

Each repository's task is today's `poll_once`, unchanged: `ls-remote`,
fetch, import, CAS `UpdateRef` on the line, with the followed-line marker.
The authority switch (design §11.3) stays per line and per repository.

**Status writes.** A task writes `SetMirrorStatus` at most once per poll
per repository, and skips the call when nothing changed since the last
write (the same sha, error code and divergence; only the poll time moved).
In that case it writes at most once a minute, so status shows the task is
alive. Status is operational state, not a logical write in the rule 3
sense. It isn't journaled and never moves a ref. Its schema:

| Field | Meaning |
| --- | --- |
| `last_poll_at`, `last_success_at` | times |
| `last_imported_sha`, `last_changeset` | progress |
| `lag_changesets`, `lag_seconds` | lag |
| `error_code` | typed: `upstream_unreachable`, `auth_failed`, `not_found`, `diverged`, `force_pushed`, `rate_limited`, `timeout`, `url_blocked`, `lease_lost`, `internal` |
| `error_message` | at most 200 characters, written by the service from the code and safe context. **Never raw git stderr**, which can echo URLs that carry credentials. |
| `backoff_until` | when the next poll is allowed |
| `lease_epoch` | the epoch the writer holds (§3.6) |

The server rejects a `SetMirrorStatus` whose `lease_epoch` isn't the
current one, so a fenced-off process can't overwrite status.

**Metrics.** One `/metrics` endpoint, where every mirror series gains a
`repo` label. A repository's series are present only while the service has
a task for an enabled row, and they are dropped when the task stops, so the
label's cardinality follows the enabled rows. The alert rules in
`deploy/monitoring/` change from thirteen jobs to one job with a label.

Why a separate service and not the server: otherwise the server would have
to run git, hold a forge credential and push to a forge. Design §11 keeps
all of that out of the server. D38's web UI and D39's independence are both
easier to hold when the server stays a store plus an engine.

### 3.4 Credentials

**Interim: per-row PATs (operator, 2026-11-05).** The GitHub App doesn't
exist yet. Every row starts with a row credential. The migration (§6) seeds
each row from today's `GRAFT_DEV_MIRROR_TOKEN_*` value for that repository.
So the shared PAT stops being a deploy secret and becomes thirteen
encrypted rows, which can be rotated one at a time.

**Later: the GitHub App (D8).** A follow-up item installs one App on the
organisation. Its key and installation id go into the service's secret.
Rows in that organisation can then drop their credential. Even with the
App, a row is fetched under the App only if the operator bound it
(`mirror:bind`), and only if its `OWNER/REPO` is in the installation's
repository list. The service checks the list before minting a token for a
row. The App never widens what a repository admin can reach. One
installation token is cached and shared across tasks until shortly before
it expires, rather than minted per call.

**Storage.**

- A separate **credential key-encryption key (KEK)**, unrelated to the
  signing key, is given to the server as its own deploy secret
  (`credential_kek`, with an id). The server keeps a key ring: one active
  KEK and zero or more retired ones that may only decrypt.
- Each row stores a fresh random 256-bit data key wrapped by the active KEK,
  the KEK id, a nonce, and the ciphertext. The credential is sealed with an
  AEAD (XChaCha20-Poly1305). Its **associated data** is
  `"graft.mirror.cred.v1" ‖ repo ‖ upstream ‖ kek_id`, so a database writer
  can't move one row's ciphertext into another row: it fails to open.
- KEK rotation: add a new KEK as active, then run
  `graft admin mirror rewrap`, which re-wraps each row's data key under the
  new KEK in one transaction per row. Then retire the old KEK. Rotating the
  signing key doesn't touch credentials, and a leaked signing key doesn't
  expose them.
- The envelope format is a durable encoding, frozen by a golden vector
  (rule 1) from its first release.

**Access rule.** This revision replaces "never returned by the API" in
WI-3209 with the following rule:

- `SetMirrorCredential` is write-only, under `mirror:bind`. The CLI reads
  the value from a file or stdin, never argv (rule 8).
- `GetMirrorCredential` returns the plaintext **only** to a `mirror`-kind
  token that holds the current authority lease for that repository (§3.6),
  over the authenticated channel. No other token kind can call it, and no
  other API returns the credential or any part of it. `GetMirror` and
  `ListMirrors` return only `credential_set: bool` and the KEK id.
- The service holds the plaintext in memory only, wrapped in a type whose
  `Debug` and `Display` print `<redacted>`. It is never written to disk.
  The git invocation gets it through a credential helper on a pipe, never
  in the URL, argv or the environment of a logged process. It is dropped
  after use (re-fetched per poll, or cached in memory for at most the poll).
- Neither the server nor the service logs it. Tests assert redaction:
  `Debug` and `Display` of the credential type and of `MirrorCredential`
  responses contain no plaintext, and a forced fetch failure leaves no
  plaintext in logs or `status`.

**Rejected:** storing Infisical references. That ties graft to the estate's
secret store, against D39.

### 3.5 Batching and rewind as server requests

Batch history stays in the mirror's per-repository SQLite (`batches*`
tables). The control moves to the server as a request table,
`mirror_requests`, one row per request:

| Column | Meaning |
| --- | --- |
| `id` | server-assigned |
| `repo`, `line`, `kind` | `kind` is `cut_batch`, `settle_batch`, `exclude`, `rewind` or `purge` |
| `client_request_id` | caller-supplied idempotency key, unique per `(repo, kind)` |
| `args` | base, excludes and so on |
| `state` | `pending` → `claimed` → `done` / `failed` / `expired` |
| `claimed_by_epoch`, `claimed_at` | the lease epoch of the claimer (§3.6) |
| `deadline` | `created_at` + request timeout (default 30 min) |
| `result` | PR URL and number, branch, commits, or a typed error code |
| `requested_by`, timestamps | audit |

| Call | Effect |
| --- | --- |
| `CutBatch(repo, line, base, excludes, client_request_id)` | records a `cut_batch` request; returns the existing request if the id was seen |
| `GetBatch(repo, line)` | the open batch, its PR, its lands, its state |
| `ListBatches(repo)` | paginated history |
| `RewindFollowed(repo, line, client_request_id)` | records a `rewind` request (WI-3142) |
| `GetRequest(id)` | one request's state and result |

CLI `graft repo batch cut|status|list`, MCP `batch_cut`, `batch_status`,
`batch_list`. A batch is requested under `repo:<name>:admin`, and a rewind
under `mirror:bind` because it discards history. `land-pr` is unchanged,
since the PR itself is what it merges.

**Rules, enforced by the server:**

- **Idempotency.** A retried `CutBatch` with the same `client_request_id`
  returns the original request and never creates a second one. The CLI and
  MCP generate the id once per user action and reuse it on retry.
- **One open batch per `(repo, line)`.** The server keeps an
  `open_batch` pointer per line, moved only by CAS. `CutBatch` fails with a
  typed `batch_open` conflict while a batch is `open` or `sealed`
  (auto-merge on, delivery-process.md). Settling (merged or closed) clears
  the pointer by CAS. A batch moves `requested → open → sealed → settled`,
  each step a CAS on the request or pointer row, so two cuts can't both
  win.
- **Claiming.** The service claims a pending request by CAS from `pending`
  to `claimed` with its lease epoch. A request claimed under an epoch that
  is no longer current returns to `pending` (§3.6).
- **Crash recovery.** The PR branch name is deterministic:
  `graft/batch-<n>`, where `<n>` is the request's server id. Before it
  creates a PR, the service looks up an open PR on that branch. If one
  exists, which means an earlier attempt created it but never recorded the
  result, the service adopts it and records it. So a crash between "PR
  created" and "result recorded" never opens a second PR. The same applies
  to the pushed branch.
- **Timeout and expiry.** A request still `pending` or `claimed` past its
  `deadline` moves to `expired` (checked lazily on read and by the
  service's loop). An expired cut releases the `open_batch` pointer only if
  no PR was found on its branch, so an expiry can't orphan a live PR.
- **Rewind** follows the same idempotency, claim, recovery and expiry
  rules. It is one request per line at a time, and a rewind refuses while a
  batch is open on that line.

### 3.6 Per-repository authority lease (amends §8)

Revision 1 kept the authority claim only in the per-repository SQLite. If
an operator copies a data directory instead of moving it, two processes
each hold a claim, and both import, write status and cut batches. CAS still
keeps the refs consistent, but the duplicate work and duplicate PRs are
real. The operator accepted a server-side lease (2026-11-05).

- A meta table `mirror_leases`, one row per repository: `holder` (the
  token principal plus a random instance id chosen at process start; this
  isn't an id under rule 5, it is never durable in a ref or object),
  `epoch` (monotonic per repository), `expires_at`.
- `AcquireLease(repo)` succeeds by CAS if the lease is free or expired. It
  bumps `epoch`. `RenewLease(repo, epoch)` extends it (TTL 60 s, renewed
  every 20 s). `ReleaseLease(repo, epoch)` frees it.
- **Fencing.** Every mirror write, `UpdateRef` on a followed line,
  `SetMirrorStatus`, `GetMirrorCredential` and request claim or complete,
  carries the epoch. The server rejects a stale epoch with a typed
  `lease_lost`. A process that loses its lease stops the repository's task
  after the current step and doesn't retry the write.
- The lease covers *who may act*. The per-line authority switch
  (design §11.3, which of graft or the upstream is the source of truth)
  is unchanged and stays in the line state.
- **Migration marker.** `serve` also refuses to adopt a data directory
  that lacks the marker file `import-config` writes (§6). A stray copy of
  an old subPath can't start silently.

This is the one piece of mirror state that moves into the server. The
per-repository SQLite (cache, import bookkeeping, batch history) stays in
the service.

## 4. Isolation and backpressure

Segregation is in the data and the authorisation, not in processes.

- **Data:** every mirror row, lease and request is keyed by repository.
  Every server call the service makes is authorised by scope, fenced by
  lease epoch and moved by CAS. The service's own state is one directory
  per repository, so a corrupt cache or SQLite file breaks one repository.
- **Authorisation:** the service holds only `repo:*:mirror` (§3.1). It can
  move followed lines, read credentials for repositories it holds the
  lease on, and write status. It can't delete repositories, mint tokens,
  bind upstreams, enable export or touch unfollowed lines. What it fetches
  and pushes is set by the operator through `mirror:bind` and
  `mirror:export`.
- **Runtime limits**, set in service config rather than by per-repository
  processes:
  - a timeout per poll and per fetch;
  - a cap on concurrent tasks actively fetching (default 4) and a
    per-upstream-host limit, so one forge's rate budget is shared fairly;
  - a **server-side minimum `interval_s`** (default 15 s, config
    `mirror.min_interval_s`). `SetMirror` rejects a smaller value;
  - a request budget per host. GitHub allows 5,000 requests/h per token or
    installation. N repositories × `ls-remote` every 15 s is 240 N/h, so a
    shared credential tops out at about 20 repositories at 15 s. The
    service measures its rate per host and per credential, and stretches
    every task's effective interval evenly when it nears 80% of the budget.
    It also honours `Retry-After` and secondary-rate-limit replies
    (`rate_limited` in status);
  - exponential backoff for a repository whose polls keep failing, with the
    failure in its `status`, so one broken upstream doesn't slow the rest;
  - **backoff when the graft server is unreachable**: the reconcile loop and
    every task back off exponentially (capped at 60 s) on server errors,
    rather than hammering a restarting server. This replaces today's
    restart-storm;
  - a bound on the import work per poll (today's `TAGS_PER_POLL` and the
    equivalent for commits), so one large push can't starve the loop.
- **What a buggy or compromised repository task can still affect:** the
  service's shared memory and disk, and the forge rate budget. These are
  the trade for one container, as in Forgejo and Gitea. Through the
  `mirror` scope it can affect other repositories' *followed* lines, and
  then only by fenced CAS that the journal records. It can't reach an
  upstream the operator didn't bind.

## 5. Scale, HA and failure modes

- One replica. The lease (§3.6) makes a second replica safe, because it
  fences and doesn't fork, but it doesn't make one useful. Multi-replica
  sharding by lease stays out of scope (§8). Thirteen repositories polled
  every 15 s is far below one process's capacity; the limits in §4 matter
  when that grows.
- A service crash loses nothing. Every write to graft was a CAS the server
  committed. The lease expires within 60 s, and the next start reacquires
  it, reconciles from `ListMirrors` and the per-repository state, and
  recovers in-flight requests (§3.5).
- Divergence and force-push stop one line, as today, with the `repo` label
  on the alert and a typed code in `status`. Resolution is unchanged:
  `reconcile`, and the deferred full rewind (WI-3142) via `RewindFollowed`.
- The server being down stops every mirror, as it does today. The service
  backs off instead of crash-looping (§4).

## 6. Migration and rollback

1. Land the scope grammar, the grants, the `mirror` token kind, the
   followed-line guard change (§3.1), the tables (`repo_mirrors`,
   `mirror_leases`, `mirror_requests`), the credential envelope with its
   golden vector, the API and the `serve` subcommand. The old
   `follow --config` stays until step 5.
2. The operator provisions `credential_kek` and mints one `mirror`-kind
   token.
3. `graft-mirror import-config <toml>…` runs as the operator (it needs
   `mirror:bind`). It reads the thirteen configs, creates the mirror rows,
   and seeds each row's credential from that repository's
   `GRAFT_DEV_MIRROR_TOKEN_*` value through `SetMirrorCredential`, read
   from a file, never argv. It writes the migration marker into each
   per-repository directory. It is idempotent: an existing row with
   identical fields is left alone, and a differing one is reported, not
   overwritten.
4. Scale the old Deployment to zero, then deploy one `graft-mirror serve`
   container with the thirteen per-repository directories **moved** from
   the subPaths into its data volume. If a copy is made anyway, the lease
   fences the second process (§3.6), and `serve` refuses a directory
   without the marker.
5. Remove the per-repository containers, `mirrors.tsv`, `mirror-*.toml`,
   `onboard-repo.sh`, `render-onboarded-compose.sh`, the ops `gen.py`
   mirror rendering and the `GRAFT_DEV_MIRROR_TOKEN_*` deploy secrets.
   `deploy/README.md` documents `graft repo mirror add` (operator) as the
   onboarding step.
6. Later, separate item: install the GitHub App. Clear row credentials for
   in-organisation rows once the App is verified.
7. Rollback at any point before step 5 is the previous Deployment manifest
   and token secret. The rows, leases and requests are additive, and the
   per-repository data layout is unchanged. The old processes don't read
   the lease. That is safe only because the new service is scaled to zero
   first, so rollback scales `serve` to zero before restoring the old
   Deployment.

## 7. What the guard and rewind become

The followed-line guard (`graft.followed/<line>`, WI-3140) stays a
server-side, per-line concept. Its only change is that it admits the
`mirror` token kind's principal (§3.1). Removing a mirror leaves the marker
unless asked to clear it (§3.2). The full rewind of a followed line
(WI-3142) becomes a server request, `RewindFollowed`, under `mirror:bind`,
executed by the service under the rules in §3.5.

## 8. Out of scope

- Webhooks instead of polling. D9 already allows either, and the reconcile
  loop doesn't depend on which.
- Moving mirror state into the server, **except** the authority lease
  (§3.6), which the operator accepted on 2026-11-05. The per-repository
  SQLite stays in the service.
- Multi-replica mirror services, including lease-sharded replicas.
- Export-only and graft-primary mode changes (roadmap, v0.3 onward). The
  `direction` field is reserved for them, and setting it needs
  `mirror:export`.
- Any scheduling of build, test or check jobs. Phase 1 runs none (D22,
  D35). The mirror service schedules only mirror polls and mirror requests.

## 9. Decisions requested (D40)

1. Adopt §2 as the target, amending `design.md` §11 to say the mirror
   service is one process driven by repository settings.
2. Add `repos:admin`, `repo:*:<perm>`, the operator-only `mirror:bind` and
   `mirror:export` grants, and the `mirror` token kind limited to the
   `repo:*:mirror` set (§3.1).
3. Store per-mirror credentials in the server, encrypted under a separate
   KEK with key ids and associated data (§3.4), not as secret-store
   references. Seed them from today's PATs; the GitHub App follows later.
4. Add the per-repository authority lease with epoch fencing (§3.6),
   amending §8.
5. Batching and the followed-line rewind are idempotent, server-side
   requests executed by the mirror service (§3.5).
6. Validate upstream URLs against a scheme and host allowlist (§3.2).

## Appendix A. Changes from revision 1 (f55bc56)

| Review item | Where it's resolved |
| --- | --- |
| Must-fix 1: cross-repo upstream hijack and SSRF | §3.1 `mirror:bind` and `mirror:export` (operator only); §3.4 App installation repo check; §3.2 URL allowlist plus a post-resolution check in the service |
| Must-fix 2: over-scoped mirror token | §3.1 `repo:*:mirror` set, bound to the `mirror` kind; §4 |
| Must-fix 3: credential encryption | §3.4 separate KEK ring with ids, per-row data key, AEAD associated data, rewrap; access rule replacing "never returned"; redaction tests |
| Must-fix 4: migration fencing | §3.6 server lease and epoch fencing, migration marker; §3.1 guard admits the `mirror` kind; §6 steps 1, 3, 4 |
| Must-fix 5: batch semantics | §3.5 idempotency key, CAS open-batch pointer with sealed state, claim by epoch, deterministic-branch recovery, deadline and expiry; same for rewind |
| Should-fix: change detection | §3.2 `version`; §3.3 steps 1 and 3, restart only between polls |
| Should-fix: removal and marker | §3.2 Mirror removal, `--clear-followed` |
| Should-fix: backpressure | §4 minimum interval, shared installation token (§3.4), rate budget, server-down backoff |
| Should-fix: status writes | §3.3 one per poll, coalesced, not journaled, epoch-checked |
| Should-fix: metrics cardinality | §3.3 Metrics |
| Should-fix: data removal | §3.2 `PurgeMirror` |
| Should-fix: status error schema | §3.3 typed `error_code` and a short message, no stderr |
| Should-fix: initiative drift | Appendix B; §8 no checks |
| Operator answers 1–4 | §3.4 interim PATs; §3.1 bind and export grants; §3.6 lease |

## Appendix B. Rewrites needed to WI-3207–3213

To be applied after D40 is signed:

- **WI-3207 (DB model):** retitle "mirror tables". Cover `repo_mirrors`
  with `version`, `mirror_leases` and `mirror_requests`. Drop "schedules":
  scheduling lives in the service, not the server.
- **WI-3208 (scheduler in graft-server):** rewrite as "`graft-mirror
  serve`: reconcile loop, lease, limits and backoff" (§3.3, §3.6, §4), in
  `graft-mirror`. **Drop check jobs entirely** (D22, D35), and drop the
  server-side worker pool.
- **WI-3209 (credentials in the secret store):** rewrite as "encrypted
  per-row credentials" (§3.4): KEK ring, envelope with a golden vector,
  AAD, rewrap, the `GetMirrorCredential` access rule and redaction tests.
  Drop Infisical references (D39). Raise the GitHub App as a separate later
  item.
- **WI-3210 (onboarding and pr-batch):** split. (a) The scope grammar, the
  `mirror:bind` and `mirror:export` grants, the `mirror` token kind, the
  followed-line guard change, the URL allowlist and the mirror CLI and MCP.
  (b) `mirror_requests`: batch and rewind API, idempotency, recovery and
  expiry (§3.5).
- **WI-3211 (metrics plus status API):** reduce to metrics with a bounded
  `repo` label and the alert-rule change. The status API is part of
  WI-3207 and WI-3210(a) (§3.3).
- **WI-3212 (migration):** `import-config` as the operator, seeding PAT
  credentials and writing migration markers. Make it idempotent and have it
  report differences (§6 steps 2–4).
- **WI-3213 (retire containers):** add removing the
  `GRAFT_DEV_MIRROR_TOKEN_*` deploy secrets and the rollback order
  (§6 steps 5 and 7).
